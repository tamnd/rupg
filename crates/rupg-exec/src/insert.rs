//! The checks of `ExecConstraints` on the rows of an `INSERT`: the not-null constraints of the columns in the order of the attributes, then the check constraints in the order of their names.

use rupg_analyze::InsertInfo;
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_types::Value;

/// The most bytes of a value in the detail of an error, as `ExecBuildSlotValueDescription` cuts it.
const MAX_FIELD_LEN: usize = 64;

/// Checks each new row and gives the rows with only the columns of the table.
pub(crate) fn check(
    info: &InsertInfo,
    rows: Vec<Vec<Value>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let rel = session.catalog().and_then(|c| c.relation(info.table));
    let types: Vec<u32> = rel.map(|r| r.columns.iter().map(|c| c.ty).collect()).unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        if let Some(rel) = rel {
            for (column, value) in rel.columns.iter().zip(&row) {
                if column.not_null && matches!(value, Value::Null) {
                    return Err(Error::new(
                        SqlState::NOT_NULL_VIOLATION,
                        format!(
                            "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                            column.name, info.name
                        ),
                    )
                    .with_detail(failing_row(&row[..info.width], &types, session)?));
                }
            }
        }
        for (i, name) in info.checks.iter().enumerate() {
            if matches!(row.get(info.width + i), Some(Value::Bool(false))) {
                return Err(Error::new(
                    SqlState::CHECK_VIOLATION,
                    format!(
                        "new row for relation \"{}\" violates check constraint \"{name}\"",
                        info.name
                    ),
                )
                .with_detail(failing_row(&row[..info.width], &types, session)?));
            }
        }
        row.truncate(info.width);
        out.push(row);
    }
    Ok(out)
}

/// `ExecBuildSlotValueDescription`: the detail `Failing row contains (...)` with the text of each value, `null` for a null value, and a value of more than 64 bytes cut with `...` after it.
fn failing_row(row: &[Value], types: &[u32], session: &dyn Session) -> Result<String> {
    let mut parts = Vec::with_capacity(row.len());
    for (value, &ty) in row.iter().zip(types) {
        if matches!(value, Value::Null) {
            parts.push("null".to_string());
            continue;
        }
        let mut text = Vec::new();
        rupg_func::output(ty, value, session, &mut text)?;
        let mut text = String::from_utf8_lossy(&text).into_owned();
        if text.len() > MAX_FIELD_LEN {
            let mut end = MAX_FIELD_LEN;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("...");
        }
        parts.push(text);
    }
    Ok(format!("Failing row contains ({}).", parts.join(", ")))
}

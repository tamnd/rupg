//! The checks of `ExecConstraints` on the new rows of an `INSERT` or an `UPDATE`: the not-null constraints of the columns in the order of the attributes, then the check constraints in the order of their names.

use rupg_analyze::{Write, WriteKind};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_types::Value;

/// The most bytes of a value in the detail of an error, as `ExecBuildSlotValueDescription` cuts it.
const MAX_FIELD_LEN: usize = 64;

/// Checks each new row and gives the rows with only the columns of the table, and the ID of the version of the old row for an `UPDATE`. The rows of a `DELETE` have nothing to check, and the rows of a `MERGE` are the rows of the join, which the clauses of the `MERGE` check. A write with [`Write::recheck`] keeps the rows as they are, because the triggers come before the checks.
pub(crate) fn check(
    info: &Write,
    rows: Vec<Vec<Value>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    if matches!(info.kind, WriteKind::Delete | WriteKind::Merge) || info.recheck.is_some() {
        return Ok(rows);
    }
    check_rows(info, info.kind, rows, session)
}

/// The checks of [`check`] on new rows of the table, with the conditions of the check constraints after the columns. `kind` is the write of the rows: `WriteKind::Insert` or `WriteKind::Update` for the action of a `MERGE`, which tells when the partition constraint of a partition checks the row.
pub(crate) fn check_rows(
    info: &Write,
    kind: WriteKind,
    rows: Vec<Vec<Value>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let rel = session.catalog().and_then(|c| c.relation(info.table));
    // The partition of each row of a partitioned table checks the row after the routing.
    if rel.is_some_and(|r| r.kind == rupg_catalog::RelKind::Partitioned) {
        return Ok(rows);
    }
    let types: Vec<u32> = rel.map(|r| r.columns.iter().map(|c| c.ty).collect()).unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    let mut router = crate::partition::Router::default();
    let partition = rel.filter(|r| r.partition_of.is_some());
    for mut row in rows {
        // `ExecUpdateAct` checks the partition constraint first.
        if kind == WriteKind::Update
            && let Some(rel) = partition
            && !router.check(rel.oid, &row[..info.width], session)?
        {
            return Err(crate::partition::partition_check_error(rel, &row[..info.width], session)?);
        }
        if let Some(rel) = rel {
            for (column, value) in rel.columns.iter().zip(&row[..info.width]) {
                if column.not_null && matches!(value, Value::Null) {
                    let error = Error::new(
                        SqlState::NOT_NULL_VIOLATION,
                        format!(
                            "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                            column.name, info.name
                        ),
                    )
                    .with_detail(failing_row(&row[..info.width], &types, session)?);
                    return Err(rupg_func::with_table_column(
                        error,
                        rel.oid,
                        &column.name,
                        session,
                    ));
                }
            }
        }
        for (i, name) in info.checks.iter().enumerate() {
            if matches!(row.get(info.width + i), Some(Value::Bool(false))) {
                let error = Error::new(
                    SqlState::CHECK_VIOLATION,
                    format!(
                        "new row for relation \"{}\" violates check constraint \"{name}\"",
                        info.name
                    ),
                )
                .with_detail(failing_row(&row[..info.width], &types, session)?);
                return Err(rupg_func::with_table_constraint(error, info.table, name, session));
            }
        }
        row.drain(info.width..info.width + info.checks.len());
        // `ExecInsert` checks the partition constraint of a row that goes in a partition with no routing last.
        if kind == WriteKind::Insert
            && let Some(rel) = partition
            && !router.check(rel.oid, &row[..info.width], session)?
        {
            return Err(crate::partition::partition_check_error(rel, &row[..info.width], session)?);
        }
        out.push(row);
    }
    Ok(out)
}

/// `ExecBuildSlotValueDescription`: the detail `Failing row contains (...)` with the text of each value, `null` for a null value, and a value of more than 64 bytes cut with `...` after it.
pub(crate) fn failing_row(row: &[Value], types: &[u32], session: &dyn Session) -> Result<String> {
    let mut parts = Vec::with_capacity(row.len());
    // A dropped column has the type 0, and `ExecBuildSlotValueDescription` skips it.
    for (value, &ty) in row.iter().zip(types).filter(|&(_, &ty)| ty != 0) {
        parts.push(shown(ty, value, MAX_FIELD_LEN, session)?);
    }
    Ok(format!("Failing row contains ({}).", parts.join(", ")))
}

/// The text of a value in the detail of an error: `null` for a null value, and a value of more than `max` bytes cut with `...` after it, as `pg_mbcliplen` cuts it.
pub(crate) fn shown(ty: u32, value: &Value, max: usize, session: &dyn Session) -> Result<String> {
    if matches!(value, Value::Null) {
        return Ok("null".to_string());
    }
    let mut text = Vec::new();
    rupg_func::output(ty, value, session, &mut text)?;
    let mut text = String::from_utf8_lossy(&text).into_owned();
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    Ok(text)
}

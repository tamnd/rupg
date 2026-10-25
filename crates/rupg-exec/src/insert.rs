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
    policy: Vec<Option<Error>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    if !checks_now(info) {
        return Ok(rows);
    }
    check_rows(info, info.kind, rows, policy, session)
}

/// True when [`check`] checks the rows of the write.
pub(crate) fn checks_now(info: &Write) -> bool {
    !matches!(info.kind, WriteKind::Delete | WriteKind::Merge) && info.recheck.is_none()
}

/// The checks of [`check`] on new rows of the table, with the conditions of the check constraints after the columns. `kind` is the write of the rows: `WriteKind::Insert` or `WriteKind::Update` for the action of a `MERGE`, which tells when the partition constraint of a partition checks the row. `policy` has the error of the checks of the policies of row-level security for each row that fails them, which comes before the errors of the constraints.
pub(crate) fn check_rows(
    info: &Write,
    kind: WriteKind,
    rows: Vec<Vec<Value>>,
    mut policy: Vec<Option<Error>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let rel = session.catalog().and_then(|c| c.relation(info.table));
    // The partition of each row of a partitioned table checks the row after the routing, also with the policies of the partitioned table.
    if rel.is_some_and(|r| r.kind == rupg_catalog::RelKind::Partitioned) {
        return Ok(rows);
    }
    let types: Vec<u32> = rel.map(row_types).unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    let mut router = crate::partition::Router::default();
    let partition = rel.filter(|r| r.partition_of.is_some());
    for (at, mut row) in rows.into_iter().enumerate() {
        // `ExecUpdateAct` checks the partition constraint first.
        if kind == WriteKind::Update
            && let Some(rel) = partition
            && !router.check(rel.oid, &row[..info.width], session)?
        {
            return Err(crate::partition::partition_check_error(
                rel,
                &row[..info.width],
                &info.modified,
                session,
            )?);
        }
        // `ExecInsert` and `ExecUpdateAct` check the policies before the constraints.
        if let Some(error) = policy.get_mut(at).and_then(Option::take) {
            return Err(error);
        }
        if let Some(rel) = rel {
            // `ExecConstraints` checks the not-null constraints of the virtual generated columns after the others, with the generation expressions on the row.
            let plain = rel.columns.iter().zip(&row[..info.width]).filter(|(c, value)| {
                c.not_null && c.generated != b'v' && matches!(value, Value::Null)
            });
            let mut failed = plain.map(|(c, _)| c).next();
            if failed.is_none() {
                failed = virtual_null(rel, &row[..info.width], session)?;
            }
            if let Some(column) = failed {
                let error = Error::new(
                    SqlState::NOT_NULL_VIOLATION,
                    format!(
                        "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                        column.name, info.name
                    ),
                )
                .with_detail_opt(failing_row(
                    &row[..info.width],
                    &types,
                    (info.table, &info.modified),
                    session,
                )?);
                return Err(rupg_func::with_table_column(error, rel.oid, &column.name, session));
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
                .with_detail_opt(failing_row(
                    &row[..info.width],
                    &types,
                    (info.table, &info.modified),
                    session,
                )?);
                return Err(rupg_func::with_table_constraint(error, info.table, name, session));
            }
        }
        row.drain(info.width..info.width + info.checks.len());
        // `ExecInsert` checks the partition constraint of a row that goes in a partition with no routing last.
        if kind == WriteKind::Insert
            && let Some(rel) = partition
            && !router.check(rel.oid, &row[..info.width], session)?
        {
            return Err(crate::partition::partition_check_error(
                rel,
                &row[..info.width],
                &info.modified,
                session,
            )?);
        }
        out.push(row);
    }
    Ok(out)
}

/// `ExecRelGenVirtualNotNull`: the first virtual generated column of the table `rel` with a not-null constraint whose generation expression is null for the row `row`.
///
/// # Errors
///
/// The error of a generation expression.
fn virtual_null<'r>(
    rel: &'r rupg_catalog::Relation,
    row: &[Value],
    session: &dyn Session,
) -> Result<Option<&'r rupg_catalog::Column>> {
    let Some(catalog) = session.catalog() else { return Ok(None) };
    for (index, column) in rel.columns.iter().enumerate() {
        if !column.not_null || column.generated != b'v' {
            continue;
        }
        let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
        let Some(default) = catalog.default_of(rel.oid, attnum) else { continue };
        let expr = rupg_analyze::node::read(&default.expr)?;
        let types: Vec<u32> = rel.columns.iter().map(|c| c.ty).collect();
        let value =
            crate::evaluate_row(std::slice::from_ref(&expr), rel.oid, &types, row, session)?;
        if matches!(value.as_slice(), [Value::Null]) {
            return Ok(Some(column));
        }
    }
    Ok(None)
}

/// The type of a virtual generated column in the types of [`failing_row`], whose value the detail shows as `virtual`.
pub(crate) const VIRTUAL_TYPE: u32 = u32::MAX;

/// The types of the columns of the table `rel` for [`failing_row`], with `u32::MAX` for a virtual generated column.
pub fn row_types(rel: &rupg_catalog::Relation) -> Vec<u32> {
    let ty = |c: &rupg_catalog::Column| {
        if c.generated == b'v' && !c.dropped { VIRTUAL_TYPE } else { c.ty }
    };
    rel.columns.iter().map(ty).collect()
}

/// `ExecBuildSlotValueDescription`: the detail `Failing row contains (...)` for a row of the table `relid` with the text of each value, `null` for a null value, `virtual` for a virtual generated column, and a value of more than 64 bytes cut with `...` after it. When the current user has no `SELECT` on the table, the detail names the columns that it shows, `(a, b) = (...)`, with only the columns of [`rupg_func::shown_columns`], and there is no detail when no column shows. `modified` are the columns that the statement gives values.
///
/// # Errors
///
/// The errors of the output functions of the types.
pub fn failing_row(
    row: &[Value],
    types: &[u32],
    (relid, modified): (u32, &[i16]),
    session: &dyn Session,
) -> Result<Option<String>> {
    if rupg_func::row_security_hides(relid, session) {
        return Ok(None);
    }
    let visible = rupg_func::shown_columns(relid, modified, session)?;
    let rel = session.catalog().and_then(|c| c.relation(relid));
    let (mut parts, mut names) = (Vec::with_capacity(row.len()), Vec::new());
    // A dropped column has the type 0, and `ExecBuildSlotValueDescription` skips it.
    for (index, (value, &ty)) in row.iter().zip(types).enumerate().filter(|(_, (_, ty))| **ty != 0)
    {
        if let Some(visible) = &visible {
            if !visible.contains(&i16::try_from(index + 1).unwrap_or(i16::MAX)) {
                continue;
            }
            names.push(rel.and_then(|r| r.columns.get(index)).map_or("", |c| c.name.as_str()));
        }
        if ty == VIRTUAL_TYPE {
            parts.push("virtual".to_string());
            continue;
        }
        parts.push(shown(ty, value, MAX_FIELD_LEN, session)?);
    }
    let values = parts.join(", ");
    Ok(match visible {
        None => Some(format!("Failing row contains ({values}).")),
        Some(_) if names.is_empty() => None,
        Some(_) => Some(format!("Failing row contains ({}) = ({values}).", names.join(", "))),
    })
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

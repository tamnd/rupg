//! `pg_relation_is_updatable` and `pg_column_is_updatable`: a port of `relation_is_updatable` and of the checks of `rewriteHandler.c` that make a view automatically updatable.
//!
//! rupg has no rules other than the `_RETURN` rule of a view, no `INSTEAD OF` triggers and no foreign tables. So the events of a relation come from its kind and, for a view, from its query. The rules `pg_settings_u` and `pg_settings_n` are not done yet, so `pg_settings` gives 0 and not the `UPDATE` event of `pg_settings_n`.

use std::collections::BTreeSet;

use rupg_analyze::{ExprKind, FromItem, Query, Target};
use rupg_common::Result;
use rupg_pgcatalog::builtin;
use rupg_types::Value;

use crate::reg;
use crate::{Call, Kernel, Session, bad_value};

/// The bit of `CMD_UPDATE`.
const UPDATE: i32 = 1 << 2;
/// The bit of `CMD_INSERT`.
const INSERT: i32 = 1 << 3;
/// The bit of `CMD_DELETE`.
const DELETE: i32 = 1 << 4;
/// `ALL_EVENTS`: the events of a table.
const ALL_EVENTS: i32 = INSERT | UPDATE | DELETE;

/// The `relkind` of the relation with this OID, from the catalog of the session or from the built-in catalogs, or `None` when no relation has the OID.
fn relkind(oid: u32, session: &dyn Session) -> Option<char> {
    session
        .catalog()
        .and_then(|c| c.relation(oid))
        .map(|r| r.kind.code())
        .or_else(|| builtin::class_by_oid(oid).map(|r| char::from(r.kind)))
}

/// `view_col_is_auto_updatable`: the attribute number of the column of the base relation that a column of the view shows, or `None` when the column is not updatable. Only a plain column of the base relation is updatable.
fn base_column(target: &Target, base: usize) -> Option<i16> {
    if target.junk {
        return None;
    }
    match &target.expr.kind {
        ExprKind::Var(var) if var.relation == base && var.levels_up == 0 && var.attnum > 0 => {
            Some(var.attnum)
        }
        _ => None,
    }
}

/// `view_query_is_auto_updatable` with `check_cols` false: the index and the OID of the base relation of a view that the database can update with no rule, or `None`. The query must read one table or view, with no `DISTINCT`, no groups, no aggregate, no set operation, no `LIMIT` or `OFFSET`, and no window function or set-returning function in its targets.
fn auto_base(query: &Query, session: &dyn Session) -> Option<(usize, u32)> {
    if !query.distinct.is_empty()
        || query.grouped
        || query.set_op.is_some()
        || query.limit.is_some()
        || query.offset.is_some()
        || query.targets.iter().any(|t| t.expr.first_window().is_some())
        || query.has_target_srfs()
    {
        return None;
    }
    let [FromItem::Relation(index)] = query.from.as_slice() else { return None };
    let relation = &query.relations[*index];
    if relation.oid == 0 || relation.function.is_some() || relation.values.is_some() {
        return None;
    }
    let kind = relkind(relation.oid, session)?;
    matches!(kind, 'r' | 'f' | 'v' | 'p').then_some((*index, relation.oid))
}

/// `relation_is_updatable`: the bits of the events that the relation supports. `columns` has the attribute numbers of the columns to look at, or is `None` for all the columns. `outer` has the views that the check is in, so a view that reads itself gives 0.
fn relation_is_updatable(
    oid: u32,
    outer: &mut Vec<u32>,
    columns: Option<&BTreeSet<i16>>,
    session: &dyn Session,
) -> Result<i32> {
    let Some(kind) = relkind(oid, session) else { return Ok(0) };
    if outer.contains(&oid) {
        return Ok(0);
    }
    match kind {
        'r' | 'p' => return Ok(ALL_EVENTS),
        'v' => {}
        _ => return Ok(0),
    }
    let env = reg::SessionEnv(session);
    let Some(query) = rupg_analyze::view_query(&env, oid)? else { return Ok(0) };
    let Some((base, base_oid)) = auto_base(&query, session) else { return Ok(0) };
    // The updatable columns of the view in `columns`, each with the column of the base relation that it shows.
    let updatable: Vec<i16> = (1i16..)
        .zip(&query.targets)
        .filter(|(n, _)| columns.is_none_or(|c| c.contains(n)))
        .filter_map(|(_, target)| base_column(target, base))
        .collect();
    let mut events = if updatable.is_empty() { DELETE } else { ALL_EVENTS };
    if !matches!(relkind(base_oid, session), Some('r' | 'p')) {
        // `adjust_view_column_set`. An empty set is null in PostgreSQL, so the base relation then looks at all its columns.
        let base_columns: BTreeSet<i16> = updatable.into_iter().collect();
        let base_columns = (!base_columns.is_empty()).then_some(&base_columns);
        outer.push(oid);
        events &= relation_is_updatable(base_oid, outer, base_columns, session)?;
        outer.pop();
    }
    Ok(events)
}

/// `pg_relation_is_updatable(regclass, boolean)`. The second argument adds the events of the `INSTEAD OF` triggers, and rupg has none.
fn pg_relation_is_updatable(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    Ok(Value::Int4(relation_is_updatable(oid, &mut Vec::new(), None, call.session)?))
}

/// `pg_column_is_updatable(regclass, smallint, boolean)`: true when the relation supports `UPDATE` and `DELETE` for the column. A system column is never updatable.
fn pg_column_is_updatable(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let Some(Value::Int2(attnum)) = args.get(1) else { return Err(bad_value()) };
    if *attnum <= 0 {
        return Ok(Value::Bool(false));
    }
    let columns = BTreeSet::from([*attnum]);
    let events = relation_is_updatable(oid, &mut Vec::new(), Some(&columns), call.session)?;
    Ok(Value::Bool(events & (UPDATE | DELETE) == UPDATE | DELETE))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_relation_is_updatable" => pg_relation_is_updatable,
        "pg_column_is_updatable" => pg_column_is_updatable,
        _ => return None,
    };
    Some(kernel)
}

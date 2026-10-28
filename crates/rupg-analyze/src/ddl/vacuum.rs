//! The relations of `VACUUM` and `ANALYZE`, as `expand_vacuum_rel` and `vacuum_rel` of `vacuum.c` and `analyze_rel` of `analyze.c` check them. The checks give the relations that the statement processes.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{List, Node};

use super::copy::relation_columns;
use super::lock::{class_name, lookup, relkind};
use crate::{Analyzer, Env, Params};

/// The OID of `pg_statistic`, which `analyze_rel` skips.
const PG_STATISTIC: u32 = 2619;

/// A relation of the list of `VACUUM` or `ANALYZE`, as `expand_vacuum_rel` gives it.
#[derive(Debug)]
pub struct VacuumRel {
    pub relid: u32,
    /// The names of the columns of the column list.
    pub columns: Vec<String>,
    /// `inh` of the name, which is false for `ONLY`.
    pub inh: bool,
    /// The result of `vacuum_is_permitted_for_relation`. The statement skips a relation without it, but not its children.
    pub permitted: bool,
}

/// `vacuum` for the relations `rels` of a `VacuumStmt`, after `ExecVacuum` has read the options. `permitted` is `vacuum_is_permitted_for_relation`, which `expand_vacuum_rel` checks after the lookup of each name. `vacuum` is true for `VACUUM`, and `analyze` is true for `ANALYZE` or for `VACUUM` with the option `ANALYZE`. The warnings for the relations that the statement skips go to `warnings`, also when a later relation gives an error. The result is the relations that the statement processes, in the order of the list.
///
/// # Errors
///
/// The errors of `RangeVarGetRelid` for a name, and `42703` and `42701` for a column list of `ANALYZE` with a name that is not a column or that the list gives twice.
pub fn vacuum_relations(
    rels: &List,
    env: &dyn Env,
    vacuum: bool,
    analyze: bool,
    permitted: &dyn Fn(u32) -> bool,
    warnings: &mut Vec<Error>,
) -> Result<Vec<VacuumRel>> {
    let params = Params::default();
    let an = Analyzer::new(env, &params);
    let mut found = Vec::new();
    let mut done = Vec::new();
    for node in rels.iter().flatten() {
        let Node::VacuumRelation(vrel) = node else { continue };
        let Some(rv) = &vrel.relation else { continue };
        let relid = lookup(&an, rv)?;
        let permitted = permitted(relid);
        if !permitted {
            warnings.push(vacuum_denied(&class_name(env, relid), vacuum));
        }
        found.push((relid, &vrel.va_cols, rv.inh, permitted));
        if vacuum && !rv.inh && relkind(env, relid) == b'p' {
            let name = rv.relname.as_deref().unwrap_or_default();
            warnings.push(Error::new(
                SqlState::WARNING,
                format!("VACUUM ONLY of partitioned table \"{name}\" has no effect"),
            ));
        }
    }
    for (relid, va_cols, inh, permitted) in found {
        let columns = va_cols
            .iter()
            .flatten()
            .filter_map(|node| match node {
                Node::String(name) => Some(name.to_string()),
                _ => None,
            })
            .collect();
        let rel = VacuumRel { relid, columns, inh, permitted };
        // `expand_vacuum_rel` adds the children of a relation that the role cannot maintain, and `vacuum_rel` checks each of them.
        if !permitted {
            done.push(rel);
            continue;
        }
        let kind = relkind(env, relid);
        let name = class_name(env, relid);
        if vacuum && !matches!(kind, b'r' | b'm' | b't' | b'p') {
            warnings.push(skipping(&name, "vacuum"));
            continue;
        }
        if !analyze || relid == PG_STATISTIC {
            done.push(rel);
            continue;
        }
        check_columns(env, relid, &name, va_cols)?;
        if !vacuum && !matches!(kind, b'r' | b'm' | b'p') {
            warnings.push(skipping(&name, "analyze"));
            continue;
        }
        done.push(rel);
    }
    Ok(done)
}

/// `validate_va_cols_list`: each name of the list is a user column of the relation, and no name comes twice.
fn check_columns(env: &dyn Env, relid: u32, rel: &str, va_cols: &List) -> Result<()> {
    let columns = relation_columns(env, relid);
    let mut seen = Vec::new();
    for node in va_cols.iter().flatten() {
        let Node::String(name) = node else { continue };
        let name: &str = name;
        let Some(at) = columns.iter().position(|c| c == name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" of relation \"{rel}\" does not exist"),
            ));
        };
        if seen.contains(&at) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{name}\" of relation \"{rel}\" appears more than once"),
            ));
        }
        seen.push(at);
    }
    Ok(())
}

/// The warning of `vacuum_rel` or `analyze_rel` for a relation that is not a table.
fn skipping(name: &str, what: &str) -> Error {
    Error::new(
        SqlState::WARNING,
        format!("skipping \"{name}\" --- cannot {what} non-tables or special system tables"),
    )
}

/// The warning of `vacuum_is_permitted_for_relation` for a relation that the role cannot vacuum or analyze. `vacuum` is true for `VACUUM`, also with the option `ANALYZE`.
pub fn vacuum_denied(name: &str, vacuum: bool) -> Error {
    let what = if vacuum { "vacuum" } else { "analyze" };
    Error::new(SqlState::WARNING, format!("permission denied to {what} \"{name}\", skipping it"))
}

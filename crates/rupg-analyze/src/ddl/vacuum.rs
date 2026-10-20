//! The relations of `VACUUM` and `ANALYZE`, as `expand_vacuum_rel` and `vacuum_rel` of `vacuum.c` and `analyze_rel` of `analyze.c` check them. rupg keeps no dead row versions and no statistics yet, so the statement checks the names and changes nothing.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{List, Node};

use super::copy::relation_columns;
use super::lock::{class_name, lookup, relkind};
use crate::{Analyzer, Env, Params};

/// The OID of `pg_statistic`, which `analyze_rel` skips.
const PG_STATISTIC: u32 = 2619;

/// `vacuum` for the relations `rels` of a `VacuumStmt`, after `ExecVacuum` has read the options. `vacuum` is true for `VACUUM`, and `analyze` is true for `ANALYZE` or for `VACUUM` with the option `ANALYZE`. The warnings for the relations that the statement skips go to `warnings`, also when a later relation gives an error.
///
/// # Errors
///
/// The errors of `RangeVarGetRelid` for a name, and `42703` and `42701` for a column list of `ANALYZE` with a name that is not a column or that the list gives twice.
pub fn vacuum_relations(
    rels: &List,
    env: &dyn Env,
    vacuum: bool,
    analyze: bool,
    warnings: &mut Vec<Error>,
) -> Result<()> {
    let params = Params::default();
    let an = Analyzer::new(env, &params);
    let mut found = Vec::new();
    for node in rels.iter().flatten() {
        let Node::VacuumRelation(vrel) = node else { continue };
        let Some(rv) = &vrel.relation else { continue };
        found.push((lookup(&an, rv)?, &vrel.va_cols));
    }
    for (relid, va_cols) in found {
        let kind = relkind(env, relid);
        let name = class_name(env, relid);
        if vacuum && !matches!(kind, b'r' | b'm' | b't' | b'p') {
            warnings.push(skipping(&name, "vacuum"));
            continue;
        }
        if !analyze || relid == PG_STATISTIC {
            continue;
        }
        check_columns(env, relid, &name, va_cols)?;
        if !vacuum && !matches!(kind, b'r' | b'm' | b'p') {
            warnings.push(skipping(&name, "analyze"));
        }
    }
    Ok(())
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

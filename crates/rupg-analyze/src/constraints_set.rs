//! `SET CONSTRAINTS`, as `AfterTriggerSetState` in `trigger.c` finds the constraints of the statement by their names.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{ConstraintsSetStmt, Node};

use crate::Env;

/// The constraints of a `SET CONSTRAINTS`: `None` for `ALL`, or the OIDs of the deferrable constraints of the names, in the order of the names. A name with no schema finds the constraints with the name in the first schema of `search_path` that has one.
///
/// # Errors
///
/// `3F000` for a schema that does not exist, `42704` for a name that no constraint has, and `42809` for a constraint that is not deferrable when the statement defers it.
pub fn set_constraints(stmt: &ConstraintsSetStmt, env: &dyn Env) -> Result<Option<Vec<u32>>> {
    if stmt.constraints.is_empty() {
        return Ok(None);
    }
    let mut oids = Vec::new();
    for node in stmt.constraints.iter().flatten() {
        let Node::RangeVar(rv) = node else {
            return Err(Error::internal("a constraint of SET CONSTRAINTS that is not RangeVar"));
        };
        let name = rv.relname.as_deref().unwrap_or_default();
        let schemas = match rv.schemaname.as_deref() {
            Some(schema) => match env.namespace(schema) {
                Some(namespace) => vec![namespace],
                None => {
                    return Err(Error::new(
                        SqlState::INVALID_SCHEMA_NAME,
                        format!("schema \"{schema}\" does not exist"),
                    ));
                }
            },
            None => env.search_path(),
        };
        let mut found = false;
        if let Some(catalog) = env.catalog() {
            for namespace in schemas {
                let mut list: Vec<_> = catalog
                    .constraints()
                    .filter(|c| c.namespace == namespace && c.name == name)
                    .collect();
                list.sort_by_key(|c| c.oid);
                for constraint in list {
                    if constraint.deferrable {
                        oids.push(constraint.oid);
                    } else if stmt.deferred {
                        return Err(Error::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!("constraint \"{name}\" is not deferrable"),
                        ));
                    }
                    found = true;
                }
                if found {
                    break;
                }
            }
        }
        if !found {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" does not exist"),
            ));
        }
    }
    Ok(Some(oids))
}

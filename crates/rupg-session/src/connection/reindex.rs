//! `REINDEX`: the parts of `ExecReindex` and `ReindexMultipleTables` of `indexcmds.c` that read the state of the session, before [`rupg_analyze::define`] runs the rest of the statement.

use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{Node, ReindexObjectType, ReindexStmt};

use super::{Connection, Done};
use crate::utility::Notice;

/// `ROLE_PG_MAINTAIN`.
const PG_MAINTAIN: u32 = 6337;

impl Connection {
    /// `ExecReindex`: the options, `PreventInTransactionBlock` of `CONCURRENTLY`, the tablespace of `TABLESPACE`, `PreventInTransactionBlock` of `SCHEMA`, `SYSTEM` and `DATABASE`, and the checks of `ReindexMultipleTables` on the database, then [`Connection::define`] for the rest. `call` is true for a statement of a function or a procedure, which is not at the top level. Each relation of rupg is in `pg_default`, so a move to another tablespace is not supported yet.
    pub(super) fn reindex(
        &self,
        node: &Node,
        stmt: &ReindexStmt,
        text: &str,
        notices: &mut Vec<Notice>,
        call: bool,
    ) -> Result<Done, Error> {
        let options = rupg_analyze::reindex_options(stmt)?;
        let prevent = |statement: &str| {
            if call { Err(self.prevent_in_call(statement)) } else { self.prevent_block(statement) }
        };
        if options.concurrently {
            prevent("REINDEX CONCURRENTLY")?;
        }
        if let Some(name) = &options.tablespace {
            if self.tablespace_oid(name).is_none() {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("tablespace \"{name}\" does not exist"),
                ));
            }
            if name != "pg_default" {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "REINDEX with a tablespace other than pg_default is not supported yet",
                ));
            }
        }
        let statement = match stmt.kind {
            ReindexObjectType::REINDEX_OBJECT_SCHEMA => Some("REINDEX SCHEMA"),
            ReindexObjectType::REINDEX_OBJECT_SYSTEM => Some("REINDEX SYSTEM"),
            ReindexObjectType::REINDEX_OBJECT_DATABASE => Some("REINDEX DATABASE"),
            _ => None,
        };
        if let Some(statement) = statement {
            prevent(statement)?;
            if stmt.kind == ReindexObjectType::REINDEX_OBJECT_SYSTEM && options.concurrently {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot reindex system catalogs concurrently",
                ));
            }
            if stmt.kind != ReindexObjectType::REINDEX_OBJECT_SCHEMA {
                self.check_reindex_database(stmt.name.as_deref())?;
            }
        }
        self.define_with(node, text, notices, self.current_role(), call)
    }

    /// The checks of `ReindexMultipleTables` for `SYSTEM` and `DATABASE`: the name is the name of the current database, and the role owns the database or is a member of `pg_maintain`.
    fn check_reindex_database(&self, name: Option<&str>) -> Result<(), Error> {
        if name.is_some_and(|name| name != self.database) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "can only reindex the currently open database",
            ));
        }
        let roles = self.roles();
        let role = self.current_role();
        let owner = self.store.database(&self.database).map(|row| row.owner);
        if !owner.is_some_and(|owner| roles.has_privs_of_role(role, owner))
            && !roles.has_privs_of_role(role, PG_MAINTAIN)
        {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of database {}", self.database),
            ));
        }
        Ok(())
    }
}

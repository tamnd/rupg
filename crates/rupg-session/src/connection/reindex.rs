//! `REINDEX`: the parts of `ExecReindex` and `ReindexMultipleTables` of `indexcmds.c` that read the state of the session, before [`rupg_analyze::define`] runs the rest of the statement.

use rupg_catalog::RelKind;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{Node, ReindexObjectType, ReindexStmt};

use super::{Connection, Done};
use crate::utility::Notice;

/// `ROLE_PG_MAINTAIN`.
const PG_MAINTAIN: u32 = 6337;

impl Connection {
    /// `ExecReindex`: the options, `PreventInTransactionBlock` of `CONCURRENTLY`, the tablespace of `TABLESPACE`, `PreventInTransactionBlock` of `SCHEMA`, `SYSTEM` and `DATABASE`, and the checks of `ReindexMultipleTables` on the database, then [`Connection::define`] for the rest. `call` is true for a statement of a function or a procedure, which is not at the top level.
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
        if let Some(name) = &options.tablespace
            && self.tablespace_oid(name).is_none()
        {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("tablespace \"{name}\" does not exist"),
            ));
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
            self.lock_tables_first(stmt, options.concurrently)?;
        }
        self.define_fired(node, text, notices, self.current_role(), call)
    }

    /// `ReindexMultipleTables` locks each table in a transaction of its own, so a wait for one table holds no lock of the others. rupg runs the statement in one transaction with the lock of the catalog, so it takes the locks of the tables of the schema or of the database first, in the order of their OIDs, before the lock of the catalog: a session can drop a table while the statement waits for another. The analysis takes the locks again, and leaves out the tables that are gone.
    fn lock_tables_first(&self, stmt: &ReindexStmt, concurrently: bool) -> Result<(), Error> {
        // `ShareLock`, or `ShareUpdateExclusiveLock` for `CONCURRENTLY`.
        let mode = if concurrently { 4 } else { 5 };
        if self.lease.borrow().is_some() {
            return Ok(());
        }
        let catalog = self.catalog();
        let schema = match stmt.kind {
            ReindexObjectType::REINDEX_OBJECT_SCHEMA => {
                let Some(schema) = stmt.name.as_deref().and_then(|n| catalog.schema_by_name(n))
                else {
                    return Ok(());
                };
                let roles = self.roles();
                let role = self.current_role();
                if !roles.has_privs_of_role(role, schema.owner)
                    && !roles.has_privs_of_role(role, PG_MAINTAIN)
                {
                    return Ok(());
                }
                Some(schema.oid)
            }
            ReindexObjectType::REINDEX_OBJECT_DATABASE => None,
            _ => return Ok(()),
        };
        let mut tables: Vec<u32> = catalog
            .relations()
            .filter(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
            .filter(|r| schema.is_none_or(|schema| r.namespace == schema))
            .filter(|r| !catalog.schema(r.namespace).is_some_and(|s| s.name.starts_with("pg_temp")))
            .map(|r| r.oid)
            .collect();
        tables.sort_unstable();
        let reader = self.reader();
        for table in tables {
            reader.lock_relation(table, mode, true)?;
        }
        Ok(())
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
        let owner = self.database_row(&self.database).map(|row| row.owner);
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

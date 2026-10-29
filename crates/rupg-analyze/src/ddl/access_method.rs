//! `CREATE ACCESS METHOD`, as `CreateAccessMethod` of `amcmds.c` runs it, and the table access method of a relation, as `get_table_am_oid` finds it.

use rupg_catalog::{AccessMethod, HEAP_AM, ObjRef, PG_AM};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::CreateAmStmt;
use rupg_types::oid;

use super::Definer;
use crate::typename::names;

impl Definer<'_, '_> {
    /// `CreateAccessMethod`: only a superuser can make an access method. The handler takes `internal` and gives `table_am_handler` for a table access method or `index_am_handler` for an index access method.
    pub(super) fn create_access_method(&mut self, stmt: &CreateAmStmt) -> Result<()> {
        let name = stmt.amname.as_deref().unwrap_or_default();
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create access method \"{name}\""),
            )
            .with_hint("Must be superuser to create an access method.".to_string()));
        }
        if self.access_method_oid(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("access method \"{name}\" already exists"),
            ));
        }
        let (wanted, word) = match stmt.amtype {
            b't' => (oid::TABLE_AM_HANDLER, "table_am_handler"),
            b'i' => (oid::INDEX_AM_HANDLER, "index_am_handler"),
            other => {
                return Err(Error::internal(format!(
                    "unrecognized access method type \"{}\"",
                    char::from(other)
                )));
            }
        };
        let func = names(&stmt.handler_name);
        let (handler, returns) = self.func_oid(&func, &[oid::INTERNAL])?;
        if returns != wanted {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("function {} must return type {word}", func.join(".")),
            ));
        }
        let new = AccessMethod { oid: 0, name: name.to_string(), handler, kind: stmt.amtype };
        let oid = self.catalog.create_access_method(new);
        self.collect(None, ObjRef::new(PG_AM, oid));
        Ok(())
    }

    /// `get_am_oid`: the access method with the name, built in or made by a statement.
    pub(super) fn access_method_oid(&self, name: &str) -> Option<u32> {
        builtin::access_method_oid(name)
            .or_else(|| self.catalog.access_method_by_name(name).map(|m| m.oid))
    }

    /// `get_table_am_oid`: the table access method with the name. `heap` is the only built-in table access method.
    pub(super) fn table_am(&self, name: &str) -> Result<u32> {
        let found = match builtin::access_method_oid(name) {
            Some(oid) => Some((oid, if oid == HEAP_AM { b't' } else { b'i' })),
            None => self.catalog.access_method_by_name(name).map(|m| (m.oid, m.kind)),
        };
        match found {
            Some((oid, b't')) => Ok(oid),
            Some(_) => Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("access method \"{name}\" is not of type TABLE"),
            )),
            None => Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("access method \"{name}\" does not exist"),
            )),
        }
    }

    /// The table access method of `default_table_access_method`.
    pub(super) fn default_table_am(&self) -> Result<u32> {
        self.table_am(&self.an.env.default_table_access_method())
    }

    /// The text of `getObjectDescription` for an access method.
    pub(super) fn describe_access_method(&self, oid: u32) -> String {
        let name = builtin::access_method_name(oid)
            .or_else(|| self.catalog.access_method(oid).map(|m| m.name.as_str()));
        match name {
            Some(name) => format!("access method {name}"),
            None => format!("access method {oid}"),
        }
    }
}

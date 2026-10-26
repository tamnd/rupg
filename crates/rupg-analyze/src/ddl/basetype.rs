//! `CREATE TYPE` of a base type and of a shell type, as `DefineType` runs it.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::DefineStmt;

use super::{Definer, not_yet};
use crate::typename::names;

impl Definer<'_, '_> {
    /// `DefineType`: a shell type when the statement has no parameters. Only a superuser makes a base type, and the check comes first, as for a shell type too.
    pub(super) fn define_type(&mut self, stmt: &DefineStmt) -> Result<()> {
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create a base type",
            ));
        }
        let parts = names(&stmt.defnames);
        let (schema, name) = self.an.split_name(&parts, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        if !stmt.definition.is_empty() {
            return Err(not_yet("CREATE TYPE of a base type", None));
        }
        self.check_type_name(namespace, name)?;
        // `TypeShellMake` gives a shell type no privileges, so the default privileges do not apply.
        self.catalog.create_shell_type(namespace, name, self.user).map(|_| ())
    }
}

//! The tablespace of a new relation, as `DefineRelation` and `DefineIndex` of `tablecmds.c` and `indexcmds.c` choose it, with `GetDefaultTablespace` of `tablespace.c`, and the move of a relation to another tablespace, as `ALTER TABLE ... SET TABLESPACE`, `ALTER TABLE ALL IN TABLESPACE` and `REINDEX (TABLESPACE)` do it.

use rupg_catalog::{ObjRef, PG_CLASS, PG_TOAST_NAMESPACE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterTableMoveAllStmt, Node, ObjectType};

use super::comment::not_owner;
use super::drop::owner_word;
use super::{Definer, Found};

/// `GLOBALTABLESPACE_OID`: the tablespace of the shared catalogs.
pub(super) const GLOBAL_TABLESPACE: u32 = 1664;

/// `PG_CATALOG_NAMESPACE`.
const PG_CATALOG_NAMESPACE: u32 = 11;

impl Definer<'_, '_> {
    /// The tablespace of a new relation. `name` is the name of the `TABLESPACE` clause. `inherited` is the tablespace that the relation takes when it has no clause, as a partition takes the tablespace of its parent. A temporary relation has `temp`, and a partitioned table or a partitioned index has `partitioned`. The result is 0 for the default tablespace of the database, as `heap_create` stores it.
    pub(super) fn relation_tablespace(
        &self,
        name: Option<&str>,
        inherited: u32,
        temp: bool,
        partitioned: bool,
    ) -> Result<u32> {
        let database = self.an.env.database_tablespace();
        let mut tablespace = match name {
            Some(name) => {
                let oid = self.an.env.tablespace_oid(name).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("tablespace \"{name}\" does not exist"),
                    )
                })?;
                if partitioned && oid == database {
                    return Err(partitioned_default());
                }
                oid
            }
            None => inherited,
        };
        if tablespace == 0 {
            tablespace = self.default_tablespace(temp, partitioned)?;
        }
        if tablespace != 0
            && tablespace != database
            && !self.an.env.tablespace_create_allowed(tablespace)
        {
            let name = self.an.env.tablespace_name(tablespace).unwrap_or_default();
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {name}"),
            ));
        }
        if tablespace == GLOBAL_TABLESPACE {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ));
        }
        Ok(if tablespace == database { 0 } else { tablespace })
    }

    /// `get_tablespace_oid` and the check of `CREATE` on the tablespace of `ATPrepSetTableSpace` and `ExecReindex`. The default tablespace of the database needs no privilege.
    pub(super) fn target_tablespace(&self, name: &str) -> Result<u32> {
        let oid = self.an.env.tablespace_oid(name).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_OBJECT, format!("tablespace \"{name}\" does not exist"))
        })?;
        if oid != self.an.env.database_tablespace() && !self.an.env.tablespace_create_allowed(oid) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {name}"),
            ));
        }
        Ok(oid)
    }

    /// `CheckRelationTableSpaceMove` and `SetRelationTableSpace`: false when the relation is in the tablespace already. A relation of the built-in catalog cannot move, and no relation can go to `pg_global`. The default tablespace of the database is stored as 0.
    pub(super) fn move_tablespace(&mut self, oid: u32, tablespace: u32) -> Result<bool> {
        let database = self.an.env.database_tablespace();
        let Some(rel) = self.catalog.relation(oid) else {
            if tablespace == database {
                return Ok(false);
            }
            let name = rupg_pgcatalog::builtin::class_by_oid(oid).map_or("", |r| r.name);
            return Err(system_move(name));
        };
        if tablespace == rel.tablespace || (tablespace == database && rel.tablespace == 0) {
            return Ok(false);
        }
        if tablespace == GLOBAL_TABLESPACE {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ));
        }
        if self.other_temp(rel.namespace) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move temporary tables of other sessions",
            ));
        }
        self.catalog.set_tablespace(oid, if tablespace == database { 0 } else { tablespace })?;
        Ok(true)
    }

    /// `ATExecSetTableSpace` and `ATExecSetTableSpaceNoStorage`: the relation moves to the tablespace, and a relation with storage gets a new file. The TOAST table of a table and the index of the TOAST table move with it, each with a new file. The indexes of the table stay where they are.
    pub(super) fn set_relation_tablespace(&mut self, oid: u32, tablespace: u32) -> Result<()> {
        if !self.move_tablespace(oid, tablespace)? {
            return Ok(());
        }
        let Some(rel) = self.catalog.relation(oid) else { return Ok(()) };
        if matches!(rel.kind, RelKind::Partitioned | RelKind::PartitionedIndex) {
            return Ok(());
        }
        let toast =
            if matches!(rel.kind, RelKind::Table | RelKind::MatView) { rel.toast } else { 0 };
        self.catalog.set_new_file(oid)?;
        if toast != 0 {
            self.catalog.set_new_file(toast)?;
            for index in self.indexes_on(toast) {
                self.catalog.set_new_file(index)?;
            }
        }
        Ok(())
    }

    /// `AlterTableMoveAll`: `ALTER TABLE`, `ALTER INDEX` or `ALTER MATERIALIZED VIEW ALL IN TABLESPACE ... SET TABLESPACE`. Each relation of the kind in the first tablespace goes to the second one, except the relations of the system schemas, of the temporary schemas and of TOAST. `OWNED BY` keeps the relations of the roles. The role must own each relation that moves.
    pub(super) fn alter_table_move_all(&mut self, stmt: &AlterTableMoveAllStmt) -> Result<()> {
        let kinds: &[RelKind] = match stmt.objtype {
            ObjectType::OBJECT_TABLE => &[RelKind::Table, RelKind::Partitioned],
            ObjectType::OBJECT_INDEX => &[RelKind::Index, RelKind::PartitionedIndex],
            ObjectType::OBJECT_MATVIEW => &[RelKind::MatView],
            _ => {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "only tables, indexes, and materialized views exist in tablespaces",
                ));
            }
        };
        let orig_name = stmt.orig_tablespacename.as_deref().unwrap_or_default();
        let new_name = stmt.new_tablespacename.as_deref().unwrap_or_default();
        let lookup = |name: &str| {
            self.an.env.tablespace_oid(name).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("tablespace \"{name}\" does not exist"),
                )
            })
        };
        let orig = lookup(orig_name)?;
        let new = lookup(new_name)?;
        if orig == GLOBAL_TABLESPACE || new == GLOBAL_TABLESPACE {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "cannot move relations in to or out of pg_global tablespace",
            ));
        }
        self.target_tablespace(new_name)?;
        let database = self.an.env.database_tablespace();
        let stored = |oid: u32| if oid == database { 0 } else { oid };
        if stored(orig) == stored(new) {
            return Ok(());
        }
        let mut roles = Vec::new();
        for role in stmt.roles.iter().flatten() {
            if let Node::RoleSpec(role) = role {
                roles.push(self.role_oid(role)?);
            }
        }
        let mut found: Vec<(u32, u32, String)> = self
            .catalog
            .relations()
            .filter(|r| r.tablespace == stored(orig) && kinds.contains(&r.kind))
            .filter(|r| {
                r.namespace != PG_TOAST_NAMESPACE
                    && r.namespace != PG_CATALOG_NAMESPACE
                    && !self.catalog.is_temp_schema(r.namespace)
            })
            .filter(|r| roles.is_empty() || roles.contains(&r.owner))
            .map(|r| (r.oid, r.owner, r.name.clone()))
            .collect();
        found.sort_by_key(|r| r.0);
        for (oid, owner, name) in &found {
            if !self.is_superuser() && !self.owns(*owner) {
                return Err(not_owner(owner_word(self.kind_of(Found::User(*oid))), name));
            }
        }
        if found.is_empty() {
            let shown = if stored(orig) == 0 { "(database default)" } else { orig_name };
            self.notice(
                SqlState::NO_DATA_FOUND,
                format!("no matching relations in tablespace \"{shown}\" found"),
            );
        }
        for (oid, _, _) in found {
            self.set_relation_tablespace(oid, new)?;
            self.collect(None, ObjRef::new(PG_CLASS, oid));
        }
        Ok(())
    }

    /// `GetDefaultTablespace`: the tablespace of `default_tablespace`, or 0 when the setting is empty, when the tablespace is not there, or for a temporary relation. The default tablespace of the database gives 0, but a partitioned relation cannot use it.
    fn default_tablespace(&self, temp: bool, partitioned: bool) -> Result<u32> {
        if temp {
            return Ok(0);
        }
        let name = self.an.env.default_tablespace();
        if name.is_empty() {
            return Ok(0);
        }
        let Some(oid) = self.an.env.tablespace_oid(&name) else { return Ok(0) };
        if oid == self.an.env.database_tablespace() {
            if partitioned {
                return Err(partitioned_default());
            }
            return Ok(0);
        }
        Ok(oid)
    }
}

/// The error of `CheckRelationTableSpaceMove` and `reindex_index` for a relation of the system.
pub(super) fn system_move(name: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot move system relation \"{name}\""))
}

fn partitioned_default() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "cannot specify default tablespace for partitioned relations",
    )
}

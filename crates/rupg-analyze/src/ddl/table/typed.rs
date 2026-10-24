//! Typed tables, `CREATE TABLE ... OF`: the check of `check_of_type`, the columns that `transformOfType` takes from the composite type and that `MergeAttributes` merges with the column options of the statement, `ALTER TABLE ... OF` and `NOT OF` as `ATExecAddOf` and `ATExecDropOf` run them, and the typed tables of `find_typed_table_dependencies`.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{DropBehavior, TypeName};

use super::super::Definer;
use super::super::alter::Target;
use super::{NewColumn, Plan};
use crate::types;

impl Definer<'_, '_> {
    /// `typenameType` and `check_of_type`: the OID of the type, which must be a composite type of `CREATE TYPE ... AS` and not the row type of a table.
    pub(in crate::ddl) fn of_type(&mut self, name: &TypeName) -> Result<u32> {
        let (ty, _) = self.an.type_name(name)?;
        let standalone = match self.catalog.type_by_oid(ty) {
            Some(row) if row.relation != 0 => Some(
                self.catalog.relation(row.relation).is_some_and(|r| r.kind == RelKind::Composite),
            ),
            Some(_) => None,
            // A built-in composite type is the row type of a system catalog or of a view.
            None => types::row(ty).filter(|row| row.kind == b'c').map(|_| false),
        };
        match standalone {
            Some(true) => Ok(ty),
            Some(false) => Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("type {} is the row type of another table", self.type_text(ty)),
            )
            .with_detail(
                "A typed table must use a stand-alone composite type created with CREATE TYPE.",
            )),
            None => Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("type {} is not a composite type", self.type_text(ty)),
            )),
        }
    }

    /// `transformOfType` and the merge of `MergeAttributes`: the columns of the type come first, and a column of the statement gives its options to the column of the type with the same name.
    pub(super) fn merge_type_columns(&self, plan: &mut Plan, ty: u32) -> Result<()> {
        let relation = self.catalog.type_by_oid(ty).map_or(0, |row| row.relation);
        let rel = self.catalog.relation(relation).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for relation {relation}"))
        })?;
        let mut columns: Vec<(NewColumn, bool)> = rel
            .columns
            .iter()
            .filter(|c| !c.dropped)
            .map(|c| {
                let type_name = TypeName {
                    typeOid: c.ty,
                    typemod: c.typmod,
                    location: -1,
                    ..TypeName::default()
                };
                let column = NewColumn {
                    name: c.name.clone(),
                    type_name,
                    collate: None,
                    collation: c.collation,
                    is_not_null: false,
                    default: None,
                    identity: 0,
                    generated: 0,
                    storage: None,
                    compression: None,
                    inherited: None,
                };
                (column, true)
            })
            .collect();
        // The columns of the type come first in the list of `MergeAttributes`, so a second option list for a column fails before a column that the type does not have.
        let mut missing = None;
        for local in std::mem::take(&mut plan.columns) {
            let Some((column, from_type)) = columns.iter_mut().find(|(c, _)| c.name == local.name)
            else {
                missing = missing.or(Some(local.name));
                continue;
            };
            if !*from_type {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", local.name),
                ));
            }
            column.is_not_null = local.is_not_null;
            column.default = local.default;
            column.identity = local.identity;
            *from_type = false;
        }
        if let Some(name) = missing {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" does not exist"),
            ));
        }
        plan.columns = columns.into_iter().map(|(c, _)| c).collect();
        Ok(())
    }

    /// The check of `ATPrepAddColumn`, `ATPrepDropColumn`, `ATPrepAlterColumnType` and `renameatt_check`: the columns of a typed table come from its type, so only a change of the type changes them.
    pub(in crate::ddl) fn check_not_typed(&self, oid: u32, message: &str) -> Result<()> {
        if self.catalog.relation(oid).is_some_and(|r| r.of_type != 0) {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message.to_string()));
        }
        Ok(())
    }

    /// `find_typed_table_dependencies`: the typed tables of the composite type `target`. With `RESTRICT`, a typed table is an error.
    pub(in crate::ddl) fn typed_tables(
        &self,
        target: &Target,
        behavior: DropBehavior,
    ) -> Result<Vec<Target>> {
        let ty = self.catalog.relation(target.oid).map_or(0, |r| r.row_type);
        let tables: Vec<u32> = self
            .catalog
            .relations()
            .filter(|r| ty != 0 && r.of_type == ty)
            .map(|r| r.oid)
            .collect();
        if !tables.is_empty() && behavior != DropBehavior::DROP_CASCADE {
            return Err(Error::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot alter type \"{}\" because it is the type of a typed table",
                    target.name
                ),
            )
            .with_hint("Use ALTER ... CASCADE to alter the typed tables too."));
        }
        Ok(tables.into_iter().map(|oid| self.target_of(oid)).collect())
    }

    /// `ATExecAddOf`: the table becomes a typed table of the type. The table must have no parents, and its columns that are not dropped must have the names, the types and the collations of the attributes of the type, in the same order.
    pub(in crate::ddl) fn add_of(&mut self, target: &Target, name: &TypeName) -> Result<()> {
        let ty = self.of_type(name)?;
        let lookup = |oid: u32| Error::internal(format!("cache lookup failed for relation {oid}"));
        let rel = self.catalog.relation(target.oid).cloned().ok_or_else(|| lookup(target.oid))?;
        if rel.partition_of.is_some() || rel.inherits.iter().any(|&p| p != 0) {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, "typed tables cannot inherit"));
        }
        let relation = self.catalog.type_by_oid(ty).map_or(0, |row| row.relation);
        let shape = self.catalog.relation(relation).cloned().ok_or_else(|| lookup(relation))?;
        let mismatch = |message: String| Error::new(SqlState::DATATYPE_MISMATCH, message);
        let mut table = rel.columns.iter().filter(|c| !c.dropped);
        for attr in shape.columns.iter().filter(|c| !c.dropped) {
            let Some(column) = table.next() else {
                return Err(mismatch(format!("table is missing column \"{}\"", attr.name)));
            };
            if column.name != attr.name {
                return Err(mismatch(format!(
                    "table has column \"{}\" where type requires \"{}\"",
                    column.name, attr.name
                )));
            }
            if column.ty != attr.ty
                || column.typmod != attr.typmod
                || column.collation != attr.collation
            {
                return Err(mismatch(format!(
                    "table \"{}\" has different type for column \"{}\"",
                    rel.name, attr.name
                )));
            }
        }
        if let Some(extra) = table.next() {
            return Err(mismatch(format!("table has extra column \"{}\"", extra.name)));
        }
        self.catalog.set_of_type(target.oid, ty)
    }

    /// `ATExecDropOf`: the table is no longer a typed table, and it keeps its columns.
    pub(in crate::ddl) fn drop_of(&mut self, target: &Target) -> Result<()> {
        if self.catalog.relation(target.oid).is_none_or(|r| r.of_type == 0) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a typed table", target.name),
            ));
        }
        self.catalog.set_of_type(target.oid, 0)
    }
}

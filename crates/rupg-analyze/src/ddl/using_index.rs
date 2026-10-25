//! `ALTER TABLE ... ADD CONSTRAINT ... USING INDEX`: a primary key or a unique constraint on an existing unique index of the table. `transformIndexConstraint` checks the index and finds its columns, and `ATExecAddIndexConstraint` makes the constraint for the index.

use rupg_catalog::{ConKind, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{ConstrType, Constraint};

use super::alter::Target;
use super::{Definer, Found};
use crate::coerce::AtOpt;
use crate::typcache::{BTREE, default_opclass};
use crate::typename::place;

impl Definer<'_, '_> {
    /// `transformIndexConstraint` for a constraint with `USING INDEX`: the OID of the index and the names of its key columns. The index must be a valid unique `btree` index of the table with no constraint, no expression and no predicate, and each key column must have the default operator class, collation and sort options.
    pub(super) fn constraint_index_of(
        &self,
        target: &Target,
        con: &Constraint,
    ) -> Result<(u32, Vec<String>)> {
        let name = con.indexname.as_deref().unwrap_or_default();
        let at = place(con.location);
        let fail = |state: SqlState, message: String| Error::new(state, message).at_opt(at);
        let unusable = |message: String| {
            fail(SqlState::WRONG_OBJECT_TYPE, message).with_detail(
                "Cannot create a primary key or unique constraint using such an index.",
            )
        };
        let prerequisite = SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE;
        let oid = match self.relation_in(target.namespace, name) {
            None => {
                return Err(fail(
                    SqlState::UNDEFINED_OBJECT,
                    format!("index \"{name}\" does not exist"),
                ));
            }
            Some(Found::Builtin(_)) => {
                return Err(fail(
                    prerequisite,
                    format!("index \"{name}\" does not belong to table \"{}\"", target.name),
                ));
            }
            Some(Found::User(oid)) => oid,
        };
        let rel = self.catalog.relation(oid);
        let rel = rel.filter(|r| matches!(r.kind, RelKind::Index | RelKind::PartitionedIndex));
        let Some((index, info)) = rel.and_then(|r| Some((r, r.index.as_ref()?))) else {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index"),
            ));
        };
        let associated = self
            .catalog
            .constraints()
            .any(|c| c.index == oid && matches!(c.kind, ConKind::Primary | ConKind::Unique));
        if associated {
            return Err(fail(
                prerequisite,
                format!("index \"{name}\" is already associated with a constraint"),
            ));
        }
        if info.table != target.oid {
            return Err(fail(
                prerequisite,
                format!("index \"{name}\" does not belong to table \"{}\"", target.name),
            ));
        }
        if !info.valid {
            return Err(fail(prerequisite, format!("index \"{name}\" is not valid")));
        }
        if !info.unique {
            return Err(unusable(format!("\"{name}\" is not a unique index")));
        }
        if info.exprs.is_some() {
            return Err(unusable(format!("index \"{name}\" contains expressions")));
        }
        if info.predicate.is_some() {
            return Err(unusable(format!("\"{name}\" is a partial index")));
        }
        if !info.immediate && !con.deferrable {
            return Err(fail(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is a deferrable index"),
            )
            .with_detail("Cannot create a non-deferrable constraint using a deferrable index."));
        }
        if info.method != BTREE {
            return Err(fail(
                SqlState::WRONG_OBJECT_TYPE,
                format!("index \"{name}\" is not a btree"),
            ));
        }
        let table = self.catalog.relation(target.oid);
        let key_count = usize::try_from(info.key_count).unwrap_or(0);
        let mut keys = Vec::new();
        for (i, &attnum) in info.keys.iter().enumerate().take(key_count) {
            let Some(column) = table.and_then(|t| t.column(attnum)) else {
                return Err(Error::internal(format!("cache lookup failed for attribute {attnum}")));
            };
            let class = default_opclass(column.ty, info.method).map_or(0, |c| c.oid);
            let options = index.columns.get(i).is_some_and(|c| !c.options.is_empty());
            if info.classes.get(i) != Some(&class)
                || info.collations.get(i) != Some(&column.collation)
                || options
                || info.options.get(i).is_some_and(|&o| o != 0)
            {
                return Err(unusable(format!(
                    "index \"{name}\" column number {} does not have default sorting behavior",
                    i + 1
                )));
            }
            keys.push(column.name.clone());
        }
        Ok((oid, keys))
    }

    /// `ATExecAddIndexConstraint`: the index of `USING INDEX` gets the constraint. A constraint name that is not the name of the index renames the index. A primary key needs the checks of `index_check_primary_key`.
    pub(super) fn add_index_constraint(&mut self, target: &Target, con: &Constraint) -> Result<()> {
        if target.kind == b'p' {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
            ));
        }
        let (index, keys) = self.constraint_index_of(target, con)?;
        let index_name = con.indexname.as_deref().unwrap_or_default();
        if let Some(name) = con.conname.as_deref().filter(|&n| n != index_name) {
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!(
                    "ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index \"{index_name}\" to \"{name}\""
                ),
            );
            self.catalog.rename_relation(index, name)?;
        }
        let primary = con.contype == ConstrType::CONSTR_PRIMARY;
        if primary {
            let invalid = |message: String| Error::new(SqlState::INVALID_TABLE_DEFINITION, message);
            if self.catalog.constraints_of(target.oid).any(|c| c.kind == ConKind::Primary) {
                return Err(invalid(format!(
                    "multiple primary keys for table \"{}\" are not allowed",
                    target.name
                )));
            }
            let info = self.catalog.relation(index).and_then(|r| r.index.as_ref());
            if info.is_some_and(|i| i.nulls_not_distinct) {
                return Err(invalid("primary keys cannot use NULLS NOT DISTINCT indexes".into()));
            }
            let table = self.catalog.relation(target.oid);
            for key in &keys {
                let column =
                    table.and_then(|t| t.column_number(key)).and_then(|n| table?.column(n));
                if column.is_some_and(|c| !c.not_null) {
                    return Err(invalid(format!(
                        "primary key column \"{key}\" is not marked NOT NULL"
                    )));
                }
            }
        }
        let kind = if primary { ConKind::Primary } else { ConKind::Unique };
        self.catalog.attach_index_constraint(index, kind, con.deferrable, con.initdeferred)?;
        Ok(())
    }
}

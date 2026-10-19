//! `ALTER ... RENAME` of a relation, a column, a constraint of a table and a schema, as `RenameRelation`, `renameatt`, `RenameConstraint` and `RenameSchema` run them.
//!
//! The catalog keeps the text of a view, and the text names the objects that the query reads. So a rename gives the views that read the renamed objects in [`super::Defined::views`], and the caller writes their text again from the query of the old names.

use rupg_catalog::{ConKind, ObjRef, PG_CLASS, PG_PROC, PG_REWRITE, PG_TYPE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{ObjectType, RangeVar, RenameStmt};

use super::alter::Target;
use super::comment::not_owner;
use super::drop::owner_word;
use super::{Definer, Found, not_yet, relkind_detail, relname};
use crate::from::system_column;

/// True for the kinds of objects of `RenameStmt` that [`super::define`] runs.
pub(super) fn is_rename_kind(kind: ObjectType) -> bool {
    matches!(
        kind,
        ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_COLUMN
            | ObjectType::OBJECT_TABCONSTRAINT
            | ObjectType::OBJECT_SCHEMA
    )
}

impl Definer<'_, '_> {
    /// `ExecRenameStmt` for the kinds of [`is_rename_kind`].
    pub(super) fn rename(&mut self, stmt: &RenameStmt) -> Result<()> {
        let newname = stmt.newname.as_deref().unwrap_or_default();
        match stmt.renameType {
            ObjectType::OBJECT_SCHEMA => {
                self.rename_schema(stmt.subname.as_deref().unwrap_or_default(), newname)
            }
            ObjectType::OBJECT_COLUMN => self.rename_column(stmt, newname),
            ObjectType::OBJECT_TABCONSTRAINT => self.rename_table_constraint(stmt, newname),
            _ => self.rename_relation(stmt, newname),
        }
    }

    /// The lookup of the relation of a rename. With `IF EXISTS`, a relation that does not exist gives a notice and `None`.
    fn rename_lookup(&mut self, rv: &RangeVar, missing_ok: bool) -> Result<Option<Found>> {
        match self.lookup_relation(rv) {
            Ok(found) => Ok(Some(found)),
            Err(e)
                if missing_ok
                    && matches!(
                        e.state(),
                        SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                    ) =>
            {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("relation \"{}\" does not exist, skipping", relname(rv)),
                );
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// `RenameRelation` and `RenameRelationInternal`. `ALTER INDEX` can rename a relation of any kind, as `RangeVarCallbackForAlterRelation` allows it for a rename.
    fn rename_relation(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let Some(found) = self.rename_lookup(&rv, stmt.missing_ok)? else { return Ok(()) };
        let objtype = match stmt.renameType {
            ObjectType::OBJECT_INDEX => ObjectType::OBJECT_TABLE,
            other => other,
        };
        let target = self.alter_target(objtype, found, relname(&rv))?;
        let row_type = self.catalog.relation(target.oid).map_or(0, |r| r.row_type);
        self.catalog.rename_relation(target.oid, newname)?;
        self.views_using(|object| match object.class {
            PG_CLASS => object.oid == target.oid,
            PG_TYPE => row_type != 0 && object.oid == row_type,
            _ => false,
        })
    }

    /// `RangeVarCallbackForRenameAttribute` and `renameatt_check`: the relation of `RENAME COLUMN` and `RENAME CONSTRAINT`. A composite type and an index can have their columns renamed.
    fn rename_target(&mut self, rv: &RangeVar, missing_ok: bool) -> Result<Option<Target>> {
        let Some(found) = self.rename_lookup(rv, missing_ok)? else { return Ok(None) };
        let name = relname(rv);
        let kind = self.kind_of(found);
        if !b"rvmciIfp".contains(&kind) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot rename columns of relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && owner != self.user {
            return Err(not_owner(owner_word(kind), name));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let Found::User(oid) = found else {
            return Err(not_yet("ALTER on a built-in relation", None));
        };
        let namespace = self.catalog.relation(oid).map_or(0, |r| r.namespace);
        Ok(Some(Target { oid, kind, name: name.to_string(), namespace }))
    }

    /// `renameatt` and `renameatt_internal`: the new name of a column of a table, a view, an index or a composite type.
    fn rename_column(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let Some(target) = self.rename_target(&rv, stmt.missing_ok)? else { return Ok(()) };
        let column = stmt.subname.as_deref().unwrap_or_default();
        let rel = self.catalog.relation(target.oid);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            if system_column(column).is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot rename system column \"{column}\""),
                ));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" does not exist"),
            ));
        };
        // `check_for_column_name_collision`.
        if system_column(newname).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column name \"{newname}\" conflicts with a system column name"),
            ));
        }
        if rel.is_some_and(|r| r.column_number(newname).is_some()) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{newname}\" of relation \"{}\" already exists", target.name),
            ));
        }
        let row_type = rel.map_or(0, |r| r.row_type);
        let is_view = rel.is_some_and(|r| r.kind == RelKind::View);
        self.catalog.rename_column(target.oid, number, newname)?;
        // The text of a view names its columns in the query, so a view whose column gets a new name gets a new text too.
        self.views_using(|object| match object.class {
            PG_CLASS => object.oid == target.oid,
            PG_TYPE => row_type != 0 && object.oid == row_type,
            _ => false,
        })?;
        if is_view && !self.views.contains(&target.oid) {
            self.views.push(target.oid);
        }
        Ok(())
    }

    /// `RenameConstraint` of a constraint of a table. The index of a primary key or a unique constraint gets the new name, and the constraint gets it from the index.
    fn rename_table_constraint(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let Some(target) = self.rename_target(&rv, stmt.missing_ok)? else { return Ok(()) };
        let name = stmt.subname.as_deref().unwrap_or_default();
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some((oid, kind, index)) = found.map(|c| (c.oid, c.kind, c.index)) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" for table \"{}\" does not exist", target.name),
            ));
        };
        if target.kind == b'r' && matches!(kind, ConKind::Primary | ConKind::Unique) && index != 0 {
            return self.catalog.rename_relation(index, newname);
        }
        self.catalog.rename_constraint(oid, newname)
    }

    /// `RenameSchema`. The views that read an object of the schema get a new text.
    fn rename_schema(&mut self, name: &str, newname: &str) -> Result<()> {
        let Some(schema) = self.catalog.schema_by_name(name) else {
            if builtin::named(Named::Namespace).iter().any(|n| n.name == name) {
                return Err(not_yet("ALTER SCHEMA RENAME of a built-in schema", None));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_SCHEMA,
                format!("schema \"{name}\" does not exist"),
            ));
        };
        let (oid, owner) = (schema.oid, schema.owner);
        let exists = self.catalog.schema_by_name(newname).is_some()
            || builtin::named(Named::Namespace).iter().any(|n| n.name == newname);
        if exists {
            return Err(Error::new(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{newname}\" already exists"),
            ));
        }
        if !self.is_superuser() && owner != self.user {
            return Err(not_owner("schema", name));
        }
        if newname.starts_with("pg_") && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::RESERVED_NAME,
                format!("unacceptable schema name \"{newname}\""),
            )
            .with_detail("The prefix \"pg_\" is reserved for system schemas."));
        }
        self.catalog.rename_schema(oid, newname)?;
        let catalog = &*self.catalog;
        let in_schema = |object: ObjRef| match object.class {
            PG_CLASS => catalog.relation(object.oid).is_some_and(|r| r.namespace == oid),
            PG_TYPE => catalog.type_by_oid(object.oid).is_some_and(|t| t.namespace == oid),
            PG_PROC => catalog.function(object.oid).is_some_and(|f| f.namespace == oid),
            _ => false,
        };
        let objects: Vec<ObjRef> = catalog
            .depends()
            .iter()
            .map(|d| d.referenced)
            .filter(|&object| in_schema(object))
            .collect();
        self.views_using(|object| objects.contains(&object))
    }

    /// Adds to [`super::Defined::views`] the views whose query reads an object that `uses` accepts. A rule that is not the rule of a view and reads such an object gives `0A000`, as rupg cannot write the text of a rule again yet.
    fn views_using(&mut self, uses: impl Fn(ObjRef) -> bool) -> Result<()> {
        let rules: Vec<u32> = self
            .catalog
            .depends()
            .iter()
            .filter(|d| d.object.class == PG_REWRITE && uses(d.referenced))
            .map(|d| d.object.oid)
            .collect();
        for rule in rules {
            let Some(view) = self.catalog.view_of_rule(rule).map(|v| v.oid) else {
                return Err(not_yet("ALTER ... RENAME of an object that a rule reads", None));
            };
            if !self.views.contains(&view) {
                self.views.push(view);
            }
        }
        Ok(())
    }
}

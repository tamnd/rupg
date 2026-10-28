//! `ALTER ... RENAME` of a relation, a column, a constraint of a table and a schema, as `RenameRelation`, `renameatt`, `RenameConstraint` and `RenameSchema` run them. A type and a function have their own modules.
//!
//! The catalog keeps the text of a view, and the text names the objects that the query reads. So a rename gives the views that read the renamed objects in [`super::Defined::views`], and the caller writes their text again from the query of the old names.

use rupg_catalog::{
    ConKind, DepKind, ObjRef, PG_CLASS, PG_POLICY, PG_PROC, PG_REWRITE, PG_TYPE, RelKind,
};
use rupg_common::{Error, Result, SqlState};
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
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_FOREIGN_TABLE
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_COLUMN
            | ObjectType::OBJECT_ATTRIBUTE
            | ObjectType::OBJECT_TABCONSTRAINT
            | ObjectType::OBJECT_DOMCONSTRAINT
            | ObjectType::OBJECT_SCHEMA
            | ObjectType::OBJECT_TYPE
            | ObjectType::OBJECT_DOMAIN
            | ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE
            | ObjectType::OBJECT_TRIGGER
            | ObjectType::OBJECT_RULE
            | ObjectType::OBJECT_POLICY
            | ObjectType::OBJECT_OPCLASS
            | ObjectType::OBJECT_OPFAMILY
            | ObjectType::OBJECT_STATISTIC_EXT
            | ObjectType::OBJECT_CONVERSION
            | ObjectType::OBJECT_FDW
            | ObjectType::OBJECT_FOREIGN_SERVER
            | ObjectType::OBJECT_PUBLICATION
            | ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSCONFIGURATION
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
            ObjectType::OBJECT_COLUMN | ObjectType::OBJECT_ATTRIBUTE => {
                self.rename_column(stmt, newname)
            }
            ObjectType::OBJECT_TABCONSTRAINT => self.rename_table_constraint(stmt, newname),
            ObjectType::OBJECT_DOMCONSTRAINT => self.rename_domain_constraint(
                stmt.object.as_ref(),
                stmt.subname.as_deref().unwrap_or_default(),
                newname,
            ),
            ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN => self.rename_type(stmt),
            ObjectType::OBJECT_TRIGGER => self.rename_trigger(stmt, newname),
            ObjectType::OBJECT_RULE => self.rename_rule(stmt, newname),
            ObjectType::OBJECT_POLICY => self.rename_policy(stmt, newname),
            ObjectType::OBJECT_OPCLASS | ObjectType::OBJECT_OPFAMILY => self.rename_opcf(stmt),
            ObjectType::OBJECT_STATISTIC_EXT => self.rename_stats(stmt),
            ObjectType::OBJECT_CONVERSION => self.rename_conversion(stmt),
            ObjectType::OBJECT_FDW => self.rename_foreign(true, stmt),
            ObjectType::OBJECT_FOREIGN_SERVER => self.rename_foreign(false, stmt),
            ObjectType::OBJECT_PUBLICATION => self.rename_publication(stmt),
            ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSCONFIGURATION => self.rename_ts(stmt),
            ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE => {
                self.rename_function(stmt.renameType, stmt.object.as_ref(), newname)
            }
            _ => self.rename_relation(stmt, newname),
        }
    }

    /// The lookup of the relation of a rename. With `IF EXISTS`, a relation that does not exist gives a notice and `None`.
    pub(super) fn rename_lookup(
        &mut self,
        rv: &RangeVar,
        missing_ok: bool,
    ) -> Result<Option<Found>> {
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

    /// `RangeVarCallbackForRenameAttribute` and `renameatt_check`: the relation of `RENAME COLUMN` and `RENAME CONSTRAINT`. A composite type and an index can have their columns renamed, and a typed table cannot.
    fn rename_target(&mut self, rv: &RangeVar, missing_ok: bool) -> Result<Option<Target>> {
        let Some(found) = self.rename_lookup(rv, missing_ok)? else { return Ok(None) };
        let name = relname(rv);
        if let Found::User(oid) = found {
            self.check_not_typed(oid, "cannot rename column of typed table")?;
        }
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
        if !self.is_superuser() && !self.owns(owner) {
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

    /// `renameatt`: the new name of a column of a table, a view, an index or a composite type. The column of a partitioned table gets the new name in each of its partitions first, at each level, and with `ONLY` a table with partitions cannot rename a column. The attribute of a composite type gets the new name in its typed tables first, which `CASCADE` allows.
    fn rename_column(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let Some(target) = self.rename_target(&rv, stmt.missing_ok)? else { return Ok(()) };
        let column = stmt.subname.as_deref().unwrap_or_default();
        if rv.inh {
            for (child, parents) in self.inheritors_with_parents(&target) {
                self.rename_one_column(&child, column, newname, parents)?;
            }
        } else if !self.catalog.children(target.oid).is_empty() {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("inherited column \"{column}\" must be renamed in child tables too"),
            ));
        }
        // The typed tables of a composite type get the new name of the attribute too, with their children.
        if target.kind == b'c' {
            for table in self.typed_tables(&target, stmt.behavior)? {
                for (child, parents) in self.inheritors_with_parents(&table) {
                    self.rename_one_column(&child, column, newname, parents)?;
                }
                self.rename_one_column(&table, column, newname, 0)?;
            }
        }
        self.rename_one_column(&target, column, newname, 0)
    }

    /// `find_all_inheritors` with `numparents`: the children of the table at each level, each with the number of its parents in the tree of the table.
    fn inheritors_with_parents(&self, target: &Target) -> Vec<(Target, i16)> {
        let children = self.all_inheritors(target.oid);
        let mut tree: Vec<u32> = children.iter().map(|c| c.oid).collect();
        tree.push(target.oid);
        children
            .into_iter()
            .map(|child| {
                let inherits = self.catalog.relation(child.oid).map(|r| r.inherits.clone());
                let found =
                    inherits.unwrap_or_default().iter().filter(|p| tree.contains(p)).count();
                (child, i16::try_from(found.max(1)).unwrap_or(i16::MAX))
            })
            .collect()
    }

    /// `renameatt_internal` for one relation. A column that the relation takes from more than `parents` parent tables cannot get a new name.
    fn rename_one_column(
        &mut self,
        target: &Target,
        column: &str,
        newname: &str,
        parents: i16,
    ) -> Result<()> {
        let rel = self.catalog.relation(target.oid);
        // A composite type has no system columns.
        let system = |name: &str| target.kind != b'c' && system_column(name).is_some();
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            if system(column) {
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
        if rel.and_then(|r| r.column(number)).is_some_and(|c| c.inhcount > parents) {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot rename inherited column \"{column}\""),
            ));
        }
        // `check_for_column_name_collision`.
        if system(newname) {
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
        self.rename_one_constraint(&target, name, newname, rv.inh, 0)
    }

    /// `rename_constraint_internal` for one table. A check constraint or a not-null constraint that the children inherit gets the new name in each of them first, and with `ONLY` a table with children cannot rename it. A constraint that the table takes from more than `parents` parent tables cannot get a new name.
    fn rename_one_constraint(
        &mut self,
        target: &Target,
        name: &str,
        newname: &str,
        recurse: bool,
        parents: i16,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some((oid, kind, index, no_inherit, inhcount)) =
            found.map(|c| (c.oid, c.kind, c.index, c.no_inherit, c.inhcount))
        else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" for table \"{}\" does not exist", target.name),
            ));
        };
        if matches!(kind, ConKind::Check | ConKind::NotNull) && !no_inherit {
            if recurse {
                for (child, parents) in self.inheritors_with_parents(target) {
                    self.rename_one_constraint(&child, name, newname, false, parents)?;
                }
            } else if parents == 0 && !self.catalog.children(target.oid).is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!("inherited constraint \"{name}\" must be renamed in child tables too"),
                ));
            }
            if inhcount > parents {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!("cannot rename inherited constraint \"{name}\""),
                ));
            }
        }
        if target.kind == b'r' && matches!(kind, ConKind::Primary | ConKind::Unique) && index != 0 {
            return self.catalog.rename_relation(index, newname);
        }
        self.catalog.rename_constraint(oid, newname)
    }

    /// `RenameSchema`. The views that read an object of the schema get a new text.
    fn rename_schema(&mut self, name: &str, newname: &str) -> Result<()> {
        let Some(schema) = self.catalog.schema_by_name(name) else {
            if self.builtin_schema(name).is_some() {
                return Err(not_yet("ALTER SCHEMA RENAME of a built-in schema", None));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_SCHEMA,
                format!("schema \"{name}\" does not exist"),
            ));
        };
        let (oid, owner) = (schema.oid, schema.owner);
        let exists = self.catalog.schema_by_name(newname).is_some()
            || self.builtin_schema(newname).is_some();
        if exists {
            return Err(Error::new(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{newname}\" already exists"),
            ));
        }
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner("schema", name));
        }
        if !self.database_create_allowed(self.user) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.an.env.database()),
            ));
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

    /// Adds to [`super::Defined::views`] the views whose query reads the relation `table`. A rule that is not the rule of a view keeps its text.
    pub(super) fn views_reading(&mut self, table: u32) {
        self.views_reading_where(table, false);
    }

    /// [`Self::views_reading`] for the views that read the table itself in `FROM`, where the text can give an alias to each column of the table. A view that reads only the columns of a function that gives the row type of the table keeps its text.
    pub(super) fn views_reading_table(&mut self, table: u32) {
        self.views_reading_where(table, true);
    }

    fn views_reading_where(&mut self, table: u32, in_from: bool) {
        let rules: Vec<u32> = self
            .catalog
            .depends()
            .iter()
            .filter(|d| d.object.class == PG_REWRITE)
            .filter(|d| d.referenced.class == PG_CLASS && d.referenced.oid == table)
            .map(|d| d.object.oid)
            .collect();
        for rule in rules {
            if let Some(view) = self.catalog.view_of_rule(rule).map(|v| v.oid)
                && view != table
                && !self.views.contains(&view)
                && (!in_from || self.view_reads_relation(view, table))
            {
                self.views.push(view);
            }
        }
    }

    /// Whether the query of the view has the relation `table` in a `FROM`, also in a subquery. A query that does not analyze counts as one that has it.
    fn view_reads_relation(&self, view: u32, table: u32) -> bool {
        let Ok(Some(query)) = self.an.stored_query(view) else { return true };
        let mut refs = Vec::new();
        super::view::query_references(self.catalog, &query, &[], &mut refs);
        refs.contains(&ObjRef::new(PG_CLASS, table))
    }

    /// Adds to [`super::Defined::views`] the views whose query reads an object that `uses` accepts. A rule that is not the rule of a view and reads such an object gives `0A000`, as rupg cannot write the text of a rule again yet.
    pub(super) fn views_using(&mut self, uses: impl Fn(ObjRef) -> bool) -> Result<()> {
        // The text of a policy names the objects that its expressions read.
        if self
            .catalog
            .depends()
            .iter()
            .any(|d| d.object.class == PG_POLICY && d.kind == DepKind::Normal && uses(d.referenced))
        {
            return Err(not_yet("ALTER ... RENAME of an object that a policy reads", None));
        }
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

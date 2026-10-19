//! `ALTER TABLE`, `ALTER VIEW`, `ALTER SEQUENCE`, `ALTER MATERIALIZED VIEW` and `ALTER FOREIGN TABLE` with `SET SCHEMA`, as `AlterTableNamespace` runs them. The relation moves with its row type, its indexes, the sequences that its columns own and its constraints. The views that read a moved relation get a new text.

use rupg_catalog::{PG_CLASS, PG_TYPE};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::AlterObjectSchemaStmt;

use super::{Definer, relname};

/// The OID of the schema `pg_toast`.
const PG_TOAST: u32 = 99;

impl Definer<'_, '_> {
    /// `AlterTableNamespace`: the lookup and the checks of `RangeVarCallbackForAlterRelation`, the check of an owned sequence, `RangeVarGetAndCheckCreationNamespace` and `CheckSetNamespace` for the new schema, and then the move.
    pub(super) fn alter_relation_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let Some(found) = self.rename_lookup(&rv, stmt.missing_ok)? else { return Ok(()) };
        let name = relname(&rv);
        let kind = self.kind_of(found);
        let cannot = |what: &str, hint: &str| {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot change schema of {what} \"{name}\""),
            )
            .with_hint(hint)
        };
        let target = self.alter_target(stmt.objectType, found, name)?;
        match kind {
            b'i' | b'I' => return Err(cannot("index", "Change the schema of the table instead.")),
            b't' => return Err(cannot("TOAST table", "Change the schema of the table instead.")),
            _ => {}
        }
        if let Some(table) = self.catalog.sequence_owner(target.oid).filter(|_| kind == b'S') {
            let table = self.catalog.relation(table).map_or("", |r| r.name.as_str());
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move an owned sequence into another schema",
            )
            .with_detail(format!("Sequence \"{name}\" is linked to table \"{table}\".")));
        }
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        let namespace = self.an.schema(schema).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        })?;
        self.check_create_in(namespace, None)?;
        if namespace == PG_TOAST || target.namespace == PG_TOAST {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        if namespace == target.namespace {
            return Ok(());
        }
        let moved = self.catalog.moved_relations(target.oid);
        for &oid in &moved {
            let Some(rel) = self.catalog.relation(oid) else { continue };
            if self.relation_in(namespace, &rel.name).is_some() {
                return Err(Error::new(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{}\" already exists in schema \"{schema}\"", rel.name),
                ));
            }
            self.check_type_names(rel.row_type, namespace, schema)?;
        }
        let types: Vec<u32> = moved
            .iter()
            .filter_map(|&oid| self.catalog.relation(oid))
            .flat_map(|r| {
                let array = self.catalog.type_by_oid(r.row_type).map_or(0, |t| t.array);
                [r.row_type, array]
            })
            .filter(|&t| t != 0)
            .collect();
        self.catalog.set_relation_namespace(target.oid, namespace)?;
        self.views_using(|object| match object.class {
            PG_CLASS => moved.contains(&object.oid),
            PG_TYPE => types.contains(&object.oid),
            _ => false,
        })
    }

    /// `AlterTypeNamespaceInternal`: the row type and its array type must not have the name of a type of the new schema.
    fn check_type_names(&self, row_type: u32, namespace: u32, schema: &str) -> Result<()> {
        let Some(ty) = self.catalog.type_by_oid(row_type) else { return Ok(()) };
        let array = self.catalog.type_by_oid(ty.array);
        for ty in std::iter::once(ty).chain(array) {
            if self.catalog.type_by_name(namespace, &ty.name).is_some() {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("type \"{}\" already exists in schema \"{schema}\"", ty.name),
                ));
            }
        }
        Ok(())
    }
}

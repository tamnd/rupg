//! `ALTER TYPE` and `ALTER DOMAIN` with `RENAME TO`, `SET SCHEMA` and `OWNER TO`, as `RenameType`, `AlterTypeNamespace` and `AlterTypeOwner` run them. A composite type changes with its relation. The views that name a changed type get a new text.

use rupg_catalog::{ObjRef, PG_CLASS, PG_TYPE, RangePart, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    AlterObjectSchemaStmt, AlterOwnerStmt, Node, ObjectType, RenameStmt, TypeName,
};

use super::comment::not_owner;
use super::{Definer, PG_TOAST_NAMESPACE, not_yet};
use crate::typename::type_name_text;

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in types.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// The facts of a type that the checks of `ALTER TYPE` use.
#[derive(Debug)]
struct Facts {
    oid: u32,
    name: String,
    namespace: u32,
    owner: u32,
    /// `typtype`.
    kind: u8,
    /// `typarray`, or 0.
    array: u32,
    /// The relation of a composite type of `CREATE TYPE ... AS`, or 0.
    composite: u32,
    /// True for the row type of a table, a view or another relation that is not a composite type.
    table_row: bool,
    /// The element type of a true array type, or 0.
    element: u32,
    /// True for a type of the catalog, and false for a built-in type.
    user: bool,
}

/// True for the kinds of objects of `ALTER TYPE` and `ALTER DOMAIN` that [`super::define`] runs with `RENAME TO`, `SET SCHEMA` and `OWNER TO`.
pub(super) fn is_type_kind(kind: ObjectType) -> bool {
    matches!(kind, ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN)
}

impl Definer<'_, '_> {
    /// `RenameType`: the checks, then `RenameRelationInternal` for a composite type or `RenameTypeInternal` for a type of another kind.
    pub(super) fn rename_type(&mut self, stmt: &RenameStmt) -> Result<()> {
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let facts = self.altered_type(stmt.object.as_ref())?;
        self.check_type_owner(&facts)?;
        self.check_domain(&facts, stmt.renameType)?;
        self.check_not_table_row(&facts)?;
        self.check_not_array(&facts)?;
        if !facts.user {
            return Err(not_yet("ALTER TYPE of a built-in type", None));
        }
        if facts.composite != 0 {
            self.catalog.rename_relation(facts.composite, newname)?;
        } else {
            self.catalog.rename_type(facts.oid, newname)?;
        }
        self.views_of_type(&facts)
    }

    /// `AlterTypeNamespace`: the checks of `AlterTypeNamespace_oid`, `CheckSetNamespace` and `AlterTypeNamespaceInternal`, then the move of the type, its array type, and the relation of a composite type or the constraints of a domain.
    pub(super) fn alter_type_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let facts = self.altered_type(stmt.object.as_ref())?;
        self.check_domain(&facts, stmt.objectType)?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        // `LookupCreationNamespace` gives the temporary schema for `pg_temp`, and rupg has no temporary schema.
        let temp = schema == "pg_temp";
        let namespace = if temp {
            0
        } else {
            let namespace = self.an.schema(schema).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                )
            })?;
            self.check_create_in(namespace, None)?;
            namespace
        };
        self.check_type_owner(&facts)?;
        self.check_not_array(&facts)?;
        if facts.namespace != namespace {
            if temp {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot move objects into or out of temporary schemas",
                ));
            }
            if namespace == PG_TOAST_NAMESPACE || facts.namespace == PG_TOAST_NAMESPACE {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot move objects into or out of TOAST schema",
                ));
            }
            self.check_type_name_in(namespace, &facts.name, schema)?;
        }
        self.check_not_table_row(&facts)?;
        if facts.namespace == namespace {
            return Ok(());
        }
        if !facts.user {
            return Err(not_yet("ALTER TYPE of a built-in type", None));
        }
        if facts.composite != 0 && self.relation_in(namespace, &facts.name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists in schema \"{schema}\"", facts.name),
            ));
        }
        if let Some(array) = self.catalog.type_by_oid(facts.array).map(|t| t.name.clone()) {
            self.check_type_name_in(namespace, &array, schema)?;
        }
        self.catalog.set_type_namespace(facts.oid, namespace)?;
        self.views_of_type(&facts)
    }

    /// `AlterTypeOwner`: the checks, then `ATExecChangeOwner` for the relation of a composite type or `AlterTypeOwnerInternal` for a type of another kind. A type that has the owner already needs no change.
    pub(super) fn alter_type_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let facts = self.altered_type(stmt.object.as_ref())?;
        self.check_domain(&facts, stmt.objectType)?;
        self.check_not_table_row(&facts)?;
        self.check_not_array(&facts)?;
        if facts.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.check_type_owner(&facts)?;
            self.check_can_set_role(new)?;
            if !self.may_create_in(facts.namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(facts.namespace)),
                ));
            }
        }
        if !facts.user {
            return Err(not_yet("ALTER TYPE of a built-in type", None));
        }
        if facts.composite != 0 {
            return self.change_owner(facts.composite, new, true);
        }
        self.catalog.set_type_owner(facts.oid, new)?;
        let roles = rupg_pgcatalog::roles::current();
        let (old, new_name) = (roles.name(facts.owner), roles.name(new));
        let object = ObjRef::new(PG_TYPE, facts.oid);
        self.catalog.set_object_owner(
            object,
            new,
            old.unwrap_or_default(),
            new_name.unwrap_or_default(),
        )
    }

    /// The type that the names of `ALTER TYPE` or `ALTER DOMAIN` give, as `typenameTypeId` and `LookupTypeName` find it.
    fn altered_type(&self, object: Option<&Node>) -> Result<Facts> {
        let names = match object {
            Some(Node::List(list)) => list.clone(),
            _ => Vec::new(),
        };
        let name = TypeName { names, ..TypeName::default() };
        let oid = self.an.lookup_type_name(&name)?.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("type \"{}\" does not exist", type_name_text(&name)),
            )
        })?;
        if let Some(ty) = self.catalog.type_by_oid(oid) {
            let composite = self
                .catalog
                .relation(ty.relation)
                .filter(|r| r.kind == RelKind::Composite)
                .map_or(0, |r| r.oid);
            let kind = match &ty.range {
                Some(RangePart::Range(_)) => b'r',
                Some(RangePart::Multirange(_)) => b'm',
                None if ty.domain.is_some() => b'd',
                None if ty.labels.is_some() => b'e',
                None if ty.relation != 0 => b'c',
                None => b'b',
            };
            let is_array = ty.element != 0
                && ty.relation == 0
                && ty.domain.is_none()
                && self.catalog.type_by_oid(ty.element).is_some_and(|e| e.array == oid);
            return Ok(Facts {
                oid,
                name: ty.name.clone(),
                namespace: ty.namespace,
                owner: ty.owner,
                kind,
                array: ty.array,
                composite,
                table_row: ty.relation != 0 && composite == 0,
                element: if is_array { ty.element } else { 0 },
                user: true,
            });
        }
        let row = builtin::type_by_oid(oid)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for type {oid}")))?;
        let is_array =
            row.elem != 0 && builtin::type_by_oid(row.elem).is_some_and(|e| e.array == oid);
        Ok(Facts {
            oid,
            name: row.name.to_string(),
            namespace: row.namespace,
            owner: BOOTSTRAP_SUPERUSER,
            kind: row.kind,
            array: row.array,
            composite: 0,
            table_row: row.kind == b'c',
            element: if is_array { row.elem } else { 0 },
            user: false,
        })
    }

    /// `object_ownercheck` of the type, with the error of `aclcheck_error_type`.
    fn check_type_owner(&self, facts: &Facts) -> Result<()> {
        if self.is_superuser() || facts.owner == self.user {
            return Ok(());
        }
        Err(not_owner("type", &self.type_text(facts.oid)))
    }

    /// `ALTER DOMAIN` needs a domain.
    fn check_domain(&self, facts: &Facts, objtype: ObjectType) -> Result<()> {
        if objtype != ObjectType::OBJECT_DOMAIN || facts.kind == b'd' {
            return Ok(());
        }
        Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("{} is not a domain", self.type_text(facts.oid)),
        ))
    }

    /// The row type of a table changes with `ALTER TABLE`.
    fn check_not_table_row(&self, facts: &Facts) -> Result<()> {
        if !facts.table_row {
            return Ok(());
        }
        Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("{} is a table's row type", self.type_text(facts.oid)),
        )
        .with_hint("Use ALTER TABLE instead."))
    }

    /// A true array type changes with its element type.
    fn check_not_array(&self, facts: &Facts) -> Result<()> {
        if facts.element == 0 {
            return Ok(());
        }
        Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot alter array type {}", self.type_text(facts.oid)),
        )
        .with_hint(format!(
            "You can alter type {}, which will alter the array type as well.",
            self.type_text(facts.element)
        )))
    }

    /// The check of `AlterTypeNamespaceInternal` for a duplicate name in the new schema.
    fn check_type_name_in(&self, namespace: u32, name: &str, schema: &str) -> Result<()> {
        let exists = self.catalog.type_by_name(namespace, name).is_some()
            || builtin::type_by_name(namespace, name).is_some();
        if !exists {
            return Ok(());
        }
        Err(Error::new(
            SqlState::DUPLICATE_OBJECT,
            format!("type \"{name}\" already exists in schema \"{schema}\""),
        ))
    }

    /// Adds the views that read the type, its array type or the relation of a composite type to [`super::Defined::views`], as their text names the type.
    fn views_of_type(&mut self, facts: &Facts) -> Result<()> {
        let (oid, array, relation) = (facts.oid, facts.array, facts.composite);
        self.views_using(|object| match object.class {
            PG_CLASS => relation != 0 && object.oid == relation,
            PG_TYPE => object.oid == oid || (array != 0 && object.oid == array),
            _ => false,
        })
    }
}

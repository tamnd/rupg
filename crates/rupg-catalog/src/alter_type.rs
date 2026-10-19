//! `ALTER TYPE` and `ALTER DOMAIN` with `RENAME TO`, `SET SCHEMA` and `OWNER TO`: the changes of `RenameTypeInternal`, `AlterTypeNamespaceInternal` and `AlterTypeOwnerInternal`. The analyzer runs the checks.

use rupg_common::Result;

use super::{Catalog, ObjRef, PG_TYPE, RangePart, RelKind, no_object};

impl Catalog {
    /// `RenameTypeInternal`: the new name of a type that is not a composite type. The array type gets the name that `makeArrayTypeName` gives it. When the name is the name of an array type that PostgreSQL made, that array type gets a new name first, as `moveArrayTypeName` gives it.
    ///
    /// # Errors
    ///
    /// `42710` when the schema has another type with the name, and an internal error for a type that the catalog does not have.
    pub fn rename_type(&mut self, oid: u32, name: &str) -> Result<()> {
        let ty = self.types.get(&oid).ok_or_else(|| no_object("type", oid))?;
        let (namespace, array) = (ty.namespace, ty.array);
        self.check_new_type(namespace, name)?;
        self.type_mut(oid)?.name = name.to_string();
        if array != 0 {
            let array_name = self.array_type_name(name, namespace);
            self.type_mut(array)?.name = array_name;
        }
        Ok(())
    }

    /// `AlterTypeNamespaceInternal`: moves the type and its array type to the schema. A composite type moves with its relation, and a domain moves with its constraints. `changeDependencyFor` changes the dependency of the type on the schema. The caller checks the names in the new schema.
    ///
    /// # Errors
    ///
    /// An internal error for a type that the catalog does not have.
    pub fn set_type_namespace(&mut self, oid: u32, namespace: u32) -> Result<()> {
        let ty = self.types.get(&oid).ok_or_else(|| no_object("type", oid))?;
        let (old, array, relation) = (ty.namespace, ty.array, ty.relation);
        let is_domain = ty.domain.is_some();
        if old == namespace {
            return Ok(());
        }
        if self.relation(relation).is_some_and(|r| r.kind == RelKind::Composite) {
            self.set_relation_namespace(relation, namespace)?;
        } else {
            for ty in [oid, array] {
                if let Some(ty) = self.types.get_mut(&ty) {
                    ty.namespace = namespace;
                }
            }
        }
        if is_domain {
            for con in self.constraints.values_mut().filter(|c| c.domain == oid) {
                con.namespace = namespace;
            }
        }
        self.change_namespace_dependency(ObjRef::new(PG_TYPE, oid), old, namespace);
        Ok(())
    }

    /// `AlterTypeOwnerInternal`: the new owner of a type that is not a composite type, of its array type, and of the multirange type of a range type with its array type.
    ///
    /// # Errors
    ///
    /// An internal error for a type that the catalog does not have.
    pub fn set_type_owner(&mut self, oid: u32, owner: u32) -> Result<()> {
        let ty = self.type_mut(oid)?;
        ty.owner = owner;
        let (array, range) = (ty.array, ty.range.clone());
        if let Some(RangePart::Range(info)) = range {
            self.set_type_owner(info.multirange, owner)?;
        }
        if array != 0 {
            self.set_type_owner(array, owner)?;
        }
        Ok(())
    }

    /// The type with the OID.
    fn type_mut(&mut self, oid: u32) -> Result<&mut super::Type> {
        self.types.get_mut(&oid).ok_or_else(|| no_object("type", oid))
    }
}

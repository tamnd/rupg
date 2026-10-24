//! The default privileges of `pg_default_acl`. They change as `SetDefaultACL` of `aclchk.c` changes them. An entry of a schema has an automatic dependency on the schema, so it goes with the schema.

use std::sync::Arc;

use super::{Catalog, DepKind, ObjRef, PG_NAMESPACE};

/// The OID of `pg_default_acl`.
pub const PG_DEFAULT_ACL: u32 = 826;

/// The default privileges of a role for the new objects of a kind, one row of `pg_default_acl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultAcl {
    /// The OID.
    pub oid: u32,
    /// `defaclrole`: the OID of the role that creates the objects.
    pub role: u32,
    /// `defaclnamespace`: the OID of the schema, or 0 for the objects of all the schemas.
    pub namespace: u32,
    /// `defaclobjtype`: `r` for relations, `S` for sequences, `f` for functions, `T` for types, `n` for schemas and `L` for large objects.
    pub objtype: u8,
    /// `defaclacl`: the ACL, as `aclitem` texts.
    pub acl: Vec<String>,
}

impl Catalog {
    /// The default privileges, in the order of their OIDs.
    pub fn default_acls(&self) -> impl Iterator<Item = &DefaultAcl> {
        self.default_acls.values()
    }

    /// The default privileges with the OID.
    pub fn default_acl(&self, oid: u32) -> Option<&DefaultAcl> {
        self.default_acls.get(&oid)
    }

    /// `get_default_acl_internal`: the entry of the role, the schema and the kind of objects, or `None`.
    pub fn find_default_acl(&self, role: u32, namespace: u32, objtype: u8) -> Option<&DefaultAcl> {
        self.default_acls
            .values()
            .find(|d| d.role == role && d.namespace == namespace && d.objtype == objtype)
    }

    /// `SetDefaultACL`: sets the ACL of the entry of the role, the schema and the kind of objects. A new entry gets the next OID and, for a schema, an automatic dependency on the schema. `None` removes the entry.
    pub fn set_default_acl(
        &mut self,
        role: u32,
        namespace: u32,
        objtype: u8,
        acl: Option<Vec<String>>,
    ) {
        let found = self.find_default_acl(role, namespace, objtype).map(|d| d.oid);
        match (found, acl) {
            (Some(oid), Some(acl)) => {
                if let Some(entry) = Arc::make_mut(&mut self.default_acls).get_mut(&oid) {
                    entry.acl = acl;
                }
            }
            (Some(oid), None) => {
                Arc::make_mut(&mut self.default_acls).remove(&oid);
                let me = ObjRef::new(PG_DEFAULT_ACL, oid);
                self.depends.retain(|d| d.object != me);
            }
            (None, Some(acl)) => {
                let oid = self.new_oid();
                let entry = DefaultAcl { oid, role, namespace, objtype, acl };
                Arc::make_mut(&mut self.default_acls).insert(oid, entry);
                if namespace != 0 {
                    let me = ObjRef::new(PG_DEFAULT_ACL, oid);
                    self.depend(me, ObjRef::new(PG_NAMESPACE, namespace), DepKind::Auto);
                }
            }
            (None, None) => {}
        }
    }

    /// `doDeletion` of an entry.
    pub(super) fn remove_default_acl(&mut self, oid: u32) {
        if self.default_acls.contains_key(&oid) {
            Arc::make_mut(&mut self.default_acls).remove(&oid);
        }
    }
}

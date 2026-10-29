//! The access methods of `pg_am` that `CREATE ACCESS METHOD` makes, as `CreateAccessMethod` of `amcmds.c` stores them, and the access method of each table in `relam`.

use super::{Catalog, DepKind, FIRST_NORMAL_OID, ObjRef, PG_AM, PG_CLASS, PG_PROC, RelKind};

/// The OID of the access method `heap`, the default access method of a table.
pub const HEAP_AM: u32 = 2;

/// An access method that a statement made, one row of `pg_am`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessMethod {
    /// The OID. [`Catalog::create_access_method`] sets it.
    pub oid: u32,
    /// `amname`: the name.
    pub name: String,
    /// `amhandler`: the OID of the handler function.
    pub handler: u32,
    /// `amtype`: `t` for a table access method and `i` for an index access method.
    pub kind: u8,
}

impl Catalog {
    /// `CreateAccessMethod`: a new access method, which gets the next OID. It depends on its handler. The analyzer checks the name and the handler first. The return value is the OID.
    pub fn create_access_method(&mut self, mut new: AccessMethod) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        self.depend(ObjRef::new(PG_AM, oid), ObjRef::new(PG_PROC, new.handler), DepKind::Normal);
        self.access_methods.insert(oid, new);
        oid
    }

    /// The access methods that statements made, in the order of their OIDs.
    pub fn access_methods(&self) -> impl Iterator<Item = &AccessMethod> {
        self.access_methods.values()
    }

    /// The access method with the OID, when a statement made it.
    pub fn access_method(&self, oid: u32) -> Option<&AccessMethod> {
        self.access_methods.get(&oid)
    }

    /// The access method with the name, when a statement made it.
    pub fn access_method_by_name(&self, name: &str) -> Option<&AccessMethod> {
        self.access_methods.values().find(|m| m.name == name)
    }

    /// `ATExecSetAccessMethodNoStorage` and the rewrite of `SET ACCESS METHOD`: the relation gets `relam` `method`, and its TOAST table gets it too. The relation depends on an access method that a statement made, and on no other access method, as `changeDependencyFor` keeps it.
    pub fn set_access_method(&mut self, oid: u32, method: u32) {
        let Some(rel) = self.relations.get_mut(&oid) else { return };
        rel.access_method = method;
        let toast =
            if matches!(rel.kind, RelKind::Table | RelKind::MatView) { rel.toast } else { 0 };
        if let Some(toast) = self.relations.get_mut(&toast) {
            toast.access_method = method;
        }
        let me = ObjRef::new(PG_CLASS, oid);
        self.depends.retain(|d| !(d.object == me && d.referenced.class == PG_AM));
        if method >= FIRST_NORMAL_OID {
            self.depend(me, ObjRef::new(PG_AM, method), DepKind::Normal);
        }
    }
}

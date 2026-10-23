//! The operator families of `pg_opfamily`, the operator classes of `pg_opclass` and their members in `pg_amop` and `pg_amproc`, as `CreateOpFamily`, `DefineOpClass`, `storeOperators` and `storeProcedures` of `opclasscmds.c` make them, with the same rows of `pg_depend`.

use super::{
    Catalog, DepKind, ObjRef, PG_NAMESPACE, PG_OPCLASS, PG_OPERATOR, PG_PROC, PG_TYPE, is_pinned,
};

/// The OID of `pg_am`.
pub const PG_AM: u32 = 2601;
/// The OID of `pg_amop`.
pub const PG_AMOP: u32 = 2602;
/// The OID of `pg_amproc`.
pub const PG_AMPROC: u32 = 2603;
/// The OID of `pg_opfamily`.
pub const PG_OPFAMILY: u32 = 2753;

/// An operator family, one row of `pg_opfamily`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpFamily {
    /// The OID. [`Catalog::create_opfamily`] sets it.
    pub oid: u32,
    /// `opfmethod`: the OID of the access method.
    pub method: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
}

/// An operator class, one row of `pg_opclass`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpClass {
    /// The OID. [`Catalog::create_opclass`] sets it.
    pub oid: u32,
    /// `opcmethod`: the OID of the access method.
    pub method: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// `opcfamily`: the OID of the operator family.
    pub family: u32,
    /// `opcintype`: the type that the class indexes.
    pub intype: u32,
    /// `opcdefault`: true for the default class of the type and the access method.
    pub default: bool,
    /// `opckeytype`: the type of the data in the index, or 0 for the type that the class indexes.
    pub keytype: u32,
}

/// An operator of an operator family, one row of `pg_amop`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmOp {
    /// The OID. [`Catalog::add_amop`] sets it.
    pub oid: u32,
    /// `amopfamily`: the OID of the operator family.
    pub family: u32,
    /// `amoplefttype`.
    pub left: u32,
    /// `amoprighttype`.
    pub right: u32,
    /// `amopstrategy`: the number of the operator in the family.
    pub strategy: i16,
    /// `amoppurpose`: `s` for search or `o` for ordering.
    pub purpose: u8,
    /// `amopopr`: the OID of the operator.
    pub operator: u32,
    /// `amopmethod`: the OID of the access method.
    pub method: u32,
    /// `amopsortfamily`: the B-tree family of the order of an ordering operator, or 0.
    pub sortfamily: u32,
}

/// A support function of an operator family, one row of `pg_amproc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmProc {
    /// The OID. [`Catalog::add_amproc`] sets it.
    pub oid: u32,
    /// `amprocfamily`: the OID of the operator family.
    pub family: u32,
    /// `amproclefttype`.
    pub left: u32,
    /// `amprocrighttype`.
    pub right: u32,
    /// `amprocnum`: the number of the function in the family.
    pub number: i16,
    /// `amproc`: the OID of the function.
    pub function: u32,
}

/// The dependency facts of a member of a family, the fields `ref_is_hard`, `ref_is_family` and `refobjid` of `OpFamilyMember`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberLink {
    /// `ref_is_hard`: the member is a part of `owner`, and its operator or function needs `CASCADE` to go.
    pub hard: bool,
    /// The operator class or the operator family that the member depends on.
    pub owner: ObjRef,
    /// The input types of the operator or the function. A type of the member that is one of them needs no dependency, as `typeDepNeeded` finds.
    pub inputs: Vec<u32>,
}

impl MemberLink {
    /// The kind of the dependencies on the operator or the function and on the types: `Normal` for a hard member, else `Auto`.
    fn kind(&self) -> DepKind {
        if self.hard { DepKind::Normal } else { DepKind::Auto }
    }

    /// `typeDepNeeded`.
    fn needs_type(&self, ty: u32) -> bool {
        !is_pinned(ObjRef::new(PG_TYPE, ty)) && !self.inputs.contains(&ty)
    }
}

impl Catalog {
    /// `CreateOpFamily`: a new operator family, which gets the next OID. It depends on its access method and its schema.
    pub fn create_opfamily(&mut self, mut new: OpFamily) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_OPFAMILY, oid);
        self.depend(me, ObjRef::new(PG_AM, new.method), DepKind::Auto);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        self.opfamilies.insert(oid, new);
        oid
    }

    /// The row of `pg_opclass` of `DefineOpClass`, which gets the next OID. The caller adds the members and then runs [`Catalog::opclass_depends`], in the order of `DefineOpClass`.
    pub fn create_opclass(&mut self, mut new: OpClass) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        self.opclasses.insert(oid, new);
        oid
    }

    /// The dependencies of `DefineOpClass` for the class: on its schema, its family, its type and its storage type.
    pub fn opclass_depends(&mut self, oid: u32) {
        let Some(class) = self.opclasses.get(&oid) else { return };
        let me = ObjRef::new(PG_OPCLASS, oid);
        let mut refs = vec![
            (ObjRef::new(PG_NAMESPACE, class.namespace), DepKind::Normal),
            (ObjRef::new(PG_OPFAMILY, class.family), DepKind::Auto),
            (ObjRef::new(PG_TYPE, class.intype), DepKind::Normal),
        ];
        if class.keytype != 0 {
            refs.push((ObjRef::new(PG_TYPE, class.keytype), DepKind::Normal));
        }
        for (referenced, kind) in refs {
            self.depend(me, referenced, kind);
        }
    }

    /// The part of `storeOperators` for one operator: the row of `pg_amop`, which gets the next OID, and its dependencies.
    pub fn add_amop(&mut self, mut new: AmOp, link: &MemberLink) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_AMOP, oid);
        self.depend(me, ObjRef::new(PG_OPERATOR, new.operator), link.kind());
        self.member_depends(me, link, new.left, new.right);
        if new.sortfamily != 0 {
            self.depend(me, ObjRef::new(PG_OPFAMILY, new.sortfamily), link.kind());
        }
        self.amops.insert(oid, new);
        oid
    }

    /// The part of `storeProcedures` for one function: the row of `pg_amproc`, which gets the next OID, and its dependencies.
    pub fn add_amproc(&mut self, mut new: AmProc, link: &MemberLink) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_AMPROC, oid);
        self.depend(me, ObjRef::new(PG_PROC, new.function), link.kind());
        self.member_depends(me, link, new.left, new.right);
        self.amprocs.insert(oid, new);
        oid
    }

    /// The dependencies of a member on its class or family and on its types.
    fn member_depends(&mut self, me: ObjRef, link: &MemberLink, left: u32, right: u32) {
        let owner = if link.hard { DepKind::Internal } else { DepKind::Auto };
        self.depend(me, link.owner, owner);
        if link.needs_type(left) {
            self.depend(me, ObjRef::new(PG_TYPE, left), link.kind());
        }
        if left != right && link.needs_type(right) {
            self.depend(me, ObjRef::new(PG_TYPE, right), link.kind());
        }
    }

    /// Changes the row of the operator family.
    pub fn alter_opfamily(&mut self, oid: u32, change: impl FnOnce(&mut OpFamily)) {
        if let Some(family) = self.opfamilies.get_mut(&oid) {
            change(family);
        }
    }

    /// Changes the row of the operator class.
    pub fn alter_opclass(&mut self, oid: u32, change: impl FnOnce(&mut OpClass)) {
        if let Some(class) = self.opclasses.get_mut(&oid) {
            change(class);
        }
    }

    /// `AlterObjectNamespace_internal` for an operator family, or for an operator class when `family` is false: the move to the schema, with `changeDependencyFor` for the dependency on the schema.
    pub fn set_opcf_namespace(&mut self, family: bool, oid: u32, namespace: u32) {
        let (class, old) = if family {
            let row = self.opfamilies.get_mut(&oid);
            (PG_OPFAMILY, row.map(|f| std::mem::replace(&mut f.namespace, namespace)))
        } else {
            let row = self.opclasses.get_mut(&oid);
            (PG_OPCLASS, row.map(|c| std::mem::replace(&mut c.namespace, namespace)))
        };
        if let Some(old) = old {
            self.change_namespace_dependency(ObjRef::new(class, oid), old, namespace);
        }
    }

    /// The operator families, in the order of their OIDs.
    pub fn opfamilies(&self) -> impl Iterator<Item = &OpFamily> {
        self.opfamilies.values()
    }

    /// The operator family with the OID.
    pub fn opfamily(&self, oid: u32) -> Option<&OpFamily> {
        self.opfamilies.get(&oid)
    }

    /// The operator classes, in the order of their OIDs.
    pub fn opclasses(&self) -> impl Iterator<Item = &OpClass> {
        self.opclasses.values()
    }

    /// The operator class with the OID.
    pub fn opclass(&self, oid: u32) -> Option<&OpClass> {
        self.opclasses.get(&oid)
    }

    /// The rows of `pg_amop`, in the order of their OIDs.
    pub fn amops(&self) -> impl Iterator<Item = &AmOp> {
        self.amops.values()
    }

    /// The row of `pg_amop` with the OID.
    pub fn amop(&self, oid: u32) -> Option<&AmOp> {
        self.amops.get(&oid)
    }

    /// The rows of `pg_amproc`, in the order of their OIDs.
    pub fn amprocs(&self) -> impl Iterator<Item = &AmProc> {
        self.amprocs.values()
    }

    /// The row of `pg_amproc` with the OID.
    pub fn amproc(&self, oid: u32) -> Option<&AmProc> {
        self.amprocs.get(&oid)
    }
}

//! The encoding conversions of `pg_conversion` that `CREATE CONVERSION` makes. They change as `ConversionCreate` of `pg_conversion.c` and `AlterObjectRename_internal`, `AlterObjectOwner_internal` and `AlterObjectNamespace_internal` of `alter.c` change them, with the same rows of `pg_depend`.

use super::{Catalog, DepKind, ObjRef, PG_NAMESPACE, PG_PROC};

/// The OID of `pg_conversion`.
pub const PG_CONVERSION: u32 = 2607;

/// An encoding conversion, one row of `pg_conversion`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversion {
    /// The OID. [`Catalog::create_conversion`] sets it.
    pub oid: u32,
    /// `conname`: the name.
    pub name: String,
    /// `connamespace`: the OID of the schema.
    pub namespace: u32,
    /// `conowner`: the OID of the owner role.
    pub owner: u32,
    /// `conforencoding`: the number of the source encoding.
    pub from: i32,
    /// `contoencoding`: the number of the destination encoding.
    pub to: i32,
    /// `conproc`: the OID of the conversion function.
    pub proc: u32,
    /// `condefault`: true for the default conversion of the two encodings in the schema.
    pub default: bool,
}

impl Catalog {
    /// `ConversionCreate`: a new conversion, which gets the next OID. It depends on its function and on its schema. The analyzer checks the name and the default conversion first. The return value is the OID of the conversion.
    pub fn create_conversion(&mut self, mut new: Conversion) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_CONVERSION, oid);
        self.depend(me, ObjRef::new(PG_PROC, new.proc), DepKind::Normal);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        self.conversions.insert(oid, new);
        oid
    }

    /// The conversions, in the order of their OIDs.
    pub fn conversions(&self) -> impl Iterator<Item = &Conversion> {
        self.conversions.values()
    }

    /// The conversion with the OID.
    pub fn conversion(&self, oid: u32) -> Option<&Conversion> {
        self.conversions.get(&oid)
    }

    /// The conversion with the name in the schema.
    pub fn conversion_by_name(&self, namespace: u32, name: &str) -> Option<&Conversion> {
        self.conversions.values().find(|c| c.namespace == namespace && c.name == name)
    }

    /// Changes the row of the conversion.
    pub fn alter_conversion(&mut self, oid: u32, change: impl FnOnce(&mut Conversion)) {
        if let Some(conversion) = self.conversions.get_mut(&oid) {
            change(conversion);
        }
    }

    /// `AlterObjectNamespace_internal` for a conversion: the move to the schema, with `changeDependencyFor` for the dependency on the schema.
    pub fn set_conversion_namespace(&mut self, oid: u32, namespace: u32) {
        let old =
            self.conversions.get_mut(&oid).map(|c| std::mem::replace(&mut c.namespace, namespace));
        if let Some(old) = old {
            self.change_namespace_dependency(ObjRef::new(PG_CONVERSION, oid), old, namespace);
        }
    }
}

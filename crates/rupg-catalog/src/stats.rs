//! The extended statistics objects of `pg_statistic_ext`. They change as `CreateStatistics` and `AlterStatistics` of `statscmds.c` change them, with the same rows of `pg_depend`.

use rupg_common::{Error, Object, Result, SqlState};

use super::{Catalog, DepKind, ObjRef, PG_CLASS, PG_NAMESPACE};

/// The OID of `pg_statistic_ext`.
pub const PG_STATISTIC_EXT: u32 = 3381;

/// An extended statistics object, one row of `pg_statistic_ext`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatExt {
    /// The OID. [`Catalog::create_stat_ext`] sets it.
    pub oid: u32,
    /// `stxname`: the name.
    pub name: String,
    /// `stxnamespace`: the OID of the schema.
    pub namespace: u32,
    /// `stxowner`: the OID of the owner role.
    pub owner: u32,
    /// `stxrelid`: the OID of the table.
    pub relation: u32,
    /// `stxkeys`: the numbers of the plain columns, in order.
    pub keys: Vec<i16>,
    /// `stxkind`: the kinds, `d` for ndistinct, `f` for dependencies, `m` for MCV and `e` for the expressions.
    pub kinds: Vec<u8>,
    /// `stxstattarget`: the statistics target, or `None` for the default.
    pub target: Option<i16>,
    /// `stxexprs`: the expressions in the form of `nodeToString` of a list, or `None` when there is no expression.
    pub exprs: Option<String>,
}

impl Catalog {
    /// `CreateStatistics`: a new statistics object, which gets the next OID. It depends on each key column, or on the table when there is no key column, on the objects `refs` that the expressions refer to, and on its schema.
    ///
    /// # Errors
    ///
    /// `23505` when the schema has a statistics object with the name.
    pub fn create_stat_ext(&mut self, mut new: StatExt, refs: &[ObjRef]) -> Result<u32> {
        let oid = self.new_oid();
        if self.stat_ext_by_name(new.namespace, &new.name).is_some() {
            return Err(Error::new(
                SqlState::UNIQUE_VIOLATION,
                "duplicate key value violates unique constraint \"pg_statistic_ext_name_index\"",
            )
            .with_detail(format!(
                "Key (stxname, stxnamespace)=({}, {}) already exists.",
                new.name, new.namespace
            ))
            .with_object(Object::Schema, "pg_catalog")
            .with_object(Object::Table, "pg_statistic_ext")
            .with_object(Object::Constraint, "pg_statistic_ext_name_index"));
        }
        new.oid = oid;
        let me = ObjRef::new(PG_STATISTIC_EXT, oid);
        for &number in &new.keys {
            self.depend(me, ObjRef::column(new.relation, number), DepKind::Auto);
        }
        if new.keys.is_empty() {
            self.depend(me, ObjRef::new(PG_CLASS, new.relation), DepKind::Auto);
        }
        self.depend_on_expr(me, refs, new.relation, DepKind::Auto);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        self.stat_exts.insert(oid, new);
        Ok(oid)
    }

    /// The statistics objects, in the order of their OIDs.
    pub fn stat_exts(&self) -> impl Iterator<Item = &StatExt> {
        self.stat_exts.values()
    }

    /// The statistics object with the OID.
    pub fn stat_ext(&self, oid: u32) -> Option<&StatExt> {
        self.stat_exts.get(&oid)
    }

    /// The statistics object with the name in the schema.
    pub fn stat_ext_by_name(&self, namespace: u32, name: &str) -> Option<&StatExt> {
        self.stat_exts.values().find(|s| s.namespace == namespace && s.name == name)
    }

    /// Changes the row of the statistics object.
    pub fn alter_stat_ext(&mut self, oid: u32, change: impl FnOnce(&mut StatExt)) {
        if let Some(stat) = self.stat_exts.get_mut(&oid) {
            change(stat);
        }
    }

    /// `AlterObjectNamespace_internal` for a statistics object: the move to the schema, with `changeDependencyFor` for the dependency on the schema.
    pub fn set_stat_ext_namespace(&mut self, oid: u32, namespace: u32) {
        let old =
            self.stat_exts.get_mut(&oid).map(|s| std::mem::replace(&mut s.namespace, namespace));
        if let Some(old) = old {
            self.change_namespace_dependency(ObjRef::new(PG_STATISTIC_EXT, oid), old, namespace);
        }
    }
}

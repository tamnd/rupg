//! The publications of `pg_publication`, the tables of `pg_publication_rel` and the schemas of `pg_publication_namespace`. They change as `CreatePublication`, `AlterPublication`, `publication_add_relation` and `publication_add_schema` of `publicationcmds.c` and `pg_publication.c` change them, with the same rows of `pg_depend`.

use super::{
    Catalog, DepKind, FIRST_NORMAL_OID, ObjRef, PG_CLASS, PG_NAMESPACE, RelKind, Relation,
};

/// The OID of `pg_publication`.
pub const PG_PUBLICATION: u32 = 6104;
/// The OID of `pg_publication_rel`.
pub const PG_PUBLICATION_REL: u32 = 6106;
/// The OID of `pg_publication_namespace`.
pub const PG_PUBLICATION_NAMESPACE: u32 = 6237;

/// A publication, one row of `pg_publication`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    /// The OID. [`Catalog::create_publication`] sets it.
    pub oid: u32,
    /// `pubname`: the name.
    pub name: String,
    /// `pubowner`: the OID of the owner role.
    pub owner: u32,
    /// `puballtables`: true for `FOR ALL TABLES`.
    pub all_tables: bool,
    /// `puballsequences`: true for `FOR ALL SEQUENCES`.
    pub all_sequences: bool,
    /// `pubinsert`.
    pub insert: bool,
    /// `pubupdate`.
    pub update: bool,
    /// `pubdelete`.
    pub delete: bool,
    /// `pubtruncate`.
    pub truncate: bool,
    /// `pubviaroot`: the value of `publish_via_partition_root`.
    pub via_root: bool,
    /// `pubgencols`: `n` for no generated columns, `s` for the stored generated columns.
    pub gencols: u8,
}

/// A table of a publication, one row of `pg_publication_rel`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationRel {
    /// The OID. [`Catalog::add_publication_rel`] sets it.
    pub oid: u32,
    /// `prpubid`: the OID of the publication.
    pub publication: u32,
    /// `prrelid`: the OID of the table.
    pub relation: u32,
    /// `prexcept`: true for a table of the `EXCEPT` clause.
    pub except: bool,
    /// `prqual`: the stored form of the row filter, or `None`.
    pub qual: Option<String>,
    /// `prattrs`: the numbers of the columns of the column list in order, or `None` for no column list.
    pub attrs: Option<Vec<i16>>,
}

/// A schema of a publication, one row of `pg_publication_namespace`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationNamespace {
    /// The OID.
    pub oid: u32,
    /// `pnpubid`: the OID of the publication.
    pub publication: u32,
    /// `pnnspid`: the OID of the schema.
    pub schema: u32,
}

impl Catalog {
    /// `CreatePublication`: a new publication, which gets the next OID. The analyzer checks the name first. The return value is the OID of the publication.
    pub fn create_publication(&mut self, mut new: Publication) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        self.publications.insert(oid, new);
        oid
    }

    /// The publications, in the order of their OIDs.
    pub fn publications(&self) -> impl Iterator<Item = &Publication> {
        self.publications.values()
    }

    /// The publication with the OID.
    pub fn publication(&self, oid: u32) -> Option<&Publication> {
        self.publications.get(&oid)
    }

    /// `GetPublicationByName`: the publication with the name.
    pub fn publication_by_name(&self, name: &str) -> Option<&Publication> {
        self.publications.values().find(|p| p.name == name)
    }

    /// Changes the row of the publication.
    pub fn alter_publication(&mut self, oid: u32, change: impl FnOnce(&mut Publication)) {
        if let Some(publication) = self.publications.get_mut(&oid) {
            change(publication);
        }
    }

    /// `publication_add_relation`: a new row of `pg_publication_rel`, which gets the next OID. It has automatic dependencies on the publication and on the table, a normal dependency on each column of the column list, and the dependencies of `recordDependencyOnSingleRelExpr` on the objects that the row filter refers to. The return value is the OID of the row.
    pub fn add_publication_rel(&mut self, mut new: PublicationRel, refs: &[ObjRef]) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_PUBLICATION_REL, oid);
        self.depend(me, ObjRef::new(PG_PUBLICATION, new.publication), DepKind::Auto);
        self.depend(me, ObjRef::new(PG_CLASS, new.relation), DepKind::Auto);
        self.depend_on_expr(me, refs, new.relation, DepKind::Normal);
        let mut columns = new.attrs.clone().unwrap_or_default();
        columns.sort_unstable();
        for column in columns {
            self.depend(me, ObjRef::column(new.relation, column), DepKind::Normal);
        }
        self.publication_rels.insert(oid, new);
        oid
    }

    /// The rows of `pg_publication_rel`, in the order of their OIDs.
    pub fn publication_rels(&self) -> impl Iterator<Item = &PublicationRel> {
        self.publication_rels.values()
    }

    /// The row of `pg_publication_rel` with the OID.
    pub fn publication_rel(&self, oid: u32) -> Option<&PublicationRel> {
        self.publication_rels.get(&oid)
    }

    /// The row of `pg_publication_rel` of the table in the publication, as the syscache `PUBLICATIONRELMAP` finds it.
    pub fn publication_rel_of(&self, publication: u32, relation: u32) -> Option<&PublicationRel> {
        self.publication_rels
            .values()
            .find(|r| r.publication == publication && r.relation == relation)
    }

    /// `publication_add_schema`: a new row of `pg_publication_namespace`, which gets the next OID. It has automatic dependencies on the publication and on the schema. The return value is the OID of the row.
    pub fn add_publication_namespace(&mut self, publication: u32, schema: u32) -> u32 {
        let oid = self.new_oid();
        let me = ObjRef::new(PG_PUBLICATION_NAMESPACE, oid);
        self.depend(me, ObjRef::new(PG_PUBLICATION, publication), DepKind::Auto);
        self.depend(me, ObjRef::new(PG_NAMESPACE, schema), DepKind::Auto);
        self.publication_namespaces.insert(oid, PublicationNamespace { oid, publication, schema });
        oid
    }

    /// The rows of `pg_publication_namespace`, in the order of their OIDs.
    pub fn publication_namespaces(&self) -> impl Iterator<Item = &PublicationNamespace> {
        self.publication_namespaces.values()
    }

    /// The row of `pg_publication_namespace` with the OID.
    pub fn publication_namespace(&self, oid: u32) -> Option<&PublicationNamespace> {
        self.publication_namespaces.get(&oid)
    }

    /// `is_publishable_class`: a permanent table, partitioned table or sequence with an OID from `FirstNormalObjectId`, which leaves out the relations of `information_schema`.
    pub fn is_publishable(&self, rel: &Relation) -> bool {
        matches!(rel.kind, RelKind::Table | RelKind::Partitioned | RelKind::Sequence)
            && rel.oid >= FIRST_NORMAL_OID
            && !rel.unlogged
            && !self.is_temp_schema(rel.namespace)
    }

    /// `is_publishable_table`: the relation `relid` is a table that a publication can have.
    pub fn is_publishable_table(&self, relid: u32) -> bool {
        self.relation(relid).is_some_and(|r| r.kind != RelKind::Sequence && self.is_publishable(r))
    }

    /// `GetPubPartitionOptionRelations`: the table, or the leaf partitions of a partitioned table when `leaf` is true.
    fn partition_option(&self, out: &mut Vec<u32>, relation: u32, leaf: bool) {
        let partitioned = self.relation(relation).is_some_and(|r| r.kind == RelKind::Partitioned);
        if !(partitioned && leaf) {
            out.push(relation);
            return;
        }
        for part in self.all_inheritors(relation) {
            if self.relation(part).is_some_and(|r| r.kind != RelKind::Partitioned) {
                out.push(part);
            }
        }
    }

    /// `get_publication_relations`: the tables of the rows of the publication with the `EXCEPT` flag `except`, in the order of their OIDs.
    fn publication_relations(&self, publication: u32, leaf: bool, except: bool) -> Vec<u32> {
        let mut out = Vec::new();
        for row in self.publication_rels.values() {
            if row.publication == publication && row.except == except {
                self.partition_option(&mut out, row.relation, leaf);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// `GetAllPublicationRelations`: the publishable relations of the kind, without the tables of the `EXCEPT` clause of the publication. With `via_root`, a partition gives its place to its root table.
    pub fn all_publication_relations(
        &self,
        publication: u32,
        kind: RelKind,
        via_root: bool,
    ) -> Vec<u32> {
        let except = if kind == RelKind::Table {
            self.publication_relations(publication, !via_root, true)
        } else {
            Vec::new()
        };
        let take = |r: &&Relation, kind: RelKind, partition_ok: bool| {
            r.kind == kind
                && self.is_publishable(r)
                && (partition_ok || r.partition_of.is_none())
                && !except.contains(&r.oid)
        };
        let mut out: Vec<u32> =
            self.relations.values().filter(|r| take(r, kind, !via_root)).map(|r| r.oid).collect();
        if via_root {
            out.extend(
                self.relations
                    .values()
                    .filter(|r| take(r, RelKind::Partitioned, false))
                    .map(|r| r.oid),
            );
        }
        out
    }

    /// `GetAllSchemaPublicationRelations`: the tables of the schemas of the publication, with the leaf partitions of each partitioned table in place of the table when `leaf` is true.
    fn schema_publication_relations(&self, publication: u32, leaf: bool) -> Vec<u32> {
        let mut out = Vec::new();
        for row in self.publication_namespaces.values().filter(|n| n.publication == publication) {
            let mut found: Vec<u32> = Vec::new();
            for rel in self.relations.values() {
                if rel.namespace != row.schema || !self.is_publishable(rel) {
                    continue;
                }
                match rel.kind {
                    RelKind::Table => found.push(rel.oid),
                    RelKind::Partitioned => {
                        let mut parts = Vec::new();
                        self.partition_option(&mut parts, rel.oid, leaf);
                        for part in parts {
                            if !found.contains(&part) {
                                found.push(part);
                            }
                        }
                    }
                    _ => {}
                }
            }
            out.extend(found);
        }
        out
    }

    /// The partitions above a table, from its parent up, as `get_partition_ancestors`.
    pub fn partition_ancestors(&self, relation: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = relation;
        while let Some(parent) =
            self.relation(at).and_then(|r| r.partition_of.as_ref()).map(|p| p.parent)
        {
            out.push(parent);
            at = parent;
        }
        out
    }

    /// True when the publication has the table, not in its `EXCEPT` clause, or the schema of the table.
    fn publishes(&self, publication: u32, relation: u32) -> bool {
        let namespace = self.relation(relation).map_or(0, |r| r.namespace);
        self.publication_rels
            .values()
            .any(|r| r.publication == publication && r.relation == relation && !r.except)
            || self
                .publication_namespaces
                .values()
                .any(|n| n.publication == publication && n.schema == namespace)
    }

    /// `is_table_publishable_in_publication`.
    fn publishable_in(&self, relation: u32, publication: &Publication) -> bool {
        let Some(rel) = self.relation(relation) else { return false };
        if !publication.via_root && rel.kind == RelKind::Partitioned {
            return false;
        }
        let ancestors = self.partition_ancestors(relation);
        if publication.all_tables {
            if publication.via_root && rel.partition_of.is_some() {
                return false;
            }
            let top = ancestors.last().copied().unwrap_or(relation);
            return self.publication_rel_of(publication.oid, top).is_none();
        }
        if ancestors.iter().any(|&a| self.publishes(publication.oid, a)) {
            return !publication.via_root;
        }
        self.publishes(publication.oid, relation)
    }

    /// `pg_get_publication_tables`: the pairs of the publication and the table that the publications give, in order, or only the pairs of the table `target`. A name that is not a publication is an error when `target` is `None` and is skipped when it is `Some`. The error gives the name.
    pub fn publication_tables(
        &self,
        names: &[&str],
        target: Option<u32>,
    ) -> Result<Vec<(u32, u32)>, String> {
        if let Some(target) = target
            && !self.is_publishable_table(target)
        {
            return Ok(Vec::new());
        }
        let mut out: Vec<(u32, u32)> = Vec::new();
        let mut via_root = false;
        for &name in names {
            let Some(publication) = self.publication_by_name(name) else {
                if target.is_some() {
                    continue;
                }
                return Err(name.to_string());
            };
            let tables = match target {
                Some(target) => {
                    if self.publishable_in(target, publication) {
                        vec![target]
                    } else {
                        Vec::new()
                    }
                }
                None if publication.all_tables => self.all_publication_relations(
                    publication.oid,
                    RelKind::Table,
                    publication.via_root,
                ),
                None => {
                    let leaf = !publication.via_root;
                    let mut tables = self.publication_relations(publication.oid, leaf, false);
                    for table in self.schema_publication_relations(publication.oid, leaf) {
                        if !tables.contains(&table) {
                            tables.push(table);
                        }
                    }
                    tables
                }
            };
            out.extend(tables.into_iter().map(|t| (publication.oid, t)));
            via_root |= publication.via_root;
        }
        if via_root {
            let all: Vec<u32> = out.iter().map(|&(_, t)| t).collect();
            out.retain(|&(_, t)| !self.partition_ancestors(t).iter().any(|a| all.contains(a)));
        }
        Ok(out)
    }
}

rupg_common::persist_struct!(Publication {
    oid,
    name,
    owner,
    all_tables,
    all_sequences,
    insert,
    update,
    delete,
    truncate,
    via_root,
    gencols
});
rupg_common::persist_struct!(PublicationRel { oid, publication, relation, except, qual, attrs });
rupg_common::persist_struct!(PublicationNamespace { oid, publication, schema });

//! `ALTER ... SET SCHEMA` of a relation: the changes of `AlterTableNamespaceInternal`, `AlterRelationNamespaceInternal`, `AlterTypeNamespaceInternal`, `AlterIndexNamespaces`, `AlterSeqNamespaces` and `AlterConstraintNamespaces`.

use rupg_common::Result;

use super::{Catalog, DepKind, ObjRef, PG_CLASS, PG_NAMESPACE, RelKind, is_pinned, no_object};

impl Catalog {
    /// The relations that move with a relation to a new schema, as `AlterTableNamespaceInternal` moves them: the relation, then its indexes, then the sequences that its columns own.
    pub fn moved_relations(&self, oid: u32) -> Vec<u32> {
        let mut moved = vec![oid];
        moved.extend(
            self.relations
                .values()
                .filter(|r| r.index.as_ref().is_some_and(|i| i.table == oid))
                .map(|r| r.oid),
        );
        moved.extend(self.owned_sequences(oid));
        moved
    }

    /// `sequenceIsOwned`: the table whose column owns the sequence, by an automatic or an internal dependency.
    pub fn sequence_owner(&self, sequence: u32) -> Option<u32> {
        let me = ObjRef::new(PG_CLASS, sequence);
        self.depends
            .iter()
            .find(|d| {
                d.object == me
                    && d.referenced.class == PG_CLASS
                    && matches!(d.kind, DepKind::Auto | DepKind::Internal)
            })
            .map(|d| d.referenced.oid)
    }

    /// `AlterTableNamespaceInternal`: moves the relations of [`Catalog::moved_relations`] to the schema, with the row type and the array type of each one and the constraints of the table. `changeDependencyFor` changes the dependency on the schema of each relation that is not an index. The caller checks the names in the new schema.
    ///
    /// # Errors
    ///
    /// An internal error for a relation that the catalog does not have.
    pub fn set_relation_namespace(&mut self, oid: u32, namespace: u32) -> Result<()> {
        let old = self.relation(oid).ok_or_else(|| no_object("relation", oid))?.namespace;
        for moved in self.moved_relations(oid) {
            let rel = self.relation_mut(moved)?;
            rel.namespace = namespace;
            let (row_type, rel_kind) = (rel.row_type, rel.kind);
            let array = self.types.get(&row_type).map_or(0, |t| t.array);
            for ty in [row_type, array] {
                if let Some(ty) = self.types.get_mut(&ty) {
                    ty.namespace = namespace;
                }
            }
            if rel_kind != RelKind::Index {
                self.change_namespace_dependency(ObjRef::new(PG_CLASS, moved), old, namespace);
            }
        }
        for con in self.constraints.values_mut().filter(|c| c.relation == oid) {
            con.namespace = namespace;
        }
        Ok(())
    }

    /// `changeDependencyFor`: the dependency of the object on the schema `old` becomes a dependency on `new`. A pinned schema has no dependency.
    fn change_namespace_dependency(&mut self, object: ObjRef, old: u32, new: u32) {
        let (from, to) = (ObjRef::new(PG_NAMESPACE, old), ObjRef::new(PG_NAMESPACE, new));
        if let Some(at) =
            self.depends.iter().position(|d| d.object == object && d.referenced == from)
        {
            if is_pinned(to) {
                self.depends.remove(at);
            } else if let Some(d) = self.depends.get_mut(at) {
                d.referenced = to;
            }
        } else if is_pinned(from) {
            self.depend(object, to, DepKind::Normal);
        }
    }
}

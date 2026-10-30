//! The rows of `pg_tablespace` that `CREATE TABLESPACE` makes. A tablespace belongs to the cluster, so the store of the server keeps them, as it keeps the rows of `pg_database`. The rows of `pg_default` and `pg_global` are static rows.

use super::{Catalog, RelKind, Result, no_object};

/// A row of `pg_tablespace`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tablespace {
    pub oid: u32,
    pub name: String,
    /// `spcowner`, the owner.
    pub owner: u32,
    /// `spcacl` as the texts of its items, or `None` for the default privileges.
    pub acl: Option<Vec<String>>,
    /// `spcoptions`, each as `name=value`.
    pub options: Vec<String>,
}

impl Catalog {
    /// Sets `reltablespace` of a relation, and of the TOAST table of a table or a materialized view and of the index of the TOAST table. 0 is the default tablespace of the database.
    ///
    /// # Errors
    ///
    /// An error when the relation is not there.
    pub fn set_tablespace(&mut self, oid: u32, tablespace: u32) -> Result<()> {
        let rel = self.relations.get_mut(&oid).ok_or_else(|| no_object("relation", oid))?;
        rel.tablespace = tablespace;
        let toast =
            if matches!(rel.kind, RelKind::Table | RelKind::MatView) { rel.toast } else { 0 };
        let toast_indexes: Vec<u32> = self
            .relations
            .values()
            .filter(|r| toast != 0 && r.index.as_ref().is_some_and(|i| i.table == toast))
            .map(|r| r.oid)
            .collect();
        for oid in std::iter::once(toast).chain(toast_indexes) {
            if let Some(rel) = self.relations.get_mut(&oid) {
                rel.tablespace = tablespace;
            }
        }
        Ok(())
    }

    /// True when a relation of the catalog is in the tablespace, as the files of `destroy_tablespace_directories` show it.
    pub fn uses_tablespace(&self, tablespace: u32) -> bool {
        self.relations.values().any(|r| r.tablespace == tablespace)
    }
}

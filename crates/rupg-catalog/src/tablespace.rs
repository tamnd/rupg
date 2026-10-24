//! The rows of `pg_tablespace` that `CREATE TABLESPACE` makes. A tablespace belongs to the cluster, so the store of the server keeps them, as it keeps the rows of `pg_database`. The rows of `pg_default` and `pg_global` are static rows.

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

//! `CLUSTER ON`, `SET WITHOUT CLUSTER` and `REPLICA IDENTITY` of `ALTER TABLE`, as `ATExecClusterOn`, `ATExecDropCluster` and `ATExecReplicaIdentity` run them. They change `indisclustered`, `indisreplident` and `relreplident` only. rupg does not yet run the statement `CLUSTER`.

use rupg_catalog::{IndexInfo, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{REPLICA_IDENTITY_INDEX, ReplicaIdentityStmt};

use super::alter::Target;
use super::{Definer, Found};

/// The OIDs of the access methods with `amclusterable`: `btree` and `gist`.
const CLUSTERABLE: &[u32] = &[403, 783];

impl Definer<'_, '_> {
    /// The index with this name in the schema of the table, as `get_relname_relid` and `index_open` find it, with the check that it is an index of the table.
    fn table_index(&self, target: &Target, name: &str) -> Result<(u32, &IndexInfo)> {
        let not_index =
            || Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index"));
        let not_for_table = || {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index for table \"{}\"", target.name),
            )
        };
        match self.relation_in(target.namespace, name) {
            None => Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("index \"{name}\" for table \"{}\" does not exist", target.name),
            )),
            Some(Found::Builtin(row)) => {
                Err(if row.kind == b'i' { not_for_table() } else { not_index() })
            }
            Some(Found::User(oid)) => {
                let rel = self.catalog.relation(oid).filter(|r| r.kind == RelKind::Index);
                let info = rel.and_then(|r| r.index.as_ref()).ok_or_else(not_index)?;
                if info.table == target.oid { Ok((oid, info)) } else { Err(not_for_table()) }
            }
        }
    }

    /// `ATExecClusterOn`: the checks of `check_index_is_clusterable`, and then `mark_index_clustered` on the index.
    pub(super) fn cluster_on(&mut self, target: &Target, name: &str) -> Result<()> {
        let (index, info) = self.table_index(target, name)?;
        if !CLUSTERABLE.contains(&info.method) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot cluster on index \"{name}\" because access method does not support clustering"
                ),
            ));
        }
        if info.predicate.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot cluster on partial index \"{name}\""),
            ));
        }
        self.catalog.set_clustered(target.oid, Some(index));
        Ok(())
    }

    /// `ATExecDropCluster`: `mark_index_clustered` with no index clears `indisclustered` on all indexes of the table.
    pub(super) fn drop_cluster(&mut self, target: &Target) {
        self.catalog.set_clustered(target.oid, None);
    }

    /// `ATExecReplicaIdentity`: `DEFAULT`, `FULL` and `NOTHING` set `relreplident`. `USING INDEX` checks that the index is unique, immediate, with no expression and no predicate, and that each key column is not null.
    pub(super) fn replica_identity(
        &mut self,
        target: &Target,
        stmt: &ReplicaIdentityStmt,
    ) -> Result<()> {
        if stmt.identity_type != REPLICA_IDENTITY_INDEX {
            return self.catalog.set_replica_identity(target.oid, stmt.identity_type, None);
        }
        let name = stmt.name.as_deref().unwrap_or_default();
        let (index, info) = self.table_index(target, name)?;
        let unsupported = |what: &str| {
            Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot use {what} index \"{name}\" as replica identity"),
            )
        };
        if !info.unique {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot use non-unique index \"{name}\" as replica identity"),
            ));
        }
        if !info.immediate {
            return Err(unsupported("non-immediate"));
        }
        if info.exprs.is_some() {
            return Err(unsupported("expression"));
        }
        if info.predicate.is_some() {
            return Err(unsupported("partial"));
        }
        let rel = self.catalog.relation(target.oid);
        let count = usize::try_from(info.key_count).unwrap_or_default();
        for &key in info.keys.iter().take(count) {
            if let Some(column) = rel.and_then(|r| r.column(key)).filter(|c| !c.not_null) {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!(
                        "index \"{name}\" cannot be used as replica identity because column \"{}\" is nullable",
                        column.name
                    ),
                ));
            }
        }
        self.catalog.set_replica_identity(target.oid, REPLICA_IDENTITY_INDEX, Some(index))
    }
}

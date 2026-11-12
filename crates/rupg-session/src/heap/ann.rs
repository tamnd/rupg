//! The indexes of `rupg-ann` that the scans by the indexes of `hnsw` and `ivfflat` of pgvector share, by the database, the index, the file of the index and the metric of the scan. Each holds the versions of the rows of the table by their IDs, and `VACUUM` takes out the versions that it removes before a new version can have the same ID.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rupg_ann::Metric;
use rupg_ann::index::{Index, Kind};

use super::Heap;

/// The seed of the layout of each index, so that each search of the same rows gives the same rows.
const SEED: u64 = 0x0005_eed0_fa11;

/// The key of an index of [`Shared`]: the database, the index and its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    database: u32,
    index: u32,
    file: u32,
    metric: Metric,
}

/// An index with its table.
#[derive(Debug)]
struct Entry {
    table: u32,
    index: Arc<Mutex<Index>>,
}

/// The indexes that the scans share.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    indexes: HashMap<Key, Entry>,
}

impl Heap {
    fn ann(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.ann.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The index of `kind` by `metric` of the index `index` with the file `file` of the table `table` of the database `database`, new with no versions at the first scan. The indexes of an earlier file of the index go, as `REINDEX` and `TRUNCATE` give the index a new file.
    pub(crate) fn vector_index(
        &self,
        (database, table): (u32, u32),
        (index, file): (u32, u32),
        kind: Kind,
        metric: Metric,
    ) -> Arc<Mutex<Index>> {
        let mut shared = self.ann();
        let key = Key { database, index, file, metric };
        if let Some(entry) = shared.indexes.get(&key)
            && entry.table == table
            && entry.index.lock().unwrap_or_else(PoisonError::into_inner).kind() == kind
        {
            return Arc::clone(&entry.index);
        }
        shared.indexes.retain(|k, _| k.database != database || k.index != index || k.file == file);
        let made = Arc::new(Mutex::new(Index::new(kind, metric, SEED)));
        shared.indexes.insert(key, Entry { table, index: Arc::clone(&made) });
        made
    }

    /// Takes the versions `gone` of the table `table` of the database `database` out of each index of the table, as `VACUUM` removes them.
    pub(super) fn purge_vector_indexes(&self, database: u32, table: u32, gone: &[u64]) {
        if gone.is_empty() {
            return;
        }
        let shared = self.ann();
        for (key, entry) in &shared.indexes {
            if key.database == database && entry.table == table {
                entry.index.lock().unwrap_or_else(PoisonError::into_inner).remove(gone);
            }
        }
    }

    /// Drops the indexes of the database `database` whose index or table `keep` does not keep, as [`Heap::drop_tables`] drops the tables.
    pub(super) fn drop_vector_indexes(&self, database: u32, keep: impl Fn(u32) -> bool) {
        self.ann()
            .indexes
            .retain(|k, e| k.database != database || (keep(k.index) && keep(e.table)));
    }
}

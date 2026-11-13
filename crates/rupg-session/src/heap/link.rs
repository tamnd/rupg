//! The forward links of the link indexes of spec/13 section 13.2 that the joins through them share, by the database, the index and the file of the index. A forward link holds, by the ID of each child version that a join read, the ID of the parent version that it joined. A join checks each pair that it reads, as for a validated link of section 13.2.4, so the link is a cache of the joins: a pair whose parent changed or went fails the check, and the join then finds the parent by its key and keeps the new pair.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use rupg_func::Forward;

use super::Heap;

/// The key of a forward link of [`Shared`]: the database, the index and its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    database: u32,
    index: u32,
    file: u32,
}

/// A forward link with its child table.
#[derive(Debug)]
struct Entry {
    table: u32,
    forward: Arc<Mutex<Forward>>,
}

/// The forward links that the joins share.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    links: BTreeMap<Key, Entry>,
}

impl Heap {
    fn links(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.links.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The forward link of the index `index` with the file `file` on the table `table` of the database `database`, new and empty at the first join. The links of an earlier file of the index go, as `REINDEX` and `TRUNCATE` give the index a new file.
    pub(crate) fn link_forward(
        &self,
        (database, table): (u32, u32),
        (index, file): (u32, u32),
    ) -> Arc<Mutex<Forward>> {
        let mut shared = self.links();
        let key = Key { database, index, file };
        if let Some(entry) = shared.links.get(&key)
            && entry.table == table
        {
            return Arc::clone(&entry.forward);
        }
        shared.links.retain(|k, _| k.database != database || k.index != index || k.file == file);
        let made = Arc::new(Mutex::new(Forward::new()));
        shared.links.insert(key, Entry { table, forward: Arc::clone(&made) });
        made
    }

    /// Takes the child versions `gone` of the table `table` of the database `database` out of each forward link of the table, as `VACUUM` removes them. A parent version that goes stays in the pairs, which then fail the check of the join.
    pub(super) fn purge_links(&self, database: u32, table: u32, gone: &[u64]) {
        if gone.is_empty() {
            return;
        }
        let shared = self.links();
        for (key, entry) in &shared.links {
            if key.database == database && entry.table == table {
                let mut forward = entry.forward.lock().unwrap_or_else(PoisonError::into_inner);
                for id in gone {
                    forward.remove(id);
                }
            }
        }
    }

    /// Drops the forward links of the database `database` whose index or table `keep` does not keep, as [`Heap::drop_tables`] drops the tables.
    pub(super) fn drop_links(&self, database: u32, keep: impl Fn(u32) -> bool) {
        self.links()
            .links
            .retain(|k, e| k.database != database || (keep(k.index) && keep(e.table)));
    }
}

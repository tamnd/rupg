//! The rows of the tables in memory, with the transaction IDs of `heapam_visibility.c`. Each version of a row has the transaction and the command that made it, and the transaction and the command that deleted it. A snapshot tells which versions a statement sees.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_types::Value;

/// `FirstNormalTransactionId`: the first transaction ID that a transaction gets.
const FIRST_NORMAL_XID: u32 = 3;

/// The rows of the tables of all the databases, and the transactions that are in progress.
#[derive(Debug, Default)]
pub(crate) struct Heap {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// `nextXid`: the transaction ID of the next transaction that writes.
    next_xid: u32,
    /// The transactions that are in progress, `ProcArray`.
    running: BTreeSet<u32>,
    /// The versions of the rows of each table, by the OID of the database and the OID of the table.
    tables: HashMap<(u32, u32), Vec<Version>>,
}

/// One version of a row, `HeapTupleHeaderData`.
#[derive(Debug)]
struct Version {
    /// The transaction that inserted the version.
    xmin: u32,
    /// The command of `xmin` that inserted the version.
    cmin: u32,
    /// The transaction that deleted the version, or 0.
    xmax: u32,
    /// The command of `xmax` that deleted the version.
    cmax: u32,
    values: Box<[Value]>,
}

/// `SnapshotData` of an MVCC snapshot: the transactions whose changes a statement does not see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// All the transactions from this ID are not visible.
    xmax: u32,
    /// The transactions before `xmax` that were in progress.
    running: BTreeSet<u32>,
}

/// What a statement sees of the rows: its snapshot, and the transaction and the command that run it.
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub(crate) database: u32,
    pub(crate) snapshot: Snapshot,
    /// The transaction ID of the transaction, or 0 when the transaction did not write.
    pub(crate) xid: u32,
    /// `currentCommandId`.
    pub(crate) command: u32,
}

impl Heap {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetNewTransactionId`: a transaction ID for a transaction that writes. The transaction is in progress until [`Heap::commit`] or [`Heap::abort`].
    pub(crate) fn begin(&self) -> u32 {
        let mut state = self.state();
        let xid = state.next_xid.max(FIRST_NORMAL_XID);
        state.next_xid = xid + 1;
        state.running.insert(xid);
        xid
    }

    /// `GetSnapshotData`: the transactions whose changes are not visible now.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot { xmax: state.next_xid.max(FIRST_NORMAL_XID), running: state.running.clone() }
    }

    /// The rows of a table that the view sees, `HeapTupleSatisfiesMVCC`.
    pub(crate) fn rows(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(versions) = state.tables.get(&(view.database, relation)) else {
            return Vec::new();
        };
        versions
            .iter()
            .filter(|version| visible(version, view))
            .map(|v| v.values.to_vec())
            .collect()
    }

    /// Adds the rows that the command of the view inserted.
    pub(crate) fn insert(&self, relation: u32, view: &View, rows: Vec<Vec<Value>>) {
        let mut state = self.state();
        let versions = state.tables.entry((view.database, relation)).or_default();
        versions.extend(rows.into_iter().map(|values| Version {
            xmin: view.xid,
            cmin: view.command,
            xmax: 0,
            cmax: 0,
            values: values.into_boxed_slice(),
        }));
    }

    /// True if a table has a version of a row, visible or not.
    pub(crate) fn has_rows(&self, database: u32, relation: u32) -> bool {
        self.state().tables.get(&(database, relation)).is_some_and(|versions| !versions.is_empty())
    }

    /// `RecordTransactionCommit`: the changes of the transaction become visible to the new snapshots.
    pub(crate) fn commit(&self, xid: u32) {
        self.state().running.remove(&xid);
    }

    /// `RecordTransactionAbort`: the rows that the transaction inserted go, and the rows that it deleted stay.
    pub(crate) fn abort(&self, xid: u32) {
        let mut state = self.state();
        for versions in state.tables.values_mut() {
            versions.retain(|version| version.xmin != xid);
            for version in versions.iter_mut().filter(|version| version.xmax == xid) {
                version.xmax = 0;
                version.cmax = 0;
            }
        }
        state.tables.retain(|_, versions| !versions.is_empty());
        state.running.remove(&xid);
    }

    /// Removes the rows of the tables of a database that `keep` does not keep, after a commit drops the tables.
    pub(crate) fn drop_tables(&self, database: u32, keep: impl Fn(u32) -> bool) {
        self.state().tables.retain(|&(db, relation), _| db != database || keep(relation));
    }
}

/// `XidInMVCCSnapshot` with the reverse sense: true if the snapshot sees the changes of another transaction.
fn committed_before(xid: u32, snapshot: &Snapshot) -> bool {
    xid < snapshot.xmax && !snapshot.running.contains(&xid)
}

/// `HeapTupleSatisfiesMVCC`: true if the view sees the version.
fn visible(version: &Version, view: &View) -> bool {
    let inserted = if view.xid != 0 && version.xmin == view.xid {
        version.cmin < view.command
    } else {
        committed_before(version.xmin, &view.snapshot)
    };
    if !inserted {
        return false;
    }
    match version.xmax {
        0 => true,
        xmax if view.xid != 0 && xmax == view.xid => version.cmax >= view.command,
        xmax => !committed_before(xmax, &view.snapshot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(snapshot: Snapshot, xid: u32, command: u32) -> View {
        View { database: 1, snapshot, xid, command }
    }

    #[test]
    fn a_snapshot_sees_only_the_committed_rows() {
        let heap = Heap::default();
        let xid = heap.begin();
        heap.insert(7, &view(heap.snapshot(), xid, 0), vec![vec![Value::Int4(1)]]);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), xid, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), xid, 1)).len(), 1);
        let before = heap.snapshot();
        heap.commit(xid);
        assert_eq!(heap.rows(7, &view(before, 0, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
    }

    #[test]
    fn an_abort_removes_the_rows() {
        let heap = Heap::default();
        let xid = heap.begin();
        heap.insert(7, &view(heap.snapshot(), xid, 0), vec![vec![Value::Int4(1)]]);
        heap.abort(xid);
        assert!(!heap.has_rows(1, 7));
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
    }
}

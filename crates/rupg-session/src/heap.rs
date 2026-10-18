//! The rows of the tables in memory, with the transaction IDs of `heapam_visibility.c`. Each version of a row has the transaction and the command that made it, and the transaction and the command that deleted it. A snapshot tells which versions a statement sees.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

/// `FirstNormalTransactionId`: the first transaction ID that a transaction gets.
const FIRST_NORMAL_XID: u32 = 3;

/// `MaxHeapTuplesPerPage`: the most versions that a page of 8 kB has. The ID of a version gives its page and its place in the page as if each page had this number of versions.
const TUPLES_PER_PAGE: u64 = 291;

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
    /// The process ID of the session of each transaction that is in progress.
    owners: HashMap<u32, i32>,
    /// The versions of the rows of each table, by the OID of the database and the OID of the table.
    tables: HashMap<(u32, u32), Table>,
    /// The transaction that each waiting transaction waits for, the edges of the wait graph of `DeadLockCheck`.
    waits: HashMap<u32, u32>,
}

/// An edge of a cycle of waits: the process that waits, the transaction that it waits for, and the process of that transaction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) pid: i32,
    pub(crate) xid: u32,
    pub(crate) holder: i32,
}

/// The versions of the rows of a table, in the order of their IDs.
#[derive(Debug, Default)]
struct Table {
    versions: Vec<Version>,
    /// The ID of the last version that the table got. An ID is not used again, also after an abort.
    last_tid: u64,
}

impl Table {
    /// Adds a version and gives its ID.
    fn push(&mut self, xmin: u32, cmin: u32, values: Vec<Value>) -> u64 {
        self.last_tid += 1;
        let tid = self.last_tid;
        let values = values.into_boxed_slice();
        self.versions.push(Version { tid, xmin, cmin, xmax: 0, cmax: 0, next: 0, values });
        tid
    }
}

/// One version of a row, `HeapTupleHeaderData`.
#[derive(Debug)]
struct Version {
    /// The ID of the version in its table, from 1, which takes the place of the item pointer of the version. It does not change while the version lives.
    tid: u64,
    /// The transaction that inserted the version.
    xmin: u32,
    /// The command of `xmin` that inserted the version.
    cmin: u32,
    /// The transaction that deleted the version, or 0.
    xmax: u32,
    /// The command of `xmax` that deleted the version.
    cmax: u32,
    /// `t_ctid`: the ID of the version that an `UPDATE` made from this version, or 0.
    next: u64,
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
    /// True in `REPEATABLE READ` and `SERIALIZABLE`, where a row that another transaction changed after the snapshot is an error.
    pub(crate) serializable: bool,
}

/// A change of a row by `UPDATE` or `DELETE`: the ID of the version, and the new values, or `None` to delete it.
pub(crate) type Change = (u64, Option<Vec<Value>>);

/// The result of [`Heap::change`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Changed {
    /// The number of rows that changed.
    Done(u64),
    /// The transaction `holder` is in progress and changed the version `tid`, so the statement must wait for the end of `holder` and try again.
    Wait { holder: u32, tid: u64 },
    /// In `READ COMMITTED`, a transaction that committed after the snapshot changed a row. The statement must run again with a new snapshot, which sees the last version of the row, as `EvalPlanQual` checks it.
    Again,
}

/// The text of the item pointer of a version, `(page,item)`, as `tidout` gives it.
pub(crate) fn tid_text(tid: u64) -> String {
    let at = tid.saturating_sub(1);
    format!("({},{})", at / TUPLES_PER_PAGE, at % TUPLES_PER_PAGE + 1)
}

impl Heap {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetNewTransactionId`: a transaction ID for a transaction of the session `pid` that writes. The transaction is in progress until [`Heap::commit`] or [`Heap::abort`].
    pub(crate) fn begin(&self, pid: i32) -> u32 {
        let mut state = self.state();
        let xid = state.next_xid.max(FIRST_NORMAL_XID);
        state.next_xid = xid + 1;
        state.running.insert(xid);
        state.owners.insert(xid, pid);
        xid
    }

    /// `GetSnapshotData`: the transactions whose changes are not visible now.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot { xmax: state.next_xid.max(FIRST_NORMAL_XID), running: state.running.clone() }
    }

    /// The rows of a table that the view sees, `HeapTupleSatisfiesMVCC`. Each row ends with the ID of its version.
    pub(crate) fn rows(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.get(&(view.database, relation)) else {
            return Vec::new();
        };
        table
            .versions
            .iter()
            .filter(|version| visible(version, view))
            .map(|version| {
                let mut row = Vec::with_capacity(version.values.len() + 1);
                row.extend_from_slice(&version.values);
                row.push(Value::Int8(version.tid.cast_signed()));
                row
            })
            .collect()
    }

    /// Adds the rows that the command of the view inserted.
    pub(crate) fn insert(&self, relation: u32, view: &View, rows: Vec<Vec<Value>>) {
        let mut state = self.state();
        let table = state.tables.entry((view.database, relation)).or_default();
        for values in rows {
            table.push(view.xid, view.command, values);
        }
    }

    /// `heap_update` and `heap_delete` for the rows that the command of the view changes. A row that the command changed before stays as it is, as for a row that a join gives more than once. When another transaction changed a row after the snapshot, nothing changes, and the result tells the statement to wait or to run again.
    ///
    /// # Errors
    ///
    /// `40001` in `REPEATABLE READ` and `SERIALIZABLE` for a row that a transaction that committed after the snapshot changed.
    pub(crate) fn change(
        &self,
        relation: u32,
        view: &View,
        changes: Vec<Change>,
    ) -> Result<Changed> {
        let mut state = self.state();
        let State { tables, running, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(view.database, relation)) else {
            return Ok(Changed::Done(0));
        };
        let mut found = Vec::with_capacity(changes.len());
        for (tid, values) in changes {
            let Ok(at) = table.versions.binary_search_by_key(&tid, |version| version.tid) else {
                continue;
            };
            let version = &table.versions[at];
            match version.xmax {
                0 => {}
                xmax if xmax == view.xid => continue,
                xmax if running.contains(&xmax) => {
                    return Ok(Changed::Wait { holder: xmax, tid });
                }
                _ if view.serializable => {
                    let what = if version.next == 0 { "delete" } else { "update" };
                    return Err(Error::new(
                        SqlState::T_R_SERIALIZATION_FAILURE,
                        format!("could not serialize access due to concurrent {what}"),
                    ));
                }
                _ => return Ok(Changed::Again),
            }
            if found.iter().any(|&(i, _)| i == at) {
                continue;
            }
            found.push((at, values));
        }
        let count = found.len() as u64;
        for (at, values) in found {
            let next = match values {
                Some(values) => table.push(view.xid, view.command, values),
                None => 0,
            };
            let version = &mut table.versions[at];
            version.xmax = view.xid;
            version.cmax = view.command;
            version.next = next;
        }
        Ok(Changed::Done(count))
    }

    /// Records that `xid` waits for `holder`, as `LockAcquire` adds an edge to the wait graph.
    pub(crate) fn wait(&self, xid: u32, holder: u32) {
        self.state().waits.insert(xid, holder);
    }

    /// The end of the wait of `xid`.
    pub(crate) fn waited(&self, xid: u32) {
        self.state().waits.remove(&xid);
    }

    /// `DeadLockCheck`: the cycle of waits from `xid` back to `xid`, or `None` when the waits of `xid` make no cycle.
    pub(crate) fn deadlock(&self, xid: u32) -> Option<Vec<Wait>> {
        let state = self.state();
        let pid = |xid: u32| state.owners.get(&xid).copied().unwrap_or_default();
        let mut cycle = Vec::new();
        let mut next = xid;
        while let Some(&holder) = state.waits.get(&next) {
            cycle.push(Wait { pid: pid(next), xid: holder, holder: pid(holder) });
            if holder == xid {
                return Some(cycle);
            }
            if cycle.len() > state.waits.len() {
                return None;
            }
            next = holder;
        }
        None
    }

    /// True while a transaction is in progress.
    pub(crate) fn running(&self, xid: u32) -> bool {
        self.state().running.contains(&xid)
    }

    /// True if a table has a version of a row, visible or not.
    pub(crate) fn has_rows(&self, database: u32, relation: u32) -> bool {
        let state = self.state();
        state.tables.get(&(database, relation)).is_some_and(|table| !table.versions.is_empty())
    }

    /// `RecordTransactionCommit`: the changes of the transaction become visible to the new snapshots.
    pub(crate) fn commit(&self, xid: u32) {
        let mut state = self.state();
        state.running.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
    }

    /// `RecordTransactionAbort`: the rows that the transaction inserted go, and the rows that it deleted stay.
    pub(crate) fn abort(&self, xid: u32) {
        let mut state = self.state();
        for table in state.tables.values_mut() {
            table.versions.retain(|version| version.xmin != xid);
            for version in table.versions.iter_mut().filter(|version| version.xmax == xid) {
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
        state.running.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
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
        View { database: 1, snapshot, xid, command, serializable: false }
    }

    /// The ID of the version of the first row that the view sees.
    fn first_tid(heap: &Heap, view: &View) -> u64 {
        let rows = heap.rows(7, view);
        let Some(Value::Int8(tid)) = rows[0].last() else { panic!("a row with no ID") };
        tid.cast_unsigned()
    }

    #[test]
    fn a_snapshot_sees_only_the_committed_rows() {
        let heap = Heap::default();
        let xid = heap.begin(1);
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
        let xid = heap.begin(1);
        heap.insert(7, &view(heap.snapshot(), xid, 0), vec![vec![Value::Int4(1)]]);
        heap.abort(xid);
        assert!(!heap.has_rows(1, 7));
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
        let xid = heap.begin(1);
        heap.insert(7, &view(heap.snapshot(), xid, 0), vec![vec![Value::Int4(1)]]);
        assert_eq!(first_tid(&heap, &view(heap.snapshot(), xid, 1)), 2);
    }

    #[test]
    fn an_update_waits_for_the_other_transaction() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(7, &view(heap.snapshot(), first, 0), vec![vec![Value::Int4(1)]]);
        heap.commit(first);
        let a = heap.begin(1);
        let b = heap.begin(2);
        let tid = first_tid(&heap, &view(heap.snapshot(), a, 0));
        let change = || vec![(tid, Some(vec![Value::Int4(2)]))];
        let done = heap.change(7, &view(heap.snapshot(), a, 0), change()).unwrap();
        assert_eq!(done, Changed::Done(1));
        let wait = Changed::Wait { holder: a, tid };
        assert_eq!(heap.change(7, &view(heap.snapshot(), b, 0), change()).unwrap(), wait);
        heap.wait(b, a);
        assert_eq!(heap.deadlock(b), None);
        heap.wait(a, b);
        let cycle = heap.deadlock(b).unwrap();
        assert_eq!(cycle, [Wait { pid: 2, xid: a, holder: 1 }, Wait { pid: 1, xid: b, holder: 2 }]);
        heap.waited(a);
        heap.abort(a);
        let done = heap.change(7, &view(heap.snapshot(), b, 0), change()).unwrap();
        assert_eq!(done, Changed::Done(1));
        heap.commit(b);
        let rows = heap.rows(7, &view(heap.snapshot(), 0, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(2));
    }

    #[test]
    fn a_committed_change_runs_the_statement_again() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(7, &view(heap.snapshot(), first, 0), vec![vec![Value::Int4(1)]]);
        heap.commit(first);
        let old = heap.snapshot();
        let tid = first_tid(&heap, &view(old.clone(), 0, 0));
        let a = heap.begin(1);
        heap.change(7, &view(heap.snapshot(), a, 0), vec![(tid, None)]).unwrap();
        heap.commit(a);
        let b = heap.begin(2);
        let change = vec![(tid, Some(vec![Value::Int4(3)]))];
        assert_eq!(
            heap.change(7, &view(old.clone(), b, 0), change.clone()).unwrap(),
            Changed::Again
        );
        let serializable = View { serializable: true, ..view(old, b, 0) };
        let error = heap.change(7, &serializable, change).unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        assert_eq!(error.message(), "could not serialize access due to concurrent delete");
        assert_eq!(tid_text(1), "(0,1)");
        assert_eq!(tid_text(292), "(1,1)");
    }
}

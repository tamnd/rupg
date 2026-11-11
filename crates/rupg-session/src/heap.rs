//! The rows of the tables in memory, with the transaction IDs of `heapam_visibility.c`. Each version of a row has the transaction and the command that made it, and the transaction and the command that deleted it. A snapshot tells which versions a statement sees.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{self, AtomicU32};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rupg_analyze::{Derived, Unique};
use rupg_column::segment::{Kind, ROWS};
use rupg_column::stored::{Stored, VECTOR};
use rupg_common::persist::{Persist, Reader, put_count};
use rupg_common::{Error, Result, SqlState};
use rupg_encode::chooser::Sampled;
use rupg_encode::integer;
use rupg_func::{HeapSize, Statistic, XactState};
use rupg_platform::{Job, Tasks};
use rupg_types::{Value, oid};

pub(crate) use btree::{Shape, Size as BtreeSize};
pub(crate) use cells::{Cells, Values};
pub(crate) use pages::{Fixed, Skip, Storage, datum_width, stored_widths};
use pages::{Pages, Pruned, generation_of, max_align, page_of, slot_of, toast};

/// `MAX_BUFFERED_TUPLES`: the rows of a batch of a copy.
const BUFFERED_TUPLES: usize = 1000;

/// `MAX_BUFFERED_BYTES`: the bytes of the lines of a batch of a copy.
pub(crate) const BUFFERED_BYTES: usize = 65535;

/// `CopyMultiInsertInfoStore` and `CopyMultiInsertInfoIsFull`: the batch `batch` of a copy, its rows and the bytes of their lines, takes a row with a line of `line` bytes. True when the batch is then full, and the next row starts a new batch.
pub(crate) fn copy_batch_full(batch: &mut (usize, usize), line: usize) -> bool {
    batch.0 += 1;
    batch.1 = batch.1.saturating_add(line);
    let full = batch.0 >= BUFFERED_TUPLES || batch.1 >= BUFFERED_BYTES;
    if full {
        *batch = (0, 0);
    }
    full
}

mod btree;
mod cells;
mod fsm;
mod pages;
mod ssi;

/// The key of a row in a unique index with an expression or a predicate, or `None` for a row that is not in the index, as [`rupg_exec::index_key`] gives it. A function in an expression can read and change the heap, so the heap calls it only when it is not locked.
pub(crate) type Derive<'a> = &'a dyn Fn(&Derived, &[Value]) -> Result<Option<Vec<Value>>>;

/// The keys of a new row in the unique indexes with an expression or a predicate, by the place of the index in the unique keys of the write. The place of an index with no expression, or of an index that does not have the row, has `None`. The list is empty when no index has an expression.
type Fresh = Vec<DerivedKey>;

/// The key of a row in an index with an expression or a predicate, or `None` when the index does not have the row.
type DerivedKey = Option<Box<[Value]>>;

/// The keys of the new row `values` in the indexes of `unique` with an expression or a predicate. The expressions run before the write locks the heap.
fn fresh(unique: &[Unique], values: &[Value], derive: Derive<'_>) -> Result<Fresh> {
    if unique.iter().all(|key| key.derived.is_none()) {
        return Ok(Vec::new());
    }
    let mut keys = Vec::with_capacity(unique.len());
    for key in unique {
        keys.push(match &key.derived {
            Some(derived) => derive(derived, values)?.map(Vec::into_boxed_slice),
            None => None,
        });
    }
    Ok(keys)
}

/// The most versions that the log of a commit has. A commit that changed more, as a large `COPY` does, makes a checkpoint instead, so the log does not keep a second copy of the rows in memory.
const LOG_VERSIONS: usize = 1 << 20;

/// What a commit gives to the log.
pub(crate) enum Logged<'a> {
    /// The transaction changed no table, or the heap writes no logs.
    Nothing,
    /// The bytes of the changes, for [`Heap::redo`].
    Changes(&'a [u8]),
    /// The transaction changed more than [`LOG_VERSIONS`] versions, so the commit needs a checkpoint.
    Large,
}

/// `nextXid` of a new cluster: `initdb` of PostgreSQL 19 uses the transaction IDs from `FirstNormalTransactionId` up to 661, so the first transaction gets 662.
const INITDB_NEXT_XID: u32 = 662;

/// The fewest versions of a table that [`Heap::columns`] makes columns for. A smaller table reads faster by rows than the columns take to build.
const COLUMN_ROWS: usize = 65_536;

/// The number of versions that [`Frozen::append`] gives to each column at one time. The values of this many versions of a wide table fit in the cache of a core.
const APPEND_TILE: usize = 64;

/// The last pages of a table whose versions keep their values in a load, as most new versions go into them.
const LOAD_PAGES: u32 = 16;

/// The new versions that a load freezes at one time. The rows go on the open rows of the columns, so a small batch does not encode a row again.
const LOAD_ROWS: usize = 8_192;

/// The fewest columns that each thread of [`on_runs`] encodes. A thread for fewer columns costs more than it saves.
const THREAD_COLUMNS: usize = 4;

/// Runs `each` on runs of the columns, each with the place of its first column, with the jobs of [`Tasks::run_all`], and gives the result of each run in order. A column is in one run only. With no tasks, or with few columns, all the columns are one run on the thread of the caller.
fn on_runs<T: Send>(
    tasks: Option<&dyn Tasks>,
    columns: &mut [Arc<Stored>],
    each: impl Fn(usize, &mut [Arc<Stored>]) -> T + Sync,
) -> Vec<T> {
    let jobs = tasks.map_or(1, |tasks| tasks.parallelism().min(columns.len() / THREAD_COLUMNS));
    let (Some(tasks), 2..) = (tasks, jobs) else {
        return vec![each(0, columns)];
    };
    let run = columns.len().div_ceil(jobs);
    let each = &each;
    let mut results: Vec<Option<T>> = Vec::new();
    results.resize_with(columns.len().div_ceil(run), || None);
    let work: Vec<Job<'_>> = columns
        .chunks_mut(run)
        .zip(&mut results)
        .enumerate()
        .map(|(chunk, (columns, result))| {
            Box::new(move || *result = Some(each(chunk * run, columns))) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    results.into_iter().flatten().collect()
}

/// The result of [`Heap::vacuum`].
pub(crate) struct Vacuumed {
    /// `reltuples` of the heap.
    pub(crate) reltuples: f32,
    /// `rel_pages`: the pages of the heap before the truncation.
    pub(crate) pages: u32,
    /// `lpdead_item_pages`: the pages with `LP_DEAD` items, which had dead versions, until [`Heap::vacuumed_indexes`].
    pub(crate) dead_pages: u32,
}

/// The most bytes of [`State::log_buffer`] that stay after a commit, so that the log of one large commit does not keep its memory.
const LOG_BUFFER: usize = 1 << 20;

/// The rows of the tables of all the databases, and the transactions that are in progress.
#[derive(Debug, Default)]
pub(crate) struct Heap {
    /// The state of the heap. A point read of a key whose table needs no change takes a read lock, so that the point reads of many sessions do not wait for each other. Each other use takes the write lock.
    state: RwLock<State>,
    /// The snapshot of the state, which each change of `nextXid`, of the transactions in progress or of `latestCompletedXid` writes while the state is locked, or `None` until the first change. [`Heap::held_snapshot_in`] takes it and sets the `xmin` of a session under this lock only, so that a statement does not wait for the state. A horizon reads the `xmin` of the sessions under this lock too, so it sees each `xmin` that a snapshot before it set, and a later snapshot is newer than the horizon.
    published: Mutex<Option<Snapshot>>,
    /// The tasks of the server, which encode the columns of a load at the same time, or `None` to encode them on the thread of the session.
    tasks: Option<Arc<dyn Tasks>>,
    /// The serializable transactions and their `SIREAD` locks. A lock of it comes before a lock of the state.
    ssi: Mutex<ssi::Ssi>,
}

#[derive(Debug, Default)]
struct State {
    /// `nextXid`: the transaction ID of the next transaction that writes.
    next_xid: u32,
    /// The transactions that are in progress, `ProcArray`. Each snapshot shares the set until a transaction starts or ends, as `GetSnapshotData` keeps a snapshot while `xactCompletionCount` does not change.
    running: Arc<BTreeSet<u32>>,
    /// `latestCompletedXid`: the last transaction that committed or aborted, or 0.
    latest_completed: u32,
    /// The transactions that aborted, the aborted entries of `pg_xact`. A transaction before `next_xid` that is not in progress and did not abort committed.
    aborted: BTreeSet<u32>,
    /// The process ID of the session of each transaction that is in progress.
    owners: HashMap<u32, i32>,
    /// `PGPROC->xmin`: the `xmin` of the oldest snapshot that each session holds, by the process ID, or 0 for none. A hold sets it under the lock of the state, and the session clears it with no lock, as [`Heap::xmin`] gives it.
    xmins: HashMap<i32, Arc<AtomicU32>>,
    /// The versions of the rows of each table, by the OID of the database and the OID of the table.
    tables: Tables,
    /// The number of the changes that the log does not have: the statistics of `ANALYZE`, and the `LP_DEAD` items and the stats of a table of `VACUUM`. A timed checkpoint writes the image when the number changed after the last image, though the log has no record.
    unlogged: u64,
    /// The tables that each transaction in progress truncated, whose versions from before the `TRUNCATE` go at the commit.
    truncating: HashMap<u32, Vec<(u32, u32)>>,
    /// The transaction that each waiting transaction waits for, the edges of the wait graph of `DeadLockCheck`.
    waits: HashMap<u32, u32>,
    /// The version and the strength of each wait of [`State::waits`] for a row, which ends when the transaction that it waits for no longer blocks the row.
    row_waits: HashMap<u32, ((u32, u32, u64), RowStrength)>,
    /// The new versions of the transactions in progress whose keys a deferrable unique index must check again.
    rechecks: Vec<Recheck>,
    /// The number of the values that the writes moved to the TOAST tables, whose OIDs the counter did not give yet.
    toast_oids: u32,
    /// `comboCids` of each transaction in progress: the combo command ID of each pair of `cmin` and `cmax`, in the order that `GetComboCommandId` gives them.
    combos: HashMap<u32, HashMap<(u32, u32), u32>>,
    /// True when each commit writes a log of its changes, for a store with a file.
    logging: bool,
    /// The tables that each transaction in progress changed, whose changes go to the log of its commit.
    touched: HashMap<u32, Vec<Touched>>,
    /// The tables whose open rows each transaction in progress filled in a load, which become a last segment at its end.
    loading: HashMap<u32, Vec<(u32, u32)>>,
    /// The empty buffer of the log of the last commit. A commit writes its log while the heap is locked, so a buffer that keeps its size lets it write with no new memory.
    log_buffer: Vec<u8>,
    /// The row locks of `FOR UPDATE` and the other locking clauses: the transactions that lock each version, by the OID of the database, the OID of the table and the ID of the version, with the strongest strength of each (spec/11 section 11.7). A lock stays in memory only and goes at the end of its transaction.
    row_locks: RowLocks,
    /// The tuple locks of `heap_acquire_tuplock`: the transactions that wait for a row lock or a change of each version, in the order of their waits, with the strength that each wants.
    row_queues: RowQueues,
    /// `nextMXact` and `nextOffset` from `FirstMultiXactId`: the number of the multixacts that the row locks made, and the sum of their members. A lock of a version that another transaction or subtransaction locks too makes a new multixact with all the lockers, as `MultiXactIdExpand` does. rupg keeps no multixacts after their lockers end, and never truncates, so the numbers only grow.
    multixacts: (u64, u64),
    /// The prepared transactions that a restart keeps, by their transaction IDs, with the bytes of what the log keeps of each, as the state files of `pg_twophase` keep them. A prepared transaction stays in `running`, and the image keeps its changes as the changes of a commit.
    prepared: BTreeMap<u32, Vec<u8>>,
}

/// The strength of a row lock, `LockTupleMode`, in the order of `LockClauseStrength` from 1.
pub(crate) type RowStrength = u8;

/// `LockTupleKeyShare`: the lock of `FOR KEY SHARE`, which a check of a foreign key takes.
pub(crate) const KEY_SHARE: RowStrength = 1;

/// `LockTupleShare`: the lock of `FOR SHARE`.
pub(crate) const SHARE: RowStrength = 2;

/// `LockTupleNoKeyExclusive`: the lock of `FOR NO KEY UPDATE`, which an `UPDATE` that changes no key column takes.
pub(crate) const NO_KEY_UPDATE: RowStrength = 3;

/// `LockTupleExclusive`: the lock of `FOR UPDATE`, which a `DELETE` and an `UPDATE` of a key column take.
pub(crate) const UPDATE: RowStrength = 4;

/// `MovedPartitionsOffsetNumber`: the [`Version::next`] of a version that an `UPDATE` deleted to move its row to another partition, as `ItemPointerSetMovedPartitions` sets `t_ctid`.
const MOVED: u64 = u64::MAX;

/// The error of a write or a row lock in `READ COMMITTED` that finds its row moved to another partition by a transaction that committed after the snapshot.
fn moved_error() -> Error {
    Error::new(
        SqlState::T_R_SERIALIZATION_FAILURE,
        "tuple to be locked was already moved to another partition due to concurrent update",
    )
}

/// The conflicts of the row locks, as `tupleLockExtraInfo` gives them: `KEY SHARE` conflicts only with `UPDATE`, `SHARE` with `NO KEY UPDATE` and `UPDATE`, and the two updates with all the strengths.
fn row_conflict(held: RowStrength, wanted: RowStrength) -> bool {
    held + wanted > UPDATE
}

/// The row locks of [`State::row_locks`] by the version: the transaction, the strength and the command that took the lock. A subtransaction has no transaction ID of its own, as `MultiXactIdCreate` gives the lock of a subtransaction, so the command tells the locks that a rollback to a savepoint takes away.
type RowLocks = HashMap<(u32, u32, u64), Lockers>;

/// The lockers of a version: the transaction, the strength and the command that took the lock.
type Lockers = Vec<(u32, RowStrength, u32)>;

/// The row locks of [`State::row_locks`] that the transactions of `keep` hold, with only those lockers, in the order of the versions, for the log and the image of the prepared transactions.
fn kept_row_locks(locks: &RowLocks, keep: impl Fn(u32) -> bool) -> Vec<((u32, u32, u64), Lockers)> {
    let mut kept: Vec<_> = locks
        .iter()
        .filter_map(|(row, lockers)| {
            let lockers: Lockers = lockers.iter().copied().filter(|&(x, _, _)| keep(x)).collect();
            (!lockers.is_empty()).then_some((*row, lockers))
        })
        .collect();
    kept.sort_unstable_by_key(|(row, _)| *row);
    kept
}

/// [`Heap::row_blocked`] on the state of the heap.
fn row_blocked(
    state: &mut State,
    (database, relation, tid): (u32, u32, u64),
    (holder, strength): (u32, RowStrength),
) -> bool {
    let State { tables, row_locks, row_queues, .. } = state;
    // A transaction that waits in the queue of the version with a strength that conflicts holds its tuple lock.
    if row_queues.get(&(database, relation, tid)).is_some_and(|queue| {
        queue.iter().any(|&(waiter, wanted)| waiter == holder && row_conflict(wanted, strength))
    }) {
        return true;
    }
    let Some(table) = tables.get(&(database, relation)) else { return false };
    let mut tid = tid;
    loop {
        let locked = row_locks.get(&(database, relation, tid)).is_some_and(|lockers| {
            lockers
                .iter()
                .any(|&(locker, held, _)| locker == holder && row_conflict(held, strength))
        });
        let Some(at) = table.find(tid) else { return locked };
        let version = &table.versions[at];
        if locked || version.xmax == holder {
            return true;
        }
        if version.xmax == 0 || matches!(version.next, 0 | MOVED) || version.next == tid {
            return false;
        }
        tid = version.next;
    }
}

/// Adds the row locks of [`kept_row_locks`] to `locks`, after a restart.
fn restore_row_locks(locks: &mut RowLocks, kept: Vec<((u32, u32, u64), Lockers)>) {
    for (row, lockers) in kept {
        let held = locks.entry(row).or_default();
        for locker in lockers {
            if !held.contains(&locker) {
                held.push(locker);
            }
        }
    }
}

/// The waits of [`State::row_queues`] by the version: the transaction and the strength that it wants.
type RowQueues = HashMap<(u32, u32, u64), Vec<(u32, RowStrength)>>;

/// `heap_acquire_tuplock` for the transaction `xid`, which must wait for the transaction `holder` to lock or change the version `row` with `strength`. The transaction goes in the queue of the version, and waits first for a transaction before it in the queue that wants a strength that conflicts, which holds the tuple lock, so that the waits get the row in their order. A transaction that holds a lock on the version already does not go in the queue, so that an upgrade of a lock does not wait for a transaction that waits for that lock. The result is the transaction to wait for.
fn queue_row(
    (queues, locks): (&mut RowQueues, &RowLocks),
    row: (u32, u32, u64),
    (xid, strength): (u32, RowStrength),
    holder: u32,
) -> u32 {
    unqueue_rows(queues, xid, Some(row));
    if locks.get(&row).is_some_and(|lockers| lockers.iter().any(|&(locker, ..)| locker == xid)) {
        unqueue_rows(queues, xid, None);
        return holder;
    }
    let queue = queues.entry(row).or_default();
    let mine = queue.iter().position(|&(waiter, _)| waiter == xid);
    let first = queue[..mine.unwrap_or(queue.len())]
        .iter()
        .find(|&&(waiter, wanted)| waiter != xid && row_conflict(wanted, strength))
        .map(|&(waiter, _)| waiter);
    match mine {
        Some(at) => queue[at].1 = queue[at].1.max(strength),
        None => queue.push((xid, strength)),
    }
    first.unwrap_or(holder)
}

/// The end of the waits of the transaction `xid` for rows, other than for the version `kept`: it leaves their queues, as it releases its tuple locks.
fn unqueue_rows(queues: &mut RowQueues, xid: u32, kept: Option<(u32, u32, u64)>) {
    if queues.is_empty() {
        return;
    }
    queues.retain(|row, queue| {
        if Some(*row) != kept {
            queue.retain(|&(waiter, _)| waiter != xid);
        }
        !queue.is_empty()
    });
}

/// A lock of the transaction `xid` in the command `command`: no new entry when the transaction holds the strength or a stronger one already, and a stronger strength for an entry of the same command. A new lock of a version with more than one locker makes a multixact of all the lockers, which `multixacts` counts.
fn add_row_lock(
    (lockers, multixacts): (&mut Lockers, &mut (u64, u64)),
    (xid, strength, command): (u32, RowStrength, u32),
) {
    if lockers.iter().any(|&(locker, held, _)| locker == xid && held >= strength) {
        return;
    }
    match lockers.iter_mut().find(|(locker, _, at)| *locker == xid && *at == command) {
        Some((_, held, _)) => *held = strength,
        None => lockers.push((xid, strength, command)),
    }
    if lockers.len() > 1 {
        multixacts.0 += 1;
        multixacts.1 += lockers.len() as u64;
    }
}

/// The transaction in progress other than `xid` that holds a lock on the version `tid` of the table `key` that conflicts with `strength`, or `None`.
fn row_holder(
    (locks, running): (&RowLocks, &BTreeSet<u32>),
    key: (u32, u32),
    tid: u64,
    (xid, strength): (u32, RowStrength),
) -> Option<u32> {
    if locks.is_empty() {
        return None;
    }
    locks
        .get(&(key.0, key.1, tid))?
        .iter()
        .find(|&&(locker, held, _)| {
            locker != xid && row_conflict(held, strength) && running.contains(&locker)
        })
        .map(|&(locker, _, _)| locker)
}

/// `CheckForSerializableConflictIn` of the transaction of the view in `SERIALIZABLE` for a write of the table `relation` that the targets `targets` cover, or of the whole table with no targets, with the lock of [`Heap::serializable_lock`].
fn conflict_in(
    ssi: Option<&mut ssi::Ssi>,
    view: &View,
    relation: u32,
    targets: Option<&[ssi::Target]>,
) -> Result<()> {
    let Some(ssi) = ssi else { return Ok(()) };
    let xmin = view.snapshot.xmin();
    ssi.write((view.sxact, view.xid), (view.database, relation), targets, xmin)
}

/// The targets that an insert into the table `table` checks: the table, for `heap_insert`, and the leaf pages of its indexes.
fn inserted(table: ssi::Table) -> [ssi::Target; 2] {
    [ssi::Target::Relation(table), ssi::Target::Index(table)]
}

/// `HeapDetermineColumnsInfo` for the key columns: true when the new values `new` change a column of a unique index of `unique` with no expression and no predicate, or a column of a constraint `WITHOUT OVERLAPS` of `periods`, which a foreign key can reference.
fn changes_key(unique: &[Unique], periods: &[usize], old: &[Value], new: &[Value]) -> bool {
    unique
        .iter()
        .filter(|index| index.derived.is_none())
        .flat_map(|index| &index.columns)
        .chain(periods)
        .any(|&column| old.get(column) != new.get(column))
}

/// The result of [`Heap::lock_row`].
#[derive(Debug, PartialEq)]
pub(crate) enum RowLocked {
    /// The transaction holds the lock.
    Done,
    /// The transaction `holder` is in progress and holds a lock that conflicts, or changed the version.
    Wait(u32),
    /// In `READ COMMITTED`, a transaction that committed after the snapshot changed the row. The statement must run again with a new snapshot.
    Again,
    /// The version went, or the transaction deleted it: the row is not locked.
    Gone,
}

/// A table that a transaction in progress changed, for the log of its commit.
#[derive(Debug)]
struct Touched {
    key: (u32, u32),
    /// The IDs of the versions that the transaction inserted or deleted, or `None` when the log reads all the versions of the table.
    tids: Option<Vec<u64>>,
}

impl State {
    /// The row locks of the transaction `xid` go at its end.
    fn unlock_rows(&mut self, xid: u32) {
        unqueue_rows(&mut self.row_queues, xid, None);
        if self.row_locks.is_empty() {
            return;
        }
        self.row_locks.retain(|_, lockers| {
            lockers.retain(|&(locker, _, _)| locker != xid);
            !lockers.is_empty()
        });
    }

    /// `GetOldestNonRemovableTransactionId`: a version that a transaction before the horizon deleted, when that transaction is not in progress, is dead to each snapshot that a session holds or takes later.
    fn horizon(&self) -> u32 {
        self.running
            .first()
            .copied()
            .into_iter()
            .chain(held(&self.xmins, None))
            .fold((self.latest_completed + 1).max(INITDB_NEXT_XID), u32::min)
    }

    /// Notes that the transaction `xid` changed the table `key`, for the log of its commit. The log then reads all the versions of the table.
    fn wrote(&mut self, xid: u32, key: (u32, u32)) {
        if let Some(touched) = self.touch(xid, key) {
            touched.tids = None;
        }
    }

    /// Notes that the transaction `xid` inserted or deleted the versions `tids` of the table `key`, for the log of its commit. A version that a rollback removes later stays in the list, and the log skips it.
    /// Keeps the table `key` in [`State::loading`] of the transaction `xid` when a load left open rows in its frozen columns, which the commit seals.
    fn loaded(&mut self, xid: u32, key: (u32, u32)) {
        let open = self.tables.meta(&key).and_then(|table| table.frozen.as_ref());
        if open.is_some_and(|frozen| frozen.columns.iter().any(|column| !column.sealed())) {
            let tables = self.loading.entry(xid).or_default();
            if !tables.contains(&key) {
                tables.push(key);
            }
        }
    }

    fn wrote_ids(&mut self, xid: u32, key: (u32, u32), tids: &[u64]) {
        if let Some(Touched { tids: Some(list), .. }) = self.touch(xid, key) {
            list.extend_from_slice(tids);
            // A long list takes much memory, and the log of a commit that changed more than `LOG_VERSIONS` versions is a checkpoint.
            if list.len() > LOG_VERSIONS {
                self.wrote(xid, key);
            }
        }
    }

    /// The entry of the table `key` in the changes of `xid`, which starts with no versions, when the commits write a log.
    fn touch(&mut self, xid: u32, key: (u32, u32)) -> Option<&mut Touched> {
        if !self.logging {
            return None;
        }
        let tables = self.touched.entry(xid).or_default();
        let at = match tables.iter().position(|touched| touched.key == key) {
            Some(at) => at,
            None => {
                tables.push(Touched { key, tids: Some(Vec::new()) });
                tables.len() - 1
            }
        };
        tables.get_mut(at)
    }
}

/// A new version with the key of a live version, or of a version of a transaction in progress, in a deferrable unique index, as `UNIQUE_CHECK_PARTIAL` finds it. The key is checked again after the statement or at the commit, as the event of `unique_key_recheck` does it.
#[derive(Clone, Debug)]
pub(crate) struct Recheck {
    xid: u32,
    /// The command that made the version.
    command: u32,
    database: u32,
    /// The OID of the table.
    pub(crate) table: u32,
    /// The index whose key the version has.
    pub(crate) unique: Unique,
    /// The ID of the version.
    pub(crate) tid: u64,
}

/// The rechecks of the new versions `partial` of the command of `view` in the table `relation`, by the place of the index in `unique` and the ID of the version.
fn rechecks<'a>(
    view: &'a View,
    relation: u32,
    unique: &'a [Unique],
    partial: Vec<(usize, u64)>,
) -> impl Iterator<Item = Recheck> + 'a {
    partial.into_iter().filter_map(move |(index, tid)| {
        Some(Recheck {
            xid: view.xid,
            command: view.command,
            database: view.database,
            table: relation,
            unique: unique.get(index)?.clone(),
            tid,
        })
    })
}

/// An edge of a cycle of waits: the process that waits, the transaction that it waits for, and the process of that transaction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) pid: i32,
    pub(crate) xid: u32,
    pub(crate) holder: i32,
}

/// The tables of [`State`]. A change of a table drops the columns of [`Table::columns`], so that each access that can change the versions goes through [`Tables::get_mut`], [`Tables::entry`], [`Tables::values_mut`] or [`Tables::retain`].
#[derive(Debug, Default)]
struct Tables(HashMap<(u32, u32), Table>);

impl Tables {
    /// The table `key` with the values of its versions, for a read by rows.
    fn get(&mut self, key: &(u32, u32)) -> Option<&Table> {
        let table = self.0.get_mut(key)?;
        table.thaw_read();
        Some(table)
    }

    /// The table `key` for a read of its pages and settings, or of the versions that a transaction in progress inserted or deleted, which a cold table does not have.
    fn meta(&self, key: &(u32, u32)) -> Option<&Table> {
        self.0.get(key)
    }

    fn get_mut(&mut self, key: &(u32, u32)) -> Option<&mut Table> {
        let table = self.0.get_mut(key)?;
        table.thaw();
        table.columns = Columns::default();
        Some(table)
    }

    /// The table `key`, which the call makes when it has no versions yet.
    fn entry(&mut self, key: (u32, u32)) -> &mut Table {
        let table = self.0.entry(key).or_default();
        table.thaw();
        table.columns = Columns::default();
        table
    }

    /// The table `key` for a change of its settings, which does not change its versions.
    fn settings(&mut self, key: (u32, u32)) -> &mut Table {
        self.0.entry(key).or_default()
    }

    /// The table `key` for a change that keeps its versions, its values and its columns, when the heap has it.
    fn settings_of(&mut self, key: &(u32, u32)) -> Option<&mut Table> {
        self.0.get_mut(key)
    }

    /// The table `key` for new versions at its end, which keeps the frozen values of its versions.
    fn append(&mut self, key: (u32, u32)) -> &mut Table {
        let table = self.0.entry(key).or_default();
        table.warm();
        table.kept = None;
        table.columns = Columns::default();
        table
    }

    /// The tables for which `touched` is true, for a change of their versions. The other tables keep their columns.
    fn touched_mut(
        &mut self,
        touched: impl Fn(&Table) -> bool,
    ) -> impl Iterator<Item = &mut Table> {
        // A cold table has only versions of transactions that committed and no deletes, which an end of a transaction does not change.
        self.0
            .values_mut()
            .filter(move |table| table.cold.is_none() && touched(table))
            .map(Table::touch)
    }

    /// Keeps the tables whose key `keep` accepts. A kept table keeps its versions, its frozen values and its columns, as the call only drops whole tables.
    fn retain(&mut self, mut keep: impl FnMut(&(u32, u32)) -> bool) {
        self.0.retain(|key, _| keep(key));
    }
}

/// The frozen columns that [`Heap::image`] wrote, by table, which [`Heap::adopt`] compares with the columns of the tables after the image, and the number of the changes that the log does not have at the image, of [`Heap::unlogged`].
#[derive(Debug, Default)]
pub(crate) struct Imaged(Vec<((u32, u32), Columned)>, u64);

impl Imaged {
    /// The number of [`Heap::unlogged`] at the image.
    pub(crate) fn unlogged(&self) -> u64 {
        self.1
    }
}

/// The frozen columns of a table, by place.
type Columned = Vec<Arc<Stored>>;

/// The values of versions of a table in encoded columns, which take the place of the values of the versions. A frozen version has no values. A change of the table, other than new versions, or a read by rows decodes the values back into the versions.
#[derive(Debug)]
struct Frozen {
    /// The number of frozen versions, which are the rows of the columns.
    rows: usize,
    /// The place after the last frozen version. The versions from it on keep their values, and so do the versions that went between the frozen ones after the freeze.
    end: usize,
    /// The column of each place, with the values of all the versions in their order, except the moved versions.
    columns: Vec<Arc<Stored>>,
    /// The versions whose rows are not in the order of the versions, each with its ID and its row, in the order of the IDs. A load puts a new version into an earlier page with free space, as `RelationGetBufferForTuple` does, and this version goes after the frozen rows, not between them, so that no column is made again. The other rows of the columns are in the order of their versions.
    moved: Vec<(u64, usize)>,
}

impl Frozen {
    /// Puts the values of `versions` on the rows of the columns. The new rows go on the open rows of each column with no copy of the column, and the caller gives no more versions than the rows left in the open segment, so that a column that does not keep its values only cuts its open rows to go back. The open rows take only a value of the type of the column, or a null, and keep it as it is, except a `real`, which they keep as a `double precision`, so only the new rows of a `real` column get a check by a decode. False, with the columns as they were, when a column does not keep the values.
    ///
    /// The values of each version are in one allocation. The columns take the versions in groups of [`APPEND_TILE`], so that the values of a group stay in the cache while each column reads its value of each version. The columns encode on the threads of [`on_runs`], as each column is apart from the others (spec/10 section 10.8).
    fn append(&mut self, versions: &[&Version], tasks: Option<&dyn Tasks>) -> bool {
        let rows = self.rows;
        let exact = on_runs(tasks, &mut self.columns, |first, run| {
            let kept = versions.chunks(APPEND_TILE).all(|tile| {
                run.iter_mut().zip(first..).all(|(column, at)| {
                    Arc::make_mut(column)
                        .extend(tile.iter().map(|version| version.values.get(at)))
                        .is_ok()
                })
            });
            kept && run.iter().zip(first..).all(|(column, at)| {
                column.kind != Kind::Float4 || {
                    let mut decoded = Decoded::new(column);
                    decoded.pass(rows);
                    let exact = versions.iter().all(|version| {
                        decoded.next().is_some_and(|value| same(&version.values[at], &value))
                    });
                    exact && decoded.next().is_none() && !decoded.failed
                }
            })
        });
        let exact = exact.into_iter().all(|kept| kept);
        if !exact {
            for column in &mut self.columns {
                // The new rows are open rows, so the cut does not decode.
                let _ = Arc::make_mut(column).truncate(rows);
            }
            return false;
        }
        if self.columns.iter().any(|column| column.rows != rows + versions.len()) {
            return false;
        }
        self.rows += versions.len();
        true
    }

    /// The frozen values of an image, which [`Heap::image`] wrote after the versions `versions` and `cold` cold versions. The frozen versions are the cold versions and the versions with no values, and each column has a row for each of them.
    fn take(input: &mut Reader<'_>, versions: &[Version], cold: usize) -> Result<Option<Frozen>> {
        let Some((rows, end, columns, moved)) =
            <Option<(usize, usize, Vec<Arc<Stored>>, Vec<(u64, usize)>)> as Persist>::take(input)?
        else {
            return Ok(None);
        };
        let empty = cold + versions.iter().filter(|version| version.values.is_empty()).count();
        let last = versions
            .iter()
            .rposition(|version| version.values.is_empty())
            .map_or(cold, |at| cold + at + 1);
        if empty != rows
            || end != last
            || columns.is_empty()
            || columns.iter().any(|column| column.rows != rows)
            || !moved.is_sorted_by(|a, b| a.0 < b.0)
            || moved.iter().any(|&(_, row)| row >= rows)
        {
            return Err(rupg_common::persist::corrupt(
                "the frozen columns do not match the versions",
            ));
        }
        let mut places: Vec<usize> = moved.iter().map(|&(_, row)| row).collect();
        places.sort_unstable();
        places.dedup();
        if places.len() != moved.len() {
            return Err(rupg_common::persist::corrupt("two moved versions have the same row"));
        }
        Ok(Some(Frozen { rows, end, columns, moved }))
    }
}

/// The headers of the versions of a frozen table in encoded columns, which take the place of the versions (spec/10 section 10.4.1). Each version of a cold table was inserted by a transaction that committed, and no transaction deleted it, so a version needs only its ID, its place in the order of the inserts, `xmin`, `cmin` and its length. A change of the table or a read by rows makes the versions again.
#[derive(Debug)]
struct Cold {
    rows: usize,
    /// The IDs, the places, `xmin`, `cmin` and the lengths of the versions, a vector of [`VECTOR`] versions at a time, each with the integer cascade.
    vectors: Vec<[Box<[u8]>; 5]>,
    /// Each transaction that inserted versions, with the last command that did.
    xmins: Vec<(u32, u32)>,
}

impl Cold {
    /// The headers of `versions`, or `None` when a version has values, was deleted, or was deleted by a transaction that aborted.
    fn of(versions: &[Version]) -> Option<Cold> {
        let settled = |v: &Version| {
            v.values.is_empty() && v.xmax == 0 && v.cmax == 0 && v.aborted == 0 && v.next == 0
        };
        if versions.is_empty() || !versions.iter().all(settled) {
            return None;
        }
        let chooser = Sampled::new();
        let mut xmins: Vec<(u32, u32)> = Vec::new();
        let mut vectors = Vec::with_capacity(versions.len().div_ceil(VECTOR));
        let mut fields: [Vec<i64>; 5] = Default::default();
        for chunk in versions.chunks(VECTOR) {
            for field in &mut fields {
                field.clear();
            }
            for v in chunk {
                // A version ID and a place fit in 63 bits, so the cast keeps them.
                fields[0].push(v.tid as i64);
                fields[1].push(v.made as i64);
                fields[2].push(i64::from(v.xmin));
                fields[3].push(i64::from(v.cmin));
                fields[4].push(i64::from(v.len));
                match xmins.iter_mut().find(|(xmin, _)| *xmin == v.xmin) {
                    Some((_, cmin)) => *cmin = (*cmin).max(v.cmin),
                    None => xmins.push((v.xmin, v.cmin)),
                }
            }
            let mut encoded = fields
                .iter()
                .map(|field| integer::encode_with(field, &chooser).ok().map(Vec::into_boxed_slice));
            vectors.push(std::array::from_fn(|_| encoded.next().flatten().unwrap_or_default()));
            if vectors
                .last()
                .is_some_and(|vector: &[Box<[u8]>; 5]| vector.iter().any(|f| f.is_empty()))
            {
                return None;
            }
        }
        Some(Cold { rows: versions.len(), vectors, xmins })
    }

    /// Checks that each vector of headers decodes and that the vectors have all the rows, with the versions of one vector in memory at a time.
    fn check(&self) -> Result<()> {
        let mut rows = 0;
        for vector in &self.vectors {
            rows += Cold::vector(vector)?.len();
        }
        if rows != self.rows {
            return Err(rupg_common::persist::corrupt("the cold headers do not have their rows"));
        }
        Ok(())
    }

    /// The versions of the headers, with no values.
    fn versions(&self) -> Result<Vec<Version>> {
        let mut versions = Vec::with_capacity(self.rows);
        for vector in &self.vectors {
            versions.append(&mut Cold::vector(vector)?);
        }
        if versions.len() != self.rows {
            return Err(rupg_common::persist::corrupt("the cold headers do not have their rows"));
        }
        Ok(versions)
    }

    /// The versions of one vector of headers.
    fn vector(vector: &[Box<[u8]>; 5]) -> Result<Vec<Version>> {
        let [tids, made, xmins, cmins, lens] =
            vector.each_ref().map(|bytes| integer::decode(bytes));
        let (tids, made, xmins, cmins, lens) = (tids?, made?, xmins?, cmins?, lens?);
        if [made.len(), xmins.len(), cmins.len(), lens.len()].iter().any(|&n| n != tids.len()) {
            return Err(rupg_common::persist::corrupt("the cold headers have different lengths"));
        }
        Ok((0..tids.len())
            .map(|at| Version {
                tid: tids[at] as u64,
                made: made[at] as u64,
                xmin: xmins[at] as u32,
                cmin: cmins[at] as u32,
                xmax: 0,
                cmax: 0,
                aborted: 0,
                next: 0,
                key_kept: false,
                len: lens[at] as u16,
                values: Arc::default(),
            })
            .collect())
    }
}

/// The bytes of a vector of cold headers.
fn put_cold_vector(vector: &[Box<[u8]>; 5], out: &mut Vec<u8>) {
    for field in vector {
        put_count(out, field.len());
        out.extend_from_slice(field);
    }
}

impl Persist for Cold {
    fn put(&self, out: &mut Vec<u8>) {
        self.rows.put(out);
        put_count(out, self.vectors.len());
        for vector in &self.vectors {
            put_cold_vector(vector, out);
        }
        self.xmins.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Cold> {
        let rows = Persist::take(input)?;
        let mut vectors = Vec::new();
        for _ in 0..input.count()? {
            let mut field = || -> Result<Box<[u8]>> {
                let n = input.count()?;
                Ok(input.bytes(n)?.into())
            };
            vectors.push([field()?, field()?, field()?, field()?, field()?]);
        }
        let cold = Cold { rows, vectors, xmins: Persist::take(input)? };
        // A decode checks the bytes, so a bad image does not load. The check decodes one vector at a time, so that it does not hold the versions of all the rows.
        cold.check()?;
        Ok(cold)
    }
}

/// The physical type that keeps each value of `values` exactly, or `None` when a value has no such type or the values do not have the same type. A column of nulls only is a column of `bigint`.
fn kind_of<'a>(values: impl Iterator<Item = &'a Value>) -> Option<Kind> {
    let mut kind = None;
    for value in values {
        let this = match value {
            Value::Null => continue,
            Value::Bool(_) => Kind::Bool,
            Value::Int2(_) => Kind::Int2,
            Value::Int4(_) => Kind::Int4,
            Value::Int8(_) => Kind::Int8,
            Value::Float4(_) => Kind::Float4,
            Value::Float8(_) => Kind::Float8,
            Value::Date(_) => Kind::Date,
            Value::Time(_) => Kind::Time,
            Value::Timestamp(_) => Kind::Timestamp,
            Value::TimestampTz(_) => Kind::TimestampTz,
            Value::Text(_) => Kind::Text,
            Value::Bytea(_) => Kind::Bytea,
            // The integer of a value of a display scale up to 18 can fit in a `bigint`, and a value that does not fit makes the column fail.
            Value::Numeric(v) => match u8::try_from(v.dscale()) {
                Ok(scale) if scale <= 18 => Kind::Numeric(scale),
                _ => return None,
            },
            _ => return None,
        };
        if *kind.get_or_insert(this) != this {
            return None;
        }
    }
    Some(kind.unwrap_or(Kind::Int8))
}

/// True when `a` and `b` are the same value, with the same bits for a float.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float4(x), Value::Float4(y)) => x.to_bits() == y.to_bits(),
        (Value::Float8(x), Value::Float8(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// The values of a column, in the order of the rows, decoded a vector at a time.
struct Decoded<'a> {
    stored: &'a Stored,
    segment: usize,
    vector: usize,
    /// The values of the vector that are left, last first.
    values: Vec<Value>,
    /// True when a vector did not decode, which ends the values.
    failed: bool,
}

impl<'a> Decoded<'a> {
    fn new(stored: &'a Stored) -> Self {
        Decoded { stored, segment: 0, vector: 0, values: Vec::new(), failed: false }
    }

    /// Skips the first `rows` values, a whole vector at a time when it can.
    fn pass(&mut self, mut rows: usize) {
        while self.values.is_empty() {
            let size = match self.stored.segments.get(self.segment) {
                Some(column) => match column.vectors.get(self.vector) {
                    Some(vector) => vector.rows as usize,
                    None => {
                        self.segment += 1;
                        self.vector = 0;
                        continue;
                    }
                },
                // The open rows of the column come after the segments, in vectors of VECTOR rows.
                None => VECTOR.min(self.stored.open_rows().saturating_sub(self.vector * VECTOR)),
            };
            if size == 0 || size > rows {
                break;
            }
            rows -= size;
            self.vector += 1;
        }
        for _ in 0..rows {
            self.next();
        }
    }
}

impl Iterator for Decoded<'_> {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        while self.values.is_empty() && !self.failed {
            let Some(column) = self.stored.segments.get(self.segment) else {
                match self.stored.open_values(self.vector, &mut self.values) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(_) => {
                        self.failed = true;
                        return None;
                    }
                }
                self.values.reverse();
                self.vector += 1;
                continue;
            };
            if self.vector >= column.vectors.len() {
                self.segment += 1;
                self.vector = 0;
                continue;
            }
            if column.values(self.vector, self.stored.dict.as_ref(), &mut self.values).is_err() {
                self.failed = true;
            }
            self.values.reverse();
            self.vector += 1;
        }
        self.values.pop()
    }
}

/// The row of the frozen columns of each frozen version, in the order of the versions.
struct Places<'a> {
    /// The moved versions that are left, in the order of their IDs.
    moved: &'a [(u64, usize)],
    /// The rows of the moved versions, in order, which the rows in the order of the versions skip.
    skipped: Vec<usize>,
    skip: usize,
    /// The next row in the order of the versions.
    next: usize,
}

impl<'a> Places<'a> {
    fn new(frozen: &'a Frozen) -> Self {
        let mut skipped: Vec<usize> = frozen.moved.iter().map(|&(_, row)| row).collect();
        skipped.sort_unstable();
        Places { moved: &frozen.moved, skipped, skip: 0, next: 0 }
    }

    /// The row of the next frozen version, whose ID is `tid`, and true when the version moved. The caller gives each frozen version in order.
    fn row(&mut self, tid: u64) -> (usize, bool) {
        if let Some((&(first, row), rest)) = self.moved.split_first()
            && first == tid
        {
            self.moved = rest;
            return (row, true);
        }
        while self.skipped.get(self.skip) == Some(&self.next) {
            self.skip += 1;
            self.next += 1;
        }
        self.next += 1;
        (self.next - 1, false)
    }
}

/// A decoder for each column of `columns` that `used` marks.
fn decoders<'a>(columns: &'a [Arc<Stored>], used: &[bool]) -> Vec<Option<Decoded<'a>>> {
    columns.iter().zip(used).map(|(stored, &used)| used.then(|| Decoded::new(stored))).collect()
}

/// The values of the frozen versions of a table, in the order of the versions, with the columns decoded a vector at a time. The rows in the order of the versions and the moved rows each have their decoders, so that each decoder goes forward.
struct FrozenRows<'a> {
    columns: &'a [Arc<Stored>],
    /// True for each column to decode.
    used: Vec<bool>,
    places: Places<'a>,
    /// The decoders of the rows in the order of the versions, and the row that they give next.
    main: Vec<Option<Decoded<'a>>>,
    main_at: usize,
    /// The decoders of the moved rows, and the row that they give next.
    side: Vec<Option<Decoded<'a>>>,
    side_at: usize,
}

impl<'a> FrozenRows<'a> {
    /// The rows of `frozen`, which decode only the columns at `used`, or all the columns for `None`.
    fn new(frozen: &'a Frozen, used: Option<&[usize]>) -> Self {
        let used: Vec<bool> = (0..frozen.columns.len())
            .map(|at| used.is_none_or(|used| used.contains(&at)))
            .collect();
        let mut rows = FrozenRows {
            columns: &frozen.columns,
            used,
            places: Places::new(frozen),
            main: Vec::new(),
            main_at: 0,
            side: Vec::new(),
            side_at: 0,
        };
        rows.main = decoders(rows.columns, &rows.used);
        rows
    }

    /// Puts into `row` the values of the next frozen version, whose ID is `tid`, with null for a column that the rows do not decode.
    fn next(&mut self, tid: u64, row: &mut Vec<Value>) {
        let (at, moved) = self.places.row(tid);
        let (decoders, next) = if moved {
            (&mut self.side, &mut self.side_at)
        } else {
            (&mut self.main, &mut self.main_at)
        };
        if decoders.is_empty() || at < *next {
            *decoders = self::decoders(self.columns, &self.used);
            *next = 0;
        }
        let skip = at - *next;
        *next = at + 1;
        row.clear();
        row.extend(decoders.iter_mut().map(|decoded| {
            decoded
                .as_mut()
                .and_then(|decoded| {
                    decoded.pass(skip);
                    decoded.next()
                })
                .unwrap_or(Value::Null)
        }));
    }

    /// Passes the next frozen version, whose ID is `tid`, with no decode. The next row that [`FrozenRows::next`] gives passes its vectors.
    fn skip(&mut self, tid: u64) {
        self.places.row(tid);
    }

    /// True when a column did not decode.
    fn failed(&self) -> bool {
        self.main.iter().chain(&self.side).flatten().any(|decoded| decoded.failed)
    }
}

/// The columns of a table that the scans read, which the heap makes from the versions when every version is live. A change of the table drops them.
#[derive(Debug, Default)]
struct Columns {
    /// Each transaction that inserted versions of the table with the last command that did, or `None` before the heap looked at the versions. An empty list with `mixed` set is a table with a deleted version.
    xmins: Option<Vec<(u32, u32)>>,
    /// True when a version of the table was deleted, also by a transaction that did not commit yet.
    mixed: bool,
    /// The columns, by the place of the column, with the physical type of each.
    stored: HashMap<usize, Arc<Stored>>,
}

/// The versions of the rows of a table, in the order of their IDs.
#[derive(Debug, Default)]
struct Table {
    versions: Vec<Version>,
    /// The number of versions that the table got, which gives each version its place in the order of the inserts.
    made: u64,
    /// The map of each unique key that a write checked. A map has the IDs of all the versions, live or not, with a key that has no null.
    keys: Vec<KeyMap>,
    pages: Pages,
    /// The transaction and the command of the statement that fills the table with a `BulkInsertState`, as `CREATE TABLE AS` does.
    bulk: Option<(u32, u32)>,
    /// The pages of the heap before each `TRUNCATE` of the transaction in progress, with the transaction and the command, which a rollback brings back, as `RelationSetNewRelfilenumber` gives the table a new file.
    truncated: Vec<(u32, u32, Pages)>,
    /// The rows of `pg_statistic` of the table, by `staattnum` and `stainherit`, which `ANALYZE` writes outside of the transactions.
    statistics: BTreeMap<(i16, bool), Statistic>,
    /// The rows of [`Table::statistics`] with `stainherit` false by `staattnum`, which [`Heap::own_statistics`] makes once for all the plans.
    own_statistics: Option<Arc<BTreeMap<i16, Statistic>>>,
    /// The size of each `btree` index of the table by the OID of the index, from the versions that the table had.
    btree_sizes: BTreeMap<u32, btree::Cached>,
    /// The pages with `LP_DEAD` items that a `VACUUM` with no vacuum of the indexes left, with the generation of the heap.
    lp_dead: BTreeSet<(u16, u32)>,
    /// The storage of each column with a type of fixed length, which the new versions take in their pages.
    fixed: Vec<Fixed>,
    /// `attstorage` of each column, with the method that compresses its values.
    storage: Vec<Storage>,
    /// The bytes of each page that `fillfactor` keeps free for updates.
    save_free: usize,
    /// The combo command ID of each version that the transaction that inserted it deleted, by the ID of the version, which `t_cid` keeps after the end of the transaction.
    combos: HashMap<u64, u32>,
    columns: Columns,
    /// The values of the versions in encoded columns, when the versions have no values.
    frozen: Option<Frozen>,
    /// The headers of all the versions in encoded columns, when `versions` is empty.
    cold: Option<Cold>,
    /// The number of versions of the last freeze that did not make the columns, or 0. The next freeze waits until the table has twice as many versions, so that a table with a column that has no encoding, such as a `uuid` column, does not encode all its other columns again at each load and each scan.
    unfrozen: usize,
    /// The frozen columns that a read thawed, which the next freeze takes back when the versions did not change.
    kept: Option<Kept>,
}

/// The frozen columns of a table that a read by rows or by key thawed. A write drops them. When the table has the same versions at the next freeze, the freeze takes the columns back and does not make them again.
#[derive(Debug)]
struct Kept {
    frozen: Frozen,
    /// The places of the versions whose values were in the columns.
    places: Vec<usize>,
    /// [`Table::made`] and the number of versions at the thaw. The inserts change the first and the removals change the second, and each removal goes through a thaw for a write, which drops the columns.
    made: u64,
    len: usize,
}

/// The IDs of the versions of one hash of a [`KeyMap`]. Most keys have one version, so one ID stays in the map and only more than one ID needs a list on the heap. This keeps an allocation and a miss of the cache from each read of a key.
#[derive(Debug)]
enum Tids {
    One(u64),
    Many(Vec<u64>),
}

impl Tids {
    fn as_slice(&self) -> &[u64] {
        match self {
            Tids::One(tid) => std::slice::from_ref(tid),
            Tids::Many(tids) => tids,
        }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    fn push(&mut self, tid: u64) {
        match self {
            Tids::One(first) => *self = Tids::Many(vec![*first, tid]),
            Tids::Many(tids) if tids.is_empty() => *self = Tids::One(tid),
            Tids::Many(tids) => tids.push(tid),
        }
    }

    /// Keeps the IDs for which `keep` is true. One ID that is left goes back into the map from the list.
    fn retain(&mut self, mut keep: impl FnMut(&u64) -> bool) {
        match self {
            Tids::One(tid) => {
                if !keep(tid) {
                    *self = Tids::Many(Vec::new());
                }
            }
            Tids::Many(tids) => {
                tids.retain(|tid| keep(tid));
                if let [tid] = tids[..] {
                    *self = Tids::One(tid);
                }
            }
        }
    }
}

impl<'t> IntoIterator for &'t Tids {
    type Item = &'t u64;
    type IntoIter = std::slice::Iter<'t, u64>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

/// The versions of a table by the hash of their key, for one unique key, which takes the place of the btree of a unique index. The map can keep the ID of a version that went, which a check skips.
#[derive(Debug)]
struct KeyMap {
    columns: Vec<usize>,
    types: Vec<u32>,
    nulls_not_distinct: bool,
    /// The expressions and the predicate of the index. `columns` are then the places in the values of the expressions.
    derived: Option<Box<Derived>>,
    hashes: HashMap<u64, Tids>,
    /// The key of each version in an index with an expression or a predicate, by the ID of the version. The expressions ran before the heap was locked.
    derived_keys: HashMap<u64, Box<[Value]>>,
    /// The map of an index with an expression did not get the key of a new version, because the write did not have the index. A later write with the index makes the map again.
    stale: bool,
}

impl KeyMap {
    fn of(key: &Unique) -> Self {
        KeyMap {
            columns: key.columns.clone(),
            types: key.types.clone(),
            nulls_not_distinct: key.nulls_not_distinct,
            derived: key.derived.clone(),
            hashes: HashMap::new(),
            derived_keys: HashMap::new(),
            stale: false,
        }
    }

    fn is(&self, key: &Unique) -> bool {
        !self.stale
            && self.columns == key.columns
            && self.types == key.types
            && self.nulls_not_distinct == key.nulls_not_distinct
            && self.derived == key.derived
    }

    /// The values that the key columns are places in for the version `tid` with the values `values`: the row itself, or the values of the expressions of the index. `None` is a row that is not in the index.
    fn key<'v>(&'v self, tid: u64, values: &'v [Value]) -> Option<&'v [Value]> {
        match &self.derived {
            None => Some(values),
            Some(_) => self.derived_keys.get(&tid).map(|key| &**key),
        }
    }

    /// Adds the version `tid` with the values `values` to the map. For an index with an expression or a predicate, `derived` is the key of the version, or `None` when the index does not have the version.
    fn add(&mut self, tid: u64, values: &[Value], derived: DerivedKey) {
        let hash = if self.derived.is_none() {
            self.hash(values)
        } else if let Some(key) = derived {
            let hash = self.hash(&key);
            self.derived_keys.insert(tid, key);
            hash
        } else {
            self.derived_keys.remove(&tid);
            None
        };
        if let Some(hash) = hash {
            match self.hashes.entry(hash) {
                Entry::Occupied(tids) => tids.into_mut().push(tid),
                Entry::Vacant(place) => {
                    place.insert(Tids::One(tid));
                }
            }
        }
    }

    /// The map of an index with an expression stops, as it misses the key of a new version.
    fn make_stale(&mut self) {
        self.stale = true;
        self.hashes = HashMap::new();
        self.derived_keys = HashMap::new();
    }

    /// The hash of the key of a row, or `None` when the key has a null and two nulls are not equal, so the key cannot conflict. Two keys that are equal have the same hash.
    fn hash(&self, values: &[Value]) -> Option<u64> {
        let mut hasher = DefaultHasher::new();
        for (&column, &ty) in self.columns.iter().zip(&self.types) {
            match values.get(column)? {
                Value::Null if self.nulls_not_distinct => 0_u8.hash(&mut hasher),
                Value::Null => return None,
                value => hash_value(value, ty, &mut hasher),
            }
        }
        Some(hasher.finish())
    }

    /// [`KeyMap::hash`] of a key whose values come in the order of the key columns, with no row around them.
    fn hash_key(&self, key: &[Value]) -> Option<u64> {
        if key.len() != self.columns.len() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        for (value, &ty) in key.iter().zip(&self.types) {
            match value {
                Value::Null if self.nulls_not_distinct => 0_u8.hash(&mut hasher),
                Value::Null => return None,
                value => hash_value(value, ty, &mut hasher),
            }
        }
        Some(hasher.finish())
    }

    /// True when two rows have the same key, with the equality of the btree operator class of each column.
    fn same(&self, a: &[Value], b: &[Value]) -> Result<bool> {
        for (&column, &ty) in self.columns.iter().zip(&self.types) {
            let equal = match (&a[column], &b[column]) {
                (Value::Null, Value::Null) => self.nulls_not_distinct,
                (Value::Null, _) | (_, Value::Null) => false,
                (x, y) => rupg_func::equal(ty, x, ty, y)?,
            };
            if !equal {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Adds a value that is not null to a hash. The types whose equality is not the equality of their form in memory all go to the same hash.
pub(crate) fn hash_value(value: &Value, ty: u32, hasher: &mut DefaultHasher) {
    match value {
        Value::Bool(v) => v.hash(hasher),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => value.as_i64().hash(hasher),
        Value::Float4(_) | Value::Float8(_) => {
            let x = match value {
                Value::Float4(x) => f64::from(*x),
                Value::Float8(x) => *x,
                _ => 0.0,
            };
            // `float8_cmp_internal`: -0 is equal to 0, and NaN is equal to NaN.
            let x = if x.is_nan() {
                f64::NAN
            } else if x == 0.0 {
                0.0
            } else {
                x
            };
            x.to_bits().hash(hasher);
        }
        Value::Oid(v) => v.hash(hasher),
        Value::Char(v) => v.hash(hasher),
        Value::Text(v) if rupg_func::base_type(ty) == oid::BPCHAR => {
            v.trim_end_matches(' ').hash(hasher);
        }
        Value::Text(v) => v.hash(hasher),
        Value::Bytea(v) => v.hash(hasher),
        Value::Uuid(v) => v.hash(hasher),
        Value::Date(v) => v.hash(hasher),
        Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v) => v.hash(hasher),
        _ => {}
    }
}

/// What a dirty snapshot sees of a version with the same key, as `_bt_check_unique` reads it with `SnapshotDirty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// The version went, or the transaction deleted it.
    Dead,
    /// The version is live: the key is a duplicate.
    Live,
    /// A transaction that is in progress inserted or deleted the version.
    Wait(u32),
}

/// `HeapTupleSatisfiesDirty` for the transaction `xid`. An aborted transaction left no versions, so each `xmin` committed or is in progress.
fn dirty(version: &Version, xid: u32, running: &BTreeSet<u32>) -> Seen {
    if version.xmin != xid && running.contains(&version.xmin) {
        return Seen::Wait(version.xmin);
    }
    match version.xmax {
        0 => Seen::Live,
        xmax if xmax == xid => Seen::Dead,
        xmax if running.contains(&xmax) => Seen::Wait(xmax),
        _ => Seen::Dead,
    }
}

impl Table {
    /// Moves the values of all the versions into the encoded columns, when each column keeps its values exactly. When new versions went between the frozen ones, the heap makes each column again from all the versions, one column at a time.
    fn freeze(&mut self, tasks: Option<&dyn Tasks>) {
        self.take_kept();
        if self.fold(tasks) {
            self.freeze_to(self.versions.len(), tasks);
        } else {
            self.refreeze();
        }
    }

    /// Moves the values of the versions that went between the frozen ones after the freeze into new rows after the frozen rows, and keeps the row of each in [`Frozen::moved`], so that no column is made again. False when a version stays between the frozen ones with its values, as a value does not have the type of its column.
    fn fold(&mut self, tasks: Option<&dyn Tasks>) -> bool {
        let Some(frozen) = &mut self.frozen else { return true };
        let Some(between) = self.versions.get(..frozen.end) else {
            return frozen.rows == frozen.end;
        };
        let places: Vec<usize> =
            (0..between.len()).filter(|&at| !between[at].values.is_empty()).collect();
        if places.is_empty() {
            return true;
        }
        let width = frozen.columns.len();
        let moved: Vec<&Version> = places.iter().map(|&at| &between[at]).collect();
        if moved.iter().any(|version| version.values.len() != width) {
            return false;
        }
        let mut done = 0;
        while done < moved.len() {
            let to = moved.len().min(done + ROWS - frozen.rows % ROWS);
            let first = frozen.rows;
            if !frozen.append(&moved[done..to], tasks) {
                break;
            }
            frozen.moved.extend(
                moved[done..to].iter().zip(first..).map(|(version, row)| (version.tid, row)),
            );
            done = to;
        }
        frozen.moved.sort_unstable_by_key(|&(tid, _)| tid);
        for &at in &places[..done] {
            self.versions[at].values = Arc::default();
        }
        done == places.len()
    }

    /// [`Table::freeze`] in a load, which keeps the values of the versions in the last pages, as a new version can still go into them. It waits for [`LOAD_ROWS`] new versions after the frozen ones.
    fn freeze_load(&mut self, tasks: Option<&dyn Tasks>) {
        let start = self.frozen.as_ref().map_or(0, |frozen| frozen.end);
        let Some(last) = self.versions.last() else { return };
        let cut = page_of(last.tid).saturating_sub(LOAD_PAGES);
        let cut = pages::tid(generation_of(last.tid), cut, 0);
        let end = self.versions.partition_point(|version| version.tid < cut);
        if end >= start + LOAD_ROWS {
            self.freeze_to(end, tasks);
        }
    }

    /// Moves the values of the versions after the last frozen one and before the place `end` into the encoded columns. The heap checks the new rows of each column with a decode before it drops the values. When the versions have different lengths, or a value has no column type or not the type of its column, the table stays as it is.
    fn freeze_to(&mut self, end: usize, tasks: Option<&dyn Tasks>) {
        let start = self.frozen.as_ref().map_or(0, |frozen| frozen.end);
        let Some(tail) = self.versions.get(start..end) else { return };
        let Some(width) = self
            .frozen
            .as_ref()
            .map(|frozen| frozen.columns.len())
            .or_else(|| tail.first().map(|version| version.values.len()))
        else {
            return;
        };
        if width == 0 || tail.is_empty() || tail.iter().any(|version| version.values.len() != width)
        {
            return;
        }
        if let Some(frozen) = &mut self.frozen {
            let tail: Vec<&Version> = tail.iter().collect();
            let mut from = start;
            while from < end {
                let to = end.min(from + ROWS - frozen.rows % ROWS);
                if !frozen.append(&tail[from - start..to - start], tasks) {
                    break;
                }
                frozen.end = to;
                from = to;
            }
            for version in &mut self.versions[start..from] {
                version.values = Arc::default();
            }
            return;
        }
        if self.unfrozen != 0 && (self.unfrozen..self.unfrozen.saturating_mul(2)).contains(&end) {
            return;
        }
        let Some(columns) = self.first_columns(tail, width) else {
            self.unfrozen = end;
            return;
        };
        self.unfrozen = 0;
        for version in &mut self.versions[start..end] {
            version.values = Arc::default();
        }
        self.frozen = Some(Frozen { rows: end - start, end, columns, moved: Vec::new() });
    }

    /// The encoded columns of the first frozen rows `tail`, each with `width` values, or `None` when a value has no column type, not the type of its column, or does not come back the same from a decode.
    fn first_columns(&self, tail: &[Version], width: usize) -> Option<Vec<Arc<Stored>>> {
        // A column with no type of its own stops the freeze before any column is encoded.
        if (0..width).any(|at| kind_of(tail.iter().map(|version| &version.values[at])).is_none()) {
            return None;
        }
        let mut columns = Vec::with_capacity(width);
        for at in 0..width {
            let kind = kind_of(tail.iter().map(|version| &version.values[at]))?;
            let values = tail.iter().map(|version| version.values.get(at));
            let stored = match self.columns.stored.get(&at) {
                Some(stored) if stored.kind == kind => Stored::clone(stored),
                _ => {
                    let mut stored = Stored::new(kind, None, Vec::new(), 0);
                    if stored.extend(values).is_err() {
                        return None;
                    }
                    stored
                }
            };
            let mut decoded = Decoded::new(&stored);
            let exact = tail.iter().all(|version| {
                decoded.next().is_some_and(|value| same(&version.values[at], &value))
            });
            if !exact || decoded.next().is_some() || decoded.failed {
                return None;
            }
            columns.push(Arc::new(stored));
        }
        Some(columns)
    }

    /// The values of the column `at` of each version, in order, with the values of the frozen versions decoded from the column.
    fn column_values(&self, at: usize) -> Vec<Value> {
        let mut rows = self.frozen.as_ref().map(|frozen| FrozenRows::new(frozen, Some(&[at])));
        let mut row = Vec::new();
        self.versions
            .iter()
            .map(|version| match &mut rows {
                Some(rows) if version.values.is_empty() => {
                    rows.next(version.tid, &mut row);
                    std::mem::replace(&mut row[at], Value::Null)
                }
                _ => version.values.get(at).cloned().unwrap_or(Value::Null),
            })
            .collect()
    }

    /// Makes each column of a frozen table again from the values of all the versions, when new versions went between the frozen ones. The heap checks each new column with a decode, and the table stays as it is when a column does not keep its values.
    fn refreeze(&mut self) {
        let Some(frozen) = &self.frozen else { return };
        let width = frozen.columns.len();
        if self
            .versions
            .iter()
            .any(|version| !version.values.is_empty() && version.values.len() != width)
        {
            return;
        }
        let mut columns = Vec::with_capacity(width);
        for at in 0..width {
            let values = self.column_values(at);
            let Some(kind) = kind_of(values.iter()) else { return };
            let kind =
                if values.iter().all(Value::is_null) { frozen.columns[at].kind } else { kind };
            let Ok(stored) = Stored::build(kind, values.iter().map(Some)) else { return };
            let mut decoded = Decoded::new(&stored);
            let exact =
                values.iter().all(|value| decoded.next().is_some_and(|got| same(value, &got)));
            if !exact || decoded.next().is_some() || decoded.failed {
                return;
            }
            columns.push(Arc::new(stored));
        }
        for version in &mut self.versions {
            version.values = Arc::default();
        }
        let rows = self.versions.len();
        self.frozen = Some(Frozen { rows, end: rows, columns, moved: Vec::new() });
    }

    /// Gives `each` the place and the values of each version of the table, in order, with the values of the frozen versions decoded from their columns.
    fn each_row(&self, each: impl FnMut(usize, &[Value])) {
        self.each_row_of(None, each);
    }

    /// [`Table::each_row`], which decodes only the columns at `used` of a frozen version, and gives null for its other columns.
    fn each_row_of(&self, used: Option<&[usize]>, each: impl FnMut(usize, &[Value])) {
        self.each_kept_row(used, |_| true, each);
    }

    /// [`Table::each_row_of`] for the versions that `keep` keeps. The other frozen versions do not decode.
    fn each_kept_row(
        &self,
        used: Option<&[usize]>,
        keep: impl Fn(&Version) -> bool,
        mut each: impl FnMut(usize, &[Value]),
    ) {
        let Some(frozen) = &self.frozen else {
            for (at, version) in self.versions.iter().enumerate() {
                if keep(version) {
                    each(at, &version.values);
                }
            }
            return;
        };
        let mut rows = FrozenRows::new(frozen, used);
        let mut row = Vec::with_capacity(frozen.columns.len());
        for (at, version) in self.versions.iter().enumerate() {
            if !keep(version) {
                if version.values.is_empty() {
                    rows.skip(version.tid);
                }
                continue;
            }
            if version.values.is_empty() {
                rows.next(version.tid, &mut row);
                each(at, &row);
            } else {
                each(at, &version.values);
            }
        }
        // The freeze decoded each column to the same values, so the decode does not fail.
        debug_assert!(!rows.failed(), "a frozen column does not decode");
    }

    /// Gives `each` each version of the table with its values, as [`Table::each_row_of`] does, also for a cold table, whose headers decode a vector at a time.
    fn each_version_row(&self, used: Option<&[usize]>, mut each: impl FnMut(&Version, &[Value])) {
        let (Some(cold), Some(frozen)) = (&self.cold, &self.frozen) else {
            self.each_row_of(used, |at, values| each(&self.versions[at], values));
            return;
        };
        let mut rows = FrozenRows::new(frozen, used);
        let mut row = Vec::with_capacity(frozen.columns.len());
        for vector in &cold.vectors {
            // The headers decoded when they were made or read, so the decode does not fail.
            for version in Cold::vector(vector).unwrap_or_default() {
                rows.next(version.tid, &mut row);
                each(&version, &row);
            }
        }
    }

    /// The values of the versions at the places `places` in the order of [`Table::each_version`], which go up, with only the columns at `used`, in their order, or all the columns for `None`. The frozen columns decode only the vectors with these rows and make only their values, so that a sample of a long table does not make a value for each row. `None` is a column that does not decode.
    fn rows_at(&self, places: &[usize], used: Option<&[usize]>) -> Option<Vec<Vec<Value>>> {
        let pick = |values: &[Value]| -> Vec<Value> {
            match used {
                Some(used) => {
                    used.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)).collect()
                }
                None => values.to_vec(),
            }
        };
        let Some(frozen) = &self.frozen else {
            return Some(places.iter().map(|&at| pick(&self.versions[at].values)).collect());
        };
        // For each place, the row of the frozen columns, or the version with its values.
        let slots: Vec<Result<usize, usize>> = if self.cold.is_some() && frozen.moved.is_empty() {
            places.iter().map(|&place| Ok(place)).collect()
        } else {
            let mut slots = Vec::with_capacity(places.len());
            let mut next = places.iter().copied().peekable();
            let mut rows = Places::new(frozen);
            let mut at = 0;
            self.each_version(|version| {
                let row = version.values.is_empty().then(|| rows.row(version.tid).0);
                if next.peek() == Some(&at) {
                    slots.push(row.ok_or(at));
                    next.next();
                }
                at += 1;
            });
            slots
        };
        // A moved version has a row after the rows of the versions after it, so the rows go up only once they are in order.
        let mut wanted: Vec<usize> = slots.iter().filter_map(|slot| slot.ok()).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let all: Vec<usize>;
        let used_columns = match used {
            Some(used) => used,
            None => {
                all = (0..frozen.columns.len()).collect();
                &all
            }
        };
        let mut columns = Vec::with_capacity(used_columns.len());
        for &column in used_columns {
            let mut values = Vec::with_capacity(wanted.len());
            match frozen.columns.get(column) {
                Some(stored) => stored.values_at(&wanted, &mut values).ok()?,
                None => values.resize(wanted.len(), Value::Null),
            }
            columns.push(values);
        }
        slots
            .iter()
            .map(|slot| match *slot {
                Ok(row) => {
                    let at = wanted.binary_search(&row).ok()?;
                    Some(columns.iter().map(|column| column[at].clone()).collect())
                }
                Err(at) => Some(pick(&self.versions[at].values)),
            })
            .collect()
    }

    /// Gives `each` the header of each version at the places `places` in the order of [`Table::each_version`], which go up. A cold table decodes only the vectors of headers with these places. `None` is a place after the last version.
    fn versions_at(&self, places: &[usize], mut each: impl FnMut(&Version)) -> Option<()> {
        let Some(cold) = &self.cold else {
            for &place in places {
                each(self.versions.get(place)?);
            }
            return Some(());
        };
        let mut decoded: Option<(usize, Vec<Version>)> = None;
        for &place in places {
            let vector = place / VECTOR;
            if decoded.as_ref().is_none_or(|(at, _)| *at != vector) {
                decoded = Some((vector, Cold::vector(cold.vectors.get(vector)?).ok()?));
            }
            let (_, versions) = decoded.as_ref()?;
            each(versions.get(place % VECTOR)?);
        }
        Some(())
    }

    /// Gives `each` the header of each version of the table, cold or not, with no values.
    fn each_version(&self, mut each: impl FnMut(&Version)) {
        let Some(cold) = &self.cold else {
            self.versions.iter().for_each(each);
            return;
        };
        for vector in &cold.vectors {
            // The headers decoded when they were made or read, so the decode does not fail.
            for version in Cold::vector(vector).unwrap_or_default() {
                each(&version);
            }
        }
    }

    /// Encodes the open rows of the frozen columns into a last segment. A column that does not encode keeps its open rows.
    fn seal(&mut self) {
        if let Some(frozen) = &mut self.frozen {
            for column in &mut frozen.columns {
                if !column.sealed() {
                    let _ = Arc::make_mut(column).seal();
                }
            }
        }
    }

    /// Makes the versions of a cold table again from their headers.
    fn warm(&mut self) {
        let Some(cold) = self.cold.take() else { return };
        // The headers decoded when they were made or read, so the decode does not fail.
        self.versions = cold.versions().unwrap_or_default();
        debug_assert_eq!(self.versions.len(), cold.rows, "cold headers do not decode");
    }

    /// Moves the headers of the versions into encoded columns, when every version is frozen, was inserted by a transaction that committed and was not deleted.
    fn chill(&mut self) {
        let frozen = self.frozen.as_ref().is_some_and(|f| f.rows == self.versions.len());
        if self.cold.is_none()
            && frozen
            && let Some(cold) = Cold::of(&self.versions)
        {
            self.cold = Some(cold);
            self.versions = Vec::new();
        }
    }

    /// The number of versions, cold or not.
    fn len(&self) -> usize {
        self.cold.as_ref().map_or(self.versions.len(), |cold| cold.rows)
    }

    /// The table for a change of its versions, which then has the values of its versions and no columns.
    fn touch(&mut self) -> &mut Table {
        self.thaw();
        self.columns = Columns::default();
        self
    }

    /// Puts the values of the frozen versions of the table back into the versions, for a write. The columns that a read kept go.
    fn thaw(&mut self) {
        self.kept = None;
        self.thaw_rows();
    }

    /// [`Table::thaw`] for a read, which keeps the frozen columns for the next freeze.
    fn thaw_read(&mut self) {
        if let Some((frozen, places)) = self.thaw_rows() {
            let (made, len) = (self.made, self.versions.len());
            self.kept = Some(Kept { frozen, places, made, len });
        }
    }

    /// Puts the values of the frozen versions back into the versions, and gives the columns with the places of these versions.
    fn thaw_rows(&mut self) -> Option<(Frozen, Vec<usize>)> {
        self.warm();
        self.frozen.as_ref()?;
        let mut rows = Vec::new();
        self.each_row(|at, values| {
            if self.versions[at].values.is_empty() {
                rows.push((at, Arc::<[Value]>::from(values)));
            }
        });
        let frozen = self.frozen.take()?;
        let places = rows.iter().map(|&(at, _)| at).collect();
        for (at, values) in rows {
            self.versions[at].values = values;
        }
        Some((frozen, places))
    }

    /// Takes back the columns that a read thawed, when the table has the same versions, and drops the values of these versions again.
    fn take_kept(&mut self) {
        let Some(kept) = self.kept.take() else { return };
        if self.frozen.is_some() || kept.made != self.made || kept.len != self.versions.len() {
            return;
        }
        for &at in &kept.places {
            self.versions[at].values = Arc::default();
        }
        self.frozen = Some(kept.frozen);
    }

    fn add(
        &mut self,
        kept: Option<u64>,
        (xmin, cmin): (u32, u32),
        values: Vec<Value>,
        (unique, fresh): (&[Unique], &[DerivedKey]),
        old: Option<u64>,
        moved: &mut u32,
    ) -> u64 {
        let prior = old.and_then(|old| self.find(old)).map(|at| &*self.versions[at].values);
        let toasted = toast(&values, &self.fixed, &self.storage, prior);
        // A write that waited makes its versions again in the place that the first try took. The first try took the OIDs of the TOAST values.
        if kept.is_none() {
            *moved += toasted.moved;
        }
        let len = max_align(toasted.len);
        let tid = kept.unwrap_or_else(|| {
            let bulk = self.bulk.filter(|&made| made == (xmin, cmin));
            let (page, slot) = self.pages.place(len, old.map(page_of), bulk, self.save_free);
            pages::tid(self.pages.generation, page, slot)
        });
        self.put(tid, (xmin, cmin), values, (unique, fresh), len)
    }

    /// The places of the new versions of the rows `rows` of a copy, each with the length of its line in `lines`, and the length of each version, as `heap_multi_insert` places them. `ahead` has the lengths and the lines of the rows of the copy after `rows`, for the rest of their batch. The toaster takes the OIDs of the values that it moves.
    fn place_copy(
        &mut self,
        made: (u32, u32),
        rows: &[Vec<Value>],
        (lines, ahead): CopyLines<'_>,
        use_fsm: bool,
        moved: &mut u32,
    ) -> Vec<(u64, usize)> {
        let sized: Vec<(usize, usize)> = rows
            .iter()
            .enumerate()
            .map(|(at, values)| {
                let toasted = toast(values, &self.fixed, &self.storage, None);
                *moved += toasted.moved;
                (max_align(toasted.len), lines.get(at).copied().unwrap_or(0))
            })
            .collect();
        let places = self.pages.place_copy((&sized, ahead), made, use_fsm, self.save_free);
        let generation = self.pages.generation;
        let tids = places.into_iter().map(|(page, slot)| pages::tid(generation, page, slot));
        tids.zip(sized).map(|(tid, (len, _))| (tid, len)).collect()
    }

    /// The new version `tid` of `len` bytes with the values `values`, in the maps of the unique keys and in the order of the versions.
    fn put(
        &mut self,
        tid: u64,
        (xmin, cmin): (u32, u32),
        values: Vec<Value>,
        (unique, fresh): (&[Unique], &[DerivedKey]),
        len: usize,
    ) -> u64 {
        self.made += 1;
        for map in &mut self.keys {
            if map.stale {
                continue;
            }
            if map.derived.is_none() {
                map.add(tid, &values, None);
            } else if let Some(place) = unique.iter().position(|key| map.is(key)) {
                map.add(tid, &values, fresh.get(place).cloned().flatten());
            } else {
                map.make_stale();
            }
        }
        let values = Arc::<[Value]>::from(values);
        let len = u16::try_from(len).unwrap_or(u16::MAX);
        let made = self.made;
        let version = Version {
            tid,
            made,
            xmin,
            cmin,
            xmax: 0,
            cmax: 0,
            aborted: 0,
            next: 0,
            key_kept: false,
            len,
            values,
        };
        if self.versions.last().is_none_or(|last| last.tid < tid) {
            self.versions.push(version);
        } else {
            let at = self.versions.partition_point(|other| other.tid < tid);
            // A new version between the frozen ones keeps its values, and the place after the last frozen version moves on.
            if let Some(frozen) = self.frozen.as_mut().filter(|frozen| at < frozen.end) {
                frozen.end += 1;
            }
            self.versions.insert(at, version);
        }
        tid
    }

    /// The place of the version `tid`.
    fn find(&self, tid: u64) -> Option<usize> {
        place_of(&self.versions, tid)
    }

    /// `heap_lock_updated_tuple` and `EvalPlanQualFetch` follow the update chain from the version at `at`: true when the version, or a later version that committed transactions made, was deleted to move its row to another partition, as `ItemPointerIndicatesMovedPartitions` tells.
    fn chain_moved(&self, at: usize, running: &BTreeSet<u32>) -> bool {
        let mut version = &self.versions[at];
        loop {
            if version.next == MOVED {
                return true;
            }
            if version.next == 0 || version.xmax == 0 || running.contains(&version.xmax) {
                return false;
            }
            let Some(next) = self.find(version.next) else { return false };
            version = &self.versions[next];
        }
    }

    /// The place of the version of each ID of `tids`, which go up, cold or not, or `None` for an ID with no version. A cold table decodes only the IDs of its headers, a vector at a time, and only the vectors up to the last ID.
    fn places_of(&self, tids: &[u64]) -> Vec<Option<usize>> {
        let Some(cold) = &self.cold else {
            return tids.iter().map(|&tid| self.find(tid)).collect();
        };
        let mut out = Vec::with_capacity(tids.len());
        let mut next = tids.iter().copied().peekable();
        let mut first = 0;
        for vector in &cold.vectors {
            if next.peek().is_none() {
                break;
            }
            // The headers decoded when they were made or read, so the decode does not fail.
            let ids = integer::decode(&vector[0]).unwrap_or_default();
            let last = ids.last().map_or(0, |&id| id as u64);
            while let Some(tid) = next.next_if(|&tid| tid <= last) {
                let at = ids.binary_search(&tid.cast_signed()).ok();
                out.push(at.map(|at| first + at));
            }
            first += ids.len();
        }
        out.resize(tids.len(), None);
        out
    }

    /// `heap_page_prune_opt` for one key: the IDs of the versions that went, and of the versions that are dead to each snapshot, leave the list of the hash `hash` in the map `place`. So a read of a key that changed many times does not test each old version again until `VACUUM`. A version is dead when a transaction before `horizon` that is not in `running` deleted it, as in [`Heap::vacuum`].
    fn prune_key(&mut self, place: usize, hash: u64, (horizon, running): (u32, &BTreeSet<u32>)) {
        let Table { keys, versions, .. } = self;
        let Some(map) = keys.get_mut(place) else { return };
        let Some(tids) = map.hashes.get_mut(&hash).filter(|tids| tids.len() > 1) else { return };
        let dead = |tid: u64| match place_of(versions, tid) {
            Some(at) => {
                let xmax = versions[at].xmax;
                xmax != 0 && !running.contains(&xmax) && xmax < horizon
            }
            None => true,
        };
        tids.retain(|&tid| !dead(tid));
    }

    /// The place in `keys` of the map of each unique key. The first write that checks a key with no expression makes its map from all the versions. The result is `None` when an index with an expression has no map, which [`Heap::lock_keys`] makes while the heap is not locked.
    fn key_maps(&mut self, unique: &[Unique]) -> Option<Vec<usize>> {
        self.keys.retain(|map| !map.stale);
        let mut places = Vec::with_capacity(unique.len());
        for key in unique {
            let place = match self.keys.iter().position(|map| map.is(key)) {
                Some(place) => place,
                None if key.derived.is_some() => return None,
                None => {
                    let mut map = KeyMap::of(key);
                    for version in &self.versions {
                        map.add(version.tid, &version.values, None);
                    }
                    self.keys.push(map);
                    self.keys.len() - 1
                }
            };
            places.push(place);
        }
        Some(places)
    }

    /// `_bt_check_unique` of the new version `tid` for each unique key, in the order of the indexes. The result is the first conflict: a live version with the same key, or a transaction in progress that the write must wait for. `used` has the IDs of the new versions of the write up to `tid`. A conflict in a deferrable index is no error: the place of the index and `tid` go to `partial`, for the check after the statement.
    fn check_keys(
        &self,
        (places, unique): (&[usize], &[Unique]),
        used: &[u64],
        (xid, running): (u32, &BTreeSet<u32>),
        partial: &mut Vec<(usize, u64)>,
    ) -> Result<Option<Changed>> {
        let Some(&tid) = used.last() else {
            return Ok(None);
        };
        let Some(at) = self.find(tid) else {
            return Ok(None);
        };
        let row = &self.versions[at].values;
        for (index, &place) in places.iter().enumerate() {
            let map = &self.keys[place];
            let Some(values) = map.key(tid, row) else {
                continue;
            };
            let Some(hash) = map.hash(values) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                if other == tid {
                    continue;
                }
                let Some(found) = self.find(other) else {
                    continue;
                };
                let version = &self.versions[found];
                let seen = dirty(version, xid, running);
                if seen == Seen::Dead {
                    continue;
                }
                match map.key(other, &version.values) {
                    Some(other) if map.same(values, other)? => {}
                    _ => continue,
                }
                if unique.get(index).is_some_and(|key| key.deferrable.is_some()) {
                    partial.push((index, tid));
                    break;
                }
                return Ok(Some(match seen {
                    Seen::Wait(holder) => Changed::WaitKey {
                        holder,
                        index,
                        used: used.to_vec(),
                        waits: used.last().copied().unwrap_or_default(),
                    },
                    _ => Changed::Duplicate {
                        index,
                        key: map.columns.iter().map(|&column| values[column].clone()).collect(),
                    },
                }));
            }
        }
        Ok(None)
    }

    /// A transaction other than `xid` that is in progress and inserted or deleted a version of the table.
    fn holder(&self, xid: u32, running: &BTreeSet<u32>) -> Option<u32> {
        let other = |x: u32| x != 0 && x != xid && running.contains(&x);
        self.versions.iter().find_map(|v| [v.xmin, v.xmax].into_iter().find(|&x| other(x)))
    }

    /// The first version that is not dead with the key of a version before it in the map `map` of the unique index at the place `index`, as [`Heap::check_build`] gives it.
    fn first_duplicate(
        &self,
        index: usize,
        map: &KeyMap,
        (xid, running): (u32, &BTreeSet<u32>),
    ) -> Result<Option<Changed>> {
        for version in &self.versions {
            let seen = dirty(version, xid, running);
            if seen == Seen::Dead {
                continue;
            }
            let Some(values) = map.key(version.tid, &version.values) else {
                continue;
            };
            let Some(hash) = map.hash(values) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                if other >= version.tid {
                    continue;
                }
                let Some(found) = self.find(other) else {
                    continue;
                };
                let before = &self.versions[found];
                let seen_before = dirty(before, xid, running);
                if seen_before == Seen::Dead {
                    continue;
                }
                let earlier = match map.key(other, &before.values) {
                    Some(earlier) if map.same(values, earlier)? => earlier,
                    _ => continue,
                };
                return Ok(Some(match (seen_before, seen) {
                    (Seen::Wait(holder), _) | (_, Seen::Wait(holder)) => Changed::WaitKey {
                        holder,
                        index,
                        used: vec![version.tid],
                        waits: version.tid,
                    },
                    _ => Changed::Duplicate {
                        index,
                        key: map.columns.iter().map(|&column| earlier[column].clone()).collect(),
                    },
                }));
            }
        }
        Ok(None)
    }

    /// Takes back the new versions `used` and the deletes of the versions `deleted`, after a write that must wait or that failed. The IDs stay used: after a wait the write makes its versions again with the same IDs, as the versions of PostgreSQL stay in place while it waits, and after a failure the versions of PostgreSQL stay in the heap until the end of the transaction.
    fn undo(&mut self, used: &[u64], deleted: &[u64]) {
        let mut used = used.to_vec();
        used.sort_unstable();
        let used = used.as_slice();
        if let Some(&first) = used.first() {
            let at = self.versions.partition_point(|version| version.tid < first);
            let tail = self.versions.split_off(at);
            self.versions.extend(tail.into_iter().filter(|v| used.binary_search(&v.tid).is_err()));
            self.forget_keys(used);
        }
        for &tid in deleted {
            if let Some(at) = self.find(tid) {
                let version = &mut self.versions[at];
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
    }

    /// Removes the versions that `gone` picks.
    fn remove(&mut self, gone: impl Fn(&Version) -> bool) {
        if self.keys.iter().any(|map| !map.derived_keys.is_empty()) {
            let tids: Vec<u64> = self.versions.iter().filter(|v| gone(v)).map(|v| v.tid).collect();
            self.forget_keys(&tids);
        }
        self.versions.retain(|version| !gone(version));
        self.forget_combos();
    }

    /// Removes the combo command IDs of the versions that went.
    fn forget_combos(&mut self) {
        if !self.combos.is_empty() {
            let versions = &self.versions;
            self.combos.retain(|tid, _| versions.binary_search_by_key(tid, |v| v.tid).is_ok());
        }
    }

    /// Removes the versions `gone`, in the order of their IDs, that `VACUUM` found dead, also from the maps of the unique keys, as the IDs of their slots can come back.
    fn purge(&mut self, gone: &[u64]) {
        if gone.is_empty() {
            return;
        }
        self.versions.retain(|version| gone.binary_search(&version.tid).is_err());
        self.forget_keys(gone);
        self.forget_combos();
        for map in &mut self.keys {
            map.hashes.retain(|_, tids| {
                tids.retain(|tid| gone.binary_search(tid).is_err());
                !tids.is_empty()
            });
        }
    }

    /// Removes the keys of the versions `tids` that went from the maps of the indexes with an expression.
    fn forget_keys(&mut self, tids: &[u64]) {
        for map in self.keys.iter_mut().filter(|map| !map.derived_keys.is_empty()) {
            for tid in tids {
                map.derived_keys.remove(tid);
            }
        }
    }
}

/// One version of a row, `HeapTupleHeaderData`.
#[derive(Debug)]
struct Version {
    /// The ID of the version in its table, its item pointer with the generation of the heap of [`pages::tid`]. It does not change while the version lives.
    tid: u64,
    /// The place of the version in the order of the inserts into the table.
    made: u64,
    /// The transaction that inserted the version.
    xmin: u32,
    /// The command of `xmin` that inserted the version.
    cmin: u32,
    /// The transaction that deleted the version, or 0.
    xmax: u32,
    /// The command of `xmax` that deleted the version.
    cmax: u32,
    /// The transaction that deleted the version and then aborted, or 0. The column `xmax` of the version shows it while no other transaction deletes the version, as `t_xmax` keeps the ID of an aborted transaction.
    aborted: u32,
    /// `t_ctid`: the ID of the version that an `UPDATE` made from this version, or 0.
    next: u64,
    /// The update that made [`Version::next`] kept the columns of each unique index, so `HEAP_KEYS_UPDATED` is not set and a `FOR KEY SHARE` goes on over it.
    key_kept: bool,
    /// `MAXALIGN(t_len)`: the space of the version in its page.
    len: u16,
    /// The values of the columns. A point read takes a count of the `Arc` and reads the values after it lets the lock of the heap go, with no copy of them.
    values: Arc<[Value]>,
}

/// `SnapshotData` of an MVCC snapshot: the transactions whose changes a statement does not see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// All the transactions from this ID are not visible.
    xmax: u32,
    /// The transactions before `xmax` that were in progress.
    running: Arc<BTreeSet<u32>>,
    /// `latestCompletedXid + 1`, the `xmax` of the snapshot of `GetSnapshotData`. No transaction from this ID on ended, so the visibility is the same with this ID as with `xmax`.
    completed: u32,
}

impl Snapshot {
    /// The `xmin` of `GetSnapshotData`: the first transaction in progress before `latestCompletedXid + 1`, also when it is the transaction of the session, or `latestCompletedXid + 1` when there is none.
    pub(crate) fn xmin(&self) -> u32 {
        self.running.range(..self.completed).next().copied().unwrap_or(self.completed)
    }

    /// The snapshot as a `pg_snapshot`, as `GetSnapshotData` makes it for the session whose transaction is `own`: `latestCompletedXid + 1` as `xmax`, the transactions in progress before it without `own`, and [`Snapshot::xmin`] as `xmin`.
    pub(crate) fn exported(&self, own: u32) -> rupg_types::Snapshot {
        let xip: Vec<u64> = self
            .running
            .range(..self.completed)
            .filter(|&&xid| xid != own)
            .map(|&xid| u64::from(xid))
            .collect();
        rupg_types::Snapshot { xmin: u64::from(self.xmin()), xmax: u64::from(self.completed), xip }
    }
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
    /// The record of the transaction in `SERIALIZABLE`, which checks its read-write conflicts, or 0.
    pub(crate) sxact: u64,
}

impl View {
    /// True when the view sees the changes of the transaction `xid`: its own, or one that committed before its snapshot.
    pub(crate) fn sees(&self, xid: u32) -> bool {
        (self.xid != 0 && xid == self.xid) || committed_before(xid, &self.snapshot)
    }
}

/// A change of a row by `UPDATE` or `DELETE`: the ID of the version, and the new values, or `None` to delete it.
pub(crate) type Change = (u64, Option<Vec<Value>>);

/// The result of [`Heap::insert`] and [`Heap::change`].
#[derive(Debug, PartialEq)]
pub(crate) enum Changed {
    /// The number of rows that changed.
    Done(u64),
    /// The transaction `holder` is in progress and changed the version `tid`, so the statement must wait for the end of `holder` and try again.
    Wait { holder: u32, tid: u64 },
    /// In `READ COMMITTED`, a transaction that committed after the snapshot changed a row. The statement must run again with a new snapshot, which sees the last version of the row, as `EvalPlanQual` checks it.
    Again,
    /// The transaction `holder` is in progress and inserted or deleted a version with the same key in the unique index at the place `index` of the list of the write. The write must wait for the end of `holder` and try again with the IDs `used` of its new versions. `waits` is the ID of the version that waits.
    WaitKey { holder: u32, index: usize, used: Vec<u64>, waits: u64 },
    /// A live version has the key of a new version in the unique index at the place `index`. `key` is the value of each key column.
    Duplicate { index: usize, key: Vec<Value> },
    /// `TM_SelfModified` with a `cmax` that is not the command of the view: a later command of the transaction, such as a statement of a trigger, changed the row. `update` is true when the change of the row is an update.
    SelfModified { update: bool },
}

/// The result of [`Heap::speculative`] for one row of `INSERT ... ON CONFLICT`.
#[derive(Debug, PartialEq)]
pub(crate) enum Speculative {
    /// The row went in, as the version with this ID.
    Inserted(u64),
    /// A live version has the key of the row in an arbiter index: the ID of the version and its values. `own` is true when the transaction of the view made the version, `command` is true when the command of the view made it, and `visible` is true when the snapshot of the view sees it.
    Conflict { tid: u64, values: Vec<Value>, own: bool, command: bool, visible: bool },
    /// The transaction `holder` is in progress and inserted or deleted the version `tid` with the key of the row in an arbiter index. The statement must wait for its end and check the row again.
    Wait { holder: u32, tid: u64 },
    /// A conflict in a unique index that is not an arbiter, as [`Heap::insert`] gives it.
    Key(Changed),
}

/// A version that an exclusion constraint checks, from [`Heap::exclusion_rows`].
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) tid: u64,
    /// The place of the version in the order of the inserts.
    pub(crate) made: u64,
    /// True when the commands that the check is for made the version.
    pub(crate) new: bool,
    /// The transaction in progress that inserted or deleted the version.
    pub(crate) holder: Option<u32>,
    /// True when the version is live for a dirty snapshot.
    pub(crate) live: bool,
    /// The place in the order of the inserts of the version that an `UPDATE` of the commands made from this version, or 0. The version was live until then.
    pub(crate) replaced: u64,
    pub(crate) values: Vec<Value>,
}

/// The text of the item pointer of a version, `(page,item)`, as `tidout` gives it.
pub(crate) fn tid_text(tid: u64) -> String {
    format!("({},{})", page_of(tid), slot_of(tid))
}

impl Heap {
    /// Gives the heap the tasks of the server, which encode the columns of a load at the same time.
    pub(crate) fn set_tasks(&mut self, tasks: Option<Arc<dyn Tasks>>) {
        self.tasks = tasks;
    }

    fn state(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetMultiXactInfo`: the number of the multixacts that the row locks made and the sum of their members.
    pub(crate) fn multixacts(&self) -> (u64, u64) {
        self.state_read().multixacts
    }

    /// A read lock of the state, for a read that does not change it.
    fn state_read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lock of [`Heap::published`]. A thread that holds the state takes it after the state.
    fn published(&self) -> MutexGuard<'_, Option<Snapshot>> {
        self.published.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the snapshot of `state` to [`Heap::published`], after a change of the state that a snapshot reads.
    fn publish(&self, state: &State) {
        *self.published() = Some(Self::snapshot_of(state));
    }

    /// Locks the heap when the table `table` has the map of each unique key of `unique`, and gives the place of each map. The map of an index with an expression or a predicate gets the keys of the versions while the heap is not locked, as a function in an expression can read and change the heap. The versions that other writes add meanwhile get their keys in the next round.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of an index.
    fn lock_keys(
        &self,
        table: (u32, u32),
        unique: &[Unique],
        derive: Derive<'_>,
    ) -> Result<(RwLockWriteGuard<'_, State>, Vec<usize>)> {
        // The maps that this call makes, by the place of the key in `unique`, with the address of the values of each version whose key the map has, as an abort and a new try of a write can give a new version the ID of an old one.
        let mut made: Vec<(usize, KeyMap, HashMap<u64, usize>)> = Vec::new();
        loop {
            let mut state = self.state();
            let rows = state.tables.entry(table);
            if let Some(places) = rows.key_maps(unique) {
                return Ok((state, places));
            }
            let mut todo = Vec::new();
            for (place, key) in unique.iter().enumerate() {
                if key.derived.is_none() || rows.keys.iter().any(|map| map.is(key)) {
                    continue;
                }
                let at = match made.iter().position(|(other, ..)| *other == place) {
                    Some(at) => at,
                    None => {
                        made.push((place, KeyMap::of(key), HashMap::new()));
                        made.len() - 1
                    }
                };
                let seen = &made[at].2;
                let versions: Vec<_> = rows
                    .versions
                    .iter()
                    .filter(|v| seen.get(&v.tid) != Some(&(v.values.as_ptr() as usize)))
                    .map(|v| (v.tid, v.values.as_ptr() as usize, v.values.clone(), v.xmax != 0))
                    .collect();
                if versions.is_empty() {
                    let (_, map, _) = made.swap_remove(at);
                    rows.keys.push(map);
                } else {
                    todo.push((place, versions));
                }
            }
            if todo.is_empty() {
                continue;
            }
            drop(state);
            for (place, versions) in todo {
                let Some((_, map, seen)) = made.iter_mut().find(|(other, ..)| *other == place)
                else {
                    continue;
                };
                let Some(derived) = map.derived.clone() else { continue };
                for (tid, address, values, deleted) in versions {
                    // A version that a rewrite of the table deleted has the values of the old types of the columns, which the expressions of the new index can not take. The version is not in the index.
                    let key = match derive(&derived, &values) {
                        Err(_) if deleted => None,
                        key => key?.map(Vec::into_boxed_slice),
                    };
                    map.add(tid, &values, key);
                    seen.insert(tid, address);
                }
            }
        }
    }

    /// `GetNewTransactionId`: a transaction ID for a transaction of the session `pid` that writes. The transaction is in progress until [`Heap::commit`] or [`Heap::abort`].
    pub(crate) fn begin(&self, pid: i32) -> u32 {
        let mut state = self.state();
        let xid = state.next_xid.max(INITDB_NEXT_XID);
        state.next_xid = xid + 1;
        Arc::make_mut(&mut state.running).insert(xid);
        state.owners.insert(xid, pid);
        self.publish(&state);
        xid
    }

    /// `MarkAsPrepared`: the transaction `xid` stays in progress after the session that began it ends it, with `owner` as the owner of its locks.
    pub(crate) fn hand_over(&self, xid: u32, owner: i32) {
        self.state().owners.insert(xid, owner);
    }

    /// `ReadNextFullTransactionId`: the transaction ID that the next transaction that writes gets.
    pub(crate) fn next_xid(&self) -> u32 {
        self.state().next_xid.max(INITDB_NEXT_XID)
    }

    /// Sets the `xmin` of the session `pid` to the `xmin` of `snapshot` when the session holds no snapshot, as `GetSnapshotData` sets `MyProc->xmin`.
    pub(crate) fn hold(&self, pid: i32, snapshot: &Snapshot) {
        hold(&mut self.state(), pid, snapshot);
    }

    /// The `xmin` of the session `pid`, which the session clears with no lock when it holds no snapshot.
    pub(crate) fn xmin(&self, pid: i32) -> Arc<AtomicU32> {
        Arc::clone(self.state().xmins.entry(pid).or_default())
    }

    /// The session `pid` holds no snapshot, as `SnapshotResetXmin` clears `MyProc->xmin`.
    pub(crate) fn release(&self, pid: i32) {
        self.state().xmins.remove(&pid);
    }

    /// `ProcNumberGetTransactionIds`: the transaction ID of the session `pid` and its `xmin`, with 0 for none.
    pub(crate) fn transaction_ids(&self, pid: i32) -> (u32, u32) {
        let state = self.state();
        let xid = state.owners.iter().find(|&(_, &owner)| owner == pid).map_or(0, |(&xid, _)| xid);
        (xid, state.xmins.get(&pid).map_or(0, |xmin| xmin.load(atomic::Ordering::Acquire)))
    }

    /// The state of the transaction `xid` in `pg_xact` and `ProcArray`, for a transaction before [`Heap::next_xid`].
    pub(crate) fn xact_state(&self, xid: u32) -> XactState {
        let state = self.state();
        if state.running.contains(&xid) {
            XactState::InProgress
        } else if state.aborted.contains(&xid) {
            XactState::Aborted
        } else {
            XactState::Committed
        }
    }

    /// `GetSnapshotData`: the transactions whose changes are not visible now.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Snapshot {
        Self::snapshot_of(&self.state())
    }

    /// [`Heap::snapshot`] for the session `pid`, which then holds its `xmin` as [`Heap::hold`] does. The snapshot and the hold are under one lock, as `GetSnapshotData` sets `MyProc->xmin` under `ProcArrayLock`, so a prune does not remove a version that the snapshot sees before the session holds it.
    #[cfg(test)]
    pub(crate) fn held_snapshot(&self, pid: i32) -> Snapshot {
        let mut state = self.state();
        let snapshot = Self::snapshot_of(&state);
        hold(&mut state, pid, &snapshot);
        snapshot
    }

    /// [`Heap::held_snapshot`] for the session whose `xmin` is `xmin`, as [`Heap::xmin`] gives it. It locks only [`Heap::published`] after the first change of the state.
    pub(crate) fn held_snapshot_in(&self, xmin: &AtomicU32) -> Snapshot {
        let mut published = self.published();
        let snapshot = match &*published {
            Some(snapshot) => snapshot.clone(),
            None => {
                drop(published);
                let state = self.state();
                published = self.published();
                published.get_or_insert_with(|| Self::snapshot_of(&state)).clone()
            }
        };
        if xmin.load(atomic::Ordering::Acquire) == 0 {
            xmin.store(snapshot.xmin(), atomic::Ordering::Release);
        }
        drop(published);
        snapshot
    }

    /// The state of serializable snapshot isolation.
    fn ssi(&self) -> MutexGuard<'_, ssi::Ssi> {
        self.ssi.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetSerializableTransactionSnapshot`: [`Heap::held_snapshot_in`] for the first statement of a transaction in `SERIALIZABLE`, and the record of the transaction, which `read_only` declares `READ ONLY`. The snapshot and the record are under the lock of the serializable transactions, so the commits before the snapshot are the ones that the record counts.
    ///
    /// The record of a `DEFERRABLE` transaction of the session `pid` waits for a safe snapshot, which [`Heap::serializable_safety`] tells.
    pub(crate) fn serializable_snapshot_in(
        &self,
        xmin: &AtomicU32,
        (read_only, deferrable): (bool, bool),
        pid: i32,
    ) -> (Snapshot, u64) {
        let mut ssi = self.ssi();
        let snapshot = self.held_snapshot_in(xmin);
        let id = ssi.begin(snapshot.xmin(), read_only, deferrable, pid);
        (snapshot, id)
    }

    /// `GetSafeSnapshot`: whether the snapshot of the read-only transaction of the record `sxact` is safe, unsafe, or not known yet (`None`).
    pub(crate) fn serializable_safety(&self, sxact: u64) -> Option<bool> {
        self.ssi().safety(sxact)
    }

    /// `GetSafeSnapshotBlockingPids` of the session `pid`.
    pub(crate) fn safe_snapshot_blockers(&self, pid: i32) -> Vec<i32> {
        self.ssi().safe_snapshot_blockers(pid)
    }

    /// `PredicateLockRelation` and `CheckForSerializableConflictOut` for a read of the table `relation` with the view.
    ///
    /// # Errors
    ///
    /// `40001` when the read makes a dangerous structure where the transaction of the view must fail.
    pub(crate) fn serializable_read(&self, relation: u32, view: &View) -> Result<()> {
        if view.sxact == 0 {
            return Ok(());
        }
        let snapshot = &view.snapshot;
        let concurrent = |xid: u32| !committed_before(xid, snapshot);
        self.ssi().read(view.sxact, (view.database, relation), &concurrent)
    }

    /// `CheckTableForSerializableConflictIn` for a write of the whole table `relation` with the view, as `TRUNCATE` does.
    ///
    /// # Errors
    ///
    /// `40001` when the write makes a dangerous structure where the transaction of the view must fail.
    pub(crate) fn serializable_write(&self, relation: u32, view: &View) -> Result<()> {
        let mut ssi = self.serializable_lock(view);
        conflict_in(ssi.as_deref_mut(), view, relation, None)
    }

    /// The lock of the serializable transactions for a read or a write of the transaction of the view in `SERIALIZABLE`. A write holds it from its check of the `SIREAD` locks until the write is in the heap, as `heap_update` holds the lock of the buffer, and an index scan holds it from its read until it has its `SIREAD` locks, so that the read either finds the write or the write finds the lock of the read.
    fn serializable_lock(&self, view: &View) -> Option<MutexGuard<'_, ssi::Ssi>> {
        (view.sxact != 0).then(|| self.ssi())
    }

    /// `PredicateLockTID` and `HeapCheckForSerializableConflictOut` for the versions `examined` that an index scan of the table `relation` with the view examined, with the lock of [`Heap::serializable_lock`].
    fn serializable_read_versions(
        ssi: Option<&mut ssi::Ssi>,
        view: &View,
        relation: u32,
        examined: &[ssi::Examined],
    ) -> Result<()> {
        let Some(ssi) = ssi else { return Ok(()) };
        let snapshot = &view.snapshot;
        let concurrent = |xid: u32| !committed_before(xid, snapshot);
        ssi.read_versions(view.sxact, ((view.database, relation), view.xid), examined, &concurrent)
    }

    /// `PreCommit_CheckForSerializationFailure` for the transaction of the record `sxact`.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction must fail.
    pub(crate) fn serializable_pre_commit(&self, sxact: u64) -> Result<()> {
        self.ssi().pre_commit(sxact)
    }

    /// `ReleasePredicateLocks` at the end of the transaction of the record `sxact`.
    pub(crate) fn serializable_end(&self, sxact: u64, commit: bool) {
        let next_xid = self.next_xid();
        self.ssi().end(sxact, commit, next_xid);
    }

    fn snapshot_of(state: &State) -> Snapshot {
        Snapshot {
            xmax: state.next_xid.max(INITDB_NEXT_XID),
            running: Arc::clone(&state.running),
            completed: (state.latest_completed + 1).max(INITDB_NEXT_XID),
        }
    }

    /// The rows of a table that the view sees, `HeapTupleSatisfiesMVCC`. Each row ends with the ID of its version.
    pub(crate) fn rows(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        self.visible_rows(relation, view, false, None)
    }

    /// The rows of [`Heap::rows`], with the values of the system columns `xmin` and `xmax` of each version before the ID of the version when `xids` is true. With `used`, each row has only the columns that are true in `used`, in their order, and a column that the version has no value for is null.
    pub(crate) fn visible_rows(
        &self,
        relation: u32,
        view: &View,
        xids: bool,
        used: Option<&[bool]>,
    ) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return Vec::new();
        };
        let columns = used_columns(used);
        let mut rows = Vec::new();
        table.each_version_row(columns.as_deref(), |version, values| {
            if visible(version, view) {
                rows.push(version_row(version, values, &table.combos, columns.as_deref(), xids));
            }
        });
        rows
    }

    /// The rows of [`Heap::rows`] with the values of only the columns `columns`, which go up, each at its place, a null at each other place before the last of them, and then the ID of the version. A check of keys reads only its columns, so the other values of the frozen columns do not decode.
    pub(crate) fn sparse_rows(
        &self,
        relation: u32,
        view: &View,
        columns: &[usize],
    ) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return Vec::new();
        };
        let width = columns.last().map_or(0, |&last| last + 1);
        let mut rows = Vec::new();
        table.each_version_row(Some(columns), |version, values| {
            if !visible(version, view) {
                return;
            }
            let mut row = Vec::with_capacity(width + 1);
            row.resize(width, Value::Null);
            for &column in columns {
                if let Some(value) = values.get(column) {
                    row[column] = value.clone();
                }
            }
            row.extend(version_row(version, &[], &table.combos, Some(&[]), false));
            rows.push(row);
        });
        rows
    }

    /// True when the view sees a row of the table `relation`.
    pub(crate) fn sees_rows(&self, relation: u32, view: &View) -> bool {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return false;
        };
        let mut seen = false;
        table.each_version(|version| seen = seen || visible(version, view));
        seen
    }

    /// The columns `wanted` of a table, each with its physical type, in the order of the versions, when the view sees each version of the table and every version is live. The heap makes a column from the versions the first time a scan reads it, and keeps it until the table changes. It gives the number of rows with the columns. `None` is a table that the scan must read by rows: the table is small, a version is deleted, the view does not see a version, or a value does not have the type of its column.
    pub(crate) fn columns(
        &self,
        relation: u32,
        view: &View,
        wanted: &[(usize, Kind)],
    ) -> Option<(usize, Vec<Arc<Stored>>)> {
        let mut state = self.state();
        let table = state.tables.0.get_mut(&(view.database, relation))?;
        if table.len() < COLUMN_ROWS {
            return None;
        }
        let columns = &mut table.columns;
        if let (None, Some(cold)) = (&columns.xmins, &table.cold) {
            columns.xmins = Some(cold.xmins.clone());
        }
        if columns.xmins.is_none() {
            let mut xmins: Vec<(u32, u32)> = Vec::new();
            for version in &table.versions {
                if version.xmax != 0 {
                    columns.mixed = true;
                    break;
                }
                match xmins.iter_mut().find(|(xmin, _)| *xmin == version.xmin) {
                    Some((_, cmin)) => *cmin = (*cmin).max(version.cmin),
                    None => xmins.push((version.xmin, version.cmin)),
                }
            }
            columns.xmins = Some(xmins);
        }
        if columns.mixed {
            return None;
        }
        let sees = |&(xmin, cmin): &(u32, u32)| {
            if view.xid != 0 && xmin == view.xid {
                cmin < view.command
            } else {
                committed_before(xmin, &view.snapshot)
            }
        };
        let xmins = columns.xmins.as_ref()?;
        if !xmins.iter().all(sees) {
            return None;
        }
        // A commit logs the values of the versions that it inserted, so the heap freezes only the versions of transactions that ended.
        if xmins.iter().all(|&(xmin, _)| xmin != view.xid && committed_before(xmin, &view.snapshot))
        {
            table.freeze(self.tasks.as_deref());
            table.chill();
        }
        // A scan reads the segments of a column, so the open rows of a load become a last segment first.
        table.seal();
        if table.frozen.as_ref().is_some_and(|frozen| !frozen.columns.iter().all(|c| c.sealed())) {
            return None;
        }
        let rows = table.len();
        let columns = &mut table.columns;
        let mut out = Vec::with_capacity(wanted.len());
        for &(at, kind) in wanted {
            if let Some(frozen) = &table.frozen {
                // A frozen table has no values in its versions to make a column of another type from.
                if frozen.rows != rows {
                    return None;
                }
                let stored = frozen.columns.get(at).filter(|stored| stored.kind == kind)?;
                out.push(Arc::clone(stored));
                continue;
            }
            let stored = match columns.stored.get(&at) {
                Some(stored) if stored.kind == kind => Arc::clone(stored),
                _ => {
                    let values = table.versions.iter().map(|version| version.values.get(at));
                    let stored = Arc::new(Stored::build(kind, values).ok()?);
                    columns.stored.insert(at, Arc::clone(&stored));
                    stored
                }
            };
            out.push(stored);
        }
        Some((rows, out))
    }

    /// The rows of [`Heap::visible_rows`] with no system columns, for only the versions `tids` of a table, in the order of the list. A version that the view does not see gives no row. The frozen columns decode only the vectors with these versions and only the columns at `used`, so that a fetch of few rows of a long table does not put the values back into all its versions.
    pub(crate) fn rows_at(
        &self,
        relation: u32,
        view: &View,
        used: Option<&[bool]>,
        tids: &[u64],
    ) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return Vec::new();
        };
        let mut order: Vec<usize> = (0..tids.len()).collect();
        order.sort_by_key(|&at| tids[at]);
        let sorted: Vec<u64> = order.iter().map(|&at| tids[at]).collect();
        // The place of each item of the list, then the places with no repeat, which go up as the IDs do.
        let mut place = vec![None; tids.len()];
        for (&at, found) in order.iter().zip(table.places_of(&sorted)) {
            place[at] = found;
        }
        let mut places: Vec<usize> = place.iter().flatten().copied().collect();
        places.sort_unstable();
        places.dedup();
        let columns = used_columns(used);
        let Some(values) = table.rows_at(&places, columns.as_deref()) else {
            return Vec::new();
        };
        let mut values = values.into_iter();
        let mut rows = Vec::with_capacity(places.len());
        let decoded = table.versions_at(&places, |version| {
            let values = values.next().unwrap_or_default();
            rows.push(
                visible(version, view)
                    .then(|| version_row(version, &values, &table.combos, None, false)),
            );
        });
        if decoded.is_none() {
            return Vec::new();
        }
        // A version that the list has once gives its row with no copy.
        let once = places.len() == place.iter().flatten().count();
        place
            .iter()
            .filter_map(|found| {
                let row = rows.get_mut(places.binary_search(&(*found)?).ok()?)?;
                if once { row.take() } else { row.clone() }
            })
            .collect()
    }

    /// The rows of [`Heap::visible_rows`] with no system columns for the versions at the places `places` in the order of the versions, which go up, of a table whose columns [`Heap::columns`] gave with `rows` rows. A cold table decodes only the vectors with these places, so that a join that keeps few rows of a long table makes the values of only those rows. `None` is a table with another number of versions, a version that the view does not see, or a column that does not decode.
    pub(crate) fn rows_at_places(
        &self,
        relation: u32,
        view: &View,
        used: Option<&[bool]>,
        rows: usize,
        places: &[usize],
    ) -> Option<Vec<Vec<Value>>> {
        let state = self.state();
        let table = state.tables.meta(&(view.database, relation))?;
        if table.len() != rows
            || !places.windows(2).all(|pair| pair[0] < pair[1])
            || places.last().is_some_and(|&place| place >= rows)
        {
            return None;
        }
        let columns = used_columns(used);
        let mut values = table.rows_at(places, columns.as_deref())?.into_iter();
        let mut out = Vec::with_capacity(places.len());
        let mut seen = true;
        table.versions_at(places, |version| {
            seen &= visible(version, view);
            let values = values.next().unwrap_or_default();
            out.push(version_row(version, &values, &table.combos, None, false));
        })?;
        (seen && out.len() == places.len()).then_some(out)
    }

    /// The rows of [`Heap::rows_at`] for the versions whose key has the hash of `key` in the map of the unique key with the columns `columns`, the types `types` and `nulls_not_distinct`, in the order of their IDs, for a scan that tests the key again. The first read of a key makes its map from all the versions, as the first write that checks the key does. A key with a null that is not equal to a null gives no row.
    pub(crate) fn key_rows(
        &self,
        relation: u32,
        view: &View,
        used: Option<&[bool]>,
        key_columns: (&[usize], &[u32], bool),
        key: &[Value],
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let wanted = used_columns(used);
        self.key_versions(relation, view, key_columns, key, |version, combos| {
            version_row(version, &version.values, combos, wanted.as_deref(), false)
        })
    }

    /// [`Heap::key_rows`] with the values of all the columns of each version as the heap keeps them, and the ID of the version. The values are not copied, so a point read reads them after it lets the lock of the heap go.
    pub(crate) fn key_shared(
        &self,
        relation: u32,
        view: &View,
        key_columns: (&[usize], &[u32], bool),
        key: &[Value],
    ) -> Result<Option<Vec<rupg_func::SharedRow>>> {
        self.key_versions(relation, view, key_columns, key, |version, _| {
            (Arc::clone(&version.values), version.tid)
        })
    }

    /// The result of `each` for each version that [`Heap::key_rows`] reads, with the combo command IDs of the table. In `SERIALIZABLE`, the read takes the `SIREAD` locks of an index scan of the key.
    ///
    /// # Errors
    ///
    /// `40001` when the read makes a dangerous structure where the transaction of the view must fail.
    fn key_versions<T>(
        &self,
        relation: u32,
        view: &View,
        key_columns: (&[usize], &[u32], bool),
        key: &[Value],
        mut each: impl FnMut(&Version, &HashMap<u64, u32>) -> T,
    ) -> Result<Option<Vec<T>>> {
        let mut ssi = self.serializable_lock(view);
        let mut examined = Vec::new();
        let examine = ssi.is_some().then_some(&mut examined);
        let found = self.key_versions_in(relation, view, key_columns, key, examine, &mut each);
        if found.is_some() {
            Self::serializable_read_versions(ssi.as_deref_mut(), view, relation, &examined)?;
        }
        Ok(found)
    }

    /// [`Heap::key_versions`] with no `SIREAD` locks. With `examined`, each version with the key that the read looks at goes in it.
    fn key_versions_in<T>(
        &self,
        relation: u32,
        view: &View,
        (columns, types, nulls_not_distinct): (&[usize], &[u32], bool),
        key: &[Value],
        mut examined: Option<&mut Vec<ssi::Examined>>,
        each: &mut impl FnMut(&Version, &HashMap<u64, u32>) -> T,
    ) -> Option<Vec<T>> {
        let mut examine = |version: &Version| {
            let visible = visible(version, view);
            if let Some(examined) = examined.as_deref_mut() {
                examined.push(ssi::Examined {
                    tid: version.tid,
                    xmin: version.xmin,
                    xmax: version.xmax,
                    visible,
                });
            }
            visible
        };
        // A table with its versions in memory and a map of the key needs no change for a key with at most one version, so the read lock is sufficient. A key with more versions prunes its list under the write lock.
        {
            let state = self.state_read();
            let Some(table) = state.tables.0.get(&(view.database, relation)) else {
                return Some(Vec::new());
            };
            let ready = table.cold.is_none() && table.frozen.is_none();
            let map = table.keys.iter().find(|map| {
                !map.stale
                    && map.derived.is_none()
                    && map.columns == columns
                    && map.types == types
                    && map.nulls_not_distinct == nulls_not_distinct
            });
            if ready && let Some(map) = map {
                let Some(hash) = map.hash_key(key) else {
                    return Some(Vec::new());
                };
                let found = map.hashes.get(&hash).map_or(&[][..], Tids::as_slice);
                if let [] | [_] = found {
                    return Some(
                        found
                            .iter()
                            .filter_map(|&tid| table.find(tid).map(|at| &table.versions[at]))
                            .filter(|version| examine(version))
                            .map(|version| each(version, &table.combos))
                            .collect(),
                    );
                }
            }
        }
        let mut guard = self.state();
        let state = &mut *guard;
        let horizon = {
            let _published = self.published();
            state.horizon().min(view.snapshot.xmin())
        };
        // A borrow of the set and not a clone of the `Arc`, as the count of the `Arc` is on a line of the cache that the other connections also write.
        let running = &*state.running;
        let Some(table) = state.tables.0.get_mut(&(view.database, relation)) else {
            return Some(Vec::new());
        };
        table.thaw_read();
        table.keys.retain(|map| !map.stale);
        let kept = table.keys.iter().position(|map| {
            map.derived.is_none()
                && map.columns == columns
                && map.types == types
                && map.nulls_not_distinct == nulls_not_distinct
        });
        let place = match kept {
            Some(place) => place,
            None => {
                let unique = Unique {
                    oid: 0,
                    name: String::new(),
                    columns: columns.to_vec(),
                    types: types.to_vec(),
                    shown: String::new(),
                    nulls_not_distinct,
                    derived: None,
                    deferrable: None,
                };
                *table.key_maps(std::slice::from_ref(&unique))?.first()?
            }
        };
        let Some(hash) = table.keys[place].hash_key(key) else {
            return Some(Vec::new());
        };
        table.prune_key(place, hash, (horizon, running));
        let found = table.keys[place].hashes.get(&hash).map_or(&[][..], Tids::as_slice);
        // A key has one version in most cases, so only more than one version needs a sorted copy of the list.
        let mut sorted = Vec::new();
        let tids = if found.len() > 1 {
            sorted.extend_from_slice(found);
            sorted.sort_unstable();
            sorted.dedup();
            &sorted[..]
        } else {
            found
        };
        Some(
            tids.iter()
                .filter_map(|&tid| table.find(tid).map(|at| &table.versions[at]))
                .filter(|version| examine(version))
                .map(|version| each(version, &table.combos))
                .collect(),
        )
    }

    /// The values of the versions `tids` of a table, also of a version that a transaction deleted, in the order of the list. A version that is not in the table gives no row.
    pub(crate) fn versions(&self, database: u32, relation: u32, tids: &[u64]) -> Vec<Vec<Value>> {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        tids.iter()
            .filter_map(|&tid| table.find(tid).map(|at| table.versions[at].values.to_vec()))
            .collect()
    }

    /// True when a command of the transaction of `view` after the command of `view` changed the version `tid` of a table: `TM_SelfModified` with a `cmax` that is not the command of `view`.
    pub(crate) fn modified_later(&self, view: &View, relation: u32, tid: u64) -> bool {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return false;
        };
        table.find(tid).is_some_and(|at| {
            let version = &table.versions[at];
            view.xid != 0 && version.xmax == view.xid && version.cmax != view.command
        })
    }

    /// `heap_get_latest_tid`: the last version of the chain of updates from the version `tid` of a table, as the transaction of `view` sees the updates of its own and of the transactions that committed.
    pub(crate) fn latest_tid(&self, view: &View, relation: u32, mut tid: u64) -> u64 {
        let state = self.state();
        let Some(table) = state.tables.meta(&(view.database, relation)) else {
            return tid;
        };
        while let Some(version) = table.find(tid).map(|at| &table.versions[at]) {
            let done = version.xmax == view.xid || !state.running.contains(&version.xmax);
            if matches!(version.next, 0 | MOVED) || version.xmax == 0 || !done {
                break;
            }
            tid = version.next;
        }
        tid
    }

    /// For each version of a table that the command `command` of the transaction `xid` added, in the order of the inserts, true if the transaction did not delete or update it after, as `SnapshotSelf` sees it.
    pub(crate) fn live(&self, database: u32, relation: u32, xid: u32, command: u32) -> Vec<bool> {
        let state = self.state();
        let Some(table) = state.tables.meta(&(database, relation)) else {
            return Vec::new();
        };
        let mut made: Vec<(u64, bool)> = table
            .versions
            .iter()
            .filter(|version| version.xmin == xid && version.cmin == command)
            .map(|version| (version.made, version.xmax != xid))
            .collect();
        made.sort_unstable_by_key(|&(made, _)| made);
        made.into_iter().map(|(_, live)| live).collect()
    }

    /// Adds the rows that the command of the view inserted, and checks each new version for each unique key. When a key conflicts, no row is added, and the result tells the statement to wait or gives the duplicate key. `kept` has the IDs of the new versions of an earlier try that waited.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn insert(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        unique: (&[Unique], Derive<'_>),
        kept: &[u64],
    ) -> Result<Changed> {
        self.insert_ids(relation, view, rows, unique, kept, &mut Vec::new())
    }

    /// `CopyMultiInsertBufferFlush`: [`Heap::insert`] of the rows of a copy, each with the length of its line in `lines`, and with the lengths and the lines of the rows after them in the batch in `ahead`, which places the new versions in the batches of the copy, as `heap_multi_insert` does with the `BulkInsertState` of the copy. `use_fsm` is false for a table that the transaction created or gave a new file. After a wait for a key, the rows after the row that waited get new places.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn insert_copy(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        lines: CopyLines<'_>,
        unique: (&[Unique], Derive<'_>),
        (kept, use_fsm): (&[u64], bool),
    ) -> Result<Changed> {
        let copy = Some((lines, use_fsm));
        self.insert_rows(relation, view, rows, unique, (kept, copy), &mut Vec::new())
    }

    /// The length of the version of the row `values` of the table `key` after the toaster, as [`Heap::insert_copy`] places it.
    pub(crate) fn toasted_len(&self, key: (u32, u32), values: &[Value]) -> Option<usize> {
        let state = self.state();
        let table = state.tables.meta(&key)?;
        Some(max_align(toast(values, &table.fixed, &table.storage, None).len))
    }

    /// [`Heap::insert`] that gives in `ids` the ID of the new version of each row, for `ctid` of `RETURNING`.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn insert_ids(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        (unique, derive): (&[Unique], Derive<'_>),
        kept: &[u64],
        ids: &mut Vec<u64>,
    ) -> Result<Changed> {
        self.insert_rows(relation, view, rows, (unique, derive), (kept, None), ids)
    }

    /// [`Heap::insert_ids`], and [`Heap::insert_copy`] with the lengths of the lines of the rows of a copy and `use_fsm` in `copy`.
    fn insert_rows(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        (unique, derive): (&[Unique], Derive<'_>),
        (kept, copy): (&[u64], Option<(CopyLines<'_>, bool)>),
        ids: &mut Vec<u64>,
    ) -> Result<Changed> {
        // `heap_insert` checks the lock of the table, and the insert into each index the lock of its page.
        let mut ssi = self.serializable_lock(view);
        conflict_in(
            ssi.as_deref_mut(),
            view,
            relation,
            Some(&inserted((view.database, relation))),
        )?;
        ids.clear();
        let count = rows.len() as u64;
        let made = (view.xid, view.command);
        if unique.is_empty() {
            let mut state = self.state();
            let State { tables, toast_oids, .. } = &mut *state;
            let table = tables.append((view.database, relation));
            if let Some((lines, use_fsm)) = copy {
                let places = table.place_copy(made, &rows, lines, use_fsm, toast_oids);
                for (values, (tid, len)) in rows.into_iter().zip(places) {
                    ids.push(table.put(tid, made, values, (unique, &[]), len));
                }
            } else {
                for values in rows {
                    ids.push(table.add(None, made, values, (unique, &[]), None, toast_oids));
                }
            }
            // A large load moves the values of its versions into columns as it goes, so the rows of the load are not all in memory at once.
            table.freeze_load(self.tasks.as_deref());
            state.wrote_ids(view.xid, (view.database, relation), ids);
            state.loaded(view.xid, (view.database, relation));
            return Ok(Changed::Done(count));
        }
        let mut keys = Vec::with_capacity(rows.len());
        for values in &rows {
            keys.push(fresh(unique, values, derive)?);
        }
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State { tables, running, rechecks: pending, toast_oids, .. } = &mut *state;
        let table = tables.entry((view.database, relation));
        // The rows of a copy take their places as a batch before the checks of their keys, as `heap_multi_insert` comes before `ExecInsertIndexTuples`. A try after a wait takes the places of all the rows of the first try.
        let planned = match copy {
            Some((lines, use_fsm)) if kept.is_empty() => {
                table.place_copy(made, &rows, lines, use_fsm, toast_oids)
            }
            _ => Vec::new(),
        };
        let tids: Vec<u64> = planned.iter().map(|&(tid, _)| tid).collect();
        let mut placed = planned.into_iter();
        let mut kept = kept.iter().copied();
        let mut used = Vec::with_capacity(rows.len());
        let mut partial = Vec::new();
        for (values, keys) in rows.into_iter().zip(&keys) {
            used.push(match placed.next() {
                Some((tid, len)) => table.put(tid, made, values, (unique, keys), len),
                None => table.add(kept.next(), made, values, (unique, keys), None, toast_oids),
            });
            let checked = (view.xid, &**running);
            match table.check_keys((&places, unique), &used, checked, &mut partial) {
                Ok(None) => {}
                result => {
                    table.undo(&used, &[]);
                    return result.map(|conflict| match conflict {
                        Some(Changed::WaitKey { holder, index, waits, .. }) if !tids.is_empty() => {
                            Changed::WaitKey { holder, index, used: tids, waits }
                        }
                        conflict => conflict.unwrap_or(Changed::Done(0)),
                    });
                }
            }
        }
        pending.extend(rechecks(view, relation, unique, partial));
        state.wrote_ids(view.xid, (view.database, relation), &used);
        *ids = used;
        Ok(Changed::Done(count))
    }

    /// For each version of `tids` that is in the table, in the order of [`Heap::versions`], true when the transaction `xid` inserted it, as `slot_is_current_xact_tuple` tells.
    pub(crate) fn made_by(
        &self,
        database: u32,
        relation: u32,
        tids: &[u64],
        xid: u32,
    ) -> Vec<bool> {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        tids.iter()
            .filter_map(|&tid| table.find(tid).map(|at| table.versions[at].xmin == xid))
            .collect()
    }

    /// The end of the waits of the transaction `xid` for rows, after a lock that `SKIP LOCKED` or `NOWAIT` did not wait for.
    pub(crate) fn unqueue_rows(&self, xid: u32) {
        let mut state = self.state();
        unqueue_rows(&mut state.row_queues, xid, None);
    }

    /// `ExecDelete` with `changingPart`: the versions `tids` of the table `relation`, which the transaction `xid` deleted to move their rows to another partition, are marked as moved, so that a write or a row lock that waited for them fails.
    pub(crate) fn mark_moved(&self, database: u32, relation: u32, xid: u32, tids: &[u64]) {
        let mut state = self.state();
        let Some(table) = state.tables.get_mut(&(database, relation)) else { return };
        for &tid in tids {
            if let Some(at) = table.find(tid) {
                let version = &mut table.versions[at];
                if version.xmax == xid && version.next == 0 {
                    version.next = MOVED;
                }
            }
        }
    }

    /// The ID of the version after each version of `tids`, which an `UPDATE` made, or 0 for a version with no next version.
    pub(crate) fn next_versions(&self, database: u32, relation: u32, tids: &[u64]) -> Vec<u64> {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.get(&(database, relation)) else {
            return vec![0; tids.len()];
        };
        let next = |at: usize| Some(table.versions[at].next).filter(|&next| next != MOVED);
        tids.iter().map(|&tid| table.find(tid).and_then(next).unwrap_or(0)).collect()
    }

    /// `ExecCheckIndexConstraints` and the speculative insertion of `ExecInsert` for one row of `INSERT ... ON CONFLICT`. The row goes in when no version that a dirty snapshot sees has its key in an arbiter index, the indexes at the places `arbiters` of `unique`. The check and the insertion hold the lock of the heap, so no other write can come between them. `kept` is the ID of the version of an earlier try that waited.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn speculative(
        &self,
        relation: u32,
        view: &View,
        values: Vec<Value>,
        (unique, derive): (&[Unique], Derive<'_>),
        arbiters: &[usize],
        kept: Option<u64>,
    ) -> Result<Speculative> {
        let mut ssi = self.serializable_lock(view);
        conflict_in(
            ssi.as_deref_mut(),
            view,
            relation,
            Some(&inserted((view.database, relation))),
        )?;
        let keys = fresh(unique, &values, derive)?;
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State { tables, running, rechecks: pending, toast_oids, .. } = &mut *state;
        let table = tables.entry((view.database, relation));
        for &arbiter in arbiters {
            let Some(&place) = places.get(arbiter) else { continue };
            let map = &table.keys[place];
            let key = match &map.derived {
                None => Some(values.as_slice()),
                Some(_) => keys.get(arbiter).and_then(|key| key.as_deref()),
            };
            let Some(places) = key else {
                continue;
            };
            let Some(hash) = map.hash(places) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                let Some(found) = table.find(other) else {
                    continue;
                };
                let version = &table.versions[found];
                let seen = dirty(version, view.xid, running);
                if seen == Seen::Dead {
                    continue;
                }
                match map.key(other, &version.values) {
                    Some(found) if map.same(places, found)? => {}
                    _ => continue,
                }
                if let Seen::Wait(holder) = seen {
                    return Ok(Speculative::Wait { holder, tid: other });
                }
                let own = version.xmin == view.xid;
                let in_snapshot = visible(version, view);
                // `ExecCheckTIDVisible` and `ExecOnConflictUpdate` fetch the version, which takes its `SIREAD` lock. A version that the snapshot does not see fails first with a concurrent update.
                if let Some(ssi) = ssi.as_deref_mut().filter(|_| in_snapshot || own) {
                    let examined = [ssi::Examined {
                        tid: other,
                        xmin: version.xmin,
                        xmax: version.xmax,
                        visible: in_snapshot,
                    }];
                    let snapshot = &view.snapshot;
                    let concurrent = |xid: u32| !committed_before(xid, snapshot);
                    let table = ((view.database, relation), view.xid);
                    ssi.fetched(view.sxact, table, &examined, &concurrent)?;
                }
                return Ok(Speculative::Conflict {
                    tid: other,
                    values: version.values.to_vec(),
                    own,
                    command: own && version.cmin == view.command,
                    visible: in_snapshot,
                });
            }
        }
        let used =
            [table.add(kept, (view.xid, view.command), values, (unique, &keys), None, toast_oids)];
        let mut partial = Vec::new();
        let checked = (view.xid, &**running);
        match table.check_keys((&places, unique), &used, checked, &mut partial) {
            Ok(None) => {
                pending.extend(rechecks(view, relation, unique, partial));
                state.wrote_ids(view.xid, (view.database, relation), &used);
                Ok(Speculative::Inserted(used[0]))
            }
            result => {
                table.undo(&used, &[]);
                Ok(Speculative::Key(result?.unwrap_or(Changed::Done(0))))
            }
        }
    }

    /// `heap_lock_tuple` for the transaction `xid`: a lock of the strength `strength` on the version `tid` of the table `relation`, which the snapshot of the view sees. A lock that the transaction holds with a weaker strength becomes stronger.
    ///
    /// # Errors
    ///
    /// `40001` in `REPEATABLE READ` and `SERIALIZABLE` for a row that a transaction that committed after the snapshot changed.
    pub(crate) fn lock_row(
        &self,
        (relation, tid): (u32, u64),
        view: &View,
        (xid, strength): (u32, RowStrength),
    ) -> Result<RowLocked> {
        let mut state = self.state();
        let locked = Self::lock_row_in(&mut state, (relation, tid), view, (xid, strength));
        let State { row_queues, row_locks, .. } = &mut *state;
        match locked {
            Ok(RowLocked::Wait(holder)) => {
                let row = (view.database, relation, tid);
                let queues = (row_queues, &*row_locks);
                Ok(RowLocked::Wait(queue_row(queues, row, (xid, strength), holder)))
            }
            locked => {
                unqueue_rows(row_queues, xid, None);
                locked
            }
        }
    }

    /// [`Heap::lock_row`] with no tuple lock.
    fn lock_row_in(
        state: &mut State,
        (relation, tid): (u32, u64),
        view: &View,
        (xid, strength): (u32, RowStrength),
    ) -> Result<RowLocked> {
        let State { tables, running, row_locks, multixacts, .. } = state;
        let key = (view.database, relation);
        // A foreign table has no versions in the heap, and its rows take no lock.
        let Some(table) = tables.get_mut(&key) else { return Ok(RowLocked::Done) };
        let Some(at) = table.find(tid) else { return Ok(RowLocked::Done) };
        let version = &table.versions[at];
        // `heap_lock_tuple`: `FOR KEY SHARE` goes on over an update that kept the key, and `heap_lock_updated_tuple` locks the versions of the update chain too, so that a delete or an update of the key of a later version waits for the lock.
        if strength == KEY_SHARE && version.key_kept && version.next != 0 && version.xmax != xid {
            let mut chain = vec![tid];
            let mut version = version;
            while version.xmax != 0 && version.xmax != xid {
                if version.key_kept && version.next != 0 {
                    let Some(next) = table.find(version.next) else { break };
                    chain.push(version.next);
                    version = &table.versions[next];
                    continue;
                }
                // A delete or an update of the key conflicts with the lock.
                return match version.xmax {
                    xmax if running.contains(&xmax) => Ok(RowLocked::Wait(xmax)),
                    _ if view.serializable => Err(Error::new(
                        SqlState::T_R_SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    )),
                    _ if version.next == MOVED => Err(moved_error()),
                    _ => Ok(RowLocked::Again),
                };
            }
            for &tid in &chain {
                if let Some(holder) =
                    row_holder((&*row_locks, &**running), key, tid, (xid, strength))
                {
                    return Ok(RowLocked::Wait(holder));
                }
            }
            for tid in chain {
                let lockers = row_locks.entry((key.0, key.1, tid)).or_default();
                add_row_lock((lockers, &mut *multixacts), (xid, strength, view.command));
            }
            return Ok(RowLocked::Done);
        }
        match version.xmax {
            0 => {}
            xmax if xmax == xid => return Ok(RowLocked::Gone),
            xmax if running.contains(&xmax) => return Ok(RowLocked::Wait(xmax)),
            _ if view.serializable => {
                let what = if version.next == 0 { "delete" } else { "update" };
                return Err(Error::new(
                    SqlState::T_R_SERIALIZATION_FAILURE,
                    format!("could not serialize access due to concurrent {what}"),
                ));
            }
            _ if table.chain_moved(at, running) => return Err(moved_error()),
            _ => return Ok(RowLocked::Again),
        }
        if let Some(holder) = row_holder((&*row_locks, &**running), key, tid, (xid, strength)) {
            return Ok(RowLocked::Wait(holder));
        }
        let lockers = row_locks.entry((key.0, key.1, tid)).or_default();
        add_row_lock((lockers, multixacts), (xid, strength, view.command));
        Ok(RowLocked::Done)
    }

    /// `heap_update` and `heap_delete` for the rows that the command of the view changes. A row that the command changed before stays as it is, as for a row that a join gives more than once. When another transaction changed a row after the snapshot, nothing changes, and the result tells the statement to wait or to run again. When the change is done, `applied` has the index in `changes` of each row that changed, for `RETURNING`.
    ///
    /// # Errors
    ///
    /// `40001` in `REPEATABLE READ` and `SERIALIZABLE` for a row that a transaction that committed after the snapshot changed.
    pub(crate) fn change(
        &self,
        relation: u32,
        view: &View,
        changes: Vec<Change>,
        keys: (&[Unique], &[usize], Derive<'_>),
        kept: &[u64],
        applied: &mut Vec<usize>,
    ) -> Result<Changed> {
        let changed = self.change_rows(relation, view, changes, keys, kept, applied);
        if !matches!(changed, Ok(Changed::Wait { .. })) {
            let mut state = self.state();
            unqueue_rows(&mut state.row_queues, view.xid, None);
        }
        changed
    }

    /// The `heap_update` and `heap_delete` of [`Heap::change`], which leaves the queues of the rows that it waited for to the caller.
    fn change_rows(
        &self,
        relation: u32,
        view: &View,
        changes: Vec<Change>,
        (unique, periods, derive): (&[Unique], &[usize], Derive<'_>),
        kept: &[u64],
        applied: &mut Vec<usize>,
    ) -> Result<Changed> {
        let mut ssi = self.serializable_lock(view);
        applied.clear();
        let mut keys = Vec::with_capacity(changes.len());
        for (_, values) in &changes {
            keys.push(match values {
                Some(values) => fresh(unique, values, derive)?,
                None => Vec::new(),
            });
        }
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State {
            tables,
            running,
            rechecks: pending,
            toast_oids,
            combos,
            row_locks,
            row_queues,
            ..
        } = &mut *state;
        let Some(table) = tables.get_mut(&(view.database, relation)) else {
            return Ok(Changed::Done(0));
        };
        let mut found: Vec<(u64, Option<Vec<Value>>, Fresh, bool)> =
            Vec::with_capacity(changes.len());
        let mut indexes = Vec::with_capacity(changes.len());
        let mut targets = Vec::new();
        for (index, ((tid, values), keys)) in changes.into_iter().zip(keys).enumerate() {
            let Ok(at) = table.versions.binary_search_by_key(&tid, |version| version.tid) else {
                continue;
            };
            let version = &table.versions[at];
            // `heap_update` and `heap_delete` wait for the transactions that lock the row with a strength that conflicts: a `DELETE` and an `UPDATE` of a key column take `UPDATE`, and the other updates take `NO KEY UPDATE`.
            let key_kept = values
                .as_ref()
                .is_some_and(|new| !changes_key(unique, periods, &version.values, new));
            let strength = if key_kept { NO_KEY_UPDATE } else { UPDATE };
            let row = (view.database, relation, tid);
            match version.xmax {
                0 => {}
                // The command changed the row before, as for a row that a join gives more than once.
                xmax if xmax == view.xid && version.cmax == view.command => continue,
                xmax if xmax == view.xid => {
                    return Ok(Changed::SelfModified { update: values.is_some() });
                }
                xmax if running.contains(&xmax) => {
                    let queues = (&mut *row_queues, &*row_locks);
                    let holder = queue_row(queues, row, (view.xid, strength), xmax);
                    return Ok(Changed::Wait { holder, tid });
                }
                _ if view.serializable => {
                    let what = if version.next == 0 { "delete" } else { "update" };
                    return Err(Error::new(
                        SqlState::T_R_SERIALIZATION_FAILURE,
                        format!("could not serialize access due to concurrent {what}"),
                    ));
                }
                _ if table.chain_moved(at, running) => return Err(moved_error()),
                _ => return Ok(Changed::Again),
            }
            if !row_locks.is_empty() {
                let locks = (&*row_locks, &**running);
                let key = (view.database, relation);
                if let Some(holder) = row_holder(locks, key, tid, (view.xid, strength)) {
                    let queues = (&mut *row_queues, &*row_locks);
                    let holder = queue_row(queues, row, (view.xid, strength), holder);
                    return Ok(Changed::Wait { holder, tid });
                }
            }
            if found.iter().any(|(old, ..)| *old == tid) {
                continue;
            }
            if ssi.is_some() {
                // `heap_update` and `heap_delete` check the locks of the version, its page and the table, and an update that is not HOT inserts into each index.
                let table = (view.database, relation);
                targets
                    .extend([ssi::Target::Tuple(table, tid), ssi::Target::Page(table, tid >> 16)]);
                if values.is_some() && !key_kept {
                    targets.push(ssi::Target::Index(table));
                }
            }
            found.push((tid, values, keys, key_kept));
            indexes.push(index);
        }
        if ssi.is_some() {
            targets.push(ssi::Target::Relation((view.database, relation)));
            targets.sort_unstable();
            targets.dedup();
            conflict_in(ssi.as_deref_mut(), view, relation, Some(&targets))?;
        }
        let count = found.len() as u64;
        let mut kept = kept.iter().copied();
        let mut used = Vec::new();
        let mut deleted = Vec::with_capacity(found.len());
        let mut made_ids = Vec::with_capacity(found.len());
        let mut partial = Vec::new();
        for (old, values, keys, key_kept) in found {
            let made = (view.xid, view.command);
            table.pages.touch(page_of(old));
            let next = values.map_or(0, |v| {
                table.add(kept.next(), made, v, (unique, &keys), Some(old), toast_oids)
            });
            // `compute_new_xmax_infomask`: the new version keeps the locks of the transactions in progress, so a `FOR KEY SHARE` holds after an `UPDATE` of the other columns. The old version keeps them too, for a rollback of the update.
            if next != 0 && !row_locks.is_empty() {
                let (database, relation) = (view.database, relation);
                if let Some(lockers) = row_locks.get(&(database, relation, old)) {
                    let kept: Vec<_> =
                        lockers.iter().copied().filter(|(x, _, _)| running.contains(x)).collect();
                    if !kept.is_empty() {
                        row_locks.insert((database, relation, next), kept);
                    }
                }
            }
            if let Some(at) = table.find(old) {
                let version = &mut table.versions[at];
                version.xmax = view.xid;
                version.cmax = view.command;
                version.next = next;
                version.key_kept = key_kept;
                // `HeapTupleHeaderAdjustCmax`: a version that the transaction inserted keeps a combo command ID of its `cmin` and `cmax` in `t_cid`.
                if version.xmin == view.xid {
                    let pairs = combos.entry(view.xid).or_default();
                    let next_id = u32::try_from(pairs.len()).unwrap_or(u32::MAX);
                    let id = *pairs.entry((version.cmin, view.command)).or_insert(next_id);
                    table.combos.insert(old, id);
                } else if !table.combos.is_empty() {
                    table.combos.remove(&old);
                }
            }
            deleted.push(old);
            if next != 0 {
                made_ids.push(next);
            }
            if next == 0 || places.is_empty() {
                continue;
            }
            used.push(next);
            let checked = (view.xid, &**running);
            match table.check_keys((&places, unique), &used, checked, &mut partial) {
                Ok(None) => {}
                result => {
                    table.undo(&used, &deleted);
                    return result.map(|conflict| conflict.unwrap_or(Changed::Done(0)));
                }
            }
        }
        pending.extend(rechecks(view, relation, unique, partial));
        deleted.extend(made_ids);
        state.wrote_ids(view.xid, (view.database, relation), &deleted);
        *applied = indexes;
        Ok(Changed::Done(count))
    }

    /// The rows that `TRUNCATE` removes. Each version that no transaction deleted gets the transaction of the view as `xmax`, so that a rollback brings the rows back, as the old file of `RelationSetNewRelfilenumber` stays until the commit. `TRUNCATE` takes `AccessExclusiveLock`, which waits for each transaction that changed the table. So when another transaction in progress inserted or deleted a version, nothing changes, and the result is that transaction, for the statement to wait for.
    pub(crate) fn truncate(&self, relation: u32, view: &View) -> Option<u32> {
        let mut state = self.state();
        state.wrote(view.xid, (view.database, relation));
        let State { tables, running, truncating, .. } = &mut *state;
        let table = tables.get_mut(&(view.database, relation))?;
        if let Some(holder) = table.holder(view.xid, running) {
            return Some(holder);
        }
        for version in table.versions.iter_mut().filter(|version| version.xmax == 0) {
            version.xmax = view.xid;
            version.cmax = view.command;
        }
        let new = table.pages.renewed();
        let old = std::mem::replace(&mut table.pages, new);
        if table.truncated.is_empty() {
            truncating.entry(view.xid).or_default().push((view.database, relation));
        }
        table.truncated.push((view.xid, view.command, old));
        None
    }

    /// Marks the statement of `view` as the bulk insert of `CREATE TABLE AS` into the table `relation`, which extends the heap by more pages at a time, as a `BulkInsertState` does.
    pub(crate) fn start_bulk(&self, relation: u32, view: &View) {
        let mut state = self.state();
        let table = state.tables.settings((view.database, relation));
        table.bulk = Some((view.xid, view.command));
    }

    /// `RelationGetNumberOfBlocks` of the heap of a table and of its free space map and visibility map, and `relpages`, `reltuples`, `relallvisible` and `relallfrozen` of the table.
    pub(crate) fn size(&self, database: u32, relation: u32) -> HeapSize {
        let state = self.state();
        let pages = state.tables.meta(&(database, relation)).map(|table| &table.pages);
        let (fsm_blocks, vm_blocks) = pages.map_or((0, 0), Pages::fork_blocks);
        HeapSize {
            blocks: pages.map_or(0, Pages::blocks),
            relpages: pages.map_or(0, |p| p.relpages),
            reltuples: pages.map_or(-1.0, |p| p.reltuples),
            relallvisible: pages.map_or(0, |p| p.relallvisible),
            relallfrozen: pages.map_or(0, |p| p.relallfrozen),
            fsm_blocks,
            vm_blocks,
        }
    }

    /// `RelationGetNumberOfBlocks` and `_bt_getrootheight` of the `btree` index `index` of the shape `shape` on a table, from the versions of the table that a transaction that did not abort inserted, as `CREATE INDEX` would build the index now. A table with more versions than [`btree::EXACT`] gives the size of a sample, which serves until the versions change by a 32nd. The values of the sample are read with the heap locked and sorted with the heap not locked.
    pub(crate) fn btree_size(
        &self,
        database: u32,
        relation: u32,
        index: u32,
        shape: &Shape,
    ) -> BtreeSize {
        let (entries, total, made, count) = {
            let mut guard = self.state();
            let state = &mut *guard;
            // The read keeps the frozen columns of the table, as the plan of each query reads the size of each index, and a thaw would make the next scan freeze the table again.
            let Some(table) = state.tables.meta(&(database, relation)) else {
                return BtreeSize::EMPTY;
            };
            let count = table.len();
            if let Some(cached) = table.btree_sizes.get(&index)
                && cached.shape == *shape
                && (cached.made == table.made && cached.count == count
                    || count > btree::EXACT && cached.count.abs_diff(count) < cached.count / 32)
            {
                return cached.size;
            }
            let step = count.div_ceil(btree::EXACT).max(1);
            let mut total = 0;
            let mut entries: Vec<Box<[Value]>> = Vec::new();
            table.each_version_row(Some(&shape.columns), |version, values| {
                if state.aborted.contains(&version.xmin) {
                    return;
                }
                if total % step == 0 {
                    entries.push(
                        shape
                            .columns
                            .iter()
                            .map(|&c| values.get(c).cloned().unwrap_or(Value::Null))
                            .collect(),
                    );
                }
                total += 1;
            });
            (entries, total, table.made, count)
        };
        let size = btree::size(entries, total, shape);
        let mut state = self.state();
        if let Some(table) = state.tables.settings_of(&(database, relation)) {
            let cached = btree::Cached { made, count, shape: shape.clone(), size };
            table.btree_sizes.insert(index, cached);
        }
        size
    }

    /// The size of a `btree` index whose build put `entries` in it, the keys of an index with expressions or a predicate. The columns of `shape` are places in the entries.
    pub(crate) fn btree_size_of(entries: Vec<Box<[Value]>>, shape: &Shape) -> BtreeSize {
        let total = entries.len();
        btree::size(entries, total, shape)
    }

    /// The number of live versions of a table for `ANALYZE` and `VACUUM`, as `heapam_scan_analyze_next_tuple` counts them for the transaction `xid`: the versions that a transaction that committed or `xid` inserted, and that no transaction deleted, or that a transaction in progress other than `xid` deletes.
    pub(crate) fn live_count(&self, database: u32, relation: u32, xid: u32) -> u64 {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.meta(&(database, relation)) else { return 0 };
        let done = |x: u32| x == xid || !state.running.contains(&x);
        let mut count = 0;
        table.each_version(|v| {
            if done(v.xmin) && (v.xmax == 0 || (v.xmax != xid && state.running.contains(&v.xmax))) {
                count += 1;
            }
        });
        count
    }

    /// `acquire_sample_rows` of `ANALYZE` for the transaction `xid`: the values of the live versions of a table, as [`Heap::live_count`] counts them, in the order of their places in the heap, and the number of the live versions. A table with more than `target` live versions gives a random sample of `target` of them, as PostgreSQL does, with a seed from the table so that each `ANALYZE` of the same rows gives the same sample.
    pub(crate) fn sample(
        &self,
        database: u32,
        relation: u32,
        xid: u32,
        target: usize,
    ) -> (Vec<Vec<Value>>, f64) {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.meta(&(database, relation)) else {
            return (Vec::new(), 0.0);
        };
        let done = |x: u32| x == xid || !state.running.contains(&x);
        let live = |v: &Version| {
            done(v.xmin) && (v.xmax == 0 || (v.xmax != xid && state.running.contains(&v.xmax)))
        };
        let mut total = 0;
        table.each_version(|v| total += usize::from(live(v)));
        // The places among the live versions of the versions in the sample, in order.
        let chosen = sample_places(total, target, relation);
        let picks = || chosen.iter().copied();
        // The places of the versions of the sample among all the versions, for a read of only their rows.
        let mut places = Vec::with_capacity(total.min(target));
        let (mut wanted, mut at, mut place) = (picks(), 0, 0);
        let mut next = wanted.next();
        table.each_version(|v| {
            if live(v) {
                if next == Some(at) {
                    places.push(place);
                    next = wanted.next();
                }
                at += 1;
            }
            place += 1;
        });
        if let Some(rows) = table.rows_at(&places, None) {
            return (rows, total as f64);
        }
        let mut picks = picks();
        let mut next = picks.next();
        let mut at = 0;
        let mut rows = Vec::with_capacity(total.min(target));
        table.each_version_row(None, |v, values| {
            if !live(v) {
                return;
            }
            if next == Some(at) {
                rows.push(values.to_vec());
                next = picks.next();
            }
            at += 1;
        });
        (rows, total as f64)
    }

    /// `get_actual_variable_endpoint`: the least and the greatest value that is not null of the column at `column` of a table, in the order `order`, of the versions that `SnapshotNonVacuumable` sees. A version is dead when a transaction that ended deleted it before the horizon of the running transactions and of the snapshots, as `HeapTupleSatisfiesVacuum` finds it.
    pub(crate) fn variable_range(
        &self,
        database: u32,
        relation: u32,
        column: usize,
        order: impl Fn(&Value, &Value) -> Option<Ordering>,
    ) -> Option<(Value, Value)> {
        let mut guard = self.state();
        let state = &mut *guard;
        let horizon = {
            let _published = self.published();
            state.horizon()
        };
        // The read keeps the frozen columns of the table, as [`Heap::btree_size`] does.
        let table = state.tables.meta(&(database, relation))?;
        let dead =
            |v: &Version| v.xmax != 0 && !state.running.contains(&v.xmax) && v.xmax < horizon;
        let mut range: Option<(Value, Value)> = None;
        let mut unordered = false;
        table.each_version_row(Some(&[column]), |version, values| {
            let Some(value) = values.get(column).filter(|v| !v.is_null() && !dead(version)) else {
                return;
            };
            match &mut range {
                None => range = Some((value.clone(), value.clone())),
                Some((min, max)) => match (order(value, min), order(value, max)) {
                    (Some(low), Some(high)) => {
                        if low.is_lt() {
                            *min = value.clone();
                        }
                        if high.is_gt() {
                            *max = value.clone();
                        }
                    }
                    _ => unordered = true,
                },
            }
        });
        if unordered { None } else { range }
    }

    /// `update_attstats`: replaces the rows of `pg_statistic` of a table for the columns of `rows`, outside of the transactions.
    pub(crate) fn set_statistics(&self, database: u32, relation: u32, rows: Vec<Statistic>) {
        let mut state = self.state();
        state.unlogged += 1;
        let table = state.tables.settings((database, relation));
        for row in rows {
            table.statistics.insert((row.attnum, row.inherit), row);
        }
        table.own_statistics = None;
    }

    /// The rows of `pg_statistic` of a table with `stainherit` false, by `staattnum`. The plans share them until `ANALYZE` writes new rows, so that a plan does not copy the values of the statistics.
    pub(crate) fn own_statistics(
        &self,
        database: u32,
        relation: u32,
    ) -> Arc<BTreeMap<i16, Statistic>> {
        let mut state = self.state();
        let Some(table) = state.tables.0.get_mut(&(database, relation)) else {
            return Arc::default();
        };
        let own = table.own_statistics.get_or_insert_with(|| {
            let own = table.statistics.values().filter(|s| !s.inherit);
            Arc::new(own.map(|s| (s.attnum, s.clone())).collect())
        });
        Arc::clone(own)
    }

    /// The rows of `pg_statistic` of a table, in the order of `staattnum` and `stainherit`.
    pub(crate) fn statistics(&self, database: u32, relation: u32) -> Vec<Statistic> {
        let state = self.state();
        state
            .tables
            .meta(&(database, relation))
            .map(|table| table.statistics.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Sets `attlen` and `attalign` of the columns of the table `relation` with a type of fixed length, which the new versions take in their pages, as `heap_compute_data_size` stores them.
    pub(crate) fn set_layout(
        &self,
        database: u32,
        relation: u32,
        (fixed, storage): (Vec<Fixed>, Vec<Storage>),
        fillfactor: u8,
    ) {
        let mut state = self.state();
        let table = state.tables.settings((database, relation));
        table.save_free = 8192 * usize::from(100 - fillfactor.min(100)) / 100;
        if table.fixed != fixed {
            table.fixed = fixed;
        }
        if table.storage != storage {
            table.storage = storage;
        }
    }

    /// The storage of the columns of the table `relation` that [`Heap::set_layout`] set, for the widths of the values of `ANALYZE`.
    pub(crate) fn layout(&self, database: u32, relation: u32) -> (Vec<Fixed>, Vec<Storage>) {
        let state = self.state();
        state
            .tables
            .meta(&(database, relation))
            .map(|table| (table.fixed.clone(), table.storage.clone()))
            .unwrap_or_default()
    }

    /// Takes the number of the values that the writes moved to the TOAST tables since the last call. Each one used an OID of the counter of the cluster, as `toast_save_datum` gives it.
    pub(crate) fn take_toast_oids(&self) -> u32 {
        std::mem::take(&mut self.state().toast_oids)
    }

    /// `lazy_vacuum_heap_rel` after the vacuum of the indexes of a table: its `LP_DEAD` items become `LP_UNUSED`.
    pub(crate) fn vacuumed_indexes(&self, database: u32, relation: u32) {
        let mut state = self.state();
        state.unlogged += 1;
        if let Some(table) = state.tables.get_mut(&(database, relation)) {
            table.lp_dead.clear();
        }
    }

    /// The number of the changes that the log does not have, which [`Imaged::unlogged`] gives at an image.
    pub(crate) fn unlogged(&self) -> u64 {
        self.state_read().unlogged
    }

    /// `vac_update_relstats`: sets `relpages` and `reltuples` of a table, outside of the transactions.
    pub(crate) fn set_stats(&self, database: u32, relation: u32, relpages: i32, reltuples: f32) {
        let mut state = self.state();
        state.unlogged += 1;
        state.tables.settings((database, relation)).pages.set_stats(relpages, reltuples);
    }

    /// `heap_vacuum_rel` of the table `relation` for the session `pid`: the versions that a transaction deleted before each snapshot and transaction in progress went, as `HeapTupleSatisfiesVacuum` finds them `HEAPTUPLE_DEAD`, give their space and their line pointers back. The pages that the visibility map marks for `skip` keep their versions. Each page that `VACUUM` scans goes to the free space map, with its bits in the visibility map. `indexed` is true when the table has an index, `freeze` freezes the versions that each snapshot sees, and `truncate` lets the heap lose its empty pages at the end.
    pub(crate) fn vacuum(
        &self,
        (database, relation, pid): (u32, u32, i32),
        skip: Skip,
        (indexed, freeze, truncate): (bool, bool, bool),
    ) -> Vacuumed {
        let mut state = self.state();
        state.unlogged += 1;
        let State { tables, running, xmins, latest_completed, .. } = &mut *state;
        // `GetOldestNonRemovableTransactionId`, without the snapshot of `VACUUM`, as `PROC_IN_VACUUM` leaves it out.
        let published = self.published();
        let horizon = running
            .first()
            .copied()
            .into_iter()
            .chain(held(xmins, Some(pid)))
            .fold((*latest_completed + 1).max(INITDB_NEXT_XID), u32::min);
        drop(published);
        let Some(table) = tables.get_mut(&(database, relation)) else {
            return Vacuumed { reltuples: 0.0, pages: 0, dead_pages: 0 };
        };
        let generation = table.pages.generation;
        let scanned = table.pages.scanned(skip);
        let in_scan = |tid: u64| {
            generation_of(tid) == generation
                && scanned.get(page_of(tid) as usize).copied().unwrap_or(false)
        };
        let dead = |v: &Version| v.xmax != 0 && !running.contains(&v.xmax) && v.xmax < horizon;
        let gone: Vec<u64> =
            table.versions.iter().filter(|v| in_scan(v.tid) && dead(v)).map(|v| v.tid).collect();
        table.lp_dead.retain(|&(g, at)| g == generation && at < table.pages.blocks());
        table.lp_dead.extend(gone.iter().map(|&tid| (generation, page_of(tid))));
        let dead_pages = if indexed { table.lp_dead.len() } else { 0 };
        if !indexed {
            table.lp_dead.clear();
        }
        table.purge(&gone);
        let mut pruned: Vec<Pruned> = scanned
            .iter()
            .map(|_| Pruned { kept: Vec::new(), visible: true, frozen: true })
            .collect();
        let (mut live, mut nonempty) = (0.0, 0);
        for version in &table.versions {
            if generation_of(version.tid) != generation {
                continue;
            }
            let at = page_of(version.tid);
            nonempty = nonempty.max(at + 1);
            if !in_scan(version.tid) {
                continue;
            }
            let page = &mut pruned[at as usize];
            page.kept.push((slot_of(version.tid), version.len));
            let committed = !running.contains(&version.xmin);
            let visible = committed && version.xmin < horizon && version.xmax == 0;
            page.visible &= visible;
            page.frozen &= freeze && visible;
            if committed && (version.xmax == 0 || running.contains(&version.xmax)) {
                live += 1.0;
            }
        }
        for (at, page) in pruned.iter().enumerate().filter(|&(at, _)| scanned[at]) {
            table.pages.prune(at as u32, page, indexed);
        }
        let total = table.pages.vacuumed(&scanned, nonempty, truncate);
        let count = u32::try_from(scanned.iter().filter(|&&s| s).count()).unwrap_or(u32::MAX);
        let reltuples = table.pages.vacuum_stats(total, count, live);
        let dead_pages = u32::try_from(dead_pages).unwrap_or(u32::MAX);
        Vacuumed { reltuples, pages: total, dead_pages }
    }

    /// Records that `xid` waits for `holder`, as `LockAcquire` adds an edge to the wait graph.
    pub(crate) fn wait(&self, xid: u32, holder: u32, row: Option<((u32, u32, u64), RowStrength)>) {
        let mut state = self.state();
        state.waits.insert(xid, holder);
        match row {
            Some(row) => state.row_waits.insert(xid, row),
            None => state.row_waits.remove(&xid),
        };
    }

    /// The end of the wait of `xid`.
    pub(crate) fn waited(&self, xid: u32) {
        let mut state = self.state();
        state.waits.remove(&xid);
        state.row_waits.remove(&xid);
    }

    /// The process IDs of the sessions whose transactions the transactions of the session `pid` wait for.
    ///
    /// A wait for a row whose holder no longer blocks the row, after a rollback to a savepoint of the holder, does not count, though the session did not see it yet, as `LockRelease` grants the lock to the waiter at once.
    pub(crate) fn blockers(&self, pid: i32) -> Vec<i32> {
        let mut state = self.state();
        let mut pids = Vec::new();
        let waits: Vec<(u32, u32)> = state.waits.iter().map(|(&x, &h)| (x, h)).collect();
        for (xid, holder) in waits {
            if state.owners.get(&xid) != Some(&pid) {
                continue;
            }
            if let Some(&(row, strength)) = state.row_waits.get(&xid)
                && !row_blocked(&mut state, row, (holder, strength))
            {
                continue;
            }
            if let Some(&holder) = state.owners.get(&holder)
                && holder != pid
                && !pids.contains(&holder)
            {
                pids.push(holder);
            }
        }
        pids
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

    /// True while the transaction `holder` changed the version `tid` of the table `relation` of the database, or a later version of its update chain, or holds a lock on it that conflicts with `strength`. A rollback to a savepoint takes away the change and the locks of its commands, and a wait for the lock then ends, as for the end of the subtransaction in `XactLockTableWait`.
    pub(crate) fn row_blocked(
        &self,
        row: (u32, u32, u64),
        (holder, strength): (u32, RowStrength),
    ) -> bool {
        row_blocked(&mut self.state(), row, (holder, strength))
    }

    /// True while a transaction is in progress.
    pub(crate) fn running(&self, xid: u32) -> bool {
        self.state().running.contains(&xid)
    }

    /// True if a table has a version of a row, visible or not.
    pub(crate) fn has_rows(&self, database: u32, relation: u32) -> bool {
        let state = self.state();
        state.tables.meta(&(database, relation)).is_some_and(|table| table.len() > 0)
    }

    /// `RecordTransactionCommit`: the changes of the transaction become visible to the new snapshots.
    pub(crate) fn commit(&self, xid: u32) {
        self.commit_logged(xid, &mut |_| {});
    }

    /// [`Heap::commit`] that calls `log` once while the heap is locked, with the bytes of the changes of the transaction when the heap writes logs and the transaction changed a table. So the logs are in the order of the commits, and [`Heap::redo`] of each log after the image of the heap gives the heap of the commits.
    pub(crate) fn commit_logged(&self, xid: u32, log: &mut dyn FnMut(Logged<'_>)) {
        let mut state = self.state();
        let State { tables, truncating, .. } = &mut *state;
        let truncated = truncating.remove(&xid).unwrap_or_default();
        for table in &truncated {
            let Some(table) = tables.get_mut(table) else { continue };
            table.truncated.clear();
            let generation = table.pages.generation;
            table.remove(|version| generation_of(version.tid) != generation);
        }
        match state.touched.remove(&xid) {
            Some(mut touched) => {
                let mut out = std::mem::take(&mut state.log_buffer);
                if state.log_commit(xid, &mut touched, &truncated, &mut out) {
                    log(Logged::Changes(&out));
                } else {
                    log(Logged::Large);
                }
                out.clear();
                if out.capacity() <= LOG_BUFFER {
                    state.log_buffer = out;
                }
            }
            None => log(Logged::Nothing),
        }
        // The open rows of a load become a last segment when the load commits, so the typed values do not stay in memory after it (spec/10 section 10.8). The last pages of the load, which kept their values for new versions, and the versions that the load put into earlier pages go into the columns first, so that the first scan does not open the last segment again or make the columns again. The commit logged the values of these versions, so they can go.
        let tasks = self.tasks.as_deref();
        for key in state.loading.remove(&xid).unwrap_or_default() {
            if let Some(table) = state.tables.settings_of(&key) {
                if table.frozen.is_some()
                    && table.versions.iter().all(|v| v.values.is_empty() || v.xmin == xid)
                    && table.fold(tasks)
                {
                    table.freeze_to(table.versions.len(), tasks);
                }
                table.seal();
            }
        }
        Arc::make_mut(&mut state.running).remove(&xid);
        state.latest_completed = state.latest_completed.max(xid);
        self.publish(&state);
        state.prepared.remove(&xid);
        state.combos.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.row_waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
        state.unlock_rows(xid);
    }

    /// `RecordTransactionAbort`: the rows that the transaction inserted go, and the rows that it deleted stay.
    pub(crate) fn abort(&self, xid: u32) {
        let mut state = self.state();
        let touched = |table: &Table| {
            table.truncated.first().is_some_and(|(x, ..)| *x == xid)
                || table.versions.iter().any(|version| version.xmin == xid || version.xmax == xid)
        };
        for table in state.tables.touched_mut(touched) {
            if table.truncated.first().is_some_and(|(x, ..)| *x == xid) {
                table.pages = table.truncated.swap_remove(0).2;
                table.truncated.clear();
            }
            table.remove(|version| version.xmin == xid);
            // `t_cid` keeps the command of the delete, so `cmax` stays.
            for version in table.versions.iter_mut().filter(|version| version.xmax == xid) {
                version.aborted = xid;
                version.xmax = 0;
                version.next = 0;
            }
        }
        Arc::make_mut(&mut state.running).remove(&xid);
        state.latest_completed = state.latest_completed.max(xid);
        state.aborted.insert(xid);
        self.publish(&state);
        state.prepared.remove(&xid);
        state.combos.remove(&xid);
        state.touched.remove(&xid);
        state.loading.remove(&xid);
        state.truncating.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.row_waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
        state.unlock_rows(xid);
    }

    /// `EndPrepare` for the rows: the transaction `xid`, which stays in progress, becomes a prepared transaction that a restart keeps, with the bytes `meta` of what the log keeps of it. `log` gets once while the heap is locked the bytes of its changes, as [`Heap::commit_logged`] gives them for a commit, and the bytes of its row locks, which a restart takes again. The image keeps its changes from then on, so the log of its commit has no changes. A transaction that truncated a table gives [`Logged::Large`], as the log of a commit has no pages from before a `TRUNCATE`, and the image keeps them. The result is false, with no call of `log`, for a transaction that loaded rows, which a restart does not keep.
    pub(crate) fn prepare_logged(
        &self,
        xid: u32,
        meta: Vec<u8>,
        log: &mut dyn FnMut(Logged<'_>, &[u8]),
    ) -> bool {
        let mut state = self.state();
        if state.loading.contains_key(&xid) {
            return false;
        }
        state.prepared.insert(xid, meta);
        let mut locks = Vec::new();
        kept_row_locks(&state.row_locks, |x| x == xid).put(&mut locks);
        let truncated = state.truncating.contains_key(&xid);
        match state.touched.remove(&xid) {
            _ if truncated => log(Logged::Large, &locks),
            Some(mut touched) => {
                let mut out = Vec::new();
                if state.log_commit(xid, &mut touched, &[], &mut out) {
                    log(Logged::Changes(&out), &locks);
                } else {
                    log(Logged::Large, &locks);
                }
            }
            None => log(Logged::Nothing, &locks),
        }
        true
    }

    /// The prepared transactions that a restart keeps, with the bytes of what the log keeps of each.
    pub(crate) fn prepared(&self) -> Vec<(u32, Vec<u8>)> {
        self.state().prepared.iter().map(|(&xid, meta)| (xid, meta.clone())).collect()
    }

    /// The redo of a prepare from the log, after [`Heap::redo`] of its changes: the transaction `xid` is in progress again as a prepared transaction with the bytes `meta`, whose heavyweight locks the owner [`crate::twophase::Prepared::lock_owner`] holds, and which holds again the row locks that [`Heap::prepare_logged`] gave, from the rest of `input`. A record of a server before the row locks of the prepared transactions has no more bytes.
    ///
    /// # Errors
    ///
    /// `XX001` for bytes of row locks that do not read.
    pub(crate) fn redo_prepare(
        &self,
        xid: u32,
        meta: Vec<u8>,
        input: &mut Reader<'_>,
    ) -> Result<()> {
        let locks = if input.is_empty() { Vec::new() } else { Persist::take(input)? };
        let mut state = self.state();
        restore_row_locks(&mut state.row_locks, locks);
        state.next_xid = state.next_xid.max(xid.wrapping_add(1));
        Arc::make_mut(&mut state.running).insert(xid);
        state.owners.insert(xid, -xid.cast_signed());
        state.prepared.insert(xid, meta);
        self.publish(&state);
        Ok(())
    }

    /// The redo of `COMMIT PREPARED` or `ROLLBACK PREPARED` from the log. A transaction that the image or the log does not have as prepared ended before the image, so its end does nothing.
    pub(crate) fn redo_finish(&self, xid: u32, commit: bool) {
        if !self.state().prepared.contains_key(&xid) {
            return;
        }
        if commit {
            self.commit(xid);
        } else {
            self.abort(xid);
        }
    }

    /// `AbortSubTransaction` for the rows: the versions that the commands of `xid` from `command` on inserted go, and the versions that they deleted stay. A transaction has no transaction IDs for its subtransactions, so the command of each change tells the changes of the subtransaction from the changes before it.
    pub(crate) fn rollback_to(&self, xid: u32, command: u32) {
        let mut state = self.state();
        state.rechecks.retain(|recheck| recheck.xid != xid || recheck.command < command);
        unqueue_rows(&mut state.row_queues, xid, None);
        // The row locks that the subtransaction took go, and the locks from before it stay.
        if !state.row_locks.is_empty() {
            state.row_locks.retain(|_, lockers| {
                lockers.retain(|&(locker, _, at)| locker != xid || at < command);
                !lockers.is_empty()
            });
        }
        let touched = |table: &Table| {
            table.truncated.iter().any(|&(x, c, _)| x == xid && c >= command)
                || table.versions.iter().any(|version| {
                    (version.xmin == xid && version.cmin >= command)
                        || (version.xmax == xid && version.cmax >= command)
                })
        };
        for table in state.tables.touched_mut(touched) {
            if let Some(at) = table.truncated.iter().position(|&(x, c, _)| x == xid && c >= command)
            {
                table.pages = table.truncated.swap_remove(at).2;
                table.truncated.truncate(at);
            }
            table.remove(|version| version.xmin == xid && version.cmin >= command);
            let deleted = |version: &&mut Version| version.xmax == xid && version.cmax >= command;
            for version in table.versions.iter_mut().filter(deleted) {
                version.aborted = xid;
                version.xmax = 0;
                version.next = 0;
            }
        }
    }

    /// The rechecks of the transaction `xid` from the command `command` on, in the order of the writes, which leave the heap.
    pub(crate) fn take_rechecks(&self, xid: u32, command: u32) -> Vec<Recheck> {
        let mut state = self.state();
        let (taken, kept) = std::mem::take(&mut state.rechecks)
            .into_iter()
            .partition(|recheck| recheck.xid == xid && recheck.command >= command);
        state.rechecks = kept;
        taken
    }

    /// `unique_key_recheck` for the transaction `xid`: the key of the version of `recheck` in its index, checked again with `UNIQUE_CHECK_EXISTING`. A version that went needs no check. The result is the first conflict, as [`Heap::insert`] gives it: a live version with the same key, or a transaction in progress to wait for.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn recheck(
        &self,
        xid: u32,
        recheck: &Recheck,
        derive: Derive<'_>,
    ) -> Result<Option<Changed>> {
        let unique = [Unique { deferrable: None, ..recheck.unique.clone() }];
        let (mut state, places) =
            self.lock_keys((recheck.database, recheck.table), &unique, derive)?;
        let State { tables, running, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(recheck.database, recheck.table)) else {
            return Ok(None);
        };
        let live = table.find(recheck.tid).map(|at| dirty(&table.versions[at], xid, running));
        if live.is_none_or(|seen| seen == Seen::Dead) {
            return Ok(None);
        }
        let checked = (xid, &**running);
        table.check_keys((&places, &unique), &[recheck.tid], checked, &mut Vec::new())
    }

    /// `_bt_load` with `UNIQUE_CHECK_YES` of a new unique index on the versions that the table `relation` has, as `CREATE UNIQUE INDEX` and `ALTER TABLE ... ADD UNIQUE` check them. The versions stay in place. The result is the first live version, in the order of the IDs, with the key of a live version before it, or a transaction in progress to wait for.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of the index and of the equality of a key.
    pub(crate) fn check_build(
        &self,
        relation: u32,
        view: &View,
        unique: &[Unique],
        derive: Derive<'_>,
    ) -> Result<Option<Changed>> {
        let table = (view.database, relation);
        if let Some(checked) = self.check_live_build(table, view.xid, unique) {
            return checked;
        }
        let (mut guard, places) = self.lock_keys(table, unique, derive)?;
        let state = &mut *guard;
        let Some(rows) = state.tables.get(&table) else {
            return Ok(None);
        };
        let checked = (view.xid, &*state.running);
        for (index, &place) in places.iter().enumerate() {
            if let Some(changed) = rows.first_duplicate(index, &rows.keys[place], checked)? {
                return Ok(Some(changed));
            }
        }
        Ok(None)
    }

    /// [`Heap::check_build`] of unique keys of columns on a table whose versions are all live for the dirty snapshot, as after a load. The key of each version comes from the values of its columns, and the first version whose key an earlier version has is the duplicate, as [`Table::first_duplicate`] finds it, with the key of the earlier version. The check makes no map of the keys, so the frozen values of the versions stay in their columns. `None` when a key has an expression or a predicate, or when a version is deleted or a transaction in progress made it, which need [`Heap::check_build`].
    fn check_live_build(
        &self,
        table: (u32, u32),
        xid: u32,
        unique: &[Unique],
    ) -> Option<Result<Option<Changed>>> {
        if unique.iter().any(|key| key.derived.is_some()) {
            return None;
        }
        let state = self.state_read();
        let rows = state.tables.meta(&table)?;
        if rows.versions.iter().any(|version| dirty(version, xid, &state.running) != Seen::Live) {
            return None;
        }
        for (index, key) in unique.iter().enumerate() {
            // The map of the key with the key columns at the start of each key.
            let map = KeyMap { columns: (0..key.columns.len()).collect(), ..KeyMap::of(key) };
            let mut keys: Vec<Box<[Value]>> = Vec::new();
            let mut hashes: HashMap<u64, Vec<usize>> = HashMap::new();
            let mut found: Option<Result<Box<[Value]>>> = None;
            rows.each_version_row(Some(&key.columns), |_, values| {
                if found.is_some() {
                    return;
                }
                let row: Box<[Value]> = key
                    .columns
                    .iter()
                    .map(|&column| values.get(column).cloned().unwrap_or(Value::Null))
                    .collect();
                let Some(hash) = map.hash_key(&row) else { return };
                let list = hashes.entry(hash).or_default();
                for &other in list.iter() {
                    match map.same(&keys[other], &row) {
                        Ok(false) => {}
                        Ok(true) => {
                            found = Some(Ok(keys[other].clone()));
                            return;
                        }
                        Err(error) => {
                            found = Some(Err(error));
                            return;
                        }
                    }
                }
                list.push(keys.len());
                keys.push(row);
            });
            match found {
                Some(Ok(key)) => {
                    return Some(Ok(Some(Changed::Duplicate { index, key: key.into_vec() })));
                }
                Some(Err(error)) => return Some(Err(error)),
                None => {}
            }
        }
        Some(Ok(None))
    }

    /// The versions of the table `relation` that an exclusion constraint checks for the transaction `xid`, as a scan of the index with `SnapshotDirty` finds them, in the order of the IDs. A version that went before the command `command` is not in the list. The result is empty when the commands of `xid` from `command` on made no version of the table. With no command, the list has the live versions, as the build of a new index sees them.
    pub(crate) fn exclusion_rows(
        &self,
        database: u32,
        relation: u32,
        xid: u32,
        command: Option<u32>,
    ) -> Vec<Candidate> {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        let new = |v: &Version| command.is_some_and(|command| v.xmin == xid && v.cmin >= command);
        if command.is_some() && !table.versions.iter().any(new) {
            return Vec::new();
        }
        let command = command.unwrap_or(u32::MAX);
        let mut list = Vec::new();
        for version in &table.versions {
            let seen = dirty(version, xid, &state.running);
            let replaced = if version.xmax == xid && version.cmax >= command && version.next != 0 {
                table.find(version.next).map_or(0, |at| table.versions[at].made)
            } else {
                0
            };
            let fresh = new(version);
            if seen == Seen::Dead && replaced == 0 && !fresh {
                continue;
            }
            list.push(Candidate {
                tid: version.tid,
                made: version.made,
                new: fresh,
                holder: match seen {
                    Seen::Wait(holder) => Some(holder),
                    _ => None,
                },
                live: seen == Seen::Live,
                replaced,
                values: version.values.to_vec(),
            });
        }
        list
    }

    /// Removes the rows of the tables of a database that `keep` does not keep, after a commit drops the tables.
    pub(crate) fn drop_tables(&self, database: u32, keep: impl Fn(u32) -> bool) {
        self.state().tables.retain(|&(db, relation)| db != database || keep(relation));
    }
}

/// `XidInMVCCSnapshot` with the reverse sense: true if the snapshot sees the changes of another transaction.
/// Sets the `xmin` of the session `pid` to the `xmin` of `snapshot` when the session holds no snapshot.
fn hold(state: &mut State, pid: i32, snapshot: &Snapshot) {
    let xmin = state.xmins.entry(pid).or_default();
    if xmin.load(atomic::Ordering::Acquire) == 0 {
        xmin.store(snapshot.xmin(), atomic::Ordering::Release);
    }
}

/// The `xmin` of each session that holds a snapshot, other than the session `except`.
fn held(
    xmins: &HashMap<i32, Arc<AtomicU32>>,
    except: Option<i32>,
) -> impl Iterator<Item = u32> + '_ {
    xmins
        .iter()
        .filter(move |&(&pid, _)| Some(pid) != except)
        .map(|(_, xmin)| xmin.load(atomic::Ordering::Acquire))
        .filter(|&xmin| xmin != 0)
}

fn committed_before(xid: u32, snapshot: &Snapshot) -> bool {
    xid < snapshot.xmax && !snapshot.running.contains(&xid)
}

/// The lines of the rows of a copy, and the lengths and the lines of the rows of the copy after them.
pub(crate) type CopyLines<'a> = (&'a [usize], pages::Sizes<'a>);

/// The positions of the columns that a mask of used columns keeps, so that a scan reads the mask once and not for each row.
fn used_columns(used: Option<&[bool]>) -> Option<Vec<usize>> {
    used.map(|u| u.iter().enumerate().filter(|(_, u)| **u).map(|(i, _)| i).collect())
}

/// The row of a version for a scan: the values of the columns at `columns`, or all its values, then `xmin`, `xmax` and `t_cid` when `xids` is true, then the ID of the version. A column that the version has no value for is null.
fn version_row(
    version: &Version,
    values: &[Value],
    combos: &HashMap<u64, u32>,
    columns: Option<&[usize]>,
    xids: bool,
) -> Vec<Value> {
    let mut row = Vec::with_capacity(columns.map_or(values.len(), <[usize]>::len) + 4);
    match columns {
        Some(columns) => {
            row.extend(columns.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)));
        }
        None => row.extend_from_slice(values),
    }
    if xids {
        let xmax = if version.xmax == 0 { version.aborted } else { version.xmax };
        row.push(Value::Oid(version.xmin));
        row.push(Value::Oid(xmax));
        // `HeapTupleHeaderGetRawCommandId`: `cmin` until a delete, then the combo command ID or `cmax`, also after the delete aborted.
        let cid = if version.xmax == 0 && version.aborted == 0 {
            version.cmin
        } else {
            combos.get(&version.tid).copied().unwrap_or(version.cmax)
        };
        row.push(Value::Oid(cid));
    }
    row.push(Value::Int8(version.tid.cast_signed()));
    row
}

/// The place of the version `tid` in `versions`, which come in the order of their IDs. The item pointers of a heap grow at about the same rate as the places of the versions, so a guess from the IDs at the two ends of the range is near the version. A few guesses make the range small, then a binary search of the range finds the version. A binary search of a large table reads many versions that are not in the cache.
fn place_of(versions: &[Version], tid: u64) -> Option<usize> {
    let (mut low, mut high) = (0, versions.len());
    for _ in 0..4 {
        if high - low <= 8 {
            break;
        }
        let (first, last) = (versions[low].tid, versions[high - 1].tid);
        if tid < first || tid > last {
            return None;
        }
        let span = u128::from(last - first).max(1);
        let width = u128::try_from(high - 1 - low).unwrap_or(u128::MAX);
        let guess = u128::from(tid - first) * width / span;
        let at = low + usize::try_from(guess).unwrap_or(high - 1 - low);
        match versions[at].tid.cmp(&tid) {
            Ordering::Equal => return Some(at),
            Ordering::Less => low = at + 1,
            Ordering::Greater => high = at,
        }
    }
    versions[low..high].binary_search_by_key(&tid, |version| version.tid).ok().map(|at| low + at)
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

/// The bytes of a version keep no [`Version::key_kept`]: after a restart no update is in progress, and an update that committed before it is in each snapshot.
impl Persist for Version {
    fn put(&self, out: &mut Vec<u8>) {
        let Self { tid, made, xmin, cmin, xmax, cmax, aborted, next, key_kept: _, len, values } =
            self;
        tid.put(out);
        made.put(out);
        xmin.put(out);
        cmin.put(out);
        xmax.put(out);
        cmax.put(out);
        aborted.put(out);
        next.put(out);
        len.put(out);
        values.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            tid: Persist::take(input)?,
            made: Persist::take(input)?,
            xmin: Persist::take(input)?,
            cmin: Persist::take(input)?,
            xmax: Persist::take(input)?,
            cmax: Persist::take(input)?,
            aborted: Persist::take(input)?,
            next: Persist::take(input)?,
            key_kept: false,
            len: Persist::take(input)?,
            values: Persist::take(input)?,
        })
    }
}

impl Heap {
    /// The bytes of the rows of all the tables, as a restart after a crash finds them: a transaction in progress aborts, so its new versions go and its deletes do not count. The maps of the unique keys and the sizes of the indexes are not kept, as the heap makes them again from the versions.
    pub(crate) fn image(&self, out: &mut Vec<u8>) -> Imaged {
        let mut imaged = Imaged::default();
        let state = self.state();
        imaged.1 = state.unlogged;
        // A transaction in progress aborts at a restart, and a prepared transaction stays prepared with its changes.
        let prepared = &state.prepared;
        let running = &state.running;
        let gone = |xid: &u32| running.contains(xid) && !prepared.contains_key(xid);
        state.next_xid.put(out);
        state.latest_completed.put(out);
        state.aborted.put(out);
        state.toast_oids.put(out);
        state.tables.0.len().put(out);
        for (key, table) in &state.tables.0 {
            key.put(out);
            table.versions.iter().filter(|v| !gone(&v.xmin)).count().put(out);
            // The encoded columns go to the image as they are when no transaction in progress made a frozen version, so a frozen version keeps no values in the image either.
            let frozen = table
                .frozen
                .as_ref()
                .filter(|_| table.versions.iter().all(|v| !v.values.is_empty() || !gone(&v.xmin)));
            let mut end = 0usize;
            let mut written = 0usize;
            let mut put = |at: usize, values: &[Value]| {
                let v = &table.versions[at];
                if gone(&v.xmin) {
                    return;
                }
                written += 1;
                if frozen.is_some() && v.values.is_empty() {
                    end = written;
                }
                let (aborted, xmax, cmax) =
                    if gone(&v.xmax) { (v.xmax, 0, 0) } else { (v.aborted, v.xmax, v.cmax) };
                (v.tid, v.made, v.xmin, v.cmin).put(out);
                (xmax, cmax, aborted, v.next).put(out);
                v.len.put(out);
                put_count(out, values.len());
                for value in values {
                    value.put(out);
                }
            };
            match frozen {
                Some(_) => {
                    for (at, version) in table.versions.iter().enumerate() {
                        put(at, &version.values);
                    }
                }
                // A frozen version of a transaction in progress is not in the image, so it does not decode.
                None => table.each_kept_row(None, |v| !gone(&v.xmin), put),
            }
            table.made.put(out);
            // The pages of the heap before the `TRUNCATE` of a transaction in progress, which aborts. A prepared transaction keeps the pages after its `TRUNCATE`, and the pages before it come after the prepared transactions.
            let truncated = table.truncated.first().filter(|(x, ..)| !prepared.contains_key(x));
            truncated.map_or(&table.pages, |(.., pages)| pages).put(out);
            table.statistics.put(out);
            table.lp_dead.put(out);
            table.fixed.put(out);
            table.storage.put(out);
            table.save_free.put(out);
            table.combos.put(out);
            // The cold headers are all the versions, and they come before the versions of the list.
            table.cold.put(out);
            let end = table.cold.as_ref().map_or(end, |cold| cold.rows);
            // The bytes of `Option<(usize, usize, Vec<Arc<Stored>>, Vec<(u64, usize)>)>`, which `Frozen::take` reads.
            match frozen {
                Some(frozen) => {
                    out.push(1);
                    (frozen.rows, end).put(out);
                    frozen.columns.put(out);
                    frozen.moved.put(out);
                    imaged.0.push((*key, frozen.columns.clone()));
                }
                None => out.push(0),
            }
        }
        // An image of a server with no prepared transaction ends with the tables, as an image of an earlier server did.
        if !prepared.is_empty() {
            prepared.put(out);
            kept_row_locks(&state.row_locks, |x| prepared.contains_key(&x)).put(out);
            let mut truncated: Vec<_> = state
                .tables
                .0
                .iter()
                .filter_map(|(key, table)| {
                    let (xid, command, pages) = table.truncated.first()?;
                    prepared.contains_key(xid).then_some((key, xid, command, pages))
                })
                .collect();
            truncated.sort_unstable_by_key(|&(key, ..)| key);
            put_count(out, truncated.len());
            for (key, xid, command, pages) in truncated {
                (*key, *xid, *command).put(out);
                pages.put(out);
            }
        }
        imaged
    }

    /// Gives the tables of `imaged` the frozen columns of `from`, the heap of the image that [`Heap::image`] wrote, when a table still has the columns that went to the image. The columns of `from` read a mapping of the image, so the columns in the memory of the process go. A table that changed after the image keeps its columns.
    pub(crate) fn adopt(&self, imaged: Imaged, from: Heap) {
        let mut from = from.state.into_inner().unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state();
        for (key, old) in imaged.0 {
            let Some(new) = from.tables.0.get_mut(&key).and_then(|t| t.frozen.take()) else {
                continue;
            };
            let Some(table) = state.tables.0.get_mut(&key) else {
                continue;
            };
            let Some(frozen) = table.frozen.as_mut() else {
                continue;
            };
            let same = frozen.columns.len() == old.len()
                && frozen.columns.iter().zip(&old).all(|(a, b)| Arc::ptr_eq(a, b))
                && new.columns.len() == old.len()
                && new.columns.iter().zip(&old).all(|(a, b)| a.rows == b.rows && a.kind == b.kind);
            if !same {
                continue;
            }
            for stored in table.columns.stored.values_mut() {
                if let Some(at) = old.iter().position(|o| Arc::ptr_eq(o, stored)) {
                    *stored = Arc::clone(&new.columns[at]);
                }
            }
            frozen.columns = new.columns;
            // The versions of a load have no values once they are frozen, so their headers go to the cold encoding too.
            table.chill();
        }
    }

    /// The heap of the bytes of [`Heap::image`].
    ///
    /// # Errors
    ///
    /// `XX001` for bytes that are not an image.
    pub(crate) fn from_image(input: &mut Reader<'_>) -> Result<Heap> {
        let mut state = State {
            next_xid: Persist::take(input)?,
            latest_completed: Persist::take(input)?,
            aborted: Persist::take(input)?,
            toast_oids: Persist::take(input)?,
            ..State::default()
        };
        for _ in 0..input.count()? {
            let key = Persist::take(input)?;
            let mut table = Table {
                versions: Persist::take(input)?,
                made: Persist::take(input)?,
                pages: Persist::take(input)?,
                statistics: Persist::take(input)?,
                lp_dead: Persist::take(input)?,
                fixed: Persist::take(input)?,
                storage: Persist::take(input)?,
                save_free: Persist::take(input)?,
                combos: Persist::take(input)?,
                ..Table::default()
            };
            table.cold = Persist::take(input)?;
            if table.cold.is_some() && !table.versions.is_empty() {
                return Err(rupg_common::persist::corrupt("a cold table has versions in a list"));
            }
            let cold = table.cold.as_ref().map_or(0, |cold| cold.rows);
            table.frozen = Frozen::take(input, &table.versions, cold)?;
            if table.cold.is_some() && table.frozen.is_none() {
                return Err(rupg_common::persist::corrupt("a cold table has no frozen columns"));
            }
            state.tables.0.insert(key, table);
        }
        if !input.is_empty() {
            state.prepared = Persist::take(input)?;
            let running = Arc::make_mut(&mut state.running);
            for &xid in state.prepared.keys() {
                running.insert(xid);
                state.owners.insert(xid, -xid.cast_signed());
            }
            // An image of a server before the row locks of the prepared transactions ends with the transactions.
            if !input.is_empty() {
                restore_row_locks(&mut state.row_locks, Persist::take(input)?);
            }
            // The pages from before the `TRUNCATE` of each table that a prepared transaction truncated, which `ROLLBACK PREPARED` brings back. An earlier image ends with the row locks.
            if !input.is_empty() {
                for _ in 0..input.count()? {
                    let (key, xid, command): ((u32, u32), u32, u32) = Persist::take(input)?;
                    let pages = Persist::take(input)?;
                    let Some(table) = state.tables.0.get_mut(&key) else {
                        return Err(rupg_common::persist::corrupt(
                            "a prepared transaction truncated a table that is not in the image",
                        ));
                    };
                    table.truncated.push((xid, command, pages));
                    state.truncating.entry(xid).or_default().push(key);
                }
            }
        }
        Ok(Heap {
            state: RwLock::new(state),
            published: Mutex::default(),
            tasks: None,
            ssi: Mutex::default(),
        })
    }

    /// The versions of a table whose values are in encoded columns.
    #[cfg(test)]
    pub(crate) fn frozen_rows(&self, database: u32, relation: u32) -> usize {
        let state = self.state();
        let table = state.tables.meta(&(database, relation));
        table.and_then(|table| table.frozen.as_ref()).map_or(0, |frozen| frozen.rows)
    }

    /// The open rows of the first encoded column of a table, which are not in a segment yet.
    #[cfg(test)]
    pub(crate) fn open_rows(&self, database: u32, relation: u32) -> usize {
        let state = self.state();
        let table = state.tables.meta(&(database, relation));
        let frozen = table.and_then(|table| table.frozen.as_ref());
        frozen.and_then(|frozen| frozen.columns.first()).map_or(0, |column| column.open_rows())
    }

    /// The vectors of the frozen columns of a table that read a mapping of a file, and all the vectors of the frozen columns.
    #[cfg(test)]
    pub(crate) fn mapped_vectors(&self, database: u32, relation: u32) -> (usize, usize) {
        let state = self.state();
        let table = state.tables.meta(&(database, relation));
        let columns = table.and_then(|table| table.frozen.as_ref()).map(|f| f.columns.as_slice());
        let vectors = columns.unwrap_or_default().iter().flat_map(|c| &c.segments);
        let vectors = vectors.flat_map(|segment| &segment.vectors);
        vectors.fold((0, 0), |(mapped, all), v| (mapped + usize::from(v.data.is_mapped()), all + 1))
    }

    /// The versions of a table whose headers are in encoded columns.
    #[cfg(test)]
    pub(crate) fn cold_rows(&self, database: u32, relation: u32) -> usize {
        let state = self.state();
        let table = state.tables.meta(&(database, relation));
        table.and_then(|table| table.cold.as_ref()).map_or(0, |cold| cold.rows)
    }

    /// `nextXid` goes up to `next`, for a log of a catalog: the transaction that changed the catalog has an ID that the logs of the rows may not have.
    pub(crate) fn advance_xid(&self, next: u32) {
        let mut state = self.state();
        state.next_xid = state.next_xid.max(next);
        self.publish(&state);
    }

    /// The OIDs of the databases whose tables have rows.
    pub(crate) fn databases(&self) -> BTreeSet<u32> {
        self.state().tables.0.keys().map(|&(database, _)| database).collect()
    }

    /// From now on each commit gives the log of its changes to [`Heap::commit_logged`].
    pub(crate) fn start_logging(&self) {
        self.state().logging = true;
    }

    /// Makes again the changes of a commit from the bytes of its log, after the image of the heap and the logs of the commits before it. A log can have changes that the image has: a version comes again with the same ID and takes the place of the version that the image has, and a `TRUNCATE` whose generation the table has does nothing. `VACUUM` writes no log, so the dead versions that it removed come back, and a version of the log takes the place of a dead version with the same ID.
    ///
    /// # Errors
    ///
    /// `XX001` for bytes that are not a log of a commit.
    pub(crate) fn redo(&self, input: &mut Reader<'_>) -> Result<()> {
        let mut state = self.state();
        let xid = u32::take(input)?;
        state.next_xid = state.next_xid.max(xid.wrapping_add(1));
        state.latest_completed = state.latest_completed.max(xid);
        self.publish(&state);
        let mut updated = Vec::new();
        for _ in 0..input.count()? {
            let key: (u32, u32) = Persist::take(input)?;
            let generation: Option<u16> = Persist::take(input)?;
            let table = state.tables.entry(key);
            if let Some(generation) = generation
                && table.pages.generation != generation
            {
                table.pages = Pages::of_generation(generation);
                table.versions.retain(|v| generation_of(v.tid) == generation);
                table.combos.clear();
                table.lp_dead.clear();
            }
            (table.fixed, table.storage, table.save_free) = Persist::take(input)?;
            for _ in 0..input.count()? {
                let version = Version::take(input)?;
                table.pages.occupy(version.tid, usize::from(version.len));
                table.made = table.made.max(version.made);
                match table.versions.binary_search_by_key(&version.tid, |v| v.tid) {
                    Ok(at) => table.versions[at] = version,
                    Err(at) => table.versions.insert(at, version),
                }
            }
            for _ in 0..input.count()? {
                let (tid, xmax, cmax, next): (u64, u32, u32, u64) = Persist::take(input)?;
                if let Some(at) = table.find(tid) {
                    let version = &mut table.versions[at];
                    (version.xmax, version.cmax, version.next) = (xmax, cmax, next);
                    table.pages.touch(page_of(tid));
                }
                if next != 0 && next != MOVED {
                    updated.push((key, tid, next));
                }
            }
        }
        // The new version of an update keeps the row locks of the prepared transactions, as `compute_new_xmax_infomask` kept them when the update ran.
        if !state.row_locks.is_empty() {
            for ((database, relation), old, next) in updated {
                if let Some(lockers) = state.row_locks.get(&(database, relation, old)).cloned() {
                    restore_row_locks(
                        &mut state.row_locks,
                        vec![((database, relation, next), lockers)],
                    );
                }
            }
        }
        Ok(())
    }
}

impl State {
    /// The log of the commit of `xid`, which changed the tables `touched` and truncated the tables `truncated`: for each table, the generation of its heap after a `TRUNCATE`, the storage of its columns and its `fillfactor`, the versions that the transaction inserted, and the versions that it deleted with their `xmax`, `cmax` and `t_ctid`.
    ///
    /// The result is false, with no log, for a commit that changed more than [`LOG_VERSIONS`] versions.
    fn log_commit(
        &self,
        xid: u32,
        touched: &mut [Touched],
        truncated: &[(u32, u32)],
        out: &mut Vec<u8>,
    ) -> bool {
        xid.put(out);
        let mut tables = Vec::new();
        let mut changed = 0;
        for Touched { key, tids } in touched {
            let Some(table) = self.tables.meta(key) else { continue };
            // A cold table that has versions of the transaction makes the commit take a checkpoint, as its versions are not in the list.
            if table.cold.as_ref().is_some_and(|cold| cold.xmins.iter().any(|&(x, _)| x == xid)) {
                return false;
            }
            let (inserted, deleted) = match tids {
                // The versions that the transaction noted, in the order of their IDs, as the versions of the table are.
                Some(tids) => {
                    tids.sort_unstable();
                    tids.dedup();
                    let noted = tids.iter().filter_map(|&tid| table.find(tid));
                    let found = changes(noted.map(|at| &table.versions[at]), xid);
                    // The noted versions are all the versions that the transaction changed.
                    #[cfg(debug_assertions)]
                    {
                        let ids = |versions: &[&Version]| {
                            versions.iter().map(|v| v.tid).collect::<Vec<_>>()
                        };
                        let all = changes(table.versions.iter(), xid);
                        debug_assert_eq!(
                            (ids(&found.0), ids(&found.1)),
                            (ids(&all.0), ids(&all.1))
                        );
                    }
                    found
                }
                // One pass over the versions of the table finds the versions that the commit inserted and deleted.
                None => changes(table.versions.iter(), xid),
            };
            // A frozen version has no values to log, and a checkpoint writes it. The new versions after a freeze keep their values, and a delete logs no values.
            let frozen = table.frozen.as_ref().is_some_and(|frozen| !frozen.columns.is_empty());
            if frozen && inserted.iter().any(|v| v.values.is_empty()) {
                return false;
            }
            changed += inserted.len() + deleted.len();
            if changed > LOG_VERSIONS {
                return false;
            }
            tables.push((key, table, inserted, deleted));
        }
        tables.len().put(out);
        for (key, table, inserted, deleted) in tables {
            key.put(out);
            truncated.contains(key).then_some(table.pages.generation).put(out);
            table.fixed.put(out);
            table.storage.put(out);
            table.save_free.put(out);
            inserted.len().put(out);
            for version in inserted {
                version.put(out);
            }
            deleted.len().put(out);
            for v in deleted {
                (v.tid, v.xmax, v.cmax, v.next).put(out);
            }
        }
        true
    }
}

/// The versions of `versions` that the transaction `xid` inserted, and the versions that it deleted.
fn changes<'t>(
    versions: impl Iterator<Item = &'t Version>,
    xid: u32,
) -> (Vec<&'t Version>, Vec<&'t Version>) {
    let (mut inserted, mut deleted) = (Vec::new(), Vec::new());
    for version in versions {
        if version.xmin == xid {
            inserted.push(version);
        } else if version.xmax == xid {
            deleted.push(version);
        }
    }
    (inserted, deleted)
}

/// The places of a random sample of `target` of `total` versions, in order, with the seed `seed`, by the algorithm of Floyd. Places at equal distances, as an earlier `ANALYZE` took them, see one value of a column whose values repeat with a period that the distance divides. All the places come when `total` is at most `target`.
fn sample_places(total: usize, target: usize, seed: u32) -> Vec<usize> {
    if total <= target {
        return (0..total).collect();
    }
    let mut prng = rupg_types::prng::Prng::seed(u64::from(seed) ^ 0x5eed_a11a_1e5e_ed00);
    let mut chosen: HashSet<usize> = HashSet::with_capacity(target);
    for j in total - target..total {
        let t = prng.uint64_range(0, j as u64) as usize;
        if !chosen.insert(t) {
            chosen.insert(j);
        }
    }
    let mut places: Vec<usize> = chosen.into_iter().collect();
    places.sort_unstable();
    places
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key of a row in an index with an expression, which the tests do not have.
    fn none(_: &Derived, _: &[Value]) -> Result<Option<Vec<Value>>> {
        Ok(None)
    }

    fn view(snapshot: Snapshot, xid: u32, command: u32) -> View {
        View { database: 1, snapshot, xid, command, serializable: false, sxact: 0 }
    }

    /// The ID of the version of the first row that the view sees.
    fn first_tid(heap: &Heap, view: &View) -> u64 {
        let rows = heap.rows(7, view);
        let Some(Value::Int8(tid)) = rows[0].last() else { panic!("a row with no ID") };
        tid.cast_unsigned()
    }

    /// Settled versions with no values, which cold headers keep.
    fn settled(rows: u64) -> Vec<Version> {
        (0..rows)
            .map(|at| Version {
                tid: at + 1,
                made: at,
                xmin: 3 + (at % 2) as u32,
                cmin: 0,
                xmax: 0,
                cmax: 0,
                aborted: 0,
                next: 0,
                key_kept: false,
                len: 32,
                values: Arc::default(),
            })
            .collect()
    }

    #[test]
    fn an_update_of_a_column_without_overlaps_changes_the_key() {
        let old = [Value::Int4(1), Value::Int4(2)];
        let new = [Value::Int4(1), Value::Int4(3)];
        assert!(changes_key(&[], &[1], &old, &new));
        assert!(!changes_key(&[], &[0], &old, &new));
        assert!(!changes_key(&[], &[], &old, &new));
    }

    #[test]
    fn cold_headers_load_only_when_their_vectors_have_their_rows() {
        let rows = VECTOR as u64 * 2 + 5;
        let cold = Cold::of(&settled(rows)).unwrap();
        let mut image = Vec::new();
        cold.put(&mut image);
        let back = Cold::take(&mut Reader::new(&image)).unwrap();
        let got: Vec<(u64, u64, u32, u16)> =
            back.versions().unwrap().iter().map(|v| (v.tid, v.made, v.xmin, v.len)).collect();
        let want: Vec<(u64, u64, u32, u16)> =
            settled(rows).iter().map(|v| (v.tid, v.made, v.xmin, v.len)).collect();
        assert_eq!(got, want);
        // A count of rows that the vectors do not have does not load.
        let mut wrong =
            Cold { rows: cold.rows + 1, vectors: cold.vectors.clone(), xmins: cold.xmins.clone() };
        let mut image = Vec::new();
        wrong.put(&mut image);
        assert!(Cold::take(&mut Reader::new(&image)).is_err());
        // A vector whose bytes do not decode does not load.
        wrong.rows = cold.rows;
        wrong.vectors[1][0] = wrong.vectors[1][0][..1].into();
        let mut image = Vec::new();
        wrong.put(&mut image);
        assert!(Cold::take(&mut Reader::new(&image)).is_err());
    }

    #[test]
    fn the_runs_have_the_places_of_their_columns() {
        let mut columns: Vec<Arc<Stored>> = (0..37)
            .map(|at| {
                Arc::new(Stored::build(Kind::Int8, [Value::Int8(at)].iter().map(Some)).unwrap())
            })
            .collect();
        let got = on_runs(Some(&rupg_platform::os::OsTasks), &mut columns, |first, run| {
            run.iter()
                .zip(first..)
                .map(|(column, at)| (at, Decoded::new(column).next()))
                .collect::<Vec<_>>()
        });
        assert!(got.len() > 1);
        let got: Vec<_> = got.into_iter().flatten().collect();
        let want: Vec<_> = (0..37).map(|at| (at, Some(Value::Int8(at as i64)))).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn the_rows_at_some_places_are_the_rows_that_a_scan_gives() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        let row = |i: usize| {
            let text = if i.is_multiple_of(11) {
                Value::Null
            } else {
                Value::Text(format!("t{}", i % 97))
            };
            vec![Value::Int8(i as i64 * 7), text, Value::Float8(i as f64 / 4.0)]
        };
        let rows = |range: std::ops::Range<usize>| range.map(row).collect::<Vec<_>>();
        let insert = |rows| heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]);
        insert(rows(0..ROWS + 5000)).unwrap();
        let mut places: Vec<usize> =
            (0..ROWS + 9000).step_by(37).chain([10, ROWS + 8999]).collect();
        places.sort_unstable();
        let check = |table: &Table, frozen: bool| {
            assert_eq!(table.frozen.is_some(), frozen);
            let mut all = Vec::new();
            table.each_version_row(None, |_, values| all.push(values.to_vec()));
            let want: Vec<Vec<Value>> = places.iter().map(|&at| all[at].clone()).collect();
            assert_eq!(table.rows_at(&places, None).unwrap(), want);
            assert_eq!(want[2], all[37]);
        };
        insert(rows(ROWS + 5000..ROWS + 9000)).unwrap();
        // The load froze its first rows. The test starts again from versions that all have their values.
        let mut state = heap.state();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        table.frozen = None;
        for (version, at) in table.versions.iter_mut().zip(0..) {
            version.values = row(at).into();
        }
        check(table, false);
        // The first rows are frozen and the last rows keep their values.
        table.freeze_to(ROWS + 3000, None);
        check(table, true);
        // A version with its values between the frozen ones, as a new version in an earlier page is.
        let version = table.versions.remove(ROWS + 5000);
        table.versions.insert(10, version);
        table.frozen.as_mut().unwrap().end += 1;
        check(table, true);
        let mut all = Vec::new();
        table.each_version_row(None, |_, values| all.push(values.to_vec()));
        assert_eq!((&all[10], &all[11]), (&row(ROWS + 5000), &row(10)));
        table.freeze(None);
        table.seal();
        table.chill();
        assert!(table.cold.is_some());
        check(table, true);
    }

    #[test]
    fn a_fetch_of_some_versions_of_a_cold_table_does_not_thaw_it() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        let row = |i: usize| {
            vec![
                Value::Int8(i as i64),
                Value::Text(format!("t{}", i % 13)),
                Value::Int8(i as i64 % 5),
            ]
        };
        let rows = (0..VECTOR * 3 + 7).map(row).collect::<Vec<_>>();
        heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]).unwrap();
        heap.commit(xid);
        let tids = {
            let mut state = heap.state();
            let table = state.tables.0.get_mut(&(1, 7)).unwrap();
            table.freeze(None);
            table.seal();
            table.chill();
            assert!(table.cold.is_some());
            let mut tids = Vec::new();
            table.each_version(|version| tids.push(version.tid));
            tids
        };
        let reader = view(heap.snapshot(), heap.begin(1), 0);
        let used = [true, false, true];
        let all = heap.visible_rows(7, &reader, false, Some(&used));
        // The places are not in order, one place is twice in the list, and one ID has no version.
        let picked = [VECTOR * 2 + 3, 4, VECTOR + 1, 4, VECTOR * 3 + 6];
        let mut list: Vec<u64> = picked.iter().map(|&at| tids[at]).collect();
        list.insert(2, tids[tids.len() - 1] + 1000);
        let want: Vec<Vec<Value>> = picked.iter().map(|&at| all[at].clone()).collect();
        assert_eq!(heap.rows_at(7, &reader, Some(&used), &list), want);
        assert_eq!(
            heap.rows_at(7, &reader, None, &list[..1]),
            [vec![
                Value::Int8(picked[0] as i64),
                Value::Text(format!("t{}", picked[0] % 13)),
                Value::Int8(picked[0] as i64 % 5),
                Value::Int8(tids[picked[0]].cast_signed()),
            ]]
        );
        let state = heap.state();
        let table = &state.tables.0[&(1, 7)];
        assert!(table.cold.is_some() && table.frozen.is_some() && table.kept.is_none());
    }

    #[test]
    fn the_size_of_an_index_and_the_range_of_a_column_keep_the_frozen_columns() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        let row = |i: usize| vec![Value::Int8((i * 7 % 3001) as i64), Value::Int8((i % 5) as i64)];
        let rows = (0..3001).map(row).collect::<Vec<_>>();
        heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]).unwrap();
        heap.commit(xid);
        let (columns, types, options) = (vec![0, 1], vec![oid::INT8, oid::INT8], vec![0, 0]);
        let shape = Shape {
            columns,
            types,
            key_count: 2,
            options,
            unique: true,
            fillfactor: 90,
            deduplicate: true,
        };
        let order = |a: &Value, b: &Value| match (a, b) {
            (Value::Int8(a), Value::Int8(b)) => Some(a.cmp(b)),
            _ => None,
        };
        let size = heap.btree_size(1, 7, 9, &shape);
        let range = heap.variable_range(1, 7, 0, order);
        assert_eq!(range, Some((Value::Int8(0), Value::Int8(3000))));
        {
            let mut state = heap.state();
            let table = state.tables.0.get_mut(&(1, 7)).unwrap();
            table.freeze(None);
            table.seal();
            table.chill();
            assert!(table.cold.is_some());
            table.btree_sizes.clear();
        }
        assert_eq!(heap.btree_size(1, 7, 9, &shape), size);
        assert_eq!(heap.variable_range(1, 7, 0, order), range);
        let state = heap.state();
        let table = &state.tables.0[&(1, 7)];
        assert!(table.frozen.is_some() && table.cold.is_some());
    }

    #[test]
    fn a_freeze_takes_back_the_columns_that_a_read_thawed() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        let row = |i: usize| vec![Value::Int8(i as i64), Value::Text(format!("v{}", i % 13))];
        let rows = (0..3000).map(row).collect::<Vec<_>>();
        heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]).unwrap();
        heap.commit(xid);
        let mut state = heap.state();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        table.freeze(None);
        let columns: Vec<Arc<Stored>> = table.frozen.as_ref().unwrap().columns.clone();
        // A read thaws the table and keeps the columns, and the next freeze takes them back.
        let read = state.tables.get(&(1, 7)).unwrap();
        assert!(read.frozen.is_none() && read.kept.is_some());
        assert_eq!(&*read.versions[5].values, &row(5)[..]);
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        table.freeze(None);
        let frozen = table.frozen.as_ref().unwrap();
        assert!(frozen.columns.iter().zip(&columns).all(|(a, b)| Arc::ptr_eq(a, b)));
        assert!(table.versions.iter().all(|version| version.values.is_empty()));
        let mut all = Vec::new();
        table.each_version_row(None, |_, values| all.push(values.to_vec()));
        assert_eq!(all, (0..3000).map(row).collect::<Vec<_>>());
        // A write after the read drops the columns, and the freeze makes them again.
        state.tables.get(&(1, 7)).unwrap();
        state.tables.get_mut(&(1, 7)).unwrap();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        assert!(table.kept.is_none());
        table.freeze(None);
        let frozen = table.frozen.as_ref().unwrap();
        assert!(!frozen.columns.iter().zip(&columns).any(|(a, b)| Arc::ptr_eq(a, b)));
    }

    #[test]
    fn a_freeze_that_fails_waits_until_the_table_has_twice_as_many_versions() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        // A `uuid` column has no encoding, so the versions keep their values.
        let row = |i: usize| vec![Value::Int8(i as i64), Value::Uuid([i as u8; 16])];
        let rows = (0..200).map(row).collect::<Vec<_>>();
        heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]).unwrap();
        let mut state = heap.state();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        table.freeze_to(100, None);
        assert!(table.frozen.is_none());
        assert_eq!(table.unfrozen, 100);
        table.freeze_to(199, None);
        assert_eq!(table.unfrozen, 100);
        table.freeze_to(200, None);
        assert_eq!(table.unfrozen, 200);
        assert!(table.frozen.is_none());
        assert!(table.versions.iter().all(|version| version.values.len() == 2));
    }

    #[test]
    fn a_version_between_the_frozen_ones_moves_after_the_frozen_rows() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        let row = |i: usize| {
            let text = if i.is_multiple_of(13) {
                Value::Null
            } else {
                Value::Text(format!("m{}", i % 89))
            };
            vec![Value::Int8(i as i64 * 3), text, Value::Float8(i as f64 / 8.0)]
        };
        let rows = (0..ROWS + 9000).map(row).collect();
        heap.insert(7, &view(heap.snapshot(), xid, 0), rows, (&[], &none), &[]).unwrap();
        heap.commit(xid);
        let mut state = heap.state();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        table.frozen = None;
        for (version, at) in table.versions.iter_mut().zip(0..) {
            version.values = row(at).into();
        }
        table.freeze_to(ROWS + 3000, None);
        // The last version goes to the next slot of the page of the version before the place `near` or after it, as a new version in an earlier page does.
        let move_in = |table: &mut Table, near: usize| {
            let at = (near..)
                .find(|&at| page_of(table.versions[at - 1].tid) != page_of(table.versions[at].tid))
                .unwrap();
            let before = table.versions[at - 1].tid;
            let mut version = table.versions.pop().unwrap();
            version.tid = pages::tid(generation_of(before), page_of(before), slot_of(before) + 1);
            table.versions.insert(at, version);
            table.frozen.as_mut().unwrap().end += 1;
        };
        let all = |table: &Table| {
            let mut all = Vec::new();
            table.each_version_row(None, |_, values| all.push(values.to_vec()));
            all
        };
        for near in [10, 700, ROWS / 2] {
            move_in(table, near);
        }
        let want = all(table);
        assert!(table.fold(None));
        let frozen = table.frozen.as_ref().unwrap();
        assert_eq!((frozen.moved.len(), frozen.rows), (3, frozen.end));
        assert_eq!(all(table), want);
        // A second load puts versions between the frozen ones again, and the frozen rows go on after the moved ones.
        table.freeze_to(ROWS + 6000, None);
        for near in [5, 600, ROWS + 100] {
            move_in(table, near);
        }
        let want = all(table);
        table.freeze(None);
        let frozen = table.frozen.as_ref().unwrap();
        assert_eq!((frozen.moved.len(), frozen.rows), (6, table.versions.len()));
        assert!(frozen.moved.iter().any(|&(_, row)| row >= ROWS + 3000));
        assert_eq!(all(table), want);
        let places: Vec<usize> = (0..want.len())
            .step_by(29)
            .chain([5, 6, 11, 601])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let picked: Vec<Vec<Value>> = places.iter().map(|&at| want[at].clone()).collect();
        assert_eq!(table.rows_at(&places, None).unwrap(), picked);
        table.seal();
        table.chill();
        assert!(table.cold.is_some());
        assert_eq!(all(table), want);
        assert_eq!(table.rows_at(&places, None).unwrap(), picked);
        drop(state);
        let mut image = Vec::new();
        heap.image(&mut image);
        let back = Heap::from_image(&mut Reader::new(&image)).unwrap();
        let mut state = back.state();
        let table = state.tables.0.get_mut(&(1, 7)).unwrap();
        assert_eq!(table.frozen.as_ref().unwrap().moved.len(), 6);
        assert_eq!(all(table), want);
        table.warm();
        table.refreeze();
        assert!(table.frozen.as_ref().unwrap().moved.is_empty());
        assert_eq!(all(table), want);
        table.thaw();
        assert_eq!(
            table.versions.iter().map(|version| version.values.to_vec()).collect::<Vec<_>>(),
            want
        );
    }

    #[test]
    fn a_snapshot_sees_only_the_committed_rows() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
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
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.abort(xid);
        assert!(!heap.has_rows(1, 7));
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
        let xid = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        assert_eq!(first_tid(&heap, &view(heap.snapshot(), xid, 1)), 2);
    }

    #[test]
    fn a_truncate_waits_and_a_rollback_brings_the_rows_back() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        let a = heap.begin(2);
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 0)), Some(first));
        heap.commit(first);
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 0)), None);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), a, 1)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
        heap.abort(a);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
    }

    #[test]
    fn a_rollback_to_a_savepoint_undoes_the_later_commands() {
        let heap = Heap::default();
        let a = heap.begin(1);
        let row = |n| vec![vec![Value::Int4(n)]];
        heap.insert(7, &view(heap.snapshot(), a, 0), row(1), (&[], &none), &[]).unwrap();
        heap.insert(7, &view(heap.snapshot(), a, 1), row(2), (&[], &none), &[]).unwrap();
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 2)), None);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), a, 3)).len(), 0);
        heap.rollback_to(a, 1);
        let rows = heap.rows(7, &view(heap.snapshot(), a, 3));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(1));
    }

    #[test]
    fn an_update_waits_for_the_other_transaction() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let a = heap.begin(1);
        let b = heap.begin(2);
        let tid = first_tid(&heap, &view(heap.snapshot(), a, 0));
        let change = || vec![(tid, Some(vec![Value::Int4(2)]))];
        let done = heap
            .change(
                7,
                &view(heap.snapshot(), a, 0),
                change(),
                (&[], &[], &none),
                &[],
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(done, Changed::Done(1));
        let wait = Changed::Wait { holder: a, tid };
        assert_eq!(
            heap.change(
                7,
                &view(heap.snapshot(), b, 0),
                change(),
                (&[], &[], &none),
                &[],
                &mut Vec::new()
            )
            .unwrap(),
            wait
        );
        heap.wait(b, a, None);
        assert_eq!(heap.deadlock(b), None);
        assert_eq!(heap.blockers(2), [1]);
        assert_eq!(heap.blockers(1), [] as [i32; 0]);
        heap.wait(a, b, None);
        let cycle = heap.deadlock(b).unwrap();
        assert_eq!(cycle, [Wait { pid: 2, xid: a, holder: 1 }, Wait { pid: 1, xid: b, holder: 2 }]);
        heap.waited(a);
        heap.abort(a);
        let done = heap
            .change(
                7,
                &view(heap.snapshot(), b, 0),
                change(),
                (&[], &[], &none),
                &[],
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(done, Changed::Done(1));
        heap.commit(b);
        let rows = heap.rows(7, &view(heap.snapshot(), 0, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(2));
    }

    #[test]
    fn a_read_of_a_key_prunes_the_versions_that_no_snapshot_sees() {
        let heap = Heap::default();
        let key = (&[0][..], &[oid::INT4][..], false);
        let read = |view: &View| {
            let rows = heap.key_rows(7, view, None, key, &[Value::Int4(1)]).unwrap().unwrap();
            assert_eq!(rows.len(), 1);
            let Some(&Value::Int8(tid)) = rows[0].last() else { panic!("a row with no ID") };
            (rows[0][1].clone(), tid.cast_unsigned())
        };
        let bucket = || {
            let state = heap.state();
            let table = &state.tables.0[&(1, 7)];
            table.keys[0].hashes.values().map(Tids::len).sum::<usize>()
        };
        let first = heap.begin(1);
        let row = |b: i32| vec![Value::Int4(1), Value::Int4(b)];
        heap.insert(7, &view(heap.snapshot(), first, 0), vec![row(0)], (&[], &none), &[]).unwrap();
        heap.commit(first);
        // The session 2 holds a snapshot from before the updates, so their old versions stay.
        let old = view(heap.held_snapshot(2), 0, 0);
        for b in 1..=5 {
            let xid = heap.begin(1);
            let (_, tid) = read(&view(heap.held_snapshot(1), xid, 0));
            let change = vec![(tid, Some(row(b)))];
            heap.change(
                7,
                &view(heap.snapshot(), xid, 0),
                change,
                (&[], &[], &none),
                &[],
                &mut Vec::new(),
            )
            .unwrap();
            heap.commit(xid);
            heap.release(1);
        }
        assert_eq!(read(&old).0, Value::Int4(0));
        assert_eq!(bucket(), 6);
        heap.release(2);
        assert_eq!(read(&view(heap.held_snapshot(1), 0, 0)).0, Value::Int4(5));
        assert_eq!(bucket(), 1);
    }

    #[test]
    fn a_published_snapshot_keeps_the_versions_that_it_sees() {
        let heap = Heap::default();
        let key = (&[0][..], &[oid::INT4][..], false);
        let read = |view: &View| {
            let rows = heap.key_rows(7, view, None, key, &[Value::Int4(1)]).unwrap().unwrap();
            assert_eq!(rows.len(), 1);
            let Some(&Value::Int8(tid)) = rows[0].last() else { panic!("a row with no ID") };
            (rows[0][1].clone(), tid.cast_unsigned())
        };
        let (one, two) = (heap.xmin(1), heap.xmin(2));
        // Before the first change, the snapshot comes from the state.
        let empty = heap.held_snapshot_in(&two);
        assert_eq!(empty.xmin(), heap.snapshot().xmin());
        two.store(0, atomic::Ordering::Release);
        let first = heap.begin(1);
        let row = |b: i32| vec![Value::Int4(1), Value::Int4(b)];
        heap.insert(7, &view(heap.snapshot(), first, 0), vec![row(0)], (&[], &none), &[]).unwrap();
        heap.commit(first);
        // The published snapshot sees the commit.
        let old = view(heap.held_snapshot_in(&two), 0, 0);
        assert_eq!(read(&old).0, Value::Int4(0));
        for b in 1..=3 {
            let xid = heap.begin(1);
            let (_, tid) = read(&view(heap.held_snapshot_in(&one), xid, 0));
            let change = vec![(tid, Some(row(b)))];
            heap.change(
                7,
                &view(heap.snapshot(), xid, 0),
                change,
                (&[], &[], &none),
                &[],
                &mut Vec::new(),
            )
            .unwrap();
            heap.commit(xid);
            one.store(0, atomic::Ordering::Release);
        }
        // The session 2 still holds its snapshot, so a prune keeps the version that it sees.
        assert_eq!(read(&old).0, Value::Int4(0));
        assert_eq!(read(&view(heap.held_snapshot_in(&one), 0, 0)).0, Value::Int4(3));
        let aborted = heap.begin(1);
        heap.abort(aborted);
        let after = heap.held_snapshot_in(&one);
        assert!(!after.running.contains(&aborted));
        assert_eq!(after.xmin(), heap.snapshot().xmin());
    }

    #[test]
    fn a_unique_key_waits_for_the_other_transaction() {
        let heap = Heap::default();
        let key = Unique {
            oid: 0,
            name: "t_pkey".to_string(),
            columns: vec![0],
            types: vec![oid::INT4],
            shown: "a".to_string(),
            nulls_not_distinct: false,
            derived: None,
            deferrable: None,
        };
        let unique = std::slice::from_ref(&key);
        let row = |a: i32| vec![Value::Int4(a)];
        let first = heap.begin(1);
        let rows = vec![row(1), Vec::from([Value::Null]), Vec::from([Value::Null])];
        let done =
            heap.insert(7, &view(heap.snapshot(), first, 0), rows, (unique, &none), &[]).unwrap();
        assert_eq!(done, Changed::Done(3));
        let again =
            heap.insert(7, &view(heap.snapshot(), first, 1), vec![row(1)], (unique, &none), &[]);
        assert_eq!(again.unwrap(), Changed::Duplicate { index: 0, key: row(1) });
        heap.commit(first);
        let a = heap.begin(1);
        let b = heap.begin(2);
        heap.insert(7, &view(heap.snapshot(), a, 0), vec![row(2)], (unique, &none), &[]).unwrap();
        let wait = heap.insert(
            7,
            &view(heap.snapshot(), b, 0),
            vec![row(3), row(2)],
            (unique, &none),
            &[],
        );
        assert_eq!(
            wait.unwrap(),
            Changed::WaitKey { holder: a, index: 0, used: vec![6, 7], waits: 7 }
        );
        heap.abort(a);
        let done = heap.insert(
            7,
            &view(heap.snapshot(), b, 0),
            vec![row(3), row(2)],
            (unique, &none),
            &[6, 7],
        );
        assert_eq!(done.unwrap(), Changed::Done(2));
        heap.commit(b);
        // The keys of the rows that the transaction deleted are free for the transaction.
        let c = heap.begin(1);
        let view_c = view(heap.snapshot(), c, 0);
        let rows = heap.rows(7, &view_c);
        let tids: Vec<u64> = rows
            .iter()
            .filter(|r| matches!(r[0], Value::Int4(2 | 3)))
            .map(|r| match r[1] {
                Value::Int8(tid) => tid.cast_unsigned(),
                _ => 0,
            })
            .collect();
        let changes = tids.iter().map(|&tid| (tid, None)).collect();
        assert_eq!(
            heap.change(7, &view_c, changes, (unique, &[], &none), &[], &mut Vec::new()).unwrap(),
            Changed::Done(2)
        );
        let view_c = view(heap.snapshot(), c, 1);
        assert_eq!(
            heap.insert(7, &view_c, vec![row(2), row(3)], (unique, &none), &[]).unwrap(),
            Changed::Done(2)
        );
    }

    #[test]
    fn a_committed_change_runs_the_statement_again() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let old = heap.snapshot();
        let tid = first_tid(&heap, &view(old.clone(), 0, 0));
        let a = heap.begin(1);
        heap.change(
            7,
            &view(heap.snapshot(), a, 0),
            vec![(tid, None)],
            (&[], &[], &none),
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        heap.commit(a);
        let b = heap.begin(2);
        let change = vec![(tid, Some(vec![Value::Int4(3)]))];
        assert_eq!(
            heap.change(
                7,
                &view(old.clone(), b, 0),
                change.clone(),
                (&[], &[], &none),
                &[],
                &mut Vec::new()
            )
            .unwrap(),
            Changed::Again
        );
        let serializable = View { serializable: true, ..view(old, b, 0) };
        let error = heap
            .change(7, &serializable, change, (&[], &[], &none), &[], &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        assert_eq!(error.message(), "could not serialize access due to concurrent delete");
        assert_eq!(tid_text(1), "(0,1)");
        assert_eq!(tid_text(1 << 16 | 1), "(1,1)");
    }

    #[test]
    fn key_share_goes_over_an_update_that_kept_the_key() {
        let heap = Heap::default();
        let key = Unique {
            oid: 0,
            name: "t_pkey".to_string(),
            columns: vec![0],
            types: vec![oid::INT4],
            shown: "a".to_string(),
            nulls_not_distinct: false,
            derived: None,
            deferrable: None,
        };
        let unique = std::slice::from_ref(&key);
        let row = |a: i32, b: i32| vec![Value::Int4(a), Value::Int4(b)];
        let first = heap.begin(1);
        heap.insert(7, &view(heap.snapshot(), first, 0), vec![row(1, 1)], (unique, &none), &[])
            .unwrap();
        heap.commit(first);
        let old = heap.snapshot();
        let tid = first_tid(&heap, &view(old.clone(), 0, 0));
        // `b` updates the other column, then updates the key of the new version.
        let b = heap.begin(2);
        let update = |tid, values, command| {
            heap.change(
                7,
                &view(heap.snapshot(), b, command),
                vec![(tid, Some(values))],
                (unique, &[], &none),
                &[],
                &mut Vec::new(),
            )
            .unwrap()
        };
        assert_eq!(update(tid, row(1, 2), 0), Changed::Done(1));
        let a = heap.begin(3);
        let lock = |view: &View| heap.lock_row((7, tid), view, (a, KEY_SHARE)).unwrap();
        assert_eq!(lock(&view(old.clone(), a, 0)), RowLocked::Done);
        // `FOR SHARE` waits for the update.
        assert_eq!(
            heap.lock_row((7, tid), &view(old.clone(), a, 0), (a, 2)).unwrap(),
            RowLocked::Wait(b)
        );
        // The lock covers the new version too, so an update of its key waits.
        let next = first_tid(&heap, &view(heap.snapshot(), b, 1));
        assert_eq!(update(next, row(2, 2), 1), Changed::Wait { holder: a, tid: next });
        heap.abort(a);
        assert_eq!(update(next, row(2, 2), 1), Changed::Done(1));
        // The update of the key of the later version conflicts with a new lock.
        let c = heap.begin(4);
        let lock = |view: &View| heap.lock_row((7, tid), view, (c, KEY_SHARE)).unwrap();
        assert_eq!(lock(&view(old.clone(), c, 0)), RowLocked::Wait(b));
        heap.commit(b);
        assert_eq!(lock(&view(old.clone(), c, 0)), RowLocked::Again);
        let serializable = View { serializable: true, ..view(old, c, 0) };
        let error = heap.lock_row((7, tid), &serializable, (c, KEY_SHARE)).unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
    }

    #[test]
    fn rollback_to_a_savepoint_takes_away_its_row_locks() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let snapshot = heap.snapshot();
        let tid = first_tid(&heap, &view(snapshot.clone(), 0, 0));
        let a = heap.begin(2);
        let b = heap.begin(3);
        // `a` takes `FOR KEY SHARE` in command 0, then `FOR NO KEY UPDATE` after a savepoint at command 1.
        let lock = |xid, command, strength| {
            heap.lock_row((7, tid), &view(snapshot.clone(), xid, command), (xid, strength)).unwrap()
        };
        assert_eq!(lock(a, 0, KEY_SHARE), RowLocked::Done);
        assert_eq!(lock(a, 1, NO_KEY_UPDATE), RowLocked::Done);
        assert_eq!(lock(b, 0, NO_KEY_UPDATE), RowLocked::Wait(a));
        assert!(heap.row_blocked((1, 7, tid), (a, NO_KEY_UPDATE)));
        heap.rollback_to(a, 1);
        // The lock of the command before the savepoint stays, and it does not conflict with `FOR NO KEY UPDATE`.
        assert!(!heap.row_blocked((1, 7, tid), (a, NO_KEY_UPDATE)));
        assert!(heap.row_blocked((1, 7, tid), (a, UPDATE)));
        assert_eq!(lock(b, 0, NO_KEY_UPDATE), RowLocked::Done);
        assert_eq!(lock(b, 0, UPDATE), RowLocked::Wait(a));
    }

    #[test]
    fn the_waits_for_a_row_get_it_in_their_order() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let snapshot = heap.snapshot();
        let tid = first_tid(&heap, &view(snapshot.clone(), 0, 0));
        let (a, b, c) = (heap.begin(2), heap.begin(3), heap.begin(4));
        let lock = |xid, strength| {
            heap.lock_row((7, tid), &view(snapshot.clone(), xid, 0), (xid, strength)).unwrap()
        };
        assert_eq!(lock(a, SHARE), RowLocked::Done);
        // `b` waits for `a` and holds the tuple lock, and `c` waits for `b`, which holds the tuple lock until it gets the row.
        assert_eq!(lock(b, UPDATE), RowLocked::Wait(a));
        assert_eq!(lock(c, UPDATE), RowLocked::Wait(b));
        assert!(heap.row_blocked((1, 7, tid), (b, UPDATE)));
        // A lock that does not conflict with `a` does not wait in the queue.
        assert_eq!(lock(c, SHARE), RowLocked::Done);
        // `c` holds a lock on the row, so its upgrade waits for `a` and not for `b`.
        assert_eq!(lock(c, UPDATE), RowLocked::Wait(a));
        heap.abort(a);
        assert_eq!(lock(b, UPDATE), RowLocked::Wait(c));
        heap.abort(c);
        assert_eq!(lock(b, UPDATE), RowLocked::Done);
        assert!(heap.row_blocked((1, 7, tid), (b, UPDATE)));
    }
}

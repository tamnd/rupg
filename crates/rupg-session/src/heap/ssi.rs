//! Serializable snapshot isolation, as `predicate.c` of PostgreSQL does it (spec/11 section 11.5.3).
//!
//! Each transaction in `SERIALIZABLE` has a record from its first snapshot. A read of a table takes a `SIREAD` lock on the table, as the sequential scan of `heap_beginscan` takes it with `PredicateLockRelation`. The lock stays after the commit while a transaction that overlaps it runs. A write into a table that another serializable transaction read records a read-write conflict from the reader to the writer, as `CheckForSerializableConflictIn` does. A read of a table that a concurrent serializable transaction wrote records a conflict from the reader to the writer, as `CheckForSerializableConflictOut` does for the versions of the writer that the scan finds.
//!
//! Two conflicts in a row, `T0 -> T1 -> T2`, where `T2` commits first, can make a cycle. The checks of `OnConflict_CheckForSerializationFailure` at each new conflict and of `PreCommit_CheckForSerializationFailure` at each commit choose the transaction that fails with `40001`, with the same rules and in the same steps as PostgreSQL.

use std::collections::{BTreeSet, HashMap};

use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_common::{Error, SqlState};

/// A table in a database.
pub(crate) type Table = (u32, u32);

/// `PREDICATELOCKTARGETTAG`: what a `SIREAD` lock covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Target {
    /// `PredicateLockRelation`: the whole table, for a sequential scan.
    Relation(Table),
    /// `PredicateLockPage`: a page of the heap of the table, `tid >> 16` of its versions with the generation of the heap.
    Page(Table, u64),
    /// `PredicateLockTID`: the version `tid` of the table, which an index scan found.
    Tuple(Table, u64),
    /// The leaf page of the indexes of the table that an index scan read, as `_bt_first` locks it. The index of a small table has one leaf page, so the lock covers each key of each index of the table, and an insert of an index entry conflicts with it.
    Index(Table),
}

impl Target {
    /// The table of the target.
    fn table(self) -> Table {
        match self {
            Self::Relation(table)
            | Self::Page(table, _)
            | Self::Tuple(table, _)
            | Self::Index(table) => table,
        }
    }
}

impl Persist for Target {
    fn put(&self, out: &mut Vec<u8>) {
        let (kind, table, at) = match *self {
            Self::Relation(table) => (0u8, table, 0),
            Self::Page(table, page) => (1, table, page),
            Self::Tuple(table, tid) => (2, table, tid),
            Self::Index(table) => (3, table, 0),
        };
        (kind, table, at).put(out);
    }

    fn take(input: &mut Reader<'_>) -> rupg_common::Result<Self> {
        let (kind, table, at): (u8, Table, u64) = Persist::take(input)?;
        Ok(match kind {
            0 => Self::Relation(table),
            1 => Self::Page(table, at),
            2 => Self::Tuple(table, at),
            3 => Self::Index(table),
            _ => return Err(corrupt("a SIREAD lock of a prepared transaction has a bad kind")),
        })
    }
}

/// What the log and the image keep of the record of a prepared serializable transaction, as the records `TWOPHASE_PREDICATELOCK_XACT` and `TWOPHASE_PREDICATELOCK_LOCK` keep it: `xmin` of its snapshot, whether it is read-only, the tables that it wrote and the targets of its `SIREAD` locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableSxact {
    pub(crate) xmin: u32,
    pub(crate) read_only: bool,
    pub(crate) writes: Vec<Table>,
    pub(crate) reads: Vec<Target>,
}

rupg_common::persist_struct!(DurableSxact { xmin, read_only, writes, reads });

/// `max_pred_locks_per_page`: the most `SIREAD` locks of versions of one page that a transaction takes before it locks the page.
const MAX_PER_PAGE: usize = 2;

/// `max_pred_locks_per_relation` of -2: half of `max_pred_locks_per_transaction`, the most `SIREAD` locks of pages and versions of one table that a transaction takes before it locks the table.
const MAX_PER_RELATION: usize = 32;

/// A version that an index scan examined, for `HeapCheckForSerializableConflictOut`: its ID, `xmin`, `xmax`, and whether the snapshot of the scan sees it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Examined {
    pub(crate) tid: u64,
    pub(crate) xmin: u32,
    pub(crate) xmax: u32,
    pub(crate) visible: bool,
}

/// `SerCommitSeqNo`: the order of the commits of the serializable transactions.
type SeqNo = u64;

/// `RecoverySerCommitSeqNo`: the order of the prepare of a transaction that a restart prepared again, before each other.
const RECOVERY_SEQ_NO: SeqNo = 1;

/// `FirstNormalSerCommitSeqNo`.
const FIRST_NORMAL_SEQ_NO: SeqNo = 2;

/// `SERIALIZABLEXACT`: the record of a serializable transaction.
#[derive(Debug, Default)]
struct Sxact {
    /// `topXid`: the transaction ID, or 0 before the first write.
    xid: u32,
    /// `xmin` of the snapshot of the transaction.
    xmin: u32,
    /// `SeqNo.lastCommitBeforeSnapshot`.
    last_commit_before_snapshot: SeqNo,
    /// `prepareSeqNo`, from `PreCommit_CheckForSerializationFailure`, or `None` before it.
    prepare: Option<SeqNo>,
    /// `commitSeqNo`, or `None` before the commit.
    commit: Option<SeqNo>,
    /// `SeqNo.earliestOutConflictCommit`.
    earliest_out_conflict_commit: SeqNo,
    /// `finishedBefore`: `nextXid` at the end of the transaction.
    finished_before: u32,
    /// `SXACT_FLAG_READ_ONLY`: declared `READ ONLY`, or committed with no write.
    read_only: bool,
    /// The transaction was declared `READ ONLY` at its start.
    declared_read_only: bool,
    /// `SXACT_FLAG_DOOMED`.
    doomed: bool,
    /// `SXACT_FLAG_CONFLICT_OUT`: a conflict out to a transaction that committed before this one.
    conflict_out: bool,
    /// `MyXactDidWrite`.
    did_write: bool,
    /// `inConflicts`: the transactions that read what this one wrote.
    in_conflicts: BTreeSet<u64>,
    /// `outConflicts`: the transactions that wrote what this one read.
    out_conflicts: BTreeSet<u64>,
    /// The targets of the `SIREAD` locks of the transaction.
    reads: BTreeSet<Target>,
    /// The tables that the transaction wrote.
    writes: BTreeSet<Table>,
    /// The process ID of the session of the transaction.
    pid: i32,
    /// `possibleUnsafeConflicts`: for a transaction declared `READ ONLY`, the writable transactions that ran at its snapshot, and for a writable one, the read-only transactions whose snapshot it ran at.
    possible_unsafe: BTreeSet<u64>,
    /// `SXACT_FLAG_RO_SAFE`: the snapshot of the read-only transaction can see no anomaly, so it needs no `SIREAD` locks.
    ro_safe: bool,
    /// `SXACT_FLAG_RO_UNSAFE`: a writable transaction of [`Sxact::possible_unsafe`] committed with a conflict out to a transaction that committed before the snapshot.
    ro_unsafe: bool,
    /// `SXACT_FLAG_DEFERRABLE_WAITING`: the transaction is `DEFERRABLE` and waits for a safe snapshot.
    deferrable: bool,
    /// `SXACT_FLAG_SUMMARY_CONFLICT_IN`: a conflict in from a transaction that has no record, as a restart leaves a prepared transaction.
    summary_in: bool,
    /// `SXACT_FLAG_SUMMARY_CONFLICT_OUT`: a conflict out to a transaction that has no record and that committed before this one.
    summary_out: bool,
}

impl Sxact {
    /// `SxactIsCommitted`.
    fn committed(&self) -> bool {
        self.commit.is_some()
    }

    /// `SxactIsPrepared`: the transaction passed `PreCommit_CheckForSerializationFailure`.
    fn prepared(&self) -> bool {
        self.prepare.is_some()
    }

    /// `SxactIsCommitted` and not ended: the transaction runs.
    fn active(&self) -> bool {
        !self.committed()
    }
}

/// The serializable transactions, `PredXact`, and the `SIREAD` locks.
#[derive(Debug, Default)]
pub(crate) struct Ssi {
    /// The ID of the next record.
    next_id: u64,
    /// `LastSxactCommitSeqNo`.
    last_commit: SeqNo,
    /// `CanPartialClearThrough`.
    can_partial_clear_through: SeqNo,
    /// `HavePartialClearedThrough`.
    have_partial_cleared_through: SeqNo,
    /// The records by ID: the transactions that run and the committed ones that a running one overlaps.
    xacts: HashMap<u64, Sxact>,
    /// `SerializableXidHash`: the record of each transaction ID.
    by_xid: HashMap<u32, u64>,
    /// The holders of the `SIREAD` locks of each table.
    locks: HashMap<Target, BTreeSet<u64>>,
}

/// `40001` "could not serialize access due to read/write dependencies among transactions" with the reason of PostgreSQL.
fn failure(reason: &str) -> Error {
    Error::new(
        SqlState::T_R_SERIALIZATION_FAILURE,
        "could not serialize access due to read/write dependencies among transactions",
    )
    .with_detail(format!("Reason code: {reason}."))
    .with_hint("The transaction might succeed if retried.")
}

/// `XidIsConcurrent`: true when the snapshot of `xmin`, `xmax` and the running transactions `running` does not see the transaction `xid`.
pub(crate) type Concurrent<'a> = &'a dyn Fn(u32) -> bool;

impl Ssi {
    /// `GetSerializableTransactionSnapshotInt`: the record of a new serializable transaction of the session `pid` whose snapshot has `xmin`. The caller takes the snapshot under the lock of this state. A transaction declared `READ ONLY` while no writable serializable transaction runs needs no record, and the result is 0. Otherwise it has a possible unsafe conflict with each writable one that runs, and with `deferrable` it waits for a safe snapshot.
    pub(crate) fn begin(&mut self, xmin: u32, read_only: bool, deferrable: bool, pid: i32) -> u64 {
        if self.last_commit < FIRST_NORMAL_SEQ_NO - 1 {
            self.last_commit = FIRST_NORMAL_SEQ_NO - 1;
        }
        let writable: BTreeSet<u64> = if read_only {
            let writable: BTreeSet<u64> = self
                .xacts
                .iter()
                .filter(|(_, s)| s.active() && !s.doomed && !s.read_only)
                .map(|(&id, _)| id)
                .collect();
            if writable.is_empty() {
                return 0;
            }
            writable
        } else {
            BTreeSet::new()
        };
        self.next_id += 1;
        let id = self.next_id;
        for &other in &writable {
            if let Some(other) = self.xacts.get_mut(&other) {
                other.possible_unsafe.insert(id);
            }
        }
        self.xacts.insert(
            id,
            Sxact {
                xmin,
                last_commit_before_snapshot: self.last_commit,
                read_only,
                declared_read_only: read_only,
                pid,
                possible_unsafe: writable,
                deferrable: read_only && deferrable,
                ..Sxact::default()
            },
        );
        id
    }

    /// `GetSafeSnapshot`: `Some(true)` when the snapshot of the read-only transaction `id` is safe, `Some(false)` when it is unsafe and the transaction must take a new one, and `None` while a writable transaction of its snapshot runs. A safe transaction needs its record no more, and loses it.
    pub(crate) fn safety(&mut self, id: u64) -> Option<bool> {
        let Some(me) = self.get(id) else { return Some(true) };
        if me.ro_unsafe {
            return Some(false);
        }
        if me.ro_safe || me.possible_unsafe.is_empty() {
            self.end(id, false, 0);
            return Some(true);
        }
        None
    }

    /// `GetSafeSnapshotBlockingPids`: the process IDs of the sessions of the writable transactions that the transaction of the session `pid` waits for, while it waits for a safe snapshot.
    pub(crate) fn safe_snapshot_blockers(&self, pid: i32) -> Vec<i32> {
        let Some(me) = self.xacts.values().find(|s| s.pid == pid && s.deferrable && s.active())
        else {
            return Vec::new();
        };
        me.possible_unsafe
            .iter()
            .filter_map(|&other| self.get(other))
            .map(|other| other.pid)
            .filter(|&pid| pid > 0)
            .collect()
    }

    /// `FlagSxactUnsafe`: the snapshot of the read-only transaction `id` can see an anomaly, and its possible unsafe conflicts go.
    fn flag_unsafe(&mut self, id: u64) {
        let Some(me) = self.xacts.get_mut(&id) else { return };
        me.ro_unsafe = true;
        let others = std::mem::take(&mut me.possible_unsafe);
        for other in others {
            if let Some(other) = self.xacts.get_mut(&other) {
                other.possible_unsafe.remove(&id);
            }
        }
    }

    /// The end of the writable transaction `id` for the read-only transactions of its possible unsafe conflicts, as `ReleasePredicateLocks` checks them: a commit with a write and a conflict out to a transaction that committed before the snapshot of one of them makes it unsafe. Otherwise the conflict goes, and a read-only transaction with no other one is safe.
    fn check_read_only(&mut self, id: u64, commit: bool) {
        let Some(me) = self.xacts.get_mut(&id) else { return };
        let readers = std::mem::take(&mut me.possible_unsafe);
        let (did_write, conflict_out, earliest) =
            (me.did_write, me.conflict_out, me.earliest_out_conflict_commit);
        for reader in readers {
            let Some(ro) = self.get(reader) else { continue };
            if commit && did_write && conflict_out && earliest <= ro.last_commit_before_snapshot {
                self.flag_unsafe(reader);
            } else if let Some(ro) = self.xacts.get_mut(&reader) {
                ro.possible_unsafe.remove(&id);
                if ro.possible_unsafe.is_empty() {
                    ro.ro_safe = true;
                }
            }
        }
    }

    /// The record `id`, which a running transaction has.
    fn get(&self, id: u64) -> Option<&Sxact> {
        self.xacts.get(&id)
    }

    /// `CheckForSerializableConflictOutNeeded`: the error of a transaction that another one doomed.
    fn check_doomed(&self, id: u64, step: &str) -> Result<(), Error> {
        if self.get(id).is_some_and(|me| me.doomed) {
            return Err(failure(&format!("Canceled on identification as a pivot, {step}")));
        }
        Ok(())
    }

    /// `PredicateLockRelation` and `CheckForSerializableConflictOut` for a scan of the table `target` by the transaction `id`: the transaction takes a `SIREAD` lock on the table, and each concurrent serializable transaction that wrote the table gets a conflict from this one. `concurrent` is `XidIsConcurrent` for the snapshot of the transaction.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or the conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn read(
        &mut self,
        id: u64,
        table: Table,
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        let Some(me) = self.xacts.get_mut(&id) else { return Ok(()) };
        // `SerializationNeededForRead`: a read-only transaction whose snapshot is safe opts out of serializable snapshot isolation.
        if me.ro_safe {
            self.end(id, false, 0);
            return Ok(());
        }
        self.acquire(id, Target::Relation(table));
        let writers: Vec<u64> = self
            .xacts
            .iter()
            .filter(|&(&w, sxact)| w != id && sxact.xid != 0 && sxact.writes.contains(&table))
            .map(|(&w, _)| w)
            .collect();
        if writers.is_empty() {
            return Ok(());
        }
        self.check_doomed(id, "during conflict out checking")?;
        for writer in writers {
            self.conflict_out(id, writer, concurrent)?;
        }
        Ok(())
    }

    /// An index scan of the table `table` by the transaction `id`, whose transaction ID is `own`, that examined the versions `versions`: the transaction locks the leaf page of the index, as `_bt_first` does, and each version that it sees, as `heap_hot_search_buffer` does with `PredicateLockTID`, and gets a conflict to each concurrent serializable transaction that wrote a version it examined, `HeapCheckForSerializableConflictOut`.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or a conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn read_versions(
        &mut self,
        id: u64,
        (table, own): (Table, u32),
        versions: &[Examined],
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        if self.xacts.contains_key(&id) && !self.xacts[&id].ro_safe {
            self.acquire(id, Target::Index(table));
        }
        self.fetched(id, (table, own), versions, concurrent)
    }

    /// `heap_fetch` of the versions `versions` of the table `table` by the transaction `id`, whose transaction ID is `own`, as `ExecCheckTIDVisible` fetches the version that an `INSERT ... ON CONFLICT` found: [`Ssi::read_versions`] with no lock of the index.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or a conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn fetched(
        &mut self,
        id: u64,
        (table, own): (Table, u32),
        versions: &[Examined],
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        let Some(me) = self.xacts.get(&id) else { return Ok(()) };
        if me.ro_safe {
            self.end(id, false, 0);
            return Ok(());
        }
        let mut writers = BTreeSet::new();
        for version in versions {
            // `PredicateLockTID`: a version of the transaction itself needs no lock.
            if version.visible && (own == 0 || version.xmin != own) {
                self.acquire(id, Target::Tuple(table, version.tid));
            }
            // A version that the snapshot sees conflicts with the transaction that deleted it, and one that it does not see with the transaction that inserted it.
            let xid = if version.visible { version.xmax } else { version.xmin };
            if xid == 0 || xid == own || !concurrent(xid) {
                continue;
            }
            if let Some(&writer) = self.by_xid.get(&xid)
                && writer != id
            {
                writers.insert(writer);
            }
        }
        if writers.is_empty() {
            return Ok(());
        }
        self.check_doomed(id, "during conflict out checking")?;
        for writer in writers {
            self.conflict_out(id, writer, concurrent)?;
        }
        Ok(())
    }

    /// `PredicateLockAcquire` of the target `target` for the transaction `id`. A lock that a lock of the transaction on its page or table covers is not taken, `CoarserLockCovers`. A lock of a page or a table takes the place of the locks of the transaction that it covers, `DeleteChildTargetLocks`. More than [`MAX_PER_PAGE`] locks of versions of a page become a lock of the page, and more than [`MAX_PER_RELATION`] locks of pages and versions of a table become a lock of the table, `CheckAndPromotePredicateLockRequest`.
    fn acquire(&mut self, id: u64, target: Target) {
        let Some(me) = self.xacts.get_mut(&id) else { return };
        let table = target.table();
        let covered = match target {
            Target::Relation(_) | Target::Index(_) => false,
            Target::Page(..) => me.reads.contains(&Target::Relation(table)),
            Target::Tuple(_, tid) => {
                me.reads.contains(&Target::Relation(table))
                    || me.reads.contains(&Target::Page(table, tid >> 16))
            }
        };
        if covered || !me.reads.insert(target) {
            return;
        }
        self.locks.entry(target).or_default().insert(id);
        match target {
            Target::Relation(_) => {
                let children: Vec<Target> = self.children(id, table, None);
                self.drop_locks(id, &children);
            }
            Target::Page(_, page) => {
                let children: Vec<Target> = self.children(id, table, Some(page));
                self.drop_locks(id, &children);
                if self.children(id, table, None).len() > MAX_PER_RELATION {
                    self.acquire(id, Target::Relation(table));
                }
            }
            Target::Tuple(_, tid) => {
                let page = tid >> 16;
                if self.children(id, table, Some(page)).len() > MAX_PER_PAGE {
                    self.acquire(id, Target::Page(table, page));
                } else if self.children(id, table, None).len() > MAX_PER_RELATION {
                    self.acquire(id, Target::Relation(table));
                }
            }
            Target::Index(_) => {}
        }
    }

    /// The locks of the transaction `id` that a lock of the table `table`, or of its page `page`, covers: the locks of its versions, and of its pages for the table.
    fn children(&self, id: u64, table: Table, page: Option<u64>) -> Vec<Target> {
        let Some(me) = self.xacts.get(&id) else { return Vec::new() };
        let tuples = match page {
            Some(page) => {
                Target::Tuple(table, page << 16)..=Target::Tuple(table, (page << 16) | 0xffff)
            }
            None => Target::Tuple(table, 0)..=Target::Tuple(table, u64::MAX),
        };
        let mut children: Vec<Target> = me.reads.range(tuples).copied().collect();
        if page.is_none() {
            children.extend(me.reads.range(Target::Page(table, 0)..=Target::Page(table, u64::MAX)));
        }
        children
    }

    /// The locks `targets` of the transaction `id` go.
    fn drop_locks(&mut self, id: u64, targets: &[Target]) {
        let Some(me) = self.xacts.get_mut(&id) else { return };
        for target in targets {
            me.reads.remove(target);
            if let Some(holders) = self.locks.get_mut(target) {
                holders.remove(&id);
                if holders.is_empty() {
                    self.locks.remove(target);
                }
            }
        }
    }

    /// `CheckForSerializableConflictOut` of the reader `me` for a version of the transaction of the record `writer`.
    fn conflict_out(
        &mut self,
        me: u64,
        writer: u64,
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        let (Some(reader), Some(sxact)) = (self.get(me), self.get(writer)) else { return Ok(()) };
        if sxact.doomed {
            // A transaction that rolls back makes no conflict.
            return Ok(());
        }
        if !concurrent(sxact.xid) {
            return Ok(());
        }
        // The writer has a conflict out to a transaction with no record, which committed first at a time that the snapshot of the reader does not tell, so one of the two fails.
        if sxact.summary_out {
            if sxact.prepared() {
                return Err(failure("Canceled on conflict out to old pivot"));
            }
            if let Some(w) = self.xacts.get_mut(&writer) {
                w.doomed = true;
            }
            return Ok(());
        }
        // A read-only transaction appears to run first when the writer committed with no conflict out to a transaction that committed before the snapshot of the reader.
        if reader.read_only
            && sxact.committed()
            && (!sxact.conflict_out
                || reader.last_commit_before_snapshot < sxact.earliest_out_conflict_commit)
        {
            return Ok(());
        }
        if reader.out_conflicts.contains(&writer) {
            return Ok(());
        }
        self.flag_rw_conflict(me, writer, me)
    }

    /// `CheckForSerializableConflictIn` for a write of the table `table` by the transaction `id` with the transaction ID `xid`: each other transaction with a `SIREAD` lock on one of the targets `targets` that overlaps this one gets a conflict to this one. With no targets, a lock of any target of the table counts, as `CheckTableForSerializableConflictIn` does. `snapshot_xmin` is `xmin` of the snapshot of the transaction.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or a conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn write(
        &mut self,
        (id, xid): (u64, u32),
        table: Table,
        targets: Option<&[Target]>,
        snapshot_xmin: u32,
    ) -> Result<(), Error> {
        if !self.xacts.contains_key(&id) {
            return Ok(());
        }
        self.check_doomed(id, "during conflict in checking")?;
        if let Some(me) = self.xacts.get_mut(&id) {
            me.did_write = true;
            if me.xid == 0 && xid != 0 {
                me.xid = xid;
                self.by_xid.insert(xid, id);
            }
            me.writes.insert(table);
        }
        let mut holders = BTreeSet::new();
        match targets {
            Some(targets) => {
                for target in targets {
                    holders.extend(self.locks.get(target).into_iter().flatten());
                }
            }
            None => {
                for (target, held) in &self.locks {
                    if target.table() == table {
                        holders.extend(held);
                    }
                }
            }
        }
        holders.remove(&id);
        for holder in holders {
            let Some(sxact) = self.get(holder) else { continue };
            // `CheckTargetForConflictsIn`: a committed reader overlaps the writer while it ended after `xmin` of the snapshot of the writer.
            if !sxact.doomed
                && (!sxact.committed() || snapshot_xmin < sxact.finished_before)
                && !sxact.out_conflicts.contains(&id)
            {
                self.flag_rw_conflict(holder, id, id)?;
            }
        }
        Ok(())
    }

    /// `FlagRWConflict`: the conflict from `reader` to `writer`, after the check of `OnConflict_CheckForSerializationFailure`. `me` is the transaction of the session.
    fn flag_rw_conflict(&mut self, reader: u64, writer: u64, me: u64) -> Result<(), Error> {
        self.on_conflict_check(reader, writer, me)?;
        if let Some(r) = self.xacts.get_mut(&reader) {
            r.out_conflicts.insert(writer);
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.in_conflicts.insert(reader);
        }
        Ok(())
    }

    /// `OnConflict_CheckForSerializationFailure`: whether the new conflict from `reader` to `writer` makes a dangerous structure, which fails the session of `me` or dooms the writer.
    fn on_conflict_check(&mut self, reader: u64, writer: u64, me: u64) -> Result<(), Error> {
        let (Some(r), Some(w)) = (self.get(reader), self.get(writer)) else { return Ok(()) };
        // `R -> W -> T2`, where `W` committed with a conflict out to a transaction that committed before it.
        let mut failed = w.committed() && (w.conflict_out || w.summary_out);
        if !failed {
            // `W` is a pivot with a conflict out to `T2`, which committed first. A conflict out with no record may be one.
            failed = w.summary_out
                || w.out_conflicts.iter().filter_map(|t2| self.get(*t2)).any(|t2| {
                    t2.prepare.is_some_and(|t2_prepare| {
                        r.commit.is_none_or(|c| t2_prepare <= c)
                            && w.commit.is_none_or(|c| t2_prepare <= c)
                            && (!r.read_only || t2_prepare <= r.last_commit_before_snapshot)
                    })
                });
        }
        // `T0 -> R -> W`, where `W` committed first.
        if !failed
            && let Some(w_prepare) = w.prepare
            && !r.read_only
        {
            failed = r.summary_in
                || r.in_conflicts.iter().filter_map(|t0| self.get(*t0)).any(|t0| {
                    !t0.doomed
                        && t0.commit.is_none_or(|c| c >= w_prepare)
                        && (!t0.read_only || t0.last_commit_before_snapshot >= w_prepare)
                });
        }
        if !failed {
            return Ok(());
        }
        if me == writer {
            return Err(failure("Canceled on identification as a pivot, during write"));
        }
        if w.prepared() {
            return Err(failure(&format!(
                "Canceled on conflict out to pivot {}, during read",
                w.xid
            )));
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.doomed = true;
        }
        Ok(())
    }

    /// `PreCommit_CheckForSerializationFailure`: before the commit of the transaction `id`, a doomed transaction fails, and a pivot that a conflict to this one makes dangerous is doomed. The transaction then has its `prepareSeqNo`.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed.
    pub(crate) fn pre_commit(&mut self, id: u64) -> Result<(), Error> {
        let Some(me) = self.get(id) else { return Ok(()) };
        if me.doomed {
            return Err(failure("Canceled on identification as a pivot, during commit attempt"));
        }
        let mut doom = Vec::new();
        for &near in &me.in_conflicts {
            let Some(pivot) = self.get(near) else { continue };
            if pivot.committed() || pivot.doomed {
                continue;
            }
            let dangerous = pivot.in_conflicts.iter().any(|&far| {
                far == id
                    || self.get(far).is_some_and(|t| !t.committed() && !t.read_only && !t.doomed)
            });
            if dangerous {
                if pivot.prepared() {
                    return Err(failure(
                        "Canceled on commit attempt with conflict in from prepared pivot",
                    ));
                }
                doom.push(near);
            }
        }
        for near in doom {
            if let Some(pivot) = self.xacts.get_mut(&near) {
                pivot.doomed = true;
            }
        }
        self.last_commit += 1;
        let seq = self.last_commit;
        if let Some(me) = self.xacts.get_mut(&id) {
            me.prepare = Some(seq);
        }
        Ok(())
    }

    /// `AtPrepare_PredicateLocks`: what the log keeps of the record `id` of a transaction that prepares, or `None` when it has no record.
    pub(crate) fn durable(&self, id: u64) -> Option<DurableSxact> {
        let me = self.get(id)?;
        Some(DurableSxact {
            xmin: me.xmin,
            read_only: me.read_only,
            writes: me.writes.iter().copied().collect(),
            reads: me.reads.iter().copied().collect(),
        })
    }

    /// `predicatelock_twophase_recover`: the record of the prepared transaction `xid` that a restart prepares again, with its `SIREAD` locks. Its conflicts are not known, so it has a conflict in and a conflict out with no record, and it is prepared before each other transaction. The result is the ID of the record.
    pub(crate) fn recover(&mut self, xid: u32, durable: &DurableSxact) -> u64 {
        if self.last_commit < FIRST_NORMAL_SEQ_NO - 1 {
            self.last_commit = FIRST_NORMAL_SEQ_NO - 1;
        }
        self.next_id += 1;
        let id = self.next_id;
        for &target in &durable.reads {
            self.locks.entry(target).or_default().insert(id);
        }
        self.by_xid.insert(xid, id);
        self.xacts.insert(
            id,
            Sxact {
                xid,
                xmin: durable.xmin,
                last_commit_before_snapshot: RECOVERY_SEQ_NO,
                prepare: Some(RECOVERY_SEQ_NO),
                read_only: durable.read_only,
                declared_read_only: durable.read_only,
                // `predicatelock_twophase_postcommit` takes it that the transaction wrote.
                did_write: true,
                reads: durable.reads.iter().copied().collect(),
                writes: durable.writes.iter().copied().collect(),
                summary_in: true,
                summary_out: true,
                ..Sxact::default()
            },
        );
        id
    }

    /// The smallest `xmin` of the transactions that run, `SxactGlobalXmin`, or `None` when none runs.
    fn global_xmin(&self) -> Option<u32> {
        self.xacts.values().filter(|s| s.active()).map(|s| s.xmin).min()
    }

    /// `ReleasePredicateLocks`: the end of the transaction `id`, with `next_xid` as `nextXid`. A rollback removes its record. A commit keeps the record and its `SIREAD` locks while a transaction that overlaps it runs.
    pub(crate) fn end(&mut self, id: u64, commit: bool, next_xid: u32) {
        let Some(me) = self.get(id) else { return };
        let old_global = self.global_xmin();
        let my_xmin = me.xmin;
        let declared_read_only = me.declared_read_only;
        if !commit {
            if !declared_read_only {
                self.check_read_only(id, false);
            }
            self.release(id, false);
        } else {
            self.last_commit += 1;
            let seq = self.last_commit;
            let last_commit = self.last_commit;
            let Some(me) = self.xacts.get_mut(&id) else { return };
            me.finished_before = next_xid;
            me.commit = Some(seq);
            if !me.did_write {
                me.read_only = true;
            }
            let read_only = me.read_only;
            if !read_only && me.summary_out {
                // The transaction that the conflict out with no record goes to is not known, so it may be the first one.
                me.earliest_out_conflict_commit = FIRST_NORMAL_SEQ_NO;
                me.conflict_out = true;
            }
            let outs: Vec<u64> = me.out_conflicts.iter().copied().collect();
            let ins: Vec<u64> = me.in_conflicts.iter().copied().collect();
            let mut earliest = me.earliest_out_conflict_commit;
            let mut conflict_out = me.conflict_out;
            for out in outs {
                let Some(t) = self.get(out) else { continue };
                if !read_only && t.committed() {
                    let t_prepare = t.prepare.unwrap_or(0);
                    if !conflict_out || t_prepare < earliest {
                        earliest = t_prepare;
                    }
                    conflict_out = true;
                }
                if t.committed() || t.last_commit_before_snapshot >= last_commit {
                    self.release_conflict(id, out);
                }
            }
            for into in ins {
                let Some(t) = self.get(into) else { continue };
                if t.committed() || t.read_only {
                    self.release_conflict(into, id);
                }
            }
            if let Some(me) = self.xacts.get_mut(&id) {
                me.earliest_out_conflict_commit = earliest;
                me.conflict_out = conflict_out;
            }
            if !declared_read_only {
                self.check_read_only(id, true);
            }
        }
        if !declared_read_only && !self.xacts.values().any(|s| s.active() && !s.declared_read_only)
        {
            self.can_partial_clear_through = self.last_commit;
        }
        let new_global = self.global_xmin();
        if old_global == Some(my_xmin) && new_global != old_global {
            self.clear_old(new_global);
        }
    }

    /// `ClearOldPredicateLocks`: the committed transactions that no running transaction overlaps go. While only read-only transactions run, a committed transaction loses its `SIREAD` locks and its conflicts in.
    fn clear_old(&mut self, global_xmin: Option<u32>) {
        let mut finished: Vec<(SeqNo, u64)> =
            self.xacts.iter().filter_map(|(&id, s)| s.commit.map(|c| (c, id))).collect();
        finished.sort_unstable();
        for (commit, id) in finished {
            let Some(sxact) = self.get(id) else { continue };
            if global_xmin.is_none_or(|g| sxact.finished_before <= g) {
                self.release(id, false);
            } else if commit > self.have_partial_cleared_through
                && commit <= self.can_partial_clear_through
            {
                let read_only = sxact.read_only;
                self.release(id, !read_only);
                self.have_partial_cleared_through = commit;
            }
        }
    }

    /// `ReleaseOneSerializableXact`: the `SIREAD` locks and the conflicts in of the transaction `id` go, and with `partial` false, its conflicts out and its record too.
    fn release(&mut self, id: u64, partial: bool) {
        let Some(sxact) = self.xacts.get_mut(&id) else { return };
        let reads = std::mem::take(&mut sxact.reads);
        let ins: Vec<u64> = sxact.in_conflicts.iter().copied().collect();
        let outs: Vec<u64> =
            if partial { Vec::new() } else { sxact.out_conflicts.iter().copied().collect() };
        let unsafe_with: Vec<u64> = if partial {
            Vec::new()
        } else {
            std::mem::take(&mut sxact.possible_unsafe).into_iter().collect()
        };
        for other in unsafe_with {
            if let Some(other) = self.xacts.get_mut(&other) {
                other.possible_unsafe.remove(&id);
            }
        }
        for target in reads {
            if let Some(holders) = self.locks.get_mut(&target) {
                holders.remove(&id);
                if holders.is_empty() {
                    self.locks.remove(&target);
                }
            }
        }
        for into in ins {
            self.release_conflict(into, id);
        }
        for out in outs {
            self.release_conflict(id, out);
        }
        if !partial && let Some(sxact) = self.xacts.remove(&id) {
            self.by_xid.remove(&sxact.xid);
        }
    }

    /// `ReleaseRWConflict`: the conflict from `reader` to `writer` goes.
    fn release_conflict(&mut self, reader: u64, writer: u64) {
        if let Some(r) = self.xacts.get_mut(&reader) {
            r.out_conflicts.remove(&writer);
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.in_conflicts.remove(&reader);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Table = (1, 100);
    const U: Table = (1, 101);

    /// Write skew: each transaction reads the table that the other writes, and the second commit fails.
    #[test]
    fn write_skew_fails_at_the_second_commit() {
        let mut ssi = Ssi::default();
        let a = ssi.begin(10, false, false, 1);
        let b = ssi.begin(10, false, false, 1);
        let none = |_: u32| true;
        ssi.read(a, T, &none).unwrap();
        ssi.read(b, U, &none).unwrap();
        ssi.read(a, U, &none).unwrap();
        ssi.read(b, T, &none).unwrap();
        ssi.write((a, 10), T, None, 10).unwrap();
        ssi.write((b, 11), U, None, 10).unwrap();
        ssi.pre_commit(a).unwrap();
        ssi.end(a, true, 12);
        let error = ssi.pre_commit(b).unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        ssi.end(b, false, 12);
        assert!(ssi.xacts.is_empty());
    }

    /// One conflict alone is no anomaly.
    #[test]
    fn one_conflict_commits() {
        let mut ssi = Ssi::default();
        let a = ssi.begin(10, false, false, 1);
        let b = ssi.begin(10, false, false, 1);
        let none = |_: u32| true;
        ssi.read(a, T, &none).unwrap();
        ssi.write((b, 11), T, None, 10).unwrap();
        ssi.pre_commit(b).unwrap();
        ssi.end(b, true, 12);
        ssi.pre_commit(a).unwrap();
        ssi.end(a, true, 12);
        assert!(ssi.xacts.is_empty() && ssi.locks.is_empty());
    }

    /// A read-only transaction with no writable one at its snapshot needs no record.
    #[test]
    fn read_only_alone_is_safe() {
        let mut ssi = Ssi::default();
        assert_eq!(ssi.begin(10, true, true, 1), 0);
    }

    /// `read-only-anomaly-3`: the snapshot of the deferrable reader is unsafe after a writer commits with a conflict out to a transaction that committed before the snapshot, and the next one is safe.
    #[test]
    fn deferrable_waits_for_a_safe_snapshot() {
        let mut ssi = Ssi::default();
        let none = |_: u32| true;
        let s1 = ssi.begin(10, false, false, 1);
        let s2 = ssi.begin(10, false, false, 2);
        ssi.read(s2, T, &none).unwrap();
        ssi.write((s1, 10), U, None, 10).unwrap();
        ssi.read(s2, U, &none).unwrap();
        ssi.pre_commit(s1).unwrap();
        ssi.end(s1, true, 11);
        let s3 = ssi.begin(11, true, true, 3);
        assert_eq!(ssi.safety(s3), None);
        assert_eq!(ssi.safe_snapshot_blockers(3), vec![2]);
        ssi.write((s2, 11), T, None, 10).unwrap();
        ssi.pre_commit(s2).unwrap();
        ssi.end(s2, true, 12);
        assert_eq!(ssi.safety(s3), Some(false));
        ssi.end(s3, false, 12);
        assert_eq!(ssi.begin(12, true, true, 3), 0);
    }

    /// A writer that ends with no conflict out makes the snapshot of the reader safe, and the reader loses its record.
    #[test]
    fn deferrable_safe_after_a_clean_writer() {
        let mut ssi = Ssi::default();
        let w = ssi.begin(10, false, false, 1);
        let r = ssi.begin(10, true, true, 2);
        assert_eq!(ssi.safety(r), None);
        ssi.write((w, 10), T, None, 10).unwrap();
        ssi.pre_commit(w).unwrap();
        ssi.end(w, true, 11);
        assert_eq!(ssi.safety(r), Some(true));
        assert!(ssi.xacts.is_empty());
    }

    /// A version that the snapshot sees, at slot `slot` of page 0.
    fn seen(slot: u64) -> Examined {
        Examined { tid: slot, xmin: 1, xmax: 0, visible: true }
    }

    /// `read-only-anomaly-2`: index scans lock only the rows that they find, so an update of a row that no other transaction read makes no conflict.
    #[test]
    fn index_scans_lock_rows() {
        let mut ssi = Ssi::default();
        let none = |_| true;
        let s1 = ssi.begin(10, false, false, 1);
        let s2 = ssi.begin(10, false, false, 2);
        let (x, y) = (seen(1), seen(2));
        ssi.read_versions(s2, (T, 0), &[x], &none).unwrap();
        ssi.read_versions(s2, (T, 0), &[y], &none).unwrap();
        ssi.read_versions(s1, (T, 0), &[y], &none).unwrap();
        let tuple_y = [Target::Relation(T), Target::Page(T, 0), Target::Tuple(T, 2)];
        ssi.write((s1, 10), T, Some(&tuple_y), 10).unwrap();
        assert!(ssi.get(s2).unwrap().out_conflicts.contains(&s1));
        ssi.pre_commit(s1).unwrap();
        ssi.end(s1, true, 12);
        let tuple_x = [Target::Relation(T), Target::Page(T, 0), Target::Tuple(T, 1)];
        ssi.write((s2, 11), T, Some(&tuple_x), 10).unwrap();
        ssi.pre_commit(s2).unwrap();
    }

    /// A third lock of a version of a page locks the page, and an insert conflicts with the lock of the index.
    #[test]
    fn locks_of_versions_become_a_lock_of_the_page() {
        let mut ssi = Ssi::default();
        let none = |_| true;
        let r = ssi.begin(10, false, false, 1);
        let w = ssi.begin(10, false, false, 2);
        ssi.read_versions(r, (T, 0), &[seen(1), seen(2)], &none).unwrap();
        assert!(ssi.get(r).unwrap().reads.contains(&Target::Tuple(T, 2)));
        ssi.read_versions(r, (T, 0), &[seen(3)], &none).unwrap();
        let reads: Vec<Target> = ssi.get(r).unwrap().reads.iter().copied().collect();
        assert_eq!(reads, [Target::Page(T, 0), Target::Index(T)]);
        ssi.read_versions(r, (T, 0), &[seen(4)], &none).unwrap();
        assert_eq!(ssi.get(r).unwrap().reads.len(), 2);
        ssi.write((w, 11), T, Some(&[Target::Relation(T), Target::Index(T)]), 10).unwrap();
        assert!(ssi.get(r).unwrap().out_conflicts.contains(&w));
    }

    /// An index scan conflicts with the transaction that deleted a version that it sees, and not with one that wrote another version.
    #[test]
    fn index_scans_conflict_out_with_the_versions_they_examine() {
        let mut ssi = Ssi::default();
        let none = |_| true;
        let w = ssi.begin(10, false, false, 1);
        let r = ssi.begin(10, false, false, 2);
        ssi.write((w, 11), T, Some(&[Target::Tuple(T, 5)]), 10).unwrap();
        ssi.read_versions(r, (T, 0), &[seen(1)], &none).unwrap();
        assert!(ssi.get(r).unwrap().out_conflicts.is_empty());
        let deleted = Examined { tid: 5, xmin: 1, xmax: 11, visible: true };
        ssi.read_versions(r, (T, 0), &[deleted], &none).unwrap();
        assert!(ssi.get(r).unwrap().out_conflicts.contains(&w));
    }
}

//! Transactions with snapshot reads over the hot store (spec/11 sections 11.3 to 11.5).
//!
//! A writer changes a row in place. The version header of the row names the writer until it commits, and an undo record keeps the version before the change. Commit writes the commit timestamp into the header of each row that the writer changed. A reader with snapshot `S` sees a version with a commit timestamp at or below `S`, or a version that it wrote in an earlier command. For any other version it applies undo records until it gets one that it sees.
//!
//! This is the first MVCC of M1. The slot of an owner is its transaction id, which the node does not use again. `READ COMMITTED` and `REPEATABLE READ` are here. `SERIALIZABLE`, savepoints, row locks with `FOR UPDATE` and the precise cleanup rule come later. With a log, each commit writes one commit block (see `redo.rs`). At M1, cleanup removes the undo of a commit when every snapshot sees that commit (section 11.4.3 has the M1 note).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};

use rupg_common::{Error, Hlc, Oid, Result, RowId, SqlState, Xid};
use rupg_file::PageAccess;
use rupg_hot::{Row, RowIdBlock, VersionHeader};
use rupg_log::{Block, BlockKind, Log};
use rupg_table::Table;
use rupg_types::Datum;

use crate::clock::HlcClock;
use crate::redo::Changes;
use crate::undo::{Change, Record, Undo};
use crate::visible::Commits;

/// The isolation level of a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Isolation {
    /// Each command takes a new snapshot. A writer that finds a newer committed version changes that version.
    ReadCommitted,
    /// The transaction takes one snapshot. A writer that finds a newer committed version fails with SQLSTATE `40001`.
    RepeatableRead,
}

/// The number of row latches. A writer holds the latch of a row while it reads the header and writes the change.
const STRIPES: usize = 64;

/// The log ring of each commit. At M1 every commit goes to ring 0. A ring for each worker comes with the workers.
pub(crate) const RING: u16 = 0;

/// The most times that a reader reads a row again because a rollback removed an undo record under it.
const RETRIES: usize = 1_000;

/// The transactions of a node over the pages `P`.
pub struct Transactions<P> {
    pages: Arc<P>,
    commits: Commits,
    undo: Undo,
    state: Mutex<State>,
    ended: Condvar,
    stripes: Vec<Mutex<()>>,
    log: Option<Arc<Log>>,
    /// A commit holds it while it takes its timestamp and places its block, so the blocks in a ring are in timestamp order (spec/11 section 11.14.2).
    pub(crate) gate: Mutex<()>,
    /// The commit timestamp and the end of each block in the ring that is after the redo position, in ring order. A commit adds to it under the gate.
    pub(crate) placed: Mutex<VecDeque<(Hlc, u64)>>,
    /// Each change to the pages holds it shared. A checkpoint holds it exclusive, so the pages do not change while it writes them.
    pub(crate) quiet: RwLock<()>,
    /// The snapshot of the checkpoint that runs now, or 0. The page filter writes the versions of this snapshot.
    pub(crate) filter_at: AtomicU64,
}

impl<P> fmt::Debug for Transactions<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transactions")
            .field("visible", &self.commits.visible())
            .field("undo", &self.undo.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct State {
    next_xid: Xid,
    /// The writers that did not end, each with the writer that it waits for.
    writers: BTreeMap<Xid, Option<Xid>>,
    /// The snapshots in use, each with the number of users.
    snapshots: BTreeMap<Hlc, usize>,
    /// The undo of each commit that a snapshot can still need, by commit timestamp.
    kept: BTreeMap<Hlc, Kept>,
}

#[derive(Debug)]
struct Kept {
    undo: Vec<u64>,
    deleted: Vec<(Arc<Table>, RowId)>,
}

impl State {
    fn take_snapshot(&mut self, commits: &Commits) -> Hlc {
        let s = commits.visible();
        *self.snapshots.entry(s).or_default() += 1;
        s
    }

    fn drop_snapshot(&mut self, s: Hlc) {
        if let Some(n) = self.snapshots.get_mut(&s) {
            *n -= 1;
            if *n == 0 {
                self.snapshots.remove(&s);
            }
        }
    }
}

impl<P: PageAccess> Transactions<P> {
    /// The transactions over `pages`. Commit timestamps come from `clock`, and transaction ids start at `next_xid`.
    pub fn new(pages: Arc<P>, clock: HlcClock, next_xid: Xid) -> Transactions<P> {
        let state = State {
            next_xid,
            writers: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            kept: BTreeMap::new(),
        };
        Transactions {
            pages,
            commits: Commits::new(clock),
            undo: Undo::new(),
            state: Mutex::new(state),
            ended: Condvar::new(),
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
            log: None,
            gate: Mutex::new(()),
            placed: Mutex::new(VecDeque::new()),
            quiet: RwLock::new(()),
            filter_at: AtomicU64::new(0),
        }
    }

    /// Writes each commit to `log`. A commit returns when its block is safe. With no log, a commit is in memory only.
    pub fn with_log(mut self, log: Arc<Log>) -> Transactions<P> {
        self.log = Some(log);
        self
    }

    /// The log, if there is one.
    pub fn log(&self) -> Option<&Arc<Log>> {
        self.log.as_ref()
    }

    /// The pages.
    pub fn pages(&self) -> &Arc<P> {
        &self.pages
    }

    /// The commit timestamps and the `visible` word.
    pub fn commits(&self) -> &Commits {
        &self.commits
    }

    /// The next transaction id. The log records it.
    pub fn next_xid(&self) -> Xid {
        self.lock().next_xid
    }

    /// The number of undo records in memory.
    pub fn undo_records(&self) -> usize {
        self.undo.len()
    }

    /// The oldest snapshot that a transaction can use now. Every commit at or below it is visible to every snapshot.
    pub fn horizon(&self) -> Hlc {
        let state = self.lock();
        let visible = self.commits.visible();
        state.snapshots.keys().next().map_or(visible, |&s| s.min(visible))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the gate that a checkpoint closes. A thread that holds a row latch takes the row latch first, and a commit takes this before the commit gate.
    fn quiet(&self) -> RwLockReadGuard<'_, ()> {
        self.quiet.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes a snapshot that cleanup respects until [`Transactions::release`].
    pub(crate) fn hold(&self) -> Hlc {
        self.lock().take_snapshot(&self.commits)
    }

    /// Drops a snapshot from [`Transactions::hold`].
    pub(crate) fn release(&self, s: Hlc) {
        self.lock().drop_snapshot(s);
    }

    /// The version of `row` that a snapshot at `s` sees, as stored in a page, or `None` if no version is visible at `s`. The undo records that it needs must stay, so `s` must be a snapshot that cleanup respects. A deleted version stays, so that cleanup removes the row later.
    pub(crate) fn version_at(&self, mut row: Row, s: Hlc) -> Result<Option<Row>> {
        while row.header.owner().is_some() || Hlc::from_bits(row.header.stamp) > s {
            let record = self.undo.get(row.header.undo).ok_or_else(|| {
                Error::internal(format!(
                    "the undo record {} of the row {} is missing",
                    row.header.undo, row.id
                ))
            })?;
            match record.undo(row) {
                Some(older) => row = older,
                None => return Ok(None),
            }
        }
        Ok(Some(row))
    }

    fn latch(&self, table: &Table, id: RowId) -> MutexGuard<'_, ()> {
        let hash = (u64::from(table.def().oid().0) ^ id.bits()).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let at = (hash >> 58) as usize % STRIPES;
        self.stripes[at].lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a transaction.
    pub fn begin(self: &Arc<Self>, isolation: Isolation) -> Transaction<P> {
        let snapshot = self.lock().take_snapshot(&self.commits);
        Transaction {
            owner: self.clone(),
            isolation,
            xid: None,
            snapshot,
            cid: 0,
            writes: Vec::new(),
            blocks: BTreeMap::new(),
            done: false,
        }
    }

    /// Waits until the writer `owner` ends. If `owner` waits for `me`, directly or through other writers, the result is SQLSTATE `40P01`.
    fn wait(&self, me: Xid, owner: Xid) -> Result<()> {
        let mut state = self.lock();
        let mut at = owner;
        for _ in 0..state.writers.len() {
            match state.writers.get(&at) {
                Some(&Some(next)) if next == me => {
                    return Err(Error::new(SqlState::T_R_DEADLOCK_DETECTED, "deadlock detected"));
                }
                Some(&Some(next)) => at = next,
                _ => break,
            }
        }
        state.writers.insert(me, Some(owner));
        while state.writers.contains_key(&owner) {
            state = self.ended.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        state.writers.insert(me, None);
        Ok(())
    }

    /// Ends a transaction: drops its snapshot, removes it from the writers, keeps the undo of a commit, and wakes each writer that waits.
    fn end(&self, snapshot: Hlc, xid: Option<Xid>, kept: Option<(Hlc, Kept)>) {
        let mut state = self.lock();
        state.drop_snapshot(snapshot);
        if let Some(xid) = xid {
            state.writers.remove(&xid);
        }
        if let Some((ts, kept)) = kept {
            state.kept.insert(ts, kept);
        }
        drop(state);
        self.ended.notify_all();
    }

    /// Removes the undo of each commit that every snapshot sees, and the rows that those commits deleted. The result is the number of undo records removed.
    pub fn cleanup(&self) -> Result<usize> {
        let gone = {
            let mut state = self.lock();
            let visible = self.commits.visible();
            let horizon = state.snapshots.keys().next().map_or(visible, |&s| s.min(visible));
            let newer = state.kept.split_off(&Hlc::from_bits(horizon.bits().saturating_add(1)));
            std::mem::replace(&mut state.kept, newer)
        };
        let positions: Vec<u64> = gone.values().flat_map(|k| k.undo.iter().copied()).collect();
        self.undo.remove(&positions);
        for (ts, kept) in gone {
            for (table, id) in kept.deleted {
                let _latch = self.latch(&table, id);
                let _quiet = self.quiet();
                let Some(row) = table.hot().get(&*self.pages, id)? else { continue };
                if row.header.stamp == ts.bits() && row.header.flags & VersionHeader::DELETED != 0 {
                    table.hot().remove(&*self.pages, id)?;
                }
            }
        }
        Ok(positions.len())
    }
}

/// One change that a transaction made.
#[derive(Debug)]
struct Written {
    table: Arc<Table>,
    id: RowId,
    undo: u64,
    delete: bool,
}

/// What a reader sees of one row.
enum Seen {
    Version(Row),
    Nothing,
    /// A rollback removed an undo record that the reader needs. The row must be read again.
    Again,
}

/// A transaction. A transaction that is dropped before [`Transaction::commit`] rolls back.
pub struct Transaction<P: PageAccess> {
    owner: Arc<Transactions<P>>,
    isolation: Isolation,
    xid: Option<Xid>,
    snapshot: Hlc,
    cid: u32,
    writes: Vec<Written>,
    blocks: BTreeMap<Oid, RowIdBlock>,
    done: bool,
}

impl<P: PageAccess> fmt::Debug for Transaction<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("isolation", &self.isolation)
            .field("xid", &self.xid)
            .field("snapshot", &self.snapshot)
            .field("cid", &self.cid)
            .field("writes", &self.writes.len())
            .finish_non_exhaustive()
    }
}

impl<P: PageAccess> Transaction<P> {
    /// The isolation level.
    pub fn isolation(&self) -> Isolation {
        self.isolation
    }

    /// The transaction id, or `None` before the first write.
    pub fn xid(&self) -> Option<Xid> {
        self.xid
    }

    /// The snapshot of the current command.
    pub fn snapshot(&self) -> Hlc {
        self.snapshot
    }

    /// The command number.
    pub fn command(&self) -> u32 {
        self.cid
    }

    /// Starts the next command. The next command sees the changes of the commands before it. At `READ COMMITTED` it also takes a new snapshot. After 2^32 - 2 commands the result is SQLSTATE `54000`, as in PostgreSQL.
    pub fn next_command(&mut self) -> Result<()> {
        if self.cid >= u32::MAX - 1 {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "cannot have more than 2^32-2 commands in a transaction",
            ));
        }
        self.cid += 1;
        if self.isolation == Isolation::ReadCommitted {
            let owner = &self.owner;
            let mut state = owner.lock();
            state.drop_snapshot(self.snapshot);
            self.snapshot = state.take_snapshot(&owner.commits);
        }
        Ok(())
    }

    /// The version of `row` that the transaction sees.
    fn see(&self, mut row: Row) -> Result<Seen> {
        loop {
            let owner = row.header.owner();
            if owner.is_none() && Hlc::from_bits(row.header.stamp) <= self.snapshot {
                break;
            }
            let Some(record) = self.owner.undo.get(row.header.undo) else {
                if owner.is_some() {
                    return Ok(Seen::Again);
                }
                return Err(Error::internal(format!(
                    "the undo record {} of the row {} is missing",
                    row.header.undo, row.id
                )));
            };
            if owner.is_some() && owner == self.xid.map(Xid::bits) && record.cid < self.cid {
                break;
            }
            match record.undo(row) {
                Some(older) => row = older,
                None => return Ok(Seen::Nothing),
            }
        }
        if row.header.flags & VersionHeader::DELETED != 0 {
            return Ok(Seen::Nothing);
        }
        Ok(Seen::Version(row))
    }

    /// The version of the row `id` in `table` that the transaction sees, starting from `row`, the copy that the reader has.
    fn see_row(&self, table: &Table, id: RowId, mut row: Option<Row>) -> Result<Option<Row>> {
        for _ in 0..RETRIES {
            let Some(r) = row else { return Ok(None) };
            match self.see(r)? {
                Seen::Version(r) => return Ok(Some(r)),
                Seen::Nothing => return Ok(None),
                Seen::Again => row = table.hot().get(&*self.owner.pages, id)?,
            }
        }
        Err(Error::internal(format!("the row {id} changed {RETRIES} times during one read")))
    }

    /// The values of the row `id` in `table` that the transaction sees, or `None`.
    pub fn get(&self, table: &Table, id: RowId) -> Result<Option<Vec<Datum>>> {
        let row = table.hot().get(&*self.owner.pages, id)?;
        self.see_row(table, id, row)?.map(|r| table.def().decode(&r.values)).transpose()
    }

    /// Calls `f` with the id and the values of each row of `table` that the transaction sees, in row id order, until `f` gives false.
    pub fn scan(
        &self,
        table: &Table,
        f: impl FnMut(RowId, Vec<Datum>) -> Result<bool>,
    ) -> Result<()> {
        self.scan_from(table, RowId::from_bits(0), f)
    }

    /// As [`Transaction::scan`], from the first row with an id at or above `from`.
    pub fn scan_from(
        &self,
        table: &Table,
        from: RowId,
        mut f: impl FnMut(RowId, Vec<Datum>) -> Result<bool>,
    ) -> Result<()> {
        let pages = &*self.owner.pages;
        table.hot().scan(pages, from, |row| {
            let id = row.id;
            match self.see_row(table, id, Some(row))? {
                Some(r) => f(id, table.def().decode(&r.values)?),
                None => Ok(true),
            }
        })
    }

    /// The transaction id. The first write takes one.
    fn writer(&mut self) -> Xid {
        if let Some(xid) = self.xid {
            return xid;
        }
        let mut state = self.owner.lock();
        let xid = state.next_xid;
        state.next_xid = xid.next();
        state.writers.insert(xid, None);
        drop(state);
        self.xid = Some(xid);
        xid
    }

    fn row_id(&mut self, table: &Table) -> Result<RowId> {
        let oid = table.def().oid();
        if let Some(id) = self.blocks.get_mut(&oid).and_then(Iterator::next) {
            return Ok(id);
        }
        let mut block = table.take_ids()?;
        let id = block.next().ok_or_else(|| Error::internal("a new row id block is empty"))?;
        self.blocks.insert(oid, block);
        Ok(id)
    }

    /// Adds a row with `values` to `table` and gives its row id.
    pub fn insert(&mut self, table: &Arc<Table>, values: &[Datum]) -> Result<RowId> {
        let stored = table.def().encode(values)?;
        let xid = self.writer();
        let id = self.row_id(table)?;
        let mut row = Row {
            id,
            header: VersionHeader::owned_by(xid.bits(), 0, 0),
            xmin: xid.xid32(),
            xmax: 0,
            values: stored,
        };
        let undo = self.owner.undo.add(Record::of(xid, self.cid, &row, Change::Insert))?;
        row.header.undo = undo;
        let quiet = self.owner.quiet();
        let inserted = table.hot().insert(&*self.owner.pages, table.def().schema(), &row);
        drop(quiet);
        if let Err(e) = inserted {
            self.owner.undo.remove(&[undo]);
            return Err(e);
        }
        self.writes.push(Written { table: table.clone(), id, undo, delete: false });
        Ok(id)
    }

    /// Writes `values` in the row `id` of `table`. The result is false if the row is deleted, or if this command changed it already.
    ///
    /// The change goes to the newest version of the row. A row that another writer changed and did not commit makes this transaction wait for that writer. At `REPEATABLE READ`, a row with a committed version newer than the snapshot gives SQLSTATE `40001`.
    pub fn update(&mut self, table: &Arc<Table>, id: RowId, values: &[Datum]) -> Result<bool> {
        let stored = table.def().encode(values)?;
        self.change(table, id, Some(stored))
    }

    /// Deletes the row `id` of `table`. The result is false if the row is deleted, or if this command changed it already. The waits and errors are those of [`Transaction::update`].
    pub fn delete(&mut self, table: &Arc<Table>, id: RowId) -> Result<bool> {
        self.change(table, id, None)
    }

    fn change(
        &mut self,
        table: &Arc<Table>,
        id: RowId,
        values: Option<Vec<Option<Vec<u8>>>>,
    ) -> Result<bool> {
        let xid = self.writer();
        let owner = self.owner.clone();
        let pages = &*owner.pages;
        loop {
            let latch = owner.latch(table, id);
            let quiet = owner.quiet();
            let Some(row) = table.hot().get(pages, id)? else { return Ok(false) };
            match row.header.owner() {
                Some(o) if o == xid.bits() => {
                    let record = owner.undo.get(row.header.undo).ok_or_else(|| {
                        Error::internal(format!("the undo record of the row {id} is missing"))
                    })?;
                    if record.cid >= self.cid {
                        return Ok(false);
                    }
                }
                Some(o) => {
                    drop(quiet);
                    drop(latch);
                    let other = Xid::from_bits(o).ok_or_else(|| {
                        Error::corrupted(format!("the row {id} has the owner {o}"))
                    })?;
                    owner.wait(xid, other)?;
                    continue;
                }
                None => {
                    if self.isolation == Isolation::RepeatableRead
                        && Hlc::from_bits(row.header.stamp) > self.snapshot
                    {
                        let what = match row.header.flags & VersionHeader::DELETED {
                            0 => "update",
                            _ => "delete",
                        };
                        return Err(Error::new(
                            SqlState::T_R_SERIALIZATION_FAILURE,
                            format!("could not serialize access due to concurrent {what}"),
                        ));
                    }
                }
            }
            if row.header.flags & VersionHeader::DELETED != 0 {
                return Ok(false);
            }
            let flags =
                row.header.flags & !(VersionHeader::LOCK_ONLY | VersionHeader::LOCK_STRENGTH);
            let (change, new) = match values {
                Some(values) => {
                    let old = row
                        .values
                        .iter()
                        .zip(&values)
                        .enumerate()
                        .filter(|(_, (a, b))| a != b)
                        .map(|(i, (a, _))| (i, a.clone()))
                        .collect();
                    let new = Row { values, xmin: xid.xid32(), xmax: 0, ..row.clone() };
                    (Change::Update(old), new)
                }
                None => {
                    let new = Row { xmax: xid.xid32(), ..row.clone() };
                    (Change::Delete, new)
                }
            };
            let delete = change == Change::Delete;
            let undo = owner.undo.add(Record::of(xid, self.cid, &row, change))?;
            let flags = if delete { flags | VersionHeader::DELETED } else { flags };
            let new = Row { header: VersionHeader::owned_by(xid.bits(), undo, flags), ..new };
            if let Err(e) = table.hot().replace(pages, table.def().schema(), &new) {
                owner.undo.remove(&[undo]);
                return Err(e);
            }
            self.writes.push(Written { table: table.clone(), id, undo, delete });
            return Ok(true);
        }
    }

    /// Commits. The result is the commit timestamp, or `None` for a transaction that changed no row.
    ///
    /// With a log, the commit places one commit block in the log and returns when the block is safe. If the log cannot make the block safe, the result is the error of the log and the commit is not visible. The ring stops, so no later commit is visible either, and the node must restart. Recovery keeps the commit only if its block is in a safe prefix.
    pub fn commit(mut self) -> Result<Option<Hlc>> {
        self.done = true;
        let xid = match self.xid {
            Some(xid) if !self.writes.is_empty() => xid,
            _ => {
                self.owner.end(self.snapshot, self.xid, None);
                return Ok(None);
            }
        };
        let owner = self.owner.clone();
        let body = owner.log.as_ref().map(|_| self.body()).transpose();
        let quiet = owner.quiet();
        let gate = owner.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let (ts, body) = match body.and_then(|body| Ok((owner.commits.begin()?, body))) {
            Ok(begun) => begun,
            Err(e) => {
                drop(gate);
                drop(quiet);
                return self.fail(xid, None, e);
            }
        };
        let placed = self.stamp(ts).and_then(|()| match (&owner.log, body) {
            (Some(log), Some(body)) => {
                let mut block = Block {
                    kind: BlockKind::Commit,
                    ring: RING,
                    position: 0,
                    commit_ts: ts,
                    xid: xid.bits(),
                    deps: Vec::new(),
                    body,
                };
                let end = log.append(RING, &mut block)?.end;
                owner.placed.lock().unwrap_or_else(PoisonError::into_inner).push_back((ts, end));
                Ok(Some(end))
            }
            _ => Ok(None),
        });
        drop(gate);
        drop(quiet);
        let placed = match placed {
            Ok(placed) => placed,
            Err(e) => return self.fail(xid, Some(ts), e),
        };
        let kept = self.kept();
        if let (Some(log), Some(end)) = (&owner.log, placed)
            && let Err(e) = log.wait_safe(RING, end)
        {
            // The block can still be durable, so the rows keep their stamps and the undo stays for the snapshots below `ts`.
            owner.end(self.snapshot, Some(xid), Some((ts, kept)));
            return Err(e);
        }
        owner.commits.installed(ts)?;
        owner.end(self.snapshot, Some(xid), Some((ts, kept)));
        owner.cleanup()?;
        Ok(Some(ts))
    }

    /// Rolls back a commit that failed before its block was placed, gives back its timestamp, and gives the error.
    fn fail(&mut self, xid: Xid, ts: Option<Hlc>, e: Error) -> Result<Option<Hlc>> {
        let undone = self.undo_all();
        let abandoned = ts.map_or(Ok(()), |ts| self.owner.commits.abandon(ts));
        self.owner.end(self.snapshot, Some(xid), None);
        undone?;
        abandoned?;
        Err(e)
    }

    /// The body of the commit block.
    fn body(&self) -> Result<Vec<u8>> {
        let mut changes = Changes::default();
        for w in &self.writes {
            let record = self.owner.undo.get(w.undo).ok_or_else(|| {
                Error::internal(format!("the undo record of the row {} is missing", w.id))
            })?;
            changes.add(&w.table, w.id, &record.change);
        }
        changes.body(&*self.owner.pages)
    }

    /// The undo records of the commit and the rows that it deleted, which cleanup removes when every snapshot sees the commit.
    fn kept(&self) -> Kept {
        let mut last: BTreeMap<(Oid, RowId), (&Arc<Table>, bool)> = BTreeMap::new();
        for w in &self.writes {
            last.insert((w.table.def().oid(), w.id), (&w.table, w.delete));
        }
        let deleted = last
            .into_iter()
            .filter(|(_, (_, delete))| *delete)
            .map(|((_, id), (table, _))| (table.clone(), id))
            .collect();
        Kept { undo: self.writes.iter().map(|w| w.undo).collect(), deleted }
    }

    /// Writes `ts` into the header of each row that the transaction changed.
    fn stamp(&self, ts: Hlc) -> Result<()> {
        let pages = &*self.owner.pages;
        let mut done = BTreeSet::new();
        for w in &self.writes {
            if !done.insert((w.table.def().oid(), w.id)) {
                continue;
            }
            let row = w.table.hot().get(pages, w.id)?.ok_or_else(|| {
                Error::internal(format!("the row {} that a writer changed is missing", w.id))
            })?;
            let header = VersionHeader { stamp: ts.bits(), ..row.header };
            w.table.hot().set_header(pages, w.id, header)?;
        }
        Ok(())
    }

    /// Rolls back.
    pub fn rollback(mut self) -> Result<()> {
        self.done = true;
        let undone = self.undo_all();
        self.owner.end(self.snapshot, self.xid, None);
        undone
    }

    /// Applies the undo of each change in reverse order and removes the undo records.
    fn undo_all(&mut self) -> Result<()> {
        let pages = &*self.owner.pages;
        let writes = std::mem::take(&mut self.writes);
        let _quiet = self.owner.quiet();
        for w in writes.iter().rev() {
            let record = self.owner.undo.get(w.undo).ok_or_else(|| {
                Error::internal(format!("the undo record of the row {} is missing", w.id))
            })?;
            let row = w.table.hot().get(pages, w.id)?.ok_or_else(|| {
                Error::internal(format!("the row {} that a writer changed is missing", w.id))
            })?;
            match record.undo(row) {
                Some(older) => {
                    w.table.hot().replace(pages, w.table.def().schema(), &older)?;
                }
                None => {
                    w.table.hot().remove(pages, w.id)?;
                }
            }
        }
        let positions: Vec<u64> = writes.iter().map(|w| w.undo).collect();
        self.owner.undo.remove(&positions);
        Ok(())
    }
}

impl<P: PageAccess> Drop for Transaction<P> {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            // A rollback that fails here leaves rows that name this writer. The next check of the file reports them.
            let _ = self.undo_all();
            self.owner.end(self.snapshot, self.xid, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rupg_common::ShardId;
    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::{SimClock, SimRng};
    use rupg_platform::{TaskHandle, Tasks};
    use rupg_table::{ColumnDef, TableDef};
    use rupg_tree::MemPages;
    use rupg_types::TypeId;

    use super::*;

    type Txns = Arc<Transactions<MemPages>>;
    type Txn = Transaction<MemPages>;

    fn setup() -> (Txns, Arc<Table>) {
        let pages = Arc::new(MemPages::new(20_000));
        let wall = Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000));
        let clock = HlcClock::new(wall, Hlc::ZERO);
        let txns = Arc::new(Transactions::new(pages.clone(), clock, Xid::FIRST));
        let col =
            |name: &str, nullable| ColumnDef { name: name.into(), ty: TypeId::INT8, nullable };
        let def = TableDef::new(Oid(16384), "t", vec![col("k", false), col("v", true)]).unwrap();
        let table = Arc::new(Table::create(&*pages, def, ShardId(0)).unwrap());
        (txns, table)
    }

    fn row(k: i64, v: i64) -> Vec<Datum> {
        vec![Datum::Int8(k), Datum::Int8(v)]
    }

    fn read(t: &Txn, table: &Table, id: RowId) -> Option<i64> {
        t.get(table, id).unwrap().map(|values| match values[1] {
            Datum::Int8(v) => v,
            ref other => panic!("{other:?}"),
        })
    }

    fn sum(t: &Txn, table: &Table) -> i64 {
        let mut total = 0;
        t.scan(table, |_, values| {
            let Datum::Int8(v) = values[1] else { panic!("{values:?}") };
            total += v;
            Ok(true)
        })
        .unwrap();
        total
    }

    fn stored(txns: &Txns, table: &Table) -> usize {
        let mut n = 0;
        let count = |_| {
            n += 1;
            Ok(true)
        };
        table.hot().scan(&**txns.pages(), RowId::from_bits(0), count).unwrap();
        n
    }

    #[test]
    fn snapshots() {
        let (txns, table) = setup();
        let mut a = txns.begin(Isolation::ReadCommitted);
        let id = a.insert(&table, &row(1, 10)).unwrap();
        assert_eq!(read(&a, &table, id), None, "a command does not see its own insert");
        a.next_command().unwrap();
        assert_eq!(read(&a, &table, id), Some(10));
        let b = txns.begin(Isolation::RepeatableRead);
        assert_eq!(read(&b, &table, id), None);
        let ts = a.commit().unwrap().unwrap();
        assert_eq!((txns.commits().visible(), read(&b, &table, id)), (ts, None));
        assert_eq!(read(&txns.begin(Isolation::ReadCommitted), &table, id), Some(10));

        let mut c = txns.begin(Isolation::ReadCommitted);
        let mut d = txns.begin(Isolation::RepeatableRead);
        let mut e = txns.begin(Isolation::ReadCommitted);
        assert!(e.update(&table, id, &row(1, 20)).unwrap());
        assert!(!e.update(&table, id, &row(1, 21)).unwrap(), "one command changes a row once");
        assert_eq!((read(&d, &table, id), read(&e, &table, id)), (Some(10), Some(10)));
        e.next_command().unwrap();
        assert_eq!(read(&e, &table, id), Some(20));
        assert!(e.update(&table, id, &row(1, 30)).unwrap());
        e.next_command().unwrap();
        assert_eq!(read(&e, &table, id), Some(30));
        e.commit().unwrap();
        assert_eq!((read(&c, &table, id), read(&d, &table, id)), (Some(10), Some(10)));
        c.next_command().unwrap();
        assert_eq!(
            read(&c, &table, id),
            Some(30),
            "read committed takes a snapshot for each command"
        );
        c.commit().unwrap();
        assert_eq!(read(&txns.begin(Isolation::ReadCommitted), &table, id), Some(30));
        let err = d.update(&table, id, &row(1, 40)).unwrap_err();
        assert_eq!(err.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        assert_eq!(err.message(), "could not serialize access due to concurrent update");
        assert_eq!(read(&d, &table, id), Some(10));

        let mut g = txns.begin(Isolation::ReadCommitted);
        assert!(g.delete(&table, id).unwrap());
        assert!(!g.delete(&table, id).unwrap());
        g.next_command().unwrap();
        assert!(!g.update(&table, id, &row(1, 50)).unwrap(), "the row is deleted");
        assert_eq!(read(&g, &table, id), None);
        g.commit().unwrap();
        assert_eq!(read(&d, &table, id), Some(10));
        let err = d.delete(&table, id).unwrap_err();
        assert_eq!(err.message(), "could not serialize access due to concurrent delete");
        assert_eq!(read(&txns.begin(Isolation::ReadCommitted), &table, id), None);
        assert_eq!(stored(&txns, &table), 1);
        assert!(txns.undo_records() > 0);
        drop(d);
        drop(b);
        txns.cleanup().unwrap();
        assert_eq!((txns.undo_records(), stored(&txns, &table)), (0, 0));
        assert_eq!(txns.horizon(), txns.commits().visible());
    }

    #[test]
    fn rollback() {
        let (txns, table) = setup();
        let mut a = txns.begin(Isolation::ReadCommitted);
        let ids: Vec<RowId> = (0..5).map(|k| a.insert(&table, &row(k, k * 10)).unwrap()).collect();
        a.commit().unwrap();
        let old = txns.begin(Isolation::RepeatableRead);
        let mut b = txns.begin(Isolation::ReadCommitted);
        b.update(&table, ids[0], &row(0, 1)).unwrap();
        b.next_command().unwrap();
        b.update(&table, ids[0], &row(0, 2)).unwrap();
        b.delete(&table, ids[1]).unwrap();
        let new = b.insert(&table, &row(9, 90)).unwrap();
        b.next_command().unwrap();
        assert_eq!(sum(&b, &table), 2 + 20 + 30 + 40 + 90);
        assert_eq!(sum(&old, &table), 100);
        b.rollback().unwrap();
        assert_eq!(sum(&txns.begin(Isolation::ReadCommitted), &table), 100);
        assert_eq!(read(&old, &table, new), None);
        assert_eq!(stored(&txns, &table), 5);

        // A dropped transaction rolls back.
        let mut c = txns.begin(Isolation::RepeatableRead);
        c.update(&table, ids[2], &row(2, 0)).unwrap();
        c.insert(&table, &row(10, 0)).unwrap();
        drop(c);
        let mut d = txns.begin(Isolation::RepeatableRead);
        assert!(d.update(&table, ids[2], &row(2, 25)).unwrap(), "the row has no owner");
        d.commit().unwrap();
        assert_eq!(sum(&old, &table), 100);
        drop(old);
        txns.cleanup().unwrap();
        assert_eq!(sum(&txns.begin(Isolation::ReadCommitted), &table), 105);
        assert_eq!((txns.undo_records(), stored(&txns, &table)), (0, 5));
    }

    #[test]
    fn writers_wait() {
        for (isolation, commit, want) in [
            (Isolation::ReadCommitted, true, Ok(true)),
            (Isolation::ReadCommitted, false, Ok(true)),
            (Isolation::RepeatableRead, true, Err(SqlState::T_R_SERIALIZATION_FAILURE)),
            (Isolation::RepeatableRead, false, Ok(true)),
        ] {
            let (txns, table) = setup();
            let mut a = txns.begin(Isolation::ReadCommitted);
            let id = a.insert(&table, &row(1, 1)).unwrap();
            a.commit().unwrap();
            let mut a = txns.begin(Isolation::ReadCommitted);
            a.update(&table, id, &row(1, 2)).unwrap();
            let mut b = txns.begin(isolation);
            let result = Arc::new(Mutex::new(None));
            let task = {
                let (table, result) = (table.clone(), result.clone());
                Box::new(move || {
                    let r = b.update(&table, id, &row(1, 3));
                    let r = r.and_then(|ok| b.commit().map(|_| ok)).map_err(|e| e.state());
                    *result.lock().unwrap() = Some(r);
                })
            };
            let handle = OsTasks.spawn("writer b", task).unwrap();
            if commit {
                a.commit().unwrap();
            } else {
                a.rollback().unwrap();
            }
            handle.join().unwrap();
            assert_eq!(result.lock().unwrap().take(), Some(want));
            let last = if want.is_ok() { 3 } else { 2 };
            assert_eq!(read(&txns.begin(Isolation::ReadCommitted), &table, id), Some(last));
            txns.cleanup().unwrap();
            assert_eq!(txns.undo_records(), 0);
        }
    }

    #[test]
    fn deadlocks() {
        let (txns, table) = setup();
        let mut t = txns.begin(Isolation::ReadCommitted);
        let x = t.insert(&table, &row(1, 1)).unwrap();
        let y = t.insert(&table, &row(2, 2)).unwrap();
        t.commit().unwrap();
        let mut a = txns.begin(Isolation::ReadCommitted);
        let mut b = txns.begin(Isolation::ReadCommitted);
        a.update(&table, x, &row(1, 10)).unwrap();
        b.update(&table, y, &row(2, 20)).unwrap();
        let result = Arc::new(Mutex::new(None));
        let task = {
            let (table, result) = (table.clone(), result.clone());
            Box::new(move || {
                let r = b.update(&table, x, &row(1, 30));
                if r.is_ok() {
                    b.commit().unwrap();
                }
                *result.lock().unwrap() = Some(r.map_err(|e| e.state()));
            })
        };
        let handle = OsTasks.spawn("writer b", task).unwrap();
        let mine = a.update(&table, y, &row(2, 40)).map_err(|e| e.state());
        if mine.is_ok() {
            a.commit().unwrap();
        } else {
            drop(a);
        }
        handle.join().unwrap();
        let theirs = result.lock().unwrap().take().unwrap();
        let deadlock = Err(SqlState::T_R_DEADLOCK_DETECTED);
        assert!(
            (mine == deadlock && theirs == Ok(true)) || (mine == Ok(true) && theirs == deadlock),
            "{mine:?} {theirs:?}"
        );
    }

    /// One transaction of the model: its snapshot rule and its own changes by command.
    struct Open {
        txn: Txn,
        own: BTreeMap<RowId, Vec<(u32, Option<i64>)>>,
    }

    /// Random transactions in one thread, checked against a model of snapshot isolation. A transaction never changes a row that another open transaction changed, because that waits.
    #[test]
    fn a_model_of_random_transactions() {
        for seed in 0..12 {
            let (txns, table) = setup();
            let mut rng = SimRng::new(seed);
            // The committed versions of each row, oldest first.
            let mut history: BTreeMap<RowId, Vec<(Hlc, Option<i64>)>> = BTreeMap::new();
            let mut open: Vec<Open> = Vec::new();
            let committed = |history: &BTreeMap<RowId, Vec<(Hlc, Option<i64>)>>, id, s| {
                history.get(&id).and_then(|v: &Vec<(Hlc, Option<i64>)>| {
                    v.iter().rev().find(|(ts, _)| *ts <= s).and_then(|(_, value)| *value)
                })
            };
            for step in 0..3_000 {
                let op = rng.below(12);
                if open.is_empty() || (op == 0 && open.len() < 4) {
                    let isolation = match rng.below(2) {
                        0 => Isolation::ReadCommitted,
                        _ => Isolation::RepeatableRead,
                    };
                    open.push(Open { txn: txns.begin(isolation), own: BTreeMap::new() });
                    continue;
                }
                let i = rng.below(open.len() as u64) as usize;
                let ids: Vec<RowId> = history.keys().copied().collect();
                let pick = |rng: &mut SimRng| {
                    ids.get(rng.below(ids.len().max(1) as u64) as usize).copied()
                };
                let o = &open[i];
                let cid = o.txn.command();
                let mine = |o: &Open, id: RowId| -> Option<Option<i64>> {
                    o.own
                        .get(&id)
                        .and_then(|v| v.iter().rev().find(|(c, _)| *c < cid))
                        .map(|(_, v)| *v)
                };
                let sees = |o: &Open, id| match mine(o, id) {
                    Some(value) => value,
                    None => committed(&history, id, o.txn.snapshot()),
                };
                match op {
                    1 => {
                        let o = open.swap_remove(i);
                        let ts = o.txn.commit().unwrap();
                        assert_eq!(ts.is_some(), !o.own.is_empty(), "seed {seed} step {step}");
                        if let Some(ts) = ts {
                            for (id, changes) in o.own {
                                let last = changes.last().unwrap().1;
                                history.entry(id).or_default().push((ts, last));
                            }
                        }
                    }
                    2 => {
                        open.swap_remove(i).txn.rollback().unwrap();
                    }
                    3 => open[i].txn.next_command().unwrap(),
                    4 | 5 => {
                        let Some(id) = pick(&mut rng) else { continue };
                        assert_eq!(
                            read(&o.txn, &table, id),
                            sees(o, id),
                            "seed {seed} step {step}"
                        );
                    }
                    6 => {
                        let mut want: BTreeMap<RowId, i64> = BTreeMap::new();
                        for &id in history.keys().chain(o.own.keys()) {
                            if let Some(v) = sees(o, id) {
                                want.insert(id, v);
                            }
                        }
                        let mut got = BTreeMap::new();
                        o.txn
                            .scan(&table, |id, values| {
                                let Datum::Int8(v) = values[1] else { panic!() };
                                got.insert(id, v);
                                Ok(true)
                            })
                            .unwrap();
                        assert_eq!(got, want, "seed {seed} step {step}");
                    }
                    7 => {
                        let v = rng.below(1_000) as i64;
                        let id = open[i].txn.insert(&table, &row(v, v)).unwrap();
                        open[i].own.entry(id).or_default().push((cid, Some(v)));
                    }
                    _ => {
                        let Some(id) = pick(&mut rng) else { continue };
                        if open.iter().enumerate().any(|(j, p)| j != i && p.own.contains_key(&id)) {
                            continue;
                        }
                        let delete = op == 8;
                        let v = rng.below(1_000) as i64;
                        let newest_ts = history[&id].last().unwrap().0;
                        let newest = match o.own.get(&id).and_then(|v| v.last()) {
                            Some(&(c, _)) if c == cid => Err(()),
                            Some(&(_, value)) => Ok(value),
                            None => Ok(history[&id].last().unwrap().1),
                        };
                        let conflict = o.txn.isolation() == Isolation::RepeatableRead
                            && !o.own.contains_key(&id)
                            && newest_ts > o.txn.snapshot();
                        let o = &mut open[i];
                        let got = match delete {
                            true => o.txn.delete(&table, id),
                            false => o.txn.update(&table, id, &row(v, v)),
                        };
                        if conflict {
                            assert_eq!(
                                got.unwrap_err().state(),
                                SqlState::T_R_SERIALIZATION_FAILURE
                            );
                            open.swap_remove(i).txn.rollback().unwrap();
                            continue;
                        }
                        let changed = matches!(newest, Ok(Some(_)));
                        assert_eq!(got.unwrap(), changed, "seed {seed} step {step}");
                        if changed {
                            o.own.entry(id).or_default().push((cid, (!delete).then_some(v)));
                        }
                    }
                }
            }
            for o in open {
                o.txn.rollback().unwrap();
            }
            txns.cleanup().unwrap();
            let live = history.values().filter(|v| v.last().unwrap().1.is_some()).count();
            assert_eq!((txns.undo_records(), stored(&txns, &table)), (0, live), "seed {seed}");
        }
    }

    /// Threads move money between accounts at `REPEATABLE READ` and retry on `40001` and `40P01`. Each snapshot sees the same total.
    #[test]
    fn transfers_keep_the_total() {
        const ACCOUNTS: i64 = 20;
        const THREADS: u64 = 6;
        const TRANSFERS: usize = 300;
        let (txns, table) = setup();
        let mut t = txns.begin(Isolation::ReadCommitted);
        let ids: Arc<Vec<RowId>> =
            Arc::new((0..ACCOUNTS).map(|k| t.insert(&table, &row(k, 100)).unwrap()).collect());
        t.commit().unwrap();
        let handles: Vec<TaskHandle> = (0..THREADS)
            .map(|n| {
                let (txns, table, ids) = (txns.clone(), table.clone(), ids.clone());
                let task = Box::new(move || {
                    let mut rng = SimRng::new(n);
                    let mut done = 0;
                    while done < TRANSFERS {
                        let from = ids[rng.below(ids.len() as u64) as usize];
                        let to = ids[rng.below(ids.len() as u64) as usize];
                        let amount = rng.below(10) as i64;
                        let mut t = txns.begin(Isolation::RepeatableRead);
                        let step = (|| {
                            let a = read(&t, &table, from).unwrap();
                            t.update(&table, from, &row(0, a - amount))?;
                            t.next_command()?;
                            let b = read(&t, &table, to).unwrap();
                            t.update(&table, to, &row(0, b + amount))?;
                            Ok::<(), Error>(())
                        })();
                        match step {
                            Ok(()) => {
                                t.commit().unwrap();
                                done += 1;
                            }
                            Err(e) => {
                                let retry = [
                                    SqlState::T_R_SERIALIZATION_FAILURE,
                                    SqlState::T_R_DEADLOCK_DETECTED,
                                ];
                                assert!(retry.contains(&e.state()), "{e}");
                                t.rollback().unwrap();
                            }
                        }
                    }
                });
                OsTasks.spawn(&format!("transfer {n}"), task).unwrap()
            })
            .collect();
        for _ in 0..300 {
            let isolation = if sum(&txns.begin(Isolation::ReadCommitted), &table) % 2 == 0 {
                Isolation::RepeatableRead
            } else {
                Isolation::ReadCommitted
            };
            assert_eq!(sum(&txns.begin(isolation), &table), ACCOUNTS * 100);
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(sum(&txns.begin(Isolation::ReadCommitted), &table), ACCOUNTS * 100);
        txns.cleanup().unwrap();
        assert_eq!((txns.undo_records(), stored(&txns, &table)), (0, ACCOUNTS as usize));
    }
}

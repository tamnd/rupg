//! The log records of a commit and their replay in recovery (spec/11 sections 11.13.3 and 11.16).
//!
//! A commit writes one commit block with one record for each row that it changed. An insert holds every column, an update holds the columns that the transaction changed, and a delete holds no column. A row that the transaction added and then deleted has no record.
//!
//! Replay applies the records of the safe blocks in ring order. A record applies only if its commit timestamp is above the `stamp` of the row, so a block that is already in the pages changes nothing. An update or a delete of a row that is not there changes nothing too, because a delete removes the row and its stamp, and row ids are not used again. Replay starts from the pages of the last checkpoint. They hold the rows of each commit at or below the snapshot of the checkpoint and no other version (see `checkpoint.rs`), and the blocks after the redo position are all above that snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rupg_buffer::FileStore;
use rupg_common::{Error, Hlc, Oid, Result, RowId, Xid};
use rupg_file::PageAccess;
use rupg_hot::{Row, VersionHeader};
use rupg_log::{Block, BlockKind, Record, RecordKind, RecordReader, RecordWriter, Recovery};
use rupg_table::Table;

use crate::undo::Change;

/// What a transaction did to one row.
#[derive(Debug)]
struct Changed<'a> {
    table: &'a Table,
    insert: bool,
    columns: BTreeSet<usize>,
    delete: bool,
}

/// The rows that a transaction changed, for its commit block.
#[derive(Debug, Default)]
pub(crate) struct Changes<'a> {
    rows: BTreeMap<(Oid, RowId), Changed<'a>>,
}

impl<'a> Changes<'a> {
    /// Adds one change to the row `id` of `table`.
    pub(crate) fn add(&mut self, table: &'a Table, id: RowId, change: &Change) {
        let row = self.rows.entry((table.def().oid(), id)).or_insert_with(|| Changed {
            table,
            insert: false,
            columns: BTreeSet::new(),
            delete: false,
        });
        match change {
            Change::Insert => row.insert = true,
            Change::Update(old) => row.columns.extend(old.iter().map(|(column, _)| *column)),
            Change::Delete => row.delete = true,
        }
    }

    /// The body of the commit block, with the values that the rows in `pages` have now. The records are in the order of table and row id, so the deltas are small.
    pub(crate) fn body<P: PageAccess>(self, pages: &P) -> Result<Vec<u8>> {
        let mut out = RecordWriter::new();
        for ((oid, id), r) in self.rows {
            let kind = match (r.insert, r.delete) {
                (true, true) => continue,
                (false, true) => RecordKind::Delete,
                (true, false) => RecordKind::Insert,
                (false, false) if r.columns.is_empty() => continue,
                (false, false) => RecordKind::Update,
            };
            let mut values = Vec::new();
            if kind != RecordKind::Delete {
                let row = r.table.hot().get(pages, id)?.ok_or_else(|| {
                    Error::internal(format!("the row {id} that a writer changed is missing"))
                })?;
                let columns: Vec<usize> = match kind {
                    RecordKind::Insert => (0..row.values.len()).collect(),
                    _ => r.columns.into_iter().collect(),
                };
                for c in columns {
                    let column = u16::try_from(c).map_err(|_| {
                        Error::internal(format!(
                            "the column {c} of table {oid} is past the log limit"
                        ))
                    })?;
                    values.push((column, row.values[c].clone()));
                }
            }
            out.push(&Record { kind, table: oid, row: id, values })?;
        }
        Ok(out.take())
    }
}

/// The counts of a replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// The commit blocks applied.
    pub commits: u64,
    /// The records applied.
    pub applied: u64,
    /// The records that the pages had already.
    pub skipped: u64,
    /// The newest commit timestamp in the log and the checkpoint. The HLC of the node must start above it.
    pub newest: Hlc,
    /// The transaction id after the highest one in the applied blocks, or `None` if no block was applied.
    pub next_xid: Option<Xid>,
}

/// Applies each safe block of `recovery` to the tables in `pages`. `tables` gives the table of an OID. A record of a table that `tables` does not give, or a record that does not match its row, gives SQLSTATE `XX001`.
pub fn replay<P: PageAccess>(
    pages: &P,
    store: &FileStore,
    recovery: &Recovery,
    tables: &dyn Fn(Oid) -> Option<Arc<Table>>,
) -> Result<Replayed> {
    let mut done = Replayed { newest: recovery.newest(), ..Replayed::default() };
    recovery.for_each_safe(store, |_, block| apply(pages, &block, tables, &mut done))?;
    Ok(done)
}

/// Applies one block.
fn apply<P: PageAccess>(
    pages: &P,
    block: &Block,
    tables: &dyn Fn(Oid) -> Option<Arc<Table>>,
    done: &mut Replayed,
) -> Result<()> {
    if block.kind != BlockKind::Commit {
        return Err(Error::corrupted(format!(
            "the log holds a {} block at position {} of ring {}, and this version writes only commit blocks",
            block.kind, block.position, block.ring
        )));
    }
    let xid = Xid::from_bits(block.xid).ok_or_else(|| {
        Error::corrupted(format!(
            "the commit block at position {} has the xid {}",
            block.position, block.xid
        ))
    })?;
    let ts = block.commit_ts;
    for record in RecordReader::new(&block.body) {
        let record = record?;
        let table = tables(record.table).ok_or_else(|| {
            Error::corrupted(format!(
                "a log record of transaction {xid} names the table {}, which does not exist",
                record.table
            ))
        })?;
        if row_record(pages, &table, &record, xid, ts)? {
            done.applied += 1;
        } else {
            done.skipped += 1;
        }
    }
    done.commits += 1;
    done.next_xid = done.next_xid.max(Some(xid.next()));
    Ok(())
}

/// Applies one record of the commit of `xid` at `ts`. The result is false if the row has the change already.
fn row_record<P: PageAccess>(
    pages: &P,
    table: &Table,
    record: &Record,
    xid: Xid,
    ts: Hlc,
) -> Result<bool> {
    let id = record.row;
    let hot = table.hot();
    let schema = table.def().schema();
    let old = hot.get(pages, id)?;
    if let Some(row) = &old {
        if row.header.owner().is_some() {
            return Err(Error::corrupted(format!("the row {id} has an owner during recovery")));
        }
        if Hlc::from_bits(row.header.stamp) >= ts {
            return Ok(false);
        }
    }
    let header = VersionHeader { stamp: ts.bits(), undo: 0, flags: 0 };
    let bad = |what: &str| {
        Error::corrupted(format!(
            "the {} record of transaction {xid} on the row {id} of table {} {what}",
            record.kind, record.table
        ))
    };
    match (record.kind, old) {
        (RecordKind::Insert, None) => {
            let width = table.def().columns().len();
            let complete = record.values.len() == width
                && record.values.iter().enumerate().all(|(i, (c, _))| usize::from(*c) == i);
            if !complete {
                return Err(bad("does not hold every column"));
            }
            let values = record.values.iter().map(|(_, v)| v.clone()).collect();
            hot.insert(pages, schema, &Row { id, header, xmin: xid.xid32(), xmax: 0, values })?;
            table.saw(id);
        }
        (RecordKind::Update, Some(mut row)) => {
            for (c, v) in &record.values {
                let slot = row.values.get_mut(usize::from(*c));
                *slot.ok_or_else(|| bad("names a column that the table does not have"))? =
                    v.clone();
            }
            let row = Row { header, xmin: xid.xid32(), xmax: 0, ..row };
            hot.replace(pages, schema, &row)?;
        }
        (RecordKind::Delete, Some(_)) => {
            hot.remove(pages, id)?;
        }
        // A delete leaves no row and so no stamp. A row that is not there was deleted by this record or by a later one that the pages hold already.
        (RecordKind::Update | RecordKind::Delete, None) => return Ok(false),
        (RecordKind::Insert, Some(_)) => return Err(bad("finds the row there already")),
        (kind, _) => {
            return Err(bad(&format!("has the kind {kind}, which this version does not write")));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, Ordering};

    use std::sync::atomic::AtomicBool;

    use rupg_buffer::{BufferPool, Checkpoint, PoolConfig, PoolMode};
    use rupg_common::{ShardId, SqlState};
    use rupg_file::{PAGE_SIZE, PageId};
    use rupg_log::{Log, Ring};
    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::{SimClock, SimIo, SimRng};
    use rupg_platform::{File, Io, MemoryPool, OpenMode, TaskHandle, Tasks};
    use rupg_table::{ColumnDef, TableDef};
    use rupg_types::{Datum, TypeId};

    use super::*;
    use crate::{HlcClock, Isolation, Transactions};

    type Rows = BTreeMap<RowId, Vec<Datum>>;

    /// A file that fails each write, sync and size change after a number of them, as a process that dies at that point.
    #[derive(Debug)]
    struct Cut {
        inner: Arc<dyn File>,
        left: AtomicI64,
    }

    impl Cut {
        fn take(&self) -> Result<()> {
            if self.left.fetch_sub(1, Ordering::SeqCst) <= 0 {
                return Err(Error::new(SqlState::IO_ERROR, "the process died"));
            }
            Ok(())
        }
    }

    impl File for Cut {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
            self.take()?;
            self.inner.write_at(offset, data)
        }
        fn sync(&self) -> Result<()> {
            self.take()?;
            self.inner.sync()
        }
        fn size(&self) -> Result<u64> {
            self.inner.size()
        }
        fn set_size(&self, size: u64) -> Result<()> {
            self.take()?;
            self.inner.set_size(size)
        }
    }

    /// A node: the file with its log and its pages, and one table. The root of the table stands in for the catalog, which comes later.
    struct Node {
        io: SimIo,
        file: Arc<Cut>,
        store: Arc<FileStore>,
        frames: u64,
        root: PageId,
        table: Arc<Table>,
        txns: Arc<Transactions<BufferPool>>,
    }

    impl Node {
        fn checkpoint(&self) -> Result<Hlc> {
            self.txns.checkpoint(&self.store, |_| Ok(Checkpoint::default()))
        }
    }

    fn def() -> TableDef {
        let col =
            |name: &str, nullable| ColumnDef { name: name.into(), ty: TypeId::INT8, nullable };
        TableDef::new(Oid(16384), "t", vec![col("k", false), col("v", true), col("w", true)])
            .unwrap()
    }

    fn file(io: &SimIo, mode: OpenMode) -> Arc<Cut> {
        let inner = io.open(Path::new("db"), mode).unwrap();
        Arc::new(Cut { inner, left: AtomicI64::new(i64::MAX) })
    }

    /// A pool of `frames` pages over `store`.
    fn pool(store: &Arc<FileStore>, frames: u64) -> Arc<BufferPool> {
        let memory = MemoryPool::new(64 << 20);
        let config = PoolConfig {
            shared_buffers: frames * PAGE_SIZE as u64,
            mode: Some(PoolMode::Table),
            window_pages: None,
        };
        BufferPool::new(store.clone(), &memory, config).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        io: SimIo,
        file: Arc<Cut>,
        store: Arc<FileStore>,
        log: Log,
        frames: u64,
        table: Arc<Table>,
        floor: Hlc,
        xid: Xid,
    ) -> Node {
        let pool = pool(&store, frames);
        let wall = Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000));
        let txns = Transactions::new(pool, HlcClock::new(wall, floor), xid);
        let txns = Arc::new(txns.with_log(Arc::new(log)));
        txns.filter_writes().unwrap();
        Node { io, file, store, frames, root: table.root(), table, txns }
    }

    /// A new file with an empty table in a pool of `frames` pages.
    fn create(seed: u64, frames: u64) -> Node {
        let io = SimIo::new(seed);
        let file = file(&io, OpenMode::CreateNew);
        let store = Arc::new(FileStore::create(file.clone(), [5; 16], Hlc::ZERO).unwrap());
        let ring = Ring::create(store.clone(), 0, 0, 1).unwrap();
        let log = Log::new(vec![ring]).unwrap();
        // The table is made in its own pool, which is written before the node starts.
        let pages = pool(&store, frames);
        let table = Arc::new(Table::create(&*pages, def(), ShardId(0)).unwrap());
        pages.flush_all().unwrap();
        drop(pages);
        let node = start(io, file, store, log, frames, table, Hlc::ZERO, Xid::FIRST);
        node.checkpoint().unwrap();
        node
    }

    fn stored(table: &Table, pages: &BufferPool) -> Vec<Row> {
        let mut out = Vec::new();
        table
            .hot()
            .scan(pages, RowId::from_bits(0), |row| {
                out.push(row);
                Ok(true)
            })
            .unwrap();
        out
    }

    /// Cuts the power, opens the file again and replays the log into the pages of the last checkpoint. A second replay finds each change in the pages and changes nothing.
    fn recover(node: Node) -> (Node, Replayed) {
        let Node { io, frames, root, .. } = node;
        io.crash_random().unwrap();
        let file = file(&io, OpenMode::ReadWrite);
        let store = Arc::new(FileStore::open(file.clone()).unwrap());
        let pages = pool(&store, frames);
        let table = Arc::new(Table::open(def(), ShardId(0), root, 0));
        let before = stored(&table, &pages);
        assert!(before.iter().all(|row| row.header.owner().is_none()));
        if let Some(last) = before.last() {
            table.saw(last.id);
        }
        let rec = Recovery::scan(&store).unwrap();
        let tables = |oid| (oid == table.def().oid()).then(|| table.clone());
        let done = replay(&*pages, &store, &rec, &tables).unwrap();
        let after = stored(&table, &pages);
        let again = replay(&*pages, &store, &rec, &tables).unwrap();
        assert_eq!(
            (again.commits, again.applied + again.skipped),
            (done.commits, done.applied + done.skipped)
        );
        assert_eq!(stored(&table, &pages), after);
        pages.flush_all().unwrap();
        drop(pages);
        let c = Checkpoint { timestamp: done.newest, ..Checkpoint::default() };
        let log = rec.finish(store.clone(), &c).unwrap();
        let xid = done.next_xid.unwrap_or(Xid::FIRST);
        (start(io, file, store, log, frames, table, done.newest, xid), done)
    }

    fn rows(node: &Node) -> Rows {
        let t = node.txns.begin(Isolation::RepeatableRead);
        let mut out = Rows::new();
        t.scan(&node.table, |id, values| {
            out.insert(id, values);
            Ok(true)
        })
        .unwrap();
        out
    }

    fn values(rng: &mut SimRng) -> Vec<Datum> {
        let mut value = || match rng.below(4) {
            0 => Datum::Null,
            n => Datum::Int8(rng.below(1_000) as i64 * n as i64),
        };
        vec![Datum::Int8(7), value(), value()]
    }

    /// One random transaction on `node`. A commit moves `model` on. Some transactions take a checkpoint before they end. A commit that fails gives the rows that it would have made. A checkpoint that fails gives `None`.
    fn transaction(
        node: &Node,
        rng: &mut SimRng,
        model: &mut Rows,
    ) -> std::result::Result<(), Option<Rows>> {
        let table = &node.table;
        let mut next = model.clone();
        let mut t = node.txns.begin(Isolation::ReadCommitted);
        for _ in 0..1 + rng.below(6) {
            let some = (!next.is_empty())
                .then(|| *next.keys().nth(rng.below(next.len() as u64) as usize).unwrap());
            match (rng.below(10), some) {
                (0..4, _) | (_, None) => {
                    let v = values(rng);
                    let id = t.insert(table, &v).unwrap();
                    next.insert(id, v);
                }
                (4..8, Some(id)) => {
                    let v = if rng.below(8) == 0 { next[&id].clone() } else { values(rng) };
                    assert!(t.update(table, id, &v).unwrap());
                    next.insert(id, v);
                }
                (_, Some(id)) => {
                    assert!(t.delete(table, id).unwrap());
                    next.remove(&id);
                }
            }
            t.next_command().unwrap();
        }
        if rng.below(12) == 0 && node.checkpoint().is_err() {
            return Err(None);
        }
        if rng.below(7) == 0 {
            t.rollback().unwrap();
            return Ok(());
        }
        match t.commit() {
            Ok(_) => {
                *model = next;
                Ok(())
            }
            Err(e) => {
                assert_eq!(e.state(), SqlState::IO_ERROR, "{e}");
                Err(Some(next))
            }
        }
    }

    /// Random transactions with checkpoints, and a cut of the file at a random write or sync. The checkpoints come between transactions and while a transaction has rows that are not committed.
    #[test]
    fn a_crash_keeps_each_acknowledged_commit() {
        let (mut lost, mut kept, mut commits, mut checkpoints) = (0, 0, 0, 0);
        for seed in 0..10u64 {
            let mut node = create(seed, 64);
            let mut rng = SimRng::new(seed + 50);
            let mut model = Rows::new();
            for round in 0..6 {
                node.file.left.store(10 + rng.below(150) as i64, Ordering::SeqCst);
                let mut unknown = None;
                for _ in 0..60 {
                    if let Err(next) = transaction(&node, &mut rng, &mut model) {
                        unknown = next;
                        break;
                    }
                    if rng.below(10) == 0 {
                        if node.checkpoint().is_err() {
                            break;
                        }
                        checkpoints += 1;
                    }
                }
                let (back, done) = recover(node);
                node = back;
                commits += done.commits;
                let got = rows(&node);
                if got != model {
                    assert_eq!(Some(&got), unknown.as_ref(), "seed {seed} round {round}");
                    model = got;
                    kept += 1;
                } else if unknown.is_some() {
                    lost += 1;
                }
                // New row ids come after each row that recovery found.
                let mut t = node.txns.begin(Isolation::ReadCommitted);
                let id = t.insert(&node.table, &values(&mut rng)).unwrap();
                assert!(model.keys().all(|&old| old < id));
                t.rollback().unwrap();
            }
        }
        assert!(
            lost > 0 && kept > 0 && commits > 100 && checkpoints > 100,
            "{lost} {kept} {commits} {checkpoints}"
        );
    }

    /// A pool of 16 pages holds a table of more pages, so the clock writes pages while transactions are open. Each open transaction changes rows in a few leaves only, because the clock cannot evict a leaf with rows that are not committed. A checkpoint while a transaction is open writes no row of it, and recovery gives the rows of the commits.
    #[test]
    fn the_file_holds_no_row_that_is_not_committed() {
        let node = create(5, 16);
        let table = &node.table;
        let mut rng = SimRng::new(5);
        let mut model = Rows::new();
        for _ in 0..8 {
            let mut t = node.txns.begin(Isolation::ReadCommitted);
            for _ in 0..1_000 {
                let v = values(&mut rng);
                model.insert(t.insert(table, &v).unwrap(), v);
            }
            t.commit().unwrap();
        }
        let ids: Vec<RowId> = model.keys().copied().collect();
        // The open transaction changes the rows of the first leaves and adds rows to the last leaf.
        let mut open = node.txns.begin(Isolation::ReadCommitted);
        let mut later = model.clone();
        for &id in ids.iter().take(300).step_by(3) {
            if rng.below(2) == 0 {
                let v = values(&mut rng);
                assert!(open.update(table, id, &v).unwrap());
                later.insert(id, v);
            } else {
                assert!(open.delete(table, id).unwrap());
                later.remove(&id);
            }
        }
        for _ in 0..50 {
            let v = values(&mut rng);
            later.insert(open.insert(table, &v).unwrap(), v);
        }
        let mut t = node.txns.begin(Isolation::ReadCommitted);
        for &id in ids.iter().skip(4_000).step_by(31).take(20) {
            let v = values(&mut rng);
            assert!(t.update(table, id, &v).unwrap());
            model.insert(id, v.clone());
            later.insert(id, v);
        }
        t.commit().unwrap();
        assert_eq!(rows(&node), model);
        assert!(node.txns.pages().stats().evictions > 20);
        node.checkpoint().unwrap();

        // After the checkpoint, one transaction commits and the open one commits too.
        let mut t = node.txns.begin(Isolation::ReadCommitted);
        for &id in ids.iter().skip(6_000).step_by(31).take(20) {
            let v = values(&mut rng);
            assert!(t.update(table, id, &v).unwrap());
            later.insert(id, v);
        }
        t.commit().unwrap();
        open.commit().unwrap();
        assert_eq!(rows(&node), later);
        let (node, done) = recover(node);
        assert_eq!(rows(&node), later);
        assert_eq!((done.commits, done.skipped), (2, 0));
    }

    /// Writers on 4 threads commit at the same time, and a fifth thread takes checkpoints. The blocks are in timestamp order in the ring, and the pages of the last checkpoint with the blocks after it give the same rows.
    #[test]
    fn replay_gives_the_rows_of_concurrent_commits() {
        let node = create(77, 64);
        let mut t = node.txns.begin(Isolation::ReadCommitted);
        let ids: Arc<Vec<RowId>> = Arc::new(
            (0..30)
                .map(|k| {
                    t.insert(&node.table, &[Datum::Int8(k), Datum::Int8(100), Datum::Null]).unwrap()
                })
                .collect(),
        );
        t.commit().unwrap();
        let handles: Vec<TaskHandle> = (0..4u64)
            .map(|n| {
                let (txns, table, ids) = (node.txns.clone(), node.table.clone(), ids.clone());
                let task = Box::new(move || {
                    let mut rng = SimRng::new(n);
                    let mut done = 0;
                    while done < 150 {
                        let isolation = if n % 2 == 0 {
                            Isolation::RepeatableRead
                        } else {
                            Isolation::ReadCommitted
                        };
                        let mut t = txns.begin(isolation);
                        let step = (|| {
                            let id = ids[rng.below(ids.len() as u64) as usize];
                            let mut v = t.get(&table, id)?.unwrap();
                            v[2] = Datum::Int8(rng.below(1_000) as i64);
                            t.update(&table, id, &v)?;
                            let extra = t.insert(&table, &values(&mut rng))?;
                            if rng.below(2) == 0 {
                                t.next_command()?;
                                t.delete(&table, extra)?;
                            }
                            Ok::<(), Error>(())
                        })();
                        match step {
                            Ok(()) => {
                                t.commit().unwrap();
                                done += 1;
                            }
                            Err(e) => {
                                let retry =
                                    [SqlState::SERIALIZATION_FAILURE, SqlState::DEADLOCK_DETECTED];
                                assert!(retry.contains(&e.state()), "{e}");
                                t.rollback().unwrap();
                            }
                        }
                    }
                });
                OsTasks.spawn(&format!("writer {n}"), task).unwrap()
            })
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let checkpoints = Arc::new(AtomicI64::new(0));
        let checkpointer = {
            let (txns, store) = (node.txns.clone(), node.store.clone());
            let (stop, checkpoints) = (stop.clone(), checkpoints.clone());
            let task = Box::new(move || {
                while !stop.load(Ordering::Acquire) {
                    txns.checkpoint(&store, |_| Ok(Checkpoint::default())).unwrap();
                    checkpoints.fetch_add(1, Ordering::Relaxed);
                }
            });
            OsTasks.spawn("checkpoints", task).unwrap()
        };
        for h in handles {
            h.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        checkpointer.join().unwrap();
        let before = rows(&node);
        let (node, done) = recover(node);
        assert_eq!(rows(&node), before);
        assert!(checkpoints.load(Ordering::Relaxed) > 0);
        assert!(done.commits < 601 && done.skipped == 0, "{done:?}");
    }
}

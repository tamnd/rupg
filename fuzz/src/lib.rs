//! The M1 fuzz targets of spec/21 section 21.7: `file_open`, `page_decode` and `log_replay`.
//!
//! Each target starts from the same base file. The base is a database on the simulated disk with two tables, commits before and after a checkpoint, and no close, so each open replays the log. The base keeps the model of the tables after each commit. A target changes some bytes of a fork of the disk, runs `rupg check` and opens the file.
//!
//! A panic is a failure. An open that gives tables that are not equal to one of the kept models is also a failure, because the file then gave data that it does not hold.
//!
//! The input of each target is a list of changes of 7 bytes each: an offset of 4 bytes in little-endian order, a kind and a value of 2 bytes. The offset is taken modulo the length of the range that the target changes. Bytes after the last full change are not used.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rupg::{
    ColumnDef, Database, Datum, Isolation, Options, Platform, Result, RowId, SqlState, TypeId,
};
use rupg_buffer::{FileStore, Store};
use rupg_common::Hlc;
use rupg_file::{PAGE_SIZE, Page, PageHeader, PageKind, checksum, seal, stored_checksum};
use rupg_log::Recovery;
use rupg_platform::sim::{SimClock, SimEntropy, SimIo, SimRng};
use rupg_platform::{File, Io, OpenMode};

/// The path of the database file on the simulated disk.
const PATH: &str = "fuzz.rupg";

/// The commits before the checkpoint. They fill a number of hot leaves.
const BEFORE: usize = 32;

/// The commits after the checkpoint. An open replays them from the log.
const AFTER: usize = 16;

/// The first bytes of a page that `page_decode` does not change: the checksum, the timestamp and the logical page number. A change there gives `XX001` before the decoders run.
const KEPT: usize = 24;

/// The rows of each table, by table name.
pub type Model = BTreeMap<String, BTreeMap<RowId, Vec<Datum>>>;

/// The base file and what the targets know about it.
pub struct Base {
    io: SimIo,
    /// The model after each commit, in commit order. The first one is the model before the first commit.
    states: Vec<Model>,
    /// The index in `states` of the model at the last checkpoint.
    checkpoint: usize,
    /// The size of the file in bytes.
    size: u64,
    /// The bytes of the log ring from the redo position to one unit after the end of the last block.
    log: Range<u64>,
    /// The physical pages that hold the current copy of a hot store page.
    pages: Vec<u64>,
}

/// The base file. The first call builds it.
pub fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| build().expect("the base file builds"))
}

/// Changes bytes anywhere in the file, and can cut the file. The open must give one of the kept models or an error.
pub fn file_open(data: &[u8]) {
    let base = base();
    let io = base.io.fork();
    let file = open_file(&io);
    change(&mut FileBytes { file: &file, size: base.size }, 0..base.size, data, true);
    file.sync().expect("a sync of the simulated disk works");
    if let Ok(model) = try_file(&io) {
        assert!(base.states.contains(&model), "the open gave tables that no commit gave");
    }
}

/// Changes the bytes of one hot store page after the logical page number, and seals the page again, so the page reaches the decoders. The first 2 bytes of the input select the page. Only a panic or a hang is a failure.
pub fn page_decode(data: &[u8]) {
    let base = base();
    let Some((select, data)) = data.split_first_chunk::<2>() else {
        return;
    };
    let physical = base.pages[usize::from(u16::from_le_bytes(*select)) % base.pages.len()];
    let io = base.io.fork();
    let file = open_file(&io);
    let mut page: Box<Page> = Box::new([0; PAGE_SIZE]);
    file.read_at(physical * PAGE_SIZE as u64, &mut page[..]).expect("the page is in the file");
    change(&mut &mut page[..], KEPT as u64..PAGE_SIZE as u64, data, false);
    seal(&mut page);
    file.write_at(physical * PAGE_SIZE as u64, &page[..])
        .expect("a write of the simulated disk works");
    file.sync().expect("a sync of the simulated disk works");
    let _ = try_file(&io);
}

/// Changes bytes of the log ring between the redo position and the end of the log. The open must give a model at or after the checkpoint, or `XX001`.
pub fn log_replay(data: &[u8]) {
    let base = base();
    let io = base.io.fork();
    let file = open_file(&io);
    change(&mut FileBytes { file: &file, size: base.size }, base.log.clone(), data, false);
    file.sync().expect("a sync of the simulated disk works");
    match try_file(&io) {
        Ok(model) => assert!(
            base.states[base.checkpoint..].contains(&model),
            "the replay gave tables that are not a prefix of the commits"
        ),
        Err(e) => assert_eq!(e.state(), SqlState::DATA_CORRUPTED, "{e}"),
    }
}

/// Runs the check, then opens the file and reads every table.
fn try_file(io: &SimIo) -> Result<Model> {
    let platform = platform(io);
    let _ = rupg::check_on(&platform, Path::new(PATH));
    let mut options = options();
    options.create = false;
    let db = Database::open_on(&platform, Path::new(PATH), &options)?;
    read(&db)
}

/// Bytes that a change can read and write.
trait Bytes {
    fn read(&mut self, at: u64, buf: &mut [u8]);
    fn write(&mut self, at: u64, data: &[u8]);
    /// Cuts the bytes at `at`. Later changes after the cut do nothing.
    fn cut(&mut self, at: u64);
    fn end(&self) -> u64;
}

struct FileBytes<'a> {
    file: &'a Arc<dyn File>,
    size: u64,
}

impl Bytes for FileBytes<'_> {
    fn read(&mut self, at: u64, buf: &mut [u8]) {
        self.file.read_at(at, buf).expect("a read inside the file works");
    }
    fn write(&mut self, at: u64, data: &[u8]) {
        self.file.write_at(at, data).expect("a write of the simulated disk works");
    }
    fn cut(&mut self, at: u64) {
        self.file.set_size(at).expect("a cut of the simulated disk works");
        self.size = at;
    }
    fn end(&self) -> u64 {
        self.size
    }
}

impl Bytes for &mut [u8] {
    fn read(&mut self, at: u64, buf: &mut [u8]) {
        buf.copy_from_slice(&self[at as usize..][..buf.len()]);
    }
    fn write(&mut self, at: u64, data: &[u8]) {
        self[at as usize..][..data.len()].copy_from_slice(data);
    }
    fn cut(&mut self, _: u64) {}
    fn end(&self) -> u64 {
        self.len() as u64
    }
}

/// Applies the changes of `input` to `range` of `bytes`. The kinds are: 0 changes the bits of one byte, 1 sets 2 bytes, 2 sets up to 512 bytes to zero, 3 copies 64 bytes from another place in the range, and 4 cuts the file if `cut` is true.
fn change(bytes: &mut impl Bytes, range: Range<u64>, input: &[u8], cut: bool) {
    let len = range.end - range.start;
    for c in input.as_chunks::<7>().0 {
        let at = range.start + u64::from(u32::from_le_bytes([c[0], c[1], c[2], c[3]])) % len;
        let value = u16::from_le_bytes([c[5], c[6]]);
        let end = range.end.min(bytes.end());
        if at >= end {
            continue;
        }
        let room = (end - at) as usize;
        match c[4] % 5 {
            0 => {
                let mut b = [0];
                bytes.read(at, &mut b);
                bytes.write(at, &[b[0] ^ value as u8]);
            }
            1 => bytes.write(at, &value.to_le_bytes()[..room.min(2)]),
            2 => bytes.write(at, &vec![0; room.min(usize::from(value % 512) + 1)]),
            3 => {
                let from = range.start + u64::from(value) * 64 % len;
                let n = room.min(64).min(end.saturating_sub(from) as usize);
                if n == 0 {
                    continue;
                }
                let mut buf = vec![0; n];
                bytes.read(from, &mut buf);
                bytes.write(at, &buf);
            }
            _ if cut => bytes.cut(at),
            _ => {}
        }
    }
}

fn open_file(io: &SimIo) -> Arc<dyn File> {
    io.open(Path::new(PATH), OpenMode::ReadWrite).expect("the base file exists")
}

fn options() -> Options {
    Options {
        memory_limit: 16 << 20,
        shared_buffers: 2 << 20,
        log_extents: 1,
        checkpoint_log: Some(1 << 20),
        create: true,
        window_pages: Some(1 << 16),
    }
}

fn platform(io: &SimIo) -> Platform {
    Platform {
        io: Arc::new(io.clone()),
        clock: Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000)),
        entropy: Arc::new(SimEntropy::new(1)),
    }
}

fn columns() -> Vec<ColumnDef> {
    let col = |name: &str, ty, nullable| ColumnDef { name: name.into(), ty, nullable };
    vec![col("k", TypeId::INT8, false), col("v", TypeId::TEXT, true)]
}

fn values(rng: &mut SimRng) -> Vec<Datum> {
    let v = match rng.below(5) {
        0 => Datum::Null,
        n => Datum::Text("v".repeat(rng.below(40 * n) as usize)),
    };
    vec![Datum::Int8(rng.below(1 << 40) as i64), v]
}

fn read(db: &Database) -> Result<Model> {
    let tx = db.transaction_with(Isolation::RepeatableRead)?;
    let mut model = Model::new();
    for name in db.table_names() {
        let t = db.table(&name)?;
        model.insert(name, tx.scan(&t, ..)?.into_iter().collect());
    }
    Ok(model)
}

fn build() -> Result<Base> {
    let io = SimIo::new(1);
    let db = Database::open_on(&platform(&io), Path::new(PATH), &options())?;
    let tables = [db.create_table("a", columns())?, db.create_table("b", columns())?];
    let mut model = Model::new();
    for t in &tables {
        model.insert(t.name().to_string(), BTreeMap::new());
    }
    let mut rng = SimRng::new(7);
    let mut states = vec![model.clone()];
    let mut checkpoint = 0;
    for step in 0..BEFORE + AFTER {
        if step == BEFORE {
            db.checkpoint()?;
            checkpoint = states.len() - 1;
        }
        let ops = if step < BEFORE { 40 } else { 3 };
        let mut tx = db.transaction()?;
        for _ in 0..ops {
            let t = &tables[rng.below(2) as usize];
            let rows = model.get_mut(t.name()).expect("each table is in the model");
            let v = values(&mut rng);
            let old = (!rows.is_empty() && rng.below(4) == 0)
                .then(|| rows.keys().nth(rng.below(rows.len() as u64) as usize).copied())
                .flatten();
            match (old, rng.below(2)) {
                (Some(id), 0) => {
                    tx.update(t, id, &v)?;
                    rows.insert(id, v);
                }
                (Some(id), _) => {
                    tx.delete(t, id)?;
                    rows.remove(&id);
                }
                (None, _) => {
                    let id = tx.insert(t, &v)?;
                    rows.insert(id, v);
                }
            }
        }
        tx.commit()?;
        states.push(model.clone());
    }
    // No close: the open of each target replays the commits after the checkpoint.
    drop(tables);
    drop(db);

    let file = io.open(Path::new(PATH), OpenMode::Read)?;
    let size = file.size()?;
    let store = FileStore::open(file.clone())?;
    let ring = store.rings().into_iter().next().expect("the file has a log ring");
    let ring_bytes = ring.size();
    let end = Recovery::scan(&store)?.end(0).unwrap_or(ring.redo);
    assert!(end - ring.redo < ring_bytes, "the log of the base wraps");
    let start = ring.extents[0] * PAGE_SIZE as u64;
    let log = start + ring.redo % ring_bytes..start + (end % ring_bytes + 4096).min(ring_bytes);

    let mut page: Box<Page> = Box::new([0; PAGE_SIZE]);
    let mut live = BTreeSet::new();
    for id in store.allocated() {
        store.read(id, &mut page)?;
        live.insert(stored_checksum(&page));
    }
    let mut pages = Vec::new();
    for physical in 0..size / PAGE_SIZE as u64 {
        file.read_at(physical * PAGE_SIZE as u64, &mut page[..])?;
        let sum = stored_checksum(&page);
        let hot = PageHeader::read(&page)
            .is_ok_and(|h| matches!(h.kind, PageKind::HotLeaf | PageKind::HotInner));
        if hot && sum == checksum(&page[8..]) && live.contains(&sum) {
            pages.push(physical);
        }
    }
    Ok(Base { io, states, checkpoint, size, log, pages })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_opens_with_the_last_commit() {
        let base = base();
        assert_eq!(try_file(&base.io.fork()).unwrap(), *base.states.last().unwrap());
        assert_eq!(base.states.len(), 1 + BEFORE + AFTER);
        assert!(base.pages.len() > 8, "{} hot pages", base.pages.len());
        assert!(base.log.end - base.log.start > 4096, "{:?}", base.log);
    }

    #[test]
    fn random_changes_do_not_panic() {
        let mut rng = SimRng::new(3);
        for _ in 0..200 {
            let mut data = vec![0; rng.below(50) as usize];
            rng.fill(&mut data);
            file_open(&data);
            page_decode(&data);
            log_replay(&data);
        }
    }
}

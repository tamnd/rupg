//! The T8 crash test of spec/21 section 21.9, for the M1 workload: key-value writes through the Rust API, header slot changes and checkpoints.
//!
//! A seeded workload runs on the simulated disk. Before each sync of the file, the test lists the writes that are not synced and builds crash files from them (section 21.9.2). For a small set it builds every subset. For a larger set it builds every prefix in issue order and in reverse order, each prefix with the next write torn, and a seeded sample with tears and a random order. A crash file that has the same bytes as an earlier one counts once and is not tried again.
//!
//! For each crash file the test opens the database, which runs recovery, and checks that the tables hold exactly the acknowledged commits, or those and the one commit or new table that was in progress at the cut. A transaction that did not commit is not in the file. Then `rupg check` must report no error.
//!
//! The workload starts after the open that makes the file returns. A cut during the first open can leave a file that is not a database, and the user gets an error.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rupg::{
    ColumnDef, Database, Datum, Error, Isolation, Options, Platform, Result, RowId, TypeId,
};
use rupg_common::Hlc;
use rupg_platform::sim::{CrashPlan, Fate, SimClock, SimEntropy, SimIo, SimRng, Unsynced};
use rupg_platform::{File, Io, OpenMode};

/// The path of the database file on the simulated disk.
const PATH: &str = "crash.rupg";

/// A set with this many writes or fewer gives every subset.
const SMALL: usize = 8;

/// The rows of each table, by table name.
pub type Model = BTreeMap<String, BTreeMap<RowId, Vec<Datum>>>;

/// The size of a run.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The seed of the workload and of the samples.
    pub seed: u64,
    /// The number of steps of the workload. A step is a transaction, a checkpoint, a new table, a clean close or a held transaction.
    pub steps: u32,
    /// The number of random plans at each sync point with more than [`SMALL`] writes.
    pub sample: usize,
}

/// The counts of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The syncs of the workload, each one a point where the power can go.
    pub sync_points: u64,
    /// The crash files with distinct bytes. Each one was opened and checked.
    pub files: u64,
    /// The plans that gave a file that an earlier plan gave.
    pub same: u64,
    /// The commits that the workload acknowledged.
    pub commits: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Counts) {
        self.sync_points += o.sync_points;
        self.files += o.files;
        self.same += o.same;
        self.commits += o.commits;
    }
}

/// What a crash file must hold.
#[derive(Clone, Debug, Default)]
struct Expect {
    /// The acknowledged commits.
    acked: Model,
    /// The model with the change that is in progress, if one is.
    pending: Option<Model>,
}

/// The state that the hook and the workload share.
#[derive(Debug, Default)]
struct Shared {
    /// The hook runs only when this is true.
    armed: bool,
    expect: Expect,
    counts: Counts,
    /// The checksums of the crash files so far.
    seen: BTreeSet<u64>,
    rng_state: u64,
    /// The first failure. The hook cannot return it through the engine, so the workload reads it after each step.
    failure: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An `Io` that calls the hook before each sync of a file.
struct Hooked {
    io: SimIo,
    hook: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for Hooked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooked").field("io", &self.io).finish_non_exhaustive()
    }
}

struct HookedFile {
    file: Arc<dyn File>,
    hook: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for HookedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookedFile").field("file", &self.file).finish_non_exhaustive()
    }
}

impl Io for Hooked {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Arc<dyn File>> {
        let file = self.io.open(path, mode)?;
        Ok(Arc::new(HookedFile { file, hook: self.hook.clone() }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        self.io.remove(path)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        self.io.exists(path)
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        self.io.sync_dir(path)
    }
}

impl File for HookedFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.file.write_at(offset, data)
    }

    fn sync(&self) -> Result<()> {
        (self.hook)();
        self.file.sync()
    }

    fn size(&self) -> Result<u64> {
        self.file.size()
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.file.set_size(size)
    }
}

/// Small sizes and a small window, so that each crash file opens fast, and a small log limit, so that the transactions start checkpoints.
fn options() -> Options {
    Options {
        memory_limit: 16 << 20,
        shared_buffers: 2 << 20,
        log_extents: 1,
        checkpoint_log: Some(48 << 10),
        create: true,
        window_pages: Some(1 << 16),
    }
}

fn platform(io: Arc<dyn Io>, seed: u64) -> Platform {
    Platform {
        io,
        clock: Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000)),
        entropy: Arc::new(SimEntropy::new(seed)),
    }
}

fn columns() -> Vec<ColumnDef> {
    let col = |name: &str, ty, nullable| ColumnDef { name: name.into(), ty, nullable };
    vec![col("k", TypeId::INT8, false), col("v", TypeId::TEXT, true)]
}

fn values(rng: &mut SimRng) -> Vec<Datum> {
    let v = match rng.below(5) {
        0 => Datum::Null,
        n => Datum::Text("v".repeat(rng.below(60 * n) as usize)),
    };
    vec![Datum::Int8(rng.below(1 << 40) as i64), v]
}

/// The tables and rows of a database.
fn read(db: &Database) -> Result<Model> {
    let tx = db.transaction_with(Isolation::RepeatableRead)?;
    let mut model = Model::new();
    for name in db.table_names() {
        let t = db.table(&name)?;
        model.insert(name, tx.scan(&t, ..)?.into_iter().collect());
    }
    Ok(model)
}

/// The plans for the writes of one sync point.
fn plans(unsynced: &[Unsynced], rng: &mut SimRng, sample: usize) -> Vec<CrashPlan> {
    let n = unsynced.len();
    let plan = |fates: Vec<Fate>| CrashPlan { fates, order: Vec::new() };
    let mut out = Vec::new();
    if n <= SMALL {
        for mask in 0u32..1 << n {
            let fates = (0..n)
                .map(|i| if mask & 1 << i != 0 { Fate::Written } else { Fate::Lost })
                .collect();
            out.push(plan(fates));
        }
    } else {
        for k in 0..=n {
            out.push(plan(
                (0..n).map(|i| if i < k { Fate::Written } else { Fate::Lost }).collect(),
            ));
            out.push(plan(
                (0..n).map(|i| if i >= n - k { Fate::Written } else { Fate::Lost }).collect(),
            ));
        }
    }
    // Each prefix with the next write torn at a sector or at a page of the disk.
    for (k, op) in unsynced.iter().enumerate() {
        let tears = op.tears(if rng.below(2) == 0 { 4096 } else { 512 });
        if tears.is_empty() {
            continue;
        }
        let keep = tears[rng.below(tears.len() as u64) as usize];
        out.push(plan(
            (0..n)
                .map(|i| match i.cmp(&k) {
                    std::cmp::Ordering::Less => Fate::Written,
                    std::cmp::Ordering::Equal => Fate::Torn { keep },
                    std::cmp::Ordering::Greater => Fate::Lost,
                })
                .collect(),
        ));
    }
    if n > SMALL {
        for _ in 0..sample {
            let fates = unsynced
                .iter()
                .map(|op| match rng.below(3) {
                    0 => Fate::Lost,
                    1 => Fate::Written,
                    _ => {
                        let tears = op.tears(if rng.below(2) == 0 { 4096 } else { 512 });
                        match tears.len() as u64 {
                            0 => Fate::Written,
                            t => Fate::Torn { keep: tears[rng.below(t) as usize] },
                        }
                    }
                })
                .collect();
            let mut order: Vec<usize> = (0..n).collect();
            rng.shuffle(&mut order);
            out.push(CrashPlan { fates, order });
        }
    }
    out
}

/// Builds the crash files of the current sync point and checks each one. The result is the first failure.
fn sync_point(
    io: &SimIo,
    shared: &Mutex<Shared>,
    config: &Config,
) -> std::result::Result<(), String> {
    let (expect, mut rng) = {
        let mut s = lock(shared);
        s.counts.sync_points += 1;
        s.rng_state = s.rng_state.wrapping_add(1);
        (s.expect.clone(), SimRng::new(config.seed ^ s.rng_state.rotate_left(32)))
    };
    let base = io.fork();
    let point = lock(shared).counts.sync_points;
    for plan in plans(&base.unsynced(), &mut rng, config.sample) {
        let file = base.fork();
        file.crash(&plan).map_err(|e| e.to_string())?;
        let digest = file_digest(&file);
        let new = lock(shared).seen.insert(digest);
        if !new {
            lock(shared).counts.same += 1;
            continue;
        }
        try_file(&file, &expect, config.seed)
            .map_err(|e| format!("seed {} sync point {point} plan {plan:?}: {e}", config.seed))?;
        lock(shared).counts.files += 1;
    }
    Ok(())
}

/// A checksum of the bytes of the database file. Two files with the same bytes have the same result.
fn file_digest(io: &SimIo) -> u64 {
    let mut h = 0u64;
    io.read_pieces(Path::new(PATH), |piece| {
        h = h.rotate_left(17) ^ rupg_file::checksum(piece).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= piece.len() as u64;
    });
    h
}

/// Opens one crash file, compares its tables with the model, and runs `rupg check`.
fn try_file(io: &SimIo, expect: &Expect, seed: u64) -> std::result::Result<(), String> {
    let platform = platform(Arc::new(io.clone()), seed);
    let options = Options { create: false, ..options() };
    let db = Database::open_on(&platform, Path::new(PATH), &options).map_err(|e| e.to_string())?;
    let got = read(&db).map_err(|e| e.to_string())?;
    if got != expect.acked && Some(&got) != expect.pending.as_ref() {
        return Err(format!(
            "the file does not hold the acknowledged commits: {} tables with {} rows, expected {} rows",
            got.len(),
            got.values().map(BTreeMap::len).sum::<usize>(),
            expect.acked.values().map(BTreeMap::len).sum::<usize>(),
        ));
    }
    drop(db);
    rupg::check_on(&platform, Path::new(PATH)).map_err(|e| format!("rupg check: {e}"))?;
    Ok(())
}

/// A point of the workload that the caller sees.
#[derive(Debug)]
pub enum Event<'a> {
    /// A change starts: a commit or a new table. Until the next `Acked`, the file can hold this model or the last acknowledged model.
    Pending(&'a Model),
    /// The change was acknowledged. `commit` is false for a new table.
    Acked {
        /// The tables and rows after the change.
        model: &'a Model,
        /// True for a commit.
        commit: bool,
    },
    /// A step ended.
    Step,
    /// The last step ended. The held transaction, if there is one, is still open.
    End,
}

/// Opens or makes the database at `path` on `platform` and runs `steps` seeded steps on it. The workload calls `see` at each event and stops at the first error that `see` gives.
pub fn workload(
    platform: &Platform,
    path: &Path,
    seed: u64,
    steps: u32,
    mut see: impl FnMut(Event<'_>) -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    let err = |e: Error| e.to_string();
    let mut db = Database::open_on(platform, path, &options()).map_err(err)?;
    let mut acked = Model::new();
    see(Event::Acked { model: &acked, commit: false })?;
    let mut rng = SimRng::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut held = None;
    for _ in 0..steps {
        // The first step makes a table.
        let step = if acked.is_empty() { 0 } else { rng.below(40) };
        match step {
            0 if acked.len() < 3 => {
                let name = format!("t{}", acked.len());
                let mut next = acked.clone();
                next.insert(name.clone(), BTreeMap::new());
                see(Event::Pending(&next))?;
                db.create_table(&name, columns()).map_err(err)?;
                acked = next;
                see(Event::Acked { model: &acked, commit: false })?;
            }
            1 => db.checkpoint().map_err(err)?,
            2 => {
                // A clean close and an open, with a held transaction dropped first.
                drop(held.take());
                db.clone().close().map_err(err)?;
                drop(db);
                db = Database::open_on(platform, path, &options()).map_err(err)?;
            }
            3 if held.is_none() && !acked.is_empty() => {
                // A transaction that stays open over the next steps and does not commit.
                let name = acked.keys().next().cloned().unwrap_or_default();
                let t = db.table(&name).map_err(err)?;
                let mut tx = db.transaction().map_err(err)?;
                for _ in 0..1 + rng.below(30) {
                    tx.insert(&t, &values(&mut rng)).map_err(err)?;
                }
                held = Some(tx);
            }
            4 => drop(held.take()),
            _ if !acked.is_empty() => {
                let name = acked.keys().nth(rng.below(acked.len() as u64) as usize);
                let name = name.cloned().unwrap_or_default();
                let t = db.table(&name).map_err(err)?;
                let mut next = acked.clone();
                let rows = next.entry(name).or_default();
                let mut tx = db.transaction().map_err(err)?;
                for _ in 0..1 + rng.below(6) {
                    let some = (!rows.is_empty())
                        .then(|| rows.keys().nth(rng.below(rows.len() as u64) as usize).copied())
                        .flatten();
                    match (rng.below(10), some) {
                        (0..5, _) | (_, None) => {
                            let v = values(&mut rng);
                            rows.insert(tx.insert(&t, &v).map_err(err)?, v);
                        }
                        (5..8, Some(id)) => {
                            let v = values(&mut rng);
                            tx.update(&t, id, &v).map_err(err)?;
                            rows.insert(id, v);
                        }
                        (_, Some(id)) => {
                            tx.delete(&t, id).map_err(err)?;
                            rows.remove(&id);
                        }
                    }
                }
                if rng.below(10) == 0 {
                    tx.rollback().map_err(err)?;
                } else {
                    see(Event::Pending(&next))?;
                    tx.commit().map_err(err)?;
                    acked = next;
                    see(Event::Acked { model: &acked, commit: true })?;
                }
            }
            _ => {}
        }
        see(Event::Step)?;
    }
    see(Event::End)?;
    drop(held);
    Ok(())
}

/// Runs the workload of one seed and checks the crash files of each sync point.
pub fn run(config: Config) -> std::result::Result<Counts, String> {
    let io = SimIo::new(config.seed);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let hook: Arc<dyn Fn() + Send + Sync> = {
        let (io, shared) = (io.clone(), shared.clone());
        Arc::new(move || {
            let off = {
                let s = lock(&shared);
                !s.armed || s.failure.is_some()
            };
            if off {
                return;
            }
            if let Err(e) = sync_point(&io, &shared, &config) {
                lock(&shared).failure = Some(e);
            }
        })
    };
    let hooked: Arc<dyn Io> = Arc::new(Hooked { io: io.clone(), hook: hook.clone() });
    let platform = platform(hooked, config.seed);
    workload(&platform, Path::new(PATH), config.seed, config.steps, |event| {
        match event {
            // The workload starts after the open that makes the file returns.
            Event::Acked { model, commit } => {
                let mut s = lock(&shared);
                s.armed = true;
                s.expect = Expect { acked: model.clone(), pending: None };
                s.counts.commits += u64::from(commit);
            }
            Event::Pending(model) => lock(&shared).expect.pending = Some(model.clone()),
            Event::Step => {}
            // The power goes after the last step, with the held transaction open.
            Event::End => sync_point(&io, &shared, &config)?,
        }
        lock(&shared).failure.take().map_or(Ok(()), Err)
    })?;
    let counts = lock(&shared).counts;
    Ok(counts)
}

/// A checksum of a model. Two models with the same tables and rows have the same result in the same build.
pub fn digest(model: &Model) -> u64 {
    rupg_file::checksum(format!("{model:?}").as_bytes())
}

/// Opens the database at `path` on `platform`, which runs recovery, runs `rupg check` on it, and gives the digest of its tables and rows.
pub fn verify(platform: &Platform, path: &Path) -> std::result::Result<u64, String> {
    let options = Options { create: false, ..options() };
    let db = Database::open_on(platform, path, &options).map_err(|e| e.to_string())?;
    let model = read(&db).map_err(|e| e.to_string())?;
    drop(db);
    rupg::check_on(platform, path).map_err(|e| format!("rupg check: {e}"))?;
    Ok(digest(&model))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The T8 subset of each change: a short workload on two seeds.
    #[test]
    fn each_crash_file_holds_the_acknowledged_commits() {
        let mut total = Counts::default();
        for seed in 1..3 {
            total += run(Config { seed, steps: 25, sample: 2 }).unwrap();
        }
        assert!(total.commits > 30, "{total:?}");
        assert!(total.files > 120, "{total:?}");
    }

    #[test]
    fn small_sets_give_every_subset() {
        // Writes inside one sector have no tear.
        let op = |n: u64| Unsynced::Write { path: PATH.into(), offset: n * 4096, len: 100 };
        let mut rng = SimRng::new(1);
        let three = plans(&[op(0), op(1), op(2)], &mut rng, 4);
        assert_eq!(three.len(), 8);
        let many: Vec<Unsynced> = (0..10).map(op).collect();
        // Two prefixes of each length, and the sample.
        assert_eq!(plans(&many, &mut rng, 4).len(), 2 * 11 + 4);
    }
}

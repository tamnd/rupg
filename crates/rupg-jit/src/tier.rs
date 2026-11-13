// Lifted from rudb crates/rudb-qc/src/tier.rs at cd9f9676 (2026-10-05).
// Changes: the module `tier` of rupg-jit, so QIR is `crate::ir`, the runtime `crate::rt` and the backends `crate::clif` and `crate::direct`, and `clif` is the feature `jit`. The tiers take a `Platform`: the morsels and the compiles are timed on its `Clock`, and a background compile on `clif` runs on its `Tasks` and its parallelism caps them, in place of `std::thread` and `Instant`. With no tasks a function does not move up. `Tiers` and `Sink` are public, for the executor in another crate.

//! The tiers a pipeline function runs on, per section 8.1 of `spec/compiler/08-backends.md`:
//! `interp`, which always exists, `direct`, the single pass x86-64 backend, on x86-64, and
//! `clif`, the Cranelift backend, when the build has the `jit` feature. Both backends need a
//! platform with a code arena.
//!
//! Every function is lowered for the interpreter, whatever the tier. That is the fallback for a
//! function a backend does not lower, and it is what lets a query move between the tiers at
//! a morsel boundary: the two agree on the state and the morsel to the byte, so either one can
//! pick up where the other stopped.
//!
//! [`Switch`] moves a query between the tiers at morsel boundaries, which is the tier differential
//! of section 15 of the spec: the same query with and without the moves has to give the same bits.
//!
//! A function a backend refuses, or panics on, runs on `interp` and the refusal is kept in the
//! [`Report`]. A query never fails because a backend could not compile it.
//!
//! Nothing is compiled when the query is. Each pipeline's function is compiled when the pipeline
//! starts, which is rule I3 of section 9.3 of `spec/compiler/09-tiering-and-caching.md`, and a
//! pipeline that never starts costs nothing. Under `auto` a pipeline whose input is known to be at
//! most one morsel from what storage or the stage before it says runs on `interp` without being
//! compiled, which is rule I1, and every other one is compiled on `direct`, which is rule I2.
//!
//! A pipeline `auto` compiled on `direct` may then move up to `clif` while it runs, when the rows
//! it has left make the compile pay for itself, which the `up` module decides.

use std::fmt;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use rupg_platform::{Clock, TaskHandle, Tasks};

use crate::interp::Program;
use crate::ir::Module;
use crate::rt::code::Code;
use crate::rt::{Ablate, Rt};

use crate::tier::up::{Calibration, Class, Progress, Seen, Verdict};

mod cache;
mod up;

/// Which tier runs a query's pipelines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    /// `direct` when the build has it, then `clif`, then `interp`, with a pipeline whose input is
    /// at most one morsel on `interp`.
    #[default]
    Auto,
    /// The interpreter only.
    Interp,
    /// Machine code through Cranelift, with `interp` for what it does not lower.
    Clif,
    /// Machine code from the single pass backend, with `interp` for what it does not lower.
    Direct,
}

impl Tier {
    /// Every tier, in the order `SET qc_tier` lists them.
    pub const ALL: [Tier; 4] = [Tier::Auto, Tier::Interp, Tier::Clif, Tier::Direct];

    /// The name `SET qc_tier` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tier::Auto => "auto",
            Tier::Interp => "interp",
            Tier::Clif => "clif",
            Tier::Direct => "direct",
        }
    }

    /// The tier with this name, in any case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.name().eq_ignore_ascii_case(name.trim()))
    }

    /// Whether this build can run the tier. `clif` needs the `jit` feature and `direct` an
    /// x86-64 target.
    #[must_use]
    pub fn built(self) -> bool {
        match self {
            Tier::Auto | Tier::Interp => true,
            Tier::Clif => cfg!(feature = "jit"),
            Tier::Direct => cfg!(target_arch = "x86_64"),
        }
    }

    /// What the build message says a tier that is not [`Tier::built`] needs.
    #[must_use]
    pub fn needs(self) -> &'static str {
        match self {
            Tier::Clif => "a build with the jit feature",
            Tier::Direct => "an x86-64 build",
            Tier::Auto | Tier::Interp => "nothing",
        }
    }

    /// The tier `auto` compiles on in this build: `direct` when it is built, then `clif`, and
    /// `interp` when neither is.
    fn decided(self) -> Tier {
        match self {
            Tier::Auto if Tier::Direct.built() => Tier::Direct,
            Tier::Auto if Tier::Clif.built() => Tier::Clif,
            Tier::Auto => Tier::Interp,
            t => t,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// When a query moves between the tiers, which the tier differential forces and nothing else asks
/// for.
///
/// A switch happens at a morsel boundary: the tier is picked once per morsel a pipeline is fed,
/// and a morsel runs to the end on the tier it started on. Since the two tiers agree on the state
/// to the byte, a query that moves between them at random morsels has to give the answer it gives
/// on either one alone, and a difference is a bug in one of them that a whole query on one tier
/// could hide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Switch {
    /// Every morsel on the query's tier.
    #[default]
    Off,
    /// `n` morsels on the second tier, then `n` on `interp`, and so on.
    Every(u64),
    /// Each morsel on a tier drawn from this seed, so that a failure comes back with the seed.
    Random(u64),
}

impl Switch {
    /// The switch `SET qc_switch` names: `off`, `every:<n>` with `n` at least 1, or
    /// `random:<seed>`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Switch> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("off") {
            return Some(Switch::Off);
        }
        let (kind, n) = name.split_once(':')?;
        let n: u64 = n.trim().parse().ok()?;
        match kind.trim().to_ascii_lowercase().as_str() {
            "every" if n > 0 => Some(Switch::Every(n)),
            "random" => Some(Switch::Random(n)),
            _ => None,
        }
    }

    /// Whether morsel `n` of a query runs on the second tier.
    fn native(self, n: u64) -> bool {
        match self {
            Switch::Off => true,
            Switch::Every(k) => (n / k).is_multiple_of(2),
            Switch::Random(seed) => mix(seed ^ mix(n)) & 1 == 0,
        }
    }
}

impl fmt::Display for Switch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Switch::Off => f.write_str("off"),
            Switch::Every(n) => write!(f, "every:{n}"),
            Switch::Random(seed) => write!(f, "random:{seed}"),
        }
    }
}

/// Where a pipeline's rows go, which is what says whether a switch landed inside live state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sink {
    /// Rows out, with nothing held from one morsel to the next but the row count.
    Result,
    /// A hash aggregate, whose groups are live across morsels.
    Aggregate,
    /// A join build, whose table is live until it is finalized.
    Build,
}

/// How many times a query moved between the tiers from one morsel of a pipeline to the next, by
/// what the pipeline was filling at the time.
///
/// Section 15.3 of `spec/compiler/15-correctness.md` asks that the tier differential check it hit
/// live state: a switch in the middle of an aggregate or a build is the one that tests something.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    /// In a pipeline that produces rows.
    pub result: u64,
    /// In a pipeline that feeds a hash aggregate.
    pub aggregate: u64,
    /// In a pipeline that builds a join's table.
    pub build: u64,
}

impl Switches {
    /// All of them.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.result + self.aggregate + self.build
    }
}

/// splitmix64's finalizer, which is all a coin per morsel needs.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The most rows a pipeline's input can have for `auto` to leave it on `interp`: one morsel, per
/// the table of section 9.9 of the spec, where rule I1 and this number come from.
const ONE_MORSEL: usize = 16_384;

/// How a query is compiled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The tier its pipelines run on.
    pub tier: Tier,
    /// Whether it is made to move between the tiers as it runs.
    pub switch: Switch,
    /// Whether a pipeline stays on the tier it started on, where `auto` would otherwise move it up
    /// to `clif` when that pays.
    pub stay: bool,
    /// Whether every function is compiled anew, and none is taken from the code cache or kept
    /// in it.
    pub fresh: bool,
    /// The most rows one call of a body covers, which cuts a chunk of the scan into morsels of
    /// that many rows. Zero calls it once for the whole chunk.
    pub morsel: usize,
    /// Whether an aggregate with no groups that the table's statistics answer is run over the
    /// rows anyway, for testing the compiled code and for measuring what the statistics save.
    pub rows: bool,
    /// The techniques left out, for measuring what each one is worth.
    pub ablate: Ablate,
    /// Whether the first engine walks a semi or an anti join to the siblings of each row, which a
    /// plan with such a join is handed back for.
    pub walks: bool,
}

/// What the tiers need of the platform: the clock that times the morsels and the compiles, and the tasks that run a background compile on `clif`. With no tasks a function stays on the tier it started on.
#[derive(Clone, Debug)]
pub struct Platform {
    /// The monotonic clock.
    pub clock: Arc<dyn Clock>,
    /// The task scheduler of the store, for the background compiles.
    pub tasks: Option<Arc<dyn Tasks>>,
}

/// What the second tier did with a query's module, for `EXPLAIN (CODEGEN)` and the query log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The tier the query asked for, with `auto` decided.
    pub tier: &'static str,
    /// The time spent lowering the optimized plan and splitting it into pipelines, on every tier.
    pub plan: Duration,
    /// The time spent generating the QIR module from the pipelines, on every tier.
    pub generate: Duration,
    /// The time spent lowering and loading machine code, zero on `interp`.
    pub compile: Duration,
    /// How many functions the module has.
    pub functions: usize,
    /// How many QIR instructions they have between them.
    pub insts: usize,
    /// How many of them run as machine code.
    pub native: usize,
    /// The bytes of machine code loaded.
    pub bytes: usize,
    /// How many functions ran on `interp` under `auto` because their input was one morsel at most.
    pub small: usize,
    /// Each function that runs on `interp` although the tier compiles to machine code, and why.
    pub fallbacks: Vec<String>,
    /// How many functions moved up to `clif` while their pipeline ran.
    pub up: usize,
    /// The time spent compiling them, on a thread of its own and not on the workers.
    pub background: Duration,
    /// Why each of them moved up, with what the decision read.
    pub climbs: Vec<String>,
    /// How many of the functions that run as machine code were taken from the code cache.
    pub cached: usize,
    /// How many morsels ran on the version of their function that reads no NULL, because none of
    /// the columns it reads had one.
    pub nonull: u64,
    /// How many morsels a guard sent back to run again on its fallback.
    pub deopts: u64,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tier {}: {} of {} functions native, {} instructions, {} bytes, planned in {:.3} ms, generated in {:.3} ms, compiled in {:.3} ms",
            self.tier,
            self.native,
            self.functions,
            self.insts,
            self.bytes,
            self.plan.as_secs_f64() * 1e3,
            self.generate.as_secs_f64() * 1e3,
            self.compile.as_secs_f64() * 1e3,
        )?;
        if self.small > 0 {
            write!(f, ", {} on interp for one morsel of input", self.small)?;
        }
        if self.cached > 0 {
            write!(f, ", {} from the code cache", self.cached)?;
        }
        if self.nonull > 0 {
            write!(f, ", {} morsels with no NULL", self.nonull)?;
        }
        if self.deopts > 0 {
            write!(f, ", {} deoptimized", self.deopts)?;
        }
        if self.up > 0 {
            write!(
                f,
                ", {} moved up to clif in {:.3} ms in the background",
                self.up,
                self.background.as_secs_f64() * 1e3
            )?;
        }
        for reason in &self.fallbacks {
            write!(f, "\n  interp: {reason}")?;
        }
        for climb in &self.climbs {
            write!(f, "\n  clif: {climb}")?;
        }
        Ok(())
    }
}

/// A module on every tier it has.
pub struct Tiers {
    program: Program,
    /// The tier a function is compiled on, `interp` for none.
    tier: Tier,
    /// Whether a function whose input is one morsel at most stays on `interp`, which `auto` asks.
    small: bool,
    /// Each function's machine code, made when its pipeline starts.
    native: Vec<OnceLock<Option<Arc<Code>>>>,
    /// Whether the code cache is left alone.
    fresh: bool,
    /// The most rows a call covers, zero for a whole chunk.
    split: usize,
    /// Whether an aggregate the statistics answer reads its rows anyway.
    rows: bool,
    /// The techniques this query leaves out.
    ablate: Ablate,
    calibration: Calibration,
    /// The clock the progress is kept on, and its reading when the tiers were made.
    clock: Arc<dyn Clock>,
    epoch: Duration,
    /// The tasks a background compile runs on.
    tasks: Option<Arc<dyn Tasks>>,
    /// Each function's `clif` code once it has moved up, which a background compile fills. Empty
    /// unless the query is on `auto` with `direct` and `clif` both built, the only way to move up.
    upper: Vec<Arc<OnceLock<Option<Arc<Code>>>>>,
    /// Each function's rows and time so far, for the decision.
    progress: Vec<Progress>,
    /// Each function's class and size, set when it is compiled on `direct`.
    shape: Vec<OnceLock<(Class, usize)>>,
    /// The background compiles started.
    pending: Mutex<Vec<TaskHandle>>,
    report: Arc<Mutex<Report>>,
    switch: Switch,
    /// How many morsels have been fed, which numbers them for [`Switch`].
    morsels: AtomicU64,
    /// Per function, whether its last morsel ran as machine code, 2 before its first.
    last: Vec<AtomicU8>,
    /// How many times a morsel ran on another tier than the morsel of the same function before
    /// it, by [`Sink`].
    switches: [AtomicU64; 3],
    /// Per function, the morsels it was picked for over a guard's fallback and how many of them
    /// the guard sent back, for the rule of section 9.6 that gives up on it.
    tried: Vec<AtomicU64>,
    deopted: Vec<AtomicU64>,
}

impl fmt::Debug for Tiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tiers").field("report", &self.report).finish_non_exhaustive()
    }
}

impl Tiers {
    /// Lowers `module` for the interpreter and, when `options` asks for it, for the machine, on `platform`.
    #[must_use]
    pub fn new(module: &Module, options: Options, platform: &Platform) -> Tiers {
        let Options { tier, switch, stay, fresh, morsel, rows, ablate, walks: _ } = options;
        let program = Program::new(module);
        let counts = (
            AtomicU64::new(0),
            module.funcs.iter().map(|_| AtomicU8::new(2)).collect(),
            [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
        );
        let small = tier == Tier::Auto;
        let tier = tier.decided();
        let climb = small
            && !stay
            && tier == Tier::Direct
            && Tier::Clif.built()
            && platform.tasks.is_some();
        let each = if climb { module.funcs.len() } else { 0 };
        let insts = module.funcs.iter().flat_map(|f| &f.blocks).map(|b| b.prov.len()).sum();
        let report =
            Report { tier: tier.name(), functions: module.funcs.len(), insts, ..Report::default() };
        let native = module.funcs.iter().map(|_| OnceLock::new()).collect();
        let (morsels, last, switches) = counts;
        Tiers {
            program,
            tier,
            small,
            native,
            fresh,
            split: morsel,
            rows: rows || ablate.off(Ablate::STATS),
            ablate,
            calibration: Calibration::HOST,
            clock: Arc::clone(&platform.clock),
            epoch: platform.clock.monotonic(),
            tasks: platform.tasks.clone(),
            upper: (0..each).map(|_| Arc::default()).collect(),
            progress: (0..each).map(|_| Progress::default()).collect(),
            shape: (0..each).map(|_| OnceLock::new()).collect(),
            pending: Mutex::new(Vec::new()),
            report: Arc::new(Mutex::new(report)),
            switch,
            morsels,
            last,
            switches,
            tried: module.funcs.iter().map(|_| AtomicU64::new(0)).collect(),
            deopted: module.funcs.iter().map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// What the backend did so far.
    pub fn report(&self) -> Report {
        let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let sum = |v: &[AtomicU64]| v.iter().map(|n| n.load(Ordering::Relaxed)).sum::<u64>();
        report.deopts = sum(&self.deopted);
        report.nonull = sum(&self.tried) - report.deopts;
        report
    }

    /// The most rows one call of a body covers, zero for a whole chunk.
    pub fn split(&self) -> usize {
        self.split
    }

    /// Whether an aggregate the statistics answer reads its rows anyway.
    pub fn rows(&self) -> bool {
        self.rows
    }

    /// The techniques this query leaves out.
    pub fn ablate(&self) -> Ablate {
        self.ablate
    }

    /// Whether function `f`, a version behind a guard, is still worth picking, and counts the
    /// morsel it is picked for when it is. Section 9.6 gives up on it after three morsels sent
    /// back or more than one in sixteen.
    pub fn speculate(&self, f: usize) -> bool {
        let (Some(tried), Some(deopted)) = (self.tried.get(f), self.deopted.get(f)) else {
            return false;
        };
        let (back, all) = (deopted.load(Ordering::Relaxed), tried.load(Ordering::Relaxed));
        if back >= 3 || (all >= 16 && back * 16 > all) {
            return false;
        }
        tried.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Counts a morsel of function `f` that a guard sent back to its fallback.
    pub fn deopt(&self, f: usize) {
        if let Some(n) = self.deopted.get(f) {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Notes how long the plan took to lower and the module to generate, which happened before
    /// the tiers saw it.
    pub fn generated_in(&self, plan: Duration, generate: Duration) {
        let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
        report.plan = plan;
        report.generate = generate;
    }

    /// Readies function `f` of `module` for a pipeline that is about to start, whose input is
    /// `rows` rows when that is known and which probes a join table when `probes` is set. The
    /// function is compiled here, once, unless the tier is `interp` or `auto` keeps a function
    /// with at most one morsel of input on `interp`.
    pub fn prepare(&self, module: &Module, f: usize, rows: Option<usize>, probes: bool) {
        let Some(slot) = self.native.get(f) else { return };
        if slot.get().is_some() {
            return;
        }
        if self.small && rows.is_some_and(|n| n <= ONE_MORSEL) {
            if slot.set(None).is_ok() {
                self.report.lock().unwrap_or_else(PoisonError::into_inner).small += 1;
            }
            return;
        }
        let lower = match self.tier {
            Tier::Clif => clif::compile,
            Tier::Direct => direct::compile,
            Tier::Interp | Tier::Auto => {
                let _ = slot.set(None);
                return;
            }
        };
        let code = slot.get_or_init(|| {
            let func = module.funcs.get(f)?;
            let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            if self.fresh {
                return native::compile(func, &mut report, lower, &*self.clock).map(Arc::new);
            }
            // A function that moved up to `clif` in an earlier query starts there when it could
            // move up in this one, and has nothing left to decide.
            let mut hit = None;
            if !self.upper.is_empty() {
                hit = cache::get(&cache::key(Tier::Clif.name(), func));
                if hit.is_some()
                    && let Some(p) = self.progress.get(f)
                {
                    p.decided.store(true, Ordering::Relaxed);
                }
            }
            let key = cache::key(self.tier.name(), func);
            if let Some(code) = hit.or_else(|| cache::get(&key)) {
                report.native += 1;
                report.bytes += code.len();
                report.cached += 1;
                return Some(code);
            }
            let code = native::compile(func, &mut report, lower, &*self.clock).map(Arc::new)?;
            cache::put(key, &code);
            Some(code)
        });
        // Only a function on `direct` whose input has a known size may move up.
        if code.is_some()
            && let (Some(func), Some(p), Some(shape), Some(rows)) =
                (module.funcs.get(f), self.progress.get(f), self.shape.get(f), rows)
        {
            let insts = func.blocks.iter().map(|b| b.prov.len()).sum();
            let _ = shape.set((Class::of(func, probes), insts));
            p.total.store(rows as u64, Ordering::Relaxed);
        }
    }

    /// Readies every function as a pipeline with an input of unknown size would, for
    /// `EXPLAIN (CODEGEN)`, which shows the code without running the query.
    pub fn prepare_all(&self, module: &Module) {
        for f in 0..module.funcs.len() {
            self.prepare(module, f, None, false);
        }
    }

    /// The index of the function with this name.
    pub fn func(&self, name: &str) -> Option<usize> {
        self.program.func(name)
    }

    /// How many times a morsel ran on another tier than the morsel of its pipeline before it.
    pub fn switches(&self) -> Switches {
        let [result, aggregate, build] =
            self.switches.each_ref().map(|n| n.load(Ordering::Relaxed));
        Switches { result, aggregate, build }
    }

    /// The tier the next morsel of function `f`, which fills `sink`, runs on, as the argument
    /// [`Tiers::call`] takes: machine code when there is some, unless [`Switch`] says otherwise for
    /// this morsel.
    pub fn morsel(&self, f: usize, sink: Sink) -> bool {
        let n = self.morsels.fetch_add(1, Ordering::Relaxed);
        let native = self.has_native(f) && self.switch.native(n);
        let last = self.last.get(f).map_or(2, |l| l.swap(u8::from(native), Ordering::Relaxed));
        if last != 2 && last != u8::from(native) {
            self.switches[sink as usize].fetch_add(1, Ordering::Relaxed);
        }
        native
    }

    /// When a morsel of function `f` starts, when its time counts toward moving it up.
    pub fn start(&self, f: usize) -> Option<Duration> {
        let p = self.progress.get(f).filter(|p| !p.decided.load(Ordering::Relaxed));
        p.map(|_| self.clock.monotonic())
    }

    /// Counts a worker of function `f`.
    pub fn joined(&self, f: usize) {
        if let Some(p) = self.progress.get(f) {
            p.workers.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts a morsel of `rows` rows of function `f` of `module` that started at `start`, and
    /// moves the function up to `clif` when this is the worker to decide and that pays.
    pub fn ran(&self, module: &Module, f: usize, rows: usize, start: Duration) {
        let Some(p) = self.progress.get(f) else { return };
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let reading = self.clock.monotonic();
        let now = nanos(reading.saturating_sub(self.epoch));
        let took = nanos(reading.saturating_sub(start));
        let at = nanos(start.saturating_sub(self.epoch));
        if !p.ran(rows as u64, took, at, now, self.calibration.period) {
            return;
        }
        let Some(&(class, insts)) = self.shape.get(f).and_then(OnceLock::get) else { return };
        let seen = p.seen(now, insts, class);
        let Some(verdict) = up::decide(&seen, &self.calibration) else { return };
        // A compile past the cap waits for the next decision.
        let cap = self.tasks.as_ref().map_or(1, |t| (t.parallelism() / 8).max(1));
        if COMPILING.fetch_add(1, Ordering::Relaxed) >= cap {
            COMPILING.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        if p.decided.swap(true, Ordering::Relaxed) {
            COMPILING.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        self.climb(module, f, &seen, verdict);
    }

    /// Compiles function `f` of `module` on `clif` on a thread of its own, and publishes the code
    /// for the workers' next morsels. One slot in [`COMPILING`] is already taken for it.
    fn climb(&self, module: &Module, f: usize, seen: &Seen, verdict: Verdict) {
        let (Some(func), Some(slot)) = (module.funcs.get(f), self.upper.get(f)) else {
            COMPILING.fetch_sub(1, Ordering::Relaxed);
            return;
        };
        let Some(tasks) = &self.tasks else {
            COMPILING.fetch_sub(1, Ordering::Relaxed);
            return;
        };
        let (func, slot, report) = (func.clone(), Arc::clone(slot), Arc::clone(&self.report));
        let clock = Arc::clone(&self.clock);
        let fresh = self.fresh;
        let rate = seen.rows as f64 / seen.busy as f64 * 1e3;
        let why = format!(
            "{} after {} morsels on {} workers with {} rows left at {rate:.1} rows a µs a worker, {:.3} ms to stay and {:.3} ms to move up with a {:.3} ms compile",
            func.name,
            seen.morsels,
            seen.workers.max(1),
            seen.left,
            verdict.stay / 1e6,
            verdict.up / 1e6,
            verdict.cost / 1e6,
        );
        let job = move || {
            let mut done = Report { tier: Tier::Clif.name(), ..Report::default() };
            let code = native::compile(&func, &mut done, clif::compile, &*clock).map(Arc::new);
            if let Some(code) = &code
                && !fresh
            {
                cache::put(cache::key(Tier::Clif.name(), &func), code);
            }
            let _ = slot.set(code);
            let mut report = report.lock().unwrap_or_else(PoisonError::into_inner);
            report.up += done.native;
            MOVED.fetch_add(done.native as u64, Ordering::Relaxed);
            report.background += done.compile;
            report.fallbacks.append(&mut done.fallbacks);
            report.climbs.push(why);
            COMPILING.fetch_sub(1, Ordering::Relaxed);
        };
        match tasks.spawn("rupg-jit-clif", Box::new(job)) {
            Ok(handle) => self.pending.lock().unwrap_or_else(PoisonError::into_inner).push(handle),
            Err(_) => {
                COMPILING.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Waits for the background compiles started so far.
    #[cfg(test)]
    fn settle(&self) {
        let pending =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in pending {
            let _ = handle.join();
        }
    }

    /// Whether function `f` has machine code.
    fn has_native(&self, f: usize) -> bool {
        self.native.get(f).and_then(OnceLock::get).is_some_and(Option::is_some)
    }

    /// Runs function `f` on a state and a morsel, as machine code when `native` is set and there
    /// is some, and returns its status. As for [`Program::call`], `st` and `m` are the state and
    /// the morsel that the module laid out for `f`.
    pub fn call(&self, native: bool, f: usize, st: *mut u8, m: *const u8, rt: &mut Rt) -> u64 {
        if native {
            // The `clif` code once a function has moved up, which the slot's acquire makes safe to
            // call from the morsel after it was published.
            if let Some(Some(code)) = self.upper.get(f).and_then(|slot| slot.get()) {
                return native::call(code, st, m, rt);
            }
            if let Some(Some(code)) = self.native.get(f).and_then(OnceLock::get) {
                return native::call(code, st, m, rt);
            }
        }
        self.program.call(f, st, m, rt)
    }
}

/// How many background compiles are running in the process, which section 9.5 of the spec caps
/// at one for every eight threads.
static COMPILING: AtomicUsize = AtomicUsize::new(0);

/// How many functions have moved up to `clif` in the process.
static MOVED: AtomicU64 = AtomicU64::new(0);

/// How many functions have moved up to `clif` in the process so far, counted when their compile
/// is done, which may be after their query is.
#[must_use]
pub fn moved_up() -> u64 {
    MOVED.load(Ordering::Relaxed)
}

/// Loading and calling machine code, whichever backend made it.
mod native {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::OnceLock;

    use rupg_platform::Clock;

    use crate::ir::Func;
    use crate::ir::entry::Entry;
    use crate::rt::code::{Code, CodeArena, Reloc, RelocKind};
    use crate::rt::native::{Ctx, address};
    use crate::rt::{Rt, abi};

    use super::Report;

    /// A backend's output: the bytes, and where each runtime entry's address goes with its
    /// addend.
    pub(super) struct Lowered {
        pub(super) bytes: Vec<u8>,
        pub(super) relocs: Vec<(u32, Entry, i64)>,
    }

    /// The arena, made once per process, or why it could not be.
    fn arena() -> Result<&'static CodeArena, &'static str> {
        static ARENA: OnceLock<Result<CodeArena, String>> = OnceLock::new();
        ARENA
            .get_or_init(|| CodeArena::new().map_err(|e| format!("no code arena: {e}")))
            .as_ref()
            .map_err(String::as_str)
    }

    /// Compiles `f` with `lower` and loads it, and says in `report` what it did.
    pub(super) fn compile(
        f: &Func,
        report: &mut Report,
        lower: fn(&Func) -> Result<Lowered, String>,
        clock: &dyn Clock,
    ) -> Option<Code> {
        let start = clock.monotonic();
        let arena = match arena() {
            Ok(a) => a,
            Err(why) => {
                report.fallbacks.push(why.to_string());
                return None;
            }
        };
        // A backend asserts what it believes about its input, and an assertion here is a function
        // it does not handle, not a reason to fail the query.
        let compiled = catch_unwind(AssertUnwindSafe(|| lower(f)))
            .unwrap_or_else(|_| {
                Err(format!("{}: {}: the code generator panicked", report.tier, f.name))
            })
            .and_then(|code| {
                let relocs: Vec<Reloc> = code
                    .relocs
                    .iter()
                    .map(|&(offset, entry, addend)| Reloc {
                        offset,
                        kind: RelocKind::Abs8,
                        target: address(entry),
                        addend,
                    })
                    .collect();
                arena
                    .load(&code.bytes, &relocs)
                    .map_err(|e| format!("{}: {}: loading: {e}", report.tier, f.name))
            });
        report.compile += clock.monotonic().saturating_sub(start);
        match compiled {
            Ok(code) => {
                report.native += 1;
                report.bytes += code.len();
                Some(code)
            }
            Err(why) => {
                report.fallbacks.push(why);
                None
            }
        }
    }

    /// Calls machine code with the runtime reachable through the state header.
    pub(super) fn call(code: &Code, st: *mut u8, m: *const u8, rt: &mut Rt) -> u64 {
        let mut ctx = Ctx::new(rt);
        let word = ctx.word();
        let at = st.wrapping_add(abi::RT as usize).cast::<u64>();
        // SAFETY: `st` is a state block, which starts with the 64 byte header, and `rt` is the
        // pointer sized word at `abi::RT` in it. The word is the address of `ctx`, which lives
        // until after the call, and it is cleared again before `ctx` goes.
        unsafe { at.write(word) };
        // SAFETY: the code is a pipeline function the backend compiled from the module the state
        // and the morsel were laid out for, and the entries it calls find `ctx` through the word
        // just written.
        let status = unsafe { code.call(st, m) };
        // SAFETY: as for the write above.
        unsafe { at.write(0) };
        status
    }
}

#[cfg(feature = "jit")]
mod clif {
    use std::sync::OnceLock;

    use crate::clif::Backend;
    use crate::ir::Func;

    use super::native::Lowered;

    /// Compiles one function through Cranelift for this machine.
    pub(super) fn compile(f: &Func) -> Result<Lowered, String> {
        static BACKEND: OnceLock<Result<Backend, String>> = OnceLock::new();
        let backend = BACKEND
            .get_or_init(|| Backend::host().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(Clone::clone)?;
        let code = backend.compile(f).map_err(|e| e.to_string())?;
        let relocs = code.relocs.iter().map(|r| (r.offset, r.entry, r.addend)).collect();
        Ok(Lowered { bytes: code.bytes, relocs })
    }
}

#[cfg(target_arch = "x86_64")]
mod direct {
    use std::sync::OnceLock;

    use crate::direct::Backend;
    use crate::ir::Func;

    use super::native::Lowered;

    /// Compiles one function with the single pass backend, using what this processor has.
    pub(super) fn compile(f: &Func) -> Result<Lowered, String> {
        static BACKEND: OnceLock<Result<Backend, String>> = OnceLock::new();
        let backend = BACKEND
            .get_or_init(|| Backend::host().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(Clone::clone)?;
        let code = backend.compile(f).map_err(|e| e.to_string())?;
        let relocs = code.relocs.iter().map(|r| (r.offset, r.entry, r.addend)).collect();
        Ok(Lowered { bytes: code.bytes, relocs })
    }
}

/// A build without Cranelift refuses every function, and `SET qc_tier` refuses the tier first.
#[cfg(not(feature = "jit"))]
mod clif {
    use crate::ir::Func;

    use super::native::Lowered;

    pub(super) fn compile(f: &Func) -> Result<Lowered, String> {
        Err(format!("{}: this build has no clif tier", f.name))
    }
}

/// Not on x86-64, `direct` refuses every function, and `SET qc_tier` refuses the tier first.
#[cfg(not(target_arch = "x86_64"))]
mod direct {
    use crate::ir::Func;

    use super::native::Lowered;

    pub(super) fn compile(f: &Func) -> Result<Lowered, String> {
        Err(format!("{}: this build has no direct tier", f.name))
    }
}

#[cfg(test)]
mod tests;

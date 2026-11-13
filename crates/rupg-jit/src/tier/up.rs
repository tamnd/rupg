// Lifted from rudb crates/rudb-qc/src/tier/up.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so QIR is `crate::ir` and the runtime is `crate::rt`.

//! Tier-up, per sections 9.4 and 9.5 of `spec/compiler/09-tiering-and-caching.md`.
//!
//! A pipeline that `auto` compiled on `direct` counts the rows it has done and the time its
//! workers spent on them. At a morsel boundary, at most once per [`Calibration::period`], the
//! worker that just finished a morsel weighs staying on `direct` against compiling on `clif`, with
//! Kohn's extrapolation over the rows the pipeline has left, which are known exactly because the
//! size of every input is. When `clif` wins by at least its own compile time, the function is
//! compiled on a thread of its own while the workers go on, and each worker picks the new code up
//! at its next morsel. A pipeline tiers up at most once, and a compile that fails leaves it where
//! it is.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::ir::{Block, Func, Op};

/// What a pipeline does, which is what its speedup on `clif` is calibrated by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// A scan and a filter into an aggregate or rows out, with nothing below of the other kinds.
    Aggregate,
    /// A pipeline that probes a join table.
    Probe,
    /// A pipeline that reads strings.
    Strings,
    /// A pipeline with at least eight float or decimal operations.
    Arithmetic,
}

impl Class {
    /// The class of `func`, which probes a join table when `probes` is set.
    pub(crate) fn of(func: &Func, probes: bool) -> Class {
        if probes {
            return Class::Probe;
        }
        let (mut strings, mut arithmetic) = (0usize, 0usize);
        for b in 0..func.blocks.len() {
            let Ok(b) = u32::try_from(b) else { break };
            for inst in func.insts(Block(b)) {
                match inst.op {
                    Op::StrLen
                    | Op::StrW0
                    | Op::StrW1
                    | Op::StrPtr
                    | Op::StrInl
                    | Op::StrMk
                    | Op::LoadStr
                    | Op::StoreStr
                    | Op::Memeq => strings += 1,
                    Op::Fadd
                    | Op::Fsub
                    | Op::Fmul
                    | Op::Fdiv
                    | Op::SmulT
                    | Op::SdivT
                    | Op::Smulw
                    | Op::DupT
                    | Op::Ddown => arithmetic += 1,
                    _ => {}
                }
            }
        }
        if arithmetic >= 8 {
            Class::Arithmetic
        } else if strings > 0 {
            Class::Strings
        } else {
            Class::Aggregate
        }
    }
}

/// What tier-up knows about the machine: what a `clif` compile costs and what it buys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Calibration {
    /// The nanoseconds a `clif` compile costs whatever the function's size.
    pub(crate) fixed: f64,
    /// The nanoseconds a `clif` compile costs per QIR instruction.
    pub(crate) per_inst: f64,
    /// The largest function worth compiling on `clif`.
    pub(crate) max_insts: usize,
    /// How much faster `clif` code runs than `direct` code, by [`Class`].
    pub(crate) speedup: [f64; 4],
    /// How many times its own compile time tier-up has to save.
    pub(crate) margin: f64,
    /// The nanoseconds of pipeline wall time before the first decision.
    pub(crate) first_after: u64,
    /// The nanoseconds between two decisions for one pipeline.
    pub(crate) period: u64,
}

impl Calibration {
    /// The numbers for this machine.
    ///
    /// The compile cost comes from `cargo xtask compiled --tier clif` over ClickBench at commit
    /// 40699f23 on gpc (i9-13900K): 0.25 ms for 23 instructions up to 1.17 ms for 122, so about
    /// 9.5 µs an instruction. The speedups come from the same run on 10M rows, `clif` against
    /// `direct`. Strings gained 1.14 to 1.25 on q21, q23 and q24 at one thread, and 1.14 is kept.
    /// Aggregates went from 1.23 on q5 to 0.95 on q19 at one thread and lost 5 to 9% on q9 and q33
    /// at sixteen, so they get 1.0 and never move up. Probes and arithmetic keep the CGO 2024
    /// average of 1.045 until TPC-H calibrates them. The margin of 1.0 and the first decision
    /// after 1 ms are from table 9.9 of the spec, and a decision every 250 µs is ours. On AArch64
    /// there is no `direct` to tier up from.
    pub(crate) const HOST: Calibration = Calibration {
        fixed: 20_000.0,
        per_inst: 9_500.0,
        max_insts: 20_000,
        speedup: [1.0, 1.045, 1.14, 1.045],
        margin: 1.0,
        first_after: 1_000_000,
        period: 250_000,
    };
}

/// What a pipeline has seen so far, which is what a decision reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seen {
    /// The nanoseconds since its first morsel started.
    pub(crate) elapsed: u64,
    /// The morsels done.
    pub(crate) morsels: u64,
    /// The workers running it.
    pub(crate) workers: u64,
    /// The rows done.
    pub(crate) rows: u64,
    /// The rows left.
    pub(crate) left: u64,
    /// The nanoseconds its workers spent on the rows done, added up over the workers.
    pub(crate) busy: u64,
    /// The QIR instructions of its function.
    pub(crate) insts: usize,
    /// Its class.
    pub(crate) class: Class,
}

/// Why a pipeline tiers up: the nanoseconds it has left on `direct`, on `clif` counting the
/// compile, and the compile alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) stay: f64,
    pub(crate) up: f64,
    pub(crate) cost: f64,
}

/// Kohn's extrapolation, `t1 = c1 + max(n - (w - 1) r0 c1, 0) / r1 / w` from ICDE 2018, against
/// `t0 = n / r0 / w`: a verdict when compiling on `clif` saves at least the margin times its own
/// cost, and none when it does not or it is too early to say.
pub(crate) fn decide(seen: &Seen, c: &Calibration) -> Option<Verdict> {
    let workers = seen.workers.max(1);
    if seen.elapsed < c.first_after || seen.morsels < workers || seen.rows == 0 || seen.busy == 0 {
        return None;
    }
    if seen.insts > c.max_insts {
        return None;
    }
    let n = seen.left as f64;
    let w = workers as f64;
    let r0 = seen.rows as f64 / seen.busy as f64;
    let r1 = r0 * c.speedup[seen.class as usize];
    let stay = n / r0 / w;
    let cost = c.fixed + c.per_inst * seen.insts as f64;
    let meanwhile = (w - 1.0) * r0 * cost;
    let up = cost + (n - meanwhile).max(0.0) / r1 / w;
    (stay - up >= cost * c.margin).then_some(Verdict { stay, up, cost })
}

/// A pipeline's counters, which its workers add to after every morsel.
#[derive(Debug)]
pub(crate) struct Progress {
    /// The rows its input has, `u64::MAX` when that is not known, which never tiers up.
    pub(crate) total: AtomicU64,
    pub(crate) rows: AtomicU64,
    pub(crate) busy: AtomicU64,
    pub(crate) morsels: AtomicU64,
    pub(crate) workers: AtomicU64,
    /// When its first morsel started, in nanoseconds from when the tiers were made, plus one so
    /// that zero is not yet.
    pub(crate) first: AtomicU64,
    /// When the next decision may be taken, on the same clock.
    pub(crate) next: AtomicU64,
    /// Whether it has tiered up or given up, which is once.
    pub(crate) decided: AtomicBool,
}

impl Default for Progress {
    fn default() -> Progress {
        Progress {
            total: AtomicU64::new(u64::MAX),
            rows: AtomicU64::new(0),
            busy: AtomicU64::new(0),
            morsels: AtomicU64::new(0),
            workers: AtomicU64::new(0),
            first: AtomicU64::new(0),
            next: AtomicU64::new(0),
            decided: AtomicBool::new(false),
        }
    }
}

impl Progress {
    /// Counts a morsel of `rows` rows that took `took` nanoseconds and started at `start` on the
    /// tiers' clock, and says whether it is this worker's turn to decide, which it is at most once
    /// a period.
    pub(crate) fn ran(&self, rows: u64, took: u64, start: u64, now: u64, period: u64) -> bool {
        let _ = self.first.compare_exchange(0, start + 1, Ordering::Relaxed, Ordering::Relaxed);
        self.rows.fetch_add(rows, Ordering::Relaxed);
        self.busy.fetch_add(took, Ordering::Relaxed);
        self.morsels.fetch_add(1, Ordering::Relaxed);
        if self.decided.load(Ordering::Relaxed) || self.total.load(Ordering::Relaxed) == u64::MAX {
            return false;
        }
        let next = self.next.load(Ordering::Relaxed);
        now >= next
            && self
                .next
                .compare_exchange(next, now + period, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    /// What the pipeline has seen, as of `now`.
    pub(crate) fn seen(&self, now: u64, insts: usize, class: Class) -> Seen {
        let rows = self.rows.load(Ordering::Relaxed);
        let first = self.first.load(Ordering::Relaxed);
        Seen {
            elapsed: now.saturating_sub(first.saturating_sub(1)),
            morsels: self.morsels.load(Ordering::Relaxed),
            workers: self.workers.load(Ordering::Relaxed),
            rows,
            left: self.total.load(Ordering::Relaxed).saturating_sub(rows),
            busy: self.busy.load(Ordering::Relaxed),
            insts,
            class,
        }
    }
}

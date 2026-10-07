//! The commit timestamps of the node and the `visible` word (spec/11 section 11.3).
//!
//! A commit takes its timestamp with [`Commits::begin`] and calls [`Commits::installed`] after it writes the timestamp into its rows. The `visible` word is the highest timestamp at or below which every commit is installed. A snapshot reads it with one load. A commit that has a timestamp and is not installed keeps `visible` below its timestamp, so a snapshot never misses a commit with a lower timestamp. This is the ordered commit of Alhomssi and Leis (PVLDB 16, 2023).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Hlc, Result};

use crate::clock::HlcClock;

/// The commits of the node that have a timestamp, and the `visible` word.
#[derive(Debug)]
pub struct Commits {
    clock: HlcClock,
    pending: Mutex<BTreeSet<u64>>,
    visible: AtomicU64,
}

impl Commits {
    /// The commits of a node that uses `clock`. Every value that the clock gave before is visible.
    pub fn new(clock: HlcClock) -> Commits {
        let visible = AtomicU64::new(clock.last().bits());
        Commits { clock, pending: Mutex::new(BTreeSet::new()), visible }
    }

    /// The clock. Commit timestamps must come from [`Commits::begin`] and not from the clock.
    pub fn clock(&self) -> &HlcClock {
        &self.clock
    }

    /// A snapshot: every commit at or below it is installed.
    pub fn visible(&self) -> Hlc {
        Hlc::from_bits(self.visible.load(Ordering::Acquire))
    }

    fn lock(&self) -> MutexGuard<'_, BTreeSet<u64>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes a commit timestamp. `visible` stays below it until [`Commits::installed`] or [`Commits::abandon`].
    pub fn begin(&self) -> Result<Hlc> {
        // The lock makes the order of the timestamps the order in which they enter the set, so the lowest pending timestamp only goes up.
        let mut pending = self.lock();
        let ts = self.clock.next()?;
        pending.insert(ts.bits());
        Ok(ts)
    }

    /// Records that the commit at `ts` wrote its timestamp into each of its rows, and moves `visible`. A timestamp that is not pending gives SQLSTATE `XX000`.
    pub fn installed(&self, ts: Hlc) -> Result<()> {
        self.end(ts)
    }

    /// Records that the commit at `ts` stopped before it was installed, for example because its log block could not be written. No row has the timestamp.
    pub fn abandon(&self, ts: Hlc) -> Result<()> {
        self.end(ts)
    }

    fn end(&self, ts: Hlc) -> Result<()> {
        let mut pending = self.lock();
        if !pending.remove(&ts.bits()) {
            return Err(Error::internal(format!("the commit timestamp {ts} is not pending")));
        }
        let visible = match pending.first() {
            Some(&lowest) => lowest - 1,
            None => self.clock.last().bits(),
        };
        // The release order makes the rows of each installed commit visible to a reader that loads the new value.
        self.visible.fetch_max(visible, Ordering::AcqRel);
        Ok(())
    }

    /// The number of commits that have a timestamp and are not installed.
    pub fn pending(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::{SimClock, SimRng};
    use rupg_platform::{TaskHandle, Tasks};

    use super::*;

    fn commits(seed: u64) -> (Arc<SimClock>, Commits) {
        let wall = Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000 + seed));
        (wall.clone(), Commits::new(HlcClock::new(wall, Hlc::ZERO)))
    }

    #[test]
    fn visible_waits_for_the_lowest_commit() {
        let (_, c) = commits(0);
        let start = c.visible();
        let a = c.begin().unwrap();
        let b = c.begin().unwrap();
        let d = c.begin().unwrap();
        assert!(start < a && a < b && b < d);
        c.installed(b).unwrap();
        c.installed(d).unwrap();
        assert_eq!(c.visible(), Hlc::from_bits(a.bits() - 1));
        c.installed(a).unwrap();
        assert_eq!((c.visible(), c.pending()), (d, 0));

        let e = c.begin().unwrap();
        let f = c.begin().unwrap();
        c.abandon(e).unwrap();
        assert_eq!(c.visible(), Hlc::from_bits(f.bits() - 1));
        assert!(c.installed(e).is_err());
        c.installed(f).unwrap();
        assert_eq!(c.visible(), f);

        // A value that the clock saw is visible when no commit is pending, because no later commit can be below it.
        let seen = Hlc::new(f.physical() + 50, 0).unwrap();
        c.clock().observe(seen);
        let g = c.begin().unwrap();
        assert!(g > seen);
        c.installed(g).unwrap();
        assert_eq!(c.visible(), g);
    }

    /// Random commits in a random order. `visible` never passes a pending commit and never goes back.
    #[test]
    fn a_model_of_random_commits() {
        for seed in 0..20 {
            let (wall, c) = commits(seed);
            let mut rng = SimRng::new(seed);
            let mut pending: Vec<Hlc> = Vec::new();
            let mut last = c.visible();
            for _ in 0..2_000 {
                match rng.below(5) {
                    0 | 1 => pending.push(c.begin().unwrap()),
                    2 => wall.jump_wall(rng.below(5) as i64 - 2),
                    _ if !pending.is_empty() => {
                        let ts = pending.swap_remove(rng.below(pending.len() as u64) as usize);
                        if rng.below(4) == 0 { c.abandon(ts) } else { c.installed(ts) }.unwrap();
                    }
                    _ => {}
                }
                let v = c.visible();
                assert!(v >= last);
                assert!(pending.iter().all(|&p| p > v), "seed {seed}");
                if pending.is_empty() {
                    assert_eq!(v, c.clock().last());
                }
                last = v;
            }
        }
    }

    /// Threads commit while a reader takes snapshots. Every commit at or below a snapshot was installed before the reader took it.
    #[test]
    fn a_snapshot_misses_no_commit() {
        const THREADS: u64 = 6;
        const COMMITS: usize = 3_000;
        let (_, c) = commits(1);
        let c = Arc::new(c);
        let done = Arc::new(Mutex::new(BTreeSet::new()));
        let handles: Vec<TaskHandle> = (0..THREADS)
            .map(|t| {
                let (c, done) = (c.clone(), done.clone());
                let task = Box::new(move || {
                    for _ in 0..COMMITS {
                        let ts = c.begin().unwrap();
                        done.lock().unwrap().insert(ts);
                        c.installed(ts).unwrap();
                    }
                });
                OsTasks.spawn(&format!("commit {t}"), task).unwrap()
            })
            .collect();
        let mut seen = Vec::new();
        for _ in 0..2_000 {
            let v = c.visible();
            let installed = done.lock().unwrap().iter().filter(|&&ts| ts <= v).count();
            seen.push((v, installed));
        }
        for h in handles {
            h.join().unwrap();
        }
        let all = done.lock().unwrap().clone();
        assert_eq!(all.len(), THREADS as usize * COMMITS);
        for (v, installed) in seen {
            assert_eq!(all.iter().filter(|&&ts| ts <= v).count(), installed, "snapshot {v}");
        }
        assert_eq!(c.visible(), c.clock().last());
    }
}

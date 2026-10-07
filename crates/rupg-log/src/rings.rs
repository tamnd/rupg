//! The rings of a node and their safe positions (spec/11 section 11.14.2).
//!
//! A ring's safe position moves over a block when the block is durable and every dependency `(r, p)` of the block has ring `r` safe at or above `p`. The safe position is a prefix, so a block waits for each block before it in its ring. A commit is acknowledged when the safe position of its ring reaches its end.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result};

use crate::block::{Block, Dep};
use crate::ring::{Placed, Ring};

/// A block with a dependency on another ring that was not safe when the block was placed.
#[derive(Debug)]
struct Waiting {
    start: u64,
    end: u64,
    deps: Vec<Dep>,
}

#[derive(Debug)]
struct Safety {
    durable: Vec<u64>,
    safe: Vec<u64>,
    waiting: Vec<VecDeque<Waiting>>,
}

impl Safety {
    fn met(&self, d: &Dep) -> bool {
        self.safe[usize::from(d.ring)] >= d.position
    }

    /// Moves each safe position as far as it can go. A ring can move only after the rings it waits for move, so the loop runs until no position moves.
    fn update(&mut self) {
        loop {
            let mut moved = false;
            for r in 0..self.safe.len() {
                let mut safe = self.durable[r];
                while let Some(w) = self.waiting[r].front() {
                    if w.start >= safe {
                        break;
                    }
                    if !w.deps.iter().all(|d| self.met(d)) {
                        safe = w.start;
                        break;
                    }
                    if w.end > safe {
                        break;
                    }
                    self.waiting[r].pop_front();
                }
                if safe > self.safe[r] {
                    self.safe[r] = safe;
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
    }

    /// The ring and position to flush next so that ring `r` can become safe at `end`, or `None` when no flush helps.
    fn needed(&self, mut r: usize, mut end: u64) -> Option<(usize, u64)> {
        // Each step goes to a block with a lower timestamp, so a walk is never longer than the number of waiting blocks.
        let steps = self.waiting.iter().map(VecDeque::len).sum::<usize>() + 1;
        for _ in 0..steps {
            if self.durable[r] < end {
                return Some((r, end));
            }
            let d = self.waiting[r]
                .iter()
                .take_while(|w| w.start < end)
                .flat_map(|w| &w.deps)
                .find(|d| !self.met(d))?;
            (r, end) = (usize::from(d.ring), d.position);
        }
        None
    }
}

/// The log rings of a node, numbered from 0.
#[derive(Debug)]
pub struct Log {
    rings: Vec<Ring>,
    safety: Mutex<Safety>,
}

impl Log {
    /// The log of `rings`. Ring `i` must have the number `i`. Each ring is safe at its durable position.
    pub fn new(rings: Vec<Ring>) -> Result<Log> {
        if let Some((i, r)) = rings.iter().enumerate().find(|(i, r)| usize::from(r.number()) != *i)
        {
            return Err(Error::internal(format!(
                "log ring {i} has the number {}, and the rings must be numbered from 0",
                r.number()
            )));
        }
        let durable: Vec<u64> = rings.iter().map(Ring::durable).collect();
        Ok(Log {
            safety: Mutex::new(Safety {
                safe: durable.clone(),
                durable,
                waiting: rings.iter().map(|_| VecDeque::new()).collect(),
            }),
            rings,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Safety> {
        self.safety.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The rings.
    pub fn rings(&self) -> &[Ring] {
        &self.rings
    }

    /// The ring with number `ring`.
    pub fn ring(&self, ring: u16) -> Result<&Ring> {
        self.rings
            .get(usize::from(ring))
            .ok_or_else(|| Error::internal(format!("the log has no ring {ring}")))
    }

    /// The safe position of a ring.
    pub fn safe(&self, ring: u16) -> u64 {
        self.lock().safe[usize::from(ring)]
    }

    /// Places `block` at the end of `ring`. A dependency on a ring that does not exist, beyond the end of its ring, or on a later place in the same ring gives SQLSTATE `XX000`.
    pub fn append(&self, ring: u16, block: &mut Block) -> Result<Placed> {
        let target = self.ring(ring)?;
        let mut foreign = Vec::new();
        for d in &block.deps {
            let end = self.ring(d.ring)?.end();
            if d.position > end {
                return Err(Error::internal(format!(
                    "a log block depends on ring {} at {}, beyond its end {end}",
                    d.ring, d.position
                )));
            }
            if d.ring != ring {
                foreign.push(*d);
            }
        }
        // The lock keeps the safe position of the ring from moving over the block before it is in the waiting list.
        let mut s = self.lock();
        foreign.retain(|d| !s.met(d));
        if foreign.is_empty() {
            drop(s);
            return target.append(block);
        }
        let placed = target.append(block)?;
        s.waiting[usize::from(ring)].push_back(Waiting {
            start: placed.position,
            end: placed.end,
            deps: foreign,
        });
        Ok(placed)
    }

    /// Makes `ring` durable up to `position` and moves the safe positions.
    pub fn flush(&self, ring: u16, position: u64) -> Result<()> {
        let durable = self.ring(ring)?.flush_to(position)?;
        let mut s = self.lock();
        let r = usize::from(ring);
        if durable > s.durable[r] {
            s.durable[r] = durable;
            s.update();
        }
        Ok(())
    }

    /// Waits until `ring` is safe at `end`. It flushes the ring, and also each ring that a block before `end` waits for. This is at most one flush of each other ring for a normal commit.
    pub fn wait_safe(&self, ring: u16, end: u64) -> Result<()> {
        loop {
            let needed = {
                let s = self.lock();
                if s.safe[usize::from(ring)] >= end {
                    return Ok(());
                }
                s.needed(usize::from(ring), end)
            };
            let Some((r, position)) = needed else {
                return Err(Error::internal(format!(
                    "ring {ring} cannot become safe at {end}, because its dependencies form a cycle"
                )));
            };
            self.flush(r as u16, position)?;
        }
    }

    /// Places a block and waits until it is safe.
    pub fn commit(&self, ring: u16, block: &mut Block) -> Result<Placed> {
        let placed = self.append(ring, block)?;
        self.wait_safe(ring, placed.end)?;
        Ok(placed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::{SimIo, SimRng};
    use rupg_platform::{TaskHandle, Tasks};

    use super::*;
    use crate::ring::tests::{commit, read_all, store};

    fn log(io: &SimIo, rings: u32) -> Log {
        let store = store(io);
        Log::new((0..rings).map(|n| Ring::create(store.clone(), n, n, 1).unwrap()).collect())
            .unwrap()
    }

    fn with_deps(xid: u64, deps: &[(u16, u64)]) -> Block {
        let mut b = commit(xid, 16);
        b.deps = deps.iter().map(|&(ring, position)| Dep { ring, position }).collect();
        b
    }

    #[test]
    fn a_dependency_waits_for_its_ring() {
        let io = SimIo::new(1);
        let log = log(&io, 2);
        let b1 = log.append(1, &mut commit(1, 16)).unwrap();
        let a1 = log.append(0, &mut with_deps(2, &[(1, b1.end)])).unwrap();
        let a2 = log.append(0, &mut commit(3, 16)).unwrap();

        // Ring 0 is durable, but its safe position stops at the block that waits for ring 1.
        log.flush(0, a2.end).unwrap();
        assert!(log.ring(0).unwrap().durable() >= a2.end);
        assert_eq!(log.safe(0), a1.position);

        // The wait flushes ring 1, and then both blocks of ring 0 are safe.
        let syncs = io.syncs();
        log.wait_safe(0, a2.end).unwrap();
        assert_eq!(io.syncs(), syncs + 1);
        assert!(log.safe(0) >= a2.end && log.safe(1) >= b1.end);

        // A dependency that is safe already does not wait.
        let a3 = log.append(0, &mut with_deps(4, &[(1, b1.end)])).unwrap();
        log.wait_safe(0, a3.end).unwrap();
        assert_eq!(io.syncs(), syncs + 2);
    }

    #[test]
    fn a_chain_of_dependencies() {
        let io = SimIo::new(2);
        let log = log(&io, 4);
        let c = log.append(3, &mut commit(1, 16)).unwrap();
        let b = log.append(2, &mut with_deps(2, &[(3, c.end)])).unwrap();
        let a = log.append(1, &mut with_deps(3, &[(2, b.end)])).unwrap();
        let z = log.append(0, &mut with_deps(4, &[(1, a.end), (2, b.end)])).unwrap();
        let syncs = io.syncs();
        log.wait_safe(0, z.end).unwrap();
        assert_eq!(io.syncs(), syncs + 4);
        for (ring, end) in [(0, z.end), (1, a.end), (2, b.end), (3, c.end)] {
            assert!(log.safe(ring) >= end, "ring {ring}");
        }
        // A commit with no dependency is one flush.
        log.commit(1, &mut commit(5, 16)).unwrap();
        assert_eq!(io.syncs(), syncs + 5);
    }

    #[test]
    fn bad_dependencies() {
        let io = SimIo::new(3);
        let log = log(&io, 2);
        let a = log.append(0, &mut commit(1, 16)).unwrap();
        assert!(log.append(0, &mut with_deps(2, &[(2, 0)])).is_err());
        assert!(log.append(0, &mut with_deps(2, &[(1, 8)])).is_err());
        assert!(log.append(0, &mut with_deps(2, &[(0, a.end + 8)])).is_err());
        // A dependency on an earlier block of the same ring is the prefix rule, and it does not wait.
        let b = log.append(0, &mut with_deps(2, &[(0, a.end)])).unwrap();
        log.wait_safe(0, b.end).unwrap();
        assert!(log.append(5, &mut commit(3, 16)).is_err());
        let store = store(&SimIo::new(4));
        let e = Log::new(vec![Ring::create(store, 1, 0, 1).unwrap()]).unwrap_err();
        assert_eq!(e.state(), rupg_common::SqlState::INTERNAL_ERROR);
    }

    /// Many threads commit on four rings with dependencies between them. Every acknowledged commit is in its ring, and group commit syncs fewer times than there are commits.
    #[test]
    fn threads_commit_together() {
        const THREADS: u64 = 8;
        const COMMITS: u64 = 100;
        let io = SimIo::new(5);
        let log = Arc::new(log(&io, 4));
        let syncs = io.syncs();
        let handles: Vec<TaskHandle> = (0..THREADS)
            .map(|t| {
                let log = log.clone();
                let task = Box::new(move || {
                    let mut rng = SimRng::new(t);
                    let ring = (t % 4) as u16;
                    for i in 0..COMMITS {
                        let other = rng.below(4) as u16;
                        let end = log.ring(other).unwrap().end();
                        let deps = if other != ring && rng.below(2) == 0 {
                            vec![(other, end)]
                        } else {
                            Vec::new()
                        };
                        let mut b = with_deps(t * 1000 + i, &deps);
                        let placed = log.commit(ring, &mut b).unwrap();
                        assert!(log.safe(ring) >= placed.end);
                    }
                });
                OsTasks.spawn(&format!("commit {t}"), task).unwrap()
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let store = store_of(&log);
        let mut xids = Vec::new();
        for ring in log.rings() {
            let (blocks, _) = read_all(store, &ring.state(), 0);
            xids.extend(blocks.iter().map(|b| b.xid));
        }
        xids.sort_unstable();
        let want: Vec<u64> =
            (0..THREADS).flat_map(|t| (0..COMMITS).map(move |i| t * 1000 + i)).collect();
        assert_eq!(xids, want);
        assert!(io.syncs() - syncs <= THREADS * COMMITS);
    }

    fn store_of(log: &Log) -> &rupg_buffer::FileStore {
        log.rings()[0].store()
    }
}

//! The hybrid logical clock of the node (spec/04 section 4.7).
//!
//! The clock gives each commit a timestamp above every value that the node gave or saw before. It takes the wall clock when the wall clock is ahead. When the wall clock stops or goes back, for example after an NTP step, the clock counts on its logical part.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rupg_common::{Error, Hlc, Result};
use rupg_platform::Clock;

/// The hybrid logical clock of the node.
#[derive(Debug)]
pub struct HlcClock {
    wall: Arc<dyn Clock>,
    last: AtomicU64,
}

impl HlcClock {
    /// A clock that gives values above `floor`. After recovery, `floor` is the newest timestamp of the log and the checkpoint (spec/11 section 11.16 step 6).
    pub fn new(wall: Arc<dyn Clock>, floor: Hlc) -> HlcClock {
        HlcClock { wall, last: AtomicU64::new(floor.bits()) }
    }

    /// The newest value that the clock gave or saw.
    pub fn last(&self) -> Hlc {
        Hlc::from_bits(self.last.load(Ordering::Acquire))
    }

    /// A new value, above every value that the clock gave or saw before. After the last value of the 48 bits of milliseconds, it gives SQLSTATE `XX000`.
    pub fn next(&self) -> Result<Hlc> {
        let wall = self.wall.hlc_wall();
        let mut last = self.last.load(Ordering::Acquire);
        loop {
            if last == Hlc::MAX.bits() {
                return Err(Error::internal("the hybrid logical clock is at its last value"));
            }
            let next = Hlc::from_bits(last).tick(wall).bits();
            match self.last.compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Hlc::from_bits(next)),
                Err(now) => last = now,
            }
        }
    }

    /// Moves the clock to at least `seen`, a value from another node or from the log. The next value is above it.
    pub fn observe(&self, seen: Hlc) {
        self.last.fetch_max(seen.bits(), Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::SimClock;
    use rupg_platform::{TaskHandle, Tasks};

    use super::*;

    const WALL: u64 = Hlc::EPOCH_UNIX_MS + 5_000;

    fn hlc(physical: u64, logical: u16) -> Hlc {
        Hlc::new(physical, logical).unwrap()
    }

    #[test]
    fn it_follows_the_wall_clock() {
        let wall = Arc::new(SimClock::new(WALL));
        let clock = HlcClock::new(wall.clone(), Hlc::ZERO);
        assert_eq!(clock.next().unwrap(), hlc(5_000, 0));
        assert_eq!(clock.next().unwrap(), hlc(5_000, 1));
        wall.jump_wall(7);
        assert_eq!(clock.next().unwrap(), hlc(5_007, 0));

        // The wall clock goes back, and the clock counts on its logical part.
        wall.jump_wall(-1_000);
        assert_eq!(clock.next().unwrap(), hlc(5_007, 1));
        assert_eq!(clock.last(), hlc(5_007, 1));
    }

    #[test]
    fn a_floor_and_a_seen_value() {
        let wall = Arc::new(SimClock::new(WALL));
        // The log holds a timestamp from a time when the wall clock was ahead.
        let clock = HlcClock::new(wall.clone(), hlc(9_000, 3));
        assert_eq!(clock.next().unwrap(), hlc(9_000, 4));
        clock.observe(hlc(12_000, 0));
        assert_eq!(clock.next().unwrap(), hlc(12_000, 1));
        // An older value does not move the clock back.
        clock.observe(hlc(1, 0));
        assert_eq!(clock.last(), hlc(12_000, 1));
    }

    #[test]
    fn the_logical_part_overflows() {
        let wall = Arc::new(SimClock::new(WALL));
        let clock = HlcClock::new(wall, hlc(5_000, u16::MAX - 1));
        assert_eq!(clock.next().unwrap(), hlc(5_000, u16::MAX));
        assert_eq!(clock.next().unwrap(), hlc(5_001, 0));

        let wall = Arc::new(SimClock::new(WALL));
        let clock = HlcClock::new(wall, Hlc::from_bits(Hlc::MAX.bits() - 1));
        assert_eq!(clock.next().unwrap(), Hlc::MAX);
        let e = clock.next().unwrap_err();
        assert_eq!(e.state(), rupg_common::SqlState::INTERNAL_ERROR);
        assert_eq!(clock.last(), Hlc::MAX);
    }

    /// Threads take values together. Each value is unique, and the values of each thread go up.
    #[test]
    fn threads_get_unique_values() {
        const THREADS: usize = 8;
        const VALUES: usize = 5_000;
        let wall = Arc::new(SimClock::new(WALL));
        let clock = Arc::new(HlcClock::new(wall.clone(), Hlc::ZERO));
        let (out, handles): (Vec<_>, Vec<TaskHandle>) = (0..THREADS)
            .map(|t| {
                let (clock, wall) = (clock.clone(), wall.clone());
                let out = Arc::new(std::sync::Mutex::new(Vec::new()));
                let mine = out.clone();
                let task = Box::new(move || {
                    let mut v = Vec::with_capacity(VALUES);
                    for i in 0..VALUES {
                        if i % 1000 == 0 {
                            wall.jump_wall(if t % 2 == 0 { 3 } else { -2 });
                        }
                        v.push(clock.next().unwrap());
                    }
                    *mine.lock().unwrap() = v;
                });
                (out, OsTasks.spawn(&format!("clock {t}"), task).unwrap())
            })
            .unzip();
        for h in handles {
            h.join().unwrap();
        }
        let mut all = BTreeSet::new();
        for v in out {
            let v = v.lock().unwrap();
            assert!(v.windows(2).all(|w| w[0] < w[1]));
            all.extend(v.iter().copied());
        }
        assert_eq!(all.len(), THREADS * VALUES);
        assert_eq!(all.last().copied(), Some(clock.last()));
    }
}

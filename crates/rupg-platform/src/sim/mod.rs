//! The simulation implementations of the traits (spec/21 section 21.10).
//!
//! Each one is deterministic. The same seed and the same calls give the same results, bit for bit. A test picks the seed and prints it when it fails, so that the failure can be run again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::{Clock, Entropy};

mod disk;
mod net;
mod tasks;

pub use disk::{CrashPlan, Fate, Faults, SimFile, SimIo, Unsynced};
pub use net::SimNet;
pub use tasks::SimTasks;

/// Locks a mutex. A panic in another thread does not make the state unusable for a test, so the poison is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A seeded random number generator: xoshiro256** with a SplitMix64 seed expansion.
///
/// It is fast and has a period of 2^256 - 1. It is not for cryptography.
#[derive(Clone, Debug)]
pub struct SimRng {
    s: [u64; 4],
}

impl SimRng {
    /// A generator for the seed.
    pub fn new(seed: u64) -> SimRng {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        SimRng { s: [next(), next(), next(), next()] }
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// A value in `0..n`. Returns 0 if `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        // The multiply and shift of Lemire. The bias is below 2^-64 times n, which a test cannot see.
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// True with a chance of `per_million` in one million.
    pub fn chance(&mut self, per_million: u32) -> bool {
        per_million > 0 && self.below(1_000_000) < u64::from(per_million)
    }

    /// Fills `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes()[..chunk.len()]);
        }
    }

    /// Puts the items in a random order.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

/// A clock that moves only when the test moves it.
///
/// The wall clock and the monotonic clock start together. [`SimClock::advance`] moves both. [`SimClock::jump_wall`] moves only the wall clock, forward or back, as an NTP step or a bad host clock does.
#[derive(Debug)]
pub struct SimClock {
    wall_ms: AtomicU64,
    monotonic_ns: AtomicU64,
}

impl SimClock {
    /// A clock with the wall clock at `wall_ms` (Unix milliseconds) and the monotonic clock at zero.
    pub fn new(wall_ms: u64) -> SimClock {
        SimClock { wall_ms: AtomicU64::new(wall_ms), monotonic_ns: AtomicU64::new(0) }
    }

    /// Moves both clocks forward.
    pub fn advance(&self, d: Duration) {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let before = self.monotonic_ns.fetch_add(ns, Ordering::SeqCst);
        // The wall clock moves by the whole milliseconds that the monotonic clock crossed.
        let ms = (before + ns) / 1_000_000 - before / 1_000_000;
        self.wall_ms.fetch_add(ms, Ordering::SeqCst);
    }

    /// Moves the wall clock by `delta_ms`, which can be negative. The monotonic clock does not move.
    pub fn jump_wall(&self, delta_ms: i64) {
        let _ = self.wall_ms.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |w| {
            Some(w.saturating_add_signed(delta_ms))
        });
    }
}

impl Clock for SimClock {
    fn wall_ms(&self) -> u64 {
        self.wall_ms.load(Ordering::SeqCst)
    }

    fn monotonic(&self) -> Duration {
        Duration::from_nanos(self.monotonic_ns.load(Ordering::SeqCst))
    }
}

/// Random bytes from a [`SimRng`].
#[derive(Debug)]
pub struct SimEntropy {
    rng: Mutex<SimRng>,
}

impl SimEntropy {
    /// A source for the seed.
    pub fn new(seed: u64) -> SimEntropy {
        SimEntropy { rng: Mutex::new(SimRng::new(seed)) }
    }
}

impl Entropy for SimEntropy {
    fn fill(&self, buf: &mut [u8]) {
        lock(&self.rng).fill(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rng_is_deterministic() {
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        let xs: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..100).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(SimRng::new(43).next_u64(), xs[0]);
        // A fixed value guards the algorithm. A change here changes every recorded seed.
        assert_eq!(SimRng::new(0).next_u64(), 0x99ec_5f36_cb75_f2b4);
    }

    #[test]
    fn the_rng_ranges() {
        let mut r = SimRng::new(1);
        let mut seen = [false; 10];
        for _ in 0..1000 {
            seen[r.below(10) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(r.below(0), 0);
        assert!(!(0..1000).any(|_| r.chance(0)));
        assert!((0..1000).all(|_| r.chance(1_000_000)));
        let mut v: Vec<u32> = (0..20).collect();
        r.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
        assert_ne!(v, sorted);
    }

    #[test]
    fn the_clock_moves_only_when_told() {
        let c = SimClock::new(1_800_000_000_000);
        assert_eq!(c.monotonic(), Duration::ZERO);
        c.advance(Duration::from_micros(1500));
        assert_eq!(c.wall_ms(), 1_800_000_000_001);
        c.advance(Duration::from_micros(600));
        assert_eq!(c.wall_ms(), 1_800_000_000_002);
        c.jump_wall(-60_000);
        assert_eq!(c.wall_ms(), 1_799_999_940_002);
        assert_eq!(c.monotonic(), Duration::from_micros(2100));
        c.jump_wall(i64::MIN);
        assert_eq!(c.wall_ms(), 0);
    }

    #[test]
    fn the_entropy_is_seeded() {
        let a = SimEntropy::new(7);
        let b = SimEntropy::new(7);
        assert_eq!(a.next_u64(), b.next_u64());
        let mut x = [0; 13];
        let mut y = [0; 13];
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
    }
}

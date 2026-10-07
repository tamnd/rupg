//! The 64-bit state word of each frame (spec/08 section 8.8.1). This is the state word of vmcache.
//!
//! The high 8 bits hold the state and the low 56 bits hold the version. State 0 is unlocked, 1 to 252 is locked shared with that count of readers, 253 is locked exclusive and 255 is evicted. The release of an exclusive latch and an eviction increase the version. An optimistic reader keeps the version that it saw and compares it at the end.

use std::sync::atomic::{AtomicU64, Ordering, fence};

const VERSION_BITS: u32 = 56;
const VERSION_MASK: u64 = (1 << VERSION_BITS) - 1;
const ONE_READER: u64 = 1 << VERSION_BITS;

/// The state of a frame, from the high 8 bits of the state word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Unlocked,
    Shared(u8),
    Exclusive,
    Evicted,
}

const UNLOCKED: u64 = 0;
const MAX_SHARED: u64 = 252;
const EXCLUSIVE: u64 = 253;
const EVICTED: u64 = 255;

/// A value of the state word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Word(u64);

impl Word {
    fn new(state: u64, version: u64) -> Word {
        Word((state << VERSION_BITS) | (version & VERSION_MASK))
    }

    pub(crate) fn version(self) -> u64 {
        self.0 & VERSION_MASK
    }

    pub(crate) fn state(self) -> State {
        match self.0 >> VERSION_BITS {
            UNLOCKED => State::Unlocked,
            EXCLUSIVE => State::Exclusive,
            EVICTED => State::Evicted,
            // Only the methods of `Latch` write the word, and they never write state 254.
            n => State::Shared(n as u8),
        }
    }
}

/// The state word of one frame.
#[derive(Debug)]
pub(crate) struct Latch(AtomicU64);

impl Latch {
    /// A frame that holds no page.
    pub(crate) fn evicted() -> Latch {
        Latch(AtomicU64::new(Word::new(EVICTED, 0).0))
    }

    pub(crate) fn load(&self) -> Word {
        Word(self.0.load(Ordering::Acquire))
    }

    /// Changes `seen` to locked exclusive, if the word still has that value. `seen` must be unlocked or evicted.
    pub(crate) fn try_exclusive(&self, seen: Word) -> bool {
        debug_assert!(matches!(seen.state(), State::Unlocked | State::Evicted));
        let locked = Word::new(EXCLUSIVE, seen.version());
        let ok =
            self.0.compare_exchange(seen.0, locked.0, Ordering::Acquire, Ordering::Relaxed).is_ok();
        if ok {
            // The new state must be visible before the writes to the page, so that an optimistic reader that sees one of the writes also sees the lock. This is the order of the crossbeam seqlock.
            fence(Ordering::Release);
        }
        ok
    }

    /// Releases an exclusive latch and increases the version.
    pub(crate) fn release_exclusive(&self) {
        self.set_after_exclusive(UNLOCKED);
    }

    /// Releases an exclusive latch, marks the frame evicted and increases the version.
    pub(crate) fn release_evicted(&self) {
        self.set_after_exclusive(EVICTED);
    }

    fn set_after_exclusive(&self, state: u64) {
        let word = self.load();
        debug_assert_eq!(word.state(), State::Exclusive);
        self.0.store(Word::new(state, word.version() + 1).0, Ordering::Release);
    }

    /// Adds a reader to `seen`, if the word still has that value. `seen` must be unlocked or shared with fewer than 252 readers.
    pub(crate) fn try_shared(&self, seen: Word) -> bool {
        let readers = match seen.state() {
            State::Unlocked => 0,
            State::Shared(n) if u64::from(n) < MAX_SHARED => u64::from(n),
            _ => return false,
        };
        let next = Word::new(readers + 1, seen.version());
        self.0.compare_exchange(seen.0, next.0, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Removes a reader. The version does not change, because a reader does not change the page.
    pub(crate) fn release_shared(&self) {
        let before = self.0.fetch_sub(ONE_READER, Ordering::Release);
        debug_assert!(matches!(Word(before).state(), State::Shared(_)));
    }

    /// True if no writer took the frame after `seen`, which an optimistic reader got from [`Latch::load`]. The caller reads the page between the two loads.
    pub(crate) fn still(&self, seen: Word) -> bool {
        // The reads of the page must complete before this load.
        fence(Ordering::Acquire);
        let now = Word(self.0.load(Ordering::Relaxed));
        now.version() == seen.version() && matches!(now.state(), State::Unlocked | State::Shared(_))
    }
}

/// Waits a short time before a retry. It spins first and then gives the processor to other threads.
#[derive(Debug, Default)]
pub(crate) struct Backoff(u32);

impl Backoff {
    pub(crate) fn wait(&mut self) {
        if self.0 < 6 {
            for _ in 0..1 << self.0 {
                std::hint::spin_loop();
            }
        } else {
            std::thread::yield_now();
        }
        self.0 = self.0.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive() {
        let l = Latch::evicted();
        let w = l.load();
        assert_eq!((w.state(), w.version()), (State::Evicted, 0));
        assert!(l.try_exclusive(w));
        assert!(!l.try_exclusive(w));
        assert_eq!(l.load().state(), State::Exclusive);
        l.release_exclusive();
        let w = l.load();
        assert_eq!((w.state(), w.version()), (State::Unlocked, 1));
        assert!(l.try_exclusive(w));
        l.release_evicted();
        assert_eq!((l.load().state(), l.load().version()), (State::Evicted, 2));
    }

    #[test]
    fn shared() {
        let l = Latch::evicted();
        assert!(!l.try_shared(l.load()));
        assert!(l.try_exclusive(l.load()));
        assert!(!l.try_shared(l.load()));
        l.release_exclusive();
        for n in 1..=252 {
            assert!(l.try_shared(l.load()));
            assert_eq!(l.load().state(), State::Shared(n));
        }
        assert!(!l.try_shared(l.load()));
        for _ in 0..252 {
            l.release_shared();
        }
        assert_eq!((l.load().state(), l.load().version()), (State::Unlocked, 1));
    }

    #[test]
    fn optimistic() {
        let l = Latch::evicted();
        assert!(l.try_exclusive(l.load()));
        l.release_exclusive();
        let seen = l.load();
        assert!(l.still(seen));
        // A reader does not make an optimistic read fail.
        assert!(l.try_shared(l.load()));
        assert!(l.still(seen));
        l.release_shared();
        // A writer does, both while it holds the latch and after.
        assert!(l.try_exclusive(l.load()));
        assert!(!l.still(seen));
        l.release_exclusive();
        assert!(!l.still(seen));
        assert!(l.still(l.load()));
    }

    #[test]
    fn the_version_wraps() {
        let l = Latch(AtomicU64::new(Word::new(UNLOCKED, VERSION_MASK).0));
        assert!(l.try_exclusive(l.load()));
        l.release_exclusive();
        assert_eq!((l.load().state(), l.load().version()), (State::Unlocked, 0));
    }

    #[test]
    fn backoff() {
        let mut b = Backoff::default();
        for _ in 0..10 {
            b.wait();
        }
        assert_eq!(b.0, 10);
    }
}

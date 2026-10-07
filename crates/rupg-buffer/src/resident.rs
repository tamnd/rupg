//! The set of resident pages in the window. The clock hand sweeps its slots. This is the resident set of vmcache: an open addressing table with no lock.

use std::sync::atomic::{AtomicU64, Ordering};

const EMPTY: u64 = u64::MAX;
/// A slot that held a page. A search goes past it, and an insert can take it.
const REMOVED: u64 = u64::MAX - 1;

/// A set of page numbers with room for at least `max` pages.
///
/// Only the thread that holds the exclusive latch of a page inserts or removes that page, so a page is never in two slots.
#[derive(Debug)]
pub(crate) struct ResidentSet {
    slots: Box<[AtomicU64]>,
    mask: usize,
}

impl ResidentSet {
    /// A set for `max` pages. It has twice as many slots, so that a search ends soon.
    pub(crate) fn new(max: usize) -> ResidentSet {
        let n = max.saturating_mul(2).max(16).next_power_of_two();
        ResidentSet { slots: (0..n).map(|_| AtomicU64::new(EMPTY)).collect(), mask: n - 1 }
    }

    fn start(&self, page: u64) -> usize {
        (page.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as usize & self.mask
    }

    /// The number of slots.
    pub(crate) fn slots(&self) -> usize {
        self.slots.len()
    }

    /// The page in `slot`, if there is one.
    pub(crate) fn get(&self, slot: usize) -> Option<u64> {
        let v = self.slots[slot & self.mask].load(Ordering::Acquire);
        (v < REMOVED).then_some(v)
    }

    /// Adds `page`. The caller makes sure that the set has room, because the pool holds at most `max` pages.
    pub(crate) fn insert(&self, page: u64) {
        debug_assert!(page < REMOVED);
        let mut i = self.start(page);
        loop {
            let slot = &self.slots[i];
            let v = slot.load(Ordering::Relaxed);
            if (v == EMPTY || v == REMOVED)
                && slot.compare_exchange(v, page, Ordering::AcqRel, Ordering::Relaxed).is_ok()
            {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Removes `page`. An insert puts a page in the first free slot after its start, and a slot never becomes empty again, so the search finds the page before an empty slot.
    pub(crate) fn remove(&self, page: u64) {
        let mut i = self.start(page);
        for _ in 0..self.slots.len() {
            let v = self.slots[i].load(Ordering::Relaxed);
            if v == page {
                self.slots[i].store(REMOVED, Ordering::Release);
                return;
            }
            if v == EMPTY {
                break;
            }
            i = (i + 1) & self.mask;
        }
        debug_assert!(false, "page {page} is not in the resident set");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(s: &ResidentSet) -> Vec<u64> {
        let mut v: Vec<u64> = (0..s.slots()).filter_map(|i| s.get(i)).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn insert_and_remove() {
        let s = ResidentSet::new(10);
        assert_eq!(s.slots(), 32);
        for p in 0..10 {
            s.insert(p * 1000);
        }
        assert_eq!(pages(&s).len(), 10);
        s.remove(3000);
        s.remove(0);
        assert_eq!(pages(&s), [1000, 2000, 4000, 5000, 6000, 7000, 8000, 9000]);
        // Many changes leave no empty slot, and the set still works.
        for round in 0..100 {
            for p in 0..8 {
                s.insert(1_000_000 + round * 100 + p);
            }
            for p in 0..8 {
                s.remove(1_000_000 + round * 100 + p);
            }
        }
        assert_eq!(pages(&s).len(), 8);
        s.insert(77);
        s.remove(77);
        assert_eq!(pages(&s).len(), 8);
    }
}

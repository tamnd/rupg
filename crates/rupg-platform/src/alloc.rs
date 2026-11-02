//! `Counting`, a global allocator that counts the bytes that each thread allocates, for `EXPLAIN (MEMORY)`.
//!
//! The counters are in the storage of the thread, so the count costs no atomic operation. A block that a thread frees and that an other thread allocated changes the count of the thread that frees it, so the counts are correct for work that stays on one thread, as the planning of a query does.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// The bytes that the thread allocated, less the bytes that it freed. The value wraps.
    static LIVE: Cell<u64> = const { Cell::new(0) };
    /// The bytes that the thread allocated. The value wraps.
    static TOTAL: Cell<u64> = const { Cell::new(0) };
}

/// The allocator of the system, with the counts of each thread. The binary sets it with `#[global_allocator]`. Without it, [`Mark::since`] gives 0.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counting;

/// Adds `size` to the counts of the thread.
fn grow(size: usize) {
    let size = size as u64;
    LIVE.with(|c| c.set(c.get().wrapping_add(size)));
    TOTAL.with(|c| c.set(c.get().wrapping_add(size)));
}

/// Removes `size` from the live count of the thread.
fn shrink(size: usize) {
    LIVE.with(|c| c.set(c.get().wrapping_sub(size as u64)));
}

// SAFETY: each method passes its arguments to `System` without change, so the contract of `GlobalAlloc` is the contract of `System`. The counts use `Cell`s with no destructor, which do not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::realloc`.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            shrink(layout.size());
            grow(new_size);
        }
        p
    }
}

/// `M_ARENA_MAX`: the number of the arenas of the allocator of the C library. Each thread of a session takes an arena, and a block that a thread frees stays in the arena of that thread, so many arenas keep the peak of each thread.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const ARENAS: i32 = 2;

/// `M_TRIM_THRESHOLD`: the free bytes at the top of an arena above which the allocator gives the memory back to the system.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRIM_THRESHOLD: i32 = 1 << 20;

/// Sets the arenas and the trim threshold of the allocator of the C library, so that the memory that a load or a query frees goes back to the system. The binary calls it before it starts its threads. It does nothing on a system with an other allocator.
pub fn tune() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `mallopt` takes the parameters of glibc and changes only the settings of the allocator.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, ARENAS);
        libc::mallopt(libc::M_TRIM_THRESHOLD, TRIM_THRESHOLD);
        libc::mallopt(libc::M_TOP_PAD, 0);
    }
}

/// Gives the free pages of all the arenas of the allocator of the C library back to the system, after work that freed much memory, such as a load. It does nothing on a system with an other allocator.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only gives free memory of the allocator back to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// The counts of the thread at a point in time.
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    live: u64,
    total: u64,
}

impl Mark {
    /// The counts of the current thread now.
    pub fn now() -> Mark {
        Mark { live: LIVE.with(Cell::get), total: TOTAL.with(Cell::get) }
    }

    /// The bytes that the thread allocated since the mark and still has, and all the bytes that it allocated since the mark. The first value is 0 when the thread freed more than it allocated.
    pub fn since(self) -> (u64, u64) {
        let now = Mark::now();
        let live = now.live.wrapping_sub(self.live);
        let used = if live > u64::MAX / 2 { 0 } else { live };
        let total = now.total.wrapping_sub(self.total);
        (used.min(total), total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_without_the_allocator_are_zero() {
        // The tests do not set `Counting` as the global allocator.
        let mark = Mark::now();
        let v = vec![0u8; 4096];
        assert_eq!(v.len(), 4096);
        assert_eq!(mark.since(), (0, 0));
    }

    #[test]
    fn counts_of_the_allocator() {
        let mark = Mark::now();
        let layout = Layout::from_size_align(1000, 8).unwrap();
        // SAFETY: the layout has a size that is not zero, and the block is freed with the same layout.
        unsafe {
            let p = Counting.alloc(layout);
            assert!(!p.is_null());
            assert_eq!(mark.since(), (1000, 1000));
            Counting.dealloc(p, layout);
        }
        assert_eq!(mark.since(), (0, 1000));
    }
}

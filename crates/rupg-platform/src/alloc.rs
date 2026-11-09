//! `Counting`, a global allocator that counts the bytes that each thread allocates, for `EXPLAIN (MEMORY)`.
//!
//! With the feature `mimalloc`, `Counting` gets its blocks from mimalloc 2. Each thread of mimalloc has its own heap, so the sessions do not wait on the lock of an arena of the C library. Without the feature, it gets them from `System`.
//!
//! The counters are in the storage of the thread, so the count costs no atomic operation. A block that a thread frees and that an other thread allocated changes the count of the thread that frees it, so the counts are correct for work that stays on one thread, as the planning of a query does.

#[cfg(not(feature = "mimalloc"))]
use std::alloc::System;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;

thread_local! {
    /// The bytes that the thread allocated, less the bytes that it freed. The value wraps.
    static LIVE: Cell<u64> = const { Cell::new(0) };
    /// The bytes that the thread allocated. The value wraps.
    static TOTAL: Cell<u64> = const { Cell::new(0) };
}

/// The allocator of the system, or mimalloc with the feature `mimalloc`, with the counts of each thread. The binary sets it with `#[global_allocator]`. Without it, [`Mark::since`] gives 0.
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

/// The blocks of the allocator under `Counting`.
#[cfg(not(feature = "mimalloc"))]
mod inner {
    use super::{GlobalAlloc, Layout, System};

    /// Gets a block of `layout`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::alloc`.
    pub(super) unsafe fn alloc(layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }

    /// Gets a block of `layout` with all its bytes 0.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
    pub(super) unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    /// Frees a block of `layout`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::dealloc`.
    pub(super) unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Changes the size of a block of `layout` to `new_size`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::realloc`.
    pub(super) unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// The blocks of the allocator under `Counting`.
#[cfg(feature = "mimalloc")]
mod inner {
    use super::Layout;
    use core::ffi::c_void;
    use libmimalloc_sys::{mi_free, mi_malloc_aligned, mi_realloc_aligned, mi_zalloc_aligned};

    // `libmimalloc-sys` has `mi_collect` only with its feature `extended`, which rupg does not turn on. The function is in the library that the crate links.
    unsafe extern "C" {
        /// Frees the empty pages of the heaps of mimalloc, and with `force` gives them back to the system.
        pub(super) fn mi_collect(force: bool);
        /// Sets the option `option` of mimalloc to `value`.
        pub(super) fn mi_option_set(option: core::ffi::c_int, value: core::ffi::c_long);
    }

    /// Gets a block of `layout`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::alloc`.
    pub(super) unsafe fn alloc(layout: Layout) -> *mut u8 {
        // SAFETY: `mi_malloc_aligned` takes any size and an alignment that is a power of two, which `Layout` makes sure of.
        unsafe { mi_malloc_aligned(layout.size(), layout.align()).cast() }
    }

    /// Gets a block of `layout` with all its bytes 0.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
    pub(super) unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        unsafe { mi_zalloc_aligned(layout.size(), layout.align()).cast() }
    }

    /// Frees a block of `layout`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::dealloc`.
    pub(super) unsafe fn dealloc(ptr: *mut u8, _layout: Layout) {
        // SAFETY: the contract of `GlobalAlloc::dealloc` makes `ptr` a block of this allocator that is not free.
        unsafe { mi_free(ptr.cast::<c_void>()) }
    }

    /// Changes the size of a block of `layout` to `new_size`.
    ///
    /// # Safety
    ///
    /// The caller keeps the contract of `GlobalAlloc::realloc`.
    pub(super) unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the contract of `GlobalAlloc::realloc` makes `ptr` a block of this allocator with the alignment of `layout`, and `mi_realloc_aligned` keeps that alignment.
        unsafe { mi_realloc_aligned(ptr.cast::<c_void>(), new_size, layout.align()).cast() }
    }
}

// SAFETY: each method passes its arguments to `inner` without change, so the contract of `GlobalAlloc` is the contract of the allocator under it. The counts use `Cell`s with no destructor, which do not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
        let p = unsafe { inner::alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc_zeroed`.
        let p = unsafe { inner::alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
        unsafe { inner::dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc::realloc`.
        let p = unsafe { inner::realloc(ptr, layout, new_size) };
        if !p.is_null() {
            shrink(layout.size());
            grow(new_size);
        }
        p
    }
}

/// `M_ARENA_MAX`: the number of the arenas of the allocator of the C library. Each thread of a session takes an arena, and a block that a thread frees stays in the arena of that thread, so many arenas keep the peak of each thread.
#[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "mimalloc")))]
const ARENAS: i32 = 2;

/// `M_TRIM_THRESHOLD`: the free bytes at the top of an arena above which the allocator gives the memory back to the system.
#[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "mimalloc")))]
const TRIM_THRESHOLD: i32 = 1 << 20;

/// `mi_option_purge_delay`: the number of the option of mimalloc 2 that sets the time in milliseconds before mimalloc gives free pages back to the system.
#[cfg(feature = "mimalloc")]
const PURGE_DELAY: core::ffi::c_int = 15;

/// The purge delay of mimalloc in milliseconds. With the default of 10 ms, a query that frees and gets large blocks many times gives the pages back to the system and gets them again, and the kernel clears each new page. On ClickBench Q33 with 2M rows, 1000 ms decreases the CPU time from 1633 ms to 1421 ms. [`trim`] gives the free pages back after a load.
#[cfg(feature = "mimalloc")]
const PURGE_DELAY_MS: core::ffi::c_long = 1000;

/// Sets the arenas and the trim threshold of the allocator of the C library, so that the memory that a load or a query frees goes back to the system. With the feature `mimalloc`, it sets the purge delay of mimalloc, so that a query uses its free pages again before they go back to the system. The binary calls it before it starts its threads. It does nothing on a system with an other allocator.
pub fn tune() {
    #[cfg(feature = "mimalloc")]
    // SAFETY: `mi_option_set` changes only a setting of mimalloc, and `PURGE_DELAY` is an option of mimalloc 2.
    unsafe {
        inner::mi_option_set(PURGE_DELAY, PURGE_DELAY_MS);
    }
    #[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "mimalloc")))]
    // SAFETY: `mallopt` takes the parameters of glibc and changes only the settings of the allocator.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, ARENAS);
        libc::mallopt(libc::M_TRIM_THRESHOLD, TRIM_THRESHOLD);
        libc::mallopt(libc::M_TOP_PAD, 0);
    }
}

/// Gives the free pages of all the arenas of the allocator back to the system, after work that freed much memory, such as a load. With the feature `mimalloc`, it gives back the free pages of the heap of the calling thread and of the arenas of mimalloc. It does nothing on a system with an other allocator.
///
/// mimalloc 2 lets one thread at a time give the free pages of its arenas back, and a call that comes while an other thread does this does not give them back. The threads of the pool become idle at the same time after a query and call `trim` together, so one lock lets the calls come one after the other. One call can also free a page of a segment after the last page of the segment that it visits, and then it only marks the page to give back after the purge delay. The second call gives these pages back. With one call and no lock, the server kept 190 MB of ClickBench Q19 with 2M rows for 11 seconds after the query, until the threads ended. With the lock and the second call it kept 42 MB 1 second after the query.
pub fn trim() {
    #[cfg(feature = "mimalloc")]
    {
        static TRIMS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = TRIMS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: `mi_collect` only gives free memory of mimalloc back to the system.
        unsafe {
            inner::mi_collect(true);
            inner::mi_collect(true);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "mimalloc")))]
    // SAFETY: `malloc_trim` only gives free memory of the allocator back to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// The size of a large page of the memory on Linux on x86-64 and on most ARM systems.
const LARGE_PAGE: usize = 2 << 20;

/// The fewest bytes of a vector of [`large_vec`] that ask for large pages. A smaller block fits in the cache of addresses of the processor with small pages.
const LARGE_VEC: usize = 4 * LARGE_PAGE;

/// A vector of `len` copies of `value`, whose memory the system can give in large pages. Random reads of a large table, such as the hash table of `GROUP BY`, then miss the cache of addresses of the processor less often. Each miss of that cache reads the tables of the pages, and in a virtual machine these reads cost more. A vector of fewer than `LARGE_VEC` bytes, or a system with no large pages, gets small pages.
pub fn large_vec<T: Clone>(len: usize, value: T) -> Vec<T> {
    let mut v: Vec<T> = Vec::with_capacity(len);
    let bytes = len.saturating_mul(size_of::<T>());
    if bytes >= LARGE_VEC {
        // The advice comes before the first write, so that the first write of each large page can get a large page. Pages that an earlier use of the block wrote stay small until the kernel joins them.
        advise_large(v.as_mut_ptr().cast::<u8>(), bytes);
    }
    v.resize(len, value);
    v
}

/// Asks the system to give the large pages inside the `len` bytes at `start` as large pages.
#[cfg(all(target_os = "linux", not(miri)))]
fn advise_large(start: *mut u8, len: usize) {
    let from = start.addr().next_multiple_of(LARGE_PAGE);
    let end = (start.addr() + len) / LARGE_PAGE * LARGE_PAGE;
    if from < end {
        // SAFETY: MADV_HUGEPAGE only tells the kernel which size of page to use for the range. It does not change the bytes of the range, and the range is inside a block that the caller owns. A failure leaves small pages.
        unsafe {
            libc::madvise(start.with_addr(from).cast(), end - from, libc::MADV_HUGEPAGE);
        }
    }
}

/// Other systems give the pages that they give.
#[cfg(not(all(target_os = "linux", not(miri))))]
fn advise_large(_start: *mut u8, _len: usize) {}

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

    /// The resident anonymous memory of the process in KiB, from `/proc/self/status`.
    #[cfg(all(feature = "mimalloc", target_os = "linux"))]
    #[allow(clippy::disallowed_methods)] // The test reads a file of the system, which is not a file of the database.
    fn anon_kib() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("RssAnon:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    #[cfg(all(feature = "mimalloc", target_os = "linux"))]
    #[allow(clippy::disallowed_methods)] // The test needs threads of the system, which each have their own heap in mimalloc.
    #[test]
    fn trims_of_threads_at_the_same_time_give_back_the_free_blocks() {
        use std::sync::{Arc, Barrier};
        const THREADS: usize = 8;
        const MIB: usize = 1 << 20;
        tune();
        let before = anon_kib();
        // The threads free their blocks, wait for the others, and trim at the same time, as the threads of the pool do after a query. They then wait until the test has measured the memory, because a thread that ends gives its pages back.
        let freed = Arc::new(Barrier::new(THREADS + 1));
        let trimmed = Arc::new(Barrier::new(THREADS + 1));
        let measured = Arc::new(Barrier::new(THREADS + 1));
        let threads: Vec<_> = (0..THREADS)
            .map(|t| {
                let (freed, trimmed, measured) = (freed.clone(), trimmed.clone(), measured.clone());
                std::thread::spawn(move || {
                    let mut blocks = Vec::new();
                    for i in 0..24 {
                        // Sizes of different classes, from 1 to 3.5 MiB, as the hash tables of a query.
                        let size = [4, 5, 6, 7, 8, 10, 12, 14][(i + t) % 8] * MIB / 4;
                        let layout = Layout::from_size_align(size, 8).unwrap();
                        // SAFETY: the size is not 0, and the block is written only in its bytes and freed once below.
                        unsafe {
                            let p = inner::alloc(layout);
                            assert!(!p.is_null());
                            p.write_bytes(1, size);
                            blocks.push((p, layout));
                        }
                    }
                    for (p, layout) in blocks {
                        // SAFETY: `p` is a block of `inner::alloc` with `layout`.
                        unsafe { inner::dealloc(p, layout) };
                    }
                    freed.wait();
                    trim();
                    trimmed.wait();
                    measured.wait();
                })
            })
            .collect();
        freed.wait();
        let full = anon_kib();
        trimmed.wait();
        let after = anon_kib();
        measured.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        // The purge delay keeps the free blocks until the trims.
        assert!(full > before + (100 << 10), "{before} KiB before, {full} KiB before the trims");
        assert!(after < before + (32 << 10), "{before} KiB before, {after} KiB after the trims");
    }

    #[test]
    fn a_large_vec_has_its_copies_of_the_value() {
        for len in [0, 1, 1000, LARGE_VEC / 24 + 3, 3 * LARGE_VEC / 24] {
            let v = large_vec(len, (7_u64, 9_u32, 11_u64));
            assert_eq!(v.len(), len);
            assert!(v.iter().all(|&x| x == (7, 9, 11)), "{len}");
        }
    }

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

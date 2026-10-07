//! The buffer pool: a fixed table of frames, a map from logical page number to frame, the latches and the clock.
//!
//! This is the form of spec/08 section 8.8.4. The frames are one fixed allocation, reserved from the memory budget at open, and a hash map finds the frame of a page. The state word and the guards are the same as in the virtual memory window of section 8.8.1.

use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use rupg_common::{Error, Result, SqlState};
use rupg_file::{OptimisticRead, PAGE_SIZE, Page, PageAccess, PageId, in_page};
use rupg_platform::{MemoryPool, Reservation};

use crate::frames::{FrameMemory, read_racy};
use crate::latch::{Backoff, Latch, State, Word};
use crate::store::Store;

/// The smallest number of frames in a pool.
pub const MIN_FRAMES: usize = 16;

/// The number of parts of the map from page to frame. Each part has its own lock.
const SHARDS: usize = 64;

/// One frame: the state word and the bookkeeping of the page that it holds. The bytes of the page are in [`FrameMemory`].
#[derive(Debug)]
struct Frame {
    latch: Latch,
    /// The logical page number. It changes only under the exclusive latch.
    page: AtomicU64,
    /// Set at each access. The clock clears it and evicts the frame on the next sweep if it is still clear.
    referenced: AtomicBool,
    /// Set when an exclusive guard is dropped. The page writer clears it when the page did not change during the write.
    dirty: AtomicBool,
    /// Set while a thread writes the page to the store. One write of a page at a time keeps the writes in order, so an old copy never replaces a newer one.
    writing: AtomicBool,
}

/// The bytes that each frame takes from the memory budget.
const FRAME_BYTES: u64 = (PAGE_SIZE + size_of::<Frame>()) as u64;

/// Counters of a [`BufferPool`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferStats {
    /// The number of frames.
    pub frames: u64,
    /// The frames that hold a page.
    pub resident: u64,
    /// The frames that hold a page that is not written yet.
    pub dirty: u64,
    /// Accesses that found the page in a frame.
    pub hits: u64,
    /// Accesses that read the page from the store.
    pub misses: u64,
    /// Pages that the clock removed from a frame.
    pub evictions: u64,
    /// Pages written to the store.
    pub writes: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    writes: AtomicU64,
}

/// The buffer pool. It implements [`PageAccess`] over a [`Store`].
///
/// A page that is dirty when the pool is dropped is lost. Call [`BufferPool::flush`] first.
pub struct BufferPool {
    store: Arc<dyn Store>,
    memory: FrameMemory,
    frames: Box<[Frame]>,
    map: Box<[RwLock<HashMap<u64, usize>>]>,
    free: Mutex<Vec<usize>>,
    hand: AtomicUsize,
    counters: Counters,
    _reservation: Reservation,
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferPool")
            .field("store", &self.store)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl BufferPool {
    /// A pool of `bytes / 16384` frames over `store`. The pool reserves the frames and their bookkeeping from `memory` and keeps the reservation until it is dropped.
    ///
    /// Fewer than [`MIN_FRAMES`] frames gives SQLSTATE `22023`. A budget that does not have the bytes gives `53200`.
    pub fn new(store: Arc<dyn Store>, memory: &Arc<MemoryPool>, bytes: u64) -> Result<BufferPool> {
        let count = usize::try_from(bytes / PAGE_SIZE as u64).unwrap_or(usize::MAX);
        if count < MIN_FRAMES {
            return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "the buffer pool is too small")
                .with_detail(format!(
                    "{bytes} bytes give {count} frames of {PAGE_SIZE} bytes. The minimum is {MIN_FRAMES} frames."
                )));
        }
        let reserve = (count as u64).saturating_mul(FRAME_BYTES);
        let reservation = memory.reserve(reserve)?;
        let memory = FrameMemory::new(count)?;
        let frames = (0..count)
            .map(|_| Frame {
                latch: Latch::evicted(),
                page: AtomicU64::new(0),
                referenced: AtomicBool::new(false),
                dirty: AtomicBool::new(false),
                writing: AtomicBool::new(false),
            })
            .collect();
        Ok(BufferPool {
            store,
            memory,
            frames,
            map: (0..SHARDS).map(|_| RwLock::default()).collect(),
            // The frames are taken from the end, so frame 0 is used first.
            free: Mutex::new((0..count).rev().collect()),
            hand: AtomicUsize::new(0),
            counters: Counters::default(),
            _reservation: reservation,
        })
    }

    /// The counters.
    pub fn stats(&self) -> BufferStats {
        let resident = self.frames.iter().filter(|f| f.latch.load().state() != State::Evicted);
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        BufferStats {
            frames: self.frames.len() as u64,
            resident: resident.count() as u64,
            dirty: self.frames.iter().filter(|f| f.dirty.load(Ordering::Relaxed)).count() as u64,
            hits: get(&self.counters.hits),
            misses: get(&self.counters.misses),
            evictions: get(&self.counters.evictions),
            writes: get(&self.counters.writes),
        }
    }

    /// Writes every dirty page to the store and gives the number of pages written. A page that a writer holds at the time, or that another thread writes at the time, stays dirty.
    pub fn flush(&self) -> Result<usize> {
        let mut written = 0;
        for index in 0..self.frames.len() {
            if self.write_back(index)? {
                written += 1;
            }
        }
        Ok(written)
    }

    fn shard(&self, page: u64) -> &RwLock<HashMap<u64, usize>> {
        // A multiplicative hash, so that pages with numbers close together go to different shards.
        &self.map[(page.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58) as usize % SHARDS]
    }

    fn lookup(&self, page: PageId) -> Option<usize> {
        self.shard(page.0).read().unwrap_or_else(PoisonError::into_inner).get(&page.0).copied()
    }

    fn unmap(&self, page: u64) {
        self.shard(page).write().unwrap_or_else(PoisonError::into_inner).remove(&page);
    }

    /// The frame that holds `page`. It reads the page on a miss. The caller must latch the frame and check that it still holds the page.
    fn frame_of(&self, page: PageId) -> Result<usize> {
        match self.lookup(page) {
            Some(index) => {
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                Ok(index)
            }
            None => self.load(page),
        }
    }

    fn touch(&self, index: usize) {
        let r = &self.frames[index].referenced;
        if !r.load(Ordering::Relaxed) {
            r.store(true, Ordering::Relaxed);
        }
    }

    /// Reads `page` into a frame. The frame is in the map and locked exclusive during the read, so other threads that want the page wait for it.
    fn load(&self, page: PageId) -> Result<usize> {
        let index = self.take_frame()?;
        let frame = &self.frames[index];
        {
            let mut shard = self.shard(page.0).write().unwrap_or_else(PoisonError::into_inner);
            if let Some(&other) = shard.get(&page.0) {
                // Another thread read the page first.
                drop(shard);
                self.put_free(index);
                return Ok(other);
            }
            frame.page.store(page.0, Ordering::Relaxed);
            shard.insert(page.0, index);
        }
        // SAFETY: this thread holds the exclusive latch of the frame, so no other reference to its bytes exists.
        let bytes = unsafe { &mut *self.memory.page(index) };
        match self.store.read(page, bytes) {
            Ok(()) => {
                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                frame.dirty.store(false, Ordering::Relaxed);
                frame.referenced.store(true, Ordering::Relaxed);
                frame.latch.release_exclusive();
                Ok(index)
            }
            Err(e) => {
                self.unmap(page.0);
                self.put_free(index);
                Err(e)
            }
        }
    }

    /// Releases the exclusive latch of a frame that holds no page and puts the frame on the free list.
    fn put_free(&self, index: usize) {
        self.frames[index].latch.release_evicted();
        self.free.lock().unwrap_or_else(PoisonError::into_inner).push(index);
    }

    /// A frame that holds no page and is not in the map, locked exclusive.
    fn take_frame(&self) -> Result<usize> {
        let free = self.free.lock().unwrap_or_else(PoisonError::into_inner).pop();
        match free {
            Some(index) => {
                // Only the owner of a free frame locks it, because the other paths do not lock an evicted frame.
                if self.frames[index].latch.try_exclusive(self.frames[index].latch.load()) {
                    Ok(index)
                } else {
                    Err(Error::internal(format!("free buffer frame {index} is latched")))
                }
            }
            None => self.evict_one(),
        }
    }

    /// Runs the clock until it evicts a frame, and gives the frame locked exclusive. A dirty frame is written to the store first.
    fn evict_one(&self) -> Result<usize> {
        let n = self.frames.len();
        // Two sweeps clear every referenced bit, so a third finds a frame if one is not latched.
        for _ in 0..3 * n {
            let index = self.hand.fetch_add(1, Ordering::Relaxed) % n;
            let frame = &self.frames[index];
            if frame.latch.load().state() != State::Unlocked
                || frame.referenced.swap(false, Ordering::Relaxed)
            {
                continue;
            }
            if frame.dirty.load(Ordering::Acquire) {
                self.write_back(index)?;
            }
            let seen = frame.latch.load();
            if seen.state() != State::Unlocked || !frame.latch.try_exclusive(seen) {
                continue;
            }
            if frame.dirty.load(Ordering::Acquire) {
                frame.latch.release_exclusive();
                continue;
            }
            self.unmap(frame.page.load(Ordering::Relaxed));
            self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            return Ok(index);
        }
        Err(Error::new(SqlState::OUT_OF_MEMORY, "no free buffer frame")
            .with_detail(format!("All {n} frames of the buffer pool are latched or dirty."))
            .with_hint("Increase shared_buffers."))
    }

    /// Writes the page of a dirty frame to the store, as in steps 1 and 7 of spec/08 section 8.6.1. Gives false if the frame is not dirty, a writer holds it, or another thread writes it.
    fn write_back(&self, index: usize) -> Result<bool> {
        let frame = &self.frames[index];
        if !frame.dirty.load(Ordering::Acquire) || frame.writing.swap(true, Ordering::Acquire) {
            return Ok(false);
        }
        let result = self.write_copy(index);
        frame.writing.store(false, Ordering::Release);
        result
    }

    fn write_copy(&self, index: usize) -> Result<bool> {
        let frame = &self.frames[index];
        let seen = frame.latch.load();
        if !frame.latch.try_shared(seen) {
            return Ok(false);
        }
        if !frame.dirty.load(Ordering::Acquire) {
            frame.latch.release_shared();
            return Ok(false);
        }
        let page = PageId(frame.page.load(Ordering::Relaxed));
        let mut copy = Box::new([0u8; PAGE_SIZE]);
        // SAFETY: this thread holds a shared latch, so no thread writes the bytes.
        copy.copy_from_slice(unsafe { &*self.memory.page(index) });
        frame.latch.release_shared();
        self.store.write(page, &copy)?;
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        // The page is clean only if no writer took it after the copy. A writer increases the version when it releases the latch, and it sets the dirty flag before that.
        let now = frame.latch.load();
        if now.version() == seen.version() && frame.latch.try_shared(now) {
            frame.dirty.store(false, Ordering::Release);
            frame.latch.release_shared();
        }
        Ok(true)
    }

    fn latch(&self, page: PageId, exclusive: bool) -> Result<usize> {
        let mut backoff = Backoff::default();
        loop {
            let index = self.frame_of(page)?;
            let frame = &self.frames[index];
            let seen = frame.latch.load();
            let ok = match seen.state() {
                // The frame left the map after the lookup. Look again.
                State::Evicted => continue,
                State::Exclusive => false,
                State::Shared(_) if exclusive => false,
                _ if exclusive => frame.latch.try_exclusive(seen),
                _ => frame.latch.try_shared(seen),
            };
            if !ok {
                backoff.wait();
                continue;
            }
            if frame.page.load(Ordering::Relaxed) == page.0 {
                self.touch(index);
                return Ok(index);
            }
            // The frame holds another page now.
            if exclusive {
                frame.latch.release_exclusive();
            } else {
                frame.latch.release_shared();
            }
        }
    }
}

impl PageAccess for BufferPool {
    type Optimistic<'a> = OptimisticGuard<'a>;
    type Shared<'a> = SharedGuard<'a>;
    type Exclusive<'a> = ExclusiveGuard<'a>;

    fn optimistic(&self, page: PageId) -> Result<OptimisticGuard<'_>> {
        let mut backoff = Backoff::default();
        loop {
            let index = self.frame_of(page)?;
            let frame = &self.frames[index];
            let seen = frame.latch.load();
            match seen.state() {
                State::Evicted => continue,
                State::Exclusive => backoff.wait(),
                _ if frame.page.load(Ordering::Relaxed) == page.0 => {
                    self.touch(index);
                    return Ok(OptimisticGuard { pool: self, index, page, seen });
                }
                _ => {}
            }
        }
    }

    fn shared(&self, page: PageId) -> Result<SharedGuard<'_>> {
        Ok(SharedGuard { pool: self, index: self.latch(page, false)? })
    }

    fn exclusive(&self, page: PageId) -> Result<ExclusiveGuard<'_>> {
        Ok(ExclusiveGuard { pool: self, index: self.latch(page, true)? })
    }

    fn allocate(&self) -> Result<(PageId, ExclusiveGuard<'_>)> {
        let page = self.store.allocate()?;
        let index = match self.take_frame() {
            Ok(index) => index,
            Err(e) => {
                // The number goes back. The error of the allocation is the one to report.
                let _ = self.store.free(page);
                return Err(e);
            }
        };
        let frame = &self.frames[index];
        {
            let mut shard = self.shard(page.0).write().unwrap_or_else(PoisonError::into_inner);
            if shard.contains_key(&page.0) {
                drop(shard);
                self.put_free(index);
                return Err(Error::internal(format!(
                    "the store gave logical page {page}, which is in the buffer pool"
                )));
            }
            frame.page.store(page.0, Ordering::Relaxed);
            shard.insert(page.0, index);
        }
        // SAFETY: this thread holds the exclusive latch of the frame.
        unsafe { &mut *self.memory.page(index) }.fill(0);
        frame.referenced.store(true, Ordering::Relaxed);
        Ok((page, ExclusiveGuard { pool: self, index }))
    }

    fn free(&self, page: PageId) -> Result<()> {
        let mut backoff = Backoff::default();
        while let Some(index) = self.lookup(page) {
            let frame = &self.frames[index];
            let seen = frame.latch.load();
            if seen.state() != State::Unlocked || !frame.latch.try_exclusive(seen) {
                backoff.wait();
                continue;
            }
            if frame.page.load(Ordering::Relaxed) != page.0 {
                frame.latch.release_exclusive();
                continue;
            }
            if frame.writing.load(Ordering::Acquire) {
                // The write must complete before the store frees the page.
                frame.latch.release_exclusive();
                backoff.wait();
                continue;
            }
            self.unmap(page.0);
            frame.dirty.store(false, Ordering::Relaxed);
            self.put_free(index);
            break;
        }
        self.store.free(page)
    }
}

/// An optimistic read of a page in a [`BufferPool`].
#[derive(Debug)]
pub struct OptimisticGuard<'a> {
    pool: &'a BufferPool,
    index: usize,
    page: PageId,
    seen: Word,
}

impl OptimisticRead for OptimisticGuard<'_> {
    fn page(&self) -> PageId {
        self.page
    }

    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        if !in_page(at, out.len()) {
            return false;
        }
        // SAFETY: the frame memory lives as long as the pool, and the range is inside the page.
        unsafe { read_racy(self.pool.memory.page(self.index), at, out) };
        true
    }

    fn is_valid(&self) -> bool {
        self.pool.frames[self.index].latch.still(self.seen)
    }
}

/// A shared latch on a page in a [`BufferPool`]. It releases the latch when it is dropped.
#[derive(Debug)]
pub struct SharedGuard<'a> {
    pool: &'a BufferPool,
    index: usize,
}

impl SharedGuard<'_> {
    /// The logical page number.
    pub fn page(&self) -> PageId {
        PageId(self.pool.frames[self.index].page.load(Ordering::Relaxed))
    }
}

impl Deref for SharedGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        // SAFETY: the guard holds a shared latch, so no thread writes the bytes while the reference lives.
        unsafe { &*self.pool.memory.page(self.index) }
    }
}

impl Drop for SharedGuard<'_> {
    fn drop(&mut self) {
        self.pool.frames[self.index].latch.release_shared();
    }
}

/// An exclusive latch on a page in a [`BufferPool`]. It marks the page dirty and releases the latch when it is dropped.
#[derive(Debug)]
pub struct ExclusiveGuard<'a> {
    pool: &'a BufferPool,
    index: usize,
}

impl ExclusiveGuard<'_> {
    /// The logical page number.
    pub fn page(&self) -> PageId {
        PageId(self.pool.frames[self.index].page.load(Ordering::Relaxed))
    }
}

impl Deref for ExclusiveGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        // SAFETY: the guard holds the exclusive latch, so no other thread writes the bytes.
        unsafe { &*self.pool.memory.page(self.index) }
    }
}

impl DerefMut for ExclusiveGuard<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        // SAFETY: the guard holds the exclusive latch, and `&mut self` makes this the only reference. Optimistic readers can read the bytes during the write. They read with `read_racy` and discard what they read when the version changes.
        unsafe { &mut *self.pool.memory.page(self.index) }
    }
}

impl Drop for ExclusiveGuard<'_> {
    fn drop(&mut self) {
        let frame = &self.pool.frames[self.index];
        frame.dirty.store(true, Ordering::Release);
        frame.latch.release_exclusive();
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::MemStore;

    const FRAMES: u64 = MIN_FRAMES as u64;

    fn pool(store: &Arc<MemStore>, frames: u64) -> BufferPool {
        let memory = MemoryPool::new(1 << 30);
        BufferPool::new(Arc::clone(store) as Arc<dyn Store>, &memory, frames * PAGE_SIZE as u64)
            .unwrap()
    }

    /// A store with `n` pages. Page `p` holds `p` in its first 8 bytes.
    fn filled(n: u64) -> Arc<MemStore> {
        let store = Arc::new(MemStore::new());
        for _ in 0..n {
            let p = store.allocate().unwrap();
            let mut page = Box::new([0u8; PAGE_SIZE]);
            page[..8].copy_from_slice(&p.0.to_le_bytes());
            store.write(p, &page).unwrap();
        }
        store
    }

    fn first(page: &Page) -> u64 {
        u64::from_le_bytes(page[..8].try_into().unwrap())
    }

    #[test]
    fn reads_go_through() {
        let store = filled(3);
        let pool = pool(&store, FRAMES);
        for p in 1..=3 {
            assert_eq!(first(&pool.shared(PageId(p)).unwrap()), p);
        }
        let g = pool.shared(PageId(2)).unwrap();
        assert_eq!(g.page(), PageId(2));
        // Two shared latches on one page at the same time.
        assert_eq!(first(&pool.shared(PageId(2)).unwrap()), 2);
        drop(g);
        let s = pool.stats();
        assert_eq!((s.frames, s.resident, s.dirty), (16, 3, 0));
        assert_eq!((s.misses, s.hits), (3, 2));
        assert_eq!(store.counts().0, 3);
    }

    #[test]
    fn eviction_writes_dirty_pages() {
        let store = filled(100);
        let pool = pool(&store, FRAMES);
        for p in 1..=100 {
            let mut g = pool.exclusive(PageId(p)).unwrap();
            g[8..16].copy_from_slice(&(p * 3).to_le_bytes());
        }
        let s = pool.stats();
        assert_eq!(s.resident, 16);
        assert!(s.evictions >= 84, "{s:?}");
        for p in 1..=100 {
            let g = pool.shared(PageId(p)).unwrap();
            assert_eq!(u64::from_le_bytes(g[8..16].try_into().unwrap()), p * 3, "page {p}");
        }
        pool.flush().unwrap();
        assert_eq!(pool.stats().dirty, 0);
        assert_eq!(pool.flush().unwrap(), 0);
        for p in 1..=100 {
            let page = store.get(PageId(p)).unwrap();
            assert_eq!(u64::from_le_bytes(page[8..16].try_into().unwrap()), p * 3);
        }
    }

    #[test]
    fn optimistic_reads() {
        let store = filled(2);
        let pool = pool(&store, FRAMES);
        let r = pool.optimistic(PageId(1)).unwrap();
        assert_eq!(r.page(), PageId(1));
        assert_eq!(r.u64_at(0), Some(1));
        assert_eq!(r.u64_at(PAGE_SIZE - 4), None);
        assert!(r.is_valid());
        // A shared latch does not change the page.
        drop(pool.shared(PageId(1)).unwrap());
        assert!(r.is_valid());
        let mut w = pool.exclusive(PageId(1)).unwrap();
        assert!(!r.is_valid());
        w[0] = 9;
        drop(w);
        assert!(!r.is_valid());
        let r = pool.optimistic(PageId(1)).unwrap();
        assert_eq!((r.u8_at(0), r.is_valid()), (Some(9), true));
    }

    #[test]
    fn an_evicted_page_fails_an_optimistic_read() {
        let store = filled(40);
        let pool = pool(&store, FRAMES);
        let r = pool.optimistic(PageId(1)).unwrap();
        for p in 2..=40 {
            drop(pool.shared(PageId(p)).unwrap());
        }
        assert!(pool.lookup(PageId(1)).is_none());
        assert!(!r.is_valid());
    }

    #[test]
    fn allocate_and_free() {
        let store = Arc::new(MemStore::new());
        let pool = pool(&store, FRAMES);
        let (p, mut g) = pool.allocate().unwrap();
        assert_eq!(p, PageId(1));
        assert_eq!(g.page(), p);
        assert!(g.iter().all(|&b| b == 0));
        g[100] = 5;
        drop(g);
        assert_eq!(pool.stats().dirty, 1);
        assert_eq!(pool.flush().unwrap(), 1);
        assert_eq!(store.get(p).unwrap()[100], 5);
        pool.free(p).unwrap();
        assert!(store.get(p).is_none());
        assert_eq!(pool.stats().resident, 0);
        // The number comes back with zero bytes.
        let (q, g) = pool.allocate().unwrap();
        assert_eq!((q, g[100]), (p, 0));
        drop(g);
        // A page that is not in a frame is freed in the store only.
        let (r, g) = pool.allocate().unwrap();
        drop(g);
        pool.flush().unwrap();
        // Enough new pages to evict `r` from its frame.
        for _ in 0..40 {
            drop(pool.allocate().unwrap());
        }
        assert!(pool.lookup(r).is_none());
        pool.free(r).unwrap();
        assert!(store.get(r).is_none());
    }

    #[test]
    fn a_failed_read_leaves_no_frame() {
        let store = filled(1);
        let pool = pool(&store, FRAMES);
        let e = pool.shared(PageId(7)).unwrap_err();
        assert_eq!(e.state(), SqlState::INTERNAL_ERROR);
        assert_eq!(pool.stats().resident, 0);
        assert!(pool.lookup(PageId(7)).is_none());
        assert_eq!(first(&pool.shared(PageId(1)).unwrap()), 1);
    }

    #[test]
    fn all_frames_latched() {
        let store = filled(17);
        let pool = pool(&store, FRAMES);
        let held: Vec<_> = (1..=16).map(|p| pool.shared(PageId(p)).unwrap()).collect();
        let e = pool.shared(PageId(17)).unwrap_err();
        assert_eq!(e.state(), SqlState::OUT_OF_MEMORY);
        assert_eq!(e.detail(), Some("All 16 frames of the buffer pool are latched or dirty."));
        drop(held);
        assert_eq!(first(&pool.shared(PageId(17)).unwrap()), 17);
    }

    #[test]
    fn the_budget() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(1 << 20);
        let e = BufferPool::new(Arc::clone(&store), &memory, 15 * PAGE_SIZE as u64).unwrap_err();
        assert_eq!(e.state(), SqlState::INVALID_PARAMETER_VALUE);
        let e = BufferPool::new(Arc::clone(&store), &memory, 64 * PAGE_SIZE as u64).unwrap_err();
        assert_eq!(e.state(), SqlState::OUT_OF_MEMORY);
        let pool = BufferPool::new(Arc::clone(&store), &memory, 32 * PAGE_SIZE as u64).unwrap();
        assert_eq!(memory.used(), 32 * FRAME_BYTES);
        drop(pool);
        assert_eq!(memory.used(), 0);
    }

    // Miri reports the reads of an optimistic reader during a write as a data race. The race is the design of the seqlock. See `read_racy`.
    // The test needs threads of the operating system, because the simulation runs one thread and does not test the latches.
    #[test]
    #[cfg_attr(miri, ignore)]
    #[allow(clippy::disallowed_methods)]
    fn threads() {
        const PAGES: u64 = 64;
        const THREADS: u64 = 4;
        const ROUNDS: u64 = 2000;
        let store = filled(PAGES);
        let pool = pool(&store, FRAMES);
        thread::scope(|s| {
            for t in 0..THREADS {
                let pool = &pool;
                s.spawn(move || {
                    let mut x = t + 1;
                    for _ in 0..ROUNDS {
                        // A small xorshift, so that each thread visits the pages in its own order.
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let page = PageId(x % PAGES + 1);
                        // The writer keeps the same count at offset 64 and at offset 8000.
                        let mut g = pool.exclusive(page).unwrap();
                        let n = u64::from_le_bytes(g[64..72].try_into().unwrap()) + 1;
                        g[64..72].copy_from_slice(&n.to_le_bytes());
                        g[8000..8008].copy_from_slice(&n.to_le_bytes());
                        drop(g);
                        let r = pool.optimistic(PageId(x % 7 + 1)).unwrap();
                        let (a, b) = (r.u64_at(64), r.u64_at(8000));
                        if r.is_valid() {
                            assert_eq!(a, b);
                        }
                    }
                });
            }
        });
        pool.flush().unwrap();
        let total: u64 = (1..=PAGES)
            .map(|p| u64::from_le_bytes(store.get(PageId(p)).unwrap()[64..72].try_into().unwrap()))
            .sum();
        assert_eq!(total, THREADS * ROUNDS);
    }
}

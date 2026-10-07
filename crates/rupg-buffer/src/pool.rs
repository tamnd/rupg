//! The buffer pool of spec/08 section 8.8: the units, the latches, the clock and the memory budget.
//!
//! The pool has two forms with the same state word and the same guards.
//!
//! The window of section 8.8.1 reserves address space for every logical page. The bytes of page `n` are at the base plus `n * 16384`, and its state word is at index `n` of a second region, so a hit is an addition and no lookup. The pool takes memory from the budget as it reads pages, and the reclaimer gives clean pages back when another consumer needs the memory.
//!
//! The table of section 8.8.4 is one fixed allocation of frames, and a hash map finds the frame of a page. A host that cannot reserve address space uses it.

use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};

use rupg_common::{Error, Result, SqlState};
use rupg_file::{OptimisticRead, PAGE_SIZE, Page, PageAccess, PageId, in_page};
use rupg_platform::{MemoryPool, Region, Reservation};

use crate::frames::{FrameMemory, read_racy};
use crate::latch::{Backoff, Latch, State, Word};
use crate::resident::ResidentSet;
use crate::store::Store;

/// The smallest pool, in pages.
pub const MIN_FRAMES: usize = 16;

/// The number of parts of the map from page to frame in the table. Each part has its own lock.
const SHARDS: usize = 64;

/// The window takes memory from the budget this many pages at a time, so that a miss does not lock the budget each time.
const GROW_PAGES: usize = 64;

/// The largest window, 2^31 pages or 32 TiB. This is one quarter of the user address space of x86-64 with 4-level page tables.
const MAX_WINDOW_PAGES: u64 = 1 << 31;

/// The smallest window that the pool tries before it uses the table, 2^22 pages or 64 GiB.
const MIN_WINDOW_PAGES: u64 = 1 << 22;

/// The bookkeeping of one unit: the state word and the flags. The bytes of the page are in other memory.
///
/// Zero bytes are a valid `Frame`: an evicted unit with version 0 and no flags. The window depends on this, because its frames are in a region that the operating system gives as zero bytes.
#[derive(Debug)]
struct Frame {
    latch: Latch,
    /// The logical page number in the table. The window does not use it, because the index of the unit is the page number.
    page: AtomicU64,
    /// Set at each access. The clock clears it and evicts the unit on the next sweep if it is still clear.
    referenced: AtomicBool,
    /// Set when an exclusive guard is dropped. The page writer clears it when the page did not change during the write.
    dirty: AtomicBool,
    /// Set while a thread writes the page to the store. One write of a page at a time keeps the writes in order, so an old copy never replaces a newer one.
    writing: AtomicBool,
    /// One more than the latch version of the last copy that the filter changed or skipped, or 0. The clock does not write the page again until a writer changes it, because the filter would do the same again.
    kept: AtomicU64,
}

impl Frame {
    fn new() -> Frame {
        Frame {
            latch: Latch::evicted(),
            page: AtomicU64::new(0),
            referenced: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            writing: AtomicBool::new(false),
            kept: AtomicU64::new(0),
        }
    }
}

/// The bytes that each frame of the table takes from the memory budget.
const FRAME_BYTES: u64 = (PAGE_SIZE + size_of::<Frame>()) as u64;

/// What a [`PageFilter`] did to the copy of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtered {
    /// The copy is the page. The pool writes it, and the page is clean after the write if no writer changed it.
    Same,
    /// The filter changed the copy. The pool writes it, and the page stays dirty, so the clock does not evict it.
    Changed,
    /// The copy must not be written now. The page stays dirty.
    Skip,
}

/// Changes the copy of a page before the pool writes it to the store. The transaction layer uses it to keep versions that are not committed out of the file (spec/11 section 11.15).
///
/// The pool calls the filter while it holds a shared latch on the page, so no writer changes the page during the call. The filter must not latch a page of the pool.
pub trait PageFilter: Send + Sync + fmt::Debug {
    /// Changes `copy`, the copy of `page` that the pool is about to write.
    fn filter(&self, page: PageId, copy: &mut Page) -> Result<Filtered>;
}

/// The result of one try to write a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrote {
    /// The page is in the store.
    Yes,
    /// The unit is not dirty.
    Clean,
    /// A writer holds the unit.
    Latched,
    /// The filter did not let the page go to the store.
    Skipped,
}

/// The form of a [`BufferPool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMode {
    /// The virtual memory window of spec/08 section 8.8.1.
    Window,
    /// The fixed table of frames of spec/08 section 8.8.4.
    Table,
}

impl fmt::Display for PoolMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PoolMode::Window => "window",
            PoolMode::Table => "table",
        })
    }
}

/// The settings of a [`BufferPool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// `shared_buffers` in bytes. The window does not shrink below it. The table has this size.
    pub shared_buffers: u64,
    /// The form. `None` takes the window if the host can reserve it, and the table if it cannot.
    pub mode: Option<PoolMode>,
    /// The number of logical pages that the window holds. `None` takes the largest reservation that the host gives, from 2^31 pages down to 2^22 pages.
    pub window_pages: Option<u64>,
}

impl PoolConfig {
    /// The settings with `shared_buffers` and the defaults for the rest.
    pub fn new(shared_buffers: u64) -> PoolConfig {
        PoolConfig { shared_buffers, mode: None, window_pages: None }
    }
}

impl Default for PoolConfig {
    /// The default of `shared_buffers` in PostgreSQL, 128 MiB.
    fn default() -> PoolConfig {
        PoolConfig::new(128 << 20)
    }
}

/// Counters of a [`BufferPool`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferStats {
    /// The bytes that the pool holds from the memory budget.
    pub reserved: u64,
    /// The units that hold a page.
    pub resident: u64,
    /// The units that hold a page that is not written yet.
    pub dirty: u64,
    /// Accesses that found the page in memory.
    pub hits: u64,
    /// Accesses that read the page from the store.
    pub misses: u64,
    /// Pages that the clock or the reclaimer removed from memory.
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

#[derive(Debug)]
enum Units {
    Window(Window),
    Table(Table),
}

#[derive(Debug)]
struct Window {
    data: Region,
    frames: Region,
    pages: usize,
    set: ResidentSet,
}

impl Window {
    fn reserve(pages: Option<u64>, max: usize) -> Result<Window> {
        if Region::os_page_size() > PAGE_SIZE {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "the window needs pages of the operating system of at most 16384 bytes",
            ));
        }
        let mut n = pages.unwrap_or(MAX_WINDOW_PAGES);
        loop {
            match Window::try_reserve(n) {
                Ok((data, frames, size)) => {
                    let set = ResidentSet::new(max.min(size));
                    return Ok(Window { data, frames, pages: size, set });
                }
                Err(_) if pages.is_none() && n > MIN_WINDOW_PAGES => n /= 2,
                Err(e) => return Err(e),
            }
        }
    }

    fn try_reserve(n: u64) -> Result<(Region, Region, usize)> {
        let too_large = || Error::internal(format!("a window of {n} pages is too large"));
        let pages = usize::try_from(n).map_err(|_| too_large())?;
        let data = Region::reserve(pages.checked_mul(PAGE_SIZE).ok_or_else(too_large)?)?;
        let frames = Region::reserve(pages.checked_mul(size_of::<Frame>()).ok_or_else(too_large)?)?;
        Ok((data, frames, pages))
    }

    fn index(&self, page: PageId) -> Result<usize> {
        usize::try_from(page.0).ok().filter(|&i| i < self.pages).ok_or_else(|| {
            Error::new(SqlState::INSUFFICIENT_RESOURCES, "the page is outside the buffer window")
                .with_detail(format!("Logical page {page}. The window holds {} pages.", self.pages))
        })
    }
}

#[derive(Debug)]
struct Table {
    memory: FrameMemory,
    frames: Box<[Frame]>,
    map: Box<[RwLock<HashMap<u64, usize>>]>,
    free: Mutex<Vec<usize>>,
}

impl Table {
    fn new(count: usize) -> Result<Table> {
        Ok(Table {
            memory: FrameMemory::new(count)?,
            frames: (0..count).map(|_| Frame::new()).collect(),
            map: (0..SHARDS).map(|_| RwLock::default()).collect(),
            // The frames are taken from the end, so frame 0 is used first.
            free: Mutex::new((0..count).rev().collect()),
        })
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

    /// Releases the exclusive latch of a frame that holds no page and puts the frame on the free list.
    fn put_free(&self, index: usize) {
        self.frames[index].latch.release_evicted();
        self.free.lock().unwrap_or_else(PoisonError::into_inner).push(index);
    }
}

/// The part of the memory budget that the pool holds.
#[derive(Debug)]
struct Budget {
    reservation: Mutex<Reservation>,
    /// The number of pages that the reservation pays for.
    allowed: AtomicUsize,
    /// `shared_buffers` in pages.
    min: usize,
    /// The most pages that the window holds: the budget limit at open, or the size of the window if it is smaller.
    max: usize,
}

impl Budget {
    fn lock(&self) -> MutexGuard<'_, Reservation> {
        self.reservation.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The buffer pool. It implements [`PageAccess`] over a [`Store`].
///
/// A page that is dirty when the pool is dropped is lost. Call [`BufferPool::flush`] first.
pub struct BufferPool {
    store: Arc<dyn Store>,
    units: Units,
    budget: Budget,
    /// The pages in memory in the window, and the pages that a miss is reading.
    resident: AtomicUsize,
    hand: AtomicUsize,
    counters: Counters,
    filter: OnceLock<Arc<dyn PageFilter>>,
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferPool")
            .field("mode", &self.mode())
            .field("store", &self.store)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl BufferPool {
    /// A pool over `store` that takes its memory from `memory`.
    ///
    /// `shared_buffers` below 16 pages gives SQLSTATE `22023`. A budget that does not have `shared_buffers` gives `53200`. In the window form, the pool sets the reclaimer of `memory`, so that other consumers can take memory from clean pages. This replaces the reclaimer that `memory` had.
    pub fn new(
        store: Arc<dyn Store>,
        memory: &Arc<MemoryPool>,
        config: PoolConfig,
    ) -> Result<Arc<BufferPool>> {
        let min = usize::try_from(config.shared_buffers / PAGE_SIZE as u64).unwrap_or(usize::MAX);
        if min < MIN_FRAMES {
            return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "shared_buffers is too small")
                .with_detail(format!(
                    "{} bytes give {min} pages of {PAGE_SIZE} bytes. The minimum is {MIN_FRAMES} pages.",
                    config.shared_buffers
                )));
        }
        let max = usize::try_from(memory.limit() / PAGE_SIZE as u64).unwrap_or(usize::MAX).max(min);
        let window = match config.mode {
            Some(PoolMode::Table) => None,
            Some(PoolMode::Window) => Some(Window::reserve(config.window_pages, max)?),
            None => Window::reserve(config.window_pages, max).ok(),
        };
        let (units, reservation, max) = match window {
            Some(w) => {
                let reservation = memory.reserve(min as u64 * PAGE_SIZE as u64)?;
                let max = max.min(w.pages);
                (Units::Window(w), reservation, max)
            }
            None => {
                let reservation = memory.reserve((min as u64).saturating_mul(FRAME_BYTES))?;
                (Units::Table(Table::new(min)?), reservation, min)
            }
        };
        let pool = Arc::new(BufferPool {
            store,
            units,
            budget: Budget {
                reservation: Mutex::new(reservation),
                allowed: AtomicUsize::new(min),
                min,
                max,
            },
            resident: AtomicUsize::new(0),
            hand: AtomicUsize::new(0),
            counters: Counters::default(),
            filter: OnceLock::new(),
        });
        if pool.mode() == PoolMode::Window {
            let weak = Arc::downgrade(&pool);
            memory.set_reclaimer(Box::new(move |need| {
                weak.upgrade().map_or(0, |pool| pool.reclaim(need))
            }));
        }
        Ok(pool)
    }

    /// The form of the pool.
    pub fn mode(&self) -> PoolMode {
        match self.units {
            Units::Window(_) => PoolMode::Window,
            Units::Table(_) => PoolMode::Table,
        }
    }

    /// The counters.
    pub fn stats(&self) -> BufferStats {
        let mut resident = 0;
        let mut dirty = 0;
        let _ = self.each_unit(|index| {
            resident += 1;
            if self.frame(index).dirty.load(Ordering::Relaxed) {
                dirty += 1;
            }
            Ok(())
        });
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        BufferStats {
            reserved: self.budget.lock().bytes(),
            resident,
            dirty,
            hits: get(&self.counters.hits),
            misses: get(&self.counters.misses),
            evictions: get(&self.counters.evictions),
            writes: get(&self.counters.writes),
        }
    }

    /// Writes every dirty page to the store and gives the number of pages written. A page that a writer holds at the time, or that another thread writes at the time, stays dirty.
    pub fn flush(&self) -> Result<usize> {
        let mut written = 0;
        self.each_unit(|index| {
            if self.write_back(index)? {
                written += 1;
            }
            Ok(())
        })?;
        Ok(written)
    }

    /// Writes every page that is dirty at the call to the store, and gives the number of pages written. Unlike [`BufferPool::flush`], it waits for a page that a writer holds or that another thread writes. A page that the filter skips gives SQLSTATE `XX000`.
    ///
    /// A page that a writer changes during the call can stay dirty. A checkpoint stops the writers first.
    pub fn flush_all(&self) -> Result<usize> {
        let mut written = 0;
        self.each_unit(|index| {
            let frame = self.frame(index);
            let mut backoff = Backoff::default();
            loop {
                if !frame.dirty.load(Ordering::Acquire) {
                    return Ok(());
                }
                if frame.writing.swap(true, Ordering::Acquire) {
                    backoff.wait();
                    continue;
                }
                let result = self.write_copy(index);
                frame.writing.store(false, Ordering::Release);
                match result? {
                    Wrote::Yes => {
                        written += 1;
                        return Ok(());
                    }
                    Wrote::Clean => return Ok(()),
                    Wrote::Latched => backoff.wait(),
                    Wrote::Skipped => {
                        return Err(Error::internal(format!(
                            "the filter does not let page {} go to the store",
                            self.page_of(index)
                        )));
                    }
                }
            }
        })?;
        Ok(written)
    }

    /// Sets the filter that changes each page before the pool writes it. A pool has one filter, and a second call gives SQLSTATE `XX000`.
    pub fn set_filter(&self, filter: Arc<dyn PageFilter>) -> Result<()> {
        self.filter.set(filter).map_err(|_| Error::internal("the buffer pool has a filter already"))
    }

    /// Calls `f` with each unit that holds a page.
    fn each_unit(&self, mut f: impl FnMut(usize) -> Result<()>) -> Result<()> {
        match &self.units {
            Units::Window(w) => {
                for slot in 0..w.set.slots() {
                    if let Some(page) = w.set.get(slot) {
                        f(page as usize)?;
                    }
                }
            }
            Units::Table(t) => {
                for (index, frame) in t.frames.iter().enumerate() {
                    if frame.latch.load().state() != State::Evicted {
                        f(index)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn frame(&self, index: usize) -> &Frame {
        match &self.units {
            Units::Window(w) => {
                assert!(index < w.pages, "unit {index} of {}", w.pages);
                // SAFETY: the region has room for `pages` frames. It is aligned to a page of the operating system, which is more than the alignment of `Frame`. Zero bytes are a valid `Frame`, and every change to a frame is through its atomic fields. The region lives as long as the pool.
                unsafe { &*w.frames.as_ptr().cast::<Frame>().add(index) }
            }
            Units::Table(t) => &t.frames[index],
        }
    }

    /// A pointer to the bytes of a unit. The caller must hold the latch that allows the access that it makes through the pointer.
    fn bytes(&self, index: usize) -> *mut Page {
        match &self.units {
            Units::Window(w) => {
                assert!(index < w.pages, "unit {index} of {}", w.pages);
                // SAFETY: `index` is less than `pages`, so the offset is inside the region.
                unsafe { w.data.as_ptr().add(index * PAGE_SIZE).cast::<Page>() }
            }
            Units::Table(t) => t.memory.page(index),
        }
    }

    /// True if the unit holds `page`. The caller holds a latch on the unit or checks the version later.
    fn holds(&self, index: usize, page: PageId) -> bool {
        match self.units {
            Units::Window(_) => index as u64 == page.0,
            Units::Table(_) => self.frame(index).page.load(Ordering::Relaxed) == page.0,
        }
    }

    fn page_of(&self, index: usize) -> PageId {
        match self.units {
            Units::Window(_) => PageId(index as u64),
            Units::Table(_) => PageId(self.frame(index).page.load(Ordering::Relaxed)),
        }
    }

    /// The unit of `page` if the page is in memory.
    fn resident_index(&self, page: PageId) -> Option<usize> {
        match &self.units {
            Units::Window(w) => {
                w.index(page).ok().filter(|&i| self.frame(i).latch.load().state() != State::Evicted)
            }
            Units::Table(t) => t.lookup(page),
        }
    }

    fn touch(&self, index: usize) {
        let r = &self.frame(index).referenced;
        if !r.load(Ordering::Relaxed) {
            r.store(true, Ordering::Relaxed);
        }
    }

    fn no_room(&self, units: usize) -> Error {
        Error::new(SqlState::OUT_OF_MEMORY, "no free buffer frame")
            .with_detail(format!("All {units} pages in the buffer pool are latched or dirty."))
            .with_hint("Increase shared_buffers or rupg.memory_limit.")
    }

    /// The unit that holds `page`. It reads the page on a miss. The caller must latch the unit and check that it still holds the page.
    fn unit_of(&self, page: PageId) -> Result<usize> {
        match &self.units {
            Units::Window(w) => {
                let index = w.index(page)?;
                let frame = self.frame(index);
                loop {
                    let seen = frame.latch.load();
                    if seen.state() != State::Evicted {
                        self.counters.hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(index);
                    }
                    if frame.latch.try_exclusive(seen) {
                        self.load_window(w, index)?;
                        return Ok(index);
                    }
                }
            }
            Units::Table(t) => match t.lookup(page) {
                Some(index) => {
                    self.counters.hits.fetch_add(1, Ordering::Relaxed);
                    Ok(index)
                }
                None => self.load_table(t, page),
            },
        }
    }

    /// Reads a page into its unit of the window. The caller holds the exclusive latch of the evicted unit. Other threads that want the page wait for the latch.
    fn load_window(&self, w: &Window, index: usize) -> Result<()> {
        let frame = self.frame(index);
        if let Err(e) = self.make_room(w) {
            frame.latch.release_evicted();
            return Err(e);
        }
        w.set.insert(index as u64);
        // SAFETY: this thread holds the exclusive latch of the unit, so no other reference to its bytes exists.
        let bytes = unsafe { &mut *self.bytes(index) };
        match self.store.read(PageId(index as u64), bytes) {
            Ok(()) => {
                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                frame.dirty.store(false, Ordering::Relaxed);
                frame.referenced.store(true, Ordering::Relaxed);
                frame.latch.release_exclusive();
                Ok(())
            }
            Err(e) => {
                self.drop_window_unit(w, index);
                Err(e)
            }
        }
    }

    /// Counts one more resident page in the window. It takes more of the budget if it can, and it evicts a page if it cannot.
    fn make_room(&self, w: &Window) -> Result<()> {
        loop {
            let resident = self.resident.load(Ordering::Acquire);
            if resident < self.budget.allowed.load(Ordering::Acquire) {
                if self
                    .resident
                    .compare_exchange(resident, resident + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Ok(());
                }
                continue;
            }
            if self.grow() {
                continue;
            }
            if !self.evict_window(w, true)? {
                return Err(self.no_room(resident));
            }
        }
    }

    /// Takes up to [`GROW_PAGES`] more pages from the budget. Gives false if the budget or the window has no more room.
    fn grow(&self) -> bool {
        let mut reservation = self.budget.lock();
        let allowed = self.budget.allowed.load(Ordering::Acquire);
        if allowed > self.resident.load(Ordering::Acquire) {
            // Another thread took more, or an eviction made room, while this one waited for the lock.
            return true;
        }
        let more = GROW_PAGES.min(self.budget.max.saturating_sub(allowed));
        // The budget can ask the reclaimer of this pool for memory. The reclaimer does not wait for the lock that this thread holds, so it frees nothing here.
        if more == 0 || reservation.grow((more * PAGE_SIZE) as u64).is_err() {
            return false;
        }
        self.budget.allowed.fetch_add(more, Ordering::AcqRel);
        true
    }

    /// Gives memory back to the budget for another consumer, and gives the number of bytes. It evicts clean pages only and does not go below `shared_buffers`.
    fn reclaim(&self, need: u64) -> u64 {
        let Units::Window(w) = &self.units else {
            return 0;
        };
        // The pool itself can be the consumer that asks, in `grow`. It holds the lock then, and has nothing to give.
        let Ok(mut reservation) = self.budget.reservation.try_lock() else {
            return 0;
        };
        let want = usize::try_from(need.div_ceil(PAGE_SIZE as u64)).unwrap_or(usize::MAX);
        let allowed = self.budget.allowed.load(Ordering::Acquire);
        let cut = want.min(allowed.saturating_sub(self.budget.min));
        if cut == 0 {
            return 0;
        }
        // Lower the limit first, so that a miss evicts instead of taking the pages that this call frees.
        let target = allowed - cut;
        self.budget.allowed.store(target, Ordering::Release);
        while self.resident.load(Ordering::Acquire) > target {
            if !matches!(self.evict_window(w, false), Ok(true)) {
                break;
            }
        }
        // The pages that stay because they are dirty or latched keep their memory.
        let short = self.resident.load(Ordering::Acquire).saturating_sub(target).min(cut);
        self.budget.allowed.fetch_add(short, Ordering::AcqRel);
        let freed = ((cut - short) * PAGE_SIZE) as u64;
        reservation.shrink(freed);
        freed
    }

    /// Runs the clock over the resident set until it evicts a page. With `write`, a dirty page is written first. Without it, dirty pages stay. Gives false if no page can be evicted.
    fn evict_window(&self, w: &Window, write: bool) -> Result<bool> {
        let n = w.set.slots();
        // Two sweeps clear every referenced bit, so a third finds a page if one is not latched.
        for _ in 0..3 * n {
            let slot = self.hand.fetch_add(1, Ordering::Relaxed) % n;
            let Some(page) = w.set.get(slot) else {
                continue;
            };
            let index = page as usize;
            if self.claim(index, write)? {
                self.drop_window_unit(w, index);
                self.counters.evictions.fetch_add(1, Ordering::Relaxed);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Removes a page from the window and gives its memory back to the operating system. The caller holds the exclusive latch of the unit.
    fn drop_window_unit(&self, w: &Window, index: usize) {
        w.set.remove(index as u64);
        // SAFETY: the caller holds the exclusive latch, so no reference to the bytes of the unit exists. An optimistic reader can read them during the call, and it finds the new version after.
        let discarded = unsafe { w.data.discard(index * PAGE_SIZE, PAGE_SIZE) };
        // A failed discard keeps the memory. The next read of the page writes over all of it, so the result stays correct.
        debug_assert!(discarded.is_ok(), "{discarded:?}");
        self.resident.fetch_sub(1, Ordering::AcqRel);
        self.frame(index).latch.release_evicted();
    }

    /// Reads `page` into a frame of the table. The frame is in the map and locked exclusive during the read, so other threads that want the page wait for it.
    fn load_table(&self, t: &Table, page: PageId) -> Result<usize> {
        let index = self.take_frame(t)?;
        let frame = &t.frames[index];
        {
            let mut shard = t.shard(page.0).write().unwrap_or_else(PoisonError::into_inner);
            if let Some(&other) = shard.get(&page.0) {
                // Another thread read the page first.
                drop(shard);
                t.put_free(index);
                return Ok(other);
            }
            frame.page.store(page.0, Ordering::Relaxed);
            shard.insert(page.0, index);
        }
        // SAFETY: this thread holds the exclusive latch of the frame, so no other reference to its bytes exists.
        let bytes = unsafe { &mut *t.memory.page(index) };
        match self.store.read(page, bytes) {
            Ok(()) => {
                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                frame.dirty.store(false, Ordering::Relaxed);
                frame.referenced.store(true, Ordering::Relaxed);
                frame.latch.release_exclusive();
                Ok(index)
            }
            Err(e) => {
                t.unmap(page.0);
                t.put_free(index);
                Err(e)
            }
        }
    }

    /// A frame of the table that holds no page and is not in the map, locked exclusive. It runs the clock if no frame is free.
    fn take_frame(&self, t: &Table) -> Result<usize> {
        let free = t.free.lock().unwrap_or_else(PoisonError::into_inner).pop();
        if let Some(index) = free {
            // Only the owner of a free frame locks it, because the other paths do not lock an evicted frame.
            let latch = &t.frames[index].latch;
            return if latch.try_exclusive(latch.load()) {
                Ok(index)
            } else {
                Err(Error::internal(format!("free buffer frame {index} is latched")))
            };
        }
        let n = t.frames.len();
        for _ in 0..3 * n {
            let index = self.hand.fetch_add(1, Ordering::Relaxed) % n;
            if self.claim(index, true)? {
                t.unmap(t.frames[index].page.load(Ordering::Relaxed));
                self.counters.evictions.fetch_add(1, Ordering::Relaxed);
                return Ok(index);
            }
        }
        Err(self.no_room(n))
    }

    /// Takes the exclusive latch of a unit that the clock can evict. Gives false if the unit is latched, was used since the last sweep, or stays dirty. With `write`, a dirty unit is written first.
    fn claim(&self, index: usize, write: bool) -> Result<bool> {
        let frame = self.frame(index);
        let seen = frame.latch.load();
        if seen.state() != State::Unlocked || frame.referenced.swap(false, Ordering::Relaxed) {
            return Ok(false);
        }
        if frame.dirty.load(Ordering::Acquire) {
            if !write || frame.kept.load(Ordering::Acquire) == seen.version() + 1 {
                return Ok(false);
            }
            self.write_back(index)?;
            // A page that stays dirty is not latched here, because the release of a latch increases the version and the clock would write the page again.
            if frame.dirty.load(Ordering::Acquire) {
                return Ok(false);
            }
        }
        let seen = frame.latch.load();
        if seen.state() != State::Unlocked || !frame.latch.try_exclusive(seen) {
            return Ok(false);
        }
        if frame.dirty.load(Ordering::Acquire) || frame.writing.load(Ordering::Acquire) {
            frame.latch.release_exclusive();
            return Ok(false);
        }
        Ok(true)
    }

    /// Writes the page of a dirty unit to the store, as in steps 1 and 7 of spec/08 section 8.6.1. Gives false if the unit is not dirty, a writer holds it, or another thread writes it.
    fn write_back(&self, index: usize) -> Result<bool> {
        let frame = self.frame(index);
        if !frame.dirty.load(Ordering::Acquire) || frame.writing.swap(true, Ordering::Acquire) {
            return Ok(false);
        }
        let result = self.write_copy(index);
        frame.writing.store(false, Ordering::Release);
        Ok(result? == Wrote::Yes)
    }

    /// Writes a copy of the page of a unit. The caller has set the writing flag of the unit.
    fn write_copy(&self, index: usize) -> Result<Wrote> {
        let frame = self.frame(index);
        let seen = frame.latch.load();
        if !frame.latch.try_shared(seen) {
            return Ok(Wrote::Latched);
        }
        if !frame.dirty.load(Ordering::Acquire) {
            frame.latch.release_shared();
            return Ok(Wrote::Clean);
        }
        let page = self.page_of(index);
        let mut copy = Box::new([0u8; PAGE_SIZE]);
        // SAFETY: this thread holds a shared latch, so no thread writes the bytes.
        copy.copy_from_slice(unsafe { &*self.bytes(index) });
        let filtered = self.filter.get().map_or(Ok(Filtered::Same), |f| f.filter(page, &mut copy));
        frame.latch.release_shared();
        let filtered = filtered?;
        if filtered != Filtered::Same {
            frame.kept.store(seen.version() + 1, Ordering::Release);
        }
        if filtered == Filtered::Skip {
            return Ok(Wrote::Skipped);
        }
        self.store.write(page, &copy)?;
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        // The page is clean only if the copy is the page and no writer took it after the copy. A writer increases the version when it releases the latch, and it sets the dirty flag before that.
        let now = frame.latch.load();
        if filtered == Filtered::Same
            && now.version() == seen.version()
            && frame.latch.try_shared(now)
        {
            frame.dirty.store(false, Ordering::Release);
            frame.latch.release_shared();
        }
        Ok(Wrote::Yes)
    }

    fn latch(&self, page: PageId, exclusive: bool) -> Result<usize> {
        let mut backoff = Backoff::default();
        loop {
            let index = self.unit_of(page)?;
            let frame = self.frame(index);
            let seen = frame.latch.load();
            let ok = match seen.state() {
                // The page left memory after the lookup. Look again.
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
            if self.holds(index, page) {
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

    /// Takes a unit for a new page and locks it exclusive.
    fn place(&self, page: PageId) -> Result<usize> {
        let in_pool = || {
            Error::internal(format!(
                "the store gave logical page {page}, which is in the buffer pool"
            ))
        };
        match &self.units {
            Units::Window(w) => {
                let index = w.index(page)?;
                let latch = &self.frame(index).latch;
                let seen = latch.load();
                if seen.state() != State::Evicted || !latch.try_exclusive(seen) {
                    return Err(in_pool());
                }
                if let Err(e) = self.make_room(w) {
                    latch.release_evicted();
                    return Err(e);
                }
                w.set.insert(page.0);
                Ok(index)
            }
            Units::Table(t) => {
                let index = self.take_frame(t)?;
                let mut shard = t.shard(page.0).write().unwrap_or_else(PoisonError::into_inner);
                if shard.contains_key(&page.0) {
                    drop(shard);
                    t.put_free(index);
                    return Err(in_pool());
                }
                t.frames[index].page.store(page.0, Ordering::Relaxed);
                shard.insert(page.0, index);
                Ok(index)
            }
        }
    }

    /// Removes a page from memory. The caller holds the exclusive latch of the unit.
    fn drop_unit(&self, index: usize, page: PageId) {
        match &self.units {
            Units::Window(w) => self.drop_window_unit(w, index),
            Units::Table(t) => {
                t.unmap(page.0);
                t.put_free(index);
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
            let index = self.unit_of(page)?;
            let seen = self.frame(index).latch.load();
            match seen.state() {
                State::Evicted => continue,
                State::Exclusive => backoff.wait(),
                _ if self.holds(index, page) => {
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
        let index = match self.place(page) {
            Ok(index) => index,
            Err(e) => {
                // The number goes back. The error of the allocation is the one to report.
                let _ = self.store.free(page);
                return Err(e);
            }
        };
        // SAFETY: this thread holds the exclusive latch of the unit.
        unsafe { &mut *self.bytes(index) }.fill(0);
        self.frame(index).referenced.store(true, Ordering::Relaxed);
        Ok((page, ExclusiveGuard { pool: self, index }))
    }

    fn free(&self, page: PageId) -> Result<()> {
        let mut backoff = Backoff::default();
        while let Some(index) = self.resident_index(page) {
            let frame = self.frame(index);
            let seen = frame.latch.load();
            if seen.state() != State::Unlocked || !frame.latch.try_exclusive(seen) {
                backoff.wait();
                continue;
            }
            if !self.holds(index, page) {
                frame.latch.release_exclusive();
                continue;
            }
            if frame.writing.load(Ordering::Acquire) {
                // The write must complete before the store frees the page.
                frame.latch.release_exclusive();
                backoff.wait();
                continue;
            }
            frame.dirty.store(false, Ordering::Relaxed);
            self.drop_unit(index, page);
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
        // SAFETY: the memory of the unit lives as long as the pool, and the range is inside the page.
        unsafe { read_racy(self.pool.bytes(self.index), at, out) };
        true
    }

    fn is_valid(&self) -> bool {
        self.pool.frame(self.index).latch.still(self.seen)
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
        self.pool.page_of(self.index)
    }
}

impl Deref for SharedGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        // SAFETY: the guard holds a shared latch, so no thread writes the bytes while the reference lives.
        unsafe { &*self.pool.bytes(self.index) }
    }
}

impl Drop for SharedGuard<'_> {
    fn drop(&mut self) {
        self.pool.frame(self.index).latch.release_shared();
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
        self.pool.page_of(self.index)
    }
}

impl Deref for ExclusiveGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        // SAFETY: the guard holds the exclusive latch, so no other thread writes the bytes.
        unsafe { &*self.pool.bytes(self.index) }
    }
}

impl DerefMut for ExclusiveGuard<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        // SAFETY: the guard holds the exclusive latch, and `&mut self` makes this the only reference. Optimistic readers can read the bytes during the write. They read with `read_racy` and discard what they read when the version changes.
        unsafe { &mut *self.pool.bytes(self.index) }
    }
}

impl Drop for ExclusiveGuard<'_> {
    fn drop(&mut self) {
        let frame = self.pool.frame(self.index);
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
    const PAGE: u64 = PAGE_SIZE as u64;
    const HAS_WINDOW: bool = cfg!(all(unix, not(miri)));

    /// The forms that the host can run. Miri cannot reserve address space.
    fn modes() -> Vec<PoolMode> {
        if HAS_WINDOW { vec![PoolMode::Table, PoolMode::Window] } else { vec![PoolMode::Table] }
    }

    fn config(pages: u64, mode: PoolMode) -> PoolConfig {
        PoolConfig { shared_buffers: pages * PAGE, mode: Some(mode), window_pages: Some(4096) }
    }

    /// A pool of `frames` pages with a budget that holds the pool and no more.
    fn pool(store: &Arc<MemStore>, frames: u64, mode: PoolMode) -> Arc<BufferPool> {
        let memory = MemoryPool::new(frames * FRAME_BYTES);
        let store = Arc::clone(store) as Arc<dyn Store>;
        BufferPool::new(store, &memory, config(frames, mode)).unwrap()
    }

    /// A window of 16 pages that can grow to 128 pages.
    fn window(store: &Arc<MemStore>) -> (Arc<MemoryPool>, Arc<BufferPool>) {
        let memory = MemoryPool::new(128 * PAGE);
        let store = Arc::clone(store) as Arc<dyn Store>;
        let pool = BufferPool::new(store, &memory, config(16, PoolMode::Window)).unwrap();
        (memory, pool)
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

    fn word(page: &Page, at: usize) -> u64 {
        u64::from_le_bytes(page[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn reads_go_through() {
        for mode in modes() {
            let store = filled(3);
            let pool = pool(&store, FRAMES, mode);
            assert_eq!(pool.mode(), mode);
            for p in 1..=3 {
                assert_eq!(word(&pool.shared(PageId(p)).unwrap(), 0), p);
            }
            let g = pool.shared(PageId(2)).unwrap();
            assert_eq!(g.page(), PageId(2));
            // Two shared latches on one page at the same time.
            assert_eq!(word(&pool.shared(PageId(2)).unwrap(), 0), 2);
            drop(g);
            let s = pool.stats();
            assert_eq!((s.resident, s.dirty, s.misses, s.hits), (3, 0, 3, 2), "{mode}");
            assert_eq!(store.counts().0, 3);
        }
    }

    #[test]
    fn eviction_writes_dirty_pages() {
        for mode in modes() {
            let store = filled(100);
            let pool = pool(&store, FRAMES, mode);
            for p in 1..=100 {
                let mut g = pool.exclusive(PageId(p)).unwrap();
                g[8..16].copy_from_slice(&(p * 3).to_le_bytes());
            }
            let s = pool.stats();
            assert_eq!(s.resident, 16, "{mode}");
            assert!(s.evictions >= 84, "{mode} {s:?}");
            for p in 1..=100 {
                assert_eq!(word(&pool.shared(PageId(p)).unwrap(), 8), p * 3, "{mode} page {p}");
            }
            pool.flush().unwrap();
            assert_eq!(pool.stats().dirty, 0);
            assert_eq!(pool.flush().unwrap(), 0);
            for p in 1..=100 {
                assert_eq!(word(&store.get(PageId(p)).unwrap(), 8), p * 3);
            }
        }
    }

    #[test]
    fn optimistic_reads() {
        for mode in modes() {
            let store = filled(2);
            let pool = pool(&store, FRAMES, mode);
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
    }

    #[test]
    fn an_evicted_page_fails_an_optimistic_read() {
        for mode in modes() {
            let store = filled(40);
            let pool = pool(&store, FRAMES, mode);
            let r = pool.optimistic(PageId(1)).unwrap();
            for p in 2..=40 {
                drop(pool.shared(PageId(p)).unwrap());
            }
            assert!(pool.resident_index(PageId(1)).is_none(), "{mode}");
            assert!(!r.is_valid());
        }
    }

    #[test]
    fn allocate_and_free() {
        for mode in modes() {
            let store = Arc::new(MemStore::new());
            let pool = pool(&store, FRAMES, mode);
            let (p, mut g) = pool.allocate().unwrap();
            assert_eq!((p, g.page()), (PageId(1), PageId(1)));
            assert!(g.iter().all(|&b| b == 0));
            g[100] = 5;
            drop(g);
            assert_eq!(pool.stats().dirty, 1);
            assert_eq!(pool.flush().unwrap(), 1);
            assert_eq!(store.get(p).unwrap()[100], 5);
            pool.free(p).unwrap();
            assert!(store.get(p).is_none());
            assert_eq!(pool.stats().resident, 0, "{mode}");
            // The number comes back with zero bytes.
            let (q, g) = pool.allocate().unwrap();
            assert_eq!((q, g[100]), (p, 0));
            drop(g);
            // Enough new pages to evict `p` from memory. A page that is not in memory is freed in the store only.
            for _ in 0..40 {
                drop(pool.allocate().unwrap());
            }
            assert!(pool.resident_index(p).is_none());
            pool.free(p).unwrap();
            assert!(store.get(p).is_none());
        }
    }

    #[test]
    fn a_failed_read_leaves_no_unit() {
        for mode in modes() {
            let store = filled(1);
            let pool = pool(&store, FRAMES, mode);
            let e = pool.shared(PageId(7)).unwrap_err();
            assert_eq!(e.state(), SqlState::INTERNAL_ERROR);
            assert_eq!(pool.stats().resident, 0, "{mode}");
            assert!(pool.resident_index(PageId(7)).is_none());
            assert_eq!(word(&pool.shared(PageId(1)).unwrap(), 0), 1);
        }
    }

    #[test]
    fn all_units_latched() {
        for mode in modes() {
            let store = filled(17);
            let pool = pool(&store, FRAMES, mode);
            let held: Vec<_> = (1..=16).map(|p| pool.shared(PageId(p)).unwrap()).collect();
            let e = pool.shared(PageId(17)).unwrap_err();
            assert_eq!(e.state(), SqlState::OUT_OF_MEMORY, "{mode}");
            assert_eq!(e.detail(), Some("All 16 pages in the buffer pool are latched or dirty."));
            drop(held);
            assert_eq!(word(&pool.shared(PageId(17)).unwrap(), 0), 17);
        }
    }

    #[test]
    fn the_budget() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(1 << 20);
        let e =
            BufferPool::new(Arc::clone(&store), &memory, config(15, PoolMode::Table)).unwrap_err();
        assert_eq!(e.state(), SqlState::INVALID_PARAMETER_VALUE);
        let e =
            BufferPool::new(Arc::clone(&store), &memory, config(64, PoolMode::Table)).unwrap_err();
        assert_eq!(e.state(), SqlState::OUT_OF_MEMORY);
        let pool =
            BufferPool::new(Arc::clone(&store), &memory, config(32, PoolMode::Table)).unwrap();
        assert_eq!((memory.used(), pool.stats().reserved), (32 * FRAME_BYTES, 32 * FRAME_BYTES));
        drop(pool);
        assert_eq!(memory.used(), 0);
        assert_eq!(PoolConfig::default().shared_buffers, 128 << 20);
        assert_eq!(PoolMode::Window.to_string(), "window");
    }

    #[test]
    #[cfg_attr(not(all(unix, not(miri))), ignore = "the host has no window")]
    fn the_window_is_the_default() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(1 << 30);
        let pool = BufferPool::new(store, &memory, PoolConfig::new(16 * PAGE)).unwrap();
        assert_eq!(pool.mode(), PoolMode::Window);
        assert_eq!(memory.used(), 16 * PAGE);
        let e = pool.shared(PageId(u64::MAX - 1)).unwrap_err();
        assert_eq!(e.state(), SqlState::INSUFFICIENT_RESOURCES);
    }

    #[test]
    #[cfg_attr(not(all(unix, not(miri))), ignore = "the host has no window")]
    fn the_window_grows_and_gives_back() {
        let store = filled(200);
        let (memory, pool) = window(&store);
        assert_eq!(pool.stats().reserved, 16 * PAGE);
        // The pool grows to the budget and then evicts.
        for p in 1..=200 {
            drop(pool.shared(PageId(p)).unwrap());
        }
        let s = pool.stats();
        assert_eq!((s.reserved, s.resident, s.evictions), (128 * PAGE, 128, 72));
        // Another consumer takes memory, and the pool gives clean pages back.
        let query = memory.reserve(40 * PAGE).unwrap();
        let s = pool.stats();
        assert_eq!((s.reserved, s.resident), (88 * PAGE, 88));
        // The pool does not go below shared_buffers.
        assert!(memory.reserve(100 * PAGE).is_err());
        let s = pool.stats();
        assert_eq!((s.reserved, s.resident), (16 * PAGE, 16));
        assert_eq!(memory.used(), 56 * PAGE);
        drop(query);
        // The pages are read again after they were given back.
        for p in 1..=200 {
            assert_eq!(word(&pool.shared(PageId(p)).unwrap(), 0), p);
        }
        assert_eq!(pool.stats().resident, 128);
    }

    #[test]
    #[cfg_attr(not(all(unix, not(miri))), ignore = "the host has no window")]
    fn the_reclaimer_keeps_dirty_pages() {
        let store = filled(72);
        let (memory, pool) = window(&store);
        for p in 1..=40 {
            pool.exclusive(PageId(p)).unwrap()[8] = 1;
        }
        for p in 41..=72 {
            drop(pool.shared(PageId(p)).unwrap());
        }
        let s = pool.stats();
        assert_eq!((s.reserved, s.resident, s.dirty), (80 * PAGE, 72, 40));
        // The 32 clean pages go. The 40 dirty pages stay, so the request fails.
        let e = memory.reserve(100 * PAGE).unwrap_err();
        assert_eq!(e.state(), SqlState::OUT_OF_MEMORY);
        let s = pool.stats();
        assert_eq!((s.reserved, s.resident, s.dirty), (40 * PAGE, 40, 40));
        // After a flush the pages are clean and can go.
        assert_eq!(pool.flush().unwrap(), 40);
        let query = memory.reserve(100 * PAGE).unwrap();
        assert_eq!(pool.stats().reserved, 16 * PAGE);
        assert_eq!(store.get(PageId(40)).unwrap()[8], 1);
        drop(query);
        // A dropped pool gives nothing and does not fail the request.
        drop(pool);
        assert_eq!(memory.used(), 0);
        assert!(memory.reserve(128 * PAGE).is_ok());
    }

    /// Byte 16 of a page tells the filter what to do: 0 keeps the copy, 1 marks it at byte 24, and any other value skips the write.
    #[derive(Debug)]
    struct Marks;

    impl PageFilter for Marks {
        fn filter(&self, _: PageId, copy: &mut Page) -> Result<Filtered> {
            Ok(match copy[16] {
                0 => Filtered::Same,
                1 => {
                    copy[24] = 9;
                    Filtered::Changed
                }
                _ => Filtered::Skip,
            })
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[allow(clippy::disallowed_methods)]
    fn a_filter_changes_each_write() {
        for mode in modes() {
            let store = filled(40);
            let pool = pool(&store, FRAMES, mode);
            pool.set_filter(Arc::new(Marks)).unwrap();
            assert!(pool.set_filter(Arc::new(Marks)).is_err());
            for p in 1..=3 {
                let mut g = pool.exclusive(PageId(p)).unwrap();
                g[16] = (p - 1) as u8;
                g[8] = 5;
            }
            assert_eq!(pool.flush().unwrap(), 2, "{mode}");
            let stored = |p| {
                let page = store.get(PageId(p)).unwrap();
                (page[8], page[24])
            };
            // Page 1 is clean. Page 2 has the mark in the store only and stays dirty. Page 3 is not written.
            assert_eq!((stored(1), stored(2), stored(3)), ((5, 0), (5, 9), (0, 0)), "{mode}");
            assert_eq!(pool.stats().dirty, 2);
            // The clock does not evict the dirty pages, and it does not write them again.
            let writes = store.counts().1;
            for p in 4..=40 {
                drop(pool.shared(PageId(p)).unwrap());
            }
            assert_eq!(store.counts().1, writes, "{mode}");
            let misses = pool.stats().misses;
            assert_eq!(pool.shared(PageId(2)).unwrap()[24], 0);
            assert_eq!(pool.shared(PageId(3)).unwrap()[8], 5);
            assert_eq!((pool.stats().misses, pool.stats().dirty), (misses, 2), "{mode}");

            let e = pool.flush_all().unwrap_err();
            assert_eq!(e.state(), SqlState::INTERNAL_ERROR);
            pool.exclusive(PageId(3)).unwrap()[16] = 0;
            assert_eq!(pool.flush_all().unwrap(), 2);
            assert_eq!((stored(3), pool.stats().dirty), ((5, 0), 1), "{mode}");

            // flush_all waits for a dirty page that a writer holds, and writes it after the writer ends.
            pool.exclusive(PageId(5)).unwrap()[8] = 6;
            let mut g = pool.exclusive(PageId(5)).unwrap();
            g[8] = 7;
            let written = thread::scope(|s| {
                let flush = s.spawn(|| pool.flush_all().unwrap());
                for _ in 0..1_000 {
                    thread::yield_now();
                }
                drop(g);
                flush.join().unwrap()
            });
            assert_eq!((written, stored(5).0), (2, 7), "{mode}");
        }
    }

    // The test needs threads of the operating system, because the simulation runs one thread and does not test the latches. Miri reports the reads of an optimistic reader during a write as a data race. The race is the design of the seqlock. See `read_racy`.
    #[test]
    #[cfg_attr(miri, ignore)]
    #[allow(clippy::disallowed_methods)]
    fn threads() {
        const PAGES: u64 = 64;
        const THREADS: u64 = 4;
        const ROUNDS: u64 = 2000;
        for mode in modes() {
            let store = filled(PAGES);
            let pool = pool(&store, FRAMES, mode);
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
                            let n = word(&g, 64) + 1;
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
            let total: u64 = (1..=PAGES).map(|p| word(&store.get(PageId(p)).unwrap(), 64)).sum();
            assert_eq!(total, THREADS * ROUNDS, "{mode}");
        }
    }
}

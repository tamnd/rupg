//! `MemPages`, an implementation of [`PageAccess`] in memory, for tests of the tree and of the layers above it (spec/22 section 22.4).

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use rupg_common::{Error, Result};
use rupg_file::{OptimisticRead, PAGE_SIZE, Page, PageAccess, PageId, in_page};

static ZERO: Page = [0; PAGE_SIZE];

#[derive(Debug, Default)]
struct Cell {
    page: RwLock<Option<Box<Page>>>,
    version: AtomicU64,
}

/// Pages in memory with the latch modes of [`PageAccess`]. The number of pages is fixed when it is made. Logical page numbers start at 1.
#[derive(Debug)]
pub struct MemPages {
    cells: Box<[Cell]>,
    next: AtomicUsize,
    free: Mutex<Vec<usize>>,
}

impl MemPages {
    /// Room for `capacity` pages. A page takes memory only when it is allocated.
    pub fn new(capacity: usize) -> MemPages {
        MemPages {
            cells: (0..capacity).map(|_| Cell::default()).collect(),
            next: AtomicUsize::new(0),
            free: Mutex::new(Vec::new()),
        }
    }

    /// The number of pages that are allocated.
    pub fn allocated(&self) -> usize {
        let free = self.free.lock().unwrap_or_else(PoisonError::into_inner).len();
        self.next.load(Ordering::Acquire).min(self.cells.len()) - free
    }

    fn cell(&self, page: PageId) -> Result<&Cell> {
        let index = usize::try_from(page.0).ok().and_then(|n| n.checked_sub(1));
        index
            .and_then(|i| self.cells.get(i))
            .filter(|c| c.page.read().unwrap_or_else(PoisonError::into_inner).is_some())
            .ok_or_else(|| Error::internal(format!("the logical page {page} is not allocated")))
    }
}

/// An optimistic read of a [`MemPages`] page.
#[derive(Debug)]
pub struct MemOptimistic<'a> {
    cell: &'a Cell,
    page: PageId,
    seen: u64,
}

impl OptimisticRead for MemOptimistic<'_> {
    fn page(&self) -> PageId {
        self.page
    }

    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        if !in_page(at, out.len()) {
            return false;
        }
        let page = self.cell.page.read().unwrap_or_else(PoisonError::into_inner);
        let bytes: &Page = page.as_deref().unwrap_or(&ZERO);
        out.copy_from_slice(&bytes[at..at + out.len()]);
        true
    }

    fn is_valid(&self) -> bool {
        // A page that a writer holds is not valid, as in the buffer pool.
        match self.cell.page.try_read() {
            Ok(_) => self.cell.version.load(Ordering::Acquire) == self.seen,
            Err(TryLockError::Poisoned(_)) => false,
            Err(TryLockError::WouldBlock) => false,
        }
    }
}

/// A shared latch on a [`MemPages`] page.
#[derive(Debug)]
pub struct MemShared<'a>(RwLockReadGuard<'a, Option<Box<Page>>>);

impl Deref for MemShared<'_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        self.0.as_deref().unwrap_or(&ZERO)
    }
}

/// An exclusive latch on a [`MemPages`] page. The release moves the version of the page.
#[derive(Debug)]
pub struct MemExclusive<'a> {
    guard: RwLockWriteGuard<'a, Option<Box<Page>>>,
    version: &'a AtomicU64,
}

impl Deref for MemExclusive<'_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        self.guard.as_deref().unwrap_or(&ZERO)
    }
}

impl DerefMut for MemExclusive<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        self.guard.get_or_insert_with(|| Box::new([0; PAGE_SIZE]))
    }
}

impl Drop for MemExclusive<'_> {
    fn drop(&mut self) {
        // The guard is still held here, so a reader that sees the old version also sees the lock.
        self.version.fetch_add(1, Ordering::AcqRel);
    }
}

impl PageAccess for MemPages {
    type Optimistic<'a> = MemOptimistic<'a>;
    type Shared<'a> = MemShared<'a>;
    type Exclusive<'a> = MemExclusive<'a>;

    fn optimistic(&self, page: PageId) -> Result<MemOptimistic<'_>> {
        let cell = self.cell(page)?;
        loop {
            match cell.page.try_read() {
                Ok(guard) => {
                    let seen = cell.version.load(Ordering::Acquire);
                    drop(guard);
                    return Ok(MemOptimistic { cell, page, seen });
                }
                Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(Error::internal(format!("the logical page {page} is poisoned")));
                }
            }
        }
    }

    fn shared(&self, page: PageId) -> Result<MemShared<'_>> {
        let cell = self.cell(page)?;
        Ok(MemShared(cell.page.read().unwrap_or_else(PoisonError::into_inner)))
    }

    fn exclusive(&self, page: PageId) -> Result<MemExclusive<'_>> {
        let cell = self.cell(page)?;
        let guard = cell.page.write().unwrap_or_else(PoisonError::into_inner);
        Ok(MemExclusive { guard, version: &cell.version })
    }

    fn allocate(&self) -> Result<(PageId, MemExclusive<'_>)> {
        let reused = self.free.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let index = match reused {
            Some(i) => i,
            None => self.next.fetch_add(1, Ordering::AcqRel),
        };
        let cell = self.cells.get(index).ok_or_else(|| {
            Error::internal(format!("the memory pages are full at {} pages", self.cells.len()))
        })?;
        let mut guard = cell.page.write().unwrap_or_else(PoisonError::into_inner);
        *guard = Some(Box::new([0; PAGE_SIZE]));
        Ok((PageId(index as u64 + 1), MemExclusive { guard, version: &cell.version }))
    }

    fn free(&self, page: PageId) -> Result<()> {
        let cell = self.cell(page)?;
        *cell.page.write().unwrap_or_else(PoisonError::into_inner) = None;
        cell.version.fetch_add(1, Ordering::AcqRel);
        self.free.lock().unwrap_or_else(PoisonError::into_inner).push((page.0 - 1) as usize);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latches_and_versions() {
        let pages = MemPages::new(4);
        let (a, mut g) = pages.allocate().unwrap();
        assert_eq!(a, PageId(1));
        g[100] = 7;
        drop(g);
        let r = pages.optimistic(a).unwrap();
        assert_eq!((r.page(), r.u8_at(100)), (a, Some(7)));
        assert!(r.is_valid());
        drop(pages.shared(a).unwrap());
        assert!(r.is_valid());
        let mut w = pages.exclusive(a).unwrap();
        assert!(!r.is_valid());
        w[100] = 8;
        drop(w);
        assert!(!r.is_valid());
        assert_eq!(pages.shared(a).unwrap()[100], 8);

        let (b, _) = pages.allocate().unwrap();
        assert_eq!((b, pages.allocated()), (PageId(2), 2));
        pages.free(a).unwrap();
        assert!(pages.shared(a).is_err() && pages.optimistic(PageId(0)).is_err());
        let (c, g) = pages.allocate().unwrap();
        assert_eq!((c, g[100]), (a, 0));
        drop(g);
        pages.allocate().unwrap();
        pages.allocate().unwrap();
        assert!(pages.allocate().is_err());
    }
}

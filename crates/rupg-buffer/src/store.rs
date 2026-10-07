//! The `Store` trait: where the buffer pool reads pages from and writes them to.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use rupg_common::{Error, Result};
use rupg_file::{PAGE_SIZE, Page, PageId};

/// The durable home of the pages. The buffer pool calls it on a miss, on a write back and to allocate and free logical page numbers.
///
/// The store of the file maps a logical page number to a physical page through the page table, checks each page that it reads (spec/08 section 8.3.1) and writes each page out of place (section 8.6.1). [`MemStore`] keeps the pages in memory.
pub trait Store: Send + Sync + fmt::Debug {
    /// Reads `page` into `into`.
    fn read(&self, page: PageId, into: &mut Page) -> Result<()>;

    /// Writes a copy of `page`.
    fn write(&self, page: PageId, from: &Page) -> Result<()>;

    /// Gives a logical page number that is not in use.
    fn allocate(&self) -> Result<PageId>;

    /// Gives back a logical page number.
    fn free(&self, page: PageId) -> Result<()>;
}

/// A store in memory. It is for tests and for a database that is not in a file.
#[derive(Debug, Default)]
pub struct MemStore {
    inner: Mutex<MemPages>,
}

#[derive(Debug, Default)]
struct MemPages {
    pages: HashMap<u64, Box<Page>>,
    free: Vec<u64>,
    next: u64,
    reads: u64,
    writes: u64,
}

impl MemStore {
    /// An empty store.
    pub fn new() -> MemStore {
        MemStore::default()
    }

    /// The number of reads and the number of writes since the store was made.
    pub fn counts(&self) -> (u64, u64) {
        let inner = self.lock();
        (inner.reads, inner.writes)
    }

    /// A copy of the stored bytes of `page`, or `None` if the page is not allocated.
    pub fn get(&self, page: PageId) -> Option<Box<Page>> {
        self.lock().pages.get(&page.0).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemPages> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn not_allocated(page: PageId) -> Error {
    Error::internal(format!("logical page {page} is not allocated"))
}

impl Store for MemStore {
    fn read(&self, page: PageId, into: &mut Page) -> Result<()> {
        let mut inner = self.lock();
        inner.reads += 1;
        into.copy_from_slice(&inner.pages.get(&page.0).ok_or_else(|| not_allocated(page))?[..]);
        Ok(())
    }

    fn write(&self, page: PageId, from: &Page) -> Result<()> {
        let mut inner = self.lock();
        inner.writes += 1;
        inner.pages.get_mut(&page.0).ok_or_else(|| not_allocated(page))?.copy_from_slice(from);
        Ok(())
    }

    fn allocate(&self) -> Result<PageId> {
        let mut inner = self.lock();
        let n = match inner.free.pop() {
            Some(n) => n,
            None => {
                // Logical page 0 is not used, so that 0 can mean no page in a pointer.
                inner.next += 1;
                inner.next
            }
        };
        inner.pages.insert(n, Box::new([0u8; PAGE_SIZE]));
        Ok(PageId(n))
    }

    fn free(&self, page: PageId) -> Result<()> {
        let mut inner = self.lock();
        inner.pages.remove(&page.0).ok_or_else(|| not_allocated(page))?;
        inner.free.push(page.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    #[test]
    fn mem_store() {
        let s = MemStore::new();
        let a = s.allocate().unwrap();
        let b = s.allocate().unwrap();
        assert_eq!((a, b), (PageId(1), PageId(2)));
        let mut page = Box::new([7u8; PAGE_SIZE]);
        s.write(a, &page).unwrap();
        s.read(b, &mut page).unwrap();
        assert_eq!(page[0], 0);
        s.read(a, &mut page).unwrap();
        assert_eq!(page[PAGE_SIZE - 1], 7);
        assert_eq!(s.counts(), (2, 1));
        s.free(a).unwrap();
        assert_eq!(s.read(a, &mut page).unwrap_err().state(), SqlState::INTERNAL_ERROR);
        assert!(s.free(a).is_err());
        assert!(s.get(a).is_none());
        assert_eq!(s.allocate().unwrap(), a);
        assert_eq!(s.get(a).unwrap()[0], 0);
    }
}

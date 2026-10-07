//! The `PageAccess` trait: how the layers above the buffer manager read and change 16 KiB pages by logical page number.
//!
//! rupg-buffer implements the trait for the file (spec/08 section 8.8). rupg-tree and the layers above it use only the trait, so a test can run them on pages in memory with no buffer manager (spec/22 section 22.4).

use std::ops::{Deref, DerefMut};

use rupg_common::Result;

use crate::{PAGE_SIZE, Page, PageId};

/// Access to 16 KiB pages by logical page number, with the three latch modes of spec/08 section 8.8.2.
///
/// A guard releases its latch when it is dropped. An exclusive guard marks the page dirty.
pub trait PageAccess: Send + Sync {
    /// A read with no latch. See [`OptimisticRead`].
    type Optimistic<'a>: OptimisticRead
    where
        Self: 'a;

    /// A shared latch. Other readers can hold the page at the same time. No writer can.
    type Shared<'a>: Deref<Target = Page>
    where
        Self: 'a;

    /// An exclusive latch. No other latch can be held on the page at the same time.
    type Exclusive<'a>: DerefMut<Target = Page>
    where
        Self: 'a;

    /// Starts an optimistic read of `page`. The call waits while a writer holds the page.
    fn optimistic(&self, page: PageId) -> Result<Self::Optimistic<'_>>;

    /// Takes a shared latch on `page`.
    fn shared(&self, page: PageId) -> Result<Self::Shared<'_>>;

    /// Takes an exclusive latch on `page`.
    fn exclusive(&self, page: PageId) -> Result<Self::Exclusive<'_>>;

    /// Allocates a new logical page and gives it with an exclusive latch. All bytes of the new page are zero. The caller writes the page header.
    fn allocate(&self) -> Result<(PageId, Self::Exclusive<'_>)>;

    /// Frees `page`. The caller must hold no latch on it. The logical page number can come back from a later [`PageAccess::allocate`].
    fn free(&self, page: PageId) -> Result<()>;
}

/// An optimistic read: the reader takes no latch, reads the bytes, and then checks that no writer changed the page.
///
/// A writer can change the page during the read, so a value that the reader gets can be wrong. The reader must call [`OptimisticRead::is_valid`] before it acts on a value, and it must start again if the result is false. The reads give `None` for a range outside the page, so a wrong offset does not cause a panic.
pub trait OptimisticRead {
    /// The logical page number.
    fn page(&self) -> PageId;

    /// Copies the bytes at `at` into `out`. Gives false if the range is outside the page.
    fn read(&self, at: usize, out: &mut [u8]) -> bool;

    /// True if no writer changed the page after the read started.
    fn is_valid(&self) -> bool;

    /// The byte at `at`.
    fn u8_at(&self, at: usize) -> Option<u8> {
        self.array::<1>(at).map(|b| b[0])
    }

    /// The little endian `u16` at `at`.
    fn u16_at(&self, at: usize) -> Option<u16> {
        self.array(at).map(u16::from_le_bytes)
    }

    /// The little endian `u32` at `at`.
    fn u32_at(&self, at: usize) -> Option<u32> {
        self.array(at).map(u32::from_le_bytes)
    }

    /// The little endian `u64` at `at`.
    fn u64_at(&self, at: usize) -> Option<u64> {
        self.array(at).map(u64::from_le_bytes)
    }

    /// The `N` bytes at `at`.
    fn array<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        let mut out = [0u8; N];
        self.read(at, &mut out).then_some(out)
    }
}

/// True if the range of `len` bytes at `at` is inside a page. Implementations of [`OptimisticRead::read`] use it.
pub fn in_page(at: usize, len: usize) -> bool {
    at.checked_add(len).is_some_and(|end| end <= PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain(Box<Page>);

    impl OptimisticRead for Plain {
        fn page(&self) -> PageId {
            PageId(1)
        }

        fn read(&self, at: usize, out: &mut [u8]) -> bool {
            if !in_page(at, out.len()) {
                return false;
            }
            out.copy_from_slice(&self.0[at..at + out.len()]);
            true
        }

        fn is_valid(&self) -> bool {
            true
        }
    }

    #[test]
    fn reads() {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        page[100..108].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        let r = Plain(page);
        assert_eq!(r.u64_at(100), Some(0x0102_0304_0506_0708));
        assert_eq!(r.u32_at(100), Some(0x0506_0708));
        assert_eq!(r.u16_at(104), Some(0x0304));
        assert_eq!(r.u8_at(107), Some(1));
        assert_eq!(r.u64_at(PAGE_SIZE - 8), Some(0));
        assert_eq!(r.u64_at(PAGE_SIZE - 7), None);
        assert_eq!(r.u8_at(usize::MAX), None);
        assert!(in_page(0, PAGE_SIZE));
        assert!(!in_page(1, PAGE_SIZE));
    }
}

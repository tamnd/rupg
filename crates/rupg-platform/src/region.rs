//! `Region`, a reservation of virtual address space with no physical memory behind it until a page is written (spec/08 section 8.8.1).

use std::ptr::NonNull;

use rupg_common::{Error, Result};

/// A range of virtual address space that reads and writes as normal memory. All bytes are zero at the start. The operating system gives a physical page only when a page is first touched, and [`Region::discard`] gives it back.
///
/// The region is unmapped when it is dropped.
#[derive(Debug)]
pub struct Region {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: `Region` owns its mapping. The owner controls each access to the bytes.
unsafe impl Send for Region {}
// SAFETY: as for `Send`. A shared reference gives only raw pointers.
unsafe impl Sync for Region {}

impl Region {
    /// Reserves `len` bytes. A host with no virtual memory reservation gives SQLSTATE `0A000`. A reservation that the operating system refuses gives `53200`.
    pub fn reserve(len: usize) -> Result<Region> {
        if len == 0 {
            return Err(Error::internal("a region of 0 bytes"));
        }
        sys::reserve(len).map(|base| Region { base, len })
    }

    /// The size of a page of the operating system. [`Region::discard`] works on whole pages of this size.
    pub fn os_page_size() -> usize {
        sys::page_size()
    }

    /// The first byte.
    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// The size in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false, because a region has at least 1 byte.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Gives the physical memory of a range back to the operating system. The range stays mapped, and its bytes are zero after the call.
    ///
    /// # Safety
    ///
    /// No reference to a byte of the range may exist during the call. `offset` and `len` must be multiples of [`Region::os_page_size`], and the range must be inside the region.
    pub unsafe fn discard(&self, offset: usize, len: usize) -> Result<()> {
        debug_assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
        debug_assert!(
            offset.is_multiple_of(sys::page_size()) && len.is_multiple_of(sys::page_size())
        );
        // SAFETY: the caller gives a range inside the mapping, so the pointer is inside it.
        let start = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: the caller makes sure that no reference to the range exists.
        unsafe { sys::discard(start, len) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: `base` and `len` are the mapping that `reserve` made, and the region is not used after this.
        unsafe { sys::release(self.base.as_ptr(), self.len) };
    }
}

#[cfg(all(unix, not(miri)))]
mod sys {
    use std::io;
    use std::ptr::{self, NonNull};

    use rupg_common::SqlState;

    use super::{Error, Result};

    fn refused(len: usize, why: impl std::fmt::Display) -> Error {
        Error::new(SqlState::OUT_OF_MEMORY, "out of memory")
            .with_detail(format!("Failed to reserve {len} bytes of address space: {why}."))
    }

    pub(super) fn page_size() -> usize {
        // SAFETY: sysconf reads a system value and has no pointer argument.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(size).unwrap_or(4096)
    }

    const PROT: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
    const FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE;

    pub(super) fn reserve(len: usize) -> Result<NonNull<u8>> {
        // SAFETY: a new anonymous mapping at an address that the kernel picks does not touch memory that Rust owns.
        let p = unsafe { libc::mmap(ptr::null_mut(), len, PROT, FLAGS, -1, 0) };
        if p == libc::MAP_FAILED {
            return Err(refused(len, io::Error::last_os_error()));
        }
        NonNull::new(p.cast()).ok_or_else(|| refused(len, "mmap gave a null address"))
    }

    /// # Safety
    ///
    /// The range must be inside one mapping of `reserve`, and no reference to it may exist.
    pub(super) unsafe fn discard(start: *mut u8, len: usize) -> Result<()> {
        // On Linux, MADV_DONTNEED frees the pages of a private anonymous mapping at once, and the next read gives zero bytes.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: the caller gives a range of a mapping of `reserve` with no reference to it.
        let ok = unsafe { libc::madvise(start.cast(), len, libc::MADV_DONTNEED) } == 0;
        // Other systems can keep the pages after MADV_DONTNEED. A new mapping over the range frees them and gives zero bytes. MAP_FIXED replaces the old pages in one step, so the range is never unmapped.
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: as above. MAP_FIXED replaces only the pages of the range.
        let ok = unsafe { libc::mmap(start.cast(), len, PROT, FLAGS | libc::MAP_FIXED, -1, 0) }
            == start.cast();
        if ok {
            Ok(())
        } else {
            Err(Error::internal(format!(
                "could not discard {len} bytes of the buffer pool: {}",
                io::Error::last_os_error()
            )))
        }
    }

    /// # Safety
    ///
    /// The range must be a mapping of `reserve` that is not used after the call.
    pub(super) unsafe fn release(start: *mut u8, len: usize) {
        // SAFETY: the caller gives a whole mapping of `reserve`.
        unsafe { libc::munmap(start.cast(), len) };
    }
}

// Windows, WebAssembly and Miri do not reserve address space here. The buffer pool uses its table of frames there (spec/08 section 8.8.4).
#[cfg(not(all(unix, not(miri))))]
mod sys {
    use std::ptr::NonNull;

    use rupg_common::{Error, SqlState};

    use super::Result;

    pub(super) fn page_size() -> usize {
        4096
    }

    pub(super) fn reserve(len: usize) -> Result<NonNull<u8>> {
        Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "this host cannot reserve address space")
            .with_detail(format!("Failed to reserve {len} bytes.")))
    }

    pub(super) unsafe fn discard(_start: *mut u8, _len: usize) -> Result<()> {
        Err(Error::internal("discard on a host with no regions"))
    }

    pub(super) unsafe fn release(_start: *mut u8, _len: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    #[test]
    #[cfg_attr(not(all(unix, not(miri))), ignore = "the host has no regions")]
    fn reserve_and_discard() {
        let page = Region::os_page_size();
        // 1 GiB of address space costs no memory until it is written.
        let r = Region::reserve(1 << 30).unwrap();
        assert_eq!(r.len(), 1 << 30);
        assert_eq!(r.as_ptr() as usize % page, 0);
        let p = r.as_ptr();
        // SAFETY: the offsets are inside the region and no reference to the bytes exists.
        unsafe {
            p.add(5).write(1);
            p.add(page + 7).write(2);
            p.add((1 << 30) - 1).write(3);
            r.discard(0, page).unwrap();
            assert_eq!((p.add(5).read(), p.add(page + 7).read()), (0, 2));
            p.add(5).write(4);
            assert_eq!(p.add(5).read(), 4);
            r.discard(0, 1 << 30).unwrap();
            assert_eq!(
                (p.add(5).read(), p.add(page + 7).read(), p.add((1 << 30) - 1).read()),
                (0, 0, 0)
            );
        }
    }

    #[test]
    fn refusals() {
        assert_eq!(Region::reserve(0).unwrap_err().state(), SqlState::INTERNAL_ERROR);
        let e = Region::reserve(usize::MAX - 4095).unwrap_err();
        assert!(
            matches!(e.state(), SqlState::OUT_OF_MEMORY | SqlState::FEATURE_NOT_SUPPORTED),
            "{e}"
        );
    }
}

//! `FileMap`, a mapping of a file that only reads, which gives the bytes of the file where the page cache holds them.
//!
//! The store writes its image to a new file and renames it over the old one, and it never writes to an image in place or makes it shorter. A mapping of an image therefore sees bytes that do not change, also after the next checkpoint, because the old file lives until its last mapping goes.

use std::fs;

use rupg_common::bytes::Mapping;

/// The bytes of a whole file, mapped to read only. The mapping goes when the value is dropped.
#[derive(Debug)]
pub struct FileMap {
    base: *const u8,
    len: usize,
}

// SAFETY: the mapping only reads and does not belong to a thread, so any thread can read it and unmap it.
unsafe impl Send for FileMap {}
// SAFETY: as for `Send`. A shared reference gives only shared slices of bytes that do not change.
unsafe impl Sync for FileMap {}

impl FileMap {
    /// The mapping of the first `len` bytes of `file`, or `None` for an empty file or a host that does not map files.
    pub(crate) fn new(file: &fs::File, len: u64) -> Option<FileMap> {
        let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
        sys::map(file, len).map(|base| FileMap { base, len })
    }
}

impl Mapping for FileMap {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `base..base + len` is a live mapping that only reads, of a file that no one writes in place or makes shorter while it is mapped (see the module comment).
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }

    fn release(&self, at: usize, len: usize) {
        let end = at.saturating_add(len).min(self.len);
        if at < end {
            sys::release(self.base, at, end);
        }
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        sys::unmap(self.base, self.len);
    }
}

#[cfg(all(unix, not(miri)))]
mod sys {
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::ptr;

    pub(super) fn map(file: &fs::File, len: usize) -> Option<*const u8> {
        // SAFETY: a new mapping at an address that the kernel picks, of a descriptor that is open during the call. It does not touch memory that Rust owns.
        let p = unsafe {
            libc::mmap(ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        (p != libc::MAP_FAILED && !p.is_null()).then_some(p.cast_const().cast())
    }

    pub(super) fn release(base: *const u8, at: usize, end: usize) {
        // SAFETY: sysconf reads a system value and has no pointer argument.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
        // The mapping starts on a page, so the start goes down to a page inside the mapping. A page that the range shares with the bytes before it goes too, and a later read maps it again.
        let from = at & !(page - 1);
        // SAFETY: `from..end` is inside the mapping. MADV_DONTNEED on a shared mapping of a file drops the pages of the process and keeps the bytes of the file, so a later read gives the same bytes.
        unsafe { libc::madvise(base.add(from).cast_mut().cast(), end - from, libc::MADV_DONTNEED) };
    }

    pub(super) fn unmap(base: *const u8, len: usize) {
        // SAFETY: `base` and `len` are a whole mapping of `map`, which is not used after this.
        unsafe { libc::munmap(base.cast_mut().cast(), len) };
    }
}

// Other hosts read the image into memory.
#[cfg(not(all(unix, not(miri))))]
mod sys {
    use std::fs;

    pub(super) fn map(_file: &fs::File, _len: usize) -> Option<*const u8> {
        None
    }

    pub(super) fn release(_base: *const u8, _at: usize, _end: usize) {}

    pub(super) fn unmap(_base: *const u8, _len: usize) {}
}

//! The memory of the frames: one allocation of `count` pages, aligned to 4096 bytes.

use std::alloc::{self, Layout};
use std::ptr::NonNull;

use rupg_common::{Error, Result, SqlState};
use rupg_file::{PAGE_SIZE, Page};

/// The alignment of the frames. Direct I/O needs buffers that are aligned to the block size.
const ALIGN: usize = 4096;

/// The page memory of the frames. It is freed when the pool is dropped and never before, so a pointer to a frame stays valid while the pool lives.
#[derive(Debug)]
pub(crate) struct FrameMemory {
    base: NonNull<u8>,
    count: usize,
}

// SAFETY: `FrameMemory` owns its allocation. The pool controls each access to the bytes with the latch of the frame.
unsafe impl Send for FrameMemory {}
// SAFETY: as for `Send`. A shared reference gives only raw pointers.
unsafe impl Sync for FrameMemory {}

impl FrameMemory {
    /// Allocates `count` frames with all bytes zero.
    pub(crate) fn new(count: usize) -> Result<FrameMemory> {
        let layout = FrameMemory::layout(count)?;
        // SAFETY: the layout has a size of at least one page, because `layout` refuses a count of 0.
        let base = unsafe { alloc::alloc_zeroed(layout) };
        let base = NonNull::new(base).ok_or_else(|| {
            Error::new(SqlState::OUT_OF_MEMORY, "out of memory").with_detail(format!(
                "Failed to allocate {} bytes for the buffer pool.",
                layout.size()
            ))
        })?;
        Ok(FrameMemory { base, count })
    }

    fn layout(count: usize) -> Result<Layout> {
        count
            .checked_mul(PAGE_SIZE)
            .filter(|&size| size > 0)
            .and_then(|size| Layout::from_size_align(size, ALIGN).ok())
            .ok_or_else(|| Error::internal(format!("bad count of buffer frames: {count}")))
    }

    /// A pointer to frame `index`. The caller must hold the latch that allows the access that it makes through the pointer.
    pub(crate) fn page(&self, index: usize) -> *mut Page {
        assert!(index < self.count, "frame {index} of {}", self.count);
        // SAFETY: `index` is less than `count`, so the offset is inside the allocation.
        unsafe { self.base.as_ptr().add(index * PAGE_SIZE).cast::<Page>() }
    }
}

impl Drop for FrameMemory {
    fn drop(&mut self) {
        // `new` made the same layout without an error, so `layout` does not fail here.
        if let Ok(layout) = FrameMemory::layout(self.count) {
            // SAFETY: `base` came from `alloc_zeroed` with this layout and is freed only here.
            unsafe { alloc::dealloc(self.base.as_ptr(), layout) };
        }
    }
}

/// Copies `out.len()` bytes at `at` from `page` with volatile reads. A writer can change the page during the copy, so the bytes can be torn. The optimistic reader finds this with the state word and discards them.
///
/// Rust has no atomic copy of bytes yet (RFC 3301). The crossbeam seqlock uses volatile reads in the same way. A volatile read does not create a reference to the bytes, so the compiler does not assume that they stay the same.
///
/// # Safety
///
/// `page` must point to a frame of a live `FrameMemory`, and `at + out.len()` must be at most `PAGE_SIZE`.
pub(crate) unsafe fn read_racy(page: *const Page, at: usize, out: &mut [u8]) {
    let src = page.cast::<u8>();
    for (i, b) in out.iter_mut().enumerate() {
        // SAFETY: the caller gives a live frame and a range inside it.
        *b = unsafe { src.add(at + i).read_volatile() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let m = FrameMemory::new(3).unwrap();
        let p = m.page(2);
        assert_eq!(p as usize % ALIGN, 0);
        // SAFETY: no other reference to frame 2 exists in this test.
        let page = unsafe { &mut *p };
        assert!(page.iter().all(|&b| b == 0));
        page[PAGE_SIZE - 2] = 9;
        let mut out = [1u8; 2];
        // SAFETY: frame 2 is live and the range is inside it.
        unsafe { read_racy(p, PAGE_SIZE - 2, &mut out) };
        assert_eq!(out, [9, 0]);
        assert!(FrameMemory::new(0).is_err());
        assert!(FrameMemory::new(usize::MAX).is_err());
    }

    #[test]
    #[should_panic(expected = "frame 3 of 3")]
    fn out_of_range() {
        FrameMemory::new(3).unwrap().page(3);
    }
}

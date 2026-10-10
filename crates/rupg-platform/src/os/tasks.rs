//! Threads of the operating system.

use std::thread;
use std::time::Duration;

use rupg_common::{Error, Result, SqlState};

use crate::tasks::{Join, Task, TaskHandle, Tasks};

/// The stack of each thread. It is the stack of the main thread on Linux, which is the stack of a PostgreSQL backend. The parser of a debug build needs more than the 2 MiB that Rust gives a new thread.
pub const STACK_SIZE: usize = 8 << 20;

/// Runs each task on its own thread of the operating system, with a stack of [`STACK_SIZE`] bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsTasks;

#[derive(Debug)]
struct OsJoin(thread::JoinHandle<()>);

impl Join for OsJoin {
    fn join(self: Box<Self>) -> Result<()> {
        let name = self.0.thread().name().unwrap_or("unnamed").to_string();
        self.0.join().map_err(|_| Error::internal(format!("the task \"{name}\" panicked")))
    }

    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Tasks for OsTasks {
    fn spawn(&self, name: &str, task: Task) -> Result<TaskHandle> {
        let handle = thread::Builder::new()
            .name(name.to_string())
            .stack_size(STACK_SIZE)
            .spawn(task)
            .map_err(|e| {
                Error::new(
                    SqlState::INSUFFICIENT_RESOURCES,
                    format!("could not start the thread \"{name}\": {e}"),
                )
            })?;
        Ok(TaskHandle::new(Box::new(OsJoin(handle))))
    }

    fn sleep(&self, d: Duration) {
        thread::sleep(d);
    }

    fn yield_now(&self) {
        thread::yield_now();
    }

    fn parallelism(&self) -> usize {
        thread::available_parallelism().map_or(1, std::num::NonZero::get)
    }

    #[cfg(target_os = "linux")]
    fn trim_stack(&self) {
        stack::trim();
    }
}

/// The release of the stack pages that a thread does not use now. Only Linux has it.
#[cfg(target_os = "linux")]
mod stack {
    use std::cell::Cell;
    use std::mem::MaybeUninit;
    use std::ptr;

    /// The bytes below the current frame that stay in memory. They hold the frames of the call to `madvise`.
    const KEEP: usize = 16 << 10;

    thread_local! {
        /// The lowest address of the stack of this thread. It is 0 before the first call, and `usize::MAX` when the system does not give it.
        static LOW: Cell<usize> = const { Cell::new(0) };
    }

    /// Gives back the pages of the stack below the current frame, less [`KEEP`] bytes. The next use of these pages gets new pages of zeros.
    #[inline(never)]
    pub(super) fn trim() {
        let here = 0u8;
        let top = ptr::from_ref(&here).addr();
        let low = LOW.with(|low| {
            if low.get() == 0 {
                low.set(stack_low().unwrap_or(usize::MAX));
            }
            low.get()
        });
        let page = page_size();
        // The first page above the lowest address is not released. It can be a guard page.
        let start = low.saturating_add(2 * page - 1) & !(page - 1);
        let end = top.saturating_sub(KEEP) & !(page - 1);
        if start < end {
            // SAFETY: the range is in the stack of this thread and below the frames that the thread uses now, so no live data is in it. The result is only a hint, so an error is not a problem.
            unsafe {
                libc::madvise(ptr::without_provenance_mut(start), end - start, libc::MADV_DONTNEED)
            };
        }
    }

    /// The lowest address of the stack of this thread, from `pthread_getattr_np`.
    fn stack_low() -> Option<usize> {
        let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
        // SAFETY: `pthread_getattr_np` fills `attr` when it gives 0, and `pthread_attr_destroy` releases it after the read.
        unsafe {
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                return None;
            }
            let mut addr = ptr::null_mut();
            let mut size = 0;
            let got = libc::pthread_attr_getstack(attr.as_ptr(), &raw mut addr, &raw mut size);
            libc::pthread_attr_destroy(attr.as_mut_ptr());
            (got == 0 && size > 0).then(|| addr.addr())
        }
    }

    /// The size of a page of memory.
    fn page_size() -> usize {
        // SAFETY: `sysconf` only reads a value of the system.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(size).ok().filter(|s| s.is_power_of_two()).unwrap_or(4096)
    }

    #[cfg(test)]
    mod tests {
        use std::hint::black_box;

        use super::*;
        use crate::Tasks;
        use crate::os::OsTasks;

        /// Fills about `depth` KiB of the stack, and gives the address of the deepest frame.
        #[inline(never)]
        fn deep(depth: usize) -> usize {
            let frame = black_box([1u8; 1024]);
            let at = ptr::from_ref(&frame).addr();
            if depth == 0 { at } else { black_box(deep(depth - 1)).min(at) }
        }

        /// True when the page at `addr` is in memory.
        fn resident(addr: usize) -> bool {
            let page = page_size();
            let mut vec = 0u8;
            // SAFETY: `mincore` writes one byte for the one page of the range.
            let got = unsafe {
                libc::mincore(ptr::without_provenance_mut(addr & !(page - 1)), page, &raw mut vec)
            };
            got == 0 && vec & 1 == 1
        }

        #[test]
        fn trim_releases_the_pages_below_the_frame() {
            let task = OsTasks.spawn(
                "trim",
                Box::new(|| {
                    let deepest = deep(1024);
                    assert!(resident(deepest));
                    OsTasks.trim_stack();
                    assert!(!resident(deepest));
                    // The released pages work again, with new contents.
                    assert_eq!(deep(1024), deepest);
                    assert!(resident(deepest));
                }),
            );
            task.unwrap().join().unwrap();
        }
    }
}

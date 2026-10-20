//! Threads of the operating system.
//!
//! A thread that ends a task waits for [`KEEP_IDLE`] for the next task with the same name, and runs it. A new thread and its end cost more than the startup of a session: the system makes the stack and the signal stack, and at the end it gives back the pages of the stack, so the next thread gets each page again with a fault. A connection that only logs in and ends spent most of its time there (spec/06 section 6.18).

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use rupg_common::{Error, Result, SqlState};

use crate::tasks::{Join, Task, TaskHandle, Tasks};

/// The stack of each thread. It is the stack of the main thread on Linux, which is the stack of a PostgreSQL backend. The parser of a debug build needs more than the 2 MiB that Rust gives a new thread.
pub const STACK_SIZE: usize = 8 << 20;

/// How long a thread with no task waits for one before it ends.
const KEEP_IDLE: Duration = Duration::from_secs(10);

/// The most threads that wait for a task. A thread after that one ends at the end of its task.
const MAX_IDLE: usize = 64;

/// The threads that wait for a task.
static IDLE: Mutex<Vec<Arc<Worker>>> = Mutex::new(Vec::new());

/// Runs each task on a thread of the operating system with a stack of [`STACK_SIZE`] bytes. A task gets a thread that waits for a task with the same name, or a new thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsTasks;

/// One thread and the next task that it runs.
struct Worker {
    name: String,
    next: Mutex<Option<(Task, Arc<Done>)>>,
    wake: Condvar,
}

/// The end of one task: `Some(true)` when it panicked.
#[derive(Debug, Default)]
struct Done {
    state: Mutex<Option<bool>>,
    ended: Condvar,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").field("name", &self.name).finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The loop of a thread: the task, then the wait for the next one.
fn work(worker: &Arc<Worker>) {
    loop {
        let Some((task, done)) = lock(&worker.next).take() else { return };
        let panicked = panic::catch_unwind(AssertUnwindSafe(task)).is_err();
        // A thread whose task panicked ends, so that its thread-local values go too. The other threads go to the list before the task ends for `join`, so a task that starts after the `join` of the last task gets this thread.
        let keep = !panicked && {
            let mut idle = lock(&IDLE);
            let room = idle.len() < MAX_IDLE;
            if room {
                idle.push(Arc::clone(worker));
            }
            room
        };
        *lock(&done.state) = Some(panicked);
        done.ended.notify_all();
        drop(done);
        if !keep {
            return;
        }
        let mut next = lock(&worker.next);
        while next.is_none() {
            let (guard, wait) =
                worker.wake.wait_timeout(next, KEEP_IDLE).unwrap_or_else(PoisonError::into_inner);
            next = guard;
            if wait.timed_out() && next.is_none() {
                drop(next);
                let mut idle = lock(&IDLE);
                if let Some(at) = idle.iter().position(|w| Arc::ptr_eq(w, worker)) {
                    idle.swap_remove(at);
                    return;
                }
                drop(idle);
                // `spawn` took this thread and gives it a task now.
                next = lock(&worker.next);
            }
        }
    }
}

#[derive(Debug)]
struct OsJoin {
    name: String,
    done: Arc<Done>,
}

impl Join for OsJoin {
    fn join(self: Box<Self>) -> Result<()> {
        let mut state = lock(&self.done.state);
        while state.is_none() {
            state = self.done.ended.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        match *state {
            Some(true) => Err(Error::internal(format!("the task \"{}\" panicked", self.name))),
            _ => Ok(()),
        }
    }

    fn is_finished(&self) -> bool {
        lock(&self.done.state).is_some()
    }
}

impl Tasks for OsTasks {
    fn spawn(&self, name: &str, task: Task) -> Result<TaskHandle> {
        let done = Arc::new(Done::default());
        let job = (task, Arc::clone(&done));
        let waiting = {
            let mut idle = lock(&IDLE);
            idle.iter().rposition(|w| w.name == name).map(|at| idle.swap_remove(at))
        };
        if let Some(worker) = waiting {
            *lock(&worker.next) = Some(job);
            worker.wake.notify_one();
        } else {
            let worker = Arc::new(Worker {
                name: name.to_string(),
                next: Mutex::new(Some(job)),
                wake: Condvar::new(),
            });
            thread::Builder::new()
                .name(name.to_string())
                .stack_size(STACK_SIZE)
                .spawn(move || work(&worker))
                .map_err(|e| {
                    Error::new(
                        SqlState::INSUFFICIENT_RESOURCES,
                        format!("could not start the thread \"{name}\": {e}"),
                    )
                })?;
        }
        Ok(TaskHandle::new(Box::new(OsJoin { name: name.to_string(), done })))
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
        #[cfg_attr(miri, ignore = "Miri does not support mincore")]
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::mpsc;

    use super::*;

    /// Runs a task with this name, and gives the thread that ran it.
    fn id_of(name: &str) -> thread::ThreadId {
        let (send, receive) = mpsc::channel();
        let task = OsTasks.spawn(
            name,
            Box::new(move || {
                send.send(thread::current().id()).unwrap();
            }),
        );
        task.unwrap().join().unwrap();
        receive.recv().unwrap()
    }

    #[test]
    fn a_thread_runs_the_next_task_with_its_name() {
        let first = id_of("rupg-reuse");
        assert_ne!(first, thread::current().id());
        assert_eq!(id_of("rupg-reuse"), first);
        assert_eq!(id_of("rupg-reuse"), first);
        assert_ne!(id_of("rupg-other"), first);
    }

    #[test]
    fn a_thread_whose_task_panicked_ends() {
        let before = id_of("rupg-panics");
        let (send, receive) = mpsc::channel();
        let bad = OsTasks.spawn(
            "rupg-panics",
            Box::new(move || {
                send.send(thread::current().id()).unwrap();
                panic!("a test panic");
            }),
        );
        assert!(bad.unwrap().join().unwrap_err().message().contains("rupg-panics"));
        assert_eq!(receive.recv().unwrap(), before);
        assert_ne!(id_of("rupg-panics"), before);
    }

    #[test]
    fn tasks_at_the_same_time_have_their_own_threads() {
        let (send, receive) = mpsc::channel();
        let go = Arc::new((Mutex::new(false), Condvar::new()));
        let tasks: Vec<_> = (0..3)
            .map(|_| {
                let (send, go) = (send.clone(), Arc::clone(&go));
                let task = Box::new(move || {
                    send.send(thread::current().id()).unwrap();
                    let mut started = lock(&go.0);
                    while !*started {
                        started = go.1.wait(started).unwrap();
                    }
                });
                OsTasks.spawn("rupg-together", task).unwrap()
            })
            .collect();
        let ids: HashSet<_> = (0..3).map(|_| receive.recv().unwrap()).collect();
        assert!(tasks.iter().all(|t| !t.is_finished()));
        *lock(&go.0) = true;
        go.1.notify_all();
        for task in tasks {
            task.join().unwrap();
        }
        assert_eq!(ids.len(), 3);
    }
}

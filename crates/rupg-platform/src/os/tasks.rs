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
}

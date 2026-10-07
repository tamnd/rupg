//! Threads of the operating system.

use std::thread;
use std::time::Duration;

use rupg_common::{Error, Result, SqlState};

use crate::tasks::{Join, Task, TaskHandle, Tasks};

/// Runs each task on its own thread of the operating system.
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
        let handle = thread::Builder::new().name(name.to_string()).spawn(task).map_err(|e| {
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

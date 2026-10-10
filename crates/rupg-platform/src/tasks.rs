//! The `Tasks` trait.

use std::fmt;
use std::time::Duration;

use rupg_common::Result;

/// A unit of work for [`Tasks::spawn`].
pub type Task = Box<dyn FnOnce() + Send + 'static>;

/// The task scheduler. rupg has no async runtime, and the engine starts each thread and each background task through this trait (spec/22 section 22.5).
pub trait Tasks: Send + Sync + fmt::Debug {
    /// Starts a task. The name shows in a debugger and in the operating system thread list.
    fn spawn(&self, name: &str, task: Task) -> Result<TaskHandle>;

    /// Waits for at least `d`.
    fn sleep(&self, d: Duration);

    /// Lets other tasks run.
    fn yield_now(&self);

    /// The number of tasks that can run at the same time, for example the number of cores.
    fn parallelism(&self) -> usize;

    /// Gives back to the system the memory of the stack of the calling task that the task does not use now. A task calls it before it waits for a long time, so that the stack of a deep statement does not stay in memory while the session is idle (spec/06 section 6.4). The default does nothing.
    fn trim_stack(&self) {}
}

/// The part of a [`TaskHandle`] that each implementation gives.
pub trait Join: Send + fmt::Debug {
    /// Waits for the task to end. It is an error with SQLSTATE `XX000` if the task panicked.
    fn join(self: Box<Self>) -> Result<()>;

    /// True if the task has ended.
    fn is_finished(&self) -> bool;
}

/// A started task.
#[derive(Debug)]
#[must_use = "a task that is not joined can outlive its data"]
pub struct TaskHandle(Box<dyn Join>);

impl TaskHandle {
    /// Wraps the handle of an implementation.
    pub fn new(join: Box<dyn Join>) -> TaskHandle {
        TaskHandle(join)
    }

    /// Waits for the task to end. It is an error with SQLSTATE `XX000` if the task panicked.
    pub fn join(self) -> Result<()> {
        self.0.join()
    }

    /// True if the task has ended.
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

//! A seeded scheduler that runs every task on the calling thread.
//!
//! [`Tasks::spawn`] only queues the task. [`SimTasks::run_one`] runs one ready task, picked by the seeded generator, to its end. [`Tasks::yield_now`] runs one other ready task, so a task that waits in a loop with `yield_now` lets the others make progress. [`Tasks::sleep`] moves the simulated clock and does not wait.
//!
//! A task runs to its end or to a `yield_now`. The engine must not block a task on a lock or a channel that another task releases, because that task cannot run until the first one yields.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rupg_common::{Error, Result};

use super::{SimClock, SimRng, lock};
use crate::tasks::{Join, Task, TaskHandle, Tasks};

#[derive(Debug)]
struct State {
    rng: SimRng,
    next: u64,
    ready: Vec<Ready>,
    /// The tasks that ended, with true if the task panicked.
    finished: HashMap<u64, bool>,
}

struct Ready {
    id: u64,
    name: String,
    task: Task,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ready({}, {:?})", self.id, self.name)
    }
}

#[derive(Debug)]
struct Inner {
    clock: Arc<SimClock>,
    parallelism: usize,
    state: Mutex<State>,
}

impl Inner {
    fn run_one(&self) -> bool {
        let next = {
            let mut state = lock(&self.state);
            if state.ready.is_empty() {
                return false;
            }
            let n = state.ready.len() as u64;
            let i = state.rng.below(n) as usize;
            state.ready.swap_remove(i)
        };
        let panicked = catch_unwind(AssertUnwindSafe(next.task)).is_err();
        lock(&self.state).finished.insert(next.id, panicked);
        true
    }
}

/// The seeded scheduler.
#[derive(Clone, Debug)]
pub struct SimTasks {
    inner: Arc<Inner>,
}

impl SimTasks {
    /// A scheduler with its seed, the clock that `sleep` moves, and the number that `parallelism` gives.
    pub fn new(seed: u64, clock: Arc<SimClock>, parallelism: usize) -> SimTasks {
        SimTasks {
            inner: Arc::new(Inner {
                clock,
                parallelism,
                state: Mutex::new(State {
                    rng: SimRng::new(seed),
                    next: 0,
                    ready: Vec::new(),
                    finished: HashMap::new(),
                }),
            }),
        }
    }

    /// Runs one ready task to its end. Returns false if no task is ready.
    pub fn run_one(&self) -> bool {
        self.inner.run_one()
    }

    /// Runs tasks until no task is ready, and gives the number that ran.
    pub fn run_until_idle(&self) -> usize {
        let mut n = 0;
        while self.run_one() {
            n += 1;
        }
        n
    }

    /// The number of tasks that wait to run.
    pub fn ready(&self) -> usize {
        lock(&self.inner.state).ready.len()
    }
}

#[derive(Debug)]
struct SimJoin {
    inner: Arc<Inner>,
    id: u64,
    name: String,
}

impl Join for SimJoin {
    fn join(self: Box<Self>) -> Result<()> {
        loop {
            if let Some(&panicked) = lock(&self.inner.state).finished.get(&self.id) {
                if panicked {
                    return Err(Error::internal(format!("the task \"{}\" panicked", self.name)));
                }
                return Ok(());
            }
            if !self.inner.run_one() {
                return Err(Error::internal(format!(
                    "the task \"{}\" cannot end: it is running and waits for itself",
                    self.name
                )));
            }
        }
    }

    fn is_finished(&self) -> bool {
        lock(&self.inner.state).finished.contains_key(&self.id)
    }
}

impl Tasks for SimTasks {
    fn spawn(&self, name: &str, task: Task) -> Result<TaskHandle> {
        let mut state = lock(&self.inner.state);
        let id = state.next;
        state.next += 1;
        state.ready.push(Ready { id, name: name.to_string(), task });
        Ok(TaskHandle::new(Box::new(SimJoin {
            inner: Arc::clone(&self.inner),
            id,
            name: name.to_string(),
        })))
    }

    fn sleep(&self, d: Duration) {
        self.inner.clock.advance(d);
    }

    fn yield_now(&self) {
        self.inner.run_one();
    }

    fn parallelism(&self) -> usize {
        self.inner.parallelism
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::Clock;

    fn order(seed: u64) -> Vec<u32> {
        let tasks = SimTasks::new(seed, Arc::new(SimClock::new(0)), 4);
        let seen = Arc::new(Mutex::new(Vec::new()));
        for i in 0..10 {
            let seen = Arc::clone(&seen);
            let _ = tasks.spawn("t", Box::new(move || seen.lock().unwrap().push(i))).unwrap();
        }
        assert_eq!(tasks.ready(), 10);
        assert_eq!(tasks.run_until_idle(), 10);
        Arc::try_unwrap(seen).unwrap().into_inner().unwrap()
    }

    #[test]
    fn the_order_follows_the_seed() {
        assert_eq!(order(1), order(1));
        assert_ne!(order(1), order(2));
    }

    #[test]
    fn join_runs_the_task() {
        let clock = Arc::new(SimClock::new(0));
        let tasks = SimTasks::new(1, Arc::clone(&clock), 1);
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let t = tasks.clone();
        let h = tasks
            .spawn(
                "sleeper",
                Box::new(move || {
                    t.sleep(Duration::from_secs(2));
                    flag.store(true, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert!(!h.is_finished());
        h.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(clock.monotonic(), Duration::from_secs(2));

        let h = tasks.spawn("bad", Box::new(|| panic!("simulated panic"))).unwrap();
        let e = h.join().unwrap_err();
        assert!(e.message().contains("\"bad\" panicked"), "{e}");
    }

    #[test]
    fn yield_lets_a_waiting_task_make_progress() {
        let tasks = SimTasks::new(9, Arc::new(SimClock::new(0)), 2);
        let flag = Arc::new(AtomicBool::new(false));
        let (t, f) = (tasks.clone(), Arc::clone(&flag));
        let waiter = tasks
            .spawn(
                "waiter",
                Box::new(move || {
                    while !f.load(Ordering::SeqCst) {
                        t.yield_now();
                    }
                }),
            )
            .unwrap();
        let f = Arc::clone(&flag);
        let setter = tasks.spawn("setter", Box::new(move || f.store(true, Ordering::SeqCst)));
        waiter.join().unwrap();
        setter.unwrap().join().unwrap();
    }
}

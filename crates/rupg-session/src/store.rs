//! The catalog that the sessions of a server share, and the lock of the statements that change it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rupg_catalog::Catalog;
use rupg_common::{Error, Result, SqlState};
use rupg_platform::Tasks;

/// How long a session waits for the lock before it looks at its cancel flag again.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// The catalog that the sessions of a server share. A statement reads the catalog of the last commit. A transaction that defines an object holds the lock of the store until it ends, so only one transaction at a time changes the catalog.
#[derive(Debug, Default)]
pub struct Store {
    state: Mutex<State>,
    free: Condvar,
    /// The tasks of the server. A session that waits for the lock lets the other tasks run, because the simulated scheduler runs all the tasks on one thread.
    tasks: Option<Arc<dyn Tasks>>,
}

#[derive(Debug, Default)]
struct State {
    committed: Arc<Catalog>,
    locked: bool,
}

impl Store {
    /// A store with an empty catalog.
    pub fn new() -> Store {
        Store::default()
    }

    /// A store with an empty catalog for the sessions that run as `tasks`.
    pub fn with_tasks(tasks: Arc<dyn Tasks>) -> Store {
        Store { tasks: Some(tasks), ..Store::default() }
    }

    /// The catalog of the last commit.
    pub fn committed(&self) -> Arc<Catalog> {
        Arc::clone(&self.state().committed)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the lock for a transaction, with a copy of the catalog of the last commit. The session waits while another transaction holds the lock. A cancel of the session stops the wait with `57014`.
    pub(crate) fn lease(self: &Arc<Self>, cancel: &AtomicBool) -> Result<Lease> {
        let mut state = self.state();
        while state.locked {
            if cancel.swap(false, Ordering::Relaxed) {
                return Err(Error::new(
                    SqlState::QUERY_CANCELED,
                    "canceling statement due to user request",
                ));
            }
            if let Some(tasks) = &self.tasks {
                drop(state);
                tasks.yield_now();
                state = self.state();
                if !state.locked {
                    break;
                }
            }
            state =
                self.free.wait_timeout(state, WAIT_SLICE).unwrap_or_else(PoisonError::into_inner).0;
        }
        state.locked = true;
        Ok(Lease { store: Arc::clone(self), catalog: Arc::clone(&state.committed) })
    }
}

/// The lock of a transaction and its copy of the catalog. The drop of a lease releases the lock and discards the changes, but the OIDs that the transaction used stay used, as in PostgreSQL.
#[derive(Debug)]
pub(crate) struct Lease {
    store: Arc<Store>,
    /// The catalog with the changes of the transaction.
    pub(crate) catalog: Arc<Catalog>,
}

impl Lease {
    /// Makes the changes of the transaction the catalog of the last commit, and releases the lock.
    pub(crate) fn commit(mut self) {
        let catalog = std::mem::take(&mut self.catalog);
        self.store.state().committed = catalog;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.store.state();
        let next = self.catalog.next_oid();
        if state.committed.next_oid() < next {
            Arc::make_mut(&mut state.committed).advance_oid(next);
        }
        state.locked = false;
        drop(state);
        self.store.free.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_keeps_the_oids() {
        let store = Arc::new(Store::new());
        let cancel = AtomicBool::new(false);
        let mut lease = store.lease(&cancel).unwrap();
        Arc::make_mut(&mut lease.catalog).create_schema("s", 10).unwrap();
        let next = lease.catalog.next_oid();
        drop(lease);
        assert!(store.committed().schema_by_name("s").is_none());
        assert_eq!(store.committed().next_oid(), next);
        let mut lease = store.lease(&cancel).unwrap();
        let oid = Arc::make_mut(&mut lease.catalog).create_schema("s", 10).unwrap();
        assert_eq!(oid, next);
        lease.commit();
        assert_eq!(store.committed().schema_by_name("s").map(|s| s.oid), Some(oid));
    }

    #[test]
    fn cancel_stops_the_wait() {
        let store = Arc::new(Store::new());
        let held = store.lease(&AtomicBool::new(false)).unwrap();
        let error = store.lease(&AtomicBool::new(true)).unwrap_err();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        drop(held);
        assert!(store.lease(&AtomicBool::new(false)).is_ok());
    }

    /// Tasks whose `yield_now` runs the other task: the end of the transaction that holds the lock.
    #[derive(Debug, Default)]
    struct Holder(Mutex<Option<Lease>>);

    impl Tasks for Holder {
        fn spawn(&self, _: &str, _: rupg_platform::Task) -> Result<rupg_platform::TaskHandle> {
            Err(Error::internal("no tasks in this test"))
        }

        fn sleep(&self, _: Duration) {}

        fn yield_now(&self) {
            drop(self.0.lock().unwrap().take());
        }

        fn parallelism(&self) -> usize {
            1
        }
    }

    #[test]
    fn a_waiter_lets_the_holder_run() {
        let holder = Arc::new(Holder::default());
        let store = Arc::new(Store::with_tasks(holder.clone()));
        let held = store.lease(&AtomicBool::new(false)).unwrap();
        *holder.0.lock().unwrap() = Some(held);
        assert!(store.lease(&AtomicBool::new(false)).is_ok());
        assert!(holder.0.lock().unwrap().is_none());
    }
}

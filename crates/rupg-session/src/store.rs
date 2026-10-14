//! The catalogs that the sessions of a server share, one for each database, the lock of the statements that change a catalog, the rows of `pg_database`, and the table of the sessions.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rupg_catalog::{Catalog, Database};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Backend;
use rupg_platform::Tasks;

use crate::auth::AuthFiles;

/// How long a session waits for the lock before it looks at its cancel flag again.
const WAIT_SLICE: Duration = Duration::from_millis(50);
/// `CountOtherDBBackends` looks at the other sessions of a database 50 times, with this time between two looks, before it gives up.
const BUSY_SLICE: Duration = Duration::from_millis(100);
const BUSY_TRIES: u32 = 50;

/// The catalogs that the sessions of a server share. A statement reads the catalog of the last commit of its database. A transaction that defines an object holds the lock of its database until it ends, so only one transaction at a time changes the catalog of a database.
#[derive(Debug, Default)]
pub struct Store {
    state: Mutex<State>,
    free: Condvar,
    /// The tasks of the server. A session that waits for the lock lets the other tasks run, because the simulated scheduler runs all the tasks on one thread.
    tasks: Option<Arc<dyn Tasks>>,
    /// The table of the sessions, `BackendStatusArray`. The index of a slot is the number of its session.
    backends: Mutex<Vec<Option<Slot>>>,
    /// The end of a session, which wakes a session that waits for the other sessions of a database to end.
    left: Condvar,
    /// The time of the last reset of the statistics of the whole server, or 0 before the first session. PostgreSQL resets them when it starts with no statistics file, so the start of the first session sets it.
    stats_reset: AtomicI64,
    /// The authentication files of the server, which the views `pg_hba_file_rules` and `pg_ident_file_mappings` read.
    auth: Option<Arc<dyn AuthFiles>>,
}

/// A slot of the table of the sessions.
#[derive(Debug)]
struct Slot {
    backend: Backend,
    /// The flag that tells the session to end, as `SIGTERM` does. A session with the flag does not count as a session of its database.
    end: Arc<AtomicBool>,
}

#[derive(Debug)]
struct State {
    /// The databases by OID.
    databases: BTreeMap<u32, Db>,
    /// `GetNewObjectId`: the OID counter of the cluster. Each catalog has a counter, and a transaction starts its counter at the counter of the cluster, so two databases do not give the same OID, as in PostgreSQL.
    next_oid: u32,
}

/// A database: its row of `pg_database`, the catalog of its last commit and its lock.
#[derive(Debug)]
struct Db {
    row: Database,
    committed: Arc<Catalog>,
    locked: bool,
}

impl Default for State {
    /// The three databases of a new cluster, each with the catalog of `initdb` and the system views.
    fn default() -> State {
        let catalog = crate::system::catalog();
        let next_oid = catalog.next_oid();
        let databases = Database::initdb()
            .into_iter()
            .map(|row| (row.oid, Db { row, committed: Arc::clone(&catalog), locked: false }))
            .collect();
        State { databases, next_oid }
    }
}

/// A new database, before the store gives it an OID.
#[derive(Debug)]
pub(crate) struct NewDatabase {
    /// The row. Its OID is the OID of the option `OID`, or 0 for the next OID of the cluster.
    pub(crate) row: Database,
    /// The OID of the template.
    pub(crate) template: u32,
}

impl Store {
    /// A store with the catalog of a new cluster.
    pub fn new() -> Store {
        Store::default()
    }

    /// A store with the catalog of a new cluster for the sessions that run as `tasks`.
    pub fn with_tasks(tasks: Arc<dyn Tasks>) -> Store {
        Store { tasks: Some(tasks), ..Store::default() }
    }

    /// The store with the authentication files of the server.
    #[must_use]
    pub fn with_auth(self, auth: Arc<dyn AuthFiles>) -> Store {
        Store { auth: Some(auth), ..self }
    }

    /// The authentication files of the server, or `None` for a store with no server.
    pub(crate) fn auth(&self) -> Option<&dyn AuthFiles> {
        self.auth.as_deref()
    }

    /// The catalog of the last commit of a database. A database that does not exist, because another session dropped it, has the catalog of a new database.
    pub fn committed(&self, database: u32) -> Arc<Catalog> {
        match self.state().databases.get(&database) {
            Some(db) => Arc::clone(&db.committed),
            None => crate::system::catalog(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The row of `pg_database` of a database.
    pub fn database(&self, name: &str) -> Option<Database> {
        self.state().databases.values().find(|db| db.row.name == name).map(|db| db.row.clone())
    }

    /// The rows of `pg_database` of the databases that `CREATE DATABASE` made, in the order of their OIDs. The rows of `initdb` are static rows.
    pub fn created(&self) -> Arc<[Database]> {
        let initdb = Database::initdb();
        self.state()
            .databases
            .values()
            .filter(|db| initdb.iter().all(|row| row.oid != db.row.oid))
            .map(|db| db.row.clone())
            .collect()
    }

    /// The checks of `InitPostgres` for the database of a new session. It gives the OID of the database.
    ///
    /// # Errors
    ///
    /// `3D000` for a database that does not exist, and `55000` for a database that does not allow connections.
    pub fn admit(&self, name: &str) -> Result<u32> {
        match self.database(name) {
            None => Err(Error::new(
                SqlState::INVALID_CATALOG_NAME,
                format!("database \"{name}\" does not exist"),
            )),
            Some(row) if !row.allow_connections => Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("database \"{name}\" is not currently accepting connections"),
            )),
            Some(row) => Ok(row.oid),
        }
    }

    /// Takes the lock of a database for a transaction, with a copy of the catalog of the last commit. The session waits while another transaction holds the lock. A cancel of the session stops the wait with `57014`.
    pub(crate) fn lease(self: &Arc<Self>, database: u32, cancel: &AtomicBool) -> Result<Lease> {
        let mut state = self.state();
        loop {
            let Some(db) = state.databases.get(&database) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_DATABASE,
                    format!("database with OID {database} does not exist"),
                ));
            };
            if !db.locked {
                break;
            }
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
                continue;
            }
            state =
                self.free.wait_timeout(state, WAIT_SLICE).unwrap_or_else(PoisonError::into_inner).0;
        }
        let next = state.next_oid;
        let Some(db) = state.databases.get_mut(&database) else {
            return Err(Error::internal("the database is gone"));
        };
        db.locked = true;
        let mut catalog = Arc::clone(&db.committed);
        if catalog.next_oid() < next {
            Arc::make_mut(&mut catalog).advance_oid(next);
        }
        Ok(Lease { store: Arc::clone(self), database, catalog })
    }

    /// `createdb` after the checks of the options: the check of the name, the wait for the other sessions of the template to end, and the copy of the catalog of the template. `me` is the number of the session in the table. It gives the OID of the new database.
    ///
    /// # Errors
    ///
    /// `42P04` for a name that a database has, `55006` while other sessions use the template, and `22023` for an OID that a database has.
    pub(crate) fn create_database(&self, new: NewDatabase, me: Option<i32>) -> Result<u32> {
        let NewDatabase { mut row, template } = new;
        if self.database(&row.name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_DATABASE,
                format!("database \"{}\" already exists", row.name),
            ));
        }
        let others = self.others(template, me);
        if others > 0 {
            let name = self.state().databases.get(&template).map(|db| db.row.name.clone());
            return Err(busy(
                format!(
                    "source database \"{}\" is being accessed by other users",
                    name.unwrap_or_default()
                ),
                others,
            ));
        }
        let mut state = self.state();
        if state.databases.values().any(|db| db.row.name == row.name) {
            return Err(Error::new(
                SqlState::DUPLICATE_DATABASE,
                format!("database \"{}\" already exists", row.name),
            ));
        }
        if row.oid != 0 {
            if let Some(db) = state.databases.get(&row.oid) {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "database OID {} is already in use by database \"{}\"",
                        row.oid, db.row.name
                    ),
                ));
            }
        } else {
            row.oid = state.next_oid;
            state.next_oid += 1;
        }
        let Some(source) = state.databases.get(&template) else {
            return Err(Error::internal("the template is gone"));
        };
        let mut committed = Arc::clone(&source.committed);
        let next = state.next_oid.max(row.oid + 1);
        Arc::make_mut(&mut committed).advance_oid(next);
        state.next_oid = next;
        let oid = row.oid;
        state.databases.insert(oid, Db { row, committed, locked: false });
        Ok(oid)
    }

    /// `dropdb` after the checks of the name and of the template: the end of the other sessions with `force`, the wait for the other sessions to end, and the removal of the database.
    ///
    /// # Errors
    ///
    /// `55006` while other sessions use the database.
    pub(crate) fn drop_database(&self, row: &Database, force: bool, me: Option<i32>) -> Result<()> {
        if force {
            for slot in self.backends_mut().iter().flatten() {
                if slot.backend.database == row.oid && Some(slot.backend.number) != me {
                    slot.end.store(true, Ordering::Relaxed);
                }
            }
        }
        let others = self.others(row.oid, me);
        if others > 0 {
            return Err(busy(
                format!("database \"{}\" is being accessed by other users", row.name),
                others,
            ));
        }
        self.state().databases.remove(&row.oid);
        Ok(())
    }

    /// `CountOtherDBBackends`: the sessions of a database other than `me` that do not end. It waits up to 5 seconds for them to end.
    fn others(&self, database: u32, me: Option<i32>) -> usize {
        let count = |backends: &[Option<Slot>]| {
            backends
                .iter()
                .flatten()
                .filter(|slot| {
                    slot.backend.database == database
                        && Some(slot.backend.number) != me
                        && !slot.end.load(Ordering::Relaxed)
                })
                .count()
        };
        let mut backends = self.backends_mut();
        for _ in 0..BUSY_TRIES {
            let others = count(&backends);
            if others == 0 {
                return 0;
            }
            if let Some(tasks) = &self.tasks {
                drop(backends);
                tasks.sleep(BUSY_SLICE);
                backends = self.backends_mut();
            } else {
                backends = self
                    .left
                    .wait_timeout(backends, BUSY_SLICE)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        count(&backends)
    }

    fn backends_mut(&self) -> MutexGuard<'_, Vec<Option<Slot>>> {
        self.backends.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `pgstat_bestart`: puts a session in the first free slot of the table, and gives the number of the slot. The session ends when another session sets `end`.
    pub(crate) fn join(&self, mut backend: Backend, end: Arc<AtomicBool>) -> i32 {
        let _ = self.stats_reset.compare_exchange(
            0,
            backend.backend_start,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        let mut backends = self.backends_mut();
        let slot = backends.iter().position(Option::is_none).unwrap_or(backends.len());
        let number = i32::try_from(slot).unwrap_or(i32::MAX);
        backend.number = number;
        let entry = Some(Slot { backend, end });
        if slot == backends.len() {
            backends.push(entry);
        } else {
            backends[slot] = entry;
        }
        number
    }

    /// The end of a session, which frees its slot.
    pub(crate) fn leave(&self, number: i32) {
        let mut backends = self.backends_mut();
        if let Some(slot) = usize::try_from(number).ok().and_then(|at| backends.get_mut(at)) {
            *slot = None;
        }
        drop(backends);
        self.left.notify_all();
    }

    /// Changes the row of the session with this number.
    pub(crate) fn report(&self, number: i32, change: impl FnOnce(&mut Backend)) {
        let mut backends = self.backends_mut();
        if let Some(Some(slot)) = usize::try_from(number).ok().and_then(|at| backends.get_mut(at)) {
            change(&mut slot.backend);
        }
    }

    /// The time of the last reset of the statistics of the whole server, or 0 before the first session.
    pub(crate) fn stats_reset(&self) -> i64 {
        self.stats_reset.load(Ordering::Relaxed)
    }

    /// `pgstat_read_current_status`: a copy of the rows of the table, in the order of their numbers.
    pub(crate) fn backends(&self) -> Arc<[Backend]> {
        self.backends_mut().iter().flatten().map(|slot| slot.backend.clone()).collect()
    }
}

/// The error of a database that other sessions use, with `errdetail_busy_db`.
fn busy(message: String, others: usize) -> Error {
    let detail = if others == 1 {
        "There is 1 other session using the database.".to_owned()
    } else {
        format!("There are {others} other sessions using the database.")
    };
    Error::new(SqlState::OBJECT_IN_USE, message).with_detail(detail)
}

/// The lock of a transaction on a database and its copy of the catalog. The drop of a lease releases the lock and discards the changes, but the OIDs that the transaction used stay used, as in PostgreSQL.
#[derive(Debug)]
pub(crate) struct Lease {
    store: Arc<Store>,
    database: u32,
    /// The catalog with the changes of the transaction.
    pub(crate) catalog: Arc<Catalog>,
}

impl Lease {
    /// Makes the changes of the transaction the catalog of the last commit, and releases the lock.
    pub(crate) fn commit(mut self) {
        let catalog = std::mem::take(&mut self.catalog);
        if let Some(db) = self.store.state().databases.get_mut(&self.database) {
            db.committed = catalog;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.store.state();
        let next = self.catalog.next_oid();
        state.next_oid = state.next_oid.max(next);
        if let Some(db) = state.databases.get_mut(&self.database) {
            if db.committed.next_oid() < next {
                Arc::make_mut(&mut db.committed).advance_oid(next);
            }
            db.locked = false;
        }
        drop(state);
        self.store.free.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_catalog::database::POSTGRES;

    #[test]
    fn rollback_keeps_the_oids() {
        let store = Arc::new(Store::new());
        let cancel = AtomicBool::new(false);
        let mut lease = store.lease(POSTGRES, &cancel).unwrap();
        Arc::make_mut(&mut lease.catalog).create_schema("s", 10).unwrap();
        let next = lease.catalog.next_oid();
        drop(lease);
        assert!(store.committed(POSTGRES).schema_by_name("s").is_none());
        assert_eq!(store.committed(POSTGRES).next_oid(), next);
        let mut lease = store.lease(POSTGRES, &cancel).unwrap();
        let oid = Arc::make_mut(&mut lease.catalog).create_schema("s", 10).unwrap();
        assert_eq!(oid, next);
        lease.commit();
        assert_eq!(store.committed(POSTGRES).schema_by_name("s").map(|s| s.oid), Some(oid));
    }

    /// A session takes the first free slot of the table, and a snapshot has the rows in the order of the slots.
    #[test]
    fn sessions_take_the_free_slots() {
        let store = Store::new();
        let backend = |pid| Backend { pid, ..Backend::default() };
        assert_eq!(store.join(backend(1), Arc::default()), 0);
        assert_eq!(store.join(backend(2), Arc::default()), 1);
        store.leave(0);
        assert_eq!(store.join(backend(3), Arc::default()), 0);
        store.report(1, |b| b.query = "SELECT 1".to_owned());
        let rows = store.backends();
        let seen: Vec<(i32, i32, &str)> =
            rows.iter().map(|b| (b.number, b.pid, b.query.as_str())).collect();
        assert_eq!(seen, [(0, 3, ""), (1, 2, "SELECT 1")]);
    }

    #[test]
    fn cancel_stops_the_wait() {
        let store = Arc::new(Store::new());
        let held = store.lease(POSTGRES, &AtomicBool::new(false)).unwrap();
        let error = store.lease(POSTGRES, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        drop(held);
        assert!(store.lease(POSTGRES, &AtomicBool::new(false)).is_ok());
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
        let held = store.lease(POSTGRES, &AtomicBool::new(false)).unwrap();
        *holder.0.lock().unwrap() = Some(held);
        assert!(store.lease(POSTGRES, &AtomicBool::new(false)).is_ok());
        assert!(holder.0.lock().unwrap().is_none());
    }
}

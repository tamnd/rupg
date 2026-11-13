//! The catalogs that the sessions of a server share, one for each database, the lock of the statements that change a catalog, the rows of `pg_database`, and the table of the sessions.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rupg_catalog::{Catalog, Database, SequenceData, Tablespace};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Backend, FunctionCounts, RelationCounts};
use rupg_pgcatalog::roles::{DatabaseChange, Roles, TablespaceChange};
use rupg_platform::Tasks;

use crate::advisory::Locks;
use crate::auth::AuthFiles;
use crate::connection::{Calls, Pending, Scans};
use crate::guc::Source;
use crate::heap::Heap;
use crate::notify::Listeners;
use crate::twophase::{DurableLease, PreparedXacts};

mod image;
mod log;

/// `BOOTSTRAP_SUPERUSERID`.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// True when the role exists. The bootstrap superuser always exists.
fn role_exists(roles: &Roles, role: u32) -> bool {
    role == BOOTSTRAP_SUPERUSER || roles.name(role).is_some()
}

/// How long a session waits for the lock before it looks at its cancel flag again.
const WAIT_SLICE: Duration = Duration::from_millis(50);
/// `CountOtherDBBackends` looks at the other sessions of a database 50 times, with this time between two looks, before it gives up.
const BUSY_SLICE: Duration = Duration::from_millis(100);
const BUSY_TRIES: u32 = 50;

/// The catalogs that the sessions of a server share. A statement reads the catalog of the last commit of its database. A transaction that defines an object holds the lock of its database until it ends, so only one transaction at a time changes the catalog of a database.
#[derive(Debug, Default)]
pub struct Store {
    state: Mutex<State>,
    /// The count of the changes of the catalog of the last commit of a database, of the rows of `pg_database` and of the roles. A session keeps what it read of them while the count stays, so a statement does not lock [`Store::state`].
    generation: AtomicU64,
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
    /// The stored secret of the bootstrap superuser, which `pg_authid.rolpassword` shows.
    password: Option<String>,
    /// The advisory locks of the sessions.
    locks: Locks,
    /// The rows of the files of the sequences that the sessions changed, by the OID of the database and the number of the file. A number of a file is unique only in its database, as in PostgreSQL. The rows are not in a transaction, so a rollback does not undo a change, as in PostgreSQL.
    sequences: Mutex<HashMap<(u32, u32), SequenceData>>,
    /// The rows of the sequences that changed after the last record of the log, for a store with a file.
    changed: Mutex<HashMap<(u32, u32), SequenceData>>,
    /// The counters of the relations in the cumulative statistics, by the OID of the database and the OID of the relation.
    relations: Mutex<HashMap<(u32, u32), RelationCounts>>,
    /// The counters of the functions in the cumulative statistics, by the OID of the database and the OID of the function.
    functions: Mutex<HashMap<(u32, u32), FunctionCounts>>,
    /// The rows of the tables.
    heap: Heap,
    /// The file of the image of the store, which a checkpoint writes, or `None` for a store with no file.
    image: Option<image::ImageFile>,
    /// The sessions that listen for notifications.
    listeners: Listeners,
    /// The prepared transactions of two phase commit.
    prepared: PreparedXacts,
}

/// What [`Store::committed_view`] gives: the count of [`Store::generation`], the catalog of the last commit of a database, the roles and the owner of the database.
pub(crate) type CommittedView = (u64, Arc<Catalog>, Arc<Roles>, u32);

/// The row of a session in the table of the sessions.
pub(crate) type Row = Arc<Mutex<Backend>>;

/// A slot of the table of the sessions.
#[derive(Debug)]
struct Slot {
    /// The row of the session. The session keeps its own handle of the row, so that a change of the row takes no lock of the table.
    backend: Row,
    /// The flag that tells the session to end, as `SIGTERM` does. A session with the flag does not count as a session of its database.
    end: Arc<AtomicBool>,
    /// The flag that tells the session to cancel its statement, as `SIGINT` does.
    cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
struct State {
    /// The databases by OID.
    databases: BTreeMap<u32, Db>,
    /// `GetNewObjectId`: the OID counter of the cluster. Each catalog has a counter, and a transaction starts its counter at the counter of the cluster, so two databases do not give the same OID, as in PostgreSQL.
    next_oid: u32,
    /// The roles of `CREATE ROLE` and the memberships of `GRANT` of the last commit. The roles belong to the cluster, so each database sees them.
    roles: Arc<Roles>,
    /// The rows of `pg_tablespace` of `CREATE TABLESPACE`, in the order of their OIDs. `CREATE TABLESPACE` and `DROP TABLESPACE` cannot run in a transaction block, so a change is at once in the rows.
    tablespaces: Arc<[Tablespace]>,
    /// The database whose lock each waiting session waits for, by the process ID of the session.
    waiting: HashMap<i32, u32>,
    /// The process ID of the session whose open transaction changed the row of `pg_database` of each database, by the OID of the database: the lock of the row until the end of the transaction.
    row_locks: HashMap<u32, i32>,
    /// The database whose row of `pg_database` each waiting session waits for, by the process ID of the session.
    row_waiting: HashMap<i32, u32>,
    /// The rows of `pg_db_role_setting` by the OID of the database and the OID of the role, 0 for all: the settings of `ALTER DATABASE ... SET` and `ALTER ROLE ... SET`, each as `name=value`. A row of a database or a role that no longer exists has no effect.
    settings: BTreeMap<(u32, u32), Vec<String>>,
}

/// A database: its row of `pg_database`, the catalog of its last commit and its lock.
#[derive(Debug)]
struct Db {
    row: Database,
    committed: Arc<Catalog>,
    /// The process ID of the session whose transaction holds the lock of the database, or `None` when the lock is free.
    locked: Option<i32>,
}

impl Default for State {
    /// The three databases of a new cluster, each with the catalog of `initdb` and the system views.
    fn default() -> State {
        let catalog = crate::system::catalog();
        let next_oid = catalog.next_oid();
        let databases = Database::initdb()
            .into_iter()
            .map(|row| (row.oid, Db { row, committed: Arc::clone(&catalog), locked: None }))
            .collect();
        State {
            databases,
            next_oid,
            roles: Arc::default(),
            tablespaces: Arc::from(Vec::new()),
            waiting: HashMap::new(),
            row_locks: HashMap::new(),
            row_waiting: HashMap::new(),
            settings: BTreeMap::new(),
        }
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
        let mut store = Store { tasks: Some(Arc::clone(&tasks)), ..Store::default() };
        store.heap.set_tasks(Some(tasks));
        store
    }

    /// The store with the authentication files of the server.
    #[must_use]
    pub fn with_auth(self, auth: Arc<dyn AuthFiles>) -> Store {
        Store { auth: Some(auth), ..self }
    }

    /// The store with the stored secret of the bootstrap superuser, in the format of `pg_authid.rolpassword`.
    #[must_use]
    pub fn with_password(self, password: Option<String>) -> Store {
        Store { password, ..self }
    }

    /// The stored secret of the bootstrap superuser, or `None` for a superuser with no password.
    pub(crate) fn password(&self) -> Option<&str> {
        self.password.as_deref()
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

    /// The rows of `pg_database` and the catalogs of the last commit of all the databases, in the order of their OIDs.
    pub(crate) fn catalogs(&self) -> Vec<(Database, Arc<Catalog>)> {
        self.state()
            .databases
            .values()
            .map(|db| (db.row.clone(), Arc::clone(&db.committed)))
            .collect()
    }

    /// The count of the changes of [`Store::committed`], of the roles and of the owners of the databases. A change adds one to the count while the state is locked.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The catalog of the last commit of a database, the roles and the owner of the database, under one lock of the state, with the count of [`Store::generation`] that they have.
    pub(crate) fn committed_view(&self, database: u32) -> CommittedView {
        let state = self.state();
        let generation = self.generation.load(Ordering::Acquire);
        let (catalog, owner) = match state.databases.get(&database) {
            Some(db) => (Arc::clone(&db.committed), db.row.owner),
            None => (crate::system::catalog(), 10),
        };
        (generation, catalog, Arc::clone(&state.roles), owner)
    }

    /// Notes a change of the state that [`Store::committed_view`] reads. The caller holds the lock of the state.
    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The row of `pg_database` of a database.
    pub fn database(&self, name: &str) -> Option<Database> {
        self.state().databases.values().find(|db| db.row.name == name).map(|db| db.row.clone())
    }

    /// The rows of `pg_database` of the databases that `CREATE DATABASE` made, and the rows of `initdb` that a statement changed, in the order of their OIDs. The other rows of `initdb` are static rows.
    pub fn created(&self) -> Arc<[Database]> {
        let initdb = Database::initdb();
        self.state()
            .databases
            .values()
            .filter(|db| initdb.iter().all(|row| *row != db.row))
            .map(|db| db.row.clone())
            .collect()
    }

    /// [`Store::created`] with the rows that a transaction changed and did not commit yet.
    pub(crate) fn databases_with(&self, changes: &[DatabaseChange]) -> Arc<[Database]> {
        if changes.is_empty() {
            return self.created();
        }
        let initdb = Database::initdb();
        self.rows_with(changes).into_iter().filter(|row| initdb.iter().all(|i| i != row)).collect()
    }

    /// The rows of `pg_database` of all databases with the rows that a transaction changed and did not commit yet, in the order of their OIDs.
    pub(crate) fn rows_with(&self, changes: &[DatabaseChange]) -> Vec<Database> {
        self.state()
            .databases
            .values()
            .map(|db| {
                let mut row = db.row.clone();
                if let Some(change) = changes.iter().find(|c| c.oid == row.oid) {
                    rupg_analyze::apply_database_change(change, &mut row);
                }
                row
            })
            .collect()
    }

    /// `CountOtherDBBackends` for `RenameDatabase` and `movedb`.
    ///
    /// # Errors
    ///
    /// `55006` while other sessions use the database.
    pub(crate) fn check_unused(&self, row: &Database, me: Option<i32>) -> Result<()> {
        let others = self.others(row.oid, me);
        if others > 0 {
            return Err(busy(
                format!("database \"{}\" is being accessed by other users", row.name),
                others,
            ));
        }
        Ok(())
    }

    /// `dathasloginevt` of a database.
    pub(crate) fn login_event(&self, oid: u32) -> bool {
        self.state().databases.get(&oid).is_some_and(|db| db.row.has_login_event)
    }

    /// `SetDatabaseHasLoginEventTriggers`, and the clear of `EventTriggerOnLogin`: the change of `dathasloginevt` in place, which does not wait for the commit.
    pub(crate) fn set_login_event(&self, oid: u32, on: bool) {
        if let Some(db) = self.state().databases.get_mut(&oid) {
            db.row.has_login_event = on;
        }
        self.log_catalog(true);
    }

    /// The rows of `pg_tablespace` of the tablespaces that `CREATE TABLESPACE` made, in the order of their OIDs.
    pub(crate) fn tablespaces(&self) -> Arc<[Tablespace]> {
        Arc::clone(&self.state().tablespaces)
    }

    /// [`Store::tablespaces`] with the rows that a transaction changed and did not commit yet.
    pub(crate) fn tablespaces_with(&self, changes: &[TablespaceChange]) -> Arc<[Tablespace]> {
        let rows = self.tablespaces();
        if changes.is_empty() {
            return rows;
        }
        rows.iter()
            .map(|row| {
                let mut row = row.clone();
                if let Some(change) = changes.iter().find(|c| c.oid == row.oid) {
                    rupg_analyze::apply_tablespace_change(change, &mut row);
                }
                row
            })
            .collect()
    }

    /// `CreateTableSpace` after the checks of the statement: the new row of `pg_tablespace` with the next OID of the cluster. It gives the OID.
    ///
    /// # Errors
    ///
    /// `42710` for a name that another tablespace has.
    pub(crate) fn create_tablespace(&self, mut row: Tablespace) -> Result<u32> {
        let mut state = self.state();
        if state.tablespaces.iter().any(|t| t.name == row.name) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("tablespace \"{}\" already exists", row.name),
            ));
        }
        row.oid = state.next_oid;
        state.next_oid += 1;
        let oid = row.oid;
        let mut rows = state.tablespaces.to_vec();
        rows.push(row);
        state.tablespaces = Arc::from(rows);
        drop(state);
        self.log_catalog(true);
        Ok(oid)
    }

    /// `DropTableSpace` after the checks of the statement: the removal of the row of `pg_tablespace`.
    pub(crate) fn drop_tablespace(&self, oid: u32) {
        let mut state = self.state();
        let rows: Vec<Tablespace> =
            state.tablespaces.iter().filter(|t| t.oid != oid).cloned().collect();
        state.tablespaces = Arc::from(rows);
        drop(state);
        self.log_catalog(true);
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

    /// The stored settings for a session of the database as the user, each as `name=value` with its source, which `process_settings` applies at the start of the session. A setting comes after the settings of a lower source, so it takes their place.
    pub fn session_settings(&self, database: u32, user: &str) -> Vec<(Source, String)> {
        let state = self.state();
        let role = state.roles.oid(user).unwrap_or(BOOTSTRAP_SUPERUSER);
        [
            ((0, 0), Source::Global),
            ((database, 0), Source::Database),
            ((0, role), Source::User),
            ((database, role), Source::DatabaseUser),
        ]
        .into_iter()
        .filter_map(|(key, source)| Some((state.settings.get(&key)?, source)))
        .flat_map(|(items, source)| items.iter().map(move |item| (source, item.clone())))
        .collect()
    }

    /// The rows of `pg_db_role_setting` of the databases and the roles that exist, in the order of the OIDs of the databases and of the roles.
    pub(crate) fn db_role_settings(&self) -> Vec<(u32, u32, Vec<String>)> {
        let state = self.state();
        state
            .settings
            .iter()
            .filter(|((database, role), items)| {
                !items.is_empty()
                    && (*database == 0 || state.databases.contains_key(database))
                    && (*role == 0 || role_exists(&state.roles, *role))
            })
            .map(|(&(database, role), items)| (database, role, items.clone()))
            .collect()
    }

    /// Changes the row of `pg_db_role_setting` for a database and a role, 0 for all, as `AlterSetting` does. The change does not wait for the end of the transaction.
    pub(crate) fn change_settings(
        &self,
        database: u32,
        role: u32,
        change: impl FnOnce(&mut Vec<String>),
    ) {
        change(self.state().settings.entry((database, role)).or_default());
        self.log_catalog(true);
    }

    /// Takes the lock of a database for a transaction of the session `pid`, with a copy of the catalog of the last commit. The session waits while another transaction holds the lock, and `report` tells the start and the end of the wait. A cancel of the session stops the wait with `57014`.
    pub(crate) fn lease(
        self: &Arc<Self>,
        database: u32,
        pid: i32,
        cancel: &AtomicBool,
        report: &dyn Fn(bool),
    ) -> Result<Lease> {
        let mut state = self.state();
        let mut waited = false;
        let result = loop {
            let Some(db) = state.databases.get(&database) else {
                break Err(Error::new(
                    SqlState::UNDEFINED_DATABASE,
                    format!("database with OID {database} does not exist"),
                ));
            };
            if db.locked.is_none() {
                break Ok(());
            }
            if cancel.swap(false, Ordering::Relaxed) {
                break Err(Error::new(
                    SqlState::QUERY_CANCELED,
                    "canceling statement due to user request",
                ));
            }
            if !waited {
                waited = true;
                state.waiting.insert(pid, database);
                drop(state);
                report(true);
                state = self.state();
                continue;
            }
            if let Some(tasks) = &self.tasks {
                drop(state);
                tasks.yield_now();
                state = self.state();
                continue;
            }
            state =
                self.free.wait_timeout(state, WAIT_SLICE).unwrap_or_else(PoisonError::into_inner).0;
        };
        if waited {
            state.waiting.remove(&pid);
        }
        // The session takes the lock before it releases the mutex for the report of the end of the wait. Else another session can take the lock in the gap, and the commit of one transaction discards the changes of the other.
        let lease = result.map(|()| {
            let next = state.next_oid;
            let roles = Arc::clone(&state.roles);
            let db = state.databases.get_mut(&database).expect("the loop found the database");
            db.locked = Some(pid);
            let mut catalog = Arc::clone(&db.committed);
            if catalog.next_oid() < next {
                Arc::make_mut(&mut catalog).advance_oid(next);
            }
            Lease { store: Arc::clone(self), database, catalog, base: Arc::clone(&roles), roles }
        });
        drop(state);
        if waited {
            report(false);
        }
        lease
    }

    /// The lock of the database `database` and of the rows of `pg_database` of `lease` go to `owner`, the owner of the locks of a prepared transaction that changed the catalog and that a restart prepared again. The counter of the OIDs takes the counter of the catalog of the transaction, so that no other object takes an OID of the transaction.
    pub(crate) fn relock(&self, database: u32, owner: i32, lease: &DurableLease) {
        let mut state = self.state();
        state.next_oid = state.next_oid.max(lease.catalog.next_oid());
        if let Some(db) = state.databases.get_mut(&database) {
            db.locked = Some(owner);
        }
        for &row in &lease.rows {
            state.row_locks.insert(row, owner);
        }
    }

    /// The lease of the prepared transaction that a restart prepared again with the changes of the catalog `lease`, for its `COMMIT PREPARED` or `ROLLBACK PREPARED`. [`Store::relock`] gave it the lock of the database.
    pub(crate) fn recovered_lease(self: &Arc<Self>, database: u32, lease: DurableLease) -> Lease {
        let DurableLease { catalog, base, roles, rows: _ } = lease;
        Lease { store: Arc::clone(self), database, catalog, base, roles }
    }

    /// The session whose transaction holds the lock of the database, or of the row of `pg_database`, that the session `pid` waits for.
    pub(crate) fn lease_blocker(&self, pid: i32) -> Option<i32> {
        let state = self.state();
        if let Some(database) = state.row_waiting.get(&pid) {
            return state.row_locks.get(database).copied().filter(|&holder| holder != pid);
        }
        let database = state.waiting.get(&pid)?;
        state.databases.get(database)?.locked.filter(|&holder| holder != pid)
    }

    /// Takes the lock of the rows of `pg_database` of `databases` for the transaction of the session `pid`, which changed them. The lock goes with the lease of the transaction.
    pub(crate) fn lock_database_rows(&self, pid: i32, databases: impl IntoIterator<Item = u32>) {
        let mut state = self.state();
        for database in databases {
            state.row_locks.insert(database, pid);
        }
    }

    /// Waits while the transaction of another session holds the lock of the row of `pg_database` of `database`, as `heap_inplace_lock` does, and `report` tells the start and the end of the wait. A cancel of the session stops the wait with `57014`.
    pub(crate) fn wait_database_row(
        &self,
        database: u32,
        pid: i32,
        cancel: &AtomicBool,
        report: &dyn Fn(bool),
    ) -> Result<()> {
        let mut state = self.state();
        let mut waited = false;
        let result = loop {
            if state.row_locks.get(&database).is_none_or(|&holder| holder == pid) {
                break Ok(());
            }
            if cancel.swap(false, Ordering::Relaxed) {
                break Err(Error::new(
                    SqlState::QUERY_CANCELED,
                    "canceling statement due to user request",
                ));
            }
            if !waited {
                waited = true;
                state.row_waiting.insert(pid, database);
                drop(state);
                report(true);
                state = self.state();
                continue;
            }
            if let Some(tasks) = &self.tasks {
                drop(state);
                tasks.yield_now();
                state = self.state();
                continue;
            }
            state =
                self.free.wait_timeout(state, WAIT_SLICE).unwrap_or_else(PoisonError::into_inner).0;
        };
        if waited {
            state.row_waiting.remove(&pid);
        }
        drop(state);
        if waited {
            report(false);
        }
        result
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
        state.databases.insert(oid, Db { row, committed, locked: None });
        self.changed();
        drop(state);
        self.log_catalog(true);
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
                let backend = lock(&slot.backend);
                if backend.database == row.oid && Some(backend.number) != me {
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
        let mut state = self.state();
        state.databases.remove(&row.oid);
        self.changed();
        // `DropSetting`.
        state.settings.retain(|&(database, _), _| database != row.oid);
        drop(state);
        self.heap.drop_tables(row.oid, |_| false);
        self.log_catalog(true);
        Ok(())
    }

    /// `CountOtherDBBackends`: the sessions of a database other than `me` that do not end. It waits up to 5 seconds for them to end.
    fn others(&self, database: u32, me: Option<i32>) -> usize {
        let count = |backends: &[Option<Slot>]| {
            backends
                .iter()
                .flatten()
                .filter(|slot| {
                    let backend = lock(&slot.backend);
                    backend.database == database
                        && Some(backend.number) != me
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

    /// A wait of a session for `time`, which lets the other tasks run. A store with no tasks waits on the release of the lock, and a spurious wake ends the wait early.
    pub(crate) fn nap(&self, time: Duration) {
        if let Some(tasks) = &self.tasks {
            tasks.sleep(time);
            return;
        }
        drop(self.free.wait_timeout(self.state(), time).unwrap_or_else(PoisonError::into_inner));
    }

    /// The advisory locks of the sessions.
    pub(crate) fn locks(&self) -> &Locks {
        &self.locks
    }

    /// The rows of the tables.
    /// Uses `count` OIDs of the counter of the cluster, as the values that the writes moved to the TOAST tables use them.
    pub(crate) fn skip_oids(&self, count: u32) {
        let mut state = self.state();
        state.next_oid = state.next_oid.saturating_add(count);
    }

    pub(crate) fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The prepared transactions of two phase commit.
    pub(crate) fn prepared(&self) -> &PreparedXacts {
        &self.prepared
    }

    /// The sessions that listen for notifications.
    pub(crate) fn listeners(&self) -> &Listeners {
        &self.listeners
    }

    /// The row of the file of a sequence in a database, or `None` when no session changed it.
    pub(crate) fn sequence_row(&self, database: u32, file: u32) -> Option<SequenceData> {
        self.sequences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(database, file))
            .copied()
    }

    /// `read_seq_tuple` and the change of the row under the lock of the buffer: the row of the file of a sequence changes only when `change` gives no error. `initial` is the row when no session changed it yet.
    pub(crate) fn change_sequence(
        &self,
        database: u32,
        file: u32,
        initial: SequenceData,
        change: &mut dyn FnMut(&mut SequenceData) -> Result<()>,
    ) -> Result<()> {
        let mut rows = self.sequences.lock().unwrap_or_else(PoisonError::into_inner);
        let mut row = rows.get(&(database, file)).copied().unwrap_or(initial);
        change(&mut row)?;
        rows.insert((database, file), row);
        if self.image.is_some() {
            self.changed_sequences().insert((database, file), row);
        }
        Ok(())
    }

    /// The rows of the sequences that changed after the last record of the log.
    fn changed_sequences(&self) -> MutexGuard<'_, HashMap<(u32, u32), SequenceData>> {
        self.changed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The tasks of the server, or `None` for sessions that run on threads.
    pub(crate) fn tasks(&self) -> Option<&dyn Tasks> {
        self.tasks.as_deref()
    }

    /// The tasks of the server, shared, for the background compiles of the compile tiers.
    pub(crate) fn shared_tasks(&self) -> Option<Arc<dyn Tasks>> {
        self.tasks.clone()
    }

    fn backends_mut(&self) -> MutexGuard<'_, Vec<Option<Slot>>> {
        self.backends.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `pgstat_bestart`: puts a session in the first free slot of the table, and gives the number of the slot and the row. The session ends when another session sets `end`.
    pub(crate) fn join_row(
        &self,
        mut backend: Backend,
        end: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
    ) -> (i32, Row) {
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
        let row = Arc::new(Mutex::new(backend));
        let entry = Some(Slot { backend: Arc::clone(&row), end, cancel });
        if slot == backends.len() {
            backends.push(entry);
        } else {
            backends[slot] = entry;
        }
        (number, row)
    }

    /// [`Store::join_row`], with the number of the slot only.
    #[cfg(test)]
    pub(crate) fn join(&self, backend: Backend, end: Arc<AtomicBool>) -> i32 {
        self.join_row(backend, end, Arc::default()).0
    }

    /// `BackendPidGetProc`: the role of the session with the process ID `pid`, or `None` when no session has it or the session ends.
    pub(crate) fn backend_role(&self, pid: i32) -> Option<u32> {
        self.backends_mut().iter().flatten().find_map(|slot| {
            let backend = lock(&slot.backend);
            (backend.pid == pid && !slot.end.load(Ordering::Relaxed)).then_some(backend.role)
        })
    }

    /// The signal of `pg_signal_backend` to the session with the process ID `pid`: the cancel of its statement, or its end with `terminate`, which also stops a wait of the session. False when no session has the process ID.
    pub(crate) fn signal(&self, pid: i32, terminate: bool) -> bool {
        let backends = self.backends_mut();
        let Some(slot) = backends.iter().flatten().find(|slot| lock(&slot.backend).pid == pid)
        else {
            return false;
        };
        if terminate {
            slot.end.store(true, Ordering::Relaxed);
        }
        slot.cancel.store(true, Ordering::Relaxed);
        drop(backends);
        self.left.notify_all();
        true
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
    #[cfg(test)]
    pub(crate) fn report(&self, number: i32, change: impl FnOnce(&mut Backend)) {
        let row = self
            .backends_mut()
            .get(usize::try_from(number).unwrap_or(usize::MAX))
            .and_then(|slot| slot.as_ref().map(|slot| Arc::clone(&slot.backend)));
        if let Some(row) = row {
            change(&mut lock(&row));
        }
    }

    /// The time of the last reset of the statistics of the whole server, or 0 before the first session.
    pub(crate) fn stats_reset(&self) -> i64 {
        self.stats_reset.load(Ordering::Relaxed)
    }

    /// `pgstat_relation_flush_cb`: adds the sequential scans that a transaction counted to the counters of the relations of its database. `time` is the end of the transaction, which becomes `lastscan` when it is later than the last scan.
    pub(crate) fn report_scans(&self, database: u32, scans: &BTreeMap<u32, Scans>, time: i64) {
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        for (&relation, scans) in scans {
            let counts = relations.entry((database, relation)).or_default();
            counts.tuples_returned += scans.returned;
            if scans.scans > 0 {
                counts.numscans += scans.scans;
                counts.lastscan = Some(counts.lastscan.map_or(time, |last| last.max(time)));
            }
        }
    }

    /// `pgstat_function_flush_cb`: adds the calls that a session counted to the counters of the functions of its database. A function that `exists` does not tell, which a committed `DROP FUNCTION` dropped, gets no entry again.
    pub(crate) fn report_functions(
        &self,
        database: u32,
        calls: &HashMap<u32, Calls>,
        exists: impl Fn(u32) -> bool,
    ) {
        let mut functions = self.functions.lock().unwrap_or_else(PoisonError::into_inner);
        for (&func, calls) in calls {
            if !exists(func) {
                continue;
            }
            let add = calls.counts();
            let counts = functions.entry((database, func)).or_default();
            counts.calls += add.calls;
            counts.total += add.total;
            counts.own += add.own;
        }
    }

    /// The counters of a function of a database, or `None` for a function with no entry.
    pub(crate) fn function_counts(&self, database: u32, func: u32) -> Option<FunctionCounts> {
        let functions = self.functions.lock().unwrap_or_else(PoisonError::into_inner);
        functions.get(&(database, func)).copied()
    }

    /// The counters of the functions of a database, for the snapshot of a transaction.
    pub(crate) fn all_function_counts(&self, database: u32) -> HashMap<u32, FunctionCounts> {
        let functions = self.functions.lock().unwrap_or_else(PoisonError::into_inner);
        functions
            .iter()
            .filter(|((db, _), _)| *db == database)
            .map(|(&(_, func), &c)| (func, c))
            .collect()
    }

    /// `pgstat_reset_counters` for `None`: sets the counters of the relations and the functions of a database to 0 at `time`. `pgstat_reset` for an object: sets the counters of the relation or the function to 0, when it has an entry.
    pub(crate) fn reset_counters(&self, database: u32, object: Option<u32>, time: i64) {
        let chosen = |db: u32, oid: u32| db == database && object.is_none_or(|o| o == oid);
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        for (&(db, oid), counts) in relations.iter_mut() {
            if chosen(db, oid) {
                *counts =
                    RelationCounts { stat_reset_time: Some(time), ..RelationCounts::default() };
            }
        }
        drop(relations);
        let mut functions = self.functions.lock().unwrap_or_else(PoisonError::into_inner);
        for (&(db, oid), counts) in functions.iter_mut() {
            if chosen(db, oid) {
                *counts = FunctionCounts { reset: Some(time), ..FunctionCounts::default() };
            }
        }
    }

    /// `pgstat_drop_transactional` at the commit of a change of the catalog of a database: drops the counters of the relations and the functions that the catalog no longer has.
    pub(crate) fn drop_counters(&self, database: u32, catalog: &Catalog) {
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        relations.retain(|&(db, oid), _| db != database || catalog.relation(oid).is_some());
        drop(relations);
        let mut functions = self.functions.lock().unwrap_or_else(PoisonError::into_inner);
        functions.retain(|&(db, oid), _| db != database || catalog.function(oid).is_some());
    }

    /// `AtEOXact_PgStat_Relations` and `pgstat_relation_flush_cb`: adds the rows that a transaction wrote to the counters of the relations of its database. Each write counts, also after a rollback. A commit adds the rows that the transaction inserted to the live rows and its updates and deletes to the dead rows. A rollback adds the rows that it inserted and updated to the dead rows. After a `TRUNCATE` that committed, the live and the dead rows start again from 0.
    pub(crate) fn report_writes(
        &self,
        database: u32,
        writes: &BTreeMap<u32, Pending>,
        commit: bool,
    ) {
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        for (&relation, pending) in writes {
            let counts = relations.entry((database, relation)).or_default();
            let aborted = pending.aborted;
            if commit {
                let kept = pending.kept;
                counts.written = counts.written + aborted + kept;
                if pending.truncated {
                    counts.live_tuples = 0;
                    counts.dead_tuples = 0;
                    counts.ins_since_vacuum = 0;
                } else {
                    counts.dead_tuples += aborted.inserted + aborted.updated;
                }
                counts.live_tuples += kept.inserted - kept.deleted;
                counts.dead_tuples += kept.updated + kept.deleted;
                counts.mod_since_analyze += kept.inserted + kept.updated + kept.deleted;
                counts.ins_since_vacuum += aborted.inserted + kept.inserted;
            } else {
                // A rollback brings back the counts before the first `TRUNCATE`, as `restore_truncdrop_counters`.
                let tried = aborted + if pending.truncated { pending.saved } else { pending.kept };
                counts.written = counts.written + tried;
                counts.dead_tuples += tried.inserted + tried.updated;
                counts.ins_since_vacuum += tried.inserted;
            }
            counts.live_tuples = counts.live_tuples.max(0);
            counts.dead_tuples = counts.dead_tuples.max(0);
        }
    }

    /// `pgstat_report_vacuum`: a `VACUUM` of a relation at `time` sets its live and dead rows, starts the inserted rows from 0 and counts the command.
    pub(crate) fn report_vacuum(&self, database: u32, relation: u32, live: i64, time: i64) {
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        let counts = relations.entry((database, relation)).or_default();
        counts.live_tuples = live;
        counts.dead_tuples = 0;
        counts.ins_since_vacuum = 0;
        counts.last_vacuum_time = Some(time);
        counts.vacuum_count += 1;
    }

    /// `pgstat_report_analyze`: an `ANALYZE` of a relation at `time` sets its live rows, starts the modified rows from 0 for `reset`, an `ANALYZE` with no column list, and counts the command. `live` is `None` for a partitioned table, which has no rows of its own.
    pub(crate) fn report_analyze(
        &self,
        database: u32,
        relation: u32,
        live: Option<i64>,
        reset: bool,
        time: i64,
    ) {
        let mut relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        let counts = relations.entry((database, relation)).or_default();
        match live {
            Some(live) => counts.live_tuples = live.max(0),
            None => (counts.live_tuples, counts.dead_tuples) = (0, 0),
        }
        if reset {
            counts.mod_since_analyze = 0;
        }
        counts.last_analyze_time = Some(time);
        counts.analyze_count += 1;
    }

    /// The counters of a relation of a database, or `None` for a relation with no entry.
    pub(crate) fn one_relation_counts(
        &self,
        database: u32,
        relation: u32,
    ) -> Option<RelationCounts> {
        let relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        relations.get(&(database, relation)).copied()
    }

    /// The counters of the relations of a database, for the snapshot of a transaction.
    pub(crate) fn relation_counts(&self, database: u32) -> HashMap<u32, RelationCounts> {
        let relations = self.relations.lock().unwrap_or_else(PoisonError::into_inner);
        relations
            .iter()
            .filter(|((db, _), _)| *db == database)
            .map(|(&(_, rel), &c)| (rel, c))
            .collect()
    }

    /// `pgstat_read_current_status`: a copy of the rows of the table, in the order of their numbers.
    pub(crate) fn backends(&self) -> Arc<[Backend]> {
        let heap = self.heap();
        self.backends_mut()
            .iter()
            .flatten()
            .map(|slot| {
                let backend = lock(&slot.backend).clone();
                let (xid, xmin) = heap.transaction_ids(backend.pid);
                Backend { xid, xmin, ..backend }
            })
            .collect()
    }
}

/// Locks the row of a session.
pub(crate) fn lock(row: &Row) -> MutexGuard<'_, Backend> {
    row.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// The roles of the last commit when the transaction took the lock.
    base: Arc<Roles>,
    /// The roles with the changes of the transaction.
    pub(crate) roles: Arc<Roles>,
}

impl Lease {
    /// The lock of the database, and the locks of the rows of `pg_database` of the session `pid`, go to `owner`, the owner of the locks of a prepared transaction, as `PostPrepare_Locks` gives them to the dummy `PGPROC`.
    pub(crate) fn hand_over(&self, pid: i32, owner: i32) {
        let mut state = self.store.state();
        for holder in state.row_locks.values_mut() {
            if *holder == pid {
                *holder = owner;
            }
        }
        if let Some(db) = state.databases.get_mut(&self.database)
            && db.locked == Some(pid)
        {
            db.locked = Some(owner);
        }
    }

    /// What the log keeps of the lease of the prepared transaction whose locks `owner` holds, after [`Lease::hand_over`]: the changes of the catalog and of the roles, and the rows of `pg_database` that the transaction locked.
    pub(crate) fn durable(&self, owner: i32) -> DurableLease {
        let state = self.store.state();
        let mut rows: Vec<u32> = state
            .row_locks
            .iter()
            .filter(|&(_, &holder)| holder == owner)
            .map(|(&row, _)| row)
            .collect();
        drop(state);
        rows.sort_unstable();
        DurableLease {
            catalog: Arc::clone(&self.catalog),
            base: Arc::clone(&self.base),
            roles: Arc::clone(&self.roles),
            rows,
        }
    }

    /// Makes the changes of the transaction the catalog of the last commit, and releases the lock. After the commit the new labels of the enum types are safe to use. The rows of `pg_database` and `pg_tablespace` that the transaction changed go to the store.
    pub(crate) fn commit(mut self) {
        let mut catalog = std::mem::take(&mut self.catalog);
        if catalog.has_uncommitted_enums() {
            Arc::make_mut(&mut catalog).commit_enums();
        }
        let (changes, tablespaces) =
            if self.roles.databases.is_empty() && self.roles.tablespaces.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                let roles = Arc::make_mut(&mut self.roles);
                (std::mem::take(&mut roles.databases), std::mem::take(&mut roles.tablespaces))
            };
        let mut state = self.store.state();
        // `GetNewObjectId` takes the OIDs of the roles and of the databases from the same counter, so the counter of the cluster takes the counter of the catalog before the drop of the lease, which finds no catalog.
        state.next_oid = state.next_oid.max(catalog.next_oid());
        if let Some(db) = state.databases.get_mut(&self.database) {
            db.committed = catalog;
        }
        for change in changes {
            if let Some(db) = state.databases.get_mut(&change.oid) {
                rupg_analyze::apply_database_change(&change, &mut db.row);
            }
        }
        if !tablespaces.is_empty() {
            let mut rows = state.tablespaces.to_vec();
            for change in &tablespaces {
                if let Some(row) = rows.iter_mut().find(|t| t.oid == change.oid) {
                    rupg_analyze::apply_tablespace_change(change, row);
                }
            }
            state.tablespaces = Arc::from(rows);
        }
        if !Arc::ptr_eq(&self.base, &self.roles) {
            state.roles = if Arc::ptr_eq(&state.roles, &self.base) {
                Arc::clone(&self.roles)
            } else {
                Arc::new(rebase(&state.roles, &self.base, &self.roles))
            };
        }
        self.store.changed();
        drop(state);
        self.store.log_catalog(false);
    }
}

/// The changes from `base` to `work` on the roles of a commit that came after `base`: the roles and the memberships that `work` added, changed and removed, and the comments that `work` changed.
fn rebase(now: &Roles, base: &Roles, work: &Roles) -> Roles {
    let mut roles: Vec<_> = now
        .roles
        .iter()
        .filter(|r| work.roles.iter().any(|w| w.oid == r.oid) || base.created(r.oid).is_none())
        .cloned()
        .collect();
    for role in &work.roles {
        match roles.iter_mut().find(|r| r.oid == role.oid) {
            Some(r) => {
                if base.created(role.oid) != Some(role) {
                    r.clone_from(role);
                }
            }
            None if base.created(role.oid).is_none() => roles.push(role.clone()),
            None => {}
        }
    }
    let mut members: Vec<_> = now
        .members
        .iter()
        .filter(|m| {
            work.members.iter().any(|w| w.oid == m.oid)
                || base.members.iter().all(|b| b.oid != m.oid)
        })
        .copied()
        .collect();
    for member in &work.members {
        let before = base.members.iter().find(|b| b.oid == member.oid);
        match members.iter_mut().find(|m| m.oid == member.oid) {
            Some(m) => {
                if before != Some(member) {
                    *m = *member;
                }
            }
            None if before.is_none() => members.push(*member),
            None => {}
        }
    }
    let mut comments = now.comments.clone();
    for comment in &work.comments {
        if !base.comments.contains(comment) {
            match comments.iter_mut().find(|c| c.class == comment.class && c.oid == comment.oid) {
                Some(c) => c.clone_from(comment),
                None => comments.push(comment.clone()),
            }
        }
    }
    Roles { roles, members, databases: Vec::new(), tablespaces: Vec::new(), comments }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.store.state();
        if let Some(holder) = state.databases.get(&self.database).and_then(|db| db.locked) {
            state.row_locks.retain(|_, pid| *pid != holder);
        }
        let next = self.catalog.next_oid();
        state.next_oid = state.next_oid.max(next);
        if let Some(db) = state.databases.get_mut(&self.database) {
            if db.committed.next_oid() < next {
                Arc::make_mut(&mut db.committed).advance_oid(next);
                self.store.changed();
            }
            db.locked = None;
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
        let mut lease = store.lease(POSTGRES, 1, &cancel, &|_| {}).unwrap();
        Arc::make_mut(&mut lease.catalog).create_schema("s", 10).unwrap();
        let next = lease.catalog.next_oid();
        drop(lease);
        assert!(store.committed(POSTGRES).schema_by_name("s").is_none());
        assert_eq!(store.committed(POSTGRES).next_oid(), next);
        let mut lease = store.lease(POSTGRES, 1, &cancel, &|_| {}).unwrap();
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
        let held = store.lease(POSTGRES, 1, &AtomicBool::new(false), &|_| {}).unwrap();
        let error = store.lease(POSTGRES, 2, &AtomicBool::new(true), &|_| {}).unwrap_err();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        drop(held);
        assert!(store.lease(POSTGRES, 2, &AtomicBool::new(false), &|_| {}).is_ok());
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
        let held = store.lease(POSTGRES, 1, &AtomicBool::new(false), &|_| {}).unwrap();
        *holder.0.lock().unwrap() = Some(held);
        assert!(store.lease(POSTGRES, 2, &AtomicBool::new(false), &|_| {}).is_ok());
        assert!(holder.0.lock().unwrap().is_none());
    }

    /// A session that waited holds the lock when it reports the end of the wait, so no other session can take the lock in the gap and lose the changes of one of the two transactions.
    #[test]
    fn the_waiter_holds_the_lock_at_the_end_of_the_wait() {
        let holder = Arc::new(Holder::default());
        let store = Arc::new(Store::with_tasks(holder.clone()));
        let held = store.lease(POSTGRES, 1, &AtomicBool::new(false), &|_| {}).unwrap();
        *holder.0.lock().unwrap() = Some(held);
        let locked = Mutex::new(None);
        let report = |waits: bool| {
            if !waits {
                *locked.lock().unwrap() = store.state().databases[&POSTGRES].locked;
            }
        };
        let lease = store.lease(POSTGRES, 2, &AtomicBool::new(false), &report).unwrap();
        assert_eq!(*locked.lock().unwrap(), Some(2));
        drop(lease);
    }

    #[test]
    fn the_end_of_the_transaction_frees_the_row_of_the_database() {
        let store = Arc::new(Store::new());
        let cancel = AtomicBool::new(false);
        let lease = store.lease(POSTGRES, 1, &cancel, &|_| {}).unwrap();
        store.lock_database_rows(1, [POSTGRES]);
        // The holder does not wait for itself, and another session sees it as the blocker.
        store.wait_database_row(POSTGRES, 1, &cancel, &|_| {}).unwrap();
        cancel.store(true, Ordering::Relaxed);
        let error = store.wait_database_row(POSTGRES, 2, &cancel, &|_| {}).unwrap_err();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        lease.commit();
        store.wait_database_row(POSTGRES, 2, &cancel, &|_| {}).unwrap();
    }
}

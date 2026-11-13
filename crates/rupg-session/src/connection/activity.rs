//! The row of the session in the table of the sessions, which `pg_stat_activity` shows: what `pgstat_bestart`, `pgstat_report_activity`, `pgstat_report_xact_timestamp` and `pgstat_report_appname` write, and the snapshot of the table that a transaction reads.
//!
//! The session reports the state `active` and the text of the statement when a `Query`, `Parse`, `Bind` or `Execute` starts, and the idle state that `ReadyForQuery` gives before it sends the message. The start of a transaction sets `xact_start`, and the end of a transaction clears it and drops the snapshot. When `track_activities` is off, the next report sets the state `disabled` and clears the text and the times, as in PostgreSQL.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use rupg_func::{
    Backend, BackendState, Client, FunctionCounts, Process, Progress, RelationCounts, Ssl, Writes,
};
use rupg_wire::TransactionStatus;

use super::Connection;
use crate::advisory::Fast;
use crate::guc::{self, Settings};
use crate::heap::Change;
use crate::query;
use crate::store::{self, Row, Store};

/// `NAMEDATALEN - 1`, the most bytes of `application_name` and of the texts of the TLS status in the table.
const NAME_BYTES: usize = 63;

/// The rows that a transaction wrote in one relation, `PgStat_TableXactStatus` and the counts of `PgStat_TableStatus` that the subtransactions that rolled back left.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pending {
    /// The writes since the last `TRUNCATE`, or since the start of the transaction.
    pub(crate) kept: Writes,
    /// The writes before the first `TRUNCATE`, which a rollback brings back.
    pub(crate) saved: Writes,
    /// True after a `TRUNCATE` of the relation.
    pub(crate) truncated: bool,
    /// The writes of the subtransactions that rolled back. They count, but leave only dead rows.
    pub(crate) aborted: Writes,
    /// All the writes that did not roll back, which tell the writes of a subtransaction from the writes before it.
    all: Writes,
}

/// The sequential scans of one relation that a transaction counted, and the rows that they read.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Scans {
    /// `numscans`.
    pub(crate) scans: i64,
    /// `tuples_returned`.
    pub(crate) returned: i64,
}

/// The calls of one function that the session counted and did not report, `PgStat_FunctionCounts`, with the times as `instr_time`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Calls {
    pub(crate) calls: i64,
    pub(crate) total: Duration,
    pub(crate) own: Duration,
}

impl Calls {
    /// The counts as the statistics keep them, with the times in microseconds.
    pub(crate) fn counts(&self) -> FunctionCounts {
        let micros = |time: Duration| i64::try_from(time.as_micros()).unwrap_or(i64::MAX);
        FunctionCounts {
            calls: self.calls,
            total: micros(self.total),
            own: micros(self.own),
            reset: None,
        }
    }
}

/// `stats_fetch_consistency`: how a transaction reads the counters of the statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// `none`: each read reads the counters of the server.
    None,
    /// `cache`: the first read of an object keeps its counters for the transaction.
    #[default]
    Cache,
    /// `snapshot`: the first read keeps the counters of all the objects of the database for the transaction.
    Snapshot,
}

/// The counters of one kind of objects that a transaction read, as [`Fetch`] keeps them.
#[derive(Debug, Default)]
enum Kept<T> {
    /// No read yet.
    #[default]
    Empty,
    /// The counters of each object that the transaction read, or `None` for an object with no entry.
    Cache(HashMap<u32, Option<T>>),
    /// The counters of all the objects of the database.
    Snapshot(Arc<HashMap<u32, T>>),
}

impl<T: Copy> Kept<T> {
    /// `pgstat_fetch_entry`: the counters of the object `oid`, from what the transaction kept, or from `load` for an object that it did not read yet and for `stats_fetch_consistency = none`.
    fn read(&mut self, oid: u32, fetch: Fetch, load: impl FnOnce() -> Option<T>) -> Option<T> {
        match self {
            Kept::Snapshot(all) => all.get(&oid).copied(),
            _ if fetch == Fetch::None => load(),
            Kept::Cache(cache) => *cache.entry(oid).or_insert_with(load),
            Kept::Empty => {
                let counts = load();
                *self = Kept::Cache(HashMap::from([(oid, counts)]));
                counts
            }
        }
    }
}

/// What a transaction read of the cumulative statistics, which its end or `pg_stat_clear_snapshot` drops.
#[derive(Debug, Default)]
struct Reads {
    relations: Kept<RelationCounts>,
    functions: Kept<FunctionCounts>,
    /// `blks_zeroed` of the SLRU `notify`, a statistic of a fixed amount, which `cache` also keeps.
    notify_zeroed: Option<i64>,
    /// True after the first read with `snapshot`, which took the counters of all the objects and the fixed statistics.
    snapshot: bool,
}

/// The process ID of the first process of the server. The process IDs of the sessions start at 1001.
const FIRST_SERVER_PID: i32 = 100;

/// The bootstrap superuser, the role of the logical replication launcher.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// The processes of the server that the postmaster of PostgreSQL starts with these settings, after the sessions, as `pg_stat_activity` shows them on a server with no work to do. Each one waits for work with the wait event of its main loop.
///
/// rupg has none of these processes. The rows tell a client which processes a server of PostgreSQL has. The numbers are the `ProcNumber` of each process on PostgreSQL: the autovacuum launcher has the first slot after the sessions, the logical replication launcher has a slot of the background workers, and the auxiliary processes come after `MaxBackends`. Each process started with the first session.
fn server_processes(settings: &Settings, start: i64) -> Vec<Backend> {
    let int = |name| settings.get(name).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
    let on = |name| settings.get(name).as_deref() == Some("on");
    let connections = int("max_connections");
    let autovacuum = int("autovacuum_worker_slots");
    let max_backends =
        connections + autovacuum + 1 + int("max_worker_processes") + int("max_wal_senders");
    let mut processes: Vec<(i32, u32, &'static str, &'static str)> = Vec::new();
    if on("autovacuum") {
        processes.push((connections, 0, "autovacuum launcher", "AutovacuumMain"));
    }
    if int("max_logical_replication_workers") > 0 {
        processes.push((
            connections + autovacuum + 2,
            BOOTSTRAP_SUPERUSER,
            "logical replication launcher",
            "LogicalLauncherMain",
        ));
    }
    let io_workers = if settings.get("io_method").as_deref() == Some("worker") {
        int("io_min_workers")
    } else {
        0
    };
    let mut auxiliary = Vec::new();
    if io_workers > 0 {
        auxiliary.push(("io worker", "IoWorkerMain"));
    }
    auxiliary.push(("background writer", "BgwriterMain"));
    auxiliary.extend((1..io_workers).map(|_| ("io worker", "IoWorkerMain")));
    auxiliary.push(("checkpointer", "CheckpointerMain"));
    auxiliary.push(("walwriter", "WalWriterMain"));
    if settings.get("archive_mode").as_deref() != Some("off") {
        auxiliary.push(("archiver", "ArchiverMain"));
    }
    if on("summarize_wal") {
        auxiliary.push(("walsummarizer", "WalSummarizerWal"));
    }
    processes.extend(
        (max_backends + 1..).zip(auxiliary).map(|(number, (name, event))| (number, 0, name, event)),
    );
    (FIRST_SERVER_PID..)
        .zip(processes)
        .map(|(pid, (number, role, name, event))| Backend {
            number,
            pid,
            role,
            process: Process::Server(name),
            state: BackendState::Undefined,
            backend_start: start,
            waiting: Some(("Activity", event)),
            ..Backend::default()
        })
        .collect()
}

/// What a session knows of the table of the sessions.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    /// The number of the session in the table, after [`Connection::greet`].
    pub(super) number: Option<i32>,
    /// The row of the session in the table, after [`Connection::greet`].
    row: Option<Row>,
    /// The two ends of the connection.
    pub(crate) client: Client,
    pub(crate) server: Client,
    /// The TLS of the connection, if it has TLS.
    ssl: Option<Ssl>,
    /// The snapshot of the table that the transaction reads, after its first read.
    snapshot: RefCell<Option<Arc<[Backend]>>>,
    /// The OID of the database of the session.
    database: u32,
    /// The sequential scans of each relation that the transaction counted. The end of the transaction reports them to the store.
    scans: RefCell<BTreeMap<u32, Scans>>,
    /// The rows that the transaction wrote in each relation. The end of the transaction reports them to the store.
    writes: RefCell<BTreeMap<u32, Pending>>,
    /// The counters of the statistics that the transaction read.
    reads: RefCell<Reads>,
    /// The calls of each function that the session counted. The end of the transaction reports them to the store.
    functions: RefCell<HashMap<u32, Calls>>,
    /// `fcu->save_f_total_time`: for each counted call that runs, from the outer call in, the time of the counted calls that it made.
    nested: RefCell<Vec<Duration>>,
    /// `debug_query_string`: the text of the message that runs, in full. It is `None` while the session waits for the client.
    pub(crate) query: Option<String>,
    /// The buffer of the last text of [`Activity::query`], which the next statement fills again with no new allocation.
    spare: String,
    /// `SYSTEM_USER`, as `auth_method:authn_id`, or `None` for a method that checks no identity.
    pub(crate) system_user: Option<String>,
    /// True while the row of the session has the progress of a command.
    progress: Cell<bool>,
    /// The fast path of the weak locks of the transaction on relations, after [`Connection::join`].
    pub(crate) fast: Option<Arc<Fast>>,
}

impl Activity {
    /// The number of the session in the table, after [`Connection::greet`].
    pub(crate) fn number(&self) -> Option<i32> {
        self.number
    }

    /// The OID of the database of the session.
    pub(crate) fn database(&self) -> u32 {
        self.database
    }

    /// Changes the row of the session in the table, if the session has a row.
    fn change(&self, change: impl FnOnce(&mut Backend)) {
        if let Some(row) = &self.row {
            change(&mut store::lock(row));
        }
    }

    /// The snapshot of the transaction. The first read takes it, after it writes the current `application_name` of the session, which PostgreSQL writes when the setting changes.
    pub(crate) fn backends(&self, store: &Store, settings: &Settings) -> Arc<[Backend]> {
        let mut snapshot = self.snapshot.borrow_mut();
        if let Some(rows) = &*snapshot {
            return Arc::clone(rows);
        }
        if self.row.is_some() {
            let application = application_name(settings);
            self.change(|backend| backend.application = application);
        }
        let mut rows = store.backends().to_vec();
        rows.extend(server_processes(settings, store.stats_reset()));
        let rows: Arc<[Backend]> = rows.into();
        *snapshot = Some(Arc::clone(&rows));
        rows
    }

    /// `pgstat_report_wait_start` and `pgstat_report_wait_end`: the type and the name of the wait event of a wait in a statement, or `None` at the end of the wait.
    pub(crate) fn report_wait(&self, event: Option<(&'static str, &'static str)>) {
        self.change(|backend| backend.waiting = event);
    }

    /// `pgstat_count_heap_scan`: counts a sequential scan of a relation.
    pub(crate) fn count_scan(&self, relation: u32) {
        self.scans.borrow_mut().entry(relation).or_default().scans += 1;
    }

    /// `pgstat_count_heap_getnext`: counts the rows that a sequential scan of a relation read.
    pub(crate) fn count_returned(&self, relation: u32, rows: usize) {
        self.scans.borrow_mut().entry(relation).or_default().returned += count(rows);
    }

    /// The sequential scans of a relation that the transaction counted.
    pub(crate) fn pending_scans(&self, relation: u32) -> i64 {
        self.scans.borrow().get(&relation).map_or(0, |s| s.scans)
    }

    /// The rows that the sequential scans of the transaction read in a relation.
    pub(crate) fn pending_returned(&self, relation: u32) -> i64 {
        self.scans.borrow().get(&relation).map_or(0, |s| s.returned)
    }

    /// `pgstat_init_function_usage` and `pgstat_end_function_usage`: runs a call of the function `func` and counts it when it ends with no error. The time of the call counts in the total time of the counted call that made it, and not in its own time.
    pub(crate) fn count_call<T>(
        &self,
        clock: &dyn rupg_platform::Clock,
        func: u32,
        call: impl FnOnce() -> T,
        ok: fn(&T) -> bool,
    ) -> T {
        self.nested.borrow_mut().push(Duration::ZERO);
        let start = clock.monotonic();
        let result = call();
        let total = clock.monotonic().saturating_sub(start);
        let mut nested = self.nested.borrow_mut();
        let inner = nested.pop().unwrap_or_default();
        if ok(&result) {
            if let Some(outer) = nested.last_mut() {
                *outer += total;
            }
            let mut functions = self.functions.borrow_mut();
            let calls = functions.entry(func).or_default();
            calls.calls += 1;
            calls.total += total;
            calls.own += total.saturating_sub(inner);
        }
        result
    }

    /// The calls of a function that the session counted and did not report.
    pub(crate) fn pending_function(&self, func: u32) -> Option<FunctionCounts> {
        self.functions.borrow().get(&func).map(Calls::counts)
    }

    /// The calls of the functions that the session counted, which the end of the transaction reports.
    pub(crate) fn take_functions(&self) -> HashMap<u32, Calls> {
        self.functions.take()
    }

    /// `pgstat_count_heap_insert`, `pgstat_count_heap_update` and `pgstat_count_heap_delete`: counts the rows that a statement wrote in a relation.
    pub(crate) fn count_writes(&self, relation: u32, writes: Writes) {
        let mut all = self.writes.borrow_mut();
        let pending = all.entry(relation).or_default();
        pending.kept = pending.kept + writes;
        pending.all = pending.all + writes;
    }

    /// `pgstat_count_truncate`: the writes before a `TRUNCATE` of a relation do not count after a commit. The first `TRUNCATE` keeps them for a rollback.
    pub(crate) fn count_truncate(&self, relation: u32) {
        let mut all = self.writes.borrow_mut();
        let pending = all.entry(relation).or_default();
        if !pending.truncated {
            pending.saved = pending.kept;
            pending.truncated = true;
        }
        pending.kept = Writes::default();
    }

    /// The rows that the transaction wrote in a relation, as the functions `pg_stat_get_xact_tuples_*` count them.
    pub(crate) fn pending_writes(&self, relation: u32) -> Writes {
        self.writes.borrow().get(&relation).map_or_else(Writes::default, |p| p.aborted + p.kept)
    }

    /// A copy of the writes of the transaction for a savepoint.
    pub(crate) fn writes(&self) -> BTreeMap<u32, Pending> {
        self.writes.borrow().clone()
    }

    /// `AtEOSubXact_PgStat_Relations` at the rollback of a subtransaction: the writes go back to `before`, the writes at the savepoint, and the writes after the savepoint count as rolled back.
    pub(crate) fn roll_back_writes(&self, before: &BTreeMap<u32, Pending>) {
        for (relation, pending) in self.writes.borrow_mut().iter_mut() {
            let old = before.get(relation).copied().unwrap_or_default();
            let aborted = pending.aborted + (pending.all - old.all);
            *pending = Pending { aborted, ..old };
        }
    }

    /// The writes of the transaction, which its end reports.
    pub(crate) fn take_writes(&self) -> BTreeMap<u32, Pending> {
        self.writes.take()
    }

    /// What the transaction read of the statistics. The first read with `snapshot` takes the counters of all the objects of the database and the fixed statistics, as `pgstat_build_snapshot`.
    fn reads(&self, store: &Store, fetch: Fetch) -> std::cell::RefMut<'_, Reads> {
        let mut reads = self.reads.borrow_mut();
        if fetch == Fetch::Snapshot && !reads.snapshot {
            reads.relations = Kept::Snapshot(Arc::new(store.relation_counts(self.database)));
            reads.functions = Kept::Snapshot(Arc::new(store.all_function_counts(self.database)));
            reads.notify_zeroed = Some(store.listeners().zeroed());
            reads.snapshot = true;
        }
        reads
    }

    /// The counters of a relation as `fetch` reads them in the transaction.
    pub(crate) fn relation_counts(
        &self,
        store: &Store,
        relation: u32,
        fetch: Fetch,
    ) -> Option<RelationCounts> {
        let mut reads = self.reads(store, fetch);
        reads.relations.read(relation, fetch, || store.one_relation_counts(self.database, relation))
    }

    /// The counters of a function as `fetch` reads them in the transaction.
    pub(crate) fn function_counts(
        &self,
        store: &Store,
        func: u32,
        fetch: Fetch,
    ) -> Option<FunctionCounts> {
        let mut reads = self.reads(store, fetch);
        reads.functions.read(func, fetch, || store.function_counts(self.database, func))
    }

    /// `blks_zeroed` of the SLRU `notify` as `fetch` reads it in the transaction.
    pub(crate) fn notify_zeroed(&self, store: &Store, fetch: Fetch) -> i64 {
        let mut reads = self.reads(store, fetch);
        match reads.notify_zeroed {
            Some(zeroed) if fetch != Fetch::None || reads.snapshot => zeroed,
            _ => {
                let zeroed = store.listeners().zeroed();
                if fetch != Fetch::None {
                    reads.notify_zeroed = Some(zeroed);
                }
                zeroed
            }
        }
    }

    /// `pgstat_clear_snapshot`: drops the snapshots of the table of the sessions and of the counters of the relations and the functions.
    pub(crate) fn clear(&self) {
        self.snapshot.borrow_mut().take();
        self.reads.take();
    }
}

/// A count of rows as a counter.
fn count(rows: usize) -> i64 {
    i64::try_from(rows).unwrap_or(i64::MAX)
}

/// The value of `application_name`, cut to the bytes that the table keeps.
pub(super) fn application_name(settings: &Settings) -> String {
    let mut name = settings.get_of(guc::known!("application_name")).unwrap_or_default();
    clip(&mut name, NAME_BYTES);
    name
}

/// `pg_mbcliplen`: cuts the text to at most `max` bytes at the end of a character.
fn clip(text: &mut String, max: usize) {
    let end = clip_len(text, max);
    text.truncate(end);
}

/// The length of the text that [`clip`] keeps.
fn clip_len(text: &str, max: usize) -> usize {
    if text.len() > max {
        (0..=max).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0)
    } else {
        text.len()
    }
}

/// The TLS status as the table keeps it: `pgstat_bestart` copies each text with `strlcpy` into `NAMEDATALEN` bytes. rupg cuts a long text at the end of a character, so that the text stays valid UTF-8.
fn clipped(mut ssl: Ssl) -> Ssl {
    for text in [
        &mut ssl.version,
        &mut ssl.cipher,
        &mut ssl.client_dn,
        &mut ssl.client_serial,
        &mut ssl.issuer_dn,
    ] {
        clip(text, NAME_BYTES);
    }
    ssl
}

impl Connection {
    /// Sets the two ends of the connection, the client and the server, before [`Connection::greet`]. A connection with no addresses shows null addresses.
    pub fn set_addresses(&mut self, client: Client, server: Client) {
        self.activity.client = client;
        self.activity.server = server;
    }

    /// Sets `SYSTEM_USER` after the authentication, before [`Connection::greet`].
    pub fn set_system_user(&mut self, system_user: Option<String>) {
        self.activity.system_user = system_user;
    }

    /// Sets the TLS of the connection, before [`Connection::greet`].
    pub fn set_ssl(&mut self, ssl: Ssl) {
        self.activity.ssl = Some(clipped(ssl));
    }

    /// `pgstat_bestart`: puts the session in the table of its store.
    pub(super) fn join(&mut self) {
        let backend = Backend {
            pid: self.pid,
            database: self.database_oid,
            role: self.role,
            application: application_name(self.settings.get_mut()),
            backend_start: query::now(&*self.clock),
            client: self.activity.client,
            ssl: self.activity.ssl.clone(),
            ..Backend::default()
        };
        self.activity.database = self.database_oid;
        let (number, row) =
            self.store.join_row(backend, Arc::clone(&self.end), Arc::clone(&self.cancel));
        self.activity.number = Some(number);
        self.activity.row = Some(row);
        self.activity.fast = Some(self.store.locks().join(self.pid));
    }

    /// `pgstat_report_activity`: the state of the session, and the text of the statement that starts when `text` is `Some`.
    pub(super) fn report(&mut self, state: BackendState, text: Option<&str>) {
        if let Some(text) = text {
            match &mut self.activity.query {
                Some(query) if query == text => {}
                query => {
                    let mut kept =
                        query.take().unwrap_or_else(|| std::mem::take(&mut self.activity.spare));
                    kept.clear();
                    kept.push_str(text);
                    *query = Some(kept);
                }
            }
        }
        if self.activity.row.is_none() {
            return;
        }
        let known = self.known.get_mut().of(self.settings.get_mut());
        let tracked = known.tracked;
        // `pgstat_report_query_id`: the identifier of the first statement of the text, which parse analysis gives when `compute_query_id` is `on` or `regress`.
        let query_id = match (state, text) {
            (BackendState::Running, Some(text)) if known.query_id => rupg_sql::query_id(text),
            _ => 0,
        };
        let text = text.map(|text| &text[..clip_len(text, known.query_bytes)]);
        let now = query::now(&*self.clock);
        let start = self.statement_start;
        self.activity.change(|backend| {
            if backend.application != known.application {
                backend.application.clone_from(&known.application);
            }
            backend.reading = state != BackendState::Running;
            if !tracked {
                if backend.state != BackendState::Disabled {
                    backend.state = BackendState::Disabled;
                    backend.state_change = None;
                    backend.query.clear();
                    backend.query_start = None;
                    backend.transaction_start = None;
                    backend.query_id = 0;
                }
                return;
            }
            // A new statement resets the identifier, so that the identifier of the last statement does not show.
            if state == BackendState::Running {
                backend.query_id = query_id;
            }
            backend.state = state;
            backend.state_change = Some(now);
            if let Some(text) = text {
                // The text of a prepared statement that runs again is the same, so the row keeps it.
                if backend.query != text {
                    backend.query.clear();
                    backend.query.push_str(text);
                }
                backend.query_start = Some(start);
            }
        });
    }

    /// The idle state that `ReadyForQuery` reports.
    pub(super) fn report_idle(&mut self) {
        let state = match self.transaction.status() {
            TransactionStatus::Idle => BackendState::Idle,
            TransactionStatus::Block => BackendState::IdleInTransaction,
            TransactionStatus::Failed => BackendState::IdleInTransactionAborted,
        };
        if let Some(query) = self.activity.query.take() {
            self.activity.spare = query;
        }
        self.report(state, None);
        // Out of a statement, only the snapshot of a transaction in `REPEATABLE READ` stays, as `SnapshotResetXmin` keeps the registered snapshots.
        if state == BackendState::IdleInTransactionAborted || self.snapshot.borrow().is_none() {
            self.release_xmin();
        }
    }

    /// `pgstat_report_xact_timestamp`: the start of the transaction, or `None` at its end.
    fn report_transaction(&self, start: Option<i64>) {
        if self.activity.row.is_some() && self.tracked() {
            self.activity.change(|backend| backend.transaction_start = start);
        }
    }

    /// `pgstat_progress_start_command` and `pgstat_progress_update_multi_param`: the progress of the command that the session runs, or `None` at its end, `pgstat_progress_end_command`. A session with `track_activities` off reports no progress.
    pub(super) fn report_progress(&self, progress: Option<Progress>) {
        if self.activity.row.is_none() || progress.is_some() && !self.tracked() {
            return;
        }
        if progress.is_some() || self.activity.progress.get() {
            self.activity.progress.set(progress.is_some());
            self.activity.change(|backend| backend.progress = progress);
        }
    }

    /// The start of a transaction.
    pub(super) fn transaction_started(&self) {
        self.report_transaction(Some(self.transaction_start));
    }

    /// Counts the rows that a statement wrote in a relation, when `track_counts` is on.
    pub(super) fn count_writes(&self, relation: u32, writes: Writes) {
        if writes != Writes::default() && self.counts_tracked() {
            self.activity.count_writes(relation, writes);
        }
    }

    /// Counts the rows that a statement changed in a relation: each change in `applied` with a new version is an update, and each change with no new version is a delete.
    pub(super) fn count_changes(&self, relation: u32, changes: &[Change], applied: &[usize]) {
        let updated =
            applied.iter().filter(|&&at| changes.get(at).is_some_and(|c| c.1.is_some())).count();
        let deleted = applied.len() - updated;
        self.count_writes(
            relation,
            Writes { inserted: 0, updated: count(updated), deleted: count(deleted) },
        );
    }

    /// Counts the rows that a statement inserted in a relation.
    pub(super) fn count_inserts(&self, relation: u32, inserted: usize) {
        self.count_writes(relation, Writes { inserted: count(inserted), ..Writes::default() });
    }

    /// `pgstat_count_truncate` for a `TRUNCATE` of a relation.
    pub(super) fn count_truncate(&self, relation: u32) {
        if self.counts_tracked() {
            self.activity.count_truncate(relation);
        }
    }

    /// `pgstat_track_counts`.
    pub(super) fn counts_tracked(&self) -> bool {
        self.known.borrow_mut().of(&self.settings.borrow()).counts
    }

    /// `pgstat_track_functions`: 0 for `none`, 1 for `pl` and 2 for `all`.
    pub(crate) fn functions_tracked(&self) -> u8 {
        self.known.borrow_mut().of(&self.settings.borrow()).functions
    }

    /// `pgstat_fetch_consistency`.
    pub(crate) fn fetch_consistency(&self) -> Fetch {
        self.known.borrow_mut().of(&self.settings.borrow()).fetch
    }

    /// A call of the function `func` in a language that `track_functions` counts from the value `level`: 1 for PL/pgSQL and 2 for SQL.
    pub(crate) fn tracked_call<T>(
        &self,
        func: u32,
        level: u8,
        call: impl FnOnce() -> rupg_common::Result<T>,
    ) -> rupg_common::Result<T> {
        if self.functions_tracked() < level {
            return call();
        }
        self.activity.count_call(&*self.clock, func, call, rupg_common::Result::is_ok)
    }

    /// `pgstat_track_activities`.
    fn tracked(&self) -> bool {
        self.known.borrow_mut().of(&self.settings.borrow()).tracked
    }

    /// The end of a transaction at a commit or at an abort: the session releases its advisory locks of the transaction level, `AtEOXact_PgStat` drops the snapshots, and `pgstat_report_stat` reports the scans of the transaction. A scan counts also after an abort, as in PostgreSQL.
    pub(super) fn transaction_ended(&self) {
        self.large_objects.borrow_mut().clear();
        let locks = self.store.locks();
        if let Some(fast) = &self.activity.fast {
            locks.release_fast(fast);
        }
        locks.release_all(self.pid, false);
        self.activity.clear();
        // `AbortTransaction` ends the progress of a command that failed.
        self.report_progress(None);
        let scans = self.activity.scans.take();
        if !scans.is_empty() {
            self.store.report_scans(self.database_oid, &scans, query::now(&*self.clock));
        }
        let functions = self.activity.take_functions();
        if !functions.is_empty() {
            let catalog = self.store.committed(self.database_oid);
            self.store.report_functions(self.database_oid, &functions, |func| {
                catalog.function(func).is_some()
            });
        }
        self.report_transaction(None);
    }

    /// The end of the session, which releases its advisory locks and frees its row of the table.
    pub(super) fn leave(&mut self) {
        let locks = self.store.locks();
        if let Some(fast) = self.activity.fast.take() {
            locks.leave(&fast);
        }
        locks.release_all(self.pid, false);
        locks.release_all(self.pid, true);
        self.store.heap().release(self.pid);
        self.store.listeners().leave(self.pid);
        self.activity.row = None;
        if let Some(number) = self.activity.number.take() {
            self.store.leave(number);
        }
    }
}

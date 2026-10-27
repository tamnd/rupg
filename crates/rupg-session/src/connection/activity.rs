//! The row of the session in the table of the sessions, which `pg_stat_activity` shows: what `pgstat_bestart`, `pgstat_report_activity`, `pgstat_report_xact_timestamp` and `pgstat_report_appname` write, and the snapshot of the table that a transaction reads.
//!
//! The session reports the state `active` and the text of the statement when a `Query`, `Parse`, `Bind` or `Execute` starts, and the idle state that `ReadyForQuery` gives before it sends the message. The start of a transaction sets `xact_start`, and the end of a transaction clears it and drops the snapshot. When `track_activities` is off, the next report sets the state `disabled` and clears the text and the times, as in PostgreSQL.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rupg_func::{Backend, BackendState, Client, Process, RelationCounts, Ssl, Writes};
use rupg_wire::TransactionStatus;

use super::Connection;
use crate::guc::{self, Setting, Settings};
use crate::heap::Change;
use crate::query;
use crate::store::Store;

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
    scans: RefCell<BTreeMap<u32, i64>>,
    /// The rows that the transaction wrote in each relation. The end of the transaction reports them to the store.
    writes: RefCell<BTreeMap<u32, Pending>>,
    /// The snapshot of the counters of the relations that the transaction reads, after its first read.
    relations: RefCell<Option<Arc<HashMap<u32, RelationCounts>>>>,
    /// `debug_query_string`: the text of the message that runs, in full. It is `None` while the session waits for the client.
    pub(crate) query: Option<String>,
    /// `SYSTEM_USER`, as `auth_method:authn_id`, or `None` for a method that checks no identity.
    pub(crate) system_user: Option<String>,
}

impl Activity {
    /// The number of the session in the table, after [`Connection::greet`].
    pub(crate) fn number(&self) -> Option<i32> {
        self.number
    }

    /// The snapshot of the transaction. The first read takes it, after it writes the current `application_name` of the session, which PostgreSQL writes when the setting changes.
    pub(crate) fn backends(&self, store: &Store, settings: &Settings) -> Arc<[Backend]> {
        let mut snapshot = self.snapshot.borrow_mut();
        if let Some(rows) = &*snapshot {
            return Arc::clone(rows);
        }
        if let Some(number) = self.number {
            let application = application_name(settings);
            store.report(number, |backend| backend.application = application);
        }
        let mut rows = store.backends().to_vec();
        rows.extend(server_processes(settings, store.stats_reset()));
        let rows: Arc<[Backend]> = rows.into();
        *snapshot = Some(Arc::clone(&rows));
        rows
    }

    /// `pgstat_report_wait_start` and `pgstat_report_wait_end`: the type and the name of the wait event of a wait in a statement, or `None` at the end of the wait.
    pub(crate) fn report_wait(&self, store: &Store, event: Option<(&'static str, &'static str)>) {
        if let Some(number) = self.number {
            store.report(number, |backend| backend.waiting = event);
        }
    }

    /// `pgstat_count_heap_scan`: counts a sequential scan of a relation.
    pub(crate) fn count_scan(&self, relation: u32) {
        *self.scans.borrow_mut().entry(relation).or_default() += 1;
    }

    /// The sequential scans of a relation that the transaction counted.
    pub(crate) fn pending_scans(&self, relation: u32) -> i64 {
        self.scans.borrow().get(&relation).copied().unwrap_or(0)
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

    /// The counters of a relation from the snapshot of the transaction. The first read takes the snapshot.
    pub(crate) fn relation_counts(&self, store: &Store, relation: u32) -> Option<RelationCounts> {
        let mut snapshot = self.relations.borrow_mut();
        let counts = snapshot.get_or_insert_with(|| Arc::new(store.relation_counts(self.database)));
        counts.get(&relation).copied()
    }

    /// `pgstat_clear_snapshot`: drops the snapshots of the table of the sessions and of the counters of the relations.
    pub(crate) fn clear(&self) {
        self.snapshot.borrow_mut().take();
        self.relations.borrow_mut().take();
    }
}

/// A count of rows as a counter.
fn count(rows: usize) -> i64 {
    i64::try_from(rows).unwrap_or(i64::MAX)
}

/// The value of `application_name`, cut to the bytes that the table keeps.
fn application_name(settings: &Settings) -> String {
    let mut name = settings.get("application_name").unwrap_or_default();
    clip(&mut name, NAME_BYTES);
    name
}

/// `pg_mbcliplen`: cuts the text to at most `max` bytes at the end of a character.
fn clip(text: &mut String, max: usize) {
    if text.len() > max {
        let end = (0..=max).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0);
        text.truncate(end);
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

/// `pgstat_track_activities`.
fn tracked(settings: &Settings) -> bool {
    settings.get("track_activities").as_deref() != Some("off")
}

/// `pgstat_track_activity_query_size - 1`, the most bytes of the text of a statement in the table.
fn query_bytes(settings: &Settings) -> usize {
    let size = match guc::find("track_activity_query_size").map(|p| settings.setting(p)) {
        Some(Setting::Int(size)) => size,
        _ => 1024,
    };
    usize::try_from(size).map_or(0, |size| size.saturating_sub(1))
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
        self.activity.number = Some(self.store.join(backend, Arc::clone(&self.end)));
    }

    /// `pgstat_report_activity`: the state of the session, and the text of the statement that starts when `text` is `Some`.
    pub(super) fn report(&mut self, state: BackendState, text: Option<&str>) {
        if let Some(text) = text {
            self.activity.query = Some(text.to_owned());
        }
        let Some(number) = self.activity.number else { return };
        let settings = self.settings.get_mut();
        let application = application_name(settings);
        let tracked = tracked(settings);
        let text = text.map(|text| {
            let mut text = text.to_owned();
            clip(&mut text, query_bytes(settings));
            text
        });
        let now = query::now(&*self.clock);
        let start = self.statement_start;
        self.store.report(number, |backend| {
            backend.application = application;
            backend.reading = state != BackendState::Running;
            if !tracked {
                if backend.state != BackendState::Disabled {
                    backend.state = BackendState::Disabled;
                    backend.state_change = None;
                    backend.query.clear();
                    backend.query_start = None;
                    backend.transaction_start = None;
                }
                return;
            }
            backend.state = state;
            backend.state_change = Some(now);
            if let Some(text) = text {
                backend.query = text;
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
        self.activity.query = None;
        self.report(state, None);
        // Out of a statement, only the snapshot of a transaction in `REPEATABLE READ` stays, as `SnapshotResetXmin` keeps the registered snapshots.
        if state == BackendState::IdleInTransactionAborted || self.snapshot.borrow().is_none() {
            self.store.heap().release(self.pid);
        }
    }

    /// `pgstat_report_xact_timestamp`: the start of the transaction, or `None` at its end.
    fn report_transaction(&self, start: Option<i64>) {
        let Some(number) = self.activity.number else { return };
        if tracked(&self.settings.borrow()) {
            self.store.report(number, |backend| backend.transaction_start = start);
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
    fn counts_tracked(&self) -> bool {
        self.settings.borrow().get("track_counts").as_deref() != Some("off")
    }

    /// The end of a transaction at a commit or at an abort: the session releases its advisory locks of the transaction level, `AtEOXact_PgStat` drops the snapshots, and `pgstat_report_stat` reports the scans of the transaction. A scan counts also after an abort, as in PostgreSQL.
    pub(super) fn transaction_ended(&self) {
        self.store.locks().release_all(self.pid, false);
        self.activity.clear();
        let scans = self.activity.scans.take();
        if !scans.is_empty() {
            self.store.report_scans(self.database_oid, &scans, query::now(&*self.clock));
        }
        self.report_transaction(None);
    }

    /// The end of the session, which releases its advisory locks and frees its row of the table.
    pub(super) fn leave(&mut self) {
        let locks = self.store.locks();
        locks.release_all(self.pid, false);
        locks.release_all(self.pid, true);
        self.store.heap().release(self.pid);
        self.store.listeners().leave(self.pid);
        if let Some(number) = self.activity.number.take() {
            self.store.leave(number);
        }
    }
}

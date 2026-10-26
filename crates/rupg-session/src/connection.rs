//! One client connection after the startup packet: the settings of the session, the messages of the start, and the main loop of `PostgresMain` for the simple and the extended query protocol.
//!
//! The connection does no I/O. The server gives it the bytes that the client sent and an output buffer, and [`Connection::step`] tells the server what to do next. See `spec/06-server-and-wire.md` section 6.1.

use std::cell::{Cell, Ref, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rupg_analyze::{Derived, Message, Params, SchemaElements, Write, WriteKind};
use rupg_catalog::{Catalog, database};
use rupg_common::{Error, Level as ErrorLevel, Object, SqlState};
use rupg_exec::Plan;
use rupg_func::{BackendState, Randomness, Sequences};
pub use rupg_func::{Client, Ssl};
use rupg_pgcatalog::builtin;
use rupg_pgcatalog::roles::Roles;
use rupg_platform::os::OsClock;
use rupg_platform::{Clock, Io};
use rupg_sql::Severity;
use rupg_sql::nodes::{
    A_Star, AlterTableType, ColumnRef, InsertStmt, LockStmt, Node, ObjectType, RangeVar, ResTarget,
    SelectStmt,
};
use rupg_types::Value;
use rupg_wire::{
    CancelKey, CommandTag, Field, Frontend, Level, OutBuf, Portals, ProtocolError, Session,
    Statements, source_fields,
};

use crate::block::{Block, Ending, Transaction};
use crate::guc::{Action, Origin, Settings};
use crate::heap::{Change, Changed, Heap, Recheck, Snapshot, View, tid_text};
use crate::query::{self, Column, Reader, Row, Transition};
use crate::store::{Lease, Store};
use crate::utility::{self, Context, Notice, Outcome};
use foreign::Batch;

mod activity;
mod analyze;
mod copy;
mod copy_from;
mod cursor;
mod databases;
mod deferred;
mod explain;
mod extended;
mod foreign;
mod listen;
mod merge;
mod plpgsql;
mod prepare;
mod reindex;
mod rewrite;
mod roles;
mod route;
mod savepoint;
mod tablespaces;
mod trigger;
mod upsert;
mod vacuum;
mod writes;

pub(crate) use activity::{Activity, Pending};
use copy_from::Begin;
pub(crate) use explain::join_order;
use extended::Failed;
pub(crate) use extended::{Portal, Prepared};
use writes::Writes;

/// The version of rupg, which `server_version` and the parameter `rupg.version` report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the startup packet asked for, after the server accepted it.
#[derive(Clone, Debug, Default)]
pub struct Start {
    pub user: String,
    pub database: String,
    /// True when the user is a superuser.
    pub superuser: bool,
    /// The `options` parameter of the startup packet.
    pub options: Option<String>,
    /// The other parameters of the startup packet, in their order.
    pub settings: Vec<(String, String)>,
}

/// `server_version` as the spec gives it: the major version of PostgreSQL and the version of rupg.
pub fn server_version() -> String {
    format!("19.0 (rupg {VERSION})")
}

/// The text of `version()`: the version, the target triple of the build and the version of the compiler, in the form of PostgreSQL (spec/05 section 5.7).
pub fn version_text() -> String {
    format!(
        "PostgreSQL 19.0 (rupg {VERSION}) on {}, compiled by rustc {}, {}-bit",
        env!("RUPG_TARGET"),
        env!("RUPG_RUSTC"),
        usize::BITS
    )
}

/// The result of the write of the rows of a statement.
#[derive(Debug)]
struct Applied {
    tag: CommandTag,
    /// The number of rows that the statement wrote.
    count: u64,
    /// The rows that `RETURNING` reads.
    written: Vec<Vec<Value>>,
    /// The view of the last try of the write.
    view: View,
    /// The rows that the triggers of the foreign keys get, when the table has foreign keys and the statement wrote rows. `INSERT ... ON CONFLICT DO UPDATE` gives a batch for the rows that went in, then a batch for the rows that changed.
    batches: Vec<Batch>,
}

/// The result of a statement as the session sends it.
#[derive(Debug)]
enum Done {
    /// `CommandComplete` with this tag and no rows.
    Tag(CommandTag),
    /// `RowDescription`, the rows, then `CommandComplete` with the tag and the number of rows.
    Rows { columns: Vec<Column>, rows: Vec<Row>, tag: CommandTag },
    /// The rows of the list of `RETURNING` of the action of a rule, then `CommandComplete` with the tag and the number of rows of the statement that sets the tag.
    Returned { columns: Vec<Column>, rows: Vec<Row>, tag: CommandTag, count: u64 },
    /// The rows of a `FETCH`: `RowDescription` in the format, the rows, then `CommandComplete` with `FETCH` and the number of rows.
    Fetch { columns: Vec<Column>, rows: Vec<Row>, format: i16 },
    /// `CommandComplete` with this tag and a count, as `MOVE` gives.
    Count(CommandTag, u64),
    /// `CopyOutResponse`, the `CopyData` messages, `CopyDone`, then `CommandComplete` with `COPY` and the number of rows.
    Copy(copy::CopyOut),
}

impl From<Outcome> for Done {
    fn from(outcome: Outcome) -> Done {
        match outcome {
            Outcome::Tag(tag) => Done::Tag(tag),
            Outcome::Rows { columns, rows } => Done::Rows {
                columns: columns.into_iter().map(Column::text).collect(),
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(|v| Some(v.into_bytes())).collect())
                    .collect(),
                tag: CommandTag::Show,
            },
        }
    }
}

/// The values that every session starts with: the values that the server owns, then the values of the command line. The values that depend on the session come later in [`session_settings`].
///
/// # Errors
///
/// A value of the command line that is not valid.
pub fn base_settings(args: &[(String, String)]) -> Result<Settings, Error> {
    let mut settings = Settings::new(false);
    let version = server_version();
    let internal = [
        ("server_version", version.as_str()),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("TimeZone", "UTC"),
        ("log_timezone", "UTC"),
        ("lc_messages", "C"),
        ("lc_monetary", "C"),
        ("lc_numeric", "C"),
        ("lc_time", "C"),
        // The values that PostgreSQL works out at startup or that initdb writes.
        ("max_stack_depth", "2MB"),
        ("timezone_abbreviations", "Default"),
        ("default_text_search_config", "pg_catalog.english"),
        ("huge_pages_status", "off"),
        // rupg checks each page and has no way to turn it off (spec/08 section 8.3).
        ("data_checksums", "on"),
        ("io_max_concurrency", "64"),
        ("wal_buffers", "4MB"),
        ("commit_timestamp_buffers", "256kB"),
        ("subtransaction_buffers", "256kB"),
        ("transaction_buffers", "256kB"),
    ];
    for (name, value) in internal {
        settings.set_internal(name, value)?;
    }
    for (name, value) in args {
        settings.set_argument(name, value)?;
    }
    // Each session shares these slots and holds only the ones that it changes.
    settings.share();
    Ok(settings)
}

/// `NUM_SPECIAL_WORKER_PROCS` of `proc.h`: the autovacuum launcher and the slot sync worker.
const SPECIAL_WORKER_PROCS: i64 = 2;
/// `NUM_AUXILIARY_PROCS` of `proc.h`: six auxiliary processes and `MAX_IO_WORKERS`, which is 32.
const AUXILIARY_PROCS: i64 = 6 + 32;

/// `GUC_SAFE_SEARCH_PATH`: the `search_path` that `RestrictSearchPath` sets for a maintenance command.
const SAFE_SEARCH_PATH: &str = "pg_catalog, pg_temp";

/// `InitializeShmemGUCs` for `num_os_semaphores`, after the configuration file: `ProcGlobalSemas`, which is one semaphore for each backend that `InitializeMaxBackends` counts and one for each auxiliary process. rupg uses no semaphores, but it gives the number that PostgreSQL uses. The sizes of the shared memory stay 0, because rupg does not have the shared memory of PostgreSQL.
///
/// # Errors
///
/// A value that the parameter does not take.
pub fn semaphore_count(settings: &mut Settings) -> Result<(), Error> {
    let get = |name: &str| settings.get(name).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let backends =
        ["max_connections", "autovacuum_worker_slots", "max_worker_processes", "max_wal_senders"]
            .into_iter()
            .map(get)
            .sum::<i64>()
            + SPECIAL_WORKER_PROCS;
    settings.set_internal("num_os_semaphores", &(backends + AUXILIARY_PROCS).to_string())
}

/// `pg_split_opts` and the switches of `process_postgres_switches` that set a parameter: the names and values of `-c name=value` and `--name=value` in the `options` of the startup packet. A backslash takes the next character as it is, also a space.
///
/// # Errors
///
/// A word that is not one of these switches, and a switch without a value.
pub fn split_options(options: &str) -> Result<Vec<(String, String)>, Error> {
    let mut words = Vec::new();
    let mut chars = options.chars().peekable();
    loop {
        while chars.peek().is_some_and(char::is_ascii_whitespace) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else {
                word.push(c);
            }
        }
        words.push(word);
    }
    let syntax = |message: String| Error::new(SqlState::SYNTAX_ERROR, message);
    let mut settings = Vec::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let (switch, pair) = if word == "-c" {
            let pair = words.next();
            ("-c", pair.ok_or_else(|| syntax("option requires an argument -- 'c'".into()))?)
        } else if let Some(pair) = word.strip_prefix("--") {
            ("--", pair.to_owned())
        } else if let Some(pair) = word.strip_prefix("-c") {
            ("-c", pair.to_owned())
        } else {
            return Err(syntax(format!(
                "invalid command-line argument for server process: {word}"
            )));
        };
        let Some((name, value)) = pair.split_once('=') else {
            return Err(syntax(if switch == "--" {
                format!("--{pair} requires a value")
            } else {
                format!("-c {pair} requires a value")
            }));
        };
        settings.push((name.replace('-', "_"), value.to_owned()));
    }
    Ok(settings)
}

/// The settings of a new session: the base settings, the values that the server sets for the session, the settings of `ALTER DATABASE ... SET` in `database`, then the `options` of the startup packet and its other parameters, as `process_startup_options` applies them.
///
/// # Errors
///
/// A bad `options` parameter and a setting that is not valid. The server sends them as `FATAL`.
pub fn session_settings(
    base: &Settings,
    start: &Start,
    database: &[String],
) -> Result<Settings, Error> {
    let mut settings = base.clone();
    settings.set_superuser(start.superuser);
    settings.set_internal("is_superuser", if start.superuser { "on" } else { "off" })?;
    settings.set_internal("session_authorization", &start.user)?;
    // `process_settings`: a stored setting that is no longer valid gives a warning in PostgreSQL, and the session starts without it.
    for item in database {
        if let Some((name, value)) = item.split_once('=') {
            drop(settings.set_database(name, value));
        }
    }
    let options = match &start.options {
        Some(options) => split_options(options)?,
        None => Vec::new(),
    };
    for (name, value) in options.iter().chain(&start.settings) {
        settings.set(name, Some(value), Action::Set, Origin::Startup)?;
    }
    Ok(settings)
}

/// Writes an `ErrorResponse` or a `NoticeResponse` with the severity, the SQLSTATE, the message, the detail, the hint, the position in the query, the context and the place in the source.
pub fn write_error(
    out: &mut OutBuf,
    severity: &str,
    error: &Error,
    position: Option<usize>,
    notice: bool,
) {
    let position = position.map(|p| p.to_string());
    let state = error.state();
    let mut fields: Vec<(u8, &[u8])> = vec![
        (b'S', severity.as_bytes()),
        (b'V', severity.as_bytes()),
        (b'C', state.as_str().as_bytes()),
        (b'M', error.message().as_bytes()),
    ];
    if let Some(detail) = error.detail() {
        fields.push((b'D', detail.as_bytes()));
    }
    if let Some(hint) = error.hint() {
        fields.push((b'H', hint.as_bytes()));
    }
    if let Some(position) = &position {
        fields.push((b'P', position.as_bytes()));
    }
    let internal = error.internal_query();
    let internal_position =
        internal.and_then(|(query, at)| at.map(|at| character_position(query, at).to_string()));
    if let Some(position) = &internal_position {
        fields.push((b'p', position.as_bytes()));
    }
    if let Some((query, _)) = internal {
        fields.push((b'q', query.as_bytes()));
    }
    if let Some(context) = error.context() {
        fields.push((b'W', context.as_bytes()));
    }
    for object in Object::ALL {
        if let Some(name) = error.object(object) {
            fields.push((object.code(), name.as_bytes()));
        }
    }
    let (file, line, routine) = source_fields(error.place());
    fields.extend([(b'F', file.as_bytes()), (b'L', line.as_bytes()), (b'R', routine.as_bytes())]);
    if notice {
        out.notice_response(&fields);
    } else {
        out.error_response(&fields);
    }
}

/// What the server does after a [`Connection::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Call `step` again at once.
    Continue,
    /// Send the output, then call `step` again.
    Flush,
    /// Read more input, then call `step` again. The server sends the output first if the client could wait for it.
    Read,
    /// Send the output and close the connection.
    Close,
}

/// The result of one [`Connection::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The number of input bytes that the connection used. The server drops them.
    pub used: usize,
    pub next: Next,
}

/// The state of one session in the main loop.
#[derive(Debug)]
pub struct Connection {
    settings: RefCell<Settings>,
    transaction: Transaction,
    session: Session,
    user: String,
    database: String,
    /// The OID of the database of the session.
    database_oid: u32,
    protocol: u32,
    statements: Statements<Arc<Prepared>>,
    portals: RefCell<Portals<Portal>>,
    /// `unnamed_portal_count` of `portalmem.c`: the number of the last portal that a function in PL/pgSQL opened with no name.
    unnamed_portals: Cell<u32>,
    /// The OID of the materialized view that `CREATE MATERIALIZED VIEW` or `REFRESH MATERIALIZED VIEW` fills, or 0. [`rupg_analyze::Env::filling`] gives it.
    pub(crate) filling: Cell<u32>,
    /// The OID of the table that `CREATE TABLE AS`, `SELECT INTO` or a materialized view fills with a `BulkInsertState`, or 0.
    bulk: Cell<u32>,
    /// `xact_started` of `postgres.c`: a statement runs in the transaction, so `finish_xact_command` must end it.
    xact_started: bool,
    /// `XACT_FLAGS_PIPELINING`: an `Execute` completed in the transaction, so the next message starts an implicit block.
    pipelining: bool,
    /// `QueryCancelPending`. The server sets it when a `CancelRequest` with the key of this session arrives.
    cancel: Arc<AtomicBool>,
    /// `ProcDiePending`. `DROP DATABASE` with `FORCE` sets it in another session, and the session ends at its next message.
    end: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
    /// The process ID of the cancel key, which `pg_backend_pid()` gives.
    pid: i32,
    /// `xactStartTimestamp` and `stmtStartTimestamp`, in microseconds since 2000-01-01 UTC.
    transaction_start: i64,
    statement_start: i64,
    /// `GetUserId`: the OID of the current user.
    role: u32,
    /// `GetSessionUserId`: the OID of the session user.
    session_role: u32,
    /// The names of the current user and of the session user, which `current_user` and `session_user` give.
    current_user: String,
    session_user: String,
    /// The catalog that the sessions of the server share.
    store: Arc<Store>,
    /// The file system of the server, which `COPY ... FROM` a file reads. A session with no server has none.
    io: Option<Arc<dyn Io>>,
    /// The lock of the store and the copy of the catalog, while the transaction changes the catalog.
    lease: RefCell<Option<Lease>>,
    /// The row of the session in the table of the sessions of the store.
    activity: Activity,
    /// The cache of the sequences of the session, with the state of `currval` and `lastval`.
    sequences: RefCell<Sequences>,
    /// The state of `random` and `uuidv7`, which lasts as long as the session.
    randomness: RefCell<Randomness>,
    /// The `COPY ... FROM STDIN` that reads its data.
    copy_in: Option<copy_from::CopyIn>,
    /// The transaction ID of the transaction, or 0 while the transaction did not write a row.
    xid: Cell<u32>,
    /// `GetStableLatestTransactionId`: the reference ID of `age` for the transaction, or 0 before its first call in the transaction.
    stable_xid: Cell<u32>,
    /// `currentCommandId`: the commands of the transaction that wrote rows.
    command: Cell<u32>,
    /// The snapshot of a transaction in `REPEATABLE READ` or `SERIALIZABLE`, from its first statement to its end.
    snapshot: RefCell<Option<Snapshot>>,
    /// The rows of the deferred triggers of the foreign keys, in the order of the writes.
    deferred: RefCell<Vec<Batch>>,
    /// The `LISTEN`, `UNLISTEN` and `NOTIFY` of the transaction.
    notify: RefCell<crate::notify::Pending>,
    /// The events of the deferred constraint triggers, in the order of the writes.
    deferred_events: RefCell<Vec<trigger::Event>>,
    /// The new versions whose keys a deferred unique constraint checks at the commit, in the order of the writes.
    deferred_unique: RefCell<Vec<Recheck>>,
    /// The modes that `SET CONSTRAINTS` gave in the transaction.
    modes: RefCell<deferred::Modes>,
    /// The state of the transaction at each savepoint, the outermost first, as the names of the savepoints in [`Transaction`].
    marks: Vec<savepoint::Mark>,
    /// The functions in PL/pgSQL that the session compiled, and the calls that run.
    plpgsql: rupg_plpgsql::Cache,
    /// The state of the calls in PL/pgSQL that run, for `COMMIT` and `ROLLBACK` in a call.
    calls: plpgsql::Calls,
    /// The queue of the triggers `AFTER` of the statement.
    events: RefCell<Vec<trigger::Event>>,
    /// The messages of `RAISE` of the triggers of the statement, for the client.
    trigger_warnings: RefCell<Vec<Error>>,
    /// The transition tables of the trigger whose function runs, which the readers of its queries get.
    transition: RefCell<Option<Transition>>,
    /// `MyTriggerDepth`: the number of the trigger functions that run, one in the other.
    trigger_depth: Cell<i32>,
}

/// The changes of the rows of an `UPDATE` or a `DELETE`: the ID of the version at the end of each row, and the new row of an `UPDATE`.
fn row_changes(
    rows: Vec<Vec<Value>>,
    info: &Write,
) -> Result<(Vec<Change>, Vec<Vec<Value>>), Error> {
    let update = info.kind == WriteKind::Update;
    let returning = info.keeps_written();
    let mut changes = Vec::with_capacity(rows.len());
    let mut kept = Vec::with_capacity(if returning { rows.len() } else { 0 });
    for mut row in rows {
        let Some(Value::Int8(tid)) = row.pop() else {
            return Err(Error::internal("a row to change with no ID of its version"));
        };
        if returning {
            kept.push(if update { row.clone() } else { std::mem::take(&mut row) });
        }
        row.truncate(info.width);
        changes.push((tid.cast_unsigned(), update.then_some(row)));
    }
    Ok((changes, kept))
}

/// The OID of the bootstrap superuser, which owns the objects of initdb.
const BOOTSTRAP_SUPERUSER: u32 = 10;

impl Connection {
    /// A session with the settings that [`session_settings`] gave. `protocol` is the version of the protocol that the client uses.
    pub fn new(start: &Start, settings: Settings, protocol: u32) -> Connection {
        let mut connection = Connection {
            settings: RefCell::new(settings),
            transaction: Transaction::default(),
            session: Session::new(),
            user: start.user.clone(),
            current_user: start.user.clone(),
            session_user: start.user.clone(),
            database: start.database.clone(),
            database_oid: database::POSTGRES,
            protocol,
            statements: Statements::new(),
            portals: RefCell::new(Portals::new()),
            unnamed_portals: Cell::new(0),
            filling: Cell::new(0),
            bulk: Cell::new(0),
            xact_started: false,
            pipelining: false,
            cancel: Arc::new(AtomicBool::new(false)),
            end: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(OsClock::new()),
            pid: 0,
            transaction_start: 0,
            statement_start: 0,
            role: role_oid(start),
            session_role: role_oid(start),
            store: Arc::new(Store::new()),
            io: None,
            lease: RefCell::new(None),
            activity: Activity::default(),
            sequences: RefCell::new(Sequences::default()),
            randomness: RefCell::new(Randomness::default()),
            copy_in: None,
            xid: Cell::new(0),
            stable_xid: Cell::new(0),
            command: Cell::new(0),
            snapshot: RefCell::new(None),
            deferred: RefCell::new(Vec::new()),
            notify: RefCell::default(),
            deferred_events: RefCell::new(Vec::new()),
            deferred_unique: RefCell::new(Vec::new()),
            modes: RefCell::new(deferred::Modes::default()),
            marks: Vec::new(),
            plpgsql: rupg_plpgsql::Cache::default(),
            calls: plpgsql::Calls::default(),
            events: RefCell::new(Vec::new()),
            trigger_warnings: RefCell::new(Vec::new()),
            transition: RefCell::new(None),
            trigger_depth: Cell::new(0),
        };
        connection.session.set_utf8(connection.utf8());
        connection
    }

    /// Sets the clock of `now()` and the other times of the session. The clock of the operating system is the default.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Sets the file system of the server, which `COPY ... FROM` a file reads.
    pub fn set_io(&mut self, io: Arc<dyn Io>) {
        self.io = Some(io);
    }

    /// Sets the catalog and the table of the sessions that the sessions of the server share, before [`Connection::greet`]. A new session has a store of its own.
    pub fn set_store(&mut self, store: Arc<Store>) {
        self.database_oid =
            store.database(&self.database).map_or(database::POSTGRES, |row| row.oid);
        self.store = store;
    }

    /// `SetCurrentStatementStartTimestamp`: the start of a message that runs a statement.
    fn start_statement(&mut self) {
        self.statement_start = query::now(&*self.clock);
    }

    /// `SetSessionAuthorization` and `SetCurrentRoleId`: the session user from `session_authorization`, the current user from `role`, or the session user when `role` is `none`, and `is_superuser` from the current user. A rollback and `RESET` change the settings too, so the session reads them again before each statement and before `ReadyForQuery`.
    fn sync_role(&mut self) {
        let roles = self.roles();
        let settings = self.settings.get_mut();
        let session = settings.get("session_authorization").unwrap_or_else(|| self.user.clone());
        let current =
            settings.get("role").filter(|role| role != "none").unwrap_or_else(|| session.clone());
        let role = utility::role_oid(&roles, &current, &self.user).unwrap_or(BOOTSTRAP_SUPERUSER);
        let superuser = if roles.superuser(role) { "on" } else { "off" };
        if settings.get("is_superuser").as_deref() != Some(superuser) {
            drop(settings.set_internal("is_superuser", superuser));
        }
        self.session_role =
            utility::role_oid(&roles, &session, &self.user).unwrap_or(BOOTSTRAP_SUPERUSER);
        self.role = role;
        self.current_user = current;
        self.session_user = session;
    }

    /// `GetUserId`: the OID of the current user, which is the owner of the function in a call of a `SECURITY DEFINER` function.
    fn current_role(&self) -> u32 {
        self.settings.borrow().definer().map_or(self.role, |(role, _)| *role)
    }

    /// What a statement reads of the session.
    fn reader(&self) -> Reader<'_> {
        self.reader_on(self.catalog())
    }

    /// What a statement reads of the session, with the catalog `catalog`.
    fn reader_on(&self, catalog: Arc<Catalog>) -> Reader<'_> {
        self.reader_without_view(catalog).with_view(self.view())
    }

    /// A reader that does not see the rows of the tables, for the expressions of an index: they are `IMMUTABLE` and run while the heap is locked, so they must not take a snapshot.
    fn reader_without_view(&self, catalog: Arc<Catalog>) -> Reader<'_> {
        rupg_pgcatalog::roles::use_roles(self.roles());
        Reader::new(
            &self.settings,
            &self.current_user,
            &self.database,
            (self.transaction_start, self.statement_start),
            &*self.clock,
            self.pid,
            catalog,
        )
        .with_users(&self.session_user, &self.user)
        .with_activity(&self.activity, &self.store)
        .with_sequences(&self.sequences)
        .with_randomness(&self.randomness)
        .with_statements(&self.statements)
        .with_portals(&self.portals)
        .with_cancel(&self.cancel)
        .with_connection(self)
        .with_transition(self.transition.borrow().clone())
    }

    /// `GetTransactionSnapshot`: a new snapshot for each statement in `READ COMMITTED`, and the snapshot of the first statement for the whole transaction in the other levels.
    fn view(&self) -> View {
        let heap = self.store.heap();
        let isolation = self.settings.borrow().get("transaction_isolation");
        let serializable = matches!(isolation.as_deref(), Some("repeatable read" | "serializable"));
        let snapshot = if serializable {
            self.snapshot.borrow_mut().get_or_insert_with(|| heap.snapshot()).clone()
        } else {
            heap.snapshot()
        };
        heap.hold(self.pid, &snapshot);
        View {
            database: self.database_oid,
            snapshot,
            xid: self.xid.get(),
            command: self.command.get(),
            serializable,
        }
    }

    /// Writes the rows of an `INSERT`, an `UPDATE` or a `DELETE` in the heap and gives the tag of the command with the number of rows, or the rows of `RETURNING`. Then the statements of `WITH` that no part reads run. The triggers of the foreign keys fire after all the writes, as `AfterTriggerEndQuery` fires them at the end of the statement.
    fn write_rows(
        &self,
        plan: &Plan,
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
        formats: &[i16],
        writes: Writes,
    ) -> Result<Done, Error> {
        let Some(info) = plan.write() else {
            return Err(Error::internal("a plan that does not write"));
        };
        let Applied { tag, count, written, view, batches } =
            self.apply_write(plan, params, view, rows)?;
        let done = if info.returning.is_some() {
            // A subquery of `RETURNING` reads with the snapshot of the statement, which does not see the rows of the statement.
            let reader = self.reader().with_view(view.clone());
            let rows = plan.returning(written, params, &reader)?;
            let columns = query::columns(plan);
            let types: Vec<u32> = plan.columns().iter().map(|target| target.expr.ty).collect();
            let rows = rows
                .iter()
                .map(|values| query::encode(&types, values, &reader, formats))
                .collect::<Result<Vec<_>, _>>()?;
            Done::Rows { columns, rows, tag }
        } else {
            Done::Count(tag, count)
        };
        self.end_writes(writes, params, batches)?;
        Ok(done)
    }

    /// Writes the rows of a statement in the heap. The first write of a transaction gives it a transaction ID, and each write ends a command, as `CommandCounterIncrement` does. When another transaction that is in progress changed a row, the statement waits for its end and then tries again. In `READ COMMITTED`, when a transaction that committed after the snapshot changed a row, the plan runs again with a new snapshot.
    fn apply_write(
        &self,
        plan: &Plan,
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Applied, Error> {
        let Some(info) = plan.write() else {
            return Err(Error::internal("a plan that does not write"));
        };
        let mut triggers = self.write_triggers(info)?;
        if info.instead && info.kind == WriteKind::Merge {
            let Some(merge) = info.merge.as_ref() else {
                return Err(Error::internal("a MERGE with no clauses"));
            };
            let fire = self.merge_triggers(info, merge)?;
            self.before_merge(&fire, info)?;
            let done = self.merge_instead(plan, info, (merge, &fire), params, view, rows)?;
            let (count, written) = (done.count, done.written);
            return Ok(Applied {
                tag: CommandTag::Merge,
                count,
                written,
                view: view.clone(),
                batches: Vec::new(),
            });
        }
        if info.instead {
            // The triggers `INSTEAD OF` of a view write the rows, and the triggers `FOR EACH STATEMENT` of the view fire before and after them.
            self.before_statement(&triggers, info)?;
            let (count, written) = self.instead_rows(info, &triggers, rows)?;
            // `ExecInsert` and `ExecUpdate` check the rows that the triggers give against the views above the view.
            if info.view_check.is_some() {
                plan.view_check(&written, params, &self.reader().with_view(view.clone()))?;
            }
            self.after_write(info, &triggers, &[], &[])?;
            let tag = match info.kind {
                WriteKind::Insert => CommandTag::Insert,
                WriteKind::Update => CommandTag::Update,
                _ => CommandTag::Delete,
            };
            return Ok(Applied { tag, count, written, view: view.clone(), batches: Vec::new() });
        }
        let heap = self.store.heap();
        if self.xid.get() == 0 {
            self.xid.set(heap.begin(self.pid));
        }
        let mut view = View { xid: self.xid.get(), ..view.clone() };
        // The tables of the tree of `INHERITS` change their rows as the leaves of a partitioned table.
        let partitioned = info.inherit
            || self
                .catalog()
                .relation(info.table)
                .is_some_and(|r| r.kind == rupg_catalog::RelKind::Partitioned);
        if partitioned {
            triggers.retain_statement();
        }
        // The leaves of a partitioned table can have unique keys that the table does not have.
        let upsert = info
            .on_conflict
            .as_ref()
            .filter(|conflict| partitioned || !conflict.arbiters.is_empty());
        let mut fire = upsert::Upsert::default();
        let rows = if upsert.is_some() {
            // `INSERT ... ON CONFLICT` fires the triggers `BEFORE ... FOR EACH ROW` for each row in turn, and the triggers `UPDATE` of `DO UPDATE` with the triggers `INSERT`.
            let mut update = self.conflict_triggers(info)?;
            if partitioned {
                update.retain_statement();
            }
            fire = upsert::Upsert { insert: std::mem::take(&mut triggers), update };
            self.before_statement(&fire.insert, info)?;
            self.before_statement(&fire.update, info)?;
            rows
        } else if triggers.is_empty() {
            rows
        } else if partitioned {
            // The triggers `FOR EACH ROW` of the leaves fire when the rows go to the leaves.
            self.before_statement(&triggers, info)?;
            rows
        } else {
            self.before_statement(&triggers, info)?;
            self.before_rows(plan, info, &triggers, rows)?
        };
        // The rows that `RETURNING` reads: the rows that the statement wrote.
        let mut written = Vec::new();
        // The new and the old rows that the foreign keys check and that the triggers `AFTER ... FOR EACH ROW` get.
        let foreign = !info.foreign.is_empty();
        let keep_rows = (foreign && !partitioned) || triggers.after_rows();
        let (mut new_rows, mut old_rows) = (Vec::new(), Vec::new());
        // The new and the old rows of `ON CONFLICT DO UPDATE`.
        let mut updated = (Vec::new(), Vec::new());
        let mut batches = Vec::new();
        let (tag, count) = match (info.kind, upsert) {
            (WriteKind::Merge, _) => {
                let Some(merge) = info.merge.as_ref() else {
                    return Err(Error::internal("a MERGE with no clauses"));
                };
                // `fireBSTriggers`: the triggers of the actions of the clauses, which fire also when no row takes the clause.
                let mut fire = self.merge_triggers(info, merge)?;
                if partitioned {
                    // The leaves fire the triggers `FOR EACH ROW`.
                    fire.retain_statement();
                }
                self.before_merge(&fire, info)?;
                let done = self.merge(plan, info, (merge, &fire), params, &view, rows)?;
                written = done.written;
                batches = done.batches;
                view = done.view;
                (CommandTag::Merge, done.count)
            }
            (WriteKind::Insert, Some(conflict)) => {
                let done = if partitioned {
                    self.upsert_routed(plan, (info, conflict, &fire), params, &view, rows)?
                } else {
                    self.upsert(plan, (info, conflict), params, &view, rows, &fire)?
                };
                if !fire.insert.is_empty() || !fire.update.is_empty() {
                    self.after_upsert(info, &fire, &done)?;
                }
                written = done.written;
                new_rows = done.inserted;
                updated = (done.new, done.old);
                (CommandTag::Insert, done.count)
            }
            (kind, None) if partitioned => match kind {
                WriteKind::Insert => {
                    let routed = self.insert_routed(info, &view, rows, keep_rows)?;
                    written = routed.written;
                    batches = routed.batches;
                    new_rows = routed.inserted;
                    (CommandTag::Insert, routed.count)
                }
                _ => {
                    let changed =
                        self.change_routed((plan, info), params, &view, rows, keep_rows)?;
                    written = changed.written;
                    view = changed.view;
                    batches = changed.batches;
                    (old_rows, new_rows) = (changed.old, changed.new);
                    let tag = if kind == WriteKind::Update {
                        CommandTag::Update
                    } else {
                        CommandTag::Delete
                    };
                    (tag, changed.count)
                }
            },
            (WriteKind::Insert, None) => {
                let count = rows.len() as u64;
                if info.keeps_written() {
                    written.clone_from(&rows);
                }
                if keep_rows {
                    new_rows.clone_from(&rows);
                }
                let inserted = rows.len();
                if self.bulk.get() == info.table {
                    heap.start_bulk(info.table, &view);
                }
                if info.unique.is_empty() {
                    heap.insert(
                        info.table,
                        &view,
                        rows,
                        (&[], &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                        &[],
                    )?;
                } else {
                    let mut kept = Vec::new();
                    loop {
                        match heap.insert(
                            info.table,
                            &view,
                            rows.clone(),
                            (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                            &kept,
                        )? {
                            Changed::Done(_) => break,
                            conflict => kept = self.key_conflict(info, &view, conflict)?,
                        }
                    }
                }
                self.count_inserts(info.table, inserted);
                (CommandTag::Insert, count)
            }
            (kind, _) => {
                let update = kind == WriteKind::Update;
                let (mut changes, mut given) = row_changes(rows, info)?;
                let mut kept = Vec::new();
                let mut applied = Vec::new();
                let count = loop {
                    let changed = heap.change(
                        info.table,
                        &view,
                        changes.clone(),
                        (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                        &kept,
                        &mut applied,
                    )?;
                    match changed {
                        Changed::Done(count) => break count,
                        Changed::Wait { holder, tid } => {
                            let what = if update { "updating" } else { "deleting" };
                            let context = format!(
                                "while {what} tuple {} in relation \"{}\"",
                                tid_text(tid),
                                info.name
                            );
                            self.reader().wait_xact(view.xid, holder, &context)?;
                        }
                        Changed::Again => {
                            let reader = self.reader();
                            let rows = plan.run(params, &reader)?;
                            view = reader.view().cloned().unwrap_or_default();
                            drop(reader);
                            let rows = self.before_rows(plan, info, &triggers, rows)?;
                            (changes, given) = row_changes(rows, info)?;
                        }
                        conflict => kept = self.key_conflict(info, &view, conflict)?,
                    }
                };
                self.count_changes(info.table, &changes, &applied);
                if keep_rows {
                    let tids: Vec<u64> =
                        applied.iter().filter_map(|&at| changes.get(at).map(|c| c.0)).collect();
                    old_rows = heap.versions(view.database, info.table, &tids);
                    new_rows = applied
                        .iter()
                        .filter_map(|&at| changes.get(at).and_then(|c| c.1.clone()))
                        .collect();
                }
                if info.keeps_written() {
                    let mut given: Vec<Option<Vec<Value>>> = given.into_iter().map(Some).collect();
                    written = applied
                        .iter()
                        .filter_map(|&at| given.get_mut(at).and_then(Option::take))
                        .collect();
                }
                (if update { CommandTag::Update } else { CommandTag::Delete }, count)
            }
        };
        // `ExecWithCheckOptions` checks the rows after the write, before the triggers `AFTER` fire. Its subqueries read with the snapshot of the statement.
        // A `MERGE` checks the rows of its inserts and its updates.
        if info.view_check.is_some() && info.kind != WriteKind::Merge {
            plan.view_check(&written, params, &self.reader().with_view(view.clone()))?;
        }
        // The statements of the triggers `BEFORE` can end commands after the command of the write.
        self.command.set(self.command.get().max(view.command + 1));
        if !triggers.is_empty() {
            self.after_write(info, &triggers, &old_rows, &new_rows)?;
        }
        // A partitioned table has its batches from the leaves.
        if foreign && !partitioned && (!new_rows.is_empty() || !old_rows.is_empty()) {
            batches.push(Batch::new(info, &info.foreign, view.command, new_rows, old_rows));
        }
        if let Some(conflict) = upsert
            && !conflict.foreign.is_empty()
            && !updated.0.is_empty()
        {
            let (new, old) = updated;
            batches.push(Batch::new(info, &conflict.foreign, view.command, new, old));
        }
        Ok(Applied { tag, count, written, view, batches })
    }

    /// A conflict of a new version in a unique index. The write waits for the transaction that holds the key, as `_bt_check_unique` does, and gives the IDs of the new versions for the next try. Or it fails with the duplicate key and its value, as `BuildIndexValueDescription` gives it.
    fn key_conflict(
        &self,
        info: &Write,
        view: &View,
        conflict: Changed,
    ) -> Result<Vec<u64>, Error> {
        match conflict {
            Changed::WaitKey { holder, index, used } => {
                let name = info.unique.get(index).map_or("", |key| key.name.as_str());
                let tid = tid_text(used.last().copied().unwrap_or_default());
                let context = format!("while inserting index tuple {tid} in relation \"{name}\"");
                self.reader().wait_xact(view.xid, holder, &context)?;
                Ok(used)
            }
            Changed::Duplicate { index, key } => {
                let Some(unique) = info.unique.get(index) else {
                    return Err(Error::internal(
                        "a duplicate key of an index that is not in the list",
                    ));
                };
                Err(self.duplicate_key(info.table, unique, &key))
            }
            _ => Err(Error::internal("a write result that is not a conflict of a key")),
        }
    }

    /// The error of the key `key` of a new version of the table `table` that a live version of the unique index `unique` has, with the value of the key as `BuildIndexValueDescription` gives it.
    fn duplicate_key(&self, table: u32, unique: &rupg_analyze::Unique, key: &[Value]) -> Error {
        let shown = self
            .index_key_text(&unique.types, key)
            .and_then(|values| Ok((self.key_shown(unique, self.catalog())?, values)));
        let (columns, values) = match shown {
            Ok(shown) => shown,
            Err(error) => return error,
        };
        let mut error = Error::new(
            SqlState::UNIQUE_VIOLATION,
            format!("duplicate key value violates unique constraint \"{}\"", unique.name),
        );
        if !rupg_func::row_security_hides(table, &self.reader()) {
            error = error.with_detail(format!("Key ({columns})=({values}) already exists."));
        }
        rupg_func::with_table_constraint(error, table, &unique.name, &self.reader())
    }

    /// The key columns of a unique index for the detail of an error, as `pg_get_indexdef_columns` gives them in the catalog `catalog`.
    fn key_shown(
        &self,
        unique: &rupg_analyze::Unique,
        catalog: Arc<Catalog>,
    ) -> Result<String, Error> {
        match unique.derived {
            None => Ok(unique.shown.clone()),
            Some(_) => rupg_func::index_key_columns(unique.oid, &self.reader_on(catalog)),
        }
    }

    /// The key of a row in a unique index with an expression or a predicate, [`rupg_exec::index_key`].
    fn index_key(&self, derived: &Derived, row: &[Value]) -> Result<Option<Vec<Value>>, Error> {
        rupg_exec::index_key(derived, row, &self.reader_without_view(self.catalog()))
    }

    /// The values of a key for the detail of an error, `BuildIndexValueDescription`: the output of each value, or `null`.
    fn index_key_text(&self, types: &[u32], key: &[Value]) -> Result<String, Error> {
        let reader = self.reader();
        let mut values = String::new();
        for (value, &ty) in key.iter().zip(types) {
            if !values.is_empty() {
                values.push_str(", ");
            }
            if matches!(value, Value::Null) {
                values.push_str("null");
            } else {
                let mut out = Vec::new();
                rupg_func::output(ty, value, &reader, &mut out)?;
                values.push_str(&String::from_utf8_lossy(&out));
            }
        }
        Ok(values)
    }

    /// The end of the rows of a transaction: a commit makes them visible and removes the rows of the tables that the transaction dropped, and a rollback removes them.
    fn end_rows(&mut self, commit: bool, defined: bool) {
        self.end_heap(commit, defined);
        self.marks.clear();
    }

    /// The transaction ID of the transaction, or `None` when it has none. With `assign`, a transaction with no ID gets one, as `AssignTransactionId` gives it, and it is in progress until the transaction ends.
    pub(crate) fn transaction_id(&self, assign: bool) -> Option<u32> {
        if assign && self.xid.get() == 0 {
            self.xid.set(self.store.heap().begin(self.pid));
        }
        Some(self.xid.get()).filter(|&xid| xid != 0)
    }

    /// `GetStableLatestTransactionId`: the ID of the transaction, or the ID that the next transaction gets when it has none. The first call of a transaction keeps the value until the end of the transaction.
    pub(crate) fn stable_xid(&self) -> u32 {
        if self.stable_xid.get() == 0 {
            let xid = self.transaction_id(false).unwrap_or_else(|| self.heap().next_xid());
            self.stable_xid.set(xid);
        }
        self.stable_xid.get()
    }

    /// The rows of the tables and the transactions in progress.
    pub(crate) fn heap(&self) -> &Heap {
        self.store.heap()
    }

    /// The part of [`Connection::end_rows`] that a call in PL/pgSQL can do, as the transaction has no savepoints there.
    fn end_heap(&self, commit: bool, defined: bool) {
        let heap = self.store.heap();
        let xid = self.xid.replace(0);
        self.stable_xid.set(0);
        self.end_notify(commit);
        if xid != 0 {
            if commit {
                heap.commit(xid);
            } else {
                heap.abort(xid);
            }
        }
        let writes = self.activity.take_writes();
        if !writes.is_empty() {
            self.store.report_writes(self.database_oid, &writes, commit);
        }
        if commit && defined {
            let catalog = self.store.committed(self.database_oid);
            heap.drop_tables(self.database_oid, |relation| catalog.relation(relation).is_some());
        }
        self.command.set(0);
        self.snapshot.replace(None);
        heap.release(self.pid);
        self.end_deferred();
        self.events.borrow_mut().clear();
    }

    /// The check hook of `default_text_search_config` for a `SET` of it, which reads the catalog.
    fn ts_config(&self, node: &Node) -> Result<Option<String>, Error> {
        if !matches!(node, Node::VariableSetStmt(_)) {
            return Ok(None);
        }
        utility::ts_config(node, &self.reader())
    }

    /// The catalog that a statement sees: the copy of the transaction when it changed the catalog, or the catalog of the last commit.
    fn catalog(&self) -> Arc<Catalog> {
        match &*self.lease.borrow() {
            Some(lease) => Arc::clone(&lease.catalog),
            None => self.store.committed(self.database_oid),
        }
    }

    /// Runs the elements of `CREATE SCHEMA` after the statement made the schema, as `CreateSchemaCommand` runs them: as the owner of the schema and with the schema first in `search_path`. The settings of the session come back after the elements, also after an error.
    fn schema_elements(
        &self,
        schema: &SchemaElements,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let saved = self.settings.borrow().clone();
        let old = saved.get("search_path").unwrap_or_default();
        let mut path = rupg_func::quote_identifier(&schema.schema);
        if !old.trim_start().is_empty() {
            path = format!("{path}, {}", old.trim_start());
        }
        let set = self.settings.borrow_mut().set(
            "search_path",
            Some(&path),
            Action::Set,
            Origin::Statement,
        );
        let result = set.and_then(|()| {
            schema.elements.iter().try_for_each(|(node, text)| {
                self.define_with(node, text, notices, schema.owner, true).map(drop)
            })
        });
        *self.settings.borrow_mut() = saved;
        result
    }

    /// The roles that a statement sees: the copy of the transaction when it holds the lock of the catalog, or the roles of the last commit.
    fn roles(&self) -> Arc<Roles> {
        match &*self.lease.borrow() {
            Some(lease) => Arc::clone(&lease.roles),
            None => self.store.roles(),
        }
    }

    /// Runs a statement that defines an object. The statement takes the lock of the store, which the transaction holds until it ends. After an error the changes of the statement go, but its OIDs stay used.
    fn define(&self, node: &Node, text: &str, notices: &mut Vec<Notice>) -> Result<Done, Error> {
        self.define_with(node, text, notices, self.current_role(), false)
    }

    /// [`Connection::define`] as the role `role`, which owns the new objects. `call` is true for a statement that is not at the top level, a statement of a function or a procedure or an element of `CREATE SCHEMA`, for `PreventInTransactionBlock` of the statements with `CONCURRENTLY` and the check of [`rupg_analyze::Defined::top_level`]. After a failure of the build of `CREATE INDEX CONCURRENTLY` the index stays, as [`rupg_analyze::Defined::concurrent`] tells.
    fn define_with(
        &self,
        node: &Node,
        text: &str,
        notices: &mut Vec<Notice>,
        role: u32,
        call: bool,
    ) -> Result<Done, Error> {
        // `PreventInTransactionBlock` of the statements with `CONCURRENTLY`.
        let concurrent = match node {
            Node::IndexStmt(stmt) if stmt.concurrent => Some("CREATE INDEX CONCURRENTLY"),
            Node::DropStmt(stmt) if stmt.concurrent => Some("DROP INDEX CONCURRENTLY"),
            Node::AlterTableStmt(stmt)
                if stmt.cmds.iter().flatten().any(|cmd| {
                    matches!(cmd, Node::AlterTableCmd(cmd) if cmd.subtype == AlterTableType::AT_DetachPartition
                        && matches!(&cmd.def, Some(Node::PartitionCmd(part)) if part.concurrent))
                }) =>
            {
                Some("ALTER TABLE ... DETACH CONCURRENTLY")
            }
            _ => None,
        };
        if let Some(statement) = concurrent {
            if call {
                return Err(self.prevent_in_call(statement));
            }
            self.prevent_block(statement)?;
        }
        if matches!(node, Node::TruncateStmt(_))
            && self.settings.borrow().get("transaction_read_only").as_deref() == Some("on")
        {
            return Err(Error::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                "cannot execute TRUNCATE TABLE in a read-only transaction",
            ));
        }
        if self.lease.borrow().is_none() {
            let lease = self.lease_catalog()?;
            *self.lease.borrow_mut() = Some(lease);
        }
        // A change of the catalog writes rows of the catalog, so the transaction gets an ID.
        self.transaction_id(true);
        let mut work = Catalog::clone(&self.catalog());
        let mut defined = rupg_analyze::define(node, text, &self.reader(), &mut work, role);
        if let Some(top) = &defined.top_level
            && defined.result.is_ok()
        {
            let checked = if call {
                Err(self.prevent_in_call(top.statement))
            } else {
                self.prevent_block(top.statement)
            };
            if let Err(error) = checked {
                defined.messages.clear();
                defined.result = Err(error.with_context(top.context.clone()));
            }
        }
        if self.settings.borrow().get("track_counts").as_deref() != Some("off") {
            for &table in &defined.scans {
                self.activity.count_scan(table);
            }
        }
        notices.extend(defined.messages.into_iter().map(|message| match message {
            Message::Warning(error) => Notice::warning(error),
            Message::Notice(error) => Notice { severity: Severity::Notice, error },
        }));
        let (transforms, fills) = (&defined.transforms, &defined.fills);
        let analyzed = defined.result.is_ok();
        let rewrite = defined
            .result
            .and_then(|()| self.check_detached(&defined.detached))
            .and_then(|()| self.rows_to_rewrite(node, &work, transforms, fills));
        let mut work = Arc::new(work);
        let result = rewrite
            .and_then(|tables| self.rewrite_rows(&tables, &work, transforms, fills))
            .and_then(|()| self.check_partitions(&defined.checks, &work))
            .and_then(|()| self.truncate_rows(&defined.truncated, &work))
            .and_then(|()| self.view_texts(&defined.views, &mut work));
        let mut held = self.lease.borrow_mut();
        let Some(lease) = held.as_mut() else {
            return Err(Error::internal("the lock of the catalog is gone"));
        };
        match result {
            Ok(()) => {
                let old = std::mem::replace(&mut lease.catalog, work);
                self.index_stats(&old, &lease.catalog);
                if !defined.grantors.is_empty() {
                    let mut roles = Roles::clone(&lease.roles);
                    roles.members.retain(|m| !defined.grantors.contains(&m.grantor));
                    lease.roles = Arc::new(roles);
                }
                if let Some(table) = defined.filled {
                    drop(held);
                    return self.fill(node, table, &old, notices);
                }
                if let Some(schema) = defined.schema {
                    drop(held);
                    self.schema_elements(&schema, notices)?;
                    return Ok(Done::Tag(CommandTag::CreateSchema));
                }
                Ok(Done::Tag(match node {
                    Node::CreateTableAsStmt(stmt) if stmt.objtype == ObjectType::OBJECT_MATVIEW => {
                        CommandTag::CreateMaterializedView
                    }
                    Node::CreateTableAsStmt(_) | Node::SelectStmt(_) => CommandTag::CreateTableAs,
                    Node::RefreshMatViewStmt(_) => CommandTag::RefreshMaterializedView,
                    Node::IndexStmt(_) => CommandTag::CreateIndex,
                    Node::CreateSchemaStmt(_) => CommandTag::CreateSchema,
                    Node::CreateDomainStmt(_) => CommandTag::CreateDomain,
                    Node::CreateFunctionStmt(stmt) if stmt.is_procedure => {
                        CommandTag::CreateProcedure
                    }
                    Node::CreateFunctionStmt(_) => CommandTag::CreateFunction,
                    Node::AlterFunctionStmt(stmt) => alter_tag(stmt.objtype),
                    Node::CompositeTypeStmt(_)
                    | Node::CreateEnumStmt(_)
                    | Node::CreateRangeStmt(_) => CommandTag::CreateType,
                    Node::AlterEnumStmt(_) => CommandTag::AlterType,
                    Node::ViewStmt(_) => CommandTag::CreateView,
                    Node::RuleStmt(_) => CommandTag::CreateRule,
                    Node::CreatePolicyStmt(_) => CommandTag::CreatePolicy,
                    Node::AlterPolicyStmt(_) => CommandTag::AlterPolicy,
                    Node::CreateTrigStmt(_) => CommandTag::CreateTrigger,
                    Node::CommentStmt(_) => CommandTag::Comment,
                    Node::GrantStmt(stmt) if stmt.is_grant => CommandTag::Grant,
                    Node::GrantStmt(_) => CommandTag::Revoke,
                    Node::DropStmt(stmt) => match stmt.removeType {
                        ObjectType::OBJECT_SCHEMA => CommandTag::DropSchema,
                        ObjectType::OBJECT_VIEW => CommandTag::DropView,
                        ObjectType::OBJECT_MATVIEW => CommandTag::DropMaterializedView,
                        ObjectType::OBJECT_INDEX => CommandTag::DropIndex,
                        ObjectType::OBJECT_SEQUENCE => CommandTag::DropSequence,
                        ObjectType::OBJECT_TYPE => CommandTag::DropType,
                        ObjectType::OBJECT_DOMAIN => CommandTag::DropDomain,
                        ObjectType::OBJECT_RULE => CommandTag::DropRule,
                        ObjectType::OBJECT_POLICY => CommandTag::DropPolicy,
                        ObjectType::OBJECT_TRIGGER => CommandTag::DropTrigger,
                        ObjectType::OBJECT_FUNCTION => CommandTag::DropFunction,
                        ObjectType::OBJECT_PROCEDURE => CommandTag::DropProcedure,
                        ObjectType::OBJECT_ROUTINE => CommandTag::DropRoutine,
                        ObjectType::OBJECT_AGGREGATE => CommandTag::DropAggregate,
                        ObjectType::OBJECT_OPERATOR => CommandTag::DropOperator,
                        ObjectType::OBJECT_OPCLASS => CommandTag::DropOperatorClass,
                        ObjectType::OBJECT_OPFAMILY => CommandTag::DropOperatorFamily,
                        ObjectType::OBJECT_TSPARSER => CommandTag::DropTextSearchParser,
                        ObjectType::OBJECT_TSDICTIONARY => CommandTag::DropTextSearchDictionary,
                        ObjectType::OBJECT_TSTEMPLATE => CommandTag::DropTextSearchTemplate,
                        ObjectType::OBJECT_TSCONFIGURATION => {
                            CommandTag::DropTextSearchConfiguration
                        }
                        ObjectType::OBJECT_STATISTIC_EXT => CommandTag::DropStatistics,
                        ObjectType::OBJECT_FOREIGN_TABLE => CommandTag::DropForeignTable,
                        ObjectType::OBJECT_ACCESS_METHOD => CommandTag::DropAccessMethod,
                        ObjectType::OBJECT_CAST => CommandTag::DropCast,
                        ObjectType::OBJECT_COLLATION => CommandTag::DropCollation,
                        ObjectType::OBJECT_CONVERSION => CommandTag::DropConversion,
                        ObjectType::OBJECT_EXTENSION => CommandTag::DropExtension,
                        ObjectType::OBJECT_LANGUAGE => CommandTag::DropLanguage,
                        ObjectType::OBJECT_FDW => CommandTag::DropForeignDataWrapper,
                        ObjectType::OBJECT_FOREIGN_SERVER => CommandTag::DropServer,
                        ObjectType::OBJECT_EVENT_TRIGGER => CommandTag::DropEventTrigger,
                        ObjectType::OBJECT_PUBLICATION => CommandTag::DropPublication,
                        _ => CommandTag::DropTable,
                    },
                    Node::AlterTableStmt(stmt) => alter_tag(stmt.objtype),
                    Node::AlterOwnerStmt(stmt) => match stmt.objectType {
                        ObjectType::OBJECT_SCHEMA => CommandTag::AlterSchema,
                        kind => alter_tag(kind),
                    },
                    Node::RenameStmt(stmt) => match stmt.renameType {
                        ObjectType::OBJECT_COLUMN | ObjectType::OBJECT_ATTRIBUTE => {
                            alter_tag(stmt.relationType)
                        }
                        ObjectType::OBJECT_SCHEMA => CommandTag::AlterSchema,
                        kind => alter_tag(kind),
                    },
                    Node::AlterObjectSchemaStmt(stmt) => alter_tag(stmt.objectType),
                    Node::CreateSeqStmt(_) => CommandTag::CreateSequence,
                    Node::AlterSeqStmt(_) => CommandTag::AlterSequence,
                    Node::DefineStmt(stmt) => match stmt.kind {
                        ObjectType::OBJECT_OPERATOR => CommandTag::CreateOperator,
                        ObjectType::OBJECT_AGGREGATE => CommandTag::CreateAggregate,
                        ObjectType::OBJECT_TSPARSER => CommandTag::CreateTextSearchParser,
                        ObjectType::OBJECT_TSDICTIONARY => CommandTag::CreateTextSearchDictionary,
                        ObjectType::OBJECT_TSTEMPLATE => CommandTag::CreateTextSearchTemplate,
                        ObjectType::OBJECT_TSCONFIGURATION => {
                            CommandTag::CreateTextSearchConfiguration
                        }
                        _ => CommandTag::CreateTable,
                    },
                    Node::AlterTSConfigurationStmt(_) => CommandTag::AlterTextSearchConfiguration,
                    Node::AlterOperatorStmt(_) => CommandTag::AlterOperator,
                    Node::CreateOpClassStmt(_) => CommandTag::CreateOperatorClass,
                    Node::CreateOpFamilyStmt(_) => CommandTag::CreateOperatorFamily,
                    Node::AlterOpFamilyStmt(_) => CommandTag::AlterOperatorFamily,
                    Node::TruncateStmt(_) => CommandTag::TruncateTable,
                    Node::CreateStatsStmt(_) => CommandTag::CreateStatistics,
                    Node::AlterStatsStmt(_) => CommandTag::AlterStatistics,
                    Node::DropOwnedStmt(_) => CommandTag::DropOwned,
                    Node::AlterDefaultPrivilegesStmt(_) => CommandTag::AlterDefaultPrivileges,
                    Node::ReassignOwnedStmt(_) => CommandTag::ReassignOwned,
                    Node::ReindexStmt(_) => CommandTag::Reindex,
                    _ => CommandTag::CreateTable,
                }))
            }
            Err(error) => {
                if let Some(index) = defined.concurrent.filter(|_| analyzed) {
                    // The first transaction of `DefineIndex` committed the index, so after a failure of the build it stays, invalid and not ready.
                    let catalog = Arc::make_mut(&mut work);
                    catalog.set_index_valid(index, false)?;
                    catalog.set_index_ready(index, false)?;
                    lease.catalog = work;
                    if let Some(lease) = held.take() {
                        lease.commit();
                    }
                    return Err(error);
                }
                Arc::make_mut(&mut lease.catalog).advance_oid(work.next_oid());
                Err(error)
            }
        }
    }

    /// `intorel_receive`: the rows of the query of `CREATE TABLE AS` or `SELECT INTO` go into the new table `table`, as `INSERT ... SELECT` writes them. When the query fails, the catalog goes back to `old`, so the table is gone, and its OIDs stay used. `REFRESH MATERIALIZED VIEW` fills the materialized view with the rows of its query, as `refresh_matview_datafill` writes them, and the `INSERT` reads the materialized view, which gives its query there.
    fn fill(
        &self,
        node: &Node,
        table: u32,
        old: &Arc<Catalog>,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let query = match node {
            Node::CreateTableAsStmt(stmt) => stmt.query.clone(),
            Node::SelectStmt(stmt) => {
                Some(Node::SelectStmt(Box::new(rupg_analyze::without_into(stmt))))
            }
            _ => None,
        };
        let catalog = self.catalog();
        let Some(relation) = catalog.relation(table) else {
            return Err(Error::internal("the table of CREATE TABLE AS is gone"));
        };
        let matview = relation.kind == rupg_catalog::RelKind::MatView;
        let owner = relation.owner;
        // A built-in schema such as `public` has no row in the catalog of the user.
        let schema = builtin::named_by_oid(builtin::Named::Namespace, relation.namespace)
            .map(|row| row.name)
            .or_else(|| catalog.schema(relation.namespace).map(|s| s.name.as_str()));
        let relation = RangeVar {
            schemaname: schema.map(Into::into),
            relname: Some(relation.name.as_str().into()),
            inh: true,
            relpersistence: b'p',
            location: -1,
            ..RangeVar::default()
        };
        // A materialized view reads its own query, which the catalog keeps, so that `search_path` of the fill does not change its names.
        let query = match node {
            _ if matview => {
                let star = ColumnRef { fields: vec![Some(A_Star {}.into())], location: -1 };
                let target =
                    ResTarget { val: Some(star.into()), location: -1, ..ResTarget::default() };
                let select = SelectStmt {
                    targetList: vec![Some(target.into())],
                    fromClause: vec![Some(relation.clone().into())],
                    ..SelectStmt::default()
                };
                Some(select.into())
            }
            _ => query,
        };
        let insert = InsertStmt {
            relation: Some(Box::new(relation)),
            selectStmt: query,
            ..InsertStmt::default()
        };
        let mut late = Vec::new();
        let mut described = None;
        if matview {
            // `RefreshMatViewByOid` fills the materialized view as its owner, with the `search_path` of `RestrictSearchPath`.
            let config = [format!("search_path={SAFE_SEARCH_PATH}")];
            rupg_func::Session::start_function_call(&self.reader(), &config, Some(owner))?;
            self.filling.set(table);
        }
        self.bulk.set(table);
        let result =
            self.select(&Node::InsertStmt(Box::new(insert)), notices, &mut late, &mut described);
        self.bulk.set(0);
        self.filling.set(0);
        if matview {
            rupg_func::Session::end_function_call(&self.reader());
        }
        notices.append(&mut late);
        match result {
            Ok(Done::Count(..)) if matches!(node, Node::RefreshMatViewStmt(_)) => {
                Ok(Done::Tag(CommandTag::RefreshMaterializedView))
            }
            Ok(Done::Count(_, count)) => Ok(Done::Count(CommandTag::Select, count)),
            Ok(_) => Err(Error::internal("the INSERT of CREATE TABLE AS gave no count")),
            Err(error) => {
                if let Some(lease) = self.lease.borrow_mut().as_mut() {
                    let next = lease.catalog.next_oid();
                    lease.catalog = Arc::clone(old);
                    Arc::make_mut(&mut lease.catalog).advance_oid(next);
                }
                Err(error)
            }
        }
    }

    /// The new text of each view of `views` after `ALTER ... RENAME`. The old catalog gives the query of the view, and the deparse writes it with the names of `work`, as `pg_get_viewdef` gives it. The view keeps `search_path` of the statement, which the deparse uses for the names that it does not qualify.
    fn view_texts(&self, views: &[u32], work: &mut Arc<Catalog>) -> Result<(), Error> {
        if views.is_empty() {
            return Ok(());
        }
        // The old catalog reads the stored queries before the reader of the new catalog gives its functions to the thread.
        let old = self.reader();
        let mut queries = Vec::with_capacity(views.len());
        for &oid in views {
            if let Some(query) = rupg_analyze::view_query(&old, oid)? {
                queries.push((oid, query));
            }
        }
        drop(old);
        let reader = self.reader_on(Arc::clone(work));
        // The analyzer looks in `pg_catalog` first when `search_path` does not name it, as `recomputeNamespacePath` does.
        let mut path = rupg_analyze::Env::search_path(&reader);
        if !path.contains(&rupg_analyze::PG_CATALOG_NAMESPACE) {
            path.insert(0, rupg_analyze::PG_CATALOG_NAMESPACE);
        }
        let mut texts = Vec::with_capacity(queries.len());
        for (oid, query) in queries {
            let Some(rel) = work.relation(oid) else { continue };
            let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
            texts.push((oid, rupg_func::view_statement(&query, &rel.name, &columns, &reader)?));
        }
        drop(reader);
        let catalog = Arc::make_mut(work);
        for (oid, text) in texts {
            catalog.set_view_text(oid, text, path.clone())?;
        }
        Ok(())
    }

    /// The rows of the tables of `TRUNCATE`. The first write of a transaction gives it a transaction ID, and the statement ends a command. When another transaction in progress changed a table, the statement waits for its end, as the lock of `TRUNCATE` waits.
    fn truncate_rows(&self, tables: &[u32], work: &Catalog) -> Result<(), Error> {
        if tables.is_empty() {
            return Ok(());
        }
        let replica =
            self.settings.borrow().get("session_replication_role").as_deref() == Some("replica");
        let truncate = |t: &&rupg_catalog::Trigger| {
            !t.internal && t.fires(replica) && t.kind & rupg_catalog::TRIGGER_TRUNCATE != 0
        };
        if work.triggers().filter(truncate).any(|t| tables.contains(&t.relation)) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "TRUNCATE of a table with triggers is not supported yet",
            ));
        }
        let heap = self.store.heap();
        if self.xid.get() == 0 {
            self.xid.set(heap.begin(self.pid));
        }
        let view = View { xid: self.xid.get(), ..self.view() };
        for &table in tables {
            while let Some(holder) = heap.truncate(table, &view) {
                let name = work.relation(table).map_or("", |r| r.name.as_str());
                let context = format!("while truncating relation \"{name}\"");
                self.reader().wait_xact(view.xid, holder, &context)?;
            }
            self.count_truncate(table);
        }
        self.command.set(view.command + 1);
        Ok(())
    }

    /// `LOCK TABLE`: `RequireTransactionBlock`, then the checks of [`rupg_analyze::lock_table`] with the privileges of `LockTableAclCheck`, and the locks of the relations in the mode of the statement. The transaction holds the locks until it ends.
    fn lock(&self, stmt: &LockStmt) -> Result<Done, Error> {
        utility::require_block(&self.transaction, "LOCK TABLE")?;
        let reader = self.reader();
        let mode = u8::try_from(stmt.mode).unwrap_or(8);
        let mut allowed =
            |relid, role| rupg_func::lock_table_allowed(relid, role, stmt.mode, &reader);
        let mut take = |relid| reader.lock_relation(relid, mode, !stmt.nowait);
        rupg_analyze::lock_table(stmt, &reader, self.current_role(), &mut allowed, &mut take)?;
        // `LogAccessExclusiveLockPrepare`: the transaction gets an ID for the log of an `ACCESS EXCLUSIVE` lock.
        if mode == 8 {
            self.transaction_id(true);
        }
        Ok(Done::Tag(CommandTag::LockTable))
    }

    /// The flag that a `CancelRequest` sets. The server keeps a clone of it with the cancel key of the session.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// `CHECK_FOR_INTERRUPTS` for a cancel. It clears the flag and gives `57014` when the flag was set.
    fn interrupted(&self) -> Result<(), Error> {
        if self.cancel.swap(false, Ordering::Relaxed) {
            return Err(Error::new(
                SqlState::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        Ok(())
    }

    /// The settings of the session.
    pub fn settings(&self) -> Ref<'_, Settings> {
        self.settings.borrow()
    }

    /// The messages after `AuthenticationOk`: a `ParameterStatus` for each parameter that the client must know, then `BackendKeyData`. The first `ReadyForQuery` comes from the first [`Connection::step`].
    pub fn greet(&mut self, key: &CancelKey, out: &mut OutBuf) {
        for (name, value) in self.settings.get_mut().startup_reports() {
            out.parameter_status(name.as_bytes(), value.as_bytes());
        }
        out.parameter_status(b"rupg.version", VERSION.as_bytes());
        self.pid = key.pid();
        self.join();
        key.write(out);
    }

    /// True when the client encoding is `UTF8`, so the strings of the messages must be valid UTF-8.
    fn utf8(&self) -> bool {
        self.settings.borrow().get("client_encoding").is_none_or(|name| name != "SQL_ASCII")
    }

    /// Reads at most one message from `input` and writes the answer to `out`.
    pub fn step(&mut self, input: &[u8], out: &mut OutBuf) -> Step {
        // `ProcessInterrupts` with `ProcDiePending`: `DROP DATABASE` with `FORCE` in another session ended this session.
        if self.end.load(Ordering::Relaxed) {
            let error = Error::new(
                SqlState::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            );
            write_error(out, "FATAL", &error, None, false);
            return Step { used: 0, next: Next::Close };
        }
        if self.session.wants_ready() {
            self.ready_for_query(out);
            return Step { used: 0, next: Next::Flush };
        }
        let read = self.session.read(input);
        let used = read.used;
        if read.message.is_some() {
            // A cancel does nothing when no statement runs, so the main loop of `postgres.c` drops a cancel that came while it read the message.
            self.cancel.store(false, Ordering::Relaxed);
        }
        if let Some(Ok(
            Frontend::Query(_)
            | Frontend::Parse { .. }
            | Frontend::Bind(_)
            | Frontend::Describe { .. }
            | Frontend::Execute { .. },
        )) = &read.message
        {
            self.start_statement();
        }
        let next = match read.message {
            None => Next::Read,
            Some(Err(error)) => {
                out.protocol_error(&error, self.protocol);
                if self.copy_in.take().is_some() {
                    self.abort();
                }
                if error.level == Level::Error { self.recover(out) } else { Next::Close }
            }
            Some(Ok(Frontend::Query(sql))) => {
                let text = String::from_utf8_lossy(sql);
                let failed = self.simple_query(&text, out);
                self.after_query(failed, out)
            }
            Some(Ok(
                message @ (Frontend::CopyData(_) | Frontend::CopyDone | Frontend::CopyFail(_)),
            )) if self.copy_in.is_some() => self.copy_message(message, out),
            Some(Ok(Frontend::Parse { name, sql, types })) => {
                let result = self.parse(name, sql, types, out);
                self.after(result, out)
            }
            Some(Ok(Frontend::Bind(bind))) => {
                let result = self.bind(bind, out);
                self.after(result, out)
            }
            Some(Ok(Frontend::Describe { target, name })) => {
                let result = self.describe(target, name, out);
                self.after(result, out)
            }
            Some(Ok(Frontend::Execute { portal, max_rows })) => {
                let result = self.execute(portal, max_rows, out);
                self.after(result, out)
            }
            Some(Ok(Frontend::Close { target, name })) => {
                self.close(target, name, out);
                Next::Continue
            }
            Some(Ok(Frontend::Sync)) => {
                self.sync(out);
                Next::Continue
            }
            Some(Ok(Frontend::Flush)) => Next::Flush,
            Some(Ok(Frontend::Terminate)) => Next::Close,
            Some(Ok(_)) => {
                let error = ProtocolError::new(
                    Level::Error,
                    "0A000",
                    "the function call protocol is not supported yet",
                );
                out.protocol_error(&error, self.protocol);
                self.recover(out)
            }
        };
        Step { used, next }
    }

    /// The end of a `Query` message, or of the part of it after a `COPY ... FROM STDIN`. A copy that waits for its data sends `CopyInResponse` at once.
    fn after_query(&mut self, failed: bool, out: &mut OutBuf) -> Next {
        if failed {
            self.recover(out)
        } else if self.copy_in.is_some() {
            Next::Flush
        } else {
            Next::Continue
        }
    }

    /// `CopyData`, `CopyDone` or `CopyFail` of a `COPY ... FROM STDIN`. After `CopyDone` the session runs the rest of the statements of the `Query` message.
    fn copy_message(&mut self, message: Frontend<'_>, out: &mut OutBuf) -> Next {
        let Some(mut copy) = self.copy_in.take() else { return Next::Continue };
        let result = match message {
            Frontend::CopyData(data) => match self.copy_data(&mut copy, data) {
                Ok(()) => {
                    self.copy_in = Some(copy);
                    return Next::Continue;
                }
                Err(error) => Err(error),
            },
            Frontend::CopyFail(message) => Err(copy.fail(message)),
            _ => self.copy_done(&mut copy),
        };
        copy.notices.extend(self.trigger_notices());
        self.notices(&mut copy.notices, "", &[], out);
        match result {
            Ok(count) => {
                let failed = self.resume_query(&copy.text, copy.index, count, out);
                self.after_query(failed, out)
            }
            Err(error) => {
                self.fail(&error, None, out);
                self.recover(out)
            }
        }
    }

    /// The end of a message of the extended protocol. After an error the session drops the messages until `Sync`.
    fn after(&mut self, result: Result<(), Failed>, out: &mut OutBuf) -> Next {
        match result {
            Ok(()) => Next::Continue,
            Err(failed) => {
                self.fail(&failed.error, failed.position, out);
                self.recover(out)
            }
        }
    }

    /// The parameters that changed, then `ReadyForQuery`.
    fn ready_for_query(&mut self, out: &mut OutBuf) {
        self.report_idle();
        self.sync_role();
        self.session.set_utf8(self.utf8());
        self.send_notifications(out);
        let mut reports = self.settings.get_mut().reports();
        // PostgreSQL changes `is_superuser` inside the change of `session_authorization`, so it reports `session_authorization` first.
        let at = |reports: &[(&str, String)], name| reports.iter().position(|(n, _)| *n == name);
        if let (Some(user), Some(superuser)) =
            (at(&reports, "session_authorization"), at(&reports, "is_superuser"))
            && superuser < user
        {
            let report = reports.remove(user);
            reports.insert(superuser, report);
        }
        for (name, value) in reports {
            out.parameter_status(name.as_bytes(), value.as_bytes());
        }
        self.session.ready_for_query(self.transaction.status(), out);
    }

    /// The end of a message that failed. The error goes to the client at once.
    fn recover(&mut self, out: &mut OutBuf) -> Next {
        match self.session.recover() {
            Some(fatal) => {
                out.protocol_error(&fatal, self.protocol);
                Next::Close
            }
            None => Next::Flush,
        }
    }

    /// Writes the notices that `client_min_messages` lets through.
    fn notices(
        &self,
        notices: &mut Vec<Notice>,
        text: &str,
        locations: &[Option<usize>],
        out: &mut OutBuf,
    ) {
        let least = match self.settings.borrow().get("client_min_messages").as_deref() {
            Some("warning") => 1,
            Some("error") => 2,
            Some("log") => -1,
            Some(level) if level.starts_with("debug") => -2,
            _ => 0,
        };
        for (i, notice) in notices.drain(..).enumerate() {
            let (rank, severity) = match (notice.error.level(), notice.severity) {
                (Some(ErrorLevel::Debug), _) => (-2, "DEBUG"),
                (Some(ErrorLevel::Log), _) => (-1, "LOG"),
                // `INFO` goes to the client at any level of `client_min_messages`.
                (Some(ErrorLevel::Info), _) => (least, "INFO"),
                (Some(ErrorLevel::Notice), _) | (None, Severity::Notice) => (0, "NOTICE"),
                (Some(ErrorLevel::Warning), _) | (None, Severity::Warning) => (1, "WARNING"),
            };
            if rank >= least {
                let at = locations.get(i).copied().flatten().or_else(|| notice.error.position());
                let position = at.map(|at| character_position(text, at));
                write_error(out, severity, &notice.error, position, true);
            }
        }
    }

    /// `exec_simple_query`. It gives true when a statement failed.
    fn simple_query(&mut self, text: &str, out: &mut OutBuf) -> bool {
        self.report(BackendState::Running, Some(text));
        self.start_xact();
        // A Query works as if it used the unnamed statement and the unnamed portal.
        drop(self.statements.close(b""));
        let (list, parser_notices) = match rupg_sql::parse(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                let position = error.position(text);
                self.fail(&error.into(), position, out);
                return true;
            }
        };
        let locations: Vec<Option<usize>> = parser_notices.iter().map(|n| n.location).collect();
        let mut notices: Vec<Notice> = parser_notices
            .into_iter()
            .map(|n| Notice { severity: n.severity, error: Error::new(n.code, n.message) })
            .collect();
        self.notices(&mut notices, text, &locations, out);
        let statements = statements(&list, text);
        if statements.is_empty() {
            out.empty_query_response();
            self.finish_xact();
            return false;
        }
        self.run_statements(text, &statements, 0, None, out)
    }

    /// The rest of a `Query` message after the `COPY ... FROM STDIN` at `index` ended with `count` rows. The text parsed before, so the parse gives no error and its notices went to the client.
    fn resume_query(&mut self, text: &str, index: usize, count: u64, out: &mut OutBuf) -> bool {
        let Ok((list, _)) = rupg_sql::parse(text) else { return false };
        let statements = statements(&list, text);
        let copied = Some(Done::Count(CommandTag::Copy, count));
        self.run_statements(text, &statements, index, copied, out)
    }

    /// The loop of `exec_simple_query` over the statements from `from`. `copied` is the result of a `COPY ... FROM STDIN` at `from` that ended. A `COPY ... FROM STDIN` stops the loop until its data ends. It gives true when a statement failed.
    fn run_statements(
        &mut self,
        text: &str,
        statements: &[(&Node, &str)],
        from: usize,
        mut copied: Option<Done>,
        out: &mut OutBuf,
    ) -> bool {
        let mut notices = Vec::new();
        let many = statements.len() > 1;
        for (i, &(node, statement)) in statements.iter().enumerate().skip(from) {
            if copied.is_none() {
                self.start_xact();
                if many {
                    self.transaction.begin_implicit();
                }
            }
            // The columns of a query that failed at run time, and the rows that it made before the error. PostgreSQL sends `RowDescription` when the executor starts and each row when it makes it, so the error comes after them.
            let mut described = None;
            // The format and the number of columns of a `COPY` that failed at run time.
            let mut copying = None;
            // The warnings of the run of a query. PostgreSQL sends them after `RowDescription`.
            let mut late = Vec::new();
            let result = if let Some(done) = copied.take() {
                Ok(done)
            } else if let Err(error) = self.interrupted() {
                Err(error)
            } else if self.transaction.failed() && !utility::exits_transaction(node) {
                Err(Error::new(
                    SqlState::IN_FAILED_SQL_TRANSACTION,
                    "current transaction is aborted, commands ignored until end of transaction block",
                ))
            } else if let Some(stmt) = utility::is_savepoint(node) {
                drop(self.portals.get_mut().close(b""));
                self.savepoint(stmt)
            } else if prepare::is_prepare(node) {
                drop(self.portals.get_mut().close(b""));
                self.run_prepare(node, text, &[], &mut notices, 0)
            } else if let Node::ExplainStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.explain(stmt, &mut notices)
            } else if query::is_query(node) {
                drop(self.portals.get_mut().close(b""));
                self.select(node, &mut notices, &mut late, &mut described)
            } else if let Node::CallStmt(_) = node {
                drop(self.portals.get_mut().close(b""));
                self.call(node, &mut notices, &mut late, &mut described)
            } else if roles::is_role_statement(node) {
                drop(self.portals.get_mut().close(b""));
                self.role_statement(node, &mut notices)
            } else if rupg_analyze::is_definition(node) {
                drop(self.portals.get_mut().close(b""));
                self.define(node, statement, &mut notices)
            } else if let Node::LockStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.lock(stmt)
            } else if let Some(done) = self.async_statement(node) {
                drop(self.portals.get_mut().close(b""));
                done
            } else if let Node::ConstraintsSetStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.set_constraints(stmt, &mut notices)
            } else if let Node::CopyStmt(stmt) = node
                && stmt.is_from
                && stmt.filename.is_some()
            {
                drop(self.portals.get_mut().close(b""));
                self.copy_from_file(stmt, &mut notices)
            } else if let Node::CopyStmt(stmt) = node
                && stmt.is_from
            {
                drop(self.portals.get_mut().close(b""));
                match self.copy_from(stmt, Begin::Client(out)) {
                    Ok(mut copy) => {
                        copy.text = text.to_string();
                        copy.index = i;
                        self.copy_in = Some(copy);
                        self.session.copy_in();
                        return false;
                    }
                    Err(error) => Err(error),
                }
            } else if let Node::CopyStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.copy(stmt, &mut copying)
            } else if let Node::CreatedbStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.create_database(stmt, &mut notices)
            } else if let Node::DropdbStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.drop_database(stmt, &mut notices)
            } else if let Node::AlterDatabaseSetStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.alter_database_set(stmt)
            } else if let Node::CreateTableSpaceStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.create_tablespace(stmt)
            } else if let Node::DropTableSpaceStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.drop_tablespace(stmt, &mut notices)
            } else if let Node::VacuumStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.vacuum(stmt, &mut notices)
            } else if let Node::ReindexStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.reindex(node, stmt, statement, &mut notices, false)
            } else if let Node::DeclareCursorStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.declare(stmt, text, None, &[], &mut notices)
            } else if let Node::FetchStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.fetch_cursor(stmt, None)
            } else if let Node::ClosePortalStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                self.close_cursor(stmt, b"")
            } else if let Node::DoStmt(stmt) = node {
                drop(self.portals.get_mut().close(b""));
                let mut warnings = Vec::new();
                let result = self.run_do(stmt, text, &mut warnings);
                notices.extend(warnings.into_iter().map(Notice::of_function));
                result.map(|()| Done::Tag(CommandTag::Do))
            } else {
                drop(self.portals.get_mut().close(b""));
                match self.ts_config(node) {
                    Ok(ts_config) => {
                        let mut cx = Context {
                            settings: self.settings.get_mut(),
                            transaction: &mut self.transaction,
                            user: &self.user,
                            notices: &mut notices,
                            immediate_commit: false,
                            ts_config,
                            top_level: true,
                        };
                        utility::run(node, statement, &mut cx).map(Done::from)
                    }
                    Err(error) => Err(error),
                }
            };
            self.notices(&mut notices, text, &[], out);
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    let sent = described.map(|(columns, rows)| {
                        let fields: Vec<Field<'_>> =
                            columns.iter().map(|column| column.field(0)).collect();
                        out.row_description(&fields);
                        rows
                    });
                    self.notices(&mut late, text, &[], out);
                    for row in sent.iter().flatten() {
                        let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
                        out.data_row(&values);
                    }
                    if let Some((binary, columns)) = copying {
                        copy::copy_out_response(out, binary, columns);
                    }
                    let position = error.position().map(|at| character_position(text, at));
                    self.fail(&error, position, out);
                    return true;
                }
            };
            if matches!(outcome, Done::Tag(CommandTag::DiscardAll)) {
                self.statements.deallocate_all();
                self.portals.get_mut().clear();
            }
            if matches!(outcome, Done::Tag(CommandTag::DiscardAll | CommandTag::DiscardSequences)) {
                self.sequences.get_mut().discard();
            }
            let last = i + 1 == statements.len();
            if last && many {
                self.transaction.end_implicit();
            }
            if last || matches!(node, Node::TransactionStmt(_)) {
                // The deferred triggers fire before the commit. Their error ends the transaction, and the tag of the statement does not go.
                let committed = self.pre_commit();
                late.extend(self.trigger_notices());
                if let Err(error) = committed {
                    self.notices(&mut late, text, &[], out);
                    self.fail(&error, None, out);
                    return true;
                }
                self.finish_xact();
            }
            let returned = match &outcome {
                Done::Returned { count, .. } => Some(*count),
                _ => None,
            };
            match outcome {
                Done::Tag(tag) => {
                    self.notices(&mut late, text, &[], out);
                    out.command_tag(tag, 0);
                }
                Done::Rows { columns, rows, tag } | Done::Returned { columns, rows, tag, .. } => {
                    let count = returned.unwrap_or(rows.len() as u64);
                    // The rows of `RETURNING` come after the end of the write, as `FillPortalStore` gives them, so the notices of the write come before the description of the rows.
                    if tag != CommandTag::Select {
                        self.notices(&mut late, text, &[], out);
                    }
                    let fields: Vec<Field<'_>> =
                        columns.iter().map(|column| column.field(0)).collect();
                    out.row_description(&fields);
                    self.notices(&mut late, text, &[], out);
                    for row in &rows {
                        let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
                        out.data_row(&values);
                    }
                    out.command_tag(tag, count);
                }
                Done::Fetch { columns, rows, format } => {
                    let fields: Vec<Field<'_>> =
                        columns.iter().map(|column| column.field(format)).collect();
                    out.row_description(&fields);
                    self.notices(&mut late, text, &[], out);
                    for row in &rows {
                        let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
                        out.data_row(&values);
                    }
                    out.command_tag(CommandTag::Fetch, rows.len() as u64);
                }
                Done::Count(tag, count) => {
                    self.notices(&mut late, text, &[], out);
                    out.command_tag(tag, count);
                }
                Done::Copy(mut copy) => self.send_copy(&mut copy, text, out),
            }
        }
        false
    }

    /// Plans and runs a query of a `Query` message. The warnings of the analysis and of the plan go to `notices`, and the warnings of the run go to `late`. When the run fails after the plan, the columns of the plan and the rows before the error go to `described`.
    fn select(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
        late: &mut Vec<Notice>,
        described: &mut Option<(Vec<Column>, Vec<Row>)>,
    ) -> Result<Done, Error> {
        self.settings.borrow_mut().take_snapshot();
        if let Some(products) = rupg_analyze::query_rewrite(node, &self.reader())? {
            return self.run_products(node, &products, notices, late);
        }
        let reader = self.reader();
        let plan = query::plan(node, &reader, &Params::default());
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        let plan = plan.fold(None, &reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        plan.start(&reader)?;
        let Some(view) = reader.view() else {
            return Err(Error::internal("the reader of the session has no view of the rows"));
        };
        // `PORTAL_ONE_MOD_WITH`: `FillPortalStore` runs all of a statement with a write in `WITH` before the description of the rows, so all its notices come first.
        let store = plan.modifying_cte();
        let (plan, writes) = self.start_writes(plan, &[], view)?;
        if plan.write().is_some() {
            let rows = plan.run(&[], &reader);
            late.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            let done = self.write_rows(&plan, &[], view, rows?, &[], writes);
            late.extend(self.trigger_notices());
            return done;
        }
        let columns = query::columns(&plan);
        let rows = query::run_select(&plan, &[], &reader, &[]);
        late.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        match rows {
            Ok(rows) => {
                let ended = self.end_writes(writes, &[], Vec::new());
                late.extend(self.trigger_notices());
                ended?;
                if store {
                    notices.append(late);
                }
                Ok(Done::Rows { columns, rows, tag: CommandTag::Select })
            }
            Err((rows, error)) => {
                *described = Some((columns, rows));
                Err(error)
            }
        }
    }

    /// `ProcessQuery` for each statement of the rewrite of a write by the rules of its table, in order. Each statement sees the writes of the statements before it. The statement that sets the tag gives the result, and with none, the tag of the write has no rows.
    fn run_products(
        &self,
        node: &Node,
        products: &[rupg_analyze::Product],
        notices: &mut Vec<Notice>,
        late: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let mut done = None;
        let mut returned = None;
        // The rows of an action `SELECT`, which the client gets when no list of `RETURNING` gives the rows, as each query of `PORTAL_MULTI_QUERY` sends its own rows.
        let mut selected = None;
        for product in products {
            if let Some(done) = self.async_statement(&product.stmt) {
                done?;
                continue;
            }
            let reader = self.reader();
            let plan = rupg_analyze::analyze_product(product, &reader, &Params::default())
                .and_then(|query| rupg_exec::prepare(query, &reader));
            notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            let plan = plan?;
            notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
            let plan = plan.fold(None, &reader);
            notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            let plan = plan?;
            plan.start(&reader)?;
            if plan.write().is_none() {
                // An action `SELECT` runs as a query, and its rows go to the client.
                let mut columns = query::columns(&plan);
                let rows = query::run_select(&plan, &[], &reader, &[]).map_err(|(_, error)| error);
                late.extend(reader.take_warnings().into_iter().map(Notice::of_function));
                let rows = rows?;
                if selected.is_some() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "more than one rule action SELECT is not supported yet",
                    ));
                }
                for column in &mut columns {
                    column.origin = (0, 0);
                }
                selected = Some((columns, rows));
                continue;
            }
            let Some(view) = reader.view() else {
                return Err(Error::internal("the reader of the session has no view of the rows"));
            };
            let (plan, writes) = self.start_writes(plan, &[], view)?;
            let rows = plan.run(&[], &reader);
            late.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            let result = self.write_rows(&plan, &[], view, rows?, &[], writes);
            late.extend(self.trigger_notices());
            let result = result?;
            if product.gives_rows()
                && !product.sets_tag
                && let Done::Rows { mut columns, rows, .. } = result
            {
                // The columns of the rows of an action that does not set the tag have no table, as `PortalStart` describes them from the statement that sets it.
                for column in &mut columns {
                    column.origin = (0, 0);
                }
                returned = Some((columns, rows));
            } else if product.sets_tag {
                done = Some(result);
            }
        }
        let tag = match node {
            Node::InsertStmt(_) => CommandTag::Insert,
            Node::UpdateStmt(_) => CommandTag::Update,
            _ => CommandTag::Delete,
        };
        let done = done.unwrap_or(Done::Count(tag, 0));
        Ok(match (returned.or(selected), done) {
            (Some((columns, rows)), Done::Count(tag, count)) => {
                Done::Returned { columns, rows, tag, count }
            }
            (_, done) => done,
        })
    }

    /// `ExecuteCallStmt`: `CALL` runs as a query that calls the procedure. A procedure with output arguments gives one row with them, and a procedure with none gives no rows.
    fn call(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
        late: &mut Vec<Notice>,
        described: &mut Option<(Vec<Column>, Vec<Row>)>,
    ) -> Result<Done, Error> {
        // `ExecuteCallStmt` describes the row only when it sends the row, so an error gives no description.
        let done = self.top_call(|| self.select(node, notices, late, described));
        if done.is_err() {
            *described = None;
        }
        match done? {
            Done::Rows { columns, rows, .. }
                if columns.first().is_none_or(|c| c.ty != rupg_types::oid::VOID) =>
            {
                Ok(Done::Rows { columns, rows, tag: CommandTag::Call })
            }
            _ => Ok(Done::Tag(CommandTag::Call)),
        }
    }

    /// `start_xact_command`: the start of a statement, which starts a transaction if there is none. After an `Execute` that completed, it starts an implicit block.
    fn start_xact(&mut self) {
        self.sync_role();
        if !self.xact_started {
            if self.transaction.start_command() {
                self.settings.get_mut().start_transaction(None);
                self.transaction_start = self.statement_start;
                self.transaction_started();
            }
            self.xact_started = true;
        } else if self.pipelining {
            self.transaction.begin_implicit();
        }
    }

    /// `finish_xact_command`: the end of a statement, and the end or the chained start of its transaction. It does nothing when no statement runs.
    fn finish_xact(&mut self) {
        if !self.xact_started {
            return;
        }
        self.xact_started = false;
        let (ending, chained) = self.transaction.finish_command();
        let kept = chained.then(|| self.settings.get_mut().characteristics());
        if ending != Ending::None {
            self.settings.get_mut().end(ending == Ending::Commit);
            let defined = self.lease.get_mut().is_some();
            if let Some(lease) = self.lease.get_mut().take()
                && ending == Ending::Commit
            {
                lease.commit();
            }
            self.end_rows(ending == Ending::Commit, defined);
            self.transaction_ended();
        }
        if kept.is_some() {
            self.settings.get_mut().start_transaction(kept);
            self.transaction_start = self.statement_start;
            self.transaction_started();
        }
        if chained || self.transaction.block() == Block::Default {
            self.ended(ending != Ending::Rollback);
        }
    }

    /// The end of a transaction, which removes the portals. A cursor `WITH HOLD` stays after a commit, and a cursor `WITH HOLD` of a transaction before this one stays after a rollback, as `PreCommit_Portals` and `AtCleanup_Portals` keep them.
    fn ended(&mut self, commit: bool) {
        self.portals.get_mut().retain(|_, portal| match &mut portal.run {
            extended::Run::Cursor(cursor) if commit && cursor.holdable() => {
                cursor.fresh = false;
                true
            }
            extended::Run::Cursor(cursor) => cursor.held(),
            _ => false,
        });
        self.pipelining = false;
    }

    /// Writes the error of a statement and aborts its transaction. In a failed block the portals stay, but they cannot run.
    fn fail(&mut self, error: &Error, position: Option<usize>, out: &mut OutBuf) {
        write_error(out, "ERROR", error, position, false);
        self.abort();
    }

    /// The part of `AbortCurrentTransaction` of [`Connection::fail`] after the error went to the client.
    fn abort(&mut self) {
        self.xact_started = false;
        match self.transaction.abort_current() {
            Ending::Savepoint => self.roll_back_subtransaction(),
            ending => {
                if ending == Ending::Rollback {
                    self.settings.get_mut().end(false);
                    *self.lease.get_mut() = None;
                    self.end_rows(false, false);
                }
                self.transaction_ended();
            }
        }
        if self.transaction.failed() {
            // In a subtransaction only the portals of the subtransaction fail.
            let depth = self.transaction.depth();
            self.portals.get_mut().retain(|_, portal| {
                let held = portal.as_cursor().is_some_and(cursor::Cursor::held);
                portal.failed |= portal.level >= depth && !held;
                true
            });
        } else {
            self.ended(false);
        }
    }
}

impl Drop for Connection {
    /// `ShutdownPostgres` and `RemoveTempRelationsCallback`: the open transaction rolls back, then the temporary schemas of the session go with all their objects.
    fn drop(&mut self) {
        self.settings.get_mut().end(false);
        *self.lease.get_mut() = None;
        self.end_rows(false, false);
        self.xact_started = false;
        self.transaction = Transaction::default();
        self.remove_temp();
        self.leave();
    }
}

impl Connection {
    /// `RemoveTempRelations`: drops the temporary schema of the session and its schema for TOAST tables. The drop runs as the bootstrap superuser, which owns the schemas.
    fn remove_temp(&mut self) {
        let Some(number) = self.activity.number else { return };
        let temp = format!("pg_temp_{number}");
        if self.store.committed(self.database_oid).schema_by_name(&temp).is_none() {
            return;
        }
        let settings = self.settings.get_mut();
        drop(settings.set_internal("role", "none"));
        drop(settings.set_internal("session_authorization", &self.user));
        let mut out = OutBuf::new();
        self.simple_query(&format!("DROP SCHEMA {temp}, pg_toast_temp_{number} CASCADE"), &mut out);
        self.finish_xact();
    }
}

/// `AlterObjectTypeCommandTag` for the kinds of relations, of types, of functions, of operators, of operator classes and families, and of statistics objects: the tag of `ALTER TABLE` and of `ALTER ... RENAME` for the kind of the object.
fn alter_tag(kind: ObjectType) -> CommandTag {
    match kind {
        ObjectType::OBJECT_INDEX => CommandTag::AlterIndex,
        ObjectType::OBJECT_SEQUENCE => CommandTag::AlterSequence,
        ObjectType::OBJECT_VIEW => CommandTag::AlterView,
        ObjectType::OBJECT_MATVIEW => CommandTag::AlterMaterializedView,
        ObjectType::OBJECT_FOREIGN_TABLE => CommandTag::AlterForeignTable,
        ObjectType::OBJECT_TYPE => CommandTag::AlterType,
        ObjectType::OBJECT_DOMAIN => CommandTag::AlterDomain,
        ObjectType::OBJECT_FUNCTION => CommandTag::AlterFunction,
        ObjectType::OBJECT_ROUTINE => CommandTag::AlterRoutine,
        ObjectType::OBJECT_PROCEDURE => CommandTag::AlterProcedure,
        ObjectType::OBJECT_AGGREGATE => CommandTag::AlterAggregate,
        ObjectType::OBJECT_TRIGGER => CommandTag::AlterTrigger,
        ObjectType::OBJECT_RULE => CommandTag::AlterRule,
        ObjectType::OBJECT_POLICY => CommandTag::AlterPolicy,
        ObjectType::OBJECT_OPERATOR => CommandTag::AlterOperator,
        ObjectType::OBJECT_OPCLASS => CommandTag::AlterOperatorClass,
        ObjectType::OBJECT_OPFAMILY => CommandTag::AlterOperatorFamily,
        ObjectType::OBJECT_STATISTIC_EXT => CommandTag::AlterStatistics,
        _ => CommandTag::AlterTable,
    }
}

/// The OID of the role of a session. The superuser of the server is the bootstrap superuser.
fn role_oid(start: &Start) -> u32 {
    if start.superuser {
        return BOOTSTRAP_SUPERUSER;
    }
    builtin::roles().iter().find(|role| role.name == start.user).map_or(0, |role| role.oid)
}

/// The statements of a parsed `Query` message, each with its part of the text.
fn statements<'a>(list: &'a rupg_sql::nodes::List, text: &'a str) -> Vec<(&'a Node, &'a str)> {
    list.iter()
        .flatten()
        .filter_map(|node| match node {
            Node::RawStmt(raw) => {
                let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(text.len());
                let end = match usize::try_from(raw.stmt_len) {
                    Ok(0) | Err(_) => text.len(),
                    Ok(len) => (start + len).min(text.len()),
                };
                raw.stmt.as_ref().map(|stmt| (stmt, text.get(start..end).unwrap_or(text)))
            }
            _ => None,
        })
        .collect()
}

/// The position of a byte offset in characters, from 1, as the field `P` gives it. An offset after the end of the text, such as an offset in the text of a prepared statement that `EXECUTE` plans again, counts one character for each byte after the end, as PostgreSQL gives the position in the text of the statement.
fn character_position(text: &str, at: usize) -> usize {
    let end = at.min(text.len());
    text.as_bytes()[..end].iter().filter(|&&b| b & 0xc0 != 0x80).count() + (at - end) + 1
}

#[cfg(test)]
mod tests {
    use rupg_wire::{Backend, CANCEL_KEY_LEN};

    use super::*;

    const PROTOCOL_3_0: u32 = 3 << 16;

    /// Runs the cases on a thread with the stack of a backend. The parser of a debug build needs more than the stack of a test thread.
    #[allow(clippy::disallowed_methods)] // A test thread, not a task of the engine.
    fn big_stack(cases: fn()) {
        std::thread::Builder::new().stack_size(8 << 20).spawn(cases).unwrap().join().unwrap();
    }

    fn start(options: Option<&str>, settings: &[(&str, &str)]) -> Start {
        Start {
            user: "postgres".into(),
            database: "postgres".into(),
            superuser: true,
            options: options.map(str::to_owned),
            settings: settings.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
        }
    }

    fn connect() -> (Connection, Vec<String>) {
        connect_to(&Arc::new(Store::new()), 1234)
    }

    /// A session with the process ID `pid` on the store of a server.
    fn connect_to(store: &Arc<Store>, pid: i32) -> (Connection, Vec<String>) {
        let start = start(None, &[("application_name", "test")]);
        let settings = session_settings(&base_settings(&[]).unwrap(), &start, &[]).unwrap();
        let mut connection = Connection::new(&start, settings, PROTOCOL_3_0);
        connection.set_store(Arc::clone(store));
        // The oracle runs the scripts on a Unix socket.
        connection.set_addresses(Client::Local, Client::Local);
        let mut out = OutBuf::new();
        connection.greet(&CancelKey::new(pid, PROTOCOL_3_0, [7; CANCEL_KEY_LEN]), &mut out);
        let mut lines = render(&out, false);
        lines.extend(send(&mut connection, &[]));
        (connection, lines)
    }

    /// A message of the client: the type byte, the length and the body.
    fn message(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn query(sql: &str) -> Vec<u8> {
        message(b'Q', format!("{sql}\0").as_bytes())
    }

    fn parse(name: &str, sql: &str, types: &[u32]) -> Vec<u8> {
        let mut body = format!("{name}\0{sql}\0").into_bytes();
        body.extend(u16::try_from(types.len()).unwrap().to_be_bytes());
        for t in types {
            body.extend(t.to_be_bytes());
        }
        message(b'P', &body)
    }

    fn bind(
        portal: &str,
        statement: &str,
        formats: &[i16],
        values: &[Option<&[u8]>],
        results: &[i16],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        rupg_wire::Bind::encode(
            &mut out,
            portal.as_bytes(),
            statement.as_bytes(),
            formats,
            values,
            results,
        );
        out
    }

    fn describe(target: char, name: &str) -> Vec<u8> {
        message(b'D', format!("{target}{name}\0").as_bytes())
    }

    fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
        let mut body = format!("{portal}\0").into_bytes();
        body.extend(max_rows.to_be_bytes());
        message(b'E', &body)
    }

    fn close(target: char, name: &str) -> Vec<u8> {
        message(b'C', format!("{target}{name}\0").as_bytes())
    }

    fn sync() -> Vec<u8> {
        message(b'S', b"")
    }

    /// Parse, Bind and Execute of the unnamed statement and portal.
    fn run(sql: &str) -> Vec<u8> {
        [parse("", sql, &[]), bind("", "", &[], &[], &[]), execute("", 0)].concat()
    }

    /// Gives `input` to the connection until it needs more, and renders what it wrote.
    fn exchange(connection: &mut Connection, input: &[u8]) -> OutBuf {
        let mut out = OutBuf::new();
        let mut at = 0;
        loop {
            let step = connection.step(&input[at..], &mut out);
            at += step.used;
            match step.next {
                Next::Read => break,
                Next::Close => {
                    out.command_complete(b"(closed)");
                    break;
                }
                Next::Continue | Next::Flush => {}
            }
        }
        assert_eq!(at, input.len());
        out
    }

    fn send(connection: &mut Connection, input: &[u8]) -> Vec<String> {
        render(&exchange(connection, input), false)
    }

    /// One short line for each message. With `full`, a `RowDescription` is a line `fields` with the name, the table OID, the column number, the type, the size and the typmod of each column, and else a line `columns` with the names.
    fn render(out: &OutBuf, full: bool) -> Vec<String> {
        render_details(out, full, Details::None)
    }

    /// The lines of an error or a notice that [`render_details`] adds.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Details {
        None,
        /// A line `DETAIL`, a line `HINT` and a line `QUERY` when the message has them.
        Lines,
        /// [`Details::Lines`], then a line `SCHEMA`, `TABLE`, `COLUMN`, `DATATYPE` or `CONSTRAINT` for each name of the object of the message.
        Objects,
    }

    /// [`render`], with the lines of `details` after an error or a notice.
    fn render_details(out: &OutBuf, full: bool, details: Details) -> Vec<String> {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let field = |fields: &[(u8, &[u8])], code: u8| {
            fields.iter().find(|(c, _)| *c == code).map_or(String::new(), |(_, v)| text(v))
        };
        let mut input = out.as_bytes();
        let mut lines = Vec::new();
        while let Some((message, size)) = Backend::decode(input).unwrap() {
            input = &input[size..];
            lines.push(match message {
                Backend::BackendKeyData { pid, .. } => format!("key {pid}"),
                Backend::ParameterStatus { name, value } => {
                    format!("{}={}", text(name), text(value))
                }
                Backend::ReadyForQuery(status) => format!("ready {}", char::from(status.byte())),
                Backend::CommandComplete(tag) => text(tag),
                Backend::EmptyQueryResponse => "empty".into(),
                Backend::RowDescription(fields) if full => {
                    let parts: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            let name = match f.table {
                                0 => text(f.name),
                                table => format!("{}@{table}.{}", text(f.name), f.column),
                            };
                            format!("{name}:{}:{}:{}", f.type_oid, f.type_size, f.type_modifier)
                        })
                        .collect();
                    format!("fields {}", parts.join(","))
                }
                Backend::RowDescription(fields) => {
                    let names: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            let name = match f.table {
                                0 => text(f.name),
                                table => format!("{}@{table}.{}", text(f.name), f.column),
                            };
                            match f.format {
                                0 => name,
                                format => format!("{name}/{format}"),
                            }
                        })
                        .collect();
                    format!("columns {}", names.join(","))
                }
                Backend::ParameterDescription(types) => {
                    let types: Vec<String> = types.iter().map(u32::to_string).collect();
                    format!("params {}", types.join(","))
                }
                Backend::DataRow(values) => {
                    let values: Vec<String> =
                        values.iter().map(|v| text(v.unwrap_or(b"NULL"))).collect();
                    format!("row {}", values.join(","))
                }
                Backend::CopyInResponse { format, columns } => {
                    let columns: Vec<String> = columns.iter().map(i16::to_string).collect();
                    format!("copy in {format} {}", columns.join(","))
                }
                Backend::CopyOutResponse { format, columns } => {
                    let columns: Vec<String> = columns.iter().map(i16::to_string).collect();
                    format!("copy out {format} {}", columns.join(","))
                }
                Backend::CopyData(data) => format!("data {}", data.escape_ascii()),
                Backend::CopyDone => "copy done".into(),
                Backend::NotificationResponse { channel, payload, .. } => {
                    format!("notify {} {}", text(channel), text(payload))
                }
                Backend::ErrorResponse(fields) | Backend::NoticeResponse(fields) => {
                    let mut line = format!(
                        "{} {} {}",
                        field(&fields, b'S'),
                        field(&fields, b'C'),
                        field(&fields, b'M')
                    );
                    let position = field(&fields, b'P');
                    if !position.is_empty() {
                        line.push_str(&format!(" at {position}"));
                    }
                    let context = field(&fields, b'W');
                    if !context.is_empty() {
                        line.push_str(&format!(" ({context})"));
                    }
                    for (code, word) in [(b'D', "DETAIL"), (b'H', "HINT")] {
                        let value = field(&fields, code);
                        if details >= Details::Lines && !value.is_empty() {
                            line.push_str(&format!("\n{word} {value}"));
                        }
                    }
                    let internal = field(&fields, b'q');
                    if details >= Details::Lines && !internal.is_empty() {
                        line.push_str(&format!("\nQUERY {internal}"));
                        let position = field(&fields, b'p');
                        if !position.is_empty() {
                            line.push_str(&format!(" at {position}"));
                        }
                    }
                    let objects = [
                        (b's', "SCHEMA"),
                        (b't', "TABLE"),
                        (b'c', "COLUMN"),
                        (b'd', "DATATYPE"),
                        (b'n', "CONSTRAINT"),
                    ];
                    for (code, word) in objects {
                        let value = field(&fields, code);
                        if details == Details::Objects && !value.is_empty() {
                            line.push_str(&format!("\n{word} {value}"));
                        }
                    }
                    line
                }
                other => format!("{other:?}"),
            });
        }
        assert!(input.is_empty());
        lines
    }

    #[test]
    fn the_start() {
        big_stack(the_start_cases);
    }

    fn the_start_cases() {
        let (_, lines) = connect();
        let version = format!("server_version=19.0 (rupg {VERSION})");
        for line in [
            version.as_str(),
            "application_name=test",
            "client_encoding=UTF8",
            "is_superuser=on",
            "session_authorization=postgres",
        ] {
            assert!(lines.iter().any(|l| l == line), "{line} in {lines:?}");
        }
        let n = lines.len();
        assert_eq!(
            lines[n - 3..],
            [format!("rupg.version={VERSION}"), "key 1234".into(), "ready I".into()]
        );
    }

    #[test]
    fn simple_queries() {
        big_stack(simple_queries_cases);
    }

    fn simple_queries_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(&mut c, &query("SET work_mem = '8MB'; SHOW work_mem")),
            ["SET", "columns work_mem", "row 8MB", "SHOW", "ready I"]
        );
        assert_eq!(send(&mut c, &query("")), ["empty", "ready I"]);
        assert_eq!(send(&mut c, &query(";;")), ["empty", "ready I"]);
        assert_eq!(
            send(&mut c, &query("SELEC 1")),
            ["ERROR 42601 syntax error at or near \"SELEC\" at 1", "ready I"]
        );
        // A parameter with GUC_REPORT is reported before ReadyForQuery when it changes.
        assert_eq!(
            send(&mut c, &query("SET application_name = 'x'")),
            ["SET", "application_name=x", "ready I"]
        );
    }

    #[test]
    fn set_config() {
        big_stack(set_config_cases);
    }

    /// `set_config` in a query. Each result is the result of PostgreSQL 19.
    fn set_config_cases() {
        let (mut c, _) = connect();
        let one = |c: &mut Connection, sql: &str| send(c, &query(sql));
        let row = |c: &mut Connection, sql: &str| one(c, sql)[1].clone();
        assert_eq!(
            one(&mut c, "SELECT set_config('work_mem', '16MB', false); SHOW work_mem"),
            [
                "columns set_config",
                "row 16MB",
                "SELECT 1",
                "columns work_mem",
                "row 16MB",
                "SHOW",
                "ready I"
            ]
        );
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', '1024', false)"), "row 1MB");
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', NULL, false)"), "row 4MB");
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', '16MB', NULL)"), "row 16MB");
        assert_eq!(row(&mut c, "SELECT set_config('Work_Mem', '2MB', false)"), "row 2MB");
        assert_eq!(one(&mut c, "RESET work_mem"), ["RESET", "ready I"]);
        assert_eq!(
            row(&mut c, "SELECT pg_typeof(set_config('work_mem', '4MB', false))"),
            "row text"
        );
        // `set_config` is volatile, so its errors come when the query runs, after the description of the rows.
        for (sql, error) in [
            ("SELECT set_config(NULL, 'x', false)", "ERROR 22004 SET requires parameter name"),
            (
                "SELECT set_config('nope', 'x', false)",
                "ERROR 42704 unrecognized configuration parameter \"nope\"",
            ),
            (
                "SELECT set_config('', 'x', false)",
                "ERROR 42704 unrecognized configuration parameter \"\"",
            ),
            (
                "SELECT set_config('role', 'nobody', false)",
                "ERROR 22023 role \"nobody\" does not exist",
            ),
            (
                "SELECT set_config('session_authorization', 'nobody', false)",
                "ERROR 22023 role \"nobody\" does not exist",
            ),
            (
                "SELECT set_config('server_version', '1', false)",
                "ERROR 55P02 parameter \"server_version\" cannot be changed",
            ),
            (
                "SELECT set_config('work_mem', 'abc', false)",
                "ERROR 22023 invalid value for parameter \"work_mem\": \"abc\"",
            ),
            (
                "SELECT set_config('work_mem', '', false)",
                "ERROR 22023 invalid value for parameter \"work_mem\": \"\"",
            ),
        ] {
            assert_eq!(one(&mut c, sql), ["columns set_config", error, "ready I"], "{sql}");
        }
        // A placeholder of an extension, and its reset to an empty string.
        assert_eq!(row(&mut c, "SELECT set_config('my.var', 'x', false)"), "row x");
        assert_eq!(row(&mut c, "SELECT current_setting('my.var')"), "row x");
        assert_eq!(row(&mut c, "SELECT set_config('my.var', NULL, false)"), "row ");
        assert_eq!(row(&mut c, "SELECT set_config('a.b.c', 'x', false)"), "row x");
        // A local value ends with its transaction.
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', '8MB', true)"), "row 8MB");
        assert_eq!(row(&mut c, "SHOW work_mem"), "row 4MB");
        assert_eq!(one(&mut c, "BEGIN"), ["BEGIN", "ready T"]);
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', '8MB', true)"), "row 8MB");
        assert_eq!(row(&mut c, "SHOW work_mem"), "row 8MB");
        assert_eq!(one(&mut c, "COMMIT"), ["COMMIT", "ready I"]);
        assert_eq!(row(&mut c, "SHOW work_mem"), "row 4MB");
        // An error or a rollback undoes the change.
        assert_eq!(
            one(&mut c, "SELECT set_config('work_mem', '16MB', false), 1/0"),
            ["ERROR 22012 division by zero", "ready I"]
        );
        assert_eq!(row(&mut c, "SHOW work_mem"), "row 4MB");
        assert_eq!(one(&mut c, "BEGIN"), ["BEGIN", "ready T"]);
        assert_eq!(row(&mut c, "SELECT set_config('work_mem', '16MB', false)"), "row 16MB");
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
        assert_eq!(row(&mut c, "SHOW work_mem"), "row 4MB");
        // The new value counts for the rest of the query, and a reported parameter is reported.
        assert_eq!(
            one(
                &mut c,
                "SELECT set_config('timezone', 'Etc/GMT-3', false), '2000-01-01 00:00+00'::timestamptz::text, '2000-01-01 00:00+00'::timestamptz"
            ),
            [
                "columns set_config,text,timestamptz",
                "row Etc/GMT-3,2000-01-01 03:00:00+03,2000-01-01 03:00:00+03",
                "SELECT 1",
                "TimeZone=Etc/GMT-3",
                "ready I"
            ]
        );
        assert_eq!(
            one(
                &mut c,
                "SELECT '2000-01-01 00:00+00'::timestamptz::text, set_config('timezone', 'UTC', false), '2000-01-01 00:00+00'::timestamptz::text"
            ),
            [
                "columns text,set_config,text",
                "row 2000-01-01 03:00:00+03,UTC,2000-01-01 00:00:00+00",
                "SELECT 1",
                "TimeZone=UTC",
                "ready I"
            ]
        );
        assert_eq!(
            one(
                &mut c,
                "SELECT set_config('search_path', 'pg_catalog', false), current_schemas(false)"
            ),
            [
                "columns set_config,current_schemas",
                "row pg_catalog,{pg_catalog}",
                "SELECT 1",
                "search_path=pg_catalog",
                "ready I"
            ]
        );
        assert_eq!(
            one(&mut c, "RESET search_path"),
            ["RESET", "search_path=\"$user\", public", "ready I"]
        );
        assert_eq!(
            one(&mut c, "SELECT set_config('datestyle', 'german', false)"),
            [
                "columns set_config",
                "row German, DMY",
                "SELECT 1",
                "DateStyle=German, DMY",
                "ready I"
            ]
        );
        assert_eq!(one(&mut c, "RESET datestyle"), ["RESET", "DateStyle=ISO, MDY", "ready I"]);
        // A query takes a snapshot, so the isolation level and the deferrable mode cannot change after it.
        assert_eq!(
            row(&mut c, "SELECT set_config('transaction_isolation', 'read committed', false)"),
            "row read committed"
        );
        assert_eq!(
            one(&mut c, "SELECT set_config('transaction_isolation', 'serializable', false)"),
            [
                "columns set_config",
                "ERROR 25001 SET TRANSACTION ISOLATION LEVEL must be called before any query",
                "ready I"
            ]
        );
        assert_eq!(
            one(&mut c, "SELECT set_config('transaction_deferrable', 'on', false)"),
            [
                "columns set_config",
                "ERROR 25001 SET TRANSACTION [NOT] DEFERRABLE must be called before any query",
                "ready I"
            ]
        );
        assert_eq!(
            row(&mut c, "SELECT set_config('transaction_read_only', 'on', false)"),
            "row on"
        );
        assert_eq!(
            row(&mut c, "SELECT set_config('transaction_read_only', 'off', false)"),
            "row off"
        );
        assert_eq!(
            row(&mut c, "SELECT set_config('default_transaction_read_only', 'on', true)"),
            "row on"
        );
    }

    #[test]
    fn transaction_modes_after_a_query() {
        big_stack(transaction_modes_after_a_query_cases);
    }

    /// `SET TRANSACTION` after the first snapshot of the transaction. Each result is the result of PostgreSQL 19.
    fn transaction_modes_after_a_query_cases() {
        let (mut c, _) = connect();
        let one = |c: &mut Connection, sql: &str| send(c, &query(sql));
        assert_eq!(one(&mut c, "BEGIN READ ONLY")[0], "BEGIN");
        assert_eq!(one(&mut c, "SELECT 1")[1], "row 1");
        assert_eq!(one(&mut c, "SET TRANSACTION READ ONLY"), ["SET", "ready T"]);
        assert_eq!(
            one(&mut c, "SET TRANSACTION READ WRITE"),
            ["ERROR 25001 transaction read-write mode must be set before any query", "ready E"]
        );
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
        assert_eq!(one(&mut c, "BEGIN READ ONLY")[0], "BEGIN");
        assert_eq!(one(&mut c, "SET TRANSACTION READ WRITE"), ["SET", "ready T"]);
        assert_eq!(one(&mut c, "SHOW transaction_read_only")[1], "row off");
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
        // SHOW takes no snapshot.
        assert_eq!(one(&mut c, "BEGIN")[0], "BEGIN");
        assert_eq!(one(&mut c, "SHOW work_mem")[1], "row 4MB");
        assert_eq!(
            one(&mut c, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
            ["SET", "ready T"]
        );
        assert_eq!(one(&mut c, "SHOW transaction_isolation")[1], "row repeatable read");
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
        assert_eq!(one(&mut c, "BEGIN")[0], "BEGIN");
        assert_eq!(one(&mut c, "SELECT 1")[1], "row 1");
        assert_eq!(
            one(&mut c, "SET TRANSACTION NOT DEFERRABLE"),
            [
                "ERROR 25001 SET TRANSACTION [NOT] DEFERRABLE must be called before any query",
                "ready E"
            ]
        );
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
        assert_eq!(
            one(&mut c, "SELECT 1; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
            [
                "columns ?column?",
                "row 1",
                "SELECT 1",
                "ERROR 25001 SET TRANSACTION ISOLATION LEVEL must be called before any query",
                "ready I"
            ]
        );
        assert_eq!(
            one(
                &mut c,
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; SELECT 1; SHOW transaction_isolation"
            ),
            [
                "SET",
                "columns ?column?",
                "row 1",
                "SELECT 1",
                "columns transaction_isolation",
                "row repeatable read",
                "SHOW",
                "ready I"
            ]
        );
        // Parse of a query in a block takes the snapshot too.
        assert_eq!(one(&mut c, "BEGIN")[0], "BEGIN");
        assert_eq!(
            send(&mut c, &[parse("", "SELECT 1", &[]), sync()].concat()),
            ["ParseComplete", "ready T"]
        );
        assert_eq!(
            one(&mut c, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
            [
                "ERROR 25001 SET TRANSACTION ISOLATION LEVEL must be called before any query",
                "ready E"
            ]
        );
        assert_eq!(one(&mut c, "ROLLBACK"), ["ROLLBACK", "ready I"]);
    }

    #[test]
    fn a_failed_block() {
        big_stack(a_failed_block_cases);
    }

    fn a_failed_block_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(&mut c, &query("BEGIN; SET work_mem = '1MB'; SELECT 1/0")),
            ["BEGIN", "SET", "ERROR 22012 division by zero", "ready E"]
        );
        assert_eq!(
            send(&mut c, &query("SHOW work_mem")),
            [
                "ERROR 25P02 current transaction is aborted, commands ignored until end of transaction block",
                "ready E"
            ]
        );
        assert_eq!(send(&mut c, &query("COMMIT")), ["ROLLBACK", "ready I"]);
        assert_eq!(
            send(&mut c, &query("SHOW work_mem")),
            ["columns work_mem", "row 4MB", "SHOW", "ready I"]
        );
        // An error in the implicit block of a Query rolls back the statements before it.
        assert_eq!(
            send(&mut c, &query("SET work_mem = '2MB'; SELECT 1/0")),
            ["SET", "ERROR 22012 division by zero", "ready I"]
        );
        assert_eq!(send(&mut c, &query("SHOW work_mem"))[1], "row 4MB");
    }

    #[test]
    fn notices() {
        big_stack(notices_cases);
    }

    fn queries_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(&mut c, &query("SELECT 1 AS a, 'x' || 'y', NULL::int4")),
            ["columns a,?column?,int4", "row 1,xy,NULL", "SELECT 1", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("SELECT 1 WHERE false")),
            ["columns ?column?", "SELECT 0", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("SELECT amname, oid::int, tableoid FROM pg_am WHERE oid < 405")),
            [
                "columns amname@2601.2,oid,tableoid@2601.-6",
                "row heap,2,2601",
                "row btree,403,2601",
                "SELECT 2",
                "ready I"
            ]
        );
        assert_eq!(
            send(&mut c, &query("SELECT current_user, current_database(), pg_backend_pid()")),
            [
                "columns current_user,current_database,pg_backend_pid",
                "row postgres,postgres,1234",
                "SELECT 1",
                "ready I"
            ]
        );
        assert_eq!(
            send(&mut c, &query("SELECT version() LIKE 'PostgreSQL 19.0 (rupg %, 64-bit'"))[1],
            "row t"
        );
        // The times of a transaction block are the time of its start.
        assert_eq!(
            send(
                &mut c,
                &query("BEGIN; SELECT now() = statement_timestamp(), now() <= clock_timestamp()")
            )[2],
            "row t,t"
        );
        assert_eq!(send(&mut c, &query("COMMIT")), ["COMMIT", "ready I"]);
        assert_eq!(
            send(
                &mut c,
                &query("SET TimeZone = 'Etc/GMT+5'; SELECT '2000-01-01 12:00+00'::timestamptz")
            ),
            [
                "SET",
                "columns timestamptz",
                "row 2000-01-01 07:00:00-05",
                "SELECT 1",
                "TimeZone=Etc/GMT+5",
                "ready I"
            ]
        );
        assert_eq!(
            send(
                &mut c,
                &query(
                    "SET TimeZone = 'Europe/Paris'; SELECT '2024-07-01 12:00:00+00'::timestamptz"
                )
            ),
            [
                "SET",
                "columns timestamptz",
                "row 2024-07-01 14:00:00+02",
                "SELECT 1",
                "TimeZone=Europe/Paris",
                "ready I"
            ]
        );
    }

    /// `CREATE SCHEMA`, `CREATE TABLE` and `CREATE INDEX`, with the messages of PostgreSQL 19 for the same statements.
    fn definitions_cases() {
        let (mut c, _) = connect();
        assert_eq!(send(&mut c, &query("CREATE SCHEMA s")), ["CREATE SCHEMA", "ready I"]);
        assert_eq!(
            send(&mut c, &query("SET search_path = s, public; SELECT current_schemas(false)")),
            [
                "SET",
                "columns current_schemas",
                "row {s,public}",
                "SELECT 1",
                "search_path=s, public",
                "ready I"
            ]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE t (a int PRIMARY KEY)")),
            ["CREATE TABLE", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE t (a int)")),
            ["ERROR 42P07 relation \"t\" already exists", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE IF NOT EXISTS t (a int)")),
            ["NOTICE 42P07 relation \"t\" already exists, skipping", "CREATE TABLE", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("BEGIN; CREATE TABLE u (a int); ROLLBACK")),
            ["BEGIN", "CREATE TABLE", "ROLLBACK", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE u (b nosuch)")),
            ["ERROR 42704 type \"nosuch\" does not exist at 19", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE u (a int); CREATE INDEX ON u (a)")),
            ["CREATE TABLE", "CREATE INDEX", "ready I"]
        );
        let catalog = c.store.committed(c.database_oid);
        let names: Vec<&str> =
            catalog.relations().filter(|r| r.oid >= 16384).map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["t", "t_pkey", "u", "u_a_idx"]);
        assert_eq!(
            send(&mut c, &query("BEGIN; CREATE TABLE w (a int)")),
            ["BEGIN", "CREATE TABLE", "ready T"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE w (a int)")),
            ["ERROR 42P07 relation \"w\" already exists", "ready E"]
        );
        assert_eq!(
            send(&mut c, &query("CREATE TABLE x (a int)")),
            [
                "ERROR 25P02 current transaction is aborted, commands ignored until end of transaction block",
                "ready E"
            ]
        );
        assert_eq!(send(&mut c, &query("ROLLBACK")), ["ROLLBACK", "ready I"]);
        assert_eq!(send(&mut c, &query("CREATE TABLE w (a int)")), ["CREATE TABLE", "ready I"]);
        assert_eq!(
            send(&mut c, &run("CREATE TABLE pg_catalog.z (a int)")),
            [
                "ParseComplete",
                "BindComplete",
                "ERROR 42501 permission denied to create \"pg_catalog.z\""
            ]
        );
        assert_eq!(send(&mut c, &sync()), ["ready I"]);
        // A second session of the same server sees the catalog of the last commit.
        let (mut d, _) = connect();
        d.set_store(Arc::clone(&c.store));
        assert_eq!(
            send(&mut d, &query("CREATE TABLE s.t (a int)")),
            ["ERROR 42P07 relation \"t\" already exists", "ready I"]
        );
    }

    #[test]
    fn definitions() {
        big_stack(definitions_cases);
    }

    /// The queries of the catalog in `connection/catalog_rows.out`.
    const CATALOG_QUERIES: [(&str, &str); 9] = [
        (
            "pg_namespace",
            "SELECT oid, nspname, nspowner, nspacl FROM pg_namespace WHERE oid >= 16384 ORDER BY oid",
        ),
        (
            "pg_class",
            "SELECT oid, relname, relnamespace, reltype, reloftype, relowner, relam, relfilenode, reltablespace, relpages, reltuples, relallvisible, relallfrozen, reltoastrelid, relhasindex, relisshared, relpersistence, relkind, relnatts, relchecks, relhasrules, relhastriggers, relhassubclass, relrowsecurity, relforcerowsecurity, relispopulated, relreplident, relispartition, relrewrite, relfrozenxid = 0, relminmxid, relacl, reloptions, relpartbound FROM pg_class WHERE oid >= 16384 ORDER BY oid",
        ),
        (
            "pg_type",
            "SELECT oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typispreferred, typisdefined, typdelim, typrelid, typsubscript, typelem, typarray, typinput, typoutput, typreceive, typsend, typmodin, typmodout, typanalyze, typalign, typstorage, typnotnull, typbasetype, typtypmod, typndims, typcollation, typdefaultbin, typdefault, typacl FROM pg_type WHERE oid >= 16384 ORDER BY oid",
        ),
        (
            "pg_attribute",
            "SELECT attrelid, attname, atttypid, attlen, attnum, atttypmod, attndims, attbyval, attalign, attstorage, attcompression, attnotnull, atthasdef, atthasmissing, attidentity, attgenerated, attisdropped, attislocal, attinhcount, attcollation, attstattarget, attacl, attoptions, attfdwoptions, attmissingval FROM pg_attribute WHERE attrelid >= 16384 ORDER BY attrelid, attnum",
        ),
        (
            "pg_attrdef",
            "SELECT oid, adrelid, adnum, adbin IS NOT NULL FROM pg_attrdef WHERE oid >= 16384 ORDER BY oid",
        ),
        (
            "pg_constraint",
            "SELECT oid, conname, connamespace, contype, condeferrable, condeferred, conenforced, convalidated, conrelid, contypid, conindid, conparentid, confrelid, confupdtype, confdeltype, confmatchtype, conislocal, coninhcount, connoinherit, conperiod, conkey, confkey, conpfeqop, conppeqop, conffeqop, confdelsetcols, conexclop, conbin IS NOT NULL FROM pg_constraint WHERE oid >= 16384 ORDER BY oid",
        ),
        (
            "pg_index",
            "SELECT indexrelid, indrelid, indnatts, indnkeyatts, indisunique, indnullsnotdistinct, indisprimary, indisexclusion, indimmediate, indisclustered, indisvalid, indcheckxmin, indisready, indislive, indisreplident, indkey, indcollation, indclass, indoption, indexprs IS NOT NULL, indpred IS NOT NULL FROM pg_index WHERE indexrelid >= 16384 ORDER BY indexrelid",
        ),
        (
            "pg_sequence",
            "SELECT seqrelid, seqtypid, seqstart, seqincrement, seqmax, seqmin, seqcache, seqcycle FROM pg_sequence WHERE seqrelid >= 16384 ORDER BY seqrelid",
        ),
        (
            "pg_depend",
            "SELECT classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype FROM pg_depend WHERE objid >= 16384 AND classid <> 2620 ORDER BY classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype",
        ),
    ];

    /// The rows of the user objects in the catalog. `connection/catalog_rows.out` is the output of PostgreSQL 19 for the same statements and queries in a new database, in the unaligned format of `psql` with the field separator `,` and the null string `NULL`, with the OIDs moved to start at 16384. The queries leave out the expressions, which rupg keeps in its own form, and the value of `relfrozenxid`, which depends on the cluster.
    fn catalog_rows_cases() {
        let (mut c, _) = connect();
        let statements = [
            ("CREATE SCHEMA s", "CREATE SCHEMA"),
            (
                "CREATE TABLE s.t (a int PRIMARY KEY, b text NOT NULL DEFAULT 'x', c numeric(5,2) CHECK (c > 0), d serial)",
                "CREATE TABLE",
            ),
            ("CREATE INDEX ON s.t (lower(b)) WHERE a > 1", "CREATE INDEX"),
            ("CREATE TABLE s.u (a int REFERENCES s.t, b varchar(10)[] UNIQUE)", "CREATE TABLE"),
        ];
        for (sql, tag) in statements {
            assert_eq!(send(&mut c, &query(sql)), [tag, "ready I"], "{sql}");
        }
        let mut expected: Vec<(&str, Vec<&str>)> = Vec::new();
        for line in include_str!("connection/catalog_rows.out").lines() {
            match expected.last_mut() {
                Some((_, rows)) if !line.starts_with("pg_") => rows.push(line),
                _ => expected.push((line, Vec::new())),
            }
        }
        assert_eq!(expected.len(), CATALOG_QUERIES.len());
        for ((name, sql), (want_name, want)) in CATALOG_QUERIES.iter().zip(&expected) {
            assert_eq!(name, want_name);
            let lines = send(&mut c, &query(sql));
            let rows: Vec<&str> = lines.iter().filter_map(|l| l.strip_prefix("row ")).collect();
            for (i, (got, want)) in rows.iter().zip(want).enumerate() {
                assert_eq!(got, want, "row {} of {name}", i + 1);
            }
            assert_eq!(rows.len(), want.len(), "{name}: {lines:?}");
        }
        assert_eq!(
            send(&mut c, &query("CREATE INDEX ON pg_toast.pg_toast_16386 (chunk_id)")),
            ["ERROR 42501 permission denied: \"pg_toast_16386\" is a system catalog", "ready I"]
        );
        assert_eq!(
            send(
                &mut c,
                &query("CREATE TABLE v (a oid REFERENCES pg_toast.pg_toast_16386 (chunk_id))")
            ),
            ["ERROR 42809 referenced relation \"pg_toast_16386\" is not a table", "ready I"]
        );
    }

    #[test]
    fn catalog_rows() {
        big_stack(catalog_rows_cases);
    }

    /// Runs a script of statements on a new connection. A line that starts with `> ` is a simple query, and the lines after it are the messages that the query must give, in the full form of `render`, without the lines of `ReadyForQuery`. The table of a column shows as its `regclass` text, because the OIDs of the oracle are not the OIDs of a new database. A line `>> text` or `>>` after a statement goes on the statement as a new line. After a line `!details`, an error or a notice also has its detail, its hint and its internal query. After a line `!objects`, it also has the names of its object. The lines `< text`, `<hex HEX`, `<done` and `<fail message` right after a simple query are the messages of `COPY ... FROM STDIN` that go with the query: `CopyData` of the text and a newline, `CopyData` of the bytes, `CopyDone` and `CopyFail`.
    fn script(text: &str) {
        let (mut c, _) = connect();
        let starts = |l: &str| {
            l == "!details"
                || l == "!objects"
                || ["> ", "P ", "X ", "D ", "C "].iter().any(|p| l.starts_with(p))
        };
        let mut details = Details::None;
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            assert!(starts(line), "not a statement: {line}");
            if line == "!details" {
                details = Details::Lines;
                continue;
            }
            if line == "!objects" {
                details = Details::Objects;
                continue;
            }
            let mut statement = line.to_string();
            while let Some(next) = lines.next_if(|l| *l == ">>" || l.starts_with(">> ")) {
                statement.push('\n');
                statement.push_str(next.strip_prefix(">> ").unwrap_or_default());
            }
            let mut copy = Vec::new();
            while let Some(next) = lines.next_if(|l| l.starts_with('<')) {
                copy.push(next);
            }
            let mut want = Vec::new();
            while let Some(next) = lines.next_if(|l| !starts(l)) {
                want.push(next);
            }
            assert_eq!(script_copy(&mut c, &statement, &copy, details), want, "{statement}");
        }
    }

    /// The messages of `COPY ... FROM STDIN` of the lines `<` of a script.
    fn copy_input(lines: &[&str]) -> Vec<u8> {
        let mut input = Vec::new();
        for line in lines {
            input.extend(if *line == "<done" {
                message(b'c', b"")
            } else if let Some(text) = line.strip_prefix("<fail ") {
                message(b'f', &[text.as_bytes(), b"\0"].concat())
            } else if let Some(hex) = line.strip_prefix("<hex ") {
                let bytes: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
                    .collect();
                message(b'd', &bytes)
            } else {
                let text = line.strip_prefix("< ").unwrap_or_default();
                message(b'd', &[text.as_bytes(), b"\n"].concat())
            });
        }
        input
    }

    /// The lines of a statement of a script. A line `> sql` is a simple query. A line `P name|sql` is `Parse` of a statement and `Sync`, and a line `X name|value|value` is `Bind` of the statement to the unnamed portal with values in the text format, `Execute` and `Sync`. A line `D S name` or `D P name` is `Describe` of a statement or a portal and `Sync`, and a line `C S name` or `C P name` is `Close` and `Sync`.
    fn script_lines(c: &mut Connection, line: &str) -> Vec<String> {
        script_details(c, line, Details::None)
    }

    /// [`script_lines`], with the lines of `details` of [`render_details`].
    fn script_details(c: &mut Connection, line: &str, details: Details) -> Vec<String> {
        script_copy(c, line, &[], details)
    }

    /// [`script_details`], with the messages of `COPY ... FROM STDIN` of the lines `copy` after a simple query.
    fn script_copy(c: &mut Connection, line: &str, copy: &[&str], details: Details) -> Vec<String> {
        let target = |rest: &str| {
            let (target, name) = rest.split_once(' ').unwrap();
            (target.chars().next().unwrap(), name.to_string())
        };
        let input = match line.split_at(2) {
            ("D ", rest) => {
                let (target, name) = target(rest);
                [describe(target, &name), sync()].concat()
            }
            ("C ", rest) => {
                let (target, name) = target(rest);
                [close(target, &name), sync()].concat()
            }
            ("P ", rest) => {
                let (name, sql) = rest.split_once('|').unwrap();
                [parse(name, sql, &[]), sync()].concat()
            }
            ("X ", rest) => {
                let mut parts = rest.split('|');
                let name = parts.next().unwrap();
                let values: Vec<Option<&[u8]>> = parts.map(|v| Some(v.as_bytes())).collect();
                [bind("", name, &[], &values, &[]), execute("", 0), sync()].concat()
            }
            (_, sql) => [query(sql), copy_input(copy)].concat(),
        };
        render_details(&exchange(c, &input), true, details)
            .into_iter()
            .filter(|l| !l.starts_with("ready ") && l != "ParseComplete" && l != "BindComplete")
            .map(|l| match l.strip_prefix("fields ") {
                Some(fields) => format!("fields {}", table_names(c, fields)),
                None if l == "CloseComplete" => "close".to_string(),
                None if l == "NoData" => "nodata".to_string(),
                None => l,
            })
            .flat_map(|l| l.split('\n').map(str::to_string).collect::<Vec<_>>())
            .collect()
    }

    /// Runs a script on two sessions of one server, as [`script`] does. A line `A> ` or `B> ` is a simple query of the session `A` or `B`.
    fn two_sessions(text: &str) {
        let store = Arc::new(Store::new());
        let (mut a, _) = connect_to(&store, 1234);
        let (mut b, _) = connect_to(&store, 1235);
        let starts = |l: &str| l.starts_with("A> ") || l.starts_with("B> ");
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let (c, sql) = match line.split_at_checked(3) {
                Some(("A> ", sql)) => (&mut a, sql),
                Some(("B> ", sql)) => (&mut b, sql),
                _ => panic!("not a statement: {line}"),
            };
            let mut want = Vec::new();
            while let Some(next) = lines.next_if(|l| !starts(l)) {
                want.push(next);
            }
            assert_eq!(script_lines(c, &format!("> {sql}")), want, "{line}");
        }
    }

    /// The columns of a line `fields`, with the OID of each table replaced by its `regclass` text on the connection.
    fn table_names(c: &mut Connection, fields: &str) -> String {
        let parts: Vec<String> = fields
            .split(',')
            .map(|part| {
                let Some((name, rest)) = part.split_once('@') else { return part.to_string() };
                let (table, rest) = rest.split_once('.').unwrap();
                let lines = send(c, &query(&format!("SELECT {table}::regclass")));
                let row = lines.iter().find_map(|l| l.strip_prefix("row ")).unwrap();
                format!("{name}@{row}.{rest}")
            })
            .collect();
        parts.join(",")
    }

    /// The names of the user objects in the OID alias types, `format_type` and the visibility functions. `connection/reg_names.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn reg_names() {
        big_stack(|| script(include_str!("connection/reg_names.test")));
    }

    /// A function in PL/pgSQL in the expression or the predicate of a unique index reads a table, and one in an exception block rolls back its subtransaction while the write of `CREATE INDEX` goes on. The function runs while the heap is not locked. `connection/index_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn index_functions() {
        big_stack(|| script(include_str!("connection/index_functions.test")));
    }

    /// The implicit cast of `int2` to `oid` and to the OID alias types, whose function `int4(smallint)` gives an `int4`. `connection/int2_oid_cast.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn int2_oid_cast() {
        big_stack(|| script(include_str!("connection/int2_oid_cast.test")));
    }

    /// The names of the object of an error, in the fields `s`, `t`, `c`, `d` and `n`, for the errors of a constraint, of a domain and of a partition, and in `RAISE` and `GET STACKED DIAGNOSTICS` of PL/pgSQL. `connection/error_objects.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn error_objects() {
        big_stack(|| script(include_str!("connection/error_objects.test")));
    }

    /// The extension `plpgsql` that `initdb` makes: its rows in `pg_extension`, `pg_language`, `pg_proc`, `pg_depend` and `pg_description`, and the queries of `\dx` and `\dL` of psql. `connection/extensions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn extensions() {
        big_stack(|| script(include_str!("connection/extensions.test")));
    }

    /// The deparse functions `pg_get_expr`, `pg_get_constraintdef` and `pg_get_indexdef`, also for the indexes and the constraints of the system catalogs. `connection/deparse.test` is the output of PostgreSQL 19 for the same script in a new database. A value with a newline takes more than one line.
    #[test]
    fn deparse() {
        big_stack(|| script(include_str!("connection/deparse.test")));
    }

    /// `pg_get_viewdef` for the views of the user and for the system views: joins, subqueries, set operations, aggregates, the special syntax of the SQL standard, the wrap column and the names of a view in text. `connection/viewdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn viewdef() {
        big_stack(|| script(include_str!("connection/viewdef.test")));
    }

    /// The rules of the time zone data: `TimeZone` with the names of the data, POSIX zone strings and the error for leap seconds, the output and the input of `timestamptz` in zones with daylight saving time, in a gap and in an overlap, the casts from `timestamp` and `date`, and the views `pg_timezone_names` and `pg_timezone_abbrevs` for zones without a change now. `connection/timezones.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn timezones() {
        big_stack(|| script(include_str!("connection/timezones.test")));
    }

    /// `pg_prepared_statements` for the statements of `Parse`: the types of the parameters and of the columns, the statements that give no rows, the empty query, a name that is longer than 63 bytes, and the count of the generic and the custom plans in each mode of `plan_cache_mode`. A `Bind` that fails does not count, and `DISCARD ALL` drops the statements. `connection/prepared_statements.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn prepared_statements() {
        big_stack(|| script(include_str!("connection/prepared_statements.test")));
    }

    /// `EXPLAIN`: the options and their errors, the text, JSON, XML and YAML forms, the costs of the scans, the joins, the aggregates, the windows, the set operations, the sorts, `LIMIT`, the subplans and the init plans of subqueries, the statements that write a table, and `VERBOSE` with the outputs of the nodes. `connection/explain.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain() {
        big_stack(|| script(include_str!("connection/explain.test")));
    }

    /// `EXPLAIN` of the queries on a table with a primary key, whose plans do not use the index. `connection/explain_keys.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_keys() {
        big_stack(|| script(include_str!("connection/explain_keys.test")));
    }

    /// Cursors: `DECLARE` with each option, `FETCH` and `MOVE` in each direction on cursors that can and cannot scan backward, `CLOSE` and `CLOSE ALL`, the view `pg_cursors`, a cursor `WITH HOLD` after a commit and after a rollback, the errors of the statements and of the grammar, `Describe` of a statement of `FETCH` and of the portal of a cursor, and `Close` of the portal. `connection/cursors.test` is the output of PostgreSQL 19 for the same script in a new database, without the rows of a `BINARY` cursor and without a query that fails after the first row.
    #[test]
    fn cursors() {
        big_stack(|| script(include_str!("connection/cursors.test")));
    }

    /// `DROP TABLE`, `DROP VIEW`, `DROP INDEX`, `DROP SEQUENCE` and `DROP SCHEMA`: the errors of the lookup and of the checks, the objects that depend on others with `RESTRICT` and with `CASCADE` in the detail of the error and of the notice, the names with and without the schema, the rows of the catalogs after the removal and a removal in a block that rolls back. `connection/drop.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn drop_objects() {
        big_stack(|| script(include_str!("connection/drop.test")));
    }

    /// The internal triggers of the foreign keys in `pg_trigger`: the function of each action, the triggers that can wait to the end of the transaction, the names from the OIDs, the rows of `pg_depend`, and the removal of the triggers with their constraint, with `CASCADE` and in a block that rolls back. `connection/fk_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn foreign_key_triggers() {
        big_stack(|| script(include_str!("connection/fk_triggers.test")));
    }

    /// `DROP SCHEMA public`: the schema of `initdb` is not pinned, so `DROP` removes it, its objects and its row of `pg_init_privs`, and `CREATE SCHEMA public` makes a new schema with no row in `pg_init_privs`. `connection/drop_public.test` is the output of PostgreSQL 19 for the same statements.
    #[test]
    fn drop_public() {
        big_stack(|| script(include_str!("connection/drop_public.test")));
    }

    /// `pg_get_function_sqlbody` of functions with a `RETURN` body, with a `BEGIN ATOMIC` body, with a body in a string, and of the built-in `ts_debug(text)`. `connection/function_sqlbody.test` is the output of PostgreSQL 19 for the same statements.
    #[test]
    fn function_sqlbody() {
        big_stack(|| script(include_str!("connection/function_sqlbody.test")));
    }

    /// `pg_get_functiondef` of the built-in functions with a body in SQL, and of functions with a `RETURN` body and a `BEGIN ATOMIC` body. `connection/functiondef_sqlbody.test` is the output of PostgreSQL 19 for the same statements.
    #[test]
    fn functiondef_sqlbody() {
        big_stack(|| script(include_str!("connection/functiondef_sqlbody.test")));
    }

    /// The calls of the built-in functions that `system_functions.sql` gives a body in SQL: the engine runs the body of each one that has no kernel. The rows of `pg_proc` have an empty `prosrc` and a `prosqlbody`. `connection/builtin_bodies.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn builtin_sql_bodies() {
        big_stack(|| script(include_str!("connection/builtin_bodies.test")));
    }

    /// The arithmetic of the date and time types: the operators of `interval`, an interval added to and subtracted from a timestamp, a `timestamptz` in a zone with daylight saving time, a time, a `timetz` and a date, the difference of two timestamps, `age`, the `justify` functions, a date with a time and `OVERLAPS`, also with the infinities and the errors of the range. `connection/datetime_arith.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn datetime_arith() {
        big_stack(|| script(include_str!("connection/datetime_arith.test")));
    }

    /// `EXTRACT` and `date_part` for a timestamp, a `timestamptz` in two zones, a date, a time, a `timetz` and an interval: each unit, the short names of the units, the years before 1 AD, the ISO weeks, the infinities and the errors for a unit that a type does not support or that is not a unit. Also `date_trunc` for a timestamp, a `timestamptz` across the changes of daylight saving time and in a zone with an offset of seconds, and an interval, and `date_bin` with its errors. `connection/extract.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn extract() {
        big_stack(|| script(include_str!("connection/extract.test")));
    }

    /// `AT TIME ZONE` and `AT LOCAL` for a timestamp, a `timestamptz` and a `timetz`, with a zone of the data, an abbreviation with one offset, an abbreviation whose offset changed, a POSIX zone, an abbreviation of the zone of the session and an interval, with the changes of daylight saving time, the ends of the range and the errors. Also `date_trunc` in a zone that a name gives. `connection/at_time_zone.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn at_time_zone() {
        big_stack(|| script(include_str!("connection/at_time_zone.test")));
    }

    /// `sqrt`, `exp`, `ln`, `log` and `power` of `numeric` and the operator `^`, with the scales that PostgreSQL picks, long arguments, the results near the limits of the format, `NaN`, the infinities and the errors. `connection/numeric_math.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn numeric_math() {
        big_stack(|| script(include_str!("connection/numeric_math.test")));
    }

    /// `div`, `gcd`, `lcm`, `factorial`, `scale`, `min_scale`, `trim_scale`, `numeric_larger`, `numeric_smaller`, `numeric_uplus`, `numeric_uminus` and `width_bucket` of `numeric`, with `NaN`, the infinities and the errors. `connection/numeric_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn numeric_functions() {
        big_stack(|| script(include_str!("connection/numeric_functions.test")));
    }

    /// `make_date`, `make_time`, `make_timestamp` and `make_timestamptz` with the years BC, the ends of the range, the zones of the seventh argument and the errors with their messages. Also `isfinite`, the cast of a time to a `timetz`, `timeofday`, `to_timestamp` of a number of seconds, and `generate_series` of the timestamps in the zone of the session and in a named zone, and of `numeric`, with the errors of the step. `connection/calendar.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn calendar() {
        big_stack(|| script(include_str!("connection/calendar.test")));
    }

    /// The comparisons of a date, a timestamp and a `timestamptz` with each other, with the infinities and the values that are out of the range of the other type. Also the support functions `date_cmp`, `time_cmp`, `timetz_cmp`, `interval_cmp`, `numeric_cmp`, `bpcharcmp`, `byteacmp` and `uuid_cmp`, the operators of the pattern classes, and `date_add` and `date_subtract` in a named zone. `connection/datetime_compare.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn datetime_compare() {
        big_stack(|| script(include_str!("connection/datetime_compare.test")));
    }

    /// `setseed` and `random` of each type, `random_normal`, `gen_random_uuid`, `uuidv7`, `uuid_extract_version`, `uuid_extract_timestamp` and the casts between `uuid` and `bytea`. The numbers after `setseed` are those of PostgreSQL. `connection/random.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn random() {
        big_stack(|| script(include_str!("connection/random.test")));
    }

    /// `array_position`, `array_positions`, `array_fill`, `array_reverse`, `array_sort`, `trim_array`, `array_shuffle`, `array_sample`, the operators `@>`, `<@` and `&&`, and `width_bucket` with an array of thresholds, with their errors. `connection/array_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_more_functions() {
        big_stack(|| script(include_str!("connection/array_more.test")));
    }

    /// The trigonometric functions in radians and in degrees, `degrees`, `radians`, the hyperbolic functions, `erf`, `erfc`, `gamma`, `lgamma` and `width_bucket` of `float8`, with the special values and the errors. `connection/trig.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the C library of this platform can give a different last bit than glibc, see Q20 (#33)"
    )]
    fn float_math() {
        big_stack(|| script(include_str!("connection/trig.test")));
    }

    /// The functions and the operators of `bytea`: `||`, `substring`, `overlay`, `position`, `get_byte`, `set_bit` and the other functions of the bytes and the bits, `reverse`, the casts to and from the integer types, `LIKE` with `ESCAPE`, `trim`, `md5`, the SHA-2 hashes, `crc32` and `crc32c`, with the errors. `connection/bytea_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn bytea_functions() {
        big_stack(|| script(include_str!("connection/bytea_functions.test")));
    }

    /// `translate`, `overlay` of `text`, `to_bin`, `to_oct`, `unistr` with its escapes and surrogate pairs, `casefold`, and `gcd` and `lcm` of the integer types, with the errors. `connection/text_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn text_more_functions() {
        big_stack(|| script(include_str!("connection/text_more.test")));
    }

    /// The regression aggregates `regr_sxx`, `regr_slope`, `regr_intercept`, `covar_pop`, `corr` and the others, with constant inputs, `Infinity`, `NaN`, values near the limits of `float8` and the overflow errors, and their transition, combine and final functions as functions. `connection/regression_aggregates.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn regression_aggregates() {
        big_stack(|| script(include_str!("connection/regression_aggregates.test")));
    }

    /// `var_pop`, `var_samp`, `stddev_pop`, `stddev_samp`, `variance` and `stddev` of `smallint`, `integer`, `bigint` and `numeric`, with no rows, one row, equal values, `NaN`, the infinities, the limits of the integer types, sums that do not fit in 128 bits and the scales of `numeric`. `connection/variance_aggregates.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn variance_aggregates() {
        big_stack(|| script(include_str!("connection/variance_aggregates.test")));
    }

    /// The operators `->`, `->>`, `#>` and `#>>` of `json` and `jsonb`, the `extract_path` functions, `typeof`, `array_length`, `strip_nulls`, `jsonb_pretty`, the casts of `jsonb`, `?`, `?|`, `?&`, `@>`, `<@` and the order of `jsonb`. The script has the keys that come twice, the negative indexes, the paths with a null element, the escapes that `json` takes and the operators refuse, and the white space around a scalar at the root. `connection/json_access.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn json_access() {
        big_stack(|| script(include_str!("connection/json_access.test")));
    }

    /// `||`, `-` and `#-` of `jsonb`, `jsonb_set`, `jsonb_insert`, `jsonb_set_lax`, and the set functions `each`, `each_text`, `object_keys`, `array_elements` and `array_elements_text` of `json` and `jsonb`. The script has the indexes before the first element and after the last one, the paths with a null element or a step that is not an integer, a scalar at the root, and the keys that come twice in a `json` object. `connection/json_modify.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn json_modify() {
        big_stack(|| script(include_str!("connection/json_modify.test")));
    }

    /// `to_json`, `to_jsonb`, `array_to_json`, `row_to_json`, `json_build_object`, `json_build_array` and `json_object`, and the same functions of `jsonb`. The script has the numbers that are not finite, the dates in the XSD form, the arrays of two dimensions, the pretty form, the names of the fields of a subquery, a `VALUES` list with an alias and a table with an alias, the keys that are not scalars, the null keys and the arrays of the wrong shape. `connection/json_build.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn json_build() {
        big_stack(|| script(include_str!("connection/json_build.test")));
    }

    /// `json_agg`, `jsonb_agg`, `json_object_agg`, `jsonb_object_agg` and their `strict` and `unique` forms. The script has the null values and the null keys, the groups with no rows, the records and the arrays that `json_agg` puts on new lines, the keys that come twice, `ORDER BY`, `DISTINCT` and `FILTER`. `connection/json_agg.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn json_agg() {
        big_stack(|| script(include_str!("connection/json_agg.test")));
    }

    /// `normalize`, `IS NORMALIZED`, `is_normalized`, `unicode_assigned` and `unicode_version`, with `num_nulls`, `num_nonnulls`, `error_on_null`, `oidvectortypes`, `current_query` and `system_user`. The script has the four forms, the Hangul syllables, the code points that Unicode does not assign, the `VARIADIC` calls with a null array and with an array of two dimensions, the types that do not exist, a message of two statements and a view. `connection/normalize.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn normalize() {
        big_stack(|| script(include_str!("connection/normalize.test")));
    }

    /// `to_char` of a timestamp, a `timestamptz` and an interval: each pattern of `formatting.c` for these types, the `FM`, `TM`, `TH`, `th` and `SP` affixes, the quoted strings and the backslashes, the years before Christ, the zones with an offset that is not a whole hour, a date and a time that the call casts, and the patterns that are not valid for an interval. `connection/to_char.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn to_char() {
        big_stack(|| script(include_str!("connection/to_char.test")));
    }

    /// `to_timestamp` and `to_date`: each pattern of `formatting.c` in the input, the `FM` and `FX` modes, the spaces and the separators that the input skips, the zone offsets and the abbreviations, the years of fewer than 4 digits, the centuries, the Julian days, the ISO weeks, the days of the year, the fractions of the second and the errors of each check. `connection/to_timestamp.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn to_timestamp() {
        big_stack(|| script(include_str!("connection/to_timestamp.test")));
    }

    /// `to_char` of `int2`, `int4`, `int8`, `float4`, `float8` and `numeric`, and `to_number`: each number pattern of `formatting.c`, the `FM` mode, the signs of `S`, `MI`, `PL`, `SG` and `PR`, the Roman numerals, `EEEE`, `V`, the ordinal suffixes, the overflows, `NaN` and the infinities, the quoted text, and the errors of the format and of the input. `connection/to_number.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn to_number() {
        big_stack(|| script(include_str!("connection/to_number.test")));
    }

    /// The functions that `\d` of psql calls for a table: `pg_partition_tree`, `pg_partition_ancestors` and `pg_partition_root` for relations that are not in a partition tree, `pg_relation_is_publishable`, and the query of psql for the foreign keys that reference a table. `connection/describe.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn describe_functions() {
        big_stack(|| script(include_str!("connection/describe.test")));
    }

    /// `pg_get_serial_sequence` for serial and bigserial columns, quoted and qualified names, `search_path`, a system column, a column of a catalog and of a sequence, and the errors for a column, a relation and a schema that do not exist. `connection/serial_sequence.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn serial_sequence() {
        big_stack(|| script(include_str!("connection/serial_sequence.test")));
    }

    /// The identity columns: `GENERATED ALWAYS` and `GENERATED BY DEFAULT`, the options of the sequence, `OVERRIDING SYSTEM VALUE` and `OVERRIDING USER VALUE`, `UPDATE` to a value and to `DEFAULT`, the catalogs, `information_schema.columns`, the dependency of the sequence, and the errors of `CREATE TABLE`. `connection/identity.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn identity() {
        big_stack(|| script(include_str!("connection/identity.test")));
    }

    /// The identity columns with `NULL` and `NOT NULL`, `SEQUENCE NAME` in another schema and with a name in use, a domain type, `ON CONFLICT`, `MERGE` with `OVERRIDING`, `ALTER COLUMN ... SET DEFAULT` and a prepared `INSERT`. `connection/identity_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn identity_more() {
        big_stack(|| script(include_str!("connection/identity_more.test")));
    }

    /// `ALTER TABLE ... ALTER COLUMN` with `ADD GENERATED ... AS IDENTITY`, `SET GENERATED`, the `SET` options and `RESTART` of the sequence, and `DROP IDENTITY` with and without `IF EXISTS`, their errors, and a rollback of `DROP IDENTITY`. `connection/alter_identity.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_identity() {
        big_stack(|| script(include_str!("connection/alter_identity.test")));
    }

    /// The order of the identity subcommands in one `ALTER TABLE`, a column with a default, a view, an index and a sequence, `SEQUENCE NAME` in another schema and with a name in use, and the system columns. `connection/alter_identity_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_identity_more() {
        big_stack(|| script(include_str!("connection/alter_identity_more.test")));
    }

    /// `ALTER TABLE ... ALTER COLUMN ... TYPE` with and without `USING`, the rows with the new values, a default, a check constraint, `COLLATE`, an index and a unique index, a view, and the errors of the checks. `connection/alter_column_type.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_column_type() {
        big_stack(|| script(include_str!("connection/alter_column_type.test")));
    }

    /// The objects on a column that `ALTER COLUMN ... TYPE` changes: a default that cannot get the new type, the errors of `USING`, the sequence of a `serial` column and of an identity column, a foreign key on both sides, an index with an operator class and with a collation, a domain, several subcommands, and a rollback. `connection/alter_column_type_dependents.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_column_type_dependents() {
        big_stack(|| script(include_str!("connection/alter_column_type_dependents.test")));
    }

    /// The order of the checks of `ALTER COLUMN ... TYPE`, a column that two subcommands change, the collation, a pseudo-type, the row type of the table, an array, the check constraints that it parses again in the order of their OIDs, and the indexes that keep their OIDs. `connection/alter_column_type_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_column_type_checks() {
        big_stack(|| script(include_str!("connection/alter_column_type_checks.test")));
    }

    /// `DROP CONSTRAINT` with `IF EXISTS` and `CASCADE`, the objects that depend on a key, a not-null constraint of a key or an identity column, `DROP NOT NULL`, and the pass of the drops before the new constraints. `connection/alter_drop_constraint.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_drop_constraint() {
        big_stack(|| script(include_str!("connection/alter_drop_constraint.test")));
    }

    /// `ALTER ... RENAME` of a table, a view, an index, a sequence, a column and a constraint: the row type and the array type of a table, the index of a key, the text of the views that read the renamed objects, the errors of a name that exists, and a rollback. `connection/alter_rename.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_rename() {
        big_stack(|| script(include_str!("connection/alter_rename.test")));
    }

    /// The checks of `ALTER ... RENAME COLUMN` and `RENAME CONSTRAINT` for each kind of relation, the columns of an index, a system catalog, an array type that moves for a new row type, and `ALTER SCHEMA ... RENAME` with the views that read the schema. `connection/alter_rename_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_rename_checks() {
        big_stack(|| script(include_str!("connection/alter_rename_checks.test")));
    }

    /// `ALTER TABLE ... DROP COLUMN` with the indexes, the constraints, the defaults, the sequences and the views that go with the column, `CASCADE`, `IF EXISTS`, the errors, the rows of `pg_attribute` and `information_schema.columns` after the drop, `INSERT`, `COPY`, whole rows and a row type of a dropped column, and a rollback. `connection/alter_drop_column.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_drop_column() {
        big_stack(|| script(include_str!("connection/alter_drop_column.test")));
    }

    /// The columns after `DROP COLUMN`: the column names of an alias, a whole row, a failing row, `ALTER COLUMN ... TYPE` and `ADD COLUMN` that write the rows again, `COPY FROM`, `UPDATE` and `DELETE` with `RETURNING`, a table with no columns, and the errors for a composite type and a dropped column. `connection/alter_drop_column_rows.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_drop_column_rows() {
        big_stack(|| script(include_str!("connection/alter_drop_column_rows.test")));
    }

    /// `SET STATISTICS`, `SET STORAGE`, `SET COMPRESSION` and the `SET` and `RESET` of the options of a column, `STORAGE` and `COMPRESSION` in `CREATE TABLE`, the facts that an index column takes from its table column, and the errors for a type that cannot change, a system column, an index column and a relation of the wrong kind. `connection/alter_column_options.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_column_options() {
        big_stack(|| script(include_str!("connection/alter_column_options.test")));
    }

    /// `WITH (...)` of `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW` and the index of a constraint, and `SET (...)` and `RESET (...)` of `ALTER TABLE`, `ALTER INDEX` and `ALTER VIEW`: the options of a table and of its TOAST table, of a btree index, of a hash index and of a view, the parse of each kind of value, `WITH OIDS`, and the errors for a namespace, a name, a value and a relation of the wrong kind. `connection/relation_options.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn relation_options() {
        big_stack(|| script(include_str!("connection/relation_options.test")));
    }

    /// `CLUSTER ON`, `SET WITHOUT CLUSTER` and `REPLICA IDENTITY` of `ALTER TABLE`: `indisclustered`, `indisreplident` and `relreplident`, the checks of the index, the order of the subcommands, `DROP NOT NULL` on a column of the replica identity index, and the flags after `ALTER COLUMN ... TYPE`, `DROP INDEX` and `DROP COLUMN`. `connection/cluster_replica_identity.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn cluster_replica_identity() {
        big_stack(|| script(include_str!("connection/cluster_replica_identity.test")));
    }

    /// `ALTER TABLE`, `ALTER VIEW` and `ALTER SEQUENCE` with `SET SCHEMA`: the move of the row type, the indexes, the owned sequences and the constraints, the dependency on the schema, the text of the views that read a moved relation, and the errors for the relation, the schema and a name that the new schema has. `connection/set_schema.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn set_schema() {
        big_stack(|| script(include_str!("connection/set_schema.test")));
    }

    /// `ALTER TYPE ... ADD VALUE` and `ALTER TYPE ... RENAME VALUE`: the sort order and the OID of a new label, the new sort orders when `float4` has no middle, the order of the labels with odd OIDs in comparisons, sorts and a unique index, the error for a new label that the transaction uses before the commit, and the errors for the type and the labels. `connection/enum_values.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn enum_values() {
        big_stack(|| script(include_str!("connection/enum_values.test")));
    }

    /// `ALTER TYPE` and `ALTER DOMAIN` with `RENAME TO`, `SET SCHEMA` and `OWNER TO` for an enum type, a composite type, a domain and a range type: the new names of the array types, the move of the relation of a composite type and of the constraints of a domain, the new owner of a multirange type, the new text of a view, and the errors for array types, row types of tables and names that exist. `connection/type_alter.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn type_alter() {
        big_stack(|| script(include_str!("connection/type_alter.test")));
    }

    /// `ALTER TYPE` with `ADD ATTRIBUTE`, `DROP ATTRIBUTE`, `ALTER ATTRIBUTE` and `RENAME ATTRIBUTE` for composite types: the stored values that get a null for a new attribute and lose a dropped one, also in arrays, domains and other composite types, the columns that stop a change of the type of an attribute, and the errors for types that would hold themselves. `connection/type_attribute.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn type_attribute() {
        big_stack(|| script(include_str!("connection/type_attribute.test")));
    }

    /// The dependencies of a function on the objects that its body in SQL reads: a `RETURN` expression with subqueries, a body in `BEGIN ATOMIC`, a cast to a domain, and a relation that the body reads as a whole. A column takes the place of its relation, so `DROP TABLE` and `DROP FUNCTION` find each function. `connection/body_dependencies.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn body_dependencies() {
        big_stack(|| script(include_str!("connection/body_dependencies.test")));
    }

    /// The dependencies of views, defaults, check constraints, indexes, domains and functions on the fields of composite types that they select: `ALTER TYPE ... DROP ATTRIBUTE` and `ALTER TABLE ... DROP COLUMN` stop or drop these objects, the stored expressions keep the right fields after a drop, a view shows the new name of a field, and `ALTER TYPE ... ALTER ATTRIBUTE ... TYPE` makes the check constraints again. `connection/field_dependency.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn field_dependency() {
        big_stack(|| script(include_str!("connection/field_dependency.test")));
    }

    /// The order of the rows of `pg_attribute` for a relation, without `ORDER BY`. PostgreSQL reads them by the index on `attrelid` and `attnum`, so the system columns come first from `tableoid` (-6), and the user columns come last. Sequelize reads the columns of an index with `array_agg` in this order. `connection/attribute_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn attribute_order() {
        big_stack(|| script(include_str!("connection/attribute_order.test")));
    }

    /// `pg_describe_object` for the kinds of objects: relations, columns and system columns, types, functions, schemas, constraints, defaults, rules, collations, languages, access methods, operators, operator classes and families, casts, objects of text search, conversions, roles, databases, tablespaces, extensions and the members of operator families. A name has its schema when the search path does not find the object, an object that does not exist gives null, and a catalog that is not a catalog of objects gives an error. `connection/describe_object.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn describe_object() {
        big_stack(|| script(include_str!("connection/describe_object.test")));
    }

    /// `pg_identify_object` for the kinds of objects: the type, the schema, the name and the identity of relations, columns, types, functions, schemas, constraints, defaults, rules, triggers, collations, languages, access methods, operators, operator classes and families and their members, casts, objects of text search, conversions, roles, databases, tablespaces and extensions. The identity always has the schema, and an object that does not exist gives only its type. `connection/identify_object.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn identify_object() {
        big_stack(|| script(include_str!("connection/identify_object.test")));
    }

    /// `CREATE FUNCTION` and `DROP FUNCTION` for functions in SQL: calls in the target list, in `FROM` and in `WHERE`, a set, `OUT` parameters, `RETURNS TABLE`, the rows of `pg_proc`, the errors of a call and of a body, the dependencies of a view, of a type and of a schema, and the errors and notices of `DROP FUNCTION`. `connection/create_function.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn create_function() {
        big_stack(|| script(include_str!("connection/create_function.test")));
    }

    /// `CREATE OR REPLACE FUNCTION`: a new body for a function that exists, which a view sees, new options, new dependencies, and the errors for a new result type, new `OUT` parameters, a new name of an argument and a body that does not fit. `connection/replace_function.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn replace_function() {
        big_stack(|| script(include_str!("connection/replace_function.test")));
    }

    /// The defaults of the arguments of a function in SQL: calls that leave out arguments or name them, `pronargdefaults`, `proargdefaults`, `pg_get_function_arguments`, the errors of a bad default, the checks of `CREATE OR REPLACE FUNCTION` and a default that calls a function. `connection/function_defaults.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn function_defaults() {
        big_stack(|| script(include_str!("connection/function_defaults.test")));
    }

    /// `VARIADIC` arguments of functions in SQL, with calls that give a list of values or an array, `provariadic` and the errors for a bad `VARIADIC` argument. The script also has the checks of `interpret_function_parameter_list` and `fmgr_sql_validator` for the names and types of the arguments and for the result type. `connection/variadic_function.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn variadic_function() {
        big_stack(|| script(include_str!("connection/variadic_function.test")));
    }

    /// `ALTER FUNCTION` and `ALTER ROUTINE` with options, `RENAME TO`, `SET SCHEMA` and `OWNER TO`, and `COMMENT ON FUNCTION`, with the errors of the lookup of the function, of the options and of a name that the schema has. `connection/alter_function.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_function() {
        big_stack(|| script(include_str!("connection/alter_function.test")));
    }

    /// `TRUNCATE` with `RESTART IDENTITY` and `CONTINUE IDENTITY`, a rollback, the foreign keys with `RESTRICT` and `CASCADE`, a table that refers to itself, a view, a sequence, an index, a system catalog and a prepared query. `connection/truncate.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn truncate() {
        big_stack(|| script(include_str!("connection/truncate.test")));
    }

    /// `TRUNCATE` with the sequences that a column owns, `currval` after `RESTART IDENTITY`, the rows of the same transaction, a read-only transaction, the names with a schema, the relations that are not tables, the order of the error of a foreign key, a cascade over two levels and `PREPARE`. `connection/truncate_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn truncate_more() {
        big_stack(|| script(include_str!("connection/truncate_more.test")));
    }

    /// `SAVEPOINT`, `RELEASE SAVEPOINT` and `ROLLBACK TO SAVEPOINT` outside of a block and in an implicit block, the rows of a subtransaction after a rollback and after a release, the names that no savepoint has, an error in a subtransaction, nested savepoints with the same name, a quoted name, `COMMIT` and `ROLLBACK` of a failed subtransaction, and the statements of a failed subtransaction. `connection/savepoint.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn savepoint() {
        big_stack(|| script(include_str!("connection/savepoint.test")));
    }

    /// Savepoints with the settings and their reports, `SET TRANSACTION` in a subtransaction, the catalog, a sequence, the deferred triggers of a foreign key and `SET CONSTRAINTS`, the cursors from before and after a savepoint, `TRUNCATE`, the advisory locks of the transaction level and `PREPARE`. `connection/savepoint_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn savepoint_more() {
        big_stack(|| script(include_str!("connection/savepoint_more.test")));
    }

    /// Functions in PL/pgSQL and `DO`: the variables, the control statements, the queries with `INTO`, `EXECUTE`, `RAISE` with its options, the blocks with `EXCEPTION`, `GET DIAGNOSTICS`, `ASSERT`, the functions that return a set and the context lines of the errors and the messages. `connection/plpgsql.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql() {
        big_stack(|| script(include_str!("connection/plpgsql.test")));
    }

    /// More functions in PL/pgSQL: the `OUT` parameters, `%TYPE` and `%ROWTYPE`, the polymorphic functions, recursion, the labels of the loops, the condition names of the handlers, the writes in a block that rolls back, the assignments to fields and elements, the errors of the compile and `DO` with its errors. `connection/plpgsql_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_more() {
        big_stack(|| script(include_str!("connection/plpgsql_more.test")));
    }

    /// The system columns `xmin`, `cmin`, `xmax` and `cmax` of a table and of a catalog, their types and their errors in a view, a subquery and a check constraint, the partition key functions for a table that is not partitioned, and the recovery functions on a server that is not in recovery. The script does not show the value of `xmin`, which is the ID of a transaction. `connection/system_columns.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn system_columns() {
        big_stack(|| script(include_str!("connection/system_columns.test")));
    }

    /// `encode` and `decode` in the forms `hex`, `base64`, `base64url`, `base32hex` and `escape`: the padding and the new lines of the output, the white space of the input, the end of a value without padding, and the errors of a bad symbol, a bad end and a bad name. `connection/encode.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn encode() {
        big_stack(|| script(include_str!("connection/encode.test")));
    }

    /// The defaults of the arguments of the built-in functions and the calls with named arguments: a call with only defaults, the named arguments after the positional arguments in any order, a view that keeps the names, the text of the defaults, and the errors of a name used twice, a positional argument after a named argument, a name that no function has, a required argument that the call does not give, an aggregate with a named argument, and the errors of `make_interval` and `parse_ident`. `connection/func_args.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn func_args() {
        big_stack(|| script(include_str!("connection/func_args.test")));
    }

    /// `PREPARE`, `EXECUTE` and `DEALLOCATE`: the types of the parameters, the coercion of the values of `EXECUTE` and their errors, the statements of `Parse` that `EXECUTE` runs, the errors in a failed transaction, the count of the plans, a query string with more than one statement, the three statements in a `Parse`, and a statement of `Parse` that runs `EXECUTE` of itself. `connection/sql_prepare.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sql_prepare() {
        big_stack(|| script(include_str!("connection/sql_prepare.test")));
    }

    /// `WITH` and `WITH RECURSIVE`: the names of the columns, a query of `WITH` that a subquery and a set operation read, the order of the queries, the recursive queries with `UNION` and `UNION ALL`, the checks of the form of a recursive query and their errors, and the types of the recursive term. `connection/with_queries.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn with_queries() {
        big_stack(|| script(include_str!("connection/with_queries.test")));
    }

    /// `pg_get_viewdef` of views with `WITH` and `WITH RECURSIVE`: the names of the columns of a query of `WITH`, `MATERIALIZED` and `NOT MATERIALIZED`, an alias of a reference, a quoted name, a `WITH` in a subquery and in a query of `WITH`, a `VALUES` list, the rows of the views, and the rows of `pg_depend` for the relations that the queries of `WITH` read, also in a query of `WITH` that no part of the view reads. `connection/with_viewdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn with_viewdef() {
        big_stack(|| script(include_str!("connection/with_viewdef.test")));
    }

    /// `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result` and `pg_get_function_arg_default`: a hash of the text of all the built-in functions of `pg_proc.dat`, the defaults of the arguments, the ordered-set aggregates, the output arguments, and an OID that is not a function. `connection/function_arguments.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn function_arguments() {
        big_stack(|| script(include_str!("connection/function_arguments.test")));
    }

    /// `pg_get_functiondef`: the built-in functions with options, a support function, `OUT` arguments, a window function and a result of `SETOF record`, a hash of the text of all the built-in functions without a body in SQL, an aggregate, an OID that is not a function, and the functions in SQL and in PL/pgSQL of a session with the options, a body that has the tag `$function$`, a procedure, a name that needs quotes and a temporary function. `connection/functiondef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn functiondef() {
        big_stack(|| script(include_str!("connection/functiondef.test")));
    }

    /// The view `pg_sequences` for the sequences of `serial` columns, `pg_sequence_last_value` with its errors, `pg_my_temp_schema`, `pg_is_other_temp_schema` and the view `pg_seclabels`. `connection/sequences.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sequences() {
        big_stack(|| script(include_str!("connection/sequences.test")));
    }

    /// `money`, `xid8`, `oid8`, `tid` and `refcursor`: the text forms with the errors of a bad value, the rounding of `money`, the order of the unsigned types, the operators of `money`, the casts, `cash_words`, the aggregates, the arrays and the text and binary forms of `COPY`. `connection/money.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn money() {
        big_stack(|| script(include_str!("connection/money.test")));
    }

    /// `bit` and `bit varying`: the constants, the text forms with the errors of a bad digit or length, the length casts, the casts to and from the integer types, the operators, the functions, the aggregates, the order, the arrays and the text and binary forms of `COPY`. `connection/bit.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn bit() {
        big_stack(|| script(include_str!("connection/bit.test")));
    }

    /// The operators and functions of `path`, `polygon` and `circle`: the winding test of a point in a polygon, the containment and the overlap of polygons, the distances, the casts between the types and the errors of `polygon(integer, circle)`. `connection/geofn2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the C library of this platform can give a different last bit than glibc, see Q20 (#33)"
    )]
    fn geofn2() {
        big_stack(|| script(include_str!("connection/geofn2.test")));
    }

    /// The operators and functions of `point`, `lseg`, `line` and `box`: the comparisons with the tolerance of `EPSILON`, NaN, the arithmetic with the errors of overflow, underflow and division by zero, the distances, the closest points and the intersections. `connection/geofn.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn geofn() {
        big_stack(|| script(include_str!("connection/geofn.test")));
    }

    /// `xid`, `cid`, `pg_lsn`, `pg_snapshot` and `txid_snapshot`: the text forms with the errors of a bad value, the order and the arithmetic of `pg_lsn`, the functions of the snapshots, the soft errors, the types with no input, the arrays and the text and binary forms of `COPY`. `connection/snap.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn snap() {
        big_stack(|| script(include_str!("connection/snap.test")));
    }

    /// `aclitem`: the text forms with the errors and the warning of a bad item, the soft errors, `makeaclitem`, `aclcontains`, `aclexplode`, the hash functions, the arrays, the text form of `COPY` and the error of the binary form. A warning of the input of a literal points at the literal, in a simple query and in `Parse`. `connection/acl.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn acl() {
        big_stack(|| script(include_str!("connection/acl.test")));
    }

    /// The geometric types `point`, `lseg`, `box`, `path`, `polygon`, `line` and `circle`: the text forms of each format with the errors of a bad value, `extra_float_digits`, the arrays, the soft errors and the text and binary forms of `COPY`. `connection/geo.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn geo() {
        big_stack(|| script(include_str!("connection/geo.test")));
    }

    /// `macaddr` and `macaddr8`: the text forms of each format with the errors of a bad value, the order, `trunc`, the bit operators, `macaddr8_set7bit`, the casts, the arrays and the text and binary forms of `COPY`. `connection/mac.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn mac() {
        big_stack(|| script(include_str!("connection/mac.test")));
    }

    /// The range types: the text forms with the canonical form of `int4range`, `int8range` and `daterange`, the errors of a bad literal and of a lower bound above the upper bound, the soft errors, the constructors, the functions of the bounds and the flags, the order, the arrays of ranges, a parameter and the text and binary forms of `COPY`. `connection/range.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn range() {
        big_stack(|| script(include_str!("connection/range.test")));
    }

    /// The operators of the ranges: overlap, containment of a range and of a value, the position operators, adjacency, union, intersection and difference with the errors of a result that is not contiguous, `range_merge`, `range_minus_multi`, the subtype difference functions and the operators in a query with a parameter. `connection/range_ops.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn range_ops() {
        big_stack(|| script(include_str!("connection/range_ops.test")));
    }

    /// The multirange types: the text forms with the merge of the ranges on input and the errors of a bad literal, the constructors with a `VARIADIC` array, the cast from a range, the functions that read the bounds and the flags, `range_merge`, `unnest`, the order, `DISTINCT` and `GROUP BY`, the soft errors, `COPY` in text and binary and a parameter. `connection/multirange.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn multirange() {
        big_stack(|| script(include_str!("connection/multirange.test")));
    }

    /// `range_agg` and `range_intersect_agg` of ranges and of multiranges: null inputs, no input, empty ranges and empty multiranges, `GROUP BY`, `ORDER BY`, `DISTINCT` and `FILTER` in the call, the errors of a type that is not a range, and a query with parameters. `connection/range_agg.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn range_agg() {
        big_stack(|| script(include_str!("connection/range_agg.test")));
    }

    /// The operators of the multiranges: union, difference and intersection, `multirange_minus_multi`, overlap, containment of a multirange, a range and a value, the position operators and adjacency with a multirange or a range on each side, the errors of the types that do not match, and a query with parameters. `connection/multirange_ops.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn multirange_ops() {
        big_stack(|| script(include_str!("connection/multirange_ops.test")));
    }

    /// The type `tsvector`: the text input with quotes, escapes, positions and weights and its errors, the output, the order, `length`, `strip`, `setweight`, `ts_delete`, `tsvector_to_array`, `array_to_tsvector`, `ts_filter`, `unnest`, the operator `||`, the limits of the positions and of a lexeme, arrays of `tsvector`, `GROUP BY` and `DISTINCT`, the text and binary forms of `COPY` and a query with parameters. `connection/tsvector.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn tsvector() {
        big_stack(|| script(include_str!("connection/tsvector.test")));
    }

    /// The Snowball dictionaries and the text search DDL: `ts_lexize` with the built-in and new dictionaries, the rows of `pg_ts_template`, `pg_ts_dict`, `pg_ts_config` and `pg_ts_config_map`, the casts to `regdictionary` and `regconfig`, `CREATE TEXT SEARCH TEMPLATE`, `DICTIONARY` and `CONFIGURATION`, `ALTER TEXT SEARCH CONFIGURATION` with `ADD`, `ALTER` and `DROP MAPPING`, `COMMENT`, the visibility functions, the dependencies and the errors. `connection/ts_dict.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_dict() {
        big_stack(|| script(include_str!("connection/ts_dict.test")));
    }

    /// The default text search parser and `to_tsvector`: the tokens of `ts_parse` for each token type, `ts_token_type`, `to_tsvector` with the built-in and new configurations, stop words, repeated words, the notice for a long word, `get_current_ts_config`, `text @@ tsquery`, the check of `default_text_search_config` in `SET` and `set_config`, and the errors. `connection/ts_parse.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_parse() {
        big_stack(|| script(include_str!("connection/ts_parse.test")));
    }

    /// The functions that make a query with a configuration: `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery` and `websearch_to_tsquery`, with weights, prefixes, stop words and the distances of `<->` around them, the syntax of a web search with `-`, `or` and text in double quotes, the notices for an empty query, `text @@ text`, and the errors. `connection/ts_query.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_query() {
        big_stack(|| script(include_str!("connection/ts_query.test")));
    }

    /// The `json` and `jsonb` forms of `to_tsvector`, `json_to_tsvector` and `jsonb_to_tsvector`: the positions of the strings, the filter of strings, numbers, booleans and keys, the errors of the filter, and the duplicate keys of `json`. `connection/ts_json.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_json() {
        big_stack(|| script(include_str!("connection/ts_json.test")));
    }

    /// The functions `ts_rank` and `ts_rank_cd` with all forms of arguments: the operators of the query, weights, prefixes, the methods of normalization, an array of weights and its errors, and a document with no positions. `connection/ts_rank.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_rank() {
        big_stack(|| script(include_str!("connection/ts_rank.test")));
    }

    /// The function `ts_rewrite` with a target and a substitute: the operators `&`, `|`, `!` and `<N>`, a target that matches a part of the children of `&` or `|`, weights and prefixes, an empty query, target or substitute, and an operator that has no children after the rewrite. `connection/ts_rewrite.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_rewrite() {
        big_stack(|| script(include_str!("connection/ts_rewrite.test")));
    }

    /// The function `ts_headline` with and without a configuration and options: the selection of the best part of the text, the fragments of `MaxFragments`, `HighlightAll` with tags, phrases, prefixes, `!`, an empty query and an empty text, the options `StartSel`, `StopSel` and `FragmentDelimiter`, and the errors of the options. `connection/ts_headline.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_headline() {
        big_stack(|| script(include_str!("connection/ts_headline.test")));
    }

    /// The function `ts_headline` for `json` and `jsonb`: the headline of each string that is not a key, the text of the other scalars, a scalar at the root, duplicate keys, escapes, and the errors of the options, which come only for a value with a string. `connection/ts_headline_json.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_headline_json() {
        big_stack(|| script(include_str!("connection/ts_headline_json.test")));
    }

    /// The send functions of the base types and of their arrays called from SQL, which give the binary form of each type: the special values of the numbers and the times, multidimensional arrays with lower bounds and null elements, empty ranges and ranges with no bounds, `jsonb`, `tsvector`, `tsquery`, `jsonpath`, the network and geometric types, the limits of the integer types, and `record_send`. `connection/type_send.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn type_send() {
        big_stack(|| script(include_str!("connection/type_send.test")));
    }

    /// The text input and output functions of the types called from SQL: the typmod of the third argument of an input function, the errors of the input, null values, and the result of type `cstring`. `connection/type_in_out.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn type_in_out() {
        big_stack(|| script(include_str!("connection/type_in_out.test")));
    }

    /// The function `ts_stat`: the counts of the lexemes and of their positions, the weights, the order of the rows, null values, the errors of a query that does not give one `tsvector` column, an empty text, two statements, a statement that gives no rows and a statement that is not read-only, and the errors of the query with and without a position. `connection/ts_stat.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_stat() {
        big_stack(|| script(include_str!("connection/ts_stat.test")));
    }

    /// The function `ts_rewrite` with a query: the rewrite of each row from the result of the row before it, the rows with a null value or an empty target, the end of the rewrite at an empty query, an empty query that does not run the query, and the errors of a query that does not give two `tsquery` columns. `connection/ts_rewrite_query.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_rewrite_query() {
        big_stack(|| script(include_str!("connection/ts_rewrite_query.test")));
    }

    /// The type `tsquery`: the text input with its operators, weights and prefixes and its errors, the notice for a query with no lexemes, the output, the order, `numnode`, `querytree`, `tsquery_phrase`, the operators `&&`, `||`, `!!`, `<->`, `@>` and `<@`, the limits of the distance, the lexeme and the stack of operators, arrays of `tsquery`, `GROUP BY`, `pg_input_is_valid`, the text and binary forms of `COPY` and a query with parameters. `connection/tsquery.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn tsquery() {
        big_stack(|| script(include_str!("connection/tsquery.test")));
    }

    /// The type `jsonpath`: the text input with its keys, variables, literals, operators, filters, subscripts and methods, the escapes and the comments, the errors of the scanner and of the parser at the same token as PostgreSQL, the checks of `@` and `last`, the output, arrays of `jsonpath`, `pg_input_is_valid`, `pg_input_error_info`, the text and binary forms of `COPY` and a query with parameters. `connection/jsonpath.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn jsonpath() {
        big_stack(|| script(include_str!("connection/jsonpath.test")));
    }

    /// The type `xml`: the check of a well formed value in content and in document mode with the errors and the warnings of libxml2, the XML declaration, `DOCTYPE` and entities, the limit of depth, `xmloption`, the output, the casts, arrays of `xml`, `pg_input_is_valid`, `pg_input_error_info`, the text and binary forms of `COPY` and a query with parameters. `connection/xml.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn xml() {
        big_stack(|| script(include_str!("connection/xml.test")));
    }

    /// The operators `@@` and `@@@` of `tsvector` and `tsquery`: the operators `&`, `|` and `!`, weights, prefixes, a vector with no positions, `<->` and `<N>` with `!`, `&` and `|` under them, the largest distance, the bits of a position that PostgreSQL keeps, `GROUP BY` and a query with parameters. `connection/ts_match.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_match() {
        big_stack(|| script(include_str!("connection/ts_match.test")));
    }

    /// `inet` and `cidr`: the text forms of IPv4 and IPv6 with the errors of a bad value, the class of a `cidr` with no mask length, the casts, the functions such as `abbrev` and `set_masklen`, the containment, bit and arithmetic operators and the order. `connection/inet.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inet() {
        big_stack(|| script(include_str!("connection/inet.test")));
    }

    /// The databases that `initdb` makes: the rows of `template0`, `template1` and `postgres` in `pg_database`, their comments, their privileges, and `regdatabase`. `connection/databases.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn databases() {
        big_stack(|| script(include_str!("connection/databases.test")));
    }

    /// `CREATE TABLESPACE` and `DROP TABLESPACE` of in-place tablespaces: the checks of the statements, the rows of `pg_tablespace`, the functions of a tablespace, and the owner as a dependency of a role. `connection/tablespaces.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn tablespaces() {
        big_stack(|| script(include_str!("connection/tablespaces.test")));
    }

    /// `CREATE ROLE`, `ALTER ROLE`, `DROP ROLE` and `GRANT` and `REVOKE` of a role: the rows of `pg_roles`, `pg_authid` and `pg_auth_members`, the errors, the notices, and a role that a transaction makes and rolls back. `connection/roles.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn roles() {
        big_stack(|| script(include_str!("connection/roles.test")));
    }

    /// `SET ROLE`, `SET SESSION AUTHORIZATION`, `RESET` and `set_config` of them, in a transaction and out of one: `current_user`, `session_user`, the reports of `is_superuser`, the checks of the roles, and the owner of a new table. `connection/set_role.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn set_role() {
        big_stack(|| script(include_str!("connection/set_role.test")));
    }

    /// The functions of the size of an object: `pg_size_pretty` of a `bigint` and of a `numeric`, `pg_size_bytes` with its errors, the size of each fork of an empty table, an index of each access method, a sequence and a view, the file of a relation and the relation of a file, the location of a tablespace, `pg_tablespace_databases` and the size of a database and of a tablespace. `connection/db_size.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn db_size() {
        big_stack(|| script(include_str!("connection/db_size.test")));
    }

    /// The ordered-set and hypothetical-set aggregates: `percentile_disc` and `percentile_cont` of one percentile and of an array, of `double precision` and of `interval`, `mode`, and `rank`, `dense_rank`, `percent_rank` and `cume_dist` of a hypothetical row, with groups, `FILTER`, nulls, no rows, the errors of the percentile and of the arguments, and a view. `connection/ordered_set.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ordered_set() {
        big_stack(|| script(include_str!("connection/ordered_set.test")));
    }

    /// `sum` and `avg` of `interval`: groups, nulls, no rows, infinities, an overflow, the rounding of the average and `DISTINCT`. `connection/interval_sum.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn interval_sum() {
        big_stack(|| script(include_str!("connection/interval_sum.test")));
    }

    /// The hash support functions of the types, plain and extended: the integer types, `bool`, `"char"`, the OID types, the text types, `bytea`, the float types with zeros and `NaN`, `numeric`, the date and time types, `uuid`, the network types, `pg_lsn`, `xid`, `tid`, `oidvector`, arrays, ranges, multiranges and rows, with nulls and the error of a type with no hash function. `connection/hash.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn hash() {
        big_stack(|| script(include_str!("connection/hash.test")));
    }

    /// The comparison of `record` values: the operators, `btrecordcmp`, `ORDER BY`, `DISTINCT`, `max` and `min`, `GREATEST` and `LEAST`, `record_larger` and `record_smaller`, `IN`, nulls in the columns, nested rows, and the errors of rows with other numbers of columns, of other column types and of a column type with no comparison. `connection/record_compare.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn record_compare() {
        big_stack(|| script(include_str!("connection/record_compare.test")));
    }

    /// The wait events of `pg_wait_events`, which `cargo xtask waitevents` makes from the vendored `wait_event_names.txt`: the count of each type, all the rows of `pg_get_wait_events` in their order, and the join with `pg_stat_activity`, which has no row because the session that runs the statement waits for nothing. `connection/wait_events.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn wait_events() {
        big_stack(|| script(include_str!("connection/wait_events.test")));
    }

    /// `convert`, `convert_from`, `convert_to`, `length(bytea, name)` and `to_ascii`: each byte from 0x80 to 0xFF that a single-byte encoding maps, to UTF8 and back, the first byte that it does not map, the names of the encodings and their aliases, SQL_ASCII, the bad UTF-8 sequences, the zero byte, the pairs of encodings with no conversion, ASCII text in the multibyte encodings, and the four encodings of `to_ascii`. `connection/convert.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn convert() {
        big_stack(|| script(include_str!("connection/convert.test")));
    }

    /// The five domains of `information_schema`: the schema and its privileges, the rows of `pg_type`, `pg_constraint` and `pg_depend` with the OIDs of PostgreSQL, `format_type`, the casts to a domain with the check constraints in a constant, in a column and in each element of an array, and the type of a domain in `RowDescription`, which is the base type. `connection/domains.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn domains() {
        big_stack(|| script(include_str!("connection/domains.test")));
    }

    /// `CREATE DOMAIN` and `DROP DOMAIN` of a user: the default, the collation, the `NOT NULL` constraint and the check constraints with their names, the rows of `pg_type` and `pg_constraint`, the views of `information_schema`, the casts that check the constraints, a domain over a domain and over an array, `DROP TYPE` of a domain, and the errors of each statement. `connection/domain_user.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn domain_user() {
        big_stack(|| script(include_str!("connection/domain_user.test")));
    }

    /// `CREATE TYPE` for a composite type: the rows of `pg_type`, `pg_class` and `pg_attribute`, the casts of `ROW` values and of text to the type, the errors of a bad count of columns and of a malformed literal, `pg_input_is_valid`, a nested composite type, comparisons and `ORDER BY`, the selection of a field, the row type of a table, and `DROP TYPE`. `connection/comptype.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn comptype() {
        big_stack(|| script(include_str!("connection/comptype.test")));
    }

    /// `CREATE TYPE ... AS ENUM`: the errors of a type that exists, a label that is too long and a label that is there twice, the input and the output of the labels, `pg_input_is_valid`, the comparisons, `ORDER BY`, `max`, `min`, `DISTINCT` and `GROUP BY`, `enum_first`, `enum_last`, `enum_range` and `enum_cmp`, arrays of an enum type, the casts from and to text, the rows of `pg_enum`, `pg_type` and `pg_depend`, an enum type with no labels, and `DROP TYPE`. `connection/enum_type.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn enum_type() {
        big_stack(|| script(include_str!("connection/enum_type.test")));
    }

    /// `CREATE TYPE ... AS RANGE`: the constructors, the input and the output, the operators and `range_agg` of a new range type and its multirange type, the rows of `pg_type`, `pg_range`, `pg_proc` and `pg_depend`, the options `collation` and `multirange_type_name`, the errors of bad options, and `DROP TYPE`. `connection/range_type.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn range_type() {
        big_stack(|| script(include_str!("connection/range_type.test")));
    }

    /// `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE` and `FOR KEY SHARE`: the clauses on all the relations and on the relations with a name, the errors of `transformLockingClause` and `CheckSelectLocking`, the locks on subqueries, joins, functions, queries of `WITH` and views, the nullable side of an outer join, a sequence, the cursors that lock rows, and a read-only transaction. `connection/for_update.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn for_update() {
        big_stack(|| script(include_str!("connection/for_update.test")));
    }

    /// `INSERT` with `VALUES`, a query and `DEFAULT VALUES`: the defaults of the columns and of the domains, the coercion to the types of the columns, the not-null and check constraints with the failing row, the errors of `transformInsertStmt`, a rollback of the rows, a read-only transaction, `PREPARE` and the extended protocol. `connection/insert.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn insert() {
        big_stack(|| script(include_str!("connection/insert.test")));
    }

    /// `UPDATE` and `DELETE`: `SET` with `DEFAULT` and `ROW`, `FROM` and `USING`, subqueries, the errors of `transformUpdateTargetList`, the check and not-null constraints, a rollback, a read-only transaction, `PREPARE` and the extended protocol. `connection/update.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn update() {
        big_stack(|| script(include_str!("connection/update.test")));
    }

    /// The unique indexes and the primary keys of `INSERT` and `UPDATE`: the error of a duplicate key with the key as `BuildIndexValueDescription` gives it, a key in more than one column, nulls that are distinct or not, the equality of `numeric`, `float8`, `bpchar`, `timestamptz` and `interval`, the order of the indexes, an `UPDATE` that conflicts with a row that it did not change yet, and a key that a delete of the same transaction made free. `connection/unique.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn unique() {
        big_stack(|| script(include_str!("connection/unique.test")));
    }

    /// `RETURNING` of `INSERT`, `UPDATE` and `DELETE`: `*` and expressions, the names `old` and `new` and the options that rename them, the columns of `FROM` and `USING`, subqueries, the errors of `transformReturningClause`, the check constraints, a rollback, `PREPARE` and the extended protocol, and the columns of the target table by a name with the schema, which do not find `old` and `new`. `connection/returning.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn returning() {
        big_stack(|| script(include_str!("connection/returning.test")));
    }

    /// The checks of the foreign keys: a new key that the referenced table does not have, nulls with `MATCH SIMPLE` and `MATCH FULL`, an update that keeps the key, the old key of a delete or an update of the referenced table with `NO ACTION` and `RESTRICT`, the order of the checks, a table that references itself, keys of more than one column, keys of two types, `RETURNING` and a transaction. `connection/foreign_key.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn foreign_key() {
        big_stack(|| script(include_str!("connection/foreign_key.test")));
    }

    /// The actions of the foreign keys: `CASCADE`, `SET NULL` and `SET DEFAULT` for a delete and an update, the checks of the rows that an action changed, an action that a check of `NO ACTION` of another table stops, a table that references itself, keys of more than one column, the casts of the statement of an action in the context of its errors, a row that two actions change, `RETURNING` and a transaction. `connection/foreign_key_actions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn foreign_key_actions() {
        big_stack(|| script(include_str!("connection/foreign_key_actions.test")));
    }

    /// `INSERT ... ON CONFLICT`: `DO NOTHING` with and without an inference, `DO UPDATE` with `EXCLUDED`, an alias, `WHERE`, `DEFAULT` and a multiple-column `SET`, a row that the command proposes two times, a conflict on an index that is not an arbiter, the checks and the foreign keys of the new row, `RETURNING` with `old` and `new`, the errors of the inference, a transaction and a prepared statement. `connection/on_conflict.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn on_conflict() {
        big_stack(|| script(include_str!("connection/on_conflict.test")));
    }

    /// `MERGE`: `WHEN MATCHED`, `WHEN NOT MATCHED BY SOURCE` and `WHEN NOT MATCHED BY TARGET` with `INSERT`, `UPDATE`, `DELETE` and `DO NOTHING`, conditions, `RETURNING` with `merge_action()`, `old` and `new`, a row of the target table that the command changes two times, the checks, the unique keys and the foreign keys, the errors of the analysis, a join as the source, a transaction and a prepared statement. `connection/merge.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn merge() {
        big_stack(|| script(include_str!("connection/merge.test")));
    }

    /// The deferred foreign keys: `DEFERRABLE` and `INITIALLY DEFERRED`, the checks at the commit and at the end of a statement outside of a block, `SET CONSTRAINTS` with `ALL`, a list of names and a name with a schema, the errors of `SET CONSTRAINTS`, an old key of the referenced table that a later insert gives back, a new row that a later statement deletes, and an action of `ON DELETE CASCADE`, which fires at once. `connection/deferred_constraints.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn deferred_constraints() {
        big_stack(|| script(include_str!("connection/deferred_constraints.test")));
    }

    /// `WITH` before `INSERT`, `UPDATE`, `DELETE` and `MERGE`: a query of `WITH` in `FROM`, in `USING`, as the source of `MERGE`, in a subquery of `VALUES`, of `SET`, of `WHERE` and of `RETURNING`, `WITH RECURSIVE`, `MATERIALIZED`, a `WITH` of the `SELECT` of an `INSERT`, a target table with the name of a query of `WITH`, a prepared statement, and the errors. `connection/write_with.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn write_with() {
        big_stack(|| script(include_str!("connection/write_with.test")));
    }

    /// The statements of `WITH` that change data: `DELETE`, `UPDATE`, `INSERT` and `MERGE` with `RETURNING` that the main query reads, a statement that no part reads, a statement that reads another, two parts that change one row, a prepared statement, and the error of a reference to a statement with no `RETURNING` and of a statement below the top. `connection/write_cte.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn write_cte() {
        big_stack(|| script(include_str!("connection/write_cte.test")));
    }

    /// The checks of the statements of `WITH` that change data: a view, a cursor, `COPY`, a recursive query, the `SELECT` of an `INSERT`, the names of the columns, a read-only transaction, `RETURNING` with `old` and `new`, a subquery, a nested `WITH`, a set operation and the foreign keys of the rows of the statements. `connection/write_cte_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn write_cte_checks() {
        big_stack(|| script(include_str!("connection/write_cte_checks.test")));
    }

    /// A row comparison with a subquery, as `(a, b) = (SELECT ...)`: the operators `=` and `<>`, a subquery with no rows or more than one row, null fields, the errors for the number of columns and for the operators, a correlated subquery, `ROW(a)`, a prepared statement and the text of a view. `connection/row_compare_sublink.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_compare_sublink() {
        big_stack(|| script(include_str!("connection/row_compare_sublink.test")));
    }

    /// A row comparison with `<`, `<=`, `>` or `>=`: the pairs of fields after the first pair that is not equal, null fields, fields of different types, the errors, a comparison in `WHERE`, with a subquery and with `ANY` and `ALL`, a prepared statement and the text of a view. `connection/row_compare.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_compare() {
        big_stack(|| script(include_str!("connection/row_compare.test")));
    }

    /// `IS DISTINCT FROM` and `IS NOT DISTINCT FROM` for rows and with a `NULL` literal: null fields, rows of one field and of no fields, fields of different types, the errors, a test in `WHERE`, a prepared statement and the text of a view. `connection/row_distinct.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_distinct() {
        big_stack(|| script(include_str!("connection/row_distinct.test")));
    }

    /// `ARRAY(SELECT ...)` of a column that is an array: rows of arrays with one and two dimensions, lower bounds that are not 1, no rows, the errors for a null array, an empty array, arrays of different dimensions and too many dimensions, a correlated subquery and the text of a view. `connection/array_sublink_array.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_sublink_array() {
        big_stack(|| script(include_str!("connection/array_sublink_array.test")));
    }

    /// The output of `record[]`: rows with null fields, quoted fields, nested rows and arrays, rows of no fields, arrays of two dimensions, `ARRAY(SELECT ...)` of rows, a subscript, a cast to text, the error for the input and the error for a view. `connection/record_array.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn record_array() {
        big_stack(|| script(include_str!("connection/record_array.test")));
    }

    /// An expression in `FROM` that gives a composite type: casts, `COALESCE` and `NULLIF`, the row type of a table, a nested composite type, aliases of the columns, the errors for too many aliases and a column definition list, a null value, `WITH ORDINALITY`, `ROWS FROM`, `LATERAL`, and the text of views with casts of rows. `connection/composite_from.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn composite_from() {
        big_stack(|| script(include_str!("connection/composite_from.test")));
    }

    /// `RESPECT NULLS` and `IGNORE NULLS` of `lag`, `lead`, `first_value`, `last_value` and `nth_value`: offsets that are negative, zero and null, defaults, partitions, frames with exclusions, the errors for the other window functions, aggregates and plain functions, a query with no rows and the text of a view. `connection/window_ignore_nulls.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn window_ignore_nulls() {
        big_stack(|| script(include_str!("connection/window_ignore_nulls.test")));
    }

    /// The whole row of a join with an alias and of the alias of `USING`: inner, outer, full and cross joins, the aliases of the columns, the fields and the error for a name that is no field, `.*`, `row_to_json`, `IS NULL`, comparisons, a reference from a subquery, a join in a join, and views. `connection/join_whole_row.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn join_whole_row() {
        big_stack(|| script(include_str!("connection/join_whole_row.test")));
    }

    /// `pg_get_viewdef` for a join with an alias: the names of the join and of its columns, the aliases of the columns that it shows only when a name changes, the names of `USING` that go down to the relations in the join, a column of the join in the list, `WHERE`, `ORDER BY` and `GROUP BY`, a join in a join, a name that another relation has, `USING ... AS`, `LATERAL` and a subquery. `connection/join_alias_viewdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn join_alias_viewdef() {
        big_stack(|| script(include_str!("connection/join_alias_viewdef.test")));
    }

    /// `t.*` for a relation of a query outside the subquery, in the list of a scalar subquery, `ARRAY`, `EXISTS`, `IN` and `LATERAL`, and `t.*` and `(e).*` in `ROW`, in a row comparison and in `IN` with a subquery, with the errors and the text of views. `connection/outer_star.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn outer_star() {
        big_stack(|| script(include_str!("connection/outer_star.test")));
    }

    /// An aggregate of an outer query in a subquery, as `agglevelsup`: in the select list, `HAVING` and `ORDER BY` of the outer query, with `GROUP BY`, `FILTER`, `ORDER BY` and `DISTINCT` in the call, an ordered-set aggregate, a subquery in a subquery, an aggregate of the subquery in the same expression, `EXISTS` and `ARRAY`, the errors of the places where the aggregate cannot be and of the columns that are not in `GROUP BY`, and the text of views. `connection/outer_aggregate.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn outer_aggregate() {
        big_stack(|| script(include_str!("connection/outer_aggregate.test")));
    }

    /// `GROUPING SETS`, `ROLLUP`, `CUBE`, `GROUP BY DISTINCT` and `GROUPING`: the groups of each grouping set, the items that are null in the groups of a set that does not have them, also in a subquery, `HAVING`, window functions, the aggregates with `DISTINCT` and `ORDER BY`, an empty input, the errors of the places where `GROUPING` cannot be, of its arguments and of the limits, the check of a primary key with only the items in all the sets, and the text of views. `connection/grouping_sets.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn grouping_sets() {
        big_stack(|| script(include_str!("connection/grouping_sets.test")));
    }

    /// The SQL/JSON constructors `JSON_OBJECT`, `JSON_ARRAY`, `JSON`, `JSON_SCALAR` and `JSON_SERIALIZE`, and the predicate `IS JSON`: `ABSENT ON NULL`, `WITH UNIQUE KEYS`, `FORMAT JSON`, the types of `RETURNING`, the names of the columns, the errors of keys that are null or not unique and of bad input, and the text of views. `connection/sqljson.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqljson() {
        big_stack(|| script(include_str!("connection/sqljson.test")));
    }

    /// The SQL/JSON aggregates `JSON_OBJECTAGG` and `JSON_ARRAYAGG` and `JSON_ARRAY` with a subquery: `ABSENT ON NULL`, `WITH UNIQUE KEYS`, `FORMAT JSON`, `ORDER BY`, `FILTER`, `OVER`, the types of `RETURNING`, an empty input, the errors of keys, of nested calls and of a subquery with more than one column, and the text of views. `connection/sqljson_agg.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqljson_agg() {
        big_stack(|| script(include_str!("connection/sqljson_agg.test")));
    }

    /// The SQL/JSON query functions `JSON_EXISTS`, `JSON_VALUE` and `JSON_QUERY`: `PASSING`, the types of `RETURNING` with domains and arrays, the wrappers, `OMIT QUOTES`, the behaviors `ON EMPTY` and `ON ERROR` and their casts, the errors of the analyzer and of the path, the flags that a null context item reads from the row before, and the text of views. `connection/sqljson_query.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqljson_query() {
        big_stack(|| script(include_str!("connection/sqljson_query.test")));
    }

    /// `JSON_TABLE` in `FROM`: the columns of `JSON_VALUE`, `JSON_QUERY` and `EXISTS`, `FOR ORDINALITY`, `PASSING`, the plans of `NESTED PATH` and of sibling paths, `LATERAL` references, the top `ON ERROR`, the errors of the analyzer and of the columns, and the text of views. `connection/sqljson_table.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqljson_table() {
        big_stack(|| script(include_str!("connection/sqljson_table.test")));
    }

    /// `SEARCH DEPTH FIRST`, `SEARCH BREADTH FIRST` and `CYCLE` in `WITH RECURSIVE`: the columns of the clauses in each reference, `UNION` and `UNION ALL`, the type of the mark, the columns that `*` does not give inside the query, the errors of the clauses, and the text of views. `connection/with_search_cycle.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn with_search_cycle() {
        big_stack(|| script(include_str!("connection/with_search_cycle.test")));
    }

    /// The SQL/XML functions with their own syntax, `XMLELEMENT`, `XMLFOREST`, `XMLCONCAT`, `XMLPARSE`, `XMLPI`, `XMLROOT`, `XMLSERIALIZE` and `IS DOCUMENT`, and the functions `xmlcomment`, `xmltext`, `xml`, `xmlagg` and `xml_is_well_formed`: the names of elements and attributes, the forms of the values, the declarations, the errors of the analyzer and of the functions, the text of views and of stored expressions. `connection/xml_exprs.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn xml_exprs() {
        big_stack(|| script(include_str!("connection/xml_exprs.test")));
    }

    /// The `TABLESAMPLE` clause with the methods `system` and `bernoulli`: the rows of a sample of 0 and 100 percent, the checks of the percentage and of `REPEATABLE`, the errors of the analyzer, a sample in a prepared statement, and the text of views. `connection/tablesample.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn tablesample() {
        big_stack(|| script(include_str!("connection/tablesample.test")));
    }

    /// `DROP TEXT SEARCH` for templates, parsers, dictionaries and configurations: the errors and the notices of names that give no object, the pinned objects, the configurations that depend on a dictionary, the dictionaries that depend on a template, a schema with objects of text search, and a removal in a transaction that rolls back. `connection/drop_text_search.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn drop_text_search() {
        big_stack(|| script(include_str!("connection/drop_text_search.test")));
    }

    /// `CREATE TEXT SEARCH PARSER`: the rows of `pg_ts_parser`, the errors of `DefineTSParser` and `get_ts_parser_func`, `ts_token_type` and `ts_parse` with a new parser, a configuration of a new parser, `ts_headline` with a parser that has no headline function, `COMMENT`, the dependencies and `DROP`. `connection/ts_parser.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ts_parser() {
        big_stack(|| script(include_str!("connection/ts_parser.test")));
    }

    /// The functions in SQL of `information_schema` that have a `RETURN` body, such as `_pg_char_max_length`: the rows of `pg_proc` and `pg_depend` with the OIDs of PostgreSQL, `pg_get_function_arguments` and `pg_get_function_result`, the results for many types and type modifiers and for the columns of a table, a call with and without the schema in `search_path`, and the errors of a bad argument. Also the shift operators `<<` and `>>` of the integer types, which the bodies use. `connection/sql_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sql_functions() {
        big_stack(|| script(include_str!("connection/sql_functions.test")));
    }

    /// Whole-row references and fields of a row: the text form of a row with the quotes of its fields, `IS NULL` and `IS NOT NULL` on a row, `(t).a` and `(x).typname`, a row of the null side of an outer join, the errors of a bad field, and `pg_get_viewdef` of a view with `t.*` and `(x).typname`. Also the functions `_pg_truetypid` and `_pg_truetypmod` of `information_schema`, which take the rows of `pg_attribute` and `pg_type`. `connection/rows.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn rows() {
        big_stack(|| script(include_str!("connection/rows.test")));
    }

    /// The views of the parts of the server that rupg does not have, which have no rows: asynchronous I/O, the registry of dynamic shared memory, the replication origins, recovery, the publications and the cursors. Also the error of `pg_shmem_allocations_numa` with no NUMA support, and the errors of the functions of the publications for a name that is not a publication. `connection/absent.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn absent() {
        big_stack(|| script(include_str!("connection/absent.test")));
    }

    /// The views of the statistics of the whole server: `pg_stat_bgwriter`, `pg_stat_checkpointer`, `pg_stat_wal`, `pg_stat_io`, `pg_stat_archiver`, `pg_stat_slru`, `pg_stat_recovery_prefetch` and `pg_stat_lock`. The script checks the columns and their types, the rows and the null cells of `pg_stat_io`, and that each counter is not negative and each `stats_reset` is not null. `connection/shared.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn shared_statistics() {
        big_stack(|| script(include_str!("connection/shared.test")));
    }

    /// The views of the work that rupg does not do yet, which have no rows: the progress of commands, replication, the logical replication workers, the replication slots and the prepared transactions. Also the statistics of a subscription with no statistics, the error for an unknown command name and the names of the phases of an index build. `connection/replication.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn replication() {
        big_stack(|| script(include_str!("connection/replication.test")));
    }

    /// The table of the sessions in `pg_stat_activity` and the functions of one session: the state, the text of the statement and its times, the snapshot of a transaction and `pg_stat_clear_snapshot`, `track_activities` and `track_activity_query_size`, the addresses of a Unix socket, and the views `pg_stat_ssl`, `pg_stat_gssapi` and `pg_stat_database`. `connection/activity.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn activity() {
        big_stack(|| script(include_str!("connection/activity.test")));
    }

    /// `CREATE DATABASE` and `DROP DATABASE`: the checks of the options, the rows of `pg_database`, the transaction blocks and the extended protocol. `connection/create_database.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn create_database() {
        big_stack(|| script(include_str!("connection/create_database.test")));
    }

    /// A correlated subquery with an `=` of a column and an expression of the columns of the outer query, which the scan finds through an index of the column: an arithmetic expression, a text concatenation, `EXISTS`, `NULLIF` that gives a null, and a function. `connection/outer_lookups.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn outer_lookups() {
        big_stack(|| script(include_str!("connection/outer_lookups.test")));
    }

    /// The runs of a `LATERAL` relation in a nested loop: a join that keeps a few rows of many runs, an outer join with runs that give no row, a whole-row reference, two `LATERAL` relations in a chain, and an aggregate over the joined rows. `connection/lateral_runs.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn lateral_runs() {
        big_stack(|| script(include_str!("connection/lateral_runs.test")));
    }

    /// `CREATE TABLE AS` and `SELECT INTO`: the types, typmods and collations of the columns, a list of column names, `IF NOT EXISTS`, `WITH NO DATA`, the options of the table, the errors of the query and of the table, a query that fails at run time, and a transaction block. `connection/create_table_as.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn create_table_as() {
        big_stack(|| script(include_str!("connection/create_table_as.test")));
    }

    /// `VACUUM` and `ANALYZE`: the options and their checks, the column lists, a view, a sequence, an index and a view of the catalog that the statement skips, names that do not exist, a catalog, and a transaction block. `connection/vacuum.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vacuum() {
        big_stack(|| script(include_str!("connection/vacuum.test")));
    }

    /// `REINDEX`: the options and their checks, an index, a table with a TOAST table, a partitioned table and a partitioned index, a schema, the system catalogs, the database, `CONCURRENTLY` in a transaction block and in a function, the messages of `VERBOSE`, the permissions, and the new `relfilenode` of each index. `connection/reindex.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn reindex() {
        big_stack(|| script(include_str!("connection/reindex.test")));
    }

    /// `CREATE INDEX CONCURRENTLY`: `IF NOT EXISTS`, a unique index, a partial index and an expression index, a transaction block, a function and an element of `CREATE SCHEMA`, a partitioned table, a temporary table, and a failed build, which leaves an index that is not valid and not ready until `REINDEX` builds it again. `connection/cic.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn cic() {
        big_stack(|| script(include_str!("connection/cic.test")));
    }

    /// `UPDATE ... SET (a, b) = (SELECT ...)`: a correlated and an uncorrelated subquery, no row, too many rows, the number of columns, the types, `RETURNING`, a volatile function that runs once for each row, `EXPLAIN`, `ON CONFLICT DO UPDATE`, `MERGE`, a partitioned table, a view and a function. `connection/multiset.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn multiset() {
        big_stack(|| script(include_str!("connection/multiset.test")));
    }

    /// `MERGE` into an automatically updatable view: the columns of the view, its conditions on the rows that match and on `NOT MATCHED BY SOURCE`, `RETURNING`, `WITH CHECK OPTION`, a view on a view, the defaults of the view, the errors of a view that is not updatable, of a column that is not updatable and of a view with triggers `INSTEAD OF` for some actions, the triggers of the base table, a partitioned table, `security_barrier` and `WITH`. `connection/mergeview.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn mergeview() {
        big_stack(|| script(include_str!("connection/mergeview.test")));
    }

    /// `MERGE` into a view with triggers `INSTEAD OF` for all its actions: the triggers of each action with the old and the new rows of the view, a trigger that skips a row, `RETURNING` with the row of a trigger, a row of the view that two source rows match, `NOT MATCHED BY SOURCE`, the triggers `FOR EACH STATEMENT` of the view, a view that is not updatable and `WITH`. `connection/mergetrig.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn mergetrig() {
        big_stack(|| script(include_str!("connection/mergetrig.test")));
    }

    /// `INSERT ... ON CONFLICT` into an automatically updatable view: the inference, `SET` and `EXCLUDED` name the columns of the view, a column of the view that is an expression infers an index on the expression, `ON CONSTRAINT` looks for a constraint of the view, and `WITH CHECK OPTION` checks the row of `DO UPDATE`. `connection/viewconflict.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn viewconflict() {
        big_stack(|| script(include_str!("connection/viewconflict.test")));
    }

    /// `INSERT ... ON CONFLICT DO SELECT`: a row with a conflict gives the row that the table has to `RETURNING` when `WHERE` lets it through, and the statement writes no row. `RETURNING` is required, the old and the new rows are the row that the table has, a lock needs `UPDATE`, and a lock of a row of the same command is an error. `connection/doselect.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn doselect() {
        big_stack(|| script(include_str!("connection/doselect.test")));
    }

    /// The subscripts of `point`, `lseg`, `box` and `line`, which read and write a coordinate or a point of the value as an element of a fixed-length array from 0. `connection/geosub.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn geosub() {
        big_stack(|| script(include_str!("connection/geosub.test")));
    }

    /// `ALTER TABLE ... ALTER CONSTRAINT`: the deferrability of a foreign key and of its triggers, also on partitions, the inheritability of a not-null constraint, and the errors. `connection/altercon.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn altercon() {
        big_stack(|| script(include_str!("connection/altercon.test")));
    }

    /// `ADD FOREIGN KEY ... NOT VALID` and `VALIDATE CONSTRAINT` of a foreign key, on a table, on the partitions of a partitioned table and to a partitioned table. `connection/fknv.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn fknv() {
        big_stack(|| script(include_str!("connection/fknv.test")));
    }

    /// `ADD CONSTRAINT ... NOT NULL ... NOT VALID`, `VALIDATE CONSTRAINT` and `SET NOT NULL` of a not-null constraint that is not valid, on a table, on the children of `INHERITS` and on the partitions. A query does not take the column as not null. `connection/nnnv.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn nnnv() {
        big_stack(|| script(include_str!("connection/nnnv.test")));
    }

    /// `NOT ENFORCED` check constraints and foreign keys, `ALTER CONSTRAINT ... ENFORCED` and `NOT ENFORCED`, `VALIDATE CONSTRAINT` of a constraint that is not enforced, the partitions of a table with such a constraint and the conflict of `ATTACH PARTITION`. A foreign key that is not enforced has no triggers. `connection/noenf.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn noenf() {
        big_stack(|| script(include_str!("connection/noenf.test")));
    }

    /// `ALTER TABLE ... ADD CONSTRAINT ... USING INDEX`: a primary key and a unique constraint on an existing index, the rename of the index, the not-null constraints of a primary key, a deferrable constraint and its trigger, an `INCLUDE` column, `NULLS NOT DISTINCT`, and the errors for an index that cannot have the constraint. `connection/uix.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn uix() {
        big_stack(|| script(include_str!("connection/uix.test")));
    }

    /// `WHERE CURRENT OF` in `UPDATE` and `DELETE`: a plain scan of a table, a cursor with `FOR UPDATE`, a join with `FOR UPDATE OF`, `LIMIT`, a subquery in `FROM`, a prepared statement, a child of the table, a cursor of a function in PL/pgSQL, and the errors of `execCurrentOf`. `connection/curof.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn curof() {
        big_stack(|| script(include_str!("connection/curof.test")));
    }

    /// `COPY` of `INSERT`, `UPDATE`, `DELETE` and `MERGE` with `RETURNING`, the notices of the triggers of the write, the CSV and JSON formats, and the errors for a statement with no `RETURNING` and for the rules of the table. `connection/cdml.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn cdml() {
        big_stack(|| script(include_str!("connection/cdml.test")));
    }

    /// `ON CONFLICT` with a collation or an operator class on an element of the inference takes only the unique indexes whose key column has the same collation, or an operator class of the same family and input type, as `infer_collation_opclass_match` matches them. `ASC`, `DESC`, `NULLS FIRST`, `NULLS LAST` and operator class options are errors. `connection/icoll.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn icoll() {
        big_stack(|| script(include_str!("connection/icoll.test")));
    }

    /// `EXPLAIN` of `INSERT ... ON CONFLICT` names the target table by its alias, and the row that the insert proposes `excluded`, as `set_rtable_names` names the entries of the one range table. `connection/excl.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn excl() {
        big_stack(|| script(include_str!("connection/excl.test")));
    }

    /// `SET (a, b) = ROW(...)` counts the fields after `t.*` in the row gives the columns of `t`, as `transformRowExpr` expands the row with `transformExpressionList`. `connection/rowstar.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn rowstar() {
        big_stack(|| script(include_str!("connection/rowstar.test")));
    }

    /// A write of an updatable view whose base view has triggers `INSTEAD OF` goes to the triggers of the base view. `connection/vtrig.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vtrig() {
        big_stack(|| script(include_str!("connection/vtrig.test")));
    }

    /// A new column of a table that a view reads gives the view a new text, as `pg_get_viewdef` writes it with an alias for the columns of the table, so that the columns of `USING` and of `NATURAL` stay the same. `connection/addcol.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn addcol() {
        big_stack(|| script(include_str!("connection/addcol.test")));
    }

    /// `COST` of `CREATE FUNCTION` and of `ALTER FUNCTION` takes a number with a fraction, as `procost` is a `float4`. `connection/fcost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn fcost() {
        big_stack(|| script(include_str!("connection/fcost.test")));
    }

    /// The planner orders the conditions of a scan by the `procost` of their functions, as `order_qual_clauses` does, and `EXPLAIN` adds `procost` times `cpu_operator_cost` for each call. `connection/fqcost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn fqcost() {
        big_stack(|| script(include_str!("connection/fqcost.test")));
    }

    /// The executor runs the conditions of `WHERE` in the order of their costs, as `order_qual_clauses` orders them, and the conditions of a view with `security_barrier` of an `UPDATE` or a `DELETE` first, but for a cheap leakproof condition. `connection/qorder.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn qorder() {
        big_stack(|| script(include_str!("connection/qorder.test")));
    }

    /// A simple view and a simple subquery of `FROM` merge into the query, as `pull_up_simple_subquery` merges them, so that a cheap condition of the query runs before the condition of the view. A view with `security_barrier` and a subquery with `OFFSET` stay subqueries. `connection/vsel.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vsel() {
        big_stack(|| script(include_str!("connection/vsel.test")));
    }

    /// `EXPLAIN` of a simple view shows the scan of its base table, with the conditions of the view and of the query, as `pull_up_simple_subquery` merges the view. A view with `security_barrier` stays a subquery and takes only the leakproof conditions. `connection/vexp.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vexp() {
        big_stack(|| script(include_str!("connection/vexp.test")));
    }

    /// The executor orders the conditions of `WHERE` by their costs, and `x op ANY (array)` costs its operator for half of the items of the array, or one hash and one comparison for a constant array of 9 items or more, as `cost_qual_eval_walker` finds it. `connection/saorder.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn saorder() {
        big_stack(|| script(include_str!("connection/saorder.test")));
    }

    /// A volatile condition that reads no column runs for each row of the scan of the one relation of `FROM`, in the order of the costs of the conditions, and it moves into a view with `security_barrier`, as `distribute_qual_to_rels` gives it the relations of its level. A condition that reads no column and calls no volatile function runs once, in the one-time filter. A prepared statement reads a view with and without `security_barrier`. `connection/vleak.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vleak() {
        big_stack(|| script(include_str!("connection/vleak.test")));
    }

    /// `EXPLAIN` names only the relations that the plan scans, as `select_rtable_names_for_explain` passes `rels_used` to `set_rtable_names`: a subquery or a view whose `Subquery Scan` the plan does not keep takes no name, so a relation in it can take the alias. A subquery of `UNION ALL` keeps its name. `connection/valias.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn valias() {
        big_stack(|| script(include_str!("connection/valias.test")));
    }

    /// `ctid` of a table of the user: the item pointer of the version of each row, which a view can give, a condition can compare, and a cursor finds the row of `CURRENT OF` by. A view column of `ctid` is not updatable. `connection/vctid.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vctid() {
        big_stack(|| script(include_str!("connection/vctid.test")));
    }

    /// A view with `security_invoker` under a view of another owner: the view outside checks the view inside as its owner, and the view inside checks its tables as the current user, as `RelationBuildRuleLock` gives `InvalidOid` to `setRuleCheckAsUser` for its rules. `connection/vinvoker.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vinvoker() {
        big_stack(|| script(include_str!("connection/vinvoker.test")));
    }

    /// `EXPLAIN` of a write through automatically updatable views names the relations before the columns, as the range table keeps the entry of each view, so it has more than one entry. `connection/vexpw.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vexpw() {
        big_stack(|| script(include_str!("connection/vexpw.test")));
    }

    /// A leaky condition on a view of `security_barrier` over a join, and on a view of `security_barrier` over that view and a table, runs on the rows of the view only. `connection/vjb.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vjb() {
        big_stack(|| script(include_str!("connection/vjb.test")));
    }

    /// `CREATE OR REPLACE VIEW` keeps the collation of each old column of the view, as `checkViewColumns` checks it, and `pg_get_viewdef` shows a `COLLATE` clause, as `get_rule_expr` does. `connection/vcoll.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vcoll() {
        big_stack(|| script(include_str!("connection/vcoll.test")));
    }

    /// A view with two joins of `USING` on columns of the same name shows the second column of `USING` by its unique name, and `ALTER TABLE ... DROP COLUMN` writes the text of a view again with no alias for the dropped column, as `set_relation_column_names` skips it. `connection/vdrop.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vdrop() {
        big_stack(|| script(include_str!("connection/vdrop.test")));
    }

    /// A view of a function in `FROM` that gives a composite type depends on each column of the relation of the type that the view reads, as `process_function_rte_ref` records it, so `ALTER TABLE` cannot drop the column or change its type. `connection/vfndep.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vfndep() {
        big_stack(|| script(include_str!("connection/vfndep.test")));
    }

    /// A whole-row reference in `ROW()`, in `VALUES` and in a row comparison prints with the cast to its row type, as `get_rule_expr_toplevel` prints it, so the parser does not expand `*` when it reads the text of the view again. `connection/vwrow.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vwrow() {
        big_stack(|| script(include_str!("connection/vwrow.test")));
    }

    /// A long alias that is not unique gets `_1` after characters are dropped from its end, so the name stays shorter than `NAMEDATALEN`, as `set_rtable_names` makes it. `connection/vlong.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vlong() {
        big_stack(|| script(include_str!("connection/vlong.test")));
    }

    /// A scalar subquery after `ANY` or `ALL` prints with a cast to its type, as `get_rule_expr` prints a `ScalarArrayOpExpr`, so the text does not read back as an `ANY` subquery. `connection/vsaop.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vsaop() {
        big_stack(|| script(include_str!("connection/vsaop.test")));
    }

    /// `restrict_nonsystem_relation_kind` with `view` refuses a read or a write of a view of the user, as `ApplyRetrieveRule` and `rewriteTargetView` check it, and the check hook takes only `view` and `foreign-table`. `connection/vrestrict.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrestrict() {
        big_stack(|| script(include_str!("connection/vrestrict.test")));
    }

    /// `DROP` of the kinds of objects that rupg cannot make: a missing access method, cast, collation, conversion, extension, language, foreign-data wrapper, server, event trigger, publication or foreign table gives the error of `get_object_address`, or with `IF EXISTS` the notice of `does_not_exist_skipping`, a pinned object gives the error of `findDependentObjects`, and a role that is not a superuser fails the check of `check_object_ownership`. A missing schema of a type of the arguments of `DROP FUNCTION` and `DROP OPERATOR` with `IF EXISTS` gives a notice. `connection/vdropx.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vdropx() {
        big_stack(|| script(include_str!("connection/vdropx.test")));
    }

    /// `ALTER PROCEDURE` with an attribute that only a function has gives the error of `AlterFunction`, and `CALL` of a procedure that the current role cannot execute names the procedure in the error of `aclcheck_error`. `connection/vproc.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vproc() {
        big_stack(|| script(include_str!("connection/vproc.test")));
    }

    /// A join gives its rows in the order of its plan: a nested loop gives the rows of its inner side for each row of its outer side, and a hash join gives the rows of its inner side that match in the reverse order of the scan, as `ExecHashTableInsert` puts each row at the head of its bucket. `connection/vjoinorder.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vjoinorder() {
        big_stack(|| script(include_str!("connection/vjoinorder.test")));
    }

    /// A `SELECT ... INTO` in a subquery, a CTE, a cursor or `INSERT ... SELECT` gives the error of `transformSelectStmt`, and the `INTO` of a set operation is on its leftmost `SELECT`, where `transformOptionalSelectInto` finds it. `connection/vselinto.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vselinto() {
        big_stack(|| script(include_str!("connection/vselinto.test")));
    }

    /// `CREATE MATERIALIZED VIEW` and `REFRESH MATERIALIZED VIEW` fill the materialized view as its owner with the `search_path` of `RestrictSearchPath`, as `RefreshMatViewByOid` does, and `CREATE TABLE AS` keeps the `search_path` and the user of the session. `connection/vmvpath.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vmvpath() {
        big_stack(|| script(include_str!("connection/vmvpath.test")));
    }

    /// The detail of a failing row shows only the columns that the user can read or that the statement gives values, as `ExecBuildSlotValueDescription` builds it, and the detail of a row that no partition takes shows the key only when the user can read all its columns, as `ExecBuildSlotPartitionKeyDescription` builds it. `connection/vrowdesc.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrowdesc() {
        big_stack(|| script(include_str!("connection/vrowdesc.test")));
    }

    /// A function that gives a set in the single row of `VALUES` of an `INSERT` inserts a row for each of its rows, as `check_srf_call_placement` allows for `EXPR_KIND_VALUES_SINGLE`, and the action `INSERT` of a `MERGE` gives the error of `ExecInitFunc`. `connection/vsrfval.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vsrfval() {
        big_stack(|| script(include_str!("connection/vsrfval.test")));
    }

    /// A statement with no plan in a `BEGIN ATOMIC` body is an error with the command tag of the statement, as `interpret_AS_clause` gives it, after the analysis of the statements before it. A `CALL` resolves its procedure first, and `SELECT ... INTO` is not allowed. `connection/vatomutil.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vatomutil() {
        big_stack(|| script(include_str!("connection/vatomutil.test")));
    }

    /// `pg_get_functiondef` shows an `INSERT` of a `BEGIN ATOMIC` body as `get_insert_query_def` does: the columns that have a value in the order of the column list, the alias, `OVERRIDING`, `DEFAULT`, and the rows of `VALUES` or the `SELECT`. `connection/vinsdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vinsdef() {
        big_stack(|| script(include_str!("connection/vinsdef.test")));
    }

    /// `pg_get_ruledef` shows an action `INSERT` of a rule as `get_insert_query_def` does, with `old` and `new`, the alias, the rows of `VALUES`, `DEFAULT VALUES`, the `SELECT`, and the parts of the columns that the values go to. `connection/vruleins.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vruleins() {
        big_stack(|| script(include_str!("connection/vruleins.test")));
    }

    /// `pg_get_ruledef` shows an action `UPDATE` or `DELETE` of a rule as `get_update_query_def` and `get_delete_query_def` do, with `old` and `new`, the alias, `ONLY`, the items of `SET` with the parts of the columns, `SET (a, b) = (SELECT ...)`, `FROM`, `USING` and `WHERE`. `pg_get_functiondef` shows them in a `BEGIN ATOMIC` body. `connection/vruleupd.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vruleupd() {
        big_stack(|| script(include_str!("connection/vruleupd.test")));
    }

    /// `old.*` and `new.*` in the `VALUES` of an action `INSERT` of a rule, and `(e).*` in `VALUES`, give a value for each column or each field, as `transformExpressionList` expands them. `connection/vrulestar.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrulestar() {
        big_stack(|| script(include_str!("connection/vrulestar.test")));
    }

    /// `ALTER RULE ... RENAME` and `ALTER TABLE ... ENABLE RULE` and `DISABLE RULE`: the new name and the new `ev_enabled` in `pg_rewrite`, the rules that fire after the change, and the errors of `RenameRewriteRule`, `RangeVarCallbackForRenameRule` and `EnableDisableRule`. `connection/vrulealter.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrulealter() {
        big_stack(|| script(include_str!("connection/vrulealter.test")));
    }

    /// `RETURNING` in the action of a rule: the rows of the list of the statement of the user with the values of the list of the action, the tag of the statement that sets it, the deparse of the list, and the errors of `checkRuleResultList` and `RewriteQuery`. `connection/vruleret.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vruleret() {
        big_stack(|| script(include_str!("connection/vruleret.test")));
    }

    /// A cast with no typmod of a value with a typmod, as `a::numeric` of a column `numeric(6,2)`: the typmod -1 that the `RelabelType` of `coerce_type_typmod` shows, in the description of the rows, in the columns of a view and of `CREATE TABLE AS`, and in the deparse. `connection/vcasttm.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vcasttm() {
        big_stack(|| script(include_str!("connection/vcasttm.test")));
    }

    /// `LISTEN`, `UNLISTEN`, `NOTIFY` and `pg_notify` in one session: the notifications that the session sends to itself after a commit, with no two the same in a transaction, none after a rollback or for a subtransaction that rolled back, the errors of the channel and the payload, and `pg_listening_channels`. `connection/vnotify.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vnotify() {
        big_stack(|| script(include_str!("connection/vnotify.test")));
    }

    /// `NOTIFY` as an action of a rule: it runs once for the statement, with no regard to its rows, also for a rule `DO INSTEAD`. `pg_get_ruledef` shows it as `get_utility_query_def` does, and `CREATE RULE` gives the error of PostgreSQL for `NOTIFY` in a rule with a condition. `connection/vrulenotify.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrulenotify() {
        big_stack(|| script(include_str!("connection/vrulenotify.test")));
    }

    /// `SELECT` as an action of a rule: it reads the rows of the statement as `old` and `new`, and the client gets its rows before the tag of the statement when no list of `RETURNING` gives the rows. `pg_get_ruledef` shows the action. `connection/vrulesel.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrulesel() {
        big_stack(|| script(include_str!("connection/vrulesel.test")));
    }

    /// `COMMENT ON RULE` sets and drops the comment of a rule, and `DROP RULE` drops it too. `connection/vcomrule.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vcomrule() {
        big_stack(|| script(include_str!("connection/vcomrule.test")));
    }

    /// The shoelace rules of the rules chapter of the PostgreSQL manual: views with rules `DO INSTEAD` that write the tables under them, and a rule that updates such a view. `connection/vshoe.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vshoe() {
        big_stack(|| script(include_str!("connection/vshoe.test")));
    }

    /// The rules regression test after the shoelace part: conditional rules `DO INSTEAD`, a rule of a view with two actions, rules and foreign keys, and `CREATE OR REPLACE RULE`. `connection/vrules2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrules2() {
        big_stack(|| script(include_str!("connection/vrules2.test")));
    }

    /// More of the rules regression test: rules and views, the ON SELECT rules that PostgreSQL does not allow, `ALTER RULE ... RENAME`, and rules with `FOR UPDATE`. `connection/vrules3.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrules3() {
        big_stack(|| script(include_str!("connection/vrules3.test")));
    }

    /// The rules regression test up to the rules with `ON CONFLICT`, which `pg_rules` shows and which give the rows of the conflicts. `connection/vrules4.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrules4() {
        big_stack(|| script(include_str!("connection/vrules4.test")));
    }

    /// The rules regression test to its end, with the rules that `ALTER TABLE` disables and that fire in the mode `replica` of `session_replication_role`, and with `pg_get_functiondef` of the settings of a function and of a `MERGE`, the functions of `pg_get_*def` for objects that do not exist and `MERGE` on a table with rules. `\sf` is `pg_get_functiondef` here. `connection/vrules5.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn vrules5() {
        big_stack(|| script(include_str!("connection/vrules5.test")));
    }

    /// `session_replication_role`: in the mode `replica` only the triggers of `ENABLE REPLICA` and `ENABLE ALWAYS` fire, and the foreign keys are not checked. In the mode `local` the triggers of the origin fire. `connection/replication_role.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn replication_role() {
        big_stack(|| script(include_str!("connection/replication_role.test")));
    }

    /// The privileges of the actions of rules: the owner of the table of the rule needs the privileges on the tables of the action, also for a view with `security_invoker` and for a rule that a rule fires, and the privileges of the statement of the user count first. `connection/ruleperm.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ruleperm() {
        big_stack(|| script(include_str!("connection/ruleperm.test")));
    }

    /// `pg_get_viewdef` of a view of `VALUES` after `ALTER TABLE ... RENAME COLUMN`: the list `"*VALUES*"` shows as `(VALUES ...)`, and a subquery of `VALUES` in the text of the view shows as `( VALUES ...)`. `connection/values_view.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn values_view() {
        big_stack(|| script(include_str!("connection/values_view.test")));
    }

    /// `EXPLAIN` of a statement that rules rewrite: the plan of each statement of the rewrite, in the text form and in the form JSON, and `Query rewrites to nothing` for `DO INSTEAD NOTHING`. `connection/explain_rules.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_rules() {
        big_stack(|| script(include_str!("connection/explain_rules.test")));
    }

    /// `NULLIF`, `IS DISTINCT FROM` and `ANY` with an operator whose function is in SQL or in PL/pgSQL, which run the body of the function. `connection/operator_bodies.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn operator_bodies() {
        big_stack(|| script(include_str!("connection/operator_bodies.test")));
    }

    /// `CREATE POLICY`, `ALTER POLICY`, `DROP POLICY` and the row security of tables: the rows of `pg_policy` and `pg_policies`, the errors of the statements, the dependencies, `DROP ROLE` and `DROP OWNED`. `connection/policies.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn policies() {
        big_stack(|| script(include_str!("connection/policies.test")));
    }

    /// Row security on the rows that a query reads: permissive and restrictive policies, the policy roles, views, the recursion error and the `row_security` setting. `connection/row_security_reads.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_reads() {
        big_stack(|| script(include_str!("connection/row_security_reads.test")));
    }

    /// Row security on the rows that `INSERT`, `UPDATE` and `DELETE` write: the conditions of `USING` on the rows of the table, the checks of `WITH CHECK` on the new rows, and the policies of `SELECT` for `RETURNING`. `connection/row_security_writes.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_writes() {
        big_stack(|| script(include_str!("connection/row_security_writes.test")));
    }

    /// The checks of the policies of row security after the triggers `BEFORE`, with a subquery, with a unique key, and on a partitioned table. `connection/row_security_write_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_write_checks() {
        big_stack(|| script(include_str!("connection/row_security_write_checks.test")));
    }

    /// Row-level security on `INSERT ... ON CONFLICT DO UPDATE` and `DO SELECT`: the checks of the proposed row, of the row that the table has and of the new row. `connection/row_security_upsert.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_upsert() {
        big_stack(|| script(include_str!("connection/row_security_upsert.test")));
    }

    /// Row-level security on `MERGE`: the policies of `SELECT` hide rows of the target table from the join, and each action checks the row of the target table and its new row. `connection/row_security_merge.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_merge() {
        big_stack(|| script(include_str!("connection/row_security_merge.test")));
    }

    /// Row-level security on `COPY`: `COPY TO` gives the rows that the policies let through, and `COPY FROM` is an error when the policies apply. `connection/row_security_copy.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_copy() {
        big_stack(|| script(include_str!("connection/row_security_copy.test")));
    }

    /// The system columns of tables that row-level security policies apply to, and the policies of the target tables of the statements of `WITH` that change data. `connection/row_security_system_columns.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_system_columns() {
        big_stack(|| script(include_str!("connection/row_security_system_columns.test")));
    }

    /// A prepared statement on a table with row security, which gets the policies of the current role and of the current `row_security` at each run. `connection/row_security_prepared.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_prepared() {
        big_stack(|| script(include_str!("connection/row_security_prepared.test")));
    }

    /// `EXPLAIN EXECUTE`, which shows the custom plan of a prepared statement with the values of its parameters. `connection/explain_execute.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_execute() {
        big_stack(|| script(include_str!("connection/explain_execute.test")));
    }

    /// The prepared statements after a change of the catalog: a plan that reads the new catalog, `0A000` for a plan that gives other columns, the errors of a table or a column that is gone, and the policies of a table that enables row security after `PREPARE`. `connection/prepared_statements_replan.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn prepared_statements_replan() {
        big_stack(|| script(include_str!("connection/prepared_statements_replan.test")));
    }

    /// The `Result` above the `Append` of a table with children of `INHERITS` that computes the targets of a `SELECT` and the new values of an `UPDATE`, and the plans of a partitioned table, of `ONLY` and of a `DELETE`, which have no such `Result`. `connection/explain_inheritance_result.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_inheritance_result() {
        big_stack(|| script(include_str!("connection/explain_inheritance_result.test")));
    }

    /// The plans of `EXPLAIN` for tables with row security, which scan the table with the conditions of the policies first and do not show a subquery. `connection/explain_row_security.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_row_security() {
        big_stack(|| script(include_str!("connection/explain_row_security.test")));
    }

    /// The policies that each command of a user runs, which tell their rows with a notice. `connection/row_security_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_checks() {
        big_stack(|| script(include_str!("connection/row_security_checks.test")));
    }

    /// The policies that each action of a `MERGE` runs on the source and the target, which tell their rows with a notice. `connection/row_security_merge_checks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_merge_checks() {
        big_stack(|| script(include_str!("connection/row_security_merge_checks.test")));
    }

    /// The detail of the error of a foreign key hides the key when the current user has no `SELECT` on the table or on each column of the key, or when the policies of the table apply, as `ri_ReportViolation` hides it. `connection/foreign_key_hidden_detail.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn foreign_key_hidden_detail() {
        big_stack(|| script(include_str!("connection/foreign_key_hidden_detail.test")));
    }

    /// The conditions of the policies of a table on the nullable side of an outer join remove the rows of the table before the join, so the join stays an outer join and `EXPLAIN` shows them on the scan of the table. A left join to such a table with a unique key leaves the plan. `connection/row_security_outer_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_security_outer_join() {
        big_stack(|| script(include_str!("connection/row_security_outer_join.test")));
    }

    /// The pages of the heap that `pg_relation_size` gives, `relpages` and `reltuples` after `ANALYZE`, `VACUUM`, `CREATE INDEX` and `TRUNCATE`, and the sizes of the scans that `EXPLAIN` shows from them. `connection/heap_size.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn heap_size() {
        big_stack(|| script(include_str!("connection/heap_size.test")));
    }

    /// `VACUUM` removes the dead versions, gives their slots and their space back through the free space map, truncates the empty pages at the end of the heap, and sets `relallvisible` and `relallfrozen` from the visibility map. `connection/heap_vacuum.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn heap_vacuum() {
        big_stack(|| script(include_str!("connection/heap_vacuum.test")));
    }

    /// `ANALYZE` writes the statistics of the columns, which `pg_stats` and `pg_statistic` show. `connection/analyze_stats.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn analyze_stats() {
        big_stack(|| script(include_str!("connection/analyze_stats.test")));
    }

    /// `EXPLAIN` of the scans of tables after `ANALYZE`: the rows of `=`, `<>`, `IS NULL` and the tests of a `boolean` come from the most common values, the null fraction and the number of distinct values in `pg_statistic`, and the widths of the columns come from `stawidth`. `connection/plan_statistics.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_statistics() {
        big_stack(|| script(include_str!("connection/plan_statistics.test")));
    }

    /// The estimates of `<`, `<=`, `>` and `>=` with a constant come from the most common values and the histogram of the column, and from the least and the greatest value of an index. `connection/plan_inequality.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_inequality() {
        big_stack(|| script(include_str!("connection/plan_inequality.test")));
    }

    /// The estimates of the joins of the columns with statistics, from the most common values of both sides for the inner joins, the outer joins, the semi joins and the anti joins, and the buckets of the hash joins. `connection/plan_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_join() {
        big_stack(|| script(include_str!("connection/plan_join.test")));
    }

    /// The costs of the index scans of analyzed tables, with the correlation of the first column of the index. Each index has one leaf page. `connection/plan_index_stats.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_index_stats() {
        big_stack(|| script(include_str!("connection/plan_index_stats.test")));
    }

    /// The `Memoize` node over a parameterized index scan of the inner side of a nested loop, with its keys, its mode and the estimates of the cache from the statistics of the tables. `connection/plan_memoize.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_memoize() {
        big_stack(|| script(include_str!("connection/plan_memoize.test")));
    }

    /// A notification goes to the other sessions of the database that listen on the channel, at their next `ReadyForQuery` outside of a transaction block, and not to a session of another database.
    #[test]
    fn notify_other_sessions() {
        big_stack(|| {
            let store = Arc::new(Store::new());
            let (mut a, _) = connect_to(&store, 11);
            let (mut b, _) = connect_to(&store, 12);
            let mut c = connect_database(&store, 13, "template1");
            for conn in [&mut b, &mut c] {
                assert_eq!(send(conn, &query("LISTEN ch")), ["LISTEN", "ready I"]);
            }
            assert_eq!(send(&mut b, &query("BEGIN")), ["BEGIN", "ready T"]);
            assert_eq!(send(&mut a, &query("NOTIFY ch, 'p'")), ["NOTIFY", "ready I"]);
            assert_eq!(
                send(&mut b, &query("SELECT 1")),
                ["columns ?column?", "row 1", "SELECT 1", "ready T"]
            );
            assert_eq!(send(&mut b, &query("COMMIT")), ["COMMIT", "notify ch p", "ready I"]);
            assert_eq!(
                send(&mut c, &query("SELECT 1")),
                ["columns ?column?", "row 1", "SELECT 1", "ready I"]
            );
        });
    }

    /// `ALTER DATABASE ... SET`: the checks of the names and the values, and the rows of `pg_db_role_setting`. `connection/alter_database_set.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_database_set() {
        big_stack(|| script(include_str!("connection/alter_database_set.test")));
    }

    /// A new session of a database takes the settings of `ALTER DATABASE ... SET` with the source `database`, and `RESET` goes back to them. A session of another database does not take them.
    #[test]
    fn database_settings() {
        big_stack(|| {
            let store = Arc::new(Store::new());
            let (mut a, _) = connect_to(&store, 1234);
            for sql in [
                "CREATE DATABASE x",
                "ALTER DATABASE x SET work_mem TO '16MB'",
                "ALTER DATABASE x SET my.thing TO 'v'",
            ] {
                assert!(!line_of(&mut a, sql, "").starts_with("ERROR"), "{sql}");
            }
            assert_eq!(line_of(&mut a, "SHOW work_mem", "row"), "row 4MB");
            let mut b = connect_database(&store, 1235, "x");
            assert_eq!(line_of(&mut b, "SHOW work_mem", "row"), "row 16MB");
            assert_eq!(line_of(&mut b, "SHOW my.thing", "row"), "row v");
            let source = "SELECT source FROM pg_settings WHERE name = 'work_mem'";
            assert_eq!(line_of(&mut b, source, "row"), "row database");
            line_of(&mut b, "SET work_mem TO '1MB'", "SET");
            line_of(&mut b, "RESET work_mem", "RESET");
            assert_eq!(line_of(&mut b, "SHOW work_mem", "row"), "row 16MB");
            let mut c = connect_database(&store, 1236, "postgres");
            assert_eq!(line_of(&mut c, "SHOW work_mem", "row"), "row 4MB");
        });
    }

    /// A session of a database other than `postgres`, with the settings of `ALTER DATABASE ... SET` for the database.
    fn connect_database(store: &Arc<Store>, pid: i32, database: &str) -> Connection {
        let start = Start { database: database.into(), ..start(None, &[]) };
        let stored = store.database_settings(store.admit(database).unwrap());
        let settings = session_settings(&base_settings(&[]).unwrap(), &start, &stored).unwrap();
        let mut connection = Connection::new(&start, settings, PROTOCOL_3_0);
        connection.set_store(Arc::clone(store));
        let mut out = OutBuf::new();
        connection.greet(&CancelKey::new(pid, PROTOCOL_3_0, [7; CANCEL_KEY_LEN]), &mut out);
        send(&mut connection, &[]);
        connection
    }

    /// The first line of the result of a query that starts with `prefix`.
    fn line_of(c: &mut Connection, sql: &str, prefix: &str) -> String {
        let lines = send(c, &query(sql));
        lines
            .iter()
            .find(|l| l.starts_with(prefix))
            .cloned()
            .unwrap_or_else(|| panic!("{sql}: {lines:?}"))
    }

    /// The encodings and the locales that the engine does not have, the catalog of each database, and `DROP DATABASE` of a database that another session uses, with and without `FORCE`.
    #[test]
    fn database_sessions() {
        big_stack(|| {
            let store = Arc::new(Store::new());
            let (mut a, _) = connect_to(&store, 1234);
            for (sql, message) in [
                (
                    "CREATE DATABASE x LOCALE 'en_US.utf8' TEMPLATE template0",
                    "locale \"en_US.utf8\" is not supported yet",
                ),
                (
                    "CREATE DATABASE x LC_CTYPE 'C.UTF-8' TEMPLATE template0",
                    "locale \"C.UTF-8\" is not supported yet",
                ),
                (
                    "CREATE DATABASE x ENCODING 'LATIN1' TEMPLATE template0",
                    "encoding \"LATIN1\" is not supported yet",
                ),
                (
                    "CREATE DATABASE x LOCALE_PROVIDER icu ICU_LOCALE 'en' TEMPLATE template0",
                    "the ICU locale provider is not supported yet",
                ),
                (
                    "CREATE DATABASE x LOCALE_PROVIDER builtin LOCALE 'C.UTF-8' TEMPLATE template0",
                    "locale \"C.UTF-8\" is not supported yet",
                ),
                (
                    "CREATE DATABASE x LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF8' TEMPLATE template0",
                    "builtin locale \"C.UTF-8\" is not supported yet",
                ),
            ] {
                assert_eq!(
                    line_of(&mut a, sql, "ERROR"),
                    format!("ERROR 0A000 {message}"),
                    "{sql}"
                );
            }
            assert_eq!(line_of(&mut a, "CREATE TABLE t1 (a int)", "CREATE"), "CREATE TABLE");
            assert_eq!(
                line_of(&mut a, "CREATE DATABASE x TEMPLATE postgres", "CREATE"),
                "CREATE DATABASE"
            );
            assert_eq!(line_of(&mut a, "CREATE DATABASE y", "CREATE"), "CREATE DATABASE");
            let mut b = connect_database(&store, 1235, "x");
            assert_eq!(
                line_of(&mut b, "SELECT current_database(), to_regclass('t1') IS NOT NULL", "row"),
                "row x,t"
            );
            assert_eq!(line_of(&mut b, "CREATE TABLE t2 (a int)", "CREATE"), "CREATE TABLE");
            assert_eq!(line_of(&mut a, "SELECT to_regclass('t2') IS NULL", "row"), "row t");
            assert_eq!(
                line_of(&mut a, "SELECT datname FROM pg_stat_activity WHERE pid = 1235", "row"),
                "row x"
            );
            assert_eq!(
                line_of(&mut b, "DROP DATABASE postgres", "ERROR"),
                "ERROR 0A000 DROP DATABASE of the database \"postgres\" of initdb is not supported yet"
            );
            let mut c = connect_database(&store, 1236, "y");
            assert_eq!(line_of(&mut c, "SELECT to_regclass('t1') IS NULL", "row"), "row t");
            assert_eq!(
                line_of(&mut b, "DROP DATABASE x", "ERROR"),
                "ERROR 55006 cannot drop the currently open database"
            );
            assert_eq!(
                line_of(&mut b, "CREATE DATABASE z TEMPLATE y", "ERROR"),
                "ERROR 55006 source database \"y\" is being accessed by other users"
            );
            assert_eq!(
                line_of(&mut c, "CREATE DATABASE z TEMPLATE y", "CREATE"),
                "CREATE DATABASE"
            );
            assert_eq!(
                line_of(&mut a, "DROP DATABASE x", "ERROR"),
                "ERROR 55006 database \"x\" is being accessed by other users"
            );
            assert_eq!(line_of(&mut a, "DROP DATABASE x WITH (FORCE)", "DROP"), "DROP DATABASE");
            let mut out = OutBuf::new();
            let step = b.step(&query("SELECT 1"), &mut out);
            assert!(matches!(step.next, Next::Close));
            assert_eq!(
                render(&out, false),
                ["FATAL 57P01 terminating connection due to administrator command"]
            );
            assert_eq!(
                line_of(
                    &mut a,
                    "SELECT count(*) FROM pg_database WHERE datname IN ('x', 'y', 'z')",
                    "row"
                ),
                "row 2"
            );
        });
    }

    /// Two sessions that look at each other in `pg_stat_activity`: the idle states, the wait for the client, the end of `xact_start` in a failed block, the snapshot of a transaction and `application_name`. `connection/two_sessions.test` is the output of PostgreSQL 19 for the same script on two connections to a new database.
    #[test]
    fn activity_of_two_sessions() {
        big_stack(|| two_sessions(include_str!("connection/two_sessions.test")));
    }

    /// `pg_sleep`, its sibling `pg_sleep_until` and the type `void`. `connection/sleep.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sleep() {
        big_stack(|| script(include_str!("connection/sleep.test")));
    }

    /// A session in `pg_sleep` while a second session looks at it, as the test `cancel` of `libpq_pipeline` does: the wait event `PgSleep` in `pg_stat_activity`, then a cancel that ends the wait with `57014` and clears the wait event.
    #[test]
    fn sleep_cancel() {
        big_stack(sleep_cancel_cases);
    }

    #[allow(clippy::disallowed_methods)] // Test threads, not tasks of the engine.
    fn sleep_cancel_cases() {
        let store = Arc::new(Store::new());
        let (flags, flag) = std::sync::mpsc::channel();
        let (outs, out) = std::sync::mpsc::channel();
        let (ends, end) = std::sync::mpsc::channel::<()>();
        let sleeper = Arc::clone(&store);
        // The session stays in the table until the end of the test.
        let a = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let (mut a, _) = connect_to(&sleeper, 1234);
                flags.send(a.cancel_flag()).unwrap();
                outs.send(send(&mut a, &query("SELECT pg_sleep(60)"))).unwrap();
                end.recv().unwrap();
            })
            .unwrap();
        let flag = flag.recv().unwrap();
        let (mut b, _) = connect_to(&store, 1235);
        let look = query(
            "SELECT state, wait_event_type, wait_event, query FROM pg_stat_activity WHERE pid = 1234",
        );
        let mut lines = Vec::new();
        for _ in 0..2000 {
            lines = send(&mut b, &look);
            if lines.iter().any(|l| l.contains("PgSleep")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(lines[1], "row active,Timeout,PgSleep,SELECT pg_sleep(60)");
        flag.store(true, Ordering::Relaxed);
        let out = out.recv().unwrap();
        assert_eq!(out.last().map(String::as_str), Some("ready I"));
        assert!(out.iter().any(|l| l == "ERROR 57014 canceling statement due to user request"));
        let lines = send(&mut b, &look);
        assert_eq!(lines[1], "row idle,Client,ClientRead,SELECT pg_sleep(60)");
        ends.send(()).unwrap();
        a.join().unwrap();
    }

    /// The properties of the access methods, of the btree and hash indexes and of their columns: the key columns and the `INCLUDE` column, the options `DESC` and `NULLS FIRST`, a built-in index, the names in upper case, and the null of an unknown name, of column 0, of a column out of range and of a relation that is not an index. `connection/am_properties.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn am_properties() {
        big_stack(|| script(include_str!("connection/am_properties.test")));
    }

    /// The advisory lock functions in one session: the counts of each mode and level, the warning of an unlock of a lock that the session does not hold, and the release of the locks of the transaction level at the end of the transaction and at an error. `connection/advisory.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn advisory() {
        big_stack(|| script(include_str!("connection/advisory.test")));
    }

    /// Advisory locks in two sessions: a wait with the wait event `advisory` that ends at the release, a deadlock that the session with the shorter `deadlock_timeout` finds, and the end of `lock_timeout`.
    #[test]
    fn advisory_wait() {
        big_stack(advisory_wait_cases);
    }

    /// `UPDATE` and `DELETE` in two sessions: a wait with the wait event `transactionid` that ends at the rollback or the commit of the other transaction, the second run of the statement after a commit, the serialization error of repeatable read, and the end of `lock_timeout`.
    #[test]
    fn row_wait() {
        big_stack(row_wait_cases);
    }

    /// The warning of a function in an `Execute` comes after the description and before the rows, as in the test `pipeline_idle` of `libpq_pipeline`.
    #[test]
    fn advisory_extended() {
        big_stack(|| {
            let (mut c, _) = connect();
            let input = [
                parse("", "SELECT pg_catalog.pg_advisory_unlock(1,1)", &[]),
                bind("", "", &[], &[], &[]),
                describe('P', ""),
                execute("", 0),
                sync(),
            ]
            .concat();
            assert_eq!(
                send(&mut c, &input),
                [
                    "ParseComplete",
                    "BindComplete",
                    "columns pg_advisory_unlock",
                    "WARNING 01000 you don't own a lock of type ExclusiveLock",
                    "row f",
                    "SELECT 1",
                    "ready I"
                ]
            );
        });
    }

    #[allow(clippy::disallowed_methods)] // Test threads, not tasks of the engine.
    fn row_wait_cases() {
        let store = Arc::new(Store::new());
        let (cmds, cmd) = std::sync::mpsc::channel::<String>();
        let (outs, out) = std::sync::mpsc::channel();
        let waiter = Arc::clone(&store);
        let b = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let (mut b, _) = connect_to(&waiter, 1235);
                while let Ok(text) = cmd.recv() {
                    outs.send(send(&mut b, &query(&text))).unwrap();
                }
            })
            .unwrap();
        let (mut a, _) = connect_to(&store, 1234);
        // A reads `pg_stat_activity` from a snapshot that lasts to the end of its transaction, so a third session looks at the wait.
        let (mut c, _) = connect_to(&store, 1236);
        let look = query(
            "SELECT state, wait_event_type, wait_event, query FROM pg_stat_activity WHERE pid = 1235",
        );
        let wait_for_b = |c: &mut Connection| {
            let mut lines = Vec::new();
            for _ in 0..2000 {
                lines = send(c, &look);
                if lines.iter().any(|l| l.contains(",Lock,transactionid,")) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            lines.swap_remove(1)
        };
        let has = |lines: &[String], line: &str| lines.iter().any(|l| l.starts_with(line));
        send(&mut a, &query("CREATE TABLE t (a int, b text)"));
        send(&mut a, &query("INSERT INTO t VALUES (1, 'x'), (2, 'y')"));

        // A rollback of the other transaction: the statement changes the old version.
        send(&mut a, &query("BEGIN"));
        send(&mut a, &query("DELETE FROM t WHERE a = 2"));
        cmds.send("UPDATE t SET b = 'z' WHERE a = 2".into()).unwrap();
        assert_eq!(
            wait_for_b(&mut c),
            "row active,Lock,transactionid,UPDATE t SET b = 'z' WHERE a = 2"
        );
        send(&mut a, &query("ROLLBACK"));
        let lines = out.recv().unwrap();
        assert!(has(&lines, "UPDATE 1"), "{lines:?}");

        // A commit of the other transaction: the statement runs again and changes the new version.
        send(&mut a, &query("BEGIN"));
        send(&mut a, &query("UPDATE t SET b = b || 'a' WHERE a = 1"));
        cmds.send("UPDATE t SET b = b || 'b' WHERE a = 1".into()).unwrap();
        assert_eq!(
            wait_for_b(&mut c),
            "row active,Lock,transactionid,UPDATE t SET b = b || 'b' WHERE a = 1"
        );
        send(&mut a, &query("COMMIT"));
        let lines = out.recv().unwrap();
        assert!(has(&lines, "UPDATE 1"), "{lines:?}");
        let lines = send(&mut a, &query("SELECT b FROM t ORDER BY a"));
        assert_eq!(lines[1..3], ["row xab", "row z"]);

        // Repeatable read: a commit of the other transaction is a serialization error.
        cmds.send("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM t".into()).unwrap();
        out.recv().unwrap();
        send(&mut a, &query("BEGIN"));
        send(&mut a, &query("UPDATE t SET b = 'c' WHERE a = 2"));
        cmds.send("DELETE FROM t WHERE a = 2".into()).unwrap();
        assert_eq!(wait_for_b(&mut c), "row active,Lock,transactionid,DELETE FROM t WHERE a = 2");
        send(&mut a, &query("COMMIT"));
        let lines = out.recv().unwrap();
        assert!(
            has(&lines, "ERROR 40001 could not serialize access due to concurrent update"),
            "{lines:?}"
        );
        cmds.send("ROLLBACK".into()).unwrap();
        out.recv().unwrap();

        // The end of lock_timeout.
        send(&mut a, &query("BEGIN"));
        send(&mut a, &query("UPDATE t SET b = 'g' WHERE a = 1"));
        cmds.send("SET lock_timeout = 20; DELETE FROM t WHERE a = 1".into()).unwrap();
        let lines = out.recv().unwrap();
        let context = "(waiting for ShareLock on transaction ";
        assert!(
            has(&lines, &format!("ERROR 55P03 canceling statement due to lock timeout {context}")),
            "{lines:?}"
        );
        send(&mut a, &query("ROLLBACK"));
        let lines = send(&mut a, &query("SELECT b FROM t ORDER BY a"));
        assert_eq!(lines[1..3], ["row xab", "row c"]);

        // A key that a transaction in progress inserted: the insert waits, and then adds its row after a rollback or fails after a commit.
        send(&mut a, &query("CREATE TABLE k (a int PRIMARY KEY)"));
        for (end, result) in [("ROLLBACK", "INSERT 0 1"), ("COMMIT", "ERROR 23505")] {
            send(&mut a, &query("BEGIN"));
            send(&mut a, &query("INSERT INTO k VALUES (1)"));
            cmds.send("SET lock_timeout = 0; INSERT INTO k VALUES (1)".into()).unwrap();
            assert_eq!(
                wait_for_b(&mut c),
                "row active,Lock,transactionid,SET lock_timeout = 0; INSERT INTO k VALUES (1)"
            );
            send(&mut a, &query(end));
            let lines = out.recv().unwrap();
            assert!(has(&lines, result), "{lines:?}");
            if end == "ROLLBACK" {
                send(&mut a, &query("DELETE FROM k"));
            }
        }
        drop(cmds);
        b.join().unwrap();
    }

    #[allow(clippy::disallowed_methods)] // Test threads, not tasks of the engine.
    fn advisory_wait_cases() {
        let store = Arc::new(Store::new());
        let (cmds, cmd) = std::sync::mpsc::channel::<String>();
        let (outs, out) = std::sync::mpsc::channel();
        let waiter = Arc::clone(&store);
        let b = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let (mut b, _) = connect_to(&waiter, 1235);
                while let Ok(text) = cmd.recv() {
                    outs.send(send(&mut b, &query(&text))).unwrap();
                }
            })
            .unwrap();
        let (mut a, _) = connect_to(&store, 1234);
        let look = query(
            "SELECT state, wait_event_type, wait_event, query FROM pg_stat_activity WHERE pid = 1235",
        );
        let wait_for_b = |a: &mut Connection| {
            let mut lines = Vec::new();
            for _ in 0..2000 {
                lines = send(a, &look);
                // The query of the idle row also has the word "advisory", so the wait looks for the wait event.
                if lines.iter().any(|l| l.contains(",Lock,advisory,")) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            lines.swap_remove(1)
        };
        send(&mut a, &query("SELECT pg_advisory_lock(1), pg_advisory_lock(2)"));
        cmds.send("SELECT pg_advisory_lock(1)".into()).unwrap();
        assert_eq!(wait_for_b(&mut a), "row active,Lock,advisory,SELECT pg_advisory_lock(1)");
        let lines = send(&mut a, &query("SELECT pg_try_advisory_lock(1)"));
        assert_eq!(lines[1], "row t");
        send(&mut a, &query("SELECT pg_advisory_unlock(1), pg_advisory_unlock(1)"));
        let lines = out.recv().unwrap();
        assert_eq!(lines[1..], ["row ", "SELECT 1", "ready I"]);

        cmds.send("SELECT pg_advisory_lock(2)".into()).unwrap();
        assert_eq!(wait_for_b(&mut a), "row active,Lock,advisory,SELECT pg_advisory_lock(2)");
        send(&mut a, &query("SET deadlock_timeout = 10"));
        let lines = send(&mut a, &query("SELECT pg_advisory_lock(1)"));
        assert!(lines.iter().any(|l| l == "ERROR 40P01 deadlock detected"), "{lines:?}");
        send(&mut a, &query("SELECT pg_advisory_unlock_all()"));
        let lines = out.recv().unwrap();
        assert_eq!(lines[1..], ["row ", "SELECT 1", "ready I"]);

        send(&mut a, &query("SET lock_timeout = 20"));
        let lines = send(&mut a, &query("SELECT pg_advisory_xact_lock(2)"));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("ERROR 55P03 canceling statement due to lock timeout")),
            "{lines:?}"
        );
        let lines = send(&mut a, &query("SELECT pg_try_advisory_lock(1), pg_try_advisory_lock(3)"));
        assert_eq!(lines[1], "row f,t");
        drop(cmds);
        b.join().unwrap();
        let lines = send(&mut a, &query("SELECT pg_try_advisory_lock(1), pg_try_advisory_lock(2)"));
        assert_eq!(lines[1], "row t,t");
    }

    /// The fold of the constant parts of the expressions, as `eval_const_expressions` does it. An error of the fold comes before the description of the rows, and a part that the fold drops gives no error. `connection/fold.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn fold() {
        big_stack(|| script(include_str!("connection/fold.test")));
    }

    /// `CREATE VIEW` and `CREATE OR REPLACE VIEW`, the queries that read the views, and the rows of the views in `pg_class`, `pg_type`, `pg_attribute`, `pg_rewrite` and `pg_depend`. `connection/views.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn views() {
        big_stack(|| script(include_str!("connection/views.test")));
    }

    /// The views that `system_views.sql` makes in `pg_catalog` when the cluster starts: their rows in `pg_class`, `pg_type`, `pg_rewrite` and `pg_attribute`, with the privileges and the options, and the queries that read them. The script also shows the options of `CREATE VIEW` and their errors, and `row_security_active` and `pg_get_statisticsobjdef_expressions`, which the view `pg_stats_ext_exprs` calls. `connection/system_views.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn system_views() {
        big_stack(|| script(include_str!("connection/system_views.test")));
    }

    /// `pg_show_all_settings` and the view `pg_settings`: the columns of each parameter, the source and the reset value after `SET`, `SET LOCAL`, `SET TRANSACTION`, `set_config`, `RESET` and `RESET ALL`, and the source of the modes of the transaction after a commit and a rollback. The script reads only parameters that the configuration of the oracle does not set. `connection/show_all_settings.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn show_all_settings() {
        big_stack(|| script(include_str!("connection/show_all_settings.test")));
    }

    /// The views `pg_stat_*` and `pg_statio_*` of tables, indexes, sequences, functions and database conflicts: the columns and their types, a row for each object, and the functions `pg_stat_get_*` for an object with no entry. The counters of PostgreSQL change when the backend sends its counts, so the script tests only that each counter is not negative. `connection/statistics.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn statistics() {
        big_stack(|| script(include_str!("connection/statistics.test")));
    }

    /// The sequential scans of the tables in `pg_stat_user_tables`: the build of each index, the scan of `ALTER TABLE` that checks new constraints, the check of a new foreign key and the read of a table, the counts of a transaction that rolls back, and `track_counts`. `connection/scans.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn scans() {
        big_stack(|| script(include_str!("connection/scans.test")));
    }

    /// The counts of the rows that the transactions wrote in `pg_stat_user_tables` and `pg_stat_xact_user_tables`: `INSERT`, `UPDATE`, `DELETE`, a rollback, a rollback to a savepoint, `TRUNCATE` with a commit and with a rollback, `ON CONFLICT`, `MERGE`, the partitions, a rewrite of the table, `track_counts`, `CREATE TABLE AS` and `COPY FROM`. `connection/table_counts.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn table_counts() {
        big_stack(|| script(include_str!("connection/table_counts.test")));
    }

    /// `SELECT` from the tables and sequences of the user, with the types of the columns. `connection/user_select.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn user_select() {
        big_stack(|| script(include_str!("connection/user_select.test")));
    }

    /// Subqueries in `FROM`: the names and the aliases of their columns, the errors of the names, and the order of the errors of the fold. A subquery that the planner pulls up folds where the query reads its columns, and a column that the query does not read does not run. `connection/from_subquery.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn from_subquery() {
        big_stack(|| script(include_str!("connection/from_subquery.test")));
    }

    /// Functions in `FROM`: `generate_series`, `unnest`, `ROWS FROM`, `WITH ORDINALITY`, the functions with `OUT` parameters, a function of a scalar type, a function that reads the parts of `FROM` before it, and the errors of the column definition lists. `connection/from_function.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn from_function() {
        big_stack(|| script(include_str!("connection/from_function.test")));
    }

    /// `UNION`, `INTERSECT` and `EXCEPT` with and without `ALL`: the types of the columns, the order of the rows, `ORDER BY` and `LIMIT` on the result, a set operation in a subquery, and the errors of the analysis. `connection/set_operation.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn set_operation() {
        big_stack(|| script(include_str!("connection/set_operation.test")));
    }

    /// `VALUES` lists: as a query with `ORDER BY`, `LIMIT` and `OFFSET`, in `FROM` with column aliases, in a `LATERAL` subquery, in `IN`, in a scalar subquery and in a set operation, the column types and typmods, and the errors of the analysis. `connection/values.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn values() {
        big_stack(|| script(include_str!("connection/values.test")));
    }

    /// `LATERAL` subqueries in `FROM`, which run for each row of the relations before them, in a list, in inner and outer joins and in a subquery. `connection/lateral.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn lateral() {
        big_stack(|| script(include_str!("connection/lateral.test")));
    }

    /// Casts of arrays that cast each element, as `ArrayCoerceExpr` does: `text[]` to `name[]`, casts through the text form, length casts of the elements, and arrays with more than one dimension or other lower bounds. `connection/array_coerce.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_coerce() {
        big_stack(|| script(include_str!("connection/array_coerce.test")));
    }

    /// The functions of the arrays: `string_to_array` with and without the null string, `array_to_string`, `array_length`, `array_lower`, `array_upper`, `array_ndims`, `array_dims`, `cardinality`, `array_append`, `array_prepend`, `array_cat` and the `||` operators of the arrays, with null arguments, empty arrays, other lower bounds and arrays with two dimensions. `connection/array_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_functions() {
        big_stack(|| script(include_str!("connection/array_functions.test")));
    }

    /// The subscripts of the arrays, of `int2vector` and of `oidvector`: one element, slices with and without bounds, arrays with two dimensions, null and out of range subscripts, the errors of the analyzer, the default expressions that `pg_get_expr` shows, and the types and typmods of the columns of a view. `connection/subscripts.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn subscripts() {
        big_stack(|| script(include_str!("connection/subscripts.test")));
    }

    /// The regular expression operators `~`, `~*`, `!~` and `!~*` on `text`, `name` and `character`: the anchors, the bounds, the brackets with classes and collating elements, the escapes, the embedded options, the back references, the lookarounds, the newline modes, the queries of `psql` on the names of the catalog, long strings, and each error of the compiler. `connection/regex.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn regex() {
        big_stack(|| script(include_str!("connection/regex.test")));
    }

    /// The functions of regular expressions that give the text of a match: `substring` with a pattern of POSIX and of SQL, `similar_to_escape`, `similar_escape`, `SIMILAR TO`, `regexp_match` and `regexp_like`. The cases include the greedy and the lazy quantifiers, the groups that do not match, the groups in iterations, the back references, the options and each error. `connection/regex_functions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn regex_functions() {
        big_stack(|| script(include_str!("connection/regex_functions.test")));
    }

    /// The functions of regular expressions that find each match: `regexp_replace` in each form with the replacement escapes, `regexp_count`, `regexp_instr`, `regexp_substr`, `regexp_matches` and `regexp_split_to_table` in `FROM`, and `regexp_split_to_array`. The cases include empty matches, a start after the first character, the characters of more than one byte, each error and a view. `connection/regex_matches.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn regex_matches() {
        big_stack(|| script(include_str!("connection/regex_matches.test")));
    }

    /// The functions that give a set in the target list, as the nodes `ProjectSet` of PostgreSQL run them: calls side by side with nulls for the shorter sets, nested calls, calls inside expressions, the rows with no values, the calls above aggregates and below `ORDER BY`, `DISTINCT` and `LIMIT`, subqueries, row values and each error of a call in a wrong place. `connection/target_srf.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn target_srf() {
        big_stack(|| script(include_str!("connection/target_srf.test")));
    }

    /// The fields of a value of type `record`, as `(x).name` reads them: the `OUT` parameters of a call, the columns of a subquery through more subqueries, `LATERAL` and `UNION ALL`, a whole row with the names of its alias, the views that show such fields, and the errors of a field that is not there and of a view column of type `record`. `connection/record_fields.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn record_fields() {
        big_stack(|| script(include_str!("connection/record_fields.test")));
    }

    /// `(x).*` in a target list, for a call of a function that gives a set, a whole row and a view, the row comparisons of `=` and `<>` with their kinds from the btree operator families and their errors, and `acldefault` for each kind of object. `connection/row_star.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_star() {
        big_stack(|| script(include_str!("connection/row_star.test")));
    }

    /// The functions of `information_schema` with a query in the body: `_pg_expandarray` with a query in a string and polymorphic `OUT` parameters, and `_pg_index_position` with `BEGIN ATOMIC`. The script calls them in `FROM`, in the target list and with `LATERAL`, also with null and empty arrays, and reads their rows of `pg_proc` and `pg_depend`. `connection/sql_bodies.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sql_bodies() {
        big_stack(|| script(include_str!("connection/sql_bodies.test")));
    }

    /// `format_type` of `interval` with each range and precision that a typmod can have, the error for a bad range, `_pg_interval_type` of `information_schema`, and the interval types of a view in `pg_get_viewdef` and `pg_attribute`. `connection/interval_typmods.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn interval_typmods() {
        big_stack(|| script(include_str!("connection/interval_typmods.test")));
    }

    /// The window functions: `row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value` and `nth_value` with the default frame, the `WINDOW` clause and windows that copy a window, the order of the rows after the sorts of the windows, the errors of the analyzer and of the planner, and views with windows in `pg_get_viewdef`. `connection/windows.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn windows() {
        big_stack(|| script(include_str!("connection/windows.test")));
    }

    /// The aggregate functions with `OVER` and the default frame: running results with `ORDER BY`, results for the whole partition with no `ORDER BY`, peers, `FILTER`, the aggregates of numbers, of text, of arrays, of JSON, of intervals and of ranges, a view with `pg_get_viewdef`, and the errors of the analyzer. `connection/window_agg.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn window_agg() {
        big_stack(|| script(include_str!("connection/window_agg.test")));
    }

    /// The window functions in a query with `GROUP BY`, `HAVING` or an aggregate call: windows that read the groups after `HAVING`, window functions of aggregate calls, windows that sort on aggregate calls, a view with `pg_get_viewdef`, and the errors of the analyzer. `connection/window_group.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn window_group() {
        big_stack(|| script(include_str!("connection/window_group.test")));
    }

    /// The frames of the windows: `ROWS`, `RANGE` and `GROUPS` mode with offsets, the clause `EXCLUDE`, `first_value`, `last_value` and `nth_value` in a frame, the `in_range` functions of the integer, float, numeric, date, time and interval types with their infinite values, the errors of the offsets, a view with `pg_get_viewdef`, and the errors of the analyzer. `connection/window_frame.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn window_frame() {
        big_stack(|| script(include_str!("connection/window_frame.test")));
    }

    /// The executor of `jsonpath`: `jsonb_path_exists`, `jsonb_path_match`, `jsonb_path_query`, `jsonb_path_query_array` and `jsonb_path_query_first`, the operators `@?` and `@@`, the lax and the strict modes, the accessors, the filters and the predicates, the arithmetic, the variables, the item methods but the datetime methods, the ids of `.keyvalue()`, the silent calls, and the errors. `connection/jsonpath_exec.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn jsonpath_exec() {
        big_stack(|| script(include_str!("connection/jsonpath_exec.test")));
    }

    /// The datetime methods of `jsonpath`: `.datetime()` with the ISO formats and with a template, the errors of a template in the strict mode, `.date()`, `.time()`, `.time_tz()`, `.timestamp()` and `.timestamp_tz()` with a precision, the warnings for a precision that is too large, the casts between the types and the errors without the time zone, the `_tz` functions, `.type()` and `.string()` of a datetime, the comparisons of the datetime types, and the silent calls. `connection/jsonpath_datetime.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn jsonpath_datetime() {
        big_stack(|| script(include_str!("connection/jsonpath_datetime.test")));
    }

    /// A row on the left of `IN`, `ANY` and `ALL` with a subquery: one operator for each column of the subquery that `AND` or `OR` combines, the NULL results, the errors for a wrong number of columns and for an operator that does not give `boolean`, a view with these tests in `pg_get_viewdef`, and the same results when a hash table of the subquery gives them. `connection/row_in_subquery.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn row_in_subquery() {
        big_stack(|| script(include_str!("connection/row_in_subquery.test")));
    }

    /// The views and the tables that `information_schema.sql` makes when the cluster starts: their rows in `pg_class`, `pg_type`, `pg_attribute`, `pg_rewrite` and `pg_depend` and the text of their queries, then the views that read a new table, a new view and a foreign key,, the domains of the schema and their checks. `connection/information_schema.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn information_schema() {
        big_stack(|| script(include_str!("connection/information_schema.test")));
    }

    /// `nameconcatoid` and `getdatabaseencoding`, and the views of `information_schema` that call them: the character sets, the collations of each character set, the routines with their parameters and privileges, and the usage views of the routines. `nameconcatoid` cuts a long name at a character boundary and keeps the OID whole. `connection/routine_views.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn routine_views() {
        big_stack(|| script(include_str!("connection/routine_views.test")));
    }

    /// The functions and catalog rows that the queries of `pg_dump` read in a new database: `pg_is_in_recovery`, `array_remove` and `array_replace` with arrays of each shape, a subscript of a `name`, which is an array of `"char"`, and the rows of `pg_init_privs` for the built-in objects. `connection/dump_queries.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn dump_queries() {
        big_stack(|| script(include_str!("connection/dump_queries.test")));
    }

    /// `pg_get_sequence_data`, which `pg_dump` calls for each row of `pg_sequence`: the sequences of `serial` columns, a call in `FROM` and in the list of the targets, and the row of nulls for a table, a catalog, an OID that no relation has and a null. `connection/sequence_data.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sequence_data() {
        big_stack(|| script(include_str!("connection/sequence_data.test")));
    }

    /// `LOCK TABLE` in the modes up to `ROW EXCLUSIVE`, which `pg_dump` uses: the error outside a transaction block, a table, a view, a view with `WITH`, a catalog and a built-in view, `ONLY`, `*` and `NOWAIT`, a name that does not exist, a schema that does not exist, an index, a sequence and an index of the catalog, a `Query` with more than one statement, and the extended protocol. `connection/lock_table.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn lock_table() {
        big_stack(|| script(include_str!("connection/lock_table.test")));
    }

    /// `COMMENT ON` for each kind of object that rupg has: a schema, a table, a view, an index, a sequence, a column, a constraint of a table and a type, the rows of `pg_description`, a change and a drop of a comment, the errors for a wrong kind of object and for names that do not exist, a column of an index and of a sequence, names from `search_path`, and a rollback. `connection/comment.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn comment() {
        big_stack(|| script(include_str!("connection/comment.test")));
    }

    /// `COPY ... TO STDOUT` of a table, of a list of columns and of a query, in the text, CSV and binary formats, with the old and the new forms of the options, the errors of the names, of the kind of the relation and of each option, a file, a program and the extended protocol. `connection/copy_to.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn copy_to() {
        big_stack(|| script(include_str!("connection/copy_to.test")));
    }

    /// `COPY ... FROM` a file of the server: the rows of the file in the simple and the extended protocol, an error of a line with the context of the line, and a file that does not exist. A session with no server cannot read files.
    #[test]
    fn copy_from_file() {
        use rupg_platform::{OpenMode, sim::SimIo};
        big_stack(|| {
            let (mut c, _) = connect();
            assert_eq!(script_lines(&mut c, "> CREATE TABLE t (a int, b text)"), ["CREATE TABLE"]);
            assert_eq!(
                script_lines(&mut c, "> COPY t FROM '/data/t.data'"),
                ["ERROR 0A000 COPY FROM a file in a session with no server is not supported yet"]
            );
            let io = SimIo::new(1);
            let file = io.open(std::path::Path::new("/data/t.data"), OpenMode::Create).unwrap();
            file.write_at(0, b"1\tone\n2\ttwo\n").unwrap();
            let file = io.open(std::path::Path::new("/data/bad.data"), OpenMode::Create).unwrap();
            file.write_at(0, b"3\tthree\nx\tfour\n").unwrap();
            c.set_io(Arc::new(io));
            assert_eq!(script_lines(&mut c, "> COPY t FROM '/data/t.data'"), ["COPY 2"]);
            assert!(script_lines(&mut c, "P s|COPY t (a, b) FROM '/data/t.data'").is_empty());
            assert_eq!(script_lines(&mut c, "X s"), ["COPY 2"]);
            assert_eq!(
                script_lines(&mut c, "> COPY t FROM '/data/bad.data'"),
                [
                    "ERROR 22P02 invalid input syntax for type integer: \"x\" (COPY t, line 2, column a: \"x\")"
                ]
            );
            assert_eq!(
                script_lines(&mut c, "> COPY t FROM '/data/none.data'"),
                ["ERROR 58P01 could not open file \"/data/none.data\" for reading: no such file"]
            );
            let rows =
                script_lines(&mut c, "> SELECT a, b, count(*) FROM t GROUP BY a, b ORDER BY a");
            assert_eq!(rows[1..], ["row 1,one,2", "row 2,two,2", "SELECT 2"]);
        });
    }

    /// `COPY FROM` a program, an encoding other than `UTF8` and `COPY TO` a file or a program give `0A000`.
    #[test]
    fn copy_not_supported() {
        big_stack(|| {
            let (mut c, _) = connect();
            assert_eq!(script_lines(&mut c, "> CREATE TABLE t (a int)"), ["CREATE TABLE"]);
            for (sql, error) in [
                ("COPY t FROM PROGRAM 'cat'", "COPY FROM a program is not supported yet"),
                (
                    "COPY t TO stdout (ENCODING 'latin1')",
                    "COPY with ENCODING LATIN1 is not supported yet",
                ),
                ("COPY t TO '/tmp/rupg-copy.txt'", "COPY to a file is not supported yet"),
                ("COPY t TO PROGRAM 'cat'", "COPY to a program is not supported yet"),
            ] {
                let want = format!("ERROR 0A000 {error}");
                assert_eq!(script_lines(&mut c, &format!("> {sql}")), [want], "{sql}");
            }
        });
    }

    /// A comment on a built-in object, on a system column and on an object of another kind gives `0A000`, and the built-in comment stays.
    #[test]
    fn comment_not_supported() {
        big_stack(|| {
            let (mut c, _) = connect();
            assert_eq!(script_lines(&mut c, "> CREATE TABLE t (a int)"), ["CREATE TABLE"]);
            let builtin = "ERROR 0A000 COMMENT on a built-in object is not supported yet";
            for sql in [
                "> COMMENT ON TABLE pg_class IS 'x'",
                "> COMMENT ON COLUMN pg_class.relname IS 'x'",
                "> COMMENT ON SCHEMA public IS 'x'",
                "> COMMENT ON TYPE int4 IS 'x'",
            ] {
                assert_eq!(script_lines(&mut c, sql), [builtin], "{sql}");
            }
            assert_eq!(
                script_lines(&mut c, "> COMMENT ON COLUMN t.ctid IS 'x'"),
                ["ERROR 0A000 COMMENT on a system column is not supported yet"]
            );
            assert_eq!(script_lines(&mut c, "> COMMENT ON FUNCTION lower(text) IS 'x'"), [builtin]);
            assert_eq!(
                script_lines(
                    &mut c,
                    "> SELECT obj_description('public'::regnamespace, 'pg_namespace')"
                ),
                ["fields obj_description:25:-1:-1", "row standard public schema", "SELECT 1"]
            );
        });
    }

    /// `LOCK TABLE` in two sessions with `NOWAIT`: the modes that conflict and the modes that do not, the name with its schema in the error, the children of a table with and without `ONLY`, the tables that a view reads, and the release at the end of the transaction and at the rollback to a savepoint. `connection/lock_conflicts.test` is the output of PostgreSQL 19 for the same script on two connections to a new database.
    #[test]
    fn lock_conflicts() {
        big_stack(|| two_sessions(include_str!("connection/lock_conflicts.test")));
    }

    /// `LOCK TABLE` in two sessions that wait: a wait with the wait event `relation` that ends at the commit of the other session, a deadlock that the session with the shorter `deadlock_timeout` finds, and the end of `lock_timeout` with the lock in the context.
    #[test]
    fn lock_wait() {
        big_stack(lock_wait_cases);
    }

    #[allow(clippy::disallowed_methods)] // Test threads, not tasks of the engine.
    fn lock_wait_cases() {
        let store = Arc::new(Store::new());
        let (cmds, cmd) = std::sync::mpsc::channel::<String>();
        let (outs, out) = std::sync::mpsc::channel();
        let waiter = Arc::clone(&store);
        let b = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let (mut b, _) = connect_to(&waiter, 1235);
                while let Ok(text) = cmd.recv() {
                    outs.send(send(&mut b, &query(&text))).unwrap();
                }
            })
            .unwrap();
        let (mut a, _) = connect_to(&store, 1234);
        // A reads `pg_stat_activity` from a snapshot that lasts to the end of its transaction, so a third session looks at the wait.
        let (mut c, _) = connect_to(&store, 1236);
        let look = query(
            "SELECT state, wait_event_type, wait_event, query FROM pg_stat_activity WHERE pid = 1235",
        );
        let wait_for_b = |c: &mut Connection| {
            let mut lines = Vec::new();
            for _ in 0..2000 {
                lines = send(c, &look);
                if lines.iter().any(|l| l.contains(",Lock,relation,")) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            lines.swap_remove(1)
        };
        send(&mut a, &query("CREATE TABLE t (a int); CREATE TABLE u (a int)"));
        send(&mut a, &query("BEGIN; LOCK t IN SHARE UPDATE EXCLUSIVE MODE"));
        cmds.send("BEGIN; LOCK t IN SHARE UPDATE EXCLUSIVE MODE".into()).unwrap();
        let want = "row active,Lock,relation,BEGIN; LOCK t IN SHARE UPDATE EXCLUSIVE MODE";
        assert_eq!(wait_for_b(&mut c), want);
        send(&mut a, &query("COMMIT"));
        let lines = out.recv().unwrap();
        assert_eq!(lines, ["BEGIN", "LOCK TABLE", "ready T"]);

        send(&mut a, &query("BEGIN; LOCK u"));
        cmds.send("LOCK u IN ACCESS SHARE MODE".into()).unwrap();
        assert_eq!(wait_for_b(&mut c), "row active,Lock,relation,LOCK u IN ACCESS SHARE MODE");
        send(&mut a, &query("SET deadlock_timeout = 10"));
        let lines = send(&mut a, &query("LOCK t IN SHARE MODE"));
        assert!(lines.iter().any(|l| l == "ERROR 40P01 deadlock detected"), "{lines:?}");
        assert_eq!(out.recv().unwrap(), ["LOCK TABLE", "ready T"]);
        send(&mut a, &query("ROLLBACK"));
        let lines = send(&mut a, &query("SELECT 't'::regclass::oid"));
        let oid = lines[1].strip_prefix("row ").unwrap().to_owned();
        send(&mut a, &query("BEGIN; SET LOCAL lock_timeout = 10"));
        let lines = send(&mut a, &query("LOCK t IN SHARE MODE"));
        let want = format!(
            "ERROR 55P03 canceling statement due to lock timeout (waiting for ShareLock on relation {oid} of database "
        );
        assert!(lines.iter().any(|l| l.starts_with(&want)), "{lines:?}");
        send(&mut a, &query("ROLLBACK"));
        cmds.send("COMMIT".into()).unwrap();
        assert_eq!(out.recv().unwrap(), ["COMMIT", "ready I"]);
        drop(cmds);
        b.join().unwrap();
    }

    /// Correlated subqueries that find their rows through an `=` of a column and a column of the outer query: in `EXISTS` and `NOT EXISTS`, in the target list, in `LATERAL`, in a join in the subquery, in a subquery in a subquery, with a null outer value, and with an outer value of another type of the same form. The first statement is the query of `pg_dump` that finds the functions to dump. `connection/correlated_lookups.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn correlated_lookups() {
        big_stack(|| script(include_str!("connection/correlated_lookups.test")));
    }

    /// Subqueries in the conditions of a scan of a subquery and of a join, which each give their own rows, and the views of `information_schema` that read `conkey` of the built-in and the user constraints. `connection/subquery_filters.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn subquery_filters() {
        big_stack(|| script(include_str!("connection/subquery_filters.test")));
    }

    /// `pg_relation_is_updatable` and `pg_column_is_updatable` for tables, for views that the database can update and for views that it cannot update, for views on views, for the built-in relations and for the views of `information_schema` that call them. `connection/updatable.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn updatable() {
        big_stack(|| script(include_str!("connection/updatable.test")));
    }

    /// `format` with `%s`, `%I`, `%L` and `%%`, with the positions of the arguments, the widths and the flag `-`, with `VARIADIC` arrays and null values, its errors, and the view `check_constraints` of `information_schema`, which calls it. `connection/format.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn format() {
        big_stack(|| script(include_str!("connection/format.test")));
    }

    /// `pg_get_ruledef` for the `_RETURN` rules of user views and of the system views, with and without the pretty flags, `pg_get_triggerdef`, and the views `pg_rules`, `triggers` and `triggered_update_columns`. `connection/ruledef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ruledef() {
        big_stack(|| script(include_str!("connection/ruledef.test")));
    }

    /// `CREATE RULE` and `DROP RULE` on a table and a view: `pg_rewrite`, `pg_rules`, `pg_get_ruledef`, the rows of `pg_depend`, `relhasrules`, `pg_relation_is_updatable`, the rules `pg_settings_u` and `pg_settings_n`, and the errors of `transformRuleStmt` and `DefineQueryRewrite`. `connection/rules.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn rules() {
        big_stack(|| script(include_str!("connection/rules.test")));
    }

    /// The rules `ON INSERT`, `ON UPDATE` and `ON DELETE` of a table and of a view: the actions `DO ALSO` and `DO INSTEAD` with `NEW` and `OLD`, a condition, the inverse condition on the statement for a rule `DO INSTEAD` with a condition, the command tags, and the errors for `RETURNING`, `ON CONFLICT`, `MERGE` and a rule that fires itself again. `connection/rules2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn rules2() {
        big_stack(|| script(include_str!("connection/rules2.test")));
    }

    /// `CREATE TRIGGER`, `DROP TRIGGER`, `ALTER TRIGGER ... RENAME`, `ALTER TABLE ... ENABLE TRIGGER` and `COMMENT ON TRIGGER`: `pg_trigger`, `pg_get_triggerdef`, the rows of `pg_depend`, the views `triggers` and `triggered_update_columns`, and the errors of `CreateTriggerFiringOn`, `renametrig` and `EnableDisableTrigger`. `connection/triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn triggers() {
        big_stack(|| script(include_str!("connection/triggers.test")));
    }

    /// The calls of the triggers of `CREATE TRIGGER` in PL/pgSQL: the order of the triggers `BEFORE` and `AFTER` for each statement and for each row, the variables `TG_*`, `NEW` and `OLD`, a row that a trigger changes or skips, the checks after the triggers, `WHEN`, `UPDATE OF`, a disabled trigger, and the errors of a trigger function. `connection/trigger_fire.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn trigger_fire() {
        big_stack(|| script(include_str!("connection/trigger_fire.test")));
    }

    /// `DO` in PL/pgSQL: the messages and errors of the code with the statement as their context, a `DO` in a function, a `DO` in a `DO`, the writes of the code and their rollback in a block with `EXCEPTION`, `FOUND` and `ROW_COUNT` after `DO`, and the errors of the options of `DO`. `connection/plpgsql_do.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_do() {
        big_stack(|| script(include_str!("connection/plpgsql_do.test")));
    }

    /// `CREATE PROCEDURE` and `CALL`: input, `INOUT` and `OUT` arguments, named arguments, defaults and `VARIADIC`, the errors of a call of a function with `CALL` and of a procedure with `SELECT`, `RETURN` in a procedure, the options that a procedure cannot have, `CREATE OR REPLACE` that changes the kind, and `DROP`, `DROP ROUTINE` and `ALTER ... RENAME` with the checks of the kind. `connection/procedures.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn procedures() {
        big_stack(|| script(include_str!("connection/procedures.test")));
    }

    /// `CALL` in PL/pgSQL: the values of `INOUT` and `OUT` arguments go back to the variables and to the fields of records, with named arguments and defaults, a `CALL` in a procedure and in a function, and the errors of an argument that is not a variable, of a `CONSTANT` and of a cast. `FOUND` does not change. `connection/plpgsql_call.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_call() {
        big_stack(|| script(include_str!("connection/plpgsql_call.test")));
    }

    /// `CALL` in the extended protocol: the parameters of the call, the description of the row of the output arguments, no description for a procedure with no output arguments, and the errors of `Parse` and `Execute`. `connection/call_extended.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn call_extended() {
        big_stack(|| script(include_str!("connection/call_extended.test")));
    }

    /// `FOREACH ... SLICE` in PL/pgSQL: the slices of the last dimensions with their lower bounds, the checks of the slice and of the type of the loop variable, the typmod of the loop variable, `EXIT` and `FOUND`. `connection/plpgsql_foreach_slice.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_foreach_slice() {
        big_stack(|| script(include_str!("connection/plpgsql_foreach_slice.test")));
    }

    /// A field of a record as a target of `INTO`, of `EXECUTE ... INTO`, of `RETURNING ... INTO`, of `GET DIAGNOSTICS` and of the loops, alone and in a list with variables. A field that does not exist, a `record` with no value and a `CONSTANT` record give the errors of PostgreSQL. `connection/plpgsql_field_targets.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_field_targets() {
        big_stack(|| script(include_str!("connection/plpgsql_field_targets.test")));
    }

    /// The cursors of PL/pgSQL: bound and unbound cursor variables, `OPEN` with arguments by place and by name, `OPEN FOR EXECUTE`, `FETCH` and `MOVE` in each direction, `CLOSE`, and the `FOR` loop over a cursor. A cursor that a function opens is a portal of the session that SQL can fetch from, and a subtransaction that rolls back closes the cursors that it opened. The errors of the compile and of the run are the errors of PostgreSQL. `connection/plpgsql_cursors.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_cursors() {
        big_stack(|| script(include_str!("connection/plpgsql_cursors.test")));
    }

    /// The assignment to an element or a slice of an array variable in PL/pgSQL, as `RAW_PARSE_PLPGSQL_ASSIGNn` parses it: an array of more than one dimension, a slice that makes a one-dimensional array larger, a variable that is null, the typmod of the element, and the errors of the subscripts and of the bounds. `connection/plpgsql_array_assign.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_array_assign() {
        big_stack(|| script(include_str!("connection/plpgsql_array_assign.test")));
    }

    /// Subscripts on a column in `UPDATE`, `INSERT`, `ON CONFLICT DO UPDATE` and `MERGE`: an element or a slice of the column gets the new value, more than one item can set parts of the same column, `INSERT` sets a part of a null value, and a domain over an array checks the new value. `connection/array_assign.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_assign() {
        big_stack(|| script(include_str!("connection/array_assign.test")));
    }

    /// The assignment to a field of a composite column in `UPDATE` and `INSERT`, and of a composite variable in PL/pgSQL, as `FieldStore` makes it: a field of a field, a field of an element of an array, an element of an array in a field, more than one item for the same column, a null row, a domain over a row type, and the errors. `connection/field_assign.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn field_assign() {
        big_stack(|| script(include_str!("connection/field_assign.test")));
    }

    /// `COMMIT` and `ROLLBACK` in procedures and in `DO`, with `AND CHAIN`: a nested `CALL`, `SET LOCAL`, the loops over a query and over a cursor, a cursor that the commit closes, a deferred foreign key that fails at the commit, and the errors in a function, in a trigger, in a transaction block, in a block with `EXCEPTION` and in a loop over a query that writes. `connection/plpgsql_transactions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_transactions() {
        big_stack(|| script(include_str!("connection/plpgsql_transactions.test")));
    }

    /// The statements that are not queries in PL/pgSQL, in a statement and in `EXECUTE`: `CREATE TABLE`, `CREATE TABLE AS` with its count of rows, `ALTER TABLE`, `DROP TABLE`, `TRUNCATE` and `CREATE FUNCTION`, the queries after them that read the new catalog, a block with `EXCEPTION` that undoes them and `SET`, `COMMIT` and `ROLLBACK` after them in a procedure, `SET` and `SET LOCAL`, and the errors of `INTO`, of the transaction commands, of `COPY` to the client and of `VACUUM`. `connection/plpgsql_utility.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_utility() {
        big_stack(|| script(include_str!("connection/plpgsql_utility.test")));
    }

    /// The transition tables of triggers, `REFERENCING OLD TABLE` and `REFERENCING NEW TABLE`: the rows of an `INSERT`, an `UPDATE` and a `DELETE` in triggers `FOR EACH STATEMENT` and `FOR EACH ROW`, with a join, `UNION ALL`, `EXECUTE`, an `INSERT` that reads them, the rows that a trigger `BEFORE` changed, an empty write, the columns after `ALTER COLUMN TYPE` and `DROP COLUMN`, the error of a function that is not a trigger, and the error of a write into a transition table. `connection/trigger_transition.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn trigger_transition() {
        big_stack(|| script(include_str!("connection/trigger_transition.test")));
    }

    /// The triggers `INSTEAD OF ... FOR EACH ROW` of a view: an `INSERT`, an `UPDATE` and a `DELETE` of the view with the triggers `FOR EACH STATEMENT` of the view, a trigger that skips a row, `RETURNING` with the row that the trigger gives, `FROM` and `USING`, two triggers in the order of their names, a default of a column of the view, a write in `WITH`, and the errors of `CREATE TRIGGER` for the wrong kind of trigger. `connection/view_instead_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn view_instead_triggers() {
        big_stack(|| script(include_str!("connection/view_instead_triggers.test")));
    }

    /// `r.*` of a variable of PL/pgSQL: `row(old.*)` and `new.*::text` in a trigger, `SELECT r.* INTO`, `RETURN QUERY SELECT r.*`, a subquery that reads the fields of an argument, and the errors for a variable of type `record` and for a variable that is not composite. `connection/plpgsql_record_star.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plpgsql_record_star() {
        big_stack(|| script(include_str!("connection/plpgsql_record_star.test")));
    }

    /// Triggers on `INSERT ... ON CONFLICT`: the triggers `INSERT` and `UPDATE ... FOR EACH STATEMENT` of `DO UPDATE`, the triggers `BEFORE ... FOR EACH ROW` that change the row before the conflict and the new row of `DO UPDATE` or skip it, `UPDATE OF`, `WHEN`, the checks after the triggers, and the transition tables of the rows that went in and the rows that changed. `connection/on_conflict_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn on_conflict_triggers() {
        big_stack(|| script(include_str!("connection/on_conflict_triggers.test")));
    }

    /// Triggers on `COPY FROM`: the triggers `FOR EACH STATEMENT` with and without rows, the triggers `BEFORE ... FOR EACH ROW` after `WHERE` that change, skip or fail a row with the line of the copy in their context, the checks after the triggers, a duplicate key after the triggers of the rows before it, `WHEN`, and a transition table. `connection/copy_from_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn copy_from_triggers() {
        big_stack(|| script(include_str!("connection/copy_from_triggers.test")));
    }

    /// `pg_trigger_depth`: 0 outside of a trigger and in a plain function, the depth in the triggers `BEFORE` and `AFTER`, `FOR EACH ROW` and `FOR EACH STATEMENT`, in the triggers of a write in a trigger, in a trigger that calls itself, and after an error in a trigger and an error that a trigger catches. `connection/trigger_depth.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn trigger_depth() {
        big_stack(|| script(include_str!("connection/trigger_depth.test")));
    }

    /// `CREATE CONSTRAINT TRIGGER`: the rows of `pg_trigger` and `pg_constraint`, `pg_get_triggerdef` and `pg_get_constraintdef`, the triggers `NOT DEFERRABLE`, `INITIALLY IMMEDIATE` and `INITIALLY DEFERRED` with `FROM` and `WHEN`, the events that fire at the commit, at `SET CONSTRAINTS ... IMMEDIATE` and after a savepoint, an error at the commit and after `SET CONSTRAINTS` in a savepoint, the errors of `CREATE`, and the drops of the trigger, of the constraint, of the table of `FROM` and of the table. `connection/constraint_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn constraint_triggers() {
        big_stack(|| script(include_str!("connection/constraint_triggers.test")));
    }

    /// The triggers of `MERGE`: the triggers `FOR EACH STATEMENT` of the actions of the clauses, the triggers `BEFORE ... FOR EACH ROW` that change or skip a row before the checks, the triggers `AFTER ... FOR EACH ROW` in the order of the actions, `UPDATE OF` with the columns of all the clauses `UPDATE`, a condition of `WHEN`, the transition tables of each event and `RETURNING` with the row of a trigger. `connection/merge_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn merge_triggers() {
        big_stack(|| script(include_str!("connection/merge_triggers.test")));
    }

    /// The whole old and new rows in `RETURNING` of `MERGE`: a null old row for an insert and a null new row for a delete, a table with a dropped column, a subquery, the names of `WITH (OLD AS ..., NEW AS ...)` and an old row with only nulls. `connection/merge_returning_rows.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn merge_returning_rows() {
        big_stack(|| script(include_str!("connection/merge_returning_rows.test")));
    }

    /// `CREATE TABLE ... PARTITION BY` and `CREATE TABLE ... PARTITION OF`: the partition key, the bounds and their checks, the merged columns and constraints, and the catalog rows. `connection/partition_create.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_create() {
        big_stack(|| script(include_str!("connection/partition_create.test")));
    }

    /// `INSERT` into and `SELECT` from partitioned tables: the routing of rows by range, list and hash keys, sub-partitions, default partitions, keys with expressions and several columns, `ONLY`, `RETURNING tableoid`, the checks of the leaves, an insert into a partition, and the errors for a row that no partition takes. `connection/partition_dml.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_dml() {
        big_stack(|| script(include_str!("connection/partition_dml.test")));
    }

    /// `UPDATE` and `DELETE` of partitioned tables and partitions: changes in place, rows that move to another partition, sub-partitions and defaults, `ONLY`, `RETURNING` with `tableoid` and the old and new rows, `FROM` and `USING`, unique keys of a partition, a rollback, prepared statements, CTEs, and the errors for a row that no partition takes and for the constraints of the partitions. `connection/partition_change.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_change() {
        big_stack(|| script(include_str!("connection/partition_change.test")));
    }

    /// `COPY FROM` into partitioned tables and partitions, `COPY TO` of a partitioned table, and `TRUNCATE` of partitioned tables and partitions: the routing of each row with the line of the row in the errors, the constraints and unique keys of the partitions, `WHERE`, CSV, a rollback, `ONLY`, and `CASCADE` with a foreign key. `connection/partition_copy.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_copy() {
        big_stack(|| script(include_str!("connection/partition_copy.test")));
    }

    /// Indexes, unique indexes and primary keys of partitioned tables: the index of each partition with an index that matches or a new index, `ONLY`, the indexes of a new partition, the checks of the partition key, the unique keys of the partitions in `INSERT`, `UPDATE` and `COPY`, and `DROP INDEX` and `DROP TABLE` with the partition dependencies. `connection/partition_index.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_index() {
        big_stack(|| script(include_str!("connection/partition_index.test")));
    }

    /// `ALTER TABLE ... ATTACH PARTITION` and `DETACH PARTITION`, and `ALTER INDEX ... ATTACH PARTITION`: the checks of the new partition, its columns, its constraints and its indexes, the check of its rows and of the rows of the default partition, the detach of a partition with its columns, constraints and indexes, `CONCURRENTLY` and `FINALIZE`, and the indexes of partitioned tables that become valid. `connection/partition_attach.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_attach() {
        big_stack(|| script(include_str!("connection/partition_attach.test")));
    }

    /// `ALTER TABLE` of partitioned tables and of partitions: `ADD COLUMN`, `SET DEFAULT`, `DROP COLUMN`, `ALTER COLUMN ... TYPE`, `SET NOT NULL`, `DROP NOT NULL`, `ADD CONSTRAINT` and `DROP CONSTRAINT` on the partitions, with `ONLY`, the counts of the inherited columns and constraints, `SET STATISTICS` and `SET STORAGE`, which go on to the partitions, and the subcommands that do not, the errors for the columns of the partition key and for the inherited columns and constraints of a partition, and the indexes of the primary keys and unique constraints. `connection/partition_alter.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_alter() {
        big_stack(|| script(include_str!("connection/partition_alter.test")));
    }

    /// Identity columns of partitioned tables: the partitions get the identity of the column with no sequence of their own, and an `INSERT` into a partition takes the values of the sequence of the parent. `ADD`, `SET` and `DROP IDENTITY` and `ADD COLUMN` go on to the partitions, and give the errors of PostgreSQL with `ONLY` and on a partition. `ATTACH PARTITION` gives the identity to the new partition, and `DETACH PARTITION` takes it away. `connection/partition_identity.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_identity() {
        big_stack(|| script(include_str!("connection/partition_identity.test")));
    }

    /// The foreign keys of partitioned tables and the foreign keys that refer to them: the clone of a foreign key for each partition, a new partition, `ATTACH PARTITION` of a table with and without the same foreign key, the checks of the leaves with the names of the leaves in the errors, the actions of `CASCADE` and `SET NULL` on the partitions, the clones of a foreign key for each partition of the referenced table with their names, the check of a row that moves to another partition, `DETACH PARTITION` with its check of the references, and the error for a clone of `DROP CONSTRAINT`. `connection/partition_fkeys.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_fkeys() {
        big_stack(|| script(include_str!("connection/partition_fkeys.test")));
    }

    /// The catalog rows of the foreign keys of partitioned tables: `pg_constraint`, the triggers in `pg_trigger` with their parents, the rows of `pg_depend`, `DROP TABLE` of a referenced partition, `ATTACH PARTITION` of a table with the same foreign key, and `DETACH PARTITION`, which gives the partition its own triggers and clones. `connection/partition_fkeys_catalog.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_fkeys_catalog() {
        big_stack(|| script(include_str!("connection/partition_fkeys_catalog.test")));
    }

    /// `INSERT ... ON CONFLICT` into partitioned tables: each row goes to its partition, and the arbiters are the indexes of the partition whose partitioned index the clause infers. With no inference, a unique index of the partition alone is an arbiter too. `DO UPDATE` changes the row in the partition, and a new row for another partition is an error. `connection/partition_upsert.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_upsert() {
        big_stack(|| script(include_str!("connection/partition_upsert.test")));
    }

    /// The triggers of partitioned tables: a row trigger has a clone in each partition, also in a new or an attached partition, and `DETACH PARTITION` drops the clones. A clone cannot be dropped or renamed alone, and `ALTER TABLE ... DISABLE TRIGGER` changes the clones too. A row of a partitioned table fires the statement triggers of the table and the row triggers of its leaf, and a trigger `BEFORE` cannot move the row to another partition. `connection/partition_triggers.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_triggers() {
        big_stack(|| script(include_str!("connection/partition_triggers.test")));
    }

    /// The triggers of the leaves of a partitioned table for `COPY FROM` and `INSERT ... ON CONFLICT`: the triggers `BEFORE ... FOR EACH ROW` of the leaf fire for each row, with the line of the copy in the context of their messages, and `EXCLUDED` is the row that they give. The triggers `AFTER ... FOR EACH ROW` fire in the order of the rows, and the transition tables of the partitioned table get the rows of the leaves. A trigger of a leaf that takes the row out of the bounds of the leaf gives an error. `connection/partition_trigger_copy.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_trigger_copy() {
        big_stack(|| script(include_str!("connection/partition_trigger_copy.test")));
    }

    /// The triggers of the leaves of a partitioned table for an `UPDATE` that moves a row: the triggers `BEFORE UPDATE` of the old leaf fire, then `BEFORE DELETE` of the old leaf and `BEFORE INSERT` of the new leaf, and then the triggers `AFTER DELETE` and `AFTER INSERT`. A trigger `BEFORE DELETE` that skips the row keeps the row, and a trigger `BEFORE INSERT` that skips it leaves the row deleted. The transition tables of `UPDATE` and `DELETE` on the partitioned table have the rows of the leaves and the rows that move. `connection/partition_trigger_move.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_trigger_move() {
        big_stack(|| script(include_str!("connection/partition_trigger_move.test")));
    }

    /// `MERGE` into a partitioned table and into a partition: updates in a leaf, updates that move a row to another leaf, deletes and inserts with `RETURNING` and the leaf of each row, a row that no partition takes, the constraints and the unique keys of the leaves, a row of the target table that the command changes two times, and the partition constraint of a partition. The triggers of the leaves fire in the order of the join, and a row that moves is in the transition tables of `DELETE` and of `INSERT`. `connection/partition_merge.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_merge() {
        big_stack(|| script(include_str!("connection/partition_merge.test")));
    }

    /// Partitions whose columns are in another order than the columns of the parent table, or that have dropped columns: `CREATE TABLE ... PARTITION OF` a table with a dropped column, `ATTACH PARTITION` of such tables, the reads and the writes of the partitioned table and of its partitions, `COPY`, the check constraints, the indexes and the triggers that the partitions take from the parent, `ALTER TABLE` of the parent, and a partition key with an expression. `connection/partition_columns.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_columns() {
        big_stack(|| script(include_str!("connection/partition_columns.test")));
    }

    /// The rows of a table after `ADD COLUMN` and a change of the default of the new column in the same statement: the rows get the default of `ADD COLUMN`, and new rows get the new default, also for the partitions. `DROP DEFAULT`, `ALTER COLUMN ... TYPE` and `DROP COLUMN` run before `ADD COLUMN`, so they cannot find the new column. `connection/add_column_default.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn add_column_default() {
        big_stack(|| script(include_str!("connection/add_column_default.test")));
    }

    /// `array_agg` of arrays: arrays of one and of more dimensions, lower bounds that are not 1, null elements, `ORDER BY`, `DISTINCT` and `FILTER`, groups, the errors for a null array, an empty array, arrays of different dimensions and too many dimensions, and the view `pg_stats_ext_exprs`. `connection/array_agg_array.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn array_agg_array() {
        big_stack(|| script(include_str!("connection/array_agg_array.test")));
    }

    /// The input and output of `pg_ndistinct` and `pg_dependencies`, their errors, the casts to `bytea`, the error for `pg_mcv_list` input, `pg_mcv_list_items` and the view `pg_stats_ext`. `connection/stats_types.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn stats_types() {
        big_stack(|| script(include_str!("connection/stats_types.test")));
    }

    /// `pg_input_is_valid` and `pg_input_error_info` for the built-in types, arrays, the reg types, the domains of `information_schema` and their arrays, a function with a row result in `FROM`, null arguments, and the hard errors of a bad type name and of a type with no input. `connection/soft_input.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn soft_input() {
        big_stack(|| script(include_str!("connection/soft_input.test")));
    }

    /// The views `pg_config`, `pg_file_settings`, `pg_available_extensions` and `pg_available_extension_versions` and their functions: the names of `pg_config` in order, the columns of each view, the errors of a call with a bad argument and the row of `plpgsql`. The values of `pg_config` are not in the script, because they come from the build. `connection/state_views.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn state_views() {
        big_stack(|| script(include_str!("connection/state_views.test")));
    }

    /// Temporary tables, views and sequences: the schema `pg_temp_N` that the first temporary object makes, its place at the front of the search path, a temporary table that hides a table of the same name, `pg_temp` in a name and in `search_path`, the errors of a temporary object in another schema and of `ON COMMIT` on a table that is not temporary, a view of a temporary table, a TOAST table in `pg_toast_temp_N`, and a rollback of a `CREATE TEMP TABLE`. `connection/temp_tables.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn temp_tables() {
        big_stack(|| script(include_str!("connection/temp_tables.test")));
    }

    /// Unlogged tables and sequences: `relpersistence` `u` on the table, its indexes, its TOAST table and its owned sequences, `SET LOGGED` and `SET UNLOGGED` also on a partition, and the errors of a temporary schema, a partitioned table, a materialized view, a view and a foreign key between a permanent and an unlogged table. The rows stay in memory as for a permanent table. `connection/unlogged.test` is the output of PostgreSQL 19 for the same script in a new database, with the names of the TOAST relations hidden.
    #[test]
    fn unlogged() {
        big_stack(|| script(include_str!("connection/unlogged.test")));
    }

    /// The temporary schemas of a session go when the session ends, and another session cannot make a relation in them.
    #[test]
    fn temp_schemas_go_at_the_end() {
        big_stack(|| {
            let store = Arc::new(Store::new());
            let (mut a, _) = connect_to(&store, 1234);
            let (mut b, _) = connect_to(&store, 1235);
            let row = |c: &mut Connection, sql: &str| {
                let lines = send(c, &query(sql));
                lines.iter().find_map(|l| l.strip_prefix("row ").map(str::to_string))
            };
            let count = "SELECT count(*) FROM pg_namespace WHERE nspname ~ '^pg_(toast_)?temp_'";
            send(&mut a, &query("BEGIN; CREATE TEMP TABLE t (a text); INSERT INTO t VALUES ('x')"));
            assert_eq!(row(&mut b, count).as_deref(), Some("0"));
            send(&mut a, &query("COMMIT"));
            assert_eq!(row(&mut b, count).as_deref(), Some("2"));
            let temp = row(&mut a, "SELECT pg_my_temp_schema()::regnamespace").unwrap();
            assert_eq!(
                row(&mut b, &format!("SELECT pg_is_other_temp_schema('{temp}'::regnamespace)"))
                    .as_deref(),
                Some("t")
            );
            let lines = send(&mut b, &query(&format!("CREATE TABLE {temp}.u (a int)")));
            assert!(
                lines.iter().any(|l| l
                    .contains("cannot create relations in temporary schemas of other sessions")),
                "{lines:?}"
            );
            assert_eq!(row(&mut b, "SELECT count(*) FROM t").as_deref(), None);
            send(&mut a, &query("BEGIN; INSERT INTO t VALUES ('y')"));
            drop(a);
            assert_eq!(row(&mut b, count).as_deref(), Some("0"));
            assert_eq!(
                row(&mut b, "SELECT count(*) FROM pg_class WHERE relname = 't'").as_deref(),
                Some("0")
            );
        });
    }

    /// `has_table_privilege`, `has_column_privilege`, `has_any_column_privilege` and `has_sequence_privilege` on the system views, the views of `information_schema`, and a table, a view and an index that the script makes. The checks run for the superuser and for the predefined roles, with the system columns, column numbers that do not exist, and the errors of a bad name or a relation that is not a sequence. `connection/relation_privileges.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn relation_privileges() {
        big_stack(|| script(include_str!("connection/relation_privileges.test")));
    }

    /// `has_schema_privilege`, `has_type_privilege` and `has_function_privilege` on the objects of the catalog of the session: `information_schema` with its domains and functions, a schema that the script makes, and the row types of its tables and views with their arrays. The checks run for the superuser and for the predefined roles, with OIDs that do not exist and the errors of a bad name or a bad privilege. `connection/object_privileges.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn object_privileges() {
        big_stack(|| script(include_str!("connection/object_privileges.test")));
    }

    /// `ALTER TABLE` with `SET DEFAULT`, `DROP DEFAULT`, `ADD CONSTRAINT` and `OWNER TO` on tables, views, indexes and sequences, and `ALTER SCHEMA ... OWNER TO`. The script checks the rows of `pg_attrdef`, `pg_constraint`, `pg_index`, `pg_class` and `pg_type` after each change, with the errors of a bad column, a bad role, a second primary key, a relation of the wrong kind and a system catalog. `connection/alter_table.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_table() {
        big_stack(|| script(include_str!("connection/alter_table.test")));
    }

    /// `CREATE SEQUENCE` and `ALTER SEQUENCE` with their options and their errors, `OWNED BY`, and `nextval`, `currval`, `lastval` and `setval` with the cache of the session. The script checks that a value stays used after a rollback, that a rollback of `ALTER SEQUENCE` gives back the old row, and that `DISCARD SEQUENCES` drops the state of the session. `connection/sequence.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sequence() {
        big_stack(|| script(include_str!("connection/sequence.test")));
    }

    /// `COPY ... FROM STDIN` with no rows: the checks before `CopyInResponse` and after it, the header lines with `HEADER MATCH`, the end-of-copy marker, the header of the binary format, `CopyFail`, and the statements after a `COPY` in one `Query` message. `connection/copy_from.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn copy_from() {
        big_stack(|| script(include_str!("connection/copy_from.test")));
    }

    /// `COPY ... FROM STDIN` with rows: the text, CSV and binary formats, the defaults of the columns that the copy does not give, `FORCE_NULL`, `FORCE_NOT_NULL`, the condition `WHERE`, identity columns, the errors of the fields and of the constraints with the context of their lines, foreign keys, a copy of more rows than the copy keeps before it writes them, and a copy in a subtransaction. `connection/copy_from_rows.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn copy_from_rows() {
        big_stack(|| script(include_str!("connection/copy_from_rows.test")));
    }

    /// `COPY ... FROM STDIN` with `DEFAULT`, `ON_ERROR IGNORE` and `ON_ERROR SET_NULL`, `REJECT_LIMIT` and `LOG_VERBOSITY`, and the checks of domains in the input of the fields. `connection/copy_from_options.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn copy_from_options() {
        big_stack(|| script(include_str!("connection/copy_from_options.test")));
    }

    /// `ALTER TABLE ... ADD COLUMN` on an empty table and on a table with rows: defaults, `serial` and identity columns, not-null, check, unique and foreign key constraints, domains, and a rollback. Also `SET NOT NULL`, `ADD CONSTRAINT` and `CREATE UNIQUE INDEX` on a table with rows. `connection/alter_add_column.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_add_column() {
        big_stack(|| script(include_str!("connection/alter_add_column.test")));
    }

    /// `CREATE TABLE ... INHERITS`: the columns, the defaults, the check and not-null constraints that a table takes from its parents, the errors of the parents that a table cannot inherit from, `pg_inherits`, a `SELECT` of a parent, `TRUNCATE` and `DROP TABLE`. `connection/inherit.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit() {
        big_stack(|| script(include_str!("connection/inherit.test")));
    }

    /// `CREATE TABLE ... INHERITS` with two parents and with columns of the statement: the columns and the constraints that merge, the conflicts of types, collations and defaults, a `serial` column of a parent, and `tableoid` and whole rows in a `SELECT` of a parent. `connection/inherit_merge.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_merge() {
        big_stack(|| script(include_str!("connection/inherit_merge.test")));
    }

    /// `UPDATE` and `DELETE` of a table with children of `INHERITS`: the rows of the children change in their tables and keep the values of their own columns, `ONLY`, `RETURNING` with `tableoid`, `FROM` and `USING`, the check constraints and the unique keys of the children, the triggers of the parent and of the children, a child of two parents, and a rollback. `connection/inherit_change.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_change() {
        big_stack(|| script(include_str!("connection/inherit_change.test")));
    }

    /// `MERGE` into a table with children of `INHERITS`: an update or a delete of a row of a child changes the row in its table, an insert goes to the table of the statement, `ONLY`, `RETURNING` with `tableoid`, the check constraints of the children, a row that two source rows match, the triggers of the parent and of the children, `NOT MATCHED BY SOURCE`, and a rollback. `connection/inherit_merge_into.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_merge_into() {
        big_stack(|| script(include_str!("connection/inherit_merge_into.test")));
    }

    /// `ALTER TABLE` of the columns of a table with children of `INHERITS`: `ADD COLUMN` that merges with a column of a child, a child with two parents, `DROP COLUMN` of an inherited column and of a column with two parents, `ALTER COLUMN TYPE`, `SET DEFAULT` and `RENAME COLUMN`, and the errors of `ONLY` and of a child. `connection/inherit_alter_column.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_alter_column() {
        big_stack(|| script(include_str!("connection/inherit_alter_column.test")));
    }

    /// `ALTER TABLE` of the constraints of a table with children of `INHERITS`: `ADD CONSTRAINT` that merges with a constraint of a child, `NO INHERIT`, `DROP CONSTRAINT` with and without `ONLY`, `SET NOT NULL` and `DROP NOT NULL`, `SET STATISTICS`, `RENAME CONSTRAINT`, and the errors of `ONLY` and of a child. `connection/inherit_alter_constraint.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_alter_constraint() {
        big_stack(|| script(include_str!("connection/inherit_alter_constraint.test")));
    }

    /// `ALTER TABLE` of a table with children of `INHERITS` for the other subcommands: `ADD COLUMN` with a not-null constraint, a check constraint and a default, `ADD PRIMARY KEY` and `ADD UNIQUE`, which give the children the not-null constraints but no index, an identity that the children do not get, `SET STORAGE`, `SET COMPRESSION`, `ALTER COLUMN TYPE` with `USING`, `SET DEFAULT` with `ONLY`, `DROP COLUMN` with a view, `ONLY` and `IF NOT EXISTS`, and `SET UNLOGGED`. `connection/inherit_alter_other.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_alter_other() {
        big_stack(|| script(include_str!("connection/inherit_alter_other.test")));
    }

    /// `ALTER TABLE ... INHERIT` and `NO INHERIT`: the columns and the constraints of the child count the new parent and stay local, the numbers of `pg_inherits`, a column with two parents, the errors for a parent that is the table, a child, a partitioned table, a partition, a temporary table, a view or an index, for a missing column or constraint, a column with a different type or no not-null constraint, a check constraint with a different definition, a table that is not a parent and a transition table, and `NO INHERIT` in a transaction that rolls back. `connection/inherit_attach.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherit_attach() {
        big_stack(|| script(include_str!("connection/inherit_attach.test")));
    }

    /// `CREATE MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW` and `DROP MATERIALIZED VIEW`: the rows stay as the query gave them until a refresh, `WITH NO DATA` and the error of a scan of a materialized view that is not populated, the rows of `pg_class`, `pg_matviews` and `pg_rewrite`, the errors of `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE` and `COPY ... FROM`, `COPY ... TO`, the checks of `CONCURRENTLY`, the dependencies on the table and of a view, the errors of `DROP` of the wrong kind, a rename of the materialized view and of a column, and a materialized view in a transaction that rolls back. `connection/matview.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn matview() {
        big_stack(|| script(include_str!("connection/matview.test")));
    }

    /// More materialized views: the errors of `CREATE OR REPLACE VIEW` of a materialized view, of a data-modifying `WITH`, of a temporary table and of too many column names, a materialized view of a materialized view, a rename of the base table and of its column, a unique index on rows that are not unique, a prepared query, a view and a sublink of a materialized view with no data, `COMMENT`, `SET SCHEMA`, `OWNER TO`, `DROP` with `CASCADE`, `EXPLAIN` and `FOR UPDATE`. `connection/matview_more.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn matview_more() {
        big_stack(|| script(include_str!("connection/matview_more.test")));
    }

    /// Typed tables: `CREATE TABLE ... OF` with the options of its columns and the errors of the type, the changes of `ALTER TABLE` that a typed table cannot have, `ALTER TYPE` with `CASCADE` on the typed tables of the type, `ALTER TABLE ... OF` and `NOT OF` with the checks of the columns, a partitioned typed table, a child of a typed table, and `DROP TYPE`. `connection/typed_table.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn typed_table() {
        big_stack(|| script(include_str!("connection/typed_table.test")));
    }

    /// Partition pruning: the leaves that a scan of a range, a list, a hash and a multi-level partitioned table reads for `=`, `<`, `BETWEEN`, `IN`, `= ANY`, `IS NULL` and `OR` of the key, with a key of two columns, a constant of another integer type, a parameter of `EXECUTE`, `UPDATE` and `DELETE`, and `IS NULL` of a table that a left join can make null, as `pg_stat_user_tables` counts the scans. `connection/partition_pruning.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_pruning() {
        big_stack(|| script(include_str!("connection/partition_pruning.test")));
    }

    /// `EXPLAIN` of a partitioned table and of a table with children of `INHERITS`: an `Append` of the scans of the leaves that the conditions do not prune, one scan with no `Append` for one leaf, a `Result` for no leaf, and the target tables of `UPDATE` and `DELETE`. `connection/explain_append.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn explain_append() {
        big_stack(|| script(include_str!("connection/explain_append.test")));
    }

    /// `EXPLAIN` of the conditions that an equivalence class with a constant gives: a condition of each member with the constant in place of the `=` of a join, the nullable side of a left join, a scan with no rows for two constants that are not equal, and the partitions that the new conditions prune. `connection/equivalence_const.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn equivalence_const() {
        big_stack(|| script(include_str!("connection/equivalence_const.test")));
    }

    /// `EXPLAIN` of joins of three and four tables in the order that the join search of PostgreSQL finds: by the conditions of the joins, by the classes with a constant, with a table that has no condition, with a lateral function, and with a left join as one item. `connection/join_search.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn join_search() {
        big_stack(|| script(include_str!("connection/join_search.test")));
    }

    /// `EXPLAIN` of plans whose rows do not fit in `work_mem`: a sort on disk, a `Materialize` that writes its rows to disk, a merge join whose sorted inner side gets a `Materialize`, and a hash join in batches. The width of each side is the width of the columns that the nodes above it read. `connection/plan_widths.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_widths() {
        big_stack(|| script(include_str!("connection/plan_widths.test")));
    }

    /// `EXPLAIN` of joins where `add_path` keeps the plan with the lower start cost when two plans cost about the same in total, unless the order of the rows of the other plan is of use to `ORDER BY`, `GROUP BY`, `DISTINCT` or a merge join above it. `connection/plan_paths.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_paths() {
        big_stack(|| script(include_str!("connection/plan_paths.test")));
    }

    /// `EXPLAIN` of merge joins whose sides the planner sorts in the order of `ORDER BY`, `GROUP BY` or `DISTINCT`, with their directions, when the merge conditions have the keys of the query, as `select_outer_pathkeys_for_merge`. `connection/plan_merge_keys.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_merge_keys() {
        big_stack(|| script(include_str!("connection/plan_merge_keys.test")));
    }

    /// `EXPLAIN` of conditions on a column with a unique index of one key column and no predicate, as `has_unique_index` finds it: the column has as many values as the table has rows, for `=` and `<>` with a value and for the conditions of joins. A join whose inner side is a table with a unique index that is immediate and has no predicate, with an `=` condition for each key column, costs less, as `innerrel_is_unique` finds it. `connection/plan_unique.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_unique() {
        big_stack(|| script(include_str!("connection/plan_unique.test")));
    }

    /// `EXPLAIN` of `GROUP BY`, `DISTINCT` and windows with the number of groups of `estimate_num_groups`: the columns of one table have at most a tenth of its rows as groups, the conditions of a table make fewer groups, and two columns that an equivalence class makes equal count once. `connection/plan_groups.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_groups() {
        big_stack(|| script(include_str!("connection/plan_groups.test")));
    }

    /// `EXPLAIN` of the keys of `GROUP BY`, `DISTINCT` and `ORDER BY` without the keys that add nothing: a key whose equivalence class has a constant, a key in the class of an earlier key, as `pathkey_is_redundant` finds them, and a column of `GROUP BY` that a unique index of the other columns of its table gives, as `remove_useless_groupby_columns` finds it. A `DISTINCT` with no keys left takes the first row, and a window shows only the items of `PARTITION BY` that are not redundant. `connection/plan_redundant_keys.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_redundant_keys() {
        big_stack(|| script(include_str!("connection/plan_redundant_keys.test")));
    }

    /// `EXPLAIN` of the columns of a subquery of `FROM` whose `Subquery Scan` the plan does not keep, as `trivial_subqueryscan` takes it out. A node above it reads the target list of the plan of the subquery, so a column shows the target of the subquery with the names of the subquery, as `get_variable` finds it for `OUTER_VAR`, and a target that is not a column gets parentheses. `connection/plan_removed_scan.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_removed_scan() {
        big_stack(|| script(include_str!("connection/plan_removed_scan.test")));
    }

    /// `EXPLAIN` of the conditions with a subquery that runs as a `SubPlan` for each row, as `cost_subplan`: a hashed subplan costs its plan once when the node starts, and a correlated subplan costs its plan for each call, half of it for `ANY` and `ALL`. The plan of a subplan with no parameters that keeps its rows, as a `Materialize`, costs its start once. The conditions of a scan are in the order of their cost, so a condition with a subplan comes last, as `order_qual_clauses`. A `HAVING` and the filter of a join cost their subplans in the same way. `connection/plan_subplan_cost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_subplan_cost() {
        big_stack(|| script(include_str!("connection/plan_subplan_cost.test")));
    }

    /// `EXPLAIN` of the conditions `x op ANY (array)` and `x op ALL (array)`, as `cost_qual_eval` finds their cost. The operator runs for half of the items of the array for each row, and for 10 items of an array that is not a constant or a list, as `estimate_array_length`. A constant array of 9 items or more with an operator that can hash is a hash table, as `convert_saop_to_hashed_saop`: the scan hashes each item of the array when it starts, then does one hash and one comparison for each row. `connection/plan_array_cost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_array_cost() {
        big_stack(|| script(include_str!("connection/plan_array_cost.test")));
    }

    /// `EXPLAIN` of a correlated `EXISTS` that can run as `ANY` on a hash table, as `convert_EXISTS_to_ANY` makes it. Each condition of the subquery that compares a column of the query with an operator that can hash becomes a column of the subquery, and the plan keeps the subplan of `EXISTS` or of `ANY` with the lower cost for the rows of its node, as `fix_alternative_subplan`. The cost of the node is the cost of `EXISTS`. `connection/plan_exists_to_any.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_exists_to_any() {
        big_stack(|| script(include_str!("connection/plan_exists_to_any.test")));
    }

    /// `EXPLAIN` of the subqueries of `EXISTS`, `ANY` and `ALL` that read only a part of their rows, as the `tuple_fraction` of `make_subplan`: `EXISTS` reads one row, and `ANY` and `ALL` read half the rows. The joins of the subquery keep the plans with a lower start cost, and the subplan runs the plan that costs the least for the rows that it reads, as `get_cheapest_fractional_path`. `LIMIT` in the subquery changes the fraction, as `preprocess_limit`. `connection/plan_fraction.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_fraction() {
        big_stack(|| script(include_str!("connection/plan_fraction.test")));
    }

    /// `EXPLAIN` of the conditions of `WHERE` on a subquery of `FROM` that the planner moves into the subquery, as `set_subquery_pathlist` and `subquery_push_qual`: a condition joins the `WHERE` of the subquery, or its `HAVING` when the subquery makes groups. A condition stays outside a subquery with `LIMIT`, a condition on a column of a window function, on a column that is not an item of `DISTINCT ON` or of `PARTITION BY`, on a column that calls a volatile function or a function that gives a set, and a condition with a volatile function on a subquery with `DISTINCT`. An outer join that a condition makes an inner join is an inner join before the condition moves, as `reduce_outer_joins`, and a `Subquery Scan` with no condition that gives each column of its subquery costs its subquery, as `cost_subqueryscan`. `connection/plan_subquery_pushdown.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_subquery_pushdown() {
        big_stack(|| script(include_str!("connection/plan_subquery_pushdown.test")));
    }

    /// `EXPLAIN` of the cost of the subplans that run once, as `SS_charge_for_initplans` adds it to the start cost and the total cost of the top node of the query: each `InitPlan` and each `CTE` costs its plan, and an `InitPlan` of `EXISTS` costs its first row, as `cost_subplan` finds it. A `CTE Scan` and a `WorkTable Scan` cost two `cpu_tuple_cost` for each row, as `cost_ctescan`. The value of an `InitPlan` of `EXISTS` passes half of the rows, as `boolvarsel` finds it with no statistics, and a row comparison with `=` is the comparisons of its columns. `connection/plan_initplan_cost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_initplan_cost() {
        big_stack(|| script(include_str!("connection/plan_initplan_cost.test")));
    }

    /// `EXPLAIN` of a low bound and a high bound of the same value, such as `x > 1 AND x < 5` and `BETWEEN`, which `clauselist_selectivity` estimates as one range with the selectivity `DEFAULT_RANGE_INEQ_SEL`. The two sides of each bound are a value of one relation and a value that reads no relation and calls no volatile function. `connection/plan_range_pair.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_range_pair() {
        big_stack(|| script(include_str!("connection/plan_range_pair.test")));
    }

    /// `EXPLAIN` of the junk columns of the row marks: `FOR UPDATE` and `FOR SHARE` carry the `ctid` of each table of `FROM`, the `tableoid` of a table with an inheritance tree and the whole row of a function, and an `UPDATE` or a `DELETE` carries the `ctid` of each table other than its target. A whole-row column counts the header of its row in the width. `connection/plan_row_marks.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_row_marks() {
        big_stack(|| script(include_str!("connection/plan_row_marks.test")));
    }

    /// `EXPLAIN` of the subqueries of `FROM` that the planner merges into the query above them, as `pull_up_subqueries`: a subquery that is not grouped, sorted or limited becomes the relations of its `FROM`, in a list of `FROM` and in a side of a join. Its `WHERE` joins the `WHERE` of the query or the `ON` of its join. A merged subquery keeps its place in the range table, so that `VERBOSE` names the columns with their relations, and the relations of the subquery get their names after the relations of the query. `connection/plan_subquery_pullup.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_subquery_pullup() {
        big_stack(|| script(include_str!("connection/plan_subquery_pullup.test")));
    }

    /// `EXPLAIN` of the parts of `WHERE` that are `EXISTS`, `NOT EXISTS` or `IN` on a subquery, which the planner makes semi joins and anti joins, as `pull_up_sublinks`. A semi join is a semi join of the two sides, or an inner join with the unique rows of the inner side, from a `HashAggregate` or from a `Unique` over a `Sort`, as `create_unique_paths` makes them. A semi join with a primary key on the inner side is an inner join, as `reduce_unique_semijoins`. `NOT IN` and an `EXISTS` with no column of the query above stay subplans. `connection/plan_semi_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_semi_join() {
        big_stack(|| script(include_str!("connection/plan_semi_join.test")));
    }

    /// `EXPLAIN` of `NOT IN` on a subquery, which the planner makes an anti join when no value of the test can be null, as `convert_ANY_sublink_to_join` proves it with `sublink_testexpr_is_not_nullable` and `query_outputs_are_not_nullable`. A column of a table with `NOT NULL`, a primary key and a condition of the subquery such as `x IS NOT NULL` or `x > 0` prove it, and a column on the nullable side of an outer join does not. `connection/plan_not_in.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_not_in() {
        big_stack(|| script(include_str!("connection/plan_not_in.test")));
    }

    /// `EXPLAIN` of a left join with a test of `IS NULL` above it, which the planner makes an anti join when the column cannot be null in a row that the join matches, as `reduce_outer_joins` makes it. The condition of `ON` is false or null when the column is null, or the column has `NOT NULL` in its table, and the test of `IS NULL` goes away. A right join becomes an anti join with its sides in the other order. `connection/plan_left_anti.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_left_anti() {
        big_stack(|| script(include_str!("connection/plan_left_anti.test")));
    }

    /// `EXPLAIN` of semi joins and anti joins in the order of the joins of a join domain, as `join_is_legal` lets the planner make it: the inner side of each one is one item of the domain, the outer side is the relations that its conditions read, and a semi join can also be an inner join with the unique rows of its inner side in any place of the order. `connection/plan_join_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_join_order() {
        big_stack(|| script(include_str!("connection/plan_join_order.test")));
    }

    /// `EXPLAIN` of left joins in the order of the joins of a join domain: an inner join can come before or after a left join, as identity 1 of `optimizer/README`, and a condition above a left join that reads its inner side waits for the left join. `connection/plan_left_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_left_order() {
        big_stack(|| script(include_str!("connection/plan_left_order.test")));
    }

    /// `EXPLAIN` of joins with an `OR` in a condition: the selectivity of each part of the `OR` comes from `eqjoinsel` for an `=` of a column of each side, and from `var_eq_const` for an `=` of a unique column and a constant, as `clause_selectivity` finds it. `connection/plan_or_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_or_join() {
        big_stack(|| script(include_str!("connection/plan_or_join.test")));
    }

    /// `EXPLAIN` of `NOT`: the planner moves a `NOT` into its argument, as `negate_clause` of `eval_const_expressions`. An operator becomes its negator, `AND` and `OR` change places, and `IS NULL` and `IS TRUE` take the opposite test. `connection/plan_negate.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_negate() {
        big_stack(|| script(include_str!("connection/plan_negate.test")));
    }

    /// `EXPLAIN` of relations with a condition that is never true: a relation becomes a `Result` with no rows and a width of 0, as `set_dummy_rel_pathlist`, and a join of such relations keeps the width of its columns. `connection/plan_dummy.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_dummy() {
        big_stack(|| script(include_str!("connection/plan_dummy.test")));
    }

    /// `EXPLAIN` of joins with a condition that is never true, as `populate_joinrel_with_paths` marks them dummy: a part of `ON` of an inner join with no column is a condition of the join of all the relations, a left join or an anti join with such a condition has an inner side with no rows that keeps its width, and a join below an outer join with such a condition is a `Result` with no rows. A nested loop counts an inner side with no rows as one row. `connection/plan_dummy_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_dummy_join() {
        big_stack(|| script(include_str!("connection/plan_dummy_join.test")));
    }

    /// `EXPLAIN` of `ANY` and `IN` on a list of `VALUES`, as `convert_VALUES_to_ANY` makes `x IN (VALUES (1), (2))` the condition `x = ANY ('{1,2}')` when the list has two rows at least and each value folds to a constant. A list with a volatile function, a column of the query, `LIMIT` or `ORDER BY`, a subquery that reads the list, `NOT IN`, and `IN` in an `OR` or in the `ON` of a full join stay a subquery. `connection/plan_values_any.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_values_any() {
        big_stack(|| script(include_str!("connection/plan_values_any.test")));
    }

    /// `EXPLAIN` of a target that `make_sort_input_target` computes after the sort: a target that is not a key of the query and calls a volatile function, or costs more than 10 times `cpu_operator_cost` with `LIMIT`. A `Result` above the sort computes it from the columns that the sort gives, with the cost of the targets and `cpu_tuple_cost` for each row, as `create_projection_path`. A sort of no rows rounds as the `LOG2` of PostgreSQL. `connection/plan_sort_late.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_sort_late() {
        big_stack(|| script(include_str!("connection/plan_sort_late.test")));
    }

    /// `EXPLAIN` of `LIKE`, `ILIKE`, regular expressions and `^@` with a fixed prefix on a column of a btree index, as `match_pattern_prefix` makes the conditions of the index from the prefix. `connection/plan_like_prefix.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_like_prefix() {
        big_stack(|| script(include_str!("connection/plan_like_prefix.test")));
    }

    /// `EXPLAIN` of conditions that are an `OR`, as `generate_bitmap_or_paths` makes a `BitmapOr` of the bitmaps of the arms, and `match_orclause_to_indexcol` makes `x = ANY (array)` of the arms that test one column. `connection/plan_bitmap_or.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_bitmap_or() {
        big_stack(|| script(include_str!("connection/plan_bitmap_or.test")));
    }

    /// `EXPLAIN` of conditions of `WHERE`, `ON` and `HAVING` that `canonicalize_qual` changes: an `OR` drops an arm that is null, an `AND` with an arm that is null is false, and the conditions that each arm of an `OR` has go out of the `OR`, so that `(a = 1 AND b = 2) OR (a = 1 AND b = 3)` is `(a = 1) AND ((b = 2) OR (b = 3))`. `connection/plan_canonical_qual.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_canonical_qual() {
        big_stack(|| script(include_str!("connection/plan_canonical_qual.test")));
    }

    /// `EXPLAIN` of joins where the inner side is a parameterized index or bitmap scan in a nested loop: the join condition is an `Index Cond` or a `Recheck Cond` that reads the outer row, as `create_index_paths` and `final_cost_nestloop` choose it, and a hash join wins when the outer side has many rows. `connection/plan_param_nestloop.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_param_nestloop() {
        big_stack(|| script(include_str!("connection/plan_param_nestloop.test")));
    }

    /// `EXPLAIN` of queries whose aggregate calls are each `min` or `max` of one table: a `Result` that replaces the `Agg` node reads each value from an `InitPlan` with a `Limit` over an index scan in the order of the aggregate, as `preprocess_minmax_aggregates` plans it, and the `Agg` node stays when a call has no index. `connection/plan_minmax.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_minmax() {
        big_stack(|| script(include_str!("connection/plan_minmax.test")));
    }

    /// A subquery with no `FROM` in `FROM` merges into the query, and its place in `FROM` goes away, as `remove_useless_result_rtes` drops an `RTE_RESULT`: the constants take the place of its columns, and its `WHERE` runs once in a `Result` above the plan. `connection/plan_result_pullup.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_result_pullup() {
        big_stack(|| script(include_str!("connection/plan_result_pullup.test")));
    }

    /// A `UNION ALL` in `FROM` is an `Append` of its `SELECT`s with no `Subquery Scan` above it, as `pull_up_simple_union_all` makes it an append relation, and each `SELECT` takes the conditions of `WHERE`. A `UNION` takes them too, as `subquery_push_qual` moves them into each `SELECT` of a set operation. `connection/plan_union_all.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_union_all() {
        big_stack(|| script(include_str!("connection/plan_union_all.test")));
    }

    /// A `UNION ALL` in `FROM` is a `Merge Append` of the plans of its `SELECT`s in the order that the query reads when that costs less than a sort of its `Append`, as `generate_orderedappend_paths` makes it. A `SELECT` with no plan in the order gets a sort, and each `SELECT` reads only the columns that the query reads, so that an index only scan can give them. `connection/plan_merge_append.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_merge_append() {
        big_stack(|| script(include_str!("connection/plan_merge_append.test")));
    }

    /// `INTERSECT` and `EXCEPT` are a `HashSetOp`, or a `SetOp` that reads its sides in the order of their columns when that costs less, as `generate_nonunion_paths` makes them. The planner plans each `SELECT` of a side in that order, `INTERSECT` reads first the side with fewer groups, and the rows of the `SetOp` with the sorted strategy take the place of a sort for `ORDER BY`. `connection/plan_setop.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_setop() {
        big_stack(|| script(include_str!("connection/plan_setop.test")));
    }

    /// A `SELECT` of a set operation whose columns need a cast to the types of the operation gets a `Subquery Scan` above its plan that adds the casts, as `generate_setop_tlist` and `recurse_set_operations` make it. An operation below another one with other types gets a `Result` that adds the casts, and the width of an `Append` is the average width of its members, as `create_setop_pathtarget`. `connection/plan_setop_cast.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_setop_cast() {
        big_stack(|| script(include_str!("connection/plan_setop_cast.test")));
    }

    /// A setting `enable_*` that is off makes the nodes of its kind disabled nodes, and the planner takes the plan with the fewest disabled nodes, then the plan with the lowest cost, as `compare_path_costs` compares `disabled_nodes` first. `EXPLAIN` shows `Disabled: true` for a disabled node. `enable_indexonlyscan` off makes index only scans plain index scans, and `enable_mergejoin` and `enable_hashjoin` off take the joins of their kinds away except for a full join, as `add_paths_to_joinrel`. `connection/plan_enable.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_enable() {
        big_stack(|| script(include_str!("connection/plan_enable.test")));
    }

    /// `enable_material` off takes away the `Material` node of a nested loop, of a merge join that does not need it and of a subplan, as `match_unsorted_outer`, `final_cost_mergejoin` and `make_subplan`. `enable_presorted_aggregate` off gives the input of an ordered aggregate call no sort, as `make_pathkeys_for_groupagg`, and with it on the keys of the call of a query with no groups are the keys of its groups, so an index in their order is useful. `connection/plan_enable_material.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_enable_material() {
        big_stack(|| script(include_str!("connection/plan_enable_material.test")));
    }

    /// A constant of a `SELECT` of a set operation with another type than the operation keeps its cast to the type above the `SELECT`, as `generate_setop_tlist` adds the cast after `subquery_planner` folds the `SELECT`. So the plan has a `Subquery Scan` above the `Result` of the constant, also in an operation below another one, and a sorted `SetOp` needs no sort of the constant. `connection/plan_setop_const_cast.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_setop_const_cast() {
        big_stack(|| script(include_str!("connection/plan_setop_const_cast.test")));
    }

    /// `UNION` without `ALL` also has the plan of a `Unique` above a `Merge Append` of the plans of its `SELECT`s in the order of the columns, as `generate_union_paths`, when each `SELECT` has a plan with the keys of all the columns. The `Unique` of a sort and of a `Merge Append` give the rows in the order of the columns, so `ORDER BY` of the columns needs no sort, also when a `SELECT` casts its column. `connection/plan_union_merge.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_union_merge() {
        big_stack(|| script(include_str!("connection/plan_union_merge.test")));
    }

    /// The scan of a subquery of `FROM` has a plan for each plan of the subquery that `add_path` keeps, with the order of the rows of the plan in the columns of the scan, as `set_subquery_pathlist` and `convert_subquery_pathkeys`. So `ORDER BY` and a merge join above the subquery need no sort when the subquery gives its rows in their order, and a `UNION` in `FROM` can use the `Unique` above a `Merge Append`. The scan of a query of `WITH` has the order of the rows of its plan, as `set_cte_pathlist`. A lock clears the order, and the junk columns of the lock make the target list of the subquery not trivial. `connection/plan_subquery_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_subquery_order() {
        big_stack(|| script(include_str!("connection/plan_subquery_order.test")));
    }

    /// `GROUP BY` with no aggregate in the targets or in `HAVING` is a `Group` node of a sorted input, as `create_group_path` makes it, with no cost for each group, as `cost_group`. A hash table of the groups stays a `HashAggregate`. `connection/plan_group_node.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_group_node() {
        big_stack(|| script(include_str!("connection/plan_group_node.test")));
    }

    /// A column of a table whose primary key is in `GROUP BY` can be in the targets and in `HAVING` without an aggregate, as `check_functional_grouping` allows it, also for a key of two columns and a table of a join. A deferrable key, a unique constraint and a subquery do not allow it. A view of such a query depends on the primary key, so `DROP CONSTRAINT` fails. `connection/group_primary_key.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn group_primary_key() {
        big_stack(|| script(include_str!("connection/group_primary_key.test")));
    }

    /// The plan of grouping sets, as `consider_groupingsets_paths` makes it: the sets that one sort gives form a rollup, as `extract_rollup_sets` finds them, and the `Agg` can hash each set, sort the rows once for each rollup, or do both in a `MixedAggregate`, whichever costs less. A sort of the rows for a rollup after the first one shows as `Sort Key` with the group keys of the rollup below it. A call of `GROUPING` costs one operator. `connection/plan_grouping_sets.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_grouping_sets() {
        big_stack(|| script(include_str!("connection/plan_grouping_sets.test")));
    }

    /// A `btree` index of a table with rows has its root page after the metapage, so a nested loop over a scan of the index costs the pages that `index_pages_fetched` reads from the two pages, as `genericcostestimate` costs them for the scans of the loop. The index of an empty table has only the metapage. `connection/plan_nested_loop_index_pages.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_nested_loop_index_pages() {
        big_stack(|| script(include_str!("connection/plan_nested_loop_index_pages.test")));
    }

    /// A subquery in `FROM` on the side of an outer join that can give nulls merges into the query, and each of its columns that the query reads above the join becomes a `PlaceHolderVar`, as `pullup_replace_vars_callback` makes it, unless the expression of the column reads a column of the subquery and is null when its inputs are null. The scan or the join of the relations of the subquery computes the value, as `add_placeholders_to_base_rels` and `add_placeholders_to_joinrel` place it, and each node above reads it as a column, which shows in parentheses. In the `ON` of that join, the column is the expression of the subquery, which can fold to a constant. A `CASE` in `EXPLAIN` takes one line. `connection/plan_placeholder_vars.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_placeholder_vars() {
        big_stack(|| script(include_str!("connection/plan_placeholder_vars.test")));
    }

    /// The keys of the sort of the first window are `window_pathkeys`, which `standard_qp_callback` makes `query_pathkeys` when the query has no `GROUP BY`, so an index scan whose order is the first keys of the window is a useful path, as `truncate_useless_pathkeys` keeps it. Under a `LIMIT`, the `WindowAgg` over that scan with an `Incremental Sort` costs less to start. The `Window` of a node shows only the expressions of the keys, with no direction, as `show_window_keys`. `connection/plan_window_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_window_order() {
        big_stack(|| script(include_str!("connection/plan_window_order.test")));
    }

    /// The cost of a `Sample Scan`, as `set_tablesample_rel_size` and `cost_samplescan` find it: `SYSTEM` reads the fraction of the pages of the table, each at the cost of a random page, and `BERNOULLI` reads each page in order. `Sampling` shows `REPEATABLE` on the same line in the text form, and the other forms have a property for each part. A `ModifyTable` with `RETURNING` has the rows and the width of the node below it, as `create_modifytable_path`. `connection/plan_sample_returning.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_sample_returning() {
        big_stack(|| script(include_str!("connection/plan_sample_returning.test")));
    }

    /// `EXPLAIN VERBOSE` of a `MERGE` shows the targets of `preprocess_targetlist`: the ID of the version of the row of the target table, the columns of the source that the `WHEN` clauses read, then the ID of the version of the row of the source. `connection/plan_merge_targets.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_merge_targets() {
        big_stack(|| script(include_str!("connection/plan_merge_targets.test")));
    }

    /// `EXPLAIN VERBOSE` of an `INSERT`, an `UPDATE`, a `DELETE` and a `MERGE` with `RETURNING` shows the list of `RETURNING` as the `Output` of the `ModifyTable`, and the node below gives the columns of the other relations that the list reads after the row marks. `connection/plan_returning_output.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_returning_output() {
        big_stack(|| script(include_str!("connection/plan_returning_output.test")));
    }

    /// `pull_up_subqueries` and `pull_up_sublinks` on an `UPDATE`, a `DELETE`, a `MERGE` and an `INSERT`: a simple subquery of `FROM` or of the source merges into the query, and `EXISTS`, `NOT EXISTS` and `IN` become joins, also for a table with check constraints. The target table of an `UPDATE`, a `DELETE` or a `MERGE` stays the relation 0. `connection/plan_write_pullup.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_write_pullup() {
        big_stack(|| script(include_str!("connection/plan_write_pullup.test")));
    }

    /// `convert_ANY_sublink_to_join` on a correlated subquery: the subquery of `IN` is a `LATERAL` relation of the semi join, which `pull_up_subqueries` then merges, so that its `WHERE` joins the conditions of the semi join. A `LATERAL` subquery of `FROM` merges too. The columns that a join reads come in the order of its conditions in the join tree. `connection/plan_correlated_in.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_correlated_in() {
        big_stack(|| script(include_str!("connection/plan_correlated_in.test")));
    }

    /// `remove_useless_joins`: `EXPLAIN` takes out a left join to a table with a unique index on the columns of the condition of the join, or to a subquery with `DISTINCT` or `GROUP BY` on them, when the query reads no column of the inner side outside the condition. A join with other conditions, a cast of the inner column, a partial index, a lock or a column of the inner side in the targets keeps its join. `connection/plan_join_removal.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_join_removal() {
        big_stack(|| script(include_str!("connection/plan_join_removal.test")));
    }

    /// `RETURNING` with `old` and `new` and `MERGE_ACTION()` gives the old and the new row, and `EXPLAIN VERBOSE` shows the list in the `Output` of the `ModifyTable` as `set_deparse_context_plan` names it. `connection/plan_returning_old_new.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_returning_old_new() {
        big_stack(|| script(include_str!("connection/plan_returning_old_new.test")));
    }

    /// The plan of a `MERGE` joins the target table and the source with the target table on the left side, as `transform_MERGE_to_join` makes it. With `NOT MATCHED BY SOURCE`, the rows carry the whole row of the source and the columns of the join condition, and the plan has no scan of the marker of the source rows. `connection/plan_merge_join.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_merge_join() {
        big_stack(|| script(include_str!("connection/plan_merge_join.test")));
    }

    /// `EXPLAIN` uses an index on an expression for a condition and an order of the same expression, also for a parameterized scan of a join, and a partial index when the conditions imply its predicate. The scan does not test a condition that the predicate implies, and `=` of the expression of a unique index passes one row. `connection/plan_index_expressions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_index_expressions() {
        big_stack(|| script(include_str!("connection/plan_index_expressions.test")));
    }

    /// Unique indexes on expressions and partial unique indexes in the writes: a duplicate of `lower(email)` in `INSERT` and `UPDATE`, `ON CONFLICT` with an expression and with the predicate of a partial index, the error of an inference that no index matches, a key of a column and an expression, `CREATE UNIQUE INDEX` on rows with a duplicate, and an insert of `MERGE`. `connection/unique_expressions.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn unique_expressions() {
        big_stack(|| script(include_str!("connection/unique_expressions.test")));
    }

    /// The deferrable unique constraints and primary keys: the check at the end of the statement, an update that moves the keys, `INITIALLY DEFERRED` and `SET CONSTRAINTS` with the check at the commit, nulls, a rollback to a savepoint, the error of `ON CONFLICT` with a deferrable arbiter, a deferrable index that is not an arbiter, the internal trigger in `pg_trigger` and `pg_depend`, a new constraint on duplicate rows, `DROP CONSTRAINT`, and the partitions of a partitioned table. `connection/deferrable_unique.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn deferrable_unique() {
        big_stack(|| script(include_str!("connection/deferrable_unique.test")));
    }

    /// `LEAKPROOF` and `NOT LEAKPROOF` in `CREATE FUNCTION`, `CREATE OR REPLACE FUNCTION` and `ALTER FUNCTION`, with `proleakproof`, the error for a procedure, the error for a role that is not a superuser and a second `LEAKPROOF`. `connection/leakproof.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn leakproof() {
        big_stack(|| script(include_str!("connection/leakproof.test")));
    }

    /// A join of parts of `FROM` that have no condition between them in the written order joins first the parts that have an `=` between them, so that no cross join of two tables makes more tuples than a join can keep. `connection/join_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn join_order() {
        big_stack(|| script(include_str!("connection/join_order.test")));
    }

    /// `GRANT` and `REVOKE` on tables, sequences, views and their columns: the `relacl` and `attacl` values, the grant options with `CASCADE`, the grantor that a role with the privileges of the owner gets, `GRANT ... ON ALL TABLES IN SCHEMA`, the errors for a bad privilege or object, and the privileges that `DROP ROLE` lists. `connection/grant_relation.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn grant_relation() {
        big_stack(|| script(include_str!("connection/grant_relation.test")));
    }

    /// `GRANT` and `REVOKE` on schemas, functions, procedures, types, domains and languages: the `nspacl`, `proacl`, `typacl` and `lanacl` values, `ON ALL ROUTINES IN SCHEMA`, the checks of the `has_*_privilege` functions, the errors for an array type and an untrusted language, and the privileges that `DROP ROLE` lists. `connection/grant_object.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn grant_object() {
        big_stack(|| script(include_str!("connection/grant_object.test")));
    }

    /// The privileges that a statement needs on the tables and the columns that it reads and writes: `SELECT`, `INSERT`, `UPDATE` and `DELETE` with grants on columns, `RETURNING`, `ON CONFLICT`, `MERGE`, `FOR UPDATE`, a view as its owner and with `security_invoker`, and the system catalogs. `connection/privdml.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn dml_privileges() {
        big_stack(|| script(include_str!("connection/privdml.test")));
    }

    /// A query with `HAVING` and no aggregate and no `GROUP BY`, which does not read `FROM`, the overflow of `make_interval`, and the `regrole` and `regdatabase` constants that a stored expression cannot hold. `connection/smallreg.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn degenerate_grouping_and_shared_constants() {
        big_stack(|| script(include_str!("connection/smallreg.test")));
    }

    /// `CREATE FUNCTION` in the languages `internal` and `c`: the functions of `regress.so` that rupg runs, a function in `internal` that runs a built-in function, the errors of `fmgr_c_validator` and `fmgr_internal_validator`, the errors of `AS`, the rows of `pg_proc`, `LOAD`, and the error for a role that is not a superuser. `connection/cfunc.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn functions_in_c_and_internal() {
        big_stack(|| script(include_str!("connection/cfunc.test")));
    }

    /// `CREATE OPERATOR` and `DROP OPERATOR`: an operator of a built-in function or of a function in SQL, the shells that a commutator or a negator makes and a later `CREATE OPERATOR` fills, the links of `oprcom` and `oprnegate`, the errors of `DefineOperator` and `OperatorCreate`, `COMMENT ON OPERATOR`, the description of an operator in a list of dependencies, and the notices of `IF EXISTS`. `connection/oper.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn create_and_drop_operators() {
        big_stack(|| script(include_str!("connection/oper.test")));
    }

    /// `ALTER OPERATOR ... SET` with the estimators, the commutator, the negator, `MERGES` and `HASHES`, which can only be set once, and `ALTER OPERATOR ... OWNER TO` and `SET SCHEMA` with the checks of the owner and of the schema. `connection/alteroper.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn alter_operators() {
        big_stack(|| script(include_str!("connection/alteroper.test")));
    }

    /// `CREATE OPERATOR FAMILY`, `CREATE OPERATOR CLASS`, `ALTER OPERATOR FAMILY ... ADD` and `... DROP`, and `DROP OPERATOR CLASS` and `DROP OPERATOR FAMILY`, with the rows of `pg_opfamily`, `pg_opclass`, `pg_amop`, `pg_amproc` and `pg_depend`, and the errors of the checks of `DefineOpClass` and `AlterOpFamily`. `connection/opclass.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn opclass_commands() {
        big_stack(|| script(include_str!("connection/opclass.test")));
    }

    /// Operator classes of the catalog in `PARTITION BY` and `CREATE INDEX`: a row of a table that `PARTITION BY HASH` partitions goes to the partition that the support function 2 of the class gives, the pruning of `EXPLAIN` uses the same hash, `pg_get_partkeydef` and `pg_get_indexdef` show the names of the classes, and `DROP OPERATOR CLASS` finds the tables and the indexes that use the class. `connection/opclass_partition.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn opclass_partition() {
        big_stack(|| script(include_str!("connection/opclass_partition.test")));
    }

    /// `ALTER OPERATOR CLASS` and `ALTER OPERATOR FAMILY` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`: the checks of the owner, of the role, of `CREATE` on the schema and of the name, and the dependency on the schema. `connection/opclass_alter.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn opclass_alter() {
        big_stack(|| script(include_str!("connection/opclass_alter.test")));
    }

    /// `CREATE AGGREGATE` in the old and the new syntax: the checks of `DefineAggregate` and `AggregateCreate`, the rows of `pg_aggregate`, `pg_proc` and `pg_depend`, aggregates with user transition and final functions, `CREATE OR REPLACE AGGREGATE` and `DROP AGGREGATE`. `connection/aggcreate.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn aggcreate() {
        big_stack(|| script(include_str!("connection/aggcreate.test")));
    }

    /// `ALTER AGGREGATE` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`, and `ALTER FUNCTION` and `ALTER ROUTINE` on an aggregate and on its support functions, with the errors of the lookup, of the owner and of a name that the schema has. A view that calls a renamed aggregate shows the new name. `connection/aggalter.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn aggalter() {
        big_stack(|| script(include_str!("connection/aggalter.test")));
    }

    /// `inline_function` for `textanycat`, `anytextcat`, `quote_literal(anyelement)` and `quote_nullable(anyelement)`: the plan casts the argument of any type to `text` and calls the function of the body, which `EXPLAIN` shows, and the cast and the call fold with constant arguments. A view keeps the call. `connection/inlinecat.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inlinecat() {
        big_stack(|| script(include_str!("connection/inlinecat.test")));
    }

    /// A function that gives a set in the target list stops after the rows that `LIMIT` reads, as a function in the value-per-call mode does: `generate_series` of the timestamps to infinity, of the integers and of `numeric`, with `OFFSET`, with more than one call, with nested calls and with an error after the last row that `LIMIT` reads. `connection/srflimit.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn srflimit() {
        big_stack(|| script(include_str!("connection/srflimit.test")));
    }

    /// A function in SQL with a body in a string can call itself, because `ProcedureCreate` adds the function before `fmgr_sql_validator` reads the body. A body of `BEGIN ATOMIC` or `RETURN` cannot, and a body with an error leaves no function. A function that calls itself with no end gives the error of `check_stack_depth`, which the scripts catch in PL/pgSQL. `connection/sqlrecurse.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqlrecurse() {
        big_stack(|| script(include_str!("connection/sqlrecurse.test")));
    }

    /// `ALTER TABLE ... INHERIT` and `ALTER TABLE ... NO INHERIT` give the errors of `ATPrepChangeInherit` for a typed table and for a partition before the other checks. `connection/inherittyped.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn inherittyped() {
        big_stack(|| script(include_str!("connection/inherittyped.test")));
    }

    /// `CREATE OPERATOR` needs `USAGE` on the argument types and on the result type, and `EXECUTE` on the function and on the estimators. An array type has the privileges of its element type, and the error names the element type. `connection/opperm.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn opperm() {
        big_stack(|| script(include_str!("connection/opperm.test")));
    }

    /// `jsonb_hash` and `jsonb_hash_extended` hash the keys, the values and the elements in the order of `JsonbIteratorNext`, so a `jsonb` column can be the key of `PARTITION BY HASH`. `hash_aclitem_extended` takes a seed that is not 0, and `hash_array_extended` and `hash_record_extended` name the extended hash function in their error. `connection/hashjsonb.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn hashjsonb() {
        big_stack(|| script(include_str!("connection/hashjsonb.test")));
    }

    /// Stored generated columns, from the first part of `generated_stored.sql` of the PostgreSQL tests: the errors of `CREATE TABLE` for a generated column, the values that `INSERT`, `UPDATE` and `MERGE` compute, the errors for a value that is not `DEFAULT`, and the columns of `information_schema`. `connection/gstored.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored() {
        big_stack(|| script(include_str!("connection/gstored.test")));
    }

    /// Stored generated columns and inheritance, from the second part of `generated_stored.sql` of the PostgreSQL tests: the rules of `MergeAttributes` and of `ALTER TABLE ... INHERIT` for a generated column, the expression of a child that replaces the expression of its parent, and `COPY` of a table with a generated column. `connection/gstored2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored2() {
        big_stack(|| script(include_str!("connection/gstored2.test")));
    }

    /// Stored generated columns, from the third part of `generated_stored.sql` of the PostgreSQL tests: a null expression, `varchar` and composite values, `ADD COLUMN` of a generated column, `DROP COLUMN` of a column that a generation expression reads, and the privileges of the columns and of a function that an expression calls. `connection/gstored3.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored3() {
        big_stack(|| script(include_str!("connection/gstored3.test")));
    }

    /// Stored generated columns, from the fourth part of `generated_stored.sql` of the PostgreSQL tests: check constraints and not-null constraints on generated columns, `ALTER COLUMN ... SET EXPRESSION` with the checks of the existing rows and the indexes on the column, and generated columns of partitioned tables with not-null constraints. `connection/gstored4.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored4() {
        big_stack(|| script(include_str!("connection/gstored4.test")));
    }

    /// Stored generated columns, from the fifth part of `generated_stored.sql` of the PostgreSQL tests: unique constraints, primary keys and indexes on generated columns, with the plans that read the indexes after `SET EXPRESSION`, foreign keys that refer to and from generated columns, and generated columns of domains and of range types over domains. `connection/gstored5.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored5() {
        big_stack(|| script(include_str!("connection/gstored5.test")));
    }

    /// Stored generated columns, from the sixth part of `generated_stored.sql` of the PostgreSQL tests: generated columns of partitions, with the checks of `CREATE TABLE ... PARTITION OF` and `ATTACH PARTITION`, `SET EXPRESSION` on a partitioned table and on a partition, generated columns in a partition key, and `ADD COLUMN` of generated columns with other subcommands. `connection/gstored6.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored6() {
        big_stack(|| script(include_str!("connection/gstored6.test")));
    }

    /// Stored generated columns, from the seventh part of `generated_stored.sql` of the PostgreSQL tests: `ALTER COLUMN ... TYPE` of a generated column and of a column that a generated column reads, `DROP COLUMN` with `ADD COLUMN` in one statement, `SET EXPRESSION` and `DROP EXPRESSION` on plain columns, and `DROP EXPRESSION` on a table with children of `INHERITS`. `connection/gstored7.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored7() {
        big_stack(|| script(include_str!("connection/gstored7.test")));
    }

    /// Stored generated columns, from the eighth part of `generated_stored.sql` of the PostgreSQL tests: `DROP EXPRESSION` in a tree of partitions, and the changes of a generated column of a table whose row type another table, a check constraint or an index uses. `connection/gstored8.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored8() {
        big_stack(|| script(include_str!("connection/gstored8.test")));
    }

    /// Stored generated columns, from the ninth part of `generated_stored.sql` of the PostgreSQL tests: the `WHEN` conditions and the row values of triggers on a table with a generated column, `UPDATE OF` a generated column, and a `BEFORE` trigger that changes a generated column. `connection/gstored9.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored9() {
        big_stack(|| script(include_str!("connection/gstored9.test")));
    }

    /// Stored generated columns, from the part of `generated_stored.sql` of the PostgreSQL tests on rules: the rules `DO ALSO` of `INSERT` and `UPDATE` see the new value of a generated column in `NEW`, and a rule `DO INSTEAD NOTHING` with a condition on a generated column stops the `UPDATE`. `connection/gstored10.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored10() {
        big_stack(|| script(include_str!("connection/gstored10.test")));
    }

    /// Stored generated columns, from the part of `generated_stored.sql` of the PostgreSQL tests on triggers: the condition `WHEN` of a trigger `BEFORE` cannot read a generated column of `NEW`, and the new row of a trigger `BEFORE` has nulls for the generated columns, also after a dropped column and for `ON CONFLICT DO UPDATE`. `connection/gstored11.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gstored11() {
        big_stack(|| script(include_str!("connection/gstored11.test")));
    }

    /// Automatically updatable views, as `rewriteTargetView` writes them: `INSERT`, `UPDATE` and `DELETE` of a view write its base relation, with the conditions of the view, the names of the columns of the view, `RETURNING`, views on views and `security_barrier`. The script also has the errors of a view that is not automatically updatable and of a column that is not updatable, the triggers of the base relation, and the privileges of the owner of the view. `connection/autoview1.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn autoview1() {
        big_stack(|| script(include_str!("connection/autoview1.test")));
    }

    /// Automatically updatable views, as `rewriteTargetView` writes them: `INSERT` with `VALUES`, `SELECT` and the defaults of the base table, `UPDATE ... FROM`, `DELETE ... USING`, writes of views in `WITH`, a partitioned base table, a table with children of `INHERITS` and `ONLY`, nested views with `security_barrier`, rules of the view, prepared statements and `EXPLAIN`. `connection/autoview2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn autoview2() {
        big_stack(|| script(include_str!("connection/autoview2.test")));
    }

    /// `WITH CHECK OPTION` of automatically updatable views, from `updatable_views.sql` of the PostgreSQL tests: `LOCAL` and `CASCADED` checks of views on views, the option `check_option` of `ALTER VIEW`, a view with no condition, conditions with arrays and subqueries, and a trigger `BEFORE` on the base table that changes the row. `connection/wco1.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn wco1() {
        big_stack(|| script(include_str!("connection/wco1.test")));
    }

    /// `WITH CHECK OPTION` of automatically updatable views: the errors for a view that is not automatically updatable and for a bad option, a check with `security_barrier`, the detail of a long value and of nulls, writes in `WITH`, a duplicate key before the check, a trigger `AFTER` that the failed check stops, generated columns, a partitioned base table and a table with children of `INHERITS`. `connection/wco2.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn wco2() {
        big_stack(|| script(include_str!("connection/wco2.test")));
    }

    /// `COPY TO` with `FORMAT json`, from PostgreSQL `copy.sql`: a line with the row as `composite_to_json` writes it, `FORCE_ARRAY`, the column lists, the types, the escapes, partitions and the option errors.
    #[test]
    fn copyjson() {
        big_stack(|| script(include_str!("connection/copyjson.test")));
    }

    /// `CREATE STATISTICS`, `ALTER STATISTICS`, `DROP STATISTICS` and `COMMENT ON STATISTICS`, from `stats_ext.sql` of the PostgreSQL tests: the errors of the definition, the rows of `pg_statistic_ext` and `pg_depend`, the functions of the definition, the checks of the owner, and the names in the temporary schema and in other schemas. `connection/stx1.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn stx1() {
        big_stack(|| script(include_str!("connection/stx1.test")));
    }

    /// Virtual generated columns, from `generated_virtual.sql` of the PostgreSQL tests: the errors of the definition, `INSERT` and `UPDATE` with `DEFAULT`, whole-row and sublink reads, `UPDATE ... RETURNING old.*, new.*`, `MERGE`, views, CTEs, inheritance, check and not-null constraints, and reads on the nullable side of an outer join. The script does not have the psql meta-commands and the `COPY`, privilege and `ALTER TABLE` parts. `connection/gvirt.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn gvirt() {
        big_stack(|| script(include_str!("connection/gvirt.test")));
    }

    /// The input of `regclass` in the defaults, the generation expressions and the check constraints of `CREATE TABLE` finds the new table, because PostgreSQL makes the table before it reads these expressions. `connection/regnew.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn regnew() {
        big_stack(|| script(include_str!("connection/regnew.test")));
    }

    /// `CREATE TABLE ... (LIKE ... INCLUDING STATISTICS)`, from `create_table_like.sql` of the PostgreSQL tests: the copies of the statistics objects with the names that `ChooseExtendedStatisticName` gives, their kinds, their expressions with the column numbers of the new table, and their comments with `INCLUDING COMMENTS`. `connection/ctlstat.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn ctlstat() {
        big_stack(|| script(include_str!("connection/ctlstat.test")));
    }

    /// `DROP OWNED` and `REASSIGN OWNED`, from `dependency.sql` of the PostgreSQL tests: the checks of the privileges, the revoke of the grants of the role, the drop of its objects and of the grants of membership that it made, and the new owner of its objects. The script has catalog queries in place of `\z` and `\d`, and it does not have default privileges and the grant on the database. `connection/dropowned.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn dropowned() {
        big_stack(|| script(include_str!("connection/dropowned.test")));
    }

    /// `ALTER DEFAULT PRIVILEGES`: the rows of `pg_default_acl`, the errors of the statement, the ACLs of the new tables, views, sequences, types, functions and schemas, `pg_describe_object` of an entry, the entries of a schema that goes, and `DROP OWNED` and `DROP ROLE` with default privileges. The script does not have large objects and schema elements. `connection/defacl.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn defacl() {
        big_stack(|| script(include_str!("connection/defacl.test")));
    }

    /// `CREATE SCHEMA` with schema elements: the errors for an element in another schema and for a temporary table, the order in which the elements run, the owner and the grants of the new objects, the foreign keys of the tables, the text of a view, `search_path` after the statement, and an error in an element, which takes the schema away. `connection/schemaelts.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn schemaelts() {
        big_stack(|| script(include_str!("connection/schemaelts.test")));
    }

    /// The ID and the snapshot of a transaction: `pg_current_xact_id`, `txid_current` and their `_if_assigned` forms, which a write, a change of the catalog and `LOCK ... ACCESS EXCLUSIVE` assign and a read does not, `pg_current_snapshot` and `txid_current_snapshot` without the transaction of the session, and `pg_xact_status` and `txid_status` of a committed, an aborted and a running transaction, of the special IDs and of an ID in the future. `connection/xactid.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn xactid() {
        big_stack(|| script(include_str!("connection/xactid.test")));
    }

    /// `backend_xid` and `backend_xmin` of `pg_stat_activity` in two sessions: the `xmin` of the snapshot of a transaction in `REPEATABLE READ` that stays out of a statement, the `xmin` of a statement in `READ COMMITTED` that goes at its end, the ID of a transaction that writes, and the `xmin` of `pg_current_snapshot` that is the transaction of the session. `connection/backend_xids.test` is the output of PostgreSQL 19 for the same script on two connections to a new database.
    #[test]
    fn backend_xids() {
        big_stack(|| two_sessions(include_str!("connection/backend_xids.test")));
    }

    /// The system columns `xmin` and `xmax` of the rows of a table, a partitioned table and a table with children of `INHERITS`: the transaction that inserted the row, the transaction that deleted it, the ID of an aborted transaction that deleted it, and an `UPDATE` that checks `xmin`. `connection/xmin.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn xmin() {
        big_stack(|| script(include_str!("connection/xmin.test")));
    }

    /// `age` and `mxid_age` of an `xid`: the IDs that are not normal, the age of `datfrozenxid` and `datminmxid`, and the age of the ID of the transaction and of `xmin`. `connection/xidage.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn xidage() {
        big_stack(|| script(include_str!("connection/xidage.test")));
    }

    /// `SET` of `CREATE FUNCTION` and `ALTER FUNCTION`: `proconfig` keeps each setting, `pg_get_functiondef` shows it, and a call runs with the settings, which go back at the end of the call, other than one that a `SET` in the call changed. `connection/fnset.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn fnset() {
        big_stack(|| script(include_str!("connection/fnset.test")));
    }

    /// `SECURITY DEFINER`: a call runs as the owner of the function, for the privileges, `current_user` and the owner of a new table, and `SET ROLE` cannot run in the call. `connection/secdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn secdef() {
        big_stack(|| script(include_str!("connection/secdef.test")));
    }

    /// A function in SQL with more than one statement, or with `INSERT`, `UPDATE` or `DELETE`: the statements run one after the other, each one sees the changes of the statements before it, and the last statement gives the result, also from `RETURNING`. An error has the number of the statement in its context, and `check_function_bodies` lets the body use a table that an earlier statement makes. `connection/sqlfn.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn sqlfn() {
        big_stack(|| script(include_str!("connection/sqlfn.test")));
    }

    /// `CREATE TABLE ... (LIKE ...)`, from `create_table_like.sql` of the PostgreSQL tests: the columns and the not-null constraints of the other relation, the options `INCLUDING DEFAULTS`, `GENERATED`, `IDENTITY`, `CONSTRAINTS`, `INDEXES`, `STORAGE`, `COMMENTS` and `ALL`, `LIKE` with `INHERITS`, and `LIKE` of a view, a composite type and a sequence. The script has catalog queries in place of `\d`, and it does not have the parts for virtual generated columns, `NOT ENFORCED`, `CREATE STATISTICS` and foreign tables. `connection/create_table_like.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn create_table_like() {
        big_stack(|| script(include_str!("connection/create_table_like.test")));
    }

    /// `ADD CONSTRAINT ... NOT VALID` of a check constraint, which does not check the rows that the table has, and `VALIDATE CONSTRAINT`, which checks them and marks the constraint valid on the table and on its children first. A whole-row reference in a check constraint or in an index shows as `table.*`. `connection/notvalid.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn notvalid() {
        big_stack(|| script(include_str!("connection/notvalid.test")));
    }

    /// A query that is a `UNION ALL` of `SELECT`s with the types of its columns and has `ORDER BY` becomes an append relation, as `flatten_simple_union_all` makes it, so the planner can plan a `Merge Append` of the plans of the `SELECT`s in the order. A `SELECT` with its own `WHERE` is not a member of the append relation, as `is_safe_append_member`, and its plan gets a sort. `connection/plan_union_all_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_union_all_order() {
        big_stack(|| script(include_str!("connection/plan_union_all_order.test")));
    }

    /// `EXPLAIN` of the bitmap heap scans of a table by its btree indexes, as `create_index_paths` makes them with `choose_bitmap_and` and the costs of `cost_bitmap_heap_scan`. `connection/plan_bitmap_scan.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_bitmap_scan() {
        big_stack(|| script(include_str!("connection/plan_bitmap_scan.test")));
    }

    /// `EXPLAIN` of the scans of a table by its btree indexes, as `create_index_paths` makes them with the costs of `btcostestimate` and `cost_index`. A condition on a key column is an `Index Cond`, and the other conditions are the filter. A scan in the order of the index, forward or backward, takes the place of a sort for `ORDER BY`, and an index that has each column that the query reads gives an `Index Only Scan`. `connection/plan_index_scan.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_index_scan() {
        big_stack(|| script(include_str!("connection/plan_index_scan.test")));
    }

    /// `EXPLAIN` of targets that call functions, as `set_pathtarget_cost_width` finds their costs. The plan of `FROM` computes the targets before the sort, or the keys of `GROUP BY` with groups, or the keys of the windows with windows, and each scan of a partitioned table computes them. The `Agg` node, the last `WindowAgg` node and the `HashAggregate` of `DISTINCT` add the cost of their targets, in which an aggregate call costs nothing. `connection/plan_target_cost.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_target_cost() {
        big_stack(|| script(include_str!("connection/plan_target_cost.test")));
    }

    /// `EXPLAIN` of an `OR` of a join whose arms each have a part for one relation, as `extract_restriction_or_clauses` makes an `OR` of those parts a condition of the scan of the relation when it passes 0.9 of the rows at most. The `OR` stays a condition of the join, and its selectivity for an inner join is divided by the selectivity of the new condition, so that the rows of the join stay the same. `connection/plan_or_extract.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_or_extract() {
        big_stack(|| script(include_str!("connection/plan_or_extract.test")));
    }

    /// `EXPLAIN` of `LIKE`, `ILIKE`, regular expressions and `^@` of a column and a constant pattern, as `patternsel` estimates them with no statistics: a pattern that is its fixed prefix is `=` of the prefix, and the other patterns take `DEFAULT_MATCH_SEL` for a fixed prefix times the selectivity of the rest of the pattern. `connection/plan_pattern.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_pattern() {
        big_stack(|| script(include_str!("connection/plan_pattern.test")));
    }

    /// `EXPLAIN` of an `Incremental Sort`: the planner sorts rows whose first keys are in order already, as `make_ordered_path` and `create_ordered_paths` choose it, with the costs of `cost_incremental_sort`. `connection/plan_incremental_sort.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_incremental_sort() {
        big_stack(|| script(include_str!("connection/plan_incremental_sort.test")));
    }

    /// `EXPLAIN` of `ORDER BY`, `GROUP BY`, `DISTINCT`, windows and `LIMIT` over a join. The planner reads each plan of the join that `add_path` keeps, so a merge join whose rows are in the order of the query can cost less in total than the cheapest join with a sort above it, as `create_ordered_paths` finds it. `connection/plan_upper_order.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_upper_order() {
        big_stack(|| script(include_str!("connection/plan_upper_order.test")));
    }

    /// `EXPLAIN` of joins whose conditions an equivalence class with no constant makes, as `generate_join_implied_equalities`: one condition for each class at each join, of the first members of the two sides, also for two tables that the query does not compare. `connection/plan_join_classes.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn plan_join_classes() {
        big_stack(|| script(include_str!("connection/plan_join_classes.test")));
    }

    /// `pg_get_partition_constraintdef` for list, range and hash partitions, default partitions before and after other partitions, a key of more than one column with `MINVALUE` and `MAXVALUE`, a key of expressions, a key column whose type is not the one of the operator class, a partition of a partition with its columns in another order, and a table that is not a partition. `connection/partition_constraintdef.test` is the output of PostgreSQL 19 for the same script in a new database.
    #[test]
    fn partition_constraintdef() {
        big_stack(|| script(include_str!("connection/partition_constraintdef.test")));
    }

    /// The planner takes the parameters of `Bind` as constants, so an error of the fold comes at `Bind` and not at `Parse`. The messages are the messages of PostgreSQL 19 for the same input.
    #[test]
    fn fold_at_bind() {
        big_stack(|| {
            let (mut c, _) = connect();
            let input = [parse("f", "SELECT $1 / 0", &[23]), describe('S', "f"), sync()];
            assert_eq!(
                send(&mut c, &input.concat()),
                ["ParseComplete", "params 23", "columns ?column?", "ready I"]
            );
            let run = [describe('P', ""), execute("", 0), sync()].concat();
            let input = [bind("", "f", &[], &[Some(b"1")], &[]), run.clone()];
            assert_eq!(send(&mut c, &input.concat()), ["ERROR 22012 division by zero", "ready I"]);
            let input = [bind("", "f", &[], &[None], &[]), run.clone()];
            assert_eq!(
                send(&mut c, &input.concat()),
                ["BindComplete", "columns ?column?", "row NULL", "SELECT 1", "ready I"]
            );
            let input = [parse("", "SELECT 1 / 0", &[]), sync()];
            assert_eq!(send(&mut c, &input.concat()), ["ParseComplete", "ready I"]);
            let input = [bind("", "", &[], &[], &[]), run];
            assert_eq!(send(&mut c, &input.concat()), ["ERROR 22012 division by zero", "ready I"]);
        });
    }

    /// `PortalRunSelect` sends each row when the executor makes it, so the rows before a row whose target fails reach the client before the error. A query with a sort makes all its rows first, so it sends none. In the extended protocol the error comes only when the client wants the row that fails, so `Execute` with a limit of 3 rows gives `PortalSuspended`. PostgreSQL 18 gives the same messages.
    #[test]
    fn rows_before_an_error() {
        big_stack(rows_before_an_error_cases);
    }

    fn rows_before_an_error_cases() {
        let (mut c, _) = connect();
        let sql = "SELECT 6 / g AS n FROM generate_series(3, -1, -1) g";
        let rows = ["row 2", "row 3", "row 6"];
        let error = "ERROR 22012 division by zero";
        assert_eq!(
            send(&mut c, &query(sql)),
            [&["columns n"][..], &rows, &[error, "ready I"]].concat()
        );
        assert_eq!(
            send(&mut c, &query(&format!("{sql} ORDER BY n"))),
            ["columns n", error, "ready I"]
        );
        assert_eq!(
            send(&mut c, &query(&format!("{sql} LIMIT 2 OFFSET 1"))),
            ["columns n", "row 3", "row 6", "SELECT 2", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("SELECT g, 1 / (g - 1) AS n FROM generate_series(3, 0, -1) g")),
            ["columns g,n", "row 3,0", "row 2,1", error, "ready I"]
        );
        assert_eq!(send(&mut c, &parse("s", sql, &[])), ["ParseComplete"]);
        let mut run = |maxes: &[i32]| {
            let mut input = bind("", "s", &[], &[], &[]);
            for &max in maxes {
                input.extend(execute("", max));
            }
            input.extend(sync());
            send(&mut c, &input)
        };
        assert_eq!(run(&[0]), [&["BindComplete"][..], &rows, &[error, "ready I"]].concat());
        assert_eq!(
            run(&[2, 2]),
            ["BindComplete", "row 2", "row 3", "PortalSuspended", "row 6", error, "ready I"]
        );
        assert_eq!(
            run(&[3]),
            [&["BindComplete"][..], &rows, &["PortalSuspended", "ready I"]].concat()
        );
        assert_eq!(
            run(&[3, 1]),
            [&["BindComplete"][..], &rows, &["PortalSuspended", error, "ready I"]].concat()
        );
        assert_eq!(run(&[4]), [&["BindComplete"][..], &rows, &[error, "ready I"]].concat());
        assert_eq!(
            send(&mut c, &query("SELECT 1 AS n")),
            ["columns n", "row 1", "SELECT 1", "ready I"]
        );
    }

    #[test]
    fn extended_selects() {
        big_stack(extended_selects_cases);
    }

    fn extended_selects_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(
                &mut c,
                &[parse("s", "SELECT $1 + 1 AS n", &[]), describe('S', "s"), sync()].concat()
            ),
            ["ParseComplete", "params 23", "columns n", "ready I"]
        );
        assert_eq!(
            send(
                &mut c,
                &[bind("", "s", &[], &[Some(b"41")], &[]), execute("", 0), sync()].concat()
            ),
            ["BindComplete", "row 42", "SELECT 1", "ready I"]
        );
        let input = [
            bind("", "s", &[1], &[Some(&41_i32.to_be_bytes())], &[1]),
            describe('P', ""),
            execute("", 1),
            execute("", 1),
            sync(),
        ];
        assert_eq!(
            send(&mut c, &input.concat()),
            [
                "BindComplete",
                "columns n/1",
                "row \0\0\0*",
                "PortalSuspended",
                "SELECT 0",
                "ready I"
            ]
        );
        assert_eq!(
            send(&mut c, &[bind("", "s", &[], &[None], &[]), execute("", 0), sync()].concat()),
            ["BindComplete", "row NULL", "SELECT 1", "ready I"]
        );
        assert_eq!(
            send(&mut c, &[bind("", "s", &[], &[Some(b"x")], &[]), sync()].concat()),
            [
                "ERROR 22P02 invalid input syntax for type integer: \"x\" (unnamed portal parameter $1 = '...')",
                "ready I"
            ]
        );
        send(&mut c, &query("SET log_parameter_max_length_on_error = 3"));
        assert_eq!(
            send(&mut c, &[bind("p", "s", &[], &[Some("ab'éz".as_bytes())], &[]), sync()].concat())
                [0],
            "ERROR 22P02 invalid input syntax for type integer: \"ab'éz\" (portal \"p\" parameter $1 = 'ab''...')"
        );
    }

    #[test]
    fn queries() {
        big_stack(queries_cases);
    }

    fn notices_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(&mut c, &query("COMMIT")),
            ["WARNING 25P01 there is no transaction in progress", "COMMIT", "ready I"]
        );
        assert_eq!(
            send(&mut c, &query("SET client_min_messages = error; COMMIT")),
            ["SET", "COMMIT", "ready I"]
        );
    }

    #[test]
    fn extended_queries() {
        big_stack(extended_queries_cases);
    }

    fn extended_queries_cases() {
        let (mut c, _) = connect();
        let input = [parse("", "SHOW work_mem", &[]), bind("", "", &[], &[], &[])].concat();
        let input = [input, describe('P', ""), execute("", 0), sync()].concat();
        assert_eq!(
            send(&mut c, &input),
            ["ParseComplete", "BindComplete", "columns work_mem", "row 4MB", "SHOW", "ready I"]
        );
        let input = [parse("s", "SET work_mem = '8MB'", &[23]), describe('S', "s")].concat();
        let input =
            [input, bind("", "s", &[], &[Some(b"5")], &[]), execute("", 0), sync()].concat();
        assert_eq!(
            send(&mut c, &input),
            ["ParseComplete", "params 23", "NoData", "BindComplete", "SET", "ready I"]
        );
        assert_eq!(send(&mut c, &[run("SHOW work_mem"), sync()].concat())[2], "row 8MB");
        assert_eq!(
            send(&mut c, &[run(""), sync()].concat()),
            ["ParseComplete", "BindComplete", "empty", "ready I"]
        );
        // A row limit suspends the portal, and the next Execute goes on from there.
        let input = [parse("", "SHOW work_mem", &[]), bind("", "", &[], &[], &[1])].concat();
        let input = [input, describe('P', ""), execute("", 1), execute("", 1), execute("", 0)];
        assert_eq!(
            send(&mut c, &[input.concat(), sync()].concat()),
            [
                "ParseComplete",
                "BindComplete",
                "columns work_mem/1",
                "row 8MB",
                "PortalSuspended",
                "SHOW",
                "SHOW",
                "ready I"
            ]
        );
        assert_eq!(
            send(&mut c, &[close('S', "s"), close('P', "none"), sync()].concat()),
            ["CloseComplete", "CloseComplete", "ready I"]
        );
        assert_eq!(send(&mut c, &message(b'X', b"")), ["(closed)"]);
    }

    #[test]
    fn extended_errors() {
        big_stack(extended_errors_cases);
    }

    fn extended_errors_cases() {
        let (mut c, _) = connect();
        let cases: [(Vec<u8>, &str); 12] = [
            (run("SELEC 1"), "ERROR 42601 syntax error at or near \"SELEC\" at 1"),
            (
                run("SHOW work_mem; SHOW work_mem"),
                "ERROR 42601 cannot insert multiple commands into a prepared statement",
            ),
            (
                parse("", "SHOW work_mem", &[0]),
                "ERROR 42P18 could not determine data type of parameter $1",
            ),
            (run("SHOW nosuch"), "ERROR 42704 unrecognized configuration parameter \"nosuch\""),
            (run("SELECT nosuch()"), "ERROR 42883 function nosuch() does not exist at 8"),
            (bind("", "", &[], &[], &[]), "ERROR 26000 unnamed prepared statement does not exist"),
            (
                bind("", "nope", &[], &[], &[]),
                "ERROR 26000 prepared statement \"nope\" does not exist",
            ),
            (describe('S', "nope"), "ERROR 26000 prepared statement \"nope\" does not exist"),
            (execute("nope", 0), "ERROR 34000 portal \"nope\" does not exist"),
            (describe('P', "nope"), "ERROR 34000 portal \"nope\" does not exist"),
            (
                [parse("", "SHOW ALL", &[]), bind("", "", &[], &[], &[0, 0])].concat(),
                "ERROR 08P01 bind message has 2 result formats but query has 3 columns",
            ),
            (
                [parse("", "SHOW work_mem", &[]), bind("", "", &[], &[], &[2]), execute("", 0)]
                    .concat(),
                "ERROR 22023 unsupported format code: 2",
            ),
        ];
        for (input, error) in cases {
            let lines = send(&mut c, &[input, execute("", 0), sync()].concat());
            assert_eq!(lines[lines.len() - 2..], [error, "ready I"], "{lines:?}");
        }
        assert_eq!(
            send(&mut c, &[parse("s", "SET work_mem = '8MB'", &[23]), sync()].concat()).len(),
            2
        );
        let cases: [(Vec<u8>, &str); 5] = [
            (
                parse("s", "SHOW work_mem", &[]),
                "ERROR 42P05 prepared statement \"s\" already exists",
            ),
            (
                bind("", "s", &[], &[], &[]),
                "ERROR 08P01 bind message supplies 0 parameters, but prepared statement \"s\" requires 1",
            ),
            (
                bind("", "s", &[], &[Some(b"abc")], &[]),
                "ERROR 22P02 invalid input syntax for type integer: \"abc\" (unnamed portal parameter $1 = '...')",
            ),
            (
                bind("", "s", &[3], &[None], &[]),
                "ERROR 22023 unsupported format code: 3 (unnamed portal parameter $1)",
            ),
            (
                [bind("p", "s", &[], &[None], &[]), execute("p", 0), execute("p", 0)].concat(),
                "ERROR 55000 portal \"p\" cannot be run",
            ),
        ];
        for (input, error) in cases {
            let lines = send(&mut c, &[input, sync()].concat());
            assert_eq!(lines[lines.len() - 2..], [error, "ready I"], "{lines:?}");
        }
        // The error rolled back the SET of the same pipeline.
        assert_eq!(send(&mut c, &query("SHOW work_mem"))[1], "row 4MB");
        // A Query drops the unnamed statement.
        send(&mut c, &[parse("", "SHOW work_mem", &[]), sync()].concat());
        send(&mut c, &query("SHOW work_mem"));
        assert_eq!(
            send(&mut c, &[bind("", "", &[], &[], &[]), sync()].concat()),
            ["ERROR 26000 unnamed prepared statement does not exist", "ready I"]
        );
    }

    #[test]
    fn extended_transactions() {
        big_stack(extended_transactions_cases);
    }

    fn extended_transactions_cases() {
        let (mut c, _) = connect();
        // A pipeline runs in one implicit block, and COMMIT in it gives the warning of no block.
        let input = [run("SET work_mem = '2MB'"), run("COMMIT"), run("SHOW work_mem"), sync()];
        assert_eq!(
            send(&mut c, &input.concat()),
            [
                "ParseComplete",
                "BindComplete",
                "SET",
                "ParseComplete",
                "BindComplete",
                "WARNING 25P01 there is no transaction in progress",
                "COMMIT",
                "ParseComplete",
                "BindComplete",
                "row 2MB",
                "SHOW",
                "ready I"
            ]
        );
        // The portals of a block stay after Sync and end with the block.
        let input = [run("BEGIN"), parse("", "SHOW work_mem", &[]), bind("p", "", &[], &[], &[])];
        assert_eq!(send(&mut c, &[input.concat(), sync()].concat()).last().unwrap(), "ready T");
        assert_eq!(
            send(&mut c, &[execute("p", 0), sync()].concat()),
            ["row 2MB", "SHOW", "ready T"]
        );
        assert_eq!(send(&mut c, &[run("COMMIT"), sync()].concat())[2], "COMMIT");
        assert_eq!(
            send(&mut c, &[execute("p", 0), sync()].concat()),
            ["ERROR 34000 portal \"p\" does not exist", "ready I"]
        );
        // A failed block takes only the statements that end it.
        let input = [run("BEGIN"), run("SELECT 1/0"), sync()].concat();
        assert_eq!(send(&mut c, &input).last().unwrap(), "ready E");
        assert_eq!(
            send(&mut c, &[parse("", "SHOW work_mem", &[]), sync()].concat()),
            [
                "ERROR 25P02 current transaction is aborted, commands ignored until end of transaction block",
                "ready E"
            ]
        );
        assert_eq!(
            send(&mut c, &[run("ROLLBACK"), sync()].concat()),
            ["ParseComplete", "BindComplete", "ROLLBACK", "ready I"]
        );
        // DISCARD ALL commits at once and drops the named statements.
        send(&mut c, &[parse("s", "SHOW work_mem", &[]), sync()].concat());
        assert_eq!(
            send(&mut c, &[run("DISCARD ALL"), bind("", "s", &[], &[], &[]), sync()].concat()),
            [
                "ParseComplete",
                "BindComplete",
                "DISCARD ALL",
                "ERROR 26000 prepared statement \"s\" does not exist",
                "ready I"
            ]
        );
        assert_eq!(send(&mut c, &query("SHOW work_mem"))[1], "row 4MB");
        assert_eq!(
            send(&mut c, &[run("SET work_mem = '1MB'"), run("DISCARD ALL"), sync()].concat())[5],
            "ERROR 25001 DISCARD ALL cannot run inside a transaction block"
        );
    }

    #[test]
    fn startup_options() {
        assert_eq!(
            split_options(r"-c work_mem=8MB --search-path=a\ b -cstatement_timeout=5").unwrap(),
            [
                ("work_mem".to_owned(), "8MB".to_owned()),
                ("search_path".to_owned(), "a b".to_owned()),
                ("statement_timeout".to_owned(), "5".to_owned())
            ]
        );
        assert_eq!(
            split_options("-c").unwrap_err().message(),
            "option requires an argument -- 'c'"
        );
        assert_eq!(
            split_options("-x").unwrap_err().message(),
            "invalid command-line argument for server process: -x"
        );
        let base = base_settings(&[]).unwrap();
        let settings = session_settings(&base, &start(Some("-c work_mem=16MB"), &[]), &[]).unwrap();
        assert_eq!(settings.get("work_mem").as_deref(), Some("16MB"));
        let latin1 = session_settings(&base, &start(None, &[("client_encoding", "LATIN1")]), &[]);
        assert_eq!(latin1.unwrap_err().state(), SqlState::FEATURE_NOT_SUPPORTED);
        let ascii = session_settings(&base, &start(None, &[("client_encoding", "sql_ascii")]), &[])
            .unwrap();
        assert_eq!(ascii.get("client_encoding").as_deref(), Some("SQL_ASCII"));
    }

    #[test]
    fn the_semaphore_count() {
        let mut settings = base_settings(&[("max_connections".into(), "20".into())]).unwrap();
        semaphore_count(&mut settings).unwrap();
        // 20 connections, 16 autovacuum workers, 8 workers, 10 WAL senders, 2 special workers and 38 auxiliary processes.
        assert_eq!(settings.get("num_os_semaphores").as_deref(), Some("94"));
    }

    #[test]
    fn values_of_the_oracle_build() {
        let settings = base_settings(&[]).unwrap();
        assert_eq!(settings.get("data_checksums").as_deref(), Some("on"));
        assert_eq!(settings.get("default_toast_compression").as_deref(), Some("lz4"));
        assert_eq!(settings.get("wal_compression").as_deref(), Some("off"));
        let wal_compression = crate::guc::find("wal_compression").unwrap();
        assert!(wal_compression.parse("zstd").is_ok());
    }

    #[test]
    fn cancel() {
        big_stack(cancel_cases);
    }

    fn cancel_cases() {
        let (mut c, _) = connect();
        let flag = c.cancel_flag();
        // A cancel that comes while the session waits for a message does nothing.
        flag.store(true, Ordering::Relaxed);
        assert_eq!(
            send(&mut c, &query("SHOW work_mem")),
            ["columns work_mem", "row 4MB", "SHOW", "ready I"]
        );
        assert!(!flag.load(Ordering::Relaxed));
        // A cancel that comes after the session read the message stops the statement at the next check.
        let mut out = OutBuf::new();
        flag.store(true, Ordering::Relaxed);
        assert!(c.simple_query("SET work_mem = '8MB'; SHOW work_mem", &mut out));
        c.recover(&mut out);
        assert_eq!(render(&out, false), ["ERROR 57014 canceling statement due to user request"]);
        assert_eq!(
            send(&mut c, &query("SHOW work_mem")),
            ["ready I", "columns work_mem", "row 4MB", "SHOW", "ready I"]
        );
        // Execute checks after the portal is found and before it runs, and the error drops the messages up to Sync.
        let input = [parse("s", "SHOW work_mem", &[]), bind("p", "s", &[], &[], &[])].concat();
        assert_eq!(send(&mut c, &input), ["ParseComplete", "BindComplete"]);
        let mut out = OutBuf::new();
        flag.store(true, Ordering::Relaxed);
        let result = c.execute(b"p", 0, &mut out);
        c.after(result, &mut out);
        assert_eq!(render(&out, false), ["ERROR 57014 canceling statement due to user request"]);
        assert_eq!(send(&mut c, &[execute("p", 0), sync()].concat()), ["ready I"]);
        assert!(!flag.load(Ordering::Relaxed));
    }
}

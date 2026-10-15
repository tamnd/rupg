//! One client connection after the startup packet: the settings of the session, the messages of the start, and the main loop of `PostgresMain` for the simple and the extended query protocol.
//!
//! The connection does no I/O. The server gives it the bytes that the client sent and an output buffer, and [`Connection::step`] tells the server what to do next. See `spec/06-server-and-wire.md` section 6.1.

use std::cell::{Ref, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rupg_analyze::{Message, Params};
use rupg_catalog::{Catalog, database};
use rupg_common::{Error, SqlState};
use rupg_func::{BackendState, Sequences};
pub use rupg_func::{Client, Ssl};
use rupg_pgcatalog::builtin;
use rupg_platform::Clock;
use rupg_platform::os::OsClock;
use rupg_sql::Severity;
use rupg_sql::nodes::{LockStmt, Node, ObjectType};
use rupg_wire::{
    CancelKey, CommandTag, Field, Frontend, Level, OutBuf, Portals, ProtocolError, Session,
    Statements, source_fields,
};

use crate::block::{Block, Ending, Transaction};
use crate::guc::{Action, Origin, Settings};
use crate::query::{self, Column, Reader, Row};
use crate::store::{Lease, Store};
use crate::utility::{self, Context, Notice, Outcome};

mod activity;
mod copy;
mod copy_from;
mod cursor;
mod databases;
mod extended;
mod prepare;

pub(crate) use activity::Activity;
use extended::Failed;
pub(crate) use extended::{Portal, Prepared};

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

/// The result of a statement as the session sends it.
#[derive(Debug)]
enum Done {
    /// `CommandComplete` with this tag and no rows.
    Tag(CommandTag),
    /// `RowDescription`, the rows, then `CommandComplete` with the tag and the number of rows.
    Rows { columns: Vec<Column>, rows: Vec<Row>, tag: CommandTag },
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

/// The settings of a new session: the base settings, the values that the server sets for the session, then the `options` of the startup packet and its other parameters, as `process_startup_options` applies them.
///
/// # Errors
///
/// A bad `options` parameter and a setting that is not valid. The server sends them as `FATAL`.
pub fn session_settings(base: &Settings, start: &Start) -> Result<Settings, Error> {
    let mut settings = base.clone();
    settings.set_superuser(start.superuser);
    settings.set_internal("is_superuser", if start.superuser { "on" } else { "off" })?;
    settings.set_internal("session_authorization", &start.user)?;
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
    if let Some(context) = error.context() {
        fields.push((b'W', context.as_bytes()));
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
    portals: Portals<Portal>,
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
    /// The OID of the role of the session.
    role: u32,
    /// The catalog that the sessions of the server share.
    store: Arc<Store>,
    /// The lock of the store and the copy of the catalog, while the transaction changes the catalog.
    lease: Option<Lease>,
    /// The row of the session in the table of the sessions of the store.
    activity: Activity,
    /// The cache of the sequences of the session, with the state of `currval` and `lastval`.
    sequences: RefCell<Sequences>,
    /// The `COPY ... FROM STDIN` that reads its data.
    copy_in: Option<copy_from::CopyIn>,
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
            database: start.database.clone(),
            database_oid: database::POSTGRES,
            protocol,
            statements: Statements::new(),
            portals: Portals::new(),
            xact_started: false,
            pipelining: false,
            cancel: Arc::new(AtomicBool::new(false)),
            end: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(OsClock::new()),
            pid: 0,
            transaction_start: 0,
            statement_start: 0,
            role: role_oid(start),
            store: Arc::new(Store::new()),
            lease: None,
            activity: Activity::default(),
            sequences: RefCell::new(Sequences::default()),
            copy_in: None,
        };
        connection.session.set_utf8(connection.utf8());
        connection
    }

    /// Sets the clock of `now()` and the other times of the session. The clock of the operating system is the default.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
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

    /// What a statement reads of the session.
    fn reader(&self) -> Reader<'_> {
        Reader::new(
            &self.settings,
            &self.user,
            &self.database,
            (self.transaction_start, self.statement_start),
            &*self.clock,
            self.pid,
            self.catalog(),
        )
        .with_activity(&self.activity, &self.store)
        .with_sequences(&self.sequences)
        .with_statements(&self.statements)
        .with_portals(&self.portals)
        .with_cancel(&self.cancel)
    }

    /// The catalog that a statement sees: the copy of the transaction when it changed the catalog, or the catalog of the last commit.
    fn catalog(&self) -> Arc<Catalog> {
        match &self.lease {
            Some(lease) => Arc::clone(&lease.catalog),
            None => self.store.committed(self.database_oid),
        }
    }

    /// Runs a statement that defines an object. The statement takes the lock of the store, which the transaction holds until it ends. After an error the changes of the statement go, but its OIDs stay used.
    fn define(
        &mut self,
        node: &Node,
        text: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        if let Node::DropStmt(stmt) = node
            && stmt.concurrent
            && self.transaction.in_block()
        {
            return Err(Error::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "DROP INDEX CONCURRENTLY cannot run inside a transaction block",
            ));
        }
        if self.lease.is_none() {
            self.lease = Some(self.store.lease(self.database_oid, &self.cancel)?);
        }
        let mut work = Catalog::clone(&self.catalog());
        let defined = rupg_analyze::define(node, text, &self.reader(), &mut work, self.role);
        notices.extend(defined.messages.into_iter().map(|message| match message {
            Message::Warning(error) => Notice::warning(error),
            Message::Notice(error) => Notice { severity: Severity::Notice, error },
        }));
        let Some(lease) = self.lease.as_mut() else {
            return Err(Error::internal("the lock of the catalog is gone"));
        };
        match defined.result {
            Ok(()) => {
                lease.catalog = Arc::new(work);
                Ok(Done::Tag(match node {
                    Node::IndexStmt(_) => CommandTag::CreateIndex,
                    Node::CreateSchemaStmt(_) => CommandTag::CreateSchema,
                    Node::ViewStmt(_) => CommandTag::CreateView,
                    Node::CommentStmt(_) => CommandTag::Comment,
                    Node::DropStmt(stmt) => match stmt.removeType {
                        ObjectType::OBJECT_SCHEMA => CommandTag::DropSchema,
                        ObjectType::OBJECT_VIEW => CommandTag::DropView,
                        ObjectType::OBJECT_INDEX => CommandTag::DropIndex,
                        ObjectType::OBJECT_SEQUENCE => CommandTag::DropSequence,
                        _ => CommandTag::DropTable,
                    },
                    Node::AlterTableStmt(stmt) => match stmt.objtype {
                        ObjectType::OBJECT_INDEX => CommandTag::AlterIndex,
                        ObjectType::OBJECT_SEQUENCE => CommandTag::AlterSequence,
                        ObjectType::OBJECT_VIEW => CommandTag::AlterView,
                        ObjectType::OBJECT_MATVIEW => CommandTag::AlterMaterializedView,
                        ObjectType::OBJECT_FOREIGN_TABLE => CommandTag::AlterForeignTable,
                        _ => CommandTag::AlterTable,
                    },
                    Node::AlterOwnerStmt(_) => CommandTag::AlterSchema,
                    Node::CreateSeqStmt(_) => CommandTag::CreateSequence,
                    Node::AlterSeqStmt(_) => CommandTag::AlterSequence,
                    _ => CommandTag::CreateTable,
                }))
            }
            Err(error) => {
                Arc::make_mut(&mut lease.catalog).advance_oid(work.next_oid());
                Err(error)
            }
        }
    }

    /// `LOCK TABLE`: `RequireTransactionBlock`, then the checks of [`rupg_analyze::lock_table`] with the privileges of `LockTableAclCheck`. The statement takes no lock.
    fn lock(&self, stmt: &LockStmt) -> Result<Done, Error> {
        utility::require_block(&self.transaction, "LOCK TABLE")?;
        let reader = self.reader();
        let mut allowed =
            |relid, role| rupg_func::lock_table_allowed(relid, role, stmt.mode, &reader);
        rupg_analyze::lock_table(stmt, &reader, self.role, &mut allowed)?;
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
                self.sync();
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
        let Some(copy) = self.copy_in.as_mut() else { return Next::Continue };
        let result = match message {
            Frontend::CopyData(data) => match copy.data(data) {
                Ok(()) => return Next::Continue,
                Err(error) => Err(error),
            },
            Frontend::CopyFail(message) => Err(copy.fail(message)),
            _ => copy.done(),
        };
        let Some(copy) = self.copy_in.take() else { return Next::Continue };
        if let Err(error) = result {
            self.fail(&error, None, out);
            return self.recover(out);
        }
        let failed = self.resume_query(&copy.text, copy.index, out);
        self.after_query(failed, out)
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
        self.session.set_utf8(self.utf8());
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
            _ => 0,
        };
        for (i, notice) in notices.drain(..).enumerate() {
            let (rank, severity) = match notice.severity {
                Severity::Notice => (0, "NOTICE"),
                Severity::Warning => (1, "WARNING"),
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

    /// The rest of a `Query` message after the `COPY ... FROM STDIN` at `index` ended. The text parsed before, so the parse gives no error and its notices went to the client.
    fn resume_query(&mut self, text: &str, index: usize, out: &mut OutBuf) -> bool {
        let Ok((list, _)) = rupg_sql::parse(text) else { return false };
        let statements = statements(&list, text);
        self.run_statements(text, &statements, index, Some(Done::Count(CommandTag::Copy, 0)), out)
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
            // The columns of a query that failed at run time. PostgreSQL sends `RowDescription` when the executor starts, so the error comes after it.
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
            } else if prepare::is_prepare(node) {
                drop(self.portals.close(b""));
                self.run_prepare(node, text, &[], &mut notices, 0)
            } else if query::is_query(node) {
                drop(self.portals.close(b""));
                self.select(node, &mut notices, &mut late, &mut described)
            } else if rupg_analyze::is_definition(node) {
                drop(self.portals.close(b""));
                self.define(node, statement, &mut notices)
            } else if let Node::LockStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.lock(stmt)
            } else if let Node::CopyStmt(stmt) = node
                && stmt.is_from
            {
                drop(self.portals.close(b""));
                match self.copy_from(stmt, out) {
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
                drop(self.portals.close(b""));
                self.copy(stmt, &mut copying)
            } else if let Node::CreatedbStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.create_database(stmt, &mut notices)
            } else if let Node::DropdbStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.drop_database(stmt, &mut notices)
            } else if let Node::DeclareCursorStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.declare(stmt, text, None, &[], &mut notices)
            } else if let Node::FetchStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.fetch_cursor(stmt, None)
            } else if let Node::ClosePortalStmt(stmt) = node {
                drop(self.portals.close(b""));
                self.close_cursor(stmt, b"")
            } else {
                drop(self.portals.close(b""));
                let mut cx = Context {
                    settings: self.settings.get_mut(),
                    transaction: &mut self.transaction,
                    user: &self.user,
                    notices: &mut notices,
                    immediate_commit: false,
                };
                utility::run(node, statement, &mut cx).map(Done::from)
            };
            self.notices(&mut notices, text, &[], out);
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    if let Some(columns) = described {
                        let fields: Vec<Field<'_>> =
                            columns.iter().map(|column| column.field(0)).collect();
                        out.row_description(&fields);
                    }
                    self.notices(&mut late, text, &[], out);
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
                self.portals.clear();
            }
            if matches!(outcome, Done::Tag(CommandTag::DiscardAll | CommandTag::DiscardSequences)) {
                self.sequences.get_mut().discard();
            }
            if i + 1 == statements.len() {
                if many {
                    self.transaction.end_implicit();
                }
                self.finish_xact();
            } else if matches!(node, Node::TransactionStmt(_)) {
                self.finish_xact();
            }
            match outcome {
                Done::Tag(tag) => out.command_tag(tag, 0),
                Done::Rows { columns, rows, tag } => {
                    let fields: Vec<Field<'_>> =
                        columns.iter().map(|column| column.field(0)).collect();
                    out.row_description(&fields);
                    self.notices(&mut late, text, &[], out);
                    for row in &rows {
                        let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
                        out.data_row(&values);
                    }
                    out.command_tag(tag, rows.len() as u64);
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
                Done::Count(tag, count) => out.command_tag(tag, count),
                Done::Copy(copy) => copy.send(out),
            }
        }
        false
    }

    /// Plans and runs a query of a `Query` message. The warnings of the analysis and of the plan go to `notices`, and the warnings of the run go to `late`. When the run fails after the plan, the columns of the plan go to `described`.
    fn select(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
        late: &mut Vec<Notice>,
        described: &mut Option<Vec<Column>>,
    ) -> Result<Done, Error> {
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let plan = query::plan(node, &reader, &Params::default());
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        let plan = plan.fold(None, &reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        let columns = query::columns(&plan);
        let rows = query::run(&plan, &[], &reader, &[]);
        late.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        match rows {
            Ok(rows) => Ok(Done::Rows { columns, rows, tag: CommandTag::Select }),
            Err(error) => {
                *described = Some(columns);
                Err(error)
            }
        }
    }

    /// `start_xact_command`: the start of a statement, which starts a transaction if there is none. After an `Execute` that completed, it starts an implicit block.
    fn start_xact(&mut self) {
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
            if let Some(lease) = self.lease.take()
                && ending == Ending::Commit
            {
                lease.commit();
            }
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
        self.portals.retain(|_, portal| match &mut portal.run {
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
        if self.transaction.abort_current() == Ending::Rollback {
            self.settings.get_mut().end(false);
            self.lease = None;
        }
        self.transaction_ended();
        if self.transaction.failed() {
            self.portals.retain(|_, portal| {
                portal.failed |= !portal.as_cursor().is_some_and(cursor::Cursor::held);
                true
            });
        } else {
            self.ended(false);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.leave();
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

/// The position of a byte offset in characters, from 1, as the field `P` gives it.
fn character_position(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    text.as_bytes()[..at].iter().filter(|&&b| b & 0xc0 != 0x80).count() + 1
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
        let settings = session_settings(&base_settings(&[]).unwrap(), &start).unwrap();
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
        render_details(out, full, false)
    }

    /// [`render`], and with `details` a line `DETAIL` and a line `HINT` after an error or a notice that has them.
    fn render_details(out: &OutBuf, full: bool, details: bool) -> Vec<String> {
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
                        if details && !value.is_empty() {
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

    /// Runs a script of statements on a new connection. A line that starts with `> ` is a simple query, and the lines after it are the messages that the query must give, in the full form of `render`, without the lines of `ReadyForQuery`. The table of a column shows as its `regclass` text, because the OIDs of the oracle are not the OIDs of a new database. After a line `!details`, an error or a notice also has its detail and its hint. The lines `< text`, `<hex HEX`, `<done` and `<fail message` right after a simple query are the messages of `COPY ... FROM STDIN` that go with the query: `CopyData` of the text and a newline, `CopyData` of the bytes, `CopyDone` and `CopyFail`.
    fn script(text: &str) {
        let (mut c, _) = connect();
        let starts = |l: &str| {
            l == "!details" || ["> ", "P ", "X ", "D ", "C "].iter().any(|p| l.starts_with(p))
        };
        let mut details = false;
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            assert!(starts(line), "not a statement: {line}");
            if line == "!details" {
                details = true;
                continue;
            }
            let mut copy = Vec::new();
            while let Some(next) = lines.next_if(|l| l.starts_with('<')) {
                copy.push(next);
            }
            let mut want = Vec::new();
            while let Some(next) = lines.next_if(|l| !starts(l)) {
                want.push(next);
            }
            assert_eq!(script_copy(&mut c, line, &copy, details), want, "{line}");
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
        script_details(c, line, false)
    }

    /// [`script_lines`], with the details of [`render_details`] when `details` is true.
    fn script_details(c: &mut Connection, line: &str, details: bool) -> Vec<String> {
        script_copy(c, line, &[], details)
    }

    /// [`script_details`], with the messages of `COPY ... FROM STDIN` of the lines `copy` after a simple query.
    fn script_copy(c: &mut Connection, line: &str, copy: &[&str], details: bool) -> Vec<String> {
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

    /// The system columns `xmin`, `cmin`, `xmax` and `cmax` of a table and of a catalog, their types and their errors in a view, a subquery and a check constraint, the partition key functions for a table that is not partitioned, and the recovery functions on a server that is not in recovery. The script does not show the value of `xmin`, because rupg has no row versions yet. `connection/system_columns.test` is the output of PostgreSQL 19 for the same script in a new database.
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

    /// A session of a database other than `postgres`.
    fn connect_database(store: &Arc<Store>, pid: i32, database: &str) -> Connection {
        let start = Start { database: database.into(), ..start(None, &[]) };
        let settings = session_settings(&base_settings(&[]).unwrap(), &start).unwrap();
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
            lines.iter().any(|l| l == "ERROR 55P03 canceling statement due to lock timeout"),
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

    /// `COPY FROM` a file or a program, the JSON format, an encoding other than `UTF8` and `COPY TO` a file or a program give `0A000`.
    #[test]
    fn copy_not_supported() {
        big_stack(|| {
            let (mut c, _) = connect();
            assert_eq!(script_lines(&mut c, "> CREATE TABLE t (a int)"), ["CREATE TABLE"]);
            for (sql, error) in [
                ("COPY t FROM '/tmp/rupg-copy.txt'", "COPY FROM a file is not supported yet"),
                ("COPY t FROM PROGRAM 'cat'", "COPY FROM a program is not supported yet"),
                ("COPY t TO stdout (FORMAT json)", "COPY FORMAT JSON is not supported yet"),
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
            assert_eq!(
                script_lines(&mut c, "> COMMENT ON FUNCTION lower(text) IS 'x'"),
                ["ERROR 0A000 COMMENT on this kind of object is not supported yet"]
            );
            assert_eq!(
                script_lines(
                    &mut c,
                    "> SELECT obj_description('public'::regnamespace, 'pg_namespace')"
                ),
                ["fields obj_description:25:-1:-1", "row standard public schema", "SELECT 1"]
            );
        });
    }

    /// A lock mode stronger than `ROW EXCLUSIVE` gives `0A000`, because rupg takes no lock on a relation. The check comes after the check of the transaction block and before the names.
    #[test]
    fn lock_table_strong_modes() {
        big_stack(|| {
            let (mut c, _) = connect();
            assert_eq!(
                script_lines(&mut c, "> LOCK pg_class"),
                ["ERROR 25P01 LOCK TABLE can only be used in transaction blocks"]
            );
            assert_eq!(script_lines(&mut c, "> BEGIN"), ["BEGIN"]);
            for mode in ["SHARE UPDATE EXCLUSIVE", "SHARE", "SHARE ROW EXCLUSIVE", "EXCLUSIVE"] {
                let sql = format!("> LOCK pg_class IN {mode} MODE");
                let want = format!("ERROR 0A000 LOCK TABLE in {mode} mode is not supported yet");
                assert_eq!(script_lines(&mut c, &sql), [want]);
                assert_eq!(script_lines(&mut c, "> ROLLBACK"), ["ROLLBACK"]);
                assert_eq!(script_lines(&mut c, "> BEGIN"), ["BEGIN"]);
            }
            let want = "ERROR 0A000 LOCK TABLE in ACCESS EXCLUSIVE mode is not supported yet";
            assert_eq!(script_lines(&mut c, "> LOCK missing"), [want]);
        });
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
        let settings = session_settings(&base, &start(Some("-c work_mem=16MB"), &[])).unwrap();
        assert_eq!(settings.get("work_mem").as_deref(), Some("16MB"));
        let latin1 = session_settings(&base, &start(None, &[("client_encoding", "LATIN1")]));
        assert_eq!(latin1.unwrap_err().state(), SqlState::FEATURE_NOT_SUPPORTED);
        let ascii =
            session_settings(&base, &start(None, &[("client_encoding", "sql_ascii")])).unwrap();
        assert_eq!(ascii.get("client_encoding").as_deref(), Some("SQL_ASCII"));
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

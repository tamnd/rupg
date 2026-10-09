//! One client connection after the startup packet: the settings of the session, the messages of the start, and the main loop of `PostgresMain` for the simple and the extended query protocol.
//!
//! The connection does no I/O. The server gives it the bytes that the client sent and an output buffer, and [`Connection::step`] tells the server what to do next. See `spec/06-server-and-wire.md` section 6.1.

use std::cell::{Ref, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rupg_analyze::{Message, Params};
use rupg_catalog::Catalog;
use rupg_common::{Error, SqlState};
use rupg_pgcatalog::builtin;
use rupg_platform::Clock;
use rupg_platform::os::OsClock;
use rupg_sql::Severity;
use rupg_sql::nodes::Node;
use rupg_wire::{
    CancelKey, CommandTag, Field, Frontend, Level, OutBuf, Portals, ProtocolError, Session,
    Statements,
};

use crate::block::{Block, Ending, Transaction};
use crate::guc::{Action, Origin, Settings};
use crate::query::{self, Column, Reader, Row};
use crate::store::{Lease, Store};
use crate::utility::{self, Context, Notice, Outcome};

mod extended;

use extended::{Failed, Portal, Prepared};

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

/// Writes an `ErrorResponse` or a `NoticeResponse` with the severity, the SQLSTATE, the message, the detail, the hint and the position in the query.
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
    protocol: u32,
    statements: Statements<Arc<Prepared>>,
    portals: Portals<Portal>,
    /// `xact_started` of `postgres.c`: a statement runs in the transaction, so `finish_xact_command` must end it.
    xact_started: bool,
    /// `XACT_FLAGS_PIPELINING`: an `Execute` completed in the transaction, so the next message starts an implicit block.
    pipelining: bool,
    /// `QueryCancelPending`. The server sets it when a `CancelRequest` with the key of this session arrives.
    cancel: Arc<AtomicBool>,
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
            protocol,
            statements: Statements::new(),
            portals: Portals::new(),
            xact_started: false,
            pipelining: false,
            cancel: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(OsClock::new()),
            pid: 0,
            transaction_start: 0,
            statement_start: 0,
            role: role_oid(start),
            store: Arc::new(Store::new()),
            lease: None,
        };
        connection.session.set_utf8(connection.utf8());
        connection
    }

    /// Sets the clock of `now()` and the other times of the session. The clock of the operating system is the default.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Sets the catalog that the sessions of the server share. A new session has a catalog of its own.
    pub fn set_store(&mut self, store: Arc<Store>) {
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
    }

    /// The catalog that a statement sees: the copy of the transaction when it changed the catalog, or the catalog of the last commit.
    fn catalog(&self) -> Arc<Catalog> {
        match &self.lease {
            Some(lease) => Arc::clone(&lease.catalog),
            None => self.store.committed(),
        }
    }

    /// Runs a statement that defines an object. The statement takes the lock of the store, which the transaction holds until it ends. After an error the changes of the statement go, but its OIDs stay used.
    fn define(
        &mut self,
        node: &Node,
        text: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        if self.lease.is_none() {
            self.lease = Some(self.store.lease(&self.cancel)?);
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
                    _ => CommandTag::CreateTable,
                }))
            }
            Err(error) => {
                Arc::make_mut(&mut lease.catalog).advance_oid(work.next_oid());
                Err(error)
            }
        }
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
        key.write(out);
    }

    /// True when the client encoding is `UTF8`, so the strings of the messages must be valid UTF-8.
    fn utf8(&self) -> bool {
        self.settings.borrow().get("client_encoding").is_none_or(|name| name != "SQL_ASCII")
    }

    /// Reads at most one message from `input` and writes the answer to `out`.
    pub fn step(&mut self, input: &[u8], out: &mut OutBuf) -> Step {
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
                if error.level == Level::Error { self.recover(out) } else { Next::Close }
            }
            Some(Ok(Frontend::Query(sql))) => {
                let text = String::from_utf8_lossy(sql);
                if self.simple_query(&text, out) { self.recover(out) } else { Next::Continue }
            }
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
                let error = ProtocolError {
                    level: Level::Error,
                    sqlstate: "0A000",
                    message: "the function call protocol is not supported yet".to_owned(),
                    detail: None,
                    hint: None,
                };
                out.protocol_error(&error, self.protocol);
                self.recover(out)
            }
        };
        Step { used, next }
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
        let statements: Vec<(&Node, &str)> = list
            .iter()
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
            .collect();
        if statements.is_empty() {
            out.empty_query_response();
            self.finish_xact();
            return false;
        }
        let many = statements.len() > 1;
        for (i, (node, statement)) in statements.iter().enumerate() {
            self.start_xact();
            if many {
                self.transaction.begin_implicit();
            }
            // The columns of a query that failed at run time. PostgreSQL sends `RowDescription` when the executor starts, so the error comes after it.
            let mut described = None;
            let result = if let Err(error) = self.interrupted() {
                Err(error)
            } else if self.transaction.failed() && !utility::exits_transaction(node) {
                Err(Error::new(
                    SqlState::IN_FAILED_SQL_TRANSACTION,
                    "current transaction is aborted, commands ignored until end of transaction block",
                ))
            } else if query::is_query(node) {
                drop(self.portals.close(b""));
                self.select(node, &mut notices, &mut described)
            } else if rupg_analyze::is_definition(node) {
                drop(self.portals.close(b""));
                self.define(node, statement, &mut notices)
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
                    let position = error.position().map(|at| character_position(text, at));
                    self.fail(&error, position, out);
                    return true;
                }
            };
            if matches!(outcome, Done::Tag(CommandTag::DiscardAll)) {
                self.statements.deallocate_all();
                self.portals.clear();
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
                    for row in &rows {
                        let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
                        out.data_row(&values);
                    }
                    out.command_tag(tag, rows.len() as u64);
                }
            }
        }
        false
    }

    /// Plans and runs a query of a `Query` message. The warnings of the analysis go to `notices`. When the run fails after the plan, the columns of the plan go to `described`.
    fn select(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
        described: &mut Option<Vec<Column>>,
    ) -> Result<Done, Error> {
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let plan = query::plan(node, &reader, &Params::default())?;
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        let plan = plan.fold(None, &reader)?;
        let columns = query::columns(&plan);
        match query::run(&plan, &[], &reader, &[]) {
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
        }
        if kept.is_some() {
            self.settings.get_mut().start_transaction(kept);
            self.transaction_start = self.statement_start;
        }
        if chained || self.transaction.block() == Block::Default {
            self.ended();
        }
    }

    /// The end of a transaction, which removes the portals.
    fn ended(&mut self) {
        self.portals.clear();
        self.pipelining = false;
    }

    /// Writes the error of a statement and aborts its transaction. In a failed block the portals stay, but they cannot run.
    fn fail(&mut self, error: &Error, position: Option<usize>, out: &mut OutBuf) {
        write_error(out, "ERROR", error, position, false);
        self.xact_started = false;
        if self.transaction.abort_current() == Ending::Rollback {
            self.settings.get_mut().end(false);
            self.lease = None;
        }
        if self.transaction.failed() {
            self.portals.retain(|_, portal| {
                portal.failed = true;
                true
            });
        } else {
            self.ended();
        }
    }
}

/// The OID of the role of a session. The superuser of the server is the bootstrap superuser.
fn role_oid(start: &Start) -> u32 {
    if start.superuser {
        return BOOTSTRAP_SUPERUSER;
    }
    builtin::roles().iter().find(|role| role.name == start.user).map_or(0, |role| role.oid)
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
        let start = start(None, &[("application_name", "test")]);
        let settings = session_settings(&base_settings(&[]).unwrap(), &start).unwrap();
        let mut connection = Connection::new(&start, settings, PROTOCOL_3_0);
        let mut out = OutBuf::new();
        connection.greet(&CancelKey::new(1234, PROTOCOL_3_0, [7; CANCEL_KEY_LEN]), &mut out);
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
            send(&mut c, &query("SET TimeZone = 'Europe/Paris'; SELECT now()::text")),
            [
                "SET",
                "columns now",
                "ERROR 0A000 the time zone \"Europe/Paris\" is not supported yet",
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
        let catalog = c.store.committed();
        let names: Vec<&str> = catalog.relations().map(|r| r.name.as_str()).collect();
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

    /// Runs a script of statements on a new connection. A line that starts with `> ` is a simple query, and the lines after it are the messages that the query must give, in the full form of `render`, without the lines of `ReadyForQuery`. The table of a column shows as its `regclass` text, because the OIDs of the oracle are not the OIDs of a new database.
    fn script(text: &str) {
        let (mut c, _) = connect();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let Some(sql) = line.strip_prefix("> ") else { panic!("not a statement: {line}") };
            let mut want = Vec::new();
            while let Some(next) = lines.next_if(|l| !l.starts_with("> ")) {
                want.push(next);
            }
            let got: Vec<String> = render(&exchange(&mut c, &query(sql)), true)
                .into_iter()
                .filter(|l| !l.starts_with("ready "))
                .map(|l| match l.strip_prefix("fields ") {
                    Some(fields) => format!("fields {}", table_names(&mut c, fields)),
                    None => l,
                })
                .flat_map(|l| l.split('\n').map(str::to_string).collect::<Vec<_>>())
                .collect();
            assert_eq!(got, want, "{sql}");
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

    /// The deparse functions `pg_get_expr`, `pg_get_constraintdef` and `pg_get_indexdef`. `connection/deparse.test` is the output of PostgreSQL 19 for the same script in a new database. A value with a newline takes more than one line.
    #[test]
    fn deparse() {
        big_stack(|| script(include_str!("connection/deparse.test")));
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

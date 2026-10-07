//! One client connection after the startup packet: the settings of the session, the messages of the start, and the main loop of `PostgresMain` for the simple query protocol.
//!
//! The connection does no I/O. The server gives it the bytes that the client sent and an output buffer, and [`Connection::step`] tells the server what to do next. See `spec/06-server-and-wire.md` section 6.1.

use rupg_common::{Error, SqlState};
use rupg_sql::Severity;
use rupg_sql::nodes::Node;
use rupg_wire::{CancelKey, CommandTag, Field, Frontend, Level, OutBuf, ProtocolError, Session};

use crate::block::{Ending, Transaction};
use crate::guc::{Action, Origin, Settings};
use crate::utility::{self, Context, Notice, Outcome};

/// The version of rupg, which `server_version` and the parameter `rupg.version` report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The OID of the type `text`.
const TEXT_OID: u32 = 25;

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
    settings: Settings,
    transaction: Transaction,
    session: Session,
    user: String,
    protocol: u32,
}

impl Connection {
    /// A session with the settings that [`session_settings`] gave. `protocol` is the version of the protocol that the client uses.
    pub fn new(start: &Start, settings: Settings, protocol: u32) -> Connection {
        let mut connection = Connection {
            settings,
            transaction: Transaction::default(),
            session: Session::new(),
            user: start.user.clone(),
            protocol,
        };
        connection.session.set_utf8(connection.utf8());
        connection
    }

    /// The settings of the session.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The messages after `AuthenticationOk`: a `ParameterStatus` for each parameter that the client must know, then `BackendKeyData`. The first `ReadyForQuery` comes from the first [`Connection::step`].
    pub fn greet(&mut self, key: &CancelKey, out: &mut OutBuf) {
        for (name, value) in self.settings.startup_reports() {
            out.parameter_status(name.as_bytes(), value.as_bytes());
        }
        out.parameter_status(b"rupg.version", VERSION.as_bytes());
        key.write(out);
    }

    /// True when the client encoding is `UTF8`, so the strings of the messages must be valid UTF-8.
    fn utf8(&self) -> bool {
        self.settings.get("client_encoding").is_none_or(|name| name != "SQL_ASCII")
    }

    /// Reads at most one message from `input` and writes the answer to `out`.
    pub fn step(&mut self, input: &[u8], out: &mut OutBuf) -> Step {
        if self.session.wants_ready() {
            self.ready_for_query(out);
            return Step { used: 0, next: Next::Flush };
        }
        let read = self.session.read(input);
        let used = read.used;
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
            Some(Ok(Frontend::Sync)) => Next::Continue,
            Some(Ok(Frontend::Flush)) => Next::Flush,
            Some(Ok(Frontend::Terminate)) => Next::Close,
            Some(Ok(other)) => {
                let message = match other {
                    Frontend::FunctionCall(_) => "the function call protocol is not supported yet",
                    _ => "the extended query protocol is not supported yet",
                };
                let error = ProtocolError {
                    level: Level::Error,
                    sqlstate: "0A000",
                    message: message.to_owned(),
                    detail: None,
                    hint: None,
                };
                out.protocol_error(&error, self.protocol);
                self.recover(out)
            }
        };
        Step { used, next }
    }

    /// The parameters that changed, then `ReadyForQuery`.
    fn ready_for_query(&mut self, out: &mut OutBuf) {
        self.session.set_utf8(self.utf8());
        let mut reports = self.settings.reports();
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
        let least = match self.settings.get("client_min_messages").as_deref() {
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
                let position =
                    locations.get(i).copied().flatten().map(|at| character_position(text, at));
                write_error(out, severity, &notice.error, position, true);
            }
        }
    }

    /// `exec_simple_query`. It gives true when a statement failed.
    fn simple_query(&mut self, text: &str, out: &mut OutBuf) -> bool {
        if self.transaction.start_command() {
            self.settings.start_transaction(None);
        }
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
            self.finish();
            return false;
        }
        let many = statements.len() > 1;
        for (i, (node, statement)) in statements.iter().enumerate() {
            if self.transaction.start_command() {
                self.settings.start_transaction(None);
            }
            if many {
                self.transaction.begin_implicit();
            }
            let result = if self.transaction.failed() && !utility::exits_transaction(node) {
                Err(Error::new(
                    SqlState::IN_FAILED_SQL_TRANSACTION,
                    "current transaction is aborted, commands ignored until end of transaction block",
                ))
            } else {
                let mut cx = Context {
                    settings: &mut self.settings,
                    transaction: &mut self.transaction,
                    user: &self.user,
                    notices: &mut notices,
                };
                utility::run(node, statement, &mut cx)
            };
            self.notices(&mut notices, text, &[], out);
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.fail(&error, None, out);
                    return true;
                }
            };
            if i + 1 == statements.len() {
                self.transaction.end_implicit();
                self.finish();
            } else if matches!(node, Node::TransactionStmt(_)) {
                self.finish();
            }
            match outcome {
                Outcome::Tag(tag) => out.command_tag(tag, 0),
                Outcome::Rows { columns, rows } => {
                    let fields: Vec<Field<'_>> = columns
                        .iter()
                        .map(|name| Field {
                            name: name.as_bytes(),
                            table: 0,
                            column: 0,
                            type_oid: TEXT_OID,
                            type_size: -1,
                            type_modifier: -1,
                            format: 0,
                        })
                        .collect();
                    out.row_description(&fields);
                    for row in &rows {
                        let values: Vec<Option<&[u8]>> =
                            row.iter().map(|value| Some(value.as_bytes())).collect();
                        out.data_row(&values);
                    }
                    out.command_tag(CommandTag::Show, 0);
                }
            }
        }
        false
    }

    /// `finish_xact_command`: the end of a statement, and the end or the chained start of its transaction.
    fn finish(&mut self) {
        let (ending, chained) = self.transaction.finish_command();
        let kept = chained.then(|| self.settings.characteristics());
        if ending != Ending::None {
            self.settings.end(ending == Ending::Commit);
        }
        if kept.is_some() {
            self.settings.start_transaction(kept);
        }
    }

    /// Writes the error of a statement and aborts its transaction.
    fn fail(&mut self, error: &Error, position: Option<usize>, out: &mut OutBuf) {
        write_error(out, "ERROR", error, position, false);
        if self.transaction.abort_current() == Ending::Rollback {
            self.settings.end(false);
        }
    }
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
        let mut lines = render(&out);
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

    /// Gives `input` to the connection until it needs more, and renders what it wrote.
    fn send(connection: &mut Connection, input: &[u8]) -> Vec<String> {
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
        render(&out)
    }

    /// One short line for each message.
    fn render(out: &OutBuf) -> Vec<String> {
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
                Backend::RowDescription(fields) => {
                    let names: Vec<String> = fields.iter().map(|f| text(f.name)).collect();
                    format!("columns {}", names.join(","))
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
    fn a_failed_block() {
        big_stack(a_failed_block_cases);
    }

    fn a_failed_block_cases() {
        let (mut c, _) = connect();
        assert_eq!(
            send(&mut c, &query("BEGIN; SET work_mem = '1MB'; SELECT 1")),
            ["BEGIN", "SET", "ERROR 0A000 SELECT is not supported yet", "ready E"]
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
            send(&mut c, &query("SET work_mem = '2MB'; SELECT 1")),
            ["SET", "ERROR 0A000 SELECT is not supported yet", "ready I"]
        );
        assert_eq!(send(&mut c, &query("SHOW work_mem"))[1], "row 4MB");
    }

    #[test]
    fn notices() {
        big_stack(notices_cases);
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
    fn messages_that_are_not_supported() {
        big_stack(messages_that_are_not_supported_cases);
    }

    fn messages_that_are_not_supported_cases() {
        let (mut c, _) = connect();
        let mut input = message(b'P', b"\0SELECT 1\0\0\0");
        input.extend(message(b'B', b"\0\0\0\0\0\0\0\0"));
        input.extend(message(b'S', b""));
        assert_eq!(
            send(&mut c, &input),
            ["ERROR 0A000 the extended query protocol is not supported yet", "ready I"]
        );
        assert_eq!(send(&mut c, &message(b'X', b"")), ["(closed)"]);
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
}

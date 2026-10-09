//! The table of the sessions, from `backend_status.c` and `pgstatfuncs.c`: the rows of `pg_stat_get_activity`, which the views `pg_stat_activity`, `pg_stat_ssl` and `pg_stat_gssapi` read, the functions `pg_stat_get_backend_*` of one session, `pg_stat_get_db_numbackends`, and the addresses of the connection of `inet_client_addr` and its sibling functions.
//!
//! The session gives the table as [`Session::backends`]. The first call in a transaction takes a snapshot of the table, and the later calls of the transaction read the same snapshot, as `pgstat_read_current_status` does. `pg_stat_clear_snapshot` drops it.
//!
//! A row of another role shows less: the columns that `HAS_PGSTAT_PERMISSIONS` guards are null, and the text of the statement is `<insufficient privilege>`. rupg has no transaction IDs, no parallel workers, no GSSAPI and no query IDs yet, so `backend_xid`, `backend_xmin`, `leader_pid` and `query_id` are null, and the GSSAPI columns show a session with no GSSAPI.

use std::net::SocketAddr;

use rupg_common::Result;
use rupg_types::{Value, inet_in, numeric_in};

use crate::{Call, Kernel, Session, acl, bad_value, type_error};

/// `GetBackendTypeDesc(B_BACKEND)`. Each session of rupg is a client backend.
const CLIENT_BACKEND: &str = "client backend";

/// The text of the functions of one session for a number that has no session.
const NOT_AVAILABLE: &str = "<backend information not available>";

/// The text of the statement of a session that the current user cannot see.
const INSUFFICIENT: &str = "<insufficient privilege>";

/// The state of a session, `BackendState` of `backend_status.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendState {
    /// The session started, and did not send `ReadyForQuery` yet.
    #[default]
    Starting,
    /// The session waits for a statement.
    Idle,
    /// The session runs a statement.
    Running,
    /// The session waits for a statement in a transaction block.
    IdleInTransaction,
    /// The session waits for the end of a failed transaction block.
    IdleInTransactionAborted,
    /// `track_activities` is off, so the session does not report its state.
    Disabled,
}

impl BackendState {
    /// The text of the column `state`.
    fn text(self) -> &'static str {
        match self {
            BackendState::Starting => "starting",
            BackendState::Idle => "idle",
            BackendState::Running => "active",
            BackendState::IdleInTransaction => "idle in transaction",
            BackendState::IdleInTransactionAborted => "idle in transaction (aborted)",
            BackendState::Disabled => "disabled",
        }
    }
}

/// An end of the connection of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Client {
    /// The address is not known: the columns of the address are null.
    #[default]
    Unknown,
    /// A Unix socket. It has no address, and `pg_stat_activity` shows the port -1.
    Local,
    /// A TCP connection.
    Inet(SocketAddr),
}

impl Client {
    /// The address as `inet`, for a TCP connection only. PostgreSQL makes the value from the text of `getnameinfo`, which is the text of the address with no zone.
    fn addr(self) -> Result<Value> {
        let Client::Inet(addr) = self else { return Ok(Value::Null) };
        inet_in(&addr.ip().to_string(), false).map(Value::Inet).map_err(type_error)
    }

    /// The port, for a TCP connection only.
    fn port(self) -> Value {
        match self {
            Client::Inet(addr) => Value::Int4(i32::from(addr.port())),
            _ => Value::Null,
        }
    }

    /// The port as `pg_stat_activity` shows it: -1 for a Unix socket.
    fn shown_port(self) -> Value {
        match self {
            Client::Local => Value::Int4(-1),
            _ => self.port(),
        }
    }
}

/// The TLS of a session, `PgBackendSSLStatus`. An empty text of the client certificate is null, as for a session with no client certificate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ssl {
    /// The protocol, as `SSL_get_version` gives it, for example `TLSv1.3`.
    pub version: String,
    /// The cipher suite, by its name in OpenSSL.
    pub cipher: String,
    /// The bits of the key of the cipher.
    pub bits: i32,
    /// The subject of the client certificate, in the form of `X509_NAME_to_cstring`.
    pub client_dn: String,
    /// The serial number of the client certificate, in decimal.
    pub client_serial: String,
    /// The issuer of the client certificate, in the form of `X509_NAME_to_cstring`.
    pub issuer_dn: String,
}

/// A row of the table of the sessions, `PgBackendStatus`.
#[derive(Clone, Debug, Default)]
pub struct Backend {
    /// The number of the session in the table, `ProcNumber`, which `pg_stat_get_backend_idset` gives.
    pub number: i32,
    pub pid: i32,
    /// The OID of the database.
    pub database: u32,
    /// The OID of the role of the session.
    pub role: u32,
    pub application: String,
    pub state: BackendState,
    /// The text of the last statement, as at most `track_activity_query_size - 1` bytes.
    pub query: String,
    /// The times in microseconds since 2000-01-01 UTC. `None` is null.
    pub backend_start: i64,
    pub transaction_start: Option<i64>,
    pub query_start: Option<i64>,
    pub state_change: Option<i64>,
    /// True when the session waits for a message of the client, the wait event `ClientRead`.
    pub reading: bool,
    pub client: Client,
    /// The TLS of the session, or `None` for a session with no TLS.
    pub ssl: Option<Ssl>,
}

/// `HAS_PGSTAT_PERMISSIONS`: the current user has the privileges of `pg_read_all_stats` or of the role of the session.
fn permitted(session: &dyn Session, role: u32) -> bool {
    acl::has_pgstat_permissions(session, role)
}

/// A time of a row, or null.
fn time(at: Option<i64>) -> Value {
    at.map_or(Value::Null, Value::TimestampTz)
}

/// A text value, or null.
fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::text)
}

/// A text of the TLS status that is null when it is empty.
fn filled(value: &str) -> Value {
    if value.is_empty() { Value::Null } else { Value::text(value) }
}

/// An OID that is null when it is 0.
fn valid_oid(oid: u32) -> Value {
    if oid == 0 { Value::Null } else { Value::Oid(oid) }
}

/// The wait event of a row as its type and its name.
fn wait_event(backend: &Backend) -> (Option<&'static str>, Option<&'static str>) {
    if backend.reading { (Some("Client"), Some("ClientRead")) } else { (None, None) }
}

/// The 31 columns of `pg_stat_get_activity` for a row.
fn activity_row(session: &dyn Session, backend: &Backend) -> Result<Vec<Value>> {
    let mut row = vec![Value::Null; 31];
    row[0] = valid_oid(backend.database);
    row[1] = Value::Int4(backend.pid);
    row[2] = valid_oid(backend.role);
    row[3] = Value::text(&backend.application);
    if !permitted(session, backend.role) {
        row[5] = Value::text(INSUFFICIENT);
        return Ok(row);
    }
    row[4] = Value::text(backend.state.text());
    row[5] = Value::text(&backend.query);
    let (kind, event) = wait_event(backend);
    row[6] = text(kind);
    row[7] = text(event);
    row[8] = time(backend.transaction_start);
    row[9] = time(backend.query_start);
    row[10] = Value::TimestampTz(backend.backend_start);
    row[11] = time(backend.state_change);
    row[12] = backend.client.addr()?;
    row[14] = backend.client.shown_port();
    row[17] = Value::text(CLIENT_BACKEND);
    row[18] = Value::Bool(backend.ssl.is_some());
    if let Some(ssl) = &backend.ssl {
        row[19] = Value::text(&ssl.version);
        row[20] = Value::text(&ssl.cipher);
        row[21] = Value::Int4(ssl.bits);
        row[22] = filled(&ssl.client_dn);
        if !ssl.client_serial.is_empty() {
            row[23] = Value::Numeric(numeric_in(&ssl.client_serial, -1).map_err(type_error)?);
        }
        row[24] = filled(&ssl.issuer_dn);
    }
    row[25] = Value::Bool(false);
    row[27] = Value::Bool(false);
    row[28] = Value::Bool(false);
    Ok(row)
}

/// `pg_stat_get_activity`: a row for each session, or for the session with the process ID of the argument. A null argument gives all the sessions.
pub(crate) fn activity(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let pid = match args.first() {
        Some(Value::Int4(pid)) => Some(*pid),
        Some(Value::Null) | None => None,
        Some(_) => return Err(bad_value()),
    };
    let backends = call.session.backends();
    let mut rows = Vec::new();
    for backend in backends.iter().filter(|b| pid.is_none_or(|pid| b.pid == pid)) {
        rows.push(activity_row(call.session, backend)?);
        if pid.is_some() {
            break;
        }
    }
    Ok(rows)
}

/// `pg_stat_get_backend_idset`: the number of each session.
pub(crate) fn backend_idset(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call.session.backends().iter().map(|b| vec![Value::Int4(b.number)]).collect())
}

/// `pgstat_get_beentry_by_proc_number`: the row with the number of the argument, or `None`.
fn backend<T>(
    call: &Call<'_>,
    args: &[Value],
    read: impl FnOnce(&Backend) -> T,
) -> Result<Option<T>> {
    let Some(Value::Int4(number)) = args.first() else { return Err(bad_value()) };
    let backends = call.session.backends();
    Ok(backends.iter().find(|b| b.number == *number).map(read))
}

/// A value of a row that only a permitted user sees, or null.
fn guarded(
    call: &Call<'_>,
    args: &[Value],
    read: impl FnOnce(&Backend) -> Result<Value>,
) -> Result<Value> {
    match backend(call, args, |b| permitted(call.session, b.role).then(|| read(b)))? {
        Some(Some(value)) => value,
        _ => Ok(Value::Null),
    }
}

fn backend_pid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(backend(call, args, |b| Value::Int4(b.pid))?.unwrap_or(Value::Null))
}

fn backend_dbid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(backend(call, args, |b| Value::Oid(b.database))?.unwrap_or(Value::Null))
}

fn backend_userid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(backend(call, args, |b| Value::Oid(b.role))?.unwrap_or(Value::Null))
}

/// `pg_stat_get_backend_activity`: the text of the statement, or a text that tells why it is not there.
fn backend_activity(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = backend(call, args, |b| {
        if !permitted(call.session, b.role) {
            INSUFFICIENT.to_owned()
        } else if b.query.is_empty() {
            "<command string not enabled>".to_owned()
        } else {
            b.query.clone()
        }
    })?;
    Ok(Value::text(found.as_deref().unwrap_or(NOT_AVAILABLE)))
}

/// `pg_stat_get_backend_wait_event_type` and `pg_stat_get_backend_wait_event`.
fn backend_wait(call: &Call<'_>, args: &[Value], name: bool) -> Result<Value> {
    let found = backend(call, args, |b| {
        if !permitted(call.session, b.role) {
            return Some(INSUFFICIENT);
        }
        let (kind, event) = wait_event(b);
        if name { event } else { kind }
    })?;
    Ok(text(found.unwrap_or(Some(NOT_AVAILABLE))))
}

fn backend_wait_event_type(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    backend_wait(call, args, false)
}

fn backend_wait_event(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    backend_wait(call, args, true)
}

fn backend_activity_start(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    guarded(call, args, |b| Ok(time(b.query_start)))
}

fn backend_xact_start(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    guarded(call, args, |b| Ok(time(b.transaction_start)))
}

fn backend_start(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    guarded(call, args, |b| Ok(Value::TimestampTz(b.backend_start)))
}

fn backend_client_addr(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    guarded(call, args, |b| b.client.addr())
}

fn backend_client_port(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    guarded(call, args, |b| Ok(b.client.shown_port()))
}

/// `pg_stat_get_db_numbackends`: the number of sessions in the database.
fn db_numbackends(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let database = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let count = call.session.backends().iter().filter(|b| b.database == database).count();
    Ok(Value::Int4(i32::try_from(count).unwrap_or(i32::MAX)))
}

/// `pg_stat_clear_snapshot`: the next read of the table takes a new snapshot. The result is `void`, whose value the engine keeps as an empty text.
fn clear_snapshot(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    call.session.clear_backends();
    Ok(Value::text(""))
}

fn client_addr(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    call.session.addresses().0.addr()
}

fn client_port(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(call.session.addresses().0.port())
}

fn server_addr(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    call.session.addresses().1.addr()
}

fn server_port(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(call.session.addresses().1.port())
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_stat_get_backend_pid" => backend_pid,
        "pg_stat_get_backend_dbid" => backend_dbid,
        "pg_stat_get_backend_userid" => backend_userid,
        "pg_stat_get_backend_activity" => backend_activity,
        "pg_stat_get_backend_wait_event_type" => backend_wait_event_type,
        "pg_stat_get_backend_wait_event" => backend_wait_event,
        "pg_stat_get_backend_activity_start" => backend_activity_start,
        "pg_stat_get_backend_xact_start" => backend_xact_start,
        "pg_stat_get_backend_start" => backend_start,
        "pg_stat_get_backend_client_addr" => backend_client_addr,
        "pg_stat_get_backend_client_port" => backend_client_port,
        "pg_stat_get_db_numbackends" => db_numbackends,
        "pg_stat_clear_snapshot" => clear_snapshot,
        "inet_client_addr" => client_addr,
        "inet_client_port" => client_port,
        "inet_server_addr" => server_addr,
        "inet_server_port" => server_port,
        _ => return None,
    })
}

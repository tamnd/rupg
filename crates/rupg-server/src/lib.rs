//! The listener, TLS, authentication, `pg_hba.conf`, the task per connection, cancel.
//!
//! The server accepts connections, reads the startup packet, checks the user and the database, and gives the rest of the connection to [`rupg_session::connection::Connection`]. It owns the process IDs and the cancel keys of the sessions. The main loop of a session is in `rupg-session` (spec/06 section 6.1).
//!
//! At this step each connection has its own task. The server checks each connection with the rules of `pg_hba.conf` and the methods of spec/06 section 6.7, except the methods that need TLS. TLS and the workers of spec/06 section 6.2 come later in M2.
//!
//! The rules come from the file of the setting `hba_file` when it is set, else from [`Config::hba`], else from the defaults of spec/06 section 6.8. The tables `rupg_hba` and `rupg_ident` of the spec do not exist yet, so [`Config::hba`] and [`Config::ident`] stand in for them.
//!
//! The server listens on the TCP address of [`Config::listen`] and on the Unix socket `<dir>/.s.PGSQL.<port>` for each directory of `unix_socket_directories`, with the port of the TCP address (spec/06 section 6.15).
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod auth;
mod hba;

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rupg_common::{Error, Result, SqlState};
use rupg_platform::{Entropy, Io, Listener, Net, Stream, TaskHandle, Tasks};
use rupg_session::connection::{self, Connection, Next, Start};
use rupg_session::guc::Settings;
use rupg_wire::{
    CANCEL_KEY_LEN, CancelKey, Handshake, Hashes, Level, MOCK_NONCE_LEN, OutBuf, Replication,
    SCRAM_ITERATIONS, SCRAM_SALT_LEN, ScramSecret, Step, cancel_target, md5_encrypt,
    prepare_password, split_startup,
};

use crate::hba::{Hba, Ident, Roles, Source};

/// The size of the first read of a connection, which holds the startup packet.
const FIRST_READ: usize = 1024;
/// The size of each later read.
pub(crate) const READ_SIZE: usize = 16 * 1024;
/// The server sends the output when it holds this many bytes, also in the middle of a result.
const FLUSH_AT: usize = 64 * 1024;
/// The lowest process ID of a session. PostgreSQL uses the process ID of the backend, which is never this low on a host that runs it.
const FIRST_PID: i32 = 1001;

/// The name of the host rules in the messages when they do not come from a file.
const HBA_NAME: &str = "pg_hba.conf";
/// The name of the user maps in the messages when they do not come from a file.
const IDENT_NAME: &str = "pg_ident.conf";

/// The server log. The server writes the messages that PostgreSQL writes to its log and does not send to the client, such as the reason of a failed login.
pub trait Log: Send + Sync + fmt::Debug {
    /// Writes one message with its severity, such as `LOG` or `FATAL`. The text can have more lines, such as `DETAIL:  ...`.
    fn write(&self, severity: &str, text: &str);
}

/// The settings of a server.
#[derive(Clone, Debug)]
pub struct Config {
    /// The address of the listener, such as `127.0.0.1:5432`. The port 0 takes a free port.
    pub listen: String,
    /// The name of the only role, which is a superuser.
    pub superuser: String,
    /// The password of the superuser. `None` and an empty password give a role with no password.
    pub password: Option<String>,
    /// The text of the host rules, in the format of `pg_hba.conf`. The setting `hba_file` comes first. `None` gives the defaults of spec/06 section 6.8.
    pub hba: Option<String>,
    /// The text of the user maps, in the format of `pg_ident.conf`. The setting `ident_file` comes first. `None` gives no maps.
    pub ident: Option<String>,
    /// The settings of the command line, as `-c name=value`.
    pub settings: Vec<(String, String)>,
    /// The server log. `None` drops the messages.
    pub log: Option<Arc<dyn Log>>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            listen: "127.0.0.1:5432".into(),
            superuser: "postgres".into(),
            password: None,
            hba: None,
            ident: None,
            settings: Vec::new(),
            log: None,
        }
    }
}

/// The host rules of spec/06 section 6.8 when no rules are given. Without a password, only the Unix socket is open.
fn default_hba(password: bool) -> &'static str {
    if password {
        "local all all scram-sha-256\nhost all all 127.0.0.1/32 scram-sha-256\nhost all all ::1/128 scram-sha-256\n"
    } else {
        "local all all trust\n"
    }
}

/// The state that the tasks of the server share.
#[derive(Debug)]
struct Shared {
    base: Settings,
    roles: Roles,
    /// The stored secret of the superuser, in the format of `pg_authid.rolpassword`.
    secret: Option<String>,
    hba: Hba,
    ident: Ident,
    net: Arc<dyn Net>,
    /// The random bytes of the mock salt of a role that does not exist. They are new at each start of the server.
    mock_nonce: [u8; MOCK_NONCE_LEN],
    /// The iterations of the mock secret, `scram_iterations`.
    iterations: i32,
    log: Option<Arc<dyn Log>>,
    entropy: Arc<dyn Entropy>,
    /// The cancel key of each session, by process ID.
    keys: Mutex<BTreeMap<i32, CancelKey>>,
    stopping: AtomicBool,
}

/// A server that accepts connections until [`Server::stop`].
#[derive(Debug)]
pub struct Server {
    shared: Arc<Shared>,
    net: Arc<dyn Net>,
    address: String,
    sockets: Vec<String>,
    accept: Vec<TaskHandle>,
    connections: Arc<Mutex<Vec<TaskHandle>>>,
}

/// The name of the socket file of a port, as `UNIXSOCK_PATH` in `pqcomm.h` gives it.
fn socket_name(port: &str) -> String {
    format!(".s.PGSQL.{port}")
}

/// `SplitDirectoriesString` in `varlena.c`: the directories of a list separated by commas. A directory can be in double quotes, and a pair of double quotes in it stands for one. The spaces around each item and the slashes at the end of a path are not part of it.
///
/// # Errors
///
/// A list with a quote that does not end, or with an empty item.
fn split_directories(list: &str) -> std::result::Result<Vec<String>, ()> {
    let mut dirs = Vec::new();
    let mut rest = list.trim_start();
    if rest.is_empty() {
        return Ok(dirs);
    }
    loop {
        let mut dir = String::new();
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut chars = quoted.char_indices();
            loop {
                match chars.next() {
                    Some((i, '"')) if quoted[i + 1..].starts_with('"') => {
                        dir.push('"');
                        chars.next();
                    }
                    Some((i, '"')) => {
                        rest = quoted[i + 1..].trim_start();
                        break;
                    }
                    Some((_, c)) => dir.push(c),
                    None => return Err(()),
                }
            }
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            dir = rest[..end].trim_end().to_string();
            rest = &rest[end..];
        }
        while dir.len() > 1 && dir.ends_with('/') {
            dir.pop();
        }
        if dir.is_empty() {
            return Err(());
        }
        dirs.push(dir);
        match rest.strip_prefix(',') {
            Some(next) => rest = next.trim_start(),
            None if rest.is_empty() => return Ok(dirs),
            None => return Err(()),
        }
    }
}

/// The paths of the Unix sockets for the TCP address `address`.
fn socket_paths(settings: &Settings, address: &str) -> Result<Vec<String>> {
    let list = settings.get("unix_socket_directories").unwrap_or_default();
    let dirs = split_directories(&list).map_err(|()| {
        Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "invalid list syntax in parameter \"unix_socket_directories\"",
        )
    })?;
    let port = address.rsplit_once(':').map_or("5432", |(_, port)| port);
    let name = socket_name(port);
    dirs.iter()
        .map(|dir| {
            if !dir.starts_with('/') {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"unix_socket_directories\"",
                )
                .with_detail(format!("The directory \"{dir}\" is not an absolute path.")));
            }
            Ok(if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") })
        })
        .collect()
}

impl Server {
    /// Starts the listener and the task that accepts connections.
    ///
    /// # Errors
    ///
    /// A setting of the command line that is not valid, host rules that do not load, an address that the server cannot listen on, and a task that does not start.
    pub fn start(
        config: &Config,
        net: Arc<dyn Net>,
        io: Arc<dyn Io>,
        tasks: Arc<dyn Tasks>,
        entropy: Arc<dyn Entropy>,
    ) -> Result<Server> {
        let listener = net.listen(&config.listen)?;
        let address = listener.local_addr();
        // `listen_addresses` and `port` show the TCP address that the server got.
        let mut args = config.settings.clone();
        if let Some((host, port)) = address.rsplit_once(':') {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            args.push(("listen_addresses".into(), host.into()));
            args.push(("port".into(), port.into()));
        }
        let base = connection::base_settings(&args)?;
        let sockets = socket_paths(&base, &address)?;
        let write = |severity: &str, lines: &[String]| {
            if let Some(log) = &config.log {
                for line in lines {
                    log.write(severity, line);
                }
            }
        };
        let (hba, ident) = load_rules(config, &base, &*io, &write)?;
        let mut listeners = vec![listener];
        for path in &sockets {
            listeners.push(net.listen(path)?);
        }
        let secret = config
            .password
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| stored_secret(&base, &*entropy, &config.superuser, p));
        let mut mock_nonce = [0; MOCK_NONCE_LEN];
        entropy.fill(&mut mock_nonce);
        let iterations = scram_iterations(&base);
        let shared = Arc::new(Shared {
            base,
            roles: Roles { superuser: config.superuser.clone() },
            secret,
            hba,
            ident,
            net: net.clone(),
            mock_nonce,
            iterations,
            log: config.log.clone(),
            entropy,
            keys: Mutex::new(BTreeMap::new()),
            stopping: AtomicBool::new(false),
        });
        let connections = Arc::new(Mutex::new(Vec::new()));
        let mut server = Server { shared, net, address, sockets, accept: Vec::new(), connections };
        for listener in listeners {
            let shared = server.shared.clone();
            let connections = server.connections.clone();
            let spawner = tasks.clone();
            let task = tasks.spawn(
                "rupg-accept",
                Box::new(move || accept(&shared, &*listener, &*spawner, &connections)),
            );
            match task {
                Ok(task) => server.accept.push(task),
                Err(error) => {
                    let _ = server.stop();
                    return Err(error);
                }
            }
        }
        Ok(server)
    }

    /// The TCP address of the server, with the port that it got.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The paths of the Unix sockets of the server.
    pub fn sockets(&self) -> &[String] {
        &self.sockets
    }

    /// The smart shutdown: the server accepts no more connections, and waits until each client closes its connection.
    ///
    /// # Errors
    ///
    /// A task of the server that panicked.
    pub fn stop(mut self) -> Result<()> {
        self.shared.stopping.store(true, Ordering::SeqCst);
        // A connection to each listener wakes the task that waits in accept.
        let addresses = std::iter::once(&self.address).chain(&self.sockets);
        for address in addresses.take(self.accept.len()) {
            if let Ok(stream) = self.net.connect(address) {
                let _ = stream.shutdown();
            }
        }
        for accept in std::mem::take(&mut self.accept) {
            accept.join()?;
        }
        let tasks = std::mem::take(&mut *lock(&self.connections));
        for task in tasks {
            task.join()?;
        }
        Ok(())
    }

    /// Waits until the tasks that accept connections end, which is never before [`Server::stop`].
    ///
    /// # Errors
    ///
    /// A task panicked.
    pub fn wait(mut self) -> Result<()> {
        for accept in std::mem::take(&mut self.accept) {
            accept.join()?;
        }
        Ok(())
    }
}

impl Shared {
    /// Writes a message to the server log.
    fn log(&self, severity: &str, text: &str) {
        if let Some(log) = &self.log {
            log.write(severity, text);
        }
    }

    /// Writes an error to the server log with its detail and its hint.
    fn log_error(&self, severity: &str, error: &Error) {
        let mut text = error.message().to_owned();
        if let Some(detail) = error.detail() {
            text.push_str("\nDETAIL:  ");
            text.push_str(detail);
        }
        if let Some(hint) = error.hint() {
            text.push_str("\nHINT:  ");
            text.push_str(hint);
        }
        self.log(severity, &text);
    }

    /// Writes each message of a list to the server log.
    fn log_all(&self, severity: &str, lines: &[String]) {
        for line in lines {
            self.log(severity, line);
        }
    }
}

/// The stored secret of a password, as `encrypt_password` makes it with the setting `password_encryption`.
fn stored_secret(settings: &Settings, entropy: &dyn Entropy, user: &str, password: &str) -> String {
    if settings.get("password_encryption").as_deref() == Some("md5") {
        return md5_encrypt(&Hashes, password.as_bytes(), user.as_bytes());
    }
    let mut salt = [0; SCRAM_SALT_LEN];
    entropy.fill(&mut salt);
    let prepared = prepare_password(password.as_bytes());
    ScramSecret::build(&Hashes, &prepared, &salt, scram_iterations(settings)).to_string()
}

/// The setting `scram_iterations`.
fn scram_iterations(settings: &Settings) -> i32 {
    settings.get("scram_iterations").and_then(|v| v.parse().ok()).unwrap_or(SCRAM_ITERATIONS)
}

/// `load_hba` and `load_ident`. An error in the host rules stops the start, as in `PostmasterMain`. An error in the user maps only goes to the log, and then no map matches.
fn load_rules(
    config: &Config,
    base: &Settings,
    io: &dyn Io,
    log: &dyn Fn(&str, &[String]),
) -> Result<(Hba, Ident)> {
    let ssl = base.get("ssl").as_deref() == Some("on");
    let password = config.password.as_deref().is_some_and(|p| !p.is_empty());
    let hba_file = base.get("hba_file").filter(|f| !f.is_empty());
    let (source, name) = match &hba_file {
        Some(path) => (Source::File(path), path.as_str()),
        None => {
            let text = config.hba.as_deref().unwrap_or(default_hba(password));
            (Source::Text { name: HBA_NAME, text }, HBA_NAME)
        }
    };
    let mut lines = Vec::new();
    let hba = Hba::load(io, source, ssl, &mut lines);
    log("LOG", &lines);
    let Some(hba) = hba else {
        return Err(Error::internal(format!("could not load {name}")));
    };
    let ident_file = base.get("ident_file").filter(|f| !f.is_empty());
    let source = match &ident_file {
        Some(path) => Source::File(path),
        None => Source::Text { name: IDENT_NAME, text: config.ident.as_deref().unwrap_or("") },
    };
    let mut lines = Vec::new();
    let ident = Ident::load(io, source, &mut lines);
    log("LOG", &lines);
    Ok((hba, ident.unwrap_or_default()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The task that accepts connections and starts a task for each one.
fn accept(
    shared: &Arc<Shared>,
    listener: &dyn Listener,
    tasks: &dyn Tasks,
    connections: &Mutex<Vec<TaskHandle>>,
) {
    loop {
        let stream = listener.accept();
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        // An error of accept is about one connection, such as a client that reset it first.
        let Ok(stream) = stream else { continue };
        let task_shared = shared.clone();
        let task = tasks.spawn("rupg-connection", Box::new(move || serve(&task_shared, stream)));
        let mut connections = lock(connections);
        connections.retain(|task| !task.is_finished());
        if let Ok(task) = task {
            connections.push(task);
        }
    }
}

/// The bytes that a connection read and did not use yet, and the output that waits.
struct Wire {
    stream: Box<dyn Stream>,
    input: Vec<u8>,
    at: usize,
    out: OutBuf,
}

impl Wire {
    fn pending(&self) -> &[u8] {
        &self.input[self.at..]
    }

    fn consume(&mut self, n: usize) {
        self.at += n;
    }

    /// Reads more bytes. It gives false at the end of the stream and at an error.
    fn fill(&mut self, size: usize) -> bool {
        self.input.drain(..self.at);
        self.at = 0;
        let len = self.input.len();
        self.input.resize(len + size, 0);
        let read = self.stream.read(&mut self.input[len..]);
        let n = read.unwrap_or(0);
        self.input.truncate(len + n);
        n > 0
    }

    /// Sends the output. It gives false at an error.
    fn flush(&mut self) -> bool {
        if self.out.is_empty() {
            return true;
        }
        let sent = self.stream.write_all(self.out.as_bytes()).and_then(|()| self.stream.flush());
        self.out.consume(self.out.len());
        sent.is_ok()
    }

    /// Sends a `FATAL` error.
    fn fatal(&mut self, error: &Error) {
        connection::write_error(&mut self.out, "FATAL", error, None, false);
        self.flush();
    }
}

/// The task of one connection.
fn serve(shared: &Shared, stream: Box<dyn Stream>) {
    let mut wire = Wire { stream, input: Vec::new(), at: 0, out: OutBuf::new() };
    if let Some((start, protocol)) = startup(shared, &mut wire) {
        session(shared, &mut wire, &start, protocol);
    }
    let _ = wire.stream.shutdown();
}

/// Reads the packets before the startup message, and the startup message. It gives the startup request and the version of the protocol, or `None` when the connection ends here.
fn startup(shared: &Shared, wire: &mut Wire) -> Option<(Start, u32)> {
    let mut handshake = Handshake::new(false);
    loop {
        let (packet, size) = match split_startup(wire.pending()) {
            Ok(Some(found)) => found,
            Ok(None) => {
                if !wire.fill(FIRST_READ) {
                    return None;
                }
                continue;
            }
            // PostgreSQL writes a bad length to its log only and closes the connection.
            Err(error) if error.level == Level::Log => return None,
            Err(error) => {
                wire.out.protocol_error(&error, handshake.protocol());
                wire.flush();
                return None;
            }
        };
        let mut out = OutBuf::new();
        let step = handshake.packet(packet, &mut out);
        let found = match step {
            Ok(Step::Answer { request, .. }) => {
                let buffered = wire.pending().len() - size;
                Err(Some(request.check_buffered(buffered)))
            }
            Ok(Step::Cancel(cancel)) => {
                let keys = lock(&shared.keys);
                // A session runs a statement only for a short time and checks for no cancel yet, so a good request has nothing to stop.
                let _ = cancel_target(&cancel, |pid| keys.get(&pid).copied());
                Err(None)
            }
            Ok(Step::Start(request)) => Ok((
                Start {
                    user: String::from_utf8_lossy(request.user).into_owned(),
                    database: String::from_utf8_lossy(request.database).into_owned(),
                    superuser: true,
                    options: request.options.map(|o| String::from_utf8_lossy(o).into_owned()),
                    settings: request
                        .settings
                        .iter()
                        .map(|(n, v)| {
                            (
                                String::from_utf8_lossy(n).into_owned(),
                                String::from_utf8_lossy(v).into_owned(),
                            )
                        })
                        .collect(),
                },
                request.protocol,
                request.replication,
            )),
            Err(error) => {
                out.protocol_error(&error, handshake.protocol());
                Err(None)
            }
        };
        wire.consume(size);
        wire.out.bytes_mut().extend_from_slice(out.as_bytes());
        match found {
            Ok((start, protocol, replication)) => {
                if replication != Replication::Off {
                    let error = Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "replication connections are not supported",
                    );
                    wire.fatal(&error);
                    return None;
                }
                return Some((start, protocol));
            }
            // The answer to an encryption request, then the next packet.
            Err(Some(Ok(()))) => {
                if !wire.flush() {
                    return None;
                }
            }
            Err(Some(Err(error))) => {
                wire.out.protocol_error(&error, handshake.protocol());
                wire.flush();
                return None;
            }
            Err(None) => {
                wire.flush();
                return None;
            }
        }
    }
}

/// The checks of `InitPostgres` after the authentication: the role and the database.
fn admit(shared: &Shared, start: &Start) -> std::result::Result<(), Error> {
    if start.user != shared.roles.superuser {
        return Err(Error::new(
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            format!("role \"{}\" does not exist", start.user),
        ));
    }
    match start.database.as_str() {
        "postgres" | "template1" => Ok(()),
        "template0" => Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "database \"template0\" is not currently accepting connections",
        )),
        name => Err(Error::new(
            SqlState::INVALID_CATALOG_NAME,
            format!("database \"{name}\" does not exist"),
        )),
    }
}

/// Removes the cancel key of a session when the session ends.
struct Registered<'a> {
    shared: &'a Shared,
    pid: i32,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(&self.shared.keys).remove(&self.pid);
    }
}

/// Gives the session a process ID that no other session has, and a random cancel key.
fn register(shared: &Shared, protocol: u32) -> (Registered<'_>, CancelKey) {
    let mut keys = lock(&shared.keys);
    let span = u64::try_from(i32::MAX - FIRST_PID).unwrap_or(1);
    loop {
        let pid = FIRST_PID + i32::try_from(shared.entropy.next_u64() % span).unwrap_or(0);
        if keys.contains_key(&pid) {
            continue;
        }
        let mut random = [0; CANCEL_KEY_LEN];
        shared.entropy.fill(&mut random);
        let key = CancelKey::new(pid, protocol, random);
        keys.insert(pid, key);
        return (Registered { shared, pid }, key);
    }
}

/// The authentication, the start of the session, and the main loop.
fn session(shared: &Shared, wire: &mut Wire, start: &Start, protocol: u32) {
    let Some(notices) = auth::authenticate(shared, wire, start, protocol) else {
        return;
    };
    wire.out.authentication_ok();
    let settings =
        admit(shared, start).and_then(|()| connection::session_settings(&shared.base, start));
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            shared.log_error("FATAL", &error);
            return wire.fatal(&error);
        }
    };
    // `EmitConnectionWarnings` at the end of `InitPostgres`, after the settings of the startup packet.
    let md5_warnings = settings.get("md5_password_warnings").as_deref() != Some("off");
    let quiet = settings.get("client_min_messages").as_deref() == Some("error");
    for notice in notices {
        if quiet || (notice.md5 && !md5_warnings) {
            continue;
        }
        let warning = Error::new(SqlState::WARNING, notice.message).with_detail(notice.detail);
        connection::write_error(&mut wire.out, "WARNING", &warning, None, true);
    }
    let (_registered, key) = register(shared, protocol);
    let mut connection = Connection::new(start, settings, protocol);
    connection.greet(&key, &mut wire.out);
    loop {
        let step = connection.step(&wire.input[wire.at..], &mut wire.out);
        wire.consume(step.used);
        let going = match step.next {
            Next::Continue => wire.out.len() < FLUSH_AT || wire.flush(),
            Next::Flush => wire.flush(),
            Next::Read => wire.flush() && wire.fill(READ_SIZE),
            Next::Close => {
                wire.flush();
                false
            }
        };
        if !going {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_directories;

    #[test]
    fn directory_lists() {
        let split = |list: &str| split_directories(list);
        assert_eq!(split(""), Ok(vec![]));
        assert_eq!(split(" /tmp "), Ok(vec!["/tmp".to_string()]));
        assert_eq!(
            split("/tmp/, /run/rupg"),
            Ok(vec!["/tmp".to_string(), "/run/rupg".to_string()])
        );
        assert_eq!(
            split(r#""/a,b" , "/c""d""#),
            Ok(vec!["/a,b".to_string(), r#"/c"d"#.to_string()])
        );
        assert_eq!(split("/"), Ok(vec!["/".to_string()]));
        assert_eq!(split("/tmp,"), Err(()));
        assert_eq!(split(r#""/tmp"#), Err(()));
        assert_eq!(split(r#""/a" b"#), Err(()));
    }
}

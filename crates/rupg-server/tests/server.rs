//! Runs the server on a free port of the loopback address and talks to it as a client.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rupg_platform::os::{OsEntropy, OsIo, OsNet, OsTasks};
use rupg_platform::{Net, Stream, Tasks};
use rupg_server::{Config, Log, Server};
use rupg_wire::{
    Authentication, Backend, CANCEL_REQUEST_CODE, Crypto, Hashes, PROTOCOL_3_0, md5_encrypt,
};

fn start(config: &Config) -> rupg_common::Result<Server> {
    Server::start(config, Arc::new(OsNet), Arc::new(OsIo), Arc::new(OsTasks), Arc::new(OsEntropy))
}

fn server() -> Server {
    let config = Config {
        listen: "127.0.0.1:0".into(),
        hba: Some("local all all trust\nhost all all 127.0.0.1/32 trust\n".into()),
        settings: vec![("work_mem".into(), "16MB".into())],
        ..Config::default()
    };
    start(&config).unwrap()
}

/// A log that keeps the messages for the test.
#[derive(Debug, Default)]
struct Kept(Mutex<Vec<String>>);

impl Log for Kept {
    fn write(&self, severity: &str, text: &str) {
        self.0.lock().unwrap().push(format!("{severity}:  {text}"));
    }
}

impl Kept {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A server with the password `secret` for `postgres`, the host rules `hba` and the settings `settings`.
fn password_server(hba: &str, settings: &[(&str, &str)]) -> (Server, Arc<Kept>) {
    let log = Arc::new(Kept::default());
    let config = Config {
        listen: "127.0.0.1:0".into(),
        password: Some("secret".into()),
        hba: Some(hba.into()),
        settings: settings.iter().map(|(n, v)| ((*n).into(), (*v).into())).collect(),
        log: Some(log.clone()),
        ..Config::default()
    };
    (start(&config).unwrap(), log)
}

/// A `StartupMessage` with the parameters.
fn startup(params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
    for (name, value) in params {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = u32::try_from(body.len() + 4).unwrap().to_be_bytes().to_vec();
    packet.extend(body);
    packet
}

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn query(sql: &str) -> Vec<u8> {
    message(b'Q', format!("{sql}\0").as_bytes())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Reads messages until `ReadyForQuery` or the end of the stream, and gives one short line for each message.
fn read(stream: &mut dyn Stream) -> Vec<String> {
    let mut input = Vec::new();
    let mut lines = Vec::new();
    loop {
        while let Some((message, size)) = Backend::decode(&input).unwrap() {
            let (line, ready) = match message {
                Backend::Authentication(_) => ("auth".to_owned(), false),
                Backend::NoticeResponse(fields) => {
                    let field = |code| text(fields.iter().find(|(c, _)| *c == code).unwrap().1);
                    (format!("{} {} {}", field(b'S'), field(b'C'), field(b'M')), false)
                }
                Backend::ParameterStatus { name, value } => {
                    (format!("{}={}", text(name), text(value)), false)
                }
                Backend::BackendKeyData { pid, .. } => (format!("key {}", pid > 1000), false),
                Backend::ReadyForQuery(status) => {
                    (format!("ready {}", char::from(status.byte())), true)
                }
                Backend::CommandComplete(tag) => (text(tag), false),
                Backend::RowDescription(fields) => (format!("columns {}", fields.len()), false),
                Backend::DataRow(values) => {
                    let values: Vec<String> = values.iter().map(|v| text(v.unwrap())).collect();
                    (format!("row {}", values.join(",")), false)
                }
                Backend::ErrorResponse(fields) => {
                    let field = |code| text(fields.iter().find(|(c, _)| *c == code).unwrap().1);
                    (format!("{} {} {}", field(b'S'), field(b'C'), field(b'M')), false)
                }
                other => (format!("{other:?}"), false),
            };
            input.drain(..size);
            lines.push(line);
            if ready {
                return lines;
            }
        }
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return lines;
        }
        input.extend_from_slice(&buf[..n]);
    }
}

fn connect(server: &Server, params: &[(&str, &str)]) -> (Box<dyn Stream>, Vec<String>) {
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&startup(params)).unwrap();
    let lines = read(&mut *stream);
    (stream, lines)
}

#[test]
fn a_session() {
    let server = server();
    let (mut stream, lines) = connect(&server, &[("user", "postgres"), ("database", "postgres")]);
    assert_eq!(lines[0], "auth");
    assert!(lines.contains(&"session_authorization=postgres".to_owned()), "{lines:?}");
    assert_eq!(lines[lines.len() - 2..], ["key true", "ready I"]);
    stream.write_all(&query("SHOW work_mem; BEGIN")).unwrap();
    assert_eq!(read(&mut *stream), ["columns 1", "row 16MB", "SHOW", "BEGIN", "ready T"]);
    stream.write_all(&query("SELECT 1")).unwrap();
    assert_eq!(read(&mut *stream), ["columns 1", "row 1", "SELECT 1", "ready T"]);
    stream.write_all(&query("SELECT 1/0")).unwrap();
    assert_eq!(read(&mut *stream), ["ERROR 22012 division by zero", "ready E"]);
    stream.write_all(&query("ROLLBACK")).unwrap();
    assert_eq!(read(&mut *stream), ["ROLLBACK", "ready I"]);
    stream.write_all(&message(b'X', b"")).unwrap();
    assert!(read(&mut *stream).is_empty());
    server.stop().unwrap();
}

#[test]
fn an_idle_session() {
    // After 1 second with no message, the session gives back the memory of its stack and of its buffers. It keeps its state, and also the part of a message that it has.
    let server = server();
    let (mut stream, _) = connect(&server, &[("user", "postgres"), ("database", "postgres")]);
    stream.write_all(&query("BEGIN; SET work_mem = '1MB'")).unwrap();
    assert_eq!(read(&mut *stream), ["BEGIN", "SET", "ready T"]);
    OsTasks.sleep(Duration::from_millis(1500));
    let show = query("SHOW work_mem");
    stream.write_all(&show[..7]).unwrap();
    OsTasks.sleep(Duration::from_millis(1500));
    stream.write_all(&show[7..]).unwrap();
    assert_eq!(read(&mut *stream), ["columns 1", "row 1MB", "SHOW", "ready T"]);
    stream.write_all(&query("COMMIT")).unwrap();
    assert_eq!(read(&mut *stream), ["COMMIT", "ready I"]);
    drop(stream);
    server.stop().unwrap();
}

#[test]
fn refused_connections() {
    let server = server();
    let cases = [
        (("bob", "postgres"), "FATAL 28000 role \"bob\" does not exist"),
        (("postgres", "nope"), "FATAL 3D000 database \"nope\" does not exist"),
        (
            ("postgres", "template0"),
            "FATAL 55000 database \"template0\" is not currently accepting connections",
        ),
    ];
    for ((user, database), error) in cases {
        let (_, lines) = connect(&server, &[("user", user), ("database", database)]);
        assert_eq!(lines, ["auth", error]);
    }
    let (_, lines) = connect(&server, &[("user", "postgres"), ("client_encoding", "LATIN1")]);
    assert_eq!(
        lines,
        ["auth", "FATAL 0A000 invalid value for parameter \"client_encoding\": \"LATIN1\""]
    );
    server.stop().unwrap();
}

#[test]
fn an_ssl_request() {
    let server = server();
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    stream.write_all(&startup(&[("user", "postgres")])).unwrap();
    assert_eq!(read(&mut *stream).last().unwrap(), "ready I");
    drop(stream);
    server.stop().unwrap();
}

#[cfg(unix)]
#[test]
fn a_unix_socket() {
    let server = server();
    let port = server.address().rsplit_once(':').unwrap().1.to_owned();
    let path = format!("/tmp/.s.PGSQL.{port}");
    assert_eq!(server.sockets(), std::slice::from_ref(&path));
    let mut stream = OsNet.connect(&path).unwrap();
    stream.write_all(&startup(&[("user", "postgres")])).unwrap();
    assert_eq!(read(&mut *stream).last().unwrap(), "ready I");
    stream
        .write_all(&query("SHOW unix_socket_directories; SHOW port; SHOW listen_addresses"))
        .unwrap();
    let port_row = format!("row {port}");
    assert_eq!(
        read(&mut *stream),
        [
            "columns 1",
            "row /tmp",
            "SHOW",
            "columns 1",
            &port_row,
            "SHOW",
            "columns 1",
            "row 127.0.0.1",
            "SHOW",
            "ready I"
        ]
    );
    drop(stream);
    server.stop().unwrap();
    assert!(OsNet.connect(&path).is_err());
}

#[test]
fn socket_directories() {
    let start_in = |dirs: &str| {
        let config = Config {
            listen: "127.0.0.1:0".into(),
            settings: vec![("unix_socket_directories".into(), dirs.into())],
            ..Config::default()
        };
        start(&config)
    };
    let server = start_in("").unwrap();
    assert!(server.sockets().is_empty());
    server.stop().unwrap();
    let error = start_in("/tmp,").unwrap_err();
    assert_eq!(error.message(), "invalid list syntax in parameter \"unix_socket_directories\"");
    let error = start_in("tmp").unwrap_err();
    assert_eq!(error.detail(), Some("The directory \"tmp\" is not an absolute path."));
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Vec<u8> {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let digits: Vec<u32> = text
        .bytes()
        .filter(|b| *b != b'=')
        .map(|b| u32::try_from(ABC.iter().position(|c| *c == b).unwrap()).unwrap())
        .collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, d)| n | d << (18 - 6 * i));
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// The client side of SCRAM-SHA-256 without channel binding, RFC 5802.
fn scram_final(password: &str, first_bare: &str, server_first: &str) -> String {
    let field = |name: &str| {
        server_first.split(',').find_map(|part| part.strip_prefix(name)).unwrap().to_owned()
    };
    let nonce = field("r=");
    let salt = unbase64(&field("s="));
    let iterations: u32 = field("i=").parse().unwrap();
    let salted = Hashes.pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    let client_key = Hashes.hmac_sha256(&salted, &[b"Client Key"]);
    let stored_key = Hashes.sha256(&[&client_key]);
    let without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{first_bare},{server_first},{without_proof}");
    let signature = Hashes.hmac_sha256(&stored_key, &[auth_message.as_bytes()]);
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    format!("{without_proof},p={}", base64(&proof))
}

/// Connects to `at`, answers each request for a password with `password`, and gives one line for each message until `ReadyForQuery` or the end of the stream. `ParameterStatus` and `BackendKeyData` give no line.
fn login(at: &str, params: &[(&str, &str)], password: &str) -> Vec<String> {
    const FIRST_BARE: &str = "n=,r=rOprNGfwEbeRWgbNEkqO";
    let user = params.iter().find(|(name, _)| *name == "user").unwrap().1;
    let mut stream = OsNet.connect(at).unwrap();
    stream.write_all(&startup(params)).unwrap();
    let mut input = Vec::new();
    let mut lines = Vec::new();
    loop {
        while let Some((m, size)) = Backend::decode(&input).unwrap() {
            let mut answer = None;
            let line = match m {
                Backend::Authentication(Authentication::Ok) => Some("ok".to_owned()),
                Backend::Authentication(Authentication::CleartextPassword) => {
                    answer = Some(message(b'p', format!("{password}\0").as_bytes()));
                    Some("cleartext".to_owned())
                }
                Backend::Authentication(Authentication::Md5Password(salt)) => {
                    let inner = md5_encrypt(&Hashes, password.as_bytes(), user.as_bytes());
                    let outer = md5_encrypt(&Hashes, &inner.as_bytes()[3..], &salt);
                    answer = Some(message(b'p', format!("{outer}\0").as_bytes()));
                    Some("md5".to_owned())
                }
                Backend::Authentication(Authentication::Sasl(mechanisms)) => {
                    let first = format!("n,,{FIRST_BARE}");
                    let mut body = b"SCRAM-SHA-256\0".to_vec();
                    body.extend_from_slice(&i32::try_from(first.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(first.as_bytes());
                    answer = Some(message(b'p', &body));
                    let names: Vec<String> = mechanisms.iter().map(|m| text(m)).collect();
                    Some(format!("sasl {}", names.join(",")))
                }
                Backend::Authentication(Authentication::SaslContinue(data)) => {
                    let last = scram_final(password, FIRST_BARE, &text(data));
                    answer = Some(message(b'p', last.as_bytes()));
                    Some("sasl continue".to_owned())
                }
                Backend::Authentication(Authentication::SaslFinal(_)) => {
                    Some("sasl final".to_owned())
                }
                Backend::ErrorResponse(fields) | Backend::NoticeResponse(fields) => {
                    let field = |code| text(fields.iter().find(|(c, _)| *c == code).unwrap().1);
                    Some(format!("{} {} {}", field(b'S'), field(b'C'), field(b'M')))
                }
                Backend::ReadyForQuery(status) => {
                    lines.push(format!("ready {}", char::from(status.byte())));
                    return lines;
                }
                Backend::ParameterStatus { .. } | Backend::BackendKeyData { .. } => None,
                other => Some(format!("{other:?}")),
            };
            input.drain(..size);
            lines.extend(line);
            if let Some(answer) = answer {
                stream.write_all(&answer).unwrap();
            }
        }
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return lines;
        }
        input.extend_from_slice(&buf[..n]);
    }
}

const POSTGRES: [(&str, &str); 2] = [("user", "postgres"), ("database", "postgres")];

#[test]
fn scram() {
    let (server, log) = password_server("host all all 127.0.0.1/32 scram-sha-256\n", &[]);
    let lines = login(server.address(), &POSTGRES, "secret");
    assert_eq!(lines, ["sasl SCRAM-SHA-256", "sasl continue", "sasl final", "ok", "ready I"]);
    let failed = "FATAL 28P01 password authentication failed for user \"postgres\"";
    let lines = login(server.address(), &POSTGRES, "wrong");
    assert_eq!(lines, ["sasl SCRAM-SHA-256", "sasl continue", failed]);
    let matched = "Connection matched file \"pg_hba.conf\" line 1: \"host all all 127.0.0.1/32 scram-sha-256\"";
    assert_eq!(
        log.take(),
        [format!(
            "FATAL:  password authentication failed for user \"postgres\"\nDETAIL:  {matched}"
        )]
    );
    // A role that does not exist goes through the same exchange with a mock secret.
    let lines = login(server.address(), &[("user", "bob"), ("database", "postgres")], "secret");
    assert_eq!(
        lines,
        [
            "sasl SCRAM-SHA-256",
            "sasl continue",
            "FATAL 28P01 password authentication failed for user \"bob\""
        ]
    );
    assert_eq!(
        log.take(),
        [format!(
            "FATAL:  password authentication failed for user \"bob\"\nDETAIL:  Role \"bob\" does not exist.\n{matched}"
        )]
    );
    server.stop().unwrap();
}

#[test]
fn md5() {
    let hba = "host all all 127.0.0.1/32 md5\n";
    let (server, _) = password_server(hba, &[("password_encryption", "md5")]);
    let warning = "WARNING 01000 authenticated with an MD5-encrypted password";
    let lines = login(server.address(), &POSTGRES, "secret");
    assert_eq!(lines, ["md5", "ok", warning, "ready I"]);
    let quiet = [("user", "postgres"), ("md5_password_warnings", "off")];
    assert_eq!(login(server.address(), &quiet, "secret"), ["md5", "ok", "ready I"]);
    let quiet = [("user", "postgres"), ("client_min_messages", "error")];
    assert_eq!(login(server.address(), &quiet, "secret"), ["md5", "ok", "ready I"]);
    let lines = login(server.address(), &POSTGRES, "wrong");
    assert_eq!(lines, ["md5", "FATAL 28P01 password authentication failed for user \"postgres\""]);
    // `password` checks an MD5 secret too, and warns.
    server.stop().unwrap();
    let (server, _) =
        password_server("host all all 127.0.0.1/32 password\n", &[("password_encryption", "md5")]);
    assert_eq!(
        login(server.address(), &POSTGRES, "secret"),
        ["cleartext", "ok", warning, "ready I"]
    );
    server.stop().unwrap();
    // With a SCRAM secret, `md5` runs SCRAM.
    let (server, _) = password_server(hba, &[]);
    let lines = login(server.address(), &POSTGRES, "secret");
    assert_eq!(lines, ["sasl SCRAM-SHA-256", "sasl continue", "sasl final", "ok", "ready I"]);
    server.stop().unwrap();
}

#[test]
fn password() {
    let (server, log) = password_server("host all all 127.0.0.1/32 password\n", &[]);
    assert_eq!(login(server.address(), &POSTGRES, "secret"), ["cleartext", "ok", "ready I"]);
    let lines = login(server.address(), &POSTGRES, "wrong");
    assert_eq!(
        lines,
        ["cleartext", "FATAL 28P01 password authentication failed for user \"postgres\""]
    );
    assert_eq!(
        log.take(),
        [
            "FATAL:  password authentication failed for user \"postgres\"\nDETAIL:  Password does not match for user \"postgres\".\nConnection matched file \"pg_hba.conf\" line 1: \"host all all 127.0.0.1/32 password\""
        ]
    );
    // The connection ends without a message when the client closes it in the exchange.
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&startup(&POSTGRES)).unwrap();
    let mut request = [0; 9];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, [b'R', 0, 0, 0, 8, 0, 0, 0, 3]);
    drop(stream);
    server.stop().unwrap();
    assert!(log.take().is_empty());
}

#[test]
fn rejected_hosts() {
    let (server, log) = password_server("host all all 127.0.0.1/32 reject\n", &[]);
    assert_eq!(
        login(server.address(), &POSTGRES, "secret"),
        [
            "FATAL 28000 pg_hba.conf rejects connection for host \"127.0.0.1\", user \"postgres\", database \"postgres\", no encryption"
        ]
    );
    server.stop().unwrap();
    assert_eq!(log.take().len(), 1);
    let (server, log) = password_server("local all all trust\n", &[]);
    let error = "no pg_hba.conf entry for host \"127.0.0.1\", user \"postgres\", database \"postgres\", no encryption";
    assert_eq!(login(server.address(), &POSTGRES, "secret"), [format!("FATAL 28000 {error}")]);
    server.stop().unwrap();
    assert_eq!(log.take(), [format!("FATAL:  {error}")]);
}

#[test]
fn default_rules() {
    let config = Config {
        listen: "127.0.0.1:0".into(),
        password: Some("secret".into()),
        ..Config::default()
    };
    let server = start(&config).unwrap();
    let lines = login(server.address(), &POSTGRES, "secret");
    assert_eq!(lines, ["sasl SCRAM-SHA-256", "sasl continue", "sasl final", "ok", "ready I"]);
    server.stop().unwrap();
    // Without a password only the Unix socket is open.
    let config = Config { listen: "127.0.0.1:0".into(), ..Config::default() };
    let server = start(&config).unwrap();
    let lines = login(server.address(), &POSTGRES, "secret");
    assert!(lines[0].starts_with("FATAL 28000 no pg_hba.conf entry for host"), "{lines:?}");
    #[cfg(unix)]
    assert_eq!(login(&server.sockets()[0], &POSTGRES, ""), ["ok", "ready I"]);
    server.stop().unwrap();
}

#[test]
fn rules_that_do_not_load() {
    let log = Arc::new(Kept::default());
    let config = Config {
        listen: "127.0.0.1:0".into(),
        hba: Some("local all all trust\nfoo all all trust\n".into()),
        log: Some(log.clone()),
        ..Config::default()
    };
    let error = start(&config).unwrap_err();
    assert_eq!((error.state().as_str(), error.message()), ("XX000", "could not load pg_hba.conf"));
    assert_eq!(
        log.take(),
        [
            "LOG:  invalid connection type \"foo\"\nCONTEXT:  line 2 of configuration file \"pg_hba.conf\""
        ]
    );
    let path = "/nonexistent/rupg/pg_hba.conf";
    let config = Config { settings: vec![("hba_file".into(), path.into())], hba: None, ..config };
    let error = start(&config).unwrap_err();
    assert_eq!(error.message(), format!("could not load {path}"));
    assert_eq!(
        log.take(),
        [format!("LOG:  could not open file \"{path}\": No such file or directory")]
    );
    // An error in the user maps only goes to the log.
    let config = Config { settings: Vec::new(), ident: Some("map tom\n".into()), ..config };
    let server = start(&config).unwrap();
    server.stop().unwrap();
    assert_eq!(
        log.take(),
        [
            "LOG:  missing entry at end of line\nCONTEXT:  line 1 of configuration file \"pg_ident.conf\""
        ]
    );
}

#[cfg(unix)]
#[test]
fn peer() {
    let log = Arc::new(Kept::default());
    let config = Config {
        listen: "127.0.0.1:0".into(),
        hba: Some("local all all peer\n".into()),
        log: Some(log.clone()),
        ..Config::default()
    };
    let server = start(&config).unwrap();
    let lines =
        login(&server.sockets()[0], &[("user", "nobody-here"), ("database", "postgres")], "");
    assert_eq!(lines, ["FATAL 28000 Peer authentication failed for user \"nobody-here\""]);
    server.stop().unwrap();
    let lines = log.take();
    assert!(
        lines[0]
            .starts_with("LOG:  provided user name (nobody-here) and authenticated user name ("),
        "{lines:?}"
    );
    assert!(
        lines[1]
            .ends_with("Connection matched file \"pg_hba.conf\" line 1: \"local all all peer\"")
    );
}

#[test]
fn the_log_of_refused_roles() {
    let (server, log) = password_server("host all all 127.0.0.1/32 trust\n", &[]);
    let lines = login(server.address(), &[("user", "bob"), ("database", "postgres")], "");
    assert_eq!(lines, ["ok", "FATAL 28000 role \"bob\" does not exist"]);
    server.stop().unwrap();
    assert_eq!(log.take(), ["FATAL:  role \"bob\" does not exist"]);
}

/// Sends a `CancelRequest` on a new connection and waits until the server closes it.
fn cancel(server: &Server, pid: i32, key: &[u8]) {
    let mut packet = u32::try_from(12 + key.len()).unwrap().to_be_bytes().to_vec();
    packet.extend(CANCEL_REQUEST_CODE.to_be_bytes());
    packet.extend(pid.to_be_bytes());
    packet.extend(key);
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&packet).unwrap();
    let mut buf = [0; 16];
    assert_eq!(stream.read(&mut buf).unwrap(), 0);
}

#[test]
fn cancel_requests() {
    let (server, log) = password_server("host all all 127.0.0.1/32 trust\n", &[]);
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&startup(&[("user", "postgres"), ("database", "postgres")])).unwrap();
    let mut input = Vec::new();
    let (pid, key) = loop {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0);
        input.extend_from_slice(&buf[..n]);
        let mut at = 0;
        let mut found = None;
        while let Some((message, size)) = Backend::decode(&input[at..]).unwrap() {
            if let Backend::BackendKeyData { pid, key } = message {
                found = Some((pid, key.to_vec()));
            }
            at += size;
            if matches!(message, Backend::ReadyForQuery(_)) {
                break;
            }
        }
        if let Some(found) = found {
            break found;
        }
    };
    // A good key while the session waits for a query cancels nothing, and the server logs nothing.
    cancel(&server, pid, &key);
    let mut wrong = key.clone();
    wrong[0] ^= 1;
    cancel(&server, pid, &wrong);
    cancel(&server, 0, &key);
    cancel(&server, pid ^ 1, &key);
    stream.write_all(&query("SHOW work_mem")).unwrap();
    let lines = read(&mut *stream);
    assert!(
        lines.ends_with(&["row 4MB".to_owned(), "SHOW".to_owned(), "ready I".to_owned()]),
        "{lines:?}"
    );
    stream.write_all(&message(b'X', b"")).unwrap();
    assert!(read(&mut *stream).is_empty());
    server.stop().unwrap();
    assert_eq!(
        log.take(),
        [
            format!("LOG:  wrong key in cancel request for process {pid}"),
            "LOG:  invalid cancel request with PID 0".to_owned(),
            format!("LOG:  PID {} in cancel request did not match any process", pid ^ 1),
        ]
    );
}

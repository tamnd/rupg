//! The client parts that the examples `idle` and `connrate` share: the start of `rupg serve`, the connection and the startup of a session with trust or SCRAM, and the processes of a server in `/proc`.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rupg_wire::{Authentication, Backend, Crypto, Hashes, PROTOCOL_3_0};
use rustls::client::Resumption;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// The password of the superuser for SCRAM.
pub(crate) const PASSWORD: &str = "secret";
/// An `SSLRequest` packet.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];
/// The first message of the client in SCRAM, without the header of the channel binding.
const FIRST_BARE: &str = "n=,r=rOprNGfwEbeRWgbNEkqO";

/// A session that a test keeps open.
pub(crate) trait Session: Read + Write {}

impl<T: Read + Write> Session for T {}

/// How a client reaches a server.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Way<'a> {
    /// A Unix socket, by the path of the socket file.
    Unix(&'a str),
    /// TCP with no TLS, by `host:port`.
    Tcp(&'a str),
    /// TCP with an `SSLRequest` and TLS, by `host:port`.
    Tls(&'a str),
}

/// The path of a file in `tests/tls`.
fn tls_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls").join(name)
}

/// The process `root` and its children.
pub(crate) fn tree(root: u32) -> Vec<u32> {
    let mut pids = vec![root];
    let Ok(entries) = fs::read_dir("/proc") else { return pids };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        if stat_fields(&stat).get(1).and_then(|p| p.parse::<u32>().ok()) == Some(root) {
            pids.push(pid);
        }
    }
    pids
}

/// The fields of `/proc/<pid>/stat` after the name of the command, from the state on. The name is in parentheses and can have spaces, so the fields start after the last parenthesis.
pub(crate) fn stat_fields(stat: &str) -> Vec<&str> {
    stat.rsplit_once(')').map(|(_, rest)| rest.split_whitespace().collect()).unwrap_or_default()
}

pub(crate) fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// A `StartupMessage` for the user `user` and the database `postgres`.
fn startup(user: &str) -> Vec<u8> {
    let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
    body.extend_from_slice(format!("user\0{user}\0database\0postgres\0\0").as_bytes());
    let mut packet = u32::try_from(body.len() + 4).unwrap_or(u32::MAX).to_be_bytes().to_vec();
    packet.extend(body);
    packet
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[usize::try_from(n >> (18 - 6 * i) & 63).unwrap_or(0)]));
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
        .filter_map(|b| ABC.iter().position(|c| *c == b))
        .filter_map(|d| u32::try_from(d).ok())
        .collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, d)| n | d << (18 - 6 * i));
        for i in 0..chunk.len() - 1 {
            out.extend_from_slice(&(n >> (16 - 8 * i)).to_be_bytes()[3..]);
        }
    }
    out
}

/// The salt, the iteration count and the `ClientKey` of each salt that the client saw.
type Keys = Vec<(Vec<u8>, u32, [u8; 32])>;

/// The keys that the client already made. A server gives the same salt and iteration count each time for a role, so the client does the PBKDF2 of the password only once. A test then measures the server and not the client.
static KEYS: Mutex<Keys> = Mutex::new(Vec::new());

/// The `ClientKey` of [`PASSWORD`] for a salt and an iteration count.
fn client_key(salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut keys = KEYS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, _, key)) = keys.iter().find(|(s, i, _)| s == salt && *i == iterations) {
        return *key;
    }
    let salted = Hashes.pbkdf2_sha256(PASSWORD.as_bytes(), salt, iterations);
    let key = Hashes.hmac_sha256(&salted, &[b"Client Key"]);
    keys.push((salt.to_vec(), iterations, key));
    key
}

/// The client side of SCRAM-SHA-256 with no channel binding, RFC 5802: the last message of the client.
fn scram_final(server_first: &str) -> Result<String, String> {
    let field = |name: &str| {
        server_first
            .split(',')
            .find_map(|part| part.strip_prefix(name))
            .map(str::to_owned)
            .ok_or_else(|| format!("no {name} in the first message of the server"))
    };
    let nonce = field("r=")?;
    let salt = unbase64(&field("s=")?);
    let iterations: u32 = field("i=")?.parse().map_err(|_| "a bad iteration count")?;
    let client_key = client_key(&salt, iterations);
    let stored_key = Hashes.sha256(&[&client_key]);
    let without_proof = format!("c={},r={nonce}", base64(b"n,,"));
    let auth_message = format!("{FIRST_BARE},{server_first},{without_proof}");
    let signature = Hashes.hmac_sha256(&stored_key, &[auth_message.as_bytes()]);
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    Ok(format!("{without_proof},p={}", base64(&proof)))
}

/// Sends `packet` and reads the answers up to `ReadyForQuery`. The function answers a request for SCRAM with the password [`PASSWORD`].
pub(crate) fn talk(stream: &mut dyn Session, packet: &[u8]) -> Result<(), String> {
    let failed = |e: std::io::Error| e.to_string();
    stream.write_all(packet).and_then(|()| stream.flush()).map_err(failed)?;
    let mut input = Vec::new();
    loop {
        while let Some((m, size)) = Backend::decode(&input).map_err(|e| e.to_string())? {
            let answer = match m {
                Backend::Authentication(Authentication::Sasl(_)) => {
                    let first = format!("n,,{FIRST_BARE}");
                    let mut body = b"SCRAM-SHA-256\0".to_vec();
                    body.extend_from_slice(&u32::try_from(first.len()).unwrap_or(0).to_be_bytes());
                    body.extend_from_slice(first.as_bytes());
                    Some(message(b'p', &body))
                }
                Backend::Authentication(Authentication::SaslContinue(data)) => {
                    let last = scram_final(&String::from_utf8_lossy(data))?;
                    Some(message(b'p', last.as_bytes()))
                }
                Backend::ErrorResponse(fields) => {
                    let text: Vec<String> = fields
                        .iter()
                        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
                        .collect();
                    return Err(text.join(" "));
                }
                Backend::ReadyForQuery(_) => return Ok(()),
                _ => None,
            };
            input.drain(..size);
            if let Some(answer) = answer {
                stream.write_all(&answer).and_then(|()| stream.flush()).map_err(failed)?;
            }
        }
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).map_err(failed)?;
        if n == 0 {
            return Err("the server closed the connection".to_owned());
        }
        input.extend_from_slice(&buf[..n]);
    }
}

/// A client that trusts `ca.crt` of `tests/tls`. It does not resume a TLS session, so each connection has a full handshake, as with libpq and PostgreSQL.
pub(crate) fn tls_client() -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let ca = CertificateDer::from_pem_file(tls_file("ca.crt")).map_err(|e| e.to_string())?;
    roots.add(ca).map_err(|e| e.to_string())?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.resumption = Resumption::disabled();
    Ok(Arc::new(config))
}

/// A TCP connection to `address` with `TCP_NODELAY`, as libpq makes it.
fn tcp(address: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect(address).map_err(|e| format!("could not connect: {e}"))?;
    stream.set_nodelay(true).map_err(|e| format!("could not set TCP_NODELAY: {e}"))?;
    Ok(stream)
}

/// Connects to a server as `user`, and runs the startup up to the first `ReadyForQuery`.
pub(crate) fn connect(
    way: Way<'_>,
    tls: &Arc<ClientConfig>,
    user: &str,
) -> Result<Box<dyn Session>, String> {
    let failed = |e: std::io::Error| format!("could not connect: {e}");
    let mut session: Box<dyn Session> = match way {
        Way::Unix(path) => Box::new(UnixStream::connect(path).map_err(failed)?),
        Way::Tcp(address) => Box::new(tcp(address)?),
        Way::Tls(address) => {
            let mut stream = tcp(address)?;
            stream.write_all(&SSL_REQUEST).map_err(failed)?;
            let mut answer = [0];
            stream.read_exact(&mut answer).map_err(failed)?;
            if answer != *b"S" {
                return Err("the server does not have TLS".to_owned());
            }
            let name = ServerName::try_from("localhost").map_err(|e| e.to_string())?;
            let connection = ClientConnection::new(tls.clone(), name).map_err(|e| e.to_string())?;
            Box::new(StreamOwned::new(connection, stream))
        }
    };
    talk(&mut *session, &startup(user))?;
    Ok(session)
}

/// A running `rupg serve` and the addresses that it listens on.
pub(crate) struct Rupg {
    pub(crate) child: Child,
    pub(crate) tcp: String,
    pub(crate) socket: String,
}

impl Drop for Rupg {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `<rupg> serve` with the host rules `hba`, the superuser password [`PASSWORD`] and the certificate of `tests/tls`, with its files in `dir`. Then it waits for the lines of the log that give the addresses.
pub(crate) fn serve(rupg: &Path, dir: &Path, hba: &str) -> Result<Rupg, String> {
    let write = |name: &str, text: &[u8]| {
        fs::write(dir.join(name), text).map_err(|e| format!("could not write {name}: {e}"))
    };
    write("pg_hba.conf", hba.as_bytes())?;
    write("password", PASSWORD.as_bytes())?;
    for name in ["server.crt", "server.key"] {
        let text = fs::read(tls_file(name)).map_err(|e| format!("could not read {name}: {e}"))?;
        write(name, &text)?;
    }
    // Git does not keep the mode of the key, and the server wants 0600.
    fs::set_permissions(dir.join("server.key"), fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("could not change the mode of server.key: {e}"))?;
    let path = |name: &str| dir.join(name).display().to_string();
    let log = dir.join("server.log");
    let file = fs::File::create(&log).map_err(|e| format!("could not make the log: {e}"))?;
    let child = Command::new(rupg)
        .arg("serve")
        .args(["--listen", "127.0.0.1:0", "--pwfile", &path("password")])
        .args(["-c".to_owned(), format!("hba_file={}", path("pg_hba.conf"))])
        .args(["-c".to_owned(), format!("unix_socket_directories={}", dir.display())])
        .args(["-c", "ssl=on", "-c"])
        .arg(format!("ssl_cert_file={}", path("server.crt")))
        .args(["-c".to_owned(), format!("ssl_key_file={}", path("server.key"))])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(file)
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", rupg.display()))?;
    let mut server = Rupg { child, tcp: String::new(), socket: String::new() };
    let start = Instant::now();
    while server.tcp.is_empty() || server.socket.is_empty() {
        if start.elapsed() > Duration::from_secs(60) {
            return Err("the server did not start in 60 seconds".to_owned());
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            let text = fs::read_to_string(&log).unwrap_or_default();
            return Err(format!("the server ended with {status}: {text}"));
        }
        thread::sleep(Duration::from_millis(50));
        let text = fs::read_to_string(&log).unwrap_or_default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("rupg serve: listening on Unix socket ") {
                server.socket = rest.trim_matches('"').to_owned();
            } else if let Some(rest) = line.strip_prefix("rupg serve: listening on ") {
                server.tcp = rest.to_owned();
            }
        }
    }
    Ok(server)
}

/// A running PostgreSQL server, from `postmaster.pid` in its data directory.
#[derive(Debug)]
pub(crate) struct Postmaster {
    pub(crate) pid: u32,
    /// The path of the Unix socket.
    pub(crate) socket: String,
    /// `127.0.0.1:<port>`.
    pub(crate) tcp: String,
}

/// Reads `postmaster.pid` of the data directory `data`: the pid on line 1, the port on line 4 and the socket directory on line 5.
pub(crate) fn postmaster(data: &Path) -> Result<Postmaster, String> {
    let text = fs::read_to_string(data.join("postmaster.pid"))
        .map_err(|e| format!("could not read postmaster.pid: {e}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let pid = lines.first().and_then(|l| l.trim().parse().ok());
    let port = lines.get(3).map(|l| l.trim());
    let dir = lines.get(4).map(|l| l.trim()).filter(|l| !l.is_empty());
    match (pid, port, dir) {
        (Some(pid), Some(port), Some(dir)) => Ok(Postmaster {
            pid,
            socket: format!("{dir}/.s.PGSQL.{port}"),
            tcp: format!("127.0.0.1:{port}"),
        }),
        _ => Err("postmaster.pid has no pid, port or socket directory".to_owned()),
    }
}

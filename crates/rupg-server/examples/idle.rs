//! The idle session test of spec/06 section 6.4.3 and spec/21 section 21.13. `cargo xtask idle` builds and runs this program.
//!
//! Usage: `idle <rupg> <dir> [--sessions N] [--postgres <data dir>] [--postgres-sessions N]`. The program starts `<rupg> serve` once for each variant, with the files of the server in `<dir>`. It opens and closes one session first, so that the work that the server does once is not in the number. Then it opens the sessions of the variant, keeps them idle, and takes the increase of the proportional set size of the server process (`Pss` of `/proc/<pid>/smaps_rollup`). The server runs in its own process, so the memory of the clients is not in the number. The variants are trust over a Unix socket, trust over TCP, trust over TLS, SCRAM over TLS with `DISCARD ALL` after the startup, and trust over a Unix socket with a statement that the session prepares and drops after the startup, as spec/21 section 21.13 repeats the test. The program prints the size of one session for each variant and fails when the largest is over 64 KiB.
//!
//! `--postgres` measures a PostgreSQL server that runs with the data directory `<data dir>`, as the user `postgres` with trust over its Unix socket. The number is the increase of the postmaster and all its children. The server must allow the sessions in `max_connections`.
//!
//! The program reads `/proc`, so it runs on Linux only.

// The program measures another process and talks to it as a client. It is not a part of the engine, so it uses std and not the traits of rupg-platform.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rupg_wire::{Authentication, Backend, Crypto, Hashes, PROTOCOL_3_0};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// The limit of spec/06 section 6.4.1, in KiB.
const LIMIT: f64 = 64.0;
/// The password of the superuser for SCRAM.
const PASSWORD: &str = "secret";
/// An `SSLRequest` packet.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];
/// The time that the server gets to settle before each measurement. It is longer than the 1 second after which an idle session of rupg gives back the memory of its last statement.
const SETTLE: Duration = Duration::from_secs(3);

/// The ways to connect that the test measures.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    Unix,
    Tcp,
    Tls,
    Scram,
    Prepared,
}

impl Variant {
    const ALL: [Variant; 5] =
        [Variant::Unix, Variant::Tcp, Variant::Tls, Variant::Scram, Variant::Prepared];

    fn name(self) -> &'static str {
        match self {
            Variant::Unix => "trust, Unix socket",
            Variant::Tcp => "trust, TCP",
            Variant::Tls => "trust, TLS",
            Variant::Scram => "SCRAM, TLS, DISCARD ALL",
            Variant::Prepared => "trust, Unix, PREPARE, DEALLOCATE",
        }
    }

    fn hba(self) -> &'static str {
        match self {
            Variant::Unix | Variant::Prepared => "local all all trust\n",
            Variant::Tcp => "host all all 127.0.0.1/32 trust\n",
            Variant::Tls => "hostssl all all 127.0.0.1/32 trust\n",
            Variant::Scram => "hostssl all all 127.0.0.1/32 scram-sha-256\n",
        }
    }
}

/// A session that the test keeps open.
trait Session: Read + Write {}

impl<T: Read + Write> Session for T {}

/// The options of the command line.
#[derive(Debug)]
struct Options {
    rupg: PathBuf,
    dir: PathBuf,
    sessions: usize,
    postgres: Option<PathBuf>,
    postgres_sessions: usize,
}

fn options() -> Result<Options, String> {
    const USAGE: &str =
        "usage: idle <rupg> <dir> [--sessions N] [--postgres <data dir>] [--postgres-sessions N]";
    let mut args = std::env::args().skip(1);
    let rupg = args.next().ok_or(USAGE)?.into();
    let dir = args.next().ok_or(USAGE)?.into();
    let mut options = Options { rupg, dir, sessions: 1000, postgres: None, postgres_sessions: 90 };
    while let Some(arg) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let count = || value.parse::<usize>().ok().filter(|n| *n > 0).ok_or(USAGE);
        match arg.as_str() {
            "--sessions" => options.sessions = count()?,
            "--postgres" => options.postgres = Some(value.clone().into()),
            "--postgres-sessions" => options.postgres_sessions = count()?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

/// The path of a file in `tests/tls`.
fn tls_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls").join(name)
}

/// The sum of `Pss` of the processes `pids`, in KiB. A process that ended counts as 0.
fn pss(pids: &[u32]) -> u64 {
    let mut total = 0;
    for pid in pids {
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) else { continue };
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Pss:") {
                total += rest.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    total
}

/// The process `root` and its children.
fn tree(root: u32) -> Vec<u32> {
    let mut pids = vec![root];
    let Ok(entries) = fs::read_dir("/proc") else { return pids };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        // The name of the command is in parentheses and can have spaces, so the fields start after the last parenthesis.
        let parent = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1));
        if parent.and_then(|p| p.parse::<u32>().ok()) == Some(root) {
            pids.push(pid);
        }
    }
    pids
}

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
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

/// The client side of SCRAM-SHA-256 with no channel binding, RFC 5802: the last message of the client.
fn scram_final(first_bare: &str, server_first: &str) -> Result<String, String> {
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
    let salted = Hashes.pbkdf2_sha256(PASSWORD.as_bytes(), &salt, iterations);
    let client_key = Hashes.hmac_sha256(&salted, &[b"Client Key"]);
    let stored_key = Hashes.sha256(&[&client_key]);
    let without_proof = format!("c={},r={nonce}", base64(b"n,,"));
    let auth_message = format!("{first_bare},{server_first},{without_proof}");
    let signature = Hashes.hmac_sha256(&stored_key, &[auth_message.as_bytes()]);
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    Ok(format!("{without_proof},p={}", base64(&proof)))
}

/// Sends `packet` and reads the answers up to `ReadyForQuery`. The function answers a request for SCRAM with the password [`PASSWORD`].
fn talk(stream: &mut dyn Session, packet: &[u8]) -> Result<(), String> {
    const FIRST_BARE: &str = "n=,r=rOprNGfwEbeRWgbNEkqO";
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
                    let last = scram_final(FIRST_BARE, &String::from_utf8_lossy(data))?;
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

/// A client that trusts `ca.crt` of `tests/tls`.
fn tls_client() -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let ca = CertificateDer::from_pem_file(tls_file("ca.crt")).map_err(|e| e.to_string())?;
    roots.add(ca).map_err(|e| e.to_string())?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A running `rupg serve` and the addresses that it listens on.
struct Rupg {
    child: Child,
    tcp: String,
    socket: String,
}

impl Drop for Rupg {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `rupg serve` with the host rules of `variant`, and waits for the lines of its log that give the addresses.
fn serve(options: &Options, variant: Variant) -> Result<Rupg, String> {
    let dir = &options.dir;
    let write = |name: &str, text: &[u8]| {
        fs::write(dir.join(name), text).map_err(|e| format!("could not write {name}: {e}"))
    };
    write("pg_hba.conf", variant.hba().as_bytes())?;
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
    let child = Command::new(&options.rupg)
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
        .map_err(|e| format!("could not run {}: {e}", options.rupg.display()))?;
    let mut rupg = Rupg { child, tcp: String::new(), socket: String::new() };
    let start = Instant::now();
    while rupg.tcp.is_empty() || rupg.socket.is_empty() {
        if start.elapsed() > Duration::from_secs(60) {
            return Err("the server did not start in 60 seconds".to_owned());
        }
        if let Ok(Some(status)) = rupg.child.try_wait() {
            let text = fs::read_to_string(&log).unwrap_or_default();
            return Err(format!("the server ended with {status}: {text}"));
        }
        thread::sleep(Duration::from_millis(50));
        let text = fs::read_to_string(&log).unwrap_or_default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("rupg serve: listening on Unix socket ") {
                rupg.socket = rest.trim_matches('"').to_owned();
            } else if let Some(rest) = line.strip_prefix("rupg serve: listening on ") {
                rupg.tcp = rest.to_owned();
            }
        }
    }
    Ok(rupg)
}

/// Opens one session of `variant` and runs its startup.
fn open(
    variant: Variant,
    socket: &str,
    tcp: &str,
    tls: &Arc<ClientConfig>,
    user: &str,
) -> Result<Box<dyn Session>, String> {
    let failed = |e: std::io::Error| format!("could not connect: {e}");
    let mut session: Box<dyn Session> = match variant {
        Variant::Unix | Variant::Prepared => Box::new(UnixStream::connect(socket).map_err(failed)?),
        Variant::Tcp => Box::new(TcpStream::connect(tcp).map_err(failed)?),
        Variant::Tls | Variant::Scram => {
            let mut stream = TcpStream::connect(tcp).map_err(failed)?;
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
    match variant {
        Variant::Scram => talk(&mut *session, &message(b'Q', b"DISCARD ALL\0"))?,
        Variant::Prepared => {
            talk(&mut *session, &message(b'Q', b"PREPARE s AS SELECT relname FROM pg_class\0"))?;
            talk(&mut *session, &message(b'Q', b"DEALLOCATE s\0"))?;
        }
        Variant::Unix | Variant::Tcp | Variant::Tls => {}
    }
    Ok(session)
}

/// Opens `count` sessions and gives the increase of the memory of the processes of `root`, in KiB for each session. One session opens and closes first, so the work that the server does once, such as the load of the shared catalog at the first statement, is in the memory before.
fn measure(
    count: usize,
    root: u32,
    mut open: impl FnMut() -> Result<Box<dyn Session>, String>,
) -> Result<f64, String> {
    drop(open()?);
    thread::sleep(SETTLE);
    let before = pss(&tree(root));
    let sessions = (0..count)
        .map(|i| open().map_err(|e| format!("session {}: {e}", i + 1)))
        .collect::<Result<Vec<_>, String>>()?;
    thread::sleep(SETTLE);
    let after = pss(&tree(root));
    drop(sessions);
    let increase = after.saturating_sub(before);
    Ok(f64::from(u32::try_from(increase).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(count).unwrap_or(u32::MAX)))
}

/// The pid, the port and the socket directory of a running PostgreSQL, from `postmaster.pid`.
fn postmaster(data: &Path) -> Result<(u32, String), String> {
    let text = fs::read_to_string(data.join("postmaster.pid"))
        .map_err(|e| format!("could not read postmaster.pid: {e}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let pid = lines.first().and_then(|l| l.trim().parse().ok());
    let port = lines.get(3).map(|l| l.trim());
    let dir = lines.get(4).map(|l| l.trim()).filter(|l| !l.is_empty());
    match (pid, port, dir) {
        (Some(pid), Some(port), Some(dir)) => Ok((pid, format!("{dir}/.s.PGSQL.{port}"))),
        _ => Err("postmaster.pid has no pid, port or socket directory".to_owned()),
    }
}

fn run(options: &Options) -> Result<bool, String> {
    let tls = tls_client()?;
    let mut largest: f64 = 0.0;
    for variant in Variant::ALL {
        let server = serve(options, variant)?;
        let root = server.child.id();
        let size = measure(options.sessions, root, || {
            open(variant, &server.socket, &server.tcp, &tls, "postgres")
        })?;
        println!(
            "rupg       {:<32} {size:>8.1} KiB for each of {} sessions",
            variant.name(),
            options.sessions
        );
        largest = largest.max(size);
    }
    if let Some(data) = &options.postgres {
        let (root, socket) = postmaster(data)?;
        let count = options.postgres_sessions;
        let size = measure(count, root, || open(Variant::Unix, &socket, "", &tls, "postgres"))?;
        println!(
            "postgres   {:<32} {size:>8.1} KiB for each of {count} sessions",
            Variant::Unix.name()
        );
    }
    let pass = largest <= LIMIT;
    println!(
        "idle: the largest is {largest:.1} KiB, the limit is {LIMIT:.0} KiB: {}",
        if pass { "pass" } else { "FAIL" }
    );
    Ok(pass)
}

fn main() -> ExitCode {
    let result = options().and_then(|options| {
        fs::create_dir_all(&options.dir)
            .map_err(|e| format!("could not make {}: {e}", options.dir.display()))?;
        run(&options)
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("idle: {message}");
            ExitCode::FAILURE
        }
    }
}

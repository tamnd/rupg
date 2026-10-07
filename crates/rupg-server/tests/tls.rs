//! Runs the server with TLS on a free port of the loopback address and talks to it with a rustls client. The certificates are in `tests/tls`: a root `ca.crt`, the server `localhost`, and the clients `postgres` and `alice`.
#![cfg(feature = "tls")]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use rupg_platform::os::{OsEntropy, OsIo, OsNet, OsTasks};
use rupg_platform::{File, FileMode, Io, Net, OpenMode, Stream};
use rupg_server::{Config, Log, Server};
use rupg_wire::{Authentication, Backend, Crypto, Hashes, PROTOCOL_3_0};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// An `SSLRequest` packet.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];

/// The key file of a server, as PostgreSQL wants it.
const KEY: FileMode = FileMode { regular: true, mine: true, root: false, mode: 0o600 };

/// A TLS connection of the client.
type Tls = StreamOwned<ClientConnection, Box<dyn Stream>>;

/// The path of a file in `tests/tls`.
fn file(name: &str) -> String {
    format!("{}/tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The file system of the system, with the owner and the mode of [`Modes::0`] for each file that exists. Git does not keep the mode `0600` of the keys, and a test cannot make a file of root.
#[derive(Debug)]
struct Modes(FileMode);

impl Io for Modes {
    fn open(&self, path: &Path, mode: OpenMode) -> rupg_common::Result<Arc<dyn File>> {
        OsIo.open(path, mode)
    }

    fn remove(&self, path: &Path) -> rupg_common::Result<()> {
        OsIo.remove(path)
    }

    fn exists(&self, path: &Path) -> rupg_common::Result<bool> {
        OsIo.exists(path)
    }

    fn sync_dir(&self, path: &Path) -> rupg_common::Result<()> {
        OsIo.sync_dir(path)
    }

    fn mode(&self, path: &Path) -> rupg_common::Result<FileMode> {
        OsIo.mode(path).map(|_| self.0)
    }
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

/// Starts a server with the password `secret` for `postgres`, the host rules `hba`, and TLS with the certificate `localhost` and the root `ca.crt`. Each item of `settings` replaces or adds a setting. The key file has the owner and the mode of `key`.
fn start(
    hba: &str,
    settings: &[(&str, &str)],
    key: FileMode,
) -> (rupg_common::Result<Server>, Arc<Kept>) {
    let mut all = vec![
        ("ssl".to_owned(), "on".to_owned()),
        ("ssl_cert_file".to_owned(), file("server.crt")),
        ("ssl_key_file".to_owned(), file("server.key")),
        ("ssl_ca_file".to_owned(), file("ca.crt")),
    ];
    for (name, value) in settings {
        all.retain(|(n, _)| n != name);
        all.push(((*name).to_owned(), (*value).to_owned()));
    }
    let log = Arc::new(Kept::default());
    let config = Config {
        listen: "127.0.0.1:0".into(),
        password: Some("secret".into()),
        hba: Some(hba.into()),
        settings: all,
        log: Some(log.clone()),
        ..Config::default()
    };
    let server = Server::start(
        &config,
        Arc::new(OsNet),
        Arc::new(Modes(key)),
        Arc::new(OsTasks),
        Arc::new(OsEntropy),
    );
    (server, log)
}

fn server(hba: &str) -> (Server, Arc<Kept>) {
    let (server, log) = start(hba, &[], KEY);
    (server.unwrap(), log)
}

/// The error of a server that does not start, as `state message`, and its detail.
fn start_error(settings: &[(&str, &str)], key: FileMode) -> (String, Option<String>) {
    let (server, _) = start("hostssl all all 127.0.0.1/32 trust\n", settings, key);
    let error = server.unwrap_err();
    (format!("{} {}", error.state().as_str(), error.message()), error.detail().map(str::to_owned))
}

/// A client that trusts `ca.crt`. `cert` names the certificate of the client in `tests/tls`, and `alpn` offers the protocol `postgresql`.
fn client(cert: Option<&str>, alpn: bool) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(file("ca.crt")).unwrap()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let mut config = match cert {
        None => builder.with_no_client_auth(),
        Some(name) => {
            let certs = vec![CertificateDer::from_pem_file(file(&format!("{name}.crt"))).unwrap()];
            let key = PrivateKeyDer::from_pem_file(file(&format!("{name}.key"))).unwrap();
            builder.with_client_auth_cert(certs, key).unwrap()
        }
    };
    if alpn {
        config.alpn_protocols = vec![b"postgresql".to_vec()];
    }
    Arc::new(config)
}

/// Starts TLS on `stream` as the client. The handshake runs at the first read or write.
fn tls(stream: Box<dyn Stream>, config: Arc<ClientConfig>) -> Tls {
    let name = ServerName::try_from("localhost").unwrap();
    StreamOwned::new(ClientConnection::new(config, name).unwrap(), stream)
}

/// Sends an `SSLRequest`, checks the answer `S`, and starts TLS.
fn ssl_request(server: &Server, config: Arc<ClientConfig>) -> Tls {
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&SSL_REQUEST).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"S");
    tls(stream, config)
}

/// Starts TLS with no request first, which is direct TLS.
fn direct(server: &Server, config: Arc<ClientConfig>) -> Tls {
    tls(OsNet.connect(server.address()).unwrap(), config)
}

/// A `StartupMessage` for the user `postgres` and the database `postgres`.
fn startup() -> Vec<u8> {
    let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
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

/// The client side of SCRAM-SHA-256 with the password `secret`, RFC 5802. `cbind` is the header of GS2 and the channel binding data after it.
fn scram_final(cbind: &[u8], first_bare: &str, server_first: &str) -> String {
    let field = |name: &str| {
        server_first.split(',').find_map(|part| part.strip_prefix(name)).unwrap().to_owned()
    };
    let nonce = field("r=");
    let salt = unbase64(&field("s="));
    let iterations: u32 = field("i=").parse().unwrap();
    let salted = Hashes.pbkdf2_sha256(b"secret", &salt, iterations);
    let client_key = Hashes.hmac_sha256(&salted, &[b"Client Key"]);
    let stored_key = Hashes.sha256(&[&client_key]);
    let without_proof = format!("c={},r={nonce}", base64(cbind));
    let auth_message = format!("{first_bare},{server_first},{without_proof}");
    let signature = Hashes.hmac_sha256(&stored_key, &[auth_message.as_bytes()]);
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    format!("{without_proof},p={}", base64(&proof))
}

/// Sends `packet`, answers each request for a password, and gives one line for each message until `ReadyForQuery` or the end of the stream. `ParameterStatus`, `BackendKeyData` and `RowDescription` give no line. SCRAM uses `SCRAM-SHA-256-PLUS` with the data `binding` when it is given.
fn talk(stream: &mut (impl Read + Write), packet: &[u8], binding: Option<&[u8]>) -> Vec<String> {
    const FIRST_BARE: &str = "n=,r=rOprNGfwEbeRWgbNEkqO";
    let header: &[u8] = if binding.is_some() { b"p=tls-server-end-point,," } else { b"n,," };
    let mut lines = Vec::new();
    if stream.write_all(packet).and_then(|()| stream.flush()).is_err() {
        return lines;
    }
    let mut input = Vec::new();
    loop {
        while let Some((m, size)) = Backend::decode(&input).unwrap() {
            let mut answer = None;
            let line = match m {
                Backend::Authentication(Authentication::Ok) => Some("ok".to_owned()),
                Backend::Authentication(Authentication::Sasl(mechanisms)) => {
                    let first = format!("{}{FIRST_BARE}", text(header));
                    let name: &[u8] = if binding.is_some() {
                        b"SCRAM-SHA-256-PLUS\0"
                    } else {
                        b"SCRAM-SHA-256\0"
                    };
                    let mut body = name.to_vec();
                    body.extend_from_slice(&i32::try_from(first.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(first.as_bytes());
                    answer = Some(message(b'p', &body));
                    let names: Vec<String> = mechanisms.iter().map(|m| text(m)).collect();
                    Some(format!("sasl {}", names.join(",")))
                }
                Backend::Authentication(Authentication::SaslContinue(data)) => {
                    let mut cbind = header.to_vec();
                    cbind.extend_from_slice(binding.unwrap_or_default());
                    let last = scram_final(&cbind, FIRST_BARE, &text(data));
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
                Backend::DataRow(values) => {
                    let values: Vec<String> = values.iter().map(|v| text(v.unwrap())).collect();
                    Some(format!("row {}", values.join(",")))
                }
                Backend::CommandComplete(tag) => Some(text(tag)),
                Backend::ReadyForQuery(status) => {
                    lines.push(format!("ready {}", char::from(status.byte())));
                    return lines;
                }
                Backend::ParameterStatus { .. }
                | Backend::BackendKeyData { .. }
                | Backend::RowDescription(_) => None,
                other => Some(format!("{other:?}")),
            };
            input.drain(..size);
            lines.extend(line);
            if let Some(answer) = answer {
                stream.write_all(&answer).unwrap();
                stream.flush().unwrap();
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

#[test]
fn an_ssl_request() {
    let (server, log) =
        server("hostssl all all 127.0.0.1/32 trust\nhostnossl all all 127.0.0.1/32 reject\n");
    let mut stream = ssl_request(&server, client(None, false));
    assert_eq!(talk(&mut stream, &startup(), None), ["ok", "ready I"]);
    assert_eq!(talk(&mut stream, &query("SHOW ssl"), None), ["row on", "SHOW", "ready I"]);
    assert_eq!(stream.conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
    drop(stream);
    let mut plain = OsNet.connect(server.address()).unwrap();
    let rejected = "FATAL 28000 pg_hba.conf rejects connection for host \"127.0.0.1\", user \"postgres\", database \"postgres\", no encryption";
    assert_eq!(talk(&mut plain, &startup(), None), [rejected]);
    // The Unix socket has no TLS.
    #[cfg(unix)]
    {
        let mut local = OsNet.connect(&server.sockets()[0]).unwrap();
        local.write_all(&SSL_REQUEST).unwrap();
        let mut answer = [0];
        local.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"N");
    }
    server.stop().unwrap();
    assert_eq!(log.take(), [rejected.replacen(" 28000 ", ":  ", 1)]);
}

#[test]
fn direct_tls() {
    let (server, log) = server("hostssl all all 127.0.0.1/32 trust\n");
    let mut stream = direct(&server, client(None, true));
    assert_eq!(talk(&mut stream, &startup(), None), ["ok", "ready I"]);
    assert_eq!(stream.conn.alpn_protocol(), Some(&b"postgresql"[..]));
    // After direct TLS, an `SSLRequest` gets `N`.
    drop(stream);
    let mut stream = direct(&server, client(None, true));
    stream.write_all(&SSL_REQUEST).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    assert_eq!(talk(&mut stream, &startup(), None), ["ok", "ready I"]);
    // Direct TLS needs ALPN.
    drop(stream);
    let mut stream = direct(&server, client(None, false));
    assert_eq!(talk(&mut stream, &startup(), None), Vec::<String>::new());
    drop(stream);
    server.stop().unwrap();
    assert_eq!(
        log.take(),
        [
            "LOG:  received direct SSL connection request without ALPN protocol negotiation extension"
        ]
    );
}

#[test]
fn a_server_without_ssl() {
    let (server, log) = start("host all all 127.0.0.1/32 trust\n", &[("ssl", "off")], KEY);
    let server = server.unwrap();
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&SSL_REQUEST).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    // Direct TLS ends with no message.
    drop(stream);
    let mut stream = direct(&server, client(None, true));
    assert_eq!(talk(&mut stream, &startup(), None), Vec::<String>::new());
    drop(stream);
    server.stop().unwrap();
    assert_eq!(log.take(), Vec::<String>::new());
}

#[test]
fn data_after_an_ssl_request() {
    let (server, _) = server("hostssl all all 127.0.0.1/32 trust\n");
    let mut stream = OsNet.connect(server.address()).unwrap();
    let mut packets = SSL_REQUEST.to_vec();
    packets.extend(startup());
    stream.write_all(&packets).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"S");
    let mut stream = tls(stream, client(None, false));
    assert_eq!(
        talk(&mut stream, &[], None),
        ["FATAL 08P01 received unencrypted data after SSL request"]
    );
    drop(stream);
    server.stop().unwrap();
}

#[test]
fn scram_with_channel_binding() {
    let (server, log) = server("hostssl all all 127.0.0.1/32 scram-sha-256\n");
    let cert = CertificateDer::from_pem_file(file("server.crt")).unwrap();
    let hash = Hashes.sha256(&[&cert]);
    let mut stream = ssl_request(&server, client(None, false));
    assert_eq!(
        talk(&mut stream, &startup(), Some(&hash)),
        ["sasl SCRAM-SHA-256-PLUS,SCRAM-SHA-256", "sasl continue", "sasl final", "ok", "ready I"]
    );
    drop(stream);
    let mut stream = direct(&server, client(None, true));
    assert_eq!(
        talk(&mut stream, &startup(), None),
        ["sasl SCRAM-SHA-256-PLUS,SCRAM-SHA-256", "sasl continue", "sasl final", "ok", "ready I"]
    );
    drop(stream);
    let mut stream = ssl_request(&server, client(None, false));
    assert_eq!(
        talk(&mut stream, &startup(), Some(&[0; 32])),
        [
            "sasl SCRAM-SHA-256-PLUS,SCRAM-SHA-256",
            "sasl continue",
            "FATAL 28000 SCRAM channel binding check failed"
        ]
    );
    drop(stream);
    server.stop().unwrap();
    assert_eq!(log.take(), ["FATAL:  SCRAM channel binding check failed"]);
}

#[test]
fn the_cert_method() {
    let (server, log) = server("hostssl all all 127.0.0.1/32 cert\n");
    let mut stream = ssl_request(&server, client(Some("postgres"), false));
    assert_eq!(talk(&mut stream, &startup(), None), ["ok", "ready I"]);
    drop(stream);
    let mut stream = direct(&server, client(Some("alice"), true));
    assert_eq!(
        talk(&mut stream, &startup(), None),
        ["FATAL 28000 certificate authentication failed for user \"postgres\""]
    );
    drop(stream);
    let mut stream = ssl_request(&server, client(None, false));
    assert_eq!(
        talk(&mut stream, &startup(), None),
        ["FATAL 28000 connection requires a valid client certificate"]
    );
    drop(stream);
    server.stop().unwrap();
    assert_eq!(
        log.take(),
        [
            "LOG:  provided user name (postgres) and authenticated user name (alice) do not match",
            "FATAL:  certificate authentication failed for user \"postgres\"\nDETAIL:  Connection matched file \"pg_hba.conf\" line 1: \"hostssl all all 127.0.0.1/32 cert\"",
            "FATAL:  connection requires a valid client certificate",
        ]
    );
}

#[test]
fn clientcert_verify_full() {
    let line = "hostssl all all 127.0.0.1/32 scram-sha-256 clientcert=verify-full";
    let (server, log) = server(&format!("{line}\n"));
    let mut stream = ssl_request(&server, client(Some("postgres"), false));
    let lines = talk(&mut stream, &startup(), None);
    assert_eq!(lines.last().unwrap(), "ready I");
    drop(stream);
    let mut stream = ssl_request(&server, client(Some("alice"), false));
    assert_eq!(
        talk(&mut stream, &startup(), None),
        [
            "sasl SCRAM-SHA-256-PLUS,SCRAM-SHA-256",
            "sasl continue",
            "sasl final",
            "FATAL 28P01 password authentication failed for user \"postgres\""
        ]
    );
    drop(stream);
    server.stop().unwrap();
    assert_eq!(
        log.take(),
        [
            "LOG:  provided user name (postgres) and authenticated user name (alice) do not match".to_owned(),
            "LOG:  certificate validation (clientcert=verify-full) failed for user \"postgres\": CN mismatch".to_owned(),
            format!("FATAL:  password authentication failed for user \"postgres\"\nDETAIL:  Connection matched file \"pg_hba.conf\" line 1: \"{line}\""),
        ]
    );
}

#[test]
fn no_root_certificates() {
    let (server, log) = start("hostssl all all 127.0.0.1/32 cert\n", &[("ssl_ca_file", "")], KEY);
    let server = server.unwrap();
    let mut stream = ssl_request(&server, client(Some("postgres"), false));
    let message =
        "client certificates can only be checked if a root certificate store is available";
    assert_eq!(talk(&mut stream, &startup(), None), [format!("FATAL F0000 {message}")]);
    drop(stream);
    server.stop().unwrap();
    assert_eq!(log.take(), [format!("FATAL:  {message}")]);
}

#[test]
fn settings_that_do_not_load() {
    let key = file("server.key");
    let detail = "File must have permissions u=rw (0600) or less if owned by the database user, or permissions u=rw,g=r (0640) or less if owned by root.";
    let group = FileMode { mode: 0o640, ..KEY };
    assert_eq!(
        start_error(&[], group),
        (
            format!("F0000 private key file \"{key}\" has group or world access"),
            Some(detail.into())
        )
    );
    let root = FileMode { mine: false, root: true, mode: 0o640, ..KEY };
    let (server, _) = start("hostssl all all 127.0.0.1/32 trust\n", &[], root);
    server.unwrap().stop().unwrap();
    let other = FileMode { mine: false, ..KEY };
    assert_eq!(
        start_error(&[], other).0,
        format!("F0000 private key file \"{key}\" must be owned by the database user or root")
    );
    let directory = FileMode { regular: false, ..KEY };
    assert_eq!(
        start_error(&[], directory).0,
        format!("F0000 private key file \"{key}\" is not a regular file")
    );
    let missing = "/nonexistent/rupg/server.key";
    assert_eq!(
        start_error(&[("ssl_key_file", missing)], KEY).0,
        format!("F0000 could not access private key file \"{missing}\": No such file or directory")
    );
    let missing = "/nonexistent/rupg/server.crt";
    assert_eq!(
        start_error(&[("ssl_cert_file", missing)], KEY).0,
        format!(
            "F0000 could not load server certificate file \"{missing}\": No such file or directory"
        )
    );
    let ca = file("server.key");
    assert_eq!(
        start_error(&[("ssl_ca_file", &ca)], KEY).0,
        format!("F0000 could not load root certificate file \"{ca}\": no start line")
    );
    let other_key = file("alice.key");
    assert!(
        start_error(&[("ssl_key_file", &other_key)], KEY)
            .0
            .starts_with("F0000 check of private key failed: ")
    );
    assert_eq!(
        start_error(
            &[("ssl_min_protocol_version", "TLSv1.3"), ("ssl_max_protocol_version", "TLSv1.2")],
            KEY
        ),
        (
            "F0000 could not set SSL protocol version range".to_owned(),
            Some(
                "\"ssl_min_protocol_version\" cannot be higher than \"ssl_max_protocol_version\"."
                    .to_owned()
            )
        )
    );
    assert_eq!(
        start_error(&[("ssl_max_protocol_version", "TLSv1.1")], KEY).0,
        "F0000 \"ssl_max_protocol_version\" setting \"TLSv1.1\" not supported by this build"
    );
    assert_eq!(
        start_error(&[("ssl_sni", "on")], KEY).0,
        "0A000 rupg does not support parameter \"ssl_sni\" set to \"on\""
    );
}

#[test]
fn the_versions() {
    let (server, log) = start(
        "hostssl all all 127.0.0.1/32 trust\n",
        &[("ssl_max_protocol_version", "TLSv1.2")],
        KEY,
    );
    let server = server.unwrap();
    let mut stream = ssl_request(&server, client(None, false));
    assert_eq!(talk(&mut stream, &startup(), None), ["ok", "ready I"]);
    assert_eq!(stream.conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_2));
    drop(stream);
    server.stop().unwrap();
    assert_eq!(log.take(), Vec::<String>::new());
}

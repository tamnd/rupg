//! TLS with rustls and its `ring` provider: the configuration that the server loads at start, and the handshake of one connection (spec/06 sections 6.5 and 6.6).
//!
//! The server loads the certificate and the key at start, as `be_tls_init` of PostgreSQL does, and does not start when they do not load. A connection starts TLS in one of two ways: with an `SSLRequest` and the answer `S`, or with a TLS handshake as its first bytes, which is direct TLS. Direct TLS needs the ALPN protocol `postgresql`, as in PostgreSQL 17 and later.
//!
//! rustls has TLS 1.2 and 1.3 only, and it picks the cipher suites and the key exchange groups. So `ssl_ciphers`, `ssl_tls13_ciphers`, `ssl_groups`, `ssl_dh_params_file` and `ssl_crl_dir` are accepted and not used yet, and an `ssl_min_protocol_version` below `TLSv1.2` gives `TLSv1.2`.
//!
//! Lifted from `crates/rudb-server/src/tls.rs` of tamnd/rudb at f5f7065a.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ring::digest;
use rupg_common::{Error, Result, SqlState};
use rupg_platform::{Io, Stream};
use rupg_session::guc::Settings;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

use crate::Secured;
use crate::x509;

/// The ALPN protocol of PostgreSQL, `PG_ALPN_PROTOCOL` in `pqcomm.h`.
const ALPN: &[u8] = b"postgresql";

/// The text of OpenSSL for a PEM file with no item in it.
const NO_START_LINE: &str = "no start line";

/// The TLS configuration of the server.
#[derive(Debug)]
pub(crate) struct Tls {
    config: Arc<ServerConfig>,
    /// The hash of the certificate for the channel binding `tls-server-end-point`.
    hash: Vec<u8>,
    /// True when `ssl_ca_file` gave root certificates.
    ca: bool,
}

impl Tls {
    /// True when the server can check a client certificate, `secure_loaded_verify_locations`.
    pub(crate) fn ca(&self) -> bool {
        self.ca
    }
}

fn config_error(message: String) -> Error {
    Error::new(SqlState::CONFIG_FILE_ERROR, message)
}

/// The bytes of a file of the TLS settings. An error is the text of the system only, as `SSLerrmessage` gives it.
fn read(io: &dyn Io, file: &str) -> std::result::Result<Vec<u8>, String> {
    let path = Path::new(file);
    io.mode(path).map_err(|e| e.message().to_owned())?;
    io.read_file(path).map_err(|e| {
        let prefix = format!("could not open file \"{file}\": ");
        e.message().strip_prefix(&prefix).unwrap_or(e.message()).to_owned()
    })
}

/// The version of an `ssl_*_protocol_version` value: 1 for `TLSv1` to 4 for `TLSv1.3`, and 0 for the empty value, which is any version.
fn version(value: &str) -> u8 {
    match value {
        "TLSv1" => 1,
        "TLSv1.1" => 2,
        "TLSv1.2" => 3,
        "TLSv1.3" => 4,
        _ => 0,
    }
}

/// The TLS configuration, or `None` when `ssl` is off. The checks come in the order of `be_tls_init`.
///
/// # Errors
///
/// The text of PostgreSQL with `F0000` for a certificate or a key that does not load, a key that other users can read, a key that is not the key of the certificate, root certificates or a revocation list that do not load, and a version range that is empty.
pub(crate) fn load(settings: &Settings, io: &dyn Io) -> Result<Option<Tls>> {
    if settings.get("ssl").as_deref() != Some("on") {
        return Ok(None);
    }
    let get = |name: &str| settings.get(name).unwrap_or_default();
    let (min_text, max_text) = (get("ssl_min_protocol_version"), get("ssl_max_protocol_version"));
    let (min, max) = (version(&min_text), version(&max_text));
    if max != 0 && max < 3 {
        return Err(config_error(format!(
            "\"ssl_max_protocol_version\" setting \"{max_text}\" not supported by this build"
        )));
    }
    if max != 0 && min > max {
        return Err(config_error("could not set SSL protocol version range".to_owned())
            .with_detail(
                "\"ssl_min_protocol_version\" cannot be higher than \"ssl_max_protocol_version\".",
            ));
    }
    let cert_file = get("ssl_cert_file");
    let certs = read(io, &cert_file).and_then(|bytes| {
        let certs = CertificateDer::pem_slice_iter(&bytes)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if certs.is_empty() { Err(NO_START_LINE.to_owned()) } else { Ok(certs) }
    });
    let certs = certs.map_err(|text| {
        config_error(format!("could not load server certificate file \"{cert_file}\": {text}"))
    })?;
    let key_file = get("ssl_key_file");
    check_key_access(io, &key_file)?;
    let key = read(io, &key_file)
        .and_then(|bytes| PrivateKeyDer::from_pem_slice(&bytes).map_err(|e| e.to_string()))
        .map_err(|text| {
            config_error(format!("could not load private key file \"{key_file}\": {text}"))
        })?;
    let ca_file = get("ssl_ca_file");
    let roots = if ca_file.is_empty() { None } else { Some(roots(io, &ca_file)?) };
    let crl_file = get("ssl_crl_file");
    let crls = if crl_file.is_empty() { Vec::new() } else { crls(io, &crl_file)? };
    let versions: Vec<&'static rustls::SupportedProtocolVersion> =
        [(3, &rustls::version::TLS12), (4, &rustls::version::TLS13)]
            .into_iter()
            .filter(|(v, _)| *v >= min && (max == 0 || *v <= max))
            .map(|(_, version)| version)
            .collect();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&versions)
        .map_err(|e| config_error(format!("could not create SSL context: {e}")))?;
    let ca = roots.is_some();
    let builder = match roots {
        None => builder.with_no_client_auth(),
        Some(roots) => {
            // PostgreSQL asks for a client certificate when it has root certificates, and lets a client without one go on to the host rules.
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .with_crls(crls)
                .allow_unauthenticated()
                .build()
                .map_err(|e| {
                    config_error(format!("could not load root certificate file \"{ca_file}\": {e}"))
                })?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    let hash = certificate_hash(&certs[0]);
    let mut config = builder.with_single_cert(certs, key).map_err(|e| match e {
        rustls::Error::InconsistentKeys(_) => {
            config_error(format!("check of private key failed: {e}"))
        }
        _ => config_error(format!("could not load private key file \"{key_file}\": {e}")),
    })?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.ignore_client_order = get("ssl_prefer_server_ciphers") != "off";
    // PostgreSQL turns off session resumption, both the cache and the tickets.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Some(Tls { config: Arc::new(config), hash, ca }))
}

/// The root certificates of `ssl_ca_file`.
fn roots(io: &dyn Io, file: &str) -> Result<RootCertStore> {
    let failed = |text: String| {
        config_error(format!("could not load root certificate file \"{file}\": {text}"))
    };
    let bytes = read(io, file).map_err(failed)?;
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&bytes) {
        let cert = cert.map_err(|e| failed(e.to_string()))?;
        roots.add(cert).map_err(|e| failed(e.to_string()))?;
    }
    if roots.is_empty() {
        return Err(failed(NO_START_LINE.to_owned()));
    }
    Ok(roots)
}

/// The certificate revocation lists of `ssl_crl_file`.
fn crls(io: &dyn Io, file: &str) -> Result<Vec<CertificateRevocationListDer<'static>>> {
    let failed = |text: String| {
        config_error(format!(
            "could not load SSL certificate revocation list file \"{file}\": {text}"
        ))
    };
    let bytes = read(io, file).map_err(failed)?;
    let crls = CertificateRevocationListDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| failed(e.to_string()))?;
    if crls.is_empty() {
        return Err(failed(NO_START_LINE.to_owned()));
    }
    Ok(crls)
}

/// `check_ssl_key_file_permissions`: the owner of the key must be the user of the server or root, and the mode at most 0600, or 0640 for root.
fn check_key_access(io: &dyn Io, file: &str) -> Result<()> {
    let mode = io.mode(Path::new(file)).map_err(|e| {
        config_error(format!("could not access private key file \"{file}\": {}", e.message()))
    })?;
    if !mode.regular {
        return Err(config_error(format!("private key file \"{file}\" is not a regular file")));
    }
    if !mode.mine && !mode.root {
        return Err(config_error(format!(
            "private key file \"{file}\" must be owned by the database user or root"
        )));
    }
    if (mode.mine && mode.mode & 0o077 != 0) || (mode.root && mode.mode & 0o037 != 0) {
        return Err(config_error(format!("private key file \"{file}\" has group or world access")).with_detail("File must have permissions u=rw (0600) or less if owned by the database user, or permissions u=rw,g=r (0640) or less if owned by root."));
    }
    Ok(())
}

/// The hash of the certificate for `tls-server-end-point`, as `be_tls_get_certificate_hash` makes it: the hash of the signature algorithm of the certificate, with SHA-256 in place of MD5 and SHA-1 as RFC 5929 asks. Other algorithms get SHA-256 too.
fn certificate_hash(cert: &[u8]) -> Vec<u8> {
    // The OIDs of sha384WithRSAEncryption, sha512WithRSAEncryption, ecdsa-with-SHA384 and ecdsa-with-SHA512.
    const SHA384: [&[u8]; 2] = [
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03],
    ];
    const SHA512: [&[u8]; 2] = [
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04],
    ];
    let algorithm = signature_algorithm(cert).unwrap_or_default();
    let digest = if SHA384.contains(&algorithm) {
        &digest::SHA384
    } else if SHA512.contains(&algorithm) {
        &digest::SHA512
    } else {
        &digest::SHA256
    };
    digest::digest(digest, cert).as_ref().to_vec()
}

/// The OID of `signatureAlgorithm` in a certificate: the second item of the outer sequence.
fn signature_algorithm(cert: &[u8]) -> Option<&[u8]> {
    let (_, certificate, _) = der_item(cert)?;
    let (_, _, rest) = der_item(certificate)?;
    let (_, algorithm, _) = der_item(rest)?;
    let (tag, oid, _) = der_item(algorithm)?;
    (tag == 0x06).then_some(oid)
}

/// The first DER item of `input`: its tag, its content and the bytes after it.
fn der_item(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > size_of::<usize>() || rest.len() < count {
            return None;
        }
        let len = rest[..count].iter().fold(0usize, |len, &b| len << 8 | usize::from(b));
        (len, &rest[count..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// Runs the TLS handshake on `stream`, and puts the TLS connection in its place. `early` holds the bytes of the handshake that the server already read, for direct TLS. An error holds the text of the log line of PostgreSQL, if there is one, and then the connection ends.
pub(crate) fn accept(
    tls: &Tls,
    stream: &mut Box<dyn Stream>,
    mut early: &[u8],
    direct: bool,
) -> std::result::Result<Secured, Option<String>> {
    let failed = |e: &dyn std::fmt::Display| Some(format!("could not accept SSL connection: {e}"));
    let mut conn = ServerConnection::new(tls.config.clone()).map_err(|e| failed(&e))?;
    while !early.is_empty() {
        conn.read_tls(&mut early).map_err(|e| failed(&e))?;
        conn.process_new_packets().map_err(|e| failed(&e))?;
    }
    let socket = std::mem::replace(stream, Box::new(Ended));
    let mut owned = StreamOwned::new(conn, socket);
    while owned.conn.is_handshaking() {
        match owned.conn.complete_io(&mut owned.sock) {
            Ok((0, 0)) => return Err(failed(&"EOF detected")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(failed(&"EOF detected"));
            }
            Err(e) => return Err(failed(&e)),
        }
    }
    if direct && owned.conn.alpn_protocol() != Some(ALPN) {
        return Err(Some(
            "received direct SSL connection request without ALPN protocol negotiation extension"
                .to_owned(),
        ));
    }
    let peer = match owned.conn.peer_certificates().and_then(<[_]>::first) {
        Some(cert) => Some(x509::subject(cert).map_err(|text| text.map(str::to_owned))?),
        None => None,
    };
    *stream = Box::new(TlsStream(Mutex::new(owned)));
    Ok(Secured { hash: tls.hash.clone(), peer })
}

/// The place of a stream while the handshake runs.
#[derive(Debug)]
struct Ended;

impl Read for Ended {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl Write for Ended {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::NotConnected.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stream for Ended {
    fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    fn peer_addr(&self) -> String {
        String::new()
    }
}

/// A TLS connection over a stream of the platform. The lock is only for [`Stream::shutdown`] and [`Stream::peer_addr`], which take `&self`. A read and a write take `&mut self` and do not lock.
#[derive(Debug)]
struct TlsStream(Mutex<StreamOwned<ServerConnection, Box<dyn Stream>>>);

impl TlsStream {
    fn inner(&mut self) -> &mut StreamOwned<ServerConnection, Box<dyn Stream>> {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner().read(buf)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner().flush()
    }
}

impl Stream for TlsStream {
    /// Sends the alert `close_notify`, as `SSL_shutdown` in `be_tls_close`, and closes the socket.
    fn shutdown(&self) -> Result<()> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let owned = &mut *guard;
        owned.conn.send_close_notify();
        while owned.conn.wants_write() {
            if owned.conn.write_tls(&mut owned.sock).is_err() {
                break;
            }
        }
        owned.sock.shutdown()
    }

    fn peer_addr(&self) -> String {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).sock.peer_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_follows_the_signature_algorithm() {
        // A certificate with only the parts that the hash reads: the inner sequence and the algorithm.
        let cert = |oid: &[u8]| {
            let mut algorithm = vec![0x06, u8::try_from(oid.len()).unwrap()];
            algorithm.extend_from_slice(oid);
            let mut content = vec![0x30, 0x00, 0x30, u8::try_from(algorithm.len()).unwrap()];
            content.extend(algorithm);
            let mut cert = vec![0x30, u8::try_from(content.len()).unwrap()];
            cert.extend(content);
            cert
        };
        let ecdsa_sha256 = cert(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]);
        let ecdsa_sha384 = cert(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03]);
        let rsa_sha512 = cert(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d]);
        let sha1 = cert(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05]);
        assert_eq!(certificate_hash(&ecdsa_sha256).len(), 32);
        assert_eq!(certificate_hash(&ecdsa_sha384).len(), 48);
        assert_eq!(certificate_hash(&rsa_sha512).len(), 64);
        assert_eq!(certificate_hash(&sha1).len(), 32);
        assert_eq!(certificate_hash(b"junk").len(), 32);
        assert_eq!(
            certificate_hash(&ecdsa_sha256),
            digest::digest(&digest::SHA256, &ecdsa_sha256).as_ref()
        );
    }

    #[test]
    fn versions() {
        assert_eq!(version("TLSv1"), 1);
        assert_eq!(version("TLSv1.2"), 3);
        assert_eq!(version("TLSv1.3"), 4);
        assert_eq!(version(""), 0);
    }
}

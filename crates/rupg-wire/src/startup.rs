//! The start of a connection: the requests for encryption, the startup packet and the version.
//!
//! This is `ProcessStartupPacket` in `src/backend/tcop/backend_startup.c`, without the socket and without TLS. The server reads each packet with [`split_startup`](crate::split_startup) and gives it to [`Handshake::packet`], which writes the answer into the [`OutBuf`] and says what the server must do next. The checks and their order are those of PostgreSQL, so a client that sends a bad packet gets the same bytes back from rupg.
//!
//! Lifted from `crates/rudb-pgwire/src/startup.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::backend::OutBuf;
use crate::error::{Level, ProtocolError};
use crate::frame::{Cancel, GSSENC_REQUEST_CODE, PROTOCOL_3_2, Packet, SSL_REQUEST_CODE, Startup};

/// The longest user name or database name, in bytes. PostgreSQL cuts a longer name to this length, which is `NAMEDATALEN` minus one.
pub const NAME_LIMIT: usize = 63;

const FEATURE_NOT_SUPPORTED: &str = "0A000";
const INVALID_PARAMETER_VALUE: &str = "22023";
const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";

/// The state of a connection before its `StartupMessage`.
///
/// A client can ask for TLS once and for GSSAPI encryption once, in either order. A second request of the same kind is not an error of its own: PostgreSQL reads its code as a protocol version, and rejects the version. A connection that starts TLS is also done with GSSAPI.
#[derive(Debug, Clone)]
pub struct Handshake {
    tls: bool,
    ssl_done: bool,
    gss_done: bool,
    protocol: u32,
}

/// What the server does after [`Handshake::packet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<'a> {
    /// The answer to an encryption request is in the output. The server sends it. If `tls` is true, the server then runs the TLS handshake. Then it calls [`Encryption::check_buffered`] and reads the next packet.
    Answer { request: Encryption, tls: bool },
    /// A `CancelRequest`. The server looks for the key, and closes the connection without an answer in all cases.
    Cancel(Cancel<'a>),
    /// A good `StartupMessage`. The output can hold a `NegotiateProtocolVersion`, which the server sends before the first message of authentication.
    Start(StartupRequest<'a>),
}

/// A request for an encrypted connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    /// `SSLRequest`.
    Ssl,
    /// `GSSENCRequest`.
    Gss,
}

/// The `replication` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replication {
    /// No `replication` parameter, or a false value.
    Off,
    /// A true value: a WAL sender for physical replication, with no database.
    Physical,
    /// The value `database`: a WAL sender for logical replication, in a database.
    Database,
}

/// A `StartupMessage` that passed every check of PostgreSQL before authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupRequest<'a> {
    /// The version that the client sent.
    pub version: u32,
    /// The version of the session: the lower of [`StartupRequest::version`] and 3.2.
    pub protocol: u32,
    /// The `user` parameter, never empty, and at most [`NAME_LIMIT`] bytes.
    pub user: &'a [u8],
    /// The `database` parameter, or the user name when it is missing or empty, and at most [`NAME_LIMIT`] bytes. It is empty for [`Replication::Physical`].
    pub database: &'a [u8],
    /// The `options` parameter, which holds settings in the format of a command line.
    pub options: Option<&'a [u8]>,
    pub replication: Replication,
    /// Every other parameter, in the order the client sent them. The server applies each one as a setting with the source `client`.
    pub settings: Vec<(&'a [u8], &'a [u8])>,
    /// The names that start with `_pq_.`. rupg knows no protocol option, so all of them are here.
    pub unrecognized: Vec<&'a [u8]>,
}

impl Handshake {
    /// A connection on which the server can start TLS when `tls` is true. It is false when the server has no certificate, when the connection is on a Unix socket, and when the connection started with direct TLS.
    pub fn new(tls: bool) -> Handshake {
        Handshake { tls, ssl_done: false, gss_done: false, protocol: 0 }
    }

    /// The version for the format of an error, which is `FrontendProtocol` in PostgreSQL. Give it to [`OutBuf::protocol_error`]. It is zero until the server reads a version.
    pub fn protocol(&self) -> u32 {
        self.protocol
    }

    /// Answers one packet.
    ///
    /// # Errors
    ///
    /// A version with a major part other than 3, a bad `replication` value, a bad layout of the parameters, or no user name. All of them are [`Level::Fatal`]. The server writes the error with [`OutBuf::protocol_error`] and the version of [`Handshake::protocol`], after what this method already wrote, and closes the connection.
    pub fn packet<'a>(
        &mut self,
        packet: Packet<'a>,
        out: &mut OutBuf,
    ) -> Result<Step<'a>, ProtocolError> {
        match packet {
            Packet::Cancel(cancel) => Ok(Step::Cancel(cancel)),
            Packet::SslRequest if !self.ssl_done => {
                let tls = self.tls;
                out.encryption_response(if tls { b'S' } else { b'N' });
                self.ssl_done = true;
                self.gss_done |= tls;
                Ok(Step::Answer { request: Encryption::Ssl, tls })
            }
            Packet::GssEncRequest if !self.gss_done => {
                out.encryption_response(b'N');
                self.gss_done = true;
                Ok(Step::Answer { request: Encryption::Gss, tls: false })
            }
            Packet::SslRequest => {
                self.start(Startup { version: SSL_REQUEST_CODE, options: &[] }, out)
            }
            Packet::GssEncRequest => {
                self.start(Startup { version: GSSENC_REQUEST_CODE, options: &[] }, out)
            }
            Packet::Startup(startup) => self.start(startup, out),
        }
    }

    fn start<'a>(
        &mut self,
        startup: Startup<'a>,
        out: &mut OutBuf,
    ) -> Result<Step<'a>, ProtocolError> {
        let version = startup.version;
        self.protocol = version.min(PROTOCOL_3_2);
        let (major, minor) = (version >> 16, version & 0xffff);
        if major != 3 {
            return Err(ProtocolError::new(
                Level::Fatal,
                FEATURE_NOT_SUPPORTED,
                format!(
                    "unsupported frontend protocol {major}.{minor}: server supports 3.0 to 3.2"
                ),
            ));
        }
        let mut request = StartupRequest {
            version,
            protocol: self.protocol,
            user: &[],
            database: &[],
            options: None,
            replication: Replication::Off,
            settings: Vec::new(),
            unrecognized: Vec::new(),
        };
        // PostgreSQL keeps two flags. A later `replication=false` clears the first and not the second, so the order of the values matters in the same way here.
        let (mut walsender, mut db_walsender) = (false, false);
        startup.scan(|name, value| {
            match name {
                b"database" => request.database = value,
                b"user" => request.user = value,
                b"options" => request.options = Some(value),
                b"replication" if value == b"database" => (walsender, db_walsender) = (true, true),
                b"replication" => match parse_bool(value) {
                    Some(on) => walsender = on,
                    None => {
                        return Err(ProtocolError::new(
                            Level::Fatal,
                            INVALID_PARAMETER_VALUE,
                            format!(
                                "invalid value for parameter \"replication\": \"{}\"",
                                String::from_utf8_lossy(value)
                            ),
                        )
                        .with_hint("Valid values are: \"false\", 0, \"true\", 1, \"database\"."));
                    }
                },
                _ if name.starts_with(b"_pq_.") => request.unrecognized.push(name),
                _ => request.settings.push((name, value)),
            }
            Ok(())
        })?;
        if minor > PROTOCOL_3_2 & 0xffff || !request.unrecognized.is_empty() {
            out.negotiate_protocol_version(self.protocol, &request.unrecognized);
        }
        if request.user.is_empty() {
            return Err(ProtocolError::new(
                Level::Fatal,
                INVALID_AUTHORIZATION_SPECIFICATION,
                "no PostgreSQL user name specified in startup packet",
            ));
        }
        if request.database.is_empty() {
            request.database = request.user;
        }
        request.user = truncate(request.user);
        request.database = truncate(request.database);
        request.replication = match (walsender, db_walsender) {
            (false, _) => Replication::Off,
            (true, false) => Replication::Physical,
            (true, true) => Replication::Database,
        };
        if request.replication == Replication::Physical {
            request.database = &[];
        }
        Ok(Step::Start(request))
    }
}

impl Encryption {
    /// The check after the answer to an encryption request, and after the TLS handshake when there is one. `buffered` is the number of bytes that the server read from the socket before the handshake and did not use yet. Those bytes were not encrypted, so an attacker could have put them there.
    ///
    /// # Errors
    ///
    /// Any byte in the buffer. The error is [`Level::Fatal`], and the server sends it over the new TLS connection if there is one.
    pub fn check_buffered(self, buffered: usize) -> Result<(), ProtocolError> {
        if buffered == 0 {
            return Ok(());
        }
        let message = match self {
            Encryption::Ssl => "received unencrypted data after SSL request",
            Encryption::Gss => "received unencrypted data after GSSAPI encryption request",
        };
        Err(ProtocolError::fatal(message).with_detail(
            "This could be either a client-software bug or evidence of an attempted \
             man-in-the-middle attack.",
        ))
    }
}

/// `parse_bool` of `src/backend/utils/adt/bool.c`. It takes any prefix of `true`, `false`, `yes` and `no` in any case, `on`, `off` and a prefix of `off` of two bytes or more, `1` and `0`.
fn parse_bool(value: &[u8]) -> Option<bool> {
    let prefix_of = |word: &[u8], min: usize| {
        value.len() >= min
            && value.len() <= word.len()
            && word[..value.len()].eq_ignore_ascii_case(value)
    };
    match value.first()?.to_ascii_lowercase() {
        b't' if prefix_of(b"true", 1) => Some(true),
        b'f' if prefix_of(b"false", 1) => Some(false),
        b'y' if prefix_of(b"yes", 1) => Some(true),
        b'n' if prefix_of(b"no", 1) => Some(false),
        b'o' if prefix_of(b"on", 2) => Some(true),
        b'o' if prefix_of(b"off", 2) => Some(false),
        b'1' if value.len() == 1 => Some(true),
        b'0' if value.len() == 1 => Some(false),
        _ => None,
    }
}

/// Cuts a name to [`NAME_LIMIT`] bytes, as PostgreSQL does. The cut can split a character of more than one byte, and PostgreSQL does not check for that here.
fn truncate(name: &[u8]) -> &[u8] {
    &name[..name.len().min(NAME_LIMIT)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::frame::{PROTOCOL_3_0, encode_options, protocol};

    fn startup<'a>(version: u32, params: &[(&[u8], &[u8])], buf: &'a mut Vec<u8>) -> Startup<'a> {
        encode_options(params, buf);
        Startup { version, options: buf }
    }

    /// Runs one `StartupMessage` through a new handshake. Gives the result and the messages in the output, with the error written after them as the server would write it.
    fn run<'a>(
        version: u32,
        params: &[(&[u8], &[u8])],
        buf: &'a mut Vec<u8>,
    ) -> (Result<StartupRequest<'a>, ProtocolError>, Vec<u8>) {
        let packet = Packet::Startup(startup(version, params, buf));
        let mut handshake = Handshake::new(false);
        let mut out = OutBuf::new();
        let result = handshake.packet(packet, &mut out).map(|step| match step {
            Step::Start(request) => request,
            step => panic!("not a start: {step:?}"),
        });
        if let Err(error) = &result {
            out.protocol_error(error, handshake.protocol());
        }
        (result, out.as_bytes().to_vec())
    }

    fn messages(bytes: &[u8]) -> Vec<Backend<'_>> {
        let mut messages = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let (message, len) = Backend::decode(&bytes[at..]).unwrap().unwrap();
            messages.push(message);
            at += len;
        }
        messages
    }

    #[test]
    fn plain_startup() {
        let mut buf = Vec::new();
        let params: &[(&[u8], &[u8])] = &[
            (b"user", b"alice"),
            (b"application_name", b"psql"),
            (b"client_encoding", b"UTF8"),
            (b"options", b"-c work_mem=64MB"),
        ];
        let (request, out) = run(PROTOCOL_3_0, params, &mut buf);
        let request = request.unwrap();
        assert!(out.is_empty());
        assert_eq!(request.protocol, PROTOCOL_3_0);
        assert_eq!(request.user, b"alice");
        assert_eq!(request.database, b"alice");
        assert_eq!(request.options, Some(&b"-c work_mem=64MB"[..]));
        assert_eq!(request.replication, Replication::Off);
        assert_eq!(
            request.settings,
            vec![(&b"application_name"[..], &b"psql"[..]), (b"client_encoding", b"UTF8")]
        );
    }

    #[test]
    fn last_value_wins() {
        let mut buf = Vec::new();
        let params: &[(&[u8], &[u8])] =
            &[(b"user", b"a"), (b"database", b"x"), (b"user", b"b"), (b"database", b"")];
        let request = run(PROTOCOL_3_0, params, &mut buf).0.unwrap();
        assert_eq!((request.user, request.database), (&b"b"[..], &b"b"[..]));
    }

    #[test]
    fn grease_version() {
        let mut buf = Vec::new();
        let (request, out) = run(protocol(3, 9999), &[(b"user", b"u")], &mut buf);
        let request = request.unwrap();
        assert_eq!(request.protocol, PROTOCOL_3_2);
        assert_eq!(
            messages(&out),
            vec![Backend::NegotiateProtocolVersion { version: PROTOCOL_3_2, options: vec![] }]
        );
    }

    #[test]
    fn grease_option() {
        let mut buf = Vec::new();
        let params: &[(&[u8], &[u8])] =
            &[(b"user", b"u"), (b"_pq_.test_protocol_negotiation", b""), (b"_pq_.other", b"1")];
        let (request, out) = run(PROTOCOL_3_0, params, &mut buf);
        assert_eq!(request.unwrap().settings, vec![]);
        assert_eq!(
            messages(&out),
            vec![Backend::NegotiateProtocolVersion {
                version: PROTOCOL_3_0,
                options: vec![b"_pq_.test_protocol_negotiation", b"_pq_.other"],
            }]
        );
    }

    #[test]
    fn versions_without_negotiation() {
        for version in [PROTOCOL_3_0, protocol(3, 1), PROTOCOL_3_2] {
            let mut buf = Vec::new();
            let (request, out) = run(version, &[(b"user", b"u")], &mut buf);
            assert_eq!(request.unwrap().protocol, version);
            assert!(out.is_empty());
        }
    }

    #[test]
    fn unsupported_major_version() {
        let mut buf = Vec::new();
        let (request, out) = run(protocol(4, 0), &[(b"user", b"u")], &mut buf);
        let error = request.unwrap_err();
        assert_eq!(error.sqlstate, "0A000");
        assert_eq!(error.message, "unsupported frontend protocol 4.0: server supports 3.0 to 3.2");
        assert_eq!(
            messages(&out),
            vec![Backend::ErrorResponse(vec![
                (b'S', b"FATAL"),
                (b'V', b"FATAL"),
                (b'C', b"0A000"),
                (b'M', b"unsupported frontend protocol 4.0: server supports 3.0 to 3.2"),
            ])]
        );
    }

    #[test]
    fn old_major_version_gets_old_format() {
        let mut buf = Vec::new();
        let (_, out) = run(protocol(2, 0), &[(b"user", b"u")], &mut buf);
        assert_eq!(
            out,
            b"EFATAL:  unsupported frontend protocol 2.0: server supports 3.0 to 3.2\n\0"
        );
    }

    #[test]
    fn version_zero_gets_new_format() {
        let mut buf = Vec::new();
        let (_, out) = run(0, &[(b"user", b"u")], &mut buf);
        assert!(matches!(&messages(&out)[..], [Backend::ErrorResponse(fields)]
            if fields[3] == (b'M', &b"unsupported frontend protocol 0.0: server supports 3.0 to 3.2"[..])));
    }

    #[test]
    fn missing_user_after_negotiation() {
        let mut buf = Vec::new();
        let (request, out) = run(protocol(3, 5), &[(b"database", b"d")], &mut buf);
        assert_eq!(request.unwrap_err().sqlstate, "28000");
        let messages = messages(&out);
        assert_eq!(
            messages[0],
            Backend::NegotiateProtocolVersion { version: PROTOCOL_3_2, options: vec![] }
        );
        assert!(matches!(&messages[1], Backend::ErrorResponse(fields)
            if fields[3] == (b'M', &b"no PostgreSQL user name specified in startup packet"[..])));
    }

    #[test]
    fn replication_values() {
        let cases: &[(&[u8], Replication)] = &[
            (b"tr", Replication::Physical),
            (b"YES", Replication::Physical),
            (b"on", Replication::Physical),
            (b"1", Replication::Physical),
            (b"of", Replication::Off),
            (b"0", Replication::Off),
            (b"database", Replication::Database),
        ];
        for &(value, replication) in cases {
            let mut buf = Vec::new();
            let params: &[(&[u8], &[u8])] =
                &[(b"user", b"u"), (b"database", b"d"), (b"replication", value)];
            let request = run(PROTOCOL_3_0, params, &mut buf).0.unwrap();
            assert_eq!(request.replication, replication, "{value:?}");
            let database: &[u8] = if replication == Replication::Physical { b"" } else { b"d" };
            assert_eq!(request.database, database);
        }
        // A false value after `database` keeps the second flag of PostgreSQL, and a true value after it then gives a logical WAL sender.
        let mut buf = Vec::new();
        let params: &[(&[u8], &[u8])] = &[
            (b"user", b"u"),
            (b"replication", b"database"),
            (b"replication", b"off"),
            (b"replication", b"true"),
        ];
        assert_eq!(
            run(PROTOCOL_3_0, params, &mut buf).0.unwrap().replication,
            Replication::Database
        );
    }

    #[test]
    fn bad_replication_value_comes_before_layout() {
        for value in [&b"o"[..], b"", b"truex", b"2", b"Database"] {
            let mut buf = Vec::new();
            encode_options(&[(b"replication", value)], &mut buf);
            buf.push(b'x');
            let packet = Packet::Startup(Startup { version: PROTOCOL_3_0, options: &buf });
            let error = Handshake::new(false).packet(packet, &mut OutBuf::new()).unwrap_err();
            assert_eq!(error.sqlstate, "22023");
            assert_eq!(
                error.message,
                format!(
                    "invalid value for parameter \"replication\": \"{}\"",
                    String::from_utf8_lossy(value)
                )
            );
            assert_eq!(
                error.hint,
                Some("Valid values are: \"false\", 0, \"true\", 1, \"database\".")
            );
        }
    }

    #[test]
    fn long_names_are_cut() {
        let mut buf = Vec::new();
        let long = [b'n'; 100];
        let request = run(PROTOCOL_3_0, &[(b"user", &long)], &mut buf).0.unwrap();
        assert_eq!(request.user.len(), NAME_LIMIT);
        assert_eq!(request.database.len(), NAME_LIMIT);
    }

    #[test]
    fn encryption_requests() {
        let mut out = OutBuf::new();
        let mut handshake = Handshake::new(false);
        assert_eq!(
            handshake.packet(Packet::GssEncRequest, &mut out),
            Ok(Step::Answer { request: Encryption::Gss, tls: false })
        );
        assert_eq!(
            handshake.packet(Packet::SslRequest, &mut out),
            Ok(Step::Answer { request: Encryption::Ssl, tls: false })
        );
        assert_eq!(out.as_bytes(), b"NN");
        // A second request reads as a version.
        let error = handshake.packet(Packet::SslRequest, &mut out).unwrap_err();
        assert_eq!(
            error.message,
            "unsupported frontend protocol 1234.5679: server supports 3.0 to 3.2"
        );
        assert_eq!(handshake.protocol(), PROTOCOL_3_2);

        // After TLS, a GSSAPI request is a second request.
        let mut out = OutBuf::new();
        let mut handshake = Handshake::new(true);
        assert_eq!(
            handshake.packet(Packet::SslRequest, &mut out),
            Ok(Step::Answer { request: Encryption::Ssl, tls: true })
        );
        assert_eq!(out.as_bytes(), b"S");
        let error = handshake.packet(Packet::GssEncRequest, &mut out).unwrap_err();
        assert_eq!(
            error.message,
            "unsupported frontend protocol 1234.5680: server supports 3.0 to 3.2"
        );
    }

    #[test]
    fn unencrypted_data() {
        assert_eq!(Encryption::Ssl.check_buffered(0), Ok(()));
        let error = Encryption::Ssl.check_buffered(1).unwrap_err();
        assert_eq!((error.level, error.sqlstate), (Level::Fatal, "08P01"));
        assert_eq!(error.message, "received unencrypted data after SSL request");
        let error = Encryption::Gss.check_buffered(5).unwrap_err();
        assert_eq!(error.message, "received unencrypted data after GSSAPI encryption request");
        assert!(error.detail.unwrap().ends_with("man-in-the-middle attack."));
    }

    #[test]
    fn cancel_passes_through() {
        let packet = Packet::Cancel(Cancel { pid: 7, key: b"abcd" });
        let mut out = OutBuf::new();
        assert_eq!(
            Handshake::new(false).packet(packet, &mut out),
            Ok(Step::Cancel(Cancel { pid: 7, key: b"abcd" }))
        );
        assert!(out.is_empty());
    }

    #[test]
    fn log_errors_write_nothing() {
        let mut out = OutBuf::new();
        out.protocol_error(&ProtocolError::log("invalid length of startup packet"), 0);
        assert!(out.is_empty());
    }
}

//! Frames: where one message ends and the next one starts.
//!
//! A message after startup is one type byte, a 32-bit big-endian length that counts itself but not the type byte, and the body. The startup packet and the three special requests have no type byte. They start with the length, and the first four bytes of the body are a code that tells them apart.
//!
//! The limits and the error texts are the ones of `SocketBackend` in `src/backend/tcop/postgres.c`, `pq_getmessage` in `src/backend/libpq/pqcomm.c`, `ProcessStartupPacket` in `src/backend/tcop/backend_startup.c` and the reads of the authentication messages in `src/backend/libpq/auth.c` and `auth-sasl.c`.
//!
//! Lifted from `crates/rudb-pgwire/src/frame.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::ProtocolError;
use crate::reader::Reader;

/// `PQ_SMALL_MESSAGE_LIMIT` in `libpq.h`. The length word counts toward it.
pub const SMALL_MESSAGE_LIMIT: usize = 10_000;

/// `PQ_LARGE_MESSAGE_LIMIT` in `libpq.h`, which is `MaxAllocSize - 1`. `MaxAllocSize` is 1 GB minus 1, so this is 1 GB minus 2.
pub const LARGE_MESSAGE_LIMIT: usize = 0x3fff_fffe;

/// `PG_MAX_AUTH_TOKEN_LENGTH` in `auth.h`, the limit for the messages of authentication.
pub const AUTH_MESSAGE_LIMIT: usize = 65_535;

/// `MAX_STARTUP_PACKET_LENGTH` in `pqcomm.h`. It is the limit for the body, without the length word.
pub const STARTUP_PACKET_LIMIT: usize = 10_000;

/// The longest cancel key that a `CancelRequest` can carry. Protocol 3.0 has a 4-byte key and protocol 3.2 allows up to 256 bytes.
pub const CANCEL_KEY_LIMIT: usize = 256;

/// Protocol version 3.0.
pub const PROTOCOL_3_0: u32 = protocol(3, 0);
/// Protocol version 3.2, the newest version that PostgreSQL 19 speaks.
pub const PROTOCOL_3_2: u32 = protocol(3, 2);
/// The code of `CancelRequest`.
pub const CANCEL_REQUEST_CODE: u32 = protocol(1234, 5678);
/// The code of `SSLRequest`.
pub const SSL_REQUEST_CODE: u32 = protocol(1234, 5679);
/// The code of `GSSENCRequest`.
pub const GSSENC_REQUEST_CODE: u32 = protocol(1234, 5680);

/// `PG_PROTOCOL(major, minor)`.
pub const fn protocol(major: u16, minor: u16) -> u32 {
    (major as u32) << 16 | minor as u32
}

/// What the server is reading, which sets the type bytes that it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The main loop of a session, between queries and in the extended protocol.
    Query,
    /// The data of `COPY FROM STDIN`.
    CopyIn,
    /// The answer to `AuthenticationCleartextPassword` or `AuthenticationMD5Password`.
    Password,
    /// An answer in a SASL exchange.
    Sasl,
    /// An answer in a GSSAPI exchange.
    Gss,
}

/// One message, with its type byte and its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub tag: u8,
    pub body: &'a [u8],
}

impl Frame<'_> {
    /// The number of input bytes that the frame takes: the type byte, the length word and the body.
    pub fn size(&self) -> usize {
        5 + self.body.len()
    }
}

/// The length limit for a message of type `tag` in `mode`, or the error that PostgreSQL gives for the type byte alone.
fn limit(tag: u8, mode: Mode) -> Result<usize, ProtocolError> {
    let expected = match mode {
        Mode::Query => {
            return match tag {
                b'Q' | b'F' | b'B' | b'P' | b'd' => Ok(LARGE_MESSAGE_LIMIT),
                b'X' | b'C' | b'D' | b'E' | b'H' | b'S' | b'c' | b'f' => Ok(SMALL_MESSAGE_LIMIT),
                _ => Err(ProtocolError::fatal(format!("invalid frontend message type {tag}"))),
            };
        }
        Mode::CopyIn => {
            return match tag {
                b'd' => Ok(LARGE_MESSAGE_LIMIT),
                b'c' | b'f' | b'H' | b'S' => Ok(SMALL_MESSAGE_LIMIT),
                _ => Err(ProtocolError::error(format!(
                    "unexpected message type 0x{tag:02X} during COPY from stdin"
                ))),
            };
        }
        Mode::Password => "password",
        Mode::Sasl => "SASL",
        Mode::Gss => "GSS",
    };
    if tag == b'p' {
        Ok(AUTH_MESSAGE_LIMIT)
    } else {
        // PostgreSQL raises these at the level ERROR, but an error before the session exists ends the connection, so the client reads FATAL.
        Err(ProtocolError::fatal(format!("expected {expected} response, got message type {tag}")))
    }
}

/// Finds the first message in `input`.
///
/// It gives `Ok(None)` when `input` does not hold all of the message yet. The server then reads more bytes and calls again with all the bytes it has not used. It checks the type byte as soon as there is one, and the length as soon as there are four more, as PostgreSQL does, so a client that sends garbage gets its error without the server waiting for more bytes.
///
/// # Errors
///
/// A type byte that `mode` does not accept, or a length below 4 or above the limit for the type. [`ProtocolError::level`] tells the server what to do next.
pub fn split(input: &[u8], mode: Mode) -> Result<Option<Frame<'_>>, ProtocolError> {
    let Some(&tag) = input.first() else {
        return Ok(None);
    };
    let limit = limit(tag, mode)?;
    let Some(word) = input.get(1..5) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes([word[0], word[1], word[2], word[3]]) as usize;
    if !(4..=limit).contains(&len) {
        return Err(ProtocolError::log("invalid message length"));
    }
    Ok(input.get(5..1 + len).map(|body| Frame { tag, body }))
}

/// The first packet of a connection, or the packet after an `SSLRequest` or `GSSENCRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    Startup(Startup<'a>),
    SslRequest,
    GssEncRequest,
    Cancel(Cancel<'a>),
}

/// A `StartupMessage`. The codec does not check the version, because the server must answer an unknown version before it reads the parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Startup<'a> {
    pub version: u32,
    /// The bytes after the version: pairs of zero-terminated names and values, and a zero byte.
    pub options: &'a [u8],
}

/// A name and a value in a `StartupMessage`.
pub type Param<'a> = (&'a [u8], &'a [u8]);

/// A `CancelRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancel<'a> {
    pub pid: i32,
    pub key: &'a [u8],
}

impl<'a> Startup<'a> {
    /// The parameters in the order the client sent them.
    ///
    /// # Errors
    ///
    /// A packet that does not end with the zero byte right after the last pair. This is the check of `ProcessStartupPacket`, which also finds a name without a value this way.
    pub fn params(&self) -> Result<Vec<Param<'a>>, ProtocolError> {
        let mut params = Vec::new();
        self.scan(|name, value| {
            params.push((name, value));
            Ok(())
        })?;
        Ok(params)
    }

    /// Gives each parameter to `each` as it reads it, and checks the layout at the end. An error from `each` stops the scan, so it comes before an error in the layout, as in PostgreSQL.
    pub(crate) fn scan(
        &self,
        mut each: impl FnMut(&'a [u8], &'a [u8]) -> Result<(), ProtocolError>,
    ) -> Result<(), ProtocolError> {
        let options = self.options;
        let mut at = 0;
        // PostgreSQL reads the packet into a buffer with one more zero byte at the end, so a string that runs to the end of the packet still ends.
        let string = |at: usize| {
            let rest = &options[at.min(options.len())..];
            &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())]
        };
        while at < options.len() {
            let name = string(at);
            if name.is_empty() {
                break;
            }
            let value_at = at + name.len() + 1;
            if value_at >= options.len() {
                break;
            }
            let value = string(value_at);
            each(name, value)?;
            at = value_at + value.len() + 1;
        }
        if at + 1 != options.len() {
            return Err(ProtocolError::fatal(
                "invalid startup packet layout: expected terminator as last byte",
            ));
        }
        Ok(())
    }
}

/// Finds the first packet in `input`, which has no type byte. It gives the packet and the number of bytes it takes, or `Ok(None)` when `input` does not hold all of it yet.
///
/// # Errors
///
/// A length out of range, or a `CancelRequest` without a key or with a key that is too long. These are all [`Level::Log`](crate::Level::Log), as in PostgreSQL.
pub fn split_startup(input: &[u8]) -> Result<Option<(Packet<'_>, usize)>, ProtocolError> {
    let Some(word) = input.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes([word[0], word[1], word[2], word[3]]) as usize;
    if !(8..=4 + STARTUP_PACKET_LIMIT).contains(&len) {
        return Err(ProtocolError::log("invalid length of startup packet"));
    }
    let Some(body) = input.get(4..len) else {
        return Ok(None);
    };
    let mut reader = Reader::new(body);
    let code = reader.u32()?;
    let packet = match code {
        CANCEL_REQUEST_CODE => {
            let Ok(pid) = reader.i32() else {
                return Err(ProtocolError::log("invalid length of cancel request packet"));
            };
            let key = reader.rest();
            if key.is_empty() || key.len() > CANCEL_KEY_LIMIT {
                return Err(ProtocolError::log(
                    "invalid length of cancel key in cancel request packet",
                ));
            }
            Packet::Cancel(Cancel { pid, key })
        }
        SSL_REQUEST_CODE => Packet::SslRequest,
        GSSENC_REQUEST_CODE => Packet::GssEncRequest,
        version => Packet::Startup(Startup { version, options: reader.rest() }),
    };
    Ok(Some((packet, len)))
}

impl Packet<'_> {
    /// Writes the packet as a client sends it.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        match self {
            Packet::Startup(startup) => {
                out.extend_from_slice(&startup.version.to_be_bytes());
                out.extend_from_slice(startup.options);
            }
            Packet::SslRequest => out.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes()),
            Packet::GssEncRequest => out.extend_from_slice(&GSSENC_REQUEST_CODE.to_be_bytes()),
            Packet::Cancel(cancel) => {
                out.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
                out.extend_from_slice(&cancel.pid.to_be_bytes());
                out.extend_from_slice(cancel.key);
            }
        }
        let len = (out.len() - start) as u32;
        out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }
}

/// Writes the options of a `StartupMessage` from a list of pairs, with the zero byte at the end.
pub fn encode_options(params: &[(&[u8], &[u8])], out: &mut Vec<u8>) {
    for (name, value) in params {
        out.extend_from_slice(name);
        out.push(0);
        out.extend_from_slice(value);
        out.push(0);
    }
    out.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Level;

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_frame_waits_for_all_of_its_bytes() {
        let bytes = frame(b'Q', b"select 1\0");
        for end in 0..bytes.len() {
            assert_eq!(split(&bytes[..end], Mode::Query), Ok(None), "{end} bytes");
        }
        let mut more = bytes.clone();
        more.extend_from_slice(&frame(b'S', b""));
        let first = split(&more, Mode::Query).unwrap().unwrap();
        assert_eq!(first, Frame { tag: b'Q', body: b"select 1\0" });
        assert_eq!(first.size(), bytes.len());
        let second = split(&more[first.size()..], Mode::Query).unwrap().unwrap();
        assert_eq!(second, Frame { tag: b'S', body: b"" });
    }

    #[test]
    fn an_unknown_type_byte_fails_before_the_length_arrives() {
        let error = split(b"p", Mode::Query).unwrap_err();
        assert_eq!(error.level, Level::Fatal);
        assert_eq!(error.message, "invalid frontend message type 112");
        let error = split(b"Q", Mode::CopyIn).unwrap_err();
        assert_eq!(error.level, Level::Error);
        assert_eq!(error.message, "unexpected message type 0x51 during COPY from stdin");
        let error = split(b"Q", Mode::Sasl).unwrap_err();
        assert_eq!(error.message, "expected SASL response, got message type 81");
        assert_eq!(split(b"p", Mode::Password), Ok(None));
    }

    #[test]
    fn each_type_has_the_limit_of_postgres() {
        let at = |tag: u8, mode: Mode, len: usize| {
            let mut bytes = vec![tag];
            bytes.extend_from_slice(&(len as u32).to_be_bytes());
            split(&bytes, mode).map(|frame| frame.is_some())
        };
        for tag in *b"QFBPd" {
            assert_eq!(at(tag, Mode::Query, LARGE_MESSAGE_LIMIT), Ok(false));
            assert!(at(tag, Mode::Query, LARGE_MESSAGE_LIMIT + 1).is_err());
        }
        for tag in *b"XCDEHScf" {
            assert_eq!(at(tag, Mode::Query, SMALL_MESSAGE_LIMIT), Ok(false));
            let error = at(tag, Mode::Query, SMALL_MESSAGE_LIMIT + 1).unwrap_err();
            assert_eq!(
                (error.level, error.message.as_str()),
                (Level::Log, "invalid message length")
            );
        }
        assert!(at(b'S', Mode::CopyIn, SMALL_MESSAGE_LIMIT + 1).is_err());
        assert!(at(b'p', Mode::Password, AUTH_MESSAGE_LIMIT + 1).is_err());
        for len in [0, 3, u32::MAX as usize] {
            assert!(at(b'S', Mode::Query, len).is_err(), "{len}");
        }
        assert_eq!(split(&frame(b'S', b""), Mode::Query).unwrap().unwrap().body, b"");
    }

    #[test]
    fn startup_packets_round_trip() {
        let mut options = Vec::new();
        encode_options(&[(b"user", b"rpg"), (b"database", b"postgres")], &mut options);
        let packets = [
            Packet::Startup(Startup { version: PROTOCOL_3_2, options: &options }),
            Packet::SslRequest,
            Packet::GssEncRequest,
            Packet::Cancel(Cancel { pid: 4242, key: &[1, 2, 3, 4] }),
            Packet::Cancel(Cancel { pid: -1, key: &[7; CANCEL_KEY_LIMIT] }),
        ];
        for packet in packets {
            let mut bytes = Vec::new();
            packet.encode(&mut bytes);
            for end in 0..bytes.len() {
                assert_eq!(split_startup(&bytes[..end]), Ok(None));
            }
            assert_eq!(split_startup(&bytes), Ok(Some((packet, bytes.len()))));
        }
        let Packet::Startup(startup) = packets[0] else { unreachable!() };
        let params: Vec<(&[u8], &[u8])> = vec![(b"user", b"rpg"), (b"database", b"postgres")];
        assert_eq!(startup.params(), Ok(params));
    }

    #[test]
    fn startup_packets_fail_as_postgres_fails_them() {
        let error = |bytes: &[u8]| split_startup(bytes).unwrap_err().message;
        assert_eq!(error(&7u32.to_be_bytes()), "invalid length of startup packet");
        assert_eq!(error(&10_005u32.to_be_bytes()), "invalid length of startup packet");
        let mut short = Vec::new();
        Packet::Cancel(Cancel { pid: 1, key: &[] }).encode(&mut short);
        assert_eq!(error(&short), "invalid length of cancel key in cancel request packet");
        let mut long = Vec::new();
        Packet::Cancel(Cancel { pid: 1, key: &[0; CANCEL_KEY_LIMIT + 1] }).encode(&mut long);
        assert_eq!(error(&long), "invalid length of cancel key in cancel request packet");
        let mut no_pid = vec![0, 0, 0, 10];
        no_pid.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        no_pid.extend_from_slice(&[0, 0]);
        assert_eq!(error(&no_pid), "invalid length of cancel request packet");

        let layout = "invalid startup packet layout: expected terminator as last byte";
        for options in [&b""[..], b"user\0", b"user\0rpg\0", b"user\0rpg", b"user\0rpg\0\0\0"] {
            let startup = Startup { version: PROTOCOL_3_0, options };
            let error = startup.params().unwrap_err();
            assert_eq!((error.level, error.message.as_str()), (Level::Fatal, layout));
        }
        assert_eq!(Startup { version: PROTOCOL_3_0, options: b"\0" }.params(), Ok(vec![]));
    }
}

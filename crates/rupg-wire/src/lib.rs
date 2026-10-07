//! The message codec of protocols 3.0 and 3.2, the SCRAM exchange, the `COPY` sub-protocol, the replication messages.
//!
//! This crate first ships in milestone M2. See `spec/06-wire-and-sessions.md` section 6.1, `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md` section 23.5.
//!
//! This crate is the codec of `rupg-server`. It reads bytes into typed messages and writes typed messages into bytes. It has no sockets, no threads, no clock and no TLS, so it is a pure function of its input. A fuzzer can drive it without a socket, and `rupg-compat` can replay a recorded session into it and compare the output byte for byte. Spec/06 says what the codec does, and the server at the pin is the reference: where the documentation and `postgres.c` disagree, this crate does what `postgres.c` does.
//!
//! # What is here
//!
//! [`split`] and [`split_startup`], which find where one message ends. They check the type byte and the length with the limits of PostgreSQL, and they accept partial input: the server gives them all the bytes it has, and they say how many they used.
//!
//! [`Frontend`], the messages of the client after startup, and [`Packet`], the startup packet and the three requests that come before it. They borrow from the input. A query string or a parameter value is never copied, and it is never converted from the client encoding here.
//!
//! [`OutBuf`], the output of one connection, with one method for each message of the server. [`Backend`] is the same set of messages as values, to read a server back.
//!
//! [`ProtocolError`], which has the SQLSTATE, the text of PostgreSQL for the same bytes, and the [`Level`] that tells the server whether to go on, to send `FATAL`, or to close without a word. [`OutBuf::protocol_error`] writes it in the format the client expects.
//!
//! [`Handshake`], the start of a connection up to authentication: the answers to `SSLRequest` and `GSSENCRequest`, the check for unencrypted bytes after them, the version check, the parameters of the `StartupMessage` and `NegotiateProtocolVersion`. It gives a [`StartupRequest`] with the user, the database and the settings.
//!
//! The password methods: [`password_message`] for `password` and `md5`, [`verify_md5`] and [`verify_password`] for the checks, and [`Scram`] for the server side of SCRAM-SHA-256 and SCRAM-SHA-256-PLUS. The hash functions come from the server through the [`Crypto`] trait, so this crate has no hash code of its own. [`saslprep`] and [`prepare_password`] apply SASLprep to a clear text password, as PostgreSQL does before it makes or checks a SCRAM secret. The normalization comes from `rupg-types`, the only dependency.
//!
//! [`Session`], the state of the main loop of a session: when the server sends `ReadyForQuery`, which messages it drops until `Sync` after an error, and which messages it ignores. The server gets from it only the messages that PostgreSQL acts on. [`verify_utf8`] checks a string from the client with the error of PostgreSQL.
//!
//! [`CommandTag`], the tags of `cmdtaglist.h` at the pin, and [`OutBuf::command_tag`], which writes `CommandComplete` with the row count where PostgreSQL shows one.
//!
//! [`Statements`] and [`Portals`], the prepared statements and the portals of a session by name, with the errors of PostgreSQL. [`Bind::params`] and [`FunctionCall::args`] give the values one by one, so the server can convert each value before it reads the next, as PostgreSQL does.
//!
//! [`CancelKey`], the key of `BackendKeyData` with the length of the protocol version, and [`cancel_target`], which checks a `CancelRequest` in constant time and gives the log line of PostgreSQL when it cancels nothing.
//!
//! Lifted from `crates/rudb-pgwire/src/lib.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

#![forbid(unsafe_code)]

mod auth;
mod backend;
mod base64;
mod cancel;
mod cmdtag;
mod crypto;
mod error;
mod frame;
mod frontend;
mod generated;
mod names;
mod reader;
mod saslprep;
mod session;
mod startup;

pub use auth::{
    Exchange, MD5_WARNING, MD5_WARNING_DETAIL, MOCK_NONCE_LEN, PasswordType, SCRAM_ITERATIONS,
    SCRAM_NONCE_LEN, SCRAM_SALT_LEN, SCRAM_SHA_256, SCRAM_SHA_256_PLUS, Scram, ScramSecret,
    md5_encrypt, password_failed, password_message, verify_md5, verify_password,
};
pub use backend::{Authentication, Backend, Field, Mark, OutBuf, TransactionStatus};
pub use cancel::{CANCEL_KEY_LEN, CancelKey, QUERY_CANCELED, cancel_target};
pub use cmdtag::CommandTag;
pub use crypto::{Crypto, md5};
pub use error::{Level, PROTOCOL_VIOLATION, ProtocolError};
pub use frame::{
    AUTH_MESSAGE_LIMIT, CANCEL_KEY_LIMIT, CANCEL_REQUEST_CODE, Cancel, Frame, GSSENC_REQUEST_CODE,
    LARGE_MESSAGE_LIMIT, Mode, PROTOCOL_3_0, PROTOCOL_3_2, Packet, Param, SMALL_MESSAGE_LIMIT,
    SSL_REQUEST_CODE, STARTUP_PACKET_LIMIT, Startup, encode_options, protocol, split,
    split_startup,
};
pub use frontend::{
    Bind, BindBody, BindParams, CallArgs, Formats, Frontend, FunctionCall, FunctionCallBody, Oids,
    Target, ValueIter, Values, encode_oids,
};
pub use names::{Portals, Statements};
pub use reader::verify_utf8;
pub use saslprep::{SaslprepError, prepare_password, saslprep};
pub use session::{Read, Session};
pub use startup::{Encryption, Handshake, NAME_LIMIT, Replication, StartupRequest, Step};

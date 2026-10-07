//! The state of a session after startup, which is what the main loop of `postgres.c` keeps in its local variables.
//!
//! PostgreSQL decides three things in that loop that a client can see: when it sends `ReadyForQuery`, which messages it drops after an error, and which messages it ignores. The rules are in `PostgresMain` and `SocketBackend`, and [`Session`] does the same thing with the same three flags. The server gives it the bytes from the socket and gets the messages that it must act on. It never sees a message that PostgreSQL would drop.
//!
//! Lifted from `crates/rudb-pgwire/src/session.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::backend::{OutBuf, TransactionStatus};
use crate::error::{Level, ProtocolError};
use crate::frame::{Mode, split};
use crate::frontend::Frontend;

/// The protocol state of one session.
///
/// The server uses it in a loop:
///
/// 1. If [`Session::wants_ready`] is true, the server sends the notifications that wait for the client if no transaction block is open, then a `ParameterStatus` for each reported setting that changed, and then calls [`Session::ready_for_query`]. This is the order of PostgreSQL.
/// 2. It calls [`Session::read`] with all the bytes that it did not use yet, and acts on the message that it gets.
/// 3. After it sends an `ErrorResponse` with severity `ERROR`, for any reason, it calls [`Session::recover`].
#[derive(Debug, Clone)]
pub struct Session {
    mode: Mode,
    /// `doing_extended_query_message`: the last message was one of the extended protocol, so an error starts the skip to `Sync`.
    extended: bool,
    /// `ignore_till_sync`.
    skip: bool,
    /// `send_ready_for_query`.
    ready: bool,
    /// An error came after the type byte of a message and before its end, so the server does not know where the next message starts.
    torn: bool,
    utf8: bool,
}

/// What [`Session::read`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read<'a> {
    /// The number of input bytes that the session used. The server drops them, also when there is no message and when there is an error. It is more than zero when the session dropped messages and found no message to give.
    pub used: usize,
    /// The next message for the server, or the error for it. `None` means that the session needs more input.
    pub message: Option<Result<Frontend<'a>, ProtocolError>>,
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

impl Session {
    /// The state at the start of the main loop, which sends `ReadyForQuery` first.
    pub fn new() -> Session {
        Session {
            mode: Mode::Query,
            extended: false,
            skip: false,
            ready: true,
            torn: false,
            utf8: true,
        }
    }

    /// Tells the session whether the client encoding is `UTF8` or `SQL_ASCII`. Then the session checks each string of a message as [`Frontend::parse_utf8`] does. It is on at the start, because the default `client_encoding` is the encoding of the database, which is `UTF8`. For any other encoding the server converts the strings itself.
    pub fn set_utf8(&mut self, utf8: bool) {
        self.utf8 = utf8;
    }

    /// True when the session drops messages until the next `Sync`.
    pub fn skipping(&self) -> bool {
        self.skip
    }

    /// True when the session reads the data of `COPY FROM STDIN`.
    pub fn in_copy(&self) -> bool {
        self.mode == Mode::CopyIn
    }

    /// True when the server must send `ReadyForQuery` before it reads the next message. It is never true in `COPY FROM STDIN`, because PostgreSQL does not go back to the main loop before the copy ends.
    pub fn wants_ready(&self) -> bool {
        self.ready && self.mode == Mode::Query
    }

    /// Writes `ReadyForQuery` and clears [`Session::wants_ready`]. PostgreSQL flushes its output after this message, and the server must do the same.
    pub fn ready_for_query(&mut self, status: TransactionStatus, out: &mut OutBuf) {
        out.ready_for_query(status);
        self.ready = false;
    }

    /// Starts `COPY FROM STDIN`. The server calls this when it sends `CopyInResponse`. The copy ends at `CopyDone`, which [`Session::read`] gives, or at an error, after which the server calls [`Session::recover`].
    pub fn copy_in(&mut self) {
        self.mode = Mode::CopyIn;
    }

    /// Reads the next message that the server must act on.
    ///
    /// In the main loop, the session drops every message other than `Sync` and `Terminate` while it skips, and it ignores `CopyData`, `CopyDone` and `CopyFail` from a client that sent them after the copy ended. In `COPY FROM STDIN` it ignores `Flush` and `Sync`, as PostgreSQL does for the convenience of client libraries, and it reads the body of no message other than `CopyFail`.
    ///
    /// The server fails the copy with SQLSTATE `57014` and the text `COPY from stdin failed: ` followed by the string of a `CopyFail` that it gets.
    pub fn read<'a>(&mut self, input: &'a [u8]) -> Read<'a> {
        let mut used = 0;
        loop {
            let frame = match split(&input[used..], self.mode) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Read { used, message: None },
                Err(error) => {
                    // Only the type check of COPY FROM STDIN fails at the level ERROR here. The type byte is read, the rest of the message is not.
                    self.torn = error.level == Level::Error;
                    return Read { used, message: Some(Err(error)) };
                }
            };
            used += frame.size();
            if self.mode == Mode::CopyIn {
                match frame.tag {
                    b'H' | b'S' => continue,
                    b'd' => {
                        return Read { used, message: Some(Ok(Frontend::CopyData(frame.body))) };
                    }
                    b'c' => {
                        self.mode = Mode::Query;
                        return Read { used, message: Some(Ok(Frontend::CopyDone)) };
                    }
                    _ => {}
                }
            } else {
                match frame.tag {
                    b'B' | b'P' | b'C' | b'D' | b'E' | b'H' => self.extended = true,
                    b'S' | b'X' => {
                        self.extended = false;
                        self.skip = false;
                    }
                    _ => self.extended = false,
                }
                if self.skip || matches!(frame.tag, b'd' | b'c' | b'f') {
                    continue;
                }
                if matches!(frame.tag, b'Q' | b'F' | b'S') {
                    self.ready = true;
                }
            }
            let message =
                if self.utf8 { Frontend::parse_utf8(frame) } else { Frontend::parse(frame) };
            return Read { used, message: Some(message) };
        }
    }

    /// Recovers from an error with severity `ERROR`, as the error handler of `PostgresMain` does. After an error in a message of the extended protocol, the session drops messages until the next `Sync`. After any other error, it wants `ReadyForQuery`. A copy that was running ends.
    ///
    /// It gives the `FATAL` error that the server must send next when the error came in the middle of a message. The server then closes the connection.
    pub fn recover(&mut self) -> Option<ProtocolError> {
        self.mode = Mode::Query;
        if self.extended {
            self.skip = true;
        }
        if !self.skip {
            self.ready = true;
        }
        std::mem::take(&mut self.torn).then(ProtocolError::lost_sync)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::frame::PROTOCOL_3_0;

    fn bytes(messages: &[Frontend<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for message in messages {
            message.encode(&mut out);
        }
        out
    }

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// A small server: it answers each message the way the main loop of PostgreSQL does when every statement fails or succeeds with `OK`, and gives the type bytes of what it wrote.
    fn run(session: &mut Session, input: &[u8]) -> String {
        let mut out = OutBuf::new();
        let mut at = 0;
        loop {
            if session.wants_ready() {
                session.ready_for_query(TransactionStatus::Idle, &mut out);
            }
            let read = session.read(&input[at..]);
            at += read.used;
            let Some(message) = read.message else { break };
            match message {
                Ok(Frontend::Query(b"copy")) => {
                    out.copy_in_response(0, &[]);
                    session.copy_in();
                }
                Ok(Frontend::Query(b"fail")) | Ok(Frontend::Execute { .. }) => {
                    out.error_response(&[(b'S', b"ERROR")]);
                    session.recover();
                }
                Ok(Frontend::CopyDone) | Ok(Frontend::Query(_)) => out.command_complete(b"OK"),
                Ok(Frontend::CopyFail(_)) => {
                    out.error_response(&[(b'S', b"ERROR")]);
                    session.recover();
                }
                Ok(Frontend::Terminate) => break,
                Ok(_) => {}
                Err(error) => {
                    out.protocol_error(&error, PROTOCOL_3_0);
                    if error.level != Level::Error {
                        break;
                    }
                    if let Some(fatal) = session.recover() {
                        out.protocol_error(&fatal, PROTOCOL_3_0);
                        break;
                    }
                }
            }
        }
        let mut tags = String::new();
        let mut rest = out.as_bytes();
        while let Ok(Some((message, used))) = Backend::decode(rest) {
            let tag = match message {
                Backend::ErrorResponse(fields) => {
                    let severity = fields.iter().find(|f| f.0 == b'S').map(|f| f.1);
                    if severity == Some(b"FATAL") { "FATAL" } else { "E" }
                }
                Backend::ReadyForQuery(_) => "Z",
                Backend::CommandComplete(_) => "C",
                Backend::CopyInResponse { .. } => "G",
                _ => "?",
            };
            if !tags.is_empty() {
                tags.push(' ');
            }
            tags.push_str(tag);
            rest = &rest[used..];
        }
        tags
    }

    #[test]
    fn a_new_session_starts_with_ready_for_query() {
        let mut session = Session::new();
        assert!(session.wants_ready());
        assert_eq!(run(&mut session, &bytes(&[Frontend::Query(b"select")])), "Z C Z");
    }

    #[test]
    fn a_bad_simple_query_ends_with_ready_for_query() {
        // The oracle: `invalid string in message`, then `invalid message format`, each with ReadyForQuery.
        let input = [frame(b'Q', b"select 1"), frame(b'Q', b"select 1\0x")].concat();
        let mut session = Session::new();
        session.ready = false;
        let first = session.read(&input);
        assert_eq!(first.message.unwrap().unwrap_err().message, "invalid string in message");
        assert!(session.recover().is_none());
        assert!(session.wants_ready());
        let mut session = Session::new();
        assert_eq!(run(&mut session, &input), "Z E Z E Z");
    }

    #[test]
    fn an_extended_error_skips_to_sync() {
        // Flush with a body is an extended message, so the Query after it is dropped.
        let input =
            [frame(b'H', b"x"), bytes(&[Frontend::Query(b"select"), Frontend::Sync])].concat();
        assert_eq!(run(&mut Session::new(), &input), "Z E Z");
        let input = bytes(&[
            Frontend::Execute { portal: b"nope", max_rows: 0 },
            Frontend::Sync,
            Frontend::Sync,
        ]);
        assert_eq!(run(&mut Session::new(), &input), "Z E Z Z");
    }

    #[test]
    fn a_skipped_message_is_not_read() {
        let input = [
            bytes(&[Frontend::Execute { portal: b"", max_rows: 0 }]),
            frame(b'Q', b"no zero byte"),
            frame(b'D', b"Xbad"),
            bytes(&[Frontend::Sync]),
        ]
        .concat();
        assert_eq!(run(&mut Session::new(), &input), "Z E Z");
    }

    #[test]
    fn a_bad_type_byte_is_fatal_also_in_the_skip() {
        let input =
            [bytes(&[Frontend::Execute { portal: b"", max_rows: 0 }]), frame(b'z', b"")].concat();
        assert_eq!(run(&mut Session::new(), &input), "Z E FATAL");
    }

    #[test]
    fn a_sync_with_a_body_is_a_simple_error() {
        assert_eq!(run(&mut Session::new(), &frame(b'S', b"x")), "Z E Z");
    }

    #[test]
    fn copy_messages_after_the_copy_are_ignored() {
        let input = [
            frame(b'd', b"junk"),
            frame(b'c', b"zz"),
            frame(b'f', b"no zero byte"),
            bytes(&[Frontend::Query(b"select")]),
        ]
        .concat();
        assert_eq!(run(&mut Session::new(), &input), "Z C Z");
    }

    #[test]
    fn copy_in_ignores_flush_and_sync() {
        let input = [
            bytes(&[Frontend::Query(b"copy"), Frontend::CopyData(b"1\n"), Frontend::Sync]),
            frame(b'H', b"xx"),
            frame(b'c', b"body"),
        ]
        .concat();
        assert_eq!(run(&mut Session::new(), &input), "Z G C Z");
    }

    #[test]
    fn copy_fail_ends_the_copy() {
        let input = bytes(&[Frontend::Query(b"copy"), Frontend::CopyFail(b"boom")]);
        assert_eq!(run(&mut Session::new(), &input), "Z G E Z");
        // The string of CopyFail is checked as UTF-8 too.
        let input = [bytes(&[Frontend::Query(b"copy")]), frame(b'f', b"bo\xffom\0x")].concat();
        let mut session = Session::new();
        assert_eq!(run(&mut session, &input), "Z G E Z");
        assert!(!session.in_copy());
    }

    #[test]
    fn a_bad_type_byte_in_copy_in_loses_the_sync() {
        // The oracle: ERROR, then FATAL, and no ReadyForQuery between them.
        let input = bytes(&[Frontend::Query(b"copy"), Frontend::Query(b"select")]);
        assert_eq!(run(&mut Session::new(), &input), "Z G E FATAL");
    }

    #[test]
    fn an_error_in_copy_in_from_execute_skips_to_sync() {
        let mut session = Session::new();
        session.ready = false;
        let input = bytes(&[Frontend::Execute { portal: b"", max_rows: 0 }]);
        assert!(session.read(&input).message.unwrap().is_ok());
        session.copy_in();
        assert!(!session.wants_ready());
        assert!(session.recover().is_none());
        assert!(session.skipping());
        assert!(!session.wants_ready());
    }

    #[test]
    fn the_query_string_is_checked_before_the_end_of_the_message() {
        // The oracle gives 22021 and not `invalid message format`.
        let input = frame(b'Q', b"select '\xff'\0x");
        let mut session = Session::new();
        let error = session.read(&input).message.unwrap().unwrap_err();
        assert_eq!(error.sqlstate, "22021");
        session.set_utf8(false);
        let error = session.read(&input).message.unwrap().unwrap_err();
        assert_eq!(error.message, "invalid message format");
    }

    #[test]
    fn partial_input_waits_and_dropped_frames_are_used() {
        let mut session = Session::new();
        let input = bytes(&[Frontend::Query(b"select 1")]);
        let read = session.read(&input[..input.len() - 1]);
        assert_eq!(read, Read { used: 0, message: None });
        let input = [frame(b'd', b"x"), frame(b'Q', b"sel")].concat();
        let read = session.read(&input[..input.len() - 1]);
        assert_eq!(read, Read { used: 6, message: None });
    }
}

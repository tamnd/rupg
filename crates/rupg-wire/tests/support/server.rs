//! A small server on the codec of `rupg-wire`, for the two fuzz targets of the codec and for the test that replays their inputs.
//!
//! The targets in `fuzz/fuzz_targets` include this file and do not copy it, so the test and the targets make the same checks. A subdirectory of `tests/` is not a test target, so this compiles only as a part of what includes it.
//!
//! It does what `rupg-server` will do with the codec, with fake statements: the handshake, the main loop of `Session`, the statements and portals by name, the values of `Bind` one by one, and the recovery after an error. A query string chooses what a statement does: `copy` starts `COPY FROM STDIN`, `begin` and `commit` open and close a transaction block, a string with `fail` in it fails, and the rest succeed. A `Bind` value `bad` fails its conversion.
//!
//! Lifted from `crates/rudb-pgwire/tests/support/server.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_wire::{
    CancelKey, Frontend, Handshake, Level, Mode, OutBuf, Portals, ProtocolError, Session,
    Statements, Step, Target, TransactionStatus, cancel_target, split, split_startup,
};

/// What the server saw and did, in order, for the checks of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// The session gave a message with this type byte to the server.
    Got(u8),
    /// The server sent `ReadyForQuery`.
    Ready,
    /// The server sent an `ErrorResponse` with severity `ERROR`.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Startup,
    Main,
    Closed,
}

/// A portal with the rows that it has left.
#[derive(Debug)]
struct Portal {
    rows: i32,
}

pub(crate) struct Server {
    phase: Phase,
    handshake: Handshake,
    protocol: u32,
    session: Session,
    statements: Statements<usize>,
    portals: Portals<Portal>,
    block: bool,
    pub(crate) out: OutBuf,
    pub(crate) events: Vec<Event>,
}

impl Server {
    pub(crate) fn new() -> Server {
        Server {
            phase: Phase::Startup,
            handshake: Handshake::new(false),
            protocol: 0,
            session: Session::new(),
            statements: Statements::new(),
            portals: Portals::new(),
            block: false,
            out: OutBuf::new(),
            events: Vec::new(),
        }
    }

    pub(crate) fn closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// Acts on all the messages in `input` that are complete, and gives the number of bytes it used. The caller keeps the rest and calls again with more bytes after it.
    pub(crate) fn feed(&mut self, input: &[u8]) -> usize {
        let mut at = 0;
        loop {
            let (used, more) = match self.phase {
                Phase::Closed => return at,
                Phase::Startup => self.startup(&input[at..]),
                Phase::Main => self.turn(&input[at..]),
            };
            at += used;
            if more {
                return at;
            }
        }
    }

    /// One packet before the main loop. It gives the bytes used, and true when it needs more.
    fn startup(&mut self, input: &[u8]) -> (usize, bool) {
        let (packet, used) = match split_startup(input) {
            Ok(Some(packet)) => packet,
            Ok(None) => return (0, true),
            Err(error) => {
                self.fatal(&error, self.handshake.protocol());
                return (0, true);
            }
        };
        match self.handshake.packet(packet, &mut self.out) {
            // The check of bytes that came with the request depends on how the bytes arrived, so the targets do not make it.
            Ok(Step::Answer { .. }) => {}
            Ok(Step::Cancel(cancel)) => {
                let key = CancelKey::new(1, self.protocol, [7; 32]);
                let _ = cancel_target(&cancel, |pid| (pid == key.pid()).then_some(key));
                self.phase = Phase::Closed;
            }
            Ok(Step::Start(request)) => {
                self.protocol = request.protocol;
                self.out.authentication_ok();
                CancelKey::new(1, request.protocol, [7; 32]).write(&mut self.out);
                self.phase = Phase::Main;
            }
            Err(error) => self.fatal(&error, self.handshake.protocol()),
        }
        (used, false)
    }

    /// One turn of the main loop.
    fn turn(&mut self, input: &[u8]) -> (usize, bool) {
        if self.session.wants_ready() {
            let status =
                if self.block { TransactionStatus::Block } else { TransactionStatus::Idle };
            self.session.ready_for_query(status, &mut self.out);
            self.events.push(Event::Ready);
        }
        let mode = if self.session.in_copy() { Mode::CopyIn } else { Mode::Query };
        let read = self.session.read(input);
        let Some(message) = read.message else { return (read.used, true) };
        let result = message.and_then(|message| {
            round_trip(message, mode);
            self.events.push(Event::Got(message.tag()));
            self.act(message)
        });
        if let Err(error) = result {
            if error.level != Level::Error {
                self.fatal(&error, self.protocol);
            } else {
                self.out.protocol_error(&error, self.protocol);
                self.events.push(Event::Error);
                if let Some(fatal) = self.session.recover() {
                    self.fatal(&fatal, self.protocol);
                }
            }
        }
        (read.used, false)
    }

    /// The checks of the `pgwire_sequence` target, for a server that got only good messages.
    ///
    /// 1. The server sends one `ReadyForQuery` at the start, and then one for each `Query`, `FunctionCall` and `Sync` that it acts on, never two for one of them, and never one without one of them.
    /// 2. After an error in a message of the extended protocol, the server acts on no message other than `Sync` and `Terminate` until the next `Sync`.
    /// 3. At the end of the input the server owes no `ReadyForQuery`, unless a copy is running.
    ///
    /// They do not use the state of the session. They follow from the protocol.
    pub(crate) fn check_ready(&self) {
        let events = &self.events;
        // The first ReadyForQuery is owed from the start of the session.
        let mut owed = true;
        let mut skipping = false;
        let mut last = None;
        for &event in events {
            match event {
                Event::Ready => {
                    assert!(
                        owed,
                        "ReadyForQuery without a Query, FunctionCall or Sync: {events:?}"
                    );
                    owed = false;
                }
                Event::Got(tag) => {
                    if skipping {
                        assert!(matches!(tag, b'S' | b'X'), "a message in the skip: {events:?}");
                    }
                    if matches!(tag, b'Q' | b'F' | b'S') {
                        assert!(!owed, "a message before its ReadyForQuery was sent: {events:?}");
                        owed = true;
                        skipping = false;
                    }
                    last = Some(tag);
                }
                Event::Error => {
                    if matches!(last, Some(b'P' | b'B' | b'D' | b'E' | b'C' | b'H')) {
                        skipping = true;
                    }
                }
            }
        }
        if !self.closed() && !self.session.in_copy() {
            assert!(!owed, "the input ended with a ReadyForQuery owed: {events:?}");
        }
    }

    fn fatal(&mut self, error: &ProtocolError, protocol: u32) {
        if error.level == Level::Fatal {
            self.out.protocol_error(error, protocol);
        }
        self.phase = Phase::Closed;
    }

    fn act(&mut self, message: Frontend<'_>) -> Result<(), ProtocolError> {
        match message {
            Frontend::Query(sql) => {
                self.query(sql)?;
                if !self.block {
                    self.portals.clear();
                }
            }
            Frontend::Parse { name, sql, types } => {
                self.statements.start_parse(name);
                if contains(sql, b"fail") {
                    return Err(failed("syntax error"));
                }
                self.statements.insert(name, types.len())?;
                self.out.parse_complete();
            }
            Frontend::Bind(bind) => {
                let params = *self.statements.get(bind.statement)?;
                let mut values = bind.params(params)?;
                self.portals.make_room(bind.portal)?;
                for value in &mut values {
                    if value? == Some(&b"bad"[..]) {
                        return Err(failed("invalid input syntax"));
                    }
                }
                values.finish()?;
                self.portals.insert(bind.portal, Portal { rows: 3 });
                self.out.bind_complete();
            }
            Frontend::Describe { target: Target::Statement, name } => {
                let params = *self.statements.get(name)?;
                self.out.parameter_description(&vec![25; params]);
                self.out.no_data();
            }
            Frontend::Describe { target: Target::Portal, name } => {
                self.portals.get(name)?;
                self.out.no_data();
            }
            Frontend::Execute { portal, max_rows } => {
                let portal = self.portals.get_mut(portal)?;
                if max_rows > 0 && portal.rows > max_rows {
                    portal.rows -= max_rows;
                    self.out.portal_suspended();
                } else {
                    let rows = std::mem::take(&mut portal.rows);
                    self.out.command_complete(format!("SELECT {rows}").as_bytes());
                }
            }
            Frontend::Close { target, name } => {
                match target {
                    Target::Statement => drop(self.statements.close(name)),
                    Target::Portal => drop(self.portals.close(name)),
                }
                self.out.close_complete();
            }
            Frontend::Sync => {
                if !self.block {
                    self.portals.clear();
                }
            }
            Frontend::Terminate => self.phase = Phase::Closed,
            Frontend::CopyDone => self.out.command_complete(b"COPY 0"),
            Frontend::CopyFail(_) => return Err(failed("COPY from stdin failed")),
            Frontend::FunctionCall(call) => {
                let mut args = call.args(call.oid as usize % 3)?;
                for arg in &mut args {
                    arg?;
                }
                args.finish()?;
                self.out.function_call_response(None);
            }
            Frontend::Flush | Frontend::CopyData(_) | Frontend::Password(_) => {}
        }
        Ok(())
    }

    fn query(&mut self, sql: &[u8]) -> Result<(), ProtocolError> {
        match sql {
            b"" => self.out.empty_query_response(),
            b"copy" => {
                self.out.copy_in_response(0, &[]);
                self.session.copy_in();
            }
            b"begin" => {
                self.block = true;
                self.out.command_complete(b"BEGIN");
            }
            b"commit" => {
                self.block = false;
                self.out.command_complete(b"COMMIT");
            }
            _ if contains(sql, b"fail") => return Err(failed("division by zero")),
            _ => self.out.command_complete(b"SELECT 1"),
        }
        Ok(())
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// An error of a statement, which the server sends as it sends an error of the codec.
fn failed(message: &str) -> ProtocolError {
    ProtocolError {
        level: Level::Error,
        sqlstate: "XX000",
        message: message.to_owned(),
        detail: None,
        hint: None,
    }
}

/// A message that the session gave must come back the same from its own bytes.
fn round_trip(message: Frontend<'_>, mode: Mode) {
    let mut bytes = Vec::new();
    message.encode(&mut bytes);
    let frame = split(&bytes, mode).expect("an encoded message splits").expect("it is whole");
    assert_eq!(frame.size(), bytes.len());
    assert_eq!(Frontend::parse_utf8(frame), Ok(message));
}

/// The check of the `pgwire` target: the server writes the same bytes and uses the same input when it gets all of `input` at one time and when it gets it in chunks of `chunk` bytes. It gives the server of the run with all the bytes.
pub(crate) fn exercise(chunk: u8, input: &[u8]) -> Server {
    let (whole, used) = serve(input, input.len().max(1));
    let (pieces, used_in_pieces) = serve(input, usize::from(chunk % 16) + 1);
    assert_eq!(
        (whole.out.as_bytes(), used, whole.closed()),
        (pieces.out.as_bytes(), used_in_pieces, pieces.closed())
    );
    whole
}

/// Gives the input to a server `step` bytes at a time. It gives the server and the bytes that it used.
fn serve(input: &[u8], step: usize) -> (Server, usize) {
    let mut server = Server::new();
    let (mut used, mut have) = (0, 0);
    loop {
        have = (have + step).min(input.len());
        used += server.feed(&input[used..have]);
        assert!(used <= have);
        if server.closed() || have == input.len() {
            return (server, used);
        }
    }
}

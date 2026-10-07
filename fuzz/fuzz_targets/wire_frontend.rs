//! Build a valid sequence of client messages and check when the server sends `ReadyForQuery`.
//!
//! The other target starts at raw bytes, so most of its inputs end at the first bad length or type byte. This one writes only good messages, so the budget goes to the order of the messages: errors in the middle of a pipeline, a `Bind` to a statement that a `Close` removed, names that are the same in their first 63 bytes, a `COPY` that ends with `CopyFail`, a portal that `Execute` reads in parts.
//!
//! The checks are in `Server::check_ready`. They do not use the state of `Session`, and they follow from the protocol: one `ReadyForQuery` for each `Query`, `FunctionCall` and `Sync`, never two for one of them, and no message other than `Sync` and `Terminate` in the skip after an error in the extended protocol.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use rupg_wire::{Bind, Frontend, FunctionCall, PROTOCOL_3_0, PROTOCOL_3_2, Packet, Startup};

// The target uses only a part of the test server.
#[allow(dead_code)]
#[path = "../../crates/rupg-wire/tests/support/server.rs"]
mod server;

use server::Server;

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Name {
    Unnamed,
    A,
    B,
    /// Two long names that are the same in their first 63 bytes.
    LongA,
    LongB,
}

impl Name {
    fn bytes(self) -> Vec<u8> {
        match self {
            Name::Unnamed => Vec::new(),
            Name::A => b"a".to_vec(),
            Name::B => b"b".to_vec(),
            Name::LongA => [&[b'n'; 63][..], b"a"].concat(),
            Name::LongB => [&[b'n'; 63][..], b"b"].concat(),
        }
    }
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Sql {
    Select,
    Fail,
    Copy,
    Begin,
    Commit,
    Empty,
}

impl Sql {
    fn bytes(self) -> &'static [u8] {
        match self {
            Sql::Select => b"select 1",
            Sql::Fail => b"select fail",
            Sql::Copy => b"copy",
            Sql::Begin => b"begin",
            Sql::Commit => b"commit",
            Sql::Empty => b"",
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Message {
    Query(Sql),
    Parse { name: Name, sql: Sql, params: u8 },
    Bind { portal: Name, statement: Name, values: Vec<Option<bool>>, binary: bool },
    DescribeStatement(Name),
    DescribePortal(Name),
    Execute { portal: Name, max_rows: u8 },
    CloseStatement(Name),
    ClosePortal(Name),
    Sync,
    Flush,
    CopyData,
    CopyDone,
    CopyFail,
    FunctionCall { oid: u8, args: u8 },
}

#[derive(Arbitrary, Debug)]
struct Input {
    protocol_3_2: bool,
    messages: Vec<Message>,
    terminate: bool,
}

fuzz_target!(|input: Input| {
    let mut bytes = Vec::new();
    let version = if input.protocol_3_2 { PROTOCOL_3_2 } else { PROTOCOL_3_0 };
    Packet::Startup(Startup { version, options: b"user\0fuzz\0\0" }).encode(&mut bytes);
    for message in &input.messages {
        encode(message, &mut bytes);
    }
    if input.terminate {
        Frontend::Terminate.encode(&mut bytes);
    }
    let mut server = Server::new();
    // A message other than the copy messages in a copy is fatal, so the server can stop early.
    let used = server.feed(&bytes);
    assert!(used == bytes.len() || server.closed());
    server.check_ready();
});

fn encode(message: &Message, out: &mut Vec<u8>) {
    match *message {
        Message::Query(sql) => Frontend::Query(sql.bytes()).encode(out),
        Message::Parse { name, sql, params } => {
            let types = rupg_wire::encode_oids(&vec![0; usize::from(params % 4)]);
            let types = rupg_wire::Oids::from_bytes(&types);
            Frontend::Parse { name: &name.bytes(), sql: sql.bytes(), types }.encode(out);
        }
        Message::Bind { portal, statement, ref values, binary } => {
            let values: Vec<_> = values
                .iter()
                .take(4)
                .map(|value| value.map(|good| if good { &b"1"[..] } else { &b"bad"[..] }))
                .collect();
            let formats: &[i16] = if binary { &[1] } else { &[] };
            Bind::encode(out, &portal.bytes(), &statement.bytes(), formats, &values, &[]);
        }
        Message::DescribeStatement(name) => {
            let target = rupg_wire::Target::Statement;
            Frontend::Describe { target, name: &name.bytes() }.encode(out);
        }
        Message::DescribePortal(name) => {
            let target = rupg_wire::Target::Portal;
            Frontend::Describe { target, name: &name.bytes() }.encode(out);
        }
        Message::Execute { portal, max_rows } => {
            let max_rows = i32::from(max_rows % 4);
            Frontend::Execute { portal: &portal.bytes(), max_rows }.encode(out);
        }
        Message::CloseStatement(name) => {
            let target = rupg_wire::Target::Statement;
            Frontend::Close { target, name: &name.bytes() }.encode(out);
        }
        Message::ClosePortal(name) => {
            let target = rupg_wire::Target::Portal;
            Frontend::Close { target, name: &name.bytes() }.encode(out);
        }
        Message::Sync => Frontend::Sync.encode(out),
        Message::Flush => Frontend::Flush.encode(out),
        Message::CopyData => Frontend::CopyData(b"1\n").encode(out),
        Message::CopyDone => Frontend::CopyDone.encode(out),
        Message::CopyFail => Frontend::CopyFail(b"stop").encode(out),
        Message::FunctionCall { oid, args } => {
            let args = vec![Some(&b"x"[..]); usize::from(args % 3)];
            FunctionCall::encode(out, u32::from(oid), &[], &args, 0);
        }
    }
}

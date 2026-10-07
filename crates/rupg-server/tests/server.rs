//! Runs the server on a free port of the loopback address and talks to it as a client.

use std::io::{Read, Write};
use std::sync::Arc;

use rupg_platform::os::{OsEntropy, OsNet, OsTasks};
use rupg_platform::{Net, Stream};
use rupg_server::{Config, Server};
use rupg_wire::{Backend, PROTOCOL_3_0};

fn server() -> Server {
    let config = Config {
        listen: "127.0.0.1:0".into(),
        settings: vec![("work_mem".into(), "16MB".into())],
        ..Config::default()
    };
    Server::start(&config, Arc::new(OsNet), Arc::new(OsTasks), Arc::new(OsEntropy)).unwrap()
}

/// A `StartupMessage` with the parameters.
fn startup(params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
    for (name, value) in params {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
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

/// Reads messages until `ReadyForQuery` or the end of the stream, and gives one short line for each message.
fn read(stream: &mut dyn Stream) -> Vec<String> {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let mut input = Vec::new();
    let mut lines = Vec::new();
    loop {
        while let Some((message, size)) = Backend::decode(&input).unwrap() {
            let (line, ready) = match message {
                Backend::Authentication(_) => ("auth".to_owned(), false),
                Backend::ParameterStatus { name, value } => {
                    (format!("{}={}", text(name), text(value)), false)
                }
                Backend::BackendKeyData { pid, .. } => (format!("key {}", pid > 1000), false),
                Backend::ReadyForQuery(status) => {
                    (format!("ready {}", char::from(status.byte())), true)
                }
                Backend::CommandComplete(tag) => (text(tag), false),
                Backend::RowDescription(fields) => (format!("columns {}", fields.len()), false),
                Backend::DataRow(values) => {
                    let values: Vec<String> = values.iter().map(|v| text(v.unwrap())).collect();
                    (format!("row {}", values.join(",")), false)
                }
                Backend::ErrorResponse(fields) => {
                    let field = |code| text(fields.iter().find(|(c, _)| *c == code).unwrap().1);
                    (format!("{} {} {}", field(b'S'), field(b'C'), field(b'M')), false)
                }
                other => (format!("{other:?}"), false),
            };
            input.drain(..size);
            lines.push(line);
            if ready {
                return lines;
            }
        }
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return lines;
        }
        input.extend_from_slice(&buf[..n]);
    }
}

fn connect(server: &Server, params: &[(&str, &str)]) -> (Box<dyn Stream>, Vec<String>) {
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&startup(params)).unwrap();
    let lines = read(&mut *stream);
    (stream, lines)
}

#[test]
fn a_session() {
    let server = server();
    let (mut stream, lines) = connect(&server, &[("user", "postgres"), ("database", "postgres")]);
    assert_eq!(lines[0], "auth");
    assert!(lines.contains(&"session_authorization=postgres".to_owned()), "{lines:?}");
    assert_eq!(lines[lines.len() - 2..], ["key true", "ready I"]);
    stream.write_all(&query("SHOW work_mem; BEGIN")).unwrap();
    assert_eq!(read(&mut *stream), ["columns 1", "row 16MB", "SHOW", "BEGIN", "ready T"]);
    stream.write_all(&query("SELECT 1")).unwrap();
    assert_eq!(read(&mut *stream), ["ERROR 0A000 SELECT is not supported yet", "ready E"]);
    stream.write_all(&query("ROLLBACK")).unwrap();
    assert_eq!(read(&mut *stream), ["ROLLBACK", "ready I"]);
    stream.write_all(&message(b'X', b"")).unwrap();
    assert!(read(&mut *stream).is_empty());
    server.stop().unwrap();
}

#[test]
fn refused_connections() {
    let server = server();
    let cases = [
        (("bob", "postgres"), "FATAL 28000 role \"bob\" does not exist"),
        (("postgres", "nope"), "FATAL 3D000 database \"nope\" does not exist"),
        (
            ("postgres", "template0"),
            "FATAL 55000 database \"template0\" is not currently accepting connections",
        ),
    ];
    for ((user, database), error) in cases {
        let (_, lines) = connect(&server, &[("user", user), ("database", database)]);
        assert_eq!(lines, ["auth", error]);
    }
    let (_, lines) = connect(&server, &[("user", "postgres"), ("client_encoding", "LATIN1")]);
    assert_eq!(
        lines,
        ["auth", "FATAL 0A000 invalid value for parameter \"client_encoding\": \"LATIN1\""]
    );
    server.stop().unwrap();
}

#[test]
fn an_ssl_request() {
    let server = server();
    let mut stream = OsNet.connect(server.address()).unwrap();
    stream.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    let mut answer = [0];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    stream.write_all(&startup(&[("user", "postgres")])).unwrap();
    assert_eq!(read(&mut *stream).last().unwrap(), "ready I");
    drop(stream);
    server.stop().unwrap();
}

#[cfg(unix)]
#[test]
fn a_unix_socket() {
    let server = server();
    let port = server.address().rsplit_once(':').unwrap().1.to_owned();
    let path = format!("/tmp/.s.PGSQL.{port}");
    assert_eq!(server.sockets(), std::slice::from_ref(&path));
    let mut stream = OsNet.connect(&path).unwrap();
    stream.write_all(&startup(&[("user", "postgres")])).unwrap();
    assert_eq!(read(&mut *stream).last().unwrap(), "ready I");
    stream
        .write_all(&query("SHOW unix_socket_directories; SHOW port; SHOW listen_addresses"))
        .unwrap();
    let port_row = format!("row {port}");
    assert_eq!(
        read(&mut *stream),
        [
            "columns 1",
            "row /tmp",
            "SHOW",
            "columns 1",
            &port_row,
            "SHOW",
            "columns 1",
            "row 127.0.0.1",
            "SHOW",
            "ready I"
        ]
    );
    drop(stream);
    server.stop().unwrap();
    assert!(OsNet.connect(&path).is_err());
}

#[test]
fn socket_directories() {
    let start = |dirs: &str| {
        let config = Config {
            listen: "127.0.0.1:0".into(),
            settings: vec![("unix_socket_directories".into(), dirs.into())],
            ..Config::default()
        };
        Server::start(&config, Arc::new(OsNet), Arc::new(OsTasks), Arc::new(OsEntropy))
    };
    let server = start("").unwrap();
    assert!(server.sockets().is_empty());
    server.stop().unwrap();
    let error = start("/tmp,").unwrap_err();
    assert_eq!(error.message(), "invalid list syntax in parameter \"unix_socket_directories\"");
    let error = start("tmp").unwrap_err();
    assert_eq!(error.detail(), Some("The directory \"tmp\" is not an absolute path."));
}

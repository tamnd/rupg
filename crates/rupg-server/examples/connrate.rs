//! The connection rate test of spec/06 section 6.18. `cargo xtask connrate` builds and runs this program.
//!
//! Usage: `connrate <rupg> <dir> [--seconds N] [--clients N] [--postgres <data dir>]`. The program starts `<rupg> serve` with the files of the server in `<dir>`. For each variant, `--clients` threads (4 by default) connect, run the startup up to the first `ReadyForQuery`, send `Terminate` and wait for the server to close the connection, again and again for `--seconds` seconds (10 by default). The program prints the connections in each second and the CPU time of the server for each connection, from `utime` and `stime` of `/proc/<pid>/stat`. The variants are trust over a Unix socket, and SCRAM-SHA-256 over TLS with the iteration count of the server, 4,096 by default.
//!
//! The client does the PBKDF2 of the password once for each salt and keeps the key, so the number shows the work of the server and not of the client. The client does not resume a TLS session, so each connection has a full handshake, as with libpq.
//!
//! `--postgres` measures the same two variants on a PostgreSQL server that runs with the data directory `<data dir>`, as the user `postgres`: trust over its Unix socket, and SCRAM over TLS on 127.0.0.1 at its port. The server must have `ssl = on` with the certificate `server.crt` of `tests/tls`, a rule `hostssl ... 127.0.0.1/32 scram-sha-256` and the password `secret` for `postgres`. The CPU time of PostgreSQL is the time of the postmaster, of its children, and of the children that ended (`cutime` and `cstime`). The program fails when rupg makes fewer than 10 times the connections of PostgreSQL in a variant. It also prints how many times the server CPU of rupg the server CPU of PostgreSQL is for each connection. The clients run on the same machine as the server, so on a machine with few cores the CPU of the clients can limit the rate of rupg before the rate of PostgreSQL.
//!
//! The program reads `/proc`, so it runs on Linux only.

// The program measures another process and talks to it as a client. It is not a part of the engine, so it uses std and not the traits of rupg-platform.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[allow(dead_code, reason = "the test does not connect over TCP without TLS")]
mod client;

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use client::{Way, connect, message, postmaster, serve, stat_fields, tls_client, tree};
use rustls::ClientConfig;

/// The target of spec/06 section 6.18: the connections of rupg in a second, over the connections of PostgreSQL.
const TARGET: f64 = 10.0;
/// The clock ticks in a second of the times in `/proc/<pid>/stat`. Linux gives them in units of `USER_HZ`, which is 100.
const TICKS: f64 = 100.0;
/// The time that the server gets to end the sessions of a measurement before the program reads its CPU time.
const SETTLE: Duration = Duration::from_millis(500);

/// The ways to connect that the test measures.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    Trust,
    Scram,
}

impl Variant {
    const ALL: [Variant; 2] = [Variant::Trust, Variant::Scram];

    fn name(self) -> &'static str {
        match self {
            Variant::Trust => "trust, Unix socket",
            Variant::Scram => "SCRAM, TLS",
        }
    }
}

/// The options of the command line.
#[derive(Debug)]
struct Options {
    rupg: PathBuf,
    dir: PathBuf,
    seconds: u64,
    clients: usize,
    postgres: Option<PathBuf>,
}

fn options() -> Result<Options, String> {
    const USAGE: &str =
        "usage: connrate <rupg> <dir> [--seconds N] [--clients N] [--postgres <data dir>]";
    let mut args = std::env::args().skip(1);
    let rupg = args.next().ok_or(USAGE)?.into();
    let dir = args.next().ok_or(USAGE)?.into();
    let mut options = Options { rupg, dir, seconds: 10, clients: 4, postgres: None };
    while let Some(arg) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let count = || value.parse::<usize>().ok().filter(|n| *n > 0).ok_or(USAGE);
        match arg.as_str() {
            "--seconds" => options.seconds = u64::try_from(count()?).map_err(|_| USAGE)?,
            "--clients" => options.clients = count()?,
            "--postgres" => options.postgres = Some(value.clone().into()),
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

/// The CPU time of the processes `pids` in clock ticks: `utime`, `stime`, `cutime` and `cstime`. A process that ended counts as 0.
fn cpu(pids: &[u32]) -> u64 {
    let mut total = 0;
    for pid in pids {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // The state is the first field after the name, and `utime` to `cstime` are the fields 11 to 14 after it.
        let fields = stat_fields(&stat);
        total += fields.iter().skip(11).take(4).filter_map(|f| f.parse::<u64>().ok()).sum::<u64>();
    }
    total
}

/// The result of a measurement.
#[derive(Debug)]
struct Rate {
    /// Connections in each second.
    per_second: f64,
    /// Microseconds of the CPU time of the server for each connection.
    cpu: f64,
}

/// Opens a session, ends it with `Terminate`, and waits for the server to close it. The server closes first, so the client does not keep its port in `TIME_WAIT`.
fn once(way: Way<'_>, tls: &Arc<ClientConfig>) -> Result<(), String> {
    let mut session = connect(way, tls, "postgres")?;
    session
        .write_all(&message(b'X', b""))
        .and_then(|()| session.flush())
        .map_err(|e| format!("could not send Terminate: {e}"))?;
    let mut buf = [0; 256];
    // The end of the stream, or an error when the server closes the socket with no TLS alert.
    while matches!(session.read(&mut buf), Ok(n) if n > 0) {}
    Ok(())
}

/// Runs the clients for `seconds` seconds against the server with the processes of `root`.
fn measure(options: &Options, root: u32, way: Way<'_>) -> Result<Rate, String> {
    let tls = tls_client()?;
    // The first session loads what the server loads once, and the client makes the key of SCRAM.
    once(way, &tls)?;
    thread::sleep(SETTLE);
    let before = cpu(&tree(root));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(options.seconds);
    let counts: Vec<Result<u64, String>> = thread::scope(|scope| {
        let clients: Vec<_> = (0..options.clients)
            .map(|_| {
                scope.spawn(|| {
                    let mut count = 0u64;
                    while Instant::now() < deadline {
                        once(way, &tls).map_err(|e| format!("connection {}: {e}", count + 1))?;
                        count += 1;
                    }
                    Ok(count)
                })
            })
            .collect();
        clients
            .into_iter()
            .map(|client| client.join().unwrap_or_else(|_| Err("a client panicked".to_owned())))
            .collect()
    });
    let elapsed = start.elapsed().as_secs_f64();
    thread::sleep(SETTLE);
    let after = cpu(&tree(root));
    let count: u64 = counts.into_iter().sum::<Result<u64, String>>()?;
    let count = f64::from(u32::try_from(count).unwrap_or(u32::MAX));
    let ticks = f64::from(u32::try_from(after.saturating_sub(before)).unwrap_or(u32::MAX));
    Ok(Rate { per_second: count / elapsed, cpu: ticks / TICKS * 1e6 / count.max(1.0) })
}

fn print(server: &str, variant: Variant, rate: &Rate) {
    println!(
        "{server:<10} {:<20} {:>9.0} connections/s {:>9.1} us of server CPU for each",
        variant.name(),
        rate.per_second,
        rate.cpu
    );
}

fn run(options: &Options) -> Result<bool, String> {
    let hba = "local all all trust\nhostssl all all 127.0.0.1/32 scram-sha-256\n";
    let server = serve(&options.rupg, &options.dir, hba)?;
    let root = server.child.id();
    let mut rupg = Vec::new();
    for variant in Variant::ALL {
        let way = match variant {
            Variant::Trust => Way::Unix(&server.socket),
            Variant::Scram => Way::Tls(&server.tcp),
        };
        let rate = measure(options, root, way)?;
        print("rupg", variant, &rate);
        rupg.push(rate);
    }
    drop(server);
    let Some(data) = &options.postgres else { return Ok(true) };
    let postgres = postmaster(data)?;
    let mut pass = true;
    for (variant, mine) in Variant::ALL.into_iter().zip(&rupg) {
        let way = match variant {
            Variant::Trust => Way::Unix(&postgres.socket),
            Variant::Scram => Way::Tls(&postgres.tcp),
        };
        let rate = measure(options, postgres.pid, way)?;
        print("postgres", variant, &rate);
        let times = mine.per_second / rate.per_second;
        let cpu = rate.cpu / mine.cpu;
        let ok = times >= TARGET;
        println!(
            "connrate: {}: rupg makes {times:.1} times the connections of postgres, the target is {TARGET:.0}: {}. postgres uses {cpu:.1} times the server CPU of rupg for each connection.",
            variant.name(),
            if ok { "pass" } else { "FAIL" }
        );
        pass &= ok;
    }
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
            eprintln!("connrate: {message}");
            ExitCode::FAILURE
        }
    }
}

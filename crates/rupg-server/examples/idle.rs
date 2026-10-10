//! The idle session test of spec/06 section 6.4.3 and spec/21 section 21.13. `cargo xtask idle` builds and runs this program.
//!
//! Usage: `idle <rupg> <dir> [--sessions N] [--postgres <data dir>] [--postgres-sessions N]`. The program starts `<rupg> serve` once for each variant, with the files of the server in `<dir>`. It opens and closes one session first, so that the work that the server does once is not in the number. Then it opens the sessions of the variant, keeps them idle, and takes the increase of the proportional set size of the server process (`Pss` of `/proc/<pid>/smaps_rollup`). The server runs in its own process, so the memory of the clients is not in the number. The variants are trust over a Unix socket, trust over TCP, trust over TLS, SCRAM over TLS with `DISCARD ALL` after the startup, and trust over a Unix socket with a statement that the session prepares and drops after the startup, as spec/21 section 21.13 repeats the test. The program prints the size of one session for each variant and fails when the largest is over 64 KiB.
//!
//! `--postgres` measures a PostgreSQL server that runs with the data directory `<data dir>`, as the user `postgres` with trust over its Unix socket. The number is the increase of the postmaster and all its children. The server must allow the sessions in `max_connections`.
//!
//! The program reads `/proc`, so it runs on Linux only.

// The program measures another process and talks to it as a client. It is not a part of the engine, so it uses std and not the traits of rupg-platform.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[allow(dead_code, reason = "the test does not use the TCP address of PostgreSQL")]
mod client;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use client::{Session, Way, connect, message, postmaster, serve, talk, tls_client, tree};
use rustls::ClientConfig;

/// The limit of spec/06 section 6.4.1, in KiB.
const LIMIT: f64 = 64.0;
/// The time that the server gets to settle before each measurement. It is longer than the 1 second after which an idle session of rupg gives back the memory of its last statement.
const SETTLE: Duration = Duration::from_secs(3);

/// The ways to connect that the test measures.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    Unix,
    Tcp,
    Tls,
    Scram,
    Prepared,
}

impl Variant {
    const ALL: [Variant; 5] =
        [Variant::Unix, Variant::Tcp, Variant::Tls, Variant::Scram, Variant::Prepared];

    fn name(self) -> &'static str {
        match self {
            Variant::Unix => "trust, Unix socket",
            Variant::Tcp => "trust, TCP",
            Variant::Tls => "trust, TLS",
            Variant::Scram => "SCRAM, TLS, DISCARD ALL",
            Variant::Prepared => "trust, Unix, PREPARE, DEALLOCATE",
        }
    }

    fn hba(self) -> &'static str {
        match self {
            Variant::Unix | Variant::Prepared => "local all all trust\n",
            Variant::Tcp => "host all all 127.0.0.1/32 trust\n",
            Variant::Tls => "hostssl all all 127.0.0.1/32 trust\n",
            Variant::Scram => "hostssl all all 127.0.0.1/32 scram-sha-256\n",
        }
    }
}

/// The options of the command line.
#[derive(Debug)]
struct Options {
    rupg: PathBuf,
    dir: PathBuf,
    sessions: usize,
    postgres: Option<PathBuf>,
    postgres_sessions: usize,
}

fn options() -> Result<Options, String> {
    const USAGE: &str =
        "usage: idle <rupg> <dir> [--sessions N] [--postgres <data dir>] [--postgres-sessions N]";
    let mut args = std::env::args().skip(1);
    let rupg = args.next().ok_or(USAGE)?.into();
    let dir = args.next().ok_or(USAGE)?.into();
    let mut options = Options { rupg, dir, sessions: 1000, postgres: None, postgres_sessions: 90 };
    while let Some(arg) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let count = || value.parse::<usize>().ok().filter(|n| *n > 0).ok_or(USAGE);
        match arg.as_str() {
            "--sessions" => options.sessions = count()?,
            "--postgres" => options.postgres = Some(value.clone().into()),
            "--postgres-sessions" => options.postgres_sessions = count()?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

/// The sum of `Pss` of the processes `pids`, in KiB. A process that ended counts as 0.
fn pss(pids: &[u32]) -> u64 {
    let mut total = 0;
    for pid in pids {
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) else { continue };
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Pss:") {
                total += rest.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    total
}

/// Opens one session of `variant` and runs its startup.
fn open(
    variant: Variant,
    socket: &str,
    tcp: &str,
    tls: &Arc<ClientConfig>,
) -> Result<Box<dyn Session>, String> {
    let way = match variant {
        Variant::Unix | Variant::Prepared => Way::Unix(socket),
        Variant::Tcp => Way::Tcp(tcp),
        Variant::Tls | Variant::Scram => Way::Tls(tcp),
    };
    let mut session = connect(way, tls, "postgres")?;
    match variant {
        Variant::Scram => talk(&mut *session, &message(b'Q', b"DISCARD ALL\0"))?,
        Variant::Prepared => {
            talk(&mut *session, &message(b'Q', b"PREPARE s AS SELECT relname FROM pg_class\0"))?;
            talk(&mut *session, &message(b'Q', b"DEALLOCATE s\0"))?;
        }
        Variant::Unix | Variant::Tcp | Variant::Tls => {}
    }
    Ok(session)
}

/// Opens `count` sessions and gives the increase of the memory of the processes of `root`, in KiB for each session. One session opens and closes first, so the work that the server does once, such as the load of the shared catalog at the first statement, is in the memory before.
fn measure(
    count: usize,
    root: u32,
    mut open: impl FnMut() -> Result<Box<dyn Session>, String>,
) -> Result<f64, String> {
    drop(open()?);
    thread::sleep(SETTLE);
    let before = pss(&tree(root));
    let sessions = (0..count)
        .map(|i| open().map_err(|e| format!("session {}: {e}", i + 1)))
        .collect::<Result<Vec<_>, String>>()?;
    thread::sleep(SETTLE);
    let after = pss(&tree(root));
    drop(sessions);
    let increase = after.saturating_sub(before);
    Ok(f64::from(u32::try_from(increase).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(count).unwrap_or(u32::MAX)))
}

fn run(options: &Options) -> Result<bool, String> {
    let tls = tls_client()?;
    let mut largest: f64 = 0.0;
    for variant in Variant::ALL {
        let server = serve(&options.rupg, &options.dir, variant.hba())?;
        let root = server.child.id();
        let size =
            measure(options.sessions, root, || open(variant, &server.socket, &server.tcp, &tls))?;
        println!(
            "rupg       {:<32} {size:>8.1} KiB for each of {} sessions",
            variant.name(),
            options.sessions
        );
        largest = largest.max(size);
    }
    if let Some(data) = &options.postgres {
        let postgres = postmaster(data)?;
        let count = options.postgres_sessions;
        let size =
            measure(count, postgres.pid, || open(Variant::Unix, &postgres.socket, "", &tls))?;
        println!(
            "postgres   {:<32} {size:>8.1} KiB for each of {count} sessions",
            Variant::Unix.name()
        );
    }
    let pass = largest <= LIMIT;
    println!(
        "idle: the largest is {largest:.1} KiB, the limit is {LIMIT:.0} KiB: {}",
        if pass { "pass" } else { "FAIL" }
    );
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
            eprintln!("idle: {message}");
            ExitCode::FAILURE
        }
    }
}

//! `rupg-crash`: runs the T8 crash test over a range of seeds until it has tried a number of distinct crash files.
//!
//! `cargo run --release -p rupg-crash -- --files 1000000 --threads 8` is the M1 run of spec/21 section 21.9.4. The runner owns the process, so it uses the threads and the clock of the operating system.
//!
//! Two more modes run the same workload on a real file system for the tests of spec/21 section 21.9.3. See tests/realdisk/README.md.
//!
//! - `rupg-crash workload --file F --seed S --steps N [--mark CMD]` runs the workload on the file `F`. Before each change it prints `pending <n> <digest>`, and after the change is acknowledged it prints `ack <n> <digest>`. The digest is the checksum of the tables and rows. If `--mark` is given, the shell runs `CMD` after each acknowledged change, with `{}` replaced by `ack<n>`.
//! - `rupg-crash verify --file F --expect D1,D2` opens the file, which runs recovery, and runs `rupg check`. The digest of the tables and rows must be one of the given digests.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use rupg::Platform;
use rupg_crash::{Config, Counts, Event, digest, run, verify, workload};

const USAGE: &str =
    "usage: rupg-crash [--files N] [--threads N] [--first-seed N] [--steps N] [--sample N]
       rupg-crash workload --file F --seed S --steps N [--mark CMD]
       rupg-crash verify --file F --expect D1,D2";

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("workload") => real_workload(&args[1..]),
        Some("verify") => real_verify(&args[1..]),
        _ => simulated(&args),
    }
}

/// The value of each `--name value` pair, or `None` if an argument is not in a pair.
fn pairs(args: &[String]) -> Option<Vec<(&str, &str)>> {
    let pairs: Vec<_> =
        args.chunks(2).map(|p| (p[0].as_str(), p.get(1).map(String::as_str))).collect();
    pairs.into_iter().map(|(k, v)| v.map(|v| (k, v))).collect()
}

fn real_workload(args: &[String]) -> ExitCode {
    let (mut file, mut seed, mut steps, mut mark) = (None, None, None, None);
    for (k, v) in pairs(args).unwrap_or_default() {
        match k {
            "--file" => file = Some(v),
            "--seed" => seed = v.parse::<u64>().ok(),
            "--steps" => steps = v.parse::<u32>().ok(),
            "--mark" => mark = Some(v),
            _ => return usage(),
        }
    }
    let (Some(file), Some(seed), Some(steps)) = (file, seed, steps) else {
        return usage();
    };
    let mut n = 0u64;
    let mut out = std::io::stdout().lock();
    let result = workload(&Platform::os(), Path::new(file), seed, steps, |event| {
        match event {
            Event::Pending(model) => {
                writeln!(out, "pending {} {:016x}", n, digest(model)).map_err(|e| e.to_string())?;
            }
            Event::Acked { model, .. } => {
                writeln!(out, "ack {} {:016x}", n, digest(model)).map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                if let Some(cmd) = mark {
                    let cmd = cmd.replace("{}", &format!("ack{n}"));
                    let status = Command::new("sh").arg("-c").arg(&cmd).status();
                    if !status.is_ok_and(|s| s.success()) {
                        return Err(format!("the mark command failed: {cmd}"));
                    }
                }
                n += 1;
            }
            Event::Step | Event::End => {}
        }
        Ok(())
    });
    match result {
        Ok(()) => {
            println!("end {n}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rupg-crash workload: FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_verify(args: &[String]) -> ExitCode {
    let (mut file, mut expect) = (None, None);
    for (k, v) in pairs(args).unwrap_or_default() {
        match k {
            "--file" => file = Some(v),
            "--expect" => expect = Some(v),
            _ => return usage(),
        }
    }
    let (Some(file), Some(expect)) = (file, expect) else {
        return usage();
    };
    match verify(&Platform::os(), Path::new(file)) {
        Ok(d) if expect.split(',').any(|e| e == format!("{d:016x}")) => {
            println!("ok {d:016x}");
            ExitCode::SUCCESS
        }
        Ok(d) => {
            eprintln!(
                "rupg-crash verify: FAIL: the tables have digest {d:016x}, expected one of {expect}"
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("rupg-crash verify: FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The T8 run on the simulated disk.
fn simulated(args: &[String]) -> ExitCode {
    let mut files = 10_000u64;
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut first = 0u64;
    let mut steps = 200u32;
    let mut sample = 16usize;
    for pair in args.chunks(2) {
        let value = pair.get(1).and_then(|v| v.parse::<u64>().ok());
        match (pair[0].as_str(), value) {
            ("--files", Some(v)) => files = v,
            ("--threads", Some(v)) => threads = v.max(1) as usize,
            ("--first-seed", Some(v)) => first = v,
            ("--steps", Some(v)) => steps = v as u32,
            ("--sample", Some(v)) => sample = v as usize,
            _ => return usage(),
        }
    }

    let start = Instant::now();
    let next = AtomicU64::new(first);
    let total = Mutex::new(Counts::default());
    let failure = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let done = total.lock().unwrap_or_else(PoisonError::into_inner).files;
                    let failed = failure.lock().unwrap_or_else(PoisonError::into_inner).is_some();
                    if done >= files || failed {
                        return;
                    }
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    match run(Config { seed, steps, sample }) {
                        Ok(c) => {
                            let mut t = total.lock().unwrap_or_else(PoisonError::into_inner);
                            *t += c;
                            println!(
                                "seed {seed}: {} files, {} sync points, {} commits; total {} files in {:.0} s",
                                c.files,
                                c.sync_points,
                                c.commits,
                                t.files,
                                start.elapsed().as_secs_f64()
                            );
                        }
                        Err(e) => {
                            *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                        }
                    }
                }
            });
        }
    });

    let t = total.into_inner().unwrap_or_else(PoisonError::into_inner);
    let seeds = next.into_inner() - first;
    println!(
        "rupg-crash: {} distinct crash files from {} sync points of {seeds} seeds, {} commits, {} plans gave a file that an earlier plan gave, {:.0} s",
        t.files,
        t.sync_points,
        t.commits,
        t.same,
        start.elapsed().as_secs_f64()
    );
    match failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(e) => {
            eprintln!("rupg-crash: FAIL: {e}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}

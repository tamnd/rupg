//! `rupg-crash`: runs the T8 crash test over a range of seeds until it has tried a number of distinct crash files.
//!
//! `cargo run --release -p rupg-crash -- --files 1000000 --threads 8` is the M1 run of spec/21 section 21.9.4. The runner owns the process, so it uses the threads and the clock of the operating system.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use rupg_crash::{Config, Counts, run};

const USAGE: &str =
    "usage: rupg-crash [--files N] [--threads N] [--first-seed N] [--steps N] [--sample N]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
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

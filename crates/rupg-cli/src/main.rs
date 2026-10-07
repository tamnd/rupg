//! The `rupg` binary: `serve`, `check`, `strip`, `upgrade`, `import`, `dump`.
//!
//! The commands arrive with the milestones of `spec/23-milestones.md`. At M1 the binary prints its version and its build configuration, and `rupg check FILE` checks a database file.

#![forbid(unsafe_code)]

use std::process::ExitCode;

const USAGE: &str = "usage: rupg [--version | --print-config | --help | check FILE]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("rupg {} (PostgreSQL {} compatible)", rupg::VERSION, rupg::POSTGRES_PIN);
            ExitCode::SUCCESS
        }
        Some("--print-config") => {
            print!("{}", rupg::build_config());
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("check") => match &args[1..] {
            [file] => check(file),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::from(2)
            }
        },
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Some(other) => {
            eprintln!("rupg: unknown argument {other:?}");
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `rupg check FILE`. It prints the counts and exits with 0, or prints the error and exits with 1.
fn check(file: &str) -> ExitCode {
    match rupg::check(file) {
        Ok(r) => {
            println!("{file}: no errors");
            println!("pages: {}", r.pages);
            println!("tables: {}", r.tables);
            println!("rows: {}", r.rows);
            println!("log blocks: {}", r.log_blocks);
            println!("log bytes: {}", r.log_bytes);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rupg check: {file}: {e}");
            ExitCode::FAILURE
        }
    }
}

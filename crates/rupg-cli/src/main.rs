//! The `rupg` binary: `serve`, `check`, `strip`, `upgrade`, `import`, `dump`.
//!
//! The commands arrive with the milestones of `spec/23-milestones.md`. At M0 the binary prints its version and its build configuration.

#![forbid(unsafe_code)]

use std::process::ExitCode;

const USAGE: &str = "usage: rupg [--version | --print-config | --help]";

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

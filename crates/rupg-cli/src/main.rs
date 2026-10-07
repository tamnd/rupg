//! The `rupg` binary: `serve`, `check`, `strip`, `upgrade`, `import`, `dump`.
//!
//! The commands arrive with the milestones of `spec/23-milestones.md`. At M1 the binary prints its version and its build configuration, and `rupg check FILE` checks a database file. At M2 `rupg serve` runs the PostgreSQL server.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "usage: rupg [--version | --print-config | --help | check FILE | serve [--listen ADDR] [--pwfile FILE] [-c name=value]...]";

/// The server log on the standard error, one message after the other, as PostgreSQL writes it with `log_destination = stderr`.
#[derive(Debug)]
struct Stderr;

impl rupg::server::Log for Stderr {
    fn write(&self, severity: &str, text: &str) {
        eprintln!("{severity}:  {text}");
    }
}

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
        Some("serve") => match serve_config(&args[1..]) {
            Ok(config) => serve(&config),
            Err(message) => {
                eprintln!("rupg serve: {message}");
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

/// The options of `rupg serve`: `--listen ADDR`, `--pwfile FILE` with the password of the superuser, and `-c name=value` for each setting, as `postgres -c` takes them.
fn serve_config(args: &[String]) -> Result<rupg::server::Config, String> {
    let mut config = rupg::server::Config { log: Some(Arc::new(Stderr)), ..Default::default() };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let setting = match arg.as_str() {
            "--listen" => {
                config.listen = args.next().ok_or("--listen requires an address")?.clone();
                continue;
            }
            "--pwfile" => {
                let file = args.next().ok_or("--pwfile requires a file")?;
                let password =
                    rupg::server::read_password(file).map_err(|e| e.message().to_owned())?;
                config.password = Some(password);
                continue;
            }
            "-c" => args.next().ok_or("-c requires a value")?.as_str(),
            arg => match arg.strip_prefix("-c").or_else(|| arg.strip_prefix("--listen=")) {
                Some(rest) if arg.starts_with("--listen=") => {
                    config.listen = rest.to_owned();
                    continue;
                }
                Some(rest) => rest,
                None => return Err(format!("unknown argument {arg:?}")),
            },
        };
        let (name, value) =
            setting.split_once('=').ok_or(format!("-c {setting} requires a value"))?;
        let name = name.replace('-', "_");
        // PostgreSQL reads a relative path of a file setting from its data directory. rupg has no data directory yet, so the path is relative to the current directory.
        let value = match name.as_str() {
            "hba_file" | "ident_file" | "ssl_cert_file" | "ssl_key_file" | "ssl_ca_file"
            | "ssl_crl_file"
                if !value.is_empty() =>
            {
                std::path::absolute(value)
                    .map_err(|e| format!("could not find the absolute path of \"{value}\": {e}"))?
                    .to_string_lossy()
                    .into_owned()
            }
            _ => value.to_owned(),
        };
        config.settings.push((name, value));
    }
    Ok(config)
}

/// `rupg serve`. It runs until the process ends.
fn serve(config: &rupg::server::Config) -> ExitCode {
    let server = match rupg::server::serve(config) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("rupg serve: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("rupg serve: listening on {}", server.address());
    for socket in server.sockets() {
        eprintln!("rupg serve: listening on Unix socket \"{socket}\"");
    }
    match server.wait() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rupg serve: {e}");
            ExitCode::FAILURE
        }
    }
}

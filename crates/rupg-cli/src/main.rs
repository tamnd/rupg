//! The `rupg` binary: `serve`, `check`, `strip`, `upgrade`, `import`, `dump`.
//!
//! The commands arrive with the milestones of `spec/23-milestones.md`. At M1 the binary prints its version and its build configuration, `rupg check FILE` checks a database file, and `rupg upgrade OLD NEW` copies a database file to a new file in the format of this release. At M2 `rupg serve` runs the PostgreSQL server.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "usage: rupg [--version | --print-config | --help | check FILE | upgrade OLD NEW | serve [-D DIR] [--listen ADDR] [--pwfile FILE] [-c name=value]...]";

/// The server log on the standard error, one message after the other, as PostgreSQL writes it with `log_destination = stderr`.
#[derive(Debug)]
struct Stderr;

impl rupg::server::Log for Stderr {
    fn write(&self, severity: &str, text: &str) {
        eprintln!("{severity}:  {text}");
    }
}

/// The allocator of the system, with the counts of the bytes of each thread for `EXPLAIN (MEMORY)`.
#[global_allocator]
static ALLOCATOR: rupg::Counting = rupg::Counting;

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
        Some("upgrade") => match &args[1..] {
            [from, to] => upgrade(from, to),
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

/// `rupg upgrade OLD NEW`. It prints the counts and exits with 0, or prints the error and exits with 1. The old file does not change.
fn upgrade(from: &str, to: &str) -> ExitCode {
    match rupg::upgrade(from, to) {
        Ok(r) => {
            println!("{from} -> {to}: upgraded");
            println!("tables: {}", r.tables);
            println!("rows: {}", r.rows);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rupg upgrade: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The absolute path of `path`, relative to `base` or else to the current directory.
fn absolute(path: &str, base: Option<&str>) -> Result<String, String> {
    let path = match base {
        Some(base) => std::path::Path::new(base).join(path),
        None => std::path::PathBuf::from(path),
    };
    Ok(std::path::absolute(&path)
        .map_err(|e| format!("could not find the absolute path of \"{}\": {e}", path.display()))?
        .to_string_lossy()
        .into_owned())
}

/// The options of `rupg serve`: `-D DIR` with the configuration files, `--listen ADDR`, `--pwfile FILE` with the password of the superuser, and `-c name=value` for each setting, as `postgres -D` and `postgres -c` take them.
fn serve_config(args: &[String]) -> Result<rupg::server::Config, String> {
    let mut config = rupg::server::Config { log: Some(Arc::new(Stderr)), ..Default::default() };
    // `-D` comes first, because the paths of the other settings are relative to it.
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        let dir = match arg.as_str() {
            "-D" => words.next().ok_or("-D requires a directory")?.as_str(),
            arg => match arg.strip_prefix("-D") {
                Some(dir) if !dir.is_empty() => dir,
                _ => continue,
            },
        };
        config.data_directory = Some(absolute(dir, None)?);
    }
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let setting = match arg.as_str() {
            "-D" => {
                args.next();
                continue;
            }
            arg if arg.starts_with("-D") => continue,
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
        // PostgreSQL makes `config_file` and `data_directory` absolute from the current directory, and reads the other files from its data directory, or else from the current directory.
        let value = match name.as_str() {
            "config_file" | "data_directory" if !value.is_empty() => absolute(value, None)?,
            "hba_file" | "ident_file" | "ssl_cert_file" | "ssl_key_file" | "ssl_ca_file"
            | "ssl_crl_file"
                if !value.is_empty() =>
            {
                absolute(value, config.data_directory.as_deref())?
            }
            _ => value.to_owned(),
        };
        config.settings.push((name, value));
    }
    Ok(config)
}

/// `rupg serve`. It runs until the process ends. A checkpoint writes the data to `data.rupg` each `checkpoint_timeout`, and `SIGTERM` or `SIGINT` writes it and ends the process.
fn serve(config: &rupg::server::Config) -> ExitCode {
    // The mask must be in place before the server starts its threads, so that only the thread of the signals takes them.
    #[cfg(unix)]
    let signals = match rupg::server::StopSignals::block() {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("rupg serve: {e}");
            return ExitCode::FAILURE;
        }
    };
    // `ChangeToDataDir`: a relative path in the configuration file, such as `ssl_cert_file = 'server.crt'`, is in the data directory.
    if let Some(dir) = &config.data_directory
        && let Err(e) = std::env::set_current_dir(dir)
    {
        eprintln!("rupg serve: could not change directory to \"{dir}\": {e}");
        return ExitCode::FAILURE;
    }
    let server = match rupg::server::serve(config) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("rupg serve: {e}");
            return ExitCode::FAILURE;
        }
    };
    for address in server.addresses() {
        eprintln!("rupg serve: listening on {address}");
    }
    for socket in server.sockets() {
        eprintln!("rupg serve: listening on Unix socket \"{socket}\"");
    }
    checkpoints(&server);
    #[cfg(unix)]
    stop_on_signal(&server, signals);
    match server.wait() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rupg serve: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The checkpointer: a thread that writes the image of the data each `checkpoint_timeout`.
#[allow(clippy::disallowed_methods)]
fn checkpoints(server: &rupg::server::Server) {
    let store = server.store();
    let every = server.checkpoint_timeout();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(every);
            if let Err(e) = store.checkpoint() {
                eprintln!("LOG:  checkpoint failed: {e}");
            }
        }
    });
}

/// The thread of the stop signals. `SIGTERM` and `SIGINT` write the image of the data and end the process. The sessions end with the process, so a transaction in progress does not commit.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
fn stop_on_signal(server: &rupg::server::Server, signals: rupg::server::StopSignals) {
    let store = server.store();
    std::thread::spawn(move || {
        let signal = signals.wait();
        eprintln!("LOG:  received the signal {signal}, shutting down");
        let code = match store.checkpoint() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("LOG:  checkpoint failed: {e}");
                1
            }
        };
        std::process::exit(code);
    });
}

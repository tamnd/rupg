//! Build and check tasks for the rupg workspace.
//!
//! These checks read the whole tree, so they are not unit tests. They run through cargo, so they work the same on Linux, macOS and Windows. See `spec/22-crate-layout.md` section 22.8.

#![forbid(unsafe_code)]
// xtask is a build tool and not part of the engine, so it uses the file system directly. See clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod bench;
mod dat;
mod errcodes;
mod grammar;
mod guc;
mod layers;
mod lifts;
mod pgcatalog;
mod pgtype;
mod sha256;
mod sql;
mod style;
mod tzdata;
mod unicode;
mod vendor;
mod version;
mod waitevents;

const USAGE: &str = "usage: cargo xtask <bench [--binary file] [--save file] [--check file] | errcodes [--check] | grammar [file.y...] | guc [--check] | layers | lifts [rudb dir] | pgcatalog [--check] | pgtype [--check] | sql [--check | --bison] | style | tzdata [--check] | unicode [--check] | vendor [--check] [dir...] | version <x.y.z> | waitevents [--check]>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str);
    let root = root();
    let result = match task {
        Some("bench") => bench::run(&root, &args[1..]),
        Some("errcodes") => errcodes::run(&root, &args[1..]),
        Some("grammar") => grammar::run(&root, &args[1..]),
        Some("pgcatalog") => pgcatalog::run(&root, &args[1..]),
        Some("pgtype") => pgtype::run(&root, &args[1..]),
        Some("guc") => guc::run(&root, &args[1..]),
        Some("layers") => layers::check(&root),
        Some("lifts") => lifts::run(&root, &args[1..]),
        Some("sql") => sql::run(&root, &args[1..]),
        Some("style") => style::check(&root),
        Some("tzdata") => tzdata::run(&root, &args[1..]),
        Some("unicode") => unicode::run(&root, &args[1..]),
        Some("vendor") => vendor::run(&root, &args[1..]),
        Some("version") => version::run(&root, &args[1..]),
        Some("waitevents") => waitevents::run(&root, &args[1..]),
        Some(other) => Err(format!("unknown task {other:?}\n{USAGE}")),
        None => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root, one level above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is inside the workspace")
        .to_path_buf()
}

/// Reads a file, or gives an error that names the file.
pub(crate) fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

/// The crate directories under `crates/`, sorted by name.
pub(crate) fn crate_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join("crates");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// The last part of a path, as a string.
pub(crate) fn name_of(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string()
}

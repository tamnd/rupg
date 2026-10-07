//! Build and check tasks for the rupg workspace.
//!
//! These checks read the whole tree, so they are not unit tests. They run through cargo, so they work the same on Linux, macOS and Windows. See `spec/22-crate-layout.md` section 22.8.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod layers;
mod style;

const USAGE: &str = "usage: cargo xtask <layers | style>";

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let root = root();
    let result = match task.as_deref() {
        Some("layers") => layers::check(&root),
        Some("style") => style::check(&root),
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

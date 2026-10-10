//! The idle session test of spec/06 section 6.4.3 and spec/21 section 21.13.
//!
//! The task builds the binary `rupg` and the example `idle` of rupg-server in release mode, and runs the example in a new directory under the temporary directory. The example starts the server, opens the idle sessions of each variant, and fails when one session costs more than 64 KiB. `--debug` builds in the dev profile, which uses more memory, so its number is an upper bound. The other options go to the example: `--sessions N`, `--postgres <data dir>` and `--postgres-sessions N`. The example reads `/proc`, so the task runs on Linux only.

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let debug = args.iter().any(|arg| arg == "--debug");
    let rest: Vec<&String> = args.iter().filter(|arg| *arg != "--debug").collect();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let profile: &[&str] = if debug { &[] } else { &["--release"] };
    for target in [
        &["-p", "rupg-cli", "--bin", "rupg"][..],
        &["-p", "rupg-server", "--features", "tls", "--example", "idle"][..],
    ] {
        let status = Command::new(&cargo)
            .current_dir(root)
            .arg("build")
            .args(profile)
            .args(target)
            .status()
            .map_err(|e| format!("could not run cargo: {e}"))?;
        if !status.success() {
            return Err(format!("the build of {} failed", target.join(" ")));
        }
    }
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let out = target.join(if debug { "debug" } else { "release" });
    let dir = std::env::temp_dir().join(format!("rupg-idle-{}", std::process::id()));
    let status = Command::new(out.join("examples/idle"))
        .arg(out.join("rupg"))
        .arg(&dir)
        .args(rest)
        .status()
        .map_err(|e| format!("could not run the idle example: {e}"));
    let _ = std::fs::remove_dir_all(&dir);
    if status?.success() { Ok(()) } else { Err("idle: the test failed".to_string()) }
}

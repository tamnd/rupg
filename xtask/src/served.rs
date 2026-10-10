//! The tests that measure `rupg serve` as a process: the idle session test of spec/06 section 6.4.3 and spec/21 section 21.13 (`idle`), and the connection rate test of spec/06 section 6.18 (`connrate`).
//!
//! Each task builds the binary `rupg` and the example of rupg-server with the name of the task in release mode, and runs the example in a new directory under the temporary directory. `--debug` builds in the dev profile. A debug build uses more memory and more CPU, so its numbers are bounds. The other options go to the example. The `idle` example fails when one session costs more than 64 KiB, and it takes `--sessions N`, `--postgres <data dir>` and `--postgres-sessions N`. The `connrate` example fails when rupg makes fewer than 10 times the connections of PostgreSQL, and it takes `--seconds N`, `--clients N` and `--postgres <data dir>`. The examples read `/proc`, so the tasks run on Linux only.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds and runs the example `name`.
pub(crate) fn run(root: &Path, name: &str, args: &[String]) -> Result<(), String> {
    let debug = args.iter().any(|arg| arg == "--debug");
    let rest: Vec<&String> = args.iter().filter(|arg| *arg != "--debug").collect();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let profile: &[&str] = if debug { &[] } else { &["--release"] };
    for target in [
        &["-p", "rupg-cli", "--bin", "rupg"][..],
        &["-p", "rupg-server", "--features", "tls", "--example", name][..],
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
    let dir = std::env::temp_dir().join(format!("rupg-{name}-{}", std::process::id()));
    let status = Command::new(out.join("examples").join(name))
        .arg(out.join("rupg"))
        .arg(&dir)
        .args(rest)
        .status()
        .map_err(|e| format!("could not run the example {name}: {e}"));
    let _ = std::fs::remove_dir_all(&dir);
    if status?.success() { Ok(()) } else { Err(format!("{name}: the test failed")) }
}

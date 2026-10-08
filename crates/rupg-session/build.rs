//! Gives the crate the target triple and the version of the compiler, for the text of `version()` (spec/05 section 5.7).

use std::env;
use std::process::Command;

fn main() {
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    // `rustc --version` gives `rustc 1.90.0 (1159e78c4 2025-09-14)`. The second word is the version.
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.split_whitespace().nth(1).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=RUPG_TARGET={target}");
    println!("cargo:rustc-env=RUPG_RUSTC={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");
}

//! `cargo xtask version <x.y.z>` sets the workspace version and every exact internal pin (spec/22 section 22.8).
//!
//! The version is in one place, `[workspace.package]` in the root `Cargo.toml`. Each internal crate in `[workspace.dependencies]` has an exact pin `=x.y.z`, so a partial publish cannot mix versions. This task changes all of them together and then updates `Cargo.lock`, because the release workflow builds with `--locked`.

use std::path::Path;
use std::process::Command;

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let [new] = args else {
        return Err("usage: cargo xtask version <x.y.z>".to_string());
    };
    if !is_version(new) {
        return Err(format!("{new:?} is not a version of the form x.y.z"));
    }
    let path = root.join("Cargo.toml");
    let text = crate::read(&path)?;
    let (changed, old, pins) = set(&text, new)?;
    if old == *new {
        println!("version: the workspace is already at {new}");
        return Ok(());
    }
    std::fs::write(&path, changed)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;

    // Only the workspace members change in the lock file, so no network is necessary.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .args(["update", "--workspace", "--offline", "--quiet"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("could not run cargo update: {e}"))?;
    if !status.success() {
        return Err("cargo update --workspace failed".to_string());
    }
    println!("version: {old} to {new}, with {pins} internal pins and Cargo.lock");
    Ok(())
}

fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

/// Gives the new text of the root `Cargo.toml`, the old version, and the number of pins that changed.
fn set(text: &str, new: &str) -> Result<(String, String, usize), String> {
    let mut section = "";
    let mut old: Option<String> = None;
    let mut pins = 0;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = match trimmed {
                "[workspace.package]" => "package",
                "[workspace.dependencies]" => "dependencies",
                _ => "",
            };
        }
        if section == "package"
            && let Some(value) =
                trimmed.strip_prefix("version = \"").and_then(|v| v.strip_suffix('"'))
        {
            old = Some(value.to_string());
            out.push_str(&line.replacen(value, new, 1));
            continue;
        }
        if section == "dependencies" && trimmed.contains("path = \"crates/") {
            let Some(old) = &old else {
                return Err(
                    "[workspace.dependencies] comes before the version in [workspace.package]"
                        .to_string(),
                );
            };
            let pin = format!("version = \"={old}\"");
            if !line.contains(&pin) {
                return Err(format!(
                    "this internal dependency has no exact pin at {old}: {trimmed}"
                ));
            }
            out.push_str(&line.replacen(&pin, &format!("version = \"={new}\""), 1));
            pins += 1;
            continue;
        }
        out.push_str(line);
    }
    let old = old.ok_or("Cargo.toml has no version in [workspace.package]")?;
    Ok((out, old, pins))
}

#[cfg(test)]
mod tests {
    use super::{is_version, set};

    const TOML: &str = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\n# A comment.\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\nrupg = { version = \"=0.0.0\", path = \"crates/rupg\" }\nrupg-wal = { version = \"=0.0.0\", path = \"crates/rupg-wal\" }\nlibc = \"0.2\"\n";

    #[test]
    fn sets_the_version_and_the_pins() {
        let (text, old, pins) = set(TOML, "0.1.0").unwrap();
        assert_eq!(old, "0.0.0");
        assert_eq!(pins, 2);
        assert!(text.contains("\nversion = \"0.1.0\"\n"));
        assert!(text.contains("rupg = { version = \"=0.1.0\", path = \"crates/rupg\" }"));
        assert!(text.contains("rupg-wal = { version = \"=0.1.0\", path = \"crates/rupg-wal\" }"));
        assert!(text.contains("libc = \"0.2\""));
        assert_eq!(text.len(), TOML.len());
    }

    #[test]
    fn a_pin_that_does_not_match_is_an_error() {
        let toml =
            TOML.replace("rupg-wal = { version = \"=0.0.0\"", "rupg-wal = { version = \"0.0\"");
        assert!(set(&toml, "0.1.0").is_err());
    }

    #[test]
    fn version_form() {
        assert!(is_version("0.0.1"));
        assert!(is_version("1.10.0"));
        assert!(!is_version("0.1"));
        assert!(!is_version("v0.1.0"));
        assert!(!is_version("0.01.0"));
        assert!(!is_version("0.1.0-rc1"));
    }
}

//! The layer rule.
//!
//! `xtask/layers.toml` gives each crate a rank. A crate may depend only on crates of a lower rank. This check reads the ranks and each `Cargo.toml` and compares them. See `spec/22-crate-layout.md` section 22.3.
//!
//! The parser is line based. We write both files, and they keep a simple shape. If that stops being true, add a TOML dependency here. Do not make this parser more clever.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{crate_dirs, name_of, read};

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let ranks = ranks(&read(&root.join("xtask/layers.toml"))?)?;
    let mut problems = Vec::new();
    let mut checked = 0usize;
    let dirs = crate_dirs(root)?;

    for dir in &dirs {
        let name = name_of(dir);
        let Some(&rank) = ranks.get(&name) else {
            problems.push(format!("{name} has no rank in xtask/layers.toml"));
            continue;
        };
        for dep in internal_dependencies(&read(&dir.join("Cargo.toml"))?) {
            match ranks.get(&dep) {
                None => problems.push(format!("{name} depends on {dep}, which has no rank")),
                Some(&dep_rank) if dep_rank >= rank => problems.push(format!(
                    "{name} at rank {rank} depends on {dep} at rank {dep_rank}, which is not lower"
                )),
                Some(_) => {}
            }
        }
        checked += 1;
    }

    for name in ranks.keys() {
        if !dirs.iter().any(|d| &name_of(d) == name) {
            problems.push(format!("xtask/layers.toml ranks {name}, which is not a crate"));
        }
    }

    if problems.is_empty() {
        println!("layers: the rule holds for {checked} crates");
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        Err(format!("layers: {} violations", problems.len()))
    }
}

/// Reads the `[ranks]` table: lines of the form `"rupg-file" = 11`.
fn ranks(text: &str) -> Result<BTreeMap<String, u32>, String> {
    let mut ranks = BTreeMap::new();
    let mut seen = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("layers.toml line {}: expected name = rank", number + 1));
        };
        let key = key.trim().trim_matches('"').to_string();
        let rank: u32 = value
            .trim()
            .parse()
            .map_err(|_| format!("layers.toml line {}: the rank is not a number", number + 1))?;
        if let Some(other) = seen.insert(rank, key.clone()) {
            return Err(format!("layers.toml: {key} and {other} have the same rank {rank}"));
        }
        ranks.insert(key, rank);
    }
    Ok(ranks)
}

/// The names of the `rupg` crates in every dependency table of a manifest, including the dev and build tables.
fn internal_dependencies(manifest: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line.ends_with("dependencies]");
            continue;
        }
        if !in_deps || line.starts_with('#') {
            continue;
        }
        let key = line.split(['=', '.']).next().unwrap_or_default().trim().trim_matches('"');
        if key == "rupg" || key.starts_with("rupg-") {
            deps.push(key.to_string());
        }
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ranks() {
        let r = ranks("# c\n[ranks]\n\"rupg-common\" = 0\n\"rupg-file\" = 11\n").unwrap();
        assert_eq!(r["rupg-file"], 11);
    }

    #[test]
    fn rejects_a_shared_rank() {
        assert!(ranks("\"a\" = 1\n\"b\" = 1\n").is_err());
    }

    #[test]
    fn finds_internal_dependencies() {
        let m = "[dependencies]\nrupg-common.workspace = true\nlibc = \"1\"\n\n[dev-dependencies]\nrupg = { workspace = true }\n[lints]\nworkspace = true\n";
        assert_eq!(internal_dependencies(m), ["rupg-common", "rupg"]);
    }
}

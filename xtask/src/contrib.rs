//! The files of the contrib extensions that rupg ships, from `vendor/postgres-19/contrib`.
//!
//! `cargo xtask contrib [--check]` writes `crates/rupg-contrib/src/generated/files.rs`: for each extension directory, a constant with the name and the text of each file, the control files and the scripts, unchanged and in the order of their names. A published crate cannot read a file outside its directory, so the texts are in the crate. See `spec/16-procedures-and-extensions.md` section 16.7.

use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const INPUT: &str = "vendor/postgres-19/contrib";
const OUTPUT: &str = "crates/rupg-contrib/src/generated/files.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("contrib: unknown argument {other:?}"));
    }
    let text = files(&root.join(INPUT))?;
    let file = root.join(OUTPUT);
    if std::fs::read_to_string(&file).unwrap_or_default() == text {
        println!("contrib: {OUTPUT} matches {INPUT}");
        return Ok(());
    }
    if check {
        return Err(format!("contrib: {OUTPUT} is not up to date, run `cargo xtask contrib`"));
    }
    std::fs::write(&file, text).map_err(|e| format!("could not write {OUTPUT}: {e}"))?;
    println!("contrib: wrote {OUTPUT}");
    Ok(())
}

/// The names of the entries of a directory, sorted.
fn names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| crate::name_of(&e.path()))
        .collect();
    names.sort();
    Ok(names)
}

/// The text of the generated file.
fn files(input: &Path) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("//! `cargo xtask contrib` makes this file from `vendor/postgres-19/contrib`. Do not edit it.\n\n");
    for extension in names(input)? {
        let dir = input.join(&extension);
        if !dir.is_dir() {
            continue;
        }
        let constant = extension.to_ascii_uppercase();
        let _ = writeln!(out, "/// The files of `contrib/{extension}`.");
        let _ = writeln!(out, "pub(crate) const {constant}: &[(&str, &str)] = &[");
        for file in names(&dir)? {
            if !(file.ends_with(".control") || file.ends_with(".sql")) {
                continue;
            }
            let text = read(&dir.join(&file))?;
            let mut hashes = String::from("#");
            while text.contains(&format!("\"{hashes}")) {
                hashes.push('#');
            }
            let _ = writeln!(out, "    (\"{file}\", r{hashes}\"{text}\"{hashes}),");
        }
        out.push_str("];\n\n");
    }
    out.pop();
    Ok(out)
}

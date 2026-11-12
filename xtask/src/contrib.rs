//! The files of the contrib extensions that rupg ships, from `vendor/postgres-19/contrib`.
//!
//! `cargo xtask contrib [--check]` writes `crates/rupg-contrib/src/generated/files.rs`: for each extension directory, a constant with the name and the text of each file, the control files and the scripts, unchanged and in the order of their names. A published crate cannot read a file outside its directory, so the texts are in the crate. See `spec/16-procedures-and-extensions.md` section 16.7.
//!
//! The extension `vector` comes from `vendor/pgvector`. Its constant has the control file and `sql/vector.sql` with the name `vector--VERSION.sql` that the Makefile of pgvector installs it with, for the `default_version` of the control file.

use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const INPUT: &str = "vendor/postgres-19/contrib";
/// The release of pgvector.
const PGVECTOR: &str = "vendor/pgvector";
const OUTPUT: &str = "crates/rupg-contrib/src/generated/files.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("contrib: unknown argument {other:?}"));
    }
    let mut text = files(&root.join(INPUT))?;
    text.push_str(&pgvector(&root.join(PGVECTOR))?);
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
            entry(&mut out, &file, &read(&dir.join(&file))?);
        }
        out.push_str("];\n\n");
    }
    out.pop();
    Ok(out)
}

/// One item of a constant: the name and the text of a file as a raw string, in the form that `rustfmt` gives it, so that `cargo fmt --check` and `cargo xtask contrib --check` both pass.
fn entry(out: &mut String, file: &str, text: &str) {
    let mut hashes = String::from("#");
    while text.contains(&format!("\"{hashes}")) {
        hashes.push('#');
    }
    let _ =
        writeln!(out, "    (\n        \"{file}\",\n        r{hashes}\"{text}\"{hashes},\n    ),");
}

/// The constant of the extension `vector`, from the release of pgvector.
fn pgvector(input: &Path) -> Result<String, String> {
    let control = read(&input.join("vector.control"))?;
    let version = control
        .lines()
        .find_map(|line| line.strip_prefix("default_version"))
        .and_then(|rest| rest.trim_start().strip_prefix('='))
        .map(|v| v.trim().trim_matches('\'').to_string())
        .ok_or("contrib: vector.control has no default_version")?;
    let script = read(&input.join("sql/vector.sql"))?;
    let mut out = String::new();
    out.push_str("\n/// The files of the extension `vector`, from `vendor/pgvector`.\n");
    out.push_str("pub(crate) const VECTOR: &[(&str, &str)] = &[\n");
    entry(&mut out, "vector.control", &control);
    entry(&mut out, &format!("vector--{version}.sql"), &script);
    out.push_str("];\n");
    Ok(out)
}

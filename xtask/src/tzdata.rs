//! The names of the time zones.
//!
//! `cargo xtask tzdata` makes `crates/rupg-types/src/generated/tznames.rs` from `vendor/tzdata/src/timezone/data/tzdata.zi`, the compact form of the IANA data that the PostgreSQL pin carries. Each `Z` line names a zone and each `L` line names a link, and PostgreSQL takes both names in `TimeZone`. `cargo xtask tzdata --check` writes nothing and fails if a file is not up to date. See `spec/07-sql-types-and-catalog.md`.
//!
//! It also copies `tzdata.zi` and the `Default` set of the time zone abbreviations into `crates/rupg-types/src/generated/`, because `rupg-types` reads them with `include_str!`. A published crate has only the files of its own directory, so it cannot read them in `vendor/`.

use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const INPUT: &str = "vendor/tzdata/src/timezone/data/tzdata.zi";
const OUTPUT: &str = "crates/rupg-types/src/generated/tznames.rs";

/// The vendored files that `rupg-types` reads with `include_str!`, and the path of the copy of each.
const COPIES: [(&str, &str); 2] = [
    (INPUT, "crates/rupg-types/src/generated/tzdata.zi"),
    (
        "vendor/postgres-19/src/timezone/tznames/Default",
        "crates/rupg-types/src/generated/tznames_default.txt",
    ),
];

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("tzdata: unknown argument {other:?}"));
    }
    let mut files = vec![(OUTPUT, names(&read(&root.join(INPUT))?)?)];
    for (from, to) in COPIES {
        files.push((to, read(&root.join(from))?));
    }
    for (path, text) in files {
        let file = root.join(path);
        if std::fs::read_to_string(&file).unwrap_or_default() == text {
            println!("tzdata: {path} is up to date");
            continue;
        }
        if check {
            return Err(format!("tzdata: {path} is not up to date, run `cargo xtask tzdata`"));
        }
        std::fs::write(&file, text).map_err(|e| format!("could not write {path}: {e}"))?;
        println!("tzdata: wrote {path}");
    }
    Ok(())
}

/// The file with the names of the zones and the links, sorted by the name in lower case.
fn names(zi: &str) -> Result<String, String> {
    let version = zi
        .lines()
        .find_map(|l| l.strip_prefix("# version "))
        .ok_or("tzdata.zi has no version line")?;
    let mut names = Vec::new();
    for line in zi.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("Z") => names.push(words.next().ok_or_else(|| format!("tzdata.zi: {line:?}"))?),
            Some("L") => names.push(words.nth(1).ok_or_else(|| format!("tzdata.zi: {line:?}"))?),
            _ => {}
        }
    }
    names.sort_by_key(|n| n.to_ascii_lowercase());
    if names.windows(2).any(|w| w[0].eq_ignore_ascii_case(w[1])) {
        return Err("tzdata.zi: two names differ only in case".to_string());
    }
    let mut out = String::new();
    writeln!(out, "//! The names of the zones and the links of the IANA time zone data {version}.")
        .unwrap();
    out.push_str("//!\n//! `cargo xtask tzdata` makes this file from `vendor/tzdata/src/timezone/data/tzdata.zi`. Do not edit it.\n\n");
    out.push_str("/// Every name, sorted by the name in lower case.\n");
    writeln!(out, "pub(crate) static ZONE_NAMES: [&str; {}] = [", names.len()).unwrap();
    for name in names {
        writeln!(out, "    {name:?},").unwrap();
    }
    out.push_str("];\n");
    Ok(out)
}

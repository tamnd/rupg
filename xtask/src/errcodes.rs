//! The SQLSTATE list.
//!
//! `cargo xtask errcodes` makes `crates/rupg-common/src/generated/sqlstate.rs` from `vendor/postgres-19/src/backend/utils/errcodes.txt`. `cargo xtask errcodes --check` writes nothing and fails if the file is not up to date. See `spec/04-architecture.md` section 4.8.
//!
//! Lifted from `xtask/src/postgres.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const INPUT: &str = "vendor/postgres-19/src/backend/utils/errcodes.txt";
const OUTPUT: &str = "crates/rupg-common/src/generated/sqlstate.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("errcodes: unknown argument {other:?}"));
    }
    let text = sqlstate(&read(&root.join(INPUT))?)?;
    let file = root.join(OUTPUT);
    if std::fs::read_to_string(&file).unwrap_or_default() == text {
        println!("errcodes: {OUTPUT} matches {INPUT}");
        return Ok(());
    }
    if check {
        return Err(format!("errcodes: {OUTPUT} is not up to date, run `cargo xtask errcodes`"));
    }
    std::fs::write(&file, text).map_err(|e| format!("could not write {OUTPUT}: {e}"))?;
    println!("errcodes: wrote {OUTPUT}");
    Ok(())
}

/// One code line of `errcodes.txt`.
struct Code<'a> {
    state: &'a str,
    category: &'a str,
    name: &'a str,
    condition: &'a str,
}

/// Makes the SQLSTATE list from the text of `errcodes.txt`.
///
/// The file has a section line for each class and a code line for each code. A code line has the code, the category (`S`, `W` or `E`), the C macro name and the PL/pgSQL condition name. Six lines have no condition name. They give a second macro name to a code that is already in the file.
fn sqlstate(text: &str) -> Result<String, String> {
    let mut classes = Vec::new();
    let mut codes = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix("Section: Class ") {
            let Some((class, title)) = section.split_once(" - ") else {
                return Err(format!("errcodes.txt line {}: a section with no title", number + 1));
            };
            classes.push((class, title));
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("errcodes.txt line {}: not a code line: {line}", number + 1);
        if !(3..=4).contains(&fields.len()) {
            return Err(bad());
        }
        let state = fields[0];
        let valid = state.len() == 5
            && state.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
            && matches!(fields[1], "S" | "W" | "E");
        let Some(name) = fields[2].strip_prefix("ERRCODE_") else { return Err(bad()) };
        if !valid {
            return Err(bad());
        }
        let condition = fields.get(3).copied().unwrap_or("");
        codes.push(Code { state, category: fields[1], name, condition });
    }

    let mut out = String::from(
        "//! The SQLSTATE codes of PostgreSQL, one constant for each code line of `errcodes.txt`.\n//!\n//! `cargo xtask errcodes` makes this file from `vendor/postgres-19/src/backend/utils/errcodes.txt`. Do not edit it.\n\nuse crate::sqlstate::{Category, SqlState};\n\nimpl SqlState {\n",
    );
    for code in &codes {
        if code.condition.is_empty() {
            let _ = writeln!(out, "    /// `{}`, a second name for this code.", code.state);
        } else {
            let _ = writeln!(out, "    /// `{}`, condition `{}`.", code.state, code.condition);
        }
        let _ = writeln!(
            out,
            "    pub const {}: Self = Self::from_bytes(*b\"{}\");",
            code.name, code.state
        );
    }
    out.push_str("}\n\n");

    let _ = writeln!(
        out,
        "/// Every code line of the file, in file order: the code, the category, the name of the constant and the condition name. The condition name is empty for a second name.\npub(crate) static CODES: [(SqlState, Category, &str, &str); {}] = [",
        codes.len()
    );
    for code in &codes {
        let category = match code.category {
            "S" => "Category::Success",
            "W" => "Category::Warning",
            _ => "Category::Error",
        };
        let _ = writeln!(
            out,
            "    (SqlState::{}, {category}, \"{}\", \"{}\"),",
            code.name, code.name, code.condition
        );
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "/// Every class, in file order: the first two characters of its codes and its title.\npub(crate) static CLASSES: [(&str, &str); {}] = [",
        classes.len()
    );
    for (class, title) in &classes {
        let _ = writeln!(out, "    (\"{class}\", \"{title}\"),");
    }
    out.push_str("];\n");
    Ok(out)
}

//! The Unicode tables.
//!
//! `cargo xtask unicode` makes two files from the vendored data. `crates/rupg-types/src/generated/unicode_norm.rs` holds the tables of the four normalization forms. It comes from `UnicodeData.txt` and `DerivedNormalizationProps.txt` in `vendor/unicode`. `crates/rupg-wire/src/generated/saslprep.rs` holds the stringprep tables of SASLprep. It comes from `vendor/postgres-19/src/common/saslprep.c`, so it has the same ranges as PostgreSQL.
//!
//! `cargo xtask unicode --check` writes nothing. It fails if a file is not the same as the file that the task makes. See `spec/22-crate-layout.md` section 22.8.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const NORM: &str = "crates/rupg-types/src/generated/unicode_norm.rs";
const SASLPREP: &str = "crates/rupg-wire/src/generated/saslprep.rs";

/// The tables of `saslprep.c` in the order of the file, with the name of each table in Rust and its comment.
const STRINGPREP: &[(&str, &str, &str)] = &[
    (
        "non_ascii_space_ranges",
        "NON_ASCII_SPACE",
        "C.1.2, the space characters that are not ASCII. SASLprep changes them to U+0020.",
    ),
    (
        "commonly_mapped_to_nothing_ranges",
        "MAPPED_TO_NOTHING",
        "B.1, the characters that SASLprep removes.",
    ),
    (
        "prohibited_output_ranges",
        "PROHIBITED_OUTPUT",
        "C.1.2, C.2.1, C.2.2, C.3, C.4, C.5, C.6, C.7, C.8 and C.9, the characters that SASLprep does not allow.",
    ),
    (
        "unassigned_codepoint_ranges",
        "UNASSIGNED",
        "A.1, the code points that Unicode 3.2 does not assign.",
    ),
    (
        "RandALCat_codepoint_ranges",
        "RAND_AL_CAT",
        "D.1, the characters with the bidirectional property R or AL.",
    ),
    ("LCat_codepoint_ranges", "L_CAT", "D.2, the characters with the bidirectional property L."),
];

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("unicode: unknown argument {other:?}"));
    }
    let rev = read(&root.join("vendor/unicode/PIN"))?
        .lines()
        .find_map(|l| l.strip_prefix("rev ").map(str::to_string))
        .ok_or("vendor/unicode/PIN has no rev line")?;
    let norm = norm_tables(
        &rev,
        &read(&root.join("vendor/unicode/UnicodeData.txt"))?,
        &read(&root.join("vendor/unicode/DerivedNormalizationProps.txt"))?,
    )?;
    let saslprep = saslprep_tables(&read(&root.join("vendor/postgres-19/src/common/saslprep.c"))?)?;
    let mut stale = Vec::new();
    for (path, text) in [(NORM, norm), (SASLPREP, saslprep)] {
        let file = root.join(path);
        let old = std::fs::read_to_string(&file).unwrap_or_default();
        if old == text {
            continue;
        }
        if check {
            stale.push(path);
        } else {
            std::fs::write(&file, text).map_err(|e| format!("could not write {path}: {e}"))?;
            println!("unicode: wrote {path}");
        }
    }
    if stale.is_empty() {
        println!("unicode: the tables match the vendored files");
        Ok(())
    } else {
        Err(format!(
            "unicode: run `cargo xtask unicode`, these files are not up to date: {}",
            stale.join(", ")
        ))
    }
}

fn hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim(), 16).map_err(|e| format!("bad code point {s:?}: {e}"))
}

/// The table file of the normalization forms.
fn norm_tables(rev: &str, data: &str, props: &str) -> Result<String, String> {
    let mut excluded = BTreeSet::new();
    for line in props.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut fields = line.split(';');
        let (Some(range), Some(name)) = (fields.next(), fields.next()) else { continue };
        if name.trim() != "Full_Composition_Exclusion" {
            continue;
        }
        let (first, last) = match range.trim().split_once("..") {
            Some((a, b)) => (hex(a)?, hex(b)?),
            None => (hex(range)?, hex(range)?),
        };
        excluded.extend(first..=last);
    }

    let mut classes = Vec::new();
    let mut mappings = Vec::new();
    let mut chars = Vec::new();
    let mut pairs = Vec::new();
    for line in data.lines().filter(|l| !l.is_empty()) {
        let field: Vec<&str> = line.split(';').collect();
        if field.len() < 6 {
            return Err(format!("UnicodeData.txt: short line {line:?}"));
        }
        let code = hex(field[0])?;
        let class: u8 =
            field[3].parse().map_err(|e| format!("UnicodeData.txt: bad class in {line:?}: {e}"))?;
        if class != 0 {
            classes.push((code, class));
        }
        if field[5].is_empty() {
            continue;
        }
        let compat = field[5].starts_with('<');
        let parts: Vec<u32> =
            field[5].split(' ').skip(usize::from(compat)).map(hex).collect::<Result<_, _>>()?;
        if !compat && parts.len() == 2 && !excluded.contains(&code) {
            pairs.push((parts[0], parts[1], code));
        }
        let start =
            u16::try_from(chars.len()).map_err(|_| "too many decomposition characters for u16")?;
        let len = u8::try_from(parts.len()).map_err(|_| "a decomposition is too long for u8")?;
        mappings.push((code, compat, start, len));
        chars.extend(parts);
    }
    pairs.sort_unstable();

    let mut out = String::new();
    writeln!(out, "//! The tables of the Unicode normalization forms, for Unicode {rev}.").unwrap();
    out.push_str("//!\n//! `cargo xtask unicode` makes this file from `vendor/unicode/UnicodeData.txt` and `vendor/unicode/DerivedNormalizationProps.txt`. Do not edit it.\n\n");
    out.push_str("/// The canonical combining class of each code point where it is not zero, sorted by code point.\n");
    table(&mut out, "COMBINING_CLASS", "(u32, u8)", &classes, |(c, k)| format!("(0x{c:04X}, {k})"));
    out.push_str("\n/// The decomposition mapping of each code point that has one, sorted by code point. The fields are the code point, `true` for a compatibility mapping, and the start and the length of the mapping in `DECOMPOSITION_CHARS`.\n");
    table(&mut out, "DECOMPOSITION", "(u32, bool, u16, u8)", &mappings, |(c, k, s, l)| {
        format!("(0x{c:04X}, {k}, {s}, {l})")
    });
    out.push_str("\n/// The characters of the decomposition mappings.\n");
    table(&mut out, "DECOMPOSITION_CHARS", "u32", &chars, |c| format!("0x{c:04X}"));
    out.push_str("\n/// The primary composites, sorted by the pair. The fields are the first character, the second character and the composite. A canonical mapping of two characters is a primary composite if the code point does not have the property Full_Composition_Exclusion.\n");
    table(&mut out, "COMPOSITION", "(u32, u32, u32)", &pairs, |(a, b, c)| {
        format!("(0x{a:04X}, 0x{b:04X}, 0x{c:04X})")
    });
    Ok(out)
}

/// The table file of SASLprep.
fn saslprep_tables(source: &str) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("//! The stringprep tables of SASLprep (RFC 3454 and RFC 4013).\n//!\n");
    out.push_str("//! `cargo xtask unicode` makes this file from `vendor/postgres-19/src/common/saslprep.c`, so the ranges are the ranges of PostgreSQL. Do not edit it. Each table holds ranges of code points, first and last, sorted.\n");
    for (c_name, name, doc) in STRINGPREP {
        let head = format!("static const char32_t {c_name}[] =");
        let at = source.find(&head).ok_or_else(|| format!("saslprep.c: no table {c_name}"))?;
        let body = &source[at + head.len()..];
        let open = body.find('{').ok_or_else(|| format!("saslprep.c: {c_name} has no {{"))?;
        let close = body.find("};").ok_or_else(|| format!("saslprep.c: {c_name} has no }};"))?;
        let mut codes = Vec::new();
        for line in body[open + 1..close].lines() {
            let line = line.split("/*").next().unwrap_or("");
            for word in line.split(',') {
                let word = word.trim();
                if let Some(h) = word.strip_prefix("0x") {
                    codes.push(hex(h)?);
                } else if !word.is_empty() {
                    return Err(format!("saslprep.c: {c_name} has {word:?}"));
                }
            }
        }
        if codes.len() % 2 != 0 {
            return Err(format!("saslprep.c: {c_name} has an odd number of code points"));
        }
        let ranges: Vec<(u32, u32)> = codes.chunks(2).map(|r| (r[0], r[1])).collect();
        if !ranges.windows(2).all(|w| w[0].1 < w[1].0) || ranges.iter().any(|r| r.0 > r.1) {
            return Err(format!("saslprep.c: the ranges of {c_name} are not sorted"));
        }
        writeln!(out, "\n/// {doc}").unwrap();
        table(&mut out, name, "(u32, u32)", &ranges, |(a, b)| format!("(0x{a:04X}, 0x{b:04X})"));
    }
    Ok(out)
}

/// Writes one table with up to 100 columns on each line.
fn table<T>(out: &mut String, name: &str, ty: &str, rows: &[T], item: impl Fn(&T) -> String) {
    writeln!(out, "pub(crate) static {name}: [{ty}; {}] = [", rows.len()).unwrap();
    let mut line = String::new();
    for row in rows {
        let text = item(row);
        if !line.is_empty() && line.len() + text.len() + 2 > 100 {
            writeln!(out, "    {}", line.trim_end()).unwrap();
            line.clear();
        }
        line.push_str(&text);
        line.push_str(", ");
    }
    if !line.is_empty() {
        writeln!(out, "    {}", line.trim_end()).unwrap();
    }
    out.push_str("];\n");
}

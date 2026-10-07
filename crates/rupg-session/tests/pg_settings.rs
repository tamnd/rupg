//! The parameter table against `pg_settings` of the oracle (spec/06 section 6.13).
//!
//! `pg_settings.tsv` is the output of `COPY (SELECT name, vartype, context, category, short_desc, extra_desc, unit, boot_val, min_val, max_val, enumvals FROM pg_settings ORDER BY lower(name)) TO STDOUT` on the oracle of PostgreSQL 19 at the pin, built on Linux with the options of spec/21 section 21.3.3. Each row must be the same as the row that the table gives.

use rupg_session::guc::{self, Kind, flag};

/// A field of `COPY` text: `\N` is NULL, and a backslash escapes the next character.
fn field(text: &str) -> Option<String> {
    if text == "\\N" {
        return None;
    }
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

/// The text of the array in `enumvals`, with the quotes that `array_out` adds.
fn enumvals(kind: &Kind) -> Option<String> {
    let Kind::Enum { options, .. } = kind else { return None };
    let names: Vec<String> = options
        .iter()
        .filter(|o| !o.hidden)
        .map(|o| {
            let plain = !o.name.is_empty()
                && !o.name.eq_ignore_ascii_case("null")
                && !o.name.contains(|c: char| c.is_ascii_whitespace() || "{},\"\\".contains(c));
            if plain { o.name.to_string() } else { format!("\"{}\"", o.name) }
        })
        .collect();
    Some(format!("{{{}}}", names.join(",")))
}

/// The row of the table, in the columns of the file.
fn row(p: &guc::Parameter) -> Vec<Option<String>> {
    let (min, max) = match p.kind {
        Kind::Int { min, max, .. } => (Some(min.to_string()), Some(max.to_string())),
        Kind::Real { min, max, .. } => (Some(guc::g(min)), Some(guc::g(max))),
        _ => (None, None),
    };
    let boot = match p.kind {
        Kind::String { boot: None } => None,
        _ => Some(p.setting(&p.boot())),
    };
    vec![
        Some(p.name.to_string()),
        Some(p.vartype().to_string()),
        Some(p.context.name().to_string()),
        Some(p.category.to_string()),
        Some(p.short_desc.to_string()),
        p.extra_desc.map(str::to_string),
        p.unit().map(str::to_string),
        boot,
        min,
        max,
        enumvals(&p.kind),
    ]
}

#[test]
fn the_table_matches_pg_settings_of_the_oracle() {
    let shown: Vec<&guc::Parameter> =
        guc::all().iter().filter(|p| !p.has(flag::NO_SHOW_ALL)).collect();
    let lines: Vec<&str> = include_str!("pg_settings.tsv").lines().collect();
    let mut differ = Vec::new();
    for line in &lines {
        let expected: Vec<Option<String>> = line.split('\t').map(field).collect();
        let name = expected[0].clone().unwrap_or_default();
        match guc::find(&name) {
            Some(p) if row(p) == expected => {}
            Some(p) => {
                differ.push(format!("{name}:\n  oracle {expected:?}\n  table  {:?}", row(p)))
            }
            None => differ.push(format!("{name}: not in the table")),
        }
    }
    assert!(differ.is_empty(), "{} rows differ:\n{}", differ.len(), differ.join("\n"));
    assert_eq!(shown.len(), lines.len(), "the table shows another number of parameters");
}

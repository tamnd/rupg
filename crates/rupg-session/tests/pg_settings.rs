//! The parameter table against `pg_settings` of the oracle (spec/06 section 6.13).
//!
//! `pg_settings.tsv` is the output of `COPY (SELECT name, vartype, context, category, short_desc, extra_desc, unit, boot_val, min_val, max_val, enumvals FROM pg_settings ORDER BY lower(name)) TO STDOUT` on the oracle of PostgreSQL 19 at the pin, built on Linux on x86-64 with the options of spec/21 section 21.3.3. Each row must be the same as the row that the table gives.
//!
//! Some rows depend on the host that PostgreSQL builds on. `pg_settings_hosts.tsv` has these rows for the other hosts of the release builds, with the name of the host in the first column. On such a host, its row replaces the row of the oracle.

use rupg_session::guc::{self, flag};

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
fn enumvals(p: &guc::Parameter) -> Option<String> {
    let names: Vec<String> = p
        .enum_names()?
        .into_iter()
        .map(|name| {
            let plain = !name.is_empty()
                && !name.eq_ignore_ascii_case("null")
                && !name.contains(|c: char| c.is_ascii_whitespace() || "{},\"\\".contains(c));
            if plain { name.to_string() } else { format!("\"{name}\"") }
        })
        .collect();
    Some(format!("{{{}}}", names.join(",")))
}

/// The name of the host in `pg_settings_hosts.tsv`, or `None` for the host of the oracle build.
const HOST: Option<&str> = if cfg!(all(target_os = "linux", not(target_arch = "x86_64"))) {
    Some("linux-aarch64")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("macos-x86_64")
} else if cfg!(target_os = "macos") {
    Some("macos-aarch64")
} else {
    None
};

/// The row of the table, in the columns of the file.
fn row(p: &guc::Parameter) -> Vec<Option<String>> {
    let (min, max) = p.range();
    vec![
        Some(p.name.to_string()),
        Some(p.vartype().to_string()),
        Some(p.context.name().to_string()),
        Some(p.category.to_string()),
        Some(p.short_desc.to_string()),
        p.extra_desc.map(str::to_string),
        p.unit().map(str::to_string),
        p.boot_text(),
        min,
        max,
        enumvals(p),
    ]
}

#[test]
fn the_table_matches_pg_settings_of_the_oracle() {
    let shown: Vec<&guc::Parameter> =
        guc::all().iter().filter(|p| !p.has(flag::NO_SHOW_ALL)).collect();
    let lines: Vec<&str> = include_str!("pg_settings.tsv").lines().collect();
    let host: Vec<&str> = include_str!("pg_settings_hosts.tsv")
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(host, _)| Some(*host) == HOST)
        .map(|(_, line)| line)
        .collect();
    let mut differ = Vec::new();
    for line in &lines {
        let name = line.split('\t').next().unwrap_or_default();
        let line =
            host.iter().find(|h| h.split('\t').next() == Some(name)).copied().unwrap_or(line);
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

//! The reader of the catalog `.dat` files of PostgreSQL, such as `pg_type.dat`.
//!
//! Lifted from `xtask/src/postgres.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::BTreeMap;

/// Reads the entries of a catalog `.dat` file. The file is a Perl array of hashes: each entry is `{ key => 'value', ... }`, a value is in single quotes with `\'` and `\\` as escapes as in Perl, and a `#` outside a value starts a comment to the end of the line.
pub(crate) fn dat_entries(file: &str, text: &str) -> Result<Vec<BTreeMap<String, String>>, String> {
    let text: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    let mut chars = text.chars().peekable();
    let mut entries = Vec::new();
    let skip = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
        while let Some(c) = chars.next_if(|c| c.is_whitespace() || *c == '#') {
            if c == '#' {
                while chars.next_if(|c| *c != '\n').is_some() {}
            }
        }
    };
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        let mut entry = BTreeMap::new();
        loop {
            skip(&mut chars);
            if chars.next_if_eq(&'}').is_some() {
                break;
            }
            let mut key = String::new();
            while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
                key.push(c);
            }
            skip(&mut chars);
            if key.is_empty() || chars.next() != Some('=') || chars.next() != Some('>') {
                return Err(format!("{file}: a field with no `=>` after `{key}`"));
            }
            skip(&mut chars);
            if chars.next() != Some('\'') {
                return Err(format!("{file}: the value of `{key}` is not in quotes"));
            }
            let mut value = String::new();
            loop {
                match chars.next() {
                    None => return Err(format!("{file}: the value of `{key}` has no end")),
                    // Perl reads `\\` and `\'` as escapes and keeps any other backslash.
                    Some('\\') => match chars.next() {
                        Some(c @ ('\\' | '\'')) => value.push(c),
                        Some(c) => value.extend(['\\', c]),
                        None => value.push('\\'),
                    },
                    Some('\'') => break,
                    Some(c) => value.push(c),
                }
            }
            if entry.insert(key.clone(), value).is_some() {
                return Err(format!("{file}: `{key}` is two times in one entry"));
            }
            skip(&mut chars);
            let _ = chars.next_if_eq(&',');
        }
        entries.push(entry);
    }
    Ok(entries)
}

//! The type OIDs.
//!
//! `cargo xtask pgtype` makes `crates/rupg-types/src/generated/oids.rs` from `vendor/postgres-19/src/include/catalog/pg_type.dat`. `cargo xtask pgtype --check` writes nothing and fails if the file is not up to date. See `spec/07-sql-types-and-catalog.md` section 7.9.
//!
//! Lifted from `xtask/src/postgres.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::dat::dat_entries;
use crate::read;

const INPUT: &str = "vendor/postgres-19/src/include/catalog/pg_type.dat";
const OUTPUT: &str = "crates/rupg-types/src/generated/oids.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("pgtype: unknown argument {other:?}"));
    }
    let text = pgtype(&read(&root.join(INPUT))?)?;
    let file = root.join(OUTPUT);
    if std::fs::read_to_string(&file).unwrap_or_default() == text {
        println!("pgtype: {OUTPUT} matches {INPUT}");
        return Ok(());
    }
    if check {
        return Err(format!("pgtype: {OUTPUT} is not up to date, run `cargo xtask pgtype`"));
    }
    std::fs::write(&file, text).map_err(|e| format!("could not write {OUTPUT}: {e}"))?;
    println!("pgtype: wrote {OUTPUT}");
    Ok(())
}

/// One type of `pg_type.dat` with the fields that the generated table keeps. `elem` is a type name until all the entries are read, because the file refers to the element type by name.
struct TypeRow {
    oid: u32,
    name: String,
    descr: String,
    kind: char,
    category: char,
    len: i16,
    elem: String,
    array: String,
    delim: char,
}

/// Renders the type OIDs from the text of `pg_type.dat`.
///
/// An entry with `array_type_oid` also gives an array type, as `genbki.pl` makes it: the name with `_` in front, `typtype` `b`, `typcategory` `A`, `typlen` -1, the entry as `typelem` and the `typdelim` of the entry. An entry can also name its array type with `typarray`, as `record` does. The constant of a type is its name in upper case, and the constant of an array is the name of its element with `_ARRAY` after it.
fn pgtype(text: &str) -> Result<String, String> {
    let mut rows = Vec::new();
    for entry in dat_entries("pg_type.dat", text)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("typname").ok_or("pg_type.dat: an entry with no typname")?;
        let bad = |what: &str| format!("pg_type.dat: {name} has a bad {what}");
        let char_of = |key: &str, default: char| match field(key) {
            None => Ok(default),
            Some(v) if v.chars().count() == 1 => Ok(v.chars().next().unwrap_or(default)),
            Some(_) => Err(bad(key)),
        };
        let oid = field("oid").and_then(|v| v.parse().ok()).ok_or_else(|| bad("oid"))?;
        let len = match field("typlen") {
            // The server is 64-bit, and `pg_type.typlen` shows 8 there.
            Some("NAMEDATALEN") => 64,
            Some("SIZEOF_POINTER") => 8,
            Some(v) => v.parse().map_err(|_| bad("typlen"))?,
            None => return Err(bad("typlen")),
        };
        let row = TypeRow {
            oid,
            name: name.to_string(),
            descr: field("descr").unwrap_or("").to_string(),
            kind: char_of("typtype", 'b')?,
            category: char_of("typcategory", ' ')?,
            len,
            elem: field("typelem").unwrap_or("").to_string(),
            array: field("typarray").unwrap_or("").to_string(),
            delim: char_of("typdelim", ',')?,
        };
        if row.category == ' ' {
            return Err(bad("typcategory"));
        }
        let array = match field("array_type_oid") {
            Some(v) => Some(v.parse().map_err(|_| bad("array_type_oid"))?),
            None => None,
        };
        if let Some(array) = array {
            rows.push(TypeRow {
                oid: array,
                name: format!("_{name}"),
                descr: String::new(),
                kind: 'b',
                category: 'A',
                len: -1,
                elem: name.to_string(),
                array: String::new(),
                delim: row.delim,
            });
            rows.push(TypeRow { array: format!("_{name}"), ..row });
        } else {
            rows.push(row);
        }
    }

    let oids: BTreeMap<&str, u32> = rows.iter().map(|r| (r.name.as_str(), r.oid)).collect();
    if oids.len() != rows.len() {
        return Err("pg_type.dat: two types have the same name".to_string());
    }
    let resolve = |name: &str| match name {
        "" => Ok(0),
        name => oids.get(name).copied().ok_or_else(|| format!("pg_type.dat: no type {name}")),
    };
    let mut links = Vec::new();
    for row in &rows {
        links.push((resolve(&row.elem)?, resolve(&row.array)?));
    }
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].oid);
    if order.windows(2).any(|w| rows[w[0]].oid == rows[w[1]].oid) {
        return Err("pg_type.dat: two types have the same OID".to_string());
    }

    let mut out = String::from(
        "//! The built-in types of PostgreSQL: one for each entry of `pg_type.dat`, and one for each array type that an entry asks for with `array_type_oid`, as `genbki.pl` makes them.\n\
         //!\n\
         //! `cargo xtask pgtype` makes this file from `vendor/postgres-19/src/include/catalog/pg_type.dat`. Do not edit it.\n\
         \n\
         use crate::types::{Oid, TypeInfo, t};\n\
         \n",
    );
    for &i in &order {
        let row = &rows[i];
        let constant = match row.name.strip_prefix('_') {
            Some(elem) => format!("{}_ARRAY", elem.to_ascii_uppercase()),
            None => row.name.to_ascii_uppercase(),
        };
        if row.descr.is_empty() && row.category == 'A' && row.name.starts_with('_') {
            let _ = writeln!(out, "/// `{}`, the array of `{}`.", row.name, &row.name[1..]);
        } else if row.descr.is_empty() {
            let _ = writeln!(out, "/// `{}`.", row.name);
        } else {
            // A description such as `format '[point1,point2]'` must not read as a link or a tag.
            let mut descr = String::new();
            for c in row.descr.chars() {
                if "[]<>".contains(c) {
                    descr.push('\\');
                }
                descr.push(c);
            }
            let _ = writeln!(out, "/// `{}`, {descr}.", row.name);
        }
        let _ = writeln!(out, "pub const {constant}: Oid = {};", row.oid);
    }
    let _ = writeln!(
        out,
        "\n/// Every type in the order of the OIDs: the OID, `typname`, `typtype`, `typcategory`, `typlen`, `typelem`, `typarray` and `typdelim`.\n\
         pub(crate) static TYPES: [TypeInfo; {}] = [",
        rows.len()
    );
    for &i in &order {
        let row = &rows[i];
        let _ = writeln!(
            out,
            "    t({}, \"{}\", b'{}', b'{}', {}, {}, {}, b'{}'),",
            row.oid, row.name, row.kind, row.category, row.len, links[i].0, links[i].1, row.delim
        );
    }
    out.push_str("];\n");
    Ok(out)
}

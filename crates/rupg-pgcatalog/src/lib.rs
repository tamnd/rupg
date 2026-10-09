//! The built-in objects with their PostgreSQL OIDs, generated from the vendored `.dat` files, and the 64 virtual catalogs, 86 system views and 65 `information_schema` views.
//!
//! `cargo xtask pgcatalog` generates the schema of each system catalog from its header and the static rows from the `.dat` files, with the rules of `genbki.pl`. The rows are column batches in the binary, so a scan of the built-in rows allocates nothing for each row. See `spec/07-sql-types-and-catalog.md` sections 7.13.1 and 7.13.2.

#![forbid(unsafe_code)]

pub mod builtin;
mod generated;

use std::fmt::Write as _;

/// One system catalog of PostgreSQL.
#[derive(Debug)]
pub struct Catalog {
    /// The name, such as `pg_type`.
    pub name: &'static str,
    /// The OID of the `pg_class` row.
    pub oid: u32,
    /// The OID of the row type, or 0 if the catalog has no row type.
    pub rowtype_oid: u32,
    /// True if the catalog is shared by all databases of a cluster.
    pub shared: bool,
    /// True for the four catalogs that the bootstrap mode makes first.
    pub bootstrap: bool,
    /// The columns in the order of the header.
    pub columns: &'static [Column],
    /// One batch for each column. It is empty if the catalog has no static rows.
    pub rows: &'static [Batch],
    /// The number of static rows.
    pub len: usize,
}

/// One column of a system catalog, with the properties of its `pg_attribute` row.
#[derive(Debug)]
pub struct Column {
    pub name: &'static str,
    /// The OID of the type, `atttypid`.
    pub type_oid: u32,
    /// `attlen`: the length of a fixed-length type, or -1.
    pub len: i16,
    pub by_val: bool,
    /// `attalign`: `c`, `s`, `i` or `d`.
    pub align: u8,
    /// `attstorage`: `p`, `e`, `m` or `x`.
    pub storage: u8,
    /// `attndims`: 1 for an array type, else 0.
    pub ndims: i16,
    /// `attcollation`: the C collation for a collatable type, else 0.
    pub collation: u32,
    pub not_null: bool,
}

/// The values of one column of the static rows. A null row has the zero value of the kind.
#[derive(Debug)]
pub enum Values {
    /// Every row is null.
    Null,
    Bool(&'static [bool]),
    /// The type `"char"`. The byte 0 is the empty string.
    Char(&'static [u8]),
    Int2(&'static [i16]),
    Int4(&'static [i32]),
    Float4(&'static [f32]),
    /// The types `oid`, `regproc` and `xid`.
    Oid(&'static [u32]),
    /// The types `name` and `text` and any other type that is stored as its text form.
    Text(&'static [&'static str]),
    Int2Vector(&'static [&'static [i16]]),
    OidVector(&'static [&'static [u32]]),
    OidArray(&'static [&'static [u32]]),
    CharArray(&'static [&'static [u8]]),
    TextArray(&'static [&'static [&'static str]]),
}

/// The static values of one column, with a null bitmap.
#[derive(Debug)]
pub struct Batch {
    pub values: Values,
    /// Bit `i % 64` of word `i / 64` is set if row `i` is null. It is empty if no row is null.
    pub nulls: &'static [u64],
}

impl Batch {
    /// A column with a null in every row.
    pub const NULL: Batch = Batch { values: Values::Null, nulls: &[] };

    /// True if row `row` is null.
    pub fn is_null(&self, row: usize) -> bool {
        matches!(self.values, Values::Null)
            || self.nulls.get(row / 64).is_some_and(|w| w & (1 << (row % 64)) != 0)
    }

    /// The text form of a value, as the output function of its type gives it, or `None` for a null. A `regproc` value is the OID.
    pub fn text(&self, row: usize) -> Option<String> {
        if self.is_null(row) {
            return None;
        }
        Some(match &self.values {
            Values::Null => return None,
            Values::Bool(v) => if v[row] { "t" } else { "f" }.to_string(),
            Values::Char(v) => char_text(v[row]),
            Values::Int2(v) => v[row].to_string(),
            Values::Int4(v) => v[row].to_string(),
            Values::Float4(v) => float4_text(v[row]),
            Values::Oid(v) => v[row].to_string(),
            Values::Text(v) => v[row].to_string(),
            Values::Int2Vector(v) => join(v[row], " "),
            Values::OidVector(v) => join(v[row], " "),
            Values::OidArray(v) => format!("{{{}}}", join(v[row], ",")),
            Values::CharArray(v) => {
                let items: Vec<String> = v[row].iter().map(|&c| char_text(c)).collect();
                array_text(&items)
            }
            Values::TextArray(v) => array_text(v[row]),
        })
    }
}

impl Catalog {
    /// The position of a column.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// The 64 system catalogs, in the order of `genbki.pl`.
pub fn catalogs() -> &'static [Catalog] {
    &generated::catalogs::CATALOGS
}

/// A system catalog by name.
pub fn catalog(name: &str) -> Option<&'static Catalog> {
    catalogs().iter().find(|c| c.name == name)
}

/// The text of `system_views.sql`, the script that `initdb` runs after the bootstrap. It makes the system views of `pg_catalog` and gives privileges on them and on some catalogs.
pub fn system_views() -> &'static str {
    generated::system_views::TEXT
}

/// The text of `information_schema.sql`, the script that `initdb` runs after `system_views.sql`. It makes the schema `information_schema` with its functions, domains, tables and views.
pub fn information_schema() -> &'static str {
    generated::information_schema::TEXT
}

/// A system catalog by the OID of its `pg_class` row.
pub fn catalog_by_oid(oid: u32) -> Option<&'static Catalog> {
    catalogs().iter().find(|c| c.oid == oid)
}

fn join<T: ToString>(items: &[T], sep: &str) -> String {
    items.iter().map(T::to_string).collect::<Vec<_>>().join(sep)
}

/// `charout`: the byte 0 is the empty string, a byte above 127 is a backslash and three octal digits.
fn char_text(c: u8) -> String {
    match c {
        0 => String::new(),
        1..=127 => char::from(c).to_string(),
        _ => format!("\\{c:03o}"),
    }
}

/// `float4out` with the default `extra_float_digits`: the shortest text that reads back as the same value.
fn float4_text(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let e = format!("{f:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    // float_to_shortest_decimal_buf uses the exponent form if the decimal exponent is below -4 or not below FLT_DIG, which is 6.
    if (-4..6).contains(&exp) {
        format!("{f}")
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// `array_out` for a one-dimensional array of text values.
fn array_text<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let item = item.as_ref();
        let quote = item.is_empty()
            || item.eq_ignore_ascii_case("NULL")
            || item
                .chars()
                .any(|c| matches!(c, '"' | '\\' | '{' | '}' | ',') || c.is_ascii_whitespace());
        if !quote {
            out.push_str(item);
            continue;
        }
        out.push('"');
        for c in item.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    out.push('}');
    out
}

/// One static row as text, with `\N` for a null and the escapes of the text form of `COPY`, the columns separated by tabs.
pub fn copy_line(catalog: &Catalog, row: usize) -> String {
    let mut out = String::new();
    for (i, batch) in catalog.rows.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        match batch.text(row) {
            None => out.push_str("\\N"),
            Some(text) => {
                for c in text.chars() {
                    match c {
                        '\\' => out.push_str("\\\\"),
                        '\t' => out.push_str("\\t"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        c if u32::from(c) < 0x20 => {
                            let _ = write!(out, "\\{:03o}", u32::from(c));
                        }
                        c => out.push(c),
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogs() {
        assert_eq!(catalogs().len(), 64);
        let pg_type = catalog("pg_type").unwrap();
        assert_eq!((pg_type.oid, pg_type.rowtype_oid, pg_type.bootstrap), (1247, 71, true));
        assert_eq!(pg_type.columns[1].name, "typname");
        assert_eq!(pg_type.columns[1].type_oid, 19);
        assert_eq!(catalog_by_oid(1262).unwrap().name, "pg_database");
        assert!(catalog("pg_authid").unwrap().shared);
        assert_eq!(catalog("pg_statistic").unwrap().len, 0);
    }

    #[test]
    fn text_forms() {
        assert_eq!(array_text(&["a", "b c", "", "NULL", "x\"y"]), r#"{a,"b c","","NULL","x\"y"}"#);
        assert_eq!(float4_text(1.0), "1");
        assert_eq!(float4_text(0.0025), "0.0025");
        assert_eq!(float4_text(1e10), "1e+10");
        assert_eq!(float4_text(1e-5), "1e-05");
        assert_eq!(char_text(0), "");
    }
}

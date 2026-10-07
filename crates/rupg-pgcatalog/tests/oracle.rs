//! Compares the static rows with the rows of PostgreSQL 19 at the pin.
//!
//! `oracle.tsv` is the output of `oracle.sql` on the oracle. Each line is a catalog name and one row in the text form of `COPY`.
//!
//! The oracle rows are the rows after `initdb`, and the static rows are the rows of the `.dat` files after the bootstrap mode reads them. The rules below remove the changes that the later steps of `initdb` make. Each rule names the step.

use std::collections::{BTreeMap, BTreeSet};

use rupg_pgcatalog::{catalog, catalogs, copy_line};

const ORACLE: &str = include_str!("oracle.tsv");

/// Columns that a later step of `initdb` sets. The test does not compare them.
const INITDB_COLUMNS: [(&str, &[&str]); 5] = [
    // initdb sets the password of the bootstrap superuser from --pwfile.
    ("pg_authid", &["rolpassword"]),
    // The bootstrap commands `declare index` and `declare toast` set relhasindex and reltoastrelid. VACUUM sets the statistics and relfrozenxid, and setup_privileges sets relacl.
    (
        "pg_class",
        &[
            "relpages",
            "reltuples",
            "relallvisible",
            "relallfrozen",
            "reltoastrelid",
            "relhasindex",
            "relfrozenxid",
            "relacl",
        ],
    ),
    // setup_collation sets the version of an ICU collation from the ICU library.
    ("pg_collation", &["collversion"]),
    // VACUUM FREEZE sets datfrozenxid, and setup_privileges sets datacl.
    ("pg_database", &["datfrozenxid", "datacl"]),
    // setup_privileges sets nspacl.
    ("pg_namespace", &["nspacl"]),
];

/// The `prosrc` of a function that `system_functions.sql` replaces with a SQL body. The replacement sets `prosrc`, `prosqlbody` and `procost`.
const SQL_BODY: &str = "see system_functions.sql";
const SQL_BODY_COLUMNS: [&str; 3] = ["prosrc", "prosqlbody", "procost"];

/// True for an oracle row that is not a static row. The bootstrap commands `create`, `declare index` and `declare toast` add the `pg_class` rows of the other catalogs, the indexes and the toast tables. make_template0 and make_postgres of `initdb` add the databases 4 and 5 and their descriptions.
fn made_by_initdb(name: &str, row: &[String]) -> bool {
    match name {
        "pg_class" => true,
        "pg_database" | "pg_shdescription" => row[0] == "4" || row[0] == "5",
        _ => false,
    }
}

/// The rows of each catalog, as lists of fields.
type Rows = BTreeMap<&'static str, Vec<Vec<String>>>;

fn generated() -> Rows {
    let mut out = Rows::new();
    for catalog in catalogs().iter().filter(|c| c.len > 0) {
        let rows = (0..catalog.len).map(|row| fields(&copy_line(catalog, row))).collect();
        out.insert(catalog.name, rows);
    }
    out
}

fn oracle() -> Rows {
    let mut out = Rows::new();
    for line in ORACLE.lines() {
        let (name, row) = line.split_once('\t').expect("a catalog name and a row");
        out.entry(name).or_default().push(fields(row));
    }
    out
}

fn fields(line: &str) -> Vec<String> {
    line.split('\t').map(str::to_string).collect()
}

/// Sets the fields of the named columns to an empty string.
fn blank(name: &str, row: &mut [String], columns: &[&str]) {
    let catalog = catalog(name).expect("a catalog");
    for column in columns {
        row[catalog.column(column).expect("a column")].clear();
    }
}

/// Applies the rules to the rows of both sides.
fn normalize(ours: &mut Rows, theirs: &mut Rows) {
    for (name, columns) in INITDB_COLUMNS {
        for rows in [ours.get_mut(name), theirs.get_mut(name)].into_iter().flatten() {
            for row in rows {
                blank(name, row, columns);
            }
        }
    }
    let prosrc = catalog("pg_proc").unwrap().column("prosrc").unwrap();
    let replaced: BTreeSet<String> = ours["pg_proc"]
        .iter()
        .filter(|row| row[prosrc] == SQL_BODY)
        .map(|row| row[0].clone())
        .collect();
    for rows in [ours.get_mut("pg_proc"), theirs.get_mut("pg_proc")].into_iter().flatten() {
        for row in rows.iter_mut().filter(|row| replaced.contains(&row[0])) {
            blank("pg_proc", row, &SQL_BODY_COLUMNS);
        }
    }
    for (name, rows) in theirs.iter_mut() {
        let static_rows: BTreeSet<&String> =
            ours.get(name).into_iter().flatten().map(|row| &row[0]).collect();
        rows.retain(|row| static_rows.contains(&row[0]) || !made_by_initdb(name, row));
    }
}

#[test]
fn every_static_row_is_the_row_of_the_oracle() {
    let mut ours = generated();
    let mut theirs = oracle();
    assert_eq!(ours.keys().collect::<Vec<_>>(), theirs.keys().collect::<Vec<_>>());
    normalize(&mut ours, &mut theirs);
    let mut report = String::new();
    for (name, rows) in &ours {
        let rows: BTreeSet<String> = rows.iter().map(|row| row.join("\t")).collect();
        let other: BTreeSet<String> = theirs[name].iter().map(|row| row.join("\t")).collect();
        let extra: Vec<&String> = rows.difference(&other).collect();
        let missing: Vec<&String> = other.difference(&rows).collect();
        if extra.is_empty() && missing.is_empty() {
            continue;
        }
        report.push_str(&format!(
            "{name}: {} rows not in the oracle, {} rows missing\n",
            extra.len(),
            missing.len()
        ));
        for row in extra.iter().take(3) {
            report.push_str(&format!("  ours:   {row}\n"));
        }
        for row in missing.iter().take(3) {
            report.push_str(&format!("  oracle: {row}\n"));
        }
    }
    assert!(report.is_empty(), "{report}");
}

/// The rules must not hide rows: each catalog keeps its row count, except for the rows that `initdb` adds.
#[test]
fn the_rules_keep_the_rows() {
    let ours = generated();
    let theirs = oracle();
    let count = |rows: &Rows, name: &str| rows.get(name).map_or(0, Vec::len);
    for name in ours.keys() {
        let added = theirs[name].iter().filter(|row| made_by_initdb(name, row)).count();
        let bootstrap = if *name == "pg_class" { count(&ours, name) } else { 0 };
        assert_eq!(count(&ours, name), count(&theirs, name) - added + bootstrap, "{name}");
    }
    assert_eq!(count(&ours, "pg_class"), 4);
    assert_eq!(count(&ours, "pg_proc"), 3414);
}

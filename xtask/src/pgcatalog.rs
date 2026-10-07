//! The schemas and the static rows of the system catalogs.
//!
//! `cargo xtask pgcatalog` reads the 64 catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog` and writes `crates/rupg-pgcatalog/src/generated`. It follows `genbki.pl` and `Catalog.pm` of the pin. It fills the defaults of `BKI_DEFAULT`, makes the array types of `pg_type`, gives an OID from 10000 to each row that has none, turns the names of `BKI_LOOKUP` into OIDs, and makes the rows of `pg_description` and `pg_shdescription` from the `descr` fields. Then it replaces the tokens that `initdb` replaces in `postgres.bki`, and adds the descriptions of the operator functions that `setup_description` of `initdb` adds.
//!
//! `cargo xtask pgcatalog --check` writes nothing. It fails if a file is not the same as the file that the task makes. See `spec/07-sql-types-and-catalog.md` section 7.13.2.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::dat::dat_entries;
use crate::read;

const INPUT: &str = "vendor/postgres-19/src/include";
const OUTPUT: &str = "crates/rupg-pgcatalog/src/generated";

/// The catalog headers in the order of `src/include/catalog/meson.build`. `genbki.pl` reads them in this order.
const HEADERS: [&str; 64] = [
    "pg_proc",
    "pg_type",
    "pg_attribute",
    "pg_class",
    "pg_attrdef",
    "pg_constraint",
    "pg_inherits",
    "pg_index",
    "pg_operator",
    "pg_opfamily",
    "pg_opclass",
    "pg_am",
    "pg_amop",
    "pg_amproc",
    "pg_language",
    "pg_largeobject_metadata",
    "pg_largeobject",
    "pg_aggregate",
    "pg_statistic",
    "pg_statistic_ext",
    "pg_statistic_ext_data",
    "pg_rewrite",
    "pg_trigger",
    "pg_event_trigger",
    "pg_description",
    "pg_cast",
    "pg_enum",
    "pg_namespace",
    "pg_conversion",
    "pg_depend",
    "pg_database",
    "pg_db_role_setting",
    "pg_tablespace",
    "pg_authid",
    "pg_auth_members",
    "pg_shdepend",
    "pg_shdescription",
    "pg_ts_config",
    "pg_ts_config_map",
    "pg_ts_dict",
    "pg_ts_parser",
    "pg_ts_template",
    "pg_extension",
    "pg_foreign_data_wrapper",
    "pg_foreign_server",
    "pg_user_mapping",
    "pg_foreign_table",
    "pg_policy",
    "pg_replication_origin",
    "pg_default_acl",
    "pg_init_privs",
    "pg_seclabel",
    "pg_shseclabel",
    "pg_collation",
    "pg_parameter_acl",
    "pg_partitioned_table",
    "pg_range",
    "pg_transform",
    "pg_sequence",
    "pg_publication",
    "pg_publication_namespace",
    "pg_publication_rel",
    "pg_subscription",
    "pg_subscription_rel",
];

/// The tokens that `initdb` replaces in `postgres.bki`, with the values of a rupg file: the bootstrap superuser `postgres`, the encoding UTF8, the locale C and the libc locale provider. The value `_null_` is a null.
const TOKENS: [(&str, &str); 10] = [
    ("NAMEDATALEN", "64"),
    ("SIZEOF_POINTER", "8"),
    ("ALIGNOF_POINTER", "d"),
    ("POSTGRES", "postgres"),
    ("ENCODING", "6"),
    ("LC_COLLATE", "C"),
    ("LC_CTYPE", "C"),
    ("DATLOCALE", "_null_"),
    ("ICU_RULES", "_null_"),
    ("LOCALE_PROVIDER", "c"),
];

/// The fields of a `.dat` entry that are not columns.
const METADATA: [&str; 3] = ["oid_symbol", "array_type_oid", "descr"];

/// One column of a catalog header.
#[derive(Debug, Default)]
struct Column {
    name: String,
    /// The SQL type name. An array type has a `_` in front.
    ty: String,
    force_null: bool,
    force_not_null: bool,
    default: Option<String>,
    array_default: Option<String>,
    lookup: Option<String>,
    lookup_opt: bool,
}

/// One catalog header.
#[derive(Debug, Default)]
struct Catalog {
    name: String,
    oid: u32,
    rowtype_oid: u32,
    shared: bool,
    bootstrap: bool,
    columns: Vec<Column>,
}

/// One row: the value of each column as `genbki.pl` writes it, with `_null_` for a null.
type Row = BTreeMap<String, String>;

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("pgcatalog: unknown argument {other:?}"));
    }
    let files = generate(&root.join(INPUT))?;
    let dir = root.join(OUTPUT);
    let mut stale = Vec::new();
    let mut names = BTreeSet::new();
    for (name, text) in &files {
        names.insert(name.clone());
        let file = dir.join(name);
        if std::fs::read_to_string(&file).unwrap_or_default() == *text {
            continue;
        }
        if check {
            stale.push(name.clone());
        } else {
            std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {OUTPUT}: {e}"))?;
            std::fs::write(&file, text).map_err(|e| format!("could not write {name}: {e}"))?;
            println!("pgcatalog: wrote {OUTPUT}/{name}");
        }
    }
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("could not read {OUTPUT}: {e}"))? {
        let name = entry.map_err(|e| e.to_string())?.file_name().to_string_lossy().into_owned();
        if !names.contains(&name) {
            stale.push(name);
        }
    }
    if stale.is_empty() {
        println!("pgcatalog: {OUTPUT} matches the vendored catalog files");
        Ok(())
    } else {
        Err(format!(
            "pgcatalog: run `cargo xtask pgcatalog`, these files in {OUTPUT} are not up to date: {}",
            stale.join(", ")
        ))
    }
}

/// Makes the generated files, as (file name, text).
fn generate(include: &Path) -> Result<Vec<(String, String)>, String> {
    let catalog_dir = include.join("catalog");
    let mut catalogs = Vec::new();
    let mut data: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut descriptions = Vec::new();
    let mut shared_descriptions = Vec::new();
    for name in HEADERS {
        let catalog = parse_header(name, &read(&catalog_dir.join(format!("{name}.h")))?)?;
        let dat = catalog_dir.join(format!("{name}.dat"));
        if dat.is_file() {
            let file = format!("{name}.dat");
            let mut rows = Vec::new();
            for mut row in dat_entries(&file, &read(&dat)?)? {
                add_defaults(&mut row, &catalog, &file)?;
                rows.push(row);
            }
            if name == "pg_type" {
                generate_array_types(&catalog, &mut rows);
            }
            for row in &rows {
                let Some(descr) = row.get("descr") else { continue };
                let objoid =
                    row.get("oid").ok_or_else(|| format!("{file}: a descr with no oid"))?;
                let mut d = Row::new();
                d.insert("objoid".into(), objoid.clone());
                d.insert("classoid".into(), catalog.oid.to_string());
                d.insert("description".into(), descr.clone());
                if catalog.shared {
                    shared_descriptions.push(d);
                } else {
                    d.insert("objsubid".into(), "0".into());
                    descriptions.push(d);
                }
            }
            data.insert(name.to_string(), rows);
        }
        catalogs.push(catalog);
    }
    data.insert("pg_description".into(), descriptions);
    data.insert("pg_shdescription".into(), shared_descriptions);

    // genbki.pl sets relnatts of the pg_class rows from the headers.
    for row in data.get_mut("pg_class").into_iter().flatten() {
        let relname = &row["relname"];
        let catalog = catalogs.iter().find(|c| c.name == *relname);
        let natts =
            catalog.ok_or_else(|| format!("pg_class.dat: no header for {relname}"))?.columns.len();
        row.insert("relnatts".into(), natts.to_string());
    }

    let lookups = lookup_maps(&data, &read(&include.join("mb/pg_wchar.h"))?)?;
    let transam = read(&include.join("access/transam.h"))?;
    let first_genbki = defined_symbol(&transam, "FirstGenbkiObjectId")?;
    let first_unpinned = defined_symbol(&transam, "FirstUnpinnedObjectId")?;

    // The pg_type rows before the lookups give the column properties, as the %types hash of genbki.pl does.
    let mut types: BTreeMap<String, Row> =
        data["pg_type"].iter().map(|r| (r["typname"].clone(), r.clone())).collect();
    let c_collation = data["pg_collation"]
        .iter()
        .find(|r| r.get("oid_symbol").map(String::as_str) == Some("C_COLLATION_OID"))
        .map(|r| r["oid"].clone())
        .ok_or("pg_collation.dat has no C_COLLATION_OID")?;

    let mut rows_out: BTreeMap<String, Vec<Vec<Option<String>>>> = BTreeMap::new();
    for catalog in &catalogs {
        let Some(rows) = data.get(&catalog.name) else { continue };
        let mut next_oid = first_genbki;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let file = format!("{}.dat", catalog.name);
            for key in row.keys() {
                if !METADATA.contains(&key.as_str())
                    && !catalog.columns.iter().any(|c| c.name == *key)
                {
                    return Err(format!("{file}: unknown field {key:?}"));
                }
            }
            let mut values = Vec::with_capacity(catalog.columns.len());
            for column in &catalog.columns {
                let mut value = match row.get(&column.name) {
                    Some(v) => v.clone(),
                    None if column.name == "oid" => {
                        if next_oid >= first_unpinned {
                            return Err(format!(
                                "the OID counter of {} reached {next_oid}",
                                catalog.name
                            ));
                        }
                        next_oid += 1;
                        (next_oid - 1).to_string()
                    }
                    None => return Err(format!("{file}: no value for {}", column.name)),
                };
                if let Some(kind) = &column.lookup {
                    value = lookup(&lookups, kind, column, &value, &file)?;
                }
                let value = bootstrap_value(&value);
                values.push(match value {
                    Some(v) if column.ty == "_aclitem" => {
                        Some(acl_text(&v).ok_or_else(|| format!("{file}: bad aclitem[] {v:?}"))?)
                    }
                    v => v,
                });
            }
            out.push(values);
        }
        rows_out.insert(catalog.name.clone(), out);
    }
    catalog_rowtypes(&catalogs, &lookups, first_genbki, &mut rows_out, &mut types)?;
    proc_arg_defaults(&catalogs, &mut rows_out)?;
    operator_descriptions(&catalogs, &mut rows_out)?;

    let mut files = Vec::new();
    files.push((
        "catalogs.rs".to_string(),
        catalogs_file(&catalogs, &types, &c_collation, &rows_out)?,
    ));
    for catalog in &catalogs {
        if let Some(rows) = rows_out.get(&catalog.name) {
            files.push((format!("{}.rs", catalog.name), rows_file(catalog, rows)?));
        }
    }
    files.push(("mod.rs".to_string(), mod_file(&catalogs, &rows_out)));
    Ok(files)
}

/// `ParseHeader` of `Catalog.pm`: the name, the OIDs, the flags and the columns of a catalog.
fn parse_header(name: &str, text: &str) -> Result<Catalog, String> {
    let file = format!("{name}.h");
    let text = strip_comments(text);
    let mut catalog = Catalog::default();
    let mut declaring = false;
    let mut client_code = false;
    for line in text.lines() {
        let line = line.trim();
        if client_code {
            client_code = !line.starts_with("#endif");
            continue;
        }
        if line.starts_with('#') {
            client_code = line.starts_with("#ifdef EXPOSE_TO_CLIENT_CODE");
            continue;
        }
        let line = line.trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(rest) = line.strip_prefix("CATALOG(") {
            let (args, flags) =
                rest.split_once(')').ok_or_else(|| format!("{file}: bad CATALOG line"))?;
            let args: Vec<&str> = args.split(',').map(str::trim).collect();
            if args.len() != 3 || args[0] != name {
                return Err(format!("{file}: bad CATALOG line"));
            }
            catalog.name = name.to_string();
            catalog.oid = args[1].parse().map_err(|_| format!("{file}: bad OID {}", args[1]))?;
            catalog.shared = flags.contains("BKI_SHARED_RELATION");
            catalog.bootstrap = flags.contains("BKI_BOOTSTRAP");
            if let Some(rest) = flags.split_once("BKI_ROWTYPE_OID(").map(|p| p.1) {
                let oid = rest.split(',').next().unwrap_or("").trim();
                catalog.rowtype_oid =
                    oid.parse().map_err(|_| format!("{file}: bad rowtype OID"))?;
            }
            declaring = true;
            continue;
        }
        if !declaring || line.is_empty() || line.starts_with('{') {
            continue;
        }
        if line.starts_with('}') {
            declaring = false;
            continue;
        }
        catalog.columns.push(parse_column(&file, &line)?);
    }
    if catalog.name.is_empty() {
        return Err(format!("{file}: no CATALOG line"));
    }
    Ok(catalog)
}

/// Removes the C comments. A comment can span lines, so it can join two lines, as in `Catalog.pm`.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn parse_column(file: &str, line: &str) -> Result<Column, String> {
    let mut words = line.split(' ');
    let (Some(c_type), Some(c_name)) = (words.next(), words.next()) else {
        return Err(format!("{file}: bad column line {line:?}"));
    };
    let mut ty = match c_type {
        "int16" => "int2",
        "int32" => "int4",
        "int64" => "int8",
        "Oid" => "oid",
        "NameData" => "name",
        "TransactionId" => "xid",
        "XLogRecPtr" => "pg_lsn",
        other => other,
    }
    .to_string();
    let mut name = c_name.to_string();
    if let Some((base, _)) = c_name.split_once('[') {
        name = base.to_string();
        ty = format!("_{ty}");
    }
    let mut column = Column { name, ty, ..Column::default() };
    for option in words {
        let arg = |prefix: &str| {
            option
                .strip_prefix(prefix)
                .and_then(|r| r.strip_suffix(')'))
                .map(|r| r.trim_matches(|c| c == '\'' || c == '"').to_string())
        };
        if option == "BKI_FORCE_NULL" {
            column.force_null = true;
        } else if option == "BKI_FORCE_NOT_NULL" {
            column.force_not_null = true;
        } else if let Some(v) = arg("BKI_DEFAULT(") {
            column.default = Some(v);
        } else if let Some(v) = arg("BKI_ARRAY_DEFAULT(") {
            column.array_default = Some(v);
        } else if let Some(v) = arg("BKI_LOOKUP_OPT(") {
            column.lookup = Some(v);
            column.lookup_opt = true;
        } else if let Some(v) = arg("BKI_LOOKUP(") {
            column.lookup = Some(v);
        } else {
            return Err(format!("{file}: unknown column option {option:?} of {}", column.name));
        }
    }
    Ok(column)
}

/// `AddDefaultValues` of `Catalog.pm`.
fn add_defaults(row: &mut Row, catalog: &Catalog, file: &str) -> Result<(), String> {
    if catalog.name == "pg_proc"
        && let Some(args) = row.get("proargtypes")
    {
        let n = args.split_whitespace().count();
        row.insert("pronargs".into(), n.to_string());
    }
    let mut missing = Vec::new();
    for column in &catalog.columns {
        if row.contains_key(&column.name) || column.name == "oid" {
            continue;
        }
        match &column.default {
            Some(d) => {
                row.insert(column.name.clone(), d.clone());
            }
            None => missing.push(column.name.as_str()),
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{file}: no value for {} in an entry", missing.join(", ")))
    }
}

/// `GenerateArrayTypes` of `Catalog.pm`: an array type for each `pg_type` entry with `array_type_oid`.
fn generate_array_types(catalog: &Catalog, rows: &mut Vec<Row>) {
    let mut arrays = Vec::new();
    for elem in rows.iter_mut() {
        let Some(oid) = elem.get("array_type_oid").cloned() else { continue };
        let mut array = Row::new();
        array.insert("oid".into(), oid);
        array.insert("typname".into(), format!("_{}", elem["typname"]));
        array.insert("typelem".into(), elem["typname"].clone());
        let align = if elem["typalign"] == "d" { "d" } else { "i" };
        array.insert("typalign".into(), align.into());
        for column in &catalog.columns {
            if array.contains_key(&column.name) {
                continue;
            }
            let value = match &column.array_default {
                Some(d) => d.clone(),
                None => elem[&column.name].clone(),
            };
            array.insert(column.name.clone(), value);
        }
        elem.insert("typarray".into(), array["typname"].clone());
        arrays.push(array);
    }
    rows.extend(arrays);
}

/// The lookup hashes of `genbki.pl`, by the name in `BKI_LOOKUP`. The value `None` marks a name that is not unique.
type Lookups = BTreeMap<&'static str, BTreeMap<String, Option<String>>>;

fn lookup_maps(data: &BTreeMap<String, Vec<Row>>, pg_wchar: &str) -> Result<Lookups, String> {
    let mut maps = Lookups::new();
    let simple = [
        ("pg_am", "amname"),
        ("pg_authid", "rolname"),
        ("pg_class", "relname"),
        ("pg_collation", "collname"),
        ("pg_language", "lanname"),
        ("pg_namespace", "nspname"),
        ("pg_tablespace", "spcname"),
        ("pg_ts_config", "cfgname"),
        ("pg_ts_dict", "dictname"),
        ("pg_ts_parser", "prsname"),
        ("pg_ts_template", "tmplname"),
        ("pg_type", "typname"),
    ];
    for (catalog, key) in simple {
        let map = maps.entry(catalog).or_default();
        for row in &data[catalog] {
            map.insert(row[key].clone(), row.get("oid").cloned());
        }
    }
    let map = maps.entry("pg_opclass").or_default();
    for row in &data["pg_opclass"] {
        map.insert(format!("{}/{}", row["opcmethod"], row["opcname"]), row.get("oid").cloned());
    }
    let map = maps.entry("pg_opfamily").or_default();
    for row in &data["pg_opfamily"] {
        map.insert(format!("{}/{}", row["opfmethod"], row["opfname"]), row.get("oid").cloned());
    }
    let map = maps.entry("pg_operator").or_default();
    for row in &data["pg_operator"] {
        let key = format!("{}({},{})", row["oprname"], row["oprleft"], row["oprright"]);
        map.insert(key, row.get("oid").cloned());
    }
    let map = maps.entry("pg_proc").or_default();
    for row in &data["pg_proc"] {
        let args: Vec<&str> = row["proargtypes"].split_whitespace().collect();
        for key in [row["proname"].clone(), format!("{}({})", row["proname"], args.join(","))] {
            let oid = if map.contains_key(&key) { None } else { row.get("oid").cloned() };
            map.insert(key, oid);
        }
    }
    // The encodings are the members of enum pg_enc in pg_wchar.h, numbered from 0.
    let map = maps.entry("encoding").or_default();
    let body = pg_wchar
        .split_once("typedef enum pg_enc")
        .and_then(|(_, r)| r.split_once("_PG_LAST_ENCODING_"))
        .map(|(b, _)| b)
        .ok_or("pg_wchar.h has no enum pg_enc")?;
    let mut id = 0;
    for line in body.lines() {
        let word = line.trim_start().split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next();
        if let Some(word) =
            word.filter(|w| w.starts_with("PG_") && line.starts_with(char::is_whitespace))
        {
            map.insert(word.to_string(), Some(id.to_string()));
            id += 1;
        }
    }
    Ok(maps)
}

/// `lookup_oids` of `genbki.pl` for one column value.
fn lookup(
    maps: &Lookups,
    kind: &str,
    column: &Column,
    value: &str,
    file: &str,
) -> Result<String, String> {
    let map = maps.get(kind).ok_or_else(|| format!("unknown BKI_LOOKUP kind {kind}"))?;
    let one = |name: &str| -> Result<String, String> {
        if let Some(Some(oid)) = map.get(name) {
            return Ok(oid.clone());
        }
        if (name == "-" || name == "0") && column.lookup_opt {
            return Ok(name.to_string());
        }
        Err(format!("{file}: no OID for {name:?} in {}", column.name))
    };
    match column.ty.as_str() {
        "oidvector" => {
            Ok(value.split_whitespace().map(one).collect::<Result<Vec<_>, _>>()?.join(" "))
        }
        "_oid" if value == "_null_" => Ok(value.to_string()),
        "_oid" => {
            let names = value.trim_matches(|c| c == '{' || c == '}');
            let oids: Vec<String> = names.split(',').map(one).collect::<Result<_, _>>()?;
            Ok(format!("{{{}}}", oids.join(",")))
        }
        _ => one(value),
    }
}

/// The value that the bootstrap mode reads for a value of `genbki.pl`: the `initdb` tokens replaced, `\0` as an empty string, the escapes of a quoted string read, and `_null_` as a null.
fn bootstrap_value(value: &str) -> Option<String> {
    let value = TOKENS.iter().find(|t| t.0 == value).map_or(value, |t| t.1);
    if value == "_null_" {
        return None;
    }
    if value == "\\0" {
        return Some(String::new());
    }
    Some(deescape(value))
}

/// The roles that `boot_get_role_oid` of `bootstrap.c` knows, by name, with the name that `aclitemout` gives after `initdb`. Only these roles can be in an `aclitem` of a `.dat` file.
const BOOT_ROLES: [(&str, &str); 17] = [
    ("POSTGRES", "postgres"),
    ("pg_database_owner", "pg_database_owner"),
    ("pg_read_all_data", "pg_read_all_data"),
    ("pg_write_all_data", "pg_write_all_data"),
    ("pg_monitor", "pg_monitor"),
    ("pg_read_all_settings", "pg_read_all_settings"),
    ("pg_read_all_stats", "pg_read_all_stats"),
    ("pg_stat_scan_tables", "pg_stat_scan_tables"),
    ("pg_read_server_files", "pg_read_server_files"),
    ("pg_write_server_files", "pg_write_server_files"),
    ("pg_execute_server_program", "pg_execute_server_program"),
    ("pg_signal_backend", "pg_signal_backend"),
    ("pg_checkpoint", "pg_checkpoint"),
    ("pg_maintain", "pg_maintain"),
    ("pg_use_reserved_connections", "pg_use_reserved_connections"),
    ("pg_create_subscription", "pg_create_subscription"),
    ("pg_signal_autovacuum_worker", "pg_signal_autovacuum_worker"),
];

/// `ACL_ALL_RIGHTS_STR` of `acl.h`: the privilege letters in the order of `aclitemout`.
const ACL_RIGHTS: &str = "arwdDxtXUCTcsAm";

/// An `aclitem[]` value as `aclitemin` reads it in the bootstrap mode and `aclitemout` writes it after `initdb`. The grantor is the bootstrap superuser if the item gives none.
fn acl_text(value: &str) -> Option<String> {
    let role = |name: &str| BOOT_ROLES.iter().find(|r| r.0 == name).map(|r| r.1);
    let mut items = Vec::new();
    for item in array_elements(value)? {
        let (grantee, rest) = item.split_once('=')?;
        let (privs, grantor) = rest.split_once('/').unwrap_or((rest, "POSTGRES"));
        let grantee = if grantee.is_empty() { "" } else { role(grantee)? };
        let mut letters = String::new();
        for right in ACL_RIGHTS.chars() {
            let Some(at) = privs.find(right) else { continue };
            letters.push(right);
            if privs[at + 1..].starts_with('*') {
                letters.push('*');
            }
        }
        if letters.len() != privs.len() {
            return None;
        }
        items.push(format!("{grantee}={letters}/{}", role(grantor)?));
    }
    Some(format!("{{{}}}", items.join(",")))
}

/// `InsertOneProargdefaultsValue` of `bootstrap.c`. In the `.dat` file, `proargdefaults` is a text array with one value for each of the last arguments. The bootstrap mode reads each value with the input function of the argument type and stores the list of `Const` nodes as the text of `nodeToString`, and it sets `pronargdefaults` to the number of values.
fn proc_arg_defaults(
    catalogs: &[Catalog],
    rows_out: &mut BTreeMap<String, Vec<Vec<Option<String>>>>,
) -> Result<(), String> {
    let pos = |catalog: &str, column: &str| -> Result<usize, String> {
        let catalog = catalogs.iter().find(|c| c.name == catalog).ok_or("no catalog")?;
        catalog
            .columns
            .iter()
            .position(|c| c.name == column)
            .ok_or_else(|| format!("{} has no {column}", catalog.name))
    };
    let (oid, len, by_val, collation) = (
        pos("pg_type", "oid")?,
        pos("pg_type", "typlen")?,
        pos("pg_type", "typbyval")?,
        pos("pg_type", "typcollation")?,
    );
    let mut types = BTreeMap::new();
    for row in &rows_out["pg_type"] {
        let field = |i: usize| row[i].clone().unwrap_or_default();
        types.insert(field(oid), (field(len), field(by_val) == "t", field(collation)));
    }
    let (proname, argtypes, ndefaults, defaults) = (
        pos("pg_proc", "proname")?,
        pos("pg_proc", "proargtypes")?,
        pos("pg_proc", "pronargdefaults")?,
        pos("pg_proc", "proargdefaults")?,
    );
    for row in rows_out.get_mut("pg_proc").into_iter().flatten() {
        let Some(text) = row[defaults].take() else { continue };
        let name = row[proname].clone().unwrap_or_default();
        let bad = || format!("pg_proc.dat: bad proargdefaults {text:?} of {name}");
        let items = array_items(&text).ok_or_else(bad)?;
        let args = row[argtypes].clone().unwrap_or_default();
        let args: Vec<&str> = args.split_whitespace().collect();
        if items.len() > args.len() {
            return Err(bad());
        }
        let mut nodes = Vec::new();
        for (item, ty) in items.iter().zip(&args[args.len() - items.len()..]) {
            let (len, by_val, collation) = types.get(*ty).ok_or_else(bad)?;
            let value = match item {
                None => "<>".to_string(),
                Some(input) => datum_text(ty, input).ok_or_else(bad)?,
            };
            nodes.push(format!(
                "{{CONST :consttype {ty} :consttypmod -1 :constcollid {collation} :constlen {len} :constbyval {by_val} :constisnull {} :location -1 :constvalue {value}}}",
                item.is_none()
            ));
        }
        row[defaults] = Some(format!("({})", nodes.join(" ")));
        row[ndefaults] = Some(items.len().to_string());
    }
    Ok(())
}

/// `outDatum` of `outfuncs.c` for the value that the input function of a type gives. A value passed by value is the 8 bytes of the `Datum`. A varlena value is its bytes with a 4-byte header. Each byte is a signed `char`. Only the types and values that `pg_proc.dat` uses are known.
fn datum_text(ty: &str, input: &str) -> Option<String> {
    let (len, bytes): (usize, Vec<u8>) = match ty {
        // bool
        "16" => (
            1,
            u64::from(match input {
                "true" => true,
                "false" => false,
                _ => return None,
            })
            .to_le_bytes()
            .to_vec(),
        ),
        // int8
        "20" => (8, input.parse::<i64>().ok()?.to_le_bytes().to_vec()),
        // int4: Int32GetDatum extends the sign.
        "23" => (4, i64::from(input.parse::<i32>().ok()?).to_le_bytes().to_vec()),
        // float8
        "701" => (8, input.parse::<f64>().ok()?.to_bits().to_le_bytes().to_vec()),
        // text
        "25" => (0, varlena(input.as_bytes())),
        // text[]: only the empty array, which is an ArrayType header with ndim 0 and the element type text.
        "1009" if input == "{}" => (0, varlena(&[0, 0, 0, 0, 0, 0, 0, 0, 25, 0, 0, 0])),
        // jsonb: only the empty object, which is a JsonbContainer header with the count 0 and JB_FOBJECT.
        "3802" if input == "{}" => (0, varlena(&0x2000_0000u32.to_le_bytes())),
        _ => return None,
    };
    let len = if len == 0 { bytes.len() } else { len };
    let items: Vec<String> = bytes.iter().map(|&b| (b as i8).to_string()).collect();
    Some(format!("{len} [ {} ]", items.join(" ")))
}

/// A varlena value with a 4-byte header, as `SET_VARSIZE` writes it on a little-endian machine.
fn varlena(data: &[u8]) -> Vec<u8> {
    let size = u32::try_from(data.len() + 4).unwrap_or(u32::MAX);
    let mut out = (size << 2).to_le_bytes().to_vec();
    out.extend_from_slice(data);
    out
}

/// `DeescapeQuotedString` of `bootscanner.l` without the quotes: the escapes `\b`, `\f`, `\n`, `\r`, `\t` and up to three octal digits. A backslash before any other character gives that character.
fn deescape(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 == bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = bytes[i];
        i += 1;
        match c {
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'0'..=b'7' => {
                let mut n = u32::from(c - b'0');
                for _ in 0..2 {
                    match bytes.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            n = n * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(n as u8);
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The row types of the catalogs that the bootstrap mode makes. For each catalog that is not a bootstrap catalog, in the order of the headers, `heap_create_with_catalog` takes an OID for the array type and then an OID for the row type if the header gives none. In the bootstrap mode the OID counter starts at `FirstGenbkiObjectId` and checks no index.
fn catalog_rowtypes(
    catalogs: &[Catalog],
    lookups: &Lookups,
    first_genbki: u32,
    rows: &mut BTreeMap<String, Vec<Vec<Option<String>>>>,
    types: &mut BTreeMap<String, Row>,
) -> Result<(), String> {
    let pg_type = catalogs.iter().find(|c| c.name == "pg_type").ok_or("no pg_type header")?;
    let proc = |name: &str| -> Result<String, String> {
        lookups["pg_proc"]
            .get(name)
            .cloned()
            .flatten()
            .ok_or_else(|| format!("no pg_proc row {name}"))
    };
    let mut next = first_genbki;
    let mut out = Vec::new();
    for catalog in catalogs.iter().filter(|c| !c.bootstrap) {
        let array_oid = next.to_string();
        next += 1;
        let row_oid = if catalog.rowtype_oid == 0 {
            next += 1;
            (next - 1).to_string()
        } else {
            catalog.rowtype_oid.to_string()
        };
        let common = [
            ("typnamespace", "11"),
            ("typowner", "10"),
            ("typlen", "-1"),
            ("typbyval", "f"),
            ("typispreferred", "f"),
            ("typisdefined", "t"),
            ("typdelim", ","),
            ("typmodin", "0"),
            ("typmodout", "0"),
            ("typalign", "d"),
            ("typstorage", "x"),
            ("typnotnull", "f"),
            ("typbasetype", "0"),
            ("typtypmod", "-1"),
            ("typndims", "0"),
            ("typcollation", "0"),
        ];
        let mut rowtype: Row = common.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let mut array = rowtype.clone();
        for (k, v) in [
            ("oid", row_oid.clone()),
            ("typname", catalog.name.clone()),
            ("typtype", "c".into()),
            ("typcategory", "C".into()),
            ("typrelid", catalog.oid.to_string()),
            ("typsubscript", "0".into()),
            ("typelem", "0".into()),
            ("typarray", array_oid.clone()),
            ("typinput", proc("record_in")?),
            ("typoutput", proc("record_out")?),
            ("typreceive", proc("record_recv")?),
            ("typsend", proc("record_send")?),
            ("typanalyze", "0".into()),
        ] {
            rowtype.insert(k.into(), v);
        }
        for (k, v) in [
            ("oid", array_oid),
            ("typname", format!("_{}", catalog.name)),
            ("typtype", "b".into()),
            ("typcategory", "A".into()),
            ("typrelid", "0".into()),
            ("typsubscript", proc("array_subscript_handler")?),
            ("typelem", row_oid),
            ("typarray", "0".into()),
            ("typinput", proc("array_in")?),
            ("typoutput", proc("array_out")?),
            ("typreceive", proc("array_recv")?),
            ("typsend", proc("array_send")?),
            ("typanalyze", proc("array_typanalyze")?),
        ] {
            array.insert(k.into(), v);
        }
        for row in [array, rowtype] {
            let values = pg_type.columns.iter().map(|c| row.get(&c.name).cloned()).collect();
            out.push(values);
            types.insert(row["typname"].clone(), row);
        }
    }
    rows.get_mut("pg_type").ok_or("no pg_type rows")?.extend(out);
    Ok(())
}

/// `setup_description` of `initdb` gives each function that implements an operator the description `implementation of X operator`, if the function has no description and the description of the operator does not start with `deprecated`.
fn operator_descriptions(
    catalogs: &[Catalog],
    rows: &mut BTreeMap<String, Vec<Vec<Option<String>>>>,
) -> Result<(), String> {
    let index = |catalog: &str, column: &str| -> Result<usize, String> {
        catalogs
            .iter()
            .find(|c| c.name == catalog)
            .and_then(|c| c.columns.iter().position(|col| col.name == column))
            .ok_or_else(|| format!("no column {catalog}.{column}"))
    };
    let class_oid =
        |name: &str| catalogs.iter().find(|c| c.name == name).map(|c| c.oid.to_string());
    let (proc_class, operator_class) = (class_oid("pg_proc"), class_oid("pg_operator"));
    let (d_objoid, d_classoid, d_objsubid, d_description) = (
        index("pg_description", "objoid")?,
        index("pg_description", "classoid")?,
        index("pg_description", "objsubid")?,
        index("pg_description", "description")?,
    );
    let described: BTreeMap<(Option<String>, Option<String>), Option<String>> =
        rows["pg_description"]
            .iter()
            .map(|r| ((r[d_objoid].clone(), r[d_classoid].clone()), r[d_description].clone()))
            .collect();
    let (o_oid, o_name, o_code) = (
        index("pg_operator", "oid")?,
        index("pg_operator", "oprname")?,
        index("pg_operator", "oprcode")?,
    );
    let mut added = Vec::new();
    let mut seen = BTreeSet::new();
    for op in &rows["pg_operator"] {
        let code = op[o_code].clone();
        if described.contains_key(&(code.clone(), proc_class.clone())) {
            continue;
        }
        if let Some(Some(d)) = described.get(&(op[o_oid].clone(), operator_class.clone()))
            && d.starts_with("deprecated")
        {
            continue;
        }
        if !seen.insert(code.clone()) {
            return Err(format!("two operators have the function {code:?} and no description"));
        }
        let name = op[o_name].clone().unwrap_or_default();
        let mut row = vec![None; 4];
        row[d_objoid] = code;
        row[d_classoid] = proc_class.clone();
        row[d_objsubid] = Some("0".into());
        row[d_description] = Some(format!("implementation of {name} operator"));
        added.push(row);
    }
    rows.get_mut("pg_description").ok_or("no pg_description rows")?.extend(added);
    Ok(())
}

fn defined_symbol(header: &str, symbol: &str) -> Result<u32, String> {
    header
        .lines()
        .find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("#define") && w.next() == Some(symbol)).then(|| w.next()).flatten()
        })
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("transam.h has no {symbol}"))
}

/// The Rust form of the values of one column type.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Char,
    Int2,
    Int4,
    Float4,
    Oid,
    Text,
    Int2Vector,
    OidVector,
    OidArray,
    CharArray,
    TextArray,
}

impl Kind {
    fn of(ty: &str) -> Kind {
        match ty {
            "bool" => Kind::Bool,
            "char" => Kind::Char,
            "int2" => Kind::Int2,
            "int4" => Kind::Int4,
            "float4" => Kind::Float4,
            "oid" | "regproc" | "xid" => Kind::Oid,
            "int2vector" => Kind::Int2Vector,
            "oidvector" => Kind::OidVector,
            "_oid" => Kind::OidArray,
            "_char" => Kind::CharArray,
            t if t.starts_with('_') => Kind::TextArray,
            _ => Kind::Text,
        }
    }

    fn variant(self) -> &'static str {
        match self {
            Kind::Bool => "Bool",
            Kind::Char => "Char",
            Kind::Int2 => "Int2",
            Kind::Int4 => "Int4",
            Kind::Float4 => "Float4",
            Kind::Oid => "Oid",
            Kind::Text => "Text",
            Kind::Int2Vector => "Int2Vector",
            Kind::OidVector => "OidVector",
            Kind::OidArray => "OidArray",
            Kind::CharArray => "CharArray",
            Kind::TextArray => "TextArray",
        }
    }

    /// The Rust literal of one value. A null gets the zero value of the kind.
    fn literal(self, value: Option<&str>, at: &str) -> Result<String, String> {
        let bad = || format!("{at}: bad value {value:?}");
        let Some(v) = value else {
            return Ok(match self {
                Kind::Bool => "false".into(),
                Kind::Char | Kind::Int2 | Kind::Int4 | Kind::Oid => "0".into(),
                Kind::Float4 => "0.0".into(),
                Kind::Text => "\"\"".into(),
                _ => "&[]".into(),
            });
        };
        Ok(match self {
            Kind::Bool => match v {
                "t" => "true".into(),
                "f" => "false".into(),
                _ => return Err(bad()),
            },
            Kind::Char => match v.as_bytes() {
                [] => "0".into(),
                [b] => byte_literal(*b),
                _ => return Err(bad()),
            },
            Kind::Int2 => v.parse::<i16>().map_err(|_| bad())?.to_string(),
            Kind::Int4 => v.parse::<i32>().map_err(|_| bad())?.to_string(),
            Kind::Float4 => format!("{:?}", v.parse::<f32>().map_err(|_| bad())?),
            Kind::Oid => match v {
                "-" => "0".into(),
                _ => v.parse::<u32>().map_err(|_| bad())?.to_string(),
            },
            Kind::Text => format!("{v:?}"),
            Kind::Int2Vector => {
                let n: Vec<String> = v
                    .split_whitespace()
                    .map(|w| w.parse::<i16>().map(|n| n.to_string()))
                    .collect::<Result<_, _>>()
                    .map_err(|_| bad())?;
                format!("&[{}]", n.join(", "))
            }
            Kind::OidVector => {
                let n: Vec<String> = v
                    .split_whitespace()
                    .map(|w| w.parse::<u32>().map(|n| n.to_string()))
                    .collect::<Result<_, _>>()
                    .map_err(|_| bad())?;
                format!("&[{}]", n.join(", "))
            }
            Kind::OidArray => {
                let n: Vec<String> = array_elements(v)
                    .ok_or_else(bad)?
                    .iter()
                    .map(|w| w.parse::<u32>().map(|n| n.to_string()))
                    .collect::<Result<_, _>>()
                    .map_err(|_| bad())?;
                format!("&[{}]", n.join(", "))
            }
            Kind::CharArray => {
                let mut n = Vec::new();
                for e in array_elements(v).ok_or_else(bad)? {
                    match e.as_bytes() {
                        [b] => n.push(byte_literal(*b)),
                        _ => return Err(bad()),
                    }
                }
                format!("&[{}]", n.join(", "))
            }
            Kind::TextArray => {
                let n: Vec<String> =
                    array_elements(v).ok_or_else(bad)?.iter().map(|e| format!("{e:?}")).collect();
                format!("&[{}]", n.join(", "))
            }
        })
    }
}

/// A byte as a Rust literal: `b'x'` for printable ASCII, a number for any other byte.
fn byte_literal(b: u8) -> String {
    if b == b' ' || b.is_ascii_graphic() { format!("b{:?}", char::from(b)) } else { b.to_string() }
}

/// The elements of a one-dimensional array literal such as `{a,"b c"}`. A null element is not allowed.
fn array_elements(text: &str) -> Option<Vec<String>> {
    array_items(text)?.into_iter().collect()
}

/// The elements of a one-dimensional array literal, as `array_in` reads them. An unquoted `NULL` is `None`.
fn array_items(text: &str) -> Option<Vec<Option<String>>> {
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    let mut out = Vec::new();
    if inner.is_empty() {
        return Some(out);
    }
    let mut chars = inner.chars().peekable();
    loop {
        let mut element = String::new();
        let mut quoted = false;
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next_if_eq(&'"').is_some() {
            quoted = true;
            loop {
                match chars.next()? {
                    '\\' => element.push(chars.next()?),
                    '"' => break,
                    c => element.push(c),
                }
            }
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                if c == '\\' {
                    element.push(chars.next()?);
                } else {
                    element.push(c);
                }
            }
            element = element.trim_end().to_string();
        }
        let null = !quoted && element.eq_ignore_ascii_case("NULL");
        out.push((!null).then_some(element));
        match chars.next() {
            None => return Some(out),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

const HEAD: &str = "`cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.";

/// The file with the schema of each catalog. The column properties follow `morph_row_for_pgattr` of `genbki.pl`.
fn catalogs_file(
    catalogs: &[Catalog],
    types: &BTreeMap<String, Row>,
    c_collation: &str,
    rows: &BTreeMap<String, Vec<Vec<Option<String>>>>,
) -> Result<String, String> {
    let mut out = String::new();
    writeln!(out, "//! The schemas of the {} system catalogs, in the order of `genbki.pl`.\n//!\n//! {HEAD}\n", catalogs.len()).unwrap();
    out.push_str("use crate::{Catalog, Column};\n\n");
    writeln!(
        out,
        "/// The system catalogs.\npub(crate) static CATALOGS: [Catalog; {}] = [",
        catalogs.len()
    )
    .unwrap();
    for catalog in catalogs {
        let name = &catalog.name;
        writeln!(out, "    Catalog {{").unwrap();
        writeln!(out, "        name: {name:?},").unwrap();
        writeln!(out, "        oid: {},", catalog.oid).unwrap();
        writeln!(out, "        rowtype_oid: {},", catalog.rowtype_oid).unwrap();
        writeln!(out, "        shared: {},", catalog.shared).unwrap();
        writeln!(out, "        bootstrap: {},", catalog.bootstrap).unwrap();
        writeln!(out, "        columns: &[").unwrap();
        let mut prior_fixed = true;
        for column in &catalog.columns {
            let ty = types
                .get(&column.ty)
                .ok_or_else(|| format!("{name}: no pg_type row for {}", column.ty))?;
            let len: i16 = match ty["typlen"].as_str() {
                "NAMEDATALEN" => 64,
                n => n.parse().map_err(|_| format!("pg_type: bad typlen {n}"))?,
            };
            let not_null = if column.force_not_null {
                true
            } else if column.force_null {
                false
            } else {
                prior_fixed && len > 0
            };
            prior_fixed &= not_null && len > 0;
            let collation = if ty["typcollation"] == "0" { "0" } else { c_collation };
            let ndims = i16::from(ty["typcategory"] == "A");
            let type_oid = &ty["oid"];
            let by_val = match ty["typbyval"].as_str() {
                "t" | "FLOAT8PASSBYVAL" => true,
                "f" => false,
                v => return Err(format!("pg_type: bad typbyval {v}")),
            };
            let align = match ty["typalign"].as_str() {
                "ALIGNOF_POINTER" => "d",
                a => a,
            };
            writeln!(
                out,
                "            Column {{ name: {:?}, type_oid: {type_oid}, len: {len}, by_val: {by_val}, align: b'{align}', storage: b'{}', ndims: {ndims}, collation: {collation}, not_null: {not_null} }},",
                column.name, ty["typstorage"]
            )
            .unwrap();
        }
        writeln!(out, "        ],").unwrap();
        match rows.get(name) {
            Some(r) => {
                writeln!(out, "        rows: &super::{name}::COLUMNS,").unwrap();
                writeln!(out, "        len: {},", r.len()).unwrap();
            }
            None => {
                writeln!(out, "        rows: &[],").unwrap();
                writeln!(out, "        len: 0,").unwrap();
            }
        }
        writeln!(out, "    }},").unwrap();
    }
    out.push_str("];\n");
    Ok(out)
}

/// The file with the static rows of one catalog, one batch for each column.
fn rows_file(catalog: &Catalog, rows: &[Vec<Option<String>>]) -> Result<String, String> {
    let name = &catalog.name;
    let mut out = String::new();
    writeln!(
        out,
        "//! The {} static rows of `{name}`, one batch for each column.\n//!\n//! {HEAD}\n",
        rows.len()
    )
    .unwrap();
    out.push_str("use crate::{Batch, Values};\n\n");
    writeln!(out, "pub(crate) static COLUMNS: [Batch; {}] = [", catalog.columns.len()).unwrap();
    for (i, column) in catalog.columns.iter().enumerate() {
        let kind = Kind::of(&column.ty);
        let at = format!("{name}.{}", column.name);
        writeln!(out, "    // {}", column.name).unwrap();
        if rows.iter().all(|r| r[i].is_none()) {
            out.push_str("    Batch::NULL,\n");
            continue;
        }
        let mut nulls = vec![0u64; rows.len().div_ceil(64)];
        for (n, row) in rows.iter().enumerate() {
            if row[i].is_none() {
                nulls[n / 64] |= 1 << (n % 64);
            }
        }
        let items: Vec<String> =
            rows.iter().map(|r| kind.literal(r[i].as_deref(), &at)).collect::<Result<_, _>>()?;
        writeln!(out, "    Batch {{").unwrap();
        writeln!(out, "        values: Values::{}(&[", kind.variant()).unwrap();
        wrap(&mut out, "            ", &items);
        writeln!(out, "        ]),").unwrap();
        if nulls.iter().all(|w| *w == 0) {
            writeln!(out, "        nulls: &[],").unwrap();
        } else {
            let words: Vec<String> = nulls.iter().map(|w| format!("0x{w:016x}")).collect();
            writeln!(out, "        nulls: &[").unwrap();
            wrap(&mut out, "            ", &words);
            writeln!(out, "        ],").unwrap();
        }
        writeln!(out, "    }},").unwrap();
    }
    out.push_str("];\n");
    Ok(out)
}

fn mod_file(catalogs: &[Catalog], rows: &BTreeMap<String, Vec<Vec<Option<String>>>>) -> String {
    let mut out = format!("//! The generated schemas and static rows.\n//!\n//! {HEAD}\n\n");
    out.push_str("#![allow(clippy::unreadable_literal, clippy::approx_constant, clippy::byte_char_slices)]\n\n");
    out.push_str("// The files have the layout of the generator. rustfmt does not change them, so that `cargo xtask pgcatalog --check` can compare them byte for byte.\n");
    let mut names: Vec<&str> =
        catalogs.iter().filter(|c| rows.contains_key(&c.name)).map(|c| c.name.as_str()).collect();
    names.push("catalogs");
    names.sort_unstable();
    for name in names {
        writeln!(out, "#[rustfmt::skip]\npub(crate) mod {name};").unwrap();
    }
    out
}

/// Writes items with a comma after each, up to 100 columns on each line.
fn wrap(out: &mut String, indent: &str, items: &[String]) {
    let mut line = String::new();
    for item in items {
        if !line.is_empty() && indent.len() + line.len() + item.len() + 1 > 100 {
            writeln!(out, "{indent}{}", line.trim_end()).unwrap();
            line.clear();
        }
        line.push_str(item);
        line.push_str(", ");
    }
    if !line.is_empty() {
        writeln!(out, "{indent}{}", line.trim_end()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_literals() {
        assert_eq!(array_elements("{}"), Some(vec![]));
        assert_eq!(array_elements("{a,b}"), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(
            array_elements("{\"a b\",\"\",c\\,d}"),
            Some(vec!["a b".into(), String::new(), "c,d".into()])
        );
        assert_eq!(array_elements("{a,NULL}"), None);
        assert_eq!(array_elements("a"), None);
    }

    #[test]
    fn bootstrap_values() {
        assert_eq!(bootstrap_value("_null_"), None);
        assert_eq!(bootstrap_value("\\0"), Some(String::new()));
        assert_eq!(bootstrap_value("\\054"), Some(",".into()));
        assert_eq!(bootstrap_value("NAMEDATALEN"), Some("64".into()));
        assert_eq!(bootstrap_value("DATLOCALE"), None);
    }

    #[test]
    fn input_functions() {
        assert_eq!(
            acl_text("{POSTGRES=X,pg_monitor=X}").as_deref(),
            Some("{postgres=X/postgres,pg_monitor=X/postgres}")
        );
        assert_eq!(acl_text("{=X*r/POSTGRES}").as_deref(), Some("{=rX*/postgres}"));
        assert_eq!(acl_text("{nobody=X}"), None);
        assert_eq!(datum_text("16", "true").as_deref(), Some("1 [ 1 0 0 0 0 0 0 0 ]"));
        assert_eq!(datum_text("23", "-1").as_deref(), Some("4 [ -1 -1 -1 -1 -1 -1 -1 -1 ]"));
        assert_eq!(datum_text("701", "1").as_deref(), Some("8 [ 0 0 0 0 0 0 -16 63 ]"));
        assert_eq!(datum_text("25", "NFC").as_deref(), Some("7 [ 28 0 0 0 78 70 67 ]"));
        assert_eq!(datum_text("3802", "{}").as_deref(), Some("8 [ 32 0 0 0 0 0 0 32 ]"));
        assert_eq!(datum_text("3802", "{\"a\": 1}"), None);
        assert_eq!(array_items("{NULL,\"NULL\"}"), Some(vec![None, Some("NULL".into())]));
    }

    #[test]
    fn header_columns() {
        let text = "CATALOG(pg_x,9,XId) BKI_SHARED_RELATION BKI_ROWTYPE_OID(8,XRowtype)\n{\n\tOid oid; /* the oid */\n\tNameData x_name;\n\tint16 x_len BKI_DEFAULT(-1) BKI_ARRAY_DEFAULT(-1);\n#ifdef CATALOG_VARLEN\n\ttext x_text[1] BKI_FORCE_NULL;\n#endif\n} FormData_pg_x;\n#ifdef EXPOSE_TO_CLIENT_CODE\n#define X 1\n#endif\n";
        let c = parse_header("pg_x", text).unwrap();
        assert_eq!((c.oid, c.rowtype_oid, c.shared, c.bootstrap), (9, 8, true, false));
        let names: Vec<(&str, &str)> =
            c.columns.iter().map(|c| (c.name.as_str(), c.ty.as_str())).collect();
        assert_eq!(
            names,
            [("oid", "oid"), ("x_name", "name"), ("x_len", "int2"), ("x_text", "_text")]
        );
        assert_eq!(c.columns[2].default.as_deref(), Some("-1"));
        assert!(c.columns[3].force_null);
    }
}

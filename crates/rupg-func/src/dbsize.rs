//! The functions of `dbsize.c`: the sizes of the relations, the databases and the tablespaces, `pg_size_pretty`, `pg_size_bytes`, and the files of the relations. The module also has `pg_tablespace_location` and `pg_tablespace_databases` of `misc.c`.
//!
//! rupg keeps no files, so the size of a relation is the size of the files that PostgreSQL makes for an empty relation of the same kind: no pages for a table, a TOAST table and a view, one page for a sequence, and the first pages of each index access method. The relations of the system catalogs have no pages. The size of a database is the sum of the sizes of the relations of the catalog of the session.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned};
use rupg_types::{Numeric, Value, numeric_in, numeric_out};

use crate::acl;
use crate::{Call, Kernel, bad_value, type_error};

/// `BLCKSZ`.
const BLOCK: i64 = 8192;
/// The OID of `pg_default`, `DEFAULTTABLESPACE_OID`.
const DEFAULT_TABLESPACE: u32 = 1663;
/// The OID of `pg_global`, `GLOBALTABLESPACE_OID`.
const GLOBAL_TABLESPACE: u32 = 1664;
/// The OID of the access method `brin`, the one access method that makes a free space map for an empty index.
const BRIN: u32 = 3580;
/// The pages of the free space map of an empty BRIN index.
const BRIN_FSM_PAGES: i64 = 3;

/// A unit of `pg_size_pretty`: the name, the limit before the half rounding in this unit, the half rounding, and the bits of one of this unit. Each unit is a power of 2.
struct Unit {
    name: &'static str,
    limit: i64,
    round: bool,
    bits: u32,
}

/// `size_pretty_units`.
const UNITS: [Unit; 6] = [
    Unit { name: "bytes", limit: 10 * 1024, round: false, bits: 0 },
    Unit { name: "kB", limit: 20 * 1024 - 1, round: true, bits: 10 },
    Unit { name: "MB", limit: 20 * 1024 - 1, round: true, bits: 20 },
    Unit { name: "GB", limit: 20 * 1024 - 1, round: true, bits: 30 },
    Unit { name: "TB", limit: 20 * 1024 - 1, round: true, bits: 40 },
    Unit { name: "PB", limit: 20 * 1024 - 1, round: true, bits: 50 },
];

/// The bits of the divisor from a unit to the next unit. It is 1 bit less when the next unit rounds, and 1 bit more when this unit rounds and the next one does not.
fn shift(at: usize) -> u32 {
    let (unit, next) = (&UNITS[at], &UNITS[at + 1]);
    next.bits - unit.bits - u32::from(next.round) + u32::from(unit.round)
}

/// `pg_size_pretty(bigint)`.
fn size_pretty(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let mut size = args.first().and_then(Value::as_i64).ok_or_else(bad_value)?;
    for (at, unit) in UNITS.iter().enumerate() {
        if at + 1 == UNITS.len() || size.unsigned_abs() < unit.limit.unsigned_abs() {
            if unit.round {
                // `half_rounded`: divide by 2 and round away from zero.
                size = (size + if size < 0 { -1 } else { 1 }) / 2;
            }
            return Ok(Value::Text(format!("{size} {}", unit.name)));
        }
        size /= 1i64 << shift(at);
    }
    Err(bad_value())
}

/// `pg_size_pretty(numeric)`.
fn size_pretty_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Numeric(size)) = args.first() else { return Err(bad_value()) };
    let mut size = size.clone();
    for (at, unit) in UNITS.iter().enumerate() {
        if at + 1 == UNITS.len()
            || size.abs().compare(&Numeric::from_integer(unit.limit.into())).is_lt()
        {
            if unit.round {
                let one =
                    Numeric::from_integer(if size.compare(&Numeric::from_integer(0)).is_ge() {
                        1
                    } else {
                        -1
                    });
                size = size
                    .add(&one)
                    .and_then(|s| s.div_trunc(&Numeric::from_integer(2)))
                    .map_err(type_error)?;
            }
            let mut out = Vec::new();
            numeric_out(&size, &mut out);
            return Ok(Value::Text(format!("{} {}", String::from_utf8_lossy(&out), unit.name)));
        }
        let divisor = Numeric::from_integer(1i128 << shift(at));
        size = size.div_trunc(&divisor).map_err(type_error)?;
    }
    Err(bad_value())
}

/// `isspace` of the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The end of the number that `strtol` reads at `at`: the spaces, a sign and the digits. It is `at` when there are no digits.
fn strtol_end(s: &[u8], at: usize) -> usize {
    let mut end = at;
    while s.get(end).copied().is_some_and(is_space) {
        end += 1;
    }
    if matches!(s.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    let digits = end;
    while s.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == digits { at } else { end }
}

/// The end of a run of digits at `at`.
fn digits_end(s: &[u8], mut at: usize) -> usize {
    while s.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    at
}

/// `pg_size_bytes(text)`: a size with an optional unit, as `pg_size_pretty` writes it, in bytes. The number can have a sign, a fraction and an exponent, and the unit can have any case.
fn size_bytes(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let s = text.as_bytes();
    let invalid =
        || Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid size: \"{text}\""));
    let mut start = 0;
    while s.get(start).copied().is_some_and(is_space) {
        start += 1;
    }
    let mut end = start;
    if matches!(s.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    let after = digits_end(s, end);
    let mut have_digits = after > end;
    end = after;
    if s.get(end) == Some(&b'.') {
        let after = digits_end(s, end + 1);
        have_digits |= after > end + 1;
        end = after;
    }
    if !have_digits {
        return Err(invalid());
    }
    if matches!(s.get(end), Some(b'e' | b'E')) {
        // A unit such as `EB` can follow, so the `e` is a part of the number only when digits follow it.
        let after = strtol_end(s, end + 1);
        if after > end + 1 {
            end = after;
        }
    }
    let mut num = numeric_in(&text[start..end], -1).map_err(type_error)?;
    let mut unit_at = end;
    while s.get(unit_at).copied().is_some_and(is_space) {
        unit_at += 1;
    }
    if unit_at < s.len() {
        let mut unit_end = s.len();
        while unit_end > unit_at && is_space(s[unit_end - 1]) {
            unit_end -= 1;
        }
        let name = &text[unit_at..unit_end];
        let found = UNITS.iter().find(|u| u.name.eq_ignore_ascii_case(name)).or_else(|| {
            // `size_bytes_aliases`: `B` is `bytes`.
            name.eq_ignore_ascii_case("B").then_some(&UNITS[0])
        });
        let Some(unit) = found else {
            return Err(invalid()
                .with_detail(format!("Invalid size unit: \"{name}\"."))
                .with_hint(
                    "Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\".",
                ));
        };
        if unit.bits > 0 {
            num = Numeric::from_integer(1i128 << unit.bits).mul(&num).map_err(type_error)?;
        }
    }
    let value = num.to_integer("bigint").map_err(type_error)?;
    let value = i64::try_from(value)
        .map_err(|_| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?;
    Ok(Value::Int8(value))
}

/// The storage of a relation, the columns of `pg_class` that the functions of this module read.
struct Storage {
    oid: u32,
    /// `relkind`.
    kind: u8,
    /// `relisshared`.
    shared: bool,
    /// `relfilenode`, or the OID for a mapped catalog.
    filenode: u32,
    /// The pages of the main fork and of the free space map.
    pages: (i64, i64),
    /// `reltoastrelid`.
    toast: u32,
}

impl Storage {
    fn has_storage(&self) -> bool {
        has_storage(self.kind)
    }
}

/// `RELKIND_HAS_STORAGE`.
fn has_storage(kind: u8) -> bool {
    matches!(kind, b'r' | b'i' | b'S' | b't' | b'm')
}

/// The storage of the relation with this OID, or `None` when no relation has it. A relation of the catalog of the session has the pages of a new relation of its kind.
fn storage(relid: u32, call: &Call<'_>) -> Option<Storage> {
    if let Some(rel) = call.session.catalog().and_then(|c| c.relation(relid)) {
        let pages = match (rel.kind, &rel.index) {
            (RelKind::Index, Some(index)) => {
                let fsm = if index.method == BRIN { BRIN_FSM_PAGES } else { 0 };
                (i64::from(index.pages), fsm)
            }
            (RelKind::Sequence, _) => (1, 0),
            _ => (0, 0),
        };
        let filenode = match (&rel.sequence, rel.kind) {
            (Some(info), _) => info.file,
            (None, RelKind::View | RelKind::Composite) => 0,
            (None, _) => rel.oid,
        };
        return Some(Storage {
            oid: rel.oid,
            kind: rel.kind.code() as u8,
            shared: false,
            filenode,
            pages,
            toast: rel.toast,
        });
    }
    let row = builtin::class_by_oid(relid)?;
    let filenode = if row.filenode == 0 && has_storage(row.kind) {
        // A mapped catalog: the relation mapper gives the OID until a rewrite such as `VACUUM FULL`.
        row.oid
    } else {
        row.filenode
    };
    Some(Storage {
        oid: row.oid,
        kind: row.kind,
        shared: row.shared,
        filenode,
        pages: (0, 0),
        toast: 0,
    })
}

/// `forkname_to_number`: the pages of a fork.
fn fork_pages(rel: &Storage, fork: &str) -> Result<i64> {
    match fork {
        "main" => Ok(rel.pages.0),
        "fsm" => Ok(rel.pages.1),
        "vm" | "init" => Ok(0),
        _ => Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "invalid fork name")
            .with_hint("Valid fork names are \"main\", \"fsm\", \"vm\", and \"init\".")),
    }
}

/// The size of each fork of a relation.
fn forks_size(rel: &Storage) -> i64 {
    (rel.pages.0 + rel.pages.1) * BLOCK
}

/// The indexes of a relation of the catalog of the session.
fn indexes_of(relid: u32, call: &Call<'_>) -> Vec<u32> {
    let Some(catalog) = call.session.catalog() else { return Vec::new() };
    catalog
        .relations()
        .filter(|r| r.index.as_ref().is_some_and(|i| i.table == relid))
        .map(|r| r.oid)
        .collect()
}

/// `calculate_indexes_size`.
fn indexes_size(relid: u32, call: &Call<'_>) -> i64 {
    indexes_of(relid, call)
        .into_iter()
        .filter_map(|i| storage(i, call))
        .map(|i| forks_size(&i))
        .sum()
}

/// `calculate_table_size`: each fork, then the TOAST table with its indexes.
fn table_size(rel: &Storage, call: &Call<'_>) -> i64 {
    let mut size = forks_size(rel);
    if let Some(toast) = (rel.toast != 0).then(|| storage(rel.toast, call)).flatten() {
        size += forks_size(&toast) + indexes_size(toast.oid, call);
    }
    size
}

/// The OID argument at `index`.
fn oid_arg(args: &[Value], index: usize) -> Result<u32> {
    args.get(index).and_then(Value::as_oid).ok_or_else(bad_value)
}

/// `pg_relation_size(regclass, text)`: the size of one fork. The result is null for an OID that no relation has.
fn relation_size(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(rel) = storage(oid_arg(args, 0)?, call) else { return Ok(Value::Null) };
    let fork = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(Value::Int8(fork_pages(&rel, fork)? * BLOCK))
}

/// `pg_table_size`, `pg_indexes_size` and `pg_total_relation_size`.
fn sizes(call: &Call<'_>, args: &[Value], table: bool, indexes: bool) -> Result<Value> {
    let Some(rel) = storage(oid_arg(args, 0)?, call) else { return Ok(Value::Null) };
    let mut size = 0;
    if table {
        size += table_size(&rel, call);
    }
    if indexes {
        size += indexes_size(rel.oid, call);
    }
    Ok(Value::Int8(size))
}

/// The OID of the current database.
fn current_database(call: &Call<'_>) -> Result<u32> {
    acl::simple_name(Owned::Database, call.session.database(), call.session)
}

/// `pg_relation_filenode(regclass)`: null for a relation with no storage.
fn relation_filenode(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(rel) = storage(oid_arg(args, 0)?, call) else { return Ok(Value::Null) };
    if !rel.has_storage() || rel.filenode == 0 {
        return Ok(Value::Null);
    }
    Ok(Value::Oid(rel.filenode))
}

/// `pg_relation_filepath(regclass)`: the path of the main fork in the data directory, `global/` for a shared catalog and `base/` with the OID of the database for the others.
fn relation_filepath(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(rel) = storage(oid_arg(args, 0)?, call) else { return Ok(Value::Null) };
    if !rel.has_storage() || rel.filenode == 0 {
        return Ok(Value::Null);
    }
    if rel.shared {
        return Ok(Value::Text(format!("global/{}", rel.filenode)));
    }
    Ok(Value::Text(format!("base/{}/{}", current_database(call)?, rel.filenode)))
}

/// `pg_filenode_relation(oid, oid)`: the relation of a file in a tablespace, where 0 is the default tablespace of the database, as `RelidByRelfilenumber` finds it. A file in `pg_global` is a shared catalog of the relation mapper. In another tablespace, a relation of `pg_class` with this `relfilenode` comes first, then a catalog of the relation mapper that is not shared. Each relation of rupg is in the default tablespace. The result is null when no relation has the file.
fn filenode_relation(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (tablespace, filenode) = (oid_arg(args, 0)?, oid_arg(args, 1)?);
    if filenode == 0 {
        return Ok(Value::Null);
    }
    let mapped = |shared: bool| {
        builtin::classes()
            .iter()
            .find(|r| {
                r.filenode == 0 && r.shared == shared && r.oid == filenode && has_storage(r.kind)
            })
            .map(|r| r.oid)
    };
    let found = match tablespace {
        GLOBAL_TABLESPACE => mapped(true),
        0 | DEFAULT_TABLESPACE => {
            let plain = builtin::classes()
                .iter()
                .find(|r| !r.shared && r.filenode == filenode)
                .map(|r| r.oid);
            let session = || {
                call.session.catalog()?.relations().map(|r| r.oid).find(|&oid| {
                    storage(oid, call).is_some_and(|s| s.has_storage() && s.filenode == filenode)
                })
            };
            plain.or_else(session).or_else(|| mapped(false))
        }
        _ => mapped(false),
    };
    Ok(found.map_or(Value::Null, Value::Oid))
}

/// The OID of a database by its OID, with the error of PostgreSQL when no database has it.
fn database_by_oid(oid: u32, call: &Call<'_>) -> Result<u32> {
    let known = builtin::owned_by_oid(Owned::Database, oid).is_some()
        || call.session.databases().iter().any(|d| d.oid == oid);
    if !known {
        return Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("database with OID {oid} does not exist"),
        ));
    }
    Ok(oid)
}

/// The name of a database or a tablespace for the error of a missing privilege.
fn object_name(kind: Owned, oid: u32, call: &Call<'_>) -> String {
    builtin::owned_by_oid(kind, oid).map(|r| r.name.to_string()).unwrap_or_else(|| {
        let databases = call.session.databases();
        databases.iter().find(|d| d.oid == oid).map(|d| d.name.clone()).unwrap_or_default()
    })
}

/// The error of `aclcheck_error` for a missing privilege.
fn denied(kind: Owned, oid: u32, call: &Call<'_>) -> Error {
    let what = if kind == Owned::Database { "database" } else { "tablespace" };
    Error::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!("permission denied for {what} {}", object_name(kind, oid, call)),
    )
}

/// The size of the relations of the catalog of the session.
fn session_size(call: &Call<'_>) -> i64 {
    let Some(catalog) = call.session.catalog() else { return 0 };
    let oids: Vec<u32> = catalog.relations().map(|r| r.oid).collect();
    oids.into_iter().filter_map(|oid| storage(oid, call)).map(|r| forks_size(&r)).sum()
}

/// `calculate_database_size`: the user needs `CONNECT` on the database or the privileges of `pg_read_all_stats`. rupg knows the relations of the current database only, so another database has no size. A size of 0 gives null, as for a database with no files.
fn database_size(oid: u32, call: &Call<'_>) -> Result<Value> {
    if !acl::reads_all_stats(call.session)
        && !acl::current_user_has(Owned::Database, oid, acl::ACL_CONNECT, call.session)?
    {
        return Err(denied(Owned::Database, oid, call));
    }
    let size = if oid == current_database(call)? { session_size(call) } else { 0 };
    Ok(if size == 0 { Value::Null } else { Value::Int8(size) })
}

/// `pg_database_size(oid)`.
fn database_size_oid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    database_size(database_by_oid(oid_arg(args, 0)?, call)?, call)
}

/// `pg_database_size(name)`.
fn database_size_name(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    database_size(acl::simple_name(Owned::Database, name, call.session)?, call)
}

/// `calculate_tablespace_size`: the user needs `CREATE` on the tablespace or the privileges of `pg_read_all_stats`, except for the default tablespace of the database. `pg_default` has the relations of the current database, and `pg_global` has the shared catalogs, which have no pages.
fn tablespace_size(oid: u32, call: &Call<'_>) -> Result<Value> {
    if oid != DEFAULT_TABLESPACE
        && !acl::reads_all_stats(call.session)
        && !acl::current_user_has(Owned::Tablespace, oid, acl::ACL_CREATE, call.session)?
    {
        return Err(denied(Owned::Tablespace, oid, call));
    }
    Ok(Value::Int8(if oid == DEFAULT_TABLESPACE { session_size(call) } else { 0 }))
}

/// `pg_tablespace_size(oid)`.
fn tablespace_size_oid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    if builtin::owned_by_oid(Owned::Tablespace, oid).is_none() {
        return Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("tablespace with OID {oid} does not exist"),
        ));
    }
    tablespace_size(oid, call)
}

/// `pg_tablespace_size(name)`.
fn tablespace_size_name(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    tablespace_size(acl::simple_name(Owned::Tablespace, name, call.session)?, call)
}

/// `pg_tablespace_location(oid)`: the empty string for `pg_default` and `pg_global`, where 0 is the default tablespace of the database. Another tablespace needs its link in `pg_tblspc`, and rupg has none.
fn tablespace_location(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    if matches!(oid, 0 | DEFAULT_TABLESPACE | GLOBAL_TABLESPACE) {
        return Ok(Value::text(""));
    }
    Err(Error::new(
        SqlState::UNDEFINED_FILE,
        format!("could not stat file \"pg_tblspc/{oid}\": No such file or directory"),
    ))
}

/// `pg_tablespace_databases(oid)`: the databases with files in a tablespace. Each database of rupg is in `pg_default`. `pg_global` and a tablespace with no directory give a warning and no rows.
pub(crate) fn tablespace_databases(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let oid = oid_arg(args, 0)?;
    if oid == GLOBAL_TABLESPACE {
        call.session.warn(Error::new(SqlState::WARNING, "global tablespace never has databases"));
        return Ok(Vec::new());
    }
    if oid != DEFAULT_TABLESPACE {
        call.session.warn(Error::new(SqlState::WARNING, format!("{oid} is not a tablespace OID")));
        return Ok(Vec::new());
    }
    let mut oids: Vec<u32> = builtin::owned(Owned::Database).iter().map(|r| r.oid).collect();
    oids.extend(call.session.databases().iter().map(|d| d.oid));
    oids.sort_unstable();
    oids.dedup();
    Ok(oids.into_iter().map(|oid| vec![Value::Oid(oid)]).collect())
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_size_pretty" => size_pretty,
        "pg_size_pretty_numeric" => size_pretty_numeric,
        "pg_size_bytes" => size_bytes,
        "pg_relation_size" => relation_size,
        "pg_table_size" => |c, a| sizes(c, a, true, false),
        "pg_indexes_size" => |c, a| sizes(c, a, false, true),
        "pg_total_relation_size" => |c, a| sizes(c, a, true, true),
        "pg_relation_filenode" => relation_filenode,
        "pg_relation_filepath" => relation_filepath,
        "pg_filenode_relation" => filenode_relation,
        "pg_database_size_oid" => database_size_oid,
        "pg_database_size_name" => database_size_name,
        "pg_tablespace_size_oid" => tablespace_size_oid,
        "pg_tablespace_size_name" => tablespace_size_name,
        "pg_tablespace_location" => tablespace_location,
        _ => return None,
    })
}

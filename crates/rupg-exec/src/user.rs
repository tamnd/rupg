//! The rows of the catalog for the user objects: the schemas, the relations, their row types and columns, the column defaults, the constraints, the triggers of the foreign keys, the indexes, the sequences, the functions, the objects of text search, the dependencies between them and their comments. A scan of a table of the catalog gives these rows after the static rows.
//!
//! The columns of `pg_node_tree` hold the expressions in the form of the analyzer, not in the form of `nodeToString`.

use rupg_catalog::{Catalog, ConKind, Constraint, Function, RangePart, RelKind, Relation, Type};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_pgcatalog::{Catalog as Table, builtin};
use rupg_types::{Array, Value};

use crate::scan::value;

/// `FirstNormalTransactionId`, the `relfrozenxid` of the tables of the catalog.
const FIRST_NORMAL_XID: u32 = 3;
/// The OID of the access method `heap`.
const HEAP: u32 = 2;

/// A row of a table of the catalog.
struct Row<'a> {
    table: &'a Table,
    values: Vec<Value>,
}

impl<'a> Row<'a> {
    /// A row with a null in each column.
    fn new(table: &'a Table) -> Row<'a> {
        Row { table, values: vec![Value::Null; table.columns.len()] }
    }

    /// A copy of a static row.
    fn copy(table: &'a Table, row: usize, session: &dyn Session) -> Result<Row<'a>> {
        let values = table
            .columns
            .iter()
            .zip(table.rows)
            .map(|(column, batch)| value(batch, row, column.type_oid, session))
            .collect::<Result<_>>()?;
        Ok(Row { table, values })
    }

    /// Sets the value of a column.
    fn set(&mut self, name: &str, value: Value) -> &mut Row<'a> {
        if let Some(i) = self.table.columns.iter().position(|c| c.name == name) {
            self.values[i] = value;
        }
        self
    }
}

/// The value of a static row of a column.
fn cell(table: &Table, row: usize, name: &str, session: &dyn Session) -> Result<Value> {
    let i = table
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| Error::internal(format!("{} has no column {name}", table.name)))?;
    value(&table.rows[i], row, table.columns[i].type_oid, session)
}

fn oid(value: u32) -> Value {
    Value::Oid(value)
}

/// A `"char"` value from a code.
fn code(value: char) -> Value {
    Value::Char(u8::try_from(value).unwrap_or(b' '))
}

/// An `int2[]`, or null for no element.
fn int2_array(values: &[i16]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    Value::Array(Box::new(Array::one(values.iter().map(|&v| Some(Value::Int2(v))).collect())))
}

/// An `oid[]`, or null for no element.
fn oid_array(values: &[u32]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    Value::Array(Box::new(Array::one(values.iter().map(|&v| Some(Value::Oid(v))).collect())))
}

/// A `"char"[]`, or null for no element.
fn char_array(values: &[u8]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    Value::Array(Box::new(Array::one(values.iter().map(|&v| Some(Value::Char(v))).collect())))
}

/// A `text[]` or an `aclitem[]` of the texts.
fn text_array(values: &[String]) -> Value {
    Value::Array(Box::new(Array::one(
        values.iter().map(|v| Some(Value::text(v.clone()))).collect(),
    )))
}

/// An `int2vector`.
fn int2_vector(values: &[i16]) -> Value {
    Value::Array(Box::new(Array::vector(values.iter().map(|&v| Some(Value::Int2(v))).collect())))
}

/// An `oidvector`.
fn oid_vector(values: &[u32]) -> Value {
    Value::Array(Box::new(Array::vector(values.iter().map(|&v| Some(Value::Oid(v))).collect())))
}

/// A `pg_node_tree` value, or null.
fn tree(expr: Option<&String>) -> Value {
    expr.map_or(Value::Null, |e| Value::text(e.clone()))
}

/// The number of a column, from 1.
fn number(index: usize) -> i16 {
    i16::try_from(index + 1).unwrap_or(i16::MAX)
}

/// The rows of the user objects in a table of the catalog, with a value for each column.
///
/// # Errors
///
/// An internal error when a static row that a row copies is not there.
pub(crate) fn rows(
    table: &Table,
    catalog: &Catalog,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let rows = match table.name {
        "pg_namespace" => namespaces(table, catalog),
        "pg_class" => classes(table, catalog),
        "pg_type" => types(table, catalog, session)?,
        "pg_attribute" => attributes(table, catalog, session)?,
        "pg_attrdef" => defaults(table, catalog),
        "pg_constraint" => constraints(table, catalog),
        "pg_trigger" => triggers(table, catalog),
        "pg_index" => indexes(table, catalog),
        "pg_sequence" => sequences(table, catalog),
        "pg_proc" => functions(table, catalog),
        "pg_language" => languages(table, catalog),
        "pg_extension" => extensions(table, catalog),
        "pg_depend" => depends(table, catalog),
        "pg_description" => descriptions(table, catalog),
        "pg_rewrite" => rewrites(table, catalog),
        "pg_init_privs" => init_privs(table, catalog, session)?,
        "pg_database" => databases(table, session),
        "pg_ts_template" => ts_templates(table, catalog),
        "pg_ts_parser" => ts_parsers(table, catalog),
        "pg_ts_dict" => ts_dictionaries(table, catalog),
        "pg_ts_config" => ts_configs(table, catalog),
        "pg_ts_config_map" => ts_mappings(table, catalog),
        "pg_collation" => collations(table, catalog),
        "pg_enum" => enum_labels(table, catalog),
        "pg_range" => ranges(table, catalog),
        "pg_cast" => casts(table, catalog),
        _ => Vec::new(),
    };
    Ok(rows.into_iter().map(|row| row.values).collect())
}

/// The rows of `pg_init_privs`, as `setup_privileges` of initdb makes them: the ACL of each table, view, materialized view and sequence, of each column of these relations, and of each function, type, language and schema, when the ACL is not null. initdb runs `setup_privileges` after `system_views.sql` and before `information_schema.sql`, so the views of the system are in the table and the objects of `information_schema` are not. The objects of a cluster get their OIDs in that order, so an object is in the table when its OID is lower than the OID of the schema `information_schema`. Each row has the type `i`.
fn init_privs<'a>(
    table: &'a Table,
    catalog: &Catalog,
    session: &dyn Session,
) -> Result<Vec<Row<'a>>> {
    let limit = catalog
        .schema_by_name("information_schema")
        .map_or(rupg_catalog::FIRST_NORMAL_OID, |schema| schema.oid);
    let mut rows = Vec::new();
    let mut add = |object: u32, class: u32, sub: i32, acl: Value| {
        let mut row = Row::new(table);
        row.set("objoid", oid(object))
            .set("classoid", oid(class))
            .set("objsubid", Value::Int4(sub))
            .set("privtype", code('i'))
            .set("initprivs", acl);
        rows.push(row);
    };
    let relation = |kind: &Value| matches!(kind, Value::Char(b'r' | b'v' | b'm' | b'S'));
    let mut relations = Vec::new();
    if let Some(pg_class) = rupg_pgcatalog::catalog("pg_class") {
        for row in 0..pg_class.len {
            let object = cell(pg_class, row, "oid", session)?;
            if relation(&cell(pg_class, row, "relkind", session)?) {
                relations.push(object.clone());
                let acl = cell(pg_class, row, "relacl", session)?;
                if let (Value::Oid(object), false) = (object, acl.is_null()) {
                    add(object, pg_class.oid, 0, acl);
                }
            }
        }
        for rel in catalog.relations() {
            let kind = matches!(rel.kind, RelKind::Table | RelKind::View | RelKind::Sequence);
            if let (true, true, Some(acl)) = (rel.oid < limit, kind, &rel.acl) {
                add(rel.oid, pg_class.oid, 0, text_array(acl));
            }
        }
        if let Some(pg_attribute) = rupg_pgcatalog::catalog("pg_attribute") {
            for row in 0..pg_attribute.len {
                let acl = cell(pg_attribute, row, "attacl", session)?;
                let object = cell(pg_attribute, row, "attrelid", session)?;
                if acl.is_null() || !relations.contains(&object) {
                    continue;
                }
                let sub = cell(pg_attribute, row, "attnum", session)?.as_i64().unwrap_or(0);
                if let Value::Oid(object) = object {
                    add(object, pg_class.oid, i32::try_from(sub).unwrap_or(0), acl);
                }
            }
        }
    }
    for (name, column) in [
        ("pg_proc", "proacl"),
        ("pg_type", "typacl"),
        ("pg_language", "lanacl"),
        ("pg_namespace", "nspacl"),
    ] {
        let Some(source) = rupg_pgcatalog::catalog(name) else { continue };
        for row in 0..source.len {
            let acl = cell(source, row, column, session)?;
            if let (Value::Oid(object), false) = (cell(source, row, "oid", session)?, acl.is_null())
            {
                add(object, source.oid, 0, acl);
            }
        }
    }
    for schema in catalog.schemas() {
        if let (true, Some(acl)) = (schema.oid < limit, &schema.acl) {
            add(
                schema.oid,
                rupg_pgcatalog::catalog("pg_namespace").map_or(0, |c| c.oid),
                0,
                text_array(acl),
            );
        }
    }
    Ok(rows)
}

/// The rows of `pg_database` of the databases that `CREATE DATABASE` made.
fn databases<'a>(table: &'a Table, session: &dyn Session) -> Vec<Row<'a>> {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::text);
    session
        .databases()
        .iter()
        .map(|database| {
            let mut row = Row::new(table);
            row.set("oid", oid(database.oid))
                .set("datname", Value::text(database.name.clone()))
                .set("datdba", oid(database.owner))
                .set("encoding", Value::Int4(database.encoding))
                .set("datlocprovider", Value::Char(database.locale_provider))
                .set("datistemplate", Value::Bool(database.is_template))
                .set("datallowconn", Value::Bool(database.allow_connections))
                .set("dathasloginevt", Value::Bool(false))
                .set("datconnlimit", Value::Int4(database.connection_limit))
                .set("datfrozenxid", oid(database.frozen_xid))
                .set("datminmxid", oid(database.min_mxid))
                .set("dattablespace", oid(database.tablespace))
                .set("datcollate", Value::text(database.collate.clone()))
                .set("datctype", Value::text(database.ctype.clone()))
                .set("datlocale", text(&database.locale))
                .set("daticurules", text(&database.icu_rules))
                .set("datcollversion", text(&database.collation_version))
                .set("datacl", database.acl.as_deref().map_or(Value::Null, text_array));
            row
        })
        .collect()
}

fn namespaces<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .schemas()
        .map(|schema| {
            let mut row = Row::new(table);
            row.set("oid", oid(schema.oid))
                .set("nspname", Value::text(schema.name.clone()))
                .set("nspowner", oid(schema.owner))
                .set("nspacl", schema.acl.as_deref().map_or(Value::Null, text_array));
            row
        })
        .collect()
}

fn classes<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .map(|rel| {
            let heap = matches!(rel.kind, RelKind::Table | RelKind::Toast);
            let (am, pages, tuples) = match rel.kind {
                RelKind::Table | RelKind::Toast => (HEAP, 0, -1.0),
                RelKind::Index => {
                    rel.index.as_ref().map_or((0, 1, 0.0), |i| (i.method, i.pages, 0.0))
                }
                RelKind::Sequence => (0, 1, 1.0),
                RelKind::View | RelKind::Composite => (0, 0, -1.0),
            };
            let checks = catalog.constraints_of(rel.oid).filter(|c| c.kind == ConKind::Check);
            let mut row = Row::new(table);
            row.set("oid", oid(rel.oid))
                .set("relname", Value::text(rel.name.clone()))
                .set("relnamespace", oid(rel.namespace))
                .set("reltype", oid(rel.row_type))
                .set("reloftype", oid(0))
                .set("relowner", oid(rel.owner))
                .set("relam", oid(am))
                .set(
                    "relfilenode",
                    oid(match (&rel.sequence, rel.kind) {
                        (Some(info), _) => info.file,
                        (None, RelKind::View | RelKind::Composite) => 0,
                        (None, _) => rel.oid,
                    }),
                )
                .set("reltablespace", oid(0))
                .set("relpages", Value::Int4(pages))
                .set("reltuples", Value::Float4(tuples))
                .set("relallvisible", Value::Int4(0))
                .set("relallfrozen", Value::Int4(0))
                .set("reltoastrelid", oid(rel.toast))
                .set("relhasindex", Value::Bool(rel.has_index))
                .set("relisshared", Value::Bool(false))
                .set("relpersistence", code('p'))
                .set("relkind", code(rel.kind.code()))
                .set("relnatts", Value::Int2(number(rel.columns.len()) - 1))
                .set("relchecks", Value::Int2(i16::try_from(checks.count()).unwrap_or(i16::MAX)))
                .set(
                    "relhasrules",
                    Value::Bool(
                        rel.view.is_some() || catalog.rules().any(|r| r.relation == rel.oid),
                    ),
                )
                .set("relhastriggers", Value::Bool(rel.has_triggers))
                .set("relhassubclass", Value::Bool(false))
                .set("relrowsecurity", Value::Bool(false))
                .set("relforcerowsecurity", Value::Bool(false))
                .set("relispopulated", Value::Bool(true))
                .set("relreplident", code(if rel.kind == RelKind::Table { 'd' } else { 'n' }))
                .set("relispartition", Value::Bool(false))
                .set("relrewrite", oid(0))
                .set("relfrozenxid", oid(if heap { FIRST_NORMAL_XID } else { 0 }))
                .set("relminmxid", oid(u32::from(heap)))
                .set("relacl", rel.acl.as_deref().map_or(Value::Null, text_array))
                .set(
                    "reloptions",
                    if rel.options.is_empty() { Value::Null } else { text_array(&rel.options) },
                );
            row
        })
        .collect()
}

/// The OID of `_int4`, the template of the array type of a domain over a type with no array type.
const INT4_ARRAY: u32 = 1007;

/// The OID of `oid`, which an enum type copies, as `DefineEnum` gives it the length, the alignment and the storage of `oid`.
const OID_TYPE: u32 = 26;

/// The OID of `_oid`, which the array type of an enum type copies.
const OID_ARRAY: u32 = 1028;

/// The OIDs of `int4range`, `_int4range`, `int4multirange` and `_int4multirange`, which the types of a range type copy, as `DefineRange` gives each range type the same functions.
const INT4_RANGE_TYPES: [u32; 4] = [3904, 3905, 4451, 6150];

/// The template of the row of a type of a range type in [`INT4_RANGE_TYPES`] and the alignment of the type, or `None` for a type of another kind.
fn range_template(catalog: &Catalog, ty: &Type) -> Option<(u32, u8)> {
    let [range, range_array, multirange, multirange_array] = INT4_RANGE_TYPES;
    let info = |oid: u32| match catalog.type_by_oid(oid).and_then(|t| t.range.as_ref()) {
        Some(RangePart::Range(info)) => Some(info),
        _ => None,
    };
    let part = |ty: &Type| match &ty.range {
        Some(RangePart::Range(info)) => Some((false, info.align)),
        Some(RangePart::Multirange(of)) => Some((true, info(*of)?.align)),
        None => None,
    };
    if let Some((multi, align)) = part(ty) {
        return Some((if multi { multirange } else { range }, align));
    }
    let (multi, align) = part(catalog.type_by_oid(ty.element)?)?;
    Some((if multi { multirange_array } else { range_array }, align))
}

/// The OID of a function of an enum type, such as `enum_in`.
fn enum_function(name: &str) -> Value {
    oid(builtin::procs_named(name).next().map_or(0, |p| p.oid))
}

/// The static row of `pg_type` with the OID.
fn type_row(table: &Table, type_oid: u32, session: &dyn Session) -> Result<usize> {
    for row in 0..table.len {
        if cell(table, row, "oid", session)? == Value::Oid(type_oid) {
            return Ok(row);
        }
    }
    Err(Error::internal(format!("cache lookup failed for type {type_oid}")))
}

/// The row types, the domains and their array types. The row of a row type copies the row of the row type of `pg_class`, which has the same form. The row of a domain copies the row of its base type, as `DefineDomain` takes the storage of the base type. An enum type copies the row of `oid`, and its array type copies the row of `_oid`. Another array type copies the array type of the row type of `pg_class` or of the base type of the domain.
fn types<'a>(table: &'a Table, catalog: &Catalog, session: &dyn Session) -> Result<Vec<Row<'a>>> {
    let row_type = rupg_pgcatalog::catalog("pg_class").map_or(0, |c| c.rowtype_oid);
    let array_type = builtin::type_by_oid(row_type).map_or(0, |t| t.array);
    let (row_template, array_template) =
        (type_row(table, row_type, session)?, type_row(table, array_type, session)?);
    let mut rows: Vec<Row<'a>> = Vec::new();
    // A base type of the catalog comes before its domain, so the row of the domain can copy its row.
    let copy =
        |rows: &[Row<'a>], type_oid: u32| match catalog.types().position(|t| t.oid == type_oid) {
            Some(i) if i < rows.len() => Ok(Row { table, values: rows[i].values.clone() }),
            _ => Row::copy(table, type_row(table, type_oid, session)?, session),
        };
    for ty in catalog.types() {
        let element = catalog.type_by_oid(ty.element);
        let element_domain = element.and_then(|t| t.domain.as_ref());
        let mut row = if let Some(domain) = &ty.domain {
            copy(&rows, domain.base)?
        } else if let Some((template, align)) = range_template(catalog, ty) {
            let mut row = Row::copy(table, type_row(table, template, session)?, session)?;
            row.set("typalign", Value::Char(align));
            row
        } else if ty.labels.is_some() {
            let mut row = Row::copy(table, type_row(table, OID_TYPE, session)?, session)?;
            row.set("typtype", code('e'))
                .set("typcategory", code('E'))
                .set("typispreferred", Value::Bool(false))
                .set("typinput", enum_function("enum_in"))
                .set("typoutput", enum_function("enum_out"))
                .set("typreceive", enum_function("enum_recv"))
                .set("typsend", enum_function("enum_send"));
            row
        } else if element.is_some_and(|e| e.labels.is_some()) {
            Row::copy(table, type_row(table, OID_ARRAY, session)?, session)?
        } else if let Some(domain) = element_domain {
            let base_array = match catalog.type_by_oid(domain.base) {
                Some(base) => base.array,
                None => builtin::type_by_oid(domain.base).map_or(0, |t| t.array),
            };
            if base_array == 0 {
                // A base type with no array type, such as an array type, gives the array type of the domain the alignment of `DefineDomain`.
                let double = builtin::type_by_oid(domain.base).is_some_and(|t| t.align == b'd');
                let mut row = copy(&rows, INT4_ARRAY)?;
                row.set("typalign", code(if double { 'd' } else { 'i' }));
                row
            } else {
                copy(&rows, base_array)?
            }
        } else if ty.relation == 0 {
            Row::copy(table, array_template, session)?
        } else {
            Row::copy(table, row_template, session)?
        };
        if let Some(domain) = &ty.domain {
            let default = match &domain.default {
                Some(text) => Value::text(rupg_func::deparse_expression(text, session)?),
                None => Value::Null,
            };
            row.set("typtype", code('d'))
                .set("typispreferred", Value::Bool(false))
                .set("typsubscript", oid(0))
                .set("typinput", oid(builtin::DOMAIN_IN))
                .set("typreceive", oid(builtin::DOMAIN_RECV))
                .set("typmodin", oid(0))
                .set("typmodout", oid(0))
                .set("typnotnull", Value::Bool(domain.not_null))
                .set("typbasetype", oid(domain.base))
                .set("typtypmod", Value::Int4(domain.typmod))
                .set("typndims", Value::Int4(0))
                .set("typcollation", oid(domain.collation))
                .set("typdefaultbin", tree(domain.default.as_ref()))
                .set("typdefault", default)
                .set("typacl", Value::Null);
        }
        if let Some(domain) = element_domain {
            row.set("typcollation", oid(domain.collation))
                .set("typmodin", oid(0))
                .set("typmodout", oid(0));
        }
        row.set("oid", oid(ty.oid))
            .set("typname", Value::text(ty.name.clone()))
            .set("typnamespace", oid(ty.namespace))
            .set("typowner", oid(ty.owner))
            .set("typrelid", oid(ty.relation))
            .set("typelem", oid(ty.element))
            .set("typarray", oid(ty.array));
        rows.push(row);
    }
    Ok(rows)
}

/// The system columns of each table, sequence and TOAST table, which copy those of `pg_class`, and the columns of each relation.
fn attributes<'a>(
    table: &'a Table,
    catalog: &Catalog,
    session: &dyn Session,
) -> Result<Vec<Row<'a>>> {
    let pg_class = rupg_pgcatalog::catalog("pg_class").map_or(0, |c| c.oid);
    let mut system = Vec::new();
    for row in 0..table.len {
        if cell(table, row, "attrelid", session)? == Value::Oid(pg_class)
            && cell(table, row, "attnum", session)?.as_i64().is_some_and(|n| n < 0)
        {
            system.push(row);
        }
    }
    let mut rows = Vec::new();
    for rel in catalog.relations() {
        if !matches!(rel.kind, RelKind::Index | RelKind::View | RelKind::Composite) {
            for &template in &system {
                let mut row = Row::copy(table, template, session)?;
                row.set("attrelid", oid(rel.oid));
                rows.push(row);
            }
        }
        for (i, column) in rel.columns.iter().enumerate() {
            rows.push(attribute(table, rel, i, column));
        }
    }
    Ok(rows)
}

/// The `pg_attribute` row of a column.
fn attribute<'a>(
    table: &'a Table,
    rel: &Relation,
    index: usize,
    column: &rupg_catalog::Column,
) -> Row<'a> {
    let (len, by_val, align, mut storage) = match builtin::type_by_oid(column.ty) {
        Some(t) => (t.len, t.by_val, t.align, t.storage),
        None => (-1, false, b'd', b'x'),
    };
    if rel.kind == RelKind::Toast {
        storage = b'p';
    }
    let mut row = Row::new(table);
    row.set("attrelid", oid(rel.oid))
        .set("attname", Value::text(column.name.clone()))
        .set("atttypid", oid(column.ty))
        .set("attlen", Value::Int2(len))
        .set("attnum", Value::Int2(number(index)))
        .set("atttypmod", Value::Int4(column.typmod))
        .set("attndims", Value::Int2(column.ndims))
        .set("attbyval", Value::Bool(by_val))
        .set("attalign", Value::Char(align))
        .set("attstorage", Value::Char(storage))
        .set("attcompression", Value::Char(0))
        .set("attnotnull", Value::Bool(column.not_null))
        .set("atthasdef", Value::Bool(column.has_default))
        .set("atthasmissing", Value::Bool(false))
        .set("attidentity", Value::Char(column.identity))
        .set("attgenerated", Value::Char(0))
        .set("attisdropped", Value::Bool(false))
        .set("attislocal", Value::Bool(true))
        .set("attinhcount", Value::Int2(0))
        .set("attcollation", oid(column.collation));
    row
}

fn defaults<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .defaults()
        .map(|def| {
            let mut row = Row::new(table);
            row.set("oid", oid(def.oid))
                .set("adrelid", oid(def.relation))
                .set("adnum", Value::Int2(def.column))
                .set("adbin", Value::text(def.expr.clone()));
            row
        })
        .collect()
}

fn constraints<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog.constraints().map(|con| constraint(table, con)).collect()
}

/// The `pg_constraint` row of a constraint.
fn constraint<'a>(table: &'a Table, con: &Constraint) -> Row<'a> {
    let foreign = con.foreign.as_ref();
    let mut row = Row::new(table);
    row.set("oid", oid(con.oid))
        .set("conname", Value::text(con.name.clone()))
        .set("connamespace", oid(con.namespace))
        .set("contype", code(con.kind.code()))
        .set("condeferrable", Value::Bool(con.deferrable))
        .set("condeferred", Value::Bool(con.deferred))
        .set("conenforced", Value::Bool(true))
        .set("convalidated", Value::Bool(con.validated))
        .set("conrelid", oid(con.relation))
        .set("contypid", oid(con.domain))
        .set("conindid", oid(con.index))
        .set("conparentid", oid(0))
        .set("confrelid", oid(foreign.map_or(0, |f| f.table)))
        .set("confupdtype", code(foreign.map_or(' ', |f| f.update)))
        .set("confdeltype", code(foreign.map_or(' ', |f| f.delete)))
        .set("confmatchtype", code(foreign.map_or(' ', |f| f.match_type)))
        .set("conislocal", Value::Bool(true))
        .set("coninhcount", Value::Int2(0))
        .set("connoinherit", Value::Bool(con.no_inherit))
        .set("conperiod", Value::Bool(false))
        .set("conkey", int2_array(&con.keys))
        .set("conbin", tree(con.expr.as_ref()));
    if let Some(f) = foreign {
        row.set("confkey", int2_array(&f.keys))
            .set("conpfeqop", oid_array(&f.pf_eq))
            .set("conppeqop", oid_array(&f.pp_eq))
            .set("conffeqop", oid_array(&f.ff_eq));
    }
    row
}

/// The internal triggers of the foreign keys. They fire after each row and have no arguments and no columns.
fn triggers<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .triggers()
        .map(|trigger| {
            let mut row = Row::new(table);
            row.set("oid", oid(trigger.oid))
                .set("tgrelid", oid(trigger.relation))
                .set("tgparentid", oid(0))
                .set("tgname", Value::text(trigger.name.clone()))
                .set("tgfoid", oid(trigger.function))
                .set("tgtype", Value::Int2(trigger.kind))
                .set("tgenabled", code('O'))
                .set("tgisinternal", Value::Bool(true))
                .set("tgconstrrelid", oid(trigger.constraint_relation))
                .set("tgconstrindid", oid(trigger.constraint_index))
                .set("tgconstraint", oid(trigger.constraint))
                .set("tgdeferrable", Value::Bool(trigger.deferrable))
                .set("tginitdeferred", Value::Bool(trigger.deferred))
                .set("tgnargs", Value::Int2(0))
                .set("tgattr", int2_vector(&[]))
                .set("tgargs", Value::Bytea(Vec::new()));
            row
        })
        .collect()
}

fn indexes<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .filter_map(|rel| {
            let info = rel.index.as_ref()?;
            let mut row = Row::new(table);
            row.set("indexrelid", oid(rel.oid))
                .set("indrelid", oid(info.table))
                .set("indnatts", Value::Int2(number(info.keys.len()) - 1))
                .set("indnkeyatts", Value::Int2(info.key_count))
                .set("indisunique", Value::Bool(info.unique))
                .set("indnullsnotdistinct", Value::Bool(info.nulls_not_distinct))
                .set("indisprimary", Value::Bool(info.primary))
                .set("indisexclusion", Value::Bool(false))
                .set("indimmediate", Value::Bool(info.immediate))
                .set("indisclustered", Value::Bool(false))
                .set("indisvalid", Value::Bool(true))
                .set("indcheckxmin", Value::Bool(false))
                .set("indisready", Value::Bool(true))
                .set("indislive", Value::Bool(true))
                .set("indisreplident", Value::Bool(false))
                .set("indkey", int2_vector(&info.keys))
                .set("indcollation", oid_vector(&info.collations))
                .set("indclass", oid_vector(&info.classes))
                .set("indoption", int2_vector(&info.options))
                .set("indexprs", tree(info.exprs.as_ref()))
                .set("indpred", tree(info.predicate.as_ref()));
            Some(row)
        })
        .collect()
}

/// The `_RETURN` rule of each view. `ev_action` holds the statement that made the view.
/// The `_RETURN` rules of the views and the rules of `CREATE RULE`, in the order of their OIDs. `ev_qual` and `ev_action` have the text of the statement in place of the node trees, and `ev_qual` is `<>` for a rule with no condition.
fn rewrites<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    let views = catalog.relations().filter_map(|rel| {
        let view = rel.view.as_ref()?;
        Some((view.rule, "_RETURN", rel.oid, b'1', b'O', true, false, &view.text))
    });
    let rules = catalog.rules().map(|r| {
        (r.oid, r.name.as_str(), r.relation, r.event, r.enabled, r.instead, r.qualified, &r.text)
    });
    let mut all: Vec<_> = views.chain(rules).collect();
    all.sort_by_key(|r| r.0);
    all.into_iter()
        .map(|(rule, name, class, event, enabled, instead, qualified, text)| {
            let mut row = Row::new(table);
            row.set("oid", oid(rule))
                .set("rulename", Value::text(name))
                .set("ev_class", oid(class))
                .set("ev_type", code(char::from(event)))
                .set("ev_enabled", code(char::from(enabled)))
                .set("is_instead", Value::Bool(instead))
                .set("ev_qual", Value::text(if qualified { text.as_str() } else { "<>" }))
                .set("ev_action", Value::text(text.clone()));
            row
        })
        .collect()
}

fn sequences<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .filter_map(|rel| {
            let seq = rel.sequence.as_ref()?;
            let mut row = Row::new(table);
            row.set("seqrelid", oid(rel.oid))
                .set("seqtypid", oid(seq.ty))
                .set("seqstart", Value::Int8(seq.start))
                .set("seqincrement", Value::Int8(seq.increment))
                .set("seqmax", Value::Int8(seq.max))
                .set("seqmin", Value::Int8(seq.min))
                .set("seqcache", Value::Int8(seq.cache))
                .set("seqcycle", Value::Bool(seq.cycle));
            Some(row)
        })
        .collect()
}

/// The functions in SQL. A body in a string is in `prosrc`. For a body in SQL, `prosqlbody` has the text of the statement that made the function, and `prosrc` is empty.
/// `provariadic`: the element type of the `VARIADIC` argument, or 0.
fn variadic(catalog: &Catalog, f: &Function) -> u32 {
    let Some(i) = f.argmodes.iter().position(|&mode| mode == b'v') else { return 0 };
    let array = f.allargtypes.get(i).copied().unwrap_or(0);
    catalog
        .type_by_oid(array)
        .map(|t| t.element)
        .or_else(|| builtin::type_by_oid(array).map(|t| t.elem))
        .unwrap_or(0)
}

fn functions<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .functions()
        .map(|f| {
            let mut row = Row::new(table);
            #[allow(clippy::cast_precision_loss)]
            let (cost, rows) = (f.cost as f32, f.rows as f32);
            let names = if f.argnames.is_empty() { Value::Null } else { text_array(&f.argnames) };
            row.set("oid", oid(f.oid))
                .set("proname", Value::text(f.name.clone()))
                .set("pronamespace", oid(f.namespace))
                .set("proowner", oid(f.owner))
                .set("prolang", oid(f.lang))
                .set("procost", Value::Float4(cost))
                .set("prorows", Value::Float4(rows))
                .set("provariadic", oid(variadic(catalog, f)))
                .set("prosupport", oid(f.support))
                .set("prokind", code('f'))
                .set("prosecdef", Value::Bool(false))
                .set("proleakproof", Value::Bool(false))
                .set("proisstrict", Value::Bool(f.strict))
                .set("proretset", Value::Bool(f.retset))
                .set("provolatile", Value::Char(f.volatile))
                .set("proparallel", Value::Char(f.parallel))
                .set("pronargs", Value::Int2(i16::try_from(f.argtypes.len()).unwrap_or(i16::MAX)))
                .set("pronargdefaults", Value::Int2(0))
                .set("prorettype", oid(f.rettype))
                .set("proargtypes", oid_vector(&f.argtypes))
                .set("proallargtypes", oid_array(&f.allargtypes))
                .set("proargmodes", char_array(&f.argmodes))
                .set("proargnames", names)
                .set("prosrc", Value::text(f.src.clone()))
                .set(
                    "probin",
                    if f.bin.is_empty() { Value::Null } else { Value::text(f.bin.clone()) },
                )
                .set(
                    "prosqlbody",
                    tree(f.body.as_ref().filter(|_| f.src.is_empty()).map(|b| &b.text)),
                );
            row
        })
        .collect()
}

fn languages<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .languages()
        .map(|l| {
            let mut row = Row::new(table);
            row.set("oid", oid(l.oid))
                .set("lanname", Value::text(l.name.clone()))
                .set("lanowner", oid(l.owner))
                .set("lanispl", Value::Bool(true))
                .set("lanpltrusted", Value::Bool(l.trusted))
                .set("lanplcallfoid", oid(l.handler))
                .set("laninline", oid(l.inline))
                .set("lanvalidator", oid(l.validator));
            row
        })
        .collect()
}

fn extensions<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .extensions()
        .map(|e| {
            let mut row = Row::new(table);
            row.set("oid", oid(e.oid))
                .set("extname", Value::text(e.name.clone()))
                .set("extowner", oid(e.owner))
                .set("extnamespace", oid(e.namespace))
                .set("extrelocatable", Value::Bool(e.relocatable))
                .set("extversion", Value::text(e.version.clone()));
            row
        })
        .collect()
}

fn ts_templates<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .ts_templates()
        .map(|t| {
            let mut row = Row::new(table);
            row.set("oid", oid(t.oid))
                .set("tmplname", Value::text(t.name.clone()))
                .set("tmplnamespace", oid(t.namespace))
                .set("tmplinit", oid(t.init))
                .set("tmpllexize", oid(t.lexize));
            row
        })
        .collect()
}

fn ts_parsers<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .ts_parsers()
        .map(|p| {
            let mut row = Row::new(table);
            row.set("oid", oid(p.oid))
                .set("prsname", Value::text(p.name.clone()))
                .set("prsnamespace", oid(p.namespace))
                .set("prsstart", oid(p.start))
                .set("prstoken", oid(p.token))
                .set("prsend", oid(p.end))
                .set("prsheadline", oid(p.headline))
                .set("prslextype", oid(p.lextype));
            row
        })
        .collect()
}

fn ts_dictionaries<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .ts_dictionaries()
        .map(|d| {
            let mut row = Row::new(table);
            row.set("oid", oid(d.oid))
                .set("dictname", Value::text(d.name.clone()))
                .set("dictnamespace", oid(d.namespace))
                .set("dictowner", oid(d.owner))
                .set("dicttemplate", oid(d.template))
                .set("dictinitoption", d.options.clone().map_or(Value::Null, Value::text));
            row
        })
        .collect()
}

fn ts_configs<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .ts_configs()
        .map(|c| {
            let mut row = Row::new(table);
            row.set("oid", oid(c.oid))
                .set("cfgname", Value::text(c.name.clone()))
                .set("cfgnamespace", oid(c.namespace))
                .set("cfgowner", oid(c.owner))
                .set("cfgparser", oid(c.parser));
            row
        })
        .collect()
}

/// The rows of `pg_collation` that `pg_import_system_collations` adds at the setup of a cluster.
fn collations<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .collations()
        .map(|c| {
            let mut row = Row::new(table);
            let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::text);
            row.set("oid", oid(c.oid))
                .set("collname", Value::text(c.name.clone()))
                .set("collnamespace", oid(c.namespace))
                .set("collowner", oid(c.owner))
                .set("collprovider", Value::Char(c.provider))
                .set("collisdeterministic", Value::Bool(true))
                .set("collencoding", Value::Int4(c.encoding))
                .set("collcollate", text(&c.libc_locale))
                .set("collctype", text(&c.libc_locale))
                .set("colllocale", text(&c.locale))
                .set("collversion", text(&c.version));
            row
        })
        .collect()
}

fn enum_labels<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for ty in catalog.types() {
        for label in ty.labels.iter().flatten() {
            let mut row = Row::new(table);
            row.set("oid", oid(label.oid))
                .set("enumtypid", oid(ty.oid))
                .set("enumsortorder", Value::Float4(label.sort_order))
                .set("enumlabel", Value::text(label.label.clone()));
            rows.push(row);
        }
    }
    rows
}

fn ranges<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for ty in catalog.types() {
        let Some(RangePart::Range(info)) = &ty.range else { continue };
        let mut row = Row::new(table);
        row.set("rngtypid", oid(ty.oid))
            .set("rngsubtype", oid(info.subtype))
            .set("rngmultitypid", oid(info.multirange))
            .set("rngcollation", oid(info.collation))
            .set("rngsubopc", oid(info.subopc))
            .set("rngcanonical", oid(info.canonical))
            .set("rngsubdiff", oid(info.subdiff));
        rows.push(row);
    }
    rows
}

fn casts<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .casts()
        .map(|c| {
            let mut row = Row::new(table);
            row.set("oid", oid(c.oid))
                .set("castsource", oid(c.source))
                .set("casttarget", oid(c.target))
                .set("castfunc", oid(c.func))
                .set("castcontext", Value::Char(c.context))
                .set("castmethod", Value::Char(c.method));
            row
        })
        .collect()
}

fn ts_mappings<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .ts_mappings()
        .iter()
        .map(|m| {
            let mut row = Row::new(table);
            row.set("mapcfg", oid(m.config))
                .set("maptokentype", Value::Int4(m.token))
                .set("mapseqno", Value::Int4(m.seqno))
                .set("mapdict", oid(m.dict));
            row
        })
        .collect()
}

fn descriptions<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .descriptions()
        .map(|(object, text)| {
            let mut row = Row::new(table);
            row.set("objoid", oid(object.oid))
                .set("classoid", oid(object.class))
                .set("objsubid", Value::Int4(object.sub))
                .set("description", Value::text(text));
            row
        })
        .collect()
}

fn depends<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .depends()
        .iter()
        .map(|dep| {
            let mut row = Row::new(table);
            row.set("classid", oid(dep.object.class))
                .set("objid", oid(dep.object.oid))
                .set("objsubid", Value::Int4(dep.object.sub))
                .set("refclassid", oid(dep.referenced.class))
                .set("refobjid", oid(dep.referenced.oid))
                .set("refobjsubid", Value::Int4(dep.referenced.sub))
                .set("deptype", code(dep.kind.code()));
            row
        })
        .collect()
}

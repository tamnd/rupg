//! The rows of the catalog for the user objects: the schemas, the relations, their row types and columns, the column defaults, the constraints, the indexes, the sequences, the functions and the dependencies between them. A scan of a table of the catalog gives these rows after the static rows.
//!
//! The columns of `pg_node_tree` hold the expressions in the form of the analyzer, not in the form of `nodeToString`.

use rupg_catalog::{Catalog, ConKind, Constraint, RelKind, Relation};
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
        "pg_index" => indexes(table, catalog),
        "pg_sequence" => sequences(table, catalog),
        "pg_proc" => functions(table, catalog),
        "pg_depend" => depends(table, catalog),
        "pg_rewrite" => rewrites(table, catalog),
        "pg_init_privs" => init_privs(table, catalog, session)?,
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
                RelKind::Index => (rel.index.as_ref().map_or(0, |i| i.method), 1, 0.0),
                RelKind::Sequence => (0, 1, 1.0),
                RelKind::View => (0, 0, -1.0),
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
                .set("relfilenode", oid(if rel.kind == RelKind::View { 0 } else { rel.oid }))
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
                .set("relhasrules", Value::Bool(rel.view.is_some()))
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

/// The static row of `pg_type` with the OID.
fn type_row(table: &Table, type_oid: u32, session: &dyn Session) -> Result<usize> {
    for row in 0..table.len {
        if cell(table, row, "oid", session)? == Value::Oid(type_oid) {
            return Ok(row);
        }
    }
    Err(Error::internal(format!("cache lookup failed for type {type_oid}")))
}

/// The row types, the domains and their array types. The row of a row type copies the row of the row type of `pg_class`, which has the same form. The row of a domain copies the row of its base type, as `DefineDomain` takes the storage of the base type. An array type copies the array type of the row type of `pg_class` or of the base type of the domain.
fn types<'a>(table: &'a Table, catalog: &Catalog, session: &dyn Session) -> Result<Vec<Row<'a>>> {
    let row_type = rupg_pgcatalog::catalog("pg_class").map_or(0, |c| c.rowtype_oid);
    let array_type = builtin::type_by_oid(row_type).map_or(0, |t| t.array);
    let (row_template, array_template) =
        (type_row(table, row_type, session)?, type_row(table, array_type, session)?);
    let mut rows = Vec::new();
    for ty in catalog.types() {
        let element_domain = catalog.type_by_oid(ty.element).and_then(|t| t.domain.as_ref());
        let template = if let Some(domain) = &ty.domain {
            type_row(table, domain.base, session)?
        } else if let Some(domain) = element_domain {
            let base_array = builtin::type_by_oid(domain.base).map_or(0, |t| t.array);
            type_row(table, base_array, session)?
        } else if ty.relation == 0 {
            array_template
        } else {
            row_template
        };
        let mut row = Row::copy(table, template, session)?;
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
                .set("typnotnull", Value::Bool(false))
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
        if !matches!(rel.kind, RelKind::Index | RelKind::View) {
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
        .set("attidentity", Value::Char(0))
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
fn rewrites<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .filter_map(|rel| {
            let view = rel.view.as_ref()?;
            let mut row = Row::new(table);
            row.set("oid", oid(view.rule))
                .set("rulename", Value::text("_RETURN"))
                .set("ev_class", oid(rel.oid))
                .set("ev_type", code('1'))
                .set("ev_enabled", code('O'))
                .set("is_instead", Value::Bool(true))
                .set("ev_qual", Value::text("<>"))
                .set("ev_action", Value::text(view.text.clone()));
            Some(row)
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
                .set("provariadic", oid(0))
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
                    "prosqlbody",
                    tree(f.body.as_ref().filter(|_| f.src.is_empty()).map(|b| &b.text)),
                );
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

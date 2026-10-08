//! The rows of the catalog for the user objects: the schemas, the relations, their row types and columns, the column defaults, the constraints, the indexes, the sequences and the dependencies between them. A scan of a table of the catalog gives these rows after the static rows.
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
        "pg_depend" => depends(table, catalog),
        _ => Vec::new(),
    };
    Ok(rows.into_iter().map(|row| row.values).collect())
}

fn namespaces<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .schemas()
        .map(|schema| {
            let mut row = Row::new(table);
            row.set("oid", oid(schema.oid))
                .set("nspname", Value::text(schema.name.clone()))
                .set("nspowner", oid(schema.owner));
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
                .set("relfilenode", oid(rel.oid))
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
                .set("relhasrules", Value::Bool(false))
                .set("relhastriggers", Value::Bool(rel.has_triggers))
                .set("relhassubclass", Value::Bool(false))
                .set("relrowsecurity", Value::Bool(false))
                .set("relforcerowsecurity", Value::Bool(false))
                .set("relispopulated", Value::Bool(true))
                .set("relreplident", code(if rel.kind == RelKind::Table { 'd' } else { 'n' }))
                .set("relispartition", Value::Bool(false))
                .set("relrewrite", oid(0))
                .set("relfrozenxid", oid(if heap { FIRST_NORMAL_XID } else { 0 }))
                .set("relminmxid", oid(u32::from(heap)));
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

/// The row types and their array types. Each row copies the row of the row type of `pg_class` or of its array type, which have the same form.
fn types<'a>(table: &'a Table, catalog: &Catalog, session: &dyn Session) -> Result<Vec<Row<'a>>> {
    let row_type = rupg_pgcatalog::catalog("pg_class").map_or(0, |c| c.rowtype_oid);
    let array_type = builtin::type_by_oid(row_type).map_or(0, |t| t.array);
    let (row_template, array_template) =
        (type_row(table, row_type, session)?, type_row(table, array_type, session)?);
    let mut rows = Vec::new();
    for ty in catalog.types() {
        let template = if ty.relation == 0 { array_template } else { row_template };
        let mut row = Row::copy(table, template, session)?;
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
        if rel.kind != RelKind::Index {
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
        .set("contypid", oid(0))
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

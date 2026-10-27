//! The rows of the catalog for the user objects: the schemas, the relations, their row types and columns, the column defaults, the constraints, the triggers of the foreign keys, the indexes, the sequences, the functions, the objects of text search, the dependencies between them and their comments. A scan of a table of the catalog gives these rows after the static rows.
//!
//! The columns of `pg_node_tree` hold the expressions in the form of the analyzer, not in the form of `nodeToString`.

use rupg_analyze::node;
use rupg_catalog::{
    BaseType, Catalog, ConKind, Constraint, Function, ObjRef, PG_CLASS, PG_FOREIGN_DATA_WRAPPER,
    PG_FOREIGN_SERVER, PG_LANGUAGE, PG_NAMESPACE, PG_PROC, PG_TYPE, PartitionBound, RangePart,
    RelKind, Relation, Type,
};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_pgcatalog::{Catalog as Table, builtin};
use rupg_types::oid::{ANY, ANYARRAY, ANYCOMPATIBLE, ANYCOMPATIBLEARRAY, ANYELEMENT};
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

/// The ACL that `GRANT` gave to an object, or null. The empty ACL of a column is null.
fn granted(catalog: &Catalog, object: ObjRef) -> Value {
    match catalog.object_acl(object) {
        Some(acl) if !acl.is_empty() || object.sub == 0 => text_array(acl),
        _ => Value::Null,
    }
}

/// Puts the ACLs that `GRANT` gave to the built-in objects into the static rows of `pg_namespace`, `pg_class`, `pg_attribute`, `pg_type`, `pg_proc` and `pg_language`.
///
/// # Errors
///
/// An internal error when a column of the table is not there.
pub(crate) fn grant_static(
    table: &Table,
    rows: &mut [Vec<Value>],
    catalog: &Catalog,
    session: &dyn Session,
) -> Result<()> {
    let (class, column) = match table.name {
        "pg_namespace" => (PG_NAMESPACE, "nspacl"),
        "pg_class" => (PG_CLASS, "relacl"),
        "pg_attribute" => (PG_CLASS, "attacl"),
        "pg_type" => (PG_TYPE, "typacl"),
        "pg_proc" => (PG_PROC, "proacl"),
        "pg_language" => (PG_LANGUAGE, "lanacl"),
        _ => return Ok(()),
    };
    let attribute = table.name == "pg_attribute";
    let changed: std::collections::BTreeMap<(u32, i32), &[String]> = catalog
        .object_acls()
        .filter(|(object, _)| object.class == class && (object.sub != 0) == attribute)
        .map(|(object, acl)| ((object.oid, object.sub), acl))
        .collect();
    let Some(at) = table.columns.iter().position(|c| c.name == column) else { return Ok(()) };
    if changed.is_empty() {
        return Ok(());
    }
    for (row, values) in rows.iter_mut().enumerate().take(table.len) {
        let (owner, sub) = if attribute {
            let sub = cell(table, row, "attnum", session)?.as_i64().unwrap_or_default();
            (cell(table, row, "attrelid", session)?, i32::try_from(sub).unwrap_or_default())
        } else {
            (cell(table, row, "oid", session)?, 0)
        };
        if let Value::Oid(owner) = owner
            && let Some(acl) = changed.get(&(owner, sub))
        {
            values[at] = if acl.is_empty() && attribute { Value::Null } else { text_array(acl) };
        }
    }
    Ok(())
}

/// Takes out the static rows of the objects of `initdb` that `DROP` removed: the row of `pg_namespace`, and the rows of `pg_description` and `pg_init_privs` for the object.
///
/// # Errors
///
/// An internal error when a column of the table is not there.
pub(crate) fn drop_static(
    table: &Table,
    rows: &mut Vec<Vec<Value>>,
    catalog: &Catalog,
    session: &dyn Session,
) -> Result<()> {
    let (oid, class) = match table.name {
        "pg_namespace" => ("oid", None),
        "pg_description" | "pg_init_privs" => ("objoid", Some("classoid")),
        _ => return Ok(()),
    };
    if !catalog.any_builtin_dropped() {
        return Ok(());
    }
    let mut dropped = Vec::new();
    for row in 0..table.len.min(rows.len()) {
        let Value::Oid(object) = cell(table, row, oid, session)? else { continue };
        let class = match class {
            Some(name) => match cell(table, row, name, session)? {
                Value::Oid(class) => class,
                _ => continue,
            },
            None => PG_NAMESPACE,
        };
        dropped.push(catalog.builtin_dropped(ObjRef::new(class, object)));
    }
    let mut at = 0;
    rows.retain(|_| {
        let keep = !dropped.get(at).copied().unwrap_or(false);
        at += 1;
        keep
    });
    Ok(())
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
        "pg_class" => classes(table, catalog, session),
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
        "pg_policy" => policies(table, catalog),
        "pg_init_privs" => init_privs(table, catalog, session)?,
        "pg_database" => databases(table, session),
        "pg_authid" => authids(table),
        "pg_auth_members" => auth_members(table),
        "pg_db_role_setting" => db_role_settings(table, session),
        "pg_tablespace" => tablespaces(table, session),
        "pg_ts_template" => ts_templates(table, catalog),
        "pg_ts_parser" => ts_parsers(table, catalog),
        "pg_ts_dict" => ts_dictionaries(table, catalog),
        "pg_ts_config" => ts_configs(table, catalog),
        "pg_ts_config_map" => ts_mappings(table, catalog),
        "pg_statistic_ext" => statistic_exts(table, catalog),
        "pg_statistic" => statistics(table, catalog, session),
        "pg_default_acl" => default_acls(table, catalog),
        "pg_collation" => collations(table, catalog),
        "pg_enum" => enum_labels(table, catalog),
        "pg_range" => ranges(table, catalog),
        "pg_cast" => casts(table, catalog),
        "pg_conversion" => conversions(table, catalog),
        "pg_foreign_data_wrapper" => fdws(table, catalog),
        "pg_foreign_server" => servers(table, catalog),
        "pg_user_mapping" => user_mappings(table, catalog),
        "pg_publication" => publications(table, catalog),
        "pg_publication_rel" => publication_rels(table, catalog),
        "pg_publication_namespace" => publication_namespaces(table, catalog),
        "pg_foreign_table" => foreign_tables(table, catalog),
        "pg_operator" => operators(table, catalog),
        "pg_aggregate" => aggregates(table, catalog),
        "pg_opfamily" => opfamilies(table, catalog),
        "pg_opclass" => opclasses(table, catalog),
        "pg_amop" => amops(table, catalog),
        "pg_amproc" => amprocs(table, catalog),
        "pg_partitioned_table" => partitioned_tables(table, catalog),
        "pg_inherits" => inherits(table, catalog),
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
        // `DROP` of a built-in object, for example `DROP SCHEMA public`, also removes its rows from `pg_init_privs`.
        if catalog.builtin_dropped(ObjRef::new(class, object)) {
            return;
        }
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
            let kind = matches!(
                rel.kind,
                RelKind::Table | RelKind::View | RelKind::Sequence | RelKind::MatView
            );
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

/// The rows of `pg_authid` of the roles that `CREATE ROLE` made.
fn authids(table: &Table) -> Vec<Row<'_>> {
    let roles = rupg_pgcatalog::roles::current();
    roles
        .roles
        .iter()
        .map(|role| {
            let a = role.attrs;
            let mut row = Row::new(table);
            row.set("oid", oid(role.oid))
                .set("rolname", Value::text(role.name.clone()))
                .set("rolsuper", Value::Bool(a.superuser))
                .set("rolinherit", Value::Bool(a.inherit))
                .set("rolcreaterole", Value::Bool(a.createrole))
                .set("rolcreatedb", Value::Bool(a.createdb))
                .set("rolcanlogin", Value::Bool(a.login))
                .set("rolreplication", Value::Bool(a.replication))
                .set("rolbypassrls", Value::Bool(a.bypassrls))
                .set("rolconnlimit", Value::Int4(role.connlimit))
                .set("rolpassword", role.password.clone().map_or(Value::Null, Value::text))
                .set("rolvaliduntil", role.valid_until.map_or(Value::Null, Value::TimestampTz));
            row
        })
        .collect()
}

/// The rows of `pg_auth_members` of the memberships that `GRANT` made.
fn auth_members(table: &Table) -> Vec<Row<'_>> {
    let roles = rupg_pgcatalog::roles::current();
    roles
        .members
        .iter()
        .map(|member| {
            let mut row = Row::new(table);
            row.set("oid", oid(member.oid))
                .set("roleid", oid(member.role))
                .set("member", oid(member.member))
                .set("grantor", oid(member.grantor))
                .set("admin_option", Value::Bool(member.admin))
                .set("inherit_option", Value::Bool(member.inherit))
                .set("set_option", Value::Bool(member.set));
            row
        })
        .collect()
}

/// The rows of `pg_tablespace` of the tablespaces that `CREATE TABLESPACE` made. A tablespace with no options has a null `spcoptions`.
fn tablespaces<'a>(table: &'a Table, session: &dyn Session) -> Vec<Row<'a>> {
    session
        .tablespaces()
        .iter()
        .map(|tablespace| {
            let mut row = Row::new(table);
            row.set("oid", oid(tablespace.oid))
                .set("spcname", Value::text(tablespace.name.clone()))
                .set("spcowner", oid(tablespace.owner))
                .set("spcacl", tablespace.acl.as_deref().map_or(Value::Null, text_array))
                .set(
                    "spcoptions",
                    if tablespace.options.is_empty() {
                        Value::Null
                    } else {
                        text_array(&tablespace.options)
                    },
                );
            row
        })
        .collect()
}

/// The rows of `pg_db_role_setting`: the settings of `ALTER DATABASE ... SET`.
fn db_role_settings<'a>(table: &'a Table, session: &dyn Session) -> Vec<Row<'a>> {
    session
        .db_role_settings()
        .into_iter()
        .map(|(database, role, settings)| {
            let mut row = Row::new(table);
            row.set("setdatabase", oid(database))
                .set("setrole", oid(role))
                .set("setconfig", text_array(&settings));
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

fn classes<'a>(table: &'a Table, catalog: &Catalog, session: &dyn Session) -> Vec<Row<'a>> {
    catalog
        .relations()
        .map(|rel| {
            let heap = matches!(rel.kind, RelKind::Table | RelKind::Toast | RelKind::MatView);
            let size = session.heap_size(rel.oid);
            let stats = size.map_or((0, -1.0), |s| (s.relpages, s.reltuples));
            let visible =
                size.filter(|_| heap).map_or((0, 0), |s| (s.relallvisible, s.relallfrozen));
            let (am, pages, tuples) = match rel.kind {
                RelKind::Table | RelKind::Toast | RelKind::MatView => (HEAP, stats.0, stats.1),
                RelKind::Partitioned => (0, stats.0, stats.1),
                // An index that `index_update_stats` saw has the stats of its build, and another one the pages of a new index.
                RelKind::Index => rel.index.as_ref().map_or((0, 1, 0.0), |i| match stats {
                    (pages, tuples) if tuples >= 0.0 => (i.method, pages, tuples),
                    _ => (i.method, i.pages, 0.0),
                }),
                RelKind::PartitionedIndex => {
                    rel.index.as_ref().map_or((0, 0, -1.0), |i| (i.method, 0, -1.0))
                }
                RelKind::Sequence => (0, 1, 1.0),
                RelKind::View | RelKind::Composite | RelKind::Foreign => (0, 0, -1.0),
            };
            let checks = catalog.constraints_of(rel.oid).filter(|c| c.kind == ConKind::Check);
            let mut row = Row::new(table);
            row.set("oid", oid(rel.oid))
                .set("relname", Value::text(rel.name.clone()))
                .set("relnamespace", oid(rel.namespace))
                .set("reltype", oid(rel.row_type))
                .set("reloftype", oid(rel.of_type))
                .set("relowner", oid(rel.owner))
                .set("relam", oid(am))
                .set(
                    "relfilenode",
                    oid(match (&rel.sequence, rel.kind) {
                        (Some(info), _) => info.file,
                        (
                            None,
                            RelKind::View
                            | RelKind::Composite
                            | RelKind::Partitioned
                            | RelKind::PartitionedIndex,
                        ) => 0,
                        (None, _) => rel.file,
                    }),
                )
                .set("reltablespace", oid(0))
                .set("relpages", Value::Int4(pages))
                .set("reltuples", Value::Float4(tuples))
                .set("relallvisible", Value::Int4(visible.0))
                .set("relallfrozen", Value::Int4(visible.1))
                .set("reltoastrelid", oid(rel.toast))
                .set("relhasindex", Value::Bool(rel.has_index))
                .set("relisshared", Value::Bool(false))
                .set(
                    "relpersistence",
                    code(if catalog.is_temp_schema(rel.namespace) {
                        't'
                    } else if rel.unlogged {
                        'u'
                    } else {
                        'p'
                    }),
                )
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
                .set(
                    "relhassubclass",
                    Value::Bool(match rel.kind {
                        RelKind::Table | RelKind::Partitioned => has_children(catalog, rel.oid),
                        RelKind::PartitionedIndex => has_index_partitions(catalog, rel.oid),
                        _ => false,
                    }),
                )
                .set("relrowsecurity", Value::Bool(rel.row_security))
                .set("relforcerowsecurity", Value::Bool(rel.force_row_security))
                .set("relispopulated", Value::Bool(rel.view.as_ref().is_none_or(|v| v.populated)))
                .set(
                    "relreplident",
                    code(match rel.replica_identity {
                        0 if matches!(
                            rel.kind,
                            RelKind::Table | RelKind::Partitioned | RelKind::MatView
                        ) =>
                        {
                            'd'
                        }
                        0 => 'n',
                        c => char::from(c),
                    }),
                )
                .set(
                    "relispartition",
                    Value::Bool(
                        rel.partition_of.is_some()
                            || rel.index.as_ref().is_some_and(|i| i.parent != 0),
                    ),
                )
                .set(
                    "relpartbound",
                    rel.partition_of
                        .as_ref()
                        .map_or(Value::Null, |p| Value::text(node::write_bound(&p.bound))),
                )
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

/// The row of a base type that `CREATE TYPE` made or of a shell type, as `TypeCreate` and `TypeShellMake` fill it. A shell type has the `typtype` `p`.
fn base_row<'a>(table: &'a Table, base: &BaseType, session: &dyn Session) -> Result<Row<'a>> {
    let mut row = Row::copy(table, type_row(table, OID_TYPE, session)?, session)?;
    row.set("typlen", Value::Int2(base.len))
        .set("typbyval", Value::Bool(base.by_val))
        .set("typtype", code(if base.defined { 'b' } else { 'p' }))
        .set("typcategory", Value::Char(base.category))
        .set("typispreferred", Value::Bool(base.preferred))
        .set("typisdefined", Value::Bool(base.defined))
        .set("typdelim", Value::Char(base.delim))
        .set("typsubscript", oid(base.subscript))
        .set("typinput", oid(base.input))
        .set("typoutput", oid(base.output))
        .set("typreceive", oid(base.receive))
        .set("typsend", oid(base.send))
        .set("typmodin", oid(base.modin))
        .set("typmodout", oid(base.modout))
        .set("typanalyze", oid(base.analyze))
        .set("typalign", Value::Char(base.align))
        .set("typstorage", Value::Char(base.storage))
        .set("typcollation", oid(base.collation))
        .set("typdefault", base.default.clone().map_or(Value::Null, Value::text));
    Ok(row)
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
        } else if let Some(base) = &ty.base {
            base_row(table, base, session)?
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
        } else if let Some(base) = element.and_then(|e| e.base.as_ref()) {
            // The array type of a base type, as `DefineType` makes it: the alignment is `d` or `i`.
            let mut row = Row::copy(table, type_row(table, INT4_ARRAY, session)?, session)?;
            row.set("typalign", code(if base.align == b'd' { 'd' } else { 'i' }))
                .set("typdelim", Value::Char(base.delim))
                .set("typmodin", oid(base.modin))
                .set("typmodout", oid(base.modout))
                .set("typcollation", oid(base.collation));
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
            .set("typarray", oid(ty.array))
            .set("typacl", granted(catalog, ObjRef::new(PG_TYPE, ty.oid)));
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
            && let Some(number) = cell(table, row, "attnum", session)?.as_i64().filter(|&n| n < 0)
        {
            system.push((number, row));
        }
    }
    // PostgreSQL reads the rows of a relation by the index on `attrelid` and `attnum`, so a query without `ORDER BY` gets `tableoid` (-6) first and the user columns last.
    system.sort_unstable();
    let system: Vec<usize> = system.into_iter().map(|(_, row)| row).collect();
    let mut rows = Vec::new();
    for rel in catalog.relations() {
        if !matches!(
            rel.kind,
            RelKind::Index | RelKind::PartitionedIndex | RelKind::View | RelKind::Composite
        ) {
            for &template in &system {
                let mut row = Row::copy(table, template, session)?;
                let number = cell(table, template, "attnum", session)?.as_i64().unwrap_or_default();
                let number = i16::try_from(number).unwrap_or_default();
                row.set("attrelid", oid(rel.oid))
                    .set("attacl", granted(catalog, ObjRef::column(rel.oid, number)));
                rows.push(row);
            }
        }
        for (i, column) in rel.columns.iter().enumerate() {
            let mut row = attribute(table, rel, i, column);
            row.set("attacl", granted(catalog, ObjRef::column(rel.oid, number(i))));
            rows.push(row);
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
    if column.storage != 0 {
        storage = column.storage;
    }
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
        .set("attcompression", Value::Char(column.compression))
        .set("attnotnull", Value::Bool(column.not_null))
        .set("atthasdef", Value::Bool(column.has_default))
        .set("atthasmissing", Value::Bool(false))
        .set("attidentity", Value::Char(column.identity))
        .set("attgenerated", Value::Char(column.generated))
        .set("attisdropped", Value::Bool(column.dropped))
        .set("attislocal", Value::Bool(column.local))
        .set("attinhcount", Value::Int2(column.inhcount))
        .set("attcollation", oid(column.collation))
        .set("attstattarget", column.stat_target.map_or(Value::Null, Value::Int2))
        .set(
            "attoptions",
            if column.options.is_empty() { Value::Null } else { text_array(&column.options) },
        )
        .set("attfdwoptions", options(&column.fdw_options));
    row
}

/// True for a table with a partition or a table that inherits from it, as `relhassubclass` of `StoreCatalogInheritance1` gives it.
fn has_children(catalog: &Catalog, parent: u32) -> bool {
    !catalog.children(parent).is_empty()
}

/// True for a partitioned index with a partition, as `relhassubclass` of `IndexSetParentIndex` gives it.
fn has_index_partitions(catalog: &Catalog, parent: u32) -> bool {
    catalog.relations().any(|r| r.index.as_ref().is_some_and(|i| i.parent == parent))
}

/// The rows of `pg_partitioned_table`: the partition key of each partitioned table and its default partition.
fn partitioned_tables<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .filter_map(|rel| {
            let partkey = rel.partition_key.as_ref()?;
            let default = catalog
                .partitions(rel.oid)
                .into_iter()
                .find(|p| {
                    p.partition_of.as_ref().is_some_and(|o| o.bound == PartitionBound::Default)
                })
                .map_or(0, |p| p.oid);
            let mut row = Row::new(table);
            row.set("partrelid", oid(rel.oid))
                .set("partstrat", code(partkey.strategy.code()))
                .set(
                    "partnatts",
                    Value::Int2(i16::try_from(partkey.attrs.len()).unwrap_or(i16::MAX)),
                )
                .set("partdefid", oid(default))
                .set("partattrs", int2_vector(&partkey.attrs))
                .set("partclass", oid_vector(&partkey.classes))
                .set("partcollation", oid_vector(&partkey.collations))
                .set("partexprs", tree(partkey.exprs.as_ref()));
            Some(row)
        })
        .collect()
}

/// The rows of `pg_inherits`: one row for each partition and for each index of a partition that is a partition of an index of the parent table, with the sequence number 1, and one row for each parent of a table of `INHERITS`, with the sequence numbers in the order of the parents. A parent that `NO INHERIT` removed leaves its number unused.
fn inherits<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for rel in catalog.relations() {
        let parents = match (&rel.partition_of, &rel.index) {
            (Some(of), _) => vec![of.parent],
            (None, Some(info)) if info.parent != 0 => vec![info.parent],
            _ => rel.inherits.clone(),
        };
        for (seqno, parent) in (1..).zip(parents) {
            if parent == 0 {
                continue;
            }
            let mut row = Row::new(table);
            row.set("inhrelid", oid(rel.oid))
                .set("inhparent", oid(parent))
                .set("inhseqno", Value::Int4(seqno))
                .set("inhdetachpending", Value::Bool(false));
            rows.push(row);
        }
    }
    rows
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
        .set("conenforced", Value::Bool(con.enforced))
        .set("convalidated", Value::Bool(con.validated))
        .set("conrelid", oid(con.relation))
        .set("contypid", oid(con.domain))
        .set("conindid", oid(con.index))
        .set("conparentid", oid(con.parent))
        .set("confrelid", oid(foreign.map_or(0, |f| f.table)))
        .set("confupdtype", code(foreign.map_or(' ', |f| f.update)))
        .set("confdeltype", code(foreign.map_or(' ', |f| f.delete)))
        .set("confmatchtype", code(foreign.map_or(' ', |f| f.match_type)))
        .set("conislocal", Value::Bool(con.local))
        .set("coninhcount", Value::Int2(con.inhcount))
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

/// The triggers of `CREATE TRIGGER` and the internal triggers of the foreign keys. `tgargs` has each argument with a zero byte after it.
fn triggers<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .triggers()
        .map(|trigger| {
            let args: Vec<u8> =
                trigger.args.iter().flat_map(|a| a.bytes().chain(std::iter::once(0))).collect();
            let name = |n: Option<&String>| n.map_or(Value::Null, |n| Value::text(n.clone()));
            let mut row = Row::new(table);
            row.set("oid", oid(trigger.oid))
                .set("tgrelid", oid(trigger.relation))
                .set("tgparentid", oid(trigger.parent))
                .set("tgname", Value::text(trigger.name.clone()))
                .set("tgfoid", oid(trigger.function))
                .set("tgtype", Value::Int2(trigger.kind))
                .set("tgenabled", Value::Char(trigger.enabled))
                .set("tgisinternal", Value::Bool(trigger.internal))
                .set("tgconstrrelid", oid(trigger.constraint_relation))
                .set("tgconstrindid", oid(trigger.constraint_index))
                .set("tgconstraint", oid(trigger.constraint))
                .set("tgdeferrable", Value::Bool(trigger.deferrable))
                .set("tginitdeferred", Value::Bool(trigger.deferred))
                .set("tgnargs", Value::Int2(i16::try_from(trigger.args.len()).unwrap_or(i16::MAX)))
                .set("tgattr", int2_vector(&trigger.columns))
                .set("tgargs", Value::Bytea(args))
                .set("tgqual", tree(trigger.qual.as_ref()))
                .set("tgoldtable", name(trigger.old_table.as_ref()))
                .set("tgnewtable", name(trigger.new_table.as_ref()));
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
                .set("indisclustered", Value::Bool(info.clustered))
                .set("indisvalid", Value::Bool(info.valid))
                .set("indcheckxmin", Value::Bool(false))
                .set("indisready", Value::Bool(info.ready))
                .set("indislive", Value::Bool(true))
                .set("indisreplident", Value::Bool(info.replica_identity))
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

/// The rows of `pg_policy`. `polqual` and `polwithcheck` give a text that `pg_get_expr` knows, as the catalog keeps the text of the statement of a policy.
fn policies<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .policies()
        .map(|p| {
            let marker = |which: &str| Value::text(format!("policy {} {which}", p.oid));
            let mut row = Row::new(table);
            row.set("oid", oid(p.oid))
                .set("polname", Value::text(p.name.as_str()))
                .set("polrelid", oid(p.relation))
                .set("polcmd", code(char::from(p.cmd)))
                .set("polpermissive", Value::Bool(p.permissive))
                .set("polroles", oid_array(&p.roles))
                .set("polqual", p.qual.as_ref().map_or(Value::Null, |_| marker("qual")))
                .set("polwithcheck", p.check.as_ref().map_or(Value::Null, |_| marker("check")));
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

/// `provariadic`, as `ProcedureCreate` sets it: the element type of the `VARIADIC` argument, or 0 for a function with no `VARIADIC` argument. `anyarray` gives `anyelement`, `anycompatiblearray` gives `anycompatible`, and `"any"` gives `"any"`.
pub fn variadic_type(catalog: &Catalog, f: &Function) -> u32 {
    let Some(i) = f.argmodes.iter().position(|&mode| mode == b'v') else { return 0 };
    match f.allargtypes.get(i).copied().unwrap_or(0) {
        ANYARRAY => ANYELEMENT,
        ANYCOMPATIBLEARRAY => ANYCOMPATIBLE,
        ANY => ANY,
        array => catalog
            .type_by_oid(array)
            .map(|t| t.element)
            .or_else(|| builtin::type_by_oid(array).map(|t| t.elem))
            .unwrap_or(0),
    }
}

/// The functions in SQL. A body in a string is in `prosrc`. For a body in SQL, `prosqlbody` has the text of the statement that made the function, and `prosrc` is empty.
fn functions<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .functions()
        .map(|f| {
            let mut row = Row::new(table);
            #[allow(clippy::cast_precision_loss)]
            let (cost, rows) = (f.cost, f.rows as f32);
            let names = if f.argnames.is_empty() { Value::Null } else { text_array(&f.argnames) };
            row.set("oid", oid(f.oid))
                .set("proname", Value::text(f.name.clone()))
                .set("pronamespace", oid(f.namespace))
                .set("proowner", oid(f.owner))
                .set("prolang", oid(f.lang))
                .set("procost", Value::Float4(cost))
                .set("prorows", Value::Float4(rows))
                .set("provariadic", oid(variadic_type(catalog, f)))
                .set("prosupport", oid(f.support))
                .set("prokind", Value::Char(f.kind))
                .set("prosecdef", Value::Bool(f.secdef))
                .set("proleakproof", Value::Bool(f.leakproof))
                .set("proisstrict", Value::Bool(f.strict))
                .set("proretset", Value::Bool(f.retset))
                .set("provolatile", Value::Char(f.volatile))
                .set("proparallel", Value::Char(f.parallel))
                .set("pronargs", Value::Int2(i16::try_from(f.argtypes.len()).unwrap_or(i16::MAX)))
                .set("pronargdefaults", Value::Int2(f.nargdefaults))
                .set("prorettype", oid(f.rettype))
                .set("proargtypes", oid_vector(&f.argtypes))
                .set("proallargtypes", oid_array(&f.allargtypes))
                .set("proargmodes", char_array(&f.argmodes))
                .set("proargnames", names)
                .set("proargdefaults", tree(f.argdefaults.as_ref()))
                .set("prosrc", Value::text(f.src.clone()))
                .set(
                    "probin",
                    if f.bin.is_empty() { Value::Null } else { Value::text(f.bin.clone()) },
                )
                .set(
                    "prosqlbody",
                    tree(f.body.as_ref().filter(|_| f.src.is_empty()).map(|b| &b.text)),
                )
                .set(
                    "proconfig",
                    if f.config.is_empty() { Value::Null } else { text_array(&f.config) },
                )
                .set("proacl", granted(catalog, ObjRef::new(PG_PROC, f.oid)));
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
                .set("lanvalidator", oid(l.validator))
                .set("lanacl", granted(catalog, ObjRef::new(PG_LANGUAGE, l.oid)));
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

fn conversions<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .conversions()
        .map(|c| {
            let mut row = Row::new(table);
            row.set("oid", oid(c.oid))
                .set("conname", Value::text(c.name.clone()))
                .set("connamespace", oid(c.namespace))
                .set("conowner", oid(c.owner))
                .set("conforencoding", Value::Int4(c.from))
                .set("contoencoding", Value::Int4(c.to))
                .set("conproc", oid(c.proc))
                .set("condefault", Value::Bool(c.default));
            row
        })
        .collect()
}

/// The options of a foreign-data wrapper or a server: a `text[]`, or null for no options.
fn options(options: &[String]) -> Value {
    if options.is_empty() { Value::Null } else { text_array(options) }
}

fn fdws<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .fdws()
        .map(|f| {
            let mut row = Row::new(table);
            row.set("oid", oid(f.oid))
                .set("fdwname", Value::text(f.name.clone()))
                .set("fdwowner", oid(f.owner))
                .set("fdwhandler", oid(f.handler))
                .set("fdwvalidator", oid(f.validator))
                .set("fdwconnection", oid(f.connection))
                .set("fdwacl", granted(catalog, ObjRef::new(PG_FOREIGN_DATA_WRAPPER, f.oid)))
                .set("fdwoptions", options(&f.options));
            row
        })
        .collect()
}

fn servers<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .servers()
        .map(|s| {
            let text = |v: &Option<String>| v.clone().map_or(Value::Null, Value::text);
            let mut row = Row::new(table);
            row.set("oid", oid(s.oid))
                .set("srvname", Value::text(s.name.clone()))
                .set("srvowner", oid(s.owner))
                .set("srvfdw", oid(s.fdw))
                .set("srvtype", text(&s.kind))
                .set("srvversion", text(&s.version))
                .set("srvacl", granted(catalog, ObjRef::new(PG_FOREIGN_SERVER, s.oid)))
                .set("srvoptions", options(&s.options));
            row
        })
        .collect()
}

/// The rows of `pg_publication`.
fn publications<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .publications()
        .map(|p| {
            let mut row = Row::new(table);
            row.set("oid", oid(p.oid))
                .set("pubname", Value::text(p.name.as_str()))
                .set("pubowner", oid(p.owner))
                .set("puballtables", Value::Bool(p.all_tables))
                .set("puballsequences", Value::Bool(p.all_sequences))
                .set("pubinsert", Value::Bool(p.insert))
                .set("pubupdate", Value::Bool(p.update))
                .set("pubdelete", Value::Bool(p.delete))
                .set("pubtruncate", Value::Bool(p.truncate))
                .set("pubviaroot", Value::Bool(p.via_root))
                .set("pubgencols", code(char::from(p.gencols)));
            row
        })
        .collect()
}

/// The rows of `pg_publication_rel`.
fn publication_rels<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .publication_rels()
        .map(|r| {
            let mut row = Row::new(table);
            row.set("oid", oid(r.oid))
                .set("prpubid", oid(r.publication))
                .set("prrelid", oid(r.relation))
                .set("prexcept", Value::Bool(r.except))
                .set("prqual", tree(r.qual.as_ref()))
                .set("prattrs", r.attrs.as_deref().map_or(Value::Null, int2_vector));
            row
        })
        .collect()
}

/// The rows of `pg_publication_namespace`.
fn publication_namespaces<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .publication_namespaces()
        .map(|n| {
            let mut row = Row::new(table);
            row.set("oid", oid(n.oid))
                .set("pnpubid", oid(n.publication))
                .set("pnnspid", oid(n.schema));
            row
        })
        .collect()
}

fn user_mappings<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .user_mappings()
        .map(|m| {
            let mut row = Row::new(table);
            row.set("oid", oid(m.oid))
                .set("umuser", oid(m.user))
                .set("umserver", oid(m.server))
                .set("umoptions", options(&m.options));
            row
        })
        .collect()
}

fn foreign_tables<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .relations()
        .filter_map(|rel| {
            let foreign = rel.foreign.as_ref()?;
            let mut row = Row::new(table);
            row.set("ftrelid", oid(rel.oid))
                .set("ftserver", oid(foreign.server))
                .set("ftoptions", options(&foreign.options));
            Some(row)
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

/// The statistics objects of `CREATE STATISTICS`. `stxstattarget` is null for the default target.
fn statistic_exts<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .stat_exts()
        .map(|s| {
            let mut row = Row::new(table);
            row.set("oid", oid(s.oid))
                .set("stxrelid", oid(s.relation))
                .set("stxname", Value::text(s.name.clone()))
                .set("stxnamespace", oid(s.namespace))
                .set("stxowner", oid(s.owner))
                .set("stxkeys", int2_vector(&s.keys))
                .set("stxstattarget", s.target.map_or(Value::Null, Value::Int2))
                .set("stxkind", char_array(&s.kinds))
                .set("stxexprs", tree(s.exprs.as_ref()));
            row
        })
        .collect()
}

/// The rows of `pg_statistic`: the rows of the system catalogs that the `ANALYZE` of `initdb` wrote, then the rows that `ANALYZE` wrote for the tables of the catalog, as `update_attstats` forms them, with no slot after the slots that have a kind. `stavaluesN` is an `anyarray`, which keeps the text that `array_out` gives with the output function of the type of the column, as the array keeps its element type.
fn statistics<'a>(table: &'a Table, catalog: &Catalog, session: &dyn Session) -> Vec<Row<'a>> {
    let numbers_value = |numbers: Option<&[f32]>| {
        numbers.map_or(Value::Null, |numbers| {
            let items = numbers.iter().map(|&f| Some(Value::Float4(f))).collect();
            Value::Array(Box::new(Array::one(items)))
        })
    };
    let start = |relid: u32, attnum: i16, inherit: bool, null_frac, width, distinct| {
        let mut row = Row::new(table);
        row.set("starelid", oid(relid))
            .set("staattnum", Value::Int2(attnum))
            .set("stainherit", Value::Bool(inherit))
            .set("stanullfrac", Value::Float4(null_frac))
            .set("stawidth", Value::Int4(width))
            .set("stadistinct", Value::Float4(distinct));
        row
    };
    let mut rows = Vec::new();
    for stat in builtin::all_catalog_statistics() {
        let mut row =
            start(stat.table, stat.attnum, stat.inherit, stat.null_frac, stat.width, stat.distinct);
        for n in 1..=5 {
            let slot = stat.slots.get(n - 1);
            let values = slot.and_then(|s| s.values).map_or(Value::Null, Value::text);
            row.set(&format!("stakind{n}"), Value::Int2(slot.map_or(0, |s| s.kind)))
                .set(&format!("staop{n}"), oid(slot.map_or(0, |s| s.operator)))
                .set(&format!("stacoll{n}"), oid(slot.map_or(0, |s| s.collation)))
                .set(&format!("stanumbers{n}"), numbers_value(slot.and_then(|s| s.numbers)))
                .set(&format!("stavalues{n}"), values);
        }
        rows.push(row);
    }
    let kinds =
        |r: &&Relation| matches!(r.kind, RelKind::Table | RelKind::MatView | RelKind::Partitioned);
    for rel in catalog.relations().filter(kinds) {
        for stat in session.statistics(rel.oid) {
            let at = usize::try_from(stat.attnum - 1).unwrap_or(usize::MAX);
            let Some(column) = rel.columns.get(at) else { continue };
            let mut row = start(
                rel.oid,
                stat.attnum,
                stat.inherit,
                stat.null_frac,
                stat.width,
                stat.distinct,
            );
            for n in 1..=5 {
                let slot = stat.slots.get(n - 1);
                let values = slot
                    .and_then(|s| s.values.as_ref())
                    .and_then(|values| anyarray_text(column.ty, values, session))
                    .map_or(Value::Null, Value::Text);
                row.set(&format!("stakind{n}"), Value::Int2(slot.map_or(0, |s| s.kind)))
                    .set(&format!("staop{n}"), oid(slot.map_or(0, |s| s.operator)))
                    .set(&format!("stacoll{n}"), oid(slot.map_or(0, |s| s.collation)))
                    .set(
                        &format!("stanumbers{n}"),
                        numbers_value(slot.and_then(|s| s.numbers.as_deref())),
                    )
                    .set(&format!("stavalues{n}"), values);
            }
            rows.push(row);
        }
    }
    rows
}

/// `array_out` of a one-dimensional array of the values `values` of the type `ty`, or `None` when the output function of a value fails.
fn anyarray_text(ty: u32, values: &[Value], session: &dyn Session) -> Option<String> {
    let delim = builtin::type_by_oid(rupg_func::base_type(ty)).map_or(b',', |t| t.delim);
    let array = Array::one(values.iter().cloned().map(Some).collect());
    let mut out = Vec::new();
    let mut failed = false;
    rupg_types::array_out(&array, delim, &mut out, |item, out| {
        failed |= rupg_func::output(ty, item, session, out).is_err();
    });
    if failed {
        return None;
    }
    String::from_utf8(out).ok()
}

/// The rows of `pg_default_acl` that `ALTER DEFAULT PRIVILEGES` adds.
fn default_acls<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .default_acls()
        .map(|d| {
            let mut row = Row::new(table);
            row.set("oid", oid(d.oid))
                .set("defaclrole", oid(d.role))
                .set("defaclnamespace", oid(d.namespace))
                .set("defaclobjtype", Value::Char(d.objtype))
                .set("defaclacl", text_array(&d.acl));
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

fn operators<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .operators()
        .map(|o| {
            let mut row = Row::new(table);
            row.set("oid", oid(o.oid))
                .set("oprname", Value::text(o.name.clone()))
                .set("oprnamespace", oid(o.namespace))
                .set("oprowner", oid(o.owner))
                .set("oprkind", Value::Char(o.kind))
                .set("oprcanmerge", Value::Bool(o.can_merge))
                .set("oprcanhash", Value::Bool(o.can_hash))
                .set("oprleft", oid(o.left))
                .set("oprright", oid(o.right))
                .set("oprresult", oid(o.result))
                .set("oprcom", oid(o.commutator))
                .set("oprnegate", oid(o.negator))
                .set("oprcode", oid(o.code))
                .set("oprrest", oid(o.restrict))
                .set("oprjoin", oid(o.join));
            row
        })
        .collect()
}

fn aggregates<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::text);
    catalog
        .aggregates()
        .map(|a| {
            let mut row = Row::new(table);
            row.set("aggfnoid", oid(a.function))
                .set("aggkind", Value::Char(a.kind))
                .set("aggnumdirectargs", Value::Int2(a.ndirect))
                .set("aggtransfn", oid(a.transfn))
                .set("aggfinalfn", oid(a.finalfn))
                .set("aggcombinefn", oid(a.combinefn))
                .set("aggserialfn", oid(a.serialfn))
                .set("aggdeserialfn", oid(a.deserialfn))
                .set("aggmtransfn", oid(a.mtransfn))
                .set("aggminvtransfn", oid(a.minvtransfn))
                .set("aggmfinalfn", oid(a.mfinalfn))
                .set("aggfinalextra", Value::Bool(a.finalextra))
                .set("aggmfinalextra", Value::Bool(a.mfinalextra))
                .set("aggfinalmodify", Value::Char(a.finalmodify))
                .set("aggmfinalmodify", Value::Char(a.mfinalmodify))
                .set("aggsortop", oid(a.sortop))
                .set("aggtranstype", oid(a.transtype))
                .set("aggtransspace", Value::Int4(a.transspace))
                .set("aggmtranstype", oid(a.mtranstype))
                .set("aggmtransspace", Value::Int4(a.mtransspace))
                .set("agginitval", text(&a.initval))
                .set("aggminitval", text(&a.minitval));
            row
        })
        .collect()
}

fn opfamilies<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .opfamilies()
        .map(|f| {
            let mut row = Row::new(table);
            row.set("oid", oid(f.oid))
                .set("opfmethod", oid(f.method))
                .set("opfname", Value::text(f.name.clone()))
                .set("opfnamespace", oid(f.namespace))
                .set("opfowner", oid(f.owner));
            row
        })
        .collect()
}

fn opclasses<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .opclasses()
        .map(|c| {
            let mut row = Row::new(table);
            row.set("oid", oid(c.oid))
                .set("opcmethod", oid(c.method))
                .set("opcname", Value::text(c.name.clone()))
                .set("opcnamespace", oid(c.namespace))
                .set("opcowner", oid(c.owner))
                .set("opcfamily", oid(c.family))
                .set("opcintype", oid(c.intype))
                .set("opcdefault", Value::Bool(c.default))
                .set("opckeytype", oid(c.keytype));
            row
        })
        .collect()
}

fn amops<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .amops()
        .map(|a| {
            let mut row = Row::new(table);
            row.set("oid", oid(a.oid))
                .set("amopfamily", oid(a.family))
                .set("amoplefttype", oid(a.left))
                .set("amoprighttype", oid(a.right))
                .set("amopstrategy", Value::Int2(a.strategy))
                .set("amoppurpose", Value::Char(a.purpose))
                .set("amopopr", oid(a.operator))
                .set("amopmethod", oid(a.method))
                .set("amopsortfamily", oid(a.sortfamily));
            row
        })
        .collect()
}

fn amprocs<'a>(table: &'a Table, catalog: &Catalog) -> Vec<Row<'a>> {
    catalog
        .amprocs()
        .map(|a| {
            let mut row = Row::new(table);
            row.set("oid", oid(a.oid))
                .set("amprocfamily", oid(a.family))
                .set("amproclefttype", oid(a.left))
                .set("amprocrighttype", oid(a.right))
                .set("amprocnum", Value::Int2(a.number))
                .set("amproc", oid(a.function));
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

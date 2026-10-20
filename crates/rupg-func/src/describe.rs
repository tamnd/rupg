//! `pg_describe_object`: the description of an object that `getObjectDescription` of `objectaddress.c` gives, as the messages of the dependencies show it.
//!
//! A name has its schema when the search path does not find the object by its name. The name of a column, a constraint, a rule, a trigger, a schema, a role, a database, a tablespace, a language, an access method or an extension has no quotes. Each object that does not exist gives null, as `getObjectDescription` with `missing_ok` gives.

use rupg_catalog::{ObjRef, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_types::{RegKind, Value};

use crate::{Call, Kernel, Session, bad_value, reg};

/// `pg_class`.
const PG_CLASS: u32 = 1259;
/// `pg_proc`.
const PG_PROC: u32 = 1255;
/// `pg_type`.
const PG_TYPE: u32 = 1247;
/// `pg_cast`.
const PG_CAST: u32 = 2605;
/// `pg_collation`.
const PG_COLLATION: u32 = 3456;
/// `pg_constraint`.
const PG_CONSTRAINT: u32 = 2606;
/// `pg_conversion`.
const PG_CONVERSION: u32 = 2607;
/// `pg_attrdef`.
const PG_ATTRDEF: u32 = 2604;
/// `pg_language`.
const PG_LANGUAGE: u32 = 2612;
/// `pg_largeobject`.
const PG_LARGEOBJECT: u32 = 2613;
/// `pg_operator`.
const PG_OPERATOR: u32 = 2617;
/// `pg_opclass`.
const PG_OPCLASS: u32 = 2616;
/// `pg_opfamily`.
const PG_OPFAMILY: u32 = 2753;
/// `pg_am`.
const PG_AM: u32 = 2601;
/// `pg_amop`.
const PG_AMOP: u32 = 2602;
/// `pg_amproc`.
const PG_AMPROC: u32 = 2603;
/// `pg_rewrite`.
const PG_REWRITE: u32 = 2618;
/// `pg_trigger`.
const PG_TRIGGER: u32 = 2620;
/// `pg_namespace`.
const PG_NAMESPACE: u32 = 2615;
/// `pg_ts_parser`.
const PG_TS_PARSER: u32 = 3601;
/// `pg_ts_dict`.
const PG_TS_DICT: u32 = 3600;
/// `pg_ts_template`.
const PG_TS_TEMPLATE: u32 = 3764;
/// `pg_ts_config`.
const PG_TS_CONFIG: u32 = 3602;
/// `pg_authid`.
const PG_AUTHID: u32 = 1260;
/// `pg_database`.
const PG_DATABASE: u32 = 1262;
/// `pg_tablespace`.
const PG_TABLESPACE: u32 = 1213;
/// `pg_extension`.
const PG_EXTENSION: u32 = 3079;

/// The catalogs of the kinds of objects that rupg does not have: `pg_statistic_ext`, `pg_auth_members`, `pg_foreign_data_wrapper`, `pg_foreign_server`, `pg_user_mapping`, `pg_default_acl`, `pg_event_trigger`, `pg_parameter_acl`, `pg_policy`, `pg_publication`, `pg_publication_namespace`, `pg_publication_rel`, `pg_subscription` and `pg_transform`.
const NO_OBJECTS: [u32; 14] =
    [3381, 1261, 2328, 1417, 1418, 826, 3466, 6243, 3256, 6104, 6237, 6106, 6100, 3576];

/// The names of the system columns, from `ctid` with the number -1.
const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"];

/// The OID argument at `index`.
fn oid_arg(args: &[Value], index: usize) -> Result<u32> {
    args.get(index).and_then(Value::as_oid).ok_or_else(bad_value)
}

/// `pg_describe_object(oid, oid, integer)`: the description of the object, or null for OID 0 in both the catalog and the object.
fn describe_object(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (class, oid) = (oid_arg(args, 0)?, oid_arg(args, 1)?);
    let sub = args
        .get(2)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(bad_value)?;
    if class == 0 && oid == 0 {
        return Ok(Value::Null);
    }
    Ok(describe(ObjRef { class, oid, sub }, call.session)?.map_or(Value::Null, Value::Text))
}

/// `getObjectDescription` with `missing_ok`: the description of the object, or `None` when it does not exist.
///
/// # Errors
///
/// `XX000` for a catalog that is not a catalog of objects.
pub fn describe(object: ObjRef, session: &dyn Session) -> Result<Option<String>> {
    let oid = object.oid;
    let catalog = session.catalog();
    let text = match object.class {
        PG_CLASS if object.sub != 0 => column_text(oid, object.sub, session),
        PG_CLASS => relation_text(oid, session),
        PG_PROC => function_text(oid, session),
        PG_TYPE => reg::name_of(RegKind::Type, oid, session).map(|t| format!("type {t}")),
        PG_CAST => cast_text(oid, session),
        PG_COLLATION => {
            reg::catalog_text(Named::Collation, oid, session).map(|c| format!("collation {c}"))
        }
        PG_CONSTRAINT => catalog.and_then(|c| c.constraint(oid)).and_then(|con| {
            if con.relation == 0 {
                return Some(format!("constraint {}", con.name));
            }
            Some(format!("constraint {} on {}", con.name, relation_text(con.relation, session)?))
        }),
        PG_CONVERSION => {
            named_text(Named::Conversion, oid, session).map(|c| format!("conversion {c}"))
        }
        PG_ATTRDEF => catalog
            .and_then(|c| c.defaults().find(|d| d.oid == oid))
            .and_then(|d| column_text(d.relation, i32::from(d.column), session))
            .map(|column| format!("default value for {column}")),
        PG_LANGUAGE => builtin::owned_by_oid(Owned::Language, oid)
            .map(|l| l.name)
            .or_else(|| catalog?.languages().find(|l| l.oid == oid).map(|l| l.name.as_str()))
            .map(|name| format!("language {name}")),
        PG_LARGEOBJECT => {
            builtin::owned_by_oid(Owned::LargeObject, oid).map(|_| format!("large object {oid}"))
        }
        PG_OPERATOR => {
            reg::name_of(RegKind::Operator, oid, session).map(|o| format!("operator {o}"))
        }
        PG_OPCLASS => {
            method_text(Named::Opclass, oid, session).map(|c| format!("operator class {c}"))
        }
        PG_OPFAMILY => opfamily_text(oid, session),
        PG_AM => builtin::access_method_name(oid).map(|name| format!("access method {name}")),
        PG_AMOP => builtin::amop_by_oid(oid).and_then(|a| {
            Some(format!(
                "operator {} ({}, {}) of {}: {}",
                a.strategy,
                reg::type_text(a.left, None, session).ok()?,
                reg::type_text(a.right, None, session).ok()?,
                opfamily_text(a.family, session)?,
                reg::name_of(RegKind::Operator, a.operator, session)?
            ))
        }),
        PG_AMPROC => builtin::amproc_by_oid(oid).and_then(|a| {
            Some(format!(
                "function {} ({}, {}) of {}: {}",
                a.number,
                reg::type_text(a.left, None, session).ok()?,
                reg::type_text(a.right, None, session).ok()?,
                opfamily_text(a.family, session)?,
                procedure_text(a.proc, session)?
            ))
        }),
        PG_REWRITE => catalog.and_then(|c| match c.view_of_rule(oid) {
            Some(view) => Some(format!("rule _RETURN on {}", relation_text(view.oid, session)?)),
            None => {
                let rule = c.rule(oid)?;
                Some(format!("rule {} on {}", rule.name, relation_text(rule.relation, session)?))
            }
        }),
        PG_TRIGGER => catalog.and_then(|c| c.trigger(oid)).and_then(|t| {
            Some(format!("trigger {} on {}", t.name, relation_text(t.relation, session)?))
        }),
        PG_NAMESPACE => reg::namespace_name(oid, session).map(|name| format!("schema {name}")),
        PG_TS_PARSER => ts_text(Named::Parser, "parser", oid, session),
        PG_TS_DICT => ts_text(Named::Dictionary, "dictionary", oid, session),
        PG_TS_TEMPLATE => ts_text(Named::Template, "template", oid, session),
        PG_TS_CONFIG => ts_text(Named::Config, "configuration", oid, session),
        PG_AUTHID => rupg_pgcatalog::roles::current().name(oid).map(|name| format!("role {name}")),
        PG_DATABASE => match builtin::named_by_oid(Named::Database, oid) {
            Some(row) => Some(format!("database {}", row.name)),
            None => session
                .databases()
                .iter()
                .find(|d| d.oid == oid)
                .map(|d| format!("database {}", d.name)),
        },
        PG_TABLESPACE => {
            builtin::owned_by_oid(Owned::Tablespace, oid).map(|t| format!("tablespace {}", t.name))
        }
        PG_EXTENSION => catalog
            .and_then(|c| c.extensions().find(|e| e.oid == oid))
            .map(|e| format!("extension {}", e.name)),
        class if NO_OBJECTS.contains(&class) => None,
        class => {
            return Err(Error::new(
                SqlState::INTERNAL_ERROR,
                format!("unsupported object class: {class}"),
            ));
        }
    };
    Ok(text)
}

/// `getRelationDescription`: the kind and the name of the relation.
fn relation_text(oid: u32, session: &dyn Session) -> Option<String> {
    let (code, name, namespace) = match builtin::class_by_oid(oid) {
        Some(row) => (row.kind, row.name, row.namespace),
        None => {
            let rel = session.catalog()?.relation(oid)?;
            (u8::try_from(rel.kind.code()).ok()?, rel.name.as_str(), rel.namespace)
        }
    };
    let word = match code {
        b'i' | b'I' => "index",
        b'S' => "sequence",
        b't' => "toast table",
        b'v' => "view",
        b'm' => "materialized view",
        b'c' => "composite type",
        b'f' => "foreign table",
        _ => "table",
    };
    let schema = if reg::class_visible(oid, session)? {
        None
    } else {
        reg::namespace_name(namespace, session)
    };
    Some(format!("{word} {}", reg::qualified(schema, name)))
}

/// The description of a column of a relation, also of a system column of a relation that has them.
fn column_text(oid: u32, number: i32, session: &dyn Session) -> Option<String> {
    let number = i16::try_from(number).ok()?;
    let name = match builtin::class_by_oid(oid) {
        Some(_) => builtin::attributes(oid).iter().find(|a| a.num == number)?.name,
        None => {
            let rel = session.catalog()?.relation(oid)?;
            if number > 0 {
                rel.column(number)?.name.as_str()
            } else if matches!(rel.kind, RelKind::Table | RelKind::Toast | RelKind::Sequence) {
                SYSTEM_COLUMNS.get(usize::try_from(-i32::from(number) - 1).ok()?)?
            } else {
                return None;
            }
        }
    };
    Some(format!("column {name} of {}", relation_text(oid, session)?))
}

/// `format_procedure` with the kind of the function.
fn function_text(oid: u32, session: &dyn Session) -> Option<String> {
    let kind = builtin::proc_by_oid(oid).map_or(b'f', |p| p.kind);
    let word = if kind == b'p' { "procedure" } else { "function" };
    Some(format!("{word} {}", procedure_text(oid, session)?))
}

/// `format_procedure`: the name of a built-in function or a function of the catalog, with its schema when it is not visible, and the types of its arguments.
fn procedure_text(oid: u32, session: &dyn Session) -> Option<String> {
    let catalog = session.catalog();
    let (name, namespace, args) = match builtin::proc_by_oid(oid) {
        Some(proc) => (proc.name, proc.namespace, proc.argtypes),
        None => {
            let function = catalog?.function(oid)?;
            (function.name.as_str(), function.namespace, function.argtypes.as_slice())
        }
    };
    let first = reg::path(session).into_iter().find_map(|ns| {
        builtin::procs_named(name)
            .find(|p| p.namespace == ns && p.argtypes == args)
            .map(|p| p.oid)
            .or_else(|| {
                catalog?
                    .functions()
                    .find(|f| f.namespace == ns && f.name == name && f.argtypes == args)
                    .map(|f| f.oid)
            })
    });
    let schema = if first == Some(oid) { None } else { reg::namespace_name(namespace, session) };
    let args: Vec<String> =
        args.iter().map(|&ty| reg::type_text(ty, None, session)).collect::<Result<_>>().ok()?;
    Some(format!("{}({})", reg::qualified(schema, name), args.join(",")))
}

/// `getCastDescription`.
fn cast_text(oid: u32, session: &dyn Session) -> Option<String> {
    let (source, target) = match builtin::casts().iter().find(|c| c.oid == oid) {
        Some(cast) => (cast.source, cast.target),
        None => {
            let cast = session.catalog()?.casts().find(|c| c.oid == oid)?;
            (cast.source, cast.target)
        }
    };
    Some(format!(
        "cast from {} to {}",
        reg::type_text(source, None, session).ok()?,
        reg::type_text(target, None, session).ok()?
    ))
}

/// The name of a built-in object of a kind that has only built-in objects, with its schema when it is not visible.
fn named_text(kind: Named, oid: u32, session: &dyn Session) -> Option<String> {
    let row = builtin::named_by_oid(kind, oid)?;
    let schema = if reg::visible(kind, row, session) {
        None
    } else {
        reg::namespace_name(row.namespace, session)
    };
    Some(reg::qualified(schema, row.name))
}

/// The name of an operator class or an operator family and its access method.
fn method_text(kind: Named, oid: u32, session: &dyn Session) -> Option<String> {
    let method = builtin::named_by_oid(kind, oid)?.method;
    Some(format!(
        "{} for access method {}",
        named_text(kind, oid, session)?,
        builtin::access_method_name(method)?
    ))
}

/// `getOpFamilyDescription`.
fn opfamily_text(oid: u32, session: &dyn Session) -> Option<String> {
    method_text(Named::Opfamily, oid, session).map(|f| format!("operator family {f}"))
}

/// The description of an object of text search.
fn ts_text(kind: Named, word: &str, oid: u32, session: &dyn Session) -> Option<String> {
    reg::catalog_text(kind, oid, session).map(|name| format!("text search {word} {name}"))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_describe_object" => describe_object,
        _ => return None,
    };
    Some(kernel)
}

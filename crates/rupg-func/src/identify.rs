//! `pg_identify_object`: the type, the schema, the name and the identity of an object, as `getObjectTypeDescription` and `getObjectIdentityParts` of `objectaddress.c` give them.
//!
//! The identity always has the schema of the object, and each name in it has quotes when it needs them. The name is given only for the kinds of objects whose name is unique in its schema. An object that does not exist gives its type and null in the other fields.

use rupg_catalog::ObjRef;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_types::{Record, Value, oid};

use crate::describe::{
    NO_OBJECTS, PG_AM, PG_AMOP, PG_AMPROC, PG_ATTRDEF, PG_AUTHID, PG_CAST, PG_CLASS, PG_COLLATION,
    PG_CONSTRAINT, PG_CONVERSION, PG_DATABASE, PG_EXTENSION, PG_LANGUAGE, PG_LARGEOBJECT,
    PG_NAMESPACE, PG_OPCLASS, PG_OPERATOR, PG_OPFAMILY, PG_PROC, PG_REWRITE, PG_STATISTIC_EXT,
    PG_TABLESPACE, PG_TRIGGER, PG_TS_CONFIG, PG_TS_DICT, PG_TS_PARSER, PG_TS_TEMPLATE, PG_TYPE,
    amop_parts, amproc_parts, column_name, oid_arg,
};
use crate::text::quote_identifier;
use crate::{Call, Kernel, Session, bad_value, reg};

/// The parts of an object that exists: the OID of its schema, its name and its identity.
struct Parts {
    schema: Option<u32>,
    name: Option<String>,
    identity: String,
}

impl Parts {
    /// The parts of an object without a schema and without a unique name.
    fn identity(identity: String) -> Self {
        Self { schema: None, name: None, identity }
    }

    /// The parts of an object whose name is unique in the schema `schema`, and whose identity is the name with the schema.
    fn in_schema(schema: u32, name: &str, session: &dyn Session) -> Option<Self> {
        let identity = reg::qualified(Some(reg::namespace_name(schema, session)?), name);
        Some(Self { schema: Some(schema), name: Some(quote_identifier(name)), identity })
    }

    /// The parts of an object without a schema whose name is unique.
    fn named(name: &str) -> Self {
        let name = quote_identifier(name);
        Self { schema: None, identity: name.clone(), name: Some(name) }
    }
}

/// `pg_identify_object(oid, oid, integer)`: the type, the schema, the name and the identity of the object.
fn identify_object(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (class, oid) = (oid_arg(args, 0)?, oid_arg(args, 1)?);
    let sub = args
        .get(2)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(bad_value)?;
    let object = ObjRef { class, oid, sub };
    let kind = type_description(object, call.session)?;
    let mut values = vec![Value::Text(kind), Value::Null, Value::Null, Value::Null];
    if let Some(parts) = parts(object, call.session) {
        let schema = parts.schema.and_then(|ns| reg::namespace_name(ns, call.session));
        values[1] = schema.map_or(Value::Null, |s| Value::Text(quote_identifier(s)));
        values[2] = parts.name.map_or(Value::Null, Value::Text);
        values[3] = Value::Text(parts.identity);
    }
    Ok(Value::Record(Box::new(Record::new(vec![oid::TEXT; 4], values))))
}

/// `getObjectTypeDescription` with `missing_ok`: the kind of the object.
///
/// # Errors
///
/// `XX000` for a catalog that is not a catalog of objects.
fn type_description(object: ObjRef, session: &dyn Session) -> Result<String> {
    let word = match object.class {
        PG_CLASS => {
            // A relation that does not exist is `relation`, also with the number of a column.
            return Ok(match relation_kind(object.oid, session) {
                Some(word) if object.sub != 0 => format!("{word} column"),
                Some(word) => word.to_string(),
                None => "relation".to_string(),
            });
        }
        PG_PROC => match proc_kind(object.oid, session) {
            None => "routine",
            Some(b'a') => "aggregate",
            Some(b'p') => "procedure",
            Some(_) => "function",
        },
        PG_TYPE => "type",
        PG_CAST => "cast",
        PG_COLLATION => "collation",
        PG_CONSTRAINT => match session.catalog().and_then(|c| c.constraint(object.oid)) {
            Some(con) if con.relation != 0 => "table constraint",
            Some(con) if con.domain != 0 => "domain constraint",
            Some(con) => {
                return Err(Error::new(
                    SqlState::INTERNAL_ERROR,
                    format!("invalid constraint {} has neither relation nor domain", con.oid),
                ));
            }
            None => "constraint",
        },
        PG_CONVERSION => "conversion",
        PG_ATTRDEF => "default value",
        PG_LANGUAGE => "language",
        PG_LARGEOBJECT => "large object",
        PG_OPERATOR => "operator",
        PG_OPCLASS => "operator class",
        PG_OPFAMILY => "operator family",
        PG_AM => "access method",
        PG_AMOP => "operator of access method",
        PG_AMPROC => "function of access method",
        PG_REWRITE => "rule",
        PG_TRIGGER => "trigger",
        PG_NAMESPACE => "schema",
        PG_TS_PARSER => "text search parser",
        PG_TS_DICT => "text search dictionary",
        PG_TS_TEMPLATE => "text search template",
        PG_TS_CONFIG => "text search configuration",
        PG_AUTHID => "role",
        PG_DATABASE => "database",
        PG_TABLESPACE => "tablespace",
        PG_EXTENSION => "extension",
        PG_STATISTIC_EXT => "statistics object",
        class if NO_OBJECTS.contains(&class) => no_object_kind(class),
        class => {
            return Err(Error::new(
                SqlState::INTERNAL_ERROR,
                format!("unsupported object class: {class}"),
            ));
        }
    };
    Ok(word.to_string())
}

/// The kind of an object of a catalog of `NO_OBJECTS`.
fn no_object_kind(class: u32) -> &'static str {
    match class {
        1261 => "role membership",
        2328 => "foreign-data wrapper",
        1417 => "server",
        1418 => "user mapping",
        826 => "default acl",
        3466 => "event trigger",
        6243 => "parameter ACL",
        3256 => "policy",
        6104 => "publication",
        6237 => "publication namespace",
        6106 => "publication relation",
        6100 => "subscription",
        _ => "transform",
    }
}

/// `getRelationTypeDescription`: the kind of the relation, or `None` when it does not exist.
fn relation_kind(oid: u32, session: &dyn Session) -> Option<&'static str> {
    let code = match builtin::class_by_oid(oid) {
        Some(row) => row.kind,
        None => u8::try_from(session.catalog()?.relation(oid)?.kind.code()).ok()?,
    };
    Some(match code {
        b'i' | b'I' => "index",
        b'S' => "sequence",
        b't' => "toast table",
        b'v' => "view",
        b'm' => "materialized view",
        b'c' => "composite type",
        b'f' => "foreign table",
        b'r' | b'p' => "table",
        _ => "relation",
    })
}

/// The `prokind` of the function, or `None` when it does not exist.
fn proc_kind(oid: u32, session: &dyn Session) -> Option<u8> {
    builtin::proc_by_oid(oid)
        .map(|p| p.kind)
        .or_else(|| session.catalog()?.function(oid).map(|f| f.kind))
}

/// `getObjectIdentityParts` with `missing_ok`, and the schema and the name of `pg_identify_object`: `None` when the object does not exist.
fn parts(object: ObjRef, session: &dyn Session) -> Option<Parts> {
    let oid = object.oid;
    let catalog = session.catalog();
    match object.class {
        PG_CLASS => {
            let (name, namespace) = reg::class_name(oid, session)?;
            let mut parts = Parts::in_schema(namespace, name, session)?;
            if object.sub != 0 {
                let column = column_name(oid, object.sub, session)?;
                parts.identity = format!("{}.{}", parts.identity, quote_identifier(column));
            }
            Some(parts)
        }
        PG_PROC => {
            let namespace = match builtin::proc_by_oid(oid) {
                Some(proc) => proc.namespace,
                None => catalog?.function(oid)?.namespace,
            };
            Some(Parts { schema: Some(namespace), name: None, identity: procedure(oid, session)? })
        }
        PG_TYPE => {
            let (name, namespace) = reg::type_name(oid, session)?;
            Some(Parts {
                schema: Some(namespace),
                name: Some(quote_identifier(name)),
                identity: reg::type_text_qualified(oid, session)?,
            })
        }
        PG_CAST => {
            let (source, target) = match builtin::casts().iter().find(|c| c.oid == oid) {
                Some(cast) => (cast.source, cast.target),
                None => {
                    let cast = catalog?.casts().find(|c| c.oid == oid)?;
                    (cast.source, cast.target)
                }
            };
            Some(Parts::identity(format!(
                "({} AS {})",
                reg::type_text_qualified(source, session)?,
                reg::type_text_qualified(target, session)?
            )))
        }
        PG_COLLATION => in_catalog(Named::Collation, oid, session),
        PG_CONSTRAINT => {
            let con = catalog?.constraint(oid)?;
            let on = if con.relation != 0 {
                relation(con.relation, session)?
            } else {
                reg::type_text_qualified(con.domain, session)?
            };
            Some(Parts {
                schema: Some(con.namespace),
                name: None,
                identity: format!("{} on {on}", quote_identifier(&con.name)),
            })
        }
        PG_CONVERSION => match builtin::named_by_oid(Named::Conversion, oid) {
            Some(row) => Parts::in_schema(row.namespace, row.name, session),
            None => {
                let conversion = catalog?.conversion(oid)?;
                Parts::in_schema(conversion.namespace, &conversion.name, session)
            }
        },
        PG_ATTRDEF => {
            let default = catalog?.defaults().find(|d| d.oid == oid)?;
            let column = column_name(default.relation, i32::from(default.column), session)?;
            Some(Parts::identity(format!(
                "for {}.{}",
                relation(default.relation, session)?,
                quote_identifier(column)
            )))
        }
        PG_LANGUAGE => builtin::owned_by_oid(Owned::Language, oid)
            .map(|l| l.name)
            .or_else(|| catalog?.languages().find(|l| l.oid == oid).map(|l| l.name.as_str()))
            .map(Parts::named),
        PG_LARGEOBJECT => {
            builtin::owned_by_oid(Owned::LargeObject, oid).map(|_| Parts::identity(oid.to_string()))
        }
        PG_OPERATOR => {
            let op = builtin::operator_by_oid(oid)?;
            let arg = |ty: u32| {
                if ty == 0 {
                    Some("NONE".to_string())
                } else {
                    reg::type_text_qualified(ty, session)
                }
            };
            let identity = format!(
                "{}.{}({},{})",
                quote_identifier(reg::namespace_name(op.namespace, session)?),
                op.name,
                arg(op.left)?,
                arg(op.right)?
            );
            Some(Parts { schema: Some(op.namespace), name: None, identity })
        }
        PG_OPCLASS => method_parts(Named::Opclass, oid, session),
        PG_OPFAMILY => method_parts(Named::Opfamily, oid, session),
        PG_AM => builtin::access_method_name(oid).map(Parts::named),
        PG_AMOP => amop_parts(oid, session).and_then(|(family, left, right, n, _)| {
            Some(Parts::identity(format!(
                "operator {n} ({}, {}) of {}",
                reg::type_text_qualified(left, session)?,
                reg::type_text_qualified(right, session)?,
                method_parts(Named::Opfamily, family, session)?.identity
            )))
        }),
        PG_AMPROC => amproc_parts(oid, session).and_then(|(family, left, right, n, _)| {
            Some(Parts::identity(format!(
                "function {n} ({}, {}) of {}",
                reg::type_text_qualified(left, session)?,
                reg::type_text_qualified(right, session)?,
                method_parts(Named::Opfamily, family, session)?.identity
            )))
        }),
        PG_REWRITE => {
            let catalog = catalog?;
            let (name, on) = match catalog.view_of_rule(oid) {
                Some(view) => ("_RETURN", view.oid),
                None => {
                    let rule = catalog.rule(oid)?;
                    (rule.name.as_str(), rule.relation)
                }
            };
            Some(Parts::identity(format!(
                "{} on {}",
                quote_identifier(name),
                relation(on, session)?
            )))
        }
        rupg_catalog::PG_POLICY => {
            let policy = catalog?.policy(oid)?;
            Some(Parts::identity(format!(
                "{} on {}",
                quote_identifier(&policy.name),
                relation(policy.relation, session)?
            )))
        }
        PG_TRIGGER => {
            let trigger = catalog?.trigger(oid)?;
            Some(Parts::identity(format!(
                "{} on {}",
                quote_identifier(&trigger.name),
                relation(trigger.relation, session)?
            )))
        }
        PG_NAMESPACE => reg::namespace_name(oid, session).map(Parts::named),
        PG_TS_PARSER => in_catalog(Named::Parser, oid, session),
        PG_TS_DICT => in_catalog(Named::Dictionary, oid, session),
        PG_TS_TEMPLATE => in_catalog(Named::Template, oid, session),
        PG_TS_CONFIG => in_catalog(Named::Config, oid, session),
        PG_AUTHID => rupg_pgcatalog::roles::current().name(oid).map(Parts::named),
        PG_DATABASE => match builtin::named_by_oid(Named::Database, oid) {
            Some(row) => Some(Parts::named(row.name)),
            None => {
                session.databases().iter().find(|d| d.oid == oid).map(|d| Parts::named(&d.name))
            }
        },
        PG_TABLESPACE => {
            crate::dbsize::tablespace_name(oid, session).map(|name| Parts::named(&name))
        }
        PG_EXTENSION => catalog?.extensions().find(|e| e.oid == oid).map(|e| Parts::named(&e.name)),
        PG_STATISTIC_EXT => {
            let stat = catalog?.stat_ext(oid)?;
            Parts::in_schema(stat.namespace, &stat.name, session)
        }
        _ => None,
    }
}

/// `getRelationIdentity`: the name of the relation with its schema.
fn relation(oid: u32, session: &dyn Session) -> Option<String> {
    let (name, namespace) = reg::class_name(oid, session)?;
    Some(reg::qualified(Some(reg::namespace_name(namespace, session)?), name))
}

/// `format_procedure_qualified`: the name of the function with its schema, and the types of its arguments with their schemas.
fn procedure(oid: u32, session: &dyn Session) -> Option<String> {
    let (name, namespace, args) = match builtin::proc_by_oid(oid) {
        Some(proc) => (proc.name, proc.namespace, proc.argtypes),
        None => {
            let function = session.catalog()?.function(oid)?;
            (function.name.as_str(), function.namespace, function.argtypes.as_slice())
        }
    };
    let args: Vec<String> =
        args.iter().map(|&ty| reg::type_text_qualified(ty, session)).collect::<Option<_>>()?;
    let name = reg::qualified(Some(reg::namespace_name(namespace, session)?), name);
    Some(format!("{name}({})", args.join(",")))
}

/// The parts of a collation or an object of text search, built in or of the catalog.
fn in_catalog(kind: Named, oid: u32, session: &dyn Session) -> Option<Parts> {
    let (name, namespace) = reg::catalog_name(kind, oid, session)?;
    Parts::in_schema(namespace, &name, session)
}

/// The parts of an operator class or an operator family: the identity has `USING` and the access method.
fn method_parts(kind: Named, oid: u32, session: &dyn Session) -> Option<Parts> {
    let (name, namespace, method) = reg::method_row(kind, oid, session)?;
    let mut parts = Parts::in_schema(namespace, &name, session)?;
    let method = builtin::access_method_name(method)?;
    parts.identity = format!("{} USING {}", parts.identity, quote_identifier(method));
    Some(parts)
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_identify_object" => identify_object,
        _ => return None,
    };
    Some(kernel)
}

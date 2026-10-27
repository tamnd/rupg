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

/// The parts of an object that exists: the OID of its schema, its name, its identity, and the names and the arguments that `pg_get_object_address` takes to find it again.
struct Parts {
    schema: Option<u32>,
    name: Option<String>,
    identity: String,
    names: Vec<String>,
    args: Vec<String>,
}

impl Parts {
    /// The parts of an object without a schema and without a unique name.
    fn identity(identity: String) -> Self {
        Self { schema: None, name: None, identity, names: Vec::new(), args: Vec::new() }
    }

    /// The parts of an object whose name is unique in the schema `schema`, and whose identity is the name with the schema.
    fn in_schema(schema: u32, name: &str, session: &dyn Session) -> Option<Self> {
        let schema_name = reg::namespace_name(schema, session)?;
        let identity = reg::qualified(Some(schema_name), name);
        let names = vec![schema_name.to_string(), name.to_string()];
        Some(Self {
            schema: Some(schema),
            name: Some(quote_identifier(name)),
            identity,
            names,
            args: Vec::new(),
        })
    }

    /// The parts of an object without a schema whose name is unique.
    fn named(name: &str) -> Self {
        let names = vec![name.to_string()];
        let name = quote_identifier(name);
        Self { schema: None, identity: name.clone(), name: Some(name), names, args: Vec::new() }
    }

    /// The parts with the names and the arguments `names` and `args`.
    fn with(mut self, names: Vec<String>, args: Vec<String>) -> Self {
        self.names = names;
        self.args = args;
        self
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

/// `pg_identify_object_as_address(oid, oid, integer)`: the type of the object, and the names and the arguments that `pg_get_object_address` takes to find it.
fn identify_object_as_address(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (class, oid) = (oid_arg(args, 0)?, oid_arg(args, 1)?);
    let sub = args
        .get(2)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(bad_value)?;
    let object = ObjRef { class, oid, sub };
    let kind = type_description(object, call.session)?;
    let mut values = vec![Value::Text(kind), Value::Null, Value::Null];
    if let Some(parts) = parts(object, call.session) {
        let array = |list: Vec<String>| {
            Value::Array(Box::new(rupg_types::Array::one(
                list.into_iter().map(|s| Some(Value::Text(s))).collect(),
            )))
        };
        values[1] = array(parts.names);
        values[2] = array(parts.args);
    }
    Ok(Value::Record(Box::new(Record::new(
        vec![oid::TEXT, oid::TEXT_ARRAY, oid::TEXT_ARRAY],
        values,
    ))))
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
                parts.names.push(column.to_string());
            }
            Some(parts)
        }
        PG_PROC => {
            let namespace = match builtin::proc_by_oid(oid) {
                Some(proc) => proc.namespace,
                None => catalog?.function(oid)?.namespace,
            };
            let (identity, names, args) = procedure(oid, session)?;
            Some(Parts { schema: Some(namespace), name: None, identity, names, args })
        }
        PG_TYPE => {
            let (name, namespace) = reg::type_name(oid, session)?;
            let identity = reg::type_text_qualified(oid, session)?;
            Some(Parts {
                schema: Some(namespace),
                name: Some(quote_identifier(name)),
                names: vec![identity.clone()],
                identity,
                args: Vec::new(),
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
            let (source, target) = (
                reg::type_text_qualified(source, session)?,
                reg::type_text_qualified(target, session)?,
            );
            Some(
                Parts::identity(format!("({source} AS {target})")).with(vec![source], vec![target]),
            )
        }
        PG_COLLATION => in_catalog(Named::Collation, oid, session),
        PG_CONSTRAINT => {
            let con = catalog?.constraint(oid)?;
            let (on, names, args) = if con.relation != 0 {
                let (on, mut names) = relation_parts(con.relation, session)?;
                names.push(con.name.clone());
                (on, names, Vec::new())
            } else {
                let on = reg::type_text_qualified(con.domain, session)?;
                (on.clone(), vec![on], vec![con.name.clone()])
            };
            Some(Parts {
                schema: Some(con.namespace),
                name: None,
                identity: format!("{} on {on}", quote_identifier(&con.name)),
                names,
                args,
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
            let (on, mut names) = relation_parts(default.relation, session)?;
            names.push(column.to_string());
            Some(
                Parts::identity(format!("for {on}.{}", quote_identifier(column)))
                    .with(names, Vec::new()),
            )
        }
        PG_LANGUAGE => builtin::owned_by_oid(Owned::Language, oid)
            .map(|l| l.name)
            .or_else(|| catalog?.languages().find(|l| l.oid == oid).map(|l| l.name.as_str()))
            .map(Parts::named),
        PG_LARGEOBJECT => builtin::owned_by_oid(Owned::LargeObject, oid)
            .map(|_| Parts::identity(oid.to_string()).with(vec![oid.to_string()], Vec::new())),
        PG_OPERATOR => {
            let op = builtin::operator_by_oid(oid)?;
            let arg = |ty: u32| {
                if ty == 0 {
                    Some("NONE".to_string())
                } else {
                    reg::type_text_qualified(ty, session)
                }
            };
            let schema = reg::namespace_name(op.namespace, session)?;
            let (left, right) = (arg(op.left)?, arg(op.right)?);
            let identity = format!("{}.{}({left},{right})", quote_identifier(schema), op.name);
            let names = vec![schema.to_string(), op.name.to_string()];
            let args = [(op.left, left), (op.right, right)]
                .into_iter()
                .filter(|(ty, _)| *ty != 0)
                .map(|(_, t)| t)
                .collect();
            Some(Parts { schema: Some(op.namespace), name: None, identity, names, args })
        }
        PG_OPCLASS => method_parts(Named::Opclass, oid, session),
        PG_OPFAMILY => method_parts(Named::Opfamily, oid, session),
        PG_AM => builtin::access_method_name(oid).map(Parts::named),
        PG_AMOP => amop_parts(oid, session).and_then(|(family, left, right, n, _)| {
            let (left, right) = (
                reg::type_text_qualified(left, session)?,
                reg::type_text_qualified(right, session)?,
            );
            let family = method_parts(Named::Opfamily, family, session)?;
            let identity = format!("operator {n} ({left}, {right}) of {}", family.identity);
            let mut names = family.names;
            names.push(n.to_string());
            Some(Parts::identity(identity).with(names, vec![left, right]))
        }),
        PG_AMPROC => amproc_parts(oid, session).and_then(|(family, left, right, n, _)| {
            let (left, right) = (
                reg::type_text_qualified(left, session)?,
                reg::type_text_qualified(right, session)?,
            );
            let family = method_parts(Named::Opfamily, family, session)?;
            let identity = format!("function {n} ({left}, {right}) of {}", family.identity);
            let mut names = family.names;
            names.push(n.to_string());
            Some(Parts::identity(identity).with(names, vec![left, right]))
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
            Some(named_on(name, on, session)?)
        }
        rupg_catalog::PG_POLICY => {
            let policy = catalog?.policy(oid)?;
            named_on(&policy.name, policy.relation, session)
        }
        PG_TRIGGER => {
            let trigger = catalog?.trigger(oid)?;
            named_on(&trigger.name, trigger.relation, session)
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
        rupg_catalog::PG_DEFAULT_ACL => {
            let acl = catalog?.default_acl(oid)?;
            let role = rupg_pgcatalog::roles::current().name(acl.role)?.to_string();
            let mut identity = format!("for role {}", quote_identifier(&role));
            let mut names = vec![role];
            if acl.namespace != 0 {
                let schema = reg::namespace_name(acl.namespace, session)?;
                identity.push_str(&format!(" in schema {}", quote_identifier(schema)));
                names.push(schema.to_string());
            }
            let kind = match acl.objtype {
                b'r' => "tables",
                b'S' => "sequences",
                b'f' => "functions",
                b'T' => "types",
                b'n' => "schemas",
                _ => "large objects",
            };
            identity.push_str(&format!(" on {kind}"));
            Some(Parts::identity(identity).with(names, vec![char::from(acl.objtype).to_string()]))
        }
        _ => None,
    }
}

/// `getRelationIdentity`: the name of the relation with its schema, and the schema and the name as a list.
fn relation_parts(oid: u32, session: &dyn Session) -> Option<(String, Vec<String>)> {
    let (name, namespace) = reg::class_name(oid, session)?;
    let schema = reg::namespace_name(namespace, session)?;
    Some((reg::qualified(Some(schema), name), vec![schema.to_string(), name.to_string()]))
}

/// The parts of a rule, a trigger or a policy `name` on the relation `on`.
fn named_on(name: &str, on: u32, session: &dyn Session) -> Option<Parts> {
    let (on, mut names) = relation_parts(on, session)?;
    names.push(name.to_string());
    Some(Parts::identity(format!("{} on {on}", quote_identifier(name))).with(names, Vec::new()))
}

/// `format_procedure_qualified` and `format_procedure_parts`: the name of the function with its schema and the types of its arguments with their schemas, and the schema, the name and the types as lists.
fn procedure(oid: u32, session: &dyn Session) -> Option<(String, Vec<String>, Vec<String>)> {
    let (name, namespace, args) = match builtin::proc_by_oid(oid) {
        Some(proc) => (proc.name, proc.namespace, proc.argtypes),
        None => {
            let function = session.catalog()?.function(oid)?;
            (function.name.as_str(), function.namespace, function.argtypes.as_slice())
        }
    };
    let args: Vec<String> =
        args.iter().map(|&ty| reg::type_text_qualified(ty, session)).collect::<Option<_>>()?;
    let schema = reg::namespace_name(namespace, session)?;
    let qualified = reg::qualified(Some(schema), name);
    Some((
        format!("{qualified}({})", args.join(",")),
        vec![schema.to_string(), name.to_string()],
        args,
    ))
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
    parts.names.insert(0, method.to_string());
    Some(parts)
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_identify_object" => identify_object,
        "pg_identify_object_as_address" => identify_object_as_address,
        _ => return None,
    };
    Some(kernel)
}

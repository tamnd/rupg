//! `pg_get_object_address`: the catalog, the OID and the number of the column of an object that a kind and two lists of names give, as `pg_get_object_address` of `objectaddress.c` finds them.
//!
//! The kernel turns the lists into the nodes that `get_object_address` takes and checks their lengths. The analyzer looks up the objects in the schemas, and the kernel looks up the databases, the tablespaces, the roles and the other objects that have one name.

use rupg_catalog::{Catalog, ObjRef, PG_PUBLICATION};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned};
use rupg_sql::nodes::{List, Node, ObjectType, ObjectWithArgs};
use rupg_types::{Record, Value, oid};

use crate::acl::{current_user_oid, simple_name};
use crate::describe::{
    PG_AM, PG_AUTHID, PG_DATABASE, PG_EXTENSION, PG_LANGUAGE, PG_NAMESPACE, PG_TABLESPACE,
};
use crate::reg::SessionEnv;
use crate::{Call, Kernel, Session, bad_value, type_error};

/// `ObjectTypeMap`: the names of the kinds of objects, with `None` for a kind that has no address.
const OBJECT_TYPES: &[(&str, Option<ObjectType>)] = &[
    ("table", Some(ObjectType::OBJECT_TABLE)),
    ("index", Some(ObjectType::OBJECT_INDEX)),
    ("sequence", Some(ObjectType::OBJECT_SEQUENCE)),
    ("toast table", None),
    ("view", Some(ObjectType::OBJECT_VIEW)),
    ("materialized view", Some(ObjectType::OBJECT_MATVIEW)),
    ("composite type", None),
    ("foreign table", Some(ObjectType::OBJECT_FOREIGN_TABLE)),
    ("table column", Some(ObjectType::OBJECT_COLUMN)),
    ("index column", None),
    ("sequence column", None),
    ("toast table column", None),
    ("view column", None),
    ("materialized view column", None),
    ("composite type column", None),
    ("foreign table column", Some(ObjectType::OBJECT_COLUMN)),
    ("aggregate", Some(ObjectType::OBJECT_AGGREGATE)),
    ("function", Some(ObjectType::OBJECT_FUNCTION)),
    ("procedure", Some(ObjectType::OBJECT_PROCEDURE)),
    ("type", Some(ObjectType::OBJECT_TYPE)),
    ("cast", Some(ObjectType::OBJECT_CAST)),
    ("collation", Some(ObjectType::OBJECT_COLLATION)),
    ("table constraint", Some(ObjectType::OBJECT_TABCONSTRAINT)),
    ("domain constraint", Some(ObjectType::OBJECT_DOMCONSTRAINT)),
    ("conversion", Some(ObjectType::OBJECT_CONVERSION)),
    ("default value", Some(ObjectType::OBJECT_DEFAULT)),
    ("language", Some(ObjectType::OBJECT_LANGUAGE)),
    ("large object", Some(ObjectType::OBJECT_LARGEOBJECT)),
    ("operator", Some(ObjectType::OBJECT_OPERATOR)),
    ("operator class", Some(ObjectType::OBJECT_OPCLASS)),
    ("operator family", Some(ObjectType::OBJECT_OPFAMILY)),
    ("access method", Some(ObjectType::OBJECT_ACCESS_METHOD)),
    ("operator of access method", Some(ObjectType::OBJECT_AMOP)),
    ("function of access method", Some(ObjectType::OBJECT_AMPROC)),
    ("rule", Some(ObjectType::OBJECT_RULE)),
    ("trigger", Some(ObjectType::OBJECT_TRIGGER)),
    ("schema", Some(ObjectType::OBJECT_SCHEMA)),
    ("text search parser", Some(ObjectType::OBJECT_TSPARSER)),
    ("text search dictionary", Some(ObjectType::OBJECT_TSDICTIONARY)),
    ("text search template", Some(ObjectType::OBJECT_TSTEMPLATE)),
    ("text search configuration", Some(ObjectType::OBJECT_TSCONFIGURATION)),
    ("role", Some(ObjectType::OBJECT_ROLE)),
    ("role membership", None),
    ("database", Some(ObjectType::OBJECT_DATABASE)),
    ("tablespace", Some(ObjectType::OBJECT_TABLESPACE)),
    ("foreign-data wrapper", Some(ObjectType::OBJECT_FDW)),
    ("server", Some(ObjectType::OBJECT_FOREIGN_SERVER)),
    ("user mapping", Some(ObjectType::OBJECT_USER_MAPPING)),
    ("default acl", Some(ObjectType::OBJECT_DEFACL)),
    ("extension", Some(ObjectType::OBJECT_EXTENSION)),
    ("event trigger", Some(ObjectType::OBJECT_EVENT_TRIGGER)),
    ("policy", Some(ObjectType::OBJECT_POLICY)),
    ("publication", Some(ObjectType::OBJECT_PUBLICATION)),
    ("publication namespace", Some(ObjectType::OBJECT_PUBLICATION_NAMESPACE)),
    ("publication relation", Some(ObjectType::OBJECT_PUBLICATION_REL)),
    ("publication excluded relation", Some(ObjectType::OBJECT_PUBLICATION_EXCLUDED_REL)),
    ("subscription", Some(ObjectType::OBJECT_SUBSCRIPTION)),
    ("transform", Some(ObjectType::OBJECT_TRANSFORM)),
    ("statistics object", Some(ObjectType::OBJECT_STATISTIC_EXT)),
];

/// An error of `ERRCODE_INVALID_PARAMETER_VALUE`.
fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// The error for a list of the wrong length.
fn length(what: &str, relation: &str, n: usize) -> Error {
    invalid(format!("{what} list length must be {relation} {n}"))
}

/// The error for a null in a list.
fn nulls() -> Error {
    invalid("name or argument lists may not contain nulls")
}

/// The elements of a `text[]`, with `None` for a null.
fn texts(value: &Value) -> Result<Vec<Option<&str>>> {
    let Value::Array(array) = value else { return Err(bad_value()) };
    array
        .values
        .iter()
        .map(|v| v.as_ref().map(|v| v.as_str().ok_or_else(bad_value)).transpose())
        .collect()
}

/// `textarray_to_strvaluelist`: the strings of the elements, which must not be null.
fn strings(list: &[Option<&str>]) -> Result<Vec<String>> {
    list.iter().map(|s| s.map(str::to_string).ok_or_else(nulls)).collect()
}

/// A list node of strings.
fn string_list(list: &[String]) -> Node {
    Node::List(list.iter().map(|s| Some(Node::String(s.as_str().into()))).collect())
}

/// `typeStringToTypeName`.
fn type_name(text: &str) -> Result<Node> {
    let name = rupg_sql::parse_type_name(text).map_err(Error::from)?;
    Ok(Node::TypeName(Box::new(name)))
}

/// `pg_get_object_address(text, text[], text[])`.
fn get_object_address(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [kind, names, objargs] = args else { return Err(bad_value()) };
    let kind = kind.as_str().ok_or_else(bad_value)?;
    let objtype = match OBJECT_TYPES.iter().find(|(name, _)| *name == kind) {
        Some((_, Some(objtype))) => *objtype,
        Some((_, None)) => return Err(invalid(format!("unsupported object type \"{kind}\""))),
        None => return Err(invalid(format!("unrecognized object type \"{kind}\""))),
    };
    let (names, objargs) = (texts(names)?, texts(objargs)?);
    let mut name = Vec::new();
    let mut typename = None;
    let mut large_object = None;
    match objtype {
        ObjectType::OBJECT_TYPE
        | ObjectType::OBJECT_DOMAIN
        | ObjectType::OBJECT_CAST
        | ObjectType::OBJECT_TRANSFORM
        | ObjectType::OBJECT_DOMCONSTRAINT => {
            let [element] = names.as_slice() else { return Err(length("name", "exactly", 1)) };
            typename = Some(type_name(element.ok_or_else(nulls)?)?);
        }
        ObjectType::OBJECT_LARGEOBJECT => {
            let [element] = names.as_slice() else { return Err(length("name", "exactly", 1)) };
            large_object =
                Some(element.ok_or_else(|| invalid("large object OID may not be null"))?);
        }
        _ => {
            name = strings(&names)?;
            if name.is_empty() {
                return Err(length("name", "at least", 1));
            }
        }
    }
    let typed = matches!(
        objtype,
        ObjectType::OBJECT_AGGREGATE
            | ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_OPERATOR
            | ObjectType::OBJECT_CAST
            | ObjectType::OBJECT_AMOP
            | ObjectType::OBJECT_AMPROC
    );
    let args: List = if typed {
        let texts = strings(&objargs)?;
        texts.iter().map(|t| type_name(t).map(Some)).collect::<Result<_>>()?
    } else {
        strings(&objargs)?.iter().map(|s| Some(Node::String(s.as_str().into()))).collect()
    };
    match objtype {
        ObjectType::OBJECT_PUBLICATION_NAMESPACE | ObjectType::OBJECT_USER_MAPPING => {
            if name.len() != 1 {
                return Err(length("name", "exactly", 1));
            }
            if args.len() != 1 {
                return Err(length("argument", "exactly", 1));
            }
        }
        ObjectType::OBJECT_DOMCONSTRAINT
        | ObjectType::OBJECT_CAST
        | ObjectType::OBJECT_PUBLICATION_EXCLUDED_REL
        | ObjectType::OBJECT_PUBLICATION_REL
        | ObjectType::OBJECT_DEFACL
        | ObjectType::OBJECT_TRANSFORM => {
            if args.len() != 1 {
                return Err(length("argument", "exactly", 1));
            }
        }
        ObjectType::OBJECT_OPFAMILY | ObjectType::OBJECT_OPCLASS if name.len() < 2 => {
            return Err(length("name", "at least", 2));
        }
        ObjectType::OBJECT_AMOP | ObjectType::OBJECT_AMPROC | ObjectType::OBJECT_OPERATOR => {
            if objtype != ObjectType::OBJECT_OPERATOR && name.len() < 3 {
                return Err(length("name", "at least", 3));
            }
            if args.len() != 2 {
                return Err(length("argument", "exactly", 2));
            }
        }
        _ => {}
    }
    let first = |list: &List| list.first().cloned().flatten().unwrap_or(Node::List(Vec::new()));
    let object = match objtype {
        ObjectType::OBJECT_ACCESS_METHOD
        | ObjectType::OBJECT_DATABASE
        | ObjectType::OBJECT_EVENT_TRIGGER
        | ObjectType::OBJECT_EXTENSION
        | ObjectType::OBJECT_FDW
        | ObjectType::OBJECT_FOREIGN_SERVER
        | ObjectType::OBJECT_LANGUAGE
        | ObjectType::OBJECT_PUBLICATION
        | ObjectType::OBJECT_ROLE
        | ObjectType::OBJECT_SCHEMA
        | ObjectType::OBJECT_SUBSCRIPTION
        | ObjectType::OBJECT_TABLESPACE => {
            let [one] = name.as_slice() else { return Err(length("name", "exactly", 1)) };
            return address(unqualified(objtype, one, call.session)?);
        }
        ObjectType::OBJECT_LARGEOBJECT => {
            let text = large_object.unwrap_or_default();
            let oid = rupg_types::oid_in(text).map_err(type_error)?;
            // rupg has no large objects.
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("large object {oid} does not exist"),
            ));
        }
        ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN => typename.ok_or_else(bad_value)?,
        ObjectType::OBJECT_CAST
        | ObjectType::OBJECT_DOMCONSTRAINT
        | ObjectType::OBJECT_TRANSFORM => Node::List(vec![typename, Some(first(&args))]),
        ObjectType::OBJECT_PUBLICATION_EXCLUDED_REL | ObjectType::OBJECT_PUBLICATION_REL => {
            Node::List(vec![Some(string_list(&name)), Some(first(&args))])
        }
        ObjectType::OBJECT_PUBLICATION_NAMESPACE | ObjectType::OBJECT_USER_MAPPING => {
            Node::List(vec![Some(Node::String(name[0].as_str().into())), Some(first(&args))])
        }
        ObjectType::OBJECT_DEFACL => {
            let mut list = vec![Some(first(&args))];
            list.extend(name.iter().map(|s| Some(Node::String(s.as_str().into()))));
            Node::List(list)
        }
        ObjectType::OBJECT_AMOP | ObjectType::OBJECT_AMPROC => {
            Node::List(vec![Some(string_list(&name)), Some(Node::List(args))])
        }
        ObjectType::OBJECT_FUNCTION
        | ObjectType::OBJECT_PROCEDURE
        | ObjectType::OBJECT_ROUTINE
        | ObjectType::OBJECT_AGGREGATE
        | ObjectType::OBJECT_OPERATOR => {
            let Node::List(objname) = string_list(&name) else { return Err(bad_value()) };
            Node::ObjectWithArgs(Box::new(ObjectWithArgs {
                objname,
                objargs: args,
                ..ObjectWithArgs::default()
            }))
        }
        _ => string_list(&name),
    };
    let empty = Catalog::default();
    let catalog = call.session.catalog().unwrap_or(&empty);
    let user = current_user_oid(call.session);
    address(rupg_analyze::object_address(
        objtype,
        &object,
        &SessionEnv(call.session),
        catalog,
        user,
    )?)
}

/// The record of an address.
fn address(object: ObjRef) -> Result<Value> {
    let values = vec![Value::Oid(object.class), Value::Oid(object.oid), Value::Int4(object.sub)];
    Ok(Value::Record(Box::new(Record::new(vec![oid::OID, oid::OID, oid::INT4], values))))
}

/// `get_object_address_unqualified`: an object whose name has no schema.
fn unqualified(objtype: ObjectType, name: &str, session: &dyn Session) -> Result<ObjRef> {
    let missing = |what: &str| {
        Error::new(SqlState::UNDEFINED_OBJECT, format!("{what} \"{name}\" does not exist"))
    };
    let (class, oid) = match objtype {
        ObjectType::OBJECT_DATABASE => (PG_DATABASE, simple_name(Owned::Database, name, session)?),
        ObjectType::OBJECT_TABLESPACE => {
            (PG_TABLESPACE, simple_name(Owned::Tablespace, name, session)?)
        }
        ObjectType::OBJECT_SCHEMA => (PG_NAMESPACE, simple_name(Owned::Namespace, name, session)?),
        ObjectType::OBJECT_FDW => (2328, simple_name(Owned::ForeignDataWrapper, name, session)?),
        ObjectType::OBJECT_FOREIGN_SERVER => {
            (1417, simple_name(Owned::ForeignServer, name, session)?)
        }
        ObjectType::OBJECT_LANGUAGE => {
            let user = session.catalog().and_then(|c| c.languages().find(|l| l.name == name));
            match user {
                Some(language) => (PG_LANGUAGE, language.oid),
                None => (PG_LANGUAGE, simple_name(Owned::Language, name, session)?),
            }
        }
        ObjectType::OBJECT_ROLE => {
            let oid = rupg_pgcatalog::roles::current().oid(name).ok_or_else(|| missing("role"))?;
            (PG_AUTHID, oid)
        }
        ObjectType::OBJECT_EXTENSION => {
            let found = session.catalog().and_then(|c| c.extensions().find(|e| e.name == name));
            (PG_EXTENSION, found.map(|e| e.oid).ok_or_else(|| missing("extension"))?)
        }
        ObjectType::OBJECT_ACCESS_METHOD => {
            (PG_AM, builtin::access_method_oid(name).ok_or_else(|| missing("access method"))?)
        }
        ObjectType::OBJECT_EVENT_TRIGGER => return Err(missing("event trigger")),
        ObjectType::OBJECT_PUBLICATION => {
            let found = session.catalog().and_then(|c| c.publication_by_name(name));
            (PG_PUBLICATION, found.map(|p| p.oid).ok_or_else(|| missing("publication"))?)
        }
        _ => return Err(missing("subscription")),
    };
    Ok(ObjRef::new(class, oid))
}

/// The kernel of a built-in function of this module.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_get_object_address" => get_object_address,
        _ => return None,
    };
    Some(kernel)
}

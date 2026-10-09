//! The text input and output of the OID alias types, such as `regclass` and `regprocedure`, and the functions `to_regclass` and the others, which give a null for a name that does not exist.
//!
//! A port of `src/backend/utils/adt/regproc.c` and of the lookups of `namespace.c` that it calls. A schema, a relation or a type is a built-in object or an object of the catalog of the session. The other kinds have only the built-in objects.
//!
//! The input separates two kinds of errors, as the `escontext` of PostgreSQL does. A soft error, such as a name that does not exist, is an error of the input function and a null of `to_regclass`. A hard error, such as a syntax error in a type name, is an error of both.

use std::borrow::Cow;

use rupg_analyze::Env;
use rupg_catalog::Catalog;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, NamedRow, OperatorRow, ProcRow};
use rupg_types::{self as types, RegInput, RegKind, Value, oid, qualified_name_list};

use crate::text::quote_identifier;
use crate::{Call, Kernel, Session, bad_value, type_error};

/// The OID of `pg_catalog`.
const PG_CATALOG: u32 = 11;
/// `PG_UTF8`, the encoding of the database.
const UTF8: i32 = 6;
/// The name of the encoding of the database, as `GetDatabaseEncodingName` gives it.
const UTF8_NAME: &str = "UTF8";
/// `FUNC_MAX_ARGS`.
const FUNC_MAX_ARGS: usize = 100;

/// The result of the input: the outer error is a hard error, and the inner error is a soft error.
type Soft<T> = Result<Result<T>>;

/// The session as the analyzer reads it, for the type names in the input of `regtype` and `regprocedure` and for the query of a view.
pub(crate) struct SessionEnv<'a>(pub(crate) &'a dyn Session);

impl Env for SessionEnv<'_> {
    fn search_path(&self) -> Vec<u32> {
        self.0.schemas().iter().filter_map(|name| namespace_oid(name, self.0)).collect()
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        namespace_oid(name, self.0)
    }

    fn database(&self) -> String {
        self.0.database().to_string()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        crate::io::input(ty, text, typmod, self.0)
    }

    fn catalog(&self) -> Option<&Catalog> {
        self.0.catalog()
    }
}

/// The OID of the schema with this name.
fn namespace_oid(name: &str, session: &dyn Session) -> Option<u32> {
    builtin::named(Named::Namespace)
        .iter()
        .find(|row| row.name == name)
        .map(|row| row.oid)
        .or_else(|| session.catalog()?.schema_by_name(name).map(|s| s.oid))
}

/// The name of the schema with this OID.
pub(crate) fn namespace_name(oid: u32, session: &dyn Session) -> Option<&str> {
    builtin::named_by_oid(Named::Namespace, oid)
        .map(|row| row.name)
        .or_else(|| session.catalog()?.schema(oid).map(|s| s.name.as_str()))
}

/// The schemas that an unqualified name searches: `pg_catalog` first when `search_path` does not name it, then the schemas of `search_path` that exist.
pub(crate) fn path(session: &dyn Session) -> Vec<u32> {
    let mut path: Vec<u32> =
        session.schemas().iter().filter_map(|n| namespace_oid(n, session)).collect();
    if !path.contains(&PG_CATALOG) {
        path.insert(0, PG_CATALOG);
    }
    path
}

/// `quote_qualified_identifier`: the name with its schema, or the name alone when `schema` is `None`.
pub(crate) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{}.{}", quote_identifier(schema), quote_identifier(name)),
        None => quote_identifier(name),
    }
}

/// `NameListToString`: the names with a dot between them, without quotes.
fn name_list(names: &[Cow<'_, str>]) -> String {
    names.join(".")
}

/// The error of a soft error with this SQLSTATE and message.
fn soft<T>(state: SqlState, message: impl Into<String>) -> Soft<T> {
    Ok(Err(Error::new(state, message)))
}

/// `DeconstructQualifiedName`: the schema and the name of a list of names.
///
/// # Errors
///
/// `0A000` for a database that is not the current database, and `42601` for more than three names.
fn deconstruct<'n>(
    names: &'n [Cow<'_, str>],
    session: &dyn Session,
) -> Result<(Option<&'n str>, &'n str)> {
    match names {
        [name] => Ok((None, name)),
        [schema, name] => Ok((Some(schema), name)),
        [database, schema, name] => {
            if database != session.database() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cross-database references are not implemented: {}", name_list(names)),
                ));
            }
            Ok((Some(schema), name))
        }
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("improper qualified name (too many dotted names): {}", name_list(names)),
        )),
    }
}

/// The schemas that a name searches: the schema that the name gives, or none when it does not exist, as the lookups with `missing_ok` give. A name without a schema searches the path.
fn spaces(schema: Option<&str>, session: &dyn Session) -> Vec<u32> {
    match schema {
        Some(schema) => namespace_oid(schema, session).into_iter().collect(),
        None => path(session),
    }
}

/// The object of a kind with a name in the first schema of `spaces` that has one. An operator class or an operator family must have the access method `method`, which is 0 for the other kinds. A collation of the encoding of the database comes before a collation of any encoding in the same schema, as `lookup_collation` finds it.
fn find(kind: Named, spaces: &[u32], name: &str, method: u32) -> Option<&'static NamedRow> {
    let rows = builtin::named(kind);
    spaces.iter().find_map(|&ns| {
        let here = |encoding: i32| {
            rows.iter().find(|r| {
                r.namespace == ns && r.name == name && r.method == method && r.encoding == encoding
            })
        };
        if kind == Named::Collation { here(UTF8).or_else(|| here(-1)) } else { here(-1) }
    })
}

/// The checks of `namespace.c` for the kinds that have only built-in objects: true when the unqualified name finds the object.
pub(crate) fn visible(kind: Named, row: &NamedRow, session: &dyn Session) -> bool {
    find(kind, &path(session), row.name, row.method).is_some_and(|found| found.oid == row.oid)
}

/// The OID of the relation with the name in the first schema of `spaces` that has one.
fn find_class(spaces: &[u32], name: &str, session: &dyn Session) -> Option<u32> {
    spaces.iter().find_map(|&ns| {
        find(Named::Class, &[ns], name, 0)
            .map(|row| row.oid)
            .or_else(|| session.catalog()?.relation_by_name(ns, name).map(|r| r.oid))
    })
}

/// The OID of the type with the name in the first schema of `spaces` that has one.
fn find_type(spaces: &[u32], name: &str, session: &dyn Session) -> Option<u32> {
    spaces.iter().find_map(|&ns| {
        builtin::type_by_name(ns, name)
            .map(|row| row.oid)
            .or_else(|| session.catalog()?.type_by_name(ns, name).map(|t| t.oid))
    })
}

/// The name and the schema of the relation with this OID.
pub(crate) fn class_name(oid: u32, session: &dyn Session) -> Option<(&str, u32)> {
    builtin::named_by_oid(Named::Class, oid)
        .map(|row| (row.name, row.namespace))
        .or_else(|| session.catalog()?.relation(oid).map(|r| (r.name.as_str(), r.namespace)))
}

/// The name and the schema of the type with this OID.
pub(crate) fn type_name(oid: u32, session: &dyn Session) -> Option<(&str, u32)> {
    builtin::type_by_oid(oid)
        .map(|row| (row.name, row.namespace))
        .or_else(|| session.catalog()?.type_by_oid(oid).map(|t| (t.name.as_str(), t.namespace)))
}

/// `RelationIsVisible`, or `None` when no relation has the OID.
pub(crate) fn class_visible(oid: u32, session: &dyn Session) -> Option<bool> {
    let (name, _) = class_name(oid, session)?;
    Some(find_class(&path(session), name, session) == Some(oid))
}

/// `TypeIsVisible`, or `None` when no type has the OID.
pub(crate) fn type_visible(oid: u32, session: &dyn Session) -> Option<bool> {
    let (name, _) = type_name(oid, session)?;
    Some(find_type(&path(session), name, session) == Some(oid))
}

/// `FuncnameGetCandidates`: the functions with the name in the schemas, in the order of the schemas. A function with the same argument types as a function in a schema before it is hidden.
fn functions(spaces: &[u32], name: &str, nargs: Option<usize>) -> Vec<&'static ProcRow> {
    let mut found: Vec<&'static ProcRow> = Vec::new();
    for &ns in spaces {
        for proc in builtin::procs_named(name) {
            if proc.namespace == ns
                && nargs.is_none_or(|n| proc.argtypes.len() == n)
                && !found.iter().any(|f| f.argtypes == proc.argtypes)
            {
                found.push(proc);
            }
        }
    }
    found
}

/// `OpernameGetCandidates`: the operators with the name in the schemas, in the order of the schemas. An operator with the same argument types as an operator in a schema before it is hidden.
fn operators(spaces: &[u32], name: &str) -> Vec<&'static OperatorRow> {
    let mut found: Vec<&'static OperatorRow> = Vec::new();
    for &ns in spaces {
        for op in builtin::operators_named(name) {
            if op.namespace == ns && !found.iter().any(|f| (f.left, f.right) == (op.left, op.right))
            {
                found.push(op);
            }
        }
    }
    found
}

/// `FunctionIsVisible`.
pub(crate) fn function_visible(proc: &ProcRow, session: &dyn Session) -> bool {
    functions(&path(session), proc.name, Some(proc.argtypes.len()))
        .iter()
        .find(|f| f.argtypes == proc.argtypes)
        .is_some_and(|f| f.oid == proc.oid)
}

/// `OperatorIsVisible`.
pub(crate) fn operator_visible(op: &OperatorRow, session: &dyn Session) -> bool {
    operators(&path(session), op.name)
        .iter()
        .find(|f| (f.left, f.right) == (op.left, op.right))
        .is_some_and(|f| f.oid == op.oid)
}

/// `F_ARRAY_SUBSCRIPT_HANDLER`, the `typsubscript` of a true array type.
const ARRAY_SUBSCRIPT_HANDLER: u32 = 6179;
/// The bytes of the header of a `varlena` value, which a typmod of a string type counts.
const VARHDRSZ: i32 = 4;

/// `format_type_extended` with `FORMAT_TYPE_ALLOW_INVALID`: `-` for OID 0, or the name of a type, with its schema when the name alone does not find it, and with its typmod. A typmod of `None` is the form without `FORMAT_TYPE_TYPEMOD_GIVEN`.
///
/// # Errors
///
/// `0A000` for a typmod of `interval`, which the engine cannot show yet.
pub(crate) fn type_text(ty: u32, typmod: Option<i32>, session: &dyn Session) -> Result<String> {
    if ty == 0 {
        return Ok("-".to_string());
    }
    // A true array type shows the name of its element type with `[]`, and a plain array type such as `oidvector` shows its own name.
    let element = match builtin::type_by_oid(ty) {
        Some(row) if row.elem != 0 && row.subscript == ARRAY_SUBSCRIPT_HANDLER => {
            (row.storage != b'p').then_some(row.elem)
        }
        Some(_) => None,
        None => match session.catalog().and_then(|c| c.type_by_oid(ty)) {
            Some(user) => (user.element != 0).then_some(user.element),
            None => return Ok("???".to_string()),
        },
    };
    match element {
        Some(element) if type_name(element, session).is_none() => Ok("???[]".to_string()),
        Some(element) => Ok(format!("{}[]", base_text(element, typmod, session)?)),
        None => base_text(ty, typmod, session),
    }
}

/// The part of `format_type_extended` after the array test: the name of a type that is not a true array type.
fn base_text(ty: u32, typmod: Option<i32>, session: &dyn Session) -> Result<String> {
    let given = typmod.is_some();
    let with = typmod.filter(|m| *m >= 0);
    let special = match (ty, with) {
        (oid::BIT, Some(m)) => Some(format!("bit({m})")),
        (oid::BIT, None) if !given => Some("bit".to_string()),
        (oid::BOOL, _) => Some("boolean".to_string()),
        (oid::BPCHAR, Some(m)) => Some(format!("character{}", length(m))),
        (oid::BPCHAR, None) if !given => Some("character".to_string()),
        (oid::FLOAT4, _) => Some("real".to_string()),
        (oid::FLOAT8, _) => Some("double precision".to_string()),
        (oid::INT2, _) => Some("smallint".to_string()),
        (oid::INT4, _) => Some("integer".to_string()),
        (oid::INT8, _) => Some("bigint".to_string()),
        (oid::NUMERIC, Some(m)) if m >= VARHDRSZ => {
            let bits = m - VARHDRSZ;
            let scale = ((bits & 0x7ff) ^ 1024) - 1024;
            Some(format!("numeric({},{scale})", (bits >> 16) & 0xffff))
        }
        (oid::NUMERIC, _) => Some("numeric".to_string()),
        (oid::INTERVAL, Some(m)) => Some(format!("interval{}", interval_typmod(m)?)),
        (oid::INTERVAL, None) => Some("interval".to_string()),
        (oid::TIME, m) => Some(format!("time{} without time zone", precision(m))),
        (oid::TIMETZ, m) => Some(format!("time{} with time zone", precision(m))),
        (oid::TIMESTAMP, m) => Some(format!("timestamp{} without time zone", precision(m))),
        (oid::TIMESTAMPTZ, m) => Some(format!("timestamp{} with time zone", precision(m))),
        (oid::VARBIT, m) => Some(format!("bit varying{}", precision(m))),
        (oid::VARCHAR, m) => Some(format!("character varying{}", m.map_or(String::new(), length))),
        (oid::JSON, _) => Some("json".to_string()),
        _ => None,
    };
    if let Some(text) = special {
        return Ok(text);
    }
    let Some((name, namespace)) = type_name(ty, session) else { return Ok("???".to_string()) };
    let schema = if type_visible(ty, session) == Some(true) {
        None
    } else {
        namespace_name(namespace, session)
    };
    let text = qualified(schema, name);
    // The types with a `typmodout` function all have a special form, so the default form shows the typmod as a number.
    Ok(match with {
        Some(m) => format!("{text}({m})"),
        None => text,
    })
}

/// The typmod text of `anytime_typmodout` and `bittypmodout`.
fn precision(typmod: Option<i32>) -> String {
    typmod.map_or(String::new(), |m| format!("({m})"))
}

/// The typmod text of `intervaltypmodout`: the fields of the range, then the precision when the typmod has one.
fn interval_typmod(typmod: i32) -> Result<String> {
    const YEAR: i32 = 1 << 2;
    const MONTH: i32 = 1 << 1;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    let (precision, range) = types::typmod::interval_precision_range(typmod).unwrap_or_default();
    let fields = match range {
        YEAR => " year",
        MONTH => " month",
        DAY => " day",
        HOUR => " hour",
        MINUTE => " minute",
        SECOND => " second",
        r if r == YEAR | MONTH => " year to month",
        r if r == DAY | HOUR => " day to hour",
        r if r == DAY | HOUR | MINUTE => " day to minute",
        r if r == DAY | HOUR | MINUTE | SECOND => " day to second",
        r if r == HOUR | MINUTE => " hour to minute",
        r if r == HOUR | MINUTE | SECOND => " hour to second",
        r if r == MINUTE | SECOND => " minute to second",
        types::typmod::INTERVAL_FULL_RANGE => "",
        _ => {
            return Err(Error::new(
                SqlState::INTERNAL_ERROR,
                format!("invalid INTERVAL typmod: 0x{typmod:x}"),
            ));
        }
    };
    Ok(if precision == types::typmod::INTERVAL_FULL_PRECISION {
        fields.to_string()
    } else {
        format!("{fields}({precision})")
    })
}

/// The typmod text of `bpchartypmodout` and `varchartypmodout`.
fn length(typmod: i32) -> String {
    if typmod > VARHDRSZ { format!("({})", typmod - VARHDRSZ) } else { String::new() }
}

/// The text output of an OID alias type: the name of the object, with its schema when the name alone does not find it, the text of OID 0 for the kind, or the number for an OID that no object has.
pub(crate) fn output(kind: RegKind, oid: u32, session: &dyn Session, out: &mut Vec<u8>) {
    let text = if oid == 0 { None } else { name_of(kind, oid, session) };
    match text {
        Some(text) => out.extend_from_slice(text.as_bytes()),
        None => types::reg_out_oid(kind, oid, out),
    }
}

/// The text of the object with this OID, or `None` when no object has it.
fn name_of(kind: RegKind, oid: u32, session: &dyn Session) -> Option<String> {
    let named = |named: Named| {
        let row = builtin::named_by_oid(named, oid)?;
        let schema = if visible(named, row, session) {
            None
        } else {
            namespace_name(row.namespace, session)
        };
        Some(qualified(schema, row.name))
    };
    match kind {
        RegKind::Class => {
            let (name, namespace) = class_name(oid, session)?;
            let schema = if class_visible(oid, session)? {
                None
            } else {
                namespace_name(namespace, session)
            };
            Some(qualified(schema, name))
        }
        RegKind::Collation => named(Named::Collation),
        RegKind::Config => named(Named::Config),
        RegKind::Dictionary => named(Named::Dictionary),
        RegKind::Namespace => namespace_name(oid, session).map(quote_identifier),
        RegKind::Role => builtin::named_by_oid(Named::Role, oid).map(|r| quote_identifier(r.name)),
        RegKind::Database => {
            builtin::named_by_oid(Named::Database, oid).map(|r| quote_identifier(r.name))
        }
        RegKind::Type => {
            type_name(oid, session)?;
            Some(type_text(oid, None, session).ok()?)
        }
        RegKind::Proc => {
            let proc = builtin::proc_by_oid(oid)?;
            let only =
                matches!(functions(&path(session), proc.name, None)[..], [f] if f.oid == oid);
            let schema = if only { None } else { namespace_name(proc.namespace, session) };
            Some(qualified(schema, proc.name))
        }
        RegKind::Procedure => {
            let proc = builtin::proc_by_oid(oid)?;
            let schema = if function_visible(proc, session) {
                None
            } else {
                namespace_name(proc.namespace, session)
            };
            let args: Vec<_> = proc.argtypes.iter().map(|&ty| types::format_type(ty)).collect();
            Some(format!("{}({})", qualified(schema, proc.name), args.join(",")))
        }
        RegKind::Oper => {
            let op = builtin::operator_by_oid(oid)?;
            let only = matches!(operators(&path(session), op.name)[..], [f] if f.oid == oid);
            match namespace_name(op.namespace, session) {
                Some(schema) if !only => Some(format!("{}.{}", quote_identifier(schema), op.name)),
                _ => Some(op.name.to_string()),
            }
        }
        RegKind::Operator => {
            let op = builtin::operator_by_oid(oid)?;
            let mut text = String::new();
            if !operator_visible(op, session)
                && let Some(schema) = namespace_name(op.namespace, session)
            {
                text.push_str(&quote_identifier(schema));
                text.push('.');
            }
            let arg =
                |ty: u32| if ty == 0 { Cow::Borrowed("NONE") } else { types::format_type(ty) };
            Some(format!("{text}{}({},{})", op.name, arg(op.left), arg(op.right)))
        }
    }
}

/// The text input of an OID alias type: a number, `-` for OID 0, or the name of an object.
///
/// # Errors
///
/// The outer error is an error for any caller, such as a name with too many dots or a syntax error in a type name. The inner error is an error of the input function that `to_regclass` and the other functions give as a null, such as a name that does not exist.
pub(crate) fn input(kind: RegKind, text: &str, session: &dyn Session) -> Soft<u32> {
    match types::reg_in(kind, text) {
        Ok(RegInput::Oid(oid)) => return Ok(Ok(oid)),
        Ok(RegInput::Name(_)) => {}
        Err(e) => return Ok(Err(type_error(e))),
    }
    match kind {
        RegKind::Type => {
            let env = SessionEnv(session);
            Ok(rupg_analyze::parse_type(text, &env)?.map(|(ty, _)| ty))
        }
        RegKind::Procedure => procedure_in(text, session),
        RegKind::Operator => operator_in(text, session),
        _ => {
            let names = match qualified_name_list(text) {
                Ok(names) => names,
                Err(e) => return Ok(Err(type_error(e))),
            };
            name_in(kind, text, &names, session)
        }
    }
}

/// The input of a kind whose name is a list of names.
fn name_in(kind: RegKind, text: &str, names: &[Cow<'_, str>], session: &dyn Session) -> Soft<u32> {
    let list = name_list(names);
    let simple = |named: &[(&str, u32)], state: SqlState, what: &str| match names {
        [name] => match named.iter().find(|n| n.0 == name) {
            Some(n) => Ok(Ok(n.1)),
            None => soft(state, format!("{what} \"{name}\" does not exist")),
        },
        _ => soft(SqlState::INVALID_NAME, "invalid name syntax"),
    };
    let rows = |named: Named| -> Vec<(&str, u32)> {
        builtin::named(named).iter().map(|r| (r.name, r.oid)).collect()
    };
    let mut schemas = rows(Named::Namespace);
    if let Some(catalog) = session.catalog() {
        schemas.extend(catalog.schemas().map(|s| (s.name.as_str(), s.oid)));
    }
    let in_schema = |named: Named, state: SqlState, message: String| {
        let (schema, name) = deconstruct(names, session)?;
        match find(named, &spaces(schema, session), name, 0) {
            Some(row) => Ok(Ok(row.oid)),
            None => soft(state, message),
        }
    };
    match kind {
        RegKind::Class => class_in(names, session, false),
        RegKind::Namespace => simple(&schemas, SqlState::UNDEFINED_SCHEMA, "schema"),
        RegKind::Role => simple(&rows(Named::Role), SqlState::UNDEFINED_OBJECT, "role"),
        RegKind::Database => simple(&rows(Named::Database), SqlState::UNDEFINED_OBJECT, "database"),
        RegKind::Collation => in_schema(
            Named::Collation,
            SqlState::UNDEFINED_OBJECT,
            format!("collation \"{list}\" for encoding \"{UTF8_NAME}\" does not exist"),
        ),
        RegKind::Config => in_schema(
            Named::Config,
            SqlState::UNDEFINED_OBJECT,
            format!("text search configuration \"{list}\" does not exist"),
        ),
        RegKind::Dictionary => in_schema(
            Named::Dictionary,
            SqlState::UNDEFINED_OBJECT,
            format!("text search dictionary \"{list}\" does not exist"),
        ),
        RegKind::Proc => {
            let (schema, name) = deconstruct(names, session)?;
            match functions(&spaces(schema, session), name, None)[..] {
                [] => soft(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function \"{text}\" does not exist"),
                ),
                [proc] => Ok(Ok(proc.oid)),
                _ => soft(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("more than one function named \"{text}\""),
                ),
            }
        }
        RegKind::Oper => {
            let (schema, name) = deconstruct(names, session)?;
            match operators(&spaces(schema, session), name)[..] {
                [] => {
                    soft(SqlState::UNDEFINED_FUNCTION, format!("operator does not exist: {text}"))
                }
                [op] => Ok(Ok(op.oid)),
                _ => soft(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("more than one operator named {text}"),
                ),
            }
        }
        RegKind::Type | RegKind::Procedure | RegKind::Operator => {
            Err(Error::internal("a type name read as a list of names"))
        }
    }
}

/// `makeRangeVarFromNameList` and `RangeVarGetRelid`: the relation of a list of names. With `strict`, as the cast from `text` gives, a relation or a schema that does not exist is a hard error with the message of `RangeVarGetRelid`.
fn class_in(names: &[Cow<'_, str>], session: &dyn Session, strict: bool) -> Soft<u32> {
    let (schema, name) = match names {
        [name] => (None, name),
        [schema, name] => (Some(schema), name),
        [database, schema, name] => {
            if database != session.database() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cross-database references are not implemented: \"{database}.{schema}.{name}\""
                    ),
                ));
            }
            (Some(schema), name)
        }
        _ => {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("improper relation name (too many dotted names): {}", name_list(names)),
            ));
        }
    };
    let spaces = match schema {
        Some(schema) => match namespace_oid(schema, session) {
            Some(ns) => vec![ns],
            None if strict => {
                return Err(Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                ));
            }
            None => Vec::new(),
        },
        None => path(session),
    };
    if let Some(oid) = find_class(&spaces, name, session) {
        return Ok(Ok(oid));
    }
    let message = if strict {
        match schema {
            Some(schema) => format!("relation \"{schema}.{name}\" does not exist"),
            None => format!("relation \"{name}\" does not exist"),
        }
    } else {
        format!("relation \"{}\" does not exist", name_list(names))
    };
    let error = Error::new(SqlState::UNDEFINED_TABLE, message);
    if strict { Err(error) } else { Ok(Err(error)) }
}

/// The bytes that `scanner_isspace` takes as white space.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The error `22P02` of `parseNameAndArgTypes`.
fn bad_syntax<T>(message: &str) -> Soft<T> {
    soft(SqlState::INVALID_TEXT_REPRESENTATION, message)
}

/// The name and the argument types of a function or an operator.
type NameAndArgs<'a> = (Vec<Cow<'a, str>>, Vec<u32>);

/// `parseNameAndArgTypes`: the name and the argument types of a string such as `abs(integer)`. With `allow_none`, `NONE` in any case is the missing argument of a prefix operator, which is 0.
fn name_and_args<'a>(
    text: &'a str,
    allow_none: bool,
    session: &dyn Session,
) -> Soft<NameAndArgs<'a>> {
    let b = text.as_bytes();
    let mut in_quote = false;
    let mut open = None;
    for (i, &c) in b.iter().enumerate() {
        if c == b'"' {
            in_quote = !in_quote;
        } else if c == b'(' && !in_quote {
            open = Some(i);
            break;
        }
    }
    let Some(open) = open else { return bad_syntax("expected a left parenthesis") };
    let names = match qualified_name_list(&text[..open]) {
        Ok(names) => names,
        Err(e) => return Ok(Err(type_error(e))),
    };
    let rest = &text[open + 1..];
    let rb = rest.as_bytes();
    // The backward scan of the C code stops at the first byte after the parenthesis, so that byte is the end when all the bytes after it are white space.
    let close = (1..rb.len()).rev().find(|&i| !is_space(rb[i])).unwrap_or(0);
    if rb.get(close) != Some(&b')') {
        return bad_syntax("expected a right parenthesis");
    }
    let args = &rest[..close];
    let ab = args.as_bytes();
    let env = SessionEnv(session);
    let mut types = Vec::new();
    let mut at = 0;
    let mut had_comma = false;
    loop {
        while at < ab.len() && is_space(ab[at]) {
            at += 1;
        }
        if at == ab.len() {
            if had_comma {
                return bad_syntax("expected a type name");
            }
            break;
        }
        let start = at;
        let mut in_quote = false;
        let mut depth = 0i32;
        while at < ab.len() {
            match ab[at] {
                b'"' => in_quote = !in_quote,
                b',' if !in_quote && depth == 0 => break,
                b'(' | b'[' if !in_quote => depth += 1,
                b')' | b']' if !in_quote => depth -= 1,
                _ => {}
            }
            at += 1;
        }
        if in_quote || depth != 0 {
            return bad_syntax("improper type name");
        }
        let name = args[start..at].trim_end_matches(|c: char| u8::try_from(c).is_ok_and(is_space));
        had_comma = at < ab.len();
        if had_comma {
            at += 1;
        }
        let ty = if allow_none && name.eq_ignore_ascii_case("none") {
            0
        } else {
            match rupg_analyze::parse_type(name, &env)? {
                Ok((ty, _)) => ty,
                Err(e) => return Ok(Err(e)),
            }
        };
        if types.len() >= FUNC_MAX_ARGS {
            return soft(SqlState::TOO_MANY_ARGUMENTS, "too many arguments");
        }
        types.push(ty);
    }
    Ok(Ok((names, types)))
}

/// `regprocedurein`.
fn procedure_in(text: &str, session: &dyn Session) -> Soft<u32> {
    let (names, args) = match name_and_args(text, false, session)? {
        Ok(found) => found,
        Err(e) => return Ok(Err(e)),
    };
    let (schema, name) = deconstruct(&names, session)?;
    match functions(&spaces(schema, session), name, Some(args.len()))
        .iter()
        .find(|f| f.argtypes == args.as_slice())
    {
        Some(proc) => Ok(Ok(proc.oid)),
        None => soft(SqlState::UNDEFINED_FUNCTION, format!("function \"{text}\" does not exist")),
    }
}

/// `regoperatorin`.
fn operator_in(text: &str, session: &dyn Session) -> Soft<u32> {
    let (names, args) = match name_and_args(text, true, session)? {
        Ok(found) => found,
        Err(e) => return Ok(Err(e)),
    };
    let [left, right] = args[..] else {
        let error = if args.len() == 1 {
            Error::new(SqlState::UNDEFINED_PARAMETER, "missing argument")
                .with_hint("Use NONE to denote the missing argument of a unary operator.")
        } else {
            Error::new(SqlState::TOO_MANY_ARGUMENTS, "too many arguments")
                .with_hint("Provide two argument types for operator.")
        };
        return Ok(Err(error));
    };
    let (schema, name) = deconstruct(&names, session)?;
    let found = spaces(schema, session).into_iter().find_map(|ns| {
        builtin::operators_named(name)
            .find(|op| op.namespace == ns && op.left == left && op.right == right)
    });
    match found {
        Some(op) => Ok(Ok(op.oid)),
        None => soft(SqlState::UNDEFINED_FUNCTION, format!("operator does not exist: {text}")),
    }
}

/// The text argument of a function.
pub(crate) fn text_arg(args: &[Value]) -> Result<&str> {
    args.first().and_then(Value::as_str).ok_or_else(bad_value)
}

/// `to_regclass` and the other functions: the input of the type, or null for a soft error.
fn to_reg(kind: RegKind, call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(input(kind, text_arg(args)?, call.session)?.map_or(Value::Null, Value::Oid))
}

/// `to_regtypemod`: the typmod of a type name, or null for a type that does not exist.
fn to_regtypemod(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let env = SessionEnv(call.session);
    let found = rupg_analyze::parse_type(text_arg(args)?, &env)?;
    Ok(found.map_or(Value::Null, |(_, typmod)| Value::Int4(typmod)))
}

/// The relation of a list of names, as `RangeVarGetRelid` finds it for `text_regclass`.
///
/// # Errors
///
/// `42P01` for a relation that does not exist, `3F000` for a schema that does not exist, and the errors of a list of names that is not correct.
pub(crate) fn relation_oid(names: &[Cow<'_, str>], session: &dyn Session) -> Result<u32> {
    class_in(names, session, true)?
}

/// `text_regclass`, the cast from `text` to `regclass`. A name that does not exist is an error, as `RangeVarGetRelid` gives it.
fn text_regclass(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let names = qualified_name_list(text_arg(args)?).map_err(type_error)?;
    Ok(Value::Oid(relation_oid(&names, call.session)?))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "to_regclass" => |c, a| to_reg(RegKind::Class, c, a),
        "to_regcollation" => |c, a| to_reg(RegKind::Collation, c, a),
        "to_regdatabase" => |c, a| to_reg(RegKind::Database, c, a),
        "to_regnamespace" => |c, a| to_reg(RegKind::Namespace, c, a),
        "to_regoper" => |c, a| to_reg(RegKind::Oper, c, a),
        "to_regoperator" => |c, a| to_reg(RegKind::Operator, c, a),
        "to_regproc" => |c, a| to_reg(RegKind::Proc, c, a),
        "to_regprocedure" => |c, a| to_reg(RegKind::Procedure, c, a),
        "to_regrole" => |c, a| to_reg(RegKind::Role, c, a),
        "to_regtype" => |c, a| to_reg(RegKind::Type, c, a),
        "to_regtypemod" => to_regtypemod,
        "text_regclass" => text_regclass,
        _ => return None,
    };
    Some(kernel)
}

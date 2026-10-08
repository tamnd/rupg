//! The functions that read the session: `version()`, `now()`, `current_user`, `current_setting` and others, and the kernels of the functions in SQL.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Value, format_type as type_name, oid};

use crate::math;
use crate::text::{any_concat, bit_length, lpad_space, quote_any, rpad_space};
use crate::{Call, Kernel, bad_value, not_yet};

/// The bytes of the header of a `varlena` value, which a typmod of a string type counts.
const VARHDRSZ: i32 = 4;

fn current_database(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text(call.session.database()))
}

fn current_user(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text(call.session.user()))
}

fn session_user(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text(call.session.session_user()))
}

/// `current_schema`: the first schema of the search path that exists, or null.
fn current_schema(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(call.session.schemas().into_iter().next().map_or(Value::Null, Value::Text))
}

/// `current_schemas(bool)`: the schemas of the search path that exist, with `pg_catalog` first when the argument is true and the path does not name it.
fn current_schemas(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let implicit = args.first().and_then(Value::as_bool).ok_or_else(bad_value)?;
    let mut schemas = call.session.schemas();
    if implicit && !schemas.iter().any(|s| s == "pg_catalog") {
        schemas.insert(0, "pg_catalog".to_string());
    }
    let values = schemas.into_iter().map(|s| Some(Value::Text(s))).collect();
    Ok(Value::Array(Box::new(Array::one(values))))
}

fn version(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Text(call.session.version()))
}

/// `now` and `transaction_timestamp`.
fn now(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::TimestampTz(call.session.transaction_start()))
}

fn statement_timestamp(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::TimestampTz(call.session.statement_start()))
}

fn clock_timestamp(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::TimestampTz(call.session.clock()))
}

/// `pg_typeof`: the type of the argument as the query gives it. It is not strict.
fn pg_typeof(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Oid(call.args.first().copied().ok_or_else(bad_value)?))
}

fn pg_backend_pid(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Int4(call.session.backend_pid()))
}

fn pg_client_encoding(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text("UTF8"))
}

/// The typmod text of `anytime_typmodout`.
fn precision(typmod: i32) -> String {
    format!("({typmod})")
}

/// `format_type_extended` with `FORMAT_TYPE_ALLOW_INVALID`: the name of a type with its typmod.
fn format_with_typmod(ty: u32, typmod: Option<i32>) -> Result<String> {
    let given = typmod.is_some();
    let Some(typmod) = typmod.filter(|m| *m >= 0) else {
        if ty == oid::BPCHAR && given {
            return Ok("bpchar".to_string());
        }
        return Ok(type_name(ty).into_owned());
    };
    Ok(match ty {
        oid::BPCHAR | oid::VARCHAR => {
            let length =
                if typmod > VARHDRSZ { format!("({})", typmod - VARHDRSZ) } else { String::new() };
            let base = if ty == oid::BPCHAR { "character" } else { "character varying" };
            format!("{base}{length}")
        }
        oid::NUMERIC => {
            if typmod >= VARHDRSZ {
                let bits = typmod - VARHDRSZ;
                let scale = ((bits & 0x7ff) ^ 1024) - 1024;
                format!("numeric({},{scale})", (bits >> 16) & 0xffff)
            } else {
                "numeric".to_string()
            }
        }
        oid::TIME => format!("time{} without time zone", precision(typmod)),
        oid::TIMETZ => format!("time{} with time zone", precision(typmod)),
        oid::TIMESTAMP => format!("timestamp{} without time zone", precision(typmod)),
        oid::TIMESTAMPTZ => format!("timestamp{} with time zone", precision(typmod)),
        oid::INTERVAL => return Err(not_yet("format_type of interval with a typmod")),
        _ => {
            let has_typmod =
                rupg_pgcatalog::builtin::type_by_oid(ty).is_some_and(|row| row.modin != 0);
            let name = type_name(ty).into_owned();
            if has_typmod {
                return Err(not_yet(format!("format_type of {name} with a typmod")));
            }
            name
        }
    })
}

/// `format_type(oid, int4)`. It is not strict: a null type gives null, and a null typmod gives the name without a typmod.
fn format_type(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ty = match args.first() {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Oid(ty)) => *ty,
        _ => return Err(bad_value()),
    };
    let typmod = match args.get(1) {
        Some(Value::Null) => None,
        Some(Value::Int4(m)) => Some(*m),
        _ => return Err(bad_value()),
    };
    Ok(Value::Text(format_with_typmod(ty, typmod)?))
}

/// `show_config_by_name` and `show_config_by_name_missing_ok`: `current_setting`.
fn current_setting(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    if let Some(value) = call.session.setting(name) {
        return Ok(Value::Text(value));
    }
    if args.get(1).and_then(Value::as_bool) == Some(true) {
        return Ok(Value::Null);
    }
    Err(Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    ))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "current_database" => current_database,
        "current_user" => current_user,
        "session_user" => session_user,
        "current_schema" => current_schema,
        "current_schemas" => current_schemas,
        "pgsql_version" => version,
        "now" => now,
        "statement_timestamp" => statement_timestamp,
        "clock_timestamp" => clock_timestamp,
        "pg_typeof" => pg_typeof,
        "pg_backend_pid" => pg_backend_pid,
        "pg_client_encoding" => pg_client_encoding,
        "format_type" => format_type,
        "show_config_by_name" | "show_config_by_name_missing_ok" => current_setting,
        _ => return None,
    })
}

/// The kernel of a function in SQL by its OID. The engine does not run the SQL text of these functions: each one has a kernel that gives the same result.
pub(crate) fn sql_function(func: u32) -> Option<Kernel> {
    Some(match func {
        879 => lpad_space,
        880 => rpad_space,
        1810 | 1811 => bit_length,
        2003 | 2004 => any_concat,
        1285 | 1290 => quote_any,
        _ => return math::numeric_sql(func),
    })
}

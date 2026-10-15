//! The functions that read the session: `version()`, `now()`, `current_user`, `current_setting` and others, and the kernels of the functions in SQL.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Value};

use crate::math;
use crate::text::{any_concat, bit_length, lpad_space, quote_any, rpad_space};
use crate::{Call, Kernel, bad_value};

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

/// `tid_block`: the block number of a `tid`.
fn tid_block(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int8(v)] = args else { return Err(bad_value()) };
    Ok(Value::Int8(i64::from(crate::io::tid_parts(*v)?.0)))
}

/// `tid_offset`: the offset of a `tid`.
fn tid_offset(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int8(v)] = args else { return Err(bad_value()) };
    Ok(Value::Int4(i32::from(crate::io::tid_parts(*v)?.1)))
}

/// `pg_typeof`: the type of the argument as the query gives it. It is not strict.
fn pg_typeof(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Oid(call.args.first().copied().ok_or_else(bad_value)?))
}

fn pg_backend_pid(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Int4(call.session.backend_pid()))
}

/// The longest wait of one call of [`crate::Session::sleep`], 10 minutes, as in `pg_sleep`.
const LONGEST_WAIT_US: i64 = 600_000_000;

/// `pg_sleep`: waits for the seconds, rounded up to a microsecond, or until a cancel. NaN and a value of zero or less give no wait. A wait longer than `i64::MAX / 2` microseconds stops there, as in PostgreSQL.
fn pg_sleep(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float8(secs)] = args else { return Err(bad_value()) };
    sleep(call, *secs)
}

/// `pg_sleep_until`: `pg_sleep(extract(epoch from $1) - extract(epoch from clock_timestamp()))`. `infinity` waits with no end, and `-infinity` gives no wait.
#[allow(clippy::cast_precision_loss)]
fn pg_sleep_until(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TimestampTz(at)] = args else { return Err(bad_value()) };
    let secs = match *at {
        i64::MAX => f64::INFINITY,
        i64::MIN => f64::NEG_INFINITY,
        at => at.saturating_sub(call.session.clock()) as f64 / 1e6,
    };
    sleep(call, secs)
}

/// The loop of `pg_sleep`, which looks for a cancel before each wait.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn sleep(call: &Call<'_>, secs: f64) -> Result<Value> {
    if secs.is_nan() || secs <= 0.0 {
        return Ok(Value::Text(String::new()));
    }
    let micros = (secs * 1e6).ceil().min((i64::MAX / 2) as f64) as i64;
    let end = call.session.clock().saturating_add(micros);
    loop {
        call.session.interrupted()?;
        let delay = end.saturating_sub(call.session.clock());
        if delay <= 0 {
            return Ok(Value::Text(String::new()));
        }
        call.session.sleep(delay.min(LONGEST_WAIT_US))?;
    }
}

fn pg_client_encoding(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text("UTF8"))
}

/// `getdatabaseencoding`: the encoding of the database, which is always UTF8.
fn getdatabaseencoding(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text("UTF8"))
}

/// `pg_is_in_recovery`: false, because a rupg server is never a standby in recovery.
fn pg_is_in_recovery(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Bool(false))
}

/// The recovery control functions `pg_is_wal_replay_paused`, `pg_get_wal_replay_pause_state`, `pg_wal_replay_pause`, `pg_wal_replay_resume` and `pg_promote`. A rupg server is never in recovery, so each one gives the error of PostgreSQL for a server that is not in recovery.
fn not_in_recovery(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "recovery is not in progress")
        .with_hint("Recovery control functions can only be executed during recovery."))
}

/// `format_type(oid, int4)`. It is not strict: a null type gives null, and a null typmod gives the name without a typmod.
fn format_type(call: &Call<'_>, args: &[Value]) -> Result<Value> {
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
    Ok(Value::Text(crate::reg::type_text(ty, typmod, call.session)?))
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

/// `pg_wchar_table[].maxmblen`: the longest character of each encoding in bytes, by the number of the encoding. The number 7, which was `MULE_INTERNAL`, has no encoding.
const MAX_CHAR_LENGTHS: [Option<i32>; 42] = [
    Some(1),
    Some(3),
    Some(3),
    Some(3),
    Some(4),
    Some(3),
    Some(4),
    None,
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(1),
    Some(2),
    Some(2),
    Some(2),
    Some(2),
    Some(4),
    Some(3),
    Some(2),
];

/// `pg_encoding_max_length_sql`: the longest character of the encoding in bytes, or null for a number that is not an encoding.
fn pg_encoding_max_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let encoding = args.first().and_then(Value::as_i64).ok_or_else(bad_value)?;
    let length =
        usize::try_from(encoding).ok().and_then(|e| MAX_CHAR_LENGTHS.get(e).copied().flatten());
    Ok(length.map_or(Value::Null, Value::Int4))
}

/// `set_config_by_name`: `set_config`, which is `SET` as a function. A null value resets the parameter, and a null `is_local` is false.
fn set_config(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = match args.first() {
        Some(Value::Null) => {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "SET requires parameter name",
            ));
        }
        Some(name) => name.as_str().ok_or_else(bad_value)?,
        None => return Err(bad_value()),
    };
    let value = match args.get(1) {
        Some(Value::Null) => None,
        Some(value) => Some(value.as_str().ok_or_else(bad_value)?),
        None => return Err(bad_value()),
    };
    let local = args.get(2).and_then(Value::as_bool).unwrap_or(false);
    Ok(Value::Text(call.session.set_setting(name, value, local)?))
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
        "tid_block" => tid_block,
        "tid_offset" => tid_offset,
        "pg_backend_pid" => pg_backend_pid,
        "pg_sleep" => pg_sleep,
        "pg_client_encoding" => pg_client_encoding,
        "getdatabaseencoding" => getdatabaseencoding,
        "pg_is_in_recovery" => pg_is_in_recovery,
        "pg_is_wal_replay_paused"
        | "pg_get_wal_replay_pause_state"
        | "pg_wal_replay_pause"
        | "pg_wal_replay_resume"
        | "pg_promote" => not_in_recovery,
        // A server that is not a standby has received and replayed no WAL from a primary.
        "pg_last_wal_receive_lsn" | "pg_last_wal_replay_lsn" | "pg_last_xact_replay_timestamp" => {
            |_, _| Ok(Value::Null)
        }
        "pg_encoding_max_length_sql" => pg_encoding_max_length,
        "format_type" => format_type,
        "show_config_by_name" | "show_config_by_name_missing_ok" => current_setting,
        "set_config_by_name" => set_config,
        _ => return None,
    })
}

/// The kernel of a function in SQL by its OID. The engine does not run the SQL text of these functions: each one has a kernel that gives the same result.
pub(crate) fn sql_function(func: u32) -> Option<Kernel> {
    Some(match func {
        879 => lpad_space,
        880 => rpad_space,
        1810 | 1811 => bit_length,
        3936 => pg_sleep_until,
        1812 => crate::bit::bit_length,
        1426 => crate::geo::path_contain_pt,
        2003 | 2004 => any_concat,
        1285 | 1290 => quote_any,
        2631 => crate::network::int8pl_inet,
        _ => return math::numeric_sql(func).or_else(|| crate::catinfo::sql_function(func)),
    })
}

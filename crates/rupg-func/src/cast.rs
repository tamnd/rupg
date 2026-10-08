//! The functions of the casts of `pg_cast` and of the typmod casts, such as `int48`, `numeric_int4`, `text_name` and `varchar`.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    self as types, DATE_INFINITY, DATE_NEGATIVE_INFINITY, Numeric, NumericSign,
    POSTGRES_EPOCH_JDATE, TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, USECS_PER_DAY,
    USECS_PER_SEC, Value, oid,
};

use crate::compare::float;
use crate::io::base_type;
use crate::math::{float_value, integer};
use crate::{Call, Kernel, Session, bad_value, not_yet, type_error};

/// `TIMESTAMP_END_JULIAN`: the first Julian day after the last timestamp.
const TIMESTAMP_END_JULIAN: i32 = 109_203_528;
/// `MIN_TIMESTAMP`.
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
/// `END_TIMESTAMP`.
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;
/// The seconds from 1970-01-01 to 2000-01-01.
const UNIX_TO_POSTGRES_SECS: i64 = 946_684_800;

/// The first argument.
fn first(args: &[Value]) -> Result<&Value> {
    args.first().ok_or_else(bad_value)
}

/// The string of the first argument.
fn string(args: &[Value]) -> Result<&str> {
    first(args)?.as_str().ok_or_else(bad_value)
}

/// The typmod argument of a typmod cast.
fn typmod(args: &[Value]) -> Result<i32> {
    match args.get(1) {
        Some(Value::Int4(v)) => Ok(*v),
        _ => Err(bad_value()),
    }
}

/// The flag of an explicit cast of a typmod cast.
fn explicit(args: &[Value]) -> Result<bool> {
    args.get(2).and_then(Value::as_bool).ok_or_else(bad_value)
}

/// `int48` and the other casts between the integer types.
fn int_to_int(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_i64().ok_or_else(bad_value)?;
    integer(call.ret, i128::from(v))
}

/// `i4tod` and the other casts from an integer to a float. The conversion rounds once.
#[allow(clippy::cast_precision_loss)]
fn int_to_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_i64().ok_or_else(bad_value)?;
    Ok(if base_type(call.ret) == oid::FLOAT4 {
        Value::Float4(v as f32)
    } else {
        Value::Float8(v as f64)
    })
}

/// `dtoi4` and the other casts from a float to an integer: `rint`, then the range check.
#[allow(clippy::cast_possible_truncation)]
fn float_to_int(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = float(first(args)?).ok_or_else(bad_value)?.round_ties_even();
    if x.is_nan() {
        return integer(call.ret, i128::MAX);
    }
    integer(call.ret, x as i128)
}

/// `ftod`.
fn float4_to_float8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(float(first(args)?).ok_or_else(bad_value)?))
}

/// `dtof`, with the checks of overflow and underflow.
#[allow(clippy::cast_possible_truncation)]
fn float8_to_float4(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = float(first(args)?).ok_or_else(bad_value)?;
    let r = x as f32;
    if r.is_infinite() && !x.is_infinite() {
        return Err(Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: overflow",
        ));
    }
    if r == 0.0 && x != 0.0 {
        return Err(Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow",
        ));
    }
    Ok(Value::Float4(r))
}

/// `int4_numeric` and the others.
fn int_to_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_i64().ok_or_else(bad_value)?;
    Ok(Value::Numeric(Numeric::from_integer(i128::from(v))))
}

/// `numeric_int4` and the others: round half away from zero, then the range check.
fn numeric_to_int(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Numeric(n) = first(args)? else { return Err(bad_value()) };
    let name = match base_type(call.ret) {
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        _ => "bigint",
    };
    let v = n.to_integer(name).map_err(type_error)?;
    integer(call.ret, v)
}

/// `numeric_float8` and `numeric_float4`: the input function of the float type on the text of the number.
fn numeric_to_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Numeric(n) = first(args)? else { return Err(bad_value()) };
    let special = match n.sign() {
        NumericSign::NaN => Some(f64::NAN),
        NumericSign::Infinity => Some(f64::INFINITY),
        NumericSign::NegativeInfinity => Some(f64::NEG_INFINITY),
        _ => None,
    };
    if let Some(v) = special {
        return Ok(float_value(call.ret, v));
    }
    let mut text = Vec::new();
    types::numeric_out(n, &mut text);
    let text = String::from_utf8(text).map_err(|_| bad_value())?;
    if base_type(call.ret) == oid::FLOAT4 {
        Ok(Value::Float4(types::float4_in(&text).map_err(type_error)?))
    } else {
        Ok(Value::Float8(types::float8_in(&text).map_err(type_error)?))
    }
}

/// `float8_numeric`.
fn float8_to_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Numeric(Numeric::from_f64(float(first(args)?).ok_or_else(bad_value)?)))
}

/// `float4_numeric`: the value printed with 6 significant digits, as `%.*g` with `FLT_DIG` gives it.
fn float4_to_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = float(first(args)?).ok_or_else(bad_value)?;
    if !v.is_finite() {
        return Ok(Value::Numeric(Numeric::from_f64(v)));
    }
    let text = format!("{v:.5e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
    types::numeric_in(&format!("{mantissa}e{exponent}"), -1).map(Value::Numeric).map_err(type_error)
}

/// `int4_bool`.
fn int_to_bool(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(first(args)?.as_i64().ok_or_else(bad_value)? != 0))
}

/// `bool_int4`.
fn bool_to_int(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(i32::from(first(args)?.as_bool().ok_or_else(bad_value)?)))
}

/// `booltext`.
fn bool_to_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_bool().ok_or_else(bad_value)?;
    Ok(Value::text(if v { "true" } else { "false" }))
}

/// `text_name`: the string cut to 63 bytes.
fn to_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(types::name_in(string(args)?)))
}

/// `name_text`, `name_bpchar` and the other casts that keep the string.
fn same_string(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(string(args)?))
}

/// `bpchar_name`: the string cut to 63 bytes, then without its trailing spaces.
fn bpchar_to_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(types::name_in(string(args)?).trim_end_matches(' ')))
}

/// `char_text`.
fn char_to_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Char(c) = first(args)? else { return Err(bad_value()) };
    let mut out = Vec::new();
    types::char_out(*c, &mut out);
    Ok(Value::Text(String::from_utf8(out).map_err(|_| bad_value())?))
}

/// `text_char`.
fn text_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Char(types::char_in(string(args)?)))
}

/// `i4tochar`.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn int_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_i64().ok_or_else(bad_value)?;
    if !(-128..=127).contains(&v) {
        return Err(Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "\"char\" out of range"));
    }
    Ok(Value::Char(v as i8 as u8))
}

/// `chartoi4`: the byte as a signed number.
#[allow(clippy::cast_possible_wrap)]
fn char_to_int(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Char(c) = first(args)? else { return Err(bad_value()) };
    Ok(Value::Int4(i32::from(*c as i8)))
}

/// `i8tooid`.
fn int8_to_oid(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = first(args)?.as_i64().ok_or_else(bad_value)?;
    u32::try_from(v)
        .map(Value::Oid)
        .map_err(|_| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "OID out of range"))
}

/// `oidtoi8`.
fn oid_to_int8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int8(i64::from(first(args)?.as_oid().ok_or_else(bad_value)?)))
}

/// `varchar(varchar, int4, bool)`: the length of the typmod.
fn varchar_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let s =
        types::varchar_coerce(string(args)?, typmod(args)?, explicit(args)?).map_err(type_error)?;
    Ok(Value::text(s))
}

/// `bpchar(bpchar, int4, bool)`: the length of the typmod, with spaces added.
fn bpchar_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let s =
        types::bpchar_coerce(string(args)?, typmod(args)?, explicit(args)?).map_err(type_error)?;
    Ok(Value::text(s))
}

/// `numeric(numeric, int4)`: the precision and the scale of the typmod.
fn numeric_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Numeric(n) = first(args)? else { return Err(bad_value()) };
    n.with_typmod(typmod(args)?).map(Value::Numeric).map_err(type_error)
}

/// `timestamp_scale` and `timestamptz_scale`.
fn timestamp_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let m = typmod(args)?;
    match first(args)? {
        Value::Timestamp(t) => {
            Ok(Value::Timestamp(types::adjust_timestamp(*t, m).map_err(type_error)?))
        }
        Value::TimestampTz(t) => {
            Ok(Value::TimestampTz(types::adjust_timestamp(*t, m).map_err(type_error)?))
        }
        _ => Err(bad_value()),
    }
}

/// `time_scale` and `timetz_scale`.
fn time_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let m = typmod(args)?;
    match first(args)? {
        Value::Time(t) => Ok(Value::Time(types::adjust_time(*t, m))),
        Value::TimeTz(t, z) => Ok(Value::TimeTz(types::adjust_time(*t, m), *z)),
        _ => Err(bad_value()),
    }
}

/// `interval_scale`.
fn interval_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Interval(iv) = first(args)? else { return Err(bad_value()) };
    types::adjust_interval(*iv, typmod(args)?).map(Value::Interval).map_err(type_error)
}

/// The offset east of UTC in seconds of the zone of the session at an instant in microseconds since 2000-01-01 UTC.
fn offset_at(session: &dyn Session, utc: i64) -> Result<i64> {
    let unix = utc.div_euclid(USECS_PER_SEC) + UNIX_TO_POSTGRES_SECS;
    Ok(i64::from(session.zone()?.at(unix).0))
}

/// The local time of the session for an instant.
pub(crate) fn local(session: &dyn Session, utc: i64) -> Result<i64> {
    let offset = offset_at(session, utc)?;
    utc.checked_add(offset * USECS_PER_SEC)
        .ok_or_else(|| Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "timestamp out of range"))
}

/// The local time of the session for an instant in microseconds since 2000-01-01 UTC, and the offset east of UTC in seconds of the zone at that instant.
///
/// # Errors
///
/// The error of [`Session::zone`], and `22008` when the local time is out of range.
#[allow(clippy::cast_possible_truncation)]
pub fn local_time(session: &dyn Session, utc: i64) -> Result<(i64, i32)> {
    Ok((local(session, utc)?, offset_at(session, utc)? as i32))
}

/// The instant of a local time of the session. Only a zone with one offset has it here.
fn instant(session: &dyn Session, local: i64) -> Result<i64> {
    let Some(offset) = session.zone()?.fixed_offset() else {
        return Err(not_yet("a time zone with daylight saving time"));
    };
    Ok(local - i64::from(offset) * USECS_PER_SEC)
}

/// `IS_VALID_TIMESTAMP`.
fn valid_timestamp(t: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&t)
}

fn timestamp_range() -> Error {
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "timestamp out of range")
}

/// `date2timestamp`: midnight of the date.
fn midnight(date: i32) -> Result<i64> {
    match date {
        DATE_NEGATIVE_INFINITY => Ok(TIMESTAMP_NEGATIVE_INFINITY),
        DATE_INFINITY => Ok(TIMESTAMP_INFINITY),
        d if d >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "date out of range for timestamp",
        )),
        d => Ok(i64::from(d) * USECS_PER_DAY),
    }
}

/// The date of a local time.
#[allow(clippy::cast_possible_truncation)]
fn date_of(local: i64) -> i32 {
    match local {
        TIMESTAMP_NEGATIVE_INFINITY => DATE_NEGATIVE_INFINITY,
        TIMESTAMP_INFINITY => DATE_INFINITY,
        t => t.div_euclid(USECS_PER_DAY) as i32,
    }
}

fn infinite(t: i64) -> bool {
    t == TIMESTAMP_NEGATIVE_INFINITY || t == TIMESTAMP_INFINITY
}

fn date_timestamp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Date(d) = first(args)? else { return Err(bad_value()) };
    Ok(Value::Timestamp(midnight(*d)?))
}

fn date_timestamptz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Date(d) = first(args)? else { return Err(bad_value()) };
    let t = midnight(*d)?;
    if infinite(t) {
        return Ok(Value::TimestampTz(t));
    }
    let utc = instant(call.session, t)?;
    if !valid_timestamp(utc) {
        return Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "date out of range for timestamp",
        ));
    }
    Ok(Value::TimestampTz(utc))
}

fn timestamp_date(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Timestamp(t) = first(args)? else { return Err(bad_value()) };
    Ok(Value::Date(date_of(*t)))
}

fn timestamptz_date(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::TimestampTz(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::Date(date_of(*t)));
    }
    Ok(Value::Date(date_of(local(call.session, *t)?)))
}

fn timestamp_timestamptz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Timestamp(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::TimestampTz(*t));
    }
    let utc = instant(call.session, *t)?;
    if !valid_timestamp(utc) {
        return Err(timestamp_range());
    }
    Ok(Value::TimestampTz(utc))
}

fn timestamptz_timestamp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::TimestampTz(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::Timestamp(*t));
    }
    let local = local(call.session, *t)?;
    if !valid_timestamp(local) {
        return Err(timestamp_range());
    }
    Ok(Value::Timestamp(local))
}

/// `timestamp_time`: null for an infinite timestamp.
fn timestamp_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Timestamp(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::Null);
    }
    Ok(Value::Time(t.rem_euclid(USECS_PER_DAY)))
}

/// `timestamptz_time`: null for an infinite timestamp.
fn timestamptz_time(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::TimestampTz(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::Null);
    }
    Ok(Value::Time(local(call.session, *t)?.rem_euclid(USECS_PER_DAY)))
}

/// `timestamptz_timetz`: null for an infinite timestamp.
#[allow(clippy::cast_possible_truncation)]
fn timestamptz_timetz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::TimestampTz(t) = first(args)? else { return Err(bad_value()) };
    if infinite(*t) {
        return Ok(Value::Null);
    }
    let offset = offset_at(call.session, *t)?;
    let local = local(call.session, *t)?;
    Ok(Value::TimeTz(local.rem_euclid(USECS_PER_DAY), -offset as i32))
}

/// `timetz_time`.
fn timetz_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::TimeTz(t, _) = first(args)? else { return Err(bad_value()) };
    Ok(Value::Time(*t))
}

/// The kernel of a cast function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "int48" | "int84" | "i2toi4" | "i4toi2" | "int28" | "int82" => int_to_int,
        "i2tod" | "i4tod" | "i8tod" | "i2tof" | "i4tof" | "i8tof" => int_to_float,
        "dtoi2" | "dtoi4" | "dtoi8" | "ftoi2" | "ftoi4" | "ftoi8" => float_to_int,
        "ftod" => float4_to_float8,
        "dtof" => float8_to_float4,
        "int2_numeric" | "int4_numeric" | "int8_numeric" => int_to_numeric,
        "numeric_int2" | "numeric_int4" | "numeric_int8" => numeric_to_int,
        "numeric_float4" | "numeric_float8" => numeric_to_float,
        "float8_numeric" => float8_to_numeric,
        "float4_numeric" => float4_to_numeric,
        "int4_bool" => int_to_bool,
        "bool_int4" => bool_to_int,
        "booltext" => bool_to_text,
        "text_name" => to_name,
        "name_text" | "name_bpchar" => same_string,
        "bpchar_name" => bpchar_to_name,
        "char_text" | "char_bpchar" => char_to_text,
        "text_char" => text_to_char,
        "i4tochar" => int_to_char,
        "chartoi4" => char_to_int,
        "i8tooid" => int8_to_oid,
        "oidtoi8" => oid_to_int8,
        "varchar" => varchar_typmod,
        "bpchar" => bpchar_typmod,
        "numeric" => numeric_typmod,
        "timestamp_scale" | "timestamptz_scale" => timestamp_typmod,
        "time_scale" | "timetz_scale" => time_typmod,
        "interval_scale" => interval_typmod,
        "date_timestamp" => date_timestamp,
        "date_timestamptz" => date_timestamptz,
        "timestamp_date" => timestamp_date,
        "timestamptz_date" => timestamptz_date,
        "timestamp_timestamptz" => timestamp_timestamptz,
        "timestamptz_timestamp" => timestamptz_timestamp,
        "timestamp_time" => timestamp_time,
        "timestamptz_time" => timestamptz_time,
        "timestamptz_timetz" => timestamptz_timetz,
        "timetz_time" => timetz_time,
        _ => return None,
    })
}

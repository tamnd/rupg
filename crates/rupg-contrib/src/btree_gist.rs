//! `contrib/btree_gist`: GiST operator classes for the types that B-tree orders. A port of the `btree_*.c` files.

use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel};
use rupg_types::{Interval, TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, Value};

use crate::support::index_support;

/// The C symbols of `btree_gist` and their kernels.
pub(crate) static SYMBOLS: &[(&str, Kernel)] = &[
    ("cash_dist", cash_dist),
    ("date_dist", date_dist),
    ("float4_dist", float4_dist),
    ("float8_dist", float8_dist),
    ("gbt_bit_compress", index_support),
    ("gbt_bit_consistent", index_support),
    ("gbt_bit_penalty", index_support),
    ("gbt_bit_picksplit", index_support),
    ("gbt_bit_same", index_support),
    ("gbt_bit_sortsupport", index_support),
    ("gbt_bit_union", index_support),
    ("gbt_bool_compress", index_support),
    ("gbt_bool_consistent", index_support),
    ("gbt_bool_fetch", index_support),
    ("gbt_bool_penalty", index_support),
    ("gbt_bool_picksplit", index_support),
    ("gbt_bool_same", index_support),
    ("gbt_bool_sortsupport", index_support),
    ("gbt_bool_union", index_support),
    ("gbt_bpchar_compress", index_support),
    ("gbt_bpchar_consistent", index_support),
    ("gbt_bpchar_sortsupport", index_support),
    ("gbt_bytea_compress", index_support),
    ("gbt_bytea_consistent", index_support),
    ("gbt_bytea_penalty", index_support),
    ("gbt_bytea_picksplit", index_support),
    ("gbt_bytea_same", index_support),
    ("gbt_bytea_sortsupport", index_support),
    ("gbt_bytea_union", index_support),
    ("gbt_cash_compress", index_support),
    ("gbt_cash_consistent", index_support),
    ("gbt_cash_distance", index_support),
    ("gbt_cash_fetch", index_support),
    ("gbt_cash_penalty", index_support),
    ("gbt_cash_picksplit", index_support),
    ("gbt_cash_same", index_support),
    ("gbt_cash_sortsupport", index_support),
    ("gbt_cash_union", index_support),
    ("gbt_date_compress", index_support),
    ("gbt_date_consistent", index_support),
    ("gbt_date_distance", index_support),
    ("gbt_date_fetch", index_support),
    ("gbt_date_penalty", index_support),
    ("gbt_date_picksplit", index_support),
    ("gbt_date_same", index_support),
    ("gbt_date_sortsupport", index_support),
    ("gbt_date_union", index_support),
    ("gbt_decompress", index_support),
    ("gbt_enum_compress", index_support),
    ("gbt_enum_consistent", index_support),
    ("gbt_enum_fetch", index_support),
    ("gbt_enum_penalty", index_support),
    ("gbt_enum_picksplit", index_support),
    ("gbt_enum_same", index_support),
    ("gbt_enum_sortsupport", index_support),
    ("gbt_enum_union", index_support),
    ("gbt_float4_compress", index_support),
    ("gbt_float4_consistent", index_support),
    ("gbt_float4_distance", index_support),
    ("gbt_float4_fetch", index_support),
    ("gbt_float4_penalty", index_support),
    ("gbt_float4_picksplit", index_support),
    ("gbt_float4_same", index_support),
    ("gbt_float4_sortsupport", index_support),
    ("gbt_float4_union", index_support),
    ("gbt_float8_compress", index_support),
    ("gbt_float8_consistent", index_support),
    ("gbt_float8_distance", index_support),
    ("gbt_float8_fetch", index_support),
    ("gbt_float8_penalty", index_support),
    ("gbt_float8_picksplit", index_support),
    ("gbt_float8_same", index_support),
    ("gbt_float8_sortsupport", index_support),
    ("gbt_float8_union", index_support),
    ("gbt_inet_compress", index_support),
    ("gbt_inet_consistent", index_support),
    ("gbt_inet_penalty", index_support),
    ("gbt_inet_picksplit", index_support),
    ("gbt_inet_same", index_support),
    ("gbt_inet_sortsupport", index_support),
    ("gbt_inet_union", index_support),
    ("gbt_int2_compress", index_support),
    ("gbt_int2_consistent", index_support),
    ("gbt_int2_distance", index_support),
    ("gbt_int2_fetch", index_support),
    ("gbt_int2_penalty", index_support),
    ("gbt_int2_picksplit", index_support),
    ("gbt_int2_same", index_support),
    ("gbt_int2_sortsupport", index_support),
    ("gbt_int2_union", index_support),
    ("gbt_int4_compress", index_support),
    ("gbt_int4_consistent", index_support),
    ("gbt_int4_distance", index_support),
    ("gbt_int4_fetch", index_support),
    ("gbt_int4_penalty", index_support),
    ("gbt_int4_picksplit", index_support),
    ("gbt_int4_same", index_support),
    ("gbt_int4_sortsupport", index_support),
    ("gbt_int4_union", index_support),
    ("gbt_int8_compress", index_support),
    ("gbt_int8_consistent", index_support),
    ("gbt_int8_distance", index_support),
    ("gbt_int8_fetch", index_support),
    ("gbt_int8_penalty", index_support),
    ("gbt_int8_picksplit", index_support),
    ("gbt_int8_same", index_support),
    ("gbt_int8_sortsupport", index_support),
    ("gbt_int8_union", index_support),
    ("gbt_intv_compress", index_support),
    ("gbt_intv_consistent", index_support),
    ("gbt_intv_decompress", index_support),
    ("gbt_intv_distance", index_support),
    ("gbt_intv_fetch", index_support),
    ("gbt_intv_penalty", index_support),
    ("gbt_intv_picksplit", index_support),
    ("gbt_intv_same", index_support),
    ("gbt_intv_sortsupport", index_support),
    ("gbt_intv_union", index_support),
    ("gbt_macad8_compress", index_support),
    ("gbt_macad8_consistent", index_support),
    ("gbt_macad8_fetch", index_support),
    ("gbt_macad8_penalty", index_support),
    ("gbt_macad8_picksplit", index_support),
    ("gbt_macad8_same", index_support),
    ("gbt_macad8_sortsupport", index_support),
    ("gbt_macad8_union", index_support),
    ("gbt_macad_compress", index_support),
    ("gbt_macad_consistent", index_support),
    ("gbt_macad_fetch", index_support),
    ("gbt_macad_penalty", index_support),
    ("gbt_macad_picksplit", index_support),
    ("gbt_macad_same", index_support),
    ("gbt_macad_union", index_support),
    ("gbt_macaddr_sortsupport", index_support),
    ("gbt_numeric_compress", index_support),
    ("gbt_numeric_consistent", index_support),
    ("gbt_numeric_penalty", index_support),
    ("gbt_numeric_picksplit", index_support),
    ("gbt_numeric_same", index_support),
    ("gbt_numeric_sortsupport", index_support),
    ("gbt_numeric_union", index_support),
    ("gbt_oid_compress", index_support),
    ("gbt_oid_consistent", index_support),
    ("gbt_oid_distance", index_support),
    ("gbt_oid_fetch", index_support),
    ("gbt_oid_penalty", index_support),
    ("gbt_oid_picksplit", index_support),
    ("gbt_oid_same", index_support),
    ("gbt_oid_sortsupport", index_support),
    ("gbt_oid_union", index_support),
    ("gbt_text_compress", index_support),
    ("gbt_text_consistent", index_support),
    ("gbt_text_penalty", index_support),
    ("gbt_text_picksplit", index_support),
    ("gbt_text_same", index_support),
    ("gbt_text_sortsupport", index_support),
    ("gbt_text_union", index_support),
    ("gbt_time_compress", index_support),
    ("gbt_time_consistent", index_support),
    ("gbt_time_distance", index_support),
    ("gbt_time_fetch", index_support),
    ("gbt_time_penalty", index_support),
    ("gbt_time_picksplit", index_support),
    ("gbt_time_same", index_support),
    ("gbt_time_sortsupport", index_support),
    ("gbt_time_union", index_support),
    ("gbt_timetz_compress", index_support),
    ("gbt_timetz_consistent", index_support),
    ("gbt_ts_compress", index_support),
    ("gbt_ts_consistent", index_support),
    ("gbt_ts_distance", index_support),
    ("gbt_ts_fetch", index_support),
    ("gbt_ts_penalty", index_support),
    ("gbt_ts_picksplit", index_support),
    ("gbt_ts_same", index_support),
    ("gbt_ts_sortsupport", index_support),
    ("gbt_ts_union", index_support),
    ("gbt_tstz_compress", index_support),
    ("gbt_tstz_consistent", index_support),
    ("gbt_tstz_distance", index_support),
    ("gbt_uuid_compress", index_support),
    ("gbt_uuid_consistent", index_support),
    ("gbt_uuid_fetch", index_support),
    ("gbt_uuid_penalty", index_support),
    ("gbt_uuid_picksplit", index_support),
    ("gbt_uuid_same", index_support),
    ("gbt_uuid_sortsupport", index_support),
    ("gbt_uuid_union", index_support),
    ("gbt_var_decompress", index_support),
    ("gbt_var_fetch", index_support),
    ("gbt_varbit_sortsupport", index_support),
    ("gbtreekey_in", gbtreekey_in),
    ("gbtreekey_out", gbtreekey_out),
    ("gist_translate_cmptype_btree", gist_translate_cmptype_btree),
    ("int2_dist", int2_dist),
    ("int4_dist", int4_dist),
    ("int8_dist", int8_dist),
    ("interval_dist", interval_dist),
    ("oid_dist", oid_dist),
    ("time_dist", time_dist),
    ("ts_dist", ts_dist),
    ("tstz_dist", tstz_dist),
];

/// `date_mi`.
const DATE_MI: u32 = 1140;
/// `time_mi_time`.
const TIME_MI_TIME: u32 = 1690;
/// `timestamp_mi`.
const TIMESTAMP_MI: u32 = 2031;
/// `interval_mi`.
const INTERVAL_MI: u32 = 1170;
/// `interval_lt`.
const INTERVAL_LT: u32 = 1164;
/// `interval_um`.
const INTERVAL_UM: u32 = 1168;

fn wrong_kind() -> Error {
    Error::internal("a function got a value of the wrong kind")
}

/// The result of the built-in function `func` for `args`.
fn builtin(func: u32, call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let kernel = rupg_func::kernel(func)
        .ok_or_else(|| Error::internal(format!("no kernel for the function {func}")))?;
    kernel(call, args)
}

fn out_of_range(what: &str) -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("{what} out of range"))
}

/// `int2_dist`: `|a - b|`, with an error when it is not a `smallint`.
fn int2_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int2(a), Value::Int2(b)] = args else { return Err(wrong_kind()) };
    match a.checked_sub(*b) {
        Some(r) if r != i16::MIN => Ok(Value::Int2(r.abs())),
        _ => Err(out_of_range("smallint")),
    }
}

/// `int4_dist`.
fn int4_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int4(a), Value::Int4(b)] = args else { return Err(wrong_kind()) };
    match a.checked_sub(*b) {
        Some(r) if r != i32::MIN => Ok(Value::Int4(r.abs())),
        _ => Err(out_of_range("integer")),
    }
}

/// `|a - b|` of two `bigint` values, or `None` when it is not one.
fn i64_dist(a: &Value, b: &Value) -> Result<Option<i64>> {
    let (Value::Int8(a), Value::Int8(b)) = (a, b) else { return Err(wrong_kind()) };
    Ok(a.checked_sub(*b).filter(|r| *r != i64::MIN).map(i64::abs))
}

/// `int8_dist`.
fn int8_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(wrong_kind()) };
    i64_dist(a, b)?.map(Value::Int8).ok_or_else(|| out_of_range("bigint"))
}

/// `cash_dist`: a `money` value is its cents as a `bigint`.
fn cash_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(wrong_kind()) };
    i64_dist(a, b)?.map(Value::Int8).ok_or_else(|| out_of_range("money"))
}

/// `float_overflow_error`.
fn float_overflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// The distance of `float4_dist` and `float8_dist`: `|a - b|`, with an error for an overflow, 0 for two NaN values and for two infinities of the same sign, and infinity for NaN and a number.
macro_rules! float_dist {
    ($a:expr, $b:expr, $ty:ty) => {{
        let (a, b): ($ty, $ty) = ($a, $b);
        let mut r = a - b;
        if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
            return Err(float_overflow());
        }
        if r.is_nan() {
            r = if a.is_nan() && b.is_nan() {
                0.0
            } else if a.is_nan() || b.is_nan() {
                <$ty>::INFINITY
            } else {
                0.0
            };
        }
        r.abs()
    }};
}

/// `float4_dist`.
fn float4_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float4(a), Value::Float4(b)] = args else { return Err(wrong_kind()) };
    Ok(Value::Float4(float_dist!(*a, *b, f32)))
}

/// `float8_dist`.
fn float8_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float8(a), Value::Float8(b)] = args else { return Err(wrong_kind()) };
    Ok(Value::Float8(float_dist!(*a, *b, f64)))
}

/// `oid_dist`.
fn oid_dist(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Oid(a), Value::Oid(b)] = args else { return Err(wrong_kind()) };
    Ok(Value::Oid(a.abs_diff(*b)))
}

/// `date_dist`: `|date_mi(a, b)|`.
fn date_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Value::Int4(days) = builtin(DATE_MI, call, args)? else { return Err(wrong_kind()) };
    Ok(Value::Int4(days.wrapping_abs()))
}

/// `abs_interval`: the interval, negated when it is less than zero.
fn abs_interval(call: &Call<'_>, interval: Value) -> Result<Value> {
    let zero = Value::Interval(Interval { time: 0, day: 0, month: 0 });
    let negative = builtin(INTERVAL_LT, call, &[interval.clone(), zero])?;
    if negative == Value::Bool(true) {
        builtin(INTERVAL_UM, call, &[interval])
    } else {
        Ok(interval)
    }
}

/// `time_dist`.
fn time_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let diff = builtin(TIME_MI_TIME, call, args)?;
    abs_interval(call, diff)
}

/// `ts_dist` and `tstz_dist`: the greatest interval when a timestamp is not finite, else `|timestamp_mi(a, b)|`.
fn timestamp_dist(call: &Call<'_>, a: i64, b: i64, args: &[Value]) -> Result<Value> {
    let infinite = |t: i64| t == TIMESTAMP_INFINITY || t == TIMESTAMP_NEGATIVE_INFINITY;
    if infinite(a) || infinite(b) {
        return Ok(Value::Interval(Interval { time: i64::MAX, day: i32::MAX, month: i32::MAX }));
    }
    let diff = builtin(TIMESTAMP_MI, call, args)?;
    abs_interval(call, diff)
}

/// `ts_dist`.
fn ts_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Timestamp(a), Value::Timestamp(b)] = args else { return Err(wrong_kind()) };
    timestamp_dist(call, *a, *b, args)
}

/// `tstz_dist`: `timestamp_mi` reads the two values as `timestamp` values.
fn tstz_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TimestampTz(a), Value::TimestampTz(b)] = args else { return Err(wrong_kind()) };
    timestamp_dist(call, *a, *b, &[Value::Timestamp(*a), Value::Timestamp(*b)])
}

/// `interval_dist`.
fn interval_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let diff = builtin(INTERVAL_MI, call, args)?;
    abs_interval(call, diff)
}

/// `gbtreekey_in`: the key types have no text form. The second argument of an input function is the type, else the result type is.
fn gbtreekey_in(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ty = match args.get(1) {
        Some(Value::Oid(ty)) => *ty,
        _ => call.ret,
    };
    let name = rupg_func::type_message_name(ty, call.session);
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("cannot accept a value of type {name}"),
    ))
}

/// `gbtreekey_out`: the function does not know the type of the key.
fn gbtreekey_out(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot display a value of type gbtreekey?"))
}

/// `gist_translate_cmptype_btree`: the B-tree strategy of a `CompareType`, or `InvalidStrategy`. `COMPARE_LT` to `COMPARE_GT` are 1 to 5, as `BTLessStrategyNumber` to `BTGreaterStrategyNumber` are.
fn gist_translate_cmptype_btree(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int4(cmptype)] = args else { return Err(wrong_kind()) };
    let strategy = match *cmptype {
        c @ 1..=5 => i16::try_from(c).unwrap_or(0),
        _ => 0,
    };
    Ok(Value::Int2(strategy))
}

#[cfg(test)]
mod tests {
    use rupg_types::{USECS_PER_DAY, oid};

    use super::*;
    use crate::pg_trgm::test_session::TestSession;

    #[test]
    fn numbers() {
        let s = TestSession::default();
        let run = |k: Kernel, a: Value, b: Value| s.call(k, &[], 0, &[a, b]);
        assert_eq!(run(int2_dist, Value::Int2(-3), Value::Int2(4)).unwrap(), Value::Int2(7));
        let err = run(int2_dist, Value::Int2(i16::MAX), Value::Int2(-1)).unwrap_err();
        assert_eq!(err.message(), "smallint out of range");
        assert!(run(int2_dist, Value::Int2(-1), Value::Int2(i16::MAX)).is_err());
        assert_eq!(run(int4_dist, Value::Int4(5), Value::Int4(9)).unwrap(), Value::Int4(4));
        assert_eq!(
            run(int4_dist, Value::Int4(i32::MIN), Value::Int4(0)).unwrap_err().message(),
            "integer out of range"
        );
        assert_eq!(
            run(int8_dist, Value::Int8(i64::MIN), Value::Int8(1)).unwrap_err().message(),
            "bigint out of range"
        );
        assert_eq!(run(cash_dist, Value::Int8(100), Value::Int8(250)).unwrap(), Value::Int8(150));
        assert_eq!(
            run(cash_dist, Value::Int8(i64::MAX), Value::Int8(-1)).unwrap_err().message(),
            "money out of range"
        );
        assert_eq!(run(oid_dist, Value::Oid(3), Value::Oid(10)).unwrap(), Value::Oid(7));
        let f8 = |a: f64, b: f64| run(float8_dist, Value::Float8(a), Value::Float8(b));
        assert_eq!(f8(1.5, -2.0).unwrap(), Value::Float8(3.5));
        assert_eq!(f8(f64::NAN, f64::NAN).unwrap(), Value::Float8(0.0));
        assert_eq!(f8(f64::NAN, 1.0).unwrap(), Value::Float8(f64::INFINITY));
        assert_eq!(f8(f64::INFINITY, f64::INFINITY).unwrap(), Value::Float8(0.0));
        assert_eq!(f8(f64::INFINITY, 1.0).unwrap(), Value::Float8(f64::INFINITY));
        assert_eq!(f8(f64::MAX, -f64::MAX).unwrap_err().message(), "value out of range: overflow");
        let f4 = run(float4_dist, Value::Float4(f32::MAX), Value::Float4(-f32::MAX));
        assert_eq!(f4.unwrap_err().message(), "value out of range: overflow");
        let cmptype = |c: i32| {
            s.call(gist_translate_cmptype_btree, &[oid::INT4], oid::INT2, &[Value::Int4(c)])
                .unwrap()
        };
        assert_eq!(cmptype(3), Value::Int2(3));
        assert_eq!(cmptype(6), Value::Int2(0));
    }

    #[test]
    fn datetimes() {
        let s = TestSession::default();
        let interval =
            |time: i64, day: i32, month: i32| Value::Interval(Interval { time, day, month });
        let dates = [oid::DATE, oid::DATE];
        assert_eq!(
            s.call(date_dist, &dates, oid::INT4, &[Value::Date(3), Value::Date(10)]).unwrap(),
            Value::Int4(7)
        );
        let times = [oid::TIME, oid::TIME];
        let dist =
            s.call(time_dist, &times, oid::INTERVAL, &[Value::Time(5), Value::Time(9)]).unwrap();
        assert_eq!(dist, interval(4, 0, 0));
        let ts = [oid::TIMESTAMP, oid::TIMESTAMP];
        let day = USECS_PER_DAY;
        let dist = s
            .call(
                ts_dist,
                &ts,
                oid::INTERVAL,
                &[Value::Timestamp(0), Value::Timestamp(2 * day + 7)],
            )
            .unwrap();
        assert_eq!(dist, interval(7, 2, 0));
        let dist = s.call(
            ts_dist,
            &ts,
            oid::INTERVAL,
            &[Value::Timestamp(TIMESTAMP_INFINITY), Value::Timestamp(0)],
        );
        assert_eq!(dist.unwrap(), interval(i64::MAX, i32::MAX, i32::MAX));
        let tstz = [oid::TIMESTAMPTZ, oid::TIMESTAMPTZ];
        let dist = s
            .call(
                tstz_dist,
                &tstz,
                oid::INTERVAL,
                &[Value::TimestampTz(day), Value::TimestampTz(0)],
            )
            .unwrap();
        assert_eq!(dist, interval(0, 1, 0));
        let intervals = [oid::INTERVAL, oid::INTERVAL];
        let dist = s
            .call(interval_dist, &intervals, oid::INTERVAL, &[interval(0, 1, 0), interval(0, 0, 1)])
            .unwrap();
        assert_eq!(dist, interval(0, -1, 1));
        let dist = s
            .call(interval_dist, &intervals, oid::INTERVAL, &[interval(0, 0, 1), interval(0, 1, 0)])
            .unwrap();
        assert_eq!(dist, interval(0, -1, 1));
        let err = s.call(gbtreekey_out, &[], oid::CSTRING, &[Value::Null]).unwrap_err();
        assert_eq!(err.message(), "cannot display a value of type gbtreekey?");
    }
}

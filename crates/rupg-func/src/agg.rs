//! The transition and final functions of the aggregates whose state is a value of SQL, from `int8.c`, `numeric.c`, `float.c`, `bool.c` and `misc.c`. The executor calls them for each row of a group, and a query can call them as functions too.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Numeric, Value};

use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a transition or final function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "int2_sum" | "int4_sum" => int_sum,
        "int8inc_any" | "int8inc_float8_float8" => count_any,
        "booland_statefunc" => bool_and,
        "boolor_statefunc" => bool_or,
        "int2_avg_accum" | "int4_avg_accum" => int_avg_accum,
        "int8_avg" => int8_avg,
        "float4_accum" | "float8_accum" => float_accum,
        "float8_avg" => float8_avg,
        "float8_var_pop" => float8_var_pop,
        "float8_var_samp" => float8_var_samp,
        "float8_stddev_pop" => float8_stddev_pop,
        "float8_stddev_samp" => float8_stddev_samp,
        "any_value_transfn" => any_value,
        _ => return None,
    })
}

/// `int2_sum` and `int4_sum`: the sum as `bigint`. The function is not strict. A null sum takes the first value that is not null, and a null value does not change the sum. The addition has no check of overflow, as in PostgreSQL.
fn int_sum(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [sum, value] = args else { return Err(bad_value()) };
    let value = match value {
        Value::Null => return Ok(sum.clone()),
        v => v.as_i64().ok_or_else(bad_value)?,
    };
    match sum {
        Value::Null => Ok(Value::Int8(value)),
        Value::Int8(sum) => Ok(Value::Int8(sum.wrapping_add(value))),
        _ => Err(bad_value()),
    }
}

/// `int8inc_any`: the count plus 1. The other arguments only count.
fn count_any(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Int8(count)) = args.first() else { return Err(bad_value()) };
    count
        .checked_add(1)
        .map(Value::Int8)
        .ok_or_else(|| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))
}

fn bool_and(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bool(a), Value::Bool(b)] = args else { return Err(bad_value()) };
    Ok(Value::Bool(*a && *b))
}

fn bool_or(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bool(a), Value::Bool(b)] = args else { return Err(bad_value()) };
    Ok(Value::Bool(*a || *b))
}

/// The values of an array with no nulls, as `check_float8_array` and the checks of `int8_avg` want it.
fn elements<const N: usize>(value: &Value) -> Result<[&Value; N]> {
    let Value::Array(array) = value else { return Err(bad_value()) };
    let values: Vec<&Value> =
        array.values.iter().map(Option::as_ref).collect::<Option<_>>().ok_or_else(bad_value)?;
    values.try_into().map_err(|_| bad_value())
}

/// A one-dimensional array of the values.
fn array(values: Vec<Value>) -> Value {
    Value::Array(Box::new(Array::one(values.into_iter().map(Some).collect())))
}

/// `int2_avg_accum` and `int4_avg_accum`: the state is the `bigint` array of the count and the sum. The addition has no check of overflow, as in PostgreSQL.
fn int_avg_accum(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state, value] = args else { return Err(bad_value()) };
    let [Value::Int8(count), Value::Int8(sum)] = elements(state)? else { return Err(bad_value()) };
    let value = value.as_i64().ok_or_else(bad_value)?;
    Ok(array(vec![Value::Int8(count.wrapping_add(1)), Value::Int8(sum.wrapping_add(value))]))
}

/// `int8_avg`: the sum divided by the count as `numeric`, or null for no values.
fn int8_avg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state] = args else { return Err(bad_value()) };
    let [Value::Int8(count), Value::Int8(sum)] = elements(state)? else { return Err(bad_value()) };
    if *count == 0 {
        return Ok(Value::Null);
    }
    let count = Numeric::from_integer(i128::from(*count));
    let sum = Numeric::from_integer(i128::from(*sum));
    sum.div(&count).map(Value::Numeric).map_err(type_error)
}

/// The values of the `float8` array of `N`, `Sx` and `Sxx`.
fn float_state(state: &Value) -> Result<[f64; 3]> {
    let [Value::Float8(n), Value::Float8(sx), Value::Float8(sxx)] = elements(state)? else {
        return Err(bad_value());
    };
    Ok([*n, *sx, *sxx])
}

/// `float4_accum` and `float8_accum`: adds a value to `N`, `Sx` and `Sxx` with the algorithm of Youngs and Cramer. Only finite values that give an infinite result are an overflow. An infinite value makes `Sxx` NaN.
fn float_accum(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state, value] = args else { return Err(bad_value()) };
    let [n0, sx0, mut sxx] = float_state(state)?;
    let value = match value {
        Value::Float4(v) => f64::from(*v),
        Value::Float8(v) => *v,
        _ => return Err(bad_value()),
    };
    let n = n0 + 1.0;
    let sx = sx0 + value;
    if n0 > 0.0 {
        let tmp = value * n - sx;
        sxx += tmp * tmp / (n * n0);
        if sx.is_infinite() || sxx.is_infinite() {
            if !sx0.is_infinite() && !value.is_infinite() {
                return Err(Error::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    "value out of range: overflow",
                ));
            }
            sxx = f64::NAN;
        }
    } else if value.is_nan() || value.is_infinite() {
        sxx = f64::NAN;
    }
    Ok(array(vec![Value::Float8(n), Value::Float8(sx), Value::Float8(sxx)]))
}

/// The result of a final function of `float.c` that is null when `N` is not more than `min`.
fn float_final(args: &[Value], min: f64, f: impl Fn(f64, f64, f64) -> f64) -> Result<Value> {
    let [state] = args else { return Err(bad_value()) };
    let [n, sx, sxx] = float_state(state)?;
    if n <= min {
        return Ok(Value::Null);
    }
    Ok(Value::Float8(f(n, sx, sxx)))
}

fn float8_avg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_final(args, 0.0, |n, sx, _| sx / n)
}

fn float8_var_pop(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_final(args, 0.0, |n, _, sxx| sxx / n)
}

fn float8_var_samp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_final(args, 1.0, |n, _, sxx| sxx / (n - 1.0))
}

fn float8_stddev_pop(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_final(args, 0.0, |n, _, sxx| (sxx / n).sqrt())
}

fn float8_stddev_samp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_final(args, 1.0, |n, _, sxx| (sxx / (n - 1.0)).sqrt())
}

/// `any_value_transfn`: keeps the state, which is the first value that is not null.
fn any_value(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    args.first().cloned().ok_or_else(bad_value)
}

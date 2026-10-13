//! The functions of `money` in `cash.c`: the arithmetic, `cash_words` and the casts to and from `numeric`, `integer` and `bigint`. A value is the `bigint` count of cents in [`Value::Int8`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{self as types, Numeric, Value};

use crate::compare::float;
use crate::math::{division_by_zero, overflow};
use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a function of `money` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "cash_pl" => cash_pl,
        "cash_mi" => cash_mi,
        "cash_mul_flt4" | "cash_mul_flt8" | "flt4_mul_cash" | "flt8_mul_cash" => cash_mul_float,
        "cash_div_flt4" | "cash_div_flt8" => cash_div_float,
        "cash_mul_int2" | "cash_mul_int4" | "cash_mul_int8" | "int2_mul_cash" | "int4_mul_cash"
        | "int8_mul_cash" => cash_mul_int,
        "cash_div_int2" | "cash_div_int4" | "cash_div_int8" => cash_div_int,
        "cash_div_cash" => cash_div_cash,
        "cash_words" => cash_words,
        "cash_numeric" => cash_numeric,
        "numeric_cash" => numeric_cash,
        "int4_cash" | "int8_cash" => int_cash,
        _ => return None,
    })
}

/// `22003`, `money out of range`.
fn out_of_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "money out of range")
}

/// The amount of a `money` argument.
fn cents(value: &Value) -> Result<i64> {
    match value {
        Value::Int8(v) => Ok(*v),
        _ => Err(bad_value()),
    }
}

/// The amount and the other argument of a call, with the amount first or second.
fn amount_and_other<'a>(call: &Call<'_>, args: &'a [Value]) -> Result<(i64, &'a Value)> {
    let [a, b] = args else { return Err(bad_value()) };
    if call.args.first().is_some_and(|ty| crate::io::base_type(*ty) == types::oid::MONEY) {
        Ok((cents(a)?, b))
    } else {
        Ok((cents(b)?, a))
    }
}

fn cash_pl(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    cents(a)?.checked_add(cents(b)?).map(Value::Int8).ok_or_else(out_of_range)
}

fn cash_mi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    cents(a)?.checked_sub(cents(b)?).map(Value::Int8).ok_or_else(out_of_range)
}

/// A float result as `money`: `rint`, then the check that it is a number in the range of `bigint`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn from_float(r: f64) -> Result<Value> {
    let r = r.round_ties_even();
    if r.is_nan() || r < i64::MIN as f64 || r >= -(i64::MIN as f64) {
        return Err(out_of_range());
    }
    Ok(Value::Int8(r as i64))
}

/// `cash_mul_flt8` and the others: `float8_mul`, which gives an overflow error for an infinite result of finite values.
#[allow(clippy::cast_precision_loss)]
fn cash_mul_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (c, f) = amount_and_other(call, args)?;
    let f = float(f).ok_or_else(bad_value)?;
    let r = c as f64 * f;
    if r.is_infinite() && f.is_finite() {
        return Err(overflow());
    }
    from_float(r)
}

/// `cash_div_flt8` and `cash_div_flt4`: `float8_div`, which gives an error for a division by zero and an overflow error for an infinite result. The amount is at least 1 when it is not 0, so the quotient does not underflow.
#[allow(clippy::cast_precision_loss)]
fn cash_div_float(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let c = cents(a)?;
    let f = float(b).ok_or_else(bad_value)?;
    if f == 0.0 {
        return Err(division_by_zero());
    }
    let r = c as f64 / f;
    if r.is_infinite() {
        return Err(overflow());
    }
    from_float(r)
}

fn cash_mul_int(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (c, i) = amount_and_other(call, args)?;
    let i = i.as_i64().ok_or_else(bad_value)?;
    c.checked_mul(i).map(Value::Int8).ok_or_else(out_of_range)
}

/// `cash_div_int8` and the others: the quotient truncates toward zero.
fn cash_div_int(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let c = cents(a)?;
    let i = b.as_i64().ok_or_else(bad_value)?;
    if i == 0 {
        return Err(division_by_zero());
    }
    c.checked_div(i).map(Value::Int8).ok_or_else(out_of_range)
}

/// `cash_div_cash`: the quotient as `float8`.
#[allow(clippy::cast_precision_loss)]
fn cash_div_cash(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let divisor = cents(b)?;
    if divisor == 0 {
        return Err(division_by_zero());
    }
    Ok(Value::Float8(cents(a)? as f64 / divisor as f64))
}

fn cash_words(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    Ok(Value::Text(types::cash_words(cents(a)?)))
}

/// `cash_numeric`: the amount with 2 decimal places.
fn cash_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    Ok(Value::Numeric(Numeric::from_decimal(i128::from(cents(a)?), 2)))
}

/// `numeric_cash`: the number times 100, rounded half away from zero as `numeric_int8` rounds.
fn numeric_cash(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Numeric(n)] = args else { return Err(bad_value()) };
    let v = n.mul(&Numeric::from_integer(100)).and_then(|v| v.to_integer("bigint"));
    let v = v.map_err(type_error)?;
    i64::try_from(v).map(Value::Int8).map_err(|_| bigint_out_of_range())
}

/// `22003`, `bigint out of range`, the error of the casts to `money`.
fn bigint_out_of_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
}

/// `int4_cash` and `int8_cash`: the number times 100.
fn int_cash(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    let v = a.as_i64().ok_or_else(bad_value)?;
    v.checked_mul(100).map(Value::Int8).ok_or_else(bigint_out_of_range)
}

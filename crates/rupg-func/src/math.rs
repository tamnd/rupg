//! The arithmetic of the number types and of `date`: the operators of `int.c`, `int8.c`, `float.c`, `numeric.c` and `date.c`, and the functions of one number such as `abs` and `round`.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Numeric, NumericSign, POSTGRES_EPOCH_JDATE, Value, oid};

use crate::compare::float;
use crate::io::base_type;
use crate::{Call, Kernel, bad_value, type_error};

/// `DATE_END_JULIAN`: the first Julian day after the last date.
const DATE_END_JULIAN: i32 = 2_147_483_494;

/// The number types.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Integer,
    Float,
    Numeric,
}

fn kind(ty: u32) -> Option<Kind> {
    match base_type(ty) {
        oid::INT2 | oid::INT4 | oid::INT8 => Some(Kind::Integer),
        oid::FLOAT4 | oid::FLOAT8 => Some(Kind::Float),
        oid::NUMERIC => Some(Kind::Numeric),
        _ => None,
    }
}

/// `22003`, such as `integer out of range`.
fn out_of_range(ret: u32) -> Error {
    let name = match ret {
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        _ => "bigint",
    };
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("{name} out of range"))
}

/// `22012`.
pub(crate) fn division_by_zero() -> Error {
    Error::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}

/// An integer of the type of the result, or the error of a value out of its range.
pub(crate) fn integer(ret: u32, v: i128) -> Result<Value> {
    let value = match base_type(ret) {
        oid::INT2 => i16::try_from(v).map(Value::Int2).ok(),
        oid::INT4 => i32::try_from(v).map(Value::Int4).ok(),
        _ => i64::try_from(v).map(Value::Int8).ok(),
    };
    value.ok_or_else(|| out_of_range(base_type(ret)))
}

/// The two arguments of a call of an integer operator.
fn integers(args: &[Value]) -> Result<(i128, i128)> {
    match args {
        [a, b] => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => Ok((i128::from(a), i128::from(b))),
            _ => Err(bad_value()),
        },
        _ => Err(bad_value()),
    }
}

/// The argument of a call of a function of one integer.
fn one_integer(args: &[Value]) -> Result<i128> {
    args.first().and_then(Value::as_i64).map(i128::from).ok_or_else(bad_value)
}

fn int_add(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a + b)
}

fn int_sub(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a - b)
}

fn int_mul(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a * b)
}

fn int_div(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    if b == 0 {
        return Err(division_by_zero());
    }
    integer(call.ret, a / b)
}

fn int_mod(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    if b == 0 {
        return Err(division_by_zero());
    }
    integer(call.ret, a % b)
}

fn int_and(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a & b)
}

fn int_or(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a | b)
}

fn int_xor(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = integers(args)?;
    integer(call.ret, a ^ b)
}

fn int_neg(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    integer(call.ret, -one_integer(args)?)
}

fn int_not(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    integer(call.ret, !one_integer(args)?)
}

fn int_abs(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    integer(call.ret, one_integer(args)?.abs())
}

fn int_inc(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    integer(call.ret, one_integer(args)? + 1)
}

/// `22003` for a float result that is too large.
fn overflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// `22003` for a float result that is too small.
fn underflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}

/// The operators of `float.c`.
#[derive(Clone, Copy)]
enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// `float8_pl` and the others with their checks. A `float4` result is computed in single precision, as `float4_pl` does.
fn float_op(call: &Call<'_>, args: &[Value], op: FloatOp) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let (Some(x), Some(y)) = (float(a), float(b)) else { return Err(bad_value()) };
    if matches!(op, FloatOp::Div) && y == 0.0 && !x.is_nan() {
        return Err(division_by_zero());
    }
    #[allow(clippy::cast_possible_truncation)]
    let r = if base_type(call.ret) == oid::FLOAT4 {
        let (x, y) = (x as f32, y as f32);
        f64::from(match op {
            FloatOp::Add => x + y,
            FloatOp::Sub => x - y,
            FloatOp::Mul => x * y,
            FloatOp::Div => x / y,
        })
    } else {
        match op {
            FloatOp::Add => x + y,
            FloatOp::Sub => x - y,
            FloatOp::Mul => x * y,
            FloatOp::Div => x / y,
        }
    };
    match op {
        FloatOp::Add | FloatOp::Sub => {
            if r.is_infinite() && !x.is_infinite() && !y.is_infinite() {
                return Err(overflow());
            }
        }
        FloatOp::Mul => {
            if r.is_infinite() && !x.is_infinite() && !y.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && x != 0.0 && y != 0.0 {
                return Err(underflow());
            }
        }
        FloatOp::Div => {
            if r.is_infinite() && !x.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && x != 0.0 && !y.is_infinite() {
                return Err(underflow());
            }
        }
    }
    Ok(float_value(call.ret, r))
}

/// A float of the type of the result. The value of a `float4` result is already in single precision.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn float_value(ret: u32, v: f64) -> Value {
    if base_type(ret) == oid::FLOAT4 { Value::Float4(v as f32) } else { Value::Float8(v) }
}

fn float_add(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_op(call, args, FloatOp::Add)
}

fn float_sub(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_op(call, args, FloatOp::Sub)
}

fn float_mul(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_op(call, args, FloatOp::Mul)
}

fn float_div(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_op(call, args, FloatOp::Div)
}

/// The argument of a function of one float.
fn one_float(args: &[Value]) -> Result<f64> {
    args.first().and_then(float).ok_or_else(bad_value)
}

/// A function of one float that cannot fail.
fn float_fn(call: &Call<'_>, args: &[Value], f: fn(f64) -> f64) -> Result<Value> {
    Ok(float_value(call.ret, f(one_float(args)?)))
}

fn float_neg(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, |x| -x)
}

fn float_abs(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::abs)
}

fn float_round(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::round_ties_even)
}

fn float_trunc(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::trunc)
}

fn float_ceil(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::ceil)
}

fn float_floor(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::floor)
}

/// `dsign`: 1, -1 or 0. NaN gives 0.
fn float_sign(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, |x| {
        if x > 0.0 {
            1.0
        } else if x < 0.0 {
            -1.0
        } else {
            0.0
        }
    })
}

/// `dsqrt`.
fn float_sqrt(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one_float(args)?;
    if x < 0.0 {
        return Err(Error::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "cannot take square root of a negative number",
        ));
    }
    Ok(float_value(call.ret, x.sqrt()))
}

/// `dcbrt`.
fn float_cbrt(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_fn(call, args, f64::cbrt)
}

/// `dpow`, with the special cases of `float.c` for NaN and the infinities.
fn float_pow(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(x), Some(y)) = (args.first().and_then(float), args.get(1).and_then(float)) else {
        return Err(bad_value());
    };
    if x.is_nan() {
        return Ok(Value::Float8(if y.is_nan() || y != 0.0 { f64::NAN } else { 1.0 }));
    }
    if y.is_nan() {
        return Ok(Value::Float8(if x == 1.0 { 1.0 } else { f64::NAN }));
    }
    if x == 0.0 && y < 0.0 {
        return Err(Error::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "zero raised to a negative power is undefined",
        ));
    }
    if x < 0.0 && y.floor() != y {
        return Err(Error::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    let result = if y.is_infinite() {
        let abs = x.abs();
        if abs == 1.0 {
            1.0
        } else if (y > 0.0) == (abs > 1.0) {
            f64::INFINITY
        } else {
            0.0
        }
    } else if x.is_infinite() {
        let odd = (y / 2.0).floor() != y / 2.0;
        match (y == 0.0, x > 0.0, y > 0.0) {
            (true, _, _) => 1.0,
            (false, true, true) => x,
            (false, true, false) => 0.0,
            (false, false, true) => {
                if odd {
                    x
                } else {
                    -x
                }
            }
            (false, false, false) => {
                if odd {
                    -0.0
                } else {
                    0.0
                }
            }
        }
    } else {
        let result = x.powf(y);
        if result.is_infinite() {
            return Err(overflow());
        }
        if result == 0.0 && x != 0.0 {
            return Err(underflow());
        }
        result
    };
    Ok(Value::Float8(result))
}

/// `dexp`.
fn float_exp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one_float(args)?;
    if x.is_nan() || x.is_infinite() {
        return Ok(Value::Float8(if x == f64::NEG_INFINITY { 0.0 } else { x }));
    }
    let result = x.exp();
    if result.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(result))
}

/// `dlog1` and `dlog10`.
fn float_log(args: &[Value], f: fn(f64) -> f64) -> Result<Value> {
    let x = one_float(args)?;
    if x == 0.0 {
        return Err(Error::new(
            SqlState::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of zero",
        ));
    }
    if x < 0.0 {
        return Err(Error::new(
            SqlState::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of a negative number",
        ));
    }
    let result = f(x);
    if result.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && x != 1.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(result))
}

fn float_ln(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_log(args, f64::ln)
}

fn float_log10(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    float_log(args, f64::log10)
}

/// `dpi`.
fn pi(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Float8(std::f64::consts::PI))
}

/// The arguments of a numeric operator.
fn numerics(args: &[Value]) -> Result<(&Numeric, &Numeric)> {
    match args {
        [Value::Numeric(a), Value::Numeric(b)] => Ok((a, b)),
        _ => Err(bad_value()),
    }
}

/// The argument of a function of one numeric.
fn one_numeric(args: &[Value]) -> Result<&Numeric> {
    match args.first() {
        Some(Value::Numeric(a)) => Ok(a),
        _ => Err(bad_value()),
    }
}

/// The value of an `int4` argument.
fn int4_arg(args: &[Value], at: usize) -> Result<i32> {
    match args.get(at) {
        Some(Value::Int4(v)) => Ok(*v),
        _ => Err(bad_value()),
    }
}

fn num(v: std::result::Result<Numeric, rupg_types::TypeError>) -> Result<Value> {
    v.map(Value::Numeric).map_err(type_error)
}

fn numeric_add(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = numerics(args)?;
    num(a.add(b))
}

fn numeric_sub(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = numerics(args)?;
    num(a.sub(b))
}

fn numeric_mul(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = numerics(args)?;
    num(a.mul(b))
}

fn numeric_div(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = numerics(args)?;
    num(a.div(b))
}

fn numeric_mod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = numerics(args)?;
    num(a.modulo(b))
}

fn numeric_neg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Numeric(one_numeric(args)?.negate()))
}

fn numeric_abs(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Numeric(one_numeric(args)?.abs()))
}

fn numeric_inc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.add(&Numeric::from_integer(1)))
}

fn numeric_round(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.round(int4_arg(args, 1)?))
}

fn numeric_trunc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.trunc(int4_arg(args, 1)?))
}

/// `round(numeric)`, a function in SQL that calls `round($1, 0)`.
fn numeric_round0(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.round(0))
}

/// `trunc(numeric)`, a function in SQL that calls `trunc($1, 0)`.
fn numeric_trunc0(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.trunc(0))
}

fn numeric_ceil(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.whole(true))
}

fn numeric_floor(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    num(one_numeric(args)?.whole(false))
}

/// `numeric_sign`: NaN, or -1, 0 or 1 with a display scale of 0.
fn numeric_sign(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one_numeric(args)?;
    Ok(Value::Numeric(match a.sign() {
        NumericSign::NaN => Numeric::NAN,
        NumericSign::Infinity => Numeric::from_integer(1),
        NumericSign::NegativeInfinity => Numeric::from_integer(-1),
        NumericSign::Negative if !a.digits().is_empty() => Numeric::from_integer(-1),
        _ if a.digits().is_empty() => Numeric::from_integer(0),
        _ => Numeric::from_integer(1),
    }))
}

/// `IS_VALID_DATE`.
fn valid_date(d: i64) -> bool {
    (-i64::from(POSTGRES_EPOCH_JDATE)..i64::from(DATE_END_JULIAN - POSTGRES_EPOCH_JDATE))
        .contains(&d)
}

/// A date with a number of days added, as `date_pli` and `date_mii` give it. An infinite date does not change.
fn date_plus(date: i32, days: i64) -> Result<Value> {
    if date == i32::MIN || date == i32::MAX {
        return Ok(Value::Date(date));
    }
    let result = i64::from(date) + days;
    match i32::try_from(result) {
        Ok(d) if valid_date(result) => Ok(Value::Date(d)),
        _ => Err(Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range")),
    }
}

fn date_pli(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), Value::Int4(n)] = args else { return Err(bad_value()) };
    date_plus(*d, i64::from(*n))
}

fn integer_pl_date(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Int4(n), Value::Date(d)] = args else { return Err(bad_value()) };
    date_plus(*d, i64::from(*n))
}

fn date_mii(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), Value::Int4(n)] = args else { return Err(bad_value()) };
    date_plus(*d, -i64::from(*n))
}

/// `date_mi`: the days between two dates.
fn date_mi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(a), Value::Date(b)] = args else { return Err(bad_value()) };
    let infinite = |d: i32| d == i32::MIN || d == i32::MAX;
    if infinite(*a) || infinite(*b) {
        return Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "cannot subtract infinite dates",
        ));
    }
    Ok(Value::Int4(a.wrapping_sub(*b)))
}

/// The kernel of an internal function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "int2abs" | "int4abs" | "int8abs" => int_abs,
        "int2mod" | "int4mod" | "int8mod" => int_mod,
        "int4inc" | "int8inc" => int_inc,
        "float4abs" | "float8abs" => float_abs,
        "dround" => float_round,
        "dtrunc" => float_trunc,
        "dceil" => float_ceil,
        "dfloor" => float_floor,
        "dsign" => float_sign,
        "dsqrt" => float_sqrt,
        "dcbrt" => float_cbrt,
        "dpi" => pi,
        "dpow" => float_pow,
        "dexp" => float_exp,
        "dlog1" => float_ln,
        "dlog10" => float_log10,
        "numeric_abs" => numeric_abs,
        "numeric_mod" => numeric_mod,
        "numeric_inc" => numeric_inc,
        "numeric_round" => numeric_round,
        "numeric_trunc" => numeric_trunc,
        "numeric_ceil" => numeric_ceil,
        "numeric_floor" => numeric_floor,
        "numeric_sign" => numeric_sign,
        "date_pli" => date_pli,
        "date_mii" => date_mii,
        "date_mi" => date_mi,
        "integer_pl_date" => integer_pl_date,
        _ => return None,
    })
}

/// The kernels of `round(numeric)` and `trunc(numeric)`, which are functions in SQL.
pub(crate) fn numeric_sql(func: u32) -> Option<Kernel> {
    match func {
        1708 => Some(numeric_round0),
        1710 => Some(numeric_trunc0),
        _ => None,
    }
}

/// The kernel of an arithmetic operator of two numbers of the same kind.
pub(crate) fn by_operator(name: &str, left: u32, right: u32, ret: u32) -> Option<Kernel> {
    let k = kind(left)?;
    if kind(right) != Some(k) || kind(ret) != Some(k) {
        return None;
    }
    Some(match (k, name) {
        (Kind::Integer, "+") => int_add,
        (Kind::Integer, "-") => int_sub,
        (Kind::Integer, "*") => int_mul,
        (Kind::Integer, "/") => int_div,
        (Kind::Integer, "%") => int_mod,
        (Kind::Integer, "&") => int_and,
        (Kind::Integer, "|") => int_or,
        (Kind::Integer, "#") => int_xor,
        (Kind::Float, "+") => float_add,
        (Kind::Float, "-") => float_sub,
        (Kind::Float, "*") => float_mul,
        (Kind::Float, "/") => float_div,
        (Kind::Numeric, "+") => numeric_add,
        (Kind::Numeric, "-") => numeric_sub,
        (Kind::Numeric, "*") => numeric_mul,
        (Kind::Numeric, "/") => numeric_div,
        (Kind::Numeric, "%") => numeric_mod,
        _ => return None,
    })
}

/// The kernel of a prefix operator of a number.
pub(crate) fn by_prefix_operator(name: &str, arg: u32, ret: u32) -> Option<Kernel> {
    let k = kind(arg)?;
    if kind(ret) != Some(k) {
        return None;
    }
    Some(match (k, name) {
        (_, "+") => identity,
        (Kind::Integer, "-") => int_neg,
        (Kind::Integer, "~") => int_not,
        (Kind::Integer, "@") => int_abs,
        (Kind::Float, "-") => float_neg,
        (Kind::Float, "@") => float_abs,
        (Kind::Float, "|/") => float_sqrt,
        (Kind::Float, "||/") => float_cbrt,
        (Kind::Numeric, "-") => numeric_neg,
        (Kind::Numeric, "@") => numeric_abs,
        _ => return None,
    })
}

/// The value of the argument, as `int4up` gives it.
fn identity(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    args.first().cloned().ok_or_else(bad_value)
}

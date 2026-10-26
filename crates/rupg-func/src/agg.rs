//! The transition and final functions of the aggregates whose state is a value of SQL, from `int8.c`, `numeric.c`, `float.c`, `bool.c` and `misc.c`. The executor calls them for each row of a group, and a query can call them as functions too.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Numeric, Value};

use crate::{Call, Kernel, bad_value, type_error};

/// The check of a percentile of `percentile_disc` and `percentile_cont`, as `percentile_disc_final` and `setup_pct_info` do it.
pub fn check_percentile(p: f64) -> Result<()> {
    if (0.0..=1.0).contains(&p) {
        return Ok(());
    }
    Err(Error::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("percentile value {} is not between 0 and 1", crate::calendar::format_g(p)),
    ))
}

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
        "float8_regr_accum" => regr_accum,
        "float8_regr_combine" => regr_combine,
        "float8_regr_sxx" => regr_sxx,
        "float8_regr_syy" => regr_syy,
        "float8_regr_sxy" => regr_sxy,
        "float8_regr_avgx" => regr_avgx,
        "float8_regr_avgy" => regr_avgy,
        "float8_covar_pop" => covar_pop,
        "float8_covar_samp" => covar_samp,
        "float8_corr" => corr,
        "float8_regr_r2" => regr_r2,
        "float8_regr_slope" => regr_slope,
        "float8_regr_intercept" => regr_intercept,
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

/// The state of the regression aggregates: `N`, `Sx`, `Sxx`, `Sy`, `Syy`, `Sxy`, and the `X` and the `Y` that all inputs had so far, or NaN when the inputs were not all the same.
#[derive(Clone, Copy)]
struct Regr {
    n: f64,
    sx: f64,
    sxx: f64,
    sy: f64,
    syy: f64,
    sxy: f64,
    common_x: f64,
    common_y: f64,
}

impl Regr {
    fn of(state: &Value) -> Result<Regr> {
        let values: [&Value; 8] = elements(state)?;
        let mut f = [0.0; 8];
        for (to, value) in f.iter_mut().zip(values) {
            let Value::Float8(v) = value else { return Err(bad_value()) };
            *to = *v;
        }
        let [n, sx, sxx, sy, syy, sxy, common_x, common_y] = f;
        Ok(Regr { n, sx, sxx, sy, syy, sxy, common_x, common_y })
    }

    fn value(self) -> Value {
        let Regr { n, sx, sxx, sy, syy, sxy, common_x, common_y } = self;
        array([n, sx, sxx, sy, syy, sxy, common_x, common_y].map(Value::Float8).to_vec())
    }
}

fn float_overflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// `float8_regr_accum`: adds a pair of `Y` and `X` to the state with the algorithm of Youngs and Cramer. While all the inputs of `X` are the same, `Sxx` stays exactly 0, and the same is true for `Y`. Only finite values that give an infinite result are an overflow.
fn regr_accum(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state, Value::Float8(y), Value::Float8(x)] = args else { return Err(bad_value()) };
    let (x, y) = (*x, *y);
    let old = Regr::of(state)?;
    let mut r = old;
    r.n += 1.0;
    r.sx += x;
    r.sy += y;
    let bad = |v: f64| v.is_nan() || v.is_infinite();
    if old.n > 0.0 {
        if x != r.common_x || x.is_nan() {
            r.common_x = f64::NAN;
        }
        if y != r.common_y || y.is_nan() {
            r.common_y = f64::NAN;
        }
        let tmp_x = x * r.n - r.sx;
        let tmp_y = y * r.n - r.sy;
        let scale = 1.0 / (r.n * old.n);
        if r.common_x.is_nan() {
            r.sxx += tmp_x * tmp_x * scale;
        }
        if r.common_y.is_nan() {
            r.syy += tmp_y * tmp_y * scale;
        }
        if r.common_x.is_nan() && r.common_y.is_nan() {
            r.sxy += tmp_x * tmp_y * scale;
        } else if bad(x) || bad(y) {
            r.sxy = f64::NAN;
        }
        if r.sx.is_infinite()
            || r.sxx.is_infinite()
            || r.sy.is_infinite()
            || r.syy.is_infinite()
            || r.sxy.is_infinite()
        {
            let finite_x = !old.sx.is_infinite() && !x.is_infinite();
            let finite_y = !old.sy.is_infinite() && !y.is_infinite();
            if ((r.sx.is_infinite() || r.sxx.is_infinite()) && finite_x)
                || ((r.sy.is_infinite() || r.syy.is_infinite()) && finite_y)
                || (r.sxy.is_infinite() && finite_x && finite_y)
            {
                return Err(float_overflow());
            }
            for v in [&mut r.sxx, &mut r.syy, &mut r.sxy] {
                if v.is_infinite() {
                    *v = f64::NAN;
                }
            }
        }
    } else {
        if bad(x) {
            r.sxx = f64::NAN;
            r.sxy = f64::NAN;
        }
        if bad(y) {
            r.syy = f64::NAN;
            r.sxy = f64::NAN;
        }
        r.common_x = x;
        r.common_y = y;
    }
    Ok(r.value())
}

/// `float8_regr_combine`: the state of the inputs of two states.
#[allow(clippy::float_cmp)]
fn regr_combine(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let (a, b) = (Regr::of(a)?, Regr::of(b)?);
    if a.n == 0.0 {
        return Ok(b.value());
    }
    if b.n == 0.0 {
        return Ok(a.value());
    }
    let n = a.n + b.n;
    let sx = a.sx + b.sx;
    if sx.is_infinite() && !a.sx.is_infinite() && !b.sx.is_infinite() {
        return Err(float_overflow());
    }
    let tmp1 = a.sx / a.n - b.sx / b.n;
    let sxx = a.sxx + b.sxx + a.n * b.n * tmp1 * tmp1 / n;
    if sxx.is_infinite() && !a.sxx.is_infinite() && !b.sxx.is_infinite() {
        return Err(float_overflow());
    }
    let sy = a.sy + b.sy;
    if sy.is_infinite() && !a.sy.is_infinite() && !b.sy.is_infinite() {
        return Err(float_overflow());
    }
    let tmp2 = a.sy / a.n - b.sy / b.n;
    let syy = a.syy + b.syy + a.n * b.n * tmp2 * tmp2 / n;
    if syy.is_infinite() && !a.syy.is_infinite() && !b.syy.is_infinite() {
        return Err(float_overflow());
    }
    let sxy = a.sxy + b.sxy + a.n * b.n * tmp1 * tmp2 / n;
    if sxy.is_infinite() && !a.sxy.is_infinite() && !b.sxy.is_infinite() {
        return Err(float_overflow());
    }
    // `float8_eq` makes NaN equal to NaN.
    let same = |p: f64, q: f64| p == q || p.is_nan() && q.is_nan();
    let common_x = if same(a.common_x, b.common_x) { a.common_x } else { f64::NAN };
    let common_y = if same(a.common_y, b.common_y) { a.common_y } else { f64::NAN };
    Ok(Regr { n, sx, sxx, sy, syy, sxy, common_x, common_y }.value())
}

/// The result of a final function of the regression aggregates, which is null when `N` is less than `min`, or when `f` gives `None`.
fn regr_final(args: &[Value], min: f64, f: impl Fn(Regr) -> Option<f64>) -> Result<Value> {
    let [state] = args else { return Err(bad_value()) };
    let r = Regr::of(state)?;
    if r.n < min {
        return Ok(Value::Null);
    }
    Ok(f(r).map_or(Value::Null, Value::Float8))
}

fn regr_sxx(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(r.sxx))
}

fn regr_syy(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(r.syy))
}

fn regr_sxy(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(r.sxy))
}

/// `float8_regr_avgx`: the common value when all the inputs were the same, so that the result has no error of rounding.
fn regr_avgx(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(if r.common_x.is_nan() { r.sx / r.n } else { r.common_x }))
}

fn regr_avgy(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(if r.common_y.is_nan() { r.sy / r.n } else { r.common_y }))
}

fn covar_pop(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| Some(r.sxy / r.n))
}

fn covar_samp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 2.0, |r| Some(r.sxy / (r.n - 1.0)))
}

/// `float8_corr`: null for a horizontal or a vertical line. The result is clamped to the range from -1 to 1.
fn corr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| {
        if r.sxx == 0.0 || r.syy == 0.0 {
            return None;
        }
        let product = r.sxx * r.syy;
        let root = if product == 0.0 || product.is_infinite() {
            r.sxx.sqrt() * r.syy.sqrt()
        } else {
            product.sqrt()
        };
        Some((r.sxy / root).clamp(-1.0, 1.0))
    })
}

/// `float8_regr_r2`: null for a vertical line and 1 for a horizontal line. The result is not more than 1.
fn regr_r2(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| {
        if r.sxx == 0.0 {
            return None;
        }
        if r.syy == 0.0 {
            return Some(1.0);
        }
        let numerator = r.sxy * r.sxy;
        let denominator = r.sxx * r.syy;
        let result = if numerator == 0.0
            || numerator.is_infinite()
            || denominator == 0.0
            || denominator.is_infinite()
        {
            let root = r.sxy / (r.sxx.sqrt() * r.syy.sqrt());
            root * root
        } else {
            numerator / denominator
        };
        // `f64::min` takes 1 for NaN, which the comparison of PG keeps.
        Some(if result > 1.0 { 1.0 } else { result })
    })
}

/// `float8_regr_slope`: null for a vertical line.
fn regr_slope(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| if r.sxx == 0.0 { None } else { Some(r.sxy / r.sxx) })
}

/// `frexp`: the mantissa from 0.5 up to but not including 1, and the exponent of 2. Zero, the infinities and NaN have the exponent 0.
fn frexp(x: f64) -> (f64, i32) {
    let bits = x.to_bits();
    let exponent = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    match exponent {
        0 if x == 0.0 => (x, 0),
        0 => {
            let (m, e) = frexp(x * f64::from_bits(0x43f0_0000_0000_0000));
            (m, e - 64)
        }
        0x7ff => (x, 0),
        _ => (f64::from_bits(bits & !(0x7ff << 52) | 0x3fe << 52), exponent - 0x3fe),
    }
}

/// `ldexp`: `x` times 2 to the power `n`, with one rounding.
fn ldexp(x: f64, mut n: i32) -> f64 {
    let pow2 = |e: i32| f64::from_bits(u64::try_from(0x3ff + e).unwrap_or(0) << 52);
    let mut y = x;
    if n > 1023 {
        y *= pow2(1023);
        n -= 1023;
        if n > 1023 {
            y *= pow2(1023);
            n = (n - 1023).min(1023);
        }
    } else if n < -1022 {
        // 2 to the power -969 keeps the value normal, so that only the last product rounds.
        y *= pow2(-1022 + 53);
        n += 1022 - 53;
        if n < -1022 {
            y *= pow2(-1022 + 53);
            n = (n + 1022 - 53).max(-1022);
        }
    }
    y * pow2(n)
}

/// `float8_regr_intercept`: null for a vertical line. When `Sx * Sxy` underflows or overflows, the quotient `Sx * Sxy / Sxx` is computed from the mantissas and the exponents.
fn regr_intercept(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    regr_final(args, 1.0, |r| {
        if r.sxx == 0.0 {
            return None;
        }
        let mut dy = r.sx * r.sxy / r.sxx;
        if (dy == 0.0 || dy.is_infinite()) && [r.sx, r.sxy, r.sxx].iter().all(|v| v.is_finite()) {
            let ((m_sx, n_sx), (m_sxy, n_sxy), (m_sxx, n_sxx)) =
                (frexp(r.sx), frexp(r.sxy), frexp(r.sxx));
            dy = ldexp(m_sx * m_sxy / m_sxx, n_sx + n_sxy - n_sxx);
        }
        Some((r.sy - dy) / r.n)
    })
}

/// `any_value_transfn`: keeps the state, which is the first value that is not null.
fn any_value(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    args.first().cloned().ok_or_else(bad_value)
}

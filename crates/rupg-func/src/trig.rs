//! The trigonometric, hyperbolic, error and gamma functions of `float.c`, and `width_bucket` of `float8`.
//!
//! The functions in degrees use the method of PostgreSQL: they reduce the angle to the first quadrant and scale the result of the C library by constants such as `sin(30)`, so that `sind(30)` is exactly 0.5 and `tand(45)` is exactly 1.

use std::hint::black_box;
use std::sync::OnceLock;

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::compare::float;
use crate::math::{overflow, underflow};
use crate::{Call, Kernel, bad_value, special};

/// `RADIANS_PER_DEGREE` of `float.h`.
const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// The float argument at `at`.
fn arg(args: &[Value], at: usize) -> Result<f64> {
    args.get(at).and_then(float).ok_or_else(bad_value)
}

/// `22003` for an argument outside the domain of the function.
fn out_of_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range")
}

/// A `float8` result that must be finite.
fn finite(r: f64) -> Result<Value> {
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(Value::Float8(r))
}

/// A function of one float that gives NaN for NaN and rejects arguments outside `[-1, 1]`.
fn unit_domain(args: &[Value], f: fn(f64) -> f64) -> Result<Value> {
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range());
    }
    finite(f(x))
}

/// A function of one float that gives NaN for NaN and rejects an infinite argument.
fn periodic(args: &[Value], f: fn(f64) -> f64) -> Result<f64> {
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range());
    }
    Ok(f(x))
}

fn dacos(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    unit_domain(args, f64::acos)
}

fn dasin(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    unit_domain(args, f64::asin)
}

fn datan(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    finite(x.atan())
}

fn datan2(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (y, x) = (arg(args, 0)?, arg(args, 1)?);
    if y.is_nan() || x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    finite(y.atan2(x))
}

fn dcos(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    finite(periodic(args, f64::cos)?)
}

fn dsin(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    finite(periodic(args, f64::sin)?)
}

/// `dtan`. `tan(pi/2)` can be infinite, so there is no check of overflow.
fn dtan(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(periodic(args, f64::tan)?))
}

/// `dcot`: `1 / tan(x)`. `cot(0)` is infinite.
fn dcot(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(1.0 / periodic(args, f64::tan)?))
}

/// The constants of `init_degree_constants`. The arguments go through `black_box`, so the compiler cannot compute them with a different `sin`.
struct Degrees {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn degrees() -> &'static Degrees {
    static DEGREES: OnceLock<Degrees> = OnceLock::new();
    DEGREES.get_or_init(|| {
        let mut d = Degrees {
            sin_30: (black_box(30.0) * RADIANS_PER_DEGREE).sin(),
            one_minus_cos_60: 1.0 - (black_box(60.0) * RADIANS_PER_DEGREE).cos(),
            asin_0_5: black_box(0.5f64).asin(),
            acos_0_5: black_box(0.5f64).acos(),
            atan_1_0: black_box(1.0f64).atan(),
            tan_45: 1.0,
            cot_45: 1.0,
        };
        let forty_five = black_box(45.0);
        d.tan_45 = sind_q1(&d, forty_five) / cosd_q1(&d, forty_five);
        d.cot_45 = cosd_q1(&d, forty_five) / sind_q1(&d, forty_five);
        d
    })
}

/// The arcsine in degrees of `x` in `[0, 1]`. 0, 0.5 and 1 give exactly 0, 30 and 90.
fn asind_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 0.5 { (x.asin() / d.asin_0_5) * 30.0 } else { 90.0 - (x.acos() / d.acos_0_5) * 60.0 }
}

/// The arccosine in degrees of `x` in `[0, 1]`. 0, 0.5 and 1 give exactly 90, 60 and 0.
fn acosd_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 0.5 { 90.0 - (x.asin() / d.asin_0_5) * 30.0 } else { (x.acos() / d.acos_0_5) * 60.0 }
}

/// The sine of an angle from 0 to 30 degrees.
fn sind_0_to_30(d: &Degrees, x: f64) -> f64 {
    ((x * RADIANS_PER_DEGREE).sin() / d.sin_30) / 2.0
}

/// The cosine of an angle from 0 to 60 degrees.
fn cosd_0_to_60(d: &Degrees, x: f64) -> f64 {
    let one_minus_cos_x = 1.0 - (x * RADIANS_PER_DEGREE).cos();
    1.0 - (one_minus_cos_x / d.one_minus_cos_60) / 2.0
}

/// The sine of an angle in the first quadrant.
fn sind_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 30.0 { sind_0_to_30(d, x) } else { cosd_0_to_60(d, 90.0 - x) }
}

/// The cosine of an angle in the first quadrant.
fn cosd_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 60.0 { cosd_0_to_60(d, x) } else { sind_0_to_30(d, 90.0 - x) }
}

fn dacosd(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let d = degrees();
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range());
    }
    finite(if x >= 0.0 { acosd_q1(d, x) } else { 90.0 + asind_q1(d, -x) })
}

fn dasind(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let d = degrees();
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range());
    }
    finite(if x >= 0.0 { asind_q1(d, x) } else { -asind_q1(d, -x) })
}

fn datand(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    finite((x.atan() / degrees().atan_1_0) * 45.0)
}

fn datan2d(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (y, x) = (arg(args, 0)?, arg(args, 1)?);
    if y.is_nan() || x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    finite((y.atan2(x) / degrees().atan_1_0) * 45.0)
}

/// The angle in `[0, 90]` and the sign of the result for a function in degrees. `odd` tells if `f(-x) = -f(x)`, and `back` tells if `f(180 - x) = -f(x)`. Then `f(360 - x)` has the sign of `f(-x)`.
fn reduce(x: f64, odd: bool, back: bool) -> (f64, f64) {
    let mut x = x % 360.0;
    let mut sign = 1.0;
    if x < 0.0 {
        x = -x;
        if odd {
            sign = -sign;
        }
    }
    if x > 180.0 {
        x = 360.0 - x;
        if odd {
            sign = -sign;
        }
    }
    if x > 90.0 {
        x = 180.0 - x;
        if back {
            sign = -sign;
        }
    }
    (x, sign)
}

/// The argument of a function in degrees: NaN gives `None`, and infinity is an error.
fn degree_arg(args: &[Value]) -> Result<Option<f64>> {
    let x = arg(args, 0)?;
    if x.is_nan() {
        return Ok(None);
    }
    if x.is_infinite() {
        return Err(out_of_range());
    }
    Ok(Some(x))
}

fn dcosd(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(x) = degree_arg(args)? else { return Ok(Value::Float8(f64::NAN)) };
    let (x, sign) = reduce(x, false, true);
    finite(sign * cosd_q1(degrees(), x))
}

fn dsind(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(x) = degree_arg(args)? else { return Ok(Value::Float8(f64::NAN)) };
    let (x, sign) = reduce(x, true, false);
    finite(sign * sind_q1(degrees(), x))
}

/// `dtand`. Minus zero becomes zero, and `tand(90)` is infinite.
fn dtand(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(x) = degree_arg(args)? else { return Ok(Value::Float8(f64::NAN)) };
    let d = degrees();
    let (x, sign) = reduce(x, true, true);
    let tan_x = sind_q1(d, x) / cosd_q1(d, x);
    let r = sign * (tan_x / d.tan_45);
    Ok(Value::Float8(if r == 0.0 { 0.0 } else { r }))
}

/// `dcotd`. Minus zero becomes zero, and `cotd(0)` is infinite.
fn dcotd(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(x) = degree_arg(args)? else { return Ok(Value::Float8(f64::NAN)) };
    let d = degrees();
    let (x, sign) = reduce(x, true, true);
    let cot_x = cosd_q1(d, x) / sind_q1(d, x);
    let r = sign * (cot_x / d.cot_45);
    Ok(Value::Float8(if r == 0.0 { 0.0 } else { r }))
}

/// `degrees`: `float8_div(x, RADIANS_PER_DEGREE)`.
fn to_degrees(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    let r = x / RADIANS_PER_DEGREE;
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(r))
}

/// `radians`: `float8_mul(x, RADIANS_PER_DEGREE)`.
fn to_radians(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    let r = x * RADIANS_PER_DEGREE;
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(r))
}

/// `dsinh`. An overflow gives an infinity with the sign of the argument.
fn dsinh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(arg(args, 0)?.sinh()))
}

/// `dcosh`. An overflow gives infinity.
fn dcosh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let r = arg(args, 0)?.cosh();
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(r))
}

fn dtanh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    finite(arg(args, 0)?.tanh())
}

fn dasinh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(special::asinh(arg(args, 0)?)))
}

fn dacosh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    if x < 1.0 {
        return Err(out_of_range());
    }
    Ok(Value::Float8(special::acosh(x)))
}

fn datanh(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    if x < -1.0 || x > 1.0 {
        return Err(out_of_range());
    }
    Ok(Value::Float8(if x == -1.0 {
        f64::NEG_INFINITY
    } else if x == 1.0 {
        f64::INFINITY
    } else {
        special::atanh(x)
    }))
}

fn derf(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    finite(special::erf(arg(args, 0)?))
}

fn derfc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    finite(special::erfc(arg(args, 0)?))
}

/// `dgamma`. A pole, an overflow and minus infinity are errors of overflow. A result of zero is an error of underflow.
fn dgamma(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    if x.is_nan() || x == f64::INFINITY {
        return Ok(Value::Float8(x));
    }
    if x == f64::NEG_INFINITY {
        return Err(overflow());
    }
    let r = special::tgamma(x);
    if r.is_infinite() || r.is_nan() {
        return Err(overflow());
    }
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(Value::Float8(r))
}

/// `dlgamma`. A pole and an overflow are errors.
fn dlgamma(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = arg(args, 0)?;
    let r = special::lgamma(x);
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    Ok(Value::Float8(r))
}

/// `2201G` for a bad argument of `width_bucket`.
fn bucket_error(message: &str) -> Error {
    Error::new(SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION, message)
}

/// `width_bucket_float8`: the bucket of `operand` among `count` buckets of the same width from `bound1` to `bound2`.
#[allow(clippy::cast_possible_truncation)]
fn width_bucket_float8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (operand, bound1, bound2) = (arg(args, 0)?, arg(args, 1)?, arg(args, 2)?);
    let Some(Value::Int4(count)) = args.get(3) else { return Err(bad_value()) };
    let count = *count;
    if count <= 0 {
        return Err(bucket_error("count must be greater than zero"));
    }
    if bound1.is_nan() || bound2.is_nan() {
        return Err(bucket_error("lower and upper bounds cannot be NaN"));
    }
    if bound1.is_infinite() || bound2.is_infinite() {
        return Err(bucket_error("lower and upper bounds must be finite"));
    }
    let past = || {
        count
            .checked_add(1)
            .ok_or_else(|| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))
    };
    let inside = |low: f64, high: f64, x: f64, ascending: bool| {
        let fraction = if (high - low).is_infinite() {
            if ascending {
                (x / 2.0 - low / 2.0) / (high / 2.0 - low / 2.0)
            } else {
                (high / 2.0 - x / 2.0) / (high / 2.0 - low / 2.0)
            }
        } else if ascending {
            (x - low) / (high - low)
        } else {
            (high - x) / (high - low)
        };
        let r = (f64::from(count) * fraction) as i32;
        r.min(count - 1) + 1
    };
    let result = if bound1 < bound2 {
        if operand.is_nan() || operand >= bound2 {
            past()?
        } else if operand < bound1 {
            0
        } else {
            inside(bound1, bound2, operand, true)
        }
    } else if bound1 > bound2 {
        if operand.is_nan() || operand > bound1 {
            0
        } else if operand <= bound2 {
            past()?
        } else {
            inside(bound2, bound1, operand, false)
        }
    } else {
        return Err(bucket_error("lower bound cannot equal upper bound"));
    };
    Ok(Value::Int4(result))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "dacos" => dacos,
        "dasin" => dasin,
        "datan" => datan,
        "datan2" => datan2,
        "dcos" => dcos,
        "dsin" => dsin,
        "dtan" => dtan,
        "dcot" => dcot,
        "dacosd" => dacosd,
        "dasind" => dasind,
        "datand" => datand,
        "datan2d" => datan2d,
        "dcosd" => dcosd,
        "dsind" => dsind,
        "dtand" => dtand,
        "dcotd" => dcotd,
        "degrees" => to_degrees,
        "radians" => to_radians,
        "dsinh" => dsinh,
        "dcosh" => dcosh,
        "dtanh" => dtanh,
        "dasinh" => dasinh,
        "dacosh" => dacosh,
        "datanh" => datanh,
        "derf" => derf,
        "derfc" => derfc,
        "dgamma" => dgamma,
        "dlgamma" => dlgamma,
        "width_bucket_float8" => width_bucket_float8,
        _ => return None,
    })
}

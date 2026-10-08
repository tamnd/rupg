//! The comparison operators of the types, and `GREATEST` and `LEAST` through `larger` and `smaller`. One comparison serves each type, as the B-tree support function `cmp` of the type does.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_types::{Array, RegKind, USECS_PER_DAY, USECS_PER_SEC, Value, oid};

use crate::io::base_type;
use crate::{Call, Kernel, bad_value};

/// The groups of types that compare with each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Integer,
    Float,
    Numeric,
    Text,
    Bool,
    Char,
    Oid,
    Bytea,
    Uuid,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Array,
}

/// The family of a type, or `None` for a type that has no comparison here.
fn family(ty: u32) -> Option<Family> {
    let ty = base_type(ty);
    Some(match ty {
        oid::INT2 | oid::INT4 | oid::INT8 => Family::Integer,
        oid::FLOAT4 | oid::FLOAT8 => Family::Float,
        oid::NUMERIC => Family::Numeric,
        oid::TEXT | oid::NAME | oid::VARCHAR | oid::BPCHAR => Family::Text,
        oid::BOOL => Family::Bool,
        oid::CHAR => Family::Char,
        oid::OID => Family::Oid,
        ty if RegKind::from_oid(ty).is_some() => Family::Oid,
        oid::BYTEA => Family::Bytea,
        oid::UUID => Family::Uuid,
        oid::DATE => Family::Date,
        oid::TIME => Family::Time,
        oid::TIMETZ => Family::TimeTz,
        oid::TIMESTAMP => Family::Timestamp,
        oid::TIMESTAMPTZ => Family::TimestampTz,
        oid::INTERVAL => Family::Interval,
        oid::ANYARRAY | oid::INT2VECTOR | oid::OIDVECTOR => Family::Array,
        _ => match builtin::type_by_oid(ty) {
            Some(row) if row.category == b'A' && row.elem != 0 => Family::Array,
            _ => return None,
        },
    })
}

/// The element type of an array type, or 0.
fn element(ty: u32) -> u32 {
    builtin::type_by_oid(base_type(ty)).map_or(0, |row| row.elem)
}

/// The order of two values that are not null. `left` and `right` are their types: a `bpchar` value compares without its trailing spaces.
///
/// # Errors
///
/// `0A000` for values of types that do not compare here.
pub fn compare(left: u32, a: &Value, right: u32, b: &Value) -> Result<Ordering> {
    let trim = |ty: u32, s: &'_ str| -> usize {
        if base_type(ty) == oid::BPCHAR { s.trim_end_matches(' ').len() } else { s.len() }
    };
    Ok(match (a, b) {
        (Value::Int2(_) | Value::Int4(_) | Value::Int8(_), _) => {
            let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) else { return Err(bad_value()) };
            x.cmp(&y)
        }
        (Value::Float4(_) | Value::Float8(_), _) => {
            let (Some(x), Some(y)) = (float(a), float(b)) else { return Err(bad_value()) };
            float_order(x, y)
        }
        (Value::Numeric(x), Value::Numeric(y)) => x.compare(y),
        (Value::Text(x), Value::Text(y)) => {
            x.as_bytes()[..trim(left, x)].cmp(&y.as_bytes()[..trim(right, y)])
        }
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Char(x), Value::Char(y)) => x.cmp(y),
        (Value::Oid(x), Value::Oid(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y))
        | (Value::Timestamp(x), Value::Timestamp(y))
        | (Value::TimestampTz(x), Value::TimestampTz(y)) => x.cmp(y),
        (Value::TimeTz(t1, z1), Value::TimeTz(t2, z2)) => {
            let utc1 = i128::from(*t1) + i128::from(*z1) * i128::from(USECS_PER_SEC);
            let utc2 = i128::from(*t2) + i128::from(*z2) * i128::from(USECS_PER_SEC);
            utc1.cmp(&utc2).then(z1.cmp(z2))
        }
        (Value::Interval(x), Value::Interval(y)) => span(x).cmp(&span(y)),
        (Value::Array(x), Value::Array(y)) => array_order(element(left), x, element(right), y)?,
        _ => return Err(bad_value()),
    })
}

/// True when two values that are not null are equal.
///
/// # Errors
///
/// The errors of [`compare`].
pub fn equal(left: u32, a: &Value, right: u32, b: &Value) -> Result<bool> {
    Ok(compare(left, a, right, b)?.is_eq())
}

/// The value of a `float4` or a `float8` as a double.
pub(crate) fn float(v: &Value) -> Option<f64> {
    match v {
        Value::Float4(x) => Some(f64::from(*x)),
        Value::Float8(x) => Some(*x),
        _ => None,
    }
}

/// `float8_cmp_internal`: NaN is equal to itself and above every other value.
fn float_order(x: f64, y: f64) -> Ordering {
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
    }
}

/// `interval_cmp_value`: the length of an interval in microseconds, with 30 days in a month and 24 hours in a day.
fn span(iv: &rupg_types::Interval) -> i128 {
    let days = i128::from(iv.month) * 30 + i128::from(iv.day);
    i128::from(iv.time) + days * i128::from(USECS_PER_DAY)
}

/// `array_cmp`: the elements in order, a null element above the others, then the number of elements, the number of dimensions, the lengths and the lower bounds.
fn array_order(left: u32, a: &Array<Value>, right: u32, b: &Array<Value>) -> Result<Ordering> {
    for (x, y) in a.values.iter().zip(&b.values) {
        let order = match (x, y) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => compare(left, x, right, y)?,
        };
        if order.is_ne() {
            return Ok(order);
        }
    }
    let lens = |arr: &Array<Value>| arr.dims.iter().map(|d| d.len).collect::<Vec<_>>();
    let lowers = |arr: &Array<Value>| arr.dims.iter().map(|d| d.lower).collect::<Vec<_>>();
    Ok(a.values
        .len()
        .cmp(&b.values.len())
        .then(a.dims.len().cmp(&b.dims.len()))
        .then_with(|| lens(a).cmp(&lens(b)))
        .then_with(|| lowers(a).cmp(&lowers(b))))
}

/// The order of the two arguments of a call.
fn order(call: &Call<'_>, args: &[Value]) -> Result<Ordering> {
    let [a, b] = args else { return Err(bad_value()) };
    let left = call.args.first().copied().unwrap_or(0);
    let right = call.args.get(1).copied().unwrap_or(0);
    if family(left) == Some(Family::Array)
        && family(right) == Some(Family::Array)
        && base_type(element(left)) != base_type(element(right))
    {
        return Err(Error::new(
            SqlState::DATATYPE_MISMATCH,
            "cannot compare arrays of different element types",
        ));
    }
    compare(left, a, right, b)
}

fn eq(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_eq()))
}

fn ne(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_ne()))
}

fn lt(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_lt()))
}

fn le(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_le()))
}

fn gt(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_gt()))
}

fn ge(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(call, args)?.is_ge()))
}

/// `larger`: the first argument when it is greater, else the second. So for two equal values, such as `1.0` and `1.00`, the result is the second.
fn larger(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let pick = !order(call, args)?.is_gt();
    Ok(args[usize::from(pick)].clone())
}

/// `smaller`: the first argument when it is less, else the second.
fn smaller(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let pick = !order(call, args)?.is_lt();
    Ok(args[usize::from(pick)].clone())
}

/// `bpchar_larger` and `tidlarger`: the first argument when it is greater or equal, else the second.
fn larger_or_first(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let pick = order(call, args)?.is_lt();
    Ok(args[usize::from(pick)].clone())
}

/// `bpchar_smaller` and `tidsmaller`: the first argument when it is less or equal, else the second.
fn smaller_or_first(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let pick = order(call, args)?.is_gt();
    Ok(args[usize::from(pick)].clone())
}

/// `cmp`: -1, 0 or 1.
fn cmp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(match order(call, args)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

/// `btoidvectorcmp`: an `oidvector` with fewer elements comes first, then the elements in order.
fn oidvector_order(args: &[Value]) -> Result<Ordering> {
    let [Value::Array(a), Value::Array(b)] = args else { return Err(bad_value()) };
    let mut order = a.values.len().cmp(&b.values.len());
    for (x, y) in a.values.iter().zip(&b.values) {
        if order.is_ne() {
            break;
        }
        let (Some(Value::Oid(x)), Some(Value::Oid(y))) = (x, y) else { return Err(bad_value()) };
        order = x.cmp(y);
    }
    Ok(order)
}

fn oidvector_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_eq()))
}

fn oidvector_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_ne()))
}

fn oidvector_lt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_lt()))
}

fn oidvector_le(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_le()))
}

fn oidvector_gt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_gt()))
}

fn oidvector_ge(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(oidvector_order(args)?.is_ge()))
}

fn oidvector_cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(match oidvector_order(args)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

/// The comparison functions of `oidvector`, which do not compare as arrays.
fn oidvector_by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "oidvectoreq" => oidvector_eq,
        "oidvectorne" => oidvector_ne,
        "oidvectorlt" => oidvector_lt,
        "oidvectorle" => oidvector_le,
        "oidvectorgt" => oidvector_gt,
        "oidvectorge" => oidvector_ge,
        "btoidvectorcmp" => oidvector_cmp,
        _ => return None,
    })
}

/// True when both arguments of the function are of one family that compares here.
fn comparable(left: u32, right: u32) -> bool {
    family(left).is_some() && family(left) == family(right)
}

/// The kernel of a comparison operator for its argument types.
pub(crate) fn by_operator(name: &str, left: u32, right: u32) -> Option<Kernel> {
    if !comparable(left, right) {
        return None;
    }
    Some(match name {
        "=" => eq,
        "<>" => ne,
        "<" => lt,
        "<=" => le,
        ">" => gt,
        ">=" => ge,
        _ => return None,
    })
}

/// `xideq` and `xidneq` of `xid.c`: the values are equal when the numbers are equal.
fn xid_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Oid(a), Value::Oid(b)] = args else { return Err(bad_value()) };
    Ok(Value::Bool(a == b))
}

fn xid_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Oid(a), Value::Oid(b)] = args else { return Err(bad_value()) };
    Ok(Value::Bool(a != b))
}

/// The kernel of `larger`, `smaller` and the B-tree `cmp` functions.
pub(crate) fn by_src(src: &str, proc: &ProcRow) -> Option<Kernel> {
    let [left, right] = proc.argtypes else { return None };
    // `xid` has `=` and `<>` and no order, so it is not in a family.
    match src {
        "xideq" => return Some(xid_eq),
        "xidneq" => return Some(xid_ne),
        _ => {}
    }
    if !comparable(*left, *right) {
        return None;
    }
    if let Some(kernel) = oidvector_by_src(src) {
        Some(kernel)
    } else if src == "bpchar_larger" || src == "tidlarger" {
        Some(larger_or_first)
    } else if src == "bpchar_smaller" || src == "tidsmaller" {
        Some(smaller_or_first)
    } else if src.ends_with("larger") {
        Some(larger)
    } else if src.ends_with("smaller") {
        Some(smaller)
    } else if src.starts_with("bt") && src.ends_with("cmp") || src == "array_cmp" {
        Some(cmp)
    } else {
        None
    }
}

//! The comparison operators of the types, and `GREATEST` and `LEAST` through `larger` and `smaller`. One comparison serves each type, as the B-tree support function `cmp` of the type does.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_types::{
    Array, DATE_INFINITY, DATE_NEGATIVE_INFINITY, POSTGRES_EPOCH_JDATE, Record, RegKind,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, UNIX_TO_POSTGRES_USECS, USECS_PER_DAY,
    USECS_PER_SEC, Value, format_type, oid,
};

use crate::cast::{TIMESTAMP_END_JULIAN, infinite, valid_timestamp};
use crate::io::base_type;
use crate::{Call, Kernel, bad_value, multirange, range};

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
    Network,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Array,
    Money,
    Tid,
    /// `xid8` and `oid8`, which hold a `u64` in the bits of a `bigint`.
    Xid8,
    Oid8,
    Macaddr,
    /// `macaddr8`, which holds a `u64` in the bits of a `bigint`.
    Macaddr8,
    /// `pg_lsn`, which holds a `u64` in the bits of a `bigint`.
    PgLsn,
    /// `bit` and `bit varying`, a string of `0` and `1` whose byte order is the order of `bitcmp`.
    Bit,
    /// The range types, which compare as `range_cmp` does.
    Range,
    /// The multirange types, which compare as `multirange_cmp` does.
    Multirange,
    /// `tsvector`, which compares as `tsvector_cmp` does.
    TsVector,
    /// `tsquery`, which compares as `tsquery_cmp` does.
    TsQuery,
    /// `record`, which compares as `record_cmp` does.
    Record,
    /// The enum types. A value is the OID of its label. The even OIDs of the labels of a type are in their sort order, so two even OIDs compare as numbers, as `enum_cmp_internal` compares them. An odd OID comes from `ALTER TYPE ... ADD VALUE` when no even OID sorts correctly, and then the places of the labels in the sort order give the order.
    Enum,
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
        oid::INET | oid::CIDR => Family::Network,
        oid::DATE => Family::Date,
        oid::TIME => Family::Time,
        oid::TIMETZ => Family::TimeTz,
        oid::TIMESTAMP => Family::Timestamp,
        oid::TIMESTAMPTZ => Family::TimestampTz,
        oid::INTERVAL => Family::Interval,
        oid::ANYARRAY | oid::INT2VECTOR | oid::OIDVECTOR => Family::Array,
        oid::MONEY => Family::Money,
        oid::TID => Family::Tid,
        oid::XID8 => Family::Xid8,
        oid::OID8 => Family::Oid8,
        oid::MACADDR => Family::Macaddr,
        oid::MACADDR8 => Family::Macaddr8,
        oid::PG_LSN => Family::PgLsn,
        oid::BIT | oid::VARBIT => Family::Bit,
        oid::ANYRANGE => Family::Range,
        ty if builtin::range_subtype(ty).is_some() => Family::Range,
        oid::ANYMULTIRANGE => Family::Multirange,
        ty if builtin::multirange_range(ty).is_some() => Family::Multirange,
        oid::TSVECTOR => Family::TsVector,
        oid::TSQUERY => Family::TsQuery,
        oid::RECORD => Family::Record,
        oid::ANYENUM => Family::Enum,
        _ => match builtin::type_by_oid(ty) {
            Some(row) if row.category == b'A' && row.elem != 0 => Family::Array,
            Some(row) if row.kind == b'e' => Family::Enum,
            _ => return None,
        },
    })
}

/// True when one of the two types is an enum type or `anyenum`, so that two OIDs are labels of an enum type.
fn is_enum_pair(left: u32, right: u32) -> bool {
    family(left) == Some(Family::Enum) || family(right) == Some(Family::Enum)
}

/// `enum_cmp_internal` with an odd OID: the order of the places of the two labels in the sort order of their type.
fn enum_order(x: u32, y: u32) -> Ordering {
    match (builtin::enum_label_place(x), builtin::enum_label_place(y)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => x.cmp(&y),
    }
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
    let unsigned =
        |ty: u32| matches!(base_type(ty), oid::XID8 | oid::OID8 | oid::MACADDR8 | oid::PG_LSN);
    Ok(match (a, b) {
        (Value::Int8(x), Value::Int8(y)) if unsigned(left) && unsigned(right) => {
            x.cast_unsigned().cmp(&y.cast_unsigned())
        }
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
        (Value::Oid(x), Value::Oid(y)) if (x | y) & 1 == 1 && is_enum_pair(left, right) => {
            enum_order(*x, *y)
        }
        (Value::Oid(x), Value::Oid(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        (Value::Inet(x), Value::Inet(y)) => rupg_types::network_cmp(x, y).cmp(&0),
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
        (Value::Range(x), Value::Range(y)) => {
            let sub = match range::subtype(left) {
                0 => range::subtype(right),
                sub => sub,
            };
            range::range_order(sub, x, y)?
        }
        (Value::Multirange(x), Value::Multirange(y)) => {
            let sub = match range::subtype(multirange::range_of(left)) {
                0 => range::subtype(multirange::range_of(right)),
                sub => sub,
            };
            multirange::multirange_order(sub, x, y)?
        }
        (Value::TsVector(x), Value::TsVector(y)) => rupg_types::tsvector_cmp(x, y).cmp(&0),
        (Value::TsQuery(x), Value::TsQuery(y)) => rupg_types::tsquery_cmp(x, y).cmp(&0),
        (Value::Record(x), Value::Record(y)) => record_order(x, y, false)?,
        _ => return Err(bad_value()),
    })
}

/// `record_cmp`, or `record_eq` when `equality` is true, which has its own error for a column type with no comparison: the columns in order, with a null column above the others and two null columns equal. The types of the columns must be the same up to the first column that differs, and the records must have the same number of columns when no column differs.
fn record_order(a: &Record, b: &Record, equality: bool) -> Result<Ordering> {
    let columns = a.types.iter().zip(&a.values).zip(b.types.iter().zip(&b.values));
    for (n, ((&left, x), (&right, y))) in columns.enumerate() {
        if left != right {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "cannot compare dissimilar column types {} and {} at record column {}",
                    format_type(left),
                    format_type(right),
                    n + 1
                ),
            ));
        }
        if family(left).is_none() && !matches!(x, Value::Record(_)) {
            let message = if equality {
                format!("could not identify an equality operator for type {}", format_type(left))
            } else {
                format!("could not identify a comparison function for type {}", format_type(left))
            };
            return Err(Error::new(SqlState::UNDEFINED_FUNCTION, message));
        }
        let order = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare(left, x, right, y)?,
        };
        if order.is_ne() {
            return Ok(order);
        }
    }
    if a.values.len() != b.values.len() {
        return Err(Error::new(
            SqlState::DATATYPE_MISMATCH,
            "cannot compare record types with different numbers of columns",
        ));
    }
    Ok(Ordering::Equal)
}

/// `record_eq` and `record_ne`, which have the error of `record_eq` for a column type with no equality.
fn record_equal(args: &[Value]) -> Result<bool> {
    let [Value::Record(a), Value::Record(b)] = args else { return Err(bad_value()) };
    Ok(record_order(a, b, true)?.is_eq())
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
pub(crate) fn span(period: &rupg_types::Interval) -> i128 {
    let days = i128::from(period.month) * 30 + i128::from(period.day);
    i128::from(period.time) + days * i128::from(USECS_PER_DAY)
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
    if let Some(order) = mixed_datetime(call, a, b)? {
        return Ok(order);
    }
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
    let (left, right) = (family(left), family(right));
    left.is_some() && (left == right || datetime(left) && datetime(right))
}

/// True for the families of `date`, `timestamp` and `timestamptz`, which compare with each other.
fn datetime(family: Option<Family>) -> bool {
    matches!(family, Some(Family::Date | Family::Timestamp | Family::TimestampTz))
}

/// The rank of a date, a timestamp and a `timestamptz` for the comparisons across these types: the value of the lower rank is promoted to the type of the higher rank.
fn datetime_rank(value: &Value) -> Option<u8> {
    match value {
        Value::Date(_) => Some(0),
        Value::Timestamp(_) => Some(1),
        Value::TimestampTz(_) => Some(2),
        _ => None,
    }
}

/// A date, a timestamp or a `timestamptz` promoted to the type of a rank, as `date2timestamp_safe`, `date2timestamptz_safe` and `timestamp2timestamptz_safe` give it with a soft error. The result is twice the microseconds. A value above the range is one less than twice infinity and a value below the range is one more than twice minus infinity, so it is above or below each finite value and not equal to an infinity, as `date_cmp_timestamp_internal`, `date_cmp_timestamptz_internal` and `timestamp_cmp_timestamptz_internal` give it.
fn promote(call: &Call<'_>, value: &Value, rank: u8) -> Result<i128> {
    let above = 2 * i128::from(TIMESTAMP_INFINITY) - 1;
    let below = 2 * i128::from(TIMESTAMP_NEGATIVE_INFINITY) + 1;
    let ts = match *value {
        Value::Date(DATE_NEGATIVE_INFINITY) => TIMESTAMP_NEGATIVE_INFINITY,
        Value::Date(DATE_INFINITY) => TIMESTAMP_INFINITY,
        Value::Date(d) if d >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => return Ok(above),
        Value::Date(d) => i64::from(d) * USECS_PER_DAY,
        Value::Timestamp(t) | Value::TimestampTz(t) => t,
        _ => return Err(bad_value()),
    };
    if infinite(ts) || rank < 2 || matches!(value, Value::TimestampTz(_)) {
        return Ok(2 * i128::from(ts));
    }
    let zone = call.session.zone()?;
    let seconds = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
    let (west, _) = rupg_types::local_offset(seconds, zone.as_ref());
    let utc = ts + i64::from(west) * USECS_PER_SEC;
    Ok(if valid_timestamp(utc) {
        2 * i128::from(utc)
    } else if ts < 0 {
        below
    } else {
        above
    })
}

/// The order of a date, a timestamp and a `timestamptz` of two different types, or `None` for other values.
fn mixed_datetime(call: &Call<'_>, a: &Value, b: &Value) -> Result<Option<Ordering>> {
    let (Some(x), Some(y)) = (datetime_rank(a), datetime_rank(b)) else { return Ok(None) };
    if x == y {
        return Ok(None);
    }
    let rank = x.max(y);
    Ok(Some(promote(call, a, rank)?.cmp(&promote(call, b, rank)?)))
}

/// The order of two values of one type, or of two values of `date`, `timestamp` and `timestamptz`.
pub(crate) fn datetime_order(call: &Call<'_>, a: &Value, b: &Value) -> Result<Ordering> {
    match mixed_datetime(call, a, b)? {
        Some(order) => Ok(order),
        None => compare(0, a, 0, b),
    }
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

/// The number of an `xid`, or of the `int4` of `xideqint4` and `xidneqint4`, which compare the bits of the `int4` as an `xid`.
fn xid_number(value: &Value) -> Result<u32> {
    match value {
        Value::Oid(n) => Ok(*n),
        Value::Int4(n) => Ok(n.cast_unsigned()),
        _ => Err(bad_value()),
    }
}

/// `xideq` and `xidneq` of `xid.c`: the values are equal when the numbers are equal.
fn xid_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Bool(xid_number(a)? == xid_number(b)?))
}

fn xid_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Bool(xid_number(a)? != xid_number(b)?))
}

/// The kernel of `larger`, `smaller` and the B-tree `cmp` functions.
pub(crate) fn by_src(src: &str, proc: &ProcRow) -> Option<Kernel> {
    let [left, right] = proc.argtypes else { return None };
    // `xid` and `cid` have `=` and `<>` and no order, so they are not in a family.
    match src {
        "xideq" | "cideq" => return Some(xid_eq),
        "xidneq" => return Some(xid_ne),
        "record_eq" => return Some(|_, args| record_equal(args).map(Value::Bool)),
        "record_ne" => return Some(|_, args| record_equal(args).map(|eq| Value::Bool(!eq))),
        _ => {}
    }
    if !comparable(*left, *right) {
        return None;
    }
    // The operators `~<~`, `~<=~`, `~>=~` and `~>~` compare the bytes, as the comparison of text does here.
    match src {
        "text_pattern_lt" | "bpchar_pattern_lt" => return Some(lt),
        "text_pattern_le" | "bpchar_pattern_le" => return Some(le),
        "text_pattern_ge" | "bpchar_pattern_ge" => return Some(ge),
        "text_pattern_gt" | "bpchar_pattern_gt" => return Some(gt),
        _ => {}
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
    } else if src.starts_with("bt") && src.ends_with("cmp")
        || matches!(
            src,
            "array_cmp"
                | "cash_cmp"
                | "xid8cmp"
                | "macaddr_cmp"
                | "macaddr8_cmp"
                | "pg_lsn_cmp"
                | "range_cmp"
                | "multirange_cmp"
                | "bpcharcmp"
                | "enum_cmp"
                | "byteacmp"
                | "date_cmp"
                | "date_cmp_timestamp"
                | "date_cmp_timestamptz"
                | "interval_cmp"
                | "numeric_cmp"
                | "time_cmp"
                | "timestamp_cmp"
                | "timestamp_cmp_date"
                | "timestamp_cmp_timestamptz"
                | "timestamptz_cmp_date"
                | "timestamptz_cmp_timestamp"
                | "timetz_cmp"
                | "uuid_cmp"
        )
    {
        Some(cmp)
    } else {
        None
    }
}

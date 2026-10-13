//! The range types of `rangetypes.c`: `make_range` with the canonical functions of the discrete ranges, the order of the ranges, the constructors and the functions that read the bounds and the flags.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, RANGE_EMPTY, RANGE_LB_INC, RANGE_LB_INF, RANGE_UB_INC,
    RANGE_UB_INF, Range, Value, oid,
};

use crate::compare::compare;
use crate::io::base_type;
use crate::{Call, Kernel, bad_value};

/// The subtype of a range type, or 0 for a type that is not a range type.
pub(crate) fn subtype(range: u32) -> u32 {
    builtin::range_subtype(base_type(range)).unwrap_or(0)
}

/// One bound of a range, as `RangeBound` of `rangetypes.h` gives it.
struct Bound<'a> {
    value: &'a Value,
    infinite: bool,
    inclusive: bool,
    lower: bool,
}

/// The lower bound and the upper bound of a range.
fn bounds(range: &Range) -> (Bound<'_>, Bound<'_>) {
    let flags = range.flags;
    (
        Bound {
            value: &range.lower,
            infinite: flags & RANGE_LB_INF != 0,
            inclusive: flags & RANGE_LB_INC != 0,
            lower: true,
        },
        Bound {
            value: &range.upper,
            infinite: flags & RANGE_UB_INF != 0,
            inclusive: flags & RANGE_UB_INC != 0,
            lower: false,
        },
    )
}

/// The side of an infinite bound: a lower bound is below each value and an upper bound is above.
fn side(lower: bool) -> Ordering {
    if lower { Ordering::Less } else { Ordering::Greater }
}

/// `range_cmp_bound_values`: the order of the values of two bounds, with an infinite bound below or above each value. The inclusive flags have no effect.
fn cmp_bound_values(sub: u32, b1: &Bound<'_>, b2: &Bound<'_>) -> Result<Ordering> {
    Ok(match (b1.infinite, b2.infinite) {
        (true, true) if b1.lower == b2.lower => Ordering::Equal,
        (true, _) => side(b1.lower),
        (false, true) => side(b2.lower).reverse(),
        (false, false) => compare(sub, b1.value, sub, b2.value)?,
    })
}

/// `range_cmp_bounds`: the order of two bounds. For equal values, an exclusive lower bound is after the value and an exclusive upper bound is before it.
fn cmp_bounds(sub: u32, b1: &Bound<'_>, b2: &Bound<'_>) -> Result<Ordering> {
    let order = cmp_bound_values(sub, b1, b2)?;
    if order.is_ne() || b1.infinite || b2.infinite {
        return Ok(order);
    }
    Ok(match (b1.inclusive, b2.inclusive) {
        (false, false) if b1.lower == b2.lower => Ordering::Equal,
        (false, _) => side(b1.lower).reverse(),
        (true, false) => side(b2.lower),
        (true, true) => Ordering::Equal,
    })
}

/// `range_cmp`: an empty range is first, then the order of the lower bounds, then the order of the upper bounds. `sub` is the subtype.
pub(crate) fn range_order(sub: u32, a: &Range, b: &Range) -> Result<Ordering> {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        (false, false) => {}
    }
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    Ok(cmp_bounds(sub, &lower1, &lower2)?.then(cmp_bounds(sub, &upper1, &upper2)?))
}

/// `range_serialize`: an infinite bound has no value and is not inclusive. Two equal values give an empty range when a bound is exclusive.
fn serialize(sub: u32, flags: u8, lower: Value, upper: Value) -> Result<Range> {
    if flags & RANGE_EMPTY != 0 {
        return Ok(Range::empty());
    }
    let mut flags = flags & (RANGE_LB_INC | RANGE_UB_INC | RANGE_LB_INF | RANGE_UB_INF);
    let lower = if flags & RANGE_LB_INF != 0 {
        flags &= !RANGE_LB_INC;
        Value::Null
    } else {
        lower
    };
    let upper = if flags & RANGE_UB_INF != 0 {
        flags &= !RANGE_UB_INC;
        Value::Null
    } else {
        upper
    };
    if flags & (RANGE_LB_INF | RANGE_UB_INF) == 0 {
        match compare(sub, &lower, sub, &upper)? {
            Ordering::Greater => {
                return Err(Error::new(
                    SqlState::DATA_EXCEPTION,
                    "range lower bound must be less than or equal to range upper bound",
                ));
            }
            Ordering::Equal
                if flags & (RANGE_LB_INC | RANGE_UB_INC) != RANGE_LB_INC | RANGE_UB_INC =>
            {
                return Ok(Range::empty());
            }
            _ => {}
        }
    }
    Ok(Range { flags, lower, upper })
}

/// The next value of a discrete subtype: `int4range_canonical`, `int8range_canonical` and `daterange_canonical` add 1. An infinite date does not change.
fn next(value: &Value) -> Result<Value> {
    let out_of_range =
        |message: &str| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, message.to_string());
    match value {
        Value::Int4(v) => {
            v.checked_add(1).map(Value::Int4).ok_or_else(|| out_of_range("integer out of range"))
        }
        Value::Int8(v) => {
            v.checked_add(1).map(Value::Int8).ok_or_else(|| out_of_range("bigint out of range"))
        }
        Value::Date(v) if *v == DATE_INFINITY || *v == DATE_NEGATIVE_INFINITY => {
            Ok(Value::Date(*v))
        }
        Value::Date(v) => match v.checked_add(1) {
            Some(d) if crate::math::valid_date(i64::from(d)) => Ok(Value::Date(d)),
            _ => Err(Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range")),
        },
        _ => Err(bad_value()),
    }
}

/// True for a value that the canonical function of a date range leaves as it is.
fn infinite_date(value: &Value) -> bool {
    matches!(value, Value::Date(v) if *v == DATE_INFINITY || *v == DATE_NEGATIVE_INFINITY)
}

/// `make_range`: the range of the type `range` from the flags and the bounds, with the canonical form of `int4range`, `int8range` and `daterange`, which have an inclusive lower bound and an exclusive upper bound.
///
/// # Errors
///
/// `22000` when the lower bound is above the upper bound, and the error of a bound that has no next value.
pub(crate) fn make_range(range: u32, flags: u8, lower: Value, upper: Value) -> Result<Range> {
    let range_ty = base_type(range);
    let sub = subtype(range_ty);
    let mut out = serialize(sub, flags, lower, upper)?;
    if out.is_empty() || !matches!(range_ty, oid::INT4RANGE | oid::INT8RANGE | oid::DATERANGE) {
        return Ok(out);
    }
    let mut flags = out.flags;
    if out.has_lower() && flags & RANGE_LB_INC == 0 && !infinite_date(&out.lower) {
        out.lower = next(&out.lower)?;
        flags |= RANGE_LB_INC;
    }
    if out.has_upper() && flags & RANGE_UB_INC != 0 && !infinite_date(&out.upper) {
        out.upper = next(&out.upper)?;
        flags &= !RANGE_UB_INC;
    }
    serialize(sub, flags, out.lower, out.upper)
}

/// The range of a call.
fn arg(args: &[Value]) -> Result<&Range> {
    match args.first() {
        Some(Value::Range(range)) => Ok(range),
        _ => Err(bad_value()),
    }
}

/// `range_parse_flags`: the flags of the text `[]`, `[)`, `(]` or `()`.
fn parse_flags(text: &str) -> Result<u8> {
    let b = text.as_bytes();
    let (lower, upper) = match b {
        [l @ (b'[' | b'('), u @ (b']' | b')')] => (*l, *u),
        _ => {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "invalid range bound flags")
                .with_hint("Valid values are \"[]\", \"[)\", \"(]\", and \"()\"."));
        }
    };
    Ok(if lower == b'[' { RANGE_LB_INC } else { 0 } | if upper == b']' { RANGE_UB_INC } else { 0 })
}

/// The flags of a bound that is null, which is infinite.
fn infinite(value: &Value, flag: u8) -> u8 {
    if value.is_null() { flag } else { 0 }
}

/// `range_constructor2`: a range with an inclusive lower bound and an exclusive upper bound. A null bound is infinite.
fn constructor2(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [lower, upper] = args else { return Err(bad_value()) };
    let flags = RANGE_LB_INC | infinite(lower, RANGE_LB_INF) | infinite(upper, RANGE_UB_INF);
    Ok(Value::Range(Box::new(make_range(call.ret, flags, lower.clone(), upper.clone())?)))
}

/// `range_constructor3`: a range with the bound flags of the third argument.
fn constructor3(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [lower, upper, text] = args else { return Err(bad_value()) };
    let text = match text {
        Value::Text(text) => text,
        Value::Null => {
            return Err(Error::new(
                SqlState::DATA_EXCEPTION,
                "range constructor flags argument must not be null",
            ));
        }
        _ => return Err(bad_value()),
    };
    let flags = parse_flags(text)? | infinite(lower, RANGE_LB_INF) | infinite(upper, RANGE_UB_INF);
    Ok(Value::Range(Box::new(make_range(call.ret, flags, lower.clone(), upper.clone())?)))
}

/// `range_lower`: the lower bound, or null for an empty range or an infinite bound.
fn lower(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let range = arg(args)?;
    Ok(if range.has_lower() { range.lower.clone() } else { Value::Null })
}

/// `range_upper`: the upper bound, or null for an empty range or an infinite bound.
fn upper(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let range = arg(args)?;
    Ok(if range.has_upper() { range.upper.clone() } else { Value::Null })
}

/// The kernel of a function that tests a flag of the range.
macro_rules! flag_test {
    ($name:ident, $flag:expr, $doc:literal) => {
        #[doc = $doc]
        fn $name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
            Ok(Value::Bool(arg(args)?.flags & $flag != 0))
        }
    };
}

flag_test!(is_empty, RANGE_EMPTY, "`range_empty`: true for an empty range.");
flag_test!(
    lower_inc,
    RANGE_LB_INC,
    "`range_lower_inc`: true when the lower bound is part of the range."
);
flag_test!(
    upper_inc,
    RANGE_UB_INC,
    "`range_upper_inc`: true when the upper bound is part of the range."
);
flag_test!(lower_inf, RANGE_LB_INF, "`range_lower_inf`: true when the range has no lower bound.");
flag_test!(upper_inf, RANGE_UB_INF, "`range_upper_inf`: true when the range has no upper bound.");

/// The kernel of a range function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "range_constructor2" => constructor2,
        "range_constructor3" => constructor3,
        "range_lower" => lower,
        "range_upper" => upper,
        "range_empty" => is_empty,
        "range_lower_inc" => lower_inc,
        "range_upper_inc" => upper_inc,
        "range_lower_inf" => lower_inf,
        "range_upper_inf" => upper_inf,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int4(flags: u8, lower: i32, upper: i32) -> Result<Range> {
        make_range(oid::INT4RANGE, flags, Value::Int4(lower), Value::Int4(upper))
    }

    #[test]
    fn canonical() {
        let range = int4(RANGE_UB_INC, 1, 10).unwrap();
        assert_eq!(
            (range.flags, range.lower, range.upper),
            (RANGE_LB_INC, Value::Int4(2), Value::Int4(11))
        );
        assert!(int4(0, 5, 6).unwrap().is_empty());
        assert!(int4(RANGE_LB_INC, 5, 5).unwrap().is_empty());
        assert!(!int4(RANGE_LB_INC | RANGE_UB_INC, 5, 5).unwrap().is_empty());
        assert_eq!(int4(RANGE_LB_INC, 10, 1).unwrap_err().state(), SqlState::DATA_EXCEPTION);
        assert_eq!(int4(RANGE_UB_INC, 1, i32::MAX).unwrap_err().message(), "integer out of range");
        let range =
            make_range(oid::INT4RANGE, RANGE_LB_INF | RANGE_LB_INC, Value::Null, Value::Int4(3));
        assert_eq!(range.unwrap().flags, RANGE_LB_INF);
    }

    #[test]
    fn order() {
        let a = int4(RANGE_LB_INC, 1, 10).unwrap();
        let b = int4(RANGE_LB_INC | RANGE_UB_INC, 1, 9).unwrap();
        assert_eq!(range_order(oid::INT4, &a, &b).unwrap(), Ordering::Equal);
        let empty = Range::empty();
        assert_eq!(range_order(oid::INT4, &empty, &a).unwrap(), Ordering::Less);
        let all =
            Range { flags: RANGE_LB_INF | RANGE_UB_INF, lower: Value::Null, upper: Value::Null };
        assert_eq!(range_order(oid::INT4, &all, &a).unwrap(), Ordering::Less);
        let open =
            Range { flags: RANGE_LB_INC | RANGE_UB_INF, lower: Value::Int4(1), upper: Value::Null };
        assert_eq!(range_order(oid::INT4, &open, &a).unwrap(), Ordering::Greater);
    }
}

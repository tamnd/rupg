//! The range types of `rangetypes.c`: `make_range` with the canonical functions of the discrete ranges, the order of the ranges, the constructors, the functions that read the bounds and the flags, the operators of the ranges and the subtype difference functions.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, RANGE_EMPTY, RANGE_LB_INC, RANGE_LB_INF, RANGE_UB_INC,
    RANGE_UB_INF, Range, Value, oid,
};

use crate::compare::compare;
use crate::io::base_type;
use crate::{Call, Kernel, bad_value, type_error};

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

/// The range type of a call: the first argument or the result that has a range type.
fn range_type(call: &Call<'_>) -> u32 {
    call.args.iter().chain([&call.ret]).copied().find(|&ty| subtype(ty) != 0).unwrap_or(0)
}

/// The range type of a call and its two ranges.
fn pair<'a>(call: &Call<'_>, args: &'a [Value]) -> Result<(u32, &'a Range, &'a Range)> {
    match args {
        [Value::Range(a), Value::Range(b)] => Ok((range_type(call), a, b)),
        _ => Err(bad_value()),
    }
}

/// The range of two bounds, as `make_range` makes it from two `RangeBound` values.
fn from_bounds(range: u32, lower: &Bound<'_>, upper: &Bound<'_>) -> Result<Range> {
    let flag = |set: bool, flag: u8| if set { flag } else { 0 };
    let flags = flag(lower.inclusive, RANGE_LB_INC)
        | flag(lower.infinite, RANGE_LB_INF)
        | flag(upper.inclusive, RANGE_UB_INC)
        | flag(upper.infinite, RANGE_UB_INF);
    make_range(range, flags, lower.value.clone(), upper.value.clone())
}

/// The bound on the other side of the same value: a lower bound becomes an upper bound and the inclusive flag changes.
fn flip<'a>(bound: &Bound<'a>) -> Bound<'a> {
    Bound {
        value: bound.value,
        infinite: bound.infinite,
        inclusive: !bound.inclusive,
        lower: !bound.lower,
    }
}

/// `range_overlaps_internal`: true when the ranges have a common value.
fn overlaps(range: u32, a: &Range, b: &Range) -> Result<bool> {
    if a.is_empty() || b.is_empty() {
        return Ok(false);
    }
    let sub = subtype(range);
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    Ok((cmp_bounds(sub, &lower1, &lower2)?.is_ge() && cmp_bounds(sub, &lower1, &upper2)?.is_le())
        || (cmp_bounds(sub, &lower2, &lower1)?.is_ge()
            && cmp_bounds(sub, &lower2, &upper1)?.is_le()))
}

/// `range_contains_internal`: true when each value of `b` is in `a`. Each range contains an empty range.
fn contains(range: u32, a: &Range, b: &Range) -> Result<bool> {
    if b.is_empty() {
        return Ok(true);
    }
    if a.is_empty() {
        return Ok(false);
    }
    let sub = subtype(range);
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    Ok(cmp_bounds(sub, &lower1, &lower2)?.is_le() && cmp_bounds(sub, &upper1, &upper2)?.is_ge())
}

/// `range_contained_by_internal`: true when each value of `a` is in `b`.
fn contained_by(range: u32, a: &Range, b: &Range) -> Result<bool> {
    contains(range, b, a)
}

/// The order of a bound of `a` and a bound of `b`, or `None` when a range is empty. An empty range is not before, after or next to a range.
fn order_of(
    range: u32,
    a: &Range,
    b: &Range,
    upper_a: bool,
    upper_b: bool,
) -> Result<Option<Ordering>> {
    if a.is_empty() || b.is_empty() {
        return Ok(None);
    }
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    let x = if upper_a { upper1 } else { lower1 };
    let y = if upper_b { upper2 } else { lower2 };
    cmp_bounds(subtype(range), &x, &y).map(Some)
}

/// `range_before_internal`: true when the upper bound of `a` is before the lower bound of `b`.
pub(crate) fn before(range: u32, a: &Range, b: &Range) -> Result<bool> {
    Ok(order_of(range, a, b, true, false)?.is_some_and(Ordering::is_lt))
}

/// `range_after_internal`: true when the lower bound of `a` is after the upper bound of `b`.
fn after(range: u32, a: &Range, b: &Range) -> Result<bool> {
    Ok(order_of(range, a, b, false, true)?.is_some_and(Ordering::is_gt))
}

/// `range_overleft_internal`: true when `a` does not go to the right of `b`.
fn over_left(range: u32, a: &Range, b: &Range) -> Result<bool> {
    Ok(order_of(range, a, b, true, true)?.is_some_and(Ordering::is_le))
}

/// The internal function of the operator `&>`: true when `a` does not go to the left of `b`.
fn over_right(range: u32, a: &Range, b: &Range) -> Result<bool> {
    Ok(order_of(range, a, b, false, false)?.is_some_and(Ordering::is_ge))
}

/// `bounds_adjacent`: true when no value is between the upper bound `a` and the lower bound `b` and no value is in both. For a discrete subtype, the range between the bounds must be empty.
fn bounds_adjacent(range: u32, a: &Bound<'_>, b: &Bound<'_>) -> Result<bool> {
    Ok(match cmp_bound_values(subtype(range), a, b)? {
        Ordering::Less => {
            if !matches!(base_type(range), oid::INT4RANGE | oid::INT8RANGE | oid::DATERANGE) {
                return Ok(false);
            }
            from_bounds(range, &flip(a), &flip(b))?.is_empty()
        }
        Ordering::Equal => a.inclusive != b.inclusive,
        Ordering::Greater => false,
    })
}

/// `range_adjacent_internal`: true when one range stops where the other range starts. An empty range is not adjacent to a range.
pub(crate) fn adjacent(range: u32, a: &Range, b: &Range) -> Result<bool> {
    if a.is_empty() || b.is_empty() {
        return Ok(false);
    }
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    Ok(bounds_adjacent(range, &upper1, &lower2)? || bounds_adjacent(range, &upper2, &lower1)?)
}

/// `range_union_internal`: the smallest range that contains the two ranges. When `strict` is true, the ranges must overlap or be adjacent.
pub(crate) fn union(range: u32, a: &Range, b: &Range, strict: bool) -> Result<Range> {
    if a.is_empty() {
        return Ok(b.clone());
    }
    if b.is_empty() {
        return Ok(a.clone());
    }
    if strict && !overlaps(range, a, b)? && !adjacent(range, a, b)? {
        return Err(Error::new(
            SqlState::DATA_EXCEPTION,
            "result of range union would not be contiguous",
        ));
    }
    let sub = subtype(range);
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    let lower = if cmp_bounds(sub, &lower1, &lower2)?.is_lt() { lower1 } else { lower2 };
    let upper = if cmp_bounds(sub, &upper1, &upper2)?.is_gt() { upper1 } else { upper2 };
    from_bounds(range, &lower, &upper)
}

/// `range_intersect_internal`: the values that are in the two ranges.
fn intersect(range: u32, a: &Range, b: &Range) -> Result<Range> {
    if !overlaps(range, a, b)? {
        return Ok(Range::empty());
    }
    let sub = subtype(range);
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    let lower = if cmp_bounds(sub, &lower1, &lower2)?.is_ge() { lower1 } else { lower2 };
    let upper = if cmp_bounds(sub, &upper1, &upper2)?.is_le() { upper1 } else { upper2 };
    from_bounds(range, &lower, &upper)
}

/// `range_minus_multi_internal`: the ranges of the values of `a` that are not in `b`, in order. The result has no empty range.
fn minus_multi(range: u32, a: &Range, b: &Range) -> Result<Vec<Range>> {
    if a.is_empty() {
        return Ok(Vec::new());
    }
    if b.is_empty() {
        return Ok(vec![a.clone()]);
    }
    let sub = subtype(range);
    let (lower1, upper1) = bounds(a);
    let (lower2, upper2) = bounds(b);
    let l1l2 = cmp_bounds(sub, &lower1, &lower2)?;
    let l1u2 = cmp_bounds(sub, &lower1, &upper2)?;
    let u1l2 = cmp_bounds(sub, &upper1, &lower2)?;
    let u1u2 = cmp_bounds(sub, &upper1, &upper2)?;
    Ok(if l1l2.is_lt() && u1u2.is_gt() {
        vec![
            from_bounds(range, &lower1, &flip(&lower2))?,
            from_bounds(range, &flip(&upper2), &upper1)?,
        ]
    } else if l1u2.is_gt() || u1l2.is_lt() {
        vec![a.clone()]
    } else if l1l2.is_ge() && u1u2.is_le() {
        Vec::new()
    } else if l1l2.is_le() && u1l2.is_ge() && u1u2.is_le() {
        vec![from_bounds(range, &lower1, &flip(&lower2))?]
    } else if l1l2.is_ge() && u1u2.is_ge() && l1u2.is_le() {
        vec![from_bounds(range, &flip(&upper2), &upper1)?]
    } else {
        return Err(Error::new(SqlState::INTERNAL_ERROR, "unexpected case in range_minus_multi"));
    })
}

/// `range_minus_internal`: the values of `a` that are not in `b`. The result must be one range.
fn minus(range: u32, a: &Range, b: &Range) -> Result<Range> {
    if a.is_empty() || b.is_empty() {
        return Ok(a.clone());
    }
    let mut parts = minus_multi(range, a, b)?;
    if parts.len() > 1 {
        return Err(Error::new(
            SqlState::DATA_EXCEPTION,
            "result of range difference would not be contiguous",
        ));
    }
    Ok(parts.pop().unwrap_or_else(Range::empty))
}

/// The kernel of an operator that tests two ranges.
macro_rules! range_test {
    ($name:ident, $test:ident, $doc:literal) => {
        #[doc = $doc]
        fn $name(call: &Call<'_>, args: &[Value]) -> Result<Value> {
            let (range, a, b) = pair(call, args)?;
            Ok(Value::Bool($test(range, a, b)?))
        }
    };
}

range_test!(overlaps_op, overlaps, "`range_overlaps`: the operator `&&`.");
range_test!(contains_op, contains, "`range_contains`: the operator `@>` of two ranges.");
range_test!(
    contained_by_op,
    contained_by,
    "`range_contained_by`: the operator `<@` of two ranges."
);
range_test!(before_op, before, "`range_before`: the operator `<<`.");
range_test!(after_op, after, "`range_after`: the operator `>>`.");
range_test!(over_left_op, over_left, "`range_overleft`: the operator `&<`.");
range_test!(over_right_op, over_right, "The function of the operator `&>`.");
range_test!(adjacent_op, adjacent, "`range_adjacent`: the operator `-|-`.");

/// `range_union`: the operator `+`.
fn union_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (range, a, b) = pair(call, args)?;
    Ok(Value::Range(Box::new(union(range, a, b, true)?)))
}

/// `range_merge`: the smallest range that contains the two ranges, also when they have a gap between them.
fn merge(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (range, a, b) = pair(call, args)?;
    Ok(Value::Range(Box::new(union(range, a, b, false)?)))
}

/// `range_intersect`: the operator `*`.
fn intersect_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (range, a, b) = pair(call, args)?;
    Ok(Value::Range(Box::new(intersect(range, a, b)?)))
}

/// `range_minus`: the operator `-`.
fn minus_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (range, a, b) = pair(call, args)?;
    Ok(Value::Range(Box::new(minus(range, a, b)?)))
}

/// `range_minus_multi`: one row for each range of the values of the first range that are not in the second range.
pub(crate) fn range_minus_multi(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let (range, a, b) = pair(call, args)?;
    Ok(minus_multi(range, a, b)?.into_iter().map(|r| vec![Value::Range(Box::new(r))]).collect())
}

/// `range_contains_elem`: the operator `@>` of a range and a value.
fn contains_elem_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Range(range), value] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_elem(subtype(range_type(call)), range, value)?))
}

/// `elem_contained_by_range`: the operator `<@` of a value and a range.
fn elem_contained_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [value, Value::Range(range)] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_elem(subtype(range_type(call)), range, value)?))
}

/// `range_contains_elem_internal`: true when the value is in the range.
fn contains_elem(sub: u32, range: &Range, value: &Value) -> Result<bool> {
    if range.is_empty() {
        return Ok(false);
    }
    let (lower, upper) = bounds(range);
    if !lower.infinite {
        match compare(sub, lower.value, sub, value)? {
            Ordering::Greater => return Ok(false),
            Ordering::Equal if !lower.inclusive => return Ok(false),
            _ => {}
        }
    }
    if !upper.infinite {
        match compare(sub, upper.value, sub, value)? {
            Ordering::Less => return Ok(false),
            Ordering::Equal if !upper.inclusive => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

/// The subtype difference functions of the built-in range types: the first value minus the second value as `float8`. A timestamp difference is in seconds.
fn subdiff(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(match args {
        [Value::Int4(a), Value::Int4(b)] | [Value::Date(a), Value::Date(b)] => {
            f64::from(*a) - f64::from(*b)
        }
        [Value::Int8(a), Value::Int8(b)] => *a as f64 - *b as f64,
        [Value::Timestamp(a), Value::Timestamp(b)]
        | [Value::TimestampTz(a), Value::TimestampTz(b)] => (*a as f64 - *b as f64) / 1_000_000.0,
        [Value::Numeric(a), Value::Numeric(b)] => {
            let diff = a.sub(b).map_err(type_error)?;
            return crate::cast::numeric_to_float(call, &[Value::Numeric(diff)]);
        }
        _ => return Err(bad_value()),
    }))
}

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
        "range_overlaps" => overlaps_op,
        "range_contains" => contains_op,
        "range_contained_by" => contained_by_op,
        "range_contains_elem" => contains_elem_op,
        "elem_contained_by_range" => elem_contained_op,
        "range_before" => before_op,
        "range_after" => after_op,
        "range_overleft" => over_left_op,
        "range_adjacent" => adjacent_op,
        "range_union" => union_op,
        "range_merge" => merge,
        "range_intersect" => intersect_op,
        "range_minus" => minus_op,
        "int4range_subdiff" | "int8range_subdiff" | "numrange_subdiff" | "daterange_subdiff"
        | "tsrange_subdiff" | "tstzrange_subdiff" => subdiff,
        _ if src == concat!("range_over", "right") => over_right_op,
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

    #[test]
    fn operators() {
        let r = oid::INT4RANGE;
        let a = int4(RANGE_LB_INC, 1, 5).unwrap();
        let b = int4(RANGE_LB_INC, 5, 8).unwrap();
        let c = int4(RANGE_LB_INC, 3, 8).unwrap();
        assert!(adjacent(r, &a, &b).unwrap() && adjacent(r, &b, &a).unwrap());
        assert!(!adjacent(r, &a, &c).unwrap() && overlaps(r, &a, &c).unwrap());
        assert!(before(r, &a, &b).unwrap() && !before(r, &a, &c).unwrap());
        let both = union(r, &a, &b, true).unwrap();
        assert_eq!((both.lower, both.upper), (Value::Int4(1), Value::Int4(8)));
        let far = int4(RANGE_LB_INC, 7, 9).unwrap();
        assert_eq!(union(r, &a, &far, true).unwrap_err().state(), SqlState::DATA_EXCEPTION);
        assert!(intersect(r, &a, &b).unwrap().is_empty());
        let wide = int4(RANGE_LB_INC, 0, 10).unwrap();
        assert_eq!(minus_multi(r, &wide, &c).unwrap().len(), 2);
        assert_eq!(minus(r, &wide, &c).unwrap_err().state(), SqlState::DATA_EXCEPTION);
        assert!(contains_elem(oid::INT4, &a, &Value::Int4(4)).unwrap());
        assert!(!contains_elem(oid::INT4, &a, &Value::Int4(5)).unwrap());
    }
}

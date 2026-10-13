//! The multirange types of `multirangetypes.c`: `make_multirange`, which sorts the ranges and merges the ranges that overlap or touch, the order of the multiranges, the constructors, the functions that read the bounds and the flags, `range_merge` of a multirange, `unnest` and the operators of a multirange with a multirange, a range or a value.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{RANGE_LB_INC, RANGE_LB_INF, RANGE_UB_INC, RANGE_UB_INF, Range, Value};

use crate::io::base_type;
use crate::range::{
    Bound, adjacent, before, bounds, bounds_adjacent, cmp_bounds, contains, contains_elem,
    intersect, make_range, minus, over_left, overlaps, range_order, split, subtype, union,
};
use crate::{Call, Kernel, bad_value};

/// The range type of a multirange type, or 0 for a type that is not a multirange type.
pub(crate) fn range_of(multirange: u32) -> u32 {
    builtin::multirange_range(base_type(multirange)).unwrap_or(0)
}

/// `make_multirange` with `multirange_canonicalize`: the ranges in the order of `range_cmp` with no empty range, and one range for each group of ranges that overlap or touch. `range` is the range type.
///
/// # Errors
///
/// The errors of the comparison of the bounds.
pub(crate) fn make_multirange(range: u32, mut ranges: Vec<Range>) -> Result<Vec<Range>> {
    let sub = subtype(range);
    let mut failed = None;
    ranges.sort_by(|a, b| {
        range_order(sub, a, b).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            Ordering::Equal
        })
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let mut out: Vec<Range> = Vec::with_capacity(ranges.len());
    for current in ranges {
        if current.is_empty() {
            continue;
        }
        let Some(last) = out.last_mut() else {
            out.push(current);
            continue;
        };
        if adjacent(range, last, &current)? {
            *last = union(range, last, &current, false)?;
        } else if before(range, last, &current)? {
            out.push(current);
        } else {
            *last = union(range, last, &current, true)?;
        }
    }
    Ok(out)
}

/// `multirange_cmp`: the order of the ranges one by one, and a multirange with fewer ranges is first when its ranges are the first ranges of the other. `sub` is the subtype.
pub(crate) fn multirange_order(sub: u32, a: &[Range], b: &[Range]) -> Result<Ordering> {
    for (x, y) in a.iter().zip(b) {
        let order = range_order(sub, x, y)?;
        if order.is_ne() {
            return Ok(order);
        }
    }
    Ok(a.len().cmp(&b.len()))
}

/// The ranges of a call.
fn arg(args: &[Value]) -> Result<&[Range]> {
    match args.first() {
        Some(Value::Multirange(ranges)) => Ok(ranges),
        _ => Err(bad_value()),
    }
}

/// `multirange_constructor0`: the empty multirange.
fn constructor0(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Multirange(Vec::new()))
}

/// `multirange_constructor1`: the multirange of one range. The cast from a range type to its multirange type calls it.
fn constructor1(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Range(range)] = args else { return Err(bad_value()) };
    Ok(Value::Multirange(make_multirange(range_of(call.ret), vec![(**range).clone()])?))
}

/// `multirange_constructor2`: the multirange of the ranges of a `VARIADIC` array. The array must have one dimension or none and no null element.
fn constructor2(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Array(array)] = args else { return Err(bad_value()) };
    if array.dims.len() > 1 {
        return Err(Error::new(
            SqlState::CARDINALITY_VIOLATION,
            "multiranges cannot be constructed from multidimensional arrays",
        ));
    }
    let ranges = array
        .values
        .iter()
        .map(|value| match value {
            Some(Value::Range(range)) => Ok((**range).clone()),
            Some(_) => Err(bad_value()),
            None => Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "multirange values cannot contain null members",
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Multirange(make_multirange(range_of(call.ret), ranges)?))
}

/// `multirange_lower`: the lower bound of the first range, or null for an empty multirange or an infinite bound.
fn lower(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(arg(args)?.first().map_or(Value::Null, |range| range.lower.clone()))
}

/// `multirange_upper`: the upper bound of the last range, or null for an empty multirange or an infinite bound.
fn upper(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(arg(args)?.last().map_or(Value::Null, |range| range.upper.clone()))
}

/// `multirange_empty`: true for a multirange with no range.
fn is_empty(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(arg(args)?.is_empty()))
}

/// The kernel of a function that tests a flag of the first or the last range. An empty multirange gives false.
macro_rules! flag_test {
    ($name:ident, $end:ident, $flag:expr, $doc:literal) => {
        #[doc = $doc]
        fn $name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
            Ok(Value::Bool(arg(args)?.$end().is_some_and(|range| range.flags & $flag != 0)))
        }
    };
}

flag_test!(
    lower_inc,
    first,
    RANGE_LB_INC,
    "`multirange_lower_inc`: true when the lower bound is part of the multirange."
);
flag_test!(
    upper_inc,
    last,
    RANGE_UB_INC,
    "`multirange_upper_inc`: true when the upper bound is part of the multirange."
);
flag_test!(
    lower_inf,
    first,
    RANGE_LB_INF,
    "`multirange_lower_inf`: true when the multirange has no lower bound."
);
flag_test!(
    upper_inf,
    last,
    RANGE_UB_INF,
    "`multirange_upper_inf`: true when the multirange has no upper bound."
);

/// `range_merge_from_multirange`: the smallest range that contains each range of the multirange.
fn merge(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ranges = arg(args)?;
    let range = match ranges {
        [] => Range::empty(),
        [one] => one.clone(),
        [first, .., last] => {
            let flags = (first.flags & (RANGE_LB_INC | RANGE_LB_INF))
                | (last.flags & (RANGE_UB_INC | RANGE_UB_INF));
            make_range(call.ret, flags, first.lower.clone(), last.upper.clone())?
        }
    };
    Ok(Value::Range(Box::new(range)))
}

/// `multirange_unnest`: one row for each range of the multirange, in order.
pub(crate) fn unnest(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(arg(args)?.iter().map(|range| vec![Value::Range(Box::new(range.clone()))]).collect())
}

/// The range type of a call: the range type of the first argument or of the result that is a multirange type, or the first that is a range type.
fn call_range(call: &Call<'_>) -> u32 {
    let types = || call.args.iter().chain([&call.ret]).copied();
    types()
        .map(range_of)
        .find(|&ty| ty != 0)
        .or_else(|| types().find(|&ty| subtype(ty) != 0))
        .unwrap_or(0)
}

/// The ranges of a range or a multirange argument. An empty range gives no range.
fn ranges_of(value: &Value) -> Result<&[Range]> {
    match value {
        Value::Multirange(ranges) => Ok(ranges),
        Value::Range(range) if range.is_empty() => Ok(&[]),
        Value::Range(range) => Ok(std::slice::from_ref(&**range)),
        _ => Err(bad_value()),
    }
}

/// The lower bound of the first range and the upper bound of the last range, or `None` for no range.
fn span(ranges: &[Range]) -> Option<(Bound<'_>, Bound<'_>)> {
    let (first, last) = (ranges.first()?, ranges.last()?);
    Some((bounds(first).0, bounds(last).1))
}

/// `range_overlaps_multirange_internal` and `multirange_overlaps_multirange_internal`: true when a range of `a` and a range of `b` have a common value. The ranges of each side are in order and do not overlap, so one pass finds the pair.
fn overlaps_all(range: u32, a: &[Range], b: &[Range]) -> Result<bool> {
    let sub = subtype(range);
    let (mut i, mut j) = (0, 0);
    while let (Some(x), Some(y)) = (a.get(i), b.get(j)) {
        if overlaps(range, x, y)? {
            return Ok(true);
        }
        if cmp_bounds(sub, &bounds(x).1, &bounds(y).1)?.is_lt() {
            i += 1;
        } else {
            j += 1;
        }
    }
    Ok(false)
}

/// `multirange_contains_multirange_internal`: true when each range of `b` is in one range of `a`. No range in `b` gives true.
fn contains_all(range: u32, a: &[Range], b: &[Range]) -> Result<bool> {
    let sub = subtype(range);
    let mut i = 0;
    for y in b {
        let lower = bounds(y).0;
        loop {
            let Some(x) = a.get(i) else { return Ok(false) };
            if cmp_bounds(sub, &bounds(x).1, &lower)?.is_ge() {
                break;
            }
            i += 1;
        }
        if !contains(range, &a[i], y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The kernel of an operator that compares the outer bounds of two ranges or multiranges. An empty side gives false.
macro_rules! position {
    ($name:ident, $doc:literal, |$sub:pat, $range:ident, $a:ident, $b:ident| $test:expr) => {
        #[doc = $doc]
        fn $name(call: &Call<'_>, args: &[Value]) -> Result<Value> {
            let [a, b] = args else { return Err(bad_value()) };
            let $range = call_range(call);
            let $sub = subtype($range);
            let (Some($a), Some($b)) = (span(ranges_of(a)?), span(ranges_of(b)?)) else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool($test))
        }
    };
}

position!(
    before_op,
    "`<<`: true when the upper bound of the left side is before the lower bound of the right side.",
    |sub, range, a, b| cmp_bounds(sub, &a.1, &b.0)?.is_lt()
);
position!(
    after_op,
    "`>>`: true when the lower bound of the left side is after the upper bound of the right side.",
    |sub, range, a, b| cmp_bounds(sub, &a.0, &b.1)?.is_gt()
);
position!(
    over_left_op,
    "`&<`: true when the left side does not go to the right of the right side.",
    |sub, range, a, b| cmp_bounds(sub, &a.1, &b.1)?.is_le()
);
position!(
    over_right_op,
    "`&>`: true when the left side does not go to the left of the right side.",
    |sub, range, a, b| cmp_bounds(sub, &a.0, &b.0)?.is_ge()
);
position!(
    adjacent_op,
    "`-|-`: true when the upper bound of one side touches the lower bound of the other side. Only the outer bounds count, as in PostgreSQL.",
    |_, range, a, b| bounds_adjacent(range, &a.1, &b.0)? || bounds_adjacent(range, &b.1, &a.0)?
);

/// `&&` of a range or a multirange and a multirange.
fn overlaps_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Bool(overlaps_all(call_range(call), ranges_of(a)?, ranges_of(b)?)?))
}

/// `@>` of a range or a multirange and a multirange, or of a multirange and a range.
fn contains_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_all(call_range(call), ranges_of(a)?, ranges_of(b)?)?))
}

/// `<@` of a range or a multirange and a multirange, or of a multirange and a range.
fn contained_by_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_all(call_range(call), ranges_of(b)?, ranges_of(a)?)?))
}

/// `multirange_contains_elem_internal`: true when a range of the multirange has the value.
fn contains_value(range: u32, ranges: &[Range], value: &Value) -> Result<bool> {
    let sub = subtype(range);
    for x in ranges {
        if contains_elem(sub, x, value)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `multirange_contains_elem`: `@>` of a multirange and a value.
fn contains_elem_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, value] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_value(call_range(call), ranges_of(a)?, value)?))
}

/// `elem_contained_by_multirange`: `<@` of a value and a multirange.
fn elem_contained_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [value, a] = args else { return Err(bad_value()) };
    Ok(Value::Bool(contains_value(call_range(call), ranges_of(a)?, value)?))
}

/// The ranges of two multirange arguments.
fn pair(args: &[Value]) -> Result<(&[Range], &[Range])> {
    match args {
        [Value::Multirange(a), Value::Multirange(b)] => Ok((a, b)),
        _ => Err(bad_value()),
    }
}

/// `multirange_union`: the ranges of the two multiranges, merged.
fn union_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = pair(args)?;
    if a.is_empty() || b.is_empty() {
        return Ok(Value::Multirange(if a.is_empty() { b } else { a }.to_vec()));
    }
    Ok(Value::Multirange(make_multirange(call_range(call), [a, b].concat())?))
}

/// `multirange_minus_internal`: the values of `a` that are not in `b`. Each range of `a` loses the ranges of `b` that overlap it, in one pass over the two multiranges.
fn minus_all(range: u32, a: &[Range], b: &[Range]) -> Result<Vec<Range>> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut j = 0;
    for x in a {
        let mut x = x.clone();
        while let Some(y) = b.get(j)
            && before(range, y, &x)?
        {
            j += 1;
        }
        while let Some(y) = b.get(j) {
            if let Some((left, right)) = split(range, &x, y)? {
                out.push(left);
                x = right;
                j += 1;
            } else if overlaps(range, &x, y)? {
                x = minus(range, &x, y)?;
                if x.is_empty() || before(range, &x, y)? {
                    break;
                }
                j += 1;
            } else {
                break;
            }
        }
        out.push(x);
    }
    make_multirange(range, out)
}

/// `multirange_minus`: the values of the left multirange that are not in the right one.
fn minus_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = pair(args)?;
    if a.is_empty() || b.is_empty() {
        return Ok(Value::Multirange(a.to_vec()));
    }
    Ok(Value::Multirange(minus_all(call_range(call), a, b)?))
}

/// `multirange_minus_multi`: the difference as a set function, with no row for an empty result.
pub(crate) fn minus_multi(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let (a, b) = pair(args)?;
    let out =
        if a.is_empty() || b.is_empty() { a.to_vec() } else { minus_all(call_range(call), a, b)? };
    Ok(if out.is_empty() { Vec::new() } else { vec![vec![Value::Multirange(out)]] })
}

/// `multirange_intersect_internal`: the values that are in the two multiranges, in one pass over them.
fn intersect_all(range: u32, a: &[Range], b: &[Range]) -> Result<Vec<Range>> {
    let mut out = Vec::new();
    let mut j = 0;
    for x in a {
        while let Some(y) = b.get(j)
            && before(range, y, x)?
        {
            j += 1;
        }
        while let Some(y) = b.get(j) {
            if !overlaps(range, x, y)? {
                break;
            }
            out.push(intersect(range, x, y)?);
            if !over_left(range, y, x)? {
                break;
            }
            j += 1;
        }
        if j >= b.len() {
            break;
        }
    }
    make_multirange(range, out)
}

/// `multirange_intersect`: the values that are in the two multiranges.
fn intersect_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = pair(args)?;
    if a.is_empty() || b.is_empty() {
        return Ok(Value::Multirange(Vec::new()));
    }
    Ok(Value::Multirange(intersect_all(call_range(call), a, b)?))
}

/// The kernel of a multirange function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "multirange_constructor0" => constructor0,
        "multirange_constructor1" => constructor1,
        "multirange_constructor2" => constructor2,
        "multirange_lower" => lower,
        "multirange_upper" => upper,
        "multirange_empty" => is_empty,
        "multirange_lower_inc" => lower_inc,
        "multirange_upper_inc" => upper_inc,
        "multirange_lower_inf" => lower_inf,
        "multirange_upper_inf" => upper_inf,
        "range_merge_from_multirange" => merge,
        "multirange_union" => union_op,
        "multirange_minus" => minus_op,
        "multirange_intersect" => intersect_op,
        "range_overlaps_multirange"
        | "multirange_overlaps_range"
        | "multirange_overlaps_multirange" => overlaps_op,
        "multirange_contains_range"
        | "range_contains_multirange"
        | "multirange_contains_multirange" => contains_op,
        "range_contained_by_multirange"
        | "multirange_contained_by_range"
        | "multirange_contained_by_multirange" => contained_by_op,
        "multirange_contains_elem" => contains_elem_op,
        "elem_contained_by_multirange" => elem_contained_op,
        "range_before_multirange" | "multirange_before_range" | "multirange_before_multirange" => {
            before_op
        }
        "range_after_multirange" | "multirange_after_range" | "multirange_after_multirange" => {
            after_op
        }
        "range_overleft_multirange"
        | "multirange_overleft_range"
        | "multirange_overleft_multirange" => over_left_op,
        "range_adjacent_multirange"
        | "multirange_adjacent_range"
        | "multirange_adjacent_multirange" => adjacent_op,
        // The names of the `&>` functions have the word "over" and the word "right" side by side.
        _ if src.contains("multirange") && src.contains(concat!("over", "right")) => over_right_op,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use rupg_types::oid;

    use super::*;

    fn int4(lower: i32, upper: i32) -> Range {
        make_range(oid::INT4RANGE, RANGE_LB_INC, Value::Int4(lower), Value::Int4(upper)).unwrap()
    }

    #[test]
    fn canonical() {
        let ranges = vec![int4(8, 10), int4(1, 5), Range::empty(), int4(3, 7), int4(10, 12)];
        let out = make_multirange(oid::INT4RANGE, ranges).unwrap();
        assert_eq!(out, [int4(1, 7), int4(8, 12)]);
        assert!(make_multirange(oid::INT4RANGE, vec![Range::empty()]).unwrap().is_empty());
    }

    #[test]
    fn order() {
        let one = [int4(1, 2)];
        let two = [int4(1, 2), int4(3, 4)];
        assert_eq!(multirange_order(oid::INT4, &one, &two).unwrap(), Ordering::Less);
        assert_eq!(multirange_order(oid::INT4, &[], &[]).unwrap(), Ordering::Equal);
        assert_eq!(multirange_order(oid::INT4, &[int4(2, 3)], &two).unwrap(), Ordering::Greater);
    }

    #[test]
    fn operators() {
        let r = oid::INT4RANGE;
        let a = [int4(1, 5), int4(10, 20)];
        let b = [int4(0, 2), int4(4, 11), int4(15, 16), int4(19, 30)];
        assert_eq!(minus_all(r, &a, &b).unwrap(), [int4(2, 4), int4(11, 15), int4(16, 19)]);
        assert_eq!(
            intersect_all(r, &a, &b).unwrap(),
            [int4(1, 2), int4(4, 5), int4(10, 11), int4(15, 16), int4(19, 20)]
        );
        assert!(overlaps_all(r, &a, &[int4(7, 12)]).unwrap());
        assert!(!overlaps_all(r, &a, &[int4(5, 10)]).unwrap());
        assert!(contains_all(r, &a, &[int4(2, 3), int4(12, 14)]).unwrap());
        assert!(!contains_all(r, &a, &[int4(2, 11)]).unwrap());
        assert!(contains_all(r, &[], &[]).unwrap());
    }
}

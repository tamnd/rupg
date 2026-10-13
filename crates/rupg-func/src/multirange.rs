//! The multirange types of `multirangetypes.c`: `make_multirange`, which sorts the ranges and merges the ranges that overlap or touch, the order of the multiranges, the constructors, the functions that read the bounds and the flags, `range_merge` of a multirange and `unnest`.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{RANGE_LB_INC, RANGE_LB_INF, RANGE_UB_INC, RANGE_UB_INF, Range, Value};

use crate::io::base_type;
use crate::range::{adjacent, before, make_range, range_order, subtype, union};
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
}

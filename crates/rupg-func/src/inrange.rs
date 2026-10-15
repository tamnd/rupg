//! The `in_range` support functions of the btree operator families, from `int.c`, `int8.c`, `float.c`, `numeric.c`, `date.c` and `timestamp.c`. A window frame in `RANGE` mode with an offset uses them to find the rows of the frame. The arguments are the value of a row, the value of the current row, the offset, `sub` and `less`. The result is `val <= base - offset` or `val >= base + offset` and the other two combinations.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    Interval, Numeric, NumericSign, TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone,
    Value, oid,
};

use crate::cast::midnight;
use crate::compare::{compare, float, span};
use crate::datetime::{
    interval_arg, interval_minus, interval_plus, is_nobegin, is_noend, negate, timestamp_arg,
    timestamp_plus,
};
use crate::{Call, Kernel, bad_value};

/// The kernel of an `in_range` function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "in_range_int8_int8" | "in_range_int4_int8" | "in_range_int4_int4"
        | "in_range_int4_int2" | "in_range_int2_int8" | "in_range_int2_int4"
        | "in_range_int2_int2" => integer,
        "in_range_float8_float8" | "in_range_float4_float8" => floating,
        "in_range_numeric_numeric" => numeric,
        "in_range_time_interval" => time,
        "in_range_timetz_interval" => timetz,
        "in_range_date_interval" => date,
        "in_range_timestamp_interval" => |_, args| timestamp(args, None),
        "in_range_timestamptz_interval" => |call, args| {
            let zone = call.session.zone()?;
            timestamp(args, Some(zone.as_ref()))
        },
        "in_range_interval_interval" => interval,
        _ => return None,
    })
}

/// `ERRCODE_INVALID_PRECEDING_OR_FOLLOWING_SIZE`.
fn invalid_size() -> Error {
    Error::new(
        SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
        "invalid preceding or following size in window function",
    )
}

/// The three values and the two flags of a call.
fn parts(args: &[Value]) -> Result<(&Value, &Value, &Value, bool, bool)> {
    let [val, base, offset, Value::Bool(sub), Value::Bool(less)] = args else {
        return Err(bad_value());
    };
    Ok((val, base, offset, *sub, *less))
}

/// The result of the comparison of the value with the sum.
fn result(order: Ordering, less: bool) -> Value {
    Value::Bool(if less { order.is_le() } else { order.is_ge() })
}

/// All the integer functions. The sum has no overflow in 128 bits, and that gives the same result as the overflow checks of PostgreSQL.
fn integer(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (val, base, offset, sub, less) = parts(args)?;
    let (Some(val), Some(base), Some(offset)) = (val.as_i64(), base.as_i64(), offset.as_i64())
    else {
        return Err(bad_value());
    };
    if offset < 0 {
        return Err(invalid_size());
    }
    let (base, offset) = (i128::from(base), i128::from(offset));
    let sum = if sub { base - offset } else { base + offset };
    Ok(result(i128::from(val).cmp(&sum), less))
}

/// `in_range_float8_float8` and `in_range_float4_float8`.
fn floating(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (val, base, offset, sub, less) = parts(args)?;
    let (Some(val), Some(base), Some(offset)) = (float(val), float(base), float(offset)) else {
        return Err(bad_value());
    };
    if offset.is_nan() || offset < 0.0 {
        return Err(invalid_size());
    }
    if val.is_nan() {
        return Ok(Value::Bool(base.is_nan() || !less));
    }
    if base.is_nan() {
        return Ok(Value::Bool(less));
    }
    if offset.is_infinite() && base.is_infinite() && (if sub { base > 0.0 } else { base < 0.0 }) {
        return Ok(Value::Bool(true));
    }
    let sum = if sub { base - offset } else { base + offset };
    Ok(Value::Bool(if less { val <= sum } else { val >= sum }))
}

/// `in_range_numeric_numeric`.
fn numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Value::Numeric(val), Value::Numeric(base), Value::Numeric(offset), sub, less) =
        parts(args)?
    else {
        return Err(bad_value());
    };
    use NumericSign::{Infinity, NaN, Negative, NegativeInfinity};
    if matches!(offset.sign(), NaN | NegativeInfinity | Negative) {
        return Err(invalid_size());
    }
    let (v, b) = (val.sign(), base.sign());
    let answer = if v == NaN {
        b == NaN || !less
    } else if b == NaN {
        less
    } else if offset.sign() == Infinity {
        if if sub { b == Infinity } else { b == NegativeInfinity } {
            true
        } else if sub {
            !less || v == NegativeInfinity
        } else {
            less || v == Infinity
        }
    } else if v == Infinity {
        b == Infinity || !less
    } else if v == NegativeInfinity {
        b == NegativeInfinity || less
    } else if b == NegativeInfinity {
        !less
    } else if b == Infinity {
        less
    } else {
        let sum: Numeric =
            if sub { base.sub(offset) } else { base.add(offset) }.map_err(crate::type_error)?;
        return Ok(result(val.compare(&sum), less));
    };
    Ok(Value::Bool(answer))
}

/// `in_range_time_interval`.
fn time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Value::Time(val), Value::Time(base), Value::Interval(offset), sub, less) = parts(args)?
    else {
        return Err(bad_value());
    };
    if offset.time < 0 {
        return Err(invalid_size());
    }
    let sum = if sub {
        base.wrapping_sub(offset.time)
    } else {
        match base.checked_add(offset.time) {
            Some(sum) => sum,
            None => return Ok(Value::Bool(less)),
        }
    };
    Ok(result(val.cmp(&sum), less))
}

/// `in_range_timetz_interval`: the sum has the zone of the current row.
fn timetz(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (val, Value::TimeTz(base, zone), Value::Interval(offset), sub, less) = parts(args)? else {
        return Err(bad_value());
    };
    if offset.time < 0 {
        return Err(invalid_size());
    }
    let sum = if sub {
        base.wrapping_sub(offset.time)
    } else {
        match base.checked_add(offset.time) {
            Some(sum) => sum,
            None => return Ok(Value::Bool(less)),
        }
    };
    let order = compare(oid::TIMETZ, val, oid::TIMETZ, &Value::TimeTz(sum, *zone))?;
    Ok(result(order, less))
}

/// `in_range_date_interval`: the dates become timestamps at midnight.
fn date(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Value::Date(val), Value::Date(base), offset, sub, less) = parts(args)? else {
        return Err(bad_value());
    };
    let args = [
        Value::Timestamp(midnight(*val)?),
        Value::Timestamp(midnight(*base)?),
        offset.clone(),
        Value::Bool(sub),
        Value::Bool(less),
    ];
    timestamp(&args, None)
}

/// The sign of an interval as `interval_sign` gives it.
fn negative(offset: &Interval) -> bool {
    span(offset) < 0
}

/// `in_range_timestamp_interval`, and `in_range_timestamptz_interval` when `zone` is given.
fn timestamp(args: &[Value], zone: Option<&dyn TimeZone>) -> Result<Value> {
    let (val, base, offset, sub, less) = parts(args)?;
    let (val, base, offset) = (timestamp_arg(val)?, timestamp_arg(base)?, interval_arg(offset)?);
    if negative(&offset) {
        return Err(invalid_size());
    }
    let end = if sub { TIMESTAMP_INFINITY } else { TIMESTAMP_NEGATIVE_INFINITY };
    if is_noend(&offset) && base == end {
        return Ok(Value::Bool(true));
    }
    let offset = if sub { negate(&offset)? } else { offset };
    let sum = timestamp_plus(base, &offset, zone)?;
    Ok(result(val.cmp(&sum), less))
}

/// `in_range_interval_interval`.
fn interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (val, base, offset, sub, less) = parts(args)?;
    let (val, base, offset) = (interval_arg(val)?, interval_arg(base)?, interval_arg(offset)?);
    if negative(&offset) {
        return Err(invalid_size());
    }
    if is_noend(&offset) && (if sub { is_noend(&base) } else { is_nobegin(&base) }) {
        return Ok(Value::Bool(true));
    }
    let sum = if sub { interval_minus(&base, &offset)? } else { interval_plus(&base, &offset)? };
    Ok(result(span(&val).cmp(&span(&sum)), less))
}

//! The functions that make a date or a time from its fields, `isfinite`, the cast of a time to a `timetz`, `timeofday`, `to_timestamp` of a number of seconds, and `generate_series` of the timestamps and of `numeric`. The rules are those of `make_date`, `make_time`, `make_timestamp_internal`, `make_timestamptz_at_timezone`, `time_timetz`, `timeofday`, `float8_timestamptz`, `generate_series_timestamp`, `generate_series_timestamptz_internal` and `generate_series_step_numeric`.

use std::rc::Rc;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, Interval, Numeric, NumericSign, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE, USECS_PER_DAY,
    USECS_PER_SEC, Value, date2j, float8_out, j2date, local_offset,
};

use crate::cast::{
    TIMESTAMP_END_JULIAN, infinite, instant, local, timestamp_range, valid_timestamp,
};
use crate::compare::span;
use crate::datetime::{days_of_month, timestamp_plus};
use crate::math::valid_date;
use crate::timezone::{lookup_timezone, parse_sane_timezone};
use crate::{Call, Kernel, bad_value};

/// The seconds from 1970-01-01 to 2000-01-01.
const UNIX_TO_POSTGRES_SECS: i64 = 946_684_800;
/// The number of rows between two checks for a cancel.
const CHECK_EVERY: usize = 4096;

fn int_arg(args: &[Value], at: usize) -> Result<i32> {
    match args.get(at) {
        Some(Value::Int4(n)) => Ok(*n),
        _ => Err(bad_value()),
    }
}

fn float_arg(args: &[Value], at: usize) -> Result<f64> {
    match args.get(at) {
        Some(Value::Float8(f)) => Ok(*f),
        _ => Err(bad_value()),
    }
}

/// `%g` of the `snprintf` of PostgreSQL.
pub(crate) fn format_g(v: f64) -> String {
    let mut out = Vec::new();
    float8_out(v, -9, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// `%02g`: a finite value of one character gets a leading zero.
fn format_02g(v: f64) -> String {
    let text = format_g(v);
    if text.len() == 1 { format!("0{text}") } else { text }
}

/// `IS_VALID_JULIAN`.
pub(crate) fn valid_julian(year: i32, mon: i32) -> bool {
    (year > -4713 || (year == -4713 && mon >= 11))
        && (year < 5_874_898 || (year == 5_874_898 && mon < 6))
}

/// `ValidateDate` for a year, a month and a day with no other fields. A year of BC becomes the internal year, where 1 BC is year zero. Gives false for a field out of range.
fn validate_date(year: &mut i32, mon: i32, mday: i32, bc: bool) -> bool {
    if *year <= 0 {
        return false;
    }
    if bc {
        *year = -(*year - 1);
    }
    (1..=12).contains(&mon) && (1..=31).contains(&mday) && mday <= days_of_month(*year, mon)
}

fn field_overflow(message: String) -> Error {
    Error::new(SqlState::DATETIME_FIELD_OVERFLOW, message)
}

fn value_range(message: String) -> Error {
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, message)
}

/// `make_date`: a negative year is a year BC. The messages show the fields after `ValidateDate`, as PostgreSQL shows them.
fn make_date(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (mut year, mon, mday) = (int_arg(args, 0)?, int_arg(args, 1)?, int_arg(args, 2)?);
    let bc = year < 0;
    if bc {
        year = year.checked_neg().ok_or_else(|| {
            field_overflow(format!("date field value out of range: {year}-{mon:02}-{mday:02}"))
        })?;
    }
    if !validate_date(&mut year, mon, mday, bc) {
        return Err(field_overflow(format!(
            "date field value out of range: {year}-{mon:02}-{mday:02}"
        )));
    }
    let out_of_range = || value_range(format!("date out of range: {year}-{mon:02}-{mday:02}"));
    if !valid_julian(year, mon) {
        return Err(out_of_range());
    }
    let date = date2j(year, mon, mday) - POSTGRES_EPOCH_JDATE;
    if !valid_date(i64::from(date)) {
        return Err(out_of_range());
    }
    Ok(Value::Date(date))
}

/// `float_time_overflows`: the hour can be 24 and the seconds can be 60, but the time cannot be after 24:00:00.
fn time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    if !(0..=24).contains(&hour) || !(0..60).contains(&min) || sec.is_nan() {
        return true;
    }
    let sec = (sec * 1e6).round_ties_even();
    if !(0.0..=60e6).contains(&sec) {
        return true;
    }
    time_of(hour, min, sec) > USECS_PER_DAY
}

/// The time of the fields as `tm2time` gives it, with the microseconds of the seconds already rounded.
#[allow(clippy::cast_possible_truncation)]
fn time_of(hour: i32, min: i32, micros: f64) -> i64 {
    (i64::from(hour) * 60 + i64::from(min)) * 60 * USECS_PER_SEC + micros as i64
}

/// `make_time`.
fn make_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (hour, min, sec) = (int_arg(args, 0)?, int_arg(args, 1)?, float_arg(args, 2)?);
    if time_overflows(hour, min, sec) {
        return Err(field_overflow(format!(
            "time field value out of range: {hour}:{min:02}:{}",
            format_02g(sec)
        )));
    }
    Ok(Value::Time(time_of(hour, min, (sec * 1e6).round_ties_even())))
}

/// `make_timestamp_internal`. The messages show the arguments as the call gives them.
fn timestamp_of(args: &[Value]) -> Result<i64> {
    let (year, mon, mday) = (int_arg(args, 0)?, int_arg(args, 1)?, int_arg(args, 2)?);
    let (hour, min, sec) = (int_arg(args, 3)?, int_arg(args, 4)?, float_arg(args, 5)?);
    let bc = year < 0;
    let mut internal = if bc { year.wrapping_neg() } else { year };
    if !validate_date(&mut internal, mon, mday, bc) {
        return Err(field_overflow(format!(
            "date field value out of range: {year}-{mon:02}-{mday:02}"
        )));
    }
    if !valid_julian(internal, mon) {
        return Err(value_range(format!("date out of range: {year}-{mon:02}-{mday:02}")));
    }
    let date = i64::from(date2j(internal, mon, mday) - POSTGRES_EPOCH_JDATE);
    if time_overflows(hour, min, sec) {
        return Err(field_overflow(format!(
            "time field value out of range: {hour}:{min:02}:{}",
            format_02g(sec)
        )));
    }
    let time = time_of(hour, min, (sec * 1e6).round_ties_even());
    date.checked_mul(USECS_PER_DAY)
        .and_then(|t| t.checked_add(time))
        .filter(|&t| valid_timestamp(t))
        .ok_or_else(|| {
            value_range(format!(
                "timestamp out of range: {year}-{mon:02}-{mday:02} {hour}:{min:02}:{}",
                format_02g(sec)
            ))
        })
}

/// `make_timestamp`.
fn make_timestamp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    timestamp_of(args).map(Value::Timestamp)
}

/// `make_timestamptz`: the fields are a local time of the zone of the session.
fn make_timestamptz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let utc = instant(call.session, timestamp_of(args)?)?;
    if !valid_timestamp(utc) {
        return Err(timestamp_range());
    }
    Ok(Value::TimestampTz(utc))
}

/// `make_timestamptz_at_timezone`: the fields are a local time of the zone that the seventh argument gives.
fn make_timestamptz_at_timezone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let timestamp = timestamp_of(args)?;
    let zone = args.get(6).ok_or_else(bad_value)?;
    let west = parse_sane_timezone(call, timestamp, zone)?;
    timestamp
        .checked_add(i64::from(west) * USECS_PER_SEC)
        .filter(|&t| valid_timestamp(t))
        .map(Value::TimestampTz)
        .ok_or_else(timestamp_range)
}

/// `date_finite`, `timestamp_finite` and `interval_finite`.
fn finite(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(match args.first() {
        Some(Value::Date(d)) => *d != DATE_INFINITY && *d != DATE_NEGATIVE_INFINITY,
        Some(Value::Timestamp(t) | Value::TimestampTz(t)) => !infinite(*t),
        Some(Value::Interval(i)) => *i != Interval::INFINITY && *i != Interval::NEGATIVE_INFINITY,
        _ => return Err(bad_value()),
    }))
}

/// `time_timetz`: the offset is the one of the zone of the session for the time on the date of the start of the transaction.
fn time_timetz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Time(time)) = args.first() else { return Err(bad_value()) };
    let today = local(call.session, call.session.transaction_start())?.div_euclid(USECS_PER_DAY);
    let seconds = today * 86_400 + time / USECS_PER_SEC + UNIX_TO_POSTGRES_SECS;
    let (west, _) = local_offset(seconds, call.session.zone()?.as_ref());
    Ok(Value::TimeTz(*time, west))
}

/// `timeofday`: the time now in the zone of the session, in the format `%a %b %d %H:%M:%S.%06d %Y %Z`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn timeofday(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let now = call.session.clock();
    let unix = now.div_euclid(USECS_PER_SEC) + UNIX_TO_POSTGRES_SECS;
    let zone = call.session.zone()?;
    let (east, abbrev) = zone.at(unix);
    let local = unix + i64::from(east);
    let julian = (local.div_euclid(86_400) + i64::from(UNIX_EPOCH_JDATE)) as i32;
    let (year, mon, mday) = j2date(julian);
    let secs = local.rem_euclid(86_400);
    Ok(Value::text(format!(
        "{} {} {mday:02} {:02}:{:02}:{:02}.{:06} {year} {abbrev}",
        DAYS[(julian + 1).rem_euclid(7) as usize],
        MONTHS[mon as usize - 1],
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        now.rem_euclid(USECS_PER_SEC),
    )))
}

/// `float8_timestamptz`: `to_timestamp` of the seconds since 1970-01-01 UTC.
#[allow(clippy::cast_possible_truncation)]
fn float8_timestamptz(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let seconds = float_arg(args, 0)?;
    if seconds.is_nan() {
        return Err(value_range("timestamp cannot be NaN".to_owned()));
    }
    if seconds.is_infinite() {
        let result = if seconds < 0.0 { TIMESTAMP_NEGATIVE_INFINITY } else { TIMESTAMP_INFINITY };
        return Ok(Value::TimestampTz(result));
    }
    let range = || value_range(format!("timestamp out of range: \"{}\"", format_g(seconds)));
    let low = 86_400.0 * f64::from(-UNIX_EPOCH_JDATE);
    let high = 86_400.0 * f64::from(TIMESTAMP_END_JULIAN - UNIX_EPOCH_JDATE);
    if seconds < low || seconds >= high {
        return Err(range());
    }
    let shifted = seconds - f64::from(POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * 86_400.0;
    let result = (shifted * 1e6).round_ties_even() as i64;
    if !valid_timestamp(result) {
        return Err(range());
    }
    Ok(Value::TimestampTz(result))
}

/// The checks of the step of `generate_series` of the timestamps.
fn step_arg(v: Option<&Value>) -> Result<Interval> {
    let Some(Value::Interval(step)) = v else { return Err(bad_value()) };
    if span(step) == 0 {
        return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
    }
    if *step == Interval::INFINITY || *step == Interval::NEGATIVE_INFINITY {
        return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "step size cannot be infinite"));
    }
    Ok(*step)
}

/// The timestamps from the start to the finish with the step. With a zone, the step is added to the local time of the zone.
fn timestamp_series(
    call: &Call<'_>,
    args: &[Value],
    zone: Option<&dyn TimeZone>,
    kind: fn(i64) -> Value,
) -> Result<Vec<Vec<Value>>> {
    let (
        Some(Value::Timestamp(start) | Value::TimestampTz(start)),
        Some(Value::Timestamp(finish) | Value::TimestampTz(finish)),
    ) = (args.first(), args.get(1))
    else {
        return Err(bad_value());
    };
    let step = step_arg(args.get(2))?;
    let up = span(&step) > 0;
    let (mut current, finish) = (*start, *finish);
    let mut rows = Vec::new();
    while if up { current <= finish } else { current >= finish } {
        if rows.len() % CHECK_EVERY == CHECK_EVERY - 1 {
            call.session.interrupted()?;
        }
        rows.push(vec![kind(current)]);
        current = timestamp_plus(current, &step, zone)?;
    }
    Ok(rows)
}

/// `generate_series_timestamp`.
fn series_timestamp(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    timestamp_series(call, args, None, Value::Timestamp)
}

/// `generate_series_timestamptz` and `generate_series_timestamptz_at_zone`: the step is added in the zone of the session or in the zone of the fourth argument.
fn series_timestamptz(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let zone: Rc<dyn TimeZone> = match args.get(3) {
        Some(name) => lookup_timezone(call, name)?,
        None => call.session.zone()?,
    };
    timestamp_series(call, args, Some(zone.as_ref()), Value::TimestampTz)
}

fn numeric_arg(args: &[Value], at: usize) -> Result<&Numeric> {
    match args.get(at) {
        Some(Value::Numeric(n)) => Ok(n),
        _ => Err(bad_value()),
    }
}

/// The error of a value of `generate_series` of `numeric` that is NaN or infinite.
fn special(what: &str, n: &Numeric) -> Result<()> {
    let name = match n.sign() {
        NumericSign::NaN => "NaN",
        NumericSign::Infinity | NumericSign::NegativeInfinity => "infinity",
        _ => return Ok(()),
    };
    Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("{what} cannot be {name}")))
}

/// `generate_series_numeric` and `generate_series_step_numeric`: the numbers from the start to the stop, with the step or 1.
fn series_numeric(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let (start, stop) = (numeric_arg(args, 0)?, numeric_arg(args, 1)?);
    special("start value", start)?;
    special("stop value", stop)?;
    let step = match args.get(2) {
        Some(_) => {
            let step = numeric_arg(args, 2)?;
            special("step size", step)?;
            if step.digits().is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "step size cannot equal zero",
                ));
            }
            step.clone()
        }
        None => Numeric::from_integer(1),
    };
    let up = step.sign() == NumericSign::Positive;
    let mut current = start.clone();
    let mut rows = Vec::new();
    loop {
        let order = current.compare(stop);
        if (up && order.is_gt()) || (!up && order.is_lt()) {
            break;
        }
        if rows.len() % CHECK_EVERY == CHECK_EVERY - 1 {
            call.session.interrupted()?;
        }
        let next = current.add(&step).map_err(crate::type_error)?;
        rows.push(vec![Value::Numeric(current)]);
        current = next;
    }
    Ok(rows)
}

/// The set kernels of this file.
pub(crate) fn set_kernel(src: &str) -> Option<crate::srf::SetKernel> {
    Some(match src {
        "generate_series_timestamp" => series_timestamp,
        "generate_series_timestamptz" | "generate_series_timestamptz_at_zone" => series_timestamptz,
        "generate_series_numeric" | "generate_series_step_numeric" => series_numeric,
        _ => return None,
    })
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "make_date" => make_date,
        "make_time" => make_time,
        "make_timestamp" => make_timestamp,
        "make_timestamptz" => make_timestamptz,
        "make_timestamptz_at_timezone" => make_timestamptz_at_timezone,
        "date_finite" | "timestamp_finite" | "interval_finite" => finite,
        "time_timetz" => time_timetz,
        "timeofday" => timeofday,
        "float8_timestamptz" => float8_timestamptz,
        _ => return None,
    })
}

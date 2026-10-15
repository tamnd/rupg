//! The arithmetic of the date and time types: an interval added to a timestamp, a time or a date, the difference of two timestamps, `age`, the `justify` functions, the operators of `interval` and `OVERLAPS`. The rules are those of `timestamp.c` and `date.c`.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, Interval, POSTGRES_EPOCH_JDATE, TIMESTAMP_INFINITY,
    TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_TO_POSTGRES_USECS, USECS_PER_DAY, USECS_PER_SEC,
    Value, date2j, j2date,
};

use crate::cast::{
    TIMESTAMP_END_JULIAN, infinite, local, midnight, timestamp_range, valid_timestamp,
};
use crate::compare::{compare, span};
use crate::timezone::lookup_timezone;
use crate::{Call, Kernel, bad_value};

/// `DAYS_PER_MONTH`.
const DAYS_PER_MONTH: i32 = 30;
/// `SECS_PER_DAY`.
const SECS_PER_DAY: i32 = 86_400;
/// `MONTHS_PER_YEAR`.
const MONTHS_PER_YEAR: i32 = 12;

fn interval_range() -> Error {
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "interval out of range")
}

fn date_range() -> Error {
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range for timestamp")
}

fn is_nobegin(interval: &Interval) -> bool {
    *interval == Interval::NEGATIVE_INFINITY
}

fn is_noend(interval: &Interval) -> bool {
    *interval == Interval::INFINITY
}

/// `INTERVAL_NOT_FINITE`.
fn not_finite(interval: &Interval) -> bool {
    is_nobegin(interval) || is_noend(interval)
}

fn interval_arg(v: &Value) -> Result<Interval> {
    let Value::Interval(interval) = v else { return Err(bad_value()) };
    Ok(*interval)
}

fn timestamp_arg(v: &Value) -> Result<i64> {
    match v {
        Value::Timestamp(t) | Value::TimestampTz(t) => Ok(*t),
        _ => Err(bad_value()),
    }
}

fn float_arg(v: &Value) -> Result<f64> {
    let Value::Float8(f) = v else { return Err(bad_value()) };
    Ok(*f)
}

/// `interval_um_internal`.
fn negate(interval: &Interval) -> Result<Interval> {
    if is_nobegin(interval) {
        return Ok(Interval::INFINITY);
    }
    if is_noend(interval) {
        return Ok(Interval::NEGATIVE_INFINITY);
    }
    let result = Interval {
        time: 0i64.checked_sub(interval.time).ok_or_else(interval_range)?,
        day: 0i32.checked_sub(interval.day).ok_or_else(interval_range)?,
        month: 0i32.checked_sub(interval.month).ok_or_else(interval_range)?,
    };
    if not_finite(&result) {
        return Err(interval_range());
    }
    Ok(result)
}

/// The sum of two intervals, as `interval_pl` gives it.
fn interval_plus(a: &Interval, b: &Interval) -> Result<Interval> {
    if is_nobegin(a) {
        if is_noend(b) {
            return Err(interval_range());
        }
        return Ok(Interval::NEGATIVE_INFINITY);
    }
    if is_noend(a) {
        if is_nobegin(b) {
            return Err(interval_range());
        }
        return Ok(Interval::INFINITY);
    }
    if not_finite(b) {
        return Ok(*b);
    }
    let result = Interval {
        month: a.month.checked_add(b.month).ok_or_else(interval_range)?,
        day: a.day.checked_add(b.day).ok_or_else(interval_range)?,
        time: a.time.checked_add(b.time).ok_or_else(interval_range)?,
    };
    if not_finite(&result) {
        return Err(interval_range());
    }
    Ok(result)
}

/// The difference of two intervals, as `interval_mi` gives it.
fn interval_minus(a: &Interval, b: &Interval) -> Result<Interval> {
    if is_nobegin(a) {
        if is_nobegin(b) {
            return Err(interval_range());
        }
        return Ok(Interval::NEGATIVE_INFINITY);
    }
    if is_noend(a) {
        if is_noend(b) {
            return Err(interval_range());
        }
        return Ok(Interval::INFINITY);
    }
    if is_nobegin(b) {
        return Ok(Interval::INFINITY);
    }
    if is_noend(b) {
        return Ok(Interval::NEGATIVE_INFINITY);
    }
    let result = Interval {
        month: a.month.checked_sub(b.month).ok_or_else(interval_range)?,
        day: a.day.checked_sub(b.day).ok_or_else(interval_range)?,
        time: a.time.checked_sub(b.time).ok_or_else(interval_range)?,
    };
    if not_finite(&result) {
        return Err(interval_range());
    }
    Ok(result)
}

fn interval_pl(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    interval_plus(&interval_arg(a)?, &interval_arg(b)?).map(Value::Interval)
}

fn interval_mi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    interval_minus(&interval_arg(a)?, &interval_arg(b)?).map(Value::Interval)
}

fn interval_um(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    negate(&interval_arg(a)?).map(Value::Interval)
}

/// `TSROUND`: a number rounded to microseconds.
fn ts_round(x: f64) -> f64 {
    (x * 1_000_000.0).round_ties_even() / 1_000_000.0
}

/// `FLOAT8_FITS_IN_INT32`.
fn fits_i32(x: f64) -> bool {
    x >= f64::from(i32::MIN) && x < -f64::from(i32::MIN)
}

/// `FLOAT8_FITS_IN_INT64`.
#[allow(clippy::cast_precision_loss)]
fn fits_i64(x: f64) -> bool {
    x >= i64::MIN as f64 && x < -(i64::MIN as f64)
}

/// The finite part of `interval_mul` and `interval_div`: `op` multiplies or divides each field by the factor. The fractions of the months and the days go down into the days and the time.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn scale(interval: &Interval, op: impl Fn(f64) -> f64) -> Result<Interval> {
    let months = op(f64::from(interval.month));
    if months.is_nan() || !fits_i32(months) {
        return Err(interval_range());
    }
    let month = months as i32;
    let days = op(f64::from(interval.day));
    if days.is_nan() || !fits_i32(days) {
        return Err(interval_range());
    }
    let mut day = days as i32;
    let month_remainder_days = ts_round((months - f64::from(month)) * f64::from(DAYS_PER_MONTH));
    let mut sec_remainder = ts_round(
        (days - f64::from(day) + month_remainder_days - f64::from(month_remainder_days as i32))
            * f64::from(SECS_PER_DAY),
    );
    if sec_remainder.abs() >= f64::from(SECS_PER_DAY) {
        let whole = (sec_remainder / f64::from(SECS_PER_DAY)) as i32;
        day = day.checked_add(whole).ok_or_else(interval_range)?;
        sec_remainder -= f64::from(whole * SECS_PER_DAY);
    }
    day = day.checked_add(month_remainder_days as i32).ok_or_else(interval_range)?;
    let time = (op(interval.time as f64) + sec_remainder * USECS_PER_SEC as f64).round_ties_even();
    if time.is_nan() || !fits_i64(time) {
        return Err(interval_range());
    }
    let result = Interval { time: time as i64, day, month };
    if not_finite(&result) {
        return Err(interval_range());
    }
    Ok(result)
}

/// `interval_mul`.
fn interval_times(interval: &Interval, factor: f64) -> Result<Interval> {
    if factor.is_nan() {
        return Err(interval_range());
    }
    if not_finite(interval) {
        if factor == 0.0 {
            return Err(interval_range());
        }
        return if factor < 0.0 { negate(interval) } else { Ok(*interval) };
    }
    if factor.is_infinite() {
        let sign = match span(interval).cmp(&0) {
            Ordering::Less => -1.0,
            Ordering::Equal => return Err(interval_range()),
            Ordering::Greater => 1.0,
        };
        return Ok(if factor * sign < 0.0 {
            Interval::NEGATIVE_INFINITY
        } else {
            Interval::INFINITY
        });
    }
    scale(interval, |x| x * factor)
}

/// `interval_lerp` of `orderedsetaggs.c`: the interval at the fraction `pct` of the way from `lo` to `hi`, for `percentile_cont`.
pub fn interval_lerp(lo: &Interval, hi: &Interval, pct: f64) -> Result<Interval> {
    interval_plus(&interval_times(&interval_minus(hi, lo)?, pct)?, lo)
}

fn interval_mul(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    interval_times(&interval_arg(a)?, float_arg(b)?).map(Value::Interval)
}

fn mul_d_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    interval_times(&interval_arg(b)?, float_arg(a)?).map(Value::Interval)
}

fn interval_div(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let (interval, factor) = (interval_arg(a)?, float_arg(b)?);
    if factor == 0.0 {
        return Err(Error::new(SqlState::DIVISION_BY_ZERO, "division by zero"));
    }
    if factor.is_nan() {
        return Err(interval_range());
    }
    if not_finite(&interval) {
        if factor.is_infinite() {
            return Err(interval_range());
        }
        let result = if factor < 0.0 { negate(&interval)? } else { interval };
        return Ok(Value::Interval(result));
    }
    scale(&interval, |x| x / factor).map(Value::Interval)
}

/// `interval_justify_hours`: the whole days of the time go into the days.
fn justify_hours(interval: &Interval) -> Result<Interval> {
    if not_finite(interval) {
        return Ok(*interval);
    }
    let mut result = *interval;
    let whole = result.time / USECS_PER_DAY;
    result.time -= whole * USECS_PER_DAY;
    let whole = i32::try_from(whole).map_err(|_| interval_range())?;
    result.day = result.day.checked_add(whole).ok_or_else(interval_range)?;
    if result.day > 0 && result.time < 0 {
        result.time += USECS_PER_DAY;
        result.day -= 1;
    } else if result.day < 0 && result.time > 0 {
        result.time -= USECS_PER_DAY;
        result.day += 1;
    }
    Ok(result)
}

/// `interval_justify_days`: each 30 days go into the months.
fn justify_days(interval: &Interval) -> Result<Interval> {
    if not_finite(interval) {
        return Ok(*interval);
    }
    let mut result = *interval;
    let whole = result.day / DAYS_PER_MONTH;
    result.day -= whole * DAYS_PER_MONTH;
    result.month = result.month.checked_add(whole).ok_or_else(interval_range)?;
    if result.month > 0 && result.day < 0 {
        result.day += DAYS_PER_MONTH;
        result.month -= 1;
    } else if result.month < 0 && result.day > 0 {
        result.day -= DAYS_PER_MONTH;
        result.month += 1;
    }
    Ok(result)
}

/// `interval_justify_interval`: the two justifications, and one sign for the three fields.
#[allow(clippy::cast_possible_truncation)]
fn justify_interval(interval: &Interval) -> Result<Interval> {
    if not_finite(interval) {
        return Ok(*interval);
    }
    let mut result = *interval;
    if (result.day > 0 && result.time > 0) || (result.day < 0 && result.time < 0) {
        let whole = result.day / DAYS_PER_MONTH;
        result.day -= whole * DAYS_PER_MONTH;
        result.month = result.month.checked_add(whole).ok_or_else(interval_range)?;
    }
    let whole = result.time / USECS_PER_DAY;
    result.time -= whole * USECS_PER_DAY;
    result.day += whole as i32;
    let whole = result.day / DAYS_PER_MONTH;
    result.day -= whole * DAYS_PER_MONTH;
    result.month = result.month.checked_add(whole).ok_or_else(interval_range)?;
    if result.month > 0 && (result.day < 0 || (result.day == 0 && result.time < 0)) {
        result.day += DAYS_PER_MONTH;
        result.month -= 1;
    } else if result.month < 0 && (result.day > 0 || (result.day == 0 && result.time > 0)) {
        result.day -= DAYS_PER_MONTH;
        result.month += 1;
    }
    if result.day > 0 && result.time < 0 {
        result.time += USECS_PER_DAY;
        result.day -= 1;
    } else if result.day < 0 && result.time > 0 {
        result.time -= USECS_PER_DAY;
        result.day += 1;
    }
    Ok(result)
}

fn interval_justify_hours(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    justify_hours(&interval_arg(a)?).map(Value::Interval)
}

fn interval_justify_days(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    justify_days(&interval_arg(a)?).map(Value::Interval)
}

fn interval_justify_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    justify_interval(&interval_arg(a)?).map(Value::Interval)
}

/// The date fields and the time of day of a timestamp, as `struct pg_tm` and `fsec_t` hold them.
pub(crate) struct Tm {
    pub(crate) year: i32,
    pub(crate) mon: i32,
    pub(crate) mday: i32,
    /// The microseconds since midnight.
    pub(crate) time: i64,
}

/// `isleap`.
fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// `day_tab`: the days of a month.
pub(crate) fn days_of_month(year: i32, mon: i32) -> i32 {
    const DAYS: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if mon == 2 && is_leap(year) { 29 } else { DAYS[(mon - 1) as usize] }
}

impl Tm {
    /// `timestamp2tm` with no zone, with the error of its callers.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub(crate) fn of(ts: i64) -> Result<Tm> {
        let julian = ts.div_euclid(USECS_PER_DAY) + i64::from(POSTGRES_EPOCH_JDATE);
        let julian = i32::try_from(julian).ok().filter(|&j| j >= 0).ok_or_else(timestamp_range)?;
        let (year, mon, mday) = j2date(julian);
        Ok(Tm { year, mon: mon as i32, mday: mday as i32, time: ts.rem_euclid(USECS_PER_DAY) })
    }

    /// `timestamp2tm` in the zone of the session: the fields of the local time.
    fn of_local(call: &Call<'_>, ts: i64) -> Result<Tm> {
        Tm::of(ts)?;
        Tm::of(local(call.session, ts)?)
    }

    /// `timestamp2tm` in a zone: the fields of the local time.
    fn of_zone(zone: &dyn TimeZone, ts: i64) -> Result<Tm> {
        Tm::of(ts)?;
        let unix = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
        let east = i64::from(zone.at(unix).0);
        Tm::of(ts.checked_add(east * USECS_PER_SEC).ok_or_else(timestamp_range)?)
    }

    /// `tm2timestamp`. With a zone, the offset is the one that `DetermineTimeZoneOffset` gives for the fields.
    pub(crate) fn timestamp(&self, zone: Option<&dyn TimeZone>) -> Result<i64> {
        let local = self.local()?;
        let west = zone.map_or(0, |zone| {
            let seconds = local.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
            rupg_types::local_offset(seconds, zone).0
        });
        Tm::shift(local, west)
    }

    /// `tm2timestamp` with an offset in seconds west of UTC.
    pub(crate) fn timestamp_west(&self, west: i32) -> Result<i64> {
        Tm::shift(self.local()?, west)
    }

    /// The fields as a timestamp with no zone, with the checks of `tm2timestamp` before it applies the offset.
    fn local(&self) -> Result<i64> {
        let valid = (self.year > -4713 || (self.year == -4713 && self.mon >= 11))
            && (self.year < 5_874_898 || (self.year == 5_874_898 && self.mon < 6));
        if !valid {
            return Err(timestamp_range());
        }
        let date = i64::from(date2j(self.year, self.mon, self.mday) - POSTGRES_EPOCH_JDATE);
        date.checked_mul(USECS_PER_DAY)
            .and_then(|t| t.checked_add(self.time))
            .ok_or_else(timestamp_range)
    }

    /// `dt2local` with the offset, and the range check of `tm2timestamp`.
    fn shift(local: i64, west: i32) -> Result<i64> {
        let result = local.wrapping_add(i64::from(west) * USECS_PER_SEC);
        if !valid_timestamp(result) {
            return Err(timestamp_range());
        }
        Ok(result)
    }
}

/// `timestamp_pl_interval`, and `timestamptz_pl_interval_internal` when `zone` is given: the months and the days are added to the fields of the local time, and then the time is added.
pub(crate) fn timestamp_plus(
    ts: i64,
    interval: &Interval,
    zone: Option<&dyn TimeZone>,
) -> Result<i64> {
    if is_nobegin(interval) {
        if ts == TIMESTAMP_INFINITY {
            return Err(timestamp_range());
        }
        return Ok(TIMESTAMP_NEGATIVE_INFINITY);
    }
    if is_noend(interval) {
        if ts == TIMESTAMP_NEGATIVE_INFINITY {
            return Err(timestamp_range());
        }
        return Ok(TIMESTAMP_INFINITY);
    }
    if infinite(ts) {
        return Ok(ts);
    }
    let fields = |ts: i64| match zone {
        Some(zone) => Tm::of_zone(zone, ts),
        None => Tm::of(ts),
    };
    let mut ts = ts;
    if interval.month != 0 {
        let mut tm = fields(ts)?;
        tm.mon = tm.mon.checked_add(interval.month).ok_or_else(timestamp_range)?;
        if tm.mon > MONTHS_PER_YEAR {
            tm.year = tm.year.wrapping_add((tm.mon - 1) / MONTHS_PER_YEAR);
            tm.mon = (tm.mon - 1) % MONTHS_PER_YEAR + 1;
        } else if tm.mon < 1 {
            tm.year = tm.year.wrapping_add(tm.mon / MONTHS_PER_YEAR - 1);
            tm.mon = tm.mon % MONTHS_PER_YEAR + MONTHS_PER_YEAR;
        }
        tm.mday = tm.mday.min(days_of_month(tm.year, tm.mon));
        ts = tm.timestamp(zone)?;
    }
    if interval.day != 0 {
        let mut tm = fields(ts)?;
        let lowest = if zone.is_some() { -1 } else { 0 };
        let julian = date2j(tm.year, tm.mon, tm.mday)
            .checked_add(interval.day)
            .filter(|&j| j >= lowest)
            .ok_or_else(timestamp_range)?;
        let (year, mon, mday) = j2date(julian);
        (tm.year, tm.mon, tm.mday) = (year, mon as i32, mday as i32);
        ts = tm.timestamp(zone)?;
    }
    let ts = ts.checked_add(interval.time).ok_or_else(timestamp_range)?;
    if !valid_timestamp(ts) {
        return Err(timestamp_range());
    }
    Ok(ts)
}

fn timestamp_pl_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    timestamp_plus(timestamp_arg(a)?, &interval_arg(b)?, None).map(Value::Timestamp)
}

fn timestamp_mi_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    timestamp_plus(timestamp_arg(a)?, &negate(&interval_arg(b)?)?, None).map(Value::Timestamp)
}

fn timestamptz_pl_interval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let zone = call.session.zone()?;
    timestamp_plus(timestamp_arg(a)?, &interval_arg(b)?, Some(zone.as_ref()))
        .map(Value::TimestampTz)
}

fn timestamptz_mi_interval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let interval = negate(&interval_arg(b)?)?;
    let zone = call.session.zone()?;
    timestamp_plus(timestamp_arg(a)?, &interval, Some(zone.as_ref())).map(Value::TimestampTz)
}

/// `timestamptz_pl_interval_at_zone`: `date_add` with the arithmetic in a zone that a name gives.
fn timestamptz_pl_interval_at_zone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b, zone] = args else { return Err(bad_value()) };
    let zone = lookup_timezone(call, zone)?;
    timestamp_plus(timestamp_arg(a)?, &interval_arg(b)?, Some(zone.as_ref()))
        .map(Value::TimestampTz)
}

/// `timestamptz_mi_interval_at_zone`: `date_subtract` with the arithmetic in a zone that a name gives.
fn timestamptz_mi_interval_at_zone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b, zone] = args else { return Err(bad_value()) };
    let interval = negate(&interval_arg(b)?)?;
    let zone = lookup_timezone(call, zone)?;
    timestamp_plus(timestamp_arg(a)?, &interval, Some(zone.as_ref())).map(Value::TimestampTz)
}

/// `timestamp_mi`, also for `timestamptz`: the difference in microseconds, with the whole days in the days.
fn timestamp_mi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    let (a, b) = (timestamp_arg(a)?, timestamp_arg(b)?);
    if infinite(a) || infinite(b) {
        let result = match (a, b) {
            (TIMESTAMP_NEGATIVE_INFINITY, TIMESTAMP_NEGATIVE_INFINITY)
            | (TIMESTAMP_INFINITY, TIMESTAMP_INFINITY) => return Err(interval_range()),
            (TIMESTAMP_NEGATIVE_INFINITY, _) | (_, TIMESTAMP_INFINITY) => {
                Interval::NEGATIVE_INFINITY
            }
            _ => Interval::INFINITY,
        };
        return Ok(Value::Interval(result));
    }
    let time = a.checked_sub(b).ok_or_else(interval_range)?;
    justify_hours(&Interval { time, day: 0, month: 0 }).map(Value::Interval)
}

/// `timestamp_age` and `timestamptz_age`: the differences of the fields, with a borrow from the next field when a field is negative.
fn age(a: i64, b: i64, call: Option<&Call<'_>>) -> Result<Interval> {
    match (a, b) {
        (TIMESTAMP_NEGATIVE_INFINITY, TIMESTAMP_NEGATIVE_INFINITY)
        | (TIMESTAMP_INFINITY, TIMESTAMP_INFINITY) => return Err(interval_range()),
        (TIMESTAMP_NEGATIVE_INFINITY, _) | (_, TIMESTAMP_INFINITY) => {
            return Ok(Interval::NEGATIVE_INFINITY);
        }
        (TIMESTAMP_INFINITY, _) | (_, TIMESTAMP_NEGATIVE_INFINITY) => {
            return Ok(Interval::INFINITY);
        }
        _ => {}
    }
    let fields = |ts: i64| match call {
        Some(call) => Tm::of_local(call, ts),
        None => Tm::of(ts),
    };
    let (tm1, tm2) = (fields(a)?, fields(b)?);
    let split = |tm: &Tm| {
        let seconds = tm.time / USECS_PER_SEC;
        [tm.time % USECS_PER_SEC, seconds % 60, seconds / 60 % 60, seconds / 3600]
    };
    let (t1, t2) = (split(&tm1), split(&tm2));
    let sign = if a < b { -1 } else { 1 };
    let mut usec = sign * (t1[0] - t2[0]);
    let mut sec = sign * (t1[1] - t2[1]);
    let mut min = sign * (t1[2] - t2[2]);
    let mut hour = sign * (t1[3] - t2[3]);
    let mut mday = sign * i64::from(tm1.mday - tm2.mday);
    let mut mon = sign * i64::from(tm1.mon - tm2.mon);
    let mut year = sign * (i64::from(tm1.year) - i64::from(tm2.year));
    while usec < 0 {
        usec += USECS_PER_SEC;
        sec -= 1;
    }
    while sec < 0 {
        sec += 60;
        min -= 1;
    }
    while min < 0 {
        min += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        mday -= 1;
    }
    let borrow = if a < b { &tm1 } else { &tm2 };
    while mday < 0 {
        mday += i64::from(days_of_month(borrow.year, borrow.mon));
        mon -= 1;
    }
    while mon < 0 {
        mon += i64::from(MONTHS_PER_YEAR);
        year -= 1;
    }
    let months = sign * (year * i64::from(MONTHS_PER_YEAR) + mon);
    let month = i32::try_from(months).map_err(|_| interval_range())?;
    let day = i32::try_from(sign * mday).map_err(|_| interval_range())?;
    let time = (sign * hour)
        .checked_mul(3600 * USECS_PER_SEC)
        .and_then(|t| t.checked_add(sign * min * 60 * USECS_PER_SEC))
        .and_then(|t| t.checked_add(sign * sec * USECS_PER_SEC))
        .and_then(|t| t.checked_add(sign * usec))
        .ok_or_else(interval_range)?;
    let result = Interval { time, day, month };
    if not_finite(&result) {
        return Err(interval_range());
    }
    Ok(result)
}

fn timestamp_age(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    age(timestamp_arg(a)?, timestamp_arg(b)?, None).map(Value::Interval)
}

fn timestamptz_age(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    age(timestamp_arg(a)?, timestamp_arg(b)?, Some(call)).map(Value::Interval)
}

/// A time of day plus a number of microseconds, which wraps at midnight.
fn wrap_time(time: i64, add: i64) -> i64 {
    time.wrapping_add(add).rem_euclid(USECS_PER_DAY)
}

fn infinite_time(add: bool) -> Error {
    let message = if add {
        "cannot add infinite interval to time"
    } else {
        "cannot subtract infinite interval from time"
    };
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, message)
}

fn time_pl_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Time(t), b] = args else { return Err(bad_value()) };
    let interval = interval_arg(b)?;
    if not_finite(&interval) {
        return Err(infinite_time(true));
    }
    Ok(Value::Time(wrap_time(*t, interval.time)))
}

fn time_mi_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Time(t), b] = args else { return Err(bad_value()) };
    let interval = interval_arg(b)?;
    if not_finite(&interval) {
        return Err(infinite_time(false));
    }
    Ok(Value::Time(wrap_time(*t, interval.time.wrapping_neg())))
}

fn timetz_pl_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TimeTz(t, zone), b] = args else { return Err(bad_value()) };
    let interval = interval_arg(b)?;
    if not_finite(&interval) {
        return Err(infinite_time(true));
    }
    Ok(Value::TimeTz(wrap_time(*t, interval.time), *zone))
}

fn timetz_mi_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TimeTz(t, zone), b] = args else { return Err(bad_value()) };
    let interval = interval_arg(b)?;
    if not_finite(&interval) {
        return Err(infinite_time(false));
    }
    Ok(Value::TimeTz(wrap_time(*t, interval.time.wrapping_neg()), *zone))
}

fn time_mi_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Time(a), Value::Time(b)] = args else { return Err(bad_value()) };
    Ok(Value::Interval(Interval { time: a - b, day: 0, month: 0 }))
}

/// `interval_time`: the part of the interval after the whole days, with the floor for a negative interval.
fn interval_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    let interval = interval_arg(a)?;
    if not_finite(&interval) {
        return Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "cannot convert infinite interval to time",
        ));
    }
    Ok(Value::Time(interval.time.rem_euclid(USECS_PER_DAY)))
}

fn time_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Time(t)] = args else { return Err(bad_value()) };
    Ok(Value::Interval(Interval { time: *t, day: 0, month: 0 }))
}

fn date_pl_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), b] = args else { return Err(bad_value()) };
    timestamp_plus(midnight(*d)?, &interval_arg(b)?, None).map(Value::Timestamp)
}

fn date_mi_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), b] = args else { return Err(bad_value()) };
    timestamp_plus(midnight(*d)?, &negate(&interval_arg(b)?)?, None).map(Value::Timestamp)
}

/// `datetime_timestamp`: a date and a time of day.
fn datetime_timestamp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), Value::Time(t)] = args else { return Err(bad_value()) };
    let result = midnight(*d)?;
    if infinite(result) {
        return Ok(Value::Timestamp(result));
    }
    let result = result + t;
    if !valid_timestamp(result) {
        return Err(timestamp_range());
    }
    Ok(Value::Timestamp(result))
}

/// `datetimetz_timestamptz`: a date and a time of day with its offset.
fn datetimetz_timestamptz(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Date(d), Value::TimeTz(t, zone)] = args else { return Err(bad_value()) };
    match *d {
        DATE_NEGATIVE_INFINITY => return Ok(Value::TimestampTz(TIMESTAMP_NEGATIVE_INFINITY)),
        DATE_INFINITY => return Ok(Value::TimestampTz(TIMESTAMP_INFINITY)),
        d if d >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => return Err(date_range()),
        _ => {}
    }
    let result = i64::from(*d) * USECS_PER_DAY + t + i64::from(*zone) * USECS_PER_SEC;
    if !valid_timestamp(result) {
        return Err(date_range());
    }
    Ok(Value::TimestampTz(result))
}

/// `overlaps_timestamp`, `overlaps_time` and `overlaps_timetz`, which are not strict: a null end point gives null when the answer depends on it.
fn overlaps(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [s1, e1, s2, e2] = args else { return Err(bad_value()) };
    let order = |a: &Value, b: &Value| compare(0, a, 0, b);
    /// The lesser end point first, or the end point that is not null and None.
    fn ends<'a>(
        s: &'a Value,
        e: &'a Value,
        order: &impl Fn(&Value, &Value) -> Result<Ordering>,
    ) -> Result<Option<(&'a Value, Option<&'a Value>)>> {
        Ok(match (s.is_null(), e.is_null()) {
            (true, true) => None,
            (true, false) => Some((e, None)),
            (false, true) => Some((s, None)),
            (false, false) if order(s, e)? == Ordering::Greater => Some((e, Some(s))),
            (false, false) => Some((s, Some(e))),
        })
    }
    let Some((s1, e1)) = ends(s1, e1, &order)? else { return Ok(Value::Null) };
    let Some((s2, e2)) = ends(s2, e2, &order)? else { return Ok(Value::Null) };
    // When the starts differ, the later start must be before the end of the other interval.
    let later = |start: &Value, end: Option<&Value>, other_end: Option<&Value>| -> Result<Value> {
        let Some(end) = end else { return Ok(Value::Null) };
        if order(start, end)? == Ordering::Less {
            return Ok(Value::Bool(true));
        }
        Ok(if other_end.is_none() { Value::Null } else { Value::Bool(false) })
    };
    match order(s1, s2)? {
        Ordering::Greater => later(s1, e2, e1),
        Ordering::Less => later(s2, e1, e2),
        Ordering::Equal => {
            Ok(if e1.is_none() || e2.is_none() { Value::Null } else { Value::Bool(true) })
        }
    }
}

/// The kernel of an internal function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "interval_pl" => interval_pl,
        "interval_mi" => interval_mi,
        "interval_um" => interval_um,
        "interval_mul" => interval_mul,
        "mul_d_interval" => mul_d_interval,
        "interval_div" => interval_div,
        "interval_justify_hours" => interval_justify_hours,
        "interval_justify_days" => interval_justify_days,
        "interval_justify_interval" => interval_justify_interval,
        "timestamp_pl_interval" => timestamp_pl_interval,
        "timestamp_mi_interval" => timestamp_mi_interval,
        "timestamptz_pl_interval" => timestamptz_pl_interval,
        "timestamptz_mi_interval" => timestamptz_mi_interval,
        "timestamptz_pl_interval_at_zone" => timestamptz_pl_interval_at_zone,
        "timestamptz_mi_interval_at_zone" => timestamptz_mi_interval_at_zone,
        "timestamp_mi" => timestamp_mi,
        "timestamp_age" => timestamp_age,
        "timestamptz_age" => timestamptz_age,
        "time_pl_interval" => time_pl_interval,
        "time_mi_interval" => time_mi_interval,
        "timetz_pl_interval" => timetz_pl_interval,
        "timetz_mi_interval" => timetz_mi_interval,
        "time_mi_time" => time_mi_time,
        "interval_time" => interval_time,
        "time_interval" => time_interval,
        "date_pl_interval" => date_pl_interval,
        "date_mi_interval" => date_mi_interval,
        "datetime_timestamp" => datetime_timestamp,
        "datetimetz_timestamptz" => datetimetz_timestamptz,
        "overlaps_timestamp" | "overlaps_time" | "overlaps_timetz" => overlaps,
        _ => return None,
    })
}

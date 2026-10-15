//! `EXTRACT` and `date_part`: a field of a date, a time, a `timetz`, a timestamp or an interval. `extract` gives a `numeric` and `date_part` gives a `float8`. Also `date_trunc`, which cuts a timestamp or an interval to a unit, and `date_bin`, which puts a timestamp into a bin of a stride. The rules are those of `timestamp_part_common`, `interval_part_common`, `extract_date`, `time_part_common`, `timetz_part_common`, `timestamp_trunc`, `timestamptz_trunc_internal`, `interval_trunc` and `timestamp_bin`.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateUnit, Interval, Numeric, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_NEGATIVE_INFINITY, UNIX_EPOCH_JDATE, UNIX_TO_POSTGRES_USECS, USECS_PER_DAY,
    USECS_PER_SEC, Value, date2j, j2date, numeric_in,
};

use crate::cast::{infinite, local_time, valid_timestamp};
use crate::datetime::Tm;
use crate::{Call, Kernel, bad_value, type_error};

/// `NAMEDATALEN`: the units are cut to fewer bytes than this.
const NAMEDATALEN: usize = 64;
const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 3600 * USECS_PER_SEC;
/// `SetEpochTimestamp`: 1970-01-01 as a timestamp.
const EPOCH: i64 = UNIX_TO_POSTGRES_USECS;

/// The value of a field before it becomes a `numeric` or a `float8`.
enum Field {
    Null,
    /// `Infinity`, or `-Infinity` when the flag is set.
    Infinite(bool),
    Int(i64),
    /// `value / 10^scale` as a `numeric` with that scale, and the `float8` that `date_part` computes in its own order.
    Fixed {
        value: i128,
        scale: u32,
        float: f64,
    },
    /// A `numeric` that is already computed, and the `float8`.
    Numeric {
        numeric: Numeric,
        float: f64,
    },
    /// The Julian day and the microseconds since midnight.
    Julian {
        day: i32,
        usecs: i64,
    },
}

impl Field {
    #[allow(clippy::cast_precision_loss)]
    fn value(self, numeric: bool) -> Result<Value> {
        Ok(match self {
            Field::Null => Value::Null,
            Field::Infinite(negative) if numeric => {
                let text = if negative { "-Infinity" } else { "Infinity" };
                Value::Numeric(numeric_in(text, -1).map_err(type_error)?)
            }
            Field::Infinite(negative) => {
                Value::Float8(if negative { f64::NEG_INFINITY } else { f64::INFINITY })
            }
            Field::Int(v) if numeric => Value::Numeric(Numeric::from_integer(i128::from(v))),
            Field::Int(v) => Value::Float8(v as f64),
            Field::Fixed { value, scale, .. } if numeric => {
                Value::Numeric(Numeric::from_decimal(value, scale))
            }
            Field::Fixed { float, .. } => Value::Float8(float),
            Field::Numeric { numeric: n, .. } if numeric => Value::Numeric(n),
            Field::Numeric { float, .. } => Value::Float8(float),
            Field::Julian { day, usecs } if numeric => {
                let day = Numeric::from_integer(i128::from(day));
                let frac = Numeric::from_integer(i128::from(usecs))
                    .div(&Numeric::from_integer(i128::from(USECS_PER_DAY)))
                    .map_err(type_error)?;
                Value::Numeric(day.add(&frac).map_err(type_error)?)
            }
            Field::Julian { day, usecs } => {
                let secs = (usecs / USECS_PER_SEC) as f64;
                let fsec = (usecs % USECS_PER_SEC) as f64;
                Value::Float8(f64::from(day) + (secs + fsec / 1_000_000.0) / 86_400.0)
            }
        })
    }
}

fn not_supported(lowunits: &str, ty: &str) -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("unit \"{lowunits}\" not supported for type {ty}"),
    )
}

fn not_recognized(lowunits: &str, ty: &str) -> Error {
    Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unit \"{lowunits}\" not recognized for type {ty}"),
    )
}

/// `downcase_truncate_identifier` with no warning: the ASCII letters in lower case, cut to fewer than `NAMEDATALEN` bytes at the start of a character.
fn lower_units(v: &Value) -> Result<String> {
    let Value::Text(s) = v else { return Err(bad_value()) };
    let mut end = s.len().min(NAMEDATALEN - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Ok(s[..end].to_ascii_lowercase())
}

/// `j2day`: the day of the week, with Sunday as 0.
fn j2day(julian: i32) -> i32 {
    (julian + 1).rem_euclid(7)
}

/// The Julian day of the Monday of the first ISO week of `year`.
fn iso_week_start(year: i32) -> i32 {
    let day4 = date2j(year, 1, 4);
    day4 - j2day(day4 - 1)
}

/// `date2isoweek` and `date2isoyear`: the ISO week and the ISO year of a date.
fn iso_week(year: i32, mon: i32, mday: i32) -> (i32, i32) {
    let dayn = date2j(year, mon, mday);
    let mut year = year;
    let mut start = iso_week_start(year);
    if dayn < start {
        year -= 1;
        start = iso_week_start(year);
    }
    let mut week = (dayn - start) / 7 + 1;
    if week >= 52 {
        let next = iso_week_start(year + 1);
        if dayn >= next {
            week = (dayn - next) / 7 + 1;
            year += 1;
        }
    }
    (week, year)
}

/// The units that `timestamp_part_common` and `extract_date` compute from the date in the same way. `None` for another unit.
fn date_field(unit: DateUnit, year: i32, mon: i32, mday: i32) -> Option<i64> {
    let v = match unit {
        DateUnit::Day => mday,
        DateUnit::Month => mon,
        DateUnit::Quarter => (mon - 1) / 3 + 1,
        DateUnit::Week => iso_week(year, mon, mday).0,
        // There is no year 0: 1 BC comes before 1 AD.
        DateUnit::Year if year > 0 => year,
        DateUnit::Year => year - 1,
        DateUnit::Decade if year >= 0 => year / 10,
        DateUnit::Decade => -((8 - (year - 1)) / 10),
        DateUnit::Century if year > 0 => (year + 99) / 100,
        DateUnit::Century => -((99 - (year - 1)) / 100),
        DateUnit::Millennium if year > 0 => (year + 999) / 1000,
        DateUnit::Millennium => -((999 - (year - 1)) / 1000),
        DateUnit::IsoYear => match iso_week(year, mon, mday).1 {
            y if y <= 0 => y - 1,
            y => y,
        },
        DateUnit::Dow => j2day(date2j(year, mon, mday)),
        DateUnit::IsoDow => match j2day(date2j(year, mon, mday)) {
            0 => 7,
            d => d,
        },
        DateUnit::Doy => date2j(year, mon, mday) - date2j(year, 1, 1) + 1,
        _ => return None,
    };
    Some(i64::from(v))
}

/// `microseconds`, `milliseconds` and `seconds` from `tm_sec` and `fsec`. `None` for another unit.
#[allow(clippy::cast_precision_loss)]
fn seconds(unit: DateUnit, sec: i64, fsec: i64) -> Option<Field> {
    let usecs = sec * USECS_PER_SEC + fsec;
    Some(match unit {
        DateUnit::Microsecond => Field::Int(usecs),
        DateUnit::Millisecond => Field::Fixed {
            value: i128::from(usecs),
            scale: 3,
            float: sec as f64 * 1000.0 + fsec as f64 / 1000.0,
        },
        DateUnit::Second => Field::Fixed {
            value: i128::from(usecs),
            scale: 6,
            float: sec as f64 + fsec as f64 / 1_000_000.0,
        },
        _ => return None,
    })
}

/// The fields of a time of day: the units of `time2tm` and `timestamp2tm` that come from the time. `None` for another unit.
fn time_field(unit: DateUnit, time: i64) -> Option<Field> {
    match unit {
        DateUnit::Hour => Some(Field::Int(time / USECS_PER_HOUR)),
        DateUnit::Minute => Some(Field::Int(time % USECS_PER_HOUR / USECS_PER_MINUTE)),
        _ => {
            let rest = time % USECS_PER_MINUTE;
            seconds(unit, rest / USECS_PER_SEC, rest % USECS_PER_SEC)
        }
    }
}

/// The zone units of a `timestamptz` and a `timetz`, from the offset east of UTC in seconds. `None` for another unit.
fn zone_field(unit: DateUnit, east: i32) -> Option<Field> {
    let v = match unit {
        DateUnit::Timezone => east,
        DateUnit::TimezoneMinute => east / 60 % 60,
        DateUnit::TimezoneHour => east / 3600,
        _ => return None,
    };
    Some(Field::Int(i64::from(v)))
}

/// `timestamp_part_common` and `timestamptz_part_common`.
#[allow(clippy::cast_precision_loss)]
fn timestamp_part(call: &Call<'_>, args: &[Value], tz: bool) -> Result<Field> {
    let [units, value] = args else { return Err(bad_value()) };
    let (Value::Timestamp(ts) | Value::TimestampTz(ts)) = *value else {
        return Err(bad_value());
    };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode(&lowunits);
    let ty = if tz { "timestamp with time zone" } else { "timestamp without time zone" };
    if infinite(ts) {
        // `NonFiniteTimestampTzPart`.
        return match unit {
            DateUnit::Unknown => Err(not_recognized(&lowunits, ty)),
            DateUnit::Year
            | DateUnit::Decade
            | DateUnit::Century
            | DateUnit::Millennium
            | DateUnit::Julian
            | DateUnit::IsoYear
            | DateUnit::Epoch => Ok(Field::Infinite(ts == TIMESTAMP_NEGATIVE_INFINITY)),
            DateUnit::Reserved => Err(not_supported(&lowunits, ty)),
            _ => Ok(Field::Null),
        };
    }
    if unit.is_units() {
        let tm = Tm::of(ts)?;
        let (tm, east) = if tz {
            let (local, east) = local_time(call.session, ts)?;
            (Tm::of(local)?, Some(east))
        } else {
            (tm, None)
        };
        if let Some(east) = east
            && let Some(field) = zone_field(unit, east)
        {
            return Ok(field);
        }
        if unit == DateUnit::Julian {
            return Ok(Field::Julian { day: date2j(tm.year, tm.mon, tm.mday), usecs: tm.time });
        }
        if let Some(field) = time_field(unit, tm.time) {
            return Ok(field);
        }
        return match date_field(unit, tm.year, tm.mon, tm.mday) {
            Some(v) => Ok(Field::Int(v)),
            None => Err(not_supported(&lowunits, ty)),
        };
    }
    match unit {
        DateUnit::Epoch if ts < i64::MAX + EPOCH => Ok(Field::Fixed {
            value: i128::from(ts - EPOCH),
            scale: 6,
            float: (ts - EPOCH) as f64 / 1_000_000.0,
        }),
        DateUnit::Epoch => {
            // The difference overflows an `int8`, so it is computed in `numeric`, where the division rounds to the scale that `numeric_div` selects, and in `float8`.
            let epoch = Numeric::from_integer(i128::from(ts))
                .sub(&Numeric::from_integer(i128::from(EPOCH)))
                .and_then(|n| n.div(&Numeric::from_integer(i128::from(USECS_PER_SEC))))
                .and_then(|n| n.round(6))
                .map_err(type_error)?;
            Ok(Field::Numeric { numeric: epoch, float: (ts as f64 - EPOCH as f64) / 1_000_000.0 })
        }
        DateUnit::Reserved => Err(not_supported(&lowunits, ty)),
        _ => Err(not_recognized(&lowunits, ty)),
    }
}

/// `interval_part_common`.
fn interval_part(args: &[Value]) -> Result<Field> {
    const TYPE: &str = "interval";
    let [units, Value::Interval(interval)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode(&lowunits);
    if *interval == Interval::INFINITY || *interval == Interval::NEGATIVE_INFINITY {
        // `NonFiniteIntervalPart`.
        return match unit {
            DateUnit::Unknown => Err(not_recognized(&lowunits, TYPE)),
            DateUnit::Microsecond
            | DateUnit::Millisecond
            | DateUnit::Second
            | DateUnit::Minute
            | DateUnit::Week
            | DateUnit::Month
            | DateUnit::Quarter => Ok(Field::Null),
            DateUnit::Hour
            | DateUnit::Day
            | DateUnit::Year
            | DateUnit::Decade
            | DateUnit::Century
            | DateUnit::Millennium
            | DateUnit::Epoch => Ok(Field::Infinite(*interval == Interval::NEGATIVE_INFINITY)),
            _ => Err(not_supported(&lowunits, TYPE)),
        };
    }
    if unit.is_units() {
        // `interval2itm`.
        let year = interval.month / 12;
        let mon = interval.month % 12;
        if let Some(field) = time_field(unit, interval.time) {
            return Ok(field);
        }
        let v = match unit {
            DateUnit::Day => interval.day,
            DateUnit::Week => interval.day / 7,
            DateUnit::Month => mon,
            // A field of a negative interval is the negative of the field of the interval with the other sign.
            DateUnit::Quarter if interval.month >= 0 => mon / 3 + 1,
            DateUnit::Quarter => -((interval.month.wrapping_neg() % 12) / 3 + 1),
            DateUnit::Year => year,
            DateUnit::Decade => year / 10,
            DateUnit::Century => year / 100,
            DateUnit::Millennium => year / 1000,
            _ => return Err(not_supported(&lowunits, TYPE)),
        };
        return Ok(Field::Int(i64::from(v)));
    }
    if unit != DateUnit::Epoch {
        return Err(not_recognized(&lowunits, TYPE));
    }
    // A year is 365.25 days and a month is 30 days. Four times the days is an integer.
    let secs = (1461 * i64::from(interval.month / 12)
        + 120 * i64::from(interval.month % 12)
        + 4 * i64::from(interval.day))
        * (86_400 / 4);
    #[allow(clippy::cast_precision_loss)]
    let mut float = interval.time as f64 / 1_000_000.0;
    float += 365.25 * 86_400.0 * f64::from(interval.month / 12);
    float += 30.0 * 86_400.0 * f64::from(interval.month % 12);
    float += 86_400.0 * f64::from(interval.day);
    let value = i128::from(secs) * i128::from(USECS_PER_SEC) + i128::from(interval.time);
    Ok(Field::Fixed { value, scale: 6, float })
}

/// `extract_date`. `date_part` of a date is in SQL and casts the date to a timestamp.
#[allow(clippy::cast_possible_wrap)]
fn date_part(args: &[Value]) -> Result<Field> {
    const TYPE: &str = "date";
    let [units, Value::Date(date)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode(&lowunits);
    if (*date == DATE_INFINITY || *date == DATE_NEGATIVE_INFINITY) && unit != DateUnit::Unknown {
        return match unit {
            DateUnit::Day
            | DateUnit::Month
            | DateUnit::Quarter
            | DateUnit::Week
            | DateUnit::Dow
            | DateUnit::IsoDow
            | DateUnit::Doy => Ok(Field::Null),
            DateUnit::Year
            | DateUnit::Decade
            | DateUnit::Century
            | DateUnit::Millennium
            | DateUnit::Julian
            | DateUnit::IsoYear
            | DateUnit::Epoch => Ok(Field::Infinite(*date == DATE_NEGATIVE_INFINITY)),
            _ => Err(not_supported(&lowunits, TYPE)),
        };
    }
    let julian = i64::from(*date) + i64::from(POSTGRES_EPOCH_JDATE);
    if unit.is_units() {
        if unit == DateUnit::Julian {
            return Ok(Field::Int(julian));
        }
        let (year, mon, mday) = j2date(i32::try_from(julian).map_err(|_| bad_value())?);
        return match date_field(unit, year, mon as i32, mday as i32) {
            Some(v) => Ok(Field::Int(v)),
            None => Err(not_supported(&lowunits, TYPE)),
        };
    }
    match unit {
        DateUnit::Epoch => Ok(Field::Int((julian - i64::from(UNIX_EPOCH_JDATE)) * 86_400)),
        DateUnit::Reserved => Err(not_supported(&lowunits, TYPE)),
        _ => Err(not_recognized(&lowunits, TYPE)),
    }
}

/// `time_part_common`.
fn time_part(args: &[Value]) -> Result<Field> {
    const TYPE: &str = "time without time zone";
    let [units, Value::Time(time)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode(&lowunits);
    if unit.is_units() {
        return time_field(unit, *time).ok_or_else(|| not_supported(&lowunits, TYPE));
    }
    if unit != DateUnit::Epoch {
        return Err(not_recognized(&lowunits, TYPE));
    }
    #[allow(clippy::cast_precision_loss)]
    let float = *time as f64 / 1_000_000.0;
    Ok(Field::Fixed { value: i128::from(*time), scale: 6, float })
}

/// `timetz_part_common`. The zone of a `timetz` is in seconds west of UTC.
fn timetz_part(args: &[Value]) -> Result<Field> {
    const TYPE: &str = "time with time zone";
    let [units, Value::TimeTz(time, zone)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode(&lowunits);
    if unit.is_units() {
        return zone_field(unit, -*zone)
            .or_else(|| time_field(unit, *time))
            .ok_or_else(|| not_supported(&lowunits, TYPE));
    }
    if unit != DateUnit::Epoch {
        return Err(not_recognized(&lowunits, TYPE));
    }
    #[allow(clippy::cast_precision_loss)]
    let float = *time as f64 / 1_000_000.0 + f64::from(*zone);
    let value = i128::from(*time) + i128::from(*zone) * i128::from(USECS_PER_SEC);
    Ok(Field::Fixed { value, scale: 6, float })
}

fn extract_timestamp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    timestamp_part(call, args, false)?.value(true)
}

fn extract_timestamptz(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    timestamp_part(call, args, true)?.value(true)
}

fn timestamp_part_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    timestamp_part(call, args, false)?.value(false)
}

fn timestamptz_part_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    timestamp_part(call, args, true)?.value(false)
}

fn extract_interval(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    interval_part(args)?.value(true)
}

fn interval_part_float(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    interval_part(args)?.value(false)
}

fn extract_date(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    date_part(args)?.value(true)
}

fn extract_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    time_part(args)?.value(true)
}

fn time_part_float(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    time_part(args)?.value(false)
}

fn extract_timetz(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    timetz_part(args)?.value(true)
}

fn timetz_part_float(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    timetz_part(args)?.value(false)
}

/// The units that `date_trunc` gives back an infinite timestamp for.
fn truncates(unit: DateUnit) -> bool {
    matches!(
        unit,
        DateUnit::Week
            | DateUnit::Millennium
            | DateUnit::Century
            | DateUnit::Decade
            | DateUnit::Year
            | DateUnit::Quarter
            | DateUnit::Month
            | DateUnit::Day
            | DateUnit::Hour
            | DateUnit::Minute
            | DateUnit::Second
            | DateUnit::Millisecond
            | DateUnit::Microsecond
    )
}

/// The fields cut to `unit`, as the cases of `timestamp_trunc` fall through. `None` for a unit that it does not support, and else whether the zone offset must be found again for the new fields, as `redotz` says in `timestamptz_trunc_internal`.
fn trunc_tm(tm: &mut Tm, unit: DateUnit) -> Option<bool> {
    if unit == DateUnit::Week {
        let (week, _) = iso_week(tm.year, tm.mon, tm.mday);
        // Week 52 or 53 in January is a week of the year before, and week 1 in December is a week of the next year.
        if week >= 52 && tm.mon == 1 {
            tm.year -= 1;
        }
        if week <= 1 && tm.mon == 12 {
            tm.year += 1;
        }
        let (year, mon, mday) = j2date(iso_week_start(tm.year) + (week - 1) * 7);
        #[allow(clippy::cast_possible_wrap)]
        let (mon, mday) = (mon as i32, mday as i32);
        (tm.year, tm.mon, tm.mday, tm.time) = (year, mon, mday, 0);
        return Some(true);
    }
    let rank = match unit {
        DateUnit::Millennium => 0,
        DateUnit::Century => 1,
        DateUnit::Decade => 2,
        DateUnit::Year => 3,
        DateUnit::Quarter => 4,
        DateUnit::Month => 5,
        DateUnit::Day => 6,
        DateUnit::Hour => 7,
        DateUnit::Minute => 8,
        DateUnit::Second => 9,
        DateUnit::Millisecond => 10,
        DateUnit::Microsecond => 11,
        _ => return None,
    };
    let year = tm.year;
    // The first year of a millennium is 1, 1001 or 2001, and -1000 before 1 AD. A century and a decade are the same.
    if rank == 0 {
        tm.year = if year > 0 {
            (year + 999) / 1000 * 1000 - 999
        } else {
            -((999 - (year - 1)) / 1000) * 1000 + 1
        };
    }
    let year = tm.year;
    if rank <= 1 {
        tm.year = if year > 0 {
            (year + 99) / 100 * 100 - 99
        } else {
            -((99 - (year - 1)) / 100) * 100 + 1
        };
    }
    if rank == 2 {
        tm.year = if year > 0 { year / 10 * 10 } else { -((8 - (year - 1)) / 10) * 10 };
    }
    if rank <= 3 {
        tm.mon = 1;
    }
    if rank <= 4 {
        tm.mon = 3 * ((tm.mon - 1) / 3) + 1;
    }
    if rank <= 5 {
        tm.mday = 1;
    }
    let keep = match rank {
        ..=6 => 0,
        7 => USECS_PER_HOUR,
        8 => USECS_PER_MINUTE,
        9 => USECS_PER_SEC,
        10 => 1000,
        _ => 1,
    };
    if keep > 0 {
        tm.time = tm.time / keep * keep;
    } else {
        tm.time = 0;
    }
    Some(rank <= 6)
}

/// `timestamp_trunc`.
fn timestamp_trunc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    const TYPE: &str = "timestamp without time zone";
    let [units, Value::Timestamp(ts)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode_units(&lowunits);
    if !unit.is_units() {
        return Err(not_recognized(&lowunits, TYPE));
    }
    if infinite(*ts) {
        return if truncates(unit) {
            Ok(Value::Timestamp(*ts))
        } else {
            Err(not_supported(&lowunits, TYPE))
        };
    }
    let mut tm = Tm::of(*ts)?;
    trunc_tm(&mut tm, unit).ok_or_else(|| not_supported(&lowunits, TYPE))?;
    tm.timestamp(None).map(Value::Timestamp)
}

/// `timestamptz_trunc`: the fields of the local time in the zone of the session are cut.
fn timestamptz_trunc(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    const TYPE: &str = "timestamp with time zone";
    let [units, Value::TimestampTz(ts)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode_units(&lowunits);
    if !unit.is_units() {
        return Err(not_recognized(&lowunits, TYPE));
    }
    if infinite(*ts) {
        return if truncates(unit) {
            Ok(Value::TimestampTz(*ts))
        } else {
            Err(not_supported(&lowunits, TYPE))
        };
    }
    Tm::of(*ts)?;
    let (local, east) = local_time(call.session, *ts)?;
    let mut tm = Tm::of(local)?;
    let redo = trunc_tm(&mut tm, unit).ok_or_else(|| not_supported(&lowunits, TYPE))?;
    let result = if redo {
        tm.timestamp(Some(call.session.zone()?.as_ref()))?
    } else {
        tm.timestamp_west(-east)?
    };
    Ok(Value::TimestampTz(result))
}

/// `interval_trunc`.
fn interval_trunc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    const TYPE: &str = "interval";
    let [units, Value::Interval(interval)] = args else { return Err(bad_value()) };
    let lowunits = lower_units(units)?;
    let unit = DateUnit::decode_units(&lowunits);
    if !unit.is_units() {
        return Err(not_recognized(&lowunits, TYPE));
    }
    let unsupported = || {
        let error = not_supported(&lowunits, TYPE);
        if unit == DateUnit::Week {
            error.with_detail("Months usually have fractional weeks.")
        } else {
            error
        }
    };
    let rank = match unit {
        DateUnit::Millennium => 0,
        DateUnit::Century => 1,
        DateUnit::Decade => 2,
        DateUnit::Year => 3,
        DateUnit::Quarter => 4,
        DateUnit::Month => 5,
        DateUnit::Day => 6,
        DateUnit::Hour => 7,
        DateUnit::Minute => 8,
        DateUnit::Second => 9,
        DateUnit::Millisecond => 10,
        DateUnit::Microsecond => 11,
        _ => return Err(unsupported()),
    };
    if *interval == Interval::INFINITY || *interval == Interval::NEGATIVE_INFINITY {
        return Ok(Value::Interval(*interval));
    }
    // `interval2itm`, the cases that fall through, and `itm2interval`. The divisions go to zero, so a negative field stays negative.
    let mut year = interval.month / 12;
    let mut mon = interval.month % 12;
    let mut day = interval.day;
    if rank == 0 {
        year = year / 1000 * 1000;
    }
    if rank <= 1 {
        year = year / 100 * 100;
    }
    if rank <= 2 {
        year = year / 10 * 10;
    }
    if rank <= 3 {
        mon = 0;
    }
    if rank <= 4 {
        mon = 3 * (mon / 3);
    }
    if rank <= 5 {
        day = 0;
    }
    let keep = match rank {
        ..=6 => 0,
        7 => USECS_PER_HOUR,
        8 => USECS_PER_MINUTE,
        9 => USECS_PER_SEC,
        10 => 1000,
        _ => 1,
    };
    let time = if keep > 0 { interval.time / keep * keep } else { 0 };
    let result = Interval { time, day, month: year * 12 + mon };
    if result == Interval::INFINITY || result == Interval::NEGATIVE_INFINITY {
        return Err(Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "interval out of range"));
    }
    Ok(Value::Interval(result))
}

/// `timestamp_bin` and `timestamptz_bin`: the start of the bin of `stride` that holds the timestamp, with the bins counted from `origin`.
fn date_bin(stride: &Interval, ts: i64, origin: i64) -> Result<i64> {
    let range = |message: &str| Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, message);
    if infinite(ts) {
        return Ok(ts);
    }
    if infinite(origin) {
        return Err(range("origin out of range"));
    }
    if *stride == Interval::INFINITY || *stride == Interval::NEGATIVE_INFINITY {
        return Err(range("timestamps cannot be binned into infinite intervals"));
    }
    if stride.month != 0 {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "timestamps cannot be binned into intervals containing months or years",
        ));
    }
    let stride = i64::from(stride.day)
        .checked_mul(USECS_PER_DAY)
        .and_then(|s| s.checked_add(stride.time))
        .ok_or_else(|| range("interval out of range"))?;
    if stride <= 0 {
        return Err(range("stride must be greater than zero"));
    }
    let diff = ts.checked_sub(origin).ok_or_else(|| range("interval out of range"))?;
    let modulo = diff % stride;
    let result = origin + (diff - modulo);
    // The bin of a timestamp before the origin starts before it, not after it.
    if modulo < 0 {
        return result
            .checked_sub(stride)
            .filter(|&r| valid_timestamp(r))
            .ok_or_else(|| range("timestamp out of range"));
    }
    Ok(result)
}

fn timestamp_bin(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Interval(stride), Value::Timestamp(ts), Value::Timestamp(origin)] = args else {
        return Err(bad_value());
    };
    date_bin(stride, *ts, *origin).map(Value::Timestamp)
}

fn timestamptz_bin(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Interval(stride), Value::TimestampTz(ts), Value::TimestampTz(origin)] = args else {
        return Err(bad_value());
    };
    date_bin(stride, *ts, *origin).map(Value::TimestampTz)
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "extract_timestamp" => extract_timestamp,
        "extract_timestamptz" => extract_timestamptz,
        "timestamp_part" => timestamp_part_float,
        "timestamptz_part" => timestamptz_part_float,
        "extract_interval" => extract_interval,
        "interval_part" => interval_part_float,
        "extract_date" => extract_date,
        "extract_time" => extract_time,
        "time_part" => time_part_float,
        "extract_timetz" => extract_timetz,
        "timetz_part" => timetz_part_float,
        "timestamp_trunc" => timestamp_trunc,
        "timestamptz_trunc" => timestamptz_trunc,
        "interval_trunc" => interval_trunc,
        "timestamp_bin" => timestamp_bin,
        "timestamptz_bin" => timestamptz_bin,
        _ => return None,
    })
}

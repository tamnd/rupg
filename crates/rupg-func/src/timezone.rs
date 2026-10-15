//! The `timezone` functions of `AT TIME ZONE` and `AT LOCAL`: a timestamp, a `timestamptz` or a `timetz` in a zone that a name or an interval gives. Also the zone of a name for `date_trunc`. The rules are those of `timestamp_zone`, `timestamp_izone`, `timestamptz_zone`, `timestamptz_izone`, `timetz_zone`, `timetz_izone`, `timetz_at_local` and `DecodeTimezoneName`.

use std::rc::Rc;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    FixedZone, Interval, TimeZone, USECS_PER_DAY, USECS_PER_SEC, Value, ZoneName,
    decode_numeric_timezone, decode_timezone_name, interval_out, local_offset,
};

use crate::cast::{infinite, timestamp_range, valid_timestamp};
use crate::datetime::Tm;
use crate::{Call, Kernel, bad_value, type_error};

/// `TZ_STRLEN_MAX`: the name of a zone is cut to this number of bytes.
const TZ_STRLEN_MAX: usize = 255;
/// The seconds from 1970-01-01 to 2000-01-01.
const UNIX_TO_POSTGRES_SECS: i64 = 946_684_800;

/// A zone as `DecodeTimezoneName` finds it.
enum Named {
    /// An abbreviation with one offset, in seconds east of UTC.
    Fixed(i32),
    /// An abbreviation whose offset changed over time, and its zone.
    Dynamic(Rc<dyn TimeZone>),
    /// A zone of the data.
    Zone(Rc<dyn TimeZone>),
}

/// `text_to_cstring_buffer` with a buffer of `TZ_STRLEN_MAX + 1` bytes.
fn zone_name(v: &Value) -> Result<&str> {
    let Value::Text(s) = v else { return Err(bad_value()) };
    let mut end = s.len().min(TZ_STRLEN_MAX);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Ok(&s[..end])
}

/// `DecodeTimezoneName` with the abbreviations of the session.
fn decode(call: &Call<'_>, name: &str) -> Result<Named> {
    let session = call.session.zone()?;
    let cx = crate::io::datetime_input(call.session, session.as_ref());
    let named = decode_timezone_name(name, &cx).map_err(type_error)?;
    Ok(match named {
        ZoneName::Fixed(east) => Named::Fixed(east),
        ZoneName::Dynamic(None) => Named::Dynamic(Rc::clone(&session)),
        ZoneName::Dynamic(Some(zone)) => Named::Dynamic(Rc::new(zone)),
        ZoneName::Zone(zone) => Named::Zone(Rc::new(zone)),
    })
}

/// `lookup_timezone`: the zone of a name, with a zone of one offset for an abbreviation with one offset.
pub(crate) fn lookup_timezone(call: &Call<'_>, zone: &Value) -> Result<Rc<dyn TimeZone>> {
    Ok(match decode(call, zone_name(zone)?)? {
        Named::Fixed(offset) => Rc::new(FixedZone { offset, abbrev: String::new() }),
        Named::Dynamic(zone) | Named::Zone(zone) => zone,
    })
}

/// `parse_sane_timezone`: the offset in seconds west of UTC of a zone at a local time. A numeric zone comes first, then the abbreviations and the zones of the data.
pub(crate) fn parse_sane_timezone(call: &Call<'_>, local: i64, zone: &Value) -> Result<i32> {
    let name = zone_name(zone)?;
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid input syntax for type numeric time zone: \"{name}\""),
        )
        .with_hint("Numeric time zones must have \"-\" or \"+\" as first character."));
    }
    if let Some(west) = decode_numeric_timezone(name).map_err(type_error)? {
        return Ok(west);
    }
    Ok(match decode(call, name)? {
        Named::Fixed(east) => -east,
        Named::Dynamic(zone) => abbrev_offset(local, name, zone.as_ref()),
        Named::Zone(zone) => local_offset(whole_seconds(local), zone.as_ref()).0,
    })
}

/// `timestamptz_to_time_t`: the seconds since 1970-01-01 UTC, with the division toward zero.
fn time_t(ts: i64) -> i64 {
    ts / USECS_PER_SEC + UNIX_TO_POSTGRES_SECS
}

/// The seconds since 1970-01-01 of the whole seconds of a timestamp, as `timestamp2tm` gives them.
fn whole_seconds(ts: i64) -> i64 {
    ts.div_euclid(USECS_PER_SEC) + UNIX_TO_POSTGRES_SECS
}

/// The offset east of UTC in seconds of a zone at an instant.
fn east_at(zone: &dyn TimeZone, ts: i64) -> i32 {
    zone.at(ts.div_euclid(USECS_PER_SEC) + UNIX_TO_POSTGRES_SECS).0
}

/// `timestamp2tm` with a zone: the local time of an instant in the zone.
fn local_in(zone: &dyn TimeZone, ts: i64) -> Result<i64> {
    Tm::of(ts)?;
    let local =
        ts.checked_add(i64::from(east_at(zone, ts)) * USECS_PER_SEC).ok_or_else(timestamp_range)?;
    Tm::of(local)?;
    Ok(local)
}

/// `DetermineTimeZoneAbbrevOffset`: the offset west of UTC of a local time with an abbreviation of a zone.
fn abbrev_offset(local: i64, abbrev: &str, zone: &dyn TimeZone) -> i32 {
    let (west, instant) = local_offset(whole_seconds(local), zone);
    match zone.abbrev_at(&abbrev.to_ascii_uppercase(), instant) {
        Some((east, _)) => -east,
        None => west,
    }
}

/// `DetermineTimeZoneAbbrevOffsetTS`: the offset west of UTC of an instant with an abbreviation of a zone.
fn abbrev_offset_ts(ts: i64, abbrev: &str, zone: &dyn TimeZone) -> Result<i32> {
    if let Some((east, _)) = zone.abbrev_at(&abbrev.to_ascii_uppercase(), time_t(ts)) {
        return Ok(-east);
    }
    let local = local_in(zone, ts)?;
    Ok(local_offset(whole_seconds(local), zone).0)
}

/// The offset west of UTC in seconds of an interval as a zone, which has no months and no days.
fn interval_zone(call: &Call<'_>, v: &Value) -> Result<i32> {
    let Value::Interval(zone) = v else { return Err(bad_value()) };
    let problem = if *zone == Interval::INFINITY || *zone == Interval::NEGATIVE_INFINITY {
        Some("must be finite")
    } else if zone.month != 0 || zone.day != 0 {
        Some("must not include months or days")
    } else {
        None
    };
    if let Some(problem) = problem {
        let mut text = Vec::new();
        interval_out(zone, call.session.interval_style(), &mut text);
        let text = String::from_utf8_lossy(&text);
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("interval time zone \"{text}\" {problem}"),
        ));
    }
    // The offset is an `int` in C, so a large interval wraps.
    #[allow(clippy::cast_possible_truncation)]
    let east = (zone.time / USECS_PER_SEC) as i32;
    Ok(east.wrapping_neg())
}

/// `dt2local`: the timestamp less an offset west of UTC.
fn dt2local(ts: i64, west: i32) -> i64 {
    ts.wrapping_sub(i64::from(west).wrapping_mul(USECS_PER_SEC))
}

fn checked(ts: i64) -> Result<i64> {
    if valid_timestamp(ts) { Ok(ts) } else { Err(timestamp_range()) }
}

/// `timestamp_zone`: a local time in a zone as a `timestamptz`.
fn timestamp_zone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::Timestamp(ts)] = args else { return Err(bad_value()) };
    if infinite(*ts) {
        return Ok(Value::TimestampTz(*ts));
    }
    let name = zone_name(zone)?;
    let result = match decode(call, name)? {
        Named::Fixed(east) => dt2local(*ts, east),
        Named::Dynamic(zone) => {
            Tm::of(*ts)?;
            dt2local(*ts, -abbrev_offset(*ts, name, zone.as_ref()))
        }
        Named::Zone(zone) => Tm::of(*ts)?.timestamp(Some(zone.as_ref()))?,
    };
    checked(result).map(Value::TimestampTz)
}

/// `timestamp_izone`: a local time in the zone of an interval as a `timestamptz`.
fn timestamp_izone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::Timestamp(ts)] = args else { return Err(bad_value()) };
    if infinite(*ts) {
        return Ok(Value::TimestampTz(*ts));
    }
    let west = interval_zone(call, zone)?;
    checked(dt2local(*ts, west.wrapping_neg())).map(Value::TimestampTz)
}

/// `timestamptz_zone`: the local time of a `timestamptz` in a zone.
fn timestamptz_zone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::TimestampTz(ts)] = args else { return Err(bad_value()) };
    if infinite(*ts) {
        return Ok(Value::Timestamp(*ts));
    }
    let name = zone_name(zone)?;
    let result = match decode(call, name)? {
        Named::Fixed(east) => dt2local(*ts, -east),
        Named::Dynamic(zone) => dt2local(*ts, abbrev_offset_ts(*ts, name, zone.as_ref())?),
        Named::Zone(zone) => Tm::of(local_in(zone.as_ref(), *ts)?)?.timestamp(None)?,
    };
    checked(result).map(Value::Timestamp)
}

/// `timestamptz_izone`: the local time of a `timestamptz` in the zone of an interval.
fn timestamptz_izone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::TimestampTz(ts)] = args else { return Err(bad_value()) };
    if infinite(*ts) {
        return Ok(Value::Timestamp(*ts));
    }
    let west = interval_zone(call, zone)?;
    checked(dt2local(*ts, west)).map(Value::Timestamp)
}

/// The time of a `timetz` in an offset west of UTC.
fn rotate(time: i64, zone: i32, west: i32) -> Value {
    let mut time = time + i64::from(zone - west) * USECS_PER_SEC;
    while time < 0 {
        time += USECS_PER_DAY;
    }
    if time >= USECS_PER_DAY {
        time %= USECS_PER_DAY;
    }
    Value::TimeTz(time, west)
}

/// `timetz_zone`: a `timetz` in a zone, with the rules of the zone at the start of the transaction.
fn timetz_zone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::TimeTz(time, offset)] = args else { return Err(bad_value()) };
    timetz_in_zone(call, zone_name(zone)?, *time, *offset)
}

fn timetz_in_zone(call: &Call<'_>, name: &str, time: i64, offset: i32) -> Result<Value> {
    let west = match decode(call, name)? {
        Named::Fixed(east) => -east,
        Named::Dynamic(zone) => {
            abbrev_offset_ts(call.session.transaction_start(), name, zone.as_ref())?
        }
        Named::Zone(zone) => -east_at(zone.as_ref(), call.session.transaction_start()),
    };
    Ok(rotate(time, offset, west))
}

/// `timetz_izone`: a `timetz` in the zone of an interval.
fn timetz_izone(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [zone, Value::TimeTz(time, offset)] = args else { return Err(bad_value()) };
    let west = interval_zone(call, zone)?;
    Ok(rotate(*time, *offset, west))
}

/// `timetz_at_local`: `timetz_zone` with the name of the zone of the session.
fn timetz_at_local(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TimeTz(time, offset)] = args else { return Err(bad_value()) };
    let name = call.session.setting("timezone").ok_or_else(bad_value)?;
    timetz_in_zone(call, &name, *time, *offset)
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "timestamp_zone" => timestamp_zone,
        "timestamp_izone" => timestamp_izone,
        "timestamptz_zone" => timestamptz_zone,
        "timestamptz_izone" => timestamptz_izone,
        "timetz_zone" => timetz_zone,
        "timetz_izone" => timetz_izone,
        "timetz_at_local" => timetz_at_local,
        _ => return None,
    })
}

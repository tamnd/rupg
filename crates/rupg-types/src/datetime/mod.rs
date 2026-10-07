//! `date`, `time`, `timetz`, `timestamp`, `timestamptz` and `interval`: the binary forms and the text output.
//!
//! The values are the integers of PostgreSQL. A date is days and a timestamp is microseconds since 2000-01-01, a time is microseconds since midnight, and a `timetz` zone is seconds west of UTC. The engine counts from 1970-01-01, so the row encoder adds [`UNIX_TO_POSTGRES_DAYS`] or [`UNIX_TO_POSTGRES_USECS`] after it has tested for infinity.
//!
//! The output functions are ports of `EncodeDateOnly`, `EncodeTimeOnly`, `EncodeDateTime` and `EncodeInterval` in `src/backend/utils/adt/datetime.c`, with `DateStyle` and `IntervalStyle` as arguments. A `timestamptz` prints in the session time zone, which is a [`TimeZone`]: the caller gives the offset and the abbreviation at each instant. The receive functions are ports of the functions in `date.c` and `timestamp.c`, with the same range checks and the same rounding to the typmod.
//!
//! Lifted from `crates/rudb-pgtypes/src/datetime/mod.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

mod decode;

use rupg_common::SqlState;

pub use decode::{
    Abbrev, DateTimeInput, NoZones, ZoneAbbrevs, ZoneLookup, date_in, interval_in, time_in,
    timestamp_in, timestamptz_in, timetz_in,
};

use crate::binary::Recv;
use crate::error::TypeError;
use crate::number::u64_out;
use crate::typmod::{
    INTERVAL_FULL_PRECISION, INTERVAL_FULL_RANGE, IntervalField, MAX_TIME_PRECISION,
};

/// The Julian day of 2000-01-01, the epoch of PostgreSQL.
pub const POSTGRES_EPOCH_JDATE: i32 = 2451545;
/// The Julian day of 1970-01-01, the epoch of the engine.
pub const UNIX_EPOCH_JDATE: i32 = 2440588;
/// Add this to days since 1970-01-01 to get days since 2000-01-01.
pub const UNIX_TO_POSTGRES_DAYS: i32 = UNIX_EPOCH_JDATE - POSTGRES_EPOCH_JDATE;
pub const USECS_PER_SEC: i64 = 1_000_000;
pub const USECS_PER_DAY: i64 = 86_400 * USECS_PER_SEC;
/// Add this to microseconds since 1970-01-01 to get microseconds since 2000-01-01.
pub const UNIX_TO_POSTGRES_USECS: i64 = UNIX_TO_POSTGRES_DAYS as i64 * USECS_PER_DAY;

/// `DATEVAL_NOBEGIN` and `DATEVAL_NOEND`, the dates `-infinity` and `infinity`.
pub const DATE_NEGATIVE_INFINITY: i32 = i32::MIN;
pub const DATE_INFINITY: i32 = i32::MAX;
/// `DT_NOBEGIN` and `DT_NOEND`, the timestamps `-infinity` and `infinity`.
pub const TIMESTAMP_NEGATIVE_INFINITY: i64 = i64::MIN;
pub const TIMESTAMP_INFINITY: i64 = i64::MAX;

/// `DATE_END_JULIAN`, the first Julian day after the last valid date, 5874898-01-01.
const DATE_END_JULIAN: i32 = 2147483494;
/// `MIN_TIMESTAMP` and `END_TIMESTAMP`: 4714-11-24 BC and 294277-01-01.
const MIN_TIMESTAMP: i64 = -211813488000000000;
const END_TIMESTAMP: i64 = 9223371331200000000;
/// `TZDISP_LIMIT`, the limit of the zone of a `timetz`, in seconds.
const TZDISP_LIMIT: i32 = 16 * 3600;
/// `MAXTZLEN`, the most bytes of an abbreviation that the output shows.
const MAX_TZ_LEN: usize = 10;

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The output part of `DateStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateStyle {
    Iso,
    Sql,
    Postgres,
    German,
}

/// The field order part of `DateStyle`. The output uses it only to put the day before the month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    Mdy,
    Dmy,
    Ymd,
}

/// The `DateStyle` setting. The default of `initdb` is `ISO, MDY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateFormat {
    pub style: DateStyle,
    pub order: DateOrder,
}

impl DateFormat {
    pub const ISO_MDY: DateFormat = DateFormat { style: DateStyle::Iso, order: DateOrder::Mdy };

    fn day_first(self) -> bool {
        self.order == DateOrder::Dmy
    }
}

/// The `IntervalStyle` setting. The default is `Postgres`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalStyle {
    Postgres,
    PostgresVerbose,
    SqlStandard,
    Iso8601,
}

/// The rules of a time zone: the offset east of UTC in seconds and the abbreviation, at an instant in seconds since 1970-01-01 UTC, as `pg_localtime` gives them.
pub trait TimeZone {
    fn at(&self, unix_seconds: i64) -> (i32, &str);

    /// The offset east of UTC before the first change of the rules after an instant, and the instant and the offset of that change, as `pg_next_dst_boundary` gives them.
    fn next_change(&self, unix_seconds: i64) -> (i32, Option<(i64, i32)>) {
        (self.at(unix_seconds).0, None)
    }

    /// The meaning of an abbreviation in upper case in this zone over all of its history, as `pg_interpret_timezone_abbrev` gives it.
    fn abbrev_meaning(&self, _abbrev: &str) -> Option<AbbrevMeaning> {
        None
    }

    /// The offset east of UTC and the daylight saving flag of an abbreviation in upper case at an instant, as `pg_timezone_abbrev_is_known` gives them.
    fn abbrev_at(&self, _abbrev: &str, _unix_seconds: i64) -> Option<(i32, bool)> {
        None
    }

    /// The offset east of UTC if the zone has one offset at all instants.
    fn fixed_offset(&self) -> Option<i32> {
        None
    }
}

/// The meaning of a time zone abbreviation in the zone that uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbbrevMeaning {
    /// One offset east of UTC at all instants, and whether it is daylight saving time.
    Fixed { offset: i32, dst: bool },
    /// The offset changed over the history of the zone.
    Varies,
}

/// A zone with one offset at all instants, such as `UTC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedZone {
    /// Seconds east of UTC.
    pub offset: i32,
    pub abbrev: String,
}

impl FixedZone {
    pub fn utc() -> FixedZone {
        FixedZone { offset: 0, abbrev: "UTC".to_string() }
    }
}

impl TimeZone for FixedZone {
    fn at(&self, _: i64) -> (i32, &str) {
        (self.offset, &self.abbrev)
    }

    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        (!self.abbrev.is_empty() && abbrev == self.abbrev)
            .then_some(AbbrevMeaning::Fixed { offset: self.offset, dst: false })
    }

    fn abbrev_at(&self, abbrev: &str, _: i64) -> Option<(i32, bool)> {
        (!self.abbrev.is_empty() && abbrev == self.abbrev).then_some((self.offset, false))
    }

    fn fixed_offset(&self) -> Option<i32> {
        Some(self.offset)
    }
}

/// An `interval`: months, days and microseconds, which do not convert into each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub time: i64,
    pub day: i32,
    pub month: i32,
}

impl Interval {
    pub const NEGATIVE_INFINITY: Interval =
        Interval { time: i64::MIN, day: i32::MIN, month: i32::MIN };
    pub const INFINITY: Interval = Interval { time: i64::MAX, day: i32::MAX, month: i32::MAX };
}

/// A date of the engine as PostgreSQL holds it. The engine counts days from 1970-01-01 and has `i32::MAX` and `-i32::MAX` for the infinities. PostgreSQL counts days from 2000-01-01 and has [`DATE_INFINITY`] and [`DATE_NEGATIVE_INFINITY`]. A date that PostgreSQL cannot hold is an error.
pub fn date_from_unix(days: i32) -> Result<i32, TypeError> {
    match days {
        i32::MAX => Ok(DATE_INFINITY),
        days if days == -i32::MAX => Ok(DATE_NEGATIVE_INFINITY),
        days => {
            let date = i64::from(days) + i64::from(UNIX_TO_POSTGRES_DAYS);
            let valid =
                i64::from(-POSTGRES_EPOCH_JDATE)..i64::from(DATE_END_JULIAN - POSTGRES_EPOCH_JDATE);
            if valid.contains(&date) { Ok(date as i32) } else { Err(out_of_range("date")) }
        }
    }
}

/// A `timestamp` or a `timestamptz` of the engine as PostgreSQL holds it. The engine counts microseconds from 1970-01-01 and has `i64::MAX` and `-i64::MAX` for the infinities. A value that PostgreSQL cannot hold is an error.
pub fn timestamp_from_unix(micros: i64) -> Result<i64, TypeError> {
    match micros {
        i64::MAX => Ok(TIMESTAMP_INFINITY),
        micros if micros == -i64::MAX => Ok(TIMESTAMP_NEGATIVE_INFINITY),
        micros => micros
            .checked_add(UNIX_TO_POSTGRES_USECS)
            .filter(|ts| (MIN_TIMESTAMP..END_TIMESTAMP).contains(ts))
            .ok_or_else(|| out_of_range("timestamp")),
    }
}

fn out_of_range(what: &str) -> TypeError {
    TypeError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("{what} out of range"))
}

/// `date2j`: the Julian day of a date in the proleptic Gregorian calendar. The year before 1 is 0.
///
/// The arithmetic wraps as the C code does on a value far out of range. The callers check the range before they use the result.
pub fn date2j(year: i32, month: i32, day: i32) -> i32 {
    let (month, year) = if month > 2 {
        (month.wrapping_add(1), year.wrapping_add(4800))
    } else {
        (month.wrapping_add(13), year.wrapping_add(4799))
    };
    let century = year / 100;
    let julian = year.wrapping_mul(365).wrapping_sub(32167);
    let julian = julian.wrapping_add(year / 4 - century + century / 4);
    julian.wrapping_add(7834i32.wrapping_mul(month) / 256).wrapping_add(day)
}

/// `j2date`: the year, the month and the day of a Julian day.
pub fn j2date(jd: i32) -> (i32, u32, u32) {
    let mut julian = (jd as u32).wrapping_add(32044);
    let mut quad = julian / 146097;
    let extra = (julian - quad * 146097) * 4 + 3;
    julian = julian.wrapping_add(60 + quad * 3 + extra / 146097);
    quad = julian / 1461;
    julian -= quad * 1461;
    let mut y = julian * 4 / 1461;
    julian = if y != 0 { (julian + 305) % 365 } else { (julian + 306) % 366 } + 123;
    y += quad * 4;
    let quad = julian * 2141 / 65536;
    (y as i32 - 4800, (quad + 10) % 12 + 1, julian - 7834 * quad / 256)
}

/// The fields of a date and a time, as `struct pg_tm` and `fsec_t` hold them.
struct Fields {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    usec: u32,
}

impl Fields {
    /// `timestamp2tm` with no zone. `None` when the Julian day does not fit in an `int`.
    fn of_timestamp(ts: i64) -> Option<Fields> {
        let date = ts.div_euclid(USECS_PER_DAY) + i64::from(POSTGRES_EPOCH_JDATE);
        let jd = i32::try_from(date).ok().filter(|&jd| jd >= 0)?;
        let time = ts.rem_euclid(USECS_PER_DAY);
        let (year, month, day) = j2date(jd);
        let (hour, minute, second, usec) = split_time(time);
        Some(Fields { year, month, day, hour, minute, second, usec })
    }
}

/// `dt2time`: the hours, the minutes, the seconds and the microseconds of a time of day.
fn split_time(time: i64) -> (u32, u32, u32, u32) {
    let usec = (time % USECS_PER_SEC) as u32;
    let seconds = time / USECS_PER_SEC;
    ((seconds / 3600) as u32, (seconds / 60 % 60) as u32, (seconds % 60) as u32, usec)
}

/// `pg_ultostr_zeropad`: at least `width` digits.
fn zeropad(n: u64, width: usize, out: &mut Vec<u8>) {
    let start = out.len();
    u64_out(n, out);
    let len = out.len() - start;
    if len < width {
        out.splice(start..start, std::iter::repeat_n(b'0', width - len));
    }
}

/// Two digits of a field that is less than 100.
fn two(n: u32, out: &mut Vec<u8>) {
    debug_assert!(n < 100);
    out.extend_from_slice(&[b'0' + (n / 10) as u8, b'0' + (n % 10) as u8]);
}

/// The year as PostgreSQL shows it: four digits at least, and a positive number for a year BC.
fn year(year: i32, out: &mut Vec<u8>) {
    let shown = if year > 0 { year } else { 1 - year };
    zeropad(shown as u64, 4, out);
}

/// `AppendSeconds`: the seconds, and the microseconds with no zeros at the end. The sign is dropped.
fn seconds(sec: i64, usec: i64, fill: bool, out: &mut Vec<u8>) {
    if fill {
        zeropad(sec.unsigned_abs(), 2, out);
    } else {
        u64_out(sec.unsigned_abs(), out);
    }
    if usec != 0 {
        out.push(b'.');
        let mut usec = usec.unsigned_abs();
        let mut digits = 6;
        while usec.is_multiple_of(10) {
            usec /= 10;
            digits -= 1;
        }
        zeropad(usec, digits, out);
    }
}

/// `EncodeTimezone`: the offset east of UTC, with minutes and seconds only when they are not zero.
fn zone_offset(offset: i32, out: &mut Vec<u8>) {
    out.push(if offset >= 0 { b'+' } else { b'-' });
    let total = offset.unsigned_abs();
    let (hour, min, sec) = (total / 3600, total / 60 % 60, total % 60);
    zeropad(u64::from(hour), 2, out);
    if min != 0 || sec != 0 {
        out.push(b':');
        two(min, out);
    }
    if sec != 0 {
        out.push(b':');
        two(sec, out);
    }
}

/// The date part in the order and with the separator of the style.
fn date_part(f: &Fields, format: DateFormat, out: &mut Vec<u8>) {
    let (first, second, sep) = match (format.style, format.day_first()) {
        (DateStyle::Iso, _) => {
            year(f.year, out);
            out.push(b'-');
            two(f.month, out);
            out.push(b'-');
            two(f.day, out);
            return;
        }
        (DateStyle::German, _) => (f.day, f.month, b'.'),
        (DateStyle::Sql, true) => (f.day, f.month, b'/'),
        (DateStyle::Sql, false) => (f.month, f.day, b'/'),
        (DateStyle::Postgres, true) => (f.day, f.month, b'-'),
        (DateStyle::Postgres, false) => (f.month, f.day, b'-'),
    };
    two(first, out);
    out.push(sep);
    two(second, out);
    out.push(sep);
    year(f.year, out);
}

fn time_part(hour: u32, minute: u32, second: u32, usec: u32, out: &mut Vec<u8>) {
    zeropad(u64::from(hour), 2, out);
    out.push(b':');
    two(minute, out);
    out.push(b':');
    seconds(i64::from(second), i64::from(usec), true, out);
}

fn bc(year: i32, out: &mut Vec<u8>) {
    if year <= 0 {
        out.extend_from_slice(b" BC");
    }
}

/// The text output of `date`.
pub fn date_out(date: i32, format: DateFormat, out: &mut Vec<u8>) {
    match date {
        DATE_NEGATIVE_INFINITY => return out.extend_from_slice(b"-infinity"),
        DATE_INFINITY => return out.extend_from_slice(b"infinity"),
        _ => {}
    }
    let (year, month, day) = j2date(date.wrapping_add(POSTGRES_EPOCH_JDATE));
    let fields = Fields { year, month, day, hour: 0, minute: 0, second: 0, usec: 0 };
    date_part(&fields, format, out);
    bc(year, out);
}

/// `date_recv`: the range is the range that `date_in` takes, and the infinities.
pub fn date_recv(recv: &mut Recv<'_>) -> Result<i32, TypeError> {
    let date = recv.i32()?;
    let valid = -POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE;
    if date == DATE_NEGATIVE_INFINITY || date == DATE_INFINITY || valid.contains(&date) {
        Ok(date)
    } else {
        Err(out_of_range("date"))
    }
}

/// The text output of `time`, microseconds since midnight. `24:00:00` is a valid time.
pub fn time_out(time: i64, out: &mut Vec<u8>) {
    let (hour, minute, second, usec) = split_time(time);
    time_part(hour, minute, second, usec, out);
}

/// The text output of `timetz`. The zone is in seconds west of UTC, as PostgreSQL stores it.
pub fn timetz_out(time: i64, zone: i32, out: &mut Vec<u8>) {
    time_out(time, out);
    zone_offset(-zone, out);
}

/// `AdjustTimeForTypmod` and the rounding of `AdjustTimestampForTypmod`: rounds half away from zero to `precision` digits after the point.
fn round_usecs(value: i64, precision: i32) -> i64 {
    let scale = 10i64.pow((MAX_TIME_PRECISION - precision) as u32);
    let half = scale / 2;
    if value >= 0 { (value + half) / scale * scale } else { -((-value + half) / scale * scale) }
}

fn time_value(recv: &mut Recv<'_>) -> Result<i64, TypeError> {
    let time = recv.i64()?;
    if !(0..=USECS_PER_DAY).contains(&time) {
        return Err(out_of_range("time"));
    }
    Ok(time)
}

fn adjust_time(time: i64, typmod: i32) -> i64 {
    if (0..=MAX_TIME_PRECISION).contains(&typmod) { round_usecs(time, typmod) } else { time }
}

/// `time_recv`.
pub fn time_recv(recv: &mut Recv<'_>, typmod: i32) -> Result<i64, TypeError> {
    Ok(adjust_time(time_value(recv)?, typmod))
}

/// `timetz_recv`: the time and the zone in seconds west of UTC.
pub fn timetz_recv(recv: &mut Recv<'_>, typmod: i32) -> Result<(i64, i32), TypeError> {
    let time = time_value(recv)?;
    let zone = recv.i32()?;
    if zone <= -TZDISP_LIMIT || zone >= TZDISP_LIMIT {
        return Err(TypeError::new(
            SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
            "time zone displacement out of range".to_string(),
        ));
    }
    Ok((adjust_time(time, typmod), zone))
}

/// `EncodeDateTime`. `zone` is the offset east of UTC and the abbreviation, for `timestamptz`.
fn date_time(f: &Fields, zone: Option<(i32, &str)>, format: DateFormat, out: &mut Vec<u8>) {
    let abbrev = |abbrev: &str, out: &mut Vec<u8>| {
        out.push(b' ');
        out.extend_from_slice(&abbrev.as_bytes()[..abbrev.len().min(MAX_TZ_LEN)]);
    };
    match format.style {
        DateStyle::Iso => {
            date_part(f, format, out);
            out.push(b' ');
            time_part(f.hour, f.minute, f.second, f.usec, out);
            if let Some((offset, _)) = zone {
                zone_offset(offset, out);
            }
        }
        DateStyle::Sql | DateStyle::German => {
            date_part(f, format, out);
            out.push(b' ');
            time_part(f.hour, f.minute, f.second, f.usec, out);
            if let Some((_, name)) = zone {
                abbrev(name, out);
            }
        }
        DateStyle::Postgres => {
            let weekday = (date2j(f.year, f.month as i32, f.day as i32) + 1).rem_euclid(7);
            out.extend_from_slice(DAYS[weekday as usize].as_bytes());
            out.push(b' ');
            let month = MONTHS[f.month as usize - 1].as_bytes();
            if format.day_first() {
                two(f.day, out);
                out.push(b' ');
                out.extend_from_slice(month);
            } else {
                out.extend_from_slice(month);
                out.push(b' ');
                two(f.day, out);
            }
            out.push(b' ');
            time_part(f.hour, f.minute, f.second, f.usec, out);
            out.push(b' ');
            year(f.year, out);
            if let Some((_, name)) = zone {
                abbrev(name, out);
            }
        }
    }
    bc(f.year, out);
}

/// The text output of `timestamp`. A value before 4714-11-24 BC has no Julian day and is an error, as in PostgreSQL.
pub fn timestamp_out(ts: i64, format: DateFormat, out: &mut Vec<u8>) -> Result<(), TypeError> {
    match ts {
        TIMESTAMP_NEGATIVE_INFINITY => out.extend_from_slice(b"-infinity"),
        TIMESTAMP_INFINITY => out.extend_from_slice(b"infinity"),
        _ => date_time(
            &Fields::of_timestamp(ts).ok_or_else(|| out_of_range("timestamp"))?,
            None,
            format,
            out,
        ),
    }
    Ok(())
}

/// The text output of `timestamptz` in the session time zone.
pub fn timestamptz_out(
    ts: i64,
    format: DateFormat,
    zone: &(impl TimeZone + ?Sized),
    out: &mut Vec<u8>,
) -> Result<(), TypeError> {
    let infinity: &[u8] = match ts {
        TIMESTAMP_NEGATIVE_INFINITY => b"-infinity",
        TIMESTAMP_INFINITY => b"infinity",
        _ => b"",
    };
    if !infinity.is_empty() {
        out.extend_from_slice(infinity);
        return Ok(());
    }
    // The UTC fields only check the range, as `timestamp2tm` does before it asks the zone.
    let utc = Fields::of_timestamp(ts).ok_or_else(|| out_of_range("timestamp"))?;
    let unix = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
    let (offset, name) = zone.at(unix);
    let local = unix + i64::from(offset);
    let (year, month, day) = j2date((local.div_euclid(86400) + i64::from(UNIX_EPOCH_JDATE)) as i32);
    let (hour, minute, second, _) = split_time(local.rem_euclid(86400) * USECS_PER_SEC);
    let fields = Fields { year, month, day, hour, minute, second, usec: utc.usec };
    date_time(&fields, Some((offset, name)), format, out);
    Ok(())
}

/// `timestamp_recv` and `timestamptz_recv`: the range is the range that the input takes, and the value is rounded to the precision of the typmod.
pub fn timestamp_recv(recv: &mut Recv<'_>, typmod: i32) -> Result<i64, TypeError> {
    let ts = recv.i64()?;
    if ts == TIMESTAMP_NEGATIVE_INFINITY || ts == TIMESTAMP_INFINITY {
        return Ok(ts);
    }
    if Fields::of_timestamp(ts).is_none() || !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&ts) {
        return Err(out_of_range("timestamp"));
    }
    adjust_timestamp(ts, typmod)
}

/// `AdjustTimestampForTypmod`.
fn adjust_timestamp(ts: i64, typmod: i32) -> Result<i64, TypeError> {
    if ts == TIMESTAMP_NEGATIVE_INFINITY
        || ts == TIMESTAMP_INFINITY
        || typmod == -1
        || typmod == MAX_TIME_PRECISION
    {
        return Ok(ts);
    }
    if !(0..=MAX_TIME_PRECISION).contains(&typmod) {
        return Err(TypeError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("timestamp({typmod}) precision must be between 0 and {MAX_TIME_PRECISION}"),
        ));
    }
    Ok(round_usecs(ts, typmod))
}

/// `AddPostgresIntPart` and `AddVerboseIntPart` need to know if a field came before and if the last field was negative.
struct Parts {
    zero: bool,
    before: bool,
}

impl Parts {
    /// `AddPostgresIntPart`: `-1 years` has an `s`, and a positive field after a negative one has a `+`.
    fn postgres(&mut self, value: i64, unit: &str, out: &mut Vec<u8>) {
        if value == 0 {
            return;
        }
        if !self.zero {
            out.push(b' ');
        }
        if self.before && value > 0 {
            out.push(b'+');
        }
        out.extend_from_slice(format!("{value} {unit}").as_bytes());
        if value != 1 {
            out.push(b's');
        }
        self.before = value < 0;
        self.zero = false;
    }

    /// `AddVerboseIntPart`: the first field sets the sign, and the text has `ago` at the end in place of a minus sign.
    fn verbose(&mut self, mut value: i64, unit: &str, out: &mut Vec<u8>) {
        if value == 0 {
            return;
        }
        if self.zero {
            self.before = value < 0;
            value = value.abs();
        } else if self.before {
            value = -value;
        }
        out.extend_from_slice(format!(" {value} {unit}").as_bytes());
        if value != 1 {
            out.push(b's');
        }
        self.zero = false;
    }
}

/// The text output of `interval`.
pub fn interval_out(iv: &Interval, style: IntervalStyle, out: &mut Vec<u8>) {
    if *iv == Interval::NEGATIVE_INFINITY {
        return out.extend_from_slice(b"-infinity");
    }
    if *iv == Interval::INFINITY {
        return out.extend_from_slice(b"infinity");
    }
    // `interval2itm`. Each field has the sign of its source, as C division gives it.
    let mut year = i64::from(iv.month / 12);
    let mut mon = i64::from(iv.month % 12);
    let mut mday = i64::from(iv.day);
    let mut hour = iv.time / 3_600_000_000;
    let mut min = iv.time / 60_000_000 % 60;
    let mut sec = iv.time / USECS_PER_SEC % 60;
    let mut usec = iv.time % USECS_PER_SEC;
    let time_negative = hour < 0 || min < 0 || sec < 0 || usec < 0;
    let time_nonzero = hour != 0 || min != 0 || sec != 0 || usec != 0;
    let mut parts = Parts { zero: true, before: false };
    match style {
        IntervalStyle::SqlStandard => {
            let negative = year < 0 || mon < 0 || mday < 0 || time_negative;
            let positive =
                year > 0 || mon > 0 || mday > 0 || hour > 0 || min > 0 || sec > 0 || usec > 0;
            let year_month = year != 0 || mon != 0;
            let day_time = mday != 0 || time_nonzero;
            let standard = !(negative && positive) && !(year_month && day_time);
            if negative && standard {
                out.push(b'-');
                (year, mon, mday, hour, min, sec, usec) =
                    (-year, -mon, -mday, -hour, -min, -sec, -usec);
            }
            if !negative && !positive {
                out.push(b'0');
            } else if !standard {
                let sign = |negative: bool| if negative { '-' } else { '+' };
                out.extend_from_slice(
                    format!(
                        "{}{}-{} {}{} {}{}:{:02}:",
                        sign(year < 0 || mon < 0),
                        year.abs(),
                        mon.abs(),
                        sign(mday < 0),
                        mday.abs(),
                        sign(time_negative),
                        hour.abs(),
                        min.abs()
                    )
                    .as_bytes(),
                );
                seconds(sec, usec, true, out);
            } else if year_month {
                out.extend_from_slice(format!("{year}-{mon}").as_bytes());
            } else {
                if mday != 0 {
                    out.extend_from_slice(format!("{mday} ").as_bytes());
                }
                out.extend_from_slice(format!("{hour}:{min:02}:").as_bytes());
                seconds(sec, usec, true, out);
            }
        }
        IntervalStyle::Iso8601 => {
            if year == 0 && mon == 0 && mday == 0 && !time_nonzero {
                return out.extend_from_slice(b"PT0S");
            }
            out.push(b'P');
            let part = |value: i64, unit: char, out: &mut Vec<u8>| {
                if value != 0 {
                    out.extend_from_slice(format!("{value}{unit}").as_bytes());
                }
            };
            part(year, 'Y', out);
            part(mon, 'M', out);
            part(mday, 'D', out);
            if time_nonzero {
                out.push(b'T');
            }
            part(hour, 'H', out);
            part(min, 'M', out);
            if sec != 0 || usec != 0 {
                if sec < 0 || usec < 0 {
                    out.push(b'-');
                }
                seconds(sec, usec, false, out);
                out.push(b'S');
            }
        }
        IntervalStyle::Postgres => {
            parts.postgres(year, "year", out);
            parts.postgres(mon, "mon", out);
            parts.postgres(mday, "day", out);
            if parts.zero || time_nonzero {
                if !parts.zero {
                    out.push(b' ');
                }
                if time_negative {
                    out.push(b'-');
                } else if parts.before {
                    out.push(b'+');
                }
                zeropad(hour.unsigned_abs(), 2, out);
                out.push(b':');
                zeropad(min.unsigned_abs(), 2, out);
                out.push(b':');
                seconds(sec, usec, true, out);
            }
        }
        IntervalStyle::PostgresVerbose => {
            out.push(b'@');
            parts.verbose(year, "year", out);
            parts.verbose(mon, "mon", out);
            parts.verbose(mday, "day", out);
            parts.verbose(hour, "hour", out);
            parts.verbose(min, "min", out);
            if sec != 0 || usec != 0 {
                out.push(b' ');
                if sec < 0 || (sec == 0 && usec < 0) {
                    if parts.zero {
                        parts.before = true;
                    } else if !parts.before {
                        out.push(b'-');
                    }
                } else if parts.before {
                    out.push(b'-');
                }
                seconds(sec, usec, false, out);
                out.extend_from_slice(if sec.abs() != 1 || usec != 0 { b" secs" } else { b" sec" });
                parts.zero = false;
            }
            if parts.zero {
                out.extend_from_slice(b" 0");
            }
            if parts.before {
                out.extend_from_slice(b" ago");
            }
        }
    }
}

/// `interval_recv`: the fields after the last field of the typmod are cleared, and the microseconds are rounded to the precision.
pub fn interval_recv(recv: &mut Recv<'_>, typmod: i32) -> Result<Interval, TypeError> {
    let iv = Interval { time: recv.i64()?, day: recv.i32()?, month: recv.i32()? };
    adjust_interval(iv, typmod)
}

/// `AdjustIntervalForTypmod`.
fn adjust_interval(mut iv: Interval, typmod: i32) -> Result<Interval, TypeError> {
    if typmod < 0 || iv == Interval::INFINITY || iv == Interval::NEGATIVE_INFINITY {
        return Ok(iv);
    }
    let (range, precision) = ((typmod >> 16) & 0x7fff, typmod & 0xffff);
    let mask = |fields: &[IntervalField]| crate::typmod::interval_range(fields);
    use IntervalField::*;
    const HOUR_USECS: i64 = 3_600_000_000;
    const MINUTE_USECS: i64 = 60_000_000;
    let hours = |time: i64| time / HOUR_USECS * HOUR_USECS;
    let minutes = |time: i64| time / MINUTE_USECS * MINUTE_USECS;
    match range {
        r if r == INTERVAL_FULL_RANGE
            || r == mask(&[Second])
            || r == mask(&[Day, Hour, Minute, Second])
            || r == mask(&[Hour, Minute, Second])
            || r == mask(&[Minute, Second]) => {}
        r if r == mask(&[Year]) => iv = Interval { time: 0, day: 0, month: iv.month / 12 * 12 },
        r if r == mask(&[Month]) || r == mask(&[Year, Month]) => (iv.day, iv.time) = (0, 0),
        r if r == mask(&[Day]) => iv.time = 0,
        r if r == mask(&[Hour]) || r == mask(&[Day, Hour]) => iv.time = hours(iv.time),
        r if r == mask(&[Minute])
            || r == mask(&[Day, Hour, Minute])
            || r == mask(&[Hour, Minute]) =>
        {
            iv.time = minutes(iv.time)
        }
        _ => {
            return Err(TypeError::new(
                SqlState::INTERNAL_ERROR,
                format!("unrecognized interval typmod: {typmod}"),
            ));
        }
    }
    if precision != INTERVAL_FULL_PRECISION {
        if !(0..=MAX_TIME_PRECISION).contains(&precision) {
            return Err(TypeError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "interval({precision}) precision must be between 0 and {MAX_TIME_PRECISION}"
                ),
            ));
        }
        let scale = 10i64.pow((MAX_TIME_PRECISION - precision) as u32);
        let half = scale / 2;
        let time = if iv.time >= 0 { iv.time.checked_add(half) } else { iv.time.checked_sub(half) };
        let time = time.ok_or_else(|| out_of_range("interval"))?;
        iv.time = time - time % scale;
    }
    Ok(iv)
}

/// The binary output of `interval`.
pub fn interval_send(iv: &Interval, out: &mut Vec<u8>) {
    out.extend_from_slice(&iv.time.to_be_bytes());
    out.extend_from_slice(&iv.day.to_be_bytes());
    out.extend_from_slice(&iv.month.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_julian_day_goes_both_ways() {
        assert_eq!(date2j(2000, 1, 1), POSTGRES_EPOCH_JDATE);
        assert_eq!(date2j(1970, 1, 1), UNIX_EPOCH_JDATE);
        assert_eq!(date2j(5874898, 1, 1), DATE_END_JULIAN);
        assert_eq!(date2j(-4713, 11, 24), 0);
        for jd in [0, 1, 1721425, 1721426, 2451545, 2460954, 109203528, DATE_END_JULIAN - 1] {
            let (y, m, d) = j2date(jd);
            assert_eq!(date2j(y, m as i32, d as i32), jd);
        }
        assert_eq!(UNIX_TO_POSTGRES_USECS, -946_684_800_000_000);
    }

    #[test]
    fn each_style_writes_its_layout() {
        let ts =
            (date2j(2026, 10, 5) - POSTGRES_EPOCH_JDATE) as i64 * USECS_PER_DAY + 45_296_789_000;
        let at =
            |style, order| text(|out| timestamp_out(ts, DateFormat { style, order }, out).unwrap());
        assert_eq!(at(DateStyle::Iso, DateOrder::Mdy), "2026-10-05 12:34:56.789");
        assert_eq!(at(DateStyle::Sql, DateOrder::Mdy), "10/05/2026 12:34:56.789");
        assert_eq!(at(DateStyle::Sql, DateOrder::Dmy), "05/10/2026 12:34:56.789");
        assert_eq!(at(DateStyle::German, DateOrder::Ymd), "05.10.2026 12:34:56.789");
        assert_eq!(at(DateStyle::Postgres, DateOrder::Mdy), "Mon Oct 05 12:34:56.789 2026");
        assert_eq!(at(DateStyle::Postgres, DateOrder::Dmy), "Mon 05 Oct 12:34:56.789 2026");
        let zone = FixedZone { offset: 19800, abbrev: "IST".to_string() };
        let tz = |style| {
            text(|out| {
                timestamptz_out(ts, DateFormat { style, order: DateOrder::Mdy }, &zone, out)
                    .unwrap()
            })
        };
        assert_eq!(tz(DateStyle::Iso), "2026-10-05 18:04:56.789+05:30");
        assert_eq!(tz(DateStyle::Postgres), "Mon Oct 05 18:04:56.789 2026 IST");
        let bc = (date2j(-43, 3, 15) - POSTGRES_EPOCH_JDATE) as i64 * USECS_PER_DAY;
        assert_eq!(
            text(|out| timestamp_out(bc, DateFormat::ISO_MDY, out).unwrap()),
            "0044-03-15 00:00:00 BC"
        );
        assert_eq!(text(|out| timetz_out(USECS_PER_DAY, -20130, out)), "24:00:00+05:35:30");
        assert_eq!(text(|out| date_out(DATE_INFINITY, DateFormat::ISO_MDY, out)), "infinity");
        let error = timestamp_out(i64::MIN + 1, DateFormat::ISO_MDY, &mut Vec::new()).unwrap_err();
        assert_eq!(error.message, "timestamp out of range");
    }

    #[test]
    fn the_receive_functions_check_the_range() {
        let date = |d: i32| date_recv(&mut Recv::new(&d.to_be_bytes()));
        assert_eq!(date(i32::MIN), Ok(i32::MIN));
        assert_eq!(date(-POSTGRES_EPOCH_JDATE), Ok(-POSTGRES_EPOCH_JDATE));
        assert_eq!(date(-POSTGRES_EPOCH_JDATE - 1).unwrap_err().message, "date out of range");
        let time = |t: i64, typmod| time_recv(&mut Recv::new(&t.to_be_bytes()), typmod);
        assert_eq!(time(USECS_PER_DAY, -1), Ok(USECS_PER_DAY));
        assert_eq!(time(USECS_PER_DAY + 1, -1).unwrap_err().message, "time out of range");
        assert_eq!(time(1_500_000, 0), Ok(2_000_000));
        let ts = |t: i64, typmod| timestamp_recv(&mut Recv::new(&t.to_be_bytes()), typmod);
        assert_eq!(ts(-1_500_000, 0), Ok(-2_000_000));
        assert_eq!(ts(END_TIMESTAMP, -1).unwrap_err().message, "timestamp out of range");
        assert_eq!(ts(0, 7).unwrap_err().message, "timestamp(7) precision must be between 0 and 6");
        let zone = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xe1, 0];
        assert_eq!(timetz_recv(&mut Recv::new(&zone), -1).unwrap_err().sqlstate.as_str(), "22009");
    }

    #[test]
    fn an_interval_typmod_clears_the_small_fields() {
        let iv = Interval { time: 90_061_500_000, day: 3, month: 14 };
        let mut bytes = Vec::new();
        interval_send(&iv, &mut bytes);
        let recv = |typmod| interval_recv(&mut Recv::new(&bytes), typmod).unwrap();
        let typmod = |fields: &[IntervalField], precision| {
            crate::typmod::interval_typmod(precision, crate::typmod::interval_range(fields))
        };
        use IntervalField::*;
        assert_eq!(recv(-1), iv);
        assert_eq!(
            recv(typmod(&[Year], INTERVAL_FULL_PRECISION)),
            Interval { time: 0, day: 0, month: 12 }
        );
        assert_eq!(recv(typmod(&[Day, Hour], INTERVAL_FULL_PRECISION)).time, 90_000_000_000);
        assert_eq!(recv(typmod(&[Day, Hour, Minute, Second], 0)).time, 90_062_000_000);
        let error = interval_recv(&mut Recv::new(&bytes), typmod(&[Second], 7)).unwrap_err();
        assert_eq!(error.message, "interval(7) precision must be between 0 and 6");
        let error = interval_recv(&mut Recv::new(&bytes), typmod(&[Year, Day], 0)).unwrap_err();
        assert_eq!(error.sqlstate.as_str(), "XX000");
    }
}

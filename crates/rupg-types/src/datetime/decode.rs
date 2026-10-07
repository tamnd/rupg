//! The text input of `date`, `time`, `timetz`, `timestamp`, `timestamptz` and `interval`.
//!
//! This is a port of `ParseDateTime`, `DecodeDateTime`, `DecodeTimeOnly`, `DecodeInterval` and `DecodeISO8601Interval` in `src/backend/utils/adt/datetime.c`, with the helpers that they call. The structure follows the C code closely, so that a difference against the server can be found by reading the two side by side. The field masks, the token types and the error codes have the values of `datetime.h`.
//!
//! The server reads some inputs from the session: the field order of `DateStyle`, the time zone, the time zone abbreviations, the zone names and the time of the transaction start, for `now` and `today`. [`DateTimeInput`] holds all of them.
//!
//! Lifted from `crates/rudb-pgtypes/src/datetime/decode.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::sync::{Arc, OnceLock};

use rupg_common::SqlState;

use super::{
    AbbrevMeaning, DATE_END_JULIAN, DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateOrder,
    END_TIMESTAMP, Interval, IntervalStyle, MIN_TIMESTAMP, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE, USECS_PER_DAY,
    USECS_PER_SEC, adjust_interval, adjust_time, adjust_timestamp, date2j, j2date, out_of_range,
};
use crate::error::TypeError;
use crate::float::strtod;
use crate::number::is_space;
use crate::typmod::INTERVAL_FULL_RANGE;

/// The zones that a name in the input can give, as `pg_tzset` finds them. The name is in lower case, as the input has it.
pub trait ZoneLookup {
    fn zone(&self, name: &str) -> Option<Arc<dyn TimeZone + Send + Sync>>;
}

/// A [`ZoneLookup`] that knows no zone, until the tz database is here.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoZones;

impl ZoneLookup for NoZones {
    fn zone(&self, _: &str) -> Option<Arc<dyn TimeZone + Send + Sync>> {
        None
    }
}

/// One line of a `timezone_abbreviations` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Abbrev {
    /// A fixed offset east of UTC in seconds, and whether it is daylight saving time.
    Fixed { offset: i32, dst: bool },
    /// The name of a zone, for an abbreviation whose offset changed over time.
    Zone(String),
}

/// The time zone abbreviations of the `timezone_abbreviations` setting, sorted by the lower case abbreviation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneAbbrevs {
    entries: Vec<(String, Abbrev)>,
}

impl ZoneAbbrevs {
    /// Reads a file in the format of `src/timezone/tznames`, as `ParseTzFile` does. The `@INCLUDE` and `@OVERRIDE` lines are not supported.
    pub fn parse(text: &str) -> Result<ZoneAbbrevs, String> {
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let lineno = index + 1;
            let line = line.trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8));
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('@') {
                return Err(format!("line {lineno}: {line} is not supported"));
            }
            let mut words = line.split([' ', '\t', '\n', '\r']).filter(|w| !w.is_empty());
            let abbrev = words.next().unwrap_or_default();
            let Some(offset) = words.next() else {
                return Err(format!("missing time zone offset in line {lineno}"));
            };
            let (abbrev_value, rest) = if offset.starts_with(|c: char| c.is_ascii_digit())
                || offset.starts_with(['+', '-'])
            {
                let Ok(offset) = offset.parse::<i32>() else {
                    return Err(format!("invalid number for time zone offset in line {lineno}"));
                };
                if !(-14 * 3600..=14 * 3600).contains(&offset) {
                    return Err(format!(
                        "time zone offset {offset} is out of range in line {lineno}"
                    ));
                }
                match words.next() {
                    Some(word) if word.eq_ignore_ascii_case("D") => {
                        (Abbrev::Fixed { offset, dst: true }, words.next())
                    }
                    word => (Abbrev::Fixed { offset, dst: false }, word),
                }
            } else {
                (Abbrev::Zone(offset.to_string()), words.next())
            };
            if rest.is_some_and(|word| !word.starts_with('#')) {
                return Err(format!("invalid syntax in line {lineno}"));
            }
            if abbrev.len() > TOKMAXLEN {
                return Err(format!(
                    "time zone abbreviation \"{abbrev}\" is too long (maximum {TOKMAXLEN} characters) in line {lineno}"
                ));
            }
            entries.push((abbrev.to_ascii_lowercase(), abbrev_value));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in entries.windows(2) {
            if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
                return Err(format!(
                    "time zone abbreviation \"{}\" is multiply defined",
                    pair[0].0
                ));
            }
        }
        entries.dedup();
        Ok(ZoneAbbrevs { entries })
    }

    /// The `Default` set of PostgreSQL, the default of `timezone_abbreviations`.
    ///
    /// # Panics
    ///
    /// Only if the vendored file does not parse, which the unit tests check.
    pub fn postgres_default() -> &'static ZoneAbbrevs {
        static DEFAULT: OnceLock<ZoneAbbrevs> = OnceLock::new();
        DEFAULT.get_or_init(|| {
            ZoneAbbrevs::parse(include_str!(
                "../../../../vendor/postgres-19/src/timezone/tznames/Default"
            ))
            .expect("the vendored tznames file is valid")
        })
    }

    /// The abbreviation and its meaning. Only the first 10 bytes of the token count, as in `datebsearch`.
    pub fn get(&self, token: &str) -> Option<(&str, &Abbrev)> {
        let key = truncate(token.as_bytes());
        let found = self.entries.binary_search_by(|(abbrev, _)| abbrev.as_bytes().cmp(key)).ok()?;
        let (abbrev, value) = &self.entries[found];
        Some((abbrev, value))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The session state that the text input of the date and time types reads.
#[derive(Clone, Copy)]
pub struct DateTimeInput<'a> {
    /// The field order part of `DateStyle`.
    pub order: DateOrder,
    /// The `TimeZone` setting.
    pub zone: &'a dyn TimeZone,
    /// The zone names, such as `america/new_york`.
    pub zones: &'a dyn ZoneLookup,
    /// The `timezone_abbreviations` setting.
    pub abbrevs: &'a ZoneAbbrevs,
    /// The start of the transaction in microseconds since 2000-01-01, for `now` and `today`.
    pub now: i64,
}

impl std::fmt::Debug for DateTimeInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DateTimeInput")
            .field("order", &self.order)
            .field("abbrevs", &self.abbrevs.len())
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

// The field kinds of `ParseDateTime`.
const DTK_NUMBER: u8 = 0;
const DTK_STRING: u8 = 1;
const DTK_DATE: u8 = 2;
const DTK_TIME: u8 = 3;
const DTK_TZ: u8 = 4;
const DTK_SPECIAL: u8 = 6;

// The values of the reserved words and the units.
const DTK_EARLY: i32 = 9;
const DTK_LATE: i32 = 10;
const DTK_EPOCH: i32 = 11;
const DTK_NOW: i32 = 12;
const DTK_YESTERDAY: i32 = 13;
const DTK_TODAY: i32 = 14;
const DTK_TOMORROW: i32 = 15;
const DTK_ZULU: i32 = 16;
const DTK_DELTA: i32 = 17;
const DTK_SECOND: i32 = 18;
const DTK_MINUTE: i32 = 19;
const DTK_HOUR: i32 = 20;
const DTK_DAY: i32 = 21;
const DTK_WEEK: i32 = 22;
const DTK_MONTH: i32 = 23;
const DTK_QUARTER: i32 = 24;
const DTK_YEAR: i32 = 25;
const DTK_DECADE: i32 = 26;
const DTK_CENTURY: i32 = 27;
const DTK_MILLENNIUM: i32 = 28;
const DTK_MILLISEC: i32 = 29;
const DTK_MICROSEC: i32 = 30;
const DTK_JULIAN: i32 = 31;
const DTK_DOW: i32 = 32;
const DTK_DOY: i32 = 33;
const DTK_TZ_HOUR: i32 = 34;
const DTK_TZ_MINUTE: i32 = 35;
const DTK_ISOYEAR: i32 = 36;
const DTK_ISODOW: i32 = 37;
/// `DTK_DATE` and `DTK_TIME` as the values of a decoded input and of the `t` token.
const DTK_DATE_VALUE: i32 = 2;
const DTK_TIME_VALUE: i32 = 3;
/// `DTK_TZ` as the value of the `timezone` unit.
const DTK_TZ_VALUE: i32 = 4;

// The token types. Each one is also the bit of the field in a mask.
const RESERV: i32 = 0;
const MONTH: i32 = 1;
const YEAR: i32 = 2;
const DAY: i32 = 3;
const TZ: i32 = 5;
const DTZ: i32 = 6;
const DYNTZ: i32 = 7;
const IGNORE_DTF: i32 = 8;
const AMPM: i32 = 9;
const HOUR: i32 = 10;
const MINUTE: i32 = 11;
const SECOND: i32 = 12;
const MILLISECOND: i32 = 13;
const MICROSECOND: i32 = 14;
const DOY: i32 = 15;
const DOW: i32 = 16;
const UNITS: i32 = 17;
const ADBC: i32 = 18;
const AGO: i32 = 19;
const ISOTIME: i32 = 23;
const WEEK: i32 = 24;
const DECADE: i32 = 25;
const CENTURY: i32 = 26;
const MILLENNIUM: i32 = 27;
const DTZMOD: i32 = 28;
const UNKNOWN_FIELD: i32 = 31;

const fn m(t: i32) -> u32 {
    1 << t
}

const DATE_M: u32 = m(YEAR) | m(MONTH) | m(DAY);
const ALL_SECS_M: u32 = m(SECOND) | m(MILLISECOND) | m(MICROSECOND);
const TIME_M: u32 = m(HOUR) | m(MINUTE) | ALL_SECS_M;

const AM: i32 = 0;
const PM: i32 = 1;
const HR24: i32 = 2;
const BC: i32 = 1;

const MAXDATEFIELDS: usize = 25;
const TOKMAXLEN: usize = 10;
/// The work buffers of the input functions: `MAXDATELEN + 1`, `MAXDATELEN + MAXDATEFIELDS` and 256 for `interval`.
const DATE_BUFLEN: usize = 129;
const TIMESTAMP_BUFLEN: usize = 153;
const INTERVAL_BUFLEN: usize = 256;

const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 3600 * USECS_PER_SEC;
const DAYS_PER_MONTH: i32 = 30;
const MAX_TZDISP_HOUR: i32 = 15;

/// The errors of the decode functions, the `DTERR` codes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DtErr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    IntervalOverflow,
    TzDispOverflow,
    BadTimezone(String),
    BadZoneAbbrev { zone: String, abbrev: String },
}

use DtErr::{BadFormat, FieldOverflow};

impl DtErr {
    /// `DateTimeParseError`.
    fn into_error(self, input: &str, type_name: &str) -> TypeError {
        let overflow = || {
            TypeError::new(
                SqlState::DATETIME_FIELD_OVERFLOW,
                format!("date/time field value out of range: \"{input}\""),
            )
        };
        match self {
            BadFormat => TypeError::new(
                SqlState::INVALID_DATETIME_FORMAT,
                format!("invalid input syntax for type {type_name}: \"{input}\""),
            ),
            FieldOverflow => overflow(),
            DtErr::MdFieldOverflow => TypeError {
                hint: Some("Perhaps you need a different \"DateStyle\" setting.".to_string()),
                ..overflow()
            },
            DtErr::IntervalOverflow => TypeError::new(
                SqlState::INTERVAL_FIELD_OVERFLOW,
                format!("interval field value out of range: \"{input}\""),
            ),
            DtErr::TzDispOverflow => TypeError::new(
                SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                format!("time zone displacement out of range: \"{input}\""),
            ),
            DtErr::BadTimezone(zone) => TypeError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("time zone \"{zone}\" not recognized"),
            ),
            DtErr::BadZoneAbbrev { zone, abbrev } => TypeError {
                detail: Some(format!(
                    "This time zone name appears in the configuration file for time zone abbreviation \"{abbrev}\"."
                )),
                ..TypeError::new(
                    SqlState::CONFIG_FILE_ERROR,
                    format!("time zone \"{zone}\" not recognized"),
                )
            },
        }
    }
}

/// The first `TOKMAXLEN` bytes of a token, which is all that the token tables compare.
fn truncate(key: &[u8]) -> &[u8] {
    &key[..key.len().min(TOKMAXLEN)]
}

/// A row of `datetktbl`.
fn date_token(key: &[u8]) -> Option<(i32, i32)> {
    Some(match truncate(key) {
        b"+infinity" | b"infinity" => (RESERV, DTK_LATE),
        b"-infinity" => (RESERV, DTK_EARLY),
        b"ad" => (ADBC, 0),
        b"allballs" => (RESERV, DTK_ZULU),
        b"am" => (AMPM, AM),
        b"apr" | b"april" => (MONTH, 4),
        b"at" | b"on" => (IGNORE_DTF, 0),
        b"aug" | b"august" => (MONTH, 8),
        b"bc" => (ADBC, BC),
        b"d" => (UNITS, DTK_DAY),
        b"dec" | b"december" => (MONTH, 12),
        b"dow" => (UNITS, DTK_DOW),
        b"doy" => (UNITS, DTK_DOY),
        b"dst" => (DTZMOD, 3600),
        b"epoch" => (RESERV, DTK_EPOCH),
        b"feb" | b"february" => (MONTH, 2),
        b"fri" | b"friday" => (DOW, 5),
        b"h" => (UNITS, DTK_HOUR),
        b"isodow" => (UNITS, DTK_ISODOW),
        b"isoyear" => (UNITS, DTK_ISOYEAR),
        b"j" | b"jd" | b"julian" => (UNITS, DTK_JULIAN),
        b"jan" | b"january" => (MONTH, 1),
        b"jul" | b"july" => (MONTH, 7),
        b"jun" | b"june" => (MONTH, 6),
        b"m" => (UNITS, DTK_MONTH),
        b"mar" | b"march" => (MONTH, 3),
        b"may" => (MONTH, 5),
        b"mm" => (UNITS, DTK_MINUTE),
        b"mon" | b"monday" => (DOW, 1),
        b"nov" | b"november" => (MONTH, 11),
        b"now" => (RESERV, DTK_NOW),
        b"oct" | b"october" => (MONTH, 10),
        b"pm" => (AMPM, PM),
        b"s" => (UNITS, DTK_SECOND),
        b"sat" | b"saturday" => (DOW, 6),
        b"sep" | b"sept" | b"september" => (MONTH, 9),
        b"sun" | b"sunday" => (DOW, 0),
        b"t" => (ISOTIME, DTK_TIME_VALUE),
        b"thu" | b"thur" | b"thurs" | b"thursday" => (DOW, 4),
        b"today" => (RESERV, DTK_TODAY),
        b"tomorrow" => (RESERV, DTK_TOMORROW),
        b"tue" | b"tues" | b"tuesday" => (DOW, 2),
        b"wed" | b"wednesday" | b"weds" => (DOW, 3),
        b"y" => (UNITS, DTK_YEAR),
        b"yesterday" => (RESERV, DTK_YESTERDAY),
        _ => return None,
    })
}

/// A row of `deltatktbl`.
fn delta_token(key: &[u8]) -> Option<(i32, i32)> {
    let unit = match truncate(key) {
        b"@" => return Some((IGNORE_DTF, 0)),
        b"ago" => return Some((AGO, 0)),
        b"c" | b"cent" | b"centuries" | b"century" => DTK_CENTURY,
        b"d" | b"day" | b"days" => DTK_DAY,
        b"dec" | b"decade" | b"decades" | b"decs" => DTK_DECADE,
        b"h" | b"hour" | b"hours" | b"hr" | b"hrs" => DTK_HOUR,
        b"m" | b"min" | b"mins" | b"minute" | b"minutes" => DTK_MINUTE,
        b"microsecon" | b"us" | b"usec" | b"usecond" | b"useconds" | b"usecs" => DTK_MICROSEC,
        b"mil" | b"millennia" | b"millennium" | b"mils" => DTK_MILLENNIUM,
        b"millisecon" | b"ms" | b"msec" | b"msecond" | b"mseconds" | b"msecs" => DTK_MILLISEC,
        b"mon" | b"mons" | b"month" | b"months" => DTK_MONTH,
        b"qtr" | b"quarter" => DTK_QUARTER,
        b"s" | b"sec" | b"second" | b"seconds" | b"secs" => DTK_SECOND,
        b"timezone" => DTK_TZ_VALUE,
        b"timezone_h" => DTK_TZ_HOUR,
        b"timezone_m" => DTK_TZ_MINUTE,
        b"w" | b"week" | b"weeks" => DTK_WEEK,
        b"y" | b"year" | b"years" | b"yr" | b"yrs" => DTK_YEAR,
        _ => return None,
    };
    Some((UNITS, unit))
}

/// The byte at `i`, or 0 at the end, as a C string reads.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// A field as text. The fields hold only ASCII, so this does not fail.
fn text(field: &[u8]) -> &str {
    std::str::from_utf8(field).unwrap_or_default()
}

/// The input up to the first NUL, which ends a C string.
fn c_string(input: &str) -> &[u8] {
    let bytes = input.as_bytes();
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}

/// `strtol` in base 10 from `start`: the value, the end, and whether the value saturated. With no digits the end is `start`.
fn strtol(s: &[u8], start: usize) -> (i64, usize, bool) {
    let mut i = start;
    while is_space(at(s, i)) {
        i += 1;
    }
    let negative = at(s, i) == b'-';
    if matches!(at(s, i), b'-' | b'+') {
        i += 1;
    }
    let digits = i;
    let (mut value, mut range) = (0i64, false);
    while at(s, i).is_ascii_digit() {
        let digit = i64::from(at(s, i) - b'0');
        if !range {
            let next = value
                .checked_mul(10)
                .and_then(|v| if negative { v.checked_sub(digit) } else { v.checked_add(digit) });
            match next {
                Some(next) => value = next,
                None => {
                    range = true;
                    value = if negative { i64::MIN } else { i64::MAX };
                }
            }
        }
        i += 1;
    }
    if i == digits {
        return (0, start, false);
    }
    (value, i, range)
}

/// `strtoint`: `strtol` with `ERANGE` when the value does not fit in an `int`.
fn strtoint(s: &[u8], start: usize) -> (i32, usize, bool) {
    let (value, end, range) = strtol(s, start);
    (value as i32, end, range || i64::from(value as i32) != value)
}

/// `atoi`, which is `strtol` cut to an `int`.
fn atoi(s: &[u8]) -> i32 {
    strtol(s, 0).0 as i32
}

/// `ParseFraction`: the field from the decimal point to the end, as a number from 0 to 1.
fn parse_fraction(s: &[u8]) -> Result<f64, DtErr> {
    if s.len() == 1 {
        return Ok(0.0);
    }
    if !s[1..].iter().all(u8::is_ascii_digit) {
        return Err(BadFormat);
    }
    match strtod::<f64>(s) {
        Some((value, end, false)) if end == s.len() => Ok(value),
        _ => Err(BadFormat),
    }
}

/// `ParseFractionalSecond`: the fraction in microseconds.
fn parse_fractional_second(s: &[u8]) -> Result<i32, DtErr> {
    Ok((parse_fraction(s)? * 1e6).round_ties_even() as i32)
}

/// The fields of `ParseDateTime` in one buffer, with the bounds and the kind of each field.
struct Parsed {
    buf: [u8; INTERVAL_BUFLEN],
    bounds: [(u16, u16); MAXDATEFIELDS],
    kinds: [u8; MAXDATEFIELDS],
    n: usize,
}

impl Parsed {
    fn field(&self, i: usize) -> &[u8] {
        let (start, end) = self.bounds[i];
        &self.buf[start as usize..end as usize]
    }
}

/// `ParseDateTime`: splits the input into fields, in lower case, and gives each one a kind. `buflen` is the size of the work buffer of the caller, which limits the length of the input.
fn parse_date_time(s: &[u8], buflen: usize) -> Result<Parsed, DtErr> {
    let mut out = Parsed {
        buf: [0; INTERVAL_BUFLEN],
        bounds: [(0, 0); MAXDATEFIELDS],
        kinds: [0; MAXDATEFIELDS],
        n: 0,
    };
    let mut pos = 0;
    let mut cp = 0;
    macro_rules! push {
        ($c:expr) => {{
            if pos + 1 >= buflen {
                return Err(BadFormat);
            }
            out.buf[pos] = $c;
            pos += 1;
        }};
    }
    while cp < s.len() {
        let c = s[cp];
        if is_space(c) {
            cp += 1;
            continue;
        }
        if out.n >= MAXDATEFIELDS {
            return Err(BadFormat);
        }
        let start = pos;
        let kind;
        if c.is_ascii_digit() {
            push!(c);
            cp += 1;
            while at(s, cp).is_ascii_digit() {
                push!(s[cp]);
                cp += 1;
            }
            if at(s, cp) == b':' {
                kind = DTK_TIME;
                push!(b':');
                cp += 1;
                while matches!(at(s, cp), b'0'..=b'9' | b':' | b'.') {
                    push!(s[cp]);
                    cp += 1;
                }
            } else if matches!(at(s, cp), b'-' | b'/' | b'.') {
                let delim = s[cp];
                push!(delim);
                cp += 1;
                if at(s, cp).is_ascii_digit() {
                    let mut k = if delim == b'.' { DTK_NUMBER } else { DTK_DATE };
                    while at(s, cp).is_ascii_digit() {
                        push!(s[cp]);
                        cp += 1;
                    }
                    if at(s, cp) == delim {
                        k = DTK_DATE;
                        push!(delim);
                        cp += 1;
                        while at(s, cp).is_ascii_digit() || at(s, cp) == delim {
                            push!(s[cp]);
                            cp += 1;
                        }
                    }
                    kind = k;
                } else {
                    kind = DTK_DATE;
                    while at(s, cp).is_ascii_alphanumeric() || at(s, cp) == delim {
                        push!(s[cp].to_ascii_lowercase());
                        cp += 1;
                    }
                }
            } else {
                kind = DTK_NUMBER;
            }
        } else if c == b'.' {
            push!(c);
            cp += 1;
            while at(s, cp).is_ascii_digit() {
                push!(s[cp]);
                cp += 1;
            }
            kind = DTK_NUMBER;
        } else if c.is_ascii_alphabetic() {
            push!(c.to_ascii_lowercase());
            cp += 1;
            while at(s, cp).is_ascii_alphabetic() {
                push!(s[cp].to_ascii_lowercase());
                cp += 1;
            }
            // A date such as `jan-01-2001`, or a zone name such as `america/new_york` or `etc/gmt+8`, unless the word is a keyword before a digit or a sign.
            let next = at(s, cp);
            let is_date = matches!(next, b'-' | b'/' | b'.')
                || ((next == b'+' || next.is_ascii_digit())
                    && date_token(&out.buf[start..pos]).is_none());
            if is_date {
                kind = DTK_DATE;
                loop {
                    push!(s[cp].to_ascii_lowercase());
                    cp += 1;
                    let next = at(s, cp);
                    if !(matches!(next, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || next.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            } else {
                kind = DTK_STRING;
            }
        } else if c == b'+' || c == b'-' {
            push!(c);
            cp += 1;
            while is_space(at(s, cp)) {
                cp += 1;
            }
            if at(s, cp).is_ascii_digit() {
                kind = DTK_TZ;
                push!(s[cp]);
                cp += 1;
                while matches!(at(s, cp), b'0'..=b'9' | b':' | b'.' | b'-') {
                    push!(s[cp]);
                    cp += 1;
                }
            } else if at(s, cp).is_ascii_alphabetic() {
                kind = DTK_SPECIAL;
                push!(s[cp].to_ascii_lowercase());
                cp += 1;
                while at(s, cp).is_ascii_alphabetic() {
                    push!(s[cp].to_ascii_lowercase());
                    cp += 1;
                }
            } else {
                return Err(BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            cp += 1;
            continue;
        } else {
            return Err(BadFormat);
        }
        out.bounds[out.n] = (start as u16, pos as u16);
        out.kinds[out.n] = kind;
        out.n += 1;
        // The NUL after the field, which the C code writes without a check.
        pos += 1;
    }
    Ok(out)
}

/// `struct pg_tm`, with only the fields that the input uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    yday: i32,
}

/// The result of `DecodeDateTime`: the kind of value, the fields, the microseconds and the zone in seconds west of UTC.
struct Decoded {
    dtype: i32,
    tm: Tm,
    fsec: i32,
    tz: i32,
}

/// A zone that the input named: the session zone or a zone from [`ZoneLookup`].
enum ZoneRef {
    Session,
    Other(Arc<dyn TimeZone + Send + Sync>),
}

impl ZoneRef {
    fn get<'z>(&'z self, session: &'z dyn TimeZone) -> &'z dyn TimeZone {
        match self {
            ZoneRef::Session => session,
            ZoneRef::Other(zone) => &**zone,
        }
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: i32) -> i32 {
    const DAYS: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    DAYS[month as usize - 1] + i32::from(month == 2 && is_leap(year))
}

/// `IS_VALID_JULIAN`: the date is in the range of the Julian day functions, to the month.
fn is_valid_julian(year: i32, month: i32) -> bool {
    (year > -4713 || (year == -4713 && month >= 11))
        && (year < 5874898 || (year == 5874898 && month < 6))
}

/// `j2date` into the fields.
fn set_date(tm: &mut Tm, julian: i32) {
    let (year, month, day) = j2date(julian);
    (tm.year, tm.mon, tm.mday) = (year, month as i32, day as i32);
}

/// `dt2time` for a time of day in microseconds.
fn set_time(tm: &mut Tm, fsec: &mut i32, time: i64) {
    tm.hour = (time / USECS_PER_HOUR) as i32;
    let time = time - i64::from(tm.hour) * USECS_PER_HOUR;
    tm.min = (time / USECS_PER_MINUTE) as i32;
    let time = time - i64::from(tm.min) * USECS_PER_MINUTE;
    tm.sec = (time / USECS_PER_SEC) as i32;
    *fsec = (time - i64::from(tm.sec) * USECS_PER_SEC) as i32;
}

/// `time_overflows`: the time is not in 00:00:00 to 24:00:00.
fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=24).contains(&hour)
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return true;
    }
    ((i64::from(hour) * 60 + i64::from(min)) * 60 + i64::from(sec)) * USECS_PER_SEC
        + i64::from(fsec)
        > USECS_PER_DAY
}

/// `GetCurrentTimeUsec`: the local fields of the transaction start in the session zone, the microseconds, and the zone in seconds west of UTC.
fn current_time(cx: &DateTimeInput<'_>) -> (Tm, i32, i32) {
    let unix = cx.now.div_euclid(USECS_PER_SEC) + 946_684_800;
    let fsec = cx.now.rem_euclid(USECS_PER_SEC) as i32;
    let offset = cx.zone.at(unix).0;
    let local = unix + i64::from(offset);
    let mut tm = Tm::default();
    set_date(&mut tm, (local.div_euclid(86400) + i64::from(UNIX_EPOCH_JDATE)) as i32);
    let seconds = local.rem_euclid(86400) as i32;
    (tm.hour, tm.min, tm.sec) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    (tm, fsec, -offset)
}

/// `DetermineTimeZoneOffsetInternal`: the offset west of UTC of a local time in a zone, and the instant in seconds since 1970-01-01 UTC.
fn determine_offset(tm: &Tm, zone: &dyn TimeZone) -> (i32, i64) {
    if !is_valid_julian(tm.year, tm.mon) {
        return (0, 0);
    }
    let day = i64::from(date2j(tm.year, tm.mon, tm.mday) - UNIX_EPOCH_JDATE) * 86400;
    let mytime = day + i64::from(tm.sec) + (i64::from(tm.min) + i64::from(tm.hour) * 60) * 60;
    let (before, change) = zone.next_change(mytime - 86400);
    let before_time = mytime - i64::from(before);
    let Some((boundary, after)) = change else {
        return (-before, before_time);
    };
    let after_time = mytime - i64::from(after);
    if before_time < boundary && after_time < boundary {
        return (-before, before_time);
    }
    if before_time > boundary && after_time >= boundary {
        return (-after, after_time);
    }
    if before_time > after_time { (-before, before_time) } else { (-after, after_time) }
}

/// `DetermineTimeZoneAbbrevOffset`: the offset west of UTC of a local time with an abbreviation whose meaning changed over time.
fn determine_abbrev_offset(tm: &Tm, abbrev: &[u8], zone: &dyn TimeZone) -> i32 {
    let (offset, instant) = determine_offset(tm, zone);
    match zone.abbrev_at(&text(abbrev).to_ascii_uppercase(), instant) {
        Some((east, _)) => -east,
        None => offset,
    }
}

/// `DecodeTimezoneAbbrev`: the token type, the offset east of UTC, and the zone of a dynamic abbreviation.
fn decode_timezone_abbrev(
    field: &[u8],
    cx: &DateTimeInput<'_>,
) -> Result<(i32, i32, Option<ZoneRef>), DtErr> {
    let token = text(field);
    match cx.zone.abbrev_meaning(&token.to_ascii_uppercase()) {
        Some(AbbrevMeaning::Fixed { offset, dst }) => {
            return Ok((if dst { DTZ } else { TZ }, offset, None));
        }
        Some(AbbrevMeaning::Varies) => return Ok((DYNTZ, 0, Some(ZoneRef::Session))),
        None => {}
    }
    match cx.abbrevs.get(token) {
        None => Ok((UNKNOWN_FIELD, 0, None)),
        Some((_, Abbrev::Fixed { offset, dst })) => {
            Ok((if *dst { DTZ } else { TZ }, *offset, None))
        }
        Some((abbrev, Abbrev::Zone(name))) => match cx.zones.zone(name) {
            Some(zone) => Ok((DYNTZ, 0, Some(ZoneRef::Other(zone)))),
            None => Err(DtErr::BadZoneAbbrev { zone: name.clone(), abbrev: abbrev.to_string() }),
        },
    }
}

/// `DecodeTimezone`: a numeric zone such as `+05`, `-0530` or `+05:30:15`, in seconds west of UTC.
fn decode_timezone(s: &[u8]) -> Result<i32, DtErr> {
    let sign = at(s, 0);
    if sign != b'+' && sign != b'-' {
        return Err(BadFormat);
    }
    let (mut hr, mut end, range) = strtoint(s, 1);
    if range {
        return Err(DtErr::TzDispOverflow);
    }
    let (mut min, mut sec) = (0, 0);
    if at(s, end) == b':' {
        let range;
        (min, end, range) = strtoint(s, end + 1);
        if range {
            return Err(DtErr::TzDispOverflow);
        }
        if at(s, end) == b':' {
            let range;
            (sec, end, range) = strtoint(s, end + 1);
            if range {
                return Err(DtErr::TzDispOverflow);
            }
        }
    } else if end == s.len() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    }
    if !(0..=MAX_TZDISP_HOUR).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DtErr::TzDispOverflow);
    }
    if end != s.len() {
        return Err(BadFormat);
    }
    let tz = (hr * 60 + min) * 60 + sec;
    Ok(if sign == b'-' { tz } else { -tz })
}

/// `DecodeTimeCommon`: `hh:mm`, `hh:mm:ss`, `mm:ss.fff` and so on. Gives the hours, the minutes, the seconds and the microseconds.
fn decode_time_common(s: &[u8], range: i32) -> Result<(i64, i32, i32, i32), DtErr> {
    let (mut hour, end, overflow) = strtol(s, 0);
    if overflow {
        return Err(FieldOverflow);
    }
    if at(s, end) != b':' {
        return Err(BadFormat);
    }
    let (mut min, end, overflow) = strtoint(s, end + 1);
    if overflow {
        return Err(FieldOverflow);
    }
    let mut sec = 0;
    let mut fsec = 0;
    let minutes_seconds = (1 << MINUTE) | (1 << SECOND);
    if end == s.len() {
        if range == minutes_seconds {
            if i32::try_from(hour).is_err() {
                return Err(FieldOverflow);
            }
            (sec, min, hour) = (min, hour as i32, 0);
        }
    } else if at(s, end) == b'.' {
        fsec = parse_fractional_second(&s[end..])?;
        if i32::try_from(hour).is_err() {
            return Err(FieldOverflow);
        }
        (sec, min, hour) = (min, hour as i32, 0);
    } else if at(s, end) == b':' {
        let (value, end, overflow) = strtoint(s, end + 1);
        if overflow {
            return Err(FieldOverflow);
        }
        sec = value;
        if at(s, end) == b'.' {
            fsec = parse_fractional_second(&s[end..])?;
        } else if end != s.len() {
            return Err(BadFormat);
        }
    } else {
        return Err(BadFormat);
    }
    if hour < 0
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return Err(FieldOverflow);
    }
    Ok((hour, min, sec, fsec))
}

/// `DecodeTime`: a time field of a date and time. Gives the mask of the fields.
fn decode_time(s: &[u8], tm: &mut Tm, fsec: &mut i32) -> Result<u32, DtErr> {
    let (hour, min, sec, usec) = decode_time_common(s, INTERVAL_FULL_RANGE)?;
    let hour = i32::try_from(hour).map_err(|_| FieldOverflow)?;
    (tm.hour, tm.min, tm.sec, *fsec) = (hour, min, sec, usec);
    Ok(TIME_M)
}

/// `DecodeNumberField`: a run of digits that holds a date (`yyyymmdd`, `yymmdd`) or a time (`hhmmss`, `hhmm`), with an optional fraction. Gives the mask of the fields.
fn decode_number_field(
    s: &[u8],
    fmask: u32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> Result<u32, DtErr> {
    if !s.iter().all(|&c| c.is_ascii_digit() || c == b'.') {
        return Err(BadFormat);
    }
    let mut s = s;
    if let Some(dot) = s.iter().position(|&c| c == b'.') {
        *fsec = parse_fractional_second(&s[dot..])?;
        s = &s[..dot];
    } else if fmask & DATE_M != DATE_M && s.len() >= 6 {
        let len = s.len();
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok(DATE_M);
    }
    if fmask & TIME_M != TIME_M {
        if s.len() == 6 {
            tm.sec = atoi(&s[4..]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(TIME_M);
        }
        if s.len() == 4 {
            tm.sec = 0;
            tm.min = atoi(&s[2..]);
            tm.hour = atoi(&s[..2]);
            return Ok(TIME_M);
        }
    }
    Err(BadFormat)
}

/// `DecodeNumber`: one number of a date, read in the field order of `DateStyle`. Gives the mask of the fields.
fn decode_number(
    s: &[u8],
    have_text_month: bool,
    fmask: u32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
    order: DateOrder,
) -> Result<u32, DtErr> {
    let flen = s.len();
    let (val, end, overflow) = strtoint(s, 0);
    if overflow {
        return Err(FieldOverflow);
    }
    if end == 0 {
        return Err(BadFormat);
    }
    if at(s, end) == b'.' {
        // More than two digits before the point is a date or a time run together.
        if end > 2 {
            return decode_number_field(s, fmask | DATE_M, tm, fsec, is2digits);
        }
        *fsec = parse_fractional_second(&s[end..])?;
    } else if end != flen {
        return Err(BadFormat);
    }
    // A day of the year after a year.
    if flen == 3 && fmask & DATE_M == m(YEAR) && (1..=366).contains(&val) {
        tm.yday = val;
        return Ok(m(DOY) | m(MONTH) | m(DAY));
    }
    let tmask = match fmask & DATE_M {
        0 => {
            if flen >= 3 || order == DateOrder::Ymd {
                tm.year = val;
                m(YEAR)
            } else if order == DateOrder::Dmy {
                tm.mday = val;
                m(DAY)
            } else {
                tm.mon = val;
                m(MONTH)
            }
        }
        mask if mask == m(YEAR) => {
            tm.mon = val;
            m(MONTH)
        }
        mask if mask == m(MONTH) => {
            if have_text_month && (flen >= 3 || order == DateOrder::Ymd) {
                tm.year = val;
                m(YEAR)
            } else {
                tm.mday = val;
                m(DAY)
            }
        }
        mask if mask == m(YEAR) | m(MONTH) => {
            if have_text_month && flen >= 3 && *is2digits {
                tm.mday = tm.year;
                tm.year = val;
                *is2digits = false;
            } else {
                tm.mday = val;
            }
            m(DAY)
        }
        mask if mask == m(DAY) => {
            tm.mon = val;
            m(MONTH)
        }
        mask if mask == m(MONTH) | m(DAY) => {
            tm.year = val;
            m(YEAR)
        }
        DATE_M => return decode_number_field(s, fmask, tm, fsec, is2digits),
        _ => return Err(BadFormat),
    };
    if tmask == m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(tmask)
}

/// `DecodeDate`: a date with separators, such as `2001-02-03` or `feb-03-2001`. Gives the mask of the fields.
fn decode_date(
    s: &[u8],
    mut fmask: u32,
    is2digits: &mut bool,
    tm: &mut Tm,
    order: DateOrder,
) -> Result<u32, DtErr> {
    let mut fields = [(0usize, 0usize); MAXDATEFIELDS];
    let mut nf = 0;
    let mut i = 0;
    while i < s.len() && nf < MAXDATEFIELDS {
        while i < s.len() && !s[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i == s.len() {
            return Err(BadFormat);
        }
        let start = i;
        if s[i].is_ascii_digit() {
            while at(s, i).is_ascii_digit() {
                i += 1;
            }
        } else {
            while at(s, i).is_ascii_alphabetic() {
                i += 1;
            }
        }
        fields[nf] = (start, i);
        nf += 1;
        // The C code ends the field with a NUL over the next byte, whatever it is.
        if i < s.len() {
            i += 1;
        }
    }
    let mut tmask = 0;
    let mut have_text_month = false;
    let mut numeric = [false; MAXDATEFIELDS];
    for (k, &(start, end)) in fields[..nf].iter().enumerate() {
        let field = &s[start..end];
        if !field[0].is_ascii_alphabetic() {
            numeric[k] = true;
            continue;
        }
        let (ty, val) = date_token(field).unwrap_or((UNKNOWN_FIELD, 0));
        if ty == IGNORE_DTF {
            continue;
        }
        if ty != MONTH {
            return Err(BadFormat);
        }
        tm.mon = val;
        have_text_month = true;
        if fmask & m(MONTH) != 0 {
            return Err(BadFormat);
        }
        fmask |= m(MONTH);
        tmask |= m(MONTH);
    }
    let mut fsec = 0;
    for (k, &(start, end)) in fields[..nf].iter().enumerate() {
        if !numeric[k] {
            continue;
        }
        let dmask =
            decode_number(&s[start..end], have_text_month, fmask, tm, &mut fsec, is2digits, order)?;
        if fmask & dmask != 0 {
            return Err(BadFormat);
        }
        fmask |= dmask;
        tmask |= dmask;
    }
    if fmask & !(m(DOY) | m(TZ)) != DATE_M {
        return Err(BadFormat);
    }
    Ok(tmask)
}

/// `ValidateDate`: the checks and the changes of the year, the month and the day after all the fields are read.
fn validate_date(
    fmask: u32,
    isjulian: bool,
    is2digits: bool,
    bc: bool,
    tm: &mut Tm,
) -> Result<(), DtErr> {
    if fmask & m(YEAR) != 0 {
        if isjulian {
            // The year came from a Julian day.
        } else if bc {
            if tm.year <= 0 {
                return Err(FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return Err(FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(FieldOverflow);
        }
    }
    if fmask & m(DOY) != 0 {
        set_date(tm, date2j(tm.year, 1, 1).wrapping_add(tm.yday).wrapping_sub(1));
    }
    if fmask & m(MONTH) != 0 && !(1..=12).contains(&tm.mon) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & m(DAY) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & DATE_M == DATE_M && tm.mday > days_in_month(tm.year, tm.mon) {
        return Err(FieldOverflow);
    }
    Ok(())
}

/// The `AM` and `PM` change of the hour, the same in `DecodeDateTime` and `DecodeTimeOnly`.
fn apply_meridian(mer: i32, tm: &mut Tm) -> Result<(), DtErr> {
    if mer != HR24 && tm.hour > 12 {
        return Err(FieldOverflow);
    }
    if mer == AM && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == PM && tm.hour != 12 {
        tm.hour += 12;
    }
    Ok(())
}

/// `DecodeDateTime`: the fields of a date or a timestamp.
fn decode_date_time(p: &Parsed, cx: &DateTimeInput<'_>) -> Result<Decoded, DtErr> {
    let mut fmask = 0u32;
    let mut ptype = 0;
    let mut dtype = DTK_DATE_VALUE;
    let mut tm = Tm::default();
    let mut fsec = 0;
    let mut tz = 0;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named: Option<Arc<dyn TimeZone + Send + Sync>> = None;
    let mut dynamic: Option<(ZoneRef, &[u8])> = None;

    for i in 0..p.n {
        let field = p.field(i);
        let tmask = match p.kinds[i] {
            DTK_DATE => {
                if ptype == DTK_JULIAN {
                    // A Julian day with a zone, such as `j2451187-08`.
                    let (jday, end, overflow) = strtoint(field, 0);
                    if overflow || jday < 0 {
                        return Err(FieldOverflow);
                    }
                    set_date(&mut tm, jday);
                    isjulian = true;
                    tz = decode_timezone(&field[end..])?;
                    ptype = 0;
                    DATE_M | TIME_M | m(TZ)
                } else if ptype != 0 || fmask & (m(MONTH) | m(DAY)) == m(MONTH) | m(DAY) {
                    // A zone name, or a time run together with a zone such as `040506-08`.
                    if field[0].is_ascii_digit() || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME_VALUE {
                                return Err(BadFormat);
                            }
                            ptype = 0;
                        }
                        if fmask & TIME_M == TIME_M {
                            return Err(BadFormat);
                        }
                        let Some(dash) = field.iter().position(|&c| c == b'-') else {
                            return Err(BadFormat);
                        };
                        tz = decode_timezone(&field[dash..])?;
                        let tmask = decode_number_field(
                            &field[..dash],
                            fmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        tmask | m(TZ)
                    } else {
                        let Some(zone) = cx.zones.zone(text(field)) else {
                            return Err(DtErr::BadTimezone(text(field).to_string()));
                        };
                        named = Some(zone);
                        m(TZ)
                    }
                } else {
                    decode_date(field, fmask, &mut is2digits, &mut tm, cx.order)?
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME_VALUE {
                        return Err(BadFormat);
                    }
                    ptype = 0;
                }
                let tmask = decode_time(field, &mut tm, &mut fsec)?;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return Err(FieldOverflow);
                }
                tmask
            }
            DTK_TZ => {
                tz = decode_timezone(field)?;
                m(TZ)
            }
            DTK_NUMBER if ptype != 0 => {
                // The field after a `j` or a `t`.
                let (value, end, overflow) = strtoint(field, 0);
                if overflow {
                    return Err(FieldOverflow);
                }
                if at(field, end) != b'.' && end != field.len() {
                    return Err(BadFormat);
                }
                let tmask = match ptype {
                    DTK_JULIAN => {
                        if value < 0 {
                            return Err(FieldOverflow);
                        }
                        set_date(&mut tm, value);
                        isjulian = true;
                        if at(field, end) == b'.' {
                            let time = parse_fraction(&field[end..])? * USECS_PER_DAY as f64;
                            set_time(&mut tm, &mut fsec, time as i64);
                            DATE_M | TIME_M
                        } else {
                            DATE_M
                        }
                    }
                    DTK_TIME_VALUE => {
                        let tmask = decode_number_field(
                            field,
                            fmask | DATE_M,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        if tmask != TIME_M {
                            return Err(BadFormat);
                        }
                        tmask
                    }
                    _ => return Err(BadFormat),
                };
                ptype = 0;
                dtype = DTK_DATE_VALUE;
                tmask
            }
            DTK_NUMBER => {
                let flen = field.len();
                let dot = field.iter().position(|&c| c == b'.');
                if dot.is_some() && fmask & DATE_M == 0 {
                    decode_date(field, fmask, &mut is2digits, &mut tm, cx.order)?
                } else if dot.is_some_and(|dot| dot > 2)
                    || (flen >= 6 && (fmask & DATE_M == 0 || fmask & TIME_M == 0))
                {
                    decode_number_field(field, fmask, &mut tm, &mut fsec, &mut is2digits)?
                } else {
                    decode_number(
                        field,
                        have_text_month,
                        fmask,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                        cx.order,
                    )?
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (mut ty, mut val, valtz) = decode_timezone_abbrev(field, cx)?;
                if ty == UNKNOWN_FIELD {
                    (ty, val) = date_token(field).unwrap_or((UNKNOWN_FIELD, 0));
                }
                if ty == IGNORE_DTF {
                    continue;
                }
                let mut tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DATE_M | TIME_M | m(TZ);
                            dtype = DTK_DATE_VALUE;
                            (tm, fsec, tz) = current_time(cx);
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DATE_M;
                            dtype = DTK_DATE_VALUE;
                            let (now, _, _) = current_time(cx);
                            let shift = match val {
                                DTK_YESTERDAY => -1,
                                DTK_TOMORROW => 1,
                                _ => 0,
                            };
                            set_date(&mut tm, date2j(now.year, now.mon, now.mday) + shift);
                        }
                        DTK_ZULU => {
                            tmask = TIME_M | m(TZ);
                            dtype = DTK_DATE_VALUE;
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                            tz = 0;
                        }
                        _ => {
                            tmask = DATE_M | TIME_M | m(TZ);
                            dtype = val;
                        }
                    },
                    MONTH => {
                        // A number before a month name was the day, as in `1 jan 2001`.
                        if fmask & m(MONTH) != 0
                            && !have_text_month
                            && fmask & m(DAY) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = m(DAY);
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    DTZMOD => {
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                    }
                    TZ => tz = -val,
                    DYNTZ => {
                        tmask |= m(TZ);
                        dynamic = valtz.map(|zone| (zone, field));
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => {}
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        tmask = 0;
                        if fmask & DATE_M != DATE_M || ptype != 0 {
                            return Err(BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        named = Some(cx.zones.zone(text(field)).ok_or(BadFormat)?);
                        tmask = m(TZ);
                    }
                    _ => return Err(BadFormat),
                }
                tmask
            }
            _ => return Err(BadFormat),
        };
        if tmask & fmask != 0 {
            return Err(BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(BadFormat);
    }
    if dtype == DTK_DATE_VALUE {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        apply_meridian(mer, &mut tm)?;
        // A time with no date is an error for every caller.
        if fmask & DATE_M != DATE_M {
            return Err(BadFormat);
        }
        if let Some(zone) = &named {
            if fmask & m(DTZMOD) != 0 {
                return Err(BadFormat);
            }
            tz = determine_offset(&tm, &**zone).0;
        }
        if let Some((zone, abbrev)) = &dynamic {
            if fmask & m(DTZMOD) != 0 {
                return Err(BadFormat);
            }
            tz = determine_abbrev_offset(&tm, abbrev, zone.get(cx.zone));
        }
        if fmask & m(TZ) == 0 {
            if fmask & m(DTZMOD) != 0 {
                return Err(BadFormat);
            }
            tz = determine_offset(&tm, cx.zone).0;
        }
    }
    Ok(Decoded { dtype, tm, fsec, tz })
}

/// `DecodeTimeOnly`: the fields of a `time` or a `timetz`. Gives the fields, the microseconds and the zone in seconds west of UTC.
fn decode_time_only(p: &Parsed, cx: &DateTimeInput<'_>) -> Result<(Tm, i32, i32), DtErr> {
    let nf = p.n;
    let mut fmask = 0u32;
    let mut ptype = 0;
    let mut tm = Tm::default();
    let mut fsec = 0;
    let mut tz = 0;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut mer = HR24;
    let mut named: Option<Arc<dyn TimeZone + Send + Sync>> = None;
    let mut dynamic: Option<(ZoneRef, &[u8])> = None;

    for i in 0..nf {
        let field = p.field(i);
        let tmask = match p.kinds[i] {
            DTK_DATE => {
                if i == 0 && nf >= 2 && (p.kinds[nf - 1] == DTK_DATE || p.kinds[1] == DTK_TIME) {
                    decode_date(field, fmask, &mut is2digits, &mut tm, cx.order)?
                } else if field[0].is_ascii_digit() {
                    if fmask & TIME_M == TIME_M {
                        return Err(BadFormat);
                    }
                    let Some(dash) = field.iter().position(|&c| c == b'-') else {
                        return Err(BadFormat);
                    };
                    tz = decode_timezone(&field[dash..])?;
                    let tmask = decode_number_field(
                        &field[..dash],
                        fmask | DATE_M,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?;
                    tmask | m(TZ)
                } else {
                    let Some(zone) = cx.zones.zone(text(field)) else {
                        return Err(DtErr::BadTimezone(text(field).to_string()));
                    };
                    named = Some(zone);
                    m(TZ)
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME_VALUE {
                        return Err(BadFormat);
                    }
                    ptype = 0;
                }
                decode_time(field, &mut tm, &mut fsec)?
            }
            DTK_TZ => {
                tz = decode_timezone(field)?;
                m(TZ)
            }
            DTK_NUMBER if ptype != 0 => {
                let (value, end, overflow) = strtoint(field, 0);
                if overflow {
                    return Err(FieldOverflow);
                }
                if at(field, end) != b'.' && end != field.len() {
                    return Err(BadFormat);
                }
                let tmask = match ptype {
                    DTK_JULIAN => {
                        if value < 0 {
                            return Err(FieldOverflow);
                        }
                        set_date(&mut tm, value);
                        isjulian = true;
                        if at(field, end) == b'.' {
                            let time = parse_fraction(&field[end..])? * USECS_PER_DAY as f64;
                            set_time(&mut tm, &mut fsec, time as i64);
                            DATE_M | TIME_M
                        } else {
                            DATE_M
                        }
                    }
                    DTK_TIME_VALUE => {
                        let tmask = decode_number_field(
                            field,
                            fmask | DATE_M,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        if tmask != TIME_M {
                            return Err(BadFormat);
                        }
                        tmask
                    }
                    _ => return Err(BadFormat),
                };
                ptype = 0;
                tmask
            }
            DTK_NUMBER => {
                let flen = field.len();
                match field.iter().position(|&c| c == b'.') {
                    Some(_) if i == 0 && nf >= 2 && p.kinds[nf - 1] == DTK_DATE => {
                        decode_date(field, fmask, &mut is2digits, &mut tm, cx.order)?
                    }
                    Some(dot) if dot > 2 => decode_number_field(
                        field,
                        fmask | DATE_M,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?,
                    Some(_) => return Err(BadFormat),
                    None if flen > 4 => decode_number_field(
                        field,
                        fmask | DATE_M,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?,
                    None => decode_number(
                        field,
                        false,
                        fmask | DATE_M,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                        cx.order,
                    )?,
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (mut ty, mut val, valtz) = decode_timezone_abbrev(field, cx)?;
                if ty == UNKNOWN_FIELD {
                    (ty, val) = date_token(field).unwrap_or((UNKNOWN_FIELD, 0));
                }
                if ty == IGNORE_DTF {
                    continue;
                }
                let mut tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = TIME_M;
                            (tm, fsec, _) = current_time(cx);
                        }
                        DTK_ZULU => {
                            tmask = TIME_M | m(TZ);
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                        }
                        _ => return Err(BadFormat),
                    },
                    DTZMOD => {
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                    }
                    TZ => tz = -val,
                    DYNTZ => {
                        tmask |= m(TZ);
                        dynamic = valtz.map(|zone| (zone, field));
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    UNITS | ISOTIME => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        named = Some(cx.zones.zone(text(field)).ok_or(BadFormat)?);
                        tmask = m(TZ);
                    }
                    _ => return Err(BadFormat),
                }
                tmask
            }
            _ => return Err(BadFormat),
        };
        if tmask & fmask != 0 {
            return Err(BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(BadFormat);
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    apply_meridian(mer, &mut tm)?;
    if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
        return Err(FieldOverflow);
    }
    if fmask & TIME_M != TIME_M {
        return Err(BadFormat);
    }
    // The date of the zone rules: the date of the input, or today when there is none.
    let local = |tm: &Tm| -> Result<Tm, DtErr> {
        let mut local = if fmask & DATE_M == 0 {
            current_time(cx).0
        } else if fmask & DATE_M != DATE_M {
            return Err(BadFormat);
        } else {
            *tm
        };
        (local.hour, local.min, local.sec) = (tm.hour, tm.min, tm.sec);
        Ok(local)
    };
    if let Some(zone) = &named {
        if fmask & m(DTZMOD) != 0 {
            return Err(BadFormat);
        }
        tz = match zone.fixed_offset() {
            Some(offset) => -offset,
            None => {
                if fmask & DATE_M != DATE_M {
                    return Err(BadFormat);
                }
                determine_offset(&tm, &**zone).0
            }
        };
    }
    if let Some((zone, abbrev)) = &dynamic {
        if fmask & m(DTZMOD) != 0 {
            return Err(BadFormat);
        }
        tz = determine_abbrev_offset(&local(&tm)?, abbrev, zone.get(cx.zone));
    }
    if fmask & m(TZ) == 0 {
        if fmask & m(DTZMOD) != 0 {
            return Err(BadFormat);
        }
        tz = determine_offset(&local(&tm)?, cx.zone).0;
    }
    Ok((tm, fsec, tz))
}

/// `struct pg_itm_in`: the fields of an interval while it is read.
#[derive(Debug, Clone, Copy, Default)]
struct Itm {
    usec: i64,
    mday: i32,
    mon: i32,
    year: i32,
}

impl Itm {
    /// `AdjustFractMicroseconds`.
    fn fract_usecs(&mut self, frac: f64, scale: i64) -> bool {
        if frac == 0.0 {
            return true;
        }
        let frac = frac * scale as f64;
        let mut usec = frac as i64;
        let rest = frac - usec as f64;
        if rest > 0.5 {
            usec += 1;
        } else if rest < -0.5 {
            usec -= 1;
        }
        self.usec.checked_add(usec).map(|usec| self.usec = usec).is_some()
    }

    /// `AdjustFractDays`.
    fn fract_days(&mut self, frac: f64, scale: i32) -> bool {
        if frac == 0.0 {
            return true;
        }
        let frac = frac * f64::from(scale);
        let days = frac as i32;
        match self.mday.checked_add(days) {
            Some(mday) => self.mday = mday,
            None => return false,
        }
        self.fract_usecs(frac - f64::from(days), USECS_PER_DAY)
    }

    /// `AdjustFractYears`.
    fn fract_years(&mut self, frac: f64, scale: i32) -> bool {
        let months = (frac * f64::from(scale) * 12.0).round_ties_even() as i32;
        self.mon.checked_add(months).map(|mon| self.mon = mon).is_some()
    }

    /// `AdjustMicroseconds`.
    fn usecs(&mut self, val: i64, fval: f64, scale: i64) -> bool {
        mul_add(val, scale, &mut self.usec) && self.fract_usecs(fval, scale)
    }

    /// `AdjustDays`.
    fn days(&mut self, val: i64, scale: i32) -> bool {
        let Ok(val) = i32::try_from(val) else { return false };
        match val.checked_mul(scale).and_then(|days| self.mday.checked_add(days)) {
            Some(mday) => {
                self.mday = mday;
                true
            }
            None => false,
        }
    }

    /// `AdjustMonths`.
    fn months(&mut self, val: i64) -> bool {
        let Ok(val) = i32::try_from(val) else { return false };
        self.mon.checked_add(val).map(|mon| self.mon = mon).is_some()
    }

    /// `AdjustYears`.
    fn years(&mut self, val: i64, scale: i32) -> bool {
        let Ok(val) = i32::try_from(val) else { return false };
        match val.checked_mul(scale).and_then(|years| self.year.checked_add(years)) {
            Some(year) => {
                self.year = year;
                true
            }
            None => false,
        }
    }
}

/// `int64_multiply_add`.
fn mul_add(val: i64, multiplier: i64, sum: &mut i64) -> bool {
    match val.checked_mul(multiplier).and_then(|product| sum.checked_add(product)) {
        Some(value) => {
            *sum = value;
            true
        }
        None => false,
    }
}

fn check(ok: bool) -> Result<(), DtErr> {
    if ok { Ok(()) } else { Err(FieldOverflow) }
}

/// `DecodeTimeForInterval`. The time replaces the microseconds read so far, as in the C code.
fn decode_time_for_interval(s: &[u8], range: i32, itm: &mut Itm) -> Result<(), DtErr> {
    let (hour, min, sec, fsec) = decode_time_common(s, range)?;
    itm.usec = i64::from(fsec);
    check(
        mul_add(hour, USECS_PER_HOUR, &mut itm.usec)
            && mul_add(i64::from(min), USECS_PER_MINUTE, &mut itm.usec)
            && mul_add(i64::from(sec), USECS_PER_SEC, &mut itm.usec),
    )
}

/// The unit of the rightmost number of an interval with no unit, from the range of the typmod.
fn default_unit(range: i32) -> i32 {
    let mask = |fields: &[i32]| fields.iter().fold(0, |mask, &field| mask | 1 << field);
    match range {
        r if r == mask(&[YEAR]) => DTK_YEAR,
        r if r == mask(&[MONTH]) || r == mask(&[YEAR, MONTH]) => DTK_MONTH,
        r if r == mask(&[DAY]) => DTK_DAY,
        r if r == mask(&[HOUR]) || r == mask(&[DAY, HOUR]) => DTK_HOUR,
        r if r == mask(&[MINUTE])
            || r == mask(&[HOUR, MINUTE])
            || r == mask(&[DAY, HOUR, MINUTE]) =>
        {
            DTK_MINUTE
        }
        _ => DTK_SECOND,
    }
}

/// `DecodeInterval`: the fields of an interval in the PostgreSQL and the SQL formats. The fields are read from the right, so that a unit comes before its number.
fn decode_interval(p: &Parsed, range: i32, style: IntervalStyle) -> Result<(i32, Itm), DtErr> {
    let nf = p.n;
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask = 0u32;
    let mut ty = IGNORE_DTF;
    let mut dtype = DTK_DELTA;
    let mut itm = Itm::default();

    // In the SQL standard style, a leading minus applies to all the fields when no other field has a sign.
    let force_negative = style == IntervalStyle::SqlStandard
        && nf > 0
        && p.field(0)[0] == b'-'
        && !(1..nf).any(|i| matches!(p.field(i)[0], b'-' | b'+'));

    for i in (0..nf).rev() {
        let field = p.field(i);
        let kind = p.kinds[i];
        let tmask = if kind == DTK_TIME {
            decode_time_for_interval(field, range, &mut itm)?;
            if force_negative && itm.usec > 0 {
                itm.usec = -itm.usec;
            }
            ty = DTK_DAY;
            parsing_unit_val = false;
            TIME_M
        } else if kind == DTK_TZ
            && field[1..].contains(&b':')
            && decode_time_for_interval(&field[1..], range, &mut itm).is_ok()
        {
            // A signed time such as `-01:02:03`.
            if field[0] == b'-' {
                if itm.usec == i64::MIN {
                    return Err(FieldOverflow);
                }
                itm.usec = -itm.usec;
            }
            if force_negative && itm.usec > 0 {
                itm.usec = -itm.usec;
            }
            ty = DTK_DAY;
            parsing_unit_val = false;
            TIME_M
        } else if matches!(kind, DTK_TZ | DTK_DATE | DTK_NUMBER) {
            if ty == IGNORE_DTF {
                ty = default_unit(range);
            }
            let (mut val, end, overflow) = strtol(field, 0);
            if overflow {
                return Err(FieldOverflow);
            }
            let mut fval;
            match at(field, end) {
                b'-' => {
                    // The SQL `years-months` form.
                    let (val2, end, overflow) = strtoint(field, end + 1);
                    if overflow || !(0..12).contains(&val2) {
                        return Err(FieldOverflow);
                    }
                    if end != field.len() {
                        return Err(BadFormat);
                    }
                    ty = DTK_MONTH;
                    let val2 = if field[0] == b'-' { -val2 } else { val2 };
                    val = val
                        .checked_mul(12)
                        .and_then(|v| v.checked_add(i64::from(val2)))
                        .ok_or(FieldOverflow)?;
                    fval = 0.0;
                }
                b'.' => {
                    fval = parse_fraction(&field[end..])?;
                    if field[0] == b'-' {
                        fval = -fval;
                    }
                }
                _ if end == field.len() => fval = 0.0,
                _ => return Err(BadFormat),
            }
            if force_negative {
                if val > 0 {
                    val = -val;
                }
                if fval > 0.0 {
                    fval = -fval;
                }
            }
            let tmask = match ty {
                DTK_MICROSEC => {
                    check(itm.usecs(val, fval, 1))?;
                    m(MICROSECOND)
                }
                DTK_MILLISEC => {
                    check(itm.usecs(val, fval, 1000))?;
                    m(MILLISECOND)
                }
                DTK_SECOND => {
                    check(itm.usecs(val, fval, USECS_PER_SEC))?;
                    // A fraction of a second also fills the smaller units.
                    if fval == 0.0 { m(SECOND) } else { ALL_SECS_M }
                }
                DTK_MINUTE => {
                    check(itm.usecs(val, fval, USECS_PER_MINUTE))?;
                    m(MINUTE)
                }
                DTK_HOUR => {
                    check(itm.usecs(val, fval, USECS_PER_HOUR))?;
                    ty = DTK_DAY;
                    m(HOUR)
                }
                DTK_DAY => {
                    check(itm.days(val, 1) && itm.fract_usecs(fval, USECS_PER_DAY))?;
                    m(DAY)
                }
                DTK_WEEK => {
                    check(itm.days(val, 7) && itm.fract_days(fval, 7))?;
                    m(WEEK)
                }
                DTK_MONTH => {
                    check(itm.months(val) && itm.fract_days(fval, DAYS_PER_MONTH))?;
                    m(MONTH)
                }
                DTK_YEAR => {
                    check(itm.years(val, 1) && itm.fract_years(fval, 1))?;
                    m(YEAR)
                }
                DTK_DECADE => {
                    check(itm.years(val, 10) && itm.fract_years(fval, 10))?;
                    m(DECADE)
                }
                DTK_CENTURY => {
                    check(itm.years(val, 100) && itm.fract_years(fval, 100))?;
                    m(CENTURY)
                }
                DTK_MILLENNIUM => {
                    check(itm.years(val, 1000) && itm.fract_years(fval, 1000))?;
                    m(MILLENNIUM)
                }
                _ => return Err(BadFormat),
            };
            parsing_unit_val = false;
            tmask
        } else if matches!(kind, DTK_STRING | DTK_SPECIAL) {
            if parsing_unit_val {
                return Err(BadFormat);
            }
            let (t, uval) =
                delta_token(field).or_else(|| date_token(field)).unwrap_or((UNKNOWN_FIELD, 0));
            ty = t;
            if ty == IGNORE_DTF {
                continue;
            }
            match ty {
                UNITS => {
                    ty = uval;
                    parsing_unit_val = true;
                    0
                }
                AGO => {
                    // Only at the end.
                    if i != nf - 1 {
                        return Err(BadFormat);
                    }
                    is_before = true;
                    ty = uval;
                    0
                }
                RESERV => {
                    // Only `infinity` and `-infinity`, and nothing after them.
                    if (uval != DTK_LATE && uval != DTK_EARLY) || i != nf - 1 {
                        return Err(BadFormat);
                    }
                    dtype = uval;
                    DATE_M | TIME_M
                }
                _ => return Err(BadFormat),
            }
        } else {
            return Err(BadFormat);
        };
        if tmask & fmask != 0 {
            return Err(BadFormat);
        }
        fmask |= tmask;
    }
    if fmask == 0 || parsing_unit_val {
        return Err(BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN
            || itm.mday == i32::MIN
            || itm.mon == i32::MIN
            || itm.year == i32::MIN
        {
            return Err(FieldOverflow);
        }
        itm = Itm { usec: -itm.usec, mday: -itm.mday, mon: -itm.mon, year: -itm.year };
    }
    Ok((dtype, itm))
}

/// `ParseISO8601Number`: a number as `strtod` reads it, cut into the integer part and the fraction. Gives the two parts and the end.
fn iso8601_number(s: &[u8], pos: usize) -> Result<(i64, f64, usize), DtErr> {
    let c = at(s, pos);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(BadFormat);
    }
    let (value, len) = match strtod::<f64>(&s[pos..]) {
        Some((value, len, false)) if len > 0 => (value, len),
        _ => return Err(BadFormat),
    };
    if value.is_nan() || !(-1.0e15..=1.0e15).contains(&value) {
        return Err(FieldOverflow);
    }
    let ipart = value.trunc() as i64;
    Ok((ipart, value - ipart as f64, pos + len))
}

/// `ISO8601IntegerWidth`: the number of digits before the point, after an optional minus.
fn iso8601_width(s: &[u8], pos: usize) -> usize {
    let pos = pos + usize::from(at(s, pos) == b'-');
    s[pos.min(s.len())..].iter().take_while(|c| c.is_ascii_digit()).count()
}

/// `DecodeISO8601Interval`: `P1Y2M3DT4H5M6S` and the alternative forms `P0001-02-03T04:05:06` and `P00010203T040506`.
fn decode_iso8601_interval(s: &[u8]) -> Result<(i32, Itm), DtErr> {
    let mut itm = Itm::default();
    if s.len() < 2 || s[0] != b'P' {
        return Err(BadFormat);
    }
    let mut pos = 1;
    let mut datepart = true;
    let mut havefield = false;
    while at(s, pos) != 0 {
        if at(s, pos) == b'T' {
            datepart = false;
            havefield = false;
            pos += 1;
            continue;
        }
        let fieldstart = pos;
        let (val, fval, end) = iso8601_number(s, pos)?;
        let unit = at(s, end);
        pos = end + 1;
        if datepart {
            match unit {
                b'Y' => check(itm.years(val, 1) && itm.fract_years(fval, 1))?,
                b'M' => check(itm.months(val) && itm.fract_days(fval, DAYS_PER_MONTH))?,
                b'W' => check(itm.days(val, 7) && itm.fract_days(fval, 7))?,
                b'D' => check(itm.days(val, 1) && itm.fract_usecs(fval, USECS_PER_DAY))?,
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso8601_width(s, fieldstart) == 8 && !havefield {
                        // The basic alternative form, `yyyymmdd`.
                        check(
                            itm.years(val / 10000, 1)
                                && itm.months((val / 100) % 100)
                                && itm.days(val % 100, 1)
                                && itm.fract_usecs(fval, USECS_PER_DAY),
                        )?;
                        if unit == 0 {
                            return Ok((DTK_DELTA, itm));
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    // The extended alternative form, `yyyy-mm-dd`.
                    if havefield {
                        return Err(BadFormat);
                    }
                    check(itm.years(val, 1) && itm.fract_years(fval, 1))?;
                    if unit == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (val, fval, end) = iso8601_number(s, pos)?;
                    pos = end;
                    check(itm.months(val) && itm.fract_days(fval, DAYS_PER_MONTH))?;
                    if at(s, pos) == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    if at(s, pos) == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if at(s, pos) != b'-' {
                        return Err(BadFormat);
                    }
                    let (val, fval, end) = iso8601_number(s, pos + 1)?;
                    pos = end;
                    check(itm.days(val, 1) && itm.fract_usecs(fval, USECS_PER_DAY))?;
                    if at(s, pos) == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    if at(s, pos) == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    return Err(BadFormat);
                }
                _ => return Err(BadFormat),
            }
        } else {
            match unit {
                b'H' => check(itm.usecs(val, fval, USECS_PER_HOUR))?,
                b'M' => check(itm.usecs(val, fval, USECS_PER_MINUTE))?,
                b'S' => check(itm.usecs(val, fval, USECS_PER_SEC))?,
                0 | b':' => {
                    if unit == 0 && iso8601_width(s, fieldstart) == 6 && !havefield {
                        // The basic alternative form, `hhmmss`.
                        check(
                            itm.usecs(val / 10000, 0.0, USECS_PER_HOUR)
                                && itm.usecs((val / 100) % 100, 0.0, USECS_PER_MINUTE)
                                && itm.usecs(val % 100, 0.0, USECS_PER_SEC)
                                && itm.fract_usecs(fval, 1),
                        )?;
                        return Ok((DTK_DELTA, itm));
                    }
                    // The extended alternative form, `hh:mm:ss`.
                    if havefield {
                        return Err(BadFormat);
                    }
                    check(itm.usecs(val, fval, USECS_PER_HOUR))?;
                    if unit == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    let (val, fval, end) = iso8601_number(s, pos)?;
                    pos = end;
                    check(itm.usecs(val, fval, USECS_PER_MINUTE))?;
                    if at(s, pos) == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    if at(s, pos) != b':' {
                        return Err(BadFormat);
                    }
                    let (val, fval, end) = iso8601_number(s, pos + 1)?;
                    pos = end;
                    check(itm.usecs(val, fval, USECS_PER_SEC))?;
                    if at(s, pos) == 0 {
                        return Ok((DTK_DELTA, itm));
                    }
                    return Err(BadFormat);
                }
                _ => return Err(BadFormat),
            }
        }
        havefield = true;
    }
    Ok((DTK_DELTA, itm))
}

/// `tm2timestamp`: the timestamp of the fields, with the zone in seconds west of UTC for a `timestamptz`.
fn tm2timestamp(tm: &Tm, fsec: i32, tz: Option<i32>) -> Option<i64> {
    if !is_valid_julian(tm.year, tm.mon) {
        return None;
    }
    let date = i64::from(date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE);
    let time = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec))
        * USECS_PER_SEC
        + i64::from(fsec);
    let mut ts = date.checked_mul(USECS_PER_DAY)?.checked_add(time)?;
    if let Some(tz) = tz {
        ts = ts.wrapping_add(i64::from(tz) * USECS_PER_SEC);
    }
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&ts).then_some(ts)
}

/// `date_in`: the days since 2000-01-01.
pub fn date_in(input: &str, cx: &DateTimeInput<'_>) -> Result<i32, TypeError> {
    match iso_date(input.as_bytes()) {
        Some(date) => Ok(date),
        None => parsed_date(input, cx),
    }
}

/// `date_in` through `ParseDateTime` and `DecodeDateTime`.
fn parsed_date(input: &str, cx: &DateTimeInput<'_>) -> Result<i32, TypeError> {
    let error = |e: DtErr| e.into_error(input, "date");
    let parsed = parse_date_time(c_string(input), DATE_BUFLEN).map_err(error)?;
    let decoded = decode_date_time(&parsed, cx).map_err(error)?;
    let tm = match decoded.dtype {
        DTK_DATE_VALUE => decoded.tm,
        DTK_EPOCH => Tm { year: 1970, mon: 1, mday: 1, ..Tm::default() },
        DTK_LATE => return Ok(DATE_INFINITY),
        DTK_EARLY => return Ok(DATE_NEGATIVE_INFINITY),
        _ => return Err(error(BadFormat)),
    };
    let range = || {
        TypeError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("date out of range: \"{input}\""),
        )
    };
    if !is_valid_julian(tm.year, tm.mon) {
        return Err(range());
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !(-POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE).contains(&date) {
        return Err(range());
    }
    Ok(date)
}

/// The date of a valid `YYYY-MM-DD`, which every `DateStyle` reads the same way. Other input gives `None` and goes through the full parser, which also gives the errors.
fn iso_date(b: &[u8]) -> Option<i32> {
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        b[range]
            .iter()
            .try_fold(0, |n: i32, &c| c.is_ascii_digit().then(|| n * 10 + (c - b'0') as i32))
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(date2j(year, month, day) - POSTGRES_EPOCH_JDATE)
}

/// `DecodeTimeOnly` for `time_in` and `timetz_in`: the time in microseconds and the zone.
fn time_value(
    input: &str,
    type_name: &str,
    cx: &DateTimeInput<'_>,
) -> Result<(i64, i32), TypeError> {
    let error = |e: DtErr| e.into_error(input, type_name);
    let parsed = parse_date_time(c_string(input), DATE_BUFLEN).map_err(error)?;
    let (tm, fsec, tz) = decode_time_only(&parsed, cx).map_err(error)?;
    let time = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec))
        * USECS_PER_SEC
        + i64::from(fsec);
    Ok((time, tz))
}

/// `time_in`: the microseconds since midnight, rounded to the typmod.
pub fn time_in(input: &str, typmod: i32, cx: &DateTimeInput<'_>) -> Result<i64, TypeError> {
    let (time, _) = time_value(input, "time", cx)?;
    Ok(adjust_time(time, typmod))
}

/// `timetz_in`: the time and the zone in seconds west of UTC.
pub fn timetz_in(
    input: &str,
    typmod: i32,
    cx: &DateTimeInput<'_>,
) -> Result<(i64, i32), TypeError> {
    let (time, tz) = time_value(input, "time with time zone", cx)?;
    Ok((adjust_time(time, typmod), tz))
}

fn timestamp_value(
    input: &str,
    typmod: i32,
    cx: &DateTimeInput<'_>,
    with_zone: bool,
) -> Result<i64, TypeError> {
    let type_name = if with_zone { "timestamp with time zone" } else { "timestamp" };
    let error = |e: DtErr| e.into_error(input, type_name);
    let parsed = parse_date_time(c_string(input), TIMESTAMP_BUFLEN).map_err(error)?;
    let decoded = decode_date_time(&parsed, cx).map_err(error)?;
    let ts = match decoded.dtype {
        DTK_DATE_VALUE => tm2timestamp(&decoded.tm, decoded.fsec, with_zone.then_some(decoded.tz))
            .ok_or_else(|| {
                TypeError::new(
                    SqlState::DATETIME_VALUE_OUT_OF_RANGE,
                    format!("timestamp out of range: \"{input}\""),
                )
            })?,
        DTK_EPOCH => -946_684_800 * USECS_PER_SEC,
        DTK_LATE => TIMESTAMP_INFINITY,
        _ => TIMESTAMP_NEGATIVE_INFINITY,
    };
    adjust_timestamp(ts, typmod)
}

/// `timestamp_in`: the microseconds since 2000-01-01 in local time, rounded to the typmod.
pub fn timestamp_in(input: &str, typmod: i32, cx: &DateTimeInput<'_>) -> Result<i64, TypeError> {
    timestamp_value(input, typmod, cx, false)
}

/// `timestamptz_in`: the microseconds since 2000-01-01 UTC, rounded to the typmod. A value with no zone is in the session zone.
pub fn timestamptz_in(input: &str, typmod: i32, cx: &DateTimeInput<'_>) -> Result<i64, TypeError> {
    timestamp_value(input, typmod, cx, true)
}

/// `interval_in`: the PostgreSQL, SQL and ISO 8601 forms. The typmod gives the unit of a number with no unit, and the result is cut and rounded to it.
pub fn interval_in(input: &str, typmod: i32, style: IntervalStyle) -> Result<Interval, TypeError> {
    let range = if typmod >= 0 { (typmod >> 16) & 0x7fff } else { INTERVAL_FULL_RANGE };
    let bytes = c_string(input);
    let mut result =
        parse_date_time(bytes, INTERVAL_BUFLEN).and_then(|p| decode_interval(&p, range, style));
    if matches!(result, Err(BadFormat)) {
        result = decode_iso8601_interval(bytes);
    }
    let (dtype, itm) = result.map_err(|e| {
        let e = if e == FieldOverflow { DtErr::IntervalOverflow } else { e };
        e.into_error(input, "interval")
    })?;
    let iv = match dtype {
        DTK_LATE => Interval::INFINITY,
        DTK_EARLY => Interval::NEGATIVE_INFINITY,
        _ => {
            let months = i64::from(itm.year) * 12 + i64::from(itm.mon);
            let month = i32::try_from(months).map_err(|_| out_of_range("interval"))?;
            Interval { time: itm.usec, day: itm.mday, month }
        }
    };
    adjust_interval(iv, typmod)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::FixedZone;

    fn fields(input: &str) -> Vec<(u8, String)> {
        let p = parse_date_time(input.as_bytes(), INTERVAL_BUFLEN).unwrap();
        (0..p.n).map(|i| (p.kinds[i], text(p.field(i)).to_string())).collect()
    }

    #[test]
    fn the_fields_have_the_kinds_of_parse_date_time() {
        assert_eq!(
            fields("Feb-7-1997 15:23:27.5 +05:30"),
            [
                (DTK_DATE, "feb-7-1997".to_string()),
                (DTK_TIME, "15:23:27.5".to_string()),
                (DTK_TZ, "+05:30".to_string()),
            ]
        );
        assert_eq!(fields("1997.038"), [(DTK_NUMBER, "1997.038".to_string())]);
        assert_eq!(fields("America/New_York"), [(DTK_DATE, "america/new_york".to_string())]);
        assert_eq!(
            fields("j2451187"),
            [(DTK_STRING, "j".to_string()), (DTK_NUMBER, "2451187".to_string())]
        );
        assert_eq!(fields("- infinity"), [(DTK_SPECIAL, "-infinity".to_string())]);
        assert_eq!(parse_date_time("1 \u{e9}".as_bytes(), 256).err(), Some(BadFormat));
    }

    #[test]
    fn the_zone_abbreviations_parse() {
        let abbrevs = ZoneAbbrevs::postgres_default();
        assert!(abbrevs.len() > 150);
        assert_eq!(
            abbrevs.get("est"),
            Some(("est", &Abbrev::Fixed { offset: -18000, dst: false }))
        );
        assert_eq!(abbrevs.get("pdt"), Some(("pdt", &Abbrev::Fixed { offset: -25200, dst: true })));
        assert_eq!(abbrevs.get("msk"), Some(("msk", &Abbrev::Zone("Europe/Moscow".to_string()))));
        assert!(ZoneAbbrevs::parse("TOOLONGABBREV 0").is_err());
        assert!(ZoneAbbrevs::parse("X 0 junk").is_err());
        assert!(ZoneAbbrevs::parse("X 0\nX 3600").is_err());
    }

    #[test]
    fn the_input_reads_the_common_forms() {
        let utc = FixedZone::utc();
        let cx = DateTimeInput {
            order: DateOrder::Mdy,
            zone: &utc,
            zones: &NoZones,
            abbrevs: ZoneAbbrevs::postgres_default(),
            now: 0,
        };
        assert_eq!(date_in("2000-01-02", &cx), Ok(1));
        assert_eq!(date_in("today", &cx), Ok(0));
        assert_eq!(timestamp_in("2000-01-01 00:00:01.5", -1, &cx), Ok(1_500_000));
        assert_eq!(timestamptz_in("2000-01-01 00:00 EST", -1, &cx), Ok(5 * USECS_PER_HOUR));
        assert_eq!(time_in("allballs", -1, &cx), Ok(0));
        assert_eq!(
            timetz_in("04:05 -08", -1, &cx),
            Ok((4 * USECS_PER_HOUR + 5 * USECS_PER_MINUTE, 28800))
        );
        let day = Interval { time: 0, day: 1, month: 0 };
        assert_eq!(interval_in("1 day", -1, IntervalStyle::Postgres), Ok(day));
        assert_eq!(interval_in("P1D", -1, IntervalStyle::Postgres), Ok(day));
        let error = date_in("2000-13-01", &cx).unwrap_err();
        assert_eq!(
            error.hint.as_deref(),
            Some("Perhaps you need a different \"DateStyle\" setting.")
        );
    }

    #[test]
    fn the_iso_date_path_agrees_with_the_parser() {
        let utc = FixedZone::utc();
        for order in [DateOrder::Mdy, DateOrder::Dmy, DateOrder::Ymd] {
            let cx = DateTimeInput {
                order,
                zone: &utc,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: 0,
            };
            for year in [1, 4, 100, 1582, 1900, 1999, 2000, 2024, 2100, 9999] {
                for month in 0..=13 {
                    for day in 0..=32 {
                        let input = format!("{year:04}-{month:02}-{day:02}");
                        match iso_date(input.as_bytes()) {
                            Some(date) => assert_eq!(parsed_date(&input, &cx), Ok(date), "{input}"),
                            None => assert!(parsed_date(&input, &cx).is_err(), "{input}"),
                        }
                    }
                }
            }
        }
    }
}

//! `to_char` of a timestamp, a `timestamptz` and an interval, `to_timestamp` and `to_date`: the date and time patterns of `formatting.c`. The `TM` prefix gives the names of `lc_time`, and rupg has only the C locale, so these are the English names.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    DateTimeInput, Interval, POSTGRES_EPOCH_JDATE, TimeZone, UNIX_EPOCH_JDATE,
    UNIX_TO_POSTGRES_USECS, USECS_PER_SEC, Value, ZoneName, adjust_timestamp, date2j,
    decode_timezone_abbrev_prefix, j2date, local_offset,
};

use crate::calendar::valid_julian;
use crate::cast::{infinite, timestamp_range};
use crate::datetime::{Tm, days_of_month};
use crate::extract::{iso_week, iso_week_start};
use crate::io::datetime_input;
use crate::math::valid_date;
use crate::{Call, Kernel, bad_value, type_error};

const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 3600 * USECS_PER_SEC;

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "timestamp_to_char" => timestamp_to_char,
        "timestamptz_to_char" => timestamptz_to_char,
        "interval_to_char" => interval_to_char,
        "to_timestamp" => to_timestamp,
        "to_date" => to_date,
        _ => return None,
    })
}

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAYS: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
/// `rm_months_upper`: December first.
const ROMAN_MONTHS: [&str; 12] =
    ["XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I"];

/// The case of a name or of a mark such as `AD`.
#[derive(Clone, Copy)]
enum Case {
    Upper,
    Initial,
    Lower,
}

impl Case {
    fn apply(self, text: &str) -> String {
        match self {
            Case::Upper => text.to_ascii_uppercase(),
            Case::Initial => text.to_string(),
            Case::Lower => text.to_ascii_lowercase(),
        }
    }
}

/// A pattern of `DCH_keywords`. The patterns that give the same output share a variant.
#[derive(Clone, Copy)]
enum Pattern {
    /// `AD`, `BC`, `A.D.`, `B.C.` and the lower case forms.
    Era {
        dots: bool,
        case: Case,
    },
    /// `AM`, `PM`, `A.M.`, `P.M.` and the lower case forms.
    Meridiem {
        dots: bool,
        case: Case,
    },
    Century,
    DayName(Case),
    DayShort(Case),
    DayOfYear,
    IsoDayOfYear,
    DayOfMonth,
    DayOfWeek,
    IsoDayOfWeek,
    /// `FF1` to `FF6`: the number of digits of the fraction of the second.
    Fraction(u32),
    /// `MS`.
    Millisecond,
    /// `US`.
    Microsecond,
    /// `FX` has an effect only in the input functions.
    Fx,
    Hour24,
    Hour12,
    IsoWeek,
    /// `IYYY`, `IYY`, `IY` and `I`: the number of digits.
    IsoYear(u32),
    Julian,
    Minute,
    Month,
    MonthName(Case),
    MonthShort(Case),
    Offset,
    Quarter,
    Roman {
        upper: bool,
    },
    SecondsOfDay,
    Second,
    ZoneHour,
    ZoneMinute,
    Zone {
        upper: bool,
    },
    WeekOfYear,
    WeekOfMonth,
    /// `Y,YYY`.
    YearComma,
    /// `YYYY`, `YYY`, `YY` and `Y`: the number of digits.
    Year(u32),
}

use Pattern as P;

/// `FromCharDateMode`: the date convention of a pattern in the input functions.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    None,
    Gregorian,
    IsoWeek,
}

impl Pattern {
    /// `is_digit` of `DCH_keywords`: the input of the pattern is a number.
    fn is_digit(self) -> bool {
        !matches!(
            self,
            P::Era { .. }
                | P::Meridiem { .. }
                | P::DayName(_)
                | P::DayShort(_)
                | P::MonthName(_)
                | P::MonthShort(_)
                | P::Fx
                | P::Offset
                | P::Roman { .. }
                | P::ZoneHour
                | P::Zone { .. }
        )
    }

    /// `date_mode` of `DCH_keywords`.
    fn mode(self) -> Mode {
        match self {
            P::DayOfYear
            | P::DayOfMonth
            | P::DayOfWeek
            | P::Month
            | P::MonthName(_)
            | P::MonthShort(_)
            | P::Roman { .. }
            | P::WeekOfYear
            | P::WeekOfMonth
            | P::YearComma
            | P::Year(_) => Mode::Gregorian,
            P::IsoDayOfYear | P::IsoDayOfWeek | P::IsoWeek | P::IsoYear(_) => Mode::IsoWeek,
            _ => Mode::None,
        }
    }
}

/// `DCH_keywords`, in the order of the search: a longer pattern comes before a pattern that is its prefix.
const KEYWORDS: [(&str, Pattern); 112] = [
    ("A.D.", P::Era { dots: true, case: Case::Upper }),
    ("A.M.", P::Meridiem { dots: true, case: Case::Upper }),
    ("AD", P::Era { dots: false, case: Case::Upper }),
    ("AM", P::Meridiem { dots: false, case: Case::Upper }),
    ("B.C.", P::Era { dots: true, case: Case::Upper }),
    ("BC", P::Era { dots: false, case: Case::Upper }),
    ("CC", P::Century),
    ("DAY", P::DayName(Case::Upper)),
    ("DDD", P::DayOfYear),
    ("DD", P::DayOfMonth),
    ("DY", P::DayShort(Case::Upper)),
    ("Day", P::DayName(Case::Initial)),
    ("Dy", P::DayShort(Case::Initial)),
    ("D", P::DayOfWeek),
    ("FF1", P::Fraction(1)),
    ("FF2", P::Fraction(2)),
    ("FF3", P::Fraction(3)),
    ("FF4", P::Fraction(4)),
    ("FF5", P::Fraction(5)),
    ("FF6", P::Fraction(6)),
    ("FX", P::Fx),
    ("HH24", P::Hour24),
    ("HH12", P::Hour12),
    ("HH", P::Hour12),
    ("IDDD", P::IsoDayOfYear),
    ("ID", P::IsoDayOfWeek),
    ("IW", P::IsoWeek),
    ("IYYY", P::IsoYear(4)),
    ("IYY", P::IsoYear(3)),
    ("IY", P::IsoYear(2)),
    ("I", P::IsoYear(1)),
    ("J", P::Julian),
    ("MI", P::Minute),
    ("MM", P::Month),
    ("MONTH", P::MonthName(Case::Upper)),
    ("MON", P::MonthShort(Case::Upper)),
    ("MS", P::Millisecond),
    ("Month", P::MonthName(Case::Initial)),
    ("Mon", P::MonthShort(Case::Initial)),
    ("OF", P::Offset),
    ("P.M.", P::Meridiem { dots: true, case: Case::Upper }),
    ("PM", P::Meridiem { dots: false, case: Case::Upper }),
    ("Q", P::Quarter),
    ("RM", P::Roman { upper: true }),
    ("SSSSS", P::SecondsOfDay),
    ("SSSS", P::SecondsOfDay),
    ("SS", P::Second),
    ("TZH", P::ZoneHour),
    ("TZM", P::ZoneMinute),
    ("TZ", P::Zone { upper: true }),
    ("US", P::Microsecond),
    ("WW", P::WeekOfYear),
    ("W", P::WeekOfMonth),
    ("Y,YYY", P::YearComma),
    ("YYYY", P::Year(4)),
    ("YYY", P::Year(3)),
    ("YY", P::Year(2)),
    ("Y", P::Year(1)),
    ("a.d.", P::Era { dots: true, case: Case::Lower }),
    ("a.m.", P::Meridiem { dots: true, case: Case::Lower }),
    ("ad", P::Era { dots: false, case: Case::Lower }),
    ("am", P::Meridiem { dots: false, case: Case::Lower }),
    ("b.c.", P::Era { dots: true, case: Case::Lower }),
    ("bc", P::Era { dots: false, case: Case::Lower }),
    ("cc", P::Century),
    ("day", P::DayName(Case::Lower)),
    ("ddd", P::DayOfYear),
    ("dd", P::DayOfMonth),
    ("dy", P::DayShort(Case::Lower)),
    ("d", P::DayOfWeek),
    ("ff1", P::Fraction(1)),
    ("ff2", P::Fraction(2)),
    ("ff3", P::Fraction(3)),
    ("ff4", P::Fraction(4)),
    ("ff5", P::Fraction(5)),
    ("ff6", P::Fraction(6)),
    ("fx", P::Fx),
    ("hh24", P::Hour24),
    ("hh12", P::Hour12),
    ("hh", P::Hour12),
    ("iddd", P::IsoDayOfYear),
    ("id", P::IsoDayOfWeek),
    ("iw", P::IsoWeek),
    ("iyyy", P::IsoYear(4)),
    ("iyy", P::IsoYear(3)),
    ("iy", P::IsoYear(2)),
    ("i", P::IsoYear(1)),
    ("j", P::Julian),
    ("mi", P::Minute),
    ("mm", P::Month),
    ("month", P::MonthName(Case::Lower)),
    ("mon", P::MonthShort(Case::Lower)),
    ("ms", P::Millisecond),
    ("of", P::Offset),
    ("p.m.", P::Meridiem { dots: true, case: Case::Lower }),
    ("pm", P::Meridiem { dots: false, case: Case::Lower }),
    ("q", P::Quarter),
    ("rm", P::Roman { upper: false }),
    ("sssss", P::SecondsOfDay),
    ("ssss", P::SecondsOfDay),
    ("ss", P::Second),
    ("tzh", P::ZoneHour),
    ("tzm", P::ZoneMinute),
    ("tz", P::Zone { upper: false }),
    ("us", P::Microsecond),
    ("ww", P::WeekOfYear),
    ("w", P::WeekOfMonth),
    ("y,yyy", P::YearComma),
    ("yyyy", P::Year(4)),
    ("yyy", P::Year(3)),
    ("yy", P::Year(2)),
    ("y", P::Year(1)),
];

/// The kind of a character of the format. The input functions read a space or a separator in a different way from other text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Space,
    Separator,
}

/// A node of the parsed format: a pattern with its prefixes and its suffix, or a character that is copied.
enum Node {
    Action {
        /// The pattern as the format gives it, for the errors.
        name: &'static str,
        pattern: Pattern,
        /// The `FM` prefix: no padding.
        fill: bool,
        /// The `TM` prefix: the names of the locale.
        locale: bool,
        /// The `TH` or `th` suffix: the ordinal suffix in upper case or lower case.
        ordinal: Option<Case>,
    },
    Char(char, Kind),
}

/// `isspace` in the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `is_separator_char`: a printable ASCII character that is not a letter or a digit.
fn is_separator(c: u32) -> bool {
    (0x21..0x7f).contains(&c) && !char::from_u32(c).is_some_and(|c| c.is_ascii_alphanumeric())
}

/// `parse_format` with `DCH_FLAG`.
fn parse(format: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut rest = format;
    while !rest.is_empty() {
        let (mut fill, mut locale) = (false, false);
        // `DCH_suff`: the prefixes are `FM`, `fm`, `TM` and `tm`.
        if let Some(r) = rest.strip_prefix("FM").or_else(|| rest.strip_prefix("fm")) {
            fill = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("TM").or_else(|| rest.strip_prefix("tm")) {
            locale = true;
            rest = r;
        }
        if let Some((name, pattern)) = KEYWORDS.iter().find(|(name, _)| rest.starts_with(name)) {
            rest = &rest[name.len()..];
            let mut ordinal = None;
            if let Some(r) = rest.strip_prefix("TH") {
                ordinal = Some(Case::Upper);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("th") {
                ordinal = Some(Case::Lower);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("SP") {
                // `SP` is accepted and has no effect.
                rest = r;
            }
            nodes.push(Node::Action { name, pattern: *pattern, fill, locale, ordinal });
        } else if let Some(r) = rest.strip_prefix('"') {
            // A string in double quotes, where a backslash quotes the next character.
            let mut chars = r.chars();
            loop {
                match chars.next() {
                    None | Some('"') => break,
                    Some('\\') => {
                        let after = chars.as_str();
                        match chars.next() {
                            Some(c) => nodes.push(Node::Char(c, Kind::Text)),
                            None => {
                                nodes.push(Node::Char('\\', Kind::Text));
                                chars = after.chars();
                            }
                        }
                    }
                    Some(c) => nodes.push(Node::Char(c, Kind::Text)),
                }
            }
            rest = chars.as_str();
        } else {
            // Outside a quoted string, a backslash is special only before a double quote.
            if rest.starts_with("\\\"") {
                rest = &rest[1..];
            }
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                let kind = if is_separator(c as u32) {
                    Kind::Separator
                } else if c.is_ascii() && is_space(c as u8) {
                    Kind::Space
                } else {
                    Kind::Text
                };
                nodes.push(Node::Char(c, kind));
            }
            rest = chars.as_str();
        }
    }
    nodes
}

/// `struct fmt_tm` and the fraction and the zone of `TmToChar`.
struct Fields<'a> {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i64,
    min: i32,
    sec: i32,
    /// The microseconds.
    fsec: i32,
    /// The day of the week, with Sunday as 0.
    wday: i32,
    yday: i32,
    /// The offset east of UTC in seconds.
    gmtoff: i32,
    zone: Option<&'a str>,
}

impl Fields<'_> {
    /// The fields of a timestamp with no zone, or of a local time.
    #[allow(clippy::cast_possible_truncation)]
    fn of(tm: &Tm) -> Fields<'static> {
        let thisdate = date2j(tm.year, tm.mon, tm.mday);
        Fields {
            year: tm.year,
            mon: tm.mon,
            mday: tm.mday,
            hour: tm.time / USECS_PER_HOUR,
            min: (tm.time % USECS_PER_HOUR / USECS_PER_MINUTE) as i32,
            sec: (tm.time % USECS_PER_MINUTE / USECS_PER_SEC) as i32,
            fsec: (tm.time % USECS_PER_SEC) as i32,
            wday: (thisdate + 1) % 7,
            yday: thisdate - date2j(tm.year, 1, 1) + 1,
            gmtoff: 0,
            zone: None,
        }
    }

    /// `ADJUST_YEAR`: there is no year 0, so year 0 is 1 BC. An interval keeps its years.
    fn adjust_year(year: i32, interval: bool) -> i32 {
        if interval || year > 0 { year } else { -(year - 1) }
    }

    fn iso_year(&self) -> i32 {
        iso_week(self.year, self.mon, self.mday).1
    }
}

/// `get_th`: the ordinal suffix of the digits.
fn ordinal(digits: &str, case: Case) -> &'static str {
    let bytes = digits.as_bytes();
    let last = match bytes {
        [.., b'1', _] => b'0',
        [.., last] => *last,
        [] => b'0',
    };
    let suffix = match last {
        b'1' => "st",
        b'2' => "nd",
        b'3' => "rd",
        _ => "th",
    };
    match case {
        Case::Upper => match suffix {
            "st" => "ST",
            "nd" => "ND",
            "rd" => "RD",
            _ => "TH",
        },
        _ => suffix,
    }
}

/// `INVALID_FOR_INTERVAL`.
fn invalid_for_interval() -> Error {
    Error::new(
        SqlState::INVALID_DATETIME_FORMAT,
        "invalid format specification for an interval value",
    )
    .with_hint("Intervals are not tied to specific calendar dates.")
}

/// `DCH_to_char`.
#[allow(clippy::too_many_lines)]
fn to_char(nodes: &[Node], interval: bool, tm: &Fields<'_>) -> Result<String> {
    let mut out = String::new();
    for node in nodes {
        let &Node::Action { pattern, fill, locale, ordinal: th, .. } = node else {
            if let Node::Char(c, _) = node {
                out.push(*c);
            }
            continue;
        };
        // The width of a number: none with `FM`, and one more for a negative number.
        let width = |w: usize, negative: bool| if fill { 0 } else { w + usize::from(negative) };
        // A number, then the ordinal suffix of `TH` or `th`.
        let mut number = |v: i64, w: usize| {
            let digits = format!("{v:0w$}");
            out.push_str(&digits);
            if let Some(case) = th {
                out.push_str(ordinal(&digits, case));
            }
        };
        let name = |out: &mut String, text: &str, case: Case, pad: bool| {
            let text = case.apply(text);
            if pad && !fill && !locale {
                out.push_str(&format!("{text:<9}"));
            } else {
                out.push_str(&text);
            }
        };
        let adjusted = Fields::adjust_year(tm.year, interval);
        match pattern {
            P::Era { .. }
            | P::DayName(_)
            | P::DayShort(_)
            | P::MonthName(_)
            | P::MonthShort(_)
            | P::DayOfWeek
            | P::IsoDayOfWeek
            | P::Offset
            | P::ZoneHour
            | P::ZoneMinute
            | P::Zone { .. }
                if interval =>
            {
                return Err(invalid_for_interval());
            }
            P::Meridiem { dots, case } => {
                let pm = tm.hour % 24 >= 12;
                let text = match (pm, dots) {
                    (false, false) => "AM",
                    (false, true) => "A.M.",
                    (true, false) => "PM",
                    (true, true) => "P.M.",
                };
                out.push_str(&case.apply(text));
            }
            P::Era { dots, case } => {
                let text = match (tm.year <= 0, dots) {
                    (false, false) => "AD",
                    (false, true) => "A.D.",
                    (true, false) => "BC",
                    (true, true) => "B.C.",
                };
                out.push_str(&case.apply(text));
            }
            P::Hour12 => {
                let h = match tm.hour % 12 {
                    0 => 12,
                    h => h,
                };
                number(h, width(2, tm.hour < 0));
            }
            P::Hour24 => number(tm.hour, width(2, tm.hour < 0)),
            P::Minute => number(i64::from(tm.min), width(2, tm.min < 0)),
            P::Second => number(i64::from(tm.sec), width(2, tm.sec < 0)),
            P::Fraction(_) | P::Millisecond | P::Microsecond => {
                let digits = match pattern {
                    P::Fraction(digits) => digits,
                    P::Millisecond => 3,
                    _ => 6,
                };
                let v = tm.fsec / 10i32.pow(6 - digits);
                number(i64::from(v), digits as usize);
            }
            P::SecondsOfDay => {
                number(tm.hour * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec), 0);
            }
            P::Zone { upper } => {
                if let Some(zone) = tm.zone {
                    out.push_str(&if upper { zone.to_string() } else { zone.to_ascii_lowercase() });
                }
            }
            P::ZoneHour => {
                let sign = if tm.gmtoff >= 0 { '+' } else { '-' };
                out.push_str(&format!("{sign}{:02}", tm.gmtoff.abs() / 3600));
            }
            P::ZoneMinute => out.push_str(&format!("{:02}", tm.gmtoff.abs() % 3600 / 60)),
            P::Offset => {
                let sign = if tm.gmtoff >= 0 { '+' } else { '-' };
                let w = if fill { 0 } else { 2 };
                out.push_str(&format!("{sign}{:0w$}", tm.gmtoff.abs() / 3600));
                if tm.gmtoff.abs() % 3600 != 0 {
                    out.push_str(&format!(":{:02}", tm.gmtoff.abs() % 3600 / 60));
                }
            }
            P::MonthName(case) => {
                if tm.mon != 0 {
                    name(&mut out, MONTHS_FULL[(tm.mon - 1) as usize], case, true);
                }
            }
            P::MonthShort(case) => {
                if tm.mon != 0 {
                    name(&mut out, MONTHS[(tm.mon - 1) as usize], case, false);
                }
            }
            P::Month => number(i64::from(tm.mon), width(2, tm.mon < 0)),
            P::DayName(case) => name(&mut out, DAYS[tm.wday as usize], case, true),
            P::DayShort(case) => name(&mut out, DAYS_SHORT[tm.wday as usize], case, false),
            P::DayOfYear => number(i64::from(tm.yday), width(3, false)),
            P::IsoDayOfYear => {
                let day = date2j(tm.year, tm.mon, tm.mday) - iso_week_start(tm.iso_year()) + 1;
                number(i64::from(day), width(3, false));
            }
            P::DayOfMonth => number(i64::from(tm.mday), width(2, false)),
            P::DayOfWeek => number(i64::from(tm.wday + 1), 0),
            P::IsoDayOfWeek => number(i64::from(if tm.wday == 0 { 7 } else { tm.wday }), 0),
            P::WeekOfYear => number(i64::from((tm.yday - 1) / 7 + 1), width(2, false)),
            P::IsoWeek => {
                number(i64::from(iso_week(tm.year, tm.mon, tm.mday).0), width(2, false));
            }
            P::Quarter => {
                if tm.mon != 0 {
                    number(i64::from((tm.mon - 1) / 3 + 1), 0);
                }
            }
            P::Century => {
                let i = if interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    // Century 20 is 1901 to 2000.
                    (tm.year - 1) / 100 + 1
                } else {
                    // Century 6 BC is 600 BC to 501 BC.
                    tm.year / 100 - 1
                };
                let w = if (-99..=99).contains(&i) { width(2, i < 0) } else { 0 };
                number(i64::from(i), w);
            }
            P::YearComma => {
                let i = adjusted / 1000;
                let digits = format!("{i},{:03}", adjusted - i * 1000);
                out.push_str(&digits);
                if let Some(case) = th {
                    out.push_str(ordinal(&digits, case));
                }
            }
            P::Year(digits) | P::IsoYear(digits) => {
                let year = match pattern {
                    P::Year(_) => adjusted,
                    _ => Fields::adjust_year(tm.iso_year(), interval),
                };
                let (v, w) = match digits {
                    4 => (year, width(4, adjusted < 0)),
                    3 => (year % 1000, width(3, adjusted < 0)),
                    2 => (year % 100, width(2, adjusted < 0)),
                    _ => (year % 10, 1),
                };
                number(i64::from(v), w);
            }
            P::Roman { upper } => {
                // An interval of whole years has no month, and it gives the month of the sign.
                if tm.mon != 0 || tm.year != 0 {
                    let i = match tm.mon {
                        0 if tm.year >= 0 => 0,
                        0 => 11,
                        m if m < 0 => -(m + 1),
                        m => 12 - m,
                    };
                    let text = ROMAN_MONTHS[i as usize];
                    let text = if upper { text.to_string() } else { text.to_ascii_lowercase() };
                    if fill {
                        out.push_str(&text);
                    } else {
                        out.push_str(&format!("{text:<4}"));
                    }
                }
            }
            P::WeekOfMonth => number(i64::from((tm.mday - 1) / 7 + 1), 0),
            P::Julian => number(i64::from(date2j(tm.year, tm.mon, tm.mday)), 0),
            P::Fx => {}
        }
    }
    Ok(out)
}

/// The format of the second argument, or `None` when it is empty: `datetime_to_char_body` then gives null.
fn format(args: &[Value]) -> Result<Option<Vec<Node>>> {
    let format = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok((!format.is_empty()).then(|| parse(format)))
}

/// `timestamp_to_char`: null for an infinite timestamp.
fn timestamp_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Timestamp(ts)) = args.first() else { return Err(bad_value()) };
    let Some(format) = format(args)? else { return Ok(Value::Null) };
    if infinite(ts) {
        return Ok(Value::Null);
    }
    Ok(Value::Text(to_char(&format, false, &Fields::of(&Tm::of(ts)?))?))
}

/// `timestamptz_to_char`: the fields of the local time in the session time zone, with the offset and the abbreviation of the zone.
fn timestamptz_to_char(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::TimestampTz(ts)) = args.first() else { return Err(bad_value()) };
    let Some(format) = format(args)? else { return Ok(Value::Null) };
    if infinite(ts) {
        return Ok(Value::Null);
    }
    Tm::of(ts)?;
    let zone = call.session.zone()?;
    let unix = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
    let (gmtoff, abbrev) = zone.at(unix);
    let local = ts.checked_add(i64::from(gmtoff) * USECS_PER_SEC).ok_or_else(timestamp_range)?;
    let mut tm = Fields::of(&Tm::of(local)?);
    tm.gmtoff = gmtoff;
    tm.zone = Some(abbrev);
    Ok(Value::Text(to_char(&format, false, &tm)?))
}

/// `interval_to_char`: the fields of `interval2itm`. The day of the year is an approximate count of the days of the interval, with 30 days in a month.
#[allow(clippy::cast_possible_truncation)]
fn interval_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Interval(iv)) = args.first() else { return Err(bad_value()) };
    let Some(format) = format(args)? else { return Ok(Value::Null) };
    if iv == Interval::INFINITY || iv == Interval::NEGATIVE_INFINITY {
        return Ok(Value::Null);
    }
    let (year, mon) = (iv.month / 12, iv.month % 12);
    let tm = Fields {
        year,
        mon,
        mday: iv.day,
        hour: iv.time / USECS_PER_HOUR,
        min: (iv.time % USECS_PER_HOUR / USECS_PER_MINUTE) as i32,
        sec: (iv.time % USECS_PER_MINUTE / USECS_PER_SEC) as i32,
        fsec: (iv.time % USECS_PER_SEC) as i32,
        wday: 0,
        yday: (year * 12 + mon) * 30 + iv.day,
        gmtoff: 0,
        zone: None,
    };
    Ok(Value::Text(to_char(&format, true, &tm)?))
}

/// `ampm_strings` and `adbc_strings`, and their forms with dots. The search is in any case, so the forms in upper case are not necessary.
const AM_PM: [&str; 2] = ["am", "pm"];
const AM_PM_DOTS: [&str; 2] = ["a.m.", "p.m."];
const AD_BC: [&str; 2] = ["ad", "bc"];
const AD_BC_DOTS: [&str; 2] = ["a.d.", "b.c."];
/// The hint of the errors of a field of fixed width.
const FM_HINT: &str = "If your source string is not fixed-width, try using the \"FM\" modifier.";

/// `TmFromChar`: the fields of the input. Zero is a field that the input does not give.
#[derive(Default)]
struct FromChar {
    mode: Mode,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock_12_hour: bool,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
    /// The meaning of a `TZ` abbreviation, and the abbreviation.
    zone: Option<(ZoneName, String)>,
}

impl FromChar {
    /// `from_char_set_mode`.
    fn set_mode(&mut self, mode: Mode) -> Result<()> {
        if mode != Mode::None {
            if self.mode == Mode::None {
                self.mode = mode;
            } else if self.mode != mode {
                return Err(format_error("invalid combination of date conventions".to_string())
                    .with_hint(
                        "Do not mix Gregorian and ISO week date conventions in a formatting template.",
                    ));
            }
        }
        Ok(())
    }
}

fn format_error(message: String) -> Error {
    Error::new(SqlState::INVALID_DATETIME_FORMAT, message)
}

/// `from_char_set_int`: a field can get a second value only when it is the same.
fn set_int(dest: &mut i32, value: i32, name: &str) -> Result<()> {
    if *dest != 0 && *dest != value {
        return Err(format_error(format!(
            "conflicting values for \"{name}\" field in formatting string"
        ))
        .with_detail("This value contradicts a previous setting for the same field type."));
    }
    *dest = value;
    Ok(())
}

/// `strtol` in base 10: the value, true when the value is out of the range of a `long`, and the number of bytes. There are no bytes when there are no digits.
fn strtol(bytes: &[u8]) -> (i64, bool, usize) {
    let mut at = bytes.iter().take_while(|&&b| is_space(b)).count();
    let negative = bytes.get(at) == Some(&b'-');
    if matches!(bytes.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let start = at;
    let (mut value, mut overflow) = (0i64, false);
    while let Some(&b) = bytes.get(at).filter(|b| b.is_ascii_digit()) {
        let digit = i64::from(b - b'0');
        let next = value
            .checked_mul(10)
            .and_then(|v| if negative { v.checked_sub(digit) } else { v.checked_add(digit) });
        match next {
            Some(v) => value = v,
            None => {
                overflow = true;
                value = if negative { i64::MIN } else { i64::MAX };
            }
        }
        at += 1;
    }
    if at == start { (0, false, 0) } else { (value, overflow, at) }
}

/// The byte after the character at `at`, which is in the text.
fn next_char(input: &[u8], at: usize) -> usize {
    let len = match input[at] {
        b if b < 0x80 => 1,
        b if b & 0xe0 == 0xc0 => 2,
        b if b & 0xf0 == 0xe0 => 3,
        _ => 4,
    };
    (at + len).min(input.len())
}

/// `SKIP_THth`: the two characters of the ordinal suffix.
fn skip_ordinal(input: &[u8], s: &mut usize, ordinal: Option<Case>) {
    if ordinal.is_some() {
        for _ in 0..2 {
            if *s < input.len() {
                *s = next_char(input, *s);
            }
        }
    }
}

/// `is_next_separator`: true when the number of the node can end at the first byte that is not a digit.
fn is_next_separator(nodes: &[Node], at: usize) -> bool {
    if let Some(Node::Action { ordinal: Some(_), .. }) = nodes.get(at) {
        return true;
    }
    match nodes.get(at + 1) {
        None => true,
        Some(Node::Action { pattern, .. }) => !pattern.is_digit(),
        Some(Node::Char(c, _)) => !c.is_ascii_digit(),
    }
}

/// `from_char_parse_int_len`: a number of exactly `len` bytes, or of any length when `free` is true, after any spaces. Gives the number and the bytes that it uses, with the spaces.
fn parse_int(
    input: &[u8],
    s: &mut usize,
    len: usize,
    name: &str,
    free: bool,
) -> Result<(i32, usize)> {
    let init = *s;
    *s += input[init..].iter().take_while(|&&b| is_space(b)).count();
    let copy = &input[*s..input.len().min(*s + len)];
    let invalid = || format!("invalid value \"{}\" for \"{name}\"", String::from_utf8_lossy(copy));
    let (value, overflow) = if free {
        let (value, overflow, used) = strtol(&input[init..]);
        *s = init + used;
        (value, overflow)
    } else {
        let remain = input.len() - *s;
        if remain < len {
            return Err(format_error(format!(
                "source string too short for \"{name}\" formatting field"
            ))
            .with_detail(format!("Field requires {len} characters, but only {remain} remain."))
            .with_hint(FM_HINT));
        }
        let (value, overflow, used) = strtol(copy);
        if used > 0 && used < len {
            return Err(format_error(invalid())
                .with_detail(format!(
                    "Field requires {len} characters, but only {used} could be parsed."
                ))
                .with_hint(FM_HINT));
        }
        *s += used;
        (value, overflow)
    };
    if *s == init {
        return Err(format_error(invalid()).with_detail("Value must be an integer."));
    }
    let value = i32::try_from(value).ok().filter(|_| !overflow).ok_or_else(|| {
        Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("value for \"{name}\" in source string is out of range"),
        )
        .with_detail("Value must be in the range -2147483648 to 2147483647.")
    })?;
    Ok((value, *s - init))
}

/// `from_char_seq_search`: the index of the first name of the array that starts the input, in any case.
fn seq_search(input: &[u8], s: &mut usize, array: &[&str], name: &str) -> Result<i32> {
    let rest = &input[*s..];
    let found = array
        .iter()
        .position(|a| rest.len() >= a.len() && rest[..a.len()].eq_ignore_ascii_case(a.as_bytes()));
    let Some(i) = found else {
        // The error shows the input up to the first space.
        let end = rest
            .iter()
            .position(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c))
            .unwrap_or(rest.len());
        return Err(format_error(format!(
            "invalid value \"{}\" for \"{name}\"",
            String::from_utf8_lossy(&rest[..end])
        ))
        .with_detail("The given value did not match any of the allowed values for this field."));
    };
    *s += array[i].len();
    Ok(i as i32)
}

/// The sign of `TZH` and `OF`. The sign can be a space, and a minus sign that the spaces and the separators before the field skip is the sign.
fn zone_sign(input: &[u8], s: &mut usize, extra_skip: i32) -> i32 {
    match input.get(*s) {
        Some(b'+' | b' ') => {
            *s += 1;
            1
        }
        Some(b'-') => {
            *s += 1;
            -1
        }
        _ if extra_skip > 0 && *s > 0 && input[*s - 1] == b'-' => -1,
        _ => 1,
    }
}

/// `sscanf` with `"%d,%03d%n"`: the two numbers and the bytes that they use, or `None` when there are not two numbers.
#[allow(clippy::cast_possible_truncation)]
fn scan_year_comma(input: &[u8]) -> Option<(i32, i32, usize)> {
    let (millennia, _, used) = strtol(input);
    if used == 0 || input.get(used) != Some(&b',') {
        return None;
    }
    let mut at = used + 1;
    at += input[at..].iter().take_while(|&&b| is_space(b)).count();
    let (years, _, used) = strtol(&input[at..input.len().min(at + 3)]);
    (used > 0).then_some((millennia as i32, years as i32, at + used))
}

/// `adjust_partial_year_to_2020`: a year of fewer than 4 digits is the year nearest to 2020 with the same last digits.
fn adjust_partial_year(year: i32) -> i32 {
    match year {
        ..70 => year + 2000,
        70..100 => year + 1900,
        100..520 => year + 2000,
        520..1000 => year + 1000,
        _ => year,
    }
}

/// `DCH_from_char` outside the standard mode of the SQL/JSON path.
#[allow(clippy::too_many_lines)]
fn from_char(nodes: &[Node], text: &str, cx: &DateTimeInput<'_>) -> Result<FromChar> {
    let input = text.as_bytes();
    let mut out = FromChar::default();
    let mut fx = false;
    // The spaces and the separators of the input that the format does not give.
    let mut extra_skip = 0i32;
    let mut s = 0;
    for (at, node) in nodes.iter().enumerate() {
        if s >= input.len() {
            break;
        }
        // Outside the `FX` mode, the spaces at the start and before a field are skipped.
        let action = matches!(node, Node::Action { .. });
        if !fx && !matches!(node, Node::Action { pattern: P::Fx, .. }) && (action || at == 0) {
            while s < input.len() && is_space(input[s]) {
                s += 1;
                extra_skip += 1;
            }
        }
        let (name, pattern, fill, ordinal) = match *node {
            Node::Char(_, Kind::Space | Kind::Separator) => {
                // A space or a separator of the format matches one space or separator of the input, or nothing. In the `FX` mode it matches any character.
                if fx {
                    s = next_char(input, s);
                } else {
                    extra_skip -= 1;
                    if s < input.len() && (is_space(input[s]) || is_separator(u32::from(input[s])))
                    {
                        s += 1;
                        extra_skip += 1;
                    }
                }
                continue;
            }
            Node::Char(_, Kind::Text) => {
                // Other text matches any character. A character that the spaces skipped can be part of the next field.
                if !fx && extra_skip > 0 {
                    extra_skip -= 1;
                } else if s < input.len() {
                    s = next_char(input, s);
                }
                continue;
            }
            Node::Action { name, pattern, fill, ordinal, .. } => (name, pattern, fill, ordinal),
        };
        out.set_mode(pattern.mode())?;
        let free = fill || is_next_separator(nodes, at);
        let int = |s: &mut usize, len: usize| parse_int(input, s, len, name, free);
        match pattern {
            P::Fx => fx = true,
            P::Meridiem { dots, .. } => {
                let value =
                    seq_search(input, &mut s, if dots { &AM_PM_DOTS } else { &AM_PM }, name)?;
                set_int(&mut out.pm, value % 2, name)?;
                out.clock_12_hour = true;
            }
            P::Hour12 | P::Hour24 => {
                set_int(&mut out.hh, int(&mut s, 2)?.0, name)?;
                out.clock_12_hour |= matches!(pattern, P::Hour12);
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Minute => {
                set_int(&mut out.mi, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Second => {
                set_int(&mut out.ss, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Millisecond => {
                let (value, len) = int(&mut s, 3)?;
                set_int(&mut out.ms, value, name)?;
                // 25 and 250 are 0.25 seconds, and 025 is 0.025 seconds.
                out.ms = out.ms.wrapping_mul(match len {
                    1 => 100,
                    2 => 10,
                    _ => 1,
                });
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Fraction(_) | P::Microsecond => {
                let digits = match pattern {
                    P::Fraction(digits) => {
                        out.ff = digits as i32;
                        digits as usize
                    }
                    _ => 6,
                };
                let (value, len) = int(&mut s, digits)?;
                set_int(&mut out.us, value, name)?;
                out.us = out.us.wrapping_mul(10i32.pow(6 - len.clamp(1, 6) as u32));
                skip_ordinal(input, &mut s, ordinal);
            }
            P::SecondsOfDay => {
                set_int(&mut out.ssss, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Zone { .. } | P::Offset => {
                let abbrev = match pattern {
                    P::Zone { .. } => decode_timezone_abbrev_prefix(&text[s..], cx),
                    _ => None,
                };
                if let Some((len, zone)) = abbrev {
                    out.zone = Some((zone, text[s..s + len].to_string()));
                    // A zone abbreviation drops the `TZH` and the `TZM` before it.
                    out.tzsign = 0;
                    s += len;
                } else if matches!(pattern, P::Zone { .. })
                    && input.get(s).is_some_and(u8::is_ascii_alphabetic)
                {
                    return Err(format_error(format!(
                        "invalid value \"{}\" for \"{name}\"",
                        &text[s..]
                    ))
                    .with_detail("Time zone abbreviation is not recognized."));
                } else {
                    // `OF` is `TZH` or `TZH:TZM`, and a `TZ` that is not an abbreviation is `OF`.
                    out.tzsign = zone_sign(input, &mut s, extra_skip);
                    set_int(&mut out.tzh, int(&mut s, 2)?.0, name)?;
                    if input.get(s) == Some(&b':') {
                        s += 1;
                        set_int(&mut out.tzm, int(&mut s, 2)?.0, name)?;
                    }
                }
            }
            P::ZoneHour => {
                out.tzsign = zone_sign(input, &mut s, extra_skip);
                set_int(&mut out.tzh, int(&mut s, 2)?.0, name)?;
            }
            P::ZoneMinute => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                set_int(&mut out.tzm, int(&mut s, 2)?.0, name)?;
            }
            P::Era { dots, .. } => {
                let value =
                    seq_search(input, &mut s, if dots { &AD_BC_DOTS } else { &AD_BC }, name)?;
                set_int(&mut out.bc, value % 2, name)?;
            }
            P::MonthName(_) => {
                set_int(&mut out.mm, seq_search(input, &mut s, &MONTHS_FULL, name)? + 1, name)?;
            }
            P::MonthShort(_) => {
                set_int(&mut out.mm, seq_search(input, &mut s, &MONTHS, name)? + 1, name)?;
            }
            P::Month => {
                set_int(&mut out.mm, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::DayName(_) | P::DayShort(_) => {
                let names: &[&str] = if let P::DayName(_) = pattern { &DAYS } else { &DAYS_SHORT };
                set_int(&mut out.d, seq_search(input, &mut s, names, name)?, name)?;
                out.d += 1;
            }
            P::DayOfYear | P::IsoDayOfYear => {
                let len = if let P::DayOfYear = pattern { name.len() } else { 3 };
                set_int(&mut out.ddd, int(&mut s, len)?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::DayOfMonth => {
                set_int(&mut out.dd, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::DayOfWeek => {
                set_int(&mut out.d, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::IsoDayOfWeek => {
                set_int(&mut out.d, int(&mut s, 1)?.0, name)?;
                // Sunday is 1, as in the Gregorian convention.
                out.d += 1;
                if out.d > 7 {
                    out.d = 1;
                }
                skip_ordinal(input, &mut s, ordinal);
            }
            P::WeekOfYear | P::IsoWeek => {
                set_int(&mut out.ww, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Quarter => {
                // The quarter is read and not used.
                int(&mut s, name.len())?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Century => {
                set_int(&mut out.cc, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::YearComma => {
                let Some((millennia, years, used)) = scan_year_comma(&input[s..]) else {
                    return Err(format_error(format!(
                        "invalid value \"{}\" for \"Y,YYY\"",
                        &text[s..]
                    )));
                };
                let years = millennia
                    .checked_mul(1000)
                    .and_then(|m| years.checked_add(m))
                    .ok_or_else(|| {
                        Error::new(
                            SqlState::DATETIME_FIELD_OVERFLOW,
                            "value for \"Y,YYY\" in source string is out of range",
                        )
                    })?;
                set_int(&mut out.year, years, name)?;
                out.yysz = 4;
                s += used;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Year(digits) | P::IsoYear(digits) => {
                let (value, len) = int(&mut s, name.len())?;
                set_int(&mut out.year, value, name)?;
                if digits < 4 && len < 4 {
                    out.year = adjust_partial_year(out.year);
                }
                out.yysz = digits as i32;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Roman { .. } => {
                set_int(&mut out.mm, 12 - seq_search(input, &mut s, &ROMAN_MONTHS, name)?, name)?;
            }
            P::WeekOfMonth => {
                set_int(&mut out.w, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
            P::Julian => {
                set_int(&mut out.j, int(&mut s, name.len())?.0, name)?;
                skip_ordinal(input, &mut s, ordinal);
            }
        }
        // Outside the `FX` mode, the spaces after a field are skipped.
        if !fx {
            extra_skip = 0;
            while s < input.len() && is_space(input[s]) {
                s += 1;
                extra_skip += 1;
            }
        }
    }
    Ok(out)
}

/// `j2date` with the fields as `int`.
#[allow(clippy::cast_possible_wrap)]
fn julian_date(julian: i32) -> (i32, i32, i32) {
    let (year, mon, mday) = j2date(julian);
    (year, mon as i32, mday as i32)
}

/// `isoweek2j`: the Julian day of the Monday of an ISO week.
fn iso_week_julian(year: i32, week: i32) -> i32 {
    week.wrapping_sub(1).wrapping_mul(7).wrapping_add(iso_week_start(year))
}

/// `DetermineTimeZoneAbbrevOffset`: the offset west of UTC of an abbreviation whose meaning changed over time, at the local time of the fields.
fn abbrev_offset(tm: &Tm, abbrev: &str, zone: &dyn TimeZone) -> i32 {
    let (west, instant) = if valid_julian(tm.year, tm.mon) {
        let day = i64::from(date2j(tm.year, tm.mon, tm.mday) - UNIX_EPOCH_JDATE);
        local_offset(day * 86400 + tm.time / USECS_PER_SEC, zone)
    } else {
        (0, 0)
    };
    zone.abbrev_at(&abbrev.to_ascii_uppercase(), instant).map_or(west, |(east, _)| -east)
}

/// `do_to_timestamp` outside the standard mode: the fields of the input, the offset west of UTC that the input gives, and the precision of `FF1` to `FF6`.
#[allow(clippy::too_many_lines)]
fn do_to_timestamp(call: &Call<'_>, args: &[Value]) -> Result<(Tm, Option<i32>, i32)> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let format = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    let session = call.session.zone()?;
    let mut f = if format.is_empty() {
        FromChar::default()
    } else {
        let cx = datetime_input(call.session, session.as_ref());
        from_char(&parse(format), text, &cx)?
    };
    let overflow = || {
        Error::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date/time field value out of range: \"{text}\""),
        )
    };
    let (mut year, mut mon, mut mday, mut hour, mut min, mut sec) = (0, 1, 1, 0, 0, 0);
    // The fields of the mask of `ValidateDate`.
    let (mut has_year, mut has_mon, mut has_day) = (false, false, false);
    if f.ssss != 0 {
        (hour, min, sec) = (f.ssss / 3600, f.ssss % 3600 / 60, f.ssss % 60);
    }
    if f.ss != 0 {
        sec = f.ss;
    }
    if f.mi != 0 {
        min = f.mi;
    }
    if f.hh != 0 {
        hour = f.hh;
    }
    if f.clock_12_hour {
        if !(1..=12).contains(&hour) {
            return Err(format_error(format!("hour \"{hour}\" is invalid for the 12-hour clock"))
                .with_hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        if f.pm != 0 && hour < 12 {
            hour += 12;
        } else if f.pm == 0 && hour == 12 {
            hour = 0;
        }
    }
    if f.year != 0 {
        if f.cc != 0 && f.yysz <= 2 {
            // `CC` with `YY` or `Y`: the last two digits of a year in the century. The 21st century is 2001 to 2100, and the 6th century BC is 600 BC to 501 BC.
            if f.bc != 0 {
                f.cc = f.cc.wrapping_neg();
            }
            year = f.year % 100;
            if year != 0 {
                year = if f.cc >= 0 {
                    (f.cc - 1).checked_mul(100).and_then(|t| year.checked_add(t))
                } else {
                    (f.cc + 1)
                        .checked_mul(100)
                        .and_then(|t| t.checked_sub(year))
                        .and_then(|t| t.checked_add(1))
                }
                .ok_or_else(overflow)?;
            } else {
                year = f.cc.wrapping_mul(100).wrapping_add(i32::from(f.cc < 0));
            }
        } else {
            // A year of 4 digits does not use `CC`. Year 0 is 1 BC.
            year = f.year;
            if f.bc != 0 {
                year = year.wrapping_neg();
            }
            if year < 0 {
                year += 1;
            }
        }
        has_year = true;
    } else if f.cc != 0 {
        // The first year of the century.
        if f.bc != 0 {
            f.cc = f.cc.wrapping_neg();
        }
        let start = if f.cc >= 0 { f.cc - 1 } else { f.cc };
        year = start.checked_mul(100).and_then(|t| t.checked_add(1)).ok_or_else(overflow)?;
        has_year = true;
    }
    if f.j != 0 {
        (year, mon, mday) = julian_date(f.j);
        (has_year, has_mon, has_day) = (true, true, true);
    }
    let week_start =
        |n: i32| n.checked_sub(1).and_then(|t| t.checked_mul(7)).and_then(|t| t.checked_add(1));
    if f.ww != 0 {
        if f.mode == Mode::IsoWeek {
            // Without a day of the week, the date is the Monday of the week.
            let mut julian = iso_week_julian(year, f.ww);
            if f.d != 0 {
                julian = julian.wrapping_add(if f.d > 1 { f.d - 2 } else { 6 });
            }
            (year, mon, mday) = julian_date(julian);
            (has_year, has_mon, has_day) = (true, true, true);
        } else {
            f.ddd = week_start(f.ww).ok_or_else(overflow)?;
        }
    }
    if f.w != 0 {
        f.dd = week_start(f.w).ok_or_else(overflow)?;
    }
    if f.dd != 0 {
        mday = f.dd;
        has_day = true;
    }
    if f.mm != 0 {
        mon = f.mm;
        has_mon = true;
    }
    if f.ddd != 0 && (mon <= 1 || mday <= 1) {
        // The day of the year gives the month and the day when the input does not give them.
        if year == 0 && f.bc == 0 {
            return Err(format_error(
                "cannot calculate day of year without year information".to_string(),
            ));
        }
        if f.mode == Mode::IsoWeek {
            (year, mon, mday) = julian_date(iso_week_julian(year, 1) - 1 + f.ddd);
            (has_year, has_mon, has_day) = (true, true, true);
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[usize::from(days_of_month(year, 2) == 29)];
            let i = (1..=12).find(|&i| f.ddd <= y[i]).unwrap_or(13);
            if mon <= 1 {
                mon = i as i32;
            }
            if mday <= 1 {
                mday = f.ddd - y[i - 1];
            }
            (has_mon, has_day) = (true, true);
        }
    }
    let mut fsec = 0i32;
    if f.ms != 0 {
        fsec = f.ms.checked_mul(1000).ok_or_else(overflow)?;
    }
    if f.us != 0 {
        fsec = fsec.wrapping_add(f.us);
    }
    // `ValidateDate` with the year as it is.
    if (has_mon && !(1..=12).contains(&mon))
        || (has_day && !(1..=31).contains(&mday))
        || (has_year && has_mon && has_day && mday > days_of_month(year, mon))
    {
        return Err(overflow());
    }
    if !(0..24).contains(&hour)
        || !(0..60).contains(&min)
        || !(0..60).contains(&sec)
        || !(0..1_000_000).contains(&fsec)
    {
        return Err(overflow());
    }
    let time = ((i64::from(hour) * 60 + i64::from(min)) * 60 + i64::from(sec)) * USECS_PER_SEC
        + i64::from(fsec);
    let tm = Tm { year, mon, mday, time };
    let west = if f.tzsign != 0 {
        if !(0..=15).contains(&f.tzh) || !(0..60).contains(&f.tzm) {
            return Err(Error::new(
                SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                format!("time zone displacement out of range: \"{text}\""),
            ));
        }
        let east = (f.tzh * 60 + f.tzm) * 60;
        Some(if f.tzsign > 0 { -east } else { east })
    } else {
        match &f.zone {
            None => None,
            Some((ZoneName::Fixed(east), _)) => Some(-east),
            Some((ZoneName::Dynamic(Some(zone)) | ZoneName::Zone(zone), abbrev)) => {
                Some(abbrev_offset(&tm, abbrev, zone.as_ref()))
            }
            Some((ZoneName::Dynamic(None), abbrev)) => {
                Some(abbrev_offset(&tm, abbrev, session.as_ref()))
            }
        }
    };
    Ok((tm, west, f.ff))
}

/// `to_timestamp(text, text)`: the offset that the input gives, or else the offset of the zone of the session at the local time. `FF1` to `FF6` round the result to their precision.
fn to_timestamp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (tm, west, precision) = do_to_timestamp(call, args)?;
    let ts = match west {
        Some(west) => tm.timestamp_west(west)?,
        None => tm.timestamp(Some(call.session.zone()?.as_ref()))?,
    };
    if precision == 0 {
        return Ok(Value::TimestampTz(ts));
    }
    adjust_timestamp(ts, precision).map(Value::TimestampTz).map_err(type_error)
}

/// `to_date`: the date of the fields. The time and the zone of the input are checked and not used.
fn to_date(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (tm, _, _) = do_to_timestamp(call, args)?;
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let range = || {
        Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("date out of range: \"{text}\""))
    };
    if !valid_julian(tm.year, tm.mon) {
        return Err(range());
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !valid_date(i64::from(date)) {
        return Err(range());
    }
    Ok(Value::Date(date))
}

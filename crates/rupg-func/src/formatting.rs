//! `to_char` of a timestamp, a `timestamptz` and an interval: the date and time patterns of `formatting.c`. The `TM` prefix gives the names of `lc_time`, and rupg has only the C locale, so these are the English names.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Interval, UNIX_TO_POSTGRES_USECS, USECS_PER_SEC, Value, date2j};

use crate::cast::{infinite, timestamp_range};
use crate::datetime::Tm;
use crate::extract::{iso_week, iso_week_start};
use crate::{Call, Kernel, bad_value};

const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 3600 * USECS_PER_SEC;

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "timestamp_to_char" => timestamp_to_char,
        "timestamptz_to_char" => timestamptz_to_char,
        "interval_to_char" => interval_to_char,
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
    /// `FF1` to `FF6`, `MS` and `US`: the number of digits of the fraction of the second.
    Fraction(u32),
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
    ("MS", P::Fraction(3)),
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
    ("US", P::Fraction(6)),
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
    ("ms", P::Fraction(3)),
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
    ("us", P::Fraction(6)),
    ("ww", P::WeekOfYear),
    ("w", P::WeekOfMonth),
    ("y,yyy", P::YearComma),
    ("yyyy", P::Year(4)),
    ("yyy", P::Year(3)),
    ("yy", P::Year(2)),
    ("y", P::Year(1)),
];

/// A node of the parsed format: a pattern with its prefixes and its suffix, or a character that is copied.
enum Node {
    Action {
        pattern: Pattern,
        /// The `FM` prefix: no padding.
        fill: bool,
        /// The `TM` prefix: the names of the locale.
        locale: bool,
        /// The `TH` or `th` suffix: the ordinal suffix in upper case or lower case.
        ordinal: Option<Case>,
    },
    Char(char),
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
            nodes.push(Node::Action { pattern: *pattern, fill, locale, ordinal });
        } else if let Some(r) = rest.strip_prefix('"') {
            // A string in double quotes, where a backslash quotes the next character.
            let mut chars = r.chars();
            loop {
                match chars.next() {
                    None | Some('"') => break,
                    Some('\\') => {
                        let after = chars.as_str();
                        match chars.next() {
                            Some(c) => nodes.push(Node::Char(c)),
                            None => {
                                nodes.push(Node::Char('\\'));
                                chars = after.chars();
                            }
                        }
                    }
                    Some(c) => nodes.push(Node::Char(c)),
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
                nodes.push(Node::Char(c));
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
        let &Node::Action { pattern, fill, locale, ordinal: th } = node else {
            if let Node::Char(c) = node {
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
            P::Fraction(digits) => {
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

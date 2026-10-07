//! The check hooks of PostgreSQL for the parameters that have one and that rupg honors.
//!
//! A check hook reads the value after the type rules of [`Parameter::parse`] took it, and it can refuse the value or give its canonical form. `SET DateStyle = 'german'` stores `German, DMY` and `SET TimeZone = 'asia/tokyo'` stores `Asia/Tokyo`, so `SHOW` and `ParameterStatus` give the canonical form, as they do in PostgreSQL. The hooks are the ones in `src/backend/commands/variable.c` and `src/backend/catalog/namespace.c`.
//!
//! Lifted from `crates/rudb-common/src/guc/check.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_types::tz::zone_name;

use super::{Parameter, Setting};
use rupg_common::Error;
use rupg_common::SqlState;

/// The error of a check hook that refuses a value, with the detail that the hook gives.
fn invalid(parameter: &Parameter, value: &str, detail: Option<String>) -> Error {
    let error = Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{}\": \"{value}\"", parameter.name),
    );
    match detail {
        Some(detail) => error.with_detail(detail),
        None => error,
    }
}

/// Runs the check hook of a parameter on a value. `reset` is the reset value of `DateStyle`, which the key word `DEFAULT` in a `DateStyle` value stands for, and `current` is the value that the parts of a `DateStyle` value that are not given keep.
pub(super) fn check(
    parameter: &Parameter,
    value: Setting,
    current: &str,
    reset: &str,
) -> Result<Setting, Error> {
    match (parameter.name, value) {
        ("standard_conforming_strings", Setting::Bool(false)) => Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "non-standard string literals are not supported",
        )),
        ("DateStyle", Setting::String(text)) => datestyle(&text, current, reset)
            .map(Setting::String)
            .map_err(|detail| invalid(parameter, &text, Some(detail))),
        ("TimeZone" | "log_timezone", Setting::String(text)) => match zone(&text) {
            Ok((_, name)) => Ok(Setting::String(name)),
            Err(detail) => Err(invalid(parameter, &text, detail)),
        },
        ("client_encoding", Setting::String(text)) => {
            let Some(encoding) = encoding(&text) else {
                return Err(invalid(parameter, &text, None));
            };
            if encoding != "UTF8" && encoding != "SQL_ASCII" {
                // PostgreSQL converts between these and UTF8. rupg does not convert yet, so it gives the error of PostgreSQL for an encoding without a conversion.
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("invalid value for parameter \"{}\": \"{text}\"", parameter.name),
                )
                .with_detail(format!("Conversion between {encoding} and UTF8 is not supported.")));
            }
            // PostgreSQL keeps `UNICODE` as it is written, for old JDBC drivers.
            let name = if text == "UNICODE" { text } else { encoding.to_owned() };
            Ok(Setting::String(name))
        }
        ("application_name" | "cluster_name", Setting::String(text)) => {
            Ok(Setting::String(clean_ascii(&text)))
        }
        ("search_path", Setting::String(text)) => match split_identifiers(&text, ',') {
            Some(_) => Ok(Setting::String(text)),
            None => Err(invalid(parameter, &text, Some("List syntax is invalid.".to_owned()))),
        },
        (_, value) => Ok(value),
    }
}

/// `pg_clean_ascii`: each byte that is not printable ASCII becomes `\xNN`.
fn clean_ascii(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for byte in text.bytes() {
        if (32..=126).contains(&byte) {
            clean.push(char::from(byte));
        } else {
            clean.push_str(&format!("\\x{byte:02x}"));
        }
    }
    clean
}

/// `SplitIdentifierString`: a list of names with `separator` between them. A name in double quotes keeps its case and a doubled quote stands for one. A name without quotes is in lower case. `None` for a list with a syntax error.
pub fn split_identifiers(text: &str, separator: char) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut chars = text.trim_start_matches(char::is_whitespace).chars().peekable();
    if chars.peek().is_none() {
        return Some(names);
    }
    loop {
        let mut name = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next()? {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        name.push('"');
                    }
                    '"' => break,
                    c => name.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == separator || c.is_whitespace() {
                    break;
                }
                if c == '"' {
                    return None;
                }
                name.push(c.to_ascii_lowercase());
                chars.next();
            }
            if name.is_empty() {
                return None;
            }
        }
        names.push(name);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => return Some(names),
            Some(c) if c == separator => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some(_) => return None,
        }
    }
}

/// `check_datestyle`: one output style and one field order, in any order, with the parts that are not given kept from `current`. The detail of the error is the error.
fn datestyle(text: &str, current: &str, reset: &str) -> Result<String, String> {
    let (mut style, mut order) = split_datestyle(current);
    let (mut have_style, mut have_order, mut ok) = (false, false, true);
    let names = split_identifiers(text, ',').ok_or_else(|| "List syntax is invalid.".to_owned())?;
    for name in &names {
        let mut set_style = |new: &'static str, style: &mut &'static str| {
            if have_style && *style != new {
                ok = false;
            }
            *style = new;
            have_style = true;
        };
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "iso" => set_style("ISO", &mut style),
            "sql" => set_style("SQL", &mut style),
            "german" => {
                set_style("German", &mut style);
                if !have_order {
                    order = "DMY";
                }
            }
            _ if lower.starts_with("postgres") => set_style("Postgres", &mut style),
            "default" => {
                let (reset_style, reset_order) = split_datestyle(reset);
                if !have_style {
                    style = reset_style;
                }
                if !have_order {
                    order = reset_order;
                }
            }
            _ => {
                let new = match lower.as_str() {
                    "ymd" => "YMD",
                    "dmy" => "DMY",
                    "mdy" | "us" => "MDY",
                    _ if lower.starts_with("euro") => "DMY",
                    _ if lower.starts_with("noneuro") => "MDY",
                    _ => return Err(format!("Unrecognized key word: \"{name}\".")),
                };
                if have_order && order != new {
                    ok = false;
                }
                order = new;
                have_order = true;
            }
        }
    }
    if !ok {
        return Err("Conflicting \"DateStyle\" specifications.".to_owned());
    }
    Ok(format!("{style}, {order}"))
}

/// The style and the order of a canonical `DateStyle` value.
fn split_datestyle(text: &str) -> (&'static str, &'static str) {
    let (style, order) = text.split_once(", ").unwrap_or((text, "MDY"));
    let style = ["ISO", "SQL", "German", "Postgres"].into_iter().find(|s| *s == style);
    let order = ["YMD", "DMY", "MDY"].into_iter().find(|o| *o == order);
    (style.unwrap_or("ISO"), order.unwrap_or("MDY"))
}

/// A time zone of the `TimeZone` parameter.
#[derive(Clone, Debug, PartialEq)]
pub enum Zone {
    /// A zone of the time zone database, by its name in the data.
    Named(&'static str),
    /// A zone with one offset at all instants: a number of hours, an interval, or a POSIX zone without daylight saving time.
    Fixed {
        /// Seconds east of UTC.
        offset: i32,
        /// The abbreviation, which can be empty.
        abbrev: String,
    },
}

/// The largest number of hours in a POSIX offset, from `getsecs` in `localtime.c`.
const MAX_OFFSET_HOURS: i64 = 24 * 7 - 1;

/// `check_timezone` and `pg_tzset`: the zone and its canonical name. The error is the detail, if there is one.
pub fn zone(text: &str) -> Result<(Zone, String), Option<String>> {
    if text.len() >= 8 && text[..8].eq_ignore_ascii_case("interval") {
        let rest = text[8..].trim_start();
        let inner = rest
            .strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .filter(|r| !r.contains('\''));
        let seconds = inner.and_then(interval_seconds).ok_or(None)?;
        return offset_zone(-seconds);
    }
    if let Some((hours, rest)) = super::strtod(text)
        && rest.is_empty()
    {
        if !hours.is_finite() {
            return Err(Some("UTC timezone offset is out of range.".to_owned()));
        }
        // The cast saturates, and a value that is too large fails the range check.
        #[allow(clippy::cast_possible_truncation)]
        let seconds = (-hours * 3600.0) as i64;
        return offset_zone(seconds);
    }
    named_zone(text).ok_or(None)
}

/// `pg_tzset_offset`: a zone with an offset in seconds west of UTC, with a name such as `<+05:30>-05:30`.
fn offset_zone(west: i64) -> Result<(Zone, String), Option<String>> {
    let out_of_range = || Some("UTC timezone offset is out of range.".to_owned());
    let abs = west.unsigned_abs();
    if abs > (MAX_OFFSET_HOURS as u64) * 3600 + 3599 {
        return Err(out_of_range());
    }
    let mut text = format!("{:02}", abs / 3600);
    if !abs.is_multiple_of(3600) {
        text.push_str(&format!(":{:02}", abs % 3600 / 60));
        if !abs.is_multiple_of(60) {
            text.push_str(&format!(":{:02}", abs % 60));
        }
    }
    let name = if west > 0 { format!("<-{text}>+{text}") } else { format!("<+{text}>-{text}") };
    named_zone(&name).ok_or_else(out_of_range)
}

/// The seconds of an interval of the forms that `SET TIME ZONE INTERVAL` takes in practice: `[+-]h[:mm[:ss]]`, a number of hours, and the output of `interval_out` such as `05:30:00` and `-08:00:00`. `None` for an interval with days or months, or one that is not of these forms.
fn interval_seconds(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let mut parts = digits.split(':');
    let hours: i64 = parts.next()?.trim().parse().ok()?;
    let minutes: i64 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    let seconds: i64 = parts.next().map_or(Some(0), |s| s.parse().ok())?;
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    let total = hours * 3600 + minutes * 60 + seconds;
    Some(if negative { -total } else { total })
}

/// `pg_tzset`: a zone of the time zone database without regard to case, or a POSIX zone in upper case. `GMT` is always the POSIX zone.
fn named_zone(text: &str) -> Option<(Zone, String)> {
    let upper = text.to_ascii_uppercase();
    if upper != "GMT"
        && let Some(name) = zone_name(text)
    {
        return Some((Zone::Named(name), name.to_owned()));
    }
    if upper.starts_with(':') {
        return None;
    }
    let (abbrev, west) = posix(&upper)?;
    Some((Zone::Fixed { offset: -west, abbrev }, upper))
}

/// `tzparse` for a POSIX zone without daylight saving time: a name, alphabetic or in angle brackets, then an offset `[+-]h[:mm[:ss]]` west of UTC. The name and the offset in seconds.
fn posix(text: &str) -> Option<(String, i32)> {
    let (name, rest) = if let Some(rest) = text.strip_prefix('<') {
        let end = rest.find('>')?;
        (&rest[..end], &rest[end + 1..])
    } else {
        let end = text.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(text.len());
        text.split_at(end)
    };
    let (negative, rest) = match rest.as_bytes().first()? {
        b'-' => (true, &rest[1..]),
        b'+' => (false, &rest[1..]),
        _ => (false, rest),
    };
    let mut parts = rest.split(':');
    let number = |part: Option<&str>, max: i64| -> Option<i64> {
        let part = part?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok().filter(|n| *n <= max)
    };
    let hours = number(parts.next(), MAX_OFFSET_HOURS)?;
    let minutes = parts.next().map_or(Some(0), |m| number(Some(m), 59))?;
    let seconds = parts.next().map_or(Some(0), |s| number(Some(s), 59))?;
    if parts.next().is_some() {
        return None;
    }
    let total = i32::try_from(hours * 3600 + minutes * 60 + seconds).ok()?;
    Some((name.to_owned(), if negative { -total } else { total }))
}

/// `pg_char_to_encoding` for the client encodings: the canonical name of an encoding name or alias, without regard to case and to the characters that are not letters or digits.
pub fn encoding(name: &str) -> Option<&'static str> {
    const ALIASES: &[(&str, &str)] = &[
        ("abc", "WIN1258"),
        ("alt", "WIN866"),
        ("big5", "BIG5"),
        ("euccn", "EUC_CN"),
        ("eucjis2004", "EUC_JIS_2004"),
        ("eucjp", "EUC_JP"),
        ("euckr", "EUC_KR"),
        ("euctw", "EUC_TW"),
        ("gb18030", "GB18030"),
        ("gbk", "GBK"),
        ("iso88591", "LATIN1"),
        ("iso885910", "LATIN6"),
        ("iso885913", "LATIN7"),
        ("iso885914", "LATIN8"),
        ("iso885915", "LATIN9"),
        ("iso885916", "LATIN10"),
        ("iso88592", "LATIN2"),
        ("iso88593", "LATIN3"),
        ("iso88594", "LATIN4"),
        ("iso88595", "ISO_8859_5"),
        ("iso88596", "ISO_8859_6"),
        ("iso88597", "ISO_8859_7"),
        ("iso88598", "ISO_8859_8"),
        ("iso88599", "LATIN5"),
        ("johab", "JOHAB"),
        ("koi8", "KOI8R"),
        ("koi8r", "KOI8R"),
        ("koi8u", "KOI8U"),
        ("latin1", "LATIN1"),
        ("latin10", "LATIN10"),
        ("latin2", "LATIN2"),
        ("latin3", "LATIN3"),
        ("latin4", "LATIN4"),
        ("latin5", "LATIN5"),
        ("latin6", "LATIN6"),
        ("latin7", "LATIN7"),
        ("latin8", "LATIN8"),
        ("latin9", "LATIN9"),
        ("mskanji", "SJIS"),
        ("shiftjis", "SJIS"),
        ("shiftjis2004", "SHIFT_JIS_2004"),
        ("sjis", "SJIS"),
        ("sqlascii", "SQL_ASCII"),
        ("tcvn", "WIN1258"),
        ("tcvn5712", "WIN1258"),
        ("uhc", "UHC"),
        ("unicode", "UTF8"),
        ("utf8", "UTF8"),
        ("vscii", "WIN1258"),
        ("win", "WIN1251"),
        ("win1250", "WIN1250"),
        ("win1251", "WIN1251"),
        ("win1252", "WIN1252"),
        ("win1253", "WIN1253"),
        ("win1254", "WIN1254"),
        ("win1255", "WIN1255"),
        ("win1256", "WIN1256"),
        ("win1257", "WIN1257"),
        ("win1258", "WIN1258"),
        ("win866", "WIN866"),
        ("win874", "WIN874"),
        ("win932", "SJIS"),
        ("win936", "GBK"),
        ("win949", "UHC"),
        ("win950", "BIG5"),
        ("windows1250", "WIN1250"),
        ("windows1251", "WIN1251"),
        ("windows1252", "WIN1252"),
        ("windows1253", "WIN1253"),
        ("windows1254", "WIN1254"),
        ("windows1255", "WIN1255"),
        ("windows1256", "WIN1256"),
        ("windows1257", "WIN1257"),
        ("windows1258", "WIN1258"),
        ("windows866", "WIN866"),
        ("windows874", "WIN874"),
        ("windows932", "SJIS"),
        ("windows936", "GBK"),
        ("windows949", "UHC"),
        ("windows950", "BIG5"),
    ];
    let key: String =
        name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    ALIASES.iter().find(|(alias, _)| *alias == key).map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datestyle_keeps_the_part_that_is_not_given() {
        assert_eq!(datestyle("sql, dmy", "ISO, MDY", "ISO, MDY").unwrap(), "SQL, DMY");
        assert_eq!(datestyle("german", "ISO, MDY", "ISO, MDY").unwrap(), "German, DMY");
        assert_eq!(datestyle("iso", "German, DMY", "ISO, MDY").unwrap(), "ISO, DMY");
        assert_eq!(datestyle("ymd", "German, DMY", "ISO, MDY").unwrap(), "German, YMD");
        assert_eq!(datestyle("Postgres, US", "ISO, DMY", "ISO, MDY").unwrap(), "Postgres, MDY");
        assert_eq!(datestyle("default", "SQL, DMY", "ISO, MDY").unwrap(), "ISO, MDY");
        assert_eq!(
            datestyle("iso, sql", "ISO, MDY", "ISO, MDY").unwrap_err(),
            "Conflicting \"DateStyle\" specifications."
        );
        assert_eq!(
            datestyle("foo", "ISO, MDY", "ISO, MDY").unwrap_err(),
            "Unrecognized key word: \"foo\"."
        );
    }

    #[test]
    fn zones_have_the_canonical_names_of_postgres() {
        let name = |text: &str| zone(text).map(|(_, name)| name).ok();
        assert_eq!(name("asia/tokyo").as_deref(), Some("Asia/Tokyo"));
        assert_eq!(name("utc").as_deref(), Some("UTC"));
        assert_eq!(name("America/new_york").as_deref(), Some("America/New_York"));
        assert_eq!(name("-3").as_deref(), Some("<-03>+03"));
        assert_eq!(name("5.5").as_deref(), Some("<+05:30>-05:30"));
        assert_eq!(name("1e1").as_deref(), Some("<+10>-10"));
        assert_eq!(name("+03:00").as_deref(), Some("+03:00"));
        assert_eq!(name("abc+3").as_deref(), Some("ABC+3"));
        assert_eq!(name("<+0530>-5:30").as_deref(), Some("<+0530>-5:30"));
        assert_eq!(name("interval '-08:00'").as_deref(), Some("<-08>+08"));
        assert_eq!(name("Z"), None);
        assert_eq!(name("Nowhere/X"), None);
        assert_eq!(
            zone("XYZ+3").unwrap().0,
            Zone::Fixed { offset: -3 * 3600, abbrev: "XYZ".into() }
        );
        assert_eq!(zone("-3").unwrap().0, Zone::Fixed { offset: -3 * 3600, abbrev: "-03".into() });
        assert!(zone("200").is_err());
    }

    #[test]
    fn identifier_lists_follow_split_identifier_string() {
        assert_eq!(
            split_identifiers("\"My Schema\", public", ',').unwrap(),
            ["My Schema", "public"]
        );
        assert_eq!(split_identifiers("A,b", ',').unwrap(), ["a", "b"]);
        assert_eq!(split_identifiers("", ',').unwrap(), Vec::<String>::new());
        assert!(split_identifiers("a,,b", ',').is_none());
        assert_eq!(split_identifiers("\"\"", ',').unwrap(), vec![String::new()]);
        assert!(split_identifiers("a b", ',').is_none());
    }

    #[test]
    fn encodings_by_any_spelling() {
        assert_eq!(encoding("utf-8"), Some("UTF8"));
        assert_eq!(encoding("Unicode"), Some("UTF8"));
        assert_eq!(encoding("ISO-8859-1"), Some("LATIN1"));
        assert_eq!(encoding("bogus"), None);
        assert_eq!(clean_ascii("h\u{e9}llo"), "h\\xc3\\xa9llo");
    }
}

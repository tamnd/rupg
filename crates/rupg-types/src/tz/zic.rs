//! A port of the parts of `zic.c` that the PostgreSQL build runs on `tzdata.zi`: the reader of the rule, zone and link lines, `outzone`, `stringzone`, and the second pass of `writezone`, in the slim mode that is the default of `zic`.
//!
//! [`compile`] gives the data that `zic` writes into the 64-bit part of the file of a zone, and the TZ string at the end of the file. So the result is equal to the files that PostgreSQL installs in `share/timezone`. The code uses only `std`, so a check can compile this file alone and compare each zone with the files of a PostgreSQL install.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The `tzdata.zi` of the vendored PostgreSQL source.
const DATA: &str = include_str!("../../../../vendor/tzdata/src/timezone/data/tzdata.zi");

const MIN_TIME: i64 = i64::MIN;
const MAX_TIME: i64 = i64::MAX;
const ZIC_MIN: i64 = i64::MIN;
const ZIC_MAX: i64 = i64::MAX;
const SECS_PER_MIN: i64 = 60;
const SECS_PER_HOUR: i64 = 3600;
const SECS_PER_DAY: i64 = 86400;
const EPOCH_YEAR: i64 = 1970;
const EPOCH_WDAY: i64 = 4;
const YEARS_PER_REPEAT: i64 = 400;
const DAYS_PER_REPEAT: i64 = 146_097;
const LEN_MONTHS: [[i64; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];
const LEN_YEARS: [i64; 2] = [365, 366];
const MON_NAMES: [&str; 12] = [
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
const WDAY_NAMES: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const LASTS: [&str; 7] = [
    "last-Sunday",
    "last-Monday",
    "last-Tuesday",
    "last-Wednesday",
    "last-Thursday",
    "last-Friday",
    "last-Saturday",
];
const BEGIN_YEARS: [&str; 2] = ["minimum", "maximum"];
const END_YEARS: [&str; 3] = ["minimum", "maximum", "only"];
const LINE_CODES: [&str; 3] = ["Rule", "Zone", "Link"];

/// The data of a zone as `zic` writes it: the transitions, the type of each transition, the types as the offset east of UTC, the daylight saving flag and the index of the abbreviation, the abbreviations with a NUL after each, and the TZ string for the instants after the last transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Compiled {
    pub(super) ats: Vec<i64>,
    pub(super) types: Vec<u8>,
    pub(super) ttis: Vec<(i32, bool, u8)>,
    pub(super) chars: Vec<u8>,
    pub(super) footer: String,
}

/// The data of the zone or the link with this name, spelled as in the data, or `None` if there is no such name.
pub(super) fn compile(name: &str) -> Option<Compiled> {
    let data = data();
    let mut name = name;
    for _ in 0..data.links.len() + 1 {
        if let Some(lines) = data.zones.get(name) {
            return Some(outzone(&data.rules, lines));
        }
        name = data.links.get(name)?;
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DayCode {
    /// A day of the month.
    Dom,
    /// The first day of the week on or after a day of the month.
    DowGeq,
    /// The last day of the week on or before a day of the month.
    DowLeq,
}

/// A `Rule` line, or the time of the end of a zone line.
#[derive(Debug, Clone)]
struct Rule {
    loyear: i64,
    hiyear: i64,
    lowasnum: bool,
    hiwasnum: bool,
    month: usize,
    dycode: DayCode,
    dayofmonth: i64,
    wday: i64,
    tod: i64,
    todisstd: bool,
    todisut: bool,
    save: i64,
    isdst: bool,
    abbrvar: String,
}

/// A `Zone` line or a continuation line.
#[derive(Debug)]
struct ZoneLine {
    stdoff: i64,
    rule: String,
    /// The format, with `%s` for `%z`.
    format: String,
    /// `s` or `z` if the format has `%s` or `%z`, else 0.
    specifier: u8,
    until: Option<(Rule, i64)>,
    /// The rules of the line, in the sorted rules.
    rules: std::ops::Range<usize>,
    /// The saved time of a line without rules.
    save: i64,
    isdst: bool,
}

#[derive(Debug, Default)]
struct Data {
    rules: Vec<Rule>,
    zones: BTreeMap<&'static str, Vec<ZoneLine>>,
    links: BTreeMap<&'static str, &'static str>,
}

fn data() -> &'static Data {
    static DATA_LINES: OnceLock<Data> = OnceLock::new();
    DATA_LINES.get_or_init(|| read(DATA))
}

/// `infile` and `associate`: the rules sorted by their names, and the zone lines with their rules.
fn read(text: &'static str) -> Data {
    let mut named: Vec<(&str, Rule)> = Vec::new();
    let mut zones: Vec<(&str, Vec<ZoneLine>)> = Vec::new();
    let mut links = BTreeMap::new();
    let mut wantcont = false;
    for line in text.lines() {
        let fields: Vec<&str> = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_ascii_whitespace()
            .map(|f| if f == "-" { "" } else { f })
            .collect();
        if fields.is_empty() {
            continue;
        }
        if wantcont {
            wantcont = false;
            if let Some((_, lines)) = zones.last_mut()
                && (3..=7).contains(&fields.len())
                && let Some(zone) = zone_line(&fields)
            {
                wantcont = zone.until.is_some();
                lines.push(zone);
            }
            continue;
        }
        match byword(fields[0], &LINE_CODES) {
            Some(0) if fields.len() == 10 => {
                if let Some(rule) = rule_line(&fields) {
                    named.push((fields[1], rule));
                }
            }
            Some(1) if (5..=9).contains(&fields.len()) => {
                if let Some(zone) = zone_line(&fields[2..]) {
                    wantcont = zone.until.is_some();
                    zones.push((fields[1], vec![zone]));
                }
            }
            Some(2) if fields.len() == 3 => {
                links.insert(fields[2], fields[1]);
            }
            _ => {}
        }
    }
    named.sort_by(|a, b| a.0.cmp(b.0));
    let mut data = Data { rules: Vec::new(), zones: BTreeMap::new(), links };
    for (name, mut lines) in zones {
        for zone in &mut lines {
            let start = named.partition_point(|(n, _)| *n < zone.rule.as_str());
            let end = named.partition_point(|(n, _)| *n <= zone.rule.as_str());
            zone.rules = start..end;
            if start == end {
                let (save, isdst) = getsave(&zone.rule).unwrap_or((0, false));
                zone.save = save;
                zone.isdst = isdst;
            }
        }
        data.zones.insert(name, lines);
    }
    data.rules = named.into_iter().map(|(_, rule)| rule).collect();
    data
}

/// `inrule`: the fields `Rule NAME FROM TO - IN ON AT SAVE LETTER/S`.
fn rule_line(fields: &[&str]) -> Option<Rule> {
    let first = fields[1].bytes().next()?;
    if first.is_ascii_digit() || matches!(first, b'+' | b'-') {
        return None;
    }
    let (save, isdst) = getsave(fields[8])?;
    let mut rule = rulesub(fields[2], fields[3], fields[4], fields[5], fields[6], fields[7])?;
    rule.save = save;
    rule.isdst = isdst;
    rule.abbrvar = fields[9].to_string();
    Some(rule)
}

/// `inzsub`: the fields `STDOFF RULES FORMAT [UNTIL]` of a zone line.
fn zone_line(fields: &[&str]) -> Option<ZoneLine> {
    let stdoff = gethms(fields[0])?;
    let mut format = fields[2].to_string();
    let mut specifier = 0;
    if let Some(at) = format.find('%') {
        let after = &format[at + 1..];
        specifier = after.bytes().next().unwrap_or(0);
        if !matches!(specifier, b's' | b'z') || after.contains('%') || format.contains('/') {
            return None;
        }
        if specifier == b'z' {
            format.replace_range(at + 1..at + 2, "s");
        }
    }
    let until = if fields.len() > 3 {
        let get = |i: usize, default: &'static str| fields.get(i).copied().unwrap_or(default);
        let rule = rulesub(fields[3], "only", "", get(4, "Jan"), get(5, "1"), get(6, "0"))?;
        let time = rpytime(&rule, rule.loyear);
        Some((rule, time))
    } else {
        None
    };
    Some(ZoneLine {
        stdoff,
        rule: fields[1].to_string(),
        format,
        specifier,
        until,
        rules: 0..0,
        save: 0,
        isdst: false,
    })
}

/// `gethms`: `[-]h[:mm[:ss]]` in seconds. An empty field is 0.
fn gethms(text: &str) -> Option<i64> {
    if text.is_empty() {
        return Some(0);
    }
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text),
    };
    let mut parts = text.split(':');
    let number = |part: &str| -> Option<i64> {
        let digits = part.strip_prefix('+').unwrap_or(part);
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse().ok())?
    };
    let hh = number(parts.next()?)?;
    let mm = parts.next().map_or(Some(0), number)?;
    let ss = parts.next().map_or(Some(0), number)?;
    if parts.next().is_some() || mm >= 60 || ss > 60 {
        return None;
    }
    Some(sign * (hh * SECS_PER_HOUR + mm * SECS_PER_MIN + ss))
}

/// `getsave`: the saved time and whether it is daylight saving time. A last `d` or `s` sets the flag, else the flag is true if the time is not 0.
fn getsave(field: &str) -> Option<(i64, bool)> {
    let (field, dst) = match field.as_bytes().last() {
        Some(b'd') => (&field[..field.len() - 1], Some(true)),
        Some(b's') => (&field[..field.len() - 1], Some(false)),
        _ => (field, None),
    };
    let save = gethms(field)?;
    Some((save, dst.unwrap_or(save != 0)))
}

/// `rulesub`: the years, the month, the day and the time of a rule.
fn rulesub(
    loyear: &str,
    hiyear: &str,
    kind: &str,
    month: &str,
    day: &str,
    time: &str,
) -> Option<Rule> {
    let month = byword(month, &MON_NAMES)?;
    let (time, todisstd, todisut) = match time.as_bytes().last().map(u8::to_ascii_lowercase) {
        Some(b's') => (&time[..time.len() - 1], true, false),
        Some(b'w') => (&time[..time.len() - 1], false, false),
        Some(b'g' | b'u' | b'z') => (&time[..time.len() - 1], true, true),
        _ => (time, false, false),
    };
    let tod = gethms(time)?;
    let (loyear, lowasnum) = match byword(loyear, &BEGIN_YEARS) {
        Some(0) => (ZIC_MIN, false),
        Some(_) => (ZIC_MAX, false),
        None => (loyear.parse().ok()?, true),
    };
    let (hiyear, hiwasnum) = match byword(hiyear, &END_YEARS) {
        Some(0) => (ZIC_MIN, false),
        Some(1) => (ZIC_MAX, false),
        Some(_) => (loyear, false),
        None => (hiyear.parse().ok()?, true),
    };
    if loyear > hiyear || !kind.is_empty() {
        return None;
    }
    let mut rule = Rule {
        loyear,
        hiyear,
        lowasnum,
        hiwasnum,
        month,
        dycode: DayCode::Dom,
        dayofmonth: 0,
        wday: 0,
        tod,
        todisstd,
        todisut,
        save: 0,
        isdst: false,
        abbrvar: String::new(),
    };
    if let Some(wday) = lasts(day) {
        rule.dycode = DayCode::DowLeq;
        rule.wday = wday as i64;
        rule.dayofmonth = LEN_MONTHS[1][month];
        return Some(rule);
    }
    let number = if let Some((name, rest)) = day.split_once('<') {
        rule.dycode = DayCode::DowLeq;
        rule.wday = byword(name, &WDAY_NAMES)? as i64;
        rest.strip_prefix('=')?
    } else if let Some((name, rest)) = day.split_once('>') {
        rule.dycode = DayCode::DowGeq;
        rule.wday = byword(name, &WDAY_NAMES)? as i64;
        rest.strip_prefix('=')?
    } else {
        day
    };
    rule.dayofmonth = number.parse().ok()?;
    if rule.dayofmonth <= 0 || rule.dayofmonth > LEN_MONTHS[1][month] {
        return None;
    }
    Some(rule)
}

/// `byword` with the table `lasts`: `lastSun` and the other days, as the day of the week.
fn lasts(word: &str) -> Option<usize> {
    let bytes = word.as_bytes();
    if bytes.len() > 4 && bytes[..4].eq_ignore_ascii_case(b"last") && bytes[4] != b'-' {
        return byword(&word[4..], &WDAY_NAMES);
    }
    byword(word, &LASTS)
}

/// `byword`: the index of the word of the table that equals the word without regard to case, else of the one word that starts with it.
fn byword(word: &str, table: &[&str]) -> Option<usize> {
    if let Some(i) = table.iter().position(|w| w.eq_ignore_ascii_case(word)) {
        return Some(i);
    }
    let mut found = None;
    for (i, w) in table.iter().enumerate() {
        if w.len() >= word.len() && w.as_bytes()[..word.len()].eq_ignore_ascii_case(word.as_bytes())
        {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

fn is_leap(year: i64) -> usize {
    usize::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0))
}

/// `tadd`: a sum that stays at the smallest or the largest time.
fn tadd(t1: i64, t2: i64) -> i64 {
    if t1 < 0 {
        if t2 < MIN_TIME - t1 {
            return MIN_TIME;
        }
    } else if MAX_TIME - t1 < t2 {
        return MAX_TIME;
    }
    t1 + t2
}

/// `rpytime`: the time of a rule in a year, in seconds since 1970 in the local time that the rule uses.
fn rpytime(rule: &Rule, wanted: i64) -> i64 {
    if wanted == ZIC_MIN {
        return MIN_TIME;
    }
    if wanted == ZIC_MAX {
        return MAX_TIME;
    }
    let mut wantedy = wanted;
    let mut dayoff = 0;
    let mut y = EPOCH_YEAR;
    if y < wantedy {
        wantedy -= y;
        dayoff = (wantedy / YEARS_PER_REPEAT) * DAYS_PER_REPEAT;
        wantedy %= YEARS_PER_REPEAT;
        wantedy += y;
    } else if wantedy < 0 {
        dayoff = (wantedy / YEARS_PER_REPEAT) * DAYS_PER_REPEAT;
        wantedy %= YEARS_PER_REPEAT;
    }
    while wantedy != y {
        if wantedy > y {
            dayoff += LEN_YEARS[is_leap(y)];
            y += 1;
        } else {
            y -= 1;
            dayoff -= LEN_YEARS[is_leap(y)];
        }
    }
    dayoff += LEN_MONTHS[is_leap(y)][..rule.month].iter().sum::<i64>();
    let mut i = rule.dayofmonth;
    if rule.month == 1 && i == 29 && is_leap(y) == 0 && rule.dycode == DayCode::DowLeq {
        i -= 1;
    }
    i -= 1;
    dayoff += i;
    if rule.dycode != DayCode::Dom {
        let mut wday = if dayoff >= 0 {
            (EPOCH_WDAY + dayoff) % 7
        } else {
            let w = EPOCH_WDAY - (-dayoff) % 7;
            if w < 0 { w + 7 } else { w }
        };
        while wday != rule.wday {
            if rule.dycode == DayCode::DowGeq {
                dayoff += 1;
                wday = (wday + 1) % 7;
            } else {
                dayoff -= 1;
                wday = (wday + 6) % 7;
            }
        }
    }
    if dayoff < MIN_TIME / SECS_PER_DAY {
        return MIN_TIME;
    }
    if dayoff > MAX_TIME / SECS_PER_DAY {
        return MAX_TIME;
    }
    tadd(dayoff * SECS_PER_DAY, rule.tod)
}

fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// `abbroffset`: an offset as `+HH`, `+HHMM` or `+HHMMSS`, for `%z`.
fn abbroffset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    let (hours, minutes, seconds) = (offset / 3600, offset / 60 % 60, offset % 60);
    if hours >= 100 {
        return "%z".to_string();
    }
    let mut text = format!("{sign}{hours:02}");
    if minutes != 0 || seconds != 0 {
        text.push_str(&format!("{minutes:02}"));
        if seconds != 0 {
            text.push_str(&format!("{seconds:02}"));
        }
    }
    text
}

/// `doabbr`: the abbreviation of a zone line with the letters of a rule, in angle brackets if `quotes` and it is not all letters.
fn doabbr(zone: &ZoneLine, letters: Option<&str>, isdst: bool, save: i64, quotes: bool) -> String {
    let abbr = match zone.format.split_once('/') {
        None => {
            let offset;
            let letters = if zone.specifier == b'z' {
                offset = abbroffset(zone.stdoff + save);
                offset.as_str()
            } else {
                letters.unwrap_or("%s")
            };
            zone.format.replacen("%s", letters, 1)
        }
        Some((_, dst)) if isdst => dst.to_string(),
        Some((std, _)) => std.to_string(),
    };
    if !quotes || (!abbr.is_empty() && abbr.bytes().all(is_alpha)) {
        return abbr;
    }
    format!("<{abbr}>")
}

/// `stringoffset`: `[-]h[:mm[:ss]]`, or `None` from a week on.
fn stringoffset(offset: i64) -> Option<String> {
    let negative = offset < 0;
    let offset = offset.abs();
    let (hours, minutes, seconds) = (offset / 3600, offset / 60 % 60, offset % 60);
    if hours >= 24 * 7 {
        return None;
    }
    let mut text = if negative { format!("-{hours}") } else { hours.to_string() };
    if minutes != 0 || seconds != 0 {
        text.push_str(&format!(":{minutes:02}"));
        if seconds != 0 {
            text.push_str(&format!(":{seconds:02}"));
        }
    }
    Some(text)
}

/// `stringrule`: the POSIX form of a rule, and the year of the oldest `zic` that reads it, or `None`.
fn stringrule(rule: &Rule, save: i64, stdoff: i64) -> Option<(String, i32)> {
    let mut tod = rule.tod;
    let mut compat = 0;
    let mut text = match rule.dycode {
        DayCode::Dom => {
            if rule.dayofmonth == 29 && rule.month == 1 {
                return None;
            }
            let total: i64 = LEN_MONTHS[0][..rule.month].iter().sum();
            if rule.month <= 1 {
                format!("{}", total + rule.dayofmonth - 1)
            } else {
                format!("J{}", total + rule.dayofmonth)
            }
        }
        DayCode::DowGeq | DayCode::DowLeq => {
            let mut wday = rule.wday;
            let week = if rule.dycode == DayCode::DowGeq {
                let wdayoff = (rule.dayofmonth - 1) % 7;
                if wdayoff != 0 {
                    compat = 2013;
                }
                wday -= wdayoff;
                tod += wdayoff * SECS_PER_DAY;
                1 + (rule.dayofmonth - 1) / 7
            } else if rule.dayofmonth == LEN_MONTHS[1][rule.month] {
                5
            } else {
                let wdayoff = rule.dayofmonth % 7;
                if wdayoff != 0 {
                    compat = 2013;
                }
                wday -= wdayoff;
                tod += wdayoff * SECS_PER_DAY;
                rule.dayofmonth / 7
            };
            if wday < 0 {
                wday += 7;
            }
            format!("M{}.{week}.{wday}", rule.month + 1)
        }
    };
    if rule.todisut {
        tod += stdoff;
    }
    if rule.todisstd && !rule.isdst {
        tod += save;
    }
    if tod != 2 * SECS_PER_HOUR {
        text.push('/');
        text.push_str(&stringoffset(tod)?);
        if tod < 0 {
            compat = compat.max(2013);
        } else if SECS_PER_DAY <= tod {
            compat = compat.max(1994);
        }
    }
    Some((text, compat))
}

/// `rule_cmp`: the order of the rules by the last year, the month and the day.
fn rule_cmp(a: Option<&Rule>, b: Option<&Rule>) -> i64 {
    let (a, b) = match (a, b) {
        (None, b) => return -i64::from(b.is_some()),
        (_, None) => return 1,
        (Some(a), Some(b)) => (a, b),
    };
    if a.hiyear != b.hiyear {
        return if a.hiyear < b.hiyear { -1 } else { 1 };
    }
    if a.month != b.month {
        return a.month as i64 - b.month as i64;
    }
    a.dayofmonth - b.dayofmonth
}

/// `stringzone`: the TZ string of the last zone line, and the year of the oldest `zic` that reads it, or an empty string and -1 if no TZ string gives the future of the zone.
fn stringzone(rules: &[Rule], lines: &[ZoneLine]) -> (String, i32) {
    let fail = || (String::new(), -1);
    let zone = &lines[lines.len() - 1];
    let zone_rules = &rules[zone.rules.clone()];
    let mut stdrp: Option<&Rule> = None;
    let mut dstrp: Option<&Rule> = None;
    for rule in zone_rules {
        if rule.hiwasnum || rule.hiyear != ZIC_MAX {
            continue;
        }
        let slot = if rule.isdst { &mut dstrp } else { &mut stdrp };
        if slot.is_some() {
            return fail();
        }
        *slot = Some(rule);
    }
    let (stdr, dstr);
    if stdrp.is_none() && dstrp.is_none() {
        let mut stdabbrrp: Option<&Rule> = None;
        for rule in zone_rules {
            if !rule.isdst && rule_cmp(stdabbrrp, Some(rule)) < 0 {
                stdabbrrp = Some(rule);
            }
            if rule_cmp(stdrp, Some(rule)) < 0 {
                stdrp = Some(rule);
            }
        }
        if let Some(last) = stdrp
            && last.isdst
        {
            let base = Rule {
                loyear: 0,
                hiyear: 0,
                lowasnum: false,
                hiwasnum: false,
                month: 0,
                dycode: DayCode::Dom,
                dayofmonth: 1,
                wday: 0,
                tod: 0,
                todisstd: false,
                todisut: false,
                save: last.save,
                isdst: last.isdst,
                abbrvar: last.abbrvar.clone(),
            };
            stdr = Rule {
                month: 11,
                dayofmonth: 31,
                tod: SECS_PER_DAY + last.save,
                save: 0,
                isdst: false,
                abbrvar: stdabbrrp.map_or_else(String::new, |r| r.abbrvar.clone()),
                ..base.clone()
            };
            dstr = base;
            dstrp = Some(&dstr);
            stdrp = Some(&stdr);
        }
    }
    if stdrp.is_none() && (!zone_rules.is_empty() || zone.isdst) {
        return fail();
    }
    let abbrvar = stdrp.map_or("", |r| r.abbrvar.as_str());
    let mut text = doabbr(zone, Some(abbrvar), false, 0, true);
    let Some(offset) = stringoffset(-zone.stdoff) else { return fail() };
    text.push_str(&offset);
    let (Some(dst), Some(std)) = (dstrp, stdrp) else { return (text, 0) };
    text.push_str(&doabbr(zone, Some(&dst.abbrvar), dst.isdst, dst.save, true));
    if dst.save != SECS_PER_HOUR {
        let Some(offset) = stringoffset(-(zone.stdoff + dst.save)) else { return fail() };
        text.push_str(&offset);
    }
    let mut compat = 0;
    for rule in [dst, std] {
        let Some((part, c)) = stringrule(rule, dst.save, zone.stdoff) else { return fail() };
        text.push(',');
        text.push_str(&part);
        compat = compat.max(c);
    }
    (text, compat)
}

/// A transition while `outzone` runs.
#[derive(Debug, Clone, Copy)]
struct AtType {
    at: i64,
    dontmerge: bool,
    kind: usize,
}

/// The transitions, the types and the abbreviations while `outzone` runs.
#[derive(Debug, Default)]
struct Out {
    attypes: Vec<AtType>,
    /// The offset, the daylight saving flag and the index of the abbreviation of each type.
    types: Vec<(i64, bool, usize)>,
    chars: Vec<u8>,
}

/// The index of a string with a NUL after it at any byte of a buffer of strings, as `strcmp` finds it.
fn find_string(chars: &[u8], text: &[u8]) -> Option<usize> {
    (0..chars.len()).find(|&j| chars[j..].split(|&b| b == 0).next() == Some(text))
}

impl Out {
    fn addtt(&mut self, at: i64, kind: usize) {
        self.attypes.push(AtType { at, dontmerge: false, kind });
    }

    /// `addtype`, where the slim mode clears the standard and UT flags.
    fn addtype(&mut self, utoff: i64, abbr: &str, isdst: bool) -> usize {
        let j = match find_string(&self.chars, abbr.as_bytes()) {
            Some(j) => {
                if let Some(i) = self.types.iter().position(|t| *t == (utoff, isdst, j)) {
                    return i;
                }
                j
            }
            None => {
                let j = self.chars.len();
                self.chars.extend_from_slice(abbr.as_bytes());
                self.chars.push(0);
                j
            }
        };
        self.types.push((utoff, isdst, j));
        self.types.len() - 1
    }
}

/// `updateminmax`.
fn widen(range: &mut (i64, i64), year: i64) {
    range.0 = range.0.min(year);
    range.1 = range.1.max(year);
}

/// `outzone` and `writezone`: the data of a zone from its lines.
#[allow(clippy::too_many_lines)]
fn outzone(rules: &[Rule], lines: &[ZoneLine]) -> Compiled {
    let y2038_boundary: i64 = 1 << 31;
    let mut out = Out::default();
    let mut prodstic = lines.len() == 1;
    let mut years = (EPOCH_YEAR, EPOCH_YEAR);
    for (i, zone) in lines.iter().enumerate() {
        if i < lines.len() - 1
            && let Some((until, _)) = &zone.until
        {
            widen(&mut years, until.loyear);
        }
        for rule in &rules[zone.rules.clone()] {
            if rule.lowasnum {
                widen(&mut years, rule.loyear);
            }
            if rule.hiwasnum {
                widen(&mut years, rule.hiyear);
            }
            if rule.lowasnum || rule.hiwasnum {
                prodstic = false;
            }
        }
    }
    let (footer, compat) = stringzone(rules, lines);
    let do_extend = compat < 0;
    let (mut min_year, mut max_year) = years;
    if do_extend {
        let observed = YEARS_PER_REPEAT + 2;
        min_year = if min_year >= ZIC_MIN + observed { min_year - observed } else { ZIC_MIN };
        max_year = if max_year <= ZIC_MAX - observed { max_year + observed } else { ZIC_MAX };
        if prodstic {
            min_year = 1900;
            max_year = min_year + observed;
        }
    }
    let max_year0 = max_year;

    let mut defaulttype: Option<usize> = None;
    let mut lastatmax: Option<usize> = None;
    let mut starttime = 0;
    for (i, zone) in lines.iter().enumerate() {
        let mut prevrp: Option<&Rule> = None;
        let mut save = 0;
        let prev_until =
            if i > 0 { lines[i - 1].until.as_ref().map_or(MIN_TIME, |u| u.1) } else { MIN_TIME };
        let mut usestart = i > 0 && prev_until > MIN_TIME;
        let useuntil = i < lines.len() - 1;
        let until = zone.until.as_ref();
        if useuntil && until.is_some_and(|u| u.1 == MIN_TIME) {
            continue;
        }
        let stdoff = zone.stdoff;
        let mut startbuf = String::new();
        let mut startoff = zone.stdoff;
        let zone_rules = &rules[zone.rules.clone()];
        if zone_rules.is_empty() {
            save = zone.save;
            startbuf = doabbr(zone, None, zone.isdst, save, false);
            let kind = out.addtype(zone.stdoff + save, &startbuf, zone.isdst);
            if usestart {
                out.addtt(starttime, kind);
                usestart = false;
            } else {
                defaulttype = Some(kind);
            }
        } else {
            let mut todo = vec![false; zone_rules.len()];
            let mut temp = vec![0; zone_rules.len()];
            let mut year = min_year;
            while year <= max_year {
                if useuntil && until.is_some_and(|u| year > u.0.hiyear) {
                    break;
                }
                for (j, rule) in zone_rules.iter().enumerate() {
                    todo[j] = year >= rule.loyear && year <= rule.hiyear;
                    if todo[j] {
                        temp[j] = rpytime(rule, year);
                        todo[j] = temp[j] < y2038_boundary || year <= max_year0;
                    }
                }
                loop {
                    let mut untiltime = 0;
                    if useuntil && let Some((rule, time)) = until {
                        untiltime = *time;
                        if !rule.todisut {
                            untiltime = tadd(untiltime, -stdoff);
                        }
                        if !rule.todisstd {
                            untiltime = tadd(untiltime, -save);
                        }
                    }
                    let mut k: Option<usize> = None;
                    let mut ktime = 0;
                    for (j, rule) in zone_rules.iter().enumerate() {
                        if !todo[j] {
                            continue;
                        }
                        let mut offset = if rule.todisut { 0 } else { stdoff };
                        if !rule.todisstd {
                            offset += save;
                        }
                        let jtime = temp[j];
                        if jtime == MIN_TIME || jtime == MAX_TIME {
                            continue;
                        }
                        let jtime = tadd(jtime, -offset);
                        if k.is_none() || jtime < ktime {
                            k = Some(j);
                            ktime = jtime;
                        }
                    }
                    let Some(k) = k else { break };
                    let rule = &zone_rules[k];
                    todo[k] = false;
                    if useuntil && ktime >= untiltime {
                        break;
                    }
                    save = rule.save;
                    if usestart && ktime == starttime {
                        usestart = false;
                    }
                    if usestart {
                        if ktime < starttime {
                            startoff = zone.stdoff + save;
                            startbuf =
                                doabbr(zone, Some(&rule.abbrvar), rule.isdst, rule.save, false);
                            continue;
                        }
                        if startbuf.is_empty() && startoff == zone.stdoff + save {
                            startbuf =
                                doabbr(zone, Some(&rule.abbrvar), rule.isdst, rule.save, false);
                        }
                    }
                    let ab = doabbr(zone, Some(&rule.abbrvar), rule.isdst, rule.save, false);
                    let offset = zone.stdoff + rule.save;
                    if !useuntil
                        && !do_extend
                        && prevrp.is_some_and(|p| p.hiyear == ZIC_MAX)
                        && rule.hiyear == ZIC_MAX
                    {
                        break;
                    }
                    let kind = out.addtype(offset, &ab, rule.isdst);
                    if defaulttype.is_none() && !rule.isdst {
                        defaulttype = Some(kind);
                    }
                    if rule.hiyear == ZIC_MAX
                        && !lastatmax.is_some_and(|l| ktime < out.attypes[l].at)
                    {
                        lastatmax = Some(out.attypes.len());
                    }
                    out.addtt(ktime, kind);
                    prevrp = Some(rule);
                }
                year += 1;
            }
        }
        if usestart {
            if startbuf.is_empty() && !zone.format.contains('%') && !zone.format.contains('/') {
                startbuf.clone_from(&zone.format);
            }
            if !startbuf.is_empty() {
                let isdst = startoff != zone.stdoff;
                let kind = out.addtype(startoff, &startbuf, isdst);
                if defaulttype.is_none() && !isdst {
                    defaulttype = Some(kind);
                }
                out.addtt(starttime, kind);
            }
        }
        if useuntil && let Some((rule, time)) = until {
            starttime = *time;
            if !rule.todisstd {
                starttime = tadd(starttime, -save);
            }
            if !rule.todisut {
                starttime = tadd(starttime, -stdoff);
            }
        }
    }
    let defaulttype = defaulttype.unwrap_or(0);
    if let Some(l) = lastatmax {
        out.attypes[l].dontmerge = true;
    }
    if do_extend {
        let jan1 = |year| {
            let rule = Rule {
                loyear: 0,
                hiyear: 0,
                lowasnum: false,
                hiwasnum: false,
                month: 0,
                dycode: DayCode::Dom,
                dayofmonth: 1,
                wday: 0,
                tod: 0,
                todisstd: false,
                todisut: false,
                save: 0,
                isdst: false,
                abbrvar: String::new(),
            };
            rpytime(&rule, year)
        };
        let mut lastat: Option<AtType> = out.attypes.first().copied();
        for at in out.attypes.iter().skip(1) {
            if lastat.is_some_and(|l| at.at > l.at) {
                lastat = Some(*at);
            }
        }
        if lastat.is_none_or(|l| l.at < jan1(max_year - 1)) {
            out.addtt(jan1(max_year + 1), lastat.map_or(defaulttype, |l| l.kind));
            if let Some(last) = out.attypes.last_mut() {
                last.dontmerge = true;
            }
        }
    }
    writezone(out, footer, defaulttype)
}

/// The second pass of `writezone`: the sorted and merged transitions, and the used types with the default type first.
fn writezone(mut out: Out, footer: String, defaulttype: usize) -> Compiled {
    out.attypes.sort_by_key(|a| a.at);
    let utoff = |kind: usize| out.types[kind].0;
    let mut kept: Vec<AtType> = Vec::new();
    for from in out.attypes.iter().copied() {
        let toi = kept.len();
        if toi != 0 {
            let before = if toi == 1 { 0 } else { kept[toi - 2].kind };
            if from.at + utoff(kept[toi - 1].kind) <= kept[toi - 1].at + utoff(before) {
                kept[toi - 1].kind = from.kind;
                continue;
            }
        }
        if toi == 0 || from.dontmerge || out.types[kept[toi - 1].kind] != out.types[from.kind] {
            kept.push(from);
        }
    }

    let typecnt = out.types.len();
    let mut omit = vec![true; typecnt];
    omit[defaulttype] = false;
    for at in &kept {
        omit[at.kind] = false;
    }
    let old0 = omit.iter().position(|o| !o).unwrap_or(0);
    let swap = |i: usize| {
        if i == old0 {
            defaulttype
        } else if i == defaulttype {
            old0
        } else {
            i
        }
    };
    let mut typemap = vec![0u8; typecnt];
    let mut count = 0u8;
    for i in old0..typecnt {
        if !omit[i] {
            typemap[swap(i)] = count;
            count = count.wrapping_add(1);
        }
    }
    let mut indmap: BTreeMap<usize, usize> = BTreeMap::new();
    let mut chars: Vec<u8> = Vec::new();
    for (i, &omitted) in omit.iter().enumerate().take(typecnt).skip(old0) {
        let desigidx = out.types[i].2;
        if omitted || indmap.contains_key(&desigidx) {
            continue;
        }
        let abbr = out.chars[desigidx..].split(|&b| b == 0).next().unwrap_or(&[]);
        let j = find_string(&chars, abbr).unwrap_or_else(|| {
            let j = chars.len();
            chars.extend_from_slice(abbr);
            chars.push(0);
            j
        });
        indmap.insert(desigidx, j);
    }
    let mut ttis = Vec::new();
    for i in old0..typecnt {
        let h = swap(i);
        if !omit[h] {
            let (utoff, isdst, desigidx) = out.types[h];
            // `addtype` keeps the offsets in the range of i32, and `zic` keeps the index of an abbreviation below 50.
            #[allow(clippy::cast_possible_truncation)]
            ttis.push((utoff as i32, isdst, indmap[&desigidx] as u8));
        }
    }
    Compiled {
        ats: kept.iter().map(|a| a.at).collect(),
        types: kept.iter().map(|a| typemap[a.kind]).collect(),
        ttis,
        chars,
        footer,
    }
}

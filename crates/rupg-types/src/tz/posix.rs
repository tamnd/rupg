//! A port of `tzparse` in `localtime.c`: the rules of a POSIX time zone string such as `EST5EDT,M3.2.0,M11.1.0`, as PostgreSQL reads them from the end of a zone file and from a `TimeZone` value that is not the name of a zone.

use super::{State, TZ_MAX_TIMES, Tti, YEARS_PER_REPEAT};

const SECS_PER_HOUR: i64 = 3600;
const SECS_PER_DAY: i64 = 86400;
const EPOCH_YEAR: i64 = 1970;
const MON_LENGTHS: [[i64; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];
const YEAR_LENGTHS: [i64; 2] = [365, 366];
/// `TZDEFRULESTRING`: the rules of a zone string with a daylight saving name and no rules.
const DEFAULT_RULES: &[u8] = b",M3.2.0,M11.1.0";

/// The kind of the date of a rule.
#[derive(Debug, Clone, Copy)]
enum Day {
    /// `Jn`: the day of the year from 1, without February 29.
    Julian(i64),
    /// `n`: the day of the year from 0.
    OfYear(i64),
    /// `Mm.w.d`: the day `d` of the week `w` of the month `m`.
    OfWeek { month: i64, week: i64, day: i64 },
}

/// A rule of a zone string: the date and the time of the change, in seconds after midnight.
#[derive(Debug, Clone, Copy)]
struct Rule {
    day: Day,
    time: i64,
}

fn is_leap(year: i64) -> usize {
    usize::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0))
}

/// The byte at an index, or 0 at the end, as the C string has it.
fn at(text: &[u8], i: usize) -> u8 {
    text.get(i).copied().unwrap_or(0)
}

/// `getzname`: the end of a name that is not in angle brackets.
fn getzname(text: &[u8], mut i: usize) -> usize {
    while !matches!(at(text, i), 0 | b'0'..=b'9' | b',' | b'-' | b'+') {
        i += 1;
    }
    i
}

/// A name in angle brackets or not: the name and the index after it.
fn getname(text: &[u8], i: usize) -> Option<(&[u8], usize)> {
    if at(text, i) == b'<' {
        let start = i + 1;
        let mut end = start;
        while !matches!(at(text, end), 0 | b'>') {
            end += 1;
        }
        if at(text, end) != b'>' {
            return None;
        }
        return Some((&text[start..end], end + 1));
    }
    let end = getzname(text, i);
    Some((&text[i..end], end))
}

/// `getnum`: a number from `min` to `max`, and the index after it.
fn getnum(text: &[u8], mut i: usize, min: i64, max: i64) -> Option<(usize, i64)> {
    if !at(text, i).is_ascii_digit() {
        return None;
    }
    let mut num = 0;
    while at(text, i).is_ascii_digit() {
        num = num * 10 + i64::from(at(text, i) - b'0');
        if num > max {
            return None;
        }
        i += 1;
    }
    (num >= min).then_some((i, num))
}

/// `getsecs`: `hh[:mm[:ss]]` in seconds, with up to 167 hours.
fn getsecs(text: &[u8], i: usize) -> Option<(usize, i64)> {
    let (mut i, hours) = getnum(text, i, 0, 24 * 7 - 1)?;
    let mut secs = hours * SECS_PER_HOUR;
    if at(text, i) == b':' {
        let minutes;
        (i, minutes) = getnum(text, i + 1, 0, 59)?;
        secs += minutes * 60;
        if at(text, i) == b':' {
            let seconds;
            (i, seconds) = getnum(text, i + 1, 0, 60)?;
            secs += seconds;
        }
    }
    Some((i, secs))
}

/// `getoffset`: `[+-]hh[:mm[:ss]]` in seconds.
fn getoffset(text: &[u8], i: usize) -> Option<(usize, i64)> {
    let (negative, i) = match at(text, i) {
        b'-' => (true, i + 1),
        b'+' => (false, i + 1),
        _ => (false, i),
    };
    let (i, secs) = getsecs(text, i)?;
    Some((i, if negative { -secs } else { secs }))
}

/// `getrule`: `date[/time]`, where the time is 2:00 if there is none.
fn getrule(text: &[u8], i: usize) -> Option<(usize, Rule)> {
    let (mut i, day) = match at(text, i) {
        b'J' => {
            let (i, day) = getnum(text, i + 1, 1, 365)?;
            (i, Day::Julian(day))
        }
        b'M' => {
            let (i, month) = getnum(text, i + 1, 1, 12)?;
            if at(text, i) != b'.' {
                return None;
            }
            let (i, week) = getnum(text, i + 1, 1, 5)?;
            if at(text, i) != b'.' {
                return None;
            }
            let (i, day) = getnum(text, i + 1, 0, 6)?;
            (i, Day::OfWeek { month, week, day })
        }
        b'0'..=b'9' => {
            let (i, day) = getnum(text, i, 0, 365)?;
            (i, Day::OfYear(day))
        }
        _ => return None,
    };
    let mut time = 2 * SECS_PER_HOUR;
    if at(text, i) == b'/' {
        (i, time) = getoffset(text, i + 1)?;
    }
    Some((i, Rule { day, time }))
}

/// `transtime`: the time of a rule in a year, in seconds after the start of the year in UTC, for a zone with this offset west of UTC.
fn transtime(year: i64, rule: &Rule, offset: i64) -> i64 {
    let leap = is_leap(year);
    let value = match rule.day {
        Day::Julian(day) => {
            let mut value = (day - 1) * SECS_PER_DAY;
            if leap == 1 && day >= 60 {
                value += SECS_PER_DAY;
            }
            value
        }
        Day::OfYear(day) => day * SECS_PER_DAY,
        Day::OfWeek { month, week, day } => {
            // Zeller's congruence gives the day of the week of the first day of the month.
            let m1 = (month + 9) % 12 + 1;
            let yy0 = if month <= 2 { year - 1 } else { year };
            let (yy1, yy2) = (yy0 / 100, yy0 % 100);
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = day - dow;
            if d < 0 {
                d += 7;
            }
            let month = month as usize - 1;
            for _ in 1..week {
                if d + 7 >= MON_LENGTHS[leap][month] {
                    break;
                }
                d += 7;
            }
            d * SECS_PER_DAY + MON_LENGTHS[leap][..month].iter().sum::<i64>() * SECS_PER_DAY
        }
    };
    value + rule.time + offset
}

/// `tzparse`: the rules of a zone string, or `None` if it is not one. With `lastditch` the whole text is the name of a zone at UTC, as for `GMT`.
pub(super) fn tzparse(text: &[u8], lastditch: bool) -> Option<State> {
    let (stdname, stdoffset, mut i) = if lastditch {
        (text, 0, text.len())
    } else {
        let (name, i) = getname(text, 0)?;
        // PostgreSQL takes an empty standard name, but not a name without an offset.
        if at(text, i) == 0 {
            return None;
        }
        let (i, offset) = getoffset(text, i)?;
        (name, offset, i)
    };
    let mut state = State::default();
    state.chars.extend_from_slice(stdname);
    state.chars.push(0);
    // The offsets of a zone string are at most 167 hours.
    #[allow(clippy::cast_possible_truncation)]
    let tti =
        |offset: i64, isdst: bool, desigidx: usize| Tti { utoff: -offset as i32, isdst, desigidx };
    if at(text, i) == 0 {
        state.ttis.push(tti(stdoffset, false, 0));
        return Some(state);
    }
    let (dstname, next) = getname(text, i)?;
    i = next;
    if dstname.is_empty() {
        return None;
    }
    state.chars.extend_from_slice(dstname);
    state.chars.push(0);
    let mut dstoffset = stdoffset - SECS_PER_HOUR;
    if !matches!(at(text, i), 0 | b',' | b';') {
        (i, dstoffset) = getoffset(text, i)?;
    }
    let rules = if at(text, i) == 0 { DEFAULT_RULES } else { &text[i..] };
    if !matches!(at(rules, 0), b',' | b';') {
        return None;
    }
    let (i, start) = getrule(rules, 1)?;
    if at(rules, i) != b',' {
        return None;
    }
    let (i, end) = getrule(rules, i + 1)?;
    if at(rules, i) != 0 {
        return None;
    }
    state.ttis = vec![tti(stdoffset, false, 0), tti(dstoffset, true, stdname.len() + 1)];
    let mut janfirst: i64 = 0;
    let mut yearbeg = EPOCH_YEAR;
    loop {
        janfirst -= YEAR_LENGTHS[is_leap(yearbeg - 1)] * SECS_PER_DAY;
        yearbeg -= 1;
        if EPOCH_YEAR - YEARS_PER_REPEAT / 2 >= yearbeg {
            break;
        }
    }
    let mut yearlim = yearbeg + YEARS_PER_REPEAT + 1;
    let mut year = yearbeg;
    while year < yearlim {
        let mut starttime = transtime(year, &start, stdoffset);
        let mut endtime = transtime(year, &end, dstoffset);
        let yearsecs = YEAR_LENGTHS[is_leap(year)] * SECS_PER_DAY;
        let reversed = endtime < starttime;
        if reversed {
            std::mem::swap(&mut starttime, &mut endtime);
        }
        if reversed
            || (starttime < endtime && endtime - starttime < yearsecs + (stdoffset - dstoffset))
        {
            if TZ_MAX_TIMES - 2 < state.ats.len() {
                break;
            }
            state.ats.push(janfirst + starttime);
            state.types.push(u8::from(!reversed));
            state.ats.push(janfirst + endtime);
            state.types.push(u8::from(reversed));
            yearlim = year + YEARS_PER_REPEAT + 1;
        }
        janfirst += yearsecs;
        year += 1;
    }
    if state.ats.is_empty() {
        // Daylight saving time all year.
        state.ttis.remove(0);
    } else if YEARS_PER_REPEAT < year - yearbeg {
        state.goback = true;
        state.goahead = true;
    }
    Some(state)
}

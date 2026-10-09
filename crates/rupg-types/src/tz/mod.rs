//! The time zones of the vendored IANA data (spec/07 section 7.9).
//!
//! The names and the rules come from `vendor/tzdata`, so `TimeZone` takes the same zones as the PostgreSQL pin and gives the same offsets. The build of PostgreSQL compiles `tzdata.zi` with its copy of `zic` and installs a file for each zone. Here `zic` is a port that runs when a zone is first loaded, and its result is equal to those files, byte for byte. The reader of that result is a port of `localtime.c`, and [`load`] is a port of `pg_tzset`, which also takes a POSIX zone string such as `EST5EDT` or `<+05>-05`.

mod posix;
mod zic;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::datetime::{AbbrevMeaning, TimeZone, ZoneLookup};
use crate::generated::tznames::ZONE_NAMES;

/// The limits of `tzfile.h`.
const TZ_MAX_TIMES: usize = 2000;
const TZ_MAX_TYPES: usize = 256;
const TZ_MAX_CHARS: usize = 50;
/// `TZ_STRLEN_MAX`: the longest name that `pg_tzset` takes.
const TZ_STRLEN_MAX: usize = 255;
const YEARS_PER_REPEAT: i64 = 400;
const AVG_SECS_PER_YEAR: i64 = 31_556_952;
const SECS_PER_REPEAT: i64 = YEARS_PER_REPEAT * AVG_SECS_PER_YEAR;
/// 2000-01-01 00:00 UTC in seconds since 1970, the instant of `pg_tz_acceptable`.
const TIME_2000: i64 = 946_684_800;

/// The name of a zone or a link as the data spells it, found without regard to case, as `pg_tzset` finds it.
pub fn zone_name(name: &str) -> Option<&'static str> {
    let key = name.to_ascii_lowercase();
    ZONE_NAMES
        .binary_search_by(|n| n.to_ascii_lowercase().as_str().cmp(&key))
        .ok()
        .map(|i| ZONE_NAMES[i])
}

/// Every name of a zone or a link, sorted by the name in lower case.
pub fn zone_names() -> &'static [&'static str] {
    &ZONE_NAMES
}

/// `ttinfo`: a local time type, as the offset east of UTC, the daylight saving flag, and the index of the abbreviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tti {
    utoff: i32,
    isdst: bool,
    desigidx: usize,
}

/// `struct state` of `pgtz.h`, without the leap seconds, which the data of PostgreSQL does not have.
#[derive(Debug, Clone, Default)]
struct State {
    ats: Vec<i64>,
    types: Vec<u8>,
    ttis: Vec<Tti>,
    /// The abbreviations, each with a NUL after it.
    chars: Vec<u8>,
    goback: bool,
    goahead: bool,
    defaulttype: usize,
}

/// The string at an index of a buffer of strings, up to the next NUL.
fn string_at(chars: &[u8], index: usize) -> &[u8] {
    let rest = chars.get(index..).unwrap_or(&[]);
    rest.split(|&b| b == 0).next().unwrap_or(&[])
}

impl State {
    /// `tzloadbody`: the state from the data of a zone file, with the transitions of the TZ string at the end of the file for the 400 years after the last transition.
    fn from_compiled(compiled: zic::Compiled) -> Option<State> {
        let mut state = State { chars: compiled.chars, ..State::default() };
        for (&at, &kind) in compiled.ats.iter().zip(&compiled.types) {
            if let Some(&last) = state.ats.last()
                && at <= last
            {
                if at < last {
                    return None;
                }
                state.ats.pop();
                state.types.pop();
            }
            state.ats.push(at);
            state.types.push(kind);
        }
        for &(utoff, isdst, desigidx) in &compiled.ttis {
            state.ttis.push(Tti { utoff, isdst, desigidx: usize::from(desigidx) });
        }
        if !compiled.footer.is_empty() && state.ttis.len() + 2 <= TZ_MAX_TYPES {
            state.extend(compiled.footer.as_bytes());
        }
        if state.ttis.is_empty() {
            return None;
        }
        state.repeats();
        state.defaulttype = state.default_type();
        Some(state)
    }

    /// The part of `tzloadbody` that adds the rules of the TZ string, if all of its abbreviations fit.
    fn extend(&mut self, footer: &[u8]) {
        let Some(mut ts) = posix::tzparse(footer, false) else { return };
        let abbrs: Vec<Vec<u8>> =
            ts.ttis.iter().map(|t| string_at(&ts.chars, t.desigidx).to_vec()).collect();
        let mut chars = self.chars.clone();
        let mut got = 0;
        for (tti, abbr) in ts.ttis.iter_mut().zip(&abbrs) {
            let found = (0..chars.len()).find(|&j| string_at(&chars, j) == abbr.as_slice());
            if let Some(j) = found {
                tti.desigidx = j;
                got += 1;
            } else if chars.len() + abbr.len() < TZ_MAX_CHARS {
                tti.desigidx = chars.len();
                chars.extend_from_slice(abbr);
                chars.push(0);
                got += 1;
            }
        }
        if got != ts.ttis.len() {
            return;
        }
        self.chars = chars;
        while self.types.len() > 1
            && self.types[self.types.len() - 1] == self.types[self.types.len() - 2]
        {
            self.ats.pop();
            self.types.pop();
        }
        let first = ts.ats.iter().position(|&at| self.ats.last().is_none_or(|&last| last < at));
        // There are at most 256 types, so the index of a type fits in a byte.
        #[allow(clippy::cast_possible_truncation)]
        let typecnt = self.ttis.len() as u8;
        for (&at, &kind) in ts.ats.iter().zip(&ts.types).skip(first.unwrap_or(ts.ats.len())) {
            if self.ats.len() >= TZ_MAX_TIMES {
                break;
            }
            self.ats.push(at);
            self.types.push(typecnt + kind);
        }
        self.ttis.append(&mut ts.ttis);
    }

    /// `typesequiv`.
    fn types_equiv(&self, a: u8, b: u8) -> bool {
        let (a, b) = (self.ttis[usize::from(a)], self.ttis[usize::from(b)]);
        a.utoff == b.utoff
            && a.isdst == b.isdst
            && string_at(&self.chars, a.desigidx) == string_at(&self.chars, b.desigidx)
    }

    /// The part of `tzloadbody` that sets `goback` and `goahead`: the rules repeat every 400 years before the first transition or after the last one.
    fn repeats(&mut self) {
        let n = self.ats.len();
        if n <= 1 {
            return;
        }
        let repeat = |t1: i64, t0: i64| t1.wrapping_sub(t0) == SECS_PER_REPEAT;
        self.goback = (1..n).any(|i| {
            self.types_equiv(self.types[i], self.types[0]) && repeat(self.ats[i], self.ats[0])
        });
        self.goahead = (0..n - 1).rev().any(|i| {
            self.types_equiv(self.types[n - 1], self.types[i])
                && repeat(self.ats[n - 1], self.ats[i])
        });
    }

    /// The part of `tzloadbody` that finds the type of the instants before the first transition.
    fn default_type(&self) -> usize {
        if !self.types.contains(&0) {
            return 0;
        }
        if let Some(&first) = self.types.first()
            && self.ttis[usize::from(first)].isdst
            && let Some(i) = (0..usize::from(first)).rev().find(|&i| !self.ttis[i].isdst)
        {
            return i;
        }
        self.ttis.iter().position(|t| !t.isdst).unwrap_or(0)
    }

    /// The instant moved by whole cycles of 400 years into the range of the transitions, and the shift, for an instant out of the range of a zone that repeats.
    fn in_range(&self, t: i64) -> Option<(i64, i64)> {
        let (Some(&first), Some(&last)) = (self.ats.first(), self.ats.last()) else { return None };
        if !((self.goback && t < first) || (self.goahead && t > last)) {
            return None;
        }
        let seconds = if t < first { first - t } else { t - last } - 1;
        let shift = (seconds / SECS_PER_REPEAT + 1)
            .checked_mul(YEARS_PER_REPEAT)?
            .checked_mul(AVG_SECS_PER_YEAR)?;
        let newt = if t < first { t.checked_add(shift)? } else { t.checked_sub(shift)? };
        if newt < first || newt > last {
            return None;
        }
        Some((newt, if t < first { -shift } else { shift }))
    }

    /// `localsub`: the type of the local time at an instant.
    fn local_type(&self, t: i64) -> usize {
        // `localsub` fails if the move overflows, which does not occur for the instants of a timestamp. Then the search below gives the type at the end of the range.
        if let Some((newt, _)) = self.in_range(t) {
            return self.local_type(newt);
        }
        if self.ats.is_empty() || t < self.ats[0] {
            return self.defaulttype;
        }
        let (mut lo, mut hi) = (1, self.ats.len());
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if t < self.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        usize::from(self.types[lo - 1])
    }

    /// `pg_next_dst_boundary`: the type at an instant, and the next transition after it with its type.
    fn next_boundary(&self, t: i64) -> (Tti, Option<(i64, Tti)>) {
        let tti = |i: u8| self.ttis[usize::from(i)];
        let n = self.ats.len();
        if n == 0 {
            return (self.ttis[self.defaulttype], None);
        }
        if let Some((newt, shift)) = self.in_range(t) {
            let (before, after) = self.next_boundary(newt);
            return (before, after.map(|(boundary, after)| (boundary + shift, after)));
        }
        if t >= self.ats[n - 1] {
            return (tti(self.types[n - 1]), None);
        }
        if t < self.ats[0] {
            return (self.ttis[self.defaulttype], Some((self.ats[0], tti(self.types[0]))));
        }
        let (mut lo, mut hi) = (1, n - 1);
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if t < self.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        (tti(self.types[lo - 1]), Some((self.ats[lo], tti(self.types[lo]))))
    }

    /// The index of an abbreviation at the start of a string of the buffer, as `pg_interpret_timezone_abbrev` finds it.
    fn abbrev_index(&self, abbrev: &str) -> Option<usize> {
        let mut index = 0;
        while index < self.chars.len() {
            let text = string_at(&self.chars, index);
            if text == abbrev.as_bytes() {
                return Some(index);
            }
            index += text.len() + 1;
        }
        None
    }
}

/// A zone of the time zone database or of a POSIX zone string, with the name that `pg_get_timezone_name` gives.
#[derive(Debug)]
pub struct Zone {
    name: String,
    state: State,
}

impl Zone {
    /// The canonical name: the name in the data, or the zone string in upper case.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `pg_localtime`: the offset east of UTC, the daylight saving flag and the abbreviation at an instant in seconds since 1970-01-01 UTC.
    pub fn local(&self, unix_seconds: i64) -> (i32, bool, &str) {
        let tti = self.state.ttis[self.state.local_type(unix_seconds)];
        (tti.utoff, tti.isdst, self.abbrev(tti.desigidx))
    }

    /// `pg_tz_acceptable`: the local time of 2000-01-01 00:00 UTC is on a whole minute, so the zone does not count leap seconds.
    pub fn acceptable(&self) -> bool {
        let (offset, _, _) = self.local(TIME_2000);
        (TIME_2000 + i64::from(offset)).rem_euclid(60) == 0
    }

    fn abbrev(&self, index: usize) -> &str {
        std::str::from_utf8(string_at(&self.state.chars, index)).unwrap_or("")
    }

    /// The zone of a file of the data, by its name in the data.
    fn from_file(name: &'static str) -> Option<Zone> {
        let state = State::from_compiled(zic::compile(name)?)?;
        Some(Zone { name: name.to_owned(), state })
    }

    /// `pg_tzset` without the cache.
    fn make(upper: &str) -> Option<Zone> {
        if upper == "GMT" {
            return Some(Zone {
                name: upper.to_owned(),
                state: posix::tzparse(upper.as_bytes(), true)?,
            });
        }
        if let Some(name) = zone_name(upper.strip_prefix(':').unwrap_or(upper))
            && let Some(zone) = Zone::from_file(name)
        {
            return Some(zone);
        }
        if upper.starts_with(':') {
            return None;
        }
        Some(Zone { name: upper.to_owned(), state: posix::tzparse(upper.as_bytes(), false)? })
    }
}

impl TimeZone for Zone {
    fn at(&self, unix_seconds: i64) -> (i32, &str) {
        let (offset, _, abbrev) = self.local(unix_seconds);
        (offset, abbrev)
    }

    fn next_change(&self, unix_seconds: i64) -> (i32, Option<(i64, i32)>) {
        let (before, after) = self.state.next_boundary(unix_seconds);
        (before.utoff, after.map(|(boundary, after)| (boundary, after.utoff)))
    }

    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        let index = self.state.abbrev_index(abbrev)?;
        let mut uses = self.state.ttis.iter().filter(|t| t.desigidx == index);
        let first = uses.next()?;
        if uses.all(|t| t.utoff == first.utoff && t.isdst == first.isdst) {
            Some(AbbrevMeaning::Fixed { offset: first.utoff, dst: first.isdst })
        } else {
            Some(AbbrevMeaning::Varies)
        }
    }

    fn abbrev_at(&self, abbrev: &str, unix_seconds: i64) -> Option<(i32, bool)> {
        let state = &self.state;
        let index = state.abbrev_index(abbrev)?;
        let cutoff = state.ats.partition_point(|&at| at <= unix_seconds);
        let tti = |i: usize| state.ttis[usize::from(state.types[i])];
        let found = (0..cutoff)
            .rev()
            .map(tti)
            .chain(std::iter::once(state.ttis[state.defaulttype]))
            .chain((cutoff..state.ats.len()).map(tti))
            .find(|t| t.desigidx == index)?;
        Some((found.utoff, found.isdst))
    }

    fn fixed_offset(&self) -> Option<i32> {
        let first = self.state.ttis[0].utoff;
        self.state.ttis.iter().all(|t| t.utoff == first).then_some(first)
    }

    fn abbrevs(&self) -> Vec<&str> {
        let mut out = Vec::new();
        let mut index = 0;
        while index < self.state.chars.len() {
            let abbrev = string_at(&self.state.chars, index);
            out.push(self.abbrev(index));
            index += abbrev.len() + 1;
        }
        out
    }
}

/// `pg_tzset`: the zone of a name of the data without regard to case, or of a POSIX zone string, or `None`. `GMT` is always the zone string, and a name that starts with `:` is only a name of the data.
pub fn load(name: &str) -> Option<Arc<Zone>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Arc<Zone>>>> = OnceLock::new();
    if name.len() > TZ_STRLEN_MAX {
        return None;
    }
    let upper = name.to_ascii_uppercase();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(zone) = cache.lock().unwrap_or_else(PoisonError::into_inner).get(&upper) {
        return Some(Arc::clone(zone));
    }
    let zone = Arc::new(Zone::make(&upper)?);
    cache.lock().unwrap_or_else(PoisonError::into_inner).insert(upper, Arc::clone(&zone));
    Some(zone)
}

/// `pg_tzenumerate_next`: each zone of the data that [`Zone::acceptable`] takes, in the order of [`zone_names`]. The zones do not go into the cache of [`load`].
pub fn zones() -> impl Iterator<Item = Zone> {
    ZONE_NAMES.iter().filter_map(|name| Zone::from_file(name)).filter(Zone::acceptable)
}

/// The [`ZoneLookup`] of the time zone database: [`load`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TzDatabase;

impl ZoneLookup for TzDatabase {
    fn zone(&self, name: &str) -> Option<Arc<dyn TimeZone + Send + Sync>> {
        load(name).map(|zone| zone as Arc<dyn TimeZone + Send + Sync>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_give_the_offsets_and_the_changes() {
        let zone = load("america/new_york").unwrap();
        assert_eq!(zone.name(), "America/New_York");
        // 2024-07-01 12:00 UTC and 2024-01-01 12:00 UTC.
        assert_eq!(zone.local(1_719_835_200), (-14400, true, "EDT"));
        assert_eq!(zone.local(1_704_110_400), (-18000, false, "EST"));
        // The change at 2024-03-10 07:00 UTC.
        assert_eq!(zone.next_change(1_710_000_000), (-18000, Some((1_710_054_000, -14400))));
        assert!(zone.acceptable());
        let posix = load("xyz5abc").unwrap();
        assert_eq!(posix.name(), "XYZ5ABC");
        assert_eq!(posix.local(1_719_835_200), (-14400, true, "ABC"));
        assert!(!load("XYZ+3:00:30").unwrap().acceptable());
        assert!(load(":XYZ5ABC").is_none());
        assert_eq!(zones().count(), 598);
    }

    #[test]
    fn names_are_found_without_regard_to_case() {
        assert_eq!(zone_name("europe/paris"), Some("Europe/Paris"));
        assert_eq!(zone_name("UTC"), Some("UTC"));
        assert_eq!(zone_name("us/eastern"), Some("US/Eastern"));
        assert_eq!(zone_name("Mars/Olympus"), None);
        assert_eq!(zone_names().len(), 598);
    }
}

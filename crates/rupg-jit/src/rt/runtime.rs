// Lifted from rudb crates/rudb-qc-rt/src/rt.rs at cd9f9676 (2026-10-05).
// Changes: the module `runtime` of `crate::rt`. The cancel flag is the session's `Arc<AtomicBool>`. The errors are rupg's, with the SQLSTATEs and the messages of PostgreSQL for the division of a decimal. A regular expression is a `Pattern` that the driver makes from rupg-func's, in place of rudb-regex, and the tests use a stand-in. A date is PostgreSQL's day number from 2000-01-01, so the date functions go through `j2date` and `date2j`, a year before 1 is the year BC that `extract` gives, and the truncations keep the infinities. `RUNTIME_ERROR` is in `crate::rt`. `hash` and `insert` read the words with `as_chunks`, and two calls are split so that each `unsafe` block has its SAFETY comment right above it. The tests check the PostgreSQL forms, the errors of a pattern and the cancel flag.

//! The runtime compiled code calls into, per section 6.5 of `spec/compiler/06-qir.md`.
//!
//! [`Rt`] owns everything a query's pipelines share: the heap strings are made in, the objects
//! a handle names, the kernels a `vcall` runs, the counters and the cancel flag. A handle is a
//! small integer the generator put in the code as a `ptr` constant, and it indexes the objects
//! the driver added before the query started. A row or accumulator address is a real address.

#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rupg_common::{Error, SqlState};
use rupg_types::{
    POSTGRES_EPOCH_JDATE, TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, date2j, j2date,
};

use crate::interp::Runtime;
use crate::ir::{CATALOGUE, status};
use crate::rt::RUNTIME_ERROR;
use crate::rt::ablate::Ablate;
use crate::rt::join::{JoinTable, Published};
use crate::rt::like::Like;
use crate::rt::mem;
use crate::rt::table::{Distinct, GroupTable, read_u128};
use crate::rt::text::{self, Heap};

/// A runtime function by what it does, so that a call goes to its code by one jump on the proxy
/// rather than by comparing names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    StrPromote,
    StrEq,
    StrCmp,
    StrHash,
    StrLength,
    StrLike,
    StrRegex,
    StrRegexReplace,
    StrLower,
    StrUpper,
    StrConcat,
    HtInsert,
    HtPage,
    JtAppend,
    AggDistinct,
    AggDistinctInt,
    AggDistinctRoom,
    AggMinStr,
    AggMaxStr,
    I128Div,
    DateTruncMinute,
    DateExtractMinute,
    DateExtractYear,
    DateTruncMonth,
    /// A name in the catalogue with no code here, which fails when it is called.
    Missing,
}

impl Call {
    fn named(name: &str) -> Call {
        match name {
            "str_promote" => Call::StrPromote,
            "str_eq" => Call::StrEq,
            "str_cmp" => Call::StrCmp,
            "str_hash" => Call::StrHash,
            "str_length" => Call::StrLength,
            "str_like" => Call::StrLike,
            "str_regex" => Call::StrRegex,
            "str_regex_replace" => Call::StrRegexReplace,
            "str_lower" => Call::StrLower,
            "str_upper" => Call::StrUpper,
            "str_concat" => Call::StrConcat,
            "ht_insert" => Call::HtInsert,
            "ht_page" => Call::HtPage,
            "jt_append" => Call::JtAppend,
            "agg_distinct" => Call::AggDistinct,
            "agg_distinct_int" => Call::AggDistinctInt,
            "agg_distinct_room" => Call::AggDistinctRoom,
            "agg_min_str" => Call::AggMinStr,
            "agg_max_str" => Call::AggMaxStr,
            "i128_div" => Call::I128Div,
            "date_trunc_minute" => Call::DateTruncMinute,
            "date_extract_minute" => Call::DateExtractMinute,
            "date_extract_year" => Call::DateExtractYear,
            "date_trunc_month" => Call::DateTruncMonth,
            _ => Call::Missing,
        }
    }
}

/// The [`Call`] of every proxy, in the catalogue's order.
static CALLS: std::sync::LazyLock<Vec<Call>> =
    std::sync::LazyLock::new(|| CATALOGUE.iter().map(|p| Call::named(p.name)).collect());

/// A first engine kernel over `n` rows of the buffers a `vcall` passes.
pub type Kernel = Box<dyn FnMut(u64, &[u128]) -> Result<(), Error> + Send>;

/// Makes a [`Kernel`]. A kernel keeps state between calls, so every worker needs its own.
pub type Maker = Arc<dyn Fn() -> Kernel + Send + Sync>;

/// A regular expression behind a handle, for `str_regex` and `str_regex_replace`. The runtime has no regular expressions of its own: the driver makes each pattern from the ones of `rupg-func`, so that a compiled `~` or `regexp_replace` gives what PostgreSQL gives, with the flags and the collation of the call.
pub trait Pattern: Send {
    /// Whether `s` has a match.
    ///
    /// # Errors
    ///
    /// The errors of the match, as for a pattern that is too complex.
    fn is_match(&self, s: &str) -> Result<bool, Error>;

    /// Adds `s` with its matches replaced to the end of `out`.
    ///
    /// # Errors
    ///
    /// As for [`Pattern::is_match`].
    fn replace_into(&self, s: &str, out: &mut String) -> Result<(), Error>;

    /// The same pattern for a worker of a parallel pipeline.
    fn worker(&self) -> Box<dyn Pattern>;
}

/// Something a handle names.
enum Object {
    Like(Like),
    Regex(Box<dyn Pattern>),
    Table(GroupTable),
    Distinct(Distinct),
    Join(JoinTable),
    /// What a worker has where the query's runtime has a join table. A probe reads the table
    /// through the addresses its state was given and never through the handle, and a build runs
    /// on one worker, so a worker never needs one.
    Absent,
}

/// The runtime of one query.
pub struct Rt {
    heap: Heap,
    objects: Vec<Object>,
    kernels: Vec<Kernel>,
    makers: Vec<Maker>,
    counters: Vec<u64>,
    cancel: Arc<AtomicBool>,
    error: Option<Error>,
    buffer: String,
    /// The techniques this query leaves out.
    ablate: Ablate,
    /// The runtimes of the workers folded into this one. Their heaps hold strings the rows of
    /// this one's tables point at, so they live as long as it does.
    held: Vec<Rt>,
}

impl std::fmt::Debug for Rt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rt")
            .field("objects", &self.objects.len())
            .field("kernels", &self.kernels.len())
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl Rt {
    /// A runtime that stops when `cancel` is set, which is the cancel flag of the session. Compiled code only reads the flag and stops, and the driver then asks the session why, so that a cancel and a timeout give their own errors.
    #[must_use]
    pub fn new(cancel: Arc<AtomicBool>) -> Rt {
        Rt {
            heap: Heap::new(),
            objects: Vec::new(),
            kernels: Vec::new(),
            makers: Vec::new(),
            counters: Vec::new(),
            cancel,
            error: None,
            buffer: String::new(),
            ablate: Ablate::NONE,
            held: Vec::new(),
        }
    }

    /// Leaves out the techniques in `ablate`, before any handle is made.
    pub fn set_ablate(&mut self, ablate: Ablate) {
        self.ablate = ablate;
    }

    /// The techniques this query leaves out.
    #[must_use]
    pub fn ablate(&self) -> Ablate {
        self.ablate
    }

    /// A runtime for one worker of a parallel pipeline. Every handle names the same kind of
    /// object as here, so the same code runs against it: the patterns are copies, the kernels are
    /// new, and the group tables and distinct sets are empty ones of the same shape, for the
    /// worker to fill and [`Rt::absorb`] to fold back in.
    #[must_use]
    pub fn worker(&self) -> Rt {
        let objects = self
            .objects
            .iter()
            .map(|o| match o {
                Object::Like(like) => Object::Like(like.clone()),
                Object::Regex(pattern) => Object::Regex(pattern.worker()),
                Object::Table(t) => Object::Table(GroupTable::new(t.layout().clone())),
                Object::Distinct(_) => Object::Distinct(Distinct::new()),
                Object::Join(_) | Object::Absent => Object::Absent,
            })
            .collect();
        Rt {
            heap: Heap::new(),
            objects,
            kernels: self.makers.iter().map(|make| make()).collect(),
            makers: self.makers.clone(),
            counters: Vec::new(),
            cancel: self.cancel.clone(),
            error: None,
            ablate: self.ablate,
            buffer: String::new(),
            held: Vec::new(),
        }
    }

    /// Takes a worker's group table `table` and distinct sets `sets` in place of this runtime's,
    /// which is what the first worker of a parallel pipeline does. Nothing was written to this
    /// runtime's while the workers ran, so there is nothing in them to fold.
    pub fn adopt(&mut self, mut worker: Rt, table: u64, sets: &[u64]) {
        for &h in std::iter::once(&table).chain(sets) {
            let h = h as usize;
            if let (Some(mine), Some(theirs)) = (self.objects.get_mut(h), worker.objects.get_mut(h))
            {
                std::mem::swap(mine, theirs);
            }
        }
        self.keep_worker(worker);
    }

    /// Folds a worker's group table `table` and distinct sets `sets` into this runtime's, and says
    /// which group of this table each of the worker's became. `combine` gets a row of this table
    /// and the worker's row of the same group, and folds the accumulators of the second into the
    /// first.
    ///
    /// # Errors
    ///
    /// When a handle does not name a table or a set in both runtimes.
    pub fn absorb(
        &mut self,
        mut worker: Rt,
        table: u64,
        sets: &[u64],
        combine: impl FnMut(&mut [u8], &[u8]),
    ) -> Result<Vec<usize>, Error> {
        let mut map = Vec::new();
        {
            let (Some(Object::Table(mine)), Some(Object::Table(theirs))) =
                (self.objects.get_mut(table as usize), worker.objects.get(table as usize))
            else {
                return Err(bad_handle("absorb"));
            };
            mine.absorb(theirs, &mut map, combine);
        }
        for &h in sets {
            let theirs = match worker.objects.get_mut(h as usize) {
                Some(o @ Object::Distinct(_)) => std::mem::replace(o, Object::Absent),
                _ => return Err(bad_handle("absorb")),
            };
            let (Some(Object::Distinct(mine)), Object::Distinct(theirs)) =
                (self.objects.get_mut(h as usize), theirs)
            else {
                return Err(bad_handle("absorb"));
            };
            mine.absorb(theirs, &map);
        }
        self.keep_worker(worker);
        Ok(map)
    }

    /// Takes the distinct sets behind a handle out of this runtime, and leaves nothing there.
    pub fn take_distinct(&mut self, handle: u64) -> Option<Distinct> {
        match self.objects.get_mut(handle as usize) {
            Some(o @ Object::Distinct(_)) => match std::mem::replace(o, Object::Absent) {
                Object::Distinct(d) => Some(d),
                _ => None,
            },
            _ => None,
        }
    }

    /// Puts `sets` behind a handle that named distinct sets before [`take_distinct`](Rt::take_distinct)
    /// took them.
    ///
    /// # Errors
    ///
    /// When the handle names something else.
    pub fn put_distinct(&mut self, handle: u64, sets: Distinct) -> Result<(), Error> {
        match self.objects.get_mut(handle as usize) {
            Some(o @ (Object::Distinct(_) | Object::Absent)) => {
                *o = Object::Distinct(sets);
                Ok(())
            }
            _ => Err(bad_handle("put_distinct")),
        }
    }

    /// Puts `merged` in place of group table `table`, and keeps `workers` the way a fold does. The
    /// table is what the workers' tables of the same handle became when they were merged, and its
    /// strings point into their heaps, which is why they are kept.
    ///
    /// # Errors
    ///
    /// When `table` does not name a group table.
    pub fn settle(
        &mut self,
        workers: Vec<Rt>,
        table: u64,
        merged: GroupTable,
    ) -> Result<(), Error> {
        let Some(Object::Table(mine)) = self.objects.get_mut(table as usize) else {
            return Err(bad_handle("settle"));
        };
        *mine = merged;
        for worker in workers {
            self.keep_worker(worker);
        }
        Ok(())
    }

    /// Adds the counters of a worker that has nothing left to fold to these, and keeps it.
    pub fn retire(&mut self, worker: Rt) {
        self.keep_worker(worker);
    }

    /// Adds a worker's counters to these and keeps its heap alive.
    fn keep_worker(&mut self, mut worker: Rt) {
        let counters = std::mem::take(&mut worker.counters);
        if self.counters.len() < counters.len() {
            self.counters.resize(counters.len(), 0);
        }
        for (mine, theirs) in self.counters.iter_mut().zip(counters) {
            *mine = mine.wrapping_add(theirs);
        }
        self.held.push(worker);
    }

    fn add(&mut self, object: Object) -> u64 {
        self.objects.push(object);
        self.objects.len() as u64 - 1
    }

    /// A handle on a `LIKE` pattern, folded for `ILIKE`.
    pub fn add_like(&mut self, pattern: &str, fold: bool) -> u64 {
        self.add(Object::Like(Like::new(pattern, fold)))
    }

    /// A handle on a comparison of text against `literal`, answered through the same handle as a
    /// `LIKE`, see [`Like::ordered`].
    pub fn add_ordered(&mut self, literal: &str, keep: [bool; 3]) -> u64 {
        self.add(Object::Like(Like::ordered(literal, keep)))
    }

    /// The `LIKE` pattern behind a handle.
    #[must_use]
    pub fn like(&self, handle: u64) -> Option<&Like> {
        match self.objects.get(handle as usize) {
            Some(Object::Like(like)) => Some(like),
            _ => None,
        }
    }

    /// A handle on a regular expression and, for `regexp_replace`, its replacement.
    pub fn add_pattern(&mut self, pattern: Box<dyn Pattern>) -> u64 {
        self.add(Object::Regex(pattern))
    }

    /// A handle on a grouping table.
    pub fn add_table(&mut self, table: GroupTable) -> u64 {
        self.add(Object::Table(table))
    }

    /// A handle on a join hash table.
    pub fn add_join(&mut self, table: JoinTable) -> u64 {
        self.add(Object::Join(table))
    }

    /// The join table behind a handle.
    #[must_use]
    pub fn join(&self, handle: u64) -> Option<&JoinTable> {
        match self.objects.get(handle as usize) {
            Some(Object::Join(t)) => Some(t),
            _ => None,
        }
    }

    /// Runs the finalize step of a join build: lays out the table behind `handle` so that probes
    /// can read it, and returns what they read.
    ///
    /// # Errors
    ///
    /// When the handle is not a join table, or the table cannot be laid out.
    pub fn finish_join(&mut self, handle: u64) -> Result<Published, Error> {
        let Some(Object::Join(table)) = self.objects.get_mut(handle as usize) else {
            return Err(bad_handle("finish_join"));
        };
        table.finish().map_err(Error::internal)?;
        Ok(table.published())
    }

    /// A handle on a set of distinct values per group.
    pub fn add_distinct(&mut self) -> u64 {
        self.add(Object::Distinct(Distinct::new()))
    }

    /// The id a `vcall` uses for the kernel `make` makes.
    pub fn add_kernel(&mut self, make: Maker) -> u32 {
        self.kernels.push(make());
        self.makers.push(make);
        self.kernels.len() as u32 - 1
    }

    /// The table behind a handle.
    #[must_use]
    pub fn table(&self, handle: u64) -> Option<&GroupTable> {
        match self.objects.get(handle as usize) {
            Some(Object::Table(t)) => Some(t),
            _ => None,
        }
    }

    /// The group table behind a handle, to change.
    pub fn table_mut(&mut self, handle: u64) -> Option<&mut GroupTable> {
        match self.objects.get_mut(handle as usize) {
            Some(Object::Table(t)) => Some(t),
            _ => None,
        }
    }

    /// The distinct sets behind a handle.
    #[must_use]
    pub fn distinct(&self, handle: u64) -> Option<&Distinct> {
        match self.objects.get(handle as usize) {
            Some(Object::Distinct(d)) => Some(d),
            _ => None,
        }
    }

    /// The distinct sets behind a handle, to change.
    pub fn distinct_mut(&mut self, handle: u64) -> Option<&mut Distinct> {
        match self.objects.get_mut(handle as usize) {
            Some(Object::Distinct(d)) => Some(d),
            _ => None,
        }
    }

    /// Takes in the pairs compiled code appended to the distinct sets `handle` through the two
    /// words at the start of `words`.
    pub fn take_appended(&mut self, handle: u64, words: &mut [u8]) {
        let Some(Object::Distinct(set)) = self.objects.get_mut(handle as usize) else {
            return;
        };
        let word =
            |i: usize| u64::from_le_bytes(words[i * 8..i * 8 + 8].try_into().unwrap_or_default());
        let mut pair = [word(0), word(1)];
        set.settle(&mut pair);
        words[..8].copy_from_slice(&pair[0].to_le_bytes());
    }

    /// Keeps `bytes` in the runtime heap for as long as the query runs.
    pub fn keep(&mut self, bytes: &[u8]) -> u128 {
        self.heap.keep(bytes)
    }

    /// The counters, by id.
    #[must_use]
    pub fn counters(&self) -> &[u64] {
        &self.counters
    }

    /// Why the last call that failed with [`RUNTIME_ERROR`] failed.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    /// The bytes the heap holds.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.heap.footprint() + self.held.iter().map(Rt::footprint).sum::<usize>()
    }

    fn fail(&mut self, error: Error) -> u64 {
        self.error = Some(error);
        status::make(status::ERROR, RUNTIME_ERROR)
    }

    fn string(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= text::INLINE { text::make(bytes) } else { self.heap.keep(bytes) }
    }

    fn ht_insert(&mut self, a: &[u128]) -> Result<u128, u64> {
        let Some(Object::Table(table)) = self.objects.get_mut(a[0] as usize) else {
            return Err(self.fail(bad_handle("ht_insert")));
        };
        let at = a.get(3).map_or(0, |&w| w as usize);
        // SAFETY: the generator passes the key buffer and the published words in its state.
        let row = unsafe { insert(table, a[1] as usize, a[2] as u64, at, &mut self.heap) };
        Ok(row as u128)
    }

    fn agg_distinct(&mut self, a: &[u128], text: bool) -> Result<u128, u64> {
        // SAFETY: the second argument is a row `ht_insert` returned, and it starts with the group
        // id.
        let row = unsafe { mem::slice(a[1] as usize, 8) };
        let gid = u64::from_le_bytes(row.try_into().unwrap_or_default());
        let Some(Object::Distinct(set)) = self.objects.get_mut(a[0] as usize) else {
            return Err(self.fail(bad_handle("agg_distinct")));
        };
        if text {
            // SAFETY: as for every `str16` compiled code passes, see `call`.
            set.add_text(gid as usize, unsafe { text::bytes(&a[2]) });
        } else {
            set.add_int(gid as usize, a[2]);
        }
        Ok(0)
    }

    fn agg_distinct_room(&mut self, a: &[u128]) -> Result<u128, u64> {
        let Some(Object::Distinct(set)) = self.objects.get_mut(a[0] as usize) else {
            return Err(self.fail(bad_handle("agg_distinct_room")));
        };
        let at = a[1] as usize;
        // SAFETY: the second argument is the two words in the state compiled code appends through.
        let bytes = unsafe { mem::slice(at, 16) };
        let word =
            |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap_or_default());
        let mut words = [word(0), word(1)];
        set.room(&mut words);
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&words[0].to_le_bytes());
        bytes[8..].copy_from_slice(&words[1].to_le_bytes());
        // SAFETY: as above.
        unsafe { mem::write(at, &bytes) };
        Ok(0)
    }

    #[inline(always)]
    fn call(&mut self, call: Call, name: &str, a: &[u128]) -> Result<u128, u64> {
        // SAFETY: every `str16` compiled code passes is inline or points at bytes the driver or
        // this heap keeps alive for the whole query, which is rule V10.
        let s = |i: usize| unsafe { text::bytes(&a[i]) };
        Ok(match call {
            Call::StrPromote => {
                let bytes = s(1).to_vec();
                self.string(&bytes)
            }
            Call::StrEq => u128::from(a[0] == a[1] || s(0) == s(1)),
            Call::StrCmp => u128::from(s(0).cmp(s(1)) as i32 as u32),
            Call::StrHash => u128::from(hash(s(0), a[1] as u64)),
            Call::StrLength => {
                u128::from(s(0).iter().filter(|b| (**b as i8) >= -0x40).count() as u64)
            }
            Call::StrLike => match self.objects.get(a[0] as usize) {
                Some(Object::Like(like)) => u128::from(like.matches(s(1))),
                _ => return Err(self.fail(bad_handle(name))),
            },
            Call::StrRegex => {
                let Some(Object::Regex(pattern)) = self.objects.get(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                match pattern.is_match(utf8(s(1))) {
                    Ok(found) => u128::from(found),
                    Err(e) => return Err(self.fail(e)),
                }
            }
            Call::StrRegexReplace => {
                let Some(Object::Regex(pattern)) = self.objects.get(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                let mut out = std::mem::take(&mut self.buffer);
                out.clear();
                let done = pattern.replace_into(utf8(s(1)), &mut out);
                let v = self.string(out.as_bytes());
                self.buffer = out;
                if let Err(e) = done {
                    return Err(self.fail(e));
                }
                v
            }
            Call::StrLower | Call::StrUpper => {
                let t = utf8(s(0));
                let out = if call == Call::StrLower { t.to_lowercase() } else { t.to_uppercase() };
                self.string(out.as_bytes())
            }
            Call::StrConcat => {
                let mut out = Vec::with_capacity(s(0).len() + s(1).len());
                out.extend_from_slice(s(0));
                out.extend_from_slice(s(1));
                self.string(&out)
            }
            Call::HtInsert => self.ht_insert(a)?,
            Call::HtPage => {
                let Some(Object::Table(table)) = self.objects.get_mut(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                table.page() as u128
            }
            Call::JtAppend => {
                let Some(Object::Join(table)) = self.objects.get_mut(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                // SAFETY: the generator passes the record buffer in its state, laid out as the
                // table's layout says.
                unsafe { table.append(a[1] as usize, a[2] as u64, &mut self.heap) };
                0
            }
            Call::AggDistinct => self.agg_distinct(a, true)?,
            Call::AggDistinctInt => self.agg_distinct(a, false)?,
            Call::AggDistinctRoom => self.agg_distinct_room(a)?,
            Call::AggMinStr | Call::AggMaxStr => {
                let at = a[0] as usize;
                // SAFETY: the first argument is the accumulator in a group row, a `str16` and a
                // byte that says whether it holds a value yet.
                let acc = unsafe { mem::slice(at, 17) };
                let (old, seen) = (read_u128(acc), acc[16] != 0);
                let better = !seen || {
                    // SAFETY: the accumulator holds a string this heap keeps.
                    let old = unsafe { text::bytes(&old) };
                    let ord = s(1).cmp(old);
                    if call == Call::AggMinStr { ord.is_lt() } else { ord.is_gt() }
                };
                if better {
                    let bytes = s(1).to_vec();
                    let v = self.string(&bytes);
                    let mut w = [1u8; 17];
                    w[..16].copy_from_slice(&v.to_le_bytes());
                    // SAFETY: as above, and nothing else writes the row while this runs.
                    unsafe { mem::write(at, &w) };
                }
                0
            }
            Call::I128Div => {
                let (x, y) = (a[0] as i128, a[1] as i128);
                match x.checked_div(y) {
                    Some(q) => q as u128,
                    None if y == 0 => {
                        return Err(
                            self.fail(Error::new(SqlState::DIVISION_BY_ZERO, "division by zero"))
                        );
                    }
                    None => {
                        return Err(self.fail(Error::new(
                            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                            "value overflows numeric format",
                        )));
                    }
                }
            }
            Call::DateTruncMinute => {
                let t = a[0] as u64 as i64;
                if t == TIMESTAMP_INFINITY || t == TIMESTAMP_NEGATIVE_INFINITY {
                    return Ok(a[0]);
                }
                u128::from(t.div_euclid(MINUTE).wrapping_mul(MINUTE) as u64)
            }
            Call::DateExtractMinute => {
                let t = a[0] as u64 as i64;
                u128::from(t.div_euclid(MINUTE).rem_euclid(60) as u64)
            }
            Call::DateExtractYear => {
                let (y, _, _) = j2date((a[0] as u32 as i32).wrapping_add(POSTGRES_EPOCH_JDATE));
                // The year before 1 is 1 BC, as `extract` gives it.
                let y = if y > 0 { y } else { y - 1 };
                u128::from(i64::from(y) as u64)
            }
            Call::DateTruncMonth => {
                let day = a[0] as u32 as i32;
                if day == i32::MIN || day == i32::MAX {
                    return Ok(a[0]);
                }
                let (y, m, _) = j2date(day.wrapping_add(POSTGRES_EPOCH_JDATE));
                u128::from(date2j(y, m as i32, 1).wrapping_sub(POSTGRES_EPOCH_JDATE) as u32)
            }
            Call::Missing => {
                return Err(self.fail(Error::internal(format!("no runtime function {name}"))));
            }
        })
    }
}

const MINUTE: i64 = 60_000_000;

fn bad_handle(name: &str) -> Error {
    Error::internal(format!("{name} got a handle on the wrong kind of object"))
}

fn utf8(bytes: &[u8]) -> &str {
    // A varchar is UTF-8 everywhere it is made, so a string that is not came from a bug, and the
    // replacement keeps the bug visible without undefined behavior.
    std::str::from_utf8(bytes).unwrap_or("\u{fffd}")
}

/// The hash of a string's bytes, mixed with `seed` so a key of several columns chains.
#[must_use]
pub fn hash(bytes: &[u8], seed: u64) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut h = seed ^ (bytes.len() as u64).wrapping_mul(K);
    let (chunks, rest) = bytes.as_chunks::<8>();
    for c in chunks {
        h = (h ^ u64::from_le_bytes(*c)).wrapping_mul(K).rotate_left(29);
    }
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(K).rotate_left(29);
    }
    h ^= h >> 32;
    h.wrapping_mul(K) ^ (h >> 29)
}

impl Runtime for Rt {
    fn rtcall(&mut self, proxy: u32, args: &[u128]) -> Result<u128, u64> {
        let mut out = 0;
        match self.rtcall_into(proxy, args, &mut out) {
            0 => Ok(out),
            s => Err(s),
        }
    }

    fn rtcall_into(&mut self, proxy: u32, args: &[u128], out: &mut u128) -> u64 {
        let (Some(&call), Some(p)) = (CALLS.get(proxy as usize), CATALOGUE.get(proxy as usize))
        else {
            return self.fail(Error::internal(format!("no runtime function {proxy}")));
        };
        // The value is written to `out` here, where it is still in a register. Returned in a
        // `Result`, it went through memory and was read back with a wider load than the stores
        // that wrote it, which stalled every call.
        let r = match call {
            Call::HtInsert => {
                let Some(Object::Table(table)) = self.objects.get_mut(args[0] as usize) else {
                    return self.fail(bad_handle("ht_insert"));
                };
                let at = args.get(3).map_or(0, |&w| w as usize);
                let (key, hash) = (args[1] as usize, args[2] as u64);
                // SAFETY: as for `ht_insert`.
                let row = unsafe { insert(table, key, hash, at, &mut self.heap) };
                Ok(row as u128)
            }
            Call::AggDistinct => self.agg_distinct(args, true),
            Call::AggDistinctInt => self.agg_distinct(args, false),
            Call::AggDistinctRoom => self.agg_distinct_room(args),
            _ => self.call(call, p.name, args),
        };
        match r {
            Ok(v) => {
                *out = v;
                0
            }
            Err(s) => s,
        }
    }

    fn vcall(&mut self, kernel: u32, n: u64, buffers: &[u128]) -> Result<(), u64> {
        let Some(k) = self.kernels.get_mut(kernel as usize) else {
            return Err(self.fail(Error::internal(format!("no kernel {kernel}"))));
        };
        match k(n, buffers) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn count(&mut self, k: u32, v: u64) {
        let k = k as usize;
        if self.counters.len() <= k {
            self.counters.resize(k + 1, 0);
        }
        self.counters[k] = self.counters[k].wrapping_add(v);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Inserts the key at `key` into `table`. When `at` is not zero it is the address of the six
/// words of the state where compiled code finds a key's row without calling here: the slots, their
/// mask, the rows, how many keys it found that way since the last call, and the lanes' tails and
/// how many more rows it may append to them without looking. This adds the found keys to the
/// table's count and the rows appended to its groups, and publishes all six again with the count
/// zeroed, since the insert may have moved them.
///
/// # Safety
///
/// `key` must be a key laid out as the table's layout says and `at` zero or the address of six
/// writable words.
unsafe fn insert(
    table: &mut GroupTable,
    key: usize,
    hash: u64,
    at: usize,
    heap: &mut Heap,
) -> usize {
    if at == 0 {
        // SAFETY: the caller's contract.
        return unsafe { table.insert(key, hash, heap) };
    }
    // SAFETY: the caller's contract.
    let old = unsafe { mem::slice(at + 24, 24) };
    let word = |i: usize| u64::from_le_bytes(old[i * 8..i * 8 + 8].try_into().unwrap_or_default());
    table.found(word(0) as usize);
    if word(1) != 0 {
        table.made_blind(word(2));
    }
    // SAFETY: the caller's contract.
    let row = unsafe { table.insert(key, hash, heap) };
    let mut words = [0u8; 48];
    let [slots, mask, rows] = table.published();
    let [tails, left] = table.blind_words();
    for (w, v) in words.as_chunks_mut::<8>().0.iter_mut().zip([slots, mask, rows, 0, tails, left]) {
        w.copy_from_slice(&v.to_le_bytes());
    }
    // SAFETY: the caller's contract.
    unsafe { mem::write(at, &words) };
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::catalogue::proxy;

    /// A pattern that stands for `^https?://(?:www\\.)?([^/]+)/.*$` replaced with its group, as the tests of the runtime need no regular expressions of their own.
    struct Host;

    impl Pattern for Host {
        fn is_match(&self, s: &str) -> Result<bool, Error> {
            Ok(s.starts_with("http://") || s.starts_with("https://"))
        }

        fn replace_into(&self, s: &str, out: &mut String) -> Result<(), Error> {
            let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
            let Some(rest) = rest else {
                out.push_str(s);
                return Ok(());
            };
            let rest = rest.strip_prefix("www.").unwrap_or(rest);
            match rest.split_once('/') {
                Some((host, _)) => out.push_str(host),
                None => out.push_str(s),
            }
            Ok(())
        }

        fn worker(&self) -> Box<dyn Pattern> {
            Box::new(Host)
        }
    }

    fn call(rt: &mut Rt, name: &str, args: &[u128]) -> Result<u128, u64> {
        rt.rtcall(proxy(name).unwrap(), args)
    }

    #[test]
    fn every_proxy_in_the_catalogue_is_implemented() {
        let mut rt = Rt::new(Arc::default());
        let like = rt.add_like("%a%", false);
        let re = rt.add_pattern(Box::new(Host));
        let table = rt.add_table(GroupTable::new(crate::rt::table::Layout {
            init: vec![0; 24],
            ..Default::default()
        }));
        let set = rt.add_distinct();
        let join = rt.add_join(JoinTable::new(crate::rt::join::JoinLayout::default()));
        let row = rt.table(table).unwrap().address(0) as u128;
        let s = text::make(b"abc");
        let mut words = [0u64; 2];
        for (i, p) in CATALOGUE.iter().enumerate() {
            let args: Vec<u128> = match p.name {
                "str_like" => vec![u128::from(like), s],
                "str_regex" | "str_regex_replace" => vec![u128::from(re), s],
                "ht_insert" => vec![u128::from(table), 0, 5, 0],
                "ht_page" => vec![u128::from(table)],
                "jt_append" => vec![u128::from(join), 0, 5],
                "agg_distinct" | "agg_distinct_int" => vec![u128::from(set), row, s],
                "agg_distinct_room" => vec![u128::from(set), words.as_mut_ptr() as u128],
                "agg_min_str" | "agg_max_str" => vec![row + 8],
                "i128_div" => vec![10, 3],
                _ => vec![s; p.args.len()],
            };
            let mut args = args;
            args.resize(p.args.len(), s);
            let r = rt.rtcall(i as u32, &args);
            assert!(r.is_ok(), "{} failed: {:?}", p.name, rt.take_error());
        }
    }

    #[test]
    fn strings_and_dates_match_the_first_engine() {
        let mut rt = Rt::new(Arc::default());
        let re = rt.add_pattern(Box::new(Host));
        let url = rt.keep(b"http://www.example.com/page?x=1");
        let host = call(&mut rt, "str_regex_replace", &[u128::from(re), url]).unwrap();
        // SAFETY: the host is inline or in the runtime heap.
        assert_eq!(unsafe { text::bytes(&host) }, b"example.com");
        let long = rt.keep("a string in the heap, Ünïcode".as_bytes());
        assert_eq!(call(&mut rt, "str_length", &[long]).unwrap(), 29);
        let up = call(&mut rt, "str_upper", &[long]).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { text::bytes(&up) }, "A STRING IN THE HEAP, ÜNÏCODE".as_bytes());
        let same = rt.keep("a string in the heap, Ünïcode".as_bytes());
        assert_eq!(call(&mut rt, "str_eq", &[long, same]).unwrap(), 1);
        assert_eq!(
            call(&mut rt, "str_cmp", &[text::make(b"a"), text::make(b"b")]).unwrap(),
            u128::from(u32::MAX)
        );
        let micros = 1_373_846_400_000_000u128 + 61 * 60_000_000 + 5;
        assert_eq!(call(&mut rt, "date_extract_minute", &[micros]).unwrap(), 1);
        assert_eq!(call(&mut rt, "date_trunc_minute", &[micros]).unwrap(), micros - 5);
        let day = u128::from((date2j(2013, 7, 15) - POSTGRES_EPOCH_JDATE) as u32);
        assert_eq!(call(&mut rt, "date_extract_year", &[day]).unwrap(), 2013);
        assert_eq!(
            call(&mut rt, "date_trunc_month", &[day]).unwrap(),
            u128::from((date2j(2013, 7, 1) - POSTGRES_EPOCH_JDATE) as u32)
        );
        let e = call(&mut rt, "i128_div", &[1, 0]).unwrap_err();
        assert_eq!(status::kind(e), status::ERROR);
        assert!(rt.take_error().is_some());
    }

    #[test]
    fn dates_and_decimals_follow_postgres() {
        let mut rt = Rt::new(Arc::default());
        // 2000-01-01 is day 0, and the year before 1 is 1 BC.
        assert_eq!(call(&mut rt, "date_extract_year", &[0]).unwrap(), 2000);
        let bc = u128::from((date2j(0, 6, 1) - POSTGRES_EPOCH_JDATE) as u32);
        assert_eq!(call(&mut rt, "date_extract_year", &[bc]).unwrap() as u64 as i64, -1);
        let day = u128::from((date2j(1999, 12, 31) - POSTGRES_EPOCH_JDATE) as u32);
        assert_eq!(
            call(&mut rt, "date_trunc_month", &[day]).unwrap(),
            u128::from((date2j(1999, 12, 1) - POSTGRES_EPOCH_JDATE) as u32)
        );
        for d in [i32::MIN, i32::MAX] {
            let d = u128::from(d as u32);
            assert_eq!(call(&mut rt, "date_trunc_month", &[d]).unwrap(), d);
        }
        for t in [TIMESTAMP_NEGATIVE_INFINITY, TIMESTAMP_INFINITY] {
            let t = u128::from(t as u64);
            assert_eq!(call(&mut rt, "date_trunc_minute", &[t]).unwrap(), t);
        }
        // A minute before 2000-01-01 truncates down.
        let before = u128::from((-MINUTE - 1) as u64);
        assert_eq!(
            call(&mut rt, "date_trunc_minute", &[before]).unwrap(),
            u128::from((-2 * MINUTE) as u64)
        );
        call(&mut rt, "i128_div", &[1, 0]).unwrap_err();
        assert_eq!(rt.take_error().unwrap().state(), SqlState::DIVISION_BY_ZERO);
        call(&mut rt, "i128_div", &[i128::MIN as u128, -1i128 as u128]).unwrap_err();
        assert_eq!(rt.take_error().unwrap().state(), SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    }

    #[test]
    fn a_pattern_error_is_the_runtime_error() {
        struct Fails;
        impl Pattern for Fails {
            fn is_match(&self, _: &str) -> Result<bool, Error> {
                Err(Error::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "regular expression is too complex",
                ))
            }
            fn replace_into(&self, s: &str, _: &mut String) -> Result<(), Error> {
                self.is_match(s).map(drop)
            }
            fn worker(&self) -> Box<dyn Pattern> {
                Box::new(Fails)
            }
        }
        let mut rt = Rt::new(Arc::default());
        let re = rt.add_pattern(Box::new(Fails));
        for name in ["str_regex", "str_regex_replace"] {
            let e = call(&mut rt, name, &[u128::from(re), text::make(b"abc")]).unwrap_err();
            assert_eq!(e, status::make(status::ERROR, RUNTIME_ERROR));
            assert_eq!(rt.take_error().unwrap().state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
        }
        // A worker gets its own pattern behind the same handle.
        let mut worker = rt.worker();
        call(&mut worker, "str_regex", &[u128::from(re), text::make(b"abc")]).unwrap_err();
        assert!(worker.take_error().is_some());
    }

    #[test]
    fn the_cancel_flag_is_the_sessions() {
        let flag = Arc::new(AtomicBool::new(false));
        let rt = Rt::new(Arc::clone(&flag));
        let worker = rt.worker();
        assert!(!rt.cancelled());
        flag.store(true, Ordering::Relaxed);
        assert!(rt.cancelled() && worker.cancelled());
    }

    #[test]
    fn min_and_max_of_strings_keep_the_extreme() {
        let mut rt = Rt::new(Arc::default());
        let mut acc = [0u8; 17];
        let at = acc.as_mut_ptr().expose_provenance() as u128;
        for w in ["pear", "apple and a long tail", "zebra"] {
            let s = rt.keep(w.as_bytes());
            call(&mut rt, "agg_min_str", &[at, s]).unwrap();
        }
        let v = read_u128(&acc);
        // SAFETY: the accumulator holds a heap string.
        assert_eq!(unsafe { text::bytes(&v) }, b"apple and a long tail");
    }
}

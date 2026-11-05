//! The groups of a vector scan on the tasks of the server (spec/14 section 14.5), as a hash aggregate with parts of the hashes. Each task reads a run of the vectors and keeps the rows that pass the conditions. It folds the rows into a small table that stays in the cache, and when the table is full, it moves the groups of the table into [`PARTS`] buffers, one for each part of the hashes of the keys. When the table folds few rows into each group, the task puts each row into the buffers with no table. Then each task merges the buffers of some parts, from the buffers of the first run to the last, so the first row of each group is its first row in the scan. The groups then go in the order of their first rows, which is the order of the scan on one task.

use std::sync::Arc;

use rupg_column::segment::ROWS;
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::Result;
use rupg_platform::{Job, Tasks};
use rupg_summary::zone::{Test, Verdict};

use super::{Decoded, DecodedData, FxMap, WordTable, decode, passes};

/// The number of parts of the hashes of the keys.
const PARTS: usize = 256;

/// The most groups of the table of a task before the task moves the groups into the buffers of the parts. The table then fits in the cache of one core.
const LOCAL: usize = 1 << 13;

/// The rows of a vector after a merge of the buffers, by which a probe asks for its slot first.
const PREFETCH: usize = 16;

/// The least number of rows for each group, by the estimate of the plan, for the scan to find the groups at the same time. With fewer rows for each group, the table of each task keeps few rows out, and the parts, their merge and their order cost more CPU time than the scan of one task: on 2M rows of ClickBench, 1 to 7 rows for each group took 1.4 to 2 times the CPU time, and 9 rows for each group took less.
pub(super) const GROUP_ROWS: usize = 8;

/// The fewest rows for each task, so that a small table does not pay for the tasks.
pub(super) const TASK_ROWS: usize = 1 << 16;

/// A key of `GROUP BY`: a column of integers or codes by its place in the columns of the scan, a column of integers that each value rounds down to a multiple of a step, or a constant word, with `None` for null.
#[derive(Clone, Copy)]
pub(super) enum Input {
    Column(usize),
    /// `date_trunc` of a column of timestamps to a unit of `step` microseconds. An infinite timestamp keeps its value, as in `timestamp_trunc`.
    Floor(usize, i64),
    Const(Option<u64>),
}

/// How a call of an aggregate, or a base of calls, folds the rows of a group. Each column is a column of integers by its place, except the column of [`Fold::CountArg`], which can have any values.
#[derive(Clone, Copy)]
pub(super) enum Fold {
    Count,
    CountArg(usize),
    Sum(usize),
    Min(usize),
    Max(usize),
    /// A call whose states are the states of its base.
    None,
}

/// The states of a fold for each group.
pub(super) enum Folded {
    Count(Vec<i64>),
    Sum(Vec<(i64, i128)>),
    Extreme(Vec<Option<i64>>),
    None,
}

impl Folded {
    fn of(fold: Fold) -> Folded {
        match fold {
            Fold::Count | Fold::CountArg(_) => Folded::Count(Vec::new()),
            Fold::Sum(_) => Folded::Sum(Vec::new()),
            Fold::Min(_) | Fold::Max(_) => Folded::Extreme(Vec::new()),
            Fold::None => Folded::None,
        }
    }

    fn push(&mut self) {
        match self {
            Folded::Count(v) => v.push(0),
            Folded::Sum(v) => v.push((0, 0)),
            Folded::Extreme(v) => v.push(None),
            Folded::None => {}
        }
    }

    /// Adds a new state, the state `from` of `other`.
    fn take(&mut self, other: &Folded, from: usize) {
        match (self, other) {
            (Folded::Count(a), Folded::Count(b)) => a.push(b[from]),
            (Folded::Sum(a), Folded::Sum(b)) => a.push(b[from]),
            (Folded::Extreme(a), Folded::Extreme(b)) => a.push(b[from]),
            _ => {}
        }
    }

    /// Adds a new state of one row, whose part is `cell`.
    #[inline]
    fn push_cell(&mut self, cell: Cell) {
        match (self, cell) {
            (Folded::Count(a), Cell::Count(n)) => a.push(n),
            (Folded::Sum(a), Cell::Sum(n, v)) => a.push((n, v)),
            (Folded::Extreme(a), Cell::Extreme(v)) => a.push(v),
            _ => {}
        }
    }

    /// Adds the part `cell` of one row into the state `to`.
    #[inline]
    fn fold_cell(&mut self, to: usize, cell: Cell, min: bool) {
        match (self, cell) {
            (Folded::Count(a), Cell::Count(n)) => a[to] += n,
            (Folded::Sum(a), Cell::Sum(n, v)) => {
                a[to].0 += n;
                a[to].1 += v;
            }
            (Folded::Extreme(a), Cell::Extreme(Some(v))) => {
                a[to] = Some(a[to].map_or(v, |s| if min { s.min(v) } else { s.max(v) }));
            }
            _ => {}
        }
    }

    fn reserve(&mut self, more: usize) {
        match self {
            Folded::Count(v) => v.reserve(more),
            Folded::Sum(v) => v.reserve(more),
            Folded::Extreme(v) => v.reserve(more),
            Folded::None => {}
        }
    }

    fn clear(&mut self) {
        match self {
            Folded::Count(v) => v.clear(),
            Folded::Sum(v) => v.clear(),
            Folded::Extreme(v) => v.clear(),
            Folded::None => {}
        }
    }

    /// Adds the state `from` of `other` into the state `to`.
    fn merge(&mut self, to: usize, other: &Folded, from: usize, min: bool) {
        match (self, other) {
            (Folded::Count(a), Folded::Count(b)) => a[to] += b[from],
            (Folded::Sum(a), Folded::Sum(b)) => {
                a[to].0 += b[from].0;
                a[to].1 += b[from].1;
            }
            (Folded::Extreme(a), Folded::Extreme(b)) => {
                if let Some(v) = b[from] {
                    a[to] = Some(a[to].map_or(v, |s| if min { s.min(v) } else { s.max(v) }));
                }
            }
            _ => {}
        }
    }
}

/// The part of the state of a fold that one row gives.
#[derive(Clone, Copy)]
enum Cell {
    Count(i64),
    Sum(i64, i128),
    Extreme(Option<i64>),
    None,
}

/// The keys and the folds of a scan of groups. The folds of the bases come after the folds of the calls.
pub(super) struct Plan {
    pub(super) keys: Vec<Input>,
    pub(super) folds: Vec<Fold>,
    /// The number of folds of bases at the end of `folds`, whose extremes the scan keeps.
    pub(super) bases: usize,
    /// The conditions of the scan, each a test of one column by its place. A test other than a null test is on a column of integers.
    pub(super) filters: Vec<(usize, Test)>,
}

/// The groups of a scan, in the order of their first rows.
pub(super) struct Found {
    /// The words of the keys and the mask of nulls of each group.
    pub(super) words: Vec<u64>,
    /// The first row of each group, as `segment * ROWS + vector * VECTOR + row`.
    pub(super) firsts: Vec<u64>,
    /// The states of each fold of the plan.
    pub(super) states: Vec<Folded>,
    /// The smallest and the largest value of each base.
    pub(super) extremes: Vec<Option<(i64, i64)>>,
    /// True when the last row of the scan is not the first row of its group.
    pub(super) trailing: bool,
}

/// Groups with the words of their keys, the first row and the states of each group, in the order of their first rows. Each key has `width` words, the words of the keys and the mask of nulls, so that a buffer keeps only the words that the plan uses. The hash of a key is not kept: to compute it again costs less than to write it and to read it again.
struct Buf {
    width: usize,
    keys: Vec<u64>,
    firsts: Vec<u64>,
    states: Vec<Folded>,
}

impl Buf {
    fn new(folds: &[Fold], width: usize) -> Buf {
        Buf {
            width,
            keys: Vec::new(),
            firsts: Vec::new(),
            states: folds.iter().map(|&f| Folded::of(f)).collect(),
        }
    }

    fn len(&self) -> usize {
        self.firsts.len()
    }

    /// The words of the key of the group `g`, with 0 after its words.
    #[inline]
    fn key(&self, g: usize) -> [u64; 4] {
        // Each arm copies a known number of words, so the copy is not a call of `memcpy`.
        match self.keys[g * self.width..(g + 1) * self.width] {
            [a] => [a, 0, 0, 0],
            [a, b] => [a, b, 0, 0],
            [a, b, c] => [a, b, c, 0],
            [a, b, c, d] => [a, b, c, d],
            _ => unreachable!("a key has 1 to 4 words"),
        }
    }

    #[inline]
    fn push(&mut self, key: &[u64; 4], first: u64) {
        match self.width {
            1 => self.keys.push(key[0]),
            2 => self.keys.extend_from_slice(&key[..2]),
            3 => self.keys.extend_from_slice(&key[..3]),
            _ => self.keys.extend_from_slice(key),
        }
        self.firsts.push(first);
    }

    fn reserve(&mut self, more: usize) {
        self.keys.reserve(more * self.width);
        self.firsts.reserve(more);
        for state in &mut self.states {
            state.reserve(more);
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.firsts.clear();
        for state in &mut self.states {
            state.clear();
        }
    }
}

/// The groups of one run of vectors, in the buffer of each part.
struct Run {
    parts: Vec<Buf>,
    extremes: Vec<Option<(i64, i64)>>,
    /// The last row of the run that passes the conditions.
    last: Option<u64>,
}

fn null(decoded: &Decoded, i: usize) -> bool {
    decoded.nulls.as_ref().is_some_and(|n| n[i])
}

fn int(decoded: &Decoded, i: usize) -> i64 {
    match &decoded.data {
        DecodedData::Int(v) => v[i],
        DecodedData::Code(v) => i64::from(v[i]),
        DecodedData::Float(v) => v[i].to_bits() as i64,
    }
}

/// The part of the hashes of a key with the hash `hash`. The multiply makes the bits of the part other bits than the low bits that the tables read.
#[inline]
fn part_of(hash: u64) -> usize {
    (hash.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - PARTS.trailing_zeros())) as usize
}

/// The part of the state of `fold` that the row `i` gives, from the decoded columns `decoded`.
#[inline]
fn cell(fold: Fold, decoded: &[Option<Decoded>], i: usize) -> Cell {
    let value = |place: usize| decoded[place].as_ref().filter(|d| !null(d, i)).map(|d| int(d, i));
    match fold {
        Fold::Count => Cell::Count(1),
        Fold::CountArg(place) => Cell::Count(i64::from(value(place).is_some())),
        Fold::Sum(place) => value(place).map_or(Cell::Sum(0, 0), |v| Cell::Sum(1, i128::from(v))),
        Fold::Min(place) | Fold::Max(place) => Cell::Extreme(value(place)),
        Fold::None => Cell::None,
    }
}

/// Moves the groups of `local` into the buffers of their parts, and empties `local` and its table.
fn flush(local: &mut Buf, table: &mut WordTable<4>, parts: &mut [Buf]) {
    for g in 0..local.len() {
        let key = local.key(g);
        let part = &mut parts[part_of(WordTable::<4>::hash(&key))];
        part.push(&key, local.firsts[g]);
        for (to, from) in part.states.iter_mut().zip(&local.states) {
            to.take(from, g);
        }
    }
    local.clear();
    table.clear();
}

/// Puts into `sel` the rows of the vector `vector` of the segment `segment` that pass the filters of `plan`, and the columns that the filters test into `decoded`. False when no row passes.
fn select<'p>(
    stored: &[Arc<Stored>],
    plan: &'p Plan,
    rows: usize,
    (segment, vector): (usize, usize),
    tests: &mut Vec<&'p (usize, Test)>,
    sel: &mut Vec<usize>,
    decoded: &mut [Option<Decoded>],
) -> Result<bool> {
    let segment_rows = rows.saturating_sub(segment * ROWS).min(ROWS);
    let count = (segment_rows - vector * VECTOR).min(VECTOR);
    // spec/12 section 12.6.1: a vector whose zones show that no row passes a condition is not read, and a condition that every row passes is not tested.
    tests.clear();
    for filter in &plan.filters {
        let zones = &stored[filter.0].segments[segment].summary.vectors;
        let verdict = if zones.len() == stored::vectors(segment_rows) {
            zones[vector].test(&filter.1)
        } else {
            Verdict::Some
        };
        match verdict {
            Verdict::None => return Ok(false),
            Verdict::All => {}
            Verdict::Some => tests.push(filter),
        }
    }
    sel.clear();
    sel.extend(0..count);
    for &&(place, ref test) in tests.iter() {
        let d = decode(&stored[place], segment, vector)?;
        keep(test, &d, sel);
        decoded[place] = Some(d);
        if sel.is_empty() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Keeps the rows of `sel` whose values in `d` pass `test`. The kind of the test and the form of the values are found once for the vector and not once for each row, and the loop keeps a row with no branch, since a branch on a condition that some rows pass and some do not is often wrong.
fn keep(test: &Test, d: &Decoded, sel: &mut Vec<usize>) {
    let DecodedData::Int(v) = &d.data else {
        retain(sel, |i| passes(test, null(d, i), int(d, i)));
        return;
    };
    let c = |key: &u64| rupg_summary::key::int_of(*key);
    match (test, d.nulls.as_deref()) {
        (Test::IsNull, None) => sel.clear(),
        (Test::IsNotNull, None) => {}
        (Test::IsNull | Test::IsNotNull, Some(_)) => {
            retain(sel, |i| passes(test, null(d, i), 0));
        }
        (Test::Eq(k), n) => compare(v, n, sel, |x| x == c(k)),
        (Test::Ne(k), n) => compare(v, n, sel, |x| x != c(k)),
        (Test::Lt(k), n) => compare(v, n, sel, |x| x < c(k)),
        (Test::Le(k), n) => compare(v, n, sel, |x| x <= c(k)),
        (Test::Gt(k), n) => compare(v, n, sel, |x| x > c(k)),
        (Test::Ge(k), n) => compare(v, n, sel, |x| x >= c(k)),
        (Test::In(keys), n) => {
            let values: Vec<i64> = keys.iter().map(c).collect();
            compare(v, n, sel, |x| values.contains(&x));
        }
    }
}

/// Keeps the rows of `sel` whose values in `v` are not null by `nulls` and pass `test`.
#[inline]
fn compare(v: &[i64], nulls: Option<&[bool]>, sel: &mut Vec<usize>, test: impl Fn(i64) -> bool) {
    match nulls {
        None => retain(sel, |i| test(v[i])),
        Some(nulls) => retain(sel, |i| !nulls[i] & test(v[i])),
    }
}

/// Keeps the rows of `sel` that `keep` gives true for, in their order. Each row is written, and the end of the kept rows moves only when the row is kept.
#[inline]
fn retain(sel: &mut Vec<usize>, keep: impl Fn(usize) -> bool) {
    let mut kept = 0;
    for j in 0..sel.len() {
        let i = sel[j];
        sel[kept] = i;
        kept += usize::from(keep(i));
    }
    sel.truncate(kept);
}

/// The number of the values that are not null of the key of `plan`, which is its one key and a column, in the rows of the scan that pass its filters, and the first of these rows. Each task reads a run of the vectors, and puts each value that is not the value of the row before it into one of [`PARTS`] buffers by its hash. Then each task counts the values of some parts, and the counts of the parts add up, since a value is in one part. `None` for a plan of other keys.
pub(super) fn distinct(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    plan: &Plan,
    rows: usize,
    segments: usize,
) -> Result<Option<(i64, Option<u64>)>> {
    let [Input::Column(place)] = plan.keys[..] else { return Ok(None) };
    let units = units_of(rows, segments);
    if units.is_empty() {
        return Ok(Some((0, None)));
    }
    let jobs = tasks.parallelism().min(rows / TASK_ROWS).clamp(1, units.len());
    let chunk = units.len().div_ceil(jobs);
    let mut runs: Vec<Option<Result<Values>>> = Vec::new();
    runs.resize_with(units.len().div_ceil(chunk), || None);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut runs)
        .map(|(units, out)| {
            Box::new(move || {
                *out = Some((|| {
                    let mut parts: Vec<Vec<u64>> = vec![Vec::new(); PARTS];
                    let mut decoded: Vec<Option<Decoded>> =
                        (0..stored.len()).map(|_| None).collect();
                    let mut sel = Vec::with_capacity(VECTOR);
                    let mut tests = Vec::with_capacity(plan.filters.len());
                    let mut first = None;
                    let mut previous = None;
                    for &unit in units {
                        if !select(stored, plan, rows, unit, &mut tests, &mut sel, &mut decoded)? {
                            continue;
                        }
                        let d = match decoded[place].take() {
                            Some(d) => d,
                            None => decode(&stored[place], unit.0, unit.1)?,
                        };
                        let base = (unit.0 * ROWS + unit.1 * VECTOR) as u64;
                        for &i in &sel {
                            if null(&d, i) {
                                continue;
                            }
                            first = first.or(Some(base + i as u64));
                            let v = int(&d, i) as u64;
                            // In ClickBench, 82% of the rows have the `UserID` of the row before them.
                            if previous != Some(v) {
                                parts[part_of(v)].push(v);
                                previous = Some(v);
                            }
                        }
                    }
                    Ok((parts, first))
                })());
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let runs: Vec<Values> = runs.into_iter().flatten().collect::<Result<_>>()?;
    let first = runs.iter().filter_map(|r| r.1).min();
    let mut by_part: Vec<Vec<Vec<u64>>> =
        (0..PARTS).map(|_| Vec::with_capacity(runs.len())).collect();
    for (parts, _) in runs {
        for (to, part) in by_part.iter_mut().zip(parts) {
            to.push(part);
        }
    }
    let per = PARTS.div_ceil(jobs);
    let mut counts = vec![0_i64; by_part.len().div_ceil(per)];
    let work: Vec<Job<'_>> = by_part
        .chunks_mut(per)
        .zip(&mut counts)
        .map(|(parts, out)| {
            Box::new(move || {
                let mut all = Vec::new();
                for part in parts {
                    all.clear();
                    for run in part.iter() {
                        all.extend_from_slice(run);
                    }
                    all.sort_unstable();
                    all.dedup();
                    *out += i64::try_from(all.len()).unwrap_or(i64::MAX);
                }
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    Ok(Some((counts.iter().sum(), first)))
}

/// The pairs of a group and a value of one run of vectors, by the part of the hash of the value; the values of the group of the null key; the first row of each group with a key that is not null; the first row of the group of the null key; and the last row of the run that passes the filters.
type Pairs = (Vec<Vec<(u64, u64)>>, Vec<u64>, FxMap<u64, u64>, Option<u64>, Option<u64>);

/// The groups of `plan` and the number of the values that are not null of its second key in each group, when the first key of the plan is the one key of `GROUP BY` and the second key is the argument of `count(DISTINCT)`. Each task reads a run of the vectors, keeps the first row of each group, and puts each pair of a group and a value that is not the pair of the row before it into one of [`PARTS`] buffers by the hash of the value. Then each task sorts the pairs of some parts, and counts the pairs that are not the same as the pair before them for each group. The groups are in the order of their first rows, as [`run`] gives them, with one count for each of the `calls` calls. `None` for a plan of other keys.
pub(super) fn distinct_groups(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    plan: &Plan,
    rows: usize,
    segments: usize,
    calls: usize,
) -> Result<Option<Found>> {
    let [Input::Column(key), Input::Column(place)] = plan.keys[..] else { return Ok(None) };
    let units = units_of(rows, segments);
    let jobs = tasks.parallelism().min(rows / TASK_ROWS).clamp(1, units.len().max(1));
    let chunk = units.len().div_ceil(jobs).max(1);
    let mut runs: Vec<Option<Result<Pairs>>> = Vec::new();
    runs.resize_with(units.len().div_ceil(chunk), || None);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut runs)
        .map(|(units, out)| {
            Box::new(move || {
                *out = Some((|| {
                    let mut parts: Vec<Vec<(u64, u64)>> = vec![Vec::new(); PARTS];
                    let mut nulls = Vec::new();
                    let mut firsts: FxMap<u64, u64> = FxMap::default();
                    let mut null_first = None;
                    let mut last = None;
                    let mut decoded: Vec<Option<Decoded>> =
                        (0..stored.len()).map(|_| None).collect();
                    let mut sel = Vec::with_capacity(VECTOR);
                    let mut tests = Vec::with_capacity(plan.filters.len());
                    // The group and the value of the row before, with `None` for null.
                    let mut previous: Option<(Option<u64>, Option<u64>)> = None;
                    for &unit in units {
                        if !select(stored, plan, rows, unit, &mut tests, &mut sel, &mut decoded)? {
                            continue;
                        }
                        let g = match decoded[key].take() {
                            Some(d) => d,
                            None => decode(&stored[key], unit.0, unit.1)?,
                        };
                        let d = match decoded[place].take() {
                            Some(d) => d,
                            None if place == key => decode(&stored[key], unit.0, unit.1)?,
                            None => decode(&stored[place], unit.0, unit.1)?,
                        };
                        let base = (unit.0 * ROWS + unit.1 * VECTOR) as u64;
                        last = sel.last().map(|&i| base + i as u64).or(last);
                        for &i in &sel {
                            let group = (!null(&g, i)).then(|| int(&g, i) as u64);
                            let value = (!null(&d, i)).then(|| int(&d, i) as u64);
                            // In ClickBench, most rows have the `RegionID` and the `UserID` of the row before them.
                            if previous == Some((group, value)) {
                                continue;
                            }
                            previous = Some((group, value));
                            let row = base + i as u64;
                            match group {
                                Some(group) => {
                                    firsts.entry(group).or_insert(row);
                                }
                                None => null_first = null_first.or(Some(row)),
                            }
                            match (group, value) {
                                (Some(group), Some(v)) => parts[part_of(v)].push((group, v)),
                                (None, Some(v)) => nulls.push(v),
                                (_, None) => {}
                            }
                        }
                    }
                    Ok((parts, nulls, firsts, null_first, last))
                })());
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let runs: Vec<Pairs> = runs.into_iter().flatten().collect::<Result<_>>()?;
    let last = runs.iter().filter_map(|r| r.4).max();
    let null_first = runs.iter().filter_map(|r| r.3).min();
    let mut firsts: FxMap<u64, u64> = FxMap::default();
    let mut nulls = Vec::new();
    let mut by_part: Vec<Vec<Vec<(u64, u64)>>> =
        (0..PARTS).map(|_| Vec::with_capacity(runs.len())).collect();
    for (parts, run_nulls, run_firsts, ..) in runs {
        for (group, first) in run_firsts {
            let at = firsts.entry(group).or_insert(first);
            *at = (*at).min(first);
        }
        nulls.extend(run_nulls);
        for (to, part) in by_part.iter_mut().zip(parts) {
            to.push(part);
        }
    }
    let per = PARTS.div_ceil(jobs);
    let mut counts: Vec<Vec<(u64, i64)>> = vec![Vec::new(); by_part.len().div_ceil(per)];
    let work: Vec<Job<'_>> = by_part
        .chunks_mut(per)
        .zip(&mut counts)
        .map(|(parts, out)| {
            Box::new(move || {
                let mut all = Vec::new();
                for part in parts {
                    all.clear();
                    for run in part.iter() {
                        all.extend_from_slice(run);
                    }
                    all.sort_unstable();
                    all.dedup();
                    for pair in all.chunk_by(|a, b| a.0 == b.0) {
                        out.push((pair[0].0, i64::try_from(pair.len()).unwrap_or(i64::MAX)));
                    }
                }
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let mut sums: FxMap<u64, i64> = FxMap::default();
    for (group, n) in counts.into_iter().flatten() {
        *sums.entry(group).or_insert(0) += n;
    }
    nulls.sort_unstable();
    nulls.dedup();
    let mut order: Vec<(u64, Option<u64>)> =
        firsts.into_iter().map(|(group, first)| (first, Some(group))).collect();
    order.extend(null_first.map(|first| (first, None)));
    order.sort_unstable();
    let mut words = Vec::with_capacity(order.len() * 2);
    let mut found_firsts = Vec::with_capacity(order.len());
    let mut found_counts = Vec::with_capacity(order.len());
    for &(first, group) in &order {
        words.extend_from_slice(&[group.unwrap_or(0), u64::from(group.is_none())]);
        found_firsts.push(first);
        found_counts.push(match group {
            Some(group) => sums.get(&group).copied().unwrap_or(0),
            None => i64::try_from(nulls.len()).unwrap_or(i64::MAX),
        });
    }
    // As on one task, the scan ends inside a group when its last row that passes is not the first row of a group.
    let trailing = last.is_some() && found_firsts.last() != last.as_ref();
    Ok(Some(Found {
        words,
        firsts: found_firsts,
        states: (0..calls).map(|_| Folded::Count(found_counts.clone())).collect(),
        extremes: Vec::new(),
        trailing,
    }))
}

/// The values of each part of the hashes for one run of vectors, and the first row of the run with a value.
type Values = (Vec<Vec<u64>>, Option<u64>);

/// The vectors of the scan of `rows` rows in `segments` segments, as their segment and their place in it.
fn units_of(rows: usize, segments: usize) -> Vec<(usize, usize)> {
    let mut units = Vec::new();
    for segment in 0..segments {
        let segment_rows = rows.saturating_sub(segment * ROWS).min(ROWS);
        units.extend((0..stored::vectors(segment_rows)).map(|vector| (segment, vector)));
    }
    units
}

/// The groups of the vectors `units` of the columns `stored`, which have `rows` rows.
fn scan_run(
    stored: &[Arc<Stored>],
    plan: &Plan,
    places: &[usize],
    units: &[(usize, usize)],
    rows: usize,
) -> Result<Run> {
    let width = plan.keys.len();
    let mut parts: Vec<Buf> = (0..PARTS).map(|_| Buf::new(&plan.folds, width + 1)).collect();
    let mut extremes: Vec<Option<(i64, i64)>> = vec![None; plan.bases];
    let first_base = plan.folds.len() - plan.bases;
    let mins: Vec<bool> = plan.folds.iter().map(|f| matches!(f, Fold::Min(_))).collect();
    let mut decoded: Vec<Option<Decoded>> = (0..stored.len()).map(|_| None).collect();
    let mut last = None;
    let mut sel: Vec<usize> = Vec::with_capacity(VECTOR);
    let mut keys: Vec<[u64; 4]> = Vec::with_capacity(VECTOR);
    let mut hashes: Vec<u64> = Vec::with_capacity(VECTOR);
    let mut ids: Vec<u32> = Vec::with_capacity(VECTOR);
    let mut tests: Vec<&(usize, Test)> = Vec::with_capacity(plan.filters.len());
    let mut local = Buf::new(&plan.folds, width + 1);
    let mut table = WordTable::<4>::new(LOCAL);
    // The rows that the table folded since it was last empty, and true when the rows go into the buffers with no table.
    let mut folded = 0;
    let mut direct = false;
    'units: for (n, &(segment, vector)) in units.iter().enumerate() {
        if !select(stored, plan, rows, (segment, vector), &mut tests, &mut sel, &mut decoded)? {
            continue 'units;
        }
        for &place in places {
            if !tests.iter().any(|t| t.0 == place) {
                decoded[place] = Some(decode(&stored[place], segment, vector)?);
            }
        }
        let base = (segment * ROWS + vector * VECTOR) as u64;
        last = sel.last().map(|&i| base + i as u64);
        keys.clear();
        keys.resize(sel.len(), [0; 4]);
        for (k, input) in plan.keys.iter().enumerate() {
            match *input {
                Input::Column(place) => {
                    let Some(d) = decoded[place].as_ref() else { continue };
                    for (key, &i) in keys.iter_mut().zip(&sel) {
                        if null(d, i) {
                            key[width] |= 1 << k;
                        } else {
                            key[k] = int(d, i) as u64;
                        }
                    }
                }
                Input::Floor(place, step) => {
                    let Some(d) = decoded[place].as_ref() else { continue };
                    for (key, &i) in keys.iter_mut().zip(&sel) {
                        if null(d, i) {
                            key[width] |= 1 << k;
                        } else {
                            let v = int(d, i);
                            let infinite = v == i64::MIN || v == i64::MAX;
                            key[k] = if infinite { v } else { v - v.rem_euclid(step) } as u64;
                        }
                    }
                }
                Input::Const(Some(word)) => keys.iter_mut().for_each(|key| key[k] = word),
                Input::Const(None) => keys.iter_mut().for_each(|key| key[width] |= 1 << k),
            }
        }
        hashes.clear();
        hashes.extend(keys.iter().map(WordTable::<4>::hash));
        for base_fold in first_base..plan.folds.len() {
            if let Fold::Sum(place) = plan.folds[base_fold]
                && let Some(d) = decoded[place].as_ref()
            {
                let e = &mut extremes[base_fold - first_base];
                for &i in &sel {
                    if !null(d, i) {
                        let v = int(d, i);
                        *e = Some(e.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
                    }
                }
            }
        }
        if direct {
            for ((key, &hash), &i) in keys.iter().zip(&hashes).zip(&sel) {
                let part = &mut parts[part_of(hash)];
                part.push(key, base + i as u64);
                for (state, &fold) in part.states.iter_mut().zip(&plan.folds) {
                    state.push_cell(cell(fold, &decoded, i));
                }
            }
            continue;
        }
        if table.room() < sel.len() {
            // A table that folds fewer than two rows into each group does not pay for its probes.
            direct = folded < 2 * local.len();
            flush(&mut local, &mut table, &mut parts);
            folded = 0;
            if direct {
                // Each part gets about its share of the rows that are left, so its buffers do not grow many times.
                let more = (units.len() - n) * VECTOR / PARTS;
                for part in &mut parts {
                    part.reserve(more + more / 8);
                }
                for ((key, &hash), &i) in keys.iter().zip(&hashes).zip(&sel) {
                    let part = &mut parts[part_of(hash)];
                    part.push(key, base + i as u64);
                    for (state, &fold) in part.states.iter_mut().zip(&plan.folds) {
                        state.push_cell(cell(fold, &decoded, i));
                    }
                }
                continue;
            }
        }
        folded += sel.len();
        ids.clear();
        for ((key, &hash), &i) in keys.iter().zip(&hashes).zip(&sel) {
            let next = local.len() as u32;
            let (id, new) = table.id(key, hash, next);
            if new {
                local.push(key, base + i as u64);
                for state in &mut local.states {
                    state.push();
                }
            }
            ids.push(id);
        }
        for ((state, &fold), &min) in local.states.iter_mut().zip(&plan.folds).zip(&mins) {
            if matches!(fold, Fold::None) {
                continue;
            }
            for (&id, &i) in ids.iter().zip(&sel) {
                state.fold_cell(id as usize, cell(fold, &decoded, i), min);
            }
        }
    }
    flush(&mut local, &mut table, &mut parts);
    Ok(Run { parts, extremes, last })
}

/// The groups of one part from its buffers `bufs` of all the runs, which come in the order of the runs.
fn merge(bufs: Vec<Buf>, folds: &[Fold], mins: &[bool], width: usize) -> Buf {
    let mut merged = Buf::new(folds, width);
    let size: usize = bufs.iter().map(Buf::len).sum();
    // The part has at most `size` groups, so its buffers do not grow again.
    merged.reserve(size);
    let mut table = WordTable::<4>::new(size);
    let mut hashes = Vec::new();
    for from in bufs {
        hashes.clear();
        hashes.extend((0..from.len()).map(|g| WordTable::<4>::hash(&from.key(g))));
        for (g, &hash) in hashes.iter().enumerate() {
            if let Some(&ahead) = hashes.get(g + PREFETCH) {
                table.prefetch(ahead);
            }
            let next = merged.len() as u32;
            let key = from.key(g);
            let (id, new) = table.id(&key, hash, next);
            if new {
                merged.push(&key, from.firsts[g]);
                for (to, other) in merged.states.iter_mut().zip(&from.states) {
                    to.take(other, g);
                }
            } else {
                for ((to, other), &min) in merged.states.iter_mut().zip(&from.states).zip(mins) {
                    to.merge(id as usize, other, g, min);
                }
            }
        }
    }
    merged
}

/// The groups of the scan of the columns `stored`, which have `rows` rows in `segments` segments, on the tasks `tasks`.
///
/// # Errors
///
/// An error of the decode of a vector.
pub(super) fn run(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    plan: &Plan,
    rows: usize,
    segments: usize,
) -> Result<Found> {
    let mut places: Vec<usize> = plan
        .keys
        .iter()
        .filter_map(|k| match k {
            Input::Column(place) | Input::Floor(place, _) => Some(*place),
            Input::Const(_) => None,
        })
        .chain(plan.folds.iter().filter_map(|f| match f {
            Fold::CountArg(p) | Fold::Sum(p) | Fold::Min(p) | Fold::Max(p) => Some(*p),
            Fold::Count | Fold::None => None,
        }))
        .collect();
    places.sort_unstable();
    places.dedup();
    let units = units_of(rows, segments);
    if units.is_empty() {
        return Ok(Found {
            words: Vec::new(),
            firsts: Vec::new(),
            states: plan.folds.iter().map(|&f| Folded::of(f)).collect(),
            extremes: vec![None; plan.bases],
            trailing: false,
        });
    }
    let jobs = tasks.parallelism().min(rows / TASK_ROWS).clamp(1, units.len());
    let chunk = units.len().div_ceil(jobs);
    let mut runs: Vec<Option<Result<Run>>> = Vec::new();
    runs.resize_with(units.len().div_ceil(chunk), || None);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut runs)
        .map(|(units, out)| {
            let places = &places;
            Box::new(move || *out = Some(scan_run(stored, plan, places, units, rows))) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let runs: Vec<Run> = runs.into_iter().flatten().collect::<Result<_>>()?;
    let mut extremes = vec![None; plan.bases];
    let mut last = None;
    let mut by_part: Vec<Vec<Buf>> = (0..PARTS).map(|_| Vec::with_capacity(runs.len())).collect();
    for run in runs {
        for (e, r) in extremes.iter_mut().zip(&run.extremes) {
            if let Some((lo, hi)) = *r {
                *e = Some(e.map_or((lo, hi), |(a, b): (i64, i64)| (a.min(lo), b.max(hi))));
            }
        }
        last = run.last.or(last);
        for (to, buf) in by_part.iter_mut().zip(run.parts) {
            to.push(buf);
        }
    }
    let mins: Vec<bool> = plan.folds.iter().map(|f| matches!(f, Fold::Min(_))).collect();
    let width = plan.keys.len() + 1;
    let mut parts: Vec<Option<Buf>> = (0..PARTS).map(|_| None).collect();
    let per = PARTS.div_ceil(jobs);
    let work: Vec<Job<'_>> = parts
        .chunks_mut(per)
        .zip(by_part.chunks_mut(per))
        .map(|(out, bufs)| {
            let mins = &mins;
            Box::new(move || {
                for (slot, bufs) in out.iter_mut().zip(bufs) {
                    *slot = Some(merge(std::mem::take(bufs), &plan.folds, mins, width));
                }
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let parts: Vec<Buf> = parts.into_iter().flatten().collect();
    // The place of each group is the number of first rows before its first row, which a map of the first rows gives.
    let mut map = vec![0_u64; rows / 64 + 1];
    for part in &parts {
        for &first in &part.firsts {
            map[(first / 64) as usize] |= 1 << (first % 64);
        }
    }
    let mut before = Vec::with_capacity(map.len());
    let mut count = 0_u32;
    for word in &map {
        before.push(count);
        count += word.count_ones();
    }
    let groups = count as usize;
    let mut order: Vec<(u32, u32)> = vec![(0, 0); groups];
    for (p, part) in parts.iter().enumerate() {
        for (g, &first) in part.firsts.iter().enumerate() {
            let (word, bit) = ((first / 64) as usize, first % 64);
            let place = before[word] + (map[word] & ((1 << bit) - 1)).count_ones();
            order[place as usize] = (p as u32, g as u32);
        }
    }
    drop(map);
    drop(before);
    // The tasks copy the keys, the first rows and the states of the groups in their order, each task a run of the groups.
    let mut words = vec![0_u64; groups * width];
    let mut firsts = vec![0_u64; groups];
    let mut states: Vec<Folded> = plan
        .folds
        .iter()
        .map(|&fold| match fold {
            Fold::Count | Fold::CountArg(_) => Folded::Count(vec![0; groups]),
            Fold::Sum(_) => Folded::Sum(vec![(0, 0); groups]),
            Fold::Min(_) | Fold::Max(_) => Folded::Extreme(vec![None; groups]),
            Fold::None => Folded::None,
        })
        .collect();
    let chunk = groups.div_ceil(jobs).max(1);
    let mut columns: Vec<Vec<Column<'_>>> =
        (0..groups.div_ceil(chunk)).map(|_| Vec::new()).collect();
    for state in &mut states {
        match state {
            Folded::Count(v) => {
                columns
                    .iter_mut()
                    .zip(v.chunks_mut(chunk))
                    .for_each(|(c, v)| c.push(Column::Count(v)));
            }
            Folded::Sum(v) => {
                columns
                    .iter_mut()
                    .zip(v.chunks_mut(chunk))
                    .for_each(|(c, v)| c.push(Column::Sum(v)));
            }
            Folded::Extreme(v) => {
                columns
                    .iter_mut()
                    .zip(v.chunks_mut(chunk))
                    .for_each(|(c, v)| c.push(Column::Extreme(v)));
            }
            Folded::None => columns.iter_mut().for_each(|c| c.push(Column::None)),
        }
    }
    let work: Vec<Job<'_>> = order
        .chunks(chunk)
        .zip(words.chunks_mut(chunk * width))
        .zip(firsts.chunks_mut(chunk))
        .zip(columns)
        .map(|(((order, words), firsts), mut columns)| {
            let parts = &parts;
            Box::new(move || {
                for (n, &(p, g)) in order.iter().enumerate() {
                    let (part, g) = (&parts[p as usize], g as usize);
                    // A loop of at most 4 words, which costs less than a call of `memcpy` for each group.
                    for (to, &word) in
                        words[n * width..(n + 1) * width].iter_mut().zip(&part.key(g))
                    {
                        *to = word;
                    }
                    firsts[n] = part.firsts[g];
                    for (to, from) in columns.iter_mut().zip(&part.states) {
                        match (to, from) {
                            (Column::Count(a), Folded::Count(b)) => a[n] = b[g],
                            (Column::Sum(a), Folded::Sum(b)) => a[n] = b[g],
                            (Column::Extreme(a), Folded::Extreme(b)) => a[n] = b[g],
                            _ => {}
                        }
                    }
                }
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    // As on one task, the scan ends inside a group when its last row that passes is not the first row of a group.
    let trailing = last.is_some() && firsts.last() != last.as_ref();
    Ok(Found { words, firsts, states, extremes, trailing })
}

/// A run of the states of one fold in the order of the groups, which one task fills.
enum Column<'a> {
    Count(&'a mut [i64]),
    Sum(&'a mut [(i64, i128)]),
    Extreme(&'a mut [Option<i64>]),
    None,
}

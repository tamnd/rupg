//! The top of the rows of a vector scan with `ORDER BY` and `LIMIT` on the tasks of the server (spec/14 section 14.5), when each target is a key of the order. Rows that are equal on all the keys are then the same row of the result, so the rows that the bounded heap of `tuplesort.c` keeps from the equal rows do not change the result, and each task can keep its own top. Each task reads a run of the vectors, and keeps the rows that pass the conditions and that come before the last row of its top. Then the tops of the tasks merge, and the first rows of all of them are the top of the scan.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rupg_column::segment::ROWS;
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::Result;
use rupg_platform::{Job, Tasks};
use rupg_summary::zone::{Test, Verdict};

use super::groups::{self, Plan, TASK_ROWS};
use super::{Decoded, decode};

/// The shared first word when no task has a full top. A first word of all ones, the word of the least `bigint` in a descending order, does not then cut any row.
const NO_CUT: u64 = u64::MAX;

/// The most keys of the order.
pub(super) const KEYS: usize = 4;

/// A key of the order: a column of integers or of text codes by its place in the columns of the scan.
pub(super) struct Key<'a> {
    pub(super) place: usize,
    /// For a column of text codes: the place of each code in the order of the bytes of the texts, as [`rupg_column::dict::Dictionary::order`] gives it.
    pub(super) order: Option<&'a [u32]>,
    pub(super) descending: bool,
    pub(super) nulls_first: bool,
}

/// A row of a top: the word of each key in the order of the keys, and the value of each key, which is `None` for null and else the integer or the code.
#[derive(Clone, Copy)]
pub(super) struct Entry {
    words: [u128; KEYS],
    pub(super) values: [Option<u64>; KEYS],
}

/// The word of the value `value` of a key, or of null: words in the order of the key compare as their values do. The high part puts a null before or after all the values.
#[inline]
fn word(key: &Key<'_>, value: Option<u64>) -> u128 {
    match value {
        None => {
            if key.nulls_first {
                0
            } else {
                2 << 64
            }
        }
        Some(v) => {
            let w = match key.order {
                Some(order) => u64::from(order.get(v as usize).copied().unwrap_or(u32::MAX)),
                None => rupg_summary::key::int(v as i64),
            };
            (1 << 64) | u128::from(if key.descending { !w } else { w })
        }
    }
}

/// The value of the row `i` of a decoded vector of a key: `None` for null, else the integer or the code.
#[inline]
fn value(d: &Decoded, i: usize) -> Option<u64> {
    (!groups::null(d, i)).then(|| groups::int(d, i) as u64)
}

/// True when the zones of the vector `unit` of the first key `key`, a key of integers, show that each row of the vector comes after a row whose first word is `lead`, as the cut of `each_cut` does (spec/12 section 12.6.1). A row with the same value as `lead` can come before the row, since its other keys can put it first. A null comes after it only if the nulls come last, or the vector has no nulls.
fn after(
    stored: &[Arc<Stored>],
    key: &Key<'_>,
    lead: u128,
    rows: usize,
    unit: (usize, usize),
) -> bool {
    if key.order.is_some() || lead >> 64 != 1 {
        return false;
    }
    let word = lead as u64;
    let test = if key.descending { Test::Ge(!word) } else { Test::Le(word) };
    let (segment, vector) = unit;
    let segment_rows = rows.saturating_sub(segment * ROWS).min(ROWS);
    let zones = &stored[key.place].segments[segment].summary.vectors;
    if zones.len() != stored::vectors(segment_rows) {
        return false;
    }
    let zone = &zones[vector];
    matches!(zone.test(&test), Verdict::None)
        && (!key.nulls_first || matches!(zone.test(&Test::IsNull), Verdict::None))
}

/// Keeps in `top` the `bound` first rows of `top` in the order of the words, in that order.
fn cut(top: &mut Vec<Entry>, bound: usize) {
    if top.len() > bound {
        top.select_nth_unstable_by(bound - 1, |a, b| a.words.cmp(&b.words));
        top.truncate(bound);
    }
    top.sort_unstable_by_key(|a| a.words);
}

/// The `bound` first rows, in the order of `keys`, of the rows of the columns `stored`, which have `rows` rows in `segments` segments, that pass the tests `filters`. Two rows with the same words of the keys can come in any order.
pub(super) fn top(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    filters: &[(usize, Test)],
    keys: &[Key<'_>],
    rows: usize,
    segments: usize,
    bound: usize,
) -> Result<Vec<Entry>> {
    assert!(!keys.is_empty() && keys.len() <= KEYS && bound > 0, "a top has 1 to 4 keys");
    let plan = Plan {
        keys: Vec::new(),
        folds: Vec::new(),
        bases: 0,
        filters: filters.to_vec(),
        maps: Vec::new(),
    };
    let units = groups::units_of(rows, segments);
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let jobs = tasks.parallelism().min(rows / TASK_ROWS).clamp(1, units.len());
    let chunk = units.len().div_ceil(jobs);
    let mut runs: Vec<Option<Result<Vec<Entry>>>> = Vec::new();
    runs.resize_with(units.len().div_ceil(chunk), || None);
    let plan = &plan;
    let shared = &AtomicU64::new(NO_CUT);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut runs)
        .map(|(units, out)| {
            Box::new(move || {
                *out = Some(run(stored, plan, keys, units, rows, bound, shared));
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let mut all = Vec::new();
    for run in runs.into_iter().flatten() {
        all.extend(run?);
    }
    cut(&mut all, bound);
    Ok(all)
}

/// The top of one task, for the vectors `units`.
fn run(
    stored: &[Arc<Stored>],
    plan: &Plan,
    keys: &[Key<'_>],
    units: &[(usize, usize)],
    rows: usize,
    bound: usize,
    shared: &AtomicU64,
) -> Result<Vec<Entry>> {
    let mut decoded: Vec<Option<Decoded>> = (0..stored.len()).map(|_| None).collect();
    let mut sel = Vec::with_capacity(VECTOR);
    let mut tests = Vec::with_capacity(plan.filters.len());
    let mut top: Vec<Entry> = Vec::with_capacity(2 * bound + VECTOR);
    // The words of the last row of the top, after which a row cannot go in the top.
    let mut last: Option<[u128; KEYS]> = None;
    // The size at which the top is cut. The first cut comes when the top has `bound` rows, so that the zones of the first key skip vectors early, and after that the cuts come in larger steps.
    let mut limit = bound;
    let (lead, rest) = keys.split_first().unwrap_or_else(|| unreachable!());
    for &unit in units {
        // The first word after which a row cannot go in the top: the first word of the last row of this top, or of the top of another task.
        let other = match shared.load(Ordering::Relaxed) {
            NO_CUT => None,
            word => Some((1 << 64) | u128::from(word)),
        };
        let cut_word = match (last, other) {
            (Some(last), Some(other)) => Some(last[0].min(other)),
            (Some(last), None) => Some(last[0]),
            (None, other) => other,
        };
        if cut_word.is_some_and(|word| after(stored, lead, word, rows, unit)) {
            continue;
        }
        if !groups::select(stored, plan, rows, unit, &mut tests, &mut sel, &mut decoded)? {
            continue;
        }
        // The filters of this vector decoded the columns that they tested. A column that they did not test can keep a vector of an earlier unit, so the scan decodes it again.
        let mut columns: Vec<Decoded> = Vec::with_capacity(keys.len());
        for key in keys {
            let tested = tests.iter().any(|t| t.0 == key.place);
            columns.push(match decoded[key.place].take().filter(|_| tested) {
                Some(d) => d,
                None => decode(&stored[key.place], unit.0, unit.1)?,
            });
        }
        let first = &columns[0];
        for &i in &sel {
            let v = value(first, i);
            let w = word(lead, v);
            // Most rows come after the last row of the top by the first key, and the scan makes no other word of them. A row equal to the last row of a full top is the same row of the result, so it is not kept.
            if cut_word.is_some_and(|word| w > word) {
                continue;
            }
            let mut entry = Entry { words: [0; KEYS], values: [None; KEYS] };
            entry.words[0] = w;
            entry.values[0] = v;
            for (k, (key, d)) in rest.iter().zip(&columns[1..]).enumerate() {
                let v = value(d, i);
                entry.words[k + 1] = word(key, v);
                entry.values[k + 1] = v;
            }
            if last.is_some_and(|last| entry.words >= last) {
                continue;
            }
            top.push(entry);
            if top.len() >= limit {
                cut(&mut top, bound);
                last = top.last().map(|e| e.words);
                limit = 2 * bound + VECTOR;
                if let Some(word) = last.map(|last| last[0]).filter(|word| word >> 64 == 1) {
                    shared.fetch_min(word as u64, Ordering::Relaxed);
                }
            }
        }
    }
    cut(&mut top, bound);
    Ok(top)
}

//! The groups of a vector scan on the tasks of the server (spec/14 section 14.5), as a hash aggregate with parts of the hashes. Each task reads a run of the vectors and keeps the rows that pass the conditions. It folds the rows into a small table that stays in the cache, and when the table is full, it moves the groups of the table into [`PARTS`] buffers, one for each part of the hashes of the keys. When the table folds few rows into each group, the task puts each row into the buffers with no table. Then each task merges the buffers of some parts, from the buffers of the first run to the last, so the first row of each group is its first row in the scan. The groups then go in the order of their first rows, which is the order of the scan on one task.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use rupg_column::segment::{Kind, ROWS};
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::Result;
use rupg_kernels::hash::prefetch;
use rupg_platform::{Job, Tasks};
use rupg_summary::zone::{Test, Verdict, has_code};

use super::{Decoded, DecodedData, FxMap, WordTable, decode, passes};

/// The number of parts of the hashes of the keys.
const PARTS: usize = 256;

/// The most groups of the table of a task before the task moves the groups into the buffers of the parts. The table then fits in the cache of one core.
const LOCAL: usize = 1 << 13;

/// The least number of rows of the scan for each group for which [`run`] puts the groups in the order of their first rows with a sort of the first rows, in place of a map of one bit for each row. The sort reads each group, and the map clears and reads one word for 64 rows.
const SORT_GROUPS: usize = 1024;

/// The rows of a vector after a merge of the buffers, by which a probe asks for its slot first.
const PREFETCH: usize = 16;

/// The least number of rows for each group, by the estimate of the plan, for the scan to find the groups at the same time. With fewer rows for each group, the table of each task keeps few rows out, and the parts, their merge and their order cost more CPU time than the scan of one task: on 2M rows of ClickBench, 1 to 7 rows for each group took 1.4 to 2 times the CPU time, and 9 rows for each group took less.
pub(super) const GROUP_ROWS: usize = 8;

/// The least number of rows that pass the conditions for each group, by the estimates of the plan, for the scan to find the groups at the same time. The tasks read all the rows of the table, but each row that passes goes into the parts: on 2M rows of ClickBench, `WHERE SearchPhrase <> ''` keeps about 200000 rows, and q31 with 1.5 rows for each group and q32 with 1 row took 1.3 to 1.5 times the CPU time on the tasks, but q15 with 2.9 rows for each group took 0.8 times.
pub(super) const GROUP_INPUT: usize = 2;

/// The fewest rows for each task, so that a small table does not pay for the tasks.
pub(super) const TASK_ROWS: usize = 1 << 16;

/// A key of `GROUP BY`: a column of integers or codes by its place in the columns of the scan, a column of integers that each value rounds down to a multiple of a step, a choice of a column or a constant word, or a constant word, with `None` for null.
#[derive(Clone)]
pub(super) enum Input {
    Column(usize),
    /// `date_trunc` of a column of timestamps to a unit of `step` microseconds. An infinite timestamp keeps its value, as in `timestamp_trunc`.
    Floor(usize, i64),
    /// `extract` of a field from a column of timestamps: the number of units of `step` microseconds, modulo `span`. An infinite timestamp gives null, as in `extract_timestamp`.
    Part(usize, i64, i64),
    /// `CASE WHEN tests THEN column ELSE default END`: the word of the column `then` for a row that passes all the tests, and the word `default` for the other rows.
    Case {
        tests: Vec<(usize, Test)>,
        then: usize,
        default: Option<u64>,
    },
    Const(Option<u64>),
}

impl Input {
    /// The places of the columns that the key reads.
    fn places(&self) -> Vec<usize> {
        match self {
            Input::Column(place) | Input::Floor(place, _) | Input::Part(place, ..) => vec![*place],
            Input::Case { tests, then, .. } => {
                tests.iter().map(|t| t.0).chain(std::iter::once(*then)).collect()
            }
            Input::Const(_) => Vec::new(),
        }
    }
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
    /// The conditions of the scan, each a test of one column by its place. A test other than a null test is on a column of integers, or an equal or not equal test on a column of text codes.
    pub(super) filters: Vec<(usize, Test)>,
    /// The columns of integers that the scan computes from columns of codes. The place of a map is the number of the stored columns plus its index here, and a key or a fold reads it as a stored column of integers.
    pub(super) maps: Vec<Map>,
}

/// A column of integers from a column of codes: the value of a row is the value of its code, and a row with a null code is null. The values are not negative.
#[derive(Clone)]
pub(super) struct Map {
    /// The place of the column of codes.
    pub(super) from: usize,
    /// The value of each code, at the index of the code.
    pub(super) values: Arc<[i64]>,
}

impl Plan {
    /// The place of the stored column that the scan decodes for the column at `place`: the column of codes of a map, or else the column.
    fn source(&self, stored: usize, place: usize) -> usize {
        place.checked_sub(stored).and_then(|m| self.maps.get(m)).map_or(place, |m| m.from)
    }
}

/// The decoded vector of the map `map` from the decoded vector of its column of codes `codes`.
fn map_vector(map: &Map, codes: &Decoded) -> Decoded {
    let value = |code: u32| map.values.get(code as usize).copied().unwrap_or(0);
    let data = match &codes.data {
        DecodedData::Code(v) => v.iter().map(|&c| value(c)).collect(),
        DecodedData::Int(v) => v.iter().map(|&c| u32::try_from(c).map_or(0, value)).collect(),
        DecodedData::Float(v) => vec![0; v.len()],
    };
    Decoded { data: DecodedData::Int(data), nulls: codes.nulls.clone() }
}

/// The groups of a scan, in the order of their first rows, or in any order when the scan does not need it.
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
    /// True when the groups are only the groups that can change the bounded heap of the sort of [`Top`], in the order of their first rows, which [`heap_groups`] gives.
    pub(super) heap: bool,
}

impl Found {
    /// Gives the groups of a plan with no keys one group, with the states of no rows, when no row passes the conditions, as an aggregate with no `GROUP BY` gives one row.
    pub(super) fn or_one(&mut self) {
        if self.firsts.is_empty() {
            self.words = vec![0];
            self.firsts = vec![0];
            for state in &mut self.states {
                state.push();
            }
        }
    }
}

/// Groups with their packed keys of [`Pack`], the first row and the states of each group, in the order of their first rows. Each key has `width` words, so that a buffer keeps only the words that the plan uses. The hash of a key is not kept: to compute it again costs less than to write it and to read it again.
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
    fn key<const W: usize>(&self, g: usize) -> [u64; W] {
        // The loop has a known number of steps, so the copy is not a call of `memcpy`.
        let words = &self.keys[g * self.width..(g + 1) * self.width];
        std::array::from_fn(|i| words.get(i).copied().unwrap_or(0))
    }

    /// Starts the reads of the key, the first row and the states of the group `g`.
    #[inline]
    fn prefetch(&self, g: usize) {
        if let Some(word) = self.keys.get(g * self.width) {
            prefetch(word);
        }
        if let Some(first) = self.firsts.get(g) {
            prefetch(first);
        }
        for state in &self.states {
            match state {
                Folded::Count(v) => v.get(g).map(prefetch),
                Folded::Sum(v) => v.get(g).map(prefetch),
                Folded::Extreme(v) => v.get(g).map(prefetch),
                Folded::None => None,
            };
        }
    }

    #[inline]
    fn push<const W: usize>(&mut self, key: &[u64; W], first: u64) {
        match self.width {
            1 => self.keys.push(key[0]),
            2 => self.keys.extend_from_slice(&key[..2]),
            3 => self.keys.extend_from_slice(&key[..3]),
            // A copy of a known number of words is not a call of `memcpy`.
            4 => self.keys.extend_from_slice(&key[..4]),
            width => self.keys.extend(key.iter().take(width)),
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

/// The bits of a value or of a null flag of a key in the words of a packed key.
#[derive(Clone, Copy)]
struct Field {
    word: usize,
    shift: u32,
    bits: u32,
    /// True when the bits are the low bits of a signed integer, which a read extends with its sign.
    signed: bool,
}

impl Field {
    #[inline]
    fn mask(self) -> u64 {
        if self.bits >= 64 { u64::MAX } else { (1 << self.bits) - 1 }
    }

    /// Puts the low bits of `value` into the field of `key`, whose bits are 0.
    #[inline]
    fn put(self, key: &mut [u64], value: u64) {
        key[self.word] |= (value & self.mask()) << self.shift;
    }

    #[inline]
    fn get(self, key: &[u64]) -> u64 {
        let v = (key[self.word] >> self.shift) & self.mask();
        if self.signed && self.bits < 64 {
            let rest = 64 - self.bits;
            ((v << rest) as i64 >> rest) as u64
        } else {
            v
        }
    }
}

/// Where a key of a plan is in a packed key.
#[derive(Clone, Copy)]
enum Place {
    /// A key with the same word for each row, or null for each row with `None`, which has no bits.
    Const(Option<u64>),
    /// The bits of the value, and the flag of a null when the key can be null.
    Bits(Field, Option<Field>),
}

/// The bits of the word of a value of the kind `kind`, and true when the word is a signed value.
fn bits_of(kind: Kind) -> (u32, bool) {
    match kind {
        Kind::Bool => (1, false),
        Kind::Int2 => (16, true),
        Kind::Int4 | Kind::Date => (32, true),
        // A code of a dictionary is a `u32`.
        Kind::Text | Kind::Bytea => (32, false),
        _ => (64, false),
    }
}

/// The keys of a plan in fewer words. Each key keeps only the bits that its type can have, and a flag of one bit for a null only when its column has a null, so that `UserID`, the minute of `EventTime` and the code of `SearchPhrase` fit in 2 words in place of 4. A smaller key makes a smaller slot in each table, and a hash, a compare and a copy of fewer words. The tasks keep the packed keys, and [`Pack::unpack`] gives the words of the keys and the mask of nulls of each group of the result.
struct Pack {
    places: Vec<Place>,
    /// The words of a packed key, at least 1.
    width: usize,
}

impl Pack {
    fn new(stored: &[Arc<Stored>], plan: &Plan) -> Pack {
        let nulls = |place: usize| stored[place].segments.iter().any(|s| s.summary.nulls > 0);
        // The bits of the value of each key, true for a signed value, and true when the key can be null.
        let shapes: Vec<Option<(u32, bool, bool)>> = plan
            .keys
            .iter()
            .map(|input| match *input {
                // A map has values that are not negative, and nulls where its column of codes has nulls.
                Input::Column(place) if place >= stored.len() => {
                    Some((64, false, nulls(plan.source(stored.len(), place))))
                }
                Input::Column(place) => {
                    let (bits, signed) = bits_of(stored[place].kind);
                    Some((bits, signed, nulls(place)))
                }
                Input::Floor(place, _) => Some((64, false, nulls(place))),
                // The value is from 0 to `span - 1`, and an infinite timestamp gives null.
                Input::Part(_, _, span) => {
                    let most = u64::try_from(span - 1).unwrap_or(u64::MAX);
                    Some(((64 - most.leading_zeros()).max(1), false, true))
                }
                Input::Case { .. } => Some((64, false, true)),
                Input::Const(_) => None,
            })
            .collect();
        // The fields go into the words largest first, each into the first word with room, so the words are few.
        let mut fields: Vec<(u32, usize, bool)> = Vec::new();
        for (k, shape) in shapes.iter().enumerate() {
            if let Some((bits, _, null)) = *shape {
                fields.push((bits, k, false));
                if null {
                    fields.push((1, k, true));
                }
            }
        }
        fields.sort_by_key(|&(bits, ..)| Reverse(bits));
        let mut used: Vec<u32> = Vec::new();
        let mut values: Vec<Option<Field>> = vec![None; plan.keys.len()];
        let mut flags: Vec<Option<Field>> = vec![None; plan.keys.len()];
        for (bits, k, null) in fields {
            let word = used.iter().position(|&u| u + bits <= 64).unwrap_or_else(|| {
                used.push(0);
                used.len() - 1
            });
            let signed = !null && shapes[k].is_some_and(|s| s.1);
            let field = Field { word, shift: used[word], bits, signed };
            used[word] += bits;
            if null {
                flags[k] = Some(field);
            } else {
                values[k] = Some(field);
            }
        }
        let places = plan
            .keys
            .iter()
            .zip(values.into_iter().zip(flags))
            .map(|(input, (value, flag))| match (input, value) {
                (_, Some(value)) => Place::Bits(value, flag),
                (Input::Const(word), None) => Place::Const(*word),
                (_, None) => Place::Const(None),
            })
            .collect();
        Pack { places, width: used.len().max(1) }
    }

    /// Puts the word `value` of the key `k` into the packed key `key`.
    #[inline]
    fn put(&self, key: &mut [u64], k: usize, value: u64) {
        if let Place::Bits(field, _) = self.places[k] {
            field.put(key, value);
        }
    }

    /// Puts a null of the key `k` into the packed key `key`.
    #[inline]
    fn put_null(&self, key: &mut [u64], k: usize) {
        if let Place::Bits(_, Some(flag)) = self.places[k] {
            flag.put(key, 1);
        }
    }

    /// The word of the key `k` of the packed key `packed`, or `None` for a null.
    #[inline]
    fn value(&self, packed: &[u64], k: usize) -> Option<u64> {
        match self.places[k] {
            Place::Const(word) => word,
            Place::Bits(value, flag) => {
                (!flag.is_some_and(|f| f.get(packed) != 0)).then(|| value.get(packed))
            }
        }
    }

    /// The words of the keys and the mask of nulls of the packed key `packed`, into `out`, which has a word for each key and the mask.
    #[inline]
    fn unpack(&self, packed: &[u64], out: &mut [u64]) {
        let mut mask = 0;
        for (k, (place, to)) in self.places.iter().zip(out.iter_mut()).enumerate() {
            match *place {
                Place::Const(Some(word)) => *to = word,
                Place::Const(None) => {
                    *to = 0;
                    mask |= 1 << k;
                }
                Place::Bits(value, flag) => {
                    *to = value.get(packed);
                    if flag.is_some_and(|f| f.get(packed) != 0) {
                        mask |= 1 << k;
                    }
                }
            }
        }
        out[self.places.len()] = mask;
    }
}

pub(super) fn null(decoded: &Decoded, i: usize) -> bool {
    decoded.nulls.as_ref().is_some_and(|n| n[i])
}

pub(super) fn int(decoded: &Decoded, i: usize) -> i64 {
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

/// Folds the rows `sel` of a vector into the states `state` of their groups `ids`, for the fold `fold`. The match on the fold and on its column is once for each vector, not once for each row. A column that is not decoded gives no value, as [`cell`] gives it.
#[inline]
fn fold_rows(
    state: &mut Folded,
    fold: Fold,
    ids: &[u32],
    sel: &[usize],
    decoded: &[Option<Decoded>],
    min: bool,
) {
    let rows = ids.iter().map(|&id| id as usize).zip(sel.iter().copied());
    match (state, fold) {
        (Folded::Count(a), Fold::Count) => ids.iter().for_each(|&id| a[id as usize] += 1),
        (Folded::Count(a), Fold::CountArg(place)) => {
            let Some(d) = decoded[place].as_ref() else { return };
            match &d.nulls {
                None => ids.iter().for_each(|&id| a[id as usize] += 1),
                Some(nulls) => rows.for_each(|(id, i)| a[id] += i64::from(!nulls[i])),
            }
        }
        (Folded::Sum(a), Fold::Sum(place)) => {
            let Some(d) = decoded[place].as_ref() else { return };
            for (id, i) in rows {
                if !null(d, i) {
                    a[id].0 += 1;
                    a[id].1 += i128::from(int(d, i));
                }
            }
        }
        (Folded::Extreme(a), Fold::Min(place) | Fold::Max(place)) => {
            let Some(d) = decoded[place].as_ref() else { return };
            for (id, i) in rows {
                if !null(d, i) {
                    let v = int(d, i);
                    a[id] = Some(a[id].map_or(v, |s| if min { s.min(v) } else { s.max(v) }));
                }
            }
        }
        _ => {}
    }
}

/// Puts each run of rows `sel` of a vector with equal keys of `keys` into the buffer of its part as a new group, with no table. The rows of the vector start at the row `base` of the scan. The rows of a table often come in the order of a key, so a run has many rows, and a count of a run is its number of rows. `runs` is room for the first row, the rows and the part of each run.
fn push_direct<const W: usize>(
    parts: &mut [Buf],
    folds: &[Fold],
    keys: &[[u64; W]],
    sel: &[usize],
    base: u64,
    decoded: &[Option<Decoded>],
    runs: &mut Vec<(u32, u32, u8)>,
) {
    runs.clear();
    for (n, key) in keys.iter().enumerate() {
        if n > 0
            && keys[n - 1] == *key
            && let Some(run) = runs.last_mut()
        {
            run.1 += 1;
            continue;
        }
        let p = if parts.len() == 1 { 0 } else { part_of(WordTable::<W>::hash(key)) };
        parts[p].push(key, base + sel[n] as u64);
        runs.push((n as u32, 1, p as u8));
    }
    for (f, &fold) in folds.iter().enumerate() {
        let column = |place: usize| decoded[place].as_ref();
        let min = matches!(fold, Fold::Min(_));
        for &(start, len, p) in runs.iter() {
            let rows = &sel[start as usize..(start + len) as usize];
            let state = &mut parts[usize::from(p)].states[f];
            let value =
                |d: Option<&Decoded>, i: usize| d.filter(|d| !null(d, i)).map(|d| int(d, i));
            match fold {
                Fold::Count => state.push_cell(Cell::Count(i64::from(len))),
                Fold::CountArg(place) => {
                    let d = column(place);
                    let n = rows.iter().filter(|&&i| d.is_some_and(|d| !null(d, i))).count();
                    state.push_cell(Cell::Count(n as i64));
                }
                Fold::Sum(place) => {
                    let d = column(place);
                    let (mut n, mut sum) = (0, 0);
                    for v in rows.iter().filter_map(|&i| value(d, i)) {
                        n += 1;
                        sum += i128::from(v);
                    }
                    state.push_cell(Cell::Sum(n, sum));
                }
                Fold::Min(place) | Fold::Max(place) => {
                    let d = column(place);
                    let mut e: Option<i64> = None;
                    for v in rows.iter().filter_map(|&i| value(d, i)) {
                        e = Some(e.map_or(v, |s| if min { s.min(v) } else { s.max(v) }));
                    }
                    state.push_cell(Cell::Extreme(e));
                }
                Fold::None => {}
            }
        }
    }
}

/// Moves the groups of `local` into the buffers of their parts, and empties `local` and its table.
fn flush<const W: usize>(local: &mut Buf, table: &mut WordTable<W>, parts: &mut [Buf]) {
    for g in 0..local.len() {
        let key = local.key::<W>(g);
        let part = if parts.len() == 1 {
            &mut parts[0]
        } else {
            &mut parts[part_of(WordTable::<W>::hash(&key))]
        };
        part.push(&key, local.firsts[g]);
        for (to, from) in part.states.iter_mut().zip(&local.states) {
            to.take(from, g);
        }
    }
    local.clear();
    table.clear();
}

/// Puts into `sel` the rows of the vector `vector` of the segment `segment` that pass the filters of `plan`, and the columns that the filters test into `decoded`. False when no row passes.
pub(super) fn select<'p>(
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

/// A row of a vector, as an index into its decoded values.
pub(super) trait Row: Copy {
    /// The index of the row.
    fn at(self) -> usize;
}

impl Row for usize {
    #[inline]
    fn at(self) -> usize {
        self
    }
}

impl Row for u32 {
    #[inline]
    fn at(self) -> usize {
        self as usize
    }
}

/// Keeps the rows of `sel` whose values in `d` pass `test`. The kind of the test and the form of the values are found once for the vector and not once for each row, and the loop keeps a row with no branch, since a branch on a condition that some rows pass and some do not is often wrong.
pub(super) fn keep<R: Row>(test: &Test, d: &Decoded, sel: &mut Vec<R>) {
    let c = |key: &u64| rupg_summary::key::int_of(*key);
    let n = d.nulls.as_deref();
    match (test, n) {
        (Test::IsNull, None) => return sel.clear(),
        (Test::IsNotNull, None) => return,
        (Test::IsNull | Test::IsNotNull, Some(_)) => {
            return retain(sel, |i| passes(test, null(d, i), 0));
        }
        _ => {}
    }
    match &d.data {
        DecodedData::Int(v) => match test {
            Test::Eq(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x == k);
            }
            Test::Ne(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x != k);
            }
            Test::Lt(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x < k);
            }
            Test::Le(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x <= k);
            }
            Test::Gt(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x > k);
            }
            Test::Ge(k) => {
                let k = c(k);
                compare(v, n, sel, |x| x >= k);
            }
            Test::In(keys) => {
                let values: Vec<i64> = keys.iter().map(c).collect();
                compare(v, n, sel, |x| values.contains(&x));
            }
            Test::Codes(bits) => {
                compare(v, n, sel, |x| u64::try_from(x).is_ok_and(|x| has_code(bits, x)));
            }
            Test::IsNull | Test::IsNotNull => {}
        },
        // The test of a text is on its code, which the zones test gives as an integer. A code that does not fit is the code of no row.
        DecodedData::Code(v) => match test {
            Test::Eq(k) => match u32::try_from(c(k)) {
                Ok(k) => compare(v, n, sel, |x| x == k),
                Err(_) => sel.clear(),
            },
            Test::Ne(k) => match u32::try_from(c(k)) {
                Ok(k) => compare(v, n, sel, |x| x != k),
                Err(_) => compare(v, n, sel, |_| true),
            },
            Test::Codes(bits) => compare(v, n, sel, |x| has_code(bits, u64::from(x))),
            _ => retain(sel, |i| passes(test, null(d, i), int(d, i))),
        },
        DecodedData::Float(_) => retain(sel, |i| passes(test, null(d, i), int(d, i))),
    }
}

/// Keeps the rows of `sel` whose values in `v` are not null by `nulls` and pass `test`.
#[inline]
fn compare<T: Copy, R: Row>(
    v: &[T],
    nulls: Option<&[bool]>,
    sel: &mut Vec<R>,
    test: impl Fn(T) -> bool,
) {
    match nulls {
        None => retain(sel, |i| test(v[i])),
        Some(nulls) => retain(sel, |i| !nulls[i] & test(v[i])),
    }
}

/// Keeps the rows of `sel` that `keep` gives true for, in their order. Each row is written, and the end of the kept rows moves only when the row is kept.
#[inline]
fn retain<R: Row>(sel: &mut Vec<R>, keep: impl Fn(usize) -> bool) {
    let mut kept = 0;
    for j in 0..sel.len() {
        let i = sel[j];
        sel[kept] = i;
        kept += usize::from(keep(i.at()));
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
        heap: false,
    }))
}

/// The values of each part of the hashes for one run of vectors, and the first row of the run with a value.
type Values = (Vec<Vec<u64>>, Option<u64>);

/// The most groups of a `LIMIT` with no sort for which [`lead_cut`] finds a test of the first key.
pub(super) const FIRST_GROUPS: usize = 256;

/// For a `LIMIT` with no sort of `n` groups that go in the order of a sort of their keys, with the column of integers at `place` as the first key in ascending order and nulls last: the `n`th least value of the column that is not null in the rows that pass the filters of `plan`, or `None` when the column has fewer than `n` such values. Each value of the column has at least one group, so the first `n` groups have a value that is not more than this value, and each row of a group has the value of the group. The scan reads the vectors in order, and does not read a vector whose zone shows that no value of the column is less than the `n`th least value so far.
pub(super) fn lead_cut(
    stored: &[Arc<Stored>],
    plan: &Plan,
    place: usize,
    rows: usize,
    segments: usize,
    n: usize,
) -> Result<Option<i64>> {
    if n == 0 {
        return Ok(None);
    }
    let mut least: BTreeSet<i64> = BTreeSet::new();
    let mut decoded: Vec<Option<Decoded>> = (0..stored.len()).map(|_| None).collect();
    let mut sel = Vec::with_capacity(VECTOR);
    let mut tests = Vec::with_capacity(plan.filters.len());
    for (segment, vector) in units_of(rows, segments) {
        let cut = least.last().copied().filter(|_| least.len() == n);
        if let Some(cut) = cut {
            let segment_rows = rows.saturating_sub(segment * ROWS).min(ROWS);
            let zones = &stored[place].segments[segment].summary.vectors;
            if zones.len() == stored::vectors(segment_rows)
                && matches!(
                    zones[vector].test(&Test::Lt(rupg_summary::key::int(cut))),
                    Verdict::None
                )
            {
                continue;
            }
        }
        if !select(stored, plan, rows, (segment, vector), &mut tests, &mut sel, &mut decoded)? {
            continue;
        }
        // The filters decoded the column when they tested it. Else the column can keep a vector of an earlier unit, so the scan decodes it again.
        let tested = tests.iter().any(|t| t.0 == place);
        let d = match decoded[place].take().filter(|_| tested) {
            Some(d) => d,
            None => decode(&stored[place], segment, vector)?,
        };
        let mut cut = cut.unwrap_or(i64::MAX);
        for &i in &sel {
            if null(&d, i) {
                continue;
            }
            let v = int(&d, i);
            if v >= cut || !least.insert(v) {
                continue;
            }
            if least.len() > n {
                least.pop_last();
            }
            if least.len() == n {
                cut = least.last().copied().unwrap_or(i64::MAX);
            }
        }
    }
    Ok(least.last().copied().filter(|_| least.len() == n))
}

/// The vectors of the scan of `rows` rows in `segments` segments, as their segment and their place in it.
pub(super) fn units_of(rows: usize, segments: usize) -> Vec<(usize, usize)> {
    let mut units = Vec::new();
    for segment in 0..segments {
        let segment_rows = rows.saturating_sub(segment * ROWS).min(ROWS);
        units.extend((0..stored::vectors(segment_rows)).map(|vector| (segment, vector)));
    }
    units
}

/// The groups of the vectors `units` of the columns `stored`, which have `rows` rows.
/// Puts into `keys` the packed key of each row `sel` of a vector, from the decoded columns `decoded`. A key whose columns are not decoded keeps its bits at 0.
fn fill_keys<const W: usize>(
    plan: &Plan,
    pack: &Pack,
    decoded: &[Option<Decoded>],
    sel: &[usize],
    keys: &mut Vec<[u64; W]>,
) {
    keys.clear();
    keys.resize(sel.len(), [0; W]);
    for (k, input) in plan.keys.iter().enumerate() {
        match *input {
            Input::Case { ref tests, then, default } => {
                let Some(d) = decoded[then].as_ref() else { continue };
                let tested: Vec<&Decoded> =
                    tests.iter().filter_map(|t| decoded[t.0].as_ref()).collect();
                if tested.len() < tests.len() {
                    continue;
                }
                for (key, &i) in keys.iter_mut().zip(sel) {
                    let chosen =
                        tests.iter().zip(&tested).all(|(t, d)| passes(&t.1, null(d, i), int(d, i)));
                    match (chosen, default) {
                        (true, _) if null(d, i) => pack.put_null(key, k),
                        (true, _) => pack.put(key, k, int(d, i) as u64),
                        (false, Some(word)) => pack.put(key, k, word),
                        (false, None) => pack.put_null(key, k),
                    }
                }
            }
            Input::Column(place) => {
                let Some(d) = decoded[place].as_ref() else { continue };
                let Place::Bits(value, flag) = pack.places[k] else { continue };
                // A vector with no nulls, the most common, puts each value with no test.
                match (&d.data, &d.nulls) {
                    (DecodedData::Int(v), None) => {
                        for (key, &i) in keys.iter_mut().zip(sel) {
                            value.put(key, v[i] as u64);
                        }
                    }
                    (DecodedData::Code(v), None) => {
                        for (key, &i) in keys.iter_mut().zip(sel) {
                            value.put(key, u64::from(v[i]));
                        }
                    }
                    _ => {
                        for (key, &i) in keys.iter_mut().zip(sel) {
                            if !null(d, i) {
                                value.put(key, int(d, i) as u64);
                            } else if let Some(flag) = flag {
                                flag.put(key, 1);
                            }
                        }
                    }
                }
            }
            Input::Floor(place, step) => {
                let Some(d) = decoded[place].as_ref() else { continue };
                for (key, &i) in keys.iter_mut().zip(sel) {
                    if null(d, i) {
                        pack.put_null(key, k);
                    } else {
                        let v = int(d, i);
                        let infinite = v == i64::MIN || v == i64::MAX;
                        pack.put(key, k, if infinite { v } else { v - v.rem_euclid(step) } as u64);
                    }
                }
            }
            Input::Part(place, step, span) => {
                let Some(d) = decoded[place].as_ref() else { continue };
                for (key, &i) in keys.iter_mut().zip(sel) {
                    let v = int(d, i);
                    if null(d, i) || v == i64::MIN || v == i64::MAX {
                        pack.put_null(key, k);
                    } else {
                        pack.put(key, k, v.div_euclid(step).rem_euclid(span) as u64);
                    }
                }
            }
            // A constant key has no bits in a packed key.
            Input::Const(_) => {}
        }
    }
}

fn scan_run<const W: usize>(
    stored: &[Arc<Stored>],
    plan: &Plan,
    pack: &Pack,
    places: &[usize],
    units: &[(usize, usize)],
    rows: usize,
) -> Result<Run> {
    let width = plan.keys.len();
    // With no keys the scan has one group, so one part holds it.
    let count = if width == 0 { 1 } else { PARTS };
    let mut parts: Vec<Buf> = (0..count).map(|_| Buf::new(&plan.folds, pack.width)).collect();
    let mut extremes: Vec<Option<(i64, i64)>> = vec![None; plan.bases];
    let first_base = plan.folds.len() - plan.bases;
    let mins: Vec<bool> = plan.folds.iter().map(|f| matches!(f, Fold::Min(_))).collect();
    let mut decoded: Vec<Option<Decoded>> =
        (0..stored.len() + plan.maps.len()).map(|_| None).collect();
    let mut last = None;
    let mut sel: Vec<usize> = Vec::with_capacity(VECTOR);
    let mut keys: Vec<[u64; W]> = Vec::with_capacity(VECTOR);
    let mut hashes: Vec<u64> = Vec::with_capacity(VECTOR);
    let mut ids: Vec<u32> = Vec::with_capacity(VECTOR);
    let mut runs: Vec<(u32, u32, u8)> = Vec::with_capacity(VECTOR);
    let mut tests: Vec<&(usize, Test)> = Vec::with_capacity(plan.filters.len());
    let mut local = Buf::new(&plan.folds, pack.width);
    // With no keys the rows fold with no table, so the table has no room.
    let mut table = WordTable::<W>::new(if width == 0 { 0 } else { LOCAL });
    // The runs of equal keys that the table folded since it was last empty, and true when the runs go into the buffers with no table.
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
        for (m, map) in plan.maps.iter().enumerate() {
            decoded[stored.len() + m] = decoded[map.from].as_ref().map(|d| map_vector(map, d));
        }
        let base = (segment * ROWS + vector * VECTOR) as u64;
        last = sel.last().map(|&i| base + i as u64);
        fill_keys(plan, pack, &decoded, &sel, &mut keys);
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
        if width == 0 {
            // With no keys, each row is in the one group, so the rows fold with no table.
            if local.len() == 0
                && let Some(&i) = sel.first()
            {
                local.push(&[0; W], base + i as u64);
                for state in &mut local.states {
                    state.push();
                }
            }
            for ((state, &fold), &min) in local.states.iter_mut().zip(&plan.folds).zip(&mins) {
                if let Fold::Count = fold {
                    state.fold_cell(0, Cell::Count(sel.len() as i64), min);
                } else if !matches!(fold, Fold::None) {
                    for &i in &sel {
                        state.fold_cell(0, cell(fold, &decoded, i), min);
                    }
                }
            }
            continue;
        }
        if direct {
            push_direct(&mut parts, &plan.folds, &keys, &sel, base, &decoded, &mut runs);
            continue;
        }
        if table.room() < sel.len() {
            // A table that folds fewer than two runs of equal keys into each group does not pay for its probes, as the runs fold with no table.
            direct = folded < 2 * local.len();
            flush(&mut local, &mut table, &mut parts);
            folded = 0;
            if direct {
                // Each part gets about its share of the rows that are left, so its buffers do not grow many times.
                let more = (units.len() - n) * VECTOR / PARTS;
                for part in &mut parts {
                    part.reserve(more + more / 8);
                }
                push_direct(&mut parts, &plan.folds, &keys, &sel, base, &decoded, &mut runs);
                continue;
            }
        }
        hashes.clear();
        hashes.extend(keys.iter().map(WordTable::<W>::hash));
        ids.clear();
        for (n, ((key, &hash), &i)) in keys.iter().zip(&hashes).zip(&sel).enumerate() {
            // The rows of a table often come in the order of a key, so a row often has the key of the row before it, and then its group.
            if n > 0 && keys[n - 1] == *key {
                ids.push(ids[n - 1]);
                continue;
            }
            folded += 1;
            // The table is larger than the cache of the core for many groups, so the read of a slot starts early.
            if let Some(&ahead) = hashes.get(n + PREFETCH) {
                table.prefetch(ahead);
            }
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
            fold_rows(state, fold, &ids, &sel, &decoded, min);
        }
    }
    flush(&mut local, &mut table, &mut parts);
    Ok(Run { parts, extremes, last })
}

/// The groups of one part from its buffers `bufs` of all the runs, which come in the order of the runs.
fn merge<const W: usize>(bufs: Vec<Buf>, folds: &[Fold], mins: &[bool], width: usize) -> Buf {
    let mut merged = Buf::new(folds, width);
    let size: usize = bufs.iter().map(Buf::len).sum();
    // The part has at most `size` groups, so its buffers do not grow again.
    merged.reserve(size);
    let mut table = WordTable::<W>::new(size);
    let mut hashes = Vec::new();
    for from in bufs {
        hashes.clear();
        hashes.extend((0..from.len()).map(|g| WordTable::<W>::hash(&from.key(g))));
        for (g, &hash) in hashes.iter().enumerate() {
            if let Some(&ahead) = hashes.get(g + PREFETCH) {
                table.prefetch(ahead);
            }
            let next = merged.len() as u32;
            let key = from.key::<W>(g);
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

/// For `ORDER BY` of one count with `LIMIT`, the groups that [`run`] gives: the place of the fold of the count in the folds of the plan, true for a descending order, and the bound of the sort. For each key of the plan that a derived key reads, `ranges` has the place of the key and the range of its values for which the derived key has a value of its type.
pub(super) struct Top {
    pub(super) fold: usize,
    pub(super) descending: bool,
    pub(super) bound: usize,
    pub(super) ranges: Vec<(usize, i128, i128)>,
    /// The order of the groups is the order of their first rows for more groups than this, as for a hash table that spills, which [`heap_groups`] reads.
    pub(super) spill: Option<usize>,
}

/// True when the value of each key of `part` that a derived key reads is in its range of `top.ranges`.
fn fits(part: &Buf, pack: &Pack, top: &Top) -> bool {
    top.ranges.iter().all(|&(k, lo, hi)| {
        part.keys.chunks_exact(pack.width).all(|packed| {
            pack.value(packed, k).is_none_or(|v| (lo..=hi).contains(&i128::from(v as i64)))
        })
    })
}

/// The groups `chosen` of the parts `parts`, each a part and a place in the part, in the order of `chosen`.
fn gather(
    parts: &[Buf],
    plan: &Plan,
    pack: &Pack,
    chosen: impl Iterator<Item = (u32, u32)>,
) -> (Buf, Vec<u64>) {
    let mut words = [0_u64; 8];
    let mut out = Buf::new(&plan.folds, plan.keys.len() + 1);
    let mut packed = Vec::new();
    for (p, g) in chosen {
        let (part, g) = (&parts[p as usize], g as usize);
        let key = &part.keys[g * pack.width..(g + 1) * pack.width];
        packed.extend_from_slice(key);
        pack.unpack(key, &mut words);
        out.push(&words, part.firsts[g]);
        for (to, from) in out.states.iter_mut().zip(&part.states) {
            to.take(from, g);
        }
    }
    (out, packed)
}

/// The groups of the first `top.bound` counts of the parts `parts`, in the order of their first rows, when the first `top.bound` counts in the order of the sort and the count after them are all different. The bounded heap of the sort then keeps these groups for each order of the groups, as `distinct_top` of the scan finds them. The parts have different keys, so the count of each group is in one part. `None` when two of these counts are equal, for few groups, or when the value of a key of a group is out of `top.ranges`, so that the order of all the groups gives the error of the derived key.
fn top_groups(
    parts: &[Buf],
    plan: &Plan,
    pack: &Pack,
    top: &Top,
    last: Option<u64>,
    extremes: &[Option<(i64, i64)>],
) -> Option<(Found, Vec<u64>)> {
    if !matches!(plan.folds.get(top.fold), Some(Fold::Count | Fold::CountArg(_))) {
        return None;
    }
    let groups: usize = parts.iter().map(Buf::len).sum();
    if groups <= top.bound.saturating_mul(4) {
        return None;
    }
    // A larger rank is first in the sort. The heap has the first `bound + 1` ranks, with the last of them at its top.
    let rank = |c: i64| if top.descending { i128::from(c) } else { -i128::from(c) };
    let mut best: BinaryHeap<Reverse<(i128, u32, u32)>> = BinaryHeap::with_capacity(top.bound + 2);
    let mut most = None;
    for (p, part) in parts.iter().enumerate() {
        let Folded::Count(counts) = &part.states[top.fold] else { return None };
        for (g, &c) in counts.iter().enumerate() {
            let r = rank(c);
            if best.len() <= top.bound {
                best.push(Reverse((r, p as u32, g as u32)));
            } else if best.peek().is_some_and(|t| r > t.0.0) {
                best.pop();
                best.push(Reverse((r, p as u32, g as u32)));
            }
        }
        if !fits(part, pack, top) {
            return None;
        }
        most = most.max(part.firsts.iter().max().copied());
    }
    let mut best: Vec<(i128, u32, u32)> = best.into_iter().map(|r| r.0).collect();
    best.sort_unstable_by_key(|a| Reverse(a.0));
    if best.len() <= top.bound || best.windows(2).any(|w| w[0].0 == w[1].0) {
        return None;
    }
    best.truncate(top.bound);
    best.sort_unstable_by_key(|&(_, p, g)| parts[p as usize].firsts[g as usize]);
    let (out, packed) = gather(parts, plan, pack, best.iter().map(|&(_, p, g)| (p, g)));
    // As on one task, the scan ends inside a group when its last row that passes is not the first row of a group.
    let trailing = last.is_some() && most != last;
    let found = Found {
        words: out.keys,
        firsts: out.firsts,
        states: out.states,
        extremes: extremes.to_vec(),
        trailing,
        heap: false,
    };
    Some((found, packed))
}

/// The groups of the parts `parts` that can change the bounded heap of the sort of `top`, in the order of their first rows, when the order of all the groups is the order of their first rows: for more than `top.spill` groups. The heap takes the first `top.bound` groups of the order, and then keeps a group only when its count comes before the count at its top. That count only moves forward from the last count of the first `top.bound` groups, so a later group whose count does not come before that count changes nothing. `None` for few groups, for another order of the groups, or when the value of a key of a group is out of `top.ranges`, as for [`top_groups`].
fn heap_groups(
    parts: &[Buf],
    plan: &Plan,
    pack: &Pack,
    top: &Top,
    last: Option<u64>,
    extremes: &[Option<(i64, i64)>],
) -> Option<(Found, Vec<u64>)> {
    if !matches!(plan.folds.get(top.fold), Some(Fold::Count | Fold::CountArg(_))) {
        return None;
    }
    let groups: usize = parts.iter().map(Buf::len).sum();
    if top.bound == 0 || groups <= top.spill? || groups <= top.bound.saturating_mul(4) {
        return None;
    }
    // A larger rank is first in the sort.
    let rank = |c: i64| if top.descending { i128::from(c) } else { -i128::from(c) };
    // The first `bound` groups by their first rows, with the last of them at the top of the heap.
    let mut first: BinaryHeap<(u64, i128)> = BinaryHeap::with_capacity(top.bound + 1);
    for part in parts {
        let Folded::Count(counts) = &part.states[top.fold] else { return None };
        if !fits(part, pack, top) {
            return None;
        }
        for (&f, &c) in part.firsts.iter().zip(counts) {
            if first.len() < top.bound {
                first.push((f, rank(c)));
            } else if first.peek().is_some_and(|t| f < t.0) {
                first.pop();
                first.push((f, rank(c)));
            }
        }
    }
    let edge = first.peek()?.0;
    let least = first.iter().map(|t| t.1).min()?;
    let mut chosen: Vec<(u64, u32, u32)> = Vec::new();
    let mut most = None;
    for (p, part) in parts.iter().enumerate() {
        let Folded::Count(counts) = &part.states[top.fold] else { return None };
        for (g, (&f, &c)) in part.firsts.iter().zip(counts).enumerate() {
            if f <= edge || rank(c) > least {
                chosen.push((f, p as u32, g as u32));
            }
        }
        most = most.max(part.firsts.iter().max().copied());
    }
    chosen.sort_unstable_by_key(|c| c.0);
    let (out, packed) = gather(parts, plan, pack, chosen.into_iter().map(|(_, p, g)| (p, g)));
    // As on one task, the scan ends inside a group when its last row that passes is not the first row of a group.
    let trailing = last.is_some() && most != last;
    let found = Found {
        words: out.keys,
        firsts: out.firsts,
        states: out.states,
        extremes: extremes.to_vec(),
        trailing,
        heap: true,
    };
    Some((found, packed))
}

/// The groups of the scan of the columns `stored`, which have `rows` rows in `segments` segments, on the tasks `tasks`. With `top`, the groups can be only the groups of the result of the sort, as [`top_groups`] gives them. With `ordered` false, the groups come in the order of the parts, for a query that sorts the groups by their keys: the order of their first rows then costs a map of the rows and a read of each group from one of many parts, and does not change the result.
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
    top: Option<&Top>,
    ordered: bool,
) -> Result<Found> {
    // spec/14 section 14.7: for `ORDER BY` of a count, the tasks first find the groups with only the count, so that each group of the many groups keeps fewer bytes. The other folds then run only for the few groups of the result, in a second scan.
    if let Some(top) = top
        && let Some(lean) = lean_plan(plan, top)
        && let Grouped::Top(mut found, packed) =
            grouped(tasks, stored, &lean, rows, segments, Some(top), ordered, true)?
    {
        let pack = Pack::new(stored, plan);
        let states = match pack.width {
            1 => fold_chosen::<1>(tasks, stored, plan, &pack, rows, segments, &packed, &found, top),
            2 => fold_chosen::<2>(tasks, stored, plan, &pack, rows, segments, &packed, &found, top),
            3 | 4 => {
                fold_chosen::<4>(tasks, stored, plan, &pack, rows, segments, &packed, &found, top)
            }
            _ => fold_chosen::<8>(tasks, stored, plan, &pack, rows, segments, &packed, &found, top),
        }?;
        for ((to, state), (&fold, &was)) in
            found.states.iter_mut().zip(states).zip(plan.folds.iter().zip(&lean.folds))
        {
            if matches!(was, Fold::None) && !matches!(fold, Fold::None) {
                *to = state;
            }
        }
        return Ok(found);
    }
    match grouped(tasks, stored, plan, rows, segments, top, ordered, false)? {
        Grouped::Top(found, _) | Grouped::All(found) => Ok(found),
        Grouped::Unsure => Err(rupg_common::Error::internal("a scan of groups gave no groups")),
    }
}

/// The groups that [`grouped`] gives.
enum Grouped {
    /// The groups of the result of the sort of a [`Top`], and the packed key of each group.
    Top(Found, Vec<u64>),
    /// All the groups.
    All(Found),
    /// No groups, for a lean scan whose groups are not only the groups of the result of the sort.
    Unsure,
}

/// The plan of `plan` with no fold other than the count of `top`, or `None` when the plan has no other fold, or has bases, whose extremes need all the rows. The groups and their counts are the same, so the groups of the result of the sort are the same.
fn lean_plan(plan: &Plan, top: &Top) -> Option<Plan> {
    if plan.bases > 0
        || !matches!(plan.folds.get(top.fold), Some(Fold::Count | Fold::CountArg(_)))
        || plan
            .folds
            .iter()
            .enumerate()
            .all(|(f, fold)| f == top.fold || matches!(fold, Fold::None))
    {
        return None;
    }
    Some(Plan {
        keys: plan.keys.clone(),
        folds: plan
            .folds
            .iter()
            .enumerate()
            .map(|(f, &fold)| if f == top.fold { fold } else { Fold::None })
            .collect(),
        bases: 0,
        filters: plan.filters.clone(),
        maps: plan.maps.clone(),
    })
}

/// The states of each fold of `plan` for the groups of `found`, whose packed keys of `pack` are `packed`, from a scan of the rows of these groups. The scan starts at the vector of the first row of the groups. When the count of `top` counts rows, the scan stops when it has folded the counted rows of all the groups.
#[allow(clippy::too_many_arguments)]
fn fold_chosen<const W: usize>(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    plan: &Plan,
    pack: &Pack,
    rows: usize,
    segments: usize,
    packed: &[u64],
    found: &Found,
    top: &Top,
) -> Result<Vec<Folded>> {
    let groups = found.firsts.len();
    let mut chosen: FxMap<[u64; W], u32> = FxMap::default();
    for (g, words) in packed.chunks_exact(pack.width).enumerate() {
        chosen.insert(std::array::from_fn(|i| words.get(i).copied().unwrap_or(0)), g as u32);
    }
    let empty = || -> Vec<Folded> {
        plan.folds
            .iter()
            .map(|&fold| {
                let mut state = Folded::of(fold);
                for _ in 0..groups {
                    state.push();
                }
                state
            })
            .collect()
    };
    // A group has no row before its first row.
    let start = found.firsts.iter().min().copied().unwrap_or(0);
    let units: Vec<(usize, usize)> = units_of(rows, segments)
        .into_iter()
        .filter(|&(segment, vector)| (segment * ROWS + (vector + 1) * VECTOR) as u64 > start)
        .collect();
    if units.is_empty() || groups == 0 {
        return Ok(empty());
    }
    // The rows that the scan has yet to fold, when the count counts each row of a group.
    let left = match (plan.folds[top.fold], &found.states[top.fold]) {
        (Fold::Count, Folded::Count(counts)) => Some(AtomicI64::new(counts.iter().sum())),
        _ => None,
    };
    let source = |place: usize| plan.source(stored.len(), place);
    let mut key_places: Vec<usize> = plan.keys.iter().flat_map(Input::places).map(source).collect();
    key_places.sort_unstable();
    key_places.dedup();
    let mut fold_places: Vec<usize> = plan
        .folds
        .iter()
        .filter_map(|f| match f {
            Fold::CountArg(p) | Fold::Sum(p) | Fold::Min(p) | Fold::Max(p) => Some(source(*p)),
            Fold::Count | Fold::None => None,
        })
        .filter(|place| !key_places.contains(place))
        .collect();
    fold_places.sort_unstable();
    fold_places.dedup();
    let mins: Vec<bool> = plan.folds.iter().map(|f| matches!(f, Fold::Min(_))).collect();
    let jobs = tasks.parallelism().min(units.len() * VECTOR / TASK_ROWS).clamp(1, units.len());
    let chunk = units.len().div_ceil(jobs);
    let mut outs: Vec<Option<Result<Vec<Folded>>>> = Vec::new();
    outs.resize_with(units.len().div_ceil(chunk), || None);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut outs)
        .map(|(units, out)| {
            let (chosen, left, key_places, fold_places, mins, empty) =
                (&chosen, &left, &key_places, &fold_places, &mins, &empty);
            Box::new(move || {
                *out = Some((|| {
                    let mut states = empty();
                    let mut decoded: Vec<Option<Decoded>> =
                        (0..stored.len() + plan.maps.len()).map(|_| None).collect();
                    let mut tests: Vec<&(usize, Test)> = Vec::with_capacity(plan.filters.len());
                    let mut sel: Vec<usize> = Vec::with_capacity(VECTOR);
                    let mut keys: Vec<[u64; W]> = Vec::with_capacity(VECTOR);
                    let (mut ids, mut hits) = (Vec::new(), Vec::new());
                    for &(segment, vector) in units {
                        if left.as_ref().is_some_and(|l| l.load(Ordering::Relaxed) <= 0) {
                            break;
                        }
                        if !select(
                            stored,
                            plan,
                            rows,
                            (segment, vector),
                            &mut tests,
                            &mut sel,
                            &mut decoded,
                        )? {
                            continue;
                        }
                        let decode_all = |places: &[usize],
                                          decoded: &mut [Option<Decoded>],
                                          tests: &[&(usize, Test)]|
                         -> Result<()> {
                            for &place in places {
                                if !tests.iter().any(|t| t.0 == place) {
                                    decoded[place] = Some(decode(&stored[place], segment, vector)?);
                                }
                            }
                            for (m, map) in plan.maps.iter().enumerate() {
                                decoded[stored.len() + m] =
                                    decoded[map.from].as_ref().map(|d| map_vector(map, d));
                            }
                            Ok(())
                        };
                        decode_all(key_places, &mut decoded, &tests)?;
                        fill_keys(plan, pack, &decoded, &sel, &mut keys);
                        ids.clear();
                        hits.clear();
                        for (key, &i) in keys.iter().zip(&sel) {
                            if let Some(&id) = chosen.get(key) {
                                ids.push(id);
                                hits.push(i);
                            }
                        }
                        if ids.is_empty() {
                            continue;
                        }
                        decode_all(fold_places, &mut decoded, &tests)?;
                        for ((state, &fold), &min) in states.iter_mut().zip(&plan.folds).zip(mins) {
                            if !matches!(fold, Fold::None) {
                                fold_rows(state, fold, &ids, &hits, &decoded, min);
                            }
                        }
                        if let Some(left) = left {
                            left.fetch_sub(ids.len() as i64, Ordering::Relaxed);
                        }
                    }
                    Ok(states)
                })());
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let mut outs = outs.into_iter().flatten();
    let mut states = outs.next().transpose()?.unwrap_or_else(empty);
    for other in outs {
        let other = other?;
        for ((to, from), &min) in states.iter_mut().zip(&other).zip(&mins) {
            for g in 0..groups {
                to.merge(g, from, g, min);
            }
        }
    }
    Ok(states)
}

#[allow(clippy::too_many_arguments)]
fn grouped(
    tasks: &dyn Tasks,
    stored: &[Arc<Stored>],
    plan: &Plan,
    rows: usize,
    segments: usize,
    top: Option<&Top>,
    ordered: bool,
    lean: bool,
) -> Result<Grouped> {
    let mut places: Vec<usize> = plan
        .keys
        .iter()
        .flat_map(Input::places)
        .chain(plan.folds.iter().filter_map(|f| match f {
            Fold::CountArg(p) | Fold::Sum(p) | Fold::Min(p) | Fold::Max(p) => Some(*p),
            Fold::Count | Fold::None => None,
        }))
        .map(|place| plan.source(stored.len(), place))
        .collect();
    places.sort_unstable();
    places.dedup();
    let units = units_of(rows, segments);
    if units.is_empty() {
        return Ok(Grouped::All(Found {
            words: Vec::new(),
            firsts: Vec::new(),
            states: plan.folds.iter().map(|&f| Folded::of(f)).collect(),
            extremes: vec![None; plan.bases],
            trailing: false,
            heap: false,
        }));
    }
    let jobs = tasks.parallelism().min(rows / TASK_ROWS).clamp(1, units.len());
    let chunk = units.len().div_ceil(jobs);
    let pack = Pack::new(stored, plan);
    let packed = pack.width;
    let mut runs: Vec<Option<Result<Run>>> = Vec::new();
    runs.resize_with(units.len().div_ceil(chunk), || None);
    let work: Vec<Job<'_>> = units
        .chunks(chunk)
        .zip(&mut runs)
        .map(|(units, out)| {
            let (places, pack) = (&places, &pack);
            Box::new(move || {
                // A key of fewer words has smaller slots, so more of the table stays in the cache, and a probe hashes and compares fewer words.
                *out = Some(match packed {
                    1 => scan_run::<1>(stored, plan, pack, places, units, rows),
                    2 => scan_run::<2>(stored, plan, pack, places, units, rows),
                    3 | 4 => scan_run::<4>(stored, plan, pack, places, units, rows),
                    _ => scan_run::<8>(stored, plan, pack, places, units, rows),
                });
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let runs: Vec<Run> = runs.into_iter().flatten().collect::<Result<_>>()?;
    let mut extremes = vec![None; plan.bases];
    let mut last = None;
    let count = runs.first().map_or(PARTS, |run| run.parts.len());
    let mut by_part: Vec<Vec<Buf>> = (0..count).map(|_| Vec::with_capacity(runs.len())).collect();
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
    let mut parts: Vec<Option<Buf>> = (0..count).map(|_| None).collect();
    let per = count.div_ceil(jobs);
    let work: Vec<Job<'_>> = parts
        .chunks_mut(per)
        .zip(by_part.chunks_mut(per))
        .map(|(out, bufs)| {
            let mins = &mins;
            Box::new(move || {
                for (slot, bufs) in out.iter_mut().zip(bufs) {
                    let bufs = std::mem::take(bufs);
                    *slot = Some(match packed {
                        1 => merge::<1>(bufs, &plan.folds, mins, packed),
                        2 => merge::<2>(bufs, &plan.folds, mins, packed),
                        3 | 4 => merge::<4>(bufs, &plan.folds, mins, packed),
                        _ => merge::<8>(bufs, &plan.folds, mins, packed),
                    });
                }
            }) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    let parts: Vec<Buf> = parts.into_iter().flatten().collect();
    if let Some((found, packed)) =
        top.and_then(|top| top_groups(&parts, plan, &pack, top, last, &extremes))
    {
        return Ok(Grouped::Top(found, packed));
    }
    if ordered
        && let Some((found, packed)) =
            top.and_then(|top| heap_groups(&parts, plan, &pack, top, last, &extremes))
    {
        return Ok(Grouped::Top(found, packed));
    }
    if lean {
        return Ok(Grouped::Unsure);
    }
    let groups: usize = parts.iter().map(Buf::len).sum();
    let order = if !ordered {
        parts
            .iter()
            .enumerate()
            .flat_map(|(p, part)| (0..part.len() as u32).map(move |g| (p as u32, g)))
            .collect()
    } else if groups.saturating_mul(SORT_GROUPS) < rows {
        // For few groups, a sort of their first rows costs less than a map of all the rows.
        let mut order: Vec<(u64, u32, u32)> = parts
            .iter()
            .enumerate()
            .flat_map(|(p, part)| {
                part.firsts.iter().enumerate().map(move |(g, &first)| (first, p as u32, g as u32))
            })
            .collect();
        order.sort_unstable_by_key(|&(first, ..)| first);
        order.into_iter().map(|(_, p, g)| (p, g)).collect()
    } else {
        order_by_map(&parts, rows, groups)
    };
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
            let (parts, pack) = (&parts, &pack);
            Box::new(move || {
                for (n, &(p, g)) in order.iter().enumerate() {
                    // Each group of the run comes from one of many parts, so the processor cannot predict the next reads.
                    if let Some(&(p, g)) = order.get(n + PREFETCH) {
                        parts[p as usize].prefetch(g as usize);
                    }
                    let (part, g) = (&parts[p as usize], g as usize);
                    pack.unpack(
                        &part.keys[g * packed..(g + 1) * packed],
                        &mut words[n * width..(n + 1) * width],
                    );
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
    let trailing = last.is_some() && firsts.iter().max() != last.as_ref();
    Ok(Grouped::All(Found { words, firsts, states, extremes, trailing, heap: false }))
}

/// The part and the place in the part of each group of `parts`, in the order of their first rows of the `rows` rows of the scan. The place of each group is the number of first rows before its first row, which a map of the first rows gives.
fn order_by_map(parts: &[Buf], rows: usize, groups: usize) -> Vec<(u32, u32)> {
    let mut map = vec![0_u64; rows / 64 + 1];
    for part in parts {
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
    debug_assert_eq!(count as usize, groups);
    let mut order: Vec<(u32, u32)> = vec![(0, 0); groups];
    for (p, part) in parts.iter().enumerate() {
        for (g, &first) in part.firsts.iter().enumerate() {
            let (word, bit) = ((first / 64) as usize, first % 64);
            let place = before[word] + (map[word] & ((1 << bit) - 1)).count_ones();
            order[place as usize] = (p as u32, g as u32);
        }
    }
    order
}

/// A run of the states of one fold in the order of the groups, which one task fills.
enum Column<'a> {
    Count(&'a mut [i64]),
    Sum(&'a mut [(i64, i128)]),
    Extreme(&'a mut [Option<i64>]),
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_the_extreme_values_of_each_kind() {
        let cases: [(Kind, &[i64]); 6] = [
            (Kind::Bool, &[0, 1]),
            (Kind::Int2, &[i64::from(i16::MIN), -1, 0, i64::from(i16::MAX)]),
            (Kind::Int4, &[i64::from(i32::MIN), -1, 0, i64::from(i32::MAX)]),
            (Kind::Date, &[i64::from(i32::MIN), -2451545, 0, i64::from(i32::MAX)]),
            (Kind::Text, &[0, i64::from(u32::MAX)]),
            (Kind::Int8, &[i64::MIN, -1, 0, i64::MAX]),
        ];
        for (kind, values) in cases {
            let (bits, signed) = bits_of(kind);
            // A field after another field in the same word, and a flag after it, must keep the value.
            let shift = (64 - bits).min(3);
            let field = Field { word: 1, shift, bits, signed };
            for &v in values {
                let mut key = [u64::MAX, 0];
                if shift > 0 {
                    key[1] = (1 << shift) - 1;
                }
                field.put(&mut key, v as u64);
                assert_eq!(field.get(&key) as i64, v, "{kind:?} {v}");
                assert_eq!(key[0], u64::MAX);
            }
        }
    }
}

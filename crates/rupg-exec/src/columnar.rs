//! The vector scan: a query of one table whose columns the session keeps in segments reads the encoded vectors, and not the rows.
//!
//! The scan tests the conditions of `WHERE` on each vector of 1024 rows with a selection vector, and it makes the groups and the aggregates with the codes and the integers of the vectors. The targets, `HAVING` and `ORDER BY` then run with [`Eval`] on one row for each group, or on each row that passes, so that they give the values of the row engine. The groups come in the order that the row engine gives them, which is the order of the hash table of `HashAggregate`. A query that the scan cannot run gives `None`, and the row engine runs it.

use std::cell::Cell;
use std::sync::Arc;

use rupg_analyze::{Aggref, BoolOp, Expr, ExprKind, FromItem, Query, Var};
use rupg_column::dict::{Dictionary, MISSING};
use rupg_column::segment::{Kind, Nulls, ROWS, Store};
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_platform::{Job, Tasks};
use rupg_summary::column::{Bound, ColumnSummary, Sum};
use rupg_summary::zone::{Test, Verdict, Zone};
use rupg_types::{Numeric, Value, oid};

use crate::agg;
use crate::{Cache, Eval, Group, hashed, scan, sort};

mod groups;

/// The most entries of the memo of an expression of one column of integers, so that a column of many values does not fill the memory.
const MEMO: usize = 1 << 20;

/// The number of rows between the read of the slot of a row and its probe: enough reads at once to hide most of the wait for the memory, and few enough that the slots stay in the cache.
const PREFETCH_ROWS: usize = 16;

/// The most groups that the map of `GROUP BY` has room for before the scan sees the rate of new groups: four vectors.
const START_GROUPS: usize = 4 * VECTOR;

/// The hash tables of the engine, with its one hash (spec/22 section 22.6).
type FxMap<K, V> = rupg_kernels::hash::HashMap<K, V>;
type FxSet<K> = rupg_kernels::hash::HashSet<K>;

/// The ID of a slot of a [`WordTable`] with no key.
const NO_GROUP: u32 = u32::MAX;

/// A slot of a [`WordTable`]: the words of a key and the ID of its group, together in one cache line.
#[derive(Clone, Copy)]
struct Slot<const W: usize> {
    key: [u64; W],
    id: u32,
}

/// The groups of keys of `W` words: an open table with linear probing that is at most half full, so a probe reads about one slot. A probe of a table that is larger than the cache waits for the memory, so the scan asks for the slot of a row some rows before its probe with [`WordTable::prefetch`].
struct WordTable<const W: usize> {
    slots: Vec<Slot<W>>,
    len: usize,
}

impl<const W: usize> WordTable<W> {
    /// A table with room for `groups` groups.
    fn new(groups: usize) -> WordTable<W> {
        let mut table = WordTable { slots: Vec::new(), len: 0 };
        table.resize((groups * 2).max(16).next_power_of_two());
        table
    }

    fn resize(&mut self, size: usize) {
        let old = std::mem::replace(
            &mut self.slots,
            rupg_platform::alloc::large_vec(size, Slot { key: [0; W], id: NO_GROUP }),
        );
        let mask = size - 1;
        for slot in old.into_iter().filter(|s| s.id != NO_GROUP) {
            let mut at = Self::hash(&slot.key) as usize & mask;
            while self.slots[at].id != NO_GROUP {
                at = (at + 1) & mask;
            }
            self.slots[at] = slot;
        }
    }

    /// The hash of a key, one word at a time.
    #[inline]
    fn hash(key: &[u64; W]) -> u64 {
        use std::hash::Hasher;
        let mut state = rupg_kernels::hash::Fold::default();
        for &word in key {
            state.write_u64(word);
        }
        state.finish()
    }

    /// Starts the read of the first slot of the key whose hash is `hash`.
    #[inline]
    fn prefetch(&self, hash: u64) {
        rupg_kernels::hash::prefetch(&self.slots[hash as usize & (self.slots.len() - 1)]);
    }

    /// The group of `key`, whose hash is `hash`, and true for a new group, which gets the ID `next`.
    #[inline]
    fn id(&mut self, key: &[u64; W], hash: u64, next: u32) -> (u32, bool) {
        if self.room() == 0 {
            self.resize(self.slots.len() * 2);
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = &mut self.slots[at];
            if slot.id == NO_GROUP {
                *slot = Slot { key: *key, id: next };
                self.len += 1;
                return (next, true);
            }
            if slot.key == *key {
                return (slot.id, false);
            }
            at = (at + 1) & mask;
        }
    }

    /// Takes out every group, and keeps the size of the table.
    fn clear(&mut self) {
        if self.len > 0 {
            self.slots.fill(Slot { key: [0; W], id: NO_GROUP });
            self.len = 0;
        }
    }

    /// The number of groups that can go in with no growth of the table.
    fn room(&self) -> usize {
        (self.slots.len() / 2).saturating_sub(self.len)
    }

    /// Makes room for `more` more groups.
    fn reserve(&mut self, more: usize) {
        let want = ((self.len + more) * 2).next_power_of_two();
        if want > self.slots.len() {
            self.resize(want);
        }
    }
}

/// The words of a key of `GROUP BY` and its mask of nulls, the last word. A map for one key keeps the word of the key alone, and the group of the null key on the side. Keys of up to 7 words and the mask are kept in place, and a key of 4 to 7 words has words of 0 after it, so that a new group allocates no key.
enum KeyMap {
    One(WordTable<1>, Option<u32>),
    Two(WordTable<3>),
    Three(WordTable<4>),
    Wide(WordTable<8>),
    Large(FxMap<Box<[u64]>, u32>),
}

/// The words of a key of 4 to 7 words and its mask, with words of 0 after them.
#[inline]
fn wide(word: &[u64]) -> [u64; 8] {
    let mut key = [0; 8];
    key[..word.len()].copy_from_slice(word);
    key
}

impl KeyMap {
    /// A map for keys of `words` words with the mask, with room for `groups` groups.
    fn new(words: usize, groups: usize) -> KeyMap {
        match words {
            0..=2 => KeyMap::One(WordTable::new(groups), None),
            3 => KeyMap::Two(WordTable::new(groups)),
            4 => KeyMap::Three(WordTable::new(groups)),
            5..=8 => KeyMap::Wide(WordTable::new(groups)),
            _ => KeyMap::Large(FxMap::with_capacity_and_hasher(groups, rupg_kernels::hash::Build)),
        }
    }

    /// The hash of the key `word` for [`KeyMap::id`], or 0 for a map that finds its own.
    #[inline]
    fn hash(&self, word: &[u64]) -> u64 {
        match self {
            KeyMap::One(..) => WordTable::<1>::hash(&[word.first().copied().unwrap_or(0)]),
            KeyMap::Two(_) => WordTable::<3>::hash(&[word[0], word[1], word[2]]),
            KeyMap::Three(_) => WordTable::<4>::hash(&[word[0], word[1], word[2], word[3]]),
            KeyMap::Wide(_) => WordTable::<8>::hash(&wide(word)),
            KeyMap::Large(_) => 0,
        }
    }

    /// Starts the read of the slot of a key whose hash [`KeyMap::hash`] gave.
    #[inline]
    fn prefetch(&self, hash: u64) {
        match self {
            KeyMap::One(table, _) => table.prefetch(hash),
            KeyMap::Two(table) => table.prefetch(hash),
            KeyMap::Three(table) => table.prefetch(hash),
            KeyMap::Wide(table) => table.prefetch(hash),
            KeyMap::Large(_) => {}
        }
    }

    /// The group of the key `word`, whose hash [`KeyMap::hash`] gave, and true for a new group, which gets the ID `next`.
    #[inline]
    fn id(&mut self, word: &[u64], hash: u64, next: u32) -> (u32, bool) {
        match self {
            KeyMap::One(_, null) if word.get(1).is_some_and(|&mask| mask != 0) => match null {
                Some(id) => (*id, false),
                None => (*null.insert(next), true),
            },
            KeyMap::One(table, _) => table.id(&[word.first().copied().unwrap_or(0)], hash, next),
            KeyMap::Two(table) => table.id(&[word[0], word[1], word[2]], hash, next),
            KeyMap::Three(table) => table.id(&[word[0], word[1], word[2], word[3]], hash, next),
            KeyMap::Wide(table) => table.id(&wide(word), hash, next),
            KeyMap::Large(map) => match map.get(word) {
                Some(&id) => (id, false),
                None => {
                    map.insert(word.into(), next);
                    (next, true)
                }
            },
        }
    }

    /// The number of groups that can go in with no growth of the map.
    fn room(&self) -> usize {
        match self {
            KeyMap::One(table, _) => table.room(),
            KeyMap::Two(table) => table.room(),
            KeyMap::Three(table) => table.room(),
            KeyMap::Wide(table) => table.room(),
            KeyMap::Large(map) => map.capacity() - map.len(),
        }
    }

    /// Makes room for `more` more groups.
    fn reserve(&mut self, more: usize) {
        match self {
            KeyMap::One(table, _) => table.reserve(more),
            KeyMap::Two(table) => table.reserve(more),
            KeyMap::Three(table) => table.reserve(more),
            KeyMap::Wide(table) => table.reserve(more),
            KeyMap::Large(map) => map.reserve(more),
        }
    }
}

/// The physical type that a column of the type `ty` has in a segment.
fn kind_of(ty: u32) -> Option<Kind> {
    Some(match ty {
        oid::BOOL => Kind::Bool,
        oid::INT2 => Kind::Int2,
        oid::INT4 => Kind::Int4,
        oid::INT8 => Kind::Int8,
        oid::FLOAT4 => Kind::Float4,
        oid::FLOAT8 => Kind::Float8,
        oid::DATE => Kind::Date,
        oid::TIME => Kind::Time,
        oid::TIMESTAMP => Kind::Timestamp,
        oid::TIMESTAMPTZ => Kind::TimestampTz,
        oid::TEXT | oid::VARCHAR | oid::BPCHAR => Kind::Text,
        oid::BYTEA => Kind::Bytea,
        _ => return None,
    })
}

/// True for a collation whose equality is the equality of the bytes: the default collation, `C` and `POSIX`.
fn plain(collation: u32) -> bool {
    matches!(collation, 0 | 100 | 950 | 951)
}

/// The integer of a value of an integer type, a date or a time.
fn int_of(value: &Value) -> Option<i64> {
    Some(match value {
        Value::Bool(v) => i64::from(*v),
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        Value::Date(v) => i64::from(*v),
        Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v) => *v,
        _ => return None,
    })
}

/// The word of a float as a key: `-0` is `0`, and each NaN is the same NaN, as `float8eq` finds them equal.
fn float_word(v: f64) -> u64 {
    if v == 0.0 {
        0
    } else if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

/// The strings of a column of codes.
enum Dict {
    /// The dictionary of a column of the table.
    Stored(Arc<Stored>),
    /// The dictionary of the text values that an expression gives.
    Own(Dictionary),
}

impl Dict {
    fn value(&self, code: u32) -> Value {
        match self {
            Dict::Stored(stored) => {
                let dict = stored.dict.as_ref().map(|d| d.get(code)).unwrap_or_default();
                stored.kind.bytes_value(dict)
            }
            Dict::Own(dict) => Kind::Text.bytes_value(dict.get(code)),
        }
    }

    /// Whether the codes are of text, so that [`Dict::text`] gives their bytes.
    fn is_text(&self) -> bool {
        match self {
            Dict::Stored(stored) => stored.kind == Kind::Text,
            Dict::Own(_) => true,
        }
    }

    /// The bytes of the text of the code `code`, or `None` for a dictionary of values that are not text.
    fn text(&self, code: u32) -> Option<&[u8]> {
        match self {
            Dict::Stored(stored) if stored.kind == Kind::Text => {
                Some(stored.dict.as_ref().map(|d| d.get(code)).unwrap_or_default())
            }
            Dict::Stored(_) => None,
            Dict::Own(dict) => Some(dict.get(code)),
        }
    }

    fn find(&self, bytes: &[u8]) -> u32 {
        match self {
            Dict::Stored(stored) => stored.dict.as_ref().map_or(MISSING, |d| d.find(bytes)),
            Dict::Own(dict) => dict.find(bytes),
        }
    }
}

/// The values of an expression for the rows of a selection.
enum Data {
    Int(Vec<i64>),
    Float(Vec<f64>),
    /// Codes into a dictionary of [`Exec::dicts`]. A null is the code 0.
    Code(Vec<u32>, usize),
    Values(Vec<Value>),
    /// The same value for each row.
    Const(Value),
}

/// The integers of a column for a loop on the rows: a vector with its null rows, or one value for each row.
#[derive(Clone, Copy)]
enum Ints<'a> {
    Vector(&'a [i64], Option<&'a [bool]>),
    Const(Option<i64>),
}

impl Ints<'_> {
    #[inline]
    fn get(self, i: usize) -> Option<i64> {
        match self {
            Ints::Vector(v, None) => Some(v[i]),
            Ints::Vector(v, Some(nulls)) => (!nulls[i]).then(|| v[i]),
            Ints::Const(v) => v,
        }
    }
}

/// The values of an expression of the type `ty` for the rows of a selection, with the null rows. A `boolean` has the integers 0 and 1. A `numeric` of [`Data::Int`] is an integer.
struct Col {
    data: Data,
    nulls: Option<Vec<bool>>,
    ty: u32,
}

impl Col {
    fn constant(value: Value, ty: u32) -> Col {
        Col { data: Data::Const(value), nulls: None, ty }
    }

    fn bools(values: &[Option<bool>]) -> Col {
        let nulls = values
            .iter()
            .any(Option::is_none)
            .then(|| values.iter().map(Option::is_none).collect());
        let data = Data::Int(values.iter().map(|v| i64::from(v.unwrap_or(false))).collect());
        Col { data, nulls, ty: oid::BOOL }
    }

    #[inline]
    fn null(&self, i: usize) -> bool {
        match &self.data {
            Data::Const(v) => v.is_null(),
            Data::Values(v) => v[i].is_null(),
            _ => self.nulls.as_ref().is_some_and(|n| n[i]),
        }
    }

    /// The value of a row as a condition: `None` for null.
    #[inline]
    fn truth(&self, i: usize) -> Option<bool> {
        if self.null(i) {
            return None;
        }
        match &self.data {
            Data::Int(v) => Some(v[i] != 0),
            Data::Const(Value::Bool(b)) => Some(*b),
            Data::Values(v) => match v[i] {
                Value::Bool(b) => Some(b),
                _ => None,
            },
            _ => None,
        }
    }

    /// The integer of a row of [`Data::Int`] or of a constant integer, or `None` for null.
    #[inline]
    fn int(&self, i: usize) -> Option<i64> {
        if self.null(i) {
            return None;
        }
        match &self.data {
            Data::Int(v) => Some(v[i]),
            Data::Const(v) => int_of(v),
            _ => None,
        }
    }

    /// The integers of [`Col::int`] with the kind of the data found once, for a loop on the rows.
    fn ints(&self) -> Option<Ints<'_>> {
        match &self.data {
            Data::Int(v) => Some(Ints::Vector(v, self.nulls.as_deref())),
            Data::Const(v) if v.is_null() => Some(Ints::Const(None)),
            Data::Const(v) => Some(Ints::Const(Some(int_of(v)?))),
            _ => None,
        }
    }

    fn is_int(&self) -> bool {
        match &self.data {
            Data::Int(_) => true,
            Data::Const(v) => v.is_null() || int_of(v).is_some(),
            _ => false,
        }
    }

    /// The value of a row.
    fn value(&self, i: usize, dicts: &[Dict]) -> Value {
        if self.null(i) {
            return Value::Null;
        }
        match &self.data {
            Data::Int(v) => int_value(self.ty, v[i]),
            Data::Float(v) => {
                if self.ty == oid::FLOAT4 {
                    Value::Float4(v[i] as f32)
                } else {
                    Value::Float8(v[i])
                }
            }
            Data::Code(v, d) => dicts[*d].value(v[i]),
            Data::Values(v) => v[i].clone(),
            Data::Const(v) => v.clone(),
        }
    }

    /// The word of a row as a key of a group, or `None` for a value that has no word.
    #[inline]
    fn word(&self, i: usize) -> Option<u64> {
        Some(match &self.data {
            Data::Int(v) => v[i] as u64,
            Data::Float(v) => float_word(v[i]),
            Data::Code(v, _) => u64::from(v[i]),
            Data::Const(v) => int_of(v).map_or(0, |v| v as u64),
            Data::Values(_) => return None,
        })
    }
}

/// The value of the integer `v` of a column of the type `ty`.
fn int_value(ty: u32, v: i64) -> Value {
    match ty {
        oid::NUMERIC => Value::Numeric(Numeric::from_integer(i128::from(v))),
        _ => kind_of(ty).map_or(Value::Int8(v), |k| k.int_value(v)),
    }
}

/// The values of a column in one vector.
struct Decoded {
    data: DecodedData,
    nulls: Option<Vec<bool>>,
}

enum DecodedData {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Code(Vec<u32>),
}

/// One vector of the table.
struct Batch {
    segment: usize,
    vector: usize,
    rows: usize,
    /// The decoded vector of each column that the scan read, by its place in [`Exec::stored`].
    decoded: Vec<Option<Decoded>>,
}

/// A memo of the values of an expression of one column, by the value of the column.
enum Memo {
    /// By the code, for a column of codes.
    Code(Vec<Option<(Value, u32)>>),
    /// By the word of the integer or the float.
    Word(FxMap<u64, (Value, u32)>),
}

/// An expression that the scan runs with [`Eval`]: a copy whose columns are parameters after the parameters of the query.
struct Generic {
    template: Expr,
    /// The columns of the expression, in the order of their parameters.
    vars: Vec<Var>,
    memo: Option<Memo>,
    /// The memo of a null value of the column.
    null: Option<(Value, u32)>,
    /// The dictionary of the text values of the expression.
    dict: Option<usize>,
}

/// The state of the scan of one query.
struct Exec<'q> {
    params: &'q [Value],
    session: &'q dyn Session,
    cache: &'q Cache,
    /// The columns that the scan reads.
    stored: Vec<Arc<Stored>>,
    /// The place of each column of the table in `stored`, or `usize::MAX`.
    places: Vec<usize>,
    /// The type of each column of the table.
    types: Vec<u32>,
    /// The dictionary of each column of codes in `stored`, by its place.
    dict_of: Vec<usize>,
    dicts: Vec<Dict>,
    generic: FxMap<*const Expr, Generic>,
    /// For each `LIKE` of a column of codes and a constant pattern, the result for each code: 0 for a code that it has not seen, 1 for false and 2 for true.
    likes: FxMap<*const Expr, Vec<u8>>,
    /// For each dictionary, the number of characters of each code plus one, or 0 for a code that `length` has not seen.
    lengths: FxMap<usize, Vec<u32>>,
    /// The parameters of the query, then the values of the columns of a [`Generic`].
    slots: Vec<Value>,
    empty: scan::Tables,
}

/// The number of characters of the bytes of a `String`: the bytes less the bytes that continue a character, with the form `10xxxxxx`. Eight bytes are tested at a time.
fn chars(text: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let (words, rest) = text.as_chunks::<8>();
    let mut continued = 0;
    for &word in words {
        let w = u64::from_le_bytes(word);
        continued += (w & !(w << 1) & HIGH).count_ones() as usize;
    }
    continued += rest.iter().filter(|&&b| b & 0xc0 == 0x80).count();
    text.len() - continued
}

/// The source name of the function of a call.
fn src(oid: u32) -> &'static str {
    builtin::proc_by_oid(oid).map_or("", |p| p.src)
}

/// The comparison of a function of integers, dates, times and booleans.
fn int_compare(src: &str) -> Option<fn(i64, i64) -> bool> {
    let (head, op) = src.split_at_checked(src.len().checked_sub(2)?)?;
    let head = head.strip_suffix('_').unwrap_or(head);
    if !matches!(
        head,
        "int2"
            | "int4"
            | "int8"
            | "int24"
            | "int42"
            | "int28"
            | "int82"
            | "int48"
            | "int84"
            | "date"
            | "timestamp"
            | "timestamptz"
            | "time"
            | "bool"
    ) {
        return None;
    }
    Some(match op {
        "eq" => |a, b| a == b,
        "ne" => |a, b| a != b,
        "lt" => |a, b| a < b,
        "le" => |a, b| a <= b,
        "gt" => |a, b| a > b,
        "ge" => |a, b| a >= b,
        _ => return None,
    })
}

/// The arithmetic of a function of integers.
fn int_arith(src: &str) -> Option<fn(i64, i64) -> Option<i64>> {
    let head = ["int2", "int4", "int8", "int24", "int42", "int28", "int82", "int48", "int84"]
        .into_iter()
        .filter(|h| src.starts_with(h))
        .max_by_key(|h| h.len())?;
    Some(match &src[head.len()..] {
        "pl" => i64::checked_add,
        "mi" => i64::checked_sub,
        "mul" => i64::checked_mul,
        _ => return None,
    })
}

/// The range of the integer type `ty`.
fn int_range(ty: u32) -> Option<(i64, i64)> {
    Some(match ty {
        oid::INT2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
        oid::INT4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        oid::INT8 => (i64::MIN, i64::MAX),
        _ => return None,
    })
}

/// The microseconds of the unit of `date_trunc` and `extract` that the scan computes for a `timestamp`.
fn unit(field: &Expr) -> Option<i64> {
    let ExprKind::Const(Value::Text(name)) = &field.kind else { return None };
    Some(match name.as_str() {
        "second" => 1_000_000,
        "minute" => 60_000_000,
        "hour" => 3_600_000_000,
        "day" => 86_400_000_000,
        _ => return None,
    })
}

/// The values of the vector `vector` of the segment `segment` of a column, with the code 0 for a null code.
fn decode(stored: &Stored, segment: usize, vector: usize) -> Result<Decoded> {
    let column = &stored.segments[segment];
    let at = vector;
    let vector = &column.vectors[at];
    let rows = vector.rows as usize;
    let nulls = match &vector.nulls {
        Nulls::None => None,
        Nulls::All => Some(vec![true; rows]),
        some => Some((0..rows).map(|i| some.is_null(i)).collect()),
    };
    let all = vector.nulls == Nulls::All;
    let data = match column.kind.store() {
        Store::Int => {
            let mut out = Vec::new();
            if all {
                out.resize(rows, 0);
            } else {
                column.decode_ints(at, &mut out)?;
            }
            DecodedData::Int(out)
        }
        Store::Float => {
            let mut out = Vec::new();
            if all {
                out.resize(rows, 0.0);
            } else {
                column.decode_floats(at, &mut out)?;
            }
            DecodedData::Float(out)
        }
        Store::Code => {
            let mut out = Vec::new();
            if all {
                out.resize(rows, 0);
            } else {
                column.decode_codes(at, &mut out)?;
            }
            if let Some(nulls) = &nulls {
                for (code, null) in out.iter_mut().zip(nulls) {
                    if *null {
                        *code = 0;
                    }
                }
            }
            DecodedData::Code(out)
        }
    };
    if data_len(&data) != rows {
        return Err(Error::corrupted("a vector has another number of values than its rows"));
    }
    Ok(Decoded { data, nulls })
}

impl<'q> Exec<'q> {
    /// The vector of the column `column` for the selection `sel`.
    fn var(&mut self, batch: &mut Batch, column: usize, ty: u32, sel: &[u32]) -> Result<Col> {
        let place = self.places[column];
        self.decode(batch, place)?;
        let Some(decoded) = &batch.decoded[place] else {
            return Err(Error::internal("a vector is not decoded"));
        };
        let nulls = decoded.nulls.as_ref().map(|n| sel.iter().map(|&i| n[i as usize]).collect());
        let data = match &decoded.data {
            DecodedData::Int(v) => Data::Int(sel.iter().map(|&i| v[i as usize]).collect()),
            DecodedData::Float(v) => Data::Float(sel.iter().map(|&i| v[i as usize]).collect()),
            DecodedData::Code(v) => {
                Data::Code(sel.iter().map(|&i| v[i as usize]).collect(), self.dict_of[place])
            }
        };
        Ok(Col { data, nulls, ty })
    }

    fn decode(&self, batch: &mut Batch, place: usize) -> Result<()> {
        if batch.decoded[place].is_some() {
            return Ok(());
        }
        batch.decoded[place] = Some(decode(&self.stored[place], batch.segment, batch.vector)?);
        Ok(())
    }

    /// The values of `expr` for the rows `sel` of the batch.
    fn eval(&mut self, expr: &Expr, batch: &mut Batch, sel: &[u32]) -> Result<Col> {
        match &expr.kind {
            ExprKind::Var(var) => {
                return self.var(batch, usize::try_from(var.attnum - 1).unwrap_or(0), expr.ty, sel);
            }
            ExprKind::Const(value) => return Ok(Col::constant(value.clone(), expr.ty)),
            ExprKind::Param(n) => {
                let value = self.params.get(n.wrapping_sub(1)).cloned().unwrap_or(Value::Null);
                return Ok(Col::constant(value, expr.ty));
            }
            ExprKind::Relabel(arg, _)
                if kind_of(expr.ty).is_some() && kind_of(expr.ty) == kind_of(arg.ty) =>
            {
                let mut col = self.eval(arg, batch, sel)?;
                col.ty = expr.ty;
                return Ok(col);
            }
            ExprKind::Bool(op, args) => return self.boolean(*op, args, batch, sel),
            ExprKind::NullTest(arg, is_null) if kind_of(arg.ty).is_some() => {
                let col = self.eval(arg, batch, sel)?;
                if !matches!(col.data, Data::Values(_)) {
                    let out: Vec<Option<bool>> =
                        (0..sel.len()).map(|i| Some(col.null(i) == *is_null)).collect();
                    return Ok(Col::bools(&out));
                }
            }
            ExprKind::Func(func) if !func.retset => {
                if let Some(col) = self.func(expr, &func.args, src(func.oid), batch, sel)? {
                    return Ok(col);
                }
            }
            ExprKind::Case(case) if case.arg.is_none() => {
                if let Some(col) = self.case(expr, case, batch, sel)? {
                    return Ok(col);
                }
            }
            _ => {}
        }
        self.generic(expr, batch, sel)
    }

    /// A `CASE` with no argument whose results are integers, or codes of one dictionary and constant texts that the dictionary has, so that equal texts have equal codes. Each condition runs on the rows that no condition before it took, and each result runs only on its rows, as the row engine evaluates the result of the first condition that is true. `None` for other results, for which the row engine evaluates the expression.
    fn case(
        &mut self,
        expr: &Expr,
        case: &rupg_analyze::Case,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Option<Col>> {
        if kind_of(expr.ty).is_none() {
            return Ok(None);
        }
        let results: Vec<&Expr> =
            case.whens.iter().map(|(_, r)| r).chain(std::iter::once(&*case.default)).collect();
        // The result of each row, by its place in `results`.
        let mut branch = vec![case.whens.len(); sel.len()];
        let mut live: Vec<usize> = (0..sel.len()).collect();
        let mut sub = Vec::new();
        for (b, (cond, _)) in case.whens.iter().enumerate() {
            if live.is_empty() {
                break;
            }
            sub.clear();
            sub.extend(live.iter().map(|&i| sel[i]));
            let col = self.eval(cond, batch, &sub)?;
            let mut next = Vec::with_capacity(live.len());
            for (k, &i) in live.iter().enumerate() {
                if col.truth(k) == Some(true) {
                    branch[i] = b;
                } else {
                    next.push(i);
                }
            }
            live = next;
        }
        let mut rows: Vec<Vec<usize>> = vec![Vec::new(); results.len()];
        for (i, &b) in branch.iter().enumerate() {
            rows[b].push(i);
        }
        let mut cols = Vec::with_capacity(results.len());
        for (result, rows) in results.iter().zip(&rows) {
            if rows.is_empty() {
                cols.push(None);
                continue;
            }
            sub.clear();
            sub.extend(rows.iter().map(|&i| sel[i]));
            cols.push(Some(self.eval(result, batch, &sub)?));
        }
        // The dictionary of the codes, from the first result that has codes.
        let dict = cols.iter().flatten().find_map(|c| match &c.data {
            Data::Code(_, d) => Some(*d),
            _ => None,
        });
        let mut words = vec![0_i64; sel.len()];
        let mut nulls = vec![false; sel.len()];
        for (col, rows) in cols.iter().zip(&rows) {
            let Some(col) = col else { continue };
            for (k, &i) in rows.iter().enumerate() {
                if col.null(k) {
                    nulls[i] = true;
                    continue;
                }
                words[i] = match (&col.data, dict) {
                    (Data::Int(v), None) => v[k],
                    (Data::Code(v, d), Some(dict)) if *d == dict => i64::from(v[k]),
                    (Data::Const(value), None) => match int_of(value) {
                        Some(v) => v,
                        None => return Ok(None),
                    },
                    (Data::Const(Value::Text(text)), Some(dict)) if self.dicts[dict].is_text() => {
                        let code = self.dicts[dict].find(text.as_bytes());
                        if code == MISSING {
                            return Ok(None);
                        }
                        i64::from(code)
                    }
                    _ => return Ok(None),
                };
            }
        }
        let nulls = nulls.contains(&true).then_some(nulls);
        let data = match dict {
            Some(d) => Data::Code(words.into_iter().map(|w| w as u32).collect(), d),
            None => Data::Int(words),
        };
        Ok(Some(Col { data, nulls, ty: expr.ty }))
    }

    /// `AND`, `OR` and `NOT` with the logic of three values. Each argument runs only on the rows whose value it can change, as the row engine stops at the first argument that decides the value.
    fn boolean(
        &mut self,
        op: BoolOp,
        args: &[Expr],
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Col> {
        if op == BoolOp::Not {
            let col = self.eval(&args[0], batch, sel)?;
            let out: Vec<Option<bool>> = (0..sel.len()).map(|i| col.truth(i).map(|b| !b)).collect();
            return Ok(Col::bools(&out));
        }
        // The value that decides: `false` for `AND`, `true` for `OR`.
        let decides = op == BoolOp::Or;
        let mut out = vec![Some(!decides); sel.len()];
        let mut live: Vec<usize> = (0..sel.len()).collect();
        let mut sub = Vec::new();
        for arg in args {
            if live.is_empty() {
                break;
            }
            sub.clear();
            sub.extend(live.iter().map(|&i| sel[i]));
            let col = self.eval(arg, batch, &sub)?;
            let mut next = Vec::with_capacity(live.len());
            for (k, &i) in live.iter().enumerate() {
                match col.truth(k) {
                    Some(b) if b == decides => out[i] = Some(decides),
                    None => {
                        out[i] = None;
                        next.push(i);
                    }
                    Some(_) => next.push(i),
                }
            }
            live = next;
        }
        Ok(Col::bools(&out))
    }

    /// A call of a function that the scan computes on the vectors, or `None`.
    fn func(
        &mut self,
        expr: &Expr,
        args: &[Expr],
        src: &str,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Option<Col>> {
        if let (Some(compare), [a, b]) = (int_compare(src), args) {
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            if let (Some(a), Some(b)) = (a.ints(), b.ints()) {
                let out: Vec<Option<bool>> =
                    (0..sel.len()).map(|i| Some(compare(a.get(i)?, b.get(i)?))).collect();
                return Ok(Some(Col::bools(&out)));
            }
            return Ok(None);
        }
        if let ("texteq" | "textne", [a, b]) = (src, args) {
            let equal = src == "texteq";
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            let (code, other) = match (&a.data, &b.data) {
                (Data::Code(..), _) => (&a, &b),
                (_, Data::Code(..)) => (&b, &a),
                _ => return Ok(None),
            };
            let Data::Code(codes, d) = &code.data else { return Ok(None) };
            let out: Vec<Option<bool>> = match &other.data {
                Data::Const(Value::Null) => vec![None; sel.len()],
                Data::Const(Value::Text(text)) => {
                    let want = self.dicts[*d].find(text.as_bytes());
                    codes.iter().map(|&c| (c != 0).then_some((c == want) == equal)).collect()
                }
                Data::Code(others, e) if e == d => codes
                    .iter()
                    .zip(others)
                    .map(|(&c, &o)| (c != 0 && o != 0).then_some((c == o) == equal))
                    .collect(),
                _ => return Ok(None),
            };
            return Ok(Some(Col::bools(&out)));
        }
        if let ("textlike" | "textnlike" | "texticlike" | "texticnlike", [a, b]) = (src, args) {
            return self.like(expr, src, a, b, batch, sel);
        }
        if let ("textlen" | "textoctetlen", [a]) = (src, args)
            && matches!(a.kind, ExprKind::Var(_))
        {
            return self.length(expr, src == "textoctetlen", a, batch, sel);
        }
        if let (Some(op), Some((low, high)), [a, b]) = (int_arith(src), int_range(expr.ty), args) {
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            let (Some(a), Some(b)) = (a.ints(), b.ints()) else { return Ok(None) };
            if let (Ints::Vector(x, None), Ints::Const(Some(y))) = (a, b) {
                // A column and a constant with no null: the common case, as in `ResolutionWidth + 1`.
                let mut values = Vec::with_capacity(x.len());
                for &x in x {
                    match op(x, y).filter(|v| (low..=high).contains(v)) {
                        Some(v) => values.push(v),
                        None => return Ok(None),
                    }
                }
                return Ok(Some(Col { data: Data::Int(values), nulls: None, ty: expr.ty }));
            }
            let mut values = Vec::with_capacity(sel.len());
            let mut nulls = Vec::with_capacity(sel.len());
            for i in 0..sel.len() {
                match (a.get(i), b.get(i)) {
                    (Some(x), Some(y)) => match op(x, y).filter(|v| (low..=high).contains(v)) {
                        Some(v) => {
                            values.push(v);
                            nulls.push(false);
                        }
                        // The row engine gives the error of the overflow.
                        None => return Ok(None),
                    },
                    _ => {
                        values.push(0);
                        nulls.push(true);
                    }
                }
            }
            let nulls = nulls.contains(&true).then_some(nulls);
            return Ok(Some(Col { data: Data::Int(values), nulls, ty: expr.ty }));
        }
        if let ("timestamp_trunc" | "extract_timestamp", [field, arg]) = (src, args)
            && arg.ty == oid::TIMESTAMP
            && let Some(unit) = unit(field)
        {
            let trunc = src == "timestamp_trunc";
            if !trunc && !(expr.ty == oid::NUMERIC && matches!(unit, 60_000_000 | 3_600_000_000)) {
                return Ok(None);
            }
            let col = self.eval(arg, batch, sel)?;
            if !col.is_int() {
                return Ok(None);
            }
            let mut values = Vec::with_capacity(sel.len());
            let mut nulls = Vec::with_capacity(sel.len());
            for i in 0..sel.len() {
                let value = col.int(i).and_then(|v| {
                    let infinite = v == i64::MIN || v == i64::MAX;
                    if trunc {
                        Some(if infinite { v } else { v - v.rem_euclid(unit) })
                    } else if infinite {
                        None
                    } else {
                        let span = if unit == 60_000_000 { 60 } else { 24 };
                        Some(v.div_euclid(unit).rem_euclid(span))
                    }
                });
                values.push(value.unwrap_or(0));
                nulls.push(value.is_none());
            }
            let nulls = nulls.contains(&true).then_some(nulls);
            return Ok(Some(Col { data: Data::Int(values), nulls, ty: expr.ty }));
        }
        Ok(None)
    }

    /// `LIKE` and `ILIKE` of a column of codes of the table and a constant pattern, which tests the bytes of each code once with no copy of the text, or `None` for other arguments. A pattern with an error gives `None`, and the row engine gives the same result.
    fn like(
        &mut self,
        expr: &Expr,
        src: &str,
        a: &Expr,
        b: &Expr,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Option<Col>> {
        let pattern = match &b.kind {
            ExprKind::Const(Value::Text(p)) => p.clone(),
            ExprKind::Const(Value::Null) => return Ok(Some(Col::bools(&vec![None; sel.len()]))),
            _ => return Ok(None),
        };
        if !matches!(a.kind, ExprKind::Var(_)) {
            return Ok(None);
        }
        let fold = matches!(src, "texticlike" | "texticnlike");
        let negate = matches!(src, "textnlike" | "texticnlike");
        let pattern = if fold { pattern.to_ascii_lowercase() } else { pattern };
        let col = self.eval(a, batch, sel)?;
        let Data::Code(codes, d) = &col.data else { return Ok(None) };
        let key = std::ptr::from_ref(expr);
        let mut memo = self.likes.remove(&key).unwrap_or_default();
        let mut folded = Vec::new();
        let mut out = Vec::with_capacity(sel.len());
        let mut failed = false;
        for (i, &code) in codes.iter().enumerate() {
            if col.null(i) {
                out.push(None);
                continue;
            }
            let at = code as usize;
            if memo.len() <= at {
                memo.resize(at + 1, 0);
            }
            if memo[at] == 0 {
                // The dictionary of a text holds only the bytes of a `String`, so the text is UTF8.
                let Some(text) = self.dicts[*d].text(code) else {
                    failed = true;
                    break;
                };
                let text = if fold {
                    folded.clear();
                    folded.extend(text.iter().map(u8::to_ascii_lowercase));
                    &folded[..]
                } else {
                    text
                };
                let Ok(found) = rupg_func::like_text(text, pattern.as_bytes()) else {
                    failed = true;
                    break;
                };
                memo[at] = 1 + u8::from(found);
            }
            out.push(Some((memo[at] == 2) != negate));
        }
        self.likes.insert(key, memo);
        Ok((!failed).then(|| Col::bools(&out)))
    }

    /// `length` and `octet_length` of a column of codes of the table, which count the bytes of each code with no copy of the text, or `None` for other arguments.
    fn length(
        &mut self,
        expr: &Expr,
        octets: bool,
        a: &Expr,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Option<Col>> {
        let col = self.eval(a, batch, sel)?;
        let Data::Code(codes, d) = &col.data else { return Ok(None) };
        let mut values = Vec::with_capacity(sel.len());
        let dict = &self.dicts[*d];
        // The characters of a code are counted once for the query.
        let memo = self.lengths.entry(*d).or_default();
        for (i, &code) in codes.iter().enumerate() {
            if col.null(i) {
                values.push(0);
                continue;
            }
            let Some(text) = dict.text(code) else { return Ok(None) };
            if octets {
                values.push(i64::try_from(text.len()).unwrap_or(i64::MAX));
                continue;
            }
            let at = code as usize;
            if memo.len() <= at {
                memo.resize(at + 1, 0);
            }
            if memo[at] == 0 {
                // The dictionary of a text column holds only the bytes of a `String`, so each byte that does not continue a character starts one.
                memo[at] = u32::try_from(chars(text)).map_or(u32::MAX, |n| n.saturating_add(1));
            }
            values.push(i64::from(memo[at] - 1));
        }
        Ok(Some(Col { data: Data::Int(values), nulls: col.nulls, ty: expr.ty }))
    }

    /// The values of `expr` from [`Eval`], with its columns as parameters. An expression of one column keeps its value for each value of the column.
    fn generic(&mut self, expr: &Expr, batch: &mut Batch, sel: &[u32]) -> Result<Col> {
        let key = std::ptr::from_ref(expr);
        let mut generic = match self.generic.remove(&key) {
            Some(generic) => generic,
            None => self.template(expr),
        };
        let result = self.run_generic(&mut generic, expr, batch, sel);
        self.generic.insert(key, generic);
        result
    }

    fn template(&mut self, expr: &Expr) -> Generic {
        let mut vars: Vec<Var> = Vec::new();
        let mut template = expr.clone();
        let base = self.params.len();
        template.each_mut(0, &mut |e, _| {
            if let ExprKind::Var(var) = e.kind {
                let at = match vars.iter().position(|v| v.attnum == var.attnum) {
                    Some(at) => at,
                    None => {
                        vars.push(var);
                        vars.len() - 1
                    }
                };
                e.kind = ExprKind::Param(base + at + 1);
                return true;
            }
            false
        });
        let dict = (kind_of(expr.ty) == Some(Kind::Text)).then(|| {
            self.dicts.push(Dict::Own(Dictionary::new()));
            self.dicts.len() - 1
        });
        Generic { template, vars, memo: None, null: None, dict }
    }

    /// The value of the template of `generic` with the slots of its columns.
    fn call(&mut self, generic: &Generic) -> Result<(Value, u32)> {
        let value = {
            let mut eval = Eval::new(&self.slots, self.session, &self.empty, self.cache, &[]);
            eval.eval(&generic.template)?
        };
        let code = match (&value, generic.dict) {
            (Value::Text(text), Some(d)) => match &mut self.dicts[d] {
                Dict::Own(dict) => dict.intern(text.as_bytes()),
                Dict::Stored(_) => 0,
            },
            _ => 0,
        };
        Ok((value, code))
    }

    fn run_generic(
        &mut self,
        generic: &mut Generic,
        expr: &Expr,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Col> {
        let base = self.params.len();
        self.slots.truncate(base);
        self.slots.resize(base + generic.vars.len(), Value::Null);
        let mut inputs = Vec::with_capacity(generic.vars.len());
        for var in &generic.vars {
            let column = usize::try_from(var.attnum - 1).unwrap_or(0);
            inputs.push(self.var(batch, column, self.types[column], sel)?);
        }
        let mut out: Vec<(Value, u32)> = Vec::with_capacity(sel.len());
        match inputs.as_slice() {
            [] => {
                if generic.null.is_none() {
                    generic.null = Some(self.call(generic)?);
                }
                let (value, _) = generic.null.clone().unwrap_or((Value::Null, 0));
                return Ok(Col::constant(value, expr.ty));
            }
            [input] => {
                for i in 0..sel.len() {
                    if input.null(i) {
                        if generic.null.is_none() {
                            self.slots[base] = Value::Null;
                            generic.null = Some(self.call(generic)?);
                        }
                        out.push(generic.null.clone().unwrap_or((Value::Null, 0)));
                        continue;
                    }
                    let found = match (&generic.memo, &input.data) {
                        (Some(Memo::Code(memo)), Data::Code(codes, _)) => {
                            memo.get(codes[i] as usize).and_then(Option::as_ref)
                        }
                        (Some(Memo::Word(memo)), _) => input.word(i).and_then(|w| memo.get(&w)),
                        _ => None,
                    };
                    if let Some(found) = found {
                        // A text with a code is only its code in the column, so the text is not copied.
                        out.push(match found {
                            (Value::Text(_), code) if *code != 0 => {
                                (Value::Text(String::new()), *code)
                            }
                            found => found.clone(),
                        });
                        continue;
                    }
                    self.slots[base] = input.value(i, &self.dicts);
                    let result = match self.call(generic)? {
                        // The column keeps only the code of a text with a code, so the memo and the output do not keep a copy of the text.
                        (Value::Text(_), code) if code != 0 => (Value::Text(String::new()), code),
                        result => result,
                    };
                    match (&mut generic.memo, &input.data) {
                        (memo @ None, Data::Code(..)) => *memo = Some(Memo::Code(Vec::new())),
                        (memo @ None, Data::Int(_) | Data::Float(_)) => {
                            *memo = Some(Memo::Word(FxMap::default()));
                        }
                        _ => {}
                    }
                    match (&mut generic.memo, &input.data) {
                        (Some(Memo::Code(memo)), Data::Code(codes, _)) => {
                            let at = codes[i] as usize;
                            if memo.len() <= at {
                                memo.resize(at + 1, None);
                            }
                            memo[at] = Some(result.clone());
                        }
                        (Some(Memo::Word(memo)), _) if memo.len() < MEMO => {
                            if let Some(w) = input.word(i) {
                                memo.insert(w, result.clone());
                            }
                        }
                        _ => {}
                    }
                    out.push(result);
                }
            }
            _ => {
                for i in 0..sel.len() {
                    for (k, input) in inputs.iter().enumerate() {
                        self.slots[base + k] = input.value(i, &self.dicts);
                    }
                    out.push(self.call(generic)?);
                }
            }
        }
        Ok(self.collect(expr.ty, generic.dict, out))
    }

    /// A column of the values of a [`Generic`].
    fn collect(&self, ty: u32, dict: Option<usize>, out: Vec<(Value, u32)>) -> Col {
        let nulls: Vec<bool> = out.iter().map(|(v, _)| v.is_null()).collect();
        let any = nulls.contains(&true);
        if let Some(d) = dict
            && out.iter().all(|(v, c)| v.is_null() || *c != 0)
        {
            let codes = out.iter().map(|(_, c)| *c).collect();
            return Col { data: Data::Code(codes, d), nulls: any.then_some(nulls), ty };
        }
        match kind_of(ty).map(Kind::store) {
            Some(Store::Int) if ty != oid::NUMERIC => {
                let ints: Option<Vec<i64>> = out
                    .iter()
                    .map(|(v, _)| if v.is_null() { Some(0) } else { int_of(v) })
                    .collect();
                if let Some(ints) = ints {
                    return Col { data: Data::Int(ints), nulls: any.then_some(nulls), ty };
                }
            }
            Some(Store::Float) => {
                let floats: Option<Vec<f64>> = out
                    .iter()
                    .map(|(v, _)| match v {
                        Value::Null => Some(0.0),
                        Value::Float4(f) => Some(f64::from(*f)),
                        Value::Float8(f) => Some(*f),
                        _ => None,
                    })
                    .collect();
                if let Some(floats) = floats {
                    return Col { data: Data::Float(floats), nulls: any.then_some(nulls), ty };
                }
            }
            _ => {}
        }
        // The text of a value with a code is in the dictionary of the code.
        let values = out
            .into_iter()
            .map(|(v, c)| match (&v, dict) {
                (Value::Text(_), Some(d)) if c != 0 => self.dicts[d].value(c),
                _ => v,
            })
            .collect();
        Col { data: Data::Values(values), nulls: None, ty }
    }
}

fn data_len(data: &DecodedData) -> usize {
    match data {
        DecodedData::Int(v) => v.len(),
        DecodedData::Float(v) => v.len(),
        DecodedData::Code(v) => v.len(),
    }
}

/// An expression that the scan cannot run: a subquery, a column of another query or a system column, a call that is volatile, or an aggregate that the scan does not make.
fn unsupported(expr: &Expr) -> bool {
    crate::volatile(expr)
        || expr
            .find(0, &mut |e, _| {
                let bad = match &e.kind {
                    ExprKind::SubLink(_)
                    | ExprKind::SubColumn(_)
                    | ExprKind::Window(_)
                    | ExprKind::Grouping(_)
                    | ExprKind::PlaceHolder(_)
                    | ExprKind::CurrentOf(_)
                    | ExprKind::JsonTable(_) => true,
                    ExprKind::Var(var) => var.levels_up != 0 || var.attnum <= 0,
                    ExprKind::Collate(_, collation) => !plain(*collation),
                    ExprKind::Agg(agg) => {
                        agg.levels_up != 0
                            || agg.kind != b'n'
                            || !agg.order.is_empty()
                            || agg.filter.is_some()
                            || (!agg.distinct.is_empty() && !distinct_count(agg))
                    }
                    _ => false,
                };
                bad.then_some(())
            })
            .is_some()
}

/// True for `count(DISTINCT x)` of a built-in `count`.
fn distinct_count(agg: &Aggref) -> bool {
    agg.oid < rupg_catalog::FIRST_NORMAL_OID
        && agg.nargs == 1
        && agg.args.len() == 1
        && builtin::proc_by_oid(agg.oid).is_some_and(|p| p.name == "count")
        && agg.distinct.iter().all(|d| plain(d.collation))
}

/// The columns that `expr` reads outside the arguments of its aggregate calls.
fn outside_aggs(expr: &Expr, out: &mut [bool]) {
    match &expr.kind {
        ExprKind::Agg(_) => {}
        ExprKind::Var(var) => {
            if let Some(slot) = usize::try_from(var.attnum - 1).ok().and_then(|i| out.get_mut(i)) {
                *slot = true;
            }
        }
        _ => {
            for child in expr.children() {
                outside_aggs(child, out);
            }
        }
    }
}

/// How the scan makes an aggregate.
enum Method<'q> {
    /// `count(*)`.
    Count,
    /// `count(x)`.
    CountArg,
    /// `count(DISTINCT x)`.
    Distinct,
    /// `sum` of `smallint` or `integer`, which gives `bigint`.
    SumInt,
    /// `sum` of `bigint` and `avg` of integers, which give `numeric`.
    Int128 {
        avg: bool,
    },
    Min,
    Max,
    /// `sum` or `avg` of `x + by`: the sums of the column `x` in `bases[base]` and `by` times the count. `wide` is `None` for [`Method::SumInt`], else the `avg` of [`Method::Int128`]. The values of `x + by` must be in `range`, else the row engine runs the query and gives the error.
    Shift {
        base: usize,
        by: i64,
        range: (i64, i64),
        wide: Option<bool>,
    },
    /// The method of the row engine, with the values of the arguments.
    Row(agg::Agg<'q>),
}

/// A column that the calls of [`Method::Shift`] sum: the count and the sum of each group, with the smallest and the largest value of all the groups.
struct Base<'q> {
    var: &'q Expr,
    states: Vec<(i64, i128)>,
    extremes: Option<(i64, i64)>,
}

/// The column `x` and the integer `by` of `x + by` or `x - by`, with the range of the type of the result.
fn shifted(arg: &Expr) -> Option<(&Expr, i64, (i64, i64))> {
    let ExprKind::Func(func) = &arg.kind else { return None };
    let src = src(func.oid);
    int_arith(src)?;
    let plus = src.ends_with("pl");
    let ([a, b], true) = (func.args.as_slice(), plus || src.ends_with("mi")) else { return None };
    let (var, by, negate) = match (&a.kind, &b.kind) {
        (ExprKind::Var(_), ExprKind::Const(c)) => (a, c, !plus),
        (ExprKind::Const(c), ExprKind::Var(_)) if plus => (b, c, false),
        _ => return None,
    };
    int_range(var.ty)?;
    let by = int_of(by)?;
    let by = if negate { by.checked_neg()? } else { by };
    Some((var, by, int_range(arg.ty)?))
}

/// The states of an aggregate of each group.
enum States {
    Count(Vec<i64>),
    Sum(Vec<(i64, i128)>),
    Extreme(Vec<Option<i64>>),
    Distinct(Vec<FxSet<u64>>),
    Row(Vec<agg::State>),
    /// The states of a [`Method::Shift`] are the states of its base.
    Shift,
    /// The results of a call of [`Method::Row`] for each group.
    Done(Vec<Value>),
}

/// The codes that the scan keeps for `min` or `max` of a column of codes.
struct Codes {
    /// The codes that each group saw, for a type that does not compare the bytes.
    seen: Vec<FxSet<u32>>,
    /// The extreme code of each group, or `u32::MAX` for no code, for `text`.
    best: Vec<u32>,
    /// The dictionary of the codes, or `usize::MAX` before the first vector.
    dict: usize,
    /// `Some(true)` for `max` of `text` and `Some(false)` for `min` of `text`.
    max: Option<bool>,
}

struct Call<'q> {
    agg: &'q Aggref,
    key: *const Aggref,
    method: Method<'q>,
    states: States,
    /// For `min` and `max` of the row engine, the codes of a column of codes that each group saw. Each code goes into the state once, after the scan, as the result does not depend on the order or the number of the values.
    codes: Option<Codes>,
}

/// The states of the aggregate calls of all the groups after the scan, which give the results of the calls of a group.
struct Results<'q> {
    calls: Vec<Call<'q>>,
    groups: usize,
}

impl Results<'_> {
    /// The results of the calls of the group `g`, or of only the calls in `only`.
    fn group(
        &self,
        g: usize,
        only: Option<&[*const Aggref]>,
    ) -> Result<Vec<(*const Aggref, Value)>> {
        let mut out = Vec::with_capacity(only.map_or(self.calls.len(), <[_]>::len));
        for (c, call) in self.calls.iter().enumerate() {
            if only.is_none_or(|only| only.contains(&call.key)) {
                out.push((call.key, self.value(c, g)?));
            }
        }
        Ok(out)
    }

    /// The counts of the groups, for a call of `count`.
    fn counts(&self, c: usize) -> Option<&[i64]> {
        let call = &self.calls[c];
        match (&call.method, &call.states) {
            (Method::Count | Method::CountArg | Method::Distinct, States::Count(states)) => {
                Some(states)
            }
            _ => None,
        }
    }

    /// The result of the call `c` for the group `g`.
    fn value(&self, c: usize, g: usize) -> Result<Value> {
        let call = &self.calls[c];
        Ok(match (&call.method, &call.states) {
            (Method::Count | Method::CountArg, States::Count(states)) => Value::Int8(states[g]),
            (Method::Distinct, States::Count(states)) => Value::Int8(states[g]),
            (Method::SumInt, States::Sum(states)) => match states[g] {
                (0, _) => Value::Null,
                (_, sum) => Value::Int8(i64::try_from(sum).map_err(|_| {
                    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
                })?),
            },
            (Method::Int128 { avg }, States::Sum(states)) => match (states[g], avg) {
                ((0, _), _) => Value::Null,
                ((_, sum), false) => Value::Numeric(Numeric::from_integer(sum)),
                ((n, sum), true) => Value::Numeric(
                    Numeric::from_integer(sum)
                        .div(&Numeric::from_integer(i128::from(n)))
                        .map_err(rupg_func::type_error)?,
                ),
            },
            (Method::Min | Method::Max, States::Extreme(states)) => {
                states[g].map_or(Value::Null, |v| int_value(call.agg.args[0].ty, v))
            }
            (Method::Row(_), States::Done(done)) => done[g].clone(),
            _ => return Err(Error::internal("an aggregate has another state")),
        })
    }
}

/// The method of the scan for an aggregate call.
fn method<'q>(agg: &'q Aggref, expr: &'q Expr) -> Result<Option<Method<'q>>> {
    let name = if agg.oid < rupg_catalog::FIRST_NORMAL_OID {
        builtin::proc_by_oid(agg.oid).map_or("", |p| p.name)
    } else {
        ""
    };
    let arg = agg.args.first().filter(|_| agg.nargs == 1 && agg.args.len() == 1);
    let arg_ty = arg.map_or(0, |a| a.ty);
    let int = matches!(kind_of(arg_ty).map(Kind::store), Some(Store::Int)) && arg_ty != oid::BOOL;
    Ok(Some(match name {
        "count" if agg.star || agg.nargs == 0 => Method::Count,
        "count" if arg.is_some() && !agg.distinct.is_empty() => Method::Distinct,
        "count" if arg.is_some() => Method::CountArg,
        "sum" if matches!(arg_ty, oid::INT2 | oid::INT4) && expr.ty == oid::INT8 => Method::SumInt,
        "sum" if arg_ty == oid::INT8 && expr.ty == oid::NUMERIC => Method::Int128 { avg: false },
        "avg" if matches!(arg_ty, oid::INT2 | oid::INT4 | oid::INT8) && expr.ty == oid::NUMERIC => {
            Method::Int128 { avg: true }
        }
        "min" if int && expr.ty == arg_ty => Method::Min,
        "max" if int && expr.ty == arg_ty => Method::Max,
        _ => {
            if !agg.distinct.is_empty() {
                return Ok(None);
            }
            let Ok(method) = agg::method(agg, expr.ty, expr.location) else { return Ok(None) };
            if matches!(method, agg::Method::OrderedSet(_)) {
                return Ok(None);
            }
            Method::Row(agg::Agg {
                agg,
                key: std::ptr::from_ref(agg),
                ty: expr.ty,
                method,
                types: agg.args[..agg.nargs].iter().map(|a| a.ty).collect(),
            })
        }
    }))
}

/// The rows of a query of one table that the scan can run, or `None` for a query that the row engine runs.
///
/// # Errors
///
/// The error of an expression, as the row engine gives it.
pub(crate) fn run(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    want: Option<usize>,
) -> Result<Option<Vec<Vec<Value>>>> {
    let [relation] = query.relations.as_slice() else { return Ok(None) };
    if !matches!(query.from.as_slice(), [FromItem::Relation(0)])
        || relation.subquery.is_some()
        || relation.function.is_some()
        || relation.values.is_some()
        || relation.cte.is_some()
        || relation.tablesample.is_some()
        || relation.oid < rupg_catalog::FIRST_NORMAL_OID
        || query.set_op.is_some()
        || !query.windows.is_empty()
        || !query.ctes.is_empty()
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || !query.grouping_sets.is_empty()
        || query.with_ties
        || query.has_target_srfs()
    {
        return Ok(None);
    }
    // The row engine reads the rows of the keys of a unique index, with no plan.
    if scan::unique_key(query, params, session) {
        return Ok(None);
    }
    // An index scan gives its rows in the order of the index, which the columns do not keep.
    if session.join_order(query).is_some_and(|planned| planned.iter().any(|s| !s.keys.is_empty())) {
        return Ok(None);
    }
    let Some(catalog) = session.catalog() else { return Ok(None) };
    let Some(table) = catalog.relation(relation.oid) else { return Ok(None) };
    if table.kind != rupg_catalog::RelKind::Table
        || !catalog.all_inheritors(relation.oid).is_empty()
    {
        return Ok(None);
    }
    let exprs = query
        .targets
        .iter()
        .map(|t| &t.expr)
        .chain(&query.filter)
        .chain(&query.having)
        .chain(&query.offset)
        .chain(&query.limit);
    let mut used = vec![false; relation.columns.len()];
    for expr in exprs.clone() {
        if unsupported(expr) {
            return Ok(None);
        }
        expr.find(0, &mut |e, _| {
            if let ExprKind::Var(var) = e.kind
                && let Some(slot) =
                    usize::try_from(var.attnum - 1).ok().and_then(|i| used.get_mut(i))
            {
                *slot = true;
            }
            None::<()>
        });
    }
    for item in &query.group {
        let ty = query.targets[item.target].expr.ty;
        if !plain(item.collation) || ty == oid::BPCHAR {
            return Ok(None);
        }
    }
    let types: Vec<u32> = relation.columns.iter().map(|c| c.ty).collect();
    let mut wanted = Vec::new();
    let mut places = vec![usize::MAX; types.len()];
    for (column, _) in used.iter().enumerate().filter(|(_, u)| **u) {
        let Some(kind) = kind_of(types[column]) else { return Ok(None) };
        if kind == Kind::Text && !table.columns.get(column).is_some_and(|c| plain(c.collation)) {
            return Ok(None);
        }
        places[column] = wanted.len();
        wanted.push((column, kind));
    }
    let Some((rows, stored)) = session.table_columns(relation.oid, &wanted)? else {
        return Ok(None);
    };
    let mut dicts = Vec::new();
    let mut dict_of = vec![usize::MAX; stored.len()];
    for (place, column) in stored.iter().enumerate() {
        if column.kind.store() == Store::Code {
            dict_of[place] = dicts.len();
            dicts.push(Dict::Stored(Arc::clone(column)));
        }
    }
    let mut exec = Exec {
        params,
        session,
        cache,
        stored,
        places,
        types,
        dict_of,
        dicts,
        generic: FxMap::default(),
        likes: FxMap::default(),
        lengths: FxMap::default(),
        slots: params.to_vec(),
        empty: scan::Tables::default(),
    };
    let (offset, count) = {
        let mut eval = Eval::new(params, session, &exec.empty, cache, &[]);
        eval.limits(query)?
    };
    if count == Some(0) {
        return Ok(Some(Vec::new()));
    }
    // `is_degenerate_grouping`: with `HAVING` and no aggregate and no `GROUP BY`, the query does not read the table.
    let calls = agg::calls(query);
    let degenerate =
        query.grouped && query.having.is_some() && query.group.is_empty() && calls.is_empty();
    let open = !degenerate && {
        let mut eval = Eval::new(params, session, &exec.empty, cache, &[]);
        crate::gate(query, &mut eval)?
    };
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        scan::conjuncts(filter, &mut conds);
    }
    let conds: Vec<&Expr> = scan::order_quals(conds, &query.barrier)
        .into_iter()
        .filter(|c| c.first_var().is_some())
        .collect();
    // The columns that the targets and `HAVING` read, which the rows of the result keep.
    let mut kept = vec![false; relation.columns.len()];
    for expr in query.targets.iter().map(|t| &t.expr).chain(&query.having) {
        if query.grouped {
            outside_aggs(expr, &mut kept);
        } else {
            expr.find(0, &mut |e, _| {
                if let ExprKind::Var(var) = e.kind
                    && let Some(slot) =
                        usize::try_from(var.attnum - 1).ok().and_then(|i| kept.get_mut(i))
                {
                    *slot = true;
                }
                None::<()>
            });
        }
    }
    let kept_columns: Vec<usize> = (0..kept.len()).filter(|&c| kept[c]).collect();
    if open {
        session.count_scan(relation.oid);
    }
    let segments = rows.div_ceil(ROWS);
    let mut scan =
        Scan { exec: &mut exec, conds: &conds, segments, rows, open, answered: Vec::new() };
    let (rows, tables, ordered) = if query.grouped {
        let Some(out) = grouped(&mut scan, query, &calls, &kept_columns, offset, count)? else {
            return Ok(None);
        };
        out
    } else if let Some((columns, bound)) = top_columns(query, offset, count) {
        let rows = top(&mut scan, query, session, &columns, bound, offset, count)?;
        (rows, scan::kept_tables(query, Vec::new(), &kept), true)
    } else {
        let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
        let mut needed = match count {
            Some(count) if !ordered => offset.saturating_add(count),
            _ => usize::MAX,
        };
        if let Some(want) = want
            && !ordered
        {
            needed = needed.min(offset.saturating_add(want));
        }
        let mut found: Vec<Vec<Value>> = Vec::new();
        scan.each(&mut |exec, batch, sel| {
            let sel = &sel[..sel.len().min(needed - found.len())];
            let mut cols = Vec::with_capacity(kept_columns.len());
            for &column in &kept_columns {
                cols.push(exec.var(batch, column, exec.types[column], sel)?);
            }
            for i in 0..sel.len() {
                found.push(cols.iter().map(|c| c.value(i, &exec.dicts)).collect());
            }
            Ok(found.len() < needed)
        })?;
        let tables = scan::kept_tables(query, found, &kept);
        let (rows, _) = project(
            query,
            params,
            session,
            cache,
            &tables,
            needed,
            query.having.as_ref(),
            None,
            offset,
            count,
        )?;
        (rows, tables, false)
    };
    drop(tables);
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let rows =
        if ordered { rows } else { crate::order(query, rows, &types, offset, count, session)? };
    let width = query.targets.iter().take_while(|t| !t.junk).count();
    Ok(Some(
        rows.into_iter()
            .map(|mut row| {
                row.truncate(width);
                row
            })
            .collect(),
    ))
}

/// For a query with no groups whose targets are columns of the table, with `ORDER BY` and `LIMIT` and no `DISTINCT`: the column of each target, and the bound of the sort, which must allow `bound * 2` in an `int` as `tuplesort_set_bound` does.
fn top_columns(query: &Query, offset: usize, count: Option<usize>) -> Option<(Vec<usize>, usize)> {
    let count = count?;
    if query.sort.is_empty() || !query.distinct.is_empty() || query.with_ties {
        return None;
    }
    let bound = offset
        .checked_add(count)
        .filter(|b| *b > 0 && *b <= usize::try_from(i32::MAX / 2).unwrap_or(usize::MAX))?;
    let columns = query
        .targets
        .iter()
        .map(|t| match &t.expr.kind {
            ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => {
                usize::try_from(var.attnum - 1).ok()
            }
            _ => None,
        })
        .collect::<Option<Vec<usize>>>()?;
    Some((columns, bound))
}

/// The rows of a query of [`top_columns`] in the order of `ORDER BY`, with `OFFSET` and `LIMIT` done, as [`crate::order`] gives them for all the rows of the scan. The first `bound * 2` rows wait for a sort of all of them. After them, each row goes into the bounded heap as it comes, and a row reads the columns of the targets that `ORDER BY` does not have only when the heap keeps it.
fn top(
    scan: &mut Scan<'_, '_>,
    query: &Query,
    session: &dyn Session,
    columns: &[usize],
    bound: usize,
    offset: usize,
    count: Option<usize>,
) -> Result<Vec<Vec<Value>>> {
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let keys = sort::Keys::new(&query.sort, &types, session)?;
    let mut cmp = |a: &Vec<Value>, b: &Vec<Value>| keys.compare(a, b);
    let sorted: Vec<bool> =
        (0..columns.len()).map(|t| query.sort.iter().any(|s| s.target == t)).collect();
    let mut first: Vec<Vec<Value>> = Vec::new();
    let mut heap = sort::Bounded::new(bound);
    let mut streaming = false;
    let mut spare: Option<Vec<Value>> = None;
    // The first key of `ORDER BY` when it compares integers, and its integer in the top of the full heap. A row whose first key comes after it cannot go in the heap, so the scan reads no other value of the row.
    let lead = keys.lead();
    let lead_of = |heap: &sort::Bounded<Vec<Value>>| {
        let (target, ..) = lead?;
        sort::int(&heap.top()?[target])
    };
    let cut: Cell<Option<i64>> = Cell::new(None);
    // The scan skips the rows after the cut when the column of the lead key is a stored column of integers and its nulls come last.
    let scan_cut = lead.and_then(|(target, descending, nulls_first)| {
        let place = *scan.exec.places.get(columns[target])?;
        (!nulls_first && scan.exec.stored.get(place).is_some_and(|s| s.kind.store() == Store::Int))
            .then_some((place, descending, &cut))
    });
    scan.each_cut(scan_cut, &mut |exec, batch, sel| {
        let cols = columns
            .iter()
            .map(|&c| exec.var(batch, c, exec.types[c], sel))
            .collect::<Result<Vec<_>>>()?;
        let lead_ints = lead.and_then(|(target, ..)| cols[target].ints());
        // The codes of the lead key when it is a column of text codes: a row whose text comes after the text of the top of the full heap cannot go in the heap, so the scan makes no value of the row.
        let lead_codes = lead.and_then(|(target, ..)| match &cols[target].data {
            Data::Code(v, d) if exec.dicts[*d].is_text() => {
                Some((v, &exec.dicts[*d], cols[target].nulls.as_deref()))
            }
            _ => None,
        });
        for i in 0..sel.len() {
            if !streaming {
                first.push(cols.iter().map(|c| c.value(i, &exec.dicts)).collect());
                if first.len() > bound * 2 {
                    for row in first.drain(..) {
                        heap.push(row, &mut cmp)?;
                    }
                    streaming = true;
                    cut.set(lead_of(&heap));
                }
                continue;
            }
            if let (Some((_, descending, nulls_first)), Some(ints), Some(top)) =
                (lead, lead_ints, cut.get())
            {
                let after = match ints.get(i) {
                    Some(v) if descending => v < top,
                    Some(v) => v > top,
                    None => !nulls_first,
                };
                if after {
                    continue;
                }
            }
            if let (Some((target, descending, nulls_first)), Some((codes, dict, nulls))) =
                (lead, lead_codes)
                && let Some(Value::Text(top)) = heap.top().map(|row| &row[target])
            {
                let after = if nulls.is_some_and(|n| n[i]) {
                    !nulls_first
                } else {
                    let text = dict.text(codes[i]).unwrap_or_default();
                    if descending { text < top.as_bytes() } else { text > top.as_bytes() }
                };
                if after {
                    continue;
                }
            }
            let mut row = spare.take().unwrap_or_default();
            row.clear();
            row.extend(
                cols.iter().zip(&sorted).map(
                    |(c, &s)| {
                        if s { c.value(i, &exec.dicts) } else { Value::Null }
                    },
                ),
            );
            if heap.keeps(&row, &mut cmp)? {
                for (slot, (c, &s)) in row.iter_mut().zip(cols.iter().zip(&sorted)) {
                    if !s {
                        *slot = c.value(i, &exec.dicts);
                    }
                }
                spare = heap.push(row, &mut cmp)?;
                cut.set(lead_of(&heap));
            } else {
                spare = Some(row);
            }
        }
        Ok(true)
    })?;
    let rows = if streaming {
        heap.finish(&mut cmp)?
    } else {
        sort::qsort(&mut first, &mut cmp)?;
        first
    };
    Ok(rows.into_iter().skip(offset).take(count.unwrap_or(usize::MAX)).collect())
}

/// The rows of the targets for each row of `tables`, or for each group with the results of its aggregates, which pass `having`: the `HAVING` of the query, or `None` when the groups passed it before. It gives true when the rows are in the order of `ORDER BY` with `OFFSET` and `LIMIT` done: with a `LIMIT` and many groups, the targets of `ORDER BY` run for each group, and the other targets only for the groups that `LIMIT` keeps, when they cannot fail.
#[allow(clippy::too_many_arguments)]
fn project(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    tables: &scan::Tables,
    needed: usize,
    having: Option<&Expr>,
    groups: Option<Groups<'_>>,
    offset: usize,
    count: Option<usize>,
) -> Result<(Vec<Vec<Value>>, bool)> {
    let levels = crate::set_levels(&query.targets);
    let mut eval = Eval::new(params, session, tables, cache, &[]);
    let mut out = Vec::new();
    match groups {
        Some((order, results, firsts, top)) => {
            let tuples = firsts.tuples(results.groups);
            let tuple = |g: usize| &tuples[firsts.place(g)];
            if let Some(top) = top {
                for g in top {
                    eval.tuple = tuple(g);
                    eval.group = Group { aggs: results.group(g, None)?, ..Group::default() };
                    eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
                }
                return Ok((out, true));
            }
            if let Some(count) = count
                && levels.is_empty()
                && query.distinct.is_empty()
                && !query.sort.is_empty()
                && order.len() > offset.saturating_add(count).saturating_mul(4)
                && top_safe(query)
            {
                let sorted: Vec<bool> = (0..query.targets.len())
                    .map(|t| query.sort.iter().any(|s| s.target == t))
                    .collect();
                // The calls that the targets of `ORDER BY` and `HAVING` read.
                let mut read: Vec<*const Aggref> = Vec::new();
                let exprs = query.targets.iter().zip(&sorted).filter(|(_, s)| **s);
                for expr in exprs.map(|(t, _)| &t.expr).chain(having) {
                    expr.find(0, &mut |e, _| {
                        if let ExprKind::Agg(agg) = &e.kind {
                            read.push(std::ptr::from_ref(&**agg));
                        }
                        None::<()>
                    });
                }
                // With no `HAVING` and only calls in `ORDER BY`, the rows of the sort take the results of the calls with no evaluation.
                let direct: Option<Vec<Option<usize>>> = having
                    .is_none()
                    .then(|| {
                        query
                            .targets
                            .iter()
                            .zip(&sorted)
                            .map(|(target, &sorted)| match &target.expr.kind {
                                _ if !sorted => Some(None),
                                ExprKind::Agg(agg) => results
                                    .calls
                                    .iter()
                                    .position(|c| std::ptr::eq(c.key, &**agg))
                                    .map(Some),
                                _ => None,
                            })
                            .collect()
                    })
                    .flatten();
                let mut types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
                types.push(oid::INT8);
                // `tuplesort_set_bound`: the bound must allow `bound * 2` in an `int`.
                let bound = offset.checked_add(count).filter(|b| {
                    *b > 0 && *b <= usize::try_from(i32::MAX / 2).unwrap_or(usize::MAX)
                });
                if let (Some(direct), Some(bound)) = (&direct, bound) {
                    // Each group goes into the bounded heap of the sort as it comes, and the row of a group that is not in the heap takes the values of the next group.
                    let keys = sort::Keys::new(&query.sort, &types, session)?;
                    let mut cmp = |a: &Vec<Value>, b: &Vec<Value>| keys.compare(a, b);
                    let mut heap = sort::Bounded::new(bound);
                    let mut spare: Option<Vec<Value>> = None;
                    for (n, g) in order.into_iter().enumerate() {
                        crate::interrupt(session, n)?;
                        let mut row = spare.take().unwrap_or_default();
                        row.clear();
                        for c in direct {
                            row.push(match c {
                                Some(c) => results.value(*c, g)?,
                                None => Value::Null,
                            });
                        }
                        row.push(Value::Int8(i64::try_from(g).unwrap_or(i64::MAX)));
                        spare = heap.push(row, &mut cmp)?;
                    }
                    for row in heap.finish(&mut cmp)?.into_iter().skip(offset) {
                        let Some(&Value::Int8(g)) = row.last() else { continue };
                        let g = usize::try_from(g).unwrap_or(0);
                        eval.tuple = tuple(g);
                        eval.group = Group { aggs: results.group(g, None)?, ..Group::default() };
                        eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
                    }
                    return Ok((out, true));
                }
                let mut partial = Vec::new();
                for (n, g) in order.into_iter().enumerate() {
                    crate::interrupt(session, n)?;
                    if let Some(direct) = &direct {
                        let mut row = Vec::with_capacity(direct.len() + 1);
                        for c in direct {
                            row.push(match c {
                                Some(c) => results.value(*c, g)?,
                                None => Value::Null,
                            });
                        }
                        row.push(Value::Int8(i64::try_from(g).unwrap_or(i64::MAX)));
                        partial.push(row);
                        continue;
                    }
                    eval.tuple = tuple(g);
                    eval.group = Group { aggs: results.group(g, Some(&read))?, ..Group::default() };
                    if let Some(having) = having
                        && eval.eval(having)? != Value::Bool(true)
                    {
                        continue;
                    }
                    let mut row = Vec::with_capacity(query.targets.len() + 1);
                    for (target, &sorted) in query.targets.iter().zip(&sorted) {
                        row.push(if sorted { eval.eval(&target.expr)? } else { Value::Null });
                    }
                    row.push(Value::Int8(i64::try_from(g).unwrap_or(i64::MAX)));
                    partial.push(row);
                }
                let partial = crate::order(query, partial, &types, offset, Some(count), session)?;
                for row in partial {
                    let Some(&Value::Int8(g)) = row.last() else { continue };
                    let g = usize::try_from(g).unwrap_or(0);
                    eval.tuple = tuple(g);
                    eval.group = Group { aggs: results.group(g, None)?, ..Group::default() };
                    eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
                }
                return Ok((out, true));
            }
            for (n, g) in order.into_iter().enumerate() {
                if out.len() >= needed {
                    break;
                }
                crate::interrupt(session, n)?;
                eval.tuple = tuple(g);
                eval.group = Group { aggs: results.group(g, None)?, ..Group::default() };
                if let Some(having) = having
                    && eval.eval(having)? != Value::Bool(true)
                {
                    continue;
                }
                eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
            }
        }
        None => {
            let rows = tables.rows.first().map_or(0, |r| r.len());
            let tuples: Vec<[usize; 1]> = (0..rows).map(|r| [r]).collect();
            let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
            for (n, tuple) in tuples.iter().enumerate() {
                if out.len() >= needed {
                    break;
                }
                crate::interrupt(session, n)?;
                eval.tuple = tuple;
                if let Err(error) = eval.project(&query.targets, &levels, &mut out, needed) {
                    // With no sort, each row went to the client before the target of the next row failed.
                    if !ordered && cache.top.get() == Some(&raw const *query) {
                        let width = query.targets.iter().take_while(|t| !t.junk).count();
                        let sent = out.into_iter().skip(offset).take(count.unwrap_or(usize::MAX));
                        *cache.sent.borrow_mut() = sent
                            .map(|mut row| {
                                row.truncate(width);
                                row
                            })
                            .collect();
                    }
                    return Err(error);
                }
            }
        }
    }
    Ok((out, false))
}

/// True when a target cannot fail for a group: a column, a constant, a parameter, an aggregate, or a key of `GROUP BY`, which ran for each group.
fn safe(expr: &Expr, query: &Query) -> bool {
    match &expr.kind {
        ExprKind::Var(_) | ExprKind::Const(_) | ExprKind::Param(_) | ExprKind::Agg(_) => true,
        ExprKind::Relabel(arg, _) => safe(arg, query),
        _ => query.group.iter().any(|g| query.targets[g.target].expr == *expr),
    }
}

/// True when each target that `ORDER BY` does not have cannot fail for a group.
fn top_safe(query: &Query) -> bool {
    query
        .targets
        .iter()
        .enumerate()
        .all(|(t, target)| query.sort.iter().any(|s| s.target == t) || safe(&target.expr, query))
}

/// True when `HAVING` can run before the first rows of the groups: it reads only the results of the aggregates, constants and parameters, no target can fail, and each group gives one row. Then the groups that pass come first, and the scan reads the first rows of only those groups.
fn having_first(query: &Query) -> bool {
    fn of_aggs(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Agg(_) | ExprKind::Const(_) | ExprKind::Param(_) => true,
            ExprKind::Func(_)
            | ExprKind::Relabel(..)
            | ExprKind::Collate(..)
            | ExprKind::CoerceViaIo(..)
            | ExprKind::Bool(..)
            | ExprKind::NullTest(..)
            | ExprKind::BooleanTest(..)
            | ExprKind::Coalesce(_)
            | ExprKind::MinMax { .. }
            | ExprKind::NullIf { .. }
            | ExprKind::Distinct { .. } => {
                expr.find_child(&mut |child| (!of_aggs(child)).then_some(())).is_none()
            }
            _ => false,
        }
    }
    query.having.as_ref().is_some_and(of_aggs)
        && !query.group.is_empty()
        && crate::set_levels(&query.targets).is_empty()
        && query.targets.iter().all(|target| safe(&target.expr, query))
}

/// The scan of the vectors of the table with the conditions of `WHERE`.
struct Scan<'e, 'q> {
    exec: &'e mut Exec<'q>,
    conds: &'e [&'e Expr],
    segments: usize,
    rows: usize,
    open: bool,
    /// True for each segment whose summaries gave the aggregates, which the scan does not read.
    answered: Vec<bool>,
}

/// The rows of the groups, the tables of the first row of each group, and true when the rows are in the order of `ORDER BY` with `OFFSET` and `LIMIT` done.
type Grouped = (Vec<Vec<Value>>, scan::Tables, bool);

/// The order of the groups, the values of the aggregates of each group, the groups with a first row, and the groups of the rows that [`count_top`] gives.
type Groups<'q> = (Vec<usize>, Results<'q>, Firsts, Option<Vec<usize>>);

/// The groups whose first rows the table of [`project`] has.
enum Firsts {
    /// The first groups, each at its place: the row of the group `g` is the row `g`.
    All(usize),
    /// The groups of a sorted list: the row of the group `wanted[i]` is the row `i`. A query with `LIMIT` reads the first rows of few of many groups.
    Of(Vec<usize>),
}

impl Firsts {
    /// The tuples of the groups for [`Firsts::place`], with a last tuple of no row for the groups with no first row.
    fn tuples(&self, groups: usize) -> Vec<[usize; 1]> {
        let rows = match self {
            Firsts::All(rows) => (*rows).min(groups),
            Firsts::Of(wanted) => wanted.len(),
        };
        (0..rows).map(|row| [row]).chain([[scan::NONE]]).collect()
    }

    /// The place of the tuple of the group `g` in [`Firsts::tuples`].
    fn place(&self, g: usize) -> usize {
        match self {
            Firsts::All(rows) => g.min(*rows),
            Firsts::Of(wanted) => wanted.binary_search(&g).unwrap_or(wanted.len()),
        }
    }
}

/// For `ORDER BY` of one count with `LIMIT`, as the sort of [`project`] gives it: the call of the count, true for a descending order, and the bound of the sort. The query must have no `HAVING`, no `DISTINCT` and no sets in the targets.
fn top_key(
    query: &Query,
    session: &dyn Session,
    offset: usize,
    count: Option<usize>,
) -> Option<(*const Aggref, bool, usize)> {
    let count = count?;
    if !crate::set_levels(&query.targets).is_empty()
        || !query.distinct.is_empty()
        || query.having.is_some()
        || !top_safe(query)
    {
        return None;
    }
    let [key] = query.sort.as_slice() else { return None };
    let ExprKind::Agg(agg) = &query.targets[key.target].expr.kind else { return None };
    // `tuplesort_set_bound`: the bound must allow `bound * 2` in an `int`.
    let bound = offset
        .checked_add(count)
        .filter(|b| *b > 0 && *b <= usize::try_from(i32::MAX / 2).unwrap_or(usize::MAX))?;
    let mut types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    types.push(oid::INT8);
    let descending = sort::Keys::new(&query.sort, &types, session).ok()?.one_native()?;
    Some((std::ptr::from_ref(&**agg), descending, bound))
}

/// The number of groups at the start of the order of the groups that make the rows of a query with `LIMIT` and no sort, which [`project`] reads before it stops: `OFFSET` and `LIMIT` together. `None` when a group can make no row or many rows, with `HAVING` or a set-returning function, and with `DISTINCT` or `ORDER BY`, which read each group.
fn first_groups(query: &Query, offset: usize, count: Option<usize>) -> Option<usize> {
    (query.sort.is_empty()
        && query.distinct.is_empty()
        && query.having.is_none()
        && crate::set_levels(&query.targets).is_empty())
    .then(|| offset.checked_add(count?))
    .flatten()
}

/// The counts of the groups for the call `agg` of [`top_key`], or `None` for a call that is not a count.
fn top_counts<'r>(results: &'r Results<'_>, agg: *const Aggref) -> Option<&'r [i64]> {
    results.calls.iter().position(|c| std::ptr::eq(c.key, agg)).and_then(|c| results.counts(c))
}

/// The groups of the rows of `ORDER BY` of one count with `LIMIT`, after `OFFSET`, from [`top_key`]. A bounded heap compares the counts as integers, with the same steps as the sort of the rows. `None` for few groups or for a call that is not a count.
fn count_top(
    session: &dyn Session,
    results: &Results<'_>,
    order: &[usize],
    (agg, descending, bound): (*const Aggref, bool, usize),
    offset: usize,
) -> Result<Option<Vec<usize>>> {
    if order.len() <= bound.saturating_mul(4) {
        return Ok(None);
    }
    let Some(counts) = top_counts(results, agg) else {
        return Ok(None);
    };
    let mut cmp = |a: &(i64, usize), b: &(i64, usize)| {
        Ok(if descending { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) })
    };
    let mut heap = sort::Bounded::new(bound);
    for (n, &g) in order.iter().enumerate() {
        crate::interrupt(session, n)?;
        heap.push((counts[g], g), &mut cmp)?;
    }
    Ok(Some(heap.finish(&mut cmp)?.into_iter().skip(offset).map(|(_, g)| g).collect()))
}

/// The groups of [`count_top`] from the counts alone, with no order of the groups. When the first `bound` counts in the order of the sort and the count after them are all different, the bounded heap of the sort keeps the same groups and gives them in the order of their counts for each order in which the groups come. `None` when two of these counts are equal, or for few groups, as for [`count_top`].
fn distinct_top(
    session: &dyn Session,
    results: &Results<'_>,
    (agg, descending, bound): (*const Aggref, bool, usize),
    offset: usize,
) -> Result<Option<Vec<usize>>> {
    if results.groups <= bound.saturating_mul(4) {
        return Ok(None);
    }
    let Some(counts) = top_counts(results, agg) else {
        return Ok(None);
    };
    // The first `bound + 1` counts in the order of the sort, with the last of them at the top of the heap. A larger rank is first in the sort.
    let rank = |c: i64| if descending { i128::from(c) } else { -i128::from(c) };
    let mut best: std::collections::BinaryHeap<std::cmp::Reverse<(i128, usize)>> =
        std::collections::BinaryHeap::with_capacity(bound + 2);
    for (g, &c) in counts.iter().enumerate() {
        crate::interrupt(session, g)?;
        let r = rank(c);
        if best.len() <= bound {
            best.push(std::cmp::Reverse((r, g)));
        } else if best.peek().is_some_and(|top| r > top.0.0) {
            best.pop();
            best.push(std::cmp::Reverse((r, g)));
        }
    }
    let mut best: Vec<(i128, usize)> = best.into_iter().map(|r| r.0).collect();
    best.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    if best.windows(2).any(|w| w[0].0 == w[1].0) {
        return Ok(None);
    }
    Ok(Some(best.into_iter().take(bound).skip(offset).map(|(_, g)| g).collect()))
}

/// True when a value passes the test of the zones: a comparison or `IN` is false for a null value, as in SQL.
#[inline]
fn passes(test: &Test, null: bool, value: i64) -> bool {
    let key = rupg_summary::key::int(value);
    match test {
        Test::IsNull => null,
        Test::IsNotNull => !null,
        _ if null => false,
        Test::Eq(c) => key == *c,
        Test::Ne(c) => key != *c,
        Test::Lt(c) => key < *c,
        Test::Le(c) => key <= *c,
        Test::Gt(c) => key > *c,
        Test::Ge(c) => key >= *c,
        Test::In(cs) => cs.contains(&key),
    }
}

/// A call with the rows of a vector that pass the conditions, which gives false to stop the scan.
type Each<'a> = dyn FnMut(&mut Exec<'_>, &mut Batch, &[u32]) -> Result<bool> + 'a;

impl Scan<'_, '_> {
    /// Calls `each` with the rows of each vector that pass the conditions, until it gives false.
    fn each(&mut self, each: &mut Each<'_>) -> Result<()> {
        self.each_cut(None, each)
    }

    /// [`Scan::each`] with the cut `cut` of a top of rows: the place of an integer column, true for a descending order, and the last integer of the column that can go in the top, which `each` changes. A vector whose zone shows no row at or before the integer is not read, and the other rows of a vector, and the rows whose value is null, are not given to `each`.
    fn each_cut(
        &mut self,
        cut: Option<(usize, bool, &Cell<Option<i64>>)>,
        each: &mut Each<'_>,
    ) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        let mut sel: Vec<u32> = Vec::with_capacity(VECTOR);
        let mut next: Vec<u32> = Vec::with_capacity(VECTOR);
        let tests: Vec<Option<(usize, Test)>> =
            self.conds.iter().map(|cond| self.exec.zone_test(cond)).collect();
        let mut verdicts = vec![Verdict::Some; tests.len()];
        let mut whole = vec![Verdict::Some; tests.len()];
        for segment in 0..self.segments {
            if self.answered.get(segment).copied().unwrap_or(false) {
                continue;
            }
            let segment_rows = self.rows.saturating_sub(segment * ROWS).min(ROWS);
            let vectors = stored::vectors(segment_rows);
            // spec/12 section 12.6.1: a condition on a column whose zones show that no row of the segment passes skips the segment.
            for (verdict, test) in whole.iter_mut().zip(&tests) {
                *verdict = test.as_ref().map_or(Verdict::Some, |(place, test)| {
                    self.exec.zones(*place, segment, vectors).map_or(Verdict::Some, |zones| {
                        let mut zone = Zone::EMPTY;
                        for vector in zones {
                            zone.merge(vector);
                        }
                        zone.test(test)
                    })
                });
            }
            if whole.contains(&Verdict::None) {
                continue;
            }
            for vector in 0..vectors {
                self.exec.session.interrupted()?;
                // A vector that no row of passes is not read, and a condition that every row of the vector passes is not evaluated.
                for ((verdict, &all), test) in verdicts.iter_mut().zip(&whole).zip(&tests) {
                    *verdict = match test {
                        _ if all != Verdict::Some => all,
                        Some((place, test)) => self
                            .exec
                            .zones(*place, segment, vectors)
                            .map_or(Verdict::Some, |zones| zones[vector].test(test)),
                        None => Verdict::Some,
                    };
                }
                if verdicts.contains(&Verdict::None) {
                    continue;
                }
                let cut_test = cut.and_then(|(place, descending, last)| {
                    let key = rupg_summary::key::int(last.get()?);
                    let test = if descending { Test::Ge(key) } else { Test::Le(key) };
                    let verdict = self
                        .exec
                        .zones(place, segment, vectors)
                        .map_or(Verdict::Some, |zones| zones[vector].test(&test));
                    Some((place, test, verdict))
                });
                if matches!(cut_test, Some((_, _, Verdict::None))) {
                    continue;
                }
                let rows = (segment_rows - vector * VECTOR).min(VECTOR);
                let mut batch = Batch {
                    segment,
                    vector,
                    rows,
                    decoded: (0..self.exec.stored.len()).map(|_| None).collect(),
                };
                sel.clear();
                sel.extend(0..batch.rows as u32);
                if let Some((place, test, Verdict::Some)) = &cut_test {
                    self.exec.decode(&mut batch, *place)?;
                    if let Some(Decoded { data: DecodedData::Int(values), nulls }) =
                        &batch.decoded[*place]
                    {
                        let null = |i: u32| nulls.as_ref().is_some_and(|n| n[i as usize]);
                        sel.retain(|&i| passes(test, null(i), values[i as usize]));
                    }
                }
                for ((cond, verdict), test) in self.conds.iter().zip(&verdicts).zip(&tests) {
                    if sel.is_empty() {
                        break;
                    }
                    if *verdict == Verdict::All {
                        continue;
                    }
                    // A condition that the zones test runs on the decoded integers of its column, with no copy of the values of the rows.
                    if let Some((place, test)) = test {
                        self.exec.decode(&mut batch, *place)?;
                        if let Some(Decoded { data: DecodedData::Int(values), nulls }) =
                            &batch.decoded[*place]
                        {
                            next.clear();
                            let null = |i: u32| nulls.as_ref().is_some_and(|n| n[i as usize]);
                            next.extend(
                                sel.iter()
                                    .copied()
                                    .filter(|&i| passes(test, null(i), values[i as usize])),
                            );
                            std::mem::swap(&mut sel, &mut next);
                            continue;
                        }
                    }
                    let col = self.exec.eval(cond, &mut batch, &sel)?;
                    next.clear();
                    next.extend(
                        sel.iter()
                            .enumerate()
                            .filter(|(k, _)| col.truth(*k) == Some(true))
                            .map(|(_, &i)| i),
                    );
                    std::mem::swap(&mut sel, &mut next);
                }
                if !sel.is_empty() && !each(self.exec, &mut batch, &sel)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

impl Exec<'_> {
    /// The zones of the vectors of the segment `segment` of the column at the place `place`, when the summary has one zone for each of the `vectors` vectors.
    fn zones(&self, place: usize, segment: usize, vectors: usize) -> Option<&[Zone]> {
        let zones = &self.stored.get(place)?.segments.get(segment)?.summary.vectors;
        (zones.len() == vectors).then_some(zones.as_slice())
    }

    /// The place of the column of `cond` and the test of its zones, for a comparison of an integer column with a constant that is not null, or a null test of a column (spec/12 section 12.6.1). `None` is a condition that the zones do not test.
    fn zone_test(&self, cond: &Expr) -> Option<(usize, Test)> {
        let place = |e: &Expr| match &e.kind {
            ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => {
                let place = *self.places.get(usize::try_from(var.attnum - 1).ok()?)?;
                (place < self.stored.len()).then_some(place)
            }
            _ => None,
        };
        let constant = |e: &Expr| match &e.kind {
            ExprKind::Const(value) => int_of(value),
            ExprKind::Param(n) => self.params.get(n.wrapping_sub(1)).and_then(int_of),
            _ => None,
        };
        match &cond.kind {
            ExprKind::NullTest(arg, is_null) => {
                let place = place(arg)?;
                Some((place, if *is_null { Test::IsNull } else { Test::IsNotNull }))
            }
            ExprKind::Func(func) if !func.retset => {
                let src = src(func.oid);
                int_compare(src)?;
                let [a, b] = func.args.as_slice() else { return None };
                let op = &src[src.len() - 2..];
                let (place, value, op) = match (place(a), constant(b), place(b), constant(a)) {
                    (Some(p), Some(v), _, _) => (p, v, op),
                    (_, _, Some(p), Some(v)) => (
                        p,
                        v,
                        match op {
                            "lt" => "gt",
                            "le" => "ge",
                            "gt" => "lt",
                            "ge" => "le",
                            op => op,
                        },
                    ),
                    _ => return None,
                };
                if self.stored[place].kind.store() != Store::Int {
                    return None;
                }
                let key = rupg_summary::key::int(value);
                Some((
                    place,
                    match op {
                        "eq" => Test::Eq(key),
                        "ne" => Test::Ne(key),
                        "lt" => Test::Lt(key),
                        "le" => Test::Le(key),
                        "gt" => Test::Gt(key),
                        "ge" => Test::Ge(key),
                        _ => return None,
                    },
                ))
            }
            // `x IN (c, ...)`, which `eval_const_expressions` makes `x = ANY (array)`. A null element is equal to no value, so it is not in the test.
            ExprKind::ScalarArrayOp { func, any: true, args } => {
                let src = src(*func);
                if !src.ends_with("eq") {
                    return None;
                }
                int_compare(src)?;
                let [a, b] = args.as_slice() else { return None };
                let place = place(a)?;
                if self.stored[place].kind.store() != Store::Int {
                    return None;
                }
                let values: Vec<i64> = match &b.kind {
                    ExprKind::Const(Value::Array(array)) => {
                        array.values.iter().flatten().map(int_of).collect::<Option<_>>()?
                    }
                    ExprKind::Array { multidims: false, elements, .. } => elements
                        .iter()
                        .filter(|e| !matches!(e.kind, ExprKind::Const(Value::Null)))
                        .map(constant)
                        .collect::<Option<_>>()?,
                    _ => return None,
                };
                let mut keys: Vec<u64> = values.into_iter().map(rupg_summary::key::int).collect();
                keys.sort_unstable();
                keys.dedup();
                Some((place, Test::In(keys)))
            }
            _ => None,
        }
    }

    /// The value of the column `column` at the row `row` of the batch.
    fn value_at(&mut self, batch: &mut Batch, column: usize, row: u32) -> Result<Value> {
        let col = self.var(batch, column, self.types[column], &[row])?;
        Ok(col.value(0, &self.dicts))
    }
}

/// The rows of a query with groups, and the rows of the first row of each group that the targets read. `None` when a key of `GROUP BY` has no word.
fn grouped(
    scan: &mut Scan<'_, '_>,
    query: &Query,
    calls: &[(&Aggref, &Expr)],
    kept_columns: &[usize],
    offset: usize,
    count: Option<usize>,
) -> Result<Option<Grouped>> {
    let mut aggs: Vec<Call<'_>> = Vec::with_capacity(calls.len());
    let mut bases: Vec<Base<'_>> = Vec::new();
    for &(agg, expr) in calls {
        let Some(mut method) = method(agg, expr)? else { return Ok(None) };
        let wide = match method {
            Method::SumInt => Some(None),
            Method::Int128 { avg } => Some(Some(avg)),
            _ => None,
        };
        if let (Some(wide), Some((var, by, range))) = (wide, agg.args.first().and_then(shifted)) {
            let base =
                bases.iter().position(|b: &Base<'_>| b.var.kind == var.kind).unwrap_or_else(|| {
                    bases.push(Base { var, states: Vec::new(), extremes: None });
                    bases.len() - 1
                });
            method = Method::Shift { base, by, range, wide };
        }
        let states = match &method {
            Method::Count | Method::CountArg => States::Count(Vec::new()),
            Method::Distinct => States::Distinct(Vec::new()),
            Method::SumInt | Method::Int128 { .. } => States::Sum(Vec::new()),
            Method::Min | Method::Max => States::Extreme(Vec::new()),
            Method::Row(_) => States::Row(Vec::new()),
            Method::Shift { .. } => States::Shift,
        };
        let codes = matches!(method, Method::Row(_))
            .then(|| builtin::proc_by_oid(agg.oid))
            .flatten()
            .is_some_and(|p| matches!(p.name, "min" | "max"))
            && agg.oid < rupg_catalog::FIRST_NORMAL_OID
            && agg.nargs == 1
            && agg.args.len() == 1
            && agg.order.is_empty();
        // `min` and `max` of `text` compare the bytes, so the scan keeps only the extreme code of each group.
        let max = builtin::proc_by_oid(agg.oid)
            .filter(|p| p.argtypes == [oid::TEXT])
            .map(|p| p.name == "max");
        let codes =
            codes.then(|| Codes { seen: Vec::new(), best: Vec::new(), dict: usize::MAX, max });
        aggs.push(Call { agg, key: std::ptr::from_ref(agg), method, states, codes });
    }
    let keys: Vec<&Expr> = query.group.iter().map(|g| &query.targets[g.target].expr).collect();
    // The map starts with room for the groups of a few batches at most. The estimate of the plan can be many times the real number of groups, and a map that is larger than the cache makes each row a miss. After each batch the map grows to the rate of new groups that the scan sees.
    let expected = session_groups(scan.exec.session, query).min(scan.rows).min(START_GROUPS);
    let mut map = KeyMap::new(keys.len() + 1, if keys.is_empty() { 0 } else { expected });
    // The key of the last row and its group. Rows in the order of a key come in runs of one key, and a run needs one probe of the map.
    let mut last_word = vec![0_u64; keys.len() + 1];
    let mut last_id: Option<u32> = None;
    let mut new_rows: Vec<u32> = Vec::new();
    let mut firsts: Vec<Vec<Value>> = Vec::new();
    // For `ORDER BY` of a count with `LIMIT`, the scan keeps the place of the first row of each group, and reads the values only for the groups of the result.
    let top = if keys.is_empty() { None } else { top_key(query, scan.exec.session, offset, count) };
    // With `LIMIT` and no sort, the result has the first groups of the order, so the scan also keeps the place of the first row of each group, and reads the values only for those groups.
    let first_groups =
        if keys.is_empty() || top.is_some() { None } else { first_groups(query, offset, count) };
    // With `HAVING` of only aggregates, the scan also keeps the place of the first row of each group, and reads the values only for the groups that pass.
    let having_first =
        !keys.is_empty() && top.is_none() && first_groups.is_none() && having_first(query);
    let lazy = top.is_some() || first_groups.is_some() || having_first;
    let mut spots: Vec<[u32; 3]> = Vec::new();
    // The places and the types of the keys of the hash table of the plan. The scan finds the hash of each group from its first row, and keeps no values of the keys.
    let mut hashing = plan_keys(scan.exec.session, query);
    let mut hashes: Vec<u32> = Vec::new();
    let mut hash_error: Option<Error> = None;
    // The keys of each group, from its first row, when the scan finds no hashes: the words of the keys and the mask of nulls while each key has one shape, else the values.
    let mut key_values: Vec<Value> = Vec::new();
    let mut key_words = KeyWords { words: Vec::new(), shapes: None, by_words: keys.len() < 64 };
    let rows = scan.rows;
    let mut seen = 0;
    let mut bail = false;
    let mut words: Vec<u64> = Vec::with_capacity(VECTOR * (keys.len() + 1));
    let mut row_hashes: Vec<u64> = Vec::with_capacity(VECTOR);
    let mut ids: Vec<u32> = Vec::with_capacity(VECTOR);
    let params = scan.exec.params;
    let session = scan.exec.session;
    let cache = scan.exec.cache;
    let empty = scan::Tables::default();
    if keys.is_empty() {
        firsts.reserve(1);
        for call in &mut aggs {
            grow(call, 1, 0, params, session, cache, &empty)?;
        }
        for base in &mut bases {
            base.states.resize(1, (0, 0));
        }
    }
    let mut groups = usize::from(keys.is_empty());
    let mut first_rows = 0;
    // True when a row of a group came after the first row of the last group.
    let mut trailing = false;
    let mut args: Vec<Value> = Vec::new();
    if keys.is_empty() && kept_columns.is_empty() && scan.conds.is_empty() && scan.open {
        scan.answered = answer(scan, &mut aggs, &mut bases);
        if scan.answered.contains(&true) {
            firsts.push(Vec::new());
            first_rows = 1;
        }
    }
    let parallel = match session.tasks() {
        Some(tasks) if scan.open => {
            let estimate = session_groups(session, query);
            (estimate.saturating_mul(groups::GROUP_ROWS) <= rows)
                .then(|| {
                    parallel_plan(
                        scan.exec,
                        &keys,
                        &aggs,
                        &bases,
                        scan.conds,
                        rows,
                        tasks.parallelism(),
                    )
                })
                .flatten()
                .map(|plan| (tasks, plan))
        }
        _ => None,
    };
    let mut found = None;
    if let Some((tasks, (plan, derived))) = parallel {
        session.interrupted()?;
        let mut run = groups::run(tasks, &scan.exec.stored, &plan, rows, scan.segments)?;
        session.interrupted()?;
        // With no derived keys, the words of the plan are the words of the keys, with no copy.
        if derived.iter().all(Option::is_none) {
            found = Some(run);
        } else if let Some(words) = derive_words(&run.words, plan.keys.len() + 1, &derived) {
            run.words = words;
            found = Some(run);
        }
    }
    // Calls of `count(DISTINCT)` of one column with no keys: the tasks count the values of the column.
    if found.is_none()
        && keys.is_empty()
        && kept_columns.is_empty()
        && bases.is_empty()
        && !scan.answered.contains(&true)
        && aggs.iter().all(|call| matches!(call.method, Method::Distinct))
        && let Some(tasks) = session.tasks().filter(|_| scan.open)
        && let Some(arg) = distinct_arg(&aggs)
        && let Some((plan, derived)) =
            parallel_plan(scan.exec, &[arg], &[], &[], scan.conds, rows, tasks.parallelism())
        && derived.iter().all(Option::is_none)
    {
        session.interrupted()?;
        if let Some((count, Some(first))) =
            groups::distinct(tasks, &scan.exec.stored, &plan, rows, scan.segments)?
        {
            found = Some(groups::Found {
                words: vec![0],
                firsts: vec![first],
                states: aggs.iter().map(|_| groups::Folded::Count(vec![count])).collect(),
                extremes: Vec::new(),
                trailing: false,
            });
        }
    }
    // Calls of `count(DISTINCT)` of one column with one key: the tasks count the pairs of a group and a value.
    if found.is_none()
        && let [key] = keys[..]
        && bases.is_empty()
        && !scan.answered.contains(&true)
        && aggs.iter().all(|call| matches!(call.method, Method::Distinct))
        && let Some(tasks) = session.tasks().filter(|_| scan.open)
        && let Some(arg) = distinct_arg(&aggs)
        && let Some((plan, derived)) =
            parallel_plan(scan.exec, &[key, arg], &[], &[], scan.conds, rows, tasks.parallelism())
        && derived.iter().all(Option::is_none)
    {
        session.interrupted()?;
        found = groups::distinct_groups(
            tasks,
            &scan.exec.stored,
            &plan,
            rows,
            scan.segments,
            aggs.len(),
        )?;
    }
    if let Some(found) = found {
        groups = found.firsts.len();
        trailing = found.trailing;
        let mut folded = found.states.into_iter();
        for (call, state) in aggs.iter_mut().zip(&mut folded) {
            call.states = match state {
                groups::Folded::Count(v) => States::Count(v),
                groups::Folded::Sum(v) => States::Sum(v),
                groups::Folded::Extreme(v) => States::Extreme(v),
                groups::Folded::None => States::Shift,
            };
        }
        for ((base, state), extremes) in bases.iter_mut().zip(folded).zip(found.extremes) {
            if let groups::Folded::Sum(v) = state {
                base.states = v;
            }
            base.extremes = extremes;
        }
        let shapes: Vec<Shape> = keys.iter().map(|key| key_shape(scan.exec, key)).collect();
        if let Some(places) = &hashing {
            hashes.reserve_exact(groups);
            for (g, words) in found.words.chunks(keys.len() + 1).enumerate() {
                match word_hash(places, &shapes, words, &scan.exec.dicts) {
                    Ok(h) => hashes.push(h),
                    Err(_) if g == 0 => {
                        hashing = None;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        key_words.words = found.words;
        key_words.shapes = Some(shapes);
        spots = found.firsts.iter().map(|&first| first_spot(first)).collect();
        if !lazy {
            let wanted: Vec<usize> = (0..groups).collect();
            firsts = vec![Vec::with_capacity(kept_columns.len()); groups];
            read_spots(scan, kept_columns, &spots, &wanted, &mut firsts)?;
            spots = Vec::new();
        }
    } else {
        scan.each(&mut |exec, batch, sel| {
            ids.clear();
            if keys.is_empty() {
                ids.resize(sel.len(), 0);
                if first_rows == 0 {
                    let mut row = Vec::with_capacity(kept_columns.len());
                    for &column in kept_columns {
                        row.push(exec.value_at(batch, column, sel[0])?);
                    }
                    firsts.push(row);
                    first_rows = 1;
                }
            } else {
                let cols =
                    keys.iter().map(|k| exec.eval(k, batch, sel)).collect::<Result<Vec<_>>>()?;
                let batch_shapes: Option<Vec<Shape>> = cols.iter().map(Shape::of).collect();
                new_rows.clear();
                // The words of the keys of each row, and their hashes, come first, so that a probe can start the read of the slot of a later row.
                let width = keys.len() + 1;
                words.clear();
                for i in 0..sel.len() {
                    let mut mask = 0_u64;
                    for (k, col) in cols.iter().enumerate() {
                        if col.null(i) {
                            mask |= 1 << (k % 64);
                            words.push(0);
                        } else {
                            let Some(w) = col.word(i) else {
                                bail = true;
                                return Ok(false);
                            };
                            words.push(w);
                        }
                    }
                    words.push(mask);
                }
                row_hashes.clear();
                row_hashes.extend(words.chunks_exact(width).map(|w| map.hash(w)));
                for (i, &row_at) in sel.iter().enumerate() {
                    if let Some(&ahead) = row_hashes.get(i + PREFETCH_ROWS) {
                        map.prefetch(ahead);
                    }
                    let word = &words[i * width..(i + 1) * width];
                    let (id, new) = match last_id {
                        Some(id) if word.iter().zip(&last_word).all(|(a, b)| a == b) => (id, false),
                        _ => {
                            let next = u32::try_from(groups)
                                .map_err(|_| Error::internal("too many groups"))?;
                            let found = map.id(word, row_hashes[i], next);
                            last_word.copy_from_slice(word);
                            last_id = Some(found.0);
                            found
                        }
                    };
                    trailing = !new;
                    if new {
                        groups += 1;
                        new_rows.push(row_at);
                        if let Some(places) = &hashing {
                            if hash_error.is_none() {
                                match key_hash(places, &cols, i, &exec.dicts) {
                                    Ok(h) => hashes.push(h),
                                    // A key that the hash of the plan cannot read: the groups go in the order of a sort, as before the hash.
                                    Err(_) if hashes.is_empty() => {
                                        hashing = None;
                                        key_words.keep(
                                            word,
                                            batch_shapes.as_deref(),
                                            &cols,
                                            i,
                                            &exec.dicts,
                                            &mut key_values,
                                        );
                                    }
                                    Err(error) => hash_error = Some(error),
                                }
                            }
                        } else {
                            key_words.keep(
                                word,
                                batch_shapes.as_deref(),
                                &cols,
                                i,
                                &exec.dicts,
                                &mut key_values,
                            );
                        }
                    }
                    ids.push(id);
                }
                seen += sel.len();
                // When the next batch can fill the map, the map makes room for the groups that the rate of new groups gives, so it does not grow many times.
                if map.room() < sel.len() {
                    let rate = (groups * rows / seen.max(1)).min(rows);
                    map.reserve((rate + rate / 8).max(groups * 2).min(rows).saturating_sub(groups));
                }
                if !new_rows.is_empty() {
                    if lazy {
                        let (segment, vector) = (spot(batch.segment), spot(batch.vector));
                        spots.extend(new_rows.iter().map(|&row| [segment, vector, row]));
                    } else {
                        let start = firsts.len();
                        firsts.extend(
                            std::iter::repeat_with(|| Vec::with_capacity(kept_columns.len()))
                                .take(new_rows.len()),
                        );
                        for &column in kept_columns {
                            let col = exec.var(batch, column, exec.types[column], &new_rows)?;
                            for (i, row) in firsts[start..].iter_mut().enumerate() {
                                row.push(col.value(i, &exec.dicts));
                            }
                        }
                    }
                    let room = map.room();
                    for call in &mut aggs {
                        grow(call, groups, room, params, session, cache, &empty)?;
                    }
                    for base in &mut bases {
                        if base.states.capacity() < groups {
                            base.states.reserve_exact(groups + room - base.states.len());
                        }
                        base.states.resize(groups, (0, 0));
                    }
                }
            }
            for base in &mut bases {
                let col = exec.eval(base.var, batch, sel)?;
                let Some(ints) = col.ints() else {
                    bail = true;
                    return Ok(false);
                };
                let mut extremes = base.extremes;
                let mut widen = |lo: i64, hi: i64| {
                    extremes = Some(extremes.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
                };
                match ints {
                    Ints::Vector(v, None) if keys.is_empty() => {
                        if let (Some(&lo), Some(&hi)) = (v.iter().min(), v.iter().max()) {
                            widen(lo, hi);
                        }
                        let state = &mut base.states[0];
                        state.0 += i64::try_from(v.len()).unwrap_or(i64::MAX);
                        state.1 += if base.var.ty == oid::INT8 {
                            v.iter().map(|&x| i128::from(x)).sum()
                        } else {
                            i128::from(v.iter().sum::<i64>())
                        };
                    }
                    _ => {
                        for (i, &id) in ids.iter().enumerate() {
                            if let Some(v) = ints.get(i) {
                                widen(v, v);
                                let state = &mut base.states[id as usize];
                                state.0 += 1;
                                state.1 += i128::from(v);
                            }
                        }
                    }
                }
                base.extremes = extremes;
            }
            for call in &mut aggs {
                let arg = match &call.method {
                    Method::Shift { .. } => continue,
                    Method::Count => None,
                    _ => call.agg.args.first(),
                };
                let col = match arg {
                    Some(arg) if !matches!(call.method, Method::Row(_)) => {
                        Some(exec.eval(arg, batch, sel)?)
                    }
                    _ => None,
                };
                match (&call.method, &mut call.states) {
                    (Method::Count, States::Count(states)) => {
                        for &id in &ids {
                            states[id as usize] += 1;
                        }
                    }
                    (Method::CountArg, States::Count(states)) => {
                        let col =
                            col.as_ref().ok_or_else(|| Error::internal("count has no argument"))?;
                        for (i, &id) in ids.iter().enumerate() {
                            if !col.null(i) {
                                states[id as usize] += 1;
                            }
                        }
                    }
                    (Method::Distinct, States::Distinct(states)) => {
                        let col =
                            col.as_ref().ok_or_else(|| Error::internal("count has no argument"))?;
                        // A row with the group and the word of the row before it is already in the set. In ClickBench, 82% of the rows have the `UserID` of the row before them.
                        let mut last = None;
                        for (i, &id) in ids.iter().enumerate() {
                            if col.null(i) {
                                continue;
                            }
                            let Some(w) = col.word(i) else {
                                bail = true;
                                return Ok(false);
                            };
                            if last != Some((id, w)) {
                                states[id as usize].insert(w);
                                last = Some((id, w));
                            }
                        }
                    }
                    (Method::SumInt | Method::Int128 { .. }, States::Sum(states)) => {
                        let col =
                            col.as_ref().ok_or_else(|| Error::internal("sum has no argument"))?;
                        let Some(ints) = col.ints() else {
                            bail = true;
                            return Ok(false);
                        };
                        match ints {
                            Ints::Vector(v, None) if keys.is_empty() => {
                                let state = &mut states[0];
                                state.0 += i64::try_from(v.len()).unwrap_or(i64::MAX);
                                state.1 += v.iter().map(|&x| i128::from(x)).sum::<i128>();
                            }
                            _ => {
                                for (i, &id) in ids.iter().enumerate() {
                                    if let Some(v) = ints.get(i) {
                                        let state = &mut states[id as usize];
                                        state.0 += 1;
                                        state.1 += i128::from(v);
                                    }
                                }
                            }
                        }
                    }
                    (Method::Min | Method::Max, States::Extreme(states)) => {
                        let col =
                            col.as_ref().ok_or_else(|| Error::internal("min has no argument"))?;
                        let Some(ints) = col.ints() else {
                            bail = true;
                            return Ok(false);
                        };
                        let min = matches!(call.method, Method::Min);
                        for (i, &id) in ids.iter().enumerate() {
                            if let Some(v) = ints.get(i) {
                                let state = &mut states[id as usize];
                                *state = Some(match *state {
                                    None => v,
                                    Some(s) if min => s.min(v),
                                    Some(s) => s.max(v),
                                });
                            }
                        }
                    }
                    (Method::Row(agg), States::Row(states)) => {
                        let cols = agg.agg.args[..agg.agg.nargs]
                            .iter()
                            .map(|a| exec.eval(a, batch, sel))
                            .collect::<Result<Vec<_>>>()?;
                        if let (Some(keep), [col]) = (&mut call.codes, cols.as_slice())
                            && let Data::Code(codes, d) = &col.data
                            && (keep.dict == usize::MAX || keep.dict == *d)
                        {
                            keep.dict = *d;
                            let dict = &exec.dicts[*d];
                            if let Some(max) = keep.max
                                && dict.is_text()
                            {
                                keep.best.resize(states.len(), u32::MAX);
                                for (i, &id) in ids.iter().enumerate() {
                                    if col.null(i) {
                                        continue;
                                    }
                                    let (code, best) = (codes[i], &mut keep.best[id as usize]);
                                    if *best == u32::MAX {
                                        *best = code;
                                    } else if code != *best {
                                        let (a, b) = (dict.text(code), dict.text(*best));
                                        if a != b && (a > b) == max {
                                            *best = code;
                                        }
                                    }
                                }
                            } else {
                                keep.seen.resize_with(states.len(), FxSet::default);
                                for (i, &id) in ids.iter().enumerate() {
                                    if !col.null(i) {
                                        keep.seen[id as usize].insert(codes[i]);
                                    }
                                }
                            }
                            continue;
                        }
                        let eval = Eval::new(params, session, &empty, cache, &[]);
                        for (i, &id) in ids.iter().enumerate() {
                            args.clear();
                            args.extend(cols.iter().map(|c| c.value(i, &exec.dicts)));
                            agg.advance(&mut states[id as usize], &args, &eval)?;
                        }
                    }
                    _ => return Err(Error::internal("an aggregate has another state")),
                }
            }
            Ok(true)
        })?;
    }
    if bail {
        return Ok(None);
    }
    for call in &mut aggs {
        let Method::Shift { base, by, range, wide } = call.method else { continue };
        let base = &bases[base];
        if let Some((lo, hi)) = base.extremes
            && (lo.checked_add(by).is_none_or(|v| v < range.0)
                || hi.checked_add(by).is_none_or(|v| v > range.1))
        {
            return Ok(None);
        }
        let by = i128::from(by);
        call.states = States::Sum(
            base.states.iter().map(|&(n, sum)| (n, sum + by * i128::from(n))).collect(),
        );
        call.method = match wide {
            None => Method::SumInt,
            Some(avg) => Method::Int128 { avg },
        };
    }
    // The results of the aggregates of each group: the row engine finishes a call of its own method now, and the scan finishes each other call when the projection reads it.
    let eval = Eval::new(params, session, &empty, cache, &[]);
    let scan_dicts = &scan.exec.dicts;
    for call in &mut aggs {
        match (&call.method, &mut call.states) {
            (Method::Row(agg), States::Row(states)) => {
                if let Some(keep) = &call.codes {
                    let dict = keep.dict;
                    for (id, state) in states.iter_mut().enumerate() {
                        if let Some(&code) = keep.best.get(id).filter(|&&c| c != u32::MAX) {
                            let value = scan_dicts[dict].value(code);
                            agg.advance(state, std::slice::from_ref(&value), &eval)?;
                        }
                        let Some(seen) = keep.seen.get(id) else { continue };
                        let mut codes: Vec<u32> = seen.iter().copied().collect();
                        codes.sort_unstable();
                        for code in codes {
                            let value = scan_dicts[dict].value(code);
                            agg.advance(state, std::slice::from_ref(&value), &eval)?;
                        }
                    }
                }
                let done = std::mem::take(states)
                    .into_iter()
                    .map(|state| agg.finish(state, &eval))
                    .collect::<Result<Vec<_>>>()?;
                call.states = States::Done(done);
            }
            // The count of the values of each group, so that `ORDER BY` of the count reads the counts as for `count(*)`, and the sets go.
            (Method::Distinct, States::Distinct(states)) => {
                call.states = States::Count(
                    states.iter().map(|s| i64::try_from(s.len()).unwrap_or(i64::MAX)).collect(),
                );
            }
            // `int8_sum` gives the error of a sum out of the range of `bigint` for each group, also for a group that `LIMIT` does not keep.
            (Method::SumInt, States::Sum(states))
                if states.iter().any(|&(_, sum)| i64::try_from(sum).is_err()) =>
            {
                return Err(Error::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    "bigint out of range",
                ));
            }
            _ => {}
        }
    }
    let results = Results { calls: aggs, groups };
    drop(eval);
    if let Some(error) = hash_error {
        return Err(error);
    }
    let hashes = hashing.map(|_| hashes);
    let keys_of_groups = match &key_words.shapes {
        Some(shapes) if key_words.by_words => {
            GroupKeys::Words { words: &key_words.words, shapes, dicts: &scan.exec.dicts }
        }
        _ => GroupKeys::Values(&key_values),
    };
    // The groups of `ORDER BY` of a count come from the counts alone when the counts of the result are all different, and then the scan needs no order of the groups.
    let quick = match top {
        Some(key) => distinct_top(session, &results, key, offset)?,
        None => None,
    };
    let mut order = if quick.is_some() {
        Vec::new()
    } else {
        // With `ORDER BY` of a count, the order has only the groups that change the heap of [`count_top`], which keeps the same groups as with the full order.
        let want = match (top, first_groups) {
            (Some((agg, descending, bound)), _) if groups > bound.saturating_mul(4) => {
                top_counts(&results, agg).map_or(Want::All, |counts| Want::Top {
                    counts,
                    descending,
                    bound,
                })
            }
            (_, Some(first)) => Want::First(first),
            _ => Want::All,
        };
        group_order(query, session, &keys_of_groups, hashes.as_deref(), groups, trailing, want)?
    };
    let mut chosen = None;
    let mut first_rows = Firsts::All(firsts.len());
    if let Some(key) = top {
        chosen = match quick {
            Some(quick) => Some(quick),
            None => count_top(session, &results, &order, key, offset)?,
        };
        // The groups of the result read their first rows, and the other groups read none.
        let mut wanted: Vec<usize> = match &chosen {
            Some(chosen) => chosen.clone(),
            None => (0..spots.len()).collect(),
        };
        wanted.sort_unstable();
        firsts = vec![Vec::new(); wanted.len()];
        read_spots(scan, kept_columns, &spots, &wanted, &mut firsts)?;
        first_rows = Firsts::Of(wanted);
    } else if let Some(first) = first_groups {
        let mut wanted: Vec<usize> = order.iter().take(first).copied().collect();
        wanted.sort_unstable();
        firsts = vec![Vec::new(); wanted.len()];
        read_spots(scan, kept_columns, &spots, &wanted, &mut firsts)?;
        first_rows = Firsts::Of(wanted);
    }
    // With no sort, the limit of the query takes the first groups, and the groups after them are not evaluated, as `LIMIT` over `HashAggregate` does.
    let needed = match count {
        Some(count) if query.sort.is_empty() && query.distinct.is_empty() => {
            offset.saturating_add(count)
        }
        _ => usize::MAX,
    };
    if having_first && let Some(having) = &query.having {
        // `HAVING` runs once for each group in the order of the groups, up to the groups that the limit needs, as in the projection.
        let mut read: Vec<*const Aggref> = Vec::new();
        having.find(0, &mut |e, _| {
            if let ExprKind::Agg(agg) = &e.kind {
                read.push(std::ptr::from_ref(&**agg));
            }
            None::<()>
        });
        let mut eval = Eval::new(params, session, &empty, cache, &[]);
        eval.tuple = &[scan::NONE];
        let mut passed = Vec::new();
        for (n, &g) in order.iter().enumerate() {
            if passed.len() >= needed {
                break;
            }
            crate::interrupt(session, n)?;
            eval.group = Group { aggs: results.group(g, Some(&read))?, ..Group::default() };
            if eval.eval(having)? == Value::Bool(true) {
                passed.push(g);
            }
        }
        drop(eval);
        order = passed;
        let mut wanted = order.clone();
        wanted.sort_unstable();
        firsts = vec![Vec::new(); wanted.len()];
        read_spots(scan, kept_columns, &spots, &wanted, &mut firsts)?;
        first_rows = Firsts::Of(wanted);
    }
    let mut kept = vec![false; scan.exec.types.len()];
    for &column in kept_columns {
        kept[column] = true;
    }
    let tables = scan::kept_tables(query, firsts, &kept);
    let (rows, ordered) = project(
        query,
        params,
        session,
        cache,
        &tables,
        needed,
        if having_first { None } else { query.having.as_ref() },
        Some((order, results, first_rows, chosen)),
        offset,
        count,
    )?;
    Ok(Some((rows, tables, ordered)))
}

/// The part of the aggregate of one call that the summary of one segment gives.
enum Part {
    Count(i64),
    Sum(i64, i128),
    Extreme(Option<i64>),
    /// A call of [`Method::Shift`], whose part is the part of its base.
    Base,
}

/// The aggregates of a query with no groups, no conditions and no columns outside its aggregates, for each segment whose summaries hold them exactly (spec/12 section 12.6.1). `count(*)` comes from the rows, `count(x)` from the nulls, `sum` and `avg` from the sum, and `min` and `max` from the bounds. The columns of the scan have every row of the table live and seen by the view, so the summaries need no correction for deletes (spec/09 section 9.7.4). It adds the parts to the states of `aggs` and `bases`, and gives true for each segment that it answered. A call that a summary cannot answer leaves every segment to the scan.
fn answer(scan: &Scan<'_, '_>, aggs: &mut [Call<'_>], bases: &mut [Base<'_>]) -> Vec<bool> {
    let exec = &*scan.exec;
    let place = |e: &Expr| match &e.kind {
        ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => {
            usize::try_from(var.attnum - 1)
                .ok()
                .and_then(|c| exec.places.get(c).copied())
                .filter(|&p| p < exec.stored.len())
        }
        _ => None,
    };
    let int = |p: usize| exec.stored[p].kind.store() == Store::Int;
    let mut places = Vec::with_capacity(aggs.len());
    for call in aggs.iter() {
        let arg = call.agg.args.first();
        places.push(match call.method {
            Method::Count => None,
            Method::Shift { .. } => None,
            Method::CountArg => match arg.and_then(place) {
                Some(p) => Some(p),
                None => return Vec::new(),
            },
            Method::SumInt | Method::Int128 { .. } | Method::Min | Method::Max => {
                match arg.and_then(place).filter(|&p| int(p)) {
                    Some(p) => Some(p),
                    None => return Vec::new(),
                }
            }
            Method::Distinct | Method::Row(_) => return Vec::new(),
        });
    }
    let Some(base_places) =
        bases.iter().map(|b| place(b.var).filter(|&p| int(p))).collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    let sum = |summary: &ColumnSummary| match summary.sum {
        Sum::Int(sum) => Some((i64::from(summary.count()), sum)),
        _ => None,
    };
    let bound = |b: &Option<Bound>| match b {
        Some(Bound::Int(v)) => i64::try_from(*v).ok(),
        _ => None,
    };
    let mut answered = vec![false; scan.segments];
    let mut parts = Vec::with_capacity(aggs.len());
    let mut base_parts = Vec::with_capacity(bases.len());
    for (segment, done) in answered.iter_mut().enumerate() {
        let rows = scan.rows.saturating_sub(segment * ROWS).min(ROWS);
        let summary = |p: usize| {
            exec.stored[p]
                .segments
                .get(segment)
                .map(|column| &column.summary)
                .filter(|s| s.rows as usize == rows)
        };
        parts.clear();
        base_parts.clear();
        let mut whole = true;
        for (call, &p) in aggs.iter().zip(&places) {
            let part = match (&call.method, p.and_then(summary)) {
                (Method::Count, _) => Some(Part::Count(i64::try_from(rows).unwrap_or(i64::MAX))),
                (Method::Shift { .. }, _) => Some(Part::Base),
                (Method::CountArg, Some(s)) => Some(Part::Count(i64::from(s.count()))),
                (Method::SumInt | Method::Int128 { .. }, Some(s)) => {
                    sum(s).map(|(n, sum)| Part::Sum(n, sum))
                }
                (Method::Min, Some(s)) if s.count() == 0 => Some(Part::Extreme(None)),
                (Method::Max, Some(s)) if s.count() == 0 => Some(Part::Extreme(None)),
                (Method::Min, Some(s)) => bound(&s.min).map(|v| Part::Extreme(Some(v))),
                (Method::Max, Some(s)) => bound(&s.max).map(|v| Part::Extreme(Some(v))),
                _ => None,
            };
            let Some(part) = part else {
                whole = false;
                break;
            };
            parts.push(part);
        }
        for &p in &base_places {
            if !whole {
                break;
            }
            let Some(s) = summary(p) else {
                whole = false;
                break;
            };
            let extremes = match (s.count(), bound(&s.min), bound(&s.max)) {
                (0, _, _) => None,
                (_, Some(lo), Some(hi)) => Some((lo, hi)),
                _ => {
                    whole = false;
                    break;
                }
            };
            let Some((n, sum)) = sum(s) else {
                whole = false;
                break;
            };
            base_parts.push((n, sum, extremes));
        }
        if !whole {
            continue;
        }
        *done = true;
        for (call, part) in aggs.iter_mut().zip(parts.drain(..)) {
            match (&mut call.states, part) {
                (States::Count(states), Part::Count(n)) => states[0] += n,
                (States::Sum(states), Part::Sum(n, sum)) => {
                    states[0].0 += n;
                    states[0].1 += sum;
                }
                (States::Extreme(states), Part::Extreme(Some(v))) => {
                    let min = matches!(call.method, Method::Min);
                    states[0] = Some(match states[0] {
                        None => v,
                        Some(s) if min => s.min(v),
                        Some(s) => s.max(v),
                    });
                }
                _ => {}
            }
        }
        for (base, (n, sum, extremes)) in bases.iter_mut().zip(base_parts.drain(..)) {
            base.states[0].0 += n;
            base.states[0].1 += sum;
            if let Some((lo, hi)) = extremes {
                base.extremes =
                    Some(base.extremes.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
            }
        }
    }
    answered
}

/// The plan of [`groups::run`] for the keys `keys`, the calls `aggs` and the bases `bases` of a scan of `rows` rows with the conditions `conds`, or `None` when a key, a call, a condition or the table does not let the tasks find the groups. Each key is a column of integers or codes, or a constant integer or null, and each call is a count or an aggregate of a column of integers. Each condition is a test that the zones read (see [`Exec::zone_test`]).
fn parallel_plan(
    exec: &Exec<'_>,
    keys: &[&Expr],
    aggs: &[Call<'_>],
    bases: &[Base<'_>],
    conds: &[&Expr],
    rows: usize,
    parallelism: usize,
) -> Option<(groups::Plan, Vec<Option<Derived>>)> {
    if parallelism < 2 || rows < 2 * groups::TASK_ROWS || keys.is_empty() {
        return None;
    }
    let place = |expr: &Expr| place_of(exec, expr);
    let int_place =
        |expr: &Expr| place(expr).filter(|&p| matches!(exec.stored[p].kind.store(), Store::Int));
    // A key that is a column of an other key plus a constant gets its words from the words of that key after the scan, so that the groups have fewer words.
    let derived: Vec<Option<Derived>> = keys
        .iter()
        .map(|key| {
            let (var, by, range) = shifted(key)?;
            let from = keys.iter().position(|k| k.kind == var.kind)?;
            place_of(exec, var).filter(|&p| matches!(exec.stored[p].kind.store(), Store::Int))?;
            Some(Derived { from, by, range })
        })
        .collect();
    let mut at = 0;
    let base_at: Vec<usize> = derived
        .iter()
        .map(|d| {
            let here = at;
            at += usize::from(d.is_none());
            here
        })
        .collect();
    let derived: Vec<Option<Derived>> =
        derived.into_iter().map(|d| d.map(|d| Derived { from: base_at[d.from], ..d })).collect();
    if at > 7 {
        return None;
    }
    let keys = keys
        .iter()
        .zip(&derived)
        .filter(|(_, d)| d.is_none())
        .map(|(key, _)| match &key.kind {
            ExprKind::Const(v) if v.is_null() => Some(groups::Input::Const(None)),
            ExprKind::Const(v) => int_of(v).map(|v| groups::Input::Const(Some(v as u64))),
            ExprKind::Func(func) if src(func.oid) == "timestamp_trunc" => {
                match func.args.as_slice() {
                    [field, arg] if arg.ty == oid::TIMESTAMP => {
                        Some(groups::Input::Floor(int_place(arg)?, unit(field)?))
                    }
                    _ => None,
                }
            }
            ExprKind::Func(func)
                if src(func.oid) == "extract_timestamp" && key.ty == oid::NUMERIC =>
            {
                match func.args.as_slice() {
                    [field, arg] if arg.ty == oid::TIMESTAMP => {
                        let step = unit(field)?;
                        let span = match step {
                            60_000_000 => 60,
                            3_600_000_000 => 24,
                            _ => return None,
                        };
                        Some(groups::Input::Part(int_place(arg)?, step, span))
                    }
                    _ => None,
                }
            }
            ExprKind::Case(case) => case_input(exec, case),
            _ => place(key)
                .filter(|&p| !matches!(exec.stored[p].kind.store(), Store::Float))
                .map(groups::Input::Column),
        })
        .collect::<Option<Vec<_>>>()?;
    let mut folds = aggs
        .iter()
        .map(|call| {
            let arg = call.agg.args.first();
            Some(match call.method {
                Method::Count => groups::Fold::Count,
                Method::CountArg => groups::Fold::CountArg(place(arg?)?),
                Method::SumInt | Method::Int128 { .. } => groups::Fold::Sum(int_place(arg?)?),
                Method::Min => groups::Fold::Min(int_place(arg?)?),
                Method::Max => groups::Fold::Max(int_place(arg?)?),
                Method::Shift { .. } => groups::Fold::None,
                Method::Distinct | Method::Row(_) => return None,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    for base in bases {
        folds.push(groups::Fold::Sum(int_place(base.var)?));
    }
    let filters = conds.iter().map(|cond| exec.zone_test(cond)).collect::<Option<Vec<_>>>()?;
    Some((groups::Plan { keys, folds, bases: bases.len(), filters }, derived))
}

/// The key of [`groups::run`] for `CASE WHEN tests THEN column ELSE constant END`, where each test is a test that the zones read, the column has integers or codes, and the constant is null, an integer, or a text in the dictionary of the column.
fn case_input(exec: &Exec<'_>, case: &rupg_analyze::Case) -> Option<groups::Input> {
    let [(cond, result)] = case.whens.as_slice() else { return None };
    if case.arg.is_some() {
        return None;
    }
    let conds = match &cond.kind {
        ExprKind::Bool(BoolOp::And, args) => args.iter().collect(),
        _ => vec![cond],
    };
    let tests = conds.into_iter().map(|c| exec.zone_test(c)).collect::<Option<Vec<_>>>()?;
    let then =
        place_of(exec, result).filter(|&p| !matches!(exec.stored[p].kind.store(), Store::Float))?;
    let dict = exec.dict_of[then];
    let default = match &case.default.kind {
        ExprKind::Const(v) if v.is_null() => None,
        ExprKind::Const(Value::Text(text)) if dict != usize::MAX && exec.dicts[dict].is_text() => {
            let code = exec.dicts[dict].find(text.as_bytes());
            if code == MISSING {
                return None;
            }
            Some(u64::from(code))
        }
        ExprKind::Const(v) if dict == usize::MAX => Some(int_of(v)? as u64),
        _ => return None,
    };
    Some(groups::Input::Case { tests, then, default })
}

/// The shape of the words of the key `key` of [`groups::run`]: the codes of the dictionary of its column, or integers of its type.
fn key_shape(exec: &Exec<'_>, key: &Expr) -> Shape {
    let column = match &key.kind {
        ExprKind::Case(case) => case.whens.first().and_then(|(_, result)| place_of(exec, result)),
        _ => place_of(exec, key),
    };
    match column {
        Some(place) if exec.dict_of[place] != usize::MAX => Shape::Code(exec.dict_of[place]),
        _ => Shape::Int(key.ty),
    }
}

/// The argument of the calls of `count(DISTINCT)` of `aggs`, when there is one and every call has the same argument.
fn distinct_arg<'q>(aggs: &[Call<'q>]) -> Option<&'q Expr> {
    let mut args = aggs
        .iter()
        .filter(|call| matches!(call.method, Method::Distinct))
        .map(|call| call.agg.args.first());
    let first = args.next()??;
    args.all(|arg| arg.is_some_and(|arg| arg.kind == first.kind)).then_some(first)
}

/// The place of the stored column of `expr` when it is a column that the scan reads.
fn place_of(exec: &Exec<'_>, expr: &Expr) -> Option<usize> {
    let ExprKind::Var(var) = &expr.kind else { return None };
    let column = usize::try_from(var.attnum - 1).ok()?;
    let place = *exec.places.get(column)?;
    (place != usize::MAX).then_some(place)
}

/// A key of a group whose value is the value of the key `from` of the plan plus `by`, with the range `range` of its type.
#[derive(Clone, Copy)]
struct Derived {
    from: usize,
    by: i64,
    range: (i64, i64),
}

/// Gives the words of all the keys `derived` of each group from the words `words` of the keys of the plan, with `width` words for each group. It gives `None` when a value is out of the range of its type, so that the scan of each row gives the error.
fn derive_words(words: &[u64], width: usize, derived: &[Option<Derived>]) -> Option<Vec<u64>> {
    if derived.iter().all(Option::is_none) {
        return Some(words.to_vec());
    }
    let mut out = Vec::with_capacity(words.len() / width * (derived.len() + 1));
    for group in words.chunks_exact(width) {
        let mask = group[width - 1];
        let mut new_mask = 0_u64;
        let mut base = 0;
        for (k, d) in derived.iter().enumerate() {
            let (word, null) = match d {
                None => {
                    base += 1;
                    (group[base - 1], mask >> (base - 1) & 1 == 1)
                }
                Some(d) if mask >> d.from & 1 == 1 => (0, true),
                Some(d) => {
                    let v = (group[d.from] as i64).checked_add(d.by)?;
                    if v < d.range.0 || v > d.range.1 {
                        return None;
                    }
                    (v as u64, false)
                }
            };
            out.push(word);
            new_mask |= u64::from(null) << (k % 64);
        }
        out.push(new_mask);
    }
    Some(out)
}

/// The hash of the keys `places` of a group from its words `words` with the shapes `shapes`, as [`key_hash`] gives it from the first row of the group.
fn word_hash(
    places: &[(usize, u32)],
    shapes: &[Shape],
    words: &[u64],
    dicts: &[Dict],
) -> Result<u32> {
    let mask = words[shapes.len()];
    let mut hash: u32 = 0;
    for &(p, ty) in places {
        hash = hash.rotate_left(1);
        if mask >> p & 1 == 1 {
            continue;
        }
        let one = match shapes[p] {
            Shape::Int(t) if t == ty => rupg_func::extended_hash_int(ty, words[p] as i64, 0),
            Shape::Code(d) => {
                dicts[d].text(words[p] as u32).and_then(|b| rupg_func::extended_hash_text(ty, b, 0))
            }
            Shape::Int(_) => None,
        };
        let one = match one {
            Some(one) => one,
            None => rupg_func::extended_hash(ty, &shapes[p].value(words[p], dicts), 0)?,
        };
        #[allow(clippy::cast_possible_truncation)]
        let one = one as u32;
        hash ^= one;
    }
    Ok(hashed::murmurhash32(hash))
}

/// The spot of [`read_spots`] of the row `first` of the table, as `segment * ROWS + vector * VECTOR + row`.
fn first_spot(first: u64) -> [u32; 3] {
    let first = usize::try_from(first).unwrap_or(usize::MAX);
    [spot(first / ROWS), spot(first % ROWS / VECTOR), spot(first % VECTOR)]
}

/// A place in the table as a part of a spot of [`read_spots`].
fn spot(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The values of the columns `kept_columns` of the first row of each group of `wanted`, into `firsts[i]` for the group `wanted[i]`, from the segment, the vector and the row of each group in `spots`. The groups of `wanted` are in the order of the scan, so the scan decodes each vector once.
fn read_spots(
    scan: &mut Scan<'_, '_>,
    kept_columns: &[usize],
    spots: &[[u32; 3]],
    wanted: &[usize],
    firsts: &mut [Vec<Value>],
) -> Result<()> {
    let exec = &mut *scan.exec;
    let mut at = 0;
    while at < wanted.len() {
        let [segment, vector, _] = spots[wanted[at]];
        let end =
            at + wanted[at..].iter().take_while(|&&g| spots[g][..2] == [segment, vector]).count();
        let (segment, vector) = (segment as usize, vector as usize);
        let segment_rows = scan.rows.saturating_sub(segment * ROWS).min(ROWS);
        let mut batch = Batch {
            segment,
            vector,
            rows: (segment_rows - vector * VECTOR).min(VECTOR),
            decoded: (0..exec.stored.len()).map(|_| None).collect(),
        };
        let sel: Vec<u32> = wanted[at..end].iter().map(|&g| spots[g][2]).collect();
        for &column in kept_columns {
            let col = exec.var(&mut batch, column, exec.types[column], &sel)?;
            for (i, row) in firsts[at..end].iter_mut().enumerate() {
                row.push(col.value(i, &exec.dicts));
            }
        }
        at = end;
    }
    Ok(())
}

/// A new group for an aggregate: the states of `groups` groups, with room for `room` more when a vector of states grows.
fn grow(
    call: &mut Call<'_>,
    groups: usize,
    room: usize,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    empty: &scan::Tables,
) -> Result<()> {
    // A vector that is full gets room for the groups that the map has room for, so it does not grow many times.
    fn more<T>(s: &mut Vec<T>, groups: usize, room: usize) {
        if s.capacity() < groups {
            s.reserve_exact(groups + room - s.len());
        }
    }
    match &mut call.states {
        States::Count(s) => {
            more(s, groups, room);
            s.resize(groups, 0);
        }
        States::Sum(s) => {
            more(s, groups, room);
            s.resize(groups, (0, 0));
        }
        States::Extreme(s) => {
            more(s, groups, room);
            s.resize(groups, None);
        }
        States::Distinct(s) => s.resize_with(groups, FxSet::default),
        States::Shift | States::Done(_) => {}
        States::Row(s) => {
            if let Method::Row(agg) = &call.method {
                let eval = Eval::new(params, session, empty, cache, &[]);
                while s.len() < groups {
                    s.push(agg.start(&eval)?);
                }
            }
        }
    }
    Ok(())
}

/// The number of groups that the hash table of the plan expects, or 0 when the plan has no hash table for the groups.
fn session_groups(session: &dyn Session, query: &Query) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    session
        .hash_tables(query)
        .and_then(|t| t.group.as_ref().map(|g| g.groups as usize))
        .unwrap_or(0)
}

/// The places in `GROUP BY` and the types of the keys of the hash table of the `HashAggregate` of the plan, or `None` when the plan has no such table.
fn plan_keys(session: &dyn Session, query: &Query) -> Option<Vec<(usize, u32)>> {
    if query.group.is_empty() {
        return None;
    }
    let table = session.hash_tables(query)?.group.clone()?;
    table
        .keys
        .iter()
        .map(|t| {
            let p = query.group.iter().position(|g| g.target == *t)?;
            Some((p, query.targets[query.group[p].target].expr.ty))
        })
        .collect()
}

/// The hash of the keys `places` of the row `i` of the key columns `cols`, as the hash table of the plan finds it.
fn key_hash(places: &[(usize, u32)], cols: &[Col], i: usize, dicts: &[Dict]) -> Result<u32> {
    let mut hash: u32 = 0;
    for &(p, ty) in places {
        let col = &cols[p];
        hash = hash.rotate_left(1);
        if col.null(i) {
            continue;
        }
        // An integer or a text with a code gives its hash with no value.
        let one = match &col.data {
            Data::Int(v) if col.ty == ty => rupg_func::extended_hash_int(ty, v[i], 0),
            Data::Code(codes, d) if col.ty == ty => {
                dicts[*d].text(codes[i]).and_then(|b| rupg_func::extended_hash_text(ty, b, 0))
            }
            _ => None,
        };
        let one = match one {
            Some(one) => one,
            None => rupg_func::extended_hash(ty, &col.value(i, dicts), 0)?,
        };
        #[allow(clippy::cast_possible_truncation)]
        let one = one as u32;
        hash ^= one;
    }
    Ok(hashed::murmurhash32(hash))
}

/// The shape of the words of a key of `GROUP BY` in a batch, from which its value comes back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// An integer of the type.
    Int(u32),
    /// A code of a dictionary of [`Exec::dicts`].
    Code(usize),
}

impl Shape {
    fn of(col: &Col) -> Option<Shape> {
        match &col.data {
            Data::Int(_) => Some(Shape::Int(col.ty)),
            Data::Code(_, d) => Some(Shape::Code(*d)),
            // A constant integer has its integer as its word, as a column of integers.
            Data::Const(v) if v.is_null() || int_of(v).is_some() => Some(Shape::Int(col.ty)),
            _ => None,
        }
    }

    fn value(self, word: u64, dicts: &[Dict]) -> Value {
        match self {
            Shape::Int(ty) => int_value(ty, word as i64),
            Shape::Code(d) => dicts[d].value(word as u32),
        }
    }
}

/// The keys of the groups as their words, which cost no value for each group, while the keys of each new group have the same shapes.
struct KeyWords {
    /// The words of the keys and the mask of nulls of each group.
    words: Vec<u64>,
    shapes: Option<Vec<Shape>>,
    /// False after a key with another shape: the keys are then in the values.
    by_words: bool,
}

impl KeyWords {
    /// Keeps the keys `word` of the row `i` of a new group. The keys of the batch have the shapes `batch`.
    fn keep(
        &mut self,
        word: &[u64],
        batch: Option<&[Shape]>,
        cols: &[Col],
        i: usize,
        dicts: &[Dict],
        values: &mut Vec<Value>,
    ) {
        if self.by_words {
            if self.shapes.is_none() {
                self.shapes = batch.map(<[Shape]>::to_vec);
            }
            if batch.is_some() && batch == self.shapes.as_deref() {
                self.words.extend_from_slice(word);
                return;
            }
            // A key changes its shape: the keys of the groups before go into the values.
            if let Some(shapes) = &self.shapes {
                *values = values_of_words(&self.words, shapes, dicts);
            }
            self.words = Vec::new();
            self.by_words = false;
        }
        values.extend(cols.iter().map(|c| c.value(i, dicts)));
    }
}

/// The keys of the groups for [`group_order`].
enum GroupKeys<'a> {
    /// The values of the keys of each group.
    Values(&'a [Value]),
    /// The words of [`KeyWords`].
    Words { words: &'a [u64], shapes: &'a [Shape], dicts: &'a [Dict] },
}

/// The values of the keys of each group from the words of [`KeyWords`].
fn values_of_words(words: &[u64], shapes: &[Shape], dicts: &[Dict]) -> Vec<Value> {
    let mut values = Vec::with_capacity(words.len());
    for group in words.chunks(shapes.len() + 1) {
        let mask = group[shapes.len()];
        for (k, shape) in shapes.iter().enumerate() {
            values.push(if mask >> k & 1 == 1 {
                Value::Null
            } else {
                shape.value(group[k], dicts)
            });
        }
    }
    values
}

/// The order of the groups by the sort of their keys, from the words of the keys, for keys whose sort compares integers or bytes. The keys of two groups are never all equal, so each sort gives the order of the sort of the row engine. A key of text compares the places of its codes in the order of the dictionary, which the dictionary keeps for the next queries, or the places of the codes of the groups in the order of their bytes for a few groups. `None` for a key of codes with no text.
fn word_order(
    tasks: Option<&dyn Tasks>,
    words: &[u64],
    shapes: &[Shape],
    dicts: &[Dict],
    natives: &[(usize, bool, bool)],
    groups: usize,
    want: Want<'_>,
) -> Option<Vec<usize>> {
    let width = shapes.len() + 1;
    let null = |g: usize, k: usize| words[g * width + shapes.len()] >> k & 1 == 1;
    /// The places of a key, true for descending, true for nulls first, and the key.
    type PlaceKey = (Vec<u64>, bool, bool, usize);
    let mut places: Vec<PlaceKey> = Vec::with_capacity(natives.len());
    for &(k, descending, nulls_first) in natives {
        let key = words.chunks(width).map(|g| g[k]);
        let by = match shapes[k] {
            Shape::Int(_) => concat(on_parts(tasks, groups, |range| {
                range.map(|g| words[g * width + k] ^ (1 << 63)).collect()
            })),
            Shape::Code(d) => {
                let dict = match &dicts[d] {
                    Dict::Stored(stored) if stored.kind == Kind::Text => stored.dict.as_ref()?,
                    Dict::Own(dict) => dict,
                    Dict::Stored(_) => return None,
                };
                if dict.has_order() || groups.saturating_mul(4) >= dict.len() {
                    let order = dict.order();
                    concat(on_parts(tasks, groups, |range| {
                        range
                            .map(|g| words[g * width + k] as usize)
                            .map(|w| u64::from(order.get(w).copied().unwrap_or(0)))
                            .collect()
                    }))
                } else {
                    code_places(dict, key, |g| null(g, k))
                }
            }
        };
        places.push((by, descending, nulls_first, k));
    }
    // Each key of a group is one number in the order of the sort, with the null flag above the place, and a sort of the numbers next to the groups reads no other memory.
    let number = |g: usize, (places, descending, nulls_first, k): &PlaceKey| {
        let null = null(g, *k);
        let place = if *descending { !places[g] } else { places[g] };
        // A null is above each value, or below each value for nulls first.
        match (null, *nulls_first) {
            (true, false) => 1_u128 << 64,
            (true, true) => 0,
            (false, false) => u128::from(place),
            (false, true) => 1_u128 << 64 | u128::from(place),
        }
    };
    let order = match places.as_slice() {
        [one] => {
            let numbers: Vec<(u128, u32)> =
                (0..groups).map(|g| (number(g, one), g as u32)).collect();
            first_of(numbers, want, |n| n.1 as usize, Ord::cmp)
                .into_iter()
                .map(|(_, g)| g as usize)
                .collect()
        }
        [one, two] => {
            let numbers: Vec<(u128, u128, u32)> =
                (0..groups).map(|g| (number(g, one), number(g, two), g as u32)).collect();
            first_of(numbers, want, |n| n.2 as usize, Ord::cmp)
                .into_iter()
                .map(|(_, _, g)| g as usize)
                .collect()
        }
        _ => {
            if let Some(order) =
                packed_order(tasks, groups, places.len(), want, |g, p| number(g, &places[p]))
            {
                return Some(order);
            }
            let n = places.len();
            let numbers: Vec<u128> = (0..groups)
                .flat_map(|g| places.iter().map(move |p| (g, p)))
                .map(|(g, p)| number(g, p))
                .collect();
            first_of(
                (0..groups).collect(),
                want,
                |&g| g,
                |&a: &usize, &b: &usize| {
                    numbers[a * n..(a + 1) * n].cmp(&numbers[b * n..(b + 1) * n]).then(a.cmp(&b))
                },
            )
        }
    };
    Some(order)
}

/// The order of `groups` groups by `keys` numbers of each, when the numbers less their least value fit together in 128 bits. `number` gives the number of a key of a group. A sort of one number for each group compares no memory, and groups with equal numbers stay in their order.
fn packed_order(
    tasks: Option<&dyn Tasks>,
    groups: usize,
    keys: usize,
    want: Want<'_>,
    number: impl Fn(usize, usize) -> u128 + Sync,
) -> Option<Vec<usize>> {
    let mut least = vec![u128::MAX; keys];
    let mut most = vec![0_u128; keys];
    for (low, high) in on_parts(tasks, groups, |range| {
        let mut low = vec![u128::MAX; keys];
        let mut high = vec![0_u128; keys];
        for g in range {
            for k in 0..keys {
                let n = number(g, k);
                low[k] = low[k].min(n);
                high[k] = high[k].max(n);
            }
        }
        (low, high)
    }) {
        for k in 0..keys {
            least[k] = least[k].min(low[k]);
            most[k] = most[k].max(high[k]);
        }
    }
    let bits: Vec<u32> =
        (0..keys).map(|k| 128 - (most[k].saturating_sub(least[k])).leading_zeros()).collect();
    if bits.iter().sum::<u32>() > 128 {
        return None;
    }
    let (bits, least) = (&bits, &least);
    let numbers: Vec<(u128, u32)> = concat(on_parts(tasks, groups, |range| {
        range
            .map(|g| {
                let packed = (0..keys).fold(0_u128, |packed, k| {
                    // A key of no bits has one value and adds nothing.
                    if bits[k] == 0 {
                        packed
                    } else {
                        packed.checked_shl(bits[k]).unwrap_or(0) | (number(g, k) - least[k])
                    }
                });
                (packed, g as u32)
            })
            .collect()
    }));
    Some(
        first_of(numbers, want, |n| n.1 as usize, Ord::cmp)
            .into_iter()
            .map(|(_, g)| g as usize)
            .collect(),
    )
}

/// The fewest groups for each task of [`on_parts`], so that few groups do not pay for the tasks.
const ORDER_GROUPS: usize = 1 << 15;

/// The results of `f` for the parts of the groups `0..groups`, in the order of the parts. With tasks and many groups, each task does one part.
fn on_parts<T: Send>(
    tasks: Option<&dyn Tasks>,
    groups: usize,
    f: impl Fn(std::ops::Range<usize>) -> T + Sync,
) -> Vec<T> {
    let jobs = tasks.map_or(1, |t| t.parallelism().min(groups / ORDER_GROUPS));
    let Some(tasks) = tasks.filter(|_| jobs > 1) else {
        return vec![f(0..groups)];
    };
    let chunk = groups.div_ceil(jobs);
    let mut out: Vec<Option<T>> = (0..groups.div_ceil(chunk)).map(|_| None).collect();
    let f = &f;
    let work: Vec<Job<'_>> = out
        .iter_mut()
        .enumerate()
        .map(|(n, slot)| {
            let range = n * chunk..((n + 1) * chunk).min(groups);
            Box::new(move || *slot = Some(f(range))) as Job<'_>
        })
        .collect();
    tasks.run_all(work);
    out.into_iter().flatten().collect()
}

/// The items of the parts `parts` one after the other.
fn concat<T: Copy>(mut parts: Vec<Vec<T>>) -> Vec<T> {
    if parts.len() == 1 {
        return parts.pop().unwrap_or_default();
    }
    parts.concat()
}

/// The groups of the order of the groups that a query reads.
#[derive(Clone, Copy, Debug)]
enum Want<'a> {
    /// Each group.
    All,
    /// The first groups of the order.
    First(usize),
    /// The groups that change the bounded heap of [`count_top`] of `bound` groups by `counts`, in their order. The heap takes each group of the order, and keeps a group only when its count comes before the count at its top. The count at the top only moves forward, so after the first groups of the order, a group whose count does not come before the top of the heap of these first groups changes nothing, and the order does not have it.
    Top { counts: &'a [i64], descending: bool, bound: usize },
}

/// The part of the groups before [`Want::Top`] removes the groups that do not change the heap: one in 64, and at least 4 times the bound.
const TOP_PREFIX: usize = 64;

/// The items of `items` in the order of `cmp`, or only the items of `want`. `group` gives the group of an item. A selection puts the first items before the others with no sort of the others, so a small `want` of many items costs one pass and a sort of the first items. `cmp` gives no two items equal, so the first items are the items of a sort of all.
fn first_of<T>(
    mut items: Vec<T>,
    want: Want<'_>,
    group: impl Fn(&T) -> usize,
    mut cmp: impl FnMut(&T, &T) -> std::cmp::Ordering,
) -> Vec<T> {
    match want {
        Want::All => {}
        Want::First(limit) if limit < items.len() => {
            if limit == 0 {
                return Vec::new();
            }
            items.select_nth_unstable_by(limit - 1, &mut cmp);
            items.truncate(limit);
        }
        Want::First(_) => {}
        Want::Top { counts, descending, bound } => {
            let prefix = (items.len() / TOP_PREFIX).max(bound.saturating_mul(4));
            if bound > 0 && prefix < items.len() {
                // A larger rank comes first in the sort.
                let rank = |item: &T| {
                    let c = i128::from(counts[group(item)]);
                    if descending { c } else { -c }
                };
                items.select_nth_unstable_by(prefix - 1, &mut cmp);
                let mut rest = items.split_off(prefix);
                let mut ranks: Vec<i128> = items.iter().map(rank).collect();
                let (_, &mut top, _) = ranks.select_nth_unstable_by(bound - 1, |a, b| b.cmp(a));
                rest.retain(|item| rank(item) > top);
                items.sort_unstable_by(&mut cmp);
                rest.sort_unstable_by(&mut cmp);
                items.append(&mut rest);
                return items;
            }
        }
    }
    items.sort_unstable_by(cmp);
    items
}

/// The place of the code of each group in the order of the bytes of the codes of the groups, from a sort of each code once. Codes with the same bytes have the same place. A null group, for which `null` is true, has the place 0.
fn code_places(
    dict: &Dictionary,
    key: impl Iterator<Item = u64>,
    null: impl Fn(usize) -> bool,
) -> Vec<u64> {
    let codes: Vec<u32> =
        key.enumerate().map(|(g, w)| if null(g) { 0 } else { w as u32 }).collect();
    let mut distinct: Vec<u32> = codes.iter().copied().filter(|&c| c != 0).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let mut by_bytes: Vec<u32> = (0..distinct.len() as u32).collect();
    by_bytes.sort_unstable_by(|&a, &b| {
        dict.get(distinct[a as usize]).cmp(dict.get(distinct[b as usize]))
    });
    let mut ranks = vec![0_u64; distinct.len()];
    let mut rank = 0;
    for (n, &at) in by_bytes.iter().enumerate() {
        if n == 0 || dict.get(distinct[at as usize]) != dict.get(distinct[by_bytes[n - 1] as usize])
        {
            rank += 1;
        }
        ranks[at as usize] = rank;
    }
    codes
        .iter()
        .map(|&c| if c == 0 { 0 } else { distinct.binary_search(&c).map_or(0, |at| ranks[at]) })
        .collect()
}

/// The order of the groups, as the row engine gives them: the order of the hash table of the `HashAggregate` of the plan, the order of the keys for a sort, or the order of the first rows. `hashes` has the hash of each group when the scan found them.
fn group_order(
    query: &Query,
    session: &dyn Session,
    group_keys: &GroupKeys<'_>,
    hashes: Option<&[u32]>,
    groups: usize,
    trailing: bool,
    want: Want<'_>,
) -> Result<Vec<usize>> {
    let first: Vec<usize> = (0..groups).collect();
    if query.group.is_empty() {
        return Ok(first);
    }
    if let Some(hashes) = hashes
        && let Some(table) = session.hash_tables(query).and_then(|t| t.group.clone())
    {
        return hashed::order_of(hashes, table.groups, trailing);
    }
    let width = query.group.len();
    let types: Vec<u32> = query.group.iter().map(|g| query.targets[g.target].expr.ty).collect();
    if let GroupKeys::Words { words, shapes, dicts } = group_keys
        && session.hash_tables(query).and_then(|t| t.group.clone()).is_none()
        && query.group.iter().all(|g| g.sort != 0)
    {
        let list: Vec<rupg_analyze::SortGroup> = query
            .group
            .iter()
            .enumerate()
            .map(|(i, g)| rupg_analyze::SortGroup { target: i, ..g.clone() })
            .collect();
        if let Some(natives) = sort::Keys::new(&list, &types, session)?.natives()
            && let Some(order) =
                word_order(session.tasks(), words, shapes, dicts, &natives, groups, want)
        {
            return Ok(order);
        }
    }
    let materialized;
    let key_values = match group_keys {
        GroupKeys::Values(values) => *values,
        GroupKeys::Words { words, shapes, dicts } => {
            materialized = values_of_words(words, shapes, dicts);
            &materialized
        }
    };
    let keys: Vec<&[Value]> = key_values.chunks(width).collect();
    if let Some(table) = session.hash_tables(query).and_then(|t| t.group.clone()) {
        let places: Option<Vec<usize>> =
            table.keys.iter().map(|t| query.group.iter().position(|g| g.target == *t)).collect();
        if let Some(places) = places {
            // The hash of each group, from the values of its keys in its first row.
            let mut hashes = Vec::with_capacity(groups);
            let mut hashed = true;
            for key in &keys {
                match hashed::hash(places.iter().map(|&p| (types[p], &key[p]))) {
                    Ok(h) => hashes.push(h),
                    // A key that the hash of the plan cannot read: the groups go in the order of a sort, as before the hash.
                    Err(_) if hashes.is_empty() => {
                        hashed = false;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            if hashed {
                return hashed::order_of(&hashes, table.groups, trailing);
            }
        }
    }
    let list: Vec<rupg_analyze::SortGroup> = query
        .group
        .iter()
        .enumerate()
        .map(|(i, g)| rupg_analyze::SortGroup { target: i, ..g.clone() })
        .collect();
    if list.iter().any(|g| g.sort == 0) {
        return Ok(first);
    }
    let keys_of = sort::Keys::new(&list, &types, session)?;
    let mut order = first;
    sort::qsort(&mut order, &mut |a, b| Ok(keys_of.compare(keys[*a], keys[*b])?.then(a.cmp(b))))?;
    Ok(order)
}

#[cfg(test)]
mod tests {
    use std::hash::BuildHasher;

    use super::*;

    /// Floats whose low bits are zero go into many buckets of a small table.
    /// A packed sort gives the order of a sort of the keys one after another, with equal keys in the order of their groups.
    #[test]
    fn a_packed_order_is_the_order_of_the_keys() {
        let mut seed = 7_u64;
        for round in 0..200_u32 {
            let keys = 3 + round as usize % 3;
            let groups = 1 + (round as usize * 13) % 400;
            let range = [2_u128, 50, 1 << 20, 1 << 40][round as usize % 4];
            let numbers: Vec<u128> = (0..groups * keys)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    u128::from(seed >> 20) % range + (1 << 64)
                })
                .collect();
            let number = |g: usize, k: usize| numbers[g * keys + k];
            let mut sorted: Vec<usize> = (0..groups).collect();
            sorted.sort_by(|&a, &b| {
                numbers[a * keys..(a + 1) * keys]
                    .cmp(&numbers[b * keys..(b + 1) * keys])
                    .then(a.cmp(&b))
            });
            match packed_order(None, groups, keys, Want::All, number) {
                Some(order) => assert_eq!(order, sorted, "round {round}"),
                None => assert_eq!(range, 1 << 40, "round {round}"),
            }
            // With a limit, the order is the first groups of the order of all.
            let limit = round as usize % 7 * groups / 5;
            if let Some(order) = packed_order(None, groups, keys, Want::First(limit), number) {
                assert_eq!(order, sorted[..limit.min(groups)], "round {round} limit {limit}");
            }
        }
    }

    /// A packed sort on tasks gives the order of a packed sort on the calling task, also for many groups with equal keys.
    #[test]
    fn a_packed_order_on_tasks_is_the_order_on_one_task() {
        let tasks = rupg_platform::os::OsTasks;
        let groups = 5 * ORDER_GROUPS + 77;
        let numbers: Vec<u128> = (0..groups * 3)
            .map(|i| u128::from((i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 50) % 900)
            .collect();
        let number = |g: usize, k: usize| numbers[g * 3 + k];
        let one = packed_order(None, groups, 3, Want::All, number).unwrap();
        let many = packed_order(Some(&tasks), groups, 3, Want::All, number).unwrap();
        assert_eq!(one, many);
        let first = packed_order(Some(&tasks), groups, 3, Want::First(1000), number).unwrap();
        assert_eq!(first, one[..1000]);
    }

    /// The heap of a top of counts keeps the same groups in the same order from the order of [`Want::Top`] as from the order of all the groups, also with many equal counts, for both directions.
    #[test]
    fn a_top_order_gives_the_heap_of_all_the_groups() {
        let mut seed = 11_u64;
        for round in 0..300_u32 {
            let groups = 50 + (round as usize * 977) % 9000;
            let bound = 1 + round as usize % 12;
            let descending = round % 2 == 0;
            let spread = [2_u64, 5, 40, 1000][round as usize % 4];
            let mut next = || {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                seed >> 20
            };
            let counts: Vec<i64> =
                (0..groups).map(|_| i64::try_from(next() % spread).unwrap() + 1).collect();
            let numbers: Vec<u128> = (0..groups).map(|_| u128::from(next() % 100_000)).collect();
            let number = |g: usize, _: usize| numbers[g];
            let heap = |order: Vec<usize>| {
                let mut cmp = |a: &(i64, usize), b: &(i64, usize)| {
                    Ok(if descending { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) })
                };
                let mut heap = sort::Bounded::new(bound);
                for g in order {
                    heap.push((counts[g], g), &mut cmp).unwrap();
                }
                heap.finish(&mut cmp).unwrap()
            };
            let all = packed_order(None, groups, 1, Want::All, number).unwrap();
            let want = Want::Top { counts: &counts, descending, bound };
            let top = packed_order(None, groups, 1, want, number).unwrap();
            assert!(top.len() <= all.len(), "round {round}");
            assert_eq!(heap(top), heap(all), "round {round}");
        }
    }

    /// A table gives each key one group, in the order of the first row of the key, as it grows and makes room. Keys differ in each word, also only in the last.
    #[test]
    fn a_word_table_gives_each_key_one_group() {
        let mut table = WordTable::<3>::new(4);
        let mut known: std::collections::HashMap<[u64; 3], u32> = std::collections::HashMap::new();
        let mut seed = 3_u64;
        for row in 0..60_000_u32 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let n = (seed >> 33) % 9000;
            let key = [n / 3, n % 3, u64::from(row % 5 == 0)];
            if row == 30_000 {
                table.reserve(50_000);
            }
            let next = u32::try_from(known.len()).unwrap();
            let hash = WordTable::<3>::hash(&key);
            table.prefetch(hash);
            let (id, new) = table.id(&key, hash, next);
            match known.get(&key) {
                Some(&old) => assert_eq!((id, new), (old, false), "row {row}"),
                None => {
                    assert_eq!((id, new), (next, true), "row {row}");
                    known.insert(key, id);
                }
            }
        }
        assert_eq!(table.len, known.len());
    }

    #[test]
    fn words_with_zero_low_bits_spread_over_the_buckets() {
        let build = rupg_kernels::hash::Build;
        let buckets: FxSet<u64> =
            (0..1000).map(|i| build.hash_one((f64::from(i) / 4.0).to_bits()) & 1023).collect();
        assert!(buckets.len() > 500, "{} buckets", buckets.len());
    }
}

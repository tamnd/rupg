// Lifted from rudb crates/rudb-qc/src/finish.rs at cd9f9676 (2026-10-05).
// Changes: the module `finish` of `crate::drive`, which turns what compiled code wrote into rows of rupg's `Value`s rather than into chunks, by the PostgreSQL type of each column, and makes a source column of a batch out of values the same way. The accumulators are finished with PostgreSQL's types: a `sum` of an `int2` or `int4` is an `int8` that raises `bigint out of range` when the total does not fit, a `sum` of an `int8` is a `numeric`, and an `avg` of an integer is the `numeric` the exact total divided by the count with the scale of `numeric_div`, which is what `int8_avg` and `numeric_avg` give. A sort orders values as PostgreSQL's btree operators of the types the driver reads do, with a NaN above every other float and strings by their bytes, which are the collations the generator takes, and keeps equal rows in the order they came. `counted`, `counted_top`, `Rising`, `fetch` and the top N of the counts are not lifted yet, as nothing cuts a top N from the group rows yet, and nor are the decimals of the first engine.

//! Turning what the compiled code left behind into rows, and the stages between pipelines.
//!
//! Everything compiled code writes is a value at the width of its QIR type, and a string is a `str16`. [`value`] reads one of those as the PostgreSQL value of its column's type, which is the one place the driver knows how a type is laid out, and [`Encoded`] goes the other way for the columns a pipeline reads. The group rows of a hash aggregate are read the same way: each key is a value with a null byte after it, and each accumulator is finished into a value by the rule its [`AccOp`] names.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Numeric, Value, oid};

use crate::codegen::{AccOp, Grouping, qir_type};
use crate::ir::Ty;
use crate::plan::{Column, Key, Kind, Type};
use crate::rt::join::JoinLayout;
use crate::rt::table::{Distinct, GroupTable};
use crate::rt::{Rt, text};

/// Whether the driver reads and writes values of `ty` as compiled code holds them, which is what [`value`] and [`Encoded::push`] take.
///
/// The fixed width types whose rupg value is the machine value, and the strings. The 64 bit types `money`, `xid8` and `pg_lsn` and the `uuid` are left to the vector executor for now.
#[must_use]
pub fn readable(ty: Type) -> bool {
    matches!(
        ty.oid,
        oid::BOOL
            | oid::CHAR
            | oid::INT2
            | oid::INT4
            | oid::OID
            | oid::REGPROC
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::XID
            | oid::CID
            | oid::DATE
            | oid::INT8
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::TEXT
            | oid::VARCHAR
            | oid::BPCHAR
            | oid::NAME
            | oid::BYTEA
    )
}

/// The value of type `ty` that compiled code wrote as the bytes `b`, at the width of its QIR type.
///
/// A string is a `str16`, whose bytes are copied, so the value owns them.
///
/// # Errors
///
/// For a type that [`readable`] does not take, and for text that is not UTF-8.
pub fn value(ty: Type, b: &[u8]) -> Result<Value> {
    let take = |n: usize| -> Result<&[u8]> {
        b.get(..n).ok_or_else(|| Error::internal(format!("a value of {ty} in {} bytes", b.len())))
    };
    let i16_of = |s: &[u8]| i16::from_le_bytes([s[0], s[1]]);
    let u32_of = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let u64_of = |s: &[u8]| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&s[..8]);
        u64::from_le_bytes(w)
    };
    Ok(match ty.oid {
        oid::BOOL => Value::Bool(take(1)?[0] & 1 != 0),
        oid::CHAR => Value::Char(take(1)?[0]),
        oid::INT2 => Value::Int2(i16_of(take(2)?)),
        oid::INT4 => Value::Int4(u32_of(take(4)?) as i32),
        oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE | oid::XID | oid::CID => {
            Value::Oid(u32_of(take(4)?))
        }
        oid::DATE => Value::Date(u32_of(take(4)?) as i32),
        oid::INT8 => Value::Int8(u64_of(take(8)?) as i64),
        oid::TIME => Value::Time(u64_of(take(8)?) as i64),
        oid::TIMESTAMP => Value::Timestamp(u64_of(take(8)?) as i64),
        oid::TIMESTAMPTZ => Value::TimestampTz(u64_of(take(8)?) as i64),
        oid::FLOAT4 => Value::Float4(f32::from_bits(u32_of(take(4)?))),
        oid::FLOAT8 => Value::Float8(f64::from_bits(u64_of(take(8)?))),
        oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME | oid::BYTEA => {
            let mut w = [0u8; 16];
            w.copy_from_slice(take(16)?);
            let header = u128::from_le_bytes(w);
            // SAFETY: compiled code writes a string as a header made by `text::make` over a
            // column of the batch the driver holds for the call, or over the runtime's heap,
            // which the query's runtime keeps, and the bytes are copied before either goes.
            let bytes = unsafe { text::bytes(&header) }.to_vec();
            if ty.oid == oid::BYTEA {
                Value::Bytea(bytes)
            } else {
                Value::Text(
                    String::from_utf8(bytes).map_err(|_| {
                        Error::internal(format!("a value of {ty} that is not UTF-8"))
                    })?,
                )
            }
        }
        _ => return Err(Error::internal(format!("the driver reads no value of {ty}"))),
    })
}

/// A source column of a batch as compiled code reads it, built a value at a time.
///
/// The values are at the width of the column's QIR type in a buffer of `u128`s, so that every load is aligned, a string as a `str16` whose long bytes are in `bytes`, and the validity is a bitmap with the least significant bit first.
#[derive(Debug, Default)]
pub struct Encoded {
    /// The values, at their width.
    pub(crate) values: Vec<u128>,
    /// The bytes written so far.
    len: usize,
    /// The validity bitmap.
    pub(crate) valid: Vec<u8>,
    /// How many values there are.
    pub(crate) rows: usize,
    /// Whether no value is null.
    pub(crate) clean: bool,
    /// Whether the values are `str16` headers, which the body reads through codes.
    pub(crate) text: bool,
    /// The long strings, which each header past [`text::INLINE`] bytes points into.
    strings: Vec<Box<[u8]>>,
}

impl Encoded {
    /// An empty column of `ty`, which has a QIR type.
    ///
    /// # Errors
    ///
    /// For a type that [`readable`] does not take.
    pub fn new(ty: Type) -> Result<Encoded> {
        if !readable(ty) {
            return Err(Error::internal(format!("the driver reads no column of {ty}")));
        }
        Ok(Encoded { clean: true, text: ty.data() == Some(Ty::Str16), ..Encoded::default() })
    }

    /// The column of `ty` with `values`.
    ///
    /// # Errors
    ///
    /// For a type that [`readable`] does not take, or a value that is not of it.
    pub fn of(ty: Type, values: &[Value]) -> Result<Encoded> {
        let mut c = Encoded::new(ty)?;
        for v in values {
            c.push(ty, v)?;
        }
        Ok(c)
    }

    /// Adds `v`, a value of `ty` or a null.
    ///
    /// # Errors
    ///
    /// For a value that is not of `ty`.
    pub fn push(&mut self, ty: Type, v: &Value) -> Result<()> {
        let at = self.rows;
        self.rows += 1;
        if self.valid.len() < self.rows.div_ceil(8) {
            self.valid.push(0);
        }
        let mut w = [0u8; 16];
        let width = match (ty.oid, v) {
            (_, Value::Null) => {
                self.clean = false;
                ty.data().map_or(1, Ty::bytes) as usize
            }
            (oid::BOOL, Value::Bool(b)) => {
                w[0] = u8::from(*b);
                1
            }
            (oid::CHAR, Value::Char(c)) => {
                w[0] = *c;
                1
            }
            (oid::INT2, Value::Int2(x)) => put(&mut w, &x.to_le_bytes()),
            (oid::INT4, Value::Int4(x)) | (oid::DATE, Value::Date(x)) => {
                put(&mut w, &x.to_le_bytes())
            }
            (
                oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE | oid::XID | oid::CID,
                Value::Oid(x),
            ) => put(&mut w, &x.to_le_bytes()),
            (oid::INT8, Value::Int8(x))
            | (oid::TIME, Value::Time(x))
            | (oid::TIMESTAMP, Value::Timestamp(x))
            | (oid::TIMESTAMPTZ, Value::TimestampTz(x)) => put(&mut w, &x.to_le_bytes()),
            (oid::FLOAT4, Value::Float4(x)) => put(&mut w, &x.to_le_bytes()),
            (oid::FLOAT8, Value::Float8(x)) => put(&mut w, &x.to_le_bytes()),
            (oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME, Value::Text(s)) => {
                self.string(s.as_bytes(), &mut w)
            }
            (oid::BYTEA, Value::Bytea(b)) => self.string(b, &mut w),
            _ => return Err(Error::internal(format!("a value {v:?} in a column of {ty}"))),
        };
        if !matches!(v, Value::Null) {
            self.valid[at / 8] |= 1 << (at % 8);
        }
        let end = self.len + width;
        if self.values.len() * 16 < end {
            self.values.push(0);
        }
        // SAFETY: the buffer is `u128`s, any byte pattern is one, and it holds `end` bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                self.values.as_mut_ptr().cast::<u8>(),
                self.values.len() * 16,
            )
        };
        bytes[self.len..end].copy_from_slice(&w[..width]);
        self.len = end;
        Ok(())
    }

    /// The `str16` of `b` in `w`, its long bytes kept in the column, and its width.
    fn string(&mut self, b: &[u8], w: &mut [u8; 16]) -> usize {
        let header = if b.len() <= text::INLINE {
            text::make(b)
        } else {
            // A boxed slice does not move when the column grows, so the header stays good.
            self.strings.push(b.into());
            self.strings.last().map_or(0, |s| text::make(s))
        };
        w.copy_from_slice(&header.to_le_bytes());
        16
    }

    /// The values as bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: the buffer is `u128`s, which are bytes with no padding.
        unsafe { std::slice::from_raw_parts(self.values.as_ptr().cast::<u8>(), self.len) }
    }

    /// Whether row `i` is valid.
    pub(crate) fn is_valid(&self, i: usize) -> bool {
        self.valid[i / 8] & (1 << (i % 8)) != 0
    }

    /// The `str16` of row `i` of a text column.
    pub(crate) fn header(&self, i: usize) -> u128 {
        let mut w = [0u8; 16];
        w.copy_from_slice(&self.bytes()[16 * i..16 * (i + 1)]);
        u128::from_le_bytes(w)
    }
}

/// Copies `b` to the front of `w` and gives its length.
fn put(w: &mut [u8; 16], b: &[u8]) -> usize {
    w[..b.len()].copy_from_slice(b);
    b.len()
}

/// The rows of a join table that a semi or anti join whose left side builds keeps, as values of `columns`.
pub(crate) fn kept(
    rows: &[&[u8]],
    layout: &JoinLayout,
    columns: &[Column],
) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let mut row = Vec::with_capacity(columns.len());
        for (f, c) in layout.payload.iter().zip(columns) {
            let (at, width) = (f.offset as usize, f.width as usize);
            row.push(if r[f.null() as usize] != 0 {
                Value::Null
            } else {
                value(c.ty, &r[at..at + width])?
            });
        }
        out.push(row);
    }
    Ok(out)
}

/// The distinct sets of a hash aggregate, by handle.
pub(crate) fn sets<'r>(rt: &'r Rt, g: &Grouping) -> Result<Vec<(u64, &'r Distinct)>> {
    g.accs
        .iter()
        .filter_map(|acc| match acc.op {
            AccOp::Distinct(h, _) => Some(
                rt.distinct(h)
                    .map(|d| (h, d))
                    .ok_or_else(|| Error::internal("a distinct set is gone")),
            ),
            _ => None,
        })
        .collect()
}

/// The groups of a hash aggregate's table as rows, the keys and then the finished accumulators. `sets` are its distinct sets by handle.
pub(crate) fn groups(
    table: &GroupTable,
    sets: &[(u64, &Distinct)],
    g: &Grouping,
) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::with_capacity(table.len());
    for gid in 0..table.len() {
        let row = table.row(gid);
        let mut values = Vec::with_capacity(g.keys.len() + g.accs.len());
        for (k, ty) in &g.keys {
            let at = 8 + k.offset as usize;
            values.push(if row[8 + k.null() as usize] != 0 {
                Value::Null
            } else {
                value(*ty, &row[at..at + k.width as usize])?
            });
        }
        for acc in &g.accs {
            let at = (g.acc_offset + acc.offset) as usize;
            values.push(finish(sets, acc.op, (acc.arg, acc.ty), &row[at..], gid)?);
        }
        out.push(values);
    }
    Ok(out)
}

/// The sum a split accumulator holds: its `i128` total and the `i64` part not yet in it.
fn split(a: &[u8]) -> i128 {
    i128_at(a, 0).wrapping_add(i128::from(i64_at(a, 16)))
}

/// A whole sum as a value of `ty`: an `int8`, which is the `sum` of an `int2` or an `int4` and raises PostgreSQL's error when the total does not fit, or a `numeric`, the `sum` of an `int8`.
fn whole(total: i128, ty: Type) -> Result<Value> {
    match ty.oid {
        oid::INT8 => i64::try_from(total)
            .map(Value::Int8)
            .map_err(|_| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")),
        oid::NUMERIC => Ok(Value::Numeric(Numeric::from_integer(total))),
        _ => Err(Error::internal(format!("a sum of {ty}"))),
    }
}

/// The `avg` of `n` integers whose total is `total`: the `numeric` total divided by the `numeric` count, as `int8_avg` and `numeric_avg` divide them, with the scale `numeric_div` picks. A null for no values.
fn mean(total: i128, n: i64) -> Result<Value> {
    if n == 0 {
        return Ok(Value::Null);
    }
    let q = Numeric::from_integer(total).div(&Numeric::from_integer(i128::from(n)))?;
    Ok(Value::Numeric(q))
}

/// The value of one accumulator over values of `arg` into the type `ty`, `a` being its bytes in the group row.
fn finish(
    sets: &[(u64, &Distinct)],
    op: AccOp,
    (arg, ty): (Type, Type),
    a: &[u8],
    gid: usize,
) -> Result<Value> {
    Ok(match op {
        AccOp::CountStar | AccOp::Count => Value::Int8(i64_at(a, 0)),
        AccOp::SumSplit if a[24] != 0 => whole(split(a), ty)?,
        AccOp::SumNarrow if a[8] != 0 => whole(i128::from(i64_at(a, 0)), ty)?,
        AccOp::SumSplit | AccOp::SumNarrow => Value::Null,
        AccOp::AvgSplit => mean(split(a), i64_at(a, 24))?,
        AccOp::AvgNarrow => mean(i128::from(i64_at(a, 0)), i64_at(a, 8))?,
        AccOp::SumFloat if a[8] != 0 => Value::Float8(f64::from_bits(i64_at(a, 0) as u64)),
        AccOp::SumFloat => Value::Null,
        AccOp::Min | AccOp::Max | AccOp::AnyValue => {
            let w = qir_type(arg).map_err(|r| Error::internal(r.to_string()))?.bytes() as usize;
            if a[w] != 0 { value(ty, &a[..w])? } else { Value::Null }
        }
        AccOp::MinStr | AccOp::MaxStr if a[16] != 0 => value(ty, &a[..16])?,
        AccOp::MinStr | AccOp::MaxStr => Value::Null,
        AccOp::Distinct(h, _) => {
            let d = sets
                .iter()
                .find_map(|&(at, d)| (at == h).then_some(d))
                .ok_or_else(|| Error::internal("a distinct set is gone"))?;
            Value::Int8(i64::try_from(d.count(gid)).unwrap_or(i64::MAX))
        }
    })
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(w)
}

fn i128_at(b: &[u8], at: usize) -> i128 {
    let mut w = [0u8; 16];
    w.copy_from_slice(&b[at..at + 16]);
    i128::from_le_bytes(w)
}

/// `rows` in the order of `keys`, then `offset` of them skipped and `count` kept. Equal rows keep the order they came in.
///
/// # Errors
///
/// For a key that is not a column, which [`super::check`] refuses first.
pub(crate) fn sort(
    mut rows: Vec<Vec<Value>>,
    keys: &[Key],
    count: Option<u64>,
    offset: u64,
) -> Result<Vec<Vec<Value>>> {
    let mut by = Vec::with_capacity(keys.len());
    for k in keys {
        let Kind::Column(c) = k.expr.kind else {
            return Err(Error::internal("a sort key that is not a column"));
        };
        by.push((c, k.descending, k.nulls_first));
    }
    rows.sort_by(|a, b| {
        for &(c, descending, nulls_first) in &by {
            let o = match (&a[c], &b[c]) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (_, Value::Null) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (x, y) => {
                    let o = order(x, y);
                    if descending { o.reverse() } else { o }
                }
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    Ok(limit(rows, count, offset))
}

/// `offset` of `rows` skipped and then `count` of them kept, all of them for `None`.
pub(crate) fn limit(rows: Vec<Vec<Value>>, count: Option<u64>, offset: u64) -> Vec<Vec<Value>> {
    let skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = count.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    rows.into_iter().skip(skip).take(take).collect()
}

/// The order of two values of one type that are not null, as the btree operators of PostgreSQL order them.
///
/// A float NaN is above every other float and equal to another NaN, and `-0` equals `0`. Strings are ordered by their bytes, as the collations the generator takes order them.
pub(crate) fn order(a: &Value, b: &Value) -> Ordering {
    fn float(x: f64, y: f64) -> Ordering {
        match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        }
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Char(x), Value::Char(y)) => x.cmp(y),
        (Value::Int2(x), Value::Int2(y)) => x.cmp(y),
        (Value::Int4(x), Value::Int4(y)) | (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Oid(x), Value::Oid(y)) => x.cmp(y),
        (Value::Int8(x), Value::Int8(y))
        | (Value::Time(x), Value::Time(y))
        | (Value::Timestamp(x), Value::Timestamp(y))
        | (Value::TimestampTz(x), Value::TimestampTz(y)) => x.cmp(y),
        (Value::Float4(x), Value::Float4(y)) => float(f64::from(*x), f64::from(*y)),
        (Value::Float8(x), Value::Float8(y)) => float(*x, *y),
        (Value::Numeric(x), Value::Numeric(y)) => x.compare(y),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

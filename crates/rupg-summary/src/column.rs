//! The summary of one column of one segment (spec/09 section 9.7.1), its builders, and its bytes.

use rupg_common::{Error, Result};

use crate::codes::CodeSet;
use crate::key;
use crate::zone::Zone;

/// The rows of a vector (spec/04, spec/09 section 9.2).
pub const VECTOR: usize = 1024;

/// A bound of a segment column, with the tag of rudb `storage-v3/05`.
#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    /// An integer of any width, a scaled `numeric`, or a dictionary code rank.
    Int(i128),
    /// A `real` or a `double precision`.
    Float(f64),
    /// A string or bytes, in full.
    Bytes(Vec<u8>),
}

/// The sum of a segment column under the rules of spec/09 section 9.7.2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sum {
    /// The column has no sum, for example a string column.
    None,
    /// The exact sum of integers or of scaled integers.
    Int(i128),
    /// The sum of floats, added in row order.
    Float(f64),
    /// The sum left the range of `i128`. The summary cannot answer `sum`, and the reader scans.
    Overflow,
}

/// The special values of a float column.
pub mod special {
    /// The column has a NaN.
    pub const NAN: u8 = 1;
    /// The column has positive infinity.
    pub const POS_INF: u8 = 2;
    /// The column has negative infinity.
    pub const NEG_INF: u8 = 4;
}

/// The summary of one column of one segment.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSummary {
    /// All rows of the segment, including deleted rows.
    pub rows: u32,
    /// The null rows.
    pub nulls: u32,
    /// The lowest non-null value, or `None` when every row is null.
    pub min: Option<Bound>,
    /// The highest non-null value, or `None` when every row is null.
    pub max: Option<Bound>,
    /// The sum of the non-null values.
    pub sum: Sum,
    /// The flags of [`special`].
    pub special: u8,
    /// For a column with a dictionary, the number of codes present.
    pub distinct_codes: Option<u32>,
    /// For a column with a dictionary, the codes present, when the writer stores them (spec/09 section 9.7.3).
    pub code_set: Option<CodeSet>,
    /// The zone of each vector, in order.
    pub vectors: Vec<Zone>,
}

impl ColumnSummary {
    /// The zone of the whole segment.
    #[must_use]
    pub fn zone(&self) -> Zone {
        let mut zone = Zone::EMPTY;
        for vector in &self.vectors {
            zone.merge(vector);
        }
        zone
    }

    /// The non-null rows, the answer of `count(x)`.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.rows - self.nulls
    }
}

/// The builder of the summary of an integer column: `smallint`, `integer`, `bigint`, `date`, `timestamp`, `money` and the others of spec/09 section 9.8 that are integers.
#[derive(Clone, Debug, Default)]
pub struct IntBuilder {
    rows: u32,
    nulls: u32,
    min: i64,
    max: i64,
    sum: i128,
    vectors: Vec<Zone>,
}

impl IntBuilder {
    /// A builder with no rows.
    #[must_use]
    pub fn new() -> IntBuilder {
        IntBuilder { min: i64::MAX, max: i64::MIN, ..IntBuilder::default() }
    }

    /// Adds one vector of at most [`VECTOR`] values. `valid` is `None` when every value is valid, else one flag for each value. The value at a null position is not read.
    ///
    /// # Panics
    ///
    /// When the vector has more than [`VECTOR`] values or `valid` has another length.
    pub fn push(&mut self, values: &[i64], valid: Option<&[bool]>) {
        assert!(values.len() <= VECTOR, "a vector has at most 1024 values");
        let (mut lo, mut hi, mut sum, mut nulls) = (i64::MAX, i64::MIN, 0i128, 0u32);
        match valid {
            None => {
                for &v in values {
                    lo = lo.min(v);
                    hi = hi.max(v);
                    sum += i128::from(v);
                }
            }
            Some(valid) => {
                assert_eq!(valid.len(), values.len());
                for (&v, &ok) in values.iter().zip(valid) {
                    if ok {
                        lo = lo.min(v);
                        hi = hi.max(v);
                        sum += i128::from(v);
                    } else {
                        nulls += 1;
                    }
                }
            }
        }
        let rows = values.len() as u32;
        let zone = if nulls == rows {
            Zone { nulls, rows, ..Zone::ints(0, 0, 0, 0) }
        } else {
            Zone::ints(lo, hi, rows, nulls)
        };
        self.vectors.push(zone);
        self.rows += rows;
        self.nulls += nulls;
        if nulls < rows {
            self.min = self.min.min(lo);
            self.max = self.max.max(hi);
        }
        // A segment of 262,144 values of `bigint` sums to at most 2 to the power 81, so i128 never overflows here.
        self.sum += sum;
    }

    /// The summary.
    #[must_use]
    pub fn finish(self) -> ColumnSummary {
        let any = self.nulls < self.rows;
        ColumnSummary {
            rows: self.rows,
            nulls: self.nulls,
            min: any.then_some(Bound::Int(i128::from(self.min))),
            max: any.then_some(Bound::Int(i128::from(self.max))),
            sum: Sum::Int(self.sum),
            special: 0,
            distinct_codes: None,
            code_set: None,
            vectors: self.vectors,
        }
    }
}

/// The builder of the summary of a `real` or `double precision` column. The bounds follow the order of PostgreSQL, where NaN is above every number (spec/09 section 9.7.2).
#[derive(Clone, Debug, Default)]
pub struct FloatBuilder {
    rows: u32,
    nulls: u32,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
    special: u8,
    vectors: Vec<Zone>,
}

impl FloatBuilder {
    /// A builder with no rows.
    #[must_use]
    pub fn new() -> FloatBuilder {
        FloatBuilder::default()
    }

    /// Adds one vector, as [`IntBuilder::push`]. A `real` column passes its values widened to `f64`, which is exact.
    ///
    /// # Panics
    ///
    /// As [`IntBuilder::push`].
    pub fn push(&mut self, values: &[f64], valid: Option<&[bool]>) {
        assert!(values.len() <= VECTOR, "a vector has at most 1024 values");
        if let Some(valid) = valid {
            assert_eq!(valid.len(), values.len());
        }
        let (mut lo, mut hi, mut nulls) = (u64::MAX, 0u64, 0u32);
        for (index, &v) in values.iter().enumerate() {
            if valid.is_some_and(|valid| !valid[index]) {
                nulls += 1;
                continue;
            }
            let k = key::float(v);
            lo = lo.min(k);
            hi = hi.max(k);
            if v.is_nan() {
                self.special |= special::NAN;
            } else if v == f64::INFINITY {
                self.special |= special::POS_INF;
            } else if v == f64::NEG_INFINITY {
                self.special |= special::NEG_INF;
            }
            if self.min.is_none_or(|m| key::float(m) > k) {
                self.min = Some(v);
            }
            if self.max.is_none_or(|m| key::float(m) < k) {
                self.max = Some(v);
            }
            self.sum += v;
        }
        let rows = values.len() as u32;
        self.vectors.push(Zone { min: lo, max: hi, rows, nulls, exact: true, tail: false });
        self.rows += rows;
        self.nulls += nulls;
    }

    /// The summary.
    #[must_use]
    pub fn finish(self) -> ColumnSummary {
        ColumnSummary {
            rows: self.rows,
            nulls: self.nulls,
            min: self.min.map(Bound::Float),
            max: self.max.map(Bound::Float),
            sum: Sum::Float(self.sum),
            special: self.special,
            distinct_codes: None,
            code_set: None,
            vectors: self.vectors,
        }
    }
}

/// The builder of the summary of a string or bytes column without a dictionary. The bounds are in byte order, the order of the `C` collation, and the vector keys are the first 8 bytes (spec/09 section 9.7.1).
#[derive(Clone, Debug, Default)]
pub struct BytesBuilder {
    rows: u32,
    nulls: u32,
    min: Option<Vec<u8>>,
    max: Option<Vec<u8>>,
    vectors: Vec<Zone>,
}

impl BytesBuilder {
    /// A builder with no rows.
    #[must_use]
    pub fn new() -> BytesBuilder {
        BytesBuilder::default()
    }

    /// Adds one vector. A `None` value is null.
    ///
    /// # Panics
    ///
    /// When the vector has more than [`VECTOR`] values.
    pub fn push<'a>(&mut self, values: impl ExactSizeIterator<Item = Option<&'a [u8]>>) {
        assert!(values.len() <= VECTOR, "a vector has at most 1024 values");
        let rows = values.len() as u32;
        let (mut lo, mut hi, mut nulls): (Option<&[u8]>, Option<&[u8]>, u32) = (None, None, 0);
        for value in values {
            let Some(v) = value else {
                nulls += 1;
                continue;
            };
            if lo.is_none_or(|lo| v < lo) {
                lo = Some(v);
            }
            if hi.is_none_or(|hi| v > hi) {
                hi = Some(v);
            }
        }
        let zone = match (lo, hi) {
            (Some(lo), Some(hi)) => {
                if self.min.as_deref().is_none_or(|m| lo < m) {
                    self.min = Some(lo.to_vec());
                }
                if self.max.as_deref().is_none_or(|m| hi > m) {
                    self.max = Some(hi.to_vec());
                }
                Zone {
                    min: key::bytes(lo),
                    max: key::bytes(hi),
                    rows,
                    nulls,
                    exact: false,
                    tail: false,
                }
            }
            _ => Zone { rows, nulls, exact: false, ..Zone::EMPTY },
        };
        self.vectors.push(zone);
        self.rows += rows;
        self.nulls += nulls;
    }

    /// The summary.
    #[must_use]
    pub fn finish(self) -> ColumnSummary {
        ColumnSummary {
            rows: self.rows,
            nulls: self.nulls,
            min: self.min.map(Bound::Bytes),
            max: self.max.map(Bound::Bytes),
            sum: Sum::None,
            special: 0,
            distinct_codes: None,
            code_set: None,
            vectors: self.vectors,
        }
    }
}

/// The builder of the summary of a string column with a dictionary. The writer gives each code and its rank in the sorted base of the dictionary, or no rank for a code of the tail (spec/09 section 9.5.3).
#[derive(Clone, Debug, Default)]
pub struct CodeBuilder {
    rows: u32,
    nulls: u32,
    min: Option<u32>,
    max: Option<u32>,
    tail: bool,
    codes: Vec<u32>,
    /// The codes at the start of `codes` that are sorted and without duplicates.
    sorted: usize,
    vectors: Vec<Zone>,
}

impl CodeBuilder {
    /// A builder with no rows.
    #[must_use]
    pub fn new() -> CodeBuilder {
        CodeBuilder::default()
    }

    /// Adds one vector of codes. Code 0 is a null (spec/09 section 9.5.2). `rank` gives the rank of a base code and `None` for a tail code.
    ///
    /// # Panics
    ///
    /// When the vector has more than [`VECTOR`] values.
    pub fn push(&mut self, codes: &[u32], rank: impl Fn(u32) -> Option<u32>) {
        assert!(codes.len() <= VECTOR, "a vector has at most 1024 values");
        let rows = codes.len() as u32;
        let (mut lo, mut hi, mut nulls, mut tail) = (u32::MAX, 0u32, 0u32, false);
        for &code in codes {
            if code == 0 {
                nulls += 1;
                continue;
            }
            self.codes.push(code);
            match rank(code) {
                Some(r) => {
                    lo = lo.min(r);
                    hi = hi.max(r);
                }
                None => tail = true,
            }
        }
        let zone = if lo <= hi {
            Zone { tail, ..Zone::ints(i64::from(lo), i64::from(hi), rows, nulls) }
        } else {
            Zone { rows, nulls, tail, ..Zone::EMPTY }
        };
        if lo <= hi {
            self.min = Some(self.min.map_or(lo, |m| m.min(lo)));
            self.max = Some(self.max.map_or(hi, |m| m.max(hi)));
        }
        self.tail |= tail;
        self.vectors.push(zone);
        self.rows += rows;
        self.nulls += nulls;
        // The repeated codes go when the codes double, so that the codes of a long column with few distinct values take little memory.
        if self.codes.len() >= (2 * self.sorted).max(4 * VECTOR) {
            self.codes.sort_unstable();
            self.codes.dedup();
            self.sorted = self.codes.len();
        }
    }

    /// The summary. The code set is kept when its bytes are at most `limit`, which the writer sets to one eighth of the encoded code stream (spec/09 section 9.7.3). The bounds are ranks of the base, and they are absent when a code of the tail is present, because a tail code has no rank.
    #[must_use]
    pub fn finish(mut self, limit: usize) -> ColumnSummary {
        self.codes.sort_unstable();
        self.codes.dedup();
        let set = CodeSet::from_sorted(&self.codes);
        let bounded = !self.tail;
        ColumnSummary {
            rows: self.rows,
            nulls: self.nulls,
            min: self.min.filter(|_| bounded).map(|r| Bound::Int(i128::from(r))),
            max: self.max.filter(|_| bounded).map(|r| Bound::Int(i128::from(r))),
            sum: Sum::None,
            special: 0,
            distinct_codes: Some(self.codes.len() as u32),
            code_set: (set.encoded_len() <= limit).then_some(set),
            vectors: self.vectors,
        }
    }
}

/// The version of the bytes of [`ColumnSummary::write`].
const VERSION: u8 = 1;

fn put_bound(out: &mut Vec<u8>, bound: Option<&Bound>) {
    match bound {
        None => out.push(0),
        Some(Bound::Int(v)) => {
            out.push(1);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Some(Bound::Float(v)) => {
            out.push(2);
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Some(Bound::Bytes(v)) => {
            out.push(3);
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(v);
        }
    }
}

/// A checked cursor over the bytes of a summary.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len());
        let end =
            end.ok_or_else(|| Error::corrupted("a segment summary is shorter than its fields"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn i128(&mut self) -> Result<i128> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().expect("16 bytes")))
    }

    fn bound(&mut self) -> Result<Option<Bound>> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(Bound::Int(self.i128()?)),
            2 => Some(Bound::Float(f64::from_bits(self.u64()?))),
            3 => {
                let len = self.u32()? as usize;
                Some(Bound::Bytes(self.take(len)?.to_vec()))
            }
            tag => {
                return Err(Error::corrupted(format!(
                    "a segment summary has the unknown bound tag {tag}"
                )));
            }
        })
    }

    /// An integer array that `put_ints` wrote.
    fn ints(&mut self) -> Result<Vec<i64>> {
        let len = self.u32()? as usize;
        rupg_encode::integer::decode(self.take(len)?)
    }
}

/// Writes an integer array with the integer cascade of rupg-encode, because the bounds of neighbouring vectors are close (spec/09 section 9.7.1).
fn put_ints(out: &mut Vec<u8>, values: &[i64]) {
    let bytes = rupg_encode::integer::encode(values).expect("an integer array always encodes");
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
}

impl ColumnSummary {
    /// Writes the summary. The vector zones are five integer arrays: the minimum keys, the maximum keys, the null counts, the row counts and the tail flags, each with the integer cascade. A key is stored as the `i64` of [`key::int_of`], so that keys of integers are the integers.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.nulls.to_le_bytes());
        put_bound(out, self.min.as_ref());
        put_bound(out, self.max.as_ref());
        match self.sum {
            Sum::None => out.push(0),
            Sum::Int(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Sum::Float(v) => {
                out.push(2);
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            Sum::Overflow => out.push(3),
        }
        out.push(self.special);
        match self.distinct_codes {
            None => out.push(0),
            Some(n) => {
                out.push(1);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        match &self.code_set {
            None => out.push(0),
            Some(set) => {
                out.push(1);
                set.write(out);
            }
        }
        out.extend_from_slice(&(self.vectors.len() as u32).to_le_bytes());
        let exact = self.vectors.iter().all(|z| z.exact);
        let tails: Vec<i64> = self.vectors.iter().map(|z| i64::from(z.tail)).collect();
        out.push(u8::from(exact));
        put_ints(out, &self.vectors.iter().map(|z| key::int_of(z.min)).collect::<Vec<_>>());
        put_ints(out, &self.vectors.iter().map(|z| key::int_of(z.max)).collect::<Vec<_>>());
        put_ints(out, &self.vectors.iter().map(|z| i64::from(z.nulls)).collect::<Vec<_>>());
        put_ints(out, &self.vectors.iter().map(|z| i64::from(z.rows)).collect::<Vec<_>>());
        put_ints(out, &tails);
    }

    /// Reads a summary that [`ColumnSummary::write`] wrote, from the start of `bytes`, and returns it with the bytes it used.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a summary.
    pub fn read(bytes: &[u8]) -> Result<(ColumnSummary, usize)> {
        let mut c = Cursor { bytes, at: 0 };
        let version = c.u8()?;
        if version != VERSION {
            return Err(Error::corrupted(format!(
                "a segment summary has the unknown version {version}"
            )));
        }
        let rows = c.u32()?;
        let nulls = c.u32()?;
        let min = c.bound()?;
        let max = c.bound()?;
        let sum = match c.u8()? {
            0 => Sum::None,
            1 => Sum::Int(c.i128()?),
            2 => Sum::Float(f64::from_bits(c.u64()?)),
            3 => Sum::Overflow,
            tag => {
                return Err(Error::corrupted(format!(
                    "a segment summary has the unknown sum tag {tag}"
                )));
            }
        };
        let special = c.u8()?;
        let distinct_codes = match c.u8()? {
            0 => None,
            _ => Some(c.u32()?),
        };
        let code_set = match c.u8()? {
            0 => None,
            _ => {
                let (set, used) = CodeSet::read(&bytes[c.at..])?;
                c.at += used;
                Some(set)
            }
        };
        let count = c.u32()? as usize;
        let exact = c.u8()? != 0;
        let arrays = [c.ints()?, c.ints()?, c.ints()?, c.ints()?, c.ints()?];
        if arrays.iter().any(|a| a.len() != count) {
            return Err(Error::corrupted(
                "the vector zones of a segment summary have different lengths",
            ));
        }
        let mut vectors = Vec::with_capacity(count);
        let (mut total, mut total_nulls) = (0u64, 0u64);
        let [mins, maxes, zone_nulls, zone_rows, tails] = arrays;
        for ((((&lo, &hi), &znulls), &zrows), &tail) in
            mins.iter().zip(&maxes).zip(&zone_nulls).zip(&zone_rows).zip(&tails)
        {
            if !(0..=VECTOR as i64).contains(&zrows) || !(0..=zrows).contains(&znulls) {
                return Err(Error::corrupted("a vector zone of a segment summary has bad counts"));
            }
            total += zrows as u64;
            total_nulls += znulls as u64;
            vectors.push(Zone {
                min: key::int(lo),
                max: key::int(hi),
                rows: zrows as u32,
                nulls: znulls as u32,
                exact,
                tail: tail != 0,
            });
        }
        if total != u64::from(rows) || total_nulls != u64::from(nulls) || nulls > rows {
            return Err(Error::corrupted(
                "the vector zones of a segment summary do not add up to its counts",
            ));
        }
        Ok((
            ColumnSummary {
                rows,
                nulls,
                min,
                max,
                sum,
                special,
                distinct_codes,
                code_set,
                vectors,
            },
            c.at,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::{Test, Verdict};

    fn round_trip(summary: &ColumnSummary) {
        let mut bytes = Vec::new();
        summary.write(&mut bytes);
        let (back, used) = ColumnSummary::read(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        match (summary.sum, back.sum) {
            (Sum::Float(a), Sum::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => assert_eq!(&back, summary),
        }
        assert_eq!(back.vectors, summary.vectors);
        for len in 0..bytes.len() {
            assert!(ColumnSummary::read(&bytes[..len]).is_err(), "a prefix of {len} bytes reads");
        }
    }

    #[test]
    fn integer_summaries() {
        let mut builder = IntBuilder::new();
        let values: Vec<i64> = (0..1024).collect();
        builder.push(&values, None);
        let valid: Vec<bool> = (0..1024).map(|i| i % 2 == 0).collect();
        let values: Vec<i64> = (0..1024).map(|i| if i % 2 == 0 { -i } else { 1 << 40 }).collect();
        builder.push(&values, Some(&valid));
        builder.push(&[7; 10], Some(&[false; 10]));
        let summary = builder.finish();
        assert_eq!((summary.rows, summary.nulls, summary.count()), (2058, 522, 1536));
        assert_eq!(summary.min, Some(Bound::Int(-1022)));
        assert_eq!(summary.max, Some(Bound::Int(1023)));
        let expected: i128 = (0..1024).sum::<i128>() - (0..1024).step_by(2).sum::<i128>();
        assert_eq!(summary.sum, Sum::Int(expected));
        assert_eq!(summary.vectors[2].test(&Test::IsNull), Verdict::All);
        assert_eq!(summary.vectors[0].test(&Test::Gt(key::int(2000))), Verdict::None);
        assert_eq!(summary.zone().test(&Test::Ge(key::int(-2000))), Verdict::Some);
        round_trip(&summary);
    }

    #[test]
    fn integer_sums_do_not_overflow_a_segment() {
        let mut builder = IntBuilder::new();
        for _ in 0..256 {
            builder.push(&[i64::MAX; 1024], None);
        }
        let summary = builder.finish();
        assert_eq!(summary.sum, Sum::Int(i128::from(i64::MAX) * 262_144));
        round_trip(&summary);
    }

    #[test]
    fn float_summaries_order_nan_last() {
        let mut builder = FloatBuilder::new();
        builder.push(&[1.5, -0.0, f64::NAN, f64::NEG_INFINITY], None);
        builder.push(&[2.0, f64::INFINITY], Some(&[true, false]));
        let summary = builder.finish();
        assert!(matches!(summary.max, Some(Bound::Float(v)) if v.is_nan()));
        assert_eq!(summary.min, Some(Bound::Float(f64::NEG_INFINITY)));
        assert_eq!(summary.special, special::NAN | special::NEG_INF);
        assert!(matches!(summary.sum, Sum::Float(v) if v.is_nan()));
        assert_eq!(summary.vectors[1].test(&Test::Eq(key::float(2.0))), Verdict::Some);
        assert_eq!(summary.vectors[1].test(&Test::Gt(key::float(2.5))), Verdict::None);
        round_trip(&summary);
    }

    #[test]
    fn bytes_summaries() {
        let mut builder = BytesBuilder::new();
        let values: [Option<&[u8]>; 4] =
            [Some(b"http://b.example/1"), None, Some(b"http://a.example/zzz"), Some(b"")];
        builder.push(values.into_iter());
        builder.push([None, None].into_iter());
        let summary = builder.finish();
        assert_eq!(summary.min, Some(Bound::Bytes(b"".to_vec())));
        assert_eq!(summary.max, Some(Bound::Bytes(b"http://b.example/1".to_vec())));
        assert_eq!(summary.vectors[0].test(&Test::Eq(key::bytes(b"zzz"))), Verdict::None);
        assert_eq!(summary.vectors[1].test(&Test::IsNotNull), Verdict::None);
        round_trip(&summary);
    }

    #[test]
    fn code_summaries() {
        let mut builder = CodeBuilder::new();
        builder.push(&[3, 0, 5, 3], |c| Some(c * 10));
        builder.push(&[9, 1], |c| (c < 9).then_some(c * 10));
        let summary = builder.finish(usize::MAX);
        assert_eq!(summary.distinct_codes, Some(4));
        assert_eq!(summary.code_set.as_ref().unwrap().to_vec(), vec![1, 3, 5, 9]);
        assert_eq!(summary.min, None);
        assert_eq!(summary.vectors[0].test(&Test::Eq(key::int(40))), Verdict::Some);
        assert_eq!(summary.vectors[0].test(&Test::Eq(key::int(60))), Verdict::None);
        assert_eq!(summary.vectors[1].test(&Test::Eq(key::int(60))), Verdict::Some);
        round_trip(&summary);
        let small = CodeBuilder::new();
        assert!(small.finish(0).code_set.is_none());
    }
}

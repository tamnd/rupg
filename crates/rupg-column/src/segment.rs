//! A segment: 262,144 rows of a table, each column in vectors of 1024 values that decode alone (spec/09 section 9.2).

use rupg_common::{Error, Result};
use rupg_encode::integer;
use rupg_summary::column::{ColumnSummary, VECTOR};
use rupg_types::Value;

use crate::dict::Dictionary;

/// The rows of a full segment (spec/10).
pub const ROWS: usize = 262_144;

/// The words of the null bitmap of a vector.
pub const WORDS: usize = VECTOR / 64;

/// The physical type of a column (spec/09 section 9.8), with the PostgreSQL type that it gives back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `boolean`, stored as 0 or 1.
    Bool,
    /// `smallint`.
    Int2,
    /// `integer`.
    Int4,
    /// `bigint`.
    Int8,
    /// `real`, stored widened to `f64`, which is exact.
    Float4,
    /// `double precision`.
    Float8,
    /// `date`, the days since 1 January 2000.
    Date,
    /// `time`, the microseconds since midnight.
    Time,
    /// `timestamp`, the microseconds since 1 January 2000.
    Timestamp,
    /// `timestamptz`, the microseconds since 1 January 2000 UTC.
    TimestampTz,
    /// A string type kept as its text, with codes into the dictionary of the column.
    Text,
    /// `bytea`, with codes into the dictionary of the column.
    Bytea,
}

/// How a kind is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// The integer cascade over `i64`.
    Int,
    /// The bits of each `f64`, little endian.
    Float,
    /// Dictionary codes with the integer cascade.
    Code,
}

impl Kind {
    /// How the kind is stored.
    #[must_use]
    pub fn store(self) -> Store {
        match self {
            Kind::Float4 | Kind::Float8 => Store::Float,
            Kind::Text | Kind::Bytea => Store::Code,
            _ => Store::Int,
        }
    }

    /// The integer of a value of an integer kind, or `None` for a value of another type.
    #[must_use]
    pub fn int_of(self, value: &Value) -> Option<i64> {
        Some(match (self, value) {
            (Kind::Bool, Value::Bool(v)) => i64::from(*v),
            (Kind::Int2, Value::Int2(v)) => i64::from(*v),
            (Kind::Int4, Value::Int4(v)) => i64::from(*v),
            (Kind::Int8, Value::Int8(v)) => *v,
            (Kind::Date, Value::Date(v)) => i64::from(*v),
            (Kind::Time, Value::Time(v))
            | (Kind::Timestamp, Value::Timestamp(v))
            | (Kind::TimestampTz, Value::TimestampTz(v)) => *v,
            _ => return None,
        })
    }

    /// The value of an integer of an integer kind.
    #[must_use]
    pub fn int_value(self, v: i64) -> Value {
        match self {
            Kind::Bool => Value::Bool(v != 0),
            Kind::Int2 => Value::Int2(v as i16),
            Kind::Int4 => Value::Int4(v as i32),
            Kind::Date => Value::Date(v as i32),
            Kind::Time => Value::Time(v),
            Kind::Timestamp => Value::Timestamp(v),
            Kind::TimestampTz => Value::TimestampTz(v),
            _ => Value::Int8(v),
        }
    }

    /// The value of a float of a float kind.
    #[must_use]
    pub fn float_value(self, v: f64) -> Value {
        if self == Kind::Float4 { Value::Float4(v as f32) } else { Value::Float8(v) }
    }

    /// The value of the bytes of a string kind.
    #[must_use]
    pub fn bytes_value(self, v: &[u8]) -> Value {
        if self == Kind::Bytea {
            Value::Bytea(v.to_vec())
        } else {
            // The dictionary of a text column holds only the bytes of a `String`.
            Value::Text(
                String::from_utf8(v.to_vec())
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
            )
        }
    }
}

/// The null positions of a vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nulls {
    /// No value is null.
    None,
    /// Every value is null.
    All,
    /// A bitmap with a set bit for each null position.
    Some(Box<[u64; WORDS]>),
}

impl Nulls {
    /// True when the value at `index` is null.
    #[must_use]
    #[inline]
    pub fn is_null(&self, index: usize) -> bool {
        match self {
            Nulls::None => false,
            Nulls::All => true,
            Nulls::Some(bits) => bits[index / 64] >> (index % 64) & 1 == 1,
        }
    }
}

/// One encoded vector of a column.
#[derive(Clone, Debug)]
pub struct Vector {
    /// The rows of the vector, 1024 except in the last vector of a segment.
    pub rows: u32,
    /// The encoded values. A null position holds a value that keeps the stream compressible.
    pub data: Box<[u8]>,
    /// The null positions.
    pub nulls: Nulls,
}

/// One column of a segment.
#[derive(Clone, Debug)]
pub struct Column {
    /// The physical type.
    pub kind: Kind,
    /// The vectors, in row order.
    pub vectors: Vec<Vector>,
    /// The summary of spec/09 section 9.7.
    pub summary: ColumnSummary,
}

impl Column {
    /// The encoded bytes of the column, without the summary.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.vectors
            .iter()
            .map(|v| v.data.len() + if matches!(v.nulls, Nulls::Some(_)) { WORDS * 8 } else { 0 })
            .sum()
    }

    /// Decodes the vector `at` of an integer or code column into `out`, which it clears first.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a vector.
    pub fn decode_ints(&self, at: usize, out: &mut Vec<i64>) -> Result<()> {
        out.clear();
        out.extend(integer::decode(&self.vectors[at].data)?);
        Ok(())
    }

    /// Decodes the vector `at` of a code column into `out`, which it clears first.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a vector.
    pub fn decode_codes(&self, at: usize, out: &mut Vec<u32>) -> Result<()> {
        out.clear();
        out.extend(integer::decode_as::<u32>(&self.vectors[at].data)?);
        Ok(())
    }

    /// Decodes the vector `at` of a float column into `out`, which it clears first.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a vector.
    pub fn decode_floats(&self, at: usize, out: &mut Vec<f64>) -> Result<()> {
        out.clear();
        let (words, rest) = self.vectors[at].data.as_chunks::<8>();
        if !rest.is_empty() {
            return Err(Error::corrupted("a float vector has a partial value"));
        }
        out.extend(words.iter().map(|w| f64::from_bits(u64::from_le_bytes(*w))));
        Ok(())
    }

    /// The values of the vector `at`, as the row engine reads them. `dict` is the dictionary of a code column.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a vector.
    pub fn values(&self, at: usize, dict: Option<&Dictionary>, out: &mut Vec<Value>) -> Result<()> {
        out.clear();
        let vector = &self.vectors[at];
        if vector.nulls == Nulls::All {
            out.resize(vector.rows as usize, Value::Null);
            return Ok(());
        }
        match self.kind.store() {
            Store::Int => {
                for (i, v) in integer::decode(&vector.data)?.into_iter().enumerate() {
                    out.push(if vector.nulls.is_null(i) {
                        Value::Null
                    } else {
                        self.kind.int_value(v)
                    });
                }
            }
            Store::Float => {
                let mut floats = Vec::new();
                self.decode_floats(at, &mut floats)?;
                for (i, v) in floats.into_iter().enumerate() {
                    out.push(if vector.nulls.is_null(i) {
                        Value::Null
                    } else {
                        self.kind.float_value(v)
                    });
                }
            }
            Store::Code => {
                let dict =
                    dict.ok_or_else(|| Error::internal("a code column has no dictionary"))?;
                for (i, code) in integer::decode_as::<u32>(&vector.data)?.into_iter().enumerate() {
                    out.push(if vector.nulls.is_null(i) {
                        Value::Null
                    } else {
                        self.kind.bytes_value(dict.get(code))
                    });
                }
            }
        }
        if out.len() != vector.rows as usize {
            return Err(Error::corrupted("a vector has another number of values than its rows"));
        }
        Ok(())
    }
}

/// A segment of a table.
#[derive(Clone, Debug)]
pub struct Segment {
    /// The rows.
    pub rows: u32,
    /// The columns, in the order of the table.
    pub columns: Vec<Column>,
}

impl Segment {
    /// The number of vectors.
    #[must_use]
    pub fn vectors(&self) -> usize {
        self.rows.div_ceil(VECTOR as u32) as usize
    }

    /// The encoded bytes of the segment, with the summaries.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.columns
            .iter()
            .map(|c| {
                let mut summary = Vec::new();
                c.summary.write(&mut summary);
                c.encoded_len() + summary.len()
            })
            .sum()
    }
}

//! The builder of segments: it takes rows, keeps them as typed values, and encodes a segment from them (spec/10, the bulk load).

use rupg_common::{Error, Result};
use rupg_encode::chooser::Sampled;
use rupg_encode::integer;
use rupg_summary::column::{CodeBuilder, FloatBuilder, IntBuilder, VECTOR};
use rupg_types::Value;

use crate::dict::{Dictionary, NONE};
use crate::segment::{Column, Kind, Nulls, Segment, Store, Vector, WORDS};

/// The values of one column that wait for a segment.
#[derive(Clone, Debug)]
enum Buffer {
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Codes(Vec<u32>),
}

/// The rows that wait for a segment, by column.
#[derive(Clone, Debug)]
pub struct Builder {
    kinds: Vec<Kind>,
    buffers: Vec<Buffer>,
    nulls: Vec<Vec<bool>>,
    rows: usize,
}

impl Builder {
    /// A builder for a table with the columns `kinds`.
    #[must_use]
    pub fn new(kinds: &[Kind]) -> Builder {
        let buffers = kinds
            .iter()
            .map(|k| match k.store() {
                Store::Int => Buffer::Ints(Vec::new()),
                Store::Float => Buffer::Floats(Vec::new()),
                Store::Code => Buffer::Codes(Vec::new()),
            })
            .collect();
        Builder { kinds: kinds.to_vec(), buffers, nulls: vec![Vec::new(); kinds.len()], rows: 0 }
    }

    /// The rows that wait.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Adds a row. `dicts` has the dictionary of each code column, in the order of the columns, and `None` for the others. A string that a dictionary does not have gets a code in its tail.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of its column, and then the builder is as it was.
    pub fn push(&mut self, row: &[Value], dicts: &mut [Option<Dictionary>]) -> Result<()> {
        if row.len() < self.kinds.len() {
            return Err(Error::internal("a row has fewer values than the columns of its segment"));
        }
        for (kind, value) in self.kinds.iter().zip(row) {
            let fits = match (kind.store(), value) {
                (_, Value::Null) => true,
                (Store::Int, v) => kind.int_of(v).is_some(),
                (Store::Float, v) => matches!(
                    (kind, v),
                    (Kind::Float4, Value::Float4(_)) | (Kind::Float8, Value::Float8(_))
                ),
                (Store::Code, v) => matches!(
                    (kind, v),
                    (Kind::Text, Value::Text(_)) | (Kind::Bytea, Value::Bytea(_))
                ),
            };
            if !fits {
                return Err(Error::internal(format!(
                    "a value of a {kind:?} column has another type"
                )));
            }
        }
        for (column, value) in row.iter().take(self.kinds.len()).enumerate() {
            let kind = self.kinds[column];
            let null = matches!(value, Value::Null);
            self.nulls[column].push(null);
            match &mut self.buffers[column] {
                Buffer::Ints(values) => {
                    // A null holds the previous value, which keeps runs and deltas (spec/09 section 9.2).
                    let v =
                        kind.int_of(value).unwrap_or_else(|| values.last().copied().unwrap_or(0));
                    values.push(v);
                }
                Buffer::Floats(values) => values.push(match value {
                    Value::Float4(v) => f64::from(*v),
                    Value::Float8(v) => *v,
                    _ => 0.0,
                }),
                Buffer::Codes(codes) => {
                    let dict = dicts[column].get_or_insert_with(Dictionary::new);
                    codes.push(match value {
                        Value::Text(s) => dict.intern(s.as_bytes()),
                        Value::Bytea(b) => dict.intern(b),
                        _ => NONE,
                    });
                }
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Encodes the rows that wait into a segment and empties the builder. `dicts` gives the ranks of the codes.
    ///
    /// # Errors
    ///
    /// An error of the encoder, which is an internal error.
    pub fn finish(&mut self, dicts: &[Option<Dictionary>]) -> Result<Segment> {
        let mut columns = Vec::with_capacity(self.kinds.len());
        for column in 0..self.kinds.len() {
            columns.push(self.encode(column, dicts.get(column).and_then(Option::as_ref))?);
        }
        let rows = self.rows;
        self.clear();
        Ok(Segment { rows: rows as u32, columns })
    }

    /// Puts the values of the rows `rows` of the column `column` into `out`, which it clears first. `dict` is the dictionary of a code column.
    pub fn values(
        &self,
        column: usize,
        rows: std::ops::Range<usize>,
        dict: Option<&Dictionary>,
        out: &mut Vec<Value>,
    ) {
        out.clear();
        let kind = self.kinds[column];
        let nulls = &self.nulls[column][rows.clone()];
        match &self.buffers[column] {
            Buffer::Ints(values) => out.extend(
                values[rows]
                    .iter()
                    .zip(nulls)
                    .map(|(&v, &null)| if null { Value::Null } else { kind.int_value(v) }),
            ),
            Buffer::Floats(values) => out.extend(
                values[rows]
                    .iter()
                    .zip(nulls)
                    .map(|(&v, &null)| if null { Value::Null } else { kind.float_value(v) }),
            ),
            Buffer::Codes(codes) => {
                out.extend(codes[rows].iter().zip(nulls).map(|(&code, &null)| match dict {
                    Some(dict) if !null => kind.bytes_value(dict.get(code)),
                    _ => Value::Null,
                }));
            }
        }
    }

    /// The bytes that the rows that wait take in memory.
    #[must_use]
    pub fn footprint(&self) -> usize {
        let values: usize = self
            .buffers
            .iter()
            .map(|buffer| match buffer {
                Buffer::Ints(values) => values.capacity() * 8,
                Buffer::Floats(values) => values.capacity() * 8,
                Buffer::Codes(codes) => codes.capacity() * 4,
            })
            .sum();
        values + self.nulls.iter().map(Vec::capacity).sum::<usize>()
    }

    /// Keeps only the first `rows` rows that wait. The dictionaries keep the strings of the rows that go.
    pub fn truncate(&mut self, rows: usize) {
        if rows >= self.rows {
            return;
        }
        for buffer in &mut self.buffers {
            match buffer {
                Buffer::Ints(values) => values.truncate(rows),
                Buffer::Floats(values) => values.truncate(rows),
                Buffer::Codes(codes) => codes.truncate(rows),
            }
        }
        for nulls in &mut self.nulls {
            nulls.truncate(rows);
        }
        self.rows = rows;
    }

    /// Empties the builder.
    pub fn clear(&mut self) {
        for buffer in &mut self.buffers {
            match buffer {
                Buffer::Ints(values) => values.clear(),
                Buffer::Floats(values) => values.clear(),
                Buffer::Codes(codes) => codes.clear(),
            }
        }
        for nulls in &mut self.nulls {
            nulls.clear();
        }
        self.rows = 0;
    }

    /// Encodes the rows that wait of the column `column` into the column of a segment, and keeps the rows. `dict` gives the ranks of the codes.
    ///
    /// # Errors
    ///
    /// An error of the encoder, which is an internal error.
    pub fn encode(&self, column: usize, dict: Option<&Dictionary>) -> Result<Column> {
        let rows = self.rows;
        let kind = self.kinds[column];
        let nulls = &self.nulls[column];
        let mut vectors = Vec::with_capacity(rows.div_ceil(VECTOR));
        let chooser = Sampled::new();
        let summary = match &self.buffers[column] {
            Buffer::Ints(values) => {
                let mut summary = IntBuilder::new();
                for (chunk, valid) in values.chunks(VECTOR).zip(nulls.chunks(VECTOR)) {
                    let valid = valid_flags(valid);
                    summary.push(chunk, valid.as_deref());
                    let data = integer::encode_with(chunk, &chooser)?.into_boxed_slice();
                    vectors.push(Vector {
                        rows: chunk.len() as u32,
                        data,
                        nulls: nulls_of(valid.as_deref(), chunk.len()),
                    });
                }
                summary.finish()
            }
            Buffer::Floats(values) => {
                let mut summary = FloatBuilder::new();
                for (chunk, valid) in values.chunks(VECTOR).zip(nulls.chunks(VECTOR)) {
                    let valid = valid_flags(valid);
                    summary.push(chunk, valid.as_deref());
                    let data: Vec<u8> =
                        chunk.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect();
                    vectors.push(Vector {
                        rows: chunk.len() as u32,
                        data: data.into_boxed_slice(),
                        nulls: nulls_of(valid.as_deref(), chunk.len()),
                    });
                }
                summary.finish()
            }
            Buffer::Codes(codes) => {
                let mut summary = CodeBuilder::new();
                let mut wide = Vec::with_capacity(VECTOR);
                let mut bytes = 0;
                for (chunk, valid) in codes.chunks(VECTOR).zip(nulls.chunks(VECTOR)) {
                    let valid = valid_flags(valid);
                    summary.push(chunk, |c| dict.and_then(|d| d.rank(c)));
                    wide.clear();
                    wide.extend(chunk.iter().map(|&c| i64::from(c)));
                    let data = integer::encode_with(&wide, &chooser)?.into_boxed_slice();
                    bytes += data.len();
                    vectors.push(Vector {
                        rows: chunk.len() as u32,
                        data,
                        nulls: nulls_of(valid.as_deref(), chunk.len()),
                    });
                }
                summary.finish(bytes / 8)
            }
        };
        Ok(Column { kind, vectors, summary })
    }
}

/// The valid flags of a vector from its null flags, or `None` when no value is null.
fn valid_flags(nulls: &[bool]) -> Option<Vec<bool>> {
    nulls.contains(&true).then(|| nulls.iter().map(|n| !n).collect())
}

fn nulls_of(valid: Option<&[bool]>, rows: usize) -> Nulls {
    let Some(valid) = valid else { return Nulls::None };
    if valid.iter().all(|v| !v) {
        return Nulls::All;
    }
    let mut bits = Box::new([0u64; WORDS]);
    for (i, _) in valid.iter().enumerate().filter(|(_, v)| !**v) {
        bits[i / 64] |= 1 << (i % 64);
    }
    debug_assert!(rows <= VECTOR);
    Nulls::Some(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_summary::column::{Bound, Sum};

    fn rows(n: usize) -> Vec<Vec<Value>> {
        (0..n)
            .map(|i| {
                vec![
                    Value::Int8(i as i64 * 3),
                    if i % 7 == 0 { Value::Null } else { Value::Int2((i % 50) as i16) },
                    Value::Text(format!("http://site/{}", i % 100)),
                    Value::Float8(i as f64 / 4.0),
                    Value::Timestamp(1_000_000 * i as i64),
                    if i % 3 == 0 { Value::Null } else { Value::Text(String::new()) },
                ]
            })
            .collect()
    }

    #[test]
    fn a_segment_gives_back_its_rows() {
        let kinds = [Kind::Int8, Kind::Int2, Kind::Text, Kind::Float8, Kind::Timestamp, Kind::Text];
        let mut dicts: Vec<Option<Dictionary>> = vec![None; kinds.len()];
        let mut builder = Builder::new(&kinds);
        let input = rows(5000);
        for row in &input {
            builder.push(row, &mut dicts).unwrap();
        }
        let segment = builder.finish(&dicts).unwrap();
        assert_eq!(segment.rows, 5000);
        assert_eq!(segment.vectors(), 5);
        assert_eq!(builder.rows(), 0);
        let mut values = Vec::new();
        for (c, column) in segment.columns.iter().enumerate() {
            let mut all = Vec::new();
            for v in 0..segment.vectors() {
                column.values(v, dicts[c].as_ref(), &mut values).unwrap();
                all.append(&mut values);
            }
            let expected: Vec<Value> = input.iter().map(|r| r[c].clone()).collect();
            assert_eq!(all, expected, "column {c}");
        }
        let id = &segment.columns[0].summary;
        assert_eq!(id.min, Some(Bound::Int(0)));
        assert_eq!(id.max, Some(Bound::Int(4999 * 3)));
        assert_eq!(id.sum, Sum::Int((0..5000i128).map(|i| i * 3).sum()));
        assert_eq!(segment.columns[1].summary.nulls, 715);
        assert_eq!(segment.columns[2].summary.distinct_codes, Some(100));
        assert!(segment.columns[0].encoded_len() < 5000);
    }

    #[test]
    fn a_value_of_another_type_leaves_the_builder() {
        let mut builder = Builder::new(&[Kind::Int4, Kind::Int4]);
        let mut dicts = vec![None, None];
        assert!(builder.push(&[Value::Int4(1), Value::Text("x".into())], &mut dicts).is_err());
        assert_eq!(builder.rows(), 0);
        builder.push(&[Value::Int4(1), Value::Null], &mut dicts).unwrap();
        let segment = builder.finish(&dicts).unwrap();
        assert_eq!(segment.columns[1].vectors[0].nulls, Nulls::All);
    }
}

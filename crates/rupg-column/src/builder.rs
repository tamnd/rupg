//! The builder of segments: it takes rows, keeps them as typed values, and encodes a segment from them (spec/10, the bulk load).
//!
//! The builder encodes each vector of [`VECTOR`] rows when the next row comes, so that the rows that wait take the memory of their encoded vectors and of one vector of typed values.

use rupg_common::{Error, Result};
use rupg_encode::chooser::Sampled;
use rupg_encode::integer;
use rupg_summary::column::{CodeBuilder, FloatBuilder, IntBuilder, VECTOR};
use rupg_types::Value;

use crate::dict::{Dictionary, NONE};
use crate::segment::{Column, Kind, Nulls, Segment, Store, Vector, WORDS};

/// The values of one column that wait for a vector.
#[derive(Clone, Debug)]
enum Buffer {
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Codes(Vec<u32>),
}

/// The summary of the encoded vectors of one column.
#[derive(Clone, Debug)]
enum Summary {
    Int(IntBuilder),
    Float(FloatBuilder),
    /// The summary and the bytes of the encoded codes.
    Code(CodeBuilder, usize),
}

impl Summary {
    fn new(store: Store) -> Summary {
        match store {
            Store::Int => Summary::Int(IntBuilder::new()),
            Store::Float => Summary::Float(FloatBuilder::new()),
            Store::Code => Summary::Code(CodeBuilder::new(), 0),
        }
    }
}

/// The rows that wait for a segment, by column.
#[derive(Clone, Debug)]
pub struct Builder {
    kinds: Vec<Kind>,
    /// The values of the rows after the encoded vectors, at most one vector, by column.
    buffers: Vec<Buffer>,
    /// The null flags of the rows in `buffers`, by column.
    nulls: Vec<Vec<bool>>,
    /// The encoded vectors, by column.
    vectors: Vec<Vec<Vector>>,
    /// The summary of the encoded vectors, by column.
    summaries: Vec<Summary>,
    /// The last integer value of the encoded vectors, by column, which a null holds when it is the first row of a vector.
    last: Vec<i64>,
    /// The rows in the encoded vectors.
    encoded: usize,
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
        Builder {
            kinds: kinds.to_vec(),
            buffers,
            nulls: vec![Vec::new(); kinds.len()],
            vectors: vec![Vec::new(); kinds.len()],
            summaries: kinds.iter().map(|k| Summary::new(k.store())).collect(),
            last: vec![0; kinds.len()],
            encoded: 0,
            rows: 0,
        }
    }

    /// The rows that wait.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Adds a row. `dicts` has the dictionary of each code column, in the order of the columns, and `None` for the others. A string that a dictionary does not have gets a code in its tail. A full vector of rows that wait encodes first.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of its column, or a vector does not keep its values, and then the builder is as it was.
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
        if self.rows - self.encoded == VECTOR {
            self.flush(dicts)?;
        }
        for (column, value) in row.iter().take(self.kinds.len()).enumerate() {
            let kind = self.kinds[column];
            let null = matches!(value, Value::Null);
            self.nulls[column].push(null);
            match &mut self.buffers[column] {
                Buffer::Ints(values) => {
                    // A null holds the previous value, which keeps runs and deltas (spec/09 section 9.2).
                    let v = kind
                        .int_of(value)
                        .unwrap_or_else(|| values.last().copied().unwrap_or(self.last[column]));
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

    /// Encodes the rows in the buffers as one more vector of each column, and empties the buffers.
    ///
    /// # Errors
    ///
    /// `XX000` when a vector does not keep its values, and then the builder is as it was.
    fn flush(&mut self, dicts: &[Option<Dictionary>]) -> Result<()> {
        let chooser = Sampled::new();
        let mut made = Vec::with_capacity(self.kinds.len());
        for column in 0..self.kinds.len() {
            made.push(encode_vector(&self.buffers[column], &self.nulls[column], &chooser)?);
        }
        for (column, vector) in made.into_iter().enumerate() {
            let dict = dicts.get(column).and_then(Option::as_ref);
            summarize(
                &mut self.summaries[column],
                &self.buffers[column],
                &self.nulls[column],
                &vector,
                dict,
            );
            if let Buffer::Ints(values) = &self.buffers[column]
                && let Some(&v) = values.last()
            {
                self.last[column] = v;
            }
            self.vectors[column].push(vector);
            clear_buffer(&mut self.buffers[column]);
            self.nulls[column].clear();
        }
        self.encoded = self.rows;
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
    ///
    /// # Errors
    ///
    /// `XX001` when an encoded vector does not decode.
    pub fn values(
        &self,
        column: usize,
        rows: std::ops::Range<usize>,
        dict: Option<&Dictionary>,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        out.clear();
        let kind = self.kinds[column];
        let mut decoded = Vec::new();
        let mut at = rows.start;
        while at < rows.end.min(self.encoded) {
            let index = at / VECTOR;
            self.vectors[column][index].values(kind, dict, &mut decoded)?;
            let start = index * VECTOR;
            let end = rows.end.min(start + VECTOR);
            out.extend_from_slice(&decoded[at - start..end - start]);
            at = end;
        }
        if at >= rows.end {
            return Ok(());
        }
        let rows = at - self.encoded..rows.end - self.encoded;
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
        Ok(())
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
        let vectors: usize = self
            .vectors
            .iter()
            .flatten()
            .map(|v| v.data.len() + if matches!(v.nulls, Nulls::Some(_)) { WORDS * 8 } else { 0 })
            .sum();
        values + vectors + self.nulls.iter().map(Vec::capacity).sum::<usize>()
    }

    /// Keeps only the first `rows` rows that wait. The dictionaries keep the strings of the rows that go. `dicts` gives the ranks of the codes, as in [`Builder::push`].
    ///
    /// # Errors
    ///
    /// `XX001` when an encoded vector does not decode.
    pub fn truncate(&mut self, rows: usize, dicts: &[Option<Dictionary>]) -> Result<()> {
        if rows >= self.rows {
            return Ok(());
        }
        if rows >= self.encoded {
            let keep = rows - self.encoded;
            for buffer in &mut self.buffers {
                truncate_buffer(buffer, keep);
            }
            for nulls in &mut self.nulls {
                nulls.truncate(keep);
            }
            self.rows = rows;
            return Ok(());
        }
        // The vector of the row `rows` goes back into the buffers, and the summaries are made again from the vectors that stay.
        let whole = rows / VECTOR;
        let mut buffers = Vec::with_capacity(self.kinds.len());
        let mut nulls = Vec::with_capacity(self.kinds.len());
        let mut summaries = Vec::with_capacity(self.kinds.len());
        let mut last = Vec::with_capacity(self.kinds.len());
        for (column, kind) in self.kinds.iter().enumerate() {
            let dict = dicts.get(column).and_then(Option::as_ref);
            let mut summary = Summary::new(kind.store());
            let mut carry = 0;
            for vector in &self.vectors[column][..whole] {
                let (buffer, flags) = decode_vector(kind.store(), vector)?;
                summarize(&mut summary, &buffer, &flags, vector, dict);
                if let Buffer::Ints(values) = &buffer {
                    carry = values.last().copied().unwrap_or(0);
                }
            }
            let (mut buffer, mut flags) =
                decode_vector(kind.store(), &self.vectors[column][whole])?;
            truncate_buffer(&mut buffer, rows - whole * VECTOR);
            flags.truncate(rows - whole * VECTOR);
            buffers.push(buffer);
            nulls.push(flags);
            summaries.push(summary);
            last.push(carry);
        }
        for vectors in &mut self.vectors {
            vectors.truncate(whole);
        }
        self.buffers = buffers;
        self.nulls = nulls;
        self.summaries = summaries;
        self.last = last;
        self.encoded = whole * VECTOR;
        self.rows = rows;
        Ok(())
    }

    /// Empties the builder.
    pub fn clear(&mut self) {
        for buffer in &mut self.buffers {
            clear_buffer(buffer);
        }
        for nulls in &mut self.nulls {
            nulls.clear();
        }
        for vectors in &mut self.vectors {
            vectors.clear();
        }
        for (summary, kind) in self.summaries.iter_mut().zip(&self.kinds) {
            *summary = Summary::new(kind.store());
        }
        self.last.fill(0);
        self.encoded = 0;
        self.rows = 0;
    }

    /// Encodes the rows that wait of the column `column` into the column of a segment, and keeps the rows. `dict` gives the ranks of the codes.
    ///
    /// # Errors
    ///
    /// An error of the encoder, or a vector that does not keep its values, which is an internal error.
    pub fn encode(&self, column: usize, dict: Option<&Dictionary>) -> Result<Column> {
        let mut vectors = self.vectors[column].clone();
        let mut summary = self.summaries[column].clone();
        if self.rows > self.encoded {
            let (buffer, nulls) = (&self.buffers[column], &self.nulls[column]);
            let vector = encode_vector(buffer, nulls, &Sampled::new())?;
            summarize(&mut summary, buffer, nulls, &vector, dict);
            vectors.push(vector);
        }
        let summary = match summary {
            Summary::Int(summary) => summary.finish(),
            Summary::Float(summary) => summary.finish(),
            Summary::Code(summary, bytes) => summary.finish(bytes / 8),
        };
        Ok(Column { kind: self.kinds[column], vectors, summary })
    }
}

/// The vector of the values `buffer` with the null flags `nulls`, which the decode checks.
///
/// # Errors
///
/// An error of the encoder, or `XX000` when the vector does not decode to its values.
fn encode_vector(buffer: &Buffer, nulls: &[bool], chooser: &Sampled) -> Result<Vector> {
    let valid = valid_flags(nulls);
    let rows = nulls.len();
    let data = match buffer {
        Buffer::Ints(values) => integer::encode_with(values, chooser)?,
        Buffer::Floats(values) => values.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect(),
        Buffer::Codes(codes) => {
            let wide: Vec<i64> = codes.iter().map(|&c| i64::from(c)).collect();
            integer::encode_with(&wide, chooser)?
        }
    };
    let vector = Vector {
        rows: rows as u32,
        data: data.into_boxed_slice(),
        nulls: nulls_of(valid.as_deref(), rows),
    };
    let kept = match (buffer, decode_vector(store_of(buffer), &vector)) {
        (Buffer::Ints(a), Ok((Buffer::Ints(b), flags))) => *a == b && flags == nulls,
        (Buffer::Codes(a), Ok((Buffer::Codes(b), flags))) => *a == b && flags == nulls,
        (Buffer::Floats(a), Ok((Buffer::Floats(b), flags))) => {
            a.len() == b.len()
                && a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits())
                && flags == nulls
        }
        _ => false,
    };
    if !kept {
        return Err(Error::internal("a segment does not keep the values of its rows"));
    }
    Ok(vector)
}

/// The values and the null flags of the encoded vector `vector` of a column that stores `store`.
///
/// # Errors
///
/// `XX001` when the bytes are not a vector.
fn decode_vector(store: Store, vector: &Vector) -> Result<(Buffer, Vec<bool>)> {
    let buffer = match store {
        Store::Int => Buffer::Ints(integer::decode(&vector.data)?),
        Store::Float => Buffer::Floats(vector.floats()?),
        Store::Code => Buffer::Codes(integer::decode_as::<u32>(&vector.data)?),
    };
    let rows = vector.rows as usize;
    if buffer_len(&buffer) != rows {
        return Err(Error::corrupted("a vector has another number of values than its rows"));
    }
    Ok((buffer, (0..rows).map(|i| vector.nulls.is_null(i)).collect()))
}

/// Adds the vector `vector` of the values `buffer` with the null flags `nulls` to the summary `summary`.
fn summarize(
    summary: &mut Summary,
    buffer: &Buffer,
    nulls: &[bool],
    vector: &Vector,
    dict: Option<&Dictionary>,
) {
    let valid = valid_flags(nulls);
    match (summary, buffer) {
        (Summary::Int(summary), Buffer::Ints(values)) => summary.push(values, valid.as_deref()),
        (Summary::Float(summary), Buffer::Floats(values)) => {
            summary.push(values, valid.as_deref());
        }
        (Summary::Code(summary, bytes), Buffer::Codes(codes)) => {
            summary.push(codes, |c| dict.and_then(|d| d.rank(c)));
            *bytes += vector.data.len();
        }
        _ => {}
    }
}

fn store_of(buffer: &Buffer) -> Store {
    match buffer {
        Buffer::Ints(_) => Store::Int,
        Buffer::Floats(_) => Store::Float,
        Buffer::Codes(_) => Store::Code,
    }
}

fn buffer_len(buffer: &Buffer) -> usize {
    match buffer {
        Buffer::Ints(values) => values.len(),
        Buffer::Floats(values) => values.len(),
        Buffer::Codes(codes) => codes.len(),
    }
}

fn truncate_buffer(buffer: &mut Buffer, rows: usize) {
    match buffer {
        Buffer::Ints(values) => values.truncate(rows),
        Buffer::Floats(values) => values.truncate(rows),
        Buffer::Codes(codes) => codes.truncate(rows),
    }
}

fn clear_buffer(buffer: &mut Buffer) {
    truncate_buffer(buffer, 0);
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

    #[test]
    fn a_builder_reads_and_cuts_rows_in_its_encoded_vectors() {
        let kinds = [Kind::Int8, Kind::Int2, Kind::Text, Kind::Float8, Kind::Timestamp, Kind::Text];
        let mut dicts: Vec<Option<Dictionary>> = vec![None; kinds.len()];
        let mut builder = Builder::new(&kinds);
        let input = rows(3000);
        for row in &input {
            builder.push(row, &mut dicts).unwrap();
        }
        assert!(builder.footprint() < 3000 * 6 * 4);
        let mut values = Vec::new();
        for (c, dict) in dicts.iter().enumerate() {
            builder.values(c, 1000..2100, dict.as_ref(), &mut values).unwrap();
            let expected: Vec<Value> = input[1000..2100].iter().map(|r| r[c].clone()).collect();
            assert_eq!(values, expected, "column {c}");
        }
        builder.truncate(1500, &dicts).unwrap();
        for row in &input[1500..] {
            builder.push(row, &mut dicts).unwrap();
        }
        let segment = builder.finish(&dicts).unwrap();
        assert_eq!(segment.rows, 3000);
        for (c, column) in segment.columns.iter().enumerate() {
            let mut all = Vec::new();
            for v in 0..segment.vectors() {
                column.values(v, dicts[c].as_ref(), &mut values).unwrap();
                all.append(&mut values);
            }
            let expected: Vec<Value> = input.iter().map(|r| r[c].clone()).collect();
            assert_eq!(all, expected, "column {c}");
        }
        assert_eq!(segment.columns[0].summary.max, Some(Bound::Int(2999 * 3)));
        assert_eq!(segment.columns[1].summary.nulls, 429);
        assert_eq!(segment.columns[2].summary.distinct_codes, Some(100));
    }
}

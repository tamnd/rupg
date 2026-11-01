//! The values of one column of a table in segments, which a scan reads one vector at a time.
//!
//! All the columns of a table that the same rows fill have the same segments and the same vectors, so that the vector `v` of the segment `s` of each column holds the same rows.

use rupg_common::{Error, Result};
pub use rupg_summary::column::VECTOR;
use rupg_types::Value;

use crate::builder::Builder;
use crate::dict::Dictionary;
use crate::segment::{Column, Kind, ROWS};

/// One column of a table in segments.
#[derive(Clone, Debug)]
pub struct Stored {
    /// The physical type.
    pub kind: Kind,
    /// The global dictionary of a code column.
    pub dict: Option<Dictionary>,
    /// The column of each segment, in row order. Each segment has [`ROWS`] rows, except the last one. When the column has open rows, each segment has [`ROWS`] rows.
    pub segments: Vec<Column>,
    /// The rows of the column: the rows of all the segments and the open rows.
    pub rows: usize,
    /// The rows after the last segment that wait for a whole segment, as typed values and codes and not encoded (spec/10 section 10.8). A load adds rows here, and each [`ROWS`] rows become one segment, so that the load encodes each row one time.
    open: Option<Builder>,
}

impl Stored {
    /// The column of the values `values`, in row order, with no open rows. `None` is a row with no value for the column, which reads as null.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of the kind.
    pub fn build<'a>(
        kind: Kind,
        values: impl Iterator<Item = Option<&'a Value>>,
    ) -> Result<Stored> {
        let mut stored = Stored::new(kind, None, Vec::new(), 0);
        stored.extend(values)?;
        stored.seal()?;
        Ok(stored)
    }

    /// A column with the segments `segments` of `rows` rows and no open rows.
    #[must_use]
    pub fn new(kind: Kind, dict: Option<Dictionary>, segments: Vec<Column>, rows: usize) -> Stored {
        Stored { kind, dict, segments, rows, open: None }
    }

    /// Adds the values `values` after the rows of the column. The rows after the last whole segment stay open, and [`ROWS`] open rows become a segment when the next row comes, so that a [`Stored::truncate`] to the rows before an extend that does not cross a segment only cuts the open rows. A last segment with fewer than [`ROWS`] rows becomes open rows again first. The codes of the dictionary do not change. The column checks each new segment with a decode.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of the kind, or a new segment does not keep its values, and then the column has the values before it. `XX001` when the last segment does not decode, and then the column is as it was.
    pub fn extend<'a>(&mut self, values: impl Iterator<Item = Option<&'a Value>>) -> Result<()> {
        if self.open.is_none()
            && !self.rows.is_multiple_of(ROWS)
            && let Some(last) = self.segments.last()
        {
            let mut open = Builder::new(&[self.kind]);
            let mut decoded = Vec::new();
            for at in 0..last.vectors.len() {
                last.values(at, self.dict.as_ref(), &mut decoded)?;
                for value in &decoded {
                    // The strings of the segment are in the dictionary, so the codes do not change.
                    open.push(std::slice::from_ref(value), std::slice::from_mut(&mut self.dict))?;
                }
            }
            self.segments.pop();
            self.open = Some(open);
        }
        let start = self.rows;
        let null = [Value::Null];
        let mut dicts = [self.dict.take()];
        let mut open = self.open.take().unwrap_or_else(|| Builder::new(&[self.kind]));
        let result = (|| {
            for value in values {
                if open.rows() == ROWS {
                    self.segments.push(close(&open, dicts[0].as_ref())?);
                    open.clear();
                }
                open.push(value.map_or(&null[..], std::slice::from_ref), &mut dicts)?;
                self.rows += 1;
            }
            Ok(())
        })();
        let [dict] = dicts;
        self.dict = dict;
        self.open = (open.rows() > 0).then_some(open);
        if result.is_err() {
            // The rows of this extend are in the open rows or in segments that keep their values, which decode.
            self.truncate(start)?;
        }
        result
    }

    /// Encodes the open rows into a last segment, so that the column has only segments. The column checks the segment with a decode.
    ///
    /// # Errors
    ///
    /// An error of the encoder, or a segment that does not keep its values, which is an internal error. The column is then as it was.
    pub fn seal(&mut self) -> Result<()> {
        if let Some(open) = &self.open {
            let column = close(open, self.dict.as_ref())?;
            self.segments.push(column);
            self.open = None;
        }
        Ok(())
    }

    /// True when the column has no open rows.
    #[must_use]
    pub fn sealed(&self) -> bool {
        self.open.is_none()
    }

    /// The open rows, as one more segment, and not encoded.
    #[must_use]
    pub fn open_rows(&self) -> usize {
        self.open.as_ref().map_or(0, Builder::rows)
    }

    /// Puts the values of the vector `at` of the open rows into `out`, which it clears first. The open rows have vectors of [`VECTOR`] rows, as a segment has. The result is false when the open rows have no vector `at`.
    pub fn open_values(&self, at: usize, out: &mut Vec<Value>) -> bool {
        out.clear();
        let Some(open) = &self.open else { return false };
        let start = at * VECTOR;
        if start >= open.rows() {
            return false;
        }
        open.values(0, start..(start + VECTOR).min(open.rows()), self.dict.as_ref(), out);
        true
    }

    /// Keeps only the first `rows` rows of the column, after an [`Stored::extend`] that the caller does not keep. The dictionary keeps the strings of the rows that go.
    ///
    /// # Errors
    ///
    /// `XX001` when a segment does not decode.
    pub fn truncate(&mut self, rows: usize) -> Result<()> {
        if rows >= self.rows {
            return Ok(());
        }
        let first = self.rows - self.open_rows();
        if rows >= first {
            if let Some(open) = &mut self.open {
                open.truncate(rows - first);
                if open.rows() == 0 {
                    self.open = None;
                }
            }
            self.rows = rows;
            return Ok(());
        }
        let whole = rows / ROWS;
        let mut kept = Vec::with_capacity(rows - whole * ROWS);
        let mut vector = Vec::new();
        if let Some(last) = self.segments.get(whole) {
            for at in 0..last.vectors.len() {
                last.values(at, self.dict.as_ref(), &mut vector)?;
                kept.append(&mut vector);
            }
        } else {
            for at in 0.. {
                if !self.open_values(at, &mut vector) {
                    break;
                }
                kept.append(&mut vector);
            }
        }
        kept.truncate(rows - whole * ROWS);
        self.segments.truncate(whole);
        self.open = None;
        self.rows = whole * ROWS;
        self.extend(kept.iter().map(Some))
    }

    /// The column of a segment of the open rows, for a copy of the column that has only segments.
    ///
    /// # Errors
    ///
    /// An error of the encoder, which is an internal error.
    pub fn open_segment(&self) -> Result<Option<Column>> {
        self.open.as_ref().map(|open| open.encode(0, self.dict.as_ref())).transpose()
    }

    /// The rows of the segment `segment`.
    #[must_use]
    pub fn segment_rows(&self, segment: usize) -> usize {
        self.rows.saturating_sub(segment * ROWS).min(ROWS)
    }

    /// The bytes that the column takes in memory, for the memory report.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.segments.iter().map(Column::encoded_len).sum::<usize>()
            + self.dict.as_ref().map_or(0, Dictionary::footprint)
            + self.open.as_ref().map_or(0, Builder::footprint)
    }
}

/// The column of a segment of the open rows `open`, which the decode of each vector checks against the open rows.
///
/// # Errors
///
/// `XX000` when a vector of the segment does not decode to the values of the open rows.
fn close(open: &Builder, dict: Option<&Dictionary>) -> Result<Column> {
    let column = open.encode(0, dict)?;
    let (mut want, mut got) = (Vec::new(), Vec::new());
    for at in 0..column.vectors.len() {
        open.values(0, at * VECTOR..((at + 1) * VECTOR).min(open.rows()), dict, &mut want);
        column.values(at, dict, &mut got)?;
        if want.len() != got.len() || want.iter().zip(&got).any(|(a, b)| !same(a, b)) {
            return Err(Error::internal("a segment does not keep the values of its rows"));
        }
    }
    Ok(column)
}

/// True when `a` and `b` are the same value, with the same bits for a float.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float4(x), Value::Float4(y)) => x.to_bits() == y.to_bits(),
        (Value::Float8(x), Value::Float8(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// The number of vectors of a segment with `rows` rows.
#[must_use]
pub fn vectors(rows: usize) -> usize {
    rows.div_ceil(VECTOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_column_keeps_its_rows_and_segments() {
        let values: Vec<Value> = (0..ROWS as i64 + 10).map(Value::Int8).collect();
        let stored = Stored::build(Kind::Int8, values.iter().map(Some)).unwrap();
        assert_eq!(stored.rows, ROWS + 10);
        assert_eq!(stored.segments.len(), 2);
        assert_eq!(stored.segment_rows(1), 10);
        let mut out = Vec::new();
        stored.segments[1].values(0, None, &mut out).unwrap();
        assert_eq!(out[9], Value::Int8(ROWS as i64 + 9));
        let text = [Value::Text("a".into())];
        let stored = Stored::build(Kind::Text, [Some(&text[0]), None].into_iter()).unwrap();
        stored.segments[0].values(0, stored.dict.as_ref(), &mut out).unwrap();
        assert_eq!(out, vec![Value::Text("a".into()), Value::Null]);
    }

    #[test]
    fn an_extended_column_is_the_column_of_all_its_values() {
        let values: Vec<Value> = (0..ROWS + 3000)
            .map(|i| if i % 5 == 0 { Value::Null } else { Value::Text(format!("v{}", i % 300)) })
            .collect();
        let whole = Stored::build(Kind::Text, values.iter().map(Some)).unwrap();
        let mut parts = Stored::build(Kind::Text, values[..1000].iter().map(Some)).unwrap();
        parts.extend(values[1000..ROWS - 7].iter().map(Some)).unwrap();
        parts.extend(values[ROWS - 7..].iter().map(Some)).unwrap();
        assert_eq!(parts.rows, whole.rows);
        // The whole segment is encoded, and the rows after it are open.
        assert_eq!(parts.segments.len(), 1);
        assert_eq!(parts.open_rows(), 3000);
        let mut open = Vec::new();
        assert!(parts.open_values(2, &mut open));
        assert_eq!(open, values[ROWS + 2048..ROWS + 3000]);
        assert!(!parts.open_values(3, &mut open));
        parts.seal().unwrap();
        assert!(parts.sealed());
        assert_eq!(parts.segments.len(), 2);
        assert_eq!(parts.segment_rows(0), ROWS);
        let mut got = Vec::new();
        let mut out = Vec::new();
        for column in &parts.segments {
            for at in 0..column.vectors.len() {
                column.values(at, parts.dict.as_ref(), &mut out).unwrap();
                got.append(&mut out);
            }
        }
        assert_eq!(got, values);
    }

    #[test]
    fn a_truncated_column_has_its_first_rows() {
        let values: Vec<Value> = (0..ROWS as i64 + 2500)
            .map(|i| if i % 9 == 0 { Value::Null } else { Value::Int4(i as i32) })
            .collect();
        for keep in [ROWS + 2000, ROWS, ROWS - 5, 700] {
            let mut stored = Stored::build(Kind::Int4, values[..3000].iter().map(Some)).unwrap();
            stored.extend(values[3000..].iter().map(Some)).unwrap();
            stored.truncate(keep).unwrap();
            assert_eq!(stored.rows, keep);
            stored.extend(values[keep..].iter().map(Some)).unwrap();
            stored.seal().unwrap();
            let mut got = Vec::new();
            let mut out = Vec::new();
            for column in &stored.segments {
                for at in 0..column.vectors.len() {
                    column.values(at, None, &mut out).unwrap();
                    got.append(&mut out);
                }
            }
            assert_eq!(got, values, "keep {keep}");
        }
    }

    #[test]
    fn a_column_is_as_it_was_after_an_extend_that_fails() {
        let values: Vec<Value> = (0..ROWS as i64 + 10).map(|i| Value::Int4(i as i32)).collect();
        let mut stored = Stored::build(Kind::Int4, values[..ROWS - 4].iter().map(Some)).unwrap();
        let mut bad = values[ROWS - 4..].to_vec();
        bad.push(Value::Text("x".into()));
        assert!(stored.extend(bad.iter().map(Some)).is_err());
        assert_eq!(
            (stored.rows, stored.segments.len(), stored.open_rows()),
            (ROWS - 4, 0, ROWS - 4)
        );
        stored.extend(values[ROWS - 4..].iter().map(Some)).unwrap();
        assert_eq!((stored.segments.len(), stored.open_rows()), (1, 10));
        let mut got = Vec::new();
        assert!(stored.open_values(0, &mut got));
        assert_eq!(got, values[ROWS..]);
    }
}

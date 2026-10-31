//! The values of one column of a table in segments, which a scan reads one vector at a time.
//!
//! All the columns of a table that the same rows fill have the same segments and the same vectors, so that the vector `v` of the segment `s` of each column holds the same rows.

use rupg_common::Result;
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
    /// The column of each segment, in row order. Each segment has [`ROWS`] rows, except the last one.
    pub segments: Vec<Column>,
    /// The rows of all the segments.
    pub rows: usize,
}

impl Stored {
    /// The column of the values `values`, in row order. `None` is a row with no value for the column, which reads as null.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of the kind.
    pub fn build<'a>(
        kind: Kind,
        values: impl Iterator<Item = Option<&'a Value>>,
    ) -> Result<Stored> {
        let mut stored = Stored { kind, dict: None, segments: Vec::new(), rows: 0 };
        stored.extend(values)?;
        Ok(stored)
    }

    /// Adds the values `values` after the rows of the column. A last segment with fewer than [`ROWS`] rows is made again with the new values after its own, so each segment but the last one keeps [`ROWS`] rows. The codes of the dictionary do not change.
    ///
    /// # Errors
    ///
    /// `XX000` when a value does not have the type of the kind, and `XX001` when the last segment does not decode. The column can then have lost the rows of its last segment.
    pub fn extend<'a>(&mut self, values: impl Iterator<Item = Option<&'a Value>>) -> Result<()> {
        let null = [Value::Null];
        let mut builder = Builder::new(&[self.kind]);
        let mut dicts = [self.dict.take()];
        let partial =
            self.segments.last().is_some_and(|_| self.segment_rows(self.segments.len() - 1) < ROWS);
        let result = (|| {
            if partial && let Some(last) = self.segments.pop() {
                let mut decoded = Vec::new();
                for at in 0..last.vectors.len() {
                    last.values(at, dicts[0].as_ref(), &mut decoded)?;
                    for value in &decoded {
                        builder.push(std::slice::from_ref(value), &mut dicts)?;
                    }
                }
                self.rows -= builder.rows();
            }
            for value in values {
                builder.push(value.map_or(&null[..], std::slice::from_ref), &mut dicts)?;
                if builder.rows() == ROWS {
                    self.segments.extend(builder.finish(&dicts)?.columns);
                    self.rows += ROWS;
                }
            }
            if builder.rows() > 0 {
                let rows = builder.rows();
                self.segments.extend(builder.finish(&dicts)?.columns);
                self.rows += rows;
            }
            Ok(())
        })();
        let [dict] = dicts;
        self.dict = dict;
        result
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
}

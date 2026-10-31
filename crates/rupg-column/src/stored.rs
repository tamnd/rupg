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
#[derive(Debug)]
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
        let null = [Value::Null];
        let mut builder = Builder::new(&[kind]);
        let mut dicts = [None];
        let mut segments = Vec::new();
        let mut rows = 0;
        for value in values {
            builder.push(value.map_or(&null[..], std::slice::from_ref), &mut dicts)?;
            rows += 1;
            if builder.rows() == ROWS {
                segments.extend(builder.finish(&dicts)?.columns);
            }
        }
        if builder.rows() > 0 {
            segments.extend(builder.finish(&dicts)?.columns);
        }
        let [dict] = dicts;
        Ok(Stored { kind, dict, segments, rows })
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
}

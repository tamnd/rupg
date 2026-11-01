//! The bytes of a stored column in the file: the encoded vectors, the null bitmaps, the summaries and the dictionary, as the encoder made them (spec/10 section 10.4.1). A load reads them back with no new encode.

use rupg_common::Result;
use rupg_common::persist::{Persist, Reader, corrupt, put_count};
use rupg_summary::column::ColumnSummary;

use crate::dict::Dictionary;
use crate::segment::{Column, Kind, Nulls, Vector, WORDS};
use crate::stored::Stored;

rupg_common::persist_enum!(Kind {
    0 => Bool,
    1 => Int2,
    2 => Int4,
    3 => Int8,
    4 => Float4,
    5 => Float8,
    6 => Date,
    7 => Time,
    8 => Timestamp,
    9 => TimestampTz,
    10 => Text,
    11 => Bytea,
});

/// Appends a byte string with its length.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_count(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// Reads a byte string that [`put_bytes`] wrote.
fn take_bytes<'a>(input: &mut Reader<'a>) -> Result<&'a [u8]> {
    let n = input.count()?;
    input.bytes(n)
}

impl Persist for Nulls {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            Nulls::None => out.push(0),
            Nulls::All => out.push(1),
            Nulls::Some(bits) => {
                out.push(2);
                for word in bits.iter() {
                    out.extend_from_slice(&word.to_le_bytes());
                }
            }
        }
    }

    fn take(input: &mut Reader<'_>) -> Result<Nulls> {
        match u8::take(input)? {
            0 => Ok(Nulls::None),
            1 => Ok(Nulls::All),
            2 => {
                let mut bits = Box::new([0u64; WORDS]);
                for word in bits.iter_mut() {
                    *word = u64::take(input)?;
                }
                Ok(Nulls::Some(bits))
            }
            _ => Err(corrupt("an unknown kind of null bitmap")),
        }
    }
}

impl Persist for Vector {
    fn put(&self, out: &mut Vec<u8>) {
        self.rows.put(out);
        put_bytes(out, &self.data);
        self.nulls.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Vector> {
        Ok(Vector {
            rows: Persist::take(input)?,
            data: take_bytes(input)?.into(),
            nulls: Persist::take(input)?,
        })
    }
}

impl Persist for Column {
    fn put(&self, out: &mut Vec<u8>) {
        self.kind.put(out);
        self.vectors.put(out);
        let mut summary = Vec::new();
        self.summary.write(&mut summary);
        put_bytes(out, &summary);
    }

    fn take(input: &mut Reader<'_>) -> Result<Column> {
        let kind = Persist::take(input)?;
        let vectors = Persist::take(input)?;
        let bytes = take_bytes(input)?;
        let (summary, used) = ColumnSummary::read(bytes)?;
        if used != bytes.len() {
            return Err(corrupt("bytes after the end of a segment summary"));
        }
        Ok(Column { kind, vectors, summary })
    }
}

impl Persist for Dictionary {
    fn put(&self, out: &mut Vec<u8>) {
        let (bytes, ends, base) = self.parts();
        put_bytes(out, bytes);
        put_count(out, ends.len());
        for end in ends {
            end.put(out);
        }
        base.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Dictionary> {
        let bytes = take_bytes(input)?.to_vec();
        let ends = Persist::take(input)?;
        let base = Persist::take(input)?;
        Dictionary::from_parts(bytes, ends, base)
    }
}

/// A column with open rows writes them as one more segment, so that the bytes have only segments. The store seals its columns before it writes them, so this encode is for a column that a caller did not seal.
impl Persist for Stored {
    fn put(&self, out: &mut Vec<u8>) {
        self.kind.put(out);
        self.dict.put(out);
        // The encoder fails only on an internal error, and then the open rows do not go to the bytes.
        let open = self.open_segment().ok().flatten();
        put_count(out, self.segments.len() + usize::from(open.is_some()));
        for column in self.segments.iter().chain(&open) {
            column.put(out);
        }
        let rows = if open.is_some() { self.rows } else { self.rows - self.open_rows() };
        rows.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Stored> {
        let kind = Persist::take(input)?;
        let dict = Persist::take(input)?;
        let segments = Persist::take(input)?;
        let rows = Persist::take(input)?;
        Ok(Stored::new(kind, dict, segments, rows))
    }
}

#[cfg(test)]
mod tests {
    use rupg_common::persist::{from_bytes, to_bytes};
    use rupg_types::Value;

    use super::*;
    use crate::segment::ROWS;

    /// The values of every row of `stored`.
    fn values(stored: &Stored) -> Vec<Value> {
        let mut all = Vec::new();
        let mut vector = Vec::new();
        for column in &stored.segments {
            for at in 0..column.vectors.len() {
                column.values(at, stored.dict.as_ref(), &mut vector).unwrap();
                all.append(&mut vector);
            }
        }
        all
    }

    #[test]
    fn a_stored_column_reads_back_with_the_same_values_and_summaries() {
        let rows = ROWS + 3000;
        let texts: Vec<Value> = (0..rows)
            .map(|i| match i % 7 {
                0 => Value::Null,
                _ => Value::Text(format!("http://example.com/{}", i % 5000)),
            })
            .collect();
        let ints: Vec<Value> = (0..rows)
            .map(|i| if i / 1024 == 3 { Value::Null } else { Value::Int8(i as i64 * 3 - 70) })
            .collect();
        let floats: Vec<Value> = (0..rows).map(|i| Value::Float8(i as f64 / 7.0)).collect();
        for (kind, column) in [(Kind::Text, &texts), (Kind::Int8, &ints), (Kind::Float8, &floats)] {
            let stored = Stored::build(kind, column.iter().map(Some)).unwrap();
            let back: Stored = from_bytes(&to_bytes(&stored)).unwrap();
            assert_eq!(back.kind, stored.kind);
            assert_eq!(back.rows, rows);
            assert_eq!(back.segments.len(), 2);
            for (a, b) in back.segments.iter().zip(&stored.segments) {
                assert_eq!(a.summary, b.summary);
                assert_eq!(a.vectors.len(), b.vectors.len());
            }
            assert_eq!(values(&back), *column);
            if let (Some(a), Some(b)) = (&back.dict, &stored.dict) {
                assert_eq!(a.len(), b.len());
                assert_eq!(a.base(), b.base());
                assert_eq!(a.find(b"http://example.com/4999"), b.find(b"http://example.com/4999"));
            }
        }
    }

    #[test]
    fn a_dictionary_with_bad_ends_does_not_load() {
        let mut dict = Dictionary::new();
        dict.intern(b"abc");
        dict.intern(b"de");
        let mut bytes = to_bytes(&dict);
        // The second end, 5, becomes 9, past the arena.
        let at = bytes.iter().rposition(|&b| b == 5).unwrap();
        bytes[at] = 9;
        assert!(from_bytes::<Dictionary>(&bytes).is_err());
    }
}

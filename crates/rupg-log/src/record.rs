//! The log record of spec/11 section 11.13.3.
//!
//! A record changes one row. The body of a block is a list of records. Each record holds its kind, the table OID as a delta from the OID of the record before it, the row id as a delta from the row id before it, the changed columns as a bitmap, and the new value of each changed column. The first record of a block takes its deltas from zero.

use std::fmt;

use rupg_common::{Error, Oid, Result, RowId};

use crate::varint;

/// The kind of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// A new row.
    Insert = 1,
    /// New values for some columns of a row.
    Update = 2,
    /// A row is deleted.
    Delete = 3,
    /// A change to a catalog row. It also makes the caches invalid.
    Catalog = 4,
    /// A table is truncated.
    Truncate = 5,
    /// A new cold segment, with its extents and its row id range.
    Segment = 6,
}

impl RecordKind {
    /// Each kind, in the order of the number.
    pub const ALL: [RecordKind; 6] = [
        RecordKind::Insert,
        RecordKind::Update,
        RecordKind::Delete,
        RecordKind::Catalog,
        RecordKind::Truncate,
        RecordKind::Segment,
    ];

    /// The kind with this number.
    pub fn from_u8(n: u8) -> Option<RecordKind> {
        RecordKind::ALL.into_iter().find(|&k| k as u8 == n)
    }

    /// The name in the text form.
    pub fn name(self) -> &'static str {
        match self {
            RecordKind::Insert => "insert",
            RecordKind::Update => "update",
            RecordKind::Delete => "delete",
            RecordKind::Catalog => "catalog",
            RecordKind::Truncate => "truncate",
            RecordKind::Segment => "segment",
        }
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One log record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The kind.
    pub kind: RecordKind,
    /// The table.
    pub table: Oid,
    /// The row. The shard is in the row id.
    pub row: RowId,
    /// The changed columns in increasing order, each with its new value in storage encoding, or `None` for NULL.
    pub values: Vec<(u16, Option<Vec<u8>>)>,
}

/// Writes the records of one block body.
#[derive(Debug, Default)]
pub struct RecordWriter {
    out: Vec<u8>,
    table: u32,
    row: u64,
}

impl RecordWriter {
    /// A writer with an empty body.
    pub fn new() -> RecordWriter {
        RecordWriter::default()
    }

    /// Appends a record. Columns that are not in increasing order give SQLSTATE `XX000`, and the body does not change.
    pub fn push(&mut self, r: &Record) -> Result<()> {
        if r.values.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(Error::internal(format!(
                "the columns of a {} record on table {} are not in increasing order",
                r.kind, r.table
            )));
        }
        let out = &mut self.out;
        out.push(r.kind as u8);
        varint::put_signed(out, i64::from(r.table.0.wrapping_sub(self.table) as i32));
        varint::put_signed(out, r.row.bits().wrapping_sub(self.row) as i64);
        let width = r.values.last().map_or(0, |v| usize::from(v.0) + 1);
        varint::put(out, width as u64);
        let map = out.len();
        out.resize(map + width.div_ceil(8), 0);
        for (column, value) in &r.values {
            out[map + usize::from(*column) / 8] |= 1 << (column % 8);
            match value {
                None => varint::put(out, 0),
                Some(v) => {
                    varint::put(out, v.len() as u64 + 1);
                    out.extend_from_slice(v);
                }
            }
        }
        self.table = r.table.0;
        self.row = r.row.bits();
        Ok(())
    }

    /// The size of the body in bytes.
    pub fn len(&self) -> usize {
        self.out.len()
    }

    /// True when the body holds no record.
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// The body. The writer starts again from zero deltas.
    pub fn take(&mut self) -> Vec<u8> {
        self.table = 0;
        self.row = 0;
        std::mem::take(&mut self.out)
    }
}

/// Reads the records of one block body. A body ends at its last record or at zero bytes, because a block pads its body with zero bytes and no record starts with zero.
#[derive(Debug)]
pub struct RecordReader<'a> {
    body: &'a [u8],
    at: usize,
    table: u32,
    row: u64,
}

impl<'a> RecordReader<'a> {
    /// A reader at the start of `body`.
    pub fn new(body: &'a [u8]) -> RecordReader<'a> {
        RecordReader { body, at: 0, table: 0, row: 0 }
    }

    fn read(&mut self) -> Result<Record> {
        let b = self.body;
        let at = &mut self.at;
        let kind = RecordKind::from_u8(b[*at])
            .ok_or_else(|| varint::bad(&format!("unknown record kind {}", b[*at])))?;
        *at += 1;
        let table = i32::try_from(varint::take_signed(b, at)?)
            .map_err(|_| varint::bad("the table delta is too large"))?;
        let table = self.table.wrapping_add(table as u32);
        let row = self.row.wrapping_add(varint::take_signed(b, at)? as u64);
        let width = varint::take(b, at)?;
        if width > u64::from(u16::MAX) + 1 {
            return Err(varint::bad("the column bitmap is too wide"));
        }
        let width = width as usize;
        let bytes = width.div_ceil(8);
        let map = b.get(*at..*at + bytes).ok_or_else(|| varint::bad("the bitmap does not fit"))?;
        *at += bytes;
        // The last column is set and no bit after it is set, so each record has one form.
        if let Some(last) = width.checked_sub(1)
            && map[bytes - 1] >> (last % 8) != 1
        {
            return Err(varint::bad("the column bitmap is not in its short form"));
        }
        let mut values = Vec::new();
        for column in 0..width {
            if map[column / 8] >> (column % 8) & 1 == 0 {
                continue;
            }
            let len = varint::take(b, at)?;
            let value = if len == 0 {
                None
            } else {
                let len =
                    usize::try_from(len - 1).map_err(|_| varint::bad("a value is too long"))?;
                let end = at.checked_add(len).filter(|&e| e <= b.len());
                let end = end.ok_or_else(|| varint::bad("a value does not fit"))?;
                let v = b[*at..end].to_vec();
                *at = end;
                Some(v)
            };
            values.push((column as u16, value));
        }
        self.table = table;
        self.row = row;
        Ok(Record { kind, table: Oid(table), row: RowId::from_bits(row), values })
    }
}

impl Iterator for RecordReader<'_> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Result<Record>> {
        if self.body.get(self.at).is_none_or(|&b| b == 0) {
            if self.body[self.at.min(self.body.len())..].iter().any(|&b| b != 0) {
                self.at = self.body.len();
                return Some(Err(varint::bad("the padding after the records is not zero")));
            }
            return None;
        }
        let r = self.read();
        if r.is_err() {
            self.at = self.body.len();
        }
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::{ShardId, SqlState};

    fn row(local: u64) -> RowId {
        RowId::new(ShardId(1), local).unwrap()
    }

    fn records() -> Vec<Record> {
        vec![
            Record {
                kind: RecordKind::Insert,
                table: Oid(16384),
                row: row(100),
                values: vec![(0, Some(b"abc".to_vec())), (1, None), (2, Some(Vec::new()))],
            },
            Record {
                kind: RecordKind::Update,
                table: Oid(16384),
                row: row(101),
                values: vec![(9, Some(vec![7; 300]))],
            },
            Record { kind: RecordKind::Delete, table: Oid(16384), row: row(5), values: Vec::new() },
            Record {
                kind: RecordKind::Catalog,
                table: Oid(1259),
                row: RowId::from_bits(u64::MAX),
                values: vec![(7, None), (65_535, Some(b"x".to_vec()))],
            },
            Record {
                kind: RecordKind::Truncate,
                table: Oid(u32::MAX),
                row: row(0),
                values: Vec::new(),
            },
            Record {
                kind: RecordKind::Segment,
                table: Oid(0),
                row: RowId::from_bits(0),
                values: vec![(8, Some(b"extents".to_vec()))],
            },
        ]
    }

    fn read(body: &[u8]) -> Result<Vec<Record>> {
        RecordReader::new(body).collect()
    }

    #[test]
    fn round_trip() {
        let mut w = RecordWriter::new();
        assert!(w.is_empty());
        for r in records() {
            w.push(&r).unwrap();
        }
        let len = w.len();
        let mut body = w.take();
        assert_eq!((body.len(), w.len()), (len, 0));
        assert_eq!(read(&body).unwrap(), records());
        // The second record costs one byte for its table and one for its row.
        let mut first = RecordWriter::new();
        first.push(&records()[0]).unwrap();
        let at = first.len();
        assert_eq!(&body[at..at + 4], [2, 0, 2, 10]);
        // The zero padding of a block ends the body.
        body.extend_from_slice(&[0; 7]);
        assert_eq!(read(&body).unwrap(), records());
        assert_eq!(read(&[]).unwrap(), []);

        // After take, the deltas start again from zero.
        w.push(&records()[1]).unwrap();
        assert_eq!(read(&w.take()).unwrap(), [records()[1].clone()]);
    }

    #[test]
    fn bad_records() {
        let mut w = RecordWriter::new();
        let mut r = records()[0].clone();
        r.values.swap(0, 1);
        assert_eq!(w.push(&r).unwrap_err().state(), SqlState::INTERNAL_ERROR);
        r.values = vec![(3, None), (3, None)];
        assert!(w.push(&r).is_err());
        assert!(w.is_empty());

        let mut w = RecordWriter::new();
        w.push(&records()[0]).unwrap();
        let body = w.take();
        // [kind, table, row, width, bitmap, values...]
        let cases: Vec<Vec<u8>> = vec![
            [&[9], &body[1..]].concat(),
            body[..body.len() - 1].to_vec(),
            [&body[..], &[0, 0, 5]].concat(),
            [&body[..6], &[0x0f], &body[7..]].concat(),
            [&body[..6], &[0x03], &body[7..]].concat(),
            [&body[..5], &[0x80, 0x80, 0x04]].concat(),
            [&body[..7], &[0xff, 0xff, 0x7f]].concat(),
        ];
        for (i, case) in cases.iter().enumerate() {
            let e = read(case).unwrap_err();
            assert_eq!(e.state(), SqlState::DATA_CORRUPTED, "case {i}: {e}");
        }
        let mut it = RecordReader::new(&cases[0]);
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }

    #[test]
    fn kinds() {
        for (i, k) in RecordKind::ALL.into_iter().enumerate() {
            assert_eq!(k as usize, i + 1);
            assert_eq!(RecordKind::from_u8(k as u8), Some(k));
        }
        assert_eq!(RecordKind::from_u8(0), None);
        assert_eq!(RecordKind::from_u8(7), None);
        assert_eq!(RecordKind::Truncate.to_string(), "truncate");
    }
}

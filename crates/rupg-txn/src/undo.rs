//! Undo records (spec/11 section 11.2.2).
//!
//! A change to a hot row writes one undo record. The record holds the version header, `xmin` and `xmax` that the row had before the change, and the old values of the columns that the change wrote. The row points to its newest record, and each record points to the record before it through the old header. A reader that cannot see the newest version applies records until it gets a version that it can see.
//!
//! At M1 the records are in one map in memory. Each transaction buffer, the 4 MiB budget and the spill to undo extents come at M3.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, RowId, Xid};
use rupg_hot::{Row, VersionHeader};

/// What a change did to the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// The change added the row. No version comes before it.
    Insert,
    /// The change wrote these columns. Each entry is a column number and its old value.
    Update(Vec<(usize, Option<Vec<u8>>)>),
    /// The change set the deleted flag.
    Delete,
}

/// One undo record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// The transaction that made the change.
    pub(crate) xid: Xid,
    /// The command number of the change.
    pub(crate) cid: u32,
    /// The row.
    pub(crate) id: RowId,
    /// The version header before the change.
    pub(crate) header: VersionHeader,
    /// `xmin` before the change.
    pub(crate) xmin: u32,
    /// `xmax` before the change.
    pub(crate) xmax: u32,
    /// What the change did.
    pub(crate) change: Change,
}

impl Record {
    /// The record of a change to `old`.
    pub(crate) fn of(xid: Xid, cid: u32, old: &Row, change: Change) -> Record {
        Record { xid, cid, id: old.id, header: old.header, xmin: old.xmin, xmax: old.xmax, change }
    }

    /// The version before the change, from `row`, the version that the change made. An insert has no version before it.
    pub(crate) fn undo(&self, mut row: Row) -> Option<Row> {
        match &self.change {
            Change::Insert => return None,
            Change::Update(old) => {
                for (column, value) in old {
                    row.values[*column] = value.clone();
                }
            }
            Change::Delete => {}
        }
        row.header = self.header;
        row.xmin = self.xmin;
        row.xmax = self.xmax;
        Some(row)
    }
}

/// The undo records of the node, by position.
#[derive(Debug)]
pub(crate) struct Undo {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    next: u64,
    records: BTreeMap<u64, Arc<Record>>,
}

impl Undo {
    pub(crate) fn new() -> Undo {
        Undo { inner: Mutex::new(Inner { next: 1, records: BTreeMap::new() }) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `record` and gives its position. Positions start at 1, because 0 in a header means no record. After 48 bits of positions, the result is SQLSTATE `XX000`.
    pub(crate) fn add(&self, record: Record) -> Result<u64> {
        let mut inner = self.lock();
        let at = inner.next;
        if at > VersionHeader::MAX_UNDO {
            return Err(Error::internal("the undo positions are used up"));
        }
        inner.next += 1;
        inner.records.insert(at, Arc::new(record));
        Ok(at)
    }

    /// The record at `at`, or `None` if it was removed.
    pub(crate) fn get(&self, at: u64) -> Option<Arc<Record>> {
        self.lock().records.get(&at).cloned()
    }

    /// Removes the records at `positions`.
    pub(crate) fn remove(&self, positions: &[u64]) {
        let mut inner = self.lock();
        for at in positions {
            inner.records.remove(at);
        }
    }

    /// The number of records.
    pub(crate) fn len(&self) -> usize {
        self.lock().records.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let xid = Xid::FIRST;
        let old = Row {
            id: RowId::from_bits(7),
            header: VersionHeader { stamp: 10, undo: 0, flags: 0 },
            xmin: 1,
            xmax: 0,
            values: vec![Some(vec![1]), None, Some(vec![3])],
        };
        let new = Row {
            header: VersionHeader::owned_by(xid.bits(), 1, 0),
            xmin: xid.xid32(),
            values: vec![Some(vec![9]), Some(vec![8]), Some(vec![3])],
            ..old.clone()
        };
        let update = Record::of(xid, 0, &old, Change::Update(vec![(0, Some(vec![1])), (1, None)]));
        assert_eq!(update.undo(new.clone()), Some(old.clone()));
        let delete = Record::of(xid, 0, &old, Change::Delete);
        let deleted = Row {
            header: VersionHeader { flags: VersionHeader::DELETED, ..new.header },
            ..old.clone()
        };
        assert_eq!(delete.undo(deleted), Some(old.clone()));
        assert_eq!(Record::of(xid, 0, &old, Change::Insert).undo(new), None);

        let undo = Undo::new();
        let a = undo.add(update.clone()).unwrap();
        let b = undo.add(delete).unwrap();
        assert_eq!((a, b, undo.len()), (1, 2, 2));
        assert_eq!(undo.get(a).as_deref(), Some(&update));
        undo.remove(&[a, 5]);
        assert_eq!((undo.get(a), undo.len()), (None, 1));
        undo.lock().next = VersionHeader::MAX_UNDO + 1;
        assert!(undo.add(update).is_err());
    }
}

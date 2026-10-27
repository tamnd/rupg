//! The large objects of `pg_largeobject_metadata` and their data in `pg_largeobject`, as `LargeObjectCreate` of `pg_largeobject.c` and `inv_api.c` change them. The data of a large object is a map of pages of `LOBLKSIZE` bytes, so that a snapshot of a large object is a cheap copy of the map.

use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_common::{Error, Result, SqlState};

use super::{Catalog, ObjRef, PG_LARGEOBJECT};

/// The OID of `pg_largeobject_metadata`.
pub const PG_LARGEOBJECT_METADATA: u32 = 2995;

/// `LOBLKSIZE`: the number of bytes of a page of `pg_largeobject`.
pub const LOBLKSIZE: usize = 2048;

/// `MAX_LARGE_OBJECT_SIZE`: the largest size of a large object.
pub const MAX_LARGE_OBJECT_SIZE: i64 = i32::MAX as i64 * LOBLKSIZE as i64;

/// The data of a large object: the pages of `pg_largeobject` by `pageno`. A page that is not in the map is a hole, which reads as zeros.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LargeObjectData(Arc<BTreeMap<i32, Arc<[u8]>>>);

impl LargeObjectData {
    /// The pages, in the order of `pageno`.
    pub fn pages(&self) -> impl Iterator<Item = (i32, &[u8])> {
        self.0.iter().map(|(&pageno, data)| (pageno, &**data))
    }

    /// `inv_getsize`: the end of the last page.
    pub fn size(&self) -> i64 {
        self.0.last_key_value().map_or(0, |(&pageno, data)| page_start(pageno) + data.len() as i64)
    }

    /// `inv_read`: at most `nbytes` bytes from `offset`. A hole reads as zeros, and the read stops at the end of the data.
    pub fn read(&self, offset: i64, nbytes: usize) -> Vec<u8> {
        let end = self.size().min(offset.saturating_add(nbytes as i64));
        if end <= offset {
            return Vec::new();
        }
        let mut out = vec![0; usize::try_from(end - offset).unwrap_or_default()];
        let first = page_of(offset);
        for (&pageno, data) in self.0.range(first..) {
            let start = page_start(pageno);
            if start >= end {
                break;
            }
            let from = (offset - start).max(0) as usize;
            let to = (data.len() as i64).min(end - start);
            if (from as i64) < to {
                let at = (start + from as i64 - offset) as usize;
                out[at..at + (to as usize - from)].copy_from_slice(&data[from..to as usize]);
            }
        }
        out
    }

    /// `inv_write`: writes `bytes` at `offset`. A page that the write reaches gets zeros up to the start of the write.
    pub fn write(&mut self, offset: i64, bytes: &[u8]) {
        let pages = Arc::make_mut(&mut self.0);
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as i64;
            let pageno = page_of(at);
            let off = (at - page_start(pageno)) as usize;
            let n = (LOBLKSIZE - off).min(bytes.len() - done);
            let mut page = pages.get(&pageno).map(|p| p.to_vec()).unwrap_or_default();
            if page.len() < off + n {
                page.resize(off + n, 0);
            }
            page[off..off + n].copy_from_slice(&bytes[done..done + n]);
            pages.insert(pageno, page.into());
            done += n;
        }
    }

    /// `inv_truncate`: the size becomes `len`. The pages after `len` go, and the page of `len` gets cut or filled with zeros up to `len`. That page stays also when it has no bytes, so the size is `len`.
    pub fn truncate(&mut self, len: i64) {
        let pageno = page_of(len);
        let off = (len - page_start(pageno)) as usize;
        let pages = Arc::make_mut(&mut self.0);
        let old = pages.split_off(&pageno);
        let mut page = old.get(&pageno).map(|p| p.to_vec()).unwrap_or_default();
        page.resize(off, 0);
        pages.insert(pageno, page.into());
    }
}

/// The page of a byte of a large object.
fn page_of(offset: i64) -> i32 {
    i32::try_from(offset / LOBLKSIZE as i64).unwrap_or(i32::MAX)
}

/// The offset of the first byte of a page.
fn page_start(pageno: i32) -> i64 {
    i64::from(pageno) * LOBLKSIZE as i64
}

/// A large object, a row of `pg_largeobject_metadata` with its rows of `pg_largeobject`. Its ACL is in the ACLs of the catalog, as for a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LargeObject {
    /// The OID.
    pub oid: u32,
    /// `lomowner`: the OID of the owner role.
    pub owner: u32,
    /// The data.
    pub data: LargeObjectData,
}

impl Catalog {
    /// `LargeObjectCreate`: a new large object with no data. An OID of 0 takes the next OID that no large object has.
    ///
    /// # Errors
    ///
    /// `23505` for an OID that a large object has.
    pub fn create_large_object(&mut self, oid: u32, owner: u32) -> Result<u32> {
        let oid = if oid == 0 {
            // `GetNewOidWithIndex`: an OID that no large object has.
            loop {
                let oid = self.new_oid();
                if !self.large_objects.contains_key(&oid) {
                    break oid;
                }
            }
        } else {
            oid
        };
        if self.large_objects.contains_key(&oid) {
            return Err(Error::new(
                SqlState::UNIQUE_VIOLATION,
                "duplicate key value violates unique constraint \"pg_largeobject_metadata_oid_index\"",
            )
            .with_detail(format!("Key (oid)=({oid}) already exists.")));
        }
        let new = LargeObject { oid, owner, data: LargeObjectData::default() };
        Arc::make_mut(&mut self.large_objects).insert(oid, new);
        Ok(oid)
    }

    /// The large objects, in the order of their OIDs.
    pub fn large_objects(&self) -> impl Iterator<Item = &LargeObject> {
        self.large_objects.values()
    }

    /// The large object with the OID.
    pub fn large_object(&self, oid: u32) -> Option<&LargeObject> {
        self.large_objects.get(&oid)
    }

    /// The large object with the OID, to change.
    pub fn large_object_mut(&mut self, oid: u32) -> Option<&mut LargeObject> {
        Arc::make_mut(&mut self.large_objects).get_mut(&oid)
    }

    /// `inv_drop`: removes the large object with its comment and its ACL.
    pub fn drop_large_object(&mut self, oid: u32) {
        Arc::make_mut(&mut self.large_objects).remove(&oid);
        let object = ObjRef::new(PG_LARGEOBJECT, oid);
        if self.descriptions.contains_key(&object) {
            Arc::make_mut(&mut self.descriptions).remove(&object);
        }
        if self.acls.contains_key(&object) {
            Arc::make_mut(&mut self.acls).remove(&object);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_with_holes() {
        let mut data = LargeObjectData::default();
        data.write(10, b"abc");
        assert_eq!(data.size(), 13);
        assert_eq!(data.read(8, 10), b"\0\0abc");
        data.write(5000, b"xy");
        assert_eq!(data.size(), 5002);
        assert_eq!(
            data.pages().map(|(p, d)| (p, d.len())).collect::<Vec<_>>(),
            [(0, 13), (2, 906)]
        );
        let read = data.read(0, 6000);
        assert_eq!(read.len(), 5002);
        assert_eq!(&read[5000..], b"xy");
        assert!(read[13..5000].iter().all(|&b| b == 0));
        data.write(2040, &[7; 20]);
        assert_eq!(
            data.pages().map(|(p, d)| (p, d.len())).collect::<Vec<_>>(),
            [(0, 2048), (1, 12), (2, 906)]
        );
    }

    #[test]
    fn truncate() {
        let mut data = LargeObjectData::default();
        data.write(0, &[1; 5000]);
        data.truncate(3000);
        assert_eq!(data.size(), 3000);
        data.truncate(4096);
        assert_eq!(data.size(), 4096);
        assert_eq!(
            data.pages().map(|(p, d)| (p, d.len())).collect::<Vec<_>>(),
            [(0, 2048), (1, 952), (2, 0)]
        );
        data.truncate(5000);
        assert_eq!(data.size(), 5000);
        assert!(data.read(3000, 2000).iter().all(|&b| b == 0));
        data.truncate(0);
        assert_eq!(data.size(), 0);
    }
}

//! The catalog page of M1.
//!
//! At M1 the catalog is one page of kind `Catalog`, which the header slot names. It holds the next transaction id, the next table OID, and for each table its OID, name, columns, hot store root and row id counter. Each checkpoint writes a new copy of the page out of place, so the copy that a slot names never changes. The catalog tree of `rupg-catalog` takes its place at M2.
//!
//! The body after the page header is little endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 4 | the layout version, 1 |
//! | 8 | the next transaction id |
//! | 4 | the next table OID |
//! | 4 | the table count |
//!
//! and for each table:
//!
//! | Bytes | Field |
//! |---|---|
//! | 4 | the OID |
//! | 8 | the root page of the hot store |
//! | 8 | the first local row id that no block holds |
//! | 2 + n | the name, as a length and UTF-8 bytes |
//! | 2 | the column count |
//!
//! and for each column a 4 byte type OID, a 1 byte nullable flag and the name as a length and bytes.

use rupg_buffer::{FileStore, Store};
use rupg_common::{Error, Hlc, Oid, Result, SqlState, Xid};
use rupg_file::{
    PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind, Root, seal, stored_checksum,
};
use rupg_table::{ColumnDef, TableDef};
use rupg_types::TypeId;

/// The layout version of the body.
const VERSION: u32 = 1;

/// One table of the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) def: TableDef,
    pub(crate) root: PageId,
    pub(crate) next_local: u64,
}

/// The content of the catalog page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Contents {
    pub(crate) next_xid: Xid,
    pub(crate) next_oid: Oid,
    pub(crate) tables: Vec<Entry>,
}

impl Contents {
    /// The catalog of a new file.
    pub(crate) fn new() -> Contents {
        Contents { next_xid: Xid::FIRST, next_oid: Oid::FIRST_USER, tables: Vec::new() }
    }

    /// The page of the catalog at logical page `page`, with page timestamp `ts`. A catalog that does not fit in one page gives SQLSTATE `54000`.
    pub(crate) fn encode(&self, page: PageId, ts: Hlc) -> Result<Box<Page>> {
        let mut body = Vec::new();
        body.extend_from_slice(&VERSION.to_le_bytes());
        body.extend_from_slice(&self.next_xid.bits().to_le_bytes());
        body.extend_from_slice(&self.next_oid.0.to_le_bytes());
        put_count(&mut body, self.tables.len(), 4)?;
        for t in &self.tables {
            body.extend_from_slice(&t.def.oid().0.to_le_bytes());
            body.extend_from_slice(&t.root.0.to_le_bytes());
            body.extend_from_slice(&t.next_local.to_le_bytes());
            put_name(&mut body, t.def.name())?;
            put_count(&mut body, t.def.columns().len(), 2)?;
            for c in t.def.columns() {
                body.extend_from_slice(&c.ty.0.0.to_le_bytes());
                body.push(u8::from(c.nullable));
                put_name(&mut body, &c.name)?;
            }
        }
        if body.len() > PAGE_SIZE - PAGE_HEADER_SIZE {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "the catalog does not fit in one page",
            )
            .with_detail(format!(
                "The catalog of {} tables needs {} bytes, and a page holds {}.",
                self.tables.len(),
                body.len(),
                PAGE_SIZE - PAGE_HEADER_SIZE
            ))
            .with_hint("At M1 the catalog is one page. Use fewer tables or shorter names."));
        }
        let mut out = Box::new([0u8; PAGE_SIZE]);
        let mut header = PageHeader::new(page, PageKind::Catalog);
        header.timestamp = ts;
        header.write(&mut out);
        out[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + body.len()].copy_from_slice(&body);
        Ok(out)
    }

    /// The catalog in `page`. A page that is not a catalog page or a body that does not decode gives SQLSTATE `XX001`.
    pub(crate) fn decode(page: &Page) -> Result<Contents> {
        let header = PageHeader::read(page)?;
        if header.kind != PageKind::Catalog {
            return Err(Error::corrupted(format!(
                "the catalog root, page {}, is a {:?} page",
                header.page, header.kind
            )));
        }
        let mut r = Reader { bytes: &page[PAGE_HEADER_SIZE..] };
        let version = r.u32()?;
        if version != VERSION {
            return Err(Error::corrupted(format!(
                "the catalog page has layout version {version}, and this version reads only {VERSION}"
            )));
        }
        let next_xid = Xid::from_bits(r.u64()?).ok_or_else(|| {
            Error::corrupted("the catalog page has a next transaction id that is not valid")
        })?;
        let next_oid = Oid(r.u32()?);
        let count = r.u32()?;
        let mut tables = Vec::new();
        for _ in 0..count {
            let oid = Oid(r.u32()?);
            let root = PageId(r.u64()?);
            let next_local = r.u64()?;
            if oid < Oid::FIRST_USER || oid >= next_oid || root.0 == 0 || next_local == 0 {
                return Err(Error::corrupted(format!(
                    "the catalog page has a table with OID {oid}, root page {root} and next row id {next_local}, and the next OID is {next_oid}"
                )));
            }
            let name = r.name()?;
            let mut columns = Vec::new();
            for _ in 0..r.u16()? {
                let ty = TypeId(Oid(r.u32()?));
                let nullable = r.u8()? != 0;
                columns.push(ColumnDef { name: r.name()?, ty, nullable });
            }
            let def = TableDef::new(oid, name, columns).map_err(|e| {
                Error::corrupted(format!(
                    "the catalog page has a table that is not valid: {}",
                    e.message()
                ))
            })?;
            tables.push(Entry { def, root, next_local });
        }
        Ok(Contents { next_xid, next_oid, tables })
    }

    /// Writes the catalog to `page` of `store` and gives the root for the header slot.
    pub(crate) fn write(&self, store: &FileStore, page: PageId, ts: Hlc) -> Result<Root> {
        let mut copy = self.encode(page, ts)?;
        let checksum = seal(&mut copy);
        store.write(page, &copy)?;
        Ok(Root { page: page.0, checksum })
    }

    /// Reads the catalog that `root` names from `store`. A page whose checksum is not the one in the slot gives SQLSTATE `XX001`.
    pub(crate) fn read(store: &FileStore, root: Root) -> Result<Contents> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        store.read(PageId(root.page), &mut page)?;
        if stored_checksum(&page) != root.checksum {
            return Err(Error::corrupted(format!(
                "the catalog page {} does not have the checksum that the header slot gives",
                root.page
            )));
        }
        Contents::decode(&page)
    }
}

fn put_count(out: &mut Vec<u8>, n: usize, width: usize) -> Result<()> {
    let n = u32::try_from(n).ok().filter(|&n| width == 4 || n <= u32::from(u16::MAX));
    let n = n.ok_or_else(|| {
        Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "the catalog has too many entries")
    })?;
    out.extend_from_slice(&n.to_le_bytes()[..width]);
    Ok(())
}

fn put_name(out: &mut Vec<u8>, name: &str) -> Result<()> {
    put_count(out, name.len(), 2)?;
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.bytes.len() < n {
            return Err(Error::corrupted("the catalog page ends before its last table"));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap_or_default()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap_or_default()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap_or_default()))
    }

    fn name(&mut self) -> Result<String> {
        let n = usize::from(self.u16()?);
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| Error::corrupted("the catalog page has a name that is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents() -> Contents {
        let col = |name: &str, ty, nullable| ColumnDef { name: name.into(), ty, nullable };
        let a = TableDef::new(
            Oid(16384),
            "accounts",
            vec![col("id", TypeId::INT8, false), col("owner", TypeId::TEXT, true)],
        )
        .unwrap();
        let b =
            TableDef::new(Oid(16385), "é", vec![col("at", TypeId::TIMESTAMPTZ, false)]).unwrap();
        Contents {
            next_xid: Xid::from_bits(1 << 33 | 9).unwrap(),
            next_oid: Oid(16386),
            tables: vec![
                Entry { def: a, root: PageId(7), next_local: 2049 },
                Entry { def: b, root: PageId(12), next_local: 1 },
            ],
        }
    }

    #[test]
    fn a_catalog_page_gives_back_its_contents() {
        let c = contents();
        let page = c.encode(PageId(3), Hlc::from_bits(77)).unwrap();
        assert_eq!(Contents::decode(&page).unwrap(), c);
        let header = PageHeader::read(&page).unwrap();
        assert_eq!(
            (header.page, header.kind, header.timestamp),
            (PageId(3), PageKind::Catalog, Hlc::from_bits(77))
        );
        assert_eq!(
            Contents::decode(&Contents::new().encode(PageId(3), Hlc::ZERO).unwrap()).unwrap(),
            Contents::new()
        );
    }

    #[test]
    fn a_damaged_catalog_page_gives_an_error() {
        let c = contents();
        let page = c.encode(PageId(3), Hlc::ZERO).unwrap();
        let mut short = page.clone();
        // The table count says 3, and the page has 2 tables.
        short[PAGE_HEADER_SIZE + 16] = 3;
        assert_eq!(Contents::decode(&short).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        let mut version = page.clone();
        version[PAGE_HEADER_SIZE] = 2;
        assert_eq!(Contents::decode(&version).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        let other = PageHeader::new(PageId(3), PageKind::HotLeaf);
        let mut leaf = page;
        other.write(&mut leaf);
        assert_eq!(Contents::decode(&leaf).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn a_catalog_that_does_not_fit_gives_54000() {
        let mut c = contents();
        let def = c.tables[0].def.clone();
        for i in 0..400 {
            let name = format!("{}{i}", "t".repeat(40));
            let def = TableDef::new(Oid(20000 + i), name, def.columns().to_vec()).unwrap();
            c.tables.push(Entry { def, root: PageId(9), next_local: 1 });
        }
        let err = c.encode(PageId(3), Hlc::ZERO).unwrap_err();
        assert_eq!(err.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
    }
}

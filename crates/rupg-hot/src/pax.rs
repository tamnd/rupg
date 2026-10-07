//! The PAX leaf of the hot store (spec/10 section 10.3.1).
//!
//! A leaf is a 16 KiB page of kind 6. The kind data holds the row count at bytes 0 and 1, the column count at bytes 2 and 3, the schema version at bytes 4 to 7, the offset of the `xmax` minipage at bytes 8 and 9 (0 when the leaf has none), and the offset of the column directory at bytes 10 and 11. The body follows the header in this order: the row ids, sorted, 8 bytes each; the version headers, 16 bytes each; the `xmin` minipage, 4 bytes each; the `xmax` minipage when a row has a deleter or a locker; the column directory; the minipage of each column; and the variable heap at the end of the page.
//!
//! A directory entry is 4 bytes: the offset of the minipage as a `u16`, the fixed width (0 for a variable column), and a flag byte with bit 0 set for a nullable column. A minipage starts with a null bitmap when the column is nullable, one bit for each row and set for NULL. A fixed column then holds its values as an array, with zero bytes for a NULL. A variable column holds the row count plus one `u16` offsets into the page, and value `i` is the bytes from offset `i` to offset `i + 1`.

use rupg_common::{Error, Result, RowId};
use rupg_file::{PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind};
use rupg_tree::Leaf;

use crate::row::{Row, VersionHeader};
use crate::schema::{Column, ROW_FIXED, Schema, Width};

const COUNT: usize = 40;
const COLUMNS: usize = 42;
const SCHEMA: usize = 44;
const XMAX: usize = 48;
const DIRECTORY: usize = 50;
const IDS: usize = PAGE_HEADER_SIZE;
const MAX_ROWS: usize = (PAGE_SIZE - PAGE_HEADER_SIZE) / ROW_FIXED;
const NULLABLE: u8 = 1;

fn u16_at(page: &Page, at: usize) -> usize {
    usize::from(u16::from_le_bytes([page[at], page[at + 1]]))
}

fn u32_at(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

fn put_u16(page: &mut Page, at: usize, v: usize) {
    page[at..at + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn page_id(page: &Page) -> PageId {
    PageId(u64::from_le_bytes(page[16..24].try_into().unwrap_or_default()))
}

fn corrupt(page: &Page, what: &str) -> Error {
    Error::corrupted(format!("the hot leaf {} {what}", page_id(page)))
}

/// The tree key of a row id: its bits in big endian, so that keys sort as row ids.
pub(crate) fn key(id: RowId) -> [u8; 8] {
    id.bits().to_be_bytes()
}

/// The row id of a tree key, or `None` for a key that is not 8 bytes.
pub(crate) fn id_of(key: &[u8]) -> Option<u64> {
    key.try_into().ok().map(u64::from_be_bytes)
}

/// A checked view of a leaf. Each offset that the view reads is inside the page.
#[derive(Debug)]
pub(crate) struct View<'p> {
    page: &'p Page,
    n: usize,
    c: usize,
    xmax: Option<usize>,
    dir: usize,
}

impl<'p> View<'p> {
    /// Checks the counts and the offsets of the sections. Bad bytes give SQLSTATE `XX001`.
    pub(crate) fn new(page: &'p Page) -> Result<View<'p>> {
        let (n, c) = (u16_at(page, COUNT), u16_at(page, COLUMNS));
        if n > MAX_ROWS || c > Schema::MAX_COLUMNS {
            return Err(corrupt(page, "has a bad row count or column count"));
        }
        let mut end = IDS + ROW_FIXED * n;
        let xmax = match u16_at(page, XMAX) {
            0 => None,
            at if at == end => {
                end += 4 * n;
                Some(at)
            }
            _ => return Err(corrupt(page, "has a bad xmax offset")),
        };
        let dir = u16_at(page, DIRECTORY);
        if dir != end || dir + 4 * c > PAGE_SIZE {
            return Err(corrupt(page, "has a bad column directory"));
        }
        Ok(View { page, n, c, xmax, dir })
    }

    pub(crate) fn len(&self) -> usize {
        self.n
    }

    pub(crate) fn schema(&self) -> u32 {
        u32_at(self.page, SCHEMA)
    }

    pub(crate) fn id(&self, i: usize) -> u64 {
        let at = IDS + 8 * i;
        u64::from_le_bytes(self.page[at..at + 8].try_into().unwrap_or_default())
    }

    /// The offset of the version header of row `i`.
    pub(crate) fn header_at(&self, i: usize) -> usize {
        IDS + 8 * self.n + VersionHeader::SIZE * i
    }

    pub(crate) fn header(&self, i: usize) -> VersionHeader {
        let at = self.header_at(i);
        VersionHeader::decode(
            self.page[at..at + VersionHeader::SIZE].try_into().unwrap_or(&[0; 16]),
        )
    }

    pub(crate) fn xmin(&self, i: usize) -> u32 {
        u32_at(self.page, IDS + (8 + VersionHeader::SIZE) * self.n + 4 * i)
    }

    /// The offset of the `xmax` of row `i`, or `None` when the leaf has no `xmax` minipage.
    pub(crate) fn xmax_at(&self, i: usize) -> Option<usize> {
        self.xmax.map(|at| at + 4 * i)
    }

    pub(crate) fn xmax(&self, i: usize) -> u32 {
        self.xmax_at(i).map_or(0, |at| u32_at(self.page, at))
    }

    /// The position of `id`, or the position where it goes.
    pub(crate) fn find(&self, id: u64) -> std::result::Result<usize, usize> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.id(mid).cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Column `j` and the offset of its minipage.
    fn column(&self, j: usize) -> Result<(Column, usize)> {
        let at = self.dir + 4 * j;
        let (off, width, flags) = (u16_at(self.page, at), self.page[at + 2], self.page[at + 3]);
        if flags & !NULLABLE != 0 {
            return Err(corrupt(self.page, "has a bad column flag"));
        }
        let width = if width == 0 { Width::Variable } else { Width::Fixed(width) };
        let column = Column { width, nullable: flags & NULLABLE != 0 };
        if off < self.dir + 4 * self.c || off + column.minipage(self.n) > PAGE_SIZE {
            return Err(corrupt(self.page, "has a bad minipage offset"));
        }
        Ok((column, off))
    }

    pub(crate) fn columns(&self) -> Result<Vec<Column>> {
        (0..self.c).map(|j| self.column(j).map(|(c, _)| c)).collect()
    }

    /// The value of row `i` in column `j`. `None` is NULL.
    pub(crate) fn value(&self, i: usize, j: usize) -> Result<Option<&'p [u8]>> {
        let (column, mut at) = self.column(j)?;
        if column.nullable {
            if self.page[at + i / 8] & (1 << (i % 8)) != 0 {
                return Ok(None);
            }
            at += self.n.div_ceil(8);
        }
        match column.width {
            Width::Fixed(w) => {
                let w = usize::from(w);
                Ok(Some(&self.page[at + w * i..at + w * (i + 1)]))
            }
            Width::Variable => {
                let (start, end) =
                    (u16_at(self.page, at + 2 * i), u16_at(self.page, at + 2 * i + 2));
                if start > end || end > PAGE_SIZE {
                    return Err(corrupt(self.page, "has a bad value offset"));
                }
                Ok(Some(&self.page[start..end]))
            }
        }
    }

    pub(crate) fn row(&self, i: usize) -> Result<Row> {
        let values = (0..self.c)
            .map(|j| self.value(i, j).map(|v| v.map(<[u8]>::to_vec)))
            .collect::<Result<_>>()?;
        Ok(Row {
            id: RowId::from_bits(self.id(i)),
            header: self.header(i),
            xmin: self.xmin(i),
            xmax: self.xmax(i),
            values,
        })
    }

    /// Checks that the row ids go up and that each value can be read.
    pub(crate) fn check(&self) -> Result<()> {
        for i in 0..self.n {
            if i > 0 && self.id(i - 1) >= self.id(i) {
                return Err(corrupt(self.page, "has row ids out of order"));
            }
            for j in 0..self.c {
                self.value(i, j)?;
            }
        }
        Ok(())
    }
}

/// The values of one column, in row order.
#[derive(Clone, Debug, Default)]
struct Data {
    nulls: Vec<bool>,
    /// The values. A fixed column has `width` bytes for each row, with zero bytes for a NULL.
    bytes: Vec<u8>,
    /// For a variable column, the end of each value in `bytes`.
    ends: Vec<usize>,
}

impl Data {
    fn span(&self, column: &Column, i: usize) -> (usize, usize) {
        match column.width {
            Width::Fixed(w) => (usize::from(w) * i, usize::from(w) * (i + 1)),
            Width::Variable => (if i == 0 { 0 } else { self.ends[i - 1] }, self.ends[i]),
        }
    }

    fn get(&self, column: &Column, i: usize) -> Option<&[u8]> {
        let (start, end) = self.span(column, i);
        (!self.nulls[i]).then(|| &self.bytes[start..end])
    }

    fn insert(&mut self, column: &Column, i: usize, value: Option<&[u8]>) {
        self.nulls.insert(i, value.is_none());
        match column.width {
            Width::Fixed(w) => {
                let w = usize::from(w);
                let zero = vec![0; w];
                self.bytes.splice(w * i..w * i, value.unwrap_or(&zero).iter().copied());
            }
            Width::Variable => {
                let start = if i == 0 { 0 } else { self.ends[i - 1] };
                let value = value.unwrap_or(&[]);
                self.bytes.splice(start..start, value.iter().copied());
                self.ends.insert(i, start);
                for end in &mut self.ends[i..] {
                    *end += value.len();
                }
            }
        }
    }

    fn remove(&mut self, column: &Column, i: usize) {
        let (start, end) = self.span(column, i);
        self.nulls.remove(i);
        self.bytes.drain(start..end);
        if column.width == Width::Variable {
            self.ends.remove(i);
            for e in &mut self.ends[i..] {
                *e -= end - start;
            }
        }
    }

    fn split_off(&mut self, column: &Column, at: usize) -> Data {
        let cut = if at == self.nulls.len() { self.bytes.len() } else { self.span(column, at).0 };
        let mut ends =
            if column.width == Width::Variable { self.ends.split_off(at) } else { Vec::new() };
        for e in &mut ends {
            *e -= cut;
        }
        Data { nulls: self.nulls.split_off(at), bytes: self.bytes.split_off(cut), ends }
    }
}

/// The rows of a leaf in columns, for a change that moves rows.
#[derive(Clone, Debug)]
pub(crate) struct Pax {
    schema: u32,
    columns: Vec<Column>,
    ids: Vec<u64>,
    headers: Vec<VersionHeader>,
    xmin: Vec<u32>,
    xmax: Vec<u32>,
    data: Vec<Data>,
}

impl Pax {
    pub(crate) fn empty(schema: &Schema) -> Pax {
        Pax {
            schema: schema.version,
            columns: schema.columns.clone(),
            ids: Vec::new(),
            headers: Vec::new(),
            xmin: Vec::new(),
            xmax: Vec::new(),
            data: vec![Data::default(); schema.columns.len()],
        }
    }

    pub(crate) fn decode(page: &Page) -> Result<Pax> {
        let v = View::new(page)?;
        let columns = v.columns()?;
        let mut pax = Pax::empty(&Schema { version: v.schema(), columns });
        for i in 0..v.len() {
            if i > 0 && v.id(i - 1) >= v.id(i) {
                return Err(corrupt(page, "has row ids out of order"));
            }
            pax.ids.push(v.id(i));
            pax.headers.push(v.header(i));
            pax.xmin.push(v.xmin(i));
            pax.xmax.push(v.xmax(i));
            for (j, (column, data)) in pax.columns.iter().zip(&mut pax.data).enumerate() {
                data.insert(column, i, v.value(i, j)?);
            }
        }
        Ok(pax)
    }

    /// Reads the leaf for a write with `schema`. An empty leaf takes the schema. A leaf with rows of another schema gives SQLSTATE `XX000`, because no schema change reaches the hot store before M2.
    pub(crate) fn for_write(page: &Page, schema: &Schema) -> Result<Pax> {
        let pax = Pax::decode(page)?;
        if pax.ids.is_empty() {
            return Ok(Pax::empty(schema));
        }
        if pax.schema != schema.version || pax.columns != schema.columns {
            return Err(Error::internal(format!(
                "the hot leaf {} holds rows of schema version {} and the write has version {}",
                page_id(page),
                pax.schema,
                schema.version
            )));
        }
        Ok(pax)
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn find(&self, id: RowId) -> std::result::Result<usize, usize> {
        self.ids.binary_search(&id.bits())
    }

    pub(crate) fn row(&self, i: usize) -> Row {
        Row {
            id: RowId::from_bits(self.ids[i]),
            header: self.headers[i],
            xmin: self.xmin[i],
            xmax: self.xmax[i],
            values: self
                .columns
                .iter()
                .zip(&self.data)
                .map(|(c, d)| d.get(c, i).map(<[u8]>::to_vec))
                .collect(),
        }
    }

    /// Adds `row` at position `i`. The row must match the schema of the leaf.
    pub(crate) fn insert(&mut self, i: usize, row: &Row) {
        self.ids.insert(i, row.id.bits());
        self.headers.insert(i, row.header);
        self.xmin.insert(i, row.xmin);
        self.xmax.insert(i, row.xmax);
        for ((column, data), value) in self.columns.iter().zip(&mut self.data).zip(&row.values) {
            data.insert(column, i, value.as_deref());
        }
    }

    pub(crate) fn remove(&mut self, i: usize) -> Row {
        let row = self.row(i);
        self.ids.remove(i);
        self.headers.remove(i);
        self.xmin.remove(i);
        self.xmax.remove(i);
        for (column, data) in self.columns.iter().zip(&mut self.data) {
            data.remove(column, i);
        }
        row
    }

    pub(crate) fn set_xmax(&mut self, i: usize, xmax: u32) {
        self.xmax[i] = xmax;
    }

    /// Moves the rows from position `at` to a new leaf.
    fn split_off(&mut self, at: usize) -> Pax {
        let data =
            self.columns.iter().zip(&mut self.data).map(|(c, d)| d.split_off(c, at)).collect();
        Pax {
            schema: self.schema,
            columns: self.columns.clone(),
            ids: self.ids.split_off(at),
            headers: self.headers.split_off(at),
            xmin: self.xmin.split_off(at),
            xmax: self.xmax.split_off(at),
            data,
        }
    }

    fn has_xmax(&self) -> bool {
        self.xmax.iter().any(|&x| x != 0)
    }

    /// The bytes that the leaf takes, with the page header.
    pub(crate) fn size(&self) -> usize {
        let n = self.len();
        let heap: usize =
            self.columns.iter().zip(&self.data).map(|(c, d)| c.minipage(n) + heap_of(c, d)).sum();
        IDS + ROW_FIXED * n
            + if self.has_xmax() { 4 * n } else { 0 }
            + 4 * self.columns.len()
            + heap
    }

    pub(crate) fn fits(&self) -> bool {
        self.size() <= PAGE_SIZE
    }

    /// The bytes of row `i` on a leaf, about. A split uses it to cut at the middle of the bytes.
    fn weight(&self, i: usize) -> usize {
        let values: usize = self
            .columns
            .iter()
            .zip(&self.data)
            .map(|(c, d)| match c.width {
                Width::Fixed(w) => usize::from(w),
                Width::Variable => {
                    let (start, end) = d.span(c, i);
                    2 + end - start
                }
            })
            .sum();
        ROW_FIXED + 4 + values
    }

    /// Writes the leaf with the fields of `header` other than the kind, the kind data, `lower` and `upper`. The leaf must fit.
    pub(crate) fn encode(&self, page: &mut Page, mut header: PageHeader) -> Result<()> {
        if !self.fits() {
            return Err(Error::internal(format!("{} rows do not fit in a hot leaf", self.len())));
        }
        let n = self.len();
        page.fill(0);
        let mut at = IDS;
        for id in &self.ids {
            page[at..at + 8].copy_from_slice(&id.to_le_bytes());
            at += 8;
        }
        for h in &self.headers {
            page[at..at + VersionHeader::SIZE].copy_from_slice(&h.encode()?);
            at += VersionHeader::SIZE;
        }
        for x in &self.xmin {
            page[at..at + 4].copy_from_slice(&x.to_le_bytes());
            at += 4;
        }
        let xmax = if self.has_xmax() { at } else { 0 };
        if self.has_xmax() {
            for x in &self.xmax {
                page[at..at + 4].copy_from_slice(&x.to_le_bytes());
                at += 4;
            }
        }
        let dir = at;
        at += 4 * self.columns.len();
        let heap: usize = self.columns.iter().zip(&self.data).map(|(c, d)| heap_of(c, d)).sum();
        let mut cursor = PAGE_SIZE - heap;
        for (j, (column, data)) in self.columns.iter().zip(&self.data).enumerate() {
            let entry = dir + 4 * j;
            put_u16(page, entry, at);
            page[entry + 2] = match column.width {
                Width::Fixed(w) => w,
                Width::Variable => 0,
            };
            page[entry + 3] = if column.nullable { NULLABLE } else { 0 };
            if column.nullable {
                for (i, _) in data.nulls.iter().enumerate().filter(|(_, null)| **null) {
                    page[at + i / 8] |= 1 << (i % 8);
                }
                at += n.div_ceil(8);
            }
            match column.width {
                Width::Fixed(_) => {
                    page[at..at + data.bytes.len()].copy_from_slice(&data.bytes);
                    at += data.bytes.len();
                }
                Width::Variable => {
                    put_u16(page, at, cursor);
                    for (i, end) in data.ends.iter().enumerate() {
                        put_u16(page, at + 2 * (i + 1), cursor + end);
                    }
                    page[cursor..cursor + data.bytes.len()].copy_from_slice(&data.bytes);
                    cursor += data.bytes.len();
                    at += 2 * (n + 1);
                }
            }
        }
        header.kind = PageKind::HotLeaf;
        header.lower = at as u16;
        header.upper = (PAGE_SIZE - heap) as u16;
        header.kind_data = [0; 24];
        header.kind_data[0..2].copy_from_slice(&(n as u16).to_le_bytes());
        header.kind_data[2..4].copy_from_slice(&(self.columns.len() as u16).to_le_bytes());
        header.kind_data[4..8].copy_from_slice(&self.schema.to_le_bytes());
        header.kind_data[8..10].copy_from_slice(&(xmax as u16).to_le_bytes());
        header.kind_data[10..12].copy_from_slice(&(dir as u16).to_le_bytes());
        header.write(page);
        Ok(())
    }
}

fn heap_of(column: &Column, data: &Data) -> usize {
    match column.width {
        Width::Fixed(_) => 0,
        Width::Variable => data.bytes.len(),
    }
}

/// The leaf format of the hot store for `rupg-tree`.
#[derive(Debug)]
pub struct HotLeaf;

impl Leaf for HotLeaf {
    const INNER: PageKind = PageKind::HotInner;
    const LEAF: PageKind = PageKind::HotLeaf;

    fn init(page: &mut Page, id: PageId) {
        let empty = Pax::empty(&Schema { version: 0, columns: Vec::new() });
        // An empty leaf always fits.
        let _ = empty.encode(page, PageHeader::new(id, PageKind::HotLeaf));
    }

    /// A key above each row of the leaf is an append. The leaf then keeps all of its rows, and the new row starts the new leaf, so leaves that take rows in row id order are full. A leaf with one row gives the row to the new leaf. Otherwise the cut is at the middle of the bytes.
    fn split(left: &mut Page, right: &mut Page, key: &[u8]) -> Result<Vec<u8>> {
        let base = PageHeader::read(left)?;
        let right_id = PageHeader::read(right)?.page;
        let mut pax = Pax::decode(left)?;
        let n = pax.len();
        let k = id_of(key).ok_or_else(|| Error::internal("a hot store key is not 8 bytes"))?;
        let at = if pax.ids.last().is_none_or(|&last| k > last) {
            n
        } else if n == 1 && k < pax.ids[0] {
            // Two rows that do not fit together. The row moves to the right leaf, and the key goes to the left leaf.
            0
        } else if n == 1 {
            return Err(Error::internal(format!(
                "the hot leaf {} has 1 row and cannot split",
                base.page
            )));
        } else {
            let total: usize = (0..n).map(|i| pax.weight(i)).sum();
            let mut acc = 0;
            let mut at = n - 1;
            for i in 0..n {
                acc += pax.weight(i);
                if acc * 2 >= total {
                    at = i + 1;
                    break;
                }
            }
            at.clamp(1, n - 1)
        };
        let high = pax.split_off(at);
        pax.encode(left, base)?;
        high.encode(right, PageHeader { page: right_id, ..base })?;
        Ok(match high.ids.first() {
            Some(&first) => first.to_be_bytes().to_vec(),
            None => key.to_vec(),
        })
    }

    fn bounds(page: &Page) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let v = View::new(page)?;
        v.check()?;
        Ok(match v.len() {
            0 => None,
            n => Some((v.id(0).to_be_bytes().to_vec(), v.id(n - 1).to_be_bytes().to_vec())),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rupg_platform::sim::SimRng;

    use super::*;

    pub(crate) fn schema() -> Schema {
        let columns = vec![
            Column { width: Width::Fixed(8), nullable: false },
            Column { width: Width::Fixed(4), nullable: true },
            Column { width: Width::Variable, nullable: true },
            Column { width: Width::Variable, nullable: false },
        ];
        Schema::new(3, columns).unwrap()
    }

    pub(crate) fn random_row(rng: &mut SimRng, id: u64) -> Row {
        let mut bytes = |n: u64| (0..n).map(|_| rng.below(256) as u8).collect::<Vec<u8>>();
        let a = Some(bytes(8));
        let b = Some(bytes(4));
        let c = Some(bytes(40));
        let d = Some(bytes(8));
        let mut row = Row {
            id: RowId::from_bits(id),
            header: VersionHeader {
                stamp: rng.below(u64::MAX),
                undo: rng.below(1 << 48),
                flags: rng.below(256) as u16,
            },
            xmin: rng.below(1 << 32) as u32,
            xmax: if rng.below(4) == 0 { rng.below(1 << 32) as u32 } else { 0 },
            values: vec![a, b, c, d],
        };
        if rng.below(3) == 0 {
            row.values[1] = None;
        }
        match rng.below(3) {
            0 => row.values[2] = None,
            1 => row.values[2] = Some(Vec::new()),
            _ => row.values[2].as_mut().unwrap().truncate(rng.below(41) as usize),
        }
        row
    }

    fn leaf(pax: &Pax) -> Box<Page> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        pax.encode(&mut page, PageHeader::new(PageId(5), PageKind::HotLeaf)).unwrap();
        page
    }

    #[test]
    fn rows_round_trip() {
        let schema = schema();
        let mut rng = SimRng::new(1);
        let mut pax = Pax::empty(&schema);
        let mut rows = Vec::new();
        let mut id = 10;
        loop {
            let row = random_row(&mut rng, id);
            schema.check(&row).unwrap();
            pax.insert(pax.len(), &row);
            if !pax.fits() {
                pax.remove(pax.len() - 1);
                break;
            }
            rows.push(row);
            id += 1 + rng.below(3);
        }
        assert!(rows.len() > 100, "{}", rows.len());
        let page = leaf(&pax);
        let header = PageHeader::read(&page).unwrap();
        assert_eq!((header.page, header.kind), (PageId(5), PageKind::HotLeaf));
        assert_eq!(usize::from(header.lower) + PAGE_SIZE - usize::from(header.upper), pax.size());
        let v = View::new(&page).unwrap();
        v.check().unwrap();
        assert_eq!((v.len(), v.schema()), (rows.len(), 3));
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(&v.row(i).unwrap(), row);
            assert_eq!(&pax.row(i), row);
            assert_eq!(v.find(row.id.bits()), Ok(i));
        }
        assert_eq!(v.find(0), Err(0));
        let again = Pax::decode(&page).unwrap();
        assert_eq!(*leaf(&again), *page);

        // Remove rows from the middle and from the ends.
        let mut pax = again;
        for i in [rows.len() - 1, 50, 0, 7] {
            assert_eq!(pax.remove(i), rows.remove(i));
        }
        let page = leaf(&pax);
        let v = View::new(&page).unwrap();
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(&v.row(i).unwrap(), row);
        }

        // A leaf with no xmax has no xmax minipage.
        let mut pax = Pax::empty(&schema);
        let mut row = random_row(&mut rng, 1);
        row.xmax = 0;
        pax.insert(0, &row);
        let small = pax.size();
        pax.set_xmax(0, 9);
        assert_eq!(pax.size(), small + 4);
        let page = leaf(&pax);
        assert_eq!(View::new(&page).unwrap().xmax(0), 9);
    }

    #[test]
    fn splits() {
        let schema = schema();
        let mut rng = SimRng::new(2);
        let mut pax = Pax::empty(&schema);
        for id in 0..100 {
            pax.insert(pax.len(), &random_row(&mut rng, id * 2));
        }
        let rows: Vec<Row> = (0..100).map(|i| pax.row(i)).collect();
        let full = leaf(&pax);

        // A key in the middle cuts at the middle of the bytes.
        let mut left = full.clone();
        let mut right = Box::new([0u8; PAGE_SIZE]);
        HotLeaf::init(&mut right, PageId(6));
        let sep = HotLeaf::split(&mut left, &mut right, &key(RowId::from_bits(51))).unwrap();
        let (l, r) = (View::new(&left).unwrap(), View::new(&right).unwrap());
        assert_eq!(l.len() + r.len(), 100);
        assert!(l.len() > 30 && r.len() > 30, "{} {}", l.len(), r.len());
        assert_eq!(id_of(&sep), Some(r.id(0)));
        assert!(l.id(l.len() - 1) < r.id(0));
        assert_eq!(PageHeader::read(&right).unwrap().page, PageId(6));
        let back: Vec<Row> = (0..l.len())
            .map(|i| l.row(i).unwrap())
            .chain((0..r.len()).map(|i| r.row(i).unwrap()))
            .collect();
        assert_eq!(back, rows);

        // A key above each row keeps the leaf whole.
        let mut left = full.clone();
        HotLeaf::init(&mut right, PageId(6));
        let sep = HotLeaf::split(&mut left, &mut right, &key(RowId::from_bits(1_000))).unwrap();
        assert_eq!((id_of(&sep), View::new(&left).unwrap().len()), (Some(1_000), 100));
        assert_eq!(HotLeaf::bounds(&right).unwrap(), None);
        assert_eq!(
            HotLeaf::bounds(&left).unwrap(),
            Some((key(rows[0].id).to_vec(), key(rows[99].id).to_vec()))
        );

        // A big last row does not take the cut to the end of the leaf.
        let mut big = Pax::empty(&schema);
        for (i, len) in [0, 0, 9_000].into_iter().enumerate() {
            let mut row = random_row(&mut rng, 2 * i as u64);
            row.values[2] = Some(vec![1; len]);
            big.insert(i, &row);
        }
        let mut left = leaf(&big);
        HotLeaf::init(&mut right, PageId(6));
        let sep = HotLeaf::split(&mut left, &mut right, &key(RowId::from_bits(3))).unwrap();
        assert_eq!((View::new(&left).unwrap().len(), View::new(&right).unwrap().len()), (2, 1));
        assert_eq!(id_of(&sep), Some(4));

        // A leaf with one row gives the row to the right leaf when the key is below it.
        let mut one = Pax::empty(&schema);
        one.insert(0, &rows[1]);
        let mut left = leaf(&one);
        HotLeaf::init(&mut right, PageId(6));
        let sep = HotLeaf::split(&mut left, &mut right, &key(RowId::from_bits(1))).unwrap();
        assert_eq!((id_of(&sep), View::new(&left).unwrap().len()), (Some(2), 0));
        assert_eq!(View::new(&right).unwrap().row(0).unwrap(), rows[1]);
        assert!(HotLeaf::split(&mut left, &mut right, &[1]).is_err());
    }

    /// Random changes to the bytes of a leaf give an error or a leaf that reads, and never a panic.
    #[test]
    fn bad_bytes() {
        let schema = schema();
        let mut rng = SimRng::new(3);
        let mut pax = Pax::empty(&schema);
        for id in 0..60 {
            pax.insert(pax.len(), &random_row(&mut rng, id));
        }
        let good = leaf(&pax);
        let mut errors = 0;
        for round in 0..3_000 {
            let mut page = good.clone();
            for _ in 0..1 + rng.below(4) {
                // Most changes go to the kind data and the start of the body, where the offsets are.
                let at = if round % 2 == 0 {
                    40 + rng.below(1_200)
                } else {
                    rng.below(PAGE_SIZE as u64)
                };
                page[at as usize] = rng.below(256) as u8;
            }
            let read = View::new(&page).and_then(|v| {
                v.check()?;
                (0..v.len()).try_for_each(|i| v.row(i).map(drop))
            });
            let decode = Pax::decode(&page);
            errors += usize::from(read.is_err());
            assert_eq!(read.is_ok(), decode.is_ok(), "round {round}");
            if let Err(e) = read {
                assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED);
            }
        }
        assert!(errors > 100, "{errors}");

        // Counts and offsets that the random changes seldom make.
        let mut cases: Vec<Box<Page>> = Vec::new();
        let mut page = good.clone();
        // 500 rows and 1,000 columns put the directory past the end of the page.
        put_u16(&mut page, COUNT, 500);
        put_u16(&mut page, COLUMNS, 1_000);
        put_u16(&mut page, XMAX, 0);
        put_u16(&mut page, DIRECTORY, IDS + ROW_FIXED * 500);
        cases.push(page);
        // A minipage at offset 0 is in the page header.
        let mut page = good.clone();
        let dir = u16_at(&page, DIRECTORY);
        put_u16(&mut page, dir, 0);
        cases.push(page);
        // An xmax minipage that is not after the xmin minipage.
        let mut page = good.clone();
        put_u16(&mut page, XMAX, 64);
        cases.push(page);
        for (i, page) in cases.iter().enumerate() {
            let read = View::new(page).and_then(|v| v.check());
            assert_eq!(
                read.map_err(|e| e.state()),
                Err(rupg_common::SqlState::DATA_CORRUPTED),
                "case {i}"
            );
        }
    }
}

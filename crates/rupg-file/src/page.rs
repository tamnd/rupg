//! The 64-byte page header and the page checks (spec/08 section 8.3).

use std::fmt;
use std::str::FromStr;

use rupg_common::{Error, Hlc, Oid, Result, ShardId};

use crate::le::{get, put, put_u16, put_u32, put_u64, u8_at, u16_at, u32_at, u64_at};
use crate::text::{TextIn, TextOut};
use crate::{PAGE_SIZE, Page, PageId, checksum};

/// The size of the page header.
pub const PAGE_HEADER_SIZE: usize = 64;

/// The kind of a page. The comment of each kind names the crate that owns its layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PageKind {
    /// A page table node. rupg-file.
    PageTable = 1,
    /// A free space map page. rupg-file.
    FreeSpace = 2,
    /// The log ring directory. rupg-log.
    RingDirectory = 3,
    /// A catalog tree node. rupg-catalog.
    Catalog = 4,
    /// A hot store inner node. rupg-hot.
    HotInner = 5,
    /// A hot store leaf in PAX form. rupg-hot.
    HotLeaf = 6,
    /// Undo that did not fit in memory. rupg-txn.
    UndoSpill = 7,
    /// Delete marks of a segment. rupg-column.
    DeleteMarks = 8,
    /// A segment directory. rupg-column.
    SegmentDirectory = 9,
    /// A TOAST chunk. rupg-table.
    Toast = 10,
    /// A B-tree node. rupg-index.
    BTree = 11,
    /// A hash, GIN, GiST, SP-GiST or BRIN node. rupg-index.
    OtherIndex = 12,
    /// Graph adjacency. rupg-graph.
    Graph = 13,
    /// A vector graph node. rupg-ann.
    VectorGraph = 14,
    /// Temporary spill. rupg-exec.
    TempSpill = 15,
    /// The shard map. rupg-cluster.
    ShardMap = 16,
    /// A page of the pending free list. rupg-file.
    FreeList = 17,
}

impl PageKind {
    /// Each kind, in the order of the number.
    pub const ALL: [PageKind; 17] = [
        PageKind::PageTable,
        PageKind::FreeSpace,
        PageKind::RingDirectory,
        PageKind::Catalog,
        PageKind::HotInner,
        PageKind::HotLeaf,
        PageKind::UndoSpill,
        PageKind::DeleteMarks,
        PageKind::SegmentDirectory,
        PageKind::Toast,
        PageKind::BTree,
        PageKind::OtherIndex,
        PageKind::Graph,
        PageKind::VectorGraph,
        PageKind::TempSpill,
        PageKind::ShardMap,
        PageKind::FreeList,
    ];

    /// The kind with this number.
    pub fn from_u8(n: u8) -> Option<PageKind> {
        PageKind::ALL.into_iter().find(|&k| k as u8 == n)
    }

    /// The name in the text form.
    pub fn name(self) -> &'static str {
        match self {
            PageKind::PageTable => "page_table",
            PageKind::FreeSpace => "free_space",
            PageKind::RingDirectory => "ring_directory",
            PageKind::Catalog => "catalog",
            PageKind::HotInner => "hot_inner",
            PageKind::HotLeaf => "hot_leaf",
            PageKind::UndoSpill => "undo_spill",
            PageKind::DeleteMarks => "delete_marks",
            PageKind::SegmentDirectory => "segment_directory",
            PageKind::Toast => "toast",
            PageKind::BTree => "btree",
            PageKind::OtherIndex => "other_index",
            PageKind::Graph => "graph",
            PageKind::VectorGraph => "vector_graph",
            PageKind::TempSpill => "temp_spill",
            PageKind::ShardMap => "shard_map",
            PageKind::FreeList => "free_list",
        }
    }
}

impl fmt::Display for PageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for PageKind {
    type Err = ();

    fn from_str(s: &str) -> Result<PageKind, ()> {
        PageKind::ALL.into_iter().find(|k| k.name() == s).ok_or(())
    }
}

/// The flags of a page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PageFlags(pub u16);

const FLAG_NAMES: [(PageFlags, &str); 3] = [
    (PageFlags::ENCRYPTED, "encrypted"),
    (PageFlags::UNLOGGED, "unlogged"),
    (PageFlags::TEMPORARY, "temporary"),
];

impl PageFlags {
    /// No flag.
    pub const NONE: PageFlags = PageFlags(0);
    /// Bit 0, the page is encrypted (spec/08 section 8.11).
    pub const ENCRYPTED: PageFlags = PageFlags(1);
    /// Bit 1, the page belongs to an unlogged table.
    pub const UNLOGGED: PageFlags = PageFlags(1 << 1);
    /// Bit 2, the page belongs to a temporary table.
    pub const TEMPORARY: PageFlags = PageFlags(1 << 2);
    /// The bits that have a meaning.
    pub const KNOWN: PageFlags = PageFlags(0b111);

    /// True if each bit of `other` is set.
    pub fn contains(self, other: PageFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for PageFlags {
    type Output = PageFlags;

    fn bitor(self, other: PageFlags) -> PageFlags {
        PageFlags(self.0 | other.0)
    }
}

impl fmt::Display for PageFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> =
            FLAG_NAMES.iter().filter(|(flag, _)| self.contains(*flag)).map(|(_, n)| *n).collect();
        if names.is_empty() { f.write_str("none") } else { f.write_str(&names.join(",")) }
    }
}

impl FromStr for PageFlags {
    type Err = ();

    fn from_str(s: &str) -> Result<PageFlags, ()> {
        if s == "none" {
            return Ok(PageFlags::NONE);
        }
        s.split(',').try_fold(PageFlags::NONE, |flags, name| {
            let (flag, _) = FLAG_NAMES.iter().find(|(_, n)| *n == name).ok_or(())?;
            Ok(flags | *flag)
        })
    }
}

/// The header of a 16 KiB page, without the checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageHeader {
    /// The HLC value of the newest log record applied to the page. Recovery uses it to skip records that the page already has.
    pub timestamp: Hlc,
    /// The logical page number, the page's own identity. A read checks it, so a misdirected write is found.
    pub page: PageId,
    /// The object that owns the page, 0 for system structures.
    pub table: Oid,
    /// The shard of the page.
    pub shard: ShardId,
    /// The kind of the page.
    pub kind: PageKind,
    /// The layout version of the kind.
    pub kind_version: u8,
    /// The flags.
    pub flags: PageFlags,
    /// The end of the slot area, for kinds with slots.
    pub lower: u16,
    /// The start of the heap area, for kinds with a heap.
    pub upper: u16,
    /// Sibling links, the tree level and other fields of the kind.
    pub kind_data: [u8; 24],
}

impl PageHeader {
    /// A header with a zero timestamp, no owner, shard 0, kind version 1, no flags, and `lower` and `upper` at the ends of the free area.
    pub fn new(page: PageId, kind: PageKind) -> PageHeader {
        PageHeader {
            timestamp: Hlc::ZERO,
            page,
            table: Oid::INVALID,
            shard: ShardId(0),
            kind,
            kind_version: 1,
            flags: PageFlags::NONE,
            lower: PAGE_HEADER_SIZE as u16,
            upper: PAGE_SIZE as u16 - 1,
            kind_data: [0; 24],
        }
    }

    /// Writes bytes 8 to 63 of the page. The checksum is written by [`seal`].
    pub fn write(&self, page: &mut Page) {
        put_u64(page, 8, self.timestamp.bits());
        put_u64(page, 16, self.page.0);
        put_u32(page, 24, self.table.0);
        put_u16(page, 28, self.shard.0);
        page[30] = self.kind as u8;
        page[31] = self.kind_version;
        put_u16(page, 32, self.flags.0);
        put_u16(page, 34, self.lower);
        put_u16(page, 36, self.upper);
        put_u16(page, 38, 0);
        put(page, 40, &self.kind_data);
    }

    /// Reads the header. A kind or a flag that this rupg does not know gives SQLSTATE `XX001`. The checksum is not checked here, see [`verify`].
    pub fn read(page: &Page) -> Result<PageHeader> {
        let kind = u8_at(page, 30);
        let Some(kind) = PageKind::from_u8(kind) else {
            return Err(Error::corrupted(format!("the page has the unknown kind {kind}")));
        };
        let flags = PageFlags(u16_at(page, 32));
        if !PageFlags::KNOWN.contains(flags) {
            return Err(Error::corrupted(format!(
                "the page has the unknown flags {:#06x}",
                flags.0
            )));
        }
        Ok(PageHeader {
            timestamp: Hlc::from_bits(u64_at(page, 8)),
            page: PageId(u64_at(page, 16)),
            table: Oid(u32_at(page, 24)),
            shard: ShardId(u16_at(page, 28)),
            kind,
            kind_version: u8_at(page, 31),
            flags,
            lower: u16_at(page, 34),
            upper: u16_at(page, 36),
            kind_data: get(page, 40),
        })
    }

    /// Adds the header fields to a text form, with the kind data as hexadecimal bytes.
    pub fn put(&self, out: &mut TextOut) {
        self.put_common(out);
        out.bytes("kind_data", &self.kind_data);
    }

    /// Adds the header fields to a text form, without the kind data. A kind that gives a meaning to the kind data uses this and adds its own fields.
    pub fn put_common(&self, out: &mut TextOut) {
        out.field("flags", self.flags)
            .field("kind", self.kind)
            .field("kind_version", self.kind_version)
            .field("lower", self.lower)
            .field("page", self.page)
            .field("shard", self.shard)
            .field("table", self.table)
            .field("timestamp", self.timestamp)
            .field("upper", self.upper);
    }

    /// Takes the header fields that [`PageHeader::put`] wrote.
    pub fn take(input: &mut TextIn<'_>) -> Result<PageHeader> {
        let mut header = PageHeader::take_common(input)?;
        header.kind_data = input.take_bytes("kind_data")?;
        Ok(header)
    }

    /// Takes the header fields that [`PageHeader::put_common`] wrote. The kind data is zero.
    pub fn take_common(input: &mut TextIn<'_>) -> Result<PageHeader> {
        Ok(PageHeader {
            timestamp: input.take("timestamp")?,
            page: input.take("page")?,
            table: input.take("table")?,
            shard: ShardId(input.take("shard")?),
            kind: input.take("kind")?,
            kind_version: input.take("kind_version")?,
            flags: input.take("flags")?,
            lower: input.take("lower")?,
            upper: input.take("upper")?,
            kind_data: [0; 24],
        })
    }

    /// The text form of the header alone.
    pub fn to_text(&self) -> String {
        let mut out = TextOut::new();
        self.put(&mut out);
        out.finish()
    }

    /// Reads the text form of the header alone.
    pub fn from_text(text: &str) -> Result<PageHeader> {
        let mut input = TextIn::parse("page header", text)?;
        let header = PageHeader::take(&mut input)?;
        input.finish()?;
        Ok(header)
    }
}

/// The write tag of a page table entry: the low 16 bits of the page timestamp (spec/08 section 8.5).
pub fn write_tag(timestamp: Hlc) -> u16 {
    (timestamp.bits() & 0xffff) as u16
}

/// Writes the checksum of bytes 8 to 16383 into bytes 0 to 7, and gives it. A header slot root holds this value.
pub fn seal(page: &mut Page) -> u64 {
    let sum = checksum(&page[8..]);
    put_u64(page, 0, sum);
    sum
}

/// The checksum in bytes 0 to 7 of a page.
pub fn stored_checksum(page: &Page) -> u64 {
    u64_at(page, 0)
}

/// Checks a page read from the physical page `physical`, and gives its header. Each failure gives SQLSTATE `XX001` and names the physical and the logical page.
///
/// The checks are in this order. The checksum must match. Then the logical page number must be `expected`, or the write went to the wrong place. Then, if `tag` is given, the low 16 bits of the page timestamp must equal it, or the page is an older copy and a write was lost. Then the kind and the flags must be known.
pub fn verify(
    page: &Page,
    physical: u64,
    expected: PageId,
    tag: Option<u16>,
) -> Result<PageHeader> {
    let at = || format!("Physical page {physical}, logical page {expected}.");
    let sum = checksum(&page[8..]);
    if sum != stored_checksum(page) {
        return Err(Error::corrupted("the page has a bad checksum").with_detail(format!(
            "{} The stored checksum is {:#018x} and the computed checksum is {sum:#018x}.",
            at(),
            stored_checksum(page)
        )));
    }
    let found = PageId(u64_at(page, 16));
    if found != expected {
        return Err(Error::corrupted("misdirected write: the page holds a different logical page")
            .with_detail(format!("{} The page holds logical page {found}.", at())));
    }
    if let Some(tag) = tag {
        let has = write_tag(Hlc::from_bits(u64_at(page, 8)));
        if has != tag {
            return Err(Error::corrupted(
                "lost write: the page is older than the page table entry",
            )
            .with_detail(format!(
                "{} The page table expects write tag {tag} and the page has {has}.",
                at()
            )));
        }
    }
    PageHeader::read(page).map_err(|e| {
        let message = e.message().to_string();
        e.with_detail(format!("{} {message}.", at()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    fn sample() -> PageHeader {
        PageHeader {
            timestamp: Hlc::new(123_456, 7).unwrap(),
            page: PageId(99),
            table: Oid(16_385),
            shard: ShardId(2),
            kind: PageKind::HotLeaf,
            kind_version: 1,
            flags: PageFlags::UNLOGGED | PageFlags::TEMPORARY,
            lower: 64,
            upper: 16_000,
            kind_data: [5; 24],
        }
    }

    #[test]
    fn kinds() {
        for (i, kind) in PageKind::ALL.into_iter().enumerate() {
            assert_eq!(kind as usize, i + 1);
            assert_eq!(PageKind::from_u8(kind as u8), Some(kind));
            assert_eq!(kind.name().parse(), Ok(kind));
        }
        assert_eq!(PageKind::from_u8(0), None);
        assert_eq!(PageKind::from_u8(18), None);
        assert_eq!("encrypted,temporary".parse(), Ok(PageFlags(0b101)));
        assert_eq!(PageFlags(0b101).to_string(), "encrypted,temporary");
        assert_eq!("none".parse(), Ok(PageFlags::NONE));
        assert_eq!("dirty".parse::<PageFlags>(), Err(()));
    }

    #[test]
    fn round_trip() {
        let header = sample();
        let mut page = [0u8; PAGE_SIZE];
        header.write(&mut page);
        assert_eq!(page[30], 6);
        assert_eq!(u16_at(&page, 32), 0b110);
        let sum = seal(&mut page);
        assert_eq!(stored_checksum(&page), sum);
        assert_eq!(
            verify(&page, 40, PageId(99), Some(write_tag(header.timestamp))).unwrap(),
            header
        );
        let text = header.to_text();
        assert!(
            text.starts_with("flags unlogged,temporary\nkind hot_leaf\nkind_data 0505"),
            "{text}"
        );
        assert_eq!(PageHeader::from_text(&text).unwrap(), header);
        let fresh = PageHeader::new(PageId(1), PageKind::PageTable);
        assert_eq!(PageHeader::from_text(&fresh.to_text()).unwrap(), fresh);
    }

    #[test]
    fn checks() {
        let header = sample();
        let mut page = [0u8; PAGE_SIZE];
        header.write(&mut page);
        seal(&mut page);
        let tag = write_tag(header.timestamp);
        let message = |r: Result<PageHeader>| {
            let e = r.unwrap_err();
            assert_eq!(e.state(), SqlState::DATA_CORRUPTED);
            e.message().to_string()
        };

        // A page that was never written.
        let zero = [0u8; PAGE_SIZE];
        assert_eq!(message(verify(&zero, 40, PageId(99), None)), "the page has a bad checksum");

        let mut flipped = page;
        flipped[PAGE_SIZE - 1] ^= 0x80;
        assert_eq!(message(verify(&flipped, 40, PageId(99), None)), "the page has a bad checksum");

        let e = verify(&page, 40, PageId(98), None).unwrap_err();
        assert!(e.message().starts_with("misdirected write"));
        assert_eq!(
            e.detail(),
            Some("Physical page 40, logical page 98. The page holds logical page 99.")
        );

        assert!(
            message(verify(&page, 40, PageId(99), Some(tag.wrapping_add(1))))
                .starts_with("lost write")
        );

        let mut odd = page;
        odd[30] = 200;
        seal(&mut odd);
        assert_eq!(
            message(verify(&odd, 40, PageId(99), None)),
            "the page has the unknown kind 200"
        );
        odd[30] = 6;
        put_u16(&mut odd, 32, 0x8000);
        seal(&mut odd);
        assert_eq!(
            message(verify(&odd, 40, PageId(99), None)),
            "the page has the unknown flags 0x8000"
        );
    }

    /// The checksum covers each byte after the checksum field, including the reserved bytes and the end of the page.
    #[test]
    fn checksum_covers_the_page() {
        let mut page = [0u8; PAGE_SIZE];
        sample().write(&mut page);
        seal(&mut page);
        for at in [8, 38, 39, 63, 64, 8191, PAGE_SIZE - 1] {
            let mut changed = page;
            changed[at] ^= 1;
            assert!(verify(&changed, 1, PageId(99), None).is_err(), "byte {at}");
        }
    }
}

//! The pages of the log ring region of spec/08 section 8.7: the ring directory and the extent list of each ring.
//!
//! rupg-log owns the blocks inside a ring. The two page kinds here are in the format crate, because the open path and the file store must check every page that a header slot names.

use rupg_common::{Error, Hlc, Result, SqlState};

use crate::le::{put_u16, put_u32, put_u64, u16_at, u32_at, u64_at};
use crate::text::{TextIn, TextOut};
use crate::{
    ARENA_PAGES, FIRST_ARENA_PAGE, MAX_PHYSICAL, PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader,
    PageId, PageKind, PtEntry, Root, seal,
};

/// The size of one entry of the ring directory.
pub const RING_ENTRY_SIZE: usize = 96;

/// The entries in one ring directory page.
pub const RINGS_PER_PAGE: usize = 168;

/// The extents in one ring extent list page. A ring can have at most this many E4 extents, which is about 31 GiB.
pub const RING_EXTENTS_PER_PAGE: usize = (PAGE_SIZE - PAGE_HEADER_SIZE) / 8;

/// The size of one ring extent, an E4 extent of 16 MiB.
pub const RING_EXTENT_BYTES: u64 = ARENA_PAGES * PAGE_SIZE as u64;

/// One entry of the ring directory: the state of one ring at a checkpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RingEntry {
    /// The ring number.
    pub ring: u32,
    /// The worker that used the ring at the time of the checkpoint.
    pub worker: u32,
    /// The ring size in bytes, a multiple of [`RING_EXTENT_BYTES`].
    pub size: u64,
    /// The ring position where redo starts.
    pub redo: u64,
    /// The durable position at the checkpoint.
    pub durable: u64,
    /// The newest durable HLC value at the checkpoint.
    pub newest: Hlc,
    /// The extent list page of the ring and its checksum.
    pub extents: Root,
}

impl RingEntry {
    /// The number of extents that the size gives.
    pub fn extent_count(&self) -> u64 {
        self.size / RING_EXTENT_BYTES
    }

    fn problem(&self) -> Option<String> {
        let count = self.extent_count();
        if self.size == 0 || !self.size.is_multiple_of(RING_EXTENT_BYTES) {
            Some(format!("ring {} has the size {}", self.ring, self.size))
        } else if count > RING_EXTENTS_PER_PAGE as u64 {
            Some(format!("ring {} has {count} extents", self.ring))
        } else if self.redo > self.durable || self.durable - self.redo > self.size {
            Some(format!(
                "ring {} has the redo position {} and the durable position {}",
                self.ring, self.redo, self.durable
            ))
        } else if !(FIRST_ARENA_PAGE..=MAX_PHYSICAL).contains(&self.extents.page) {
            Some(format!("ring {} has the extent list page {}", self.ring, self.extents.page))
        } else {
            None
        }
    }

    fn encode(&self, out: &mut [u8]) {
        out[..RING_ENTRY_SIZE].fill(0);
        put_u32(out, 0, self.ring);
        put_u32(out, 4, self.worker);
        put_u64(out, 8, self.size);
        put_u64(out, 16, self.redo);
        put_u64(out, 24, self.durable);
        put_u64(out, 32, self.newest.bits());
        put_u64(out, 40, self.extents.page);
        put_u64(out, 48, self.extents.checksum);
    }

    fn decode(bytes: &[u8]) -> Result<RingEntry> {
        let entry = RingEntry {
            ring: u32_at(bytes, 0),
            worker: u32_at(bytes, 4),
            size: u64_at(bytes, 8),
            redo: u64_at(bytes, 16),
            durable: u64_at(bytes, 24),
            newest: Hlc::from_bits(u64_at(bytes, 32)),
            extents: Root { page: u64_at(bytes, 40), checksum: u64_at(bytes, 48) },
        };
        if bytes[56..RING_ENTRY_SIZE].iter().any(|&b| b != 0) {
            return Err(Error::corrupted(format!(
                "the ring directory entry of ring {} has reserved bytes that are not zero",
                entry.ring
            )));
        }
        match entry.problem() {
            Some(p) => Err(Error::corrupted(format!("the ring directory is damaged: {p}"))),
            None => Ok(entry),
        }
    }

    fn to_text(self) -> String {
        format!(
            "ring {} worker {} size {} redo {} durable {} newest {} extents {}",
            self.ring, self.worker, self.size, self.redo, self.durable, self.newest, self.extents
        )
    }

    fn from_text(text: &str) -> Option<RingEntry> {
        let words: Vec<&str> = text.split(' ').collect();
        let names = ["ring", "worker", "size", "redo", "durable", "newest", "extents"];
        if words.len() != 15 || names.iter().enumerate().any(|(i, n)| words[2 * i] != *n) {
            return None;
        }
        Some(RingEntry {
            ring: words[1].parse().ok()?,
            worker: words[3].parse().ok()?,
            size: words[5].parse().ok()?,
            redo: words[7].parse().ok()?,
            durable: words[9].parse().ok()?,
            newest: words[11].parse().ok()?,
            extents: format!("{} {}", words[13], words[14]).parse().ok()?,
        })
    }
}

/// A page of the ring directory. The header holds its physical page number in the logical page field.
///
/// The kind data holds the count of entries at bytes 8 and 9 and the next page of the directory at bytes 16 to 23, in the format of a page table entry. The entries follow the header in increasing order of the ring number.
#[derive(Clone, Copy, Debug)]
pub struct RingDirectoryPage;

impl RingDirectoryPage {
    /// Sets up a page at `physical` that holds `entries`, with `next` as the next page of the directory. The entries must be valid and in increasing order of the ring number, and there can be at most [`RINGS_PER_PAGE`]. Other input gives SQLSTATE `XX000`. The caller seals the page before it writes it.
    pub fn init(
        page: &mut Page,
        physical: u64,
        entries: &[RingEntry],
        next: PtEntry,
    ) -> Result<()> {
        if entries.len() > RINGS_PER_PAGE {
            return Err(Error::internal(format!(
                "{} rings do not fit in one ring directory page",
                entries.len()
            )));
        }
        if let Some(p) = entry_problem(entries) {
            return Err(Error::internal(format!("bad ring directory entry: {p}")));
        }
        page.fill(0);
        let mut header = PageHeader::new(PageId(physical), PageKind::RingDirectory);
        put_u16(&mut header.kind_data, 8, entries.len() as u16);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.lower = PAGE_SIZE as u16 - 1;
        header.upper = PAGE_SIZE as u16 - 1;
        header.write(page);
        for (i, e) in entries.iter().enumerate() {
            let at = PAGE_HEADER_SIZE + i * RING_ENTRY_SIZE;
            e.encode(&mut page[at..at + RING_ENTRY_SIZE]);
        }
        Ok(())
    }

    /// The count of entries in the page.
    pub fn count(page: &Page) -> u16 {
        u16_at(page, 48)
    }

    /// The entry of the next page of the directory, or the empty entry for the last page.
    pub fn next(page: &Page) -> PtEntry {
        PtEntry::from_bits(u64_at(page, 56))
    }

    /// The entries. A count above [`RINGS_PER_PAGE`], a bad entry, or ring numbers that do not increase give SQLSTATE `XX001`.
    pub fn entries(page: &Page) -> Result<Vec<RingEntry>> {
        let count = usize::from(Self::count(page));
        if count > RINGS_PER_PAGE {
            return Err(Error::corrupted(format!(
                "the ring directory page has the count {count}, above {RINGS_PER_PAGE}"
            )));
        }
        let entries = (0..count)
            .map(|i| {
                let at = PAGE_HEADER_SIZE + i * RING_ENTRY_SIZE;
                RingEntry::decode(&page[at..at + RING_ENTRY_SIZE])
            })
            .collect::<Result<Vec<_>>>()?;
        match entry_problem(&entries) {
            Some(p) => Err(Error::corrupted(format!("the ring directory is damaged: {p}"))),
            None => Ok(entries),
        }
    }

    /// The text form: the page header without the kind data, `count`, `next`, and one `ring_NNN` line for each entry.
    pub fn to_text(page: &Page) -> Result<String> {
        let header = PageHeader::read(page)?;
        let entries = Self::entries(page)?;
        let mut out = TextOut::new();
        header.put_common(&mut out);
        out.field("count", entries.len()).field("next", Self::next(page));
        for (i, e) in entries.iter().enumerate() {
            out.field(&format!("ring_{i:03}"), e.to_text());
        }
        Ok(out.finish())
    }

    /// Reads the text form back into a sealed page.
    pub fn from_text(text: &str) -> Result<Box<Page>> {
        let mut input = TextIn::parse("ring directory page", text)?;
        let mut header = PageHeader::take_common(&mut input)?;
        if header.kind != PageKind::RingDirectory {
            return Err(bad("ring directory", format!("the kind is {}", header.kind)));
        }
        let count: usize = input.take("count")?;
        if count > RINGS_PER_PAGE {
            return Err(bad(
                "ring directory",
                format!("the count {count} is above {RINGS_PER_PAGE}"),
            ));
        }
        let next: PtEntry = input.take("next")?;
        let mut entries = vec![None; count];
        for (i, value) in input.take_prefixed("ring_") {
            let n = i.parse::<usize>().ok().filter(|&n| n < count && i.len() == 3);
            let (Some(n), Some(e)) = (n, RingEntry::from_text(value)) else {
                return Err(bad(
                    "ring directory",
                    format!("ring_{i} has the bad value \"{value}\""),
                ));
            };
            entries[n] = Some(e);
        }
        let entries: Vec<RingEntry> = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| e.ok_or_else(|| bad("ring directory", format!("ring_{i:03} is missing"))))
            .collect::<Result<_>>()?;
        input.finish()?;
        let mut page = Box::new([0u8; PAGE_SIZE]);
        Self::init(&mut page, header.page.0, &entries, next)
            .map_err(|e| bad("ring directory", e.message().to_string()))?;
        // The other header fields come from the text.
        put_u16(&mut header.kind_data, 8, count as u16);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.write(&mut page);
        seal(&mut page);
        Ok(page)
    }
}

/// A page with the extents of one ring, in ring order. The header holds its physical page number in the logical page field.
///
/// The kind data holds the ring number at bytes 0 to 3 and the count of extents at bytes 8 and 9. The first physical page of each E4 extent follows the header. Each extent is a whole arena, and no extent is in the list two times.
#[derive(Clone, Copy, Debug)]
pub struct RingExtentsPage;

impl RingExtentsPage {
    /// Sets up a page at `physical` with the extents of `ring`. Bad input gives SQLSTATE `XX000`. The caller seals the page before it writes it.
    pub fn init(page: &mut Page, physical: u64, ring: u32, extents: &[u64]) -> Result<()> {
        if extents.len() > RING_EXTENTS_PER_PAGE {
            return Err(Error::internal(format!(
                "{} extents do not fit in one ring extent list page",
                extents.len()
            )));
        }
        if let Some(p) = extent_problem(extents) {
            return Err(Error::internal(format!("bad extent of ring {ring}: {p}")));
        }
        page.fill(0);
        let mut header = PageHeader::new(PageId(physical), PageKind::RingExtents);
        put_u32(&mut header.kind_data, 0, ring);
        put_u16(&mut header.kind_data, 8, extents.len() as u16);
        header.lower = PAGE_SIZE as u16 - 1;
        header.upper = PAGE_SIZE as u16 - 1;
        header.write(page);
        for (i, &start) in extents.iter().enumerate() {
            put_u64(page, PAGE_HEADER_SIZE + i * 8, start);
        }
        Ok(())
    }

    /// The ring number.
    pub fn ring(page: &Page) -> u32 {
        u32_at(page, 40)
    }

    /// The count of extents.
    pub fn count(page: &Page) -> u16 {
        u16_at(page, 48)
    }

    /// The first physical page of each extent, in ring order. Bad data gives SQLSTATE `XX001`.
    pub fn extents(page: &Page) -> Result<Vec<u64>> {
        let count = usize::from(Self::count(page));
        if count > RING_EXTENTS_PER_PAGE {
            return Err(Error::corrupted(format!(
                "the ring extent list page has the count {count}, above {RING_EXTENTS_PER_PAGE}"
            )));
        }
        let extents: Vec<u64> =
            (0..count).map(|i| u64_at(page, PAGE_HEADER_SIZE + i * 8)).collect();
        match extent_problem(&extents) {
            Some(p) => Err(Error::corrupted(format!(
                "the extent list of ring {} is damaged: {p}",
                Self::ring(page)
            ))),
            None => Ok(extents),
        }
    }

    /// The text form: the page header without the kind data, `count`, `extents` as page numbers with a comma between, or `none`, and `ring`.
    pub fn to_text(page: &Page) -> Result<String> {
        let header = PageHeader::read(page)?;
        let extents = Self::extents(page)?;
        let list = if extents.is_empty() {
            "none".to_string()
        } else {
            extents.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        };
        let mut out = TextOut::new();
        header.put_common(&mut out);
        out.field("count", extents.len()).field("extents", list).field("ring", Self::ring(page));
        Ok(out.finish())
    }

    /// Reads the text form back into a sealed page.
    pub fn from_text(text: &str) -> Result<Box<Page>> {
        let mut input = TextIn::parse("ring extent list page", text)?;
        let mut header = PageHeader::take_common(&mut input)?;
        if header.kind != PageKind::RingExtents {
            return Err(bad("ring extent list", format!("the kind is {}", header.kind)));
        }
        let count: usize = input.take("count")?;
        let ring: u32 = input.take("ring")?;
        let value = input.take_str("extents")?;
        let extents: Option<Vec<u64>> = if value == "none" {
            Some(Vec::new())
        } else {
            value.split(',').map(|s| s.parse().ok()).collect()
        };
        let extents = extents.ok_or_else(|| {
            bad("ring extent list", format!("extents has the bad value \"{value}\""))
        })?;
        if extents.len() != count {
            return Err(bad(
                "ring extent list",
                format!("the count is {count}, but extents has {} numbers", extents.len()),
            ));
        }
        input.finish()?;
        let mut page = Box::new([0u8; PAGE_SIZE]);
        Self::init(&mut page, header.page.0, ring, &extents)
            .map_err(|e| bad("ring extent list", e.message().to_string()))?;
        // The other header fields come from the text.
        put_u32(&mut header.kind_data, 0, ring);
        put_u16(&mut header.kind_data, 8, count as u16);
        header.write(&mut page);
        seal(&mut page);
        Ok(page)
    }
}

fn entry_problem(entries: &[RingEntry]) -> Option<String> {
    let mut last = None;
    for e in entries {
        if let Some(p) = e.problem() {
            return Some(p);
        }
        if last.is_some_and(|l| e.ring <= l) {
            return Some(format!("ring {} is not above the ring before it", e.ring));
        }
        last = Some(e.ring);
    }
    None
}

fn extent_problem(extents: &[u64]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    for &start in extents {
        let whole = start >= FIRST_ARENA_PAGE
            && (start - FIRST_ARENA_PAGE).is_multiple_of(ARENA_PAGES)
            && start + ARENA_PAGES - 1 <= MAX_PHYSICAL;
        if !whole {
            return Some(format!("page {start} is not the start of an arena"));
        }
        if !seen.insert(start) {
            return Some(format!("the extent at page {start} is in the list two times"));
        }
    }
    None
}

fn bad(what: &str, message: String) -> Error {
    Error::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("bad text form of the {what} page: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify;

    fn entry(ring: u32) -> RingEntry {
        RingEntry {
            ring,
            worker: ring + 10,
            size: 4 * RING_EXTENT_BYTES,
            redo: 1 << 20,
            durable: (1 << 20) + 5000,
            newest: Hlc::new(1000, 3).unwrap(),
            extents: Root { page: 3000 + u64::from(ring), checksum: 0xabcd },
        }
    }

    #[test]
    fn directory_page() {
        assert_eq!(RINGS_PER_PAGE * RING_ENTRY_SIZE + PAGE_HEADER_SIZE, 16192);
        let next = PtEntry::new(900, Hlc::from_bits(7)).unwrap();
        let entries = [entry(0), entry(1), entry(5)];
        let mut page = [0u8; PAGE_SIZE];
        RingDirectoryPage::init(&mut page, 40, &entries, next).unwrap();
        seal(&mut page);
        assert_eq!(verify(&page, 40, PageId(40), None).unwrap().kind, PageKind::RingDirectory);
        assert_eq!((RingDirectoryPage::count(&page), RingDirectoryPage::next(&page)), (3, next));
        assert_eq!(RingDirectoryPage::entries(&page).unwrap(), entries);

        let text = RingDirectoryPage::to_text(&page).unwrap();
        assert!(text.contains("\nkind ring_directory\n"), "{text}");
        assert!(
            text.contains(
                "\nring_002 ring 5 worker 15 size 67108864 redo 1048576 durable 1053576 newest 1000.3 extents 3005 0x000000000000abcd\n"
            ),
            "{text}"
        );
        assert_eq!(&RingDirectoryPage::from_text(&text).unwrap()[..], &page[..]);
        assert!(RingDirectoryPage::from_text(&text.replace("count 3", "count 4")).is_err());
        assert!(
            RingDirectoryPage::from_text(&text.replace("ring 5 worker", "ring 1 worker")).is_err()
        );
        assert!(
            RingDirectoryPage::from_text(&text.replace("size 67108864", "size 67108865")).is_err()
        );
        assert!(
            RingDirectoryPage::from_text(&text.replace("ring_002 ring", "ring_002 rung")).is_err()
        );

        // An empty directory, and a full page.
        RingDirectoryPage::init(&mut page, 41, &[], PtEntry::EMPTY).unwrap();
        seal(&mut page);
        let text = RingDirectoryPage::to_text(&page).unwrap();
        assert_eq!(&RingDirectoryPage::from_text(&text).unwrap()[..], &page[..]);
        let full: Vec<RingEntry> = (0..RINGS_PER_PAGE as u32).map(entry).collect();
        RingDirectoryPage::init(&mut page, 42, &full, PtEntry::EMPTY).unwrap();
        assert_eq!(RingDirectoryPage::entries(&page).unwrap(), full);
    }

    #[test]
    fn bad_entries() {
        let mut page = [0u8; PAGE_SIZE];
        let e = PtEntry::EMPTY;
        let mut cases = vec![vec![entry(1), entry(1)], vec![entry(2), entry(1)]];
        let too_many: Vec<RingEntry> = (0..=RINGS_PER_PAGE as u32).map(entry).collect();
        cases.push(too_many);
        for change in [
            |r: &mut RingEntry| r.size = 0,
            |r: &mut RingEntry| r.size = RING_EXTENT_BYTES + 1,
            |r: &mut RingEntry| r.size = (RING_EXTENTS_PER_PAGE as u64 + 1) * RING_EXTENT_BYTES,
            |r: &mut RingEntry| r.redo = r.durable + 1,
            |r: &mut RingEntry| r.durable = r.redo + r.size + 1,
            |r: &mut RingEntry| r.extents.page = 2,
        ] {
            let mut r = entry(0);
            change(&mut r);
            cases.push(vec![r]);
        }
        for entries in cases {
            let err = RingDirectoryPage::init(&mut page, 40, &entries, e).unwrap_err();
            assert_eq!(err.state(), SqlState::INTERNAL_ERROR, "{entries:?}");
        }

        // A damaged page gives XX001.
        RingDirectoryPage::init(&mut page, 40, &[entry(0), entry(1)], e).unwrap();
        let mut odd = page;
        put_u32(&mut odd, PAGE_HEADER_SIZE + RING_ENTRY_SIZE, 0);
        assert_eq!(RingDirectoryPage::entries(&odd).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        let mut odd = page;
        odd[PAGE_HEADER_SIZE + 90] = 1;
        assert_eq!(RingDirectoryPage::entries(&odd).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        let mut odd = page;
        put_u16(&mut odd, 48, RINGS_PER_PAGE as u16 + 1);
        assert_eq!(RingDirectoryPage::entries(&odd).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn extent_list_page() {
        let a = |n: u64| FIRST_ARENA_PAGE + n * ARENA_PAGES;
        let extents = [a(7), a(2), a(30)];
        let mut page = [0u8; PAGE_SIZE];
        RingExtentsPage::init(&mut page, 50, 4, &extents).unwrap();
        seal(&mut page);
        assert_eq!(verify(&page, 50, PageId(50), None).unwrap().kind, PageKind::RingExtents);
        assert_eq!((RingExtentsPage::ring(&page), RingExtentsPage::count(&page)), (4, 3));
        assert_eq!(RingExtentsPage::extents(&page).unwrap(), extents);

        let text = RingExtentsPage::to_text(&page).unwrap();
        assert!(
            text.contains("\nextents 7171,2051,30723\n") && text.contains("\nring 4\n"),
            "{text}"
        );
        assert_eq!(&RingExtentsPage::from_text(&text).unwrap()[..], &page[..]);
        assert!(RingExtentsPage::from_text(&text.replace("count 3", "count 2")).is_err());
        assert!(RingExtentsPage::from_text(&text.replace("7171,", "7172,")).is_err());
        assert!(RingExtentsPage::from_text(&text.replace("7171,", "2051,")).is_err());

        RingExtentsPage::init(&mut page, 50, 4, &[]).unwrap();
        seal(&mut page);
        let text = RingExtentsPage::to_text(&page).unwrap();
        assert!(text.contains("\nextents none\n"), "{text}");
        assert_eq!(&RingExtentsPage::from_text(&text).unwrap()[..], &page[..]);

        for bad in [&[a(1) + 1][..], &[a(1), a(1)], &[2]] {
            let err = RingExtentsPage::init(&mut page, 50, 4, bad).unwrap_err();
            assert_eq!(err.state(), SqlState::INTERNAL_ERROR);
        }
        let many: Vec<u64> = (0..=RING_EXTENTS_PER_PAGE as u64).map(a).collect();
        assert!(RingExtentsPage::init(&mut page, 50, 4, &many).is_err());

        RingExtentsPage::init(&mut page, 50, 4, &[a(1), a(2)]).unwrap();
        put_u64(&mut page, PAGE_HEADER_SIZE + 8, a(1));
        assert_eq!(RingExtentsPage::extents(&page).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }
}

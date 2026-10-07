//! Arenas and the free space map of spec/08 section 8.9.
//!
//! The file after page 2 is a sequence of arenas of 1024 pages. The free space map has an 8-byte entry and a 1024-bit bitmap for each arena. A bit is 1 when its page is in use.

use std::fmt;
use std::str::FromStr;

use rupg_common::{Error, Result, SqlState};

use crate::le::{get, put, put_u16, put_u64, u16_at, u64_at};
use crate::text::{TextIn, TextOut};
use crate::{
    ExtentClass, FIRST_ARENA_PAGE, PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind,
    PtEntry, seal,
};

/// The pages in an arena, 16 MiB.
pub const ARENA_PAGES: u64 = 1024;

/// The arena records in one free space map page: 120 records of 136 bytes fill the page after the header.
pub const FSM_ARENAS_PER_PAGE: usize = 120;

const RECORD_SIZE: usize = 8 + 128;

/// The arena of a physical page, or `None` for pages 0 to 2.
pub fn arena_of(physical: u64) -> Option<u64> {
    Some(physical.checked_sub(FIRST_ARENA_PAGE)? / ARENA_PAGES)
}

/// The first physical page of an arena.
pub fn arena_start(arena: u64) -> u64 {
    FIRST_ARENA_PAGE + arena * ARENA_PAGES
}

/// The kind of an arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ArenaKind {
    /// No page is in use, and the arena has no kind yet.
    #[default]
    Free = 0,
    /// 16 KiB pages of the page writer, given in increasing order.
    Write = 1,
    /// Segment column and dictionary extents, with the buddy rule.
    Extent = 2,
    /// Summary extents, with the buddy rule.
    Summary = 3,
    /// One E4 extent of a log ring.
    Ring = 4,
    /// Temporary spill and temporary tables. Not durable.
    Spill = 5,
}

impl ArenaKind {
    /// The kinds in the order of the number.
    pub const ALL: [ArenaKind; 6] = [
        ArenaKind::Free,
        ArenaKind::Write,
        ArenaKind::Extent,
        ArenaKind::Summary,
        ArenaKind::Ring,
        ArenaKind::Spill,
    ];

    /// The name in the text form.
    pub fn name(self) -> &'static str {
        match self {
            ArenaKind::Free => "free",
            ArenaKind::Write => "write",
            ArenaKind::Extent => "extent",
            ArenaKind::Summary => "summary",
            ArenaKind::Ring => "ring",
            ArenaKind::Spill => "spill",
        }
    }
}

impl fmt::Display for ArenaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ArenaKind {
    type Err = ();

    fn from_str(s: &str) -> Result<ArenaKind, ()> {
        ArenaKind::ALL.into_iter().find(|k| k.name() == s).ok_or(())
    }
}

/// The bitmap of one arena, 1024 bits. Bit `i` is 1 when page `i` of the arena is in use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ArenaMap {
    words: [u64; 16],
}

impl ArenaMap {
    /// A map with every page free.
    pub fn new() -> ArenaMap {
        ArenaMap::default()
    }

    /// True if page `offset` of the arena is in use. An offset of 1024 or more is not in use.
    pub fn is_used(&self, offset: u16) -> bool {
        let i = usize::from(offset);
        self.words.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    /// The number of free pages.
    pub fn free_pages(&self) -> u16 {
        1024 - self.words.iter().map(|w| w.count_ones() as u16).sum::<u16>()
    }

    /// True if each page of the run `offset .. offset + pages` is free. A run past the end of the arena is not free.
    fn run_is_free(&self, offset: u64, pages: u64) -> bool {
        if offset.saturating_add(pages) > ARENA_PAGES {
            return false;
        }
        (offset..offset + pages).step_by(64).all(|start| {
            let len = (offset + pages - start).min(64);
            let word = self.words[(start / 64) as usize];
            let shift = start % 64;
            let mask = if len == 64 { u64::MAX } else { ((1u64 << len) - 1) << shift };
            word & mask == 0
        })
    }

    fn set_run(&mut self, offset: u64, pages: u64, used: bool) {
        for i in offset..offset + pages {
            let word = &mut self.words[(i / 64) as usize];
            if used {
                *word |= 1 << (i % 64);
            } else {
                *word &= !(1 << (i % 64));
            }
        }
    }

    /// Takes the first free extent of `class` that is aligned inside the arena, and gives its page offset.
    pub fn allocate(&mut self, class: ExtentClass) -> Option<u16> {
        let pages = class.pages();
        let offset =
            (0..ARENA_PAGES).step_by(pages as usize).find(|&o| self.run_is_free(o, pages))?;
        self.set_run(offset, pages, true);
        Some(offset as u16)
    }

    /// Takes the first free page at or after `from`. A write arena gives pages in increasing order, so the caller keeps `from` at the last page it took.
    pub fn allocate_page(&mut self, from: u16) -> Option<u16> {
        let offset = (u64::from(from)..ARENA_PAGES).find(|&o| self.run_is_free(o, 1))?;
        self.set_run(offset, 1, true);
        Some(offset as u16)
    }

    /// Marks a run as in use, for example when recovery rebuilds the map. A run that is past the end or partly in use gives SQLSTATE `XX000`.
    pub fn mark(&mut self, offset: u16, pages: u64) -> Result<()> {
        if !self.run_is_free(u64::from(offset), pages) {
            return Err(Error::internal(format!(
                "the pages {offset} to {} of the arena are not free",
                u64::from(offset) + pages - 1
            )));
        }
        self.set_run(u64::from(offset), pages, true);
        Ok(())
    }

    /// Frees a run that [`ArenaMap::allocate`], [`ArenaMap::allocate_page`] or [`ArenaMap::mark`] took. A run that is past the end or has a free page gives SQLSTATE `XX000`, because a double free is a bug.
    pub fn release(&mut self, offset: u16, pages: u64) -> Result<()> {
        let start = u64::from(offset);
        if start.saturating_add(pages) > ARENA_PAGES
            || !(start..start + pages).all(|i| self.is_used(i as u16))
        {
            return Err(Error::internal(format!(
                "double free: the pages {offset} to {} of the arena are not all in use",
                start + pages - 1
            )));
        }
        self.set_run(start, pages, false);
        Ok(())
    }

    /// The largest class that has a free aligned extent, or `None` if no 4-page aligned run is free.
    pub fn largest_free(&self) -> Option<ExtentClass> {
        ExtentClass::ALL.into_iter().rev().find(|c| {
            let pages = c.pages();
            (0..ARENA_PAGES).step_by(pages as usize).any(|o| self.run_is_free(o, pages))
        })
    }

    /// The 128 bytes of the map.
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut out = [0u8; 128];
        for (i, w) in self.words.iter().enumerate() {
            put_u64(&mut out, i * 8, *w);
        }
        out
    }

    /// The map from its 128 bytes.
    pub fn from_bytes(bytes: &[u8; 128]) -> ArenaMap {
        let mut map = ArenaMap::new();
        for (i, w) in map.words.iter_mut().enumerate() {
            *w = u64_at(bytes, i * 8);
        }
        map
    }

    /// The pages in use as ranges, for example `0-63,128`, or `none`.
    fn ranges(&self) -> String {
        let mut out = Vec::new();
        let mut i = 0u64;
        while i < ARENA_PAGES {
            if !self.is_used(i as u16) {
                i += 1;
                continue;
            }
            let start = i;
            while i < ARENA_PAGES && self.is_used(i as u16) {
                i += 1;
            }
            out.push(if i - 1 == start {
                format!("{start}")
            } else {
                format!("{start}-{}", i - 1)
            });
        }
        if out.is_empty() { "none".to_string() } else { out.join(",") }
    }

    fn from_ranges(text: &str) -> Option<ArenaMap> {
        let mut map = ArenaMap::new();
        if text == "none" {
            return Some(map);
        }
        for range in text.split(',') {
            let (a, b) = range.split_once('-').unwrap_or((range, range));
            let (a, b): (u16, u16) = (a.parse().ok()?, b.parse().ok()?);
            if a > b || map.mark(a, u64::from(b - a) + 1).is_err() {
                return None;
            }
        }
        Some(map)
    }
}

/// The 8-byte entry of an arena in the free space map. Byte 0 is the kind, byte 1 is the order of the largest free class or 255 for none, bytes 2 and 3 are the free page count, and bytes 4 to 7 are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FsmEntry {
    /// The kind of the arena.
    pub kind: ArenaKind,
    /// The number of free pages, 0 to 1024.
    pub free_pages: u16,
    /// The largest class with a free aligned extent.
    pub largest: Option<ExtentClass>,
}

impl FsmEntry {
    /// The entry of an arena from its kind and its map.
    pub fn of(kind: ArenaKind, map: &ArenaMap) -> FsmEntry {
        FsmEntry { kind, free_pages: map.free_pages(), largest: map.largest_free() }
    }

    /// The 8 bytes of the entry.
    pub fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.kind as u8;
        out[1] = self.largest.map_or(255, ExtentClass::order);
        put_u16(&mut out, 2, self.free_pages);
        out
    }

    /// The entry from its 8 bytes. A kind, a class or a count that is not valid gives SQLSTATE `XX001`.
    pub fn from_bytes(bytes: &[u8; 8]) -> Result<FsmEntry> {
        let bad = || {
            Error::corrupted(format!(
                "the free space map has the bad entry {}",
                crate::text::hex_bytes(bytes)
            ))
        };
        let kind = ArenaKind::ALL.get(usize::from(bytes[0])).copied().ok_or_else(bad)?;
        let largest = match bytes[1] {
            255 => None,
            order => Some(ExtentClass::from_order(order).ok_or_else(bad)?),
        };
        let free_pages = u16_at(bytes, 2);
        if free_pages > 1024 || bytes[4..] != [0; 4] {
            return Err(bad());
        }
        Ok(FsmEntry { kind, free_pages, largest })
    }
}

/// A free space map page. Its header holds its physical page number in the logical page field. The kind data holds the first arena at bytes 0 to 7, the number of records in use at bytes 8 and 9, and the entry of the next map page at bytes 16 to 23, in the format of a page table entry.
#[derive(Clone, Copy, Debug)]
pub struct FsmPage;

impl FsmPage {
    /// Sets up a map page with `count` free arenas from `first`. A count above [`FSM_ARENAS_PER_PAGE`] is cut to it.
    pub fn init(page: &mut Page, physical: u64, first: u64, count: u16, next: PtEntry) {
        page.fill(0);
        let mut header = PageHeader::new(PageId(physical), PageKind::FreeSpace);
        let count = count.min(FSM_ARENAS_PER_PAGE as u16);
        put_u64(&mut header.kind_data, 0, first);
        put_u16(&mut header.kind_data, 8, count);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.lower = PAGE_SIZE as u16 - 1;
        header.upper = PAGE_SIZE as u16 - 1;
        header.write(page);
        let empty = FsmEntry::of(ArenaKind::Free, &ArenaMap::new());
        for i in 0..count {
            Self::write_record(page, i, empty, &ArenaMap::new());
        }
    }

    /// The first arena of the page.
    pub fn first(page: &Page) -> u64 {
        u64_at(page, 40)
    }

    /// The number of records in use.
    pub fn count(page: &Page) -> u16 {
        u16_at(page, 48).min(FSM_ARENAS_PER_PAGE as u16)
    }

    /// The entry of the next map page, or the empty entry for the last page.
    pub fn next(page: &Page) -> PtEntry {
        PtEntry::from_bits(u64_at(page, 56))
    }

    fn offset(i: u16) -> usize {
        PAGE_HEADER_SIZE + usize::from(i) * RECORD_SIZE
    }

    fn write_record(page: &mut Page, i: u16, entry: FsmEntry, map: &ArenaMap) {
        let at = Self::offset(i);
        put(page, at, &entry.to_bytes());
        put(page, at + 8, &map.to_bytes());
    }

    /// The record of arena `first + i`. A record whose entry does not match its map gives SQLSTATE `XX001`.
    pub fn record(page: &Page, i: u16) -> Result<(FsmEntry, ArenaMap)> {
        if i >= Self::count(page) {
            return Err(Error::internal(format!("free space map record {i} is out of range")));
        }
        let at = Self::offset(i);
        let entry = FsmEntry::from_bytes(&get(page, at))?;
        let map = ArenaMap::from_bytes(&get(page, at + 8));
        if FsmEntry::of(entry.kind, &map) != entry {
            return Err(Error::corrupted(format!(
                "the free space map entry of arena {} does not match its bitmap",
                Self::first(page) + u64::from(i)
            )));
        }
        Ok((entry, map))
    }

    /// Writes the record of arena `first + i`. The entry comes from the kind and the map.
    pub fn set_record(page: &mut Page, i: u16, kind: ArenaKind, map: &ArenaMap) -> Result<()> {
        if i >= Self::count(page) {
            return Err(Error::internal(format!("free space map record {i} is out of range")));
        }
        Self::write_record(page, i, FsmEntry::of(kind, map), map);
        Ok(())
    }

    /// The text form: the page header without the kind data, `first`, `count`, `next`, and one `arena_NNN` line for each record with the kind and the pages in use.
    pub fn to_text(page: &Page) -> Result<String> {
        let header = PageHeader::read(page)?;
        let mut out = TextOut::new();
        header.put_common(&mut out);
        out.field("count", Self::count(page))
            .field("first", Self::first(page))
            .field("next", Self::next(page));
        for i in 0..Self::count(page) {
            let (entry, map) = Self::record(page, i)?;
            out.field(&format!("arena_{i:03}"), format_args!("{} {}", entry.kind, map.ranges()));
        }
        Ok(out.finish())
    }

    /// Reads the text form back into a sealed page.
    pub fn from_text(text: &str) -> Result<Box<Page>> {
        let mut input = TextIn::parse("free space map page", text)?;
        let mut header = PageHeader::take_common(&mut input)?;
        if header.kind != PageKind::FreeSpace {
            return Err(bad(format!("the kind is {}", header.kind)));
        }
        let first: u64 = input.take("first")?;
        let count: u16 = input.take("count")?;
        if usize::from(count) > FSM_ARENAS_PER_PAGE {
            return Err(bad(format!("the count {count} is above {FSM_ARENAS_PER_PAGE}")));
        }
        let next: PtEntry = input.take("next")?;
        let mut page = Box::new([0u8; PAGE_SIZE]);
        Self::init(&mut page, header.page.0, first, count, next);
        // The other header fields come from the text.
        put_u64(&mut header.kind_data, 0, first);
        put_u16(&mut header.kind_data, 8, count);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.write(&mut page);
        for (i, value) in input.take_prefixed("arena_") {
            let parsed = i.parse::<u16>().ok().filter(|&n| n < count && i.len() == 3);
            let record = value
                .split_once(' ')
                .and_then(|(k, r)| Some((k.parse().ok()?, ArenaMap::from_ranges(r)?)));
            let (Some(n), Some((kind, map))) = (parsed, record) else {
                return Err(bad(format!("arena_{i} has the bad value \"{value}\"")));
            };
            Self::set_record(&mut page, n, kind, &map)?;
        }
        input.finish()?;
        seal(&mut page);
        Ok(page)
    }
}

fn bad(message: String) -> Error {
    Error::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("bad text form of the free space map page: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::Hlc;

    #[test]
    fn arenas() {
        assert_eq!(arena_of(2), None);
        assert_eq!(arena_of(3), Some(0));
        assert_eq!(arena_of(1026), Some(0));
        assert_eq!(arena_of(1027), Some(1));
        assert_eq!(arena_start(2), 2051);
        for k in ArenaKind::ALL {
            assert_eq!(k.name().parse(), Ok(k));
        }
    }

    #[test]
    fn buddy() {
        let mut map = ArenaMap::new();
        assert_eq!((map.free_pages(), map.largest_free()), (1024, Some(ExtentClass::E4)));
        assert_eq!(map.allocate(ExtentClass::E0), Some(0));
        assert_eq!(map.largest_free(), Some(ExtentClass::E3));
        assert_eq!(map.allocate(ExtentClass::E2), Some(64));
        assert_eq!(map.allocate(ExtentClass::E0), Some(4));
        assert_eq!(map.allocate(ExtentClass::E1), Some(16));
        assert_eq!(map.allocate(ExtentClass::E3), Some(256));
        assert_eq!(map.free_pages(), 1024 - 4 - 64 - 4 - 16 - 256);
        assert_eq!(map.allocate(ExtentClass::E4), None);
        assert_eq!(map.ranges(), "0-7,16-31,64-127,256-511");
        // Free the E2 and the run merges back with one test of the aligned run.
        map.release(64, 64).unwrap();
        assert_eq!(map.allocate(ExtentClass::E2), Some(64));
        assert!(map.release(64, 65).is_err());
        assert!(map.release(8, 4).is_err());
        assert!(map.release(1020, 8).is_err());
        assert!(map.mark(0, 1).is_err());
        assert!(map.mark(1023, 2).is_err());

        let mut full = ArenaMap::new();
        for i in 0..256u16 {
            assert_eq!(full.allocate(ExtentClass::E0), Some(i * 4));
        }
        assert_eq!(
            (full.free_pages(), full.largest_free(), full.allocate(ExtentClass::E0)),
            (0, None, None)
        );
        full.release(4, 1).unwrap();
        assert_eq!((full.free_pages(), full.largest_free()), (1, None));
    }

    #[test]
    fn write_arena() {
        let mut map = ArenaMap::new();
        map.mark(1, 1).unwrap();
        assert_eq!(map.allocate_page(0), Some(0));
        assert_eq!(map.allocate_page(0), Some(2));
        assert_eq!(map.allocate_page(100), Some(100));
        assert_eq!(map.allocate_page(1023), Some(1023));
        assert_eq!(map.allocate_page(1023), None);
        assert_eq!(map.allocate_page(2000), None);
        assert_eq!(map.ranges(), "0-2,100,1023");
        assert_eq!(ArenaMap::from_ranges("0-2,100,1023"), Some(map));
        assert_eq!(ArenaMap::from_ranges("5-3"), None);
        assert_eq!(ArenaMap::from_ranges("1,1"), None);
        assert_eq!(ArenaMap::from_ranges("1024"), None);
        assert_eq!(ArenaMap::from_bytes(&map.to_bytes()), map);
    }

    #[test]
    fn entries() {
        let mut map = ArenaMap::new();
        map.allocate(ExtentClass::E3).unwrap();
        let e = FsmEntry::of(ArenaKind::Extent, &map);
        assert_eq!(
            e,
            FsmEntry { kind: ArenaKind::Extent, free_pages: 768, largest: Some(ExtentClass::E3) }
        );
        assert_eq!(e.to_bytes(), [2, 3, 0, 3, 0, 0, 0, 0]);
        assert_eq!(FsmEntry::from_bytes(&e.to_bytes()).unwrap(), e);
        let none = FsmEntry { kind: ArenaKind::Write, free_pages: 0, largest: None };
        assert_eq!(FsmEntry::from_bytes(&none.to_bytes()).unwrap(), none);
        for bad in [
            [6, 255, 0, 0, 0, 0, 0, 0],
            [1, 5, 0, 0, 0, 0, 0, 0],
            [1, 255, 1, 4, 0, 0, 0, 0],
            [1, 255, 0, 0, 1, 0, 0, 0],
        ] {
            assert_eq!(
                FsmEntry::from_bytes(&bad).unwrap_err().state(),
                SqlState::DATA_CORRUPTED,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn page() {
        let mut page = [0u8; PAGE_SIZE];
        let next = PtEntry::new(900, Hlc::from_bits(7)).unwrap();
        FsmPage::init(&mut page, 20, 120, 3, next);
        assert_eq!(PAGE_HEADER_SIZE + FSM_ARENAS_PER_PAGE * RECORD_SIZE, PAGE_SIZE);
        let mut map = ArenaMap::new();
        map.allocate(ExtentClass::E1).unwrap();
        FsmPage::set_record(&mut page, 1, ArenaKind::Summary, &map).unwrap();
        assert!(FsmPage::set_record(&mut page, 3, ArenaKind::Summary, &map).is_err());
        seal(&mut page);
        crate::verify(&page, 20, PageId(20), None).unwrap();
        assert_eq!(
            (FsmPage::first(&page), FsmPage::count(&page), FsmPage::next(&page)),
            (120, 3, next)
        );
        assert_eq!(FsmPage::record(&page, 1).unwrap().0.kind, ArenaKind::Summary);
        assert_eq!(FsmPage::record(&page, 0).unwrap().0.free_pages, 1024);

        let text = FsmPage::to_text(&page).unwrap();
        assert!(text.starts_with("arena_000 free none\narena_001 summary 0-15\narena_002 free none\ncount 3\nfirst 120\n"), "{text}");
        assert!(text.contains("\nnext 900 7\n"), "{text}");
        assert_eq!(&FsmPage::from_text(&text).unwrap()[..], &page[..]);
        assert!(FsmPage::from_text(&text.replace("arena_002", "arena_003")).is_err());
        assert!(FsmPage::from_text(&text.replace("summary 0-15", "summary 0-1024")).is_err());
        assert!(FsmPage::from_text(&text.replace("count 3", "count 121")).is_err());

        // An entry that does not match its bitmap.
        let mut odd = page;
        odd[PAGE_HEADER_SIZE + RECORD_SIZE + 8] = 0;
        assert_eq!(FsmPage::record(&odd, 1).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }
}

//! Pages of the pending free list of spec/08 section 8.9.2.
//!
//! The pending free list holds the physical pages that the tree of the other header slot can still use. They become free at the next checkpoint. The list is a chain of pages, and the header slot names the first page.

use rupg_common::{Error, Result, SqlState};

use crate::le::{put_u16, put_u64, u16_at, u64_at};
use crate::text::{TextIn, TextOut};
use crate::{
    FIRST_ARENA_PAGE, MAX_PHYSICAL, PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId,
    PageKind, PtEntry, seal,
};

/// The physical page numbers in one page of the pending free list: 2040 numbers of 8 bytes fill the page after the header.
pub const FREE_LIST_PER_PAGE: usize = (PAGE_SIZE - PAGE_HEADER_SIZE) / 8;

/// A page of the pending free list. The header holds its physical page number in the logical page field, as a page table node does.
///
/// The kind data holds the count of numbers at bytes 8 and 9 and the next page of the list at bytes 16 to 23, in the format of a page table entry. The numbers follow the header in increasing order, with no number two times.
#[derive(Clone, Copy, Debug)]
pub struct FreeListPage;

impl FreeListPage {
    /// Sets up a page at `physical` that holds `pages`, with `next` as the next page of the list. `pages` must be in increasing order with no number two times, and each number must be a page of an arena. Other input gives SQLSTATE `XX000`. The caller seals the page before it writes it.
    pub fn init(page: &mut Page, physical: u64, pages: &[u64], next: PtEntry) -> Result<()> {
        if pages.len() > FREE_LIST_PER_PAGE {
            return Err(Error::internal(format!(
                "{} pages do not fit in one page of the pending free list",
                pages.len()
            )));
        }
        if let Some(bad) = first_bad(pages) {
            return Err(Error::internal(format!(
                "the pending free list cannot hold page {bad} at this position"
            )));
        }
        page.fill(0);
        let mut header = PageHeader::new(PageId(physical), PageKind::FreeList);
        put_u16(&mut header.kind_data, 8, pages.len() as u16);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.lower = PAGE_SIZE as u16 - 1;
        header.upper = PAGE_SIZE as u16 - 1;
        header.write(page);
        for (i, &p) in pages.iter().enumerate() {
            put_u64(page, PAGE_HEADER_SIZE + i * 8, p);
        }
        Ok(())
    }

    /// The count of numbers in the page.
    pub fn count(page: &Page) -> u16 {
        u16_at(page, 48)
    }

    /// The entry of the next page of the list, or the empty entry for the last page.
    pub fn next(page: &Page) -> PtEntry {
        PtEntry::from_bits(u64_at(page, 56))
    }

    /// The physical page numbers. A count above [`FREE_LIST_PER_PAGE`], a number outside the arenas, or numbers that are not in increasing order give SQLSTATE `XX001`.
    pub fn pages(page: &Page) -> Result<Vec<u64>> {
        let count = usize::from(Self::count(page));
        if count > FREE_LIST_PER_PAGE {
            return Err(Error::corrupted(format!(
                "the pending free list page has the count {count}, above {FREE_LIST_PER_PAGE}"
            )));
        }
        let pages: Vec<u64> = (0..count).map(|i| u64_at(page, PAGE_HEADER_SIZE + i * 8)).collect();
        match first_bad(&pages) {
            Some(bad) => Err(Error::corrupted(format!(
                "the pending free list page holds page {bad} at a bad position"
            ))),
            None => Ok(pages),
        }
    }

    /// The text form: the page header without the kind data, `count`, `next`, and `pages` as ranges such as `3-10,15`, or `none`.
    pub fn to_text(page: &Page) -> Result<String> {
        let header = PageHeader::read(page)?;
        let pages = Self::pages(page)?;
        let mut out = TextOut::new();
        header.put_common(&mut out);
        out.field("count", pages.len())
            .field("next", Self::next(page))
            .field("pages", ranges(&pages));
        Ok(out.finish())
    }

    /// Reads the text form back into a sealed page.
    pub fn from_text(text: &str) -> Result<Box<Page>> {
        let mut input = TextIn::parse("pending free list page", text)?;
        let mut header = PageHeader::take_common(&mut input)?;
        if header.kind != PageKind::FreeList {
            return Err(bad(format!("the kind is {}", header.kind)));
        }
        let count: usize = input.take("count")?;
        let next: PtEntry = input.take("next")?;
        let value = input.take_str("pages")?;
        let pages = from_ranges(value)
            .ok_or_else(|| bad(format!("pages has the bad value \"{value}\"")))?;
        if pages.len() != count {
            return Err(bad(format!(
                "the count is {count}, but pages has {} numbers",
                pages.len()
            )));
        }
        input.finish()?;
        let mut page = Box::new([0u8; PAGE_SIZE]);
        Self::init(&mut page, header.page.0, &pages, next)
            .map_err(|e| bad(e.message().to_string()))?;
        // The other header fields come from the text.
        put_u16(&mut header.kind_data, 8, count as u16);
        put_u64(&mut header.kind_data, 16, next.bits());
        header.write(&mut page);
        seal(&mut page);
        Ok(page)
    }
}

/// The first number that is outside the arenas or not above the number before it.
fn first_bad(pages: &[u64]) -> Option<u64> {
    let mut last = 0;
    for &p in pages {
        if !(FIRST_ARENA_PAGE..=MAX_PHYSICAL).contains(&p) || p <= last {
            return Some(p);
        }
        last = p;
    }
    None
}

fn ranges(pages: &[u64]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let start = pages[i];
        while i + 1 < pages.len() && pages[i + 1] == pages[i] + 1 {
            i += 1;
        }
        out.push(if pages[i] == start {
            format!("{start}")
        } else {
            format!("{start}-{}", pages[i])
        });
        i += 1;
    }
    if out.is_empty() { "none".to_string() } else { out.join(",") }
}

fn from_ranges(text: &str) -> Option<Vec<u64>> {
    let mut pages = Vec::new();
    if text == "none" {
        return Some(pages);
    }
    for range in text.split(',') {
        let (a, b) = range.split_once('-').unwrap_or((range, range));
        let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
        if a > b || pages.len() as u64 + b - a >= FREE_LIST_PER_PAGE as u64 {
            return None;
        }
        pages.extend(a..=b);
    }
    Some(pages)
}

fn bad(message: String) -> Error {
    Error::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("bad text form of the pending free list page: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify;
    use rupg_common::Hlc;

    #[test]
    fn page() {
        assert_eq!(FREE_LIST_PER_PAGE, 2040);
        let next = PtEntry::new(900, Hlc::from_bits(7)).unwrap();
        let pages = [3, 4, 5, 9, 1030];
        let mut page = [0u8; PAGE_SIZE];
        FreeListPage::init(&mut page, 40, &pages, next).unwrap();
        seal(&mut page);
        assert_eq!(verify(&page, 40, PageId(40), None).unwrap().kind, PageKind::FreeList);
        assert_eq!((FreeListPage::count(&page), FreeListPage::next(&page)), (5, next));
        assert_eq!(FreeListPage::pages(&page).unwrap(), pages);

        let text = FreeListPage::to_text(&page).unwrap();
        assert!(text.starts_with("count 5\nflags none\nkind free_list\n"), "{text}");
        assert!(text.contains("\nnext 900 7\n") && text.contains("\npages 3-5,9,1030\n"), "{text}");
        assert_eq!(&FreeListPage::from_text(&text).unwrap()[..], &page[..]);
        assert!(FreeListPage::from_text(&text.replace("count 5", "count 4")).is_err());
        assert!(FreeListPage::from_text(&text.replace("3-5,9", "9,3-5")).is_err());
        assert!(FreeListPage::from_text(&text.replace("3-5,9", "2-5,9")).is_err());
        assert!(FreeListPage::from_text(&text.replace("3-5,9", "3-5,5")).is_err());
        assert!(FreeListPage::from_text(&text.replace("3-5,9", "3-3000,9")).is_err());

        // An empty page is the end of a list or a list with no pages.
        FreeListPage::init(&mut page, 41, &[], PtEntry::EMPTY).unwrap();
        seal(&mut page);
        let text = FreeListPage::to_text(&page).unwrap();
        assert!(text.contains("\nnext none\n") && text.contains("\npages none\n"), "{text}");
        assert_eq!(&FreeListPage::from_text(&text).unwrap()[..], &page[..]);

        // A full page.
        let full: Vec<u64> = (100..100 + FREE_LIST_PER_PAGE as u64).collect();
        FreeListPage::init(&mut page, 42, &full, PtEntry::EMPTY).unwrap();
        assert_eq!(FreeListPage::pages(&page).unwrap(), full);
        seal(&mut page);
        assert_eq!(
            &FreeListPage::from_text(&FreeListPage::to_text(&page).unwrap()).unwrap()[..],
            &page[..]
        );
    }

    #[test]
    fn bad_input() {
        let mut page = [0u8; PAGE_SIZE];
        let e = PtEntry::EMPTY;
        let too_many: Vec<u64> = (3..4 + FREE_LIST_PER_PAGE as u64).collect();
        for pages in [&too_many[..], &[2], &[5, 5], &[6, 5], &[MAX_PHYSICAL + 1]] {
            let err = FreeListPage::init(&mut page, 40, pages, e).unwrap_err();
            assert_eq!(err.state(), SqlState::INTERNAL_ERROR);
        }

        // A damaged page gives XX001 and not a wrong free page.
        FreeListPage::init(&mut page, 40, &[3, 4], e).unwrap();
        let mut odd = page;
        put_u64(&mut odd, PAGE_HEADER_SIZE + 8, 3);
        assert_eq!(FreeListPage::pages(&odd).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        let mut odd = page;
        put_u16(&mut odd, 48, FREE_LIST_PER_PAGE as u16 + 1);
        assert_eq!(FreeListPage::pages(&odd).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }
}

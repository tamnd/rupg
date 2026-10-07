//! The page table of spec/08 section 8.5: a radix tree that maps a logical page number to a physical page number and a write tag.

use std::fmt;
use std::str::FromStr;

use rupg_common::{Error, Hlc, Result, SqlState};

use crate::le::{put_u64, u64_at};
use crate::text::{TextIn, TextOut};
use crate::{PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind, seal, write_tag};

/// The number of entries in a node: 2040 entries of 8 bytes fill the page after the 64-byte header.
pub const PT_FANOUT: usize = 2040;

/// The number of levels of a new page table. A fourth level is added when the next logical page number passes 2040 to the power 3.
pub const PT_MIN_LEVELS: u8 = 3;

/// The most levels that a 64-bit logical page number needs.
pub const PT_MAX_LEVELS: u8 = 6;

/// The largest physical page number, 48 bits.
pub const MAX_PHYSICAL: u64 = (1 << 48) - 1;

/// An entry of a page table node. Bits 0 to 47 are the physical page number, and 0 means not allocated. Bits 48 to 63 are the write tag. In a leaf the entry names a page. In an inner node it names the child node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PtEntry(u64);

impl PtEntry {
    /// The entry of a page that is not allocated.
    pub const EMPTY: PtEntry = PtEntry(0);

    /// An entry for a copy at `physical` with the page timestamp `timestamp`. A physical page number of 0 or above [`MAX_PHYSICAL`] gives `None`.
    pub fn new(physical: u64, timestamp: Hlc) -> Option<PtEntry> {
        if physical == 0 || physical > MAX_PHYSICAL {
            return None;
        }
        Some(PtEntry(physical | u64::from(write_tag(timestamp)) << 48))
    }

    /// The entry from its 64 bits.
    pub fn from_bits(bits: u64) -> PtEntry {
        PtEntry(bits)
    }

    /// The 64 bits.
    pub fn bits(self) -> u64 {
        self.0
    }

    /// The physical page number, or `None` if the entry is empty.
    pub fn physical(self) -> Option<u64> {
        let physical = self.0 & MAX_PHYSICAL;
        (physical != 0).then_some(physical)
    }

    /// The write tag, the low 16 bits of the page timestamp of the copy.
    pub fn tag(self) -> u16 {
        (self.0 >> 48) as u16
    }
}

impl fmt::Display for PtEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.physical() {
            Some(physical) => write!(f, "{physical} {}", self.tag()),
            None => f.write_str("none"),
        }
    }
}

impl FromStr for PtEntry {
    type Err = ();

    fn from_str(s: &str) -> Result<PtEntry, ()> {
        if s == "none" {
            return Ok(PtEntry::EMPTY);
        }
        let (physical, tag) = s.split_once(' ').ok_or(())?;
        let physical: u64 = physical.parse().map_err(|_| ())?;
        let tag: u16 = tag.parse().map_err(|_| ())?;
        if physical == 0 || physical > MAX_PHYSICAL {
            return Err(());
        }
        Ok(PtEntry(physical | u64::from(tag) << 48))
    }
}

/// The number of logical pages that a page table of `levels` levels addresses, 2040 to the power `levels`. The value saturates at `u64::MAX`.
pub fn pt_capacity(levels: u8) -> u64 {
    let mut capacity: u64 = 1;
    for _ in 0..levels {
        capacity = capacity.saturating_mul(PT_FANOUT as u64);
    }
    capacity
}

/// The number of levels for a page table whose next logical page number is `next_page`. It is at least [`PT_MIN_LEVELS`].
pub fn pt_levels(next_page: u64) -> u8 {
    (PT_MIN_LEVELS..PT_MAX_LEVELS)
        .find(|&levels| pt_capacity(levels) >= next_page)
        .unwrap_or(PT_MAX_LEVELS)
}

/// The slot in each node on the way from the root to the leaf entry of a logical page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtPath {
    levels: u8,
    slots: [u16; PT_MAX_LEVELS as usize],
}

impl PtPath {
    /// The path of `page` in a page table of `levels` levels. A page that the table cannot address, or a level count outside 1 to 6, gives `None`.
    pub fn new(page: PageId, levels: u8) -> Option<PtPath> {
        if levels == 0 || levels > PT_MAX_LEVELS {
            return None;
        }
        if levels < PT_MAX_LEVELS && page.0 >= pt_capacity(levels) {
            return None;
        }
        let mut slots = [0u16; PT_MAX_LEVELS as usize];
        let mut rest = page.0;
        for slot in slots[..usize::from(levels)].iter_mut().rev() {
            *slot = (rest % PT_FANOUT as u64) as u16;
            rest /= PT_FANOUT as u64;
        }
        Some(PtPath { levels, slots })
    }

    /// The slots from the root to the leaf. Each slot is below [`PT_FANOUT`].
    pub fn slots(&self) -> &[u16] {
        &self.slots[..usize::from(self.levels)]
    }
}

/// A page table node in a page. The node's header holds its physical page number in the logical page field, because the page table cannot map its own pages.
///
/// The kind data holds the level at byte 0, where 0 is a leaf, and the first logical page that the node covers at bytes 8 to 15.
#[derive(Clone, Copy, Debug)]
pub struct PtNode;

impl PtNode {
    /// Sets up an empty node with its header. The caller seals the page before it writes it.
    pub fn init(page: &mut Page, physical: u64, level: u8, first: PageId) {
        page.fill(0);
        let mut header = PageHeader::new(PageId(physical), PageKind::PageTable);
        header.kind_data[0] = level;
        header.kind_data[8..16].copy_from_slice(&first.0.to_le_bytes());
        header.lower = PAGE_SIZE as u16 - 1;
        header.upper = PAGE_SIZE as u16 - 1;
        header.write(page);
    }

    /// The level of the node, 0 for a leaf.
    pub fn level(page: &Page) -> u8 {
        page[40]
    }

    /// The first logical page that the node covers.
    pub fn first(page: &Page) -> PageId {
        PageId(u64_at(page, 48))
    }

    /// The entry in `slot`. A slot of [`PT_FANOUT`] or more gives the empty entry.
    pub fn entry(page: &Page, slot: u16) -> PtEntry {
        match Self::offset(slot) {
            Some(at) => PtEntry(u64_at(page, at)),
            None => PtEntry::EMPTY,
        }
    }

    /// Sets the entry in `slot`. A slot of [`PT_FANOUT`] or more gives SQLSTATE `XX000`.
    pub fn set_entry(page: &mut Page, slot: u16, entry: PtEntry) -> Result<()> {
        let at = Self::offset(slot)
            .ok_or_else(|| Error::internal(format!("page table slot {slot} is out of range")))?;
        put_u64(page, at, entry.0);
        Ok(())
    }

    fn offset(slot: u16) -> Option<usize> {
        let slot = usize::from(slot);
        (slot < PT_FANOUT).then_some(PAGE_HEADER_SIZE + slot * 8)
    }

    /// The text form: the page header without the kind data, the level, the first page, and each entry that is not empty as `entry_NNNN`.
    pub fn to_text(page: &Page) -> Result<String> {
        let header = PageHeader::read(page)?;
        let mut out = TextOut::new();
        header.put_common(&mut out);
        out.field("first", Self::first(page)).field("level", Self::level(page));
        for slot in 0..PT_FANOUT as u16 {
            let entry = Self::entry(page, slot);
            if entry != PtEntry::EMPTY {
                out.field(&format!("entry_{slot:04}"), entry);
            }
        }
        Ok(out.finish())
    }

    /// Reads the text form back into a sealed page.
    pub fn from_text(text: &str) -> Result<Box<Page>> {
        let mut input = TextIn::parse("page table node", text)?;
        let mut header = PageHeader::take_common(&mut input)?;
        if header.kind != PageKind::PageTable {
            return Err(bad(format!("the kind is {}", header.kind)));
        }
        header.kind_data[0] = input.take("level")?;
        header.kind_data[8..16].copy_from_slice(&input.take::<u64>("first")?.to_le_bytes());
        let mut page = Box::new([0u8; PAGE_SIZE]);
        header.write(&mut page);
        for (slot, entry) in input.take_prefixed("entry_") {
            let Some(slot) =
                slot.parse::<u16>().ok().filter(|&s| usize::from(s) < PT_FANOUT && slot.len() == 4)
            else {
                return Err(bad(format!("entry_{slot} is not a slot")));
            };
            let entry = entry
                .parse()
                .map_err(|()| bad(format!("entry_{slot:04} has the bad value \"{entry}\"")))?;
            Self::set_entry(&mut page, slot, entry)?;
        }
        input.finish()?;
        seal(&mut page);
        Ok(page)
    }
}

fn bad(message: String) -> Error {
    Error::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("bad text form of the page table node: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify;

    #[test]
    fn entries() {
        let ts = Hlc::from_bits(0x1234_5678_9abc_def0);
        let e = PtEntry::new(77, ts).unwrap();
        assert_eq!((e.physical(), e.tag()), (Some(77), 0xdef0));
        assert_eq!(e.bits(), 77 | 0xdef0 << 48);
        assert_eq!(PtEntry::new(0, ts), None);
        assert_eq!(PtEntry::new(MAX_PHYSICAL + 1, ts), None);
        assert!(PtEntry::new(MAX_PHYSICAL, ts).is_some());
        assert_eq!(PtEntry::EMPTY.physical(), None);
        // A tag with no physical page is still empty.
        assert_eq!(PtEntry::from_bits(5 << 48).physical(), None);
        assert_eq!(e.to_string(), "77 57072");
        assert_eq!("77 57072".parse(), Ok(e));
        assert_eq!("none".parse(), Ok(PtEntry::EMPTY));
        assert!("0 1".parse::<PtEntry>().is_err());
        assert!("5 65536".parse::<PtEntry>().is_err());
    }

    #[test]
    fn radix() {
        assert_eq!(pt_capacity(3), 8_489_664_000);
        assert_eq!(pt_capacity(6), u64::MAX);
        assert_eq!(pt_levels(0), 3);
        assert_eq!(pt_levels(8_489_664_000), 3);
        assert_eq!(pt_levels(8_489_664_001), 4);
        assert_eq!(pt_levels(u64::MAX), 6);

        assert_eq!(PtPath::new(PageId(0), 3).unwrap().slots(), [0, 0, 0]);
        assert_eq!(PtPath::new(PageId(2039), 3).unwrap().slots(), [0, 0, 2039]);
        assert_eq!(PtPath::new(PageId(2040), 3).unwrap().slots(), [0, 1, 0]);
        assert_eq!(PtPath::new(PageId(2040 * 2040 + 5), 3).unwrap().slots(), [1, 0, 5]);
        assert_eq!(PtPath::new(PageId(8_489_663_999), 3).unwrap().slots(), [2039, 2039, 2039]);
        assert_eq!(PtPath::new(PageId(8_489_664_000), 3), None);
        assert_eq!(PtPath::new(PageId(8_489_664_000), 4).unwrap().slots(), [1, 0, 0, 0]);
        assert_eq!(PtPath::new(PageId(u64::MAX), 6).unwrap().slots().len(), 6);
        assert_eq!(PtPath::new(PageId(1), 0), None);
        assert_eq!(PtPath::new(PageId(1), 7), None);
        // Each page has its own path, and the slots read back to the page number.
        for n in [0u64, 1, 2039, 2040, 4_161_599, 4_161_600, 123_456_789] {
            let back = PtPath::new(PageId(n), 3)
                .unwrap()
                .slots()
                .iter()
                .fold(0u64, |acc, &s| acc * 2040 + u64::from(s));
            assert_eq!(back, n);
        }
    }

    #[test]
    fn node() {
        let mut page = [0u8; PAGE_SIZE];
        PtNode::init(&mut page, 9, 1, PageId(2040 * 2040));
        let child = PtEntry::new(12, Hlc::from_bits(3)).unwrap();
        PtNode::set_entry(&mut page, 0, child).unwrap();
        PtNode::set_entry(&mut page, 2039, PtEntry::new(13, Hlc::from_bits(4)).unwrap()).unwrap();
        assert!(PtNode::set_entry(&mut page, 2040, child).is_err());
        assert_eq!(PtNode::entry(&page, 2040), PtEntry::EMPTY);
        // The last entry ends at the last byte of the page.
        assert_eq!(u64_at(&page, PAGE_SIZE - 8), PtNode::entry(&page, 2039).bits());
        seal(&mut page);
        assert_eq!(verify(&page, 9, PageId(9), None).unwrap().kind, PageKind::PageTable);
        assert_eq!((PtNode::level(&page), PtNode::first(&page)), (1, PageId(4_161_600)));

        let text = PtNode::to_text(&page).unwrap();
        assert!(text.starts_with("entry_0000 12 3\nentry_2039 13 4\nfirst 4161600\n"), "{text}");
        assert!(text.contains("\nlevel 1\n"), "{text}");
        let back = PtNode::from_text(&text).unwrap();
        assert_eq!(&back[..], &page[..]);

        assert!(PtNode::from_text(&text.replace("entry_2039", "entry_2040")).is_err());
        assert!(PtNode::from_text(&text.replace("entry_0000", "entry_0")).is_err());
        assert!(PtNode::from_text(&text.replace("12 3", "0 3")).is_err());
        assert!(PtNode::from_text(&text.replace("page_table", "free_space")).is_err());
    }
}

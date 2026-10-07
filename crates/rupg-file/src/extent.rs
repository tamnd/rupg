//! Extents of spec/08 section 8.4: runs of contiguous pages with no page header, in five size classes.

use std::fmt;
use std::str::FromStr;

use rupg_common::{Error, Result};

use crate::{ARENA_PAGES, FIRST_ARENA_PAGE, PAGE_SIZE, checksum};

/// The unit of checksum and of read inside an extent, 64 KiB.
pub const EXTENT_BLOCK: usize = 65536;

/// The pages in one [`EXTENT_BLOCK`].
pub const EXTENT_BLOCK_PAGES: u64 = 4;

/// The size class of an extent. Each class is 4 times the size of the class before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtentClass {
    /// 64 KiB, 4 pages.
    E0,
    /// 256 KiB, 16 pages.
    E1,
    /// 1 MiB, 64 pages.
    E2,
    /// 4 MiB, 256 pages.
    E3,
    /// 16 MiB, 1024 pages, a whole arena.
    E4,
}

impl ExtentClass {
    /// The classes from the smallest.
    pub const ALL: [ExtentClass; 5] =
        [ExtentClass::E0, ExtentClass::E1, ExtentClass::E2, ExtentClass::E3, ExtentClass::E4];

    /// The class number, 0 for E0 to 4 for E4.
    pub fn order(self) -> u8 {
        self as u8
    }

    /// The class with this number.
    pub fn from_order(order: u8) -> Option<ExtentClass> {
        ExtentClass::ALL.get(usize::from(order)).copied()
    }

    /// The size in pages.
    pub fn pages(self) -> u64 {
        4 << (2 * self.order())
    }

    /// The size in bytes.
    pub fn bytes(self) -> u64 {
        self.pages() * PAGE_SIZE as u64
    }

    /// The number of 64 KiB blocks.
    pub fn blocks(self) -> u64 {
        self.pages() / EXTENT_BLOCK_PAGES
    }

    /// The smallest class that holds `bytes`, or `None` above 16 MiB. A larger column uses a list of E4 extents.
    pub fn for_bytes(bytes: u64) -> Option<ExtentClass> {
        ExtentClass::ALL.into_iter().find(|c| c.bytes() >= bytes)
    }

    /// The name in the text form, `e0` to `e4`.
    pub fn name(self) -> &'static str {
        ["e0", "e1", "e2", "e3", "e4"][usize::from(self.order())]
    }
}

impl fmt::Display for ExtentClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ExtentClass {
    type Err = ();

    fn from_str(s: &str) -> Result<ExtentClass, ()> {
        ExtentClass::ALL.into_iter().find(|c| c.name() == s).ok_or(())
    }
}

/// An extent: its first physical page and its class.
///
/// An extent is aligned to its size inside its arena. The page offset from the start of the arena is a multiple of the class size in pages, so the extent never crosses an arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent {
    start: u64,
    class: ExtentClass,
}

impl Extent {
    /// The extent at the physical page `start`. A start before the first arena, or a start that is not aligned to the class inside its arena, gives `None`.
    pub fn new(start: u64, class: ExtentClass) -> Option<Extent> {
        let offset = start.checked_sub(FIRST_ARENA_PAGE)? % ARENA_PAGES;
        offset.is_multiple_of(class.pages()).then_some(Extent { start, class })
    }

    /// The first physical page.
    pub fn start(self) -> u64 {
        self.start
    }

    /// The class.
    pub fn class(self) -> ExtentClass {
        self.class
    }

    /// The physical page after the last page.
    pub fn end(self) -> u64 {
        self.start + self.class.pages()
    }

    /// The arena of the extent.
    pub fn arena(self) -> u64 {
        (self.start - FIRST_ARENA_PAGE) / ARENA_PAGES
    }

    /// The byte offset of the extent in the file.
    pub fn byte_offset(self) -> u64 {
        self.start * PAGE_SIZE as u64
    }

    /// The checksum of each 64 KiB block of `data`. The last block can be shorter, for a packed extent that is not full.
    pub fn block_checksums(data: &[u8]) -> Vec<u64> {
        data.chunks(EXTENT_BLOCK).map(checksum).collect()
    }

    /// Checks block `index` of the extent against the checksum that the segment directory holds. A mismatch gives SQLSTATE `XX001` and names the extent and the block.
    pub fn verify_block(self, index: u64, data: &[u8], expected: u64) -> Result<()> {
        if index >= self.class.blocks() || data.len() > EXTENT_BLOCK {
            return Err(Error::internal(format!(
                "block {index} of {} bytes is not a block of the extent {self}",
                data.len()
            )));
        }
        let sum = checksum(data);
        if sum != expected {
            return Err(Error::corrupted("an extent block has a bad checksum").with_detail(format!(
                "Extent {self}, block {index}, physical page {}. The expected checksum is {expected:#018x} and the computed checksum is {sum:#018x}.",
                self.start + index * EXTENT_BLOCK_PAGES
            )));
        }
        Ok(())
    }
}

impl fmt::Display for Extent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.class, self.start)
    }
}

impl FromStr for Extent {
    type Err = ();

    fn from_str(s: &str) -> Result<Extent, ()> {
        let (class, start) = s.split_once('@').ok_or(())?;
        Extent::new(start.parse().map_err(|_| ())?, class.parse()?).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    #[test]
    fn classes() {
        let pages: Vec<u64> = ExtentClass::ALL.iter().map(|c| c.pages()).collect();
        assert_eq!(pages, [4, 16, 64, 256, 1024]);
        assert_eq!(ExtentClass::E2.bytes(), 1 << 20);
        assert_eq!(ExtentClass::E4.bytes(), 16 << 20);
        assert_eq!(ExtentClass::E1.blocks(), 4);
        assert_eq!(ExtentClass::for_bytes(0), Some(ExtentClass::E0));
        assert_eq!(ExtentClass::for_bytes(65_536), Some(ExtentClass::E0));
        assert_eq!(ExtentClass::for_bytes(65_537), Some(ExtentClass::E1));
        assert_eq!(ExtentClass::for_bytes(16 << 20), Some(ExtentClass::E4));
        assert_eq!(ExtentClass::for_bytes((16 << 20) + 1), None);
        for c in ExtentClass::ALL {
            assert_eq!(ExtentClass::from_order(c.order()), Some(c));
            assert_eq!(c.name().parse(), Ok(c));
        }
        assert_eq!(ExtentClass::from_order(5), None);
    }

    #[test]
    fn alignment() {
        // Arena 0 starts at page 3, arena 1 at page 1027.
        assert!(Extent::new(3, ExtentClass::E4).is_some());
        assert!(Extent::new(1027, ExtentClass::E4).is_some());
        assert!(Extent::new(1024, ExtentClass::E4).is_none());
        assert!(Extent::new(3 + 64, ExtentClass::E2).is_some());
        assert!(Extent::new(64, ExtentClass::E2).is_none());
        assert!(Extent::new(2, ExtentClass::E0).is_none());
        let e = Extent::new(1027 + 256, ExtentClass::E3).unwrap();
        assert_eq!((e.arena(), e.end(), e.byte_offset()), (1, 1027 + 512, (1027 + 256) * 16384));
        assert_eq!(e.to_string(), "e3@1283");
        assert_eq!("e3@1283".parse(), Ok(e));
        assert!("e3@1284".parse::<Extent>().is_err());
        assert!("e9@3".parse::<Extent>().is_err());
        // Every aligned extent of every class stays inside its arena.
        for c in ExtentClass::ALL {
            for start in 3..3 + 2 * 1024 {
                if let Some(e) = Extent::new(start, c) {
                    assert_eq!(e.arena(), (e.end() - 1 - 3) / 1024, "{e}");
                }
            }
        }
    }

    #[test]
    fn blocks() {
        let data: Vec<u8> = (0..3 * EXTENT_BLOCK + 100).map(|i| (i % 251) as u8).collect();
        let sums = Extent::block_checksums(&data);
        assert_eq!(sums.len(), 4);
        let e = Extent::new(3, ExtentClass::E1).unwrap();
        e.verify_block(1, &data[EXTENT_BLOCK..2 * EXTENT_BLOCK], sums[1]).unwrap();
        e.verify_block(3, &data[3 * EXTENT_BLOCK..], sums[3]).unwrap();
        let err = e.verify_block(2, &data[EXTENT_BLOCK..2 * EXTENT_BLOCK], sums[2]).unwrap_err();
        assert_eq!(err.state(), SqlState::DATA_CORRUPTED);
        assert!(err.detail().unwrap().starts_with("Extent e1@3, block 2, physical page 11."));
        assert_eq!(e.verify_block(4, &[], 0).unwrap_err().state(), SqlState::INTERNAL_ERROR);
    }
}

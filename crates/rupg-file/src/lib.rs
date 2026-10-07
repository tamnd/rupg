//! The bytes of the `.rupg` file: the header slots, the page and extent formats, the free space map, the checksums, `PageId`, and the `PageAccess` trait.
//!
//! This is a format crate (spec/22 section 22.4). It reads and writes bytes in memory and does no I/O. `rupg-buffer` and `rupg-log` do the I/O through `rupg-platform` and use this crate to read the bytes. A test can build a page or a header slot in a byte array and check it with no disk. The layout is in `spec/08-the-file.md`.

#![forbid(unsafe_code)]

mod access;
mod arena;
mod checksum;
mod extent;
mod freelist;
mod identity;
mod le;
mod owner;
mod page;
mod pagetable;
mod ring;
mod slot;
pub mod text;

use std::fmt;
use std::str::FromStr;

pub use access::{OptimisticRead, PageAccess, in_page};
pub use arena::{
    ARENA_PAGES, ArenaKind, ArenaMap, FSM_ARENAS_PER_PAGE, FsmEntry, FsmPage, arena_of, arena_start,
};
pub use checksum::{CHECKSUM_ALGORITHM, checksum};
pub use extent::{EXTENT_BLOCK, EXTENT_BLOCK_PAGES, Extent, ExtentClass};
pub use freelist::{FREE_LIST_PER_PAGE, FreeListPage};
pub use identity::{FORMAT_MAJOR, FORMAT_MINOR, Identity, MAGIC};
pub use owner::{OWNER_MAGIC, OWNER_OFFSET, Owner, SOCKET_MAX};
pub use page::{
    PAGE_HEADER_SIZE, PageFlags, PageHeader, PageKind, seal, stored_checksum, verify, write_tag,
};
pub use pagetable::{
    MAX_PHYSICAL, PT_FANOUT, PT_MAX_LEVELS, PT_MIN_LEVELS, PtEntry, PtNode, PtPath, pt_capacity,
    pt_levels,
};
pub use ring::{
    RING_ENTRY_SIZE, RING_EXTENT_BYTES, RING_EXTENTS_PER_PAGE, RINGS_PER_PAGE, RingDirectoryPage,
    RingEntry, RingExtentsPage,
};
pub use slot::{Root, SLOT_MAGIC, Slot, SlotChoice, SlotName, choose_slot};

/// The size of a page, 16 KiB.
pub const PAGE_SIZE: usize = 16384;

/// The base 2 logarithm of [`PAGE_SIZE`], as in the identity block.
pub const PAGE_SHIFT: u8 = 14;

/// The size of the identity block, the owner record and a header slot. Each one is a single aligned write.
pub const BLOCK_SIZE: usize = 4096;

/// The physical page of header slot A.
pub const SLOT_A_PAGE: u64 = 1;

/// The physical page of header slot B.
pub const SLOT_B_PAGE: u64 = 2;

/// The first physical page that belongs to an arena.
pub const FIRST_ARENA_PAGE: u64 = 3;

/// A page in memory.
pub type Page = [u8; PAGE_SIZE];

/// A 4 KiB block in memory.
pub type Block = [u8; BLOCK_SIZE];

/// The 128-bit file id. It is random at create, and the identity block, each slot and the owner record hold it.
pub type FileId = [u8; 16];

/// A logical page number. The page table maps it to a physical page number (spec/08 section 8.5). Each page holds its own logical page number in its header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u64);

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for PageId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<PageId, Self::Err> {
        s.parse().map(PageId)
    }
}

/// The three feature flag words of spec/08 section 8.15, in the style of the ext4 superblock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Features {
    /// A reader that does not know a set bit opens the file normally.
    pub compat: u64,
    /// A reader that does not know a set bit opens the file read-only.
    pub ro_compat: u64,
    /// A reader that does not know a set bit refuses to open the file.
    pub incompat: u64,
}

/// What a reader can do with a file, from its feature flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The reader knows each read-only-compatible and incompatible feature.
    ReadWrite,
    /// The file has a read-only-compatible feature that the reader does not know.
    ReadOnly,
}

impl Features {
    /// No feature.
    pub const NONE: Features = Features { compat: 0, ro_compat: 0, incompat: 0 };

    /// The features that this version of rupg knows. M1 defines none.
    pub const KNOWN: Features = Features::NONE;

    /// What a reader that knows the features `known` can do with a file that has these features. A file with an incompatible feature that the reader does not know gives SQLSTATE `0A000`, with the bit numbers.
    pub fn access(&self, known: &Features) -> rupg_common::Result<Access> {
        let unknown = self.incompat & !known.incompat;
        if unknown != 0 {
            return Err(rupg_common::Error::new(
                rupg_common::SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the file uses incompatible features that this rupg does not know: {}",
                    bits(unknown)
                ),
            )
            .with_hint("Open the file with a newer version of rupg."));
        }
        if self.ro_compat & !known.ro_compat != 0 {
            return Ok(Access::ReadOnly);
        }
        Ok(Access::ReadWrite)
    }

    fn put(&self, out: &mut text::TextOut) {
        out.hex("features_compat", self.compat)
            .hex("features_incompat", self.incompat)
            .hex("features_ro_compat", self.ro_compat);
    }

    fn take(input: &mut text::TextIn<'_>) -> rupg_common::Result<Features> {
        Ok(Features {
            compat: input.take_hex("features_compat")?,
            ro_compat: input.take_hex("features_ro_compat")?,
            incompat: input.take_hex("features_incompat")?,
        })
    }
}

/// The set bits of a word as "bit 3, bit 17".
fn bits(word: u64) -> String {
    (0..64)
        .filter(|i| word & (1 << i) != 0)
        .map(|i| format!("bit {i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    #[test]
    fn features() {
        let known = Features { compat: 1, ro_compat: 1, incompat: 1 };
        assert_eq!(Features::NONE.access(&Features::KNOWN).unwrap(), Access::ReadWrite);
        assert_eq!(Features { compat: 6, ..known }.access(&known).unwrap(), Access::ReadWrite);
        assert_eq!(Features { ro_compat: 2, ..known }.access(&known).unwrap(), Access::ReadOnly);
        let e = Features { incompat: 1 << 3 | 1 << 17 | 1, ..known }.access(&known).unwrap_err();
        assert_eq!(e.state(), SqlState::FEATURE_NOT_SUPPORTED);
        assert!(e.message().ends_with("bit 3, bit 17"), "{}", e.message());
    }

    #[test]
    fn page_id() {
        assert_eq!("42".parse::<PageId>().unwrap(), PageId(42));
        assert_eq!(PageId(7).to_string(), "7");
        assert!("-1".parse::<PageId>().is_err());
    }
}

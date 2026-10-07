//! The header slots A and B, bytes 0 to 4095 of pages 1 and 2 (spec/08 section 8.2.2).

use std::fmt;
use std::str::FromStr;

use rupg_common::{Error, Hlc, Result};

use crate::le::{block_is_sealed, get, put, put_u64, seal_block, u64_at};
use crate::text::{TextIn, TextOut, hex_bytes};
use crate::{Block, Features, FileId, SLOT_A_PAGE, SLOT_B_PAGE};

/// The magic bytes at offset 0 of a slot.
pub const SLOT_MAGIC: [u8; 8] = *b"RUPGSLOT";

/// A page that a slot names, with the checksum of that page. The open path checks the page against the checksum before it uses the page, so a lost write is found at open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Root {
    /// The page number. The page table root, the ring directory, the free space map and the pending free list use a physical number. The catalog and the shard map use a logical number.
    pub page: u64,
    /// The checksum in the header of that page.
    pub checksum: u64,
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:#018x}", self.page, self.checksum)
    }
}

impl FromStr for Root {
    type Err = ();

    fn from_str(s: &str) -> Result<Root, ()> {
        let (page, sum) = s.split_once(' ').ok_or(())?;
        let sum = sum.strip_prefix("0x").ok_or(())?;
        Ok(Root {
            page: page.parse().map_err(|_| ())?,
            checksum: u64::from_str_radix(sum, 16).map_err(|_| ())?,
        })
    }
}

/// A header slot. It names the state of the database as of one checkpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    /// Increases by 1 at each checkpoint.
    pub generation: u64,
    /// Equal to the file id of the identity block.
    pub file_id: FileId,
    /// The feature flags. The slot is authoritative over the identity block.
    pub features: Features,
    /// The time of the checkpoint.
    pub checkpoint: Hlc,
    /// The length of the file in pages.
    pub file_pages: u64,
    /// The root of the page table, a physical page.
    pub page_table: Root,
    /// The root of the catalog, a logical page.
    pub catalog: Root,
    /// The log ring directory, a physical page.
    pub ring_directory: Root,
    /// The root of the free space map, a physical page.
    pub free_space: Root,
    /// The root of the shard map, a logical page.
    pub shard_map: Root,
    /// The pending free list, a physical page.
    pub pending_free: Root,
    /// The next logical page number to give out.
    pub next_page: u64,
    /// The key check value of spec/08 section 8.11, zero when the file is not encrypted.
    pub key_check: [u8; 32],
    /// True when this checkpoint ended a clean shutdown.
    pub clean_shutdown: bool,
}

const ROOTS: [(&str, usize); 6] = [
    ("page_table", 72),
    ("catalog", 88),
    ("ring_directory", 104),
    ("free_space", 120),
    ("shard_map", 136),
    ("pending_free", 152),
];

impl Slot {
    fn roots(&self) -> [&Root; 6] {
        [
            &self.page_table,
            &self.catalog,
            &self.ring_directory,
            &self.free_space,
            &self.shard_map,
            &self.pending_free,
        ]
    }

    fn roots_mut(&mut self) -> [&mut Root; 6] {
        [
            &mut self.page_table,
            &mut self.catalog,
            &mut self.ring_directory,
            &mut self.free_space,
            &mut self.shard_map,
            &mut self.pending_free,
        ]
    }

    /// Writes the slot with its checksum.
    pub fn encode(&self, block: &mut Block) {
        block.fill(0);
        put(block, 0, &SLOT_MAGIC);
        put_u64(block, 8, self.generation);
        put(block, 16, &self.file_id);
        put_u64(block, 32, self.features.compat);
        put_u64(block, 40, self.features.ro_compat);
        put_u64(block, 48, self.features.incompat);
        put_u64(block, 56, self.checkpoint.bits());
        put_u64(block, 64, self.file_pages);
        for (root, (_, at)) in self.roots().into_iter().zip(ROOTS) {
            put_u64(block, at, root.page);
            put_u64(block, at + 8, root.checksum);
        }
        put_u64(block, 168, self.next_page);
        put(block, 176, &self.key_check);
        put_u64(block, 208, u64::from(self.clean_shutdown));
        seal_block(block);
    }

    /// Reads a slot and checks its magic and its checksum. A bad slot gives SQLSTATE `XX001`. The file id is checked by [`choose_slot`].
    pub fn decode(block: &Block) -> Result<Slot> {
        if get::<8>(block, 0) != SLOT_MAGIC {
            return Err(Error::corrupted("the header slot has a bad magic"));
        }
        if !block_is_sealed(block) {
            return Err(Error::corrupted("the header slot has a bad checksum"));
        }
        let mut slot = Slot {
            generation: u64_at(block, 8),
            file_id: get(block, 16),
            features: Features {
                compat: u64_at(block, 32),
                ro_compat: u64_at(block, 40),
                incompat: u64_at(block, 48),
            },
            checkpoint: Hlc::from_bits(u64_at(block, 56)),
            file_pages: u64_at(block, 64),
            next_page: u64_at(block, 168),
            key_check: get(block, 176),
            clean_shutdown: u64_at(block, 208) == 1,
            ..Slot::default()
        };
        for (root, (_, at)) in slot.roots_mut().into_iter().zip(ROOTS) {
            *root = Root { page: u64_at(block, at), checksum: u64_at(block, at + 8) };
        }
        Ok(slot)
    }

    /// The text form of spec/08 section 8.14.
    pub fn to_text(&self) -> String {
        let mut out = TextOut::new();
        out.field("checkpoint", self.checkpoint)
            .field("clean_shutdown", self.clean_shutdown)
            .field("file_id", hex_bytes(&self.file_id))
            .field("file_pages", self.file_pages)
            .field("generation", self.generation)
            .bytes("key_check", &self.key_check)
            .field("next_page", self.next_page);
        self.features.put(&mut out);
        for (root, (name, _)) in self.roots().into_iter().zip(ROOTS) {
            out.field(name, root);
        }
        out.finish()
    }

    /// Reads the text form back.
    pub fn from_text(text: &str) -> Result<Slot> {
        let mut input = TextIn::parse("header slot", text)?;
        let mut slot = Slot {
            generation: input.take("generation")?,
            file_id: input.take_bytes("file_id")?,
            features: Features::take(&mut input)?,
            checkpoint: input.take("checkpoint")?,
            file_pages: input.take("file_pages")?,
            next_page: input.take("next_page")?,
            key_check: input.take_bytes("key_check")?,
            clean_shutdown: input.take("clean_shutdown")?,
            ..Slot::default()
        };
        for (root, (name, _)) in slot.roots_mut().into_iter().zip(ROOTS) {
            *root = input.take(name)?;
        }
        input.finish()?;
        Ok(slot)
    }
}

/// The name of a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotName {
    /// Slot A, in page 1.
    A,
    /// Slot B, in page 2.
    B,
}

impl SlotName {
    /// The physical page of the slot.
    pub fn page(self) -> u64 {
        match self {
            SlotName::A => SLOT_A_PAGE,
            SlotName::B => SLOT_B_PAGE,
        }
    }

    /// The other slot. The next checkpoint writes it.
    pub fn other(self) -> SlotName {
        match self {
            SlotName::A => SlotName::B,
            SlotName::B => SlotName::A,
        }
    }
}

impl fmt::Display for SlotName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SlotName::A => "A",
            SlotName::B => "B",
        })
    }
}

/// Marks a slot as not valid when the identity block does not confirm its file id. The caller has found that the other slot does not confirm it.
fn unconfirmed(slot: &mut std::result::Result<Slot, String>, identity: Option<&FileId>) {
    let Ok(s) = slot else { return };
    if identity == Some(&s.file_id) {
        return;
    }
    *slot = Err(match identity {
        Some(_) => "the header slot has the file id of a different file".to_string(),
        None => "the header slot has a file id that no other block confirms".to_string(),
    });
}

/// The result of [`choose_slot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotChoice {
    /// The active slot.
    pub name: SlotName,
    /// Its content.
    pub slot: Slot,
    /// Why this slot is active, for `rupg inspect FILE header`.
    pub reason: String,
}

/// Chooses the active slot by the rule of spec/08 section 8.2.2. A slot is valid when its magic and its checksum are correct and its file id equals the identity block or the other valid slot. `identity` is `None` when the identity block is damaged. Of two valid slots the higher generation wins, and slot A wins a tie. If no slot is valid, the result is SQLSTATE `XX001` with a hint to `rupg inspect --salvage`.
pub fn choose_slot(identity: Option<&FileId>, a: &Block, b: &Block) -> Result<SlotChoice> {
    let read = |block: &Block| Slot::decode(block).map_err(|e| e.message().to_string());
    let (mut a, mut b) = (read(a), read(b));
    let agree = matches!((&a, &b), (Ok(x), Ok(y)) if x.file_id == y.file_id);
    if !agree {
        unconfirmed(&mut a, identity);
        unconfirmed(&mut b, identity);
    }
    match (a, b) {
        (Ok(a), Ok(b)) if b.generation > a.generation => Ok(SlotChoice {
            name: SlotName::B,
            reason: format!(
                "both slots are valid, and B has the higher generation {} > {}",
                b.generation, a.generation
            ),
            slot: b,
        }),
        (Ok(a), Ok(b)) => Ok(SlotChoice {
            name: SlotName::A,
            reason: if a.generation == b.generation {
                format!(
                    "both slots are valid with the same generation {}, and A wins a tie",
                    a.generation
                )
            } else {
                format!(
                    "both slots are valid, and A has the higher generation {} > {}",
                    a.generation, b.generation
                )
            },
            slot: a,
        }),
        (Ok(a), Err(why)) => {
            Ok(SlotChoice { name: SlotName::A, reason: format!("B is not valid: {why}"), slot: a })
        }
        (Err(why), Ok(b)) => {
            Ok(SlotChoice { name: SlotName::B, reason: format!("A is not valid: {why}"), slot: b })
        }
        (Err(why_a), Err(why_b)) => Err(Error::corrupted("the file has no valid header slot")
            .with_detail(format!("Slot A: {why_a}. Slot B: {why_b}."))
            .with_hint("Run rupg inspect --salvage on the file.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_common::SqlState;

    fn sample(generation: u64) -> Slot {
        Slot {
            generation,
            file_id: [9; 16],
            features: Features { compat: 1, ro_compat: 0, incompat: 0 },
            checkpoint: Hlc::new(5000, 1).unwrap(),
            file_pages: 1027,
            page_table: Root { page: 3, checksum: 0xaa },
            catalog: Root { page: 1, checksum: 0xbb },
            ring_directory: Root { page: 4, checksum: 0xcc },
            free_space: Root { page: 5, checksum: 0xdd },
            shard_map: Root { page: 0, checksum: 0 },
            pending_free: Root { page: 6, checksum: 0xee },
            next_page: 2,
            key_check: [0; 32],
            clean_shutdown: true,
        }
    }

    fn block(slot: &Slot) -> Block {
        let mut b = [0u8; 4096];
        slot.encode(&mut b);
        b
    }

    #[test]
    fn round_trip() {
        let slot = sample(4);
        let b = block(&slot);
        assert_eq!(&b[..8], b"RUPGSLOT");
        assert_eq!(u64_at(&b, 72), 3);
        assert_eq!(u64_at(&b, 160), 0xee);
        assert_eq!(u64_at(&b, 208), 1);
        assert_eq!(Slot::decode(&b).unwrap(), slot);
        let text = slot.to_text();
        assert!(text.contains("page_table 3 0x00000000000000aa\n"), "{text}");
        let lines: Vec<_> = text.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted);
        assert_eq!(Slot::from_text(&text).unwrap(), slot);
        assert!(Slot::from_text(&text.replace("page_table 3 0x", "page_table 3 ")).is_err());
    }

    #[test]
    fn choice() {
        let id = [9u8; 16];
        let (a, b) = (block(&sample(4)), block(&sample(5)));
        let choice = choose_slot(Some(&id), &a, &b).unwrap();
        assert_eq!((choice.name, choice.slot.generation), (SlotName::B, 5));
        assert_eq!(choose_slot(Some(&id), &b, &a).unwrap().name, SlotName::A);
        assert_eq!(choose_slot(Some(&id), &a, &a).unwrap().name, SlotName::A);

        // A torn write to B leaves A.
        let mut torn = b;
        torn[100..].fill(0);
        let choice = choose_slot(Some(&id), &a, &torn).unwrap();
        assert_eq!(choice.name, SlotName::A);
        assert_eq!(choice.reason, "B is not valid: the header slot has a bad checksum");

        // A damaged identity block: the two slots confirm each other.
        assert_eq!(choose_slot(None, &a, &b).unwrap().name, SlotName::B);
        // A damaged identity block and a torn slot: no block confirms the file id.
        let e = choose_slot(None, &a, &torn).unwrap_err();
        assert_eq!(e.state(), SqlState::DATA_CORRUPTED);
        assert_eq!(e.hint(), Some("Run rupg inspect --salvage on the file."));

        // A slot of a different file, for example from a copy of a page of another file.
        let mut stranger = sample(9);
        stranger.file_id = [1; 16];
        let choice = choose_slot(Some(&id), &a, &block(&stranger)).unwrap();
        assert_eq!(choice.name, SlotName::A);
        assert_eq!(choose_slot(Some(&id), &block(&stranger), &b).unwrap().name, SlotName::B);
        assert!(choose_slot(None, &a, &block(&stranger)).is_err());

        let zero = [0u8; 4096];
        let e = choose_slot(Some(&id), &zero, &zero).unwrap_err();
        assert_eq!(
            e.detail(),
            Some(
                "Slot A: the header slot has a bad magic. Slot B: the header slot has a bad magic."
            )
        );
        assert_eq!(
            (SlotName::A.page(), SlotName::B.page(), SlotName::A.other()),
            (1, 2, SlotName::B)
        );
    }
}

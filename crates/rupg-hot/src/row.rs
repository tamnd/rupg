//! A hot row and its version header (spec/10 section 10.3.1, spec/11 section 11.2.1).

use rupg_common::{Error, Result, RowId};

/// The 16 byte version header of a hot row.
///
/// On the page it is `stamp` as a `u64`, then `undo` in 6 bytes, then `flags` as a `u16`, all little endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VersionHeader {
    /// The commit timestamp of the newest version. With the [`VersionHeader::OWNED`] bit set, the low bits are the slot of the transaction that owns the row and did not commit.
    pub stamp: u64,
    /// The position of the newest undo record, or 0. It has 48 bits.
    pub undo: u64,
    /// The flags. See the constants of this type.
    pub flags: u16,
}

impl VersionHeader {
    /// The size on the page.
    pub const SIZE: usize = 16;
    /// The top bit of `stamp`: the row has an owner that did not commit.
    pub const OWNED: u64 = 1 << 63;
    /// The largest undo position.
    pub const MAX_UNDO: u64 = (1 << 48) - 1;

    /// The row is deleted.
    pub const DELETED: u16 = 1;
    /// The row moved to the cold store.
    pub const MOVED_TO_COLD: u16 = 1 << 1;
    /// The version before this one is in the cold store.
    pub const PRIOR_IN_COLD: u16 = 1 << 2;
    /// The row has TOAST values.
    pub const HAS_TOAST: u16 = 1 << 3;
    /// The owner only locks the row and did not change it.
    pub const LOCK_ONLY: u16 = 1 << 4;
    /// The two bits of the lock strength, from `FOR KEY SHARE` at 0 to `FOR UPDATE` at 3.
    pub const LOCK_STRENGTH: u16 = 0b11 << 5;
    /// A group of transactions holds the lock, and the lock table names them.
    pub const LOCK_GROUP: u16 = 1 << 7;

    /// A header for a row that the transaction in `slot` owns.
    pub fn owned_by(slot: u64, undo: u64, flags: u16) -> VersionHeader {
        VersionHeader { stamp: VersionHeader::OWNED | slot, undo, flags }
    }

    /// The slot of the owner, or `None` when `stamp` is a commit timestamp.
    pub fn owner(&self) -> Option<u64> {
        (self.stamp & VersionHeader::OWNED != 0).then_some(self.stamp & !VersionHeader::OWNED)
    }

    /// The bytes on the page. An undo position above 48 bits gives SQLSTATE `XX000`.
    pub fn encode(&self) -> Result<[u8; VersionHeader::SIZE]> {
        if self.undo > VersionHeader::MAX_UNDO {
            return Err(Error::internal(format!(
                "the undo position {} has more than 48 bits",
                self.undo
            )));
        }
        let mut out = [0; VersionHeader::SIZE];
        out[0..8].copy_from_slice(&self.stamp.to_le_bytes());
        out[8..14].copy_from_slice(&self.undo.to_le_bytes()[..6]);
        out[14..16].copy_from_slice(&self.flags.to_le_bytes());
        Ok(out)
    }

    /// The header in `bytes`.
    pub fn decode(bytes: &[u8; VersionHeader::SIZE]) -> VersionHeader {
        let mut undo = [0; 8];
        undo[..6].copy_from_slice(&bytes[8..14]);
        VersionHeader {
            stamp: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            undo: u64::from_le_bytes(undo),
            flags: u16::from_le_bytes([bytes[14], bytes[15]]),
        }
    }
}

/// A row of the hot store with its values in storage form. A value of `None` is NULL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The row id. It is the key of the hot store.
    pub id: RowId,
    /// The version header.
    pub header: VersionHeader,
    /// The low 32 bits of the id of the transaction that wrote the newest version.
    pub xmin: u32,
    /// The low 32 bits of the id of the deleter or locker, or 0.
    pub xmax: u32,
    /// One value for each column of the schema.
    pub values: Vec<Option<Vec<u8>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let h = VersionHeader {
            stamp: 0x0102_0304_0506_0708,
            undo: VersionHeader::MAX_UNDO,
            flags: 0xbeef,
        };
        assert_eq!(VersionHeader::decode(&h.encode().unwrap()), h);
        assert_eq!(h.owner(), None);
        let o = VersionHeader::owned_by(42, 7, VersionHeader::DELETED);
        assert_eq!((o.owner(), VersionHeader::decode(&o.encode().unwrap())), (Some(42), o));
        let bad = VersionHeader { undo: VersionHeader::MAX_UNDO + 1, ..h };
        assert!(bad.encode().is_err());
    }
}

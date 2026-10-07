//! The log block of spec/11 section 11.13.2.
//!
//! A block has a header of 40 bytes, the dependencies and a body. Its length is a multiple of 8. The checksum is the CRC32C of the whole block with the checksum field at zero, seeded with the ring number. The checksum of a fill block covers only its header, so a fill to the end of an extent costs one header and not 16 MiB of writes. A block that fails its checksum or holds a position other than the one where it is read is the end of the ring. A block that passes both checks and is still not valid is damage and gives SQLSTATE `XX001`.

use std::fmt;

use rupg_common::{Error, Hlc, Result};

use rupg_kernels::crc32c as crc;

/// The size of the block header.
pub const BLOCK_HEADER: usize = 40;

/// The size of one dependency.
pub const DEP_SIZE: usize = 10;

/// The largest block, the size of a ring extent. A block never spans two extents.
pub const MAX_BLOCK: usize = 16 << 20;

/// The unit of a ring write. A flush pads the last unit, so that it never writes a unit that is already durable.
pub const UNIT: u64 = 4096;

/// The kind of a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BlockKind {
    /// The records of a transaction and its commit.
    Commit = 1,
    /// A part of the records of a large transaction. Its commit block names it as a dependency.
    Part = 2,
    /// The rollback of a transaction that wrote part blocks.
    Abort = 3,
    /// `PREPARE TRANSACTION`, with the records.
    Prepare = 4,
    /// `COMMIT PREPARED`.
    CommitPrepared = 5,
    /// `ROLLBACK PREPARED`.
    AbortPrepared = 6,
    /// Sequence values that do not wait for a transaction.
    Sequence = 7,
    /// Padding to the end of a unit or of an extent. Replay skips it.
    Fill = 8,
}

impl BlockKind {
    /// Each kind, in the order of the number.
    pub const ALL: [BlockKind; 8] = [
        BlockKind::Commit,
        BlockKind::Part,
        BlockKind::Abort,
        BlockKind::Prepare,
        BlockKind::CommitPrepared,
        BlockKind::AbortPrepared,
        BlockKind::Sequence,
        BlockKind::Fill,
    ];

    /// The kind with this number.
    pub fn from_u8(n: u8) -> Option<BlockKind> {
        BlockKind::ALL.into_iter().find(|&k| k as u8 == n)
    }

    /// The name in the text form.
    pub fn name(self) -> &'static str {
        match self {
            BlockKind::Commit => "commit",
            BlockKind::Part => "part",
            BlockKind::Abort => "abort",
            BlockKind::Prepare => "prepare",
            BlockKind::CommitPrepared => "commit_prepared",
            BlockKind::AbortPrepared => "abort_prepared",
            BlockKind::Sequence => "sequence",
            BlockKind::Fill => "fill",
        }
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The flag of a block whose body is compressed. No version of rupg writes it yet, and a read gives `XX001`.
pub const FLAG_COMPRESSED: u8 = 1;
/// The flag of a block with at least one dependency.
pub const FLAG_DEPS: u8 = 2;

/// A dependency: the block at `position` in ring `ring` must be safe before this block is safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dep {
    /// The ring.
    pub ring: u16,
    /// The ring position just after the block. Ring `ring` must be safe at or above it.
    pub position: u64,
}

/// A log block. The ring writer sets the ring and the position when it places the block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The kind.
    pub kind: BlockKind,
    /// The ring number.
    pub ring: u16,
    /// The ring position of the first byte of the block.
    pub position: u64,
    /// The HLC commit timestamp, or zero for a block with no commit.
    pub commit_ts: Hlc,
    /// The full transaction id, or zero.
    pub xid: u64,
    /// The blocks that must be safe first.
    pub deps: Vec<Dep>,
    /// The records, in the format of [`crate::Record`].
    pub body: Vec<u8>,
}

/// The fields of a block header that a reader needs before it reads the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    /// The block length in bytes.
    pub length: usize,
    /// The ring number.
    pub ring: u16,
    /// The ring position.
    pub position: u64,
    /// True for a fill block. A reader reads only its header and goes on after `length` bytes.
    pub fill: bool,
}

impl BlockHeader {
    /// Reads the header at the start of `bytes`. It gives `None` when the length field cannot be the length of a block, which is the end of the ring. `bytes` must hold at least [`BLOCK_HEADER`] bytes.
    pub fn read(bytes: &[u8]) -> Option<BlockHeader> {
        let length = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        if !(BLOCK_HEADER..=MAX_BLOCK).contains(&length) || !length.is_multiple_of(8) {
            return None;
        }
        Some(BlockHeader {
            length,
            ring: u16::from_le_bytes(bytes[6..8].try_into().ok()?),
            position: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            fill: bytes[4] == BlockKind::Fill as u8,
        })
    }
}

/// The length of a block with `deps` dependencies and a body of `body` bytes, padded to a multiple of 8.
pub fn block_len(deps: usize, body: usize) -> usize {
    (BLOCK_HEADER + deps * DEP_SIZE + body).next_multiple_of(8)
}

fn checksum(bytes: &[u8], ring: u16) -> u32 {
    let bytes = if bytes[4] == BlockKind::Fill as u8 { &bytes[..BLOCK_HEADER] } else { bytes };
    let c = crc::extend(u32::from(ring), &bytes[..8]);
    let c = crc::extend(c, &[0; 4]);
    crc::extend(c, &bytes[12..])
}

impl Block {
    /// The encoded length.
    pub fn len(&self) -> usize {
        block_len(self.deps.len(), self.body.len())
    }

    /// Always false, because a block has a header.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Appends the block to `out`. A block longer than [`MAX_BLOCK`], a fill block, or more than 65,535 dependencies give SQLSTATE `XX000`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let len = self.len();
        if len > MAX_BLOCK || self.deps.len() > usize::from(u16::MAX) {
            return Err(Error::internal(format!(
                "a log block of {len} bytes with {} dependencies is too large",
                self.deps.len()
            )));
        }
        if self.kind == BlockKind::Fill {
            return Err(Error::internal("a fill block comes from Block::fill"));
        }
        let start = out.len();
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.push(self.kind as u8);
        out.push(if self.deps.is_empty() { 0 } else { FLAG_DEPS });
        out.extend_from_slice(&self.ring.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(self.deps.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(&self.position.to_le_bytes());
        out.extend_from_slice(&self.commit_ts.bits().to_le_bytes());
        out.extend_from_slice(&self.xid.to_le_bytes());
        for d in &self.deps {
            out.extend_from_slice(&d.ring.to_le_bytes());
            out.extend_from_slice(&d.position.to_le_bytes());
        }
        out.extend_from_slice(&self.body);
        out.resize(start + len, 0);
        let sum = checksum(&out[start..], self.ring);
        out[start + 8..start + 12].copy_from_slice(&sum.to_le_bytes());
        Ok(())
    }

    /// The header of a fill block of `len` bytes at `position`. The writer writes only these 40 bytes, and a reader skips the rest. `len` must be a multiple of 8 from [`BLOCK_HEADER`] to [`MAX_BLOCK`].
    pub fn fill(ring: u16, position: u64, len: usize) -> Result<[u8; BLOCK_HEADER]> {
        if !(BLOCK_HEADER..=MAX_BLOCK).contains(&len) || !len.is_multiple_of(8) {
            return Err(Error::internal(format!("a fill block cannot have {len} bytes")));
        }
        let mut out = [0; BLOCK_HEADER];
        out[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        out[4] = BlockKind::Fill as u8;
        out[6..8].copy_from_slice(&ring.to_le_bytes());
        out[16..24].copy_from_slice(&position.to_le_bytes());
        let sum = checksum(&out, ring);
        out[8..12].copy_from_slice(&sum.to_le_bytes());
        Ok(out)
    }

    /// Reads the block in `bytes`. For a fill block `bytes` is the header, and for other blocks it is exactly the length that [`BlockHeader::read`] gave. It gives `Ok(None)` when the checksum fails or the block is not the block of `ring` at `position`. That is the end of the ring. A block that passes those checks and is not valid gives SQLSTATE `XX001`. A fill block comes back with an empty body.
    pub fn decode(bytes: &[u8], ring: u16, position: u64) -> Result<Option<Block>> {
        Block::decode_with(bytes, ring, position, true)
    }

    /// [`Block::decode`], with an empty body when `body` is false. The checksum covers the body in both cases.
    pub fn decode_with(
        bytes: &[u8],
        ring: u16,
        position: u64,
        body: bool,
    ) -> Result<Option<Block>> {
        let Some(h) = (bytes.len() >= BLOCK_HEADER).then(|| BlockHeader::read(bytes)).flatten()
        else {
            return Ok(None);
        };
        let bytes = if h.fill { &bytes[..BLOCK_HEADER] } else { bytes };
        if (!h.fill && h.length != bytes.len()) || h.ring != ring || h.position != position {
            return Ok(None);
        }
        let stored = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if checksum(bytes, ring) != stored {
            return Ok(None);
        }
        let bad = |what: String| {
            Error::corrupted(format!("the log block of ring {ring} at position {position} {what}"))
        };
        let kind = BlockKind::from_u8(bytes[4])
            .ok_or_else(|| bad(format!("has the unknown kind {}", bytes[4])))?;
        let flags = bytes[5];
        let deps = usize::from(u16::from_le_bytes([bytes[12], bytes[13]]));
        if flags & FLAG_COMPRESSED != 0 || flags & !(FLAG_COMPRESSED | FLAG_DEPS) != 0 {
            return Err(bad(format!("has the flags {flags:#04x}, which this rupg cannot read")));
        }
        if (flags & FLAG_DEPS != 0) != (deps > 0) || bytes[14..16] != [0, 0] {
            return Err(bad("has a bad header".to_string()));
        }
        if h.fill && bytes[24..40].iter().any(|&b| b != 0) {
            return Err(bad("is a fill block with content".to_string()));
        }
        let start = BLOCK_HEADER + deps * DEP_SIZE;
        if start > bytes.len() {
            return Err(bad(format!("has {deps} dependencies, which do not fit")));
        }
        let deps = bytes[BLOCK_HEADER..start]
            .as_chunks::<DEP_SIZE>()
            .0
            .iter()
            .map(|&[r0, r1, p @ ..]| Dep {
                ring: u16::from_le_bytes([r0, r1]),
                position: u64::from_le_bytes(p),
            })
            .collect::<Vec<_>>();
        let block = Block {
            kind,
            ring,
            position,
            commit_ts: Hlc::from_bits(u64::from_le_bytes(
                bytes[24..32].try_into().unwrap_or_default(),
            )),
            xid: u64::from_le_bytes(bytes[32..40].try_into().unwrap_or_default()),
            deps,
            body: if body { bytes[start..].to_vec() } else { Vec::new() },
        };
        Ok(Some(block))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(ring: u16, position: u64) -> Block {
        Block {
            kind: BlockKind::Commit,
            ring,
            position,
            commit_ts: Hlc::new(5000, 2).unwrap(),
            xid: 77,
            deps: vec![Dep { ring: 3, position: 4096 }, Dep { ring: 9, position: 80 }],
            body: b"some records".to_vec(),
        }
    }

    #[test]
    fn round_trip() {
        let b = sample(2, 8192);
        let mut out = vec![1, 2, 3];
        b.encode(&mut out).unwrap();
        let bytes = &out[3..];
        assert_eq!(bytes.len(), b.len());
        assert_eq!(b.len(), 40 + 20 + 12);
        assert_eq!(
            BlockHeader::read(bytes),
            Some(BlockHeader { length: 72, ring: 2, position: 8192, fill: false })
        );
        assert_eq!(Block::decode(bytes, 2, 8192).unwrap(), Some(b.clone()));

        // The body of a decoded block keeps the zero padding.
        let odd = Block { body: b"13 bytes long".to_vec(), ..b.clone() };
        let mut out = Vec::new();
        odd.encode(&mut out).unwrap();
        assert_eq!(out.len(), 80);
        let back = Block::decode(&out, 2, 8192).unwrap().unwrap();
        assert_eq!(back.body, [&b"13 bytes long"[..], &[0; 7]].concat());

        let none = Block { deps: Vec::new(), body: Vec::new(), kind: BlockKind::Abort, ..b };
        let mut out = Vec::new();
        none.encode(&mut out).unwrap();
        assert_eq!(out.len(), BLOCK_HEADER);
        assert_eq!(out[5], 0);
        assert_eq!(Block::decode(&out, 2, 8192).unwrap(), Some(none));
    }

    #[test]
    fn the_end_of_a_ring() {
        let mut out = Vec::new();
        sample(2, 8192).encode(&mut out).unwrap();
        // Another ring, another position, a changed byte or a short read is the end.
        assert_eq!(Block::decode(&out, 3, 8192).unwrap(), None);
        assert_eq!(Block::decode(&out, 2, 8192 + 4096).unwrap(), None);
        assert_eq!(Block::decode(&out[..out.len() - 8], 2, 8192).unwrap(), None);
        for i in 0..out.len() {
            let mut odd = out.clone();
            odd[i] ^= 0x10;
            assert_eq!(Block::decode(&odd, 2, 8192).unwrap(), None, "byte {i}");
        }
        assert_eq!(BlockHeader::read(&[0; 40]), None);
        let mut short = out.clone();
        short[0..4].copy_from_slice(&36u32.to_le_bytes());
        assert_eq!(BlockHeader::read(&short), None);
        short[0..4].copy_from_slice(&81u32.to_le_bytes());
        assert_eq!(BlockHeader::read(&short), None);
        short[0..4].copy_from_slice(&(MAX_BLOCK as u32 + 8).to_le_bytes());
        assert_eq!(BlockHeader::read(&short), None);
    }

    /// Sets a byte and makes the checksum right again, as damage that a checksum does not find.
    fn reseal(bytes: &mut [u8], at: usize, value: u8, ring: u16) {
        bytes[at] = value;
        let sum = checksum(bytes, ring);
        bytes[8..12].copy_from_slice(&sum.to_le_bytes());
    }

    #[test]
    fn damage() {
        let mut out = Vec::new();
        sample(2, 8192).encode(&mut out).unwrap();
        for (at, value) in [(4, 0), (4, 9), (5, 0), (5, 3), (5, 4 | 2), (12, 0), (12, 7), (14, 1)] {
            let mut odd = out.clone();
            reseal(&mut odd, at, value, 2);
            let e = Block::decode(&odd, 2, 8192).unwrap_err();
            assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED, "byte {at} value {value}");
        }
        let fill = Block::fill(2, 0, 64).unwrap();
        for at in [5, 12, 14, 24, 32, 39] {
            let mut odd = fill;
            reseal(&mut odd, at, 1, 2);
            let e = Block::decode(&odd, 2, 0).unwrap_err();
            assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED, "byte {at}");
        }
    }

    #[test]
    fn fill_blocks() {
        for len in [40, 48, 4096, MAX_BLOCK] {
            let fill = Block::fill(7, 12_288, len).unwrap();
            let h = BlockHeader::read(&fill).unwrap();
            assert_eq!((h.length, h.ring, h.position, h.fill), (len, 7, 12_288, true));
            let b = Block::decode(&fill, 7, 12_288).unwrap().unwrap();
            assert_eq!((b.kind, b.body.len()), (BlockKind::Fill, 0));
            // The bytes after the header do not count.
            let mut more = fill.to_vec();
            more.extend_from_slice(&[0xee; 64]);
            assert_eq!(Block::decode(&more, 7, 12_288).unwrap(), Some(b));
            assert_eq!(Block::decode(&fill, 7, 0).unwrap(), None);
        }
        for len in [0, 32, 44, MAX_BLOCK + 8] {
            assert!(Block::fill(7, 0, len).is_err());
        }
        let bad = Block { kind: BlockKind::Fill, ..sample(1, 0) };
        assert!(bad.encode(&mut Vec::new()).is_err());
        let big = Block { body: vec![0; MAX_BLOCK], ..sample(1, 0) };
        assert!(big.encode(&mut Vec::new()).is_err());
        // A commit block whose kind byte turns into a fill fails its checksum.
        let mut out = Vec::new();
        sample(2, 0).encode(&mut out).unwrap();
        out[4] = BlockKind::Fill as u8;
        assert_eq!(Block::decode(&out, 2, 0).unwrap(), None);
    }

    #[test]
    fn kinds() {
        for (i, k) in BlockKind::ALL.into_iter().enumerate() {
            assert_eq!(k as usize, i + 1);
            assert_eq!(BlockKind::from_u8(k as u8), Some(k));
        }
        assert_eq!(BlockKind::from_u8(0), None);
        assert_eq!(BlockKind::from_u8(9), None);
        assert_eq!(BlockKind::CommitPrepared.to_string(), "commit_prepared");
    }
}

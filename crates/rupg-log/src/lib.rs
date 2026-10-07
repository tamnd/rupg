//! The per-worker log rings, the record format, group commit, checkpoints, recovery, the replication stream.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod block;
mod crc;
mod record;
mod ring;
mod rings;
mod varint;

pub use block::{
    BLOCK_HEADER, Block, BlockHeader, BlockKind, DEP_SIZE, Dep, FLAG_COMPRESSED, FLAG_DEPS,
    MAX_BLOCK, UNIT, block_len,
};
pub use record::{Record, RecordKind, RecordReader, RecordWriter};
pub use ring::{Placed, Ring, RingReader};
pub use rings::Log;

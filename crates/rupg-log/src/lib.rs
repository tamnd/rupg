//! The per-worker log rings, the record format, group commit, checkpoints, recovery, the replication stream.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod block;
mod record;
mod recovery;
mod ring;
mod rings;
mod varint;

/// The lock and the condition variable of a ring. The loom tests build the crate with `--cfg loom` and get the types of loom. See the loom test in `ring.rs`.
mod sync {
    #[cfg(all(test, loom))]
    pub(crate) use loom::sync::{Condvar, Mutex, MutexGuard};
    #[cfg(not(all(test, loom)))]
    pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};

    /// A mark for the I/O of a ring. Loom reorders only the steps that use the same loom object, and the store is not a loom object. So under loom each write and each sync of a ring, and each read of the test, first changes this atomic, and loom then tries the I/O of one thread before and after the steps of the others.
    #[cfg(all(test, loom))]
    #[derive(Debug)]
    pub(crate) struct IoMark(loom::sync::atomic::AtomicUsize);

    #[cfg(all(test, loom))]
    impl IoMark {
        pub(crate) fn new() -> IoMark {
            IoMark(loom::sync::atomic::AtomicUsize::new(0))
        }

        pub(crate) fn touch(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Outside the loom tests the mark is empty and does nothing.
    #[cfg(not(all(test, loom)))]
    #[derive(Debug)]
    pub(crate) struct IoMark;

    #[cfg(not(all(test, loom)))]
    impl IoMark {
        pub(crate) fn new() -> IoMark {
            IoMark
        }

        #[inline]
        pub(crate) fn touch(&self) {}
    }
}

pub use block::{
    BLOCK_HEADER, Block, BlockHeader, BlockKind, DEP_SIZE, Dep, FLAG_COMPRESSED, FLAG_DEPS,
    MAX_BLOCK, UNIT, block_len,
};
pub use record::{Record, RecordKind, RecordReader, RecordWriter};
pub use recovery::{Found, Recovery};
pub use ring::{Placed, Ring, RingReader};
pub use rings::Log;

//! The buffer manager of document 08 section 8.8: frames, page latches, eviction, the I/O queue, the implementation of `PageAccess`.
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

/// The atomics of the latch. The loom tests include `latch.rs` again in a module that gives it the atomics of loom here.
use std::sync::atomic;

mod file_store;
mod frames;
mod latch;
#[cfg(all(test, loom))]
mod loom_tests;
mod pool;
mod resident;
mod store;

pub use file_store::{Checkpoint, FileStats, FileStore, RingState};
pub use pool::{
    BufferPool, BufferStats, ExclusiveGuard, Filtered, MIN_FRAMES, OptimisticGuard, PageFilter,
    PoolConfig, PoolMode, SharedGuard,
};
pub use store::{MemStore, Store};

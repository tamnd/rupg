//! The buffer manager of document 08 section 8.8: frames, page latches, eviction, the I/O queue, the implementation of `PageAccess`.
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

mod frames;
mod latch;
mod pool;
mod store;

pub use pool::{BufferPool, BufferStats, ExclusiveGuard, MIN_FRAMES, OptimisticGuard, SharedGuard};
pub use store::{MemStore, Store};

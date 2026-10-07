//! A B+ tree over `PageAccess` with optimistic latch coupling, used by the hot store, the catalog, TOAST and the B-tree index.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod inner;
pub mod mem;
mod tree;

pub use inner::MAX_KEY;
pub use mem::MemPages;
pub use tree::{Leaf, Shape, Tree, Write};

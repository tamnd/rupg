//! A table as one object over both stores, TOAST, the system columns, the row id allocator per shard, the scan that merges the stores.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod def;
mod table;

pub use def::{ColumnDef, TableDef};
pub use table::Table;

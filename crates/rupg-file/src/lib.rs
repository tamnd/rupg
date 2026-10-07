//! The bytes of the `.rupg` file: the header slots, the page and extent formats, the free space map, the checksums, `PageId`, and the `PageAccess` trait.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

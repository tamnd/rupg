//! MVCC, snapshots, undo, row and advisory locks, SSI, savepoints, two-phase commit, commit timestamps, the mover of document 04 section 4.4.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod clock;
mod visible;

pub use clock::HlcClock;
pub use visible::Commits;

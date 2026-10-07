//! One Raft group per shard: the election, the log, snapshots, membership changes.
//!
//! This crate first ships in milestone M10. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

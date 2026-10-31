//! The scalar summaries, zone maps, frequency synopses and gram sets of H1 to H4 as plain data.
//!
//! This crate first ships in milestone M4. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.
//!
//! M4 ships the summary of a segment column (spec/09 section 9.7): [`key`] maps a value to a monotone `u64`, [`zone`] tests a filter against the bounds of a vector or a segment, [`codes`] holds the set of dictionary codes of a segment, and [`column`] builds, writes and reads the summary. A summary is a cache of facts about the rows. Deleting a summary changes no answer.

#![forbid(unsafe_code)]

pub mod codes;
pub mod column;
pub mod key;
pub mod zone;

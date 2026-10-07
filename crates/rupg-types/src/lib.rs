//! The value model, `Datum`, `Vector` of 1024 values, `TypeId`, the text and binary forms of the 197 types, numeric, date and time, JSON, arrays, ranges.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod datum;
mod typeid;

pub use datum::Datum;
pub use typeid::TypeId;

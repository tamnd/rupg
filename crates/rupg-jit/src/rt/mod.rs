//! The runtime that compiled pipelines call, lifted from rudb's `rudb-qc-rt` (spec/22 section 22.7): the morsel and state ABI, `str16` strings and the heap they live in, `LIKE` patterns, the aggregation and join hash tables, the executable memory that the native tiers load their code into, in `native` the `extern "C"` entries that native code calls, and [`Rt`], which implements each function of the catalogue for whichever tier runs the code.

pub mod abi;
mod ablate;
pub mod code;
pub mod join;
pub mod like;
mod mem;
pub mod native;
mod runtime;
pub mod table;
pub mod text;

pub use ablate::Ablate;
pub use runtime::{Kernel, Maker, Pattern, Rt, hash};

/// The payload of the error status that means a runtime function failed, and the runtime says why. It is rudb's `RUNTIME_ERROR` of `rudb-qc-rt/src/rt.rs`.
pub const RUNTIME_ERROR: u64 = 0xff_ffff;

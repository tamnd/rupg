//! The runtime that compiled pipelines call, lifted from rudb's `rudb-qc-rt` (spec/22 section 22.7): the morsel and state ABI, `str16` strings and the heap they live in, `LIKE` patterns, the aggregation hash tables, and the executable memory that the native tiers load their code into. The functions of the catalogue that compiled code calls come from `rupg-func` when the tiers are lifted.

pub mod abi;
pub mod code;
pub mod like;
mod mem;
pub mod table;
pub mod text;

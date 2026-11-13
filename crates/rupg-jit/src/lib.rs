//! The compile tiers of document 14, lifted from rudb's query compiler.
//!
//! [`ir`] is QIR, the typed SSA form with block parameters, an explicit null model and a text form that round-trips (document 21 section 21.6), lifted from `rudb-qc-ir`. [`interp`] is the first tier, which runs a QIR function with no machine code, lifted from `rudb-qc-interp`. [`rt`] is the runtime that compiled pipelines call: the morsel ABI, `str16` strings, `LIKE` patterns, the aggregation hash tables and the executable memory of the native tiers, lifted from `rudb-qc-rt`. [`types`] gives the QIR type of each PostgreSQL type that fits a machine value. [`direct`] is the single pass x86-64 tier, which compiles a QIR pipeline function to machine code with no dependencies, lifted from `rudb-qc-direct`. The functions that compiled code calls and the Cranelift tier come in later lifts (document 22 section 22.7).
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M6. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod direct;
pub mod interp;
pub mod ir;
pub mod rt;
pub mod types;

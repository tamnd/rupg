//! The compile tiers of document 14, lifted from rudb's query compiler.
//!
//! [`ir`] is QIR, the typed SSA form with block parameters, an explicit null model and a text form that round-trips (document 21 section 21.6), lifted from `rudb-qc-ir`. [`interp`] is the first tier, which runs a QIR function with no machine code, lifted from `rudb-qc-interp`. [`rt`] is the runtime that compiled pipelines call: the morsel ABI, `str16` strings, `LIKE` patterns, the aggregation hash tables and the executable memory of the native tiers, lifted from `rudb-qc-rt`. [`types`] gives the QIR type of each PostgreSQL type that fits a machine value. [`direct`] is the single pass x86-64 tier, which compiles a QIR pipeline function to machine code with no dependencies, lifted from `rudb-qc-direct`. [`clif`] is the Cranelift tier, which lowers a QIR function to machine code through `cranelift-codegen`, lifted from `rudb-qc-clif` and present with the feature `jit`. [`plan`] is the relational tree on PostgreSQL types that the generator compiles, lowered from the expressions and aggregates of a query of one table, lifted from `rudb-qc-plan`. [`tier`] puts a module on each tier it has and moves a function up when it runs long enough, lifted from the tier driver of `rudb-qc`.
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M6. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "jit")]
pub mod clif;
pub mod direct;
pub mod interp;
pub mod ir;
pub mod plan;
pub mod rt;
pub mod tier;
pub mod types;

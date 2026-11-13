//! The compile tiers of document 14, lifted from rudb's query compiler.
//!
//! [`ir`] is QIR, the typed SSA form with block parameters, an explicit null model and a text form that round-trips (document 21 section 21.6), lifted from `rudb-qc-ir`. [`interp`] is the first tier, which runs a QIR function with no machine code, lifted from `rudb-qc-interp`. [`types`] gives the QIR type of each PostgreSQL type that fits a machine value. The runtime, the direct tier and the Cranelift tier come in later lifts (document 22 section 22.7).
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M6. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod interp;
pub mod ir;
pub mod types;

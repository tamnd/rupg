// Lifted from rudb crates/rudb-qc-ir/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the module `ir` of rupg-jit, not a crate.

//! QIR, the query compiler's intermediate representation, as `spec/compiler/06-qir.md`
//! describes it.
//!
//! A module is a set of pipeline functions and the tables they refer to. A function is a list of
//! blocks, and a block is a flat arena of 32-bit words: a header, a result, operands. Values are
//! dense numbers with their types in a side column, constants live in a pool, and SSA uses block
//! parameters. The builder folds constants and shares pure instructions as it appends, which is
//! the only optimization QIR does before a backend sees it, apart from the linear passes of
//! section 6.11.
//!
//! The crate depends on nothing, so that every backend, the interpreter included, can use it
//! without pulling in the rest of the engine.

#![forbid(unsafe_code)]

mod build;
pub mod catalogue;
pub mod cfg;
pub mod entry;
pub mod eval;
pub mod func;
mod op;
mod parse;
pub mod print;
pub mod status;
mod ty;
mod verify;

pub use build::{Builder, dce};
pub use catalogue::{CATALOGUE, Proxy};
pub use func::{
    Block, BlockData, Const, ErrorKind, ErrorSite, Field, Func, GuardSite, Inst, Module, Site, Val,
    ValInfo,
};
pub use op::{Form, Op};
pub use parse::{ParseError, parse};
pub use print::print;
pub use ty::{Class, Ty};
pub use verify::{VerifyError, verify};

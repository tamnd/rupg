// Lifted from rudb crates/rudb-qc-direct/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the module `direct` of rupg-jit, not a crate, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`.

//! `direct`, the query compiler's single pass backend: it turns a QIR function into x86-64 machine
//! code with no dependencies, per section 8.5 of `spec/compiler/08-backends.md`.
//!
//! It is the tier that has to make the first morsel fast, so it does in two linear passes what
//! `clif` does in a pipeline of them. The analysis pass in [`analysis`] finds the loops, lays out
//! the blocks and computes liveness as intervals over that layout. The code generation pass then
//! selects instructions, allocates registers and encodes in one walk over the blocks, writing
//! through the encoder in [`asm`].
//!
//! Like `clif`, the output is bytes and relocations and nothing else. The code arena and the
//! runtime entries are in `rudb-qc-rt` above this crate, and compiling is a pure function of the
//! QIR, which is why the crate forbids `unsafe`.
//!
//! What the code computes is what `interp` computes, to the bit, since a query moves between the
//! tiers at morsel boundaries. The common opcodes are lowered inline. The rest go through the
//! runtime's `eval` entries, which run the interpreter's own code on the same bits, and anything
//! the generator cannot place at all is an [`Error`], so the function runs on `interp` instead.

#![forbid(unsafe_code)]

use std::fmt;

use crate::ir::Func;
use crate::ir::entry::Entry;

pub mod analysis;
pub mod asm;
mod codegen;

#[cfg(test)]
mod tests;

/// Why a function was not compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The function, or empty when the backend itself could not be made.
    pub func: String,
    /// What went wrong.
    pub reason: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.func.is_empty() {
            write!(f, "direct: {}", self.reason)
        } else {
            write!(f, "direct: {}: {}", self.func, self.reason)
        }
    }
}

impl std::error::Error for Error {}

fn fail(func: &str, reason: impl Into<String>) -> Error {
    Error { func: func.to_string(), reason: reason.into() }
}

/// A place in a [`Function`]'s bytes where the absolute address of an [`Entry`] plus `addend` goes,
/// as eight little endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// The byte offset in [`Function::bytes`].
    pub offset: u32,
    /// What the address is the address of.
    pub entry: Entry,
    /// Added to the address.
    pub addend: i64,
}

/// One compiled function: position independent bytes apart from its relocations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// The QIR function's name.
    pub name: String,
    /// The machine code.
    pub bytes: Vec<u8>,
    /// Where the runtime's addresses go.
    pub relocs: Vec<Reloc>,
}

/// The x86-64 code generator, with the instruction set extensions it may use. The base
/// architecture is enough for everything; an extension only turns a helper call or a longer
/// sequence into one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Backend {
    /// SSE4.2, for `crc32`.
    pub sse42: bool,
    /// `popcnt`.
    pub popcnt: bool,
    /// ABM, for `lzcnt`.
    pub lzcnt: bool,
    /// BMI1, for `tzcnt`.
    pub bmi1: bool,
}

impl Backend {
    /// The backend for this machine, with the extensions the processor reports, since the code
    /// runs where it is compiled.
    ///
    /// # Errors
    ///
    /// On any architecture but x86-64.
    pub fn host() -> Result<Backend, Error> {
        #[cfg(target_arch = "x86_64")]
        {
            Ok(Backend {
                sse42: std::is_x86_feature_detected!("sse4.2"),
                popcnt: std::is_x86_feature_detected!("popcnt"),
                lzcnt: std::is_x86_feature_detected!("lzcnt"),
                bmi1: std::is_x86_feature_detected!("bmi1"),
            })
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(fail("", "no x86-64 processor to run the code on"))
        }
    }

    /// The backend that uses no extension, which every x86-64 processor runs.
    #[must_use]
    pub fn baseline() -> Backend {
        Backend::default()
    }

    /// The target's name, for `EXPLAIN`.
    #[must_use]
    pub fn target(&self) -> String {
        let mut out = "x86_64".to_string();
        for (on, name) in [
            (self.sse42, "sse4.2"),
            (self.popcnt, "popcnt"),
            (self.lzcnt, "lzcnt"),
            (self.bmi1, "bmi1"),
        ] {
            if on {
                out.push('+');
                out.push_str(name);
            }
        }
        out
    }

    /// Compiles one function.
    ///
    /// # Errors
    ///
    /// When the function has irreducible control flow, a frame too large to address, or an
    /// operation on types this backend does not place.
    pub fn compile(&self, f: &Func) -> Result<Function, Error> {
        codegen::compile(self, f)
    }
}

// Lifted from rudb crates/rudb-qc-ir/src/entry.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The runtime entry points native code calls, named here so that a backend and the runtime
//! agree on them without depending on each other.
//!
//! A native backend knows what it calls only by these names. Its relocations name an [`Entry`],
//! and `rudb-qc-rt` owns the one table that turns an entry into an address, at load time. That
//! keeps a backend at the rank of this crate, below the runtime, and it keeps compiled code free
//! of addresses, so that compiling stays a pure function of the QIR (section 8.3 of
//! `spec/compiler/08-backends.md`).
//!
//! Every entry takes and returns plain integers and pointers in the C calling convention. The
//! first four reach the query's runtime through the context pointer the driver stores in the
//! state header's `rt` field before a call. The three `eval` entries are the fallback for an
//! operation a backend does not lower inline: they run the same [`crate::ir::eval`] code the
//! interpreter runs, on operands the caller has spilled to a buffer, so the answer cannot differ.

/// The byte offset from `%st` of the word native code passes as `ctx`: the state header's `rt`
/// field, which `rudb-qc-rt` lays out and checks against this.
pub const CTX_OFFSET: i32 = 8;

/// A runtime entry point.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Entry {
    /// `fn(ctx, proxy: u32, args: *const u128, n: u32, out: *mut u128) -> u64`, a runtime
    /// function from the catalogue. Returns 0 with the result in `*out`, or the status to return.
    Rtcall,
    /// `fn(ctx, kernel: u32, n: u64, buffers: *const u128, count: u32) -> u64`, a first engine
    /// kernel over `n` rows. Returns 0, or the status to return.
    Vcall,
    /// `fn(ctx, counter: u32, v: u64)`, adds to a counter.
    Count,
    /// `fn(ctx) -> u32`, 1 when the query has been cancelled.
    Cancelled,
    /// `fn(op: u32, ty: u32, to: u32, slot: *mut u128) -> u32`, [`crate::ir::eval::unary`] on
    /// `slot[0]`, with the result written back to `slot[0]`. Returns 1 when it traps.
    EvalUnary,
    /// `fn(op: u32, ty: u32, slot: *mut u128) -> u32`, [`crate::ir::eval::binary`] on `slot[0]` and
    /// `slot[1]`, with the result written to `slot[0]`. Returns 1 when it traps.
    EvalBinary,
    /// `fn(op: u32, ty: u32, k: u32, slot: *mut u128) -> u32`, [`crate::ir::eval::scale`] on
    /// `slot[0]`, with the result written back to `slot[0]`. Returns 1 when it traps.
    EvalScale,
    /// Not a function: eight tables of 256 `u32`, the slice by eight tables of CRC-32C, for code
    /// that computes [`crate::ir::eval::crc32c`] without the instruction.
    Crc32cTable,
}

impl Entry {
    /// Every entry, in a fixed order.
    pub const ALL: [Entry; 8] = [
        Entry::Rtcall,
        Entry::Vcall,
        Entry::Count,
        Entry::Cancelled,
        Entry::EvalUnary,
        Entry::EvalBinary,
        Entry::EvalScale,
        Entry::Crc32cTable,
    ];

    /// The entry's position in [`Entry::ALL`], which is what a backend puts in a relocation.
    #[must_use]
    pub fn index(self) -> u32 {
        self as u32
    }

    /// The entry at `index` in [`Entry::ALL`].
    #[must_use]
    pub fn from_index(index: u32) -> Option<Entry> {
        Entry::ALL.get(index as usize).copied()
    }

    /// A name for dumps and errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Entry::Rtcall => "rtcall",
            Entry::Vcall => "vcall",
            Entry::Count => "count",
            Entry::Cancelled => "cancelled",
            Entry::EvalUnary => "eval_unary",
            Entry::EvalBinary => "eval_binary",
            Entry::EvalScale => "eval_scale",
            Entry::Crc32cTable => "crc32c_table",
        }
    }
}

/// The slice by eight tables of CRC-32C: table 0 is the byte at a time table of the reflected
/// polynomial `0x82f63b78`, and table `k` advances table `k - 1` by one more zero byte.
#[must_use]
pub fn crc32c_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    for i in 0..256u32 {
        let mut crc = i;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
        t[0][i as usize] = crc;
    }
    for k in 1..8 {
        for i in 0..256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
        }
    }
    t
}

/// [`crate::ir::eval::crc32c`] computed through [`crc32c_tables`], which is what a backend without
/// the instruction emits. Here so that the two can be checked against each other.
#[must_use]
pub fn crc32c_sliced(t: &[[u32; 256]; 8], seed: u64, word: u64) -> u64 {
    let one = (word as u32) ^ (seed as u32);
    let two = (word >> 32) as u32;
    let crc = t[7][(one & 0xff) as usize]
        ^ t[6][((one >> 8) & 0xff) as usize]
        ^ t[5][((one >> 16) & 0xff) as usize]
        ^ t[4][(one >> 24) as usize]
        ^ t[3][(two & 0xff) as usize]
        ^ t[2][((two >> 8) & 0xff) as usize]
        ^ t[1][((two >> 16) & 0xff) as usize]
        ^ t[0][(two >> 24) as usize];
    u64::from(crc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_round_trips_through_its_index() {
        for (i, e) in Entry::ALL.iter().enumerate() {
            assert_eq!(e.index() as usize, i);
            assert_eq!(Entry::from_index(i as u32), Some(*e));
        }
    }

    #[test]
    fn the_sliced_crc_is_the_bitwise_one() {
        let t = crc32c_tables();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..1000 {
            x = x.rotate_left(13).wrapping_mul(0x2545_f491_4f6c_dd1d) ^ 0x1234;
            let seed = x.rotate_left(29);
            assert_eq!(crc32c_sliced(&t, seed, x), crate::ir::eval::crc32c(seed, x));
        }
    }
}

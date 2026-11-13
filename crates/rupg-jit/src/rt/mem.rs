// Lifted from rudb crates/rudb-qc-rt/src/mem.rs at cd9f9676 (2026-10-05).
// Changes: `write` expects to be unused until the runtime functions that write through it are lifted.

//! Reads through addresses compiled code hands the runtime.

#![allow(unsafe_code)]

/// The `n` bytes at `at`.
///
/// # Safety
///
/// `at` must be the address of `n` readable bytes that stay unchanged for `'a`.
pub(crate) unsafe fn slice<'a>(at: usize, n: usize) -> &'a [u8] {
    if n == 0 {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(at), n) }
}

/// Writes `bytes` at `at`.
///
/// # Safety
///
/// `at` must be the address of `bytes.len()` writable bytes nobody else is reading.
#[expect(
    dead_code,
    reason = "the runtime functions of rudb's `rt.rs` write through it, and they are not lifted yet"
)]
pub(crate) unsafe fn write(at: usize, bytes: &[u8]) {
    // SAFETY: the caller's contract.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::with_exposed_provenance_mut::<u8>(at),
            bytes.len(),
        );
    }
}

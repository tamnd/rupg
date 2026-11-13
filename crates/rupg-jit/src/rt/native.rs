// Lifted from rudb crates/rudb-qc-rt/src/native.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit under `crate::rt`. The interpreter's `Runtime` is `crate::interp::Runtime`, QIR is `crate::ir`, and `RUNTIME_ERROR` comes from `crate::rt`.

//! What native code calls: the `extern "C"` functions behind every [`Entry`], and the context
//! that carries the query's runtime to them.
//!
//! Native code has two arguments, the state and the morsel, and no way to name a Rust trait
//! object. So the driver makes a [`Ctx`] around the runtime for the length of a call and stores
//! its address in the state header's `rt` field ([`crate::rt::abi::RT`]), and generated code
//! passes that word as the first argument of every entry that needs the runtime.
//!
//! Every entry catches a panic at the boundary, because unwinding out of an `extern "C"` function
//! aborts the process, and turns it into the status the interpreter would have returned for a
//! runtime failure. None of them allocates on the caller's behalf except through the runtime.

#![allow(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use crate::interp::Runtime;
use crate::ir::entry::{Entry, crc32c_tables};
use crate::ir::eval::{self, Fault};
use crate::ir::{Op, Ty, status};

use crate::rt::RUNTIME_ERROR;

/// The runtime of one call, as native code reaches it.
pub struct Ctx<'a> {
    rt: &'a mut dyn Runtime,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").finish_non_exhaustive()
    }
}

impl<'a> Ctx<'a> {
    /// A context around `rt`.
    pub fn new(rt: &'a mut dyn Runtime) -> Ctx<'a> {
        Ctx { rt }
    }

    /// The word to store in the state header's `rt` field. It is valid while `self` is neither
    /// moved nor dropped.
    pub fn word(&mut self) -> u64 {
        std::ptr::from_mut(self).expose_provenance() as u64
    }
}

/// The address of an entry, for the loader to put into a relocation.
#[must_use]
pub fn address(entry: Entry) -> usize {
    match entry {
        Entry::Rtcall => rtcall as *const () as usize,
        Entry::Vcall => vcall as *const () as usize,
        Entry::Count => count as *const () as usize,
        Entry::Cancelled => cancelled as *const () as usize,
        Entry::EvalUnary => eval_unary as *const () as usize,
        Entry::EvalBinary => eval_binary as *const () as usize,
        Entry::EvalScale => eval_scale as *const () as usize,
        Entry::Crc32cTable => {
            static TABLES: OnceLock<[[u32; 256]; 8]> = OnceLock::new();
            TABLES.get_or_init(crc32c_tables).as_ptr().expose_provenance()
        }
    }
}

/// The status of a runtime function that panicked.
fn panicked() -> u64 {
    status::make(status::ERROR, RUNTIME_ERROR)
}

/// The context behind a word native code passed.
///
/// # Safety
///
/// `ctx` must be the word of a live [`Ctx`] that nothing else borrows for the length of the
/// returned borrow.
unsafe fn ctx<'a>(ctx: *mut Ctx<'static>) -> &'a mut Ctx<'static> {
    // SAFETY: the caller's contract.
    unsafe { &mut *ctx }
}

/// `n` `u128` words at `at`.
///
/// # Safety
///
/// `at` must point at `n` readable words, or `n` must be 0.
unsafe fn words<'a>(at: *const u128, n: u32) -> &'a [u128] {
    if n == 0 {
        return &[];
    }
    // SAFETY: the caller's contract. Native code aligns its buffers to 16.
    unsafe { std::slice::from_raw_parts(at, n as usize) }
}

unsafe extern "C" fn rtcall(
    c: *mut Ctx<'static>,
    proxy: u32,
    args: *const u128,
    n: u32,
    out: *mut u128,
) -> u64 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: native code passes the header's context word and a buffer of `n` arguments,
        // and `out` is a 16 byte slot of its frame.
        let (c, args, out) = unsafe { (ctx(c), words(args, n), &mut *out) };
        c.rt.rtcall_into(proxy, args, out)
    }))
    .unwrap_or_else(|_| panicked())
}

unsafe extern "C" fn vcall(
    c: *mut Ctx<'static>,
    kernel: u32,
    n: u64,
    buffers: *const u128,
    count: u32,
) -> u64 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as for `rtcall`.
        let (c, buffers) = unsafe { (ctx(c), words(buffers, count)) };
        match c.rt.vcall(kernel, n, buffers) {
            Ok(()) => 0,
            Err(s) => s,
        }
    }))
    .unwrap_or_else(|_| panicked())
}

unsafe extern "C" fn count(c: *mut Ctx<'static>, k: u32, v: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as for `rtcall`.
        unsafe { ctx(c) }.rt.count(k, v);
    }));
}

unsafe extern "C" fn cancelled(c: *mut Ctx<'static>) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as for `rtcall`.
        u32::from(unsafe { ctx(c) }.rt.cancelled())
    }))
    .unwrap_or(1)
}

/// Runs `f` on the slot and writes its value back, 1 when it traps or panics.
///
/// # Safety
///
/// `slot` must point at two writable, 16 byte aligned words.
unsafe fn on_slot(slot: *mut u128, f: impl FnOnce(u128, u128) -> Result<u128, Fault>) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller's contract.
        let (a, b) = unsafe { (slot.read(), slot.add(1).read()) };
        match f(a, b) {
            Ok(v) => {
                // SAFETY: the caller's contract.
                unsafe { slot.write(v) };
                0
            }
            Err(_) => 1,
        }
    }))
    .unwrap_or(1)
}

fn decode(op: u32, ty: u32) -> Result<(Op, Ty), Fault> {
    Ok((Op::from_bits(op).ok_or(Fault::NotPure)?, Ty::from_bits(ty).ok_or(Fault::NotPure)?))
}

unsafe extern "C" fn eval_unary(op: u32, ty: u32, to: u32, slot: *mut u128) -> u32 {
    // SAFETY: native code passes a two word slot of its frame.
    unsafe {
        on_slot(slot, |a, _| {
            let (op, ty) = decode(op, ty)?;
            eval::unary(op, ty, Ty::from_bits(to).ok_or(Fault::NotPure)?, a)
        })
    }
}

unsafe extern "C" fn eval_binary(op: u32, ty: u32, slot: *mut u128) -> u32 {
    // SAFETY: as for `eval_unary`.
    unsafe {
        on_slot(slot, |a, b| {
            let (op, ty) = decode(op, ty)?;
            eval::binary(op, ty, a, b)
        })
    }
}

unsafe extern "C" fn eval_scale(op: u32, ty: u32, k: u32, slot: *mut u128) -> u32 {
    // SAFETY: as for `eval_unary`.
    unsafe {
        on_slot(slot, |a, _| {
            let (op, ty) = decode(op, ty)?;
            eval::scale(op, ty, a, k)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(Vec<(u32, u64)>);

    impl Runtime for Echo {
        fn rtcall(&mut self, proxy: u32, args: &[u128]) -> Result<u128, u64> {
            if proxy == 9 { Err(77) } else { Ok(args.iter().sum::<u128>() + u128::from(proxy)) }
        }
        fn vcall(&mut self, _kernel: u32, n: u64, _buffers: &[u128]) -> Result<(), u64> {
            if n == 0 { Err(5) } else { Ok(()) }
        }
        fn count(&mut self, k: u32, v: u64) {
            self.0.push((k, v));
        }
        fn cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_entries_reach_the_runtime_through_the_context_word() {
        let mut echo = Echo(Vec::new());
        let mut c = Ctx::new(&mut echo);
        let word = c.word();
        let p: *mut Ctx<'static> = std::ptr::with_exposed_provenance_mut(word as usize);
        let args = [3u128, 4];
        let mut out = 0u128;
        // SAFETY: `p` is the word of `c`, which lives to the end of the test.
        unsafe {
            assert_eq!(rtcall(p, 1, args.as_ptr(), 2, &raw mut out), 0);
            assert_eq!(out, 8);
            assert_eq!(rtcall(p, 9, args.as_ptr(), 2, &raw mut out), 77);
            assert_eq!(vcall(p, 0, 0, args.as_ptr(), 2), 5);
            assert_eq!(vcall(p, 0, 3, args.as_ptr(), 2), 0);
            count(p, 2, 10);
            assert_eq!(cancelled(p), 1);
        }
        assert_eq!(echo.0, vec![(2, 10)]);
    }

    #[test]
    fn the_eval_entries_are_the_interpreter() {
        #[repr(align(16))]
        struct Slot([u128; 2]);
        let mut slot = Slot([7, 0]);
        // SAFETY: the slot is two aligned words.
        unsafe {
            let p = slot.0.as_mut_ptr();
            assert_eq!(eval_binary(Op::SdivT as u32, Ty::I64 as u32, p), 1);
            p.add(1).write(2);
            assert_eq!(eval_binary(Op::SdivT as u32, Ty::I64 as u32, p), 0);
            assert_eq!(p.read(), 3);
            assert_eq!(eval_scale(Op::DupT as u32, Ty::I128 as u32, 2, p), 0);
            assert_eq!(p.read(), 300);
            assert_eq!(eval_unary(Op::Neg as u32, Ty::I8 as u32, Ty::I8 as u32, p), 0);
            assert_eq!(p.read(), 0xd4);
        }
    }
}

// Lifted from rudb crates/rudb-qc-clif/src/tests.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so QIR is `crate::ir`, and the tests read the QIR texts in `crates/rupg-jit/tests/text`.

//! What can be checked without running the code: that every form lowers, that Cranelift takes the
//! result, and that the only relocations are the runtime entries. Running it against the
//! interpreter needs the code arena, which is above this crate, so those tests live in `rudb-qc`.

use super::*;
use crate::ir::{Module, parse, verify};

fn module(text: &str) -> Module {
    let m = parse(text).expect("the test module parses");
    verify(&m).expect("the test module verifies");
    m
}

fn compile(text: &str) -> Vec<Function> {
    let backend = Backend::host().expect("a backend for this machine");
    module(text)
        .funcs
        .iter()
        .map(|f| backend.compile(f).unwrap_or_else(|e| panic!("{e}")))
        .collect()
}

#[test]
fn every_form_lowers() {
    let text = include_str!("../../tests/text/forms.qir");
    let out = compile(text);
    assert_eq!(out.len(), 1);
    let f = &out[0];
    assert!(!f.bytes.is_empty());
    let entries: std::collections::BTreeSet<&str> =
        f.relocs.iter().map(|r| r.entry.name()).collect();
    // `sdiv.t`, `dup.t` and `ddown` by constants are inline, so the calls are the runtime's.
    assert!(entries.contains("rtcall"), "{entries:?}");
    assert!(entries.contains("vcall"), "{entries:?}");
    assert!(entries.contains("count"), "{entries:?}");
    for r in &f.relocs {
        assert!(r.offset as usize + 8 <= f.bytes.len());
    }
}

#[test]
fn a_loop_with_a_poll_lowers() {
    let text = "module sum
  error !E0 overflow \"integer overflow in sum\"

func @sum version=fused plan=#1
block b0(ptr %st, ptr %m):
  %n = load.i64 [%m + 0] inv
  %a = load.ptr [%m + 8] inv
  br b1(0, 0)
loop(1) b1(i64 %i, i64 %acc):
  poll 1024
  %done = icmp.uge i64 %i, %n
  brif %done, b3, b2
block b2:
  %y = load.i64 [%a + %i*8]
  %inext = add i64 %i, 1
  %s = sadd.t i64 %acc, %y, !E0
  br b1(%inext, %s)
block b3:
  store.i64 [%st + 0], %acc
  ret 0
";
    let out = compile(text);
    let names: Vec<&str> = out[0].relocs.iter().map(|r| r.entry.name()).collect();
    assert_eq!(names, ["cancelled"]);
}

#[test]
fn compiling_is_deterministic() {
    let text = include_str!("../../tests/text/forms.qir");
    assert_eq!(compile(text), compile(text));
}

#[test]
fn the_hard_operations_call_the_helpers() {
    let text = "module hard
  error !E0 overflow \"overflow\"

func @hard version=generic plan=#1
block b0(ptr %st, ptr %m):
  %a = load.i128 [%st + 0]
  %b = load.i128 [%st + 16]
  %p = smul.t i128 %a, %b, !E0
  %q = sdiv.t i128 %p, %b, !E0
  %c = crc32c i64 1, 2
  store.i128 [%st + 32], %q
  store.i64 [%st + 48], %c
  ret 0
";
    let out = compile(text);
    let names: std::collections::BTreeSet<&str> =
        out[0].relocs.iter().map(|r| r.entry.name()).collect();
    assert!(names.contains("eval_binary"), "{names:?}");
}

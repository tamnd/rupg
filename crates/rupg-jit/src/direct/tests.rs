// Lifted from rudb crates/rudb-qc-direct/src/tests.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`. The QIR texts are those of `tests/text` of rupg-jit.

//! What can be checked without running the code: that every form lowers, that the bytes decode as
//! x86-64, and that the only relocations are the runtime entries. Running it against the
//! interpreter needs the code arena, which is above this crate, so those tests live in `rudb-qc`.

use std::collections::BTreeSet;

use crate::ir::{Module, parse, verify};
use iced_x86::{Code, Decoder, DecoderOptions};

use super::*;

const FORMS: &str = include_str!("../../tests/text/forms.qir");

const ALL: Backend = Backend { sse42: true, popcnt: true, lzcnt: true, bmi1: true };

fn module(text: &str) -> Module {
    let m = parse(text).expect("the test module parses");
    verify(&m).expect("the test module verifies");
    m
}

fn compile_with(backend: Backend, text: &str) -> Vec<Function> {
    module(text)
        .funcs
        .iter()
        .map(|f| backend.compile(f).unwrap_or_else(|e| panic!("{e}")))
        .collect()
}

fn compile(text: &str) -> Vec<Function> {
    compile_with(ALL, text)
}

fn entries(f: &Function) -> BTreeSet<&'static str> {
    f.relocs.iter().map(|r| r.entry.name()).collect()
}

/// Decodes the code in front of the literal table, failing on any byte that is not an
/// instruction or on an instruction that runs into the table.
fn decode(f: &Function) -> usize {
    let end = f.relocs.iter().map(|r| r.offset as usize).min().unwrap_or(f.bytes.len());
    let code = &f.bytes[..end];
    let mut d = Decoder::with_ip(64, code, 0, DecoderOptions::NONE);
    let mut n = 0;
    while d.can_decode() {
        let at = d.position();
        let i = d.decode();
        assert_ne!(i.code(), Code::INVALID, "{}: bad instruction at {at:#x}", f.name);
        n += 1;
    }
    assert_eq!(d.position(), end, "{}: the last instruction runs into the table", f.name);
    n
}

#[test]
fn every_form_lowers() {
    let out = compile(FORMS);
    assert_eq!(out.len(), 1);
    let f = &out[0];
    assert!(!f.bytes.is_empty());
    let entries = entries(f);
    assert!(entries.contains("rtcall"), "{entries:?}");
    assert!(entries.contains("vcall"), "{entries:?}");
    assert!(entries.contains("count"), "{entries:?}");
    for r in &f.relocs {
        assert!(r.offset as usize + 8 <= f.bytes.len());
        assert_eq!(r.offset % 8, 0);
        assert_eq!(r.addend, 0);
    }
    assert!(decode(f) > 100);
}

#[test]
fn every_form_lowers_on_the_baseline() {
    let out = compile_with(Backend::baseline(), FORMS);
    decode(&out[0]);
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
    decode(&out[0]);
}

#[test]
fn compiling_is_deterministic() {
    assert_eq!(compile(FORMS), compile(FORMS));
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
    let names = entries(&out[0]);
    assert!(names.contains("eval_binary"), "{names:?}");
    decode(&out[0]);
}

#[test]
fn the_target_names_the_extensions() {
    assert_eq!(Backend::baseline().target(), "x86_64");
    assert_eq!(ALL.target(), "x86_64+sse4.2+popcnt+lzcnt+bmi1");
}

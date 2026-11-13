// Lifted from rudb crates/rudb-qc-interp/tests/run.rs at cd9f9676 (2026-10-05).
// Changes: the paths of the crates.

//! Runs small QIR functions on real buffers and checks their results and statuses.

use rupg_jit::interp::{Program, Runtime};
use rupg_jit::ir::{parse, status, verify};

#[derive(Default)]
struct Rt {
    cancel: bool,
    counts: Vec<(u32, u64)>,
    calls: Vec<(u32, Vec<u128>)>,
}

impl Runtime for Rt {
    fn rtcall(&mut self, proxy: u32, args: &[u128]) -> Result<u128, u64> {
        self.calls.push((proxy, args.to_vec()));
        Ok(args.iter().sum())
    }

    fn vcall(&mut self, _kernel: u32, _n: u64, _buffers: &[u128]) -> Result<(), u64> {
        Ok(())
    }

    fn count(&mut self, k: u32, v: u64) {
        self.counts.push((k, v));
    }

    fn cancelled(&self) -> bool {
        self.cancel
    }
}

/// Sums the values of an i64 column whose paired i32 column is above a threshold, with a checked
/// add that fails on overflow, into the state at offset 0.
const SUM: &str = "module sum
  error !E0 overflow \"integer overflow in sum\"
  error !E1 cancel \"query cancelled\"
  counter #0 \"rows kept\"

func @sum version=fused plan=#1
block b0(ptr %st, ptr %m):
  %n = load.i64 [%m + 0] inv
  %a = load.ptr [%m + 8] inv
  %b = load.ptr [%m + 16] inv
  %acc0 = load.i64 [%st + 0]
  br b1(0, %acc0, 0)
loop(1) b1(i64 %i, i64 %acc, i64 %kept):
  poll 1024
  %done = icmp.uge i64 %i, %n
  brif %done, b4, b2
block b2:
  %x = load.i32 [%b + %i*4]
  %y = load.i64 [%a + %i*8]
  %inext = add i64 %i, 1
  %keep = icmp.sgt i32 %x, 10
  brif %keep, b3, b1(%inext, %acc, %kept)
block b3:
  %s = sadd.t i64 %acc, %y, !E0
  %k1 = add i64 %kept, 1
  br b1(%inext, %s, %k1)
block b4:
  store.i64 [%st + 0], %acc
  ctr.add #0, %kept
  ret 2
";

fn program(text: &str) -> Program {
    let m = parse(text).unwrap_or_else(|e| panic!("{e:?}"));
    if let Err(e) = verify(&m) {
        panic!("{e:?}");
    }
    Program::new(&m)
}

fn morsel(a: &[i64], b: &[i32]) -> [u64; 3] {
    [a.len() as u64, a.as_ptr() as u64, b.as_ptr() as u64]
}

#[test]
fn sums_a_filtered_column() {
    let p = program(SUM);
    let f = p.func("sum").unwrap();
    let a: Vec<i64> = (0..5000).collect();
    let b: Vec<i32> = (0..5000i32).map(|i| i % 20).collect();
    let mut st = [7i64];
    let m = morsel(&a, &b);
    let mut rt = Rt::default();
    let s = p.call(f, st.as_mut_ptr().cast(), m.as_ptr().cast(), &mut rt);
    assert_eq!(status::kind(s), status::DONE);
    let want: i64 = 7 + a.iter().zip(&b).filter(|(_, x)| **x > 10).map(|(y, _)| *y).sum::<i64>();
    assert_eq!(st[0], want);
    assert_eq!(rt.counts, vec![(0, 2250)]);
}

#[test]
fn overflow_returns_the_error_site() {
    let p = program(SUM);
    let a = [i64::MAX, 1];
    let b = [20, 20];
    let mut st = [0i64];
    let m = morsel(&a, &b);
    let s = p.call(0, st.as_mut_ptr().cast(), m.as_ptr().cast(), &mut Rt::default());
    assert_eq!(status::kind(s), status::ERROR);
    assert_eq!(status::payload(s), 0);
    assert_eq!(st[0], 0, "a failed call leaves the state alone here");
}

#[test]
fn cancel_is_seen_at_the_poll() {
    let p = program(SUM);
    let a = vec![1i64; 3000];
    let b = vec![20i32; 3000];
    let mut st = [0i64];
    let m = morsel(&a, &b);
    let mut rt = Rt { cancel: true, ..Rt::default() };
    let s = p.call(0, st.as_mut_ptr().cast(), m.as_ptr().cast(), &mut rt);
    assert_eq!(status::kind(s), status::CANCELLED);
}

#[test]
fn block_arguments_move_in_parallel() {
    // b1 swaps its two parameters on every trip, so a sequential move would lose one of them.
    let p = program(
        "module swap

func @swap version=fused plan=#1
block b0(ptr %st, ptr %m):
  br b1(0, 1, 2)
loop(1) bounded b1(i64 %i, i64 %x, i64 %y):
  %done = icmp.eq i64 %i, 3
  %inext = add i64 %i, 1
  brif %done, b2, b1(%inext, %y, %x)
block b2:
  store.i64 [%st + 0], %x
  store.i64 [%st + 8], %y
  ret 2
",
    );
    let mut st = [0i64; 2];
    let s = p.call(0, st.as_mut_ptr().cast(), std::ptr::null(), &mut Rt::default());
    assert_eq!(status::kind(s), status::DONE);
    assert_eq!(st, [2, 1]);
}

#[test]
fn runtime_calls_and_guards() {
    let p = program(
        "module calls
  guard !G0 \"x is small\" fallback=calls.staged

func @calls version=fused plan=#1
block b0(ptr %st, ptr %m):
  %x = load.i64 [%st + 0]
  %small = icmp.ult i64 %x, 100
  guard %small, !G0
  %h = rtcall @date_trunc_minute(%x)
  store.i64 [%st + 8], %h
  ret 2
",
    );
    let mut st = [5i64, 0];
    let mut rt = Rt::default();
    assert_eq!(
        status::kind(p.call(0, st.as_mut_ptr().cast(), std::ptr::null(), &mut rt)),
        status::DONE
    );
    assert_eq!(st[1], 5);
    st[0] = 500;
    let s = p.call(0, st.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
    assert_eq!(status::kind(s), status::DEOPT);
    assert_eq!(status::payload(s), 0);
}

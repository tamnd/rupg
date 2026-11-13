// Lifted from rudb crates/rudb-qc-direct/src/analysis/tests.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`. The QIR texts are those of `tests/text` of rupg-jit.

//! The analysis against what it stands in for: the loops against the ones the builder declared,
//! and the intervals against liveness computed the slow way, by iterating to a fixpoint.

use crate::ir::{Block, Func, Module, Val, parse, verify};

use super::{Analysis, Irreducible, NONE, PINS};

fn module(text: &str) -> Module {
    let m = parse(text).expect("the test module parses");
    verify(&m).expect("the test module verifies");
    m
}

/// A loop inside a loop, a join with parameters and a cold block that rejoins the hot path.
const NESTED: &str = "module nested
  error !E0 overflow \"overflow\"

func @nested version=generic plan=#1
block b0(ptr %st, ptr %m):
  %n = load.i64 [%m + 0] inv
  %a = load.ptr [%m + 8] inv
  %k = load.i64 [%st + 0]
  br b1(0, 0)
loop(1) b1(i64 %i, i64 %acc):
  poll 1024
  %done = icmp.uge i64 %i, %n
  brif %done, b6, b2
block b2:
  %y = load.i64 [%a + %i*8]
  %neg = icmp.slt i64 %y, 0
  brif %neg, b7, b3(0, %y)
loop(2) b3(i64 %j, i64 %t):
  poll 1024
  %more = icmp.ult i64 %j, %k
  brif %more, b4, b5
block b4:
  %t2 = add i64 %t, %j
  %j2 = add i64 %j, 1
  br b3(%j2, %t2)
block b5:
  %inext = add i64 %i, 1
  %s = sadd.t i64 %acc, %t, !E0
  br b1(%inext, %s)
cold block b7:
  %z = sub i64 0, %y
  br b3(0, %z)
block b6:
  store.i64 [%st + 8], %acc
  ret 0
";

const FORMS: &str = include_str!("../../../tests/text/forms.qir");
const Q1A: &str = include_str!("../../../tests/text/q1a.qir");

fn succs(f: &Func, b: Block) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(t) = f.terminator(b) {
        t.succs(|s, _| out.push(s));
    }
    out
}

/// Live in and live out of every block, by the textbook fixpoint. A block's parameters are
/// defined at its top, and the arguments of a branch are read at the bottom of the block it ends.
fn exact(f: &Func) -> (Vec<Vec<bool>>, Vec<Vec<bool>>) {
    let (n, nv) = (f.blocks.len(), f.vals.len());
    let mut gen_ = vec![vec![false; nv]; n];
    let mut kill = vec![vec![false; nv]; n];
    for b in 0..n {
        for p in &f.blocks[b].params {
            kill[b][p.index()] = true;
        }
        for i in f.insts(Block(b as u32)).filter(|i| !i.dead()) {
            i.uses(|v| {
                if v != Val::NONE && !v.is_const() && !kill[b][v.index()] {
                    gen_[b][v.index()] = true;
                }
            });
            if let Some(r) = i.result {
                kill[b][r.index()] = true;
            }
        }
    }
    let mut live_in = vec![vec![false; nv]; n];
    let mut live_out = vec![vec![false; nv]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            for s in succs(f, Block(b as u32)) {
                for v in 0..nv {
                    if live_in[s.index()][v] && !live_out[b][v] {
                        live_out[b][v] = true;
                        changed = true;
                    }
                }
            }
            for v in 0..nv {
                let x = gen_[b][v] || (live_out[b][v] && !kill[b][v]);
                if x && !live_in[b][v] {
                    live_in[b][v] = true;
                    changed = true;
                }
            }
        }
    }
    (live_in, live_out)
}

fn check(f: &Func) -> Analysis {
    let a = Analysis::new(f).expect("the function is reducible");
    // Every edge that is not a back edge goes forward, and a back edge goes to a loop header.
    for &b in &a.order {
        for s in succs(f, b) {
            if a.pos[s.index()] <= a.pos[b.index()] {
                assert_eq!(
                    a.head[s.index()],
                    s.0,
                    "{}: b{} to b{} goes back to a non header",
                    f.name,
                    b.0,
                    s.0
                );
            }
        }
    }
    // A loop is one contiguous range of the order, starting at its header.
    for &h in &a.order {
        if a.head[h.index()] != h.0 {
            continue;
        }
        let (lo, hi) = (a.pos[h.index()], a.loop_end[h.index()]);
        for &b in &a.order {
            let mut inside = false;
            let mut x = a.head[b.index()];
            while x != NONE {
                inside |= x == h.0;
                x = a.parent[x as usize];
            }
            let p = a.pos[b.index()];
            assert_eq!(inside, lo <= p && p <= hi, "{}: b{} and the loop of b{}", f.name, b.0, h.0);
        }
    }
    // The builder's loop flags and depths agree with the ones found.
    for &b in &a.order {
        let d = &f.blocks[b.index()];
        assert_eq!(d.is_loop, a.head[b.index()] == b.0, "{}: b{}", f.name, b.0);
        if d.is_loop {
            assert_eq!(d.depth, a.depth[b.index()], "{}: b{}", f.name, b.0);
        }
    }
    // The layout is the order with the cold blocks last.
    assert_eq!(a.layout.len(), a.order.len());
    assert_eq!(a.layout[0], Block(0));
    let first_cold =
        a.layout.iter().position(|b| f.blocks[b.index()].cold).unwrap_or(a.layout.len());
    assert!(a.layout[first_cold..].iter().all(|b| f.blocks[b.index()].cold));
    // The intervals cover the exact liveness.
    let (live_in, live_out) = exact(f);
    for &b in &a.order {
        let p = a.pos[b.index()];
        for v in 0..f.vals.len() {
            if live_in[b.index()][v] {
                assert!(a.start[v] <= p && p <= a.end[v], "{}: v{v} is live into b{}", f.name, b.0);
            }
            if live_out[b.index()][v] {
                assert!(a.live_out(Val(v as u32), p), "{}: v{v} is live out of b{}", f.name, b.0);
            }
        }
    }
    // Where a value dies, `last` counts its reads in that block.
    for v in 0..f.vals.len() {
        if a.start[v] == NONE || a.full[v] {
            continue;
        }
        let b = a.order[a.end[v] as usize];
        let mut reads = 0;
        for i in f.insts(b).filter(|i| !i.dead()) {
            i.uses(|u| reads += u32::from(u == Val(v as u32)));
        }
        assert_eq!(reads, a.last[v], "{}: v{v} in b{}", f.name, b.0);
    }
    // Two values that share a pinned register never overlap.
    for x in 0..f.vals.len() {
        for y in x + 1..f.vals.len() {
            if a.pin[x] != 0 && a.pin[x] == a.pin[y] {
                assert!(
                    a.end[x] < a.start[y] || a.end[y] < a.start[x],
                    "{}: v{x} and v{y}",
                    f.name
                );
            }
        }
        assert!(a.pin[x] as usize <= PINS);
    }
    a
}

#[test]
fn the_test_modules_analyse() {
    for text in [FORMS, Q1A, NESTED] {
        for f in &module(text).funcs {
            check(f);
        }
    }
}

#[test]
fn nested_loops_nest_and_the_cold_block_goes_last() {
    let m = module(NESTED);
    let f = &m.funcs[0];
    let a = check(f);
    assert_eq!(a.depth[1], 1);
    assert_eq!(a.depth[3], 2);
    assert_eq!(a.depth[4], 2);
    assert_eq!(a.depth[5], 1);
    assert_eq!(a.depth[6], 0);
    assert_eq!(a.parent[3], 1);
    assert_eq!(a.head[7], 1);
    assert_eq!(*a.layout.last().unwrap(), Block(7));
    // The inner loop reads %k, defined before both loops, so it lives to the end of the outer.
    let k = f.vals.iter().position(|v| v.name.as_deref() == Some("k")).unwrap();
    assert!(a.full[k]);
    assert_eq!(a.end[k], a.loop_end[1]);
}

#[test]
fn the_inner_loop_keeps_its_values_in_registers() {
    let m = module(NESTED);
    let f = &m.funcs[0];
    let a = check(f);
    let named = |n: &str| f.vals.iter().position(|v| v.name.as_deref() == Some(n)).unwrap();
    assert_ne!(a.pin[named("j")], 0);
    assert_ne!(a.pin[named("t")], 0);
    // Three registers, and %k is read in the inner loop too, so the outer counter goes without.
    assert_ne!(a.pin[named("k")], 0);
    assert_eq!(a.pin[named("i")], 0);
}

#[test]
fn an_irreducible_graph_is_refused() {
    // Two blocks that branch to each other, each reachable from the entry: neither dominates the
    // other, so neither edge between them is a back edge. The verifier rejects this, so only
    // the parser sees it.
    let text = "module bad

func @bad version=generic plan=#1
block b0(ptr %st, ptr %m):
  %c = load.i1 [%m + 0]
  brif %c, b1, b2
block b1:
  %d = load.i1 [%m + 1]
  brif %d, b2, b3
block b2:
  %e = load.i1 [%m + 2]
  brif %e, b1, b3
block b3:
  ret 0
";
    let m = parse(text).expect("the module parses");
    assert!(matches!(Analysis::new(&m.funcs[0]), Err(Irreducible { .. })));
}

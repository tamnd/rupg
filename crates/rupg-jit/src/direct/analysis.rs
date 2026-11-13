// Lifted from rudb crates/rudb-qc-direct/src/analysis.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`.

//! The analysis pass of section 8.5 of `spec/compiler/08-backends.md`: loops, the block order,
//! liveness and the register hints, in one walk over the function and with no hash maps.
//!
//! The order is reverse postorder with the body of every loop kept together, which is what makes
//! a live range an interval. Every edge that is not a back edge goes forward, so a value is live
//! only between the block that defines it and the last block that uses it, except that a value
//! used inside a loop it is not defined in stays live to the end of that loop. That is Kohn's
//! liveness as TPDE does it, and it costs one pass over the uses instead of a dataflow fixpoint.
//! An interval is a block range and never smaller, which is conservative and is all the code
//! generator asks: whether a value is still wanted after the block it is in, and how many uses
//! it has left in the block where it dies.
//!
//! The hints are the values worth keeping in a callee saved register for their whole life, which
//! are the ones a loop reads. They are picked greedily by how deep in loops their uses are, and
//! two values share a register only when their intervals do not meet.

use crate::ir::cfg::Cfg;
use crate::ir::{Block, Func, Ty, Val};

/// No block, no position, no loop.
pub const NONE: u32 = u32::MAX;

/// How many registers the hints hand out: `rbx`, `r12` and `r13`, the callee saved registers
/// left once `rbp` is the frame pointer and `r14` and `r15` hold the morsel and the state.
pub const PINS: usize = 3;

/// How many candidates the hints look at, so that a function with thousands of values still
/// spends a bounded time here.
const CANDIDATES: usize = 64;

/// What the code generator needs to know about a function before it emits anything.
#[derive(Clone, Debug)]
pub struct Analysis {
    /// The reachable blocks in the order liveness is computed in: reverse postorder with loop
    /// bodies contiguous.
    pub order: Vec<Block>,
    /// The order the code is laid out in: `order` with the cold blocks moved to the end.
    pub layout: Vec<Block>,
    /// Each block's position in `order`, or [`NONE`] when it is unreachable.
    pub pos: Vec<u32>,
    /// Each block's loop depth, 0 outside every loop.
    pub depth: Vec<u8>,
    /// The header of the innermost loop each block is in, itself for a header, or [`NONE`].
    pub head: Vec<u32>,
    /// For a loop header, the header of the loop around it, or [`NONE`].
    pub parent: Vec<u32>,
    /// For a loop header, the position of the last block of its loop, and [`NONE`] otherwise.
    pub loop_end: Vec<u32>,
    /// How many edges come into each block.
    pub preds: Vec<u32>,
    /// The position of the first block each value is live in, or [`NONE`] for a value that is
    /// never defined in a reachable block.
    pub start: Vec<u32>,
    /// The position of the last block each value is live in.
    pub end: Vec<u32>,
    /// Whether the value is live to the end of its last block, which is so when a loop carries it
    /// or a branch there writes it as a block parameter.
    pub full: Vec<bool>,
    /// How many times each value is read in the whole function.
    pub uses: Vec<u32>,
    /// How many times each value is read in its last block, which is where it dies when it is not
    /// [`Analysis::full`].
    pub last: Vec<u32>,
    /// The register hint of each value: 0 for none, `1..=PINS` for the callee saved register it
    /// keeps for its whole life.
    pub pin: Vec<u8>,
}

/// Why a function was not analysed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Irreducible {
    /// The block a retreating edge goes to without dominating where it comes from.
    pub block: Block,
}

impl Analysis {
    /// Analyses a function.
    ///
    /// # Errors
    ///
    /// When the control flow is irreducible. The generator never makes such a function, the
    /// verifier's loop rule does not rule one out, and the intervals would be wrong for it, so
    /// the backend refuses it and the function runs on `interp`.
    pub fn new(f: &Func) -> Result<Analysis, Irreducible> {
        let cfg = Cfg::new(f);
        let n = f.blocks.len();
        let mut a = Analysis {
            order: Vec::with_capacity(cfg.rpo.len()),
            layout: Vec::with_capacity(cfg.rpo.len()),
            pos: vec![NONE; n],
            depth: vec![0; n],
            head: vec![NONE; n],
            parent: vec![NONE; n],
            loop_end: vec![NONE; n],
            preds: cfg.preds.iter().map(|p| p.len() as u32).collect(),
            start: vec![NONE; f.vals.len()],
            end: vec![0; f.vals.len()],
            full: vec![false; f.vals.len()],
            uses: vec![0; f.vals.len()],
            last: vec![0; f.vals.len()],
            pin: vec![0; f.vals.len()],
        };
        a.loops(&cfg)?;
        a.lay_out(f, &cfg);
        let weight = a.liveness(f);
        a.hints(f, &weight);
        Ok(a)
    }

    /// Finds the loops: a header per back edge, the innermost header of every block and the
    /// nesting. Headers are taken in reverse of reverse postorder, so an inner loop is found
    /// before the loop around it and is folded into it as one node.
    fn loops(&mut self, cfg: &Cfg) -> Result<(), Irreducible> {
        let mut work: Vec<u32> = Vec::new();
        for &h in cfg.rpo.iter().rev() {
            let hi = h.index();
            for &p in &cfg.preds[hi] {
                if cfg.reachable(p) && cfg.order[p.index()] >= cfg.order[hi] {
                    if !cfg.dominates(h, p) {
                        return Err(Irreducible { block: h });
                    }
                    work.push(p.0);
                }
            }
            if work.is_empty() {
                continue;
            }
            self.head[hi] = h.0;
            while let Some(x) = work.pop() {
                let r = self.root(x);
                if r == h.0 {
                    continue;
                }
                let next = if self.head[x as usize] == NONE {
                    self.head[x as usize] = h.0;
                    x
                } else {
                    // `x` is in a loop found earlier, whose outermost header is `r`: that loop
                    // is inside this one.
                    self.parent[r as usize] = h.0;
                    r
                };
                for &p in &cfg.preds[next as usize] {
                    if cfg.reachable(p) {
                        work.push(p.0);
                    }
                }
            }
        }
        for &b in &cfg.rpo {
            let bi = b.index();
            if self.head[bi] == b.0 {
                let p = self.parent[bi];
                self.depth[bi] =
                    if p == NONE { 1 } else { self.depth[p as usize].saturating_add(1) };
            }
        }
        for &b in &cfg.rpo {
            let h = self.head[b.index()];
            if h != NONE {
                self.depth[b.index()] = self.depth[h as usize];
            }
        }
        Ok(())
    }

    /// The outermost loop header found so far that contains `x`, or `x` when it is in none.
    fn root(&self, x: u32) -> u32 {
        let mut y = self.head[x as usize];
        if y == NONE {
            return x;
        }
        while self.parent[y as usize] != NONE {
            y = self.parent[y as usize];
        }
        y
    }

    /// Orders the blocks: each is keyed by the reverse postorder positions of the headers around
    /// it, outermost first, and then its own, so that a loop sorts as one item among its
    /// neighbours and its blocks stay together.
    fn lay_out(&mut self, f: &Func, cfg: &Cfg) {
        let stride = 1 + cfg.rpo.iter().map(|b| self.depth[b.index()] as usize).max().unwrap_or(0);
        let mut keys = vec![0u32; cfg.rpo.len() * stride];
        let mut chain: Vec<u32> = Vec::with_capacity(stride);
        for (i, &b) in cfg.rpo.iter().enumerate() {
            chain.clear();
            let mut h = self.head[b.index()];
            while h != NONE {
                chain.push(cfg.order[h as usize]);
                h = self.parent[h as usize];
            }
            let key = &mut keys[i * stride..(i + 1) * stride];
            for (k, c) in chain.iter().rev().enumerate() {
                key[k] = *c;
            }
            key[chain.len()] = cfg.order[b.index()];
        }
        let mut idx: Vec<u32> = (0..cfg.rpo.len() as u32).collect();
        idx.sort_by(|&x, &y| {
            let (x, y) = (x as usize, y as usize);
            keys[x * stride..(x + 1) * stride].cmp(&keys[y * stride..(y + 1) * stride])
        });
        self.order.extend(idx.iter().map(|&i| cfg.rpo[i as usize]));
        for (p, b) in self.order.iter().enumerate() {
            self.pos[b.index()] = p as u32;
        }
        for &b in &self.order {
            let mut h = self.head[b.index()];
            while h != NONE {
                let e = &mut self.loop_end[h as usize];
                *e = if *e == NONE { self.pos[b.index()] } else { (*e).max(self.pos[b.index()]) };
                h = self.parent[h as usize];
            }
        }
        let hot = self.order.iter().filter(|b| !f.blocks[b.index()].cold);
        let cold = self.order.iter().filter(|b| f.blocks[b.index()].cold);
        self.layout.extend(hot.chain(cold).copied());
    }

    /// The intervals, one walk over the blocks in order. Returns how much each value is used,
    /// weighted by loop depth, for the hints.
    fn liveness(&mut self, f: &Func) -> Vec<u32> {
        let mut weight = vec![0u32; f.vals.len()];
        // Parameters first, since a branch to a block later in the order writes them.
        for (p, &b) in self.order.iter().enumerate() {
            for v in &f.blocks[b.index()].params {
                self.start[v.index()] = p as u32;
                self.end[v.index()] = p as u32;
            }
        }
        for p in 0..self.order.len() {
            let b = self.order[p];
            let p = p as u32;
            let w = 1u32 << (3 * u32::from(self.depth[b.index()].min(6)));
            for i in f.insts(b).filter(|i| !i.dead()) {
                i.uses(|v| {
                    if v != Val::NONE && !v.is_const() {
                        self.read(v, b, p);
                        weight[v.index()] = weight[v.index()].saturating_add(w);
                    }
                });
                if let Some(r) = i.result {
                    self.start[r.index()] = p;
                    self.end[r.index()] = p;
                }
                i.succs(|s, _| {
                    for v in &f.blocks[s.index()].params {
                        self.written(*v, p);
                    }
                });
            }
        }
        weight
    }

    /// A read of `v` in block `b` at position `p`: live to here, and to the end of every loop
    /// around `b` that `v` was defined outside of.
    fn read(&mut self, v: Val, b: Block, p: u32) {
        let x = v.index();
        self.uses[x] += 1;
        let d = self.start[x];
        let (mut e, mut full) = (p, false);
        let mut h = self.head[b.index()];
        while h != NONE {
            let hp = self.pos[h as usize];
            if hp <= d && d <= self.loop_end[h as usize] {
                break;
            }
            e = self.loop_end[h as usize];
            full = true;
            h = self.parent[h as usize];
        }
        if e > self.end[x] {
            self.end[x] = e;
            self.full[x] = full;
            self.last[x] = u32::from(!full);
        } else if e == self.end[x] {
            if full {
                self.full[x] = true;
            } else {
                self.last[x] += 1;
            }
        }
    }

    /// A branch at position `p` writes the block parameter `v`: it is live across the whole of
    /// that block, whether the block comes before or after its own.
    fn written(&mut self, v: Val, p: u32) {
        let x = v.index();
        self.start[x] = self.start[x].min(p);
        if p > self.end[x] || (p == self.end[x] && !self.full[x]) {
            self.end[x] = p;
            self.full[x] = true;
        }
    }

    /// Hands the pinned registers to the values loops read most, where their intervals allow.
    fn hints(&mut self, f: &Func, weight: &[u32]) {
        let entry = &f.blocks[0].params;
        let mut cand: Vec<u32> = (0..f.vals.len() as u32)
            .filter(|&v| {
                let x = v as usize;
                self.start[x] != NONE
                    && weight[x] >= 8
                    && (self.end[x] > self.start[x] || self.full[x])
                    && !entry.contains(&Val(v))
                    && matches!(
                        f.vals[x].ty,
                        Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::Ptr
                    )
            })
            .collect();
        cand.sort_by(|&x, &y| weight[y as usize].cmp(&weight[x as usize]).then(x.cmp(&y)));
        cand.truncate(CANDIDATES);
        let mut taken: [Vec<(u32, u32)>; PINS] = Default::default();
        for v in cand {
            let (s, e) = (self.start[v as usize], self.end[v as usize]);
            for (r, t) in taken.iter_mut().enumerate() {
                if t.iter().all(|&(ts, te)| te < s || e < ts) {
                    t.push((s, e));
                    self.pin[v as usize] = r as u8 + 1;
                    break;
                }
            }
        }
    }

    /// Whether `v` is still wanted after the block at position `p`.
    #[must_use]
    pub fn live_out(&self, v: Val, p: u32) -> bool {
        let x = v.index();
        self.end[x] > p || (self.end[x] == p && self.full[x])
    }
}

#[cfg(test)]
mod tests;

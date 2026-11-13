// Lifted from rudb crates/rudb-qc-ir/src/cfg.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The control flow graph of a function: successors, predecessors, reverse postorder and
//! dominators.
//!
//! The builder never needs this. The verifier and the backends do, and they get it from one
//! walk here. Dominators use the iterative algorithm of Cooper, Harvey and Kennedy, which on a
//! reducible graph in reverse postorder settles in two passes.

use crate::ir::func::Inst;
use crate::ir::{Block, Func};

/// The graph of one function.
#[derive(Clone, Debug)]
pub struct Cfg {
    /// Successors of each block, in the terminator's order, with repeats.
    pub succs: Vec<Vec<Block>>,
    /// Predecessors of each block, with repeats.
    pub preds: Vec<Vec<Block>>,
    /// Blocks reachable from the entry, in reverse postorder.
    pub rpo: Vec<Block>,
    /// Each block's position in `rpo`, or `u32::MAX` when it is unreachable.
    pub order: Vec<u32>,
    /// The immediate dominator of each reachable block. The entry is its own.
    pub idom: Vec<Block>,
}

impl Cfg {
    /// Builds the graph.
    #[must_use]
    pub fn new(f: &Func) -> Cfg {
        Cfg::with(f, |b| f.terminator(b))
    }

    /// Builds the graph with the terminator of each block from `terminator`, for a caller that
    /// has already decoded the blocks.
    #[must_use]
    pub fn with<'a>(f: &Func, terminator: impl Fn(Block) -> Option<Inst<'a>>) -> Cfg {
        let n = f.blocks.len();
        let mut succs = vec![Vec::new(); n];
        let mut preds = vec![Vec::new(); n];
        for (b, out) in succs.iter_mut().enumerate() {
            if let Some(t) = terminator(Block(b as u32)) {
                t.succs(|s, _| {
                    if s.index() < n {
                        out.push(s);
                    }
                });
            }
        }
        for (b, out) in succs.iter().enumerate() {
            for s in out {
                preds[s.index()].push(Block(b as u32));
            }
        }
        // Postorder by an explicit stack, so a deep graph cannot overflow.
        let mut post = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        if n > 0 {
            let mut stack = vec![(Block(0), 0usize)];
            seen[0] = true;
            while let Some((b, next)) = stack.last_mut() {
                if let Some(&s) = succs[b.index()].get(*next) {
                    *next += 1;
                    if !seen[s.index()] {
                        seen[s.index()] = true;
                        stack.push((s, 0));
                    }
                } else {
                    post.push(*b);
                    stack.pop();
                }
            }
        }
        let rpo: Vec<Block> = post.into_iter().rev().collect();
        let mut order = vec![u32::MAX; n];
        for (i, b) in rpo.iter().enumerate() {
            order[b.index()] = i as u32;
        }
        let mut idom = vec![Block(u32::MAX); n];
        if n > 0 {
            idom[0] = Block(0);
        }
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new: Option<Block> = None;
                for &p in &preds[b.index()] {
                    if idom[p.index()].0 == u32::MAX {
                        continue;
                    }
                    new = Some(match new {
                        None => p,
                        Some(q) => intersect(&idom, &order, p, q),
                    });
                }
                if let Some(d) = new
                    && idom[b.index()] != d
                {
                    idom[b.index()] = d;
                    changed = true;
                }
            }
        }
        Cfg { succs, preds, rpo, order, idom }
    }

    /// Whether `b` is reachable from the entry.
    #[must_use]
    pub fn reachable(&self, b: Block) -> bool {
        self.order[b.index()] != u32::MAX
    }

    /// Whether `a` dominates `b`. Every block dominates itself.
    #[must_use]
    pub fn dominates(&self, a: Block, b: Block) -> bool {
        if !self.reachable(a) || !self.reachable(b) {
            return false;
        }
        let mut x = b;
        loop {
            if x == a {
                return true;
            }
            // Walking up the tree moves to earlier positions in reverse postorder.
            if self.order[x.index()] < self.order[a.index()] || x == Block(0) {
                return false;
            }
            x = self.idom[x.index()];
        }
    }

    /// Whether the edge from `src` to `dst` is a back edge: its target dominates its source.
    #[must_use]
    pub fn is_back_edge(&self, src: Block, dst: Block) -> bool {
        self.dominates(dst, src)
    }
}

fn intersect(idom: &[Block], order: &[u32], mut a: Block, mut b: Block) -> Block {
    while a != b {
        while order[a.index()] > order[b.index()] {
            a = idom[a.index()];
        }
        while order[b.index()] > order[a.index()] {
            b = idom[b.index()];
        }
    }
    a
}

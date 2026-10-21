//! `standard_join_search`: the order of the joins of a join domain, by dynamic programming over the sets of its items, as `join_search_one_level` does it. Each set of items keeps the plan with the lowest total cost.

use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::JoinKind;

use super::{Qual, Rel, Stats, choose, finish, join_rels, join_rows, lateral_rels};
use crate::{Level, exprs};

/// `geqo_threshold`: with this many items or more, PostgreSQL finds the order of the joins with the genetic optimizer. The planner then joins the items in their order.
const GEQO_THRESHOLD: usize = 12;

/// A set of items with its best plan, as the `RelOptInfo` of a join.
#[derive(Debug)]
struct Set<'q> {
    /// One bit for each item of the set.
    items: u64,
    rel: Rel<'q>,
    /// `lateral_relids`: the relations of the other items that the set reads with a lateral reference.
    lateral: BTreeSet<usize>,
}

/// The problem of the order of the joins of a join domain.
#[derive(Debug)]
struct Problem<'a, 'q> {
    level: &'a Level<'q>,
    /// The conditions of the inner joins of the domain.
    pool: &'a [Qual<'q>],
    /// The relations of each equivalence class.
    spans: &'a [BTreeSet<usize>],
    stats: &'a Stats,
    sets: Vec<Set<'q>>,
    /// The set of each group of items.
    index: BTreeMap<u64, usize>,
    /// `join_rel_level`: the sets of each number of items.
    levels: Vec<Vec<usize>>,
}

/// The plan of the joins of the items with the lowest total cost. `pool` has the conditions of the inner joins of the items, each with the relations that it needs.
pub(super) fn search<'q>(
    level: &Level<'q>,
    items: Vec<Rel<'q>>,
    pool: &[Qual<'q>],
    spans: &[BTreeSet<usize>],
    stats: &Stats,
) -> Rel<'q> {
    let n = items.len();
    if n >= GEQO_THRESHOLD {
        return in_order(level, items, pool, stats);
    }
    let domain: BTreeSet<usize> = items.iter().flat_map(|r| r.rels.iter().copied()).collect();
    let mut problem = Problem {
        level,
        pool,
        spans,
        stats,
        sets: Vec::new(),
        index: BTreeMap::new(),
        levels: vec![Vec::new(); n + 1],
    };
    for (i, rel) in items.iter().enumerate() {
        let mut lateral = BTreeSet::new();
        for &r in &rel.rels {
            lateral.extend(lateral_rels(level, r).intersection(&domain).copied());
        }
        lateral.retain(|r| !rel.rels.contains(r));
        problem.index.insert(1 << i, i);
        problem.levels[1].push(i);
        problem.sets.push(Set { items: 1 << i, rel: rel.clone(), lateral });
    }
    for k in 2..=n {
        problem.one_level(k);
        if problem.levels[k].is_empty() {
            return in_order(level, items, pool, stats);
        }
    }
    let best = problem.levels[n][0];
    problem.sets.swap_remove(best).rel
}

/// The joins of the items in their order, each with the conditions that its two sides have.
fn in_order<'q>(
    level: &Level<'q>,
    items: Vec<Rel<'q>>,
    pool: &[Qual<'q>],
    stats: &Stats,
) -> Rel<'q> {
    let mut items = items.into_iter();
    let Some(mut out) = items.next() else { unreachable!("a join domain has two items or more") };
    for next in items {
        let quals = between(pool, &out.rels, &next.rels);
        out = join_rels(level, JoinKind::Inner, out, next, quals, Vec::new(), stats);
    }
    out
}

/// The conditions that a join of two sides tests: the conditions that need the relations of both sides.
fn between<'q>(
    pool: &[Qual<'q>],
    left: &BTreeSet<usize>,
    right: &BTreeSet<usize>,
) -> Vec<Qual<'q>> {
    let mut quals: Vec<Qual<'q>> = pool
        .iter()
        .filter(|q| {
            q.rels.iter().all(|r| left.contains(r) || right.contains(r))
                && !q.rels.is_subset(left)
                && !q.rels.is_subset(right)
        })
        .cloned()
        .collect();
    quals.sort_by_key(|q| exprs::calls(&q.expr));
    quals
}

impl<'q> Problem<'_, 'q> {
    /// `join_search_one_level`: the sets of `k` items. A set joins each item that it has a condition with, or each item when it has no condition with another item. Then two sets of two items or more join when they have a condition. When no set has a condition with another, each set of `k - 1` items joins each item.
    fn one_level(&mut self, k: usize) {
        let previous = self.levels[k - 1].clone();
        let initial = self.levels[1].clone();
        for (x, &old) in previous.iter().enumerate() {
            if self.has_joins(old) {
                let first = if k == 2 { x + 1 } else { 0 };
                for &other in &initial[first..] {
                    if self.disjoint(old, other) && self.relevant(old, other) {
                        self.make(old, other);
                    }
                }
            } else {
                for &other in &initial {
                    if self.disjoint(old, other) {
                        self.make(old, other);
                    }
                }
            }
        }
        let mut j = 2;
        while j <= k - j {
            let olds = self.levels[j].clone();
            let others = self.levels[k - j].clone();
            for (x, &old) in olds.iter().enumerate() {
                if !self.has_joins(old) {
                    continue;
                }
                let first = if j == k - j { x + 1 } else { 0 };
                for &other in &others[first..] {
                    if self.disjoint(old, other) && self.relevant(old, other) {
                        self.make(old, other);
                    }
                }
            }
            j += 1;
        }
        if self.levels[k].is_empty() {
            for &old in &previous {
                for &other in &initial {
                    if self.disjoint(old, other) {
                        self.make(old, other);
                    }
                }
            }
        }
    }

    fn disjoint(&self, a: usize, b: usize) -> bool {
        self.sets[a].items & self.sets[b].items == 0
    }

    /// True when the set has a condition with another item, a class with another item or a lateral reference with another item, as `joininfo`, `has_eclass_joins` and `has_join_restriction`.
    fn has_joins(&self, s: usize) -> bool {
        let set = &self.sets[s];
        let rels = &set.rel.rels;
        let crosses = |r: &BTreeSet<usize>| !r.is_disjoint(rels) && !r.is_subset(rels);
        self.pool.iter().any(|q| crosses(&q.rels))
            || self.spans.iter().any(crosses)
            || !set.lateral.is_empty()
            || self.levels[1].iter().any(|&i| {
                self.sets[i].items & set.items == 0 && !self.sets[i].lateral.is_disjoint(rels)
            })
    }

    /// True when two sets have a condition, a class or a lateral reference, as `have_relevant_joinclause` and `have_join_order_restriction`.
    fn relevant(&self, a: usize, b: usize) -> bool {
        let (sa, sb) = (&self.sets[a], &self.sets[b]);
        let both =
            |r: &BTreeSet<usize>| !r.is_disjoint(&sa.rel.rels) && !r.is_disjoint(&sb.rel.rels);
        self.pool.iter().any(|q| both(&q.rels))
            || self.spans.iter().any(both)
            || !sa.lateral.is_disjoint(&sb.rel.rels)
            || !sb.lateral.is_disjoint(&sa.rel.rels)
    }

    /// `make_join_rel`: the join of two sets, when `join_is_legal` accepts it. The plan of the set of their items is the new plan when it costs less. A set that reads the other set with a lateral reference is the inner side, and a join that leaves a lateral reference to another item is not legal.
    fn make(&mut self, a: usize, b: usize) {
        let (sa, sb) = (&self.sets[a], &self.sets[b]);
        let items = sa.items | sb.items;
        let a_reads_b = !sa.lateral.is_disjoint(&sb.rel.rels);
        let b_reads_a = !sb.lateral.is_disjoint(&sa.rel.rels);
        let mut rels = sa.rel.rels.clone();
        rels.extend(sb.rel.rels.iter().copied());
        let lateral: BTreeSet<usize> =
            sa.lateral.union(&sb.lateral).filter(|r| !rels.contains(r)).copied().collect();
        if a_reads_b && b_reads_a || !lateral.is_empty() {
            return;
        }
        let (l, r) = if a_reads_b { (&sb.rel, &sa.rel) } else { (&sa.rel, &sb.rel) };
        let quals = between(self.pool, &l.rels, &r.rels);
        let Some(best) = choose(self.level, JoinKind::Inner, l, r, &quals, &[], self.stats) else {
            return;
        };
        let existing = self.index.get(&items).copied();
        let rows = match existing {
            Some(e) if self.sets[e].rel.node.cost.total <= best.total => return,
            Some(e) => self.sets[e].rel.node.cost.rows,
            None => join_rows(JoinKind::Inner, l, r, &quals, &[], self.stats),
        };
        let rel = finish(self.level, &best, l.clone(), r.clone(), &quals, &[], rows);
        match existing {
            Some(e) => self.sets[e].rel = rel,
            None => {
                let at = self.sets.len();
                self.index.insert(items, at);
                self.levels[items.count_ones() as usize].push(at);
                self.sets.push(Set { items, rel, lateral });
            }
        }
    }
}

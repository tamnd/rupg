//! `standard_join_search`: the order of the joins of a join domain, by dynamic programming over the sets of its items, as `join_search_one_level` does it. Each set of items keeps the plans that `add_path` keeps. When the sets of each number of items are made, each of them makes these plans, with the plan with the lowest total cost first, as `set_cheapest`. A join of two sets reads the cheapest plan of each side, and a merge join can also read another plan of a side that is in the order of the merge.

use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::JoinKind;

use super::classes::{Found, Joined};
use super::paths::Paths;
use super::{Candidate, Qual, Rel, Stats, choose, finish, join_rels, join_rows, lateral_rels};
use crate::{Level, exprs};

/// `geqo_threshold`: with this many items or more, PostgreSQL finds the order of the joins with the genetic optimizer. The planner then joins the items in their order.
const GEQO_THRESHOLD: usize = 12;

/// A set of items with its best plan, as the `RelOptInfo` of a join.
#[derive(Debug)]
struct Set<'q> {
    /// One bit for each item of the set.
    items: u64,
    rels: BTreeSet<usize>,
    /// The plans of the set, the plan with the lowest total cost first, which it gets when the sets of its number of items are all made.
    plans: Vec<Rel<'q>>,
    /// The plans of the joins of two sets that make the set, with the outer set and the inner set of each.
    paths: Paths<(usize, usize, Candidate<'q>)>,
    rows: f64,
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
    /// The classes whose conditions of joins the joins make again.
    classes: &'a [Joined<'q>],
    stats: &'a Stats,
    sets: Vec<Set<'q>>,
    /// The set of each group of items.
    index: BTreeMap<u64, usize>,
    /// `join_rel_level`: the sets of each number of items.
    levels: Vec<Vec<usize>>,
}

/// The plans of the joins of the items that `add_path` keeps, with the plan with the lowest total cost first. Each item has its plans in the same order. `pool` has the conditions of the inner joins of the items, each with the relations that it needs.
pub(super) fn search<'q>(
    level: &Level<'q>,
    items: Vec<Vec<Rel<'q>>>,
    pool: &[Qual<'q>],
    found: &Found<'q>,
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let n = items.len();
    let classes = &found.classes;
    if n >= GEQO_THRESHOLD {
        return in_order(level, items, pool, classes, stats);
    }
    let domain: BTreeSet<usize> = items.iter().flat_map(|r| r[0].rels.iter().copied()).collect();
    let mut problem = Problem {
        level,
        pool,
        spans: &found.spans,
        classes,
        stats,
        sets: Vec::new(),
        index: BTreeMap::new(),
        levels: vec![Vec::new(); n + 1],
    };
    for (i, plans) in items.iter().enumerate() {
        let rel = &plans[0];
        let mut lateral = BTreeSet::new();
        for &r in &rel.rels {
            lateral.extend(lateral_rels(level, r).intersection(&domain).copied());
        }
        lateral.retain(|r| !rel.rels.contains(r));
        problem.index.insert(1 << i, i);
        problem.levels[1].push(i);
        problem.sets.push(Set {
            items: 1 << i,
            rels: rel.rels.clone(),
            plans: plans.clone(),
            paths: Paths::new(stats.startup),
            rows: rel.node.cost.rows,
            lateral,
        });
    }
    for k in 2..=n {
        problem.one_level(k);
        if problem.levels[k].is_empty() {
            return in_order(level, items, pool, classes, stats);
        }
        problem.set_cheapest(k);
    }
    let best = problem.levels[n][0];
    problem.sets.swap_remove(best).plans
}

/// The joins of the items in their order, each with the conditions that its two sides have.
fn in_order<'q>(
    level: &Level<'q>,
    items: Vec<Vec<Rel<'q>>>,
    pool: &[Qual<'q>],
    classes: &[Joined<'q>],
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let mut items = items.into_iter();
    let Some(mut out) = items.next() else { unreachable!("a join domain has two items or more") };
    for next in items {
        let quals = between(pool, classes, &out[0].rels, &next[0].rels);
        out = join_rels(level, JoinKind::Inner, out, next, quals, Vec::new(), stats);
    }
    out
}

/// The conditions that a join of two sides tests: the conditions that need the relations of both sides, and then the condition of each class with a member on each side, as `build_joinrel_restrictlist`.
fn between<'q>(
    pool: &[Qual<'q>],
    classes: &[Joined<'q>],
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
    quals.extend(classes.iter().filter_map(|c| c.condition(left, right)));
    quals.sort_by_key(|q| exprs::calls(&q.expr));
    quals
}

impl<'q> Problem<'_, 'q> {
    /// `set_cheapest` for the sets of `k` items: each set makes the plans that it keeps, with the plan with the lowest total cost first.
    fn set_cheapest(&mut self, k: usize) {
        for s in self.levels[k].clone() {
            let paths = std::mem::replace(&mut self.sets[s].paths, Paths::new(self.stats.startup));
            let mut plans = Vec::new();
            for (outer, inner, best) in paths.kept() {
                let (l, r) = if best.swapped { (best.paths.1, best.paths.0) } else { best.paths };
                let (l, r) = (self.sets[outer].plans[l].clone(), self.sets[inner].plans[r].clone());
                let quals = between(self.pool, self.classes, &l.rels, &r.rels);
                let mut rel = finish(self.level, &best, l, r, &quals, &[], self.sets[s].rows);
                rel.node.cost.width = self.stats.width(&rel.rels);
                plans.push(rel);
            }
            assert!(!plans.is_empty(), "each join of two sets has a nested loop");
            self.sets[s].plans = plans;
        }
    }

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
        let rels = &set.rels;
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
        let both = |r: &BTreeSet<usize>| !r.is_disjoint(&sa.rels) && !r.is_disjoint(&sb.rels);
        self.pool.iter().any(|q| both(&q.rels))
            || self.spans.iter().any(both)
            || !sa.lateral.is_disjoint(&sb.rels)
            || !sb.lateral.is_disjoint(&sa.rels)
    }

    /// `make_join_rel`: the join of two sets, when `join_is_legal` accepts it. The set of their items gets the plans of the join that `add_path` keeps. A set that reads the other set with a lateral reference is the inner side, and a join that leaves a lateral reference to another item is not legal.
    fn make(&mut self, a: usize, b: usize) {
        let (sa, sb) = (&self.sets[a], &self.sets[b]);
        let items = sa.items | sb.items;
        let a_reads_b = !sa.lateral.is_disjoint(&sb.rels);
        let b_reads_a = !sb.lateral.is_disjoint(&sa.rels);
        let mut rels = sa.rels.clone();
        rels.extend(sb.rels.iter().copied());
        let lateral: BTreeSet<usize> =
            sa.lateral.union(&sb.lateral).filter(|r| !rels.contains(r)).copied().collect();
        if a_reads_b && b_reads_a || !lateral.is_empty() {
            return;
        }
        let (outer, inner) = if a_reads_b { (b, a) } else { (a, b) };
        let (l, r) = (&self.sets[outer].plans, &self.sets[inner].plans);
        let quals =
            between(self.pool, self.classes, &self.sets[outer].rels, &self.sets[inner].rels);
        let candidates = choose(self.level, JoinKind::Inner, l, r, &quals, &[], self.stats);
        if candidates.is_empty() {
            return;
        }
        let at = match self.index.get(&items) {
            Some(&e) => e,
            None => {
                let rows = join_rows(JoinKind::Inner, &l[0], &r[0], &quals, &[], self.stats);
                let at = self.sets.len();
                self.index.insert(items, at);
                self.levels[items.count_ones() as usize].push(at);
                let paths = Paths::new(self.stats.startup);
                self.sets.push(Set { items, rels, plans: Vec::new(), paths, rows, lateral });
                at
            }
        };
        for (candidate, keys) in candidates {
            self.sets[at].paths.add(
                candidate.startup,
                candidate.total,
                keys,
                (outer, inner, candidate),
            );
        }
    }
}

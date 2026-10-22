//! `standard_join_search`: the order of the joins of a join domain, by dynamic programming over the sets of its items, as `join_search_one_level` does it. Each set of items keeps the plans that `add_path` keeps. When the sets of each number of items are made, each of them makes these plans, with the plan with the lowest total cost first, as `set_cheapest`. A join of two sets reads the cheapest plan of each side, and a merge join can also read another plan of a side that is in the order of the merge.
//!
//! The inner side of each semi join and anti join of the domain is one item. `join_is_legal` gives the joins of two sets that the order of these joins lets the planner make: the semi join or the anti join itself, when one set has its outer relations and the other set is its inner side, or an inner join with the unique rows of the inner side of a semi join with any other set.

use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::JoinKind;

use super::classes::{Found, Joined};
use super::paths::Paths;
use super::{
    Candidate, Part, Qual, Rel, Special, Stats, choose, finish, join_rels, join_rows, lateral_rels,
    semi_rels, semi_selectivity, semifactors, unique_rels,
};
use crate::{Level, cost, exprs};

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
    /// The plans of the joins of two sets that make the set, with the outer set and the inner set of each and what the join does.
    paths: Paths<(usize, usize, Candidate<'q>, How)>,
    rows: f64,
    /// `lateral_relids`: the relations of the other items that the set reads with a lateral reference.
    lateral: BTreeSet<usize>,
}

/// What a join of two sets does.
#[derive(Clone, Copy, Debug)]
enum How {
    /// An inner join.
    Plain,
    /// The semi join or the anti join of a special join of the domain.
    Special(usize),
    /// An inner join with the unique rows of the inner side of a semi join, as `JOIN_UNIQUE_INNER` and `JOIN_UNIQUE_OUTER`.
    Unique(usize),
}

/// A special join that a join of two sets does, as `join_is_legal` finds it: the special join, true when the first set is its inner side, and true for an inner join with the unique rows of its inner side.
#[derive(Clone, Copy, Debug)]
struct Match {
    special: usize,
    reversed: bool,
    unique: bool,
}

/// The problem of the order of the joins of a join domain.
#[derive(Debug)]
struct Problem<'a, 'q> {
    level: &'a Level<'q>,
    /// The conditions of the inner joins and the semi joins of the domain.
    pool: &'a [Qual<'q>],
    /// The semi joins and the anti joins of the domain.
    specials: &'a [Special<'q>],
    /// For each special join, the plans of the unique rows of its inner side, or none.
    uniques: Vec<Vec<Rel<'q>>>,
    /// For each anti join with a condition, the relations of its two sides, which the condition needs, as the `required_relids` of a condition of an outer join.
    scopes: Vec<BTreeSet<usize>>,
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

/// The plans of the joins of the items that `add_path` keeps, with the plan with the lowest total cost first. Each item has its plans in the same order. `pool` has the conditions of the inner joins and the semi joins of the items, each with the relations that it needs, and `specials` has the semi joins and the anti joins.
///
/// `reduce_unique_semijoins`: a semi join whose inner side is one table that the conditions with the outer relations of the join make unique is an inner join, and its inner side joins in any order.
pub(super) fn search<'q>(
    level: &Level<'q>,
    items: Vec<Vec<Rel<'q>>>,
    pool: &[Qual<'q>],
    mut specials: Vec<Special<'q>>,
    found: &Found<'q>,
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let n = items.len();
    let classes = &found.classes;
    let item = |rels: &BTreeSet<usize>| items.iter().find(|i| i[0].rels == *rels);
    specials.retain(|sp| {
        let Some(inner) = item(&sp.right).filter(|_| sp.kind == JoinKind::Semi) else {
            return true;
        };
        let quals = between(pool, classes, &sp.left, &sp.right);
        semifactors(&sp.left, &inner[0], &quals, stats).is_none()
    });
    if n >= GEQO_THRESHOLD {
        return in_order(level, items, pool, &specials, classes, stats);
    }
    let uniques = specials
        .iter()
        .map(|sp| {
            let semi = sp.semi.as_ref().filter(|_| sp.kind == JoinKind::Semi);
            let unique =
                semi.zip(item(&sp.right)).and_then(|(semi, i)| unique_rels(semi, i, stats));
            unique.unwrap_or_default()
        })
        .collect();
    let scopes = specials
        .iter()
        .filter(|sp| !sp.quals.is_empty())
        .map(|sp| sp.left.union(&sp.right).copied().collect())
        .collect();
    let domain: BTreeSet<usize> = items.iter().flat_map(|r| r[0].rels.iter().copied()).collect();
    let mut problem = Problem {
        level,
        pool,
        specials: &specials,
        uniques,
        scopes,
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
    // With a special join, a number of items can have no set, as the inner side of a join must be whole.
    for k in 2..=n {
        problem.one_level(k);
        if problem.levels[k].is_empty() && specials.is_empty() {
            break;
        }
        problem.set_cheapest(k);
    }
    let Some(&best) = problem.levels[n].first() else {
        return in_order(level, items, pool, &specials, classes, stats);
    };
    problem.sets.swap_remove(best).plans
}

/// The joins of the items in their order, each with the conditions that its two sides have. An item that is the inner side of a special join joins with its semi join or its anti join.
fn in_order<'q>(
    level: &Level<'q>,
    items: Vec<Vec<Rel<'q>>>,
    pool: &[Qual<'q>],
    specials: &[Special<'q>],
    classes: &[Joined<'q>],
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let mut items = items.into_iter();
    let Some(mut out) = items.next() else { unreachable!("a join domain has two items or more") };
    for next in items {
        let mut quals = between(pool, classes, &out[0].rels, &next[0].rels);
        out = match specials.iter().find(|sp| sp.right == next[0].rels) {
            Some(sp) => {
                quals.extend(sp.quals.iter().cloned());
                let (semi, filter) = (sp.semi.as_ref(), sp.filter.clone());
                semi_rels(level, sp.kind, semi, out, next, quals, filter, stats)
            }
            None => join_rels(level, JoinKind::Inner, out, next, quals, Vec::new(), stats),
        };
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
            for (outer, inner, best, how) in paths.kept() {
                let (l, r) = if best.swapped { (best.paths.1, best.paths.0) } else { best.paths };
                let inners = match how {
                    How::Unique(i) => &self.uniques[i],
                    _ => &self.sets[inner].plans,
                };
                let (l, r) = (self.sets[outer].plans[l].clone(), inners[r].clone());
                let (quals, filter) = self.conditions(outer, inner, how);
                let mut rel = finish(self.level, &best, l, r, &quals, &filter, self.sets[s].rows);
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

    /// True when the set has a condition with another item, a class with another item, a lateral reference with another item or a special join that it has a part of, as `joininfo`, `has_eclass_joins` and `has_join_restriction`.
    fn has_joins(&self, s: usize) -> bool {
        let set = &self.sets[s];
        let rels = &set.rels;
        let crosses = |r: &BTreeSet<usize>| !r.is_disjoint(rels) && !r.is_subset(rels);
        self.pool.iter().any(|q| crosses(&q.rels))
            || self.scopes.iter().any(crosses)
            || self.spans.iter().any(crosses)
            || !set.lateral.is_empty()
            || self.levels[1].iter().any(|&i| {
                self.sets[i].items & set.items == 0 && !self.sets[i].lateral.is_disjoint(rels)
            })
            || self.specials.iter().any(|sp| {
                !(sp.left.is_subset(rels) && sp.right.is_subset(rels))
                    && (!sp.left.is_disjoint(rels) || !sp.right.is_disjoint(rels))
            })
    }

    /// `have_relevant_joinclause`: true when two sets have a condition or a class.
    fn clause(&self, a: usize, b: usize) -> bool {
        let (sa, sb) = (&self.sets[a], &self.sets[b]);
        let both = |r: &BTreeSet<usize>| !r.is_disjoint(&sa.rels) && !r.is_disjoint(&sb.rels);
        self.pool.iter().any(|q| both(&q.rels))
            || self.scopes.iter().any(both)
            || self.spans.iter().any(both)
    }

    /// True when two sets have a condition, a class or a lateral reference, or when a special join needs their join, as `have_relevant_joinclause` and `have_join_order_restriction`. A special join needs it when the two sets are its two sides, or when each of them has a part of the same side, and neither set has a legal join with an item by a condition, so that the planner makes the joins with no condition last.
    fn relevant(&self, a: usize, b: usize) -> bool {
        let (sa, sb) = (&self.sets[a], &self.sets[b]);
        if self.clause(a, b)
            || !sa.lateral.is_disjoint(&sb.rels)
            || !sb.lateral.is_disjoint(&sa.rels)
        {
            return true;
        }
        let (ra, rb) = (&sa.rels, &sb.rels);
        let needed = self.specials.iter().any(|sp| {
            sp.left.is_subset(ra) && sp.right.is_subset(rb)
                || sp.left.is_subset(rb) && sp.right.is_subset(ra)
                || !sp.right.is_disjoint(ra) && !sp.right.is_disjoint(rb)
                || !sp.left.is_disjoint(ra) && !sp.left.is_disjoint(rb)
        });
        needed && !self.legal_clause(a) && !self.legal_clause(b)
    }

    /// `has_legal_joinclause`: true when the set has a condition or a class with an item that it can join.
    fn legal_clause(&self, s: usize) -> bool {
        self.levels[1]
            .iter()
            .any(|&i| self.disjoint(s, i) && self.clause(s, i) && self.legal(s, i).is_some())
    }

    /// `join_is_legal`: `None` when the order of the special joins does not let the two sets join, or else the special join that the join does. A special join is not relevant when the join does not have a part of its inner side, when the join is a part of its inner side or when a set has it whole. Neither is a semi join whose inner side a set has with other relations, as that set has the unique rows of the inner side. The join does the special join when one set has its outer relations and the other set has its inner side, or for a semi join when one set is its inner side, which a join can make unique. The join is not legal when it does two special joins, or when it has a part of the inner side of a special join that it does not do, unless each set has a part of it.
    fn legal(&self, a: usize, b: usize) -> Option<Option<Match>> {
        let (r1, r2) = (&self.sets[a].rels, &self.sets[b].rels);
        let mut found = None;
        for (special, sp) in self.specials.iter().enumerate() {
            let joined = |r: &BTreeSet<usize>| !sp.right.is_disjoint(r);
            if !joined(r1) && !joined(r2) {
                continue;
            }
            if r1.iter().chain(r2).all(|r| sp.right.contains(r)) {
                continue;
            }
            let done = |r: &BTreeSet<usize>| sp.left.is_subset(r) && sp.right.is_subset(r);
            if done(r1) || done(r2) {
                continue;
            }
            let semi = sp.kind == JoinKind::Semi;
            let unique_done = |r: &BTreeSet<usize>| sp.right.is_subset(r) && sp.right != *r;
            if semi && (unique_done(r1) || unique_done(r2)) {
                continue;
            }
            let can_unique = !self.uniques[special].is_empty();
            let m = if sp.left.is_subset(r1) && sp.right.is_subset(r2) {
                Match { special, reversed: false, unique: false }
            } else if sp.left.is_subset(r2) && sp.right.is_subset(r1) {
                Match { special, reversed: true, unique: false }
            } else if semi && sp.right == *r2 && can_unique {
                Match { special, reversed: false, unique: true }
            } else if semi && sp.right == *r1 && can_unique {
                Match { special, reversed: true, unique: true }
            } else if joined(r1) && joined(r2) {
                continue;
            } else {
                return None;
            };
            if found.is_some() {
                return None;
            }
            found = Some(m);
        }
        Some(found)
    }

    /// The conditions of a join of two sets and the conditions that it tests on the rows that it makes. A special join also tests its own conditions.
    fn conditions(&self, outer: usize, inner: usize, how: How) -> (Vec<Qual<'q>>, Vec<Qual<'q>>) {
        let (l, r) = (&self.sets[outer].rels, &self.sets[inner].rels);
        let mut quals = between(self.pool, self.classes, l, r);
        match how {
            How::Plain => (quals, Vec::new()),
            How::Special(i) | How::Unique(i) => {
                let sp = &self.specials[i];
                quals.extend(sp.quals.iter().cloned());
                self.stats.sort_quals(&mut quals);
                (quals, sp.filter.clone())
            }
        }
    }

    /// `make_join_rel`: the join of two sets, when `join_is_legal` accepts it. The set of their items gets the plans of the join that `add_path` keeps, as `populate_joinrel_with_paths` makes them. A set that reads the other set with a lateral reference is the inner side, and a join that leaves a lateral reference to another item is not legal. The set gets its rows from the first join that makes it, as `build_join_rel`.
    fn make(&mut self, a: usize, b: usize) {
        let Some(matched) = self.legal(a, b) else { return };
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
        // A special join with a lateral reference is a nested loop with the set that reads the other as its inner side.
        if let Some(m) = matched
            && (a_reads_b || b_reads_a)
            && (m.unique || m.reversed != a_reads_b)
        {
            return;
        }
        let (outer, inner) = match matched {
            Some(m) if m.reversed => (b, a),
            Some(_) => (a, b),
            None if a_reads_b => (b, a),
            None => (a, b),
        };
        let mut hows = Vec::new();
        match matched {
            None => hows.push(How::Plain),
            Some(m) if m.unique => hows.push(How::Unique(m.special)),
            Some(m) => {
                hows.push(How::Special(m.special));
                let sp = &self.specials[m.special];
                if sp.kind == JoinKind::Semi
                    && self.sets[inner].rels == sp.right
                    && !self.uniques[m.special].is_empty()
                {
                    hows.push(How::Unique(m.special));
                }
            }
        }
        let mut made = Vec::new();
        let mut rows = None;
        for how in hows {
            let (quals, filter) = self.conditions(outer, inner, how);
            let (l, r) = (&self.sets[outer].plans, &self.sets[inner].plans);
            let fraction = || semi_selectivity(&quals, &r[0], self.stats);
            let (kind, part, inners) = match how {
                How::Plain => (JoinKind::Inner, Part::Plain, r),
                How::Special(i) => (self.specials[i].kind, Part::Semi(fraction()), r),
                How::Unique(i) => {
                    let whole = self.specials[i].left.is_subset(&self.sets[outer].rels);
                    (JoinKind::Inner, Part::Unique(fraction(), whole), &self.uniques[i])
                }
            };
            let outer_rows = l[0].node.cost.rows;
            rows.get_or_insert_with(|| match (how, part) {
                (How::Plain, _) => join_rows(kind, &l[0], &r[0], &quals, &[], self.stats),
                (_, Part::Semi(fraction)) if kind == JoinKind::Anti => {
                    let pushed = cost::list_selectivity(filter.iter().map(|q| &*q.expr));
                    cost::clamp_rows(outer_rows * (1.0 - fraction) * pushed)
                }
                (_, Part::Semi(fraction) | Part::Unique(fraction, _)) => {
                    cost::clamp_rows(outer_rows * fraction)
                }
                (_, Part::Plain) => unreachable!("a special join has its fraction"),
            });
            let found = choose(self.level, kind, l, inners, &quals, &filter, self.stats, part);
            made.extend(found.into_iter().map(|(candidate, keys)| (candidate, keys, how)));
        }
        let Some(rows) = rows.filter(|_| !made.is_empty()) else { return };
        let at = match self.index.get(&items) {
            Some(&e) => e,
            None => {
                let at = self.sets.len();
                self.index.insert(items, at);
                self.levels[items.count_ones() as usize].push(at);
                let paths = Paths::new(self.stats.startup);
                self.sets.push(Set { items, rels, plans: Vec::new(), paths, rows, lateral });
                at
            }
        };
        for (candidate, keys, how) in made {
            self.sets[at].paths.add(
                candidate.startup,
                candidate.total,
                keys,
                (outer, inner, candidate, how),
            );
        }
    }
}

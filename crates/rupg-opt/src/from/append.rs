//! The `Merge Append` of a `UNION ALL` in `FROM`, as `generate_orderedappend_paths` makes it for an append relation.

use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, FromItem, Query, Var};
use rupg_plan::{Cost, Item, Kind, Node, SortKey};

use super::paths::costs;
use super::{OrderKey, Paths, Plans, Rel, Stats, simple_union_all};
use crate::{Builder, Level, cost, pullup, upper};

/// A key of an order by the place of a column of the append relation, with its directions.
type Key = (usize, bool, bool);

/// The keys of the order of the rows that the query above an append relation can use, by the places of the columns of the relation, as `add_child_rel_equivalences` makes the equivalence classes of the query hold the columns of each member.
#[derive(Clone, Debug)]
pub(crate) struct ChildKeys {
    /// True for each column of the relation that the query reads, as `set_append_rel_size` builds the target of each member from the target of the relation.
    used: Vec<bool>,
    sort: Vec<Key>,
    groups: Vec<Vec<Key>>,
    /// The columns that a merge join of the query can test.
    joins: Vec<usize>,
    startup: bool,
}

/// The plans of the members of the append relation that the planner plans now, which the plan of the relation reads.
#[derive(Debug)]
pub(crate) struct AppendRel<'q> {
    /// The scope of the `UNION ALL`, which is the scope of the query outside each member.
    pub(crate) scope: usize,
    pub(crate) keys: ChildKeys,
    /// The plans of each member that `add_path` keeps, in the order of the members.
    pub(crate) plans: Vec<Plans<'q>>,
}

/// The `SELECT` of each member of a `UNION ALL`, in the order of the members of its `Append`.
fn leaves(sub: &Query) -> Vec<&Query> {
    let mut at = Vec::new();
    if let Some(op) = &sub.set_op {
        op.left.each_leaf(&mut |i| at.push(i));
        op.right.each_leaf(&mut |i| at.push(i));
    }
    at.into_iter()
        .filter_map(|i| sub.relations.get(i).and_then(|r| r.subquery.as_deref()))
        .collect()
}

/// The place of the column of the relation `r` that is the expression or that the equivalence classes make equal to it.
fn place(expr: &Expr, r: usize, stats: &Stats) -> Option<usize> {
    let at = |e: &Expr| match e.kind {
        ExprKind::Var(v) if v.relation == r && v.levels_up == 0 && v.attnum > 0 => {
            usize::try_from(v.attnum - 1).ok()
        }
        _ => None,
    };
    at(expr).or_else(|| {
        let (members, _) = stats.equal.iter().find(|(m, _)| m.iter().any(|e| e.same(expr)))?;
        members.iter().find_map(at)
    })
}

/// The keys of an order of the rows of a member by the places of the columns of the relation: the first keys that are targets of the member.
fn places(order: &[OrderKey], leaf: &Query) -> Vec<Key> {
    order
        .iter()
        .map_while(|k| {
            let p = leaf.targets.iter().position(|t| k.exprs.iter().any(|e| e.same(&t.expr)))?;
            Some((p, k.desc, k.nulls_first))
        })
        .collect()
}

impl ChildKeys {
    /// The keys of the query of `level` for the members of the `UNION ALL` of its relation `r`, or `None` when a member is not a `SELECT` of one table with no subquery, whose plans the planner keeps.
    pub(crate) fn new(
        b: &Builder<'_>,
        level: &Level<'_>,
        r: usize,
        sub: &Query,
        stats: &Stats,
    ) -> Option<ChildKeys> {
        if !simple_union_all(sub) {
            return None;
        }
        let member = |leaf: &Query| {
            let [FromItem::Relation(t)] = leaf.from.as_slice() else { return false };
            let table = &leaf.relations[*t];
            let sublink = leaf.all_exprs(0).into_iter().any(|(e, depth)| {
                e.find(depth, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(()))
                    .is_some()
            });
            pullup::simple(leaf)
                && table.oid != 0
                && table.subquery.is_none()
                && table.function.is_none()
                && table.values.is_none()
                && table.cte.is_none()
                && table.tablesample.is_none()
                && !sublink
                && b.members.is_some_and(|m| m.members(leaf, *t, &[]).is_none())
        };
        if !leaves(sub).into_iter().all(member) {
            return None;
        }
        let columns = pullup::used(level.query, r);
        let used = (1..=sub.targets.len())
            .map(|at| columns.iter().any(|&c| c == 0 || usize::try_from(c).ok() == Some(at)))
            .collect();
        let key = |(e, desc, nulls_first): &(Expr, bool, bool)| {
            place(e, r, stats).map(|p| (p, *desc, *nulls_first))
        };
        let sort = stats.sort.iter().map_while(key).collect();
        let groups = stats.groups.iter().map(|g| g.iter().filter_map(key).collect()).collect();
        let mine = BTreeSet::from([r]);
        let joins = stats
            .joins
            .iter()
            .flatten()
            .filter(|(_, rels)| *rels == mine)
            .filter_map(|(e, _)| place(e, r, stats))
            .collect();
        Some(ChildKeys { used, sort, groups, joins, startup: stats.startup })
    }

    /// True for each target of a member that the query above reads.
    pub(crate) fn used(&self) -> &[bool] {
        &self.used
    }

    /// Gives the estimates of a member the keys of the query above, with the targets of the member in place of the columns of the relation, so that `truncate_useless_pathkeys` keeps the plans of the member in those orders.
    pub(crate) fn apply(&self, level: &Level<'_>, stats: &mut Stats) {
        let targets = &level.query.targets;
        let exprs = |keys: &[Key]| -> Vec<(Expr, bool, bool)> {
            keys.iter()
                .filter_map(|&(p, desc, nf)| Some((targets.get(p)?.expr.clone(), desc, nf)))
                .collect()
        };
        stats.sort = stats.canonical(exprs(&self.sort), |k| &k.0);
        stats.groups = self.groups.iter().map(|g| stats.canonical(exprs(g), |k| &k.0)).collect();
        let top = BTreeSet::from([usize::MAX]);
        for t in self.joins.iter().filter_map(|&p| targets.get(p)) {
            stats.joins.push([(t.expr.clone(), top.clone()), (t.expr.clone(), top.clone())]);
        }
        stats.startup = self.startup;
    }
}

impl<'q> AppendRel<'q> {
    /// `add_paths_to_append_rel` and `generate_orderedappend_paths`: the plans of the relation `r` that `add_path` keeps, the scan `seq` of its `Append` and a `Merge Append` for each order of a plan of a member. A member with no plan in the order gets a sort of its cheapest plan, as `create_merge_append_path`. The relation takes the order with the columns of the relation in place of the targets of the members.
    pub(super) fn plans(
        self,
        level: &Level<'q>,
        r: usize,
        seq: Rel<'q>,
        stats: &Stats,
    ) -> Vec<Rel<'q>> {
        let relation = &level.query.relations[r];
        let Some(sub) = relation.subquery.as_deref() else { return vec![seq] };
        let leaves = leaves(sub);
        let append = seq.node.children.first();
        if self.plans.len() != leaves.len()
            || self.plans.iter().any(Vec::is_empty)
            || !append
                .is_some_and(|a| matches!(a.kind, Kind::Append) && a.children.len() == leaves.len())
        {
            return vec![seq];
        }
        let orders: Vec<Vec<Vec<Key>>> = self
            .plans
            .iter()
            .zip(&leaves)
            .map(|(plans, leaf)| plans.iter().map(|(_, order)| places(order, leaf)).collect())
            .collect();
        let mut all: Vec<&Vec<Key>> = Vec::new();
        for keys in orders.iter().flatten() {
            if !keys.is_empty() && !all.contains(&keys) {
                all.push(keys);
            }
        }
        let widths: Vec<i32> = leaves
            .iter()
            .map(|leaf| {
                let used = leaf.targets.iter().zip(&self.keys.used).filter(|(_, u)| **u);
                used.map(|(t, _)| cost::expr_width(&t.expr, &leaf.relations)).sum()
            })
            .collect();
        let bound = limit_tuples(level.query, r);
        let mut paths = Paths::new(stats.startup);
        paths.add(costs(&seq.node), Vec::new(), seq.clone());
        for keys in all {
            // `pathkeys_count_contained_in`: a member whose plans have only the first keys needs an incremental sort, which the planner does not cost yet.
            let partial = orders.iter().flatten().any(|o| {
                let n = o.iter().zip(keys).take_while(|(a, b)| a == b).count();
                n > 0 && n < keys.len()
            });
            if partial {
                continue;
            }
            // `get_cheapest_path_for_pathkeys`: the plan in the order that costs the least to start and the one that costs the least in total, or the cheapest plan with a sort.
            let mut startup = Vec::new();
            let mut total = Vec::new();
            for (plans, orders) in self.plans.iter().zip(&orders) {
                let sorted = || (0..plans.len()).filter(|&i| orders[i].starts_with(keys));
                let cost = |i: usize| plans[i].0.cost;
                let first = sorted().min_by(|&a, &b| {
                    let (a, b) = (cost(a), cost(b));
                    a.startup.total_cmp(&b.startup).then(a.total.total_cmp(&b.total))
                });
                let cheapest = sorted().min_by(|&a, &b| {
                    let (a, b) = (cost(a), cost(b));
                    a.total.total_cmp(&b.total).then(a.startup.total_cmp(&b.startup))
                });
                startup.push(first.unwrap_or(0));
                total.push(cheapest.unwrap_or(0));
            }
            let mut choices = vec![startup.clone()];
            if total != startup {
                choices.push(total);
            }
            for choice in choices {
                let mut rel =
                    self.merge(level, r, &seq, &leaves, keys, &choice, &widths, bound, stats);
                rel.order = stats.useful(&rel.rels, &rel.order);
                paths.add(costs(&rel.node), rel.order.clone(), rel);
            }
        }
        paths.kept()
    }

    /// `create_merge_append_path` and `cost_merge_append`: the scan of the relation `r` with a `Merge Append` of the plans `choice` of the members in the order `keys`.
    #[allow(clippy::too_many_arguments)]
    fn merge(
        &self,
        level: &Level<'q>,
        r: usize,
        seq: &Rel<'q>,
        leaves: &[&'q Query],
        keys: &[Key],
        choice: &[usize],
        widths: &[i32],
        bound: Option<f64>,
        stats: &Stats,
    ) -> Rel<'q> {
        let sort_keys = |leaf: &'q Query| -> Vec<SortKey<'q>> {
            keys.iter()
                .map(|&(p, desc, nulls_first)| SortKey {
                    item: Item::below(&leaf.targets[p].expr),
                    desc,
                    using: None,
                    nulls_first,
                    collation: 0,
                })
                .collect()
        };
        let mut members = Vec::with_capacity(choice.len());
        for (i, &at) in choice.iter().enumerate() {
            let (node, order) = &self.plans[i][at];
            let mut node = node.clone();
            node.cost.width = widths[i];
            if !places(order, leaves[i]).starts_with(keys) {
                let output = node.output.iter().map(Item::passed).collect();
                node = super::sort(node, sort_keys(leaves[i]), bound);
                node.output = output;
            }
            members.push(node);
        }
        let n = members.len().max(2) as f64;
        let log_n = n.log2();
        let comparison = 2.0 * cost::CPU_OPERATOR;
        let tuples: f64 = members.iter().map(|m| m.cost.rows).sum();
        let first = comparison * n * log_n;
        let run = tuples * comparison * log_n + cost::CPU_TUPLE * cost::APPEND_CPU * tuples;
        let input_startup: f64 = members.iter().map(|m| m.cost.startup).sum();
        let input_total: f64 = members.iter().map(|m| m.cost.total).sum();
        let append = &seq.node.children[0];
        let scope = members[0].scope;
        let mut merged = Node::new(Kind::MergeAppend(sort_keys(leaves[0])), scope, members);
        merged.cost = Cost {
            startup: first + input_startup,
            total: first + run + input_total,
            rows: tuples,
            width: append.cost.width,
        };
        let mut node = seq.node.clone();
        node.cost.startup = merged.cost.startup + (seq.node.cost.startup - append.cost.startup);
        node.cost.total = merged.cost.total + (seq.node.cost.total - append.cost.total);
        node.children = vec![merged];
        let columns = &level.query.relations[r].columns;
        let order = keys
            .iter()
            .map(|&(p, desc, nulls_first)| {
                let attnum = i16::try_from(p + 1).unwrap_or(i16::MAX);
                let mut expr =
                    Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), 0);
                if let Some(c) = columns.get(p) {
                    expr.ty = c.ty;
                    expr.typmod = c.typmod;
                }
                stats.order_key(&expr, desc, nulls_first)
            })
            .collect();
        Rel { node, rels: seq.rels.clone(), order }
    }
}

/// `limit_tuples` of `grouping_planner`: the rows that `LIMIT` and `OFFSET` read, for a sort of the relation `r` that is all the relations of the query, when the query has no step between the sort and `LIMIT`.
pub(super) fn limit_tuples(query: &Query, r: usize) -> Option<f64> {
    let only = matches!(query.from.as_slice(), [FromItem::Relation(at)] if *at == r);
    if !only
        || query.grouped
        || !query.group.is_empty()
        || !query.grouping_sets.is_empty()
        || !query.distinct.is_empty()
        || !query.windows.is_empty()
        || query.having.is_some()
        || query.has_target_srfs()
    {
        return None;
    }
    let (count, offset) = upper::limit_estimates(query);
    (count > 0.0 && offset >= 0.0).then_some(count + offset)
}

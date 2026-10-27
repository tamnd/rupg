//! The plans of the sides of `INTERSECT` and `EXCEPT`, as `build_setop_child_paths` and `generate_nonunion_paths` make them.

use rupg_analyze::{Expr, ExprKind, Query, SetKind, SetOp, SetTree};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node, SortKey};

use crate::from::paths::{Paths, costs};
use crate::from::{self, OrderKey, Stats};
use crate::{Builder, Level, cost, pullup, upper};

/// The plan of a side of a set operation in the order of all its columns, which the `SetOp` with the sorted strategy reads.
#[derive(Debug)]
pub(crate) enum Sorted<'q> {
    /// The cheapest plan of the side has the order.
    Cheapest,
    /// A plan of the side that is not the cheapest has the order and costs less than a sort of the cheapest plan.
    Plan(Box<Node<'q>>),
    /// A sort of the cheapest plan, with the rows that its caller reads.
    Sort(Option<f64>),
}

/// A `SELECT` of a side of `INTERSECT` or `EXCEPT` that the planner plans now, as the `setops` argument of `subquery_planner` gives it.
#[derive(Debug)]
pub(crate) struct Leaf<'q> {
    /// The scope of the set operation, which is the scope of the query outside the `SELECT`.
    pub(crate) scope: usize,
    pub(crate) op: &'q SetOp,
    /// The number of groups of the rows of the `SELECT`, as `build_setop_child_paths` gives it in `pNumGroups`.
    pub(crate) groups: f64,
    pub(crate) sorted: Sorted<'q>,
    /// True when the keys of the order of the `SELECT` are all its columns, as [`complete`].
    pub(crate) complete: bool,
}

impl<'q> Leaf<'q> {
    pub(crate) fn new(scope: usize, op: &'q SetOp) -> Leaf<'q> {
        Leaf { scope, op, groups: 0.0, sorted: Sorted::Sort(None), complete: false }
    }
}

/// `generate_setop_child_grouplist`: the keys of the columns of a `SELECT` of a set operation, in the order of the columns, or none when a column has no order.
pub(crate) fn all_keys(query: &Query, op: &SetOp) -> Vec<(Expr, bool, bool)> {
    if op.groups.iter().any(|g| g.operator == 0) {
        return Vec::new();
    }
    let columns = query.targets.iter().filter(|t| !t.junk);
    columns.zip(&op.groups).map(|(t, g)| (t.expr.clone(), false, g.nulls_first)).collect()
}

/// `setop_pathkeys`: the keys of [`all_keys`] with no key that the equivalence classes of the `SELECT` make redundant, such as a column that a condition makes equal to a constant.
pub(crate) fn keys(query: &Query, op: &SetOp, stats: &Stats) -> Vec<(Expr, bool, bool)> {
    stats.canonical(all_keys(query, op), |k| &k.0)
}

/// True when `setop_pathkeys` of a `SELECT` has a key for each column: no column is a constant, and the equivalence classes of the `SELECT` make no key redundant. Only then a plan of the `SELECT` in its order has the keys of all the columns of a `UNION`, which `get_cheapest_path_for_pathkeys` of `generate_union_paths` looks for.
pub(crate) fn complete(query: &Query, op: &SetOp, stats: &Stats) -> bool {
    let all = all_keys(query, op);
    !all.is_empty()
        && !all.iter().any(|k| is_const(&k.0))
        && keys(query, op, stats).len() == all.len()
}

/// True when the plans of `FROM` of a `SELECT` are the plans of the `SELECT`, with only the targets above them.
pub(crate) fn simple(query: &Query) -> bool {
    pullup::simple(query) && query.minmax.is_none()
}

/// True when the rows of an order have the keys.
pub(crate) fn ordered(order: &[OrderKey], keys: &[(Expr, bool, bool)]) -> bool {
    !keys.is_empty()
        && order.len() >= keys.len()
        && order.iter().zip(keys).all(|(k, (e, desc, nulls_first))| k.sorts(e, *desc, *nulls_first))
}

/// The number of groups of the rows of a `SELECT` of a side, as `build_setop_child_paths` gives it: the rows of a `SELECT` with groups, and else `estimate_num_groups` of its columns.
pub(crate) fn groups(query: &Query, stats: &Stats, rows: f64) -> f64 {
    if query.grouped || !query.distinct.is_empty() {
        return rows;
    }
    let exprs: Vec<&Expr> = query.targets.iter().filter(|t| !t.junk).map(|t| &t.expr).collect();
    stats.groups(&exprs, rows)
}

/// `limit_tuples` of `grouping_planner`: the rows that `LIMIT` and `OFFSET` read, when the query has no step between its sort and `LIMIT`.
fn limit_tuples(query: &Query) -> Option<f64> {
    if query.grouped
        || !query.distinct.is_empty()
        || !query.windows.is_empty()
        || query.has_target_srfs()
    {
        return None;
    }
    let (count, offset) = upper::limit_estimates(query);
    (count > 0.0 && offset >= 0.0).then_some(count + offset)
}

/// `build_setop_child_paths` and `generate_nonunion_paths`: the plan of a `SELECT` in the order of its columns. The cheapest plan `cheapest` has the order when its rows have the keys of `setop_pathkeys`. When no key is redundant, a plan of `finals` with the order, which are the plans that the `SELECT` keeps with true for the plans with the order, is a choice when `others` is true, and else the planner sorts the cheapest plan. A sort of the cheapest plan reads only the rows of `LIMIT` of the `SELECT` when no key is redundant.
pub(crate) fn sorted<'q>(
    query: &Query,
    op: &SetOp,
    stats: &Stats,
    cheapest: &Node<'q>,
    finals: Vec<(Node<'q>, bool)>,
    others: bool,
) -> Sorted<'q> {
    if op.groups.iter().any(|g| g.operator == 0) {
        return Sorted::Sort(None);
    }
    // `generate_setop_tlist`: a constant column of the `SELECT` stays a constant above it, and its key is redundant.
    let all: Vec<_> = all_keys(query, op).into_iter().filter(|k| !is_const(&k.0)).collect();
    let keys: Vec<_> = keys(query, op, stats).into_iter().filter(|k| !is_const(&k.0)).collect();
    if all.is_empty() {
        return Sorted::Cheapest;
    }
    if keys.len() < all.len() {
        return Sorted::Sort(None);
    }
    let bound = limit_tuples(query);
    if finals.is_empty() {
        // The rows of `ORDER BY` have the order when its keys come first in the order of the columns.
        let sort: Vec<(Expr, bool, bool)> = query
            .sort
            .iter()
            .filter_map(|s| {
                let desc = builtin::operator_by_oid(s.operator).is_some_and(|o| o.name == ">");
                Some((query.targets.get(s.target)?.expr.clone(), desc, s.nulls_first))
            })
            .collect();
        let sort = stats.canonical(sort, |k| &k.0);
        let has = sort.len() >= keys.len()
            && sort.iter().zip(&keys).all(|(a, b)| a.0.same(&b.0) && a.1 == b.1 && a.2 == b.2);
        return if has { Sorted::Cheapest } else { Sorted::Sort(bound) };
    }
    let mut paths = Paths::new(false);
    for (i, (node, _)) in finals.iter().enumerate() {
        paths.add(costs(node), Vec::new(), i);
    }
    if paths.fractional(0.0, |&i| finals[i].0.cost.rows).is_some_and(|i| finals[i].1) {
        return Sorted::Cheapest;
    }
    let c = cheapest.cost;
    let (_, sort_total) = cost::sort((c.startup, c.total), c.rows, c.width, bound);
    let best = finals
        .into_iter()
        .filter(|(node, sorted)| others && *sorted && !has_member(node))
        .map(|(node, _)| node)
        .min_by(|a, b| a.cost.total.total_cmp(&b.cost.total));
    match best {
        Some(node) if node.cost.total < sort_total => Sorted::Plan(Box::new(node)),
        _ => Sorted::Sort(bound),
    }
}

/// The order of the rows of the `SetOp` with the sorted strategy, in the columns of the set operation, as `nonunion_pathkeys`.
pub(crate) fn order(query: &Query, op: &SetOp) -> Vec<OrderKey> {
    let columns = query.targets.iter().filter(|t| !t.junk);
    columns
        .zip(&op.groups)
        .map(|(t, g)| OrderKey {
            exprs: vec![t.expr.clone()],
            desc: false,
            nulls_first: g.nulls_first,
        })
        .collect()
}

/// `EstimateSetOpHashTableSpace`: the bytes of the hash table of a `SetOp` with `groups` entries of `width` bytes, as `tuplehash_estimate_space` gives the buckets of 16 bytes and each entry has a `MinimalTuple` header of 16 bytes and two counts of 8 bytes.
pub(crate) fn hash_space(groups: f64, width: i32) -> f64 {
    let buckets = (groups / 0.9).floor().max(2.0);
    let buckets = 2f64.powf(buckets.log2().ceil());
    16.0 * buckets + groups * (16.0 + f64::from(cost::maxalign(width)) + 16.0)
}

/// True when the expression is a constant, also with the casts that `generate_setop_tlist` puts on a constant of a `SELECT` of a set operation.
pub(crate) fn is_const(expr: &Expr) -> bool {
    matches!(expr.strip_implicit().kind, ExprKind::Const(_))
}

/// True when a node or a node below it scans a member of an inheritance tree, which takes a name of its own in the plan.
pub(crate) fn has_member(node: &Node<'_>) -> bool {
    node.member.is_some() || node.children.iter().any(has_member)
}

/// A side of `INTERSECT` or `EXCEPT`: its cheapest plan, the number of groups of its rows, its plan in the order of its columns, and the keys and the scope of a sort of its rows.
struct Side<'q> {
    node: Node<'q>,
    groups: f64,
    sorted: Sorted<'q>,
    keys: Vec<SortKey<'q>>,
    scope: usize,
    complete: bool,
}

impl<'q> Side<'q> {
    /// The costs of the plan of the side in the order of its columns.
    fn sorted_cost(&self) -> Cost {
        let c = self.node.cost;
        match &self.sorted {
            Sorted::Cheapest => c,
            Sorted::Plan(node) => node.cost,
            Sorted::Sort(bound) => {
                let (startup, total) = cost::sort((c.startup, c.total), c.rows, c.width, *bound);
                Cost { startup, total, ..c }
            }
        }
    }

    /// The disabled nodes of [`Self::sorted_plan`]: a `Sort` with `enable_sort` off is one more.
    fn sorted_disabled(&self) -> u32 {
        match &self.sorted {
            Sorted::Cheapest => self.node.disabled_nodes(),
            Sorted::Plan(node) => node.disabled_nodes(),
            Sorted::Sort(_) => self.node.disabled_nodes() + u32::from(!crate::enable().sort),
        }
    }

    /// The plan of the side in the order of its columns.
    fn sorted_plan(&self) -> Node<'q> {
        match &self.sorted {
            Sorted::Cheapest => self.node.clone(),
            Sorted::Plan(node) => node.as_ref().clone(),
            Sorted::Sort(bound) => {
                let output = self.node.output.iter().map(Item::passed).collect();
                let mut node = from::sort(self.node.clone(), self.keys.clone(), *bound);
                node.scope = self.scope;
                node.output = output;
                node
            }
        }
    }
}

impl<'q> Builder<'q> {
    /// `generate_nonunion_paths`: the plans of `INTERSECT` or `EXCEPT` that `add_path` keeps, the cheapest first, each a `SetOp` that hashes the rows of its sides or, with true, that reads them in the order of their columns. `top` is true for the set operation of the query, of whose rows the caller of the query reads the fraction of the level.
    pub(crate) fn nonunion(
        &mut self,
        level: &Level<'q>,
        op: &'q SetOp,
        top: bool,
    ) -> Vec<(Node<'q>, bool)> {
        let start = self.current.len();
        let mut left = self.side(level, op, &op.left);
        let middle = self.current.len();
        let mut right = self.side(level, op, &op.right);
        // `INTERSECT` reads first the side with fewer groups. Its relations then take their names first, as `set_rtable_names` names them in the order of the plan.
        if op.kind == SetKind::Intersect && left.groups > right.groups {
            std::mem::swap(&mut left, &mut right);
            if start <= middle && middle <= self.current.len() {
                self.current[start..].rotate_left(middle - start);
            }
        }
        let except = op.kind == SetKind::Except;
        let (l, r) = (left.node.cost, right.node.cost);
        let groups = left.groups;
        let rows = match (except, op.all) {
            (true, true) => l.rows,
            (false, true) => l.rows.min(r.rows),
            (_, false) => groups,
        };
        let columns = op.columns.len() as f64;
        let sortable = op.groups.iter().all(|g| g.operator != 0);
        let hashable = op.groups.iter().all(|g| g.hashable);
        // `create_setop_path`: a hash table that does not fit in `hash_mem` is a disabled node.
        let fits = hash_space(groups, l.width) < cost::HASH_MEM;
        let startup = l.total + r.total + cost::CPU_OPERATOR * (l.rows + r.rows) * columns;
        let hashed =
            Cost { startup, total: startup + cost::CPU_OPERATOR * rows, rows, width: l.width };
        let (sl, sr) = (left.sorted_cost(), right.sorted_cost());
        let sorted = Cost {
            startup: sl.startup + sr.startup,
            total: sl.total
                + sr.total
                + cost::CPU_OPERATOR * (sl.rows + sr.rows) * columns
                + cost::CPU_OPERATOR * rows,
            rows,
            width: l.width,
        };
        // `add_path`: the hashed plan comes first, and the sorted plan with about the same costs wins with the order of its rows.
        let order = order(level.query, op);
        let mut paths = Paths::new(top && level.fraction > 0.0);
        // `create_setop_path`: the disabled nodes of the sides, and one more for a hash table with `enable_hashagg` off or that does not fit.
        let below = left.node.disabled_nodes() + right.node.disabled_nodes();
        let hash_disabled = below + u32::from(!crate::enable().hashagg) + u32::from(!fits);
        let sort_disabled = left.sorted_disabled() + right.sorted_disabled();
        if hashable {
            paths.add((hash_disabled, hashed.startup, hashed.total), Vec::new(), false);
        }
        if sortable || !hashable {
            paths.add((sort_disabled, sorted.startup, sorted.total), order, true);
        }
        let scope = left.scope;
        let make = |sorted_strategy: bool, children: Vec<Node<'q>>, c: Cost| {
            let kind = Kind::SetOp { except, all: op.all, sorted: sorted_strategy };
            let mut node = Node::new(kind, scope, children);
            node.cost = c;
            node
        };
        paths
            .kept()
            .into_iter()
            .map(|s| {
                let node = if s {
                    make(true, vec![left.sorted_plan(), right.sorted_plan()], sorted)
                } else {
                    let mut node = make(false, vec![left.node.clone(), right.node.clone()], hashed);
                    node.disabled = hash_disabled > below;
                    node
                };
                (node, s)
            })
            .collect()
    }

    /// A `SELECT` of a `UNION` without `ALL`, which `recurse_set_operations` plans with the operation as `parentOp`: its cheapest plan, and its plan in the order of all the columns of the operation, which `get_cheapest_path_for_pathkeys` of `generate_union_paths` finds among the plans that `build_setop_child_paths` adds. A `SELECT` that is not [`complete`] has no such plan.
    pub(crate) fn union_leaf(
        &mut self,
        level: &Level<'q>,
        op: &'q SetOp,
        tree: &'q SetTree,
    ) -> (Node<'q>, Option<Node<'q>>) {
        let side = self.side(level, op, tree);
        let ordered = side.complete.then(|| side.sorted_plan());
        (side.node, ordered)
    }

    /// The plan of a side of `INTERSECT` or `EXCEPT`. The planner plans a `SELECT` with the set operation as its `setops`.
    fn side(&mut self, level: &Level<'q>, op: &'q SetOp, tree: &'q SetTree) -> Side<'q> {
        let keys = |items: Vec<Item<'q>>| -> Vec<SortKey<'q>> {
            items
                .into_iter()
                .zip(&op.groups)
                .map(|(item, g)| SortKey {
                    item,
                    desc: false,
                    using: None,
                    nulls_first: g.nulls_first,
                })
                .collect()
        };
        match tree {
            SetTree::Leaf(i) => {
                let Some(sub) = level.query.relations.get(*i).and_then(|r| r.subquery.as_deref())
                else {
                    let node = self.set_tree(level, tree);
                    let (groups, scope) = (node.cost.rows, node.scope);
                    return Side {
                        node,
                        groups,
                        sorted: Sorted::Sort(None),
                        keys: Vec::new(),
                        scope,
                        complete: false,
                    };
                };
                let outer = self.setop.replace(Leaf::new(level.scope, op));
                let node = self.query(sub, Some(level.scope));
                let leaf = std::mem::replace(&mut self.setop, outer);
                let (groups, sorted, complete) = leaf
                    .map_or((node.cost.rows, Sorted::Sort(None), false), |l| {
                        (l.groups, l.sorted, l.complete)
                    });
                let items =
                    sub.targets.iter().filter(|t| !t.junk).map(|t| Item::below(&t.expr)).collect();
                let mut keys = keys(items);
                keys.retain(|k| !is_const(&k.item.expr));
                let scope = node.scope;
                Side { node, groups, sorted, keys, scope, complete }
            }
            SetTree::Op(inner) => {
                let (node, sorted) = if inner.kind == SetKind::Union || inner.work.is_some() {
                    let node = self.set_node(level, inner);
                    // The `Unique` of a `UNION` reads its rows in the order of all its columns.
                    let sorted = if matches!(node.kind, Kind::Unique) {
                        Sorted::Cheapest
                    } else {
                        Sorted::Sort(None)
                    };
                    (node, sorted)
                } else {
                    // `get_cheapest_path_for_pathkeys`: the sorted plan of the set operation, when `add_path` keeps it, gives the order.
                    let mut plans = self.nonunion(level, inner, false).into_iter();
                    let Some((node, chosen)) = plans.next() else {
                        unreachable!("a set operation has a plan")
                    };
                    let sorted = match plans.next() {
                        _ if chosen => Sorted::Cheapest,
                        Some((other, true)) => Sorted::Plan(Box::new(other)),
                        _ => Sorted::Sort(None),
                    };
                    (node, sorted)
                };
                let scope = match node.kind {
                    Kind::SetOp { .. } => node.scope,
                    _ => self.leftmost_scope(level, inner).unwrap_or(level.scope),
                };
                let items = self.leaf_items(level, inner);
                Side {
                    groups: node.cost.rows,
                    node,
                    sorted,
                    keys: keys(items),
                    scope,
                    complete: false,
                }
            }
        }
    }
}

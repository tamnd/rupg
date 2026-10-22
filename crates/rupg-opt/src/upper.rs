//! The nodes above the join of `FROM`, as `grouping_planner` makes them: the aggregates, the windows, the functions that give sets, `DISTINCT`, `ORDER BY`, the locks of rows and `LIMIT`. Also the plan of a set operation and the node of a statement that writes a table.

use rupg_analyze::{
    Aggref, Expr, ExprKind, Query, SetKind, SetOp, SetTree, SortGroup, WindowFunc, Write,
    WriteKind, expand_grouping_sets,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{Conflict, Cost, Item, Kind, Node, Operation, SortKey, Strategy, TARGET};

use crate::from::paths::Paths;
use crate::from::{self, OrderKey, sort};
use crate::{Builder, Level, cost, exprs};

/// `FRAMEOPTION_DEFAULTS`: `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
const FRAME_DEFAULTS: i32 = 0x422;
/// `FRAMEOPTION_ROWS`.
const FRAME_ROWS: i32 = 0x4;
/// `FRAMEOPTION_NONDEFAULT | FRAMEOPTION_ROWS | FRAMEOPTION_START_UNBOUNDED_PRECEDING | FRAMEOPTION_END_CURRENT_ROW`: the frame that the support function of `row_number` and the other functions of the rank of a row gives.
const FRAME_ROWS_CURRENT: i32 = 0x1 | FRAME_ROWS | 0x20 | FRAME_END_CURRENT_ROW;
/// `FRAMEOPTION_END_UNBOUNDED_FOLLOWING`.
const FRAME_END_UNBOUNDED_FOLLOWING: i32 = 0x100;
/// `FRAMEOPTION_END_CURRENT_ROW`.
const FRAME_END_CURRENT_ROW: i32 = 0x400;
/// `FRAMEOPTION_END_OFFSET_FOLLOWING`.
const FRAME_END_OFFSET_FOLLOWING: i32 = 0x4000;
/// The most grouping sets that the planner expands, as the limit of `expand_grouping_sets` in `subquery_planner`.
const MAX_SETS: usize = 4096;

/// The conditions of `HAVING` that go to `WHERE`, and the conditions that stay in `HAVING`, as `subquery_planner` divides them. A condition with no aggregate goes to `WHERE`, but with no `GROUP BY` it also stays, because the group is there also when no row passes.
pub(crate) fn having_split(query: &Query) -> (Vec<&Expr>, Vec<&Expr>) {
    let mut parts = Vec::new();
    if let Some(having) = &query.having {
        exprs::and_parts(having, &mut parts);
    }
    let (mut to_where, mut keep) = (Vec::new(), Vec::new());
    for part in parts {
        let mut aggs = Vec::new();
        exprs::aggregates(&[part], &mut aggs);
        let stays = !aggs.is_empty()
            || has_grouping(part)
            || exprs::volatile(part)
            || has_sublink(part)
            || !query.grouping_sets.is_empty();
        if stays {
            keep.push(part);
        } else if !query.group.is_empty() {
            to_where.push(part);
        } else {
            to_where.push(part);
            keep.push(part);
        }
    }
    (to_where, keep)
}

/// True when the expression calls `GROUPING` of the query.
fn has_grouping(expr: &Expr) -> bool {
    expr.find(0, &mut |e, depth| match &e.kind {
        ExprKind::Grouping(g) if g.levels_up == depth => Some(()),
        _ => None,
    })
    .is_some()
}

/// True when the expression has a subquery.
fn has_sublink(expr: &Expr) -> bool {
    expr.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
}

/// A `Material` node over a node.
pub(crate) fn materialize(node: Node<'_>) -> Node<'_> {
    from::materialize(node)
}

/// A key of a sort for an item of `ORDER BY`, `GROUP BY` or `DISTINCT` with its operator: `ASC` for the `<` of the type, `DESC` for its `>`, or else `USING` with the operator.
fn sort_key<'q>(expr: &'q Expr, item: &SortGroup) -> SortKey<'q> {
    let name = builtin::operator_by_oid(item.operator).map_or("<", |o| o.name);
    SortKey {
        item: Item::below(expr),
        desc: name == ">",
        using: (name != "<" && name != ">").then(|| name.to_string()),
        nulls_first: item.nulls_first,
    }
}

/// The order of the rows after a sort on the keys.
fn sorted_order(keys: &[SortKey<'_>]) -> Vec<OrderKey> {
    keys.iter()
        .map(|k| OrderKey {
            exprs: vec![k.item.expr.as_ref().clone()],
            desc: k.desc,
            nulls_first: k.nulls_first,
        })
        .collect()
}

/// `pathkeys_count_contained_in`: true when the rows are in the order of the keys, and the number of the first keys that the rows are in the order of. A key whose value is a value of an earlier key of the order adds nothing, as `pathkey_is_redundant`.
fn presorted(order: &[OrderKey], keys: &[SortKey<'_>]) -> (bool, usize) {
    let mut at = 0;
    for (i, k) in keys.iter().enumerate() {
        if k.using.is_some() {
            return (false, i);
        }
        if order[..at].iter().any(|o| o.exprs.iter().any(|e| e.same(&k.item.expr))) {
            continue;
        }
        if !order.get(at).is_some_and(|o| o.sorts(&k.item.expr, k.desc, k.nulls_first)) {
            return (false, i);
        }
        at += 1;
    }
    (true, keys.len())
}

/// The costs of a sort of rows with these costs whose first `presorted` keys are in order already: a `Sort` when no key is in order, or else an `Incremental Sort`, as `make_ordered_path` chooses.
fn sort_cost(
    level: &Level<'_>,
    c: Cost,
    keys: &[SortKey<'_>],
    presorted: usize,
    bound: Option<f64>,
) -> (f64, f64) {
    if presorted == 0 {
        cost::sort((c.startup, c.total), c.rows, c.width, bound)
    } else {
        let exprs: Vec<&Expr> = keys[..presorted].iter().map(|k| &*k.item.expr).collect();
        let groups = level.stats.groups(&exprs, c.rows.max(2.0));
        cost::incremental_sort((c.startup, c.total), c.rows, c.width, groups, bound)
    }
}

/// `make_ordered_path`: the node in the order of the keys. A node whose rows are in the order already stays as it is. A node whose rows are in the order of the first keys gets an `Incremental Sort`, and another node gets a `Sort`. With `only`, the node is a plan that is not the cheapest, which the planner uses only when its rows are in the order of one key or more, so the result is `None` when it needs a `Sort`.
fn sort_if<'q>(
    level: &Level<'q>,
    node: Node<'q>,
    keys: Vec<SortKey<'q>>,
    order: &mut Vec<OrderKey>,
    bound: Option<f64>,
    only: bool,
) -> Option<Node<'q>> {
    let (sorted, count) = presorted(order, &keys);
    if keys.is_empty() || sorted {
        return Some(node);
    }
    if only && count == 0 {
        return None;
    }
    *order = sorted_order(&keys);
    if count == 0 {
        return Some(sort(node, keys, bound));
    }
    let c = node.cost;
    let (startup, total) = sort_cost(level, c, &keys, count, bound);
    let scope = node.scope;
    let mut out = Node::new(Kind::IncrementalSort { keys, presorted: count }, scope, vec![node]);
    out.cost = Cost { startup, total, ..c };
    Some(out)
}

/// The cost of the transition of the aggregate calls for each row, in units of `cpu_operator_cost`, as `get_agg_clause_costs`: a call of the transition function and the cost of the arguments and of `FILTER`.
fn agg_units(aggs: &[&Aggref]) -> f64 {
    aggs.iter()
        .map(|a| {
            let args: usize = a.args.iter().map(exprs::calls).sum();
            let filter = a.filter.as_deref().map_or(0, exprs::calls);
            (1 + args + filter) as f64
        })
        .sum()
}

/// The cost of the final functions of the aggregate calls for each group, in units of `cpu_operator_cost`, as the `finalCost` of `get_agg_clause_costs`: a call of the final function of each call that has one.
fn final_units(aggs: &[&Aggref]) -> f64 {
    aggs.iter().filter(|a| builtin::aggregate(a.oid).is_some_and(|g| g.finalfn != 0)).count() as f64
}

/// The value of a constant `bigint` of `LIMIT` or `OFFSET`, `Some(None)` for a null constant, or `None` for an expression that is not a constant.
fn const_count(expr: &Expr) -> Option<Option<f64>> {
    use rupg_types::Value;
    match expr.strip_implicit().as_const()? {
        Value::Null => Some(None),
        Value::Int8(v) => Some(Some(*v as f64)),
        Value::Int4(v) => Some(Some(f64::from(*v))),
        Value::Int2(v) => Some(Some(f64::from(*v))),
        _ => None,
    }
}

/// The estimates of `LIMIT` and `OFFSET`, as `preprocess_limit`: 0 for no clause, -1 for an expression that is not a constant, or else the value.
pub(crate) fn limit_estimates(query: &Query) -> (f64, f64) {
    let count = query.limit.as_ref().map_or(0.0, |l| match const_count(l) {
        Some(None) => 0.0,
        Some(Some(v)) => v.max(1.0),
        None => -1.0,
    });
    let offset = query.offset.as_ref().map_or(0.0, |o| match const_count(o) {
        Some(None) => 0.0,
        Some(Some(v)) => v.max(0.0),
        None => -1.0,
    });
    (count, offset)
}

/// `preprocess_limit`: the `tuple_fraction` of a query, the fraction of its rows that the planner plans for, from the `fraction` that the caller reads and from `LIMIT` and `OFFSET`. A fraction of 1 or more is a number of rows, and 0 is all the rows.
pub(crate) fn tuple_fraction(query: &Query, fraction: f64) -> f64 {
    if query.limit.is_none() && query.offset.is_none() {
        return fraction;
    }
    let (count, offset) = limit_estimates(query);
    if count != 0.0 {
        let limit = if count < 0.0 || offset < 0.0 { 0.10 } else { count + offset };
        if fraction >= 1.0 {
            if limit >= 1.0 { fraction.min(limit) } else { fraction }
        } else if fraction > 0.0 {
            if limit >= 1.0 { limit } else { fraction.min(limit) }
        } else {
            limit
        }
    } else if offset != 0.0 && fraction > 0.0 {
        let limit = if offset < 0.0 { 0.10 } else { offset };
        if fraction >= 1.0 {
            if limit >= 1.0 { fraction + limit } else { limit }
        } else if limit >= 1.0 {
            fraction
        } else if fraction + limit >= 1.0 {
            0.0
        } else {
            fraction + limit
        }
    } else {
        fraction
    }
}

/// `limit_needed`: false when `LIMIT` is a null constant or absent and `OFFSET` is absent, null or 0.
fn limit_needed(query: &Query) -> bool {
    let limit = query.limit.as_ref().is_some_and(|l| const_count(l) != Some(None));
    let offset = query
        .offset
        .as_ref()
        .is_some_and(|o| !matches!(const_count(o), Some(None) | Some(Some(0.0))));
    limit || offset
}

/// `adjust_limit_rows_costs`: the costs and the rows of a `Limit` over a node.
fn limit_cost(input: Cost, count: f64, offset: f64) -> Cost {
    let mut out = input;
    let input_rows = input.rows;
    if offset != 0.0 {
        let skip = if offset > 0.0 { offset } else { cost::clamp_rows(input_rows * 0.1) };
        let skip = skip.min(out.rows);
        if out.rows > 0.0 {
            out.startup += (out.total - out.startup) * skip / input_rows;
        }
        out.rows = (out.rows - skip).max(1.0);
    }
    if count != 0.0 {
        let take = if count > 0.0 { count } else { cost::clamp_rows(input_rows * 0.1) };
        let take = take.min(out.rows);
        if input_rows > 0.0 {
            out.total = out.startup + (input.total - input.startup) * take / input_rows;
        }
        out.rows = take.max(1.0);
    }
    out
}

/// The start cost and the cost for each row of the targets at each step above the plan of `FROM`, as `grouping_planner` makes them and `set_pathtarget_cost_width` finds their costs.
#[derive(Clone, Copy, Default)]
struct TargetCosts {
    /// The targets that the plan of `FROM` computes, as `apply_scanjoin_target_to_paths`: the keys of `GROUP BY` with groups, the keys of the windows with windows, and else the targets before the sort.
    scan: (f64, f64),
    /// The targets of the `Agg` node, as `create_agg_path` adds them.
    group: (f64, f64),
    /// The targets of a `WindowAgg` node below the last one, which are the targets of the input of the windows.
    window: (f64, f64),
    /// The targets before the sort, which the last `WindowAgg` node and a `HashAggregate` of `DISTINCT` compute.
    last: (f64, f64),
}

/// The costs of the targets of [`TargetCosts`], or `None` for a query with a function that gives a set in the targets. With `ORDER BY`, a target that is not a key of the query and calls a volatile function, or costs more than 10 times `cpu_operator_cost` with `LIMIT`, comes after the sort, as `make_sort_input_target` postpones it. A write computes only the columns of the table.
fn target_costs(level: &Level<'_>) -> Option<TargetCosts> {
    let query = level.query;
    if level.no_targets || query.set_op.is_some() || query.has_target_srfs() {
        return None;
    }
    let targets = match &query.write {
        Some(write) if write.kind == WriteKind::Merge => return None,
        Some(write) => &query.targets[..write.width.min(query.targets.len())],
        None => &query.targets[..],
    };
    let calls = window_calls(query);
    let windows: Vec<usize> = (0..calls.len()).filter(|&w| !calls[w].is_empty()).collect();
    let window_keys: Vec<usize> = windows
        .iter()
        .flat_map(|&w| query.windows[w].partition.iter().chain(&query.windows[w].order))
        .map(|s| s.target)
        .collect();
    let group_keys: Vec<usize> = query.group.iter().map(|s| s.target).collect();
    let keys: Vec<usize> = query
        .sort
        .iter()
        .chain(&query.distinct)
        .map(|s| s.target)
        .chain(group_keys.iter().copied())
        .chain(window_keys.iter().copied())
        .collect();
    let costs: Vec<(f64, f64)> =
        targets.iter().map(|t| level.stats.qual_costs([&t.expr])).collect();
    // A target that is not a key and is volatile or costs more than 10 times `cpu_operator_cost` can come after the sort. They all do when one of them is volatile, or when the query has `LIMIT` or reads a part of its rows.
    let late: Vec<(bool, bool)> = targets
        .iter()
        .zip(&costs)
        .enumerate()
        .map(|(at, (t, cost))| {
            let can = !query.sort.is_empty() && !keys.contains(&at);
            (can && exprs::volatile(&t.expr), can && cost.1 > 10.0 * cost::CPU_OPERATOR)
        })
        .collect();
    let postpone = late.iter().any(|l| l.0)
        || late.iter().any(|l| l.1) && (query.limit.is_some() || level.fraction > 0.0);
    let sum = |keep: &dyn Fn(usize) -> bool| {
        costs
            .iter()
            .enumerate()
            .filter(|&(at, _)| keep(at))
            .fold((0.0, 0.0), |out, (_, c)| (out.0 + c.0, out.1 + c.1))
    };
    let last = sum(&|at| !(postpone && (late[at].0 || late[at].1)));
    let window = sum(&|at| window_keys.contains(&at));
    let group = if windows.is_empty() { last } else { window };
    let scan = if query.grouped {
        sum(&|at| group_keys.contains(&at))
    } else if windows.is_empty() {
        last
    } else {
        window
    };
    Some(TargetCosts { scan, group, window, last })
}

/// The node computes the targets, with their costs, as `create_projection_path` on the plan of `FROM` and as `create_agg_path` and `create_windowagg_path` add them. The scans of the members of a table with children each compute them, as `apply_scanjoin_target_to_paths` gives the target to each child of a partitioned table.
fn project(node: &mut Node<'_>, startup: f64, per_row: f64) {
    if startup == 0.0 && per_row == 0.0 {
        return;
    }
    if matches!(node.kind, Kind::Append) && node.children.iter().all(|c| c.member.is_some()) {
        for child in &mut node.children {
            let added = startup + per_row * child.cost.rows;
            child.cost.startup += startup;
            child.cost.total += added;
            node.cost.total += added;
        }
        node.cost.startup = node.children.first().map_or(0.0, |c| c.cost.startup);
        return;
    }
    node.cost.startup += startup;
    node.cost.total += startup + per_row * node.cost.rows;
}

/// The number of rows of a function that gives a set in the targets, as `expression_returns_set_rows`.
fn set_rows(expr: &Expr) -> f64 {
    let own = match &expr.kind {
        ExprKind::Func(f) if f.retset => from::function_rows(expr),
        _ => 1.0,
    };
    expr.children().into_iter().map(set_rows).fold(own, f64::max)
}

impl<'q> Builder<'q> {
    /// `grouping_planner`: the nodes above the join of `FROM`, over each plan of `FROM` that `add_path` keeps, with the cheapest plan first. The cheapest plan gets a sort where a step needs one, and each other plan is used only when its rows are in the order of each step, as `create_ordered_paths` and the other functions of `planner.c` do it. Of the results, the plan that costs the least to give the `fraction` of its rows that the caller reads wins, as `get_cheapest_fractional_path`.
    pub(crate) fn upper(
        &mut self,
        level: &Level<'q>,
        plans: Vec<(Node<'q>, Vec<OrderKey>)>,
        fraction: f64,
    ) -> Node<'q> {
        let mut paths = Paths::new(level.fraction > 0.0);
        for (i, (node, order)) in plans.into_iter().enumerate() {
            if let Some(node) = self.upper_one(level, node, order, i > 0) {
                paths.add(node.cost.startup, node.cost.total, Vec::new(), node);
            }
        }
        let Some(node) = paths.fractional(fraction, |n| n.cost.rows) else {
            unreachable!("the cheapest plan gets a result")
        };
        node
    }

    /// The nodes above one plan of `FROM`. With `only`, the plan is not the cheapest, and the result is `None` when a step needs a sort of it or a step has no plan for it.
    fn upper_one(
        &mut self,
        level: &Level<'q>,
        mut node: Node<'q>,
        mut order: Vec<OrderKey>,
        only: bool,
    ) -> Option<Node<'q>> {
        let query = level.query;
        let scope = level.display;
        let costs = target_costs(level).unwrap_or_default();
        project(&mut node, costs.scan.0, costs.scan.1);
        if query.set_op.is_none() {
            if query.grouped {
                node = aggregate(level, node, &mut order, only)?;
                project(&mut node, costs.group.0, costs.group.1);
            }
            node = windows(level, node, &mut order, only, costs)?;
            if query.has_target_srfs() {
                let c = node.cost;
                let rows = query.targets.iter().map(|t| set_rows(&t.expr)).fold(1.0, f64::max);
                let per_row = cost::qual_cost(query.targets.iter().map(|t| &t.expr));
                let out_rows = c.rows * rows;
                node = Node::new(Kind::ProjectSet, scope, vec![node]);
                node.cost = Cost {
                    startup: c.startup,
                    total: c.total
                        + (cost::CPU_TUPLE + per_row) * c.rows
                        + (out_rows - c.rows) * cost::CPU_TUPLE / 2.0,
                    rows: out_rows,
                    width: 0,
                };
            }
            if !query.distinct.is_empty() {
                node = distinct(level, node, &mut order, only, costs.last)?;
            }
        }
        let (count, offset) = limit_estimates(query);
        if !query.sort.is_empty() {
            let keys: Vec<SortKey<'q>> =
                query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
            let keys = level.stats.canonical(keys, |k| &*k.item.expr);
            let bound = (count > 0.0 && offset >= 0.0).then_some(count + offset);
            node = sort_if(level, node, keys, &mut order, bound, only)?;
        }
        if !query.row_marks.is_empty() {
            let c = node.cost;
            node = Node::new(Kind::LockRows, scope, vec![node]);
            node.cost = Cost { total: c.total + cost::CPU_TUPLE * c.rows, ..c };
        }
        if limit_needed(query) {
            let c = node.cost;
            node = Node::new(Kind::Limit, scope, vec![node]);
            node.cost = limit_cost(c, count, offset);
        }
        Some(node)
    }

    /// The `ModifyTable` node of a statement that writes a table, as `create_modifytable_path`.
    pub(crate) fn modify(
        &mut self,
        level: &Level<'q>,
        write: &'q Write,
        node: Node<'q>,
    ) -> Node<'q> {
        let operation = match write.kind {
            WriteKind::Insert => Operation::Insert,
            WriteKind::Update => Operation::Update,
            WriteKind::Delete => Operation::Delete,
            WriteKind::Merge => Operation::Merge,
        };
        let conflict = write.on_conflict.as_ref().map(|c| {
            let mut filter = Vec::new();
            let mut scope = None;
            if let Some(update) = &c.update {
                let update = crate::unwrap_checks(update);
                scope = Some(self.scopes.len());
                self.scopes.push(rupg_plan::Scope {
                    query: update,
                    parent: Some(level.scope),
                    target: false,
                    leaf: None,
                });
                let mut parts = Vec::new();
                if let Some(cond) = &update.filter {
                    exprs::and_parts(cond, &mut parts);
                }
                filter = parts.into_iter().map(Item::of).collect();
            }
            Conflict {
                update: c.update.is_some(),
                arbiters: c
                    .arbiters
                    .iter()
                    .filter_map(|i| write.unique.get(*i))
                    .map(|u| u.oid)
                    .collect(),
                filter,
                scope,
            }
        });
        let relation = if operation == Operation::Insert { TARGET } else { 0 };
        let c = node.cost;
        let mut out = Node::new(
            Kind::Modify { operation, table: write.table, relation, conflict, targets: Vec::new() },
            level.scope,
            vec![node],
        );
        out.cost = Cost { rows: 0.0, width: 0, ..c };
        out
    }

    /// The plan of a set operation, as `plan_set_operations`. The nodes above it show the expressions of the leftmost `SELECT`.
    pub(crate) fn set_op(&mut self, level: &mut Level<'q>, op: &'q SetOp) -> Node<'q> {
        let node = self.set_node(level, op);
        if let Some(scope) = self.leftmost_scope(level, op) {
            level.display = scope;
            level.display_query = self.scopes[scope].query;
        }
        node
    }

    /// The plan of a part of the tree of a set operation.
    fn set_tree(&mut self, level: &Level<'q>, tree: &'q SetTree) -> Node<'q> {
        match tree {
            SetTree::Leaf(i) => {
                match level.query.relations.get(*i).and_then(|r| r.subquery.as_deref()) {
                    Some(sub) => self.query(sub, Some(level.scope)),
                    None => Node::new(
                        Kind::Result { replaces: None, one_time: Vec::new() },
                        level.scope,
                        Vec::new(),
                    ),
                }
            }
            SetTree::Op(op) => self.set_node(level, op),
        }
    }

    /// The members of a `UNION`, with the members of each `UNION` below it that PostgreSQL makes one `Append` with, as `recurse_union_children`.
    fn union_members(
        &mut self,
        level: &Level<'q>,
        top: &'q SetOp,
        tree: &'q SetTree,
        out: &mut Vec<Node<'q>>,
    ) {
        if let SetTree::Op(op) = tree
            && op.kind == SetKind::Union
            && op.work.is_none()
            && (op.all == top.all || op.all)
            && op.columns == top.columns
        {
            self.union_members(level, top, &op.left, out);
            self.union_members(level, top, &op.right, out);
            return;
        }
        out.push(self.set_tree(level, tree));
    }

    fn set_node(&mut self, level: &Level<'q>, op: &'q SetOp) -> Node<'q> {
        let scope = level.scope;
        if let Some(work) = op.work {
            let left = self.set_tree(level, &op.left);
            self.work.push((work, left.cost.rows));
            let right = self.set_tree(level, &op.right);
            self.work.pop();
            let (l, r) = (left.cost, right.cost);
            let rows = l.rows + 10.0 * r.rows;
            let mut node = Node::new(Kind::RecursiveUnion, scope, vec![left, right]);
            node.cost = Cost {
                startup: l.startup,
                total: l.total + 10.0 * r.total + cost::CPU_TUPLE * rows,
                rows,
                width: l.width.max(r.width),
            };
            return node;
        }
        let columns = op.columns.len();
        match op.kind {
            SetKind::Union => {
                let mut members = Vec::new();
                self.union_members(level, op, &op.left, &mut members);
                self.union_members(level, op, &op.right, &mut members);
                let startup = members.first().map_or(0.0, |m| m.cost.startup);
                let width = members.first().map_or(0, |m| m.cost.width);
                let rows: f64 = members.iter().map(|m| m.cost.rows).sum();
                let total: f64 = members.iter().map(|m| m.cost.total).sum::<f64>()
                    + 0.5 * cost::CPU_TUPLE * rows;
                let mut append = Node::new(Kind::Append, scope, members);
                append.cost = Cost { startup, total, rows, width };
                if op.all {
                    return append;
                }
                let c = append.cost;
                let hashable = op.groups.iter().all(|g| g.hashable);
                let hash_startup = c.total + cost::CPU_OPERATOR * c.rows * columns as f64;
                let hash_total = hash_startup + cost::CPU_TUPLE * rows;
                let (sort_startup, sort_total) =
                    cost::sort((c.startup, c.total), c.rows, c.width, None);
                let unique_total = sort_total + cost::CPU_OPERATOR * c.rows * columns as f64;
                let keys = self.leaf_items(level, op);
                // The keys show with the names of the leftmost `SELECT`.
                let scope = self.leftmost_scope(level, op).unwrap_or(scope);
                if hashable && hash_total <= unique_total {
                    let mut node = Node::new(
                        Kind::Aggregate { strategy: Strategy::Hashed, keys, sets: Vec::new() },
                        scope,
                        vec![append],
                    );
                    node.cost = Cost { startup: hash_startup, total: hash_total, rows, width };
                    node
                } else {
                    let sort_keys = keys
                        .into_iter()
                        .zip(&op.groups)
                        .map(|(item, g)| SortKey {
                            item,
                            desc: false,
                            using: None,
                            nulls_first: g.nulls_first,
                        })
                        .collect();
                    let mut sorted = Node::new(Kind::Sort(sort_keys), scope, vec![append]);
                    sorted.cost = Cost { startup: sort_startup, total: sort_total, ..c };
                    let mut node = Node::new(Kind::Unique, scope, vec![sorted]);
                    node.cost = Cost { startup: sort_startup, total: unique_total, rows, width };
                    node
                }
            }
            SetKind::Intersect | SetKind::Except => {
                let left = self.set_tree(level, &op.left);
                let right = self.set_tree(level, &op.right);
                let (l, r) = (left.cost, right.cost);
                let left_groups = cost::groups(l.rows, columns);
                let right_groups = cost::groups(r.rows, columns);
                let rows = if op.kind == SetKind::Intersect {
                    left_groups.min(right_groups)
                } else {
                    left_groups
                };
                let startup =
                    l.total + r.total + cost::CPU_OPERATOR * (l.rows + r.rows) * columns as f64;
                let mut node = Node::new(
                    Kind::SetOp { except: op.kind == SetKind::Except, all: op.all },
                    scope,
                    vec![left, right],
                );
                node.cost = Cost {
                    startup,
                    total: startup + cost::CPU_TUPLE * left_groups,
                    rows,
                    width: l.width,
                };
                node
            }
        }
    }

    /// The scope of the leftmost `SELECT` of a set operation.
    fn leftmost_scope(&self, level: &Level<'q>, op: &'q SetOp) -> Option<usize> {
        let sub = level.query.relations.get(op.left.leftmost())?.subquery.as_deref()?;
        self.scopes.iter().position(|s| std::ptr::eq(s.query, sub))
    }

    /// The columns of the leftmost `SELECT` of a set operation, as the keys of the node that finds the equal rows.
    fn leaf_items(&self, level: &Level<'q>, op: &'q SetOp) -> Vec<Item<'q>> {
        let leftmost = op.left.leftmost();
        match level.query.relations.get(leftmost).and_then(|r| r.subquery.as_deref()) {
            Some(sub) => {
                sub.targets.iter().filter(|t| !t.junk).map(|t| Item::below(&t.expr)).collect()
            }
            None => Vec::new(),
        }
    }
}

/// The ordered aggregate call whose keys the input of the `Agg` gets sorted on, as the presorted aggregates of `make_pathkeys_for_groupagg`: the first call with `ORDER BY` or `DISTINCT`.
fn ordered_keys<'q>(aggs: &[&'q Aggref]) -> Vec<SortKey<'q>> {
    let Some(agg) = aggs.iter().find(|a| !a.order.is_empty() || !a.distinct.is_empty()) else {
        return Vec::new();
    };
    let items = if agg.distinct.is_empty() { &agg.order } else { &agg.distinct };
    items.iter().filter_map(|s| agg.args.get(s.target).map(|e| sort_key(e, s))).collect()
}

/// The `Agg` node of a query with groups, with a `Sort` below it when the strategy needs one, as `add_paths_to_grouping_rel`. With `only`, the input is not the cheapest plan, so it gets only the sorted strategy, and only when its rows are in the order of the groups.
fn aggregate<'q>(
    level: &Level<'q>,
    input: Node<'q>,
    order: &mut Vec<OrderKey>,
    only: bool,
) -> Option<Node<'q>> {
    let query = level.query;
    let scope = level.scope;
    let mut exprs_of: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    let (_, having) = having_split(query);
    exprs_of.extend(having.iter().copied());
    let mut aggs = Vec::new();
    exprs::aggregates(&exprs_of, &mut aggs);
    let units = agg_units(&aggs);
    let finals = cost::CPU_OPERATOR * final_units(&aggs);
    let ordered = ordered_keys(&aggs);
    let selectivity = cost::list_selectivity(having.iter().copied());
    let (having_startup, having_cost) = level.stats.qual_costs(having.iter().copied());
    let filter: Vec<Item<'q>> = having.iter().map(|h| Item::of(h)).collect();
    let c = input.cost;
    let n = c.rows;
    if !query.grouping_sets.is_empty() {
        if only {
            return None;
        }
        return Some(grouping_sets(level, input, order, units, filter, selectivity));
    }
    if query.group.is_empty() {
        let ordered = level.stats.canonical(ordered, |k| &*k.item.expr);
        let input = sort_if(level, input, ordered, order, None, only)?;
        let c = input.cost;
        // `cost_agg` adds the costs in this order, which the rounding of the sums follows.
        let startup = c.total + cost::CPU_OPERATOR * units * n + finals;
        let total = startup + cost::CPU_TUPLE + having_startup + having_cost;
        let mut node = Node::new(
            Kind::Aggregate { strategy: Strategy::Plain, keys: Vec::new(), sets: Vec::new() },
            scope,
            vec![input],
        );
        node.filter = filter;
        let startup = startup + having_startup;
        node.cost = Cost { startup, total, rows: cost::clamp_rows(selectivity), width: 0 };
        order.clear();
        return Some(node);
    }
    let (group, hashable) = group_keys(level);
    let nkeys = group.len() as f64;
    let group_exprs: Vec<&Expr> = group.iter().map(|k| &*k.item.expr).collect();
    let groups = level.stats.groups(&group_exprs, n);
    let keys: Vec<Item<'q>> = group.iter().map(|k| k.item.clone()).collect();
    let rows = cost::clamp_rows(groups * selectivity);
    let hashable = hashable && !group.is_empty() && ordered.is_empty();
    // The sorted strategy: a `Sort` on the keys of the groups, and on the keys of an ordered call after them, as `adjust_group_pathkeys_for_groupagg`.
    let mut sort_keys = group.clone();
    sort_keys.extend(ordered);
    let sort_keys = level.stats.canonical(sort_keys, |k| &*k.item.expr);
    let (is_sorted, count) = presorted(order, &sort_keys);
    if only && !is_sorted && count == 0 {
        return None;
    }
    let (sorted_input, sorted_order) = if is_sorted {
        (c, order.clone())
    } else {
        let (startup, total) = sort_cost(level, c, &sort_keys, count, None);
        (Cost { startup, total, ..c }, sorted_order(&sort_keys))
    };
    // `cost_agg`: the transition cost, then the cost of the keys, which the rounding of the sums follows.
    let sorted_total = sorted_input.total
        + cost::CPU_OPERATOR * units * n
        + cost::CPU_OPERATOR * nkeys * n
        + finals * groups
        + cost::CPU_TUPLE * groups
        + having_startup
        + having_cost * groups;
    let hash_startup = c.total + cost::CPU_OPERATOR * units * n + cost::CPU_OPERATOR * nkeys * n;
    let hash_total = hash_startup
        + finals * groups
        + cost::CPU_TUPLE * groups
        + having_startup
        + having_cost * groups;
    // The hash table loses the order of the groups, so the sort of `ORDER BY` counts for it when the groups have the order of `ORDER BY`.
    let in_order = query.sort.iter().all(|s| query.group.iter().any(|g| g.target == s.target));
    let hash_after = if query.sort.is_empty() || !in_order {
        hash_total
    } else {
        cost::sort((hash_startup, hash_total), rows, c.width, None).1
    };
    // `add_path` keeps the hash table only when its total cost or its start cost is lower by more than the fuzz factor, as the sorted groups have the order of their keys.
    let kept = hash_total * 1.01 < sorted_total
        || hash_total <= sorted_total * 1.01
            && (hash_startup + having_startup) * 1.01 < sorted_input.startup + having_startup;
    if !only && hashable && kept && hash_after < sorted_total {
        let mut node = Node::new(
            Kind::Aggregate { strategy: Strategy::Hashed, keys, sets: Vec::new() },
            scope,
            vec![input],
        );
        node.filter = filter;
        let startup = hash_startup + having_startup;
        node.cost = Cost { startup, total: hash_total, rows, width: 0 };
        order.clear();
        return Some(node);
    }
    let Some(input) = sort_if(level, input, sort_keys, order, None, false) else {
        unreachable!("a sort has a result")
    };
    *order = sorted_order;
    order.truncate(group.len());
    let mut node = Node::new(
        Kind::Aggregate { strategy: Strategy::Sorted, keys, sets: Vec::new() },
        scope,
        vec![input],
    );
    node.filter = filter;
    let startup = sorted_input.startup + having_startup;
    node.cost = Cost { startup, total: sorted_total, rows, width: 0 };
    Some(node)
}

/// The keys of `GROUP BY` as the keys of a sort, in the order of `ORDER BY` when its items are items of `GROUP BY`, as `preprocess_groupclause`, and true when a hash table can hold the groups. A column that the other columns of its relation give, as `remove_useless_groupby_columns` finds it, and a key that is redundant, as `make_pathkeys_for_sortclauses_extended` finds it, are not keys.
fn group_keys<'q>(level: &Level<'q>) -> (Vec<SortKey<'q>>, bool) {
    let query = level.query;
    let mut items: Vec<&SortGroup> = Vec::new();
    let in_group = |s: &SortGroup| query.group.iter().find(|g| g.target == s.target);
    if query.sort.iter().all(|s| in_group(s).is_some_and(|g| g.equal == s.equal)) {
        items.extend(query.sort.iter());
    }
    for g in &query.group {
        if !items.iter().any(|i| i.target == g.target) {
            items.push(g);
        }
    }
    let exprs: Vec<&Expr> = items.iter().map(|s| level.target(s.target)).collect();
    let surplus = level.stats.surplus(&exprs);
    let items: Vec<&SortGroup> =
        items.into_iter().zip(surplus).filter(|(_, surplus)| !surplus).map(|(s, _)| s).collect();
    let items = level.stats.canonical(items, |s| level.target(s.target));
    let hashable = items.iter().all(|s| in_group(s).is_some_and(|g| g.hashable));
    (items.into_iter().map(|s| sort_key(level.target(s.target), s)).collect(), hashable)
}

/// The `Agg` of a query with grouping sets: `HashAggregate` when each set can use a hash table, `MixedAggregate` when one of them is `()`, or else `GroupAggregate` on the sorted rows.
fn grouping_sets<'q>(
    level: &Level<'q>,
    input: Node<'q>,
    order: &mut Vec<OrderKey>,
    units: f64,
    filter: Vec<Item<'q>>,
    selectivity: f64,
) -> Node<'q> {
    let query = level.query;
    let scope = level.scope;
    let sets = expand_grouping_sets(&query.grouping_sets, query.group_distinct, MAX_SETS)
        .unwrap_or_default();
    let group_of = |t: usize| query.group.iter().find(|g| g.target == t);
    let hashable = |set: &Vec<usize>| set.iter().all(|t| group_of(*t).is_some_and(|g| g.hashable));
    let all_hashable = sets.iter().filter(|s| !s.is_empty()).all(hashable);
    let has_empty = sets.iter().any(Vec::is_empty);
    let strategy = if !all_hashable {
        Strategy::Sorted
    } else if has_empty {
        Strategy::Mixed
    } else {
        Strategy::Hashed
    };
    let c = input.cost;
    let n = c.rows;
    // The sets with the most items come first, as `preprocess_grouping_sets` sorts them.
    let mut ordered: Vec<&Vec<usize>> = sets.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut groups = 0.0;
    let mut units_all = 0.0;
    let mut out_sets = Vec::new();
    let mut keys: Vec<Item<'q>> = Vec::new();
    for set in &ordered {
        let set_exprs: Vec<&Expr> = set.iter().map(|t| level.target(*t)).collect();
        groups += level.stats.groups(&set_exprs, n);
        units_all += set.len() as f64 + units;
        let items: Vec<Item<'q>> = set.iter().map(|t| Item::below(level.target(*t))).collect();
        for item in &items {
            if !keys.iter().any(|k| k.expr.same(&item.expr)) {
                keys.push(item.clone());
            }
        }
        out_sets.push((strategy != Strategy::Sorted && !set.is_empty(), items));
    }
    let mut input = input;
    let mut startup = c.total;
    if strategy == Strategy::Sorted {
        let first = ordered.first().map(|s| s.as_slice()).unwrap_or_default();
        let sort_keys: Vec<SortKey<'q>> = first
            .iter()
            .filter_map(|t| group_of(*t).map(|g| sort_key(level.target(*t), g)))
            .collect();
        if let Some(sorted) = sort_if(level, input, sort_keys, order, None, false) {
            input = sorted;
        } else {
            unreachable!("a sort of the cheapest plan has a result")
        }
        startup = input.cost.startup;
    }
    let total = input.cost.total.max(c.total)
        + cost::CPU_OPERATOR * n * units_all
        + cost::CPU_TUPLE * groups;
    if strategy != Strategy::Sorted {
        startup += cost::CPU_OPERATOR * n * units_all;
    }
    order.clear();
    let mut node =
        Node::new(Kind::Aggregate { strategy, keys, sets: out_sets }, scope, vec![input]);
    node.filter = filter;
    node.cost = Cost {
        startup,
        total: total.max(startup),
        rows: cost::clamp_rows(groups * selectivity),
        width: 0,
    };
    node
}

/// The window calls of the query for each window, by `winref`.
fn window_calls(query: &Query) -> Vec<Vec<&WindowFunc>> {
    let mut out = vec![Vec::new(); query.windows.len()];
    for target in &query.targets {
        let mut calls = Vec::new();
        exprs::window_calls(&target.expr, &mut calls);
        for call in calls {
            if let Some(list) = out.get_mut(call.winref.wrapping_sub(1)) {
                list.push(call);
            }
        }
    }
    out
}

/// The places of the items of `PARTITION BY` of a window whose keys are not redundant, as `make_pathkeys_for_window` keeps them.
fn window_partition(level: &Level<'_>, window: usize) -> Vec<usize> {
    let clause = &level.query.windows[window];
    let items = clause.partition.iter().enumerate().collect();
    let items = level.stats.canonical(items, |(_, item)| level.target(item.target));
    items.into_iter().map(|(i, _)| i).collect()
}

/// The keys of the sort of a window: the items of `PARTITION BY` at the places `partition`, then the items of `ORDER BY`, without the keys that are redundant, as `make_pathkeys_for_window`.
fn window_keys<'q>(level: &Level<'q>, window: usize, partition: &[usize]) -> Vec<SortKey<'q>> {
    let clause = &level.query.windows[window];
    let items = partition.iter().map(|&i| &clause.partition[i]).chain(&clause.order);
    let keys = items.map(|item| sort_key(level.target(item.target), item)).collect();
    level.stats.canonical(keys, |k| &*k.item.expr)
}

/// `get_windowclause_startup_tuples`: the rows that a window reads before its first row, for the options of its frame after `optimize_window_clauses`.
fn startup_tuples(
    level: &Level<'_>,
    window: usize,
    partition: &[usize],
    rows: f64,
    options: i32,
) -> f64 {
    let clause = &level.query.windows[window];
    let partition_rows = if partition.is_empty() {
        rows
    } else {
        let exprs: Vec<&Expr> =
            partition.iter().map(|&i| level.target(clause.partition[i].target)).collect();
        rows / level.stats.groups(&exprs, rows)
    };
    let peer_rows = if clause.order.is_empty() {
        1.0
    } else {
        let exprs: Vec<&Expr> = clause.order.iter().map(|i| level.target(i.target)).collect();
        partition_rows / level.stats.groups(&exprs, partition_rows)
    };
    let out = if options & FRAME_END_UNBOUNDED_FOLLOWING != 0 {
        partition_rows
    } else if options & FRAME_END_CURRENT_ROW != 0 {
        if options & FRAME_ROWS != 0 {
            1.0
        } else if clause.order.is_empty() {
            partition_rows
        } else {
            peer_rows
        }
    } else if options & FRAME_END_OFFSET_FOLLOWING != 0 {
        let offset = match clause.frame.end.as_ref().and_then(const_count) {
            Some(Some(v)) => v,
            Some(None) => 1.0,
            None => partition_rows / peer_rows * cost::INEQ_SEL,
        };
        if options & FRAME_ROWS != 0 { offset + 1.0 } else { peer_rows * (offset + 1.0) }
    } else {
        1.0
    };
    // The window reads one more row to know that the row before it is the last of its frame.
    let out = if partition.is_empty() && clause.order.is_empty() {
        out.min(partition_rows)
    } else {
        (out + 1.0).min(partition_rows)
    };
    cost::clamp_rows(out)
}

/// `tleSortGroupRef`: the number of each target that an item of `ORDER BY`, `GROUP BY`, `DISTINCT` or a window names, in the order that the analyzer gives the numbers.
fn sort_refs(query: &Query) -> Vec<u32> {
    let mut refs = vec![0; query.targets.len()];
    let mut next = 0;
    // `transformWindowDefinitions` takes the items of `ORDER BY` of a window before its items of `PARTITION BY`.
    let windows = query.windows.iter().flat_map(|w| w.order.iter().chain(&w.partition));
    for item in query.sort.iter().chain(&query.group).chain(&query.distinct).chain(windows) {
        if let Some(slot) = refs.get_mut(item.target)
            && *slot == 0
        {
            next += 1;
            *slot = next;
        }
    }
    refs
}

/// `select_active_windows`: the windows that a call uses, in the order of their `WindowAgg` nodes from the bottom up. `common_prefix_cmp` puts the items with the higher numbers first, so that the windows that share a prefix of their keys come next to each other, and a window with more keys comes before a window with a prefix of its keys.
fn active_windows(level: &Level<'_>, calls: &[Vec<&WindowFunc>]) -> Vec<usize> {
    let query = level.query;
    let refs = sort_refs(query);
    let unique = |w: usize| {
        let clause = &query.windows[w];
        let mut out: Vec<&SortGroup> = Vec::new();
        for item in clause.partition.iter().chain(&clause.order) {
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out
    };
    let mut active: Vec<(usize, Vec<&SortGroup>)> =
        (0..calls.len()).filter(|w| !calls[*w].is_empty()).map(|w| (w, unique(w))).collect();
    active.sort_by(|(_, a), (_, b)| {
        for (x, y) in a.iter().zip(b) {
            let rx = refs.get(x.target).copied().unwrap_or(0);
            let ry = refs.get(y.target).copied().unwrap_or(0);
            let found = ry
                .cmp(&rx)
                .then(y.operator.cmp(&x.operator))
                .then(y.nulls_first.cmp(&x.nulls_first));
            if found.is_ne() {
                return found;
            }
        }
        b.len().cmp(&a.len())
    });
    active.into_iter().map(|(w, _)| w).collect()
}

/// The `WindowAgg` nodes of the query, each with a `Sort` below it when the rows are not in the order of its window, in the order of `select_active_windows`. A window with no name takes the name `w1`, `w2` and so on in that order, as `name_active_windows`, with a number that no named window of the query uses. With `only`, the result is `None` when a window needs a sort.
fn windows<'q>(
    level: &Level<'q>,
    mut node: Node<'q>,
    order: &mut Vec<OrderKey>,
    only: bool,
    targets: TargetCosts,
) -> Option<Node<'q>> {
    let calls = window_calls(level.query);
    let active = active_windows(level, &calls);
    let mut number = 0;
    let count = active.len();
    for (at, window) in active.into_iter().enumerate() {
        let clause = &level.query.windows[window];
        let partition = window_partition(level, window);
        node = sort_if(level, node, window_keys(level, window, &partition), order, None, only)?;
        let c = node.cost;
        let mut total = c.total;
        for call in &calls[window] {
            let args: usize = call.args.iter().map(exprs::calls).sum();
            let filter = call.filter.as_deref().map_or(0, exprs::calls);
            total += cost::CPU_OPERATOR * (1 + args + filter) as f64 * c.rows;
        }
        let nkeys = (partition.len() + clause.order.len()) as f64;
        total += cost::CPU_OPERATOR * nkeys * c.rows + cost::CPU_TUPLE * c.rows;
        let rows_frame = clause.frame.options == FRAME_DEFAULTS
            && calls[window].iter().all(|f| {
                builtin::proc_by_oid(f.oid).is_some_and(|p| {
                    matches!(
                        p.name,
                        "row_number"
                            | "rank"
                            | "dense_rank"
                            | "percent_rank"
                            | "ntile"
                            | "cume_dist"
                    )
                })
            });
        // `optimize_window_clauses`: the support functions of these calls give the frame `ROWS UNBOUNDED PRECEDING`.
        let options = if rows_frame { FRAME_ROWS_CURRENT } else { clause.frame.options };
        let mut startup = c.startup;
        let tuples = startup_tuples(level, window, &partition, c.rows, options);
        if tuples > 1.0 && c.rows > 0.0 {
            startup += (total - startup) / c.rows * (tuples - 1.0);
        }
        let name = clause.name.clone().unwrap_or_else(|| {
            loop {
                number += 1;
                let name = format!("w{number}");
                if !level.query.windows.iter().any(|w| w.name.as_deref() == Some(name.as_str())) {
                    break name;
                }
            }
        });
        let kind = Kind::WindowAgg { name, window, rows_frame, partition };
        node = Node::new(kind, level.scope, vec![node]);
        node.cost = Cost { startup, total, ..c };
        // `create_windowagg_path` adds the cost of the targets, which are the targets before the sort at the last window.
        let (target_startup, per_row) = if at + 1 == count { targets.last } else { targets.window };
        project(&mut node, target_startup, per_row);
    }
    Some(node)
}

/// The node of `DISTINCT`: a `HashAggregate` on the targets, or a `Unique` on the rows sorted on them, whichever costs less, as `create_distinct_paths`. `DISTINCT ON` always sorts. With `only`, the input is not the cheapest plan, so it gets only a `Unique`, and only when its rows are in the order of the keys.
fn distinct<'q>(
    level: &Level<'q>,
    input: Node<'q>,
    order: &mut Vec<OrderKey>,
    only: bool,
    target: (f64, f64),
) -> Option<Node<'q>> {
    let query = level.query;
    let scope = level.scope;
    let c = input.cost;
    // `processed_distinctClause`: the items whose keys are not redundant.
    let processed =
        level.stats.canonical(query.distinct.iter().collect(), |d| level.target(d.target));
    let ncols = processed.len();
    let distinct_exprs: Vec<&Expr> = processed.iter().map(|d| level.target(d.target)).collect();
    let groups = level.stats.groups(&distinct_exprs, c.rows);
    // The sort puts the items of `ORDER BY` first, so that the sort of `ORDER BY` after it has no work.
    let mut keys: Vec<SortKey<'q>> = Vec::new();
    let distinct_target = |t: usize| query.distinct.iter().any(|d| d.target == t);
    if query.sort.iter().all(|s| distinct_target(s.target)) {
        keys.extend(query.sort.iter().map(|s| sort_key(level.target(s.target), s)));
    }
    for d in &query.distinct {
        let key = sort_key(level.target(d.target), d);
        if !keys.iter().any(|k| k.item.expr.same(&key.item.expr)) {
            keys.push(key);
        }
    }
    let mut keys = level.stats.canonical(keys, |k| &*k.item.expr);
    // `DISTINCT ON` sorts on all the items of `ORDER BY` when it has more keys, as `create_final_distinct_paths`.
    if query.distinct_on {
        let sort: Vec<SortKey<'q>> =
            query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
        let sort = level.stats.canonical(sort, |k| &*k.item.expr);
        if sort.len() > keys.len() {
            keys = sort;
        }
    }
    // With no keys that are not redundant, each row has the values of the other rows, so the first row is the result, as the `LIMIT 1` of `create_final_distinct_paths`.
    if processed.is_empty() {
        let input = sort_if(level, input, keys, order, Some(1.0), only)?;
        let c = input.cost;
        let mut node = Node::new(Kind::Limit, scope, vec![input]);
        node.cost = limit_cost(c, 1.0, 0.0);
        return Some(node);
    }
    let (is_sorted, count) = presorted(order, &keys);
    if only && !is_sorted && count == 0 {
        return None;
    }
    let (sort_startup, sort_total) =
        if is_sorted { (c.startup, c.total) } else { sort_cost(level, c, &keys, count, None) };
    let unique_total = sort_total + cost::CPU_OPERATOR * c.rows * ncols as f64;
    let hashable = !query.distinct_on && processed.iter().all(|d| d.hashable);
    // `create_agg_path` adds the cost of the targets to the hash table.
    let hash_startup = c.total + cost::CPU_OPERATOR * c.rows * ncols as f64;
    let hash_total = hash_startup + cost::CPU_TUPLE * groups + target.0 + target.1 * groups;
    let hash_startup = hash_startup + target.0;
    // The hash table loses its order, so the sort of `ORDER BY` counts for it.
    let hash_after = if query.sort.is_empty() {
        hash_total
    } else {
        cost::sort((hash_startup, hash_total), groups, c.width, None).1
    };
    // `add_path` keeps the hash table only when its total cost or its start cost is lower by more than the fuzz factor, as the `Unique` has the order of its keys.
    let kept = hash_total * 1.01 < unique_total
        || hash_total <= unique_total * 1.01 && hash_startup * 1.01 < sort_startup;
    if !only && hashable && kept && hash_after < unique_total {
        let items = processed.iter().map(|d| Item::below(level.target(d.target))).collect();
        let mut node = Node::new(
            Kind::Aggregate { strategy: Strategy::Hashed, keys: items, sets: Vec::new() },
            scope,
            vec![input],
        );
        node.cost = Cost { startup: hash_startup, total: hash_total, rows: groups, width: 0 };
        order.clear();
        return Some(node);
    }
    let input = sort_if(level, input, keys, order, None, only)?;
    let mut node = Node::new(Kind::Unique, scope, vec![input]);
    node.cost = Cost { startup: sort_startup, total: unique_total, rows: groups, ..c };
    Some(node)
}

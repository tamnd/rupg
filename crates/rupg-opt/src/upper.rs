//! The nodes above the join of `FROM`, as `grouping_planner` makes them: the aggregates, the windows, the functions that give sets, `DISTINCT`, `ORDER BY`, the locks of rows and `LIMIT`. Also the plan of a set operation and the node of a statement that writes a table.

use std::borrow::Cow;
use std::cmp::Ordering;

use rupg_analyze::{
    Aggref, Expr, ExprKind, MinMax, Query, SetKind, SetOp, SetTree, SortGroup, SubLink, WindowFunc,
    Write, WriteKind, expand_grouping_sets,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{
    Conflict, Cost, Item, Kind, Label, Node, Operation, Replaced, Rollup, SortKey, Strategy,
    SubPlan, TARGET,
};

use crate::from::paths::{Paths, costs};
use crate::from::{self, OrderKey, sort};
use crate::{Builder, Level, cost, exprs, setop};

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

/// The costs of a sort of rows with these costs whose first `presorted` keys are in order already: a `Sort` when no key is in order or `enable_incremental_sort` is off, or else an `Incremental Sort`, as `make_ordered_path` chooses.
fn sort_cost(
    level: &Level<'_>,
    c: Cost,
    keys: &[SortKey<'_>],
    presorted: usize,
    bound: Option<f64>,
) -> (f64, f64) {
    if presorted == 0 || !crate::enable().incremental_sort {
        cost::sort((c.startup, c.total), c.rows, c.width, bound)
    } else {
        let exprs: Vec<&Expr> = keys[..presorted].iter().map(|k| &*k.item.expr).collect();
        let groups = level.stats.groups(&exprs, c.rows.max(2.0));
        cost::incremental_sort((c.startup, c.total), c.rows, c.width, groups, bound)
    }
}

/// The subqueries in the expressions that are not in a subquery, in the order of the expressions.
fn sublinks<'q>(exprs: impl IntoIterator<Item = &'q Expr>) -> Vec<&'q SubLink> {
    let mut out: Vec<&'q SubLink> = Vec::new();
    for expr in exprs {
        expr.find(0, &mut |e, depth| {
            if let ExprKind::SubLink(link) = &e.kind
                && depth == 0
            {
                out.push(link);
            }
            None::<()>
        });
    }
    out
}

/// True when the sort that a node needs to have its rows in the order of the keys is a disabled node: a `Sort` with `enable_sort` off. A node whose rows are in the order of `count` keys gets an `Incremental Sort`, which is not a disabled node.
fn sort_disabled(is_sorted: bool, count: usize) -> bool {
    let enable = crate::enable();
    !is_sorted && (count == 0 || !enable.incremental_sort) && !enable.sort
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
    // The keys of a set operation show with the names of its leftmost `SELECT`.
    let scope = if level.query.set_op.is_some() { level.display } else { node.scope };
    if count == 0 || !crate::enable().incremental_sort {
        let mut out = sort(node, keys, bound);
        out.scope = scope;
        return Some(out);
    }
    let c = node.cost;
    let (startup, total) = sort_cost(level, c, &keys, count, bound);
    let mut out = Node::new(Kind::IncrementalSort { keys, presorted: count }, scope, vec![node]);
    out.cost = Cost { startup, total, ..c };
    Some(out)
}

/// The steps of `DISTINCT`, `ORDER BY`, row locks and `LIMIT` above the plan of the groups of a query, as the end of `grouping_planner` adds them.
fn steps<'q>(
    level: &Level<'q>,
    mut node: Node<'q>,
    order: &mut Vec<OrderKey>,
    only: bool,
    costs: TargetCosts,
) -> Option<Node<'q>> {
    let query = level.query;
    let scope = level.display;
    if query.set_op.is_none() && !query.distinct.is_empty() {
        node = distinct(level, node, order, only, costs.last)?;
    }
    let (count, offset) = limit_estimates(query);
    if !query.sort.is_empty() {
        let keys: Vec<SortKey<'q>> =
            query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
        let keys = level.stats.canonical(keys, |k| &*k.item.expr);
        let bound = (count > 0.0 && offset >= 0.0).then_some(count + offset);
        node = sort_if(level, node, keys, order, bound, only)?;
        if costs.late {
            node = project_late(level, node, costs);
        }
    }
    if !query.row_marks.is_empty() {
        let c = node.cost;
        node = Node::new(Kind::LockRows, scope, vec![node]);
        node.cost = Cost { total: c.total + cost::CPU_TUPLE * c.rows, ..c };
        // `create_lockrows_path`: a lock can fetch a new version of a row, so the rows have no order.
        order.clear();
    }
    if limit_needed(query) {
        let c = node.cost;
        node = Node::new(Kind::Limit, scope, vec![node]);
        node.cost = limit_cost(c, count, offset);
    }
    Some(node)
}

/// `set_plan_references`: the keys of the nodes above the `Result` of `create_minmaxagg_path` read the subqueries in the place of the aggregate calls. The planner finds the keys with the aggregate calls, as `standard_qp_callback` makes the pathkeys before `preprocess_minmax_aggregates`, so that a key is not a constant.
fn upper_keys<'q>(node: &mut Node<'q>, query: &'q Query, upper: &'q Query) {
    let swap = |item: &mut Item<'q>| {
        if let Some(at) = query.targets.iter().position(|t| t.expr.same(&item.expr)) {
            item.expr = Cow::Borrowed(&upper.targets[at].expr);
        }
    };
    match &mut node.kind {
        Kind::Result { replaces: Some(Replaced::MinMax), .. } => return,
        Kind::Sort(keys) | Kind::IncrementalSort { keys, .. } => {
            keys.iter_mut().for_each(|k| swap(&mut k.item));
        }
        Kind::Aggregate { keys, .. } => keys.iter_mut().for_each(swap),
        _ => {}
    }
    if let [child] = &mut node.children[..] {
        upper_keys(child, query, upper);
    }
}

/// The cost of the transition of the aggregate calls for each row, in units of `cpu_operator_cost`, as `get_agg_clause_costs`: a call of the transition function and the cost of the arguments and of `FILTER`.
fn agg_units(aggs: &[&Aggref]) -> f64 {
    aggs.iter()
        .map(|a| {
            let args: f64 = a.args.iter().map(exprs::calls).sum();
            let filter = a.filter.as_deref().map_or(0.0, exprs::calls);
            1.0 + args + filter
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
    /// All the targets.
    full: (f64, f64),
    /// True when a target comes after the sort, as [`postponed`] finds it.
    late: bool,
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
    let costs: Vec<(f64, f64)> =
        targets.iter().map(|t| level.stats.qual_costs([&t.expr])).collect();
    let late = postponed(level);
    let sum = |keep: &dyn Fn(usize) -> bool| {
        costs
            .iter()
            .enumerate()
            .filter(|&(at, _)| keep(at))
            .fold((0.0, 0.0), |out, (_, c)| (out.0 + c.0, out.1 + c.1))
    };
    let last = sum(&|at| !late.get(at).copied().unwrap_or(false));
    let full = sum(&|_| true);
    let window = sum(&|at| window_keys.contains(&at));
    let group = if windows.is_empty() { last } else { window };
    let scan = if query.grouped {
        sum(&|at| group_keys.contains(&at))
    } else if windows.is_empty() {
        last
    } else {
        window
    };
    Some(TargetCosts { scan, group, window, last, full, late: late.contains(&true) })
}

/// `make_sort_input_target`: for each target of the query, true when it comes after the sort, or an empty list when none does. With `ORDER BY`, a target that is not a key of the query and calls a volatile function, or costs more than 10 times `cpu_operator_cost`, can come after the sort. They all do when one of them is volatile, or when the query has `LIMIT` or reads a part of its rows.
pub(crate) fn postponed(level: &Level<'_>) -> Vec<bool> {
    let query = level.query;
    if query.sort.is_empty()
        || level.no_targets
        || query.set_op.is_some()
        || query.write.is_some()
        || query.has_target_srfs()
    {
        return Vec::new();
    }
    let calls = window_calls(query);
    let keys: Vec<usize> = (0..calls.len())
        .filter(|&w| !calls[w].is_empty())
        .flat_map(|w| query.windows[w].partition.iter().chain(&query.windows[w].order))
        .chain(&query.sort)
        .chain(&query.distinct)
        .chain(&query.group)
        .map(|s| s.target)
        .collect();
    let late: Vec<(bool, bool)> = query
        .targets
        .iter()
        .enumerate()
        .map(|(at, t)| {
            let can = !keys.contains(&at);
            let cost = level.stats.qual_costs([&t.expr]).1;
            (can && exprs::volatile(&t.expr), can && cost > 10.0 * cost::CPU_OPERATOR)
        })
        .collect();
    let postpone = late.iter().any(|l| l.0)
        || late.iter().any(|l| l.1) && (query.limit.is_some() || level.fraction > 0.0);
    if !postpone {
        return Vec::new();
    }
    late.into_iter().map(|(volatile, expensive)| volatile || expensive).collect()
}

/// The node above the sort that computes the targets that come after it, as `apply_projection_to_path` in `create_ordered_paths`. A node that can compute its targets adds their costs, and a `Result` above the other nodes costs the targets and `cpu_tuple_cost` for each row, as `create_projection_path`.
fn project_late<'q>(level: &Level<'q>, mut node: Node<'q>, targets: TargetCosts) -> Node<'q> {
    let (startup, per_row) = targets.full;
    if !matches!(
        node.kind,
        Kind::Sort(_)
            | Kind::IncrementalSort { .. }
            | Kind::Unique
            | Kind::Limit
            | Kind::LockRows
            | Kind::Materialize
            | Kind::Memoize(_)
            | Kind::Hash
            | Kind::Append
            | Kind::MergeAppend(_)
            | Kind::SetOp { .. }
            | Kind::RecursiveUnion
            | Kind::ProjectSet
            | Kind::Modify { .. }
    ) {
        let (last_startup, last_per_row) = targets.last;
        node.cost.startup += startup - last_startup;
        node.cost.total += startup - last_startup + (per_row - last_per_row) * node.cost.rows;
        return node;
    }
    let c = node.cost;
    let mut out =
        Node::new(Kind::Result { replaces: None, one_time: Vec::new() }, level.display, vec![node]);
    out.cost = Cost {
        startup: c.startup + startup,
        total: c.total + startup + (cost::CPU_TUPLE + per_row) * c.rows,
        ..c
    };
    out
}

/// The node computes the targets, with their costs, as `create_projection_path` on the plan of `FROM` and as `create_agg_path` and `create_windowagg_path` add them. The scans of the members of a table with children each compute them, as `apply_scanjoin_target_to_paths` gives the target to each child of a partitioned table.
fn project(node: &mut Node<'_>, startup: f64, per_row: f64) {
    if startup == 0.0 && per_row == 0.0 {
        return;
    }
    if from::members_append(node) {
        // A `Merge Append` and an ordered `Append` start with all their members, and another `Append` with its first member.
        let first = node.children.first().map_or(0.0, |c| c.cost.startup);
        let merge = matches!(node.kind, Kind::MergeAppend(_)) || node.cost.startup != first;
        for child in &mut node.children {
            let added = startup + per_row * child.cost.rows;
            child.cost.startup += startup;
            child.cost.total += added;
            node.cost.total += added;
            if merge {
                node.cost.startup += startup;
            }
        }
        if !merge {
            node.cost.startup = node.children.first().map_or(0.0, |c| c.cost.startup);
        }
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
    ) -> (Node<'q>, Vec<OrderKey>) {
        let mut paths = Paths::new(level.fraction > 0.0);
        for (i, (node, order)) in plans.into_iter().enumerate() {
            if let Some(found) = self.upper_order(level, node, order, i > 0 || level.presorted) {
                paths.add(costs(&found.0), Vec::new(), found);
            }
        }
        match paths.fractional(fraction, |(n, _)| n.cost.rows) {
            Some(found) => found,
            // `build_minmax_path`: no plan of `FROM` gives the rows in the order of the subquery, so the query has no plan.
            None if level.presorted => {
                self.failed = true;
                let node = Node::new(
                    Kind::Result { replaces: None, one_time: Vec::new() },
                    level.scope,
                    Vec::new(),
                );
                (node, Vec::new())
            }
            None => unreachable!("the cheapest plan gets a result"),
        }
    }

    /// The `pathlist` of the final relation of a query: each plan of the query that `add_path` keeps, with the order of its rows.
    pub(crate) fn final_paths(
        &mut self,
        level: &Level<'q>,
        plans: from::Plans<'q>,
    ) -> Vec<(Node<'q>, Vec<OrderKey>)> {
        let mut paths = Paths::new(level.fraction > 0.0);
        for (i, (node, order)) in plans.into_iter().enumerate() {
            if let Some((node, order)) = self.upper_order(level, node, order, i > 0) {
                paths.add(costs(&node), order.clone(), (node, order));
            }
        }
        paths.kept()
    }

    /// `preprocess_minmax_aggregates` and `create_minmaxagg_plan`: the `Result` of a query with [`Query::minmax`], which reads the value of each aggregate call from an `InitPlan` that reads the first row of its subquery, when it costs less than `normal`, the plan with an `Agg` node. Each subquery tries the first order of the nulls, then the other order, and each try takes a name. When a call has no plan in either order, or the `Result` costs more, the query keeps `normal`, and the subplans take no place in the plan.
    pub(crate) fn minmax(
        &mut self,
        level: &Level<'q>,
        minmax: &'q MinMax,
        normal: Node<'q>,
    ) -> Node<'q> {
        let scope = level.scope;
        let first = self.groups.len();
        let mut plans = Vec::new();
        for tries in &minmax.aggs {
            let mut found = None;
            for query in tries {
                self.counts[7] += 1;
                let name = format!("minmax_{}", self.counts[7]);
                let start = self.groups.len();
                self.presorted = true;
                let plan = self.subplan(|b| b.query(query, Some(scope)));
                if std::mem::take(&mut self.failed) {
                    self.groups[start..].iter_mut().for_each(Vec::clear);
                    continue;
                }
                found = Some((name, plan));
                break;
            }
            let Some(found) = found else {
                self.groups[first..].iter_mut().for_each(Vec::clear);
                return normal;
            };
            plans.push(found);
        }
        // `create_minmaxagg_path`: each subplan costs the cost of its first row, which its `Limit` has.
        let costs = target_costs(level).unwrap_or_default();
        let startup = plans.iter().map(|(_, plan)| plan.cost.total).sum::<f64>() + costs.full.0;
        let mut c =
            Cost { startup, total: startup + costs.full.1 + cost::CPU_TUPLE, rows: 1.0, width: 0 };
        if let Some(having) = &level.query.having {
            let (start, per_row) = level.stats.qual_costs([having]);
            c.startup += start;
            c.total += start + per_row;
        }
        let mut one_time = Vec::new();
        if let Some(having) = &minmax.having {
            let mut parts = Vec::new();
            exprs::and_parts(having, &mut parts);
            one_time.extend(parts.into_iter().map(Item::of));
        }
        let replaces = Some(Replaced::MinMax);
        let mut node = Node::new(Kind::Result { replaces, one_time }, level.display, Vec::new());
        node.cost = c;
        // `grouping_planner` plans `DISTINCT`, `ORDER BY` and `LIMIT` above the `MinMaxAggPath`, with the targets that read the subqueries.
        if let Some(upper) = minmax.upper.as_deref() {
            let Some(found) = steps(level, node, &mut Vec::new(), false, costs) else {
                self.groups[first..].iter_mut().for_each(Vec::clear);
                return normal;
            };
            node = found;
            upper_keys(&mut node, level.query, upper);
        }
        if node.cost.total >= normal.cost.total {
            self.groups[first..].iter_mut().for_each(Vec::clear);
            return normal;
        }
        // Each subquery of the new targets takes the name of its subplan. A subquery that the query had is a copy of it, which takes the label of the subquery of the query.
        let query = level.query;
        let upper = minmax.upper.as_deref();
        let new: [Vec<&Expr>; 2] = [
            minmax.targets.iter().chain(&minmax.having).collect(),
            upper.map_or_else(Vec::new, |u| u.targets.iter().map(|t| &t.expr).collect()),
        ];
        for exprs in new {
            let mut own =
                sublinks(query.targets.iter().map(|t| &t.expr).chain(&query.having)).into_iter();
            for link in sublinks(exprs) {
                if let Some(at) = minmax.aggs.iter().position(|tries| tries[0] == link.query) {
                    let name = plans[at].0.clone();
                    self.labels.push(Label {
                        link,
                        name,
                        init: true,
                        hashed: false,
                        alternative: false,
                    });
                } else if let Some(orig) = own.next()
                    && let Some(label) = self.labels.iter().find(|l| std::ptr::eq(l.link, orig))
                {
                    let (name, init, hashed) = (label.name.clone(), label.init, label.hashed);
                    self.labels.push(Label { link, name, init, hashed, alternative: false });
                }
            }
        }
        // `SS_finalize_plan` puts the subplans on the top node of the query.
        node.init_plans = plans
            .into_iter()
            .map(|(name, plan)| SubPlan { name: format!("InitPlan {name}"), plan })
            .collect();
        // `set_rtable_names`: the relation of the query has no scan in the plan, so it takes no name.
        self.current.retain(|&s| s != scope);
        node
    }

    /// The nodes above one plan of `FROM`. With `only`, the plan is not the cheapest, and the result is `None` when a step needs a sort of it or a step has no plan for it.
    pub(crate) fn upper_one(
        &mut self,
        level: &Level<'q>,
        node: Node<'q>,
        order: Vec<OrderKey>,
        only: bool,
    ) -> Option<Node<'q>> {
        self.upper_order(level, node, order, only).map(|(node, _)| node)
    }

    /// [`Self::upper_one`] with the order of the rows of the plan.
    fn upper_order(
        &mut self,
        level: &Level<'q>,
        mut node: Node<'q>,
        mut order: Vec<OrderKey>,
        mut only: bool,
    ) -> Option<(Node<'q>, Vec<OrderKey>)> {
        let query = level.query;
        let scope = level.display;
        let costs = target_costs(level).unwrap_or_default();
        // `apply_scanjoin_target_to_paths`: an `Append` of a `UNION ALL` or of a table with children of `INHERITS` does not compute targets, so a `Result` above it computes the targets that are not its columns, as `create_projection_path`.
        let computed = !query.grouped
            && query.windows.is_empty()
            && query.targets.iter().any(|t| !matches!(t.expr.kind, ExprKind::Var(_)));
        if computed && (from::append_rel(query, &node) || from::inheritance_append(query, &node)) {
            let c = node.cost;
            let (startup, per_row) = costs.scan;
            node =
                Node::new(Kind::Result { replaces: None, one_time: Vec::new() }, scope, vec![node]);
            node.cost = Cost {
                startup: c.startup + startup,
                total: c.total + startup + (cost::CPU_TUPLE + per_row) * c.rows,
                ..c
            };
        } else {
            project(&mut node, costs.scan.0, costs.scan.1);
        }
        if query.set_op.is_none() {
            if query.grouped {
                node = aggregate(level, node, &mut order, only)?;
                project(&mut node, costs.group.0, costs.group.1);
                // The rows of grouping sets have no order, so the steps above take the cheapest plan of the groups, as each plan has the same rows.
                only &= query.grouping_sets.is_empty();
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
        }
        let node = steps(level, node, &mut order, only, costs)?;
        Some((node, order))
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
                    target: None,
                    leaf: None,
                });
                let mut parts = Vec::new();
                if let Some(cond) = &update.filter {
                    exprs::and_parts(cond, &mut parts);
                }
                filter = parts.into_iter().map(Item::of).collect();
            }
            Conflict {
                resolution: c.resolution(),
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
        // `ApplyRetrieveRule`: the view with triggers `INSTEAD OF` that an `UPDATE`, a `DELETE` or a `MERGE` writes is one more entry of the range table after the subquery of the view.
        let relation = if operation == Operation::Insert || write.instead { TARGET } else { 0 };
        let c = node.cost;
        let mut out = Node::new(
            Kind::Modify { operation, table: write.table, relation, conflict, targets: Vec::new() },
            level.scope,
            vec![node],
        );
        // `create_modifytable_path`: with `RETURNING`, the node gives the rows of the node below it.
        let rows = if write.returning.is_some() { c.rows } else { 0.0 };
        out.cost = Cost { rows, width: 0, ..c };
        out
    }

    /// The plan of a set operation, as `plan_set_operations`. The nodes above it show the expressions of the leftmost `SELECT`.
    pub(crate) fn set_op(
        &mut self,
        level: &mut Level<'q>,
        op: &'q SetOp,
    ) -> Vec<(Node<'q>, Vec<OrderKey>)> {
        let (plans, scope) = if op.work.is_none() && op.kind == SetKind::Union && !op.all {
            (self.union_paths(level, op, true), self.leftmost_scope(level, op))
        } else if op.work.is_none() && op.kind != SetKind::Union {
            // Its outer side, which `INTERSECT` can swap, gives the names.
            let plans = self.nonunion(level, op, true);
            let scope = plans.first().map(|(node, _)| node.scope);
            (plans, scope)
        } else {
            (vec![(self.set_node(level, op), false)], self.leftmost_scope(level, op))
        };
        let mut query = level.query;
        if let Some(scope) = scope {
            level.display = scope;
            level.display_query = self.scopes[scope].query;
            query = level.display_query;
        }
        // The `Unique` of a sort or of a `Merge Append` of a `UNION`, and the `SetOp` with the sorted strategy, give the rows in the order of the columns, in the expressions of the `SELECT` that gives the names, as the keys of `ORDER BY` of the set operation.
        let order = setop::order(query, op);
        plans
            .into_iter()
            .map(|(node, sorted)| (node, if sorted { order.clone() } else { Vec::new() }))
            .collect()
    }

    /// The plan of a part of the tree of a set operation.
    pub(crate) fn set_tree(&mut self, level: &Level<'q>, tree: &'q SetTree) -> Node<'q> {
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
            SetTree::Op(op) if op.project.is_empty() => self.set_node(level, op),
            SetTree::Op(op) => {
                // `recurse_set_operations`: a `Result` above the plan casts the columns to the types of the operation above it.
                let child = self.set_node(level, op);
                let c = child.cost;
                let width = op.project.iter().map(|e| cost::type_width(e.ty, e.typmod)).sum();
                let total =
                    c.total + c.rows * (cost::CPU_TUPLE + cost::qual_cost(op.project.iter()));
                let mut node = Node::new(
                    Kind::Result { replaces: None, one_time: Vec::new() },
                    level.scope,
                    vec![child],
                );
                node.output = op.project.iter().map(Item::of).collect();
                node.cost = Cost { startup: c.startup, total, rows: c.rows, width };
                node
            }
        }
    }

    /// The members of a `UNION`, with the members of each `UNION` below it that PostgreSQL makes one `Append` with, as `recurse_union_children`.
    fn union_members(
        &mut self,
        level: &Level<'q>,
        top: &'q SetOp,
        tree: &'q SetTree,
        out: &mut Vec<Node<'q>>,
        ordered: &mut Option<Vec<Node<'q>>>,
    ) {
        if let SetTree::Op(op) = tree
            && op.kind == SetKind::Union
            && op.work.is_none()
            && (op.all == top.all || op.all)
            && op.columns == top.columns
        {
            self.union_members(level, top, &op.left, out, ordered);
            self.union_members(level, top, &op.right, out, ordered);
            return;
        }
        // `build_setop_child_paths` adds the plans in the order of the columns only for a `SELECT` of `UNION` without `ALL`.
        if top.all || !matches!(tree, SetTree::Leaf(_)) {
            *ordered = None;
            out.push(self.set_tree(level, tree));
            return;
        }
        let (node, sorted) = self.union_leaf(level, top, tree);
        match (ordered.as_mut(), sorted) {
            (Some(list), Some(sorted)) => list.push(sorted),
            _ => *ordered = None,
        }
        out.push(node);
    }

    pub(crate) fn set_node(&mut self, level: &Level<'q>, op: &'q SetOp) -> Node<'q> {
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
        match op.kind {
            SetKind::Union => match self.union_paths(level, op, false).into_iter().next() {
                Some((node, _)) => node,
                None => unreachable!("a set operation has a plan"),
            },
            SetKind::Intersect | SetKind::Except => {
                match self.nonunion(level, op, false).into_iter().next() {
                    Some((node, _)) => node,
                    None => unreachable!("a set operation has a plan"),
                }
            }
        }
    }

    /// `generate_union_paths`: the plans of a `UNION` that `add_path` keeps, the cheapest first. `UNION ALL` is an `Append` of the plans of its members. `UNION` hashes the rows of the `Append`, or sorts them for a `Unique`, or, with true, reads the plans of its members in the order of the columns with a `Merge Append` below the `Unique`. The two plans with the `Unique` give the rows in the order of the columns.
    fn union_paths(
        &mut self,
        level: &Level<'q>,
        op: &'q SetOp,
        top: bool,
    ) -> Vec<(Node<'q>, bool)> {
        let scope = level.scope;
        let columns = op.columns.len();
        let mut members = Vec::new();
        let mut ordered = Some(Vec::new());
        self.union_members(level, op, &op.left, &mut members, &mut ordered);
        self.union_members(level, op, &op.right, &mut members, &mut ordered);
        let startup = members.first().map_or(0.0, |m| m.cost.startup);
        let rows: f64 = members.iter().map(|m| m.cost.rows).sum();
        // `create_setop_pathtarget`: the average width of the members, with the width of the rows of a member below the `Result` that casts its columns.
        let size: f64 = members
            .iter()
            .map(|m| {
                let below = match (&m.kind, m.children.as_slice()) {
                    (Kind::Result { .. }, [child]) if !m.output.is_empty() => child,
                    _ => m,
                };
                f64::from(below.cost.width) * m.cost.rows
            })
            .sum();
        let width = if rows > 0.0 { (size / rows).round_ties_even() as i32 } else { 0 };
        let total: f64 =
            members.iter().map(|m| m.cost.total).sum::<f64>() + 0.5 * cost::CPU_TUPLE * rows;
        let mut append = Node::new(Kind::Append, scope, members);
        append.cost = Cost { startup, total, rows, width };
        if op.all {
            return vec![(append, false)];
        }
        let c = append.cost;
        let hashable = op.groups.iter().all(|g| g.hashable);
        let sortable = op.groups.iter().all(|g| g.operator != 0);
        let per_row = cost::CPU_OPERATOR * columns as f64;
        let hash_startup = c.total + per_row * c.rows;
        let hash_total = hash_startup + cost::CPU_TUPLE * rows;
        let (sort_startup, sort_total) = cost::sort((c.startup, c.total), c.rows, c.width, None);
        let unique_total = sort_total + per_row * c.rows;
        let keys = self.leaf_items(level, op);
        // The keys show with the names of the leftmost `SELECT`.
        let scope = self.leftmost_scope(level, op).unwrap_or(scope);
        let sort_keys: Vec<SortKey<'q>> = keys
            .iter()
            .zip(&op.groups)
            .map(|(item, g)| SortKey {
                item: item.clone(),
                desc: false,
                using: None,
                nulls_first: g.nulls_first,
            })
            .collect();
        let enable = crate::enable();
        let below = append.disabled_nodes();
        let order = setop::order(level.query, op);
        let mut paths = Paths::new(top && level.fraction > 0.0);
        if hashable {
            paths.add(
                (below + u32::from(!enable.hashagg), hash_startup, hash_total),
                Vec::new(),
                0,
            );
        }
        if sortable || !hashable {
            paths.add(
                (below + u32::from(!enable.sort), sort_startup, unique_total),
                order.clone(),
                1,
            );
        }
        // `create_merge_append_path` of the plans of the members in the order of the columns, and `create_unique_path` above it.
        let merged = ordered.filter(|list| sortable && !list.is_empty()).map(|list| {
            let n = list.len().max(2) as f64;
            let log_n = n.log2();
            let comparison = 2.0 * cost::CPU_OPERATOR;
            let tuples: f64 = list.iter().map(|m| m.cost.rows).sum();
            let first = comparison * n * log_n;
            let run = tuples * comparison * log_n + cost::CPU_TUPLE * cost::APPEND_CPU * tuples;
            let input_startup: f64 = list.iter().map(|m| m.cost.startup).sum();
            let input_total: f64 = list.iter().map(|m| m.cost.total).sum();
            let mut merge = Node::new(Kind::MergeAppend(sort_keys.clone()), scope, list);
            merge.cost = Cost {
                startup: first + input_startup,
                total: first + run + input_total,
                rows: tuples,
                width,
            };
            let m = merge.cost;
            let mut node = Node::new(Kind::Unique, scope, vec![merge]);
            node.cost = Cost { startup: m.startup, total: m.total + per_row * tuples, rows, width };
            node
        });
        if let Some(node) = &merged {
            paths.add(costs(node), order, 2);
        }
        let mut out = Vec::new();
        let mut merged = merged;
        for kept in paths.kept() {
            match kept {
                0 => {
                    let strategy = Strategy::Hashed;
                    let kind = Kind::Aggregate { strategy, keys: keys.clone(), sets: Vec::new() };
                    let mut node = Node::new(kind, scope, vec![append.clone()]);
                    node.cost = Cost { startup: hash_startup, total: hash_total, rows, width };
                    node.disabled = !enable.hashagg;
                    out.push((node, false));
                }
                1 => {
                    let mut sorted =
                        Node::new(Kind::Sort(sort_keys.clone()), scope, vec![append.clone()]);
                    sorted.cost = Cost { startup: sort_startup, total: sort_total, ..c };
                    sorted.disabled = !enable.sort;
                    let mut node = Node::new(Kind::Unique, scope, vec![sorted]);
                    node.cost = Cost { startup: sort_startup, total: unique_total, rows, width };
                    out.push((node, true));
                }
                _ => out.extend(merged.take().map(|node| (node, true))),
            }
        }
        out
    }

    /// The scope of the leftmost `SELECT` of a set operation.
    pub(crate) fn leftmost_scope(&self, level: &Level<'q>, op: &'q SetOp) -> Option<usize> {
        let sub = level.query.relations.get(op.left.leftmost())?.subquery.as_deref()?;
        self.scopes.iter().position(|s| std::ptr::eq(s.query, sub))
    }

    /// The columns of the leftmost `SELECT` of a set operation, as the keys of the node that finds the equal rows.
    pub(crate) fn leaf_items(&self, level: &Level<'q>, op: &'q SetOp) -> Vec<Item<'q>> {
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

/// The keys of the ordered aggregate call of the targets and of `HAVING` of a query, as [`ordered_keys`] finds them.
pub(crate) fn aggregate_order(query: &Query) -> Vec<SortKey<'_>> {
    let mut exprs_of: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    let (_, having) = having_split(query);
    exprs_of.extend(having.iter().copied());
    let mut aggs = Vec::new();
    exprs::aggregates(&exprs_of, &mut aggs);
    ordered_keys(&aggs)
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
    // `make_pathkeys_for_groupagg`: with `enable_presorted_aggregate` off, the input of the `Agg` gets no sort for an ordered call, and the call sorts its own rows.
    let ordered = ordered_keys(&aggs);
    let has_ordered = !ordered.is_empty();
    let ordered = if crate::enable().presorted_aggregate { ordered } else { Vec::new() };
    let selectivity = cost::list_selectivity(having.iter().copied());
    let (having_startup, having_cost) = level.stats.qual_costs(having.iter().copied());
    let filter: Vec<Item<'q>> = having.iter().map(|h| Item::of(h)).collect();
    let c = input.cost;
    let n = c.rows;
    if !query.grouping_sets.is_empty() {
        let costs = AggCosts { units, finals, having: (having_startup, having_cost), selectivity };
        return grouping_sets(level, input, order, costs, filter, has_ordered, only);
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
    let hashable = hashable && !group.is_empty() && !has_ordered;
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
    // `cost_agg`: the transition cost, then the cost of the keys, which the rounding of the sums follows. `GROUP BY` with no aggregate is a `Group` node, which `cost_group` gives no cost for each group.
    let group_node = aggs.is_empty();
    let sorted_total = sorted_input.total
        + cost::CPU_OPERATOR * units * n
        + cost::CPU_OPERATOR * nkeys * n
        + finals * groups
        + if group_node { 0.0 } else { cost::CPU_TUPLE * groups }
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
    // `cost_agg` and `cost_sort`: a setting that turns off the hash table or the sort makes a disabled node, and the plan with fewer disabled nodes wins.
    let enable = crate::enable();
    let hash_disabled = u32::from(!enable.hashagg);
    let sort_disabled = u32::from(sort_disabled(is_sorted, count));
    let hash_wins = match hash_disabled.cmp(&sort_disabled) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => kept && hash_after < sorted_total,
    };
    if !only && hashable && hash_wins {
        let mut node = Node::new(
            Kind::Aggregate { strategy: Strategy::Hashed, keys, sets: Vec::new() },
            scope,
            vec![input],
        );
        node.filter = filter;
        node.disabled = !enable.hashagg;
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
    let strategy = if group_node { Strategy::Group } else { Strategy::Sorted };
    let mut node =
        Node::new(Kind::Aggregate { strategy, keys, sets: Vec::new() }, scope, vec![input]);
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

/// The costs of the aggregate calls and of `HAVING` of a query with grouping sets, as `cost_agg` reads them.
#[derive(Clone, Copy)]
struct AggCosts {
    /// The cost of the transition of the calls for each row, in units of `cpu_operator_cost`.
    units: f64,
    /// The cost of the final functions for each group.
    finals: f64,
    /// The start cost and the cost for each group of `HAVING`.
    having: (f64, f64),
    /// The selectivity of `HAVING`.
    selectivity: f64,
}

/// `cost_agg`: the start cost, the total cost and the rows of an `Agg` of `strategy` with `cols` keys and `groups` groups, which reads `rows` rows with the start cost and the total cost `input`. `Mixed` has the costs of `Sorted`.
fn cost_agg(
    strategy: Strategy,
    costs: AggCosts,
    cols: usize,
    groups: f64,
    input: (f64, f64),
    rows: f64,
) -> (f64, f64, f64) {
    let trans = cost::CPU_OPERATOR * costs.units * rows;
    let keys = cost::CPU_OPERATOR * cols as f64 * rows;
    let (mut startup, mut total, out) = match strategy {
        Strategy::Plain => {
            let startup = input.1 + trans + costs.finals;
            (startup, startup + cost::CPU_TUPLE, 1.0)
        }
        Strategy::Hashed => {
            let startup = input.1 + trans + keys;
            (startup, startup + costs.finals * groups + cost::CPU_TUPLE * groups, groups)
        }
        _ => {
            let total = input.1 + trans + keys + costs.finals * groups + cost::CPU_TUPLE * groups;
            (input.0, total, groups)
        }
    };
    let (having_startup, having_cost) = costs.having;
    startup += having_startup;
    total += having_startup + out * having_cost;
    (startup, total, cost::clamp_rows(out * costs.selectivity))
}

/// A rollup of `preprocess_grouping_sets`: the grouping sets that one pass over the rows in the order of the items of the first set gives, as each set is the first items of the set before it. A rollup of a hash table has one set, or the sets with no items.
#[derive(Clone, Debug)]
struct RollupSets {
    /// The sets, the largest first, each with the places of its targets in the order of the sort.
    sets: Vec<Vec<usize>>,
    /// The groups of each set, as `get_number_of_groups` estimates them.
    groups: Vec<f64>,
    /// `hashable`: the first set has items, and a hash table can hold its groups.
    hashable: bool,
    /// `is_hashed`.
    hashed: bool,
}

impl RollupSets {
    /// `numGroups`: the groups of all the sets.
    fn groups(&self) -> f64 {
        self.groups.iter().sum()
    }

    /// The rollup of a hash table for one set.
    fn hash(set: &[usize], groups: f64) -> RollupSets {
        RollupSets { sets: vec![set.to_vec()], groups: vec![groups], hashable: true, hashed: true }
    }
}

/// `preprocess_grouping_sets`: the rollups of the grouping sets that a sort can give, and a rollup of a hash table for each set with an item that has no sort operator.
fn rollups(level: &Level<'_>, rows: f64) -> (Vec<RollupSets>, Vec<RollupSets>) {
    let query = level.query;
    let sets = expand_grouping_sets(&query.grouping_sets, query.group_distinct, MAX_SETS)
        .unwrap_or_default();
    let group_of = |t: usize| query.group.iter().find(|g| g.target == t);
    let unsortable = |t: &usize| group_of(*t).is_none_or(|g| g.operator == 0);
    let unhashable = |t: &usize| group_of(*t).is_none_or(|g| !g.hashable);
    let groups = |set: &[usize]| {
        let exprs: Vec<&Expr> = set.iter().map(|t| level.target(*t)).collect();
        level.stats.groups(&exprs, rows)
    };
    let (unsortable_sets, sortable): (Vec<Vec<usize>>, Vec<Vec<usize>>) =
        sets.into_iter().partition(|s| s.iter().any(unsortable));
    let chains = if sortable.is_empty() { Vec::new() } else { extract_rollup_sets(&sortable) };
    // With one rollup, the sets take the order of `ORDER BY`.
    let sort: Vec<usize> =
        if chains.len() == 1 { query.sort.iter().map(|s| s.target).collect() } else { Vec::new() };
    let rollups = chains
        .iter()
        .map(|chain| {
            let sets = reorder_grouping_sets(chain, &sort);
            let hashable = !sets[0].is_empty() && !sets[0].iter().any(unhashable);
            let groups = sets.iter().map(|s| groups(s)).collect();
            RollupSets { sets, groups, hashable, hashed: false }
        })
        .collect();
    let hash_sets = unsortable_sets.iter().map(|s| RollupSets::hash(s, groups(s))).collect();
    (rollups, hash_sets)
}

/// `extract_rollup_sets`: the fewest chains of the grouping sets, the smallest set first, where each set has all the items of the set before it. Equal sets go in the same chain, and the sets with no items go in the first chain. The sets come with the smallest first.
fn extract_rollup_sets(sets: &[Vec<usize>]) -> Vec<Vec<Vec<usize>>> {
    let empty = sets.iter().take_while(|s| s.is_empty()).count();
    if empty == sets.len() {
        return vec![sets.to_vec()];
    }
    // The lists start at 1, as the sets of `BipartiteMatch` do.
    let mut orig: Vec<Vec<Vec<usize>>> = vec![Vec::new()];
    let mut masks: Vec<Vec<usize>> = vec![Vec::new()];
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new()];
    let (mut j_size, mut j) = (0, 0);
    for candidate in &sets[empty..] {
        let mut mask = candidate.clone();
        mask.sort_unstable();
        mask.dedup();
        let i = orig.len();
        let mut dup_of = 0;
        if j_size == candidate.len() {
            dup_of = (j..i).find(|&k| masks[k] == mask).unwrap_or(0);
        } else if j_size < candidate.len() {
            j_size = candidate.len();
            j = i;
        }
        if dup_of > 0 {
            orig[dup_of].push(candidate.clone());
            continue;
        }
        orig.push(vec![candidate.clone()]);
        // The smaller sets that the candidate has all the items of, the last set first.
        adjacency
            .push((1..j).rev().filter(|&k| masks[k].iter().all(|m| mask.contains(m))).collect());
        masks.push(mask);
    }
    let count = orig.len() - 1;
    let (pair_uv, pair_vu) = bipartite_match(&adjacency);
    let mut chains = vec![0; count + 1];
    let mut num_chains = 0;
    for i in 1..=count {
        let (u, v) = (pair_vu[i], pair_uv[i]);
        chains[i] = if u > 0 && u < i {
            chains[u]
        } else if v > 0 && v < i {
            chains[v]
        } else {
            num_chains += 1;
            num_chains
        };
    }
    let mut results = vec![Vec::new(); num_chains + 1];
    results[1] = vec![Vec::new(); empty];
    for i in 1..=count {
        results[chains[i]].append(&mut orig[i]);
    }
    results.remove(0);
    results
}

/// `BipartiteMatch`: the largest matching of the Hopcroft-Karp algorithm in the graph of `adjacency`, whose nodes start at 1. The result has the node of the other side for each node of each side, or 0.
fn bipartite_match(adjacency: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
    /// `hk_depth_search`.
    fn depth(
        u: usize,
        adjacency: &[Vec<usize>],
        distance: &mut [usize],
        pair_uv: &mut [usize],
        pair_vu: &mut [usize],
    ) -> bool {
        if u == 0 {
            return true;
        }
        if distance[u] == usize::MAX {
            return false;
        }
        let next = distance[u] + 1;
        for &v in adjacency[u].iter().rev() {
            if distance[pair_vu[v]] == next
                && depth(pair_vu[v], adjacency, distance, pair_uv, pair_vu)
            {
                pair_vu[v] = u;
                pair_uv[u] = v;
                return true;
            }
        }
        distance[u] = usize::MAX;
        false
    }
    let size = adjacency.len();
    let mut pair_uv = vec![0; size];
    let mut pair_vu = vec![0; size];
    let mut distance = vec![0; size];
    loop {
        // `hk_breadth_search`.
        distance[0] = usize::MAX;
        let mut queue = Vec::new();
        for u in 1..size {
            if pair_uv[u] == 0 {
                distance[u] = 0;
                queue.push(u);
            } else {
                distance[u] = usize::MAX;
            }
        }
        let mut at = 0;
        while at < queue.len() {
            let u = queue[at];
            at += 1;
            if distance[u] < distance[0] {
                for &v in adjacency[u].iter().rev() {
                    let next = pair_vu[v];
                    if distance[next] == usize::MAX {
                        distance[next] = distance[u] + 1;
                        queue.push(next);
                    }
                }
            }
        }
        if distance[0] == usize::MAX {
            break;
        }
        for u in 1..size {
            if pair_uv[u] == 0 {
                depth(u, adjacency, &mut distance, &mut pair_uv, &mut pair_vu);
            }
        }
    }
    (pair_uv, pair_vu)
}

/// `reorder_grouping_sets`: the sets of a chain, the largest first, each with the items of the set after it first. The items take the order of `sort` as long as they can.
fn reorder_grouping_sets(sets: &[Vec<usize>], sort: &[usize]) -> Vec<Vec<usize>> {
    let mut sort = sort;
    let mut previous: Vec<usize> = Vec::new();
    let mut result = Vec::new();
    for candidate in sets {
        let mut new_elems: Vec<usize> =
            candidate.iter().filter(|c| !previous.contains(c)).copied().collect();
        while sort.len() > previous.len() && !new_elems.is_empty() {
            let item = sort[previous.len()];
            if let Some(at) = new_elems.iter().position(|&e| e == item) {
                previous.push(item);
                new_elems.remove(at);
            } else {
                sort = &[];
                break;
            }
        }
        previous.extend(new_elems);
        result.insert(0, previous.clone());
    }
    result
}

/// `create_groupingsets_path`: the strategy, the disabled nodes and the costs of an `Agg` of the rollups over rows with the costs `input`. A rollup after the first one that is not a hash table needs a sort of its own, except the first such rollup after a hash table.
fn groupingsets_cost(
    strategy: Strategy,
    rollups: &[RollupSets],
    input: Cost,
    costs: AggCosts,
) -> (Strategy, u32, Cost) {
    let enable = crate::enable();
    let strategy = match strategy {
        Strategy::Sorted if rollups.len() == 1 && rollups[0].sets[0].is_empty() => Strategy::Plain,
        Strategy::Mixed if rollups.len() == 1 => Strategy::Hashed,
        _ => strategy,
    };
    let n = input.rows;
    let mut disabled =
        u32::from(matches!(strategy, Strategy::Hashed | Strategy::Mixed) && !enable.hashagg);
    let first = &rollups[0];
    let (startup, mut total, mut rows) = cost_agg(
        strategy,
        costs,
        first.sets[0].len(),
        first.groups(),
        (input.startup, input.total),
        n,
    );
    let mut first_sort = first.hashed;
    for rollup in &rollups[1..] {
        let cols = rollup.sets[0].len();
        let (_, t, r) = if rollup.hashed || first_sort {
            first_sort &= rollup.hashed;
            disabled += u32::from(rollup.hashed && !enable.hashagg);
            let strategy = if rollup.hashed { Strategy::Hashed } else { Strategy::Sorted };
            cost_agg(strategy, costs, cols, rollup.groups(), (0.0, 0.0), n)
        } else {
            disabled += u32::from(!enable.sort);
            let sort = cost::sort((0.0, 0.0), n, input.width, None);
            cost_agg(Strategy::Sorted, costs, cols, rollup.groups(), sort, n)
        };
        total += t;
        rows += r;
    }
    (strategy, disabled, Cost { startup, total, rows, width: 0 })
}

/// The `Agg` of a query with grouping sets, as `consider_groupingsets_paths` plans it: a hash table for each set, with the sets with no items or the first rollup in the order of the rows in the same pass; the rollups over sorted rows, with a hash table for each set of the rollups after the first one; or the rollups over sorted rows. The plan with the fewest disabled nodes and then the lowest cost wins. With `only`, the input is not the cheapest plan, so it gets only the strategies of sorted rows, and only when its rows are in the order of one key or more.
fn grouping_sets<'q>(
    level: &Level<'q>,
    input: Node<'q>,
    order: &mut Vec<OrderKey>,
    costs: AggCosts,
    filter: Vec<Item<'q>>,
    has_ordered: bool,
    only: bool,
) -> Option<Node<'q>> {
    let query = level.query;
    let c = input.cost;
    let (rollups, hash_sets) = rollups(level, c.rows);
    let group_of = |t: usize| query.group.iter().find(|g| g.target == t);
    let group_keys = |r: &RollupSets| -> Vec<SortKey<'q>> {
        r.sets[0]
            .iter()
            .filter_map(|&t| group_of(t).map(|g| sort_key(level.target(t), g)))
            .collect()
    };
    let first_keys = rollups.first().map(group_keys).unwrap_or_default();
    let first_keys = level.stats.canonical(first_keys, |k| &*k.item.expr);
    let (is_sorted, count) = presorted(order, &first_keys);
    let any_hashable = rollups.iter().any(|r| r.hashable) || !hash_sets.is_empty();
    let can_hash = !has_ordered && any_hashable;
    // The candidates in the order that `add_paths_to_grouping_rel` adds them, with true when the input needs the order of the first rollup.
    let mut candidates: Vec<(Strategy, Vec<RollupSets>, bool)> = Vec::new();
    if !rollups.is_empty() && (is_sorted || !only || count > 0) {
        if can_hash {
            let mut out = Vec::new();
            let mut hashed = hash_sets.clone();
            if rollups.len() > 1 && rollups[1..].iter().any(|r| r.hashable) {
                out.push(rollups[0].clone());
                for rollup in &rollups[1..] {
                    if rollup.hashable {
                        let sets = rollup.sets.iter().zip(&rollup.groups);
                        hashed.extend(sets.map(|(s, g)| RollupSets::hash(s, *g)));
                    } else {
                        out.push(rollup.clone());
                    }
                }
            }
            if out.is_empty() && !hashed.is_empty() {
                out = rollups.clone();
            }
            for rollup in hashed {
                out.insert(0, rollup);
            }
            if !out.is_empty() {
                candidates.push((Strategy::Mixed, out, true));
            }
        }
        if hash_sets.is_empty() {
            candidates.push((Strategy::Sorted, rollups.clone(), true));
        }
    }
    if !only && can_hash {
        // The first rollup stays a pass over the rows when they are in its order.
        let (unhashed, rest) = match rollups.split_first() {
            Some((first, rest)) if is_sorted => (Some(first.clone()), rest),
            _ => (None, &rollups[..]),
        };
        if rest.iter().all(|r| r.hashable) {
            let mut out = Vec::new();
            let mut empty = 0;
            for rollup in hash_sets.iter().chain(rest) {
                for (set, groups) in rollup.sets.iter().zip(&rollup.groups) {
                    if set.is_empty() {
                        empty += 1;
                    } else {
                        out.push(RollupSets::hash(set, *groups));
                    }
                }
            }
            if !out.is_empty() {
                let mut strategy = Strategy::Hashed;
                if let Some(unhashed) = unhashed {
                    out.push(unhashed);
                    strategy = Strategy::Mixed;
                } else if empty > 0 {
                    let (sets, groups) = (vec![Vec::new(); empty], vec![1.0; empty]);
                    out.push(RollupSets { sets, groups, hashable: false, hashed: false });
                    strategy = Strategy::Mixed;
                }
                candidates.push((strategy, out, false));
            }
        }
    }
    let sorted_input = if is_sorted {
        c
    } else {
        let (startup, total) = sort_cost(level, c, &first_keys, count, None);
        Cost { startup, total, ..c }
    };
    let input_disabled = u32::from(sort_disabled(is_sorted, count));
    // `add_path`: a later plan wins with fewer disabled nodes, or with a total cost that is lower by more than the fuzz factor.
    let mut best: Option<(u32, Cost, usize)> = None;
    for (i, (strategy, rollups, sorted)) in candidates.iter().enumerate() {
        let input = if *sorted { sorted_input } else { c };
        let (_, disabled, cost) = groupingsets_cost(*strategy, rollups, input, costs);
        let disabled = disabled + if *sorted { input_disabled } else { 0 };
        let wins = best
            .is_none_or(|(d, b, _)| disabled < d || disabled == d && cost.total * 1.01 < b.total);
        if wins {
            best = Some((disabled, cost, i));
        }
    }
    let (_, _, at) = best?;
    let (strategy, rollups, sorted) = candidates.swap_remove(at);
    let mut input = input;
    if sorted {
        input = sort_if(level, input, first_keys, order, None, only)?;
    }
    let (strategy, disabled, cost) = groupingsets_cost(strategy, &rollups, input.cost, costs);
    let mut keys: Vec<Item<'q>> = Vec::new();
    let mut first_sort = rollups[0].hashed;
    let mut plan_rollups = Vec::with_capacity(rollups.len());
    for (i, rollup) in rollups.iter().enumerate() {
        let sets: Vec<Vec<Item<'q>>> = rollup
            .sets
            .iter()
            .map(|set| set.iter().map(|t| Item::below(level.target(*t))).collect())
            .collect();
        for item in sets.iter().flatten() {
            if !keys.iter().any(|k| k.expr.same(&item.expr)) {
                keys.push(item.clone());
            }
        }
        // `create_groupingsets_plan`: `make_sort_from_groupcols` sorts on all the items of the first set.
        let sort =
            if i > 0 && !rollup.hashed && !first_sort { group_keys(rollup) } else { Vec::new() };
        if i > 0 && !rollup.hashed {
            first_sort = false;
        }
        plan_rollups.push(Rollup { hashed: rollup.hashed, sort, sets });
    }
    order.clear();
    let mut node =
        Node::new(Kind::Aggregate { strategy, keys, sets: plan_rollups }, level.scope, vec![input]);
    node.filter = filter;
    node.disabled = disabled > 0;
    node.cost = cost;
    Some(node)
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

/// The first window of [`active_windows`], whose `WindowAgg` is at the bottom, or `None` when no call uses a window.
pub(crate) fn first_window(level: &Level<'_>) -> Option<usize> {
    active_windows(level, &window_calls(level.query)).first().copied()
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
            let args: f64 = call.args.iter().map(exprs::calls).sum();
            let filter = call.filter.as_deref().map_or(0.0, exprs::calls);
            total += cost::CPU_OPERATOR * (1.0 + args + filter) * c.rows;
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
    // `create_final_distinct_paths`: with `enable_hashagg` off, there is no hash table.
    let hashable =
        !query.distinct_on && processed.iter().all(|d| d.hashable) && crate::enable().hashagg;
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
    let hash_wins = sort_disabled(is_sorted, count) || kept && hash_after < unique_total;
    if !only && hashable && hash_wins {
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

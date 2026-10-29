//! The nodes above the join of `FROM`, as `grouping_planner` makes them: the aggregates, the windows, the functions that give sets, `DISTINCT`, `ORDER BY`, the locks of rows and `LIMIT`. Also the plan of a set operation and the node of a statement that writes a table.

use std::borrow::Cow;
use std::cmp::Ordering;

use rupg_analyze::{
    Aggref, BoolOp, Expr, ExprKind, FuncForm, MinMax, Query, SetKind, SetOp, SetTree, SortGroup,
    SubLink, WindowFunc, Write, WriteKind, expand_grouping_sets,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{
    Conflict, Cost, Grouped, Item, Kind, Label, Node, Operation, Replaced, Rollup, SortKey,
    Strategy, SubPlan, TARGET, Table,
};

use crate::from::paths::{Paths, costs};
use crate::from::{self, OrderKey, sort};
use crate::{Builder, Level, cost, exprs, output, setop};

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

/// The conditions of `HAVING` that go to `WHERE`, and the conditions that stay in `HAVING`, as `subquery_planner` divides them. A condition with no aggregate goes to `WHERE`, but with no `GROUP BY` or with an empty grouping set it also stays, because the group is there also when no row passes. With grouping sets, a condition that reads an item that a set does not have stays, because the item is null only in the groups.
pub(crate) fn having_split(query: &Query) -> (Vec<&Expr>, Vec<&Expr>) {
    let nullable = nullable_items(query);
    let empty_set = !query.grouping_sets.is_empty()
        && expand_grouping_sets(&query.grouping_sets, query.group_distinct, MAX_SETS)
            .unwrap_or_default()
            .iter()
            .any(Vec::is_empty);
    // `flatten_group_exprs`: an `AND` that is an item that a set does not have is in a `PlaceHolderVar`, so it is one condition.
    fn split<'e>(expr: &'e Expr, nullable: &[&Expr], out: &mut Vec<&'e Expr>) {
        match &expr.kind {
            ExprKind::Bool(BoolOp::And, args) if !nullable.iter().any(|n| n.same(expr)) => {
                args.iter().for_each(|a| split(a, nullable, out));
            }
            _ => out.push(expr),
        }
    }
    let mut parts = Vec::new();
    if let Some(having) = &query.having {
        split(having, &nullable, &mut parts);
    }
    let reads_nullable = |part: &Expr| {
        part.find(0, &mut |e, depth| {
            (depth == 0 && nullable.iter().any(|n| n.same(e))).then_some(())
        })
        .is_some()
    };
    let (mut to_where, mut keep) = (Vec::new(), Vec::new());
    for part in parts {
        let mut aggs = Vec::new();
        exprs::aggregates(&[part], &mut aggs);
        let stays = !aggs.is_empty()
            || has_grouping(part)
            || exprs::volatile(part)
            || has_sublink(part)
            || reads_nullable(part)
            || grouping_conflict(query, part);
        if stays {
            keep.push(part);
        } else if !query.group.is_empty() && !empty_set {
            to_where.push(part);
        } else {
            to_where.push(part);
            keep.push(part);
        }
    }
    (to_where, keep)
}

/// `mark_nullable_by_grouping`: the items of `GROUP BY` that a grouping set does not have.
pub(crate) fn nullable_items(query: &Query) -> Vec<&Expr> {
    if query.grouping_sets.is_empty() {
        return Vec::new();
    }
    let sets = expand_grouping_sets(&query.grouping_sets, query.group_distinct, MAX_SETS)
        .unwrap_or_default();
    query
        .group
        .iter()
        .filter(|g| !sets.iter().all(|s| s.contains(&g.target)))
        .map(|g| &query.targets[g.target].expr)
        .collect()
}

/// `Stats::canonical` for the keys of a node above the `Agg` of grouping sets. An item that a set does not have reads the output of the `Agg`, with `varnullingrels` or in a `PlaceHolderVar`, so it is not a member of the equivalence classes of the rows below. It is redundant only after the same item, and it makes no other item redundant.
fn upper_canonical<T>(level: &Level<'_>, items: Vec<T>, expr: impl Fn(&T) -> &Expr) -> Vec<T> {
    let nullable = nullable_items(level.query);
    if nullable.is_empty() {
        return level.stats.canonical(items, expr);
    }
    let is_nullable = |e: &Expr| nullable.iter().any(|n| n.same(e));
    let mut out: Vec<T> = Vec::new();
    for item in items {
        let e = expr(&item);
        let kept = if is_nullable(e) {
            !out.iter().any(|o| expr(o).same(e))
        } else {
            let before: Vec<&Expr> = out.iter().map(&expr).filter(|b| !is_nullable(b)).collect();
            !level.stats.redundant(e, &before)
        };
        if kept {
            out.push(item);
        }
    }
    out
}

/// `find_having_conflicts`: true when the condition compares an item of `GROUP BY` with an operator whose equality is not the equality of the group, as `expression_has_grouping_conflict` finds it. In `WHERE`, such a condition removes rows that the group puts with rows that pass, and so it changes the aggregates. An expression has no collation, so the checks of a nondeterministic collation are not done.
fn grouping_conflict(query: &Query, expr: &Expr) -> bool {
    let groups: Vec<(&Expr, u32)> =
        query.group.iter().map(|g| (&query.targets[g.target].expr, g.equal)).collect();
    let equal = |e: &Expr| groups.iter().find(|(g, _)| g.same(e)).map(|&(_, equal)| equal);
    !groups.is_empty() && conflict_walk(&equal, None, expr)
}

/// The function of the `=` operator that a grouping uses for an expression that is an item of the grouping, or `None` for an expression that is not.
pub(crate) type GroupEqual<'a> = dyn Fn(&Expr) -> Option<u32> + 'a;

/// `grouping_conflict_walker`: the walk of [`grouping_conflict`], with `equal` to find the items of the grouping. An operator of a btree or a hash family checks its operands, and a simple `CASE` on an item of `GROUP BY` checks the item where each `WHEN` compares it. Any other use of an item is safe.
pub(crate) fn conflict_walk(groups: &GroupEqual<'_>, case_arg: Option<&Expr>, expr: &Expr) -> bool {
    let walk = |e: &Expr| conflict_walk(groups, case_arg, e);
    match &expr.kind {
        ExprKind::Func(f) => match f.form {
            FuncForm::Operator(op) if index_member(op) => {
                f.args.iter().any(|a| conflict_operand(groups, case_arg, op, a))
            }
            _ => f.args.iter().any(walk),
        },
        ExprKind::ScalarArrayOp { func, args, .. } => {
            let element = |a: &Expr| builtin::type_by_oid(a.ty).map_or(0, |t| t.elem);
            let op = match args.as_slice() {
                [left, array] => {
                    cost::array_operator(*func, left.ty, element(array)).map(|o| o.oid)
                }
                _ => None,
            };
            match op.filter(|&op| index_member(op)) {
                Some(op) => args.iter().any(|a| conflict_operand(groups, case_arg, op, a)),
                None => args.iter().any(walk),
            }
        }
        ExprKind::Case(c) if c.arg.is_some() => {
            let arg = strip_relabel(c.arg.as_deref().unwrap_or(expr));
            let inner = if groups(arg).is_some() {
                Some(arg)
            } else if conflict_walk(groups, case_arg, arg) {
                return true;
            } else {
                None
            };
            c.whens.iter().any(|(when, _)| conflict_walk(groups, inner, when))
                || c.whens.iter().any(|(_, then)| walk(then))
                || walk(&c.default)
        }
        _ => expr.children().into_iter().any(walk),
    }
}

/// `grouping_check_operand`: true when the operand is an item of `GROUP BY` and the operator is not in a family with the `=` operator of the group, as `equality_ops_are_compatible`. Any other operand is walked.
fn conflict_operand(groups: &GroupEqual<'_>, case_arg: Option<&Expr>, op: u32, arg: &Expr) -> bool {
    let mut node = strip_relabel(arg);
    if matches!(node.kind, ExprKind::CaseTest) {
        match case_arg {
            Some(a) => node = a,
            None => return false,
        }
    }
    match groups(node) {
        Some(equal) => !equality_compatible(op, equal),
        None => conflict_walk(groups, case_arg, arg),
    }
}

/// The expression under its `RelabelType` nodes.
pub(crate) fn strip_relabel(mut expr: &Expr) -> &Expr {
    while let ExprKind::Relabel(inner, _) = &expr.kind {
        expr = inner;
    }
    expr
}

/// `op_is_safe_index_member`: true when the operator is in a btree or a hash family.
fn index_member(op: u32) -> bool {
    builtin::amops_of_operator(op).any(|a| matches!(a.method, BTREE_AM | HASH_AM))
}

/// `equality_ops_are_compatible`: true when the operator is the `=` operator whose function is `equal`, or when a btree or a hash family has both of them. When no built-in operator has the function, the operator counts as compatible.
fn equality_compatible(op: u32, equal: u32) -> bool {
    let eqops: Vec<u32> =
        builtin::operators().iter().filter(|o| o.code == equal).map(|o| o.oid).collect();
    eqops.is_empty()
        || eqops.contains(&op)
        || builtin::amops_of_operator(op).filter(|a| matches!(a.method, BTREE_AM | HASH_AM)).any(
            |a| eqops.iter().any(|&e| builtin::amops_of_operator(e).any(|b| b.family == a.family)),
        )
}

/// `BTREE_AM_OID`.
const BTREE_AM: u32 = 403;
/// `HASH_AM_OID`.
const HASH_AM: u32 = 405;

/// True when the expression calls `GROUPING` of the query.
fn has_grouping(expr: &Expr) -> bool {
    expr.find(0, &mut |e, depth| match &e.kind {
        ExprKind::Grouping(g) if g.levels_up == depth => Some(()),
        _ => None,
    })
    .is_some()
}

/// True when the expression has a subquery.
pub(crate) fn has_sublink(expr: &Expr) -> bool {
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
        collation: item.collation,
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
    mut only: bool,
    costs: TargetCosts,
) -> Option<Node<'q>> {
    let query = level.query;
    let scope = level.display;
    if query.set_op.is_none() && !query.distinct.is_empty() {
        node = distinct(level, node, order, only, costs.last)?;
        // `create_ordered_paths` sorts the cheapest path of `DISTINCT`, which can come from a plan that is not the cheapest. Each path of `DISTINCT` has the same rows, so the cheapest sort of them is the sort of the cheapest one.
        only = false;
    }
    let (count, offset) = limit_estimates(query);
    if !query.sort.is_empty() {
        let keys: Vec<SortKey<'q>> =
            query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
        // A column of a set operation is a column of its subquery and never a constant, even when the leftmost `SELECT` gives a constant, so only a key that is the same as a key before it is redundant.
        let keys = if query.set_op.is_some() {
            let mut out: Vec<SortKey<'q>> = Vec::new();
            for key in keys {
                if !out.iter().any(|k| k.item.expr.same(&key.item.expr)) {
                    out.push(key);
                }
            }
            out
        } else {
            upper_canonical(level, keys, |k| &*k.item.expr)
        };
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

/// `F_ARRAY_APPEND`: `array_append`, the transition function of an aggregate whose state is an expanded array.
const ARRAY_APPEND: u32 = 378;

/// The `numTrans` and the `transitionSpace` of the aggregate calls, as `preprocess_aggrefs` and `get_agg_clause_costs` find them. Two calls with the same inputs share a state when they are the same call, or when their aggregates have the same transition function, state type and first state and the final functions do not change the state. A state that is not passed by value adds the `aggtransspace` of its aggregate, `ALLOCSET_SMALL_INITSIZE` for `array_append` or the width of its type, and 16 bytes. An `internal` state adds the `aggtransspace` of its aggregate, or `ALLOCSET_DEFAULT_INITSIZE`.
fn trans_space(aggs: &[&Aggref]) -> (usize, f64) {
    let mut states: Vec<(&Aggref, &builtin::AggregateRow)> = Vec::new();
    let mut space = 0.0;
    for &a in aggs {
        let Some(g) = builtin::aggregate(a.oid) else { continue };
        let same_inputs = |b: &Aggref| {
            b.args == a.args
                && b.filter == a.filter
                && b.order == a.order
                && b.distinct == a.distinct
        };
        let shared = states.iter().any(|(b, h)| {
            same_inputs(b)
                && (b.oid == a.oid
                    || h.transfn == g.transfn
                        && h.transtype == g.transtype
                        && h.initval == g.initval
                        && h.finalmodify != b'w'
                        && g.finalmodify != b'w')
        });
        if shared {
            continue;
        }
        states.push((a, g));
        let by_val = builtin::type_by_oid(g.transtype).is_none_or(|t| t.by_val);
        if !by_val {
            let width = if g.transspace > 0 {
                g.transspace
            } else if g.transfn == ARRAY_APPEND {
                1024
            } else {
                cost::type_width(g.transtype, -1)
            };
            space += f64::from(cost::maxalign(width) + 16);
        } else if g.transtype == rupg_types::oid::INTERNAL {
            space += f64::from(if g.transspace > 0 { g.transspace } else { 8192 });
        }
    }
    (states.len(), space)
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
                scope = Some(level.conflict.unwrap_or_else(|| {
                    self.scopes.push(rupg_plan::Scope {
                        query: update,
                        parent: Some(level.scope),
                        target: None,
                        leaf: None,
                    });
                    self.scopes.len() - 1
                }));
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
                    .chain((c.exclusion != 0).then_some(c.exclusion))
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
            let (plans, named) = self.union_paths(level, op, true);
            // The empty result shows the columns of the operation.
            let scope = match named {
                Some(leaf) => self.leaf_scope(level, leaf),
                None => Some(level.scope),
            };
            (plans, scope)
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
        let mut order = setop::order(query, op);
        // `create_setop_path`: the `SetOp` has the order of its outer side, whose `setop_pathkeys` have no key for a constant column.
        if op.work.is_none() && op.kind != SetKind::Union {
            order.retain(|k| !k.exprs.iter().any(setop::is_const));
        }
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
            SetTree::Op(op) if op.project.is_empty() => {
                let mut node = self.set_node(level, op);
                output::nested(&mut node, &self.leaf_items(level, op));
                node
            }
            SetTree::Op(op) => {
                // `recurse_set_operations`: a `Result` above the plan casts the columns to the types of the operation above it. `is_dummy_rel` finds the operation empty through the projection.
                let mut child = self.set_node(level, op);
                output::nested(&mut child, &self.leaf_items(level, op));
                if setop::is_dummy(&child) {
                    return child;
                }
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

    /// The members of a `UNION`, with the members of each `UNION` below it that PostgreSQL makes one `Append` with, as `recurse_union_children`. `named` gets the `SELECT` that gives the names of each member, with the number of groups of the rows of a `SELECT`. `generate_union_paths` leaves out a member that is empty, as `is_dummy_rel` finds it, and its relations then take no names.
    fn union_members(
        &mut self,
        level: &Level<'q>,
        top: &'q SetOp,
        tree: &'q SetTree,
        out: &mut Vec<Node<'q>>,
        named: &mut Vec<(usize, Option<f64>)>,
        ordered: &mut Option<Vec<Node<'q>>>,
    ) {
        if let SetTree::Op(op) = tree
            && op.kind == SetKind::Union
            && op.work.is_none()
            && (op.all == top.all || op.all)
            && op.columns == top.columns
        {
            self.union_members(level, top, &op.left, out, named, ordered);
            self.union_members(level, top, &op.right, out, named, ordered);
            return;
        }
        let start = self.current.len();
        // `build_setop_child_paths` adds the plans in the order of the columns only for a `SELECT` of `UNION` without `ALL`.
        let (node, sorted, groups) = if top.all || !matches!(tree, SetTree::Leaf(_)) {
            (self.set_tree(level, tree), None, None)
        } else {
            let (node, sorted, groups) = self.union_leaf(level, top, tree);
            (node, Some(sorted), Some(groups))
        };
        if setop::is_dummy(&node) {
            self.current.truncate(start);
            return;
        }
        match (ordered.as_mut(), sorted.flatten()) {
            (Some(list), Some(sorted)) => list.push(sorted),
            _ => *ordered = None,
        }
        out.push(node);
        named.push((tree.leftmost(), groups));
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
            SetKind::Union => match self.union_paths(level, op, false).0.into_iter().next() {
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

    /// `generate_union_paths`: the plans of a `UNION` that `add_path` keeps, the cheapest first, and the `SELECT` that gives the names of the columns, the first member that is not empty. `UNION ALL` is an `Append` of the plans of its members. `UNION` hashes the rows of the `Append`, or sorts them for a `Unique`, or, with true, reads the plans of its members in the order of the columns with a `Merge Append` below the `Unique`. The two plans with the `Unique` give the rows in the order of the columns. With all the members empty, the plan is an empty `Result` and no `SELECT` gives the names. `set_plan_refs` takes out an `Append` with one member.
    fn union_paths(
        &mut self,
        level: &Level<'q>,
        op: &'q SetOp,
        top: bool,
    ) -> (Vec<(Node<'q>, bool)>, Option<usize>) {
        let scope = level.scope;
        let columns = op.columns.len();
        let mut members = Vec::new();
        let mut named = Vec::new();
        let mut ordered = Some(Vec::new());
        self.union_members(level, op, &op.left, &mut members, &mut named, &mut ordered);
        self.union_members(level, op, &op.right, &mut members, &mut named, &mut ordered);
        let Some(&(first, first_groups)) = named.first() else {
            return (vec![(setop::dummy_op(level, op), false)], None);
        };
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
        let single = members.len() == 1;
        let append = if single {
            members.remove(0)
        } else {
            let mut append = Node::new(Kind::Append, scope, members);
            append.cost = Cost { startup, total, rows, width };
            append
        };
        if op.all {
            return (vec![(append, false)], Some(first));
        }
        let c = append.cost;
        // The number of groups is the number of rows, the worst case, unless one `SELECT` is left, whose groups `estimate_num_groups` finds.
        let rows = match (named.as_slice(), first_groups) {
            ([_], Some(groups)) => groups,
            _ => rows,
        };
        let hashable = op.groups.iter().all(|g| g.hashable);
        let sortable = op.groups.iter().all(|g| g.operator != 0);
        let per_row = cost::CPU_OPERATOR * columns as f64;
        // `cost_agg` of a set operation: the query of the operation has no aggregate calls.
        let entry = cost::hashagg_entry_size(0, c.width, 0.0);
        let (spill_startup, spill_total) = cost::hashagg_spill(rows, entry, c.rows, c.width);
        let hash_startup = c.total + per_row * c.rows;
        let hash_total = hash_startup + cost::CPU_TUPLE * rows + spill_total;
        let hash_startup = hash_startup + spill_startup;
        // With no column, the rows of the `Append` are in the order of the keys, which are none, as `pathkeys_contained_in` finds, so the `Unique` needs no `Sort`.
        let presorted = op.groups.is_empty();
        let (sort_startup, sort_total) = if presorted {
            (c.startup, c.total)
        } else {
            cost::sort((c.startup, c.total), c.rows, c.width, None)
        };
        let unique_total = sort_total + per_row * c.rows;
        let keys = self.leaf_items_of(level, first);
        // The keys show with the names of the first `SELECT`.
        let scope = self.leaf_scope(level, first).unwrap_or(scope);
        let sort_keys: Vec<SortKey<'q>> = keys
            .iter()
            .zip(&op.groups)
            .map(|(item, g)| SortKey {
                item: item.clone(),
                desc: false,
                using: None,
                nulls_first: g.nulls_first,
                collation: 0,
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
                (below + u32::from(!enable.sort && !presorted), sort_startup, unique_total),
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
            let mut list = list;
            if list.len() == 1 {
                let only = list.remove(0);
                let o = only.cost;
                let mut node = Node::new(Kind::Unique, scope, vec![only]);
                node.cost =
                    Cost { startup: o.startup, total: o.total + per_row * tuples, rows, width };
                return node;
            }
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
                    // `show_agg_keys` shows the keys as the targets of the plan below, which is the plan of the member when one is left.
                    let keys = if single {
                        keys.iter().map(|k| Item { below: false, ..k.clone() }).collect()
                    } else {
                        keys.clone()
                    };
                    let table = Table {
                        of: Grouped::SetOp(op, false),
                        groups: rows,
                        keys: (0..columns).collect(),
                    };
                    let kind = Kind::Aggregate {
                        strategy,
                        keys,
                        sets: Vec::new(),
                        unique: false,
                        table: Some(table),
                    };
                    let mut node = Node::new(kind, scope, vec![append.clone()]);
                    node.cost = Cost { startup: hash_startup, total: hash_total, rows, width };
                    node.disabled = !enable.hashagg;
                    out.push((node, false));
                }
                1 => {
                    let sorted = if presorted {
                        append.clone()
                    } else {
                        let mut sorted =
                            Node::new(Kind::Sort(sort_keys.clone()), scope, vec![append.clone()]);
                        sorted.cost = Cost { startup: sort_startup, total: sort_total, ..c };
                        sorted.disabled = !enable.sort;
                        sorted
                    };
                    let mut node = Node::new(Kind::Unique, scope, vec![sorted]);
                    node.cost = Cost { startup: sort_startup, total: unique_total, rows, width };
                    out.push((node, true));
                }
                _ => out.extend(merged.take().map(|node| (node, true))),
            }
        }
        (out, Some(first))
    }

    /// The scope of the leftmost `SELECT` of a set operation.
    pub(crate) fn leftmost_scope(&self, level: &Level<'q>, op: &'q SetOp) -> Option<usize> {
        self.leaf_scope(level, op.left.leftmost())
    }

    /// The scope of the `SELECT` with this index.
    fn leaf_scope(&self, level: &Level<'q>, leaf: usize) -> Option<usize> {
        let sub = level.query.relations.get(leaf)?.subquery.as_deref()?;
        self.scopes.iter().position(|s| std::ptr::eq(s.query, sub))
    }

    /// The columns of the leftmost `SELECT` of a set operation, as the keys of the node that finds the equal rows.
    pub(crate) fn leaf_items(&self, level: &Level<'q>, op: &'q SetOp) -> Vec<Item<'q>> {
        self.leaf_items_of(level, op.left.leftmost())
    }

    /// The columns of the `SELECT` with this index.
    fn leaf_items_of(&self, level: &Level<'q>, leaf: usize) -> Vec<Item<'q>> {
        match level.query.relations.get(leaf).and_then(|r| r.subquery.as_deref()) {
            Some(sub) => {
                sub.targets.iter().filter(|t| !t.junk).map(|t| Item::below(&t.expr)).collect()
            }
            None => Vec::new(),
        }
    }
}

/// True when the aggregate call has `ORDER BY` or `DISTINCT`, as `numOrderedAggs` counts it.
fn is_ordered(agg: &Aggref) -> bool {
    !agg.order.is_empty() || !agg.distinct.is_empty()
}

/// `adjust_group_pathkeys_for_groupagg`: the keys of the groups and then the keys that suit the most ordered aggregate calls, which the input of the `Agg` gets sorted on. A pass takes the keys of the first call that is left and the stronger keys of each call whose keys are a prefix of them or have them as a prefix. The pass covers these calls, and the next pass takes the calls that are left, while they are more than the calls of the best pass. A call of an ordered-set aggregate, a call with a key that is volatile and a call with `FILTER` whose arguments are not all columns or constants take no part. The result is empty when no call takes part.
fn ordered_keys<'q>(
    aggs: &[&'q Aggref],
    group: &[SortKey<'q>],
    stats: &from::Stats,
) -> Vec<SortKey<'q>> {
    let simple = |e: &Expr| {
        let mut e = e;
        while let ExprKind::Relabel(inner, _) = &e.kind {
            e = inner;
        }
        matches!(e.kind, ExprKind::Var(_) | ExprKind::Const(_))
    };
    // `find_ec_member_matching_expr`: the `Sort` sorts on a column of its input whose type a `RelabelType` changes, and shows the column.
    let column = |e: &'q Expr| {
        let mut inner = e;
        while let ExprKind::Relabel(arg, _) = &inner.kind {
            inner = arg;
        }
        if matches!(inner.kind, ExprKind::Var(_)) { inner } else { e }
    };
    let mut seen: Vec<&Aggref> = Vec::new();
    let mut candidates: Vec<Vec<SortKey<'q>>> = Vec::new();
    for &agg in aggs {
        if seen.contains(&agg) {
            continue;
        }
        seen.push(agg);
        if agg.kind != b'n' || !is_ordered(agg) {
            continue;
        }
        if agg.filter.is_some() && !agg.args.iter().all(simple) {
            continue;
        }
        let items = if agg.distinct.is_empty() { &agg.order } else { &agg.distinct };
        let own: Vec<SortKey<'q>> = items
            .iter()
            .filter_map(|s| agg.args.get(s.target).map(|e| sort_key(column(e), s)))
            .collect();
        if own.iter().any(|k| exprs::volatile(&k.item.expr)) {
            continue;
        }
        let mut keys = group.to_vec();
        keys.extend(own);
        candidates.push(stats.canonical(keys, |k| &*k.item.expr));
    }
    let same = |a: &SortKey<'_>, b: &SortKey<'_>| {
        a.item.expr.same(&b.item.expr)
            && a.desc == b.desc
            && a.nulls_first == b.nulls_first
            && a.using == b.using
    };
    let mut unprocessed: Vec<usize> = (0..candidates.len()).collect();
    let mut best: (usize, Vec<SortKey<'q>>) = (0, Vec::new());
    while unprocessed.len() > best.0 {
        let mut current: Vec<SortKey<'q>> = Vec::new();
        let mut covered = Vec::new();
        for &at in &unprocessed {
            let keys = &candidates[at];
            // `compare_pathkeys`: the keys of the call suit the pass when one list is a prefix of the other.
            let prefix = current.iter().zip(keys).all(|(a, b)| same(a, b));
            if current.is_empty() || prefix {
                if keys.len() > current.len() {
                    current.clone_from(keys);
                }
                covered.push(at);
            }
        }
        unprocessed.retain(|at| !covered.contains(at));
        if covered.len() > best.0 {
            best = (covered.len(), current);
        }
    }
    best.1
}

/// The keys of the ordered aggregate calls of the targets and of `HAVING` of a query with no groups, as [`ordered_keys`] finds them.
pub(crate) fn aggregate_order<'q>(query: &'q Query, stats: &from::Stats) -> Vec<SortKey<'q>> {
    let mut exprs_of: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    let (_, having) = having_split(query);
    exprs_of.extend(having.iter().copied());
    let mut aggs = Vec::new();
    exprs::aggregates(&exprs_of, &mut aggs);
    ordered_keys(&aggs, &[], stats)
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
    let has_ordered = aggs.iter().any(|a| is_ordered(a));
    let presort = crate::enable().presorted_aggregate;
    let selectivity = cost::list_selectivity(having.iter().copied());
    let (having_startup, having_cost) = level.stats.qual_costs(having.iter().copied());
    let filter: Vec<Item<'q>> = having.iter().map(|h| Item::of(h)).collect();
    let c = input.cost;
    let n = c.rows;
    if !query.grouping_sets.is_empty() {
        let (trans, space) = trans_space(&aggs);
        let having = (having_startup, having_cost);
        let costs = AggCosts { units, finals, having, selectivity, trans, space };
        return grouping_sets(level, input, order, costs, filter, has_ordered, only);
    }
    if query.group.is_empty() {
        let ordered = if presort { ordered_keys(&aggs, &[], &level.stats) } else { Vec::new() };
        let input = sort_if(level, input, ordered, order, None, only)?;
        let c = input.cost;
        // `cost_agg` adds the costs in this order, which the rounding of the sums follows.
        let startup = c.total + cost::CPU_OPERATOR * units * n + finals;
        let total = startup + cost::CPU_TUPLE + having_startup + having_cost;
        let mut node = Node::new(
            Kind::Aggregate {
                strategy: Strategy::Plain,
                keys: Vec::new(),
                sets: Vec::new(),
                unique: false,
                table: None,
            },
            scope,
            vec![input],
        );
        node.filter = filter;
        let startup = startup + having_startup;
        node.cost = Cost { startup, total, rows: cost::clamp_rows(selectivity), width: 0 };
        order.clear();
        return Some(node);
    }
    let (group, hashable, targets) = group_keys(level);
    let nkeys = group.len() as f64;
    let group_exprs: Vec<&Expr> = group.iter().map(|k| &*k.item.expr).collect();
    let groups = level.stats.groups(&group_exprs, n);
    let rows = cost::clamp_rows(groups * selectivity);
    let hashable = hashable && !group.is_empty() && !has_ordered;
    // `cost_agg`: the transition cost, then the cost of the keys, which the rounding of the sums follows. `GROUP BY` with no aggregate is a `Group` node, which `cost_group` gives no cost for each group. A `GROUPING` call sets `hasAggs`, and so the node is an `Agg`.
    let group_node = aggs.is_empty()
        && !level.query.targets.iter().any(|t| has_grouping(&t.expr))
        && !level.query.having.as_ref().is_some_and(has_grouping);
    // The sorted strategy: a `Sort` on the keys of the groups, and on the keys of an ordered call after them, as `adjust_group_pathkeys_for_groupagg`, with the start cost and the total cost of the sorted input and of the `Agg`, and the total cost with the sort of `ORDER BY` above it.
    let sorted_plan = |group: Vec<SortKey<'q>>| {
        let ordered = if presort { ordered_keys(&aggs, &group, &level.stats) } else { Vec::new() };
        let sort_keys = if ordered.is_empty() { group.clone() } else { ordered };
        let (is_sorted, count) = presorted(order, &sort_keys);
        let (input, out_order) = if is_sorted {
            (c, order.clone())
        } else {
            let (startup, total) = sort_cost(level, c, &sort_keys, count, None);
            (Cost { startup, total, ..c }, sorted_order(&sort_keys))
        };
        let total = input.total
            + cost::CPU_OPERATOR * units * n
            + cost::CPU_OPERATOR * nkeys * n
            + finals * groups
            + if group_node { 0.0 } else { cost::CPU_TUPLE * groups }
            + having_startup
            + having_cost * groups;
        let after = after_sort(
            level,
            &out_order[..out_order.len().min(group.len())],
            input.startup,
            total,
            rows,
            c.width,
        );
        SortedPlan { group, sort_keys, is_sorted, count, input, out_order, total, after }
    };
    // The hash table keeps the order of the keys of the query.
    let hash_keys: Vec<Item<'q>> = group.iter().map(|k| k.item.clone()).collect();
    let mut plan = sorted_plan(group);
    // `get_useful_group_keys_orderings`: the keys of the groups also in the order of the input as far as it goes, as `group_keys_reorder_by_pathkeys` puts them, and the cheaper of the two orders wins.
    if crate::enable().group_by_reordering
        && let Some(other) = reorder(order, &plan.group, &plan.sort_keys)
    {
        let other = sorted_plan(other);
        if other.after < plan.after && (!only || other.is_sorted || other.count > 0) {
            plan = other;
        }
    }
    let SortedPlan {
        group,
        sort_keys,
        is_sorted,
        count,
        input: sorted_input,
        out_order: sorted_order,
        total: sorted_total,
        ..
    } = plan;
    if only && !is_sorted && count == 0 {
        return None;
    }
    let keys: Vec<Item<'q>> = group.iter().map(|k| k.item.clone()).collect();
    let (trans, space) = trans_space(&aggs);
    let entry = cost::hashagg_entry_size(trans, c.width, space);
    let (spill_startup, spill_total) = cost::hashagg_spill(groups, entry, n, c.width);
    let hash_startup = c.total + cost::CPU_OPERATOR * units * n + cost::CPU_OPERATOR * nkeys * n;
    let hash_total = hash_startup + finals * groups + cost::CPU_TUPLE * groups + spill_total;
    let hash_startup = hash_startup + spill_startup;
    let hash_total = hash_total + having_startup + having_cost * groups;
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
            Kind::Aggregate {
                strategy: Strategy::Hashed,
                keys: hash_keys,
                sets: Vec::new(),
                unique: false,
                table: Some(Table { of: Grouped::Group(query), groups, keys: targets }),
            },
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
    let mut node = Node::new(
        Kind::Aggregate { strategy, keys, sets: Vec::new(), unique: false, table: None },
        scope,
        vec![input],
    );
    node.filter = filter;
    let startup = sorted_input.startup + having_startup;
    node.cost = Cost { startup, total: sorted_total, rows, width: 0 };
    Some(node)
}

/// A plan of the sorted strategy of an `Agg` for one order of the keys of the groups.
struct SortedPlan<'q> {
    group: Vec<SortKey<'q>>,
    sort_keys: Vec<SortKey<'q>>,
    is_sorted: bool,
    count: usize,
    input: Cost,
    out_order: Vec<OrderKey>,
    total: f64,
    after: f64,
}

/// The total cost of the groups with the sort of `ORDER BY` above them when the order of the groups is not its order, as `create_ordered_paths` adds it.
fn after_sort(
    level: &Level<'_>,
    order: &[OrderKey],
    startup: f64,
    total: f64,
    rows: f64,
    width: i32,
) -> f64 {
    let query = level.query;
    if query.sort.is_empty() {
        return total;
    }
    let keys: Vec<SortKey<'_>> =
        query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
    let keys = level.stats.canonical(keys, |k| &*k.item.expr);
    let (sorted, count) = presorted(order, &keys);
    if sorted {
        return total;
    }
    let (limit, offset) = limit_estimates(query);
    let bound = (limit > 0.0 && offset >= 0.0).then_some(limit + offset);
    let c = Cost { startup, total, rows, width };
    sort_cost(level, c, &keys, count, bound).1
}

/// `group_keys_reorder_by_pathkeys`: the keys of the groups with the keys that the rows are in the order of first, in that order, and then the other keys, or `None` when no key is in the order of the rows, when the rows are in the order of the first keys already or when the order is the same. With `enable_incremental_sort` off, the rows must be in the order of all the keys.
fn reorder<'q>(
    order: &[OrderKey],
    group: &[SortKey<'q>],
    sort_keys: &[SortKey<'q>],
) -> Option<Vec<SortKey<'q>>> {
    if order.is_empty() || group.is_empty() {
        return None;
    }
    // `pathkeys_contained_in`: the rows in the order of the first keys need no other order.
    if order.len() <= sort_keys.len() && presorted(order, sort_keys).1 >= order.len() {
        return None;
    }
    let mut taken: Vec<usize> = Vec::new();
    for o in order.iter().take(group.len()) {
        let Some(at) = group.iter().enumerate().position(|(i, k)| {
            !taken.contains(&i) && k.using.is_none() && o.sorts(&k.item.expr, k.desc, k.nulls_first)
        }) else {
            break;
        };
        taken.push(at);
    }
    let n = taken.len();
    if n == 0
        || !crate::enable().incremental_sort && n < group.len()
        || taken.iter().enumerate().all(|(i, &t)| i == t)
    {
        return None;
    }
    let mut out: Vec<SortKey<'q>> = taken.iter().map(|&i| group[i].clone()).collect();
    out.extend(
        group.iter().enumerate().filter(|(i, _)| !taken.contains(i)).map(|(_, k)| k.clone()),
    );
    Some(out)
}

/// The keys of `GROUP BY` as the keys of a sort, in the order of `ORDER BY` when its items are items of `GROUP BY`, as `preprocess_groupclause`, and true when a hash table can hold the groups. A column that the other columns of its relation give, as `remove_useless_groupby_columns` finds it, and a key that is redundant, as `make_pathkeys_for_sortclauses_extended` finds it, are not keys.
fn group_keys<'q>(level: &Level<'q>) -> (Vec<SortKey<'q>>, bool, Vec<usize>) {
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
    let exprs: Vec<(&Expr, u32)> =
        items.iter().map(|s| (level.target(s.target), s.equal)).collect();
    let surplus = level.stats.surplus(&exprs);
    let items: Vec<&SortGroup> =
        items.into_iter().zip(surplus).filter(|(_, surplus)| !surplus).map(|(s, _)| s).collect();
    let items = level.stats.canonical(items, |s| level.target(s.target));
    let hashable = items.iter().all(|s| in_group(s).is_some_and(|g| g.hashable));
    let targets = items.iter().map(|s| s.target).collect();
    (items.into_iter().map(|s| sort_key(level.target(s.target), s)).collect(), hashable, targets)
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
    /// `numTrans`: the transition states of the calls.
    trans: usize,
    /// `transitionSpace`: the bytes of the states that are not passed by value.
    space: f64,
}

/// `cost_agg`: the start cost, the total cost and the rows of an `Agg` of `strategy` with `cols` keys and `groups` groups, which reads `rows` rows of a width with the start cost and the total cost `input`. `Mixed` has the costs of `Sorted`. A hash table that does not fit in `hash_mem` adds the cost of the rows that go to disk and back.
fn cost_agg(
    strategy: Strategy,
    costs: AggCosts,
    cols: usize,
    groups: f64,
    input: (f64, f64),
    rows: f64,
    width: i32,
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
    if matches!(strategy, Strategy::Hashed | Strategy::Mixed) {
        let entry = cost::hashagg_entry_size(costs.trans, width, costs.space);
        let (spill_startup, spill_total) = cost::hashagg_spill(groups, entry, rows, width);
        startup += spill_startup;
        total += spill_total;
    }
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

/// `DiscreteKnapsack`: the items of `weights` that fit in `capacity` with the most items, as the dynamic program of `knapsack.c` picks them, each item of value 1.
fn knapsack(capacity: usize, weights: &[usize]) -> Vec<bool> {
    let mut values = vec![0.0; capacity + 1];
    let mut sets = vec![vec![false; weights.len()]; capacity + 1];
    for (i, &weight) in weights.iter().enumerate() {
        if weight > capacity {
            continue;
        }
        for j in (weight..=capacity).rev() {
            let other = j - weight;
            if values[j] <= values[other] + 1.0 {
                if j != other {
                    sets[j] = sets[other].clone();
                }
                sets[j][i] = true;
                values[j] = values[other] + 1.0;
            }
        }
    }
    sets.swap_remove(capacity)
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
        input.width,
    );
    let mut first_sort = first.hashed;
    for rollup in &rollups[1..] {
        let cols = rollup.sets[0].len();
        let (_, t, r) = if rollup.hashed || first_sort {
            first_sort &= rollup.hashed;
            disabled += u32::from(rollup.hashed && !enable.hashagg);
            let strategy = if rollup.hashed { Strategy::Hashed } else { Strategy::Sorted };
            cost_agg(strategy, costs, cols, rollup.groups(), (0.0, 0.0), n, input.width)
        } else {
            disabled += u32::from(!enable.sort);
            let sort = cost::sort((0.0, 0.0), n, input.width, None);
            cost_agg(Strategy::Sorted, costs, cols, rollup.groups(), sort, n, input.width)
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
    // `mark_nullable_by_grouping`: a constant that a set does not have is in a `PlaceHolderVar`, so it is not redundant and the sort keeps it. A column stays a column of the sort, as its `varnullingrels` show only above the `Agg`.
    let all_sets = || rollups.iter().chain(&hash_sets).flat_map(|r| &r.sets);
    let first_targets = rollups.first().map(|r| r.sets[0].clone()).unwrap_or_default();
    let first_keys: Vec<(usize, SortKey<'q>)> = first_targets
        .iter()
        .filter_map(|&t| group_of(t).map(|g| (t, sort_key(level.target(t), g))))
        .collect();
    let first_keys = level.stats.canonical_keeping(
        first_keys,
        |(_, k)| &*k.item.expr,
        |(t, k)| {
            matches!(k.item.expr.kind, ExprKind::Const(_)) && !all_sets().all(|s| s.contains(t))
        },
    );
    // `create_groupingsets_path`: with one rollup over sorted rows, the rows come out in the order of `group_pathkeys`. Above the `Agg`, a key that a set does not have is a column with `varnullingrels`, so the order holds only up to the first such key.
    let stable = first_keys.iter().take_while(|(t, _)| all_sets().all(|s| s.contains(t))).count();
    let first_keys: Vec<SortKey<'q>> = first_keys.into_iter().map(|(_, k)| k).collect();
    let group_order = sorted_order(&first_keys[..stable]);
    let (is_sorted, count) = presorted(order, &first_keys);
    let any_hashable = rollups.iter().any(|r| r.hashable) || !hash_sets.is_empty();
    let can_hash = !has_ordered && any_hashable;
    // `estimate_hashagg_tablesize`: the bytes of the hash tables of a number of groups.
    let entry = cost::hashagg_entry_size(costs.trans, c.width, costs.space);
    let table_size = |groups: f64| entry * groups;
    let hash_groups: f64 = hash_sets.iter().map(RollupSets::groups).sum();
    let all_groups = hash_groups + rollups.iter().map(RollupSets::groups).sum::<f64>();
    // The candidates in the order that `add_paths_to_grouping_rel` adds them, with true when the input needs the order of the first rollup.
    let mut candidates: Vec<(Strategy, Vec<RollupSets>, bool)> = Vec::new();
    if !rollups.is_empty() && (is_sorted || !only || count > 0) {
        if can_hash {
            let mut out = Vec::new();
            let mut hashed = hash_sets.clone();
            // The rollups after the first one that the knapsack of `hash_mem`, less the hash tables of the sets that have no sort, picks to hash. Each rollup has the same value, one sort less.
            let space = cost::hash_mem() - table_size(hash_groups);
            if space > 0.0 && rollups.len() > 1 {
                let scale = (space / (20.0 * rollups.len() as f64)).max(1.0);
                let capacity = (space / scale).floor();
                let weights: Vec<usize> = rollups[1..]
                    .iter()
                    .filter(|r| r.hashable)
                    .map(|r| (table_size(r.groups()) / scale).floor().min(capacity + 1.0) as usize)
                    .collect();
                let picked = if weights.is_empty() {
                    Vec::new()
                } else {
                    knapsack(capacity as usize, &weights)
                };
                if picked.contains(&true) {
                    out.push(rollups[0].clone());
                    let mut at = 0;
                    for rollup in &rollups[1..] {
                        let pick = rollup.hashable && picked[at];
                        at += usize::from(rollup.hashable);
                        if pick {
                            let sets = rollup.sets.iter().zip(&rollup.groups);
                            hashed.extend(sets.map(|(s, g)| RollupSets::hash(s, *g)));
                        } else {
                            out.push(rollup.clone());
                        }
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
        // The hash tables must fit in `hash_mem`, unless no set has a sort.
        let exclude = unhashed.as_ref().map_or(0.0, RollupSets::groups);
        let fits =
            rollups.is_empty() || table_size(all_groups - exclude) <= cost::hash_mem().floor();
        if fits && rest.iter().all(|r| r.hashable) {
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
        plan_rollups.push(Rollup {
            hashed: rollup.hashed,
            sort,
            sets,
            targets: rollup.sets.clone(),
            groups: rollup.groups.clone(),
        });
    }
    *order = if matches!(strategy, Strategy::Sorted) && plan_rollups.len() == 1 {
        group_order
    } else {
        Vec::new()
    };
    let mut node = Node::new(
        Kind::Aggregate {
            strategy,
            keys,
            sets: plan_rollups,
            unique: false,
            table: Some(Table { of: Grouped::Group(query), groups: cost.rows, keys: Vec::new() }),
        },
        level.scope,
        vec![input],
    );
    node.filter = filter;
    node.disabled = disabled > 0;
    node.cost = cost;
    Some(node)
}

/// The window calls of the query for each window, by `winref`, after `optimize_window_clauses`.
fn window_calls(query: &Query) -> Vec<Vec<&WindowFunc>> {
    optimize_windows(query).calls
}

/// The windows of a query after `optimize_window_clauses`.
struct Windows<'q> {
    /// The calls of each window.
    calls: Vec<Vec<&'q WindowFunc>>,
    /// `frameOptions` of each window.
    options: Vec<i32>,
    /// For each window, the other windows whose calls it took.
    merged: Vec<Vec<usize>>,
}

/// `optimize_window_clauses`: the support functions of `row_number` and the other functions of the rank of a row give the frame `ROWS UNBOUNDED PRECEDING` to a window when all its calls are of these functions. When the new frame makes the window the same as another window, the calls of the window go to the other window.
fn optimize_windows(query: &Query) -> Windows<'_> {
    let mut calls = query_window_calls(query);
    let mut options: Vec<i32> = query.windows.iter().map(|w| w.frame.options).collect();
    let mut merged = vec![Vec::new(); query.windows.len()];
    let rank = |f: &&WindowFunc| {
        builtin::proc_by_oid(f.oid).is_some_and(|p| {
            matches!(
                p.name,
                "row_number" | "rank" | "dense_rank" | "percent_rank" | "ntile" | "cume_dist"
            )
        })
    };
    let same_offset = |a: &Option<Expr>, b: &Option<Expr>| match (a, b) {
        (Some(a), Some(b)) => a.same(b),
        (a, b) => a.is_none() && b.is_none(),
    };
    for w in 0..query.windows.len() {
        if calls[w].is_empty() || !calls[w].iter().all(rank) || options[w] == FRAME_ROWS_CURRENT {
            continue;
        }
        options[w] = FRAME_ROWS_CURRENT;
        let clause = &query.windows[w];
        let existing = (0..query.windows.len()).find(|&e| {
            let other = &query.windows[e];
            e != w
                && other.partition == clause.partition
                && other.order == clause.order
                && options[e] == options[w]
                && same_offset(&other.frame.start, &clause.frame.start)
                && same_offset(&other.frame.end, &clause.frame.end)
        });
        if let Some(e) = existing {
            let moved = std::mem::take(&mut calls[w]);
            calls[e].extend(moved);
            let moved = std::mem::take(&mut merged[w]);
            merged[e].push(w);
            merged[e].extend(moved);
        }
    }
    Windows { calls, options, merged }
}

/// The window calls of the query for each window, by `winref`.
fn query_window_calls(query: &Query) -> Vec<Vec<&WindowFunc>> {
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
    let items = upper_canonical(level, items, |(_, item)| level.target(item.target));
    items.into_iter().map(|(i, _)| i).collect()
}

/// The keys of the sort of a window: the items of `PARTITION BY` at the places `partition`, then the items of `ORDER BY`, without the keys that are redundant, as `make_pathkeys_for_window`.
fn window_keys<'q>(level: &Level<'q>, window: usize, partition: &[usize]) -> Vec<SortKey<'q>> {
    let clause = &level.query.windows[window];
    let items = partition.iter().map(|&i| &clause.partition[i]).chain(&clause.order);
    let keys = items.map(|item| sort_key(level.target(item.target), item)).collect();
    upper_canonical(level, keys, |k| &*k.item.expr)
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
    let Windows { calls, options, mut merged } = optimize_windows(level.query);
    let active = active_windows(level, &calls);
    // `create_one_window_path`: the run conditions of the windows below the top window are the conditions of the top window.
    let mut top_filter: Vec<&'q Expr> = Vec::new();
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
        let options = options[window];
        let rows_frame = options != clause.frame.options;
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
        let merged = std::mem::take(&mut merged[window]);
        let run: Vec<&'q Expr> = std::iter::once(window)
            .chain(merged.iter().copied())
            .flat_map(|w| &level.query.windows[w].run)
            .collect();
        let top = at + 1 == count;
        if !top {
            top_filter.extend(&run);
        }
        let run = run.into_iter().cloned().collect();
        let kind = Kind::WindowAgg { name, window, rows_frame, partition, merged, run };
        node = Node::new(kind, level.scope, vec![node]);
        if top {
            node.filter = top_filter.drain(..).map(Item::of).collect();
        }
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
        upper_canonical(level, query.distinct.iter().collect(), |d| level.target(d.target));
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
    let mut keys = upper_canonical(level, keys, |k| &*k.item.expr);
    // `DISTINCT ON` sorts on all the items of `ORDER BY` when it has more keys, as `create_final_distinct_paths`.
    if query.distinct_on {
        let sort: Vec<SortKey<'q>> =
            query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
        let sort = upper_canonical(level, sort, |k| &*k.item.expr);
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
    let keys = useful_distinct_keys(level, keys, order, &distinct_exprs, only, c, groups);
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
    // `cost_agg` counts the transition states of the aggregate calls of the query, with no `transitionSpace`.
    let mut exprs_of: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    exprs_of.extend(query.having.as_ref());
    let mut aggs = Vec::new();
    exprs::aggregates(&exprs_of, &mut aggs);
    let entry = cost::hashagg_entry_size(trans_space(&aggs).0, c.width, 0.0);
    let (spill_startup, spill_total) = cost::hashagg_spill(groups, entry, c.rows, c.width);
    let hash_startup = c.total + cost::CPU_OPERATOR * c.rows * ncols as f64;
    let hash_total =
        hash_startup + cost::CPU_TUPLE * groups + spill_total + target.0 + target.1 * groups;
    let hash_startup = hash_startup + spill_startup + target.0;
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
            Kind::Aggregate {
                strategy: Strategy::Hashed,
                keys: items,
                sets: Vec::new(),
                unique: false,
                table: Some(Table {
                    of: Grouped::Distinct(query),
                    groups,
                    keys: processed.iter().map(|d| d.target).collect(),
                }),
            },
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

/// `get_useful_pathkeys_for_distinct`: the keys of `DISTINCT` in the order of the rows of the input as far as it goes, then the other keys. With `DISTINCT ON`, each key of the order must also be a key of `DISTINCT ON`. Of these keys and the keys in the order of the query, the result is the one whose `Unique`, with the sort of `ORDER BY` above it, costs less, as `add_path` and `create_ordered_paths` choose.
fn useful_distinct_keys<'q>(
    level: &Level<'q>,
    keys: Vec<SortKey<'q>>,
    order: &[OrderKey],
    distinct: &[&Expr],
    only: bool,
    c: Cost,
    groups: f64,
) -> Vec<SortKey<'q>> {
    let enable = crate::enable();
    if !enable.distinct_reordering {
        return keys;
    }
    let query = level.query;
    let mut used = vec![false; keys.len()];
    let mut out: Vec<usize> = Vec::new();
    for o in order {
        let Some(i) = (0..keys.len()).find(|&i| {
            let k = &keys[i];
            !used[i] && k.using.is_none() && o.sorts(&k.item.expr, k.desc, k.nulls_first)
        }) else {
            break;
        };
        if query.distinct_on && !distinct.iter().any(|e| e.same(&keys[i].item.expr)) {
            break;
        }
        used[i] = true;
        out.push(i);
    }
    // Without `Incremental Sort`, keys that the rows are in the order of only in part are of no use.
    if out.is_empty() || out.len() < keys.len() && !enable.incremental_sort {
        return keys;
    }
    out.extend((0..keys.len()).filter(|&i| !used[i]));
    if out.iter().enumerate().all(|(at, &i)| at == i) {
        return keys;
    }
    let useful: Vec<SortKey<'q>> = out.iter().map(|&i| keys[i].clone()).collect();
    let sort: Vec<SortKey<'q>> =
        query.sort.iter().map(|s| sort_key(level.target(s.target), s)).collect();
    let sort = upper_canonical(level, sort, |k| &*k.item.expr);
    let total = |cand: &[SortKey<'q>]| {
        let (sorted, count) = presorted(order, cand);
        if only && !sorted && count == 0 {
            return f64::INFINITY;
        }
        let ncols = distinct.len() as f64;
        let total = if sorted { c.total } else { sort_cost(level, c, cand, count, None).1 }
            + cost::CPU_OPERATOR * c.rows * ncols;
        if sort.is_empty() {
            return total;
        }
        let after = if sorted { order.to_vec() } else { sorted_order(cand) };
        let (sorted, count) = presorted(&after, &sort);
        if sorted {
            return total;
        }
        let above = Cost { startup: total, total, rows: groups, width: c.width };
        sort_cost(level, above, &sort, count, None).1
    };
    if total(&useful) < total(&keys) { useful } else { keys }
}

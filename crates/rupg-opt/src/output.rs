//! The target list of each node of a query and the width of its rows, as `create_plan` makes them with `build_path_tlist` and `use_physical_tlist`.
//!
//! The width of a node is the width of the values that the nodes above it read, as the width of the `PathTarget` of its path. A scan below a join or an `Agg` gives all the columns of its relation, as a physical target list, but its width counts only the columns that the nodes above read.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rupg_analyze::{
    Expr, ExprKind, FromItem, MERGE_ACTION, MatchKind, Query, RETURNING_NEW, RETURNING_OLD,
    TABLE_OID_ATTNUM, Target, Var, Write,
};
use rupg_plan::{Item, Kind, Node, Operation, Replaced, Split, TARGET};
use rupg_types::oid;

use crate::{Level, cost, exprs, from, parallel, upper};

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`.
const CTID_ATTNUM: i16 = -1;

/// Sets the target lists and the widths of the nodes of the plan of a query level.
pub(crate) fn level<'q>(level: &Level<'q>, node: &mut Node<'q>) {
    let want = if level.no_targets {
        Vec::new()
    } else {
        let targets: Vec<Item<'q>> = match &level.query.minmax {
            Some(minmax) if replaced(level, node) => match minmax.upper.as_deref() {
                Some(upper) => upper.targets.iter().map(|t| Item::of(&t.expr)).collect(),
                None => minmax.targets.iter().map(Item::of).collect(),
            },
            _ => level.display_query.targets.iter().map(|t| Item::of(&t.expr)).collect(),
        };
        targets.into_iter().chain(level.marks.iter().map(|m| item(m.clone()))).collect()
    };
    set(level, node, want, false);
    placeholders(level, node);
}

/// True when the `Result` of `create_minmaxagg_path` takes the place of the groups of the query, below the nodes of `DISTINCT`, `ORDER BY` and `LIMIT`.
fn replaced(level: &Level<'_>, node: &Node<'_>) -> bool {
    match (&node.kind, &node.children[..]) {
        (Kind::Result { replaces: Some(Replaced::MinMax), .. }, _) => node.scope == level.display,
        (_, [child]) if node.scope == level.display => replaced(level, child),
        _ => false,
    }
}

/// True for the `Append` of `create_degenerate_grouping_paths`, which has one `Result` for each grouping set of a query with no `GROUP BY` item and no aggregate.
fn degenerate_append(node: &Node<'_>) -> bool {
    matches!(node.kind, Kind::Append)
        && !node.children.is_empty()
        && node.children.iter().all(|c| {
            matches!(&c.kind, Kind::Result { replaces: Some(Replaced::Upper(rs)), .. } if rs.is_empty())
        })
}

/// The item of an expression that the planner made.
fn item<'q>(expr: Expr) -> Item<'q> {
    Item::owned(expr)
}

/// Adds the columns of the expression that are not in the list.
fn add_columns<'q>(expr: &Expr, levels: usize, out: &mut Vec<Item<'q>>) {
    let mut columns = Vec::new();
    exprs::columns_at(expr, levels, &mut columns);
    for column in columns {
        if !out.iter().any(|o| o.expr.same(&column)) {
            out.push(item(column));
        }
    }
}

/// Adds the columns, the aggregate calls and the window calls of the expression that are not in the list, in the order of the expression, as `pull_var_clause` finds them.
fn add_inputs<'q>(expr: &Expr, out: &mut Vec<Item<'q>>) {
    match &expr.kind {
        ExprKind::Agg(agg) if agg.levels_up == 0 => {}
        ExprKind::Window(_) => {}
        ExprKind::Var(v) if v.levels_up == 0 => {}
        ExprKind::SubLink(_) => return add_columns(expr, 0, out),
        _ => {
            for child in expr.children() {
                add_inputs(child, out);
            }
            return;
        }
    }
    if !out.iter().any(|o| o.expr.same(expr)) {
        out.push(item(expr.clone()));
    }
}

/// Adds the columns and the aggregate calls of the expression that are not in the list, as `pull_var_clause` with `PVC_INCLUDE_AGGREGATES` and `PVC_RECURSE_WINDOWFUNCS` finds them.
fn add_aggregates<'q>(expr: &Expr, out: &mut Vec<Item<'q>>) {
    match &expr.kind {
        ExprKind::Agg(agg) if agg.levels_up == 0 => {
            if !out.iter().any(|o| o.expr.same(expr)) {
                out.push(item(expr.clone()));
            }
        }
        ExprKind::Var(v) if v.levels_up == 0 => {
            if !out.iter().any(|o| o.expr.same(expr)) {
                out.push(item(expr.clone()));
            }
        }
        ExprKind::SubLink(_) | ExprKind::PlaceHolder(_) => add_columns(expr, 0, out),
        _ => {
            for child in expr.children() {
                add_aggregates(child, out);
            }
        }
    }
}

/// The width of the values of the items.
fn width(level: &Level<'_>, scope: usize, items: &[Item<'_>]) -> i32 {
    let query = if scope == level.scope { level.query } else { level.display_query };
    items.iter().map(|i| cost::expr_width(&i.expr, &query.relations)).sum()
}

/// The targets of the plan of a set operation below another set operation, which reads the columns of its leftmost `SELECT` as `generate_setop_tlist` gives them: the `Sort`, `Unique`, `Agg` and `SetOp` nodes down to the `Append` of its members. The plans of the members have their targets already.
pub(crate) fn nested<'q>(node: &mut Node<'q>, items: &[Item<'q>]) {
    let down = match node.kind {
        Kind::Sort(_)
        | Kind::IncrementalSort { .. }
        | Kind::Unique
        | Kind::Limit
        | Kind::Materialize
        | Kind::Aggregate { .. } => true,
        Kind::SetOp { .. } => false,
        _ => return,
    };
    if node.output.is_empty() {
        node.output = items.iter().map(|w| Item { below: true, ..w.clone() }).collect();
    }
    if down && let Some(child) = node.children.first_mut() {
        nested(child, items);
    }
}

/// True when the nodes below the node do not project down to the `Append`, the `SetOp` or the `Agg` of a set operation. `generate_append_tlist` gives that `Append` a target list of columns, so the nodes above it read each item as a value of the node below, and `get_special_variable` shows an item that is not a column in parentheses.
fn reads_append(node: &Node<'_>) -> bool {
    let mut at = node;
    while let Some(child) = at.children.first() {
        match child.kind {
            Kind::Append | Kind::MergeAppend(_) => return !from::members_append(child),
            Kind::Aggregate { .. } | Kind::SetOp { .. } => return true,
            Kind::Sort(_)
            | Kind::IncrementalSort { .. }
            | Kind::Limit
            | Kind::Unique
            | Kind::LockRows
            | Kind::Materialize => at = child,
            _ => return false,
        }
    }
    false
}

/// True when the expression is a column of the query.
fn is_var(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Var(_))
}

/// True when the expression is a constant.
fn is_const(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Const(_))
}

/// Sets the target list of a node, whose parent reads the items of `want`, and of the nodes below it in the query level. With `physical`, a scan gives all the columns of its relation.
fn set<'q>(level: &Level<'q>, node: &mut Node<'q>, want: Vec<Item<'q>>, physical: bool) {
    if node.scope != level.scope && node.scope != level.display {
        return;
    }
    let set_op = level.query.set_op.is_some();
    match &node.kind {
        // `create_append_plan`: the members of an inheritance tree give the items that the node above reads, and the `Append` gives the rows of its members as they are.
        Kind::Append | Kind::MergeAppend(_) if from::members_append(node) => {
            let before: Vec<i32> = node.children.iter().map(|c| c.cost.width).collect();
            for child in &mut node.children {
                set(level, child, want.clone(), false);
            }
            // `set_append_rel_size`: the width of the relation is the mean of the widths of its members, by the rows of their scans with no parameters. The planner gave the node this width, so only a change in the width of a member changes it.
            if node.children.iter().map(|c| c.cost.width).eq(before) {
                return;
            }
            let rows: f64 = node.children.iter().map(|c| c.cost.rows).sum();
            let size: f64 =
                node.children.iter().map(|c| f64::from(c.cost.width) * c.cost.rows).sum();
            node.cost.width = if rows > 0.0 { (size / rows).round_ties_even() as i32 } else { 0 };
        }
        // `create_degenerate_grouping_paths`: each `Result` of a grouping set computes the targets, and the `Append` gives its rows as they are.
        Kind::Append if degenerate_append(node) => {
            for child in &mut node.children {
                set(level, child, want.clone(), false);
            }
            node.cost.width = node.children.first().map_or(0, |c| c.cost.width);
        }
        Kind::Append | Kind::MergeAppend(_) | Kind::RecursiveUnion => {}
        // The `SetOp` reads the columns of the plans of its members.
        Kind::SetOp { .. } => {
            node.output = want.iter().map(|w| Item { below: true, ..w.clone() }).collect();
        }
        Kind::Aggregate { .. } if set_op => {
            // The node reads the targets of the plan of the member that is left, or the columns of the `Append`, which show as values of the node below, as `get_special_variable`.
            node.output = want.iter().map(|w| Item { below: true, ..w.clone() }).collect();
            node.cost.width = node.children.first().map_or(0, |c| c.cost.width);
        }
        Kind::Modify { operation, .. } => {
            let want = modify_targets(level, *operation);
            if let Some(child) = node.children.first_mut() {
                set(level, child, want, false);
            }
            node.output = returning_output(level, *operation).unwrap_or_default();
            // `create_modifytable_path`: with `RETURNING`, the node has the width of the rows below it.
            let returning = level.write.is_some_and(|w| w.returning.is_some());
            node.cost.width = match node.children.first() {
                Some(child) if returning => child.cost.width,
                _ => 0,
            };
        }
        // `make_sort_input_target`: a `Result` above the sort computes the targets that come after the sort from the other targets and from the columns, the aggregate calls and the window calls of those targets, which the sort gives.
        Kind::Result { replaces: None, one_time }
            if one_time.is_empty()
                && node.children.first().is_some_and(|c| {
                    !from::append_rel(level.query, c) && !from::inheritance_append(level.query, c)
                }) =>
        {
            let late = upper::postponed(level);
            let is_late = |at: usize| late.get(at).copied().unwrap_or(false);
            let mut below: Vec<Item<'q>> = want
                .iter()
                .enumerate()
                .filter(|&(at, _)| !is_late(at))
                .map(|(_, w)| w.clone())
                .collect();
            for (at, w) in want.iter().enumerate() {
                if is_late(at) {
                    add_inputs(&w.expr, &mut below);
                }
            }
            node.output = want
                .iter()
                .enumerate()
                .map(|(at, w)| if is_late(at) { w.clone() } else { w.passed() })
                .collect();
            if let Some(child) = node.children.first_mut() {
                set(level, child, below, false);
            }
            node.cost.width = width(level, node.scope, &want);
        }
        // `create_gating_plan`: the `Result` of the conditions that run once computes the targets, and the node below gives the columns that they read, as the target of its path. The `Result` above an `Append` of a `UNION ALL` does the same, as `create_projection_path`.
        Kind::Result { replaces: None, .. } if !node.children.is_empty() => {
            let mut below = Vec::new();
            for w in &want {
                add_columns(&w.expr, 0, &mut below);
            }
            if let Some(child) = node.children.first_mut() {
                set(level, child, below, false);
            }
            node.cost.width = width(level, node.scope, &want);
            node.output = want;
        }
        // `create_gather_plan`: the `Gather` gives the target of the plan below it, which computes the target when a worker can compute it. Else the `Gather` computes the target from the columns of the plan below.
        Kind::Gather { .. } | Kind::GatherMerge { .. } if !node.children.is_empty() => {
            if want.iter().all(|w| parallel::safe(&w.expr)) {
                node.output = want.iter().map(Item::passed).collect();
                if let Some(child) = node.children.first_mut() {
                    set(level, child, want, false);
                    node.cost.width = child.cost.width;
                }
            } else {
                let mut below = Vec::new();
                for w in &want {
                    add_columns(&w.expr, 0, &mut below);
                }
                if let Some(child) = node.children.first_mut() {
                    set(level, child, below, false);
                }
                node.cost.width = width(level, node.scope, &want);
                node.output = want;
            }
        }
        Kind::Sort(_)
        | Kind::IncrementalSort { .. }
        | Kind::Limit
        | Kind::Unique
        | Kind::LockRows
        | Kind::Materialize
        | Kind::Memoize(_)
        | Kind::Hash
            if !node.children.is_empty() =>
        {
            let mut below = want.clone();
            if let Kind::Sort(keys) | Kind::IncrementalSort { keys, .. } = &node.kind {
                for key in keys {
                    if !below.iter().any(|w| w.expr.same(&key.item.expr)) {
                        below.push(Item { expr: key.item.expr.clone(), below: false });
                    }
                }
            }
            // The node does not project, so its target is the target of the node below with the keys that it computes for the sort.
            let append = set_op && reads_append(node);
            node.output = below
                .iter()
                .map(|w| {
                    if (append && !is_var(&w.expr)) || (w.below && is_const(&w.expr)) {
                        Item { below: true, ..w.clone() }
                    } else {
                        w.passed()
                    }
                })
                .collect();
            if let Some(child) = node.children.first_mut() {
                // `prepare_sort_from_pathkeys`: a key that the node below computes for the sort does not count in its width.
                let added = width(level, child.scope, &below[want.len()..]);
                // The node below computes the constant of a `PlaceHolderVar`.
                let below = below
                    .into_iter()
                    .map(|w| if is_const(&w.expr) { Item { below: false, ..w } } else { w })
                    .collect();
                set(level, child, below, false);
                child.cost.width -= added;
                node.cost.width = child.cost.width;
            }
        }
        Kind::Sort(_)
        | Kind::IncrementalSort { .. }
        | Kind::Limit
        | Kind::Unique
        | Kind::LockRows
        | Kind::Materialize
        | Kind::Memoize(_)
        | Kind::Hash
        | Kind::Gather { .. }
        | Kind::GatherMerge { .. }
        | Kind::BitmapIndexScan { .. }
        | Kind::BitmapAnd
        | Kind::BitmapOr => {}
        Kind::Aggregate { keys, unique, split, .. } => {
            let unique = *unique;
            let split = *split;
            let keys: Vec<Expr> = keys.iter().map(|k| k.expr.as_ref().clone()).collect();
            // `use_physical_tlist` with `CP_LABEL_TLIST`: two items of `GROUP BY` or `DISTINCT` that are the same column would need two labels in one place of the physical target list.
            let refs: Vec<&Expr> = labeled(level).into_iter().map(|t| level.target(t)).collect();
            let vars = keys.iter().all(is_var)
                && keys.iter().enumerate().all(|(i, k)| !keys[..i].iter().any(|o| o.same(k)))
                && refs.iter().enumerate().all(|(i, k)| !refs[..i].iter().any(|o| o.same(k)));
            if unique {
                // `create_final_unique_paths`: the input is the target of the side, then each key that is not in it.
                let mut below = want.clone();
                for key in &keys {
                    if !below.iter().any(|b| b.expr.same(key)) {
                        below.push(item(key.clone()));
                    }
                }
                if let Some(child) = node.children.first_mut() {
                    set(level, child, below, vars);
                }
                node.output = want;
            } else if split == Split::Final {
                // `make_partial_grouping_target`: the partial plan below gives the keys, then the aggregate calls of the target and of `HAVING`.
                let mut partial: Vec<Item<'q>> = Vec::new();
                for key in &keys {
                    if !partial.iter().any(|p| p.expr.same(key)) {
                        partial.push(item(key.clone()));
                    }
                }
                for w in &want {
                    add_aggregates(&w.expr, &mut partial);
                }
                for f in &node.filter {
                    add_aggregates(&f.expr, &mut partial);
                }
                grouped(level, node, want, &keys, vars);
                if let Some(child) = node.children.first_mut() {
                    set(level, child, partial, false);
                }
            } else {
                grouped(level, node, want, &keys, vars);
            }
            keys_below(node);
        }
        Kind::WindowAgg { .. } => {
            // `make_window_input_target`: the targets that an item of a window names come first, in the order of the target list.
            let mut targets: Vec<usize> = level
                .query
                .windows
                .iter()
                .flat_map(|w| w.partition.iter().chain(&w.order))
                .map(|s| s.target)
                .collect();
            targets.sort_unstable();
            targets.dedup();
            let keys: Vec<Expr> = targets.into_iter().map(|t| level.target(t).clone()).collect();
            grouped(level, node, want, &keys, false);
        }
        Kind::ProjectSet => {
            let mut below = Vec::new();
            for w in &want {
                add_columns(&w.expr, 0, &mut below);
            }
            if let Some(child) = node.children.first_mut() {
                set(level, child, below, false);
            }
            node.cost.width = width(level, node.scope, &want);
            node.output = want;
        }
        Kind::NestedLoop { join_filter, .. } => {
            let mut parts: Vec<Expr> =
                join_filter.iter().map(|i| i.expr.as_ref().clone()).collect();
            // `distribute_qual_to_rels` marks the columns of a join clause as needed at the join, also when a parameterized scan of the inner side tests the clause. The scan can be below a `Memoize` node.
            let inner = node.children.get(1);
            let inner = match inner.map(|i| (&i.kind, i.children.first())) {
                Some((Kind::Memoize(_), scan)) => scan,
                _ => inner,
            };
            if let Some(inner) = inner {
                let rels = from::node_rels(inner);
                let conds = match &inner.kind {
                    Kind::IndexScan { cond, .. } | Kind::TidScan { cond, .. } => cond.as_slice(),
                    Kind::BitmapHeapScan { recheck, .. } => recheck.as_slice(),
                    _ => &[],
                };
                for c in conds.iter().chain(&inner.filter) {
                    if !exprs::relations(&c.expr).is_subset(&rels) {
                        parts.push(c.expr.as_ref().clone());
                    }
                }
            }
            join(level, node, want, &parts);
        }
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            let mut parts: Vec<Expr> =
                cond.iter().chain(join_filter).map(|i| i.expr.as_ref().clone()).collect();
            // `deconstruct_distribute` adds the columns of the conditions to the relations in the order of the join tree, the conditions of each join after the joins below it.
            let mut order = Vec::new();
            for item in &level.query.from {
                from::join_conditions(item, &mut order);
            }
            let place = |p: &Expr| order.iter().position(|q| q.same(p));
            if parts.iter().all(|p| place(p).is_some()) {
                parts.sort_by_key(|p| place(p));
            }
            join(level, node, want, &parts);
        }
        // `set_rel_size` marks a relation with a condition that is never true as dummy before `set_rel_width`, so its `Result` has a width of 0. A join with no rows and an inner side that a join marks dummy keep the width of their columns. A target that is not a column makes `apply_scanjoin_target_to_paths` give the node a new target, whose width `set_pathtarget_cost_width` finds.
        Kind::Result { replaces: Some(Replaced::Scan(_, false)), .. } => {
            let plain = want.iter().all(|w| is_var(&w.expr));
            node.cost.width = if plain { 0 } else { width(level, node.scope, &want) };
            node.output = want;
        }
        Kind::Result { .. } => {
            node.cost.width = width(level, node.scope, &want);
            node.output = want;
        }
        Kind::SeqScan(r)
        | Kind::IndexScan { relation: r, .. }
        | Kind::TidScan { relation: r, .. }
        | Kind::BitmapHeapScan { relation: r, .. }
        | Kind::SampleScan(r)
        | Kind::FunctionScan(r)
        | Kind::TableFunctionScan(r)
        | Kind::ValuesScan(r)
        | Kind::SubqueryScan(r)
        | Kind::CteScan(r)
        | Kind::WorkTableScan(r) => {
            let r = *r;
            // The scan of a member has the widths of the columns of the member.
            if node.member.is_none() || !want.iter().all(|w| is_var(&w.expr)) {
                node.cost.width = width(level, node.scope, &want);
            }
            if let Some(output) = simple_values(level, node, r, &want) {
                node.kind = Kind::Result { replaces: None, one_time: Vec::new() };
                node.cost.startup = 0.0;
                node.cost.total = cost::CPU_TUPLE;
                node.output = output;
                return;
            }
            if matches!(node.kind, Kind::SubqueryScan(_)) && append_rel(level, node, r, &want) {
                return;
            }
            let plain =
                want.iter().all(|w| matches!(w.expr.kind, ExprKind::Var(v) if v.attnum > 0));
            let physical = physical && node.member.is_none();
            // `create_scan_plan`: the physical target list of an index only scan is the columns of the index.
            let full = physical && plain;
            node.output = match &node.kind {
                _ if !full => want,
                Kind::IndexScan { only: true, columns, .. } => {
                    index_columns(level, r, columns).unwrap_or(want)
                }
                _ => all_columns(level, r),
            };
            if matches!(node.kind, Kind::SubqueryScan(_)) && trivial(level, node, r, full) {
                let child = node.children.pop();
                if let Some(child) = child {
                    *node = child;
                }
            }
        }
    }
}

/// The items that the rows below a `ModifyTable` give, as `preprocess_targetlist`. An `UPDATE` gives the new values of the columns of `SET` and the ID of the version of the row, and a `DELETE` gives only the ID. The node computes `RETURNING` from the old and the new rows.
fn modify_targets<'q>(level: &Level<'q>, operation: Operation) -> Vec<Item<'q>> {
    let query = level.query;
    let all = || query.targets.iter().map(|t| Item::of(&t.expr)).collect();
    let (Some(write), Some(tid)) = (level.write, query.targets.last()) else { return all() };
    match operation {
        Operation::Update if write.width < query.targets.len() => {
            let mut out: Vec<Item<'q>> = query.targets[..write.width]
                .iter()
                .enumerate()
                .filter(|(i, t)| {
                    !matches!(t.expr.kind, ExprKind::Var(v) if v.relation == 0 && v.levels_up == 0 && usize::try_from(v.attnum).ok() == Some(i + 1))
                })
                .map(|(_, t)| Item::of(&t.expr))
                .collect();
            out.push(Item::of(&tid.expr));
            out.extend(level.marks.iter().map(|m| item(m.clone())));
            out.extend(returning_carried(query, write));
            out
        }
        Operation::Delete => {
            let mut out = vec![Item::of(&tid.expr)];
            out.extend(level.marks.iter().map(|m| item(m.clone())));
            out.extend(returning_carried(query, write));
            out
        }
        Operation::Merge => merge_targets(level).unwrap_or_else(all),
        _ => all(),
    }
}

/// The columns of the other relations that `RETURNING` of an `UPDATE` or a `DELETE` reads, which `preprocess_targetlist` adds after the row marks. The query carries them after the row and before the ID of the version.
fn returning_carried<'q>(query: &'q Query, write: &Write) -> impl Iterator<Item = Item<'q>> {
    let width =
        write.returning.as_deref().and_then(|r| r.relations.first()).map(|r| r.columns.len());
    let end = query.targets.len().saturating_sub(1);
    let carried = width.and_then(|w| query.targets.get(w..end)).unwrap_or_default();
    carried
        .iter()
        .filter(|t| matches!(t.expr.kind, ExprKind::Var(v) if v.relation != 0 && v.levels_up == 0))
        .map(|t| Item::of(&t.expr))
}

/// The list of `RETURNING` of a write, as the target list of its `ModifyTable`. A column of the row that the statement writes is a column of the target table, which is the relation 0 of the query or [`TARGET`] for an `INSERT`, and a carried value is the column of the other relation that the query carries. The list is [`rupg_analyze::Shown::targets`], in which a column of `old` or `new` and `merge_action()` show as PostgreSQL shows them. None for a list that reads the OID of a partition or a subquery.
fn returning_output<'q>(level: &Level<'q>, operation: Operation) -> Option<Vec<Item<'q>>> {
    let query = level.query;
    let write = level.write?;
    let returning = write.returning.as_deref()?;
    let partition = query.targets.iter().any(
        |t| matches!(t.expr.kind, ExprKind::Var(v) if v.relation == 0 && v.attnum == TABLE_OID_ATTNUM),
    );
    if write.instead || partition {
        return None;
    }
    let width = returning.relations.first()?.columns.len();
    let (base, carried): (usize, &[Target]) = match operation {
        Operation::Insert => (width, &[]),
        Operation::Update | Operation::Delete => {
            (width, query.targets.get(width..query.targets.len().checked_sub(1)?)?)
        }
        Operation::Merge => {
            let count = write.merge.as_ref()?.carried;
            (3 * width + 1, &query.targets[query.targets.len().checked_sub(count)?..])
        }
    };
    let relation = if operation == Operation::Insert { TARGET } else { 0 };
    let var = |attnum| Expr::new(ExprKind::Var(Var { relation, attnum, levels_up: 0 }), 0);
    let map = |attnum: i16| -> Option<Expr> {
        // A system column, as `ctid`, is a column of the target table.
        if attnum < 0 {
            return Some(var(attnum));
        }
        let at = usize::try_from(attnum).ok()?;
        if (1..=width).contains(&at) {
            return Some(var(attnum));
        }
        // The write puts the ID of the new version in its place, which `ctid` reads.
        if write.returning_tid.is_some() && write.returning_tid == at.checked_sub(1) {
            return Some(var(CTID_ATTNUM));
        }
        let value = &carried.get(at.checked_sub(base + 1)?)?.expr;
        // A carried column of the target table is a column of the old row of an `UPDATE`, and a carried system column of the relation 0 is `ctid` of the old row of a `DELETE`.
        match value.kind {
            ExprKind::Var(v) if v.relation != 0 && v.levels_up == 0 => Some(value.clone()),
            ExprKind::Var(v) if v.attnum < 0 && v.levels_up == 0 => Some(var(v.attnum)),
            _ => None,
        }
    };
    let shown = returning.shown.as_deref().map(|s| &s.targets);
    let exprs: Vec<&Expr> = match shown {
        Some(shown) => shown.iter().collect(),
        None => returning.targets.iter().map(|t| &t.expr).collect(),
    };
    exprs
        .into_iter()
        .map(|expr| {
            let mut expr = expr.clone();
            map_row(&mut expr, &map).then(|| Item::owned(expr))
        })
        .collect()
}

/// Puts in the place of each column of the relation 0 the expression of `map` for its number, with the type of the column. False when `map` gives no expression, or the expression has a subquery or a column of a query outside it.
fn map_row(expr: &mut Expr, map: &dyn Fn(i16) -> Option<Expr>) -> bool {
    let mut ok = true;
    expr.each_mut(0, &mut |e: &mut Expr, _| match &e.kind {
        ExprKind::Var(v) if v.relation == 0 && v.levels_up == 0 => {
            match map(v.attnum) {
                Some(mut value) => {
                    value.ty = e.ty;
                    value.typmod = e.typmod;
                    *e = value;
                }
                None => ok = false,
            }
            true
        }
        // A column of `old` or `new` and `merge_action()` show as they are.
        ExprKind::Var(v)
            if v.levels_up == 0
                && [RETURNING_OLD, RETURNING_NEW, MERGE_ACTION].contains(&v.relation) =>
        {
            true
        }
        ExprKind::Var(_) | ExprKind::SubLink(_) => {
            ok = false;
            true
        }
        _ => false,
    });
    ok
}

/// `preprocess_targetlist` for a `MERGE`: the ID of the version of the row of the target table, then the columns of the other relations that the condition and the row of each `WHEN` clause read, then the row marks, then the columns that only `RETURNING` reads. The columns of the target table do not come, as the executor reads them from the row of the table.
fn merge_targets<'q>(level: &Level<'q>) -> Option<Vec<Item<'q>>> {
    let query = level.query;
    let merge = level.write?.merge.as_ref()?;
    let tid = query.targets.iter().find(|t| {
        matches!(t.expr.kind, ExprKind::Var(v) if v.relation == 0 && v.levels_up == 0 && v.attnum == -1)
    })?;
    let carried = &query.targets[query.targets.len().checked_sub(merge.carried)?..];
    let mut read = Vec::new();
    for clause in &merge.clauses {
        // The query of a clause with check constraints reads the query of its row as its only relation.
        let rows = match clause.query.relations.as_slice() {
            [relation] if relation.oid == 0 => {
                relation.subquery.as_deref().unwrap_or(&clause.query)
            }
            _ => &clause.query,
        };
        let mut columns = Vec::new();
        for expr in rows.filter.iter().chain(rows.targets.iter().map(|t| &t.expr)) {
            exprs::vars_of(expr, &mut columns);
        }
        for column in columns {
            if let ExprKind::Var(v) = column.kind
                && v.relation == 1
                && let Some(at) = usize::try_from(v.attnum).ok().and_then(|a| a.checked_sub(1))
                && at < carried.len()
                && !read.contains(&at)
            {
                read.push(at);
            }
        }
    }
    let mut out = vec![Item::of(&tid.expr)];
    out.extend(read.iter().map(|&at| Item::of(&carried[at].expr)));
    // `transform_MERGE_to_join`: with `NOT MATCHED BY SOURCE`, the executor tests the join condition and that the whole row of the source is not null, so the rows carry the columns of both.
    let by_source = merge.clauses.iter().any(|c| c.kind == MatchKind::NotMatchedBySource);
    if by_source && let [FromItem::Join(join)] = query.from.as_slice() {
        let mut columns = Vec::new();
        if let FromItem::Relation(r) = join.right {
            let whole = Var { relation: r, attnum: 0, levels_up: 0 };
            columns.push(Expr::new(ExprKind::Var(whole), oid::RECORD));
        }
        if let Some(on) = &join.on {
            exprs::vars_of(on, &mut columns);
        }
        for column in columns {
            let target = matches!(column.kind, ExprKind::Var(v) if v.relation == 0);
            if !target && !out.iter().any(|o| o.expr.same(&column)) {
                out.push(Item::owned(column));
            }
        }
    }
    out.extend(level.marks.iter().map(|m| item(m.clone())));
    out.extend(
        (0..carried.len()).filter(|at| !read.contains(at)).map(|at| Item::of(&carried[at].expr)),
    );
    Some(out)
}

/// `pull_up_simple_values`: the expressions of the row of a `VALUES` with one row that is the only relation of a query, which a `Result` gives in place of the `Values Scan`. The items must be columns of the row.
fn simple_values<'q>(
    level: &Level<'q>,
    node: &Node<'q>,
    r: usize,
    want: &[Item<'q>],
) -> Option<Vec<Item<'q>>> {
    let query = level.query;
    if !matches!(node.kind, Kind::ValuesScan(_))
        || node.scope != level.scope
        || query.relations.len() != 1
        || query.write.is_some()
        || !node.filter.is_empty()
    {
        return None;
    }
    let [row] = query.relations[r].values.as_deref()? else { return None };
    if row.iter().any(exprs::volatile) {
        return None;
    }
    want.iter()
        .map(|w| match w.expr.kind {
            ExprKind::Var(v) if v.relation == r && v.levels_up == 0 => {
                let column = usize::try_from(v.attnum).ok()?.checked_sub(1)?;
                row.get(column).map(Item::of)
            }
            _ => None,
        })
        .collect()
}

/// The columns of a relation of the query, as `build_physical_tlist`.
fn all_columns<'q>(level: &Level<'q>, r: usize) -> Vec<Item<'q>> {
    let relation = &level.query.relations[r];
    relation
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ty != 0)
        .filter_map(|(i, c)| {
            let attnum = i16::try_from(i + 1).ok()?;
            let mut expr =
                Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), c.ty);
            expr.typmod = c.typmod;
            Some(item(expr))
        })
        .collect()
}

/// The columns of the relation that an index has, as `indextlist`, or `None` when a column of the index is an expression.
fn index_columns<'q>(level: &Level<'q>, r: usize, columns: &[i16]) -> Option<Vec<Item<'q>>> {
    let relation = &level.query.relations[r];
    columns
        .iter()
        .map(|&attnum| {
            let c = relation.columns.get(usize::try_from(attnum - 1).ok()?)?;
            let mut expr =
                Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), c.ty);
            expr.typmod = c.typmod;
            Some(item(expr))
        })
        .collect()
}

/// `pull_up_simple_union_all` and `set_append_rel_size`: the `Append` of a `UNION ALL` takes the place of the `SubqueryScan` of the relation `r` that tests no condition, and gives only the columns `want`, which each member gives in place of the columns of the relation. The width of the `Append` is the mean width of the rows of its members. False when the scan stays.
fn append_rel<'q>(level: &Level<'q>, node: &mut Node<'q>, r: usize, want: &[Item<'q>]) -> bool {
    let Some(sub) = level.query.relations[r].subquery.as_deref() else { return false };
    let attnums: Option<Vec<usize>> = want
        .iter()
        .map(|w| match w.expr.kind {
            ExprKind::Var(v) if v.relation == r && v.levels_up == 0 => {
                usize::try_from(v.attnum).ok()?.checked_sub(1)
            }
            _ => None,
        })
        .collect();
    let Some(attnums) = attnums.filter(|_| node.filter.is_empty() && from::simple_union_all(sub))
    else {
        return false;
    };
    let Some(mut child) = node.children.pop() else { return false };
    let mut leaves = Vec::new();
    if let Some(op) = &sub.set_op {
        op.left.each_leaf(&mut |at| leaves.push(at));
        op.right.each_leaf(&mut |at| leaves.push(at));
    }
    if matches!(child.kind, Kind::Append | Kind::MergeAppend(_))
        && child.children.len() == leaves.len()
    {
        let (mut size, mut rows) = (0.0, 0.0);
        for (member, at) in child.children.iter_mut().zip(leaves) {
            let Some(leaf) = sub.relations[at].subquery.as_deref() else { continue };
            let targets = attnums.iter().filter_map(|&a| leaf.targets.get(a));
            member.cost.width = targets.map(|t| cost::expr_width(&t.expr, &leaf.relations)).sum();
            // The sort of a member of a `Merge Append` gives the rows of the plan below it.
            if matches!(member.kind, Kind::Sort(_)) {
                for below in &mut member.children {
                    below.cost.width = member.cost.width;
                }
            }
            size += f64::from(member.cost.width) * member.cost.rows;
            rows += member.cost.rows;
        }
        if rows > 0.0 {
            child.cost.width = (size / rows).round() as i32;
        }
    } else {
        child.cost.width = node.cost.width;
    }
    *node = child;
    true
}

/// True when a `SubqueryScan` gives the rows of its subquery as they are, so that `trivial_subqueryscan` takes it out of the plan. The subquery gives only the columns of the scan. With `physical` the scan has the physical target list, which has the junk columns of the subquery too, as `build_physical_tlist`.
fn trivial(level: &Level<'_>, node: &Node<'_>, r: usize, physical: bool) -> bool {
    let columns = level.query.relations[r].columns.len();
    let sub = level.query.relations[r].subquery.as_deref();
    // A junk column of the subquery, such as a key of its `ORDER BY`, makes its target list longer than the target list of the scan, unless the scan has the junk columns too.
    let junk =
        if physical { sub.map_or(0, |s| s.targets.iter().filter(|t| t.junk).count()) } else { 0 };
    let set_op = sub.is_some_and(|s| s.set_op.is_some());
    // The `Append` of a set operation gives the columns of the operation.
    let child = node.children.first().map(|c| match c.kind {
        Kind::Append | Kind::MergeAppend(_)
            if (set_op || degenerate_append(c)) && c.output.is_empty() =>
        {
            columns
        }
        _ => c.output.len(),
    });
    // The nodes above that do not project share the target list of the scan, so a scan with the targets of the query has their junk marks, which `apply_tlist_labeling` gives the top node. A junk mark that the column of the subquery does not have stops the removal.
    let targets = &level.query.targets;
    let labeled = node.output.len() == targets.len()
        && node.output.iter().zip(targets).all(|(o, t)| o.expr.same(&t.expr));
    let junk_differs = labeled
        && sub.is_some_and(|s| targets.iter().zip(&s.targets).any(|(t, c)| t.junk != c.junk));
    node.filter.is_empty()
        && !junk_differs
        && child.is_none_or(|n| n == columns || junk > 0 && n == columns + junk)
        && node.output.len() == columns
        && node.output.iter().enumerate().all(|(i, o)| {
            matches!(o.expr.kind, ExprKind::Var(v) if v.relation == r && usize::try_from(v.attnum).ok() == Some(i + 1))
        })
}

/// The targets with a `ressortgroupref`, in the order of the target list. `transformGroupClause` labels only the first target of a group of items that are the same expression, and `transformDistinctClause` labels each target.
fn labeled(level: &Level<'_>) -> Vec<usize> {
    let mut refs: Vec<usize> = Vec::new();
    for s in &level.query.group {
        if !refs.iter().any(|&r| level.target(r).same(level.target(s.target))) {
            refs.push(s.target);
        }
    }
    refs.extend(level.query.distinct.iter().map(|s| s.target));
    refs.sort_unstable();
    refs.dedup();
    refs
}

/// The target list of an `Agg` or a `WindowAgg`: a value that is a key comes from below, and the node computes the others from the columns below.
fn grouped<'q>(
    level: &Level<'q>,
    node: &mut Node<'q>,
    want: Vec<Item<'q>>,
    keys: &[Expr],
    physical: bool,
) {
    // `make_group_input_target`: the keys come first, in the order of the target list, then the keys that are not in it.
    let mut below: Vec<Item<'q>> = Vec::new();
    // `mark_nullable_by_grouping`: a constant key that a grouping set does not have is in a `PlaceHolderVar`, so a `Sort` below, which does not project, reads it from below. A node below that projects computes it.
    let sorted = node
        .children
        .first()
        .is_some_and(|c| matches!(c.kind, Kind::Sort(_) | Kind::IncrementalSort { .. }));
    let nullable = |key: &Expr| {
        let Kind::Aggregate { sets, .. } = &node.kind else { return false };
        sorted
            && matches!(key.kind, ExprKind::Const(_))
            && sets.iter().flat_map(|r| &r.sets).any(|s| !s.iter().any(|i| i.expr.same(key)))
    };
    // `make_group_input_target` adds each target with a `ressortgroupref`, so a column that two targets of the grouping name is there twice.
    let labels: Vec<&Expr> = if matches!(node.kind, Kind::Aggregate { .. }) {
        labeled(level).into_iter().map(|t| level.target(t)).collect()
    } else {
        Vec::new()
    };
    let in_want = want.iter().map(|w| &*w.expr).filter(|w| keys.iter().any(|k| k.same(w)));
    for key in in_want.chain(keys) {
        let times = labels.iter().filter(|l| l.same(key)).count().max(1);
        if below.iter().filter(|b| b.expr.same(key)).count() < times {
            below.push(Item { below: nullable(key), ..item(key.clone()) });
        }
    }
    let mut output = Vec::new();
    // `create_one_window_path`: a `WindowAgg` below this one gives the targets of the input of the windows, then its window calls, as `add_column_to_pathtarget` adds them.
    let mut calls_below: Vec<Item<'q>> = Vec::new();
    for w in &want {
        if keys.iter().any(|k| k.same(&w.expr)) {
            output.push(Item { expr: w.expr.clone(), below: !is_var(&w.expr) });
            continue;
        }
        let mut from_below = false;
        if let Kind::WindowAgg { window, merged, .. } = &node.kind {
            // The window calls of the windows below this one come from below.
            let mut calls = Vec::new();
            exprs::window_exprs(&w.expr, &mut calls);
            for call in calls {
                let ExprKind::Window(f) = &call.kind else { continue };
                let at = f.winref.wrapping_sub(1);
                if at != *window && !merged.contains(&at) {
                    // A target that is only the call reads the column of the node below.
                    from_below |= std::ptr::eq(call, &*w.expr);
                    if !calls_below.iter().any(|b| b.expr.same(call)) {
                        calls_below.push(item(call.clone()));
                    }
                }
            }
        }
        if matches!(node.kind, Kind::WindowAgg { .. }) {
            // `make_window_input_target`: the aggregate calls come from below, as `PVC_INCLUDE_AGGREGATES`.
            add_aggregates(&w.expr, &mut below);
            from_below |= matches!(&w.expr.kind, ExprKind::Agg(a) if a.levels_up == 0);
        } else {
            add_columns(&w.expr, 0, &mut below);
        }
        output.push(Item { expr: w.expr.clone(), below: w.below || from_below });
    }
    for f in &node.filter {
        add_columns(&f.expr, 0, &mut below);
    }
    for call in calls_below {
        if !below.iter().any(|b| b.expr.same(&call.expr)) {
            below.push(call);
        }
    }
    if let Some(child) = node.children.first_mut() {
        set(level, child, below, physical);
    }
    node.cost.width = width(level, node.scope, &output);
    node.output = output;
}

/// The keys of an `Agg` show as the expressions of the node below it, as `show_sort_group_keys` finds them in its target list. A key is in parentheses only when that node reads it from a node below it.
fn keys_below(node: &mut Node<'_>) {
    let Some(child) = node.children.first() else { return };
    let below = |item: &Item<'_>| {
        child.output.iter().find(|o| o.expr.same(&item.expr)).map_or(item.below, |o| o.below)
    };
    let Kind::Aggregate { keys, sets, .. } = &node.kind else { return };
    let set_items =
        sets.iter().flat_map(|r| r.sort.iter().map(|k| &k.item).chain(r.sets.iter().flatten()));
    let flags: Vec<bool> = keys.iter().chain(set_items).map(below).collect();
    if let Kind::Aggregate { keys, sets, .. } = &mut node.kind {
        let set_items = sets.iter_mut().flat_map(|r| {
            r.sort.iter_mut().map(|k| &mut k.item).chain(r.sets.iter_mut().flatten())
        });
        for (item, flag) in keys.iter_mut().chain(set_items).zip(flags) {
            item.below = flag;
        }
    }
}

/// The target list of a join: the items that the parent reads. Each side gives the columns of its relations that the join and the nodes above it read, and the columns that a lateral reference of the other side reads.
fn join<'q>(level: &Level<'q>, node: &mut Node<'q>, want: Vec<Item<'q>>, parts: &[Expr]) {
    let mut needed: Vec<Item<'q>> = Vec::new();
    for expr in want
        .iter()
        .map(|w| w.expr.as_ref())
        .chain(parts)
        .chain(node.filter.iter().map(|f| f.expr.as_ref()))
    {
        add_columns(expr, 0, &mut needed);
    }
    let sides: Vec<_> = node.children.iter().map(from::node_rels).collect();
    let all = from::node_rels(node);
    // `add_placeholders_to_joinrel`: the join computes a `PlaceHolderVar` that no side gives, from the columns of the sides.
    let mut computed = Vec::new();
    for n in &needed {
        if let ExprKind::PlaceHolder(p) = &n.expr.kind
            && !sides.iter().any(|rels| from_rels(&n.expr, rels))
        {
            add_columns(&p.expr, 0, &mut computed);
        }
    }
    for c in computed {
        if !needed.iter().any(|n| n.expr.same(&c.expr)) {
            needed.push(c);
        }
    }
    for (i, child) in node.children.iter_mut().enumerate() {
        let rels = &sides[i];
        let mut below: Vec<Item<'q>> =
            needed.iter().filter(|n| from_rels(&n.expr, rels)).cloned().collect();
        // A condition of this join that an equivalence class took the place of still needs the columns of each side.
        for clause in &level.joined {
            let used = exprs::relations(clause);
            if !used.is_subset(&all) || used.is_subset(rels) {
                continue;
            }
            let mut columns = Vec::new();
            add_columns(clause, 0, &mut columns);
            for col in columns {
                if from_rels(&col.expr, rels) && !below.iter().any(|b| b.expr.same(&col.expr)) {
                    below.push(col);
                }
            }
        }
        for column in level.stats.class_columns(rels) {
            if !below.iter().any(|b| b.expr.same(column)) {
                below.push(item(column.clone()));
            }
        }
        for (j, other) in sides.iter().enumerate() {
            if j == i {
                continue;
            }
            for r in other {
                for (expr, depth) in from::lateral_exprs_at(level.query, *r) {
                    let mut refs = Vec::new();
                    add_columns(expr, depth, &mut refs);
                    for col in refs {
                        if from_rels(&col.expr, rels)
                            && !below.iter().any(|b| b.expr.same(&col.expr))
                        {
                            below.push(col);
                        }
                    }
                }
            }
        }
        // `add_placeholders_to_base_rels` and `add_placeholders_to_joinrel` add the `PlaceHolderVar`s after the columns.
        below.sort_by_key(|b| matches!(b.expr.kind, ExprKind::PlaceHolder(_)));
        set(level, child, below, true);
    }
    node.cost.width = width(level, node.scope, &want);
    node.output = want;
}

/// True when a column or a `PlaceHolderVar` comes from the relations `rels`. A `PlaceHolderVar` comes from the scan or the join that computes it.
fn from_rels(expr: &Expr, rels: &BTreeSet<usize>) -> bool {
    match &expr.kind {
        ExprKind::Var(v) => rels.contains(&v.relation),
        ExprKind::PlaceHolder(p) => exprs::eval_at(p).is_subset(rels),
        _ => false,
    }
}

/// `fix_scan_expr` and `fix_join_expr` for each `PlaceHolderVar` of the plan of a query level: the node that computes it shows its expression, and a node above that node reads it from below. A scan computes each `PlaceHolderVar` of its relation, and a join each one of its relations that no side gives.
fn placeholders(level: &Level<'_>, node: &mut Node<'_>) {
    if node.scope != level.scope && node.scope != level.display {
        return;
    }
    let mut given: Vec<Expr> = Vec::new();
    for child in &node.children {
        let outputs = child.output.iter().filter(|o| exprs::is_placeholder(&o.expr));
        given.extend(outputs.map(|o| o.expr.as_ref().clone()));
    }
    for child in &mut node.children {
        placeholders(level, child);
    }
    let rels = from::node_rels(node);
    for item in items_mut(node) {
        let mut has = false;
        item.expr.find(0, &mut |e, _| {
            has |= exprs::is_placeholder(e);
            None::<()>
        });
        if has {
            let mut expr = item.expr.as_ref().clone();
            expr.each_mut(0, &mut |e, depth| {
                if depth == 0
                    && let ExprKind::PlaceHolder(p) = &e.kind
                    && p.levels_up == 0
                    && exprs::eval_at(p).is_subset(&rels)
                    && !given.iter().any(|g| g.same(e))
                {
                    *e = p.expr.clone();
                }
                false
            });
            item.expr = Cow::Owned(expr);
        }
    }
}

/// The expressions of a node.
fn items_mut<'n, 'q>(node: &'n mut Node<'q>) -> Vec<&'n mut Item<'q>> {
    let mut out: Vec<&mut Item<'q>> = node.output.iter_mut().chain(&mut node.filter).collect();
    match &mut node.kind {
        Kind::IndexScan { cond, order_by, .. } => out.extend(cond.iter_mut().chain(order_by)),
        Kind::Result { one_time: items, .. }
        | Kind::TidScan { cond: items, .. }
        | Kind::BitmapHeapScan { recheck: items, .. }
        | Kind::BitmapIndexScan { cond: items, .. }
        | Kind::NestedLoop { join_filter: items, .. } => out.extend(items),
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            out.extend(cond.iter_mut().chain(join_filter));
        }
        Kind::Aggregate { keys, sets, .. } => {
            out.extend(keys);
            for rollup in sets {
                out.extend(rollup.sort.iter_mut().map(|k| &mut k.item));
                out.extend(rollup.sets.iter_mut().flatten());
            }
        }
        Kind::Sort(keys) | Kind::IncrementalSort { keys, .. } | Kind::MergeAppend(keys) => {
            out.extend(keys.iter_mut().map(|k| &mut k.item));
        }
        _ => {}
    }
    out
}

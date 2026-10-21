//! The target list of each node of a query and the width of its rows, as `create_plan` makes them with `build_path_tlist` and `use_physical_tlist`.
//!
//! The width of a node is the width of the values that the nodes above it read, as the width of the `PathTarget` of its path. A scan below a join or an `Agg` gives all the columns of its relation, as a physical target list, but its width counts only the columns that the nodes above read.

use rupg_analyze::{Expr, ExprKind, Var};
use rupg_plan::{Item, Kind, Node, Operation};

use crate::{Level, cost, exprs, from};

/// Sets the target lists and the widths of the nodes of the plan of a query level.
pub(crate) fn level<'q>(level: &Level<'q>, node: &mut Node<'q>) {
    let want = if level.no_targets {
        Vec::new()
    } else {
        level.display_query.targets.iter().map(|t| Item::of(&t.expr)).collect()
    };
    set(level, node, want, false);
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

/// The width of the values of the items.
fn width(level: &Level<'_>, scope: usize, items: &[Item<'_>]) -> i32 {
    let query = if scope == level.scope { level.query } else { level.display_query };
    items.iter().map(|i| cost::expr_width(&i.expr, &query.relations)).sum()
}

/// True when the expression is a column of the query.
fn is_var(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Var(_))
}

/// Sets the target list of a node, whose parent reads the items of `want`, and of the nodes below it in the query level. With `physical`, a scan gives all the columns of its relation.
fn set<'q>(level: &Level<'q>, node: &mut Node<'q>, want: Vec<Item<'q>>, physical: bool) {
    if node.scope != level.scope && node.scope != level.display {
        return;
    }
    let set_op = level.query.set_op.is_some();
    match &node.kind {
        // `create_append_plan`: the members of an inheritance tree give the items that the node above reads, and the `Append` gives the rows of its members as they are.
        Kind::Append if node.children.iter().all(|c| c.member.is_some()) => {
            for child in &mut node.children {
                set(level, child, want.clone(), false);
            }
            node.cost.width = node.children.first().map_or(0, |c| c.cost.width);
        }
        Kind::Append | Kind::SetOp { .. } | Kind::RecursiveUnion => {}
        Kind::Aggregate { .. } if set_op => {
            node.output = want.iter().map(Item::passed).collect();
            node.cost.width = node.children.first().map_or(0, |c| c.cost.width);
        }
        Kind::Modify { operation, .. } => {
            let want = modify_targets(level, *operation);
            if let Some(child) = node.children.first_mut() {
                set(level, child, want, false);
            }
            node.output.clear();
            node.cost.width = 0;
        }
        Kind::Sort(_)
        | Kind::Limit
        | Kind::Unique
        | Kind::LockRows
        | Kind::Materialize
        | Kind::Hash
        | Kind::Result { replaces: None, .. }
            if !node.children.is_empty() =>
        {
            let mut below = want.clone();
            if let Kind::Sort(keys) = &node.kind {
                for key in keys {
                    if !below.iter().any(|w| w.expr.same(&key.item.expr)) {
                        below.push(Item { expr: key.item.expr.clone(), below: false });
                    }
                }
            }
            node.output = want.iter().map(Item::passed).collect();
            if let Some(child) = node.children.first_mut() {
                set(level, child, below, false);
                node.cost.width = child.cost.width;
            }
        }
        Kind::Sort(_)
        | Kind::Limit
        | Kind::Unique
        | Kind::LockRows
        | Kind::Materialize
        | Kind::Hash => {}
        Kind::Aggregate { keys, .. } => {
            let keys: Vec<Expr> = keys.iter().map(|k| k.expr.as_ref().clone()).collect();
            let vars = keys.iter().all(is_var)
                && keys.iter().enumerate().all(|(i, k)| !keys[..i].iter().any(|o| o.same(k)));
            grouped(level, node, want, &keys, vars);
            keys_below(node);
        }
        Kind::WindowAgg { window, .. } => {
            let clause = &level.query.windows[*window];
            let keys: Vec<Expr> = clause
                .partition
                .iter()
                .chain(&clause.order)
                .map(|s| level.target(s.target).clone())
                .collect();
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
            let parts: Vec<Expr> = join_filter.iter().map(|i| i.expr.as_ref().clone()).collect();
            join(level, node, want, &parts);
        }
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            let parts: Vec<Expr> =
                cond.iter().chain(join_filter).map(|i| i.expr.as_ref().clone()).collect();
            join(level, node, want, &parts);
        }
        Kind::Result { .. } => {
            node.cost.width = width(level, node.scope, &want);
            node.output = want;
        }
        Kind::SeqScan(r)
        | Kind::SampleScan(r)
        | Kind::FunctionScan(r)
        | Kind::TableFunctionScan(r)
        | Kind::ValuesScan(r)
        | Kind::SubqueryScan(r)
        | Kind::CteScan(r)
        | Kind::WorkTableScan(r) => {
            let r = *r;
            node.cost.width = width(level, node.scope, &want);
            if let Some(output) = simple_values(level, node, r, &want) {
                node.kind = Kind::Result { replaces: None, one_time: Vec::new() };
                node.cost.startup = 0.0;
                node.cost.total = cost::CPU_TUPLE;
                node.output = output;
                return;
            }
            let plain =
                want.iter().all(|w| matches!(w.expr.kind, ExprKind::Var(v) if v.attnum > 0));
            let physical = physical && node.member.is_none();
            node.output = if physical && plain { all_columns(level, r) } else { want };
            if matches!(node.kind, Kind::SubqueryScan(_)) && trivial(level, node, r) {
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
    let (Some(write), Some(tid)) = (&query.write, query.targets.last()) else { return all() };
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
            out
        }
        Operation::Delete => vec![Item::of(&tid.expr)],
        _ => all(),
    }
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

/// True when a `SubqueryScan` gives the rows of its subquery as they are, so that `trivial_subqueryscan` takes it out of the plan.
fn trivial(level: &Level<'_>, node: &Node<'_>, r: usize) -> bool {
    let columns = level.query.relations[r].columns.len();
    node.filter.is_empty()
        && node.output.len() == columns
        && node.output.iter().enumerate().all(|(i, o)| {
            matches!(o.expr.kind, ExprKind::Var(v) if v.relation == r && usize::try_from(v.attnum).ok() == Some(i + 1))
        })
}

/// The target list of an `Agg` or a `WindowAgg`: a value that is a key comes from below, and the node computes the others from the columns below.
fn grouped<'q>(
    level: &Level<'q>,
    node: &mut Node<'q>,
    want: Vec<Item<'q>>,
    keys: &[Expr],
    physical: bool,
) {
    let mut below: Vec<Item<'q>> = Vec::new();
    for key in keys {
        if !below.iter().any(|b| b.expr.same(key)) {
            below.push(item(key.clone()));
        }
    }
    let mut output = Vec::new();
    for w in &want {
        if keys.iter().any(|k| k.same(&w.expr)) {
            output.push(Item { expr: w.expr.clone(), below: !is_var(&w.expr) });
            continue;
        }
        if let Kind::WindowAgg { .. } = node.kind {
            // The window calls of the windows below this one come from below.
            let mut calls = Vec::new();
            exprs::window_calls(&w.expr, &mut calls);
            if let Kind::WindowAgg { window, .. } = node.kind {
                for call in calls {
                    if call.winref != window + 1 {
                        let expr = Expr::new(ExprKind::Window(Box::new(call.clone())), w.expr.ty);
                        if !below.iter().any(|b| b.expr.same(&expr)) {
                            below.push(item(expr));
                        }
                    }
                }
            }
        }
        add_columns(&w.expr, 0, &mut below);
        output.push(w.clone());
    }
    for f in &node.filter {
        add_columns(&f.expr, 0, &mut below);
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
    let flags: Vec<bool> = keys.iter().chain(sets.iter().flat_map(|(_, s)| s)).map(below).collect();
    if let Kind::Aggregate { keys, sets, .. } = &mut node.kind {
        let items = keys.iter_mut().chain(sets.iter_mut().flat_map(|(_, s)| s));
        for (item, flag) in items.zip(flags) {
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
    for (i, child) in node.children.iter_mut().enumerate() {
        let rels = &sides[i];
        let mut below: Vec<Item<'q>> = needed
            .iter()
            .filter(|n| matches!(n.expr.kind, ExprKind::Var(v) if rels.contains(&v.relation)))
            .cloned()
            .collect();
        // A condition of this join that an equivalence class took the place of still needs the columns of each side.
        for clause in &level.joined {
            let used = exprs::relations(clause);
            if !used.is_subset(&all) || used.is_subset(rels) {
                continue;
            }
            let mut columns = Vec::new();
            add_columns(clause, 0, &mut columns);
            for col in columns {
                if matches!(col.expr.kind, ExprKind::Var(v) if rels.contains(&v.relation))
                    && !below.iter().any(|b| b.expr.same(&col.expr))
                {
                    below.push(col);
                }
            }
        }
        for (j, other) in sides.iter().enumerate() {
            if j == i {
                continue;
            }
            for r in other {
                for (expr, depth) in from::lateral_exprs(&level.query.relations[*r]) {
                    let mut refs = Vec::new();
                    add_columns(expr, depth, &mut refs);
                    for col in refs {
                        if matches!(col.expr.kind, ExprKind::Var(v) if rels.contains(&v.relation))
                            && !below.iter().any(|b| b.expr.same(&col.expr))
                        {
                            below.push(col);
                        }
                    }
                }
            }
        }
        set(level, child, below, true);
    }
    node.cost.width = width(level, node.scope, &want);
    node.output = want;
}

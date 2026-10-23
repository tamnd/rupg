//! The target list of each node of a query and the width of its rows, as `create_plan` makes them with `build_path_tlist` and `use_physical_tlist`.
//!
//! The width of a node is the width of the values that the nodes above it read, as the width of the `PathTarget` of its path. A scan below a join or an `Agg` gives all the columns of its relation, as a physical target list, but its width counts only the columns that the nodes above read.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, Var};
use rupg_plan::{Item, Kind, Node, Operation, Replaced};

use crate::{Level, cost, exprs, from, upper};

/// Sets the target lists and the widths of the nodes of the plan of a query level.
pub(crate) fn level<'q>(level: &Level<'q>, node: &mut Node<'q>) {
    let want = if level.no_targets {
        Vec::new()
    } else {
        let targets: Vec<Item<'q>> = match (&node.kind, &level.query.minmax) {
            (Kind::Result { replaces: Some(Replaced::MinMax), .. }, Some(minmax)) => {
                minmax.targets.iter().map(Item::of).collect()
            }
            _ => level.display_query.targets.iter().map(|t| Item::of(&t.expr)).collect(),
        };
        targets.into_iter().chain(level.marks.iter().map(|m| item(m.clone()))).collect()
    };
    set(level, node, want, false);
    placeholders(level, node);
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
        Kind::Append | Kind::MergeAppend(_) | Kind::RecursiveUnion => {}
        Kind::SetOp { .. } => node.output = want.iter().map(Item::passed).collect(),
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
            // `create_modifytable_path`: with `RETURNING`, the node has the width of the rows below it.
            let returning = level.query.write.as_ref().is_some_and(|w| w.returning.is_some());
            node.cost.width = match node.children.first() {
                Some(child) if returning => child.cost.width,
                _ => 0,
            };
        }
        // `make_sort_input_target`: a `Result` above the sort computes the targets that come after the sort from the other targets and from the columns, the aggregate calls and the window calls of those targets, which the sort gives.
        Kind::Result { replaces: None, one_time }
            if one_time.is_empty()
                && node.children.first().is_some_and(|c| !from::append_rel(level.query, c)) =>
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
        Kind::Sort(_)
        | Kind::IncrementalSort { .. }
        | Kind::Limit
        | Kind::Unique
        | Kind::LockRows
        | Kind::Materialize
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
            node.output = want.iter().map(Item::passed).collect();
            if let Some(child) = node.children.first_mut() {
                // `prepare_sort_from_pathkeys`: a key that the node below computes for the sort does not count in its width.
                let added = width(level, child.scope, &below[want.len()..]);
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
        | Kind::Hash
        | Kind::BitmapIndexScan { .. }
        | Kind::BitmapAnd
        | Kind::BitmapOr => {}
        Kind::Aggregate { keys, .. } => {
            let keys: Vec<Expr> = keys.iter().map(|k| k.expr.as_ref().clone()).collect();
            let vars = keys.iter().all(is_var)
                && keys.iter().enumerate().all(|(i, k)| !keys[..i].iter().any(|o| o.same(k)));
            grouped(level, node, want, &keys, vars);
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
            let parts: Vec<Expr> = join_filter.iter().map(|i| i.expr.as_ref().clone()).collect();
            join(level, node, want, &parts);
        }
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            let parts: Vec<Expr> =
                cond.iter().chain(join_filter).map(|i| i.expr.as_ref().clone()).collect();
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
        | Kind::BitmapHeapScan { relation: r, .. }
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
            if matches!(node.kind, Kind::SubqueryScan(_)) && append_rel(level, node, r, &want) {
                return;
            }
            let plain =
                want.iter().all(|w| matches!(w.expr.kind, ExprKind::Var(v) if v.attnum > 0));
            let physical = physical && node.member.is_none();
            // `create_scan_plan`: the physical target list of an index only scan is the columns of the index.
            node.output = match &node.kind {
                _ if !(physical && plain) => want,
                Kind::IndexScan { only: true, columns, .. } => {
                    index_columns(level, r, columns).unwrap_or(want)
                }
                _ => all_columns(level, r),
            };
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
            out.extend(level.marks.iter().map(|m| item(m.clone())));
            out
        }
        Operation::Delete => {
            let mut out = vec![Item::of(&tid.expr)];
            out.extend(level.marks.iter().map(|m| item(m.clone())));
            out
        }
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

/// True when a `SubqueryScan` gives the rows of its subquery as they are, so that `trivial_subqueryscan` takes it out of the plan. The subquery gives only the columns of the scan.
fn trivial(level: &Level<'_>, node: &Node<'_>, r: usize) -> bool {
    let columns = level.query.relations[r].columns.len();
    // A junk column of the subquery, such as a key of its `ORDER BY`, makes its target list longer than the target list of the scan.
    let set_op = level.query.relations[r].subquery.as_deref().is_some_and(|s| s.set_op.is_some());
    // The `Append` of a set operation gives the columns of the operation.
    let child = node.children.first().map(|c| match c.kind {
        Kind::Append | Kind::MergeAppend(_) if set_op && c.output.is_empty() => columns,
        _ => c.output.len(),
    });
    node.filter.is_empty()
        && child.is_none_or(|n| n == columns)
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
                for (expr, depth) in from::lateral_exprs(&level.query.relations[*r]) {
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
        Kind::Result { one_time: items, .. }
        | Kind::IndexScan { cond: items, .. }
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

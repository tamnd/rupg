//! The plan of `FROM` and `WHERE`: the scan of each relation, the joins and the place of each condition, as `query_planner`, `make_one_rel` and `create_scan_plan` make them.

use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::{BoolOp, BoolTest, Expr, ExprKind, FromItem, JoinKind, Relation};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node, Replaced, SortKey};
use rupg_types::{Value, oid};

use crate::{Builder, Level, Member, cost, exprs, upper};

/// The rows of each relation of the query before and after its conditions, as `tuples` and `rows` of its `RelOptInfo`.
type Stats = BTreeMap<usize, (f64, f64)>;

/// A condition of `WHERE` or of `ON` with the relations that it reads, as a `RestrictInfo`.
#[derive(Clone, Debug)]
struct Qual<'q> {
    expr: &'q Expr,
    rels: BTreeSet<usize>,
    /// True for a condition of `ON` of an outer join that goes to the nullable side, which is not an equivalence of PostgreSQL.
    outer: bool,
}

/// The tree of the joins of `FROM`, as `deconstruct_jointree` sees it.
#[derive(Debug)]
enum Tree<'q> {
    Base { relation: usize, quals: Vec<Qual<'q>> },
    Join(Box<JoinTree<'q>>),
}

#[derive(Debug)]
struct JoinTree<'q> {
    kind: JoinKind,
    left: Tree<'q>,
    right: Tree<'q>,
    /// The parts of the condition of `ON`, before the planner gives each a place.
    on: Vec<&'q Expr>,
    /// The conditions of the join.
    quals: Vec<Qual<'q>>,
    /// The conditions above an outer join, which the join tests on its rows with the nulls.
    filter: Vec<Qual<'q>>,
    rels: BTreeSet<usize>,
}

impl Tree<'_> {
    fn rels(&self) -> BTreeSet<usize> {
        match self {
            Tree::Base { relation, .. } => BTreeSet::from([*relation]),
            Tree::Join(join) => join.rels.clone(),
        }
    }
}

/// A key of the order of the rows of a node: the expressions that have the same value at the key, as a `PathKey` of an equivalence class.
#[derive(Clone, Debug)]
pub(crate) struct OrderKey {
    pub(crate) exprs: Vec<Expr>,
    pub(crate) desc: bool,
    pub(crate) nulls_first: bool,
}

impl OrderKey {
    /// True when the key orders the rows by the expression in this direction.
    pub(crate) fn sorts(&self, expr: &Expr, desc: bool, nulls_first: bool) -> bool {
        self.desc == desc
            && self.nulls_first == nulls_first
            && self.exprs.iter().any(|e| e.same(expr))
    }
}

/// The plan of a part of `FROM`, with the relations that it reads and the order of its rows.
#[derive(Debug)]
struct Rel<'q> {
    node: Node<'q>,
    rels: BTreeSet<usize>,
    order: Vec<OrderKey>,
}

impl<'q> Builder<'q> {
    /// `query_planner`: the plan of the rows of `FROM` that pass `WHERE`, with the order of the rows.
    pub(crate) fn from(&mut self, level: &Level<'q>) -> (Node<'q>, Vec<OrderKey>) {
        let query = level.query;
        let mut parts = Vec::new();
        if let Some(filter) = &query.filter {
            exprs::and_parts(filter, &mut parts);
        }
        parts.extend(upper::having_split(query).0);
        let trees = query.from.iter().map(tree);
        let Some(mut tree) =
            trees.reduce(|left, right| join_tree(JoinKind::Inner, left, right, Vec::new()))
        else {
            return (result(level.scope, parts), Vec::new());
        };
        let all = tree.rels();
        let mut gating = Vec::new();
        let mut quals = Vec::new();
        for part in parts {
            let rels = exprs::relations(part);
            let volatile = exprs::volatile(part);
            if rels.is_empty() && !volatile {
                if exprs::is_false(part) {
                    return (dummy(level.scope, &all), Vec::new());
                }
                if !exprs::is_true(part) {
                    gating.push(Item::of(part));
                }
                continue;
            }
            // A volatile condition with no column runs once for each row of the join of all the relations, as `distribute_qual_to_rels`.
            let rels = if rels.is_empty() { all.clone() } else { rels };
            quals.push(Qual { expr: part, rels, outer: false });
        }
        let mut nonnullable = BTreeSet::new();
        for qual in &quals {
            nonnullable.extend(nonnullable_rels(qual.expr, true));
        }
        reduce(&mut tree, &nonnullable);
        place_on(&mut tree);
        for qual in quals {
            place(&mut tree, qual);
        }
        let mut stats = Stats::new();
        let rel = self.build(level, tree, &mut stats);
        let mut node = rel.node;
        if !gating.is_empty() {
            // `create_gating_plan`: the `Result` has the costs of the node below.
            let cost = node.cost;
            node = Node::new(
                Kind::Result { replaces: None, one_time: gating },
                level.scope,
                vec![node],
            );
            node.cost = cost;
        }
        (node, rel.order)
    }

    /// The plan of a part of the tree of the joins.
    fn build(&mut self, level: &Level<'q>, tree: Tree<'q>, stats: &mut Stats) -> Rel<'q> {
        match tree {
            Tree::Base { relation, quals } => self.scan(level, relation, quals, stats),
            Tree::Join(join) => {
                let JoinTree { kind, left, right, quals, filter, .. } = *join;
                let left = self.build(level, left, stats);
                let right = self.build(level, right, stats);
                join_rels(level, kind, left, right, quals, filter, stats)
            }
        }
    }

    /// The scan of a relation with its conditions, as `create_scan_plan`, with the costs of `cost_seqscan` and the other functions of `costsize.c` for each kind of scan.
    fn scan(
        &mut self,
        level: &Level<'q>,
        r: usize,
        quals: Vec<Qual<'q>>,
        stats: &mut Stats,
    ) -> Rel<'q> {
        let relation = &level.query.relations[r];
        let quals = order_quals(quals);
        let selectivity: f64 = quals.iter().map(|q| cost::selectivity(q.expr)).product();
        let mut per_row = cost::CPU_TUPLE + cost::qual_cost(quals.iter().map(|q| q.expr));
        let mut children = Vec::new();
        let (kind, startup, run, tuples) = match &relation.cte {
            Some(cte) if cte.work.is_some() => {
                let rows = self.work.iter().rev().find(|(w, _)| Some(*w) == cte.work);
                (Kind::WorkTableScan(r), 0.0, 0.0, rows.map_or(10.0, |(_, rows)| rows * 10.0))
            }
            Some(cte)
                if relation.subquery.is_none()
                    || cte.write.is_some()
                    || self.cte(&cte.name).is_some_and(|c| !c.inline) =>
            {
                let rows = self.cte(&cte.name).map_or(1000.0, |c| c.rows);
                (Kind::CteScan(r), 0.0, 0.0, rows)
            }
            _ => match (&relation.subquery, &relation.function, &relation.values) {
                (Some(sub), _, _) => {
                    let child = self.query(sub, Some(level.scope));
                    let c = child.cost;
                    children.push(child);
                    (Kind::SubqueryScan(r), c.startup, c.total - c.startup, c.rows)
                }
                (None, Some(function), _) => {
                    let startup = cost::qual_cost(function.calls.iter().map(|(call, _)| call));
                    if relation.json_table().is_some() {
                        (Kind::TableFunctionScan(r), startup, 0.0, 100.0)
                    } else {
                        let rows = function.calls.iter().map(|(call, _)| function_rows(call));
                        (Kind::FunctionScan(r), startup, 0.0, rows.fold(1.0, f64::max))
                    }
                }
                (None, None, Some(values)) => {
                    per_row += cost::CPU_OPERATOR;
                    (Kind::ValuesScan(r), 0.0, 0.0, values.len() as f64)
                }
                (None, None, None) => {
                    let members = match &relation.tablesample {
                        Some(_) => None,
                        None => self.members.and_then(|m| m.members(level.query, r)),
                    };
                    if let Some(members) = members {
                        return members_scan(level, r, &quals, members, stats);
                    }
                    let (pages, tuples) = cost::table_size(relation);
                    match &relation.tablesample {
                        Some(sample) => {
                            let fraction = sample
                                .args
                                .first()
                                .and_then(number)
                                .map_or(0.1, |p| (p / 100.0).clamp(0.0, 1.0));
                            (Kind::SampleScan(r), 0.0, pages * cost::SEQ_PAGE, tuples * fraction)
                        }
                        None => (Kind::SeqScan(r), 0.0, pages * cost::SEQ_PAGE, tuples),
                    }
                }
            },
        };
        let rows = cost::clamp_rows(tuples * selectivity);
        stats.insert(r, (tuples, rows));
        let mut node = Node::new(kind, level.scope, children);
        node.filter = quals.iter().map(|q| Item::of(q.expr)).collect();
        node.cost = Cost { startup, total: startup + run + tuples * per_row, rows, width: 0 };
        Rel { node, rels: BTreeSet::from([r]), order: Vec::new() }
    }
}

/// `set_append_rel_size` and `create_append_plan`: the scans of the members of the inheritance tree of the relation `r`, under an `Append` when there are two or more, each with the conditions of the relation. With no member the relation is a `Result` with no rows, as `set_dummy_rel_pathlist`.
fn members_scan<'q>(
    level: &Level<'q>,
    r: usize,
    quals: &[Qual<'q>],
    members: Vec<Member>,
    stats: &mut Stats,
) -> Rel<'q> {
    let selectivity: f64 = quals.iter().map(|q| cost::selectivity(q.expr)).product();
    let per_row = cost::CPU_TUPLE + cost::qual_cost(quals.iter().map(|q| q.expr));
    let (mut tuples, mut rows) = (0.0, 0.0);
    let mut children = Vec::with_capacity(members.len());
    for member in &members {
        let (pages, count) = cost::member_size(member);
        let mut node = Node::new(Kind::SeqScan(r), level.scope, Vec::new());
        node.member = Some(member.oid);
        node.filter = quals.iter().map(|q| Item::of(q.expr)).collect();
        let n = cost::clamp_rows(count * selectivity);
        let total = pages * cost::SEQ_PAGE + count * per_row;
        node.cost = Cost { startup: 0.0, total, rows: n, width: 0 };
        tuples += count;
        rows += n;
        children.push(node);
    }
    stats.insert(r, (tuples, rows));
    let rels = BTreeSet::from([r]);
    let node = match children.len() {
        0 => dummy(level.scope, &rels),
        1 => children.remove(0),
        _ => {
            // `cost_append`: the start of the first member, and the costs of all the members with half of `cpu_tuple_cost` for each row.
            let startup = children[0].cost.startup;
            let total: f64 = children.iter().map(|c| c.cost.total).sum::<f64>()
                + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
            let mut node = Node::new(Kind::Append, level.scope, children);
            node.cost = Cost { startup, total, rows, width: 0 };
            node
        }
    };
    Rel { node, rels, order: Vec::new() }
}

/// The tree of a part of `FROM`. A right join is a left join with the sides in the other order.
fn tree(item: &FromItem) -> Tree<'_> {
    match item {
        FromItem::Relation(r) => Tree::Base { relation: *r, quals: Vec::new() },
        FromItem::Join(join) => {
            let (mut left, mut right) = (tree(&join.left), tree(&join.right));
            let mut kind = join.kind;
            if kind == JoinKind::Right {
                std::mem::swap(&mut left, &mut right);
                kind = JoinKind::Left;
            }
            let mut on = Vec::new();
            if let Some(cond) = &join.on {
                exprs::and_parts(cond, &mut on);
            }
            join_tree(kind, left, right, on)
        }
    }
}

fn join_tree<'q>(kind: JoinKind, left: Tree<'q>, right: Tree<'q>, on: Vec<&'q Expr>) -> Tree<'q> {
    let mut rels = left.rels();
    rels.extend(right.rels());
    Tree::Join(Box::new(JoinTree {
        kind,
        left,
        right,
        on,
        quals: Vec::new(),
        filter: Vec::new(),
        rels,
    }))
}

/// The `Result` of a query with no `FROM`, with the conditions of `WHERE` as its `One-Time Filter`.
fn result<'q>(scope: usize, parts: Vec<&'q Expr>) -> Node<'q> {
    let parts: Vec<&Expr> = parts.into_iter().filter(|p| !exprs::is_true(p)).collect();
    if parts.iter().any(|p| exprs::is_false(p)) {
        let mut node = Node::new(
            Kind::Result { replaces: None, one_time: vec![false_item()] },
            scope,
            Vec::new(),
        );
        node.cost = Cost::default();
        return node;
    }
    let cost = cost::qual_cost(parts.iter().copied());
    let one_time = parts.into_iter().map(Item::of).collect();
    let mut node = Node::new(Kind::Result { replaces: None, one_time }, scope, Vec::new());
    node.cost = Cost { startup: 0.0, total: cost::CPU_TUPLE + cost, rows: 1.0, width: 0 };
    node
}

/// The `Result` with no rows that takes the place of the relations when a condition of `WHERE` is never true, as `mark_dummy_rel`.
fn dummy<'q>(scope: usize, rels: &BTreeSet<usize>) -> Node<'q> {
    let replaces = match rels.iter().collect::<Vec<_>>().as_slice() {
        [r] => Replaced::Scan(**r),
        _ => Replaced::Join(rels.iter().copied().collect()),
    };
    Node::new(
        Kind::Result { replaces: Some(replaces), one_time: vec![false_item()] },
        scope,
        Vec::new(),
    )
}

/// The constant `false`.
fn false_item<'q>() -> Item<'q> {
    Item::owned(Expr::new(ExprKind::Const(Value::Bool(false)), oid::BOOL))
}

/// A number of a constant.
fn number(expr: &Expr) -> Option<f64> {
    match expr.strip_implicit().as_const()? {
        Value::Int2(v) => Some(f64::from(*v)),
        Value::Int4(v) => Some(f64::from(*v)),
        Value::Int8(v) => Some(*v as f64),
        Value::Float4(v) => Some(f64::from(*v)),
        Value::Float8(v) => Some(*v),
        _ => None,
    }
}

/// The number of rows of a function in `FROM`: the rows of `generate_series` with constant integers, as its support function gives them, or else `prorows`.
pub(crate) fn function_rows(call: &Expr) -> f64 {
    let ExprKind::Func(f) = &call.kind else { return 1.0 };
    let name = builtin::proc_by_oid(f.oid).map(|p| p.name);
    if name == Some("generate_series") {
        let ints: Option<Vec<f64>> = f
            .args
            .iter()
            .map(|a| matches!(a.ty, oid::INT4 | oid::INT8).then(|| number(a)).flatten())
            .collect();
        if let Some(ints) = ints {
            let step = ints.get(2).copied().unwrap_or(1.0);
            if let [start, stop, ..] = ints[..]
                && step != 0.0
            {
                return ((stop - start) / step).floor().max(-1.0) + 1.0;
            }
        }
    }
    // `array_unnest_support`: the number of the elements of the array, as `estimate_array_length`.
    if name == Some("unnest")
        && let [array] = f.args.as_slice()
    {
        return match &array.strip_implicit().kind {
            ExprKind::Const(Value::Array(a)) => a.values.len() as f64,
            ExprKind::Const(Value::Null) => 0.0,
            ExprKind::Array { multidims: false, elements, .. } => elements.len() as f64,
            _ => 10.0,
        };
    }
    match name {
        _ if !f.retset => 1.0,
        Some(
            "unnest" | "json_array_elements" | "jsonb_array_elements" | "json_each" | "jsonb_each",
        ) => 100.0,
        Some("regexp_matches") => 10.0,
        _ => 1000.0,
    }
}

/// `find_nonnullable_rels`: the relations whose columns cannot be null in a row that passes the condition. At the top of the condition, `IS NOT NULL` and the other tests that are false for a null count too.
fn nonnullable_rels(expr: &Expr, top: bool) -> BTreeSet<usize> {
    let strict = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.strict);
    let union = |args: &[Expr], top: bool| {
        let mut out = BTreeSet::new();
        for arg in args {
            out.extend(nonnullable_rels(arg, top));
        }
        out
    };
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 => BTreeSet::from([var.relation]),
        ExprKind::Bool(BoolOp::And, args) => union(args, top),
        ExprKind::Bool(BoolOp::Or, args) => {
            let mut sets = args.iter().map(|a| nonnullable_rels(a, top));
            let first = sets.next().unwrap_or_default();
            sets.fold(first, |acc, s| acc.intersection(&s).copied().collect())
        }
        ExprKind::Bool(BoolOp::Not, args) => union(args, false),
        ExprKind::Func(f) if strict(f.oid) => union(&f.args, false),
        ExprKind::ScalarArrayOp { func, args, .. } if strict(*func) => union(args, false),
        ExprKind::Relabel(arg, _) | ExprKind::CoerceViaIo(arg, _) => nonnullable_rels(arg, top),
        ExprKind::NullTest(arg, false) if top => nonnullable_rels(arg, false),
        ExprKind::BooleanTest(
            arg,
            BoolTest::IsTrue | BoolTest::IsFalse | BoolTest::IsNotUnknown,
        ) if top => nonnullable_rels(arg, false),
        _ => BTreeSet::new(),
    }
}

/// `reduce_outer_joins`: an outer join is an inner join when a condition above it is false or null for each row with nulls for the relations of its nullable side.
fn reduce(tree: &mut Tree<'_>, above: &BTreeSet<usize>) {
    let Tree::Join(join) = tree else { return };
    let overlaps = |rels: BTreeSet<usize>| !rels.is_disjoint(above);
    match join.kind {
        JoinKind::Left if overlaps(join.right.rels()) => join.kind = JoinKind::Inner,
        JoinKind::Full => match (overlaps(join.left.rels()), overlaps(join.right.rels())) {
            (true, true) => join.kind = JoinKind::Inner,
            (true, false) => join.kind = JoinKind::Left,
            (false, true) => {
                join.kind = JoinKind::Left;
                std::mem::swap(&mut join.left, &mut join.right);
            }
            (false, false) => {}
        },
        _ => {}
    }
    let mut local = BTreeSet::new();
    if join.kind != JoinKind::Full {
        for part in &join.on {
            local.extend(nonnullable_rels(part, true));
        }
    }
    let (left, right) = match join.kind {
        JoinKind::Inner => {
            local.extend(above.iter().copied());
            (local.clone(), local)
        }
        JoinKind::Left => (above.clone(), local),
        _ => (BTreeSet::new(), BTreeSet::new()),
    };
    reduce(&mut join.left, &left);
    reduce(&mut join.right, &right);
}

/// Gives each part of the condition of `ON` of each join its place. A part of a left join that reads only the nullable side goes to that side.
fn place_on<'q>(tree: &mut Tree<'q>) {
    let Tree::Join(join) = tree else { return };
    place_on(&mut join.left);
    place_on(&mut join.right);
    let right = join.right.rels();
    for part in std::mem::take(&mut join.on) {
        let rels = exprs::relations(part);
        let volatile = exprs::volatile(part);
        if rels.is_empty() && !volatile {
            if !exprs::is_true(part) {
                join.quals.push(Qual { expr: part, rels, outer: false });
            }
            continue;
        }
        let rels = if rels.is_empty() { join.rels.clone() } else { rels };
        match join.kind {
            JoinKind::Inner => place_join(join, Qual { expr: part, rels, outer: false }),
            JoinKind::Left if rels.is_subset(&right) => {
                place(&mut join.right, Qual { expr: part, rels, outer: true });
            }
            _ => join.quals.push(Qual { expr: part, rels, outer: false }),
        }
    }
}

/// Gives a condition from above its place in the tree: the scan of its relation, or the lowest join that has its relations. A condition on the nullable side of an outer join stays above the join.
fn place<'q>(tree: &mut Tree<'q>, qual: Qual<'q>) {
    match tree {
        Tree::Base { quals, .. } => quals.push(qual),
        Tree::Join(join) => place_join(join, qual),
    }
}

fn place_join<'q>(join: &mut JoinTree<'q>, qual: Qual<'q>) {
    match join.kind {
        JoinKind::Inner if qual.rels.is_subset(&join.left.rels()) => place(&mut join.left, qual),
        JoinKind::Inner if qual.rels.is_subset(&join.right.rels()) => place(&mut join.right, qual),
        JoinKind::Inner => join.quals.push(qual),
        JoinKind::Left if qual.rels.is_subset(&join.left.rels()) => place(&mut join.left, qual),
        _ => join.filter.push(qual),
    }
}

/// True for a condition that PostgreSQL keeps in an equivalence class: an `=` that a merge join can use, with no volatile function, that is not a condition of an outer join.
fn equivalence(qual: &Qual<'_>) -> bool {
    !qual.outer
        && !exprs::volatile(qual.expr)
        && exprs::binary(qual.expr).is_some_and(|(op, _, _)| exprs::can_merge(op))
}

/// The conditions of a scan in the order of `order_qual_clauses`: the conditions that are not equivalences, then the equivalences, which the planner makes again, then by their cost for each row.
fn order_quals(quals: Vec<Qual<'_>>) -> Vec<Qual<'_>> {
    let (mut out, classes): (Vec<_>, Vec<_>) = quals.into_iter().partition(|q| !equivalence(q));
    out.extend(classes);
    out.sort_by_key(|q| exprs::calls(q.expr));
    out
}

/// A condition of a join that tests a value of the outer side and a value of the inner side with an operator, with the outer value on the left.
struct Clause<'q> {
    item: Item<'q>,
    outer: Expr,
    inner: Expr,
}

/// The condition with the value of `outer` on the left, when the condition compares a value of each side with an operator that `usable` accepts.
fn clause<'q>(
    qual: &Qual<'q>,
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> Option<Clause<'q>> {
    let (op, left, right) = exprs::binary(qual.expr)?;
    if !usable(op) || exprs::volatile(qual.expr) {
        return None;
    }
    let (l, r) = (exprs::relations(left), exprs::relations(right));
    if l.is_empty() || r.is_empty() {
        return None;
    }
    if l.is_subset(outer) && r.is_subset(inner) {
        return Some(Clause {
            item: Item::of(qual.expr),
            outer: left.clone(),
            inner: right.clone(),
        });
    }
    if l.is_subset(inner) && r.is_subset(outer) {
        let commuted = exprs::commute(qual.expr)?;
        return Some(Clause {
            item: Item::owned(commuted),
            outer: right.clone(),
            inner: left.clone(),
        });
    }
    None
}

/// The number of distinct values of an expression with no statistics, as `get_variable_numdistinct`: the rows of a small table, or else 200, and at most the rows of the relation after its conditions. The flag is true for the default.
fn distinct(expr: &Expr, stats: &Stats) -> (f64, bool) {
    if let ExprKind::Var(var) = expr.strip_implicit().kind
        && var.levels_up == 0
        && let Some(&(tuples, rows)) = stats.get(&var.relation)
    {
        if var.attnum < 0 && var.attnum != -6 {
            // A system column other than `tableoid` is unique.
            return (rows, false);
        }
        if tuples < cost::NUM_DISTINCT {
            return (cost::clamp_rows(tuples).min(rows.max(1.0)), false);
        }
        return (cost::NUM_DISTINCT.min(rows.max(1.0)), true);
    }
    (cost::NUM_DISTINCT, true)
}

/// The selectivity of a condition of a join: `1 / max(nd1, nd2)` for an `=` of a value of each side, as `eqjoinsel` with no statistics, or else the selectivity of the condition.
fn join_selectivity(qual: &Qual<'_>, stats: &Stats) -> f64 {
    if let Some((op, left, right)) = exprs::binary(qual.expr)
        && builtin::operator_by_oid(op).is_some_and(|o| o.name == "=")
        && exprs::reads_columns(left)
        && exprs::reads_columns(right)
    {
        let (l, _) = distinct(left, stats);
        let (r, _) = distinct(right, stats);
        return 1.0 / l.max(r);
    }
    cost::selectivity(qual.expr)
}

/// The kinds of node that read the rows of the node below once and keep them, so that a scan again costs little, as `cost_rescan`.
fn keeps_rows(kind: &Kind<'_>) -> bool {
    matches!(
        kind,
        Kind::FunctionScan(_)
            | Kind::TableFunctionScan(_)
            | Kind::CteScan(_)
            | Kind::WorkTableScan(_)
            | Kind::Materialize
            | Kind::Sort(_)
    )
}

/// The costs of a scan of the inner side of a nested loop after the first, as `cost_rescan`: the start and the total.
fn rescan(node: &Node<'_>) -> (f64, f64) {
    let c = node.cost;
    match node.kind {
        Kind::Materialize | Kind::Sort(_) => (0.0, cost::CPU_OPERATOR * c.rows),
        Kind::FunctionScan(_)
        | Kind::TableFunctionScan(_)
        | Kind::CteScan(_)
        | Kind::WorkTableScan(_) => (0.0, c.total - c.startup),
        _ => (c.startup, c.total),
    }
}

/// A `Material` node over a node, as `cost_material`.
pub(crate) fn materialize(child: Node<'_>) -> Node<'_> {
    let c = child.cost;
    let mut node = Node::new(Kind::Materialize, child.scope, vec![child]);
    node.cost =
        Cost { startup: c.startup, total: c.total + 2.0 * cost::CPU_OPERATOR * c.rows, ..c };
    node
}

/// A `Sort` of a node on keys, as `cost_sort`.
pub(crate) fn sort<'q>(child: Node<'q>, keys: Vec<SortKey<'q>>, bound: Option<f64>) -> Node<'q> {
    let c = child.cost;
    let (startup, total) = cost::sort((c.startup, c.total), c.rows, bound);
    let mut node = Node::new(Kind::Sort(keys), child.scope, vec![child]);
    node.cost = Cost { startup, total, ..c };
    node
}

/// True when the relations of a node read the relations of another side, as a lateral reference.
fn lateral(level: &Level<'_>, rels: &BTreeSet<usize>, other: &BTreeSet<usize>) -> bool {
    rels.iter().any(|r| !lateral_rels(level, *r).is_disjoint(other))
}

/// The relations of the query that the expressions of a relation of `FROM` read: its function, its `VALUES` lists or the columns of the query in its subquery.
pub(crate) fn lateral_rels(level: &Level<'_>, r: usize) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for expr in lateral_exprs(&level.query.relations[r]) {
        out.extend(exprs::relations_at(expr.0, expr.1));
    }
    out.remove(&r);
    out
}

/// The relations of the query below a node of the query, and the relations that a `Result` takes the place of.
pub(crate) fn node_rels(node: &Node<'_>) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    match &node.kind {
        Kind::SeqScan(r)
        | Kind::SampleScan(r)
        | Kind::FunctionScan(r)
        | Kind::TableFunctionScan(r)
        | Kind::ValuesScan(r)
        | Kind::SubqueryScan(r)
        | Kind::CteScan(r)
        | Kind::WorkTableScan(r) => {
            out.insert(*r);
            return out;
        }
        Kind::Result { replaces: Some(Replaced::Scan(r)), .. } => {
            out.insert(*r);
        }
        Kind::Result { replaces: Some(Replaced::Join(rels)), .. } => {
            out.extend(rels.iter().copied())
        }
        _ => {}
    }
    for child in &node.children {
        if child.scope == node.scope {
            out.extend(node_rels(child));
        }
    }
    out
}

/// The expressions of a relation of `FROM` that can read the other relations of the query, each with its depth below the query.
pub(crate) fn lateral_exprs(relation: &Relation) -> Vec<(&Expr, usize)> {
    let mut out = Vec::new();
    if let Some(function) = &relation.function {
        out.extend(function.calls.iter().map(|(call, _)| (call, 0)));
    }
    if let Some(values) = &relation.values {
        out.extend(values.iter().flatten().map(|e| (e, 0)));
    }
    if relation.lateral
        && let Some(sub) = &relation.subquery
    {
        out.extend(sub.all_exprs(1));
    }
    out
}

/// A join of two plans, with the costs of each kind of join for each order of the sides, as `add_paths_to_joinrel`. The plan with the lowest total cost wins.
fn join_rels<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    stats: &Stats,
) -> Rel<'q> {
    let mut rels = left.rels.clone();
    rels.extend(right.rels.iter().copied());
    let selectivity: f64 = quals.iter().map(|q| join_selectivity(q, stats)).product();
    let mut rows = left.node.cost.rows * right.node.cost.rows * selectivity;
    match kind {
        JoinKind::Left => rows = rows.max(left.node.cost.rows),
        JoinKind::Full => rows = rows.max(left.node.cost.rows).max(right.node.cost.rows),
        _ => {}
    }
    let filter_selectivity: f64 = filter.iter().map(|q| cost::selectivity(q.expr)).product();
    let rows = cost::clamp_rows(rows * filter_selectivity);
    let mut quals = quals;
    quals.sort_by_key(|q| exprs::calls(q.expr));
    let filter_cost = cost::qual_cost(filter.iter().map(|q| q.expr));
    let lateral_right = lateral(level, &right.rels, &left.rels);
    // The orders of the sides: the left side first, and the right side first when the join can have the sides in the other order.
    let mut orders = vec![false];
    if !lateral_right && kind != JoinKind::Left || kind == JoinKind::Left && !lateral_right {
        orders.push(true);
    }
    let mut best: Option<Candidate<'q>> = None;
    for swapped in orders {
        let (outer, inner) = if swapped { (&right, &left) } else { (&left, &right) };
        let join = match (kind, swapped) {
            (JoinKind::Left, true) => JoinKind::Right,
            (k, _) => k,
        };
        let input = Input { outer, inner, quals: &quals, filter_cost, stats };
        let nested = kind == JoinKind::Inner || !swapped && kind == JoinKind::Left;
        let candidates = [
            merge_join(&input, join),
            if nested { Some(nested_loop(&input, join, lateral_right)) } else { None },
            hash_join(&input, join),
        ];
        for candidate in candidates.into_iter().flatten() {
            if best.as_ref().is_none_or(|b| candidate.total < b.total) {
                best = Some(Candidate { swapped, ..candidate });
            }
        }
    }
    let Some(best) = best else {
        return nested_join(level, kind, left, right, quals, filter, rows, rels);
    };
    let (outer, inner) = if best.swapped { (right, left) } else { (left, right) };
    let mut out = best.make(level.scope, outer, inner, &quals, rows);
    out.node.filter = filter.iter().map(|q| Item::of(q.expr)).collect();
    out.rels = rels;
    out
}

/// A full join with no condition that a merge join or a hash join can use, which PostgreSQL cannot plan. The plan is a nested loop.
#[allow(clippy::too_many_arguments)]
fn nested_join<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    rows: f64,
    rels: BTreeSet<usize>,
) -> Rel<'q> {
    let c = (left.node.cost, right.node.cost);
    let inner = materialize(right.node);
    let join_filter = quals.iter().map(|q| Item::of(q.expr)).collect();
    let mut node = Node::new(
        Kind::NestedLoop { join: kind, join_filter },
        level.scope,
        vec![left.node, inner],
    );
    node.filter = filter.iter().map(|q| Item::of(q.expr)).collect();
    let total = c.0.total + c.1.total + c.0.rows * c.1.rows * cost::CPU_TUPLE;
    node.cost = Cost { startup: c.0.startup + c.1.startup, total, rows, width: 0 };
    Rel { node, rels, order: Vec::new() }
}

/// The parts of a join that each kind of join reads.
struct Input<'a, 'q> {
    outer: &'a Rel<'q>,
    inner: &'a Rel<'q>,
    quals: &'a [Qual<'q>],
    filter_cost: f64,
    stats: &'a Stats,
}

/// A kind of join for an order of the sides, with its cost.
struct Candidate<'q> {
    method: Method,
    join: JoinKind,
    swapped: bool,
    startup: f64,
    total: f64,
    /// For a merge join, true when the inner side needs a `Material` node; for a nested loop, true when it gets one.
    materialize: bool,
    /// For a merge join, true when each side needs a `Sort`.
    sort: (bool, bool),
    _marker: std::marker::PhantomData<&'q ()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Nested,
    Merge,
    Hash,
}

/// The conditions of a join that a kind of join tests itself, with the outer value on the left, and the other conditions.
fn split<'q>(
    quals: &[Qual<'q>],
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> (Vec<Clause<'q>>, Vec<Qual<'q>>) {
    let mut clauses = Vec::new();
    let mut other = Vec::new();
    for qual in quals {
        match clause(qual, outer, inner, usable) {
            Some(c) => clauses.push(c),
            None => other.push(qual.clone()),
        }
    }
    (clauses, other)
}

/// The number of rows of the join that pass the conditions, as `approx_tuple_count`.
fn tuple_count(input: &Input<'_, '_>, clauses: &[Clause<'_>]) -> f64 {
    let mut selectivity = 1.0;
    for c in clauses {
        let (l, _) = distinct(&c.outer, input.stats);
        let (r, _) = distinct(&c.inner, input.stats);
        selectivity /= l.max(r);
    }
    cost::clamp_rows(input.outer.node.cost.rows * input.inner.node.cost.rows * selectivity)
}

/// The costs of a nested loop, as `initial_cost_nestloop` and `final_cost_nestloop`. The inner side gets a `Material` node unless it keeps its rows already or reads the outer side.
fn nested_loop<'q>(input: &Input<'_, 'q>, join: JoinKind, lateral: bool) -> Candidate<'q> {
    let o = input.outer.node.cost;
    let materialize = !lateral && !keeps_rows(&input.inner.node.kind);
    let (inner, rescan) = if materialize {
        let i = input.inner.node.cost;
        let total = i.total + 2.0 * cost::CPU_OPERATOR * i.rows;
        (Cost { total, ..i }, (0.0, cost::CPU_OPERATOR * i.rows))
    } else {
        (input.inner.node.cost, rescan(&input.inner.node))
    };
    let startup = o.startup + inner.startup;
    let mut run = o.total - o.startup + inner.total - inner.startup;
    if o.rows > 1.0 {
        run += (o.rows - 1.0) * rescan.1;
    }
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(input.quals.iter().map(|q| q.expr)) + input.filter_cost;
    run += per_tuple * o.rows * inner.rows;
    Candidate {
        method: Method::Nested,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize,
        sort: (false, false),
        _marker: std::marker::PhantomData,
    }
}

/// The costs of a hash join with a hash table of the inner side, as `initial_cost_hashjoin` and `final_cost_hashjoin`.
fn hash_join<'q>(input: &Input<'_, 'q>, join: JoinKind) -> Option<Candidate<'q>> {
    let (clauses, other) =
        split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_hash);
    if clauses.is_empty() {
        return None;
    }
    let (o, i) = (input.outer.node.cost, input.inner.node.cost);
    let n = clauses.len() as f64;
    let startup = o.startup + i.total + (cost::CPU_OPERATOR * n + cost::CPU_TUPLE) * i.rows;
    let mut run = o.total - o.startup + cost::CPU_OPERATOR * n * o.rows;
    // `estimate_hash_bucket_stats`: the fraction of the inner rows in a bucket, 0.1 with no statistics.
    let bucket = clauses
        .iter()
        .map(|c| match distinct(&c.inner, input.stats) {
            (_, true) => 0.1,
            (nd, false) => 1.0 / nd.clamp(1.0, 1024.0).max(1.0),
        })
        .fold(1.0, f64::min);
    run += cost::CPU_OPERATOR * n * o.rows * cost::clamp_rows(i.rows * bucket) * 0.5;
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(other.iter().map(|q| q.expr)) + input.filter_cost;
    run += per_tuple * tuple_count(input, &clauses);
    Some(Candidate {
        method: Method::Hash,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize: false,
        sort: (false, false),
        _marker: std::marker::PhantomData,
    })
}

/// True when the order of the rows of a plan is the order of the values of the clauses, from the first key.
fn ordered(order: &[OrderKey], values: &[&Expr]) -> bool {
    values.len() <= order.len() && values.iter().zip(order).all(|(v, k)| k.sorts(v, false, false))
}

/// The costs of a merge join of the two sides in the order of the values of the `=` conditions, as `initial_cost_mergejoin` and `final_cost_mergejoin`. A side that is not in that order gets a `Sort`.
fn merge_join<'q>(input: &Input<'_, 'q>, join: JoinKind) -> Option<Candidate<'q>> {
    let (clauses, other) =
        split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return None;
    }
    let outer_values: Vec<&Expr> = clauses.iter().map(|c| &c.outer).collect();
    let inner_values: Vec<&Expr> = clauses.iter().map(|c| &c.inner).collect();
    let sort_outer = !ordered(&input.outer.order, &outer_values);
    let sort_inner = !ordered(&input.inner.order, &inner_values);
    let side = |rel: &Rel<'_>, sorted: bool| {
        let c = rel.node.cost;
        if sorted { cost::sort((c.startup, c.total), c.rows, None) } else { (c.startup, c.total) }
    };
    let (o, i) = (side(input.outer, sort_outer), side(input.inner, sort_inner));
    let (outer_rows, inner_rows) = (input.outer.node.cost.rows, input.inner.node.cost.rows);
    let tuples = tuple_count(input, &clauses);
    let rescanned = (tuples - inner_rows).max(0.0);
    let ratio = 1.0 + rescanned / inner_rows.max(1.0);
    let inner_run = i.1 - i.0;
    let startup = o.0 + i.0;
    let mut run = o.1 - o.0;
    let material = inner_run + cost::CPU_OPERATOR * inner_rows * ratio;
    let bare = inner_run * ratio;
    // The inner side needs a `Material` node when it is cheaper, or when its node cannot go back to a row that it gave, as `ExecSupportsMarkRestore`.
    let materialize = material < bare
        || !sort_inner
            && !matches!(
                input.inner.node.kind,
                Kind::Sort(_) | Kind::Materialize | Kind::Result { .. }
            );
    run += if materialize { material } else { bare };
    let n = clauses.len() as f64;
    run += cost::CPU_OPERATOR * n * (outer_rows + inner_rows * ratio);
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(other.iter().map(|q| q.expr)) + input.filter_cost;
    run += per_tuple * tuples;
    Some(Candidate {
        method: Method::Merge,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize,
        sort: (sort_outer, sort_inner),
        _marker: std::marker::PhantomData,
    })
}

/// The sort keys of the values of the clauses of a merge join, in ascending order.
fn merge_keys<'q>(values: Vec<Expr>) -> Vec<SortKey<'q>> {
    values
        .into_iter()
        .map(|v| SortKey {
            item: Item { expr: std::borrow::Cow::Owned(v), below: true },
            desc: false,
            using: None,
            nulls_first: false,
        })
        .collect()
}

impl<'q> Candidate<'q> {
    /// The plan of the join.
    fn make(
        &self,
        scope: usize,
        outer: Rel<'q>,
        inner: Rel<'q>,
        quals: &[Qual<'q>],
        rows: f64,
    ) -> Rel<'q> {
        let mut order = Vec::new();
        let node = match self.method {
            Method::Nested => {
                let inner_node =
                    if self.materialize { materialize(inner.node) } else { inner.node };
                let join_filter = quals.iter().map(|q| Item::of(q.expr)).collect();
                order = outer.order;
                Node::new(
                    Kind::NestedLoop { join: self.join, join_filter },
                    scope,
                    vec![outer.node, inner_node],
                )
            }
            Method::Hash => {
                let (clauses, other) = split(quals, &outer.rels, &inner.rels, exprs::can_hash);
                let c = inner.node.cost;
                let mut hash = Node::new(Kind::Hash, scope, vec![inner.node]);
                hash.cost = Cost { startup: c.total, ..c };
                let cond = clauses.into_iter().map(|c| c.item).collect();
                let join_filter = other.iter().map(|q| Item::of(q.expr)).collect();
                Node::new(
                    Kind::HashJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer.node, hash],
                )
            }
            Method::Merge => {
                let (clauses, other) = split(quals, &outer.rels, &inner.rels, exprs::can_merge);
                let outer_values: Vec<Expr> = clauses.iter().map(|c| c.outer.clone()).collect();
                let inner_values: Vec<Expr> = clauses.iter().map(|c| c.inner.clone()).collect();
                if self.join != JoinKind::Full {
                    order = outer_values
                        .iter()
                        .zip(&inner_values)
                        .map(|(o, i)| OrderKey {
                            exprs: vec![o.clone(), i.clone()],
                            desc: false,
                            nulls_first: false,
                        })
                        .collect();
                }
                let outer_node = if self.sort.0 {
                    sort(outer.node, merge_keys(outer_values), None)
                } else {
                    outer.node
                };
                let mut inner_node = if self.sort.1 {
                    sort(inner.node, merge_keys(inner_values), None)
                } else {
                    inner.node
                };
                if self.materialize {
                    inner_node = materialize(inner_node);
                }
                let cond = clauses.into_iter().map(|c| c.item).collect();
                let join_filter = other.iter().map(|q| Item::of(q.expr)).collect();
                Node::new(
                    Kind::MergeJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer_node, inner_node],
                )
            }
        };
        let mut node = node;
        node.cost = Cost { startup: self.startup, total: self.total, rows, width: 0 };
        Rel { node, rels: BTreeSet::new(), order }
    }
}

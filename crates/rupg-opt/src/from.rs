//! The plan of `FROM` and `WHERE`: the scan of each relation, the joins and the place of each condition, as `query_planner`, `make_one_rel` and `create_scan_plan` make them.

mod classes;
mod paths;
mod search;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::{
    BoolOp, BoolTest, Expr, ExprKind, FromItem, JoinKind, Relation, SortGroup, Var,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node, Replaced, SortKey};
use rupg_types::{Value, oid};

use crate::{Builder, Level, Member, cost, exprs, upper};
use paths::Paths;

/// The estimates of the relations of a query level for the costs of the joins.
#[derive(Debug, Default)]
struct Stats {
    /// The rows of each relation before and after its conditions, as `tuples` and `rows` of its `RelOptInfo`.
    rows: BTreeMap<usize, (f64, f64)>,
    /// Each column that the query reads, with its width and the relations of the places that read it, as `attr_needed`. The target list of the query is `usize::MAX`.
    needed: Vec<(Var, i32, BTreeSet<usize>)>,
    /// `sort_pathkeys`: the keys of `ORDER BY`, each with its directions.
    sort: Vec<(Expr, bool, bool)>,
    /// `group_pathkeys` and `distinct_pathkeys`: the keys of `GROUP BY` and of `DISTINCT`, in any order.
    groups: Vec<Vec<(Expr, bool, bool)>>,
    /// `query_pathkeys`: the keys of `GROUP BY`, or else of `DISTINCT`, or else of `ORDER BY`.
    query: Vec<(Expr, bool, bool)>,
    /// The two sides of each `=` condition of a join that a merge join can test, each with its relations.
    joins: Vec<[(Expr, BTreeSet<usize>); 2]>,
    /// `consider_startup`: true when the query has `LIMIT`, so that a plan with a lower start cost is a different plan.
    startup: bool,
}

impl Stats {
    /// Adds the columns of an expression that a place with these relations reads.
    fn need(&mut self, expr: &Expr, depth: usize, at: &BTreeSet<usize>, relations: &[Relation]) {
        let mut columns = Vec::new();
        exprs::columns_at(expr, depth, &mut columns);
        for column in columns {
            let ExprKind::Var(var) = column.kind else { continue };
            match self.needed.iter_mut().find(|(v, _, _)| *v == var) {
                Some((_, _, places)) => places.extend(at.iter().copied()),
                None => {
                    let width = cost::expr_width(&column, relations);
                    self.needed.push((var, width, at.clone()));
                }
            }
        }
    }

    /// The width of the rows of a join of relations, as `build_joinrel_tlist` and `set_rel_width` find it: the columns of the relations that a place outside the join reads.
    fn width(&self, rels: &BTreeSet<usize>) -> i32 {
        self.needed
            .iter()
            .filter(|(v, _, places)| rels.contains(&v.relation) && !places.is_subset(rels))
            .map(|(_, width, _)| width)
            .sum()
    }

    /// `truncate_useless_pathkeys`: the first keys of the order of the rows of a join of relations that a node above can use. These are the first keys of `ORDER BY`, the first keys that are keys of `GROUP BY` or of `DISTINCT`, or the first keys that a merge join with another relation can use.
    fn useful(&self, rels: &BTreeSet<usize>, order: &[OrderKey]) -> Vec<OrderKey> {
        let has = |list: &[(Expr, bool, bool)], k: &OrderKey| {
            list.iter().any(|(e, desc, nulls_first)| k.sorts(e, *desc, *nulls_first))
        };
        let ordered = order
            .iter()
            .zip(&self.sort)
            .take_while(|(k, (e, desc, nulls_first))| k.sorts(e, *desc, *nulls_first))
            .count();
        let grouped =
            self.groups.iter().map(|g| order.iter().take_while(|k| has(g, k)).count()).max();
        // `pathkeys_useful_for_merging`: a merge join sorts its sides in ascending order.
        let merging = order
            .iter()
            .take_while(|k| {
                !k.desc
                    && !k.nulls_first
                    && self.joins.iter().any(|sides| {
                        sides.iter().zip(sides.iter().rev()).any(|((e, _), (_, other))| {
                            k.exprs.iter().any(|x| x.same(e)) && !other.is_subset(rels)
                        })
                    })
            })
            .count();
        let n = ordered.max(grouped.unwrap_or(0)).max(merging);
        order[..n].to_vec()
    }
}

/// A condition of `WHERE` or of `ON` with the relations that it reads, as a `RestrictInfo`.
#[derive(Clone, Debug)]
struct Qual<'q> {
    expr: Cow<'q, Expr>,
    rels: BTreeSet<usize>,
    /// True for a condition of `ON` of an outer join that goes to the nullable side, which is not an equivalence of PostgreSQL.
    outer: bool,
    /// The place of the condition in the order of `deconstruct_jointree`, or `None` for a condition that the planner makes from an equivalence class.
    order: Option<usize>,
}

impl<'q> Qual<'q> {
    /// A condition of the query.
    fn of(expr: &'q Expr, rels: BTreeSet<usize>, outer: bool) -> Qual<'q> {
        Qual { expr: Cow::Borrowed(expr), rels, outer, order: None }
    }

    /// A condition that the planner makes from an equivalence class.
    fn implied(expr: Expr, rels: BTreeSet<usize>) -> Qual<'q> {
        Qual { expr: Cow::Owned(expr), rels, outer: false, order: None }
    }

    /// The condition as an expression that the node computes.
    fn item(&self) -> Item<'q> {
        Item { expr: self.expr.clone(), below: false }
    }
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
#[derive(Clone, Debug)]
struct Rel<'q> {
    node: Node<'q>,
    rels: BTreeSet<usize>,
    order: Vec<OrderKey>,
}

impl<'q> Builder<'q> {
    /// `query_planner`: the plan of the rows of `FROM` that pass `WHERE`, with the order of the rows and the conditions of joins that the equivalence classes took the place of.
    pub(crate) fn from(&mut self, level: &Level<'q>) -> (Node<'q>, Vec<OrderKey>, Vec<&'q Expr>) {
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
            return (result(level.scope, parts), Vec::new(), Vec::new());
        };
        let all = tree.rels();
        let mut gating = Vec::new();
        let mut quals = Vec::new();
        for part in parts {
            let rels = exprs::relations(part);
            let volatile = exprs::volatile(part);
            if rels.is_empty() && !volatile {
                if exprs::is_false(part) {
                    return (dummy(level.scope, &all), Vec::new(), Vec::new());
                }
                if !exprs::is_true(part) {
                    gating.push(Item::of(part));
                }
                continue;
            }
            // A volatile condition with no column runs once for each row of the join of all the relations, as `distribute_qual_to_rels`.
            let rels = if rels.is_empty() { all.clone() } else { rels };
            quals.push(Qual::of(part, rels, false));
        }
        let mut nonnullable = BTreeSet::new();
        for qual in &quals {
            nonnullable.extend(nonnullable_rels(&qual.expr, true));
        }
        reduce(&mut tree, &nonnullable);
        let mut next = 0;
        place_on(&mut tree, &mut next);
        for mut qual in quals {
            qual.order = Some(next);
            next += 1;
            place(&mut tree, qual);
        }
        let mut found = classes::Found::default();
        if !classes::domain(&mut tree, true, &mut found) {
            return (dummy(level.scope, &all), Vec::new(), Vec::new());
        }
        let mut stats = Stats::default();
        needed(level, &mut stats);
        let rel = self.build(level, tree, &found, &mut stats);
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
        (node, rel.order, found.joined)
    }

    /// The plan of a part of the tree of the joins. The inner joins of a join domain are one problem of the order of the joins, with the outer joins as items of the problem. `found` has the equivalence classes.
    fn build(
        &mut self,
        level: &Level<'q>,
        tree: Tree<'q>,
        found: &classes::Found<'q>,
        stats: &mut Stats,
    ) -> Rel<'q> {
        match tree {
            Tree::Base { relation, quals } => self.scan(level, relation, quals, stats),
            Tree::Join(join) if join.kind == JoinKind::Inner => {
                let (mut items, mut pool) = (Vec::new(), Vec::new());
                flatten(Tree::Join(join), &mut items, &mut pool);
                pool.sort_by_key(|q| q.order.unwrap_or(usize::MAX));
                let rels = items.into_iter().map(|t| self.build(level, t, found, stats)).collect();
                search::search(level, rels, &pool, found, stats)
            }
            Tree::Join(join) => {
                let JoinTree { kind, left, right, quals, filter, .. } = *join;
                let left = self.build(level, left, found, stats);
                let right = self.build(level, right, found, stats);
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
        let selectivity: f64 = quals.iter().map(|q| cost::selectivity(&q.expr)).product();
        let mut per_row = cost::CPU_TUPLE + cost::qual_cost(quals.iter().map(|q| &*q.expr));
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
                        None => self.members.and_then(|m| {
                            let conds: Vec<&Expr> = quals.iter().map(|q| &*q.expr).collect();
                            m.members(level.query, r, &conds)
                        }),
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
        stats.rows.insert(r, (tuples, rows));
        let mut node = Node::new(kind, level.scope, children);
        node.filter = quals.iter().map(|q| q.item()).collect();
        let width = stats.width(&BTreeSet::from([r]));
        node.cost = Cost { startup, total: startup + run + tuples * per_row, rows, width };
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
    let selectivity: f64 = quals.iter().map(|q| cost::selectivity(&q.expr)).product();
    let per_row = cost::CPU_TUPLE + cost::qual_cost(quals.iter().map(|q| &*q.expr));
    let (mut tuples, mut rows) = (0.0, 0.0);
    let mut children = Vec::with_capacity(members.len());
    for member in &members {
        let (pages, count) = cost::member_size(member);
        let mut node = Node::new(Kind::SeqScan(r), level.scope, Vec::new());
        node.member = Some(member.oid);
        node.filter = quals.iter().map(|q| q.item()).collect();
        let n = cost::clamp_rows(count * selectivity);
        let total = pages * cost::SEQ_PAGE + count * per_row;
        node.cost = Cost { startup: 0.0, total, rows: n, width: 0 };
        tuples += count;
        rows += n;
        children.push(node);
    }
    stats.rows.insert(r, (tuples, rows));
    let rels = BTreeSet::from([r]);
    let width = stats.width(&rels);
    for child in &mut children {
        child.cost.width = width;
    }
    let node = match children.len() {
        0 => dummy(level.scope, &rels),
        1 => children.remove(0),
        _ => {
            // `cost_append`: the start of the first member, and the costs of all the members with half of `cpu_tuple_cost` for each row.
            let startup = children[0].cost.startup;
            let total: f64 = children.iter().map(|c| c.cost.total).sum::<f64>()
                + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
            let mut node = Node::new(Kind::Append, level.scope, children);
            node.cost = Cost { startup, total, rows, width };
            node
        }
    };
    Rel { node, rels, order: Vec::new() }
}

/// The columns that each place of a query level reads, as `build_base_rel_tlists`, `distribute_qual_to_rels` and `find_lateral_references` set `attr_needed`: the target list, the conditions of `WHERE` and of `ON`, and the lateral references.
fn needed(level: &Level<'_>, stats: &mut Stats) {
    let query = level.query;
    let relations = &query.relations;
    let top = BTreeSet::from([usize::MAX]);
    for target in &query.targets {
        stats.need(&target.expr, 0, &top, relations);
    }
    let (to_where, keep) = upper::having_split(query);
    for part in keep {
        stats.need(part, 0, &top, relations);
    }
    let mut parts = to_where;
    if let Some(filter) = &query.filter {
        exprs::and_parts(filter, &mut parts);
    }
    let mut items: Vec<&FromItem> = query.from.iter().collect();
    while let Some(item) = items.pop() {
        if let FromItem::Join(join) = item {
            if let Some(on) = &join.on {
                exprs::and_parts(on, &mut parts);
            }
            items.extend([&join.left, &join.right]);
        }
    }
    for part in parts {
        stats.need(part, 0, &exprs::relations(part), relations);
        if let Some((op, left, right)) = exprs::binary(part)
            && exprs::can_merge(op)
        {
            let (l, r) = (exprs::relations(left), exprs::relations(right));
            if !l.is_empty() && !r.is_empty() && l.is_disjoint(&r) {
                stats.joins.push([(left.clone(), l), (right.clone(), r)]);
            }
        }
    }
    let keys = |items: &[SortGroup]| -> Vec<(Expr, bool, bool)> {
        items
            .iter()
            .map(|item| {
                let desc = builtin::operator_by_oid(item.operator).is_some_and(|o| o.name == ">");
                (level.target(item.target).clone(), desc, item.nulls_first)
            })
            .collect()
    };
    stats.sort = keys(&query.sort);
    stats.groups = vec![keys(&query.group), keys(&query.distinct)];
    stats.query = [&query.group, &query.distinct, &query.sort]
        .into_iter()
        .find(|items| !items.is_empty())
        .map_or_else(Vec::new, |items| keys(items));
    stats.startup = upper::limit_estimates(query).0 != 0.0;
    for (r, relation) in relations.iter().enumerate() {
        for (expr, depth) in lateral_exprs(relation) {
            let mut at = exprs::relations_at(expr, depth);
            at.insert(r);
            stats.need(expr, depth, &at, relations);
        }
    }
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

/// Gives each part of the condition of `ON` of each join its place. A part of a left join that reads only the nullable side goes to that side. `next` is the place of the next condition in the order of `deconstruct_jointree`.
fn place_on<'q>(tree: &mut Tree<'q>, next: &mut usize) {
    let Tree::Join(join) = tree else { return };
    place_on(&mut join.left, next);
    place_on(&mut join.right, next);
    let right = join.right.rels();
    for part in std::mem::take(&mut join.on) {
        let rels = exprs::relations(part);
        let volatile = exprs::volatile(part);
        let order = Some(*next);
        *next += 1;
        if rels.is_empty() && !volatile {
            if !exprs::is_true(part) {
                join.quals.push(Qual { order, ..Qual::of(part, rels, false) });
            }
            continue;
        }
        let rels = if rels.is_empty() { join.rels.clone() } else { rels };
        match join.kind {
            JoinKind::Inner => place_join(join, Qual { order, ..Qual::of(part, rels, false) }),
            JoinKind::Left if rels.is_subset(&right) => {
                place(&mut join.right, Qual { order, ..Qual::of(part, rels, true) });
            }
            _ => join.quals.push(Qual { order, ..Qual::of(part, rels, false) }),
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
        && !exprs::volatile(&qual.expr)
        && exprs::binary(&qual.expr).is_some_and(|(op, _, _)| exprs::can_merge(op))
}

/// The conditions of a scan in the order of `order_qual_clauses`: the conditions that are not equivalences, then the equivalences, which the planner makes again, then by their cost for each row.
fn order_quals(quals: Vec<Qual<'_>>) -> Vec<Qual<'_>> {
    let (mut out, classes): (Vec<_>, Vec<_>) = quals.into_iter().partition(|q| !equivalence(q));
    out.extend(classes);
    out.sort_by_key(|q| exprs::calls(&q.expr));
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
    let (op, left, right) = exprs::binary(&qual.expr)?;
    if !usable(op) || exprs::volatile(&qual.expr) {
        return None;
    }
    let (l, r) = (exprs::relations(left), exprs::relations(right));
    if l.is_empty() || r.is_empty() {
        return None;
    }
    if l.is_subset(outer) && r.is_subset(inner) {
        return Some(Clause { item: qual.item(), outer: left.clone(), inner: right.clone() });
    }
    if l.is_subset(inner) && r.is_subset(outer) {
        let commuted = exprs::commute(&qual.expr)?;
        return Some(Clause {
            item: Item::owned(commuted),
            outer: right.clone(),
            inner: left.clone(),
        });
    }
    None
}

/// The number of distinct values of an expression with no statistics, as `get_variable_numdistinct`, and the tuples and the rows of the relation when the expression reads one relation. A `boolean` has 2 values, `tableoid` has 1 and the other system columns are unique. A small table has as many values as tuples, and the others have 200. The flag is true for the default.
fn numdistinct(expr: &Expr, stats: &Stats) -> (f64, bool, Option<(f64, f64)>) {
    let rels = exprs::relations(expr);
    let rel = match (rels.len(), rels.first()) {
        (1, Some(r)) => stats.rows.get(r).copied(),
        _ => None,
    };
    let known = if expr.ty == oid::BOOL {
        Some(2.0)
    } else if let ExprKind::Var(var) = expr.strip_implicit().kind
        && var.attnum < 0
    {
        // `tableoid` has one value in a relation, and the other system columns are unique.
        if var.attnum == -6 { Some(1.0) } else { rel.map(|(tuples, _)| tuples) }
    } else {
        None
    };
    let nd = match (known, rel) {
        (Some(nd), _) => (cost::clamp_rows(nd), false),
        (None, Some((tuples, _))) if tuples > 0.0 && tuples < cost::NUM_DISTINCT => {
            (cost::clamp_rows(tuples), false)
        }
        _ => (cost::NUM_DISTINCT, true),
    };
    (nd.0, nd.1, rel)
}

/// The number of distinct values of an expression with no statistics, at most the rows of its relation after the conditions. The flag is true for the default.
fn distinct(expr: &Expr, stats: &Stats) -> (f64, bool) {
    let (nd, default, rel) = numdistinct(expr, stats);
    (rel.map_or(nd, |(_, rows)| nd.min(rows.max(1.0))), default)
}

/// The selectivity of a condition of a join: `1 / max(nd1, nd2)` for an `=` of a value of each side, as `eqjoinsel` with no statistics, or else the selectivity of the condition.
fn join_selectivity(qual: &Qual<'_>, stats: &Stats) -> f64 {
    if let Some((op, left, right)) = exprs::binary(&qual.expr)
        && builtin::operator_by_oid(op).is_some_and(|o| o.name == "=")
        && exprs::reads_columns(left)
        && exprs::reads_columns(right)
    {
        let (l, _) = distinct(left, stats);
        let (r, _) = distinct(right, stats);
        return 1.0 / l.max(r);
    }
    cost::selectivity(&qual.expr)
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
        Kind::Materialize | Kind::Sort(_) => {
            (0.0, cost::CPU_OPERATOR * c.rows + cost::spill(c.rows, c.width))
        }
        Kind::FunctionScan(_)
        | Kind::TableFunctionScan(_)
        | Kind::CteScan(_)
        | Kind::WorkTableScan(_) => (0.0, c.total - c.startup),
        _ => (c.startup, c.total),
    }
}

/// A `Material` node over a node, as `cost_material`. The rows go to disk when they do not fit in `work_mem`.
pub(crate) fn materialize(child: Node<'_>) -> Node<'_> {
    let c = child.cost;
    let total = c.total + 2.0 * cost::CPU_OPERATOR * c.rows + cost::spill(c.rows, c.width);
    let mut node = Node::new(Kind::Materialize, child.scope, vec![child]);
    node.cost = Cost { startup: c.startup, total, ..c };
    node
}

/// The `Material` node over the inner side of a merge join, as `create_mergejoin_plan` makes it: one `cpu_operator_cost` for each row.
fn merge_materialize(child: Node<'_>) -> Node<'_> {
    let c = child.cost;
    let mut node = Node::new(Kind::Materialize, child.scope, vec![child]);
    node.cost = Cost { total: c.total + cost::CPU_OPERATOR * c.rows, ..c };
    node
}

/// A `Sort` of a node on keys, as `cost_sort`.
pub(crate) fn sort<'q>(child: Node<'q>, keys: Vec<SortKey<'q>>, bound: Option<f64>) -> Node<'q> {
    let c = child.cost;
    let (startup, total) = cost::sort((c.startup, c.total), c.rows, c.width, bound);
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

/// A join of two plans, with the costs of each kind of join for each order of the sides, as `add_paths_to_joinrel`. Of the plans that `add_path` keeps, the plan with the lowest total cost wins.
fn join_rels<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    stats: &Stats,
) -> Rel<'q> {
    let rows = join_rows(kind, &left, &right, &quals, &filter, stats);
    let mut quals = quals;
    quals.sort_by_key(|q| exprs::calls(&q.expr));
    let mut paths = Paths::new(stats.startup);
    let sides = (std::slice::from_ref(&left), std::slice::from_ref(&right));
    for (candidate, keys) in choose(level, kind, sides.0, sides.1, &quals, &filter, stats) {
        paths.add(candidate.startup, candidate.total, keys, candidate);
    }
    let mut out = match paths.cheapest() {
        Some(best) => finish(level, &best, left, right, &quals, &filter, rows),
        None => {
            let mut rels = left.rels.clone();
            rels.extend(right.rels.iter().copied());
            nested_join(level, kind, left, right, quals, filter, rows, rels)
        }
    };
    out.node.cost.width = stats.width(&out.rels);
    out
}

/// The rows of a join of two sides, as `calc_joinrel_size_estimate`.
fn join_rows(
    kind: JoinKind,
    left: &Rel<'_>,
    right: &Rel<'_>,
    quals: &[Qual<'_>],
    filter: &[Qual<'_>],
    stats: &Stats,
) -> f64 {
    let selectivity: f64 = quals.iter().map(|q| join_selectivity(q, stats)).product();
    let mut rows = left.node.cost.rows * right.node.cost.rows * selectivity;
    match kind {
        JoinKind::Left => rows = rows.max(left.node.cost.rows),
        JoinKind::Full => rows = rows.max(left.node.cost.rows).max(right.node.cost.rows),
        _ => {}
    }
    let filter_selectivity: f64 = filter.iter().map(|q| cost::selectivity(&q.expr)).product();
    cost::clamp_rows(rows * filter_selectivity)
}

/// Each kind of join for the two sides in each order that the join can have, in the order that `add_paths_to_joinrel` makes them, with the part of the order of its rows that a node above can use. A full join that no kind of join can do has none.
fn choose<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: &[Rel<'q>],
    right: &[Rel<'q>],
    quals: &[Qual<'q>],
    filter: &[Qual<'q>],
    stats: &Stats,
) -> Vec<(Candidate<'q>, Vec<OrderKey>)> {
    let filter_cost = cost::qual_cost(filter.iter().map(|q| &*q.expr));
    let (left_rels, right_rels) = (&left[0].rels, &right[0].rels);
    let lateral_right = lateral(level, right_rels, left_rels);
    // The orders of the sides: the left side first, and the right side first when the join can have the sides in the other order.
    let mut orders = vec![false];
    if !lateral_right && kind != JoinKind::Left || kind == JoinKind::Left && !lateral_right {
        orders.push(true);
    }
    let mut rels = left_rels.clone();
    rels.extend(right_rels.iter().copied());
    let mut out = Vec::new();
    for swapped in orders {
        let (outers, inners) = if swapped { (right, left) } else { (left, right) };
        let join = match (kind, swapped) {
            (JoinKind::Left, true) => JoinKind::Right,
            (k, _) => k,
        };
        let input = Input { outer: &outers[0], inner: &inners[0], quals, filter_cost, stats };
        let nested = kind == JoinKind::Inner || !swapped && kind == JoinKind::Left;
        let mut candidates: Vec<Candidate<'q>> = sorted_merge(&input, join).into_iter().collect();
        // `match_unsorted_outer`: each plan of the outer side, with the cheapest plan of the inner side or with an inner plan in the order of the merge.
        for (p, outer) in outers.iter().enumerate() {
            let input = Input { outer, ..input };
            let mut each = Vec::new();
            if nested {
                each.extend(nested_loops(&input, join, lateral_right));
            }
            each.extend(presorted_merges(&input, inners, join));
            candidates.extend(each.into_iter().map(|c| Candidate { paths: (p, c.paths.1), ..c }));
        }
        candidates.extend(hash_join(&input, join));
        for candidate in candidates {
            let (outer, inner) = (&outers[candidate.paths.0], &inners[candidate.paths.1]);
            let keys = stats.useful(&rels, &candidate.order(outer, inner, quals));
            out.push((Candidate { swapped, ..candidate }, keys));
        }
    }
    out
}

/// The plan of the join of two sides with the kind of join that `choose` found.
fn finish<'q>(
    level: &Level<'q>,
    best: &Candidate<'q>,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: &[Qual<'q>],
    filter: &[Qual<'q>],
    rows: f64,
) -> Rel<'q> {
    let mut rels = left.rels.clone();
    rels.extend(right.rels.iter().copied());
    let (outer, inner) = if best.swapped { (right, left) } else { (left, right) };
    let mut out = best.make(level.scope, outer, inner, quals, rows);
    out.node.filter = filter.iter().map(|q| q.item()).collect();
    out.rels = rels;
    out
}

/// The items of the problem of the order of the joins of a join domain, as the list of `deconstruct_jointree`, and the conditions of its inner joins. A condition with no column has the relations of its join.
fn flatten<'q>(tree: Tree<'q>, items: &mut Vec<Tree<'q>>, pool: &mut Vec<Qual<'q>>) {
    match tree {
        Tree::Join(join) if join.kind == JoinKind::Inner => {
            let JoinTree { left, right, quals, filter, rels, .. } = *join;
            flatten(left, items, pool);
            flatten(right, items, pool);
            for qual in quals.into_iter().chain(filter) {
                let rels = if qual.rels.is_empty() { rels.clone() } else { qual.rels.clone() };
                pool.push(Qual { rels, ..qual });
            }
        }
        other => items.push(other),
    }
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
    let join_filter = quals.iter().map(|q| q.item()).collect();
    let mut node = Node::new(
        Kind::NestedLoop { join: kind, join_filter },
        level.scope,
        vec![left.node, inner],
    );
    node.filter = filter.iter().map(|q| q.item()).collect();
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

/// A key of a merge join: the index of its clause in the clauses of `split`, and the directions of the sort.
type MergeKey = (usize, bool, bool);

/// A kind of join for an order of the sides, with its cost.
#[derive(Clone, Debug)]
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
    /// For a merge join, its keys.
    keys: Vec<MergeKey>,
    /// The plans of the outer side and of the inner side that the join reads, as indexes into the plans of each side, where the plan with the lowest total cost is 0.
    paths: (usize, usize),
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

/// The nested loops of a join, with their costs as `initial_cost_nestloop` and `final_cost_nestloop`: with the plain inner side, and then with a `Material` node over the inner side, as `match_unsorted_outer` makes them. The inner side gets no `Material` node when it keeps its rows already or reads the outer side.
fn nested_loops<'q>(input: &Input<'_, 'q>, join: JoinKind, lateral: bool) -> Vec<Candidate<'q>> {
    let mut out = Vec::new();
    let mut materialize = vec![false];
    if !lateral && !keeps_rows(&input.inner.node.kind) {
        materialize.push(true);
    }
    for materialize in materialize {
        let (startup, total) = nested_cost(input, materialize);
        out.push(Candidate {
            method: Method::Nested,
            join,
            swapped: false,
            startup,
            total,
            materialize,
            sort: (false, false),
            keys: Vec::new(),
            paths: (0, 0),
            _marker: std::marker::PhantomData,
        });
    }
    out
}

/// The start and the total cost of a nested loop, with a `Material` node on the inner side or with none.
fn nested_cost(input: &Input<'_, '_>, materialize: bool) -> (f64, f64) {
    let o = input.outer.node.cost;
    let (inner, rescan) = if materialize {
        let i = input.inner.node.cost;
        let spill = cost::spill(i.rows, i.width);
        let total = i.total + 2.0 * cost::CPU_OPERATOR * i.rows + spill;
        (Cost { total, ..i }, (0.0, cost::CPU_OPERATOR * i.rows + spill))
    } else {
        (input.inner.node.cost, rescan(&input.inner.node))
    };
    let startup = o.startup + inner.startup;
    let mut run = o.total - o.startup + inner.total - inner.startup;
    if o.rows > 1.0 {
        run += (o.rows - 1.0) * rescan.1;
    }
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(input.quals.iter().map(|q| &*q.expr)) + input.filter_cost;
    run += per_tuple * o.rows * inner.rows;
    (startup, startup + run)
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
    let mut startup = o.startup + i.total + (cost::CPU_OPERATOR * n + cost::CPU_TUPLE) * i.rows;
    let mut run = o.total - o.startup + cost::CPU_OPERATOR * n * o.rows;
    let (buckets, batches) = cost::hash_table(i.rows, i.width);
    if batches > 1.0 {
        // A join in batches writes the inner rows to disk and reads them again, and writes and reads the outer rows of the later batches.
        let (outer_pages, inner_pages) =
            (cost::page_count(o.rows, o.width), cost::page_count(i.rows, i.width));
        startup += cost::SEQ_PAGE * inner_pages;
        run += cost::SEQ_PAGE * (inner_pages + 2.0 * outer_pages);
    }
    // `estimate_hash_bucket_stats`: the fraction of the inner rows in a bucket of all the batches, 0.1 with no statistics.
    let virtual_buckets = buckets * batches;
    let bucket = clauses
        .iter()
        .map(|c| match numdistinct(&c.inner, input.stats) {
            (_, true, _) => 0.1,
            (nd, false, rel) => {
                // The conditions of the relation take away values in the same fraction as rows.
                let nd = match rel {
                    Some((tuples, rows)) if tuples > 0.0 => cost::clamp_rows(nd * rows / tuples),
                    _ => nd,
                };
                (1.0 / nd.clamp(1.0, virtual_buckets)).max(1.0e-6)
            }
        })
        .fold(1.0, f64::min);
    // With no statistics, the most common value of the inner side can be all of its rows. The bucket of that value must fit in `hash_mem`, because more batches cannot split it. A hash join that does not fit costs `disable_cost` more, so that the planner uses it only when it has no other plan.
    if cost::byte_size(i.rows, i.width) > cost::HASH_MEM {
        startup += cost::DISABLE;
    }
    // The conditions of the hash cost as `cost_qual_eval` finds them for each row that a bucket has.
    let qual_cost = cost::qual_cost(clauses.iter().map(|c| &*c.item.expr));
    run += qual_cost * o.rows * cost::clamp_rows(i.rows * bucket) * 0.5;
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(other.iter().map(|q| &*q.expr)) + input.filter_cost;
    run += per_tuple * tuple_count(input, &clauses);
    Some(Candidate {
        method: Method::Hash,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize: false,
        sort: (false, false),
        keys: Vec::new(),
        paths: (0, 0),
        _marker: std::marker::PhantomData,
    })
}

/// True when the order of the rows of a plan is the order of the keys of a merge join, from the first key. `value` gives the value of the side for a clause.
fn ordered(order: &[OrderKey], keys: &[MergeKey], value: impl Fn(usize) -> Expr) -> bool {
    keys.len() <= order.len()
        && keys
            .iter()
            .zip(order)
            .all(|(&(c, desc, nulls_first), k)| k.sorts(&value(c), desc, nulls_first))
}

/// `select_outer_pathkeys_for_merge`: the order of the keys of a merge join that sorts its sides. The keys of the query come first, in its directions, when the conditions have all of them. The condition of an outer join has only the value of its outer side, and only when that side is not the side that gets nulls. When the conditions are all keys of the query, they are the keys. The other keys follow in ascending order, first the keys with more expressions of relations outside the join that a condition makes equal to them.
fn merge_order(input: &Input<'_, '_>, join: JoinKind, clauses: &[Clause<'_>]) -> Vec<MergeKey> {
    let has = |c: &Clause<'_>, e: &Expr| match join {
        JoinKind::Inner => c.outer.same(e) || c.inner.same(e),
        JoinKind::Left => c.outer.same(e),
        JoinKind::Right | JoinKind::Full => false,
    };
    let mut used = vec![false; clauses.len()];
    let mut out = Vec::new();
    let query = &input.stats.query;
    let matches = query.iter().take_while(|(e, _, _)| clauses.iter().any(|c| has(c, e))).count();
    if !query.is_empty() && (matches == query.len() || matches == clauses.len()) {
        for (e, desc, nulls_first) in &query[..matches] {
            if let Some(i) = (0..clauses.len()).find(|&i| !used[i] && has(&clauses[i], e)) {
                used[i] = true;
                out.push((i, *desc, *nulls_first));
            }
        }
        if matches < query.len() {
            return out;
        }
    }
    let mut rels = input.outer.rels.clone();
    rels.extend(input.inner.rels.iter().copied());
    let score = |c: &Clause<'_>| {
        let mut partners: Vec<&Expr> = Vec::new();
        for sides in &input.stats.joins {
            for ((e, _), (other, other_rels)) in sides.iter().zip(sides.iter().rev()) {
                if has(c, e)
                    && other_rels.is_disjoint(&rels)
                    && !partners.iter().any(|p| p.same(other))
                {
                    partners.push(other);
                }
            }
        }
        partners.len()
    };
    let scores: Vec<usize> = clauses.iter().map(score).collect();
    while let Some(i) =
        (0..clauses.len()).filter(|&i| !used[i]).fold(None, |best: Option<usize>, i| match best {
            Some(b) if scores[b] >= scores[i] => Some(b),
            _ => Some(i),
        })
    {
        used[i] = true;
        out.push((i, false, false));
    }
    out
}

/// `find_mergeclauses_for_outer_pathkeys`: the keys of a merge join in the order of the rows of the outer side, when that order has the value of each clause.
fn outer_order(order: &[OrderKey], clauses: &[Clause<'_>]) -> Option<Vec<MergeKey>> {
    let mut used = vec![false; clauses.len()];
    let mut out = Vec::new();
    for k in order {
        let before = out.len();
        for (i, c) in clauses.iter().enumerate() {
            if !used[i] && k.sorts(&c.outer, k.desc, k.nulls_first) {
                used[i] = true;
                out.push((i, k.desc, k.nulls_first));
            }
        }
        if out.len() == before {
            break;
        }
    }
    (out.len() == clauses.len()).then_some(out)
}

/// `sort_inner_and_outer`: the merge join of the cheapest plans of the two sides in the order of `merge_order`.
fn sorted_merge<'q>(input: &Input<'_, 'q>, join: JoinKind) -> Option<Candidate<'q>> {
    let (clauses, _) = split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return None;
    }
    let keys = merge_order(input, join, &clauses);
    merge_join(input, join, keys)
}

/// `generate_mergejoin_paths`: the merge joins of a plan of the outer side in the order of its rows, when that order has the value of each condition. The cheapest plan of the inner side gets a `Sort` when it is not in that order. When it is not, the inner plans in that order with the lowest total cost and with the lowest start cost are inner sides too.
fn presorted_merges<'q>(
    input: &Input<'_, 'q>,
    inners: &[Rel<'q>],
    join: JoinKind,
) -> Vec<Candidate<'q>> {
    let (clauses, _) = split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return Vec::new();
    }
    let Some(keys) = outer_order(&input.outer.order, &clauses) else { return Vec::new() };
    let mut out: Vec<Candidate<'q>> = merge_join(input, join, keys.clone()).into_iter().collect();
    let sorted: Vec<usize> = (0..inners.len())
        .filter(|&j| ordered(&inners[j].order, &keys, |c| clauses[c].inner.clone()))
        .collect();
    let costs = |j: usize| (inners[j].node.cost.startup, inners[j].node.cost.total);
    let by = |a: usize, b: usize, total: bool| {
        let ((sa, ta), (sb, tb)) = (costs(a), costs(b));
        if total {
            ta.total_cmp(&tb).then(sa.total_cmp(&sb))
        } else {
            sa.total_cmp(&sb).then(ta.total_cmp(&tb))
        }
    };
    // `get_cheapest_path_for_pathkeys` and `compare_path_costs`: the first plan with the lowest cost.
    let best = |total: bool| sorted.iter().copied().min_by(|&a, &b| by(a, b, total));
    let presorted = sorted.first() == Some(&0);
    let mut total = None;
    if let Some(j) = best(true)
        && (!presorted || by(j, 0, true).is_lt())
    {
        total = Some(j);
    }
    let mut startup = None;
    if let Some(j) = best(false)
        && (!presorted || by(j, 0, false).is_lt())
        && j != total.unwrap_or(0)
    {
        startup = Some(j);
    }
    for j in total.into_iter().chain(startup) {
        let input = Input { inner: &inners[j], ..*input };
        out.extend(
            merge_join(&input, join, keys.clone()).map(|c| Candidate { paths: (0, j), ..c }),
        );
    }
    out
}

/// The costs of a merge join of the two sides in the order of the keys, as `initial_cost_mergejoin` and `final_cost_mergejoin`. A side that is not in that order gets a `Sort`.
fn merge_join<'q>(
    input: &Input<'_, 'q>,
    join: JoinKind,
    keys: Vec<MergeKey>,
) -> Option<Candidate<'q>> {
    let (clauses, other) =
        split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    // `select_mergejoin_clauses`: a full join is a merge join only when each of its clauses is a merge clause.
    if clauses.is_empty() || join == JoinKind::Full && !other.is_empty() {
        return None;
    }
    let sort_outer = !ordered(&input.outer.order, &keys, |c| clauses[c].outer.clone());
    let sort_inner = !ordered(&input.inner.order, &keys, |c| clauses[c].inner.clone());
    let side = |rel: &Rel<'_>, sorted: bool| {
        let c = rel.node.cost;
        if sorted {
            cost::sort((c.startup, c.total), c.rows, c.width, None)
        } else {
            (c.startup, c.total)
        }
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
    // The inner side needs a `Material` node when it is cheaper, when its node cannot go back to a row that it gave, as `ExecSupportsMarkRestore`, or when its sort does not fit in `work_mem`.
    let inner_cost = input.inner.node.cost;
    let materialize = material < bare
        || !sort_inner
            && !matches!(
                input.inner.node.kind,
                Kind::Sort(_) | Kind::Materialize | Kind::Result { .. }
            )
        || sort_inner && cost::byte_size(inner_cost.rows, inner_cost.width) > cost::WORK_MEM;
    run += if materialize { material } else { bare };
    run +=
        cost::qual_cost(clauses.iter().map(|c| &*c.item.expr)) * (outer_rows + inner_rows * ratio);
    let per_tuple =
        cost::CPU_TUPLE + cost::qual_cost(other.iter().map(|q| &*q.expr)) + input.filter_cost;
    run += per_tuple * tuples;
    Some(Candidate {
        method: Method::Merge,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize,
        sort: (sort_outer, sort_inner),
        keys,
        paths: (0, 0),
        _marker: std::marker::PhantomData,
    })
}

/// The sort keys of the values of the clauses of a merge join, with the directions of the keys of the join.
fn merge_keys<'q>(values: Vec<(Expr, bool, bool)>) -> Vec<SortKey<'q>> {
    values
        .into_iter()
        .map(|(v, desc, nulls_first)| SortKey {
            item: Item { expr: Cow::Owned(v), below: true },
            desc,
            using: None,
            nulls_first,
        })
        .collect()
}

/// The clauses of a merge join in the order of its keys, each with the directions of its key.
fn merge_clauses<'q>(clauses: Vec<Clause<'q>>, keys: &[MergeKey]) -> Vec<(Clause<'q>, bool, bool)> {
    let mut slots: Vec<Option<Clause<'q>>> = clauses.into_iter().map(Some).collect();
    keys.iter()
        .filter_map(|&(c, desc, nulls_first)| Some((slots[c].take()?, desc, nulls_first)))
        .collect()
}

impl<'q> Candidate<'q> {
    /// The order of the rows of the join, as `build_join_pathkeys`: the order of the outer side for a nested loop, the order of the values of the conditions for a merge join that is not a full join, and no order for a hash join.
    fn order(&self, outer: &Rel<'q>, inner: &Rel<'q>, quals: &[Qual<'q>]) -> Vec<OrderKey> {
        match self.method {
            Method::Nested => outer.order.clone(),
            Method::Merge if self.join != JoinKind::Full => {
                let clauses = split(quals, &outer.rels, &inner.rels, exprs::can_merge).0;
                merge_clauses(clauses, &self.keys)
                    .into_iter()
                    .map(|(c, desc, nulls_first)| OrderKey {
                        exprs: vec![c.outer, c.inner],
                        desc,
                        nulls_first,
                    })
                    .collect()
            }
            Method::Merge | Method::Hash => Vec::new(),
        }
    }

    /// The plan of the join.
    fn make(
        &self,
        scope: usize,
        outer: Rel<'q>,
        inner: Rel<'q>,
        quals: &[Qual<'q>],
        rows: f64,
    ) -> Rel<'q> {
        let order = self.order(&outer, &inner, quals);
        let node = match self.method {
            Method::Nested => {
                let inner_node =
                    if self.materialize { materialize(inner.node) } else { inner.node };
                let join_filter = quals.iter().map(|q| q.item()).collect();
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
                let join_filter = other.iter().map(|q| q.item()).collect();
                Node::new(
                    Kind::HashJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer.node, hash],
                )
            }
            Method::Merge => {
                let (clauses, other) = split(quals, &outer.rels, &inner.rels, exprs::can_merge);
                let clauses = merge_clauses(clauses, &self.keys);
                let outer_values =
                    clauses.iter().map(|(c, d, n)| (c.outer.clone(), *d, *n)).collect();
                let inner_values =
                    clauses.iter().map(|(c, d, n)| (c.inner.clone(), *d, *n)).collect();
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
                    inner_node = merge_materialize(inner_node);
                }
                let cond = clauses.into_iter().map(|(c, _, _)| c.item).collect();
                let join_filter = other.iter().map(|q| q.item()).collect();
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

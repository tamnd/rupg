//! The planner: the plan of a query, as `planner.c` and `createplan.c` make it (document 15 section 15.9).
//!
//! [`plan`] turns the query tree of the analyzer into a [`Plan`] of `rupg-plan`. The planner has no statistics and no index scans yet, so it chooses its nodes as PostgreSQL does for tables that `ANALYZE` has not seen: a sequential scan of each table, the join of the lowest cost of a merge join, a hash join and a nested loop, and the estimates of `costsize.c` with the defaults of `selfuncs.c`. The unique indexes of a table change the estimates of the `=` conditions of its columns and the costs of its joins, as `has_unique_index` and `innerrel_is_unique` use them.

#![forbid(unsafe_code)]

mod cost;
mod exprs;
mod from;
mod output;
mod upper;

use rupg_analyze::{
    Cte, Expr, ExprKind, FromItem, Query, SubLink, SubLinkKind, TABLE_OID_ATTNUM, Var, Write,
    WriteKind,
};
use rupg_plan::{Item, Kind, Label, Leaf, Node, Operation, Plan, Scope, SubPlan};
use rupg_types::oid;

/// The tables of the inheritance trees of the relations of the queries, which the planner has no catalog for.
pub trait Members: std::fmt::Debug {
    /// The tables that a scan of the relation `relation` of the query reads, as `expand_inherited_rtentry` and `prune_append_rel_partitions` choose them with the conditions `conds` of the scan, or `None` for a relation that is one table.
    fn members(&self, query: &Query, relation: usize, conds: &[&Expr]) -> Option<Vec<Member>>;

    /// The valid indexes of the table with the OID `table`, in the order of their OIDs, as `get_relation_info` reads them from `RelationGetIndexList`.
    fn indexes(&self, table: u32) -> Vec<Index>;
}

/// An index of a table, with the facts of `IndexOptInfo` that the planner uses.
#[derive(Clone, Debug)]
pub struct Index {
    pub oid: u32,
    /// `indexkeys`: the column number of each key column in the table, or 0 for an expression.
    pub keys: Vec<i16>,
    /// `indisunique`.
    pub unique: bool,
    /// `indnullsnotdistinct`: the index has one null at most in a key column.
    pub nulls_not_distinct: bool,
    /// `indimmediate`: false for the index of a deferrable constraint.
    pub immediate: bool,
    /// True for a partial index, which has a predicate.
    pub partial: bool,
}

/// A table of an inheritance tree that a scan reads.
#[derive(Clone, Debug)]
pub struct Member {
    pub oid: u32,
    /// The type and the type modifier of each column of the table that is not dropped.
    pub columns: Vec<(u32, i32)>,
    /// `relhassubclass`: the table has children, so the planner takes no pages for it when it has none.
    pub has_children: bool,
}

/// The plan of a query, with the members of the relations that `members` gives.
pub fn plan<'q>(query: &'q Query, members: &'q dyn Members) -> Plan<'q> {
    let mut b = Builder { members: Some(members), ..Builder::default() };
    let mut root = b.query(query, None);
    let mut pending = std::mem::take(&mut b.pending);
    attach(&mut root, &mut pending);
    // A subplan that no node holds, such as one in an expression that the planner made again, goes to the top node.
    root.sub_plans.extend(pending.into_iter().map(|(_, sub)| sub));
    let mut order = std::mem::take(&mut b.current);
    for group in b.groups {
        order.extend(group);
    }
    Plan { root, scopes: b.scopes, sublinks: b.labels, order }
}

/// The state of the planner for a statement, as `PlannerGlobal`.
#[derive(Debug, Default)]
struct Builder<'q> {
    scopes: Vec<Scope<'q>>,
    labels: Vec<Label<'q>>,
    /// The number of subplans of each kind of subquery so far, for their names, as `choose_plan_name`.
    counts: [usize; 6],
    /// The scopes of the subplan that the planner makes now, in the order of the range table.
    current: Vec<usize>,
    /// The scopes of each subplan that is done, in the order that the subplans end.
    groups: Vec<Vec<usize>>,
    /// The subplans that run for each row, which go to the node that holds their subquery.
    pending: Vec<(&'q SubLink, SubPlan<'q>)>,
    /// The queries of `WITH` that the query and the queries outside it give.
    ctes: Vec<CteUse<'q>>,
    /// The rows of the part of each recursive query that is not recursive, by the number of its work table, for the estimate of a `WorkTableScan`.
    work: Vec<(usize, f64)>,
    /// True when the next query is the subquery of `EXISTS`.
    exists: bool,
    members: Option<&'q dyn Members>,
}

/// A query of `WITH` in the scope of the planner.
#[derive(Debug)]
struct CteUse<'q> {
    cte: &'q Cte,
    /// True when each reference reads its own copy of the query as a subquery, as `inline_cte`. Else the plan runs the query once as an `InitPlan`, and each reference is a `CteScan`.
    inline: bool,
    /// The rows of the plan of a query that is not inline.
    rows: f64,
}

/// A query that the planner plans, as `PlannerInfo`.
#[derive(Debug)]
pub(crate) struct Level<'q> {
    pub(crate) scope: usize,
    pub(crate) query: &'q Query,
    /// The scope and the query of the expressions of the nodes above a set operation: the leftmost `SELECT` of the operation, as `EXPLAIN` shows them. For another query it is the query.
    pub(crate) display: usize,
    pub(crate) display_query: &'q Query,
    /// The subplans that run once before the top node: the queries of `WITH` and the subqueries with no reference to the query.
    pub(crate) init: Vec<SubPlan<'q>>,
    /// True when the query gives no targets, as `simplify_EXISTS_query` makes the subquery of `EXISTS`.
    pub(crate) no_targets: bool,
    /// The conditions of joins that the equivalence classes took the place of. The scans below a join still give their columns, as `attr_needed` keeps them.
    pub(crate) joined: Vec<&'q Expr>,
    /// The estimates of the relations of `FROM`, which `estimate_num_groups` reads.
    pub(crate) stats: from::Stats,
    /// The junk columns of the row marks, which the rows of the query carry after its targets.
    pub(crate) marks: Vec<Expr>,
}

impl<'q> Level<'q> {
    /// The expression of a target of the query of the display scope.
    pub(crate) fn target(&self, index: usize) -> &'q Expr {
        &self.display_query.targets[index].expr
    }
}

/// The query of the rows of a statement with the conditions of check constraints, which `with_checks` of the analyzer puts around the query of the rows.
/// True when `simplify_EXISTS_query` takes out the targets of the subquery of `EXISTS`: a query with no set operation, aggregate, window, `HAVING`, `LIMIT`, `OFFSET` or row lock.
fn simple_exists(query: &Query) -> bool {
    query.set_op.is_none()
        && !query.grouped
        && query.having.is_none()
        && query.windows.is_empty()
        && query.limit.is_none()
        && query.offset.is_none()
        && query.row_marks.is_empty()
        && query.sort.is_empty()
        && query.distinct.is_empty()
}

fn unwrap_checks(query: &Query) -> &Query {
    let Some(write) = &query.write else { return query };
    match query.relations.as_slice() {
        [relation]
            if !write.checks.is_empty() && relation.oid == 0 && relation.name == write.name =>
        {
            relation.subquery.as_deref().unwrap_or(query)
        }
        _ => query,
    }
}

impl<'q> Builder<'q> {
    /// `subquery_planner`: the plan of a query, whose scope is in the scope `parent`.
    fn query(&mut self, outer: &'q Query, parent: Option<usize>) -> Node<'q> {
        let write = outer.write.as_ref();
        let query = unwrap_checks(outer);
        let target = write.is_some_and(|w| w.kind == WriteKind::Insert);
        let scope = self.push_scope(query, parent, target);
        let no_targets = std::mem::take(&mut self.exists) && simple_exists(query);
        let marks = self.marks(query, write);
        let mut level = Level {
            scope,
            query,
            display: scope,
            display_query: query,
            init: Vec::new(),
            no_targets,
            joined: Vec::new(),
            stats: from::Stats::default(),
            marks,
        };
        let depth = self.ctes.len();
        if !std::ptr::eq(outer, query) {
            self.ctes_of(outer, &mut level);
        }
        self.ctes_of(query, &mut level);
        for expr in preprocess_order(query) {
            self.sublinks_in(expr, &mut level);
        }
        let (plans, joined, stats) = match &query.set_op {
            Some(op) => (
                vec![(self.set_op(&mut level, op), Vec::new())],
                Vec::new(),
                from::Stats::default(),
            ),
            None => self.from(&level),
        };
        level.joined = joined;
        level.stats = stats;
        let mut node = self.upper(&level, plans);
        if let Some(write) = write {
            node = self.modify(&level, write, node);
        }
        output::level(&level, &mut node);
        self.leaves(&level, &mut node, false);
        let mut init = std::mem::take(&mut level.init);
        init.append(&mut node.init_plans);
        node.init_plans = init;
        self.ctes.truncate(depth);
        node
    }

    /// `preprocess_rowmarks` and `preprocess_targetlist`: the junk columns that the rows of a query carry for its row marks. `FOR UPDATE` and `FOR SHARE` mark each relation of `FROM`, the locked tables first, and an `UPDATE` or a `DELETE` marks each relation other than its target. A table gives `ctid`, and a function, a `VALUES` list or a query of `WITH` gives the whole row, as `ROW_MARK_COPY`. A table with an inheritance tree also gives `tableoid`, which `expand_inherited_rtentry` adds after the other columns. A subquery gives no column yet.
    fn marks(&self, query: &'q Query, write: Option<&Write>) -> Vec<Expr> {
        let target = match write.map(|w| &w.kind) {
            Some(WriteKind::Update | WriteKind::Delete) => Some(0),
            _ if !query.row_marks.is_empty() && write.is_none() => None,
            _ => return Vec::new(),
        };
        let mut items: Vec<&FromItem> = query.from.iter().collect();
        let mut base = Vec::new();
        while let Some(item) = items.pop() {
            match item {
                FromItem::Relation(r) => base.push(*r),
                FromItem::Join(join) => items.extend([&join.left, &join.right]),
            }
        }
        base.sort_unstable();
        base.retain(|&r| Some(r) != target);
        let table = |r: usize| {
            let relation = &query.relations[r];
            relation.oid != 0
                && relation.subquery.is_none()
                && relation.function.is_none()
                && relation.values.is_none()
                && relation.cte.is_none()
        };
        let mut order: Vec<usize> = query
            .row_marks
            .iter()
            .map(|mark| mark.relation)
            .filter(|&r| table(r) && base.contains(&r))
            .collect();
        for r in base {
            if !order.contains(&r) {
                order.push(r);
            }
        }
        let var = |r: usize, attnum: i16, ty: u32| {
            Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), ty)
        };
        let mut out = Vec::new();
        let mut parents = Vec::new();
        for r in order {
            if table(r) {
                out.push(var(r, -1, oid::TID));
                if self.members.and_then(|m| m.members(query, r, &[])).is_some() {
                    parents.push(var(r, TABLE_OID_ATTNUM, oid::OID));
                }
            } else if query.relations[r].subquery.is_none() {
                out.push(var(r, 0, oid::RECORD));
            }
        }
        out.extend(parents);
        out
    }

    /// A new scope in the current subplan.
    fn push_scope(&mut self, query: &'q Query, parent: Option<usize>, target: bool) -> usize {
        self.scopes.push(Scope { query, parent, target, leaf: None });
        let index = self.scopes.len() - 1;
        self.current.push(index);
        index
    }

    /// The scopes of the scans of the members of the inheritance trees of a query, in the order of the nodes. A member has a name of its own when it is one of the members of an `Append` or the target of a `ModifyTable`. `append` is true for the members of an `Append`.
    fn leaves(&mut self, level: &Level<'q>, node: &mut Node<'q>, append: bool) {
        if node.scope != level.scope {
            return;
        }
        if let (Some(_), Kind::SeqScan(r)) = (node.member, &node.kind) {
            let write = level.query.write.as_ref();
            let target = *r == 0
                && write.is_some_and(|w| matches!(w.kind, WriteKind::Update | WriteKind::Delete));
            let leaf = Leaf { of: level.scope, relation: *r, renamed: append || target };
            let parent = self.scopes[level.scope].parent;
            self.scopes.push(Scope { query: level.query, parent, target: false, leaf: Some(leaf) });
            node.scope = self.scopes.len() - 1;
            self.current.push(node.scope);
            return;
        }
        let append =
            matches!(node.kind, Kind::Append) && node.children.iter().all(|c| c.member.is_some());
        for child in &mut node.children {
            self.leaves(level, child, append);
        }
        if let Kind::Modify {
            operation: Operation::Update | Operation::Delete,
            relation: 0,
            targets,
            ..
        } = &mut node.kind
        {
            let scopes = &self.scopes;
            let mut found = Vec::new();
            for child in &node.children {
                member_scopes(child, scopes, &mut found);
            }
            *targets = found;
        }
    }

    /// The plan of a subplan, whose scopes take part in the names of the relations after the scopes of the statement.
    fn subplan(&mut self, f: impl FnOnce(&mut Self) -> Node<'q>) -> Node<'q> {
        let saved = std::mem::take(&mut self.current);
        let node = f(self);
        let group = std::mem::replace(&mut self.current, saved);
        self.groups.push(group);
        node
    }

    /// `SS_process_ctes`: the queries of `WITH` of a query. A query that only one reference reads and that has no volatile function goes into the reference, as `inline_cte`. A `SELECT` that no reference reads has no plan.
    fn ctes_of(&mut self, query: &'q Query, level: &mut Level<'q>) {
        for cte in &query.ctes {
            let select = cte.write.is_none();
            let refs = exprs::cte_references(query, &cte.name);
            if select && refs == 0 {
                continue;
            }
            let inline = select
                && !cte.recursive
                && cte.materialized != Some(true)
                && (refs == 1 || cte.materialized == Some(false))
                && !exprs::query_volatile(&cte.query);
            let mut rows = 1000.0;
            if !inline {
                let scope = level.scope;
                let plan = self.subplan(|b| b.query(&cte.query, Some(scope)));
                rows = plan.cost.rows;
                level.init.push(SubPlan { name: format!("CTE {}", cte.name), plan });
            }
            self.ctes.push(CteUse { cte, inline, rows });
        }
    }

    /// The query of `WITH` that a reference names, as the query that holds the reference sees it.
    pub(crate) fn cte(&self, name: &str) -> Option<&CteUse<'q>> {
        self.ctes.iter().rev().find(|c| c.cte.name == name)
    }

    /// `SS_process_sublinks`: the subplans of the subqueries in an expression of the query, the subqueries in the test of a subquery first.
    fn sublinks_in(&mut self, expr: &'q Expr, level: &mut Level<'q>) {
        for child in expr.children() {
            self.sublinks_in(child, level);
        }
        if let ExprKind::SubLink(link) = &expr.kind {
            self.sublink(link, level);
        }
    }

    /// `make_subplan`: the plan of a subquery in an expression. A subquery with no reference to the query runs once as an `InitPlan`, except for `ANY` and `ALL`, which keep its rows in a hash table or in a `Material` node.
    fn sublink(&mut self, link: &'q SubLink, level: &mut Level<'q>) {
        let (index, prefix) = match link.kind {
            SubLinkKind::Exists => (0, "exists"),
            SubLinkKind::Expr => (1, "expr"),
            SubLinkKind::Any => (2, "any"),
            SubLinkKind::All => (3, "all"),
            SubLinkKind::Array => (4, "array"),
            SubLinkKind::RowCompare => (5, "rowcompare"),
        };
        self.counts[index] += 1;
        let name = format!("{prefix}_{}", self.counts[index]);
        let correlated = exprs::correlated(&link.query, 1);
        let scope = level.scope;
        self.exists = link.kind == SubLinkKind::Exists;
        let mut plan = self.subplan(|b| b.query(&link.query, Some(scope)));
        let init = !correlated && !matches!(link.kind, SubLinkKind::Any | SubLinkKind::All);
        let hashed = !correlated && link.kind == SubLinkKind::Any && hashable_test(link);
        if !correlated && !init && !hashed {
            plan = upper::materialize(plan);
        }
        self.labels.push(Label { link, name: name.clone(), init, hashed });
        if init {
            level.init.push(SubPlan { name: format!("InitPlan {name}"), plan });
        } else {
            self.pending.push((link, SubPlan { name: format!("SubPlan {name}"), plan }));
        }
    }
}

/// The scopes of the members of the relation 0 of the query below a node, which a `ModifyTable` changes.
fn member_scopes(node: &Node<'_>, scopes: &[Scope<'_>], out: &mut Vec<usize>) {
    if node.member.is_some()
        && let Some(leaf) = scopes.get(node.scope).and_then(|s| s.leaf)
        && leaf.relation == 0
    {
        out.push(node.scope);
        return;
    }
    for child in &node.children {
        member_scopes(child, scopes, out);
    }
}

/// True when each comparison of the test of `ANY` has an operator that can use a hash table, as `testexpr_is_hashable`.
fn hashable_test(link: &SubLink) -> bool {
    fn hashable(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bool(rupg_analyze::BoolOp::And, args) => args.iter().all(hashable),
            _ => exprs::binary(expr).is_some_and(|(op, left, right)| {
                exprs::can_hash(op)
                    && !matches!(left.kind, ExprKind::SubColumn(_))
                    && matches!(right.kind, ExprKind::SubColumn(_))
            }),
        }
    }
    link.test.as_ref().is_some_and(hashable)
}

/// The expressions of a query in the order that `subquery_planner` gives them to `preprocess_expression`: the targets, the conditions of `FROM` and `WHERE`, `HAVING`, the offsets of the frames, `OFFSET`, `LIMIT`, then the functions, the `VALUES` lists and the arguments of `TABLESAMPLE` of the relations.
fn preprocess_order(query: &Query) -> Vec<&Expr> {
    fn joins<'a>(item: &'a FromItem, out: &mut Vec<&'a Expr>) {
        if let FromItem::Join(j) = item {
            joins(&j.left, out);
            joins(&j.right, out);
            out.extend(&j.on);
        }
    }
    let mut out: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    for item in &query.from {
        joins(item, &mut out);
    }
    out.extend(query.filter.iter().chain(&query.having));
    for window in &query.windows {
        out.extend(window.frame.start.iter().chain(&window.frame.end));
    }
    out.extend(query.offset.iter().chain(&query.limit));
    for relation in &query.relations {
        if let Some(function) = &relation.function {
            out.extend(function.calls.iter().map(|(call, _)| call));
        }
        out.extend(relation.values.iter().flatten().flatten());
        if let Some(sample) = &relation.tablesample {
            out.extend(sample.args.iter().chain(&sample.repeatable));
        }
    }
    out
}

/// Moves each subplan that runs for each row to the node with the expression that holds its subquery, as `SS_finalize_plan` sees it.
fn attach<'q>(node: &mut Node<'q>, pending: &mut Vec<(&'q SubLink, SubPlan<'q>)>) {
    if !pending.is_empty() {
        let mut links = Vec::new();
        for item in own_items(node) {
            sublinks(&item.expr, &mut links);
        }
        let found: Vec<*const SubLink> = links.into_iter().map(std::ptr::from_ref).collect();
        for link in found {
            if let Some(at) = pending.iter().position(|(l, _)| std::ptr::eq(*l, link)) {
                let (_, sub) = pending.remove(at);
                node.sub_plans.push(sub);
            }
        }
    }
    for child in &mut node.children {
        attach(child, pending);
    }
    for sub in node.init_plans.iter_mut().chain(&mut node.sub_plans) {
        attach(&mut sub.plan, pending);
    }
}

/// The subqueries of an expression outside the queries of other subqueries, in the order of the text.
fn sublinks<'e>(expr: &'e Expr, out: &mut Vec<&'e SubLink>) {
    for child in expr.children() {
        sublinks(child, out);
    }
    if let ExprKind::SubLink(link) = &expr.kind {
        out.push(link);
    }
}

/// The expressions that a node computes: its target list apart from the values that it reads from below, its conditions and the other expressions of its kind.
fn own_items<'n, 'q>(node: &'n Node<'q>) -> Vec<&'n Item<'q>> {
    let mut out: Vec<&Item<'q>> = node.output.iter().filter(|i| !i.below).collect();
    match &node.kind {
        Kind::Result { one_time, .. } => out.extend(one_time),
        Kind::NestedLoop { join_filter, .. } => out.extend(join_filter),
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            out.extend(cond.iter().chain(join_filter));
        }
        _ => {}
    }
    out.extend(&node.filter);
    out
}

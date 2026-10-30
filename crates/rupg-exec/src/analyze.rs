//! The counts of `EXPLAIN ANALYZE`: a run of the nodes of a plan, as `ExecProcNode` pulls the rows of each node from the nodes below it, that counts the rows and the runs of each node, as its `Instrumentation` keeps them.
//!
//! The executor gives the rows of a query with no plan, so the counts come from a second run that follows the nodes of the plan of `EXPLAIN`. A scan reads the rows of the relations of its query, a join tests its conditions on the rows of its two sides, and the nodes above the join read the groups and the rows that the executor makes for the query.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use rupg_analyze::{ExprKind, JoinKind, Query, SubLinkKind};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_plan::{Actual, Actuals, Item, Kind, Node, Plan, Strategy, node_key};
use rupg_types::Value;

use crate::{Cache, Eval, agg, not_yet, scan, set_levels, window, with_subqueries};

/// The counts of the nodes of `plan` after a run of the plan with the values `params` of its parameters. With `project`, the run computes the targets of each row of the statement, so that their functions run as in the run of a `SELECT`.
///
/// # Errors
///
/// The errors of the run, and a feature error for a node that the run does not have yet.
pub fn analyze(
    plan: &Plan<'_>,
    params: &[Value],
    session: &dyn Session,
    project: bool,
) -> Result<Actuals> {
    analyze_in(plan, params, session, project, &Cache::default())
}

/// [`analyze`] with the cache `cache`, which has the rows of the statements of `WITH` that changed data.
pub(crate) fn analyze_in(
    plan: &Plan<'_>,
    params: &[Value],
    session: &dyn Session,
    project: bool,
    cache: &Cache,
) -> Result<Actuals> {
    let cx = Cx {
        plan,
        params,
        session,
        cache,
        tables: std::cell::RefCell::new(HashMap::new()),
        ctes: std::cell::RefCell::new(HashMap::new()),
    };
    let mut root = Exec::new(&plan.root);
    let top = plan.scopes.first().map(|s| s.query);
    let project = project
        && top.is_some_and(|q| {
            q.write.is_none()
                && !q.grouped
                && !q.has_target_srfs()
                && window::calls(q).is_empty()
                && q.set_op.is_none()
        });
    while let Some(row) = root.next(&cx, &[])? {
        if project && row.values.is_none() {
            let query = top.unwrap_or_else(|| unreachable!());
            let tables = cx.tables(0)?;
            let eval = Eval::new(cx.params, cx.session, &tables, cx.cache, &[]);
            let mut eval = eval.at(&row.tuple);
            for target in query.targets.iter().filter(|t| !t.junk) {
                eval.eval(&target.expr)?;
            }
        }
    }
    // `ExecutorEnd`: the `CTE` subplans give the rows that their scans read.
    root.ctes(&cx)?;
    let mut actuals = Actuals::new();
    root.finish(&mut actuals);
    Ok(actuals)
}

/// The state that the nodes of a run share.
struct Cx<'p, 'q, 'c> {
    plan: &'p Plan<'q>,
    params: &'c [Value],
    session: &'c dyn Session,
    cache: &'c Cache,
    /// The rows of the relations of each scope, with the rows of its subqueries and functions.
    tables: std::cell::RefCell<HashMap<usize, Rc<scan::Tables>>>,
    /// For each query of `WITH`, the rows that its scans read, and true when a scan read past its last row.
    ctes: std::cell::RefCell<HashMap<String, (usize, bool)>>,
}

impl Cx<'_, '_, '_> {
    /// The query of a scope.
    fn query(&self, scope: usize) -> &Query {
        self.plan.scopes[scope].query
    }

    /// The scope whose rows a scope reads: the scope of the query of the scan of a member.
    fn home(&self, scope: usize) -> usize {
        self.plan.scopes[scope].leaf.map_or(scope, |leaf| leaf.of)
    }

    /// The rows of the relations of a scope, with the rows of its subqueries, its functions and its lists of `VALUES`.
    fn tables(&self, scope: usize) -> Result<Rc<scan::Tables>> {
        let scope = self.home(scope);
        if let Some(tables) = self.tables.borrow().get(&scope) {
            return Ok(Rc::clone(tables));
        }
        let query = self.query(scope);
        if scope != 0 && crate::correlated(query) {
            return Err(not_yet("EXPLAIN ANALYZE of a correlated subquery"));
        }
        let base = self.cache.tables(query, self.session)?;
        let tables =
            with_subqueries(query, &base, self.params, self.session, self.cache, &[], None)?;
        if tables.needs.iter().any(|n| !n.is_empty()) {
            return Err(not_yet("EXPLAIN ANALYZE of a LATERAL reference"));
        }
        self.tables.borrow_mut().insert(scope, Rc::clone(&tables));
        Ok(tables)
    }

    /// The value of an expression of a scope for a tuple of its relations.
    fn value(&self, scope: usize, expr: &rupg_analyze::Expr, tuple: &[usize]) -> Result<Value> {
        let tables = self.tables(scope)?;
        let eval = Eval::new(self.params, self.session, &tables, self.cache, &[]);
        eval.at(tuple).eval(expr)
    }

    /// True when each condition of a list is true for a tuple.
    fn test(&self, scope: usize, items: &[Item<'_>], tuple: &[usize]) -> Result<bool> {
        if items.is_empty() {
            return Ok(true);
        }
        let tables = self.tables(scope)?;
        let eval = Eval::new(self.params, self.session, &tables, self.cache, &[]);
        let mut eval = eval.at(tuple);
        for item in items {
            if eval.eval(&item.expr)? != Value::Bool(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// A tuple of a scope with the rows of the outer side `context` of a nested loop, or with no row when the context is of another query.
    fn tuple(&self, scope: usize, context: &[usize]) -> Vec<usize> {
        let width = self.query(scope).relations.len();
        if context.len() == width { context.to_vec() } else { vec![scan::NONE; width] }
    }

    /// True when two scopes read the rows of the same query, as a scan of a member reads the rows of its relation.
    fn same(&self, a: usize, b: usize) -> bool {
        std::ptr::eq(self.query(a), self.query(b))
    }

    /// The relation of the query of `scope` whose subquery gives the rows of the scope `inner`, as a `SubqueryScan` that the plan removed reads them.
    fn relation_of(&self, scope: usize, inner: usize) -> Option<usize> {
        let query = self.query(scope);
        let mut at = Some(inner);
        while let Some(s) = at {
            let found = query.relations.iter().position(|r| {
                r.subquery.as_deref().is_some_and(|sub| std::ptr::eq(sub, self.query(s)))
            });
            if found.is_some() {
                return found;
            }
            at = self.plan.scopes[s].parent;
        }
        None
    }
}

/// A row of a node: the row of each relation of its query, and the values of the row when the node computes them, as an `Aggregate` does.
#[derive(Clone, Debug)]
struct Row {
    tuple: Vec<usize>,
    values: Option<Rc<Vec<Value>>>,
}

impl Row {
    fn of(tuple: Vec<usize>) -> Row {
        Row { tuple, values: None }
    }
}

/// The tuple of a join of a row of the outer side and a row of the inner side. The outer side keeps its rows.
fn merge(outer: &[usize], inner: &[usize]) -> Vec<usize> {
    let mut out = outer.to_vec();
    for (o, &i) in out.iter_mut().zip(inner) {
        if *o == scan::NONE {
            *o = i;
        }
    }
    out
}

/// The tuple of a row of a side of a join with no row of the relations that only the other side fills.
fn without(tuple: &[usize], other: &[usize]) -> Vec<usize> {
    let mut out = tuple.to_vec();
    for &r in other {
        if let Some(slot) = out.get_mut(r) {
            *slot = scan::NONE;
        }
    }
    out
}

/// The width of the rows of a node, as its plan estimates it.
fn width(node: &Node<'_>) -> u64 {
    u64::try_from(node.cost.width).unwrap_or_default()
}

/// The memory of the hash table of a hashed `Aggregate` with `groups` groups: the 32 kB of its first blocks, and the entries after them.
fn hashagg_kilobytes(groups: usize, width: u64) -> u64 {
    31 + kilobytes(groups as u64 * (width + 64)).max(1)
}

/// The time since `started`, a time of [`Session::clock`] in microseconds.
fn elapsed(cx: &Cx<'_, '_, '_>, started: i64) -> Duration {
    Duration::from_micros(u64::try_from(cx.session.clock() - started).unwrap_or_default())
}

/// `BYTES_TO_KILOBYTES`: the kilobytes of a number of bytes, rounded up.
fn kilobytes(bytes: u64) -> u64 {
    bytes.div_ceil(1024)
}

/// The hash table of the inner side of a hash join: the rows by the values of the keys of the inner side, and the rows that a key does not find.
#[derive(Default)]
struct Probe {
    rows: Vec<Row>,
    keys: BTreeMap<Vec<scan::Key>, Vec<usize>>,
    /// The rows whose keys have no value of [`scan::Key`], which each outer row tests.
    loose: Vec<usize>,
    /// The keys of the two sides, the outer expression and the inner expression of each `=` of the condition.
    pairs: Option<Vec<(rupg_analyze::Expr, rupg_analyze::Expr)>>,
    matched: Vec<bool>,
}

/// The state of a node in the run.
struct Exec<'p, 'q> {
    node: &'p Node<'q>,
    kids: Vec<Exec<'p, 'q>>,
    inits: Vec<Exec<'p, 'q>>,
    subs: Vec<Exec<'p, 'q>>,
    actual: Actual,
    /// True when the node ran in its current loop, as `InstrStartNode` marks it.
    running: bool,
    /// The time of the node in its current loop, and the time to its first row.
    spent: Duration,
    first: Option<Duration>,
    /// True when the subplans of the node ran.
    inited: bool,
    /// The place of the next row of a scan, or of the rows that the node keeps.
    pos: usize,
    /// The rows of a scan, or the rows that the node keeps, as a `Sort` or a `Materialize` does.
    buf: Vec<Row>,
    /// True when the node read all the rows that it keeps, or when the node gives no more rows.
    filled: bool,
    done: bool,
    /// The row of the outer side of a join, and true when an inner row matched it.
    outer: Option<Row>,
    matched: bool,
    /// The rows of the inner side of a join that match the outer row, and the next one.
    cands: Vec<usize>,
    cand: usize,
    /// The hash table of a hash join, or the rows of the inner side of a merge join.
    probe: Option<Probe>,
    /// The outer row that a hash join reads before it builds its table.
    pending: Option<Row>,
    /// The keys that a `Unique` or a hashed `Aggregate` saw.
    seen: HashSet<String>,
    /// The rows of each key of a `Memoize`, with true when the entry has all the rows of the key.
    memo: HashMap<String, (Vec<Row>, bool)>,
    memo_key: Option<String>,
    /// The rows that a `Limit` gave and skipped, its offset and its count.
    given: usize,
    limits: Option<(usize, Option<usize>)>,
    /// The rows that a `Limit` above a `Sort` reads, which make the sort bounded.
    bound: Option<usize>,
    /// The kid of an `Append` that gives rows now.
    kid: usize,
    /// The number of the next row of each subquery that the plan removed, as the relation of the query reads it.
    converted: Vec<usize>,
}

impl<'p, 'q> Exec<'p, 'q> {
    fn new(node: &'p Node<'q>) -> Exec<'p, 'q> {
        Exec {
            node,
            kids: node.children.iter().map(Exec::new).collect(),
            inits: node.init_plans.iter().map(|s| Exec::new(&s.plan)).collect(),
            subs: node.sub_plans.iter().map(|s| Exec::new(&s.plan)).collect(),
            actual: Actual::default(),
            running: false,
            spent: Duration::ZERO,
            first: None,
            inited: false,
            pos: 0,
            buf: Vec::new(),
            filled: false,
            done: false,
            outer: None,
            matched: false,
            cands: Vec::new(),
            cand: 0,
            probe: None,
            pending: None,
            seen: HashSet::new(),
            memo: HashMap::new(),
            memo_key: None,
            given: 0,
            limits: None,
            bound: None,
            kid: 0,
            converted: Vec::new(),
        }
    }

    /// `ExecProcNode` with `InstrStartNode` and `InstrStopNode`: the next row of the node, or `None` at the end of its rows in this loop.
    fn next(&mut self, cx: &Cx<'_, '_, '_>, context: &[usize]) -> Result<Option<Row>> {
        let started = cx.session.clock();
        self.running = true;
        if !self.inited {
            self.inited = true;
            self.run_subplans(cx)?;
        }
        let row = self.step(cx, context)?;
        let row = match row {
            Some(row) => Some(self.convert(cx, row, None)?),
            None => None,
        };
        self.spent += elapsed(cx, started);
        if row.is_some() {
            self.actual.rows += 1;
            if self.first.is_none() {
                self.first = Some(self.spent);
            }
        }
        Ok(row)
    }

    /// `InstrEndLoop`: the end of a loop of a node that ran in it.
    fn end_loop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        self.actual.loops += 1;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        self.actual.startup += ms(self.first.unwrap_or(self.spent));
        self.actual.total += ms(self.spent);
        self.spent = Duration::ZERO;
        self.first = None;
    }

    /// `ExecReScan`: the end of the loop of the node, and a new scan of its rows. A node whose rows do not change keeps them, as `ExecReScanSort` and `ExecReScanMaterial` keep them when no parameter of the nodes below changes.
    fn rescan(&mut self, cx: &Cx<'_, '_, '_>) {
        self.end_loop();
        let keeps = matches!(
            self.node.kind,
            Kind::Materialize
                | Kind::Sort(_)
                | Kind::IncrementalSort { .. }
                | Kind::Aggregate { .. }
        ) && self.filled
            && !depends(cx, self.node);
        self.pos = 0;
        self.done = false;
        self.outer = None;
        self.matched = false;
        self.cands.clear();
        self.cand = 0;
        self.pending = None;
        self.given = 0;
        self.limits = None;
        self.kid = 0;
        self.converted.clear();
        if keeps {
            return;
        }
        if !matches!(self.node.kind, Kind::Memoize(_)) {
            self.buf.clear();
            self.filled = false;
            self.seen.clear();
        }
        match self.node.kind {
            // `ExecReScanHashJoin` keeps the hash table when the inner side does not change, and scans the outer side again.
            Kind::HashJoin { .. }
                if self.probe.is_some() && !depends(cx, &self.node.children[1]) =>
            {
                if let Some(probe) = &mut self.probe {
                    probe.matched.iter_mut().for_each(|m| *m = false);
                }
                self.kids[0].rescan(cx);
            }
            // `ExecReScanMemoize` keeps the cache, and the node below runs again for the next miss.
            Kind::Memoize(_) => {}
            _ => {
                self.probe = None;
                for kid in &mut self.kids {
                    kid.rescan(cx);
                }
            }
        }
    }

    /// `ExecutorEnd`: the end of the last loop of each node, and its counts.
    fn finish(mut self, actuals: &mut Actuals) {
        self.end_loop();
        actuals.insert(node_key(self.node), self.actual);
        for exec in self.kids.into_iter().chain(self.inits).chain(self.subs) {
            exec.finish(actuals);
        }
    }

    /// The `InitPlan` nodes of the node, which run before its first row, and the hashed `SubPlan` nodes, which run once for their hash table. A `CTE` subplan runs at the end, for the rows that its scans read.
    fn run_subplans(&mut self, cx: &Cx<'_, '_, '_>) -> Result<()> {
        for (sub, exec) in self.node.init_plans.iter().zip(&mut self.inits) {
            if sub.name.starts_with("CTE ") {
                continue;
            }
            let kind = cx.plan.sublinks.iter().find(|l| l.name == sub.name).map(|l| l.link.kind);
            let most = match kind {
                Some(SubLinkKind::Exists) => 1,
                Some(SubLinkKind::Expr | SubLinkKind::RowCompare) => 2,
                _ => usize::MAX,
            };
            let mut count = 0;
            while count < most && exec.next(cx, &[])?.is_some() {
                count += 1;
            }
        }
        for (sub, exec) in self.node.sub_plans.iter().zip(&mut self.subs) {
            let hashed = cx.plan.sublinks.iter().any(|l| l.name == sub.name && l.hashed);
            if !hashed {
                return Err(not_yet("EXPLAIN ANALYZE of a SubPlan that runs for each row"));
            }
            while exec.next(cx, &[])?.is_some() {}
        }
        Ok(())
    }

    /// The `CTE` subplans of the node and of the nodes below it: each one gives the rows that the scans of its query read, as the tuplestore of `CteScan` pulls them.
    fn ctes(&mut self, cx: &Cx<'_, '_, '_>) -> Result<()> {
        for (sub, exec) in self.node.init_plans.iter().zip(&mut self.inits) {
            let Some(name) = sub.name.strip_prefix("CTE ") else { continue };
            let read = cx.ctes.borrow().get(name).copied();
            if let Some((count, past)) = read {
                let mut pulled = 0;
                while (past || pulled < count) && exec.next(cx, &[])?.is_some() {
                    pulled += 1;
                }
            }
            exec.ctes(cx)?;
        }
        for exec in self.kids.iter_mut().chain(&mut self.subs) {
            exec.ctes(cx)?;
        }
        Ok(())
    }

    /// A row of a node of another query, as a row of the relation of the query of this node that reads it: the subquery that the plan removed, or the set operation whose `SELECT`s an `Append` reads.
    fn convert(&mut self, cx: &Cx<'_, '_, '_>, row: Row, kid: Option<usize>) -> Result<Row> {
        let Some(kid) = kid else { return Ok(row) };
        let inner = self.node.children[kid].scope;
        let scope = self.node.scope;
        if cx.same(scope, inner) {
            return Ok(row);
        }
        let Some(relation) = cx.relation_of(cx.home(scope), inner) else {
            return Err(not_yet("EXPLAIN ANALYZE of this plan"));
        };
        if self.converted.len() <= relation {
            self.converted.resize(relation + 1, 0);
        }
        let mut tuple = cx.tuple(scope, &[]);
        tuple[relation] = self.converted[relation];
        self.converted[relation] += 1;
        Ok(Row::of(tuple))
    }

    /// The next row of a kid, as a row of the query of this node.
    fn pull(&mut self, cx: &Cx<'_, '_, '_>, kid: usize, context: &[usize]) -> Result<Option<Row>> {
        let same = cx.same(self.node.scope, self.node.children[kid].scope);
        let context = if same { context } else { &[] };
        match self.kids[kid].next(cx, context)? {
            Some(row) => Ok(Some(self.convert(cx, row, Some(kid))?)),
            None => Ok(None),
        }
    }

    /// The work of a node for its next row.
    fn step(&mut self, cx: &Cx<'_, '_, '_>, context: &[usize]) -> Result<Option<Row>> {
        let node = self.node;
        match &node.kind {
            Kind::SeqScan(r)
            | Kind::SampleScan(r)
            | Kind::ValuesScan(r)
            | Kind::FunctionScan(r)
            | Kind::TableFunctionScan(r)
            | Kind::CteScan(r) => self.scan(cx, context, *r, None),
            Kind::IndexScan { relation, cond, columns, backward, only, .. } => {
                if !self.filled {
                    self.actual.searches += 1;
                    let order = Some((columns.as_slice(), *backward));
                    self.candidates(cx, context, *relation, cond, order)?;
                }
                let row = self.scan(cx, context, *relation, None)?;
                if row.is_some() && *only {
                    // A table with no visibility map reads each row of an index-only scan.
                    self.actual.filtered[1] += 0;
                }
                Ok(row)
            }
            Kind::TidScan { relation, cond, .. } => {
                if !self.filled {
                    self.candidates(cx, context, *relation, cond, None)?;
                }
                self.scan(cx, context, *relation, None)
            }
            Kind::BitmapHeapScan { relation, .. } => {
                if !self.filled {
                    let tuple = cx.tuple(node.scope, context);
                    let kid = &mut self.kids[0];
                    let rows = bitmap(kid, cx, &tuple, *relation)?;
                    let tables = cx.tables(node.scope)?;
                    let pages: HashSet<i64> =
                        rows.iter().filter_map(|&row| tables.page(*relation, row)).collect();
                    self.actual.exact_blocks += pages.len() as u64;
                    self.buf = rows
                        .into_iter()
                        .map(|row| {
                            let mut t = tuple.clone();
                            t[*relation] = row;
                            Row::of(t)
                        })
                        .collect();
                    self.filled = true;
                }
                self.scan(cx, context, *relation, None)
            }
            Kind::BitmapIndexScan { .. } | Kind::BitmapAnd | Kind::BitmapOr => {
                Err(Error::internal("a bitmap node out of a Bitmap Heap Scan"))
            }
            Kind::SubqueryScan(r) => loop {
                let Some(row) = self.kids[0].next(cx, &[])? else { return Ok(None) };
                let _ = row;
                let mut tuple = cx.tuple(node.scope, context);
                tuple[*r] = self.pos;
                self.pos += 1;
                if cx.test(node.scope, &node.filter, &tuple)? {
                    return Ok(Some(Row::of(tuple)));
                }
                self.actual.filtered[0] += 1;
            },
            Kind::WorkTableScan(_) | Kind::RecursiveUnion => {
                Err(not_yet("EXPLAIN ANALYZE of a recursive query"))
            }
            Kind::SetOp { .. } => Err(not_yet("EXPLAIN ANALYZE of INTERSECT and EXCEPT")),
            Kind::Result { one_time, .. } => {
                if self.done {
                    return Ok(None);
                }
                if !self.filled {
                    self.filled = true;
                    let tuple = cx.tuple(node.scope, context);
                    if !cx.test(node.scope, one_time, &tuple)? {
                        self.done = true;
                        return Ok(None);
                    }
                }
                loop {
                    let row = if self.kids.is_empty() {
                        self.done = true;
                        Row::of(cx.tuple(node.scope, context))
                    } else {
                        match self.pull(cx, 0, context)? {
                            Some(row) => row,
                            None => return Ok(None),
                        }
                    };
                    if row.values.is_some() || cx.test(node.scope, &node.filter, &row.tuple)? {
                        return Ok(Some(row));
                    }
                    self.actual.filtered[0] += 1;
                    if self.done {
                        return Ok(None);
                    }
                }
            }
            Kind::ProjectSet => loop {
                if self.pos < self.buf.len() {
                    self.pos += 1;
                    return Ok(Some(self.buf[self.pos - 1].clone()));
                }
                let Some(row) = self.pull(cx, 0, context)? else { return Ok(None) };
                if row.values.is_some() {
                    return Err(not_yet(
                        "EXPLAIN ANALYZE of a set-returning function with GROUP BY",
                    ));
                }
                let query = cx.query(node.scope);
                let tables = cx.tables(node.scope)?;
                let eval = Eval::new(cx.params, cx.session, &tables, cx.cache, &[]);
                let mut eval = eval.at(&row.tuple);
                let levels = set_levels(&query.targets);
                let mut out = Vec::new();
                eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
                self.buf = out
                    .into_iter()
                    .map(|values| Row { tuple: row.tuple.clone(), values: Some(Rc::new(values)) })
                    .collect();
                self.pos = 0;
            },
            Kind::NestedLoop { join, join_filter } => {
                self.nested_loop(cx, context, *join, join_filter)
            }
            Kind::HashJoin { join, cond, join_filter } => {
                self.hash_join(cx, context, *join, cond, join_filter)
            }
            Kind::MergeJoin { join, cond, join_filter } => {
                self.merge_join(cx, context, *join, cond, join_filter)
            }
            Kind::Hash => Err(Error::internal("a Hash node out of a Hash Join")),
            Kind::Materialize => {
                if self.pos < self.buf.len() {
                    self.pos += 1;
                    return Ok(Some(self.buf[self.pos - 1].clone()));
                }
                if self.filled {
                    return Ok(None);
                }
                match self.pull(cx, 0, context)? {
                    Some(row) => {
                        self.buf.push(row.clone());
                        self.pos += 1;
                        self.store();
                        Ok(Some(row))
                    }
                    None => {
                        self.filled = true;
                        self.store();
                        Ok(None)
                    }
                }
            }
            Kind::Memoize(memo) => {
                if self.memo_key.is_none() {
                    let tuple = cx.tuple(node.scope, context);
                    let mut key = String::new();
                    for item in &memo.keys {
                        key.push_str(&format!("{:?}|", cx.value(node.scope, &item.expr, &tuple)?));
                    }
                    let hit = self.memo.get(&key).is_some_and(|(_, complete)| *complete);
                    let counts = self.actual.memo.get_or_insert([0; 5]);
                    if hit {
                        counts[0] += 1;
                    } else {
                        counts[1] += 1;
                        self.memo.insert(key.clone(), (Vec::new(), false));
                        self.kids[0].rescan(cx);
                    }
                    self.memo_key = Some(key);
                    self.pos = 0;
                }
                let key = self.memo_key.clone().unwrap_or_default();
                let (rows, complete) = self.memo.get(&key).cloned().unwrap_or_default();
                if self.pos < rows.len() {
                    self.pos += 1;
                    return Ok(Some(rows[self.pos - 1].clone()));
                }
                if complete {
                    return Ok(None);
                }
                let row = self.pull(cx, 0, context)?;
                let entry = self.memo.entry(key).or_default();
                match &row {
                    Some(row) => entry.0.push(row.clone()),
                    None => entry.1 = true,
                }
                if row.is_some() {
                    self.pos += 1;
                }
                let entries = self.memo.len() as u64;
                let stored: u64 = self.memo.values().map(|(r, _)| r.len() as u64).sum();
                let width = width(node);
                if let Some(counts) = &mut self.actual.memo {
                    counts[4] = kilobytes(entries * 96 + stored * (width + 40));
                }
                Ok(row)
            }
            Kind::Aggregate { strategy, keys, unique, .. } => {
                self.aggregate(cx, context, *strategy, keys, *unique)
            }
            Kind::Sort(_) | Kind::IncrementalSort { .. } => {
                if !self.filled {
                    while let Some(row) = self.pull(cx, 0, context)? {
                        self.buf.push(row);
                    }
                    self.filled = true;
                    let width = width(node);
                    // `tuplesort_set_bound`: a bounded sort keeps a heap of the first rows when it reads more than twice the bound.
                    let kept = match self.bound {
                        Some(bound) if self.buf.len() > bound.saturating_mul(2) => {
                            self.actual.heapsort = true;
                            bound
                        }
                        _ => self.buf.len(),
                    };
                    self.actual.sorted =
                        Some(kilobytes(24576 + kept as u64 * (width + 24)).max(25));
                }
                if self.pos < self.buf.len() {
                    self.pos += 1;
                    return Ok(Some(self.buf[self.pos - 1].clone()));
                }
                Ok(None)
            }
            Kind::Limit => {
                let (offset, count) = match self.limits {
                    Some(limits) => limits,
                    None => {
                        let query = cx.query(node.scope);
                        if query.with_ties {
                            return Err(not_yet("EXPLAIN ANALYZE of FETCH WITH TIES"));
                        }
                        let tables = cx.tables(node.scope)?;
                        let mut eval = Eval::new(cx.params, cx.session, &tables, cx.cache, &[]);
                        let limits = eval.limits(query)?;
                        self.limits = Some(limits);
                        // `ExecSetTupleBound`: the `Sort` below needs only the rows of the offset and the count.
                        if let (Some(count), Some(kid)) = (limits.1, self.kids.first_mut()) {
                            kid.set_bound(limits.0.saturating_add(count));
                        }
                        limits
                    }
                };
                // `ExecLimit` reads no row of the node below with `LIMIT 0`, and no row after the last row that it gives.
                if count.is_some_and(|c| self.given >= c) {
                    return Ok(None);
                }
                while self.pos < offset {
                    if self.pull(cx, 0, context)?.is_none() {
                        return Ok(None);
                    }
                    self.pos += 1;
                }
                let row = self.pull(cx, 0, context)?;
                if row.is_some() {
                    self.given += 1;
                }
                Ok(row)
            }
            Kind::Unique => loop {
                let Some(row) = self.pull(cx, 0, context)? else { return Ok(None) };
                let key = self.unique_key(cx, &row, None)?;
                if self.seen.insert(key) {
                    return Ok(Some(row));
                }
            },
            Kind::Append | Kind::MergeAppend(_) => {
                while self.kid < self.kids.len() {
                    let kid = self.kid;
                    if let Some(row) = self.pull(cx, kid, context)? {
                        return Ok(Some(row));
                    }
                    self.kid += 1;
                }
                Ok(None)
            }
            Kind::WindowAgg { run, .. } => {
                if !run.is_empty() || !node.filter.is_empty() {
                    return Err(not_yet("EXPLAIN ANALYZE of a run condition of a window"));
                }
                if !self.filled {
                    while let Some(row) = self.pull(cx, 0, context)? {
                        self.buf.push(row);
                    }
                    self.filled = true;
                    self.store();
                }
                if self.pos < self.buf.len() {
                    self.pos += 1;
                    return Ok(Some(self.buf[self.pos - 1].clone()));
                }
                Ok(None)
            }
            Kind::LockRows => self.pull(cx, 0, context),
            Kind::Modify { operation, conflict, .. } => {
                if conflict.is_some() {
                    return Err(not_yet("EXPLAIN ANALYZE of ON CONFLICT"));
                }
                let returning = !matches!(operation, rupg_plan::Operation::Merge)
                    && cx.query(node.scope).write.as_ref().is_some_and(|w| w.returning.is_some());
                loop {
                    let Some(row) = self.pull(cx, 0, context)? else { return Ok(None) };
                    // The rows of the plan below, for the rows of a `MERGE` with no action.
                    self.actual.filtered[0] += 1;
                    if returning {
                        return Ok(Some(row));
                    }
                }
            }
        }
    }

    /// `ExecSetTupleBound`: a `Sort` keeps only the first `bound` rows. A `SubqueryScan` with no filter gives the bound to the node below it.
    fn set_bound(&mut self, bound: usize) {
        match self.node.kind {
            Kind::Sort(_) => self.bound = Some(bound),
            Kind::SubqueryScan(_) if self.node.filter.is_empty() => {
                if let Some(kid) = self.kids.first_mut() {
                    kid.set_bound(bound);
                }
            }
            _ => {}
        }
    }

    /// The `Storage` of the tuplestore of a node that keeps its rows.
    fn store(&mut self) {
        // `tuplestore_begin_common` makes an array of 2048 pointers and `grow_memtuples` doubles it. Each row is a `MinimalTuple` with a header of 16 bytes and the header of its chunk.
        let rows = self.buf.len() as u64;
        let mut slots = 2048;
        while slots < rows {
            slots *= 2;
        }
        let tuple = (16 + width(self.node)).next_multiple_of(8) + 8;
        self.actual.storage = Some(kilobytes(slots * 8 + 16 + rows * tuple));
    }

    /// The rows of a scan, in `buf` and after `pos`, with the rows of its condition and its filter. A scan with no rows in `buf` reads the rows of its relation.
    fn scan(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        relation: usize,
        _cond: Option<()>,
    ) -> Result<Option<Row>> {
        let node = self.node;
        if !self.filled {
            let tables = cx.tables(node.scope)?;
            let tuple = cx.tuple(node.scope, context);
            let count = tables.rows[relation].len();
            let member = node.member;
            self.buf = (0..count)
                .filter(|&row| member.is_none_or(|m| tables.leaf(relation, row) == m))
                .map(|row| {
                    let mut t = tuple.clone();
                    t[relation] = row;
                    Row::of(t)
                })
                .collect();
            self.filled = true;
        }
        let cte = match &node.kind {
            Kind::CteScan(r) => {
                cx.query(node.scope).relations[*r].cte.as_ref().map(|c| c.name.clone())
            }
            _ => None,
        };
        if cte.is_some() {
            self.store();
        }
        while self.pos < self.buf.len() {
            let row = self.buf[self.pos].clone();
            self.pos += 1;
            if let Some(name) = &cte {
                let mut ctes = cx.ctes.borrow_mut();
                let entry = ctes.entry(name.clone()).or_default();
                entry.0 = entry.0.max(self.pos);
            }
            if cx.test(node.scope, &node.filter, &row.tuple)? {
                return Ok(Some(row));
            }
            self.actual.filtered[0] += 1;
        }
        if let Some(name) = &cte {
            cx.ctes.borrow_mut().entry(name.clone()).or_default().1 = true;
        }
        Ok(None)
    }

    /// The rows of the relation of an index scan or of a TID scan that pass its condition, in the order of the columns of the index.
    fn candidates(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        relation: usize,
        cond: &[Item<'_>],
        order: Option<(&[i16], bool)>,
    ) -> Result<()> {
        let node = self.node;
        let tables = cx.tables(node.scope)?;
        let tuple = cx.tuple(node.scope, context);
        let mut rows = Vec::new();
        for row in 0..tables.rows[relation].len() {
            if node.member.is_some_and(|m| tables.leaf(relation, row) != m) {
                continue;
            }
            let mut t = tuple.clone();
            t[relation] = row;
            if cx.test(node.scope, cond, &t)? {
                rows.push(Row::of(t));
            }
        }
        if let Some((columns, backward)) = order.filter(|(c, _)| !c.is_empty()) {
            let key = |row: &Row| -> Option<Vec<scan::Key>> {
                let values = &tables.rows[relation][row.tuple[relation]];
                columns
                    .iter()
                    .map(|&c| {
                        let at = usize::try_from(c).ok()?.checked_sub(1)?;
                        scan::key(values.get(tables.place(relation, at)?)?.clone())
                    })
                    .collect()
            };
            if rows.iter().all(|r| key(r).is_some()) {
                rows.sort_by_key(|r| key(r));
            }
            if backward {
                rows.reverse();
            }
        }
        self.buf = rows;
        self.filled = true;
        Ok(())
    }

    /// The key of a row for `Unique` or for a hashed `Aggregate` with no aggregate: the values of the keys of the node, or of `DISTINCT`.
    fn unique_key(
        &self,
        cx: &Cx<'_, '_, '_>,
        row: &Row,
        keys: Option<&[Item<'_>]>,
    ) -> Result<String> {
        let node = self.node;
        let query = cx.query(node.scope);
        if let Some(values) = &row.values {
            let picked: Vec<&Value> = if query.distinct.is_empty() {
                values.iter().collect()
            } else {
                query.distinct.iter().filter_map(|d| values.get(d.target)).collect()
            };
            return Ok(format!("{picked:?}"));
        }
        let mut key = String::new();
        let mut add = |expr: &rupg_analyze::Expr| -> Result<()> {
            key.push_str(&format!("{:?}|", cx.value(node.scope, expr, &row.tuple)?));
            Ok(())
        };
        match keys {
            Some(keys) if !keys.is_empty() => keys.iter().try_for_each(|k| add(&k.expr))?,
            _ if query.distinct_on || node.output.is_empty() => {
                for d in &query.distinct {
                    add(&query.targets[d.target].expr)?;
                }
            }
            _ => node.output.iter().try_for_each(|k| add(&k.expr))?,
        }
        Ok(key)
    }

    /// `ExecAgg`: the groups of a query with `GROUP BY` or aggregates, as the executor makes them, or the rows with a new key for an `Aggregate` that makes the rows of the node below unique.
    fn aggregate(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        strategy: Strategy,
        keys: &[Item<'_>],
        unique: bool,
    ) -> Result<Option<Row>> {
        let node = self.node;
        let query = cx.query(node.scope);
        let hashed = matches!(strategy, Strategy::Hashed | Strategy::Mixed);
        if unique || !query.grouped {
            // A hashed node reads all the rows below before it gives a row.
            if hashed && !self.filled {
                let mut kept = Vec::new();
                while let Some(row) = self.pull(cx, 0, context)? {
                    let key = self.unique_key(cx, &row, Some(keys))?;
                    if self.seen.insert(key) {
                        kept.push(row);
                    }
                }
                self.buf = kept;
                self.filled = true;
                let width = width(node);
                self.actual.hashagg = Some(hashagg_kilobytes(self.buf.len(), width));
            }
            if hashed {
                if self.pos < self.buf.len() {
                    self.pos += 1;
                    return Ok(Some(self.buf[self.pos - 1].clone()));
                }
                return Ok(None);
            }
            loop {
                let Some(row) = self.pull(cx, 0, context)? else { return Ok(None) };
                let key = self.unique_key(cx, &row, Some(keys))?;
                if self.seen.insert(key) {
                    return Ok(Some(row));
                }
            }
        }
        if !self.filled {
            let mut tuples = Vec::new();
            while let Some(row) = self.pull(cx, 0, context)? {
                if row.values.is_some() {
                    return Err(not_yet("EXPLAIN ANALYZE of this aggregate"));
                }
                tuples.push(row.tuple);
            }
            if query.has_target_srfs() {
                return Err(not_yet("EXPLAIN ANALYZE of a set-returning function with GROUP BY"));
            }
            let tables = cx.tables(node.scope)?;
            let mut eval = Eval::new(cx.params, cx.session, &tables, cx.cache, &[]);
            let none = vec![scan::NONE; query.relations.len()];
            let groups = agg::group_count(&mut eval, query, &tuples)?;
            let rows = agg::rows(&mut eval, query, &tuples, &none)?;
            self.actual.filtered[0] += (groups.saturating_sub(rows.len())) as u64;
            let width = width(node);
            if hashed {
                self.actual.hashagg = Some(hashagg_kilobytes(groups, width));
            }
            self.buf = rows
                .into_iter()
                .map(|values| Row { tuple: none.clone(), values: Some(Rc::new(values)) })
                .collect();
            self.filled = true;
        }
        if self.pos < self.buf.len() {
            self.pos += 1;
            return Ok(Some(self.buf[self.pos - 1].clone()));
        }
        Ok(None)
    }

    /// `ExecNestLoop`: for each outer row, a new scan of the inner side with the outer row as its parameters.
    fn nested_loop(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        join: JoinKind,
        join_filter: &[Item<'_>],
    ) -> Result<Option<Row>> {
        let node = self.node;
        let single = matches!(join, JoinKind::Semi) || node.inner_unique;
        let inner_rels = filled(cx, &node.children[1], node.scope);
        loop {
            let Some(outer) = self.outer.clone() else {
                let Some(outer) = self.pull(cx, 0, context)? else { return Ok(None) };
                self.kids[1].rescan(cx);
                self.outer = Some(outer);
                self.matched = false;
                continue;
            };
            let params = merge(&outer.tuple, context);
            match self.pull(cx, 1, &params)? {
                Some(inner) => {
                    let joined = merge(&outer.tuple, &inner.tuple);
                    if !cx.test(node.scope, join_filter, &joined)? {
                        self.actual.filtered[0] += 1;
                        continue;
                    }
                    self.matched = true;
                    if matches!(join, JoinKind::Anti) {
                        self.outer = None;
                        continue;
                    }
                    if single {
                        self.outer = None;
                    }
                    let joined =
                        if matches!(join, JoinKind::Semi) { outer.tuple.clone() } else { joined };
                    if cx.test(node.scope, &node.filter, &joined)? {
                        return Ok(Some(Row::of(joined)));
                    }
                    self.actual.filtered[1] += 1;
                }
                None => {
                    self.outer = None;
                    if !self.matched && matches!(join, JoinKind::Left | JoinKind::Anti) {
                        let tuple = without(&outer.tuple, &inner_rels);
                        if cx.test(node.scope, &node.filter, &tuple)? {
                            return Ok(Some(Row::of(tuple)));
                        }
                        self.actual.filtered[1] += 1;
                    }
                }
            }
        }
    }

    /// The hash table of the inner side of a hash join, from the rows of the node below its `Hash` node.
    fn build(&mut self, cx: &Cx<'_, '_, '_>, cond: &[Item<'_>], fill_inner: bool) -> Result<()> {
        let node = self.node;
        let scope = node.scope;
        let inner_rels = filled(cx, &node.children[1], scope);
        let pairs = key_pairs(cond, &inner_rels);
        let hash = &mut self.kids[1];
        let started = cx.session.clock();
        hash.running = true;
        let mut probe = Probe { pairs, ..Probe::default() };
        let same = cx.same(scope, hash.node.children[0].scope);
        let mut converted = 0;
        while let Some(row) = hash.kids[0].next(cx, &[])? {
            let row = if same {
                row
            } else {
                let inner = hash.node.children[0].scope;
                let Some(relation) = cx.relation_of(cx.home(scope), inner) else {
                    return Err(not_yet("EXPLAIN ANALYZE of this plan"));
                };
                let mut tuple = cx.tuple(scope, &[]);
                tuple[relation] = converted;
                converted += 1;
                Row::of(tuple)
            };
            let at = probe.rows.len();
            let mut key = Some(Vec::new());
            let mut null = false;
            if let Some(pairs) = &probe.pairs {
                for (_, inner) in pairs {
                    let value = cx.value(scope, inner, &row.tuple)?;
                    if value.is_null() {
                        null = true;
                    }
                    key = key.and_then(|mut k: Vec<scan::Key>| {
                        k.push(scan::key(value)?);
                        Some(k)
                    });
                }
            } else {
                key = None;
            }
            if null && !fill_inner {
                continue;
            }
            probe.rows.push(row);
            match key.filter(|_| !null) {
                Some(key) => probe.keys.entry(key).or_default().push(at),
                None => probe.loose.push(at),
            }
        }
        probe.matched = vec![false; probe.rows.len()];
        let count = probe.rows.len() as u64;
        hash.actual.rows += count;
        let estimate = hash.node.cost.rows.max(1.0);
        let buckets = (estimate as u64).next_power_of_two().max(1024);
        let width = width(hash.node);
        hash.actual.hash = Some((buckets, 1, kilobytes(buckets * 8 + count * (width + 32))));
        hash.spent += elapsed(cx, started);
        if hash.first.is_none() {
            hash.first = Some(hash.spent);
        }
        self.probe = Some(probe);
        Ok(())
    }

    /// The rows of the inner side that can match an outer row: the rows with the keys of the outer row, and the rows whose keys have no hash.
    fn probe_rows(&self, cx: &Cx<'_, '_, '_>, outer: &Row) -> Result<Vec<usize>> {
        let probe = self.probe.as_ref().ok_or_else(|| Error::internal("no hash table"))?;
        let Some(pairs) = &probe.pairs else { return Ok((0..probe.rows.len()).collect()) };
        let mut key = Vec::with_capacity(pairs.len());
        for (o, _) in pairs {
            let value = cx.value(self.node.scope, o, &outer.tuple)?;
            if value.is_null() {
                return Ok(Vec::new());
            }
            match scan::key(value) {
                Some(k) => key.push(k),
                None => return Ok((0..probe.rows.len()).collect()),
            }
        }
        let mut rows = probe.keys.get(&key).cloned().unwrap_or_default();
        rows.extend(&probe.loose);
        rows.sort_unstable();
        Ok(rows)
    }

    /// `ExecHashJoin`: the hash table of the inner side, then the outer rows that probe it.
    fn hash_join(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        join: JoinKind,
        cond: &[Item<'_>],
        join_filter: &[Item<'_>],
    ) -> Result<Option<Row>> {
        let node = self.node;
        let fill_inner = matches!(
            join,
            JoinKind::Right | JoinKind::Full | JoinKind::RightAnti | JoinKind::RightSemi
        );
        let fill_outer = matches!(join, JoinKind::Left | JoinKind::Full | JoinKind::Anti);
        let single = matches!(join, JoinKind::Semi) || node.inner_unique;
        if self.done {
            return Ok(None);
        }
        if self.probe.is_none() || (!self.filled && self.pos == 0) {
            if self.probe.is_none() {
                // `ExecHashJoinImpl` reads the first outer row before it builds the table, when the outer side is cheap to start.
                let outer_first = !fill_inner
                    && (fill_outer || node.children[0].cost.startup < node.children[1].cost.total);
                if outer_first {
                    let Some(row) = self.pull(cx, 0, context)? else {
                        self.done = true;
                        return Ok(None);
                    };
                    self.pending = Some(row);
                }
                self.build(cx, cond, fill_inner)?;
            }
            self.pos = 1;
            let empty = self.probe.as_ref().is_some_and(|p| p.rows.is_empty());
            if empty && !fill_outer {
                self.done = true;
                return Ok(None);
            }
        }
        let inner_rels = filled(cx, &node.children[1], node.scope);
        loop {
            if self.filled {
                // The inner rows that matched no outer row, for a join that fills the inner side.
                let probe = self.probe.as_ref().ok_or_else(|| Error::internal("no hash table"))?;
                while self.cand < probe.rows.len() {
                    let at = self.cand;
                    self.cand += 1;
                    if probe.matched[at] {
                        continue;
                    }
                    let tuple = probe.rows[at].tuple.clone();
                    if cx.test(node.scope, &node.filter, &tuple)? {
                        return Ok(Some(Row::of(tuple)));
                    }
                    self.actual.filtered[1] += 1;
                }
                self.done = true;
                return Ok(None);
            }
            let Some(outer) = self.outer.clone() else {
                let row = match self.pending.take() {
                    Some(row) => Some(row),
                    None => self.pull(cx, 0, context)?,
                };
                let Some(row) = row else {
                    if fill_inner && !matches!(join, JoinKind::RightSemi) {
                        self.filled = true;
                        self.cand = 0;
                        continue;
                    }
                    self.done = true;
                    return Ok(None);
                };
                self.cands = self.probe_rows(cx, &row)?;
                self.cand = 0;
                self.matched = false;
                self.outer = Some(row);
                continue;
            };
            if self.cand < self.cands.len() {
                let at = self.cands[self.cand];
                self.cand += 1;
                let probe = self.probe.as_ref().ok_or_else(|| Error::internal("no hash table"))?;
                let inner = &probe.rows[at];
                let joined = merge(&outer.tuple, &inner.tuple);
                if !cx.test(node.scope, cond, &joined)? {
                    continue;
                }
                if matches!(join, JoinKind::RightSemi) && probe.matched[at] {
                    continue;
                }
                if !cx.test(node.scope, join_filter, &joined)? {
                    self.actual.filtered[0] += 1;
                    continue;
                }
                self.matched = true;
                if let Some(probe) = &mut self.probe {
                    probe.matched[at] = true;
                }
                match join {
                    JoinKind::Anti => {
                        self.outer = None;
                        continue;
                    }
                    JoinKind::RightAnti => continue,
                    _ => {}
                }
                if single {
                    self.outer = None;
                }
                let joined = match join {
                    JoinKind::Semi => outer.tuple.clone(),
                    JoinKind::RightSemi => {
                        without(&joined, &filled(cx, &node.children[0], node.scope))
                    }
                    _ => joined,
                };
                if cx.test(node.scope, &node.filter, &joined)? {
                    return Ok(Some(Row::of(joined)));
                }
                self.actual.filtered[1] += 1;
                continue;
            }
            self.outer = None;
            if !self.matched && fill_outer {
                let tuple = without(&outer.tuple, &inner_rels);
                if cx.test(node.scope, &node.filter, &tuple)? {
                    return Ok(Some(Row::of(tuple)));
                }
                self.actual.filtered[1] += 1;
            }
        }
    }

    /// `ExecMergeJoin`, with the rows of the two sides read in full and the outer rows joined as a hash join joins them.
    fn merge_join(
        &mut self,
        cx: &Cx<'_, '_, '_>,
        context: &[usize],
        join: JoinKind,
        cond: &[Item<'_>],
        join_filter: &[Item<'_>],
    ) -> Result<Option<Row>> {
        if matches!(join, JoinKind::RightSemi | JoinKind::RightAnti) {
            return Err(not_yet("EXPLAIN ANALYZE of this merge join"));
        }
        let fill_inner = matches!(join, JoinKind::Right | JoinKind::Full);
        if self.done {
            return Ok(None);
        }
        if self.probe.is_none() {
            // `MJ_INITIALIZE_OUTER`: the first outer row comes before the inner side. A join that does not fill the inner side ends when the outer side has no rows, and its inner side does not run.
            let first = self.pull(cx, 0, context)?;
            if first.is_none() && !fill_inner {
                self.done = true;
                return Ok(None);
            }
            let empty = first.is_none();
            self.pending = first;
            let mut rows = Vec::new();
            while let Some(row) = self.pull(cx, 1, context)? {
                rows.push(row);
            }
            let count = rows.len();
            self.probe = Some(Probe {
                rows,
                loose: (0..count).collect(),
                matched: vec![false; count],
                ..Probe::default()
            });
            self.pos = 1;
            if empty {
                self.filled = true;
                self.cand = 0;
            }
        }
        let node = self.node;
        let inner_rels = filled(cx, &node.children[1], node.scope);
        let single = matches!(join, JoinKind::Semi) || node.inner_unique;
        loop {
            if self.filled {
                // `MJ_ENDOUTER`: the inner rows that matched no outer row, for a right or a full join.
                let probe = self.probe.as_ref().ok_or_else(|| Error::internal("no rows"))?;
                while self.cand < probe.rows.len() {
                    let at = self.cand;
                    self.cand += 1;
                    if probe.matched[at] {
                        continue;
                    }
                    let tuple = probe.rows[at].tuple.clone();
                    if cx.test(node.scope, &node.filter, &tuple)? {
                        return Ok(Some(Row::of(tuple)));
                    }
                    self.actual.filtered[1] += 1;
                }
                self.done = true;
                return Ok(None);
            }
            let Some(outer) = self.outer.clone() else {
                let row = match self.pending.take() {
                    Some(row) => Some(row),
                    None => self.pull(cx, 0, context)?,
                };
                let Some(row) = row else {
                    if fill_inner {
                        self.filled = true;
                        self.cand = 0;
                        continue;
                    }
                    self.done = true;
                    return Ok(None);
                };
                self.cand = 0;
                self.matched = false;
                self.outer = Some(row);
                continue;
            };
            let count = self.probe.as_ref().map_or(0, |p| p.rows.len());
            if self.cand < count {
                let at = self.cand;
                self.cand += 1;
                let probe = self.probe.as_ref().ok_or_else(|| Error::internal("no rows"))?;
                let joined = merge(&outer.tuple, &probe.rows[at].tuple);
                if !cx.test(node.scope, cond, &joined)? {
                    continue;
                }
                if !cx.test(node.scope, join_filter, &joined)? {
                    self.actual.filtered[0] += 1;
                    continue;
                }
                self.matched = true;
                if let Some(probe) = &mut self.probe {
                    probe.matched[at] = true;
                }
                if matches!(join, JoinKind::Anti) {
                    self.outer = None;
                    continue;
                }
                if single {
                    self.outer = None;
                }
                let joined =
                    if matches!(join, JoinKind::Semi) { outer.tuple.clone() } else { joined };
                if cx.test(node.scope, &node.filter, &joined)? {
                    return Ok(Some(Row::of(joined)));
                }
                self.actual.filtered[1] += 1;
                continue;
            }
            self.outer = None;
            if !self.matched && matches!(join, JoinKind::Left | JoinKind::Full | JoinKind::Anti) {
                let tuple = without(&outer.tuple, &inner_rels);
                if cx.test(node.scope, &node.filter, &tuple)? {
                    return Ok(Some(Row::of(tuple)));
                }
                self.actual.filtered[1] += 1;
            }
        }
    }
}

/// `MultiExecBitmapIndexScan`, `MultiExecBitmapAnd` and `MultiExecBitmapOr`: the rows of the relation that the bitmap of a node has. A `Bitmap Index Scan` counts the rows of its bitmap, and `BitmapAnd` and `BitmapOr` count no rows.
fn bitmap(
    exec: &mut Exec<'_, '_>,
    cx: &Cx<'_, '_, '_>,
    tuple: &[usize],
    relation: usize,
) -> Result<Vec<usize>> {
    let started = cx.session.clock();
    exec.running = true;
    let node = exec.node;
    let rows = match &node.kind {
        Kind::BitmapIndexScan { cond, .. } => {
            exec.actual.searches += 1;
            let tables = cx.tables(node.scope)?;
            let mut rows = Vec::new();
            for row in 0..tables.rows[relation].len() {
                if node.member.is_some_and(|m| tables.leaf(relation, row) != m) {
                    continue;
                }
                let mut t = tuple.to_vec();
                t[relation] = row;
                if cx.test(node.scope, cond, &t)? {
                    rows.push(row);
                }
            }
            exec.actual.rows += rows.len() as u64;
            rows
        }
        Kind::BitmapAnd | Kind::BitmapOr => {
            let and = matches!(node.kind, Kind::BitmapAnd);
            let mut out: Option<Vec<usize>> = None;
            for kid in &mut exec.kids {
                let rows = bitmap(kid, cx, tuple, relation)?;
                out = Some(match out {
                    None => rows,
                    Some(prev) if and => prev.into_iter().filter(|r| rows.contains(r)).collect(),
                    Some(mut prev) => {
                        prev.extend(rows);
                        prev.sort_unstable();
                        prev.dedup();
                        prev
                    }
                });
            }
            out.unwrap_or_default()
        }
        _ => return Err(Error::internal("a node that is not a bitmap below a Bitmap Heap Scan")),
    };
    exec.spent += elapsed(cx, started);
    if exec.first.is_none() {
        exec.first = Some(exec.spent);
    }
    Ok(rows)
}

/// The relations of the query of `scope` whose rows a node and the nodes below it give.
fn filled(cx: &Cx<'_, '_, '_>, node: &Node<'_>, scope: usize) -> Vec<usize> {
    let mut out = Vec::new();
    fn walk(cx: &Cx<'_, '_, '_>, node: &Node<'_>, scope: usize, out: &mut Vec<usize>) {
        if !cx.same(scope, node.scope) {
            if let Some(r) = cx.relation_of(cx.home(scope), node.scope) {
                out.push(r);
            }
            return;
        }
        match &node.kind {
            Kind::SeqScan(r)
            | Kind::SampleScan(r)
            | Kind::FunctionScan(r)
            | Kind::TableFunctionScan(r)
            | Kind::ValuesScan(r)
            | Kind::SubqueryScan(r)
            | Kind::CteScan(r)
            | Kind::WorkTableScan(r) => {
                out.push(*r);
                return;
            }
            Kind::IndexScan { relation, .. }
            | Kind::TidScan { relation, .. }
            | Kind::BitmapHeapScan { relation, .. } => {
                out.push(*relation);
                return;
            }
            _ => {}
        }
        for kid in &node.children {
            walk(cx, kid, scope, out);
        }
    }
    walk(cx, node, scope, &mut out);
    out
}

/// True when a node or a node below it reads a relation of its query that the nodes below it do not give, as a parameter of a nested loop.
fn depends(cx: &Cx<'_, '_, '_>, node: &Node<'_>) -> bool {
    let own = filled(cx, node, node.scope);
    let mut found = false;
    fn walk(cx: &Cx<'_, '_, '_>, node: &Node<'_>, scope: usize, own: &[usize], found: &mut bool) {
        if *found || !cx.same(scope, node.scope) {
            return;
        }
        let mut check = |items: &[Item<'_>]| {
            for item in items {
                let outside = item.expr.find(0, &mut |e, depth| match e.kind {
                    ExprKind::Var(var)
                        if var.levels_up == depth && !own.contains(&var.relation) =>
                    {
                        Some(())
                    }
                    _ => None,
                });
                if outside.is_some() {
                    *found = true;
                }
            }
        };
        check(&node.filter);
        match &node.kind {
            Kind::IndexScan { cond, .. }
            | Kind::TidScan { cond, .. }
            | Kind::BitmapIndexScan { cond, .. } => check(cond),
            Kind::NestedLoop { join_filter, .. } => check(join_filter),
            Kind::HashJoin { cond, join_filter, .. }
            | Kind::MergeJoin { cond, join_filter, .. } => {
                check(cond);
                check(join_filter);
            }
            Kind::Memoize(memo) => check(&memo.keys),
            _ => {}
        }
        for kid in &node.children {
            walk(cx, kid, scope, own, found);
        }
    }
    walk(cx, node, node.scope, &own, &mut found);
    found
}

/// The pairs of the expressions of the outer side and of the inner side of each `=` of the condition of a hash join, or `None` when a part of the condition is not such an `=`.
fn key_pairs(
    cond: &[Item<'_>],
    inner: &[usize],
) -> Option<Vec<(rupg_analyze::Expr, rupg_analyze::Expr)>> {
    let side = |e: &rupg_analyze::Expr| -> Option<bool> {
        let mut rels = Vec::new();
        e.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
            {
                rels.push(var.relation);
            }
            None::<()>
        });
        if rels.is_empty() {
            return None;
        }
        if rels.iter().all(|r| inner.contains(r)) {
            Some(true)
        } else if rels.iter().all(|r| !inner.contains(r)) {
            Some(false)
        } else {
            None
        }
    };
    let mut pairs = Vec::new();
    for item in cond {
        let ExprKind::Func(f) = &item.expr.kind else { return None };
        if !scan::HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
            return None;
        }
        let (a, b) = (&f.args[0], &f.args[1]);
        match (side(a)?, side(b)?) {
            (false, true) => pairs.push((a.clone(), b.clone())),
            (true, false) => pairs.push((b.clone(), a.clone())),
            _ => return None,
        }
    }
    Some(pairs)
}

/// The error of a plan whose counts the run cannot give.
#[allow(dead_code)]
fn unsupported() -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, "EXPLAIN ANALYZE of this plan is not supported yet")
}

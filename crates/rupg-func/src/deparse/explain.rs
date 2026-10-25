//! The texts of a plan for `EXPLAIN`: the names of its relations, as `select_rtable_names_for_explain` chooses them, and its expressions, as `deparse_expression` shows them with the context of `set_deparse_context_plan`.

use std::collections::HashMap;
use std::ptr;
use std::rc::Rc;

use rupg_analyze::{Expr, FromItem, SortGroup, SubLink, SubLinkKind};
use rupg_plan::{Kind, Node, Plan, TARGET, Texts};
use rupg_sql::nodes::FRAMEOPTION_NONDEFAULT;

use super::{Deparser, Error, Namespace, Removed, Result, SubName, builtin, quote_identifier, reg};
use crate::Session;

/// The output of `EXPLAIN` for a plan, with the names and the expressions that the session shows.
///
/// # Errors
///
/// The errors of the deparse of the expressions.
pub fn explain(
    plan: &Plan<'_>,
    options: &rupg_plan::Options,
    session: &dyn Session,
) -> Result<String> {
    let mut texts = PlanTexts::new(plan, session);
    rupg_plan::explain(plan, options, &mut texts)
}

/// The names of the relations of each query of a plan and the names of its subplans and of its windows.
struct PlanTexts<'a, 'q> {
    session: &'a dyn Session,
    plan: &'a Plan<'q>,
    /// The names of each scope of the plan.
    spaces: Vec<Namespace>,
    /// The name of the target table of each scope of an `INSERT`.
    targets: Vec<Option<String>>,
    /// The name of each window of each scope, as `WindowAgg.winname`.
    windows: Vec<Vec<Option<String>>>,
    subplans: Vec<SubName>,
}

impl<'a, 'q> PlanTexts<'a, 'q> {
    /// The names of the relations of the plan. The scopes in [`Plan::order`] take their names in that order, and each name is unique in all of them. The target table of an `INSERT` is the first relation of its query.
    fn new(plan: &'a Plan<'q>, session: &'a dyn Session) -> PlanTexts<'a, 'q> {
        let d = Deparser::new(session, false, 0);
        let count = plan.scopes.len();
        let mut spaces: Vec<Option<Namespace>> = vec![None; count];
        let mut targets = vec![None; count];
        let mut used: HashMap<String, u32> = HashMap::new();
        // `transformOnConflictClause`: the entry `excluded` of the range table comes after the target table of the `INSERT`.
        let mut conflicts = Vec::new();
        conflict_scopes(&plan.root, &mut conflicts);
        let mut excluded = vec![None; count];
        for &s in &plan.order {
            let Some(scope) = plan.scopes.get(s) else { continue };
            if spaces[s].is_some() {
                continue;
            }
            // `expand_inherited_rtentry`: a member has the alias of its relation, which `set_rtable_names` makes unique.
            if let Some(leaf) = scope.leaf
                && let Some(Some(of)) = spaces.get(leaf.of)
            {
                let mut space = of.clone();
                if leaf.renamed
                    && let Some(rel) = scope.query.relations.get(leaf.relation)
                {
                    space.names[leaf.relation] =
                        super::query::unique_name(d.base_name(rel), &mut used);
                }
                spaces[s] = Some(space);
                continue;
            }
            if let Some(write) = scope.target {
                let base = write.alias.clone().unwrap_or_else(|| {
                    reg::class_name(write.table, session)
                        .map_or_else(|| write.name.clone(), |(name, _)| name.to_string())
                });
                targets[s] = Some(super::query::unique_name(base, &mut used));
                if write.on_conflict.as_ref().is_some_and(|c| c.update.is_some()) {
                    excluded[s] = Some(super::query::unique_name("excluded".into(), &mut used));
                }
            }
            spaces[s] = Some(d.namespace_in(scope.query, &mut used, true));
            // `ApplyRetrieveRule`: the entry of a view with triggers `INSTEAD OF` that the statement writes comes after the entries of the query.
            if let Some(write) =
                scope.query.write.as_ref().filter(|w| w.instead && scope.target.is_none())
            {
                // The entry has the name of the subquery of the view: the alias of the statement, or the name of the view.
                let base = scope
                    .query
                    .relations
                    .first()
                    .map_or_else(|| write.name.clone(), |relation| relation.name.clone());
                targets[s] = Some(super::query::unique_name(base, &mut used));
            }
        }
        let mut spaces: Vec<Namespace> = spaces
            .into_iter()
            .enumerate()
            .map(|(s, space)| {
                space.unwrap_or_else(|| {
                    d.namespace_in(plan.scopes[s].query, &mut HashMap::new(), true)
                })
            })
            .collect();
        // The query of `DO UPDATE` reads the target table and `excluded` with the names of their entries.
        for (update, insert) in conflicts {
            let Some(space) = spaces.get_mut(update) else { continue };
            if let Some(Some(name)) = targets.get(insert)
                && let Some(slot) = space.names.get_mut(0)
            {
                slot.clone_from(name);
            }
            if let Some(Some(name)) = excluded.get(insert)
                && let Some(FromItem::Relation(at)) = plan.scopes[update].query.from.get(1)
                && let Some(slot) = space.names.get_mut(*at)
            {
                slot.clone_from(name);
            }
        }
        removed_scans(plan, &mut spaces);
        // The `Output` of the `ModifyTable` of an `INSERT` with `RETURNING` reads the columns of the target table, and a column of `old` or `new` reads the columns of the table with the name of the row.
        for (s, space) in spaces.iter_mut().enumerate() {
            let scope = &plan.scopes[s];
            let write = scope.target.or(scope.query.write.as_ref());
            let returning = scope.target.and_then(|w| w.returning.as_deref());
            if let Some(returning) = write.and_then(|w| w.returning.as_deref())
                && let (Some(shown), Some(table)) =
                    (returning.shown.as_deref(), returning.relations.first())
            {
                let columns = table.columns.iter().map(|c| c.name.clone()).collect();
                space.returning = Some((shown.old.clone(), shown.new.clone(), columns));
            }
            if let (Some(name), Some(returning)) = (&targets[s], returning)
                && let Some(table) = returning.relations.first()
            {
                let columns = table.columns.iter().map(|c| c.name.clone()).collect();
                space.target = Some((name.clone(), columns));
            }
        }
        let mut windows: Vec<Vec<Option<String>>> =
            plan.scopes.iter().map(|s| vec![None; s.query.windows.len()]).collect();
        window_names(&plan.root, &mut windows);
        let subplans = plan
            .sublinks
            .iter()
            .map(|l| SubName {
                link: ptr::from_ref(l.link),
                name: l.name.clone(),
                init: l.init,
                hashed: l.hashed,
                alternative: l.alternative,
            })
            .collect();
        PlanTexts { session, plan, spaces, targets, windows, subplans }
    }

    /// A deparse of the expressions of a scope, with the names of the scope and of the scopes outside it.
    fn deparser(&self, scope: usize, prefix: bool) -> Result<Deparser<'a>> {
        let mut chain = Vec::new();
        let mut at = Some(scope);
        while let Some(s) = at {
            let Some(found) = self.plan.scopes.get(s) else {
                return Err(Error::internal(format!("no scope {s} in the plan")));
            };
            chain.push(s);
            at = found.parent;
        }
        let mut d = Deparser::new(self.session, false, 0);
        d.flat = true;
        d.namespaces = chain.iter().rev().map(|s| self.spaces[*s].clone()).collect();
        d.prefix = prefix;
        d.outer_prefix = true;
        let query = self.plan.scopes[scope].query;
        d.windows.clone_from(&query.windows);
        d.window_targets = query.targets.iter().map(|t| t.expr.clone()).collect();
        d.window_names.clone_from(&self.windows[scope]);
        d.subplans.clone_from(&self.subplans);
        Ok(d)
    }
}

/// The subqueries of `FROM` whose `Subquery Scan` the plan does not keep, as `trivial_subqueryscan` takes the scan out of the plan. A column of such a subquery shows the target of the subquery with the names of the subquery, so the inner subqueries come first. A subquery of a set operation does not count.
fn removed_scans(plan: &Plan<'_>, spaces: &mut [Namespace]) {
    let mut kept = Vec::new();
    subquery_scans(&plan.root, &mut kept);
    let chain = |s: usize| {
        let mut out = vec![s];
        while let Some(parent) =
            out.last().and_then(|&at| plan.scopes.get(at)).and_then(|at| at.parent)
        {
            out.push(parent);
        }
        out.reverse();
        out
    };
    let mut order: Vec<usize> = (0..plan.scopes.len()).collect();
    order.sort_by_key(|&s| std::cmp::Reverse(chain(s).len()));
    for s in order {
        let scope = &plan.scopes[s];
        let Some(parent) = scope.parent.filter(|_| scope.leaf.is_none()) else { continue };
        let outer = plan.scopes[parent].query;
        if outer.set_op.is_some() {
            continue;
        }
        let of = |rel: &rupg_analyze::Relation| {
            rel.subquery.as_deref().is_some_and(|q| ptr::eq(q, scope.query))
        };
        let Some(r) = outer.relations.iter().position(of) else { continue };
        if kept.contains(&(parent, r)) {
            continue;
        }
        // `set_deparse_plan`: the columns of a set operation show as the targets of its leftmost `SELECT`, which is the first member of its `Append`.
        let leftmost = scope.query.set_op.as_ref().and_then(|op| {
            let leaf = scope.query.relations.get(op.left.leftmost())?.subquery.as_deref()?;
            let at = plan.scopes.iter().position(|q| ptr::eq(q.query, leaf))?;
            Some((at, leaf))
        });
        let (s, query) = leftmost.unwrap_or((s, scope.query));
        let namespaces = chain(s).into_iter().map(|at| spaces[at].clone()).collect();
        let targets = query.targets.iter().map(|t| t.expr.clone()).collect();
        spaces[parent].removed.push((r, Rc::new(Removed { namespaces, targets })));
    }
}

/// The scope and the relation of each `Subquery Scan` of the plan.
fn subquery_scans(node: &Node<'_>, out: &mut Vec<(usize, usize)>) {
    if let Kind::SubqueryScan(r) = node.kind {
        out.push((node.scope, r));
    }
    for child in &node.children {
        subquery_scans(child, out);
    }
    for sub in node.init_plans.iter().chain(&node.sub_plans) {
        subquery_scans(&sub.plan, out);
    }
}

/// The names of the windows of the nodes of a plan, by the scope and the number of each window.
/// The scope of the query of `DO UPDATE` of each `ModifyTable` with `ON CONFLICT`, with the scope of the `ModifyTable`.
fn conflict_scopes(node: &Node<'_>, out: &mut Vec<(usize, usize)>) {
    if let Kind::Modify { conflict: Some(conflict), .. } = &node.kind
        && let Some(update) = conflict.scope
    {
        out.push((update, node.scope));
    }
    for child in &node.children {
        conflict_scopes(child, out);
    }
    for sub in node.init_plans.iter().chain(&node.sub_plans) {
        conflict_scopes(&sub.plan, out);
    }
}

fn window_names(node: &Node<'_>, out: &mut [Vec<Option<String>>]) {
    if let Kind::WindowAgg { name, window, .. } = &node.kind
        && let Some(slot) = out.get_mut(node.scope).and_then(|w| w.get_mut(*window))
    {
        *slot = Some(name.clone());
    }
    for child in &node.children {
        window_names(child, out);
    }
    for sub in node.init_plans.iter().chain(&node.sub_plans) {
        window_names(&sub.plan, out);
    }
}

impl Texts for PlanTexts<'_, '_> {
    fn expr(&mut self, scope: usize, expr: &Expr, prefix: bool) -> Result<String> {
        let mut d = self.deparser(scope, prefix)?;
        d.expr(expr, false)?;
        Ok(d.buf)
    }

    fn below_expr(&mut self, scope: usize, expr: &Expr, prefix: bool) -> Result<String> {
        let mut d = self.deparser(scope, prefix)?;
        d.expr(expr, true)?;
        Ok(d.buf)
    }

    fn scan_expr(&mut self, scope: usize, expr: &Expr, prefix: bool, own: usize) -> Result<String> {
        let mut d = self.deparser(scope, prefix)?;
        d.own = Some(own);
        d.expr(expr, false)?;
        Ok(d.buf)
    }

    fn refname(&mut self, scope: usize, relation: usize) -> String {
        if relation == TARGET {
            return self.targets.get(scope).cloned().flatten().unwrap_or_default();
        }
        self.spaces.get(scope).and_then(|s| s.names.get(relation)).cloned().unwrap_or_default()
    }

    fn quote(&self, name: &str) -> String {
        quote_identifier(name)
    }

    fn object(&mut self, oid: u32, function: bool) -> (String, String) {
        let (name, namespace) = if function {
            match builtin::proc_by_oid(oid) {
                Some(proc) => (proc.name, proc.namespace),
                None => return (String::new(), String::new()),
            }
        } else {
            match reg::class_name(oid, self.session) {
                Some(found) => found,
                None => return (String::new(), String::new()),
            }
        };
        let schema = reg::namespace_name(namespace, self.session).unwrap_or_default();
        (name.to_string(), schema.to_string())
    }

    fn window(
        &mut self,
        scope: usize,
        window: usize,
        rows_frame: bool,
        partition: &[usize],
        prefix: bool,
    ) -> Result<String> {
        let query = self.plan.scopes[scope].query;
        let clause = query
            .windows
            .get(window)
            .ok_or_else(|| Error::internal(format!("no window {window} in the query")))?;
        let target = |target: usize| {
            query
                .targets
                .get(target)
                .map(|t| &t.expr)
                .ok_or_else(|| Error::internal("an item of a window with no expression"))
        };
        let mut d = self.deparser(scope, prefix)?;
        d.buf.push('(');
        let items: Vec<&SortGroup> =
            partition.iter().filter_map(|&i| clause.partition.get(i)).collect();
        if !items.is_empty() {
            d.buf.push_str("PARTITION BY ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    d.buf.push_str(", ");
                }
                d.expr(target(item.target)?, false)?;
            }
        }
        if !clause.order.is_empty() {
            if !items.is_empty() {
                d.buf.push(' ');
            }
            d.buf.push_str("ORDER BY ");
            for (i, item) in clause.order.iter().enumerate() {
                if i > 0 {
                    d.buf.push_str(", ");
                }
                // `show_window_keys` shows only the expression of each key, with no direction.
                d.expr(target(item.target)?, false)?;
            }
        }
        let space = !items.is_empty() || !clause.order.is_empty();
        if rows_frame {
            if space {
                d.buf.push(' ');
            }
            d.buf.push_str("ROWS UNBOUNDED PRECEDING");
        } else if clause.frame.options & FRAMEOPTION_NONDEFAULT != 0 {
            if space {
                d.buf.push(' ');
            }
            d.frame(&clause.frame)?;
        }
        d.buf.push(')');
        Ok(d.buf)
    }
}

impl Deparser<'_> {
    /// `get_rule_expr` for a `SubPlan`: the name of the subplan of a subquery, or of the `ANY` that runs in the place of `EXISTS`, with the test of `ANY`, `ALL` or a comparison of rows, where each column of the subquery shows as `(SubPlan any_1).col1`. A column of `SET (a, b) = (SELECT ...)` shows as `(SubPlan multiexpr_1).col1` or `(InitPlan multiexpr_1).col1`. The value of an `InitPlan` shows as `(InitPlan expr_1).col1`, as `get_parameter` shows the parameter that the `InitPlan` sets.
    pub(super) fn subplan_link(&mut self, sub: &SubLink, name: &SubName) -> Result<()> {
        let sub = match &sub.alternative {
            Some(any) if name.alternative => &**any,
            _ => sub,
        };
        let plan = match (name.init, name.hashed) {
            (true, _) => format!("InitPlan {}", name.name),
            (false, true) => format!("hashed SubPlan {}", name.name),
            (false, false) => format!("SubPlan {}", name.name),
        };
        if let SubLinkKind::Multi { column, .. } = sub.kind {
            self.buf.push_str(&format!("({plan}).col{}", column + 1));
            return Ok(());
        }
        if name.init {
            match &sub.test {
                Some(test) => self.subplan_test(test, &plan)?,
                None => self.buf.push_str(&format!("({plan}).col1")),
            }
            return Ok(());
        }
        self.buf.push_str(match sub.kind {
            SubLinkKind::Exists => "EXISTS(",
            SubLinkKind::All => "(ALL ",
            SubLinkKind::Any => "(ANY ",
            SubLinkKind::Array => "ARRAY(",
            SubLinkKind::RowCompare | SubLinkKind::Expr | SubLinkKind::Multi { .. } => "(",
        });
        match &sub.test {
            Some(test) => self.subplan_test(test, &plan)?,
            None => self.buf.push_str(&plan),
        }
        self.buf.push(')');
        Ok(())
    }

    /// The test of a subquery, where a column of the subquery shows as a column of the subplan.
    fn subplan_test(&mut self, test: &Expr, plan: &str) -> Result<()> {
        let saved = self.subplan.replace(format!("({plan})"));
        let done = self.expr(test, false);
        self.subplan = saved;
        done
    }
}

//! `transformSelectStmt` of `analyze.c`: `FROM`, the target list, the `WHERE` condition, `GROUP BY`, `HAVING`, `ORDER BY`, `DISTINCT`, `LIMIT` and `OFFSET`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{LimitOption, List, Node, SelectStmt, SetOperation};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::Context;
use crate::colname::figure_colname;
use crate::expr::{Expr, ExprKind};
use crate::from::{FromItem, Relation};
use crate::sort::SortGroup;
use crate::window::WindowClause;

/// The query tree of a `SELECT`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// The relations of `FROM`, which a [`crate::Var`] names by index.
    pub relations: Vec<Relation>,
    /// The parts of `FROM`. The query joins them with a cross join.
    pub from: Vec<FromItem>,
    /// The columns of the result, then the junk columns that only `ORDER BY` and `GROUP BY` read.
    pub targets: Vec<Target>,
    /// The `WHERE` condition, of type `boolean`.
    pub filter: Option<Expr>,
    /// True when the query makes groups of rows: it has an aggregate call, `GROUP BY` or `HAVING`. With no item of `GROUP BY`, all the rows make one group, also when there are no rows.
    pub grouped: bool,
    /// The items of `GROUP BY`.
    pub group: Vec<SortGroup>,
    /// `groupingSets`: the grouping sets of `GROUP BY`, or none for a plain `GROUP BY`. Each set makes groups on its own items, and the other items of `GROUP BY` are null in its rows.
    pub grouping_sets: Vec<crate::GroupingSet>,
    /// `groupDistinct`: `GROUP BY DISTINCT`, which makes the groups of the same grouping set once.
    pub group_distinct: bool,
    /// The `HAVING` condition, of type `boolean`, which each group must pass.
    pub having: Option<Expr>,
    /// The items of `ORDER BY`.
    pub sort: Vec<SortGroup>,
    /// The items of `DISTINCT` or `DISTINCT ON`. The items of `ORDER BY` come first.
    pub distinct: Vec<SortGroup>,
    /// True for `DISTINCT ON`, which keeps the first row of each group.
    pub distinct_on: bool,
    /// `OFFSET` as `bigint`.
    pub offset: Option<Expr>,
    /// `LIMIT` or `FETCH FIRST` as `bigint`.
    pub limit: Option<Expr>,
    /// `FETCH FIRST ... WITH TIES`: the rows equal to the last row on the items of `ORDER BY` come too.
    pub with_ties: bool,
    /// The type OID of each parameter, from `$1`.
    pub params: Vec<u32>,
    /// The warnings and notices of the analysis, which the session sends before the result.
    pub notices: Vec<Error>,
    /// `UNION`, `INTERSECT` or `EXCEPT`. Each `SELECT` of the operation is a subquery in [`Query::relations`], and the rows of the operation are the rows of the leftmost one, which the targets read. The query has no `FROM`.
    pub set_op: Option<crate::SetOp>,
    /// The windows of the window function calls and of the `WINDOW` clause. A [`crate::WindowFunc`] names its window by its number, from 1.
    pub windows: Vec<WindowClause>,
    /// `cteList`: the queries of the `WITH` clause of the query.
    pub ctes: Vec<crate::Cte>,
    /// `hasRecursive`: the query has `WITH RECURSIVE`.
    pub recursive: bool,
    /// `rowMarks`: the relations that `FOR UPDATE` or `FOR SHARE` locks.
    pub row_marks: Vec<crate::RowMark>,
    /// The table that an `INSERT` writes, or `None` for a `SELECT`.
    pub write: Option<crate::Write>,
    /// `outargs` of `CallStmt`: the arguments of `CALL` for the `INOUT` and `OUT` arguments of the procedure, in order.
    pub out_args: Vec<crate::OutArg>,
    /// The number of subqueries of `FROM` that the planner merged into the query, which stay in the range table of PostgreSQL with no plan.
    pub pulled: usize,
    /// The queries of `preprocess_minmax_aggregates` for a query whose aggregate calls are each `min` or `max` of one table, which the planner can run as an `InitPlan` that reads the first row of an index.
    pub minmax: Option<Box<MinMax>>,
}

/// `MinMaxAggInfo`: the form of a query whose aggregate calls are each `min` or `max`, with a subquery for each aggregate call that reads the first value in the order of its sort operator.
#[derive(Clone, Debug, PartialEq)]
pub struct MinMax {
    /// The two subqueries of each aggregate call that is not the same as an earlier one: the first one has the nulls first for `max` and last for `min`, as `build_minmax_path` tries it first, and the second one has the other order of the nulls.
    pub aggs: Vec<[Query; 2]>,
    /// The targets of the query, with a subquery of `aggs` in the place of each aggregate call. A subquery is the first subquery of its aggregate call.
    pub targets: Vec<Expr>,
    /// The `HAVING` condition of the query with the same subqueries.
    pub having: Option<Expr>,
}

/// A column of the result.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The name of the column in `RowDescription`.
    pub name: String,
    pub expr: Expr,
    /// The OID of the table and the attribute number of the column when the target is a plain column, as `resorigtbl` and `resorigcol`.
    pub origin: Option<(u32, i16)>,
    /// `resjunk`: true for a column that only `ORDER BY` or `GROUP BY` reads, which the result does not show.
    pub junk: bool,
}

impl Query {
    /// The expressions of the query, in the order of `query_tree_walker`: the targets, the conditions of the joins, `WHERE`, `HAVING`, `OFFSET`, `LIMIT`, the offsets of the frames of the windows, the calls of the functions in `FROM`, the rows of the `VALUES` lists and the arguments of `TABLESAMPLE`.
    pub fn exprs(&self) -> Vec<&Expr> {
        fn joins<'a>(item: &'a FromItem, out: &mut Vec<&'a Expr>) {
            if let FromItem::Join(j) = item {
                joins(&j.left, out);
                joins(&j.right, out);
                out.extend(&j.on);
            }
        }
        let mut out: Vec<&Expr> = self.targets.iter().map(|t| &t.expr).collect();
        for item in &self.from {
            joins(item, &mut out);
        }
        out.extend(self.filter.iter().chain(&self.having).chain(&self.offset).chain(&self.limit));
        for window in &self.windows {
            out.extend(window.frame.start.iter().chain(&window.frame.end));
        }
        for relation in &self.relations {
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

    /// The expressions of the query, in the order of [`Query::exprs`].
    pub fn exprs_mut(&mut self) -> Vec<&mut Expr> {
        fn joins<'a>(item: &'a mut FromItem, out: &mut Vec<&'a mut Expr>) {
            if let FromItem::Join(j) = item {
                let j = &mut **j;
                joins(&mut j.left, out);
                joins(&mut j.right, out);
                out.extend(&mut j.on);
            }
        }
        let mut out: Vec<&mut Expr> = self.targets.iter_mut().map(|t| &mut t.expr).collect();
        for item in &mut self.from {
            joins(item, &mut out);
        }
        out.extend(
            self.filter
                .iter_mut()
                .chain(&mut self.having)
                .chain(&mut self.offset)
                .chain(&mut self.limit),
        );
        for window in &mut self.windows {
            out.extend(window.frame.start.iter_mut().chain(&mut window.frame.end));
        }
        for relation in &mut self.relations {
            if let Some(function) = &mut relation.function {
                out.extend(function.calls.iter_mut().map(|(call, _)| call));
            }
            out.extend(relation.values.iter_mut().flatten().flatten());
            if let Some(sample) = &mut relation.tablesample {
                out.extend(sample.args.iter_mut().chain(&mut sample.repeatable));
            }
        }
        out
    }

    /// `hasTargetSRFs`: true when a target calls a function that gives a set.
    pub fn has_target_srfs(&self) -> bool {
        self.targets.iter().any(|t| t.expr.first_set_call().is_some())
    }

    /// The expressions of the query and of its subqueries in `FROM`, each with its depth: `depth` for the query, and one more for each subquery in `FROM` between the expression and the query. The subqueries in `FROM` come after the expressions of the query, as in `query_tree_walker`.
    pub fn all_exprs(&self, depth: usize) -> Vec<(&Expr, usize)> {
        let mut out: Vec<(&Expr, usize)> = self.exprs().into_iter().map(|e| (e, depth)).collect();
        for relation in &self.relations {
            if let Some(sub) = &relation.subquery {
                out.extend(sub.all_exprs(depth + 1));
            }
        }
        out
    }

    /// Calls `f` for each expression of [`Query::all_exprs`], with its depth.
    pub fn each_expr_mut(&mut self, depth: usize, f: &mut impl FnMut(&mut Expr, usize)) {
        for expr in self.exprs_mut() {
            f(expr, depth);
        }
        for relation in &mut self.relations {
            if let Some(sub) = &mut relation.subquery {
                sub.each_expr_mut(depth + 1, f);
            }
        }
    }
}

/// The error of a clause that the analyzer does not take yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

impl Analyzer<'_> {
    /// The query tree of a `SELECT`.
    pub(crate) fn select(&mut self, s: &SelectStmt) -> Result<Query> {
        // A `SELECT` of a set operation keeps its columns of type `unknown`, so that the operation can give them a type.
        let keep_unknowns = std::mem::take(&mut self.keep_unknowns);
        let parent = self.parent_cte.take();
        if let Some(with) = s.withClause.as_deref() {
            self.transform_with(with)?;
        }
        let mut query = if s.op != SetOperation::SETOP_NONE {
            self.set_operation(s, parent)?
        } else if !s.valuesLists.is_empty() {
            self.values_clause(s)?
        } else {
            self.simple_select(s, keep_unknowns)?
        };
        query.recursive = s.withClause.as_deref().is_some_and(|w| w.recursive);
        query.ctes = self.take_ctes()?;
        Ok(query)
    }

    /// `transformTargetList`: the targets of a list of `ResTarget` items in the place `kind`, with `*` and `x.*` expanded to their columns.
    pub(crate) fn target_list(&mut self, list: &List, kind: Kind) -> Result<Vec<Target>> {
        let mut targets = Vec::with_capacity(list.len());
        for target in list {
            let Some(Node::ResTarget(rt)) = target else {
                return Err(Error::internal("a target that is not ResTarget"));
            };
            if let Some(Node::ColumnRef(c)) = &rt.val
                && let Some(columns) = self.expand_star(c)?
            {
                for (name, expr) in columns {
                    targets.push(Target { name, expr, origin: None, junk: false });
                }
                continue;
            }
            if let Some(Node::A_Indirection(ind)) = &rt.val
                && let Some(fields) = self.with_kind(kind, |a| a.expand_indirection_star(ind))?
            {
                for (name, expr) in fields {
                    targets.push(Target { name, expr, origin: None, junk: false });
                }
                continue;
            }
            let expr = self.with_kind(kind, |a| a.transform(rt.val.as_ref()))?;
            let name = match &rt.name {
                Some(name) => name.to_string(),
                None => figure_colname(rt.val.as_ref()),
            };
            targets.push(Target { name, expr, origin: None, junk: false });
        }
        Ok(targets)
    }

    /// `markTargetListOrigins`: a target that is a column of a relation names the table and the column, also for a column of a query outside this query. A column of a view is a column of the view. A column of a subquery in FROM has the origin of the column of the subquery, and a column of a function in FROM has no origin.
    pub(crate) fn mark_origins(&self, targets: &mut [Target]) {
        for target in targets {
            if let ExprKind::Var(var) = &target.expr.kind {
                let relation = &self.level(var.levels_up).relations[var.relation];
                let written = relation.cte.as_ref().filter(|c| c.write.is_some());
                target.origin = match (&relation.subquery, written) {
                    (None, Some(cte)) => usize::try_from(var.attnum - 1)
                        .ok()
                        .and_then(|i| cte.origins.get(i).copied().flatten()),
                    (None, None)
                        if relation.function.is_some()
                            || relation.values.is_some()
                            || relation.cte.is_some() =>
                    {
                        None
                    }
                    (None, None) => Some((relation.oid, var.attnum)),
                    (Some(_), _) if relation.oid != 0 => Some((relation.oid, var.attnum)),
                    (Some(sub), _) => usize::try_from(var.attnum - 1)
                        .ok()
                        .and_then(|i| sub.targets.iter().filter(|t| !t.junk).nth(i))
                        .and_then(|t| t.origin),
                };
            }
        }
    }

    /// The query tree of a `SELECT` that is not a set operation or a `VALUES` list, with its windows.
    fn simple_select(&mut self, s: &SelectStmt, keep_unknowns: bool) -> Result<Query> {
        if s.intoClause.is_some() {
            return Err(not_yet("SELECT INTO"));
        }
        // The windows of the `WINDOW` clause come first, and each query has its own windows.
        let defs = s.windowClause.iter().flatten().filter_map(|node| match node {
            Node::WindowDef(def) => Some((**def).clone()),
            _ => None,
        });
        let windowdefs = std::mem::replace(&mut self.windowdefs, defs.collect());
        let has_windows = std::mem::replace(&mut self.has_windows, false);
        let result = self.plain_select(s, keep_unknowns);
        self.windowdefs = windowdefs;
        self.has_windows = has_windows;
        result
    }

    /// The query tree of a `SELECT` that is not a set operation or a `VALUES` list.
    fn plain_select(&mut self, s: &SelectStmt, keep_unknowns: bool) -> Result<Query> {
        // The subqueries of the query are not the value of the assignment.
        let assign = self.assign.take();
        let from = self.transform_from(&s.fromClause)?;
        let mut targets = self.target_list(&s.targetList, Kind::Select)?;
        if let Some(assign) = assign {
            self.assign_target(&mut targets, assign)?;
        }
        let filter = match &s.whereClause {
            Some(node) => {
                let expr = self.with_kind(Kind::Where, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        let mut having = match &s.havingClause {
            Some(node) => {
                let expr = self.with_kind(Kind::Having, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "HAVING")?)
            }
            None => None,
        };
        self.mark_origins(&mut targets);
        let sort = self.sort_clause(&s.sortClause, &mut targets)?;
        let (group, mut grouping_sets) = self.group_clause(&s.groupClause, &mut targets, &sort)?;
        // The parser gives a list with one null item for `DISTINCT` and the expressions for `DISTINCT ON`.
        let distinct_on = matches!(s.distinctClause.first(), Some(Some(_)));
        let distinct = if s.distinctClause.is_empty() {
            Vec::new()
        } else if distinct_on {
            self.distinct_on_clause(&s.distinctClause, &mut targets, &sort)?
        } else {
            self.distinct_clause(&mut targets, &sort, false)?
        };
        let with_ties = s.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES;
        let offset = self.with_kind(Kind::Offset, |a| {
            a.limit_clause(s.limitOffset.as_ref(), "OFFSET", with_ties)
        })?;
        let limit = self.with_kind(Kind::Limit, |a| {
            a.limit_clause(s.limitCount.as_ref(), "LIMIT", with_ties)
        })?;
        let windows = self.window_clauses(&mut targets)?;
        // `resolveTargetListUnknowns`: a column of type `unknown` becomes `text`.
        for target in targets.iter_mut().filter(|_| !keep_unknowns) {
            if target.expr.ty == oid::UNKNOWN {
                let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
                target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
            }
        }
        let group_distinct = s.groupDistinct;
        let grouped =
            self.has_aggs || !group.is_empty() || !grouping_sets.is_empty() || having.is_some();
        if grouped {
            self.check_grouping(
                &mut targets,
                having.as_mut(),
                &group,
                &mut grouping_sets,
                group_distinct,
            )?;
        }
        let relations = std::mem::take(&mut self.scope.relations);
        let mut query = Query {
            relations,
            from,
            targets,
            filter,
            grouped,
            group,
            grouping_sets,
            group_distinct,
            having,
            sort,
            distinct,
            distinct_on,
            offset,
            limit,
            with_ties,
            params: Vec::new(),
            notices: Vec::new(),
            set_op: None,
            windows,
            ctes: Vec::new(),
            recursive: false,
            row_marks: Vec::new(),
            write: None,
            out_args: Vec::new(),
            pulled: 0,
            minmax: None,
        };
        for clause in s.lockingClause.iter().flatten() {
            if let Node::LockingClause(clause) = clause {
                crate::locking::transform_locking_clause(&mut query, clause)?;
            }
        }
        Ok(query)
    }
}

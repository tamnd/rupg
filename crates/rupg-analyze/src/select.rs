//! `transformSelectStmt` of `analyze.c`: `FROM`, the target list, the `WHERE` condition, `GROUP BY`, `HAVING`, `ORDER BY`, `DISTINCT`, `LIMIT` and `OFFSET`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{LimitOption, Node, SelectStmt, SetOperation};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::Context;
use crate::colname::figure_colname;
use crate::expr::{Expr, ExprKind};
use crate::from::{FromItem, Relation};
use crate::sort::SortGroup;

/// The query tree of a `SELECT`.
#[derive(Clone, Debug, PartialEq)]
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
    /// The expressions of the query, in the order of `query_tree_walker`: the targets, the conditions of the joins, `WHERE`, `HAVING`, `OFFSET`, `LIMIT`, the calls of the functions in `FROM` and the rows of the `VALUES` lists.
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
        for relation in &self.relations {
            if let Some(function) = &relation.function {
                out.extend(function.calls.iter().map(|(call, _)| call));
            }
            out.extend(relation.values.iter().flatten().flatten());
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
        for relation in &mut self.relations {
            if let Some(function) = &mut relation.function {
                out.extend(function.calls.iter_mut().map(|(call, _)| call));
            }
            out.extend(relation.values.iter_mut().flatten().flatten());
        }
        out
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
        if s.op != SetOperation::SETOP_NONE {
            return self.set_operation(s);
        }
        if !s.valuesLists.is_empty() {
            return self.values_clause(s);
        }
        if s.withClause.is_some() {
            return Err(not_yet("WITH"));
        }
        if s.intoClause.is_some() {
            return Err(not_yet("SELECT INTO"));
        }
        let from = self.transform_from(&s.fromClause)?;
        let mut targets = Vec::with_capacity(s.targetList.len());
        for target in &s.targetList {
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
            let expr = self.with_kind(Kind::Select, |a| a.transform(rt.val.as_ref()))?;
            let name = match &rt.name {
                Some(name) => name.to_string(),
                None => figure_colname(rt.val.as_ref()),
            };
            targets.push(Target { name, expr, origin: None, junk: false });
        }
        let filter = match &s.whereClause {
            Some(node) => {
                let expr = self.with_kind(Kind::Where, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        let having = match &s.havingClause {
            Some(node) => {
                let expr = self.with_kind(Kind::Having, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "HAVING")?)
            }
            None => None,
        };
        if !s.windowClause.is_empty() {
            return Err(not_yet("WINDOW"));
        }
        if !s.lockingClause.is_empty() {
            return Err(not_yet("FOR UPDATE and FOR SHARE"));
        }
        // markTargetListOrigins: a target that is a column of a relation names the table and the column, also for a column of a query outside this query. A column of a view is a column of the view. A column of a subquery in FROM has the origin of the column of the subquery, and a column of a function in FROM has no origin.
        for target in &mut targets {
            if let ExprKind::Var(var) = &target.expr.kind {
                let relation = &self.level(var.levels_up).relations[var.relation];
                target.origin = match &relation.subquery {
                    None if relation.function.is_some() || relation.values.is_some() => None,
                    None => Some((relation.oid, var.attnum)),
                    Some(_) if relation.oid != 0 => Some((relation.oid, var.attnum)),
                    Some(sub) => usize::try_from(var.attnum - 1)
                        .ok()
                        .and_then(|i| sub.targets.iter().filter(|t| !t.junk).nth(i))
                        .and_then(|t| t.origin),
                };
            }
        }
        let sort = self.sort_clause(&s.sortClause, &mut targets)?;
        let (group, empty_set) = self.group_clause(&s.groupClause, &mut targets, &sort)?;
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
        // `resolveTargetListUnknowns`: a column of type `unknown` becomes `text`.
        for target in targets.iter_mut().filter(|_| !keep_unknowns) {
            if target.expr.ty == oid::UNKNOWN {
                let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
                target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
            }
        }
        let grouped = self.has_aggs || !group.is_empty() || empty_set || having.is_some();
        if grouped {
            self.check_grouping(&targets, having.as_ref(), &group)?;
        }
        let relations = std::mem::take(&mut self.scope.relations);
        Ok(Query {
            relations,
            from,
            targets,
            filter,
            grouped,
            group,
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
        })
    }
}

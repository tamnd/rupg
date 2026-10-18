//! Aggregate calls, as `transformAggregateCall` and `parseCheckAggregates` of `parse_agg.c` make and check them.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{List, Node, WindowDef};
use rupg_types::oid;

use crate::Analyzer;
use crate::coerce::AtOpt;
use crate::expr::{Aggref, Expr, ExprKind, GroupingFunc, NullTreatment, Var};
use crate::select::Target;
use crate::sort::{GroupingSet, SortGroup, expand_grouping_sets};
use crate::typename::place;

/// The part of a query that the analyzer reads, as `ParseExprKind`. It tells where an aggregate call can be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    #[default]
    Other,
    Select,
    Where,
    JoinOn,
    Having,
    Filter,
    GroupBy,
    OrderBy,
    DistinctOn,
    Limit,
    Offset,
    /// `EXPR_KIND_WINDOW_PARTITION`: the `PARTITION BY` of a window.
    WindowPartition,
    /// `EXPR_KIND_WINDOW_ORDER`: the `ORDER BY` of a window.
    WindowOrder,
    /// `EXPR_KIND_WINDOW_FRAME_RANGE`: an offset of a frame in `RANGE` mode.
    WindowRange,
    /// `EXPR_KIND_WINDOW_FRAME_ROWS`: an offset of a frame in `ROWS` mode.
    WindowRows,
    /// `EXPR_KIND_WINDOW_FRAME_GROUPS`: an offset of a frame in `GROUPS` mode.
    WindowGroups,
    /// `EXPR_KIND_COLUMN_DEFAULT`.
    ColumnDefault,
    /// `EXPR_KIND_CHECK_CONSTRAINT`.
    Check,
    /// `EXPR_KIND_INDEX_EXPRESSION`.
    IndexExpression,
    /// `EXPR_KIND_INDEX_PREDICATE`.
    IndexPredicate,
    /// `EXPR_KIND_FROM_FUNCTION`.
    FromFunction,
    /// `EXPR_KIND_VALUES`.
    Values,
    /// `EXPR_KIND_VALUES_SINGLE`: a `VALUES` list with one row.
    ValuesSingle,
    /// `EXPR_KIND_UPDATE_SOURCE`: the source of an assignment of `UPDATE ... SET`.
    UpdateSource,
    /// `EXPR_KIND_EXECUTE_PARAMETER`.
    ExecuteParameter,
    /// `EXPR_KIND_RETURNING`: the list of `RETURNING`.
    Returning,
}

impl Kind {
    /// `ParseExprKindName`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Other => "???",
            Kind::Select => "SELECT",
            Kind::Where => "WHERE",
            Kind::JoinOn => "JOIN/ON",
            Kind::Having => "HAVING",
            Kind::Filter => "FILTER",
            Kind::GroupBy => "GROUP BY",
            Kind::OrderBy => "ORDER BY",
            Kind::DistinctOn => "DISTINCT ON",
            Kind::Limit => "LIMIT",
            Kind::Offset => "OFFSET",
            Kind::WindowPartition => "window PARTITION BY",
            Kind::WindowOrder => "window ORDER BY",
            Kind::WindowRange => "window RANGE",
            Kind::WindowRows => "window ROWS",
            Kind::WindowGroups => "window GROUPS",
            Kind::ColumnDefault => "DEFAULT expressions",
            Kind::Check => "check constraints",
            Kind::IndexExpression => "index expressions",
            Kind::IndexPredicate => "index predicates",
            Kind::FromFunction => "functions in FROM",
            Kind::Values | Kind::ValuesSingle => "VALUES",
            Kind::UpdateSource => "UPDATE",
            Kind::ExecuteParameter => "EXECUTE parameters",
            Kind::Returning => "RETURNING",
        }
    }

    /// True for an expression that is not part of a query: an expression of a statement that defines an object, or a parameter of `EXECUTE`. There an aggregate, a window function, a set-returning function and a subquery are errors.
    pub(crate) fn is_standalone(self) -> bool {
        matches!(
            self,
            Kind::ColumnDefault
                | Kind::Check
                | Kind::IndexExpression
                | Kind::IndexPredicate
                | Kind::ExecuteParameter
        )
    }

    /// The name of the place in the error of a subquery, as `transformSubLink` gives it.
    pub(crate) fn subquery_name(self) -> &'static str {
        match self {
            Kind::ColumnDefault => "DEFAULT expression",
            Kind::Check => "check constraint",
            Kind::IndexExpression => "index expression",
            Kind::ExecuteParameter => "EXECUTE parameter",
            _ => "index predicate",
        }
    }
}

/// The parts of a function call that only an aggregate or a window function can have.
pub(crate) struct Parts<'a> {
    /// `agg_star`: `f(*)`.
    pub(crate) star: bool,
    /// `agg_distinct`: `f(DISTINCT ...)`.
    pub(crate) distinct: bool,
    /// `agg_within_group`: `f(...) WITHIN GROUP (ORDER BY ...)`.
    pub(crate) within_group: bool,
    /// `agg_order`: the items of `ORDER BY`.
    pub(crate) order: &'a List,
    /// The condition of `FILTER (WHERE ...)`.
    pub(crate) filter: Option<Expr>,
    /// The window of `OVER`.
    pub(crate) over: Option<&'a WindowDef>,
    /// The clause `RESPECT NULLS` or `IGNORE NULLS` of the call.
    pub(crate) nulls: NullTreatment,
    /// The number of the calls of functions that give a set before the arguments, as `last_srf` of `ParseFuncOrColumn`.
    pub(crate) srfs: usize,
}

/// The hint of the errors of a call of a function that gives a set inside an expression that cannot take a set.
pub(crate) const SRF_HINT: &str =
    "You might be able to move the set-returning function into a LATERAL FROM item.";

/// `check_agg_arguments_walker`: the first call of a function that gives a set or of a window function in the arguments of an aggregate, outside their subqueries.
fn nested_set(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Window(_) => Some(expr),
        ExprKind::Func(f) if f.retset => Some(expr),
        _ => expr.children().into_iter().find_map(nested_set),
    }
}

/// The first aggregate call or `GROUPING` in the arguments of an aggregate, outside their subqueries.
fn nested_agg(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Agg(_) | ExprKind::Grouping(_) => Some(expr),
        _ => expr.children().into_iter().find_map(nested_agg),
    }
}

/// check_agg_arguments: the level of an aggregate or of `GROUPING`. A call whose arguments read only the columns of outer queries belongs to the outer query.
fn agg_level<'a>(args: impl Iterator<Item = &'a Expr>) -> usize {
    args.filter_map(|e| {
        let mut min: Option<usize> = None;
        e.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up >= depth
            {
                let up = var.levels_up - depth;
                min = Some(min.map_or(up, |m| m.min(up)));
            }
            None::<()>
        });
        min
    })
    .min()
    .unwrap_or(0)
}

/// An error of an aggregate call in a wrong place, with the SQLSTATE `42803`.
pub(crate) fn grouping_error(message: String, at: Option<usize>) -> Error {
    Error::new(SqlState::GROUPING_ERROR, message).at_opt(at)
}

impl Analyzer<'_> {
    /// Runs `f` with the kind of the expression set to `kind`, as `transformExpr` does with `p_expr_kind`.
    pub(crate) fn with_kind<T>(
        &mut self,
        kind: Kind,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let saved = std::mem::replace(&mut self.kind, kind);
        let result = f(self);
        self.kind = saved;
        result
    }

    /// `transformAggregateCall`: the items of `ORDER BY` and `DISTINCT`, or the direct arguments and the items of `WITHIN GROUP` of an ordered-set aggregate, then the checks of `check_agglevels_and_constraints`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn aggregate_call(
        &mut self,
        oid: u32,
        kind: u8,
        mut args: Vec<Expr>,
        parts: Parts<'_>,
        variadic: bool,
        ty: u32,
        at: Option<usize>,
    ) -> Result<Expr> {
        let ordered_set = kind != b'n';
        // The items of WITHIN GROUP are the last arguments.
        let direct = if ordered_set {
            args.drain(..args.len() - parts.order.len()).collect()
        } else {
            Vec::new()
        };
        let nargs = args.len();
        let mut targets: Vec<Target> = args
            .into_iter()
            .map(|expr| Target { name: String::new(), expr, origin: None, junk: false })
            .collect();
        let order = if ordered_set {
            let mut order = Vec::with_capacity(nargs);
            for (target, item) in parts.order.iter().enumerate() {
                let Some(Node::SortBy(by)) = item else {
                    return Err(Error::internal("an item of WITHIN GROUP that is not SortBy"));
                };
                self.add_sort(by, target, &mut targets, &mut order)?;
            }
            order
        } else {
            self.with_kind(Kind::OrderBy, |a| a.sort_clause_sql99(parts.order, &mut targets))?
        };
        let distinct = if parts.distinct {
            let list = self.distinct_clause(&mut targets, &order, true)?;
            for item in &list {
                if item.operator == 0 {
                    let expr = &targets[item.target].expr;
                    return Err(Error::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!(
                            "could not identify an ordering operator for type {}",
                            crate::types::name(expr.ty)
                        ),
                    )
                    .with_detail("Aggregates with DISTINCT must be able to sort their inputs.")
                    .at_opt(expr.place()));
                }
            }
            list
        } else {
            Vec::new()
        };
        let args: Vec<Expr> = targets.into_iter().map(|t| t.expr).collect();
        let filter = parts.filter.map(Box::new);
        // check_agg_arguments: an aggregate call in the arguments is of the same level, and a set or a window function cannot be an argument. The walk gives the error of a set or a window function at once and the error of an aggregate call after the walk. The direct arguments come last.
        let inner = args
            .iter()
            .chain(filter.as_deref())
            .find_map(nested_set)
            .or_else(|| args.iter().chain(filter.as_deref()).find_map(nested_agg))
            .or_else(|| direct.iter().find_map(nested_set))
            .or_else(|| direct.iter().find_map(nested_agg));
        if let Some(inner) = inner {
            match inner.kind {
                ExprKind::Agg(_) | ExprKind::Grouping(_) => {
                    return Err(grouping_error(
                        "aggregate function calls cannot be nested".into(),
                        inner.location,
                    ));
                }
                ExprKind::Window(_) => {
                    return Err(grouping_error(
                        "aggregate function calls cannot contain window function calls".into(),
                        inner.location,
                    ));
                }
                _ => {}
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "aggregate function calls cannot contain set-returning function calls",
            )
            .with_hint(SRF_HINT)
            .at_opt(inner.location));
        }
        let level = agg_level(args.iter().chain(filter.as_deref()));
        self.check_agg_place(level, "aggregate functions", at)?;
        let agg = Aggref {
            oid,
            kind,
            direct,
            args,
            nargs,
            order,
            distinct,
            star: parts.star,
            variadic,
            filter,
            levels_up: level,
        };
        Ok(Expr { kind: ExprKind::Agg(Box::new(agg)), ty, typmod: -1, location: at })
    }

    /// `transformGroupingFunc`: a call of `GROUPING`, which the checks of `parseCheckAggregates` complete.
    pub(crate) fn grouping_func(&mut self, g: &rupg_sql::nodes::GroupingFunc) -> Result<Expr> {
        let at = place(g.location);
        if g.args.len() > 31 {
            return Err(Error::new(
                SqlState::TOO_MANY_ARGUMENTS,
                "GROUPING must have fewer than 32 arguments",
            )
            .at_opt(at));
        }
        let args = self.transform_expression_list(&g.args)?;
        let inner = args.iter().find_map(nested_set).or_else(|| args.iter().find_map(nested_agg));
        if let Some(inner) = inner {
            let message = match inner.kind {
                ExprKind::Agg(_) | ExprKind::Grouping(_) => {
                    "aggregate function calls cannot be nested"
                }
                ExprKind::Window(_) => {
                    "aggregate function calls cannot contain window function calls"
                }
                _ => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "aggregate function calls cannot contain set-returning function calls",
                    )
                    .with_hint(SRF_HINT)
                    .at_opt(inner.location));
                }
            };
            return Err(grouping_error(message.into(), inner.location));
        }
        let levels_up = agg_level(args.iter());
        self.check_agg_place(levels_up, "grouping operations", at)?;
        let grouping = GroupingFunc { args, refs: Vec::new(), levels_up };
        Ok(Expr::new(ExprKind::Grouping(Box::new(grouping)), oid::INT4).at(at))
    }

    /// The checks of `check_agglevels_and_constraints`: an aggregate call or a `GROUPING` of the query `level` levels up must be in a part of that query that can have it. The query then has aggregates. `what` names the kind of the call in the errors.
    fn check_agg_place(&mut self, level: usize, what: &str, at: Option<usize>) -> Result<()> {
        // The part of the query `level` levels up that has the subquery decides where the call can be.
        let outer = self.outer_kinds.len().checked_sub(level).filter(|_| level > 0);
        let place = match outer {
            Some(i) => self.outer_kinds.get(i).and_then(|(kind, _)| *kind),
            None => Some(self.kind),
        };
        let Some(place) = place else {
            return Err(grouping_error(
                format!("{what} are not allowed in FROM clause of their own query level"),
                at,
            ));
        };
        match place {
            Kind::JoinOn => {
                return Err(grouping_error(
                    format!("{what} are not allowed in JOIN conditions"),
                    at,
                ));
            }
            Kind::Where
            | Kind::Filter
            | Kind::GroupBy
            | Kind::Limit
            | Kind::Offset
            | Kind::WindowRange
            | Kind::WindowRows
            | Kind::WindowGroups
            | Kind::ColumnDefault
            | Kind::Check
            | Kind::IndexExpression
            | Kind::IndexPredicate
            | Kind::FromFunction
            | Kind::Values
            | Kind::ValuesSingle
            | Kind::UpdateSource
            | Kind::ExecuteParameter
            | Kind::Returning => {
                return Err(grouping_error(
                    format!("{what} are not allowed in {}", place.name()),
                    at,
                ));
            }
            _ => {}
        }
        match outer.and_then(|i| self.outer_kinds.get_mut(i)) {
            Some((_, has_aggs)) => *has_aggs = true,
            None => self.has_aggs = true,
        }
        Ok(())
    }

    /// `parseCheckAggregates`: each column that the select list and `HAVING` read outside of an aggregate call must be an item of `GROUP BY`, or a column of a table whose primary key is in all the grouping sets. Each argument of `GROUPING` must be an item of `GROUP BY`. When the grouping sets expand to one set that is not empty, the query has no grouping sets.
    pub(crate) fn check_grouping(
        &self,
        targets: &mut [Target],
        mut having: Option<&mut Expr>,
        group: &[SortGroup],
        sets: &mut Vec<GroupingSet>,
        distinct: bool,
    ) -> Result<()> {
        // `gset_common`: the items that are in all the grouping sets.
        let mut common = None;
        if !sets.is_empty() {
            let Some(expanded) = expand_grouping_sets(sets, distinct, 4096) else {
                return Err(Error::new(
                    SqlState::STATEMENT_TOO_COMPLEX,
                    "too many grouping sets present (maximum 4096)",
                ));
            };
            let mut items = expanded.first().cloned().unwrap_or_default();
            for set in expanded.iter().skip(1) {
                items.retain(|item| set.contains(item));
            }
            if expanded.len() == 1 && !group.is_empty() {
                sets.clear();
            } else {
                common = Some(items);
            }
        }
        let grouped: Vec<(usize, Expr)> =
            group.iter().map(|g| (g.target, targets[g.target].expr.clone())).collect();
        let non_var = grouped.iter().any(|(_, e)| !matches!(e.kind, ExprKind::Var(_)));
        let level_vars = |all: bool| -> Vec<Var> {
            grouped
                .iter()
                .filter(|(t, _)| all || common.as_ref().is_none_or(|c| c.contains(t)))
                .filter_map(|(_, e)| match e.kind {
                    ExprKind::Var(var) if var.levels_up == 0 => Some(var),
                    _ => None,
                })
                .collect()
        };
        let vars = level_vars(true);
        let common_vars = level_vars(false);
        let exprs: Vec<&Expr> = grouped.iter().map(|(_, e)| e).collect();
        let mut check = Check {
            analyzer: self,
            grouped: &exprs,
            raised: Vec::new(),
            non_var,
            vars: &vars,
            common: &common_vars,
            proven: Vec::new(),
        };
        for target in targets.iter_mut() {
            finalize_grouping(&mut target.expr, 0, &grouped, non_var)?;
        }
        for target in targets.iter() {
            check.expr(&target.expr, 0)?;
        }
        if let Some(having) = having.as_deref_mut() {
            finalize_grouping(having, 0, &grouped, non_var)?;
            check.expr(having, 0)?;
        }
        self.check_recursive_aggs(targets.iter().map(|t| &t.expr).chain(having.as_deref()))
    }
}

/// `finalize_grouping_exprs`: the items of `GROUP BY` that the arguments of each `GROUPING` of the query name, in an expression of the query or of a subquery `depth` levels down. `grouped` has the targets and the expressions of the items.
fn finalize_grouping(
    expr: &mut Expr,
    depth: usize,
    grouped: &[(usize, Expr)],
    non_var: bool,
) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Grouping(g) if g.levels_up == depth => {
            let mut refs = Vec::with_capacity(g.args.len());
            for arg in &g.args {
                // An argument must be an item of the query itself. A column of a table whose primary key is an item is not.
                let found = match arg.kind {
                    ExprKind::Var(var) if var.levels_up == depth => {
                        let var = Var { levels_up: 0, ..var };
                        grouped.iter().find(|(_, e)| matches!(e.kind, ExprKind::Var(v) if v == var))
                    }
                    ExprKind::Var(_) => None,
                    _ if non_var && depth == 0 => grouped.iter().find(|(_, e)| e.same(arg)),
                    _ => None,
                };
                let Some(&(target, _)) = found else {
                    return Err(grouping_error(
                        "arguments to GROUPING must be grouping expressions of the associated query level".into(),
                        arg.place(),
                    ));
                };
                refs.push(target);
            }
            g.refs = refs;
            return Ok(());
        }
        ExprKind::Grouping(_) => return Ok(()),
        ExprKind::Agg(agg) if agg.levels_up >= depth => return Ok(()),
        _ => {}
    }
    for child in expr.children_mut() {
        finalize_grouping(child, depth, grouped, non_var)?;
    }
    if let ExprKind::SubLink(sub) = &mut expr.kind {
        let mut result = Ok(());
        sub.query.each_expr_mut(depth + 1, &mut |e, depth| {
            if result.is_ok() {
                result = finalize_grouping(e, depth, grouped, non_var);
            }
        });
        result?;
    }
    Ok(())
}

/// The state of `substitute_grouped_columns`.
struct Check<'a, 'b> {
    analyzer: &'a Analyzer<'b>,
    /// The expressions of `GROUP BY`.
    grouped: &'a [&'a Expr],
    /// `groupClauseSubLevels`: the expressions of `GROUP BY` as a subquery sees them, with the index 0 for a subquery of the query.
    raised: Vec<Vec<Expr>>,
    /// `have_non_var_grouping`: true when an item of `GROUP BY` is not a column.
    non_var: bool,
    /// The items of `GROUP BY` that are columns.
    vars: &'a [Var],
    /// `groupClauseCommonVars`: the items of `GROUP BY` that are columns and are in all the grouping sets. Only these can prove a primary key.
    common: &'a [Var],
    /// `func_grouped_rels`: the relations whose primary key is in `GROUP BY`.
    proven: Vec<usize>,
}

impl Check<'_, '_> {
    /// Checks an expression of the query, or of a subquery `depth` levels down.
    fn expr(&mut self, expr: &Expr, depth: usize) -> Result<()> {
        // The direct arguments of an ordered-set aggregate are outside of the aggregate. An aggregate of this query in a subquery is `depth` levels up.
        if let ExprKind::Agg(agg) = &expr.kind
            && agg.levels_up == depth
        {
            return agg.direct.iter().try_for_each(|e| self.expr(e, depth));
        }
        // `finalize_grouping` checks the arguments of `GROUPING`.
        if let ExprKind::Grouping(g) = &expr.kind
            && g.levels_up >= depth
        {
            return Ok(());
        }
        if self.non_var && self.is_grouped(expr, depth) {
            return Ok(());
        }
        match &expr.kind {
            ExprKind::Const(_) | ExprKind::Param(_) => Ok(()),
            ExprKind::Var(var) if var.levels_up == depth => {
                self.var(Var { levels_up: 0, ..*var }, depth, expr.location)
            }
            ExprKind::Var(_) => Ok(()),
            _ => {
                for child in expr.children() {
                    self.expr(child, depth)?;
                }
                if let ExprKind::SubLink(sub) = &expr.kind {
                    for (e, depth) in sub.query.all_exprs(depth + 1) {
                        self.expr(e, depth)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// True when the expression is an item of `GROUP BY`, as a subquery `depth` levels down sees it.
    fn is_grouped(&mut self, expr: &Expr, depth: usize) -> bool {
        if depth == 0 {
            return self.grouped.iter().any(|g| g.same(expr));
        }
        while self.raised.len() < depth {
            let levels = self.raised.len() + 1;
            let raised = self
                .grouped
                .iter()
                .map(|g| {
                    let mut g = (*g).clone();
                    g.raise(levels);
                    g
                })
                .collect();
            self.raised.push(raised);
        }
        self.raised[depth - 1].iter().any(|g| g.same(expr))
    }

    fn var(&mut self, var: Var, depth: usize, at: Option<usize>) -> Result<()> {
        if !self.non_var && self.vars.contains(&var) {
            return Ok(());
        }
        if self.proven.contains(&var.relation) {
            return Ok(());
        }
        // check_functional_grouping: all the columns of the primary key are in GROUP BY.
        let relid = self.analyzer.scope.relations[var.relation].oid;
        if let Some(key) = builtin::primary_key(relid)
            && key.iter().all(|&attnum| self.common.contains(&Var { attnum, ..var }))
        {
            self.proven.push(var.relation);
            return Ok(());
        }
        let (table, column) = self.analyzer.column_names(var);
        let message = if depth == 0 {
            format!(
                "column \"{table}.{column}\" must appear in the GROUP BY clause or be used in an aggregate function"
            )
        } else {
            format!("subquery uses ungrouped column \"{table}.{column}\" from outer query")
        };
        Err(grouping_error(message, at))
    }
}

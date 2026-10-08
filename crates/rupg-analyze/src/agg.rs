//! Aggregate calls, as `transformAggregateCall` and `parseCheckAggregates` of `parse_agg.c` make and check them.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::List;

use crate::Analyzer;
use crate::coerce::AtOpt;
use crate::expr::{Aggref, Expr, ExprKind, Var};
use crate::select::Target;
use crate::sort::SortGroup;

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
    /// True when the call has `OVER`.
    pub(crate) over: bool,
    /// True when the call has `RESPECT NULLS` or `IGNORE NULLS`.
    pub(crate) null_treatment: bool,
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

    /// `transformAggregateCall` of a normal aggregate: the items of `ORDER BY` and `DISTINCT`, then the checks of `check_agglevels_and_constraints`.
    pub(crate) fn aggregate_call(
        &mut self,
        oid: u32,
        args: Vec<Expr>,
        parts: Parts<'_>,
        variadic: bool,
        ty: u32,
        at: Option<usize>,
    ) -> Result<Expr> {
        let nargs = args.len();
        let mut targets: Vec<Target> = args
            .into_iter()
            .map(|expr| Target { name: String::new(), expr, origin: None, junk: false })
            .collect();
        let order =
            self.with_kind(Kind::OrderBy, |a| a.sort_clause_sql99(parts.order, &mut targets))?;
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
        // check_agg_arguments: an aggregate call in the arguments is of the same level.
        if let Some(inner) = args.iter().chain(filter.as_deref()).find_map(Expr::first_agg) {
            return Err(grouping_error(
                "aggregate function calls cannot be nested".into(),
                inner.location,
            ));
        }
        match self.kind {
            Kind::JoinOn => {
                return Err(grouping_error(
                    "aggregate functions are not allowed in JOIN conditions".into(),
                    at,
                ));
            }
            Kind::Where | Kind::Filter | Kind::GroupBy | Kind::Limit | Kind::Offset => {
                return Err(grouping_error(
                    format!("aggregate functions are not allowed in {}", self.kind.name()),
                    at,
                ));
            }
            _ => {}
        }
        self.has_aggs = true;
        let agg = Aggref { oid, args, nargs, order, distinct, star: parts.star, variadic, filter };
        Ok(Expr { kind: ExprKind::Agg(Box::new(agg)), ty, typmod: -1, location: at })
    }

    /// `parseCheckAggregates`: each column that the select list and `HAVING` read outside of an aggregate call must be an item of `GROUP BY`, or a column of a table whose primary key is in `GROUP BY`.
    pub(crate) fn check_grouping(
        &self,
        targets: &[Target],
        having: Option<&Expr>,
        group: &[SortGroup],
    ) -> Result<()> {
        let grouped: Vec<&Expr> = group.iter().map(|g| &targets[g.target].expr).collect();
        let non_var = grouped.iter().any(|e| !matches!(e.kind, ExprKind::Var(_)));
        let vars: Vec<Var> = grouped
            .iter()
            .filter_map(|e| match e.kind {
                ExprKind::Var(var) => Some(var),
                _ => None,
            })
            .collect();
        let mut check =
            Check { analyzer: self, grouped: &grouped, non_var, vars: &vars, proven: Vec::new() };
        for target in targets {
            check.expr(&target.expr)?;
        }
        if let Some(having) = having {
            check.expr(having)?;
        }
        Ok(())
    }
}

/// The state of `substitute_grouped_columns`.
struct Check<'a, 'b> {
    analyzer: &'a Analyzer<'b>,
    /// The expressions of `GROUP BY`.
    grouped: &'a [&'a Expr],
    /// `have_non_var_grouping`: true when an item of `GROUP BY` is not a column.
    non_var: bool,
    /// The items of `GROUP BY` that are columns.
    vars: &'a [Var],
    /// `func_grouped_rels`: the relations whose primary key is in `GROUP BY`.
    proven: Vec<usize>,
}

impl Check<'_, '_> {
    fn expr(&mut self, expr: &Expr) -> Result<()> {
        if let ExprKind::Agg(_) = expr.kind {
            return Ok(());
        }
        if self.non_var && self.grouped.iter().any(|g| g.same(expr)) {
            return Ok(());
        }
        match &expr.kind {
            ExprKind::Const(_) | ExprKind::Param(_) => Ok(()),
            ExprKind::Var(var) => self.var(*var, expr.location),
            _ => {
                for child in expr.children() {
                    self.expr(child)?;
                }
                Ok(())
            }
        }
    }

    fn var(&mut self, var: Var, at: Option<usize>) -> Result<()> {
        if !self.non_var && self.vars.contains(&var) {
            return Ok(());
        }
        if self.proven.contains(&var.relation) {
            return Ok(());
        }
        // check_functional_grouping: all the columns of the primary key are in GROUP BY.
        let relid = self.analyzer.scope.relations[var.relation].oid;
        if let Some(key) = builtin::primary_key(relid)
            && key.iter().all(|&attnum| self.vars.contains(&Var { relation: var.relation, attnum }))
        {
            self.proven.push(var.relation);
            return Ok(());
        }
        let (table, column) = self.analyzer.column_names(var);
        Err(grouping_error(
            format!(
                "column \"{table}.{column}\" must appear in the GROUP BY clause or be used in an aggregate function"
            ),
            at,
        ))
    }
}

//! Window functions, as `ParseFuncOrColumn` and `transformWindowFuncCall` make a call and `transformWindowDefinitions` makes the windows of a query.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::ProcRow;
use rupg_sql::nodes::{Equal, FRAMEOPTION_DEFAULTS, WindowDef};

use crate::Analyzer;
use crate::agg::{Kind, Parts, SRF_HINT};
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, WindowFunc};
use crate::resolve::FUNC_MAX_ARGS;
use crate::select::Target;
use crate::sort::SortGroup;
use crate::typename::place;

/// A window of a query, as `WindowClause`.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowClause {
    /// The name of a window of the `WINDOW` clause, or `None` for a window that only an `OVER` clause gives.
    pub name: Option<String>,
    /// The name of the window that this window copies.
    pub refname: Option<String>,
    /// The items of `PARTITION BY`.
    pub partition: Vec<SortGroup>,
    /// The items of `ORDER BY`.
    pub order: Vec<SortGroup>,
    /// `copiedOrder`: true when the items of `ORDER BY` come from the window of `refname`.
    pub copied_order: bool,
}

/// An error with the SQLSTATE `42P20`.
fn windowing(message: String, at: Option<usize>) -> Error {
    Error::new(SqlState::WINDOWING_ERROR, message).at_opt(at)
}

/// The error of a feature of windows that the analyzer does not have yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

/// The error of a window that the query does not define.
fn no_window(name: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("window \"{name}\" does not exist")).at_opt(at)
}

impl Analyzer<'_> {
    /// The checks of `ParseFuncOrColumn` for a call with `OVER`, then `transformWindowFuncCall`: the place of the call and the number of its window. `aggregate` has the name of the function for an aggregate, which is not supported yet with `OVER`.
    pub(crate) fn window_call(
        &mut self,
        proc: &ProcRow,
        aggregate: Option<&str>,
        args: Vec<Expr>,
        parts: Parts<'_>,
        ty: u32,
        at: Option<usize>,
    ) -> Result<Expr> {
        let Some(over) = parts.over else {
            return Err(Error::internal("a window function call with no OVER"));
        };
        let unsupported =
            |message: &str| Error::new(SqlState::FEATURE_NOT_SUPPORTED, message).at_opt(at);
        if parts.distinct {
            return Err(unsupported("DISTINCT is not implemented for window functions"));
        }
        if let Some(name) = aggregate {
            if args.len() > FUNC_MAX_ARGS - 1 {
                return Err(Error::new(
                    SqlState::TOO_MANY_ARGUMENTS,
                    format!("aggregates cannot have more than {} arguments", FUNC_MAX_ARGS - 1),
                )
                .at_opt(at));
            }
            if args.is_empty() && !parts.star {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("{name}(*) must be used to call a parameterless aggregate function"),
                )
                .at_opt(at));
            }
        }
        if !parts.order.is_empty() {
            return Err(unsupported("aggregate ORDER BY is not implemented for window functions"));
        }
        if aggregate.is_none() && parts.filter.is_some() {
            return Err(unsupported(
                "FILTER is not implemented for non-aggregate window functions",
            ));
        }
        if self.srfs.0 != parts.srfs {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "window function calls cannot contain set-returning function calls",
            )
            .with_hint(SRF_HINT)
            .at_opt(self.srfs.1));
        }
        if proc.retset {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "window functions cannot return sets",
            )
            .at_opt(at));
        }
        if parts.null_treatment {
            return Err(not_yet("RESPECT NULLS and IGNORE NULLS", at));
        }
        if self.has_windows
            && let Some(inner) = args.iter().find_map(Expr::first_window)
        {
            return Err(windowing("window function calls cannot be nested".into(), inner.location));
        }
        let message = match self.kind {
            Kind::Select | Kind::OrderBy | Kind::DistinctOn => None,
            Kind::JoinOn => Some("window functions are not allowed in JOIN conditions".to_owned()),
            Kind::FromFunction => {
                Some("window functions are not allowed in functions in FROM".to_owned())
            }
            Kind::WindowPartition | Kind::WindowOrder => {
                Some("window functions are not allowed in window definitions".to_owned())
            }
            Kind::Check => Some("window functions are not allowed in check constraints".to_owned()),
            Kind::Where
            | Kind::Having
            | Kind::Filter
            | Kind::GroupBy
            | Kind::Limit
            | Kind::Offset
            | Kind::Values
            | Kind::ValuesSingle
            | Kind::ColumnDefault
            | Kind::IndexExpression
            | Kind::IndexPredicate
            | Kind::ExecuteParameter => {
                Some(format!("window functions are not allowed in {}", self.kind.name()))
            }
            Kind::Other => return Err(not_yet("a window function in this place", at)),
        };
        if let Some(message) = message {
            return Err(windowing(message, at));
        }
        let winref = if let Some(name) = &over.name {
            let found = self.windowdefs.iter().position(|d| d.name.as_ref() == Some(name));
            match found {
                Some(i) => i + 1,
                None => return Err(no_window(name, place(over.location))),
            }
        } else {
            // A window of an `OVER` clause that is the same as a window before it is that window.
            let same = |d: &WindowDef| {
                d.refname == over.refname
                    && d.partitionClause.equal(&over.partitionClause)
                    && d.orderClause.equal(&over.orderClause)
                    && d.frameOptions == over.frameOptions
                    && d.startOffset.equal(&over.startOffset)
                    && d.endOffset.equal(&over.endOffset)
            };
            match self.windowdefs.iter().position(same) {
                Some(i) => i + 1,
                None => {
                    self.windowdefs.push(over.clone());
                    self.windowdefs.len()
                }
            }
        };
        if aggregate.is_some() {
            return Err(not_yet("an aggregate function with OVER", at));
        }
        self.has_windows = true;
        let call = WindowFunc { oid: proc.oid, args, winref, star: parts.star };
        Ok(Expr { kind: ExprKind::Window(Box::new(call)), ty, typmod: -1, location: at })
    }

    /// `transformWindowDefinitions`: the windows of the query, in the order of their numbers. A window can copy a window before it.
    pub(crate) fn window_clauses(
        &mut self,
        targets: &mut Vec<Target>,
    ) -> Result<Vec<WindowClause>> {
        let defs = self.windowdefs.clone();
        let mut result: Vec<WindowClause> = Vec::with_capacity(defs.len());
        for def in &defs {
            let at = place(def.location);
            let named = |name: &str| result.iter().find(|w| w.name.as_deref() == Some(name));
            if let Some(name) = &def.name
                && named(name).is_some()
            {
                return Err(windowing(format!("window \"{name}\" is already defined"), at));
            }
            let base = match &def.refname {
                Some(refname) => {
                    Some(named(refname).ok_or_else(|| no_window(refname, at))?.clone())
                }
                None => None,
            };
            let order = self.window_order(&def.orderClause, targets)?;
            let partition = self.window_partition(&def.partitionClause, targets, &order)?;
            let mut window = WindowClause {
                name: def.name.as_deref().map(str::to_owned),
                refname: def.refname.as_deref().map(str::to_owned),
                partition,
                order,
                copied_order: false,
            };
            if let Some(base) = base {
                let refname = &base.name.unwrap_or_default();
                if !window.partition.is_empty() {
                    return Err(windowing(
                        format!("cannot override PARTITION BY clause of window \"{refname}\""),
                        at,
                    ));
                }
                window.partition = base.partition;
                if !window.order.is_empty() && !base.order.is_empty() {
                    return Err(windowing(
                        format!("cannot override ORDER BY clause of window \"{refname}\""),
                        at,
                    ));
                }
                if window.order.is_empty() {
                    window.order = base.order;
                    window.copied_order = true;
                }
            }
            if def.frameOptions != FRAMEOPTION_DEFAULTS {
                return Err(not_yet("a window frame clause", at));
            }
            result.push(window);
        }
        Ok(result)
    }
}

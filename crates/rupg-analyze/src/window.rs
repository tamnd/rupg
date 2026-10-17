//! Window functions, as `ParseFuncOrColumn` and `transformWindowFuncCall` make a call and `transformWindowDefinitions` makes the windows of a query.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_sql::nodes::{
    Equal, FRAMEOPTION_DEFAULTS, FRAMEOPTION_END_OFFSET_PRECEDING, FRAMEOPTION_EXCLUDE_CURRENT_ROW,
    FRAMEOPTION_EXCLUDE_GROUP, FRAMEOPTION_EXCLUDE_TIES, FRAMEOPTION_GROUPS, FRAMEOPTION_RANGE,
    FRAMEOPTION_ROWS, FRAMEOPTION_START_OFFSET_FOLLOWING, FRAMEOPTION_START_OFFSET_PRECEDING,
    FRAMEOPTION_START_UNBOUNDED_FOLLOWING, FRAMEOPTION_START_UNBOUNDED_PRECEDING, Node, WindowDef,
};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::{Kind, Parts, SRF_HINT};
use crate::coerce::{AtOpt, Context, can_coerce};
use crate::expr::{Expr, ExprKind, WindowFunc};
use crate::resolve::FUNC_MAX_ARGS;
use crate::select::Target;
use crate::sort::{SortGroup, invalid_reference};
use crate::typcache::ordering_properties;
use crate::typename::place;
use crate::types;

/// `FRAMEOPTION_END_UNBOUNDED_FOLLOWING`. The grammar makes the bits of the end of a frame with a shift of the bits of the start, so it does not name this bit.
pub const FRAMEOPTION_END_UNBOUNDED_FOLLOWING: i32 = FRAMEOPTION_START_UNBOUNDED_FOLLOWING << 1;
/// `FRAMEOPTION_END_OFFSET_FOLLOWING`, which the grammar does not name.
pub const FRAMEOPTION_END_OFFSET_FOLLOWING: i32 = FRAMEOPTION_START_OFFSET_FOLLOWING << 1;
/// `FRAMEOPTION_START_OFFSET`: the start of the frame has an offset.
pub const FRAMEOPTION_START_OFFSET: i32 =
    FRAMEOPTION_START_OFFSET_PRECEDING | FRAMEOPTION_START_OFFSET_FOLLOWING;
/// `FRAMEOPTION_END_OFFSET`: the end of the frame has an offset.
pub const FRAMEOPTION_END_OFFSET: i32 =
    FRAMEOPTION_END_OFFSET_PRECEDING | FRAMEOPTION_END_OFFSET_FOLLOWING;

/// `BTINRANGE_PROC`: the number of the `in_range` support function of a btree family.
const BTINRANGE_PROC: i16 = 3;

/// The frame of a window, as the fields `frameOptions`, `startOffset`, `endOffset`, `startInRangeFunc`, `endInRangeFunc`, `inRangeAsc` and `inRangeNullsFirst` of `WindowClause`.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// `frameOptions`: the bits `FRAMEOPTION_*` of the mode, the bounds and the exclusion.
    pub options: i32,
    /// The offset of the start, of type `bigint` in `ROWS` and `GROUPS` mode.
    pub start: Option<Expr>,
    /// The offset of the end, of type `bigint` in `ROWS` and `GROUPS` mode.
    pub end: Option<Expr>,
    /// `startInRangeFunc`: the `in_range` function of the start in `RANGE` mode with an offset, or 0.
    pub start_in_range: u32,
    /// `endInRangeFunc`: the `in_range` function of the end in `RANGE` mode with an offset, or 0.
    pub end_in_range: u32,
    /// `inRangeAsc`: true when the column of `ORDER BY` sorts in ascending order.
    pub in_range_asc: bool,
    /// `inRangeNullsFirst`: true when the nulls of the column of `ORDER BY` come first.
    pub in_range_nulls_first: bool,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            options: FRAMEOPTION_DEFAULTS,
            start: None,
            end: None,
            start_in_range: 0,
            end_in_range: 0,
            in_range_asc: false,
            in_range_nulls_first: false,
        }
    }
}

/// The mode of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameMode {
    Range,
    Rows,
    Groups,
}

/// The start or the end of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    /// An offset before the current row.
    Preceding,
    CurrentRow,
    /// An offset after the current row.
    Following,
    UnboundedFollowing,
}

/// The rows that the clause `EXCLUDE` removes from a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameExclusion {
    NoOthers,
    CurrentRow,
    /// The current row and its peers.
    Group,
    /// The peers of the current row, but not the current row.
    Ties,
}

impl Frame {
    /// The mode of the frame.
    pub fn mode(&self) -> FrameMode {
        if self.options & FRAMEOPTION_ROWS != 0 {
            FrameMode::Rows
        } else if self.options & FRAMEOPTION_GROUPS != 0 {
            FrameMode::Groups
        } else {
            FrameMode::Range
        }
    }

    /// The start of the frame.
    pub fn start_bound(&self) -> FrameBound {
        self.bound(0)
    }

    /// The end of the frame.
    pub fn end_bound(&self) -> FrameBound {
        self.bound(1)
    }

    /// The bound that the bits of the start give, with a shift of 1 for the end.
    fn bound(&self, shift: i32) -> FrameBound {
        let has = |bit: i32| self.options & (bit << shift) != 0;
        if has(FRAMEOPTION_START_UNBOUNDED_PRECEDING) {
            FrameBound::UnboundedPreceding
        } else if has(FRAMEOPTION_START_OFFSET_PRECEDING) {
            FrameBound::Preceding
        } else if has(FRAMEOPTION_START_OFFSET_FOLLOWING) {
            FrameBound::Following
        } else if has(FRAMEOPTION_START_UNBOUNDED_FOLLOWING) {
            FrameBound::UnboundedFollowing
        } else {
            FrameBound::CurrentRow
        }
    }

    /// The rows that the frame does not have.
    pub fn exclusion(&self) -> FrameExclusion {
        if self.options & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
            FrameExclusion::CurrentRow
        } else if self.options & FRAMEOPTION_EXCLUDE_GROUP != 0 {
            FrameExclusion::Group
        } else if self.options & FRAMEOPTION_EXCLUDE_TIES != 0 {
            FrameExclusion::Ties
        } else {
            FrameExclusion::NoOthers
        }
    }
}

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
    /// The frame of the window.
    pub frame: Frame,
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
    /// The checks of `ParseFuncOrColumn` for a call with `OVER`, then `transformWindowFuncCall`: the place of the call and the number of its window. `aggregate` has the name of the function for an aggregate.
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
            Kind::WindowPartition
            | Kind::WindowOrder
            | Kind::WindowRange
            | Kind::WindowRows
            | Kind::WindowGroups => {
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
        self.has_windows = true;
        let call = WindowFunc {
            oid: proc.oid,
            args,
            winref,
            star: parts.star,
            agg: aggregate.is_some(),
            filter: parts.filter.map(Box::new),
            nulls: parts.nulls,
        };
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
                frame: Frame::default(),
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
                if base.frame.options != FRAMEOPTION_DEFAULTS {
                    let error = windowing(
                        format!("cannot copy window \"{refname}\" because it has a frame clause"),
                        at,
                    );
                    // A bare `OVER (name)` can name the window with no parentheses.
                    let bare = def.name.is_none()
                        && def.orderClause.is_empty()
                        && def.frameOptions == FRAMEOPTION_DEFAULTS;
                    return Err(if bare {
                        error.with_hint("Omit the parentheses in this OVER clause.")
                    } else {
                        error
                    });
                }
            }
            let options = def.frameOptions;
            window.frame.options = options;
            // RANGE with an offset needs exactly one item of `ORDER BY`, and its btree family has the `in_range` functions.
            let mut family = (0, 0);
            if options & FRAMEOPTION_RANGE != 0
                && options & (FRAMEOPTION_START_OFFSET | FRAMEOPTION_END_OFFSET) != 0
            {
                let [item] = window.order.as_slice() else {
                    return Err(windowing(
                        "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
                            .into(),
                        at,
                    ));
                };
                let (opfamily, intype, descending) = ordering_properties(item.operator)
                    .ok_or_else(|| {
                        Error::internal(format!(
                            "operator {} is not a valid ordering operator",
                            item.operator
                        ))
                    })?;
                family = (opfamily, intype);
                window.frame.in_range_asc = !descending;
                window.frame.in_range_nulls_first = item.nulls_first;
            }
            if options & FRAMEOPTION_GROUPS != 0 && window.order.is_empty() {
                return Err(windowing("GROUPS mode requires an ORDER BY clause".into(), at));
            }
            (window.frame.start, window.frame.start_in_range) =
                self.frame_offset(options, family, def.startOffset.as_ref())?;
            (window.frame.end, window.frame.end_in_range) =
                self.frame_offset(options, family, def.endOffset.as_ref())?;
            result.push(window);
        }
        Ok(result)
    }

    /// `transformFrameOffset`: the offset of a bound of the frame, and the `in_range` function of the bound in `RANGE` mode. In `ROWS` and `GROUPS` mode the offset is a `bigint`. In `RANGE` mode the `in_range` functions of the family of the item of `ORDER BY` give the type of the offset. The offset cannot read a column of the query.
    fn frame_offset(
        &mut self,
        options: i32,
        (family, intype): (u32, u32),
        node: Option<&Node>,
    ) -> Result<(Option<Expr>, u32)> {
        let Some(node) = node else { return Ok((None, 0)) };
        let (construct, expr, func) = if options & FRAMEOPTION_ROWS != 0 {
            let expr = self.with_kind(Kind::WindowRows, |a| a.transform(Some(node)))?;
            ("ROWS", self.coerce_to_specific(expr, oid::INT8, "ROWS")?, 0)
        } else if options & FRAMEOPTION_RANGE != 0 {
            let expr = self.with_kind(Kind::WindowRange, |a| a.transform(Some(node)))?;
            let ty = expr.ty;
            // An offset of a type that is not known yet takes the type of the column first.
            let preferred = if ty == oid::UNKNOWN { intype } else { ty };
            let (mut funcs, mut matches) = (0, 0);
            let (mut selected, mut func) = (0, 0);
            for proc in builtin::amprocs(family, intype) {
                if proc.number != BTINRANGE_PROC {
                    continue;
                }
                funcs += 1;
                if !can_coerce(&[ty], &[proc.right], Context::Implicit) {
                    continue;
                }
                matches += 1;
                if selected != preferred {
                    (selected, func) = (proc.right, proc.proc);
                }
            }
            let unsupported = |message: String| {
                Error::new(SqlState::FEATURE_NOT_SUPPORTED, message).at_opt(expr.place())
            };
            let column = types::name(intype);
            if funcs == 0 {
                return Err(unsupported(format!(
                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {column}"
                )));
            }
            if matches == 0 {
                return Err(unsupported(format!(
                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {column} and offset type {}",
                    types::name(ty)
                ))
                .with_hint("Cast the offset value to an appropriate type."));
            }
            if matches != 1 && selected != preferred {
                return Err(unsupported(format!(
                    "RANGE with offset PRECEDING/FOLLOWING has multiple interpretations for column type {column} and offset type {}",
                    types::name(ty)
                ))
                .with_hint("Cast the offset value to the exact intended type."));
            }
            ("RANGE", self.coerce_to_specific(expr, selected, "RANGE")?, func)
        } else {
            let expr = self.with_kind(Kind::WindowGroups, |a| a.transform(Some(node)))?;
            ("GROUPS", self.coerce_to_specific(expr, oid::INT8, "GROUPS")?, 0)
        };
        if let Some(var) = expr.first_var() {
            return Err(invalid_reference(
                format!("argument of {construct} must not contain variables"),
                var.location,
            ));
        }
        Ok((Some(expr), func))
    }
}

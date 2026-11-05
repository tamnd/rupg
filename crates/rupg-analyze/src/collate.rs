//! `assign_collations_walker` of `parse_collate.c` for the columns of a query: the collation of each expression of the result.
//!
//! The analyzer does not keep a collation on each expression, so this module finds it again from the columns of the relations. A column and a constant have their own collation, a node with a result of a collatable type takes the collation of its inputs, and a node with no collatable input takes the collation of its type. A collation that is not the default wins over the default, and two different collations that are not the default give a conflict. An explicit collation from `COLLATE` wins over an implicit one.

use rupg_catalog::Catalog;
use rupg_common::{Error, Result, SqlState};

use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, SubLinkKind};
use crate::from::Relation;
use crate::select::Query;
use crate::types;

/// The OID of the collation `default`.
pub(crate) const DEFAULT_COLLATION: u32 = 100;

/// The collation state of an expression, as `assign_collations_context`. Each state has the place of the expression that gave the collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The expression has no collation, because its type is not collatable.
    None,
    /// An implicit collation.
    Implicit(u32, Option<usize>),
    /// Two different implicit collations that are not the default: the first one, the second one and the place of the second one.
    Conflict(u32, u32, Option<usize>),
    /// An explicit collation, from `COLLATE`.
    Explicit(u32, Option<usize>),
}

impl State {
    /// `CollateStrength`.
    fn strength(self) -> u8 {
        match self {
            State::None => 0,
            State::Implicit(..) => 1,
            State::Conflict(..) => 2,
            State::Explicit(..) => 3,
        }
    }

    /// `merge_collation_state`: the state of a node after the state of one more child. Two different explicit collations give an error.
    pub(crate) fn merge(self, other: State, catalog: Option<&Catalog>) -> Result<State> {
        if other.strength() > self.strength() {
            return Ok(other);
        }
        Ok(match (self, other) {
            (State::Implicit(a, _), State::Implicit(b, at)) if a != b => {
                if a == DEFAULT_COLLATION {
                    other
                } else if b != DEFAULT_COLLATION {
                    State::Conflict(a, b, at)
                } else {
                    self
                }
            }
            (State::Explicit(a, _), State::Explicit(b, at)) if a != b => {
                return Err(Error::new(
                    SqlState::COLLATION_MISMATCH,
                    format!(
                        "collation mismatch between explicit collations \"{}\" and \"{}\"",
                        collation_name(a, catalog),
                        collation_name(b, catalog)
                    ),
                )
                .at_opt(at));
            }
            _ => self,
        })
    }

    /// The state of an expression after a coercion to the type `ty`: a literal of the type `unknown` gets the collation of the type.
    pub(crate) fn coerced(self, ty: u32, at: Option<usize>) -> State {
        match (self, type_collation(ty)) {
            (State::None, 0) => State::None,
            (State::None, collation) => State::Implicit(collation, at),
            _ => self,
        }
    }

    /// `exprCollation` after the walker: 0 for no collation and for a conflict.
    pub(crate) fn oid(self) -> u32 {
        match self {
            State::Implicit(oid, _) | State::Explicit(oid, _) => oid,
            State::None | State::Conflict(..) => 0,
        }
    }

    /// The error of `select_common_collation` and of a target of `ORDER BY` or `GROUP BY` for a conflict of implicit collations.
    pub(crate) fn check(self, catalog: Option<&Catalog>) -> Result<State> {
        match self {
            State::Conflict(a, b, at) => Err(Error::new(
                SqlState::COLLATION_MISMATCH,
                format!(
                    "collation mismatch between implicit collations \"{}\" and \"{}\"",
                    collation_name(a, catalog),
                    collation_name(b, catalog)
                ),
            )
            .with_hint(
                "You can choose the collation by applying the COLLATE clause to one or both expressions.",
            )
            .at_opt(at)),
            _ => Ok(self),
        }
    }
}

/// `get_collation_name`.
pub(crate) fn collation_name(collation: u32, catalog: Option<&Catalog>) -> String {
    match rupg_pgcatalog::builtin::named_by_oid(
        rupg_pgcatalog::builtin::Named::Collation,
        collation,
    ) {
        Some(row) => row.name.to_string(),
        None => catalog
            .and_then(|c| c.collation(collation))
            .map_or_else(String::new, |c| c.name.clone()),
    }
}

/// The collation of an expression of the result of `query`, or 0 for a type that is not collatable and for a conflict. `catalog` gives the collations of the columns of the user tables.
pub(crate) fn target_collation(expr: &Expr, query: &Query, catalog: Option<&Catalog>) -> u32 {
    // A column of a set operation has the collation of the operation.
    if let (Some(op), ExprKind::Var(var)) = (&query.set_op, &expr.kind)
        && var.levels_up == 0
        && var.relation == op.left.leftmost()
        && let Some(&collation) =
            usize::try_from(var.attnum - 1).ok().and_then(|i| op.collations.get(i))
    {
        return collation;
    }
    quiet(expr, &[&query.relations], catalog)
}

/// `exprCollation` of an expression that the analyzer reads now. `levels` holds the relations of the query of the expression first, then those of the queries outside it.
pub(crate) fn expr_collation(
    expr: &Expr,
    levels: &[&[Relation]],
    catalog: Option<&Catalog>,
) -> u32 {
    quiet(expr, levels, catalog)
}

/// The collation of an expression whose errors the analysis gave before.
fn quiet(expr: &Expr, levels: &[&[Relation]], catalog: Option<&Catalog>) -> u32 {
    state(expr, levels, catalog).map_or(0, State::oid)
}

/// `get_typcollation`.
fn type_collation(ty: u32) -> u32 {
    types::row(ty).map_or(0, |t| t.collation)
}

/// `assign_collations_walker`: the collation state of an expression. `levels` holds the relations of the query of the expression first, then those of the queries outside it. Two different explicit collations of the inputs of a node give an error, and a conflict of implicit collations in the items of `ORDER BY` of an aggregate call gives an error.
pub(crate) fn state(
    expr: &Expr,
    levels: &[&[Relation]],
    catalog: Option<&Catalog>,
) -> Result<State> {
    let own = type_collation(expr.ty);
    let at = expr.place();
    // The merged state of the inputs of the node.
    let mut merged = State::None;
    let merge = |e: &Expr, merged: &mut State| -> Result<()> {
        *merged = merged.merge(state(e, levels, catalog)?, catalog)?;
        Ok(())
    };
    // `assign_expr_collations`: an expression with its own state, which only gives its errors.
    let alone = |e: &Expr| state(e, levels, catalog).map(|_| ());
    match &expr.kind {
        ExprKind::Collate(arg, collation) => {
            alone(arg)?;
            return Ok(State::Explicit(*collation, expr.location));
        }
        ExprKind::Var(var) => {
            return Ok(match column_collation(*var, levels, catalog) {
                0 => State::None,
                oid => State::Implicit(oid, at),
            });
        }
        ExprKind::SubLink(sub) => {
            if let Some(test) = &sub.test {
                alone(test)?;
            }
            let column = match sub.kind {
                SubLinkKind::Expr | SubLinkKind::Array => 0,
                SubLinkKind::Multi { column, .. } => column,
                _ => return Ok(State::None),
            };
            if own == 0 {
                return Ok(State::None);
            }
            let mut inner = vec![&sub.query.relations[..]];
            inner.extend_from_slice(levels);
            let target = sub.query.targets.iter().filter(|t| !t.junk).nth(column);
            let oid = target.map_or(0, |t| quiet(&t.expr, &inner, catalog));
            return Ok(State::Implicit(oid, target.and_then(|t| t.expr.place())));
        }
        // `RowExpr`: the fields are independent, and the row has no collation.
        ExprKind::Row(fields, _) => {
            for field in fields {
                alone(field)?;
            }
            return Ok(State::None);
        }
        ExprKind::RowCompare(rc) => {
            for op in &rc.ops {
                alone(op)?;
            }
            return Ok(State::None);
        }
        ExprKind::Case(case) => {
            if let Some(arg) = &case.arg {
                alone(arg)?;
            }
            for (when, then) in &case.whens {
                merge(when, &mut merged)?;
                merge(then, &mut merged)?;
            }
            merge(&case.default, &mut merged)?;
        }
        // `assign_aggregate_collations`: the expressions that only `ORDER BY` reads have their own state. An item of `ORDER BY` or `DISTINCT` with a conflict of implicit collations gives an error.
        ExprKind::Agg(agg) if agg.kind == b'n' => {
            for (i, arg) in agg.args.iter().enumerate() {
                let sorted = agg.order.iter().chain(&agg.distinct).any(|s| s.target == i);
                let mut one = state(arg, levels, catalog)?;
                if sorted {
                    one = one.check(catalog)?;
                }
                if i < agg.nargs {
                    merged = merged.merge(one, catalog)?;
                }
            }
            if let Some(filter) = &agg.filter {
                alone(filter)?;
            }
        }
        // `assign_ordered_set_collations`: the items of `WITHIN GROUP` count only when there can be one item.
        ExprKind::Agg(agg) => {
            let variadic = rupg_pgcatalog::builtin::proc_by_oid(agg.oid).map_or(0, |p| p.variadic);
            let one = agg.args.len() == 1 && variadic == 0;
            for e in &agg.direct {
                merge(e, &mut merged)?;
            }
            for e in agg.args.iter().take(agg.nargs) {
                if one {
                    merge(e, &mut merged)?;
                } else {
                    alone(e)?;
                }
            }
            if let Some(filter) = &agg.filter {
                alone(filter)?;
            }
        }
        ExprKind::Window(w) => {
            for e in &w.args {
                merge(e, &mut merged)?;
            }
            if let Some(filter) = &w.filter {
                alone(filter)?;
            }
        }
        _ => {
            for e in expr.children() {
                merge(e, &mut merged)?;
            }
        }
    }
    if own == 0 {
        return Ok(State::None);
    }
    // A domain with a collation that is not the default gives its own collation. Any other domain takes the collation of its input.
    if matches!(expr.kind, ExprKind::CoerceToDomain(..)) && own != DEFAULT_COLLATION {
        return Ok(State::Implicit(own, at));
    }
    Ok(if merged == State::None { State::Implicit(own, at) } else { merged })
}

/// The collation of a column of a relation of a query in `levels`.
fn column_collation(
    var: crate::expr::Var,
    levels: &[&[Relation]],
    catalog: Option<&Catalog>,
) -> u32 {
    let Some(relations) = levels.get(var.levels_up) else { return 0 };
    let Some(relation) = relations.get(var.relation) else { return 0 };
    let Ok(index) = usize::try_from(var.attnum - 1) else { return 0 };
    if relation.oid != 0 {
        if let Some(rel) = catalog.and_then(|c| c.relation(relation.oid)) {
            return rel.column(var.attnum).map_or(0, |c| c.collation);
        }
        return rupg_pgcatalog::catalog_by_oid(relation.oid)
            .and_then(|c| c.columns.get(index))
            .map_or(0, |c| c.collation);
    }
    let outer = &levels[var.levels_up..];
    if let Some(sub) = &relation.subquery {
        if let Some(op) = &sub.set_op {
            return op.collations.get(index).copied().unwrap_or(0);
        }
        let mut inner = vec![&sub.relations[..]];
        inner.extend_from_slice(outer);
        let target = sub.targets.iter().filter(|t| !t.junk).nth(index);
        return target.map_or(0, |t| quiet(&t.expr, &inner, catalog));
    }
    if let Some(rows) = &relation.values {
        let mut merged = State::None;
        for e in rows.iter().filter_map(|row| row.get(index)) {
            let one = state(e, outer, catalog).unwrap_or(State::None);
            merged = merged.merge(one, catalog).unwrap_or(merged);
        }
        return merged.oid();
    }
    relation.columns.get(index).map_or(0, |c| type_collation(c.ty))
}

/// `UNICODE_COLLATION_OID`: the collation `unicode` of ICU with the root locale.
const UNICODE_COLLATION: u32 = 963;

/// True for a collation of the ICU provider, whose order is not the order of the bytes.
pub(crate) fn icu(collation: u32, catalog: Option<&Catalog>) -> bool {
    collation == UNICODE_COLLATION
        || catalog.and_then(|c| c.collation(collation)).is_some_and(|c| c.provider == b'i')
}

/// The way a built-in function reads the collation of its inputs, as `PG_GET_COLLATION`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// The function does not read the collation, or reads it only for a collation that is not deterministic.
    None,
    /// The function orders two strings with `varstr_cmp`, which the kernels of `rupg-func` do in a collation of ICU.
    Order,
    /// The function reads the collation in a way that rupg does not do yet for ICU: the case of the letters, or the classes of the characters of a regular expression.
    Unsupported,
}

/// [`Use`] of the function `func`.
pub(crate) fn collation_use(func: u32) -> Use {
    let Some(proc) = rupg_pgcatalog::builtin::proc_by_oid(func) else { return Use::None };
    let src = proc.src;
    let order = matches!(
        src,
        "text_lt"
            | "text_le"
            | "text_gt"
            | "text_ge"
            | "bttextcmp"
            | "text_larger"
            | "text_smaller"
            | "bpcharlt"
            | "bpcharle"
            | "bpchargt"
            | "bpcharge"
            | "bpcharcmp"
            | "bpchar_larger"
            | "bpchar_smaller"
            | "namelt"
            | "namele"
            | "namegt"
            | "namege"
            | "btnamecmp"
            | "namelttext"
            | "nameletext"
            | "namegttext"
            | "namegetext"
            | "textltname"
            | "textlename"
            | "textgtname"
            | "textgename"
            | "btnametextcmp"
            | "bttextnamecmp"
            | "array_lt"
            | "array_le"
            | "array_gt"
            | "array_ge"
            | "btarraycmp"
            | "array_larger"
            | "array_smaller"
            | "array_sort"
            | "array_sort_order"
            | "array_sort_order_nulls_first"
    );
    if order {
        return Use::Order;
    }
    let unsupported = src.contains("regex")
        || src.contains("iclike")
        || src.contains("icnlike")
        || matches!(src, "upper" | "lower" | "initcap" | "casefold");
    if unsupported { Use::Unsupported } else { Use::None }
}

/// The collation of the inputs of a call, as `inputcollid`, when it is a collation of ICU, or 0. A conflict of implicit collations gives 0.
pub(crate) fn icu_input(args: &[Expr], levels: &[&[Relation]], catalog: Option<&Catalog>) -> u32 {
    if !args.iter().any(|a| types::collatable(a.ty)) {
        return 0;
    }
    let mut merged = State::None;
    for arg in args {
        let Ok(one) = state(arg, levels, catalog) else { return 0 };
        let Ok(next) = merged.merge(one, catalog) else { return 0 };
        merged = next;
    }
    let oid = merged.oid();
    if icu(oid, catalog) { oid } else { 0 }
}

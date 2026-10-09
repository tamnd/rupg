//! `assign_collations_walker` of `parse_collate.c` for the columns of a query: the collation of each expression of the result.
//!
//! The analyzer does not keep a collation on each expression, so this module finds it again from the columns of the relations. A column and a constant have their own collation, a node with a result of a collatable type takes the collation of its inputs, and a node with no collatable input takes the collation of its type. A collation that is not the default wins over the default, and two different collations that are not the default give a conflict. The analyzer drops `COLLATE`, so an explicit collation does not count.

use rupg_catalog::Catalog;

use crate::expr::{Expr, ExprKind, SubLinkKind};
use crate::select::Query;
use crate::types;

/// The OID of the collation `default`.
pub(crate) const DEFAULT_COLLATION: u32 = 100;

/// The collation state of an expression, as `assign_collations_context` without the explicit strength.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// The expression has no collation, because its type is not collatable.
    None,
    /// An implicit collation.
    Implicit(u32),
    /// Two different implicit collations that are not the default.
    Conflict,
}

impl State {
    /// `merge_collation_state` for two implicit states.
    fn merge(self, other: State) -> State {
        match (self, other) {
            (State::Conflict, _) | (_, State::Conflict) => State::Conflict,
            (State::None, s) | (s, State::None) => s,
            (State::Implicit(a), State::Implicit(b)) if a == b => self,
            (State::Implicit(DEFAULT_COLLATION), s) | (s, State::Implicit(DEFAULT_COLLATION)) => s,
            _ => State::Conflict,
        }
    }

    /// `exprCollation` after the walker: 0 for no collation and for a conflict.
    fn oid(self) -> u32 {
        match self {
            State::Implicit(oid) => oid,
            State::None | State::Conflict => 0,
        }
    }
}

/// The collation of an expression of the result of `query`, or 0 for a type that is not collatable and for a conflict. `catalog` gives the collations of the columns of the user tables.
pub(crate) fn target_collation(expr: &Expr, query: &Query, catalog: Option<&Catalog>) -> u32 {
    state(expr, &[query], catalog).oid()
}

/// `get_typcollation`.
fn type_collation(ty: u32) -> u32 {
    types::row(ty).map_or(0, |t| t.collation)
}

/// The collation state of an expression. `levels` holds the query of the expression first, then the queries outside it.
fn state(expr: &Expr, levels: &[&Query], catalog: Option<&Catalog>) -> State {
    let own = type_collation(expr.ty);
    if own == 0 {
        return State::None;
    }
    let inputs: Vec<&Expr> = match &expr.kind {
        ExprKind::Var(var) => {
            return match column_collation(*var, levels, catalog) {
                0 => State::None,
                oid => State::Implicit(oid),
            };
        }
        ExprKind::SubLink(sub) => {
            if !matches!(sub.kind, SubLinkKind::Expr | SubLinkKind::Array) {
                return State::None;
            }
            let mut inner = vec![&sub.query];
            inner.extend_from_slice(levels);
            let first = sub.query.targets.iter().find(|t| !t.junk);
            return State::Implicit(first.map_or(0, |t| state(&t.expr, &inner, catalog).oid()));
        }
        ExprKind::Case(case) => {
            case.whens.iter().map(|(_, then)| then).chain([&*case.default]).collect()
        }
        ExprKind::Agg(agg) => agg.args.iter().take(agg.nargs).collect(),
        _ => expr.children(),
    };
    let merged =
        inputs.into_iter().fold(State::None, |acc, e| acc.merge(state(e, levels, catalog)));
    if merged == State::None { State::Implicit(own) } else { merged }
}

/// The collation of a column of a relation of a query in `levels`.
fn column_collation(var: crate::expr::Var, levels: &[&Query], catalog: Option<&Catalog>) -> u32 {
    let Some(query) = levels.get(var.levels_up) else { return 0 };
    let Some(relation) = query.relations.get(var.relation) else { return 0 };
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
        let mut inner = vec![&**sub];
        inner.extend_from_slice(outer);
        let target = sub.targets.iter().filter(|t| !t.junk).nth(index);
        return target.map_or(0, |t| state(&t.expr, &inner, catalog).oid());
    }
    if let Some(rows) = &relation.values {
        let merged = rows
            .iter()
            .filter_map(|row| row.get(index))
            .fold(State::None, |acc, e| acc.merge(state(e, outer, catalog)));
        return merged.oid();
    }
    relation.columns.get(index).map_or(0, |c| type_collation(c.ty))
}

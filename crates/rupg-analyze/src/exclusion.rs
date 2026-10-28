//! The exclusion constraints of a table, which `check_exclusion_or_unique_constraint` checks for each new version of a row.

use rupg_catalog::{Catalog, ConKind};
use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin;

use crate::expr::{BoolOp, Expr, ExprKind, Var};
use crate::insert::{Deferrable, Derived};

/// An exclusion constraint: two live versions conflict when the operator of each key column gives true for their keys.
#[derive(Clone, Debug, PartialEq)]
pub struct Exclusion {
    /// The OID of the constraint.
    pub constraint: u32,
    /// The name of the constraint, for the error.
    pub name: String,
    /// The OID of the index of the constraint.
    pub index: u32,
    /// The expression of each key column and the predicate of the index.
    pub keys: Derived,
    /// `conexclop`: the operator of each key column.
    pub operators: Vec<u32>,
    /// `oprcode` of each operator.
    pub functions: Vec<u32>,
    /// The constraint of an index that is not `indimmediate`, or `None` for a constraint that checks each version at once.
    pub deferrable: Option<Deferrable>,
}

/// The exclusion constraints of the table `table` in the order of their OIDs. An index that is not ready checks no row.
///
/// # Errors
///
/// An index key that is not a column or an expression.
pub fn exclusions(catalog: &Catalog, table: u32) -> Result<Vec<Exclusion>> {
    let mut constraints: Vec<_> = catalog
        .constraints_of(table)
        .filter(|c| c.kind == ConKind::Exclusion && c.relation == table)
        .collect();
    if constraints.is_empty() {
        return Ok(Vec::new());
    }
    constraints.sort_by_key(|c| c.oid);
    let columns = catalog.relation(table).map(|r| r.columns.as_slice()).unwrap_or_default();
    let mut list = Vec::with_capacity(constraints.len());
    for con in constraints {
        let Some(info) = catalog.relation(con.index).and_then(|r| r.index.as_ref()) else {
            continue;
        };
        if !info.ready {
            continue;
        }
        let mut exprs = match &info.exprs {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        }
        .into_iter();
        let count = usize::try_from(info.key_count).unwrap_or_default();
        let mut keys = Derived {
            table,
            types: columns.iter().map(|c| c.ty).collect(),
            exprs: Vec::with_capacity(count),
            predicate: match &info.predicate {
                Some(text) => match crate::node::read(text)? {
                    Expr { kind: ExprKind::Bool(BoolOp::And, parts), .. } => parts,
                    part => vec![part],
                },
                None => Vec::new(),
            },
        };
        for &attnum in info.keys.iter().take(count) {
            let expr = if attnum == 0 {
                exprs.next().ok_or_else(|| Error::internal("too few entries in indexprs"))?
            } else {
                let column = usize::try_from(attnum - 1).ok().and_then(|c| columns.get(c));
                let Some(column) = column else {
                    return Err(Error::internal("an index key that is not a column"));
                };
                let var = Var { relation: 0, attnum, levels_up: 0 };
                Expr { typmod: column.typmod, ..Expr::new(ExprKind::Var(var), column.ty) }
            };
            keys.exprs.push(expr);
        }
        let functions = con
            .exclusion_ops
            .iter()
            .map(|&op| match builtin::operator_by_oid(op) {
                Some(row) => row.code,
                None => catalog.operator(op).map_or(0, |o| o.code),
            })
            .collect();
        let deferrable =
            (!info.immediate).then_some(Deferrable { constraint: con.oid, deferred: con.deferred });
        list.push(Exclusion {
            constraint: con.oid,
            name: con.name.clone(),
            index: con.index,
            keys,
            operators: con.exclusion_ops.clone(),
            functions,
            deferrable,
        });
    }
    Ok(list)
}

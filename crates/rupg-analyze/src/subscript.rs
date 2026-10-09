//! `transformIndirection` and `transformContainerSubscripts` of `parse_expr.c` and `parse_node.c`, with `array_subscript_transform` of `arraysubs.c`: the subscripts of an array, such as `a[1]` and `a[2:3]`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{A_Indices, A_Indirection, Node};
use rupg_types::{MAXDIM, Value, oid};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Subscript};
use crate::types;

/// `array_subscript_handler`, the `typsubscript` of a true array type.
const ARRAY_SUBSCRIPT_HANDLER: u32 = 6179;

impl Analyzer<'_> {
    /// `transformIndirection`: the subscripts after an expression. A field selection and `.*` are not supported yet.
    pub(crate) fn transform_indirection(&mut self, ind: &A_Indirection) -> Result<Expr> {
        let mut result = self.transform(ind.arg.as_ref())?;
        let mut subscripts: Vec<&A_Indices> = Vec::new();
        for item in &ind.indirection {
            match item {
                Some(Node::A_Indices(indices)) => subscripts.push(indices),
                Some(Node::A_Star(_)) => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "row expansion via \"*\" is not supported here",
                    )
                    .at_opt(result.place()));
                }
                _ => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "a field selection is not supported yet",
                    )
                    .at_opt(result.place()));
                }
            }
        }
        if !subscripts.is_empty() {
            result = self.container_subscripts(result, &subscripts)?;
        }
        Ok(result)
    }

    /// `transformContainerSubscripts` for a fetch. A domain over an array subscripts its base type, and `int2vector` and `oidvector` are `int2[]` and `oid[]`. The result is the element, or an array for a slice, with the typmod of the container.
    fn container_subscripts(&mut self, container: Expr, subscripts: &[&A_Indices]) -> Result<Expr> {
        let (ty, typmod) = container_type(container.ty, container.typmod);
        let row = types::row(ty);
        let element = match row {
            Some(row) if row.subscript == ARRAY_SUBSCRIPT_HANDLER && row.elem != 0 => row.elem,
            Some(row) if row.subscript != 0 => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("subscripts of type {} are not supported yet", types::name(ty)),
                )
                .at_opt(container.place()));
            }
            _ => {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "cannot subscript type {} because it does not support subscripting",
                        types::name(ty)
                    ),
                )
                .at_opt(container.place()));
            }
        };
        let slice = subscripts.iter().any(|s| s.is_slice);
        let mut upper = Vec::with_capacity(subscripts.len());
        let mut lower = Vec::with_capacity(if slice { subscripts.len() } else { 0 });
        for indices in subscripts {
            if slice {
                lower.push(match &indices.lidx {
                    Some(node) => Some(self.subscript_int4(node)?),
                    None if !indices.is_slice => {
                        Some(Expr::constant(Value::Int4(1), oid::INT4, -1, None))
                    }
                    None => None,
                });
            }
            upper.push(match &indices.uidx {
                Some(node) => Some(self.subscript_int4(node)?),
                None => None,
            });
        }
        if upper.len() > MAXDIM {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                    upper.len()
                ),
            ));
        }
        let result = if slice { ty } else { element };
        let kind = ExprKind::Subscript(Box::new(Subscript {
            container,
            upper,
            lower: slice.then_some(lower),
        }));
        Ok(Expr { kind, ty: result, typmod, location: None })
    }

    /// A subscript, cast to `int4` as an assignment cast.
    fn subscript_int4(&mut self, node: &Node) -> Result<Expr> {
        let expr = self.transform(Some(node))?;
        let at = expr.place();
        self.coerce_to_target(expr, oid::INT4, -1, Context::Assignment, None)?.ok_or_else(|| {
            Error::new(SqlState::DATATYPE_MISMATCH, "array subscript must have type integer")
                .at_opt(at)
        })
    }
}

/// `transformContainerType`: the base type and the typmod of a domain, with `int2vector` as `int2[]` and `oidvector` as `oid[]`.
fn container_type(ty: u32, typmod: i32) -> (u32, i32) {
    let (mut ty, mut typmod) = (ty, typmod);
    while let Some(row) = types::row(ty)
        && row.kind == b'd'
        && row.base != 0
    {
        typmod = row.typmod;
        ty = row.base;
    }
    match ty {
        oid::INT2VECTOR => (oid::INT2_ARRAY, typmod),
        oid::OIDVECTOR => (oid::OID_ARRAY, typmod),
        _ => (ty, typmod),
    }
}

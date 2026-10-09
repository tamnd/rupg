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
    /// `transformIndirection`: the subscripts and the field selections after an expression. The subscripts before a field apply first. `.*` is not supported here.
    pub(crate) fn transform_indirection(&mut self, ind: &A_Indirection) -> Result<Expr> {
        let mut result = self.transform(ind.arg.as_ref())?;
        let location = result.place();
        let mut subscripts: Vec<&A_Indices> = Vec::new();
        for item in &ind.indirection {
            match item {
                Some(Node::A_Indices(indices)) => subscripts.push(indices),
                Some(Node::A_Star(_)) => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "row expansion via \"*\" is not supported here",
                    )
                    .at_opt(location));
                }
                Some(Node::String(field)) => {
                    if !subscripts.is_empty() {
                        result = self.container_subscripts(result, &subscripts)?;
                        subscripts.clear();
                    }
                    result = self.field_select(result, field, location)?;
                }
                _ => return Err(Error::internal("unrecognized indirection")),
            }
        }
        if !subscripts.is_empty() {
            result = self.container_subscripts(result, &subscripts)?;
        }
        Ok(result)
    }

    /// `ParseComplexProjection` and `unknown_attribute`: the field of a row value with the name. A field of the whole row of a relation is the column of the relation. A name that is no field can be a function in the functional notation, which rupg does not have yet.
    fn field_select(&mut self, arg: Expr, name: &str, at: Option<usize>) -> Result<Expr> {
        if let ExprKind::Var(var) = arg.kind
            && var.attnum == 0
        {
            return self.whole_row_field(var, name, at);
        }
        let ty = arg.ty;
        let columns = if ty == oid::RECORD { None } else { self.row_columns(ty) };
        if let Some(columns) = &columns
            && let Some(i) = columns.iter().position(|c| c.name == name)
        {
            let mut expr = Expr::new(ExprKind::FieldSelect(Box::new(arg), i), columns[i].ty);
            expr.typmod = columns[i].typmod;
            return Ok(expr);
        }
        if rupg_pgcatalog::builtin::procs_named(name).any(|p| p.nargs == 1) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("the call of the function {name} as a field is not supported yet"),
            )
            .at_opt(at));
        }
        let (state, message) = match (columns, ty) {
            (Some(_), _) => (
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" not found in data type {}", types::name(ty)),
            ),
            (None, oid::RECORD) => (
                SqlState::UNDEFINED_COLUMN,
                format!("could not identify column \"{name}\" in record data type"),
            ),
            (None, _) => (
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "column notation .{name} applied to type {}, which is not a composite type",
                    types::name(ty)
                ),
            ),
        };
        Err(Error::new(state, message).at_opt(at))
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

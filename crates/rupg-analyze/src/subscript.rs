//! `transformIndirection` and `transformContainerSubscripts` of `parse_expr.c` and `parse_node.c`, with `array_subscript_transform` of `arraysubs.c`: the subscripts of an array, such as `a[1]` and `a[2:3]`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{A_Indices, A_Indirection, Node};
use rupg_types::{MAXDIM, Value, oid};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, FieldStore, Subscript};
use crate::types;

/// `array_subscript_handler`, the `typsubscript` of a true array type.
const ARRAY_SUBSCRIPT_HANDLER: u32 = 6179;
/// `raw_array_subscript_handler`, the `typsubscript` of a type of fixed length that is an array of its elements: `name`, `point`, `lseg`, `box` and `line`.
const RAW_ARRAY_SUBSCRIPT_HANDLER: u32 = 6180;

impl Analyzer<'_> {
    /// `transformIndirection`: the subscripts and the field selections after an expression. The subscripts before a field apply first. `.*` is not supported here.
    pub(crate) fn transform_indirection(&mut self, ind: &A_Indirection) -> Result<Expr> {
        self.indirection(ind.arg.as_ref(), &ind.indirection)
    }

    /// `ExpandIndirectionStar` and `ExpandRowReference`: the fields of `(expr).*` in a target list, with their names, or `None` when the indirection does not end with `*`. A whole-row reference gives the columns of its relation. Each other field is a field selection of its own copy of the expression, so the expression runs once for each field, as in PostgreSQL.
    ///
    /// # Errors
    ///
    /// `42809` for an expression of a type that is not composite, or of type `record` with fields that the analyzer cannot find.
    pub(crate) fn expand_indirection_star(
        &mut self,
        ind: &A_Indirection,
    ) -> Result<Option<Vec<(String, Expr)>>> {
        let Some((Some(Node::A_Star(_)), rest)) = ind.indirection.split_last() else {
            return Ok(None);
        };
        let expr = self.indirection(ind.arg.as_ref(), rest)?;
        let at = expr.place();
        if let ExprKind::Var(var) = expr.kind
            && var.attnum == 0
        {
            return self.whole_row_columns(var, at).map(Some);
        }
        if let ExprKind::JoinRow(row) = &expr.kind
            && row.alias.is_some()
        {
            return Ok(Some(row.names.iter().cloned().zip(row.fields.iter().cloned()).collect()));
        }
        self.composite_fields(expr).map(Some)
    }

    /// The fields of the composite value `expr`, each as a field selection of its own copy of `expr`.
    ///
    /// # Errors
    ///
    /// `42809` for an expression of a type that is not composite, or of type `record` with fields that the analyzer cannot find.
    pub(crate) fn composite_fields(&mut self, expr: Expr) -> Result<Vec<(String, Expr)>> {
        let ty = expr.ty;
        // `lookup_rowtype_tupdesc_domain`: a domain over a composite type has the fields of the type.
        let columns = if ty == oid::RECORD {
            self.record_columns(&expr)?
        } else {
            self.row_columns(types::base(ty))
        };
        let Some(columns) = columns else {
            let message = if ty == oid::RECORD {
                "record type has not been registered".to_string()
            } else {
                format!("type {} is not composite", types::name(ty))
            };
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message));
        };
        let fields = columns
            .into_iter()
            .enumerate()
            .map(|(i, column)| {
                let mut field =
                    Expr::new(ExprKind::FieldSelect(Box::new(expr.clone()), i), column.ty);
                field.typmod = column.typmod;
                (column.name, field)
            })
            .collect();
        Ok(fields)
    }

    /// The subscripts and the field selections of the items after the expression `arg`.
    fn indirection(&mut self, arg: Option<&Node>, items: &[Option<Node>]) -> Result<Expr> {
        let mut result = self.transform(arg)?;
        let location = result.place();
        let mut subscripts: Vec<&A_Indices> = Vec::new();
        for item in items {
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

    /// `ParseComplexProjection` and `unknown_attribute`: the field of a row value with the name. A field of the whole row of a relation is the column of the relation. A value of type `record` has the fields that [`crate::record_fields`] finds. A name that is no field can be a function in the functional notation, which rupg does not have yet.
    pub(crate) fn field_select(
        &mut self,
        arg: Expr,
        name: &str,
        at: Option<usize>,
    ) -> Result<Expr> {
        if let ExprKind::Var(var) = arg.kind
            && var.attnum == 0
        {
            return self.whole_row_field(var, name, at);
        }
        if let ExprKind::JoinRow(row) = &arg.kind
            && let Some(alias) = &row.alias
        {
            let Some(i) = row.names.iter().position(|n| n == name) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column {alias}.{name} does not exist"),
                )
                .at_opt(at));
            };
            return Ok(row.fields[i].clone().at(at));
        }
        let ty = arg.ty;
        let columns = if ty == oid::RECORD {
            self.record_columns(&arg)?
        } else {
            self.row_columns(types::base(ty))
        };
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
            (_, oid::RECORD) => (
                SqlState::UNDEFINED_COLUMN,
                format!("could not identify column \"{name}\" in record data type"),
            ),
            (Some(_), _) => (
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" not found in data type {}", types::name(ty)),
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

    /// `transformContainerSubscripts` for a fetch. A domain over an array subscripts its base type, and `int2vector` and `oidvector` are `int2[]` and `oid[]`. A `name` is an array of `"char"`, a `point` and a `line` are arrays of `float8`, and an `lseg` and a `box` are arrays of `point`. The result is the element, or an array for a slice, with the typmod of the container.
    fn container_subscripts(&mut self, container: Expr, subscripts: &[&A_Indices]) -> Result<Expr> {
        let (ty, typmod) = container_type(container.ty, container.typmod);
        let row = types::row(ty);
        let element = match row {
            Some(row) if row.subscript == ARRAY_SUBSCRIPT_HANDLER && row.elem != 0 => row.elem,
            Some(row) if row.subscript == RAW_ARRAY_SUBSCRIPT_HANDLER && row.elem != 0 => row.elem,
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
            assign: None,
        }));
        Ok(Expr { kind, ty: result, typmod, location: None })
    }

    /// `transformAssignmentIndirection`: the assignment of `rhs` to the part of `base` that the subscripts and the fields of `indirection` name, as an expression whose value is the whole value with the new part. `name` is the name of the target in the errors, and the type and the typmod are the type of `base`.
    pub(crate) fn assignment_indirection(
        &mut self,
        (base, name, target): (Expr, &str, (u32, i32)),
        indirection: &[Option<Node>],
        rhs: Expr,
        context: Context,
        at: Option<usize>,
    ) -> Result<Expr> {
        let assigning = Assigning { context, at };
        self.assign_indirect(Some(base), (name, false), target, indirection, rhs, &assigning)
    }

    /// `transformAssignmentIndirection` for the items of `indirection` after the first: with no `base`, `CaseTest` is the old value of the part. `subscripted` tells that the part is an element or a slice.
    fn assign_indirect(
        &mut self,
        base: Option<Expr>,
        (name, subscripted): (&str, bool),
        (ty, typmod): (u32, i32),
        indirection: &[Option<Node>],
        rhs: Expr,
        assigning: &Assigning,
    ) -> Result<Expr> {
        let at = assigning.at;
        // `CaseTest` is the old value of the part, which the executor gives.
        let base = base.unwrap_or_else(|| {
            let mut test = Expr::new(ExprKind::CaseTest, ty);
            test.typmod = typmod;
            test
        });
        let mut subscripts: Vec<&A_Indices> = Vec::new();
        for (i, item) in indirection.iter().enumerate() {
            let field = match item {
                Some(Node::A_Indices(indices)) => {
                    subscripts.push(indices);
                    continue;
                }
                Some(Node::A_Star(_)) => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "row expansion via \"*\" is not supported here",
                    )
                    .at_opt(at));
                }
                Some(Node::String(field)) => &**field,
                _ => return Err(Error::internal("unrecognized indirection")),
            };
            let target = (name, (ty, typmod));
            if !subscripts.is_empty() {
                let rest = &indirection[i..];
                return self.assign_subscripts(base, target, &subscripts, rest, rhs, assigning);
            }
            let rest = &indirection[i + 1..];
            return self.assign_field(base, target, field, rest, rhs, assigning);
        }
        if !subscripts.is_empty() {
            let target = (name, (ty, typmod));
            return self.assign_subscripts(base, target, &subscripts, &[], rhs, assigning);
        }
        let source = rhs.ty;
        let rhs = self.coerce_to_target(rhs, ty, typmod, assigning.context, None)?;
        rhs.ok_or_else(|| {
            let message = if subscripted {
                format!(
                    "subscripted assignment to \"{name}\" requires type {} but expression is of type {}",
                    types::name(ty),
                    types::name(source)
                )
            } else {
                format!(
                    "subfield \"{name}\" is of type {} but expression is of type {}",
                    types::name(ty),
                    types::name(source)
                )
            };
            Error::new(SqlState::DATATYPE_MISMATCH, message)
                .with_hint("You will need to rewrite or cast the expression.")
                .at_opt(at)
        })
    }

    /// The assignment to the field `field` of the row value `base`, as a `FieldStore`. A domain over a row type checks the new row.
    fn assign_field(
        &mut self,
        base: Expr,
        (name, (ty, typmod)): (&str, (u32, i32)),
        field: &str,
        rest: &[Option<Node>],
        rhs: Expr,
        assigning: &Assigning,
    ) -> Result<Expr> {
        let at = assigning.at;
        let row = types::base(ty);
        let Some(columns) = self.row_columns(row) else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "cannot assign to field \"{field}\" of column \"{name}\" because its type {} is not a composite type",
                    types::name(ty)
                ),
            )
            .at_opt(at));
        };
        let Some(index) = columns.iter().position(|c| c.name == field) else {
            let message = if SYSTEM_COLUMNS.contains(&field) {
                format!("cannot assign to system column \"{field}\"")
            } else {
                format!(
                    "cannot assign to field \"{field}\" of column \"{name}\" because there is no such column in data type {}",
                    types::name(ty)
                )
            };
            return Err(Error::new(SqlState::UNDEFINED_COLUMN, message).at_opt(at));
        };
        let column = &columns[index];
        let target = (column.ty, column.typmod);
        let value = self.assign_indirect(None, (field, false), target, rest, rhs, assigning)?;
        let types = columns.iter().map(|c| c.ty).collect();
        let store = FieldStore { arg: base, fields: vec![(index, value)], types };
        let result = Expr::new(ExprKind::FieldStore(Box::new(store)), row);
        if row == ty {
            return Ok(result);
        }
        self.coerce_to_target(result, ty, typmod, Context::Implicit, None)?.ok_or_else(|| {
            Error::new(
                SqlState::CANNOT_COERCE,
                format!("cannot cast type {} to {}", types::name(row), types::name(ty)),
            )
            .at_opt(at)
        })
    }

    /// `transformAssignmentSubscripts`: the assignment to the element or the slice of `base` that `subscripts` name. The value takes the type of the element, or the type of the array for a slice, and the items of `rest` name a part of it.
    fn assign_subscripts(
        &mut self,
        base: Expr,
        (name, (ty, typmod)): (&str, (u32, i32)),
        subscripts: &[&A_Indices],
        rest: &[Option<Node>],
        rhs: Expr,
        assigning: &Assigning,
    ) -> Result<Expr> {
        let (container, container_mod) = container_type(ty, typmod);
        let mut result = self.container_subscripts(base, subscripts)?;
        let needed = (result.ty, result.typmod);
        let rhs = self.assign_indirect(None, (name, true), needed, rest, rhs, assigning)?;
        let ExprKind::Subscript(sub) = &mut result.kind else {
            return Err(Error::internal(
                "the subscripts of an assignment are not a SubscriptingRef",
            ));
        };
        sub.assign = Some(rhs);
        (result.ty, result.typmod) = (container, container_mod);
        if container == ty {
            return Ok(result);
        }
        // A domain over an array checks the new array.
        let context = assigning.context;
        self.coerce_to_target(result, ty, typmod, context, None)?.ok_or_else(|| {
            Error::new(
                SqlState::CANNOT_COERCE,
                format!("cannot cast type {} to {}", types::name(container), types::name(ty)),
            )
            .at_opt(assigning.at)
        })
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

/// The names of the system columns, which an assignment cannot set.
const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "ctid", "xmin", "cmin", "xmax", "cmax"];

/// The parts of an assignment that stay the same in each step of `transformAssignmentIndirection`: the kind of the cast of the new value and the place of the target.
struct Assigning {
    context: Context,
    at: Option<usize>,
}

//! `transformExpr` of `parse_expr.c`: a raw expression of the parser to an [`Expr`] with its type, for the expressions of a query that has no `FROM`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    A_ArrayExpr, A_Const, A_Expr, A_Expr_Kind, BoolExpr, BoolExprType, BoolTestType, CaseExpr,
    FuncCall, MinMaxOp, Node, NullTestType, SQLValueFunction, SQLValueFunctionOp, TypeCast,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::{Kind, Parts};
use crate::coerce::{AtOpt, Context, can_coerce, common_of};
use crate::expr::{BoolOp, BoolTest, Case, Expr, ExprKind, SqlValue};
use crate::typename::{names, place};
use crate::types;

/// The collations that compare as the bytes of the string, which is the only order rupg has.
const C_COLLATIONS: [&str; 5] = ["default", "C", "POSIX", "ucs_basic", "pg_c_utf8"];
/// The collations of PostgreSQL that need another order.
const OTHER_COLLATIONS: [&str; 2] = ["pg_unicode_fast", "unicode"];

/// An error for an expression that the analyzer does not take yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

/// A raw `A_Expr` of one operator, as `makeSimpleA_Expr` builds it.
fn simple_op(name: &str, left: Option<Node>, right: Option<Node>, location: i32) -> Node {
    Node::A_Expr(Box::new(A_Expr {
        kind: A_Expr_Kind::AEXPR_OP,
        name: vec![Some(Node::String(name.into()))],
        lexpr: left,
        rexpr: right,
        location,
        ..A_Expr::default()
    }))
}

/// A raw `BoolExpr`.
fn bool_node(op: BoolExprType, args: Vec<Node>, location: i32) -> Node {
    Node::BoolExpr(Box::new(BoolExpr {
        boolop: op,
        args: args.into_iter().map(Some).collect(),
        location,
    }))
}

impl Analyzer<'_> {
    /// `transformExprRecurse`.
    pub(crate) fn transform(&mut self, node: Option<&Node>) -> Result<Expr> {
        let Some(node) = node else {
            return Ok(Expr::constant(Value::Null, oid::UNKNOWN, -1, None));
        };
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
                .with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."));
        }
        let result = self.transform_node(node);
        self.depth -= 1;
        result
    }

    fn transform_node(&mut self, node: &Node) -> Result<Expr> {
        match node {
            Node::A_Const(c) => self.transform_const(c),
            Node::ColumnRef(c) => self.column_ref(c),
            Node::ParamRef(p) => self.transform_param(p.number, place(p.location)),
            Node::A_Expr(a) => self.transform_a_expr(a),
            Node::BoolExpr(b) => self.transform_bool(b),
            Node::FuncCall(f) => self.transform_func(f),
            Node::TypeCast(t) => self.transform_cast(t),
            Node::CaseExpr(c) => self.transform_case(c),
            Node::CoalesceExpr(c) => {
                let args = self.transform_list(&c.args)?;
                let ty = self.common_type(&args, "COALESCE")?;
                let args = args
                    .into_iter()
                    .map(|a| self.coerce_to_common(a, ty, "COALESCE"))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Expr {
                    kind: ExprKind::Coalesce(args),
                    ty,
                    typmod: -1,
                    location: place(c.location),
                })
            }
            Node::MinMaxExpr(m) => {
                let construct = if m.op == MinMaxOp::IS_GREATEST { "GREATEST" } else { "LEAST" };
                let args = self.transform_list(&m.args)?;
                let ty = self.common_type(&args, construct)?;
                let args = args
                    .into_iter()
                    .map(|a| self.coerce_to_common(a, ty, construct))
                    .collect::<Result<Vec<_>>>()?;
                let at = place(m.location);
                let less = self.comparison(ty, at)?;
                Ok(Expr {
                    kind: ExprKind::MinMax { greatest: m.op == MinMaxOp::IS_GREATEST, less, args },
                    ty,
                    typmod: -1,
                    location: at,
                })
            }
            Node::NullTest(n) => {
                let arg = self.transform(n.arg.as_ref())?;
                let is_null = n.nulltesttype == NullTestType::IS_NULL;
                Ok(Expr::new(ExprKind::NullTest(Box::new(arg), is_null), oid::BOOL)
                    .at(place(n.location)))
            }
            Node::BooleanTest(b) => {
                let (test, construct) = match b.booltesttype {
                    BoolTestType::IS_TRUE => (BoolTest::IsTrue, "IS TRUE"),
                    BoolTestType::IS_NOT_TRUE => (BoolTest::IsNotTrue, "IS NOT TRUE"),
                    BoolTestType::IS_FALSE => (BoolTest::IsFalse, "IS FALSE"),
                    BoolTestType::IS_NOT_FALSE => (BoolTest::IsNotFalse, "IS NOT FALSE"),
                    BoolTestType::IS_UNKNOWN => (BoolTest::IsUnknown, "IS UNKNOWN"),
                    _ => (BoolTest::IsNotUnknown, "IS NOT UNKNOWN"),
                };
                let arg = self.transform(b.arg.as_ref())?;
                let arg = self.coerce_to_boolean(arg, construct)?;
                Ok(Expr::new(ExprKind::BooleanTest(Box::new(arg), test), oid::BOOL)
                    .at(place(b.location)))
            }
            Node::SQLValueFunction(f) => self.transform_sql_value(f),
            Node::A_ArrayExpr(a) => self.transform_array(a, 0, 0, -1),
            Node::CollateClause(c) => {
                let arg = self.transform(c.arg.as_ref())?;
                let at = place(c.location);
                if !types::collatable(arg.ty) && arg.ty != oid::UNKNOWN {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", types::name(arg.ty)),
                    )
                    .at_opt(at));
                }
                self.collation_oid(&c.collname, at)?;
                Ok(arg)
            }
            Node::RowExpr(r) => Err(not_yet("ROW()", place(r.location))),
            Node::SubLink(s) => self.transform_sublink(s),
            Node::A_Indirection(_) => Err(not_yet("a subscript or a field selection", None)),
            Node::GroupingFunc(g) => Err(Error::new(
                SqlState::GROUPING_ERROR,
                "arguments to GROUPING must be grouping expressions of the associated query level",
            )
            .at_opt(place(g.location))),
            _ => Err(not_yet("this kind of expression", None)),
        }
    }

    /// `LookupCollation`: the OID of the collation that a name gives. Only the collations that compare as the bytes of the string are supported.
    pub(crate) fn collation_oid(&self, list: &[Option<Node>], at: Option<usize>) -> Result<u32> {
        let parts = names(list);
        let (schema, name) = self.split_name(&parts, at)?;
        let in_catalog = schema.is_none_or(|ns| ns == crate::PG_CATALOG_NAMESPACE);
        if in_catalog && C_COLLATIONS.contains(&name) {
            let row = rupg_pgcatalog::builtin::named(rupg_pgcatalog::builtin::Named::Collation)
                .iter()
                .find(|r| r.name == name && (r.encoding == -1 || r.encoding == 6));
            return row.map(|r| r.oid).ok_or_else(|| Error::internal("a built-in collation"));
        }
        if in_catalog && OTHER_COLLATIONS.contains(&name) {
            return Err(not_yet(&format!("collation \"{name}\""), at));
        }
        Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("collation \"{}\" for encoding \"UTF8\" does not exist", parts.join(".")),
        )
        .at_opt(at))
    }

    /// The expressions of a list.
    pub(crate) fn transform_list(&mut self, list: &[Option<Node>]) -> Result<Vec<Expr>> {
        list.iter().map(|n| self.transform(n.as_ref())).collect()
    }

    /// `make_const`: the constant of a literal.
    fn transform_const(&mut self, c: &A_Const) -> Result<Expr> {
        let at = place(c.location);
        if c.isnull {
            return Ok(Expr::constant(Value::Null, oid::UNKNOWN, -1, at));
        }
        let (value, ty) = match &c.val {
            Some(Node::Integer(v)) => (Value::Int4(*v), oid::INT4),
            Some(Node::Float(text)) => match rupg_types::int8_in(text) {
                // A literal that fits `int8` is an `int8`, and any other number is a `numeric`.
                Ok(v) => (Value::Int8(v), oid::INT8),
                Err(_) => {
                    let v =
                        rupg_types::numeric_in(text, -1).map_err(|e| Error::from(e).at_opt(at))?;
                    (Value::Numeric(v), oid::NUMERIC)
                }
            },
            Some(Node::Boolean(v)) => (Value::Bool(*v), oid::BOOL),
            Some(Node::String(s)) => (Value::text(&**s), oid::UNKNOWN),
            Some(Node::BitString(_)) => return Err(not_yet("a bit string constant", at)),
            _ => return Err(Error::internal("unrecognized node type in A_Const")),
        };
        Ok(Expr::constant(value, ty, -1, at))
    }

    /// `$n`.
    fn transform_param(&mut self, number: i32, at: Option<usize>) -> Result<Expr> {
        let index = usize::try_from(number).ok().filter(|&n| n >= 1);
        if let Some(n) = index {
            // A query of the extended protocol takes any parameter. One that the client did not declare has no type until the query gives it one.
            if self.variable && n > self.params.len() && n <= MAX_PARAMS {
                self.params.resize(n, 0);
            }
        }
        let Some(n) = index.filter(|&n| n <= self.params.len()) else {
            return Err(Error::new(
                SqlState::UNDEFINED_PARAMETER,
                format!("there is no parameter ${number}"),
            )
            .at_opt(at));
        };
        let ty = match self.params[n - 1] {
            0 => oid::UNKNOWN,
            ty => ty,
        };
        Ok(Expr { kind: ExprKind::Param(n), ty, typmod: -1, location: at })
    }

    /// The operators and the other forms of `A_Expr`.
    fn transform_a_expr(&mut self, a: &A_Expr) -> Result<Expr> {
        let at = place(a.location);
        let op_names = names(&a.name);
        match a.kind {
            A_Expr_Kind::AEXPR_OP
            | A_Expr_Kind::AEXPR_LIKE
            | A_Expr_Kind::AEXPR_ILIKE
            | A_Expr_Kind::AEXPR_SIMILAR => {
                if matches!((&a.lexpr, &a.rexpr), (Some(Node::RowExpr(_)), Some(Node::RowExpr(_))))
                {
                    return Err(not_yet("a row comparison", at));
                }
                let left = match &a.lexpr {
                    Some(node) => Some(self.transform(Some(node))?),
                    None => None,
                };
                let right = self.transform(a.rexpr.as_ref())?;
                self.make_op(&op_names, left, right, at)
            }
            A_Expr_Kind::AEXPR_OP_ANY | A_Expr_Kind::AEXPR_OP_ALL => {
                let left = self.transform(a.lexpr.as_ref())?;
                let right = self.transform(a.rexpr.as_ref())?;
                self.scalar_array_op(
                    &op_names,
                    a.kind == A_Expr_Kind::AEXPR_OP_ANY,
                    left,
                    right,
                    at,
                )
            }
            A_Expr_Kind::AEXPR_DISTINCT | A_Expr_Kind::AEXPR_NOT_DISTINCT => {
                if matches!((&a.lexpr, &a.rexpr), (Some(Node::RowExpr(_)), Some(Node::RowExpr(_))))
                {
                    return Err(not_yet("a row comparison", at));
                }
                let left = self.transform(a.lexpr.as_ref())?;
                let right = self.transform(a.rexpr.as_ref())?;
                let name = op_names.last().copied().unwrap_or("=");
                let (func, ty, left, right) = self.operator_function(name, left, right, at)?;
                if ty != oid::BOOL {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        "IS DISTINCT FROM requires = operator to yield boolean",
                    )
                    .at_opt(at));
                }
                let not = a.kind == A_Expr_Kind::AEXPR_NOT_DISTINCT;
                Ok(Expr::new(
                    ExprKind::Distinct { equal: func, not, args: vec![left, right] },
                    oid::BOOL,
                )
                .at(at))
            }
            A_Expr_Kind::AEXPR_NULLIF => {
                let left = self.transform(a.lexpr.as_ref())?;
                let right = self.transform(a.rexpr.as_ref())?;
                let name = op_names.last().copied().unwrap_or("=");
                let (func, ty, left, right) = self.operator_function(name, left, right, at)?;
                if ty != oid::BOOL {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        "NULLIF requires = operator to yield boolean",
                    )
                    .at_opt(at));
                }
                let result = left.ty;
                Ok(Expr::new(ExprKind::NullIf { equal: func, args: vec![left, right] }, result)
                    .at(at))
            }
            A_Expr_Kind::AEXPR_IN => self.transform_in(a, &op_names),
            A_Expr_Kind::AEXPR_BETWEEN
            | A_Expr_Kind::AEXPR_NOT_BETWEEN
            | A_Expr_Kind::AEXPR_BETWEEN_SYM
            | A_Expr_Kind::AEXPR_NOT_BETWEEN_SYM => {
                let Some(Node::List(bounds)) = &a.rexpr else {
                    return Err(Error::internal("BETWEEN needs two bounds"));
                };
                let (Some(Some(low)), Some(Some(high))) = (bounds.first(), bounds.get(1)) else {
                    return Err(Error::internal("BETWEEN needs two bounds"));
                };
                let value = a.lexpr.clone();
                let l = a.location;
                let range = |ge: &str, le: &str, join: BoolExprType, low: &Node, high: &Node| {
                    bool_node(
                        join,
                        vec![
                            simple_op(ge, value.clone(), Some(low.clone()), l),
                            simple_op(le, value.clone(), Some(high.clone()), l),
                        ],
                        l,
                    )
                };
                let node = match a.kind {
                    A_Expr_Kind::AEXPR_BETWEEN => {
                        range(">=", "<=", BoolExprType::AND_EXPR, low, high)
                    }
                    A_Expr_Kind::AEXPR_NOT_BETWEEN => {
                        range("<", ">", BoolExprType::OR_EXPR, low, high)
                    }
                    A_Expr_Kind::AEXPR_BETWEEN_SYM => bool_node(
                        BoolExprType::OR_EXPR,
                        vec![
                            range(">=", "<=", BoolExprType::AND_EXPR, low, high),
                            range(">=", "<=", BoolExprType::AND_EXPR, high, low),
                        ],
                        l,
                    ),
                    _ => bool_node(
                        BoolExprType::AND_EXPR,
                        vec![
                            range("<", ">", BoolExprType::OR_EXPR, low, high),
                            range("<", ">", BoolExprType::OR_EXPR, high, low),
                        ],
                        l,
                    ),
                };
                self.transform(Some(&node))
            }
            _ => Err(not_yet("this kind of operator", at)),
        }
    }

    /// `make_scalar_array_op`: `x op ANY (array)` or `x op ALL (array)`.
    fn scalar_array_op(
        &mut self,
        op_names: &[&str],
        any: bool,
        left: Expr,
        right: Expr,
        at: Option<usize>,
    ) -> Result<Expr> {
        let array = right.ty;
        let element = if array == oid::UNKNOWN {
            oid::UNKNOWN
        } else {
            match types::element(types::base(array)) {
                0 => {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        "op ANY/ALL (array) requires array on right side",
                    )
                    .at_opt(at));
                }
                e => e,
            }
        };
        let op = self.find_operator(op_names, left.ty, element, at)?;
        let mut declared = [op.left, op.right];
        let result = crate::poly::resolve(&[left.ty, element], &mut declared, op.result)?;
        if result != oid::BOOL {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "op ANY/ALL (array) requires operator to yield boolean",
            )
            .at_opt(at));
        }
        let array_decl = if types::is_polymorphic(declared[1]) {
            array
        } else {
            match types::array_of(declared[1]) {
                0 => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "could not find array type for data type {}",
                            types::name(declared[1])
                        ),
                    )
                    .at_opt(at));
                }
                a => a,
            }
        };
        let left = if left.ty == declared[0] {
            left
        } else {
            self.coerce(left, declared[0], -1, Context::Implicit, None)?
        };
        let right = if right.ty == array_decl {
            right
        } else {
            self.coerce(right, array_decl, -1, Context::Implicit, None)?
        };
        Ok(Expr::new(
            ExprKind::ScalarArrayOp { func: op.code, any, args: vec![left, right] },
            oid::BOOL,
        )
        .at(at))
    }

    /// `transformAExprIn`: `x IN (a, b)` as `x = ANY (ARRAY[a, b])` when the values have a common type, or else as `x = a OR x = b`.
    fn transform_in(&mut self, a: &A_Expr, op_names: &[&str]) -> Result<Expr> {
        let at = place(a.location);
        let use_or = op_names.last() != Some(&"<>");
        let left = self.transform(a.lexpr.as_ref())?;
        let Some(Node::List(list)) = &a.rexpr else {
            return Err(not_yet("IN with a subquery", at));
        };
        let mut values = self.transform_list(list)?;
        let mut result: Option<Expr> = None;
        if values.len() > 1 {
            let mut all: Vec<u32> = vec![left.ty];
            all.extend(values.iter().map(|v| v.ty));
            let scalar = common_of(&all)
                .ok()
                .filter(|&t| all.iter().all(|&a| can_coerce(&[a], &[t], Context::Implicit)))
                .filter(|&t| t != oid::RECORD);
            let array = scalar.map_or(0, types::array_of);
            if let (Some(scalar), true) = (scalar, array != 0) {
                let elements = values
                    .drain(..)
                    .map(|v| self.coerce_to_common(v, scalar, "IN"))
                    .collect::<Result<Vec<_>>>()?;
                let array_expr = Expr::new(
                    ExprKind::Array { element: scalar, multidims: false, elements },
                    array,
                );
                result =
                    Some(self.scalar_array_op(op_names, use_or, left.clone(), array_expr, at)?);
            }
        }
        for value in values {
            let cmp = self.make_op(op_names, Some(left.clone()), value, at)?;
            let cmp = self.coerce_to_boolean(cmp, "IN")?;
            result = Some(match result {
                None => cmp,
                Some(prev) => {
                    let op = if use_or { BoolOp::Or } else { BoolOp::And };
                    Expr::new(ExprKind::Bool(op, vec![prev, cmp]), oid::BOOL).at(at)
                }
            });
        }
        result.ok_or_else(|| Error::internal("IN with no values"))
    }

    /// `AND`, `OR` and `NOT`.
    fn transform_bool(&mut self, b: &BoolExpr) -> Result<Expr> {
        let (op, construct) = match b.boolop {
            BoolExprType::AND_EXPR => (BoolOp::And, "AND"),
            BoolExprType::OR_EXPR => (BoolOp::Or, "OR"),
            _ => (BoolOp::Not, "NOT"),
        };
        let mut args = Vec::with_capacity(b.args.len());
        for arg in &b.args {
            let arg = self.transform(arg.as_ref())?;
            args.push(self.coerce_to_boolean(arg, construct)?);
        }
        Ok(Expr::new(ExprKind::Bool(op, args), oid::BOOL).at(place(b.location)))
    }

    /// A call of a function.
    fn transform_func(&mut self, f: &FuncCall) -> Result<Expr> {
        let at = place(f.location);
        let func_names = names(&f.funcname);
        if f.args.iter().any(|a| matches!(a, Some(Node::NamedArgExpr(_)))) {
            return Err(not_yet("a call with named arguments", at));
        }
        let mut args = self.transform_list(&f.args)?;
        // transformFuncCall: the items of WITHIN GROUP come after the direct arguments.
        if f.agg_within_group {
            for item in &f.agg_order {
                let Some(Node::SortBy(by)) = item else {
                    return Err(Error::internal("an item of WITHIN GROUP that is not SortBy"));
                };
                args.push(self.transform(by.node.as_ref())?);
            }
        }
        let filter = match &f.agg_filter {
            Some(node) => {
                let expr = self.with_kind(Kind::Filter, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "FILTER")?)
            }
            None => None,
        };
        let parts = Parts {
            star: f.agg_star,
            distinct: f.agg_distinct,
            within_group: f.agg_within_group,
            order: &f.agg_order,
            filter,
            over: f.over.is_some(),
            null_treatment: f.ignore_nulls != 0,
        };
        self.make_func(&func_names, args, f.func_variadic, Some(parts), at)
    }

    /// `transformTypeCast`: `x::type` and `CAST(x AS type)`.
    fn transform_cast(&mut self, t: &TypeCast) -> Result<Expr> {
        let Some(type_name) = &t.typeName else {
            return Err(Error::internal("a cast with no type"));
        };
        let (target, typmod) = self.type_name(type_name)?;
        let expr = match &t.arg {
            Some(Node::A_ArrayExpr(a)) => {
                let base = types::base(target);
                match types::element(base) {
                    0 => self.transform(t.arg.as_ref())?,
                    element => self.transform_array(a, base, element, typmod)?,
                }
            }
            arg => self.transform(arg.as_ref())?,
        };
        let at = place(t.location).or(place(type_name.location));
        let source = expr.ty;
        let place_of_expr = expr.place();
        match self.coerce_to_target(expr, target, typmod, Context::Explicit, at)? {
            Some(expr) => Ok(expr),
            None => Err(Error::new(
                SqlState::CANNOT_COERCE,
                format!("cannot cast type {} to {}", types::name(source), types::name(target)),
            )
            .at_opt(at.or(place_of_expr))),
        }
    }

    /// `transformCaseExpr`.
    fn transform_case(&mut self, c: &CaseExpr) -> Result<Expr> {
        let at = place(c.location);
        let arg = match &c.arg {
            Some(node) => {
                let arg = self.transform(Some(node))?;
                let arg = if arg.ty == oid::UNKNOWN {
                    self.coerce_to_common(arg, oid::TEXT, "CASE")?
                } else {
                    arg
                };
                Some(arg)
            }
            None => None,
        };
        let mut conditions = Vec::new();
        let mut results = Vec::new();
        for when in &c.args {
            let Some(Node::CaseWhen(w)) = when else {
                return Err(Error::internal("a CASE arm that is not WHEN"));
            };
            let condition = match &arg {
                Some(arg) => {
                    let test = Expr::new(ExprKind::CaseTest, arg.ty);
                    let value = self.transform(w.expr.as_ref())?;
                    self.make_op(&["="], Some(test), value, place(w.location))?
                }
                None => self.transform(w.expr.as_ref())?,
            };
            conditions.push(self.coerce_to_boolean(condition, "CASE/WHEN")?);
            results.push(self.transform(w.result.as_ref())?);
        }
        let default = match &c.defresult {
            Some(node) => self.transform(Some(node))?,
            None => Expr::constant(Value::Null, oid::UNKNOWN, -1, None),
        };
        // The default comes first, so that its type wins a tie as in PostgreSQL.
        let mut all = Vec::with_capacity(results.len() + 1);
        all.push(default);
        all.extend(results);
        let ty = self.common_type(&all, "CASE")?;
        let mut all = all
            .into_iter()
            .map(|e| self.coerce_to_common(e, ty, "CASE"))
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        let default = all.next().expect("the default");
        let whens = conditions.into_iter().zip(all).collect();
        Ok(Expr {
            kind: ExprKind::Case(Case {
                arg: arg.map(Box::new),
                whens,
                default: Box::new(default),
            }),
            ty,
            typmod: -1,
            location: at,
        })
    }

    /// `transformSQLValueFunction`.
    fn transform_sql_value(&mut self, f: &SQLValueFunction) -> Result<Expr> {
        let at = place(f.location);
        let precision = |s: &mut Self, what: &str, tz: bool| -> Result<Option<i32>> {
            let tz = if tz { " WITH TIME ZONE" } else { "" };
            let n = f.typmod;
            s.precision(n, &format!("{what}({n}){tz}")).map(Some).map_err(|e| e.at_opt(at))
        };
        let (value, ty) = match f.op {
            SQLValueFunctionOp::SVFOP_CURRENT_DATE => (SqlValue::CurrentDate, oid::DATE),
            SQLValueFunctionOp::SVFOP_CURRENT_TIME => (SqlValue::CurrentTime(None), oid::TIMETZ),
            SQLValueFunctionOp::SVFOP_CURRENT_TIME_N => {
                (SqlValue::CurrentTime(precision(self, "TIME", true)?), oid::TIMETZ)
            }
            SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP => {
                (SqlValue::CurrentTimestamp(None), oid::TIMESTAMPTZ)
            }
            SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N => {
                (SqlValue::CurrentTimestamp(precision(self, "TIMESTAMP", true)?), oid::TIMESTAMPTZ)
            }
            SQLValueFunctionOp::SVFOP_LOCALTIME => (SqlValue::LocalTime(None), oid::TIME),
            SQLValueFunctionOp::SVFOP_LOCALTIME_N => {
                (SqlValue::LocalTime(precision(self, "TIME", false)?), oid::TIME)
            }
            SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP => {
                (SqlValue::LocalTimestamp(None), oid::TIMESTAMP)
            }
            SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP_N => {
                (SqlValue::LocalTimestamp(precision(self, "TIMESTAMP", false)?), oid::TIMESTAMP)
            }
            SQLValueFunctionOp::SVFOP_CURRENT_ROLE => (SqlValue::CurrentRole, oid::NAME),
            SQLValueFunctionOp::SVFOP_CURRENT_USER => (SqlValue::CurrentUser, oid::NAME),
            SQLValueFunctionOp::SVFOP_USER => (SqlValue::User, oid::NAME),
            SQLValueFunctionOp::SVFOP_SESSION_USER => (SqlValue::SessionUser, oid::NAME),
            SQLValueFunctionOp::SVFOP_CURRENT_CATALOG => (SqlValue::CurrentCatalog, oid::NAME),
            _ => (SqlValue::CurrentSchema, oid::NAME),
        };
        let typmod = match value {
            SqlValue::CurrentTime(Some(p))
            | SqlValue::CurrentTimestamp(Some(p))
            | SqlValue::LocalTime(Some(p))
            | SqlValue::LocalTimestamp(Some(p)) => p,
            _ => -1,
        };
        Ok(Expr { kind: ExprKind::SqlValue(value), ty, typmod, location: at })
    }

    /// `transformArrayExpr`. A cast such as `ARRAY[]::int[]` gives the array type, the element type and the typmod. With no cast they are 0, 0 and -1.
    fn transform_array(
        &mut self,
        a: &A_ArrayExpr,
        array: u32,
        element: u32,
        typmod: i32,
    ) -> Result<Expr> {
        let at = place(a.location);
        let mut multidims = false;
        let mut elements = Vec::with_capacity(a.elements.len());
        for e in &a.elements {
            let new = match e {
                Some(Node::A_ArrayExpr(sub)) => {
                    multidims = true;
                    self.transform_array(sub, array, element, typmod)?
                }
                other => {
                    let new = self.transform(other.as_ref())?;
                    if !multidims
                        && new.ty != oid::INT2VECTOR
                        && new.ty != oid::OIDVECTOR
                        && types::element(new.ty) != 0
                    {
                        multidims = true;
                    }
                    new
                }
            };
            elements.push(new);
        }
        let (array, element, target, hard) = if array != 0 {
            (array, element, if multidims { array } else { element }, true)
        } else {
            if elements.is_empty() {
                return Err(Error::new(
                    SqlState::INDETERMINATE_DATATYPE,
                    "cannot determine type of empty array",
                )
                .with_hint("Explicitly cast to the desired type, for example ARRAY[]::integer[].")
                .at_opt(at));
            }
            let common = self.common_type(&elements, "ARRAY")?;
            if multidims {
                let element = types::element(common);
                if element == 0 {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "could not find element type for data type {}",
                            types::name(common)
                        ),
                    )
                    .at_opt(at));
                }
                (common, element, common, false)
            } else {
                let array = types::array_of(common);
                if array == 0 {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("could not find array type for data type {}", types::name(common)),
                    )
                    .at_opt(at));
                }
                (array, common, common, false)
            }
        };
        let mut coerced = Vec::with_capacity(elements.len());
        for e in elements {
            let new = if hard {
                let (source, place_of) = (e.ty, e.place());
                match self.coerce_to_target(e, target, typmod, Context::Explicit, None)? {
                    Some(new) => new,
                    None => {
                        return Err(Error::new(
                            SqlState::CANNOT_COERCE,
                            format!(
                                "cannot cast type {} to {}",
                                types::name(source),
                                types::name(target)
                            ),
                        )
                        .at_opt(place_of));
                    }
                }
            } else {
                self.coerce_to_common(e, target, "ARRAY")?
            };
            coerced.push(new);
        }
        Ok(Expr {
            kind: ExprKind::Array { element, multidims, elements: coerced },
            ty: array,
            typmod: -1,
            location: at,
        })
    }

    /// The function of the `<` operator of the type, for `GREATEST` and `LEAST`.
    fn comparison(&self, ty: u32, at: Option<usize>) -> Result<u32> {
        let base = types::base(ty);
        rupg_pgcatalog::builtin::operators_named("<")
            .find(|op| {
                op.kind == b'b' && op.left == base && op.right == base && op.namespace == PG_CATALOG
            })
            .map(|op| op.code)
            .ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!(
                        "could not identify a comparison function for type {}",
                        types::name(ty)
                    ),
                )
                .at_opt(at)
            })
    }
}

/// The OID of the schema `pg_catalog`.
const PG_CATALOG: u32 = 11;

/// The depth of nested expressions that the analyzer takes before it stops with the error of `check_stack_depth`.
const MAX_DEPTH: usize = 1000;

/// The largest number of parameters that the protocol can bind.
const MAX_PARAMS: usize = 65535;

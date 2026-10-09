//! `Expr`, an expression of the query tree with its type, as the `Expr` nodes of `primnodes.h` after `transformExpr`.

use rupg_types::Value;

use crate::select::Query;
use crate::sort::SortGroup;

/// An expression with its type and its typmod. A typmod of -1 means that the expression has no typmod.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    /// The OID of the type of the value.
    pub ty: u32,
    pub typmod: i32,
    /// The byte offset in the query text of the place that the expression starts, for the position of an error, or `None`.
    pub location: Option<usize>,
}

/// The kinds of expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// `Const`: a value. A string literal that has no type yet has the type `unknown` and a `Text` value.
    Const(Value),
    /// `Param`: the parameter `$n`, from 1.
    Param(usize),
    /// `FuncExpr` and `OpExpr`: a call of the function with this `pg_proc` OID.
    Func(Func),
    /// `RelabelType`: the value of the argument with another type that has the same binary form.
    Relabel(Box<Expr>, CastForm),
    /// `CoerceViaIO`: the output function of the type of the argument, then the input function of the type of the expression.
    CoerceViaIo(Box<Expr>, CastForm),
    /// `ArrayCoerceExpr`: a cast of each element of an array. `element` casts [`ExprKind::CaseTest`], which is the value of one element.
    ArrayCoerce { arg: Box<Expr>, element: Box<Expr>, form: CastForm },
    /// `CoerceToDomain`: the value of the argument as a value of the domain of the expression, after the domain checks its constraints. The argument has the base type of the domain.
    CoerceToDomain(Box<Expr>, CastForm),
    /// `CoerceToDomainValue`: `VALUE` in a check constraint of a domain, the value that the domain checks.
    DomainValue,
    /// `SubscriptingRef`: an element or a slice of an array.
    Subscript(Box<Subscript>),
    /// `BoolExpr`: `AND`, `OR` and `NOT`.
    Bool(BoolOp, Vec<Expr>),
    /// `NullTest`: `IS NULL` when the flag is true, `IS NOT NULL` when it is false.
    NullTest(Box<Expr>, bool),
    /// `BooleanTest`: `IS TRUE`, `IS NOT UNKNOWN` and the others.
    BooleanTest(Box<Expr>, BoolTest),
    /// `CaseExpr`. With an argument, each condition compares [`ExprKind::CaseTest`] to a value.
    Case(Case),
    /// `CaseTestExpr`: the value of the argument of the `CASE` that holds this expression.
    CaseTest,
    /// `CoalesceExpr`: the first argument that is not null.
    Coalesce(Vec<Expr>),
    /// `MinMaxExpr`: `GREATEST` or `LEAST`, with the function of the `<` operator of the type.
    MinMax { greatest: bool, less: u32, args: Vec<Expr> },
    /// `NullIfExpr`: null when the function of the `=` operator gives true, or else the first argument.
    NullIf { equal: u32, args: Vec<Expr> },
    /// `DistinctExpr`: `IS DISTINCT FROM`, or `IS NOT DISTINCT FROM` when `not` is true, with the function of the `=` operator.
    Distinct { equal: u32, not: bool, args: Vec<Expr> },
    /// `ScalarArrayOpExpr`: `x op ANY (array)` when `any` is true, or `x op ALL (array)`.
    ScalarArrayOp { func: u32, any: bool, args: Vec<Expr> },
    /// `ArrayExpr`: `ARRAY[...]`. The elements of a multidimensional array are arrays.
    Array { element: u32, multidims: bool, elements: Vec<Expr> },
    /// `SQLValueFunction`, such as `CURRENT_USER` and `CURRENT_TIMESTAMP(3)`.
    SqlValue(SqlValue),
    /// `Var`: a column of a relation of `FROM`, or with the attribute number 0 the whole row of the relation, as a value of its row type.
    Var(Var),
    /// `FieldSelect`: the field with this index, from 0, of a row value.
    FieldSelect(Box<Expr>, usize),
    /// `Aggref`: a call of an aggregate function, which reads all the rows of a group.
    Agg(Box<Aggref>),
    /// `SubLink`: a subquery in an expression.
    SubLink(Box<SubLink>),
    /// `Param` of the kind `PARAM_SUBLINK`: the value of the column with this index, from 0, of a row of the subquery, in the test of `ANY` or `ALL`.
    SubColumn(usize),
}

/// The parts of a fetch from an array, as `SubscriptingRef`.
#[derive(Clone, Debug, PartialEq)]
pub struct Subscript {
    /// `refexpr`: the array.
    pub container: Expr,
    /// `refupperindexpr`: the subscript of each dimension, or the upper bound of a slice. `None` is a bound that the query does not give.
    pub upper: Vec<Option<Expr>>,
    /// `reflowerindexpr`: the lower bound of each dimension of a slice, or `None` for one element.
    pub lower: Option<Vec<Option<Expr>>>,
}

impl Subscript {
    /// The subscripts that the query gives, the upper bounds first.
    pub fn bounds(&self) -> impl Iterator<Item = &Expr> {
        self.upper.iter().chain(self.lower.iter().flatten()).flatten()
    }
}

/// A subquery in an expression, as `SubLink`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubLink {
    pub kind: SubLinkKind,
    /// `testexpr`: for `ANY` and `ALL`, the test of each row of the subquery. It compares the left side with [`ExprKind::SubColumn`].
    pub test: Option<Expr>,
    pub query: Query,
}

/// The kinds of `SubLink` that the analyzer takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubLinkKind {
    /// `EXISTS (SELECT ...)`.
    Exists,
    /// `x op ALL (SELECT ...)`.
    All,
    /// `x op ANY (SELECT ...)` and `x IN (SELECT ...)`.
    Any,
    /// `(SELECT ...)`, which gives the value of the one column of the one row.
    Expr,
    /// `ARRAY(SELECT ...)`, which gives the values of the one column as an array.
    Array,
}

/// A column of a relation of `FROM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Var {
    /// The index of the relation in [`crate::Query::relations`].
    pub relation: usize,
    /// The attribute number: from 1 for a column of the relation, or a negative number for a system column.
    pub attnum: i16,
    /// `varlevelsup`: 0 for a relation of the query that has the expression, 1 for a relation of the query outside it, and so on.
    pub levels_up: usize,
}

/// A call of a function.
#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    /// The OID of the function in `pg_proc`.
    pub oid: u32,
    pub args: Vec<Expr>,
    /// The way the call was written.
    pub form: FuncForm,
    /// True when the last argument is the array of a variadic function, as `funcvariadic` of `FuncExpr`. A function with `VARIADIC "any"` then takes the elements of the array as its arguments.
    pub variadic: bool,
    /// `funcretset`: true when the function gives a set.
    pub retset: bool,
}

/// A call of an aggregate function, as `Aggref`.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggref {
    /// `aggfnoid`: the OID of the aggregate function in `pg_proc`.
    pub oid: u32,
    /// The arguments, then the expressions that only the `ORDER BY` of the call reads.
    pub args: Vec<Expr>,
    /// The number of arguments at the start of `args`.
    pub nargs: usize,
    /// `aggorder`: the items of the `ORDER BY` of the call. Each item names an expression of `args`.
    pub order: Vec<SortGroup>,
    /// `aggdistinct`: the items of `DISTINCT`, which are all the arguments. The items of `ORDER BY` come first.
    pub distinct: Vec<SortGroup>,
    /// `aggstar`: true for `count(*)`.
    pub star: bool,
    /// `aggvariadic`: true when the last argument is the array of a variadic aggregate.
    pub variadic: bool,
    /// `aggfilter`: the condition of `FILTER (WHERE ...)`.
    pub filter: Option<Box<Expr>>,
}

/// The way a call was written, which a deparse function shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncForm {
    /// `f(x)`.
    Call,
    /// An operator with this `pg_operator` OID.
    Operator(u32),
    /// `x::type`, `CAST(x AS type)` or `type(x)`.
    ExplicitCast,
    /// A cast that the analyzer added.
    ImplicitCast,
    /// A call in the special syntax of the SQL standard, such as `SUBSTRING(x FROM 2)` or `x AT TIME ZONE 'UTC'`, as `COERCE_SQL_SYNTAX`.
    SqlSyntax,
}

/// `CoercionForm` of a cast node: the way a cast was written, which a deparse function shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastForm {
    /// `x::type` or `CAST(x AS type)`.
    Explicit,
    /// A cast that the analyzer added.
    Implicit,
}

/// The operators of `BoolExpr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
    Not,
}

/// The tests of `BooleanTest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolTest {
    IsTrue,
    IsNotTrue,
    IsFalse,
    IsNotFalse,
    IsUnknown,
    IsNotUnknown,
}

/// A `CASE` expression.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// The value that the conditions compare, for `CASE x WHEN ...`.
    pub arg: Option<Box<Expr>>,
    /// Each condition with its result.
    pub whens: Vec<(Expr, Expr)>,
    /// The result of `ELSE`, which is a null constant when the query has no `ELSE`.
    pub default: Box<Expr>,
}

/// The functions of the SQL standard that take no parentheses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlValue {
    CurrentDate,
    /// `CURRENT_TIME`, with the precision if the query gives one.
    CurrentTime(Option<i32>),
    CurrentTimestamp(Option<i32>),
    LocalTime(Option<i32>),
    LocalTimestamp(Option<i32>),
    CurrentRole,
    CurrentUser,
    User,
    SessionUser,
    CurrentCatalog,
    CurrentSchema,
}

impl Expr {
    /// An expression with no typmod and no location.
    pub fn new(kind: ExprKind, ty: u32) -> Expr {
        Expr { kind, ty, typmod: -1, location: None }
    }

    /// A constant.
    pub fn constant(value: Value, ty: u32, typmod: i32, location: Option<usize>) -> Expr {
        Expr { kind: ExprKind::Const(value), ty, typmod, location }
    }

    /// Sets the location.
    #[must_use]
    pub fn at(mut self, location: Option<usize>) -> Expr {
        self.location = location;
        self
    }

    /// The value of a constant, or `None` for any other expression.
    pub fn as_const(&self) -> Option<&Value> {
        match &self.kind {
            ExprKind::Const(value) => Some(value),
            _ => None,
        }
    }

    /// The expressions that this expression has as its arguments.
    pub fn children(&self) -> Vec<&Expr> {
        match &self.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::CaseTest
            | ExprKind::DomainValue
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_)
            | ExprKind::SubColumn(_) => Vec::new(),
            ExprKind::SubLink(sub) => sub.test.iter().collect(),
            ExprKind::Func(f) => f.args.iter().collect(),
            ExprKind::Agg(agg) => agg.args.iter().chain(agg.filter.as_deref()).collect(),
            ExprKind::Relabel(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => vec![&**arg],
            ExprKind::ArrayCoerce { arg, element, .. } => vec![&**arg, &**element],
            ExprKind::Subscript(sub) => sub.bounds().chain([&sub.container]).collect(),
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter().collect(),
            ExprKind::Case(case) => {
                let mut all: Vec<&Expr> = case.arg.as_deref().into_iter().collect();
                for (when, then) in &case.whens {
                    all.push(when);
                    all.push(then);
                }
                all.push(&case.default);
                all
            }
        }
    }

    fn children_mut(&mut self) -> Vec<&mut Expr> {
        match &mut self.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::CaseTest
            | ExprKind::DomainValue
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_)
            | ExprKind::SubColumn(_) => Vec::new(),
            ExprKind::SubLink(sub) => sub.test.iter_mut().collect(),
            ExprKind::Func(f) => f.args.iter_mut().collect(),
            ExprKind::Agg(agg) => {
                let Aggref { args, filter, .. } = &mut **agg;
                args.iter_mut().chain(filter.as_deref_mut()).collect()
            }
            ExprKind::Relabel(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => vec![&mut **arg],
            ExprKind::ArrayCoerce { arg, element, .. } => vec![&mut **arg, &mut **element],
            ExprKind::Subscript(sub) => {
                let Subscript { container, upper, lower } = &mut **sub;
                let bounds = upper.iter_mut().chain(lower.iter_mut().flatten()).flatten();
                bounds.chain([container]).collect()
            }
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter_mut().collect(),
            ExprKind::Case(case) => {
                let mut all: Vec<&mut Expr> = case.arg.as_deref_mut().into_iter().collect();
                for (when, then) in &mut case.whens {
                    all.push(when);
                    all.push(then);
                }
                all.push(&mut case.default);
                all
            }
        }
    }

    /// `equal` of PostgreSQL: true when the two expressions are the same apart from their locations.
    pub fn same(&self, other: &Expr) -> bool {
        fn strip(expr: &mut Expr) {
            expr.location = None;
            for child in expr.children_mut() {
                strip(child);
            }
            if let ExprKind::SubLink(sub) = &mut expr.kind {
                sub.query.each_expr_mut(0, &mut |e, _| strip(e));
            }
        }
        let (mut a, mut b) = (self.clone(), other.clone());
        strip(&mut a);
        strip(&mut b);
        a == b
    }

    /// Calls `f` for the expression and for each expression in it, also in its subqueries, with the number of subqueries between the expression and this expression. The search stops at the first `Some` that `f` gives.
    pub fn find<'a, T>(
        &'a self,
        depth: usize,
        f: &mut impl FnMut(&'a Expr, usize) -> Option<T>,
    ) -> Option<T> {
        if let Some(found) = f(self, depth) {
            return Some(found);
        }
        for child in self.children() {
            if let Some(found) = child.find(depth, f) {
                return Some(found);
            }
        }
        if let ExprKind::SubLink(sub) = &self.kind {
            for (expr, depth) in sub.query.all_exprs(depth + 1) {
                if let Some(found) = expr.find(depth, f) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// The first column of the query of the expression that the expression reads, also in a subquery, as `locate_var_of_level` finds it with level 0.
    pub fn first_var(&self) -> Option<&Expr> {
        self.find(0, &mut |e, depth| match e.kind {
            ExprKind::Var(var) if var.levels_up == depth => Some(e),
            _ => None,
        })
    }

    /// `IncrementVarSublevelsUp`: the columns of the outer queries in the expression, also in its subqueries, are `levels` more levels up. A column of a query in the expression does not change.
    pub fn raise(&mut self, levels: usize) {
        fn raise_at(expr: &mut Expr, levels: usize, depth: usize) {
            if let ExprKind::Var(var) = &mut expr.kind
                && var.levels_up >= depth
            {
                var.levels_up += levels;
            }
            for child in expr.children_mut() {
                raise_at(child, levels, depth);
            }
            if let ExprKind::SubLink(sub) = &mut expr.kind {
                sub.query.each_expr_mut(depth + 1, &mut |e, depth| raise_at(e, levels, depth));
            }
        }
        raise_at(self, levels, 0);
    }

    /// `contain_aggs_of_level` and `locate_agg_of_level`: the first aggregate call in the expression.
    pub fn first_agg(&self) -> Option<&Expr> {
        if matches!(self.kind, ExprKind::Agg(_)) {
            return Some(self);
        }
        self.children().into_iter().find_map(Expr::first_agg)
    }

    /// `expression_returns_set` and `exprLocation` of the first call: the first call of a function that gives a set in the expression, outside its subqueries.
    pub fn first_set_call(&self) -> Option<&Expr> {
        if matches!(&self.kind, ExprKind::Func(f) if f.retset) {
            return Some(self);
        }
        self.children().into_iter().find_map(Expr::first_set_call)
    }

    /// `strip_implicit_coercions`: the expression without the casts that the analyzer added.
    pub fn strip_implicit(&self) -> &Expr {
        match &self.kind {
            ExprKind::Func(Func { form: FuncForm::ImplicitCast, args, .. }) if !args.is_empty() => {
                args[0].strip_implicit()
            }
            ExprKind::Relabel(arg, CastForm::Implicit)
            | ExprKind::CoerceViaIo(arg, CastForm::Implicit)
            | ExprKind::CoerceToDomain(arg, CastForm::Implicit)
            | ExprKind::ArrayCoerce { arg, form: CastForm::Implicit, .. } => arg.strip_implicit(),
            _ => self,
        }
    }

    /// `exprLocation`: the place of the expression. For a call, an operator or a test it is the leftmost of the place of the node and the place of the first argument, as in PostgreSQL.
    pub fn place(&self) -> Option<usize> {
        let first = match &self.kind {
            ExprKind::Func(Func { args, .. })
            | ExprKind::Bool(_, args)
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. } => args.first(),
            ExprKind::Relabel(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::ArrayCoerce { arg, .. }
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => Some(&**arg),
            ExprKind::Subscript(sub) => Some(&sub.container),
            ExprKind::SubLink(sub) => sub.test.as_ref(),
            _ => None,
        };
        match (self.location, first.and_then(Expr::place)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

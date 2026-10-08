//! `Expr`, an expression of the query tree with its type, as the `Expr` nodes of `primnodes.h` after `transformExpr`.

use rupg_types::Value;

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
    Relabel(Box<Expr>),
    /// `CoerceViaIO`: the output function of the type of the argument, then the input function of the type of the expression.
    CoerceViaIo(Box<Expr>),
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
    /// `Var`: a column of a relation of `FROM`.
    Var(Var),
}

/// A column of a relation of `FROM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Var {
    /// The index of the relation in [`crate::Query::relations`].
    pub relation: usize,
    /// The attribute number: from 1 for a column of the relation, or a negative number for a system column.
    pub attnum: i16,
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
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_) => Vec::new(),
            ExprKind::Func(f) => f.args.iter().collect(),
            ExprKind::Relabel(arg)
            | ExprKind::CoerceViaIo(arg)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _) => vec![&**arg],
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
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_) => Vec::new(),
            ExprKind::Func(f) => f.args.iter_mut().collect(),
            ExprKind::Relabel(arg)
            | ExprKind::CoerceViaIo(arg)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _) => vec![&mut **arg],
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
        }
        let (mut a, mut b) = (self.clone(), other.clone());
        strip(&mut a);
        strip(&mut b);
        a == b
    }

    /// The first column that the expression reads, as `locate_var_of_level` finds it.
    pub fn first_var(&self) -> Option<&Expr> {
        if matches!(self.kind, ExprKind::Var(_)) {
            return Some(self);
        }
        self.children().into_iter().find_map(Expr::first_var)
    }

    /// `exprLocation`: the place of the expression. For a call, an operator or a test it is the leftmost of the place of the node and the place of the first argument, as in PostgreSQL.
    pub fn place(&self) -> Option<usize> {
        let first = match &self.kind {
            ExprKind::Func(Func { args, .. })
            | ExprKind::Bool(_, args)
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. } => args.first(),
            ExprKind::Relabel(arg)
            | ExprKind::CoerceViaIo(arg)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _) => Some(&**arg),
            _ => None,
        };
        match (self.location, first.and_then(Expr::place)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

// Lifted from rudb crates/rudb-qc-plan/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the tree is on PostgreSQL types. A type is the OID of `pg_type` with its `typmod`, a constant is a `rupg_types::Value`, a function is its OID of `pg_proc` with its `prosrc`, and an aggregate is its OID with its `proname`. A comparison is a function of a btree family whose two sides have one QIR type, and the narrower side of a comparison of two widths gets a `Widen`. `NOT`, `IS NULL` and `Widen` are kinds of their own, and a cast is the function of the cast. The lowering reads the expressions of `rupg-analyze`, not a plan of rudb: `expr` and `aggregate` lower the parts of a query of one table, and the executor puts the tree together. `Fetch` and the lowering of the plan nodes are not lifted, since rupg's executor plans the scan, the filter and the aggregates of a query of one table itself.

//! The relational tree that the generator compiles (document 14 section 14.12).
//!
//! The executor lowers the scan, the conditions and the aggregates of a query into a [`Rel`] tree, or [`expr`] and [`aggregate`] say why they will not, as a [`Refusal`]. The tree is smaller than the query on purpose. A column is a position in the input's output rather than a `Var`, a parameter is the constant of a custom plan, and every expression that the generator cannot translate is refused here, so that the generator never meets a type it would have to refuse halfway through writing code for it.
//!
//! A function stays the function of `pg_proc` that the analyzer chose, by its OID and its `prosrc`, such as `int4pl` or `textlike`, so the generator translates exactly the functions whose bits it knows and refuses the rest by name. A function in an ICU collation is refused, because the generator compares strings by their bytes.

use std::fmt;

use rupg_analyze::{Aggref, BoolOp, BoolTest, Expr as Analyzed, ExprKind};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::ir::Ty;
use crate::types::data_type;

/// Why the compiled engine will not run a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What was refused, for grouping the log: a node kind, a function name or a type.
    pub what: String,
    /// The sentence the log prints.
    pub why: String,
}

impl Refusal {
    /// A refusal of `what` because of `why`.
    pub fn new(what: impl Into<String>, why: impl Into<String>) -> Refusal {
        Refusal { what: what.into(), why: why.into() }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.why)
    }
}

/// A result of lowering.
pub type Result<T> = std::result::Result<T, Refusal>;

/// A PostgreSQL type: the OID of `pg_type` and the `typmod`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Type {
    /// The OID of the type.
    pub oid: u32,
    /// The `typmod`, -1 for none.
    pub typmod: i32,
}

impl Type {
    /// The type `oid` with no `typmod`.
    #[must_use]
    pub const fn new(oid: u32) -> Type {
        Type { oid, typmod: -1 }
    }

    /// The type of the value of `e`.
    #[must_use]
    pub fn of(e: &Analyzed) -> Type {
        Type { oid: e.ty, typmod: e.typmod }
    }

    /// The QIR type of the data of a value of this type, or `None` when it has none.
    #[must_use]
    pub fn data(self) -> Option<Ty> {
        data_type(self.oid)
    }

    /// The name of the type in `pg_type`.
    #[must_use]
    pub fn name(self) -> &'static str {
        builtin::type_by_oid(self.oid).map_or("unknown", |t| t.name)
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One output column of a [`Rel`].
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// The name a result set gives it.
    pub name: String,
    /// Its type.
    pub ty: Type,
}

/// An expression over the columns of one input.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    /// What it computes.
    pub kind: Kind,
    /// The type of the result.
    pub ty: Type,
}

/// The comparisons of a btree family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    /// `=`.
    Equal,
    /// `<>`.
    NotEqual,
    /// `<`.
    Less,
    /// `<=`.
    LessEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterEqual,
}

impl CompareOp {
    /// The comparison of the `prosrc` suffix `eq`, `ne`, `lt`, `le`, `gt` or `ge`.
    #[must_use]
    pub fn from_suffix(suffix: &str) -> Option<CompareOp> {
        Some(match suffix {
            "eq" => CompareOp::Equal,
            "ne" => CompareOp::NotEqual,
            "lt" => CompareOp::Less,
            "le" => CompareOp::LessEqual,
            "gt" => CompareOp::Greater,
            "ge" => CompareOp::GreaterEqual,
            _ => return None,
        })
    }

    /// The operator, as `EXPLAIN` prints it.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            CompareOp::Equal => "=",
            CompareOp::NotEqual => "<>",
            CompareOp::Less => "<",
            CompareOp::LessEqual => "<=",
            CompareOp::Greater => ">",
            CompareOp::GreaterEqual => ">=",
        }
    }
}

/// The shapes of [`Expr`].
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Column `i` of the input.
    Column(usize),
    /// A constant, possibly null.
    Constant(Value),
    /// The value of the input in the expression's wider type, which holds every value of the input exactly: `int2` to `int4` or `int8`, `int4` to `int8`, and `float4` to `float8`.
    Widen(Box<Expr>),
    /// A comparison of two values of one type.
    Compare {
        /// Which one.
        op: CompareOp,
        /// The left side.
        left: Box<Expr>,
        /// The right side.
        right: Box<Expr>,
    },
    /// `AND` of the children, with SQL's three valued logic.
    And(Vec<Expr>),
    /// `OR` of the children, with SQL's three valued logic.
    Or(Vec<Expr>),
    /// `NOT`, null for a null.
    Not(Box<Expr>),
    /// `IS NULL`, or `IS NOT NULL` when `negated`.
    IsNull {
        /// What is tested.
        input: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// A call of a built-in function.
    Function {
        /// The OID of the function in `pg_proc`.
        oid: u32,
        /// Its `prosrc`, such as `int4pl` or `textlike`, which the generator translates.
        src: &'static str,
        /// Whether a null argument gives a null without the call, `proisstrict`.
        strict: bool,
        /// The arguments.
        args: Vec<Expr>,
    },
    /// `CASE WHEN ... THEN ... ELSE ... END`.
    Case {
        /// The arms in order, each a condition and a result.
        arms: Vec<(Expr, Expr)>,
        /// The `ELSE`, null when there is none.
        otherwise: Option<Box<Expr>>,
    },
}

impl Expr {
    /// Column `i` of type `ty`.
    #[must_use]
    pub fn column(i: usize, ty: Type) -> Expr {
        Expr { kind: Kind::Column(i), ty }
    }

    /// Calls `f` on every direct child.
    pub fn children(&self, mut f: impl FnMut(&Expr)) {
        match &self.kind {
            Kind::Column(_) | Kind::Constant(_) => {}
            Kind::Widen(input) | Kind::Not(input) | Kind::IsNull { input, .. } => f(input),
            Kind::Compare { left, right, .. } => {
                f(left);
                f(right);
            }
            Kind::And(c) | Kind::Or(c) | Kind::Function { args: c, .. } => c.iter().for_each(f),
            Kind::Case { arms, otherwise } => {
                for (w, t) in arms {
                    f(w);
                    f(t);
                }
                if let Some(o) = otherwise {
                    f(o);
                }
            }
        }
    }

    /// The same expression with every column `i` replaced by `with[i]`.
    #[must_use]
    pub fn substitute(&self, with: &[Expr]) -> Expr {
        let sub = |e: &Expr| Box::new(e.substitute(with));
        let all = |c: &[Expr]| c.iter().map(|e| e.substitute(with)).collect();
        let kind = match &self.kind {
            Kind::Column(i) => return with[*i].clone(),
            Kind::Constant(v) => Kind::Constant(v.clone()),
            Kind::Widen(input) => Kind::Widen(sub(input)),
            Kind::Compare { op, left, right } => {
                Kind::Compare { op: *op, left: sub(left), right: sub(right) }
            }
            Kind::And(c) => Kind::And(all(c)),
            Kind::Or(c) => Kind::Or(all(c)),
            Kind::Not(input) => Kind::Not(sub(input)),
            Kind::IsNull { input, negated } => {
                Kind::IsNull { input: sub(input), negated: *negated }
            }
            Kind::Function { oid, src, strict, args } => {
                Kind::Function { oid: *oid, src, strict: *strict, args: all(args) }
            }
            Kind::Case { arms, otherwise } => Kind::Case {
                arms: arms.iter().map(|(w, t)| (w.substitute(with), t.substitute(with))).collect(),
                otherwise: otherwise.as_ref().map(|o| sub(o)),
            },
        };
        Expr { kind, ty: self.ty }
    }

    /// The columns the expression reads, each once, in the order first read.
    #[must_use]
    pub fn columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<usize>) {
        if let Kind::Column(i) = self.kind
            && !out.contains(&i)
        {
            out.push(i);
        }
        self.children(|c| c.collect(out));
    }
}

/// One aggregate call.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregate {
    /// The function: `count_star` for `count(*)`, else the `proname`, such as `sum` or `min`.
    pub name: &'static str,
    /// The OID of the aggregate in `pg_proc`.
    pub oid: u32,
    /// The arguments.
    pub args: Vec<Expr>,
    /// `DISTINCT`.
    pub distinct: bool,
    /// `FILTER (WHERE ...)`.
    pub filter: Option<Expr>,
    /// The result type.
    pub ty: Type,
}

/// One ordering key.
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    /// What is ordered by.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST`.
    pub nulls_first: bool,
}

/// Which input of a hash join fills the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSide {
    /// The left input.
    Left,
    /// The right input.
    Right,
}

/// A relational operator of the physical plan. Every operator's output is a list of columns, and an expression above it names them by position.
#[derive(Clone, Debug, PartialEq)]
pub enum Rel {
    /// A table, whose columns the executor hands the compiled code a morsel at a time.
    Scan {
        /// The OID of the table.
        relation: u32,
        /// The table, for messages.
        table: String,
        /// The attribute number of each column the scan produces.
        attnums: Vec<i16>,
        /// The columns the scan produces.
        columns: Vec<Column>,
    },
    /// Literal rows.
    Values {
        /// The rows.
        rows: Vec<Vec<Value>>,
        /// The columns.
        columns: Vec<Column>,
    },
    /// Keeps the rows where the predicate is true.
    Filter {
        /// The input.
        input: Box<Rel>,
        /// The predicate.
        predicate: Expr,
    },
    /// New columns computed from the input's.
    Project {
        /// The input.
        input: Box<Rel>,
        /// The expressions.
        exprs: Vec<Expr>,
        /// The output columns.
        columns: Vec<Column>,
    },
    /// Grouping. The output is the groups and then the aggregates.
    Aggregate {
        /// The input.
        input: Box<Rel>,
        /// The group expressions.
        groups: Vec<Expr>,
        /// The aggregate calls.
        aggregates: Vec<Aggregate>,
        /// The output columns.
        columns: Vec<Column>,
        /// Whether the one key arrives in runs.
        runs: Option<Runs>,
    },
    /// All the rows in order.
    Sort {
        /// The input.
        input: Box<Rel>,
        /// The keys, first key first.
        keys: Vec<Key>,
    },
    /// The first `count` rows in order after skipping `offset`.
    TopN {
        /// The input.
        input: Box<Rel>,
        /// The keys, first key first.
        keys: Vec<Key>,
        /// How many rows.
        count: u64,
        /// How many to skip.
        offset: u64,
    },
    /// Some of the rows, in the order they arrive.
    Limit {
        /// The input.
        input: Box<Rel>,
        /// How many rows, all of them when `None`.
        count: Option<u64>,
        /// How many to skip.
        offset: u64,
    },
    /// An equi-join by a hash table, fused into the pipelines: the build side fills the table and the other side probes it inline. The output of an inner join is the left columns and then the right ones, whichever side builds, and that of a semi or anti join is the left columns.
    Join {
        /// The left input.
        left: Box<Rel>,
        /// The right input.
        right: Box<Rel>,
        /// Which input fills the table.
        build: BuildSide,
        /// Which left rows come out: every match, or each row once when it has a match or when it has none.
        kind: JoinType,
        /// The equalities, each a key over the left input's columns and one over the right input's, both of the same type. There is at least one.
        keys: Vec<(Expr, Expr)>,
        /// The rest of the condition, over the left columns and then the right ones, all of which must be true.
        residual: Vec<Expr>,
        /// The output columns.
        columns: Vec<Column>,
    },
}

/// Which left rows a hash join lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    /// Each left row once per right row it matches, paired with it.
    Inner,
    /// Each left row with at least one match, once.
    Semi,
    /// Each left row with no match, including one with a null key.
    Anti,
}

impl JoinType {
    /// The spelling the refusals and `EXPLAIN` use.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            JoinType::Inner => "INNER",
            JoinType::Semi => "SEMI",
            JoinType::Anti => "ANTI",
        }
    }
}

/// How the rows of an aggregate's one key arrive, when the store says each value's rows are one run, so a group is finished once the key moves past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runs {
    /// The key never goes down, which the driver checks a morsel at a time.
    Ascending,
    /// Each value's rows are together, the runs in no order.
    Grouped,
}

impl Rel {
    /// The output columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Rel::Scan { columns, .. }
            | Rel::Values { columns, .. }
            | Rel::Project { columns, .. }
            | Rel::Aggregate { columns, .. }
            | Rel::Join { columns, .. } => columns,
            Rel::Filter { input, .. }
            | Rel::Sort { input, .. }
            | Rel::TopN { input, .. }
            | Rel::Limit { input, .. } => input.columns(),
        }
    }

    /// The operator's name, as `EXPLAIN` prints it.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Rel::Scan { .. } => "Scan",
            Rel::Values { .. } => "Values",
            Rel::Filter { .. } => "Filter",
            Rel::Project { .. } => "Project",
            Rel::Aggregate { .. } => "Aggregate",
            Rel::Sort { .. } => "Sort",
            Rel::TopN { .. } => "TopN",
            Rel::Limit { .. } => "Limit",
            Rel::Join { .. } => "HashJoin",
        }
    }

    /// A filter of `input` by all of `conds`, or `input` itself when there are none.
    #[must_use]
    pub fn filtered(input: Rel, mut conds: Vec<Expr>) -> Rel {
        let predicate = match conds.len() {
            0 => return input,
            1 => conds.remove(0),
            _ => Expr { kind: Kind::And(conds), ty: Type::new(oid::BOOL) },
        };
        Rel::Filter { input: Box::new(input), predicate }
    }
}

/// Whether the compiled engine has a representation for values of `ty`: a type whose values fit one machine value ([`crate::types::data_type`]).
#[must_use]
pub fn supported(ty: Type) -> bool {
    ty.data().is_some()
}

/// The aggregates the generator has an accumulator for.
pub const AGGREGATES: &[&str] = &["count_star", "count", "sum", "avg", "min", "max"];

/// Whether the collation `collation` of a call orders strings by their bytes, as the generator does: none, `default`, `C` or `POSIX`, the collations that the executor of rupg orders by their bytes too.
#[must_use]
pub fn plain(collation: u32) -> bool {
    matches!(collation, 0 | 100 | 950 | 951)
}

/// What an expression of a query of one table reads.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    /// The index of the table in the relations of the query.
    pub relation: usize,
    /// The position in the scan of each column of the table, by its attribute number from 1, or `usize::MAX` for a column the scan does not read.
    pub places: &'a [usize],
    /// The values of the parameters, which a custom plan takes as constants.
    pub params: &'a [Value],
}

/// Lowers the expression `e` of a query of one table, whose columns are the columns of the scan of `scope`, or says why not.
///
/// # Errors
///
/// A [`Refusal`] naming the first part of `e` the compiled engine does not run.
pub fn expr(e: &Analyzed, scope: &Scope<'_>) -> Result<Expr> {
    let ty = Type::of(e);
    check_type(ty)?;
    let list = |es: &[Analyzed]| es.iter().map(|e| expr(e, scope)).collect::<Result<Vec<_>>>();
    let kind = match &e.kind {
        ExprKind::Var(var) => {
            if var.levels_up != 0 || var.relation != scope.relation {
                return Err(Refusal::new("column", "a column of another query or relation"));
            }
            let place = usize::try_from(var.attnum - 1)
                .ok()
                .and_then(|i| scope.places.get(i))
                .copied()
                .filter(|&p| p != usize::MAX);
            match place {
                Some(place) => Kind::Column(place),
                None => {
                    return Err(Refusal::new(
                        "column",
                        format!("attribute {} is not a column of the scan", var.attnum),
                    ));
                }
            }
        }
        ExprKind::Const(v) => Kind::Constant(v.clone()),
        ExprKind::Param(n) => match n.checked_sub(1).and_then(|i| scope.params.get(i)) {
            Some(v) => Kind::Constant(v.clone()),
            None => return Err(Refusal::new("parameter", format!("${n} has no value"))),
        },
        // A relabel keeps the bits of the value, so the expression is the input's with the new type when both have one QIR type.
        ExprKind::Relabel(input, _) => {
            let input = expr(input, scope)?;
            if input.ty.data() != ty.data() {
                return Err(Refusal::new(
                    format!("type {}", ty),
                    format!("a relabel of {} has another machine type", input.ty),
                ));
            }
            return Ok(Expr { kind: input.kind, ty });
        }
        // The collation is the one the call reads, which `Func::collation` gives.
        ExprKind::Collate(input, _) => return expr(input, scope),
        ExprKind::Func(f) => {
            if f.retset || f.variadic {
                return Err(Refusal::new(
                    format!("function {}", f.oid),
                    "a set returning or variadic call",
                ));
            }
            let Some(proc) = builtin::proc_by_oid(f.oid) else {
                return Err(Refusal::new(format!("function {}", f.oid), "not a built-in function"));
            };
            if proc.volatile == b'v' {
                return Err(Refusal::new(format!("function {}", proc.name), "a volatile function"));
            }
            if !plain(f.collation) {
                return Err(Refusal::new(
                    format!("function {}", proc.name),
                    "a call in a collation that does not order strings by their bytes",
                ));
            }
            let args = list(&f.args)?;
            match comparison(proc.src) {
                Some(op) if args.len() == 2 => compare(op, args)?,
                _ => Kind::Function { oid: f.oid, src: proc.src, strict: proc.strict, args },
            }
        }
        ExprKind::Bool(BoolOp::And, args) => Kind::And(list(args)?),
        ExprKind::Bool(BoolOp::Or, args) => Kind::Or(list(args)?),
        ExprKind::Bool(BoolOp::Not, args) => match args.as_slice() {
            [arg] => Kind::Not(Box::new(expr(arg, scope)?)),
            _ => return Err(Refusal::new("NOT", "a NOT of other than one argument")),
        },
        ExprKind::NullTest(input, is_null) => {
            Kind::IsNull { input: Box::new(expr(input, scope)?), negated: !is_null }
        }
        ExprKind::BooleanTest(input, test) => boolean_test(expr(input, scope)?, *test),
        ExprKind::Case(case) => {
            if case.arg.is_some() {
                return Err(Refusal::new("CASE", "a CASE with an operand"));
            }
            let mut arms = Vec::with_capacity(case.whens.len());
            for (when, then) in &case.whens {
                arms.push((expr(when, scope)?, expr(then, scope)?));
            }
            let otherwise = match &case.default.kind {
                ExprKind::Const(Value::Null) => None,
                _ => Some(Box::new(expr(&case.default, scope)?)),
            };
            Kind::Case { arms, otherwise }
        }
        // `COALESCE(a, b, c)` is `CASE WHEN a IS NOT NULL THEN a WHEN b IS NOT NULL THEN b ELSE c END`, and the arguments have no side effect.
        ExprKind::Coalesce(args) => {
            let mut args = list(args)?;
            let Some(last) = args.pop() else {
                return Err(Refusal::new("COALESCE", "a COALESCE of no argument"));
            };
            let arms = args
                .into_iter()
                .map(|a| {
                    let test = Kind::IsNull { input: Box::new(a.clone()), negated: true };
                    (Expr { kind: test, ty: Type::new(oid::BOOL) }, a)
                })
                .collect();
            Kind::Case { arms, otherwise: Some(Box::new(last)) }
        }
        other => {
            return Err(Refusal::new("expression", format!("{} is not compiled", name(other))));
        }
    };
    Ok(Expr { kind, ty })
}

/// Lowers the aggregate call `a`, whose result has the type `ty`, of a query of one table, or says why not.
///
/// # Errors
///
/// A [`Refusal`] naming the aggregate or the first part of its arguments the compiled engine does not run.
pub fn aggregate(a: &Aggref, ty: Type, scope: &Scope<'_>) -> Result<Aggregate> {
    let Some(proc) = builtin::proc_by_oid(a.oid) else {
        return Err(Refusal::new(format!("aggregate {}", a.oid), "not a built-in aggregate"));
    };
    let name = if a.star { "count_star" } else { proc.name };
    let refuse = |why: &str| Err(Refusal::new(format!("aggregate {name}"), why));
    if !AGGREGATES.contains(&name) {
        return refuse("the generator has no accumulator for it");
    }
    if a.kind != b'n' || a.levels_up != 0 || a.variadic || !a.order.is_empty() {
        return refuse("an ordered, outer or variadic call");
    }
    if !plain(a.collation) {
        return refuse("a call in a collation that does not order strings by their bytes");
    }
    let args = a.args[..a.nargs.min(a.args.len())]
        .iter()
        .map(|e| expr(e, scope))
        .collect::<Result<Vec<_>>>()?;
    let distinct = !a.distinct.is_empty();
    if distinct && name != "count" {
        return refuse("DISTINCT is only compiled for count");
    }
    // `sum` of an `int8` and `avg` of an integer give a `numeric`, which the driver makes from
    // the exact sum and the count.
    let integer = |args: &[Expr]| {
        args.first().is_some_and(|e| matches!(e.ty.oid, oid::INT2 | oid::INT4 | oid::INT8))
    };
    let numeric = ty.oid == oid::NUMERIC && matches!(name, "sum" | "avg") && integer(&args);
    if !numeric {
        check_type(ty)?;
    }
    let filter = a.filter.as_deref().map(|f| expr(f, scope)).transpose()?;
    Ok(Aggregate { name, oid: a.oid, args, distinct, filter, ty })
}

/// The comparison that a function with the `prosrc` `src` makes, when it is a comparison of a btree family whose two sides have one QIR type, or of two integer or two float widths. `oid`, `"char"` and `uuid` are not in the list, as they compare without a sign, which the signed QIR integers do not.
fn comparison(src: &str) -> Option<CompareOp> {
    let (head, suffix) = src.split_at_checked(src.len().checked_sub(2)?)?;
    let op = CompareOp::from_suffix(suffix)?;
    let head = head.strip_suffix('_').unwrap_or(head);
    matches!(
        head,
        "int2"
            | "int4"
            | "int8"
            | "int24"
            | "int42"
            | "int28"
            | "int82"
            | "int48"
            | "int84"
            | "float4"
            | "float8"
            | "float48"
            | "float84"
            | "date"
            | "timestamp"
            | "time"
            | "bool"
            | "text"
            | "bytea"
    )
    .then_some(op)
}

/// The rank of the width of an integer or a float type, so that a comparison widens the narrower side, or `None` for another type.
fn width(ty: u32) -> Option<(bool, u8)> {
    Some(match ty {
        oid::INT2 => (false, 0),
        oid::INT4 => (false, 1),
        oid::INT8 => (false, 2),
        oid::FLOAT4 => (true, 0),
        oid::FLOAT8 => (true, 1),
        _ => return None,
    })
}

/// The comparison `op` of the two arguments `args`, with the narrower one widened to the type of the other.
fn compare(op: CompareOp, args: Vec<Expr>) -> Result<Kind> {
    let [mut left, mut right]: [Expr; 2] =
        args.try_into().map_err(|_| Refusal::new("comparison", "not two arguments"))?;
    if left.ty.data() != right.ty.data() || left.ty.oid != right.ty.oid {
        match (width(left.ty.oid), width(right.ty.oid)) {
            (Some((lf, l)), Some((rf, r))) if lf == rf && l < r => left = widen(left, right.ty),
            (Some((lf, l)), Some((rf, r))) if lf == rf && r < l => right = widen(right, left.ty),
            _ if left.ty.data() == right.ty.data() => {}
            _ => {
                return Err(Refusal::new(
                    "comparison",
                    format!("a comparison of {} with {}", left.ty, right.ty),
                ));
            }
        }
    }
    Ok(Kind::Compare { op, left: Box::new(left), right: Box::new(right) })
}

/// `e` in the wider type `to`.
fn widen(e: Expr, to: Type) -> Expr {
    Expr { kind: Kind::Widen(Box::new(e)), ty: Type::new(to.oid) }
}

/// `input IS [NOT] TRUE`, `FALSE` or `UNKNOWN` as a `CASE` or a null test, which give false and not null for a null.
fn boolean_test(input: Expr, test: BoolTest) -> Kind {
    let boolean = Type::new(oid::BOOL);
    let constant = |b: bool| Expr { kind: Kind::Constant(Value::Bool(b)), ty: boolean };
    let not = |e: Expr| Expr { kind: Kind::Not(Box::new(e)), ty: boolean };
    let case = |when: Expr, then: bool| Kind::Case {
        arms: vec![(when, constant(then))],
        otherwise: Some(Box::new(constant(!then))),
    };
    match test {
        BoolTest::IsTrue => case(input, true),
        BoolTest::IsNotTrue => case(input, false),
        BoolTest::IsFalse => case(not(input), true),
        BoolTest::IsNotFalse => case(not(input), false),
        BoolTest::IsUnknown => Kind::IsNull { input: Box::new(input), negated: false },
        BoolTest::IsNotUnknown => Kind::IsNull { input: Box::new(input), negated: true },
    }
}

fn check_type(ty: Type) -> Result<()> {
    if supported(ty) || ty.oid == oid::UNKNOWN {
        Ok(())
    } else {
        Err(Refusal::new(format!("type {ty}"), "the compiled engine has no representation for it"))
    }
}

/// The name of the node of an expression, for a refusal.
fn name(kind: &ExprKind) -> String {
    let text = format!("{kind:?}");
    text.split([' ', '{', '(']).next().unwrap_or("node").to_owned()
}

/// A join key over the left input and the matching one over the right.
type KeyPair = (Expr, Expr);

/// Adds the conjuncts of `e` to `out`, looking through nested `AND`s.
pub fn flatten(e: Expr, out: &mut Vec<Expr>) {
    match e.kind {
        Kind::And(children) => children.into_iter().for_each(|c| flatten(c, out)),
        kind => out.push(Expr { kind, ty: e.ty }),
    }
}

/// Splits a join condition over the left columns and then the right ones into the key pairs a hash join looks up by and the rest. An equality is a key when one side reads only left columns and the other only right columns, and both read at least one. The right key is rebased onto the right input's own columns.
///
/// # Errors
///
/// A [`Refusal`] when the condition has no such equality, or a key of a float type, which a hash table would compare by its bits.
pub fn split(
    conjuncts: Vec<Expr>,
    left: usize,
    right: &[Column],
) -> Result<(Vec<KeyPair>, Vec<Expr>)> {
    let side = |e: &Expr| {
        let cs = e.columns();
        if cs.is_empty() {
            None
        } else if cs.iter().all(|&c| c < left) {
            Some(false)
        } else if cs.iter().all(|&c| c >= left) {
            Some(true)
        } else {
            None
        }
    };
    let rebase: Vec<Expr> = (0..left)
        .map(|_| Expr { kind: Kind::Constant(Value::Null), ty: Type::new(oid::UNKNOWN) })
        .chain(right.iter().enumerate().map(|(i, c)| Expr::column(i, c.ty)))
        .collect();
    let mut keys = Vec::new();
    let mut residual = Vec::new();
    for c in conjuncts {
        if let Kind::Compare { op: CompareOp::Equal, left: a, right: b } = &c.kind {
            let pair = match (side(a), side(b)) {
                (Some(false), Some(true)) => Some((a, b)),
                (Some(true), Some(false)) => Some((b, a)),
                _ => None,
            };
            if let Some((l, r)) = pair {
                if matches!(l.ty.data(), Some(Ty::F32 | Ty::F64)) {
                    return Err(Refusal::new(
                        "Join",
                        format!(
                            "a key of type {}, which the table would compare by its bits",
                            l.ty
                        ),
                    ));
                }
                keys.push(((**l).clone(), r.substitute(&rebase)));
                continue;
            }
        }
        residual.push(c);
    }
    if keys.is_empty() {
        return Err(Refusal::new("Join", "the condition has no equality between the two sides"));
    }
    Ok((keys, residual))
}

#[cfg(test)]
mod tests;

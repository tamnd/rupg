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
    /// `NamedArgExpr`: an argument of a call that the call gives by its name, as `name => value`. The planner puts the arguments in the order of the function.
    NamedArg(Box<NamedArg>),
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
    /// `RowExpr`: `ROW(...)`, a value of `record` with a field for each argument. The form is explicit for a cast of a row to a composite type, as `row_format`.
    Row(Vec<Expr>, CastForm),
    /// The whole row of a join, as the `Var` with the attribute number 0 of a join, which the planner of PostgreSQL makes a `RowExpr` with the names of the columns. A join with no relation of its own gives the columns of the join as the fields.
    JoinRow(Box<JoinRow>),
    /// `RowCompareExpr`: `(a, b) < (c, d)` and the other comparisons of two rows with an ordering operator.
    RowCompare(Box<RowCompare>),
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
    /// `WindowFunc`: a call of a window function, which reads the rows of its window.
    Window(Box<WindowFunc>),
    /// `GroupingFunc`: a call of `GROUPING`, of type `integer`.
    Grouping(Box<GroupingFunc>),
    /// `SubLink`: a subquery in an expression.
    SubLink(Box<SubLink>),
    /// `Param` of the kind `PARAM_SUBLINK`: the value of the column with this index, from 0, of a row of the subquery, in the test of `ANY` or `ALL`.
    SubColumn(usize),
    /// `JsonValueExpr`: an input of a SQL/JSON function with its `FORMAT` clause, as the value of the formatted expression.
    JsonValue(Box<JsonValue>),
    /// `JsonConstructorExpr`: `JSON_OBJECT`, `JSON_ARRAY`, `JSON`, `JSON_SCALAR` or `JSON_SERIALIZE`.
    JsonCtor(Box<JsonCtor>),
    /// `JsonIsPredicate`: `IS JSON`, of type `boolean`.
    IsJson(Box<IsJson>),
    /// `JsonExpr`: `JSON_EXISTS`, `JSON_QUERY` or `JSON_VALUE`.
    JsonExpr(Box<JsonExpr>),
}

/// The whole row of a join, or of the alias of `USING`.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinRow {
    /// The alias of the join, which the deparser shows as `alias.*`. `None` for the alias of `USING`, which is a `RowExpr` in PostgreSQL and shows as `ROW(...)`.
    pub alias: Option<String>,
    /// The name of each field, which is the name of the column of the join.
    pub names: Vec<String>,
    /// The value of each field.
    pub fields: Vec<Expr>,
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
    /// `testexpr`: for `ANY`, `ALL` and a row comparison, the test of each row of the subquery. It compares the left side with [`ExprKind::SubColumn`].
    pub test: Option<Expr>,
    pub query: Query,
}

/// The comparison of two rows with an ordering operator, as `RowCompareExpr`.
#[derive(Clone, Debug, PartialEq)]
pub struct RowCompare {
    pub kind: RowCompareKind,
    /// The operator call for each pair of fields, with the field of the left row and the field of the right row as its arguments.
    pub ops: Vec<Expr>,
    /// The btree comparison function of each pair of fields, from the operator family of the operator.
    pub compares: Vec<u32>,
}

/// The comparison of a [`RowCompare`], as `rctype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCompareKind {
    Less,
    LessEqual,
    GreaterEqual,
    Greater,
}

impl RowCompareKind {
    /// The kind of a btree strategy number, or `None` for `=` and the numbers that are not a strategy.
    pub fn of_strategy(strategy: u32) -> Option<RowCompareKind> {
        match strategy {
            1 => Some(RowCompareKind::Less),
            2 => Some(RowCompareKind::LessEqual),
            4 => Some(RowCompareKind::GreaterEqual),
            5 => Some(RowCompareKind::Greater),
            _ => None,
        }
    }

    /// The result of the comparison for the result of a btree comparison function: below 0, 0 or above 0.
    pub fn test(self, order: i32) -> bool {
        match self {
            RowCompareKind::Less => order < 0,
            RowCompareKind::LessEqual => order <= 0,
            RowCompareKind::GreaterEqual => order >= 0,
            RowCompareKind::Greater => order > 0,
        }
    }

    /// The name of the kind in the text of a stored expression.
    pub fn name(self) -> &'static str {
        match self {
            RowCompareKind::Less => "lt",
            RowCompareKind::LessEqual => "le",
            RowCompareKind::GreaterEqual => "ge",
            RowCompareKind::Greater => "gt",
        }
    }

    /// The kind with this name, as [`RowCompareKind::name`] gives it.
    pub fn of_name(name: &str) -> Option<RowCompareKind> {
        [
            RowCompareKind::Less,
            RowCompareKind::LessEqual,
            RowCompareKind::GreaterEqual,
            RowCompareKind::Greater,
        ]
        .into_iter()
        .find(|kind| kind.name() == name)
    }
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
    /// `(a, b) op (SELECT ...)`, which gives the result of the test on the one row.
    RowCompare,
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

/// An argument by its name, as `NamedArgExpr`.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedArg {
    pub arg: Expr,
    pub name: String,
    /// `argnumber`: the position of the argument in the input arguments of the function, from 0.
    pub number: usize,
}

/// A call of an aggregate function, as `Aggref`.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggref {
    /// `aggfnoid`: the OID of the aggregate function in `pg_proc`.
    pub oid: u32,
    /// `aggkind`: `n` for a normal aggregate, `o` for an ordered-set aggregate or `h` for a hypothetical-set aggregate.
    pub kind: u8,
    /// `aggdirectargs`: the direct arguments of an ordered-set aggregate, which the aggregate reads once for each group.
    pub direct: Vec<Expr>,
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
    /// `agglevelsup`: 0 for an aggregate of the query that has the call, 1 for an aggregate of the query outside it, and so on. The arguments of an aggregate of an outer query read only the columns of that query and of the queries outside it.
    pub levels_up: usize,
}

/// A call of `GROUPING`, as `GroupingFunc`. The result has a bit for each argument, with the bit of the last argument as the lowest bit. A bit is 1 when the grouping set of the group does not have the argument.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupingFunc {
    /// The arguments, which are items of `GROUP BY`.
    pub args: Vec<Expr>,
    /// `refs`: the target of the item of `GROUP BY` of each argument. `parseCheckAggregates` sets them.
    pub refs: Vec<usize>,
    /// `agglevelsup`: 0 for a call of the query that has the call, 1 for a call of the query outside it, and so on.
    pub levels_up: usize,
}

/// `JsonFormatType`: the format of a `FORMAT` clause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JsonFormatKind {
    /// No `FORMAT` clause.
    #[default]
    Default,
    /// `FORMAT JSON`.
    Json,
    /// The format of a `jsonb` value, which no clause can give.
    Jsonb,
}

/// `JsonEncoding`: the `ENCODING` of a `FORMAT JSON` clause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JsonEncoding {
    /// No `ENCODING` clause.
    #[default]
    Default,
    Utf8,
    Utf16,
    Utf32,
}

impl JsonEncoding {
    /// The name of the encoding in the SQL text.
    pub fn name(self) -> &'static str {
        match self {
            JsonEncoding::Default | JsonEncoding::Utf8 => "UTF8",
            JsonEncoding::Utf16 => "UTF16",
            JsonEncoding::Utf32 => "UTF32",
        }
    }
}

/// `JsonFormat`: a `FORMAT` clause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonFormat {
    pub kind: JsonFormatKind,
    pub encoding: JsonEncoding,
}

/// `JsonValueExpr`: an input of a SQL/JSON function. The executor reads `formatted`, and the deparser shows `raw` and the format.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonValue {
    /// The input as the query wrote it.
    pub raw: Expr,
    /// The input as a value of `json` or `jsonb`, or as `text` for an input of `bytea`.
    pub formatted: Expr,
    pub format: JsonFormat,
}

/// `JsonConstructorType`: the function of a [`JsonCtor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonCtorKind {
    /// `JSON_OBJECT`. The arguments are the keys and the values, one after the other.
    Object,
    /// `JSON_ARRAY` with a list of values.
    Array,
    /// `JSON`, which checks that a text is JSON.
    Parse,
    /// `JSON_SCALAR`, which makes a JSON value from a SQL value.
    Scalar,
    /// `JSON_SERIALIZE`, which makes a string from a JSON value.
    Serialize,
    /// `JSON_OBJECTAGG`. The call of the aggregate is [`JsonCtor::func`].
    ObjectAgg,
    /// `JSON_ARRAYAGG`. The call of the aggregate is [`JsonCtor::func`].
    ArrayAgg,
    /// `JSON_ARRAY` with a subquery. [`JsonCtor::func`] is `COALESCE((SELECT JSON_ARRAYAGG(q.a) FROM (subquery) q(a)), '[]')`.
    ArrayQuery,
}

/// `JsonReturning`: the type of the result of a SQL/JSON function and its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonReturning {
    pub format: JsonFormat,
    pub ty: u32,
    pub typmod: i32,
}

/// `JsonConstructorExpr`: a SQL/JSON constructor.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonCtor {
    pub kind: JsonCtorKind,
    pub args: Vec<Expr>,
    /// The expression that gives the value of `JSON_OBJECTAGG`, `JSON_ARRAYAGG` and `JSON_ARRAY` with a subquery.
    pub func: Option<Expr>,
    /// The `FORMAT` clause of the subquery of `JSON_ARRAY`.
    pub format: JsonFormat,
    /// The `RETURNING` clause, or the type that the analyzer gives when the call has no clause.
    pub returning: JsonReturning,
    /// `WITH UNIQUE KEYS`.
    pub unique: bool,
    /// `ABSENT ON NULL`.
    pub absent_on_null: bool,
    /// The cast of the `json` or `jsonb` value to the type of `RETURNING`. [`ExprKind::CaseTest`] is the value.
    pub coercion: Option<Expr>,
}

/// `JsonValueType`: the kind of JSON value that `IS JSON` accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonItemType {
    /// `IS JSON` or `IS JSON VALUE`.
    Any,
    Object,
    Array,
    Scalar,
}

/// `JsonIsPredicate`: `IS JSON`.
#[derive(Clone, Debug, PartialEq)]
pub struct IsJson {
    /// The value, of type `text`, `json` or `jsonb`.
    pub expr: Expr,
    pub item: JsonItemType,
    /// `WITH UNIQUE KEYS`.
    pub unique: bool,
    /// `exprBaseType`: the type of the value, `text`, `json` or `jsonb`.
    pub base_type: u32,
}

/// `JsonExprOp`: the function of a [`JsonExpr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonExprOp {
    /// `JSON_EXISTS`, which gives true when the path finds an item.
    Exists,
    /// `JSON_QUERY`, which gives the item that the path finds as JSON.
    Query,
    /// `JSON_VALUE`, which gives the scalar item that the path finds as a SQL value.
    Value,
}

/// `JsonWrapper`: the `WRAPPER` clause of `JSON_QUERY`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JsonWrapper {
    /// No clause, which is the same as `WITHOUT WRAPPER`.
    #[default]
    Unspec,
    /// `WITHOUT WRAPPER`.
    None,
    /// `WITH CONDITIONAL WRAPPER`: an array only for more than one item.
    Conditional,
    /// `WITH UNCONDITIONAL WRAPPER`: always an array.
    Unconditional,
}

/// `JsonBehaviorType`: what a SQL/JSON function gives on an error or when the path finds no item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonBehaviorKind {
    Null,
    Error,
    Empty,
    True,
    False,
    Unknown,
    EmptyArray,
    EmptyObject,
    Default,
}

impl JsonBehaviorKind {
    /// The name of the behavior as `ON ERROR` and `ON EMPTY` show it.
    pub fn name(self) -> &'static str {
        match self {
            JsonBehaviorKind::Null => "NULL",
            JsonBehaviorKind::Error => "ERROR",
            JsonBehaviorKind::Empty => "EMPTY",
            JsonBehaviorKind::True => "TRUE",
            JsonBehaviorKind::False => "FALSE",
            JsonBehaviorKind::Unknown => "UNKNOWN",
            JsonBehaviorKind::EmptyArray => "EMPTY ARRAY",
            JsonBehaviorKind::EmptyObject => "EMPTY OBJECT",
            JsonBehaviorKind::Default => "DEFAULT",
        }
    }
}

/// `JsonBehavior`: an `ON ERROR` or an `ON EMPTY` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonBehavior {
    pub kind: JsonBehaviorKind,
    /// The value of the behavior, or `None` for `ERROR`.
    pub expr: Option<Expr>,
    /// True when the executor casts the value to the type of `RETURNING` at run time, as `json_populate_type` does.
    pub coerce: bool,
}

/// `JsonExpr`: `JSON_EXISTS`, `JSON_QUERY` or `JSON_VALUE`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonExpr {
    pub op: JsonExprOp,
    /// The context item as a `jsonb` value.
    pub formatted: Expr,
    /// The path, of type `jsonpath`.
    pub path: Expr,
    /// The names and the values of the `PASSING` clause.
    pub passing: Vec<(String, Expr)>,
    pub returning: JsonReturning,
    /// `ON EMPTY`, which `JSON_EXISTS` does not have.
    pub on_empty: Option<JsonBehavior>,
    pub on_error: JsonBehavior,
    pub wrapper: JsonWrapper,
    /// `OMIT QUOTES`, which is always true for `JSON_VALUE`.
    pub omit_quotes: bool,
    /// True when the executor casts the result to the type of `RETURNING` as `json_populate_type` does.
    pub json_coercion: bool,
    /// True when the executor casts the text of the result to the type of `RETURNING` with its input function.
    pub io_coercion: bool,
}

impl JsonExpr {
    /// The context item, the path, the values of `PASSING` and the values of the behaviors.
    pub fn children(&self) -> Vec<&Expr> {
        let mut all = vec![&self.formatted, &self.path];
        all.extend(self.passing.iter().map(|(_, e)| e));
        let behaviors = self.on_empty.iter().chain([&self.on_error]);
        all.extend(behaviors.filter_map(|b| b.expr.as_ref()));
        all
    }

    /// [`JsonExpr::children`], to change.
    pub fn children_mut(&mut self) -> Vec<&mut Expr> {
        let JsonExpr { formatted, path, passing, on_empty, on_error, .. } = self;
        let mut all = vec![formatted, path];
        all.extend(passing.iter_mut().map(|(_, e)| e));
        all.extend(on_empty.iter_mut().chain([on_error]).filter_map(|b| b.expr.as_mut()));
        all
    }
}

/// A call of a window function, as `WindowFunc`.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowFunc {
    /// `winfnoid`: the OID of the function in `pg_proc`.
    pub oid: u32,
    pub args: Vec<Expr>,
    /// `winref`: the number of the window in [`crate::Query::windows`], from 1.
    pub winref: usize,
    /// `winstar`: true for `f(*)`.
    pub star: bool,
    /// `winagg`: true for an aggregate function.
    pub agg: bool,
    /// `aggfilter`: the condition of `FILTER (WHERE ...)` of an aggregate.
    pub filter: Option<Box<Expr>>,
    /// `ignore_nulls`: the clause `RESPECT NULLS` or `IGNORE NULLS` of the call.
    pub nulls: NullTreatment,
}

/// The null treatment of a call of a window function, as `ignore_nulls`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NullTreatment {
    /// No clause.
    #[default]
    None,
    /// `IGNORE NULLS`: `lag`, `lead`, `first_value`, `last_value` and `nth_value` skip the rows where the argument is null.
    Ignore,
    /// `RESPECT NULLS`, which is the same as no clause for the functions that accept it.
    Respect,
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
            ExprKind::RowCompare(rc) => rc.ops.iter().collect(),
            ExprKind::Func(f) => f.args.iter().collect(),
            ExprKind::NamedArg(na) => vec![&na.arg],
            ExprKind::Agg(agg) => {
                agg.direct.iter().chain(&agg.args).chain(agg.filter.as_deref()).collect()
            }
            ExprKind::Window(w) => w.args.iter().chain(w.filter.as_deref()).collect(),
            ExprKind::Grouping(g) => g.args.iter().collect(),
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
            | ExprKind::Row(args, _)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter().collect(),
            ExprKind::JoinRow(row) => row.fields.iter().collect(),
            ExprKind::JsonValue(v) => vec![&v.raw, &v.formatted],
            ExprKind::JsonCtor(c) => c.args.iter().chain(&c.func).chain(&c.coercion).collect(),
            ExprKind::IsJson(p) => vec![&p.expr],
            ExprKind::JsonExpr(j) => j.children(),
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

    pub(crate) fn children_mut(&mut self) -> Vec<&mut Expr> {
        match &mut self.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::CaseTest
            | ExprKind::DomainValue
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_)
            | ExprKind::SubColumn(_) => Vec::new(),
            ExprKind::SubLink(sub) => sub.test.iter_mut().collect(),
            ExprKind::RowCompare(rc) => rc.ops.iter_mut().collect(),
            ExprKind::Func(f) => f.args.iter_mut().collect(),
            ExprKind::NamedArg(na) => vec![&mut na.arg],
            ExprKind::Agg(agg) => {
                let Aggref { direct, args, filter, .. } = &mut **agg;
                direct.iter_mut().chain(args.iter_mut()).chain(filter.as_deref_mut()).collect()
            }
            ExprKind::Window(w) => {
                let WindowFunc { args, filter, .. } = &mut **w;
                args.iter_mut().chain(filter.as_deref_mut()).collect()
            }
            ExprKind::Grouping(g) => g.args.iter_mut().collect(),
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
            | ExprKind::Row(args, _)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter_mut().collect(),
            ExprKind::JoinRow(row) => row.fields.iter_mut().collect(),
            ExprKind::JsonValue(v) => vec![&mut v.raw, &mut v.formatted],
            ExprKind::JsonCtor(c) => {
                let JsonCtor { args, func, coercion, .. } = &mut **c;
                args.iter_mut().chain(func).chain(coercion).collect()
            }
            ExprKind::IsJson(p) => vec![&mut p.expr],
            ExprKind::JsonExpr(j) => j.children_mut(),
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
        self.raise_from(levels, 0);
    }

    /// `IncrementVarSublevelsUp` with `min_sublevels_up`: as [`Expr::raise`], but only the columns that are `depth` or more levels up change.
    pub(crate) fn raise_from(&mut self, levels: usize, depth: usize) {
        if let ExprKind::Var(var) = &mut self.kind
            && var.levels_up >= depth
        {
            var.levels_up += levels;
        }
        for child in self.children_mut() {
            child.raise_from(levels, depth);
        }
        if let ExprKind::SubLink(sub) = &mut self.kind {
            sub.query.each_expr_mut(depth + 1, &mut |e, depth| e.raise_from(levels, depth));
        }
    }

    /// `IncrementVarSublevelsUp` with a negative delta: each column of a query outside the expression comes `levels` levels down, as the planner moves an aggregate of an outer query to that query. A column of a level that is less than `levels` levels up does not change.
    pub fn lower(&mut self, levels: usize) {
        self.lower_from(levels, 0);
    }

    fn lower_from(&mut self, levels: usize, depth: usize) {
        if let ExprKind::Var(var) = &mut self.kind
            && var.levels_up >= depth + levels
        {
            var.levels_up -= levels;
        }
        for child in self.children_mut() {
            child.lower_from(levels, depth);
        }
        if let ExprKind::SubLink(sub) = &mut self.kind {
            sub.query.each_expr_mut(depth + 1, &mut |e, depth| e.lower_from(levels, depth));
        }
    }

    /// `contain_aggs_of_level` and `locate_agg_of_level`: the first aggregate call or call of `GROUPING` in the expression.
    pub fn first_agg(&self) -> Option<&Expr> {
        if matches!(&self.kind, ExprKind::Agg(agg) if agg.levels_up == 0)
            || matches!(&self.kind, ExprKind::Grouping(g) if g.levels_up == 0)
        {
            return Some(self);
        }
        self.children().into_iter().find_map(Expr::first_agg)
    }

    /// `contain_windowfuncs` and `locate_windowfunc`: the first call of a window function in the expression, outside its subqueries.
    pub fn first_window(&self) -> Option<&Expr> {
        if matches!(self.kind, ExprKind::Window(_)) {
            return Some(self);
        }
        self.children().into_iter().find_map(Expr::first_window)
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
            ExprKind::RowCompare(rc) => rc.ops.first(),
            ExprKind::NamedArg(na) => Some(&na.arg),
            _ => None,
        };
        match (self.location, first.and_then(Expr::place)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

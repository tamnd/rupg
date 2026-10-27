//! `Expr`, an expression of the query tree with its type, as the `Expr` nodes of `primnodes.h` after `transformExpr`.

use rupg_pgcatalog::builtin;
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
    /// `CollateExpr`: the value of the argument with the collation of this OID, from a `COLLATE` clause.
    Collate(Box<Expr>, u32),
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
    /// `FieldStore`: the row value of the argument with new values in some of its fields. The result is the row value.
    FieldStore(Box<FieldStore>),
    /// `Aggref`: a call of an aggregate function, which reads all the rows of a group.
    Agg(Box<Aggref>),
    /// `WindowFunc`: a call of a window function, which reads the rows of its window.
    Window(Box<WindowFunc>),
    /// `GroupingFunc`: a call of `GROUPING`, of type `integer`.
    Grouping(Box<GroupingFunc>),
    /// `PlaceHolderVar`: an expression of a subquery that the planner merged below the side of an outer join that can give nulls. Only the planner makes it.
    PlaceHolder(Box<PlaceHolder>),
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
    /// `TableFunc` of the kind `TFT_JSON_TABLE`: the call of `JSON_TABLE` in `FROM`, of type `record`.
    JsonTable(Box<JsonTable>),
    /// `XmlExpr`: `XMLELEMENT`, `XMLFOREST`, `XMLCONCAT`, `XMLPARSE`, `XMLPI`, `XMLROOT`, `XMLSERIALIZE` or `IS DOCUMENT`.
    Xml(Box<XmlExpr>),
    /// `CurrentOfExpr`: `WHERE CURRENT OF`, of type `boolean`.
    CurrentOf(Box<CurrentOf>),
}

/// `CurrentOfExpr`: the condition of `WHERE CURRENT OF` on the target table of an `UPDATE` or a `DELETE`, which is true for the row that the cursor is on, as `execCurrentOf` finds it.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentOf {
    /// The OID of the target table.
    pub table: u32,
    /// The name of the cursor: a constant for `cursor_name`, or the parameter of a variable of PL/pgSQL for `cursor_param`. Then `tableoid` and `ctid` of the target table.
    pub args: Vec<Expr>,
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

/// The parts of an assignment to fields of a row value, as `FieldStore`.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldStore {
    /// `arg`: the old row value. A null value is a row whose fields are all null.
    pub arg: Expr,
    /// `fieldnums` and `newvals`: the index from 0 of each field that gets a new value, and the new value. In the new value, `CaseTest` is the old value of the field.
    pub fields: Vec<(usize, Expr)>,
    /// The type of each field of the row type, for a row that the null value makes.
    pub types: Vec<u32>,
}

/// The parts of a fetch from an array or of an assignment to a part of an array, as `SubscriptingRef`.
#[derive(Clone, Debug, PartialEq)]
pub struct Subscript {
    /// `refexpr`: the array.
    pub container: Expr,
    /// `refupperindexpr`: the subscript of each dimension, or the upper bound of a slice. `None` is a bound that the query does not give.
    pub upper: Vec<Option<Expr>>,
    /// `reflowerindexpr`: the lower bound of each dimension of a slice, or `None` for one element.
    pub lower: Option<Vec<Option<Expr>>>,
    /// `refassgnexpr`: the new value of the element or of the slice in an assignment, as `a[1] := 5` of PL/pgSQL makes it. The result of an assignment is the array with the new value.
    pub assign: Option<Expr>,
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
    /// For a correlated `EXISTS`, the same test as `ANY` on a subquery that reads no column of the query, as `convert_EXISTS_to_ANY` makes it. The planner fills it and runs the subquery that costs less, as an `AlternativeSubPlan`.
    pub alternative: Option<Box<SubLink>>,
    /// True when the test called a volatile function before the fold. `pull_up_sublinks` runs before `preprocess_expression`, so a test with a call of a volatile SQL function that the fold puts in the query does not make a join.
    pub volatile: bool,
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
    /// `SET (a, b) = (SELECT ...)` of `UPDATE`, as `MULTIEXPR_SUBLINK` with its `PARAM_MULTIEXPR`: the value of the column `column`, from 0, of the one row. Each column of the item has a copy of the subquery with the same `id`, and the subquery runs once for each row.
    Multi { id: u32, column: usize },
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

/// A `PlaceHolderVar`: the value of `expr` at the scan or the join of the relations of the subquery, which is null in a row where an outer join above them gives nulls for them, as a column of those relations is.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceHolder {
    pub expr: Expr,
    /// `phrels`: the relations of the subquery in the query.
    pub relations: Vec<usize>,
    /// `phlevelsup`: 0 for the query of the expression, 1 for the query outside it, and so on.
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

/// `XmlExprOp`: the function of an [`XmlExpr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlOp {
    Concat,
    Element,
    Forest,
    Parse,
    Pi,
    Root,
    Serialize,
    Document,
}

/// `XmlExpr`: a SQL/XML function with its own syntax. The type is `boolean` for `IS DOCUMENT`, `text` for `XMLSERIALIZE` and `xml` for the others.
#[derive(Clone, Debug, PartialEq)]
pub struct XmlExpr {
    pub op: XmlOp,
    /// The XML name of the element of `XMLELEMENT` or the target of `XMLPI`.
    pub name: Option<String>,
    /// The attributes of `XMLELEMENT` or the elements of `XMLFOREST`, with their XML names.
    pub named: Vec<(String, Expr)>,
    /// The other arguments. `XMLPARSE` has the text and the flag of `PRESERVE WHITESPACE`. `XMLROOT` has the value, the version or a null `text` and the `integer` of `STANDALONE`.
    pub args: Vec<Expr>,
    /// `DOCUMENT`, not `CONTENT`, for `XMLPARSE` and `XMLSERIALIZE`.
    pub document: bool,
    /// `INDENT` of `XMLSERIALIZE`.
    pub indent: bool,
    /// The type and the type modifier of `AS` of `XMLSERIALIZE`.
    pub target: (u32, i32),
}

impl XmlExpr {
    /// The expressions of the named arguments, then the other arguments.
    pub fn children(&self) -> Vec<&Expr> {
        self.named.iter().map(|(_, e)| e).chain(&self.args).collect()
    }

    /// The expressions of the named arguments, then the other arguments.
    pub fn children_mut(&mut self) -> Vec<&mut Expr> {
        self.named.iter_mut().map(|(_, e)| e).chain(&mut self.args).collect()
    }
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
    /// `column_name`: the name of the column of `JSON_TABLE` that the node computes, which the errors show.
    pub column: Option<String>,
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

/// `TableFunc` of the kind `TFT_JSON_TABLE`: the call of `JSON_TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonTable {
    /// `docexpr`: the context item as a `jsonb` value.
    pub context: Expr,
    /// The names and the values of the `PASSING` clause.
    pub passing: Vec<(String, Expr)>,
    /// The top `ON ERROR` clause: `ERROR`, `EMPTY` or `EMPTY ARRAY`.
    pub on_error: JsonBehaviorKind,
    /// The plan that gives the rows.
    pub plan: JsonTablePlan,
    /// The columns, with no `NESTED` parts, in the order of the result.
    pub columns: Vec<JsonTableColumn>,
}

impl JsonTable {
    /// The context item, the values of `PASSING` and the expressions of the columns.
    pub fn children(&self) -> Vec<&Expr> {
        let mut all = vec![&self.context];
        all.extend(self.passing.iter().map(|(_, e)| e));
        all.extend(self.columns.iter().filter_map(|c| c.expr.as_ref()));
        all
    }

    /// [`JsonTable::children`], to change.
    pub fn children_mut(&mut self) -> Vec<&mut Expr> {
        let JsonTable { context, passing, columns, .. } = self;
        let mut all = vec![context];
        all.extend(passing.iter_mut().map(|(_, e)| e));
        all.extend(columns.iter_mut().filter_map(|c| c.expr.as_mut()));
        all
    }
}

/// A column of `JSON_TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonTableColumn {
    pub name: String,
    pub ty: u32,
    pub typmod: i32,
    /// The `JsonExpr` that gives the value from the row of the plan, which reads the row as a `CaseTest`, or `None` for `FOR ORDINALITY`.
    pub expr: Option<Expr>,
}

/// `JsonTablePlan`: the plan that gives the rows of `JSON_TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonTablePlan {
    /// `JsonTablePathScan`.
    Scan(Box<JsonTableScan>),
    /// `JsonTableSiblingJoin`: the rows of the left plan, and then the rows of the right plan.
    Sibling(Box<JsonTablePlan>, Box<JsonTablePlan>),
}

/// `JsonTablePathScan`: a row pattern path and the columns that read its rows.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonTableScan {
    /// The path, as a `jsonpath` value.
    pub path: Value,
    /// The name of the path, which the analyzer gives when the query does not.
    pub name: String,
    /// True when an error of the path is an error of the query, for `ERROR ON ERROR` at the top.
    pub error_on_error: bool,
    /// The indexes of the columns of the scan in [`JsonTable::columns`].
    pub columns: std::ops::Range<usize>,
    /// The plan of the `NESTED` parts.
    pub child: Option<JsonTablePlan>,
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

    /// The cost of the calls of functions and operators in the node of this expression, without its children, in units of `cpu_operator_cost`, as `cost_qual_eval_walker` adds it. A call, an operator, `NULLIF` and `IS DISTINCT FROM` cost the `procost` of their function, as `add_function_cost` adds it.
    pub fn own_cost(&self) -> f64 {
        match &self.kind {
            ExprKind::Func(f) => func_cost(f.oid),
            ExprKind::NullIf { equal, .. } | ExprKind::Distinct { equal, .. } => func_cost(*equal),
            ExprKind::ScalarArrayOp { .. } | ExprKind::MinMax { .. } => 1.0,
            // The input function of the new type and the output function of the old type.
            ExprKind::CoerceViaIo(..) => 2.0,
            _ => 0.0,
        }
    }

    /// `contain_leaked_vars`: true when the expression can give information about a column of the query other than through its result, as a call of a function that is not leakproof with a column in its arguments does. An expression of a kind that the check does not know is leaky.
    pub fn leaky(&self) -> bool {
        let func = match &self.kind {
            ExprKind::CurrentOf(_) => return false,
            ExprKind::Var(_)
            | ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::Array { .. }
            | ExprKind::FieldSelect(..)
            | ExprKind::FieldStore(_)
            | ExprKind::NamedArg(_)
            | ExprKind::Bool(..)
            | ExprKind::Relabel(..)
            | ExprKind::Collate(..)
            | ExprKind::Case(_)
            | ExprKind::CaseTest
            | ExprKind::Row(..)
            | ExprKind::SqlValue(_)
            | ExprKind::NullTest(..)
            | ExprKind::BooleanTest(..) => None,
            ExprKind::Func(f) => Some(f.oid),
            ExprKind::NullIf { equal, .. } | ExprKind::Distinct { equal, .. } => Some(*equal),
            ExprKind::ScalarArrayOp { func, .. } => Some(*func),
            _ => return true,
        };
        if let Some(func) = func
            && !builtin::proc_by_oid(func).is_some_and(|p| p.leakproof)
            && self
                .find(0, &mut |e, depth| {
                    matches!(e.kind, ExprKind::Var(v) if v.levels_up == depth).then_some(())
                })
                .is_some()
        {
            return true;
        }
        self.children().into_iter().any(Expr::leaky)
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
            ExprKind::PlaceHolder(p) => vec![&p.expr],
            ExprKind::Relabel(arg, _)
            | ExprKind::Collate(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => vec![&**arg],
            ExprKind::ArrayCoerce { arg, element, .. } => vec![&**arg, &**element],
            ExprKind::Subscript(sub) => {
                sub.bounds().chain([&sub.container]).chain(&sub.assign).collect()
            }
            ExprKind::FieldStore(store) => {
                [&store.arg].into_iter().chain(store.fields.iter().map(|(_, v)| v)).collect()
            }
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::Row(args, _)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter().collect(),
            ExprKind::CurrentOf(c) => c.args.iter().collect(),
            ExprKind::JoinRow(row) => row.fields.iter().collect(),
            ExprKind::JsonValue(v) => vec![&v.raw, &v.formatted],
            ExprKind::JsonCtor(c) => c.args.iter().chain(&c.func).chain(&c.coercion).collect(),
            ExprKind::IsJson(p) => vec![&p.expr],
            ExprKind::JsonExpr(j) => j.children(),
            ExprKind::Xml(x) => x.children(),
            ExprKind::JsonTable(j) => j.children(),
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
            ExprKind::PlaceHolder(p) => vec![&mut p.expr],
            ExprKind::Relabel(arg, _)
            | ExprKind::Collate(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => vec![&mut **arg],
            ExprKind::ArrayCoerce { arg, element, .. } => vec![&mut **arg, &mut **element],
            ExprKind::Subscript(sub) => {
                let Subscript { container, upper, lower, assign } = &mut **sub;
                let bounds = upper.iter_mut().chain(lower.iter_mut().flatten()).flatten();
                bounds.chain([container]).chain(assign).collect()
            }
            ExprKind::FieldStore(store) => {
                let FieldStore { arg, fields, .. } = &mut **store;
                [arg].into_iter().chain(fields.iter_mut().map(|(_, v)| v)).collect()
            }
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::Row(args, _)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. }
            | ExprKind::Array { elements: args, .. } => args.iter_mut().collect(),
            ExprKind::CurrentOf(c) => c.args.iter_mut().collect(),
            ExprKind::JoinRow(row) => row.fields.iter_mut().collect(),
            ExprKind::JsonValue(v) => vec![&mut v.raw, &mut v.formatted],
            ExprKind::JsonCtor(c) => {
                let JsonCtor { args, func, coercion, .. } = &mut **c;
                args.iter_mut().chain(func).chain(coercion).collect()
            }
            ExprKind::IsJson(p) => vec![&mut p.expr],
            ExprKind::JsonExpr(j) => j.children_mut(),
            ExprKind::Xml(x) => x.children_mut(),
            ExprKind::JsonTable(j) => j.children_mut(),
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
        match &mut self.kind {
            ExprKind::Var(var) if var.levels_up >= depth => var.levels_up += levels,
            ExprKind::PlaceHolder(p) if p.levels_up >= depth => p.levels_up += levels,
            _ => {}
        }
        for child in self.children_mut() {
            child.raise_from(levels, depth);
        }
        if let ExprKind::SubLink(sub) = &mut self.kind {
            sub.query.each_expr_mut(depth + 1, &mut |e, depth| e.raise_from(levels, depth));
        }
    }

    /// `expression_tree_mutator`: calls `f` for the expression and for each expression in it, also in its subqueries, with the number of subqueries between the expression and this expression. `f` sees an expression before the expressions in it, and when `f` gives true, the walk does not go into the expression, as for an expression that `f` put in its place.
    pub fn each_mut(&mut self, depth: usize, f: &mut impl FnMut(&mut Expr, usize) -> bool) {
        if f(self, depth) {
            return;
        }
        for child in self.children_mut() {
            child.each_mut(depth, f);
        }
        if let ExprKind::SubLink(sub) = &mut self.kind {
            sub.query.each_expr_mut(depth + 1, &mut |e, depth| e.each_mut(depth, f));
        }
    }

    /// `map_variable_attnos` for an expression on one relation, such as a check constraint or an index expression: each column gets the number that `map` gives for it, the number in a partition for each column of its parent table. A system column and a whole-row column keep their numbers.
    pub fn map_attnos(&mut self, map: &[i16]) {
        if let ExprKind::Var(var) = &mut self.kind
            && var.levels_up == 0
            && var.attnum > 0
            && let Some(&to) = map.get(usize::try_from(var.attnum - 1).unwrap_or(usize::MAX))
        {
            var.attnum = to;
        }
        for child in self.children_mut() {
            child.map_attnos(map);
        }
    }

    /// `IncrementVarSublevelsUp` with a negative delta: each column of a query outside the expression comes `levels` levels down, as the planner moves an aggregate of an outer query to that query. A column of a level that is less than `levels` levels up does not change.
    pub fn lower(&mut self, levels: usize) {
        self.lower_from(levels, 0);
    }

    fn lower_from(&mut self, levels: usize, depth: usize) {
        match &mut self.kind {
            ExprKind::Var(var) if var.levels_up >= depth + levels => var.levels_up -= levels,
            ExprKind::PlaceHolder(p) if p.levels_up >= depth + levels => p.levels_up -= levels,
            _ => {}
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
            | ExprKind::Collate(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::ArrayCoerce { arg, .. }
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => Some(&**arg),
            ExprKind::Subscript(sub) => Some(&sub.container),
            ExprKind::FieldStore(store) => Some(&store.arg),
            ExprKind::SubLink(sub) => sub.test.as_ref(),
            ExprKind::RowCompare(rc) => rc.ops.first(),
            ExprKind::NamedArg(na) => Some(&na.arg),
            ExprKind::Xml(x) => x.args.first(),
            _ => None,
        };
        match (self.location, first.and_then(Expr::place)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// `procost` of the function `func`, as `get_func_cost` reads it, or 1 for a function that is not known.
pub fn func_cost(func: u32) -> f64 {
    builtin::proc_by_oid(func).map_or(1.0, |p| f64::from(p.cost))
}

//! The tree of a compiled function, as `plpgsql.h` defines it: the variables of the function, the names that each block can see, and the statements.

use rupg_common::{Level, SqlState};
use rupg_sql::nodes::FetchDirection;

/// A compiled function, `PLpgSQL_function`.
#[derive(Debug)]
pub(crate) struct Function {
    /// The variables of the function, `datums`.
    pub(crate) datums: Vec<Datum>,
    /// The names of the function. Each item points to the item before it, so that an expression sees the names before its place.
    pub(crate) ns: Vec<NsItem>,
    /// The variable of each input argument, in order.
    pub(crate) args: Vec<usize>,
    /// The variable `found`.
    pub(crate) found: usize,
    /// The variable that `RETURN` gives in a function with `OUT` arguments, `out_param_varno`: the one `OUT` argument, or a row of all of them.
    pub(crate) out: Option<usize>,
    /// The top block.
    pub(crate) body: Block,
    /// The number of the expressions, which each have a number from 0.
    pub(crate) exprs: usize,
    /// True for a function that returns a set.
    pub(crate) retset: bool,
    /// The declared result type.
    pub(crate) rettype: u32,
    /// The variable `new` of a trigger function, which [`crate::compile::TRIGGER_VARIABLES`] and `old` follow.
    pub(crate) trigger: Option<usize>,
}

/// A variable, `PLpgSQL_datum`.
#[derive(Debug)]
pub(crate) struct Datum {
    pub(crate) name: String,
    pub(crate) kind: DatumKind,
    /// True for a variable that an assignment cannot change: a `CONSTANT`, `sqlstate` and `sqlerrm`.
    pub(crate) constant: bool,
    pub(crate) not_null: bool,
    /// The default of the declaration.
    pub(crate) default: Option<Expr>,
    /// The line of the declaration, for the context of an error in the default.
    pub(crate) line: u32,
    /// The query of a bound cursor.
    pub(crate) cursor: Option<Box<Bound>>,
}

/// The query of a bound cursor, `cursor_explicit_expr` of `PLpgSQL_var`.
#[derive(Debug)]
pub(crate) struct Bound {
    pub(crate) query: Expr,
    /// `cursor_explicit_argrow`: the row of the arguments of the cursor, or `None` for a cursor with no arguments.
    pub(crate) args: Option<usize>,
    /// `cursor_options`: `CURSOR_OPT_SCROLL` or `CURSOR_OPT_NO_SCROLL`, or 0.
    pub(crate) options: i32,
}

/// `plpgsql_ns_lookup`: from the item `top`, the variable of the first name, of `label.name` with two names, or of a qualified name with three, with the number of names that it used. A name with more names after it only matches a record. With `local`, the lookup stops at the start of the last block.
pub(crate) fn ns_lookup(
    ns: &[NsItem],
    datums: &[Datum],
    top: Option<usize>,
    local: bool,
    names: &[&str],
) -> Option<(usize, usize)> {
    let (&n1, rest) = names.split_first()?;
    let (n2, n3) = (rest.first().copied(), rest.get(1).copied());
    let is_var = |d: usize| !matches!(datums[d].kind, DatumKind::Rec { .. });
    let mut cur = top;
    while let Some(start) = cur {
        let mut at = start;
        while let NsEntry::Datum(d) = ns[at].entry {
            if ns[at].name == n1 && (n2.is_none() || !is_var(d)) {
                return Some((d, 1));
            }
            at = ns[at].prev?;
        }
        if let Some(n2) = n2
            && ns[at].name == n1
        {
            let mut i = start;
            while let NsEntry::Datum(d) = ns[i].entry {
                if ns[i].name == n2 && (n3.is_none() || !is_var(d)) {
                    return Some((d, 2));
                }
                i = ns[i].prev?;
            }
        }
        if local {
            break;
        }
        cur = ns[at].prev;
    }
    None
}

/// The kind of a variable.
#[derive(Debug)]
pub(crate) enum DatumKind {
    /// `PLpgSQL_var`: a variable of a scalar type. The type 0 is the variable of a `CASE`, which takes the type of each value.
    Var { ty: u32, typmod: i32 },
    /// `PLpgSQL_rec`: a variable of a row type or of `record`.
    Rec { ty: u32 },
    /// `PLpgSQL_row`: a list of variables, as the target of `INTO x, y`.
    Row { fields: Vec<usize> },
    /// `PLpgSQL_recfield`: the field `name` of the record `rec`, as a target of `INTO`, of `GET DIAGNOSTICS` and of a loop.
    Field { rec: usize, name: String },
}

/// An item of the names, `PLpgSQL_nsitem`.
#[derive(Debug)]
pub(crate) struct NsItem {
    /// The item before this one, or `None` for the first item.
    pub(crate) prev: Option<usize>,
    pub(crate) name: String,
    pub(crate) entry: NsEntry,
}

/// What a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NsEntry {
    /// The label of a block or a loop, which starts the names of the block. An unnamed block has an empty label.
    Label(LabelKind),
    /// A variable.
    Datum(usize),
}

/// `PLpgSQL_label_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelKind {
    Block,
    Loop,
    /// The names of the arguments of a cursor, which only the query of the cursor sees.
    Other,
}

/// The form of the text of an expression for the parser of SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `RAW_PARSE_PLPGSQL_EXPR`: the targets and the clauses of a `SELECT`, without `SELECT`.
    Expr,
    /// `RAW_PARSE_DEFAULT`: a statement of SQL.
    Statement,
    /// The value of an assignment, whose context shows the whole assignment.
    Assign,
    /// `RAW_PARSE_PLPGSQL_ASSIGNn`: the whole assignment to a part of a variable, as `x[1] := 5`, with the number of the names of the target. The value of the query is the new value of the variable.
    Part(usize),
}

/// An expression or a statement of SQL in the function, `PLpgSQL_expr`.
#[derive(Debug)]
pub(crate) struct Expr {
    /// The number of the expression, for the plan of the call.
    pub(crate) id: usize,
    /// The text that the parser of SQL reads.
    pub(crate) text: String,
    /// The text of the context of an error, which is the whole assignment for an assignment.
    pub(crate) shown: String,
    /// The byte offset of `text` in `shown`.
    pub(crate) lead: usize,
    pub(crate) mode: Mode,
    /// The last name that the expression sees.
    pub(crate) ns: Option<usize>,
    /// The byte offset of `text` in the body.
    pub(crate) at: usize,
    /// The variable that an assignment sets, whose type the value takes in the analysis.
    pub(crate) target: Option<usize>,
}

/// A statement with its line, `PLpgSQL_stmt`.
#[derive(Debug)]
pub(crate) struct Stmt {
    pub(crate) line: u32,
    pub(crate) kind: StmtKind,
}

/// A block, `PLpgSQL_stmt_block`.
#[derive(Debug)]
pub(crate) struct Block {
    pub(crate) label: Option<String>,
    /// The variables of `DECLARE`, which the block sets at its start.
    pub(crate) decls: Vec<usize>,
    pub(crate) body: Vec<Stmt>,
    pub(crate) exceptions: Option<Exceptions>,
}

/// The `EXCEPTION` part of a block.
#[derive(Debug)]
pub(crate) struct Exceptions {
    /// The variable `sqlstate` of the handlers.
    pub(crate) sqlstate: usize,
    /// The variable `sqlerrm` of the handlers.
    pub(crate) sqlerrm: usize,
    pub(crate) handlers: Vec<Handler>,
}

/// `PLpgSQL_exception`: the conditions of `WHEN` and the statements that handle them.
#[derive(Debug)]
pub(crate) struct Handler {
    pub(crate) conds: Vec<Cond>,
    pub(crate) body: Vec<Stmt>,
}

/// `PLpgSQL_condition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cond {
    /// `OTHERS`, which is every error except a cancel and a failed assertion.
    Others,
    /// A code, which is a class when it ends with `000`.
    State(SqlState),
}

/// The target of an assignment.
#[derive(Debug)]
pub(crate) enum Target {
    /// A variable, a row or a record.
    Datum(usize),
    /// A field of a record.
    Field(usize, String),
}

/// The query of a `FOR` loop over rows and of `RETURN QUERY`.
#[derive(Debug)]
pub(crate) enum Source {
    /// A query in the text of the function.
    Query(Expr),
    /// `EXECUTE expr USING params`.
    Dynamic { query: Expr, params: Vec<Expr> },
}

/// The query of `OPEN`.
#[derive(Debug)]
pub(crate) enum Opened {
    /// The query of a bound cursor, with the query that gives the values of its arguments.
    Bound(Option<Expr>),
    /// `OPEN c FOR query`.
    Query(Expr),
    /// `OPEN c FOR EXECUTE query USING params`.
    Dynamic { query: Expr, params: Vec<Expr> },
}

/// The value of `RETURN` and `RETURN NEXT`.
#[derive(Debug)]
pub(crate) enum Returned {
    /// No value, in a function that returns a set or `void`.
    Nothing,
    /// A variable, `retvarno`: a variable that the statement names alone, or the `OUT` arguments.
    Var(usize),
    Expr(Expr),
}

/// The kind of a `RAISE` option, `PLpgSQL_raise_option_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RaiseOption {
    Errcode,
    Message,
    Detail,
    Hint,
    Column,
    Constraint,
    Datatype,
    Table,
    Schema,
}

/// An item of `GET DIAGNOSTICS`, `PLpgSQL_getdiag_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagItem {
    RowCount,
    RoutineOid,
    Context,
    ErrorContext,
    ErrorDetail,
    ErrorHint,
    ReturnedSqlstate,
    ColumnName,
    ConstraintName,
    DatatypeName,
    MessageText,
    TableName,
    SchemaName,
}

impl DiagItem {
    /// `plpgsql_getdiag_kindname`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            DiagItem::RowCount => "ROW_COUNT",
            DiagItem::RoutineOid => "PG_ROUTINE_OID",
            DiagItem::Context => "PG_CONTEXT",
            DiagItem::ErrorContext => "PG_EXCEPTION_CONTEXT",
            DiagItem::ErrorDetail => "PG_EXCEPTION_DETAIL",
            DiagItem::ErrorHint => "PG_EXCEPTION_HINT",
            DiagItem::ReturnedSqlstate => "RETURNED_SQLSTATE",
            DiagItem::ColumnName => "COLUMN_NAME",
            DiagItem::ConstraintName => "CONSTRAINT_NAME",
            DiagItem::DatatypeName => "PG_DATATYPE_NAME",
            DiagItem::MessageText => "MESSAGE_TEXT",
            DiagItem::TableName => "TABLE_NAME",
            DiagItem::SchemaName => "SCHEMA_NAME",
        }
    }

    /// True for an item of `GET STACKED DIAGNOSTICS`, which reads the error of the handler.
    pub(crate) fn stacked(self) -> bool {
        !matches!(self, DiagItem::RowCount | DiagItem::RoutineOid | DiagItem::Context)
    }
}

/// The statements, `PLpgSQL_stmt_type`.
#[derive(Debug)]
pub(crate) enum StmtKind {
    Block(Block),
    Assign {
        target: Target,
        expr: Expr,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        elsifs: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    /// `CASE`. A `CASE` with a test value keeps it in the variable `var`, which each `WHEN` reads.
    Case {
        test: Option<(Expr, usize)>,
        whens: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
    },
    While {
        label: Option<String>,
        cond: Expr,
        body: Vec<Stmt>,
    },
    ForInt {
        label: Option<String>,
        var: usize,
        reverse: bool,
        lower: Expr,
        upper: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    ForQuery {
        label: Option<String>,
        target: usize,
        source: Source,
        body: Vec<Stmt>,
    },
    /// `FOR target IN cursor(args)`: a loop over the rows of a bound cursor.
    ForCursor {
        label: Option<String>,
        target: usize,
        cursor: usize,
        args: Option<Expr>,
        body: Vec<Stmt>,
    },
    ForEach {
        label: Option<String>,
        target: usize,
        slice: i32,
        expr: Expr,
        body: Vec<Stmt>,
    },
    /// `EXIT` and `CONTINUE`.
    Exit {
        exit: bool,
        label: Option<String>,
        cond: Option<Expr>,
    },
    Return(Returned),
    ReturnNext(Returned),
    ReturnQuery(Source),
    Raise {
        /// The level, or `None` for `EXCEPTION`.
        level: Option<Level>,
        /// The condition name or the code of `RAISE name` and `RAISE SQLSTATE 'xxxxx'`.
        condition: Option<SqlState>,
        condname: Option<String>,
        message: Option<String>,
        params: Vec<Expr>,
        options: Vec<(RaiseOption, Expr)>,
    },
    Assert {
        cond: Expr,
        message: Option<Expr>,
    },
    /// A statement of SQL, with the target of `INTO`.
    ExecSql {
        sql: Expr,
        into: Option<usize>,
        strict: bool,
    },
    Execute {
        query: Expr,
        into: Option<usize>,
        strict: bool,
        params: Vec<Expr>,
    },
    Perform(Expr),
    /// `CALL` and `DO`, which `call` tells apart.
    Call {
        expr: Expr,
        call: bool,
    },
    GetDiag {
        stacked: bool,
        items: Vec<(usize, DiagItem)>,
    },
    /// `OPEN`, with the options of `SCROLL` of an unbound cursor.
    Open {
        cursor: usize,
        query: Opened,
        options: i32,
    },
    /// `FETCH` into the target `into`, or `MOVE` when it is `None`. `expr` gives the count when it is there.
    Fetch {
        cursor: usize,
        direction: FetchDirection,
        count: i64,
        expr: Option<Expr>,
        into: Option<usize>,
    },
    Close(usize),
    /// `COMMIT` and `ROLLBACK`, which `commit` tells apart, with `AND CHAIN` when `chain` is true.
    EndTransaction {
        commit: bool,
        chain: bool,
    },
    Null,
}

impl StmtKind {
    /// `plpgsql_stmt_typename`: the name of the statement in the context of an error.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            StmtKind::Block(_) => "statement block",
            StmtKind::Assign { .. } => "assignment",
            StmtKind::If { .. } => "IF",
            StmtKind::Case { .. } => "CASE",
            StmtKind::Loop { .. } => "LOOP",
            StmtKind::While { .. } => "WHILE",
            StmtKind::ForInt { .. } => "FOR with integer loop variable",
            StmtKind::ForQuery { source: Source::Query(_), .. } => "FOR over SELECT rows",
            StmtKind::ForQuery { .. } => "FOR over EXECUTE statement",
            StmtKind::ForCursor { .. } => "FOR over cursor",
            StmtKind::ForEach { .. } => "FOREACH over array",
            StmtKind::Exit { exit: true, .. } => "EXIT",
            StmtKind::Exit { .. } => "CONTINUE",
            StmtKind::Return(_) => "RETURN",
            StmtKind::ReturnNext(_) => "RETURN NEXT",
            StmtKind::ReturnQuery(_) => "RETURN QUERY",
            StmtKind::Raise { .. } => "RAISE",
            StmtKind::Assert { .. } => "ASSERT",
            StmtKind::ExecSql { .. } => "SQL statement",
            StmtKind::Execute { .. } => "EXECUTE",
            StmtKind::Perform(_) => "PERFORM",
            StmtKind::Call { call: true, .. } => "CALL",
            StmtKind::Call { .. } => "DO",
            StmtKind::GetDiag { stacked: true, .. } => "GET STACKED DIAGNOSTICS",
            StmtKind::GetDiag { .. } => "GET DIAGNOSTICS",
            StmtKind::Open { .. } => "OPEN",
            StmtKind::Fetch { into: Some(_), .. } => "FETCH",
            StmtKind::Fetch { .. } => "MOVE",
            StmtKind::Close(_) => "CLOSE",
            StmtKind::EndTransaction { commit: true, .. } => "COMMIT",
            StmtKind::EndTransaction { .. } => "ROLLBACK",
            StmtKind::Null => "NULL",
        }
    }
}

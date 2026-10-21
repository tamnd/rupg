//! The executor of PL/pgSQL, as `pl_exec.c` runs a compiled function: the values of the variables, the statements, the plans of the expressions and the context of an error.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{ExprKind, Params, Query, Variable, Variables, types};
use rupg_common::{Error, Object, Result, SqlState};
use rupg_exec::Plan;
use rupg_func::Session;
use rupg_sql::nodes::{FetchDirection, Node};
use rupg_types::{Array, Record, Value, oid};

use crate::ast::{
    Block, Cond, DatumKind, DiagItem, Expr, Function, Mode, Opened, RaiseOption, Returned, Source,
    Stmt, StmtKind, Target, ns_lookup,
};
use crate::{Spi, TriggerCall};

/// The name, the type and the typmod of a field of a row or of a column of a result.
pub(crate) type Field = (String, u32, i32);

/// The source type and typmod and the target type and typmod of a cast.
type CastKey = (u32, i32, u32, i32);

/// The rows of a query and its columns.
type Rows = (Vec<Vec<Value>>, Vec<Field>);

/// The depth of the calls of PL/pgSQL functions where a call fails, which stands for `max_stack_depth`.
const MAX_DEPTH: usize = 100;

/// A compiled function with the plans of its expressions, `PLpgSQL_function` with the plans of `PLpgSQL_expr`.
#[derive(Debug)]
pub(crate) struct Compiled {
    pub(crate) func: Function,
    /// `fn_signature`: the name and the argument types in the context of an error.
    pub(crate) sig: Rc<str>,
    /// The OID of the function, or 0 for the code of `DO`.
    pub(crate) oid: u32,
    /// The plan of each expression, by its number.
    plans: RefCell<Vec<Option<Rc<Prepared>>>>,
    /// The plans of the casts of the assignments, `cast_hash`.
    casts: RefCell<BTreeMap<CastKey, Rc<Plan>>>,
    /// The parse tree of each statement of SQL that is not a query, by the number of its expression, and `None` for a query.
    utilities: RefCell<BTreeMap<usize, Option<Rc<Node>>>>,
}

impl Compiled {
    pub(crate) fn new(func: Function, sig: impl Into<Rc<str>>, oid: u32) -> Compiled {
        let plans = vec![None; func.exprs];
        Compiled {
            func,
            sig: sig.into(),
            oid,
            plans: RefCell::new(plans),
            casts: RefCell::default(),
            utilities: RefCell::default(),
        }
    }
}

/// A call of a function on the stack, for the context of a message, as `plpgsql_exec_error_callback` and `_SPI_error_callback` give it.
#[derive(Debug)]
pub(crate) struct Frame {
    sig: Rc<str>,
    /// The line and the name of the statement that runs, `err_stmt`.
    at: Option<(u32, &'static str)>,
    /// The context line of the query that runs, while it runs.
    spi: Option<String>,
}

impl Frame {
    /// The context line of the call.
    fn line(&self) -> String {
        match self.at {
            Some((line, name)) => format!("PL/pgSQL function {} line {line} at {name}", self.sig),
            None => format!("PL/pgSQL function {}", self.sig),
        }
    }
}

/// The context of the calls on the stack, from the innermost call, as `GetErrorContextStack` gives it.
pub(crate) fn stack(frames: &[Frame]) -> String {
    let mut lines = Vec::new();
    for frame in frames.iter().rev() {
        lines.extend(frame.spi.clone());
        lines.push(frame.line());
    }
    lines.join("\n")
}

/// The failure of a statement.
#[derive(Debug)]
enum Fail {
    /// An error with no context of the function yet.
    Raw(Error),
    /// An error with the context of the place where it started.
    Framed(Error),
}

impl From<Error> for Fail {
    fn from(error: Error) -> Fail {
        Fail::Raw(error)
    }
}

impl Fail {
    /// The error, with the context line `line` for an error that has no context of the function.
    fn framed(self, line: impl FnOnce() -> String) -> Error {
        match self {
            Fail::Raw(error) => error.with_context(line()),
            Fail::Framed(error) => error,
        }
    }
}

type Run<T> = std::result::Result<T, Fail>;

/// What the statements do next, as the `PLPGSQL_RC_*` codes give it.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Normal,
    /// `EXIT`, with its label.
    Exit(Option<String>),
    /// `CONTINUE`, with its label.
    Continue(Option<String>),
    Return,
}

/// `LOOP_RC_PROCESSING`: what a loop does after its body gave `flow`. `None` runs the body again, `Some(Flow::Normal)` ends the loop, and another flow ends the loop and goes out.
fn after_body(flow: Flow, label: Option<&str>) -> Option<Flow> {
    match flow {
        Flow::Normal | Flow::Continue(None) => None,
        Flow::Exit(None) => Some(Flow::Normal),
        Flow::Exit(Some(l)) if label == Some(l.as_str()) => Some(Flow::Normal),
        Flow::Continue(Some(l)) if label == Some(l.as_str()) => None,
        flow => Some(flow),
    }
}

/// The value of a variable.
#[derive(Clone, Debug)]
enum Slot {
    /// A scalar with its type. The variable of a `CASE` takes the type of each value.
    Var { value: Value, ty: u32, typmod: i32 },
    /// A record: the fields of its type or of the row that it got, and the values, or `None` for a null record.
    Rec { ty: u32, fields: Option<Rc<[Field]>>, values: Option<Vec<Value>> },
    /// A list of variables.
    Row,
}

/// What a parameter of a query reads: a variable, or a field of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    Datum(usize),
    Field(usize, String),
}

/// The plan of an expression with the variables that its parameters read, and their types when the plan was made.
#[derive(Debug)]
struct Prepared {
    plan: Plan,
    bindings: Vec<(Binding, u32, i32)>,
    /// The value of [`Spi::changes`] when the plan was made.
    changes: u64,
    /// `expr_simple_expr`: an expression with no table and no subquery, whose errors have no context of the query when it runs.
    simple: bool,
}

/// `exec_simple_check_plan`: true for a query that is one value with no table, no clause and no subquery.
fn is_simple(q: &Query) -> bool {
    let plain = q.relations.is_empty()
        && q.from.is_empty()
        && q.filter.is_none()
        && !q.grouped
        && q.group.is_empty()
        && q.having.is_none()
        && q.sort.is_empty()
        && q.distinct.is_empty()
        && q.limit.is_none()
        && q.offset.is_none()
        && q.set_op.is_none()
        && q.windows.is_empty()
        && q.ctes.is_empty()
        && q.write.is_none();
    plain
        && q.targets.len() == 1
        && q.targets[0].expr.first_set_call().is_none()
        && q.targets[0]
            .expr
            .find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(()))
            .is_none()
}

/// `_SPI_error_callback`: the error of a query of the function. An error with a place in the text points into the text as the internal query, and another error gets the text as a context line.
fn spi_error(e: &Expr, error: Error) -> Error {
    if let Some(at) = error.position() {
        return error.with_position(None).with_internal(e.shown.clone(), Some(at + e.lead));
    }
    error.with_context(spi_line(e.mode, &e.shown))
}

/// The context line of a query of the function that runs.
fn spi_line(mode: Mode, text: &str) -> String {
    match mode {
        Mode::Expr => format!("PL/pgSQL expression \"{text}\""),
        Mode::Assign | Mode::Part(_) => format!("PL/pgSQL assignment \"{text}\""),
        Mode::Statement => format!("SQL statement \"{text}\""),
    }
}

/// The error of the parser of SQL, at its place in the text.
fn sql_error(error: rupg_sql::Error) -> Error {
    let at = error.location;
    Error::from(error).with_position(at)
}

/// The statement of a text of SQL, the first one when there are several.
fn parse_statement(text: &str) -> Result<Node> {
    let (list, _) = rupg_sql::parse(text).map_err(sql_error)?;
    list.into_iter()
        .flatten()
        .find_map(|node| match node {
            Node::RawStmt(raw) => raw.stmt,
            _ => None,
        })
        .ok_or_else(|| Error::new(SqlState::SYNTAX_ERROR, "the query has no statement"))
}

/// True for a statement that has no plan, which SPI runs with `ProcessUtility`.
fn is_utility(node: &Node) -> bool {
    !matches!(
        node,
        Node::SelectStmt(_)
            | Node::InsertStmt(_)
            | Node::UpdateStmt(_)
            | Node::DeleteStmt(_)
            | Node::MergeStmt(_)
            | Node::CallStmt(_)
    )
}

/// The plan of a query, or the parse tree of a statement that has no plan.
enum Planned {
    Query(Box<Plan>),
    Utility(Node),
}

/// The fields of a row type with no dropped column.
fn type_fields(env: &dyn rupg_analyze::Env, ty: u32) -> Option<Rc<[Field]>> {
    let fields = rupg_analyze::row_type_fields(env, ty)?;
    Some(fields.into_iter().filter(|(_, ty, _)| *ty != 0).collect())
}

/// True for a type whose values are rows.
fn is_composite(env: &dyn rupg_analyze::Env, ty: u32) -> bool {
    ty == oid::RECORD || rupg_analyze::row_type_fields(env, ty).is_some()
}

/// The type of what a binding reads, as `plpgsql_exec_get_datum_type_info` gives it.
fn binding_type(func: &Function, slots: &[Slot], b: &Binding) -> Result<(u32, i32)> {
    match b {
        Binding::Datum(d) => Ok(match &slots[*d] {
            Slot::Var { ty, typmod, .. } => (*ty, *typmod),
            Slot::Rec { ty, .. } => (*ty, -1),
            Slot::Row => (oid::RECORD, -1),
        }),
        Binding::Field(d, name) => {
            let field = rec_field(func, slots, *d, name)?;
            Ok((field.1, field.2))
        }
    }
}

/// The place, the type and the typmod of the field `name` of the record `d`.
fn rec_field(func: &Function, slots: &[Slot], d: usize, name: &str) -> Result<(usize, u32, i32)> {
    let rec = &func.datums[d].name;
    let Slot::Rec { fields, .. } = &slots[d] else {
        return Err(Error::internal(format!("variable \"{rec}\" is not a record")));
    };
    let Some(fields) = fields else {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("record \"{rec}\" is not assigned yet"),
        )
        .with_detail("The tuple structure of a not-yet-assigned record is indeterminate."));
    };
    match fields.iter().position(|f| f.0 == name) {
        Some(i) => Ok((i, fields[i].1, fields[i].2)),
        None => Err(Error::new(
            SqlState::UNDEFINED_COLUMN,
            format!("record \"{rec}\" has no field \"{name}\""),
        )),
    }
}

/// The variables that a query of the function names, as `plpgsql_post_column_ref` and `plpgsql_param_ref` find them. The same variable keeps the same parameter.
struct Resolver<'r> {
    func: &'r Function,
    slots: &'r [Slot],
    /// The last name that the expression sees.
    ns: Option<usize>,
    bindings: RefCell<Vec<(Binding, u32, i32)>>,
}

impl Resolver<'_> {
    fn bind(&self, b: Binding) -> Result<Variable> {
        let (ty, typmod) = binding_type(self.func, self.slots, &b)?;
        let mut bindings = self.bindings.borrow_mut();
        let param = match bindings.iter().position(|(x, ..)| *x == b) {
            Some(i) => i + 1,
            None => {
                bindings.push((b, ty, typmod));
                bindings.len()
            }
        };
        Ok(Variable { param, ty, typmod })
    }

    fn is_rec(&self, d: usize) -> bool {
        matches!(self.func.datums[d].kind, DatumKind::Rec { .. })
    }
}

impl Variables for Resolver<'_> {
    fn column(&self, names: &[&str], at: Option<usize>) -> Result<Option<Variable>> {
        let _ = at;
        let f = self.func;
        let Some((d, used)) = ns_lookup(&f.ns, &f.datums, self.ns, false, names) else {
            return Ok(None);
        };
        let b = match (names.len(), used) {
            (2, 1) if self.is_rec(d) => Binding::Field(d, names[1].to_owned()),
            (1, _) | (2, _) => Binding::Datum(d),
            (3, 2) if self.is_rec(d) => Binding::Field(d, names[2].to_owned()),
            _ => return Ok(None),
        };
        self.bind(b).map(Some)
    }

    fn param(&self, number: usize) -> Option<Variable> {
        let f = self.func;
        let name = format!("${number}");
        let (d, _) = ns_lookup(&f.ns, &f.datums, self.ns, false, &[&name])?;
        self.bind(Binding::Datum(d)).ok()
    }
}

/// `PLpgSQL_execstate`: the state of one call.
struct Exec<'a> {
    spi: &'a dyn Spi,
    c: &'a Compiled,
    f: &'a Function,
    /// The place of the call in the frames of the cache.
    depth: usize,
    slots: Vec<Slot>,
    /// `eval_processed`: the rows of the last statement, for `GET DIAGNOSTICS`.
    row_count: u64,
    /// `cur_error`: the errors of the handlers that run, the innermost last.
    errors: Vec<Error>,
    /// The rows of a function that returns a set.
    rows: Vec<Value>,
    /// The value of `RETURN`, with its type and typmod.
    retval: Option<(Value, u32, i32)>,
    /// The actual result type of the call.
    ret: u32,
}

/// `plpgsql_exec_function`: a call of the compiled function `c` with the values `args` of the types `types`, for the result type `ret`. It gives the value, or the values of a function that returns a set.
pub(crate) fn run(
    spi: &dyn Spi,
    c: &Compiled,
    types: &[u32],
    ret: u32,
    args: &[Value],
) -> Result<Vec<Value>> {
    enter(spi, c, ret, |x| x.function(types, args))
}

/// `plpgsql_exec_trigger`: a call of the compiled trigger function `c` for the event `call`. It gives the row that the trigger returns, or `None` for a null row and for a trigger `FOR EACH STATEMENT`.
pub(crate) fn run_trigger(
    spi: &dyn Spi,
    c: &Compiled,
    call: &TriggerCall<'_>,
) -> Result<Option<Vec<Value>>> {
    enter(spi, c, oid::TRIGGER, |x| x.trigger(call))
}

/// A call on the stack of the calls: the frame of the call, the state and the run `body`.
fn enter<T>(
    spi: &dyn Spi,
    c: &Compiled,
    ret: u32,
    body: impl FnOnce(&mut Exec<'_>) -> Run<T>,
) -> Result<T> {
    let depth = spi.cache().frames.borrow().len();
    if depth >= MAX_DEPTH {
        return Err(Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
            .with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."));
    }
    spi.cache().frames.borrow_mut().push(Frame { sig: Rc::clone(&c.sig), at: None, spi: None });
    let mut x = Exec {
        spi,
        c,
        f: &c.func,
        depth,
        slots: Vec::new(),
        row_count: 0,
        errors: Vec::new(),
        rows: Vec::new(),
        retval: None,
        ret,
    };
    let result = body(&mut x);
    spi.cache().frames.borrow_mut().truncate(depth);
    let sig = &c.sig;
    result.map_err(|fail| fail.framed(|| format!("PL/pgSQL function {sig}")))
}

impl Exec<'_> {
    fn env(&self) -> &dyn rupg_analyze::Env {
        self.spi.env()
    }

    fn session(&self) -> &dyn Session {
        self.spi.session()
    }

    /// The context line of the call with no statement, and the words `text`.
    fn line_text(&self, line: Option<u32>, text: &str) -> String {
        match line {
            Some(line) => format!("PL/pgSQL function {} line {line} {text}", self.c.sig),
            None => format!("PL/pgSQL function {} {text}", self.c.sig),
        }
    }

    /// The statement that runs, `err_stmt`.
    fn set_at(&self, at: Option<(u32, &'static str)>) {
        self.spi.cache().frames.borrow_mut()[self.depth].at = at;
    }

    fn at(&self) -> Option<(u32, &'static str)> {
        self.spi.cache().frames.borrow()[self.depth].at
    }

    fn set_spi(&self, spi: Option<String>) {
        self.spi.cache().frames.borrow_mut()[self.depth].spi = spi;
    }

    /// The context of the calls on the stack.
    fn stack(&self) -> String {
        stack(&self.spi.cache().frames.borrow())
    }

    /// The variables of the call before their values, `copy_plpgsql_datums`.
    fn init_slots(&mut self) {
        let f = self.f;
        self.slots = f
            .datums
            .iter()
            .map(|datum| match &datum.kind {
                DatumKind::Var { ty, typmod } => {
                    Slot::Var { value: Value::Null, ty: *ty, typmod: *typmod }
                }
                DatumKind::Rec { ty } => {
                    Slot::Rec { ty: *ty, fields: type_fields(self.env(), *ty), values: None }
                }
                DatumKind::Row { .. } | DatumKind::Field { .. } => Slot::Row,
            })
            .collect();
    }

    /// The call: the variables, the arguments, the top block and the result.
    fn function(&mut self, types: &[u32], args: &[Value]) -> Run<Vec<Value>> {
        let f = self.f;
        self.init_slots();
        for (i, &d) in f.args.iter().enumerate() {
            let value = args.get(i).cloned().unwrap_or(Value::Null);
            let ty = types.get(i).copied().unwrap_or(oid::UNKNOWN);
            self.assign_datum(d, value, ty, -1).map_err(|e| {
                Fail::Framed(e.with_context(
                    self.line_text(None, "while storing call arguments into local variables"),
                ))
            })?;
        }
        self.set_found(false);
        let flow = self.block(&f.body)?;
        self.set_at(None);
        if flow != Flow::Return {
            if f.retset || f.rettype == oid::VOID || f.out.is_some() {
                // `add_dummy_return`: a plain `RETURN`, which gives the `OUT` arguments of a function that does not return a set.
                let dummy = match f.out {
                    Some(out) if !f.retset => Returned::Var(out),
                    _ => Returned::Nothing,
                };
                self.return_value(&dummy)?;
            } else {
                return Err(Fail::Framed(
                    Error::new(
                        SqlState::S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                        "control reached end of function without RETURN",
                    )
                    .with_context(format!("PL/pgSQL function {}", self.c.sig)),
                ));
            }
        }
        if f.retset {
            return Ok(std::mem::take(&mut self.rows));
        }
        let value = self.result().map_err(|e| {
            Fail::Framed(e.with_context(
                self.line_text(None, "while casting return value to function's return type"),
            ))
        })?;
        Ok(vec![value])
    }

    /// The call of a trigger: the records `new` and `old` with the columns of the table, the variables of the trigger, the top block and the row of `RETURN`.
    fn trigger(&mut self, call: &TriggerCall<'_>) -> Run<Option<Vec<Value>>> {
        let f = self.f;
        self.init_slots();
        let Some(new) = f.trigger else {
            return Err(Error::internal("a trigger function with no variable new").into());
        };
        let fields: Rc<[Field]> = call.columns.iter().filter(|c| c.1 != 0).cloned().collect();
        let live = |row: &Option<Vec<Value>>| {
            row.as_ref().map(|row| {
                let columns = row.iter().zip(&call.columns);
                columns.filter(|(_, c)| c.1 != 0).map(|(v, _)| v.clone()).collect()
            })
        };
        // The rows of the trigger have the row type of the relation, as `ExecFetchSlotHeapTuple` gives them, so `new.*` has the fields of the relation.
        let catalog = self.env().catalog();
        let rel = catalog.and_then(|c| c.relation(call.trigger.relation));
        let ty = rel.map(|r| r.row_type).filter(|&ty| ty != 0).unwrap_or(oid::RECORD);
        for (d, row) in [(new, &call.new), (new + 1, &call.old)] {
            let fields = Some(Rc::clone(&fields));
            self.slots[d] = Slot::Rec { ty, fields, values: live(row) };
        }
        let t = call.trigger;
        let argv = if t.args.is_empty() {
            Value::Null
        } else {
            let args = t.args.iter().map(|a| Some(Value::Text(a.clone()))).collect();
            Value::Array(Box::new(Array::vector(args)))
        };
        let values = [
            Value::Text(t.name.clone()),
            Value::Text(call.when.to_owned()),
            Value::Text(call.level.to_owned()),
            Value::Text(call.op.to_owned()),
            Value::Oid(t.relation),
            Value::Text(call.relname.to_owned()),
            Value::Text(call.relname.to_owned()),
            Value::Text(call.schema.to_owned()),
            Value::Int4(i32::try_from(t.args.len()).unwrap_or(i32::MAX)),
            argv,
        ];
        for (i, value) in values.into_iter().enumerate() {
            if let Slot::Var { value: slot, .. } = &mut self.slots[new + 2 + i] {
                *slot = value;
            }
        }
        self.set_found(false);
        let flow = self.block(&f.body)?;
        self.set_at(None);
        if flow != Flow::Return {
            return Err(Fail::Framed(
                Error::new(
                    SqlState::S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                    "control reached end of trigger procedure without RETURN",
                )
                .with_context(format!("PL/pgSQL function {}", self.c.sig)),
            ));
        }
        let Some((Value::Record(record), ..)) = self.retval.take() else { return Ok(None) };
        if call.level != "ROW" {
            return Ok(None);
        }
        self.trigger_row(call, *record)
            .map(Some)
            .map_err(|e| Fail::Framed(e.with_context(self.line_text(None, "during function exit"))))
    }

    /// `convert_tuples_by_position` of the row that a trigger returns to the row of the table. A dropped column of the table gets a null.
    fn trigger_row(&self, call: &TriggerCall<'_>, record: Record) -> Result<Vec<Value>> {
        let mismatch =
            "returned row structure does not match the structure of the triggering table";
        let mut values = record.values.into_iter().zip(record.types);
        let mut row = Vec::with_capacity(call.columns.len());
        let (mut ins, mut outs) = (0, 0);
        let mut short = false;
        for (name, ty, _) in &call.columns {
            if *ty == 0 {
                row.push(Value::Null);
                continue;
            }
            outs += 1;
            let Some((value, from)) = values.next() else {
                short = true;
                row.push(Value::Null);
                continue;
            };
            ins += 1;
            if from != *ty {
                return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                    "Returned type {} does not match expected type {} in column \"{name}\" (position {outs}).",
                    types::name(from),
                    types::name(*ty)
                )));
            }
            row.push(value);
        }
        let more = values.count();
        if short || more > 0 {
            return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                "Number of returned columns ({}) does not match expected column count ({outs}).",
                ins + more
            )));
        }
        Ok(row)
    }

    /// The value of the call from the value of `RETURN`, cast to the result type.
    fn result(&mut self) -> Result<Value> {
        let Some((value, ty, typmod)) = self.retval.take() else {
            return Ok(if self.ret == oid::VOID {
                Value::Text(String::new())
            } else {
                Value::Null
            });
        };
        if self.ret == oid::VOID {
            return Ok(Value::Text(String::new()));
        }
        if self.ret == oid::RECORD {
            return match value {
                Value::Null | Value::Record(_) => Ok(value),
                _ => Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot return non-composite value from function returning composite type",
                )),
            };
        }
        if is_composite(self.env(), self.ret) {
            return self.coerce_row(
                self.ret,
                value,
                "returned record type does not match expected record type",
            );
        }
        self.cast(value, (ty, typmod), (self.ret, -1))
    }

    /// A row value of the row type `ret` from `value`, with each field cast to the type of the field of `ret`.
    fn coerce_row(&self, ret: u32, value: Value, mismatch: &str) -> Result<Value> {
        let record = match value {
            Value::Null => return Ok(Value::Null),
            Value::Record(record) => record,
            _ => {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot return non-composite value from function returning composite type",
                ));
            }
        };
        let fields = type_fields(self.env(), ret).unwrap_or_else(|| Rc::from(Vec::new()));
        if record.values.len() != fields.len() {
            return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                "Number of returned columns ({}) does not match expected column count ({}).",
                record.values.len(),
                fields.len()
            )));
        }
        let mut values = Vec::with_capacity(fields.len());
        for (i, value) in record.values.into_iter().enumerate() {
            let from = record.types.get(i).copied().unwrap_or(oid::UNKNOWN);
            values.push(self.cast(value, (from, -1), (fields[i].1, fields[i].2))?);
        }
        Ok(Value::Record(Box::new(Record::new(fields.iter().map(|f| f.1).collect(), values))))
    }

    /// `exec_stmts`: the statements in order. The error of a statement gets the context of the statement where it started.
    fn stmts(&mut self, stmts: &[Stmt]) -> Run<Flow> {
        for s in stmts {
            let saved = self.at();
            self.set_at(Some((s.line, s.kind.name())));
            let flow = match self.stmt(s) {
                Ok(flow) => flow,
                Err(fail) => return Err(Fail::Framed(fail.framed(|| self.frame_line()))),
            };
            self.set_at(saved);
            if flow != Flow::Normal {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    /// The context line of the statement that runs.
    fn frame_line(&self) -> String {
        self.spi.cache().frames.borrow()[self.depth].line()
    }

    /// `exec_stmt`.
    fn stmt(&mut self, s: &Stmt) -> Run<Flow> {
        self.session().interrupted()?;
        match &s.kind {
            StmtKind::Block(b) => self.block(b),
            StmtKind::Assign { target, expr } => {
                let (value, ty, typmod) = self.eval(expr)?;
                self.assign(target, value, ty, typmod)?;
                Ok(Flow::Normal)
            }
            StmtKind::If { cond, then, elsifs, otherwise } => {
                if self.eval_bool(cond)? {
                    return self.stmts(then);
                }
                for (cond, body) in elsifs {
                    if self.eval_bool(cond)? {
                        return self.stmts(body);
                    }
                }
                match otherwise {
                    Some(body) => self.stmts(body),
                    None => Ok(Flow::Normal),
                }
            }
            StmtKind::Case { test, whens, otherwise } => {
                if let Some((test, var)) = test {
                    let (value, ty, typmod) = self.eval(test)?;
                    self.slots[*var] = Slot::Var { value, ty, typmod };
                }
                for (cond, body) in whens {
                    if self.eval_bool(cond)? {
                        return self.stmts(body);
                    }
                }
                match otherwise {
                    Some(body) => self.stmts(body),
                    None => Err(Error::new(SqlState::CASE_NOT_FOUND, "case not found")
                        .with_hint("CASE statement is missing ELSE part.")
                        .into()),
                }
            }
            StmtKind::Loop { label, body } => loop {
                let flow = self.stmts(body)?;
                if let Some(flow) = after_body(flow, label.as_deref()) {
                    return Ok(flow);
                }
            },
            StmtKind::While { label, cond, body } => {
                while self.eval_bool(cond)? {
                    let flow = self.stmts(body)?;
                    if let Some(flow) = after_body(flow, label.as_deref()) {
                        return Ok(flow);
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::ForInt { label, var, reverse, lower, upper, step, body } => {
                self.for_int(label.as_deref(), *var, *reverse, lower, upper, step.as_ref(), body)
            }
            StmtKind::ForQuery { label, target, source, body } => {
                let ((rows, columns), read_only) = self.source_rows(source)?;
                self.spi.new_portal_name();
                let found = !rows.is_empty();
                if !found {
                    self.move_rows(*target, &columns, None)?;
                }
                // `exec_for_query` pins the portal of the loop.
                self.spi.pin(None, read_only);
                let out = self.for_rows(label.as_deref(), *target, (&columns, rows), body);
                self.spi.unpin();
                let out = out?;
                self.set_found(found);
                Ok(out)
            }
            StmtKind::ForEach { label, target, slice, expr, body } => {
                self.foreach(label.as_deref(), *target, *slice, expr, body)
            }
            StmtKind::Exit { exit, label, cond } => {
                if let Some(cond) = cond
                    && !self.eval_bool(cond)?
                {
                    return Ok(Flow::Normal);
                }
                Ok(if *exit { Flow::Exit(label.clone()) } else { Flow::Continue(label.clone()) })
            }
            StmtKind::Return(value) => {
                self.return_value(value)?;
                Ok(Flow::Return)
            }
            StmtKind::ReturnNext(value) => {
                self.return_value(value)?;
                Ok(Flow::Normal)
            }
            StmtKind::ReturnQuery(source) => {
                let ((rows, columns), _) = self.source_rows(source)?;
                let n = rows.len();
                for row in rows {
                    let value = self.query_row(&columns, row)?;
                    self.rows.push(value);
                }
                self.row_count = n as u64;
                self.set_found(n > 0);
                Ok(Flow::Normal)
            }
            StmtKind::Raise { level, condition, condname, message, params, options } => self.raise(
                *level,
                *condition,
                condname.as_deref(),
                message.as_deref(),
                params,
                options,
            ),
            StmtKind::Assert { cond, message } => {
                if self.session().setting("plpgsql.check_asserts").as_deref() == Some("off") {
                    return Ok(Flow::Normal);
                }
                let (value, ty, typmod) = self.eval(cond)?;
                if self.truth(value, ty, typmod)? {
                    return Ok(Flow::Normal);
                }
                let text = match message {
                    Some(m) => {
                        let (value, ty, _) = self.eval(m)?;
                        if value.is_null() { None } else { Some(self.text(&value, ty)?) }
                    }
                    None => None,
                };
                Err(Error::new(
                    SqlState::ASSERT_FAILURE,
                    text.unwrap_or_else(|| "assertion failed".to_owned()),
                )
                .into())
            }
            StmtKind::ExecSql { sql, into, strict } => {
                self.exec_sql(sql, *into, *strict)?;
                Ok(Flow::Normal)
            }
            StmtKind::Execute { query, into, strict, params } => {
                self.dynamic(query, *into, *strict, params)?;
                Ok(Flow::Normal)
            }
            StmtKind::Perform(expr) => {
                let (rows, ..) = self.execute(expr)?;
                let n = rows.len();
                self.row_count = n as u64;
                self.set_found(n > 0);
                Ok(Flow::Normal)
            }
            StmtKind::Call { expr, call: false } => {
                self.do_block(expr)?;
                Ok(Flow::Normal)
            }
            StmtKind::Call { expr, call: true } => {
                self.call(expr)?;
                Ok(Flow::Normal)
            }
            StmtKind::GetDiag { stacked, items } => {
                self.getdiag(*stacked, items)?;
                Ok(Flow::Normal)
            }
            StmtKind::Open { cursor, query, options } => {
                self.open(*cursor, query, *options)?;
                Ok(Flow::Normal)
            }
            StmtKind::Fetch { cursor, direction, count, expr, into } => {
                self.fetch(*cursor, *direction, *count, expr.as_ref(), *into)?;
                Ok(Flow::Normal)
            }
            StmtKind::Close(cursor) => {
                // `exec_stmt_close`.
                let name = self.cursor_name(*cursor)?;
                self.spi.close_cursor(&name)?;
                Ok(Flow::Normal)
            }
            StmtKind::ForCursor { label, target, cursor, args, body } => {
                self.for_cursor(label.as_deref(), *target, *cursor, args.as_ref(), body)
            }
            StmtKind::EndTransaction { commit, chain } => {
                // `exec_stmt_commit` and `exec_stmt_rollback`.
                self.spi.end_transaction(*commit, *chain)?;
                Ok(Flow::Normal)
            }
            StmtKind::Null => Ok(Flow::Normal),
        }
    }

    /// `exec_stmt_call` for `CALL`. The values of the output arguments of the procedure go back to the variables of the call, as `make_callstmt_target` finds them. `FOUND` and the row count do not change.
    fn call(&mut self, e: &Expr) -> Result<()> {
        let p = self.plan(e)?;
        let targets = p
            .plan
            .out_args()
            .iter()
            .map(|arg| self.out_target(&p, arg))
            .collect::<Result<Vec<_>>>()?;
        let (rows, ..) = self.execute(e)?;
        if targets.is_empty() {
            return Ok(());
        }
        if rows.len() > 1 {
            return Err(Error::internal("procedure call returned more than one row"));
        }
        let Some(row) = rows.into_iter().next() else { return Ok(()) };
        for ((target, value), (_, ty, typmod)) in targets.iter().zip(row).zip(columns(&p.plan)) {
            self.assign(target, value, ty, typmod)?;
        }
        Ok(())
    }

    /// The variable of `make_callstmt_target` for an output argument of `CALL`. The argument must be a variable or a field of a record that is not `CONSTANT`, with no cast.
    fn out_target(&self, p: &Prepared, arg: &rupg_analyze::OutArg) -> Result<Target> {
        let binding = match arg.expr.kind {
            ExprKind::Param(n) => p.bindings.get(n.wrapping_sub(1)).map(|(b, ..)| b),
            _ => None,
        };
        let Some(binding) = binding else {
            let message = match &arg.name {
                Some(name) => format!(
                    "procedure parameter \"{name}\" is an output parameter but corresponding argument is not writable"
                ),
                None => format!(
                    "procedure parameter {} is an output parameter but corresponding argument is not writable",
                    arg.position
                ),
            };
            return Err(Error::new(SqlState::SYNTAX_ERROR, message));
        };
        let (Binding::Datum(d) | Binding::Field(d, _)) = binding;
        let datum = &self.f.datums[*d];
        if datum.constant {
            return Err(Error::new(
                SqlState::ERROR_IN_ASSIGNMENT,
                format!("variable \"{}\" is declared CONSTANT", datum.name),
            ));
        }
        Ok(match binding {
            Binding::Datum(d) => Target::Datum(*d),
            Binding::Field(d, name) => Target::Field(*d, name.clone()),
        })
    }

    /// `exec_stmt_call` for `DO`: the code runs as `ExecuteDoStmt` runs it, with the statement as the context of its errors and messages. `FOUND` and the row count do not change.
    fn do_block(&mut self, e: &Expr) -> Result<()> {
        let node = parse_statement(&e.text).map_err(|error| spi_error(e, error))?;
        let Node::DoStmt(stmt) = &node else {
            return Err(Error::internal("a DO statement"));
        };
        let src = crate::do_code(stmt).map_err(|error| spi_error(e, error))?;
        let line = spi_line(Mode::Statement, &e.shown);
        self.set_spi(Some(line.clone()));
        let result = crate::inline(self.spi, &src);
        self.set_spi(None);
        result.map_err(|error| match error.position() {
            // An error of the compile has its place in the code, which is a part of the statement.
            Some(at) => {
                let start = e.text.find(src.as_str());
                spi_error(e, error.with_position(start.map(|start| start + at)))
            }
            None => error.with_context(line),
        })
    }

    /// `exec_stmt_block`: the variables of `DECLARE`, then the statements, in a subtransaction when the block has handlers.
    fn block(&mut self, b: &Block) -> Run<Flow> {
        for &d in &b.decls {
            self.init_decl(d)?;
        }
        let flow = match &b.exceptions {
            None => self.stmts(&b.body)?,
            Some(ex) => {
                let sub = self.spi.begin_sub();
                match self.stmts(&b.body) {
                    Ok(flow) => {
                        self.spi.release_sub(sub);
                        flow
                    }
                    Err(fail) => {
                        let error = fail.framed(|| self.frame_line());
                        self.spi.rollback_sub(sub);
                        let state = error.state();
                        let handler = ex
                            .handlers
                            .iter()
                            .find(|h| h.conds.iter().any(|c| matches_condition(*c, state)));
                        let Some(handler) = handler else {
                            return Err(Fail::Framed(error));
                        };
                        let code = Value::Text(state.as_str().to_owned());
                        let text = Value::Text(error.message().to_owned());
                        self.slots[ex.sqlstate] =
                            Slot::Var { value: code, ty: oid::TEXT, typmod: -1 };
                        self.slots[ex.sqlerrm] =
                            Slot::Var { value: text, ty: oid::TEXT, typmod: -1 };
                        self.errors.push(error);
                        let result = self.stmts(&handler.body);
                        self.errors.pop();
                        result?
                    }
                }
            }
        };
        Ok(match flow {
            Flow::Exit(Some(label)) if b.label.as_deref() == Some(label.as_str()) => Flow::Normal,
            flow => flow,
        })
    }

    /// The start value of a variable of `DECLARE`: its default, or null.
    fn init_decl(&mut self, d: usize) -> Run<()> {
        let datum = &self.f.datums[d];
        match &mut self.slots[d] {
            Slot::Var { value, .. } => *value = Value::Null,
            Slot::Rec { values, .. } => *values = None,
            Slot::Row => {}
        }
        if let Some(default) = &datum.default {
            let result =
                self.eval(default).and_then(|(v, ty, typmod)| self.assign_datum(d, v, ty, typmod));
            result.map_err(|e| {
                Fail::Framed(e.with_context(self.line_text(
                    Some(datum.line),
                    "during statement block local variable initialization",
                )))
            })?;
        }
        Ok(())
    }

    /// `exec_stmt_fori`.
    #[allow(clippy::too_many_arguments)]
    fn for_int(
        &mut self,
        label: Option<&str>,
        var: usize,
        reverse: bool,
        lower: &Expr,
        upper: &Expr,
        step: Option<&Expr>,
        body: &[Stmt],
    ) -> Run<Flow> {
        let lower = self.eval_int(lower, "lower bound of FOR loop cannot be null")?;
        let upper = self.eval_int(upper, "upper bound of FOR loop cannot be null")?;
        let step = match step {
            Some(step) => {
                let step = self.eval_int(step, "BY value of FOR loop cannot be null")?;
                if step <= 0 {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "BY value of FOR loop must be greater than zero",
                    )
                    .into());
                }
                step
            }
            None => 1,
        };
        let mut i = lower;
        let mut found = false;
        let mut out = Flow::Normal;
        loop {
            if (reverse && i < upper) || (!reverse && i > upper) {
                break;
            }
            found = true;
            self.slots[var] = Slot::Var { value: Value::Int4(i), ty: oid::INT4, typmod: -1 };
            let flow = self.stmts(body)?;
            if let Some(flow) = after_body(flow, label) {
                out = flow;
                break;
            }
            let next = if reverse { i.checked_sub(step) } else { i.checked_add(step) };
            match next {
                Some(next) => i = next,
                None => break,
            }
        }
        self.set_found(found);
        Ok(out)
    }

    /// `exec_stmt_foreach_a`. With `SLICE n`, each value of the loop is an array of the last `n` dimensions, with their lower bounds, as `array_iterate` gives it.
    fn foreach(
        &mut self,
        label: Option<&str>,
        target: usize,
        slice: i32,
        expr: &Expr,
        body: &[Stmt],
    ) -> Run<Flow> {
        let (value, ty, typmod) = self.eval(expr)?;
        let array = match value {
            Value::Null => {
                return Err(Error::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "FOREACH expression must not be null",
                )
                .into());
            }
            Value::Array(array) => array,
            _ => {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("FOREACH expression must yield an array, not type {}", types::name(ty)),
                )
                .into());
            }
        };
        let ndim = array.dims.len();
        let Some(slice) = usize::try_from(slice).ok().filter(|&n| n <= ndim) else {
            return Err(Error::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                format!("slice dimension ({slice}) is out of the valid range 0..{ndim}"),
            )
            .into());
        };
        // A record or a row is not an array, and it can have no type yet.
        let var_elem = match &self.f.datums[target].kind {
            DatumKind::Rec { .. } | DatumKind::Row { .. } => 0,
            DatumKind::Var { .. } => {
                types::element(binding_type(self.f, &self.slots, &Binding::Datum(target))?.0)
            }
            DatumKind::Field { rec, name } => {
                types::element(rec_field(self.f, &self.slots, *rec, name)?.1)
            }
        };
        if slice > 0 && var_elem == 0 {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                "FOREACH ... SLICE loop variable must be of an array type",
            )
            .into());
        }
        if slice == 0 && var_elem != 0 {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                "FOREACH loop variable must not be of an array type",
            )
            .into());
        }
        let values: Vec<(Value, u32)> = if slice == 0 {
            let elem = types::element(ty);
            array.values.into_iter().map(|value| (value.unwrap_or(Value::Null), elem)).collect()
        } else {
            let dims = array.dims[ndim - slice..].to_vec();
            let n = dims.iter().map(|d| usize::try_from(d.len).unwrap_or(0)).product::<usize>();
            array
                .values
                .chunks(n.max(1))
                .map(|values| {
                    let part = Array { dims: dims.clone(), values: values.to_vec() };
                    (Value::Array(Box::new(part)), ty)
                })
                .collect()
        };
        let mut found = false;
        let mut out = Flow::Normal;
        for (value, vty) in values {
            found = true;
            self.assign_datum(target, value, vty, typmod)?;
            let flow = self.stmts(body)?;
            if let Some(flow) = after_body(flow, label) {
                out = flow;
                break;
            }
        }
        self.set_found(found);
        Ok(out)
    }

    /// The rows of the query of a `FOR` loop or of `RETURN QUERY`, with the columns.
    fn source_rows(&mut self, source: &Source) -> Result<(Rows, bool)> {
        match source {
            Source::Query(e) => {
                let (rows, _, p) = self.execute(e)?;
                Ok(((rows, columns(&p.plan)), p.plan.write().is_none()))
            }
            Source::Dynamic { query, params } => {
                let (rows, _, plan) = self.run_dynamic(query, params)?;
                Ok(((rows, columns(&plan)), plan.write().is_none()))
            }
        }
    }

    /// The body of a loop over the rows of a query, for each row.
    fn for_rows(
        &mut self,
        label: Option<&str>,
        target: usize,
        (columns, rows): (&[Field], Vec<Vec<Value>>),
        body: &[Stmt],
    ) -> Run<Flow> {
        for row in rows {
            self.move_rows(target, columns, Some(row))?;
            let flow = self.stmts(body)?;
            if let Some(flow) = after_body(flow, label) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    /// A row of `RETURN QUERY` as a value of the result type.
    fn query_row(&self, columns: &[Field], row: Vec<Value>) -> Result<Value> {
        let mismatch = "structure of query does not match function result type";
        if self.ret == oid::RECORD {
            return Ok(Value::Record(Box::new(Record::new(
                columns.iter().map(|c| c.1).collect(),
                row,
            ))));
        }
        if let Some(fields) = type_fields(self.env(), self.ret) {
            if fields.len() != row.len() {
                return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                    "Number of returned columns ({}) does not match expected column count ({}).",
                    row.len(),
                    fields.len()
                )));
            }
            for (i, (field, column)) in fields.iter().zip(columns).enumerate() {
                if field.1 != column.1 {
                    return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(
                        format!(
                            "Returned type {} does not match expected type {} in column \"{}\" (position {}).",
                            types::name(column.1),
                            types::name(field.1),
                            field.0,
                            i + 1
                        ),
                    ));
                }
            }
            return Ok(Value::Record(Box::new(Record::new(
                fields.iter().map(|f| f.1).collect(),
                row,
            ))));
        }
        if row.len() != 1 {
            return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                "Number of returned columns ({}) does not match expected column count (1).",
                row.len()
            )));
        }
        let ty = columns.first().map_or(oid::UNKNOWN, |c| c.1);
        if ty != self.ret {
            return Err(Error::new(SqlState::DATATYPE_MISMATCH, mismatch).with_detail(format!(
                "Returned type {} does not match expected type {} in column 1.",
                types::name(ty),
                types::name(self.ret)
            )));
        }
        Ok(row.into_iter().next().unwrap_or(Value::Null))
    }

    /// `exec_stmt_return` and `exec_stmt_return_next`: the value of `RETURN`, or a row of the set.
    fn return_value(&mut self, value: &Returned) -> Result<()> {
        let (value, ty, typmod) = match value {
            Returned::Nothing => {
                if !self.f.retset {
                    self.retval = None;
                }
                return Ok(());
            }
            Returned::Var(d) => self.datum_value(*d),
            Returned::Expr(e) => self.eval(e)?,
        };
        if self.f.trigger.is_some() && !matches!(value, Value::Null | Value::Record(_)) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                "cannot return non-composite value from function returning composite type",
            ));
        }
        if !self.f.retset {
            self.retval = Some((value, ty, typmod));
            return Ok(());
        }
        let row = if self.ret == oid::RECORD {
            match value {
                Value::Record(_) => value,
                Value::Null => Value::Null,
                _ => {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        "cannot return non-composite value from function returning composite type",
                    ));
                }
            }
        } else if is_composite(self.env(), self.ret) {
            self.coerce_row(self.ret, value, "wrong record type supplied in RETURN NEXT")?
        } else {
            self.cast(value, (ty, typmod), (self.ret, -1))?
        };
        self.rows.push(row);
        Ok(())
    }

    /// `exec_stmt_raise`.
    fn raise(
        &mut self,
        level: Option<rupg_common::Level>,
        condition: Option<SqlState>,
        condname: Option<&str>,
        message: Option<&str>,
        params: &[Expr],
        options: &[(RaiseOption, Expr)],
    ) -> Run<Flow> {
        if condition.is_none() && message.is_none() && options.is_empty() {
            return match self.errors.last() {
                Some(error) => Err(Fail::Framed(error.clone())),
                None => Err(Error::new(
                    SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                    "RAISE without parameters cannot be used outside an exception handler",
                )
                .into()),
            };
        }
        let mut code = condition;
        let mut text = match message {
            Some(message) => Some(self.format(message, params)?),
            None => None,
        };
        let (mut detail, mut hint) = (None, None);
        let mut objects = Vec::new();
        let mut seen = Vec::new();
        for (option, expr) in options {
            let (value, ty, _) = self.eval(expr)?;
            if value.is_null() {
                return Err(Error::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "RAISE statement option cannot be null",
                )
                .into());
            }
            let value = self.text(&value, ty)?;
            let taken = match option {
                RaiseOption::Errcode => code.is_some(),
                RaiseOption::Message => text.is_some(),
                RaiseOption::Detail => detail.is_some(),
                RaiseOption::Hint => hint.is_some(),
                _ => seen.contains(option),
            };
            if taken {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("RAISE option already specified: {}", option_name(*option)),
                )
                .into());
            }
            seen.push(*option);
            match option {
                RaiseOption::Errcode => code = Some(errcode(&value)?),
                RaiseOption::Message => text = Some(value),
                RaiseOption::Detail => detail = Some(value),
                RaiseOption::Hint => hint = Some(value),
                RaiseOption::Column => objects.push((Object::Column, value)),
                RaiseOption::Constraint => objects.push((Object::Constraint, value)),
                RaiseOption::Datatype => objects.push((Object::Datatype, value)),
                RaiseOption::Table => objects.push((Object::Table, value)),
                RaiseOption::Schema => objects.push((Object::Schema, value)),
            }
        }
        if level.is_none() && code.is_none() {
            code = Some(SqlState::RAISE_EXCEPTION);
        }
        let text = match (text, condname) {
            (Some(text), _) => text,
            (None, Some(name)) => name.to_owned(),
            (None, None) => code.map(|c| c.as_str().to_owned()).unwrap_or_default(),
        };
        let state = code.unwrap_or(match level {
            Some(rupg_common::Level::Warning) => SqlState::WARNING,
            _ => SqlState::SUCCESSFUL_COMPLETION,
        });
        let mut error = Error::new(state, text);
        if let Some(detail) = detail {
            error = error.with_detail(detail);
        }
        if let Some(hint) = hint {
            error = error.with_hint(hint);
        }
        for (object, name) in objects {
            error = error.with_object(object, name);
        }
        match level {
            None => Err(error.into()),
            Some(level) => {
                self.spi.notice(error.with_level(level).with_context(self.stack()));
                Ok(Flow::Normal)
            }
        }
    }

    /// The message of `RAISE`, with the text of a parameter for each `%`, and `%%` for `%`.
    fn format(&mut self, message: &str, params: &[Expr]) -> Result<String> {
        let mut out = String::new();
        let mut params = params.iter();
        let mut chars = message.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'%') {
                chars.next();
                out.push('%');
                continue;
            }
            let Some(param) = params.next() else {
                return Err(Error::internal("unexpected RAISE parameter list length"));
            };
            let (value, ty, _) = self.eval(param)?;
            if value.is_null() {
                out.push_str("<NULL>");
            } else {
                out.push_str(&self.text(&value, ty)?);
            }
        }
        Ok(out)
    }

    /// `exec_stmt_getdiag`.
    fn getdiag(&mut self, stacked: bool, items: &[(usize, DiagItem)]) -> Result<()> {
        if stacked && self.errors.is_empty() {
            return Err(Error::new(
                SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                "GET STACKED DIAGNOSTICS cannot be used outside an exception handler",
            ));
        }
        for &(d, item) in items {
            let text = |s: Option<&str>| (Value::Text(s.unwrap_or_default().to_owned()), oid::TEXT);
            let error = self.errors.last();
            let (value, ty) = match item {
                DiagItem::RowCount => {
                    (Value::Int8(i64::try_from(self.row_count).unwrap_or(i64::MAX)), oid::INT8)
                }
                DiagItem::RoutineOid => (Value::Oid(self.c.oid), oid::OID),
                DiagItem::Context => (Value::Text(self.stack()), oid::TEXT),
                DiagItem::ReturnedSqlstate => (
                    Value::Text(error.map(|e| e.state().as_str().to_owned()).unwrap_or_default()),
                    oid::TEXT,
                ),
                DiagItem::MessageText => text(error.map(Error::message)),
                DiagItem::ErrorDetail => text(error.and_then(Error::detail)),
                DiagItem::ErrorHint => text(error.and_then(Error::hint)),
                DiagItem::ErrorContext => text(error.and_then(Error::context)),
                DiagItem::ColumnName => text(error.and_then(|e| e.object(Object::Column))),
                DiagItem::ConstraintName => text(error.and_then(|e| e.object(Object::Constraint))),
                DiagItem::DatatypeName => text(error.and_then(|e| e.object(Object::Datatype))),
                DiagItem::TableName => text(error.and_then(|e| e.object(Object::Table))),
                DiagItem::SchemaName => text(error.and_then(|e| e.object(Object::Schema))),
            };
            self.assign_datum(d, value, ty, -1)?;
        }
        Ok(())
    }

    /// `exec_stmt_execsql`: a statement of SQL, with the rows into the target of `INTO`.
    fn exec_sql(&mut self, sql: &Expr, into: Option<usize>, strict: bool) -> Result<()> {
        if let Some(node) = self.utility(sql)? {
            check_utility(&node, "unsupported transaction command in PL/pgSQL")?;
            let result = self.run_utility(&node, &sql.text, spi_line(sql.mode, &sql.shown));
            self.row_count = result.map_err(|error| spi_error(sql, error))?;
            return no_into(into);
        }
        let (rows, n, p) = self.execute(sql)?;
        let write = p.plan.write().is_some();
        let Some(target) = into else {
            if !p.plan.columns().is_empty() {
                let error =
                    Error::new(SqlState::SYNTAX_ERROR, "query has no destination for result data");
                return Err(if write {
                    error
                } else {
                    error.with_hint(
                        "If you want to discard the results of a SELECT, use PERFORM instead.",
                    )
                });
            }
            self.row_count = n;
            self.set_found(n > 0);
            return Ok(());
        };
        let processed = if write { n } else { n.min(1) };
        self.rows_into(target, &columns(&p.plan), rows, strict, write)?;
        self.row_count = processed;
        self.set_found(processed > 0);
        Ok(())
    }

    /// The statement `node` with no plan. Its notices get the context line `line` and the lines of the calls.
    fn run_utility(&mut self, node: &Node, text: &str, line: String) -> Result<u64> {
        self.set_spi(Some(line));
        let mut notices = Vec::new();
        let result = self.spi.utility(node, text, &mut notices);
        let stack = self.stack();
        for notice in notices {
            self.spi.notice(notice.with_context(stack.clone()));
        }
        self.set_spi(None);
        result
    }

    /// The parse tree of the statement `e` when it is not a query, which the function keeps.
    fn utility(&self, e: &Expr) -> Result<Option<Rc<Node>>> {
        if !matches!(e.mode, Mode::Statement) {
            return Ok(None);
        }
        if let Some(known) = self.c.utilities.borrow().get(&e.id) {
            return Ok(known.clone());
        }
        let node = parse_statement(&e.text).map_err(|error| spi_error(e, error))?;
        let node = is_utility(&node).then(|| Rc::new(node));
        self.c.utilities.borrow_mut().insert(e.id, node.clone());
        Ok(node)
    }

    /// The first row of `rows` into the target of `INTO`. With `strict`, no row and more than one row are errors. For a `write` with `RETURNING`, more than one row is an error, and no row sets the target to nulls.
    fn rows_into(
        &mut self,
        target: usize,
        columns: &[Field],
        rows: Vec<Vec<Value>>,
        strict: bool,
        write: bool,
    ) -> Result<()> {
        if rows.is_empty() && strict {
            return Err(Error::new(SqlState::NO_DATA_FOUND, "query returned no rows"));
        }
        if rows.len() > 1 && (strict || write) {
            return Err(Error::new(SqlState::TOO_MANY_ROWS, "query returned more than one row")
                .with_hint("Make sure the query returns a single row, or use LIMIT 1."));
        }
        self.move_rows(target, columns, rows.into_iter().next())
    }

    /// `exec_stmt_dynexecute`.
    fn dynamic(
        &mut self,
        query: &Expr,
        into: Option<usize>,
        strict: bool,
        params: &[Expr],
    ) -> Result<()> {
        let (planned, values, text, line) = self.dynamic_planned(query, params)?;
        let plan = match planned {
            Planned::Query(plan) => *plan,
            Planned::Utility(node) => {
                check_utility(&node, "EXECUTE of transaction commands is not implemented")?;
                let result = self.run_utility(&node, &text, line.clone());
                self.row_count = result.map_err(|error| error.with_context(line))?;
                return no_into(into);
            }
        };
        self.set_spi(Some(line.clone()));
        let result = self.spi.execute(&plan, &values);
        self.set_spi(None);
        let (rows, n) = result.map_err(|error| error.with_context(line))?;
        if let Some(target) = into {
            if rows.is_empty() && strict {
                return Err(Error::new(SqlState::NO_DATA_FOUND, "query returned no rows"));
            }
            if rows.len() > 1 && strict {
                return Err(Error::new(
                    SqlState::TOO_MANY_ROWS,
                    "query returned more than one row",
                ));
            }
            self.move_rows(target, &columns(&plan), rows.into_iter().next())?;
        }
        self.row_count = n;
        Ok(())
    }

    /// The rows of the query of `EXECUTE`, with the values of `USING` as its parameters.
    fn run_dynamic(
        &mut self,
        query: &Expr,
        params: &[Expr],
    ) -> Result<(Vec<Vec<Value>>, u64, Plan)> {
        let (plan, values, _, line) = self.dynamic_plan(query, params)?;
        self.set_spi(Some(line.clone()));
        let result = self.spi.execute(&plan, &values);
        self.set_spi(None);
        let (rows, n) = result.map_err(|error| error.with_context(line))?;
        Ok((rows, n, plan))
    }

    /// The plan of the query of `EXECUTE` with the values of `USING`, the text of the query and the line of the context of its errors. A statement that is not a query is not supported here yet.
    fn dynamic_plan(
        &mut self,
        query: &Expr,
        params: &[Expr],
    ) -> Result<(Plan, Vec<Value>, String, String)> {
        let (planned, values, text, line) = self.dynamic_planned(query, params)?;
        match planned {
            Planned::Query(plan) => Ok((*plan, values, text, line)),
            Planned::Utility(_) => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "this statement is not supported yet",
            )
            .with_context(line)),
        }
    }

    /// [`Exec::dynamic_plan`] that gives the parse tree of a statement that is not a query.
    fn dynamic_planned(
        &mut self,
        query: &Expr,
        params: &[Expr],
    ) -> Result<(Planned, Vec<Value>, String, String)> {
        let (value, ty, _) = self.eval(query)?;
        if value.is_null() {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null",
            ));
        }
        let text = self.text(&value, ty)?;
        let mut values = Vec::with_capacity(params.len());
        let mut types = Vec::with_capacity(params.len());
        for param in params {
            let (value, ty, _) = self.eval(param)?;
            values.push(value);
            types.push(if ty == oid::UNKNOWN { oid::TEXT } else { ty });
        }
        let line = spi_line(Mode::Statement, &text);
        let planned = (|| {
            let node = parse_statement(&text)?;
            if is_utility(&node) {
                return Ok(Planned::Utility(node));
            }
            let query =
                rupg_analyze::analyze(&node, self.env(), &Params { types, variable: false })?;
            rupg_exec::prepare(query)?
                .fold(None, self.session())
                .map(|plan| Planned::Query(Box::new(plan)))
        })()
        .map_err(|error| match error.position() {
            Some(at) => error.with_position(None).with_internal(text.clone(), Some(at)),
            None => error.with_context(line.clone()),
        })?;
        Ok((planned, values, text, line))
    }

    /// The name of the portal in the cursor variable `d`, which must not be null.
    fn cursor_name(&self, d: usize) -> Result<String> {
        let (value, ty, _) = self.datum_value(d);
        if value.is_null() {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!("cursor variable \"{}\" is null", self.f.datums[d].name),
            ));
        }
        self.text(&value, ty)
    }

    /// The name in the cursor variable `d` for a new portal, or `None` for a null variable. A portal with the name must not be open.
    fn free_cursor(&self, d: usize) -> Result<Option<String>> {
        if self.datum_value(d).0.is_null() {
            return Ok(None);
        }
        let name = self.cursor_name(d)?;
        if self.spi.cursor_exists(&name) {
            return Err(Error::new(
                SqlState::DUPLICATE_CURSOR,
                format!("cursor \"{name}\" already in use"),
            ));
        }
        Ok(Some(name))
    }

    /// The portal of the query `e` with the name `name`, or with a new name for `None`.
    fn open_query(&mut self, name: Option<&str>, e: &Expr, options: i32) -> Result<String> {
        let p = self.plan(e)?;
        cursor_query(&p.plan)?;
        let params =
            p.bindings.iter().map(|(b, ..)| self.binding_value(b)).collect::<Result<Vec<_>>>()?;
        self.set_spi(Some(spi_line(e.mode, &e.shown)));
        let result = self.spi.open_cursor(name, &p.plan, &params, &e.text, options);
        self.set_spi(None);
        result.map_err(|error| spi_error(e, error))
    }

    /// The portal of the bound cursor `d`. The values of the arguments `args` go to the variables of the arguments first.
    fn open_bound(&mut self, d: usize, name: Option<&str>, args: Option<&Expr>) -> Result<String> {
        let f = self.f;
        let Some(bound) = f.datums[d].cursor.as_deref() else {
            return Err(Error::internal("the cursor variable has no query"));
        };
        if let (Some(args), Some(row)) = (args, bound.args) {
            self.exec_sql(args, Some(row), false)?;
        }
        self.open_query(name, &bound.query, bound.options)
    }

    /// `exec_stmt_open`.
    fn open(&mut self, d: usize, query: &Opened, options: i32) -> Result<()> {
        let name = self.free_cursor(d)?;
        let opened = match query {
            Opened::Bound(args) => self.open_bound(d, name.as_deref(), args.as_ref())?,
            Opened::Query(e) => self.open_query(name.as_deref(), e, options)?,
            Opened::Dynamic { query, params } => {
                let (plan, values, text, line) = self.dynamic_plan(query, params)?;
                cursor_query(&plan)?;
                self.set_spi(Some(line.clone()));
                let result = self.spi.open_cursor(name.as_deref(), &plan, &values, &text, options);
                self.set_spi(None);
                result.map_err(|error| error.with_context(line))?
            }
        };
        // A null variable gets the name of the new portal.
        if name.is_none() {
            self.assign_datum(d, Value::Text(opened), oid::TEXT, -1)?;
        }
        Ok(())
    }

    /// `exec_stmt_fetch`: `FETCH` into the target `into`, or `MOVE` for `None`.
    fn fetch(
        &mut self,
        d: usize,
        direction: FetchDirection,
        count: i64,
        expr: Option<&Expr>,
        into: Option<usize>,
    ) -> Result<()> {
        let name = self.cursor_name(d)?;
        if !self.spi.cursor_exists(&name) {
            return Err(Error::new(
                SqlState::UNDEFINED_CURSOR,
                format!("cursor \"{name}\" does not exist"),
            ));
        }
        let count = match expr {
            Some(e) => i64::from(self.eval_int(e, "relative or absolute cursor position is null")?),
            None => count,
        };
        let fetched = self.spi.fetch_cursor(&name, direction, count, into.is_some())?;
        if let Some(target) = into {
            self.move_rows(target, &fetched.columns, fetched.rows.into_iter().next())?;
        }
        self.row_count = fetched.count;
        self.set_found(fetched.count != 0);
        Ok(())
    }

    /// The body of a loop over the rows of the cursor `opened`, for each row that a fetch gives. It gives true when there was a row.
    fn cursor_rows(
        &mut self,
        label: Option<&str>,
        target: usize,
        opened: &str,
        body: &[Stmt],
    ) -> Run<(bool, Flow)> {
        let mut found = false;
        loop {
            let fetched = self.spi.fetch_cursor(opened, FetchDirection::FETCH_FORWARD, 1, true)?;
            let Some(row) = fetched.rows.into_iter().next() else {
                if !found {
                    self.move_rows(target, &fetched.columns, None)?;
                }
                return Ok((found, Flow::Normal));
            };
            found = true;
            self.move_rows(target, &fetched.columns, Some(row))?;
            let flow = self.stmts(body)?;
            if let Some(flow) = after_body(flow, label) {
                return Ok((found, flow));
            }
        }
    }

    /// `exec_stmt_forc`: the loop over the rows of the bound cursor `d`, one row at a time, as the body can read the cursor too. The portal closes after the loop, and a variable that was null is null again.
    fn for_cursor(
        &mut self,
        label: Option<&str>,
        target: usize,
        d: usize,
        args: Option<&Expr>,
        body: &[Stmt],
    ) -> Run<Flow> {
        let name = self.free_cursor(d)?;
        let opened = self.open_bound(d, name.as_deref(), args)?;
        if name.is_none() {
            self.assign_datum(d, Value::Text(opened.clone()), oid::TEXT, -1)?;
        }
        self.spi.pin(Some(&opened), true);
        let looped = self.cursor_rows(label, target, &opened, body);
        self.spi.unpin();
        let (found, out) = looped?;
        self.spi.close_cursor(&opened)?;
        if name.is_none() {
            self.slots[d] = Slot::Var { value: Value::Null, ty: oid::REFCURSOR, typmod: -1 };
        }
        self.set_found(found);
        Ok(out)
    }

    /// The plan of the expression `e`, made again when the type of a variable that it reads changed.
    fn plan(&self, e: &Expr) -> Result<Rc<Prepared>> {
        let cached = self.c.plans.borrow().get(e.id).cloned().flatten();
        if let Some(p) = cached
            && p.changes == self.spi.changes()
            && p.bindings.iter().all(|(b, ty, typmod)| {
                binding_type(self.f, &self.slots, b).ok() == Some((*ty, *typmod))
            })
        {
            return Ok(p);
        }
        let p = Rc::new(self.make_plan(e).map_err(|error| spi_error(e, error))?);
        let mut plans = self.c.plans.borrow_mut();
        if plans.len() <= e.id {
            plans.resize(e.id + 1, None);
        }
        plans[e.id] = Some(Rc::clone(&p));
        Ok(p)
    }

    /// `exec_prepare_plan`: the parse, the analysis and the plan of `e`.
    fn make_plan(&self, e: &Expr) -> Result<Prepared> {
        let node = match e.mode {
            Mode::Statement => parse_statement(&e.text)?,
            Mode::Expr | Mode::Assign => {
                rupg_sql::parse_plpgsql_expr(&e.text).map_err(sql_error)?
            }
            Mode::Part(names) => {
                rupg_sql::parse_plpgsql_assign(&e.text, names).map_err(sql_error)?
            }
        };
        let resolver =
            Resolver { func: self.f, slots: &self.slots, ns: e.ns, bindings: RefCell::default() };
        let datum = e.target.map(|d| &self.f.datums[d]);
        let query = match datum.map(|d| (&d.kind, &d.name)) {
            _ if matches!(e.mode, Mode::Part(_)) => {
                rupg_analyze::analyze_assign_part(&node, self.env(), &resolver)?
            }
            Some((&DatumKind::Var { ty, typmod }, name)) if ty != 0 => {
                rupg_analyze::analyze_assign(&node, self.env(), &resolver, (ty, typmod), name)?
            }
            _ => rupg_analyze::analyze_with(&node, self.env(), &Params::default(), &resolver)?,
        };
        let simple = e.mode != Mode::Statement && is_simple(&query);
        let plan = rupg_exec::prepare(query)?.fold(None, self.session())?;
        let changes = self.spi.changes();
        Ok(Prepared { plan, bindings: resolver.bindings.into_inner(), changes, simple })
    }

    /// The rows of the expression or statement `e`, the count of the rows that it read or wrote, and its plan.
    fn execute(&mut self, e: &Expr) -> Result<(Vec<Vec<Value>>, u64, Rc<Prepared>)> {
        let p = self.plan(e)?;
        let params =
            p.bindings.iter().map(|(b, ..)| self.binding_value(b)).collect::<Result<Vec<_>>>()?;
        let (rows, n) = if p.simple {
            self.spi.execute(&p.plan, &params)?
        } else {
            self.set_spi(Some(spi_line(e.mode, &e.shown)));
            let result = self.spi.execute(&p.plan, &params);
            self.set_spi(None);
            result.map_err(|error| spi_error(e, error))?
        };
        Ok((rows, n, p))
    }

    /// `exec_eval_expr`: the value of the expression `e`, with its type and typmod.
    fn eval(&mut self, e: &Expr) -> Result<(Value, u32, i32)> {
        let (rows, _, p) = self.execute(e)?;
        let columns = p.plan.columns();
        if columns.len() != 1 {
            let n = columns.len();
            let word = if n == 1 { "column" } else { "columns" };
            return Err(Error::new(SqlState::SYNTAX_ERROR, format!("query returned {n} {word}")));
        }
        if rows.len() > 1 {
            return Err(Error::new(
                SqlState::CARDINALITY_VIOLATION,
                "query returned more than one row",
            ));
        }
        let (ty, typmod) = (columns[0].expr.ty, columns[0].expr.typmod);
        let value = rows.into_iter().next().and_then(|row| row.into_iter().next());
        Ok((value.unwrap_or(Value::Null), ty, typmod))
    }

    /// `exec_eval_boolean`: the value of a condition, where null is false.
    fn eval_bool(&mut self, e: &Expr) -> Result<bool> {
        let (value, ty, typmod) = self.eval(e)?;
        self.truth(value, ty, typmod)
    }

    /// True for a value that is true after the cast to `boolean`.
    fn truth(&self, value: Value, ty: u32, typmod: i32) -> Result<bool> {
        Ok(matches!(self.cast(value, (ty, typmod), (oid::BOOL, -1))?, Value::Bool(true)))
    }

    /// The value of an expression as an `integer`, which must not be null.
    fn eval_int(&mut self, e: &Expr, null: &str) -> Result<i32> {
        let (value, ty, typmod) = self.eval(e)?;
        match self.cast(value, (ty, typmod), (oid::INT4, -1))? {
            Value::Int4(n) => Ok(n),
            Value::Null => Err(Error::new(SqlState::NULL_VALUE_NOT_ALLOWED, null)),
            _ => Err(Error::internal("the cast to integer gave another type")),
        }
    }

    /// `exec_cast_value`: the value cast from one type and typmod to another, as an assignment casts it.
    fn cast(&self, value: Value, from: (u32, i32), to: (u32, i32)) -> Result<Value> {
        if value.is_null() || (from.0 == to.0 && (to.1 == -1 || from.1 == to.1)) {
            return Ok(value);
        }
        let key = (from.0, from.1, to.0, to.1);
        let cached = self.c.casts.borrow().get(&key).cloned();
        let plan = match cached {
            Some(plan) => plan,
            None => {
                let query = rupg_analyze::plpgsql_cast(self.env(), from, to.0, to.1)?;
                let plan = Rc::new(rupg_exec::prepare(query)?);
                self.c.casts.borrow_mut().insert(key, Rc::clone(&plan));
                plan
            }
        };
        let rows = plan.run(&[value], self.session())?;
        Ok(rows.into_iter().next().and_then(|row| row.into_iter().next()).unwrap_or(Value::Null))
    }

    /// `convert_value_to_string`: the text of the output function of the type for a value that is not null. A boolean gives `t` or `f`, and a `character` value keeps its spaces, which the cast to `text` does not do.
    fn text(&self, value: &Value, ty: u32) -> Result<String> {
        match value {
            Value::Bool(v) => Ok(if *v { "t" } else { "f" }.to_owned()),
            Value::Text(v) if rupg_func::base_type(ty) == oid::BPCHAR => Ok(v.clone()),
            _ => rupg_func::to_text(ty, value, self.session()),
        }
    }

    /// `exec_eval_datum`: the value of a variable, with its type and typmod.
    fn datum_value(&self, d: usize) -> (Value, u32, i32) {
        match &self.slots[d] {
            Slot::Var { value, ty, typmod } => (value.clone(), *ty, *typmod),
            Slot::Rec { ty, fields, values } => {
                let value = match values {
                    None => Value::Null,
                    Some(values) => {
                        let fields = fields.as_deref().unwrap_or_default();
                        let mut record =
                            Record::new(fields.iter().map(|f| f.1).collect(), values.clone());
                        if *ty == oid::RECORD {
                            record.names =
                                Some(fields.iter().map(|f| f.0.clone()).collect::<Arc<[String]>>());
                        }
                        Value::Record(Box::new(record))
                    }
                };
                (value, *ty, -1)
            }
            Slot::Row => {
                let fields = match &self.f.datums[d].kind {
                    DatumKind::Row { fields } => fields,
                    DatumKind::Field { rec, name } => {
                        let binding = Binding::Field(*rec, name.clone());
                        let value = self.binding_value(&binding).unwrap_or(Value::Null);
                        let (ty, typmod) = binding_type(self.f, &self.slots, &binding)
                            .unwrap_or((oid::UNKNOWN, -1));
                        return (value, ty, typmod);
                    }
                    _ => return (Value::Null, oid::RECORD, -1),
                };
                let (types, values) = fields
                    .iter()
                    .map(|&f| {
                        let (value, ty, _) = self.datum_value(f);
                        (ty, value)
                    })
                    .unzip();
                (Value::Record(Box::new(Record::new(types, values))), oid::RECORD, -1)
            }
        }
    }

    /// The value that a parameter of a query reads.
    fn binding_value(&self, b: &Binding) -> Result<Value> {
        match b {
            Binding::Datum(d) => Ok(self.datum_value(*d).0),
            Binding::Field(d, name) => {
                let (i, ..) = rec_field(self.f, &self.slots, *d, name)?;
                let Slot::Rec { values, .. } = &self.slots[*d] else {
                    return Ok(Value::Null);
                };
                Ok(values.as_ref().and_then(|v| v.get(i).cloned()).unwrap_or(Value::Null))
            }
        }
    }

    fn set_found(&mut self, found: bool) {
        let d = self.f.found;
        self.slots[d] = Slot::Var { value: Value::Bool(found), ty: oid::BOOL, typmod: -1 };
    }

    /// `exec_assign_value` for the target of an assignment.
    fn assign(&mut self, target: &Target, value: Value, ty: u32, typmod: i32) -> Result<()> {
        match target {
            Target::Datum(d) => self.assign_datum(*d, value, ty, typmod),
            Target::Field(d, name) => self.assign_field(*d, name, value, ty, typmod),
        }
    }

    /// The assignment of a value of the type `ty` to the field `name` of the record `d`. A null record becomes a record of nulls first.
    fn assign_field(
        &mut self,
        d: usize,
        name: &str,
        value: Value,
        ty: u32,
        typmod: i32,
    ) -> Result<()> {
        let (i, fty, fmod) = rec_field(self.f, &self.slots, d, name)?;
        let value = self.cast(value, (ty, typmod), (fty, fmod))?;
        if let Slot::Rec { fields, values, .. } = &mut self.slots[d] {
            let n = fields.as_ref().map_or(0, |f| f.len());
            values.get_or_insert_with(|| vec![Value::Null; n])[i] = value;
        }
        Ok(())
    }

    /// The assignment of a value of the type `ty` to the variable `d`.
    fn assign_datum(&mut self, d: usize, value: Value, ty: u32, typmod: i32) -> Result<()> {
        let datum = &self.f.datums[d];
        match &datum.kind {
            DatumKind::Var { ty: 0, .. } => {
                self.slots[d] = Slot::Var { value, ty, typmod };
                Ok(())
            }
            DatumKind::Var { ty: dty, typmod: dmod } => {
                let value = self.cast(value, (ty, typmod), (*dty, *dmod))?;
                if value.is_null() && datum.not_null {
                    return Err(Error::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        format!(
                            "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                            datum.name
                        ),
                    ));
                }
                if let Slot::Var { value: v, .. } = &mut self.slots[d] {
                    *v = value;
                }
                Ok(())
            }
            DatumKind::Rec { .. } => match value {
                Value::Null => {
                    if let Slot::Rec { values, .. } = &mut self.slots[d] {
                        *values = None;
                    }
                    Ok(())
                }
                Value::Record(record) => {
                    let Record { types, values, names } = *record;
                    let columns: Vec<Field> = match names {
                        Some(names) => types
                            .iter()
                            .zip(names.iter())
                            .map(|(ty, name)| (name.clone(), *ty, -1))
                            .collect(),
                        None => match type_fields(self.env(), ty) {
                            Some(fields) if fields.len() == types.len() => fields.to_vec(),
                            _ => types
                                .iter()
                                .enumerate()
                                .map(|(i, ty)| (format!("f{}", i + 1), *ty, -1))
                                .collect(),
                        },
                    };
                    self.move_rows(d, &columns, Some(values))
                }
                _ => Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot assign non-composite value to a record variable",
                )),
            },
            DatumKind::Row { .. } => match value {
                Value::Null => self.move_rows(d, &[], None),
                Value::Record(record) => {
                    let columns: Vec<Field> =
                        record.types.iter().map(|ty| (String::new(), *ty, -1)).collect();
                    self.move_rows(d, &columns, Some(record.values))
                }
                _ => Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot assign non-composite value to a row variable",
                )),
            },
            DatumKind::Field { rec, name } => self.assign_field(*rec, name, value, ty, typmod),
        }
    }

    /// `exec_move_row`: a row with the columns `columns` into a record or a list of variables, or nulls for no row. The values go by their place.
    fn move_rows(&mut self, d: usize, columns: &[Field], row: Option<Vec<Value>>) -> Result<()> {
        match &self.f.datums[d].kind {
            DatumKind::Rec { ty } => {
                let ty = *ty;
                // No row with the columns of a query sets the fields to nulls.
                let row = match row {
                    Some(row) => row,
                    None if !columns.is_empty() => vec![Value::Null; columns.len()],
                    None => {
                        if let Slot::Rec { values, .. } = &mut self.slots[d] {
                            *values = None;
                        }
                        return Ok(());
                    }
                };
                if ty == oid::RECORD {
                    let fields: Rc<[Field]> = columns.iter().cloned().collect();
                    self.slots[d] = Slot::Rec { ty, fields: Some(fields), values: Some(row) };
                    return Ok(());
                }
                let fields = match &self.slots[d] {
                    Slot::Rec { fields: Some(fields), .. } => Rc::clone(fields),
                    _ => Rc::from(Vec::new()),
                };
                let mut row = row.into_iter();
                let mut values = Vec::with_capacity(fields.len());
                for (i, field) in fields.iter().enumerate() {
                    let value = row.next().unwrap_or(Value::Null);
                    let from = columns.get(i).map_or((oid::UNKNOWN, -1), |c| (c.1, c.2));
                    values.push(self.cast(value, from, (field.1, field.2))?);
                }
                if let Slot::Rec { values: v, .. } = &mut self.slots[d] {
                    *v = Some(values);
                }
                Ok(())
            }
            DatumKind::Row { fields } => {
                let mut row = row.map(Vec::into_iter);
                for (i, &f) in fields.iter().enumerate() {
                    let value = row.as_mut().and_then(Iterator::next).unwrap_or(Value::Null);
                    let (ty, typmod) = columns.get(i).map_or((oid::UNKNOWN, -1), |c| (c.1, c.2));
                    self.assign_datum(f, value, ty, typmod)?;
                }
                Ok(())
            }
            DatumKind::Var { .. } | DatumKind::Field { .. } => {
                let value = row.and_then(|row| row.into_iter().next()).unwrap_or(Value::Null);
                let (ty, typmod) = columns.first().map_or((oid::UNKNOWN, -1), |c| (c.1, c.2));
                self.assign_datum(d, value, ty, typmod)
            }
        }
    }
}

/// `SPI_cursor_open_internal`: only a query that gives rows can be the query of a cursor.
fn cursor_query(plan: &Plan) -> Result<()> {
    if let Some(write) = plan.write() {
        if write.returning.is_none() {
            return Err(Error::new(
                SqlState::INVALID_CURSOR_DEFINITION,
                format!("cannot open {} query as cursor", write.kind.name()),
            ));
        }
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "a cursor over a statement that changes data is not supported yet",
        ));
    }
    if plan.modifying_cte() {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "a statement of WITH that changes data in PL/pgSQL is not supported yet",
        ));
    }
    Ok(())
}

/// `_SPI_execute_plan`: a transaction command and `COPY` to the client or from the client cannot run in a function. `transaction` is the error for a transaction command.
fn check_utility(node: &Node, transaction: &str) -> Result<()> {
    match node {
        Node::TransactionStmt(_) => {
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, transaction.to_owned()))
        }
        Node::CopyStmt(copy) if copy.filename.is_none() => Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot COPY to/from client in PL/pgSQL",
        )),
        _ => Ok(()),
    }
}

/// The error of `INTO` after a statement that gives no rows.
fn no_into(into: Option<usize>) -> Result<()> {
    match into {
        Some(_) => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            "INTO used with a command that cannot return data",
        )),
        None => Ok(()),
    }
}

/// The columns of the result of a plan: the name, the type and the typmod of each.
fn columns(plan: &Plan) -> Vec<Field> {
    plan.columns().iter().map(|t| (t.name.clone(), t.expr.ty, t.expr.typmod)).collect()
}

/// `exception_matches_conditions`: true when the condition of a handler catches the code `state`. `OTHERS` does not catch a cancel or a failed assertion, and a code that ends with `000` catches its class.
fn matches_condition(cond: Cond, state: SqlState) -> bool {
    match cond {
        Cond::Others => state != SqlState::QUERY_CANCELED && state != SqlState::ASSERT_FAILURE,
        Cond::State(s) if s.as_str().ends_with("000") => s.class() == state.class(),
        Cond::State(s) => s == state,
    }
}

/// The name of a `RAISE` option in an error.
fn option_name(option: RaiseOption) -> &'static str {
    match option {
        RaiseOption::Errcode => "ERRCODE",
        RaiseOption::Message => "MESSAGE",
        RaiseOption::Detail => "DETAIL",
        RaiseOption::Hint => "HINT",
        RaiseOption::Column => "COLUMN",
        RaiseOption::Constraint => "CONSTRAINT",
        RaiseOption::Datatype => "DATATYPE",
        RaiseOption::Table => "TABLE",
        RaiseOption::Schema => "SCHEMA",
    }
}

/// The code of `ERRCODE`: five characters, or a condition name.
fn errcode(value: &str) -> Result<SqlState> {
    if value.len() == 5
        && let Some(state) = SqlState::parse(value)
    {
        return Ok(state);
    }
    SqlState::for_condition(value).next().ok_or_else(|| {
        Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("unrecognized exception condition \"{value}\""),
        )
    })
}

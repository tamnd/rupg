//! The extended query protocol: `Parse`, `Bind`, `Describe`, `Execute`, `Close` and `Sync`, as `exec_parse_message`, `exec_bind_message`, `exec_describe_statement_message`, `exec_describe_portal_message`, `exec_execute_message` and the main loop of `postgres.c` run them.
//!
//! A prepared statement keeps its parse tree, the types of its parameters, the names of its columns and the number of its generic and custom plans. A portal keeps its statement, the result format of each column and the rows of a `SHOW` that the client did not fetch yet. The portals end with the transaction, as in PostgreSQL. After an error the session drops the messages until `Sync`.
//!
//! An `Execute` that completes does not end the transaction. The statements after it, up to `Sync`, run in one implicit block, and `Sync` commits it. A transaction statement, and a statement that must commit at once such as `DISCARD ALL`, end the transaction at the end of their `Execute`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_func::BackendState;
use rupg_sql::nodes::Node;
use rupg_types::{Array, Value, oid};
use rupg_wire::{Bind, CommandTag, Field, Oids, OutBuf, ProtocolError, Target};

use super::{Connection, Done, character_position, prepare};
use crate::guc::{self, Setting};
use crate::param;
use crate::query::{self, Column, Row};
use crate::utility::{self, Context, Notice};

/// A prepared statement.
#[derive(Debug)]
pub(crate) struct Prepared {
    /// The text of the query, for the error of a statement that the session cannot run yet. For `PREPARE` it is the whole query string.
    pub(super) text: String,
    /// The statement, or `None` for an empty query.
    pub(super) stmt: Option<Node>,
    /// The type OIDs of the parameters.
    pub(super) params: Vec<u32>,
    /// The columns, or `None` when the statement gives no rows.
    pub(super) columns: Option<Vec<Column>>,
    /// The plan of a statement that the engine runs, or `None` for a statement that the session runs.
    pub(super) plan: Option<Plan>,
    /// The start of the statement that prepared it.
    prepare_time: i64,
    /// True for a statement of `PREPARE`, and false for a statement of `Parse`.
    from_sql: bool,
    /// The number of runs with a generic plan.
    generic_plans: AtomicI64,
    /// The number of runs with a custom plan.
    custom_plans: AtomicI64,
}

impl Prepared {
    pub(super) fn new(
        text: String,
        stmt: Option<Node>,
        params: Vec<u32>,
        columns: Option<Vec<Column>>,
        plan: Option<Plan>,
        prepare_time: i64,
        from_sql: bool,
    ) -> Prepared {
        Prepared {
            text,
            stmt,
            params,
            columns,
            plan,
            prepare_time,
            from_sql,
            generic_plans: AtomicI64::new(0),
            custom_plans: AtomicI64::new(0),
        }
    }

    /// The row of the statement in `pg_prepared_statement`, with the name that the registry keeps.
    pub(crate) fn row(&self, name: &[u8]) -> Vec<Value> {
        let types = |types: &mut dyn Iterator<Item = u32>| {
            let elements = types.map(|ty| Some(Value::Oid(ty))).collect();
            Value::Array(Box::new(Array::one(elements)))
        };
        let result_types = match &self.columns {
            Some(columns) => types(&mut columns.iter().map(|column| column.ty)),
            None => Value::Null,
        };
        vec![
            Value::text(String::from_utf8_lossy(name)),
            Value::text(self.text.as_str()),
            Value::TimestampTz(self.prepare_time),
            types(&mut self.params.iter().copied()),
            result_types,
            Value::Bool(self.from_sql),
            Value::Int8(self.generic_plans.load(Ordering::Relaxed)),
            Value::Int8(self.custom_plans.load(Ordering::Relaxed)),
        ]
    }

    /// `choose_custom_plan` in `plancache.c`: counts a run of the statement with a generic plan or with a custom plan, as `GetCachedPlan` does.
    ///
    /// A statement without parameters, a utility statement and an empty query use the generic plan. Else `plan_cache_mode` decides, and in the mode `auto` the first five runs use a custom plan. After them PostgreSQL compares the cost of the generic plan with the mean cost of the custom plans. rupg has no costs, so it uses the generic plan, as PostgreSQL does when the generic plan costs less.
    pub(super) fn count_plan(&self, mode: Option<&str>) {
        let custom = self.plan.is_some()
            && !self.params.is_empty()
            && match mode {
                Some("force_generic_plan") => false,
                Some("force_custom_plan") => true,
                _ => self.custom_plans.load(Ordering::Relaxed) < 5,
            };
        let count = if custom { &self.custom_plans } else { &self.generic_plans };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// What a portal did so far.
#[derive(Debug)]
enum Run {
    /// The statement did not run yet.
    Ready,
    /// The statement ran and gave rows. `at` is the first row that the client did not fetch, and `tag` is the tag of `CommandComplete`.
    Rows { rows: Vec<Row>, at: usize, tag: CommandTag },
    /// The statement ran and gave no rows, so it cannot run again.
    Done,
}

/// A portal.
#[derive(Debug)]
pub(super) struct Portal {
    statement: Arc<Prepared>,
    /// The plan of the query of the statement with the values of the parameters, which `Bind` makes.
    plan: Option<Plan>,
    /// The result format of each column.
    formats: Vec<i16>,
    /// The values of the parameters of a statement that the engine runs.
    params: Vec<Value>,
    run: Run,
    /// The transaction block failed after the portal was made, so the portal cannot run.
    pub(super) failed: bool,
}

/// The error of a message, with the position in the query for an error of `Parse`.
#[derive(Debug)]
pub(super) struct Failed {
    pub(super) error: Error,
    pub(super) position: Option<usize>,
}

impl From<Error> for Failed {
    fn from(error: Error) -> Failed {
        Failed { error, position: None }
    }
}

impl From<ProtocolError> for Failed {
    fn from(error: ProtocolError) -> Failed {
        protocol_error(error).into()
    }
}

/// The error of a registry of names, with its SQLSTATE.
pub(super) fn protocol_error(error: ProtocolError) -> Error {
    let state = SqlState::parse(error.sqlstate).unwrap_or(SqlState::PROTOCOL_VIOLATION);
    let mut out = Error::new(state, error.message);
    if let Some(detail) = error.detail {
        out = out.with_detail(detail);
    }
    if let Some(hint) = error.hint {
        out = out.with_hint(hint);
    }
    out
}

/// The error of a statement in a failed transaction block.
pub(super) fn aborted() -> Error {
    Error::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

/// `RowDescription` for the columns, or `NoData` when there are none.
fn describe_rows(columns: Option<&[Column]>, formats: &[i16], out: &mut OutBuf) {
    let Some(columns) = columns else {
        out.no_data();
        return;
    };
    let fields: Vec<Field<'_>> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| column.field(formats.get(i).copied().unwrap_or(0)))
        .collect();
    out.row_description(&fields);
}

impl Connection {
    /// `exec_parse_message`.
    pub(super) fn parse(
        &mut self,
        name: &[u8],
        sql: &[u8],
        types: Oids<'_>,
        out: &mut OutBuf,
    ) -> Result<(), Failed> {
        let text = String::from_utf8_lossy(sql).into_owned();
        self.report(BackendState::Running, Some(&text));
        self.start_xact();
        self.statements.start_parse(name);
        let (list, parser_notices) = rupg_sql::parse(&text).map_err(|error| {
            let position = error.position(&text);
            Failed { error: error.into(), position }
        })?;
        let locations: Vec<Option<usize>> = parser_notices.iter().map(|n| n.location).collect();
        let mut notices: Vec<Notice> = parser_notices
            .into_iter()
            .map(|n| Notice { severity: n.severity, error: Error::new(n.code, n.message) })
            .collect();
        self.notices(&mut notices, &text, &locations, out);
        let mut found = list.iter().flatten().filter_map(|node| match node {
            Node::RawStmt(raw) => raw.stmt.as_ref(),
            _ => None,
        });
        let stmt = found.next().cloned();
        if found.next().is_some() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "cannot insert multiple commands into a prepared statement",
            )
            .into());
        }
        let mut columns = None;
        let mut plan = None;
        let mut params: Vec<u32> = types.iter().collect();
        if let Some(node) = &stmt {
            if self.transaction.failed() && !utility::exits_transaction(node) {
                return Err(aborted().into());
            }
            if query::is_query(node) {
                self.settings.borrow_mut().take_snapshot();
                let given = Params { types: params.clone(), variable: true };
                let made = query::plan(node, &self.reader(), &given).map_err(|error| Failed {
                    position: error.position().map(|at| character_position(&text, at)),
                    error,
                })?;
                let mut notices: Vec<Notice> =
                    made.notices().iter().map(|error| Notice::warning(error.clone())).collect();
                self.notices(&mut notices, &text, &[], out);
                columns = Some(query::columns(&made));
                params = made.params().to_vec();
                plan = Some(made);
            } else if let Node::ExecuteStmt(stmt) = node {
                columns = self.execute_columns(stmt);
            } else if !rupg_analyze::is_definition(node) {
                utility::check(node, &text)?;
                columns = utility::columns(node, &self.settings.borrow())?
                    .map(|names| names.into_iter().map(Column::text).collect());
            }
        }
        if plan.is_none()
            && let Some(at) = params.iter().position(|&t| t == 0 || t == oid::UNKNOWN)
        {
            return Err(Error::new(
                SqlState::INDETERMINATE_DATATYPE,
                format!("could not determine data type of parameter ${}", at + 1),
            )
            .into());
        }
        self.interrupted()?;
        let prepared =
            Prepared::new(text, stmt, params, columns, plan, self.statement_start, false);
        self.statements.insert(name, Arc::new(prepared))?;
        out.parse_complete();
        Ok(())
    }

    /// `exec_bind_message`.
    pub(super) fn bind(&mut self, bind: Bind<'_>, out: &mut OutBuf) -> Result<(), Failed> {
        let statement = Arc::clone(self.statements.get(bind.statement)?);
        self.report(BackendState::Running, Some(&statement.text));
        self.start_xact();
        let mut values = bind.params(statement.params.len())?;
        // Only a statement that ends the transaction, with no parameters, can run in a failed block.
        let exits = statement.stmt.as_ref().is_some_and(utility::exits_transaction);
        if self.transaction.failed() && !(exits && statement.params.is_empty()) {
            return Err(aborted().into());
        }
        if statement.plan.is_some() || !statement.params.is_empty() {
            self.settings.borrow_mut().take_snapshot();
        }
        self.portals.make_room(bind.portal)?;
        let formats = values.formats;
        let mut params = Vec::new();
        for (i, &type_oid) in statement.params.iter().enumerate() {
            let value = values.next().transpose()?.flatten();
            let format = formats.of(i);
            let result = if statement.plan.is_some() {
                query::param(type_oid, format, value, i + 1, &self.reader()).map(|v| params.push(v))
            } else {
                param::check(type_oid, format, value, i + 1)
            };
            result.map_err(|error| {
                let text = value.filter(|_| format == 0).and_then(|v| std::str::from_utf8(v).ok());
                error.with_context(self.param_context(bind.portal, i + 1, text))
            })?;
        }
        let result_formats = values.finish()?;
        // `GetCachedPlan` plans the query with the values of the parameters, as a custom plan, before the portal takes the formats of the result.
        let plan = match &statement.plan {
            Some(plan) => Some(plan.fold(Some(&params), &self.reader())?),
            None => None,
        };
        statement.count_plan(self.settings.borrow().get("plan_cache_mode").as_deref());
        let formats = match &statement.columns {
            Some(columns) => {
                let count = result_formats.len();
                if count > 1 && count != columns.len() {
                    return Err(Error::new(
                        SqlState::PROTOCOL_VIOLATION,
                        format!(
                            "bind message has {count} result formats but query has {} columns",
                            columns.len()
                        ),
                    )
                    .into());
                }
                (0..columns.len()).map(|i| result_formats.of(i)).collect()
            }
            None => Vec::new(),
        };
        let portal = Portal { statement, plan, formats, params, run: Run::Ready, failed: false };
        self.portals.insert(bind.portal, portal);
        out.bind_complete();
        Ok(())
    }

    /// `bind_param_error_callback`: the context line of an error in the value of parameter `number`. `text` is the value in the text format, which the line shows in quotes with at most `log_parameter_max_length_on_error` bytes.
    fn param_context(&self, portal: &[u8], number: usize, text: Option<&str>) -> String {
        let max = match guc::find("log_parameter_max_length_on_error")
            .map(|parameter| self.settings.borrow().setting(parameter))
        {
            Some(Setting::Int(max)) => max,
            _ => 0,
        };
        let value = text.map(|text| {
            let (shown, ellipsis) = match usize::try_from(max) {
                Ok(max) if max < text.len() => {
                    let end = (0..=max).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0);
                    (&text[..end], "...")
                }
                _ => (text, ""),
            };
            format!(" = '{}{ellipsis}'", shown.replace('\'', "''"))
        });
        let value = value.unwrap_or_default();
        if portal.is_empty() {
            format!("unnamed portal parameter ${number}{value}")
        } else {
            format!("portal \"{}\" parameter ${number}{value}", String::from_utf8_lossy(portal))
        }
    }

    /// `exec_describe_statement_message` and `exec_describe_portal_message`.
    pub(super) fn describe(
        &mut self,
        target: Target,
        name: &[u8],
        out: &mut OutBuf,
    ) -> Result<(), Failed> {
        self.start_xact();
        let failed = self.transaction.failed();
        match target {
            Target::Statement => {
                let statement = self.statements.get(name)?;
                if failed && statement.columns.is_some() {
                    return Err(aborted().into());
                }
                out.parameter_description(&statement.params);
                describe_rows(statement.columns.as_deref(), &[], out);
            }
            Target::Portal => {
                let portal = self.portals.get(name)?;
                if failed && portal.statement.columns.is_some() {
                    return Err(aborted().into());
                }
                describe_rows(portal.statement.columns.as_deref(), &portal.formats, out);
            }
        }
        Ok(())
    }

    /// `exec_execute_message`. `max_rows` of zero or less fetches all the rows.
    pub(super) fn execute(
        &mut self,
        name: &[u8],
        max_rows: i32,
        out: &mut OutBuf,
    ) -> Result<(), Failed> {
        let statement = Arc::clone(&self.portals.get(name)?.statement);
        let Some(node) = &statement.stmt else {
            out.empty_query_response();
            return Ok(());
        };
        self.report(BackendState::Running, Some(&statement.text));
        let transaction_statement = matches!(node, Node::TransactionStmt(_));
        self.start_xact();
        if self.transaction.failed() && !utility::exits_transaction(node) {
            return Err(aborted().into());
        }
        self.interrupted()?;
        let portal = self.portals.get_mut(name)?;
        if portal.failed || matches!(portal.run, Run::Done) {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("portal \"{}\" cannot be run", String::from_utf8_lossy(name)),
            )
            .into());
        }
        let ready = matches!(portal.run, Run::Ready);
        let params = std::mem::take(&mut portal.params);
        let plan = portal.plan.take();
        let formats = portal.formats.clone();
        let mut immediate_commit = false;
        let mut tag = None;
        if ready {
            let done = if let Some(plan) = &plan {
                self.settings.borrow_mut().take_snapshot();
                let rows = query::run(plan, &params, &self.reader(), &formats)?;
                Done::Rows { columns: Vec::new(), rows, tag: CommandTag::Select }
            } else if prepare::is_prepare(node) {
                let mut notices = Vec::new();
                let result = self.run_prepare(node, &statement.text, &formats, &mut notices, 0);
                self.notices(&mut notices, &statement.text, &[], out);
                result.map_err(|error| Failed {
                    position: error.position().map(|at| character_position(&statement.text, at)),
                    error,
                })?
            } else if rupg_analyze::is_definition(node) {
                let mut notices = Vec::new();
                let result = self.define(node, &statement.text, &mut notices);
                self.notices(&mut notices, &statement.text, &[], out);
                result.map_err(|error| Failed {
                    position: error.position().map(|at| character_position(&statement.text, at)),
                    error,
                })?
            } else {
                let mut notices = Vec::new();
                let mut cx = Context {
                    settings: self.settings.get_mut(),
                    transaction: &mut self.transaction,
                    user: &self.user,
                    notices: &mut notices,
                    immediate_commit: false,
                };
                let result = utility::run(node, &statement.text, &mut cx);
                immediate_commit = cx.immediate_commit;
                self.notices(&mut notices, &statement.text, &[], out);
                result?.into()
            };
            match done {
                Done::Tag(done) => {
                    if done == CommandTag::DiscardAll {
                        self.statements.deallocate_all();
                        self.portals.clear();
                    } else if let Some(portal) = self.portals.find_mut(name) {
                        portal.run = Run::Done;
                    }
                    tag = Some((done, 0));
                }
                Done::Rows { rows, tag, .. } => {
                    if let Some(portal) = self.portals.find_mut(name) {
                        portal.run = Run::Rows { rows, at: 0, tag };
                    }
                }
            }
        }
        let (tag, count) = match tag {
            Some(tag) => tag,
            None => match self.fetch(name, max_rows, out)? {
                (tag, count, true) => (tag, count),
                (_, _, false) => {
                    out.portal_suspended();
                    self.pipelining = true;
                    return Ok(());
                }
            },
        };
        if transaction_statement || immediate_commit {
            self.finish_xact();
        } else {
            self.pipelining = true;
        }
        out.command_tag(tag, count);
        Ok(())
    }

    /// `PortalRunSelect` on the rows of a portal: sends at most `max_rows` of them, or all of them when `max_rows` is zero or less. It gives the tag of the portal, the number of rows that it sent, and true when the portal is at its end, which is when it sent fewer rows than `max_rows`.
    fn fetch(
        &mut self,
        name: &[u8],
        max_rows: i32,
        out: &mut OutBuf,
    ) -> Result<(CommandTag, u64, bool), Failed> {
        let portal = self.portals.get_mut(name)?;
        let Run::Rows { rows, at, tag } = &mut portal.run else {
            return Err(Error::internal("a portal with no rows to fetch").into());
        };
        let left = rows.len() - *at;
        let max = usize::try_from(max_rows).ok().filter(|&max| max > 0);
        let count = max.map_or(left, |max| left.min(max));
        // printtup checks the formats when it sends the first row.
        if count > 0
            && let Some(bad) = portal.formats.iter().find(|&&f| f != 0 && f != 1)
        {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unsupported format code: {bad}"),
            )
            .into());
        }
        // The binary form of `text` is its bytes, as in the text form, so the rows of a `SHOW` serve both formats.
        for row in &rows[*at..*at + count] {
            let values: Vec<Option<&[u8]>> = row.iter().map(Option::as_deref).collect();
            out.data_row(&values);
        }
        *at += count;
        Ok((*tag, count as u64, max.is_none_or(|max| count < max)))
    }

    /// The `Close` message. A name that is not there is not an error.
    pub(super) fn close(&mut self, target: Target, name: &[u8], out: &mut OutBuf) {
        match target {
            Target::Statement => drop(self.statements.close(name)),
            Target::Portal => drop(self.portals.close(name)),
        }
        out.close_complete();
    }

    /// The `Sync` message: the end of the implicit block of the messages before it, and the end of the transaction if no block is open.
    pub(super) fn sync(&mut self) {
        self.transaction.end_implicit();
        self.finish_xact();
    }
}

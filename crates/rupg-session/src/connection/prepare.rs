//! The SQL statements of the prepared statements: `PREPARE`, `EXECUTE` and `DEALLOCATE`, as `PrepareQuery`, `ExecuteQuery` and `DeallocateQuery` of `prepare.c` run them.
//!
//! They share the statements with `Parse`, `Bind` and `Close` of the extended protocol, so `EXECUTE` runs a statement of `Parse` and `Bind` binds a statement of `PREPARE`. A statement of `PREPARE` keeps the whole query string as its text, and `pg_prepared_statements` shows it with `from_sql` true.

use std::sync::Arc;

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{DeallocateStmt, ExecuteStmt, Node, PrepareStmt};
use rupg_wire::CommandTag;

use super::extended::{Prepared, protocol_error};
use super::{Connection, Done};
use crate::query::{self, Column};
use crate::utility::{self, Context, Notice};

/// The depth of `EXECUTE` in `EXECUTE` that the session takes before it stops with the error of `check_stack_depth`. A statement of `Parse` can be `EXECUTE` of itself.
const MAX_DEPTH: usize = 100;

/// True for `PREPARE`, `EXECUTE` and `DEALLOCATE`.
pub(super) fn is_prepare(node: &Node) -> bool {
    matches!(node, Node::PrepareStmt(_) | Node::ExecuteStmt(_) | Node::DeallocateStmt(_))
}

impl Connection {
    /// Runs `PREPARE`, `EXECUTE` or `DEALLOCATE`. `text` is the whole query string, and `formats` are the result formats of the columns of `EXECUTE`. `depth` is the number of `EXECUTE` statements that run this one.
    pub(super) fn run_prepare(
        &mut self,
        node: &Node,
        text: &str,
        formats: &[i16],
        notices: &mut Vec<Notice>,
        depth: usize,
    ) -> Result<Done, Error> {
        match node {
            Node::PrepareStmt(stmt) => self.prepare(stmt, text, notices),
            Node::ExecuteStmt(stmt) => self.execute_prepared(stmt, formats, notices, depth),
            Node::DeallocateStmt(stmt) => self.deallocate(stmt),
            _ => Err(Error::new(SqlState::INTERNAL_ERROR, "not a statement of PREPARE")),
        }
    }

    /// `UtilityTupleDescriptor` for `EXECUTE`: the columns of the statement that it runs, or `None` when there is no such statement or the statement gives no rows.
    pub(super) fn execute_columns(&self, stmt: &ExecuteStmt) -> Option<Vec<Column>> {
        let name = stmt.name.as_deref().unwrap_or("");
        self.statements.get(name.as_bytes()).ok().and_then(|statement| statement.columns.clone())
    }

    /// `PrepareQuery`.
    fn prepare(
        &mut self,
        stmt: &PrepareStmt,
        text: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let name = stmt.name.as_deref().unwrap_or("");
        let Some(node) = &stmt.query else {
            return Err(Error::new(SqlState::INTERNAL_ERROR, "PREPARE has no query"));
        };
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let types = rupg_analyze::param_types(&stmt.argtypes, &reader)?;
        let plan = query::plan(node, &reader, &Params { types, variable: true })?;
        drop(reader);
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        let params = plan.params().to_vec();
        let columns = Some(query::columns(&plan));
        let prepared = Prepared::new(
            text.to_owned(),
            Some(node.clone()),
            params,
            columns,
            Some(plan),
            self.statement_start,
            true,
        );
        self.statements.insert(name.as_bytes(), Arc::new(prepared)).map_err(protocol_error)?;
        Ok(Done::Tag(CommandTag::Prepare))
    }

    /// `ExecuteQuery`: computes the values of the parameters, as `EvaluateParams` does, and runs the statement with them.
    fn execute_prepared(
        &mut self,
        stmt: &ExecuteStmt,
        formats: &[i16],
        notices: &mut Vec<Notice>,
        depth: usize,
    ) -> Result<Done, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
                .with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."));
        }
        let name = stmt.name.as_deref().unwrap_or("");
        let statement = Arc::clone(self.statements.get(name.as_bytes()).map_err(protocol_error)?);
        let expected = statement.params.len();
        if stmt.params.len() != expected {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("wrong number of parameters for prepared statement \"{name}\""),
            )
            .with_detail(format!(
                "Expected {expected} parameters but got {}.",
                stmt.params.len()
            )));
        }
        if statement.plan.is_some() || expected > 0 {
            self.settings.borrow_mut().take_snapshot();
        }
        let params = if expected == 0 {
            Vec::new()
        } else {
            let reader = self.reader();
            let values = rupg_analyze::execute_params(&stmt.params, &statement.params, &reader)?;
            notices.extend(values.notices.iter().map(|error| Notice::warning(error.clone())));
            let plan = rupg_exec::prepare(values)?.fold(None, &reader)?;
            plan.run(&[], &reader)?.into_iter().next().unwrap_or_default()
        };
        let reader = self.reader();
        let plan = match &statement.plan {
            Some(plan) => Some(plan.fold(Some(&params), &reader)?),
            None => None,
        };
        statement.count_plan(reader.settings().get("plan_cache_mode").as_deref());
        if let Some(plan) = plan {
            let rows = query::run(&plan, &params, &reader, formats)?;
            let columns = statement.columns.clone().unwrap_or_default();
            return Ok(Done::Rows { columns, rows, tag: CommandTag::Select });
        }
        drop(reader);
        let Some(node) = &statement.stmt else { return Ok(Done::Tag(CommandTag::Execute)) };
        let text = &statement.text;
        if is_prepare(node) {
            self.run_prepare(node, text, formats, notices, depth + 1)
        } else if rupg_analyze::is_definition(node) {
            self.define(node, text, notices)
        } else {
            let mut cx = Context {
                settings: self.settings.get_mut(),
                transaction: &mut self.transaction,
                user: &self.user,
                notices,
                immediate_commit: false,
            };
            utility::run(node, text, &mut cx).map(Done::from)
        }
    }

    /// `DeallocateQuery`.
    fn deallocate(&mut self, stmt: &DeallocateStmt) -> Result<Done, Error> {
        if stmt.isall {
            self.statements.deallocate_all();
            return Ok(Done::Tag(CommandTag::DeallocateAll));
        }
        let name = stmt.name.as_deref().unwrap_or("");
        self.statements.deallocate(name.as_bytes()).map_err(protocol_error)?;
        Ok(Done::Tag(CommandTag::Deallocate))
    }
}

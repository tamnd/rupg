//! The functions in PL/pgSQL and `DO`, as SPI runs their queries: each query runs as a command of the statement that called the function, and a block with `EXCEPTION` runs in a subtransaction. A procedure that `CALL` runs outside of a transaction block can end the transaction with `COMMIT` and `ROLLBACK`.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::Env;
use rupg_catalog::Catalog;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_func::Session;
use rupg_plpgsql::{Cache, Fetched, Spi};
use rupg_sql::nodes::{DoStmt, FetchDirection, Node};
use rupg_types::Value;

use super::{Applied, Connection, Done, savepoint};
use crate::query::{Reader, Transition};
use crate::utility::{self, Context, Notice};

/// The state at the start of a subtransaction: the state that a savepoint keeps, and the names of the open portals.
type Mark = (savepoint::Mark, Vec<Vec<u8>>);

/// The state of the calls in PL/pgSQL that run, for `COMMIT` and `ROLLBACK` in a call.
#[derive(Debug, Default)]
pub(super) struct Calls {
    /// True while a `CALL` runs where the procedure can end the transaction, as when the flag `atomic` of `CallContext` is false.
    nonatomic: Cell<bool>,
    /// The number of the subtransactions of the blocks with `EXCEPTION` that run.
    subs: Cell<usize>,
    /// The loops that run, with the name of the portal of a loop over a cursor and true for a query that does not write.
    pinned: RefCell<Vec<(Option<String>, bool)>>,
    /// The number of the changes of the catalog and of the ends of the transactions in calls, so that a call takes a new reader and makes its plans again after a change.
    changes: Cell<u64>,
}

/// A reader with the number of the changes in calls and the command that it reads.
type Cached<'a> = ((u64, u32), Rc<Reader<'a>>);

/// A reader with the catalog after a change of the catalog in a call, which the analysis of the next queries of the call reads, and the reader after the next change.
struct Layer<'a> {
    reader: Reader<'a>,
    /// The value of [`Calls::changes`] when the reader was last checked.
    seen: Cell<u64>,
    next: OnceCell<Box<Layer<'a>>>,
}

/// The SPI of one call: the reader of the statement that called the function, and the reader of the current command of the transaction.
struct PlSpi<'a> {
    conn: &'a Connection,
    /// The reader of the statement that called the function, which gets the warnings of the function.
    outer: &'a Reader<'a>,
    /// `SPI_OPT_NONATOMIC` is not set: the call cannot end the transaction.
    atomic: bool,
    /// The reader of the last query, with the number of the ends of the transactions and the command. A write ends the command, so that the next query sees the rows.
    reader: RefCell<Option<Cached<'a>>>,
    /// The command, the deferred triggers and the names of the open portals at the start of each subtransaction.
    marks: RefCell<Vec<Mark>>,
    /// The number of the loops of the call in [`Calls::pinned`].
    pins: Cell<usize>,
    /// The value of [`Calls::changes`] when `outer` was last checked.
    seen: Cell<u64>,
    /// The readers after the changes of the catalog in the call, the first one first.
    layers: OnceCell<Box<Layer<'a>>>,
}

impl<'a> PlSpi<'a> {
    fn new(conn: &'a Connection, outer: &'a Reader<'a>, atomic: bool) -> PlSpi<'a> {
        PlSpi {
            conn,
            outer,
            atomic,
            reader: RefCell::new(None),
            marks: RefCell::new(Vec::new()),
            pins: Cell::new(0),
            seen: Cell::new(conn.calls.changes.get()),
            layers: OnceCell::new(),
        }
    }

    /// The reader that the analysis of a query reads: `outer`, or a reader with the catalog of the transaction after a change of the catalog in a call.
    fn current(&self) -> &Reader<'a> {
        let changes = self.conn.calls.changes.get();
        let (mut reader, mut seen, mut next) = (self.outer, &self.seen, &self.layers);
        while let Some(layer) = next.get() {
            (reader, seen, next) = (&layer.reader, &layer.seen, &layer.next);
        }
        if seen.get() == changes {
            return reader;
        }
        seen.set(changes);
        let catalog = self.conn.catalog();
        if Arc::ptr_eq(&catalog, &reader.catalog) {
            return reader;
        }
        let reader = self.conn.reader_on(catalog);
        &next
            .get_or_init(|| {
                Box::new(Layer { reader, seen: Cell::new(changes), next: OnceCell::new() })
            })
            .reader
    }

    /// The reader of the current command.
    fn reader(&self) -> Rc<Reader<'a>> {
        let command = (self.conn.calls.changes.get(), self.conn.command.get());
        if let Some((at, reader)) = &*self.reader.borrow()
            && *at == command
        {
            return Rc::clone(reader);
        }
        let reader = Rc::new(self.conn.reader());
        *self.reader.borrow_mut() = Some((command, Rc::clone(&reader)));
        reader
    }

    /// The subtransactions after the mark `sub` end.
    fn end_subs(&self, sub: usize) {
        let n = self.marks.borrow().len().saturating_sub(sub);
        self.conn.calls.subs.set(self.conn.calls.subs.get().saturating_sub(n));
    }

    /// `SPI_execute_plan`: the run of [`Spi::execute`].
    fn run(&self, plan: &Plan, params: &[Value]) -> rupg_common::Result<(Vec<Vec<Value>>, u64)> {
        let reader = self.reader();
        plan.start(&*reader)?;
        if plan.modifying_cte() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a statement of WITH that changes data in PL/pgSQL is not supported yet",
            ));
        }
        let mark = self.conn.event_mark();
        let rows = plan.run(params, &*reader);
        self.forward(&reader);
        let rows = rows?;
        let Some(info) = plan.write() else {
            let n = rows.len() as u64;
            return Ok((rows, n));
        };
        let view = reader.view().cloned().unwrap_or_default();
        drop(reader);
        let applied = self.conn.apply_write(plan, params, &view, rows);
        self.forward_triggers();
        let Applied { count, written, view, batches, .. } = applied?;
        let rows = if info.returning.is_some() {
            // A subquery of `RETURNING` reads with the snapshot of the statement.
            let reader = self.conn.reader().with_view(view);
            let rows = plan.returning(written, params, &reader);
            self.forward(&reader);
            rows?
        } else {
            Vec::new()
        };
        let fired = if batches.is_empty() { Ok(()) } else { self.conn.fire(batches) };
        let fired = fired.and_then(|()| self.conn.fire_events(mark));
        self.forward_triggers();
        fired?;
        Ok((rows, count))
    }

    /// The warnings of the functions of a query go to the statement that called the function.
    fn forward(&self, reader: &Reader<'_>) {
        self.outer.add_warnings(reader.take_warnings());
    }

    /// The messages of the triggers of a statement of the function go to the reader of the statement that called the function.
    fn forward_triggers(&self) {
        self.outer.add_warnings(self.conn.trigger_warnings.take());
    }
}

impl Spi for PlSpi<'_> {
    fn env(&self) -> &dyn Env {
        self.current()
    }

    fn session(&self) -> &dyn Session {
        self.current()
    }

    fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&self.current().catalog)
    }

    fn execute(
        &self,
        plan: &Plan,
        params: &[Value],
    ) -> rupg_common::Result<(Vec<Vec<Value>>, u64)> {
        // A `CALL` of the call can end the transaction when the call can.
        let nonatomic = self.conn.calls.nonatomic.replace(!self.atomic);
        let result = self.run(plan, params);
        self.conn.calls.nonatomic.set(nonatomic);
        result
    }

    fn cursor_exists(&self, name: &str) -> bool {
        self.conn.portals.borrow().find(name.as_bytes()).is_some()
    }

    fn open_cursor(
        &self,
        name: Option<&str>,
        plan: &Plan,
        params: &[Value],
        text: &str,
        options: i32,
    ) -> rupg_common::Result<String> {
        let reader = self.reader();
        plan.start(&*reader)?;
        let run = plan.run(params, &*reader);
        self.forward(&reader);
        drop(reader);
        let name = name.map_or_else(|| self.new_portal_name(), str::to_owned);
        self.conn.open_portal(&name, plan, run, text, options);
        Ok(name)
    }

    fn fetch_cursor(
        &self,
        name: &str,
        direction: FetchDirection,
        count: i64,
        rows: bool,
    ) -> rupg_common::Result<Fetched> {
        self.conn.spi_fetch(name, direction, count, rows)
    }

    fn close_cursor(&self, name: &str) -> rupg_common::Result<()> {
        match self.conn.portals.borrow_mut().close(name.as_bytes()) {
            Some(_) => Ok(()),
            None => Err(super::cursor::no_cursor(name)),
        }
    }

    fn new_portal_name(&self) -> String {
        // The first name `<unnamed portal n>` that is free.
        loop {
            let n = self.conn.unnamed_portals.get() + 1;
            self.conn.unnamed_portals.set(n);
            let name = format!("<unnamed portal {n}>");
            if !self.cursor_exists(&name) {
                return name;
            }
        }
    }

    fn begin_sub(&self) -> usize {
        self.conn.calls.subs.set(self.conn.calls.subs.get() + 1);
        let mark = self.conn.mark();
        let portals = self.conn.portals.borrow().iter().map(|(name, _)| name.to_vec()).collect();
        let mut marks = self.marks.borrow_mut();
        marks.push((mark, portals));
        marks.len() - 1
    }

    fn release_sub(&self, sub: usize) {
        self.end_subs(sub);
        let mut marks = self.marks.borrow_mut();
        if let Some((mark, _)) = marks.get(sub) {
            self.conn.release(mark);
        }
        marks.truncate(sub);
    }

    fn rollback_sub(&self, sub: usize) {
        self.end_subs(sub);
        let mark = self.marks.borrow_mut().drain(sub..).next();
        let Some((mark, portals)) = mark else { return };
        let catalog = self.conn.catalog();
        self.conn.restore(&mark);
        if !Arc::ptr_eq(&catalog, &self.conn.catalog()) {
            self.conn.calls.changes.set(self.conn.calls.changes.get() + 1);
        }
        // `AtSubAbort_Portals`: the portals that the subtransaction opened close.
        self.conn.portals.borrow_mut().retain(|name, _| portals.iter().any(|p| p == name));
    }

    fn end_transaction(&self, commit: bool, chain: bool) -> rupg_common::Result<()> {
        let termination = |message: &str| {
            Error::new(SqlState::INVALID_TRANSACTION_TERMINATION, message.to_owned())
        };
        if self.atomic {
            return Err(termination("invalid transaction termination"));
        }
        if self.conn.calls.subs.get() > 0 {
            return Err(termination(if commit {
                "cannot commit while a subtransaction is active"
            } else {
                "cannot roll back while a subtransaction is active"
            }));
        }
        self.conn.end_in_call(commit, chain)
    }

    fn utility(
        &self,
        stmt: &Node,
        text: &str,
        notices: &mut Vec<Error>,
    ) -> rupg_common::Result<u64> {
        let result = self.conn.call_utility(stmt, text, notices);
        self.conn.calls.changes.set(self.conn.calls.changes.get() + 1);
        result
    }

    fn changes(&self) -> u64 {
        self.conn.calls.changes.get()
    }

    fn pin(&self, name: Option<&str>, read_only: bool) {
        self.conn.calls.pinned.borrow_mut().push((name.map(str::to_owned), read_only));
        self.pins.set(self.pins.get() + 1);
    }

    fn unpin(&self) {
        self.conn.calls.pinned.borrow_mut().pop();
        self.pins.set(self.pins.get().saturating_sub(1));
    }

    fn notice(&self, notice: Error) {
        self.outer.warn(notice);
    }

    fn cache(&self) -> &Cache {
        &self.conn.plpgsql
    }
}

impl Drop for PlSpi<'_> {
    /// The subtransactions and the loops of a call that failed end with the call.
    fn drop(&mut self) {
        let mut next = &self.layers;
        while let Some(layer) = next.get() {
            self.outer.add_warnings(layer.reader.take_warnings());
            next = &layer.next;
        }
        self.end_subs(0);
        let mut pinned = self.conn.calls.pinned.borrow_mut();
        let left = pinned.len().saturating_sub(self.pins.get());
        pinned.truncate(left);
    }
}

impl Connection {
    /// `plpgsql_call_handler`: a call of a function in PL/pgSQL from a query that `outer` reads.
    ///
    /// # Errors
    ///
    /// The errors of the function.
    pub(crate) fn pl_call(
        &self,
        outer: &Reader<'_>,
        func: u32,
        types: &[u32],
        ret: u32,
        args: &[Value],
    ) -> rupg_common::Result<Vec<Value>> {
        // `ExecuteCallStmt`: only a procedure that `CALL` runs can end the transaction.
        let procedure = outer.catalog.function(func).is_some_and(|f| f.kind == b'p');
        let atomic = !(procedure && self.calls.nonatomic.get());
        // The queries of the function do not see the transition tables of a trigger that called it.
        let transition = self.transition.take();
        let result = rupg_plpgsql::call(&PlSpi::new(self, outer, atomic), func, types, ret, args);
        self.transition.replace(transition);
        result
    }

    /// `ExecuteCallStmt` at the top level: `CALL` runs `run`, and outside of a transaction block the procedure can end the transaction.
    pub(super) fn top_call<T>(&self, run: impl FnOnce() -> T) -> T {
        self.calls.nonatomic.set(!self.transaction.in_block());
        let result = run();
        self.calls.nonatomic.set(false);
        result
    }

    /// `_SPI_commit` and `_SPI_rollback`: the transaction of a call ends, and a new transaction starts. A commit that fails rolls back. The portals of the loops that run stay, as `HoldPinnedPortals` keeps them.
    ///
    /// # Errors
    ///
    /// `55000` in a loop over a query that writes, and the errors of the deferred triggers at the commit.
    fn end_in_call(&self, commit: bool, chain: bool) -> rupg_common::Result<()> {
        let kept = chain.then(|| self.settings.borrow().characteristics());
        let result = if self.calls.pinned.borrow().iter().any(|(_, read_only)| !read_only) {
            Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot perform transaction commands inside a cursor loop that is not read-only",
            ))
        } else if commit {
            self.pre_commit()
        } else {
            Ok(())
        };
        let commit = commit && result.is_ok();
        self.settings.borrow_mut().end(commit);
        let lease = self.lease.borrow_mut().take();
        let defined = lease.is_some();
        if let Some(lease) = lease
            && commit
        {
            lease.commit();
        }
        self.end_heap(commit, defined);
        self.end_portals(commit);
        self.transaction_ended();
        self.settings.borrow_mut().start_transaction(kept);
        self.transaction_started();
        self.calls.changes.set(self.calls.changes.get() + 1);
        result
    }

    /// `ProcessUtility` of a statement of a call, which is not at the top level. The notices of the statement go to `out`.
    ///
    /// # Errors
    ///
    /// The errors of the statement, and `0A000` for a statement that a call cannot run yet.
    fn call_utility(
        &self,
        node: &Node,
        text: &str,
        out: &mut Vec<Error>,
    ) -> rupg_common::Result<u64> {
        let mut notices = Vec::new();
        let result = if rupg_analyze::is_definition(node) {
            self.define(node, text, &mut notices)
                .map(|done| if let Done::Count(_, count) = done { count } else { 0 })
        } else if let Node::VariableSetStmt(_) = node {
            self.ts_config(node).and_then(|ts_config| {
                // The statement cannot end the transaction, so its state does not change.
                let mut transaction = self.transaction.clone();
                let mut settings = self.settings.borrow_mut();
                let mut cx = Context {
                    settings: &mut settings,
                    transaction: &mut transaction,
                    user: &self.user,
                    notices: &mut notices,
                    immediate_commit: false,
                    ts_config,
                    top_level: false,
                };
                utility::run(node, text, &mut cx).map(|_| 0)
            })
        } else if let Node::VacuumStmt(stmt) = node
            && stmt.is_vacuumcmd
        {
            Err(self.prevent_in_call("VACUUM"))
        } else if let Node::CreatedbStmt(_) = node {
            Err(self.prevent_in_call("CREATE DATABASE"))
        } else if let Node::DropdbStmt(_) = node {
            Err(self.prevent_in_call("DROP DATABASE"))
        } else {
            Err(utility::not_supported(text))
        };
        out.extend(notices.into_iter().map(Notice::into_message));
        result
    }

    /// `PreventInTransactionBlock` for the statement `statement` in a call, which is not at the top level.
    fn prevent_in_call(&self, statement: &str) -> Error {
        let place = if self.transaction.in_block() {
            "cannot run inside a transaction block"
        } else if self.calls.subs.get() > 0 {
            "cannot run inside a subtransaction"
        } else {
            "cannot be executed from a function or procedure"
        };
        Error::new(SqlState::ACTIVE_SQL_TRANSACTION, format!("{statement} {place}"))
    }

    /// The part of [`Connection::ended`] for the end of a transaction in a call: the portals of the loops that run and the portal of the statement that runs the call stay.
    fn end_portals(&self, commit: bool) {
        let pinned = self.calls.pinned.borrow();
        self.portals.borrow_mut().retain(|name, portal| match &mut portal.run {
            _ if pinned.iter().any(|(p, _)| p.as_deref().is_some_and(|p| p.as_bytes() == name)) => {
                true
            }
            super::extended::Run::Cursor(cursor) if commit && cursor.holdable() => {
                cursor.fresh = false;
                true
            }
            super::extended::Run::Cursor(cursor) => cursor.held(),
            _ => true,
        });
    }

    /// `plpgsql_exec_trigger`: a call of a trigger function in PL/pgSQL. Its messages go to the messages of the triggers of the statement.
    ///
    /// # Errors
    ///
    /// The errors of the function.
    pub(super) fn pl_trigger(
        &self,
        call: &rupg_plpgsql::TriggerCall<'_>,
        transition: Option<Transition>,
    ) -> rupg_common::Result<Option<Vec<Value>>> {
        // `SPI_register_trigger_data`: the queries of the function read the transition tables of the trigger.
        let outer = self.transition.replace(transition);
        let reader = self.reader();
        let result = rupg_plpgsql::trigger(&PlSpi::new(self, &reader, true), call);
        self.trigger_warnings.borrow_mut().extend(reader.take_warnings());
        drop(reader);
        self.transition.replace(outer);
        result
    }

    /// `ExecuteDoStmt`: runs the code of `DO`, which is a part of the query `text`. The messages of `RAISE` go to `warnings`.
    ///
    /// # Errors
    ///
    /// The errors of [`rupg_plpgsql::do_code`] and of the code.
    pub(super) fn run_do(
        &self,
        stmt: &DoStmt,
        text: &str,
        warnings: &mut Vec<Error>,
    ) -> Result<(), Error> {
        let src = rupg_plpgsql::do_code(stmt)?;
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        // `ExecuteDoStmt`: outside of a transaction block the code can end the transaction.
        let atomic = self.transaction.in_block();
        let transition = self.transition.take();
        let result = rupg_plpgsql::inline(&PlSpi::new(self, &reader, atomic), &src);
        self.transition.replace(transition);
        warnings.extend(reader.take_warnings());
        result.map_err(|error| match (error.position(), code_place(stmt, text, &src)) {
            // `function_parse_error_transpose`: an error of the compile has its place in the code, which is a part of the query.
            (Some(at), Some(start)) => error.with_position(Some(start + at)),
            _ => error,
        })
    }
}

/// The place in the query `text` of the code `src` of `DO`, which is in the string after the place of the option `AS`. `None` when the string has the code in another form, such as with doubled quotes.
fn code_place(stmt: &DoStmt, text: &str, src: &str) -> Option<usize> {
    let at = stmt.args.iter().flatten().find_map(|def| match def {
        Node::DefElem(def) if def.defname.as_deref() == Some("as") => {
            usize::try_from(def.location).ok()
        }
        _ => None,
    })?;
    text.get(at..)?.find(src).map(|i| at + i)
}

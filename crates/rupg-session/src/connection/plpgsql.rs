//! The functions in PL/pgSQL and `DO`, as SPI runs their queries: each query runs as a command of the statement that called the function, and a block with `EXCEPTION` runs in a subtransaction.

use std::cell::RefCell;
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

use super::foreign::Batch;
use super::{Applied, Connection};
use crate::query::Reader;

/// The state at the start of a subtransaction: the command, the deferred triggers and the names of the open portals.
type Mark = (u32, Vec<Batch>, Vec<Vec<u8>>);

/// The SPI of one call: the reader of the statement that called the function, and the reader of the current command of the transaction.
struct PlSpi<'a> {
    conn: &'a Connection,
    /// The reader of the statement that called the function, which gets the warnings of the function.
    outer: &'a Reader<'a>,
    /// The reader of the last query and its command. A write ends the command, so that the next query sees the rows.
    reader: RefCell<Option<(u32, Rc<Reader<'a>>)>>,
    /// The command, the deferred triggers and the names of the open portals at the start of each subtransaction.
    marks: RefCell<Vec<Mark>>,
}

impl<'a> PlSpi<'a> {
    fn new(conn: &'a Connection, outer: &'a Reader<'a>) -> PlSpi<'a> {
        PlSpi { conn, outer, reader: RefCell::new(None), marks: RefCell::new(Vec::new()) }
    }

    /// The reader of the current command.
    fn reader(&self) -> Rc<Reader<'a>> {
        let command = self.conn.command.get();
        if let Some((at, reader)) = &*self.reader.borrow()
            && *at == command
        {
            return Rc::clone(reader);
        }
        let reader = Rc::new(self.conn.reader());
        *self.reader.borrow_mut() = Some((command, Rc::clone(&reader)));
        reader
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
        self.outer
    }

    fn session(&self) -> &dyn Session {
        self.outer
    }

    fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&self.outer.catalog)
    }

    fn execute(
        &self,
        plan: &Plan,
        params: &[Value],
    ) -> rupg_common::Result<(Vec<Vec<Value>>, u64)> {
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
        let mut marks = self.marks.borrow_mut();
        let portals = self.conn.portals.borrow().iter().map(|(name, _)| name.to_vec()).collect();
        marks.push((self.conn.command.get(), self.conn.deferred.borrow().clone(), portals));
        marks.len() - 1
    }

    fn release_sub(&self, sub: usize) {
        self.marks.borrow_mut().truncate(sub);
    }

    fn rollback_sub(&self, sub: usize) {
        let mark = self.marks.borrow_mut().drain(sub..).next();
        let Some((command, deferred, portals)) = mark else { return };
        let xid = self.conn.xid.get();
        if xid != 0 {
            self.conn.store.heap().rollback_to(xid, command);
        }
        *self.conn.deferred.borrow_mut() = deferred;
        // `AtSubAbort_Portals`: the portals that the subtransaction opened close.
        self.conn.portals.borrow_mut().retain(|name, _| portals.iter().any(|p| p == name));
    }

    fn notice(&self, notice: Error) {
        self.outer.warn(notice);
    }

    fn cache(&self) -> &Cache {
        &self.conn.plpgsql
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
        rupg_plpgsql::call(&PlSpi::new(self, outer), func, types, ret, args)
    }

    /// `plpgsql_exec_trigger`: a call of a trigger function in PL/pgSQL. Its messages go to the messages of the triggers of the statement.
    ///
    /// # Errors
    ///
    /// The errors of the function.
    pub(super) fn pl_trigger(
        &self,
        call: &rupg_plpgsql::TriggerCall<'_>,
    ) -> rupg_common::Result<Option<Vec<Value>>> {
        let reader = self.reader();
        let result = rupg_plpgsql::trigger(&PlSpi::new(self, &reader), call);
        self.trigger_warnings.borrow_mut().extend(reader.take_warnings());
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
        let result = rupg_plpgsql::inline(&PlSpi::new(self, &reader), &src);
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

//! The functions in PL/pgSQL and `DO`, as SPI runs their queries: each query runs as a command of the statement that called the function, and a block with `EXCEPTION` runs in a subtransaction.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::Env;
use rupg_catalog::Catalog;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_func::Session;
use rupg_plpgsql::{Cache, Spi};
use rupg_sql::nodes::{DoStmt, Node};
use rupg_types::Value;

use super::foreign::Batch;
use super::{Applied, Connection};
use crate::query::Reader;

/// The SPI of one call: the reader of the statement that called the function, and the reader of the current command of the transaction.
struct PlSpi<'a> {
    conn: &'a Connection,
    /// The reader of the statement that called the function, which gets the warnings of the function.
    outer: &'a Reader<'a>,
    /// The reader of the last query and its command. A write ends the command, so that the next query sees the rows.
    reader: RefCell<Option<(u32, Rc<Reader<'a>>)>>,
    /// The command and the deferred triggers at the start of each subtransaction.
    marks: RefCell<Vec<(u32, Vec<Batch>)>>,
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
        let rows = plan.run(params, &*reader);
        self.forward(&reader);
        let rows = rows?;
        let Some(info) = plan.write() else {
            let n = rows.len() as u64;
            return Ok((rows, n));
        };
        let view = reader.view().cloned().unwrap_or_default();
        drop(reader);
        let Applied { count, written, view, batches, .. } =
            self.conn.apply_write(plan, params, &view, rows)?;
        let rows = if info.returning.is_some() {
            // A subquery of `RETURNING` reads with the snapshot of the statement.
            let reader = self.conn.reader().with_view(view);
            let rows = plan.returning(written, params, &reader);
            self.forward(&reader);
            rows?
        } else {
            Vec::new()
        };
        if !batches.is_empty() {
            self.conn.fire(batches)?;
        }
        Ok((rows, count))
    }

    fn begin_sub(&self) -> usize {
        let mut marks = self.marks.borrow_mut();
        marks.push((self.conn.command.get(), self.conn.deferred.borrow().clone()));
        marks.len() - 1
    }

    fn release_sub(&self, sub: usize) {
        self.marks.borrow_mut().truncate(sub);
    }

    fn rollback_sub(&self, sub: usize) {
        let mark = self.marks.borrow_mut().drain(sub..).next();
        let Some((command, deferred)) = mark else { return };
        let xid = self.conn.xid.get();
        if xid != 0 {
            self.conn.store.heap().rollback_to(xid, command);
        }
        *self.conn.deferred.borrow_mut() = deferred;
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

    /// `ExecuteDoStmt`: runs the code of `DO`. The messages of `RAISE` go to `warnings`.
    ///
    /// # Errors
    ///
    /// `42704` for a language that is not `plpgsql`, and the errors of the code.
    pub(super) fn run_do(&self, stmt: &DoStmt, warnings: &mut Vec<Error>) -> Result<(), Error> {
        let mut src = None;
        let mut language = None;
        for def in stmt.args.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let text = match &def.arg {
                Some(Node::String(s)) => Some(s.to_string()),
                _ => None,
            };
            match def.defname.as_deref() {
                Some("as") => src = text,
                Some("language") => language = text,
                _ => {}
            }
        }
        let language = language.unwrap_or_else(|| "plpgsql".to_owned());
        if language != "plpgsql" {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("language \"{language}\" does not exist"),
            ));
        }
        let Some(src) = src else {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "no inline code specified"));
        };
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let result = rupg_plpgsql::inline(&PlSpi::new(self, &reader), &src);
        warnings.extend(reader.take_warnings());
        result
    }
}

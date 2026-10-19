//! Cursors: `DECLARE CURSOR`, `FETCH`, `MOVE` and `CLOSE`, as `PerformCursorOpen`, `PerformPortalFetch` and `PerformPortalClose` of `portalcmds.c` run them, and the moves of `PortalRunSelect`, `DoPortalRewind` and `DoPortalRunFetch` of `pquery.c`.
//!
//! A cursor is a portal with a name. rupg runs the query of the cursor at `DECLARE` and keeps its rows, and a `FETCH` sends the rows with the output functions of that time. When the run fails, the first `FETCH` or `MOVE` that reads rows gives the error. PostgreSQL runs the query only when a fetch needs the rows, so it sends the rows before the row of the error first.
//!
//! A cursor ends with its transaction. A cursor `WITH HOLD` stays after the commit of its transaction, and goes after a rollback of it.

use std::sync::Arc;

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_sql::nodes::{
    CURSOR_OPT_ASENSITIVE, CURSOR_OPT_BINARY, CURSOR_OPT_HOLD, CURSOR_OPT_INSENSITIVE,
    CURSOR_OPT_NO_SCROLL, CURSOR_OPT_SCROLL, ClosePortalStmt, DeclareCursorStmt, FETCH_ALL,
    FetchDirection, FetchStmt,
};
use rupg_types::Value;
use rupg_wire::CommandTag;

use super::extended::{Portal, Prepared, Run};
use super::{Connection, Done};
use crate::query::{self, Column};
use crate::utility::{self, Notice};

/// The rows of a cursor and its place in them.
#[derive(Debug)]
pub(super) struct Cursor {
    rows: Vec<Vec<Value>>,
    /// The type of each column, for the output functions.
    types: Vec<u32>,
    /// The place of the scan of the executor: 0 is before the first row, `n` is on row `n`, and `rows.len() + 1` is after the last row.
    at: usize,
    /// `portalPos`: the number of the row that the cursor is on, from 1, or 0 before the first row.
    pos: u64,
    /// `atStart` and `atEnd`: the cursor is before the first row or after the last row.
    at_start: bool,
    at_end: bool,
    /// `CURSOR_OPT_SCROLL`: the cursor can move backward.
    scroll: bool,
    binary: bool,
    hold: bool,
    /// The query string of `DECLARE`.
    text: String,
    /// The start of the statement that declared the cursor.
    created: i64,
    /// The error of the run of the query.
    error: Option<Error>,
    /// True while the transaction that declared the cursor is open.
    pub(super) fresh: bool,
}

/// The error of a backward move of a cursor that cannot scan backward.
fn forward_only() -> Error {
    Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "cursor can only scan forward")
        .with_hint("Declare it with SCROLL option to enable backward scan.")
}

/// The error of a cursor that is not there.
pub(super) fn no_cursor(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_CURSOR, format!("cursor \"{name}\" does not exist"))
}

impl Cursor {
    /// True for a cursor `WITH HOLD` that a transaction before this one declared.
    pub(super) fn held(&self) -> bool {
        self.hold && !self.fresh
    }

    /// True for a cursor `WITH HOLD`.
    pub(super) fn holdable(&self) -> bool {
        self.hold
    }

    /// The type of each column.
    pub(super) fn types(&self) -> &[u32] {
        &self.types
    }

    /// An `Execute` of the portal of the cursor: at most `max_rows` rows forward, or all the rows when `max_rows` is zero or less. It gives the rows and true when the cursor is at its end.
    pub(super) fn forward(&mut self, max_rows: i32) -> Result<(Vec<Vec<Value>>, bool), Error> {
        let count = if max_rows > 0 { i64::from(max_rows) } else { FETCH_ALL };
        let mut found = Vec::new();
        self.run_select(true, count, Some(&mut found))?;
        Ok((self.values(&found), self.at_end))
    }

    /// The values of the rows with the indexes of `found`.
    fn values(&self, found: &[usize]) -> Vec<Vec<Value>> {
        found.iter().map(|&i| self.rows[i].clone()).collect()
    }

    /// The row of the cursor in `pg_cursor`, with its name.
    pub(super) fn row(&self, name: &[u8]) -> Vec<Value> {
        vec![
            Value::text(String::from_utf8_lossy(name)),
            Value::text(self.text.as_str()),
            Value::Bool(self.hold),
            Value::Bool(self.binary),
            Value::Bool(self.scroll),
            Value::TimestampTz(self.created),
        ]
    }

    /// `ExecutorRun` in one direction: at most `count` rows, or all the rows when `count` is 0. It gives the number of rows, and puts their indexes in `out`.
    fn scan(&mut self, forward: bool, count: u64, mut out: Option<&mut Vec<usize>>) -> u64 {
        let last = self.rows.len();
        let mut done = 0;
        while count == 0 || done < count {
            if forward && self.at < last {
                self.at += 1;
            } else if !forward && self.at > 1 {
                self.at -= 1;
            } else {
                self.at = if forward { last + 1 } else { 0 };
                break;
            }
            if let Some(out) = out.as_deref_mut() {
                out.push(self.at - 1);
            }
            done += 1;
        }
        done
    }

    /// `PortalRunSelect`: at most `count` rows in one direction, or all of them for `FETCH_ALL`, and the change of the place of the cursor.
    fn run_select(
        &mut self,
        forward: bool,
        count: i64,
        out: Option<&mut Vec<usize>>,
    ) -> Result<u64, Error> {
        if !forward && !self.scroll {
            return Err(forward_only());
        }
        let stop = if forward { self.at_end } else { self.at_start };
        if stop || count <= 0 {
            return Ok(0);
        }
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let count = if count == FETCH_ALL { 0 } else { count.unsigned_abs() };
        let done = self.scan(forward, count, out);
        if forward {
            if done > 0 {
                self.at_start = false;
            }
            if count == 0 || done < count {
                self.at_end = true;
            }
            self.pos += done;
        } else {
            if done > 0 && self.at_end {
                self.at_end = false;
                self.pos += 1;
            }
            if count == 0 || done < count {
                self.at_start = true;
                self.pos = 0;
            } else {
                self.pos -= done;
            }
        }
        Ok(done)
    }

    /// `DoPortalRewind`.
    fn rewind(&mut self) -> Result<(), Error> {
        if self.at_start && !self.at_end {
            return Ok(());
        }
        if !self.scroll {
            return Err(forward_only());
        }
        self.at = 0;
        self.at_start = true;
        self.at_end = false;
        self.pos = 0;
        Ok(())
    }

    /// `DoPortalRunFetch`: the rows of a `FETCH`, or the count of a `MOVE` when `out` is `None`.
    fn run_fetch(
        &mut self,
        direction: FetchDirection,
        mut count: i64,
        mut out: Option<&mut Vec<usize>>,
    ) -> Result<u64, Error> {
        let mut forward = true;
        match direction {
            FetchDirection::FETCH_FORWARD | FetchDirection::FETCH_BACKWARD => {
                forward = direction == FetchDirection::FETCH_FORWARD;
                if count < 0 {
                    forward = !forward;
                    count = -count;
                }
            }
            FetchDirection::FETCH_ABSOLUTE => {
                if count > 0 {
                    let pos = self.pos;
                    if (count - 1).unsigned_abs() <= pos / 2 || pos >= FETCH_ALL.unsigned_abs() {
                        self.rewind()?;
                        if count > 1 {
                            self.run_select(true, count - 1, None)?;
                        }
                    } else {
                        let mut pos = i64::try_from(pos).unwrap_or(FETCH_ALL);
                        if self.at_end {
                            pos += 1;
                        }
                        if count <= pos {
                            self.run_select(false, pos - count + 1, None)?;
                        } else if count > pos + 1 {
                            self.run_select(true, count - pos - 1, None)?;
                        }
                    }
                    return self.run_select(true, 1, out);
                }
                if count < 0 {
                    self.run_select(true, FETCH_ALL, None)?;
                    if count < -1 {
                        self.run_select(false, -count - 1, None)?;
                    }
                    return self.run_select(false, 1, out);
                }
                self.rewind()?;
                return self.run_select(true, 0, out);
            }
            FetchDirection::FETCH_RELATIVE => {
                if count > 0 {
                    if count > 1 {
                        self.run_select(true, count - 1, None)?;
                    }
                    return self.run_select(true, 1, out);
                }
                if count < 0 {
                    if count < -1 {
                        self.run_select(false, -count - 1, None)?;
                    }
                    return self.run_select(false, 1, out);
                }
            }
            _ => return Err(Error::internal("bogus direction")),
        }
        if count == 0 {
            let on_row = !self.at_start && !self.at_end;
            if out.is_none() {
                return Ok(u64::from(on_row));
            }
            if on_row {
                self.run_select(false, 1, None)?;
                count = 1;
                forward = true;
            }
        }
        if !forward && count == FETCH_ALL && out.is_none() {
            let mut result = self.pos;
            if result > 0 && !self.at_end {
                result -= 1;
            }
            self.rewind()?;
            return Ok(result);
        }
        self.run_select(forward, count, out.take())
    }
}

/// The bits of the options of `DECLARE`.
fn has(options: i32, bit: i32) -> bool {
    options & bit != 0
}

impl Connection {
    /// `transformDeclareCursorStmt` and the plan of `PerformCursorOpen`: the checks of the options and the plan of the query. `params` has the types of the parameters of `Parse`.
    pub(super) fn plan_cursor(
        &self,
        stmt: &DeclareCursorStmt,
        params: &Params,
        notices: &mut Vec<Notice>,
    ) -> Result<Plan, Error> {
        let both = |a: &str, b: &str| {
            Error::new(
                SqlState::INVALID_CURSOR_DEFINITION,
                format!("cannot specify both {a} and {b}"),
            )
        };
        if has(stmt.options, CURSOR_OPT_SCROLL) && has(stmt.options, CURSOR_OPT_NO_SCROLL) {
            return Err(both("SCROLL", "NO SCROLL"));
        }
        if has(stmt.options, CURSOR_OPT_ASENSITIVE) && has(stmt.options, CURSOR_OPT_INSENSITIVE) {
            return Err(both("ASENSITIVE", "INSENSITIVE"));
        }
        let Some(node) = &stmt.query else {
            return Err(Error::internal("DECLARE CURSOR with no query"));
        };
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let plan = query::plan(node, &reader, params);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        if plan.modifying_cte() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "DECLARE CURSOR must not contain data-modifying statements in WITH",
            ));
        }
        // `transformDeclareCursorStmt`: a cursor that locks rows must be read-only.
        if let Some(clause) = plan.lock_clause() {
            let checks = [
                (CURSOR_OPT_HOLD, "DECLARE CURSOR WITH HOLD", "not supported", "Holdable"),
                (CURSOR_OPT_SCROLL, "DECLARE SCROLL CURSOR", "not supported", "Scrollable"),
                (CURSOR_OPT_INSENSITIVE, "DECLARE INSENSITIVE CURSOR", "not valid", "Insensitive"),
            ];
            if let Some((option, command, verdict, kind)) =
                checks.into_iter().find(|check| has(stmt.options, check.0))
            {
                let code = if option == CURSOR_OPT_INSENSITIVE {
                    SqlState::INVALID_CURSOR_DEFINITION
                } else {
                    SqlState::FEATURE_NOT_SUPPORTED
                };
                return Err(Error::new(code, format!("{command} ... {clause} is {verdict}"))
                    .with_detail(format!("{kind} cursors must be READ ONLY.")));
            }
        }
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        Ok(plan)
    }

    /// `PerformCursorOpen`: makes the cursor with the rows of the query. `plan` is the plan of [`Connection::plan_cursor`] that `Bind` folded with the values of `params`, or `None` for a `DECLARE` of a `Query` message.
    pub(super) fn declare(
        &mut self,
        stmt: &DeclareCursorStmt,
        text: &str,
        plan: Option<Plan>,
        params: &[Value],
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let name = stmt.portalname.as_deref().unwrap_or_default();
        if name.is_empty() {
            return Err(Error::new(
                SqlState::INVALID_CURSOR_NAME,
                "invalid cursor name: must not be empty",
            ));
        }
        let hold = has(stmt.options, CURSOR_OPT_HOLD);
        let block = |c: &Connection| {
            if hold { Ok(()) } else { utility::require_block(&c.transaction, "DECLARE CURSOR") }
        };
        let plan = match plan {
            Some(plan) => {
                block(self)?;
                plan
            }
            None => {
                let plan = self.plan_cursor(stmt, &Params::default(), notices)?;
                block(self)?;
                let reader = self.reader();
                let folded = plan.fold(None, &reader);
                notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
                folded?
            }
        };
        self.portals.make_room(name.as_bytes()).map_err(super::extended::protocol_error)?;
        let reader = self.reader();
        let run = plan.run(params, &reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        drop(reader);
        let (rows, error) = match run {
            Ok(rows) => (rows, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let scroll = has(stmt.options, CURSOR_OPT_SCROLL)
            || (!has(stmt.options, CURSOR_OPT_NO_SCROLL) && plan.backward_scan());
        let cursor = Cursor {
            rows,
            types: plan.columns().iter().map(|target| target.expr.ty).collect(),
            at: 0,
            pos: 0,
            at_start: true,
            at_end: false,
            scroll,
            binary: has(stmt.options, CURSOR_OPT_BINARY),
            hold,
            text: text.to_owned(),
            created: self.statement_start,
            error,
            fresh: true,
        };
        let columns = query::columns(&plan);
        let statement = Prepared::new(
            text.to_owned(),
            stmt.query.clone(),
            Vec::new(),
            Some(columns),
            None,
            self.statement_start,
            false,
        );
        let mut portal = Portal::cursor(Arc::new(statement), cursor);
        portal.level = self.transaction.depth();
        self.portals.insert(name.as_bytes(), portal);
        Ok(Done::Tag(CommandTag::DeclareCursor))
    }

    /// The columns of a `FETCH` that gives rows, or `None` for a `MOVE` and for a cursor that is not there, as `UtilityTupleDescriptor` gives them.
    pub(super) fn fetch_columns(&self, stmt: &FetchStmt) -> Option<Vec<Column>> {
        if stmt.ismove {
            return None;
        }
        let name = stmt.portalname.as_deref().unwrap_or_default();
        self.portals.find(name.as_bytes()).and_then(|portal| portal.statement.columns.clone())
    }

    /// `PerformPortalFetch`: the rows of a `FETCH` in the formats of `formats`, or the count of a `MOVE`. The text format is the default, and a `FETCH` of a `Query` message on a `BINARY` cursor uses the binary format.
    pub(super) fn fetch_cursor(
        &mut self,
        stmt: &FetchStmt,
        formats: Option<&[i16]>,
    ) -> Result<Done, Error> {
        let name = stmt.portalname.as_deref().unwrap_or_default();
        let portal = self.portals.find_mut(name.as_bytes()).ok_or_else(|| no_cursor(name))?;
        let columns = portal.statement.columns.clone().unwrap_or_default();
        let Run::Cursor(cursor) = &mut portal.run else { return Err(no_cursor(name)) };
        if stmt.ismove {
            let count = cursor.run_fetch(stmt.direction, stmt.howMany, None)?;
            return Ok(Done::Count(CommandTag::Move, count));
        }
        let mut found = Vec::new();
        cursor.run_fetch(stmt.direction, stmt.howMany, Some(&mut found))?;
        let format = i16::from(formats.is_none() && cursor.binary);
        let values = cursor.values(&found);
        let types = cursor.types.clone();
        let all = vec![format; columns.len()];
        let formats = formats.unwrap_or(&all);
        let reader = self.reader();
        let rows = values
            .iter()
            .map(|values| query::encode(&types, values, &reader, formats))
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Done::Fetch { columns, rows, format })
    }

    /// `PerformPortalClose`: `CLOSE` of a cursor, or of all the portals but `current` for `CLOSE ALL`.
    pub(super) fn close_cursor(
        &mut self,
        stmt: &ClosePortalStmt,
        current: &[u8],
    ) -> Result<Done, Error> {
        let Some(name) = stmt.portalname.as_deref() else {
            self.portals.retain(|name, _| name == current);
            return Ok(Done::Tag(CommandTag::CloseCursorAll));
        };
        if self.portals.close(name.as_bytes()).is_none() {
            return Err(no_cursor(name));
        }
        Ok(Done::Tag(CommandTag::CloseCursor))
    }
}

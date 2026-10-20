//! `COPY ... FROM STDIN` and `COPY ... FROM` a file of the server, as `DoCopy`, `BeginCopyFrom`, `CopyFrom` and `copyfromparse.c` run it. The copy reads the fields of each line, and `NextCopyFrom` makes the values of the columns with the input functions or the receive functions of their types. The other columns get their defaults from a plan of `INSERT`, which also checks the constraints. The copy keeps up to `MAX_BUFFERED_TUPLES` rows and then writes them in the heap, as `CopyMultiInsertInfoFlush` does. The triggers of the foreign keys fire at the end of the copy.

use std::path::Path;
use std::rc::Rc;

use rupg_analyze::{CopyTarget, Params};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_pgcatalog::builtin;
use rupg_sql::Severity;
use rupg_sql::nodes::{CopyStmt, RangeVar};
use rupg_types::Value;
use rupg_wire::{CommandTag, OutBuf};

use super::copy::{BINARY_SIGNATURE, ForceQuote, Format, OnError, Verbosity, options};
use super::foreign::Batch;
use super::{Connection, Done};
use crate::heap::{Changed, View};
use crate::query::{self, Reader};
use crate::utility::Notice;

/// `MAX_COPY_DATA_DISPLAY`: the number of bytes of a line that the context of an error shows.
const MAX_DISPLAY: usize = 100;

/// `MAX_BUFFERED_TUPLES`: the number of rows that the copy keeps before it writes them in the heap.
const MAX_BUFFERED: usize = 1000;

/// Where the data of a copy comes from: the client, which gets `CopyInResponse`, or a file of the server, which the copy reads at the same point.
#[derive(Debug)]
pub(super) enum Begin<'a> {
    Client(&'a mut OutBuf),
    /// The name of the file, and the bytes of the file after the copy reads it.
    File(&'a str, &'a mut Vec<u8>),
}

/// A line of data that the copy read, before `NextCopyFrom` makes its values.
#[derive(Debug)]
struct Row {
    /// `cur_lineno` of the row.
    line: u64,
    /// The line, for the context of an error. A row of the binary format has no line.
    text: Option<Vec<u8>>,
    /// The fields of the line, with `None` for a null value.
    fields: Vec<Option<Vec<u8>>>,
    /// `defaults`: true for a field that is the marker of `DEFAULT`. It is empty for a row with no marker.
    defaults: Vec<bool>,
}

/// The fields of a line of the text format or the CSV format.
#[derive(Debug)]
struct Fields {
    /// The fields, with `None` for a null value.
    fields: Vec<Option<Vec<u8>>>,
    /// The marks of the fields that are the marker of `DEFAULT`, or nothing for a line with no marker.
    defaults: Vec<bool>,
}

impl Fields {
    /// The fields of a line. The marks become empty when no field is a marker, so that the copy uses the plan with no defaults.
    fn new(fields: Vec<Option<Vec<u8>>>, mut defaults: Vec<bool>) -> Fields {
        if !defaults.contains(&true) {
            defaults.clear();
        }
        Fields { fields, defaults }
    }
}

/// The values that [`CopyIn::values`] made of the fields of a row.
#[derive(Debug)]
struct Made {
    /// The values of the selected columns with no marker of `DEFAULT`, or `None` when `ON_ERROR IGNORE` skips the row.
    values: Option<Vec<Value>>,
    /// The column and the field of each value that the input function did not accept: the column of a row that the copy skips, or each column that `ON_ERROR SET_NULL` sets to null.
    failed: Vec<(usize, Option<Vec<u8>>)>,
}

/// The plans of a copy and the rows that it did not write yet.
#[derive(Debug)]
struct Load {
    /// The `INSERT` that makes the rows of the table, or `None` for a relation that is not in the catalog of the session.
    plan: Option<Rc<Plan>>,
    /// The table and its name for the plans of the rows with markers of `DEFAULT`, or `None` for a copy with no `DEFAULT`.
    source: Option<(CopyTarget, RangeVar)>,
    /// The plan for each set of columns that have the marker of `DEFAULT` in a row.
    defaulted: Vec<(Vec<bool>, Rc<Plan>)>,
    /// The query of the condition `WHERE`.
    filter: Option<Plan>,
    /// The type and the type modifier of each selected column.
    types: Vec<(u32, i32)>,
    /// True for a selected column whose type is a domain or an array of a domain. The input checks the value with the constraints of the domain, as `domain_in` does.
    domains: Vec<bool>,
    /// True for a selected column that has a default, `defexprs`.
    has_default: Vec<bool>,
    /// `force_notnull_flags` and `force_null_flags` of each selected column.
    force_not_null: Vec<bool>,
    force_null: Vec<bool>,
    on_error: OnError,
    reject_limit: Option<i64>,
    verbosity: Verbosity,
    /// `num_errors`: the number of rows with a field that the input function did not accept.
    errors: u64,
    /// The rows that the copy checked and did not write in the heap yet, and the line of each row.
    rows: Vec<Vec<Value>>,
    lines: Vec<u64>,
    /// The number of rows that the copy wrote.
    count: u64,
    /// The rows for the triggers of the foreign keys.
    batches: Vec<Batch>,
}

/// `EolType`: the line ending of the data, which the first line sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eol {
    Unknown,
    Nl,
    Cr,
    CrNl,
}

/// What [`CopyIn::next_line`] found.
#[derive(Debug)]
enum Read {
    /// A line with no line ending, and the number of newlines in quoted fields of the line.
    Line(Vec<u8>, u64),
    /// The end-of-copy marker `\.`.
    End,
    /// The end of the data.
    Eof,
}

/// The state of a `COPY ... FROM STDIN` from `CopyInResponse` to `CopyDone`.
#[derive(Debug)]
pub(super) struct CopyIn {
    /// The text of the `Query` message. The session runs the statements after the `COPY` when the copy ends.
    pub(super) text: String,
    /// The index of the `COPY` in the statements of the text.
    pub(super) index: usize,
    /// The name of the relation, for the context of the errors.
    name: String,
    /// The names of the selected columns, for `HEADER MATCH`.
    names: Vec<String>,
    format: Format,
    delimiter: u8,
    null: Vec<u8>,
    quote: u8,
    escape: u8,
    /// The number of header lines that remain to skip.
    header: i32,
    header_match: bool,
    /// `cur_lineno`: the number of the last line that the copy read.
    line: u64,
    eol: Eol,
    /// The data that the copy did not use yet.
    buf: Vec<u8>,
    /// True after the end-of-copy marker, or after the trailer of the binary format.
    ended: bool,
    /// True when the copy read the header of the binary format.
    binary_header: bool,
    /// `default_print`: the marker of `DEFAULT`.
    default: Option<Vec<u8>>,
    /// The lines that the copy read and did not make values of yet.
    pending: Vec<Row>,
    load: Load,
    /// The notices of `ON_ERROR`, which the session sends before the result of the copy.
    pub(super) notices: Vec<Notice>,
}

impl Connection {
    /// `COPY ... FROM` a file: the checks of `DoCopy`, then the copy reads the whole file where `BeginCopyFrom` opens it, and runs as a copy that got all the bytes at once. A program is not supported yet.
    pub(super) fn copy_from_file(
        &self,
        stmt: &CopyStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        if stmt.is_program {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM a program is not supported yet",
            ));
        }
        if !self.superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied to COPY from a file",
            )
            .with_detail(
                "Only roles with privileges of the \"pg_read_server_files\" role may COPY from a file.",
            )
            .with_hint(
                "Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.",
            ));
        }
        let name = stmt.filename.as_deref().unwrap_or_default();
        let mut bytes = Vec::new();
        let mut copy = self.copy_from(stmt, Begin::File(name, &mut bytes))?;
        let read = self.copy_data(&mut copy, &bytes);
        let count = read.and_then(|()| self.copy_done(&mut copy));
        notices.append(&mut copy.notices);
        Ok(Done::Count(CommandTag::Copy, count?))
    }

    /// `AllocateFile` and the check of `BeginCopyFrom` for a file of the server: the bytes of the file. A relative name is in the data directory, which is the working directory of the server.
    fn read_server_file(&self, name: &str) -> Result<Vec<u8>, Error> {
        let Some(io) = &self.io else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM a file in a session with no server is not supported yet",
            ));
        };
        let path = Path::new(name);
        let reading = |error: Error| {
            let prefix = format!("could not open file \"{name}\": ");
            let text = error.message().strip_prefix(&prefix).unwrap_or(error.message()).to_owned();
            Error::new(error.state(), format!("could not open file \"{name}\" for reading: {text}"))
        };
        if let Ok(mode) = io.mode(path)
            && !mode.regular
            && io.read_dir(path).is_ok()
        {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is a directory"),
            ));
        }
        io.read_file(path).map_err(reading)
    }

    /// The checks of `DoCopy` and `BeginCopyFrom` for `COPY ... FROM STDIN` or a file, then `CopyInResponse` or the read of the file, and the checks of `CopyFrom`. The caller starts the copy with the state that this gives.
    pub(super) fn copy_from(&self, stmt: &CopyStmt, begin: Begin<'_>) -> Result<CopyIn, Error> {
        if stmt.filename.is_some() && matches!(begin, Begin::Client(_)) {
            let what = if stmt.is_program { "a program" } else { "a file" };
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("COPY FROM {what} is not supported yet"),
            ));
        }
        let Some(rv) = &stmt.relation else {
            return Err(Error::internal("a COPY FROM with no relation"));
        };
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let target = rupg_analyze::copy_target(stmt, rv, &reader)?;
        let attnums: Vec<i16> =
            target.selected.iter().map(|&i| i16::try_from(i + 1).unwrap_or(i16::MAX)).collect();
        if !rupg_func::insert_allowed(target.relid, self.role, &attnums, &reader)? {
            let object = match target.kind {
                b'v' => "view",
                b'm' => "materialized view",
                b'S' => "sequence",
                b'f' => "foreign table",
                _ => "table",
            };
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {object} {}", target.name),
            ));
        }
        if self.settings.borrow().get("transaction_read_only").as_deref() == Some("on") {
            return Err(Error::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                "cannot execute COPY FROM in a read-only transaction",
            ));
        }
        let options = options(&stmt.options, true)?;
        for (force, option) in
            [(&options.force_not_null, "FORCE_NOT_NULL"), (&options.force_null, "FORCE_NULL")]
        {
            let ForceQuote::Names(names) = force else { continue };
            for at in rupg_analyze::attnums(&target.columns, names, Some(&target.name))? {
                if !target.selected.contains(&at) {
                    return Err(Error::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!(
                            "{option} column \"{}\" not referenced by COPY",
                            target.columns[at]
                        ),
                    ));
                }
            }
        }
        if let Some(name) = options.encoding
            && !matches!(name, "UTF8" | "SQL_ASCII")
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("COPY with ENCODING {name} is not supported yet"),
            ));
        }
        let catalog = self.catalog();
        if catalog.triggers_of(target.relid).iter().any(|t| !t.internal && t.fires()) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM into a table with triggers is not supported yet",
            ));
        }
        let rel = catalog.relation(target.relid);
        // The columns of the target are the columns that are not dropped.
        let live: Vec<&rupg_catalog::Column> =
            rel.map(|rel| rel.columns.iter().filter(|c| !c.dropped).collect()).unwrap_or_default();
        let types: Vec<(u32, i32)> = live.iter().map(|column| (column.ty, column.typmod)).collect();
        // `build_column_default`: the default of the column, the next value of an identity column, or the default of the domain of the column.
        let has_default: Vec<bool> = target
            .selected
            .iter()
            .map(|&i| {
                live.get(i).is_some_and(|column| {
                    column.has_default
                        || column.identity != 0
                        || catalog
                            .type_by_oid(column.ty)
                            .and_then(|ty| ty.domain.as_ref())
                            .is_some_and(|domain| domain.default.is_some())
                })
            })
            .collect();
        let filter = match (&stmt.whereClause, rel) {
            (Some(condition), Some(_)) => {
                let node = rupg_analyze::copy_filter(&target, rv, condition);
                let params =
                    Params { types: types.iter().map(|&(ty, _)| ty).collect(), variable: false };
                let plan = query::plan(&node, &reader, &params)?.fold(None, &reader)?;
                Some(plan)
            }
            _ => None,
        };
        let flags = |force: &ForceQuote| -> Result<Vec<bool>, Error> {
            Ok(match force {
                ForceQuote::None => vec![false; target.selected.len()],
                ForceQuote::All => vec![true; target.selected.len()],
                ForceQuote::Names(names) => {
                    let named = rupg_analyze::attnums(&target.columns, names, Some(&target.name))?;
                    target.selected.iter().map(|at| named.contains(at)).collect()
                }
            })
        };
        let force_not_null = flags(&options.force_not_null)?;
        let force_null = flags(&options.force_null)?;
        let binary = options.format == Format::Binary;
        match begin {
            Begin::Client(out) => out.copy_in_response(
                i8::from(binary),
                &vec![i16::from(binary); target.selected.len()],
            ),
            Begin::File(name, bytes) => *bytes = self.read_server_file(name)?,
        }
        let name = &target.name;
        let wrong = |message: String| Error::new(SqlState::WRONG_OBJECT_TYPE, message);
        match target.kind {
            b'r' | b'p' | b'f' => {}
            b'v' => {
                return Err(wrong(format!("cannot copy to view \"{name}\"")).with_hint(
                    "To enable copying to a view, provide an INSTEAD OF INSERT trigger.",
                ));
            }
            b'm' => return Err(wrong(format!("cannot copy to materialized view \"{name}\""))),
            b'S' => return Err(wrong(format!("cannot copy to sequence \"{name}\""))),
            _ => return Err(wrong(format!("cannot copy to non-table relation \"{name}\""))),
        }
        if options.freeze {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FREEZE is not supported yet",
            ));
        }
        let plan = match rel {
            Some(_) => Some(Rc::new(insert_plan(&target, rv, &[], &reader)?)),
            None => None,
        };
        let types: Vec<(u32, i32)> =
            target.selected.iter().filter_map(|&i| types.get(i).copied()).collect();
        let domains = types
            .iter()
            .map(|&(ty, _)| {
                let element = builtin::type_by_oid(ty).map_or(0, |row| row.elem);
                builtin::domain_by_oid(ty).is_some() || builtin::domain_by_oid(element).is_some()
            })
            .collect();
        let source = (rel.is_some() && options.default.is_some())
            .then(|| (target.clone(), RangeVar::clone(rv)));
        let load = Load {
            plan,
            source,
            defaulted: Vec::new(),
            filter,
            types,
            domains,
            has_default,
            force_not_null,
            force_null,
            on_error: options.on_error,
            reject_limit: options.reject_limit,
            verbosity: options.verbosity,
            errors: 0,
            rows: Vec::new(),
            lines: Vec::new(),
            count: 0,
            batches: Vec::new(),
        };
        let names = target.selected.iter().map(|&i| target.columns[i].clone()).collect();
        Ok(CopyIn {
            text: String::new(),
            index: 0,
            name: target.name,
            names,
            format: options.format,
            delimiter: options.delimiter,
            null: options.null.into_bytes(),
            quote: options.quote,
            escape: options.escape,
            header: if options.header_match { 1 } else { options.header },
            header_match: options.header_match,
            line: 0,
            eol: Eol::Unknown,
            buf: Vec::new(),
            ended: false,
            binary_header: false,
            default: options.default.map(String::into_bytes),
            pending: Vec::new(),
            load,
            notices: Vec::new(),
        })
    }

    /// `CopyData` of a copy: the copy reads the lines that are complete and makes the values of their rows. The rows before a line with an error come first, as `CopyFrom` reads one row at a time.
    pub(super) fn copy_data(&self, copy: &mut CopyIn, bytes: &[u8]) -> Result<(), Error> {
        let read = copy.data(bytes);
        self.load_rows(copy)?;
        read
    }

    /// `CopyDone` of a copy: the copy reads the rest of the data, writes the rows that remain, and fires the triggers of the foreign keys. It gives the number of rows that the copy wrote.
    pub(super) fn copy_done(&self, copy: &mut CopyIn) -> Result<u64, Error> {
        let read = copy.done();
        self.load_rows(copy)?;
        read?;
        self.flush_rows(copy)?;
        if copy.load.count > 0 {
            self.command.set(self.command.get() + 1);
        }
        let batches = std::mem::take(&mut copy.load.batches);
        if !batches.is_empty() {
            self.fire(batches)?;
        }
        let errors = copy.load.errors;
        if errors > 0 && copy.load.verbosity >= Verbosity::Default {
            let message = match (copy.load.on_error, errors) {
                (OnError::SetNull, 1) => {
                    "in 1 row, columns were set to null due to data type incompatibility"
                        .to_string()
                }
                (OnError::SetNull, _) => format!(
                    "in {errors} rows, columns were set to null due to data type incompatibility"
                ),
                (_, 1) => "1 row was skipped due to data type incompatibility".to_string(),
                _ => format!("{errors} rows were skipped due to data type incompatibility"),
            };
            copy.notices.push(Notice {
                severity: Severity::Notice,
                error: Error::new(SqlState::SUCCESSFUL_COMPLETION, message),
            });
        }
        Ok(copy.load.count)
    }

    /// `NextCopyFrom` and the checks of `CopyFrom` for the lines that the copy read: the values of the columns, the defaults of the other columns, the condition `WHERE` and the constraints of the table.
    fn load_rows(&self, copy: &mut CopyIn) -> Result<(), Error> {
        if copy.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut copy.pending);
        if copy.load.plan.is_none() {
            return Err(copy.at_line(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM with rows is not supported yet",
            )));
        }
        let reader = self.reader();
        for row in pending {
            let made = copy.values(&row, &reader)?;
            copy.count_failed(&row, &made)?;
            let Some(params) = made.values else { continue };
            let plan = copy.load.plan_for(&row.defaults, &reader)?;
            let width = plan.write().map_or(0, |info| info.width);
            let line = |error: Error| error.with_context(copy.row_context(&row));
            let rows = plan.run_unchecked(&params, &reader).map_err(line)?;
            let rows = match &copy.load.filter {
                Some(filter) => {
                    let mut kept = Vec::with_capacity(rows.len());
                    for values in rows {
                        let shown = filter.run(&values[..width.min(values.len())], &reader);
                        if matches!(
                            shown.map_err(line)?.first().and_then(|r| r.first()),
                            Some(Value::Bool(true))
                        ) {
                            kept.push(values);
                        }
                    }
                    kept
                }
                None => rows,
            };
            let rows = plan.check(rows, &reader).map_err(line)?;
            copy.load.lines.extend(rows.iter().map(|_| row.line));
            copy.load.rows.extend(rows);
        }
        drop(reader);
        if copy.load.rows.len() >= MAX_BUFFERED {
            self.flush_rows(copy)?;
        }
        Ok(())
    }

    /// `CopyMultiInsertBufferFlush`: the rows that the copy kept go in the heap. A duplicate key has the context of the line of its row.
    fn flush_rows(&self, copy: &mut CopyIn) -> Result<(), Error> {
        let load = &mut copy.load;
        if load.rows.is_empty() {
            return Ok(());
        }
        let Some(info) = load.plan.as_deref().and_then(Plan::write) else {
            return Err(Error::internal("a copy with no plan of its rows"));
        };
        let heap = self.store.heap();
        if self.xid.get() == 0 {
            self.xid.set(heap.begin(self.pid));
        }
        let reader = self.reader();
        let Some(view) = reader.view() else {
            return Err(Error::internal("the reader of the session has no view of the rows"));
        };
        let view = View { xid: self.xid.get(), ..view.clone() };
        drop(reader);
        let rows = std::mem::take(&mut load.rows);
        let lines = std::mem::take(&mut load.lines);
        let count = rows.len() as u64;
        let new_rows = if info.foreign.is_empty() { Vec::new() } else { rows.clone() };
        if info.unique.is_empty() {
            heap.insert(info.table, &view, rows, &[], &[])?;
        } else {
            let mut kept = Vec::new();
            loop {
                match heap.insert(info.table, &view, rows.clone(), &info.unique, &kept)? {
                    Changed::Done(_) => break,
                    Changed::Duplicate { index, key } => {
                        let line = duplicate_line(info, index, &key, &rows, &lines);
                        let conflict = Changed::Duplicate { index, key };
                        let error = match self.key_conflict(info, &view, conflict) {
                            Ok(_) => Error::internal("a duplicate key with no error"),
                            Err(error) => error,
                        };
                        return Err(error.with_context(format!("COPY {}, line {line}", copy.name)));
                    }
                    conflict => kept = self.key_conflict(info, &view, conflict)?,
                }
            }
        }
        load.count += count;
        if !new_rows.is_empty() {
            load.batches.push(Batch::new(info, &info.foreign, view.command, new_rows, Vec::new()));
        }
        Ok(())
    }
}

/// The plan of the `INSERT` of [`rupg_analyze::copy_insert`]. The parameters get the types of their columns.
fn insert_plan(
    target: &CopyTarget,
    rv: &RangeVar,
    defaults: &[bool],
    reader: &Reader<'_>,
) -> Result<Plan, Error> {
    let node = rupg_analyze::copy_insert(target, rv, defaults);
    let count =
        (0..target.selected.len()).filter(|&k| !defaults.get(k).copied().unwrap_or(false)).count();
    let params = Params { types: vec![0; count], variable: true };
    query::plan(&node, reader, &params)?.fold(None, reader)
}

impl Load {
    /// The plan for a row whose fields in `defaults` are the marker of `DEFAULT`. The copy plans the `INSERT` for a set of such columns at the first row that has it.
    fn plan_for(&mut self, defaults: &[bool], reader: &Reader<'_>) -> Result<Rc<Plan>, Error> {
        let base = self.plan.as_ref().ok_or_else(|| Error::internal("a copy with no plan"))?;
        if !defaults.contains(&true) {
            return Ok(Rc::clone(base));
        }
        if let Some((_, plan)) = self.defaulted.iter().find(|(set, _)| set == defaults) {
            return Ok(Rc::clone(plan));
        }
        let Some((target, rv)) = &self.source else {
            return Err(Error::internal("a marker of DEFAULT in a copy with no DEFAULT"));
        };
        let plan = Rc::new(insert_plan(target, rv, defaults, reader)?);
        self.defaulted.push((defaults.to_vec(), Rc::clone(&plan)));
        Ok(plan)
    }
}

/// The line of the row whose key is a duplicate. The first row with the key can conflict only with a row that was in the table, and a later row with the key conflicts with the first row.
fn duplicate_line(
    info: &rupg_analyze::Write,
    index: usize,
    key: &[Value],
    rows: &[Vec<Value>],
    lines: &[u64],
) -> u64 {
    let Some(unique) = info.unique.get(index) else { return lines.last().copied().unwrap_or(0) };
    let mut matches = rows.iter().enumerate().filter(|(_, row)| {
        unique.columns.iter().zip(key).all(|(&at, value)| row.get(at) == Some(value))
    });
    let first = matches.next().map(|(at, _)| at);
    let at = matches.next().map(|(at, _)| at).or(first);
    at.and_then(|at| lines.get(at)).or(lines.last()).copied().unwrap_or(0)
}

/// The text of a line or a value for the context of an error, `limit_printout_length`: at most `MAX_COPY_DATA_DISPLAY` bytes, cut at the start of a character, and `...` after a text that the cut made shorter.
fn shown(text: &[u8]) -> String {
    if text.len() > MAX_DISPLAY {
        let mut cut = MAX_DISPLAY;
        while cut > 0 && (text[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        format!("{}...", String::from_utf8_lossy(&text[..cut]))
    } else {
        String::from_utf8_lossy(text).into_owned()
    }
}

impl CopyIn {
    /// `CopyData`: the copy reads the lines that are complete.
    pub(super) fn data(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.ended {
            // After the end-of-copy marker, `CopyReadLine` drops the data until `CopyDone`. After the trailer of the binary format, any more data is an error.
            if self.format == Format::Binary && !bytes.is_empty() {
                return Err(self.at_line(Error::new(
                    SqlState::BAD_COPY_FILE_FORMAT,
                    "received copy data after EOF marker",
                )));
            }
            return Ok(());
        }
        self.buf.extend_from_slice(bytes);
        self.read(false)
    }

    /// `CopyDone`: the copy reads the rest of the data.
    pub(super) fn done(&mut self) -> Result<(), Error> {
        if self.ended { Ok(()) } else { self.read(true) }
    }

    /// The error of `CopyFail` with the message of the client.
    pub(super) fn fail(&self, message: &[u8]) -> Error {
        let message = String::from_utf8_lossy(message);
        let error = Error::new(
            SqlState::QUERY_CANCELED,
            format!("COPY from stdin failed: {}", message.trim_end_matches('\0')),
        );
        match self.format {
            Format::Binary if !self.binary_header => error,
            Format::Binary => self.at_line(error),
            // The copy waits for the next line, and `cur_lineno` counts it, except after the end-of-copy marker.
            _ if self.ended => self.at_line(error),
            _ => error.with_context(format!("COPY {}, line {}", self.name, self.line + 1)),
        }
    }

    /// The error with the context of the current line, `CopyFromErrorCallback`.
    fn at_line(&self, error: Error) -> Error {
        error.with_context(format!("COPY {}, line {}", self.name, self.line))
    }

    /// The context of an error in the current line, with the text of the line.
    fn line_context(&self, line: &[u8]) -> String {
        format!("COPY {}, line {}: \"{}\"", self.name, self.line, shown(line))
    }

    /// The context of an error in a row, with the text of its line when the format has lines.
    fn row_context(&self, row: &Row) -> String {
        match &row.text {
            Some(text) => format!("COPY {}, line {}: \"{}\"", self.name, row.line, shown(text)),
            None => format!("COPY {}, line {}", self.name, row.line),
        }
    }

    /// The context of an error in a field, with the text of the field when the format has lines.
    fn column_context(&self, row: &Row, i: usize, field: Option<&[u8]>) -> String {
        let name = self.names.get(i).map_or("", String::as_str);
        let at = format!("COPY {}, line {}, column {name}", self.name, row.line);
        match (field, &row.text) {
            (_, None) => at,
            (Some(field), Some(_)) => format!("{at}: \"{}\"", shown(field)),
            (None, Some(_)) => format!("{at}: null input"),
        }
    }

    /// `NextCopyFrom`: the value of each selected column from the fields of a row, with the input function or the receive function of the type of the column and its type modifier. A field that is the marker of `DEFAULT` has no value. An error has the context of the column and of its field. With `ON_ERROR IGNORE` or `ON_ERROR SET_NULL`, a soft error of the input function skips the row or sets the value to null.
    fn values(&self, row: &Row, reader: &Reader<'_>) -> Result<Made, Error> {
        let mut values = Vec::with_capacity(self.names.len());
        let mut failed = Vec::new();
        for (i, (name, &(ty, typmod))) in self.names.iter().zip(&self.load.types).enumerate() {
            let Some(field) = row.fields.get(i) else {
                return Err(Error::new(
                    SqlState::BAD_COPY_FILE_FORMAT,
                    format!("missing data for column \"{name}\""),
                )
                .with_context(self.row_context(row)));
            };
            if row.defaults.get(i).copied().unwrap_or(false) {
                continue;
            }
            let mut field = field.as_deref();
            if self.format == Format::Csv {
                if field.is_none() && self.load.force_not_null[i] {
                    field = Some(&self.null);
                } else if field == Some(self.null.as_slice()) && self.load.force_null[i] {
                    field = None;
                }
            }
            let context = || self.column_context(row, i, field);
            let hard = |error: Error| error.with_context(context());
            let value = match field {
                None => Ok(Value::Null),
                Some(bytes) if self.format == Format::Binary => {
                    Ok(rupg_func::receive(ty, bytes, typmod, 0, reader).map_err(hard)?)
                }
                Some(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) if self.load.on_error == OnError::Stop => {
                        Ok(rupg_func::input(ty, text, typmod, reader).map_err(hard)?)
                    }
                    Ok(text) => rupg_func::input_soft(ty, text, typmod, reader).map_err(hard)?,
                    // `CopyReadAttributesText` checks the encoding of the fields before it reads their values, so the error has the context of the line.
                    Err(_) => {
                        let error = query::param(ty, 0, Some(bytes), i + 1, reader).err();
                        let error =
                            error.unwrap_or_else(|| Error::internal("a field that is not UTF-8"));
                        return Err(error.with_context(self.row_context(row)));
                    }
                },
            };
            let value = match value {
                Ok(value) if self.load.domains[i] => {
                    match rupg_exec::input_failure(ty, &value, reader).map_err(hard)? {
                        Some(error) => Err(error),
                        None => Ok(value),
                    }
                }
                value => value,
            };
            match (value, self.load.on_error) {
                (Ok(value), _) => values.push(value),
                (Err(error), OnError::Stop) => return Err(hard(error)),
                (Err(_), OnError::Ignore) => {
                    failed.push((i, field.map(<[u8]>::to_vec)));
                    return Ok(Made { values: None, failed });
                }
                (Err(_), OnError::SetNull) => {
                    if self.load.domains[i]
                        && rupg_exec::input_failure(ty, &Value::Null, reader)
                            .map_err(hard)?
                            .is_some()
                    {
                        let domain = rupg_func::type_message_name(ty, reader);
                        return Err(hard(
                            Error::new(
                                SqlState::NOT_NULL_VIOLATION,
                                format!("domain {domain} does not allow null values"),
                            )
                            .with_detail(format!(
                                "ON_ERROR SET_NULL cannot be applied because column \"{name}\" (domain {domain}) does not accept null values."
                            )),
                        ));
                    }
                    failed.push((i, field.map(<[u8]>::to_vec)));
                    values.push(Value::Null);
                }
            }
        }
        Ok(Made { values: Some(values), failed })
    }

    /// The count of `num_errors` for a row with a field that the input function did not accept, the notices of `LOG_VERBOSITY VERBOSE`, and the check of `REJECT_LIMIT`.
    fn count_failed(&mut self, row: &Row, made: &Made) -> Result<(), Error> {
        let Some((at, field)) = made.failed.first() else { return Ok(()) };
        self.load.errors += 1;
        if self.load.verbosity == Verbosity::Verbose {
            let what = if made.values.is_none() { "skipping row" } else { "setting to null" };
            for (i, field) in &made.failed {
                let value = field
                    .as_deref()
                    .map_or("null input".to_string(), |field| format!("\"{}\"", shown(field)));
                let message = format!(
                    "{what} due to data type incompatibility at line {} for column \"{}\": {value}",
                    row.line, self.names[*i]
                );
                self.notices.push(Notice {
                    severity: Severity::Notice,
                    error: Error::new(SqlState::SUCCESSFUL_COMPLETION, message)
                        .with_context(format!("COPY {}", self.name)),
                });
            }
        }
        if made.values.is_none()
            && let Some(limit) = self.load.reject_limit
            && self.load.errors > limit.unsigned_abs()
        {
            return Err(Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!(
                    "skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"
                ),
            )
            .with_context(self.column_context(row, *at, field.as_deref())));
        }
        Ok(())
    }

    /// Reads the data in the buffer. At `eof`, the rest of the buffer is the last line.
    fn read(&mut self, eof: bool) -> Result<(), Error> {
        if self.format == Format::Binary {
            return self.binary(eof);
        }
        loop {
            let read = match self.next_line(eof) {
                Ok(Some(read)) => read,
                Ok(None) => return Ok(()),
                Err(error) => {
                    return Err(error.with_context(format!(
                        "COPY {}, line {}",
                        self.name,
                        self.line + 1
                    )));
                }
            };
            match read {
                Read::Line(line, embedded) => {
                    self.line += 1 + embedded;
                    if self.header > 0 {
                        self.header -= 1;
                        if self.header_match {
                            self.check_header(&line)?;
                        }
                        continue;
                    }
                    let fields = if self.format == Format::Csv {
                        self.csv_fields(&line)
                    } else {
                        self.text_fields(&line)
                    };
                    let Fields { fields, defaults } =
                        fields.map_err(|error| error.with_context(self.line_context(&line)))?;
                    let no_default = defaults
                        .iter()
                        .zip(&self.load.has_default)
                        .position(|(&marker, &has_default)| marker && !has_default);
                    if let Some(at) = no_default {
                        return Err(Error::new(
                            SqlState::BAD_COPY_FILE_FORMAT,
                            "unexpected default marker in COPY data",
                        )
                        .with_detail(format!("Column \"{}\" has no default value.", self.names[at]))
                        .with_context(self.line_context(&line)));
                    }
                    if fields.len() > self.names.len() {
                        return Err(Error::new(
                            SqlState::BAD_COPY_FILE_FORMAT,
                            "extra data after last expected column",
                        )
                        .with_context(self.line_context(&line)));
                    }
                    self.pending.push(Row { line: self.line, text: Some(line), fields, defaults });
                }
                Read::End | Read::Eof => {
                    // `cur_lineno` counts the line of the marker. `NextCopyFromRawFieldsInternal` reads the fields of the header line also when the data ended before it.
                    self.line += 1;
                    if self.header > 0 && self.header_match {
                        self.header = 0;
                        self.check_header(b"")?;
                    }
                    self.ended = matches!(read, Read::End);
                    self.buf.clear();
                    return Ok(());
                }
            }
        }
    }

    /// `CopyReadLineText`: the next line of the buffer, which the copy then drops from the buffer. `None` means that the line is not complete yet.
    fn next_line(&mut self, eof: bool) -> Result<Option<Read>, Error> {
        let csv = self.format == Format::Csv;
        let quote = self.quote;
        let escape = (self.escape != self.quote).then_some(self.escape);
        let bad = |message: &str| Error::new(SqlState::BAD_COPY_FILE_FORMAT, message);
        let not_alone = || bad("end-of-copy marker is not alone on its line");
        let style = || bad("end-of-copy marker does not match previous newline style");
        let carriage = || {
            let (message, hint) = if csv {
                (
                    "unquoted carriage return found in data",
                    "Use quoted CSV field to represent carriage return.",
                )
            } else {
                (
                    "literal carriage return found in data",
                    "Use \"\\r\" to represent carriage return.",
                )
            };
            bad(message).with_hint(hint)
        };
        let newline = || {
            let (message, hint) = if csv {
                ("unquoted newline found in data", "Use quoted CSV field to represent newline.")
            } else {
                ("literal newline found in data", "Use \"\\n\" to represent newline.")
            };
            bad(message).with_hint(hint)
        };
        let buf = &self.buf;
        let (mut in_quote, mut last_was_esc) = (false, false);
        let mut embedded = 0;
        let mut i = 0;
        let (end, next) = loop {
            let Some(&c) = buf.get(i) else {
                if !eof {
                    return Ok(None);
                }
                if buf.is_empty() {
                    return Ok(Some(Read::Eof));
                }
                break (buf.len(), buf.len());
            };
            i += 1;
            if csv {
                if in_quote && Some(c) == escape {
                    last_was_esc = !last_was_esc;
                }
                if c == quote && !last_was_esc {
                    in_quote = !in_quote;
                }
                if Some(c) != escape {
                    last_was_esc = false;
                }
                if in_quote && c == (if self.eol == Eol::Nl { b'\n' } else { b'\r' }) {
                    embedded += 1;
                }
            }
            if c == b'\r' && !in_quote {
                if matches!(self.eol, Eol::Unknown | Eol::CrNl) {
                    if i == buf.len() && !eof {
                        return Ok(None);
                    }
                    if buf.get(i) == Some(&b'\n') {
                        self.eol = Eol::CrNl;
                        break (i - 1, i + 1);
                    }
                    if self.eol == Eol::CrNl {
                        return Err(carriage());
                    }
                    self.eol = Eol::Cr;
                } else if self.eol == Eol::Nl {
                    return Err(carriage());
                }
                break (i - 1, i);
            }
            if c == b'\n' && !in_quote {
                if matches!(self.eol, Eol::Cr | Eol::CrNl) {
                    return Err(newline());
                }
                self.eol = Eol::Nl;
                break (i - 1, i);
            }
            if c == b'\\' && !csv {
                let Some(&c2) = buf.get(i) else {
                    if !eof {
                        return Ok(None);
                    }
                    break (buf.len(), buf.len());
                };
                if c2 != b'.' {
                    // The character after a backslash is never special, so `\\.` is not the marker.
                    i += 1;
                    continue;
                }
                let start = i - 1;
                i += 1;
                if self.eol == Eol::CrNl {
                    if i == buf.len() && !eof {
                        return Ok(None);
                    }
                    let c2 = buf.get(i).copied().unwrap_or(0);
                    i += 1;
                    if c2 == b'\n' {
                        return Err(style());
                    }
                    if c2 != b'\r' {
                        return Err(not_alone());
                    }
                }
                if i == buf.len() && !eof {
                    return Ok(None);
                }
                let c2 = buf.get(i).copied().unwrap_or(0);
                if c2 != b'\r' && c2 != b'\n' {
                    return Err(not_alone());
                }
                if (self.eol == Eol::Cr) != (c2 == b'\r') && self.eol != Eol::Unknown {
                    return Err(style());
                }
                if start > 0 {
                    return Err(not_alone());
                }
                return Ok(Some(Read::End));
            }
        };
        let line = buf[..end].to_vec();
        self.buf.drain(..next);
        Ok(Some(Read::Line(line, embedded)))
    }

    /// The check of `HEADER MATCH` in `NextCopyFromRawFieldsInternal`.
    fn check_header(&self, line: &[u8]) -> Result<(), Error> {
        let fields =
            if self.format == Format::Csv { self.csv_fields(line) } else { self.text_fields(line) };
        let context = || self.line_context(line);
        let bad = |message: String| {
            Error::new(SqlState::BAD_COPY_FILE_FORMAT, message).with_context(context())
        };
        let fields = fields.map_err(|error| error.with_context(context()))?.fields;
        if fields.len() != self.names.len() {
            return Err(bad(format!(
                "wrong number of fields in header line: got {}, expected {}",
                fields.len(),
                self.names.len()
            )));
        }
        for (i, (field, name)) in fields.iter().zip(&self.names).enumerate() {
            match field {
                None => {
                    return Err(bad(format!(
                        "column name mismatch in header line field {}: got null value (\"{}\"), expected \"{name}\"",
                        i + 1,
                        String::from_utf8_lossy(&self.null)
                    )));
                }
                Some(field) if field != name.as_bytes() => {
                    return Err(bad(format!(
                        "column name mismatch in header line field {}: got \"{}\", expected \"{name}\"",
                        i + 1,
                        String::from_utf8_lossy(field)
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// `CopyReadAttributesText`: the fields of a line of the text format, with `None` for the null string, and the marks of the fields that are the marker of `DEFAULT`.
    fn text_fields(&self, line: &[u8]) -> Result<Fields, Error> {
        let mut fields = Vec::new();
        let mut defaults = Vec::new();
        let mut i = 0;
        loop {
            let start = i;
            let mut field = Vec::new();
            let mut last = true;
            while let Some(&c) = line.get(i) {
                i += 1;
                if c == self.delimiter {
                    last = false;
                    break;
                }
                if c != b'\\' {
                    field.push(c);
                    continue;
                }
                let Some(&c) = line.get(i) else { break };
                i += 1;
                match c {
                    b'0'..=b'7' => {
                        let mut value = u32::from(c - b'0');
                        for _ in 0..2 {
                            match line.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        field.push((value & 0o377) as u8);
                    }
                    b'x' => match line.get(i).and_then(|&d| (d as char).to_digit(16)) {
                        Some(high) => {
                            i += 1;
                            let mut value = high;
                            if let Some(low) = line.get(i).and_then(|&d| (d as char).to_digit(16)) {
                                value = value * 16 + low;
                                i += 1;
                            }
                            field.push(value as u8);
                        }
                        None => field.push(b'x'),
                    },
                    b'b' => field.push(8),
                    b'f' => field.push(12),
                    b'n' => field.push(b'\n'),
                    b'r' => field.push(b'\r'),
                    b't' => field.push(b'\t'),
                    b'v' => field.push(11),
                    _ => field.push(c),
                }
            }
            let raw_end = if last { i } else { i - 1 };
            let raw = &line[start..raw_end];
            if self.default.is_some() {
                defaults.push(self.default.as_deref() == Some(raw));
            }
            fields.push(if raw == self.null.as_slice() { None } else { Some(field) });
            if last {
                return Ok(Fields::new(fields, defaults));
            }
        }
    }

    /// `CopyReadAttributesCSV`: the fields of a line of the CSV format, with `None` for the null string outside quotes, and the marks of the fields that are the marker of `DEFAULT` outside quotes.
    fn csv_fields(&self, line: &[u8]) -> Result<Fields, Error> {
        let mut fields = Vec::new();
        let mut defaults = Vec::new();
        let mut i = 0;
        loop {
            let start = i;
            let mut field = Vec::new();
            let mut saw_quote = false;
            let mut last = true;
            'field: while let Some(&c) = line.get(i) {
                i += 1;
                if c == self.delimiter {
                    last = false;
                    break;
                }
                if c != self.quote {
                    field.push(c);
                    continue;
                }
                saw_quote = true;
                loop {
                    let Some(&c) = line.get(i) else {
                        return Err(Error::new(
                            SqlState::BAD_COPY_FILE_FORMAT,
                            "unterminated CSV quoted field",
                        ));
                    };
                    i += 1;
                    if c == self.escape
                        && let Some(&next) = line.get(i)
                        && (next == self.escape || next == self.quote)
                    {
                        field.push(next);
                        i += 1;
                        continue;
                    }
                    if c == self.quote {
                        continue 'field;
                    }
                    field.push(c);
                }
            }
            let raw_end = if last { i } else { i - 1 };
            let raw = &line[start..raw_end];
            if self.default.is_some() {
                defaults.push(!saw_quote && self.default.as_deref() == Some(raw));
            }
            fields.push(if !saw_quote && raw == self.null.as_slice() { None } else { Some(field) });
            if last {
                return Ok(Fields::new(fields, defaults));
            }
        }
    }

    /// `ReceiveCopyBinaryHeader` and the field count of `NextCopyFrom` for the binary format.
    fn binary(&mut self, eof: bool) -> Result<(), Error> {
        let bad = |message: &str| Error::new(SqlState::BAD_COPY_FILE_FORMAT, message);
        if !self.binary_header {
            let buf = &self.buf;
            let word = |at: usize| {
                buf.get(at..at + 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            };
            if buf.len() < BINARY_SIGNATURE.len()
                || &buf[..BINARY_SIGNATURE.len()] != BINARY_SIGNATURE
            {
                if buf.len() < BINARY_SIGNATURE.len() && !eof {
                    return Ok(());
                }
                return Err(bad("COPY file signature not recognized"));
            }
            let Some(flags) = word(11) else {
                return if eof {
                    Err(bad("invalid COPY file header (missing flags)"))
                } else {
                    Ok(())
                };
            };
            if flags & (1 << 16) != 0 {
                return Err(bad("invalid COPY file header (WITH OIDS)"));
            }
            if (flags & !(1 << 16)) >> 16 != 0 {
                return Err(bad("unrecognized critical flags in COPY file header"));
            }
            let length = match word(15) {
                Some(length) if length >= 0 => length,
                Some(_) => return Err(bad("invalid COPY file header (missing length)")),
                None if eof => return Err(bad("invalid COPY file header (missing length)")),
                None => return Ok(()),
            };
            let end = 19 + usize::try_from(length).unwrap_or(usize::MAX - 19);
            if buf.len() < end {
                return if eof {
                    Err(bad("invalid COPY file header (wrong length)"))
                } else {
                    Ok(())
                };
            }
            self.buf.drain(..end);
            self.binary_header = true;
            // `NextCopyFrom` counts the tuple before it reads the field count.
            self.line = 1;
        }
        loop {
            let Some(count) = self.buf.get(..2).map(|b| i16::from_be_bytes([b[0], b[1]])) else {
                // A field count that is not complete at the end of the data is the end of the data.
                return Ok(());
            };
            if count == -1 {
                break;
            }
            let expected = self.names.len();
            if usize::try_from(count).ok() != Some(expected) {
                return Err(
                    self.at_line(bad(&format!("row field count is {count}, expected {expected}")))
                );
            }
            let mut at = 2;
            let mut fields = Vec::with_capacity(expected);
            for name in &self.names {
                let column = |error: Error| {
                    error.with_context(format!(
                        "COPY {}, line {}, column {name}",
                        self.name, self.line
                    ))
                };
                let size =
                    self.buf.get(at..at + 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                let Some(size) = size else {
                    return if eof {
                        Err(column(bad("unexpected EOF in COPY data")))
                    } else {
                        Ok(())
                    };
                };
                at += 4;
                if size == -1 {
                    fields.push(None);
                    continue;
                }
                let Ok(size) = usize::try_from(size) else {
                    return Err(column(bad("invalid field size")));
                };
                let Some(bytes) = self.buf.get(at..at + size) else {
                    return if eof {
                        Err(column(bad("unexpected EOF in COPY data")))
                    } else {
                        Ok(())
                    };
                };
                fields.push(Some(bytes.to_vec()));
                at += size;
            }
            self.buf.drain(..at);
            self.pending.push(Row { line: self.line, text: None, fields, defaults: Vec::new() });
            self.line += 1;
        }
        self.ended = true;
        let rest = self.buf.split_off(2);
        self.buf.clear();
        self.data(&rest)
    }
}

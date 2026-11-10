//! `COPY ... FROM STDIN` and `COPY ... FROM` a file of the server, as `DoCopy`, `BeginCopyFrom`, `CopyFrom` and `copyfromparse.c` run it. The copy reads the fields of each line, and `NextCopyFrom` makes the values of the columns with the input functions or the receive functions of their types. The other columns get their defaults from a plan of `INSERT`, which also checks the constraints. The copy keeps up to `MAX_BUFFERED_TUPLES` rows and then writes them in the heap, as `CopyMultiInsertInfoFlush` does. The triggers `BEFORE ... FOR EACH STATEMENT` fire before the first row, and a table with triggers `BEFORE ... FOR EACH ROW` gets one row at a time, as `CIM_SINGLE` writes them, so each trigger fires for its row after the condition `WHERE` and before the checks. The triggers `AFTER` and the triggers of the foreign keys fire at the end of the copy.

use std::path::Path;
use std::rc::Rc;

use rupg_analyze::{CopyTarget, Derived, Params};
use rupg_catalog::{Catalog, TRIGGER_INSERT};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_func::Progress;
use rupg_pgcatalog::builtin;
use rupg_sql::Severity;
use rupg_sql::nodes::{CopyStmt, RangeVar};
use rupg_types::Value;
use rupg_wire::{CommandTag, OutBuf};

use super::copy::{BINARY_SIGNATURE, ForceQuote, Format, OnError, Verbosity, options};
use super::foreign::Batch;
use super::route::Routing;
use super::trigger::Triggers;
use super::{Connection, Done};
use crate::heap::{BUFFERED_BYTES, Changed, View, copy_batch_full};
use crate::query::{self, Reader};
use crate::utility::Notice;

mod cells;

use cells::Parse;

/// `MAX_COPY_DATA_DISPLAY`: the number of bytes of a line that the context of an error shows.
const MAX_DISPLAY: usize = 100;

/// `MAX_BUFFERED_TUPLES`: the number of rows that the copy keeps before it writes them in the heap.
const MAX_BUFFERED: usize = 1000;

/// The rows that a bulk load reads before it makes their cells, so that the threads of the server each get a part of the rows.
const LOAD_BATCH: usize = 8192;

/// The parameters of the progress of a copy, `PROGRESS_COPY_*`, and the values of `PROGRESS_COPY_COMMAND_FROM`, `PROGRESS_COPY_TYPE_FILE` and `PROGRESS_COPY_TYPE_PIPE`.
const BYTES_PROCESSED: usize = 0;
const BYTES_TOTAL: usize = 1;
const TUPLES_PROCESSED: usize = 2;
const TUPLES_EXCLUDED: usize = 3;
const COMMAND: usize = 4;
const TYPE: usize = 5;
const TUPLES_SKIPPED: usize = 6;
const COMMAND_FROM: i64 = 1;
const TYPE_FILE: i64 = 1;
const TYPE_PIPE: i64 = 3;

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
    fields: Spans,
    /// `defaults`: true for a field that is the marker of `DEFAULT`. It is empty for a row with no marker.
    defaults: Vec<bool>,
    /// True when the fields are still in `text`: a bulk load reads the fields of the line on the threads of the server, and [`CopyIn::split_row`] gives them before the row goes through the plan.
    unsplit: bool,
}

/// The bytes of `bytes` up to the first byte for which `is` is true, or all the bytes. After the first 16 bytes, which go one at a time for a short field, the bytes go 16 at a time with no early exit, so that the compiler can use vector instructions.
fn find(bytes: &[u8], is: impl Fn(u8) -> bool) -> usize {
    let head = bytes.len().min(16);
    if let Some(at) = bytes[..head].iter().position(|&b| is(b)) {
        return at;
    }
    let mut at = head;
    for chunk in bytes[head..].as_chunks::<16>().0 {
        if chunk.iter().fold(false, |any, &b| any | is(b)) {
            break;
        }
        at += 16;
    }
    at + bytes[at..].iter().position(|&b| is(b)).unwrap_or(bytes.len() - at)
}

/// The fields of a line in one buffer: the bytes of the fields one after the other, and the place of each field in the bytes, with `None` for a null value. A row of a load has one buffer and not one for each field.
#[derive(Debug, Default)]
struct Spans {
    bytes: Vec<u8>,
    places: Vec<Option<(usize, usize)>>,
}

impl Spans {
    fn with_capacity(fields: usize, bytes: usize) -> Spans {
        Spans { bytes: Vec::with_capacity(bytes), places: Vec::with_capacity(fields) }
    }

    fn len(&self) -> usize {
        self.places.len()
    }

    /// The field `i`, with `None` for a null value, or `None` when the line has no field `i`.
    fn get(&self, i: usize) -> Option<Option<&[u8]>> {
        let place = *self.places.get(i)?;
        Some(place.map(|(start, end)| &self.bytes[start..end]))
    }

    fn iter(&self) -> impl Iterator<Item = Option<&[u8]>> {
        self.places.iter().map(|place| place.map(|(start, end)| &self.bytes[start..end]))
    }

    /// Adds a field.
    fn push(&mut self, field: Option<&[u8]>) {
        let start = self.bytes.len();
        if let Some(field) = field {
            self.bytes.extend_from_slice(field);
        }
        self.end(start, field.is_none());
    }

    /// Ends the field whose bytes start at `start`, as a null value when `null`.
    fn end(&mut self, start: usize, null: bool) {
        if null {
            self.bytes.truncate(start);
            self.places.push(None);
        } else {
            self.places.push(Some((start, self.bytes.len())));
        }
    }
}

/// The fields of a line of the text format or the CSV format.
#[derive(Debug)]
struct Fields {
    /// The fields, with `None` for a null value.
    fields: Spans,
    /// The marks of the fields that are the marker of `DEFAULT`, or nothing for a line with no marker.
    defaults: Vec<bool>,
}

impl Fields {
    /// The fields of a line. The marks become empty when no field is a marker, so that the copy uses the plan with no defaults.
    fn new(fields: Spans, mut defaults: Vec<bool>) -> Fields {
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
    /// The number of rows that the condition `WHERE` left out.
    excluded: u64,
    /// The rows that the copy checked and did not write in the heap yet, and the line of each row.
    rows: Vec<Vec<Value>>,
    lines: Vec<u64>,
    /// The bytes of the line of each row of `rows`, which `CopyMultiInsertInfoStore` counts for the batches of `heap_multi_insert`.
    lens: Vec<usize>,
    /// The rows and the bytes of the lines of the batch that the copy fills, which goes in the heap when `CopyMultiInsertInfoIsFull` is true.
    batch: (usize, usize),
    /// The length of the version and of the line of each row of the cells of a bulk load, and the first of them that comes after `rows`, for the batch of `rows`.
    ahead: (Vec<(usize, usize)>, usize),
    /// The number of rows that the copy wrote.
    count: u64,
    /// The rows for the triggers of the foreign keys.
    batches: Vec<Batch>,
    /// The triggers `INSERT` of the table.
    triggers: Triggers,
    /// True after the triggers `BEFORE ... FOR EACH STATEMENT` fired.
    begun: bool,
    /// The plans that [`Plan::start`] checked, so that the checks of the plan run once for the statement and not for each row, each with [`Plan::passes_params`].
    started: Vec<(Rc<Plan>, bool)>,
    /// The rows that the copy wrote, for the triggers `AFTER ... FOR EACH ROW` and the transition tables.
    inserted: Vec<Vec<Value>>,
    /// The routing of the rows of a partitioned table into its leaves, which keep the rows instead of `rows`.
    routing: Option<Routing>,
    /// The input of each column of a bulk load, which writes the rows into the encoded columns of the table, or `Some(None)` for a copy that takes each row as values. `None` until the copy read its first row.
    bulk: Option<Option<Vec<Parse>>>,
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
    /// The progress of the copy, which `pg_stat_progress_copy` shows.
    progress: Progress,
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
        self.server_file_privilege(true)?;
        let name = stmt.filename.as_deref().unwrap_or_default();
        let mut bytes = Vec::new();
        let mut copy = self.copy_from(stmt, Begin::File(name, &mut bytes))?;
        let read = self.copy_data(&mut copy, &bytes);
        let count = read.and_then(|()| self.copy_done(&mut copy));
        self.report_progress(None);
        copy.notices.extend(self.trigger_notices());
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

    /// The checks of `COPY FREEZE` in `CopyFrom`: the table is not partitioned or foreign, and the current subtransaction created it or gave it a new file, as `TRUNCATE` does. The rows of rupg have no frozen state, so the option changes nothing else.
    fn check_freeze(&self, catalog: &Catalog, relid: u32, kind: u8) -> Result<(), Error> {
        let unsupported = |what: &str| {
            Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot perform COPY FREEZE on a {what}"),
            ))
        };
        match kind {
            b'p' => return unsupported("partitioned table"),
            b'f' => return unsupported("foreign table"),
            _ => {}
        }
        if !self.new_in_subtransaction(catalog, relid) {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction",
            ));
        }
        Ok(())
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
        if !rupg_func::insert_allowed(target.relid, self.current_role(), &attnums, &reader)? {
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
        rupg_func::copy_from_row_security(target.relid, &reader)?;
        if self.settings.borrow().get_of(crate::guc::known!("transaction_read_only")).as_deref()
            == Some("on")
        {
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
                Some(plan.unchecked())
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
        // `BeginCopyFrom` plans the defaults of the columns before `CopyInResponse`, so that an error in a default comes first. With `DEFAULT`, it plans the default of each column. The kinds of relation that cannot take a copy fail after `CopyInResponse`.
        // A foreign table without a handler fails in `CheckValidResultRel`, after `CopyInResponse` and the checks of `FREEZE`.
        let table = rel.is_some() && matches!(target.kind, b'r' | b'p' | b'f');
        let planned = table.then(|| insert_plan(&target, rv, &[], &reader));
        let (plan, deferred) = match planned {
            Some(Err(error)) if target.kind == b'f' => (None, Some(error)),
            planned => (planned.transpose()?.map(Rc::new), None),
        };
        let source =
            (table && options.default.is_some()).then(|| (target.clone(), RangeVar::clone(rv)));
        let mut defaulted = Vec::new();
        if let Some((target, rv)) = &source {
            let all = vec![true; target.selected.len()];
            let plan = Rc::new(insert_plan(target, rv, &all, &reader)?);
            defaulted.push((all, plan));
        }
        let binary = options.format == Format::Binary;
        let mut progress = Progress { command: "COPY", relid: target.relid, ..Progress::default() };
        progress.params[COMMAND] = COMMAND_FROM;
        match begin {
            Begin::Client(out) => {
                progress.params[TYPE] = TYPE_PIPE;
                out.copy_in_response(
                    i8::from(binary),
                    &vec![i16::from(binary); target.selected.len()],
                );
            }
            Begin::File(name, bytes) => {
                *bytes = self.read_server_file(name)?;
                progress.params[TYPE] = TYPE_FILE;
                progress.params[BYTES_TOTAL] = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
            }
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
            self.check_freeze(&catalog, target.relid, target.kind)?;
        }
        if let Some(error) = deferred {
            return Err(error);
        }
        let types: Vec<(u32, i32)> =
            target.selected.iter().filter_map(|&i| types.get(i).copied()).collect();
        let domains = types
            .iter()
            .map(|&(ty, _)| {
                let element = builtin::type_by_oid(ty).map_or(0, |row| row.elem);
                builtin::domain_by_oid(ty).is_some() || builtin::domain_by_oid(element).is_some()
            })
            .collect();
        self.start_triggers();
        let mut triggers = match plan.as_deref().and_then(Plan::write) {
            Some(info) => self.write_triggers(info)?,
            None => Triggers::default(),
        };
        let modified =
            plan.as_deref().and_then(Plan::write).map_or(&[][..], |w| w.modified.as_slice());
        let routing = (target.kind == b'p').then(|| Routing::of(TRIGGER_INSERT, modified));
        if routing.is_some() {
            triggers.retain_statement();
        }
        let load = Load {
            plan,
            source,
            defaulted,
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
            excluded: 0,
            rows: Vec::new(),
            lines: Vec::new(),
            lens: Vec::new(),
            batch: (0, 0),
            ahead: (Vec::new(), 0),
            count: 0,
            batches: Vec::new(),
            triggers,
            begun: false,
            started: Vec::new(),
            inserted: Vec::new(),
            routing,
            bulk: None,
        };
        let names = target.selected.iter().map(|&i| target.columns[i].clone()).collect();
        self.report_progress(Some(progress));
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
            progress,
        })
    }

    /// `pgstat_progress_update_param` of a copy: the bytes that it read, the rows that it wrote, the rows that the condition `WHERE` left out and the rows that `ON_ERROR IGNORE` skipped.
    fn report_copy(&self, copy: &mut CopyIn) {
        let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        let params = &mut copy.progress.params;
        params[TUPLES_PROCESSED] = count(copy.load.count);
        params[TUPLES_EXCLUDED] = count(copy.load.excluded);
        if copy.load.on_error == OnError::Ignore {
            params[TUPLES_SKIPPED] = count(copy.load.errors);
        }
        self.report_progress(Some(copy.progress));
    }

    /// `CopyData` of a copy: the copy reads the lines that are complete and makes the values of their rows. The rows before a line with an error come first, as `CopyFrom` reads one row at a time.
    pub(super) fn copy_data(&self, copy: &mut CopyIn, bytes: &[u8]) -> Result<(), Error> {
        let read = copy.data(bytes);
        let added = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        copy.progress.params[BYTES_PROCESSED] =
            copy.progress.params[BYTES_PROCESSED].saturating_add(added);
        // The first row starts the plan of the copy, which tells if the copy is a bulk load. A bulk load then waits for a batch of rows for the threads, so that its first rows go in as one batch of `heap_multi_insert`. A line with an error comes after the rows before it.
        if read.is_ok()
            && let Err(error) = self.begin_load(copy)
        {
            self.report_copy(copy);
            return Err(error);
        }
        if read.is_ok()
            && matches!(copy.load.bulk, Some(Some(_)))
            && copy.pending.len() < LOAD_BATCH
        {
            self.report_copy(copy);
            return Ok(());
        }
        let loaded = self.load_rows(copy, read.is_err());
        self.report_copy(copy);
        loaded?;
        read
    }

    /// `CopyDone` of a copy: the copy reads the rest of the data, writes the rows that remain, and fires the triggers of the foreign keys. It gives the number of rows that the copy wrote.
    pub(super) fn copy_done(&self, copy: &mut CopyIn) -> Result<u64, Error> {
        let read = copy.done();
        self.load_rows(copy, true)?;
        read?;
        self.flush_rows(copy, None)?;
        self.report_copy(copy);
        if copy.load.count > 0 {
            self.command.set(self.command.get() + 1);
        }
        // The notice of the rows that `ON_ERROR` skipped comes before the triggers `AFTER`, as `CopyFrom` gives it.
        copy.notices.extend(self.trigger_notices());
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
        let inserted = std::mem::take(&mut copy.load.inserted);
        if !copy.load.triggers.is_empty()
            && let Some(info) = copy.load.plan.as_deref().and_then(Plan::write)
        {
            self.after_write(info, &copy.load.triggers, &[], &inserted)?;
        }
        let batches = std::mem::take(&mut copy.load.batches);
        if !batches.is_empty() {
            self.fire(batches)?;
        }
        self.recheck_unique(0)?;
        self.fire_events(0)?;
        copy.notices.extend(self.trigger_notices());
        Ok(copy.load.count)
    }

    /// `NextCopyFrom` and the checks of `CopyFrom` for the lines that the copy read: the values of the columns, the defaults of the other columns, the condition `WHERE` and the constraints of the table. A bulk load keeps the rows after its last full batch for the next lines, and `all` loads them.
    fn load_rows(&self, copy: &mut CopyIn, all: bool) -> Result<(), Error> {
        self.begin_load(copy)?;
        if copy.pending.is_empty() || self.load_cells(copy, all)? || copy.pending.is_empty() {
            return Ok(());
        }
        self.load_values(copy)
    }

    /// The triggers `BEFORE ... FOR EACH STATEMENT` of the copy fire before its first row. Then the first row starts the plan of the copy, with the context of its line for an error, and the copy is a bulk load or a copy of values. A first row with a marker of `DEFAULT` has its own plan, and its rows go in as values before the copy chooses.
    fn begin_load(&self, copy: &mut CopyIn) -> Result<(), Error> {
        if !copy.load.begun {
            copy.load.begun = true;
            if let Some(info) = copy.load.plan.as_deref().and_then(Plan::write) {
                self.before_statement(&copy.load.triggers, info)?;
            }
        }
        if copy.load.bulk.is_some() {
            return Ok(());
        }
        let Some(row) = copy.pending.first().filter(|row| row.defaults.is_empty()) else {
            return Ok(());
        };
        let Some(plan) = copy.load.plan.clone() else { return Ok(()) };
        if !copy.load.started.iter().any(|(p, _)| Rc::ptr_eq(p, &plan)) {
            let reader = self.reader();
            plan.start(&reader).map_err(|e| e.with_context(copy.row_context(row)))?;
            drop(reader);
            copy.load.started.push((Rc::clone(&plan), plan.passes_params()));
        }
        copy.load.bulk = Some(self.bulk_parse(copy));
        Ok(())
    }

    /// The rows of `pending` go through the plan of the copy as values, as `NextCopyFrom` and `ExecInsert` take them.
    fn load_values(&self, copy: &mut CopyIn) -> Result<(), Error> {
        let pending = std::mem::take(&mut copy.pending);
        if copy.load.plan.is_none() {
            return Err(copy.at_line(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM with rows is not supported yet",
            )));
        }
        let catalog = self.catalog();
        let reader = self.reader();
        // The routing leaves `copy.load` while the rows of `pending` go to the leaves, because the context of an error borrows `copy`.
        let mut routing = copy.load.routing.take();
        let mut excluded = 0;
        for row in pending {
            let row = copy.split_row(row)?;
            let made = copy.values(&row, &reader)?;
            copy.count_failed(&row, &made)?;
            let Some(params) = made.values else { continue };
            let plan = copy.load.plan_for(&row.defaults, &reader)?;
            let width = plan.write().map_or(0, |info| info.width);
            let passes = match copy.load.started.iter().find(|(p, _)| Rc::ptr_eq(p, &plan)) {
                Some(&(_, passes)) => passes,
                None => {
                    plan.start(&reader).map_err(|e| e.with_context(copy.row_context(&row)))?;
                    let passes = plan.passes_params();
                    copy.load.started.push((Rc::clone(&plan), passes));
                    passes
                }
            };
            let line = |error: Error| error.with_context(copy.row_context(&row));
            let rows = if passes {
                vec![params]
            } else {
                plan.run_started(&params, &reader).map_err(line)?
            };
            let planned = rows.len();
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
            excluded += (planned - rows.len()) as u64;
            // `CIM_SINGLE`: with triggers `BEFORE ... FOR EACH ROW`, the triggers come before the checks, and the row goes in before the next row.
            let single = plan.write().filter(|info| info.recheck.is_some());
            let said = self.trigger_warnings.borrow().len();
            let rows = match single {
                Some(info) => self.before_rows(&plan, info, (&copy.load.triggers, None), rows),
                None => plan.check(rows, &reader),
            };
            // The messages of the triggers have the line of the copy in their context, as `CopyFromErrorCallback` gives it.
            if single.is_some() {
                let mut queue = self.trigger_warnings.borrow_mut();
                let len = queue.len();
                let raised = queue.split_off(said.min(len));
                queue.extend(raised.into_iter().map(|w| w.with_context(copy.row_context(&row))));
            }
            let rows = rows.map_err(line)?;
            if let Some(leaves) = &mut routing {
                // `ExecFindPartition`, the triggers `BEFORE ... FOR EACH ROW` and the checks of the leaf, with the line of the row in the context of an error and of the messages of the triggers.
                let table = plan.write().map_or(0, |info| info.table);
                let (mut single, mut inserted) = (false, Vec::new());
                for values in rows {
                    let said = self.trigger_warnings.borrow().len();
                    let fired = leaves.route_fired(self, (table, values), &catalog, &reader);
                    {
                        let mut queue = self.trigger_warnings.borrow_mut();
                        let len = queue.len();
                        let raised = queue.split_off(said.min(len));
                        queue.extend(
                            raised.into_iter().map(|w| w.with_context(copy.row_context(&row))),
                        );
                    }
                    let Some((at, checked)) = fired.map_err(line)? else { continue };
                    if copy.load.triggers.after_rows() {
                        inserted.push(leaves.to_root(at, &checked, width));
                    }
                    // `CopyFrom` writes a row of a leaf with triggers `BEFORE ... FOR EACH ROW` before the next row.
                    single |= leaves.fires_before(at);
                    leaves.push(at, checked, row.line);
                }
                copy.load.inserted.extend(inserted);
                if single {
                    let context = copy.row_context(&row);
                    copy.load.routing = routing.take();
                    self.flush_rows(copy, Some(context))?;
                    routing = copy.load.routing.take();
                }
                continue;
            }
            // A row of `CIM_SINGLE` goes in alone, as `heap_insert` with the `BulkInsertState` of the copy puts it, and fills its batch.
            let bytes = match single {
                Some(_) => BUFFERED_BYTES,
                None => row.text.as_ref().map_or(0, Vec::len),
            };
            // `CopyMultiInsertInfoIsFull`: a full batch goes in the heap before the next row.
            let mut full = false;
            for values in rows {
                full |= copy_batch_full(&mut copy.load.batch, bytes);
                copy.load.lens.push(bytes);
                copy.load.lines.push(row.line);
                copy.load.rows.push(values);
            }
            if full {
                let context = single.map(|_| copy.row_context(&row));
                self.flush_rows(copy, context)?;
            }
        }
        drop(reader);
        copy.load.routing = routing;
        copy.load.excluded += excluded;
        if copy.load.bulk.is_none() {
            copy.load.bulk = Some(self.bulk_parse(copy));
        }
        if copy.load.rows.len() + copy.load.routing.as_ref().map_or(0, Routing::pending)
            >= MAX_BUFFERED
        {
            self.flush_rows(copy, None)?;
        }
        Ok(())
    }

    /// `CopyMultiInsertBufferFlush`: the rows that the copy kept go in the heap. A duplicate key has the context of the line of its row, or the context `context` of the one row of `CIM_SINGLE`.
    fn flush_rows(&self, copy: &mut CopyIn, context: Option<String>) -> Result<(), Error> {
        let load = &mut copy.load;
        let pending = load.routing.as_ref().map_or(load.rows.len(), Routing::pending);
        if pending == 0 {
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
        // `CopyMultiInsertInfoFlush` of a partitioned table: each leaf writes its rows.
        if let Some(routing) = &mut load.routing {
            let batches =
                self.insert_leaves(routing, &view, Some((&copy.name, context.as_deref())))?;
            load.batches.extend(batches);
            load.count += pending as u64;
            return Ok(());
        }
        let rows = std::mem::take(&mut load.rows);
        let lines = std::mem::take(&mut load.lines);
        let lens = std::mem::take(&mut load.lens);
        let (sized, after) = &load.ahead;
        let ahead = sized.get(*after..).unwrap_or_default().to_vec();
        let use_fsm = self.copy_uses_fsm(info.table);
        let inserted = rows.len();
        let count = inserted as u64;
        let new_rows = if info.foreign.is_empty() { Vec::new() } else { rows.clone() };
        if load.triggers.after_rows() {
            load.inserted.extend(rows.iter().cloned());
        }
        self.heap_layout(info.table);
        if info.unique.is_empty() {
            heap.insert_copy(
                info.table,
                &view,
                rows,
                (&lens, &ahead),
                (&[], &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                (&[], use_fsm),
            )?;
        } else {
            let mut kept = Vec::new();
            loop {
                match heap.insert_copy(
                    info.table,
                    &view,
                    rows.clone(),
                    (&lens, &ahead),
                    (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    (&kept, use_fsm),
                )? {
                    Changed::Done(_) => break,
                    Changed::Duplicate { index, key } => {
                        let derive = |d: &Derived, r: &[Value]| self.index_key(d, r);
                        let line = duplicate_line((info, index), &key, &rows, &lines, &derive);
                        let conflict = Changed::Duplicate { index, key };
                        let error = match self.key_conflict(info, &view, conflict) {
                            Ok(_) => Error::internal("a duplicate key with no error"),
                            Err(error) => error,
                        };
                        let context =
                            context.unwrap_or_else(|| format!("COPY {}, line {line}", copy.name));
                        return Err(error.with_context(context));
                    }
                    conflict => kept = self.key_conflict(info, &view, conflict)?,
                }
            }
        }
        load.count += count;
        self.count_inserts(info.table, inserted);
        if !new_rows.is_empty() {
            load.batches.push(Batch::new(info, &info.foreign, view.command, new_rows, Vec::new()));
        }
        Ok(())
    }
}

impl Connection {
    /// False when the transaction created the table `relid` or gave it a new file, as `CopyFrom` then sets `TABLE_INSERT_SKIP_FSM`: the rows of the copy do not use the free space map, and the copy does not put the free pages of an extension into it.
    fn copy_uses_fsm(&self, relid: u32) -> bool {
        let file = self.catalog().relation(relid).map(|r| r.file);
        self.store.committed(self.database_oid).relation(relid).map(|r| r.file) == file
    }

    /// The input of each column for a bulk load of the copy, after its first rows started the plan of the copy: a copy of text or CSV into an ordinary table, with no condition `WHERE`, no triggers, no unique key or foreign key, no policy, no domain, no generated column, `ON_ERROR STOP`, no `FORCE_NULL` or `FORCE_NOT_NULL`, a plan that takes the values of the fields as its row, and a type with a fast input for each column.
    fn bulk_parse(&self, copy: &CopyIn) -> Option<Vec<Parse>> {
        let load = &copy.load;
        let plan = load.plan.as_ref()?;
        let info = plan.write()?;
        let passes = load
            .started
            .iter()
            .find(|(p, _)| Rc::ptr_eq(p, plan))
            .is_some_and(|&(_, passes)| passes);
        let simple = passes
            && copy.format != Format::Binary
            && load.on_error == OnError::Stop
            && load.filter.is_none()
            && load.routing.is_none()
            && load.triggers.is_empty()
            && !load.domains.contains(&true)
            && !load.force_null.contains(&true)
            && !load.force_not_null.contains(&true)
            && info.unique.is_empty()
            && info.foreign.is_empty()
            && info.policy_for(rupg_analyze::WriteKind::Insert).is_none()
            && load.types.len() == info.width;
        let catalog = self.catalog();
        let relation = catalog.relation(info.table)?;
        let plain = relation.kind == rupg_catalog::RelKind::Table
            && relation.partition_of.is_none()
            && relation.columns.iter().all(|column| column.generated == 0);
        if !simple || !plain {
            return None;
        }
        load.types.iter().map(|&(ty, typmod)| Parse::of(ty, typmod)).collect()
    }

    /// The rows of `pending` of a bulk load go in as cells, up to the end of the last full batch of the copy, and the rows after it wait for the next lines unless `all`, as `heap_multi_insert` places the rows of a batch together. A row that is longer than `TOAST_TUPLE_THRESHOLD` goes in as values for the toaster, and the length of its version after the toaster is in the batch of the cells. The result is true when the rows of `pending` wait for the next lines.
    fn load_cells(&self, copy: &mut CopyIn, all: bool) -> Result<bool, Error> {
        if !matches!(copy.load.bulk, Some(Some(_))) {
            return Ok(false);
        }
        let Some(table) = copy.load.plan.as_deref().and_then(Plan::write).map(|info| info.table)
        else {
            return Ok(false);
        };
        let lines: Vec<usize> =
            copy.pending.iter().map(|row| row.text.as_ref().map_or(0, Vec::len)).collect();
        let mut probe = copy.load.batch;
        let whole = (0..lines.len()).filter(|&at| copy_batch_full(&mut probe, lines[at])).last();
        let cut = if all { lines.len() } else { whole.map_or(0, |at| at + 1) };
        if cut == 0 {
            return Ok(true);
        }
        let tail = copy.pending.split_off(cut);
        let store = &self.store;
        let cells = match &copy.load.bulk {
            Some(Some(parse)) => cells::read(&copy.pending, parse, &copy.csv(), store.tasks()),
            _ => None,
        };
        let heap = store.heap();
        let key = (self.database_oid, table);
        self.heap_layout(table);
        let Some((cells, sizes)) =
            cells.and_then(|cells| heap.cell_sizes(key, &cells).map(|sizes| (cells, sizes)))
        else {
            copy.pending.extend(tail);
            return Ok(false);
        };
        // The length of a long row after the toaster.
        let reader = self.reader();
        let long: Vec<(usize, Option<Vec<Value>>)> = (0..sizes.len())
            .filter(|&at| sizes[at].1)
            .map(|at| (at, copy.pending_values(at, &reader)))
            .collect();
        drop(reader);
        let mut sized: Vec<(usize, usize)> =
            sizes.iter().zip(&lines).map(|(&(len, _), &line)| (len, line)).collect();
        for (at, values) in long {
            if let Some(len) = values.and_then(|values| heap.toasted_len(key, &values)) {
                sized[at].0 = len;
            }
        }
        let use_fsm = self.copy_uses_fsm(table);
        let mut rows = std::mem::take(&mut copy.pending).into_iter();
        let mut from = 0;
        let mut whole = true;
        copy.load.ahead = (sized, 0);
        while from < cells.rows {
            // The rows that went in as values come first, in the batch of the rows after them.
            copy.load.ahead.1 = from;
            self.flush_rows(copy, None)?;
            if self.xid.get() == 0 {
                self.xid.set(heap.begin(self.pid));
            }
            let reader = self.reader();
            let Some(view) = reader.view() else {
                return Err(Error::internal("the reader of the session has no view of the rows"));
            };
            let view = View { xid: self.xid.get(), ..view.clone() };
            drop(reader);
            self.heap_layout(table);
            let start = (from, &copy.load.ahead.0[..]);
            let Some(count) = heap.insert_cells(table, &view, &cells, start, use_fsm)? else {
                whole = false;
                break;
            };
            copy.load.count += count;
            let count = count as usize;
            self.count_inserts(table, count);
            for &line in &lines[from..from + count] {
                copy_batch_full(&mut copy.load.batch, line);
            }
            from += count;
            if count > 0 {
                rows.nth(count - 1);
            }
            if from < cells.rows {
                copy.pending.extend(rows.next());
                copy.load.ahead.1 = from + 1;
                self.load_values(copy)?;
                from += 1;
            }
        }
        copy.load.ahead = (Vec::new(), 0);
        copy.pending = rows.collect();
        copy.pending.extend(tail);
        Ok(whole)
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
    // `DoCopy` checked the privileges of the copy.
    Ok(query::plan(&node, reader, &params)?.fold(None, reader)?.unchecked())
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

/// The line of the row whose key is a duplicate. The first row with the key can conflict only with a row that was in the table, and a later row with the key conflicts with the first row. `derive` gives the key of a row in an index with an expression or a predicate.
pub(super) fn duplicate_line(
    (info, index): (&rupg_analyze::Write, usize),
    key: &[Value],
    rows: &[Vec<Value>],
    lines: &[u64],
    derive: crate::heap::Derive<'_>,
) -> u64 {
    let Some(unique) = info.unique.get(index) else { return lines.last().copied().unwrap_or(0) };
    let mut matches = rows.iter().enumerate().filter(|(_, row)| {
        let derived = match &unique.derived {
            Some(derived) => match derive(derived, row) {
                Ok(Some(values)) => Some(values),
                _ => return false,
            },
            None => None,
        };
        let values = derived.as_deref().unwrap_or(row.as_slice());
        unique.columns.iter().zip(key).all(|(&at, value)| values.get(at) == Some(value))
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
            let mut field = field;
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
                        let error = Error::new(
                            SqlState::NOT_NULL_VIOLATION,
                            format!("domain {domain} does not allow null values"),
                        )
                        .with_detail(format!(
                            "ON_ERROR SET_NULL cannot be applied because column \"{name}\" (domain {domain}) does not accept null values."
                        ));
                        return Err(hard(rupg_func::with_datatype(error, ty, reader)));
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
        // The lines leave the buffer once, after the copy read them.
        let mut at = 0;
        let read = self.read_lines(eof, &mut at);
        let at = at.min(self.buf.len());
        self.buf.drain(..at);
        read
    }

    /// The lines of [`CopyIn::read`] from the place `at` of the buffer. `at` goes to the end of the last line that the copy read.
    fn read_lines(&mut self, eof: bool, at: &mut usize) -> Result<(), Error> {
        loop {
            let read = match self.next_line(eof, at) {
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
                    // A bulk load of the CSV format reads the fields on the threads of the server.
                    if matches!(self.load.bulk, Some(Some(_)))
                        && self.format == Format::Csv
                        && self.default.is_none()
                    {
                        self.pending.push(Row {
                            line: self.line,
                            text: Some(line),
                            fields: Spans::default(),
                            defaults: Vec::new(),
                            unsplit: true,
                        });
                        continue;
                    }
                    let Fields { fields, defaults } = self
                        .split(&line)
                        .map_err(|error| error.with_context(self.line_context(&line)))?;
                    self.pending.push(Row {
                        line: self.line,
                        text: Some(line),
                        fields,
                        defaults,
                        unsplit: false,
                    });
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

    /// `CopyReadLineText`: the next line of the buffer from the place `at`, which then goes to the start of the line after it. `None` means that the line is not complete yet.
    fn next_line(&mut self, eof: bool, at: &mut usize) -> Result<Option<Read>, Error> {
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
        let buf = &self.buf[*at..];
        let (mut in_quote, mut last_was_esc) = (false, false);
        let mut embedded = 0;
        let mut i = 0;
        // Only a quote, an escape, a backslash of the text format and the end of a line change the state of the line.
        let special = |b: u8| {
            b == b'\n'
                || b == b'\r'
                || if csv { b == quote || Some(b) == escape } else { b == b'\\' }
        };
        let (end, next) = loop {
            let run = find(&buf[i..], special);
            if run > 0 {
                i += run;
                last_was_esc = false;
            }
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
        *at += next;
        Ok(Some(Read::Line(line, embedded)))
    }

    /// The fields of a line of the text or the CSV format, with the checks of `NextCopyFromRawFields` on the markers of `DEFAULT` and on the number of fields. An error has no context.
    fn split(&self, line: &[u8]) -> Result<Fields, Error> {
        let Fields { fields, defaults } = if self.format == Format::Csv {
            self.csv_fields(line)
        } else {
            self.text_fields(line)
        }?;
        let no_default = defaults
            .iter()
            .zip(&self.load.has_default)
            .position(|(&marker, &has_default)| marker && !has_default);
        if let Some(at) = no_default {
            return Err(Error::new(
                SqlState::BAD_COPY_FILE_FORMAT,
                "unexpected default marker in COPY data",
            )
            .with_detail(format!("Column \"{}\" has no default value.", self.names[at])));
        }
        if fields.len() > self.names.len() {
            return Err(Error::new(
                SqlState::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column",
            ));
        }
        Ok(Fields { fields, defaults })
    }

    /// The values of the row at `at` of `pending`, which a bulk load did not read yet, or `None` for a row with an error.
    fn pending_values(&self, at: usize, reader: &Reader<'_>) -> Option<Vec<Value>> {
        let row = self.pending.get(at).filter(|row| row.unsplit)?;
        let (line, text) = (row.line, row.text.clone());
        let row = Row { line, text, fields: Spans::default(), defaults: Vec::new(), unsplit: true };
        let row = self.split_row(row).ok()?;
        self.values(&row, reader).ok()?.values
    }

    /// The row with the fields of its line, for a row that a bulk load did not read. An error has the context of the line of the row.
    fn split_row(&self, mut row: Row) -> Result<Row, Error> {
        if row.unsplit {
            let text = row.text.as_deref().unwrap_or_default();
            let Fields { fields, defaults } =
                self.split(text).map_err(|error| error.with_context(self.row_context(&row)))?;
            (row.fields, row.defaults, row.unsplit) = (fields, defaults, false);
        }
        Ok(row)
    }

    /// The bytes of the CSV format, for the threads of a bulk load.
    fn csv(&self) -> cells::Csv<'_> {
        cells::Csv {
            delimiter: self.delimiter,
            quote: self.quote,
            escape: self.escape,
            null: &self.null,
        }
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
        let mut fields = Spans::with_capacity(self.names.len(), line.len());
        let mut defaults = Vec::new();
        let mut i = 0;
        loop {
            let start = i;
            let begin = fields.bytes.len();
            let field = &mut fields.bytes;
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
            fields.end(begin, raw == self.null.as_slice());
            if last {
                return Ok(Fields::new(fields, defaults));
            }
        }
    }

    /// `CopyReadAttributesCSV`: the fields of a line of the CSV format, with `None` for the null string outside quotes, and the marks of the fields that are the marker of `DEFAULT` outside quotes.
    fn csv_fields(&self, line: &[u8]) -> Result<Fields, Error> {
        let mut fields = Spans::with_capacity(self.names.len(), line.len());
        let mut defaults = Vec::new();
        let mut i = 0;
        loop {
            let start = i;
            let begin = fields.bytes.len();
            let field = &mut fields.bytes;
            let mut saw_quote = false;
            let mut last = true;
            'field: while let Some(&c) = line.get(i) {
                i += 1;
                if c == self.delimiter {
                    last = false;
                    break;
                }
                if c != self.quote {
                    // The bytes up to the next delimiter or quote go in the field at once.
                    let rest = &line[i..];
                    let run = rest
                        .iter()
                        .position(|&b| b == self.delimiter || b == self.quote)
                        .unwrap_or(rest.len());
                    field.push(c);
                    field.extend_from_slice(&rest[..run]);
                    i += run;
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
            fields.end(begin, !saw_quote && raw == self.null.as_slice());
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
            let mut fields = Spans::with_capacity(expected, 0);
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
                fields.push(Some(bytes));
                at += size;
            }
            self.buf.drain(..at);
            self.pending.push(Row {
                line: self.line,
                text: None,
                fields,
                defaults: Vec::new(),
                unsplit: false,
            });
            self.line += 1;
        }
        self.ended = true;
        let rest = self.buf.split_off(2);
        self.buf.clear();
        self.data(&rest)
    }
}

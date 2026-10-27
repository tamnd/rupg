//! `COPY ... TO STDOUT`, as `DoCopy`, `ProcessCopyOptions`, `BeginCopyTo` and `DoCopyTo` in `copy.c` and `copyto.c` run it. The rows go to the client in the text, CSV, binary or JSON format. A file and a program give `0A000`. `COPY FROM` of the extended protocol gives `0A000` too.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_platform::{File, OpenMode};
use rupg_sql::nodes::{CopyStmt, DefElem, Node};
use rupg_types::{Record, Value, oid};
use rupg_wire::{CommandTag, OutBuf};

use super::{Applied, Connection, Done};
use crate::guc::encoding;
use crate::query::{self, Reader};
use crate::utility::Notice;

/// `ROLE_PG_READ_SERVER_FILES`.
const PG_READ_SERVER_FILES: u32 = 4569;
/// `ROLE_PG_WRITE_SERVER_FILES`.
const PG_WRITE_SERVER_FILES: u32 = 4570;

/// `PGCOPY\n\377\r\n\0`, the signature of the binary format.
pub(super) const BINARY_SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

/// The rows of a `COPY ... TO STDOUT` as the session sends them.
#[derive(Debug)]
pub(super) struct CopyOut {
    /// The binary format.
    pub(super) binary: bool,
    /// The number of columns.
    pub(super) columns: usize,
    /// The `CopyData` messages.
    pub(super) data: Vec<Vec<u8>>,
    /// The number of rows, for `CommandComplete`.
    pub(super) rows: u64,
    /// The notices of the triggers of a write before its rows, and the notices of the triggers at the end of the write, which come after `CopyDone`.
    pub(super) notices: (Vec<Notice>, Vec<Notice>),
}

/// `CopyFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Text,
    Csv,
    Binary,
    Json,
}

impl Format {
    /// The name of the format in the errors.
    fn mode(self) -> &'static str {
        if self == Format::Binary { "BINARY" } else { "JSON" }
    }
}

/// The columns of `FORCE_QUOTE`, `FORCE_NOT_NULL` or `FORCE_NULL`.
#[derive(Debug)]
pub(super) enum ForceQuote {
    None,
    All,
    Names(Vec<String>),
}

/// `CopyFormatOptions`.
#[derive(Debug)]
pub(super) struct Options {
    pub(super) format: Format,
    pub(super) delimiter: u8,
    pub(super) null: String,
    pub(super) quote: u8,
    pub(super) escape: u8,
    /// The number of header lines. `COPY ... TO` writes at most one.
    pub(super) header: i32,
    /// `HEADER MATCH`: `COPY ... FROM` checks the names of the header line.
    pub(super) header_match: bool,
    force_quote: ForceQuote,
    /// The columns of `FORCE_NOT_NULL` and `FORCE_NULL`, for `COPY ... FROM`.
    pub(super) force_not_null: ForceQuote,
    pub(super) force_null: ForceQuote,
    /// `FREEZE`.
    pub(super) freeze: bool,
    /// The canonical name of the encoding of `ENCODING`.
    pub(super) encoding: Option<&'static str>,
    /// `DEFAULT`: the text of a field that gives the default of its column.
    pub(super) default: Option<String>,
    pub(super) on_error: OnError,
    /// `REJECT_LIMIT`: the number of rows that `ON_ERROR IGNORE` can skip, or `None` for no limit.
    pub(super) reject_limit: Option<i64>,
    pub(super) verbosity: Verbosity,
    /// `FORCE_ARRAY`: the JSON format puts the rows in an array.
    force_array: bool,
}

/// `CopyOnErrorChoice`: what `COPY ... FROM` does for a field that the input function of its type does not accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnError {
    /// The copy stops with the error.
    Stop,
    /// The copy skips the row.
    Ignore,
    /// The value of the field is null.
    SetNull,
}

/// `CopyLogVerbosityChoice`: the notices of `ON_ERROR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Verbosity {
    /// No notices.
    Silent,
    /// A notice with the number of rows at the end of the copy.
    Default,
    /// Also a notice for each field that the copy did not accept.
    Verbose,
}

impl Connection {
    /// `CopyOutResponse`, the `CopyData` messages and `CopyDone`, with the notices of the triggers of a write of the query of `text`.
    pub(super) fn send_copy_data(&self, copy: &mut CopyOut, text: &str, out: &mut OutBuf) {
        copy_out_response(out, copy.binary, copy.columns);
        self.notices(&mut copy.notices.0, text, &[], out);
        for data in &copy.data {
            out.copy_data(data);
        }
        out.copy_done();
        self.notices(&mut copy.notices.1, text, &[], out);
    }

    /// The messages of [`Connection::send_copy_data`], then `CommandComplete`.
    pub(super) fn send_copy(&self, copy: &mut CopyOut, text: &str, out: &mut OutBuf) {
        self.send_copy_data(copy, text, out);
        out.command_tag(CommandTag::Copy, copy.rows);
    }
}

/// `CopyOutResponse` with the same format for all the columns.
pub(super) fn copy_out_response(out: &mut OutBuf, binary: bool, columns: usize) {
    out.copy_out_response(i8::from(binary), &vec![i16::from(binary); columns]);
}

impl Connection {
    /// `DoCopy` for `COPY ... TO STDOUT`. PostgreSQL sends `CopyOutResponse` before it runs the query, so `started` gets the format and the number of columns at that point. The caller then sends `CopyOutResponse` before an error of the run.
    pub(super) fn copy(
        &self,
        stmt: &CopyStmt,
        started: &mut Option<(bool, usize)>,
    ) -> Result<Done, Error> {
        if stmt.is_from {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM is not supported yet",
            ));
        }
        if stmt.filename.is_some() && !stmt.is_program {
            self.server_file_privilege(false)?;
        }
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let mut partitions = None;
        let (options, plan, columns, selected, rel) = if let Some(rv) = &stmt.relation {
            let rel = rupg_analyze::copy_relation(stmt, rv, &reader)?;
            let options = options(&stmt.options, false)?;
            let plan = query::plan(&rel.query, &reader, &Params::default())?;
            partitions = rel.partitions;
            (options, plan, rel.columns, rel.selected, Some(rel.name))
        } else {
            let options = options(&stmt.options, false)?;
            let Some(node) = &stmt.query else {
                return Err(Error::internal("a COPY with no relation and no query"));
            };
            let plan = match node {
                Node::SelectStmt(select) if rupg_analyze::select_into(select).is_some() => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "COPY (SELECT INTO) is not supported",
                    ));
                }
                Node::SelectStmt(_) => query::plan(node, &reader, &Params::default())?,
                _ => write_plan(node, &reader)?,
            };
            let columns: Vec<String> =
                query::columns(&plan).into_iter().map(|column| column.name).collect();
            let selected = (0..columns.len()).collect();
            (options, plan, columns, selected, None)
        };
        let plan = plan.fold(None, &reader)?;
        let force = force_quote(&options.force_quote, &columns, &selected, rel.as_deref())?;
        if let Some(name) = options.encoding
            && !matches!(name, "UTF8" | "SQL_ASCII")
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("COPY with ENCODING {name} is not supported yet"),
            ));
        }
        if let Some(file) = &stmt.filename {
            if stmt.is_program {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "COPY to a program is not supported yet",
                ));
            }
            if !file.starts_with('/') {
                return Err(Error::new(
                    SqlState::INVALID_NAME,
                    "relative path not allowed for COPY to file",
                ));
            }
        }
        let file = stmt.filename.as_deref().map(|name| self.write_server_file(name)).transpose()?;
        let binary = options.format == Format::Binary;
        let formats = vec![i16::from(binary); selected.len()];
        let json = options.format == Format::Json;
        let width = if json { 1 } else { selected.len() };
        if file.is_none() {
            *started = Some((binary, width));
        }
        let view = reader.view().cloned().unwrap_or_default();
        if plan.write().is_some() {
            plan.start(&reader)?;
        }
        let (plan, writes) = self.start_writes(plan, &[], &view)?;
        let types: Vec<u32> = plan.columns().iter().map(|target| target.expr.ty).collect();
        let mut notices = (Vec::new(), Vec::new());
        // The rows of `RETURNING` of a write, or of a query that `WITH` statements that change data end with.
        let (values, writes) = if plan.write().is_some() {
            let rows = plan.run(&[], &reader);
            let applied = rows.and_then(|rows| self.apply_write(&plan, &[], &view, rows));
            notices.0 = self.trigger_notices();
            let Applied { written, view: after, batches, .. } = applied?;
            let reader = self.reader().with_view(after);
            let rows = plan.returning(written, &[], &reader)?;
            let ended = self.end_writes(writes, &[], batches);
            notices.1 = self.trigger_notices();
            ended?;
            (Some(rows), None)
        } else {
            (None, Some(writes))
        };
        if json {
            let mut rows = match values {
                Some(rows) => rows,
                None => plan.run(&[], &reader)?,
            };
            if let Some(order) = &partitions {
                rows = by_partition(rows, order);
            }
            if let Some(writes) = writes {
                self.end_writes(writes, &[], Vec::new())?;
            }
            let names: Arc<[String]> = selected.iter().map(|&i| columns[i].clone()).collect();
            let count = rows.len() as u64;
            let data = json_rows(options.force_array, &types, &names, rows, &reader)?;
            notices.0.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            if let Some(file) = file {
                return write_file(&*file, &data, count);
            }
            return Ok(Done::Copy(CopyOut { binary, columns: width, data, rows: count, notices }));
        }
        let encode = |rows: Vec<Vec<Value>>| {
            rows.iter().map(|values| query::encode(&types, values, &reader, &formats)).collect()
        };
        let rows: Vec<_> = match (values, &partitions) {
            (Some(rows), _) => encode(rows),
            (None, Some(order)) => encode(by_partition(plan.run(&[], &reader)?, order)),
            (None, None) => query::run(&plan, &[], &reader, &formats),
        }?;
        // The messages of `RAISE` of the functions of the query, such as the functions of the policies of the table, come before the rows.
        notices.0.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        if let Some(writes) = writes {
            self.end_writes(writes, &[], Vec::new())?;
        }
        let names: Vec<&str> = selected.iter().map(|&i| columns[i].as_str()).collect();
        let data =
            if binary { binary_rows(&rows) } else { text_rows(&options, &names, &force, &rows) };
        let rows = rows.len() as u64;
        if let Some(file) = file {
            return write_file(&*file, &data, rows);
        }
        Ok(Done::Copy(CopyOut { binary, columns: selected.len(), data, rows, notices }))
    }

    /// The check of `DoCopy` for a file of the server: the current role has the privileges of `pg_read_server_files` for `COPY ... FROM` and of `pg_write_server_files` for `COPY ... TO`.
    pub(super) fn server_file_privilege(&self, is_from: bool) -> Result<(), Error> {
        let (role, name, text) = if is_from {
            (PG_READ_SERVER_FILES, "pg_read_server_files", "COPY from a file")
        } else {
            (PG_WRITE_SERVER_FILES, "pg_write_server_files", "COPY to a file")
        };
        if self.roles().has_privs_of_role(self.current_role(), role) {
            return Ok(());
        }
        Err(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to {text}"))
            .with_detail(format!("Only roles with privileges of the \"{name}\" role may {text}."))
            .with_hint(
                "Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.",
            ))
    }

    /// `AllocateFile` of `BeginCopyTo`: the file of the server that the copy writes, empty. A relative name is in the data directory, which is the working directory of the server.
    fn write_server_file(&self, name: &str) -> Result<Arc<dyn File>, Error> {
        let Some(io) = &self.io else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY TO a file in a session with no server is not supported yet",
            ));
        };
        let writing = |error: Error| {
            let prefix = format!("could not open file \"{name}\": ");
            let text = error.message().strip_prefix(&prefix).unwrap_or(error.message()).to_owned();
            let hint = matches!(text.as_str(), "No such file or directory" | "Permission denied");
            let error = Error::new(
                error.state(),
                format!("could not open file \"{name}\" for writing: {text}"),
            );
            if hint {
                error.with_hint("COPY TO instructs the PostgreSQL server process to write a file. You may want a client-side facility such as psql's \\copy.")
            } else {
                error
            }
        };
        let file = io.open(Path::new(name), OpenMode::Create).map_err(writing)?;
        file.set_size(0)?;
        Ok(file)
    }
}

/// `CopySendEndOfRow` to a file: the bytes of the rows go to the file, and the result is the number of rows.
fn write_file(file: &dyn File, data: &[Vec<u8>], rows: u64) -> Result<Done, Error> {
    let bytes = data.concat();
    file.write_at(0, &bytes)?;
    Ok(Done::Count(CommandTag::Copy, rows))
}

/// `BeginCopyTo` for the query of `COPY` that is an `INSERT`, an `UPDATE`, a `DELETE` or a `MERGE`: the plan of the statement, or of the one statement of the rewrite by a rule `DO INSTEAD`. The statement must have `RETURNING`.
fn write_plan(node: &Node, reader: &Reader<'_>) -> Result<Plan, Error> {
    let not_supported = |text: &str| Error::new(SqlState::FEATURE_NOT_SUPPORTED, text.to_owned());
    let plan = match rupg_analyze::query_rewrite(node, reader)? {
        Some(products) => {
            if let Some(error) = rupg_analyze::copy_rules_error(&products) {
                return Err(error);
            }
            let [product] = products.as_slice() else {
                return Err(Error::internal("a rewrite of more than one statement"));
            };
            if !matches!(
                product.stmt,
                Node::InsertStmt(_)
                    | Node::UpdateStmt(_)
                    | Node::DeleteStmt(_)
                    | Node::MergeStmt(_)
            ) {
                return Err(not_supported("COPY query must not be a utility command"));
            }
            let query = rupg_analyze::analyze_product(product, reader, &Params::default())?;
            rupg_exec::prepare(query, reader)?
        }
        None => query::plan(node, reader, &Params::default())?,
    };
    if plan.write().is_some_and(|write| write.returning.is_none()) {
        return Err(not_supported("COPY query must have a RETURNING clause"));
    }
    Ok(plan)
}

/// The rows of a `COPY ... TO` of a partitioned table in the order of `DoCopyTo`: the rows of each partition of `order` in turn, each partition in the order of its scan. Each row has the OID of its partition last, and the result has no OIDs.
fn by_partition(rows: Vec<Vec<Value>>, order: &[u32]) -> Vec<Vec<Value>> {
    let places: BTreeMap<u32, usize> =
        order.iter().enumerate().map(|(at, &oid)| (oid, at)).collect();
    let mut parts: Vec<Vec<Vec<Value>>> = vec![Vec::new(); order.len()];
    for mut row in rows {
        let Some(Value::Oid(oid)) = row.pop() else { continue };
        if let Some(&at) = places.get(&oid) {
            parts[at].push(row);
        }
    }
    parts.into_iter().flatten().collect()
}

/// `ProcessCopyOptions`. `is_from` is true for `COPY ... FROM`.
pub(super) fn options(list: &[Option<Node>], is_from: bool) -> Result<Options, Error> {
    let mut format = None;
    let mut freeze = None;
    let mut delimiter: Option<String> = None;
    let mut null: Option<String> = None;
    let mut default: Option<String> = None;
    let mut header = None;
    let mut quote: Option<String> = None;
    let mut escape: Option<String> = None;
    let mut force_quote = ForceQuote::None;
    let mut force_not_null = ForceQuote::None;
    let mut force_null = ForceQuote::None;
    let mut convert_selectively = false;
    let mut encoding_name = None;
    let mut force_array = None;
    let mut log_verbosity = None;
    let mut reject = None;
    let mut on_error = None;
    for node in list.iter().flatten() {
        let Node::DefElem(def) = node else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = usize::try_from(def.location).ok();
        let conflict = || {
            Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options").with_position(at)
        };
        let column_list = || {
            Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("argument to option \"{name}\" must be a list of column names"),
            )
            .with_position(at)
        };
        match name {
            "format" => {
                let text = def_string(def)?;
                if format.is_some() {
                    return Err(conflict());
                }
                format = Some(match text.as_str() {
                    "text" => Format::Text,
                    "csv" => Format::Csv,
                    "binary" => Format::Binary,
                    "json" => Format::Json,
                    _ => {
                        return Err(Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("COPY format \"{text}\" not recognized"),
                        )
                        .with_position(at));
                    }
                });
            }
            "freeze" => {
                if freeze.is_some() {
                    return Err(conflict());
                }
                freeze = Some(def_boolean(def)?);
            }
            "delimiter" | "null" | "default" | "quote" | "escape" => {
                let slot = match name {
                    "delimiter" => &mut delimiter,
                    "null" => &mut null,
                    "default" => &mut default,
                    "quote" => &mut quote,
                    _ => &mut escape,
                };
                if slot.is_some() {
                    return Err(conflict());
                }
                *slot = Some(def_string(def)?);
            }
            "header" => {
                if header.is_some() {
                    return Err(conflict());
                }
                header = Some(header_option(def, is_from)?);
            }
            "force_quote" | "force_not_null" | "force_null" => {
                let slot = match name {
                    "force_quote" => &mut force_quote,
                    "force_null" => &mut force_null,
                    _ => &mut force_not_null,
                };
                if !matches!(slot, ForceQuote::None) {
                    return Err(conflict());
                }
                *slot = match &def.arg {
                    Some(Node::A_Star(_)) => ForceQuote::All,
                    Some(Node::List(names)) => ForceQuote::Names(
                        names.iter().flatten().map(name_text).collect::<Result<_, _>>()?,
                    ),
                    _ => return Err(column_list()),
                };
            }
            "convert_selectively" => {
                if convert_selectively {
                    return Err(conflict());
                }
                convert_selectively = true;
                if !matches!(&def.arg, None | Some(Node::List(_))) {
                    return Err(column_list());
                }
            }
            "encoding" => {
                if encoding_name.is_some() {
                    return Err(conflict());
                }
                let text = def_string(def)?;
                encoding_name = Some(encoding(&text).ok_or_else(|| {
                    Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("argument to option \"{name}\" must be a valid encoding name"),
                    )
                    .with_position(at)
                })?);
            }
            "force_array" => {
                if force_array.is_some() {
                    return Err(conflict());
                }
                force_array = Some(def_boolean(def)?);
            }
            "on_error" => {
                if on_error.is_some() {
                    return Err(conflict());
                }
                let text = def_string(def)?;
                if !is_from {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "COPY ON_ERROR cannot be used with COPY TO",
                    )
                    .with_position(at));
                }
                on_error = Some(
                    [
                        ("stop", OnError::Stop),
                        ("ignore", OnError::Ignore),
                        ("set_null", OnError::SetNull),
                    ]
                    .into_iter()
                    .find(|(v, _)| text.eq_ignore_ascii_case(v))
                    .map(|(_, choice)| choice)
                    .ok_or_else(|| {
                        Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("COPY ON_ERROR \"{text}\" not recognized"),
                        )
                        .with_position(at)
                    })?,
                );
            }
            "log_verbosity" => {
                if log_verbosity.is_some() {
                    return Err(conflict());
                }
                let text = def_string(def)?;
                log_verbosity = Some(
                    [
                        ("silent", Verbosity::Silent),
                        ("default", Verbosity::Default),
                        ("verbose", Verbosity::Verbose),
                    ]
                    .into_iter()
                    .find(|(v, _)| text.eq_ignore_ascii_case(v))
                    .map(|(_, verbosity)| verbosity)
                    .ok_or_else(|| {
                        Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("COPY LOG_VERBOSITY \"{text}\" not recognized"),
                        )
                        .with_position(at)
                    })?,
                );
            }
            "reject_limit" => {
                if reject.is_some() {
                    return Err(conflict());
                }
                let limit = reject_limit(def)?;
                if limit <= 0 {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("REJECT_LIMIT ({limit}) must be greater than zero"),
                    ));
                }
                reject = Some(limit);
            }
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("option \"{name}\" not recognized"),
                )
                .with_position(at));
            }
        }
    }
    let format = format.unwrap_or(Format::Text);
    let binary_like = matches!(format, Format::Binary | Format::Json);
    for (given, option) in [(&delimiter, "DELIMITER"), (&null, "NULL"), (&default, "DEFAULT")] {
        if given.is_some() && binary_like {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("cannot specify {option} in {} mode", format.mode()),
            ));
        }
    }
    let csv = format == Format::Csv;
    let delimiter = delimiter.unwrap_or_else(|| (if csv { "," } else { "\t" }).to_string());
    let null = null.unwrap_or_else(|| (if csv { "" } else { "\\N" }).to_string());
    let given_quote = quote.is_some();
    let given_escape = escape.is_some();
    let quote = if csv { quote.or_else(|| Some("\"".to_string())) } else { quote };
    let escape = if csv { escape.or_else(|| quote.clone()) } else { escape };
    let not_supported = |message: &str| Error::new(SqlState::FEATURE_NOT_SUPPORTED, message);
    let invalid = |message: String| Error::new(SqlState::INVALID_PARAMETER_VALUE, message);
    let &[delim] = delimiter.as_bytes() else {
        return Err(not_supported("COPY delimiter must be a single one-byte character"));
    };
    if matches!(delim, b'\r' | b'\n') {
        return Err(invalid("COPY delimiter cannot be newline or carriage return".to_string()));
    }
    if null.contains(['\r', '\n']) {
        return Err(invalid(
            "COPY null representation cannot use newline or carriage return".to_string(),
        ));
    }
    if default.as_ref().is_some_and(|d| d.contains(['\r', '\n'])) {
        return Err(invalid(
            "COPY default representation cannot use newline or carriage return".to_string(),
        ));
    }
    if !csv && b"\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(&delim) {
        return Err(invalid(format!("COPY delimiter cannot be \"{delimiter}\"")));
    }
    let (header, header_match) = header.unwrap_or((0, false));
    if (header > 0 || header_match) && binary_like {
        return Err(not_supported(&format!("cannot specify HEADER in {} mode", format.mode())));
    }
    if !csv && given_quote {
        return Err(not_supported("COPY QUOTE requires CSV mode"));
    }
    let quote_byte = match quote.as_deref().map(str::as_bytes) {
        Some(&[byte]) => byte,
        Some(_) => return Err(not_supported("COPY quote must be a single one-byte character")),
        None => b'"',
    };
    if csv && delim == quote_byte {
        return Err(invalid("COPY delimiter and quote must be different".to_string()));
    }
    if !csv && given_escape {
        return Err(not_supported("COPY ESCAPE requires CSV mode"));
    }
    let escape_byte = match escape.as_deref().map(str::as_bytes) {
        Some(&[byte]) => byte,
        Some(_) => return Err(not_supported("COPY escape must be a single one-byte character")),
        None => b'"',
    };
    if !csv && !matches!(force_quote, ForceQuote::None) {
        return Err(not_supported("COPY FORCE_QUOTE requires CSV mode"));
    }
    if is_from && !matches!(force_quote, ForceQuote::None) {
        return Err(not_supported("COPY FORCE_QUOTE cannot be used with COPY FROM"));
    }
    for (force, option) in [(&force_not_null, "FORCE_NOT_NULL"), (&force_null, "FORCE_NULL")] {
        let given = !matches!(force, ForceQuote::None);
        if given && !csv {
            return Err(not_supported(&format!("COPY {option} requires CSV mode")));
        }
        if given && !is_from {
            return Err(invalid(format!("COPY {option} cannot be used with COPY TO")));
        }
    }
    if null.as_bytes().contains(&delim) {
        return Err(invalid(
            "COPY delimiter character must not appear in the NULL specification".to_string(),
        ));
    }
    if csv && null.as_bytes().contains(&quote_byte) {
        return Err(invalid(
            "CSV quote character must not appear in the NULL specification".to_string(),
        ));
    }
    let freeze = freeze == Some(true);
    if freeze && !is_from {
        return Err(invalid("COPY FREEZE cannot be used with COPY TO".to_string()));
    }
    if format == Format::Json && is_from {
        return Err(invalid("COPY FORMAT JSON is not supported for COPY FROM".to_string()));
    }
    if format != Format::Json && force_array == Some(true) {
        return Err(invalid("COPY FORCE_ARRAY can only be used with JSON mode".to_string()));
    }
    if let Some(default) = &default {
        if !is_from {
            return Err(not_supported("COPY DEFAULT cannot be used with COPY TO"));
        }
        if default.as_bytes().contains(&delim) {
            return Err(not_supported(
                "COPY delimiter character must not appear in the DEFAULT specification",
            ));
        }
        if csv && default.as_bytes().contains(&quote_byte) {
            return Err(not_supported(
                "CSV quote character must not appear in the DEFAULT specification",
            ));
        }
        if *default == null {
            return Err(not_supported(
                "NULL specification and DEFAULT specification cannot be the same",
            ));
        }
    }
    let on_error = on_error.unwrap_or(OnError::Stop);
    if format == Format::Binary && on_error != OnError::Stop {
        return Err(Error::new(
            SqlState::SYNTAX_ERROR,
            "only ON_ERROR STOP is allowed in BINARY mode",
        ));
    }
    if reject.is_some() && on_error != OnError::Ignore {
        return Err(invalid("COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE".to_string()));
    }
    Ok(Options {
        format,
        delimiter: delim,
        null,
        quote: quote_byte,
        escape: escape_byte,
        header,
        header_match,
        force_quote,
        force_not_null,
        force_null,
        freeze,
        encoding: encoding_name,
        default,
        on_error,
        reject_limit: reject,
        verbosity: log_verbosity.unwrap_or(Verbosity::Default),
        force_array: force_array == Some(true),
    })
}

/// `defGetString`.
pub(super) fn def_string(def: &DefElem) -> Result<String, Error> {
    let name = def.defname.as_deref().unwrap_or_default();
    match &def.arg {
        None => Err(Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a parameter"))),
        Some(Node::Integer(n)) => Ok(n.to_string()),
        Some(Node::Float(s) | Node::String(s)) => Ok(s.to_string()),
        Some(Node::Boolean(b)) => Ok(b.to_string()),
        Some(Node::A_Star(_)) => Ok("*".to_string()),
        Some(Node::List(names)) => {
            let parts: Vec<String> =
                names.iter().flatten().map(name_text).collect::<Result<_, _>>()?;
            Ok(parts.join("."))
        }
        Some(_) => Err(Error::internal("unrecognized node type in a COPY option")),
    }
}

/// The text of a `String` node of a list of names.
fn name_text(node: &Node) -> Result<String, Error> {
    match node {
        Node::String(s) => Ok(s.to_string()),
        _ => Err(Error::internal("unexpected node type in name list")),
    }
}

/// `defGetBoolean`.
pub(super) fn def_boolean(def: &DefElem) -> Result<bool, Error> {
    let value = match &def.arg {
        None => Some(true),
        Some(Node::Integer(n)) => match n {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        Some(_) => boolean_word(&def_string(def)?),
    };
    value.ok_or_else(|| {
        let name = def.defname.as_deref().unwrap_or_default();
        Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a Boolean value"))
    })
}

/// The words of `opt_boolean_or_string` that are a Boolean value.
fn boolean_word(text: &str) -> Option<bool> {
    if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("on") {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") || text.eq_ignore_ascii_case("off") {
        Some(false)
    } else {
        None
    }
}

/// `defGetCopyHeaderOption`: the number of header lines, and true for `MATCH`.
fn header_option(def: &DefElem, is_from: bool) -> Result<(i32, bool), Error> {
    let name = def.defname.as_deref().unwrap_or_default();
    let value = match &def.arg {
        None => return Ok((1, false)),
        Some(Node::Integer(n)) => *n,
        Some(_) => {
            let text = def_string(def)?;
            if let Some(value) = boolean_word(&text) {
                return Ok((i32::from(value), false));
            }
            if text.eq_ignore_ascii_case("match") {
                if is_from {
                    return Ok((1, true));
                }
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot use \"{text}\" with HEADER in COPY TO"),
                ));
            }
            rupg_types::int4_in(&text).map_err(|_| {
                Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!(
                        "{name} requires a Boolean value, an integer value greater than or equal to zero, or the string \"match\""
                    ),
                )
            })?
        }
    };
    if value < 0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("a negative integer value cannot be specified for {name}"),
        ));
    }
    if value > 1 && !is_from {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot use multi-line header in COPY TO",
        ));
    }
    Ok((value, false))
}

/// `defGetCopyRejectLimitOption`: the value of `REJECT_LIMIT`. A string or a number that is not an integer goes through the input function of `bigint`.
fn reject_limit(def: &DefElem) -> Result<i64, Error> {
    let name = def.defname.as_deref().unwrap_or_default();
    match &def.arg {
        None => Err(Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a numeric value"))),
        Some(Node::Integer(n)) => Ok(i64::from(*n)),
        Some(Node::String(s) | Node::Float(s)) => Ok(rupg_types::int8_in(s)?),
        Some(_) => {
            Err(Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a numeric value")))
        }
    }
}

/// The flags of `FORCE_QUOTE` for the selected columns, as `BeginCopyTo` sets them.
fn force_quote(
    force: &ForceQuote,
    columns: &[String],
    selected: &[usize],
    rel: Option<&str>,
) -> Result<Vec<bool>, Error> {
    match force {
        ForceQuote::None => Ok(vec![false; selected.len()]),
        ForceQuote::All => Ok(vec![true; selected.len()]),
        ForceQuote::Names(names) => {
            let mut flags = vec![false; selected.len()];
            for at in rupg_analyze::attnums(columns, names, rel)? {
                let Some(i) = selected.iter().position(|&s| s == at) else {
                    return Err(Error::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!("FORCE_QUOTE column \"{}\" not referenced by COPY", columns[at]),
                    ));
                };
                flags[i] = true;
            }
            Ok(flags)
        }
    }
}

/// The `CopyData` messages of the text and CSV formats: the header line, then a line for each row.
fn text_rows(
    options: &Options,
    names: &[&str],
    force: &[bool],
    rows: &[query::Row],
) -> Vec<Vec<u8>> {
    let csv = options.format == Format::Csv;
    let single = names.len() == 1;
    let mut data = Vec::with_capacity(rows.len() + 1);
    if options.header > 0 {
        let mut line = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                line.push(options.delimiter);
            }
            if csv {
                csv_value(options, name.as_bytes(), false, single, &mut line);
            } else {
                text_value(options.delimiter, name.as_bytes(), &mut line);
            }
        }
        line.push(b'\n');
        data.push(line);
    }
    for row in rows {
        let mut line = Vec::new();
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                line.push(options.delimiter);
            }
            match value {
                None => line.extend_from_slice(options.null.as_bytes()),
                Some(bytes) if csv => csv_value(options, bytes, force[i], single, &mut line),
                Some(bytes) => text_value(options.delimiter, bytes, &mut line),
            }
        }
        line.push(b'\n');
        data.push(line);
    }
    data
}

/// `CopyAttributeOutText`: a control character as its escape, and a backslash before a backslash and before the delimiter.
fn text_value(delimiter: u8, value: &[u8], out: &mut Vec<u8>) {
    for &byte in value {
        if byte < 0x20 {
            let escaped = match byte {
                0x08 => b'b',
                0x0c => b'f',
                b'\n' => b'n',
                b'\r' => b'r',
                b'\t' => b't',
                0x0b => b'v',
                _ if byte == delimiter => byte,
                _ => {
                    out.push(byte);
                    continue;
                }
            };
            out.push(b'\\');
            out.push(escaped);
        } else {
            if byte == b'\\' || byte == delimiter {
                out.push(b'\\');
            }
            out.push(byte);
        }
    }
}

/// `CopyAttributeOutCSV`: the value in quotes when it needs them, with the escape before a quote and before an escape in the quotes.
fn csv_value(options: &Options, value: &[u8], force: bool, single: bool, out: &mut Vec<u8>) {
    let quote = force
        || value == options.null.as_bytes()
        || (single && value == b"\\.")
        || value
            .iter()
            .any(|&b| b == options.delimiter || b == options.quote || b == b'\n' || b == b'\r');
    if !quote {
        out.extend_from_slice(value);
        return;
    }
    out.push(options.quote);
    for &byte in value {
        if byte == options.quote || byte == options.escape {
            out.push(options.escape);
        }
        out.push(byte);
    }
    out.push(options.quote);
}

/// `CopyToJsonOneRow`: a line for each row with the row as `composite_to_json` writes it, with the names of the columns as the keys. `FORCE_ARRAY` puts a line with `[` first and a line with `]` last, and a comma before each row after the first.
fn json_rows(
    force_array: bool,
    types: &[u32],
    names: &Arc<[String]>,
    rows: Vec<Vec<Value>>,
    session: &dyn rupg_func::Session,
) -> Result<Vec<Vec<u8>>, Error> {
    let mut data = Vec::with_capacity(rows.len() + 2);
    if force_array {
        data.push(b"[\n".to_vec());
    }
    for (i, mut values) in rows.into_iter().enumerate() {
        values.truncate(types.len());
        let record = Record { types: types.to_vec(), values, names: Some(Arc::clone(names)) };
        let mut line = String::new();
        if force_array {
            line.push(if i == 0 { ' ' } else { ',' });
        }
        rupg_func::json_value(
            oid::RECORD,
            &Value::Record(Box::new(record)),
            false,
            session,
            &mut line,
        )?;
        line.push('\n');
        data.push(line.into_bytes());
    }
    if force_array {
        data.push(b"]\n".to_vec());
    }
    Ok(data)
}

/// The `CopyData` messages of the binary format. The header goes with the first row, and the trailer is a message of its own, as `CopySendEndOfRow` sends them.
fn binary_rows(rows: &[query::Row]) -> Vec<Vec<u8>> {
    let mut data = Vec::with_capacity(rows.len() + 1);
    let mut message = Vec::new();
    message.extend_from_slice(BINARY_SIGNATURE);
    message.extend_from_slice(&0_i32.to_be_bytes());
    message.extend_from_slice(&0_i32.to_be_bytes());
    for row in rows {
        let count = i16::try_from(row.len()).unwrap_or(i16::MAX);
        message.extend_from_slice(&count.to_be_bytes());
        for value in row {
            match value {
                None => message.extend_from_slice(&(-1_i32).to_be_bytes()),
                Some(bytes) => {
                    let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
                    message.extend_from_slice(&len.to_be_bytes());
                    message.extend_from_slice(bytes);
                }
            }
        }
        data.push(std::mem::take(&mut message));
    }
    message.extend_from_slice(&(-1_i16).to_be_bytes());
    data.push(message);
    data
}

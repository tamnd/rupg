//! `COPY ... TO STDOUT`, as `DoCopy`, `ProcessCopyOptions`, `BeginCopyTo` and `DoCopyTo` in `copy.c` and `copyto.c` run it. The rows go to the client in the text, CSV or binary format. `COPY FROM`, a file, a program and the JSON format give `0A000`.

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{CopyStmt, DefElem, Node};
use rupg_wire::{CommandTag, OutBuf};

use super::{Connection, Done};
use crate::guc::encoding;
use crate::query;

/// `PGCOPY\n\377\r\n\0`, the signature of the binary format.
const BINARY_SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

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
}

/// `CopyFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
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

/// The columns of `FORCE_QUOTE`.
#[derive(Debug)]
enum ForceQuote {
    None,
    All,
    Names(Vec<String>),
}

/// `CopyFormatOptions` for `COPY ... TO`.
#[derive(Debug)]
struct Options {
    format: Format,
    delimiter: u8,
    null: String,
    quote: u8,
    escape: u8,
    header: bool,
    force_quote: ForceQuote,
    /// The canonical name of the encoding of `ENCODING`.
    encoding: Option<&'static str>,
}

impl CopyOut {
    /// `CopyOutResponse`, the `CopyData` messages and `CopyDone`.
    pub(super) fn send_data(&self, out: &mut OutBuf) {
        copy_out_response(out, self.binary, self.columns);
        for data in &self.data {
            out.copy_data(data);
        }
        out.copy_done();
    }

    /// The messages of [`CopyOut::send_data`], then `CommandComplete`.
    pub(super) fn send(&self, out: &mut OutBuf) {
        self.send_data(out);
        out.command_tag(CommandTag::Copy, self.rows);
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
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let (options, plan, columns, selected, rel) = if let Some(rv) = &stmt.relation {
            let rel = rupg_analyze::copy_relation(stmt, rv, &reader)?;
            let options = options(&stmt.options)?;
            let plan = query::plan(&rel.query, &reader, &Params::default())?;
            (options, plan, rel.columns, rel.selected, Some(rel.name))
        } else {
            let options = options(&stmt.options)?;
            let Some(node) = &stmt.query else {
                return Err(Error::internal("a COPY with no relation and no query"));
            };
            let Node::SelectStmt(select) = node else {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("COPY of {} is not supported yet", statement_name(node)),
                ));
            };
            if select.intoClause.is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "COPY (SELECT INTO) is not supported",
                ));
            }
            let plan = query::plan(node, &reader, &Params::default())?;
            let columns: Vec<String> =
                query::columns(&plan).into_iter().map(|column| column.name).collect();
            let selected = (0..columns.len()).collect();
            (options, plan, columns, selected, None)
        };
        let plan = plan.fold(None, &reader)?;
        if options.format == Format::Json {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FORMAT JSON is not supported yet",
            ));
        }
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
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY to a file is not supported yet",
            ));
        }
        let binary = options.format == Format::Binary;
        let formats = vec![i16::from(binary); selected.len()];
        *started = Some((binary, selected.len()));
        let rows = query::run(&plan, &[], &reader, &formats)?;
        let names: Vec<&str> = selected.iter().map(|&i| columns[i].as_str()).collect();
        let data =
            if binary { binary_rows(&rows) } else { text_rows(&options, &names, &force, &rows) };
        Ok(Done::Copy(CopyOut { binary, columns: selected.len(), data, rows: rows.len() as u64 }))
    }
}

/// The name of a statement in the query of `COPY` that is not a `SELECT`.
fn statement_name(node: &Node) -> &'static str {
    match node {
        Node::InsertStmt(_) => "INSERT",
        Node::UpdateStmt(_) => "UPDATE",
        Node::DeleteStmt(_) => "DELETE",
        Node::MergeStmt(_) => "MERGE",
        _ => "this statement",
    }
}

/// `ProcessCopyOptions` for `COPY ... TO`.
fn options(list: &[Option<Node>]) -> Result<Options, Error> {
    let mut format = None;
    let mut freeze = None;
    let mut delimiter: Option<String> = None;
    let mut null: Option<String> = None;
    let mut default: Option<String> = None;
    let mut header = None;
    let mut quote: Option<String> = None;
    let mut escape: Option<String> = None;
    let mut force_quote = ForceQuote::None;
    let mut force_not_null = false;
    let mut force_null = false;
    let mut convert_selectively = false;
    let mut encoding_name = None;
    let mut force_array = None;
    let mut log_verbosity = false;
    let mut reject = None;
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
                header = Some(header_option(def)?);
            }
            "force_quote" => {
                if !matches!(force_quote, ForceQuote::None) {
                    return Err(conflict());
                }
                force_quote = match &def.arg {
                    Some(Node::A_Star(_)) => ForceQuote::All,
                    Some(Node::List(names)) => ForceQuote::Names(
                        names.iter().flatten().map(name_text).collect::<Result<_, _>>()?,
                    ),
                    _ => return Err(column_list()),
                };
            }
            "force_not_null" | "force_null" => {
                let seen = if name == "force_null" { &mut force_null } else { &mut force_not_null };
                if *seen {
                    return Err(conflict());
                }
                if !matches!(&def.arg, Some(Node::A_Star(_) | Node::List(_))) {
                    return Err(column_list());
                }
                *seen = true;
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
                def_string(def)?;
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "COPY ON_ERROR cannot be used with COPY TO",
                )
                .with_position(at));
            }
            "log_verbosity" => {
                if log_verbosity {
                    return Err(conflict());
                }
                log_verbosity = true;
                let text = def_string(def)?;
                if !["silent", "default", "verbose"].iter().any(|v| text.eq_ignore_ascii_case(v)) {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("COPY LOG_VERBOSITY \"{text}\" not recognized"),
                    )
                    .with_position(at));
                }
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
    let header = header.unwrap_or(false);
    if header && binary_like {
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
    for (given, option) in [(force_not_null, "FORCE_NOT_NULL"), (force_null, "FORCE_NULL")] {
        if given && !csv {
            return Err(not_supported(&format!("COPY {option} requires CSV mode")));
        }
        if given {
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
    if freeze == Some(true) {
        return Err(invalid("COPY FREEZE cannot be used with COPY TO".to_string()));
    }
    if format != Format::Json && force_array == Some(true) {
        return Err(invalid("COPY FORCE_ARRAY can only be used with JSON mode".to_string()));
    }
    if default.is_some() {
        return Err(not_supported("COPY DEFAULT cannot be used with COPY TO"));
    }
    if reject.is_some() {
        return Err(invalid("COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE".to_string()));
    }
    Ok(Options {
        format,
        delimiter: delim,
        null,
        quote: quote_byte,
        escape: escape_byte,
        header,
        force_quote,
        encoding: encoding_name,
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

/// `defGetCopyHeaderOption` for `COPY ... TO`: true for a header line.
fn header_option(def: &DefElem) -> Result<bool, Error> {
    let name = def.defname.as_deref().unwrap_or_default();
    let value = match &def.arg {
        None => return Ok(true),
        Some(Node::Integer(n)) => *n,
        Some(_) => {
            let text = def_string(def)?;
            if let Some(value) = boolean_word(&text) {
                return Ok(value);
            }
            if text.eq_ignore_ascii_case("match") {
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
    if value > 1 {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot use multi-line header in COPY TO",
        ));
    }
    Ok(value == 1)
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
    if options.header {
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

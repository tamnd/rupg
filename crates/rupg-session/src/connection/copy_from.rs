//! `COPY ... FROM STDIN`, as `DoCopy`, `BeginCopyFrom`, `CopyFrom` and `copyfromparse.c` run it. The tables of rupg have no rows yet, so a copy reads the header lines and the end of the data, and a row of data gives `0A000`.

use rupg_common::{Error, SqlState};
use rupg_sql::nodes::CopyStmt;
use rupg_wire::OutBuf;

use super::Connection;
use super::copy::{BINARY_SIGNATURE, ForceQuote, Format, options};

/// `MAX_COPY_DATA_DISPLAY`: the number of bytes of a line that the context of an error shows.
const MAX_DISPLAY: usize = 100;

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
}

impl Connection {
    /// The checks of `DoCopy` and `BeginCopyFrom` for `COPY ... FROM STDIN`, then `CopyInResponse` and the checks of `CopyFrom`. The caller starts the copy with the state that this gives.
    pub(super) fn copy_from(&self, stmt: &CopyStmt, out: &mut OutBuf) -> Result<CopyIn, Error> {
        if stmt.filename.is_some() {
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
        let binary = options.format == Format::Binary;
        out.copy_in_response(i8::from(binary), &vec![i16::from(binary); target.selected.len()]);
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
        })
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
                    return Err(self.at_line(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "COPY FROM with rows is not supported yet",
                    )));
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
        let context = || {
            let shown = if line.len() > MAX_DISPLAY {
                let mut cut = MAX_DISPLAY;
                while cut > 0 && (line[cut] & 0xC0) == 0x80 {
                    cut -= 1;
                }
                format!("{}...", String::from_utf8_lossy(&line[..cut]))
            } else {
                String::from_utf8_lossy(line).into_owned()
            };
            format!("COPY {}, line {}: \"{shown}\"", self.name, self.line)
        };
        let bad = |message: String| {
            Error::new(SqlState::BAD_COPY_FILE_FORMAT, message).with_context(context())
        };
        let fields = fields.map_err(|error| error.with_context(context()))?;
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

    /// `CopyReadAttributesText`: the fields of a line of the text format, with `None` for the null string.
    fn text_fields(&self, line: &[u8]) -> Result<Vec<Option<Vec<u8>>>, Error> {
        let mut fields = Vec::new();
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
            fields.push(if raw == self.null.as_slice() { None } else { Some(field) });
            if last {
                return Ok(fields);
            }
        }
    }

    /// `CopyReadAttributesCSV`: the fields of a line of the CSV format, with `None` for the null string outside quotes.
    fn csv_fields(&self, line: &[u8]) -> Result<Vec<Option<Vec<u8>>>, Error> {
        let mut fields = Vec::new();
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
            fields.push(if !saw_quote && raw == self.null.as_slice() { None } else { Some(field) });
            if last {
                return Ok(fields);
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
        let Some(count) = self.buf.get(..2).map(|b| i16::from_be_bytes([b[0], b[1]])) else {
            // A field count that is not complete at the end of the data is the end of the data.
            return Ok(());
        };
        if count != -1 {
            return Err(self.at_line(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "COPY FROM with rows is not supported yet",
            )));
        }
        self.ended = true;
        let rest = self.buf.split_off(2);
        self.buf.clear();
        self.data(&rest)
    }
}

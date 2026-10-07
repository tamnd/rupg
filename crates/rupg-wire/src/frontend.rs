//! The messages that a client sends after startup.
//!
//! [`Frontend::parse`] reads the body of a [`Frame`] the way the main loop of `postgres.c` reads it, with the same checks in the same order and the same error texts. The strings and values borrow from the frame. Nothing is copied and nothing is converted from the client encoding.
//!
//! `Bind` and `FunctionCall` are read in two steps. PostgreSQL reads the names of a `Bind` and looks up the statement before it reads the rest, and it looks up the function of a `FunctionCall` before it reads the arguments. A bad `Bind` for a statement that does not exist is `26000` in PostgreSQL and not `08P01`. So [`Frontend::parse`] reads the head, and [`Bind::body`] and [`FunctionCall::body`] read the rest when the server is ready for them.
//!
//! Lifted from `crates/rudb-pgwire/src/frontend.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::ProtocolError;
use crate::frame::Frame;
use crate::reader::Reader;

/// What `Describe` and `Close` act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Statement,
    Portal,
}

impl Target {
    fn byte(self) -> u8 {
        match self {
            Target::Statement => b'S',
            Target::Portal => b'P',
        }
    }
}

/// One message from the client, after startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend<'a> {
    Query(&'a [u8]),
    Parse {
        name: &'a [u8],
        sql: &'a [u8],
        types: Oids<'a>,
    },
    Bind(Bind<'a>),
    Describe {
        target: Target,
        name: &'a [u8],
    },
    Execute {
        portal: &'a [u8],
        max_rows: i32,
    },
    Close {
        target: Target,
        name: &'a [u8],
    },
    Sync,
    Flush,
    Terminate,
    CopyData(&'a [u8]),
    CopyDone,
    CopyFail(&'a [u8]),
    FunctionCall(FunctionCall<'a>),
    /// `PasswordMessage`, `SASLInitialResponse`, `SASLResponse` or `GSSResponse`. They share the type byte `p`, and only the state of the authentication tells them apart.
    Password(&'a [u8]),
}

impl<'a> Frontend<'a> {
    /// Reads the body of a frame that [`split`](crate::split) found.
    ///
    /// The server reads `CopyData`, `CopyDone` and `CopyFail` outside of `COPY` and ignores them, as PostgreSQL does. So the server must not call this for them there, because a bad body is not an error in that state.
    ///
    /// # Errors
    ///
    /// A body that does not have the format of its type. These are all [`Level::Error`](crate::Level::Error), except a type byte that `split` would not give, which is [`Level::Fatal`](crate::Level::Fatal).
    pub fn parse(frame: Frame<'a>) -> Result<Frontend<'a>, ProtocolError> {
        Frontend::read(frame, Reader::new(frame.body))
    }

    /// [`Frontend::parse`] for a session whose client encoding is `UTF8` or `SQL_ASCII`. It also checks each string of the message with [`verify_utf8`](crate::verify_utf8), at the place where PostgreSQL checks it, so a message with two faults gets the same error as there.
    ///
    /// # Errors
    ///
    /// The errors of [`Frontend::parse`], and SQLSTATE `22021` for a string that is not UTF-8.
    pub fn parse_utf8(frame: Frame<'a>) -> Result<Frontend<'a>, ProtocolError> {
        Frontend::read(frame, Reader::utf8(frame.body))
    }

    fn read(frame: Frame<'a>, mut r: Reader<'a>) -> Result<Frontend<'a>, ProtocolError> {
        let message = match frame.tag {
            b'Q' => {
                let sql = r.string()?;
                r.end()?;
                Frontend::Query(sql)
            }
            b'P' => {
                let name = r.string()?;
                let sql = r.string()?;
                let count = r.u16()?;
                let types = r.bytes(i32::from(count) * 4)?;
                r.end()?;
                Frontend::Parse { name, sql, types: Oids(types) }
            }
            b'B' => {
                let portal = r.string()?;
                let statement = r.string()?;
                Frontend::Bind(Bind { portal, statement, rest: r.rest() })
            }
            b'D' | b'C' => {
                let kind = r.byte()?;
                let name = r.string()?;
                r.end()?;
                let target = match kind {
                    b'S' => Target::Statement,
                    b'P' => Target::Portal,
                    _ => {
                        let what = if frame.tag == b'D' { "DESCRIBE" } else { "CLOSE" };
                        return Err(ProtocolError::error(format!(
                            "invalid {what} message subtype {kind}"
                        )));
                    }
                };
                if frame.tag == b'D' {
                    Frontend::Describe { target, name }
                } else {
                    Frontend::Close { target, name }
                }
            }
            b'E' => {
                let portal = r.string()?;
                let max_rows = r.i32()?;
                r.end()?;
                Frontend::Execute { portal, max_rows }
            }
            b'S' => {
                r.end()?;
                Frontend::Sync
            }
            b'H' => {
                r.end()?;
                Frontend::Flush
            }
            // PostgreSQL does not read the body of these three.
            b'X' => Frontend::Terminate,
            b'c' => Frontend::CopyDone,
            b'd' => Frontend::CopyData(frame.body),
            b'f' => Frontend::CopyFail(r.string()?),
            b'F' => {
                let oid = r.u32()?;
                Frontend::FunctionCall(FunctionCall { oid, rest: r.rest() })
            }
            b'p' => Frontend::Password(frame.body),
            tag => {
                return Err(ProtocolError::fatal(format!("invalid frontend message type {tag}")));
            }
        };
        Ok(message)
    }

    /// The type byte of the message.
    pub fn tag(&self) -> u8 {
        match self {
            Frontend::Query(_) => b'Q',
            Frontend::Parse { .. } => b'P',
            Frontend::Bind(_) => b'B',
            Frontend::Describe { .. } => b'D',
            Frontend::Execute { .. } => b'E',
            Frontend::Close { .. } => b'C',
            Frontend::Sync => b'S',
            Frontend::Flush => b'H',
            Frontend::Terminate => b'X',
            Frontend::CopyData(_) => b'd',
            Frontend::CopyDone => b'c',
            Frontend::CopyFail(_) => b'f',
            Frontend::FunctionCall(_) => b'F',
            Frontend::Password(_) => b'p',
        }
    }

    /// Writes the message as a client sends it. The tests, the fuzz targets and the clients in `rupg-compat` use this. The server never sends these messages.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.push(self.tag());
        out.extend_from_slice(&[0; 4]);
        let string = |out: &mut Vec<u8>, s: &[u8]| {
            out.extend_from_slice(s);
            out.push(0);
        };
        match *self {
            Frontend::Query(sql) => string(out, sql),
            Frontend::Parse { name, sql, types } => {
                string(out, name);
                string(out, sql);
                out.extend_from_slice(&(types.len() as u16).to_be_bytes());
                out.extend_from_slice(types.0);
            }
            Frontend::Bind(bind) => {
                string(out, bind.portal);
                string(out, bind.statement);
                out.extend_from_slice(bind.rest);
            }
            Frontend::Describe { target, name } | Frontend::Close { target, name } => {
                out.push(target.byte());
                string(out, name);
            }
            Frontend::Execute { portal, max_rows } => {
                string(out, portal);
                out.extend_from_slice(&max_rows.to_be_bytes());
            }
            Frontend::Sync | Frontend::Flush | Frontend::Terminate | Frontend::CopyDone => {}
            Frontend::CopyData(data) | Frontend::Password(data) => out.extend_from_slice(data),
            Frontend::CopyFail(text) => string(out, text),
            Frontend::FunctionCall(call) => {
                out.extend_from_slice(&call.oid.to_be_bytes());
                out.extend_from_slice(call.rest);
            }
        }
        let len = (out.len() - start - 1) as u32;
        out[start + 1..start + 5].copy_from_slice(&len.to_be_bytes());
    }
}

/// The type OIDs of a `Parse`, in the order of the parameters. Zero means that the server infers the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oids<'a>(&'a [u8]);

impl<'a> Oids<'a> {
    pub fn len(&self) -> usize {
        self.0.len() / 4
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = u32> + 'a {
        self.0.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c))
    }
}

/// Format codes: 0 is text and 1 is binary. PostgreSQL checks each code when it uses it, with its own error, so this keeps any value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats<'a>(&'a [u8]);

impl<'a> Formats<'a> {
    pub fn len(&self) -> usize {
        self.0.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = i16> + 'a {
        self.0.as_chunks::<2>().0.iter().map(|c| i16::from_be_bytes(*c))
    }

    /// The format of value `i` of `count`, by the rule of the protocol: no code means text for all, one code is for all, and else there is one code for each value.
    pub fn of(&self, i: usize) -> i16 {
        let at = if self.len() == 1 { 0 } else { i };
        match self.0.get(at * 2..at * 2 + 2) {
            Some(c) => i16::from_be_bytes([c[0], c[1]]),
            None => 0,
        }
    }
}

/// Values that are each a length and the bytes, with the length -1 for null. The codec has checked every length, so the iterator cannot fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Values<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Values<'a> {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> ValueIter<'a> {
        ValueIter { reader: Reader::new(self.bytes), left: self.count }
    }
}

/// The iterator of [`Values`].
#[derive(Debug, Clone)]
pub struct ValueIter<'a> {
    reader: Reader<'a>,
    left: usize,
}

impl<'a> Iterator for ValueIter<'a> {
    type Item = Option<&'a [u8]>;

    fn next(&mut self) -> Option<Option<&'a [u8]>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // `Values` exists only after `ValueReader::all` checked these reads, so they do not fail.
        match self.reader.i32() {
            Ok(-1) => Some(None),
            Ok(len) => self.reader.bytes(len).ok().map(Some),
            Err(_) => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for ValueIter<'_> {}

/// Reads values one by one, as `exec_bind_message` and `parse_fcall_arguments` do. A `Bind` reads a length below -1 as a length that is too long, and a `FunctionCall` has its own text for it. After an error the reader gives no more values.
#[derive(Debug, Clone)]
struct ValueReader<'a> {
    reader: Reader<'a>,
    left: usize,
    function_call: bool,
}

impl<'a> ValueReader<'a> {
    fn next(&mut self) -> Option<Result<Option<&'a [u8]>, ProtocolError>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let value = self.read();
        if value.is_err() {
            self.left = 0;
        }
        Some(value)
    }

    fn read(&mut self) -> Result<Option<&'a [u8]>, ProtocolError> {
        let len = self.reader.i32()?;
        if len == -1 {
            return Ok(None);
        }
        if self.function_call && len < -1 {
            return Err(ProtocolError::error(format!(
                "invalid argument size {len} in function call message"
            )));
        }
        self.reader.bytes(len).map(Some)
    }

    /// Reads the values that the caller did not read, and gives the reader after them.
    fn finish(mut self) -> Result<Reader<'a>, ProtocolError> {
        while let Some(value) = self.next() {
            value?;
        }
        Ok(self.reader)
    }

    /// Reads all the values and gives them as [`Values`].
    fn all(self) -> Result<(Values<'a>, Reader<'a>), ProtocolError> {
        let (start, count) = (self.reader.clone().rest(), self.left);
        let reader = self.finish()?;
        let bytes = &start[..start.len() - reader.remaining()];
        Ok((Values { bytes, count }, reader))
    }
}

/// A `Bind`, with the names read and the rest kept for [`Bind::params`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind<'a> {
    pub portal: &'a [u8],
    pub statement: &'a [u8],
    rest: &'a [u8],
}

/// The rest of a `Bind`, read at one time by [`Bind::body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindBody<'a> {
    pub formats: Formats<'a>,
    pub values: Values<'a>,
    pub result_formats: Formats<'a>,
}

/// The parameters of a `Bind` after the counts are checked. It gives the values one by one, and [`BindParams::finish`] reads the result formats and the end of the message.
#[derive(Debug, Clone)]
pub struct BindParams<'a> {
    pub formats: Formats<'a>,
    values: ValueReader<'a>,
}

impl<'a> Iterator for BindParams<'a> {
    type Item = Result<Option<&'a [u8]>, ProtocolError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.values.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.values.left, Some(self.values.left))
    }
}

impl ExactSizeIterator for BindParams<'_> {}

impl<'a> BindParams<'a> {
    /// Reads the values that are left, the result formats and the end of the message.
    ///
    /// # Errors
    ///
    /// The read errors of [`Frontend::parse`].
    pub fn finish(self) -> Result<Formats<'a>, ProtocolError> {
        let mut r = self.values.finish()?;
        result_formats(&mut r)
    }
}

fn result_formats<'a>(r: &mut Reader<'a>) -> Result<Formats<'a>, ProtocolError> {
    let count = r.u16()?;
    let formats = Formats(r.bytes(i32::from(count) * 2)?);
    r.end()?;
    Ok(formats)
}

impl<'a> Bind<'a> {
    /// Reads the parameter formats and the number of values, and checks them.
    ///
    /// `params` is the number of parameters of the statement. PostgreSQL checks the counts after it reads the formats, then it checks for an aborted transaction and makes the portal, and only then it reads the values, and it converts each value before it reads the next. The server does the same steps with the [`BindParams`] that this gives, so a `Bind` gets the same first error from rupg as from PostgreSQL.
    ///
    /// # Errors
    ///
    /// The text of `exec_bind_message` for counts that do not agree, and the read errors of [`Frontend::parse`].
    pub fn params(&self, params: usize) -> Result<BindParams<'a>, ProtocolError> {
        let mut r = Reader::new(self.rest);
        let count = r.u16()?;
        let formats = Formats(r.bytes(i32::from(count) * 2)?);
        let values = usize::from(r.u16()?);
        if formats.len() > 1 && formats.len() != values {
            return Err(ProtocolError::error(format!(
                "bind message has {} parameter formats but {values} parameters",
                formats.len()
            )));
        }
        if values != params {
            return Err(ProtocolError::error(format!(
                "bind message supplies {values} parameters, but prepared statement \"{}\" requires {params}",
                String::from_utf8_lossy(self.statement)
            )));
        }
        let values = ValueReader { reader: r, left: values, function_call: false };
        Ok(BindParams { formats, values })
    }

    /// Reads the formats, the values and the result formats at one time, with the checks of [`Bind::params`]. It checks the length of every value before the server converts any of them, so a `Bind` with two bad values can get a different first error than from PostgreSQL. A client that follows the protocol cannot send such a message.
    ///
    /// # Errors
    ///
    /// The errors of [`Bind::params`] and [`BindParams::finish`].
    pub fn body(&self, params: usize) -> Result<BindBody<'a>, ProtocolError> {
        let BindParams { formats, values } = self.params(params)?;
        let (values, mut r) = values.all()?;
        let result_formats = result_formats(&mut r)?;
        Ok(BindBody { formats, values, result_formats })
    }

    /// Writes a whole `Bind` as a client sends it.
    pub fn encode(
        out: &mut Vec<u8>,
        portal: &[u8],
        statement: &[u8],
        formats: &[i16],
        values: &[Option<&[u8]>],
        result_formats: &[i16],
    ) {
        let mut rest = Vec::new();
        encode_formats(&mut rest, formats);
        encode_values(&mut rest, values);
        encode_formats(&mut rest, result_formats);
        Frontend::Bind(Bind { portal, statement, rest: &rest }).encode(out);
    }
}

/// A `FunctionCall`, with the OID read and the rest kept for [`FunctionCall::args`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionCall<'a> {
    pub oid: u32,
    rest: &'a [u8],
}

/// The rest of a `FunctionCall`, read at one time by [`FunctionCall::body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionCallBody<'a> {
    pub formats: Formats<'a>,
    pub args: Values<'a>,
    pub result_format: i16,
}

/// The arguments of a `FunctionCall` after the counts are checked. It gives the arguments one by one, and [`CallArgs::finish`] reads the result format and the end of the message.
#[derive(Debug, Clone)]
pub struct CallArgs<'a> {
    pub formats: Formats<'a>,
    args: ValueReader<'a>,
}

impl<'a> Iterator for CallArgs<'a> {
    type Item = Result<Option<&'a [u8]>, ProtocolError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.args.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.args.left, Some(self.args.left))
    }
}

impl ExactSizeIterator for CallArgs<'_> {}

impl CallArgs<'_> {
    /// Reads the arguments that are left, the result format and the end of the message.
    ///
    /// # Errors
    ///
    /// The texts of `parse_fcall_arguments`, and the read errors of [`Frontend::parse`].
    pub fn finish(self) -> Result<i16, ProtocolError> {
        let mut r = self.args.finish()?;
        let format = r.i16()?;
        r.end()?;
        Ok(format)
    }
}

impl<'a> FunctionCall<'a> {
    /// Reads the argument formats and the number of arguments, and checks them.
    ///
    /// `args` is the number of arguments of the function. `parse_fcall_arguments` in `src/backend/tcop/fastpath.c` checks the counts after the lookup of the function and the permission checks, and it converts each argument before it reads the next.
    ///
    /// # Errors
    ///
    /// The texts of `parse_fcall_arguments`, and the read errors of [`Frontend::parse`].
    pub fn args(&self, args: usize) -> Result<CallArgs<'a>, ProtocolError> {
        let mut r = Reader::new(self.rest);
        let count = r.u16()?;
        let formats = Formats(r.bytes(i32::from(count) * 2)?);
        let given = usize::from(r.u16()?);
        if given != args {
            return Err(ProtocolError::error(format!(
                "function call message contains {given} arguments but function requires {args}"
            )));
        }
        if formats.len() > 1 && formats.len() != given {
            return Err(ProtocolError::error(format!(
                "function call message contains {} argument formats but {given} arguments",
                formats.len()
            )));
        }
        let args = ValueReader { reader: r, left: given, function_call: true };
        Ok(CallArgs { formats, args })
    }

    /// Reads the formats, the arguments and the result format at one time, with the checks of [`FunctionCall::args`].
    ///
    /// # Errors
    ///
    /// The errors of [`FunctionCall::args`] and [`CallArgs::finish`].
    pub fn body(&self, args: usize) -> Result<FunctionCallBody<'a>, ProtocolError> {
        let CallArgs { formats, args } = self.args(args)?;
        let (args, mut r) = args.all()?;
        let result_format = r.i16()?;
        r.end()?;
        Ok(FunctionCallBody { formats, args, result_format })
    }

    /// Writes a whole `FunctionCall` as a client sends it.
    pub fn encode(
        out: &mut Vec<u8>,
        oid: u32,
        formats: &[i16],
        args: &[Option<&[u8]>],
        result_format: i16,
    ) {
        let mut rest = Vec::new();
        encode_formats(&mut rest, formats);
        encode_values(&mut rest, args);
        rest.extend_from_slice(&result_format.to_be_bytes());
        Frontend::FunctionCall(FunctionCall { oid, rest: &rest }).encode(out);
    }
}

fn encode_formats(out: &mut Vec<u8>, formats: &[i16]) {
    out.extend_from_slice(&(formats.len() as u16).to_be_bytes());
    for format in formats {
        out.extend_from_slice(&format.to_be_bytes());
    }
}

fn encode_values(out: &mut Vec<u8>, values: &[Option<&[u8]>]) {
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for value in values {
        match value {
            Some(bytes) => {
                out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                out.extend_from_slice(bytes);
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
}

/// Writes the type OIDs of a `Parse` for [`Frontend::encode`].
pub fn encode_oids(types: &[u32]) -> Vec<u8> {
    types.iter().flat_map(|oid| oid.to_be_bytes()).collect()
}

impl<'a> Oids<'a> {
    /// Type OIDs from bytes that [`encode_oids`] wrote.
    ///
    /// # Panics
    ///
    /// When the length of `bytes` is not a multiple of 4.
    pub fn from_bytes(bytes: &'a [u8]) -> Oids<'a> {
        assert!(bytes.len().is_multiple_of(4), "type OIDs take 4 bytes each");
        Oids(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Level;
    use crate::frame::{Mode, split};

    fn round_trip(message: Frontend<'_>) -> Vec<u8> {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        let mode = if message.tag() == b'p' { Mode::Password } else { Mode::Query };
        let frame = split(&bytes, mode).unwrap().unwrap();
        assert_eq!(frame.size(), bytes.len());
        assert_eq!(Frontend::parse(frame), Ok(message));
        bytes
    }

    fn parse(tag: u8, body: &[u8]) -> Result<Frontend<'_>, ProtocolError> {
        Frontend::parse(Frame { tag, body })
    }

    #[test]
    fn every_message_round_trips() {
        let oids = encode_oids(&[23, 0, 25]);
        let messages = [
            Frontend::Query(b"select 1"),
            Frontend::Query(b""),
            Frontend::Parse { name: b"s1", sql: b"select $1", types: Oids::from_bytes(&oids) },
            Frontend::Parse { name: b"", sql: b"", types: Oids::from_bytes(&[]) },
            Frontend::Describe { target: Target::Statement, name: b"s1" },
            Frontend::Describe { target: Target::Portal, name: b"" },
            Frontend::Execute { portal: b"p1", max_rows: 0 },
            Frontend::Execute { portal: b"", max_rows: -5 },
            Frontend::Close { target: Target::Statement, name: b"s1" },
            Frontend::Close { target: Target::Portal, name: b"p1" },
            Frontend::Sync,
            Frontend::Flush,
            Frontend::Terminate,
            Frontend::CopyData(b"1\t2\n"),
            Frontend::CopyData(b""),
            Frontend::CopyDone,
            Frontend::CopyFail(b"no more"),
            Frontend::Password(b"md5abc\0"),
        ];
        for message in messages {
            round_trip(message);
        }
        let Frontend::Parse { types, .. } = messages[2] else { unreachable!() };
        assert_eq!(types.iter().collect::<Vec<_>>(), [23, 0, 25]);
    }

    #[test]
    fn bind_reads_its_body_after_the_names() {
        let mut bytes = Vec::new();
        Bind::encode(
            &mut bytes,
            b"p1",
            b"s1",
            &[1],
            &[Some(b"\0\0\0\x07"), None, Some(b"")],
            &[0, 1],
        );
        let frame = split(&bytes, Mode::Query).unwrap().unwrap();
        let Ok(Frontend::Bind(bind)) = Frontend::parse(frame) else { panic!() };
        assert_eq!((bind.portal, bind.statement), (&b"p1"[..], &b"s1"[..]));
        let body = bind.body(3).unwrap();
        assert_eq!(body.formats.iter().collect::<Vec<_>>(), [1]);
        assert_eq!((body.formats.of(0), body.formats.of(2)), (1, 1));
        let values: Vec<_> = body.values.iter().collect();
        assert_eq!(values, [Some(&b"\0\0\0\x07"[..]), None, Some(&b""[..])]);
        assert_eq!(body.result_formats.iter().collect::<Vec<_>>(), [0, 1]);
        assert_eq!((body.result_formats.of(1), body.result_formats.of(9)), (1, 0));
        round_trip(Frontend::Bind(bind));

        let error = bind.body(2).unwrap_err().message;
        assert_eq!(
            error,
            "bind message supplies 3 parameters, but prepared statement \"s1\" requires 2"
        );
        let mut bytes = Vec::new();
        Bind::encode(&mut bytes, b"", b"", &[0, 1], &[None], &[]);
        let Ok(Frontend::Bind(bind)) =
            Frontend::parse(split(&bytes, Mode::Query).unwrap().unwrap())
        else {
            panic!()
        };
        let error = bind.body(1).unwrap_err().message;
        assert_eq!(error, "bind message has 2 parameter formats but 1 parameters");
    }

    #[test]
    fn a_bind_value_length_below_minus_one_is_too_long() {
        let mut rest = vec![0, 0, 0, 1];
        rest.extend_from_slice(&(-2i32).to_be_bytes());
        rest.extend_from_slice(&[0, 0]);
        let bind = Bind { portal: b"", statement: b"", rest: &rest };
        assert_eq!(bind.body(1).unwrap_err().message, "insufficient data left in message");
        let mut rest = vec![0, 0, 0, 1];
        rest.extend_from_slice(&(-1i32).to_be_bytes());
        rest.extend_from_slice(&[0, 0, 9]);
        let bind = Bind { portal: b"", statement: b"", rest: &rest };
        assert_eq!(bind.body(1).unwrap_err().message, "invalid message format");
    }

    #[test]
    fn bind_params_give_the_values_one_by_one() {
        // A text value that the server cannot convert, then a length past the end. PostgreSQL converts the first value before it reads the second, so the server must see the first value before the read error.
        let mut rest = vec![0, 0, 0, 2];
        rest.extend_from_slice(&2i32.to_be_bytes());
        rest.extend_from_slice(b"xy");
        rest.extend_from_slice(&50i32.to_be_bytes());
        let bind = Bind { portal: b"", statement: b"", rest: &rest };
        let mut params = bind.params(2).unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params.next(), Some(Ok(Some(&b"xy"[..]))));
        let error = params.next().unwrap().unwrap_err();
        assert_eq!(error.message, "insufficient data left in message");
        assert_eq!(params.next(), None);
        assert_eq!(bind.body(2).unwrap_err(), error);

        // `finish` reads the values that the server did not take.
        let mut bytes = Vec::new();
        Bind::encode(&mut bytes, b"", b"", &[], &[Some(b"1"), None], &[1]);
        let Ok(Frontend::Bind(bind)) =
            Frontend::parse(split(&bytes, Mode::Query).unwrap().unwrap())
        else {
            panic!()
        };
        let mut params = bind.params(2).unwrap();
        assert_eq!(params.next(), Some(Ok(Some(&b"1"[..]))));
        assert_eq!(params.finish().unwrap().iter().collect::<Vec<_>>(), [1]);
        assert_eq!(bind.params(2).unwrap().finish().unwrap().of(0), 1);
        let body = bind.body(2).unwrap();
        assert_eq!(body.values.iter().collect::<Vec<_>>(), [Some(&b"1"[..]), None]);
    }

    #[test]
    fn call_args_give_the_arguments_one_by_one() {
        let mut bytes = Vec::new();
        FunctionCall::encode(&mut bytes, 1598, &[], &[Some(b"ab"), None], 1);
        let Ok(Frontend::FunctionCall(call)) =
            Frontend::parse(split(&bytes, Mode::Query).unwrap().unwrap())
        else {
            panic!()
        };
        let mut args = call.args(2).unwrap();
        assert_eq!(args.next(), Some(Ok(Some(&b"ab"[..]))));
        assert_eq!(args.finish(), Ok(1));
        let mut rest = vec![0, 0, 0, 2, 0, 0, 0, 0];
        rest.extend_from_slice(&(-3i32).to_be_bytes());
        let call = FunctionCall { oid: 1, rest: &rest };
        let mut args = call.args(2).unwrap();
        assert_eq!(args.next(), Some(Ok(Some(&b""[..]))));
        let error = args.next().unwrap().unwrap_err().message;
        assert_eq!(error, "invalid argument size -3 in function call message");
    }

    #[test]
    fn function_call_reads_its_body_after_the_lookup() {
        let mut bytes = Vec::new();
        FunctionCall::encode(&mut bytes, 1598, &[1], &[Some(b"ab"), None], 1);
        let frame = split(&bytes, Mode::Query).unwrap().unwrap();
        let Ok(Frontend::FunctionCall(call)) = Frontend::parse(frame) else { panic!() };
        assert_eq!(call.oid, 1598);
        let body = call.body(2).unwrap();
        assert_eq!(body.args.iter().collect::<Vec<_>>(), [Some(&b"ab"[..]), None]);
        assert_eq!(body.result_format, 1);
        round_trip(Frontend::FunctionCall(call));
        let error = call.body(1).unwrap_err().message;
        assert_eq!(error, "function call message contains 2 arguments but function requires 1");

        let mut rest = vec![0, 0, 0, 1];
        rest.extend_from_slice(&(-7i32).to_be_bytes());
        let call = FunctionCall { oid: 1, rest: &rest };
        assert_eq!(
            call.body(1).unwrap_err().message,
            "invalid argument size -7 in function call message"
        );
        let call = FunctionCall { oid: 1, rest: &[0, 2, 0, 0, 0, 0, 0, 1] };
        let error = call.body(1).unwrap_err().message;
        assert_eq!(error, "function call message contains 2 argument formats but 1 arguments");
    }

    #[test]
    fn bad_bodies_fail_with_the_text_of_postgres() {
        let cases: [(u8, &[u8], &str); 12] = [
            (b'Q', b"select 1", "invalid string in message"),
            (b'Q', b"select 1\0x", "invalid message format"),
            (b'P', b"s\0select 1\0", "insufficient data left in message"),
            (b'P', b"s\0select 1\0\0\x01\0\0", "insufficient data left in message"),
            (b'B', b"p\0", "invalid string in message"),
            (b'D', b"", "no data left in message"),
            (b'D', b"X\0", "invalid DESCRIBE message subtype 88"),
            (b'C', b"x\0", "invalid CLOSE message subtype 120"),
            (b'C', b"S\0\0", "invalid message format"),
            (b'E', b"p\0\0\0", "insufficient data left in message"),
            (b'S', b"\0", "invalid message format"),
            (b'H', b"\0", "invalid message format"),
        ];
        for (tag, body, text) in cases {
            let error = parse(tag, body).unwrap_err();
            assert_eq!((error.level, error.message.as_str()), (Level::Error, text), "{body:?}");
        }
        // PostgreSQL does not read the body of these.
        assert_eq!(parse(b'X', b"junk"), Ok(Frontend::Terminate));
        assert_eq!(parse(b'c', b"junk"), Ok(Frontend::CopyDone));
        let error = parse(b'Z', b"").unwrap_err();
        assert_eq!(
            (error.level, error.message.as_str()),
            (Level::Fatal, "invalid frontend message type 90")
        );
    }
}

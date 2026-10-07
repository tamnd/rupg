//! The messages that the server sends.
//!
//! The server writes every message into one [`OutBuf`] for each connection, and the transport sends the bytes when the protocol says it must flush. A message is not a value that the server builds and then writes: each method of `OutBuf` writes the bytes of one message in place, so a result of a million rows does not make a million small allocations.
//!
//! [`Backend`] is the same set of messages as values. The server does not need it. The tests use it to read back what `OutBuf` wrote, and a client in `rupg-compat` or a fuzz target can use it to read a server.
//!
//! Lifted from `crates/rudb-pgwire/src/backend.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::{Level, ProtocolError};
use crate::reader::Reader;

/// The transaction status in `ReadyForQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// `I`: not in a transaction block.
    Idle,
    /// `T`: in a transaction block.
    Block,
    /// `E`: in a failed transaction block, which only ends with a rollback.
    Failed,
}

impl TransactionStatus {
    pub fn byte(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::Block => b'T',
            TransactionStatus::Failed => b'E',
        }
    }
}

/// One column of a `RowDescription`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field<'a> {
    pub name: &'a [u8],
    /// The OID of the table, or 0.
    pub table: u32,
    /// The attribute number of the column in the table, or 0.
    pub column: i16,
    pub type_oid: u32,
    /// `typlen`: the size of the type, or -1 and -2 for the variable sizes.
    pub type_size: i16,
    pub type_modifier: i32,
    /// 0 for text and 1 for binary.
    pub format: i16,
}

/// The place where a message started in an [`OutBuf`], so that [`OutBuf::finish`] can write the length when the body is complete.
#[derive(Debug)]
#[must_use = "a message without finish has no length"]
pub struct Mark(usize);

/// The output of one connection.
#[derive(Debug, Default)]
pub struct OutBuf {
    bytes: Vec<u8>,
}

impl OutBuf {
    pub fn new() -> OutBuf {
        OutBuf::default()
    }

    /// The bytes to send.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes, for a writer that adds whole messages itself, such as the `DataRow` encoder of `rupg-types`.
    pub fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Removes the first `n` bytes, after the transport sent them. The capacity stays, so the buffer does not allocate again for the next result of the same size.
    pub fn consume(&mut self, n: usize) {
        self.bytes.drain(..n.min(self.bytes.len()));
    }

    /// Starts a message of type `tag`. The body follows with the `put_` methods, and [`OutBuf::finish`] ends it.
    pub fn begin(&mut self, tag: u8) -> Mark {
        let mark = Mark(self.bytes.len());
        self.bytes.push(tag);
        self.bytes.extend_from_slice(&[0; 4]);
        mark
    }

    /// Ends the message that `mark` started, and writes its length.
    pub fn finish(&mut self, mark: Mark) {
        let len = (self.bytes.len() - mark.0 - 1) as u32;
        self.bytes[mark.0 + 1..mark.0 + 5].copy_from_slice(&len.to_be_bytes());
    }

    pub fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn put_i16(&mut self, value: i16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn put_i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn put_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn put_bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    /// A string and its terminating zero byte.
    pub fn put_string(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
        self.bytes.push(0);
    }

    /// A length, or -1 for null, and the bytes.
    pub fn put_value(&mut self, value: Option<&[u8]>) {
        match value {
            Some(bytes) => {
                self.put_i32(bytes.len() as i32);
                self.put_bytes(bytes);
            }
            None => self.put_i32(-1),
        }
    }

    fn empty(&mut self, tag: u8) {
        self.bytes.extend_from_slice(&[tag, 0, 0, 0, 4]);
    }

    fn authentication(&mut self, code: i32, data: &[u8]) {
        let mark = self.begin(b'R');
        self.put_i32(code);
        self.put_bytes(data);
        self.finish(mark);
    }

    /// The one byte that answers `SSLRequest` or `GSSENCRequest`: `S` or `G` to go on with encryption, `N` to refuse it. It has no frame.
    pub fn encryption_response(&mut self, answer: u8) {
        self.bytes.push(answer);
    }

    pub fn authentication_ok(&mut self) {
        self.authentication(0, &[]);
    }

    pub fn authentication_cleartext_password(&mut self) {
        self.authentication(3, &[]);
    }

    pub fn authentication_md5_password(&mut self, salt: [u8; 4]) {
        self.authentication(5, &salt);
    }

    pub fn authentication_gss(&mut self) {
        self.authentication(7, &[]);
    }

    pub fn authentication_gss_continue(&mut self, data: &[u8]) {
        self.authentication(8, data);
    }

    pub fn authentication_sspi(&mut self) {
        self.authentication(9, &[]);
    }

    /// `AuthenticationSASL` with the names of the mechanisms the server offers.
    pub fn authentication_sasl(&mut self, mechanisms: &[&[u8]]) {
        let mark = self.begin(b'R');
        self.put_i32(10);
        for mechanism in mechanisms {
            self.put_string(mechanism);
        }
        self.put_u8(0);
        self.finish(mark);
    }

    pub fn authentication_sasl_continue(&mut self, data: &[u8]) {
        self.authentication(11, data);
    }

    pub fn authentication_sasl_final(&mut self, data: &[u8]) {
        self.authentication(12, data);
    }

    /// `BackendKeyData`. The key is 4 bytes in protocol 3.0 and up to 256 in 3.2.
    pub fn backend_key_data(&mut self, pid: i32, key: &[u8]) {
        let mark = self.begin(b'K');
        self.put_i32(pid);
        self.put_bytes(key);
        self.finish(mark);
    }

    pub fn parameter_status(&mut self, name: &[u8], value: &[u8]) {
        let mark = self.begin(b'S');
        self.put_string(name);
        self.put_string(value);
        self.finish(mark);
    }

    /// `NegotiateProtocolVersion`: the version of the session and the `_pq_.` options the server does not know. The version is the whole 32-bit code, for example [`PROTOCOL_3_2`], and not only the minor part. PostgreSQL sends the lower of the version of the client and 3.2.
    ///
    /// [`PROTOCOL_3_2`]: crate::PROTOCOL_3_2
    pub fn negotiate_protocol_version(&mut self, version: u32, options: &[&[u8]]) {
        let mark = self.begin(b'v');
        self.put_u32(version);
        self.put_u32(options.len() as u32);
        for option in options {
            self.put_string(option);
        }
        self.finish(mark);
    }

    pub fn ready_for_query(&mut self, status: TransactionStatus) {
        self.bytes.extend_from_slice(&[b'Z', 0, 0, 0, 5, status.byte()]);
    }

    pub fn parse_complete(&mut self) {
        self.empty(b'1');
    }

    pub fn bind_complete(&mut self) {
        self.empty(b'2');
    }

    pub fn close_complete(&mut self) {
        self.empty(b'3');
    }

    pub fn no_data(&mut self) {
        self.empty(b'n');
    }

    pub fn empty_query_response(&mut self) {
        self.empty(b'I');
    }

    pub fn portal_suspended(&mut self) {
        self.empty(b's');
    }

    pub fn copy_done(&mut self) {
        self.empty(b'c');
    }

    /// `CommandComplete` with a tag such as `SELECT 1` or `INSERT 0 5`.
    pub fn command_complete(&mut self, tag: &[u8]) {
        let mark = self.begin(b'C');
        self.put_string(tag);
        self.finish(mark);
    }

    pub fn row_description(&mut self, fields: &[Field<'_>]) {
        let mark = self.begin(b'T');
        self.put_i16(fields.len() as i16);
        for field in fields {
            self.put_string(field.name);
            self.put_u32(field.table);
            self.put_i16(field.column);
            self.put_u32(field.type_oid);
            self.put_i16(field.type_size);
            self.put_i32(field.type_modifier);
            self.put_i16(field.format);
        }
        self.finish(mark);
    }

    /// One `DataRow`. A result of many rows does not come through here: the vector encoder of `rupg-types` writes the rows of a whole vector with the `put_` methods.
    pub fn data_row(&mut self, values: &[Option<&[u8]>]) {
        let mark = self.begin(b'D');
        self.put_i16(values.len() as i16);
        for value in values {
            self.put_value(*value);
        }
        self.finish(mark);
    }

    pub fn parameter_description(&mut self, types: &[u32]) {
        let mark = self.begin(b't');
        self.put_i16(types.len() as i16);
        for oid in types {
            self.put_u32(*oid);
        }
        self.finish(mark);
    }

    /// `ErrorResponse`, from fields that are each a code byte such as `S`, `C` or `M` and a value.
    pub fn error_response(&mut self, fields: &[(u8, &[u8])]) {
        self.notice_fields(b'E', fields);
    }

    /// The answer to a [`ProtocolError`], in the format of `protocol`, the version of the session. A [`Level::Log`] error writes nothing.
    ///
    /// The format is the one of `send_message_to_frontend` in PostgreSQL. A session of major version 3, and a connection that has not sent a version yet, get an `ErrorResponse`. A client that asked for an older major version gets the old format: the type byte, the text with a zero byte at the end, and no length. That client only ever sees the error that rejects its version.
    pub fn protocol_error(&mut self, error: &ProtocolError, protocol: u32) {
        let severity: &[u8] = match error.level {
            Level::Error => b"ERROR",
            Level::Fatal => b"FATAL",
            Level::Log => return,
        };
        if protocol >> 16 < 3 && protocol != 0 {
            self.bytes.push(b'E');
            self.bytes.extend_from_slice(severity);
            self.bytes.extend_from_slice(b":  ");
            self.bytes.extend_from_slice(error.message.as_bytes());
            self.bytes.extend_from_slice(b"\n\0");
            return;
        }
        let mut fields = vec![
            (b'S', severity),
            (b'V', severity),
            (b'C', error.sqlstate.as_bytes()),
            (b'M', error.message.as_bytes()),
        ];
        if let Some(detail) = &error.detail {
            fields.push((b'D', detail.as_bytes()));
        }
        if let Some(hint) = error.hint {
            fields.push((b'H', hint.as_bytes()));
        }
        self.error_response(&fields);
    }

    /// `NoticeResponse`, with the fields of [`OutBuf::error_response`].
    pub fn notice_response(&mut self, fields: &[(u8, &[u8])]) {
        self.notice_fields(b'N', fields);
    }

    fn notice_fields(&mut self, tag: u8, fields: &[(u8, &[u8])]) {
        let mark = self.begin(tag);
        for (code, value) in fields {
            self.put_u8(*code);
            self.put_string(value);
        }
        self.put_u8(0);
        self.finish(mark);
    }

    pub fn notification_response(&mut self, pid: i32, channel: &[u8], payload: &[u8]) {
        let mark = self.begin(b'A');
        self.put_i32(pid);
        self.put_string(channel);
        self.put_string(payload);
        self.finish(mark);
    }

    /// `CopyInResponse`. `format` is 0 for text and 1 for binary, and `columns` has the format of each column.
    pub fn copy_in_response(&mut self, format: i8, columns: &[i16]) {
        self.copy_response(b'G', format, columns);
    }

    pub fn copy_out_response(&mut self, format: i8, columns: &[i16]) {
        self.copy_response(b'H', format, columns);
    }

    pub fn copy_both_response(&mut self, format: i8, columns: &[i16]) {
        self.copy_response(b'W', format, columns);
    }

    fn copy_response(&mut self, tag: u8, format: i8, columns: &[i16]) {
        let mark = self.begin(tag);
        self.put_u8(format as u8);
        self.put_i16(columns.len() as i16);
        for column in columns {
            self.put_i16(*column);
        }
        self.finish(mark);
    }

    pub fn copy_data(&mut self, data: &[u8]) {
        let mark = self.begin(b'd');
        self.put_bytes(data);
        self.finish(mark);
    }

    pub fn function_call_response(&mut self, result: Option<&[u8]>) {
        let mark = self.begin(b'V');
        self.put_value(result);
        self.finish(mark);
    }
}

/// The subtypes of `Authentication`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authentication<'a> {
    Ok,
    CleartextPassword,
    Md5Password([u8; 4]),
    Gss,
    GssContinue(&'a [u8]),
    Sspi,
    Sasl(Vec<&'a [u8]>),
    SaslContinue(&'a [u8]),
    SaslFinal(&'a [u8]),
}

/// One message from the server, as a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend<'a> {
    Authentication(Authentication<'a>),
    BackendKeyData { pid: i32, key: &'a [u8] },
    BindComplete,
    CloseComplete,
    CommandComplete(&'a [u8]),
    CopyData(&'a [u8]),
    CopyDone,
    CopyInResponse { format: i8, columns: Vec<i16> },
    CopyOutResponse { format: i8, columns: Vec<i16> },
    CopyBothResponse { format: i8, columns: Vec<i16> },
    DataRow(Vec<Option<&'a [u8]>>),
    EmptyQueryResponse,
    ErrorResponse(Vec<(u8, &'a [u8])>),
    FunctionCallResponse(Option<&'a [u8]>),
    NegotiateProtocolVersion { version: u32, options: Vec<&'a [u8]> },
    NoData,
    NoticeResponse(Vec<(u8, &'a [u8])>),
    NotificationResponse { pid: i32, channel: &'a [u8], payload: &'a [u8] },
    ParameterDescription(Vec<u32>),
    ParameterStatus { name: &'a [u8], value: &'a [u8] },
    ParseComplete,
    PortalSuspended,
    ReadyForQuery(TransactionStatus),
    RowDescription(Vec<Field<'a>>),
}

impl<'a> Backend<'a> {
    /// Reads the first message in `input`, and gives it with the number of bytes it takes. It gives `Ok(None)` when `input` does not hold all of the message yet.
    ///
    /// # Errors
    ///
    /// A length below 4, an unknown type byte, or a body that does not have the format of its type.
    pub fn decode(input: &'a [u8]) -> Result<Option<(Backend<'a>, usize)>, ProtocolError> {
        let Some(head) = input.get(..5) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len < 4 {
            return Err(ProtocolError::fatal("invalid message length"));
        }
        let Some(body) = input.get(5..1 + len) else {
            return Ok(None);
        };
        Ok(Some((Backend::parse(head[0], body)?, 1 + len)))
    }

    fn parse(tag: u8, body: &'a [u8]) -> Result<Backend<'a>, ProtocolError> {
        let mut r = Reader::new(body);
        let message = match tag {
            b'R' => Backend::Authentication(match r.i32()? {
                0 => Authentication::Ok,
                3 => Authentication::CleartextPassword,
                5 => {
                    let salt = r.bytes(4)?;
                    Authentication::Md5Password([salt[0], salt[1], salt[2], salt[3]])
                }
                7 => Authentication::Gss,
                8 => Authentication::GssContinue(r.rest()),
                9 => Authentication::Sspi,
                10 => {
                    let mut mechanisms = Vec::new();
                    loop {
                        let name = r.string()?;
                        if name.is_empty() {
                            break;
                        }
                        mechanisms.push(name);
                    }
                    Authentication::Sasl(mechanisms)
                }
                11 => Authentication::SaslContinue(r.rest()),
                12 => Authentication::SaslFinal(r.rest()),
                code => {
                    return Err(ProtocolError::fatal(format!(
                        "unknown authentication request {code}"
                    )));
                }
            }),
            b'K' => Backend::BackendKeyData { pid: r.i32()?, key: r.rest() },
            b'2' => Backend::BindComplete,
            b'3' => Backend::CloseComplete,
            b'C' => Backend::CommandComplete(r.string()?),
            b'd' => Backend::CopyData(r.rest()),
            b'c' => Backend::CopyDone,
            b'G' | b'H' | b'W' => {
                let format = r.byte()? as i8;
                let count = r.u16()?;
                let columns = (0..count).map(|_| r.i16()).collect::<Result<_, _>>()?;
                match tag {
                    b'G' => Backend::CopyInResponse { format, columns },
                    b'H' => Backend::CopyOutResponse { format, columns },
                    _ => Backend::CopyBothResponse { format, columns },
                }
            }
            b'D' => {
                let count = r.u16()?;
                let values = (0..count)
                    .map(|_| match r.i32()? {
                        -1 => Ok(None),
                        len => r.bytes(len).map(Some),
                    })
                    .collect::<Result<_, _>>()?;
                Backend::DataRow(values)
            }
            b'I' => Backend::EmptyQueryResponse,
            b'E' | b'N' => {
                let mut fields = Vec::new();
                loop {
                    let code = r.byte()?;
                    if code == 0 {
                        break;
                    }
                    fields.push((code, r.string()?));
                }
                if tag == b'E' {
                    Backend::ErrorResponse(fields)
                } else {
                    Backend::NoticeResponse(fields)
                }
            }
            b'V' => Backend::FunctionCallResponse(match r.i32()? {
                -1 => None,
                len => Some(r.bytes(len)?),
            }),
            b'v' => {
                let version = r.u32()?;
                let count = r.u32()?;
                let options = (0..count).map(|_| r.string()).collect::<Result<_, _>>()?;
                Backend::NegotiateProtocolVersion { version, options }
            }
            b'n' => Backend::NoData,
            b'A' => Backend::NotificationResponse {
                pid: r.i32()?,
                channel: r.string()?,
                payload: r.string()?,
            },
            b't' => {
                let count = r.u16()?;
                Backend::ParameterDescription(
                    (0..count).map(|_| r.u32()).collect::<Result<_, _>>()?,
                )
            }
            b'S' => Backend::ParameterStatus { name: r.string()?, value: r.string()? },
            b'1' => Backend::ParseComplete,
            b's' => Backend::PortalSuspended,
            b'Z' => Backend::ReadyForQuery(match r.byte()? {
                b'I' => TransactionStatus::Idle,
                b'T' => TransactionStatus::Block,
                b'E' => TransactionStatus::Failed,
                status => {
                    return Err(ProtocolError::fatal(format!(
                        "unknown transaction status {status}"
                    )));
                }
            }),
            b'T' => {
                let count = r.u16()?;
                let mut fields = Vec::with_capacity(usize::from(count));
                for _ in 0..count {
                    fields.push(Field {
                        name: r.string()?,
                        table: r.u32()?,
                        column: r.i16()?,
                        type_oid: r.u32()?,
                        type_size: r.i16()?,
                        type_modifier: r.i32()?,
                        format: r.i16()?,
                    });
                }
                Backend::RowDescription(fields)
            }
            tag => {
                return Err(ProtocolError::fatal(format!("invalid backend message type {tag}")));
            }
        };
        r.end()?;
        Ok(message)
    }

    /// Writes the message with the method of [`OutBuf`] for its type.
    pub fn encode(&self, out: &mut OutBuf) {
        match self {
            Backend::Authentication(auth) => match auth {
                Authentication::Ok => out.authentication_ok(),
                Authentication::CleartextPassword => out.authentication_cleartext_password(),
                Authentication::Md5Password(salt) => out.authentication_md5_password(*salt),
                Authentication::Gss => out.authentication_gss(),
                Authentication::GssContinue(data) => out.authentication_gss_continue(data),
                Authentication::Sspi => out.authentication_sspi(),
                Authentication::Sasl(mechanisms) => out.authentication_sasl(mechanisms),
                Authentication::SaslContinue(data) => out.authentication_sasl_continue(data),
                Authentication::SaslFinal(data) => out.authentication_sasl_final(data),
            },
            Backend::BackendKeyData { pid, key } => out.backend_key_data(*pid, key),
            Backend::BindComplete => out.bind_complete(),
            Backend::CloseComplete => out.close_complete(),
            Backend::CommandComplete(tag) => out.command_complete(tag),
            Backend::CopyData(data) => out.copy_data(data),
            Backend::CopyDone => out.copy_done(),
            Backend::CopyInResponse { format, columns } => out.copy_in_response(*format, columns),
            Backend::CopyOutResponse { format, columns } => out.copy_out_response(*format, columns),
            Backend::CopyBothResponse { format, columns } => {
                out.copy_both_response(*format, columns);
            }
            Backend::DataRow(values) => out.data_row(values),
            Backend::EmptyQueryResponse => out.empty_query_response(),
            Backend::ErrorResponse(fields) => out.error_response(fields),
            Backend::FunctionCallResponse(result) => out.function_call_response(*result),
            Backend::NegotiateProtocolVersion { version, options } => {
                out.negotiate_protocol_version(*version, options);
            }
            Backend::NoData => out.no_data(),
            Backend::NoticeResponse(fields) => out.notice_response(fields),
            Backend::NotificationResponse { pid, channel, payload } => {
                out.notification_response(*pid, channel, payload);
            }
            Backend::ParameterDescription(types) => out.parameter_description(types),
            Backend::ParameterStatus { name, value } => out.parameter_status(name, value),
            Backend::ParseComplete => out.parse_complete(),
            Backend::PortalSuspended => out.portal_suspended(),
            Backend::ReadyForQuery(status) => out.ready_for_query(*status),
            Backend::RowDescription(fields) => out.row_description(fields),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<Backend<'static>> {
        let field = Field {
            name: b"?column?",
            table: 0,
            column: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        };
        vec![
            Backend::Authentication(Authentication::Ok),
            Backend::Authentication(Authentication::CleartextPassword),
            Backend::Authentication(Authentication::Md5Password([1, 2, 3, 4])),
            Backend::Authentication(Authentication::Gss),
            Backend::Authentication(Authentication::GssContinue(b"token")),
            Backend::Authentication(Authentication::Sspi),
            Backend::Authentication(Authentication::Sasl(vec![
                b"SCRAM-SHA-256-PLUS",
                b"SCRAM-SHA-256",
            ])),
            Backend::Authentication(Authentication::SaslContinue(b"r=abc,s=def,i=4096")),
            Backend::Authentication(Authentication::SaslFinal(b"v=xyz")),
            Backend::BackendKeyData { pid: 77, key: &[9; 32] },
            Backend::BindComplete,
            Backend::CloseComplete,
            Backend::CommandComplete(b"SELECT 1"),
            Backend::CopyData(b"1\t2\n"),
            Backend::CopyDone,
            Backend::CopyInResponse { format: 0, columns: vec![0, 0] },
            Backend::CopyOutResponse { format: 1, columns: vec![1] },
            Backend::CopyBothResponse { format: 0, columns: vec![] },
            Backend::DataRow(vec![Some(b"1"), None, Some(b"")]),
            Backend::EmptyQueryResponse,
            Backend::ErrorResponse(vec![(b'S', b"ERROR"), (b'C', b"42601"), (b'M', b"syntax")]),
            Backend::FunctionCallResponse(Some(b"\0\0\0\x01")),
            Backend::FunctionCallResponse(None),
            Backend::NegotiateProtocolVersion {
                version: crate::PROTOCOL_3_2,
                options: vec![b"_pq_.x"],
            },
            Backend::NoData,
            Backend::NoticeResponse(vec![(b'S', b"NOTICE"), (b'M', b"hello")]),
            Backend::NotificationResponse { pid: 5, channel: b"c", payload: b"" },
            Backend::ParameterDescription(vec![23, 25]),
            Backend::ParameterStatus { name: b"server_version", value: b"19beta4" },
            Backend::ParseComplete,
            Backend::PortalSuspended,
            Backend::ReadyForQuery(TransactionStatus::Idle),
            Backend::ReadyForQuery(TransactionStatus::Block),
            Backend::ReadyForQuery(TransactionStatus::Failed),
            Backend::RowDescription(vec![
                field,
                Field { name: b"", table: 16384, column: 2, ..field },
            ]),
        ]
    }

    #[test]
    fn every_message_round_trips() {
        let mut out = OutBuf::new();
        let messages = messages();
        for message in &messages {
            message.encode(&mut out);
        }
        let mut input = out.as_bytes();
        for message in &messages {
            let (read, size) = Backend::decode(input).unwrap().unwrap();
            assert_eq!(&read, message);
            input = &input[size..];
        }
        assert!(input.is_empty());
    }

    #[test]
    fn the_bytes_are_the_bytes_of_postgres() {
        // From the trace of `SELECT 1` in the libpq_pipeline suite and the protocol document.
        let mut out = OutBuf::new();
        out.row_description(&[Field {
            name: b"?column?",
            table: 0,
            column: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        }]);
        assert_eq!(out.len(), 1 + 33);
        out.consume(out.len());
        out.data_row(&[Some(b"1")]);
        assert_eq!(out.as_bytes(), b"D\0\0\0\x0b\0\x01\0\0\0\x011");
        out.consume(out.len());
        out.command_complete(b"SELECT 1");
        out.ready_for_query(TransactionStatus::Idle);
        assert_eq!(out.as_bytes(), b"C\0\0\0\x0dSELECT 1\0Z\0\0\0\x05I");
        out.consume(6);
        assert_eq!(out.len(), 14);
    }

    #[test]
    fn a_partial_message_waits() {
        let mut out = OutBuf::new();
        out.parameter_status(b"TimeZone", b"UTC");
        let bytes = out.as_bytes();
        for end in 0..bytes.len() {
            assert_eq!(Backend::decode(&bytes[..end]), Ok(None));
        }
        assert!(Backend::decode(b"Z\0\0\0\x05X").is_err());
        assert!(Backend::decode(b"Z\0\0\0\x06II").is_err());
        assert!(Backend::decode(b"Z\0\0\0\x03").is_err());
    }
}

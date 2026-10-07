//! The values of the parameters of a `Bind`, as the loop of `exec_bind_message` reads them: the format code, the check of the encoding, and the input or the receive function of the type of the parameter.
//!
//! The session has no executor yet, so it keeps no value. It runs the function of the type only to give the error of PostgreSQL for a bad value. The types that have a function here are `bool`, `bytea`, `"char"`, `name`, `int2`, `int4`, `int8`, `text`, `varchar`, `bpchar`, `oid`, `json`, `jsonb`, `float4`, `float8`, `numeric` and `uuid`, and the binary form of `date`, `time`, `timetz`, `timestamp`, `timestamptz` and `interval`. The text form of the date and time types needs the `DateStyle` and the time zone of the session, so it comes with the executor. A value of any other type is not checked.

use rupg_common::{Error, SqlState};
use rupg_types::{self as types, Recv, TypeError, oid};

/// The error of a type function as the session sends it.
pub(crate) fn type_error(error: TypeError) -> Error {
    let mut out = Error::new(error.sqlstate, error.message);
    if let Some(detail) = error.detail {
        out = out.with_detail(detail);
    }
    if let Some(hint) = error.hint {
        out = out.with_hint(hint);
    }
    out
}

/// Checks the value of parameter `number`, from 1, of type `type_oid` in `format`. A null value has no check other than the format code.
///
/// # Errors
///
/// `22023` for a format code that is not 0 or 1, and the error of the type function for a bad value.
pub(crate) fn check(
    type_oid: u32,
    format: i16,
    value: Option<&[u8]>,
    number: usize,
) -> Result<(), Error> {
    match format {
        0 => match value {
            Some(value) => text(type_oid, value).map_err(type_error),
            None => Ok(()),
        },
        1 => match value {
            Some(value) => binary(type_oid, value, number).map_err(type_error),
            None => Ok(()),
        },
        _ => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unsupported format code: {format}"),
        )),
    }
}

/// `pg_client_to_server` and the input function of the type.
fn text(type_oid: u32, value: &[u8]) -> Result<(), TypeError> {
    let s = Recv::new(value).text()?;
    match type_oid {
        oid::BOOL => types::bool_in(s).map(drop),
        oid::BYTEA => types::bytea_in(s).map(drop),
        oid::INT2 => types::int2_in(s).map(drop),
        oid::INT4 => types::int4_in(s).map(drop),
        oid::INT8 => types::int8_in(s).map(drop),
        oid::OID => types::oid_in(s).map(drop),
        oid::JSON => types::json_in(s).map(drop),
        oid::JSONB => types::jsonb_in(s).map(drop),
        oid::FLOAT4 => types::float4_in(s).map(drop),
        oid::FLOAT8 => types::float8_in(s).map(drop),
        oid::NUMERIC => types::numeric_in(s, -1).map(drop),
        oid::UUID => types::uuid_in(s).map(drop),
        _ => Ok(()),
    }
}

/// The receive function of the type, which must use all the bytes of the value.
fn binary(type_oid: u32, value: &[u8], number: usize) -> Result<(), TypeError> {
    let mut recv = Recv::new(value);
    match type_oid {
        oid::BOOL => recv.bool().map(drop)?,
        oid::BYTEA => drop(recv.rest()),
        oid::CHAR => recv.byte().map(drop)?,
        oid::NAME => types::name_recv(recv.text()?).map(drop)?,
        oid::INT2 => recv.i16().map(drop)?,
        oid::INT4 => recv.i32().map(drop)?,
        oid::INT8 => recv.i64().map(drop)?,
        oid::TEXT | oid::VARCHAR | oid::BPCHAR => recv.text().map(drop)?,
        oid::OID => recv.u32().map(drop)?,
        oid::JSON => types::json_in(recv.text()?).map(drop)?,
        oid::JSONB => types::jsonb_recv(recv.rest()).map(drop)?,
        oid::FLOAT4 => recv.f32().map(drop)?,
        oid::FLOAT8 => recv.f64().map(drop)?,
        oid::NUMERIC => types::numeric_recv(&mut recv, -1).map(drop)?,
        oid::UUID => recv.uuid().map(drop)?,
        oid::DATE => types::date_recv(&mut recv).map(drop)?,
        oid::TIME => types::time_recv(&mut recv, -1).map(drop)?,
        oid::TIMETZ => types::timetz_recv(&mut recv, -1).map(drop)?,
        oid::TIMESTAMP | oid::TIMESTAMPTZ => types::timestamp_recv(&mut recv, -1).map(drop)?,
        oid::INTERVAL => types::interval_recv(&mut recv, -1).map(drop)?,
        _ => return Ok(()),
    }
    recv.finish(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(type_oid: u32, format: i16, value: &[u8]) -> String {
        check(type_oid, format, Some(value), 1).unwrap_err().message().to_owned()
    }

    #[test]
    fn values() {
        assert!(check(oid::INT4, 0, Some(b"42"), 1).is_ok());
        assert!(check(oid::INT4, 1, Some(&7_i32.to_be_bytes()), 1).is_ok());
        assert!(check(oid::INT4, 0, None, 1).is_ok());
        assert!(check(oid::POINT, 0, Some(b"anything"), 1).is_ok());
        assert_eq!(message(oid::INT4, 0, b"abc"), "invalid input syntax for type integer: \"abc\"");
        assert_eq!(
            message(oid::BOOL, 0, b"maybe"),
            "invalid input syntax for type boolean: \"maybe\""
        );
        assert_eq!(
            message(oid::TEXT, 0, b"a\xff"),
            "invalid byte sequence for encoding \"UTF8\": 0xff"
        );
        assert_eq!(
            message(oid::INT4, 1, &[0, 0, 0, 0, 0]),
            "incorrect binary data format in bind parameter 1"
        );
        assert_eq!(message(oid::INT8, 1, &[0]), "insufficient data left in message");
        let error = check(oid::TEXT, 2, None, 1).unwrap_err();
        assert_eq!(
            (error.state(), error.message()),
            (SqlState::INVALID_PARAMETER_VALUE, "unsupported format code: 2")
        );
    }
}

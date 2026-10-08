//! The input, output, receive and send functions of the types, as `typinput`, `typoutput`, `typreceive` and `typsend` of `pg_type` give them.

use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin;
use rupg_types::{
    self as types, Array, DateTimeInput, NoZones, Recv, RegKind, Value, ZoneAbbrevs, oid,
};

use crate::{Session, bad_value, not_yet, reg, type_error};

/// The OID of `xid`.
const XID: u32 = 28;
/// The OID of `cid`.
const CID: u32 = 29;
/// The category of the array types in `pg_type`.
const ARRAY_CATEGORY: u8 = b'A';

/// The type that holds the values of a type: the base type of a domain, or the type itself.
pub fn base_type(ty: u32) -> u32 {
    let mut ty = ty;
    while let Some(row) = builtin::type_by_oid(ty)
        && row.kind == b'd'
    {
        ty = row.base;
    }
    ty
}

/// The element type and the delimiter of an array type, or `None` for a type that is not an array. `int2vector` and `oidvector` are not arrays here, because they have their own functions.
fn array_of(ty: u32) -> Option<(u32, u8)> {
    if ty == oid::INT2VECTOR || ty == oid::OIDVECTOR {
        return None;
    }
    let row = builtin::type_by_oid(ty)?;
    if row.category != ARRAY_CATEGORY || row.elem == 0 {
        return None;
    }
    let delim = builtin::type_by_oid(row.elem).map_or(b',', |e| e.delim);
    Some((row.elem, delim))
}

/// True when the engine has the output and the send function of the type.
pub fn output_supported(ty: u32) -> bool {
    let ty = base_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        return output_supported(elem);
    }
    matches!(
        ty,
        oid::BOOL
            | oid::BYTEA
            | oid::CHAR
            | oid::NAME
            | oid::INT8
            | oid::INT2
            | oid::INT4
            | oid::INT2VECTOR
            | oid::TEXT
            | oid::OID
            | XID
            | CID
            | oid::OIDVECTOR
            | oid::JSON
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::UNKNOWN
            | oid::BPCHAR
            | oid::VARCHAR
            | oid::DATE
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::INTERVAL
            | oid::TIMETZ
            | oid::NUMERIC
            | oid::CSTRING
            | oid::UUID
            | oid::JSONB
            | oid::PG_NODE_TREE
            | oid::ACLITEM
            | oid::ANYARRAY
            | oid::PG_LSN
    ) || RegKind::from_oid(ty).is_some()
}

/// True when the engine has the text input function of the type.
pub fn input_supported(ty: u32) -> bool {
    let ty = base_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        return input_supported(elem);
    }
    output_supported(ty)
        && !matches!(ty, XID | CID | oid::PG_NODE_TREE | oid::ACLITEM | oid::ANYARRAY | oid::PG_LSN)
}

/// The name of a type for an error.
fn type_name(ty: u32) -> String {
    types::format_type(ty).into_owned()
}

/// The state that the input of the date and time types reads.
fn datetime_input<'a>(
    session: &'a dyn Session,
    zone: &'a dyn types::TimeZone,
) -> DateTimeInput<'a> {
    DateTimeInput {
        order: session.date_format().order,
        zone,
        zones: &NoZones,
        abbrevs: ZoneAbbrevs::postgres_default(),
        now: session.transaction_start(),
    }
}

/// The text output function of the type.
///
/// # Errors
///
/// `0A000` for a type that has no output function here, and the error of the output function, such as a timestamp out of range.
pub fn output(ty: u32, value: &Value, session: &dyn Session, out: &mut Vec<u8>) -> Result<()> {
    let ty = base_type(ty);
    if let Some((elem, delim)) = array_of(ty) {
        let Value::Array(array) = value else { return Err(bad_value()) };
        let mut failed = None;
        types::array_out(array, delim, out, |item, out| {
            if failed.is_none()
                && let Err(error) = output(elem, item, session, out)
            {
                failed = Some(error);
            }
        });
        return failed.map_or(Ok(()), Err);
    }
    if let (Some(kind), Value::Oid(v)) = (RegKind::from_oid(ty), value) {
        reg::output(kind, *v, session, out);
        return Ok(());
    }
    match (ty, value) {
        (_, Value::Bool(v)) => types::bool_out(*v, out),
        (_, Value::Int2(v)) => types::int_out(i64::from(*v), out),
        (_, Value::Int4(v)) => types::int_out(i64::from(*v), out),
        (_, Value::Int8(v)) => types::int_out(*v, out),
        (_, Value::Float4(v)) => types::float4_out(*v, session.extra_float_digits(), out),
        (_, Value::Float8(v)) => types::float8_out(*v, session.extra_float_digits(), out),
        (_, Value::Numeric(v)) => types::numeric_out(v, out),
        (oid::OID | XID | CID, Value::Oid(v)) => types::oid_out(*v, out),
        (_, Value::Char(v)) => types::char_out(*v, out),
        (_, Value::Text(v)) => out.extend_from_slice(v.as_bytes()),
        (_, Value::Bytea(v)) => types::bytea_out(v, session.bytea_output(), out),
        (_, Value::Uuid(v)) => types::uuid_out(v, out),
        (_, Value::Date(v)) => types::date_out(*v, session.date_format(), out),
        (_, Value::Time(v)) => types::time_out(*v, out),
        (_, Value::TimeTz(time, zone)) => types::timetz_out(*time, *zone, out),
        (_, Value::Timestamp(v)) => {
            types::timestamp_out(*v, session.date_format(), out).map_err(type_error)?;
        }
        (_, Value::TimestampTz(v)) => {
            types::timestamptz_out(*v, session.date_format(), &*session.zone()?, out)
                .map_err(type_error)?;
        }
        (_, Value::Interval(v)) => types::interval_out(v, session.interval_style(), out),
        (oid::INT2VECTOR, Value::Array(array)) => {
            let values = vector(array, |v| match v {
                Value::Int2(n) => Some(*n),
                _ => None,
            })?;
            types::int2vector_out(&values, out);
        }
        (oid::OIDVECTOR, Value::Array(array)) => {
            let values = vector(array, Value::as_oid)?;
            types::oidvector_out(&values, out);
        }
        (_, Value::Null) => return Err(Error::internal("the output function got a null value")),
        _ => return Err(not_yet(format!("output of type {}", type_name(ty)))),
    }
    Ok(())
}

/// The elements of an `int2vector` or an `oidvector`, which have no null element.
fn vector<T>(array: &Array<Value>, get: impl Fn(&Value) -> Option<T>) -> Result<Vec<T>> {
    array.values.iter().map(|v| v.as_ref().and_then(&get).ok_or_else(bad_value)).collect()
}

/// The cast of a value to `text`: the output function, with the casts of `pg_cast` that differ from it. `bool` gives `true` and `false`, and `bpchar` loses its trailing spaces.
///
/// # Errors
///
/// The errors of [`output`].
pub fn to_text(ty: u32, value: &Value, session: &dyn Session) -> Result<String> {
    match (base_type(ty), value) {
        (oid::BOOL, Value::Bool(v)) => Ok(if *v { "true" } else { "false" }.to_string()),
        (oid::BPCHAR, Value::Text(v)) => Ok(v.trim_end_matches(' ').to_string()),
        (_, Value::Text(v)) => Ok(v.clone()),
        _ => {
            let mut out = Vec::new();
            output(ty, value, session, &mut out)?;
            String::from_utf8(out)
                .map_err(|_| Error::internal("an output function gave bytes that are not UTF-8"))
        }
    }
}

/// The text input function of the type, with the typmod of the target.
///
/// # Errors
///
/// `0A000` for a type that has no input function here, and the error of the input function for a bad value.
pub fn input(ty: u32, text: &str, typmod: i32, session: &dyn Session) -> Result<Value> {
    let ty = base_type(ty);
    if let Some((elem, delim)) = array_of(ty) {
        let array = types::array_in(text, delim, session.array_nulls(), |item| {
            input(elem, item, typmod, session).map_err(|e| types_error(&e))
        })
        .map_err(type_error)?;
        return Ok(Value::Array(Box::new(array)));
    }
    if let Some(kind) = RegKind::from_oid(ty) {
        return Ok(Value::Oid(reg::input(kind, text, session)??));
    }
    let value = match ty {
        oid::BOOL => Value::Bool(types::bool_in(text).map_err(type_error)?),
        oid::BYTEA => Value::Bytea(types::bytea_in(text).map_err(type_error)?),
        oid::CHAR => Value::Char(types::char_in(text)),
        oid::NAME => Value::text(types::name_in(text)),
        oid::INT2 => Value::Int2(types::int2_in(text).map_err(type_error)?),
        oid::INT4 => Value::Int4(types::int4_in(text).map_err(type_error)?),
        oid::INT8 => Value::Int8(types::int8_in(text).map_err(type_error)?),
        oid::OID => Value::Oid(types::oid_in(text).map_err(type_error)?),
        XID => Value::Oid(types::uint32_in(text, "xid").map_err(type_error)?),
        CID => Value::Oid(types::uint32_in(text, "cid").map_err(type_error)?),
        oid::TEXT | oid::UNKNOWN | oid::CSTRING => Value::text(text),
        oid::VARCHAR => Value::text(types::varchar_in(text, typmod).map_err(type_error)?),
        oid::BPCHAR => Value::text(types::bpchar_in(text, typmod).map_err(type_error)?),
        oid::JSON => Value::text(types::json_in(text).map_err(type_error)?),
        oid::JSONB => Value::Text(types::jsonb_in(text).map_err(type_error)?),
        oid::FLOAT4 => Value::Float4(types::float4_in(text).map_err(type_error)?),
        oid::FLOAT8 => Value::Float8(types::float8_in(text).map_err(type_error)?),
        oid::NUMERIC => Value::Numeric(types::numeric_in(text, typmod).map_err(type_error)?),
        oid::UUID => Value::Uuid(types::uuid_in(text).map_err(type_error)?),
        oid::INT2VECTOR => {
            let values = types::int2vector_in(text).map_err(type_error)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Int2(v))).collect(),
            )))
        }
        oid::OIDVECTOR => {
            let values = types::oidvector_in(text).map_err(type_error)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Oid(v))).collect(),
            )))
        }
        oid::DATE | oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let zone = session.zone()?;
            let cx = datetime_input(session, &*zone);
            match ty {
                oid::DATE => Value::Date(types::date_in(text, &cx).map_err(type_error)?),
                oid::TIME => Value::Time(types::time_in(text, typmod, &cx).map_err(type_error)?),
                oid::TIMETZ => {
                    let (time, zone) = types::timetz_in(text, typmod, &cx).map_err(type_error)?;
                    Value::TimeTz(time, zone)
                }
                oid::TIMESTAMP => {
                    Value::Timestamp(types::timestamp_in(text, typmod, &cx).map_err(type_error)?)
                }
                _ => Value::TimestampTz(
                    types::timestamptz_in(text, typmod, &cx).map_err(type_error)?,
                ),
            }
        }
        oid::INTERVAL => Value::Interval(
            types::interval_in(text, typmod, session.interval_style()).map_err(type_error)?,
        ),
        _ => return Err(not_yet(format!("input of type {}", type_name(ty)))),
    };
    Ok(value)
}

/// An error of the engine as the error of a type function, for an element of an array.
fn types_error(error: &Error) -> types::TypeError {
    types::TypeError {
        sqlstate: error.state(),
        message: error.message().to_string(),
        detail: error.detail().map(str::to_string),
        hint: error.hint().map(str::to_string),
        context: None,
    }
}

/// The binary input function of the type. `number` is the number of the parameter, from 1, for the error of bytes that are left over.
///
/// # Errors
///
/// `0A000` for a type that has no receive function here, and the error of the receive function for bad data.
pub fn receive(ty: u32, data: &[u8], typmod: i32, number: usize) -> Result<Value> {
    let mut recv = Recv::new(data);
    let value = receive_from(ty, &mut recv, typmod).map_err(|e| match e {
        Received::Type(e) => type_error(e),
        Received::Engine(e) => e,
    })?;
    recv.finish(number).map_err(type_error)?;
    Ok(value)
}

/// The error of a receive function.
enum Received {
    Type(types::TypeError),
    Engine(Error),
}

impl From<types::TypeError> for Received {
    fn from(error: types::TypeError) -> Received {
        Received::Type(error)
    }
}

/// The receive function of the type, which reads the bytes of one value.
fn receive_from(ty: u32, recv: &mut Recv<'_>, typmod: i32) -> std::result::Result<Value, Received> {
    let ty = base_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        let mut failed = None;
        let array = types::array_recv(recv, elem, |item| match receive_from(elem, item, typmod) {
            Ok(value) => Ok(value),
            Err(Received::Type(e)) => Err(e),
            Err(Received::Engine(e)) => {
                let text = types_error(&e);
                failed = Some(e);
                Err(text)
            }
        });
        if let Some(error) = failed {
            return Err(Received::Engine(error));
        }
        return Ok(Value::Array(Box::new(array?)));
    }
    let value = match ty {
        oid::BOOL => Value::Bool(recv.bool()?),
        oid::BYTEA => Value::Bytea(recv.rest().to_vec()),
        oid::CHAR => Value::Char(recv.byte()?),
        oid::NAME => Value::text(types::name_recv(recv.text()?)?),
        oid::INT2 => Value::Int2(recv.i16()?),
        oid::INT4 => Value::Int4(recv.i32()?),
        oid::INT8 => Value::Int8(recv.i64()?),
        oid::TEXT | oid::UNKNOWN | oid::CSTRING => Value::text(recv.text()?),
        oid::VARCHAR => Value::text(types::varchar_in(recv.text()?, typmod)?),
        oid::BPCHAR => Value::text(types::bpchar_in(recv.text()?, typmod)?),
        oid::OID => Value::Oid(recv.u32()?),
        ty if RegKind::from_oid(ty).is_some() => Value::Oid(recv.u32()?),
        oid::JSON => Value::text(types::json_in(recv.text()?)?),
        oid::JSONB => Value::Text(types::jsonb_recv(recv.rest())?),
        oid::FLOAT4 => Value::Float4(recv.f32()?),
        oid::FLOAT8 => Value::Float8(recv.f64()?),
        oid::NUMERIC => Value::Numeric(types::numeric_recv(recv, typmod)?),
        oid::UUID => Value::Uuid(recv.uuid()?),
        oid::DATE => Value::Date(types::date_recv(recv)?),
        oid::TIME => Value::Time(types::time_recv(recv, typmod)?),
        oid::TIMETZ => {
            let (time, zone) = types::timetz_recv(recv, typmod)?;
            Value::TimeTz(time, zone)
        }
        oid::TIMESTAMP => Value::Timestamp(types::timestamp_recv(recv, typmod)?),
        oid::TIMESTAMPTZ => Value::TimestampTz(types::timestamp_recv(recv, typmod)?),
        oid::INTERVAL => Value::Interval(types::interval_recv(recv, typmod)?),
        oid::INT2VECTOR => {
            let values = types::int2vector_recv(recv)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Int2(v))).collect(),
            )))
        }
        oid::OIDVECTOR => {
            let values = types::oidvector_recv(recv)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Oid(v))).collect(),
            )))
        }
        _ => {
            return Err(Received::Engine(not_yet(format!(
                "binary input of type {}",
                type_name(ty)
            ))));
        }
    };
    Ok(value)
}

/// The binary output function of the type.
///
/// # Errors
///
/// `0A000` for a type that has no send function here.
pub fn send(ty: u32, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    let ty = base_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        let Value::Array(array) = value else { return Err(bad_value()) };
        let mut failed = None;
        types::array_send(array, elem, out, |item, out| {
            if failed.is_none()
                && let Err(error) = send(elem, item, out)
            {
                failed = Some(error);
            }
        });
        return failed.map_or(Ok(()), Err);
    }
    match (ty, value) {
        (oid::ACLITEM | oid::ANYARRAY | oid::PG_LSN, _) => {
            return Err(not_yet(format!("binary output of type {}", type_name(ty))));
        }
        (_, Value::Bool(v)) => out.push(u8::from(*v)),
        (_, Value::Int2(v)) => out.extend_from_slice(&v.to_be_bytes()),
        (_, Value::Int4(v) | Value::Date(v)) => out.extend_from_slice(&v.to_be_bytes()),
        (_, Value::Int8(v) | Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v)) => {
            out.extend_from_slice(&v.to_be_bytes());
        }
        (_, Value::Float4(v)) => out.extend_from_slice(&v.to_be_bytes()),
        (_, Value::Float8(v)) => out.extend_from_slice(&v.to_be_bytes()),
        (_, Value::Numeric(v)) => types::numeric_send(v, out),
        (_, Value::Oid(v)) => out.extend_from_slice(&v.to_be_bytes()),
        (_, Value::Char(v)) => out.push(*v),
        (oid::JSONB, Value::Text(v)) => types::jsonb_send(v, out),
        (_, Value::Text(v)) => out.extend_from_slice(v.as_bytes()),
        (_, Value::Bytea(v)) => out.extend_from_slice(v),
        (_, Value::Uuid(v)) => out.extend_from_slice(v),
        (_, Value::TimeTz(time, zone)) => {
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&zone.to_be_bytes());
        }
        (_, Value::Interval(v)) => types::interval_send(v, out),
        (oid::INT2VECTOR, Value::Array(array)) => {
            let values = vector(array, |v| match v {
                Value::Int2(n) => Some(*n),
                _ => None,
            })?;
            types::int2vector_send(&values, out);
        }
        (oid::OIDVECTOR, Value::Array(array)) => {
            let values = vector(array, Value::as_oid)?;
            types::oidvector_send(&values, out);
        }
        (_, Value::Null) => return Err(Error::internal("the send function got a null value")),
        _ => return Err(not_yet(format!("binary output of type {}", type_name(ty)))),
    }
    Ok(())
}

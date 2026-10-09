//! `Value`, one value of any type in the engine.
//!
//! A value does not carry its type. The expression that gives the value has the type OID and the typmod, as a `Datum` of PostgreSQL has no type and the plan node has it. So one variant holds the values of many types: `Text` holds `text`, `name`, `varchar`, `bpchar`, `unknown` and the other types whose value is a string, and `Oid` holds `oid`, `xid`, `cid` and the OID alias types such as `regclass`. [`Datum`](crate::Datum) is the value of the M1 storage, which stores fewer types.

use crate::array::Array;
use crate::datetime::Interval;
use crate::network::Inet;
use crate::numeric::Numeric;

/// One value of the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Numeric),
    /// `oid`, `xid`, `cid` and the OID alias types.
    Oid(u32),
    /// The type `"char"`. The byte 0 is the empty string.
    Char(u8),
    /// The string types, `unknown`, `cstring`, `json` and the other types that the engine keeps as their text form.
    Text(String),
    Bytea(Vec<u8>),
    Uuid([u8; 16]),
    /// `date`, the days since 1 January 2000.
    Date(i32),
    /// `time`, the microseconds since midnight.
    Time(i64),
    /// `timetz`, the microseconds since midnight and the zone offset in seconds west of UTC.
    TimeTz(i64, i32),
    /// `timestamp`, the microseconds since 1 January 2000.
    Timestamp(i64),
    /// `timestamptz`, the microseconds since 1 January 2000 UTC.
    TimestampTz(i64),
    Interval(Interval),
    /// `inet` and `cidr`.
    Inet(Inet),
    /// An array, `int2vector` or `oidvector`. A null element is `None`, and no element is `Some(Value::Null)`.
    Array(Box<Array<Value>>),
}

impl Value {
    /// True for `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The value of a `bool`, or `None` for `Null`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// The string of a value of a string type, or `None` for `Null`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(v) => Some(v),
            _ => None,
        }
    }

    /// The value of an integer type as an `i64`, or `None` for `Null` and for the other types.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int2(v) => Some(i64::from(*v)),
            Value::Int4(v) => Some(i64::from(*v)),
            Value::Int8(v) => Some(*v),
            _ => None,
        }
    }

    /// The value of `oid` or of an OID alias type, or `None` for `Null`.
    pub fn as_oid(&self) -> Option<u32> {
        match self {
            Value::Oid(v) => Some(*v),
            _ => None,
        }
    }

    /// A text value.
    pub fn text(s: impl Into<String>) -> Value {
        Value::Text(s.into())
    }
}

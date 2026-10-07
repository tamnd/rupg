//! One value of an M1 type and its storage form (spec/07 section 7.9.2).
//!
//! The storage form of a fixed-width type is its value in little endian. `bool` is one byte, 0 or 1. `date` is the days since 1 January 2000 as an `i32`, and `timestamp` and `timestamptz` are the microseconds since 1 January 2000 as an `i64`, as in PostgreSQL. `text` is its UTF-8 bytes and `bytea` is its bytes. A NULL has no storage form.

use rupg_common::{Error, Oid, Result};

use crate::typeid::TypeId;

/// One value. `Null` has no type.
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    /// SQL NULL.
    Null,
    /// `bool`.
    Bool(bool),
    /// `int2`.
    Int2(i16),
    /// `int4`.
    Int4(i32),
    /// `int8`.
    Int8(i64),
    /// `float4`.
    Float4(f32),
    /// `float8`.
    Float8(f64),
    /// `oid`.
    Oid(Oid),
    /// `text`.
    Text(String),
    /// `bytea`.
    Bytea(Vec<u8>),
    /// `uuid`, the 16 bytes in the order of the text form.
    Uuid([u8; 16]),
    /// `date`, the days since 1 January 2000. `i32::MAX` is `infinity` and `i32::MIN` is `-infinity`.
    Date(i32),
    /// `timestamp`, the microseconds since 1 January 2000. `i64::MAX` is `infinity` and `i64::MIN` is `-infinity`.
    Timestamp(i64),
    /// `timestamptz`, the microseconds since 1 January 2000 UTC, with the infinities of `timestamp`.
    TimestampTz(i64),
}

impl Datum {
    /// The type of the value, or `None` for `Null`.
    pub fn type_id(&self) -> Option<TypeId> {
        Some(match self {
            Datum::Null => return None,
            Datum::Bool(_) => TypeId::BOOL,
            Datum::Int2(_) => TypeId::INT2,
            Datum::Int4(_) => TypeId::INT4,
            Datum::Int8(_) => TypeId::INT8,
            Datum::Float4(_) => TypeId::FLOAT4,
            Datum::Float8(_) => TypeId::FLOAT8,
            Datum::Oid(_) => TypeId::OID,
            Datum::Text(_) => TypeId::TEXT,
            Datum::Bytea(_) => TypeId::BYTEA,
            Datum::Uuid(_) => TypeId::UUID,
            Datum::Date(_) => TypeId::DATE,
            Datum::Timestamp(_) => TypeId::TIMESTAMP,
            Datum::TimestampTz(_) => TypeId::TIMESTAMPTZ,
        })
    }

    /// True for `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    /// The storage form, or `None` for `Null`.
    pub fn encode(&self) -> Option<Vec<u8>> {
        Some(match self {
            Datum::Null => return None,
            Datum::Bool(v) => vec![u8::from(*v)],
            Datum::Int2(v) => v.to_le_bytes().to_vec(),
            Datum::Int4(v) | Datum::Date(v) => v.to_le_bytes().to_vec(),
            Datum::Int8(v) | Datum::Timestamp(v) | Datum::TimestampTz(v) => {
                v.to_le_bytes().to_vec()
            }
            Datum::Float4(v) => v.to_le_bytes().to_vec(),
            Datum::Float8(v) => v.to_le_bytes().to_vec(),
            Datum::Oid(v) => v.0.to_le_bytes().to_vec(),
            Datum::Text(v) => v.as_bytes().to_vec(),
            Datum::Bytea(v) => v.clone(),
            Datum::Uuid(v) => v.to_vec(),
        })
    }

    /// The value of type `ty` in the storage form `bytes`. `None` is `Null`. A storage form of the wrong length, a `bool` that is not 0 or 1, or `text` that is not UTF-8 gives SQLSTATE `XX001`, because only the engine writes the storage form. A type that M1 does not store gives `XX000`.
    pub fn decode(ty: TypeId, bytes: Option<&[u8]>) -> Result<Datum> {
        let Some(bytes) = bytes else {
            return Ok(Datum::Null);
        };
        let bad = || {
            Error::corrupted(format!("a stored {ty} value of {} bytes is not valid", bytes.len()))
        };
        if ty.width().is_some_and(|w| usize::from(w) != bytes.len()) {
            return Err(bad());
        }
        let n2 = || <[u8; 2]>::try_from(bytes).map_err(|_| bad());
        let n4 = || <[u8; 4]>::try_from(bytes).map_err(|_| bad());
        let n8 = || <[u8; 8]>::try_from(bytes).map_err(|_| bad());
        Ok(match ty {
            TypeId::BOOL => match bytes {
                [0] => Datum::Bool(false),
                [1] => Datum::Bool(true),
                _ => return Err(bad()),
            },
            TypeId::INT2 => Datum::Int2(i16::from_le_bytes(n2()?)),
            TypeId::INT4 => Datum::Int4(i32::from_le_bytes(n4()?)),
            TypeId::INT8 => Datum::Int8(i64::from_le_bytes(n8()?)),
            TypeId::FLOAT4 => Datum::Float4(f32::from_le_bytes(n4()?)),
            TypeId::FLOAT8 => Datum::Float8(f64::from_le_bytes(n8()?)),
            TypeId::OID => Datum::Oid(Oid(u32::from_le_bytes(n4()?))),
            TypeId::DATE => Datum::Date(i32::from_le_bytes(n4()?)),
            TypeId::TIMESTAMP => Datum::Timestamp(i64::from_le_bytes(n8()?)),
            TypeId::TIMESTAMPTZ => Datum::TimestampTz(i64::from_le_bytes(n8()?)),
            TypeId::UUID => Datum::Uuid(bytes.try_into().map_err(|_| bad())?),
            TypeId::TEXT => Datum::Text(String::from_utf8(bytes.to_vec()).map_err(|_| bad())?),
            TypeId::BYTEA => Datum::Bytea(bytes.to_vec()),
            _ => {
                return Err(Error::internal(format!(
                    "the type with OID {} has no storage form at M1",
                    ty.0
                )));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use rupg_common::SqlState;

    use super::*;

    fn samples() -> Vec<Datum> {
        vec![
            Datum::Bool(false),
            Datum::Bool(true),
            Datum::Int2(i16::MIN),
            Datum::Int4(-7),
            Datum::Int8(i64::MAX),
            Datum::Float4(-0.5),
            Datum::Float8(f64::INFINITY),
            Datum::Oid(Oid(16384)),
            Datum::Text("héllo".into()),
            Datum::Text(String::new()),
            Datum::Bytea(vec![0, 255, 7]),
            Datum::Uuid([9; 16]),
            Datum::Date(i32::MIN),
            Datum::Timestamp(-1),
            Datum::TimestampTz(i64::MAX),
        ]
    }

    #[test]
    fn storage_forms() {
        for d in samples() {
            let ty = d.type_id().unwrap();
            let bytes = d.encode().unwrap();
            if let Some(w) = ty.width() {
                assert_eq!(bytes.len(), usize::from(w), "{ty}");
            }
            assert_eq!(Datum::decode(ty, Some(&bytes)).unwrap(), d);
            assert_eq!(TypeId::from_oid(ty.0), Some(ty));
            assert_ne!(ty.name(), "unknown");
        }
        assert_eq!(Datum::Int4(0x0102_0304).encode().unwrap(), [4, 3, 2, 1]);
        assert_eq!((Datum::Null.encode(), Datum::Null.type_id()), (None, None));
        assert!(Datum::decode(TypeId::TEXT, None).unwrap().is_null());
        let nan = Datum::decode(TypeId::FLOAT8, Some(&f64::NAN.to_le_bytes())).unwrap();
        assert!(matches!(nan, Datum::Float8(v) if v.is_nan()));
        assert_eq!(TypeId::from_oid(Oid(1700)), None);
    }

    #[test]
    fn bad_storage_forms() {
        let bad: [(TypeId, &[u8]); 5] = [
            (TypeId::INT4, &[1, 2, 3]),
            (TypeId::BOOL, &[2]),
            (TypeId::UUID, &[0; 15]),
            (TypeId::TEXT, &[0xff, 0xfe]),
            (TypeId::INT8, &[]),
        ];
        for (ty, bytes) in bad {
            assert_eq!(
                Datum::decode(ty, Some(bytes)).unwrap_err().state(),
                SqlState::DATA_CORRUPTED,
                "{ty}"
            );
        }
        let numeric = TypeId(Oid(1700));
        assert_eq!(
            Datum::decode(numeric, Some(&[0])).unwrap_err().state(),
            SqlState::INTERNAL_ERROR
        );
        assert_eq!(numeric.name(), "unknown");
    }
}

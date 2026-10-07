//! The type ids of the M1 types (spec/07 section 7.9).

use std::fmt;

use rupg_common::Oid;

/// A type, by its OID in `pg_type` of the PostgreSQL pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub Oid);

impl TypeId {
    /// `bool`.
    pub const BOOL: TypeId = TypeId(Oid(16));
    /// `bytea`.
    pub const BYTEA: TypeId = TypeId(Oid(17));
    /// `int8`.
    pub const INT8: TypeId = TypeId(Oid(20));
    /// `int2`.
    pub const INT2: TypeId = TypeId(Oid(21));
    /// `int4`.
    pub const INT4: TypeId = TypeId(Oid(23));
    /// `text`.
    pub const TEXT: TypeId = TypeId(Oid(25));
    /// `oid`.
    pub const OID: TypeId = TypeId(Oid(26));
    /// `float4`.
    pub const FLOAT4: TypeId = TypeId(Oid(700));
    /// `float8`.
    pub const FLOAT8: TypeId = TypeId(Oid(701));
    /// `date`.
    pub const DATE: TypeId = TypeId(Oid(1082));
    /// `timestamp`.
    pub const TIMESTAMP: TypeId = TypeId(Oid(1114));
    /// `timestamptz`.
    pub const TIMESTAMPTZ: TypeId = TypeId(Oid(1184));
    /// `uuid`.
    pub const UUID: TypeId = TypeId(Oid(2950));

    /// The types that M1 stores. Later milestones add the other types of the pin.
    pub const M1: [TypeId; 13] = [
        TypeId::BOOL,
        TypeId::BYTEA,
        TypeId::INT8,
        TypeId::INT2,
        TypeId::INT4,
        TypeId::TEXT,
        TypeId::OID,
        TypeId::FLOAT4,
        TypeId::FLOAT8,
        TypeId::DATE,
        TypeId::TIMESTAMP,
        TypeId::TIMESTAMPTZ,
        TypeId::UUID,
    ];

    /// The type with this OID, or `None` if M1 does not store it.
    pub fn from_oid(oid: Oid) -> Option<TypeId> {
        TypeId::M1.into_iter().find(|t| t.0 == oid)
    }

    /// The name in `pg_type`.
    pub fn name(self) -> &'static str {
        match self {
            TypeId::BOOL => "bool",
            TypeId::BYTEA => "bytea",
            TypeId::INT8 => "int8",
            TypeId::INT2 => "int2",
            TypeId::INT4 => "int4",
            TypeId::TEXT => "text",
            TypeId::OID => "oid",
            TypeId::FLOAT4 => "float4",
            TypeId::FLOAT8 => "float8",
            TypeId::DATE => "date",
            TypeId::TIMESTAMP => "timestamp",
            TypeId::TIMESTAMPTZ => "timestamptz",
            TypeId::UUID => "uuid",
            _ => "unknown",
        }
    }

    /// The name that PostgreSQL gives in messages, from `format_type_be`.
    pub fn sql_name(self) -> &'static str {
        match self {
            TypeId::BOOL => "boolean",
            TypeId::INT8 => "bigint",
            TypeId::INT2 => "smallint",
            TypeId::INT4 => "integer",
            TypeId::FLOAT4 => "real",
            TypeId::FLOAT8 => "double precision",
            TypeId::TIMESTAMP => "timestamp without time zone",
            TypeId::TIMESTAMPTZ => "timestamp with time zone",
            other => other.name(),
        }
    }

    /// The bytes of a value in storage form, or `None` for a type whose values have their own length.
    pub fn width(self) -> Option<u8> {
        match self {
            TypeId::BOOL => Some(1),
            TypeId::INT2 => Some(2),
            TypeId::INT4 | TypeId::OID | TypeId::FLOAT4 | TypeId::DATE => Some(4),
            TypeId::INT8 | TypeId::FLOAT8 | TypeId::TIMESTAMP | TypeId::TIMESTAMPTZ => Some(8),
            TypeId::UUID => Some(16),
            _ => None,
        }
    }
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

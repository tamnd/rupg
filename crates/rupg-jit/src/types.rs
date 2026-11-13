//! The PostgreSQL types of QIR values (document 22 section 22.7: the IR types gain the PostgreSQL types).
//!
//! QIR has machine types and no SQL type, and a nullable SQL value is two QIR values, the data and an `i1` (`ty.rs`). A pipeline that rupg compiles reads the PostgreSQL types of its columns, so this module gives the QIR type that holds the data of each PostgreSQL type whose values fit one machine value. The other types, such as `numeric`, `interval` and the arrays, have no QIR type, and a pipeline that reads them stays in the vector executor.

use rupg_types::oid;

use crate::ir::Ty;

/// The QIR type of the data of a value of the built-in PostgreSQL type `type_oid`, or `None` when its values do not fit one machine value.
///
/// The integers keep their widths, with the sign in the opcode: `oid`, `xid` and the `reg*` types are 32 bit, and `pg_lsn` and `xid8` are 64 bit. `date` is the 32 bit day number of PostgreSQL, and `time`, `timestamp` and `timestamptz` are the 64 bit microsecond counts. `money` is its 64 bit count of cents. The strings, `bytea` included, are the 16 byte `str16` header.
#[must_use]
pub fn data_type(type_oid: u32) -> Option<Ty> {
    Some(match type_oid {
        oid::BOOL => Ty::I1,
        oid::CHAR => Ty::I8,
        oid::INT2 => Ty::I16,
        oid::INT4
        | oid::OID
        | oid::REGPROC
        | oid::REGCLASS
        | oid::REGTYPE
        | oid::XID
        | oid::CID
        | oid::DATE => Ty::I32,
        oid::INT8
        | oid::MONEY
        | oid::TIME
        | oid::TIMESTAMP
        | oid::TIMESTAMPTZ
        | oid::XID8
        | oid::PG_LSN => Ty::I64,
        oid::FLOAT4 => Ty::F32,
        oid::FLOAT8 => Ty::F64,
        oid::UUID => Ty::I128,
        oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME | oid::BYTEA => Ty::Str16,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_width_types_keep_their_width() {
        for (ty, len) in [
            (oid::BOOL, 1),
            (oid::INT2, 2),
            (oid::INT4, 4),
            (oid::INT8, 8),
            (oid::FLOAT4, 4),
            (oid::FLOAT8, 8),
            (oid::DATE, 4),
            (oid::TIMESTAMPTZ, 8),
            (oid::UUID, 16),
        ] {
            assert_eq!(data_type(ty).map(Ty::bytes), Some(len), "type {ty}");
        }
    }

    #[test]
    fn the_strings_are_str16_and_numeric_has_no_type() {
        assert_eq!(data_type(oid::TEXT), Some(Ty::Str16));
        assert_eq!(data_type(oid::BYTEA), Some(Ty::Str16));
        assert_eq!(data_type(oid::NUMERIC), None);
        assert_eq!(data_type(oid::INTERVAL), None);
        assert_eq!(data_type(oid::INT4RANGE), None);
    }
}

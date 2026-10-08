//! The TOAST decision of PostgreSQL. rupg has no TOAST tables, but a PostgreSQL table that has one uses two OIDs for it, so the catalog must know when PostgreSQL makes one.
//!
//! This is a port of `heapam_relation_needs_toast_table` of `heapam_handler.c` and `type_maximum_size` of `format_type.c`.

/// The OID of `bpchar`.
const BPCHAR: u32 = 1042;
/// The OID of `varchar`.
const VARCHAR: u32 = 1043;
/// The OID of `numeric`.
const NUMERIC: u32 = 1700;
/// The OID of `bit`.
const BIT: u32 = 1560;
/// The OID of `varbit`.
const VARBIT: u32 = 1562;

/// The size of a varlena header.
const VARHDRSZ: i32 = 4;
/// The most bytes of one character in UTF-8, the encoding of each rupg database.
const MAX_CHAR_BYTES: i32 = 4;
/// `SizeofHeapTupleHeader`.
const TUPLE_HEADER: i32 = 23;
/// `TOAST_TUPLE_THRESHOLD`: `MaximumBytesPerTuple(4)` for pages of 8192 bytes, `MAXALIGN_DOWN((8192 - MAXALIGN(24 + 4 * 4)) / 4)`.
const TOAST_TUPLE_THRESHOLD: i32 = 2032;

/// The facts about one column that the TOAST decision uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// `typlen` of the type of the column: a positive length, -1 for varlena, -2 for a C string.
    pub len: i16,
    /// `typalign`: `c`, `s`, `i` or `d`.
    pub align: u8,
    /// `attstorage`: `p`, `e`, `m` or `x`.
    pub storage: u8,
    /// The type of the column.
    pub ty: u32,
    /// The type modifier of the column, or -1.
    pub typmod: i32,
}

/// `type_maximum_size`: the most bytes that a value of the type with the modifier can have, or `None` when there is no limit.
pub fn type_maximum_size(ty: u32, typmod: i32) -> Option<i32> {
    if typmod < 0 {
        return None;
    }
    match ty {
        BPCHAR | VARCHAR => Some((typmod - VARHDRSZ) * MAX_CHAR_BYTES + VARHDRSZ),
        NUMERIC => {
            if typmod < VARHDRSZ {
                return None;
            }
            let precision = ((typmod - VARHDRSZ) >> 16) & 0xffff;
            let digits = (precision + 2 * 3) / 4;
            Some(8 + digits * 2)
        }
        BIT | VARBIT => Some((typmod + 7) / 8 + 8),
        _ => None,
    }
}

/// `att_align_nominal`: `length` rounded up to the alignment `align`.
fn align_to(length: i32, align: u8) -> i32 {
    let to = match align {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    };
    (length + to - 1) / to * to
}

/// `MAXALIGN`: `length` rounded up to 8.
fn max_align(length: i32) -> i32 {
    align_to(length, b'd')
}

/// True when PostgreSQL makes a TOAST table for a new table with these columns.
pub fn needs_toast(columns: &[Shape]) -> bool {
    let mut data_length = 0;
    let mut length_unknown = false;
    let mut toastable = false;
    for column in columns {
        data_length = align_to(data_length, column.align);
        if column.len > 0 {
            data_length += i32::from(column.len);
            continue;
        }
        match type_maximum_size(column.ty, column.typmod) {
            Some(size) => data_length += size,
            None => length_unknown = true,
        }
        if column.storage != b'p' {
            toastable = true;
        }
    }
    if !toastable {
        return false;
    }
    if length_unknown {
        return true;
    }
    let natts = i32::try_from(columns.len()).unwrap_or(i32::MAX);
    let tuple_length = max_align(TUPLE_HEADER + (natts + 7) / 8) + max_align(data_length);
    tuple_length > TOAST_TUPLE_THRESHOLD
}

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

// The values of a row that `heap_toast_insert_or_update` moves to the TOAST table. Each moved value takes an OID of the counter in `toast_save_datum`, after the OID of the row.

/// `TOAST_TUPLE_TARGET`, which is the same as `TOAST_TUPLE_THRESHOLD`.
const TOAST_TUPLE_TARGET: usize = 2032;
/// `MAXALIGN(TOAST_POINTER_SIZE)`: a value of this size or less does not move.
const POINTER: usize = 24;
/// The size of a TOAST pointer in the row.
const POINTER_SIZE: usize = 18;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Raw,
    Compressed,
    Incompressible,
    External,
}

/// The size of a `varlena` value of `len` bytes in a row: a short header of 1 byte when the data is shorter than 127 bytes, or else a header of 4 bytes.
fn stored(len: usize) -> usize {
    if len < 127 { len + 1 } else { len + 4 }
}

/// The values of `values`, all with the storage `extended`, that move to the TOAST table of a row with the header of `hoff` bytes and `fixed` bytes of other data. The first round compresses the biggest value, and moves it when it is still too long, and the second round moves the biggest values.
#[must_use]
pub fn moved(
    hoff: usize,
    fixed: usize,
    values: &[&[u8]],
    method: rupg_encode::Method,
) -> Vec<bool> {
    let mut sizes: Vec<usize> = values.iter().map(|v| stored(v.len())).collect();
    let mut held = vec![Held::Raw; values.len()];
    let max = TOAST_TUPLE_TARGET - hoff;
    let data = |sizes: &[usize]| fixed + sizes.iter().sum::<usize>();
    if hoff + data(&sizes) <= TOAST_TUPLE_TARGET {
        return vec![false; values.len()];
    }
    let biggest = |sizes: &[usize], held: &[Held], round: u8| {
        let mut best: Option<usize> = None;
        for (at, &size) in sizes.iter().enumerate() {
            let fits = match round {
                1 => held[at] == Held::Raw,
                _ => matches!(held[at], Held::Raw | Held::Compressed | Held::Incompressible),
            };
            if fits && size > POINTER && best.is_none_or(|b| size > sizes[b]) {
                best = Some(at);
            }
        }
        best
    };
    while data(&sizes) > max {
        let Some(at) = biggest(&sizes, &held, 1) else { break };
        match rupg_encode::toast_compress(method, values[at]) {
            Some(size) => {
                sizes[at] = size;
                held[at] = Held::Compressed;
            }
            None => held[at] = Held::Incompressible,
        }
        if sizes[at] > max {
            sizes[at] = POINTER_SIZE;
            held[at] = Held::External;
        }
    }
    while data(&sizes) > max {
        let Some(at) = biggest(&sizes, &held, 2) else { break };
        sizes[at] = POINTER_SIZE;
        held[at] = Held::External;
    }
    held.iter().map(|&h| h == Held::External).collect()
}

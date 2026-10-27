//! `pg_column_size`: the number of bytes of a value as PostgreSQL forms it, as `toast_datum_size` and `datumGetSize` find it.
//!
//! The values of rupg are not in the forms of PostgreSQL, so the sizes come from the layouts of `varatt.h`, `numeric.c`, `array.h`, `jsonb_util.c` and `heaptuple.c`. A value of variable length that is in memory has a header of 4 bytes. A value that a table keeps has a header of 1 byte when it is short, as `heap_fill_tuple` packs it. A long value that a table compresses has the size of the value before the compression, because rupg does not compress it.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    Array, JsonItem, JsonTree, Numeric, NumericSign, Record, Value, format_type, numeric_in, oid,
};

use crate::io::base_type;
use crate::{Call, bad_value};

/// `VARHDRSZ`.
const VARHDRSZ: usize = 4;
/// `VARATT_SHORT_MAX`: the largest value with a header of 1 byte.
const VARATT_SHORT_MAX: usize = 0x7f;
/// `offsetof(HeapTupleHeaderData, t_bits)`.
const TUPLE_HEADER: usize = 23;
/// `NUMERIC_SHORT_DSCALE_MAX`, `NUMERIC_SHORT_WEIGHT_MAX` and `NUMERIC_SHORT_WEIGHT_MIN`.
const NUMERIC_SHORT_DSCALE_MAX: u16 = 0x3f;
const NUMERIC_SHORT_WEIGHT_MAX: i16 = 63;
const NUMERIC_SHORT_WEIGHT_MIN: i16 = -64;

/// `pg_column_size`: the size of the value as a value in memory.
pub(crate) fn pg_column_size(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [value] = args else { return Err(bad_value()) };
    let ty = call.args.first().copied().ok_or_else(bad_value)?;
    column_size(ty, value, false)
}

/// `pg_column_size` of a value. `stored` is true for a value that a table keeps, which has a header of 1 byte when it is short.
///
/// # Errors
///
/// A type whose form rupg does not know.
pub fn column_size(ty: u32, value: &Value, stored: bool) -> Result<Value> {
    let size = datum_size(ty, value).ok_or_else(|| {
        Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("pg_column_size of type {} is not supported yet", format_type(ty)),
        )
    })?;
    let size = match size {
        Size::Varlena(n) if stored && packable(ty) => short_size(n),
        Size::Varlena(n) | Size::Fixed(n) => n,
    };
    i32::try_from(size).map(Value::Int4).map_err(|_| bad_value())
}

/// The size of a value: a value of fixed length, or a varlena with a header of 4 bytes.
#[derive(Clone, Copy)]
enum Size {
    Fixed(usize),
    Varlena(usize),
}

/// `VARATT_CAN_MAKE_SHORT` and `VARATT_CONVERTED_SHORT_SIZE`: the size with a header of 1 byte when the value is short enough.
fn short_size(size: usize) -> usize {
    if size - VARHDRSZ < VARATT_SHORT_MAX { size - VARHDRSZ + 1 } else { size }
}

/// `ATT_IS_PACKABLE`: true for a varlena whose `typstorage` is not `plain`.
fn packable(ty: u32) -> bool {
    !matches!(base_type(ty), oid::INT2VECTOR | oid::OIDVECTOR | oid::GTSVECTOR | oid::TSQUERY)
}

/// `typlen` and `typalign` of a type.
fn layout(ty: u32) -> Option<(i16, u8)> {
    builtin::type_by_oid(base_type(ty)).map(|row| (row.len, row.align))
}

/// `datumGetSize` of a value of the type.
fn datum_size(ty: u32, value: &Value) -> Option<Size> {
    let ty = base_type(ty);
    let (len, _) = layout(ty)?;
    if let Ok(len) = usize::try_from(len) {
        return Some(Size::Fixed(len));
    }
    if len == -2 {
        return Some(Size::Fixed(value.as_str()?.len() + 1));
    }
    let data = match (ty, value) {
        (oid::JSONB, Value::Text(text)) => jsonb_size(text)?,
        (_, Value::Numeric(n)) => numeric_size(n),
        (
            oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::JSON | oid::XML | oid::UNKNOWN,
            Value::Text(text),
        ) => VARHDRSZ + text.len(),
        (oid::BYTEA, Value::Bytea(bytes)) => VARHDRSZ + bytes.len(),
        // `SET_INET_VARSIZE`: the family and the bits, then the bytes of the address.
        (_, Value::Inet(inet)) => VARHDRSZ + 2 + inet.bytes().len(),
        (_, Value::Array(array)) => array_size(element_type(ty)?, array)?,
        (_, Value::Record(record)) => record_size(record)?,
        _ => return None,
    };
    Some(Size::Varlena(data))
}

/// `typelem` of an array type.
fn element_type(ty: u32) -> Option<u32> {
    builtin::type_by_oid(ty).map(|row| row.elem).filter(|&elem| elem != 0)
}

/// `att_align_nominal`: the offset rounded up to the alignment `c`, `s`, `i` or `d`.
fn align(offset: usize, align: u8) -> usize {
    let to = match align {
        b's' => 2,
        b'i' => 4,
        b'd' => 8,
        _ => 1,
    };
    offset.div_ceil(to) * to
}

/// `MAXALIGN`.
fn max_align(offset: usize) -> usize {
    align(offset, b'd')
}

/// The size of a `numeric` as `make_result` forms it: the short header when the scale and the weight fit in it, and 2 bytes for each digit of base 10000. A special value has only the short header.
fn numeric_size(n: &Numeric) -> usize {
    let digits = n.digits().len();
    if matches!(n.sign(), NumericSign::NaN | NumericSign::Infinity | NumericSign::NegativeInfinity)
    {
        return VARHDRSZ + 2;
    }
    let weight = if digits == 0 { 0 } else { n.weight() };
    let short = n.dscale() <= NUMERIC_SHORT_DSCALE_MAX
        && (NUMERIC_SHORT_WEIGHT_MIN..=NUMERIC_SHORT_WEIGHT_MAX).contains(&weight);
    VARHDRSZ + if short { 2 } else { 4 } + 2 * digits
}

/// The size of an array as `construct_md_array` forms it: `ARR_OVERHEAD_NONULLS` or `ARR_OVERHEAD_WITHNULLS`, then each element with the padding to the alignment after it. An element of variable length has a header of 4 bytes.
fn array_size(element: u32, array: &Array<Value>) -> Option<usize> {
    let (_, element_align) = layout(element)?;
    let ndim = array.dims.len();
    if ndim == 0 {
        return Some(max_align(VARHDRSZ + 12));
    }
    let nulls = array.values.iter().any(Option::is_none);
    let bitmap = if nulls { array.values.len().div_ceil(8) } else { 0 };
    let mut size = max_align(VARHDRSZ + 12 + 8 * ndim + bitmap);
    for value in array.values.iter().flatten() {
        let n = match datum_size(element, value)? {
            Size::Fixed(n) | Size::Varlena(n) => n,
        };
        size = align(size + n, element_align);
    }
    Some(size)
}

/// `heap_compute_data_size` and `heap_form_tuple`: the size of a row value. The header has the null bitmap when a field is null, and a short varlena field has a header of 1 byte with no alignment.
fn record_size(record: &Record) -> Option<usize> {
    let nulls = record.values.iter().any(Value::is_null);
    let bitmap = if nulls { record.values.len().div_ceil(8) } else { 0 };
    let mut size = max_align(TUPLE_HEADER + bitmap);
    for (&ty, value) in record.types.iter().zip(&record.values) {
        if value.is_null() {
            continue;
        }
        let (_, field_align) = layout(ty)?;
        size = match datum_size(ty, value)? {
            Size::Varlena(n) if packable(ty) && short_size(n) < n => size + short_size(n),
            Size::Fixed(n) | Size::Varlena(n) => align(size, field_align) + n,
        };
    }
    Some(size)
}

/// The size of a `jsonb` value as `convertToJsonb` forms it from its text. A scalar at the root is the one element of an array.
fn jsonb_size(text: &str) -> Option<usize> {
    let tree = JsonTree::parse(text).ok()?;
    let mut size = VARHDRSZ;
    match tree.item(0) {
        JsonItem::Array(_) | JsonItem::Object(_) => jsonb_value(&tree, 0, &mut size)?,
        _ => jsonb_container(&tree, &[], &[0], &mut size)?,
    }
    Some(size)
}

/// `convertJsonbArray` and `convertJsonbObject`: the header of 4 bytes at an alignment of 4, a `JEntry` of 4 bytes for each key and each value, then the keys and the values.
fn jsonb_container(
    tree: &JsonTree<'_>,
    keys: &[&str],
    values: &[usize],
    size: &mut usize,
) -> Option<()> {
    *size = align(*size, b'i') + 4 + 4 * (keys.len() + values.len());
    *size += keys.iter().map(|key| key.len()).sum::<usize>();
    for &value in values {
        jsonb_value(tree, value, size)?;
    }
    Some(())
}

/// `convertJsonbValue` and `convertJsonbScalar`: a null and a boolean have no data, a string has its bytes, and a number is a `numeric` at an alignment of 4.
fn jsonb_value(tree: &JsonTree<'_>, at: usize, size: &mut usize) -> Option<()> {
    match tree.item(at) {
        JsonItem::Null | JsonItem::Bool(_) => {}
        JsonItem::String(s) => *size += s.len(),
        JsonItem::Number => {
            let n = numeric_in(tree.text(at), -1).ok()?;
            *size = align(*size, b'i') + numeric_size(&n);
        }
        JsonItem::Array(items) => jsonb_container(tree, &[], items, size)?,
        JsonItem::Object(fields) => {
            let keys: Vec<&str> = fields.iter().map(|(key, _)| key.as_str()).collect();
            let values: Vec<usize> = fields.iter().map(|&(_, value)| value).collect();
            jsonb_container(tree, &keys, &values, size)?;
        }
    }
    Some(())
}

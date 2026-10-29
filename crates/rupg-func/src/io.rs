//! The input, output, receive and send functions of the types, as `typinput`, `typoutput`, `typreceive` and `typsend` of `pg_type` give them.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    self as types, AnyArray, Array, DateTimeInput, Record, Recv, RegKind, Value, ZoneAbbrevs, oid,
    tz,
};

use crate::{Call, Session, bad_value, not_yet, reg, type_error};

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

/// The type whose input and output functions read and write the values of a type: [`base_type`], or the built-in type of a base type that `CREATE TYPE` made with internal functions of a built-in type.
fn io_type(ty: u32) -> u32 {
    let ty = base_type(ty);
    builtin::base_representation(ty).unwrap_or(ty)
}

/// The element type and the delimiter of an array type, or `None` for a type that is not an array. `int2vector` and `oidvector` are not arrays here, because they have their own functions.
pub(crate) fn array_of(ty: u32) -> Option<(u32, u8)> {
    if ty == oid::INT2VECTOR || ty == oid::OIDVECTOR {
        return None;
    }
    let row = builtin::type_by_oid(ty)?;
    // `record[]` is a pseudo-type, so its category is not the one of the arrays.
    if (row.category != ARRAY_CATEGORY && ty != oid::RECORD_ARRAY) || row.elem == 0 {
        return None;
    }
    let delim = builtin::type_by_oid(row.elem).map_or(b',', |e| e.delim);
    Some((row.elem, delim))
}

/// True when the engine has the output and the send function of the type.
pub fn output_supported(ty: u32) -> bool {
    let ty = io_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        return output_supported(elem);
    }
    if let Some(sub) = builtin::range_subtype(ty) {
        return output_supported(sub);
    }
    if let Some(range) = builtin::multirange_range(ty) {
        return output_supported(range);
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
            | oid::INET
            | oid::CIDR
            | oid::JSONB
            | oid::PG_NODE_TREE
            | oid::PG_NDISTINCT
            | oid::PG_DEPENDENCIES
            | oid::PG_MCV_LIST
            | oid::ACLITEM
            | oid::ANYARRAY
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
            | oid::PG_LSN
            | oid::VOID
            | oid::TRIGGER
            | oid::EVENT_TRIGGER
            | oid::MONEY
            | oid::TID
            | oid::XID8
            | oid::OID8
            | oid::REFCURSOR
            | oid::MACADDR
            | oid::MACADDR8
            | oid::BIT
            | oid::VARBIT
            | oid::POINT
            | oid::LSEG
            | oid::PATH
            | oid::BOX
            | oid::POLYGON
            | oid::LINE
            | oid::CIRCLE
            | oid::PG_SNAPSHOT
            | oid::TXID_SNAPSHOT
            | oid::TSVECTOR
            | oid::TSQUERY
            | oid::JSONPATH
            | oid::XML
    ) || RegKind::from_oid(ty).is_some()
        || is_row_type(ty)
        || is_enum(ty)
}

/// True for an enum type.
pub(crate) fn is_enum(ty: u32) -> bool {
    builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'e')
}

/// `enum_out`: the name of the label with this OID of the enum type.
fn enum_label(ty: u32, label: u32) -> Result<&'static str> {
    builtin::enum_labels(ty)
        .and_then(|labels| labels.iter().find(|(oid, _)| *oid == label))
        .map(|(_, name)| name.as_str())
        .ok_or_else(|| {
            Error::new(
                SqlState::INVALID_BINARY_REPRESENTATION,
                format!("invalid internal value for enum: {label}"),
            )
        })
}

/// `enum_in`: the OID of the label with this name of the enum type.
fn enum_in(ty: u32, text: &str) -> std::result::Result<u32, Error> {
    builtin::enum_labels(ty)
        .and_then(|labels| labels.iter().find(|(_, name)| name == text))
        .map(|(oid, _)| *oid)
        .ok_or_else(|| {
            Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input value for enum {}: \"{text}\"", type_name(ty)),
            )
        })
}

/// `check_safe_enum_use`: an error for a label that the open transaction added to an enum type that an earlier transaction made, as an index can have the label only after the commit.
pub(crate) fn check_safe_enum_use(ty: u32, oid: u32, label: &str) -> Result<()> {
    if !builtin::enum_label_is_uncommitted(ty, oid) {
        return Ok(());
    }
    Err(Error::new(
        SqlState::UNSAFE_NEW_ENUM_VALUE_USAGE,
        format!("unsafe use of new value \"{label}\" of enum type {}", type_name(ty)),
    )
    .with_hint("New enum values must be committed before they can be used."))
}

/// True for `record` and for the row type of a system catalog, which `record_out` and `record_send` show. The output of each field needs the output of its type, which the engine checks when it shows the row.
pub fn is_row_type(ty: u32) -> bool {
    ty == oid::RECORD || builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'c')
}

/// True when the engine has the text input function of the type.
pub fn input_supported(ty: u32) -> bool {
    let ty = io_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        return input_supported(elem);
    }
    if let Some(sub) = builtin::range_subtype(ty) {
        return input_supported(sub);
    }
    if let Some(range) = builtin::multirange_range(ty) {
        return input_supported(range);
    }
    output_supported(ty) && ty != oid::ANYARRAY
}

/// The name of a type for an error. A type of the catalog of the session has its name in its row.
pub(crate) fn type_name(ty: u32) -> String {
    let name = types::format_type(ty);
    if name != "???" {
        return name.into_owned();
    }
    match builtin::type_by_oid(ty) {
        Some(row) if row.category == ARRAY_CATEGORY && row.elem != 0 => {
            format!("{}[]", type_name(row.elem))
        }
        Some(row) => row.name.to_owned(),
        None => name.into_owned(),
    }
}

/// The state that the input of the date and time types reads.
pub(crate) fn datetime_input<'a>(
    session: &'a dyn Session,
    zone: &'a dyn types::TimeZone,
) -> DateTimeInput<'a> {
    DateTimeInput {
        order: session.date_format().order,
        zone,
        zones: &tz::TzDatabase,
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
    if let Value::AnyArray(value) = value {
        out.extend_from_slice(value.text.as_bytes());
        return Ok(());
    }
    let ty = io_type(ty);
    if is_enum(ty)
        && let Value::Oid(label) = value
    {
        out.extend_from_slice(enum_label(ty, *label)?.as_bytes());
        return Ok(());
    }
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
    if let Value::Range(range) = value {
        return range_out(ty, range, session, out);
    }
    if let Value::Multirange(ranges) = value {
        return multirange_out(ty, ranges, session, out);
    }
    if let Value::Record(record) = value {
        return record_out(record, session, out);
    }
    if let Value::TsVector(lexemes) = value {
        types::tsvector_deparse(lexemes, out);
        return Ok(());
    }
    if let Value::TsQuery(items) = value {
        types::tsquery_deparse(items, out);
        return Ok(());
    }
    if let Value::JsonPath(path) = value {
        types::jsonpath_deparse(path, out);
        return Ok(());
    }
    if let (Some(kind), Value::Bytea(v)) = (types::GeoKind::from_oid(ty), value) {
        types::geo_out(kind, v, session.extra_float_digits(), out);
        return Ok(());
    }
    match (ty, value) {
        (oid::MONEY, Value::Int8(v)) => types::cash_out(*v, out),
        (oid::PG_LSN, Value::Int8(v)) => types::pg_lsn_out(v.cast_unsigned(), out),
        (oid::PG_SNAPSHOT | oid::TXID_SNAPSHOT, Value::Bytea(v)) => {
            let snapshot = types::Snapshot::from_bytes(v).ok_or_else(bad_value)?;
            types::snapshot_out(&snapshot, out);
        }
        (oid::MACADDR, Value::Int8(v)) => types::macaddr_out(v.cast_unsigned(), out),
        (oid::MACADDR8, Value::Int8(v)) => types::macaddr8_out(v.cast_unsigned(), out),
        (oid::XID8 | oid::OID8, Value::Int8(v)) => types::u64_out(v.cast_unsigned(), out),
        (oid::TID, Value::Int8(v)) => {
            let (block, offset) = tid_parts(*v)?;
            types::tid_out(block, offset, out);
        }
        (_, Value::Bool(v)) => types::bool_out(*v, out),
        (_, Value::Int2(v)) => types::int_out(i64::from(*v), out),
        (_, Value::Int4(v)) => types::int_out(i64::from(*v), out),
        (_, Value::Int8(v)) => types::int_out(*v, out),
        (_, Value::Float4(v)) => types::float4_out(*v, session.extra_float_digits(), out),
        (_, Value::Float8(v)) => types::float8_out(*v, session.extra_float_digits(), out),
        (_, Value::Numeric(v)) => types::numeric_out(v, out),
        (oid::OID | XID | CID, Value::Oid(v)) => types::oid_out(*v, out),
        (_, Value::Char(v)) => types::char_out(*v, out),
        (oid::XML, Value::Text(v)) => {
            if let Some(warning) = types::xml_out(v, out) {
                session.warn(type_error(warning));
            }
        }
        (_, Value::Text(v)) => out.extend_from_slice(v.as_bytes()),
        (oid::PG_NDISTINCT, Value::Bytea(v)) => {
            types::pg_ndistinct_out(v, out).map_err(type_error)?
        }
        (oid::PG_DEPENDENCIES, Value::Bytea(v)) => {
            types::pg_dependencies_out(v, out).map_err(type_error)?;
        }
        (_, Value::Bytea(v)) => types::bytea_out(v, session.bytea_output(), out),
        (_, Value::Uuid(v)) => types::uuid_out(v, out),
        (_, Value::Inet(v)) => types::inet_out(v, ty == oid::CIDR, out),
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
            check_vector(array, "int2vector")?;
            let values = vector(array, |v| match v {
                Value::Int2(n) => Some(*n),
                _ => None,
            })?;
            types::int2vector_out(&values, out);
        }
        (oid::OIDVECTOR, Value::Array(array)) => {
            check_vector(array, "oidvector")?;
            let values = vector(array, Value::as_oid)?;
            types::oidvector_out(&values, out);
        }
        (_, Value::Null) => return Err(Error::internal("the output function got a null value")),
        _ => return Err(not_yet(format!("output of type {}", type_name(ty)))),
    }
    Ok(())
}

/// A `tid` value: the block number and the offset in a `bigint`, so that the order of the values is the order of `tid`.
pub(crate) fn tid_value(block: u32, offset: u16) -> Value {
    Value::Int8(i64::from(block) << 16 | i64::from(offset))
}

/// The block number and the offset of a `tid` value.
pub(crate) fn tid_parts(value: i64) -> Result<(u32, u16)> {
    let block = u32::try_from(value >> 16).map_err(|_| bad_value())?;
    let offset = u16::try_from(value & 0xFFFF).map_err(|_| bad_value())?;
    Ok((block, offset))
}

/// `range_out`: the output function of the subtype for each bound that the range has, in the text of the range.
fn range_out(
    ty: u32,
    range: &types::Range,
    session: &dyn Session,
    out: &mut Vec<u8>,
) -> Result<()> {
    let sub = crate::range::subtype(ty);
    let (mut lower, mut upper) = (Vec::new(), Vec::new());
    if range.has_lower() {
        output(sub, &range.lower, session, &mut lower)?;
    }
    if range.has_upper() {
        output(sub, &range.upper, session, &mut upper)?;
    }
    types::range_deparse(
        range.flags,
        range.has_lower().then_some(&lower[..]),
        range.has_upper().then_some(&upper[..]),
        out,
    );
    Ok(())
}

/// `range_send`: the send function of the subtype for each bound that the range has, in the binary form of the range.
fn range_send(ty: u32, range: &types::Range, out: &mut Vec<u8>) -> Result<()> {
    let sub = crate::range::subtype(ty);
    let (mut lower, mut upper) = (Vec::new(), Vec::new());
    if range.has_lower() {
        send(sub, &range.lower, &mut lower)?;
    }
    if range.has_upper() {
        send(sub, &range.upper, &mut upper)?;
    }
    types::range_send(
        range.flags,
        range.has_lower().then_some(&lower[..]),
        range.has_upper().then_some(&upper[..]),
        out,
    );
    Ok(())
}

/// `multirange_out`: the output function of the range type for each range, in braces.
fn multirange_out(
    ty: u32,
    ranges: &[types::Range],
    session: &dyn Session,
    out: &mut Vec<u8>,
) -> Result<()> {
    let range_ty = crate::multirange::range_of(ty);
    let mut texts = Vec::with_capacity(ranges.len());
    for range in ranges {
        let mut text = Vec::new();
        range_out(range_ty, range, session, &mut text)?;
        texts.push(text);
    }
    types::multirange_deparse(texts.iter().map(Vec::as_slice), out);
    Ok(())
}

/// `multirange_send`: the send function of the range type for each range, in the binary form of the multirange.
fn multirange_send(ty: u32, ranges: &[types::Range], out: &mut Vec<u8>) -> Result<()> {
    let range_ty = crate::multirange::range_of(ty);
    let mut bytes = Vec::with_capacity(ranges.len());
    for range in ranges {
        let mut one = Vec::new();
        range_send(range_ty, range, &mut one)?;
        bytes.push(one);
    }
    types::multirange_send(&bytes.iter().map(Vec::as_slice).collect::<Vec<_>>(), out);
    Ok(())
}

/// `record_out`: the fields in parentheses with a comma between them. A null field is empty. A field that is empty or has a quote, a backslash, a parenthesis, a comma or white space is in double quotes, with each quote and each backslash doubled.
fn record_out(record: &Record, session: &dyn Session, out: &mut Vec<u8>) -> Result<()> {
    out.push(b'(');
    let mut field = Vec::new();
    for (i, (ty, value)) in record.types.iter().zip(&record.values).enumerate() {
        if i > 0 {
            out.push(b',');
        }
        if value.is_null() {
            continue;
        }
        field.clear();
        output(*ty, value, session, &mut field)?;
        let quote = field.is_empty()
            || field.iter().any(|b| matches!(b, b'"' | b'\\' | b'(' | b')' | b',') || is_space(*b));
        if !quote {
            out.extend_from_slice(&field);
            continue;
        }
        out.push(b'"');
        for &b in &field {
            if matches!(b, b'"' | b'\\') {
                out.push(b);
            }
            out.push(b);
        }
        out.push(b'"');
    }
    out.push(b')');
    Ok(())
}

/// The type and the typmod of each column of the relation of a row type: a system catalog or a relation of the catalog of the session.
fn row_columns(ty: u32, session: &dyn Session) -> Option<Vec<(u32, i32)>> {
    let relid = builtin::type_by_oid(ty).filter(|row| row.relid != 0)?.relid;
    if let Some(catalog) = rupg_pgcatalog::catalog_by_oid(relid) {
        return Some(catalog.columns.iter().map(|c| (c.type_oid, -1)).collect());
    }
    let rel = session.catalog()?.relation(relid)?;
    Some(rel.columns.iter().filter(|c| !c.dropped).map(|c| (c.ty, c.typmod)).collect())
}

/// `record_in` of a composite type: the fields in parentheses with a comma between them, as [`record_out`] writes them. An empty field is null. A field can have double quotes and backslashes, as in `record_out`.
fn record_in(
    ty: u32,
    text: &str,
    session: &dyn Session,
    noisy: bool,
) -> std::result::Result<Value, Failed> {
    if ty == oid::RECORD {
        return Err(Failed::Hard(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "input of anonymous composite types is not implemented",
        )));
    }
    let columns = row_columns(ty, session).ok_or_else(|| Failed::Hard(no_input(ty)))?;
    let malformed = |detail: &str| {
        Failed::Soft(
            Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("malformed record literal: \"{text}\""),
            )
            .with_detail(detail),
        )
    };
    let bytes = text.as_bytes();
    let mut at = bytes.iter().take_while(|&&b| is_space(b)).count();
    if bytes.get(at) != Some(&b'(') {
        return Err(malformed("Missing left parenthesis."));
    }
    at += 1;
    let mut types = Vec::with_capacity(columns.len());
    let mut values = Vec::with_capacity(columns.len());
    let mut field = Vec::new();
    for (i, &(column, typmod)) in columns.iter().enumerate() {
        if i > 0 {
            if bytes.get(at) != Some(&b',') {
                return Err(malformed("Too few columns."));
            }
            at += 1;
        }
        types.push(column);
        if matches!(bytes.get(at), Some(b',' | b')')) {
            values.push(Value::Null);
            continue;
        }
        field.clear();
        let mut quoted = false;
        while quoted || !matches!(bytes.get(at), Some(b',' | b')')) {
            let Some(&b) = bytes.get(at) else {
                return Err(malformed("Unexpected end of input."));
            };
            at += 1;
            match b {
                b'\\' => {
                    let Some(&next) = bytes.get(at) else {
                        return Err(malformed("Unexpected end of input."));
                    };
                    field.push(next);
                    at += 1;
                }
                b'"' if quoted && bytes.get(at) == Some(&b'"') => {
                    field.push(b'"');
                    at += 1;
                }
                b'"' => quoted = !quoted,
                _ => field.push(b),
            }
        }
        // The bytes of the field come from the text and split it only at ASCII bytes, so they are UTF-8.
        let item = String::from_utf8_lossy(&field);
        values.push(input_value(column, &item, typmod, session, noisy)?);
    }
    if bytes.get(at) != Some(&b')') {
        return Err(malformed("Too many columns."));
    }
    at += 1;
    if bytes[at..].iter().any(|&b| !is_space(b)) {
        return Err(malformed("Junk after right parenthesis."));
    }
    Ok(Value::Record(Box::new(Record::new(types, values))))
}

/// `isspace` of C in the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// `record_send`: the number of fields, then for each field the OID of its type and the length and the bytes of its value, with the length -1 for a null.
fn record_send(record: &Record, out: &mut Vec<u8>) -> Result<()> {
    let count = i32::try_from(record.values.len()).map_err(|_| bad_value())?;
    out.extend_from_slice(&count.to_be_bytes());
    let mut field = Vec::new();
    for (ty, value) in record.types.iter().zip(&record.values) {
        out.extend_from_slice(&ty.to_be_bytes());
        if value.is_null() {
            out.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        }
        field.clear();
        send(*ty, value, &mut field)?;
        let len = i32::try_from(field.len()).map_err(|_| bad_value())?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&field);
    }
    Ok(())
}

/// The elements of an `int2vector` or an `oidvector`, which have no null element.
/// `max_stack_depth` in kilobytes, from its text with a unit such as `2MB` or `100kB`.
fn max_stack_depth(session: &dyn Session) -> Option<u32> {
    let text = session.setting("max_stack_depth")?;
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let n: u32 = text[..digits].parse().ok()?;
    let scale = match text[digits..].trim() {
        "" | "kB" => 1,
        "MB" => 1024,
        "GB" => 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(scale)
}

/// The check of `check_valid_oidvector` and `check_valid_int2vector`: a cast from an array can give a value that has no dimension, more than one dimension or a null element.
fn check_vector(array: &Array<Value>, name: &str) -> Result<()> {
    if array.dims.len() != 1 || array.values.iter().any(Option::is_none) {
        return Err(Error::new(
            SqlState::DATATYPE_MISMATCH,
            format!("array is not a valid {name}"),
        ));
    }
    Ok(())
}

fn vector<T>(array: &Array<Value>, get: impl Fn(&Value) -> Option<T>) -> Result<Vec<T>> {
    array.values.iter().map(|v| v.as_ref().and_then(&get).ok_or_else(bad_value)).collect()
}

/// The cast of a value to `text`: the output function, with the casts of `pg_cast` that differ from it. `bool` gives `true` and `false`, and `bpchar` loses its trailing spaces.
///
/// # Errors
///
/// The errors of [`output`].
pub fn to_text(ty: u32, value: &Value, session: &dyn Session) -> Result<String> {
    match (io_type(ty), value) {
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

/// The array of an `anyarray` value, read with the input function of its element type.
///
/// # Errors
///
/// The error of the input function of the element type.
pub fn anyarray_array(value: &AnyArray, session: &dyn Session) -> Result<Array<Value>> {
    let delim = builtin::type_by_oid(base_type(value.elem)).map_or(b',', |t| t.delim);
    let mut failed = None;
    let array = types::array_in(&value.text, delim, true, |item| {
        match input(value.elem, item, -1, session) {
            Ok(item) => Ok(item),
            Err(error) => {
                failed.get_or_insert(error);
                Ok(Value::Null)
            }
        }
    })
    .map_err(type_error)?;
    failed.map_or(Ok(array), Err)
}

/// The text input function of the type, with the typmod of the target.
///
/// # Errors
///
/// `0A000` for a type that has no input function here, and the error of the input function for a bad value.
pub fn input(ty: u32, text: &str, typmod: i32, session: &dyn Session) -> Result<Value> {
    match input_value(ty, text, typmod, session, true) {
        Ok(value) => Ok(value),
        Err(Failed::Soft(error) | Failed::Hard(error)) => Err(error),
    }
}

/// The text input function of the type, as `InputFunctionCallSafe` calls it. The outer error is a hard error, such as a type that has no input function. The inner error is a soft error of the input function for a bad value, which `pg_input_is_valid` reports as false. The checks of a domain are not part of the input.
///
/// # Errors
///
/// `0A000` for a type that has no input function here.
pub fn input_soft(
    ty: u32,
    text: &str,
    typmod: i32,
    session: &dyn Session,
) -> Result<Result<Value>> {
    match input_value(ty, text, typmod, session, false) {
        Ok(value) => Ok(Ok(value)),
        Err(Failed::Soft(error)) => Ok(Err(error)),
        Err(Failed::Hard(error)) => Err(error),
    }
}

/// The error of an input function.
enum Failed {
    /// An error that the input function reports with `ereturn`, for a bad value.
    Soft(Error),
    /// An error that the input function reports with `ereport`.
    Hard(Error),
}

impl From<types::TypeError> for Failed {
    fn from(error: types::TypeError) -> Failed {
        Failed::Soft(type_error(error))
    }
}

impl From<Error> for Failed {
    fn from(error: Error) -> Failed {
        Failed::Hard(error)
    }
}

/// `tsqueryin`, with the notice of PostgreSQL for a query with no lexemes when `noisy` is true. A full stack of operators and a query that is too deep are errors that `pg_input_is_valid` does not catch.
fn tsquery_in(
    text: &str,
    session: &dyn Session,
    noisy: bool,
) -> std::result::Result<Vec<types::QueryItem>, Failed> {
    let items = types::tsquery_parse(text).map_err(|error| {
        if error.sqlstate == SqlState::INTERNAL_ERROR
            || error.sqlstate == SqlState::STATEMENT_TOO_COMPLEX
        {
            Failed::Hard(type_error(error))
        } else {
            Failed::from(error)
        }
    })?;
    if noisy && items.is_empty() {
        session.warn(crate::tsquery::no_lexemes(text));
    }
    Ok(items)
}

/// `xml_in` with the `xmloption` of the session. PostgreSQL reports each warning of libxml2 as it parses, so the warnings come before the error, and they come in the soft input too.
fn xml_in(text: &[u8], session: &dyn Session) -> std::result::Result<(), types::TypeError> {
    let document = session.setting("xmloption").is_some_and(|v| v.eq_ignore_ascii_case("document"));
    types::xml_in(text, document, &mut |warning| {
        session.warn(Error::new(SqlState::WARNING, warning))
    })
}

/// `xml_recv`: the bytes are in the encoding of the XML declaration, or in UTF-8 if it has none. Only UTF-8 and `SQL_ASCII` need no conversion, so the other encodings are not supported yet. The check of the encoding comes after the check of the value, as in PostgreSQL.
fn xml_recv<'a>(
    recv: &mut Recv<'a>,
    session: &dyn Session,
) -> std::result::Result<&'a str, Received> {
    let bytes = recv.rest();
    if let Some(name) = types::xml_encoding(bytes) {
        match crate::encoding::encoding(&name) {
            Some("UTF8" | "SQL_ASCII") => {}
            Some(other) => {
                return Err(Received::Engine(not_yet(format!(
                    "binary input of type xml in the encoding {other}"
                ))));
            }
            None => {
                return Err(Received::Engine(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid encoding name \"{name}\""),
                )));
            }
        }
    }
    xml_in(bytes, session)?;
    Ok(Recv::new(bytes).text()?)
}

/// `jsonpath_in`. A number that is too large and a path that is too deep are errors that `pg_input_is_valid` does not catch. The error for a code point that is too large is not the same in the soft input, as in PostgreSQL.
fn jsonpath_in(text: &str, noisy: bool) -> std::result::Result<types::JsonPath, Failed> {
    types::jsonpath_parse(text, !noisy, &crate::regex::validate).map_err(|error| {
        if error.sqlstate == SqlState::NUMERIC_VALUE_OUT_OF_RANGE
            || error.sqlstate == SqlState::STATEMENT_TOO_COMPLEX
        {
            Failed::Hard(type_error(error))
        } else {
            Failed::from(error)
        }
    })
}

/// The text input function of the type. `noisy` is false for the soft input of `pg_input_is_valid`, which gives no notices, as `escontext` does in PostgreSQL.
fn input_value(
    ty: u32,
    text: &str,
    typmod: i32,
    session: &dyn Session,
    noisy: bool,
) -> std::result::Result<Value, Failed> {
    // `domain_in` reads the value with the typmod of the domain.
    let typmod = match builtin::type_by_oid(ty) {
        Some(row) if row.kind == b'd' => row.typmod,
        _ => typmod,
    };
    let ty = io_type(ty);
    if let Some((elem, delim)) = array_of(ty) {
        // A hard error of an element stops the input of the array as a hard error.
        let mut hard = None;
        let array = types::array_in(text, delim, session.array_nulls(), |item| {
            match input_value(elem, item, typmod, session, noisy) {
                Ok(value) => Ok(value),
                Err(Failed::Soft(e)) => Err(types_error(&e)),
                Err(Failed::Hard(e)) => {
                    let error = types_error(&e);
                    hard = Some(e);
                    Err(error)
                }
            }
        });
        return match (array, hard) {
            (_, Some(error)) => Err(Failed::Hard(error)),
            (array, None) => Ok(Value::Array(Box::new(array?))),
        };
    }
    if let Some(sub) = builtin::range_subtype(ty) {
        return range_in(ty, sub, text, session, noisy);
    }
    if let Some(range) = builtin::multirange_range(ty) {
        return multirange_in(range, text, session, noisy);
    }
    if is_row_type(ty) {
        return record_in(ty, text, session, noisy);
    }
    if is_enum(ty) {
        let oid = enum_in(ty, text).map_err(Failed::Soft)?;
        check_safe_enum_use(ty, oid, text).map_err(Failed::Hard)?;
        return Ok(Value::Oid(oid));
    }
    if let Some(kind) = RegKind::from_oid(ty) {
        return match reg::input(kind, text, session)? {
            Ok(oid) => Ok(Value::Oid(oid)),
            Err(error) => Err(Failed::Soft(error)),
        };
    }
    if let Some(kind) = types::GeoKind::from_oid(ty) {
        return Ok(Value::Bytea(types::geo_in(kind, text)?));
    }
    let value = match ty {
        oid::BOOL => Value::Bool(types::bool_in(text)?),
        oid::BYTEA => Value::Bytea(types::bytea_in(text)?),
        oid::CHAR => Value::Char(types::char_in(text)),
        oid::NAME => Value::text(types::name_in(text)),
        oid::INT2 => Value::Int2(types::int2_in(text)?),
        oid::INT4 => Value::Int4(types::int4_in(text)?),
        oid::TSVECTOR => Value::TsVector(types::tsvector_parse(text)?),
        oid::TSQUERY => Value::TsQuery(tsquery_in(text, session, noisy)?),
        oid::JSONPATH => Value::JsonPath(Box::new(jsonpath_in(text, noisy)?)),
        oid::XML => {
            xml_in(text.as_bytes(), session)?;
            Value::text(text)
        }
        oid::INT8 => Value::Int8(types::int8_in(text)?),
        oid::OID => Value::Oid(types::oid_in(text)?),
        oid::MONEY => Value::Int8(types::cash_in(text)?),
        oid::MACADDR => Value::Int8(types::macaddr_in(text)?.cast_signed()),
        oid::MACADDR8 => Value::Int8(types::macaddr8_in(text)?.cast_signed()),
        oid::BIT => Value::Text(types::bit_in(text, typmod, false)?),
        oid::VARBIT => Value::Text(types::bit_in(text, typmod, true)?),
        oid::XID8 => Value::Int8(types::uint64_in(text, "xid8")?.cast_signed()),
        oid::OID8 => Value::Int8(types::uint64_in(text, "oid8")?.cast_signed()),
        oid::PG_LSN => Value::Int8(types::pg_lsn_in(text)?.cast_signed()),
        oid::PG_SNAPSHOT | oid::TXID_SNAPSHOT => Value::Bytea(types::snapshot_in(text)?.to_bytes()),
        oid::TID => {
            let (block, offset) = types::tid_in(text)?;
            tid_value(block, offset)
        }
        XID => Value::Oid(types::uint32_in(text, "xid")?),
        CID => Value::Oid(types::uint32_in(text, "cid")?),
        oid::ACLITEM => Value::Text(crate::acl::aclitem_in(text, session).map_err(Failed::Soft)?),
        oid::TEXT | oid::UNKNOWN | oid::CSTRING | oid::REFCURSOR => Value::text(text),
        // `void_in` takes any text. A `void` value is an empty text, which `void_out` and `void_send` give.
        oid::VOID => Value::Text(String::new()),
        oid::VARCHAR => Value::text(types::varchar_in(text, typmod)?),
        oid::BPCHAR => Value::text(types::bpchar_in(text, typmod)?),
        oid::JSON => Value::text(types::json_in_within(text, max_stack_depth(session))?),
        oid::JSONB => Value::Text(types::jsonb_in_within(text, max_stack_depth(session))?),
        oid::FLOAT4 => Value::Float4(types::float4_in(text)?),
        oid::FLOAT8 => Value::Float8(types::float8_in(text)?),
        oid::NUMERIC => Value::Numeric(types::numeric_in(text, typmod)?),
        oid::UUID => Value::Uuid(types::uuid_in(text)?),
        oid::INET | oid::CIDR => Value::Inet(types::inet_in(text, ty == oid::CIDR)?),
        oid::INT2VECTOR => {
            let values = types::int2vector_in(text)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Int2(v))).collect(),
            )))
        }
        oid::OIDVECTOR => {
            let values = types::oidvector_in(text)?;
            Value::Array(Box::new(Array::vector(
                values.into_iter().map(|v| Some(Value::Oid(v))).collect(),
            )))
        }
        oid::DATE | oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let zone = session.zone()?;
            let cx = datetime_input(session, &*zone);
            match ty {
                oid::DATE => Value::Date(types::date_in(text, &cx)?),
                oid::TIME => Value::Time(types::time_in(text, typmod, &cx)?),
                oid::TIMETZ => {
                    let (time, zone) = types::timetz_in(text, typmod, &cx)?;
                    Value::TimeTz(time, zone)
                }
                oid::TIMESTAMP => Value::Timestamp(types::timestamp_in(text, typmod, &cx)?),
                _ => Value::TimestampTz(types::timestamptz_in(text, typmod, &cx)?),
            }
        }
        oid::INTERVAL => {
            Value::Interval(types::interval_in(text, typmod, session.interval_style())?)
        }
        // `pg_node_tree_in`: a stored node tree comes only from the server.
        oid::PG_NODE_TREE => return Err(Failed::Hard(no_input(ty))),
        oid::PG_NDISTINCT => Value::Bytea(types::pg_ndistinct_in(text)?),
        oid::PG_DEPENDENCIES => Value::Bytea(types::pg_dependencies_in(text)?),
        // `pg_mcv_list_in`: a list of the most common values comes only from `ANALYZE`.
        oid::PG_MCV_LIST => return Err(Failed::Hard(no_input(ty))),
        // `gtsvector_in` and `brin_bloom_summary_in`: these values come only from an index.
        oid::GTSVECTOR | oid::PG_BRIN_BLOOM_SUMMARY => return Err(Failed::Hard(no_input(ty))),
        // `brin_minmax_multi_summary_in` names the type with no `pg_` prefix.
        oid::PG_BRIN_MINMAX_MULTI_SUMMARY => {
            return Err(Failed::Hard(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot accept a value of type brin_minmax_multi_summary".to_string(),
            )));
        }
        // `PSEUDOTYPE_DUMMY_INPUT_FUNC`: a pseudo-type such as `anyelement` has no values. `void` and `record` have input functions.
        _ if builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'p')
            && !matches!(ty, oid::VOID | oid::RECORD) =>
        {
            return Err(Failed::Hard(no_input(ty)));
        }
        _ => return Err(Failed::Hard(not_yet(format!("input of type {}", type_name(ty))))),
    };
    Ok(value)
}

/// `range_in`: the text of the range, then the input function of the subtype for each bound that is not infinite. The error of a bound that is above the other bound is a soft error.
fn range_in(
    ty: u32,
    sub: u32,
    text: &str,
    session: &dyn Session,
    noisy: bool,
) -> std::result::Result<Value, Failed> {
    let parsed = types::range_parse(text)?;
    let bound = |text: Option<&String>| match text {
        Some(text) => input_value(sub, text, -1, session, noisy),
        None => Ok(Value::Null),
    };
    let lower = bound(parsed.lower.as_ref())?;
    let upper = bound(parsed.upper.as_ref())?;
    let range = crate::range::make_range(ty, parsed.flags, lower, upper).map_err(Failed::Soft)?;
    Ok(Value::Range(Box::new(range)))
}

/// `multirange_in`: the text of the multirange, then the input function of the range type for each range. The empty ranges are not kept.
fn multirange_in(
    range_ty: u32,
    text: &str,
    session: &dyn Session,
    noisy: bool,
) -> std::result::Result<Value, Failed> {
    let mut ranges = Vec::new();
    types::multirange_parse(text, |text| match input_value(range_ty, text, -1, session, noisy)? {
        Value::Range(range) => {
            if !range.is_empty() {
                ranges.push(*range);
            }
            Ok(())
        }
        _ => Err(Failed::Hard(bad_value())),
    })?;
    let ranges = crate::multirange::make_multirange(range_ty, ranges).map_err(Failed::Soft)?;
    Ok(Value::Multirange(ranges))
}

/// `range_recv`: the binary form of the range, then the receive function of the subtype for the bytes of each bound. The receive function of a bound does not have to read all its bytes.
fn range_recv(
    ty: u32,
    sub: u32,
    recv: &mut Recv<'_>,
    session: &dyn Session,
) -> std::result::Result<Value, Received> {
    let (flags, lower, upper) = types::range_recv(recv)?;
    let bound = |bytes: Option<&[u8]>| match bytes {
        Some(bytes) => receive_from(sub, &mut Recv::new(bytes), -1, session),
        None => Ok(Value::Null),
    };
    let lower = bound(lower)?;
    let upper = bound(upper)?;
    let range = crate::range::make_range(ty, flags, lower, upper).map_err(Received::Engine)?;
    Ok(Value::Range(Box::new(range)))
}

/// `multirange_recv`: the binary form of the multirange, then the receive function of the range type for the bytes of each range.
fn multirange_recv(
    range_ty: u32,
    recv: &mut Recv<'_>,
    session: &dyn Session,
) -> std::result::Result<Value, Received> {
    let mut ranges = Vec::new();
    types::multirange_recv(recv, |bytes| {
        match receive_from(range_ty, &mut Recv::new(bytes), -1, session)? {
            Value::Range(range) => {
                ranges.push(*range);
                Ok(())
            }
            _ => Err(Received::Engine(bad_value())),
        }
    })?;
    let ranges = crate::multirange::make_multirange(range_ty, ranges).map_err(Received::Engine)?;
    Ok(Value::Multirange(ranges))
}

/// The error of the input and the receive functions of a type whose values come only from the server.
fn no_input(ty: u32) -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("cannot accept a value of type {}", type_name(ty)),
    )
}

/// `getTypeBinaryInputInfo` and `getTypeBinaryOutputInfo`: the error of a type that has no receive or no send function, such as `aclitem`.
fn no_binary(ty: u32, what: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("no binary {what} function available for type {}", type_name(ty)),
    )
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
pub fn receive(
    ty: u32,
    data: &[u8],
    typmod: i32,
    number: usize,
    session: &dyn Session,
) -> Result<Value> {
    let mut recv = Recv::new(data);
    let value = receive_from(ty, &mut recv, typmod, session).map_err(|e| match e {
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
fn receive_from(
    ty: u32,
    recv: &mut Recv<'_>,
    typmod: i32,
    session: &dyn Session,
) -> std::result::Result<Value, Received> {
    let ty = io_type(ty);
    if let Some((elem, _)) = array_of(ty) {
        let mut failed = None;
        let array =
            types::array_recv(recv, elem, |item| match receive_from(elem, item, typmod, session) {
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
    if let Some(sub) = builtin::range_subtype(ty) {
        return range_recv(ty, sub, recv, session);
    }
    if let Some(range) = builtin::multirange_range(ty) {
        return multirange_recv(range, recv, session);
    }
    if let Some(kind) = types::GeoKind::from_oid(ty) {
        return Ok(Value::Bytea(types::geo_recv(kind, recv)?));
    }
    if is_enum(ty) {
        let text = recv.text()?;
        let oid = enum_in(ty, text).map_err(Received::Engine)?;
        check_safe_enum_use(ty, oid, text).map_err(Received::Engine)?;
        return Ok(Value::Oid(oid));
    }
    let value = match ty {
        oid::BOOL => Value::Bool(recv.bool()?),
        oid::BYTEA => Value::Bytea(recv.rest().to_vec()),
        oid::CHAR => Value::Char(recv.byte()?),
        oid::NAME => Value::text(types::name_recv(recv.text()?)?),
        oid::INT2 => Value::Int2(recv.i16()?),
        oid::INT4 => Value::Int4(recv.i32()?),
        oid::INT8 | oid::MONEY | oid::XID8 | oid::OID8 | oid::PG_LSN => Value::Int8(recv.i64()?),
        // `void_recv` reads no bytes, so a value with bytes is an error.
        oid::VOID => Value::Text(String::new()),
        XID | CID => Value::Oid(recv.u32()?),
        oid::PG_SNAPSHOT | oid::TXID_SNAPSHOT => {
            Value::Bytea(types::snapshot_recv(recv)?.to_bytes())
        }
        oid::MACADDR => Value::Int8(types::macaddr_recv(recv)?.cast_signed()),
        oid::MACADDR8 => Value::Int8(types::macaddr8_recv(recv)?.cast_signed()),
        oid::BIT => Value::Text(types::bit_recv(recv, typmod, false)?),
        oid::VARBIT => Value::Text(types::bit_recv(recv, typmod, true)?),
        oid::TID => {
            let block = recv.u32()?;
            tid_value(block, recv.i16()?.cast_unsigned())
        }
        oid::TEXT | oid::UNKNOWN | oid::CSTRING | oid::REFCURSOR => Value::text(recv.text()?),
        oid::VARCHAR => Value::text(types::varchar_in(recv.text()?, typmod)?),
        oid::BPCHAR => Value::text(types::bpchar_in(recv.text()?, typmod)?),
        oid::OID => Value::Oid(recv.u32()?),
        oid::TSVECTOR => Value::TsVector(types::tsvector_recv(recv)?),
        oid::TSQUERY => Value::TsQuery(types::tsquery_recv(recv)?),
        oid::JSONPATH => {
            Value::JsonPath(Box::new(types::jsonpath_recv(recv, &crate::regex::validate)?))
        }
        ty if RegKind::from_oid(ty).is_some() => Value::Oid(recv.u32()?),
        oid::JSON => Value::text(types::json_in(recv.text()?)?),
        oid::XML => Value::text(xml_recv(recv, session)?),
        oid::JSONB => Value::Text(types::jsonb_recv(recv.rest())?),
        oid::FLOAT4 => Value::Float4(recv.f32()?),
        oid::FLOAT8 => Value::Float8(recv.f64()?),
        oid::NUMERIC => Value::Numeric(types::numeric_recv(recv, typmod)?),
        oid::UUID => Value::Uuid(recv.uuid()?),
        oid::INET | oid::CIDR => Value::Inet(types::inet_recv(recv, ty == oid::CIDR)?),
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
        oid::PG_NDISTINCT | oid::PG_DEPENDENCIES | oid::PG_MCV_LIST => {
            return Err(Received::Engine(no_input(ty)));
        }
        oid::ACLITEM => return Err(Received::Engine(no_binary(ty, "input"))),
        _ => {
            return Err(Received::Engine(not_yet(format!(
                "binary input of type {}",
                type_name(ty)
            ))));
        }
    };
    Ok(value)
}

/// The kernel of each text input function of a type, such as `int4in` or `numeric_in`, called from SQL: the value of the text for the type of the result, with the typmod of the third argument.
pub(crate) fn input_kernel(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let typmod = match args.get(2) {
        Some(Value::Int4(typmod)) => *typmod,
        _ => -1,
    };
    input(call.ret, text, typmod, call.session)
}

/// `array_in(cstring, oid, integer)` called from SQL: the array of the element type of the second argument, with the type modifier of the third. The result has the type `anyarray`, whose value is the text of the array, as for a column of `pg_stats`.
pub(crate) fn array_in_kernel(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let elem = match args.get(1) {
        Some(Value::Oid(elem)) => *elem,
        _ => return Err(bad_value()),
    };
    let typmod = match args.get(2) {
        Some(Value::Int4(typmod)) => *typmod,
        _ => -1,
    };
    let array = builtin::type_by_oid(elem).map_or(0, |row| row.array);
    if array == 0 {
        return Err(not_yet(format!("array_in of the element type {}", type_name(elem))));
    }
    let value = input(array, text, typmod, call.session)?;
    let mut out = Vec::new();
    output(array, &value, call.session, &mut out)?;
    Ok(Value::text(String::from_utf8(out).map_err(|_| bad_value())?))
}

/// `PSEUDOTYPE_DUMMY_INPUT_FUNC`: the input function of a pseudo-type such as `anyrange_in` cannot give a value.
pub(crate) fn pseudo_input_kernel(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let name = builtin::type_by_oid(call.ret)
        .map_or_else(|| type_name(call.ret), |row| row.name.to_owned());
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("cannot accept a value of type {name}"),
    ))
}

/// The kernel of each text output function of a type, such as `int4out` or `array_out`, called from SQL: the text of the value for the type of the argument.
pub(crate) fn output_kernel(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ([ty], [value]) = (call.args, args) else { return Err(bad_value()) };
    let mut out = Vec::new();
    output(*ty, value, call.session, &mut out)?;
    Ok(Value::text(String::from_utf8(out).map_err(|_| bad_value())?))
}

/// The kernel of each send function of a type, such as `int4send` or `array_send`, called from SQL: the binary output of the value for the type of the argument.
pub(crate) fn send_kernel(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ([ty], [value]) = (call.args, args) else { return Err(bad_value()) };
    let mut out = Vec::new();
    send(*ty, value, &mut out)?;
    Ok(Value::Bytea(out))
}

/// The binary output function of the type.
///
/// # Errors
///
/// `0A000` for a type that has no send function here.
pub fn send(ty: u32, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    let ty = io_type(ty);
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
    if is_enum(ty)
        && let Value::Oid(label) = value
    {
        out.extend_from_slice(enum_label(ty, *label)?.as_bytes());
        return Ok(());
    }
    match (ty, value) {
        (oid::ACLITEM, _) => return Err(no_binary(ty, "output")),
        (oid::ANYARRAY, _) => {
            return Err(not_yet(format!("binary output of type {}", type_name(ty))));
        }
        (oid::MACADDR, Value::Int8(v)) => out.extend_from_slice(&v.to_be_bytes()[2..]),
        (oid::TID, Value::Int8(v)) => {
            let (block, offset) = tid_parts(*v)?;
            out.extend_from_slice(&block.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
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
        (oid::BIT | oid::VARBIT, Value::Text(v)) => types::bit_send(v, out),
        // `xml_send` gives the warning of `xml_out` for a declaration that does not parse, but each stored value has a declaration that parses.
        (oid::XML, Value::Text(v)) => {
            let _ = types::xml_out(v, out);
        }
        (_, Value::Text(v)) => out.extend_from_slice(v.as_bytes()),
        (_, Value::Bytea(v)) => out.extend_from_slice(v),
        (_, Value::Uuid(v)) => out.extend_from_slice(v),
        (_, Value::Inet(v)) => types::inet_send(v, ty == oid::CIDR, out),
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
        (_, Value::Record(record)) => record_send(record, out)?,
        (_, Value::Range(range)) => range_send(ty, range, out)?,
        (_, Value::Multirange(ranges)) => multirange_send(ty, ranges, out)?,
        (_, Value::TsVector(lexemes)) => types::tsvector_send(lexemes, out),
        (_, Value::TsQuery(items)) => types::tsquery_send(items, out),
        (_, Value::JsonPath(path)) => types::jsonpath_send(path, out),
        (_, Value::Null) => return Err(Error::internal("the send function got a null value")),
        _ => return Err(not_yet(format!("binary output of type {}", type_name(ty)))),
    }
    Ok(())
}

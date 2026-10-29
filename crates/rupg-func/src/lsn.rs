//! The functions of `pg_lsn` from `pg_lsn.c`: the difference of two positions, the addition and the subtraction of a number of bytes, and the cast from `numeric`. Also the names of the WAL files from `xlogfuncs.c`. A value is the position in the bits of [`Value::Int8`]. The comparison operators, `pg_lsn_cmp`, `pg_lsn_larger` and `pg_lsn_smaller` are in [`crate::compare`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Numeric, NumericSign, Record, Value, oid};

use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a function of `pg_lsn` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_lsn_mi" | "pg_wal_lsn_diff" => lsn_mi,
        "pg_lsn_pli" => lsn_pli,
        "pg_lsn_mii" => lsn_mii,
        "numeric_pg_lsn" => numeric_lsn,
        "pg_walfile_name" => walfile_name,
        "pg_walfile_name_offset" => walfile_name_offset,
        "pg_split_walfile_name" => split_walfile_name,
        _ => return None,
    })
}

/// The position of an argument.
fn lsn(args: &[Value], n: usize) -> Result<u64> {
    match args.get(n) {
        Some(Value::Int8(v)) => Ok(v.cast_unsigned()),
        _ => Err(bad_value()),
    }
}

/// The `numeric` argument of `pg_lsn_pli` and `pg_lsn_mii`.
fn bytes(args: &[Value]) -> Result<&Numeric> {
    match args.get(1) {
        Some(Value::Numeric(v)) => Ok(v),
        _ => Err(bad_value()),
    }
}

/// `pg_lsn_mi` and `pg_wal_lsn_diff`: the number of bytes from the second position to the first, which is negative when the first position comes first.
fn lsn_mi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let diff = i128::from(lsn(args, 0)?) - i128::from(lsn(args, 1)?);
    Ok(Value::Numeric(Numeric::from_integer(diff)))
}

/// `numeric_pg_lsn`: the number rounded to an integer, as the casts to the integer types round it.
fn to_lsn(n: &Numeric) -> Result<Value> {
    let range = || Error::new(SqlState::INVALID_PARAMETER_VALUE, "pg_lsn out of range".to_string());
    match n.to_integer("pg_lsn") {
        Ok(v) => u64::try_from(v).map(|v| Value::Int8(v.cast_signed())).map_err(|_| range()),
        Err(error) if error.sqlstate == SqlState::NUMERIC_VALUE_OUT_OF_RANGE => Err(range()),
        Err(error) => Err(type_error(error)),
    }
}

fn numeric_lsn(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Numeric(n)) = args.first() else { return Err(bad_value()) };
    to_lsn(n)
}

/// `pg_lsn_pli`: the position after a number of bytes. `NaN` has its own error, and an infinity gives the error of the cast.
fn lsn_pli(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let n = bytes(args)?;
    if matches!(n.sign(), NumericSign::NaN) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot add NaN to pg_lsn".to_string(),
        ));
    }
    let start = Numeric::from_integer(i128::from(lsn(args, 0)?));
    to_lsn(&start.add(n).map_err(type_error)?)
}

/// `pg_lsn_mii`: the position before a number of bytes.
fn lsn_mii(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let n = bytes(args)?;
    if matches!(n.sign(), NumericSign::NaN) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot subtract NaN from pg_lsn".to_string(),
        ));
    }
    let start = Numeric::from_integer(i128::from(lsn(args, 0)?));
    to_lsn(&start.sub(n).map_err(type_error)?)
}

/// The size of a WAL segment, the default of `wal_segment_size`.
const WAL_SEGMENT_SIZE: u64 = 16 * 1024 * 1024;

/// The number of segments for one value of the high 32 bits of a position, as `XLogSegmentsPerXLogId` gives it.
const SEGMENTS_PER_ID: u64 = 0x1_0000_0000 / WAL_SEGMENT_SIZE;

/// The timeline of the WAL. A new cluster has the timeline 1, and the server does no recovery.
const TIMELINE: u32 = 1;

/// The name of the WAL file that holds a position, as `XLogFileName` gives it.
fn file_name(lsn: u64) -> String {
    let segment = lsn / WAL_SEGMENT_SIZE;
    format!("{TIMELINE:08X}{:08X}{:08X}", segment / SEGMENTS_PER_ID, segment % SEGMENTS_PER_ID)
}

/// `pg_walfile_name`: the name of the WAL file that holds a position.
fn walfile_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(file_name(lsn(args, 0)?)))
}

/// `pg_walfile_name_offset`: the name of the WAL file that holds a position, and the offset of the position in that file.
fn walfile_name_offset(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let lsn = lsn(args, 0)?;
    let offset = i32::try_from(lsn % WAL_SEGMENT_SIZE).map_err(|_| bad_value())?;
    let values = vec![Value::text(file_name(lsn)), Value::Int4(offset)];
    Ok(Value::Record(Box::new(Record::new(vec![oid::TEXT, oid::INT4], values))))
}

/// `pg_split_walfile_name`: the segment number and the timeline of the name of a WAL file. The name is 24 hexadecimal digits in either case, as `IsXLogFileName` accepts them after `pg_ascii_toupper`.
fn split_walfile_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let invalid = || {
        Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid WAL file name \"{name}\""))
    };
    if name.len() != 24 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let part = |at: usize| u32::from_str_radix(&name[at..at + 8], 16).map_err(|_| invalid());
    let (timeline, log, seg) = (part(0)?, part(8)?, part(16)?);
    let segment = u64::from(log) * SEGMENTS_PER_ID + u64::from(seg);
    let values = vec![
        Value::Numeric(Numeric::from_integer(i128::from(segment))),
        Value::Int8(i64::from(timeline)),
    ];
    Ok(Value::Record(Box::new(Record::new(vec![oid::NUMERIC, oid::INT8], values))))
}

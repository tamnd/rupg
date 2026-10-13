//! The functions of `bit` and `bit varying` from `varbit.c`: the bit operators, the shifts, `substring`, `overlay`, `position`, `get_bit`, `set_bit`, the lengths, `bitcmp`, the length casts and the casts to and from the integer types. A value is a string of `0` and `1` in [`Value::Text`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{self as types, BIT_MAX_LEN, Value};

use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a function of `bit` or `bit varying` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "bit_and" => bit_and,
        "bit_or" => bit_or,
        "bitxor" => bit_xor,
        "bitnot" => bit_not,
        "bitshiftleft" => shift_left,
        "bitshiftright" => shift_right,
        "bitcat" => concat,
        "bitsubstr" | "bitsubstr_no_len" => substring,
        "bitoverlay" | "bitoverlay_no_len" => overlay,
        "bitposition" => position,
        "bitlength" => bit_length,
        "bitoctetlength" => octet_length,
        "bitgetbit" => get_bit,
        "bitsetbit" => set_bit,
        "bit_bit_count" => bit_count,
        "bitcmp" => bit_cmp,
        "bit" => bit_typmod,
        "varbit" => varbit_typmod,
        "bitfromint4" | "bitfromint8" => from_integer,
        "bittoint4" => to_int4,
        "bittoint8" => to_int8,
        _ => return None,
    })
}

/// The bits of argument `n`.
fn bits(args: &[Value], n: usize) -> Result<&str> {
    args.get(n).and_then(Value::as_str).ok_or_else(bad_value)
}

/// The `int4` of argument `n`.
fn int4(args: &[Value], n: usize) -> Result<i32> {
    match args.get(n) {
        Some(Value::Int4(v)) => Ok(*v),
        _ => Err(bad_value()),
    }
}

/// A length or a position as `int4`.
fn count(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

/// The bits of each argument, which must have the same length. `op` is the name of the operation in the error.
fn same_length<'a>(args: &'a [Value], op: &str) -> Result<(&'a str, &'a str)> {
    let (a, b) = (bits(args, 0)?, bits(args, 1)?);
    if a.len() != b.len() {
        return Err(Error::new(
            SqlState::STRING_DATA_LENGTH_MISMATCH,
            format!("cannot {op} bit strings of different sizes"),
        ));
    }
    Ok((a, b))
}

/// The bits of `op` on each pair of bits.
fn bitwise(a: &str, b: &str, op: fn(bool, bool) -> bool) -> Value {
    let out =
        a.bytes().zip(b.bytes()).map(|(x, y)| if op(x == b'1', y == b'1') { '1' } else { '0' });
    Value::Text(out.collect())
}

fn bit_and(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same_length(args, "AND")?;
    Ok(bitwise(a, b, |x, y| x && y))
}

fn bit_or(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same_length(args, "OR")?;
    Ok(bitwise(a, b, |x, y| x || y))
}

fn bit_xor(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same_length(args, "XOR")?;
    Ok(bitwise(a, b, |x, y| x != y))
}

fn bit_not(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = bits(args, 0)?;
    Ok(Value::Text(a.bytes().map(|x| if x == b'1' { '0' } else { '1' }).collect()))
}

/// The bits moved `n` places to the left, or to the right for a negative `n`, with zeros in the empty places. The length does not change.
fn shift(a: &str, n: i64) -> Value {
    let len = a.len();
    let places = usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX).min(len);
    let zeros = "0".repeat(places);
    Value::Text(if n >= 0 {
        format!("{}{zeros}", &a[places..])
    } else {
        format!("{zeros}{}", &a[..len - places])
    })
}

fn shift_left(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(shift(bits(args, 0)?, i64::from(int4(args, 1)?)))
}

fn shift_right(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(shift(bits(args, 0)?, -i64::from(int4(args, 1)?)))
}

fn concat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(format!("{}{}", bits(args, 0)?, bits(args, 1)?)))
}

/// `22011`, `negative substring length not allowed`.
fn negative_length() -> Error {
    Error::new(SqlState::SUBSTRING_ERROR, "negative substring length not allowed")
}

/// `bitsubstring`: the bits from position `start`, from 1, with `len` bits or to the end. A start before 1 shortens the result.
fn bit_substring(a: &str, start: i32, len: Option<i32>) -> Result<String> {
    let bitlen = i64::try_from(a.len()).unwrap_or(i64::MAX);
    let s1 = i64::from(start.max(1));
    let e1 = match len {
        None => bitlen + 1,
        Some(len) if len < 0 => return Err(negative_length()),
        Some(len) => (i64::from(start) + i64::from(len)).min(bitlen + 1),
    };
    if s1 > bitlen || e1 <= s1 {
        return Ok(String::new());
    }
    let (Ok(from), Ok(to)) = (usize::try_from(s1 - 1), usize::try_from(e1 - 1)) else {
        return Err(bad_value());
    };
    Ok(a[from..to].to_string())
}

/// `bitsubstr` and `bitsubstr_no_len`.
fn substring(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let len = if args.len() > 2 { Some(int4(args, 2)?) } else { None };
    bit_substring(bits(args, 0)?, int4(args, 1)?, len).map(Value::Text)
}

/// `bitoverlay` and `bitoverlay_no_len`: the bits of the first string before position `start`, the second string, then the bits of the first string after `len` more bits. The length is the length of the second string when it is not given.
fn overlay(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = (bits(args, 0)?, bits(args, 1)?);
    let start = int4(args, 2)?;
    let len =
        if args.len() > 3 { int4(args, 3)? } else { i32::try_from(b.len()).unwrap_or(i32::MAX) };
    if start <= 0 {
        return Err(negative_length());
    }
    let Some(end) = start.checked_add(len) else {
        return Err(Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"));
    };
    let head = bit_substring(a, 1, Some(start - 1))?;
    let tail = bit_substring(a, end, None)?;
    Ok(Value::Text(format!("{head}{b}{tail}")))
}

/// `bitposition`: the position, from 1, of the first match of the second string in the first, or 0. An empty first string gives 0 and an empty second string gives 1.
fn position(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = (bits(args, 0)?, bits(args, 1)?);
    if a.is_empty() || b.len() > a.len() {
        return Ok(Value::Int4(0));
    }
    Ok(a.find(b).map_or(Value::Int4(0), |at| count(at + 1)))
}

/// `bitlength`, and `bit_length(bit)`, which is a function in SQL.
pub(crate) fn bit_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(count(bits(args, 0)?.len()))
}

fn octet_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(count(bits(args, 0)?.len().div_ceil(8)))
}

/// The index of bit `n` of `a`, from 0.
fn bit_index(a: &str, n: i32) -> Result<usize> {
    usize::try_from(n).ok().filter(|&i| i < a.len()).ok_or_else(|| {
        Error::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            format!(
                "bit index {n} out of valid range (0..{})",
                i64::try_from(a.len()).unwrap_or(i64::MAX) - 1
            ),
        )
    })
}

fn get_bit(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = bits(args, 0)?;
    let i = bit_index(a, int4(args, 1)?)?;
    Ok(Value::Int4(i32::from(a.as_bytes()[i] == b'1')))
}

fn set_bit(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = bits(args, 0)?;
    let i = bit_index(a, int4(args, 1)?)?;
    let new = match int4(args, 2)? {
        0 => "0",
        1 => "1",
        _ => return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "new bit must be 0 or 1")),
    };
    Ok(Value::Text(format!("{}{new}{}", &a[..i], &a[i + 1..])))
}

fn bit_count(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ones = bits(args, 0)?.bytes().filter(|&c| c == b'1').count();
    Ok(Value::Int8(i64::try_from(ones).unwrap_or(i64::MAX)))
}

/// `bitcmp`: `memcmp` of the bytes that both values have, which gives the difference of the first bytes that are not equal, then the order of the lengths as -1 or 1.
fn bit_cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = (bits(args, 0)?, bits(args, 1)?);
    let (x, y) = (types::bit_bytes(a), types::bit_bytes(b));
    let diff = x.iter().zip(&y).find(|(p, q)| p != q).map(|(p, q)| i32::from(*p) - i32::from(*q));
    Ok(Value::Int4(diff.unwrap_or(match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })))
}

/// The typmod argument and the flag of an explicit cast of a length cast.
fn typmod_args(args: &[Value]) -> Result<(i32, bool)> {
    let explicit = args.get(2).and_then(Value::as_bool).ok_or_else(bad_value)?;
    Ok((int4(args, 1)?, explicit))
}

/// `bit(bit, int4, bool)`: the length of the typmod.
fn bit_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (typmod, explicit) = typmod_args(args)?;
    types::bit_coerce(bits(args, 0)?, typmod, false, explicit).map(Value::Text).map_err(type_error)
}

/// `varbit(varbit, int4, bool)`: no more than the length of the typmod.
fn varbit_typmod(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (typmod, explicit) = typmod_args(args)?;
    types::bit_coerce(bits(args, 0)?, typmod, true, explicit).map(Value::Text).map_err(type_error)
}

/// `bitfromint4` and `bitfromint8`: the low bits of the number, with copies of the sign bit before them when the typmod is longer than the number. A typmod that is not valid gives 1 bit.
fn from_integer(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = args.first().and_then(Value::as_i64).ok_or_else(bad_value)?;
    let len =
        usize::try_from(int4(args, 1)?).ok().filter(|&n| n > 0 && n <= BIT_MAX_LEN).unwrap_or(1);
    let out = (0..len).rev().map(|place| {
        let set = if place >= 64 { a < 0 } else { (a >> place) & 1 == 1 };
        if set { '1' } else { '0' }
    });
    Ok(Value::Text(out.collect()))
}

/// The bits as an unsigned number, or `None` for more than `max` bits.
fn unsigned(a: &str, max: usize) -> Option<u64> {
    (a.len() <= max).then(|| a.bytes().fold(0, |v, c| (v << 1) | u64::from(c == b'1')))
}

fn to_int4(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = unsigned(bits(args, 0)?, 32)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))?;
    Ok(Value::Int4(v.cast_signed()))
}

fn to_int8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let v = unsigned(bits(args, 0)?, 64)
        .ok_or_else(|| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?;
    Ok(Value::Int8(v.cast_signed()))
}

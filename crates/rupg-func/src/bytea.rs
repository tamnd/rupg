//! The functions of `bytea` from `bytea.c`, `like.c`, `oracle_compat.c`, `cryptohashfuncs.c` and `pg_crc.c`: concatenation, `substring`, `overlay`, `position`, the bytes and the bits, `LIKE`, `trim`, the casts to and from the integer types, the hashes and the CRC values.

use md5::Md5;
use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

use crate::text::like_bytes;
use crate::{Call, Kernel, bad_value};

/// The kernel of a `bytea` function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "byteacat" => byteacat,
        "byteaoverlay" | "byteaoverlay_no_len" => byteaoverlay,
        "bytea_substr" | "bytea_substr_no_len" => bytea_substr,
        "bytea_bit_count" => bytea_bit_count,
        "byteapos" => byteapos,
        "byteaGetByte" => get_byte,
        "byteaGetBit" => get_bit,
        "byteaSetByte" => set_byte,
        "byteaSetBit" => set_bit,
        "bytea_reverse" => bytea_reverse,
        "bytea_int2" => bytea_int2,
        "bytea_int4" => bytea_int4,
        "bytea_int8" => bytea_int8,
        "int2_bytea" | "int4_bytea" | "int8_bytea" => int_bytea,
        "bytealike" => bytealike,
        "byteanlike" => byteanlike,
        "like_escape_bytea" => like_escape_bytea,
        "byteatrim" => byteatrim,
        "bytealtrim" => bytealtrim,
        "byteartrim" => byteartrim,
        "md5_bytea" => md5_bytea,
        "sha224_bytea" => sha224_bytea,
        "sha256_bytea" => sha256_bytea,
        "sha384_bytea" => sha384_bytea,
        "sha512_bytea" => sha512_bytea,
        "crc32_bytea" => crc32_bytea,
        "crc32c_bytea" => crc32c_bytea,
        _ => return None,
    })
}

/// The bytes of an argument.
fn bytes(args: &[Value], at: usize) -> Result<&[u8]> {
    match args.get(at) {
        Some(Value::Bytea(b)) => Ok(b),
        _ => Err(bad_value()),
    }
}

/// The integer value of an argument.
fn int(args: &[Value], at: usize) -> Result<i64> {
    args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)
}

fn negative_length() -> Error {
    Error::new(SqlState::SUBSTRING_ERROR, "negative substring length not allowed")
}

/// `2202E` for an index that is not in the value.
fn out_of_index(n: i64, len: i64) -> Error {
    Error::new(
        SqlState::ARRAY_SUBSCRIPT_ERROR,
        format!("index {n} out of valid range, 0..{}", len - 1),
    )
}

fn len_i64(b: &[u8]) -> i64 {
    i64::try_from(b.len()).unwrap_or(i64::MAX)
}

/// `byteacat`: `||`.
fn byteacat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (left, right) = (bytes(args, 0)?, bytes(args, 1)?);
    let mut out = Vec::with_capacity(left.len() + right.len());
    out.extend_from_slice(left);
    out.extend_from_slice(right);
    Ok(Value::Bytea(out))
}

/// `bytea_substring`: the bytes from the position `start`, which starts at 1, for `length` bytes, or to the end when `length` is `None`.
fn substring(b: &[u8], start: i64, length: Option<i64>) -> Result<&[u8]> {
    let first = start.max(1);
    let end = match length {
        Some(length) if length < 0 => return Err(negative_length()),
        Some(length) => start.saturating_add(length),
        None => i64::MAX,
    };
    if end <= first {
        return Ok(&[]);
    }
    let skip = usize::try_from(first - 1).unwrap_or(usize::MAX).min(b.len());
    let take = usize::try_from(end - first).unwrap_or(usize::MAX).min(b.len() - skip);
    Ok(&b[skip..skip + take])
}

/// `bytea_substr` and `bytea_substr_no_len`.
fn bytea_substr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let length = if args.len() > 2 { Some(int(args, 2)?) } else { None };
    Ok(Value::Bytea(substring(bytes(args, 0)?, int(args, 1)?, length)?.to_vec()))
}

/// `byteaoverlay` and `byteaoverlay_no_len`: the length is the length of the new bytes when it is not given.
fn byteaoverlay(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (b, placing, from) = (bytes(args, 0)?, bytes(args, 1)?, int(args, 2)?);
    let length = if args.len() > 3 { int(args, 3)? } else { len_i64(placing) };
    if from <= 0 {
        return Err(negative_length());
    }
    let after = i32::try_from(from + length)
        .map_err(|_| Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))?;
    let head = substring(b, 1, Some(from - 1))?;
    let tail = substring(b, i64::from(after), None)?;
    let mut out = Vec::with_capacity(head.len() + placing.len() + tail.len());
    out.extend_from_slice(head);
    out.extend_from_slice(placing);
    out.extend_from_slice(tail);
    Ok(Value::Bytea(out))
}

/// `bytea_bit_count`: the number of bits that are set.
fn bytea_bit_count(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ones: u64 = bytes(args, 0)?.iter().map(|b| u64::from(b.count_ones())).sum();
    Ok(Value::Int8(i64::try_from(ones).unwrap_or(i64::MAX)))
}

/// `byteapos`: the position of the first match, which starts at 1, or 0. An empty pattern gives 1.
fn byteapos(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (b, pattern) = (bytes(args, 0)?, bytes(args, 1)?);
    if pattern.is_empty() {
        return Ok(Value::Int4(1));
    }
    let at = b.windows(pattern.len()).position(|w| w == pattern).map_or(0, |at| at + 1);
    Ok(Value::Int4(i32::try_from(at).unwrap_or(i32::MAX)))
}

/// The index of a byte, or an error when it is not in the value.
fn byte_index(b: &[u8], n: i64) -> Result<usize> {
    usize::try_from(n).ok().filter(|&at| at < b.len()).ok_or_else(|| out_of_index(n, len_i64(b)))
}

/// The index of a byte and the number of a bit in it, or an error when the bit is not in the value.
fn bit_index(b: &[u8], n: i64) -> Result<(usize, u32)> {
    let bits = len_i64(b).saturating_mul(8);
    if n < 0 || n >= bits {
        return Err(out_of_index(n, bits));
    }
    let at = usize::try_from(n / 8).map_err(|_| bad_value())?;
    let bit = u32::try_from(n % 8).map_err(|_| bad_value())?;
    Ok((at, bit))
}

/// `byteaGetByte`: `get_byte`.
fn get_byte(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let b = bytes(args, 0)?;
    Ok(Value::Int4(i32::from(b[byte_index(b, int(args, 1)?)?])))
}

/// `byteaGetBit`: `get_bit`. Bit 0 is the least significant bit of the first byte.
fn get_bit(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let b = bytes(args, 0)?;
    let (at, bit) = bit_index(b, int(args, 1)?)?;
    Ok(Value::Int4(i32::from((b[at] >> bit) & 1)))
}

/// `byteaSetByte`: `set_byte`. Only the low 8 bits of the new value are kept.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn set_byte(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let b = bytes(args, 0)?;
    let at = byte_index(b, int(args, 1)?)?;
    let mut out = b.to_vec();
    out[at] = int(args, 2)? as u8;
    Ok(Value::Bytea(out))
}

/// `byteaSetBit`: `set_bit`.
fn set_bit(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let b = bytes(args, 0)?;
    let (at, bit) = bit_index(b, int(args, 1)?)?;
    let new_bit = int(args, 2)?;
    if new_bit != 0 && new_bit != 1 {
        return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "new bit must be 0 or 1"));
    }
    let mut out = b.to_vec();
    if new_bit == 0 {
        out[at] &= !(1 << bit);
    } else {
        out[at] |= 1 << bit;
    }
    Ok(Value::Bytea(out))
}

/// `bytea_reverse`: `reverse`.
fn bytea_reverse(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(bytes(args, 0)?.iter().rev().copied().collect()))
}

/// The value of the bytes as an unsigned integer, with the most significant byte first, or an error when there are more than `size` bytes.
fn big_endian(args: &[Value], size: usize, name: &str) -> Result<u64> {
    let b = bytes(args, 0)?;
    if b.len() > size {
        return Err(Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("{name} out of range"),
        ));
    }
    Ok(b.iter().fold(0, |acc, &byte| acc << 8 | u64::from(byte)))
}

/// `bytea_int2`: the cast to `smallint`. The bits of the result are the bits of the value, so `'\xffff'` is -1.
#[allow(clippy::cast_possible_truncation)]
fn bytea_int2(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int2(big_endian(args, 2, "smallint")? as u16 as i16))
}

/// `bytea_int4`: the cast to `integer`.
#[allow(clippy::cast_possible_truncation)]
fn bytea_int4(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(big_endian(args, 4, "integer")? as u32 as i32))
}

/// `bytea_int8`: the cast to `bigint`.
#[allow(clippy::cast_possible_wrap)]
fn bytea_int8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int8(big_endian(args, 8, "bigint")? as i64))
}

/// `int2_bytea`, `int4_bytea` and `int8_bytea`: the binary form of the integer, with the most significant byte first.
fn int_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(match args.first() {
        Some(Value::Int2(v)) => v.to_be_bytes().to_vec(),
        Some(Value::Int4(v)) => v.to_be_bytes().to_vec(),
        Some(Value::Int8(v)) => v.to_be_bytes().to_vec(),
        _ => return Err(bad_value()),
    }))
}

fn bytealike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(like_bytes(bytes(args, 0)?, bytes(args, 1)?)?))
}

fn byteanlike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(!like_bytes(bytes(args, 0)?, bytes(args, 1)?)?))
}

/// `like_escape_bytea`: the pattern with `\` as its escape byte, as `SB_do_like_escape` makes it.
fn like_escape_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (pattern, escape) = (bytes(args, 0)?, bytes(args, 1)?);
    let mut out = Vec::with_capacity(pattern.len());
    let Some(&e) = escape.first() else {
        for &c in pattern {
            if c == b'\\' {
                out.push(b'\\');
            }
            out.push(c);
        }
        return Ok(Value::Bytea(out));
    };
    if escape.len() > 1 {
        return Err(Error::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid escape string")
            .with_hint("Escape string must be empty or one character."));
    }
    if e == b'\\' {
        return Ok(Value::Bytea(pattern.to_vec()));
    }
    let mut after_escape = false;
    for &c in pattern {
        if c == e && !after_escape {
            out.push(b'\\');
            after_escape = true;
        } else if c == b'\\' {
            out.push(b'\\');
            if !after_escape {
                out.push(b'\\');
            }
            after_escape = false;
        } else {
            out.push(c);
            after_escape = false;
        }
    }
    Ok(Value::Bytea(out))
}

/// `dobyteatrim`: the value without the bytes of the set at the start, at the end or at both.
fn trim(args: &[Value], start: bool, end: bool) -> Result<Value> {
    let (mut b, set) = (bytes(args, 0)?, bytes(args, 1)?);
    if start {
        while let Some((_, rest)) = b.split_first().filter(|(c, _)| set.contains(c)) {
            b = rest;
        }
    }
    if end {
        while let Some((_, rest)) = b.split_last().filter(|(c, _)| set.contains(c)) {
            b = rest;
        }
    }
    Ok(Value::Bytea(b.to_vec()))
}

/// `byteatrim`: `btrim` and `trim(BOTH ...)`.
fn byteatrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim(args, true, true)
}

/// `bytealtrim`: `ltrim` and `trim(LEADING ...)`.
fn bytealtrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim(args, true, false)
}

/// `byteartrim`: `rtrim` and `trim(TRAILING ...)`.
fn byteartrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim(args, false, true)
}

/// `md5_bytea`: the MD5 hash in lower case hexadecimal.
fn md5_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let digest = Md5::digest(bytes(args, 0)?);
    Ok(Value::Text(digest.iter().map(|b| format!("{b:02x}")).collect()))
}

fn sha224_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(Sha224::digest(bytes(args, 0)?).to_vec()))
}

fn sha256_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(Sha256::digest(bytes(args, 0)?).to_vec()))
}

fn sha384_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(Sha384::digest(bytes(args, 0)?).to_vec()))
}

fn sha512_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(Sha512::digest(bytes(args, 0)?).to_vec()))
}

/// The table of a reflected CRC of 32 bits for the polynomial `poly`.
const fn crc_table(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { crc >> 1 ^ poly } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// The CRC-32 of ISO 3309 and zlib.
const CRC32: [u32; 256] = crc_table(0xEDB8_8320);
/// The CRC-32C of Castagnoli.
const CRC32C: [u32; 256] = crc_table(0x82F6_3B78);

/// The CRC of the bytes with the table, with the start value and the final value of all ones.
fn crc(table: &[u32; 256], b: &[u8]) -> Value {
    let crc = b
        .iter()
        .fold(!0u32, |crc, &byte| table[((crc ^ u32::from(byte)) & 0xff) as usize] ^ crc >> 8);
    Value::Int8(i64::from(!crc))
}

/// `crc32_bytea`: `crc32`.
fn crc32_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(crc(&CRC32, bytes(args, 0)?))
}

/// `crc32c_bytea`: `crc32c`.
fn crc32c_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(crc(&CRC32C, bytes(args, 0)?))
}

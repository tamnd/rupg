//! The names and the numbers of the encodings, as `encnames.c` gives them: `pg_char_to_encoding` and `pg_encoding_to_char`, and the SQL functions with these names. The conversions of `mbutils.c` and `conv.c` between UTF8 and the single-byte encodings, and between UTF8 and EUC_KR: `convert`, `convert_from`, `convert_to` and `length(bytea, name)`. The function `to_ascii` of `ascii.c`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::generated::double_byte::{DoubleByte, TABLES as DOUBLE_BYTE};
use crate::generated::single_byte::TABLES;
use crate::{Call, Kernel, bad_value, not_yet};

/// `NAMEDATALEN`: a name of this length or longer is not in the table.
const NAMEDATALEN: usize = 64;

/// `pg_enc2name_tbl`: the name of each encoding by its number. The number 7, which was `MULE_INTERNAL`, has no encoding.
const NAMES: [&str; 42] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

/// `pg_encname_tbl`: each name and alias of an encoding, cleaned as `clean_encoding_name` does, with the name of the encoding.
const ALIASES: &[(&str, &str)] = &[
    ("abc", "WIN1258"),
    ("alt", "WIN866"),
    ("big5", "BIG5"),
    ("euccn", "EUC_CN"),
    ("eucjis2004", "EUC_JIS_2004"),
    ("eucjp", "EUC_JP"),
    ("euckr", "EUC_KR"),
    ("euctw", "EUC_TW"),
    ("gb18030", "GB18030"),
    ("gbk", "GBK"),
    ("iso88591", "LATIN1"),
    ("iso885910", "LATIN6"),
    ("iso885913", "LATIN7"),
    ("iso885914", "LATIN8"),
    ("iso885915", "LATIN9"),
    ("iso885916", "LATIN10"),
    ("iso88592", "LATIN2"),
    ("iso88593", "LATIN3"),
    ("iso88594", "LATIN4"),
    ("iso88595", "ISO_8859_5"),
    ("iso88596", "ISO_8859_6"),
    ("iso88597", "ISO_8859_7"),
    ("iso88598", "ISO_8859_8"),
    ("iso88599", "LATIN5"),
    ("johab", "JOHAB"),
    ("koi8", "KOI8R"),
    ("koi8r", "KOI8R"),
    ("koi8u", "KOI8U"),
    ("latin1", "LATIN1"),
    ("latin10", "LATIN10"),
    ("latin2", "LATIN2"),
    ("latin3", "LATIN3"),
    ("latin4", "LATIN4"),
    ("latin5", "LATIN5"),
    ("latin6", "LATIN6"),
    ("latin7", "LATIN7"),
    ("latin8", "LATIN8"),
    ("latin9", "LATIN9"),
    ("mskanji", "SJIS"),
    ("shiftjis", "SJIS"),
    ("shiftjis2004", "SHIFT_JIS_2004"),
    ("sjis", "SJIS"),
    ("sqlascii", "SQL_ASCII"),
    ("tcvn", "WIN1258"),
    ("tcvn5712", "WIN1258"),
    ("uhc", "UHC"),
    ("unicode", "UTF8"),
    ("utf8", "UTF8"),
    ("vscii", "WIN1258"),
    ("win", "WIN1251"),
    ("win1250", "WIN1250"),
    ("win1251", "WIN1251"),
    ("win1252", "WIN1252"),
    ("win1253", "WIN1253"),
    ("win1254", "WIN1254"),
    ("win1255", "WIN1255"),
    ("win1256", "WIN1256"),
    ("win1257", "WIN1257"),
    ("win1258", "WIN1258"),
    ("win866", "WIN866"),
    ("win874", "WIN874"),
    ("win932", "SJIS"),
    ("win936", "GBK"),
    ("win949", "UHC"),
    ("win950", "BIG5"),
    ("windows1250", "WIN1250"),
    ("windows1251", "WIN1251"),
    ("windows1252", "WIN1252"),
    ("windows1253", "WIN1253"),
    ("windows1254", "WIN1254"),
    ("windows1255", "WIN1255"),
    ("windows1256", "WIN1256"),
    ("windows1257", "WIN1257"),
    ("windows1258", "WIN1258"),
    ("windows866", "WIN866"),
    ("windows874", "WIN874"),
    ("windows932", "SJIS"),
    ("windows936", "GBK"),
    ("windows949", "UHC"),
    ("windows950", "BIG5"),
];

/// `pg_char_to_encoding` with the name of the encoding: the canonical name of an encoding name or alias, without regard to case and to the characters that are not letters or digits.
#[must_use]
pub fn encoding(name: &str) -> Option<&'static str> {
    if name.len() >= NAMEDATALEN {
        return None;
    }
    let key: String =
        name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    ALIASES.iter().find(|(alias, _)| *alias == key).map(|(_, name)| *name)
}

/// `pg_valid_server_encoding`: true for the name or alias of an encoding that a database can have. The encodings after `KOI8U` are only for a client.
pub(crate) fn valid_server_encoding(name: &str) -> bool {
    let last = NAMES.iter().position(|n| *n == "KOI8U").unwrap_or_default();
    usize::try_from(char_to_encoding(name)).is_ok_and(|n| n <= last)
}

/// `pg_char_to_encoding`: the number of an encoding name or alias, or -1.
#[must_use]
pub fn char_to_encoding(name: &str) -> i32 {
    encoding(name)
        .and_then(|name| NAMES.iter().position(|n| *n == name))
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(-1)
}

/// `pg_encoding_to_char`: the name of an encoding by its number, or an empty string for a number that has no encoding.
#[must_use]
pub fn encoding_to_char(number: i64) -> &'static str {
    usize::try_from(number).ok().and_then(|n| NAMES.get(n)).copied().unwrap_or_default()
}

/// The encoding of each database, which is always UTF8.
const DATABASE: &str = "UTF8";

/// The conversions of `pg_conversion` between two encodings that are not UTF8. rupg does not have them yet.
const DIRECT: [(&str, &str); 10] = [
    ("KOI8R", "WIN1251"),
    ("KOI8R", "WIN866"),
    ("WIN866", "WIN1251"),
    ("ISO_8859_5", "KOI8R"),
    ("ISO_8859_5", "WIN1251"),
    ("ISO_8859_5", "WIN866"),
    ("EUC_JP", "SJIS"),
    ("EUC_TW", "BIG5"),
    ("LATIN2", "WIN1250"),
    ("EUC_JIS_2004", "SHIFT_JIS_2004"),
];

/// The code points of the bytes from 0x80 to 0xFF of a single-byte encoding. LATIN1 has no table, because each of its bytes is the code point with the same number.
fn table(encoding: &str) -> Option<&'static [u16; 128]> {
    TABLES.iter().find(|(name, _)| *name == encoding).map(|(_, table)| table)
}

/// `pg_euckr_verifychar`: the length of the EUC_KR character at the start of `rest`, or None for a bad character. A character with the high bit is two bytes from 0xA1 to 0xFE.
fn euc_kr_length(rest: &[u8]) -> Option<usize> {
    let valid = |b: &u8| (0xa1..=0xfe).contains(b);
    match rest {
        [0, ..] | [] => None,
        [b, ..] if *b < 0x80 => Some(1),
        [a, b, ..] if valid(a) && valid(b) => Some(2),
        _ => None,
    }
}

/// The map of EUC_KR.
fn euc_kr_map() -> &'static DoubleByte {
    DOUBLE_BYTE.iter().find(|m| m.name == "EUC_KR").expect("the map of EUC_KR")
}

/// The code point of the pair of bytes `a` and `b` in EUC_KR, as `pg_mb_radix_conv` finds it in `euc_kr_to_unicode_tree`, or None for a pair with no character.
fn euc_kr_decode(a: u8, b: u8) -> Option<char> {
    let DoubleByte { first, second, table, .. } = euc_kr_map();
    if !(first.0..=first.1).contains(&a) || !(second.0..=second.1).contains(&b) {
        return None;
    }
    let width = usize::from(second.1 - second.0) + 1;
    let c = table[usize::from(a - first.0) * width + usize::from(b - second.0)];
    if c == 0 { None } else { char::from_u32(u32::from(c)) }
}

/// The pair of bytes of a character in EUC_KR, as `euc_kr_from_unicode_tree` gives it, or None for a character that EUC_KR does not have. The map from UTF-8 is the inverse of the map to UTF-8.
fn euc_kr_encode(c: char) -> Option<[u8; 2]> {
    static INVERSE: OnceLock<HashMap<u16, [u8; 2]>> = OnceLock::new();
    let inverse = INVERSE.get_or_init(|| {
        let DoubleByte { first, second, table, .. } = euc_kr_map();
        let width = usize::from(second.1 - second.0) + 1;
        let mut inverse = HashMap::new();
        for (n, c) in table.iter().enumerate().filter(|(_, c)| **c != 0) {
            let (a, b) = (n / width, n % width);
            let pair = [first.0 + a as u8, second.0 + b as u8];
            inverse.entry(*c).or_insert(pair);
        }
        inverse
    });
    inverse.get(&u16::try_from(u32::from(c)).ok()?).copied()
}

/// True for an encoding in which each character is one byte.
fn single_byte(encoding: &str) -> bool {
    matches!(encoding, "SQL_ASCII" | "LATIN1") || table(encoding).is_some()
}

/// The code point of a byte of a single-byte encoding, or None for a byte with no character.
fn decode_byte(encoding: &str, b: u8) -> Option<char> {
    if b < 0x80 || encoding == "LATIN1" {
        return Some(char::from(b));
    }
    let c = table(encoding)?[usize::from(b - 0x80)];
    if c == 0 { None } else { char::from_u32(u32::from(c)) }
}

/// The byte of a character in a single-byte encoding, or None for a character that the encoding does not have.
fn encode_char(encoding: &str, c: char) -> Option<u8> {
    if let Ok(b) = u8::try_from(c)
        && (b < 0x80 || encoding == "LATIN1")
    {
        return Some(b);
    }
    let c = u16::try_from(u32::from(c)).ok().filter(|c| *c != 0)?;
    let at = table(encoding)?.iter().position(|x| *x == c)?;
    u8::try_from(at + 0x80).ok()
}

/// `pg_utf_mblen`: the length of a UTF-8 character from its first byte.
fn utf8_length(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The bytes of a character as the error messages give them: at most 8 bytes, each as `0x` and two hex digits.
fn byte_text(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// The length of the character at the start of `rest` in an encoding, as `pg_encoding_mblen_or_incomplete` gives it, but not more than the bytes in `rest`.
fn char_length(encoding: &str, rest: &[u8]) -> usize {
    let length = match (encoding, rest.first()) {
        ("UTF8", Some(b)) => utf8_length(*b),
        ("EUC_KR", Some(b)) if *b >= 0x80 => 2,
        _ => 1,
    };
    length.min(rest.len())
}

/// `report_invalid_encoding`: the error of the bad character at the start of `rest`.
pub(crate) fn invalid(encoding: &str, rest: &[u8]) -> Error {
    let bytes = &rest[..char_length(encoding, rest)];
    Error::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"{encoding}\": {}", byte_text(bytes)),
    )
}

/// `report_untranslatable_char`: the error of the character at the start of `rest`, which the destination encoding does not have.
fn untranslatable(from: &str, to: &str, rest: &[u8]) -> Error {
    let bytes = &rest[..char_length(from, rest)];
    Error::new(
        SqlState::UNTRANSLATABLE_CHARACTER,
        format!(
            "character with byte sequence {} in encoding \"{from}\" has no equivalent in encoding \"{to}\"",
            byte_text(bytes)
        ),
    )
}

/// The length of the valid UTF-8 at the start of `bytes`. A zero byte is not valid.
fn utf8_valid_length(bytes: &[u8]) -> usize {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => end,
        Err(e) => e.valid_up_to(),
    }
}

/// `pg_verify_mbstr_len`: the number of characters of a string in an encoding, or the error of its first bad character.
fn verify(encoding: &str, bytes: &[u8]) -> Result<usize> {
    if encoding == "UTF8" {
        let ok = utf8_valid_length(bytes);
        if ok < bytes.len() {
            return Err(invalid(encoding, &bytes[ok..]));
        }
        return Ok(bytes.iter().filter(|b| (**b & 0xc0) != 0x80).count());
    }
    if encoding == "EUC_KR" {
        let (mut at, mut count) = (0, 0);
        while at < bytes.len() {
            at += euc_kr_length(&bytes[at..]).ok_or_else(|| invalid(encoding, &bytes[at..]))?;
            count += 1;
        }
        return Ok(count);
    }
    // Each encoding has the ASCII characters as single bytes.
    if !single_byte(encoding) && !bytes.is_ascii() {
        return Err(not_yet(format_args!("a string in the encoding {encoding}")));
    }
    match bytes.iter().position(|b| *b == 0) {
        Some(at) => Err(invalid(encoding, &bytes[at..])),
        None => Ok(bytes.len()),
    }
}

/// `pg_do_encoding_conversion`: a string that `verify` checked in the encoding `from`, in the encoding `to`.
fn conversion<'a>(bytes: &'a [u8], from: &str, to: &str) -> Result<Cow<'a, [u8]>> {
    if bytes.is_empty() || from == to || to == "SQL_ASCII" {
        return Ok(Cow::Borrowed(bytes));
    }
    if from == "SQL_ASCII" {
        verify(to, bytes)?;
        return Ok(Cow::Borrowed(bytes));
    }
    if from == "UTF8" && single_byte(to) {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid(from, bytes))?;
        let mut out = Vec::with_capacity(bytes.len());
        for (at, c) in text.char_indices() {
            out.push(encode_char(to, c).ok_or_else(|| untranslatable(from, to, &bytes[at..]))?);
        }
        return Ok(Cow::Owned(out));
    }
    if to == "UTF8" && single_byte(from) {
        let mut out = String::with_capacity(bytes.len());
        for (at, b) in bytes.iter().enumerate() {
            out.push(decode_byte(from, *b).ok_or_else(|| untranslatable(from, to, &bytes[at..]))?);
        }
        return Ok(Cow::Owned(out.into_bytes()));
    }
    // `euc_kr_to_utf8` and `utf8_to_euc_kr`: `LocalToUtf` and `UtfToLocal` with the maps of EUC_KR. The ASCII characters stay as they are.
    if from == "EUC_KR" && to == "UTF8" {
        let mut out = String::with_capacity(bytes.len() * 3 / 2);
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] < 0x80 {
                out.push(char::from(bytes[at]));
                at += 1;
                continue;
            }
            let c = bytes.get(at + 1).and_then(|b| euc_kr_decode(bytes[at], *b));
            out.push(c.ok_or_else(|| untranslatable(from, to, &bytes[at..]))?);
            at += 2;
        }
        return Ok(Cow::Owned(out.into_bytes()));
    }
    if from == "UTF8" && to == "EUC_KR" {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid(from, bytes))?;
        let mut out = Vec::with_capacity(bytes.len());
        for (at, c) in text.char_indices() {
            if c.is_ascii() {
                out.push(c as u8);
                continue;
            }
            out.extend(euc_kr_encode(c).ok_or_else(|| untranslatable(from, to, &bytes[at..]))?);
        }
        return Ok(Cow::Owned(out));
    }
    let direct = DIRECT.iter().any(|&(a, b)| (a, b) == (from, to) || (b, a) == (from, to));
    if direct || from == "UTF8" || to == "UTF8" {
        // Each conversion keeps the ASCII characters as they are.
        if bytes.is_ascii() {
            return Ok(Cow::Borrowed(bytes));
        }
        return Err(not_yet(format_args!("the conversion from {from} to {to}")));
    }
    Err(Error::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("default conversion function for encoding \"{from}\" to \"{to}\" does not exist"),
    ))
}

/// `pg_convert`: a string in the encoding `from` in the encoding `to`, with the names of the encodings.
fn convert_bytes<'a>(bytes: &'a [u8], from: &str, to: &str) -> Result<Cow<'a, [u8]>> {
    let source = encoding(from).ok_or_else(|| {
        Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid source encoding name \"{from}\""),
        )
    })?;
    let destination = encoding(to).ok_or_else(|| {
        Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid destination encoding name \"{to}\""),
        )
    })?;
    verify(source, bytes)?;
    conversion(bytes, source, destination)
}

/// The SQL function `convert(bytea, name, name)`.
fn pg_convert(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bytea(bytes), from, to] = args else { return Err(bad_value()) };
    let (from, to) = (from.as_str().ok_or_else(bad_value)?, to.as_str().ok_or_else(bad_value)?);
    Ok(Value::Bytea(convert_bytes(bytes, from, to)?.into_owned()))
}

/// The SQL function `convert_from(bytea, name)`: a string in an encoding as text of the database.
fn pg_convert_from(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bytea(bytes), from] = args else { return Err(bad_value()) };
    let out = convert_bytes(bytes, from.as_str().ok_or_else(bad_value)?, DATABASE)?;
    let text = String::from_utf8(out.into_owned()).map_err(|_| bad_value())?;
    Ok(Value::Text(text))
}

/// The SQL function `convert_to(text, name)`: text of the database in an encoding.
fn pg_convert_to(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Text(text), to] = args else { return Err(bad_value()) };
    let out = convert_bytes(text.as_bytes(), DATABASE, to.as_str().ok_or_else(bad_value)?)?;
    Ok(Value::Bytea(out.into_owned()))
}

/// The SQL function `length(bytea, name)`: the number of characters of a string in an encoding.
fn length_in_encoding(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bytea(bytes), name] = args else { return Err(bad_value()) };
    let name = name.as_str().ok_or_else(bad_value)?;
    let encoding = encoding(name).ok_or_else(|| {
        Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid encoding name \"{name}\""))
    })?;
    let count = verify(encoding, bytes)?;
    Ok(Value::Int4(i32::try_from(count).map_err(|_| bad_value())?))
}

/// `pg_to_ascii`: the text with each byte that is not ASCII changed to the ASCII character of the encoding for it. Only LATIN1, LATIN2, LATIN9 and WIN1250 have such characters.
fn to_ascii(text: &str, encoding: &str) -> Result<Value> {
    let (ascii, start): (&[u8], u8) = match encoding {
        "LATIN1" => (b"  cL Y  \"Ca  -R     'u .,      ?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty", 160),
        "LATIN2" => (b" A L LS \"SSTZ-ZZ a,l'ls ,sstz\"zzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt.", 160),
        "LATIN9" => (b"  cL YS sCa  -R     Zu .z   EeY?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty", 160),
        "WIN1250" => (b"  ' \"    %S<STZZ `'\"\".--  s>stzz   L A  \"CS  -RZ  ,l'u .,as L\"lzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt ", 128),
        _ => {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("encoding conversion from {encoding} to ASCII not supported"),
            ));
        }
    };
    let out: String = text
        .bytes()
        .map(|b| match b {
            0..0x80 => char::from(b),
            b if b < start => ' ',
            b => char::from(ascii[usize::from(b - start)]),
        })
        .collect();
    Ok(Value::Text(out))
}

/// The SQL function `to_ascii(text)`, for the encoding of the database.
fn to_ascii_default(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    to_ascii(args.first().and_then(Value::as_str).ok_or_else(bad_value)?, DATABASE)
}

/// The SQL function `to_ascii(text, name)`.
fn to_ascii_encname(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text, name] = args else { return Err(bad_value()) };
    let name = name.as_str().ok_or_else(bad_value)?;
    let encoding = encoding(name).ok_or_else(|| {
        Error::new(SqlState::UNDEFINED_OBJECT, format!("{name} is not a valid encoding name"))
    })?;
    to_ascii(text.as_str().ok_or_else(bad_value)?, encoding)
}

/// The SQL function `to_ascii(text, integer)`.
fn to_ascii_enc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text, number] = args else { return Err(bad_value()) };
    let number = number.as_i64().ok_or_else(bad_value)?;
    let encoding = encoding_to_char(number);
    if encoding.is_empty() {
        return Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("{number} is not a valid encoding code"),
        ));
    }
    to_ascii(text.as_str().ok_or_else(bad_value)?, encoding)
}

/// The SQL function `pg_char_to_encoding(name)`.
fn pg_char_to_encoding(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let name = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(Value::Int4(char_to_encoding(name)))
}

/// The SQL function `pg_encoding_to_char(integer)`.
fn pg_encoding_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let number = args.first().and_then(Value::as_i64).ok_or_else(bad_value)?;
    Ok(Value::text(encoding_to_char(number)))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "PG_char_to_encoding" => pg_char_to_encoding,
        "PG_encoding_to_char" => pg_encoding_to_char,
        "pg_convert" => pg_convert,
        "pg_convert_from" => pg_convert_from,
        "pg_convert_to" => pg_convert_to,
        "length_in_encoding" => length_in_encoding,
        "to_ascii_default" => to_ascii_default,
        "to_ascii_encname" => to_ascii_encname,
        "to_ascii_enc" => to_ascii_enc,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers() {
        assert_eq!(char_to_encoding("UTF8"), 6);
        assert_eq!(char_to_encoding("latin-1"), 8);
        assert_eq!(char_to_encoding("mule_internal"), -1);
        assert_eq!(char_to_encoding(""), -1);
        assert_eq!(encoding_to_char(6), "UTF8");
        assert_eq!(encoding_to_char(7), "");
        assert_eq!(encoding_to_char(41), "SHIFT_JIS_2004");
        assert_eq!(encoding_to_char(42), "");
        assert_eq!(encoding_to_char(-1), "");
        for (number, name) in NAMES.iter().enumerate().filter(|(_, n)| !n.is_empty()) {
            assert_eq!(usize::try_from(char_to_encoding(name)).ok(), Some(number));
        }
    }

    #[test]
    fn euc_kr_table() {
        let mut count = 0;
        for a in 0xa1..=0xfe {
            for b in 0xa1..=0xfe {
                if let Some(c) = euc_kr_decode(a, b) {
                    assert_eq!(euc_kr_encode(c), Some([a, b]), "{c}");
                    count += 1;
                }
            }
        }
        assert!(count > 8000, "{count}");
        assert_eq!(euc_kr_decode(0xb0, 0xa1), Some('가'));
    }

    #[test]
    fn single_byte_tables() {
        for (name, table) in &TABLES {
            assert!(NAMES.contains(name), "{name}");
            for (n, c) in table.iter().enumerate().filter(|(_, c)| **c != 0) {
                let b = u8::try_from(n + 0x80).unwrap();
                let c = char::from_u32(u32::from(*c)).unwrap();
                assert_eq!(decode_byte(name, b), Some(c));
                assert_eq!(encode_char(name, c), Some(b), "{name} {c}");
            }
        }
    }

    #[test]
    fn conversions() {
        let to = |s: &str, e: &str| convert_bytes(s.as_bytes(), "UTF8", e).map(Cow::into_owned);
        assert_eq!(to("aé€", "LATIN9").unwrap(), b"a\xe9\xa4");
        assert_eq!(to("é", "latin1").unwrap(), b"\xe9");
        let message = to("€", "LATIN1").unwrap_err().to_string();
        assert!(message.contains("0xe2 0x82 0xac in encoding \"UTF8\" has no"), "{message}");
        let from = |b: &[u8], e: &str| convert_bytes(b, e, "UTF8").map(Cow::into_owned);
        assert_eq!(from(b"\xa4", "LATIN9").unwrap(), "€".as_bytes());
        assert!(from(b"\x81", "WIN1252").is_err());
        assert!(from(b"a\x00", "LATIN1").is_err());
        assert_eq!(utf8_valid_length(b"ab\xc3\x28"), 2);
        assert_eq!(utf8_valid_length(b"a\x00b"), 1);
        assert_eq!(verify("UTF8", "aé€".as_bytes()).unwrap(), 3);
    }
}

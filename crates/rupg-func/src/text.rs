//! The string functions of `varlena.c`, `oracle_compat.c`, `like.c` and `quote.c` for the C locale and the UTF8 encoding.

use md5::{Digest, Md5};
use rupg_catalog::names::{NAMEDATALEN, clip};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::Category;
use rupg_types::{Value, oid};

use crate::io::base_type;
use crate::{Call, Kernel, bad_value, output};

/// `MaxAllocSize`: the largest value that PostgreSQL can allocate.
const MAX_ALLOC_SIZE: i64 = 0x3fff_ffff;
/// The 4 bytes of the header of a `varlena` value.
const VARHDRSZ: i64 = 4;
/// The longest character of UTF8 in bytes.
const MAX_CHAR_BYTES: i64 = 4;

/// The string of an argument.
fn arg(args: &[Value], at: usize) -> Result<&str> {
    args.get(at).and_then(Value::as_str).ok_or_else(bad_value)
}

/// The `int4` value of an argument.
fn int_arg(args: &[Value], at: usize) -> Result<i64> {
    args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)
}

fn too_large() -> Error {
    Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "requested length too large")
}

/// The prefix of a string with this number of characters.
fn prefix(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or(s, |(at, _)| &s[..at])
}

/// The suffix of a string after this number of characters.
fn after(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or("", |(at, _)| &s[at..])
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// An `int4` from a count of characters or bytes, which is never larger than a value can be.
fn count(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

/// `textcat`: `||`.
fn textcat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(format!("{}{}", arg(args, 0)?, arg(args, 1)?)))
}

/// `textlen`: the number of characters.
fn textlen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(count(char_count(arg(args, 0)?)))
}

/// `bpcharlen`: the number of characters without the trailing spaces.
fn bpcharlen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(count(char_count(arg(args, 0)?.trim_end_matches(' '))))
}

/// `textoctetlen` and `bpcharoctetlen`: the number of bytes.
fn octetlen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(count(arg(args, 0)?.len()))
}

/// `byteaoctetlen`.
fn byteaoctetlen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Bytea(b)) = args.first() else { return Err(bad_value()) };
    Ok(count(b.len()))
}

/// `lower` in the C locale: only the ASCII letters change.
fn lower(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(arg(args, 0)?.to_ascii_lowercase()))
}

/// `upper` in the C locale.
fn upper(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(arg(args, 0)?.to_ascii_uppercase()))
}

/// `initcap` in the C locale: an ASCII letter after a letter or a digit is in lower case, and the other ASCII letters are in upper case.
fn initcap(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let mut out = String::new();
    let mut after_word = false;
    for c in arg(args, 0)?.chars() {
        out.push(if after_word { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() });
        after_word = c.is_ascii_alphanumeric();
    }
    Ok(Value::Text(out))
}

/// Which ends of a string a trim removes.
#[derive(Clone, Copy)]
enum Ends {
    Left,
    Right,
    Both,
}

fn trim<'a>(s: &'a str, set: &str, ends: Ends) -> &'a str {
    let strip = |c: char| set.contains(c);
    match ends {
        Ends::Left => s.trim_start_matches(strip),
        Ends::Right => s.trim_end_matches(strip),
        Ends::Both => s.trim_matches(strip),
    }
}

fn trim_call(args: &[Value], ends: Ends) -> Result<Value> {
    let set = if args.len() > 1 { arg(args, 1)? } else { " " };
    Ok(Value::text(trim(arg(args, 0)?, set, ends)))
}

fn ltrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim_call(args, Ends::Left)
}

fn rtrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim_call(args, Ends::Right)
}

fn btrim(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    trim_call(args, Ends::Both)
}

/// `text_substr` and `text_substr_no_len`: the characters from a position, with a length or to the end.
fn substr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let s = arg(args, 0)?;
    let start = int_arg(args, 1)?;
    let first = start.max(1);
    let end = if args.len() > 2 {
        let length = int_arg(args, 2)?;
        if length < 0 {
            return Err(Error::new(
                SqlState::SUBSTRING_ERROR,
                "negative substring length not allowed",
            ));
        }
        start + length
    } else {
        i64::MAX
    };
    if end <= first {
        return Ok(Value::text(""));
    }
    let skip = usize::try_from(first - 1).unwrap_or(usize::MAX);
    let take = usize::try_from(end - first).unwrap_or(usize::MAX);
    Ok(Value::text(prefix(after(s, skip), take)))
}

/// `textpos`: the position of the first character of the first match, or 0.
fn textpos(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, needle) = (arg(args, 0)?, arg(args, 1)?);
    Ok(count(s.find(needle).map_or(0, |at| char_count(&s[..at]) + 1)))
}

/// `replace_text`.
fn replace(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, from, to) = (arg(args, 0)?, arg(args, 1)?, arg(args, 2)?);
    if from.is_empty() {
        return Ok(Value::text(s));
    }
    Ok(Value::Text(s.replace(from, to)))
}

/// `split_part`: a field by its position from the start, or from the end for a negative position.
fn split_part(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, delimiter, n) = (arg(args, 0)?, arg(args, 1)?, int_arg(args, 2)?);
    if n == 0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "field position must not be zero",
        ));
    }
    if s.is_empty() {
        return Ok(Value::text(""));
    }
    let fields: Vec<&str> =
        if delimiter.is_empty() { vec![s] } else { s.split(delimiter).collect() };
    let len = i64::try_from(fields.len()).unwrap_or(i64::MAX);
    let index = if n > 0 { n - 1 } else { len + n };
    let field = usize::try_from(index).ok().and_then(|i| fields.get(i)).copied().unwrap_or("");
    Ok(Value::text(field))
}

/// `to_hex32` and `to_hex64`: the bits of the number in hexadecimal.
#[allow(clippy::cast_sign_loss)]
fn to_hex(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(match args.first() {
        Some(Value::Int4(v)) => format!("{:x}", *v as u32),
        Some(Value::Int8(v)) => format!("{:x}", *v as u64),
        _ => return Err(bad_value()),
    }))
}

/// `repeat`: a negative count is 0.
fn repeat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, n) = (arg(args, 0)?, int_arg(args, 1)?.max(0));
    let bytes = i64::try_from(s.len()).unwrap_or(i64::MAX);
    match bytes.checked_mul(n).and_then(|t| t.checked_add(VARHDRSZ)) {
        Some(total) if total <= MAX_ALLOC_SIZE && i32::try_from(total).is_ok() => {}
        _ => return Err(too_large()),
    }
    Ok(Value::Text(s.repeat(usize::try_from(n).unwrap_or(0))))
}

/// `text_reverse`.
fn reverse(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(arg(args, 0)?.chars().rev().collect()))
}

/// `text_left`: the first characters, or all but the last characters for a negative count.
fn left(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, n) = (arg(args, 0)?, int_arg(args, 1)?);
    let keep = if n >= 0 {
        usize::try_from(n).unwrap_or(usize::MAX)
    } else {
        char_count(s).saturating_sub(usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX))
    };
    Ok(Value::text(prefix(s, keep)))
}

/// `text_right`: the last characters, or all but the first characters for a negative count.
fn right(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s, n) = (arg(args, 0)?, int_arg(args, 1)?);
    let skip = if n >= 0 {
        char_count(s).saturating_sub(usize::try_from(n).unwrap_or(usize::MAX))
    } else {
        usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX)
    };
    Ok(Value::text(after(s, skip)))
}

/// `lpad` and `rpad`: the string cut or filled to a number of characters.
fn pad(args: &[Value], fill: &str, at_left: bool) -> Result<Value> {
    let (s, len) = (arg(args, 0)?, int_arg(args, 1)?.max(0));
    if len * MAX_CHAR_BYTES + VARHDRSZ > MAX_ALLOC_SIZE {
        return Err(too_large());
    }
    let len = usize::try_from(len).unwrap_or(0);
    let have = char_count(s);
    if have >= len || fill.is_empty() {
        return Ok(Value::text(prefix(s, len)));
    }
    let filler: String = fill.chars().cycle().take(len - have).collect();
    Ok(Value::Text(if at_left { filler + s } else { format!("{s}{filler}") }))
}

fn lpad(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    pad(args, arg(args, 2)?, true)
}

fn rpad(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    pad(args, arg(args, 2)?, false)
}

/// `lpad(text, int4)`, which is a function in SQL that fills with spaces.
pub(crate) fn lpad_space(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    pad(args, " ", true)
}

/// `rpad(text, int4)`, which is a function in SQL that fills with spaces.
pub(crate) fn rpad_space(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    pad(args, " ", false)
}

/// `ascii`: the code point of the first character, or 0.
fn ascii(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(
        arg(args, 0)?.chars().next().map_or(0, |c| i32::try_from(u32::from(c)).unwrap_or(0)),
    ))
}

/// `chr`: the character of a code point.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn chr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let code = int_arg(args, 0)?;
    if code < 0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "character number must be positive",
        ));
    }
    if code == 0 {
        return Err(Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "null character not permitted"));
    }
    let code = code as u32;
    if code > 0x10_FFFF {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {code}"),
        ));
    }
    char::from_u32(code).map(|c| Value::Text(c.to_string())).ok_or_else(|| {
        Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character not valid for encoding: {code}"),
        )
    })
}

/// The result of a match of `MatchText`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Like {
    True,
    False,
    /// No suffix of the text matches either.
    Abort,
}

fn escape_at_end() -> Error {
    Error::new(SqlState::INVALID_ESCAPE_SEQUENCE, "LIKE pattern must not end with escape character")
}

/// The length of the UTF8 character that starts with this byte.
fn char_len(first: u8) -> usize {
    match first {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

/// `MatchText` of `like_match.c` with `\` as the escape character, byte by byte as PostgreSQL does it for a deterministic collation.
fn match_text(mut t: &[u8], mut p: &[u8]) -> Result<Like> {
    if p == b"%" {
        return Ok(Like::True);
    }
    while !t.is_empty() && !p.is_empty() {
        match p[0] {
            b'%' => {
                p = &p[1..];
                while let Some(&c) = p.first() {
                    if c == b'%' {
                        p = &p[1..];
                    } else if c == b'_' {
                        if t.is_empty() {
                            return Ok(Like::Abort);
                        }
                        t = &t[char_len(t[0]).min(t.len())..];
                        p = &p[1..];
                    } else {
                        break;
                    }
                }
                if p.is_empty() {
                    return Ok(Like::True);
                }
                let first = if p[0] == b'\\' { *p.get(1).ok_or_else(escape_at_end)? } else { p[0] };
                while !t.is_empty() {
                    if t[0] == first {
                        let matched = match_text(t, p)?;
                        if matched != Like::False {
                            return Ok(matched);
                        }
                    }
                    t = &t[char_len(t[0]).min(t.len())..];
                }
                return Ok(Like::Abort);
            }
            b'_' => {
                t = &t[char_len(t[0]).min(t.len())..];
                p = &p[1..];
                continue;
            }
            b'\\' => {
                p = &p[1..];
                let Some(&c) = p.first() else { return Err(escape_at_end()) };
                if c != t[0] {
                    return Ok(Like::False);
                }
            }
            c if c != t[0] => return Ok(Like::False),
            _ => {}
        }
        t = &t[1..];
        p = &p[1..];
    }
    if !t.is_empty() {
        return Ok(Like::False);
    }
    while p.first() == Some(&b'%') {
        p = &p[1..];
    }
    Ok(if p.is_empty() { Like::True } else { Like::Abort })
}

/// True when the text matches the pattern of `LIKE`.
pub(crate) fn like(text: &str, pattern: &str) -> Result<bool> {
    Ok(match_text(text.as_bytes(), pattern.as_bytes())? == Like::True)
}

fn textlike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(like(arg(args, 0)?, arg(args, 1)?)?))
}

fn textnlike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(!like(arg(args, 0)?, arg(args, 1)?)?))
}

/// `ILIKE` in the C locale: both strings in ASCII lower case.
fn ilike(args: &[Value]) -> Result<bool> {
    like(&arg(args, 0)?.to_ascii_lowercase(), &arg(args, 1)?.to_ascii_lowercase())
}

fn texticlike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(ilike(args)?))
}

fn texticnlike(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(!ilike(args)?))
}

/// `like_escape`: the pattern with `\` as its escape character.
fn like_escape(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (pattern, escape) = (arg(args, 0)?, arg(args, 1)?);
    let mut chars = escape.chars();
    let Some(e) = chars.next() else { return Ok(Value::Text(pattern.replace('\\', "\\\\"))) };
    if chars.next().is_some() {
        return Err(Error::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid escape string")
            .with_hint("Escape string must be empty or one character."));
    }
    if e == '\\' {
        return Ok(Value::text(pattern));
    }
    let mut out = String::with_capacity(pattern.len());
    let mut after_escape = false;
    for c in pattern.chars() {
        if c == e && !after_escape {
            out.push('\\');
            after_escape = true;
        } else if c == '\\' {
            out.push('\\');
            if !after_escape {
                out.push('\\');
            }
            after_escape = false;
        } else {
            out.push(c);
            after_escape = false;
        }
    }
    Ok(Value::Text(out))
}

/// `text_starts_with`.
fn starts_with(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(arg(args, 0)?.starts_with(arg(args, 1)?)))
}

/// `quote_identifier` of `ruleutils.c`: the name in double quotes unless it is a simple name in lower case that is not a key word, or that is an unreserved key word.
pub(crate) fn quote_identifier(name: &str) -> String {
    let simple = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && rupg_sql::keyword(name).is_none_or(|k| k.category == Category::Unreserved);
    if simple { name.to_owned() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

fn quote_ident(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(quote_identifier(arg(args, 0)?)))
}

/// `quote_literal_internal`: the string in single quotes, with `E` before it when it has a backslash.
pub(crate) fn quote_literal_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn quote_literal(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(quote_literal_text(arg(args, 0)?)))
}

/// `quote_nullable`: `NULL` for a null value, else `quote_literal`. It is not strict.
fn quote_nullable(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    match args.first() {
        Some(Value::Null) => Ok(Value::text("NULL")),
        _ => Ok(Value::Text(quote_literal_text(arg(args, 0)?))),
    }
}

/// `quote_literal(anyelement)` and `quote_nullable(anyelement)`, which are functions in SQL that cast the value to `text`.
pub(crate) fn quote_any(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    match args.first() {
        Some(Value::Null) => Ok(Value::text("NULL")),
        Some(value) => {
            let ty = call.args.first().copied().ok_or_else(bad_value)?;
            Ok(Value::Text(quote_literal_text(&crate::to_text(ty, value, call.session)?)))
        }
        None => Err(bad_value()),
    }
}

/// `md5_text`: the MD5 hash of the bytes in lower case hexadecimal.
fn md5_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let digest = Md5::digest(arg(args, 0)?.as_bytes());
    Ok(Value::Text(digest.iter().map(|b| format!("{b:02x}")).collect()))
}

/// The arguments of `concat` and `concat_ws` with their types: the elements of the array of a `VARIADIC` call, or the arguments. `None` when the array is null.
fn variadic_values<'a>(
    call: &Call<'_>,
    args: &'a [Value],
    skip: usize,
) -> Result<Option<Vec<(u32, &'a Value)>>> {
    if !call.variadic {
        let types = call.args.iter().skip(skip).copied();
        return Ok(Some(types.zip(args.iter().skip(skip)).collect()));
    }
    match args.get(skip) {
        Some(Value::Null) => Ok(None),
        Some(Value::Array(array)) => {
            let ty = call.args.get(skip).copied().ok_or_else(bad_value)?;
            let element = rupg_pgcatalog::builtin::type_by_oid(base_type(ty))
                .map_or(oid::TEXT, |row| row.elem);
            Ok(Some(
                array
                    .values
                    .iter()
                    .map(|v| (element, v.as_ref().unwrap_or(&Value::Null)))
                    .collect(),
            ))
        }
        _ => Err(bad_value()),
    }
}

/// The text of the values that are not null, with a separator between them.
fn join(call: &Call<'_>, values: &[(u32, &Value)], separator: &str) -> Result<String> {
    let mut out = Vec::new();
    let mut first = true;
    for (ty, value) in values {
        if value.is_null() {
            continue;
        }
        if !first {
            out.extend_from_slice(separator.as_bytes());
        }
        first = false;
        output(*ty, value, call.session, &mut out)?;
    }
    String::from_utf8(out)
        .map_err(|_| Error::internal("an output function gave bytes that are not UTF-8"))
}

/// `text_concat`: the text of the arguments that are not null. It is not strict.
fn concat(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(values) = variadic_values(call, args, 0)? else { return Ok(Value::Null) };
    Ok(Value::Text(join(call, &values, "")?))
}

/// `text_concat_ws`: the text of the arguments that are not null with a separator, or null for a null separator. It is not strict.
fn concat_ws(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let separator = match args.first() {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(s)) => s.as_str(),
        _ => return Err(bad_value()),
    };
    let Some(values) = variadic_values(call, args, 1)? else { return Ok(Value::Null) };
    Ok(Value::Text(join(call, &values, separator)?))
}

/// `textanycat` and `anytextcat`: `||` with one argument of a type that is not a string, which is a function in SQL that casts that argument to `text`.
pub(crate) fn any_concat(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let mut out = String::new();
    for (ty, value) in call.args.iter().zip(args) {
        out.push_str(&crate::to_text(*ty, value, call.session)?);
    }
    Ok(Value::Text(out))
}

/// `bit_length(text)` and `bit_length(bytea)`, which are functions in SQL.
pub(crate) fn bit_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let bytes = match args.first() {
        Some(Value::Text(s)) => s.len(),
        Some(Value::Bytea(b)) => b.len(),
        _ => return Err(bad_value()),
    };
    Ok(count(bytes.saturating_mul(8)))
}

/// `nameconcatoid`: the name, then `_` and the OID. The views of `information_schema` use it for names that must be unique. When the result is longer than `NAMEDATALEN - 1` bytes, the name loses characters at its end, and the OID stays whole.
fn nameconcatoid(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Oid(oid)) = args.get(1) else {
        return Err(bad_value());
    };
    let suffix = format!("_{oid}");
    let name = clip(arg(args, 0)?, NAMEDATALEN - 1 - suffix.len());
    Ok(Value::Text(format!("{name}{suffix}")))
}

/// The kernel of a string function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "textcat" => textcat,
        "textlen" => textlen,
        "bpcharlen" => bpcharlen,
        "textoctetlen" | "bpcharoctetlen" => octetlen,
        "byteaoctetlen" => byteaoctetlen,
        "lower" => lower,
        "upper" => upper,
        "initcap" => initcap,
        "ltrim1" | "ltrim" => ltrim,
        "rtrim1" | "rtrim" => rtrim,
        "btrim1" | "btrim" => btrim,
        "text_substr" | "text_substr_no_len" => substr,
        "textpos" => textpos,
        "replace_text" => replace,
        "split_part" => split_part,
        "to_hex32" | "to_hex64" => to_hex,
        "repeat" => repeat,
        "text_reverse" => reverse,
        "text_left" => left,
        "text_right" => right,
        "lpad" => lpad,
        "rpad" => rpad,
        "ascii" => ascii,
        "chr" => chr,
        "textlike" | "namelike" => textlike,
        "textnlike" | "namenlike" => textnlike,
        "texticlike" | "nameiclike" => texticlike,
        "texticnlike" | "nameicnlike" => texticnlike,
        "like_escape" => like_escape,
        "text_starts_with" => starts_with,
        "quote_ident" => quote_ident,
        "quote_literal" => quote_literal,
        "quote_nullable" => quote_nullable,
        "md5_text" => md5_text,
        "text_concat" => concat,
        "text_concat_ws" => concat_ws,
        "nameconcatoid" => nameconcatoid,
        _ => return None,
    })
}

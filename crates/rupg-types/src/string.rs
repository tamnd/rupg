//! `varchar(n)` and `character(n)` in the text format, and the length casts of the two types.
//!
//! A port of `varchar_input`, `bpchar_input`, `varchar` and `bpchar` in `src/backend/utils/adt/varchar.c`. The typmod is the length in characters plus 4, and a typmod below 4 means no length. `text` and `varchar` with no length need no function: their text form is the string.
//!
//! Lifted from `crates/rudb-pgtypes/src/string.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::borrow::Cow;

use rupg_common::SqlState;

use crate::error::TypeError;

/// The header size of a varlena, which PostgreSQL adds to the length in the typmod.
const VARHDRSZ: i32 = 4;

/// The byte length of the first `chars` characters of `s`, or of all of `s` when it is shorter. This is `pg_mbcharcliplen`.
fn clip(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(at, _)| at)
}

fn too_long(type_name: &str, max: usize) -> TypeError {
    TypeError::new(
        SqlState::STRING_DATA_RIGHT_TRUNCATION,
        format!("value too long for type {type_name}({max})"),
    )
}

/// Cuts `s` to `max` characters. Without `explicit`, the characters after `max` must all be spaces, else the value is too long.
fn cut<'a>(s: &'a str, max: usize, explicit: bool, type_name: &str) -> Result<&'a str, TypeError> {
    let end = clip(s, max);
    if !explicit && s.as_bytes()[end..].iter().any(|&b| b != b' ') {
        return Err(too_long(type_name, max));
    }
    Ok(&s[..end])
}

/// The text input of `varchar(n)`, also the receive function after the encoding check. A value longer than the length is an error, unless the extra characters are spaces, which the input removes.
pub fn varchar_in(s: &str, typmod: i32) -> Result<&str, TypeError> {
    varchar_coerce(s, typmod, false)
}

/// The length cast of `varchar`, the function `varchar(varchar, int4, bool)`. An explicit cast cuts a long value with no error. An implicit or an assignment cast is the same as the input.
pub fn varchar_coerce(s: &str, typmod: i32, explicit: bool) -> Result<&str, TypeError> {
    if typmod < VARHDRSZ {
        return Ok(s);
    }
    let max = (typmod - VARHDRSZ) as usize;
    // A value with no more bytes than the length has no more characters.
    if s.len() <= max {
        return Ok(s);
    }
    cut(s, max, explicit, "character varying")
}

/// The text input of `character(n)`, also the receive function after the encoding check. A short value gets spaces at the end up to the length. A long value is an error, unless the extra characters are spaces, which the input removes. With no length the value is the string.
pub fn bpchar_in(s: &str, typmod: i32) -> Result<Cow<'_, str>, TypeError> {
    bpchar_coerce(s, typmod, false)
}

/// The length cast of `character`, the function `bpchar(bpchar, int4, bool)`. It pads as the input does, and an explicit cast cuts a long value with no error.
pub fn bpchar_coerce(s: &str, typmod: i32, explicit: bool) -> Result<Cow<'_, str>, TypeError> {
    if typmod < VARHDRSZ {
        return Ok(Cow::Borrowed(s));
    }
    let max = (typmod - VARHDRSZ) as usize;
    // Most values are ASCII, and then the byte length is the character count.
    let chars = if s.is_ascii() { s.len() } else { s.chars().count() };
    if chars >= max {
        return cut(s, max, explicit, "character").map(Cow::Borrowed);
    }
    let mut padded = String::with_capacity(s.len() + max - chars);
    padded.push_str(s);
    padded.extend(std::iter::repeat_n(' ', max - chars));
    Ok(Cow::Owned(padded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_length_counts_characters_and_not_bytes() {
        let typmod = 3 + VARHDRSZ;
        assert_eq!(varchar_in("ééé", typmod), Ok("ééé"));
        assert_eq!(varchar_in("ééé  ", typmod), Ok("ééé"));
        let error = varchar_in("éééé", typmod).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("22001", "value too long for type character varying(3)")
        );
        assert_eq!(varchar_coerce("éééé", typmod, true), Ok("ééé"));
        assert_eq!(varchar_in("abcdef", -1), Ok("abcdef"));

        assert_eq!(bpchar_in("é", typmod).as_deref(), Ok("é  "));
        assert_eq!(bpchar_in("abc   ", typmod).as_deref(), Ok("abc"));
        assert_eq!(
            bpchar_in("abcd", typmod).unwrap_err().message,
            "value too long for type character(3)"
        );
        assert_eq!(bpchar_coerce("abcd", typmod, true).as_deref(), Ok("abc"));
        assert_eq!(bpchar_in("ab ", -1).as_deref(), Ok("ab "));
        assert_eq!(bpchar_in("", VARHDRSZ).as_deref(), Ok(""));
    }
}

//! The text and binary forms of the multirange types, from `multirange_in`, `multirange_out`, `multirange_recv` and `multirange_send` of `multirangetypes.c`. A multirange keeps its ranges in order, and these functions give and take the text or the bytes of each range. The caller reads and writes the ranges with the functions of the range type.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;

/// The text of an empty range.
const EMPTY_LITERAL: &[u8] = b"empty";

/// `isspace` of C in the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// The error of a multirange literal that does not parse.
fn malformed(text: &str, detail: &str) -> TypeError {
    let mut error = TypeError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed multirange literal: \"{text}\""),
    );
    error.detail = Some(detail.to_string());
    error
}

/// The states of the parse of `multirange_in`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    BeforeRange,
    InRange,
    InRangeEscaped,
    InRangeQuoted,
    InRangeQuotedEscaped,
    AfterRange,
    Finished,
}

/// `multirange_in`: a left brace, zero or more ranges with commas between them, and a right brace. White space can be before and after each part. The parse gives the text of each range that is not the key word `empty` to `range` when it finds the end of that range, so the error of a range comes before the errors of the text after it.
///
/// # Errors
///
/// `22P02` "malformed multirange literal" with the detail of PostgreSQL, and the errors of `range`.
pub fn multirange_parse<E: From<TypeError>>(
    text: &str,
    mut range: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let s = text.as_bytes();
    let mut at = 0;
    while at < s.len() && is_space(s[at]) {
        at += 1;
    }
    if s.get(at) != Some(&b'{') {
        return Err(malformed(text, "Missing left brace.").into());
    }
    at += 1;
    let mut state = State::BeforeRange;
    let mut seen = 0;
    let mut start = 0;
    while state != State::Finished {
        let Some(&ch) = s.get(at) else {
            return Err(malformed(text, "Unexpected end of input.").into());
        };
        if is_space(ch) {
            at += 1;
            continue;
        }
        match state {
            State::BeforeRange => {
                if ch == b'[' || ch == b'(' {
                    start = at;
                    state = State::InRange;
                } else if ch == b'}' && seen == 0 {
                    state = State::Finished;
                } else if s.len() - at >= EMPTY_LITERAL.len()
                    && s[at..at + EMPTY_LITERAL.len()].eq_ignore_ascii_case(EMPTY_LITERAL)
                {
                    seen += 1;
                    at += EMPTY_LITERAL.len() - 1;
                    state = State::AfterRange;
                } else {
                    return Err(malformed(text, "Expected range start.").into());
                }
            }
            State::InRange => {
                if ch == b']' || ch == b')' {
                    seen += 1;
                    range(&text[start..=at])?;
                    state = State::AfterRange;
                } else if ch == b'"' {
                    state = State::InRangeQuoted;
                } else if ch == b'\\' {
                    state = State::InRangeEscaped;
                }
            }
            State::InRangeEscaped => state = State::InRange,
            State::InRangeQuoted => {
                if ch == b'"' {
                    if s.get(at + 1) == Some(&b'"') {
                        at += 1;
                    } else {
                        state = State::InRange;
                    }
                } else if ch == b'\\' {
                    state = State::InRangeQuotedEscaped;
                }
            }
            State::AfterRange => {
                if ch == b',' {
                    state = State::BeforeRange;
                } else if ch == b'}' {
                    state = State::Finished;
                } else {
                    return Err(malformed(text, "Expected comma or end of multirange.").into());
                }
            }
            State::InRangeQuotedEscaped => state = State::InRangeQuoted,
            State::Finished => {}
        }
        at += 1;
    }
    while at < s.len() && is_space(s[at]) {
        at += 1;
    }
    if at < s.len() {
        return Err(malformed(text, "Junk after closing right brace.").into());
    }
    Ok(())
}

/// `multirange_out`: the ranges in braces with a comma between them.
pub fn multirange_deparse<'a>(ranges: impl IntoIterator<Item = &'a [u8]>, out: &mut Vec<u8>) {
    out.push(b'{');
    for (i, range) in ranges.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(range);
    }
    out.push(b'}');
}

/// `multirange_recv`: the number of ranges, then the length and the bytes of each range. The parse gives the bytes of each range to `range` in order.
///
/// # Errors
///
/// `08P01` for a value that is too short or too long, `XX000` for a number of ranges that `palloc` refuses, and the errors of `range`.
pub fn multirange_recv<'a, E: From<TypeError>>(
    recv: &mut Recv<'a>,
    mut range: impl FnMut(&'a [u8]) -> Result<(), E>,
) -> Result<(), E> {
    let count = recv.i32()?;
    let size = i64::from(count).cast_unsigned().wrapping_mul(8);
    if size > 0x3fff_ffff {
        return Err(TypeError::new(
            SqlState::INTERNAL_ERROR,
            format!("invalid memory alloc request size {size}"),
        )
        .into());
    }
    for _ in 0..count {
        let len = recv.u32()?;
        range(recv.bytes(usize::try_from(len).unwrap_or(usize::MAX))?)?;
    }
    if recv.remaining() != 0 {
        return Err(TypeError::new(
            SqlState::PROTOCOL_VIOLATION,
            "invalid message format".to_string(),
        )
        .into());
    }
    Ok(())
}

/// `multirange_send`: the number of ranges, then the length and the bytes of each range.
pub fn multirange_send(ranges: &[&[u8]], out: &mut Vec<u8>) {
    let count = u32::try_from(ranges.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for range in ranges {
        let len = u32::try_from(range.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<String>, TypeError> {
        let mut out = Vec::new();
        multirange_parse(text, |r: &str| -> Result<(), TypeError> {
            out.push(r.to_string());
            Ok(())
        })?;
        Ok(out)
    }

    fn detail(text: &str) -> String {
        parse(text).unwrap_err().detail.unwrap()
    }

    #[test]
    fn text_forms() {
        assert_eq!(parse(" { } ").unwrap(), Vec::<String>::new());
        assert_eq!(parse("{ [1,5) , (8, 10] }").unwrap(), ["[1,5)", "(8, 10]"]);
        assert_eq!(parse("{EMPTY,[1,2)}").unwrap(), ["[1,2)"]);
        assert_eq!(parse(r#"{["a)",")\""]}"#).unwrap(), [r#"["a)",")\""]"#]);
        assert_eq!(parse(r"{[a\),b)}").unwrap(), [r"[a\),b)"]);
        assert_eq!(detail("[1,5)"), "Missing left brace.");
        assert_eq!(detail("{[1,5)"), "Unexpected end of input.");
        assert_eq!(detail("{,}"), "Expected range start.");
        assert_eq!(detail("{[1,5),}"), "Expected range start.");
        assert_eq!(detail("{[1,5) [6,7)}"), "Expected comma or end of multirange.");
        assert_eq!(detail("{[1,5)}x"), "Junk after closing right brace.");
        let mut out = Vec::new();
        multirange_deparse([&b"[1,5)"[..], b"[8,10)"], &mut out);
        assert_eq!(out, b"{[1,5),[8,10)}");
    }

    #[test]
    fn binary_forms() {
        let mut out = Vec::new();
        multirange_send(&[&[1], &[2, 3]], &mut out);
        assert_eq!(out, [0, 0, 0, 2, 0, 0, 0, 1, 1, 0, 0, 0, 2, 2, 3]);
        let mut ranges = Vec::new();
        multirange_recv(&mut Recv::new(&out), |r| -> Result<(), TypeError> {
            ranges.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(ranges, [&[1][..], &[2, 3][..]]);
        let error = multirange_recv(&mut Recv::new(&[0, 0, 0, 0, 9]), |_| Ok::<(), TypeError>(()));
        assert_eq!(error.unwrap_err().message, "invalid message format");
        let error =
            multirange_recv(&mut Recv::new(&[255, 255, 255, 255]), |_| Ok::<(), TypeError>(()));
        assert_eq!(
            error.unwrap_err().message,
            "invalid memory alloc request size 18446744073709551608"
        );
    }
}

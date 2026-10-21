//! The text and binary forms of the range types, from `range_parse`, `range_deparse`, `range_recv` and `range_send` of `rangetypes.c`. A range keeps its bounds as values of the subtype, so these functions give and take the text or the bytes of each bound. The caller reads and writes the bounds with the functions of the subtype.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::value::Value;

/// `RANGE_EMPTY`: the range has no points.
pub const RANGE_EMPTY: u8 = 0x01;
/// `RANGE_LB_INC`: the lower bound is part of the range.
pub const RANGE_LB_INC: u8 = 0x02;
/// `RANGE_UB_INC`: the upper bound is part of the range.
pub const RANGE_UB_INC: u8 = 0x04;
/// `RANGE_LB_INF`: the range has no lower bound.
pub const RANGE_LB_INF: u8 = 0x08;
/// `RANGE_UB_INF`: the range has no upper bound.
pub const RANGE_UB_INF: u8 = 0x10;

/// The text of an empty range.
const EMPTY_LITERAL: &str = "empty";

/// A value of a range type: the flags of `rangetypes.h` and the two bounds. A bound that is infinite is `Value::Null`, and so are both bounds of an empty range.
#[derive(Clone, Debug, PartialEq)]
pub struct Range {
    pub flags: u8,
    pub lower: Value,
    pub upper: Value,
}

impl Range {
    /// The empty range.
    pub fn empty() -> Range {
        Range { flags: RANGE_EMPTY, lower: Value::Null, upper: Value::Null }
    }

    /// True for an empty range.
    pub fn is_empty(&self) -> bool {
        self.flags & RANGE_EMPTY != 0
    }

    /// `RANGE_HAS_LBOUND`: the range has a lower bound value.
    pub fn has_lower(&self) -> bool {
        self.flags & (RANGE_EMPTY | RANGE_LB_INF) == 0
    }

    /// `RANGE_HAS_UBOUND`: the range has an upper bound value.
    pub fn has_upper(&self) -> bool {
        self.flags & (RANGE_EMPTY | RANGE_UB_INF) == 0
    }
}

/// A range as `range_parse` reads it: the flags, and the text of each bound that is not infinite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeText {
    pub flags: u8,
    pub lower: Option<String>,
    pub upper: Option<String>,
}

/// `isspace` of C in the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// The error of a range literal that does not parse.
fn malformed(text: &str, detail: &str) -> TypeError {
    let mut error = TypeError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed range literal: \"{text}\""),
    );
    error.detail = Some(detail.to_string());
    error
}

/// `range_parse`: the key word `empty`, or a bracket or a parenthesis, the lower bound, a comma, the upper bound and a bracket or a parenthesis. White space before and after the range is not part of it. A bound with no characters is infinite. A bound can have characters in double quotes, and a backslash takes the next character as it is.
///
/// # Errors
///
/// `22P02` "malformed range literal" with the detail of PostgreSQL.
pub fn range_parse(text: &str) -> Result<RangeText, TypeError> {
    let s = text.as_bytes();
    let mut at = 0;
    while at < s.len() && is_space(s[at]) {
        at += 1;
    }
    if s.len() - at >= EMPTY_LITERAL.len()
        && s[at..at + EMPTY_LITERAL.len()].eq_ignore_ascii_case(EMPTY_LITERAL.as_bytes())
    {
        at += EMPTY_LITERAL.len();
        while at < s.len() && is_space(s[at]) {
            at += 1;
        }
        if at < s.len() {
            return Err(malformed(text, "Junk after \"empty\" key word."));
        }
        return Ok(RangeText { flags: RANGE_EMPTY, lower: None, upper: None });
    }
    let mut flags = 0;
    match s.get(at) {
        Some(b'[') => flags |= RANGE_LB_INC,
        Some(b'(') => {}
        _ => return Err(malformed(text, "Missing left parenthesis or bracket.")),
    }
    at += 1;
    let lower = parse_bound(text, &mut at)?;
    if lower.is_none() {
        flags |= RANGE_LB_INF;
    }
    if s.get(at) != Some(&b',') {
        return Err(malformed(text, "Missing comma after lower bound."));
    }
    at += 1;
    let upper = parse_bound(text, &mut at)?;
    if upper.is_none() {
        flags |= RANGE_UB_INF;
    }
    match s.get(at) {
        Some(b']') => flags |= RANGE_UB_INC,
        Some(b')') => {}
        _ => return Err(malformed(text, "Too many commas.")),
    }
    at += 1;
    while at < s.len() && is_space(s[at]) {
        at += 1;
    }
    if at < s.len() {
        return Err(malformed(text, "Junk after right parenthesis or bracket."));
    }
    Ok(RangeText { flags, lower, upper })
}

/// `range_parse_bound`: the text of one bound up to a comma, a parenthesis or a bracket that is not in quotes, or `None` for a bound with no characters. In quotes, two double quotes are one double quote.
fn parse_bound(text: &str, at: &mut usize) -> Result<Option<String>, TypeError> {
    let s = text.as_bytes();
    let ends = |b: Option<&u8>| matches!(b, Some(b',' | b')' | b']'));
    if ends(s.get(*at)) {
        return Ok(None);
    }
    let mut out = Vec::new();
    let mut quoted = false;
    while quoted || !ends(s.get(*at)) {
        let Some(&c) = s.get(*at) else {
            return Err(malformed(text, "Unexpected end of input."));
        };
        *at += 1;
        match c {
            b'\\' => {
                let Some(&next) = s.get(*at) else {
                    return Err(malformed(text, "Unexpected end of input."));
                };
                out.push(next);
                *at += 1;
            }
            b'"' if !quoted => quoted = true,
            b'"' if s.get(*at) == Some(&b'"') => {
                out.push(b'"');
                *at += 1;
            }
            b'"' => quoted = false,
            _ => out.push(c),
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

/// `range_deparse`: the text of a range from the flags and the output of each bound that the range has.
pub fn range_deparse(flags: u8, lower: Option<&[u8]>, upper: Option<&[u8]>, out: &mut Vec<u8>) {
    if flags & RANGE_EMPTY != 0 {
        out.extend_from_slice(EMPTY_LITERAL.as_bytes());
        return;
    }
    out.push(if flags & RANGE_LB_INC != 0 { b'[' } else { b'(' });
    if let Some(lower) = lower {
        bound_escape(lower, out);
    }
    out.push(b',');
    if let Some(upper) = upper {
        bound_escape(upper, out);
    }
    out.push(if flags & RANGE_UB_INC != 0 { b']' } else { b')' });
}

/// `range_bound_escape`: a bound that is empty or has a quote, a backslash, a parenthesis, a bracket, a comma or white space is in double quotes, with each quote and each backslash doubled.
fn bound_escape(value: &[u8], out: &mut Vec<u8>) {
    let quote = value.is_empty()
        || value
            .iter()
            .any(|&b| matches!(b, b'"' | b'\\' | b'(' | b')' | b'[' | b']' | b',') || is_space(b));
    if !quote {
        out.extend_from_slice(value);
        return;
    }
    out.push(b'"');
    for &b in value {
        if matches!(b, b'"' | b'\\') {
            out.push(b);
        }
        out.push(b);
    }
    out.push(b'"');
}

/// The flags and the bytes of each bound of a range in the binary form.
pub type RangeBytes<'a> = (u8, Option<&'a [u8]>, Option<&'a [u8]>);

/// `range_recv`: the flags, then the length and the bytes of each bound that the flags give. The flags lose the bits that are not flags of a range.
///
/// # Errors
///
/// `08P01` for a value that is too short or too long.
pub fn range_recv<'a>(recv: &mut Recv<'a>) -> Result<RangeBytes<'a>, TypeError> {
    let flags =
        recv.byte()? & (RANGE_EMPTY | RANGE_LB_INC | RANGE_LB_INF | RANGE_UB_INC | RANGE_UB_INF);
    let mut bound = |inf: u8| -> Result<Option<&'a [u8]>, TypeError> {
        if flags & (RANGE_EMPTY | inf) != 0 {
            return Ok(None);
        }
        let len = recv.u32()?;
        recv.bytes(usize::try_from(len).unwrap_or(usize::MAX)).map(Some)
    };
    let lower = bound(RANGE_LB_INF)?;
    let upper = bound(RANGE_UB_INF)?;
    if recv.remaining() != 0 {
        return Err(TypeError::new(
            SqlState::PROTOCOL_VIOLATION,
            "invalid message format".to_string(),
        ));
    }
    Ok((flags, lower, upper))
}

/// `range_send`: the flags, then the length and the bytes of each bound that the range has.
pub fn range_send(flags: u8, lower: Option<&[u8]>, upper: Option<&[u8]>, out: &mut Vec<u8>) {
    out.push(flags);
    for bound in [lower, upper].into_iter().flatten() {
        let len = u32::try_from(bound.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(bound);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (u8, Option<String>, Option<String>) {
        let r = range_parse(text).unwrap();
        (r.flags, r.lower, r.upper)
    }

    fn detail(text: &str) -> String {
        range_parse(text).unwrap_err().detail.unwrap()
    }

    #[test]
    fn text_forms() {
        assert_eq!(parse("[1,10)"), (RANGE_LB_INC, Some("1".into()), Some("10".into())));
        assert_eq!(parse(" (,] "), (RANGE_LB_INF | RANGE_UB_INF | RANGE_UB_INC, None, None));
        assert_eq!(parse(" EMPTY "), (RANGE_EMPTY, None, None));
        assert_eq!(parse(r#"["a ""b"" \c",x\,y]"#).1.as_deref(), Some(r#"a "b" c"#));
        assert_eq!(parse(r#"["a ""b"" \c",x\,y]"#).2.as_deref(), Some("x,y"));
        assert_eq!(parse(r#"["",)"#).1.as_deref(), Some(""));
        assert_eq!(detail("emptyx"), "Junk after \"empty\" key word.");
        assert_eq!(detail(""), "Missing left parenthesis or bracket.");
        assert_eq!(detail("[1 10)"), "Missing comma after lower bound.");
        assert_eq!(detail("[1,10"), "Unexpected end of input.");
        assert_eq!(detail("[1,10,11)"), "Too many commas.");
        assert_eq!(detail("[1,10)x"), "Junk after right parenthesis or bracket.");
        assert_eq!(detail("[1)"), "Missing comma after lower bound.");
        assert_eq!(detail("[1,2\\"), "Unexpected end of input.");
        let mut out = Vec::new();
        range_deparse(RANGE_LB_INC, Some(b"a b"), Some(b"x\"y"), &mut out);
        assert_eq!(out, br#"["a b","x""y")"#);
        out.clear();
        range_deparse(RANGE_LB_INF | RANGE_UB_INC, None, Some(b""), &mut out);
        assert_eq!(out, br#"(,""]"#);
    }

    #[test]
    fn binary_forms() {
        let mut out = Vec::new();
        range_send(RANGE_LB_INC, Some(&[0, 0, 0, 1]), Some(&[0, 0, 0, 10]), &mut out);
        assert_eq!(out, [2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 10]);
        let mut recv = Recv::new(&out);
        let (flags, lower, upper) = range_recv(&mut recv).unwrap();
        assert_eq!(
            (flags, lower, upper),
            (RANGE_LB_INC, Some(&[0, 0, 0, 1][..]), Some(&[0, 0, 0, 10][..]))
        );
        let mut recv = Recv::new(&[0x81]);
        assert_eq!(range_recv(&mut recv).unwrap(), (RANGE_EMPTY, None, None));
        let mut recv = Recv::new(&[RANGE_EMPTY, 0]);
        assert_eq!(range_recv(&mut recv).unwrap_err().message, "invalid message format");
    }
}

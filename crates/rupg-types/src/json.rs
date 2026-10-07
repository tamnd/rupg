//! `json` in the text format.
//!
//! A `json` value is its text, so the input function only checks the syntax. This is a port of `json_in` in `src/backend/utils/adt/json.c` and of the lexer and the parser in `src/common/jsonapi.c`, with the same detail text for each error. The parser of PostgreSQL is recursive descent. This one keeps a stack of the open arrays and objects instead, so it takes the tokens in the same order and finds the same first error.
//!
//! `json_in` does not decode the escapes, so it takes `\u0000` and a lone surrogate such as `\ud800`. Only `jsonb` refuses them: its lexer decodes each string as it reads it, which is `need_escapes` in `jsonapi.c`, and gives the events of the parse to [`Sink`].
//!
//! Lifted from `crates/rudb-pgtypes/src/json.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use crate::error::TypeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    ObjectStart,
    ObjectEnd,
    ArrayStart,
    ArrayEnd,
    Comma,
    Colon,
    String,
    Number,
    True,
    False,
    Null,
    End,
}

/// A token and its bytes in the input. The bytes go in the detail of an error.
#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// The errors of `JsonParseErrorType` that `json_in` can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Error {
    InvalidToken,
    EscapingInvalid,
    EscapingRequired,
    UnicodeEscapeFormat,
    ExpectedEnd,
    ExpectedArrayNext,
    ExpectedColon,
    ExpectedJson,
    ExpectedMore,
    ExpectedObjectFirst,
    ExpectedObjectNext,
    ExpectedString,
    CodePointZero,
    HighSurrogate,
    LowSurrogate,
}

/// An error and the bytes of the token that it is about.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fail {
    error: Error,
    start: usize,
    end: usize,
}

/// `JSON_ALPHANUMERIC_CHAR`: a byte that the lexer keeps in a bad word, so that the error shows the whole word. Each byte of a character that is not ASCII is in it.
fn alphanumeric(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

struct Lexer<'a> {
    input: &'a [u8],
    at: usize,
    /// Whether the lexer decodes the strings into `text`.
    escapes: bool,
    /// The last string, with its escapes decoded, when `escapes` is set.
    text: String,
}

impl Lexer<'_> {
    /// `json_lex`.
    fn next(&mut self) -> Result<Token, Fail> {
        let b = self.input;
        while self.at < b.len() && matches!(b[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
        let start = self.at;
        let token = |kind, end| Token { kind, start, end };
        let Some(&first) = b.get(start) else {
            return Ok(token(Kind::End, start));
        };
        let kind = match first {
            b'{' => Kind::ObjectStart,
            b'}' => Kind::ObjectEnd,
            b'[' => Kind::ArrayStart,
            b']' => Kind::ArrayEnd,
            b',' => Kind::Comma,
            b':' => Kind::Colon,
            b'"' => {
                self.at = self.string(start)?;
                return Ok(token(Kind::String, self.at));
            }
            b'-' => {
                self.at = self.number(start, start + 1)?;
                return Ok(token(Kind::Number, self.at));
            }
            b'0'..=b'9' => {
                self.at = self.number(start, start)?;
                return Ok(token(Kind::Number, self.at));
            }
            _ => {
                let end = start + b[start..].iter().take_while(|&&c| alphanumeric(c)).count();
                if end == start {
                    return Err(Fail { error: Error::InvalidToken, start, end: start + 1 });
                }
                self.at = end;
                return match &b[start..end] {
                    b"true" => Ok(token(Kind::True, end)),
                    b"false" => Ok(token(Kind::False, end)),
                    b"null" => Ok(token(Kind::Null, end)),
                    _ => Err(Fail { error: Error::InvalidToken, start, end }),
                };
            }
        };
        self.at = start + 1;
        Ok(token(kind, self.at))
    }

    /// The end of the character that starts at `at`, for an error that shows that character.
    fn char_end(&self, at: usize) -> usize {
        let len = match self.input[at] {
            b if b < 0x80 => 1,
            b if b & 0xe0 == 0xc0 => 2,
            b if b & 0xf0 == 0xe0 => 3,
            _ => 4,
        };
        (at + len).min(self.input.len())
    }

    /// `json_lex_string`: the string that starts with the quote at `start`. Gives the end of the string after the closing quote. With `escapes` set, the string goes into `text` with its escapes decoded, and a surrogate pair is one character.
    fn string(&mut self, start: usize) -> Result<usize, Fail> {
        let b = self.input;
        let unterminated = Fail { error: Error::InvalidToken, start, end: b.len() };
        let escapes = self.escapes;
        self.text.clear();
        let mut high: Option<u32> = None;
        let mut s = start + 1;
        loop {
            // The bytes that need no work. A byte below 32 must be an escape.
            let run = b[s..].iter().position(|&c| c == b'"' || c == b'\\' || c < 32);
            let Some(run) = run else {
                return Err(unterminated);
            };
            if escapes && run > 0 {
                if high.is_some() {
                    let end = self.char_end(s);
                    return Err(Fail { error: Error::LowSurrogate, start, end });
                }
                self.text.push_str(std::str::from_utf8(&b[s..s + run]).unwrap_or_default());
            }
            s += run;
            match b[s] {
                b'"' => {
                    if high.is_some() {
                        return Err(Fail { error: Error::LowSurrogate, start, end: s + 1 });
                    }
                    return Ok(s + 1);
                }
                b'\\' => {
                    s += 1;
                    match b.get(s) {
                        None => return Err(unterminated),
                        Some(b'u') => {
                            let mut ch = 0u32;
                            for _ in 0..4 {
                                s += 1;
                                match b.get(s) {
                                    None => return Err(unterminated),
                                    Some(c) if c.is_ascii_hexdigit() => {
                                        ch = ch * 16 + char::from(*c).to_digit(16).unwrap_or(0);
                                    }
                                    Some(_) => {
                                        let end = self.char_end(s);
                                        return Err(Fail {
                                            error: Error::UnicodeEscapeFormat,
                                            start,
                                            end,
                                        });
                                    }
                                }
                            }
                            if escapes {
                                let fail = |error| Fail { error, start, end: s + 1 };
                                if (0xd800..=0xdbff).contains(&ch) {
                                    if high.is_some() {
                                        return Err(fail(Error::HighSurrogate));
                                    }
                                    high = Some(ch);
                                    s += 1;
                                    continue;
                                }
                                if (0xdc00..=0xdfff).contains(&ch) {
                                    let Some(first) = high.take() else {
                                        return Err(fail(Error::LowSurrogate));
                                    };
                                    ch = 0x10000 + ((first & 0x3ff) << 10) + (ch & 0x3ff);
                                }
                                if high.is_some() {
                                    return Err(fail(Error::LowSurrogate));
                                }
                                if ch == 0 {
                                    return Err(fail(Error::CodePointZero));
                                }
                                self.text.push(char::from_u32(ch).unwrap_or('\u{fffd}'));
                            }
                        }
                        Some(&c @ (b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't')) => {
                            if escapes {
                                if high.is_some() {
                                    return Err(Fail {
                                        error: Error::LowSurrogate,
                                        start,
                                        end: s + 1,
                                    });
                                }
                                self.text.push(match c {
                                    b'b' => '\u{8}',
                                    b'f' => '\u{c}',
                                    b'n' => '\n',
                                    b'r' => '\r',
                                    b't' => '\t',
                                    other => char::from(other),
                                });
                            }
                        }
                        Some(_) => {
                            if escapes && high.is_some() {
                                let end = self.char_end(s);
                                return Err(Fail { error: Error::LowSurrogate, start, end });
                            }
                            // The error shows only the escape and not the whole string.
                            let end = self.char_end(s);
                            return Err(Fail { error: Error::EscapingInvalid, start: s, end });
                        }
                    }
                    s += 1;
                }
                _ => return Err(Fail { error: Error::EscapingRequired, start, end: s }),
            }
        }
    }

    /// `json_lex_number`: an optional minus sign, then `0` or digits that do not start with `0`, then an optional fraction and an optional exponent. Letters and digits after the number are part of the bad token. `s` is the byte after the sign.
    fn number(&self, start: usize, mut s: usize) -> Result<usize, Fail> {
        let b = self.input;
        let digit = |s: usize| b.get(s).is_some_and(u8::is_ascii_digit);
        let digits = |mut s: usize| {
            while digit(s) {
                s += 1;
            }
            s
        };
        let mut error = false;
        match b.get(s) {
            Some(b'0') => s += 1,
            Some(b'1'..=b'9') => s = digits(s),
            _ => error = true,
        }
        if b.get(s) == Some(&b'.') {
            s += 1;
            if digit(s) { s = digits(s) } else { error = true }
        }
        if matches!(b.get(s), Some(b'e' | b'E')) {
            s += 1;
            if matches!(b.get(s), Some(b'+' | b'-')) {
                s += 1;
            }
            if digit(s) { s = digits(s) } else { error = true }
        }
        while b.get(s).is_some_and(|&c| alphanumeric(c)) {
            s += 1;
            error = true;
        }
        if error {
            return Err(Fail { error: Error::InvalidToken, start, end: s });
        }
        Ok(s)
    }
}

/// An array or an object that the parser is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Open {
    Array,
    Object,
}

/// What the parser reads next.
enum Step {
    /// A value: `parse_object`, `parse_array` or `parse_scalar`.
    Value,
    /// A field of an object: `parse_object_field`.
    Field,
    /// After a value: a comma, the end of the array or the object around it, or the end of the input.
    After,
}

/// `report_parse_error`: the error when the token is not one that the context can take.
fn unexpected(token: Token, error: Error) -> Fail {
    let error = if token.kind == Kind::End { Error::ExpectedMore } else { error };
    Fail { error, start: token.start, end: token.end }
}

/// The semantic actions of a parse. Each event comes after the lexer reads its token and before it reads the next one, as in `pg_parse_json`, so an error of an action comes before an error of a later token.
pub(crate) trait Sink {
    /// The start of an array or an object.
    fn open(&mut self, open: Open);
    /// The end of the array or the object that was opened last.
    fn close(&mut self);
    /// The key of the next field of an object, decoded.
    fn key(&mut self, key: &str);
    /// A scalar: its kind, its bytes in the input and, for a string, its decoded text.
    ///
    /// # Errors
    ///
    /// The error of the action, which stops the parse.
    fn scalar(&mut self, kind: Kind, token: &str, text: &str) -> Result<(), TypeError>;
}

/// Why a parse stopped.
pub(crate) enum Stop {
    Syntax(Fail),
    Action(TypeError),
}

impl From<Fail> for Stop {
    fn from(fail: Fail) -> Stop {
        Stop::Syntax(fail)
    }
}

/// A sink with no actions, for `json_in`.
struct Check;

impl Sink for Check {
    fn open(&mut self, _: Open) {}
    fn close(&mut self) {}
    fn key(&mut self, _: &str) {}
    fn scalar(&mut self, _: Kind, _: &str, _: &str) -> Result<(), TypeError> {
        Ok(())
    }
}

/// `pg_parse_json`, which gives each event to `sink`. With `escapes` set, the lexer decodes the strings and refuses the escapes that do not make text.
pub(crate) fn parse(input: &str, escapes: bool, sink: &mut impl Sink) -> Result<(), Stop> {
    let text = |token: Token| &input[token.start..token.end];
    let input = input.as_bytes();
    let mut lexer = Lexer { input, at: 0, escapes, text: String::new() };
    let mut stack = Vec::new();
    let mut token = lexer.next()?;
    let mut step = Step::Value;
    loop {
        step = match step {
            Step::Value => match token.kind {
                Kind::ObjectStart => {
                    stack.push(Open::Object);
                    sink.open(Open::Object);
                    token = lexer.next()?;
                    match token.kind {
                        Kind::String => Step::Field,
                        Kind::ObjectEnd => {
                            stack.pop();
                            sink.close();
                            token = lexer.next()?;
                            Step::After
                        }
                        _ => return Err(unexpected(token, Error::ExpectedObjectFirst).into()),
                    }
                }
                Kind::ArrayStart => {
                    sink.open(Open::Array);
                    token = lexer.next()?;
                    if token.kind == Kind::ArrayEnd {
                        sink.close();
                        token = lexer.next()?;
                        Step::After
                    } else {
                        stack.push(Open::Array);
                        Step::Value
                    }
                }
                Kind::String | Kind::Number | Kind::True | Kind::False | Kind::Null => {
                    sink.scalar(token.kind, text(token), &lexer.text).map_err(Stop::Action)?;
                    token = lexer.next()?;
                    Step::After
                }
                _ => return Err(unexpected(token, Error::ExpectedJson).into()),
            },
            Step::Field => {
                if token.kind != Kind::String {
                    return Err(unexpected(token, Error::ExpectedString).into());
                }
                sink.key(&lexer.text);
                token = lexer.next()?;
                if token.kind != Kind::Colon {
                    return Err(unexpected(token, Error::ExpectedColon).into());
                }
                token = lexer.next()?;
                Step::Value
            }
            Step::After => {
                let Some(&open) = stack.last() else {
                    if token.kind != Kind::End {
                        return Err(unexpected(token, Error::ExpectedEnd).into());
                    }
                    return Ok(());
                };
                let (close, error, next) = match open {
                    Open::Array => (Kind::ArrayEnd, Error::ExpectedArrayNext, Step::Value),
                    Open::Object => (Kind::ObjectEnd, Error::ExpectedObjectNext, Step::Field),
                };
                if token.kind == Kind::Comma {
                    token = lexer.next()?;
                    next
                } else if token.kind == close {
                    stack.pop();
                    sink.close();
                    token = lexer.next()?;
                    Step::After
                } else {
                    return Err(unexpected(token, error).into());
                }
            }
        };
    }
}

/// The text input of `json`: the string, after a check of the syntax.
pub fn json_in(s: &str) -> Result<&str, TypeError> {
    match parse(s, false, &mut Check) {
        Ok(()) => Ok(s),
        Err(Stop::Syntax(fail)) => Err(syntax(s, fail)),
        Err(Stop::Action(error)) => Err(error),
    }
}

/// The error of PostgreSQL for a parse that failed, which says `json` for `jsonb` too.
pub(crate) fn syntax(s: &str, fail: Fail) -> TypeError {
    let token = &s[fail.start..fail.end];
    let detail = match fail.error {
        Error::InvalidToken => format!("Token \"{token}\" is invalid."),
        Error::EscapingInvalid => format!("Escape sequence \"\\{token}\" is invalid."),
        Error::EscapingRequired => {
            format!("Character with value 0x{:02x} must be escaped.", s.as_bytes()[fail.end])
        }
        Error::UnicodeEscapeFormat => {
            "\"\\u\" must be followed by four hexadecimal digits.".to_string()
        }
        Error::ExpectedEnd => format!("Expected end of input, but found \"{token}\"."),
        Error::ExpectedArrayNext => format!("Expected \",\" or \"]\", but found \"{token}\"."),
        Error::ExpectedColon => format!("Expected \":\", but found \"{token}\"."),
        Error::ExpectedJson => format!("Expected JSON value, but found \"{token}\"."),
        Error::ExpectedMore => "The input string ended unexpectedly.".to_string(),
        Error::ExpectedObjectFirst => format!("Expected string or \"}}\", but found \"{token}\"."),
        Error::ExpectedObjectNext => format!("Expected \",\" or \"}}\", but found \"{token}\"."),
        Error::ExpectedString => format!("Expected string, but found \"{token}\"."),
        Error::CodePointZero => "\\u0000 cannot be converted to text.".to_string(),
        Error::HighSurrogate => {
            "Unicode high surrogate must not follow a high surrogate.".to_string()
        }
        Error::LowSurrogate => "Unicode low surrogate must follow a high surrogate.".to_string(),
    };
    let mut error = match fail.error {
        Error::CodePointZero => TypeError::new(
            SqlState::UNTRANSLATABLE_CHARACTER,
            "unsupported Unicode escape sequence".to_string(),
        ),
        _ => TypeError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type json".to_string(),
        ),
    };
    error.detail = Some(detail);
    error.context = Some(context(s, fail));
    error
}

/// `report_json_context`: the line of the input up to the end of the bad token, cut to about 50 bytes before that end. `...` marks the text that is not shown.
fn context(s: &str, fail: Fail) -> String {
    let b = s.as_bytes();
    let end = fail.end;
    let line_start =
        b[..fail.start.min(end)].iter().rposition(|&c| c == b'\n').map_or(0, |at| at + 1);
    let line = 1 + b[..line_start].iter().filter(|&&c| c == b'\n').count();
    let mut start = line_start;
    while end - start >= 50 {
        start += s[start..].chars().next().map_or(1, char::len_utf8);
    }
    if start - line_start <= 3 {
        start = line_start;
    }
    let prefix = if start > line_start { "..." } else { "" };
    let more = fail.error != Error::ExpectedMore && end < b.len();
    let suffix = if more && !matches!(b[end], b'\n' | b'\r') { "..." } else { "" };
    format!("JSON data, line {line}: {prefix}{}{suffix}", &s[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(s: &str) -> String {
        json_in(s).unwrap_err().detail.unwrap()
    }

    #[test]
    fn the_first_error_is_the_error_of_postgres() {
        assert!(json_in(r#" {"a": [1, -2.5e+3, true, null, "\u00e9\n", {}, []]} "#).is_ok());
        assert_eq!(detail("[1 2]"), r#"Expected "," or "]", but found "2"."#);
        assert_eq!(detail("[1, 2x]"), r#"Token "2x" is invalid."#);
        assert_eq!(detail(r#"{"a" 1}"#), r#"Expected ":", but found "1"."#);
        assert_eq!(detail(r#"{"a": 1,}"#), r#"Expected string, but found "}"."#);
        assert_eq!(detail("[1,"), "The input string ended unexpectedly.");
        assert_eq!(detail(r#""\é""#), r#"Escape sequence "\é" is invalid."#);
        assert_eq!(detail("\"a\tb\""), "Character with value 0x09 must be escaped.");
        assert_eq!(detail(r#""abc"#), r#"Token ""abc" is invalid."#);
        let context = |s: &str| json_in(s).unwrap_err().context.unwrap();
        assert_eq!(context("[1 2]"), "JSON data, line 1: [1 2...");
        assert_eq!(context("[1,"), "JSON data, line 1: [1,");
        assert_eq!(context("[1,\n 2,\n x]"), "JSON data, line 3:  x...");
        let long = format!("[{}x]", "1, ".repeat(30));
        assert_eq!(context(&long), format!("JSON data, line 1: ...{}...", &long[43..92]));
    }

    #[test]
    fn deep_nesting_needs_no_recursion() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(json_in(&deep).is_ok());
    }
}

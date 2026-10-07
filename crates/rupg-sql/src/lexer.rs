//! The lexer, a port of `scan.l`.
//!
//! The rules of `scan.l` are regular expressions, and flex takes the longest match, or the first rule when two matches have the same length. Here each kind of token is a function that starts at the first byte, and the comments say which rule or which tie it follows. The exclusive states of `scan.l` (`xb`, `xc`, `xd`, `xh`, `xq`, `xqs`, `xe`, `xdolq`, `xui`, `xus`, `xeu`) are loops in these functions.
//!
//! `standard_conforming_strings` is always on, as it is in every PostgreSQL release since 9.1, so a backslash in `'...'` is a plain character and only `E'...'` has escapes. The server encoding is always UTF-8.
//!
//! Lifted from `crates/rudb-pgparse/src/lexer.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::borrow::Cow;
use std::fmt;

use rupg_common::SqlState;

use crate::error::{Error, Notice, Severity};
use crate::generated::keywords::{self, Keyword};
use crate::generated::tables::token;
use crate::parser::character;

/// The longest name in bytes, `NAMEDATALEN - 1` in PostgreSQL.
pub(crate) const NAME_LENGTH: usize = 63;

/// The token that the grammar gives to a character that is not in it, `$undefined`.
const UNDEFINED: u16 = 2;

/// The value of a token.
#[derive(Clone, Debug)]
pub enum Value<'a> {
    /// No value, for a character token, an operator token such as `::`, or the end.
    None,
    /// The value of `ICONST` and `PARAM`.
    Integer(i32),
    /// The text of `IDENT`, `UIDENT`, `SCONST`, `USCONST`, `FCONST`, `BCONST`, `XCONST` and `Op`. An identifier is in lower case and not longer than 63 bytes. A bit string starts with `b` and a hexadecimal string starts with `x`, as in PostgreSQL.
    Text(Cow<'a, str>),
    /// The keyword of a keyword token.
    Keyword(&'static Keyword),
}

impl PartialEq for Value<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Keyword(a), Value::Keyword(b)) => std::ptr::eq(*a, *b),
            _ => false,
        }
    }
}

/// One token.
#[derive(Clone, Debug, PartialEq)]
pub struct Token<'a> {
    /// The token number of the grammar. See [`crate::token`] and [`crate::character`]. The end of the text is 0.
    pub kind: u16,
    /// The byte offset where the token starts.
    pub start: usize,
    /// The byte offset after the token.
    pub end: usize,
    /// The value.
    pub value: Value<'a>,
}

impl Token<'_> {
    /// The text of the value, or `None` when the value is not text.
    pub fn text(&self) -> Option<&str> {
        match &self.value {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// The kind of a quoted string.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    /// `B'...'`, the state `xb`.
    Bit,
    /// `X'...'`, the state `xh`.
    Hex,
    /// `'...'`, the state `xq`.
    Plain,
    /// `E'...'`, the state `xe`.
    Escape,
    /// `U&'...'`, the state `xus`.
    Unicode,
}

/// The lexer. It gives one token for each call of [`Lexer::next_token`].
pub struct Lexer<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
    notices: Vec<Notice>,
}

impl fmt::Debug for Lexer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lexer").field("at", &self.at).finish_non_exhaustive()
    }
}

/// The text of a literal. It points into the input until an escape, a doubled quote or a continuation makes it different from the input. Then it is a copy.
struct Literal {
    start: usize,
    end: usize,
    copy: Option<Vec<u8>>,
}

impl Literal {
    fn new() -> Literal {
        Literal { start: 0, end: 0, copy: None }
    }

    fn copy(&mut self, bytes: &[u8]) -> &mut Vec<u8> {
        self.copy.get_or_insert_with(|| bytes[self.start..self.end].to_vec())
    }

    /// Adds `bytes[from..to]`.
    fn push(&mut self, bytes: &[u8], from: usize, to: usize) {
        if self.copy.is_none() {
            if self.start == self.end {
                (self.start, self.end) = (from, to);
                return;
            }
            if self.end == from {
                self.end = to;
                return;
            }
        }
        self.copy(bytes).extend_from_slice(&bytes[from..to]);
    }

    fn push_byte(&mut self, bytes: &[u8], b: u8) {
        self.copy(bytes).push(b);
    }

    fn push_char(&mut self, bytes: &[u8], c: char) {
        let mut buffer = [0; 4];
        self.copy(bytes).extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
    }

    fn is_empty(&self) -> bool {
        match &self.copy {
            Some(copy) => copy.is_empty(),
            None => self.start == self.end,
        }
    }

    /// The text. `verify` is the `saw_non_ascii` of `scan.l`: an escape made a zero byte or a byte over 127, so the bytes must be checked as `pg_verifymbstr` does.
    fn finish<'a>(self, text: &'a str, verify: bool) -> Result<Cow<'a, str>, Error> {
        let Some(copy) = self.copy else {
            return Ok(Cow::Borrowed(&text[self.start..self.end]));
        };
        if verify && let Some(zero) = copy.iter().position(|&b| b == 0) {
            let valid = std::str::from_utf8(&copy).map_or_else(|e| e.valid_up_to(), str::len);
            return Err(Error::encoding(&copy[zero.min(valid)..]));
        }
        String::from_utf8(copy).map(Cow::Owned).map_err(|e| {
            let at = e.utf8_error().valid_up_to();
            Error::encoding(&e.as_bytes()[at..])
        })
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `ident_start`: a letter, `_` or any byte over 127. `dolq_start` is the same.
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// `dolq_cont`: `ident_start` or a digit.
fn is_dolq_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// `ident_cont`: `dolq_cont` or `$`.
fn is_ident_cont(b: u8) -> bool {
    is_dolq_cont(b) || b == b'$'
}

/// `op_chars`.
fn is_op_char(b: u8) -> bool {
    matches!(
        b,
        b'~' | b'!'
            | b'@'
            | b'#'
            | b'^'
            | b'&'
            | b'|'
            | b'`'
            | b'?'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'='
    )
}

/// `self`: the characters that are a token by themselves.
fn is_self(b: u8) -> bool {
    matches!(
        b,
        b',' | b'('
            | b')'
            | b'['
            | b']'
            | b'.'
            | b';'
            | b':'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'^'
            | b'<'
            | b'>'
            | b'='
    )
}

fn is_first_surrogate(c: u32) -> bool {
    (0xd800..=0xdbff).contains(&c)
}

fn is_second_surrogate(c: u32) -> bool {
    (0xdc00..=0xdfff).contains(&c)
}

fn surrogate_pair(first: u32, second: u32) -> u32 {
    ((first & 0x3ff) << 10) + (second & 0x3ff) + 0x10000
}

/// The value of hexadecimal digits that the caller has checked.
fn hex_value(digits: &[u8]) -> u32 {
    digits.iter().fold(0, |value, &d| (value << 4) | char::from(d).to_digit(16).unwrap_or(0))
}

/// The value of an integer literal as `pg_strtoint32_safe` reads it, with an optional `0x`, `0o` or `0b` prefix and `_` between digits, or `None` when it is too large for 32 bits.
fn integer(text: &[u8]) -> Option<i32> {
    let (radix, digits) = match text {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', b'o' | b'O', rest @ ..] => (8, rest),
        [b'0', b'b' | b'B', rest @ ..] => (2, rest),
        _ => (10, text),
    };
    let mut value: i32 = 0;
    for &d in digits {
        if d == b'_' {
            continue;
        }
        let digit = char::from(d).to_digit(radix)?;
        value = value.checked_mul(radix as i32)?.checked_add(digit as i32)?;
    }
    Some(value)
}

/// An identifier in lower case, cut to [`NAME_LENGTH`] bytes, as `downcase_truncate_identifier` makes it. Only ASCII letters change case, because the encoding has more than one byte for a character.
fn downcase(word: &str) -> Cow<'_, str> {
    if word.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(word.to_ascii_lowercase())
    } else {
        Cow::Borrowed(word)
    }
}

/// Cuts a name to [`NAME_LENGTH`] bytes at a character boundary, as `truncate_identifier` does, and gives the notice of PostgreSQL.
pub(crate) fn truncate<'a>(name: Cow<'a, str>, notices: &mut Vec<Notice>) -> Cow<'a, str> {
    if name.len() <= NAME_LENGTH {
        return name;
    }
    let mut length = NAME_LENGTH;
    while !name.is_char_boundary(length) {
        length -= 1;
    }
    notices.push(Notice {
        severity: Severity::Notice,
        code: SqlState::NAME_TOO_LONG,
        message: format!("identifier \"{name}\" will be truncated to \"{}\"", &name[..length]),
        location: None,
    });
    match name {
        Cow::Borrowed(name) => Cow::Borrowed(&name[..length]),
        Cow::Owned(mut name) => {
            name.truncate(length);
            Cow::Owned(name)
        }
    }
}

impl<'a> Lexer<'a> {
    /// A lexer for `text`. The text ends at the first zero byte, as the C string that PostgreSQL gets does.
    pub fn new(text: &'a str) -> Lexer<'a> {
        let text = text.find('\0').map_or(text, |zero| &text[..zero]);
        Lexer { text, bytes: text.as_bytes(), at: 0, notices: Vec::new() }
    }

    /// The text that the lexer reads.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Takes the notices that the tokens until now gave.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    pub(crate) fn notices(&mut self) -> &mut Vec<Notice> {
        &mut self.notices
    }

    /// The byte at `i`, or 0 after the end, as the buffer of flex has it.
    fn byte(&self, i: usize) -> u8 {
        self.bytes.get(i).copied().unwrap_or(0)
    }

    fn syntax(&self, message: &str, start: usize, end: usize) -> Error {
        Error::syntax(self.bytes, message, start, end)
    }

    fn emit(&mut self, kind: u16, start: usize, end: usize, value: Value<'a>) -> Token<'a> {
        self.at = end;
        Token { kind, start, end, value }
    }

    /// The next token. After the last token it gives the end, token 0, again and again.
    pub fn next_token(&mut self) -> Result<Token<'a>, Error> {
        self.skip()?;
        let start = self.at;
        let c = self.byte(start);
        let n = self.byte(start + 1);
        match c {
            _ if start >= self.bytes.len() => Ok(self.emit(0, start, start, Value::None)),
            b'\'' => self.quoted(Quote::Plain, start, start + 1),
            b'"' => self.delimited(token::IDENT, start, start + 1),
            b'$' => self.dollar(start),
            b'0'..=b'9' => self.number(start),
            b'.' if n.is_ascii_digit() => self.number(start),
            b'.' if n == b'.' => Ok(self.emit(token::DOT_DOT, start, start + 2, Value::None)),
            b':' if n == b':' => Ok(self.emit(token::TYPECAST, start, start + 2, Value::None)),
            b':' if n == b'=' => Ok(self.emit(token::COLON_EQUALS, start, start + 2, Value::None)),
            b'b' | b'B' if n == b'\'' => self.quoted(Quote::Bit, start, start + 2),
            b'x' | b'X' if n == b'\'' => self.quoted(Quote::Hex, start, start + 2),
            b'e' | b'E' if n == b'\'' => self.quoted(Quote::Escape, start, start + 2),
            b'n' | b'N' if n == b'\'' => {
                // `xnstart`: the lexer gives the keyword NCHAR for the `n`, and then the string.
                let nchar = keywords::lookup(b"nchar");
                let kind = nchar.map_or(token::NCHAR, |k| k.token);
                let value = nchar.map_or(Value::None, Value::Keyword);
                Ok(self.emit(kind, start, start + 1, value))
            }
            b'u' | b'U' if n == b'&' => match self.byte(start + 2) {
                b'\'' => self.quoted(Quote::Unicode, start, start + 3),
                b'"' => self.delimited(token::UIDENT, start, start + 3),
                // `xufailed`: the `u` is an identifier, and not a keyword.
                _ => {
                    let value = Value::Text(downcase(&self.text[start..start + 1]));
                    Ok(self.emit(token::IDENT, start, start + 1, value))
                }
            },
            _ if is_ident_start(c) => Ok(self.identifier(start)),
            _ if is_op_char(c) => self.operator(start),
            // `self` and `other`.
            _ => {
                let kind = character(c).unwrap_or(UNDEFINED);
                Ok(self.emit(kind, start, start + 1, Value::None))
            }
        }
    }

    /// Skips `whitespace`, which is spaces and `--` comments, and `/* */` comments, which can nest. `/*` always starts a comment, and `--` always starts a comment, because those rules come before `operator` and their match is never shorter.
    fn skip(&mut self) -> Result<(), Error> {
        loop {
            match self.byte(self.at) {
                b if is_space(b) && self.at < self.bytes.len() => self.at += 1,
                b'-' if self.byte(self.at + 1) == b'-' => self.at = self.line_end(self.at),
                b'/' if self.byte(self.at + 1) == b'*' => self.comment()?,
                _ => return Ok(()),
            }
        }
    }

    /// The offset of the newline that ends the line of `from`, or the end of the text.
    fn line_end(&self, from: usize) -> usize {
        self.bytes[from..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .map_or(self.bytes.len(), |at| from + at)
    }

    /// The state `xc`.
    fn comment(&mut self) -> Result<(), Error> {
        let start = self.at;
        let mut depth = 0usize;
        let mut i = start + 2;
        loop {
            if i >= self.bytes.len() {
                return Err(self.syntax("unterminated /* comment", start, i));
            }
            match (self.bytes[i], self.byte(i + 1)) {
                (b'/', b'*') => {
                    depth += 1;
                    i += 2;
                }
                (b'*', b'/') => {
                    if depth == 0 {
                        self.at = i + 2;
                        return Ok(());
                    }
                    depth -= 1;
                    i += 2;
                }
                _ => i += 1,
            }
        }
    }

    /// The state `xqs`: after the end quote of a string, the string goes on when spaces with a newline and another quote come next. Gives the offset after that quote.
    fn continuation(&self, mut i: usize) -> Option<usize> {
        let mut newline = false;
        loop {
            match self.byte(i) {
                b' ' | b'\t' | 0x0b | 0x0c => i += 1,
                b'\n' | b'\r' => {
                    newline = true;
                    i += 1;
                }
                b'-' if self.byte(i + 1) == b'-' => i = self.line_end(i),
                b'\'' if newline => return Some(i + 1),
                _ => return None,
            }
        }
    }

    /// The states `xb`, `xh`, `xq`, `xe` and `xus`. `open` is the offset after the first quote.
    fn quoted(&mut self, quote: Quote, start: usize, open: usize) -> Result<Token<'a>, Error> {
        let bytes = self.bytes;
        let mut literal = Literal::new();
        match quote {
            Quote::Bit => literal.push_byte(bytes, b'b'),
            Quote::Hex => literal.push_byte(bytes, b'x'),
            _ => {}
        }
        let mut verify = false;
        let mut i = open;
        let end = loop {
            if i >= bytes.len() {
                let message = match quote {
                    Quote::Bit => "unterminated bit string literal",
                    Quote::Hex => "unterminated hexadecimal string literal",
                    _ => "unterminated quoted string",
                };
                return Err(self.syntax(message, start, i));
            }
            match bytes[i] {
                // `xqdouble`. It is not in `xb` and `xh`.
                b'\'' if self.byte(i + 1) == b'\'' && !matches!(quote, Quote::Bit | Quote::Hex) => {
                    literal.push(bytes, i, i + 1);
                    i += 2;
                }
                b'\'' => match self.continuation(i + 1) {
                    Some(next) => i = next,
                    None => break i + 1,
                },
                b'\\' if quote == Quote::Escape => {
                    i = self.escape(i, &mut literal, &mut verify)?;
                }
                _ => {
                    let stop = |&b: &u8| b == b'\'' || (b == b'\\' && quote == Quote::Escape);
                    let run = bytes[i..].iter().position(stop).map_or(bytes.len(), |n| i + n);
                    literal.push(bytes, i, run);
                    i = run;
                }
            }
        };
        let kind = match quote {
            Quote::Bit => token::BCONST,
            Quote::Hex => token::XCONST,
            Quote::Plain | Quote::Escape => token::SCONST,
            Quote::Unicode => token::USCONST,
        };
        let text = literal.finish(self.text, verify)?;
        Ok(self.emit(kind, start, end, Value::Text(text)))
    }

    /// One escape of `E'...'` at the backslash at `i`: `xeescape`, `xeoctesc`, `xehexesc`, `xeunicode` and the state `xeu`. Gives the offset after it.
    fn escape(&self, i: usize, literal: &mut Literal, verify: &mut bool) -> Result<usize, Error> {
        let bytes = self.bytes;
        let n = self.byte(i + 1);
        if i + 1 >= bytes.len() {
            // `<xe>.`, for a backslash just before the end.
            literal.push_byte(bytes, b'\\');
            return Ok(i + 1);
        }
        let mut byte = |b: u8, literal: &mut Literal| {
            literal.push_byte(bytes, b);
            if b == 0 || b >= 0x80 {
                *verify = true;
            }
        };
        match n {
            b'0'..=b'7' => {
                let digits =
                    bytes[i + 1..].iter().take(3).take_while(|b| (b'0'..=b'7').contains(b));
                let length = digits.clone().count();
                // The value is an unsigned char in C, so `\777` is 0xff.
                let value = digits.fold(0u32, |v, &d| (v << 3) | u32::from(d - b'0'));
                byte((value & 0xff) as u8, literal);
                Ok(i + 1 + length)
            }
            b'x' if self.byte(i + 2).is_ascii_hexdigit() => {
                let length = if self.byte(i + 3).is_ascii_hexdigit() { 2 } else { 1 };
                byte(hex_value(&bytes[i + 2..i + 2 + length]) as u8, literal);
                Ok(i + 2 + length)
            }
            b'u' | b'U' => {
                let (c, after) = self.unicode(i)?;
                let c = if is_first_surrogate(c) {
                    // The state `xeu`: the second half of the pair must come next.
                    if self.byte(after) != b'\\' || !matches!(self.byte(after + 1), b'u' | b'U') {
                        let end = (after + 1).min(bytes.len());
                        return Err(self.syntax("invalid Unicode surrogate pair", after, end));
                    }
                    let (second, end) = self.unicode(after)?;
                    if !is_second_surrogate(second) {
                        return Err(self.syntax("invalid Unicode surrogate pair", after, end));
                    }
                    return self.push_unicode(surrogate_pair(c, second), after, end, literal);
                } else if is_second_surrogate(c) {
                    return Err(self.syntax("invalid Unicode surrogate pair", i, after));
                } else {
                    c
                };
                self.push_unicode(c, i, after, literal)
            }
            _ => {
                // `xeescape`, one byte after the backslash, as `unescape_single_char` reads it.
                let value = match n {
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    _ => n,
                };
                byte(value, literal);
                Ok(i + 2)
            }
        }
    }

    /// `xeunicode` at the backslash at `i`: the code point and the offset after it. A `\u` with less than four digits, or a `\U` with less than eight, is `xeunicodefail`.
    fn unicode(&self, i: usize) -> Result<(u32, usize), Error> {
        let length = if self.byte(i + 1) == b'u' { 4 } else { 8 };
        let digits = &self.bytes[i + 2..];
        if digits.len() < length || !digits[..length].iter().all(u8::is_ascii_hexdigit) {
            return Err(Error {
                code: SqlState::INVALID_ESCAPE_SEQUENCE,
                message: "invalid Unicode escape".to_owned(),
                detail: None,
                hint: Some("Unicode escapes must be \\uXXXX or \\UXXXXXXXX."),
                location: Some(i),
            });
        }
        Ok((hex_value(&digits[..length]), i + 2 + length))
    }

    /// `addunicode`.
    fn push_unicode(
        &self,
        c: u32,
        start: usize,
        end: usize,
        literal: &mut Literal,
    ) -> Result<usize, Error> {
        match char::from_u32(c).filter(|&c| c != '\0') {
            Some(c) => {
                literal.push_char(self.bytes, c);
                Ok(end)
            }
            None => Err(self.syntax("invalid Unicode escape value", start, end)),
        }
    }

    /// The states `xd` and `xui`. `open` is the offset after the first double quote.
    fn delimited(&mut self, kind: u16, start: usize, open: usize) -> Result<Token<'a>, Error> {
        let bytes = self.bytes;
        let mut literal = Literal::new();
        let mut i = open;
        let end = loop {
            if i >= bytes.len() {
                return Err(self.syntax("unterminated quoted identifier", start, i));
            }
            if bytes[i] == b'"' {
                if self.byte(i + 1) != b'"' {
                    break i + 1;
                }
                literal.push(bytes, i, i + 1);
                i += 2;
            } else {
                let run = bytes[i..].iter().position(|&b| b == b'"').map_or(bytes.len(), |n| i + n);
                literal.push(bytes, i, run);
                i = run;
            }
        };
        if literal.is_empty() {
            return Err(self.syntax("zero-length delimited identifier", start, end));
        }
        let mut name = literal.finish(self.text, false)?;
        // A `U&"..."` name is cut after the escapes are made, in the filter.
        if kind == token::IDENT {
            name = truncate(name, &mut self.notices);
        }
        Ok(self.emit(kind, start, end, Value::Text(name)))
    }

    /// A `$`: `dolqdelim`, `dolqfailed`, `param`, `param_junk` or `other`.
    fn dollar(&mut self, start: usize) -> Result<Token<'a>, Error> {
        let n = self.byte(start + 1);
        if n.is_ascii_digit() {
            let digits = self.bytes[start + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
            let end = start + 1 + digits;
            if is_ident_start(self.byte(end)) {
                let end = self.identifier_end(end);
                return Err(self.syntax("trailing junk after parameter", start, end));
            }
            return match integer(&self.bytes[start + 1..end]) {
                Some(n) => Ok(self.emit(token::PARAM, start, end, Value::Integer(n))),
                None => Err(self.syntax("parameter number too large", start, end)),
            };
        }
        match self.delimiter(start) {
            Some(open) => self.dollar_quoted(start, open),
            // `dolqfailed` gives back all but the `$`, and the `$` is `other`.
            None => {
                Ok(self.emit(character(b'$').unwrap_or(UNDEFINED), start, start + 1, Value::None))
            }
        }
    }

    /// The offset after the `dolqdelim` at `i`, or `None` when there is none.
    fn delimiter(&self, i: usize) -> Option<usize> {
        let mut j = i + 1;
        if is_ident_start(self.byte(j)) {
            j += self.bytes[j..].iter().take_while(|&&b| is_dolq_cont(b)).count();
        }
        (self.byte(j) == b'$').then_some(j + 1)
    }

    /// The state `xdolq`. The text between the delimiters is the value, with no escapes.
    fn dollar_quoted(&mut self, start: usize, open: usize) -> Result<Token<'a>, Error> {
        let bytes = self.bytes;
        let tag = &bytes[start..open];
        let mut i = open;
        loop {
            let Some(at) = bytes[i..].iter().position(|&b| b == b'$') else {
                return Err(self.syntax("unterminated dollar-quoted string", start, bytes.len()));
            };
            let at = i + at;
            match self.delimiter(at) {
                Some(end) if &bytes[at..end] == tag => {
                    let value = Value::Text(Cow::Borrowed(&self.text[open..at]));
                    return Ok(self.emit(token::SCONST, start, end, value));
                }
                // Another delimiter: its last `$` can start the delimiter that ends the string.
                Some(end) => i = end - 1,
                None => i = at + 1,
            }
        }
    }

    /// The offset after the `identifier` that starts at `i`.
    fn identifier_end(&self, i: usize) -> usize {
        i + self.bytes[i..].iter().take_while(|&&b| is_ident_cont(b)).count()
    }

    /// `identifier`: a keyword, or a name in lower case.
    fn identifier(&mut self, start: usize) -> Token<'a> {
        let end = self.identifier_end(start + 1);
        let word = &self.text[start..end];
        if let Some(keyword) = keywords::lookup(word.as_bytes()) {
            return self.emit(keyword.token, start, end, Value::Keyword(keyword));
        }
        let name = truncate(downcase(word), &mut self.notices);
        self.emit(token::IDENT, start, end, Value::Text(name))
    }

    /// The length of `decinteger` at `i`, or 0.
    fn decimal(&self, i: usize) -> usize {
        if !self.byte(i).is_ascii_digit() {
            return 0;
        }
        let mut j = i + 1;
        loop {
            if self.byte(j).is_ascii_digit() {
                j += 1;
            } else if self.byte(j) == b'_' && self.byte(j + 1).is_ascii_digit() {
                j += 2;
            } else {
                return j - i;
            }
        }
    }

    /// The length of `hexinteger`, `octinteger` or `bininteger` at `i`, with digits in `radix`.
    fn prefixed(&self, i: usize, radix: u32) -> usize {
        let digit = |b: u8| char::from(b).is_digit(radix);
        let mut j = i + 2;
        loop {
            if digit(self.byte(j)) {
                j += 1;
            } else if self.byte(j) == b'_' && digit(self.byte(j + 1)) {
                j += 2;
            } else {
                return if j == i + 2 { 0 } else { j - i };
            }
        }
    }

    /// The number rules, from `decinteger` to `real_junk`. Each rule gives the length of its match, and the longest match wins. When two have the same length, the first rule wins, as in flex.
    fn number(&mut self, start: usize) -> Result<Token<'a>, Error> {
        #[derive(Clone, Copy)]
        enum Rule {
            Integer,
            Prefixed,
            HexFail,
            OctFail,
            BinFail,
            Numeric,
            NumericFail,
            Real,
            RealFail,
            Junk,
        }
        let mut best = (0, Rule::Integer);
        let mut offer = |length: usize, rule: Rule| {
            if length > best.0 {
                best = (length, rule);
            }
        };
        let whole = self.decimal(start);
        offer(whole, Rule::Integer);
        if self.byte(start) == b'0' {
            let (radix, fail) = match self.byte(start + 1) {
                b'x' | b'X' => (16, Rule::HexFail),
                b'o' | b'O' => (8, Rule::OctFail),
                b'b' | b'B' => (2, Rule::BinFail),
                _ => (0, Rule::Integer),
            };
            if radix != 0 {
                offer(self.prefixed(start, radix), Rule::Prefixed);
                offer(if self.byte(start + 2) == b'_' { 3 } else { 2 }, fail);
            }
        }
        // `numeric` is `1.`, `1.5` or `.5`. `numericfail` is `1..`, which gives back the `..`.
        let numeric = if whole > 0 && self.byte(start + whole) == b'.' {
            if self.byte(start + whole + 1) == b'.' {
                offer(whole + 2, Rule::NumericFail);
            }
            whole + 1 + self.decimal(start + whole + 1)
        } else if whole == 0 {
            1 + self.decimal(start + 1)
        } else {
            0
        };
        offer(numeric, Rule::Numeric);
        let mut real = 0;
        for base in [whole, numeric] {
            if base == 0 || !matches!(self.byte(start + base), b'e' | b'E') {
                continue;
            }
            let mut j = start + base + 1;
            let sign = matches!(self.byte(j), b'+' | b'-');
            if sign {
                j += 1;
            }
            let digits = self.decimal(j);
            if digits > 0 {
                real = real.max(j + digits - start);
                offer(j + digits - start, Rule::Real);
            } else if sign {
                offer(j - start, Rule::RealFail);
            }
        }
        // `integer_junk`, `numeric_junk` and `real_junk`: a number with an identifier after it. The identifier can also start at a `_` in the digits, because a `decinteger` can stop before any `_` and an identifier can start with `_`. So `1_000$1` is `1` and `_000$1`, and `1_0.5_0$` is `1_0.5` and `_0$`.
        for base in [whole, numeric, real] {
            if base > 0 && is_ident_start(self.byte(start + base)) {
                offer(self.identifier_end(start + base) - start, Rule::Junk);
            }
        }
        let digits = start..start + whole.max(numeric).max(real);
        for underscore in digits.filter(|&i| self.bytes[i] == b'_') {
            offer(self.identifier_end(underscore) - start, Rule::Junk);
        }
        let (length, rule) = best;
        let end = start + length;
        let text = &self.text[start..end];
        let message = match rule {
            Rule::Integer | Rule::Prefixed | Rule::NumericFail => {
                let end = if matches!(rule, Rule::NumericFail) { end - 2 } else { end };
                let text = &self.text[start..end];
                // `process_integer_literal`: a value too large for 32 bits is FCONST.
                return Ok(match integer(text.as_bytes()) {
                    Some(n) => self.emit(token::ICONST, start, end, Value::Integer(n)),
                    None => self.emit(token::FCONST, start, end, Value::Text(Cow::Borrowed(text))),
                });
            }
            Rule::Numeric | Rule::Real => {
                return Ok(self.emit(token::FCONST, start, end, Value::Text(Cow::Borrowed(text))));
            }
            Rule::HexFail => "invalid hexadecimal integer",
            Rule::OctFail => "invalid octal integer",
            Rule::BinFail => "invalid binary integer",
            Rule::RealFail | Rule::Junk => "trailing junk after numeric literal",
        };
        Err(self.syntax(message, start, end))
    }

    /// `operator`, with the special operators and the characters of `self`.
    fn operator(&mut self, start: usize) -> Result<Token<'a>, Error> {
        let run = &self.bytes
            [start..start + self.bytes[start..].iter().take_while(|&&b| is_op_char(b)).count()];
        // A comment start in the operator ends it. It is never at the first character, because then the comment rule matched.
        let mut length = run.windows(2).position(|w| w == b"/*" || w == b"--").unwrap_or(run.len());
        // A multi-character operator cannot end in `+` or `-`, unless it has a character that is not in the operators of the SQL standard. So `=-` is two operators and `?-` is one.
        if length > 1 && matches!(run[length - 1], b'+' | b'-') {
            let qualifying = |b: &u8| {
                matches!(b, b'~' | b'!' | b'@' | b'#' | b'^' | b'&' | b'|' | b'`' | b'?' | b'%')
            };
            if !run[..length - 1].iter().any(qualifying) {
                while length > 1 && matches!(run[length - 1], b'+' | b'-') {
                    length -= 1;
                }
            }
        }
        let end = start + length;
        let kind = match &run[..length] {
            [c] if is_self(*c) => character(*c),
            b"=>" => Some(token::EQUALS_GREATER),
            b"<=" => Some(token::LESS_EQUALS),
            b">=" => Some(token::GREATER_EQUALS),
            b"<>" | b"!=" => Some(token::NOT_EQUALS),
            _ => None,
        };
        if let Some(kind) = kind {
            return Ok(self.emit(kind, start, end, Value::None));
        }
        if length > NAME_LENGTH {
            return Err(self.syntax("operator too long", start, end));
        }
        let value = Value::Text(Cow::Borrowed(&self.text[start..end]));
        Ok(self.emit(token::OP, start, end, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::*;

    /// The kinds and the values of the tokens of `text`, without the end.
    fn lex(text: &str) -> Vec<(u16, Value<'_>)> {
        let mut lexer = Lexer::new(text);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token().unwrap();
            if token.kind == 0 {
                return tokens;
            }
            tokens.push((token.kind, token.value));
        }
    }

    fn text(s: &str) -> Value<'_> {
        Value::Text(Cow::Borrowed(s))
    }

    fn c(ch: u8) -> u16 {
        character(ch).unwrap()
    }

    #[test]
    fn names_and_keywords() {
        let select = keywords::lookup(b"select").unwrap();
        assert_eq!(
            lex("SeLeCt Foo \"Foo\" é_$1"),
            [
                (SELECT, Value::Keyword(select)),
                (IDENT, text("foo")),
                (IDENT, text("Foo")),
                (IDENT, text("é_$1")),
            ]
        );
        // `xufailed`: `u&` with no quote after it is the name `u` and the operator `&`.
        assert_eq!(lex("U&x"), [(IDENT, text("u")), (OP, text("&")), (IDENT, text("x"))]);
        assert_eq!(
            lex("n'a'"),
            [(NCHAR, Value::Keyword(keywords::lookup(b"nchar").unwrap())), (SCONST, text("a"))]
        );
    }

    #[test]
    fn long_names() {
        let long = "a".repeat(70);
        let mut lexer = Lexer::new(&long);
        assert_eq!(lexer.next_token().unwrap().value, text(&long[..63]));
        let notices = lexer.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].code.as_str(), "42622");
        // A cut does not split a character: 31 characters of two bytes are 62 bytes.
        let long = "é".repeat(40);
        assert_eq!(lex(&long), [(IDENT, text(&long[..62]))]);
    }

    #[test]
    fn strings() {
        assert_eq!(lex("'it''s'"), [(SCONST, text("it's"))]);
        assert_eq!(lex("'a\\b'"), [(SCONST, text("a\\b"))]);
        assert_eq!(lex("'a'\n'b' 'c'"), [(SCONST, text("ab")), (SCONST, text("c"))]);
        assert_eq!(lex("'a' -- x\n 'b'"), [(SCONST, text("ab"))]);
        assert_eq!(lex("E'\\n\\t\\101\\x41\\'\\q'"), [(SCONST, text("\n\tAA'q"))]);
        assert_eq!(lex("E'\\u00e9\\U0001F600\\ud83d\\ude00'"), [(SCONST, text("é😀😀"))]);
        assert_eq!(lex("E'\\303\\251'"), [(SCONST, text("é"))]);
        assert_eq!(lex("b'01' x'0F'"), [(BCONST, text("b01")), (XCONST, text("x0F"))]);
        assert_eq!(lex("U&'\\0041'"), [(USCONST, text("\\0041"))]);
        assert_eq!(lex("U&\"a\""), [(UIDENT, text("a"))]);
        assert_eq!(lex("$$a'b$$ $x$a$y$b$x$"), [(SCONST, text("a'b")), (SCONST, text("a$y$b"))]);
        // `dolqfailed`: `$a` with no `$` after it is the character `$`.
        assert_eq!(lex("$a")[0].0, UNDEFINED);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            lex("1 1_000 0x1F 0o17 0b101"),
            [
                (ICONST, Value::Integer(1)),
                (ICONST, Value::Integer(1000)),
                (ICONST, Value::Integer(31)),
                (ICONST, Value::Integer(15)),
                (ICONST, Value::Integer(5)),
            ]
        );
        assert_eq!(
            lex("2147483648 1.5 .5 1e3 1.5E-3"),
            [
                (FCONST, text("2147483648")),
                (FCONST, text("1.5")),
                (FCONST, text(".5")),
                (FCONST, text("1e3")),
                (FCONST, text("1.5E-3")),
            ]
        );
        // `numericfail`: `1..2` is `1`, `..` and `2`.
        assert_eq!(
            lex("1..2"),
            [(ICONST, Value::Integer(1)), (DOT_DOT, Value::None), (ICONST, Value::Integer(2))]
        );
        assert_eq!(lex("$12"), [(PARAM, Value::Integer(12))]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            lex(":: := => <= >= <> != . ; :"),
            [
                (TYPECAST, Value::None),
                (COLON_EQUALS, Value::None),
                (EQUALS_GREATER, Value::None),
                (LESS_EQUALS, Value::None),
                (GREATER_EQUALS, Value::None),
                (NOT_EQUALS, Value::None),
                (NOT_EQUALS, Value::None),
                (c(b'.'), Value::None),
                (c(b';'), Value::None),
                (c(b':'), Value::None),
            ]
        );
        // A multi-character operator ends in `+` or `-` only with a character such as `@`.
        assert_eq!(lex("=-"), [(c(b'='), Value::None), (c(b'-'), Value::None)]);
        assert_eq!(lex("@-"), [(OP, text("@-"))]);
        assert_eq!(lex("<=-"), [(LESS_EQUALS, Value::None), (c(b'-'), Value::None)]);
        assert_eq!(lex("+/*x*/-"), [(c(b'+'), Value::None), (c(b'-'), Value::None)]);
        assert_eq!(lex("~--x"), [(OP, text("~"))]);
        assert_eq!(lex("/* a /* b */ c */ 1")[0].0, ICONST);
    }

    #[test]
    fn locations() {
        let mut lexer = Lexer::new("  é 'x'\n");
        let token = lexer.next_token().unwrap();
        assert_eq!((token.start, token.end), (2, 4));
        let token = lexer.next_token().unwrap();
        assert_eq!((token.start, token.end), (5, 8));
        let token = lexer.next_token().unwrap();
        assert_eq!((token.kind, token.start, token.end), (0, 9, 9));
        // The text ends at a zero byte, as the C string of PostgreSQL does.
        assert_eq!(lex("1\u{0}2"), [(ICONST, Value::Integer(1))]);
    }
}

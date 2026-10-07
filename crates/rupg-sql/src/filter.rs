//! The filter between the lexer and the parser, a port of `base_yylex` in `parser.c`.
//!
//! The grammar is LALR(1), so it sees one token ahead. Some constructs need two. For them the filter looks at the next token and gives a different token for the current one:
//!
//! - `NOT` before `BETWEEN`, `IN`, `LIKE`, `ILIKE` or `SIMILAR` is `NOT_LA`.
//! - `NULLS` before `FIRST` or `LAST` is `NULLS_LA`.
//! - `WITH` before `TIME` or `ORDINALITY` is `WITH_LA`.
//! - `WITHOUT` before `TIME` is `WITHOUT_LA`.
//! - `FORMAT` before `JSON` is `FORMAT_LA`.
//!
//! The filter also makes `U&"..."` and `U&'...'`, with an optional `UESCAPE '...'` after them, into `IDENT` and `SCONST`.
//!
//! Lifted from `crates/rudb-pgparse/src/filter.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::borrow::Cow;

use rupg_common::SqlState;

use crate::error::{Error, Notice};
use crate::generated::tables::token;
use crate::lexer::{Lexer, Token, Value, truncate};

/// The tokens that the parser reads.
#[derive(Debug)]
pub struct Tokens<'a> {
    lexer: Lexer<'a>,
    ahead: Option<Token<'a>>,
}

impl<'a> Tokens<'a> {
    /// The tokens of `text`.
    pub fn new(text: &'a str) -> Tokens<'a> {
        Tokens { lexer: Lexer::new(text), ahead: None }
    }

    /// Takes the notices that the tokens until now gave.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        self.lexer.take_notices()
    }

    /// The next token. After the last token it gives the end, token 0, again and again.
    pub fn next_token(&mut self) -> Result<Token<'a>, Error> {
        let mut current = match self.ahead.take() {
            Some(token) => token,
            None => self.lexer.next_token()?,
        };
        if !matches!(
            current.kind,
            token::FORMAT
                | token::NOT
                | token::NULLS_P
                | token::WITH
                | token::WITHOUT
                | token::UIDENT
                | token::USCONST
        ) {
            return Ok(current);
        }
        let next = self.lexer.next_token()?;
        current.kind = match (current.kind, next.kind) {
            (token::FORMAT, token::JSON) => token::FORMAT_LA,
            (
                token::NOT,
                token::BETWEEN | token::IN_P | token::LIKE | token::ILIKE | token::SIMILAR,
            ) => token::NOT_LA,
            (token::NULLS_P, token::FIRST_P | token::LAST_P) => token::NULLS_LA,
            (token::WITH, token::TIME | token::ORDINALITY) => token::WITH_LA,
            (token::WITHOUT, token::TIME) => token::WITHOUT_LA,
            (token::UIDENT | token::USCONST, _) => return self.unicode(current, next),
            (kind, _) => kind,
        };
        self.ahead = Some(next);
        Ok(current)
    }

    /// A `U&"..."` or a `U&'...'`, and the token after it.
    fn unicode(&mut self, mut current: Token<'a>, next: Token<'a>) -> Result<Token<'a>, Error> {
        let escape = if next.kind == token::UESCAPE {
            let third = self.lexer.next_token()?;
            let error = |message| {
                Error::syntax(self.lexer.text().as_bytes(), message, third.start, third.end)
            };
            if third.kind != token::SCONST {
                return Err(error("UESCAPE must be followed by a simple string literal"));
            }
            match third.text().map(str::as_bytes) {
                Some(&[c]) if is_escape(c) => {
                    // An error at the token after this one shows the text up to the end of the escape string, as it does in PostgreSQL.
                    current.end = third.end;
                    c
                }
                _ => return Err(error("invalid Unicode escape character")),
            }
        } else {
            self.ahead = Some(next);
            b'\\'
        };
        let text = current.text().unwrap_or_default();
        let text = Cow::Owned(unescape(text, escape, current.start)?);
        if current.kind == token::UIDENT {
            current.value = Value::Text(truncate(text, self.lexer.notices()));
            current.kind = token::IDENT;
        } else {
            current.value = Value::Text(text);
            current.kind = token::SCONST;
        }
        Ok(current)
    }
}

/// `check_uescapechar`: the escape character cannot be a hexadecimal digit, `+`, a quote, a double quote or a space.
fn is_escape(c: u8) -> bool {
    !(c.is_ascii_hexdigit()
        || matches!(c, b'+' | b'\'' | b'"' | b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
}

/// `str_udeescape`: makes the escapes of a `U&` string into characters. `position` is the offset of the `U&` token. An error points at the escape, at the offset in the value after `U&"`, as in PostgreSQL.
fn unescape(text: &str, escape: u8, position: usize) -> Result<String, Error> {
    let bytes = text.as_bytes();
    let byte = |i: usize| bytes.get(i).copied().unwrap_or(0);
    let hex = |from: usize, length: usize| {
        let digits = bytes.get(from..from + length)?;
        digits.iter().all(u8::is_ascii_hexdigit).then(|| {
            digits.iter().fold(0u32, |v, &d| (v << 4) | char::from(d).to_digit(16).unwrap_or(0))
        })
    };
    let location = |i: usize| position + 3 + i;
    let pair =
        |i: usize| Error::at(SqlState::SYNTAX_ERROR, "invalid Unicode surrogate pair", location(i));
    let mut out = String::with_capacity(text.len());
    let mut first = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != escape {
            if first != 0 {
                return Err(pair(i));
            }
            let length = text[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&text[i..i + length]);
            i += length;
            continue;
        }
        let (c, length) = if byte(i + 1) == escape {
            if first != 0 {
                return Err(pair(i));
            }
            out.push(char::from(escape));
            i += 2;
            continue;
        } else if let Some(c) = hex(i + 1, 4) {
            (c, 5)
        } else if byte(i + 1) == b'+'
            && let Some(c) = hex(i + 2, 6)
        {
            (c, 8)
        } else {
            return Err(Error {
                code: SqlState::SYNTAX_ERROR,
                message: "invalid Unicode escape".to_owned(),
                detail: None,
                hint: Some("Unicode escapes must be \\XXXX or \\+XXXXXX."),
                location: Some(location(i)),
            });
        };
        if c == 0 || c > 0x10ffff {
            return Err(Error::at(
                SqlState::SYNTAX_ERROR,
                "invalid Unicode escape value",
                location(i),
            ));
        }
        let c = if first != 0 {
            if !(0xdc00..=0xdfff).contains(&c) {
                return Err(pair(i));
            }
            let c = ((first & 0x3ff) << 10) + (c & 0x3ff) + 0x10000;
            first = 0;
            c
        } else if (0xdc00..=0xdfff).contains(&c) {
            return Err(pair(i));
        } else {
            c
        };
        if (0xd800..=0xdbff).contains(&c) {
            first = c;
        } else if let Some(c) = char::from_u32(c) {
            out.push(c);
        }
        i += length;
    }
    if first != 0 {
        return Err(pair(i));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<u16> {
        let mut tokens = Tokens::new(text);
        let mut kinds = Vec::new();
        loop {
            match tokens.next_token().unwrap().kind {
                0 => return kinds,
                kind => kinds.push(kind),
            }
        }
    }

    #[test]
    fn lookahead() {
        use token::*;
        assert_eq!(kinds("NOT IN NOT x"), [NOT_LA, IN_P, NOT, IDENT]);
        assert_eq!(
            kinds("NOT BETWEEN NOT LIKE NOT ILIKE NOT SIMILAR"),
            [NOT_LA, BETWEEN, NOT_LA, LIKE, NOT_LA, ILIKE, NOT_LA, SIMILAR]
        );
        assert_eq!(
            kinds("NULLS FIRST NULLS LAST NULLS x"),
            [NULLS_LA, FIRST_P, NULLS_LA, LAST_P, NULLS_P, IDENT]
        );
        assert_eq!(
            kinds("WITH TIME WITH ORDINALITY WITH"),
            [WITH_LA, TIME, WITH_LA, ORDINALITY, WITH]
        );
        assert_eq!(kinds("WITHOUT TIME WITHOUT"), [WITHOUT_LA, TIME, WITHOUT]);
        assert_eq!(kinds("FORMAT JSON FORMAT"), [FORMAT_LA, JSON, FORMAT]);
        // Two lookahead tokens in a row.
        assert_eq!(kinds("NOT NOT IN"), [NOT, NOT_LA, IN_P]);
    }

    #[test]
    fn unicode() {
        let mut tokens =
            Tokens::new("U&'\\0041\\+01F600\\\\' U&'!0042' UESCAPE '!' U&\"\\00e9\" 1");
        let a = tokens.next_token().unwrap();
        assert_eq!((a.kind, a.text()), (token::SCONST, Some("A😀\\")));
        let b = tokens.next_token().unwrap();
        assert_eq!((b.kind, b.text()), (token::SCONST, Some("B")));
        assert_eq!(&tokens.lexer.text()[b.start..b.end], "U&'!0042' UESCAPE '!'");
        let c = tokens.next_token().unwrap();
        assert_eq!((c.kind, c.text()), (token::IDENT, Some("é")));
        assert_eq!(tokens.next_token().unwrap().kind, token::ICONST);
        assert!(!is_escape(b'a') && !is_escape(b'+') && !is_escape(b' ') && is_escape(b'!'));
    }
}

//! The scanner of PL/pgSQL, as `pl_scanner.c` reads the body of a function: the tokens of the core lexer, a word for each identifier and keyword, and a place in the list so that the parser can push tokens back.

use rupg_common::{Error, Result};
use rupg_sql::{Lexer, Value, token};

/// The reserved keywords of PL/pgSQL, `pl_reserved_kwlist.h`. A reserved keyword is never the name of a variable.
const RESERVED: [&str; 22] = [
    "all", "begin", "by", "case", "declare", "else", "end", "for", "foreach", "from", "if", "in",
    "into", "loop", "not", "null", "or", "then", "to", "using", "when", "while",
];

/// The unreserved keywords of PL/pgSQL, `pl_unreserved_kwlist.h`. A variable can have the name of one, and the name of the variable wins.
const UNRESERVED: &[&str] = &[
    "absolute",
    "alias",
    "and",
    "array",
    "assert",
    "backward",
    "call",
    "chain",
    "close",
    "collate",
    "column",
    "column_name",
    "commit",
    "constant",
    "constraint",
    "constraint_name",
    "continue",
    "current",
    "cursor",
    "datatype",
    "debug",
    "default",
    "detail",
    "diagnostics",
    "do",
    "dump",
    "elseif",
    "elsif",
    "errcode",
    "error",
    "exception",
    "execute",
    "exit",
    "fetch",
    "first",
    "forward",
    "get",
    "hint",
    "import",
    "info",
    "insert",
    "is",
    "last",
    "log",
    "merge",
    "message",
    "message_text",
    "move",
    "next",
    "no",
    "notice",
    "open",
    "option",
    "perform",
    "pg_context",
    "pg_datatype_name",
    "pg_exception_context",
    "pg_exception_detail",
    "pg_exception_hint",
    "pg_routine_oid",
    "print_strict_params",
    "prior",
    "query",
    "raise",
    "relative",
    "return",
    "returned_sqlstate",
    "reverse",
    "rollback",
    "row_count",
    "rowtype",
    "schema",
    "schema_name",
    "scroll",
    "slice",
    "sqlstate",
    "stacked",
    "strict",
    "table",
    "table_name",
    "type",
    "use_column",
    "use_variable",
    "variable_conflict",
    "warning",
];

/// One token of the body.
#[derive(Clone, Debug)]
pub(crate) struct Tok {
    /// The token number of the core grammar, or 0 at the end of the body.
    pub(crate) kind: u16,
    /// The byte offset of the start of the token in the body.
    pub(crate) start: usize,
    /// The byte offset after the token.
    pub(crate) end: usize,
    /// The word in lower case for an identifier or a keyword, or `None`.
    pub(crate) word: Option<String>,
    /// True for an identifier in double quotes, which is never a keyword.
    pub(crate) quoted: bool,
    /// The text of a string, a number or an operator.
    pub(crate) text: Option<String>,
}

impl Tok {
    /// True for the character token `c`.
    pub(crate) fn is_char(&self, c: u8) -> bool {
        rupg_sql::character(c) == Some(self.kind)
    }

    /// True for the keyword `word`, which is a word that is not in double quotes.
    pub(crate) fn is(&self, word: &str) -> bool {
        !self.quoted && self.word.as_deref() == Some(word)
    }

    /// True for the operator `op`.
    pub(crate) fn is_op(&self, op: &str) -> bool {
        self.kind == token::OP && self.text.as_deref() == Some(op)
    }

    /// True at the end of the body.
    pub(crate) fn is_end(&self) -> bool {
        self.kind == 0
    }

    /// True for a reserved keyword of PL/pgSQL, which is never the name of a variable.
    pub(crate) fn is_reserved(&self) -> bool {
        !self.quoted && self.word.as_deref().is_some_and(|w| RESERVED.contains(&w))
    }

    /// True for an unreserved keyword of PL/pgSQL.
    pub(crate) fn is_unreserved(&self) -> bool {
        !self.quoted && self.word.as_deref().is_some_and(|w| UNRESERVED.contains(&w))
    }

    /// True for a word that can be a name, `IDENT` of the scanner of PL/pgSQL: an identifier or a keyword of SQL that is not a reserved keyword of PL/pgSQL.
    pub(crate) fn is_ident(&self) -> bool {
        self.word.is_some() && !self.is_reserved()
    }

    /// The text of the token in `src`.
    pub(crate) fn source<'s>(&self, src: &'s str) -> &'s str {
        &src[self.start..self.end]
    }
}

/// The tokens of a body, with the place of the next token.
#[derive(Debug)]
pub(crate) struct Scanner<'a> {
    pub(crate) src: &'a str,
    toks: Vec<Tok>,
    /// The error of the lexer at the end of `toks`, if any.
    error: Option<Error>,
    at: usize,
}

impl<'a> Scanner<'a> {
    /// The tokens of `src`. An error of the lexer comes when the parser reaches its place.
    pub(crate) fn new(src: &'a str) -> Scanner<'a> {
        let mut lexer = Lexer::new(src);
        let mut toks = Vec::new();
        let mut error = None;
        loop {
            match lexer.next_token() {
                Ok(t) => {
                    let end = t.kind == 0;
                    let quoted = t.kind == token::UIDENT
                        || (t.kind == token::IDENT && src.as_bytes().get(t.start) == Some(&b'"'));
                    let (word, text) = match &t.value {
                        Value::Keyword(k) => (Some(k.name.to_string()), None),
                        Value::Text(text) if matches!(t.kind, token::IDENT | token::UIDENT) => {
                            (Some(text.to_string()), None)
                        }
                        Value::Text(text) => (None, Some(text.to_string())),
                        Value::Integer(n) => (None, Some(n.to_string())),
                        Value::None => (None, None),
                    };
                    toks.push(Tok { kind: t.kind, start: t.start, end: t.end, word, quoted, text });
                    if end {
                        break;
                    }
                }
                Err(e) => {
                    let at = e.location;
                    error = Some(Error::from(e).with_position(at));
                    toks.push(Tok {
                        kind: 0,
                        start: src.len(),
                        end: src.len(),
                        word: None,
                        quoted: false,
                        text: None,
                    });
                    break;
                }
            }
        }
        Scanner { src, toks, error, at: 0 }
    }

    /// The next token, which the scanner passes.
    ///
    /// # Errors
    ///
    /// The error of the lexer at the place of the token.
    pub(crate) fn next(&mut self) -> Result<Tok> {
        let tok = self.peek()?;
        if !tok.is_end() {
            self.at += 1;
        }
        Ok(tok)
    }

    /// The next token, which stays next.
    ///
    /// # Errors
    ///
    /// The error of the lexer at the place of the token.
    pub(crate) fn peek(&self) -> Result<Tok> {
        self.peek_at(0)
    }

    /// The token `n` places after the next token.
    ///
    /// # Errors
    ///
    /// The error of the lexer at the place of the token.
    pub(crate) fn peek_at(&self, n: usize) -> Result<Tok> {
        let at = (self.at + n).min(self.toks.len() - 1);
        let tok = self.toks[at].clone();
        if tok.is_end()
            && let Some(error) = &self.error
        {
            return Err(error.clone());
        }
        Ok(tok)
    }

    /// `plpgsql_push_back_token`: the last token is next again.
    pub(crate) fn back(&mut self) {
        self.at = self.at.saturating_sub(1);
    }

    /// The place of the next token, for [`Scanner::reset`].
    pub(crate) fn place(&self) -> usize {
        self.at
    }

    /// Goes back to a place that [`Scanner::place`] gave.
    pub(crate) fn reset(&mut self, place: usize) {
        self.at = place;
    }

    /// `plpgsql_location_to_lineno`: the line of a byte offset, from 1.
    pub(crate) fn line(&self, at: usize) -> u32 {
        let at = at.min(self.src.len());
        let lines = self.src.as_bytes()[..at].iter().filter(|&&b| b == b'\n').count();
        u32::try_from(lines + 1).unwrap_or(u32::MAX)
    }
}

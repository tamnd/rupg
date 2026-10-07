//! The errors and the notices of the lexer and the parser, with the fields that PostgreSQL puts in an `ErrorResponse` and a `NoticeResponse`.
//!
//! Lifted from `crates/rudb-pgparse/src/error.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::fmt;

use rupg_common::SqlState;

/// An error of the lexer or the parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The SQLSTATE, for example `42601`.
    pub code: SqlState,
    /// The primary message, as PostgreSQL writes it.
    pub message: String,
    /// The detail, or `None`.
    pub detail: Option<&'static str>,
    /// The hint, or `None`.
    pub hint: Option<&'static str>,
    /// The byte offset in the text that the error points to, or `None`.
    pub location: Option<usize>,
}

impl Error {
    /// An error in the form of `scanner_yyerror`: the message, then `at end of input` when the location is at the end of the text, or `at or near` and the text from `start` to `end`.
    pub(crate) fn syntax(text: &[u8], message: &str, start: usize, end: usize) -> Error {
        let message = if start >= text.len() {
            format!("{message} at end of input")
        } else {
            let near = String::from_utf8_lossy(&text[start..end.clamp(start, text.len())]);
            format!("{message} at or near \"{near}\"")
        };
        Error {
            code: SqlState::SYNTAX_ERROR,
            message,
            detail: None,
            hint: None,
            location: Some(start),
        }
    }

    /// An error with a location and no `at or near` part.
    pub(crate) fn at(code: SqlState, message: &str, location: usize) -> Error {
        Error {
            code,
            message: message.to_owned(),
            detail: None,
            hint: None,
            location: Some(location),
        }
    }

    /// The error for an action of `gram.y` that is not ported yet. `rule` is its name in `productions.txt`, for example `a_expr.17`.
    pub(crate) fn not_ported(rule: &str) -> Error {
        Error {
            code: SqlState::FEATURE_NOT_SUPPORTED,
            message: format!("the action of the rule {rule} of gram.y is not ported"),
            detail: None,
            hint: None,
            location: None,
        }
    }

    /// An error for a fault of the parser itself, with SQLSTATE `XX000`.
    pub(crate) fn internal(message: &str) -> Error {
        Error {
            code: SqlState::INTERNAL_ERROR,
            message: message.to_owned(),
            detail: None,
            hint: None,
            location: None,
        }
    }

    /// The error that `report_invalid_encoding` gives for bytes that are not UTF-8, or that contain a zero byte. `bad` starts at the first bad byte.
    pub(crate) fn encoding(bad: &[u8]) -> Error {
        let first = bad.first().copied().unwrap_or(0);
        let length = match first {
            b if b & 0x80 == 0 => 1,
            b if b & 0xe0 == 0xc0 => 2,
            b if b & 0xf0 == 0xe0 => 3,
            b if b & 0xf8 == 0xf0 => 4,
            _ => 1,
        };
        let bytes: Vec<String> = bad.iter().take(length).map(|b| format!("0x{b:02x}")).collect();
        Error {
            code: SqlState::CHARACTER_NOT_IN_REPERTOIRE,
            message: format!("invalid byte sequence for encoding \"UTF8\": {}", bytes.join(" ")),
            detail: None,
            hint: None,
            location: None,
        }
    }

    /// The position that an `ErrorResponse` gives: the number of the character that the location points to in `text`, where the first character is 1.
    pub fn position(&self, text: &str) -> Option<usize> {
        let location = self.location?.min(text.len());
        let characters = text.as_bytes()[..location].iter().filter(|&&b| b & 0xc0 != 0x80).count();
        Some(characters + 1)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for rupg_common::Error {
    /// The error of the engine with the SQLSTATE, the message, the detail and the hint. The location stays in the parser error: [`Error::position`] gives the position of the `ErrorResponse`.
    fn from(error: Error) -> Self {
        let mut common = rupg_common::Error::new(error.code, error.message);
        if let Some(detail) = error.detail {
            common = common.with_detail(detail);
        }
        if let Some(hint) = error.hint {
            common = common.with_hint(hint);
        }
        common
    }
}

/// A notice or a warning of the lexer or the parser, for example for an identifier that is too long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The severity, `NOTICE` or `WARNING`.
    pub severity: Severity,
    /// The SQLSTATE, for example `42622`.
    pub code: SqlState,
    /// The message, as PostgreSQL writes it.
    pub message: String,
    /// The byte offset in the text that the notice points to, or `None`.
    pub location: Option<usize>,
}

/// The severity of a [`Notice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// `NOTICE`.
    Notice,
    /// `WARNING`.
    Warning,
}

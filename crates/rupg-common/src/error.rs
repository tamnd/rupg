//! `Error`, the error of every layer, with its `SqlState`.
//!
//! Every error carries a SQLSTATE from the layer where it starts (spec/04 section 4.8). A storage error that reaches a client is then the SQLSTATE that PostgreSQL sends for the same cause, for example `53100` for a full disk and `XX001` for a page with a bad checksum.

use std::fmt;

use crate::sqlstate::SqlState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    state: SqlState,
    message: String,
    position: Option<usize>,
    /// The fields that few errors have, in a box, so that a `Result` with an `Error` stays small.
    more: Option<Box<More>>,
}

/// The detail, the hint and the context of an error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct More {
    detail: Option<String>,
    hint: Option<String>,
    context: Option<String>,
}

impl Error {
    /// An error with a SQLSTATE and a primary message.
    pub fn new(state: SqlState, message: impl Into<String>) -> Error {
        Error { state, message: message.into(), position: None, more: None }
    }

    /// An `XX001` error. A check found damaged data.
    pub fn corrupted(message: impl Into<String>) -> Error {
        Error::new(SqlState::DATA_CORRUPTED, message)
    }

    /// An `XX000` error. rupg found a state that its own rules do not allow.
    pub fn internal(message: impl Into<String>) -> Error {
        Error::new(SqlState::INTERNAL_ERROR, message)
    }

    /// Adds the detail line.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Error {
        self.more().detail = Some(detail.into());
        self
    }

    /// Adds the hint line.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Error {
        self.more().hint = Some(hint.into());
        self
    }

    /// Adds the place in the query text where the error starts, as a byte offset. The session gives it to the client as a character position in the field `P`. An error that has a place keeps it.
    #[must_use]
    pub fn at(mut self, location: usize) -> Error {
        self.position.get_or_insert(location);
        self
    }

    /// Adds a line to the context, as `errcontext` does in an error context callback. The lines go from the inner place to the outer place.
    #[must_use]
    pub fn with_context(mut self, line: impl Into<String>) -> Error {
        let line = line.into();
        let more = self.more();
        more.context = Some(match more.context.take() {
            Some(context) => format!("{context}\n{line}"),
            None => line,
        });
        self
    }

    fn more(&mut self) -> &mut More {
        self.more.get_or_insert_with(Box::default)
    }

    /// The SQLSTATE.
    pub fn state(&self) -> SqlState {
        self.state
    }

    /// The primary message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The detail, if any.
    pub fn detail(&self) -> Option<&str> {
        self.more.as_ref()?.detail.as_deref()
    }

    /// The hint, if any.
    pub fn hint(&self) -> Option<&str> {
        self.more.as_ref()?.hint.as_deref()
    }

    /// The byte offset in the query text where the error starts, if any.
    pub fn position(&self) -> Option<usize> {
        self.position
    }

    /// The context lines, if any.
    pub fn context(&self) -> Option<&str> {
        self.more.as_ref()?.context.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.state, self.message)?;
        if let Some(detail) = self.detail() {
            write!(f, "\nDETAIL: {detail}")?;
        }
        if let Some(hint) = self.hint() {
            write!(f, "\nHINT: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// The result type of rupg.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn display() {
        let e = Error::corrupted("bad checksum").with_detail("page 7").with_hint("run rupg verify");
        assert_eq!(e.to_string(), "XX001: bad checksum\nDETAIL: page 7\nHINT: run rupg verify");
        assert_eq!((e.position(), e.clone().at(7).at(9).position()), (None, Some(7)));
    }
}

//! `Error`, the error of every layer, with its `SqlState`.
//!
//! Every error carries a SQLSTATE from the layer where it starts (spec/04 section 4.8). A storage error that reaches a client is then the SQLSTATE that PostgreSQL sends for the same cause, for example `53100` for a full disk and `XX001` for a page with a bad checksum.

use std::fmt;

use crate::sqlstate::SqlState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    state: SqlState,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
}

impl Error {
    /// An error with a SQLSTATE and a primary message.
    pub fn new(state: SqlState, message: impl Into<String>) -> Error {
        Error { state, message: message.into(), detail: None, hint: None }
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
        self.detail = Some(detail.into());
        self
    }

    /// Adds the hint line.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Error {
        self.hint = Some(hint.into());
        self
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
        self.detail.as_deref()
    }

    /// The hint, if any.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.state, self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, "\nDETAIL: {detail}")?;
        }
        if let Some(hint) = &self.hint {
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
    }
}

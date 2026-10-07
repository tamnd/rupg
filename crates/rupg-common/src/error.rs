//! `Error` and `SqlState`.
//!
//! Every error carries a SQLSTATE from the layer where it starts (spec/04 section 4.8). A storage error that reaches a client is then the SQLSTATE that PostgreSQL sends for the same cause, for example `53100` for a full disk and `XX001` for a page with a bad checksum.

use std::fmt;

/// A five character SQLSTATE code, as in `src/backend/utils/errcodes.txt` of the PostgreSQL pin.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// `08001` sqlclient_unable_to_establish_sqlconnection.
    pub const UNABLE_TO_CONNECT: SqlState = SqlState(*b"08001");
    /// `08006` connection_failure.
    pub const CONNECTION_FAILURE: SqlState = SqlState(*b"08006");
    /// `0A000` feature_not_supported.
    pub const FEATURE_NOT_SUPPORTED: SqlState = SqlState(*b"0A000");
    /// `22003` numeric_value_out_of_range.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: SqlState = SqlState(*b"22003");
    /// `22023` invalid_parameter_value.
    pub const INVALID_PARAMETER_VALUE: SqlState = SqlState(*b"22023");
    /// `22P02` invalid_text_representation.
    pub const INVALID_TEXT_REPRESENTATION: SqlState = SqlState(*b"22P02");
    /// `23505` unique_violation.
    pub const UNIQUE_VIOLATION: SqlState = SqlState(*b"23505");
    /// `25006` read_only_sql_transaction.
    pub const READ_ONLY_SQL_TRANSACTION: SqlState = SqlState(*b"25006");
    /// `40001` serialization_failure.
    pub const SERIALIZATION_FAILURE: SqlState = SqlState(*b"40001");
    /// `40P01` deadlock_detected.
    pub const DEADLOCK_DETECTED: SqlState = SqlState(*b"40P01");
    /// `42P01` undefined_table.
    pub const UNDEFINED_TABLE: SqlState = SqlState(*b"42P01");
    /// `53000` insufficient_resources.
    pub const INSUFFICIENT_RESOURCES: SqlState = SqlState(*b"53000");
    /// `53100` disk_full.
    pub const DISK_FULL: SqlState = SqlState(*b"53100");
    /// `53200` out_of_memory.
    pub const OUT_OF_MEMORY: SqlState = SqlState(*b"53200");
    /// `55000` object_not_in_prerequisite_state.
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: SqlState = SqlState(*b"55000");
    /// `55006` object_in_use.
    pub const OBJECT_IN_USE: SqlState = SqlState(*b"55006");
    /// `57P01` admin_shutdown.
    pub const ADMIN_SHUTDOWN: SqlState = SqlState(*b"57P01");
    /// `58030` io_error.
    pub const IO_ERROR: SqlState = SqlState(*b"58030");
    /// `58P01` undefined_file.
    pub const UNDEFINED_FILE: SqlState = SqlState(*b"58P01");
    /// `58P02` duplicate_file.
    pub const DUPLICATE_FILE: SqlState = SqlState(*b"58P02");
    /// `XX000` internal_error.
    pub const INTERNAL_ERROR: SqlState = SqlState(*b"XX000");
    /// `XX001` data_corrupted.
    pub const DATA_CORRUPTED: SqlState = SqlState(*b"XX001");

    /// Makes a code from five characters. Each character must be a digit or an upper case ASCII letter.
    pub const fn new(code: [u8; 5]) -> Option<SqlState> {
        let mut i = 0;
        while i < 5 {
            if !code[i].is_ascii_digit() && !code[i].is_ascii_uppercase() {
                return None;
            }
            i += 1;
        }
        Some(SqlState(code))
    }

    /// The five characters.
    pub fn code(&self) -> &str {
        // `new` and the constants allow only ASCII, so the fallback is not used.
        std::str::from_utf8(&self.0).unwrap_or("XX000")
    }

    /// The class: the first two characters.
    pub fn class(&self) -> &str {
        &self.code()[..2]
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl fmt::Debug for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SqlState({})", self.code())
    }
}

/// An error with a SQLSTATE, a message and an optional detail and hint, as in the PostgreSQL `ErrorResponse`.
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
    use super::{Error, SqlState};

    /// Each constant must be in the errcodes.txt of the pin, with the same name.
    #[test]
    fn the_constants_are_in_errcodes() {
        let text = include_str!("../../../vendor/postgres-19/src/backend/utils/errcodes.txt");
        let constants = [
            (SqlState::UNABLE_TO_CONNECT, "sqlclient_unable_to_establish_sqlconnection"),
            (SqlState::CONNECTION_FAILURE, "connection_failure"),
            (SqlState::FEATURE_NOT_SUPPORTED, "feature_not_supported"),
            (SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "numeric_value_out_of_range"),
            (SqlState::INVALID_PARAMETER_VALUE, "invalid_parameter_value"),
            (SqlState::INVALID_TEXT_REPRESENTATION, "invalid_text_representation"),
            (SqlState::UNIQUE_VIOLATION, "unique_violation"),
            (SqlState::READ_ONLY_SQL_TRANSACTION, "read_only_sql_transaction"),
            (SqlState::SERIALIZATION_FAILURE, "serialization_failure"),
            (SqlState::DEADLOCK_DETECTED, "deadlock_detected"),
            (SqlState::UNDEFINED_TABLE, "undefined_table"),
            (SqlState::INSUFFICIENT_RESOURCES, "insufficient_resources"),
            (SqlState::DISK_FULL, "disk_full"),
            (SqlState::OUT_OF_MEMORY, "out_of_memory"),
            (SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "object_not_in_prerequisite_state"),
            (SqlState::OBJECT_IN_USE, "object_in_use"),
            (SqlState::ADMIN_SHUTDOWN, "admin_shutdown"),
            (SqlState::IO_ERROR, "io_error"),
            (SqlState::UNDEFINED_FILE, "undefined_file"),
            (SqlState::DUPLICATE_FILE, "duplicate_file"),
            (SqlState::INTERNAL_ERROR, "internal_error"),
            (SqlState::DATA_CORRUPTED, "data_corrupted"),
        ];
        for (state, name) in constants {
            let found = text.lines().any(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                fields.len() == 4 && fields[0] == state.code() && fields[3] == name
            });
            assert!(found, "{state} {name} is not in errcodes.txt");
        }
    }

    #[test]
    fn codes_are_checked() {
        assert_eq!(SqlState::new(*b"XX001"), Some(SqlState::DATA_CORRUPTED));
        assert_eq!(SqlState::new(*b"xx001"), None);
        assert_eq!(SqlState::DISK_FULL.class(), "53");
    }

    #[test]
    fn display() {
        let e = Error::corrupted("bad checksum").with_detail("page 7").with_hint("run rupg verify");
        assert_eq!(e.to_string(), "XX001: bad checksum\nDETAIL: page 7\nHINT: run rupg verify");
    }
}

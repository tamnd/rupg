//! SQLSTATE codes, the five characters that a PostgreSQL client reads to know what failed.
//!
//! Drivers, ORMs and retry loops branch on the code and not on the message. tokio-postgres falls back to an older catalog query on `42P01` and `42703`, Rails prepares a statement again on `0A000`, and every retry loop looks for `40001` and `40P01`. So the code of an error is part of the interface (spec/04 section 4.8).
//!
//! `cargo xtask errcodes` makes the list in [`generated::sqlstate`](crate::generated) from `vendor/postgres-19/src/backend/utils/errcodes.txt`. It has one constant for each code line of the file, named as the C macro without the `ERRCODE_` prefix, for example [`SqlState::UNDEFINED_TABLE`]. The file has 268 code lines and 262 distinct codes, because six codes have a second macro name.
//!
//! Lifted from `crates/rudb-common/src/sqlstate.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::fmt;

use crate::generated::sqlstate::{CLASSES, CODES};

/// A SQLSTATE: five ASCII characters, each a digit or an upper case letter.
///
/// The first two characters are the class. A client that does not know a code can still act on its class, which is why a code that rupg cannot give exactly must at least be in the right class.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SqlState([u8; 5]);

/// What a code means for the statement, from the second column of `errcodes.txt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// Class `00`. The statement succeeded.
    Success,
    /// Classes `01` and `02`. The statement succeeded and the server has something to say.
    Warning,
    /// Every other class. The statement failed.
    Error,
}

impl SqlState {
    /// The code with these five bytes. Only the generated list and [`SqlState::parse`] call it, so every value is a valid code.
    pub(crate) const fn from_bytes(code: [u8; 5]) -> Self {
        Self(code)
    }

    /// Reads a code from text. Any five digits or upper case letters make a code, also one that is not in the list, because a client can send any code in `RAISE ... USING ERRCODE`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let bytes: [u8; 5] = text.as_bytes().try_into().ok()?;
        bytes.iter().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()).then_some(Self(bytes))
    }

    /// The code as text, for example `42P01`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The constructors accept ASCII only, so this cannot fail.
        std::str::from_utf8(&self.0).unwrap_or("XX000")
    }

    /// The class, which is the first two characters, for example `42`.
    #[must_use]
    pub fn class(&self) -> &str {
        &self.as_str()[..2]
    }

    /// The title of the class in `errcodes.txt`, for example `Syntax Error or Access Rule Violation`. `None` for a class that the file does not have.
    #[must_use]
    pub fn class_title(self) -> Option<&'static str> {
        let class = self.class();
        CLASSES.iter().find(|(code, _)| *code == class).map(|(_, title)| *title)
    }

    /// Whether the statement succeeded, succeeded with a warning or failed. A code that is not in the list takes the category of its class, as in PostgreSQL.
    #[must_use]
    pub fn category(self) -> Category {
        match self.class() {
            "00" => Category::Success,
            "01" | "02" => Category::Warning,
            _ => Category::Error,
        }
    }

    /// The name of the constant for this code, for example `UNDEFINED_TABLE`. When a code has two names, this is the first one in the file. `None` for a code that is not in the list.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        CODES.iter().find(|(state, ..)| *state == self).map(|(_, _, name, _)| *name)
    }

    /// The PL/pgSQL condition name, for example `undefined_table`. `None` for a code that is not in the list.
    #[must_use]
    pub fn condition(self) -> Option<&'static str> {
        CODES
            .iter()
            .find(|(state, _, _, condition)| *state == self && !condition.is_empty())
            .map(|(_, _, _, condition)| *condition)
    }

    /// Every code with this PL/pgSQL condition name. Most names have one code. Five names have two, for example `string_data_right_truncation` is both `01004` and `22001`, and an exception handler for the name catches both.
    pub fn for_condition(name: &str) -> impl Iterator<Item = Self> + '_ {
        CODES.iter().filter(move |(.., condition)| *condition == name).map(|(state, ..)| *state)
    }

    /// Every code line of `errcodes.txt` in file order, as the code and the constant name.
    pub fn all() -> impl Iterator<Item = (Self, &'static str)> {
        CODES.iter().map(|(state, _, name, _)| (*state, *name))
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SqlState({})", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{Category, SqlState};

    #[test]
    fn the_list_has_every_code_line_of_the_pin() {
        assert_eq!(SqlState::all().count(), 268);
        let mut distinct: Vec<SqlState> = SqlState::all().map(|(state, _)| state).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 262);
    }

    #[test]
    fn a_constant_is_its_code() {
        assert_eq!(SqlState::UNDEFINED_TABLE.as_str(), "42P01");
        assert_eq!(SqlState::UNDEFINED_COLUMN.to_string(), "42703");
        assert_eq!(SqlState::FEATURE_NOT_SUPPORTED.as_str(), "0A000");
        assert_eq!(SqlState::T_R_SERIALIZATION_FAILURE.as_str(), "40001");
        assert_eq!(SqlState::INTERNAL_ERROR.as_str(), "XX000");
        assert_eq!(format!("{:?}", SqlState::SYNTAX_ERROR), "SqlState(42601)");
    }

    #[test]
    fn a_second_name_is_the_same_code() {
        assert_eq!(SqlState::UNDEFINED_DATABASE, SqlState::INVALID_CATALOG_NAME);
        assert_eq!(SqlState::UNDEFINED_DATABASE.name(), Some("INVALID_CATALOG_NAME"));
        assert_eq!(SqlState::UNDEFINED_DATABASE.condition(), Some("invalid_catalog_name"));
    }

    #[test]
    fn a_condition_name_finds_every_code_that_has_it() {
        let found: Vec<String> = SqlState::for_condition("string_data_right_truncation")
            .map(|s| s.to_string())
            .collect();
        assert_eq!(found, ["01004", "22001"]);
        assert_eq!(SqlState::for_condition("no_such_condition").count(), 0);
        assert_eq!(SqlState::UNIQUE_VIOLATION.condition(), Some("unique_violation"));
    }

    #[test]
    fn the_class_gives_the_category_and_the_title() {
        assert_eq!(SqlState::UNDEFINED_TABLE.class(), "42");
        assert_eq!(
            SqlState::UNDEFINED_TABLE.class_title(),
            Some("Syntax Error or Access Rule Violation")
        );
        assert_eq!(SqlState::SUCCESSFUL_COMPLETION.category(), Category::Success);
        assert_eq!(SqlState::NO_DATA.category(), Category::Warning);
        assert_eq!(SqlState::DIVISION_BY_ZERO.category(), Category::Error);
    }

    #[test]
    fn parse_takes_any_well_formed_code() {
        assert_eq!(SqlState::parse("42P01"), Some(SqlState::UNDEFINED_TABLE));
        let custom = SqlState::parse("P9999").expect("a well formed code");
        assert_eq!(custom.name(), None);
        assert_eq!(custom.category(), Category::Error);
        assert_eq!(SqlState::parse("42p01"), None);
        assert_eq!(SqlState::parse("4201"), None);
        assert_eq!(SqlState::parse("42P01X"), None);
    }

    #[test]
    fn the_generated_category_agrees_with_the_class() {
        for (state, category, _, _) in &crate::generated::sqlstate::CODES {
            assert_eq!(state.category(), *category, "{state}");
        }
    }
}

//! `Error`, the error of every layer, with its `SqlState`.
//!
//! Every error carries a SQLSTATE from the layer where it starts (spec/04 section 4.8). A storage error that reaches a client is then the SQLSTATE that PostgreSQL sends for the same cause, for example `53100` for a full disk and `XX001` for a page with a bad checksum.

use std::fmt;
use std::panic::Location;

use crate::sqlstate::SqlState;

#[derive(Clone, Debug)]
pub struct Error {
    state: SqlState,
    message: String,
    position: Option<usize>,
    /// The place in the source of rupg where the error was made. The session sends it in the fields `F`, `L` and `R`, as PostgreSQL sends the place in its C source.
    place: &'static Location<'static>,
    /// The fields that few errors have, in a box, so that a `Result` with an `Error` stays small.
    more: Option<Box<More>>,
}

/// The detail, the hint and the context of an error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct More {
    detail: Option<String>,
    hint: Option<String>,
    context: Option<String>,
    /// True for a message of `ereport(NOTICE)` with a SQLSTATE outside of class `00`.
    notice: bool,
    /// The level of a message that is not an error, as `ereport` of `RAISE` gives it, or `None` for the level that the SQLSTATE gives.
    level: Option<Level>,
    /// The text of an internal query and the byte offset in it where the error starts, as `internalerrquery` and `internalerrposition` give them.
    internal: Option<(String, Option<usize>)>,
    /// The names of the object of the error, in the order of [`Object::ALL`].
    objects: [Option<String>; 5],
}

/// A name of the object of an error, as `errtable`, `errtablecol`, `errdatatype` and `errconstraint` give it. A client reads it from the field with the code of [`Object::code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Schema,
    Table,
    Column,
    Datatype,
    Constraint,
}

impl Object {
    /// The names in the order of the fields of a message.
    pub const ALL: [Object; 5] =
        [Object::Schema, Object::Table, Object::Column, Object::Datatype, Object::Constraint];

    /// The code of the field of a message: `s`, `t`, `c`, `d` or `n`.
    pub fn code(self) -> u8 {
        match self {
            Object::Schema => b's',
            Object::Table => b't',
            Object::Column => b'c',
            Object::Datatype => b'd',
            Object::Constraint => b'n',
        }
    }
}

/// The level of a message that is not an error, from the lowest to the highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// `DEBUG`, which goes to the client only when `client_min_messages` is a level of `DEBUG`.
    Debug,
    /// `LOG`, which goes to the client only when `client_min_messages` is `log` or lower.
    Log,
    /// `INFO`, which always goes to the client.
    Info,
    /// `NOTICE`.
    Notice,
    /// `WARNING`.
    Warning,
}

impl Level {
    /// The name of the level in the field `S` of a message.
    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Log => "LOG",
            Level::Info => "INFO",
            Level::Notice => "NOTICE",
            Level::Warning => "WARNING",
        }
    }
}

impl Error {
    /// An error with a SQLSTATE and a primary message.
    #[track_caller]
    pub fn new(state: SqlState, message: impl Into<String>) -> Error {
        Error {
            state,
            message: message.into(),
            position: None,
            place: Location::caller(),
            more: None,
        }
    }

    /// An `XX001` error. A check found damaged data.
    #[track_caller]
    pub fn corrupted(message: impl Into<String>) -> Error {
        Error::new(SqlState::DATA_CORRUPTED, message)
    }

    /// An `XX000` error. rupg found a state that its own rules do not allow.
    #[track_caller]
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

    /// Sets the place in the query text and drops the place that the error has, as `parser_errposition` in an error context callback does. `None` drops the place.
    #[must_use]
    pub fn with_position(mut self, location: Option<usize>) -> Error {
        self.position = location;
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

    /// Marks a message for the client as a `NOTICE`, as `ereport(NOTICE)` with a SQLSTATE such as `54000` makes it. A message without the mark is a `NOTICE` only for a SQLSTATE of class `00`.
    #[must_use]
    pub fn as_notice(mut self) -> Error {
        self.more().notice = true;
        self
    }

    /// True for a message that [`Error::as_notice`] marked.
    pub fn is_notice(&self) -> bool {
        self.more.as_ref().is_some_and(|more| more.notice)
    }

    /// Sets the level of a message that is not an error, as `ereport` with a level such as `INFO` gives it.
    #[must_use]
    pub fn with_level(mut self, level: Level) -> Error {
        self.more().level = Some(level);
        self
    }

    /// The level that [`Error::with_level`] set, if any.
    pub fn level(&self) -> Option<Level> {
        self.more.as_ref()?.level
    }

    /// Sets the internal query and the byte offset in it where the error starts, as `internalerrquery` and `internalerrposition` do. The session gives them to the client in the fields `q` and `p`.
    #[must_use]
    pub fn with_internal(mut self, query: impl Into<String>, position: Option<usize>) -> Error {
        self.more().internal = Some((query.into(), position));
        self
    }

    /// The internal query and the byte offset in it, if any.
    pub fn internal_query(&self) -> Option<(&str, Option<usize>)> {
        let (query, position) = self.more.as_ref()?.internal.as_ref()?;
        Some((query, *position))
    }

    /// Sets a name of the object of the error, such as the table of a constraint that the error violates.
    #[must_use]
    pub fn with_object(mut self, object: Object, name: impl Into<String>) -> Error {
        self.more().objects[object as usize] = Some(name.into());
        self
    }

    /// The name that [`Error::with_object`] set, if any.
    pub fn object(&self, object: Object) -> Option<&str> {
        self.more.as_ref()?.objects[object as usize].as_deref()
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

    /// The place in the source of rupg where the error was made.
    pub fn place(&self) -> &'static Location<'static> {
        self.place
    }
}

/// Two errors are equal when they give the same fields to the client. The place in the source is not compared.
impl PartialEq for Error {
    fn eq(&self, other: &Error) -> bool {
        (self.state, &self.message, self.position, &self.more)
            == (other.state, &other.message, other.position, &other.more)
    }
}

impl Eq for Error {}

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
    use super::{Error, Object};

    #[test]
    fn display() {
        let e = Error::corrupted("bad checksum").with_detail("page 7").with_hint("run rupg verify");
        assert_eq!(e.to_string(), "XX001: bad checksum\nDETAIL: page 7\nHINT: run rupg verify");
        assert_eq!((e.position(), e.clone().at(7).at(9).position()), (None, Some(7)));
    }

    #[test]
    fn the_place_is_where_the_error_is_made() {
        let (e, line) = (Error::internal("bad state"), line!());
        assert_eq!((e.place().file(), e.place().line()), (file!(), line));
        assert_eq!(e, Error::internal("bad state"));
    }

    #[test]
    fn the_names_of_the_object() {
        let e =
            Error::internal("x").with_object(Object::Table, "t").with_object(Object::Schema, "s");
        let names: Vec<_> = Object::ALL.iter().map(|&o| e.object(o)).collect();
        assert_eq!(names, [Some("s"), Some("t"), None, None, None]);
        assert_ne!(e, Error::internal("x"));
    }
}

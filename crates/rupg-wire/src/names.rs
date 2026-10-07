//! The prepared statements and the portals of a session, by name.
//!
//! PostgreSQL keeps the named statements and the portals in hash tables with keys of `NAMEDATALEN` bytes, so it compares two names on their first 63 bytes only. A name that is longer than that finds the entry of its first 63 bytes, and the entry keeps the cut name. The errors show the name that the client sent, in full. These registries do the same, with the errors of `postgres.c`, `prepare.c` and `portalmem.c`. They are generic, so the server keeps in them what it needs: a plan, a portal with its executor state, or a cursor of SQL.
//!
//! Lifted from `crates/rudb-pgwire/src/names.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::HashMap;

use crate::error::{Level, ProtocolError};
use crate::startup::NAME_LIMIT;

const INVALID_SQL_STATEMENT_NAME: &str = "26000";
const INVALID_CURSOR_NAME: &str = "34000";
const DUPLICATE_CURSOR: &str = "42P03";
const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";

/// The part of a name that PostgreSQL compares.
fn key(name: &[u8]) -> &[u8] {
    &name[..name.len().min(NAME_LIMIT)]
}

fn show(name: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(name)
}

/// The prepared statements of a session: the unnamed statement of the extended protocol, and the named statements of `Parse` and of `PREPARE`, which share one namespace.
///
/// Prepared statements are not part of a transaction. They stay after `ROLLBACK`, and only `Close`, `DEALLOCATE` and the end of the session remove them.
#[derive(Debug, Clone)]
pub struct Statements<S> {
    unnamed: Option<S>,
    named: HashMap<Box<[u8]>, S>,
}

impl<S> Default for Statements<S> {
    fn default() -> Statements<S> {
        Statements { unnamed: None, named: HashMap::new() }
    }
}

impl<S> Statements<S> {
    pub fn new() -> Statements<S> {
        Statements::default()
    }

    /// Call this when a `Parse` starts, before the query is parsed. A `Parse` of the unnamed statement first removes the old one, so after a `Parse` that fails there is no unnamed statement.
    pub fn start_parse(&mut self, name: &[u8]) {
        if name.is_empty() {
            self.unnamed = None;
        }
    }

    /// Stores a statement. A new unnamed statement replaces the old one.
    ///
    /// PostgreSQL looks for a named statement with the same name after it parses and analyzes the query, so a syntax error comes before this error.
    ///
    /// # Errors
    ///
    /// SQLSTATE `42P05` when a named statement has the same first 63 bytes.
    pub fn insert(&mut self, name: &[u8], statement: S) -> Result<(), ProtocolError> {
        if name.is_empty() {
            self.unnamed = Some(statement);
            return Ok(());
        }
        if self.named.contains_key(key(name)) {
            return Err(ProtocolError::new(
                Level::Error,
                DUPLICATE_PREPARED_STATEMENT,
                format!("prepared statement \"{}\" already exists", show(name)),
            ));
        }
        self.named.insert(key(name).into(), statement);
        Ok(())
    }

    /// The statement for `Bind`, `Describe` and `EXECUTE`.
    ///
    /// # Errors
    ///
    /// SQLSTATE `26000` when there is no such statement.
    pub fn get(&self, name: &[u8]) -> Result<&S, ProtocolError> {
        let found = if name.is_empty() { self.unnamed.as_ref() } else { self.named.get(key(name)) };
        found.ok_or_else(|| missing_statement(name))
    }

    /// [`Statements::get`] for a change.
    ///
    /// # Errors
    ///
    /// SQLSTATE `26000` when there is no such statement.
    pub fn get_mut(&mut self, name: &[u8]) -> Result<&mut S, ProtocolError> {
        let found =
            if name.is_empty() { self.unnamed.as_mut() } else { self.named.get_mut(key(name)) };
        found.ok_or_else(|| missing_statement(name))
    }

    /// The `Close` of a statement. A name that is not there is not an error.
    pub fn close(&mut self, name: &[u8]) -> Option<S> {
        if name.is_empty() { self.unnamed.take() } else { self.named.remove(key(name)) }
    }

    /// `DEALLOCATE name`. It does not see the unnamed statement.
    ///
    /// # Errors
    ///
    /// SQLSTATE `26000` when there is no such statement.
    pub fn deallocate(&mut self, name: &[u8]) -> Result<S, ProtocolError> {
        self.named.remove(key(name)).ok_or_else(|| missing_statement(name))
    }

    /// `DEALLOCATE ALL`, which keeps the unnamed statement.
    pub fn deallocate_all(&mut self) {
        self.named.clear();
    }

    /// The named statements, with the names cut to 63 bytes, as `pg_prepared_statements` shows them. The order is not defined.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &S)> {
        self.named.iter().map(|(name, statement)| (&name[..], statement))
    }

    /// The number of named statements.
    pub fn len(&self) -> usize {
        self.named.len()
    }

    pub fn is_empty(&self) -> bool {
        self.named.is_empty()
    }
}

fn missing_statement(name: &[u8]) -> ProtocolError {
    let message = if name.is_empty() {
        "unnamed prepared statement does not exist".to_owned()
    } else {
        format!("prepared statement \"{}\" does not exist", show(name))
    };
    ProtocolError::new(Level::Error, INVALID_SQL_STATEMENT_NAME, message)
}

/// The portals of a session: the unnamed portal, the named portals of `Bind`, and the cursors of `DECLARE`, which share one namespace. `Execute` can run a cursor, and `FETCH` can read a portal of `Bind`.
#[derive(Debug, Clone)]
pub struct Portals<P> {
    portals: HashMap<Box<[u8]>, P>,
}

impl<P> Default for Portals<P> {
    fn default() -> Portals<P> {
        Portals { portals: HashMap::new() }
    }
}

impl<P> Portals<P> {
    pub fn new() -> Portals<P> {
        Portals::default()
    }

    /// Call this in a `Bind` before it makes the portal. PostgreSQL does it after the counts and the check for an aborted transaction, and before it reads the values. The unnamed portal is removed without a word.
    ///
    /// # Errors
    ///
    /// SQLSTATE `42P03` when a named portal has the same first 63 bytes. `DECLARE` gets the same error.
    pub fn make_room(&mut self, name: &[u8]) -> Result<(), ProtocolError> {
        if name.is_empty() {
            self.portals.remove(&b""[..]);
        } else if self.portals.contains_key(key(name)) {
            return Err(ProtocolError::new(
                Level::Error,
                DUPLICATE_CURSOR,
                format!("cursor \"{}\" already exists", show(name)),
            ));
        }
        Ok(())
    }

    /// Stores a portal after [`Portals::make_room`]. It replaces a portal with the same name.
    pub fn insert(&mut self, name: &[u8], portal: P) {
        self.portals.insert(key(name).into(), portal);
    }

    /// The portal for `Describe` and `Execute`, or `None` for `FETCH` and `CLOSE`, which have their own error.
    pub fn find(&self, name: &[u8]) -> Option<&P> {
        self.portals.get(key(name))
    }

    /// [`Portals::find`] for a change.
    pub fn find_mut(&mut self, name: &[u8]) -> Option<&mut P> {
        self.portals.get_mut(key(name))
    }

    /// The portal for `Describe` and `Execute`.
    ///
    /// # Errors
    ///
    /// SQLSTATE `34000` when there is no such portal. The unnamed portal has no text of its own.
    pub fn get(&self, name: &[u8]) -> Result<&P, ProtocolError> {
        self.find(name).ok_or_else(|| missing_portal(name))
    }

    /// [`Portals::get`] for a change.
    ///
    /// # Errors
    ///
    /// SQLSTATE `34000` when there is no such portal.
    pub fn get_mut(&mut self, name: &[u8]) -> Result<&mut P, ProtocolError> {
        self.portals.get_mut(key(name)).ok_or_else(|| missing_portal(name))
    }

    /// The `Close` of a portal. A name that is not there is not an error.
    pub fn close(&mut self, name: &[u8]) -> Option<P> {
        self.portals.remove(key(name))
    }

    /// Keeps the portals for which `keep` is true. At the end of a transaction PostgreSQL removes every portal that is not a cursor `WITH HOLD`, and after a failed transaction it also removes a cursor `WITH HOLD` that the transaction made.
    pub fn retain(&mut self, mut keep: impl FnMut(&[u8], &mut P) -> bool) {
        self.portals.retain(|name, portal| keep(name, portal));
    }

    /// `CLOSE ALL`, and the end of the session.
    pub fn clear(&mut self) {
        self.portals.clear();
    }

    /// The portals, with the names cut to 63 bytes. The order is not defined.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &P)> {
        self.portals.iter().map(|(name, portal)| (&name[..], portal))
    }

    pub fn len(&self) -> usize {
        self.portals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.portals.is_empty()
    }
}

fn missing_portal(name: &[u8]) -> ProtocolError {
    ProtocolError::new(
        Level::Error,
        INVALID_CURSOR_NAME,
        format!("portal \"{}\" does not exist", show(name)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(result: Result<impl std::fmt::Debug, ProtocolError>) -> (&'static str, String) {
        let error = result.unwrap_err();
        (error.sqlstate, error.message)
    }

    #[test]
    fn a_named_statement_is_not_replaced() {
        let mut statements = Statements::new();
        statements.insert(b"s1", 1).unwrap();
        assert_eq!(
            error(statements.insert(b"s1", 2)),
            ("42P05", "prepared statement \"s1\" already exists".to_owned())
        );
        assert_eq!(statements.get(b"s1"), Ok(&1));
        statements.insert(b"", 3).unwrap();
        statements.insert(b"", 4).unwrap();
        assert_eq!(statements.get(b""), Ok(&4));
        assert_eq!(statements.len(), 1);
    }

    #[test]
    fn a_failed_parse_of_the_unnamed_statement_leaves_none() {
        let mut statements = Statements::new();
        statements.insert(b"", 1).unwrap();
        statements.start_parse(b"s1");
        assert_eq!(statements.get(b""), Ok(&1));
        statements.start_parse(b"");
        assert_eq!(
            error(statements.get(b"")),
            ("26000", "unnamed prepared statement does not exist".to_owned())
        );
        assert_eq!(
            error(statements.get(b"nope")),
            ("26000", "prepared statement \"nope\" does not exist".to_owned())
        );
    }

    #[test]
    fn statement_names_compare_on_63_bytes() {
        let long_a = [b"x".repeat(63), b"a".to_vec()].concat();
        let long_b = [b"x".repeat(63), b"b".to_vec()].concat();
        let mut statements = Statements::new();
        statements.insert(&long_a, 1).unwrap();
        let (code, message) = error(statements.insert(&long_b, 2));
        assert_eq!(code, "42P05");
        assert!(message.contains(&format!("\"{}\"", show(&long_b))));
        assert_eq!(statements.get(&long_b), Ok(&1));
        assert_eq!(statements.get(&long_a[..63]), Ok(&1));
        assert_eq!(statements.iter().next().unwrap().0, &long_a[..63]);
        assert!(statements.get(&long_a[..62]).is_err());
    }

    #[test]
    fn close_and_deallocate() {
        let mut statements = Statements::new();
        statements.insert(b"", 1).unwrap();
        statements.insert(b"a", 2).unwrap();
        statements.insert(b"b", 3).unwrap();
        assert_eq!(statements.close(b"nope"), None);
        assert_eq!(statements.close(b"a"), Some(2));
        assert_eq!(
            error(statements.deallocate(b"a")),
            ("26000", "prepared statement \"a\" does not exist".to_owned())
        );
        assert!(statements.deallocate(b"").is_err());
        assert_eq!(statements.deallocate(b"b"), Ok(3));
        statements.insert(b"c", 4).unwrap();
        statements.deallocate_all();
        assert!(statements.is_empty());
        assert_eq!(statements.get(b""), Ok(&1));
    }

    #[test]
    fn a_named_portal_is_not_replaced() {
        let mut portals = Portals::new();
        portals.make_room(b"p").unwrap();
        portals.insert(b"p", 1);
        assert_eq!(
            error(portals.make_room(b"p")),
            ("42P03", "cursor \"p\" already exists".to_owned())
        );
        portals.make_room(b"").unwrap();
        portals.insert(b"", 2);
        portals.make_room(b"").unwrap();
        assert_eq!(error(portals.get(b"")), ("34000", "portal \"\" does not exist".to_owned()));
        assert_eq!(
            error(portals.get(b"nope")),
            ("34000", "portal \"nope\" does not exist".to_owned())
        );
        assert_eq!(portals.close(b"nope"), None);
        assert_eq!(portals.close(b"p"), Some(1));
    }

    #[test]
    fn portal_names_compare_on_63_bytes() {
        let long_a = [b"p".repeat(63), b"a".to_vec()].concat();
        let long_b = [b"p".repeat(63), b"b".to_vec()].concat();
        let mut portals = Portals::new();
        portals.make_room(&long_a).unwrap();
        portals.insert(&long_a, 1);
        assert_eq!(error(portals.make_room(&long_b)).0, "42P03");
        assert_eq!(portals.get(&long_b), Ok(&1));
        *portals.get_mut(&long_b).unwrap() = 2;
        assert_eq!(portals.find(&long_a[..63]), Some(&2));
    }

    #[test]
    fn the_end_of_a_transaction_keeps_the_held_cursors() {
        let mut portals = Portals::new();
        portals.insert(b"", false);
        portals.insert(b"p", false);
        portals.insert(b"held", true);
        portals.retain(|_, held| *held);
        assert_eq!(portals.iter().map(|(name, _)| name).collect::<Vec<_>>(), [&b"held"[..]]);
        portals.clear();
        assert!(portals.is_empty());
    }
}

//! `TRUNCATE`, as `ExecuteTruncate` and `ExecuteTruncateGuts` in `tablecmds.c` run it: the checks of each table, the tables that `CASCADE` adds by their foreign keys, the check of `heap_truncate_check_FKs` and the sequences that `RESTART IDENTITY` resets. The caller removes the rows of the tables in [`super::Defined::truncated`].

use rupg_catalog::SequenceData;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{DropBehavior, Node, TruncateStmt};

use super::comment::not_owner;
use super::{Definer, Found, not_yet, relname};

impl Definer<'_, '_> {
    /// `ExecuteTruncate` and `ExecuteTruncateGuts`. A table that the list names again is checked again and then skipped.
    pub(super) fn truncate(&mut self, stmt: &TruncateStmt) -> Result<()> {
        let mut tables: Vec<u32> = Vec::new();
        for node in stmt.relations.iter().flatten() {
            let Node::RangeVar(rv) = node else { continue };
            let found = self.lookup_relation(rv)?;
            let table = self.truncate_check(found, relname(rv))?;
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
        if stmt.behavior == DropBehavior::DROP_CASCADE {
            loop {
                let added: Vec<u32> =
                    self.referencing(&tables).into_iter().filter(|t| !tables.contains(t)).collect();
                if added.is_empty() {
                    break;
                }
                for table in added {
                    let name = self.catalog.relation(table).map(|r| r.name.clone());
                    let name = name.unwrap_or_default();
                    self.notice(
                        SqlState::SUCCESSFUL_COMPLETION,
                        format!("truncate cascades to table \"{name}\""),
                    );
                    self.truncate_check(Found::User(table), &name)?;
                    tables.push(table);
                }
            }
        } else {
            self.check_truncate_keys(&tables)?;
        }
        let mut sequences = Vec::new();
        if stmt.restart_seqs {
            for &table in &tables {
                for sequence in self.catalog.owned_sequences(table) {
                    let rel = self.catalog.relation(sequence);
                    let owner = rel.map_or(0, |r| r.owner);
                    if !self.is_superuser() && owner != self.user {
                        let name = rel.map(|r| r.name.clone()).unwrap_or_default();
                        return Err(not_owner("sequence", &name));
                    }
                    sequences.push(sequence);
                }
            }
        }
        for sequence in sequences {
            self.reset_sequence(sequence)?;
        }
        self.truncated = tables;
        Ok(())
    }

    /// `truncate_check_rel` and `truncate_check_perms`: the relation must be a table that is not a system catalog, and the role must own it. rupg has no `GRANT`, so the privilege `TRUNCATE` is the privilege of the owner.
    fn truncate_check(&self, found: Found, name: &str) -> Result<u32> {
        if !matches!(self.kind_of(found), b'r' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a table"),
            ));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && owner != self.user {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {name}"),
            ));
        }
        match found {
            Found::User(oid) => Ok(oid),
            Found::Builtin(_) => Err(not_yet("TRUNCATE of a built-in table", None)),
        }
    }

    /// `heap_truncate_find_FKs`: the tables with a foreign key that refers to one of the tables, in the order of their OIDs.
    fn referencing(&self, tables: &[u32]) -> Vec<u32> {
        let mut found: Vec<u32> = self
            .catalog
            .constraints()
            .filter(|c| c.foreign.as_ref().is_some_and(|f| tables.contains(&f.table)))
            .map(|c| c.relation)
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }

    /// `heap_truncate_check_FKs`: with `RESTRICT`, each table with a foreign key that refers to a table of the list must be in the list. Only the tables with `relhastriggers` count. The error names the first table of the list, in the order of the statement, that such a table refers to, and the first such table in the order of the OIDs.
    fn check_truncate_keys(&self, tables: &[u32]) -> Result<()> {
        let with_triggers: Vec<u32> = tables
            .iter()
            .copied()
            .filter(|&t| self.catalog.relation(t).is_some_and(|r| r.has_triggers))
            .collect();
        for &table in &with_triggers {
            let Some(other) =
                self.referencing(&[table]).into_iter().find(|t| !with_triggers.contains(t))
            else {
                continue;
            };
            let name =
                |oid: u32| self.catalog.relation(oid).map(|r| r.name.clone()).unwrap_or_default();
            let (table, other) = (name(table), name(other));
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot truncate a table referenced in a foreign key constraint",
            )
            .with_detail(format!("Table \"{other}\" references \"{table}\"."))
            .with_hint(format!(
                "Truncate table \"{other}\" at the same time, or use TRUNCATE ... CASCADE."
            )));
        }
        Ok(())
    }

    /// `ResetSequence`: the sequence gets a new file with its start value, so that a rollback undoes the change. The values that the sequence gave before stay used.
    fn reset_sequence(&mut self, sequence: u32) -> Result<()> {
        let Some(info) = self.catalog.relation(sequence).and_then(|r| r.sequence.clone()) else {
            return Err(Error::internal(format!("cache lookup failed for sequence {sequence}")));
        };
        let start = info.start;
        self.catalog.alter_sequence(sequence, info, Some(SequenceData::new(start)))
    }
}

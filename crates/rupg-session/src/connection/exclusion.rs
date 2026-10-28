//! The checks of the exclusion constraints, as `check_exclusion_or_unique_constraint` does them. A new version conflicts with another version when the operator of each key column gives true for their keys.
//!
//! PostgreSQL checks each version when it inserts it, so it sees the versions that the statement inserted before it and the old versions that the statement did not update yet. rupg checks the new versions after the statement, in the order of the inserts, with the versions as they were at the time of each insert. A deferrable constraint checks the live versions after the statement or at the commit, as `unique_key_recheck` does it.

use std::sync::Arc;

use rupg_analyze::{Exclusion, OnConflict, Write};
use rupg_catalog::Catalog;
use rupg_common::{Error, SqlState};
use rupg_types::Value;

use super::Connection;
use crate::heap::{Candidate, View, tid_text};

/// How a check sees the versions that it compares with a new version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    /// Each new version at the time of its insert.
    Insert,
    /// The new versions that are live, with the live versions.
    Recheck,
    /// All the live versions, as the build of a new index sees them.
    Build,
}

impl Connection {
    /// The end of a statement: the exclusion constraints of the tables that the commands from `command` on wrote. The constraints that are deferred go to the queue of the transaction.
    ///
    /// # Errors
    ///
    /// A conflict, and the errors of a wait and of the operators.
    pub(super) fn check_exclusions(&self, command: u32) -> Result<(), Error> {
        let catalog = self.catalog();
        let mut tables: Vec<u32> =
            catalog.constraints().filter(|c| c.is_exclusion()).map(|c| c.relation).collect();
        if tables.is_empty() {
            return Ok(());
        }
        tables.sort_unstable();
        tables.dedup();
        for table in tables {
            for exclusion in rupg_analyze::exclusions(&catalog, table)? {
                match exclusion.deferrable {
                    Some(mode)
                        if self.modes.borrow().state(mode.constraint, true, mode.deferred) =>
                    {
                        let mut queue = self.deferred_exclusions.borrow_mut();
                        if !queue.contains(&(table, exclusion.constraint)) {
                            queue.push((table, exclusion.constraint));
                        }
                    }
                    Some(_) => {
                        self.check_exclusion(table, &exclusion, Some(command), Pass::Recheck)?
                    }
                    None => self.check_exclusion(table, &exclusion, Some(command), Pass::Insert)?,
                }
            }
        }
        Ok(())
    }

    /// The deferred exclusion constraints that are no longer deferred: the new versions of the transaction are checked with the live versions. The other constraints stay in the queue.
    ///
    /// # Errors
    ///
    /// A conflict, and the errors of a wait and of the operators.
    pub(super) fn fire_deferred_exclusions(&self) -> Result<(), Error> {
        let queue = std::mem::take(&mut *self.deferred_exclusions.borrow_mut());
        if queue.is_empty() {
            return Ok(());
        }
        let catalog = self.catalog();
        let mut now = Vec::new();
        {
            let modes = self.modes.borrow();
            let mut kept = self.deferred_exclusions.borrow_mut();
            for (table, constraint) in queue {
                let deferred = catalog
                    .constraint(constraint)
                    .is_some_and(|c| modes.state(c.oid, true, c.deferred));
                if deferred {
                    kept.push((table, constraint));
                } else {
                    now.push((table, constraint));
                }
            }
        }
        for (table, constraint) in now {
            let list = rupg_analyze::exclusions(&catalog, table)?;
            if let Some(exclusion) = list.iter().find(|e| e.constraint == constraint) {
                self.check_exclusion(table, exclusion, Some(0), Pass::Recheck)?;
            }
        }
        Ok(())
    }

    /// `IndexCheckExclusion`: the rows of the table `table` for the new exclusion constraints of the catalog `work`, which `old` does not have.
    ///
    /// # Errors
    ///
    /// `could not create exclusion constraint` for two rows that conflict, and the errors of a wait and of the operators.
    pub(super) fn check_exclusion_build(
        &self,
        old: &Catalog,
        table: u32,
        view: &View,
        work: &Arc<Catalog>,
    ) -> Result<(), Error> {
        let list = rupg_analyze::exclusions(work, table)?;
        for exclusion in list.iter().filter(|e| old.constraint(e.constraint).is_none()) {
            self.check_exclusion_rows(table, exclusion, (view.xid, None), Pass::Build, work)?;
        }
        Ok(())
    }

    /// The check of one exclusion constraint for the versions that the commands of the transaction from `command` on made.
    fn check_exclusion(
        &self,
        table: u32,
        exclusion: &Exclusion,
        command: Option<u32>,
        pass: Pass,
    ) -> Result<(), Error> {
        let xid = self.xid.get();
        if xid == 0 {
            return Ok(());
        }
        self.check_exclusion_rows(table, exclusion, (xid, command), pass, &self.catalog())
    }

    /// The pairs of versions that [`Connection::check_exclusion`] compares. The first conflict is an error. A conflict with a version of a transaction in progress waits for the end of the transaction, and the check starts again.
    fn check_exclusion_rows(
        &self,
        table: u32,
        exclusion: &Exclusion,
        (xid, command): (u32, Option<u32>),
        pass: Pass,
        catalog: &Arc<Catalog>,
    ) -> Result<(), Error> {
        let heap = self.store.heap();
        let reader = self.reader_without_view(Arc::clone(catalog));
        'again: loop {
            let rows = heap.exclusion_rows(self.database_oid, table, xid, command);
            if rows.is_empty() {
                return Ok(());
            }
            let keys = rows
                .iter()
                .map(|row| rupg_exec::index_key(&exclusion.keys, &row.values, &reader))
                .collect::<Result<Vec<_>, Error>>()?;
            let mut checked: Vec<usize> = (0..rows.len())
                .filter(|&i| match pass {
                    Pass::Insert => rows[i].new,
                    Pass::Recheck => rows[i].new && rows[i].live,
                    Pass::Build => rows[i].live || rows[i].holder.is_some(),
                })
                .collect();
            if pass == Pass::Insert {
                checked.sort_by_key(|&i| rows[i].made);
            }
            for n in checked {
                check_not_empty(exclusion, &rows[n].values, table, catalog)?;
                let Some(new) = &keys[n] else { continue };
                for (i, other) in rows.iter().enumerate() {
                    let Some(key) = &keys[i] else { continue };
                    if i == n || !seen(pass, &rows[n], other) {
                        continue;
                    }
                    if !rupg_exec::exclusion_conflict(exclusion, new, key, &reader)? {
                        continue;
                    }
                    if let Some(holder) = other.holder {
                        let context = format!(
                            "while checking exclusion constraint on tuple {} in relation \"{}\"",
                            tid_text(rows[n].tid),
                            catalog.relation(table).map(|r| r.name.as_str()).unwrap_or_default()
                        );
                        self.reader().wait_xact(xid, holder, &context)?;
                        continue 'again;
                    }
                    return Err(self.exclusion_error(table, exclusion, (new, key), pass, catalog));
                }
            }
            return Ok(());
        }
    }

    /// The error of `check_exclusion_or_unique_constraint` for the key `new` of a version that conflicts with the key `old` of another version.
    fn exclusion_error(
        &self,
        table: u32,
        exclusion: &Exclusion,
        (new, old): (&[Value], &[Value]),
        pass: Pass,
        catalog: &Arc<Catalog>,
    ) -> Error {
        let reader = self.reader_on(Arc::clone(catalog));
        let types: Vec<u32> = exclusion.keys.exprs.iter().map(|e| e.ty).collect();
        let shown = rupg_func::index_key_columns(exclusion.index, &reader).and_then(|columns| {
            Ok((columns, self.index_key_text(&types, new)?, self.index_key_text(&types, old)?))
        });
        let (columns, new, old) = match shown {
            Ok(shown) => shown,
            Err(error) => return error,
        };
        let (message, existing) = if pass == Pass::Build {
            ("could not create exclusion constraint", "")
        } else {
            ("conflicting key value violates exclusion constraint", "existing ")
        };
        let mut error =
            Error::new(SqlState::EXCLUSION_VIOLATION, format!("{message} \"{}\"", exclusion.name));
        let attrs: Vec<i16> = match catalog.relation(exclusion.index).and_then(|r| r.index.as_ref())
        {
            Some(index) => {
                let count = usize::try_from(index.key_count).unwrap_or(0);
                index.keys.iter().take(count).copied().collect()
            }
            None => Vec::new(),
        };
        // `BuildIndexValueDescription` gives no key when the role may not see it.
        error = match rupg_func::key_shown(table, &attrs, &reader) {
            Ok(true) => error.with_detail(format!(
                "Key ({columns})=({new}) conflicts with {existing}key ({columns})=({old})."
            )),
            Ok(false) => error.with_detail(format!("Key conflicts with {existing}key.")),
            Err(error) => return error,
        };
        rupg_func::with_table_constraint(error, table, &exclusion.name, &reader)
    }
}

/// `ExecWithoutOverlapsNotEmpty`: the column of `WITHOUT OVERLAPS` of the row `row` must not be an empty range or an empty multirange. A null passes.
fn check_not_empty(
    exclusion: &Exclusion,
    row: &[Value],
    table: u32,
    catalog: &Catalog,
) -> Result<(), Error> {
    let Some((attnum, column)) = &exclusion.period else { return Ok(()) };
    let value = usize::try_from(attnum - 1).ok().and_then(|at| row.get(at));
    let empty = match value {
        Some(Value::Range(range)) => range.is_empty(),
        Some(Value::Multirange(ranges)) => ranges.is_empty(),
        _ => false,
    };
    if !empty {
        return Ok(());
    }
    let relation = catalog.relation(table).map(|r| r.name.as_str()).unwrap_or_default();
    Err(Error::new(
        SqlState::CHECK_VIOLATION,
        format!(
            "empty WITHOUT OVERLAPS value found in column \"{column}\" in relation \"{relation}\""
        ),
    ))
}

/// True when the check of the new version `new` sees the version `other`. At the insert of `new`, a version of the statement that came after it is not there yet, and an old version that the statement updated after it is still live.
fn seen(pass: Pass, new: &Candidate, other: &Candidate) -> bool {
    let live = other.live || other.holder.is_some();
    match pass {
        Pass::Insert => {
            (!other.new || other.made < new.made) && (live || other.replaced > new.made)
        }
        Pass::Recheck | Pass::Build => live,
    }
}

impl Connection {
    /// The exclusion constraints that are arbiters of `DO NOTHING` on the table `table`: the constraint of `ON CONSTRAINT`, or each one for a clause with no inference. As `ExecCheckIndexConstraints` checks it, an arbiter must not be deferrable.
    ///
    /// # Errors
    ///
    /// `55000` for a deferrable arbiter.
    pub(super) fn exclusion_arbiters(
        &self,
        table: u32,
        conflict: &OnConflict,
    ) -> Result<Vec<Exclusion>, Error> {
        if !conflict.all && conflict.exclusion == 0 {
            return Ok(Vec::new());
        }
        let catalog = self.catalog();
        let mut list = rupg_analyze::exclusions(&catalog, table)?;
        list.retain(|e| conflict.all || e.index == conflict.exclusion);
        if let Some(arbiter) = list.iter().find(|e| e.deferrable.is_some()) {
            let error = Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters",
            );
            return Err(rupg_func::with_table_constraint(
                error,
                table,
                &arbiter.name,
                &self.reader(),
            ));
        }
        Ok(list)
    }

    /// `ExecCheckIndexConstraints` for the exclusion arbiters `exclusions`: true when a live version conflicts with the new row, which `DO NOTHING` then skips. A version of a transaction in progress waits for the end of the transaction.
    ///
    /// # Errors
    ///
    /// The errors of a wait and of the operators.
    pub(super) fn excluded_row(
        &self,
        info: &Write,
        exclusions: &[Exclusion],
        view: &View,
        row: &[Value],
    ) -> Result<bool, Error> {
        if exclusions.is_empty() {
            return Ok(false);
        }
        let heap = self.store.heap();
        let reader = self.reader_without_view(self.catalog());
        for exclusion in exclusions {
            check_not_empty(exclusion, row, info.table, &self.catalog())?;
            let Some(new) = rupg_exec::index_key(&exclusion.keys, row, &reader)? else {
                continue;
            };
            'again: loop {
                for other in heap.exclusion_rows(self.database_oid, info.table, view.xid, None) {
                    if !other.live && other.holder.is_none() {
                        continue;
                    }
                    let Some(key) = rupg_exec::index_key(&exclusion.keys, &other.values, &reader)?
                    else {
                        continue;
                    };
                    if !rupg_exec::exclusion_conflict(exclusion, &new, &key, &reader)? {
                        continue;
                    }
                    let Some(holder) = other.holder else { return Ok(true) };
                    let context = format!(
                        "while checking exclusion constraint on tuple {} in relation \"{}\"",
                        tid_text(other.tid),
                        info.name
                    );
                    self.reader().wait_xact(view.xid, holder, &context)?;
                    continue 'again;
                }
                break;
            }
        }
        Ok(false)
    }
}

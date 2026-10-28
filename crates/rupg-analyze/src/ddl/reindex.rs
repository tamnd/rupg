//! `REINDEX`, as `ExecReindex`, `ReindexIndex`, `ReindexTable`, `ReindexPartitions`, `ReindexMultipleTables`, `ReindexMultipleInternal` and `ReindexRelationConcurrently` of `indexcmds.c` and `reindex_relation` and `reindex_index` of `index.c` run it. rupg builds the entries of an index from the rows of its table, so `REINDEX` gives each index that it builds again a new file, `relfilenode`, and makes the index of a failed `CREATE INDEX CONCURRENTLY` valid and ready. `REINDEX CONCURRENTLY` keeps the OID of an index, where PostgreSQL gives the new index a new OID and swaps the names. An index of the built-in catalog keeps its file, and `VERBOSE` shows no message for it.
//!
//! The session runs the parts of `ExecReindex` and `ReindexMultipleTables` that read the state of the session before [`define`](super::define) runs the statement: [`reindex_options`], `PreventInTransactionBlock` of `CONCURRENTLY`, `SCHEMA`, `SYSTEM` and `DATABASE`, the tablespace of `TABLESPACE`, and the checks of the database of `SYSTEM` and `DATABASE`.

use rupg_catalog::{ConKind, FIRST_UNPINNED_OID, RelKind};
use rupg_common::{Error, Level, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{Node, RangeVar, ReindexObjectType, ReindexStmt};

use super::reloptions::def_boolean;
use super::view::def_string;
use super::{Definer, Found, Message, TopLevel, relname};
use crate::coerce::AtOpt;
use crate::typename::place;

/// The detail of `pg_rusage_show` in a message of `VERBOSE`. rupg does not measure the time of the statement.
const RUSAGE: &str = "CPU: user: 0.00 s, system: 0.00 s, elapsed: 0.00 s";

/// The options of `REINDEX`, as `ExecReindex` reads them from the option list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReindexOptions {
    /// `VERBOSE`.
    pub verbose: bool,
    /// `CONCURRENTLY`.
    pub concurrently: bool,
    /// The name of the tablespace of `TABLESPACE`.
    pub tablespace: Option<String>,
}

/// The option list of `ExecReindex`.
///
/// # Errors
///
/// `42601` for an option that `REINDEX` does not have, at the option, and for a value that is not a Boolean value.
pub fn reindex_options(stmt: &ReindexStmt) -> Result<ReindexOptions> {
    let mut options = ReindexOptions::default();
    for node in stmt.params.iter().flatten() {
        let Node::DefElem(def) = node else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        match name {
            "verbose" => options.verbose = def_boolean(name, def.arg.as_ref())?,
            "concurrently" => options.concurrently = def_boolean(name, def.arg.as_ref())?,
            "tablespace" => options.tablespace = Some(def_string(def.arg.as_ref())),
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unrecognized REINDEX option \"{name}\""),
                )
                .at_opt(place(def.location)));
            }
        }
    }
    Ok(options)
}

/// The error of `ReindexRelationConcurrently` and `ReindexMultipleTables` for a catalog.
fn system_concurrently() -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot reindex system catalogs concurrently")
}

/// `IsCatalogRelationOid`: true for a relation of `initdb` with an OID below `FirstUnpinnedObjectId`.
fn is_catalog(oid: u32) -> bool {
    oid < FIRST_UNPINNED_OID
}

/// An `INFO` of `VERBOSE`, with the time of `pg_rusage_show` as the detail when `detail` is not `None`.
fn info(message: String, detail: Option<String>) -> Message {
    let error = Error::new(SqlState::SUCCESSFUL_COMPLETION, message).with_level(Level::Info);
    Message::Notice(match detail {
        Some(detail) => error.with_detail(detail),
        None => error,
    })
}

impl Definer<'_, '_> {
    /// `ExecReindex` after the checks of the session: `ReindexIndex`, `ReindexTable` or `ReindexMultipleTables`.
    pub(super) fn reindex(&mut self, stmt: &ReindexStmt) -> Result<()> {
        let options = reindex_options(stmt)?;
        let relation = stmt.relation.as_deref();
        match (stmt.kind, relation) {
            (ReindexObjectType::REINDEX_OBJECT_INDEX, Some(rv)) => {
                self.reindex_index_of(rv, &options)
            }
            (ReindexObjectType::REINDEX_OBJECT_TABLE, Some(rv)) => {
                self.reindex_table_of(rv, &options)
            }
            (
                ReindexObjectType::REINDEX_OBJECT_INDEX | ReindexObjectType::REINDEX_OBJECT_TABLE,
                None,
            ) => Err(Error::internal("REINDEX with no relation")),
            (kind, _) => self.reindex_multiple(kind, stmt.name.as_deref(), &options),
        }
    }

    /// `ReindexIndex`: the checks of `RangeVarCallbackForReindexIndex`, then `ReindexPartitions` for a partitioned index, `ReindexRelationConcurrently` for `CONCURRENTLY` on an index that is not temporary, or `reindex_index`.
    fn reindex_index_of(&mut self, rv: &RangeVar, options: &ReindexOptions) -> Result<()> {
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        let kind = self.kind_of(found);
        if !matches!(kind, b'i' | b'I') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index"),
            ));
        }
        let index = match found {
            Found::User(oid) => oid,
            Found::Builtin(row) => row.oid,
        };
        let table = self.index_table(index).unwrap_or(index);
        if !self.may_maintain(table) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for index {name}"),
            ));
        }
        if let Found::Builtin(row) = found {
            // The tables of the built-in indexes are catalogs.
            if options.concurrently && is_catalog(row.oid) {
                return Err(system_concurrently());
            }
            return Ok(());
        }
        if kind == b'I' {
            let context = format!(
                "while reindexing partitioned index \"{}\"",
                self.qualified_relation(index)
            );
            self.top_level = Some(TopLevel { statement: "REINDEX INDEX", context });
            let leaves = self.leaf_indexes(index);
            self.reindex_each(&leaves, options);
            return Ok(());
        }
        if options.concurrently && !self.is_temp(index) {
            if self.is_exclusion(index) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "concurrent index creation for exclusion constraints is not supported",
                ));
            }
            self.reindex_concurrently(&[index], options.verbose, true);
        } else {
            self.reindex_one(index, options.verbose);
        }
        Ok(())
    }

    /// `ReindexTable`: the checks of `RangeVarCallbackMaintainsTable`, then `ReindexPartitions` for a partitioned table, `ReindexRelationConcurrently` for `CONCURRENTLY` on a table that is not temporary, or `reindex_relation`, with a notice when the table has no index.
    fn reindex_table_of(&mut self, rv: &RangeVar, options: &ReindexOptions) -> Result<()> {
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        let kind = self.kind_of(found);
        if !matches!(kind, b'r' | b't' | b'm' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a table or materialized view"),
            ));
        }
        let table = match found {
            Found::User(oid) => oid,
            Found::Builtin(row) => row.oid,
        };
        if !self.may_maintain(table) {
            let shown = if kind == b'm' { "materialized view" } else { "table" };
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {shown} {name}"),
            ));
        }
        if let Found::Builtin(row) = found {
            if options.concurrently && is_catalog(row.oid) {
                return Err(system_concurrently());
            }
            return Ok(());
        }
        if kind == b'p' {
            let context = format!(
                "while reindexing partitioned table \"{}\"",
                self.qualified_relation(table)
            );
            self.top_level = Some(TopLevel { statement: "REINDEX TABLE", context });
            let leaves: Vec<u32> = self
                .catalog
                .all_inheritors(table)
                .into_iter()
                .filter(|&oid| {
                    self.catalog
                        .relation(oid)
                        .is_some_and(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
                })
                .collect();
            self.reindex_each(&leaves, options);
            return Ok(());
        }
        if options.concurrently && !self.is_temp(table) {
            let indexes = self.concurrent_indexes(table);
            if indexes.is_empty() {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("table \"{name}\" has no indexes that can be reindexed concurrently"),
                );
            } else {
                self.reindex_concurrently(&indexes, options.verbose, false);
                self.table_reindexed(table, options.verbose, true);
            }
        } else if !self.reindex_relation(table, options.verbose) {
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("table \"{name}\" has no indexes to reindex"),
            );
        }
        Ok(())
    }

    /// `ReindexMultipleTables` after the checks of the database: the owner of the schema for `SCHEMA`, then each table and materialized view of the schema, of the catalogs for `SYSTEM`, or of the other relations for `DATABASE`. The temporary tables of the other sessions stay as they are, and `CONCURRENTLY` skips the catalogs with one warning.
    fn reindex_multiple(
        &mut self,
        kind: ReindexObjectType,
        name: Option<&str>,
        options: &ReindexOptions,
    ) -> Result<()> {
        let schema = match (kind, name) {
            (ReindexObjectType::REINDEX_OBJECT_SCHEMA, Some(name)) => {
                let namespace = self.an.schema(name).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{name}\" does not exist"),
                    )
                })?;
                let roles = rupg_pgcatalog::roles::current();
                let owner = self.schema_owner(namespace).unwrap_or_default();
                if !roles.has_privs_of_role(self.user, owner) && !self.has_pg_maintain() {
                    return Err(Error::new(
                        SqlState::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of schema {name}"),
                    ));
                }
                Some(namespace)
            }
            _ => None,
        };
        let system = kind == ReindexObjectType::REINDEX_OBJECT_SYSTEM;
        let wanted = |oid: u32, namespace: u32| match schema {
            Some(schema) => namespace == schema,
            None => is_catalog(oid) == system,
        };
        let catalogs = builtin::classes().iter().any(|r| {
            matches!(r.kind, b'r' | b'm') && is_catalog(r.oid) && wanted(r.oid, r.namespace)
        });
        if options.concurrently && catalogs {
            self.warning(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot reindex system catalogs concurrently, skipping all".to_string(),
            );
        }
        let tables: Vec<u32> = self
            .catalog
            .relations()
            .filter(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
            .filter(|r| wanted(r.oid, r.namespace) && !self.other_temp(r.namespace))
            .map(|r| r.oid)
            .collect();
        self.reindex_each(&tables, options);
        Ok(())
    }

    /// `ReindexMultipleInternal`: `ReindexRelationConcurrently` for `CONCURRENTLY` on a relation that is not temporary, `reindex_index` for an index, and `reindex_relation` for a table, with the message of `VERBOSE` for each table that has an index.
    fn reindex_each(&mut self, relations: &[u32], options: &ReindexOptions) {
        for &oid in relations {
            let index = self.catalog.relation(oid).is_some_and(|r| r.kind == RelKind::Index);
            if options.concurrently && !self.is_temp(oid) {
                let indexes = if index { vec![oid] } else { self.concurrent_indexes(oid) };
                if !indexes.is_empty() {
                    self.reindex_concurrently(&indexes, options.verbose, index);
                    if !index {
                        self.table_reindexed(oid, options.verbose, true);
                    }
                }
            } else if index {
                self.reindex_one(oid, options.verbose);
            } else if self.reindex_relation(oid, options.verbose) {
                self.table_reindexed(oid, options.verbose, false);
            }
        }
    }

    /// `reindex_relation` with `REINDEX_REL_PROCESS_TOAST`: the indexes of the TOAST table first, then each index of the table in the order of their OIDs. It gives false when neither has an index.
    fn reindex_relation(&mut self, table: u32, verbose: bool) -> bool {
        let indexes = self.indexes_on(table);
        let mut result = !indexes.is_empty();
        let toast = self.catalog.relation(table).map_or(0, |r| r.toast);
        if toast != 0 {
            result |= self.reindex_relation(toast, verbose);
        }
        for index in indexes {
            self.reindex_one(index, verbose);
        }
        result
    }

    /// `reindex_index`: the index gets a new file.
    fn reindex_one(&mut self, index: u32, verbose: bool) {
        if self.rebuild(index).is_err() {
            return;
        }
        if verbose {
            let name = self.catalog.relation(index).map(|r| r.name.clone()).unwrap_or_default();
            self.flush();
            self.messages
                .push(info(format!("index \"{name}\" was reindexed"), Some(RUSAGE.to_string())));
        }
    }

    /// The indexes of `ReindexRelationConcurrently` for a table: its valid indexes, then the valid indexes of its TOAST table, with a warning for each invalid index.
    fn concurrent_indexes(&mut self, table: u32) -> Vec<u32> {
        let toast = self.catalog.relation(table).map_or(0, |r| r.toast);
        let mut out = Vec::new();
        for relation in [table, toast].into_iter().filter(|&oid| oid != 0) {
            for index in self.indexes_on(relation) {
                let valid = self
                    .catalog
                    .relation(index)
                    .and_then(|r| r.index.as_ref())
                    .is_some_and(|i| i.valid);
                let shown = self.qualified_relation(index);
                if valid && self.is_exclusion(index) {
                    self.flush();
                    self.messages.push(Message::Warning(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot reindex exclusion constraint index \"{shown}\" concurrently, skipping"
                        ),
                    )));
                    continue;
                }
                if valid {
                    out.push(index);
                    continue;
                }
                self.flush();
                self.messages.push(Message::Warning(
                    Error::new(
                        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("skipping reindex of invalid index \"{shown}\""),
                    )
                    .with_hint("Use DROP INDEX or REINDEX INDEX."),
                ));
            }
        }
        out
    }

    /// `indisexclusion`: the index is the index of an exclusion constraint.
    fn is_exclusion(&self, index: u32) -> bool {
        let table = self.index_table(index).unwrap_or_default();
        self.catalog.constraints_of(table).any(|c| c.index == index && c.kind == ConKind::Exclusion)
    }

    /// `ReindexRelationConcurrently` on the indexes: each one gets a new file. The messages of `VERBOSE` name each index with its schema. `alone` is true when the statement runs on the index itself, not on its table, and then the message of the index has the time. The caller of a table gives the time in the message of the table.
    fn reindex_concurrently(&mut self, indexes: &[u32], verbose: bool, alone: bool) {
        for &index in indexes {
            if self.rebuild(index).is_err() {
                continue;
            }
            if verbose {
                let shown = self.qualified_relation(index);
                self.flush();
                let detail = alone.then(|| format!("{RUSAGE}."));
                self.messages.push(info(format!("index \"{shown}\" was reindexed"), detail));
            }
        }
    }

    /// The new file of an index that the statement builds again. An index of a failed `CREATE INDEX CONCURRENTLY` becomes valid and ready, as `reindex_index` marks it, and the caller checks the keys of the rows of its table.
    pub(super) fn rebuild(&mut self, index: u32) -> Result<()> {
        self.catalog.set_new_file(index)?;
        self.catalog.set_index_valid(index, true)?;
        self.catalog.set_index_ready(index, true)
    }

    /// The message of `VERBOSE` after a table: `ReindexMultipleInternal` names the table, and `ReindexRelationConcurrently` adds the time.
    fn table_reindexed(&mut self, table: u32, verbose: bool, concurrently: bool) {
        if verbose {
            let shown = self.qualified_relation(table);
            self.flush();
            let detail = concurrently.then(|| format!("{RUSAGE}."));
            self.messages.push(info(format!("table \"{shown}\" was reindexed"), detail));
        }
    }

    /// `RelationGetIndexList`: the indexes of a table in the order of their OIDs.
    pub(super) fn indexes_on(&self, table: u32) -> Vec<u32> {
        self.catalog
            .relations()
            .filter(|r| r.kind == RelKind::Index)
            .filter(|r| r.index.as_ref().is_some_and(|i| i.table == table))
            .map(|r| r.oid)
            .collect()
    }

    /// `find_all_inheritors` of a partitioned index, with only the indexes that have storage.
    pub(super) fn leaf_indexes(&self, index: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut parents = vec![index];
        while let Some(parent) = parents.pop() {
            for child in self.catalog.relations() {
                if child.index.as_ref().is_some_and(|i| i.parent == parent) {
                    match child.kind {
                        RelKind::PartitionedIndex => parents.push(child.oid),
                        _ => out.push(child.oid),
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// `IndexGetRelation`: the table of an index of the catalog.
    pub(super) fn index_table(&self, index: u32) -> Option<u32> {
        self.catalog.relation(index).and_then(|r| r.index.as_ref()).map(|i| i.table)
    }

    /// True for a relation in a temporary schema, whose `relpersistence` is `t`.
    fn is_temp(&self, oid: u32) -> bool {
        self.catalog.relation(oid).is_some_and(|r| self.catalog.is_temp_schema(r.namespace))
    }

    /// True for a temporary schema of another session, as `isTempNamespace` is false for it.
    pub(super) fn other_temp(&self, namespace: u32) -> bool {
        if !self.catalog.is_temp_schema(namespace) {
            return false;
        }
        let own = self.an.env.temp_number();
        let name = self.schema_name(namespace);
        own.is_none_or(|n| name != format!("pg_temp_{n}") && name != format!("pg_toast_temp_{n}"))
    }

    /// The name of a relation of the catalog with the name of its schema.
    pub(super) fn qualified_relation(&self, oid: u32) -> String {
        match self.catalog.relation(oid) {
            Some(rel) => format!("{}.{}", self.schema_name(rel.namespace), rel.name),
            None => String::new(),
        }
    }
}

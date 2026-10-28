//! `CLUSTER ON`, `SET WITHOUT CLUSTER` and `REPLICA IDENTITY` of `ALTER TABLE`, as `ATExecClusterOn`, `ATExecDropCluster` and `ATExecReplicaIdentity` run them, and the statements `CLUSTER` and `REPACK`, as `ExecRepack`, `process_single_relation`, `get_tables_to_repack`, `get_tables_to_repack_partitioned` and `cluster_rel` of `repack.c` run them. The statement gives each table a new file, `relfilenode`, and the caller writes the rows again: in the order of the keys of a `btree` index, as the sort of `copy_table_data` gives them, or in their physical order with no index and with a `gist` index. rupg runs `REPACK (CONCURRENTLY)` not yet, and it keeps the files of the built-in catalogs.

use rupg_catalog::{ConKind, IndexInfo, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    Node, REPLICA_IDENTITY_INDEX, RangeVar, RepackCommand, RepackStmt, ReplicaIdentityStmt,
};

use super::alter::Target;
use super::reloptions::def_boolean;
use super::{Definer, Found, TopLevel, not_yet, relname};
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, Var};
use crate::insert::Derived;
use crate::sort::SortGroup;
use crate::typename::place;

/// The OIDs of the access methods with `amclusterable`: `btree` and `gist`.
const CLUSTERABLE: &[u32] = &[403, 783];

/// `BTREE_AM_OID`.
const BTREE: u32 = 403;

/// A table to rewrite and the index of its order, as a `RelToCluster` of `repack.c`.
type ToRepack = (u32, Option<u32>);

/// A table that `CLUSTER` or `REPACK` writes again, as `rebuild_relation` copies its rows to a new file.
#[derive(Clone, Debug)]
pub struct Repack {
    /// The OID of the table.
    pub table: u32,
    /// The order of the rows, or `None` for their physical order.
    pub order: Option<Box<Order>>,
    /// The name of the table with the name of its schema, for the messages of `VERBOSE`.
    pub shown: String,
    /// The first message of `VERBOSE`, which tells how `copy_table_data` reads the rows, or `None` with no `VERBOSE`.
    pub verbose: Option<String>,
    /// The columns of `ANALYZE` after the rewrite, with no columns for all of them, or `None` with no `ANALYZE`.
    pub analyze: Option<Vec<String>>,
}

/// The order of the rows of a [`Repack`] on a `btree` index: the key of each row and the sort of the keys, as `tuplesort_begin_cluster` takes them from the index.
#[derive(Clone, Debug)]
pub struct Order {
    /// The expression of each key column of the index, with the columns of the table as columns of the relation 0.
    pub keys: Derived,
    /// The sort of each key column, with the number of the key column in `target`.
    pub sort: Vec<SortGroup>,
}

/// The options of `ExecRepack`.
#[derive(Clone, Copy, Debug, Default)]
struct Options {
    verbose: bool,
    analyze: bool,
    concurrently: bool,
}

/// `RepackCommandAsString`.
fn command_name(command: RepackCommand) -> &'static str {
    match command {
        RepackCommand::REPACK_COMMAND_CLUSTER => "CLUSTER",
        RepackCommand::REPACK_COMMAND_REPACK => "REPACK",
        _ => "VACUUM",
    }
}

/// The option list of `ExecRepack`. `ANALYZE` and `CONCURRENTLY` are options of `REPACK` only.
fn repack_options(stmt: &RepackStmt, command: &str) -> Result<Options> {
    let mut options = Options::default();
    let repack = stmt.command == RepackCommand::REPACK_COMMAND_REPACK;
    let only_repack = |what: &str| {
        Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{what} option not supported for {command}"),
        )
    };
    for node in stmt.params.iter().flatten() {
        let Node::DefElem(def) = node else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        match name {
            "verbose" => options.verbose = def_boolean(name, def.arg.as_ref())?,
            "analyze" | "analyse" => {
                if !repack {
                    return Err(only_repack("ANALYZE"));
                }
                options.analyze = def_boolean(name, def.arg.as_ref())?;
            }
            "concurrently" => {
                if !repack {
                    return Err(only_repack("CONCURRENTLY"));
                }
                options.concurrently = def_boolean(name, def.arg.as_ref())?;
            }
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unrecognized {command} option \"{name}\""),
                )
                .at_opt(place(def.location)));
            }
        }
    }
    Ok(options)
}

/// The error of `ExecRepack` for `ANALYZE` on more than one table.
fn analyze_multiple() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "cannot execute REPACK (ANALYZE) on multiple tables",
    )
}

/// The error of `determine_clustered_index` for a table with no index that has `indisclustered`.
fn no_clustered(state: SqlState, table: &str) -> Error {
    Error::new(state, format!("there is no previously clustered index for table \"{table}\""))
}

impl Definer<'_, '_> {
    /// The index with this name in the schema of the table, as `get_relname_relid` and `index_open` find it, with the check that it is an index of the table. `index_open` accepts the index of a partitioned table too.
    fn table_index(&self, target: &Target, name: &str) -> Result<(u32, &IndexInfo)> {
        let not_index =
            || Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index"));
        let not_for_table = || {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index for table \"{}\"", target.name),
            )
        };
        match self.relation_in(target.namespace, name) {
            None => Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("index \"{name}\" for table \"{}\" does not exist", target.name),
            )),
            Some(Found::Builtin(row)) => {
                Err(if row.kind == b'i' { not_for_table() } else { not_index() })
            }
            Some(Found::User(oid)) => {
                let rel = self
                    .catalog
                    .relation(oid)
                    .filter(|r| matches!(r.kind, RelKind::Index | RelKind::PartitionedIndex));
                let info = rel.and_then(|r| r.index.as_ref()).ok_or_else(not_index)?;
                if info.table == target.oid { Ok((oid, info)) } else { Err(not_for_table()) }
            }
        }
    }

    /// `ATExecClusterOn`: the checks of `check_index_is_clusterable`, and then `mark_index_clustered` on the index.
    pub(super) fn cluster_on(&mut self, target: &Target, name: &str) -> Result<()> {
        let (index, info) = self.table_index(target, name)?;
        check_clusterable(name, info)?;
        self.catalog.set_clustered(target.oid, Some(index));
        Ok(())
    }

    /// `ATExecDropCluster`: `mark_index_clustered` with no index clears `indisclustered` on all indexes of the table.
    pub(super) fn drop_cluster(&mut self, target: &Target) {
        self.catalog.set_clustered(target.oid, None);
    }

    /// `ATExecReplicaIdentity`: `DEFAULT`, `FULL` and `NOTHING` set `relreplident`. `USING INDEX` checks that the index is unique, immediate, with no expression and no predicate, and that each key column has a valid not-null constraint.
    pub(super) fn replica_identity(
        &mut self,
        target: &Target,
        stmt: &ReplicaIdentityStmt,
    ) -> Result<()> {
        if stmt.identity_type != REPLICA_IDENTITY_INDEX {
            return self.catalog.set_replica_identity(target.oid, stmt.identity_type, None);
        }
        let name = stmt.name.as_deref().unwrap_or_default();
        let (index, info) = self.table_index(target, name)?;
        let unsupported = |what: &str| {
            Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot use {what} index \"{name}\" as replica identity"),
            )
        };
        if !info.unique {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot use non-unique index \"{name}\" as replica identity"),
            ));
        }
        if !info.immediate {
            return Err(unsupported("non-immediate"));
        }
        if info.exprs.is_some() {
            return Err(unsupported("expression"));
        }
        if info.predicate.is_some() {
            return Err(unsupported("partial"));
        }
        let rel = self.catalog.relation(target.oid);
        let count = usize::try_from(info.key_count).unwrap_or_default();
        for &key in info.keys.iter().take(count) {
            if let Some(column) = rel.and_then(|r| r.column(key)).filter(|c| !c.not_null) {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!(
                        "index \"{name}\" cannot be used as replica identity because column \"{}\" is nullable",
                        column.name
                    ),
                ));
            }
            let invalid = self.catalog.constraints_of(target.oid).find(|c| {
                c.kind == ConKind::NotNull && c.keys.first() == Some(&key) && !c.validated
            });
            if let (Some(con), Some(column)) = (invalid, rel.and_then(|r| r.column(key))) {
                return Err(Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot use index \"{name}\" as replica identity"),
                )
                .with_detail(format!(
                    "The constraint \"{}\" on column \"{}\" is marked NOT VALID.",
                    con.name, column.name
                ))
                .with_hint(
                    "You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT.",
                ));
            }
        }
        self.catalog.set_replica_identity(target.oid, REPLICA_IDENTITY_INDEX, Some(index))
    }
}

/// The checks of `check_index_is_clusterable` after the check of the table: the access method must have `amclusterable`, and the index must have no predicate and be valid.
fn check_clusterable(name: &str, info: &IndexInfo) -> Result<()> {
    let unsupported = |message: String| Error::new(SqlState::FEATURE_NOT_SUPPORTED, message);
    if !CLUSTERABLE.contains(&info.method) {
        return Err(unsupported(format!(
            "cannot cluster on index \"{name}\" because access method does not support clustering"
        )));
    }
    if info.predicate.is_some() {
        return Err(unsupported(format!("cannot cluster on partial index \"{name}\"")));
    }
    if !info.valid {
        return Err(unsupported(format!("cannot cluster on invalid index \"{name}\"")));
    }
    Ok(())
}

/// `IsCatalogRelationOid`: true for a relation of `initdb`.
fn is_catalog(oid: u32) -> bool {
    oid < rupg_catalog::FIRST_UNPINNED_OID
}

impl Definer<'_, '_> {
    /// `ExecRepack`: the options, then `process_single_relation` for a table that is not partitioned, or the tables of `get_tables_to_repack_partitioned` or `get_tables_to_repack`, each one with `cluster_rel`. The statement on more than one table cannot run in a transaction block, as each table has its own transaction in PostgreSQL.
    pub(super) fn repack(&mut self, stmt: &RepackStmt) -> Result<()> {
        let command = command_name(stmt.command);
        let options = repack_options(stmt, command)?;
        if options.concurrently {
            return Err(not_yet("REPACK (CONCURRENTLY)", None));
        }
        if options.analyze {
            self.top_level =
                Some(TopLevel { statement: "REPACK (ANALYZE)", context: String::new() });
        }
        let list = match stmt.relation.as_deref() {
            Some(relation) => {
                let Some(rv) = relation.relation.as_deref() else {
                    return Err(Error::internal("REPACK with no relation"));
                };
                if !options.analyze && !relation.va_cols.is_empty() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "ANALYZE option must be specified when a column list is provided",
                    ));
                }
                let columns = options.analyze.then(|| {
                    relation
                        .va_cols
                        .iter()
                        .flatten()
                        .filter_map(|node| match node {
                            Node::String(s) => Some(s.to_string()),
                            _ => None,
                        })
                        .collect()
                });
                match self.repack_single(stmt, rv, command, options, columns)? {
                    Some(list) => list,
                    None => return Ok(()),
                }
            }
            None if options.analyze => return Err(analyze_multiple()),
            None => self.tables_to_repack(stmt.usingindex, command),
        };
        self.top_level = Some(TopLevel { statement: command, context: String::new() });
        for (table, index) in list {
            self.cluster_rel(command, table, index, options.verbose, None)?;
        }
        Ok(())
    }

    /// `process_single_relation`: the checks of `RangeVarCallbackMaintainsTable`, then `cluster_rel` on the index of `determine_clustered_index` for a table that is not partitioned. A partitioned table gives the leaf partitions of `get_tables_to_repack_partitioned`, with their indexes.
    fn repack_single(
        &mut self,
        stmt: &RepackStmt,
        rv: &RangeVar,
        command: &'static str,
        options: Options,
        analyze: Option<Vec<String>>,
    ) -> Result<Option<Vec<ToRepack>>> {
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        let kind = self.kind_of(found);
        if !matches!(kind, b'r' | b't' | b'm' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a table or materialized view"),
            ));
        }
        let (table, namespace) = match found {
            Found::User(oid) => (oid, self.catalog.relation(oid).map_or(0, |r| r.namespace)),
            Found::Builtin(row) => (row.oid, row.namespace),
        };
        if !self.may_maintain(table) {
            let shown = if kind == b'm' { "materialized view" } else { "table" };
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {shown} {name}"),
            ));
        }
        if self.other_temp(namespace) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot execute {command} on temporary tables of other sessions"),
            ));
        }
        let target = Target { oid: table, kind, name: name.to_string(), namespace };
        if let Found::Builtin(row) = found {
            return self.repack_catalog(stmt, &target, row).map(|()| None);
        }
        if kind == b'p' {
            if options.analyze {
                return Err(analyze_multiple());
            }
            return self.tables_of_partitioned(stmt, &target, command).map(Some);
        }
        let index = match stmt.indexname.as_deref() {
            Some(index) => {
                let (oid, info) = self.table_index(&target, index)?;
                check_clusterable(index, info)?;
                Some(oid)
            }
            None if stmt.usingindex => {
                let clustered = self.indexes_on(table).into_iter().find(|&oid| {
                    self.catalog
                        .relation(oid)
                        .and_then(|r| r.index.as_ref())
                        .is_some_and(|i| i.clustered)
                });
                let Some(oid) = clustered else {
                    return Err(no_clustered(SqlState::UNDEFINED_OBJECT, name));
                };
                Some(oid)
            }
            None => None,
        };
        self.cluster_rel(command, table, index, options.verbose, analyze)?;
        Ok(None)
    }

    /// `process_single_relation` and `cluster_rel` on a table of the built-in catalog, whose file rupg keeps: the index must be an index of the table, and `REPACK` cannot use an index of a catalog that is not its clustered index. No index of the built-in catalog has `indisclustered`.
    fn repack_catalog(
        &self,
        stmt: &RepackStmt,
        target: &Target,
        row: &builtin::ClassRow,
    ) -> Result<()> {
        let Some(index) = stmt.indexname.as_deref() else {
            if stmt.usingindex {
                return Err(no_clustered(SqlState::UNDEFINED_OBJECT, &target.name));
            }
            return Ok(());
        };
        let not_for_table = || {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{index}\" is not an index for table \"{}\"", target.name),
            )
        };
        match self.relation_in(target.namespace, index) {
            None => {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("index \"{index}\" for table \"{}\" does not exist", target.name),
                ));
            }
            Some(Found::Builtin(found)) if found.kind == b'i' => {
                if !builtin::catalog_indexes(row.oid).iter().any(|i| i.oid == found.oid) {
                    return Err(not_for_table());
                }
            }
            Some(Found::Builtin(_)) => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{index}\" is not an index"),
                ));
            }
            Some(Found::User(_)) => return Err(not_for_table()),
        }
        if row.shared {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot execute {} on a shared catalog", command_name(stmt.command)),
            ));
        }
        if stmt.command != RepackCommand::REPACK_COMMAND_CLUSTER && is_catalog(row.oid) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{}\" is a system catalog", target.name),
            )
            .with_detail("System catalogs can only be clustered by the index they're already clustered on, if any, unless \"allow_system_table_mods\" is enabled."));
        }
        Ok(())
    }

    /// `get_tables_to_repack_partitioned`: the leaf indexes of the index of `USING` with their tables, or the leaf partitions with no index. A partition whose table the user cannot maintain gets a warning, and the statement skips it.
    fn tables_of_partitioned(
        &mut self,
        stmt: &RepackStmt,
        target: &Target,
        command: &str,
    ) -> Result<Vec<ToRepack>> {
        let mut list = Vec::new();
        if stmt.usingindex {
            let Some(name) = stmt.indexname.as_deref() else {
                let state = SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE;
                if stmt.command == RepackCommand::REPACK_COMMAND_CLUSTER {
                    return Err(no_clustered(state, &target.name));
                }
                return Err(Error::new(
                    state,
                    format!(
                        "cannot execute {command} on partitioned table \"{}\" USING INDEX with no index name",
                        target.name
                    ),
                ));
            };
            let (index, info) = self.table_index(target, name)?;
            check_clusterable(name, info)?;
            for leaf in self.leaf_indexes(index) {
                let Some(table) = self.index_table(leaf) else { continue };
                if self.repack_permitted(table, command) {
                    list.push((table, Some(leaf)));
                }
            }
        } else {
            for table in self.catalog.all_inheritors(target.oid) {
                let leaf = self.catalog.relation(table).is_some_and(|r| r.kind == RelKind::Table);
                if leaf && self.repack_permitted(table, command) {
                    list.push((table, None));
                }
            }
        }
        Ok(list)
    }

    /// `get_tables_to_repack`: with `USING INDEX`, the tables of the indexes with `indisclustered`, and the tables and materialized views with no index otherwise. The temporary tables of the other sessions stay as they are, and a table that the user cannot maintain gets a warning. rupg keeps the files of the built-in catalogs.
    fn tables_to_repack(&mut self, usingindex: bool, command: &str) -> Vec<ToRepack> {
        let candidates: Vec<(u32, Option<u32>, u32)> = if usingindex {
            self.catalog
                .relations()
                .filter(|r| r.kind == RelKind::Index)
                .filter_map(|r| {
                    let info = r.index.as_ref().filter(|i| i.clustered)?;
                    let table = self.catalog.relation(info.table)?;
                    Some((info.table, Some(r.oid), table.namespace))
                })
                .collect()
        } else {
            self.catalog
                .relations()
                .filter(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
                .map(|r| (r.oid, None, r.namespace))
                .collect()
        };
        let mut list = Vec::new();
        for (table, index, namespace) in candidates {
            if !self.other_temp(namespace) && self.repack_permitted(table, command) {
                list.push((table, index));
            }
        }
        list
    }

    /// `repack_is_permitted_for_relation`: `MAINTAIN` on the table, or a warning.
    fn repack_permitted(&mut self, table: u32, command: &str) -> bool {
        if self.may_maintain(table) {
            return true;
        }
        let name = self.catalog.relation(table).map(|r| r.name.clone()).unwrap_or_default();
        self.warning(
            SqlState::WARNING,
            format!("permission denied to execute {command} on \"{name}\", skipping it"),
        );
        false
    }

    /// `cluster_rel` and `rebuild_relation` of a table of the catalog: for `REPACK`, the check of `check_index_requirements` that the table has no invalid index, then the checks of `check_index_is_clusterable`, `mark_index_clustered` and a new file for the table, its TOAST table and their indexes. A materialized view with no data stays as it is.
    fn cluster_rel(
        &mut self,
        command: &str,
        table: u32,
        index: Option<u32>,
        verbose: bool,
        analyze: Option<Vec<String>>,
    ) -> Result<()> {
        let Some(rel) = self.catalog.relation(table) else { return Ok(()) };
        let (name, kind, toast) = (rel.name.clone(), rel.kind, rel.toast);
        let populated = rel.view.as_ref().is_none_or(|v| v.populated);
        if command == "REPACK" {
            let invalid: Vec<String> = self
                .indexes_on(table)
                .into_iter()
                .filter_map(|oid| self.catalog.relation(oid))
                .filter(|r| r.index.as_ref().is_some_and(|i| !i.valid))
                .map(|r| format!("\"{}\"", r.name))
                .collect();
            if !invalid.is_empty() {
                let detail = if invalid.len() == 1 {
                    format!("An invalid index cannot be processed correctly: {}.", invalid[0])
                } else {
                    format!(
                        "Some invalid indexes cannot be processed correctly: {}.",
                        invalid.join(", ")
                    )
                };
                return Err(Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot execute {command} on relation \"{name}\""),
                )
                .with_detail(detail)
                .with_hint("Use DROP INDEX or REINDEX."));
            }
        }
        let info = match index {
            Some(oid) => {
                let found = self.catalog.relation(oid);
                let info = found.and_then(|r| r.index.as_ref()).filter(|i| i.table == table);
                let index_name = found.map(|r| r.name.clone()).unwrap_or_default();
                let Some(info) = info else {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("\"{index_name}\" is not an index for table \"{name}\""),
                    ));
                };
                check_clusterable(&index_name, info)?;
                Some((index_name, info.clone()))
            }
            None => None,
        };
        if kind == RelKind::MatView && !populated {
            return Ok(());
        }
        if let Some(oid) = index {
            self.catalog.set_clustered(table, Some(oid));
        }
        let heaps = [table, toast];
        let indexes: Vec<u32> = heaps
            .into_iter()
            .filter(|&oid| oid != 0)
            .flat_map(|oid| self.indexes_on(oid))
            .collect();
        for oid in heaps.into_iter().filter(|&oid| oid != 0) {
            self.catalog.set_new_file(oid)?;
        }
        for oid in indexes {
            self.rebuild(oid)?;
        }
        let shown = self.qualified_relation(table);
        let order = match &info {
            Some((_, info)) if info.method == BTREE => {
                Some(Box::new(self.index_order(table, info)?))
            }
            _ => None,
        };
        let verbose = verbose.then(|| match &info {
            Some((index_name, info)) if info.method != BTREE => {
                format!("repacking \"{shown}\" using index scan on \"{index_name}\"")
            }
            Some(_) => format!("repacking \"{shown}\" using sequential scan and sort"),
            None => format!("repacking \"{shown}\" in physical order"),
        });
        if kind != RelKind::Toast {
            self.repacked.push(Repack { table, order, shown, verbose, analyze });
        }
        Ok(())
    }

    /// The keys of `tuplesort_begin_cluster` for a `btree` index: the expression of each key column, and the `<` operator of the family of its operator class, or the `>` operator for `DESC`, with `NULLS FIRST` from `indoption`.
    fn index_order(&self, table: u32, info: &IndexInfo) -> Result<Order> {
        let columns =
            self.catalog.relation(table).map(|r| r.columns.as_slice()).unwrap_or_default();
        let mut exprs = match &info.exprs {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        }
        .into_iter();
        let count = usize::try_from(info.key_count).unwrap_or_default();
        let mut keys = Derived {
            table,
            types: columns.iter().map(|c| c.ty).collect(),
            exprs: Vec::with_capacity(count),
            predicate: Vec::new(),
        };
        let mut sort = Vec::with_capacity(count);
        for (at, &attnum) in info.keys.iter().take(count).enumerate() {
            let expr = if attnum == 0 {
                exprs.next().ok_or_else(|| Error::internal("too few entries in indexprs"))?
            } else {
                let column = usize::try_from(attnum - 1).ok().and_then(|c| columns.get(c));
                let Some(column) = column else {
                    return Err(Error::internal("an index key that is not a column"));
                };
                let var = Var { relation: 0, attnum, levels_up: 0 };
                Expr { typmod: column.typmod, ..Expr::new(ExprKind::Var(var), column.ty) }
            };
            let option = info.options.get(at).copied().unwrap_or_default();
            let (desc, nulls_first) = (option & 1 != 0, option & 2 != 0);
            let class = info.classes.get(at).copied().unwrap_or_default();
            let family = match builtin::opclasses().iter().find(|c| c.oid == class) {
                Some(row) => Some((row.family, row.intype)),
                None => self.catalog.opclass(class).map(|c| (c.family, c.intype)),
            };
            let member = |strategy: i16| {
                family.and_then(|(family, intype)| {
                    builtin::opfamily_member(family, intype, intype, strategy).or_else(|| {
                        self.catalog
                            .amops()
                            .find(|a| {
                                a.family == family
                                    && a.left == intype
                                    && a.right == intype
                                    && a.strategy == strategy
                            })
                            .map(|a| a.operator)
                    })
                })
            };
            let (lt, gt) = crate::typcache::sort_operators(expr.ty);
            let operator = if desc { member(5).unwrap_or(gt) } else { member(1).unwrap_or(lt) };
            let code = |op: u32| builtin::operator_by_oid(op).map_or(0, |o| o.code);
            sort.push(SortGroup {
                target: at,
                operator,
                sort: code(operator),
                equal: member(3).map_or(0, code),
                nulls_first,
                hashable: false,
                collation: 0,
            });
            keys.exprs.push(expr);
        }
        Ok(Order { keys, sort })
    }
}

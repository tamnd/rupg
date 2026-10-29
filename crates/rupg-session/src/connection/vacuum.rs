//! `VACUUM` and `ANALYZE`, as `ExecVacuum` and `vacuum` of `vacuum.c` run them: the options, the checks of the options and of the relations, and the warnings for the relations that the statement skips. `VACUUM` removes the dead row versions of each table, and the statement sets `relpages`, `reltuples`, `relallvisible` and `relallfrozen` of each table. `ANALYZE` writes the statistics of the columns to `pg_statistic`. `VACUUM FULL` runs as a plain `VACUUM`.

use rupg_analyze::{Repack, VacuumRel};
use rupg_catalog::size::{empty_index_pages, new_table_tuples};
use rupg_catalog::{Catalog, Column, RelKind, Relation};

use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{CheckPointStmt, DefElem, Node, VacuumStmt};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};

/// `ROLE_PG_CHECKPOINT`.
const PG_CHECKPOINT: u32 = 4544;
use super::{Connection, Done};
use crate::heap::{Skip, View};
use crate::{guc, query};
use rupg_types::Value;
use std::sync::Arc;

use super::{analyze, explain};
use crate::utility::Notice;

/// The OID of the access method `hash`.
const HASH: u32 = 405;

/// An index of a table with expressions, `AnlIndexData`: the key of the index for the rows of the sample and the expression columns that `ANALYZE` computes statistics for.
struct IndexTargets {
    oid: u32,
    derived: rupg_analyze::Derived,
    targets: Vec<analyze::Target>,
}

/// The indexes of the table `rel` that have expressions, as `do_analyze_rel` finds them with `vac_open_indexes`: each index that is ready, with `examine_attribute` for each of its expression columns. A column has the type of its expression and the collation of the index column, which `DefineIndex` sets for each expression of a collatable type.
fn index_targets(catalog: &Catalog, rel: &Relation, default_target: i32) -> Vec<IndexTargets> {
    let columns: Vec<(u32, i32)> = rel.columns.iter().map(|c| (c.ty, c.typmod)).collect();
    catalog
        .relations()
        .filter(|r| r.kind == RelKind::Index)
        .filter_map(|index| {
            let info = index.index.as_ref()?;
            if info.table != rel.oid || info.exprs.is_none() || !info.ready {
                return None;
            }
            let derived = rupg_analyze::index_derived(info, rel.oid, &columns).ok()?;
            let targets = info
                .keys
                .iter()
                .zip(&derived.exprs)
                .zip(&index.columns)
                .zip(1i16..)
                .filter(|(((key, _), _), _)| **key == 0)
                .filter_map(|(((_, expr), column), attnum)| {
                    let at = usize::try_from(attnum - 1).ok()?;
                    let collation = info.collations.get(at).copied().unwrap_or(0);
                    let column =
                        Column { ty: expr.ty, typmod: expr.typmod, collation, ..column.clone() };
                    analyze::examine(&column, attnum, default_target)
                })
                .collect();
            Some(IndexTargets { oid: index.oid, derived, targets })
        })
        .collect()
}

/// `hashm_ffactor` of a new `hash` index: the rows of a bucket before a split, for the fill factor 75 and the width of an item with its line pointer, as `_hash_init` finds it.
const HASH_BUCKET_ROWS: f64 = 307.0;

/// `MIN_BAS_VAC_RING_SIZE_KB` and `MAX_BAS_VAC_RING_SIZE_KB`.
const RING_SIZE_KB: (i32, i32) = (128, 16 * 1024 * 1024);

/// `MAX_PARALLEL_WORKER_LIMIT`.
const MAX_PARALLEL_WORKERS: i32 = 1024;

/// `BYPASS_THRESHOLD_PAGES`: the part of the pages of a heap with dead versions under which `VACUUM` bypasses the vacuum of the indexes.
const BYPASS_THRESHOLD_PAGES: f64 = 0.02;

/// The options of `VACUUM` and `ANALYZE` that the checks of `ExecVacuum` read.
#[derive(Debug, Default)]
struct Parsed {
    analyze: bool,
    freeze: bool,
    full: bool,
    disable_page_skipping: bool,
    process_toast: bool,
    process_main: bool,
    only_database_stats: bool,
    /// True when a flag of `params.options` other than `VERBOSE`, `PROCESS_MAIN` and `PROCESS_TOAST` is on, for the check of `ONLY_DATABASE_STATS`.
    other: bool,
    workers: i32,
    ring_size: Option<i32>,
    /// `TRUNCATE`, or `None` for the reloption `vacuum_truncate`.
    truncate: Option<bool>,
    /// `INDEX_CLEANUP`, or `None` for `AUTO` and for the reloption `vacuum_index_cleanup`.
    index_cleanup: Option<bool>,
}

/// A row of the sample of `ANALYZE`: its values, and the widths of each value that `stored_widths` gives.
type SampleRow = (Vec<Value>, Vec<(usize, usize)>);

/// `defGetInt32`.
fn def_int(def: &DefElem) -> Result<i32, Error> {
    match def.arg {
        Some(Node::Integer(n)) => Ok(n),
        _ => {
            let name = def.defname.as_deref().unwrap_or_default();
            Err(Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires an integer value")))
        }
    }
}

/// The option list of `ExecVacuum`.
fn parse_options(stmt: &VacuumStmt) -> Result<Parsed, Error> {
    let command = if stmt.is_vacuumcmd { "VACUUM" } else { "ANALYZE" };
    let mut parsed = Parsed { process_toast: true, process_main: true, ..Parsed::default() };
    for def in stmt.options.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = usize::try_from(def.location).ok();
        match name {
            "verbose" | "skip_locked" => {
                let on = def_boolean(def)?;
                parsed.other |= on && name == "skip_locked";
            }
            "buffer_usage_limit" => {
                let text = def_string(def)?;
                let limit = guc::find("vacuum_buffer_usage_limit")
                    .ok_or_else(|| Error::internal("no parameter vacuum_buffer_usage_limit"));
                let value = limit?.parse_int(&text);
                let (min, max) = RING_SIZE_KB;
                match value {
                    Ok(size) if size == 0 || (min..=max).contains(&size) => {
                        parsed.ring_size = Some(size);
                    }
                    Ok(_) | Err(_) => {
                        let error = Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!(
                                "BUFFER_USAGE_LIMIT option must be 0 or between {min} kB and {max} kB"
                            ),
                        );
                        return Err(match value {
                            Err(Some(hint)) => error.with_hint(hint),
                            _ => error,
                        });
                    }
                }
            }
            _ if !stmt.is_vacuumcmd => return Err(unrecognized(command, name, at)),
            "analyze" => parsed.analyze = def_boolean(def)?,
            "freeze" => parsed.freeze = def_boolean(def)?,
            "full" => parsed.full = def_boolean(def)?,
            "disable_page_skipping" => parsed.disable_page_skipping = def_boolean(def)?,
            "index_cleanup" => {
                parsed.index_cleanup = match def.arg {
                    None => Some(true),
                    Some(_) if def_string(def)?.eq_ignore_ascii_case("auto") => None,
                    Some(_) => Some(def_boolean(def)?),
                };
            }
            "process_main" => parsed.process_main = def_boolean(def)?,
            "process_toast" => parsed.process_toast = def_boolean(def)?,
            "truncate" => parsed.truncate = Some(def_boolean(def)?),
            "parallel" => {
                let workers = def_int(def)?;
                if !(0..=MAX_PARALLEL_WORKERS).contains(&workers) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("PARALLEL option must be between 0 and {MAX_PARALLEL_WORKERS}"),
                    )
                    .with_position(at));
                }
                parsed.workers = workers;
            }
            "skip_database_stats" => parsed.other |= def_boolean(def)?,
            "only_database_stats" => parsed.only_database_stats = def_boolean(def)?,
            _ => return Err(unrecognized(command, name, at)),
        }
    }
    parsed.other |= parsed.analyze || parsed.freeze || parsed.full || parsed.disable_page_skipping;
    Ok(parsed)
}

/// The error of an option that the command does not have.
fn unrecognized(command: &str, name: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, format!("unrecognized {command} option \"{name}\""))
        .with_position(at)
}

/// The checks of `ExecVacuum` on the options together.
fn check_options(stmt: &VacuumStmt, parsed: &Parsed) -> Result<(), Error> {
    let not_supported = |message: &str| Error::new(SqlState::FEATURE_NOT_SUPPORTED, message);
    if parsed.full && parsed.workers > 0 {
        return Err(not_supported("VACUUM FULL cannot be performed in parallel"));
    }
    if parsed.ring_size.is_some() && parsed.full && !parsed.analyze {
        return Err(not_supported("BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL"));
    }
    let analyze = parsed.analyze || !stmt.is_vacuumcmd;
    let columns = stmt.rels.iter().flatten().any(|node| match node {
        Node::VacuumRelation(vrel) => !vrel.va_cols.is_empty(),
        _ => false,
    });
    if !analyze && columns {
        return Err(not_supported(
            "ANALYZE option must be specified when a column list is provided",
        ));
    }
    if parsed.full && parsed.disable_page_skipping {
        return Err(not_supported("VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL"));
    }
    if parsed.full && !parsed.process_toast {
        return Err(not_supported("PROCESS_TOAST required with VACUUM FULL"));
    }
    if parsed.only_database_stats {
        if !stmt.rels.is_empty() {
            return Err(not_supported(
                "ONLY_DATABASE_STATS cannot be specified with a list of tables",
            ));
        }
        if parsed.other {
            return Err(not_supported(
                "ONLY_DATABASE_STATS cannot be specified with other VACUUM options",
            ));
        }
    }
    Ok(())
}

impl Connection {
    /// `ExecVacuum`, then `vacuum`: `PreventInTransactionBlock` for `VACUUM`, and the checks of [`rupg_analyze::vacuum_relations`] on each relation of the statement. `vacuum_is_permitted_for_relation` skips a relation that the role cannot maintain, with a warning.
    pub(super) fn vacuum(
        &self,
        stmt: &VacuumStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let parsed = parse_options(stmt)?;
        check_options(stmt, &parsed)?;
        let tag = if stmt.is_vacuumcmd {
            self.prevent_block("VACUUM")?;
            CommandTag::Vacuum
        } else {
            CommandTag::Analyze
        };
        let analyze = parsed.analyze || !stmt.is_vacuumcmd;
        let reader = self.reader();
        let mut warnings = Vec::new();
        let permitted = |relid| rupg_func::vacuum_permitted(relid, &reader);
        let result = rupg_analyze::vacuum_relations(
            &stmt.rels,
            &reader,
            stmt.is_vacuumcmd,
            analyze,
            &permitted,
            &mut warnings,
        );
        notices.extend(warnings.into_iter().map(Notice::warning));
        let relids = result?;
        let catalog = self.catalog();
        // `get_all_vacuum_rels` and `vacuum_rel` check each relation of the list and each partition again.
        let mut permitted_or_warn = |relid: u32| {
            if permitted(relid) {
                return true;
            }
            let name = catalog.relation(relid).map_or("", |r| r.name.as_str());
            let warning = rupg_analyze::vacuum_denied(name, stmt.is_vacuumcmd);
            notices.push(Notice::warning(warning));
            false
        };
        // `get_all_vacuum_rels`: with no list, each table, materialized view and partitioned table.
        let relids = if parsed.only_database_stats {
            Vec::new()
        } else if stmt.rels.is_empty() {
            let kinds = |r: &&Relation| {
                matches!(r.kind, RelKind::Table | RelKind::MatView | RelKind::Partitioned)
            };
            let rels: Vec<u32> = catalog.relations().filter(kinds).map(|r| r.oid).collect();
            rels.into_iter().filter(|&r| permitted_or_warn(r)).map(|r| (r, Vec::new())).collect()
        } else {
            // `expand_vacuum_rel`: with no `ONLY`, the partitions and the inheritance children of the relation come after it, with its column list.
            let mut all = Vec::new();
            for VacuumRel { relid, columns, inh, permitted } in relids {
                if inh {
                    let children = catalog.all_inheritors(relid);
                    if permitted {
                        all.push((relid, columns.clone()));
                    }
                    all.extend(
                        children
                            .into_iter()
                            .filter(|&child| permitted_or_warn(child))
                            .map(|child| (child, columns.clone())),
                    );
                } else if permitted {
                    all.push((relid, columns));
                }
            }
            all
        };
        let vacuum = stmt.is_vacuumcmd.then_some(&parsed);
        for (relid, columns) in relids {
            let analyze = analyze.then_some(columns.as_slice());
            self.relation_stats(&catalog, relid, (vacuum, analyze));
        }
        Ok(Done::Tag(tag))
    }

    /// `ExecCheckpoint`: the checks of the options and of the role. Each commit of rupg is in its log before the commit returns, so the checkpoint has no more work to do.
    pub(super) fn checkpoint(&self, stmt: &CheckPointStmt) -> Result<Done, Error> {
        for def in stmt.options.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let at = usize::try_from(def.location).ok();
            match def.defname.as_deref().unwrap_or_default() {
                "mode" => {
                    let mode = def_string(def)?;
                    if mode != "spread" && mode != "fast" {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            format!(
                                "unrecognized value for CHECKPOINT option \"mode\": \"{mode}\""
                            ),
                        )
                        .with_position(at));
                    }
                }
                "flush_unlogged" => {
                    def_boolean(def)?;
                }
                other => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized CHECKPOINT option \"{other}\""),
                    )
                    .with_position(at));
                }
            }
        }
        if !self.roles().has_privs_of_role(self.current_role(), PG_CHECKPOINT) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied to execute CHECKPOINT command",
            )
            .with_detail(
                "Only roles with privileges of the \"pg_checkpoint\" role may execute this command.",
            ));
        }
        Ok(Done::Tag(CommandTag::Checkpoint))
    }

    /// `heap_vacuum_rel` with the options `vacuum` and `do_analyze_rel` for the relation `relid`, with its TOAST table for `VACUUM`. `analyze` has the columns of the column list of `ANALYZE`, or no columns for all of them. `ANALYZE` of a partitioned table sets its `reltuples` to the live rows of its partitions and its `relpages` to -1, as the table has no storage. The result is the live rows of the relation.
    fn relation_stats(
        &self,
        catalog: &Catalog,
        relid: u32,
        (vacuum, analyze): (Option<&Parsed>, Option<&[String]>),
    ) -> f32 {
        let Some(rel) = catalog.relation(relid) else { return 0.0 };
        match rel.kind {
            RelKind::Table | RelKind::MatView | RelKind::Toast => {
                let mut rows = 0.0;
                if let Some(parsed) = vacuum {
                    if rel.toast != 0 && parsed.process_toast {
                        self.vacuum_heap(catalog, rel.toast, parsed);
                    }
                    if parsed.process_main || rel.kind == RelKind::Toast {
                        rows = self.vacuum_heap(catalog, relid, parsed);
                    }
                }
                if let Some(columns) = analyze {
                    rows = self.update_relstats(relid);
                    if rel.kind != RelKind::Toast {
                        self.report_analyze(relid, Some(rows), columns.is_empty());
                        self.analyze_rel(catalog, relid, columns, false);
                        self.analyze_rel(catalog, relid, columns, true);
                    }
                }
                rows
            }
            RelKind::Partitioned => {
                let heap = self.heap();
                let rows = catalog
                    .all_inheritors(relid)
                    .into_iter()
                    .map(|part| heap.live_count(self.database_oid, part, self.xid.get()) as f32)
                    .sum();
                if let Some(columns) = analyze {
                    self.report_analyze(relid, None, columns.is_empty());
                    self.heap().set_stats(self.database_oid, relid, -1, rows);
                    self.analyze_rel(catalog, relid, columns, true);
                }
                rows
            }
            _ => 0.0,
        }
    }

    /// `do_analyze_rel` of the table `relid` for the columns `columns`, or for each column when the list is empty: the sample of the rows of the table, or of the rows of the table and of its inheritance children for `inherit`, and the statistics of each column in `pg_statistic`. The sample of the tree takes from each table a part of the rows in proportion to its pages, as `acquire_inherited_sample_rows` does, and the values of each column of a child come from its column with the same name. A tree with no children, or with no table with rows, gets no statistics. With no column list and no `inherit`, the statement also computes the statistics of the expression columns of each index of the table, as `compute_index_stats` does, from the keys of the rows of the sample that pass the predicate of the index, for the rows of the table in that proportion.
    fn analyze_rel(&self, catalog: &Catalog, relid: u32, columns: &[String], inherit: bool) {
        let Some(rel) = catalog.relation(relid) else { return };
        let default_target = self
            .settings
            .borrow()
            .get("default_statistics_target")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(100);
        let targets: Vec<analyze::Target> = rel
            .columns
            .iter()
            .zip(1i16..)
            .filter(|(c, _)| columns.is_empty() || columns.contains(&c.name))
            .filter_map(|(c, attnum)| analyze::examine(c, attnum, default_target))
            .collect();
        let indexes = if inherit || !columns.is_empty() {
            Vec::new()
        } else {
            index_targets(catalog, rel, default_target)
        };
        let targrows =
            analyze::sample_rows(targets.iter().chain(indexes.iter().flat_map(|i| &i.targets)));
        let heap = self.heap();
        let xid = self.xid.get();
        let tables: Vec<u32> = if inherit {
            let all = catalog.all_inheritors(relid);
            if all.is_empty() {
                return;
            }
            std::iter::once(relid).chain(all).collect()
        } else {
            vec![relid]
        };
        let tables: Vec<(&Relation, f64)> = tables
            .iter()
            .filter_map(|&oid| catalog.relation(oid))
            .filter(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
            .map(|r| (r, f64::from(heap.size(self.database_oid, r.oid).blocks)))
            .collect();
        let totalblocks: f64 = tables.iter().map(|t| t.1).sum();
        // The values of each row of the sample in the order of the columns of `rel`, with their widths in the versions.
        let mut rows: Vec<SampleRow> = Vec::new();
        let mut totalrows = 0.0;
        for (table, blocks) in tables {
            let target = if inherit {
                if blocks <= 0.0 {
                    continue;
                }
                let share = (targrows as f64 * blocks / totalblocks).round_ties_even() as usize;
                share.min(targrows - rows.len())
            } else {
                targrows
            };
            if target == 0 {
                continue;
            }
            let (sample, total) = heap.sample(self.database_oid, table.oid, xid, target);
            totalrows += total;
            let map: Vec<Option<usize>> = rel
                .columns
                .iter()
                .enumerate()
                .map(|(at, c)| {
                    if table.oid == relid {
                        return Some(at);
                    }
                    table.columns.iter().position(|t| !t.dropped && !c.dropped && t.name == c.name)
                })
                .collect();
            let (fixed, storage) = heap.layout(self.database_oid, table.oid);
            for row in sample {
                let widths = crate::heap::stored_widths(&row, &fixed, &storage);
                let pick =
                    |at: Option<usize>| at.and_then(|at| Some((row.get(at)?.clone(), widths[at])));
                let (values, widths) =
                    map.iter().map(|&at| pick(at).unwrap_or((Value::Null, (0, 0)))).unzip();
                rows.push((values, widths));
            }
        }
        if rows.is_empty() {
            return;
        }
        let reader = self.reader();
        let mut stats = Vec::new();
        for target in &targets {
            let at = usize::try_from(target.attnum - 1).unwrap_or(0);
            let sample: Vec<analyze::Sampled<'_>> =
                rows.iter().map(|(values, widths)| (&values[at], widths[at])).collect();
            if let Ok(Some(stat)) = target.compute(&sample, totalrows, inherit, &reader) {
                stats.push(stat);
            }
        }
        heap.set_statistics(self.database_oid, relid, stats);
        for index in &indexes {
            let keys: Result<Vec<_>, _> = rows
                .iter()
                .filter_map(|(values, _)| {
                    rupg_exec::index_key(&index.derived, values, &reader).transpose()
                })
                .collect();
            let Ok(keys) = keys else { continue };
            if keys.is_empty() {
                continue;
            }
            let totalindexrows = (keys.len() as f64 / rows.len() as f64 * totalrows).ceil();
            let mut stats = Vec::new();
            for target in &index.targets {
                let at = usize::try_from(target.attnum - 1).unwrap_or(0);
                let sample: Vec<analyze::Sampled<'_>> = keys
                    .iter()
                    .filter_map(|key| key.get(at))
                    .map(|value| (value, crate::heap::datum_width(value)))
                    .collect();
                if let Ok(Some(stat)) = target.compute(&sample, totalindexrows, false, &reader) {
                    stats.push(stat);
                }
            }
            heap.set_statistics(self.database_oid, index.oid, stats);
        }
    }

    /// `heap_vacuum_rel` of the heap of `relid`: an aggressive `VACUUM` with `FREEZE`, which skips only the pages that are all frozen, no skipped pages with `DISABLE_PAGE_SKIPPING`, and the truncation of the empty pages at the end from `TRUNCATE` or the reloption `vacuum_truncate`. The result is `reltuples`.
    fn vacuum_heap(&self, catalog: &Catalog, relid: u32, parsed: &Parsed) -> f32 {
        let skip = if parsed.disable_page_skipping {
            Skip::None
        } else if parsed.freeze {
            Skip::Frozen
        } else {
            Skip::Visible
        };
        let truncate = parsed.truncate.unwrap_or_else(|| {
            let options = catalog.relation(relid).map(|r| r.options.as_slice()).unwrap_or_default();
            !options.iter().any(|o| {
                o.strip_prefix("vacuum_truncate=").is_some_and(|v| {
                    matches!(v.to_ascii_lowercase().as_str(), "false" | "off" | "no" | "0")
                })
            })
        });
        let indexed = catalog.relations().any(|r| {
            r.kind == RelKind::Index && r.index.as_ref().is_some_and(|i| i.table == relid)
        });
        let cleanup = parsed.index_cleanup.or_else(|| {
            let options = catalog.relation(relid).map(|r| r.options.as_slice()).unwrap_or_default();
            options.iter().find_map(|o| {
                match o.strip_prefix("vacuum_index_cleanup=")?.to_ascii_lowercase().as_str() {
                    "true" | "on" | "yes" | "1" => Some(true),
                    "false" | "off" | "no" | "0" => Some(false),
                    _ => None,
                }
            })
        });
        // The pages of each `btree` index before `VACUUM` removes the versions of its build.
        let indexes: Vec<(&Relation, i32)> = if indexed && cleanup != Some(false) {
            self.btree_pages(catalog, relid)
        } else {
            Vec::new()
        };
        let target = (self.database_oid, relid, self.pid);
        let vacuumed = self.heap().vacuum(target, skip, (indexed, parsed.freeze, truncate));
        let rows = vacuumed.reltuples;
        // `lazy_vacuum`: with `INDEX_CLEANUP AUTO`, fewer pages with dead versions than a 50th of the heap bypass the vacuum of the indexes. With no `ambulkdelete` call, `btvacuumcleanup` gives no exact count, and the indexes keep their stats.
        let threshold = (f64::from(vacuumed.pages) * BYPASS_THRESHOLD_PAGES) as u32;
        let bypass = cleanup.is_none() && vacuumed.dead_pages < threshold;
        if vacuumed.dead_pages > 0 && !bypass && cleanup != Some(false) {
            self.heap().vacuumed_indexes(self.database_oid, relid);
            self.vacuumed_index_stats(relid, &indexes);
        }
        // `VACUUM FULL` rewrites the table as `REPACK` does, which reports no `VACUUM`.
        if !parsed.full && self.counts_tracked() {
            self.store.report_vacuum(
                self.database_oid,
                relid,
                rows as i64,
                query::now(&*self.clock),
            );
        }
        rows
    }

    /// `pgstat_report_analyze` after `do_analyze_rel` of `relid`, with `rows` live rows, or `None` for a partitioned table. The rows that the transaction inserted and deleted do not count, as the end of the transaction reports them.
    fn report_analyze(&self, relid: u32, rows: Option<f32>, reset: bool) {
        if !self.counts_tracked() {
            return;
        }
        let live = rows.map(|rows| {
            let written =
                self.activity.writes().get(&relid).map_or(0, |p| p.kept.inserted - p.kept.deleted);
            rows as i64 - written
        });
        let time = query::now(&*self.clock);
        self.store.report_analyze(self.database_oid, relid, live, reset, time);
    }

    /// `index_update_stats` after a statement that built indexes, the indexes of `new` that are not in `old` or that have a new file: `relpages` and `reltuples` of the table of each index, while `autovacuum` is on and the table does not turn it off. A table with no rows that no `VACUUM` or `ANALYZE` counted keeps its `reltuples` of -1.
    pub(super) fn index_stats(&self, old: &Catalog, new: &Arc<Catalog>) {
        let built: Vec<&Relation> = new
            .relations()
            .filter(|r| r.kind == RelKind::Index && r.index.is_some())
            .filter(|r| old.relation(r.oid).is_none_or(|o| o.file != r.file))
            .collect();
        // Each index has the stats of its build, from the stats of the table before the statement.
        for index in &built {
            self.built_index_stats(new, index);
        }
        let mut tables: Vec<u32> =
            built.iter().filter_map(|r| r.index.as_ref()).map(|index| index.table).collect();
        if tables.is_empty() || self.settings.borrow().get("autovacuum").as_deref() == Some("off") {
            return;
        }
        tables.sort_unstable();
        tables.dedup();
        let heap = self.heap();
        for table in tables {
            let Some(rel) = new.relation(table) else { continue };
            let off = rel.options.iter().any(|o| {
                o.strip_prefix("autovacuum_enabled=").is_some_and(|v| {
                    matches!(v.to_ascii_lowercase().as_str(), "false" | "off" | "no" | "0")
                })
            });
            if off || !matches!(rel.kind, RelKind::Table | RelKind::MatView | RelKind::Toast) {
                continue;
            }
            let size = heap.size(self.database_oid, table);
            let live = heap.live_count(self.database_oid, table, self.xid.get()) as f32;
            if live > 0.0 || size.reltuples >= 0.0 {
                let blocks = i32::try_from(size.blocks).unwrap_or(i32::MAX);
                heap.set_stats(self.database_oid, table, blocks, live);
            }
        }
    }

    /// `index_update_stats` of a new index: `relpages` from the pages of the index and `reltuples` from the rows that it has. A `btree` index has the pages that [`Heap::btree_size`] gives, and a `hash` index has the metapage, its buckets and the bitmap page, as `_hash_init` makes them for the rows that `estimate_rel_size` gives for the table, when its rows fill no bucket. A `btree` index with expressions or a predicate has the pages of the keys of its build. An index with no rows keeps the pages of a new index and a `reltuples` of 0, as do the indexes of other access methods.
    fn built_index_stats(&self, catalog: &Arc<Catalog>, index: &Relation) {
        let heap = self.heap();
        let Some(info) = &index.index else { return };
        let Some(table) = catalog.relation(info.table) else { return };
        let live = heap.live_count(self.database_oid, info.table, self.xid.get());
        let size = heap.size(self.database_oid, info.table);
        let pages = match info.method {
            rupg_catalog::BTREE
                if live > 0 && (info.predicate.is_some() || info.exprs.is_some()) =>
            {
                if let Some((pages, tuples)) = self.derived_btree_stats(catalog, index) {
                    heap.set_stats(self.database_oid, index.oid, pages, tuples);
                    return;
                }
                None
            }
            // The pages of a `hash` index come from the stats of the table, not from its keys.
            _ if live == 0
                || info.predicate.is_some()
                || (info.exprs.is_some() && info.method != HASH) =>
            {
                None
            }
            rupg_catalog::BTREE => explain::btree_shape(index, &table.columns).map(|shape| {
                heap.btree_size(self.database_oid, info.table, index.oid, &shape).blocks
            }),
            HASH => {
                let mut blocks = f64::from(size.blocks);
                if blocks < 10.0 && size.reltuples < 0.0 && catalog.children(table.oid).is_empty() {
                    blocks = 10.0;
                }
                let tuples = if size.reltuples >= 0.0 && size.relpages > 0 {
                    f64::from(size.reltuples) / f64::from(size.relpages) * blocks
                } else {
                    let widths = table
                        .columns
                        .iter()
                        .filter(|c| !c.dropped)
                        .map(|c| rupg_opt::type_width(c.ty, c.typmod));
                    let fillfactor = table
                        .options
                        .iter()
                        .find_map(|o| o.strip_prefix("fillfactor=")?.parse().ok())
                        .unwrap_or(100);
                    new_table_tuples(widths, fillfactor) / 10.0 * blocks
                };
                let pages = empty_index_pages(info.method, tuples.round_ties_even());
                // The rows that fill no bucket need no split of a bucket.
                let buckets = f64::from(pages.saturating_sub(2));
                (live as f64 <= buckets * HASH_BUCKET_ROWS)
                    .then(|| u32::try_from(pages).unwrap_or(1))
            }
            _ => None,
        };
        match pages {
            Some(pages) => {
                let pages = i32::try_from(pages).unwrap_or(i32::MAX);
                heap.set_stats(self.database_oid, index.oid, pages, live as f32);
            }
            None => heap.set_stats(self.database_oid, index.oid, 0, -1.0),
        }
    }

    /// `index_update_stats` of a new `btree` index with expressions or a predicate: the pages of the keys that the build computes for the rows of the table that pass the predicate, and the number of those rows. `None` for an index with `INCLUDE` columns or when an expression fails.
    fn derived_btree_stats(&self, catalog: &Arc<Catalog>, index: &Relation) -> Option<(i32, f32)> {
        let info = index.index.as_ref()?;
        let table = catalog.relation(info.table)?;
        let key_count = usize::try_from(info.key_count).ok()?;
        if info.keys.len() != key_count {
            return None;
        }
        let columns: Vec<(u32, i32)> = table.columns.iter().map(|c| (c.ty, c.typmod)).collect();
        let derived = rupg_analyze::index_derived(info, info.table, &columns).ok()?;
        let view = View { xid: self.xid.get(), ..self.view() };
        let rows = self.heap().rows(info.table, &view);
        let reader = self.reader_without_view(Arc::clone(catalog));
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(key) = rupg_exec::index_key(&derived, row, &reader).ok()? {
                entries.push(key.into_boxed_slice());
            }
        }
        let tuples = entries.len() as f32;
        let types = derived.exprs.iter().map(|e| e.ty).collect();
        let shape = explain::shape_of(index, (0..key_count).collect(), types);
        let size = crate::heap::Heap::btree_size_of(entries, &shape);
        Some((i32::try_from(size.blocks).unwrap_or(i32::MAX), tuples))
    }

    /// The statistics of the tables of `CLUSTER` and `REPACK` after the rewrite: `relpages` and `reltuples`, as `copy_table_data` sets them, and the statistics of the columns of `REPACK (ANALYZE)`, as `analyze_rel` writes them after the commit of the rewrite.
    pub(super) fn repacked_stats(&self, catalog: &Catalog, repacked: &[Repack]) {
        for repack in repacked {
            self.update_relstats(repack.table);
            if let Some(columns) = &repack.analyze {
                self.relation_stats(catalog, repack.table, (None, Some(columns)));
            }
        }
    }

    /// The `btree` indexes of the table `relid`, each with its pages, as `RelationGetNumberOfBlocks` gives them: the pages of the last build or `VACUUM`, or of a build from the versions of the table now when it has more.
    fn btree_pages<'c>(&self, catalog: &'c Catalog, relid: u32) -> Vec<(&'c Relation, i32)> {
        let heap = self.heap();
        let Some(table) = catalog.relation(relid) else { return Vec::new() };
        catalog
            .relations()
            .filter(|r| r.kind == RelKind::Index)
            .filter_map(|index| {
                let info = index.index.as_ref().filter(|i| i.table == relid)?;
                (info.method == rupg_catalog::BTREE).then_some(())?;
                let counted = heap.size(self.database_oid, index.oid).relpages;
                let built = explain::btree_shape(index, &table.columns).map_or(0, |shape| {
                    heap.btree_size(self.database_oid, relid, index.oid, &shape).blocks
                });
                let built = i32::try_from(built).unwrap_or(i32::MAX);
                Some((index, info.pages.max(counted).max(built)))
            })
            .collect()
    }

    /// `update_relstats_all_indexes` after `btbulkdelete`: `relpages` of each index from `indexes` and `reltuples` from the entries that are left, the live rows of the table `relid` for an index on columns with no predicate. An index whose entries fail to compute keeps its stats.
    fn vacuumed_index_stats(&self, relid: u32, indexes: &[(&Relation, i32)]) {
        let heap = self.heap();
        let catalog = self.catalog();
        let rows = heap.live_count(self.database_oid, relid, self.xid.get()) as f32;
        for &(index, pages) in indexes {
            let Some(info) = &index.index else { continue };
            let tuples = if info.predicate.is_none() && info.exprs.is_none() {
                rows
            } else {
                match self.derived_btree_stats(&catalog, index) {
                    Some((_, tuples)) => tuples,
                    None => continue,
                }
            };
            heap.set_stats(self.database_oid, index.oid, pages, tuples);
        }
    }

    /// `vac_update_relstats` for a table: `relpages` from the pages of its heap and `reltuples` from its live rows, which the result is.
    pub(super) fn update_relstats(&self, table: u32) -> f32 {
        let heap = self.heap();
        let blocks = heap.size(self.database_oid, table).blocks;
        let live = heap.live_count(self.database_oid, table, self.xid.get()) as f32;
        heap.set_stats(self.database_oid, table, i32::try_from(blocks).unwrap_or(i32::MAX), live);
        live
    }
}

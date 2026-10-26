//! `VACUUM` and `ANALYZE`, as `ExecVacuum` and `vacuum` of `vacuum.c` run them: the options, the checks of the options and of the relations, and the warnings for the relations that the statement skips. The statement sets `relpages` and `reltuples` of each table. rupg keeps the dead row versions and no column statistics yet.

use rupg_catalog::{Catalog, RelKind};

use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{DefElem, Node, VacuumStmt};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::{Connection, Done};
use crate::guc;
use crate::utility::Notice;

/// `MIN_BAS_VAC_RING_SIZE_KB` and `MAX_BAS_VAC_RING_SIZE_KB`.
const RING_SIZE_KB: (i32, i32) = (128, 16 * 1024 * 1024);

/// `MAX_PARALLEL_WORKER_LIMIT`.
const MAX_PARALLEL_WORKERS: i32 = 1024;

/// The options of `VACUUM` and `ANALYZE` that the checks of `ExecVacuum` read.
#[derive(Debug, Default)]
struct Parsed {
    analyze: bool,
    freeze: bool,
    full: bool,
    disable_page_skipping: bool,
    process_toast: bool,
    only_database_stats: bool,
    /// True when a flag of `params.options` other than `VERBOSE`, `PROCESS_MAIN` and `PROCESS_TOAST` is on, for the check of `ONLY_DATABASE_STATS`.
    other: bool,
    workers: i32,
    ring_size: Option<i32>,
}

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
    let mut parsed = Parsed { process_toast: true, ..Parsed::default() };
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
                if def.arg.is_some() && !def_string(def)?.eq_ignore_ascii_case("auto") {
                    def_boolean(def)?;
                }
            }
            "process_main" => {
                def_boolean(def)?;
            }
            "process_toast" => parsed.process_toast = def_boolean(def)?,
            "truncate" => {
                def_boolean(def)?;
            }
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
    /// `ExecVacuum`, then `vacuum`: `PreventInTransactionBlock` for `VACUUM`, and the checks of [`rupg_analyze::vacuum_relations`] on each relation of the statement. The session has only a superuser role, so the check of `vacuum_is_permitted_for_relation` always passes.
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
        let result = rupg_analyze::vacuum_relations(
            &stmt.rels,
            &reader,
            stmt.is_vacuumcmd,
            analyze,
            &mut warnings,
        );
        notices.extend(warnings.into_iter().map(Notice::warning));
        let relids = result?;
        let catalog = self.catalog();
        // `get_all_vacuum_rels`: with no list, each table, materialized view and partitioned table.
        let relids = if parsed.only_database_stats {
            Vec::new()
        } else if stmt.rels.is_empty() {
            let kinds = |r: &&rupg_catalog::Relation| {
                matches!(r.kind, RelKind::Table | RelKind::MatView | RelKind::Partitioned)
            };
            catalog.relations().filter(kinds).map(|r| r.oid).collect()
        } else {
            relids
        };
        for relid in relids {
            self.relation_stats(&catalog, relid, (stmt.is_vacuumcmd, analyze));
        }
        Ok(Done::Tag(tag))
    }

    /// `vac_update_relstats` of `lazy_vacuum_rel` and `do_analyze_rel` for the relation `relid`, with its TOAST table for `VACUUM`, and for each partition of a partitioned table, as `expand_vacuum_rel` adds them. `ANALYZE` of a partitioned table sets its `reltuples` to the rows of its partitions and its `relpages` to -1, as the table has no storage. The result is the live rows of the relation.
    fn relation_stats(
        &self,
        catalog: &Catalog,
        relid: u32,
        (vacuum, analyze): (bool, bool),
    ) -> f32 {
        let Some(rel) = catalog.relation(relid) else { return 0.0 };
        match rel.kind {
            RelKind::Table | RelKind::MatView | RelKind::Toast => {
                if vacuum && rel.toast != 0 {
                    self.update_relstats(rel.toast);
                }
                self.update_relstats(relid)
            }
            RelKind::Partitioned => {
                let parts = catalog.children(relid);
                let rows = parts
                    .iter()
                    .map(|p| self.relation_stats(catalog, p.oid, (vacuum, analyze)))
                    .sum();
                if analyze {
                    self.heap().set_stats(self.database_oid, relid, -1, rows);
                }
                rows
            }
            _ => 0.0,
        }
    }

    /// `index_update_stats` after a statement that built indexes, the indexes of `new` that are not in `old` or that have a new file: `relpages` and `reltuples` of the table of each index, while `autovacuum` is on and the table does not turn it off. A table with no rows that no `VACUUM` or `ANALYZE` counted keeps its `reltuples` of -1.
    pub(super) fn index_stats(&self, old: &Catalog, new: &Catalog) {
        let mut tables: Vec<u32> = new
            .relations()
            .filter(|r| old.relation(r.oid).is_none_or(|o| o.file != r.file))
            .filter_map(|r| r.index.as_ref().filter(|_| r.kind == RelKind::Index))
            .map(|index| index.table)
            .collect();
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

    /// `vac_update_relstats` for a table: `relpages` from the pages of its heap and `reltuples` from its live rows, which the result is.
    pub(super) fn update_relstats(&self, table: u32) -> f32 {
        let heap = self.heap();
        let blocks = heap.size(self.database_oid, table).blocks;
        let live = heap.live_count(self.database_oid, table, self.xid.get()) as f32;
        heap.set_stats(self.database_oid, table, i32::try_from(blocks).unwrap_or(i32::MAX), live);
        live
    }
}

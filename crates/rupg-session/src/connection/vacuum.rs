//! `VACUUM` and `ANALYZE`, as `ExecVacuum` and `vacuum` of `vacuum.c` run them: the options, the checks of the options and of the relations, and the warnings for the relations that the statement skips. rupg keeps no dead row versions and no statistics yet, so the statement changes nothing.

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
        result?;
        Ok(Done::Tag(tag))
    }
}

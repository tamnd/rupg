//! `EXPLAIN`, as `ExplainQuery` of `explain.c` runs it: the options, the plan of the query and the rows of its output. `EXPLAIN ANALYZE` is not supported yet.

use std::cell::RefCell;

use rupg_analyze::{Expr, Params, Query};
use rupg_common::{Error, SqlState};
use rupg_func::Session;
use rupg_opt::{Index, Member};
use rupg_plan::{Format, Options};
use rupg_sql::nodes::{DefElem, ExplainStmt, Node};
use rupg_types::oid;
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::extended::protocol_error;
use super::{Connection, Done};
use crate::query::{self, Column, Reader};
use crate::utility::Notice;

/// The members of the inheritance trees of the relations of a plan, from the catalog of the statement. The first error of the pruning of the partitions is the error of the statement.
#[derive(Debug)]
struct Members<'r, 'a> {
    reader: &'r Reader<'a>,
    error: RefCell<Option<Error>>,
}

impl rupg_opt::Members for Members<'_, '_> {
    fn members(&self, query: &Query, relation: usize, conds: &[&Expr]) -> Option<Vec<Member>> {
        let oids = match rupg_exec::members(query, relation, conds, self.reader) {
            Ok(oids) => oids?,
            Err(error) => {
                self.error.borrow_mut().get_or_insert(error);
                return None;
            }
        };
        let catalog = self.reader.catalog()?;
        let members = oids
            .into_iter()
            .filter_map(|oid| {
                let table = catalog.relation(oid)?;
                let columns =
                    table.columns.iter().filter(|c| !c.dropped).map(|c| (c.ty, c.typmod)).collect();
                let has_children = !catalog.all_inheritors(oid).is_empty();
                Some(Member { oid, columns, has_children })
            })
            .collect();
        Some(members)
    }

    fn indexes(&self, table: u32) -> Vec<Index> {
        let Some(catalog) = self.reader.catalog() else { return Vec::new() };
        let mut out: Vec<Index> = catalog
            .relations()
            .filter_map(|rel| {
                let info = rel.index.as_ref().filter(|i| i.table == table && i.valid)?;
                let count = usize::try_from(info.key_count).unwrap_or(0);
                Some(Index {
                    oid: rel.oid,
                    keys: info.keys.iter().take(count).copied().collect(),
                    unique: info.unique,
                    nulls_not_distinct: info.nulls_not_distinct,
                    immediate: info.immediate,
                    partial: info.predicate.is_some(),
                })
            })
            .collect();
        out.sort_by_key(|i| i.oid);
        out
    }
}

/// The options of `EXPLAIN`, as `ParseExplainOptionList` reads them.
#[derive(Debug, Default)]
struct Parsed {
    analyze: bool,
    verbose: bool,
    costs: Option<bool>,
    format: Format,
    summary: Option<bool>,
    memory: bool,
    generic: bool,
}

/// `ParseExplainOptionList`: the options of the statement and the checks of the options that need `ANALYZE`.
fn parse_options(stmt: &ExplainStmt) -> Result<Parsed, Error> {
    let mut parsed = Parsed::default();
    let mut wal = false;
    let mut timing = None;
    let mut serialize = false;
    for def in stmt.options.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = usize::try_from(def.location).ok();
        match name {
            "analyze" => parsed.analyze = def_boolean(def)?,
            "verbose" => parsed.verbose = def_boolean(def)?,
            "costs" => parsed.costs = Some(def_boolean(def)?),
            "buffers" | "settings" => {
                def_boolean(def)?;
            }
            "wal" => wal = def_boolean(def)?,
            "generic_plan" => parsed.generic = def_boolean(def)?,
            "timing" => timing = Some(def_boolean(def)?),
            "summary" => parsed.summary = Some(def_boolean(def)?),
            "memory" => parsed.memory = def_boolean(def)?,
            "serialize" => {
                serialize = match def.arg {
                    None => true,
                    Some(_) => match def_string(def)?.as_str() {
                        "off" | "none" => false,
                        "text" | "binary" => true,
                        other => return Err(bad_value(def, other).with_position(at)),
                    },
                }
            }
            "format" => {
                parsed.format = match def_string(def)?.as_str() {
                    "text" => Format::Text,
                    "xml" => Format::Xml,
                    "json" => Format::Json,
                    "yaml" => Format::Yaml,
                    other => return Err(bad_value(def, other).with_position(at)),
                }
            }
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unrecognized EXPLAIN option \"{name}\""),
                )
                .with_position(at));
            }
        }
    }
    let needs = |what: &str| {
        Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("EXPLAIN option {what} requires ANALYZE"),
        )
    };
    if wal && !parsed.analyze {
        return Err(needs("WAL"));
    }
    if timing.unwrap_or(parsed.analyze) && !parsed.analyze {
        return Err(needs("TIMING"));
    }
    if serialize && !parsed.analyze {
        return Err(needs("SERIALIZE"));
    }
    if parsed.generic && parsed.analyze {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
        ));
    }
    Ok(parsed)
}

/// The error of an option of `EXPLAIN` with a value that it does not take.
fn bad_value(def: &DefElem, value: &str) -> Error {
    let name = def.defname.as_deref().unwrap_or_default();
    Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unrecognized value for EXPLAIN option \"{name}\": \"{value}\""),
    )
}

/// The error of a feature of `EXPLAIN` that the engine does not have yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

impl Connection {
    /// `ExplainQuery`: the plan of the query of the statement. The text form gives a row for each line, and the other forms give one row with the whole value.
    pub(super) fn explain(
        &self,
        stmt: &ExplainStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let parsed = parse_options(stmt)?;
        if parsed.analyze {
            return Err(not_yet("EXPLAIN ANALYZE"));
        }
        if parsed.generic {
            return Err(not_yet("EXPLAIN (GENERIC_PLAN)"));
        }
        if parsed.memory {
            return Err(not_yet("EXPLAIN (MEMORY)"));
        }
        let Some(node) = &stmt.query else {
            return Err(Error::internal("EXPLAIN with no statement"));
        };
        // `ExplainOneUtility`: `CREATE TABLE AS` and `DECLARE CURSOR` show the plan of their query.
        let inner = match node {
            Node::CreateTableAsStmt(s) => s.query.as_ref(),
            Node::DeclareCursorStmt(s) => s.query.as_ref(),
            Node::ExecuteStmt(s) => {
                let name = s.name.as_deref().unwrap_or("");
                self.statements.get(name.as_bytes()).map_err(protocol_error)?;
                return Err(not_yet("EXPLAIN EXECUTE"));
            }
            _ => Some(node),
        };
        let Some(inner) = inner.filter(|n| query::is_query(n)) else {
            return Err(not_yet("EXPLAIN of this statement"));
        };
        let started = self.clock.monotonic();
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let plan = query::plan(inner, &reader, &Params::default());
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        let plan = plan.fold(None, &reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        let members = Members { reader: &reader, error: RefCell::new(None) };
        let query = rupg_opt::pull_up(plan.query());
        let tree = rupg_opt::plan(&query, &members);
        if let Some(error) = members.error.take() {
            return Err(error);
        }
        let planning_time = parsed.summary.unwrap_or(false).then(|| {
            let spent = self.clock.monotonic().saturating_sub(started);
            spent.as_secs_f64() * 1000.0
        });
        let options = Options {
            verbose: parsed.verbose,
            costs: parsed.costs.unwrap_or(true),
            format: parsed.format,
            planning_time,
        };
        let text = rupg_func::explain(&tree, &options, &reader)?;
        let ty = match parsed.format {
            Format::Json => oid::JSON,
            Format::Xml => oid::XML,
            Format::Text | Format::Yaml => oid::TEXT,
        };
        let columns = vec![Column { ty, ..Column::text("QUERY PLAN".to_string()) }];
        let rows = if parsed.format == Format::Text {
            text.split('\n').map(|line| vec![Some(line.as_bytes().to_vec())]).collect()
        } else {
            vec![vec![Some(text.into_bytes())]]
        };
        Ok(Done::Rows { columns, rows, tag: CommandTag::Explain })
    }
}

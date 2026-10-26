//! `EXPLAIN`, as `ExplainQuery` of `explain.c` runs it: the options, the plan of the query and the rows of its output. `EXPLAIN ANALYZE` is not supported yet.

use std::cell::RefCell;
use std::sync::Arc;

use rupg_analyze::{BoolOp, Expr, ExprKind, Params, Query};
use rupg_common::{Error, SqlState};
use rupg_func::Session;
use rupg_opt::{Enable, Index, Member, Size};
use rupg_pgcatalog::builtin;
use rupg_plan::{Format, Options};
use rupg_sql::nodes::{DefElem, ExecuteStmt, ExplainStmt, Node};
use rupg_types::{Value, oid};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::extended::protocol_error;
use super::{Connection, Done};
use crate::heap::Heap;
use crate::query::{self, Column, Reader};
use crate::utility::Notice;

/// The members of the inheritance trees of the relations of a plan, from the catalog of the statement. The first error of the pruning of the partitions is the error of the statement.
#[derive(Debug)]
struct Members<'r, 'a> {
    reader: &'r Reader<'a>,
    error: RefCell<Option<Error>>,
    /// The rows of the tables of the database `database`, which tell if an index has more pages than a new index.
    heap: &'r Heap,
    database: u32,
}

impl Members<'_, '_> {
    /// `RelationGetNumberOfBlocks` of an index of the table `table` with the pages `pages` of a new index: the first row of a `btree` adds its root page after the metapage, as `_bt_getroot` makes it.
    fn index_pages(&self, table: u32, method: u32, pages: i32) -> i32 {
        if method == rupg_catalog::BTREE && self.heap.has_rows(self.database, table) {
            pages.max(2)
        } else {
            pages.max(1)
        }
    }
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
                let size = rupg_opt::Members::size(self, oid);
                Some(Member { oid, columns, has_children, size })
            })
            .collect();
        Some(members)
    }

    fn size(&self, table: u32) -> Size {
        let size = self.heap.size(self.database, table);
        Size {
            blocks: f64::from(size.blocks),
            relpages: f64::from(size.relpages),
            reltuples: f64::from(size.reltuples),
        }
    }

    fn statistics(&self, table: u32) -> Vec<rupg_func::Statistic> {
        self.heap.statistics(self.database, table)
    }

    fn variable_range(&self, table: u32, attnum: i16) -> Option<(Value, Value)> {
        let catalog = self.reader.catalog()?;
        let column = usize::try_from(attnum - 1).ok()?;
        let ty = catalog.relation(table)?.columns.get(column)?.ty;
        let order = |a: &Value, b: &Value| rupg_func::compare(ty, a, ty, b).ok();
        self.heap.variable_range(self.database, table, column, order)
    }

    fn compare(&self) -> rupg_opt::Compare {
        |left, a, right, b| rupg_func::compare(left, a, right, b).ok()
    }

    fn indexes(&self, table: u32) -> Vec<Index> {
        let Some(catalog) = self.reader.catalog() else { return Vec::new() };
        let columns = catalog.relation(table).map(|t| t.columns.as_slice()).unwrap_or_default();
        let mut out: Vec<Index> = catalog
            .relations()
            .filter_map(|rel| {
                let info = rel.index.as_ref().filter(|i| i.table == table && i.valid)?;
                let count = usize::try_from(info.key_count).unwrap_or(0);
                let class = |oid: &u32| builtin::opclasses().iter().find(|c| c.oid == *oid);
                let exprs = info.exprs.as_deref().map_or_else(Vec::new, |text| {
                    rupg_analyze::node::read_list(text).unwrap_or_default()
                });
                // `make_ands_implicit`: the parts of the `AND` of the predicate.
                let predicate = match info.predicate.as_deref().map(rupg_analyze::node::read) {
                    Some(Ok(Expr { kind: ExprKind::Bool(BoolOp::And, parts), .. })) => parts,
                    Some(Ok(expr)) => vec![expr],
                    Some(Err(_)) | None => Vec::new(),
                };
                let mut next = exprs.iter();
                let width = info
                    .keys
                    .iter()
                    .map(|&attnum| match usize::try_from(attnum) {
                        Ok(n @ 1..) => {
                            columns.get(n - 1).map_or(0, |c| rupg_opt::type_width(c.ty, c.typmod))
                        }
                        _ => next.next().map_or(0, |e| rupg_opt::type_width(e.ty, e.typmod)),
                    })
                    .sum();
                Some(Index {
                    oid: rel.oid,
                    keys: info.keys.iter().take(count).copied().collect(),
                    unique: info.unique,
                    nulls_not_distinct: info.nulls_not_distinct,
                    immediate: info.immediate,
                    partial: info.predicate.is_some(),
                    method: info.method,
                    columns: info.keys.clone(),
                    families: info
                        .classes
                        .iter()
                        .map(|c| class(c).map_or(0, |c| c.family))
                        .collect(),
                    default_class: info
                        .classes
                        .iter()
                        .map(|c| class(c).is_some_and(|c| c.default))
                        .collect(),
                    options: info.options.clone(),
                    collations: info.collations.clone(),
                    pages: f64::from(self.index_pages(table, info.method, info.pages)),
                    exprs,
                    predicate,
                    width,
                })
            })
            .collect();
        out.sort_by_key(|i| i.oid);
        out
    }

    fn enable(&self) -> Enable {
        let settings = self.reader.settings();
        let on = |name: &str| settings.get(name).is_none_or(|v| v != "off");
        Enable {
            seqscan: on("enable_seqscan"),
            indexscan: on("enable_indexscan"),
            indexonlyscan: on("enable_indexonlyscan"),
            bitmapscan: on("enable_bitmapscan"),
            sort: on("enable_sort"),
            incremental_sort: on("enable_incremental_sort"),
            hashagg: on("enable_hashagg"),
            material: on("enable_material"),
            presorted_aggregate: on("enable_presorted_aggregate"),
            nestloop: on("enable_nestloop"),
            mergejoin: on("enable_mergejoin"),
            hashjoin: on("enable_hashjoin"),
        }
    }
}

/// The order of the rows of the joins of `query`, from the plan that `EXPLAIN` shows for it, as [`rupg_opt::join_order`] gives it. `None` when the plan gives its rows in another order or the planner fails.
pub(crate) fn join_order(
    reader: &Reader<'_>,
    heap: &Heap,
    database: u32,
    query: &Query,
) -> Option<Vec<(usize, bool)>> {
    let members = Members { reader, error: RefCell::new(None), heap, database };
    let barrier = |view: u32| {
        reader.catalog().and_then(|c| c.relation(view)).is_some_and(rupg_analyze::security_barrier)
    };
    let mut query = rupg_opt::pull_up(query, &barrier);
    rupg_opt::remove_joins(&mut query, &members);
    let tree = rupg_opt::plan(&query, &members);
    if members.error.take().is_some() {
        return None;
    }
    rupg_opt::join_order(&tree)
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
            Node::ExecuteStmt(s) => return self.explain_execute(s, &parsed, notices),
            _ => Some(node),
        };
        let Some(inner) = inner.filter(|n| query::is_query(n)) else {
            return Err(not_yet("EXPLAIN of this statement"));
        };
        let started = self.clock.monotonic();
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        // `ExplainQuery`: `QueryRewrite` gives the statements of the rules, and each one shows its plan. A blank line comes between the plans of the text form.
        let Some(products) = rupg_analyze::query_rewrite(inner, &reader)? else {
            let plan = query::plan(inner, &reader, &Params::default());
            let text = self.explain_plan(plan, &parsed, started, &reader, notices)?;
            return Ok(explain_rows(text, parsed.format));
        };
        // `ExplainBeginOutput` and `ExplainEndOutput` of the form JSON put the plans in one array, and `ExplainSeparatePlans` puts a blank line between the plans of the text form.
        if products.len() != 1 && !matches!(parsed.format, Format::Text | Format::Json) {
            return Err(not_yet("EXPLAIN of a statement that rules rewrite in this format"));
        }
        let mut texts = Vec::with_capacity(products.len());
        for product in &products {
            let plan = rupg_analyze::analyze_product(product, &reader, &Params::default())
                .and_then(|query| rupg_exec::prepare(query, &reader));
            texts.push(self.explain_plan(plan, &parsed, started, &reader, notices)?);
        }
        let text = match parsed.format {
            Format::Text if texts.is_empty() => "Query rewrites to nothing".to_string(),
            Format::Text => texts.join("\n\n"),
            Format::Json if texts.len() != 1 => {
                let inner: Vec<&str> = texts
                    .iter()
                    .map(|t| {
                        let t = t.strip_prefix("[\n").unwrap_or(t);
                        t.strip_suffix("\n]").unwrap_or(t)
                    })
                    .collect();
                if inner.is_empty() {
                    "[\n]".to_string()
                } else {
                    format!("[\n{}\n]", inner.join(",\n"))
                }
            }
            _ => texts.concat(),
        };
        Ok(explain_rows(text, parsed.format))
    }

    /// `ExplainOneQuery`: the text of the plan of one statement.
    fn explain_plan(
        &self,
        plan: Result<rupg_exec::Plan, Error>,
        parsed: &Parsed,
        started: std::time::Duration,
        reader: &Reader<'_>,
        notices: &mut Vec<Notice>,
    ) -> Result<String, Error> {
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        notices.extend(plan.notices().iter().map(|error| Notice::warning(error.clone())));
        self.explain_with(&plan, None, parsed, started, reader, notices)
    }

    /// `ExplainExecuteQuery`: the plan of a prepared statement with the values of the parameters of `EXECUTE`, as `GetCachedPlan` makes a custom plan with them.
    fn explain_execute(
        &self,
        stmt: &ExecuteStmt,
        parsed: &Parsed,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let name = stmt.name.as_deref().unwrap_or("");
        let statement = Arc::clone(self.statements.get(name.as_bytes()).map_err(protocol_error)?);
        let started = self.clock.monotonic();
        self.settings.borrow_mut().take_snapshot();
        let params = self.execute_params(stmt, &statement, notices)?;
        let reader = self.reader();
        let replanned = statement.replan(&reader)?;
        // `ExplainOneUtility` for a statement that the session runs.
        let Some(plan) = replanned.as_ref().or(statement.plan.as_ref()) else {
            return Err(not_yet("EXPLAIN EXECUTE of a utility statement"));
        };
        statement.count_plan(reader.settings().get("plan_cache_mode").as_deref());
        let text = self.explain_with(plan, Some(&params), parsed, started, &reader, notices)?;
        Ok(explain_rows(text, parsed.format))
    }

    /// The text of the plan `plan`, with the values `params` of its parameters for a custom plan.
    fn explain_with(
        &self,
        plan: &rupg_exec::Plan,
        params: Option<&[Value]>,
        parsed: &Parsed,
        started: std::time::Duration,
        reader: &Reader<'_>,
        notices: &mut Vec<Notice>,
    ) -> Result<String, Error> {
        let plan = plan.fold(params, reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?;
        let members = Members {
            reader,
            error: RefCell::new(None),
            heap: self.store.heap(),
            database: self.database_oid,
        };
        let barrier = |view: u32| {
            reader
                .catalog()
                .and_then(|c| c.relation(view))
                .is_some_and(rupg_analyze::security_barrier)
        };
        let mut query = rupg_opt::pull_up(plan.query(), &barrier);
        rupg_opt::remove_joins(&mut query, &members);
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
        rupg_func::explain(&tree, &options, reader)
    }
}

/// The rows of `EXPLAIN`: a row for each line of the text form, and one row with the whole value for the other forms.
fn explain_rows(text: String, format: Format) -> Done {
    let ty = match format {
        Format::Json => oid::JSON,
        Format::Xml => oid::XML,
        Format::Text | Format::Yaml => oid::TEXT,
    };
    let columns = vec![Column { ty, ..Column::text("QUERY PLAN".to_string()) }];
    let rows = if format == Format::Text {
        text.split('\n').map(|line| vec![Some(line.as_bytes().to_vec())]).collect()
    } else {
        vec![vec![Some(text.into_bytes())]]
    };
    Done::Rows { columns, rows, tag: CommandTag::Explain }
}

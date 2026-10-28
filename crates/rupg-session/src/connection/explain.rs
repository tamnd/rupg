//! `EXPLAIN`, as `ExplainQuery` of `explain.c` runs it: the options, the plan of the query and the rows of its output. `EXPLAIN ANALYZE` is not supported yet.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_analyze::{BoolOp, Expr, ExprKind, Params, Query};
use rupg_common::{Error, SqlState};
use rupg_func::Session;
use rupg_opt::{Enable, Index, Member, Size};
use rupg_pgcatalog::builtin;
use rupg_plan::{Format, HashTables, Options};
use rupg_sql::nodes::{DefElem, ExecuteStmt, ExplainStmt, Node};
use rupg_types::{Value, oid};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::extended::protocol_error;
use super::{Connection, Done};
use crate::heap::{BtreeSize, Heap, Shape};
use crate::query::{self, Column, Reader};
use crate::utility::Notice;

/// `STATISTIC_KIND_MCELEM`: the most common elements of an array column.
const STATISTIC_KIND_MCELEM: i16 = 4;
/// `STATISTIC_KIND_DECHIST`: the histogram of the counts of the distinct elements of an array column.
const STATISTIC_KIND_DECHIST: i16 = 5;

/// The members of the inheritance trees of the relations of a plan, from the catalog of the statement. The first error of the pruning of the partitions is the error of the statement.
#[derive(Debug)]
struct Members<'r, 'a> {
    reader: &'r Reader<'a>,
    error: RefCell<Option<Error>>,
    /// The rows of the tables of the database `database`, which tell if an index has more pages than a new index.
    heap: &'r Heap,
    database: u32,
}

thread_local! {
    /// The statistics of the system catalogs that [`Members::catalog_statistics`] read, by the OID of the catalog.
    static CATALOG_STATISTICS: RefCell<BTreeMap<u32, Vec<rupg_func::Statistic>>> = const { RefCell::new(BTreeMap::new()) };
}

impl Members<'_, '_> {
    /// The statistics of the system catalog `table` in a new database. The input function of the array type of the column reads the values of a slot, or the input function of the array type of the element type for a slot of the elements of an array.
    fn catalog_statistics(&self, table: u32) -> Vec<rupg_func::Statistic> {
        let Some(catalog) = rupg_pgcatalog::catalog_by_oid(table) else { return Vec::new() };
        let slot = |ty: u32, slot: &builtin::CatalogSlot| {
            let values = match slot.values {
                None => None,
                Some(text) => {
                    let ty = if matches!(slot.kind, STATISTIC_KIND_MCELEM | STATISTIC_KIND_DECHIST)
                    {
                        rupg_analyze::types::element(ty)
                    } else {
                        ty
                    };
                    let array = rupg_analyze::types::array_of(ty);
                    match rupg_func::input(array, text, -1, self.reader) {
                        Ok(Value::Array(array)) => {
                            Some(array.values.into_iter().flatten().collect())
                        }
                        _ => return None,
                    }
                }
            };
            Some(rupg_func::StatSlot {
                kind: slot.kind,
                operator: slot.operator,
                collation: slot.collation,
                numbers: slot.numbers.map(<[f32]>::to_vec),
                values,
            })
        };
        builtin::catalog_statistics(table)
            .iter()
            .filter_map(|row| {
                let column = catalog.columns.get(usize::try_from(row.attnum - 1).ok()?)?;
                Some(rupg_func::Statistic {
                    attnum: row.attnum,
                    inherit: row.inherit,
                    null_frac: row.null_frac,
                    width: row.width,
                    distinct: row.distinct,
                    slots: row.slots.iter().filter_map(|s| slot(column.type_oid, s)).collect(),
                })
            })
            .collect()
    }

    /// `RelationGetNumberOfBlocks` of an index of the table `table` with the pages `pages` of a new index: the first row of a `btree` adds its root page after the metapage, as `_bt_getroot` makes it.
    fn index_pages(&self, table: u32, method: u32, pages: i32) -> i32 {
        if method == rupg_catalog::BTREE && self.heap.has_rows(self.database, table) {
            pages.max(2)
        } else {
            pages.max(1)
        }
    }

    /// The pages and the height of the index `index` of the table `table` with the columns `columns`. A `btree` index on columns of the table with no predicate has the size that [`Heap::btree_size`] gives from the versions of the table, and another index has the pages of [`Members::index_pages`]. The root of such a `btree` index is not a leaf when the index has more pages than the metapage and one leaf, and another index has a height of 0.
    fn index_size(
        &self,
        table: u32,
        index: &rupg_catalog::Relation,
        columns: &[rupg_catalog::Column],
    ) -> BtreeSize {
        let Some(info) = &index.index else { return BtreeSize::EMPTY };
        // The build of an index with rows gave it the pages of `index_update_stats`.
        let built = self.heap.size(self.database, index.oid);
        let pages = if built.reltuples >= 0.0 { built.relpages } else { info.pages };
        let pages = self.index_pages(table, info.method, pages);
        let height = u32::from(info.method == rupg_catalog::BTREE && pages > 2);
        let fallback = BtreeSize { blocks: u32::try_from(pages).unwrap_or(1), height };
        match btree_shape(index, columns) {
            Some(shape) => self.heap.btree_size(self.database, table, index.oid, &shape),
            None => fallback,
        }
    }
}

/// The shape of the `btree` index `index` on columns of a table with the columns `columns` and no predicate, for [`Heap::btree_size`]. `None` for another index.
pub(super) fn btree_shape(
    index: &rupg_catalog::Relation,
    columns: &[rupg_catalog::Column],
) -> Option<Shape> {
    let info = index.index.as_ref()?;
    if info.method != rupg_catalog::BTREE || info.predicate.is_some() || info.pages == 0 {
        return None;
    }
    let places: Option<Vec<usize>> = info
        .keys
        .iter()
        .map(|&attnum| usize::try_from(attnum).ok().filter(|n| *n > 0).map(|n| n - 1))
        .collect();
    let places = places?;
    let types = places.iter().map(|&p| columns.get(p).map(|c| c.ty)).collect::<Option<_>>()?;
    Some(shape_of(index, places, types))
}

/// The shape of the `btree` index `index` with the columns at `places` of the types `types`.
pub(super) fn shape_of(
    index: &rupg_catalog::Relation,
    places: Vec<usize>,
    types: Vec<u32>,
) -> Shape {
    let info = index.index.as_ref();
    let option = |name: &str| {
        index.options.iter().find_map(|text| {
            let (found, value) = text.split_once('=')?;
            (found == name).then(|| value.to_ascii_lowercase())
        })
    };
    Shape {
        columns: places,
        types,
        key_count: info.map_or(0, |i| usize::try_from(i.key_count).unwrap_or(0)),
        options: info.map(|i| i.options.clone()).unwrap_or_default(),
        unique: info.is_some_and(|i| i.unique),
        fillfactor: option("fillfactor").and_then(|v| v.parse().ok()).unwrap_or(90),
        deduplicate: option("deduplicate_items")
            .is_none_or(|v| !matches!(v.as_str(), "off" | "of" | "false" | "f" | "no" | "n" | "0")),
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
        let parent = catalog.relation(query.relations.get(relation)?.oid)?;
        let members = oids
            .into_iter()
            .filter_map(|oid| {
                let table = catalog.relation(oid)?;
                let columns =
                    table.columns.iter().filter(|c| !c.dropped).map(|c| (c.ty, c.typmod)).collect();
                let has_children = !catalog.all_inheritors(oid).is_empty();
                let size = rupg_opt::Members::size(self, oid);
                // `make_inh_translation_list`: the column of the child with the name of each column of the parent.
                let attnums = parent
                    .columns
                    .iter()
                    .map(|p| {
                        let found = (!p.dropped)
                            .then(|| {
                                table.columns.iter().position(|c| !c.dropped && c.name == p.name)
                            })
                            .flatten();
                        found.and_then(|at| i16::try_from(at + 1).ok()).unwrap_or(0)
                    })
                    .collect();
                Some(Member { oid, columns, has_children, size, attnums })
            })
            .collect();
        Some(members)
    }

    fn size(&self, table: u32) -> Size {
        if table < rupg_catalog::FIRST_NORMAL_OID
            && let Some((pages, tuples)) = builtin::catalog_size(table)
        {
            let pages = f64::from(pages);
            // initdb ends with `VACUUM FREEZE`, which marks each page of a catalog all visible.
            return Size {
                blocks: pages,
                relpages: pages,
                reltuples: f64::from(tuples),
                relallvisible: pages,
            };
        }
        let size = self.heap.size(self.database, table);
        Size {
            blocks: f64::from(size.blocks),
            relpages: f64::from(size.relpages),
            reltuples: f64::from(size.reltuples),
            relallvisible: f64::from(size.relallvisible),
        }
    }

    fn statistics(&self, table: u32) -> Vec<rupg_func::Statistic> {
        if table < rupg_catalog::FIRST_NORMAL_OID {
            return CATALOG_STATISTICS.with_borrow_mut(|cache| {
                cache.entry(table).or_insert_with(|| self.catalog_statistics(table)).clone()
            });
        }
        self.heap.statistics(self.database, table)
    }

    fn variable_range(&self, table: u32, attnum: i16) -> Option<(Value, Value)> {
        let catalog = self.reader.catalog()?;
        let column = usize::try_from(attnum - 1).ok()?;
        let ty = catalog.relation(table)?.columns.get(column)?.ty;
        let order = |a: &Value, b: &Value| rupg_func::compare(ty, a, ty, b).ok();
        self.heap.variable_range(self.database, table, column, order)
    }

    fn ordered_partitions(&self, table: u32, members: &[u32]) -> Option<Vec<i16>> {
        let catalog = self.reader.catalog()?;
        let key = catalog.relation(table)?.partition_key.as_ref()?;
        // A `RANGE` partition holds the rows below the bound of the next one, and the default partition holds rows of any place in the order.
        let direct = |m: &u32| {
            let of = catalog.relation(*m).and_then(|r| r.partition_of.as_ref());
            of.is_some_and(|of| {
                of.parent == table && !matches!(of.bound, rupg_catalog::PartitionBound::Default)
            })
        };
        let ordered = matches!(key.strategy, rupg_catalog::PartStrategy::Range)
            && key.attrs.iter().all(|&a| a > 0)
            && members.iter().all(direct);
        ordered.then(|| key.attrs.clone())
    }

    fn compare(&self) -> rupg_opt::Compare {
        |left, a, right, b| rupg_func::compare(left, a, right, b).ok()
    }

    fn pattern_match(&self) -> rupg_opt::PatternMatch {
        rupg_func::pattern_match
    }

    fn indexes(&self, table: u32) -> Vec<Index> {
        if table < rupg_catalog::FIRST_NORMAL_OID {
            return catalog_indexes(table);
        }
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
                let size = self.index_size(table, rel, columns);
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
                    column_collations: info
                        .keys
                        .iter()
                        .take(count)
                        .map(|&attnum| match usize::try_from(attnum) {
                            Ok(n @ 1..) => columns.get(n - 1).map_or(0, |c| c.collation),
                            _ => 0,
                        })
                        .collect(),
                    pages: f64::from(size.blocks),
                    height: size.height,
                    exprs,
                    predicate,
                    width,
                    pages_per_range: pages_per_range(rel),
                })
            })
            .collect();
        out.sort_by_key(|i| i.oid);
        out
    }

    fn is_table(&self, table: u32) -> bool {
        let Some(catalog) = self.reader.catalog() else { return true };
        catalog.relation(table).is_none_or(|t| t.kind == rupg_catalog::RelKind::Table)
    }

    fn enable(&self) -> Enable {
        let settings = self.reader.settings();
        let on = |name: &str| settings.get(name).is_none_or(|v| v != "off");
        Enable {
            seqscan: on("enable_seqscan"),
            indexscan: on("enable_indexscan"),
            indexonlyscan: on("enable_indexonlyscan"),
            bitmapscan: on("enable_bitmapscan"),
            tidscan: on("enable_tidscan"),
            sort: on("enable_sort"),
            incremental_sort: on("enable_incremental_sort"),
            hashagg: on("enable_hashagg"),
            material: on("enable_material"),
            memoize: on("enable_memoize"),
            presorted_aggregate: on("enable_presorted_aggregate"),
            group_by_reordering: on("enable_group_by_reordering"),
            nestloop: on("enable_nestloop"),
            mergejoin: on("enable_mergejoin"),
            hashjoin: on("enable_hashjoin"),
            self_join_elimination: on("enable_self_join_elimination"),
        }
    }
}

/// `BrinGetPagesPerRange`: the option `pages_per_range` of a `brin` index, `BRIN_DEFAULT_PAGES_PER_RANGE` when it has none, and 0 for another index.
fn pages_per_range(index: &rupg_catalog::Relation) -> u32 {
    if index.index.as_ref().is_none_or(|i| i.method != BRIN_AM) {
        return 0;
    }
    let option = index.options.iter().find_map(|text| {
        let (name, value) = text.split_once('=')?;
        (name == "pages_per_range").then(|| value.parse().ok()).flatten()
    });
    option.unwrap_or(128)
}

/// `BRIN_AM_OID`.
const BRIN_AM: u32 = 3580;

/// The indexes of the system catalog `table`, with the sizes of a new database. The root of an index with more pages than the metapage and one leaf is not a leaf.
fn catalog_indexes(table: u32) -> Vec<Index> {
    let Some(catalog) = rupg_pgcatalog::catalog_by_oid(table) else { return Vec::new() };
    let class = |oid: &u32| builtin::opclasses().iter().find(|c| c.oid == *oid);
    builtin::catalog_indexes(table)
        .into_iter()
        .map(|info| {
            let pages = builtin::catalog_size(info.oid).map_or(1, |(pages, _)| pages);
            let width = info
                .keys
                .iter()
                .filter_map(|&attnum| catalog.columns.get(usize::try_from(attnum - 1).ok()?))
                .map(|c| rupg_opt::type_width(c.type_oid, -1))
                .sum();
            Index {
                oid: info.oid,
                keys: info.keys.iter().take(info.key_count).copied().collect(),
                unique: info.unique,
                nulls_not_distinct: false,
                immediate: true,
                partial: false,
                method: rupg_catalog::BTREE,
                columns: info.keys.to_vec(),
                families: info.classes.iter().map(|c| class(c).map_or(0, |c| c.family)).collect(),
                default_class: info
                    .classes
                    .iter()
                    .map(|c| class(c).is_some_and(|c| c.default))
                    .collect(),
                options: info.options.to_vec(),
                collations: info.collations.to_vec(),
                column_collations: info.collations.to_vec(),
                pages: f64::from(pages),
                height: u32::from(pages > 2),
                exprs: Vec::new(),
                predicate: Vec::new(),
                width,
                pages_per_range: 0,
            }
        })
        .collect()
}

/// The relations of a join in the order of its plan, each with true when its rows come in reverse and with the keys of its index scan.
type JoinOrder = Vec<rupg_plan::Scanned>;

/// The order of the rows of the joins of `query` and its hash tables, from the plan that `EXPLAIN` shows for it, as [`rupg_opt::join_order`] and [`rupg_opt::hash_tables`] give them. The order is `None` when the plan gives its rows in another order. `None` when the planner fails.
pub(crate) fn planned(
    reader: &Reader<'_>,
    heap: &Heap,
    database: u32,
    query: &Query,
) -> Option<(Option<JoinOrder>, HashTables)> {
    let members = Members { reader, error: RefCell::new(None), heap, database };
    let barrier = |view: u32| {
        reader.catalog().and_then(|c| c.relation(view)).is_some_and(rupg_analyze::security_barrier)
    };
    let fold = |sub: &Query, qual| rupg_exec::fold_qual(sub, qual, reader);
    let mut query = rupg_opt::pull_up(query, &barrier, &fold);
    rupg_opt::remove_joins(&mut query, &members);
    let tree = rupg_opt::plan(&query, &members);
    if members.error.take().is_some() {
        return None;
    }
    Some((rupg_opt::join_order(&tree, &query, &members), rupg_opt::hash_tables(&tree, &query)))
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
        plan.start_explain(reader)?;
        // `ExecutorStart` opens the result relations of the write with `CheckValidResultRel`.
        if let Some(info) = plan.write() {
            self.check_result_relations(info)?;
        }
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
        let fold = |sub: &Query, qual| rupg_exec::fold_qual(sub, qual, reader);
        let mut query = rupg_opt::pull_up(plan.query(), &barrier, &fold);
        rupg_opt::remove_joins(&mut query, &members);
        rupg_opt::remove_self_joins(&mut query, &members);
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

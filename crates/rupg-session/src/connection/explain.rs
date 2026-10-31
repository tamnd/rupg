//! `EXPLAIN`, as `ExplainQuery` of `explain.c` runs it: the options, the plan of the query and the rows of its output. `EXPLAIN ANALYZE` is not supported yet.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_analyze::{BoolOp, Expr, ExprKind, Params, Query};
use rupg_common::{Error, SqlState};
use rupg_func::Session;
use rupg_opt::{Enable, Index, Member, Size};
use rupg_pgcatalog::builtin;
use rupg_plan::{Format, HashTables, Options, Serialized};
use rupg_sql::nodes::{DefElem, ExecuteStmt, ExplainStmt, Node, SelectStmt};
use rupg_types::{Value, oid};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::extended::protocol_error;
use super::{Connection, Done};
use crate::guc;
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
pub(crate) fn btree_shape(
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
                // `RelationGetIndexExpressions` and `RelationGetIndexPredicate` fold the constant parts, as `eval_const_expressions` does, so that a condition can match them.
                let fold = |e: Expr| rupg_exec::fold_qual(&Query::default(), e, self.reader);
                let exprs = info.exprs.as_deref().map_or_else(Vec::new, |text| {
                    let exprs = rupg_analyze::node::read_list(text).unwrap_or_default();
                    exprs.into_iter().map(fold).collect()
                });
                // `make_ands_implicit`: the parts of the `AND` of the predicate.
                let predicate = info.predicate.as_deref().map(rupg_analyze::node::read);
                let predicate = match predicate.map(|p| p.map(fold)) {
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

    fn parallel(&self, table: u32) -> Option<Option<i32>> {
        let Some(catalog) = self.reader.catalog() else { return Some(None) };
        let Some(relation) = catalog.relation(table) else { return Some(None) };
        if catalog.is_temp_schema(relation.namespace) {
            return None;
        }
        Some(relation.options.iter().find_map(|text| {
            let (name, value) = text.split_once('=')?;
            (name == "parallel_workers").then(|| value.parse().ok()).flatten()
        }))
    }

    fn enable(&self) -> Enable {
        let settings = self.reader.settings();
        let on = |name: &str| settings.get(name).is_none_or(|v| v != "off");
        let number = |name: &str| match guc::find(name).map(|p| settings.setting(p)) {
            Some(guc::Setting::Int(value)) => Some(f64::from(value)),
            Some(guc::Setting::Real(value)) => Some(value),
            _ => None,
        };
        let count = |name: &str, default: i32| match guc::find(name).map(|p| settings.setting(p)) {
            Some(guc::Setting::Int(value)) => value,
            _ => default,
        };
        let work_mem = number("work_mem").unwrap_or(4096.0) * 1024.0;
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
            distinct_reordering: on("enable_distinct_reordering"),
            nestloop: on("enable_nestloop"),
            mergejoin: on("enable_mergejoin"),
            hashjoin: on("enable_hashjoin"),
            self_join_elimination: on("enable_self_join_elimination"),
            work_mem,
            hash_mem: work_mem * number("hash_mem_multiplier").unwrap_or(2.0),
            max_parallel_workers_per_gather: count("max_parallel_workers_per_gather", 2),
            parallel_setup_cost: number("parallel_setup_cost").unwrap_or(1000.0),
            parallel_tuple_cost: number("parallel_tuple_cost").unwrap_or(0.1),
            min_parallel_table_scan_size: count("min_parallel_table_scan_size", 1024),
            min_parallel_index_scan_size: count("min_parallel_index_scan_size", 64),
            parallel_leader_participation: on("parallel_leader_participation"),
            parallel_append: on("enable_parallel_append"),
            parallel_hash: on("enable_parallel_hash"),
            gathermerge: on("enable_gathermerge"),
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
    settings: bool,
    generic: bool,
    /// `TIMING`, which is on with `ANALYZE` when the statement does not give it.
    timing: bool,
    /// True for `CREATE TABLE AS ... WITH NO DATA`, whose plan does not run.
    no_data: bool,
    /// `BUFFERS`, which is on with `ANALYZE` when the statement does not give it.
    buffers: bool,
    /// `IO`.
    io: bool,
    /// `SERIALIZE`: true for `BINARY` and false for `TEXT`.
    serialize: Option<bool>,
    /// True for `CREATE TABLE AS` and `SELECT INTO`, whose rows go into the new table and not to the client.
    into: bool,
    /// The query identifier of the statement, from its text.
    query_id: i64,
    /// True for `DECLARE ... SCROLL CURSOR`, whose plan gets a `Material` node when it cannot scan backward.
    scroll: bool,
}

/// `ParseExplainOptionList`: the options of the statement and the checks of the options that need `ANALYZE`.
fn parse_options(stmt: &ExplainStmt) -> Result<Parsed, Error> {
    let mut parsed = Parsed::default();
    let mut wal = false;
    let mut timing = None;
    let mut buffers = None;
    for def in stmt.options.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = usize::try_from(def.location).ok();
        match name {
            "analyze" => parsed.analyze = def_boolean(def)?,
            "verbose" => parsed.verbose = def_boolean(def)?,
            "costs" => parsed.costs = Some(def_boolean(def)?),
            "buffers" => buffers = Some(def_boolean(def)?),
            "settings" => parsed.settings = def_boolean(def)?,
            "io" => parsed.io = def_boolean(def)?,
            "wal" => wal = def_boolean(def)?,
            "generic_plan" => parsed.generic = def_boolean(def)?,
            "timing" => timing = Some(def_boolean(def)?),
            "summary" => parsed.summary = Some(def_boolean(def)?),
            "memory" => parsed.memory = def_boolean(def)?,
            "serialize" => {
                parsed.serialize = match def.arg {
                    None => Some(false),
                    Some(_) => match def_string(def)?.as_str() {
                        "off" | "none" => None,
                        "text" => Some(false),
                        "binary" => Some(true),
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
    if parsed.io && !parsed.analyze {
        return Err(needs("IO"));
    }
    if parsed.serialize.is_some() && !parsed.analyze {
        return Err(needs("SERIALIZE"));
    }
    parsed.timing = timing.unwrap_or(parsed.analyze);
    parsed.buffers = buffers.unwrap_or(parsed.analyze);
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

/// The text of the statement `CREATE` of `EXPLAIN`: the text from its first keyword `CREATE` out of the parentheses of the options, or all the text when it has none.
fn inner_text(text: &str) -> &str {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ if depth == 0
                && (i == 0 || !word(bytes[i - 1]))
                && text.get(i..i + 6).is_some_and(|w| w.eq_ignore_ascii_case("create"))
                && !bytes.get(i + 6).copied().is_some_and(word) =>
            {
                return &text[i..];
            }
            _ => {}
        }
        i += 1;
    }
    text
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
        text: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let mut parsed = parse_options(stmt)?;
        parsed.query_id = rupg_sql::query_id(text);
        let Some(node) = &stmt.query else {
            return Err(Error::internal("EXPLAIN with no statement"));
        };
        if parsed.analyze {
            match node {
                // `ExplainOnePlan` runs the plan of `CREATE TABLE AS` into the new table. The check of `IF NOT EXISTS` comes first, and a table that exists gives a notice and no plan.
                // A materialized view keeps the text of its statement, so `define` gets the text of `CREATE` without `EXPLAIN`.
                Node::CreateTableAsStmt(ctas) => {
                    let before = notices.len();
                    self.define(node, inner_text(text), notices)?;
                    if notices[before..]
                        .iter()
                        .any(|n| n.error.message().ends_with("already exists, skipping"))
                    {
                        let mut done = explain_rows(String::new(), parsed.format);
                        if let Done::Rows { rows, .. } = &mut done {
                            rows.clear();
                        }
                        return Ok(done);
                    }
                    parsed.no_data = ctas.into.as_ref().is_some_and(|into| into.skipData);
                    parsed.into = true;
                }
                // `SELECT ... INTO` runs as `CREATE TABLE AS`.
                Node::SelectStmt(select) if select.intoClause.is_some() => {
                    self.define(node, text, notices)?;
                    parsed.into = true;
                }
                _ => {}
            }
        }
        // `ExplainOneUtility`: `CREATE TABLE AS` and `DECLARE CURSOR` show the plan of their query.
        // `SELECT ... INTO` shows the plan of its query.
        let stripped;
        let inner = match node {
            Node::SelectStmt(s) if s.intoClause.is_some() => {
                stripped =
                    Node::SelectStmt(Box::new(SelectStmt { intoClause: None, ..(**s).clone() }));
                Some(&stripped)
            }
            Node::CreateTableAsStmt(s) => s.query.as_ref(),
            Node::DeclareCursorStmt(s) => {
                parsed.scroll = s.options & rupg_sql::nodes::CURSOR_OPT_SCROLL != 0;
                s.query.as_ref()
            }
            Node::ExecuteStmt(s) => return self.explain_execute(s, &parsed, notices),
            _ => Some(node),
        };
        if let Some(Node::ExecuteStmt(s)) = inner {
            return self.explain_execute(s, &parsed, notices);
        }
        let Some(inner) = inner.filter(|n| query::is_query(n)) else {
            return Err(not_yet("EXPLAIN of this statement"));
        };
        let started = self.clock.monotonic();
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        // `transformExplainStmt`: with `GENERIC_PLAN`, the query can use parameters, and the analysis gets their types from the query as `setup_parse_variable_parameters` does. The plan keeps them, as a generic plan does.
        let params = Params { types: Vec::new(), variable: parsed.generic };
        // `ExplainQuery`: `QueryRewrite` gives the statements of the rules, and each one shows its plan. A blank line comes between the plans of the text form.
        let Some(products) = rupg_analyze::query_rewrite(inner, &reader)? else {
            let plan = query::plan(inner, &reader, &params);
            let text = self.explain_plan(plan, &parsed, started, &reader, notices)?;
            return Ok(explain_rows(text, parsed.format));
        };
        // `ExplainBeginOutput` and `ExplainEndOutput` of the form JSON put the plans in one array, and `ExplainSeparatePlans` puts a blank line between the plans of the text form.
        if products.len() != 1 && !matches!(parsed.format, Format::Text | Format::Json) {
            return Err(not_yet("EXPLAIN of a statement that rules rewrite in this format"));
        }
        let mut texts = Vec::with_capacity(products.len());
        for product in &products {
            let plan = rupg_analyze::analyze_product(product, &reader, &params)
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
        self.explain_with(&plan, None, None, parsed, started, reader, notices)
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
        let custom = statement.count_plan(reader.settings().get("plan_cache_mode").as_deref());
        // A generic plan keeps the parameters, and the run gets their values.
        let folded = custom.then_some(params.as_slice());
        let text =
            self.explain_with(plan, folded, Some(&params), parsed, started, &reader, notices)?;
        Ok(explain_rows(text, parsed.format))
    }

    /// The text of the plan `plan`. A custom plan takes the values `folded` of its parameters as constants, and the run with `ANALYZE` gets the values `params`.
    #[allow(clippy::too_many_arguments)]
    fn explain_with(
        &self,
        plan: &rupg_exec::Plan,
        folded: Option<&[Value]>,
        params: Option<&[Value]>,
        parsed: &Parsed,
        started: std::time::Duration,
        reader: &Reader<'_>,
        notices: &mut Vec<Notice>,
    ) -> Result<String, Error> {
        // `MemoryContextMemConsumed` of the context of the planning.
        let mark = rupg_platform::alloc::Mark::now();
        let plan = plan.fold(folded, reader);
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
        let mut tree = rupg_opt::plan(&query, &members);
        // `standard_planner`: a cursor with `SCROLL` reads its rows from a `Material` node when the plan cannot scan backward.
        if parsed.scroll && !plan.backward_scan() {
            rupg_opt::materialize_finished(&mut tree);
        }
        let memory = parsed.memory.then(|| mark.since());
        if let Some(error) = members.error.take() {
            return Err(error);
        }
        let summary = parsed.summary.unwrap_or(parsed.analyze);
        let planning_time = summary.then(|| {
            let spent = self.clock.monotonic().saturating_sub(started);
            spent.as_secs_f64() * 1000.0
        });
        // `ExplainOnePlan` with `ANALYZE`: the run of the plan gives the counts of its nodes.
        let mut execution_time = None;
        let mut serialize = None;
        let actuals = if parsed.analyze {
            let run = self.clock.monotonic();
            let params = params.unwrap_or_default();
            // The rows of `CREATE TABLE AS` go into the table, so the output has no bytes.
            serialize =
                parsed.serialize.map(|binary| Serialized { binary, ..Serialized::default() });
            let mut actuals = if parsed.no_data {
                rupg_plan::Actuals::new()
            } else if plan.write().is_none() && !plan.modifying_cte() {
                let actuals = rupg_exec::analyze(&tree, params, reader, true)?;
                if let Some(serialized) = serialize.as_mut().filter(|_| !parsed.into) {
                    self.serialize_rows(&plan, params, reader, serialized)?;
                }
                actuals
            } else {
                rupg_plan::Actuals::new()
            };
            if !parsed.no_data && (plan.write().is_some() || plan.modifying_cte()) {
                let (counts, returned, merged) =
                    self.analyze_write(plan, &tree, params, reader, notices)?;
                actuals = counts;
                // The rows of `RETURNING` are in the text form after the write.
                if let Some(serialized) = &mut serialize {
                    serialized.bytes = returned.iter().map(data_row_bytes).sum();
                }
                if let Some(actual) = actuals.get_mut(&rupg_plan::node_key(&tree.root))
                    && matches!(tree.root.kind, rupg_plan::Kind::Modify { .. })
                {
                    actual.rows = returned.len() as u64;
                    if matches!(
                        tree.root.kind,
                        rupg_plan::Kind::Modify { operation: rupg_plan::Operation::Merge, .. }
                    ) {
                        actual.merged = Some(merged);
                    }
                }
            }
            let spent = self.clock.monotonic().saturating_sub(run);
            execution_time = summary.then_some(spent.as_secs_f64() * 1000.0);
            Some(actuals)
        } else {
            None
        };
        let options = Options {
            verbose: parsed.verbose,
            costs: parsed.costs.unwrap_or(true),
            format: parsed.format,
            planning_time,
            analyze: parsed.analyze,
            timing: parsed.timing,
            execution_time,
            buffers: parsed.buffers,
            io_timing: reader.settings().get("track_io_timing").as_deref() == Some("on"),
            io: parsed.io.then(|| stream_capacity(reader)),
            serialize,
            memory,
            settings: parsed.settings.then(|| {
                let settings = reader.settings().explain();
                settings.into_iter().map(|(name, value)| (name.to_string(), value)).collect()
            }),
            query_id: (parsed.verbose
                && reader.settings().get("compute_query_id").as_deref() == Some("on"))
            .then_some(parsed.query_id),
        };
        rupg_func::explain(&tree, &options, actuals.as_ref(), reader)
    }
}

impl Connection {
    /// `ExplainOnePlan` with `ANALYZE` of a statement that writes, with its triggers and the statements of `WITH` that change data. The statements of `WITH` run first, as `ExecInitNode` and the first read of their rows run them. The run of the tree `tree` gives the counts of its nodes with the snapshot before the write of the main query, and then the write runs as `ExecutorRun` runs it. It gives the counts, the number of rows of `RETURNING` and the counts of the actions of a `MERGE`.
    fn analyze_write(
        &self,
        plan: rupg_exec::Plan,
        tree: &rupg_plan::Plan<'_>,
        params: &[Value],
        reader: &Reader<'_>,
        notices: &mut Vec<Notice>,
    ) -> Result<(rupg_plan::Actuals, Vec<query::Row>, [u64; 3]), Error> {
        let Some(view) = reader.view().cloned() else {
            return Err(Error::internal("the reader of the session has no view of the rows"));
        };
        self.merged.set([0; 3]);
        let (plan, writes) = self.start_writes(plan, params, &view)?;
        let actuals = plan.analyze_tree(tree, params, reader)?;
        let returned = if plan.write().is_some() {
            let rows = plan.run(params, reader);
            notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
            let done = self.write_rows(&plan, params, &view, rows?, &[], writes);
            notices.extend(self.trigger_notices());
            match done? {
                Done::Rows { rows, .. } => rows,
                _ => Vec::new(),
            }
        } else {
            let ended = self.end_writes(writes, params, Vec::new());
            notices.extend(self.trigger_notices());
            ended?;
            Vec::new()
        };
        Ok((actuals, returned, self.merged.get()))
    }

    /// `serializeAnalyzeReceive`: a run of the plan for the rows that a client gets, with the bytes of their `DataRow` messages and the time of the output functions.
    fn serialize_rows(
        &self,
        plan: &rupg_exec::Plan,
        params: &[Value],
        reader: &Reader<'_>,
        serialized: &mut Serialized,
    ) -> Result<(), Error> {
        let rows = plan.run(params, reader)?;
        let types: Vec<u32> = plan.columns().iter().map(|t| t.expr.ty).collect();
        let formats = vec![i16::from(serialized.binary); types.len()];
        let started = self.clock.monotonic();
        for values in &rows {
            serialized.bytes += data_row_bytes(&query::encode(&types, values, reader, &formats)?);
        }
        serialized.time = self.clock.monotonic().saturating_sub(started).as_secs_f64() * 1000.0;
        Ok(())
    }
}

/// The bytes of the `DataRow` message of a row without its type byte and its length word: the count of the columns, then the length and the bytes of each value.
fn data_row_bytes(row: &query::Row) -> u64 {
    2 + row.iter().map(|value| 4 + value.as_ref().map_or(0, |v| v.len() as u64)).sum::<u64>()
}

/// `read_stream_begin_impl`: the most buffers that the read stream of a scan pins, `(effective_io_concurrency + 1) * io_combine_limit`, and at most the share of `shared_buffers` of one process that `GetPinLimit` gives.
fn stream_capacity(reader: &Reader<'_>) -> i64 {
    let settings = reader.settings();
    let int = |name: &str| {
        settings
            .get(name)
            .and_then(|text| guc::find(name)?.parse_int(&text).ok())
            .map_or(0, i64::from)
    };
    // `MaxBackends` with the 2 `NUM_SPECIAL_WORKER_PROCS`, then the 38 `NUM_AUXILIARY_PROCS`.
    let backends = int("max_connections")
        + int("autovacuum_worker_slots")
        + int("max_worker_processes")
        + int("max_wal_senders")
        + 2;
    let pins = int("shared_buffers") / (backends + 38);
    ((int("effective_io_concurrency") + 1) * int("io_combine_limit"))
        .min(pins)
        .clamp(1, i64::from(i16::MAX))
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

//! `gincostestimate`: the costs of a scan of a `gin` index, from the entries that `extractQuery` of the operator class finds in the constants of the clauses.

use rupg_analyze::{Expr, ExprKind};
use rupg_types::{JsonItem, JsonTree, QueryItem, TSQUERY_NOT, TSQUERY_OR, Value};

use super::{Clause, ClauseKind, PAGE_CPU_MULTIPLIER, Stats, Table};
use super::{and_of, implied_by, index_rows, pages_fetched, predicate};
use crate::{Index, cost};

/// `array_ops` of `gin`.
const ARRAY_OPS: u32 = 2745;
/// `tsvector_ops` of `gin`.
const TSVECTOR_OPS: u32 = 3659;
/// `jsonb_ops` of `gin`.
const JSONB_OPS: u32 = 4036;
/// `jsonb_path_ops` of `gin`.
const JSONB_PATH_OPS: u32 = 4037;

/// The search modes of `extractQuery`.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// `GIN_SEARCH_MODE_DEFAULT`.
    Default,
    /// `GIN_SEARCH_MODE_INCLUDE_EMPTY`.
    IncludeEmpty,
    /// `GIN_SEARCH_MODE_ALL`.
    All,
}

/// `GinQualCounts`.
#[derive(Default)]
struct Counts {
    full: Vec<bool>,
    normal: Vec<bool>,
    partial: f64,
    exact: f64,
    search: f64,
    array_scans: f64,
}

impl Counts {
    fn new(columns: usize) -> Counts {
        Counts {
            full: vec![false; columns],
            normal: vec![false; columns],
            array_scans: 1.0,
            ..Counts::default()
        }
    }

    /// One ordinary entry for an operand that is not a constant.
    fn unknown(&mut self) {
        self.exact += 1.0;
        self.search += 1.0;
    }
}

/// `gincostestimate`: the start cost, the total cost and the selectivity of the scan of the `gin` index with the clauses. The statistics of the metapage are the estimates of `gincostestimate` for an index whose statistics it cannot trust: 90% of the pages, and 10 pages at least, are entry pages, the rest are data pages, and each entry page has 100 entries.
pub(super) fn gin_cost(
    table: &Table<'_, '_>,
    index: &Index,
    clauses: &[Clause<'_>],
    stats: &Stats,
) -> (f64, f64, f64) {
    let rows = index_rows(table, index);
    let pages = index.pages.max(10.0);
    let entry_pages = (pages * 0.90).floor();
    let data_pages = pages - entry_pages;
    let entries = (entry_pages * 100.0).floor().max(1.0);
    let quals: Vec<&Expr> = clauses.iter().map(|c| &*c.expr).collect();
    let given = and_of(&quals);
    let parts = predicate(index, table.r);
    let extra = parts.iter().filter(|p| !implied_by(&given, p)).cloned();
    let all: Vec<Expr> = extra.chain(quals.into_iter().cloned()).collect();
    let selectivity = cost::list_selectivity_by(&all, table.eq);
    let mut counts = Counts::new(index.keys.len());
    for clause in clauses {
        let family = index.families.get(clause.column).copied().unwrap_or(0);
        let operand = clause.other().into_iter().next().map(constant);
        let possible = match clause.kind {
            ClauseKind::Op(s) => match operand {
                Some(Some(value)) => opexpr(&mut counts, clause.column, family, s, value),
                Some(None) | None => {
                    counts.unknown();
                    true
                }
            },
            ClauseKind::Array(s, items) => match operand {
                Some(Some(value)) => array(&mut counts, clause.column, family, s, value, entries),
                Some(None) | None => {
                    counts.unknown();
                    counts.array_scans *= items;
                    true
                }
            },
            ClauseKind::Null(_) => true,
        };
        if !possible {
            return (0.0, 0.0, 0.0);
        }
    }
    let full = counts.full.iter().zip(&counts.normal).any(|(f, n)| *f && !*n);
    if full || clauses.is_empty() {
        counts.partial = 0.0;
        counts.exact = entries;
        counts.search = entries;
    }
    let (search, scans, outer) = (counts.search, counts.array_scans, table.loops);
    let page_cpu = PAGE_CPU_MULTIPLIER * cost::CPU_OPERATOR;
    let mut entry_fetched = (search * entry_pages.powf(0.15).round_ties_even()).ceil();
    let partial_scale = (counts.partial / entries).min(1.0);
    entry_fetched += (entry_pages * partial_scale).ceil();
    let mut data_fetched = (data_pages * partial_scale).ceil();
    let (mut startup, mut total) = (0.0, 0.0);
    if entries > 1.0 {
        let descent = (entries.ln() / 2.0_f64.ln()).ceil() * cost::CPU_OPERATOR;
        startup += descent * search;
        total += scans * descent * search;
    }
    startup += entry_fetched * page_cpu;
    total += entry_fetched * scans * page_cpu;
    startup += page_cpu * data_fetched;
    total += data_fetched * (scans - 1.0) * page_cpu;
    let cached = |fetched: f64, pages: f64| {
        if outer > 1.0 || scans > 1.0 {
            pages_fetched(fetched * outer * scans, pages, pages, table.pages) / outer
        } else {
            fetched
        }
    };
    entry_fetched = cached(entry_fetched, entry_pages);
    data_fetched = cached(data_fetched, data_pages);
    startup += (entry_fetched + data_fetched) * cost::random_page();
    // Each entry has the same items, and an item pointer takes 3 bytes.
    data_fetched = (data_pages * counts.exact / entries).ceil();
    data_fetched = data_fetched.max((selectivity * (rows / f64::from(8192 / 3))).ceil());
    startup += page_cpu * search;
    total += data_fetched * scans * page_cpu;
    data_fetched = cached(data_fetched, data_pages);
    total += startup + data_fetched * cost::random_page();
    let others = clauses.iter().flat_map(Clause::other);
    let qual_arg: f64 = others.map(|o| stats.qual_costs([o])).map(|(s, p)| s + p).sum();
    let qual_op = cost::CPU_OPERATOR * clauses.len() as f64;
    startup += qual_arg;
    total += qual_arg;
    total += search * scans * qual_op;
    total += rows * selectivity * cost::CPU_INDEX_TUPLE;
    (startup, total, selectivity)
}

/// The value of a constant operand, through a relabeling, or `None` for another operand.
fn constant(expr: &Expr) -> Option<&Value> {
    match &expr.kind {
        ExprKind::Const(value) => Some(value),
        ExprKind::Relabel(arg, _) => constant(arg),
        _ => None,
    }
}

/// `gincost_opexpr`: the entries of the constant of a clause. A null constant matches nothing.
fn opexpr(counts: &mut Counts, column: usize, family: u32, strategy: i16, value: &Value) -> bool {
    if matches!(value, Value::Null) {
        return false;
    }
    pattern(counts, column, family, strategy, value)
}

/// `gincost_scalararrayopexpr`: the average entries of the items of the array, with a scan of the index for each item that can match. A null item matches nothing, and an item that needs a scan of the whole index has each entry of the index.
fn array(
    counts: &mut Counts,
    column: usize,
    family: u32,
    strategy: i16,
    value: &Value,
    entries: f64,
) -> bool {
    let Value::Array(items) = value else { return false };
    let columns = counts.full.len();
    let (mut partial, mut exact, mut search, mut possible) = (0.0, 0.0, 0.0, 0.0);
    for item in items.values.iter().flatten() {
        let mut one = Counts::new(columns);
        if !pattern(&mut one, column, family, strategy, item) {
            continue;
        }
        possible += 1.0;
        if one.full[column] && !one.normal[column] {
            one.partial = 0.0;
            one.exact = entries;
            one.search = entries;
        }
        partial += one.partial;
        exact += one.exact;
        search += one.search;
    }
    if possible == 0.0 {
        return false;
    }
    counts.partial += partial / possible;
    counts.exact += exact / possible;
    counts.search += search / possible;
    counts.array_scans *= possible;
    true
}

/// `gincost_pattern`: the entries that `extractQuery` of the operator class gives for the constant, each one true for a partial match, which counts as 100 entries. No entries in the default mode match nothing. A class that the planner does not know gives one ordinary entry.
fn pattern(counts: &mut Counts, column: usize, family: u32, strategy: i16, value: &Value) -> bool {
    let Some((found, mode)) = extract(family, strategy, value) else {
        counts.unknown();
        counts.normal[column] = true;
        return true;
    };
    if found.is_empty() && mode == Mode::Default {
        return false;
    }
    for partial in found {
        if partial {
            counts.partial += 100.0;
        } else {
            counts.exact += 1.0;
        }
        counts.search += 1.0;
    }
    match mode {
        Mode::Default => counts.normal[column] = true,
        Mode::IncludeEmpty => {
            counts.normal[column] = true;
            counts.exact += 1.0;
            counts.search += 1.0;
        }
        Mode::All => counts.full[column] = true,
    }
    true
}

/// `extractQuery` of the classes `array_ops` (`ginqueryarrayextract`), `tsvector_ops` (`gin_extract_tsquery`), `jsonb_ops` (`gin_extract_jsonb_query`) and `jsonb_path_ops` (`gin_extract_jsonb_query_path`): the entries of the query with their partial matches, and the search mode. `None` for another class and for a `jsonpath`.
fn extract(family: u32, strategy: i16, value: &Value) -> Option<(Vec<bool>, Mode)> {
    match (family, value) {
        (ARRAY_OPS, Value::Array(a)) => {
            let n = a.values.len();
            let mode = match strategy {
                // `@>`: everything contains the empty set.
                2 if n == 0 => Mode::All,
                // `<@`: the empty set is in everything.
                3 => Mode::IncludeEmpty,
                4 if n == 0 => Mode::IncludeEmpty,
                _ => Mode::Default,
            };
            Some((vec![false; n], mode))
        }
        (TSVECTOR_OPS, Value::TsQuery(items)) => {
            if items.is_empty() {
                return Some((Vec::new(), Mode::Default));
            }
            let mode = if requires_match(items, 0) { Mode::Default } else { Mode::All };
            let found = items
                .iter()
                .filter_map(|i| match i {
                    QueryItem::Operand { prefix, .. } => Some(*prefix),
                    QueryItem::Operator { .. } => None,
                })
                .collect();
            Some((found, mode))
        }
        (JSONB_OPS | JSONB_PATH_OPS, Value::Text(text)) if strategy == 7 => {
            let tree = JsonTree::parse(text).ok()?;
            let n = json_entries(&tree, 0, family == JSONB_OPS);
            Some((vec![false; n], if n == 0 { Mode::All } else { Mode::Default }))
        }
        // `?`: the key.
        (JSONB_OPS, Value::Text(_)) if strategy == 9 => Some((vec![false], Mode::Default)),
        // `?|` and `?&`: the keys that are not null.
        (JSONB_OPS, Value::Array(a)) if matches!(strategy, 10 | 11) => {
            let n = a.values.iter().flatten().count();
            let mode = if n == 0 && strategy == 11 { Mode::All } else { Mode::Default };
            Some((vec![false; n], mode))
        }
        _ => None,
    }
}

/// `tsquery_requires_match`: a row must have a lexeme of the query to match it. `!` needs no lexeme, `|` needs one of its two sides, and `&` and `<N>` need one of them.
fn requires_match(items: &[QueryItem], at: usize) -> bool {
    match items.get(at) {
        Some(QueryItem::Operand { .. }) => true,
        Some(QueryItem::Operator { op, left, .. }) => {
            let right = at + 1;
            let left = at + *left as usize;
            match *op {
                TSQUERY_NOT => false,
                TSQUERY_OR => requires_match(items, right) && requires_match(items, left),
                _ => requires_match(items, right) || requires_match(items, left),
            }
        }
        None => false,
    }
}

/// The entries of `gin_extract_jsonb` of a value: each key and each scalar for `jsonb_ops`, and each scalar for `jsonb_path_ops`, whose entry is the hash of the path to the scalar.
fn json_entries(tree: &JsonTree<'_>, at: usize, keys: bool) -> usize {
    match tree.item(at) {
        JsonItem::Array(elements) => elements.iter().map(|e| json_entries(tree, *e, keys)).sum(),
        JsonItem::Object(fields) => {
            let values: usize = fields.iter().map(|(_, v)| json_entries(tree, *v, keys)).sum();
            values + if keys { fields.len() } else { 0 }
        }
        _ => 1,
    }
}

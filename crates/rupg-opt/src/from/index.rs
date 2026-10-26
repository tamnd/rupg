//! `create_index_paths` for one table: the scans of the table by its btree and hash indexes, as `build_index_paths` makes them, with the costs of `btcostestimate`, `genericcostestimate` and `cost_index`. The correlation of the first column of an index comes from the statistics of the column.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rupg_analyze::{BoolOp, Expr, ExprKind, FuncForm, Var, types};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node, STATISTIC_KIND_CORRELATION};
use rupg_types::{Array, Inet, Value, oid};

use super::classes::Joined;
use super::{OrderKey, Qual, Rel, Stats, plain_var};
use crate::{Index, Level, cost, exprs};

/// `BTREE_AM_OID`, the OID of the access method `btree`.
pub(super) const BTREE_AM: u32 = 403;

/// `HASH_AM_OID`, the OID of the access method `hash`.
const HASH_AM: u32 = 405;

/// True for an index of an access method whose scans the planner makes: `btree` and `hash`.
fn scannable(index: &Index) -> bool {
    matches!(index.method, BTREE_AM | HASH_AM)
}
/// `BTEqualStrategyNumber`.
const EQUAL: i16 = 3;
/// `TEXT_PATTERN_BTREE_FAM_OID`, the operator family `text_pattern_ops` of btree.
const TEXT_PATTERN_FAMILY: u32 = 2095;
/// `BPCHAR_PATTERN_BTREE_FAM_OID`, the operator family `bpchar_pattern_ops` of btree.
const BPCHAR_PATTERN_FAMILY: u32 = 2097;
/// `C_COLLATION_OID`, the collation of the type `name`.
const C_COLLATION: u32 = 950;
/// `INDOPTION_DESC`.
const DESC: i16 = 1;
/// `INDOPTION_NULLS_FIRST`.
const NULLS_FIRST: i16 = 2;
/// `DEFAULT_PAGE_CPU_MULTIPLIER`: the cost of a page of the descent of a btree, in units of `cpu_operator_cost`.
const PAGE_CPU_MULTIPLIER: f64 = 50.0;
/// `effective_cache_size`: 4 GB of pages of 8 kB.
const CACHE_PAGES: f64 = 524_288.0;

/// The table of a scan, with the estimates that the costs of its index scans read.
pub(super) struct Table<'a, 'q> {
    pub(super) r: usize,
    /// `pages` of the table.
    pub(super) pages: f64,
    /// `tuples` of the table.
    pub(super) tuples: f64,
    /// `rows` of the table: the rows that pass its conditions.
    pub(super) rows: f64,
    /// The conditions of the scan in the order of `baserestrictinfo`, and then for a parameterized scan the conditions of the joins that it tests, as `ppi_clauses`.
    pub(super) quals: Vec<Qual<'q>>,
    /// The selectivity of `=` of two values, as [`cost::list_selectivity_by`] reads it.
    pub(super) eq: &'a dyn cost::Estimate,
    /// `required_outer` of a parameterized scan: the relations of the values that it reads.
    pub(super) outer: BTreeSet<usize>,
    /// `loop_count`: the scans of the table that a nested loop makes, as `get_loop_count` finds them.
    pub(super) loops: f64,
}

/// The tables of the parameterized scans of a table, as `get_baserel_parampathinfo` gives them for each set of outer relations.
pub(super) type Tables<'f, 'a, 'q> = dyn Fn(&BTreeSet<usize>) -> Table<'a, 'q> + 'f;

/// The cost of an item of a bitmap in units of `cpu_operator_cost`, which `cost_bitmap_tree_node` charges for each row of an index scan.
const BITMAP_ROW_CPU: f64 = 0.1;
/// The cost of a `BitmapAnd` for each bitmap after the first, in units of `cpu_operator_cost`, as `cost_bitmap_and_node` charges it. `cost_bitmap_or_node` charges it for each bitmap after the first that is not the bitmap of one index.
const BITMAP_LIST_CPU: f64 = 100.0;

/// A condition that a column of an index can test, as an item of `indexquals` of an `IndexClause`: the condition with the column on the left, the key column of the index, the condition as the query has it, the place of the condition in the conditions of the scan and its equivalence class. A condition in an `OR` has no place. A lossy clause comes from a condition that the index cannot test exactly, so the condition stays in the filter.
#[derive(Clone)]
struct Clause<'q> {
    expr: Cow<'q, Expr>,
    column: usize,
    orig: Cow<'q, Expr>,
    at: Option<usize>,
    class: Option<usize>,
    kind: ClauseKind,
    lossy: bool,
}

impl Clause<'_> {
    /// `is_redundant_with_indexclauses`: true when a clause that is not lossy is the condition at the place, or comes from the same equivalence class.
    fn enforces(clauses: &[Clause<'_>], at: usize, qual: &Qual<'_>) -> bool {
        clauses.iter().any(|c| {
            !c.lossy && (c.at == Some(at) || qual.class.is_some() && c.class == qual.class)
        })
    }
}

/// A condition that an index can test, as a `RestrictInfo`: a condition of the scan with its place, or a condition in an `OR` of the conditions of the scan with no place. A condition of a join has its equivalence class.
struct Source<'q> {
    expr: Cow<'q, Expr>,
    at: Option<usize>,
    class: Option<usize>,
}

impl<'q> Source<'q> {
    /// The arguments of the `AND` or the `OR` of the condition, or `None` when the condition is not one.
    fn args(&self, op: BoolOp) -> Option<Vec<Source<'q>>> {
        let source = |expr| Source { expr, at: None, class: None };
        match &self.expr {
            Cow::Borrowed(e) => match &e.kind {
                ExprKind::Bool(o, args) if *o == op => {
                    Some(args.iter().map(|a| source(Cow::Borrowed(a))).collect())
                }
                _ => None,
            },
            Cow::Owned(e) => match &e.kind {
                ExprKind::Bool(o, args) if *o == op => {
                    Some(args.iter().map(|a| source(Cow::Owned(a.clone()))).collect())
                }
                _ => None,
            },
        }
    }
}

impl Clause<'_> {
    /// The value that the column is compared with, which `index_other_operands_eval_cost` costs, or `None` for `IS NULL` and `IS NOT NULL`.
    fn other(&self) -> Option<&Expr> {
        match &self.expr.kind {
            ExprKind::ScalarArrayOp { args, .. } => args.get(1),
            ExprKind::NullTest(..) => None,
            _ => exprs::binary(&self.expr).map(|(_, _, right)| right),
        }
    }
}

#[derive(Clone, Copy)]
enum ClauseKind {
    /// An operator of the operator family of the column, with its strategy.
    Op(i16),
    /// `x op ANY (array)`, with the strategy of the operator and the items of the array as `estimate_array_length` finds them.
    Array(i16, f64),
    /// `IS NULL` when true, `IS NOT NULL` when false.
    Null(bool),
}

/// A scan of an index that a bitmap can use, as an `IndexPath` that `get_index_paths` puts in `bitindexpaths`: the index, its clauses, the total cost of the scan of the index, `indextotalcost`, the selectivity of the clauses, `indexselectivity`, and the total cost of the scan of the table by the index, `total_cost` of the path. A parameterized scan reads the values of the relations of `outer` and gives the rows of the parameterization.
#[derive(Clone)]
struct BitmapIndex<'q> {
    index: u32,
    clauses: Vec<Clause<'q>>,
    total: f64,
    selectivity: f64,
    path: f64,
    rows: f64,
    /// `pages` of the index.
    pages: f64,
    outer: BTreeSet<usize>,
    /// The parts of the predicate of a partial index.
    predicate: Vec<Expr>,
}

impl<'q> BitmapIndex<'q> {
    /// The scan of the index with the clauses, with the costs of `cost_index`.
    fn new(
        table: &Table<'_, 'q>,
        index: &Index,
        clauses: Vec<Clause<'q>>,
        costs: (f64, f64, f64),
        stats: &Stats,
    ) -> BitmapIndex<'q> {
        let (_, path, _) = index_scan_cost(table, index, &clauses, costs, false, stats);
        let (_, total, selectivity) = costs;
        let (rows, pages, outer) = (table.rows, index.pages, table.outer.clone());
        let predicate = predicate(index, table.r);
        let index = index.oid;
        BitmapIndex { index, clauses, total, selectivity, path, rows, pages, outer, predicate }
    }
}

/// A bitmap of the places of the rows of the table: the scan of an index, the `BitmapAnd` of bitmaps, as a `BitmapAndPath`, or the `BitmapOr` of bitmaps, as a `BitmapOrPath`.
#[derive(Clone)]
enum Bitmap<'q> {
    Index(BitmapIndex<'q>),
    And(Vec<Bitmap<'q>>),
    Or(Vec<Bitmap<'q>>),
}

impl<'q> Bitmap<'q> {
    /// `total_cost` of the path of the bitmap: the cost of the scan of the table by the index for the scan of one index, and the cost of the bitmap for a `BitmapAnd` and a `BitmapOr`.
    fn total(&self) -> f64 {
        match self {
            Bitmap::Index(b) => b.path,
            _ => self.tree().0,
        }
    }

    /// The places of the clauses of [`reseat`] for each scan of an index of the bitmap.
    fn reseat(&mut self, table: &Table<'_, '_>, base: usize) {
        match self {
            Bitmap::Index(b) => reseat(&mut b.clauses, table, base),
            Bitmap::And(list) | Bitmap::Or(list) => {
                list.iter_mut().for_each(|b| b.reseat(table, base))
            }
        }
    }

    /// `PATH_REQ_OUTER`: the relations whose values the scans of the indexes of the bitmap read.
    fn outer(&self) -> BTreeSet<usize> {
        match self {
            Bitmap::Index(b) => b.outer.clone(),
            Bitmap::And(list) | Bitmap::Or(list) => list.iter().flat_map(Bitmap::outer).collect(),
        }
    }

    /// `get_indexpath_pages`: the pages of the indexes of the scans of the bitmap.
    fn pages(&self) -> f64 {
        match self {
            Bitmap::Index(b) => b.pages,
            Bitmap::And(list) | Bitmap::Or(list) => list.iter().map(Bitmap::pages).sum(),
        }
    }

    /// `cost_bitmap_tree_node`: the cost of the bitmap and the selectivity of its conditions. A scan of an index costs its index and a little for each of its rows. A `BitmapAnd` and a `BitmapOr` cost their bitmaps, as `cost_bitmap_and_node` and `cost_bitmap_or_node`.
    fn tree(&self) -> (f64, f64) {
        match self {
            Bitmap::Index(b) => {
                (b.total + BITMAP_ROW_CPU * cost::CPU_OPERATOR * b.rows, b.selectivity)
            }
            Bitmap::And(list) => and_tree(list),
            Bitmap::Or(list) => {
                let mut total = 0.0;
                let mut selectivity = 0.0;
                for (i, b) in list.iter().enumerate() {
                    let (cost, s) = b.tree();
                    selectivity += s;
                    total += cost;
                    if i > 0 && !matches!(b, Bitmap::Index(_)) {
                        total += BITMAP_LIST_CPU * cost::CPU_OPERATOR;
                    }
                }
                (total, f64::min(selectivity, 1.0))
            }
        }
    }

    /// `find_indexpath_quals`: the conditions as the query has them of each scan of an index in the bitmap.
    fn quals<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Bitmap::Index(b) => out.extend(b.clauses.iter().map(|c| &*c.orig)),
            Bitmap::And(list) | Bitmap::Or(list) => list.iter().for_each(|b| b.quals(out)),
        }
    }

    /// The clauses that the scans of the indexes at the top of the bitmap, or below a `BitmapAnd` at the top, test, which take the conditions that they test exactly out of the filter.
    fn exact<'a>(&'a self, out: &mut Vec<&'a Clause<'q>>) {
        match self {
            Bitmap::Index(b) => out.extend(&b.clauses),
            Bitmap::And(list) => list.iter().for_each(|b| b.exact(out)),
            Bitmap::Or(_) => {}
        }
    }
}

/// `cost_bitmap_and_node`: the cost of the `BitmapAnd` of the bitmaps and the product of their selectivities. One bitmap costs itself.
fn and_tree(list: &[Bitmap<'_>]) -> (f64, f64) {
    let mut total = 0.0;
    let mut selectivity = 1.0;
    for (i, b) in list.iter().enumerate() {
        let (cost, s) = b.tree();
        selectivity *= s;
        total += cost;
        if i > 0 {
            total += BITMAP_LIST_CPU * cost::CPU_OPERATOR;
        }
    }
    (total, selectivity)
}

/// `build_index_paths` for each btree index of the table with no predicate, the newest index first: a scan in the order of the index, and a scan in the other order when its order is useful. A scan is an index only scan when the index has each column of the table that the query reads, as `check_index_only`. Each scan has the order of its rows that a node above can use, as `truncate_useless_pathkeys` finds it. The forward scans that test a condition or that give no useful order are the scans that a bitmap can use. With the `BitmapOr` bitmaps of the conditions that are an `OR`, the end of `create_index_paths` makes the bitmap heap scan of the best of them.
pub(super) fn paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    indexes: &[Index],
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let (mut out, bitmaps) = index_paths(level, table, indexes, stats);
    if !bitmaps.is_empty() {
        let chosen = choose_bitmap_and(table, bitmaps, None, stats);
        out.push(bitmap_heap_scan(level, table, &chosen, stats));
    }
    out
}

/// The scans of [`paths`] that are not bitmap heap scans, and the bitmaps of the table, `bitindexpaths`.
fn index_paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    indexes: &[Index],
    stats: &Stats,
) -> (Vec<Rel<'q>>, Vec<Bitmap<'q>>) {
    let mut out = Vec::new();
    let mut bitmaps = Vec::new();
    let read = read_columns(table, stats);
    let sources: Vec<Source<'q>> = table
        .quals
        .iter()
        .enumerate()
        .map(|(at, q)| Source { expr: q.expr.clone(), at: Some(at), class: q.class })
        .collect();
    let all: Vec<&Source<'q>> = sources.iter().collect();
    let quals: Vec<&Expr> = table.quals.iter().map(|q| &*q.expr).collect();
    // `get_relation_info` puts each index first in `indexlist`, so the newest index comes first.
    for index in indexes.iter().rev() {
        if !scannable(index) || !predicate_ok(index, table.r, &quals) {
            continue;
        }
        let clauses = match_clauses(table.r, index, &restrict(index, table.r, &all));
        let costs = index_cost(table, index, &clauses, stats);
        let only = crate::enable().indexonlyscan
            && index.method == BTREE_AM
            && read.iter().all(|a| *a > 0 && index.columns.contains(a));
        let forward = order(level, table.r, index, false, stats);
        let useful = stats.useful(&BTreeSet::from([table.r]), &forward);
        if !clauses.is_empty() || !useful.is_empty() || only {
            // A backward scan has the clauses and the costs of the forward scan, so `choose_bitmap_and` keeps the forward scan.
            if useful.is_empty() || costs.2 < 1.0 {
                let path = BitmapIndex::new(table, index, clauses.clone(), costs, stats);
                bitmaps.push(Bitmap::Index(path));
            }
            out.push(scan(level, table, index, &clauses, costs, only, false, useful, stats));
        }
        let backward = order(level, table.r, index, true, stats);
        let useful = stats.useful(&BTreeSet::from([table.r]), &backward);
        if !useful.is_empty() {
            out.push(scan(level, table, index, &clauses, costs, only, true, useful, stats));
        }
    }
    bitmaps.extend(or_paths(table, indexes, &sources, &[], stats));
    (out, bitmaps)
}

/// `reparameterize_path` of a scan of the table `plain` with no parameters, for the parameterized table `table`: an index scan keeps its index and its clauses and gets the costs of `cost_index` again, and a bitmap heap scan keeps its bitmap, as `create_bitmap_heap_path` does. The conditions of the joins go into the filter, and the rows are the rows of the parameterization. Other scans give `None`.
pub(super) fn reparameterize<'q>(
    level: &Level<'q>,
    (plain, table): (&Table<'_, 'q>, &Table<'_, 'q>),
    indexes: &[Index],
    node: &Node<'q>,
    stats: &Stats,
) -> Option<Node<'q>> {
    match &node.kind {
        Kind::IndexScan { index, only, backward, .. } => {
            let index = indexes.iter().find(|i| i.oid == *index)?;
            let sources: Vec<Source<'q>> = plain
                .quals
                .iter()
                .enumerate()
                .map(|(at, q)| Source { expr: q.expr.clone(), at: Some(at), class: q.class })
                .collect();
            let all: Vec<&Source<'q>> = sources.iter().collect();
            let clauses = match_clauses(plain.r, index, &restrict(index, plain.r, &all));
            let costs = index_cost(table, index, &clauses, stats);
            let order = Vec::new();
            Some(scan(level, table, index, &clauses, costs, *only, *backward, order, stats).node)
        }
        Kind::BitmapHeapScan { .. } => {
            let (_, bitmaps) = index_paths(level, plain, indexes, stats);
            if bitmaps.is_empty() {
                return None;
            }
            let chosen = choose_bitmap_and(plain, bitmaps, None, stats);
            Some(bitmap_heap_scan(level, table, &chosen, stats).node)
        }
        _ => None,
    }
}

/// A parameterized scan of a table for the inner side of a nested loop, as an `IndexPath` or a `BitmapHeapPath` with a `param_info`.
#[derive(Debug)]
pub(super) struct Param<'q> {
    pub(super) rel: Rel<'q>,
    /// `ppi_req_outer`: the relations of the values that the scan reads.
    pub(super) outer: BTreeSet<usize>,
    /// `ppi_clauses`: the conditions of the joins that the scan tests, which the nested loop does not test again.
    pub(super) enforced: Vec<Qual<'q>>,
    /// `has_indexed_join_quals`: true when the index of the scan tests each condition of `enforced`.
    pub(super) indexed: bool,
}

/// `clause_relids` of the condition of a clause: the relations that it reads and the table.
fn clause_rels(r: usize, clause: &Clause<'_>) -> BTreeSet<usize> {
    let mut rels = exprs::relations(&clause.orig);
    rels.insert(r);
    rels
}

/// `has_indexed_join_quals` for the clauses of the index of a scan: true when the table has a condition of a join and the clauses test each of them.
fn indexed(table: &Table<'_, '_>, base: usize, clauses: &[Clause<'_>]) -> bool {
    let joins = table.quals.iter().enumerate().skip(base);
    base < table.quals.len() && joins.clone().all(|(at, q)| Clause::enforces(clauses, at, q))
}

/// The places of the clauses of the conditions of joins in the conditions of the parameterized table, which come after the `base` conditions of the table.
fn reseat(clauses: &mut [Clause<'_>], table: &Table<'_, '_>, base: usize) {
    for clause in clauses {
        let join = exprs::relations(&clause.orig).iter().any(|o| *o != table.r);
        if join {
            let found = table.quals[base..].iter().position(|q| q.expr.same(&clause.orig));
            clause.at = found.map(|at| at + base);
        }
    }
}

/// The parameterized scans of the table for the conditions of the joins, as the part of `create_index_paths` after `match_join_clauses_to_index`. For each btree index with no predicate, the newest index first, the conditions are the conditions of the joins in `joins` that the table can test, `match_join_clauses_to_index`, and the conditions that the equivalence classes make for each column of the index, `match_eclass_clauses_to_index`. `consider_index_join_clauses` chooses the sets of relations of the values that the scans read, and for each set `get_join_index_paths` makes the scans with the conditions of the set. The bitmaps of the scans, with the bitmaps of the scans that are not parameterized, make a bitmap heap scan for each set of relations, as the end of `create_index_paths`.
#[allow(clippy::too_many_arguments)]
pub(super) fn param_paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    indexes: &[Index],
    joins: &[Qual<'q>],
    classes: &[Joined<'q>],
    prohibited: &BTreeSet<usize>,
    ppi: &Tables<'_, '_, 'q>,
    stats: &Stats,
) -> Vec<Param<'q>> {
    let r = table.r;
    let base = table.quals.len();
    let mut out = Vec::new();
    let mut bitjoin: Vec<Bitmap<'q>> = Vec::new();
    let sources: Vec<Source<'q>> = table
        .quals
        .iter()
        .enumerate()
        .map(|(at, q)| Source { expr: q.expr.clone(), at: Some(at), class: q.class })
        .collect();
    let all: Vec<&Source<'q>> = sources.iter().collect();
    let join_sources: Vec<Source<'q>> =
        joins.iter().map(|q| Source { expr: q.expr.clone(), at: None, class: q.class }).collect();
    let all_joins: Vec<&Source<'q>> = join_sources.iter().collect();
    let quals: Vec<&Expr> = table.quals.iter().map(|q| &*q.expr).collect();
    for index in indexes.iter().rev() {
        if !scannable(index) || !predicate_ok(index, r, &quals) {
            continue;
        }
        let rclauses = match_clauses(r, index, &restrict(index, r, &all));
        let jclauses = match_clauses(r, index, &all_joins);
        // `generate_implied_equalities_for_column`: the conditions of the first class with a member that is the column.
        let mut implied = Vec::new();
        for (column, (&attnum, &family)) in index.keys.iter().zip(&index.families).enumerate() {
            // `ec_member_matches_indexcol`: a key column that is an expression matches the same expression.
            let indexed = if attnum == 0 { key_expr(index, r, column) } else { None };
            let matches = |m: &Expr, families: &[u32]| {
                families.contains(&family)
                    && match &indexed {
                        Some(indexed) => m.same(indexed),
                        None => plain_var(m).is_some_and(|v| {
                            v.relation == r && v.attnum == attnum && v.levels_up == 0
                        }),
                    }
            };
            let found = classes.iter().map(|c| c.column(r, matches, prohibited));
            if let Some(quals) = found.into_iter().find(|q| !q.is_empty()) {
                implied.extend(quals);
            }
        }
        let implied: Vec<Source<'q>> = implied
            .into_iter()
            .map(|q| Source { expr: q.expr, at: None, class: q.class })
            .collect();
        let eclauses = match_clauses(r, index, &implied.iter().collect::<Vec<_>>());
        if jclauses.is_empty() && eclauses.is_empty() {
            continue;
        }
        // `consider_index_join_clauses` and `consider_index_join_outer_rels`.
        let mut considered: Vec<BTreeSet<usize>> = Vec::new();
        let mut count = 0;
        for column in 0..index.keys.len() {
            for list in [&jclauses, &eclauses] {
                let here: Vec<&Clause<'q>> = list.iter().filter(|c| c.column == column).collect();
                count += here.len();
                for clause in &here {
                    let rels = clause_rels(r, clause);
                    if considered.contains(&rels) {
                        continue;
                    }
                    for pos in 0..considered.len() {
                        let old = &considered[pos];
                        if rels.is_subset(old) || old.is_subset(&rels) {
                            continue;
                        }
                        // `eclass_already_used`.
                        let used = clause.class.is_some()
                            && here.iter().any(|o| {
                                o.class == clause.class && clause_rels(r, o).is_subset(old)
                            });
                        if used {
                            continue;
                        }
                        if considered.len() >= 10 * count {
                            break;
                        }
                        let union: BTreeSet<usize> = rels.union(old).copied().collect();
                        if !considered.contains(&union) {
                            considered.push(union);
                        }
                    }
                    if !considered.contains(&rels) {
                        considered.push(rels);
                    }
                }
            }
        }
        let read = read_columns(table, stats);
        let only = crate::enable().indexonlyscan
            && index.method == BTREE_AM
            && read.iter().all(|a| *a > 0 && index.columns.contains(a));
        let forward = order(level, r, index, false, stats);
        let useful = stats.useful(&BTreeSet::from([r]), &forward);
        let backward = order(level, r, index, true, stats);
        let useful_backward = stats.useful(&BTreeSet::from([r]), &backward);
        // `get_join_index_paths`: for each column, the conditions of joins of the set, the first condition of a class of the set, and the conditions of the table.
        for set in &considered {
            let mut clauses: Vec<Clause<'q>> = Vec::new();
            for column in 0..index.keys.len() {
                let of_set =
                    |c: &&Clause<'q>| c.column == column && clause_rels(r, c).is_subset(set);
                clauses.extend(jclauses.iter().filter(of_set).cloned());
                clauses.extend(eclauses.iter().find(of_set).cloned());
                clauses.extend(rclauses.iter().filter(|c| c.column == column).cloned());
            }
            let mut outer: BTreeSet<usize> =
                clauses.iter().flat_map(|c| clause_rels(r, c)).collect();
            outer.remove(&r);
            let t = ppi(&outer);
            reseat(&mut clauses, &t, base);
            let costs = index_cost(&t, index, &clauses, stats);
            let indexed = indexed(&t, base, &clauses);
            let enforced = t.quals[base..].to_vec();
            if useful.is_empty() || costs.2 < 1.0 {
                let path = BitmapIndex::new(&t, index, clauses.clone(), costs, stats);
                bitjoin.push(Bitmap::Index(path));
            }
            let rel = scan(level, &t, index, &clauses, costs, only, false, useful.clone(), stats);
            out.push(Param { rel, outer: outer.clone(), enforced: enforced.clone(), indexed });
            if !useful_backward.is_empty() {
                let o = useful_backward.clone();
                let rel = scan(level, &t, index, &clauses, costs, only, true, o, stats);
                out.push(Param { rel, outer: outer.clone(), enforced, indexed });
            }
        }
    }
    if bitjoin.is_empty() {
        return out;
    }
    let (_, bitmaps) = index_paths(level, table, indexes, stats);
    let mut outers: Vec<BTreeSet<usize>> = Vec::new();
    for b in &bitjoin {
        let o = b.outer();
        if !outers.contains(&o) {
            outers.push(o);
        }
    }
    for max in &outers {
        let set: Vec<Bitmap<'q>> =
            bitmaps.iter().chain(&bitjoin).filter(|b| b.outer().is_subset(max)).cloned().collect();
        let mut chosen = choose_bitmap_and(table, set, Some(ppi), stats);
        let outer = chosen.outer();
        if outer.is_empty() {
            continue;
        }
        let t = ppi(&outer);
        chosen.reseat(&t, base);
        let indexed = match &chosen {
            Bitmap::Index(b) => indexed(&t, base, &b.clauses),
            _ => false,
        };
        let rel = bitmap_heap_scan(level, &t, &chosen, stats);
        out.push(Param { rel, outer, enforced: t.quals[base..].to_vec(), indexed });
    }
    out
}

/// `generate_bitmap_or_paths`: a `BitmapOr` for each condition that is an `OR` whose arms each have a bitmap. The bitmap of an arm is the best of the scans of the indexes that can test the arm, with the other conditions too, as `build_paths_for_OR`. An arm that is an `AND` can use a `BitmapOr` of an `OR` in it. The arms that compare one column of an index with one operator are one group, as `group_similar_or_args`, and a group gives the bitmaps of [`group_paths`].
fn or_paths<'q>(
    table: &Table<'_, 'q>,
    indexes: &[Index],
    clauses: &[Source<'q>],
    other: &[&Source<'q>],
    stats: &Stats,
) -> Vec<Bitmap<'q>> {
    let all: Vec<&Source<'q>> = clauses.iter().chain(other.iter().copied()).collect();
    let mut out = Vec::new();
    for clause in clauses {
        let Some(arms) = clause.args(BoolOp::Or) else { continue };
        let (arms, grouped) = group_arms(table.r, indexes, arms);
        // `list_delete`: the conditions other than the first that is equal to the `OR`.
        let mut inner: Vec<&Source<'q>> = Vec::new();
        if grouped {
            inner = all.clone();
            if let Some(at) = inner.iter().position(|s| s.expr.same(&clause.expr)) {
                inner.remove(at);
            }
        }
        let mut list = Vec::new();
        for (arm, group) in arms {
            if group {
                let found = group_paths(table, indexes, &arm, &inner, stats);
                if found.is_empty() {
                    list.clear();
                    break;
                }
                list.extend(found);
                continue;
            }
            let found = match arm.args(BoolOp::And) {
                Some(parts) => {
                    let mut found = arm_paths(table, indexes, &parts, &all, stats);
                    found.extend(or_paths(table, indexes, &parts, &all, stats));
                    found
                }
                None => arm_paths(table, indexes, std::slice::from_ref(&arm), &all, stats),
            };
            if found.is_empty() {
                list.clear();
                break;
            }
            list.push(choose_bitmap_and(table, found, None, stats));
        }
        if !list.is_empty() {
            out.push(Bitmap::Or(list));
        }
    }
    out
}

/// The index of the arm of an `OR` as `OrArgIndexMatch`: the place of the index in the btree indexes, the newest first, the key column and the operator.
type ArmIndex = Option<(usize, usize, u32)>;

/// `group_similar_or_args`: the arms of an `OR`, with the arms that compare a column of an index with one operator and one value that reads no column of the table as one `OR` in the place of the first of them. The index is the first btree index, the newest first, that has the column. The arm is true for a group. The result is true when an arm has an index.
fn group_arms<'q>(
    r: usize,
    indexes: &[Index],
    arms: Vec<Source<'q>>,
) -> (Vec<(Source<'q>, bool)>, bool) {
    let key = |arm: &Source<'q>| -> ArmIndex {
        let (op, left, right) = exprs::binary(&arm.expr)?;
        let reads = |e: &Expr| exprs::relations(e).contains(&r);
        let (op, column) = if reads(right) && !reads(left) && !exprs::volatile(left) {
            (builtin::operator_by_oid(op)?.commutator, right)
        } else if reads(left) && !reads(right) && !exprs::volatile(right) {
            (op, left)
        } else {
            return None;
        };
        let var = plain_var(column).filter(|v| v.relation == r && v.levels_up == 0)?;
        if op == 0 {
            return None;
        }
        let btrees = indexes.iter().rev().filter(|i| i.method == BTREE_AM);
        btrees.enumerate().find_map(|(n, index)| {
            index.keys.iter().position(|k| *k == var.attnum).map(|column| (n, column, op))
        })
    };
    let keys: Vec<ArmIndex> = arms.iter().map(key).collect();
    if keys.iter().all(Option::is_none) {
        return (arms.into_iter().map(|a| (a, false)).collect(), false);
    }
    let mut groups: Vec<(ArmIndex, Vec<usize>)> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        match groups.iter().position(|(g, _)| k.is_some() && g == k) {
            Some(g) => groups[g].1.push(i),
            None => groups.push((*k, vec![i])),
        }
    }
    let mut arms: Vec<Option<Source<'q>>> = arms.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for (_, members) in groups {
        let mut list: Vec<Source<'q>> = members.iter().filter_map(|i| arms[*i].take()).collect();
        if list.len() == 1 {
            out.extend(list.pop().map(|a| (a, false)));
            continue;
        }
        let args = list.into_iter().map(|a| a.expr.into_owned()).collect();
        let expr = Expr::new(ExprKind::Bool(BoolOp::Or, args), oid::BOOL);
        out.push((Source { expr: Cow::Owned(expr), at: None, class: None }, true));
    }
    (out, true)
}

/// `make_bitmap_paths_for_or_group`: the bitmaps of a group of arms of an `OR`. The joint bitmap tests the group as `x op ANY (array)`, and the split bitmaps test each arm of the group. With no other conditions the joint bitmap wins when there is one. Otherwise the cheaper of the joint bitmap and the split bitmaps wins.
fn group_paths<'q>(
    table: &Table<'_, 'q>,
    indexes: &[Index],
    group: &Source<'q>,
    other: &[&Source<'q>],
    stats: &Stats,
) -> Vec<Bitmap<'q>> {
    let found = arm_paths(table, indexes, std::slice::from_ref(group), other, stats);
    let joint = (!found.is_empty()).then(|| choose_bitmap_and(table, found, None, stats));
    if other.is_empty()
        && let Some(joint) = joint
    {
        return vec![joint];
    }
    let mut split = Vec::new();
    for arm in group.args(BoolOp::Or).unwrap_or_default() {
        let found = arm_paths(table, indexes, std::slice::from_ref(&arm), other, stats);
        if found.is_empty() {
            split.clear();
            break;
        }
        split.push(choose_bitmap_and(table, found, None, stats));
    }
    match joint {
        None => split,
        Some(joint) if split.is_empty() => vec![joint],
        Some(joint) => {
            let split_cost: f64 = split.iter().map(Bitmap::total).sum();
            if joint.total() < split_cost { vec![joint] } else { split }
        }
    }
}

/// `build_paths_for_OR`: the scans for a bitmap of the btree indexes with no predicate that can test one of the conditions, the newest index first. The scan of an index tests the conditions and each other condition that the index can test.
fn arm_paths<'q>(
    table: &Table<'_, 'q>,
    indexes: &[Index],
    clauses: &[Source<'q>],
    other: &[&Source<'q>],
    stats: &Stats,
) -> Vec<Bitmap<'q>> {
    let mine: Vec<&Source<'q>> = clauses.iter().collect();
    let all: Vec<&Source<'q>> = clauses.iter().chain(other.iter().copied()).collect();
    let mut out = Vec::new();
    let given: Vec<&Expr> = all.iter().map(|s| &*s.expr).collect();
    for index in indexes.iter().rev() {
        if !scannable(index)
            || !predicate_ok(index, table.r, &given)
            || match_clauses(table.r, index, &mine).is_empty()
        {
            continue;
        }
        let clauses = match_clauses(table.r, index, &restrict(index, table.r, &all));
        let costs = index_cost(table, index, &clauses, stats);
        out.push(Bitmap::Index(BitmapIndex::new(table, index, clauses, costs, stats)));
    }
    out
}

/// The columns of the table that the query reads above the scan and in the conditions of the scan, as `check_index_only` finds them with `pull_varattnos`.
fn read_columns(table: &Table<'_, '_>, stats: &Stats) -> BTreeSet<i16> {
    let mut out: BTreeSet<i16> = stats
        .needed
        .iter()
        .filter(|(v, _, _)| v.relation == table.r && v.levels_up == 0)
        .map(|(v, _, _)| v.attnum)
        .collect();
    for qual in &table.quals {
        let mut columns = Vec::new();
        exprs::columns_at(&qual.expr, 0, &mut columns);
        for column in columns {
            if let ExprKind::Var(v) = column.kind
                && v.relation == table.r
            {
                out.insert(v.attnum);
            }
        }
    }
    out
}

/// The expression of the key column `column` of the index, which is an expression, with the columns of the table as columns of the relation `r`.
pub(super) fn key_expr(index: &Index, r: usize, column: usize) -> Option<Expr> {
    let at = index.keys.iter().take(column).filter(|k| **k == 0).count();
    let mut expr = index.exprs.get(at)?.clone();
    of_relation(&mut expr, r);
    Some(expr)
}

/// The parts of the predicate of the index, with the columns of the table as columns of the relation `r`.
fn predicate(index: &Index, r: usize) -> Vec<Expr> {
    let mut parts = index.predicate.clone();
    for part in &mut parts {
        of_relation(part, r);
    }
    parts
}

/// The expression with each column of the relation 0 as a column of the relation `r`.
fn of_relation(expr: &mut Expr, r: usize) {
    expr.each_mut(0, &mut |e: &mut Expr, depth| {
        if let ExprKind::Var(v) = &mut e.kind
            && v.relation == 0
            && v.levels_up == depth
        {
            v.relation = r;
        }
        false
    });
}

/// The `AND` of the conditions, as `predicate_implied_by` reads a list.
fn and_of(given: &[&Expr]) -> Expr {
    Expr::new(ExprKind::Bool(BoolOp::And, given.iter().map(|e| (*e).clone()).collect()), oid::BOOL)
}

/// `check_index_predicates`: true when the conditions imply each part of the predicate of the index, so that the planner can use the index. An index with no predicate is always usable.
fn predicate_ok(index: &Index, r: usize, given: &[&Expr]) -> bool {
    let given = and_of(given);
    predicate(index, r).iter().all(|p| implied_by(&given, p))
}

/// `indrestrictinfo`: the conditions of the table that the predicate of the index does not imply. A condition that the predicate implies is true for each row of the index, so the scan does not test it.
fn restrict<'s, 'q>(index: &Index, r: usize, sources: &[&'s Source<'q>]) -> Vec<&'s Source<'q>> {
    let parts = predicate(index, r);
    if parts.is_empty() {
        return sources.to_vec();
    }
    let refs: Vec<&Expr> = parts.iter().collect();
    let pred = and_of(&refs);
    sources.iter().copied().filter(|s| !implied_by(&pred, &s.expr)).collect()
}

/// `match_clauses_to_index`: the conditions that a key column of the index can test, each with the first column that can, as `match_clause_to_index`. The list is in the order of the columns, then in the order of the conditions, as `build_index_paths` makes `indexclauses`.
fn match_clauses<'q>(r: usize, index: &Index, sources: &[&Source<'q>]) -> Vec<Clause<'q>> {
    let mut out: Vec<Clause<'q>> = Vec::new();
    for source in sources {
        let found = (0..index.keys.len())
            .map(|column| match_clause(r, index, column, source))
            .find(|c| !c.is_empty());
        out.extend(found.into_iter().flatten());
    }
    // `amsearcharray` and `amsearchnulls` of `hash` are false: only operators make its clauses.
    if index.method == HASH_AM {
        out.retain(|c| matches!(c.kind, ClauseKind::Op(_)));
    }
    out.sort_by_key(|c| c.column);
    out
}

/// `match_clause_to_indexcol`: the condition as clauses of the key column of the index, when the column can test it. An operator of the operator family of the column must have the column on one side and on the other side a value that reads no column of the table and calls no volatile function. With the column on the right, the commutator of the operator must be in the family, and the clause is the condition with the commutator, as `commute_op_clause`. `x op ANY (array)` and `IS NULL` and `IS NOT NULL` of the column are clauses too. A pattern operator with the column on the left gives the lossy clauses of the fixed prefix of the pattern, as `match_pattern_prefix`. A network operator `<<`, `<<=`, `>>` or `>>=` with a constant network gives the lossy clauses of the range of the network, as `network_subset_support`.
fn match_clause<'q>(
    r: usize,
    index: &Index,
    column: usize,
    source: &Source<'q>,
) -> Vec<Clause<'q>> {
    match_one(r, index, column, source).unwrap_or_default()
}

/// The clauses of [`match_clause`], or `None` when the column cannot test the condition.
fn match_one<'q>(
    r: usize,
    index: &Index,
    column: usize,
    source: &Source<'q>,
) -> Option<Vec<Clause<'q>>> {
    let expr = &*source.expr;
    let (orig, at, class) = (&source.expr, source.at, source.class);
    let attnum = *index.keys.get(column)?;
    let family = *index.families.get(column)?;
    // The collation `C` of a column of `name` is the collation of each condition of the column.
    let name_collation = index.collations.get(column) == Some(&C_COLLATION);
    if attnum < 0
        || family == 0
        || !(name_collation || matches!(index.collations.get(column), Some(0 | 100) | None))
    {
        return None;
    }
    // `match_index_to_operand`: a key column that is an expression matches the same expression.
    let indexed = if attnum == 0 { Some(key_expr(index, r, column)?) } else { None };
    let is_column = |e: &Expr| {
        let found = match &indexed {
            Some(indexed) => e.same(indexed),
            None => plain_var(e)
                .is_some_and(|v| v.relation == r && v.attnum == attnum && v.levels_up == 0),
        };
        found && (!name_collation || e.ty == oid::NAME)
    };
    let other_ok = |e: &Expr| !exprs::relations(e).contains(&r) && !exprs::volatile(e);
    let clause = |expr: Cow<'q, Expr>, kind| {
        vec![Clause { expr, column, orig: orig.clone(), at, class, kind, lossy: false }]
    };
    match &expr.kind {
        ExprKind::ScalarArrayOp { func, any: true, args } => {
            let [left, array] = args.as_slice() else { return None };
            let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
            let op = cost::array_operator(*func, left.ty, element)?;
            if !is_column(left) || !other_ok(array) {
                return None;
            }
            let s = strategy(op.oid, family)?;
            let kind = ClauseKind::Array(s, cost::array_length(array));
            Some(clause(orig.clone(), kind))
        }
        ExprKind::NullTest(arg, is_null) => {
            is_column(arg).then(|| clause(orig.clone(), ClauseKind::Null(*is_null)))
        }
        // `match_orclause_to_indexcol`: each arm compares the column with one operator of the family and a value of one type.
        ExprKind::Bool(BoolOp::Or, arms) => {
            let mut first: Option<(u32, u32)> = None;
            let mut index_expr = None;
            let mut values = Vec::new();
            for arm in arms {
                let (op, left, right) = exprs::binary(arm)?;
                let (op, column_expr, value) = if is_column(left) && other_ok(right) {
                    (op, left, right)
                } else if is_column(right) && other_ok(left) {
                    (builtin::operator_by_oid(op)?.commutator, right, left)
                } else {
                    return None;
                };
                match first {
                    None => {
                        strategy(op, family)?;
                        if types::array_of(value.ty) == 0
                            || value.ty == oid::RECORD
                            || column_expr.ty == oid::RECORD
                        {
                            return None;
                        }
                        first = Some((op, value.ty));
                    }
                    Some(f) if f != (op, value.ty) => return None,
                    Some(_) => {}
                }
                index_expr = Some(column_expr);
                values.push(value);
            }
            let ((op, element), index_expr) = (first?, index_expr?);
            let array = array_of_values(element, values);
            let s = strategy(op, family)?;
            let kind = ClauseKind::Array(s, cost::array_length(&array));
            let func = builtin::operator_by_oid(op)?.code;
            let args = vec![index_expr.clone(), array];
            let expr = Expr::new(ExprKind::ScalarArrayOp { func, any: true, args }, oid::BOOL);
            Some(clause(Cow::Owned(expr), kind))
        }
        _ => {
            let (op, left, right) = exprs::binary(expr)?;
            let lossy = |(expr, s)| Clause {
                expr: Cow::Owned(expr),
                column,
                orig: orig.clone(),
                at,
                class,
                kind: ClauseKind::Op(s),
                lossy: true,
            };
            if is_column(left) && other_ok(right) {
                let Some(s) = strategy(op, family) else {
                    let derived = prefix_clauses(expr, family)
                        .or_else(|| network_clauses(expr, true, family))?;
                    return Some(derived.into_iter().map(lossy).collect());
                };
                return Some(clause(orig.clone(), ClauseKind::Op(s)));
            }
            if is_column(right) && other_ok(left) {
                if let Some(derived) = network_clauses(expr, false, family) {
                    return Some(derived.into_iter().map(lossy).collect());
                }
                let commutator = builtin::operator_by_oid(op)?.commutator;
                let s = strategy(commutator, family)?;
                let commuted = exprs::commute(expr)?;
                return Some(clause(Cow::Owned(commuted), ClauseKind::Op(s)));
            }
            None
        }
    }
}

/// `make_SAOP_expr`: the array of the values of the type. The array is a constant when each value is one, and an `ARRAY` of the values otherwise.
fn array_of_values(element: u32, values: Vec<&Expr>) -> Expr {
    let array = types::array_of(element);
    let constants: Option<Vec<Option<Value>>> = values
        .iter()
        .map(|v| match &v.kind {
            ExprKind::Const(Value::Null) => Some(None),
            ExprKind::Const(value) => Some(Some(value.clone())),
            _ => None,
        })
        .collect();
    let kind = match constants {
        Some(items) => ExprKind::Const(Value::Array(Box::new(Array::one(items)))),
        None => {
            let elements = values.into_iter().cloned().collect();
            ExprKind::Array { element, multidims: false, elements }
        }
    };
    Expr::new(kind, array)
}

/// `match_pattern_prefix`: the clauses that the fixed prefix of a positive `LIKE`, `ILIKE`, regular expression or `^@` gives, with their strategies, when the column is on the left and the pattern is a constant. A pattern that is all prefix gives `=` of the prefix. Another prefix gives `>=` of the prefix and `<` of the next string after the prefix, when one exists. The default collation is C, so the bounds hold for each operator family. Each operator must be in the family of the column.
fn prefix_clauses(expr: &Expr, family: u32) -> Option<Vec<(Expr, i16)>> {
    let (op, left, right) = exprs::binary(expr)?;
    let (prefix, exact) = cost::operator_prefix(op, right)?;
    let pattern = family == TEXT_PATTERN_FAMILY || family == BPCHAR_PATTERN_FAMILY;
    let (less, greater_equal) = if pattern { ("~<~", "~>=~") } else { ("<", ">=") };
    let (left_ty, right_ty) = match left.ty {
        oid::TEXT if family != BPCHAR_PATTERN_FAMILY => (oid::TEXT, oid::TEXT),
        oid::NAME if !pattern => (oid::NAME, oid::TEXT),
        oid::BPCHAR if family != TEXT_PATTERN_FAMILY => (oid::BPCHAR, oid::BPCHAR),
        oid::BYTEA if !pattern => (oid::BYTEA, oid::BYTEA),
        _ => return None,
    };
    let make = |name: &str, value: &[u8]| {
        let row =
            builtin::operators_named(name).find(|o| o.left == left_ty && o.right == right_ty)?;
        let s = strategy(row.oid, family)?;
        let ExprKind::Func(f) = &expr.kind else { return None };
        let mut func = f.clone();
        func.oid = row.code;
        func.form = FuncForm::Operator(row.oid);
        let value = if right_ty == oid::BYTEA {
            Value::Bytea(value.to_vec())
        } else {
            Value::Text(String::from_utf8_lossy(value).into_owned())
        };
        func.args = vec![left.clone(), Expr::new(ExprKind::Const(value), right_ty)];
        Some((Expr::new(ExprKind::Func(func), oid::BOOL), s))
    };
    if exact {
        return Some(vec![make("=", &prefix)?]);
    }
    let mut out = vec![make(greater_equal, &prefix)?];
    if let Some(greater) = cost::greater_bytes(&prefix, right_ty == oid::BYTEA)
        && let Some(clause) = make(less, &greater)
    {
        out.push(clause);
    }
    Some(out)
}

/// `network_subset_support` and `match_network_subset`: the clauses of the column for `column << value` and `column <<= value` of `inet` when `left` is true, and for `value >> column` and `value >>= column` when it is false, with their strategies. The value must be a constant that is not NULL. The clauses are `>` of `network_scan_first` of the value, or `>=` for `<<=` and `>>=`, and `<=` of `network_scan_last`: the network of the value, and its broadcast address with the whole mask. Each operator must be in the family of the column.
fn network_clauses(expr: &Expr, left: bool, family: u32) -> Option<Vec<(Expr, i16)>> {
    let (op, l, r) = exprs::binary(expr)?;
    let row = builtin::operator_by_oid(op)?;
    if row.left != oid::INET || row.right != oid::INET {
        return None;
    }
    let (eq, key, value) = match (row.name, left) {
        ("<<", true) => (false, l, r),
        ("<<=", true) => (true, l, r),
        (">>", false) => (false, r, l),
        (">>=", false) => (true, r, l),
        _ => return None,
    };
    let ExprKind::Const(Value::Inet(net)) = &value.kind else { return None };
    let first = net.masked(net.bits);
    let mut last = *net;
    for (n, byte) in last.addr.iter_mut().take(net.family.size()).enumerate() {
        let done = n.saturating_mul(8);
        let kept = usize::from(net.bits).saturating_sub(done).min(8);
        *byte |= 0xffu8.checked_shr(u32::try_from(kept).unwrap_or(8)).unwrap_or(0);
    }
    last.bits = net.family.max_bits();
    let make = |name: &str, bound: Inet| {
        let row =
            builtin::operators_named(name).find(|o| o.left == oid::INET && o.right == oid::INET)?;
        let s = strategy(row.oid, family)?;
        let ExprKind::Func(f) = &expr.kind else { return None };
        let mut func = f.clone();
        func.oid = row.code;
        func.form = FuncForm::Operator(row.oid);
        func.args = vec![key.clone(), Expr::new(ExprKind::Const(Value::Inet(bound)), oid::INET)];
        Some((Expr::new(ExprKind::Func(func), oid::BOOL), s))
    };
    Some(vec![make(if eq { ">=" } else { ">" }, first)?, make("<=", last)?])
}

/// `get_op_opfamily_strategy`: the strategy of a search operator in the operator family, or `None` when the family has not the operator.
fn strategy(op: u32, family: u32) -> Option<i16> {
    builtin::amops_of_operator(op)
        .find(|a| a.family == family && a.purpose == b's')
        .map(|a| a.strategy)
}

/// `build_index_pathkeys`: the order of the rows of a scan of the index, forward or backward. A key that adds nothing after the keys before it, such as a column that a condition makes a constant, does not count, as `pathkey_is_redundant`. A column that is an expression gives the expression. The order stops at a column whose operator class is not the default class of its type. An index of another access method has no order.
fn order(
    level: &Level<'_>,
    r: usize,
    index: &Index,
    backward: bool,
    stats: &Stats,
) -> Vec<OrderKey> {
    // `amcanorder`: only a `btree` index gives its rows in an order.
    if index.method != BTREE_AM {
        return Vec::new();
    }
    let relation = &level.query.relations[r];
    let mut out: Vec<OrderKey> = Vec::new();
    for (i, &attnum) in index.keys.iter().enumerate() {
        let expr = if attnum == 0 {
            key_expr(index, r, i)
        } else {
            let column = usize::try_from(attnum - 1).ok().and_then(|c| relation.columns.get(c));
            column.map(|column| {
                let var = Var { relation: r, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), column.ty);
                expr.typmod = column.typmod;
                expr
            })
        };
        let (Some(expr), Some(true)) = (expr, index.default_class.get(i).copied()) else {
            break;
        };
        let before: Vec<&Expr> = out.iter().map(|k| &k.exprs[0]).collect();
        if stats.redundant(&expr, &before) {
            continue;
        }
        let option = index.options.get(i).copied().unwrap_or(0);
        let desc = (option & DESC != 0) != backward;
        let nulls_first = (option & NULLS_FIRST != 0) != backward;
        let mut exprs = vec![expr.clone()];
        if let Some((members, _)) =
            stats.equal.iter().find(|(m, _)| m.iter().any(|e| e.same(&expr)))
        {
            exprs.extend(members.iter().filter(|m| !m.same(&expr)).cloned());
        }
        out.push(OrderKey { exprs, desc, nulls_first });
    }
    out
}

/// The scan of the table by the index with the clauses, as `create_index_path` and `create_indexscan_plan` make it. The conditions of the scan that are not clauses of the index are the filter, in the order of `order_qual_clauses`.
#[allow(clippy::too_many_arguments)]
fn scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    index: &Index,
    clauses: &[Clause<'q>],
    costs: (f64, f64, f64),
    only: bool,
    backward: bool,
    order: Vec<OrderKey>,
    stats: &Stats,
) -> Rel<'q> {
    let (startup, total, filter) = index_scan_cost(table, index, clauses, costs, only, stats);
    let cond = clauses.iter().map(|c| Item { expr: c.expr.clone(), below: false }).collect();
    let kind = Kind::IndexScan {
        relation: table.r,
        index: index.oid,
        only,
        backward,
        cond,
        columns: index.columns.clone(),
    };
    let mut node = Node::new(kind, level.scope, Vec::new());
    node.filter = filter.iter().map(|q| q.item()).collect();
    let width = stats.width(&BTreeSet::from([table.r]));
    node.cost = Cost { startup, total, rows: table.rows, width };
    node.disabled = !crate::enable().indexscan;
    Rel { node, rels: BTreeSet::from([table.r]), order }
}

/// `cost_index`: the start cost and the total cost of the scan of the table by the index with the clauses, and the conditions of the scan that are not clauses of the index, in the order of `order_qual_clauses`. The pages of the table cost between a random page for each row, as `index_pages_fetched` finds the pages, and one random page and then sequential pages for the fraction of the pages that the selectivity gives, by the square of the correlation of the index. Each row costs the conditions that are not clauses. A condition that a clause from the same equivalence class tests is not in the filter, as `extract_nonindex_conditions`.
fn index_scan_cost<'q>(
    table: &Table<'_, 'q>,
    index: &Index,
    clauses: &[Clause<'q>],
    costs: (f64, f64, f64),
    only: bool,
    stats: &Stats,
) -> (f64, f64, Vec<Qual<'q>>) {
    // `extract_nonindex_conditions` of `indrestrictinfo`: a condition that the predicate of the index implies is not in the filter.
    let parts = predicate(index, table.r);
    let refs: Vec<&Expr> = parts.iter().collect();
    let pred = (!parts.is_empty()).then(|| and_of(&refs));
    let mut filter: Vec<Qual<'q>> = table
        .quals
        .iter()
        .enumerate()
        .filter(|(at, q)| {
            !Clause::enforces(clauses, *at, q)
                && !pred.as_ref().is_some_and(|p| implied_by(p, &q.expr))
        })
        .map(|(_, q)| q.clone())
        .collect();
    stats.sort_quals(&mut filter);
    let (index_startup, index_total, selectivity) = costs;
    let mut startup = index_startup;
    let mut run = index_total - index_startup;
    let fetched = cost::clamp_rows(selectivity * table.tuples);
    // An index only scan does not read the pages that the visibility map marks all visible.
    let visible = if only { stats.all_visible.get(&table.r).copied().unwrap_or(0.0) } else { 0.0 };
    let unseen = |pages: f64| if only { (pages * (1.0 - visible)).ceil() } else { pages };
    let min_pages = (selectivity * table.pages).ceil();
    // A scan in a nested loop reads the pages that the other scans read from the cache.
    let (max_io, min_io) = if table.loops > 1.0 {
        let pages = pages_fetched(fetched * table.loops, table.pages, index.pages, table.pages);
        let min = pages_fetched(min_pages * table.loops, table.pages, index.pages, table.pages);
        let (pages, min) = (unseen(pages), unseen(min));
        (pages * cost::RANDOM_PAGE / table.loops, min * cost::RANDOM_PAGE / table.loops)
    } else {
        let pages = unseen(pages_fetched(fetched, table.pages, index.pages, table.pages));
        let min_pages = unseen(min_pages);
        let min = if min_pages > 0.0 {
            cost::RANDOM_PAGE + (min_pages - 1.0).max(0.0) * cost::SEQ_PAGE
        } else {
            0.0
        };
        (pages * cost::RANDOM_PAGE, min)
    };
    let correlation = correlation(table.r, index, stats);
    run += max_io + correlation * correlation * (min_io - max_io);
    let (qual_startup, qual_cost) = stats.qual_costs(filter.iter().map(|q| &*q.expr));
    startup += qual_startup;
    run += (cost::CPU_TUPLE + qual_cost) * fetched;
    (startup, startup + run, filter)
}

/// `btcost_correlation`: the correlation of the order of the first key column of the index with the order of the rows of the table, from the statistics of the column when the index has the default operator class of the column. A `DESC` column turns it around, and an index with more key columns has 0.75 of it.
fn correlation(r: usize, index: &Index, stats: &Stats) -> f64 {
    // `genericcostestimate` gives the other access methods no correlation.
    if index.method != BTREE_AM {
        return 0.0;
    }
    let (Some(&attnum), Some(true)) = (index.keys.first(), index.default_class.first().copied())
    else {
        return 0.0;
    };
    let statistic = stats.columns.get(&r).and_then(|columns| columns.get(&attnum));
    let slot =
        statistic.and_then(|s| s.slots.iter().find(|s| s.kind == STATISTIC_KIND_CORRELATION));
    let Some(&number) = slot.and_then(|s| s.numbers.as_deref()).and_then(<[f32]>::first) else {
        return 0.0;
    };
    let mut correlation = f64::from(number);
    if index.options.first().is_some_and(|o| o & 1 != 0) {
        correlation = -correlation;
    }
    if index.keys.len() > 1 { correlation * 0.75 } else { correlation }
}

/// `choose_bitmap_and`: the best bitmap of the bitmaps. Of the bitmaps with the same conditions, the cheapest stays. In the order of their costs, each bitmap starts a list, and each bitmap after it whose conditions are new goes in the list when the `BitmapAnd` makes the heap scan cheaper. The cheapest list wins.
fn choose_bitmap_and<'q>(
    table: &Table<'_, 'q>,
    mut paths: Vec<Bitmap<'q>>,
    ppi: Option<&Tables<'_, '_, 'q>>,
    stats: &Stats,
) -> Bitmap<'q> {
    if paths.len() == 1
        && let Some(path) = paths.pop()
    {
        return path;
    }
    // `classify_index_clause_usage`: each condition is the place of the first equal condition in the list of the conditions of the bitmaps.
    let mut known: Vec<Expr> = Vec::new();
    let mut kept: Vec<(Bitmap<'q>, BTreeSet<usize>)> = Vec::new();
    for path in paths {
        let mut quals = Vec::new();
        path.quals(&mut quals);
        let mut these = BTreeSet::new();
        for qual in quals {
            let at = match known.iter().position(|k| k.same(qual)) {
                Some(at) => at,
                None => {
                    known.push(qual.clone());
                    known.len() - 1
                }
            };
            these.insert(at);
        }
        match kept.iter().position(|(_, k)| *k == these) {
            Some(i) => {
                if path.tree().0 < kept[i].0.tree().0 {
                    kept[i] = (path, these);
                }
            }
            None => kept.push((path, these)),
        }
    }
    if kept.len() == 1
        && let Some((path, _)) = kept.pop()
    {
        return path;
    }
    // `path_usage_comparator`: by cost, then by selectivity. The sort keeps the order of equal bitmaps, as the insertion sort of `qsort` does for a short list.
    kept.sort_by(|(a, _), (b, _)| {
        let (a, b) = (a.tree(), b.tree());
        a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
    });
    // `bitmap_and_cost_est`: the heap scan of the list reads the values of the relations of the scans of its indexes.
    let heap = |list: &[Bitmap<'q>]| {
        let outer: BTreeSet<usize> = list.iter().flat_map(Bitmap::outer).collect();
        let param = ppi.filter(|_| !outer.is_empty()).map(|ppi| ppi(&outer));
        let table = param.as_ref().unwrap_or(table);
        let (startup, per_row) = stats.qual_costs(table.quals.iter().map(|q| &*q.expr));
        let (tree, selectivity) = and_tree(list);
        let pages: f64 = list.iter().map(Bitmap::pages).sum();
        heap_cost(table, tree, selectivity, pages, startup, per_row).1
    };
    let mut best: Vec<Bitmap<'q>> = Vec::new();
    let mut best_cost = 0.0;
    for (i, (first, first_ids)) in kept.iter().enumerate() {
        let mut list = vec![first.clone()];
        let mut so_far = heap(&list);
        let mut ids_so_far = first_ids.clone();
        for (path, these) in &kept[i + 1..] {
            if !these.is_disjoint(&ids_so_far) {
                continue;
            }
            list.push(path.clone());
            let cost = heap(&list);
            if cost < so_far {
                so_far = cost;
                ids_so_far.extend(these);
            } else {
                list.pop();
            }
        }
        if i == 0 || so_far < best_cost {
            best = list;
            best_cost = so_far;
        }
    }
    match best.len() {
        1 => best.remove(0),
        _ => Bitmap::And(best),
    }
}

/// `cost_bitmap_heap_scan` with the pages of `compute_bitmap_pages`: the start cost and the total cost of the heap scan of a bitmap that costs `tree`, passes the rows of `selectivity` and reads indexes of `index_pages`, when the conditions of the table cost `startup` to start and `per_row` for each row. The pages cost a random page each when the scan reads few of them and less when it reads more. A scan in a nested loop reads the pages that the other scans read from the cache. A bitmap of the 4 MB of `work_mem` holds the places of the pages of each table that the planner sees, so no page is lossy.
fn heap_cost(
    table: &Table<'_, '_>,
    tree: f64,
    selectivity: f64,
    index_pages: f64,
    startup: f64,
    per_row: f64,
) -> (f64, f64) {
    let fetched = cost::clamp_rows(selectivity * table.tuples);
    let t = if table.pages > 1.0 { table.pages } else { 1.0 };
    let mut pages = 2.0 * t * fetched / (2.0 * t + fetched);
    if table.loops > 1.0 {
        pages = pages_fetched(fetched * table.loops, table.pages, index_pages, table.pages)
            / table.loops;
    }
    let pages = if pages >= t { t } else { pages.ceil() };
    let per_page = if pages >= 2.0 {
        cost::RANDOM_PAGE - (cost::RANDOM_PAGE - cost::SEQ_PAGE) * (pages / t).sqrt()
    } else {
        cost::RANDOM_PAGE
    };
    let startup = tree + startup;
    let run = pages * per_page + (cost::CPU_TUPLE + per_row) * fetched;
    (startup, startup + run)
}

/// A condition of a bitmap as the query has it: the condition with its place in the conditions of the scan, or a condition that the bitmap makes, such as the `OR` of the conditions of a `BitmapOr`, with no place.
struct Orig<'q> {
    expr: Cow<'q, Expr>,
    at: Option<usize>,
}

/// `make_ands_explicit` or `make_orclause` of the conditions: the condition when there is one, or the `AND` or the `OR` of them.
fn combine<'q>(mut list: Vec<Orig<'q>>, op: BoolOp) -> Orig<'q> {
    if list.len() == 1
        && let Some(one) = list.pop()
    {
        return one;
    }
    let args = list.into_iter().map(|o| o.expr.into_owned()).collect();
    Orig { expr: Cow::Owned(Expr::new(ExprKind::Bool(op, args), oid::BOOL)), at: None }
}

/// `list_concat_unique`: the conditions of `more` that are not equal to a condition in `list` go at the end of it.
fn concat_unique<'q>(list: &mut Vec<Orig<'q>>, more: Vec<Orig<'q>>) {
    for o in more {
        if !list.iter().any(|l| l.expr.same(&o.expr)) {
            list.push(o);
        }
    }
}

/// `create_bitmap_subplan`: the plan of the bitmap, with its conditions as the query has them, `bitmapqualorig`, and the conditions that its indexes test, `indexquals`. The conditions of a `BitmapAnd` are those of its bitmaps, and a `BitmapOr` has the `OR` of the conditions of its bitmaps. A `BitmapOr` of one bitmap is that bitmap.
fn subplan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    bitmap: &Bitmap<'q>,
) -> (Node<'q>, Vec<Orig<'q>>, Vec<Orig<'q>>) {
    let (tree, selectivity) = bitmap.tree();
    let rows = cost::clamp_rows(selectivity * table.tuples);
    match bitmap {
        Bitmap::Index(b) => {
            let cond =
                b.clauses.iter().map(|c| Item { expr: c.expr.clone(), below: false }).collect();
            let kind = Kind::BitmapIndexScan { relation: table.r, index: b.index, cond };
            let mut node = Node::new(kind, level.scope, Vec::new());
            node.cost = Cost { startup: 0.0, total: b.total, rows, width: 0 };
            let mut quals: Vec<Orig<'q>> = Vec::new();
            for (i, c) in b.clauses.iter().enumerate() {
                // The lossy clauses of one condition are one `IndexClause`.
                let same = i > 0
                    && c.lossy
                    && b.clauses[i - 1].lossy
                    && b.clauses[i - 1].orig.same(&c.orig);
                if !same {
                    quals.push(Orig { expr: c.orig.clone(), at: c.at });
                }
            }
            let mut tested: Vec<Orig<'q>> =
                b.clauses.iter().map(|c| Orig { expr: c.expr.clone(), at: None }).collect();
            // `create_bitmap_subplan`: the parts of the predicate of a partial index that the clauses do not imply are conditions of the bitmap too.
            let given: Vec<&Expr> = b.clauses.iter().map(|c| &*c.orig).collect();
            let given = and_of(&given);
            for part in b.predicate.iter().filter(|p| !implied_by(&given, p)) {
                quals.push(Orig { expr: Cow::Owned(part.clone()), at: None });
                tested.push(Orig { expr: Cow::Owned(part.clone()), at: None });
            }
            (node, quals, tested)
        }
        Bitmap::And(list) => {
            let mut children = Vec::new();
            let mut quals = Vec::new();
            let mut tested = Vec::new();
            for b in list {
                let (child, q, t) = subplan(level, table, b);
                children.push(child);
                concat_unique(&mut quals, q);
                concat_unique(&mut tested, t);
            }
            let mut node = Node::new(Kind::BitmapAnd, level.scope, children);
            node.cost = Cost { startup: tree, total: tree, rows, width: 0 };
            (node, quals, tested)
        }
        Bitmap::Or(list) => {
            let mut children = Vec::new();
            let mut quals = Some(Vec::new());
            let mut tested = Some(Vec::new());
            for b in list {
                let (child, q, t) = subplan(level, table, b);
                children.push(child);
                // A bitmap with no condition makes the `OR` true.
                for (all, part) in [(&mut quals, q), (&mut tested, t)] {
                    if part.is_empty() {
                        *all = None;
                    } else if let Some(all) = all {
                        all.push(combine(part, BoolOp::And));
                    }
                }
            }
            let node = match children.len() {
                1 => children.remove(0),
                _ => {
                    let mut node = Node::new(Kind::BitmapOr, level.scope, children);
                    node.cost = Cost { startup: tree, total: tree, rows, width: 0 };
                    node
                }
            };
            let or = |list: Option<Vec<Orig<'q>>>| match list {
                Some(list) if list.len() > 1 => vec![combine(list, BoolOp::Or)],
                Some(list) => list,
                None => Vec::new(),
            };
            (node, or(quals), or(tested))
        }
    }
}

/// The items of an `AND` or an `OR`, as `predicate_classify`, or `None` for another condition.
fn bool_items(expr: &Expr) -> Option<(BoolOp, &[Expr])> {
    match &expr.kind {
        ExprKind::Bool(op @ (BoolOp::And | BoolOp::Or), args) => Some((*op, args)),
        _ => None,
    }
}

/// `predicate_implied_by_recurse` with no proofs of operators and no items of `ANY`: true when the condition `clause` makes the condition `pred` true by the form of the `AND` and the `OR` in them. A condition that is not an `AND` or an `OR` proves an equal condition.
fn implied_by(clause: &Expr, pred: &Expr) -> bool {
    match (bool_items(clause), bool_items(pred)) {
        (Some((BoolOp::And, _)), Some((BoolOp::And, p))) | (None, Some((BoolOp::And, p))) => {
            p.iter().all(|p| implied_by(clause, p))
        }
        (Some((BoolOp::And, c)), Some((_, p))) => {
            p.iter().any(|p| implied_by(clause, p)) || c.iter().any(|c| implied_by(c, pred))
        }
        (Some((BoolOp::And, c)), None) => c.iter().any(|c| implied_by(c, pred)),
        (Some((_, c)), Some((BoolOp::Or, p))) => {
            c.iter().all(|c| p.iter().any(|p| implied_by(c, p)))
        }
        (Some((_, c)), _) => c.iter().all(|c| implied_by(c, pred)),
        (None, Some((_, p))) => p.iter().any(|p| implied_by(clause, p)),
        (None, None) => clause.same(pred),
    }
}

/// `create_bitmap_heap_path` and `create_bitmap_scan_plan`: the heap scan of the bitmap. Its filter is each condition of the table that the bitmap does not test exactly, in the order of `order_qual_clauses`. A condition that the conditions of the indexes imply, such as an `OR` that a `BitmapOr` tests, is not in the filter. The recheck conditions are the conditions of the bitmap as the query has them, but not those in the filter.
fn bitmap_heap_scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    bitmap: &Bitmap<'q>,
    stats: &Stats,
) -> Rel<'q> {
    let (child, quals, tested) = subplan(level, table, bitmap);
    let mut exact = Vec::new();
    bitmap.exact(&mut exact);
    let exact: Vec<Clause<'q>> = exact.into_iter().cloned().collect();
    let tested: Vec<&Expr> = tested.iter().map(|o| &*o.expr).collect();
    // `predicate_implied_by`: a list of more than one condition is their `AND`.
    let given = match tested.as_slice() {
        [] => None,
        [one] => Some(Cow::Borrowed(*one)),
        _ => {
            let all = tested.iter().map(|e| (*e).clone()).collect();
            Some(Cow::Owned(Expr::new(ExprKind::Bool(BoolOp::And, all), oid::BOOL)))
        }
    };
    let in_filter: BTreeSet<usize> = (0..table.quals.len())
        .filter(|at| {
            let expr = &*table.quals[*at].expr;
            !Clause::enforces(&exact, *at, &table.quals[*at])
                && !tested.iter().any(|t| t.same(expr))
                && (exprs::volatile(expr) || !given.as_ref().is_some_and(|g| implied_by(g, expr)))
        })
        .collect();
    let mut filter: Vec<Qual<'q>> = in_filter.iter().map(|at| table.quals[*at].clone()).collect();
    stats.sort_quals(&mut filter);
    let (tree, selectivity) = bitmap.tree();
    let (qual_startup, per_row) = stats.qual_costs(table.quals.iter().map(|q| &*q.expr));
    let (startup, total) =
        heap_cost(table, tree, selectivity, bitmap.pages(), qual_startup, per_row);
    let recheck = quals
        .into_iter()
        .filter_map(|o| match o.at {
            Some(at) => (!in_filter.contains(&at)).then(|| table.quals[at].item()),
            None => Some(Item { expr: o.expr, below: false }),
        })
        .collect();
    let kind = Kind::BitmapHeapScan { relation: table.r, recheck };
    let mut node = Node::new(kind, level.scope, vec![child]);
    node.filter = filter.iter().map(|q| q.item()).collect();
    let width = stats.width(&BTreeSet::from([table.r]));
    node.cost = Cost { startup, total, rows: table.rows, width };
    node.disabled = !crate::enable().bitmapscan;
    Rel { node, rels: BTreeSet::from([table.r]), order: Vec::new() }
}

/// The costs of the scan of the index with the clauses, from the estimator of its access method.
fn index_cost(
    table: &Table<'_, '_>,
    index: &Index,
    clauses: &[Clause<'_>],
    stats: &Stats,
) -> (f64, f64, f64) {
    if index.method == HASH_AM {
        hash_cost(table, index, clauses, stats)
    } else {
        btree_cost(table, index, clauses, stats)
    }
}

/// `hashcostestimate`: `genericcostestimate` with the metapage as the one page that is not a leaf page. The rows of the index that the scan reads are the rows of the table that the clauses and the parts of the predicate that they do not imply select, and the scan has no descent.
fn hash_cost(
    table: &Table<'_, '_>,
    index: &Index,
    clauses: &[Clause<'_>],
    stats: &Stats,
) -> (f64, f64, f64) {
    let tuples = table.tuples;
    let quals: Vec<&Expr> = clauses.iter().map(|c| &*c.expr).collect();
    let given = and_of(&quals);
    let parts = predicate(index, table.r);
    let extra = parts.iter().filter(|p| !implied_by(&given, p)).cloned();
    let all: Vec<Expr> = extra.chain(quals.into_iter().cloned()).collect();
    let selectivity = cost::list_selectivity_by(&all, table.eq);
    let index_tuples = (selectivity * tuples).round_ties_even().min(tuples).max(1.0);
    let index_pages = if index.pages > 1.0 && tuples > 1.0 {
        (index_tuples * (index.pages - 1.0) / tuples).ceil()
    } else {
        1.0
    };
    let mut total = if table.loops > 1.0 {
        let all = index_pages * table.loops;
        let fetched = pages_fetched(all, index.pages, index.pages, table.pages);
        fetched * cost::RANDOM_PAGE / table.loops
    } else {
        index_pages * cost::RANDOM_PAGE
    };
    let others = clauses.iter().filter_map(Clause::other);
    let qual_arg: f64 = others.map(|o| stats.qual_costs([o])).map(|(s, p)| s + p).sum();
    let qual_op = cost::CPU_OPERATOR * clauses.len() as f64;
    total += qual_arg + index_tuples * (cost::CPU_INDEX_TUPLE + qual_op);
    (qual_arg, total, selectivity)
}

/// `btcostestimate` and `genericcostestimate`: the start cost and the total cost of the scan of the index, and the selectivity of its clauses. The rows of the index that the scan reads come from the clauses of the first columns: the clauses of each column up to the first column with no `=` clause, and the clauses of that column. A unique index with `=` on each key column reads one row. Each item of `ANY` is one more descent of the tree.
fn btree_cost(
    table: &Table<'_, '_>,
    index: &Index,
    clauses: &[Clause<'_>],
    stats: &Stats,
) -> (f64, f64, f64) {
    let mut column = 0;
    let mut equal_here = false;
    let mut found_array = false;
    let mut found_null = false;
    let mut scans: f64 = 1.0;
    let mut bound: Vec<&Expr> = Vec::new();
    for clause in clauses {
        if column < clause.column {
            if equal_here {
                column += 1;
            }
            equal_here = false;
            // A column with no clause would be a skip array, but a column with no statistics has the default number of distinct values, so the bound clauses stop.
            if column < clause.column {
                found_array = true;
                break;
            }
        }
        match clause.kind {
            ClauseKind::Op(s) => equal_here |= s == EQUAL,
            ClauseKind::Array(s, items) => {
                found_array = true;
                if items > 1.0 {
                    scans *= items;
                }
                equal_here |= s == EQUAL;
            }
            ClauseKind::Null(true) => {
                found_null = true;
                equal_here = true;
            }
            ClauseKind::Null(false) => {}
        }
        bound.push(&*clause.expr);
    }
    let tuples = table.tuples;
    // `get_relation_info` and `estimate_rel_size`: a partial index has the rows that fill its pages after the metapage, at most the rows of the table. Each row has the header of a heap row and its line pointer.
    let rows = if index.predicate.is_empty() {
        tuples
    } else {
        let density = f64::from((8192 - 24) / (index.width + 24 + 4).max(1));
        (density * (index.pages - 1.0).max(0.0)).round_ties_even().min(tuples)
    };
    // `add_predicate_to_index_quals`: the parts of the predicate that the clauses do not imply come first.
    let parts = predicate(index, table.r);
    let with_predicate = |quals: Vec<&Expr>| -> Vec<Expr> {
        let given = and_of(&quals);
        let extra = parts.iter().filter(|p| !implied_by(&given, p)).cloned();
        extra.chain(quals.into_iter().cloned()).collect()
    };
    let index_tuples = if index.unique
        && column + 1 == index.keys.len()
        && equal_here
        && !found_array
        && !found_null
    {
        1.0
    } else {
        let selectivity = cost::list_selectivity_by(&with_predicate(bound), table.eq);
        scans = scans.min((index.pages * 0.3333333).ceil()).max(1.0);
        (selectivity * tuples / scans).round_ties_even()
    };
    // `genericcostestimate`.
    let all = with_predicate(clauses.iter().map(|c| &*c.expr).collect());
    let selectivity = cost::list_selectivity_by(&all, table.eq);
    let index_tuples = if index_tuples <= 0.0 {
        (selectivity * tuples / scans).round_ties_even()
    } else {
        index_tuples
    };
    let index_tuples = index_tuples.min(rows).max(1.0);
    let index_pages = if index.pages > 1.0 && rows > 1.0 {
        (index_tuples * (index.pages - 1.0) / rows).ceil()
    } else {
        1.0
    };
    // The scans of a nested loop read the pages that the other scans read from the cache.
    let mut total = if scans * table.loops > 1.0 {
        let all = index_pages * scans * table.loops;
        let fetched = pages_fetched(all, index.pages, index.pages, table.pages);
        fetched * cost::RANDOM_PAGE / table.loops
    } else {
        index_pages * cost::RANDOM_PAGE
    };
    let others = clauses.iter().filter_map(Clause::other);
    let qual_arg: f64 = others.map(|o| stats.qual_costs([o])).map(|(s, p)| s + p).sum();
    let qual_op = cost::CPU_OPERATOR * clauses.len() as f64;
    let mut startup = qual_arg;
    total += qual_arg;
    total += index_tuples * scans * (cost::CPU_INDEX_TUPLE + qual_op);
    if rows > 1.0 {
        let descent = (rows.ln() / 2.0_f64.ln()).ceil() * cost::CPU_OPERATOR;
        startup += descent;
        total += scans * descent;
    }
    // Each page from the root down to a leaf costs `cpu_operator_cost` times 50.
    let descent = f64::from(index.height + 1) * PAGE_CPU_MULTIPLIER * cost::CPU_OPERATOR;
    startup += descent;
    total += scans * descent;
    (startup, total, selectivity)
}

/// `index_pages_fetched`: the pages of a relation that a scan of the rows reads, with the pages in the cache that `effective_cache_size` gives the relation, as Mackert and Lohman estimate them. The pages of the table stand for `total_table_pages`, which changes the result only when the tables of the query are larger than the cache.
fn pages_fetched(tuples: f64, pages: f64, index_pages: f64, all_pages: f64) -> f64 {
    let t = if pages > 1.0 { pages } else { 1.0 };
    let total = (all_pages + index_pages).max(1.0);
    let b = CACHE_PAGES * t / total;
    let b = if b <= 1.0 { 1.0 } else { b.ceil() };
    if t <= b {
        let fetched = 2.0 * t * tuples / (2.0 * t + tuples);
        if fetched >= t { t } else { fetched.ceil() }
    } else {
        let lim = 2.0 * t * b / (2.0 * t - b);
        let fetched = if tuples <= lim {
            2.0 * t * tuples / (2.0 * t + tuples)
        } else {
            b + (tuples - lim) * (t - b) / t
        };
        fetched.ceil()
    }
}

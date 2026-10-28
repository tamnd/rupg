//! The planner: the plan of a query, as `planner.c` and `createplan.c` make it (document 15 section 15.9).
//!
//! [`plan`] turns the query tree of the analyzer into a [`Plan`] of `rupg-plan`. The planner has no statistics and no index scans yet, so it chooses its nodes as PostgreSQL does for tables that `ANALYZE` has not seen: a sequential scan of each table, the join of the lowest cost of a merge join, a hash join and a nested loop, and the estimates of `costsize.c` with the defaults of `selfuncs.c`. The unique indexes of a table change the estimates of the `=` conditions of its columns and the costs of its joins, as `has_unique_index` and `innerrel_is_unique` use them.

#![forbid(unsafe_code)]

mod cost;
mod exists;
mod exprs;
mod from;
mod joinrm;
mod minmax;
mod output;
mod pullup;
mod pushdown;
mod setcast;
mod setop;
mod sje;
mod upper;

use std::cell::{Cell, OnceCell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::rc::Rc;

use rupg_analyze::{
    Cte, Expr, ExprKind, FromItem, JoinKind, Query, Relation, SetOp, SetTree, SortGroup, SubLink,
    SubLinkKind, TABLE_OID_ATTNUM, Var, Write, WriteKind,
};
use rupg_plan::{
    Grouped, HashTable, HashTables, Item, Kind, Label, Leaf, Node, Operation, Pass, Plan, Scanned,
    Scope, Statistic, SubPlan, Table,
};
use rupg_types::{Value, oid};

pub use cost::{node_cost, type_width};
pub use exprs::nonstrict;
pub use joinrm::remove_joins;
pub use pullup::{merge_from, pull_up};
pub use pushdown::FoldQual;
pub use sje::remove_self_joins;

/// The tables of the inheritance trees of the relations of the queries, their indexes and the settings of the session, which the planner has no catalog for.
pub trait Members: std::fmt::Debug {
    /// The tables that a scan of the relation `relation` of the query reads, as `expand_inherited_rtentry` and `prune_append_rel_partitions` choose them with the conditions `conds` of the scan, or `None` for a relation that is one table.
    fn members(&self, query: &Query, relation: usize, conds: &[&Expr]) -> Option<Vec<Member>>;

    /// The valid indexes of the table with the OID `table`, in the order of their OIDs, as `get_relation_info` reads them from `RelationGetIndexList`.
    fn indexes(&self, table: u32) -> Vec<Index>;

    /// True when the table with the OID `table` is an ordinary table, `RELKIND_RELATION`.
    fn is_table(&self, _table: u32) -> bool {
        true
    }

    /// The size of the heap of the table with the OID `table`: `RelationGetNumberOfBlocks`, and `relpages` and `reltuples` of `pg_class`.
    fn size(&self, _table: u32) -> Size {
        Size::default()
    }

    /// The statistics of the columns of the table with the OID `table` in `pg_statistic`, which `examine_simple_variable` reads.
    fn statistics(&self, _table: u32) -> Vec<Statistic> {
        Vec::new()
    }

    /// `get_actual_variable_endpoint` of an index of the table `table` whose first key is the column `attnum`: the least and the greatest value of the column that is not null, of the versions of the rows that are not dead. `None` when the column has no value that is not null.
    fn variable_range(&self, _table: u32, _attnum: i16) -> Option<(Value, Value)> {
        None
    }

    /// `partitions_are_ordered` and `build_partition_pathkeys`: the column numbers of the key of the partitioned table `table` when the partitions `members`, in the order of their bounds, hold the rows in the order of the key, or `None`.
    fn ordered_partitions(&self, _table: u32, _members: &[u32]) -> Option<Vec<i16>> {
        None
    }

    /// The comparison of two values that are not null, of the types that the arguments give, as the B-tree support function `cmp` of the types gives it. The selectivity functions call the operators of the statistics through it, as `selfuncs.c` calls them through `fmgr`.
    fn compare(&self) -> Compare {
        no_compare
    }

    /// The result of a pattern operator of a value and a pattern, which `patternsel` gets from the function of the operator.
    fn pattern_match(&self) -> PatternMatch {
        no_pattern_match
    }

    /// The `enable_*` settings of the session.
    fn enable(&self) -> Enable {
        Enable::default()
    }
}

/// The size of the heap of a table, as `table_block_relation_estimate_size` reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    /// `RelationGetNumberOfBlocks`: the pages of the heap now.
    pub blocks: f64,
    /// `relpages` of `pg_class`, from the last `VACUUM`, `ANALYZE` or `CREATE INDEX`.
    pub relpages: f64,
    /// `reltuples` of `pg_class`, or -1 when the table was never vacuumed or analyzed.
    pub reltuples: f64,
    /// `relallvisible` of `pg_class`: the pages that the visibility map marks all visible.
    pub relallvisible: f64,
}

impl Default for Size {
    fn default() -> Size {
        Size { blocks: 0.0, relpages: 0.0, reltuples: -1.0, relallvisible: 0.0 }
    }
}

/// The `enable_*` settings of the planner, as the variables of `costsize.c` keep them. A node of a kind that a setting turns off is a disabled node, and the planner takes a plan with more disabled nodes only when no plan has fewer, as `compare_path_costs` compares `disabled_nodes` first.
#[derive(Clone, Copy, Debug)]
pub struct Enable {
    pub seqscan: bool,
    pub indexscan: bool,
    pub indexonlyscan: bool,
    pub bitmapscan: bool,
    pub tidscan: bool,
    pub sort: bool,
    pub incremental_sort: bool,
    pub hashagg: bool,
    pub material: bool,
    pub memoize: bool,
    pub presorted_aggregate: bool,
    pub group_by_reordering: bool,
    pub nestloop: bool,
    pub mergejoin: bool,
    pub hashjoin: bool,
    pub self_join_elimination: bool,
}

impl Default for Enable {
    fn default() -> Self {
        Enable {
            seqscan: true,
            indexscan: true,
            indexonlyscan: true,
            bitmapscan: true,
            tidscan: true,
            sort: true,
            incremental_sort: true,
            hashagg: true,
            material: true,
            memoize: true,
            presorted_aggregate: true,
            group_by_reordering: true,
            nestloop: true,
            mergejoin: true,
            hashjoin: true,
            self_join_elimination: true,
        }
    }
}

thread_local! {
    /// The settings of the plan that the planner makes now, which are global variables in PostgreSQL.
    static ENABLE: Cell<Enable> = Cell::new(Enable::default());
}

/// The `enable_*` settings of the plan that the planner makes now.
pub(crate) fn enable() -> Enable {
    ENABLE.with(Cell::get)
}

/// The order of two values that are not null with their types, or `None` when the types do not compare.
pub type Compare = fn(u32, &Value, u32, &Value) -> Option<Ordering>;

/// The comparison when the planner has none: no two values compare.
fn no_compare(_: u32, _: &Value, _: u32, _: &Value) -> Option<Ordering> {
    None
}

thread_local! {
    /// The comparison of the values of the plan that the planner makes now.
    static COMPARE: Cell<Compare> = Cell::new(no_compare);
    /// The pattern operators of the plan that the planner makes now.
    static PATTERN_MATCH: Cell<PatternMatch> = Cell::new(no_pattern_match);
}

/// The result of the positive pattern operator with the OID of the first argument, `~~`, `~~*`, `~`, `~*` or `^@`, of a value and a pattern that are not null, or `None` when it is not known.
pub type PatternMatch = fn(u32, &Value, &Value) -> Option<bool>;

/// The pattern operators when the planner has none: no result is known.
fn no_pattern_match(_: u32, _: &Value, _: &Value) -> Option<bool> {
    None
}

/// The result of a pattern operator, as [`Members::pattern_match`] gives it.
pub(crate) fn pattern_match(op: u32, value: &Value, pattern: &Value) -> Option<bool> {
    PATTERN_MATCH.with(Cell::get)(op, value, pattern)
}

/// The order of two values that are not null with their types, as [`Members::compare`] gives it.
pub(crate) fn compare(left: u32, a: &Value, right: u32, b: &Value) -> Option<Ordering> {
    COMPARE.with(Cell::get)(left, a, right, b)
}

/// The statistics of the columns of each table of a plan, by column number.
type Statistics = BTreeMap<u32, Rc<BTreeMap<i16, Statistic>>>;

thread_local! {
    /// The statistics with `stainherit` false of the columns of the tables of the plan that the planner makes now, which PostgreSQL reads from the cache of `pg_statistic`.
    static STATISTICS: RefCell<Statistics> = RefCell::default();
}

/// The statistics with `stainherit` false of the columns of the table `table`, by column number, as `examine_simple_variable` and `get_attavgwidth` read them.
pub(crate) fn statistics(table: u32) -> Rc<BTreeMap<i16, Statistic>> {
    STATISTICS.with(|s| s.borrow().get(&table).cloned().unwrap_or_default())
}

thread_local! {
    /// The width of each column of each relation with members of an inheritance tree, by the address of the relation in its query, as `set_append_rel_size` sets `attr_widths` of the parent.
    static WIDTHS: RefCell<BTreeMap<(usize, i16), i32>> = RefCell::default();
}

/// The width of the column `attnum` of a relation with members, which [`set_width`] set.
pub(crate) fn width(relation: &Relation, attnum: i16) -> Option<i32> {
    let at = std::ptr::from_ref(relation).addr();
    WIDTHS.with(|w| w.borrow().get(&(at, attnum)).copied())
}

/// Sets the width of the column `attnum` of a relation with members.
pub(crate) fn set_width(relation: &Relation, attnum: i16, width: i32) {
    let at = std::ptr::from_ref(relation).addr();
    WIDTHS.with(|w| w.borrow_mut().insert((at, attnum), width));
}

thread_local! {
    /// The place of each member of an inheritance tree in the list of the members of its relation, as `expand_inherited_rtentry` gives the members their range table entries.
    static MEMBER_PLACES: RefCell<BTreeMap<u32, usize>> = RefCell::default();
}

/// The place of the member `oid` in the list of the members of its relation, which [`set_member_places`] set.
fn member_place(oid: u32) -> usize {
    MEMBER_PLACES.with(|m| m.borrow().get(&oid).copied().unwrap_or(usize::MAX))
}

/// Sets the places of the members of a relation.
pub(crate) fn set_member_places(oids: &[u32]) {
    MEMBER_PLACES.with(|m| m.borrow_mut().extend(oids.iter().enumerate().map(|(at, &o)| (o, at))));
}

/// The statistics of the tables of the query and of the queries in it, from `members`.
fn query_statistics(query: &Query, members: &dyn Members, out: &mut Statistics) {
    fn expr(e: &Expr, members: &dyn Members, out: &mut Statistics) {
        for child in e.children() {
            expr(child, members, out);
        }
        if let ExprKind::SubLink(link) = &e.kind {
            query_statistics(&link.query, members, out);
        }
    }
    for relation in &query.relations {
        if let Some(sub) = &relation.subquery {
            query_statistics(sub, members, out);
        } else if relation.oid != 0 && !out.contains_key(&relation.oid) {
            let all = members.statistics(relation.oid).into_iter().filter(|s| !s.inherit);
            out.insert(relation.oid, Rc::new(all.map(|s| (s.attnum, s)).collect()));
        }
    }
    for cte in &query.ctes {
        query_statistics(&cte.query, members, out);
    }
    for e in query.exprs() {
        expr(e, members, out);
    }
}

/// An index of a table, with the facts of `IndexOptInfo` that the planner uses.
#[derive(Clone, Debug)]
pub struct Index {
    pub oid: u32,
    /// `indexkeys`: the column number of each key column in the table, or 0 for an expression.
    pub keys: Vec<i16>,
    /// `indisunique`.
    pub unique: bool,
    /// `indnullsnotdistinct`: the index has one null at most in a key column.
    pub nulls_not_distinct: bool,
    /// `indimmediate`: false for the index of a deferrable constraint.
    pub immediate: bool,
    /// True for a partial index, which has a predicate.
    pub partial: bool,
    /// `relam`: the OID of the access method.
    pub method: u32,
    /// The column number of each column of the index in the table, the key columns then the `INCLUDE` columns, or 0 for an expression.
    pub columns: Vec<i16>,
    /// `opfamily` of each key column, or 0 for an operator class that the planner does not know.
    pub families: Vec<u32>,
    /// True for each key column whose operator class is the default class of its type, so that the order of the index is the order of `ORDER BY` of the column.
    pub default_class: Vec<bool>,
    /// `indoption` of each key column: 1 for `DESC` and 2 for `NULLS FIRST`.
    pub options: Vec<i16>,
    /// `indexcollations`: the collation of each key column, or 0 for a type that has no collation.
    pub collations: Vec<u32>,
    /// The collation of the table column of each key column, which a condition of the column has when it has no `COLLATE`, or 0 for an expression.
    pub column_collations: Vec<u32>,
    /// `pages`: the pages of the index.
    pub pages: f64,
    /// `tree_height` of `_bt_getrootheight`: the level of the root of a `btree` index, 0 when the root is a leaf.
    pub height: u32,
    /// `indexprs`: the expression of each key column with the number 0, in the order of the columns. A column of the table is a column of the relation 0.
    pub exprs: Vec<Expr>,
    /// `indpred`: the parts of the `AND` of the predicate of a partial index. A column of the table is a column of the relation 0.
    pub predicate: Vec<Expr>,
    /// `get_rel_data_width` of the index: the sum of the average widths of the types of its columns.
    pub width: i32,
    /// `pagesPerRange` of a `brin` index, from its option `pages_per_range`, and 0 for another index.
    pub pages_per_range: u32,
}

/// A table of an inheritance tree that a scan reads.
#[derive(Clone, Debug)]
pub struct Member {
    pub oid: u32,
    /// The type and the type modifier of each column of the table that is not dropped.
    pub columns: Vec<(u32, i32)>,
    /// `relhassubclass`: the table has children, so the planner takes no pages for it when it has none.
    pub has_children: bool,
    /// The size of the heap of the table.
    pub size: Size,
    /// The column number in the table of each column of the parent, by name, or 0 for a dropped column of the parent.
    pub attnums: Vec<i16>,
}

/// The plan of a query, with the members of the relations that `members` gives.
pub fn plan<'q>(query: &'q Query, members: &'q dyn Members) -> Plan<'q> {
    let saved = ENABLE.with(|e| e.replace(members.enable()));
    let mut statistics = Statistics::new();
    query_statistics(query, members, &mut statistics);
    let saved_statistics = STATISTICS.with(|s| s.replace(statistics));
    let saved_compare = COMPARE.with(|c| c.replace(members.compare()));
    let saved_match = PATTERN_MATCH.with(|c| c.replace(members.pattern_match()));
    let saved_widths = WIDTHS.with(|w| w.take());
    let saved_places = MEMBER_PLACES.with(|m| m.take());
    let plan = plan_with(query, members);
    MEMBER_PLACES.with(|m| m.replace(saved_places));
    WIDTHS.with(|w| w.replace(saved_widths));
    PATTERN_MATCH.with(|c| c.set(saved_match));
    COMPARE.with(|c| c.set(saved_compare));
    ENABLE.with(|e| e.set(saved));
    STATISTICS.with(|s| s.replace(saved_statistics));
    plan
}

/// The order of the rows of the joins of the plan `plan` of `query`, as `ExecNestLoop` and `ExecHashJoin` give them: the relations of the query from the outermost loop to the innermost one, each with true when its rows come in the reverse order of the scan. A nested loop gives the rows of its inner side for each row of its outer side, and a hash join gives the rows of its inner side in the reverse order, as `ExecHashTableInsert` puts each row at the head of its bucket. An index scan gives the rows in the order of the keys of its index, which `members` gives, and a bitmap heap scan gives them in the order of the heap. `None` when the plan has a node that gives its rows in another order, such as a sort or a merge join, or an index scan of an index with an expression or an operator class that is not the default one.
pub fn join_order(plan: &Plan<'_>, query: &Query, members: &dyn Members) -> Option<Vec<Scanned>> {
    let mut node = &plan.root;
    loop {
        match &node.kind {
            Kind::Limit | Kind::LockRows | Kind::Materialize => {}
            Kind::Aggregate { strategy: rupg_plan::Strategy::Plain, .. } => {}
            Kind::Result { .. } if node.children.len() == 1 => {}
            _ => break,
        }
        node = node.children.first()?;
    }
    let mut out = Vec::new();
    loop_order(node, false, &mut |relation, reverse, index| {
        let (keys, backward) = match index {
            Some((index, backward)) => {
                (index_keys(query, members, relation, index, backward)?, backward)
            }
            None => (Vec::new(), false),
        };
        out.push(Scanned { relation, reverse, keys, backward });
        Some(())
    })
    .then_some(out)
}

/// The keys of the order of the rows of an index scan of the relation `r` by the index `index`, as `build_index_pathkeys` gives them, or the other order when `backward`. `None` when the index has an expression, or an operator class that is not the default class of the type of its column, or the relation is a catalog, whose rows have only the columns that the query reads.
fn index_keys(
    query: &Query,
    members: &dyn Members,
    r: usize,
    index: u32,
    backward: bool,
) -> Option<Vec<SortGroup>> {
    let relation = query.relations.get(r)?;
    if rupg_pgcatalog::catalog_by_oid(relation.oid).is_some() {
        return None;
    }
    let index = members.indexes(relation.oid).into_iter().find(|i| i.oid == index)?;
    // `amcanorder` of `gist` and `spgist` is false: the scan gives the rows in no order. The OIDs are `BTREE_AM_OID` and `HASH_AM_OID`.
    if !matches!(index.method, 403 | 405) {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(index.keys.len());
    for (i, &attnum) in index.keys.iter().enumerate() {
        let target = usize::try_from(attnum).ok()?.checked_sub(1)?;
        if !index.default_class.get(i).copied().unwrap_or(false) {
            return None;
        }
        let ty = relation.columns.get(target)?.ty;
        let option = index.options.get(i).copied().unwrap_or(0);
        let desc = (option & 1 != 0) != backward;
        let nulls_first = (option & 2 != 0) != backward;
        let (lt, gt) = rupg_analyze::sort_operators(ty);
        let operator = if desc { gt } else { lt };
        let sort = rupg_pgcatalog::builtin::operator_by_oid(operator)?.code;
        out.push(SortGroup {
            target,
            operator,
            sort,
            equal: 0,
            nulls_first,
            hashable: false,
            collation: 0,
        });
    }
    Some(out)
}

/// The hash tables of the plan `plan` of `query` for `GROUP BY`, `DISTINCT` and the set operations of `query`, whose groups come in the order of the table. A table of a subquery or of a subplan is not in the list.
pub fn hash_tables(plan: &Plan<'_>, query: &Query) -> HashTables {
    let mut out = HashTables::default();
    let mut nodes = vec![&plan.root];
    while let Some(node) = nodes.pop() {
        nodes.extend(&node.children);
        nodes.extend(node.init_plans.iter().chain(&node.sub_plans).map(|s| &s.plan));
        let table = match &node.kind {
            // `agg_retrieve_direct`: an `AGG_MIXED` gives the groups of its hash tables after the passes over sorted rows.
            Kind::Aggregate { table: Some(Table { of: Grouped::Group(q), .. }), sets, .. }
                if !sets.is_empty() =>
            {
                if std::ptr::eq(*q, query) {
                    let pass = |r: &rupg_plan::Rollup<'_>| Pass {
                        hashed: r.hashed,
                        sets: r.targets.clone(),
                        groups: r.groups.clone(),
                    };
                    out.passes = sets.iter().filter(|r| !r.hashed).map(pass).collect();
                    out.passes.extend(sets.iter().filter(|r| r.hashed).map(pass));
                }
                continue;
            }
            Kind::Aggregate { table: Some(table), .. } | Kind::SetOp { table: Some(table), .. } => {
                table
            }
            _ => continue,
        };
        let hash = |right| HashTable { groups: table.groups, keys: table.keys.clone(), right };
        match table.of {
            Grouped::Group(q) if std::ptr::eq(q, query) => out.group = Some(hash(false)),
            Grouped::Distinct(q) if std::ptr::eq(q, query) => out.distinct = Some(hash(false)),
            Grouped::SetOp(op, right) => {
                let mut path = Vec::new();
                if let Some(top) = &query.set_op
                    && op_path(top, op, &mut path)
                {
                    out.set_ops.push((path, hash(right)));
                }
            }
            _ => {}
        }
    }
    out
}

/// True when `op` is `top` or an operation in it, with the sides from `top` down to it in `path`.
fn op_path(top: &SetOp, op: &SetOp, path: &mut Vec<bool>) -> bool {
    if std::ptr::eq(top, op) {
        return true;
    }
    for (right, side) in [(false, &top.left), (true, &top.right)] {
        if let SetTree::Op(inner) = side {
            path.push(right);
            if op_path(inner, op, path) {
                return true;
            }
            path.pop();
        }
    }
    false
}

/// The relations of the join tree `node` for [`join_order`], with `reverse` for the rows of an inner side of a hash join. `out` takes each relation with `reverse`, and with its index and true for a backward scan for an index scan.
fn loop_order(
    node: &Node<'_>,
    reverse: bool,
    out: &mut impl FnMut(usize, bool, Option<(u32, bool)>) -> Option<()>,
) -> bool {
    let inner_or_left = |join: &JoinKind| matches!(join, JoinKind::Inner | JoinKind::Left);
    let base = node.scope == 0 && node.member.is_none();
    match (&node.kind, node.children.as_slice()) {
        (Kind::SeqScan(relation), []) if base => out(*relation, reverse, None).is_some(),
        (Kind::BitmapHeapScan { relation, .. }, [_]) if base => {
            out(*relation, reverse, None).is_some()
        }
        (Kind::IndexScan { relation, index, backward, .. }, []) if base => {
            out(*relation, reverse, Some((*index, *backward))).is_some()
        }
        (Kind::Materialize | Kind::Memoize(_), [child]) => loop_order(child, reverse, out),
        (Kind::NestedLoop { join, .. }, [outer, inner]) if inner_or_left(join) => {
            loop_order(outer, reverse, out) && loop_order(inner, reverse, out)
        }
        (Kind::HashJoin { join, .. }, [outer, hash]) if inner_or_left(join) => {
            match (&hash.kind, hash.children.as_slice()) {
                (Kind::Hash, [inner]) => {
                    loop_order(outer, reverse, out) && loop_order(inner, !reverse, out)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// [`plan`] with the settings of `members` in place.
fn plan_with<'q>(query: &'q Query, members: &'q dyn Members) -> Plan<'q> {
    let mut b = Builder { members: Some(members), ..Builder::default() };
    let mut root = b.query(query, None);
    let mut pending = std::mem::take(&mut b.pending);
    let mut chosen = Chosen { labels: &mut b.labels, dropped: Vec::new() };
    attach(&mut root, &mut pending, &mut chosen);
    // A subplan that no node holds, such as one in an expression that the planner made again, goes to the top node.
    let rows = root.cost.rows;
    while !pending.is_empty() {
        let item = pending.remove(0);
        let sub = choose(item, rows, &mut pending, &mut chosen);
        root.sub_plans.push(sub);
    }
    for range in std::mem::take(&mut chosen.dropped) {
        b.groups[range].iter_mut().for_each(Vec::clear);
    }
    let mut order = std::mem::take(&mut b.current);
    for group in b.groups {
        order.extend(group);
    }
    Plan { root, scopes: b.scopes, sublinks: b.labels, order }
}

/// The state of the planner for a statement, as `PlannerGlobal`.
#[derive(Debug, Default)]
struct Builder<'q> {
    scopes: Vec<Scope<'q>>,
    labels: Vec<Label<'q>>,
    /// The number of subplans of each kind of subquery so far, for their names, as `choose_plan_name`. Then come the subplans of [`SubLink::alternative`], of [`Query::minmax`] and of `SET (a, b) = (SELECT ...)`.
    counts: [usize; 9],
    /// The scopes of the subplan that the planner makes now, in the order of the range table.
    current: Vec<usize>,
    /// The scopes of each subplan that is done, in the order that the subplans end.
    groups: Vec<Vec<usize>>,
    /// The subplans that run for each row, which go to the node that holds their subquery.
    pending: Vec<Pending<'q>>,
    /// The queries of `WITH` that the query and the queries outside it give.
    ctes: Vec<CteUse<'q>>,
    /// The rows of the part of each recursive query that is not recursive, by the number of its work table, for the estimate of a `WorkTableScan`.
    work: Vec<(usize, f64)>,
    /// True when the next query is the subquery of `EXISTS`.
    exists: bool,
    /// The fraction of the rows of the next query that its caller reads, as the `tuple_fraction` of `subquery_planner`. A fraction of 1 or more is a number of rows, and 0 is all the rows.
    fraction: f64,
    /// True when the next query is a subquery of [`Query::minmax`], whose plan must give its rows in the order of `ORDER BY` with no sort, as `get_cheapest_fractional_path_for_pathkeys` finds it.
    presorted: bool,
    /// True when the plan of the last query with `presorted` found no plan with the order of its rows.
    failed: bool,
    members: Option<&'q dyn Members>,
    /// The tables of the scans of each scope, by the scope and the place of the table, for the parameterized scans of the tables.
    bases: BTreeMap<(usize, usize), from::Base<'q>>,
    /// The append relation of the `UNION ALL` that the planner plans now, which keeps the plans of its members.
    append: Option<from::append::AppendRel<'q>>,
    /// The `SELECT` of a side of `INTERSECT` or `EXCEPT` that the planner plans now, which keeps the facts of its plans that the set operation reads.
    setop: Option<setop::Leaf<'q>>,
    /// True when the next query is a subquery of `FROM`, whose scan reads each plan of the subquery that `add_path` keeps, as `set_subquery_pathlist`.
    paths: bool,
    /// The plans of the last query with `paths`.
    sub_paths: Option<SubPaths<'q>>,
}

/// An order of the rows of a query as the places of its columns, each with the direction and the place of the nulls.
type ColumnOrder = Vec<(usize, bool, bool)>;

/// The plans of a subquery of `FROM` with the order of their rows, as `set_subquery_pathlist` reads the `pathlist` of the final relation of the subquery. Each key of an order is the place of a column of the subquery with the direction and the place of the nulls.
#[derive(Debug, Default)]
struct SubPaths<'q> {
    /// The order of the rows of the plan that the query gives.
    order: ColumnOrder,
    /// The other plans, which the query gives only with no subplan and no member of an inheritance tree. One of them can be the plan that the query gives.
    others: Vec<(Node<'q>, ColumnOrder)>,
}

/// A subplan that runs for each row, which waits for the node that holds its subquery.
#[derive(Debug)]
struct Pending<'q> {
    link: &'q SubLink,
    sub: SubPlan<'q>,
    /// The subplan of [`SubLink::alternative`], of which the plan keeps one with `sub`.
    alternative: Option<Alternative<'q>>,
    /// The groups of scopes of `sub`.
    groups: Range<usize>,
}

/// The subplan of the `ANY` of a correlated `EXISTS`, which `make_subplan` puts with the subplan of `EXISTS` in an `AlternativeSubPlan`.
#[derive(Debug)]
struct Alternative<'q> {
    sub: SubPlan<'q>,
    /// The name of the subplan, such as `exists_to_any_1`.
    name: String,
    /// The index of the label of the subquery in [`Builder::labels`].
    label: usize,
    /// The start cost and the cost of each call of the subplan of `EXISTS`, then of this subplan.
    costs: [(f64, f64); 2],
    /// The groups of scopes of each of the two subplans in [`Builder::groups`], with the groups of their own subplans.
    groups: [Range<usize>; 2],
}

/// A query of `WITH` in the scope of the planner.
#[derive(Debug)]
struct CteUse<'q> {
    cte: &'q Cte,
    /// True when each reference reads its own copy of the query as a subquery, as `inline_cte`. Else the plan runs the query once as an `InitPlan`, and each reference is a `CteScan`.
    inline: bool,
    /// The rows of the plan of a query that is not inline.
    rows: f64,
    /// The order of the rows of the plan of a query that is not inline, as the places of its columns, which `set_cte_pathlist` gives each `CteScan` of the query.
    order: ColumnOrder,
}

/// A query that the planner plans, as `PlannerInfo`.
#[derive(Debug)]
pub(crate) struct Level<'q> {
    pub(crate) scope: usize,
    pub(crate) query: &'q Query,
    /// The scope and the query of the expressions of the nodes above a set operation: the leftmost `SELECT` of the operation, as `EXPLAIN` shows them. For another query it is the query.
    pub(crate) display: usize,
    pub(crate) display_query: &'q Query,
    /// The subplans that run once before the top node: the queries of `WITH` and the subqueries with no reference to the query.
    pub(crate) init: Vec<SubPlan<'q>>,
    /// The cost of the subplans of `init`, which each run once when the top node starts, as `SS_compute_initplan_cost` adds them.
    pub(crate) init_cost: f64,
    /// The costs of the subqueries of the expressions of the query that run as a `SubPlan` for each row.
    pub(crate) subplans: Vec<cost::SubCost>,
    /// True when the query gives no targets, as `simplify_EXISTS_query` makes the subquery of `EXISTS`.
    pub(crate) no_targets: bool,
    /// The `tuple_fraction` of the query, with the effect of its `LIMIT`, as `preprocess_limit` makes it. When it is not 0, a plan with a lower start cost is a different plan.
    pub(crate) fraction: f64,
    /// The conditions of joins that the equivalence classes took the place of. The scans below a join still give their columns, as `attr_needed` keeps them.
    pub(crate) joined: Vec<&'q Expr>,
    /// The estimates of the relations of `FROM`, which `estimate_num_groups` reads.
    pub(crate) stats: from::Stats,
    /// The junk columns of the row marks, which the rows of the query carry after its targets.
    pub(crate) marks: Vec<Expr>,
    /// The write of the statement of the query. A write with check constraints has it on the query outside [`Level::query`].
    pub(crate) write: Option<&'q Write>,
    /// True for a subquery of [`Query::minmax`]: only a plan of `FROM` whose rows are in the order of `ORDER BY` is a plan of the query.
    pub(crate) presorted: bool,
    /// [`from::lateral_rels`] of each relation of the query. The join search reads them for each pair of relations that it tries, so the planner finds them once.
    pub(crate) lateral: OnceCell<Vec<BTreeSet<usize>>>,
}

impl<'q> Level<'q> {
    /// The expression of a target of the query of the display scope.
    pub(crate) fn target(&self, index: usize) -> &'q Expr {
        &self.display_query.targets[index].expr
    }
}

/// `convert_subquery_pathkeys`: the first keys of an order that are columns of the query, as the places of the columns. A key that is a column that an earlier key has adds nothing, as `pathkey_is_redundant`.
fn columns(level: &Level<'_>, order: &[from::OrderKey]) -> ColumnOrder {
    let mut out: ColumnOrder = Vec::new();
    for key in order {
        let found = level
            .display_query
            .targets
            .iter()
            .enumerate()
            .find(|(_, t)| !t.junk && key.exprs.iter().any(|e| e.same(&t.expr)));
        let Some((at, _)) = found else { break };
        if !out.iter().any(|&(c, _, _)| c == at) {
            out.push((at, key.desc, key.nulls_first));
        }
    }
    out
}

/// The query of the rows of a statement with the conditions of check constraints, which `with_checks` of the analyzer puts around the query of the rows.
/// True when `simplify_EXISTS_query` takes out the targets of the subquery of `EXISTS`: a query with no set operation, aggregate, window, `HAVING`, `LIMIT`, `OFFSET` or row lock.
fn simple_exists(query: &Query) -> bool {
    query.set_op.is_none()
        && !query.grouped
        && query.having.is_none()
        && query.windows.is_empty()
        && query.limit.is_none()
        && query.offset.is_none()
        && query.row_marks.is_empty()
        && query.sort.is_empty()
        && query.distinct.is_empty()
}

fn unwrap_checks(query: &Query) -> &Query {
    let Some(write) = &query.write else { return query };
    match query.relations.as_slice() {
        [relation]
            if !write.checks.is_empty() && relation.oid == 0 && relation.name == write.name =>
        {
            relation.subquery.as_deref().unwrap_or(query)
        }
        _ => query,
    }
}

impl<'q> Builder<'q> {
    /// `subquery_planner`: the plan of a query, whose scope is in the scope `parent`.
    fn query(&mut self, outer: &'q Query, parent: Option<usize>) -> Node<'q> {
        let write = outer.write.as_ref();
        let query = unwrap_checks(outer);
        let target = write.filter(|w| w.kind == WriteKind::Insert);
        let scope = self.push_scope(query, parent, target);
        let no_targets = std::mem::take(&mut self.exists) && simple_exists(query);
        let caller = std::mem::take(&mut self.fraction);
        let presorted = std::mem::take(&mut self.presorted);
        let want_paths = std::mem::take(&mut self.paths);
        let marks = self.marks(query, write);
        let leaf = self.setop.as_ref().filter(|s| parent == Some(s.scope)).map(|s| s.op);
        let mut level = Level {
            scope,
            query,
            display: scope,
            display_query: query,
            init: Vec::new(),
            init_cost: 0.0,
            subplans: Vec::new(),
            no_targets,
            fraction: upper::tuple_fraction(query, caller),
            joined: Vec::new(),
            stats: from::Stats::default(),
            marks,
            write,
            presorted,
            lateral: OnceCell::new(),
        };
        let depth = self.ctes.len();
        if !std::ptr::eq(outer, query) {
            self.ctes_of(outer, &mut level);
        }
        self.ctes_of(query, &mut level);
        for expr in preprocess_order(query) {
            self.sublinks_in(expr, &mut level);
        }
        let (plans, joined, stats) = match &query.set_op {
            Some(op) => (self.set_op(&mut level, op), Vec::new(), from::Stats::default()),
            None => self.from(&level),
        };
        level.joined = joined;
        level.stats = stats;
        // `generate_orderedappend_paths` reads each plan of a member of an append relation that `add_path` keeps.
        let member = query.set_op.is_none()
            && self.append.as_ref().is_some_and(|a| self.scopes[scope].parent == Some(a.scope));
        // `build_setop_child_paths` reads each plan of a side of `INTERSECT` or `EXCEPT` that `add_path` keeps.
        let mut finals = Vec::new();
        if let Some(op) = leaf
            && setop::simple(query)
        {
            let keys = setop::keys(query, op, &level.stats);
            for (i, (node, order)) in plans.iter().enumerate() {
                if let Some(mut node) = self.upper_one(&level, node.clone(), order.clone(), i > 0) {
                    output::level(&level, &mut node);
                    finals.push((node, setop::ordered(order, &keys)));
                }
            }
        }
        if member {
            let mut kept = Vec::new();
            for (i, (node, order)) in plans.iter().enumerate() {
                if let Some(mut node) = self.upper_one(&level, node.clone(), order.clone(), i > 0) {
                    output::level(&level, &mut node);
                    kept.push((node, order.clone()));
                }
            }
            if let Some(append) = &mut self.append {
                append.plans.push(kept);
            }
        }
        let others =
            (want_paths && query.minmax.is_none() && write.is_none()).then(|| plans.clone());
        let (mut node, order) = self.upper(&level, plans, caller);
        if let Some(minmax) = &query.minmax {
            node = self.minmax(&level, minmax, node);
        }
        if let Some(write) = write {
            node = self.modify(&level, write, node);
        }
        output::level(&level, &mut node);
        self.leaves(&level, &mut node, false);
        // `SS_charge_for_initplans`: the top node runs the subplans of `init` when it starts.
        node.cost.startup += level.init_cost;
        node.cost.total += level.init_cost;
        // The path of a gating `Result` at the top is the path of the node below it, so that the node below has the same costs.
        if let Kind::Result { replaces: None, one_time } = &node.kind
            && !one_time.is_empty()
            && let [child] = &mut node.children[..]
        {
            child.cost.startup += level.init_cost;
            child.cost.total += level.init_cost;
        }
        let mut init = std::mem::take(&mut level.init);
        init.append(&mut node.init_plans);
        node.init_plans = init;
        if let Some(op) = leaf {
            // A plan that is not the cheapest can take its place only with no subplans, which the planner gives the cheapest plan.
            let others = level.init_cost == 0.0 && node.init_plans.is_empty();
            let sorted = setop::sorted(query, op, &level.stats, &node, finals, others);
            let groups = setop::groups(query, &level.stats, node.cost.rows);
            let complete = setop::complete(query, op, &level.stats);
            let wrapper = self.wrapper(scope);
            if let Some(s) = &mut self.setop {
                // The groups of a `SELECT` with casts are the groups of the subquery of the `SELECT`, which comes first.
                if !wrapper {
                    s.groups = groups;
                }
                s.sorted = sorted;
                // `generate_setop_child_grouplist`: a `SELECT` with a column of another type than the operation has no `setop_pathkeys`.
                s.complete = complete && !wrapper;
            }
        } else if let Some(p) = parent
            && self.wrapper(p)
            && self.setop.as_ref().is_some_and(|s| self.scopes[p].parent == Some(s.scope))
        {
            let groups = setop::groups(query, &level.stats, node.cost.rows);
            if let Some(s) = &mut self.setop {
                s.groups = groups;
            }
        }
        if want_paths {
            // `create_modifytable_path` and `create_minmaxagg_path` give the rows in no order.
            let order = if write.is_none() && query.minmax.is_none() {
                columns(&level, &order)
            } else {
                Vec::new()
            };
            let mut paths = SubPaths { order, others: Vec::new() };
            if let Some(plans) = others
                && level.init_cost == 0.0
                && level.subplans.is_empty()
                && node.init_plans.is_empty()
            {
                for (mut plan, order) in self.final_paths(&level, plans) {
                    output::level(&level, &mut plan);
                    if !setop::has_member(&plan) {
                        paths.others.push((plan, columns(&level, &order)));
                    }
                }
            }
            self.sub_paths = Some(paths);
        }
        self.ctes.truncate(depth);
        node
    }

    /// `preprocess_rowmarks` and `preprocess_targetlist`: the junk columns that the rows of a query carry for its row marks. `FOR UPDATE` and `FOR SHARE` mark each relation of `FROM`, the locked tables first, and an `UPDATE` or a `DELETE` marks each relation other than its target. A table gives `ctid`, and a function, a `VALUES` list or a query of `WITH` gives the whole row, as `ROW_MARK_COPY`. A table with an inheritance tree also gives `tableoid`, which `expand_inherited_rtentry` adds after the other columns. A subquery gives no column yet.
    fn marks(&self, query: &'q Query, write: Option<&Write>) -> Vec<Expr> {
        let target = match write.map(|w| &w.kind) {
            Some(WriteKind::Update | WriteKind::Delete | WriteKind::Merge) => Some(0),
            _ if !query.row_marks.is_empty() && write.is_none() => None,
            _ => return Vec::new(),
        };
        let mut items: Vec<&FromItem> = query.from.iter().collect();
        let mut base = Vec::new();
        while let Some(item) = items.pop() {
            match item {
                FromItem::Relation(r) => base.push(*r),
                FromItem::Join(join) => items.extend([&join.left, &join.right]),
            }
        }
        base.sort_unstable();
        base.retain(|&r| Some(r) != target);
        let table = |r: usize| {
            let relation = &query.relations[r];
            relation.oid != 0
                && relation.subquery.is_none()
                && relation.function.is_none()
                && relation.values.is_none()
                && relation.cte.is_none()
        };
        let mut order: Vec<usize> = query
            .row_marks
            .iter()
            .map(|mark| mark.relation)
            .filter(|&r| table(r) && base.contains(&r))
            .collect();
        for r in base {
            if !order.contains(&r) {
                order.push(r);
            }
        }
        let var = |r: usize, attnum: i16, ty: u32| {
            Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), ty)
        };
        let mut out = Vec::new();
        let mut parents = Vec::new();
        for r in order {
            if table(r) {
                out.push(var(r, -1, oid::TID));
                if self.members.and_then(|m| m.members(query, r, &[])).is_some() {
                    parents.push(var(r, TABLE_OID_ATTNUM, oid::OID));
                }
            } else if query.relations[r].subquery.is_none() {
                out.push(var(r, 0, oid::RECORD));
            }
        }
        out.extend(parents);
        out
    }

    /// A new scope in the current subplan.
    /// True when the query of a scope is a `SELECT` of a set operation in the query that adds its casts, as [`setcast::wrap_all`] makes it.
    fn wrapper(&self, scope: usize) -> bool {
        let found = &self.scopes[scope];
        setcast::is_wrapper(found.query)
            && found.parent.is_some_and(|p| self.scopes[p].query.set_op.is_some())
    }

    fn push_scope(
        &mut self,
        query: &'q Query,
        parent: Option<usize>,
        target: Option<&'q Write>,
    ) -> usize {
        self.scopes.push(Scope { query, parent, target, leaf: None });
        let index = self.scopes.len() - 1;
        self.current.push(index);
        index
    }

    /// The scopes of the scans of the members of the inheritance trees of a query, in the order of the nodes. A member has a name of its own when it is one of the members of an `Append` or the target of a `ModifyTable`. `append` is true for the members of an `Append`.
    fn leaves(&mut self, level: &Level<'q>, node: &mut Node<'q>, append: bool) {
        if node.scope != level.scope {
            return;
        }
        if let (Some(_), Some(r)) = (node.member, from::scanned(&node.kind)) {
            let write = level.write;
            let target = r == 0
                && write.is_some_and(|w| matches!(w.kind, WriteKind::Update | WriteKind::Delete));
            let leaf = Leaf { of: level.scope, relation: r, renamed: append || target };
            let parent = self.scopes[level.scope].parent;
            self.scopes.push(Scope { query: level.query, parent, target: None, leaf: Some(leaf) });
            // The index scans below a `Bitmap Heap Scan` read the member too.
            rescope(node, level.scope, self.scopes.len() - 1);
            self.current.push(node.scope);
            return;
        }
        // The sort of a member below a `Merge Append` reads the member, and has its name.
        if append && matches!(node.kind, Kind::Sort(_)) && from::member_of(node).is_some() {
            for child in &mut node.children {
                self.leaves(level, child, append);
                node.scope = child.scope;
            }
            return;
        }
        let append = from::members_append(node);
        // The members have their names in the order of their relations, also when an `Append` reads the partitions from the last one.
        let mut order: Vec<usize> = (0..node.children.len()).collect();
        if append {
            order.sort_by_key(|&at| from::member_of(&node.children[at]).map_or(0, member_place));
        }
        for at in order {
            self.leaves(level, &mut node.children[at], append);
        }
        if let Kind::Modify {
            operation: Operation::Update | Operation::Delete,
            relation: 0,
            targets,
            ..
        } = &mut node.kind
        {
            let scopes = &self.scopes;
            let mut found = Vec::new();
            for child in &node.children {
                member_scopes(child, scopes, &mut found);
            }
            *targets = found;
        }
    }

    /// The plan of a subplan, whose scopes take part in the names of the relations after the scopes of the statement.
    fn subplan(&mut self, f: impl FnOnce(&mut Self) -> Node<'q>) -> Node<'q> {
        let saved = std::mem::take(&mut self.current);
        let node = f(self);
        let group = std::mem::replace(&mut self.current, saved);
        self.groups.push(group);
        node
    }

    /// `SS_process_ctes`: the queries of `WITH` of a query. A query that only one reference reads and that has no volatile function goes into the reference, as `inline_cte`. A `SELECT` that no reference reads has no plan.
    fn ctes_of(&mut self, query: &'q Query, level: &mut Level<'q>) {
        for cte in &query.ctes {
            let select = cte.write.is_none();
            let refs = exprs::cte_references(query, &cte.name);
            if select && refs == 0 {
                continue;
            }
            let inline = select
                && !cte.recursive
                && cte.materialized != Some(true)
                && (refs == 1 || cte.materialized == Some(false))
                && !exprs::query_volatile(&cte.query);
            let mut rows = 1000.0;
            let mut order = Vec::new();
            if !inline {
                let scope = level.scope;
                self.paths = true;
                let plan = self.subplan(|b| b.query(&cte.query, Some(scope)));
                order = self.sub_paths.take().map(|p| p.order).unwrap_or_default();
                rows = plan.cost.rows;
                level.init_cost += plan.cost.total;
                level.init.push(SubPlan { name: format!("CTE {}", cte.name), plan });
            }
            self.ctes.push(CteUse { cte, inline, rows, order });
        }
    }

    /// The query of `WITH` that a reference names, as the query that holds the reference sees it.
    pub(crate) fn cte(&self, name: &str) -> Option<&CteUse<'q>> {
        self.ctes.iter().rev().find(|c| c.cte.name == name)
    }

    /// `SS_process_sublinks`: the subplans of the subqueries in an expression of the query, the subqueries in the test of a subquery first.
    fn sublinks_in(&mut self, expr: &'q Expr, level: &mut Level<'q>) {
        for child in expr.children() {
            self.sublinks_in(child, level);
        }
        if let ExprKind::SubLink(link) = &expr.kind {
            self.sublink(link, level);
        }
    }

    /// `make_subplan`: the plan of a subquery in an expression. A subquery with no reference to the query runs once as an `InitPlan`, except for `ANY` and `ALL`, which keep its rows in a hash table or in a `Material` node.
    fn sublink(&mut self, link: &'q SubLink, level: &mut Level<'q>) {
        // Each column of `SET (a, b) = (SELECT ...)` has a copy of the subquery, and the copies after the first take the subplan of the first.
        if let SubLinkKind::Multi { id, .. } = link.kind
            && let Some(first) = self.labels.iter().find(|label| {
                matches!(label.link.kind, SubLinkKind::Multi { id: other, .. } if other == id)
                    && label.link.query == link.query
            })
        {
            let (name, init) = (first.name.clone(), first.init);
            self.labels.push(Label { link, name, init, hashed: false, alternative: false });
            return;
        }
        let (index, prefix) = match link.kind {
            SubLinkKind::Exists => (0, "exists"),
            SubLinkKind::Expr => (1, "expr"),
            SubLinkKind::Any => (2, "any"),
            SubLinkKind::All => (3, "all"),
            SubLinkKind::Array => (4, "array"),
            SubLinkKind::RowCompare => (5, "rowcompare"),
            SubLinkKind::Multi { .. } => (8, "multiexpr"),
        };
        self.counts[index] += 1;
        let name = format!("{prefix}_{}", self.counts[index]);
        let correlated = exprs::correlated(&link.query, 1);
        let scope = level.scope;
        self.exists = link.kind == SubLinkKind::Exists;
        // `make_subplan`: `EXISTS` reads one row, as with `LIMIT 1`, and `ANY` and `ALL` read half the rows.
        self.fraction = match link.kind {
            SubLinkKind::Exists => 1.0,
            SubLinkKind::Any | SubLinkKind::All => 0.5,
            _ => 0.0,
        };
        let first = self.groups.len();
        let mut plan = self.subplan(|b| b.query(&link.query, Some(scope)));
        let groups = first..self.groups.len();
        let init = !correlated && !matches!(link.kind, SubLinkKind::Any | SubLinkKind::All);
        let hashed = !correlated && link.kind == SubLinkKind::Any && hashable_test(link);
        if !correlated && !init && !hashed && enable().material {
            // `materialize_finished_plan` moves the initplans of the plan to the `Material` node.
            let init_plans = std::mem::take(&mut plan.init_plans);
            let output = plan.output.iter().map(Item::passed).collect();
            plan = upper::materialize(plan);
            plan.init_plans = init_plans;
            plan.output = output;
        }
        self.labels.push(Label { link, name: name.clone(), init, hashed, alternative: false });
        if init {
            level.init_cost += init_cost(link, &plan);
            level.init.push(SubPlan { name: format!("InitPlan {name}"), plan });
        } else {
            let (startup, per_call) = subplan_cost(link, &plan, correlated, hashed);
            level.subplans.push(cost::SubCost {
                link: std::ptr::from_ref(link),
                startup,
                per_call,
            });
            let sub = SubPlan { name: format!("SubPlan {name}"), plan };
            let alternative = match &link.alternative {
                Some(any) if correlated => {
                    self.alternative(any, scope, (startup, per_call), groups.clone())
                }
                _ => None,
            };
            self.pending.push(Pending { link, sub, alternative, groups });
        }
    }

    /// `make_subplan` for the `ANY` of [`SubLink::alternative`]: its subplan, which reads all the rows of its subquery into a hash table. It is none when the hash table does not fit in `hash_mem`, as `subpath_is_hashable`. `exists` is the cost of the subplan of `EXISTS` and `groups` are its groups of scopes.
    fn alternative(
        &mut self,
        any: &'q SubLink,
        scope: usize,
        exists: (f64, f64),
        groups: Range<usize>,
    ) -> Option<Alternative<'q>> {
        self.counts[6] += 1;
        let name = format!("exists_to_any_{}", self.counts[6]);
        let label = self.labels.len() - 1;
        let (first, waiting) = (self.groups.len(), self.pending.len());
        let plan = self.subplan(|b| b.query(&any.query, Some(scope)));
        let own = first..self.groups.len();
        if cost::subplan_hash_space(plan.cost.rows, plan.cost.width) >= cost::HASH_MEM {
            self.groups[own].iter_mut().for_each(Vec::clear);
            self.pending.truncate(waiting);
            return None;
        }
        let costs = [exists, subplan_cost(any, &plan, false, true)];
        let sub = SubPlan { name: format!("SubPlan {name}"), plan };
        Some(Alternative { sub, name, label, costs, groups: [groups, own] })
    }
}

/// The choices of `fix_alternative_subplan` so far: the labels of the subqueries, which take the names of the subplans that the plan keeps, and the groups of scopes of the subplans that it does not keep.
struct Chosen<'a, 'q> {
    labels: &'a mut Vec<Label<'q>>,
    dropped: Vec<Range<usize>>,
}

/// `fix_alternative_subplan`: the subplan of a subquery that the plan keeps, for a node that runs it `num_exec` times. Of the subplan of `EXISTS` and the subplan of its `ANY`, the plan keeps the one with the lower cost, the `ANY` when they are equal. The subplans of the other one take no place in the plan.
fn choose<'q>(
    item: Pending<'q>,
    num_exec: f64,
    pending: &mut Vec<Pending<'q>>,
    chosen: &mut Chosen<'_, 'q>,
) -> SubPlan<'q> {
    let Some(alt) = item.alternative else { return item.sub };
    let cost = |(startup, per_call): (f64, f64)| startup + num_exec * per_call;
    let (kept, mut dropped, range) = if cost(alt.costs[1]) <= cost(alt.costs[0]) {
        let label = &mut chosen.labels[alt.label];
        label.name = alt.name;
        label.hashed = true;
        label.alternative = true;
        let [exists, _] = alt.groups;
        (alt.sub, item.sub, exists)
    } else {
        let [_, own] = alt.groups;
        (item.sub, alt.sub, own)
    };
    chosen.dropped.push(range);
    attach(&mut dropped.plan, pending, chosen);
    kept
}

/// `cost_subplan` of a subplan that runs for each row: the start cost and the cost of each call, which has the cost of the test. A hashed subplan runs its plan once and charges one `cpu_operator_cost` for each row that it puts in the hash table. Each call of another subplan costs the rows that it reads: the first row for `EXISTS`, half of the rows for `ANY` and `ALL` with one `cpu_operator_cost` for each, and all the rows for the others. It pays the start cost of its plan for each call, or once when it reads no column of the query and its top node keeps its rows, as `ExecMaterializesOutput`.
fn subplan_cost(link: &SubLink, plan: &Node<'_>, correlated: bool, hashed: bool) -> (f64, f64) {
    let c = plan.cost;
    let test = cost::qual_cost(&link.test);
    if hashed {
        return (c.total + cost::CPU_OPERATOR * c.rows, test);
    }
    let run = c.total - c.startup;
    let mut per_call = test;
    per_call += match link.kind {
        SubLinkKind::Exists => run / cost::clamp_rows(c.rows),
        SubLinkKind::Any | SubLinkKind::All => 0.5 * run + 0.5 * c.rows * cost::CPU_OPERATOR,
        _ => run,
    };
    let materializes = matches!(
        plan.kind,
        Kind::Materialize
            | Kind::Sort(_)
            | Kind::FunctionScan(_)
            | Kind::TableFunctionScan(_)
            | Kind::CteScan(_)
            | Kind::WorkTableScan(_)
    );
    if !correlated && materializes { (c.startup, per_call) } else { (0.0, per_call + c.startup) }
}

/// `cost_subplan` of a subplan that runs once: the start cost and the cost of each call of the subplan. It is the cost of the plan, and for `EXISTS` the cost of its first row. A subplan that runs once has no test, because the test goes into the expression of the query.
fn init_cost(link: &SubLink, plan: &Node<'_>) -> f64 {
    let c = plan.cost;
    match link.kind {
        SubLinkKind::Exists => c.startup + (c.total - c.startup) / cost::clamp_rows(c.rows),
        _ => c.total,
    }
}

/// Moves the node and the nodes below it of the scope `from` to the scope `to`.
fn rescope(node: &mut Node<'_>, from: usize, to: usize) {
    if node.scope != from {
        return;
    }
    node.scope = to;
    for child in &mut node.children {
        rescope(child, from, to);
    }
}

/// The scopes of the members of the relation 0 of the query below a node, which a `ModifyTable` changes.
fn member_scopes(node: &Node<'_>, scopes: &[Scope<'_>], out: &mut Vec<usize>) {
    if node.member.is_some()
        && let Some(leaf) = scopes.get(node.scope).and_then(|s| s.leaf)
        && leaf.relation == 0
    {
        out.push(node.scope);
        return;
    }
    for child in &node.children {
        member_scopes(child, scopes, out);
    }
}

/// True when each comparison of the test of `ANY` has an operator that can use a hash table, as `testexpr_is_hashable`.
fn hashable_test(link: &SubLink) -> bool {
    fn hashable(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bool(rupg_analyze::BoolOp::And, args) => args.iter().all(hashable),
            _ => exprs::binary(expr).is_some_and(|(_, left, right)| {
                exprs::hash_ok(expr)
                    && !matches!(left.kind, ExprKind::SubColumn(_))
                    && matches!(right.kind, ExprKind::SubColumn(_))
            }),
        }
    }
    link.test.as_ref().is_some_and(hashable)
}

/// The expressions of a query in the order that `subquery_planner` gives them to `preprocess_expression`: the targets, the conditions of `FROM` and `WHERE`, `HAVING`, the offsets of the frames, `OFFSET`, `LIMIT`, then the functions, the `VALUES` lists and the arguments of `TABLESAMPLE` of the relations.
fn preprocess_order(query: &Query) -> Vec<&Expr> {
    fn joins<'a>(item: &'a FromItem, out: &mut Vec<&'a Expr>) {
        if let FromItem::Join(j) = item {
            joins(&j.left, out);
            joins(&j.right, out);
            out.extend(&j.on);
        }
    }
    let mut out: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    for item in &query.from {
        joins(item, &mut out);
    }
    out.extend(query.filter.iter().chain(&query.having));
    for window in &query.windows {
        out.extend(window.frame.start.iter().chain(&window.frame.end));
    }
    out.extend(query.offset.iter().chain(&query.limit));
    for relation in &query.relations {
        if let Some(function) = &relation.function {
            out.extend(function.calls.iter().map(|(call, _)| call));
        }
        out.extend(relation.values.iter().flatten().flatten());
        if let Some(sample) = &relation.tablesample {
            out.extend(sample.args.iter().chain(&sample.repeatable));
        }
    }
    out
}

/// Moves each subplan that runs for each row to the node with the expression that holds its subquery, as `SS_finalize_plan` sees it.
fn attach<'q>(node: &mut Node<'q>, pending: &mut Vec<Pending<'q>>, chosen: &mut Chosen<'_, 'q>) {
    if !pending.is_empty() {
        let mut found: Vec<(*const SubLink, f64)> = Vec::new();
        let mut unused = Vec::new();
        for (item, num_exec) in own_items(node) {
            let mut links = Vec::new();
            sublinks(&item.expr, &mut links);
            found.extend(links.into_iter().map(|l| (std::ptr::from_ref(l), num_exec)));
            grouping_sublinks(&item.expr, &mut unused);
        }
        // `ExecInitExpr` does not start the arguments of `GROUPING`, so their subplans are in no node, and their relations take no names.
        for link in unused {
            if let Some(at) = pending.iter().position(|p| std::ptr::eq(p.link, link)) {
                let mut item = pending.remove(at);
                chosen.dropped.push(item.groups.clone());
                if let Some(alt) = &item.alternative {
                    chosen.dropped.push(alt.groups[1].clone());
                }
                attach(&mut item.sub.plan, pending, chosen);
            }
        }
        for (link, num_exec) in found {
            if let Some(at) = pending.iter().position(|p| std::ptr::eq(p.link, link)) {
                let item = pending.remove(at);
                let sub = choose(item, num_exec, pending, chosen);
                node.sub_plans.push(sub);
            }
        }
    }
    for child in &mut node.children {
        attach(child, pending, chosen);
    }
    for sub in node.init_plans.iter_mut().chain(&mut node.sub_plans) {
        attach(&mut sub.plan, pending, chosen);
    }
}

/// The subqueries of an expression outside the queries of other subqueries and outside the arguments of `GROUPING`, in the order of the text.
fn sublinks<'e>(expr: &'e Expr, out: &mut Vec<&'e SubLink>) {
    if let ExprKind::Grouping(_) = &expr.kind {
        return;
    }
    for child in expr.children() {
        sublinks(child, out);
    }
    if let ExprKind::SubLink(link) = &expr.kind {
        out.push(link);
    }
}

/// The subqueries in the arguments of each `GROUPING` of an expression, outside the queries of other subqueries.
fn grouping_sublinks<'e>(expr: &'e Expr, out: &mut Vec<&'e SubLink>) {
    if let ExprKind::Grouping(grouping) = &expr.kind {
        for arg in &grouping.args {
            arg.find(0, &mut |e, depth| {
                if let (ExprKind::SubLink(link), 0) = (&e.kind, depth) {
                    out.push(link);
                }
                None::<()>
            });
        }
        return;
    }
    for child in expr.children() {
        grouping_sublinks(child, out);
    }
}

/// The expressions that a node computes: its target list apart from the values that it reads from below, its conditions and the other expressions of its kind. Each one comes with the number of times that the node runs it, as `NUM_EXEC_TLIST` and `NUM_EXEC_QUAL` give them: once for each row of the node for a target, twice for a condition and once in all for a condition of a `Result` that runs once.
fn own_items<'n, 'q>(node: &'n Node<'q>) -> Vec<(&'n Item<'q>, f64)> {
    let rows = node.cost.rows;
    let mut out: Vec<(&Item<'q>, f64)> =
        node.output.iter().filter(|i| !i.below).map(|i| (i, rows)).collect();
    let quals = |items: &'n [Item<'q>]| items.iter().map(move |i| (i, rows * 2.0));
    match &node.kind {
        Kind::Result { one_time, .. } => out.extend(one_time.iter().map(|i| (i, 1.0))),
        Kind::NestedLoop { join_filter, .. } => out.extend(quals(join_filter)),
        Kind::MergeJoin { cond, join_filter, .. } | Kind::HashJoin { cond, join_filter, .. } => {
            out.extend(quals(cond).chain(quals(join_filter)));
        }
        _ => {}
    }
    out.extend(quals(&node.filter));
    out
}

//! `get_memoize_path` and `cost_memoize_rescan`: a `Memoize` node over a parameterized scan of the inner side of a nested loop. The node keeps the rows of the scan for each value of its parameters, so that a later outer row with the same values reads the rows from the cache and the scan does not run again.

use rupg_analyze::{Expr, ExprKind, JoinKind, Query, default_opclass, sort_group_operators, types};
use rupg_plan::{Cost, Item, Kind, Memoize, Node};

use super::index::Param;
use super::{Qual, Rel, Stats};
use crate::{cost, exprs};

/// `HASH_AM_OID`, the OID of the access method `hash`.
const HASH: u32 = 405;

/// `sizeof(MemoizeEntry) + sizeof(MemoizeKey)`, which `ExecEstimateCacheEntryOverheadBytes` adds for each entry of the cache.
const ENTRY_BYTES: f64 = 48.0;

/// `sizeof(MemoizeTuple)`, which `ExecEstimateCacheEntryOverheadBytes` adds for each row of an entry.
const TUPLE_BYTES: f64 = 16.0;

/// A `MemoizePath`: the parts of the node and the costs of a scan of the node after the first, as `cost_memoize_rescan` gives them.
#[derive(Clone, Debug)]
pub(super) struct Memo<'q> {
    parts: Memoize<'q>,
    /// The start and the total cost of a scan of the node after the first.
    pub(super) rescan: (f64, f64),
}

/// `get_memoize_path`: the `Memoize` node over the parameterized scan `param` for a nested loop with the outer side `outer`, or `None`. The outer side must have two rows or more and the scan must have conditions of the join. A semi join or an anti join needs an inner side that is unique. With a unique inner side, the scan must test each condition of the join. No condition of the scan can call a volatile function. Each condition must be an operator with the outer side on one side and the inner side on the other, and the type of the outer side must have a hash function and an `=` operator, as `paraminfo_get_equal_hashops`.
pub(super) fn memoize<'q>(
    outer: &Rel<'q>,
    param: &Param<'q>,
    join: JoinKind,
    unique: bool,
    (quals, filter): (&[Qual<'q>], &[Qual<'q>]),
    stats: &Stats,
) -> Option<Memo<'q>> {
    if outer.node.cost.rows < 2.0 || param.enforced.is_empty() {
        return None;
    }
    if matches!(join, JoinKind::Semi | JoinKind::Anti) && !unique {
        return None;
    }
    let enforced = |q: &Qual<'q>| param.enforced.iter().any(|e| e.expr.same(&q.expr));
    if unique && !quals.iter().chain(filter).all(enforced) {
        return None;
    }
    if volatile_node(&param.rel.node) || param.enforced.iter().any(|q| exprs::volatile(&q.expr)) {
        return None;
    }
    let mut keys: Vec<&Expr> = Vec::new();
    let mut binary = false;
    for qual in &param.enforced {
        let (op, left, right) = exprs::binary(&qual.expr)?;
        let sides = (exprs::relations(left), exprs::relations(right));
        let key = if sides.0.is_subset(&outer.rels) && sides.1.is_subset(&param.rel.rels) {
            left
        } else if sides.1.is_subset(&outer.rels) && sides.0.is_subset(&param.rel.rels) {
            right
        } else {
            return None;
        };
        if !hashable(key.ty) {
            return None;
        }
        if !keys.iter().any(|k| k.same(key)) {
            keys.push(key);
        }
        // An operator that cannot use a hash table may tell apart two values that the `=` of the hash function finds equal, such as `-0.0` and `0.0`.
        binary |= !exprs::can_hash(op);
    }
    Some(rescan_cost(&param.rel.node.cost, keys, binary, outer.node.cost.rows, stats))
}

/// `get_memoize_path` for a nested loop whose inner relation `r` reads the outer side as a lateral reference, with the conditions `moved` that its scan tests as the `ppi_clauses` of its `ParamPathInfo`. The keys are the outer sides of those conditions, as `paraminfo_get_equal_hashops` takes them, then the lateral references of the `PlaceHolderVar`s that the scan computes, as `extract_lateral_vars_from_PHVs` takes them, then the lateral references of the relation, as its `lateral_vars`. A lateral reference puts the node in binary mode. A `PlaceHolderVar` that reads no column of `r` is one key. With a unique inner side, the scan must test each of the `all` conditions of the join, and no `PlaceHolderVar` that the scan computes can call a volatile function.
#[allow(clippy::too_many_arguments)]
pub(super) fn lateral<'q>(
    query: &Query,
    r: usize,
    outer: &Rel<'q>,
    inner: &Rel<'q>,
    (join, unique): (JoinKind, bool),
    moved: &[Qual<'q>],
    all: usize,
    stats: &Stats,
) -> Option<Memo<'q>> {
    if outer.node.cost.rows < 2.0 {
        return None;
    }
    if matches!(join, JoinKind::Semi | JoinKind::Anti) && !unique || unique && moved.len() != all {
        return None;
    }
    let placeholders = placeholders_at(query, r);
    if volatile_node(&inner.node)
        || moved.iter().any(|q| exprs::volatile(&q.expr))
        || placeholders.iter().any(exprs::volatile)
    {
        return None;
    }
    let mut keys: Vec<Expr> = Vec::new();
    let mut binary = false;
    for qual in moved {
        let (op, left, right) = exprs::binary(&qual.expr)?;
        let sides = (exprs::relations(left), exprs::relations(right));
        let key = if sides.0.is_subset(&outer.rels) && sides.1.is_subset(&inner.rels) {
            left
        } else if sides.1.is_subset(&outer.rels) && sides.0.is_subset(&inner.rels) {
            right
        } else {
            return None;
        };
        if !hashable(key.ty) {
            return None;
        }
        if !keys.iter().any(|k| k.same(key)) {
            keys.push(key.clone());
        }
        binary |= !exprs::can_hash(op);
    }
    let mut lateral = Vec::new();
    for value in placeholders {
        let reads = exprs::relations_at(&value, 0);
        if reads.iter().all(|k| *k == r) {
            continue;
        }
        if !reads.contains(&r) {
            lateral.push(value);
            continue;
        }
        let mut columns = Vec::new();
        exprs::columns_at(&value, 0, &mut columns);
        lateral.extend(columns.into_iter().filter(|c| !exprs::relations_at(c, 0).contains(&r)));
    }
    // The conditions that `pushdown` moved into the members of a `UNION ALL` are the `ppi_clauses` of the path in PG, whose columns come before the lateral columns.
    let members = query.relations[r].subquery.as_deref().filter(|s| s.set_op.is_some());
    for member in members.iter().flat_map(|s| &s.relations) {
        if let Some(filter) =
            member.subquery.as_deref().filter(|m| m.pushed).and_then(|m| m.filter.as_ref())
        {
            let mut columns = Vec::new();
            exprs::columns_at(filter, 2, &mut columns);
            lateral.extend(columns.into_iter().filter(|c| !exprs::relations_at(c, 0).contains(&r)));
        }
    }
    for (expr, depth) in super::lateral_exprs(&query.relations[r]) {
        let mut columns = Vec::new();
        exprs::columns_at(expr, depth, &mut columns);
        lateral.extend(columns.into_iter().filter(|c| !exprs::relations_at(c, 0).contains(&r)));
    }
    for key in lateral {
        if exprs::volatile(&key) || !hashable(key.ty) {
            return None;
        }
        if !keys.iter().any(|k| k.same(&key)) {
            keys.push(key);
        }
        binary = true;
    }
    if keys.is_empty() {
        return None;
    }
    let keys: Vec<&Expr> = keys.iter().collect();
    Some(rescan_cost(&inner.node.cost, keys, binary, outer.node.cost.rows, stats))
}

/// The expressions of the `PlaceHolderVar`s of the query that the scan of the relation `r` computes, in the order that they first come, as `placeholder_list` has them.
fn placeholders_at(query: &Query, r: usize) -> Vec<Expr> {
    let mut out: Vec<Expr> = Vec::new();
    for (expr, depth) in query.all_exprs(0) {
        expr.find(0, &mut |e, d| {
            if let ExprKind::PlaceHolder(p) = &e.kind
                && p.levels_up == d + depth
            {
                let at = exprs::eval_at(p);
                if at.len() == 1 && at.contains(&r) {
                    let mut value = p.expr.clone();
                    value.lower(p.levels_up);
                    if !out.iter().any(|o| o.same(&value)) {
                        out.push(value);
                    }
                }
            }
            None::<()>
        });
    }
    out
}

/// `lookup_type_cache` with `TYPECACHE_HASH_PROC` and `TYPECACHE_EQ_OPR`: true when the type has a hash function and an `=` operator. An array has a hash function when its element type has one.
fn hashable(ty: u32) -> bool {
    let element = types::element(types::base(ty));
    let hashed = |t: u32| default_opclass(t, HASH).is_some();
    hashed(ty) && (element == 0 || hashed(element)) && sort_group_operators(ty).1 != 0
}

/// `contain_volatile_functions` for the conditions of the scans of a node, as `get_memoize_path` tests `baserestrictinfo`.
fn volatile_node(node: &Node<'_>) -> bool {
    let cond: &[Item<'_>] = match &node.kind {
        Kind::IndexScan { cond, .. }
        | Kind::BitmapIndexScan { cond, .. }
        | Kind::TidScan { cond, .. } => cond,
        Kind::BitmapHeapScan { recheck, .. } => recheck,
        _ => &[],
    };
    cond.iter().chain(&node.filter).any(|i| exprs::volatile(&i.expr))
        || node.children.iter().any(volatile_node)
}

/// `cost_memoize_rescan`: the estimates of the cache and the costs of a scan of the node after the first. An entry of the cache has the rows of one scan, with the bytes of the node and of the keys, and the cache holds the entries that fit in `hash_mem`. With a default number of distinct values of the keys, each call has its own values, so that the cache finds no row. The hit ratio is the fraction of the calls that repeat a value, times the fraction of the values that the cache holds. A hit costs the fraction of the scan that it saves, and each call pays for the lookup, for the evictions and for one entry with its rows.
fn rescan_cost<'q>(
    sub: &Cost,
    keys: Vec<&Expr>,
    binary: bool,
    outer_rows: f64,
    stats: &Stats,
) -> Memo<'q> {
    let tuples = sub.rows;
    let calls = cost::clamp_rows(outer_rows);
    let mut entry_bytes = cost::byte_size(tuples, sub.width) + ENTRY_BYTES + TUPLE_BYTES * tuples;
    entry_bytes += keys.iter().map(|k| f64::from(stats.expr_width(k))).sum::<f64>();
    let entries = (cost::hash_mem() / entry_bytes).floor();
    let (mut distinct, default) = stats.groups_estimated(&keys, calls);
    if default {
        distinct = calls;
    }
    let capacity = distinct.min(entries).min(f64::from(u32::MAX)) as u32;
    let evict_ratio = 1.0 - entries.min(distinct) / distinct;
    let hit = (calls - distinct) / calls * (entries / distinct.max(entries));
    let mut total = sub.total * (1.0 - hit) + cost::CPU_OPERATOR;
    total += cost::CPU_TUPLE * evict_ratio;
    total += cost::CPU_OPERATOR / 10.0 * evict_ratio * tuples;
    total += cost::CPU_TUPLE + cost::CPU_OPERATOR * tuples;
    let startup = sub.startup * (1.0 - hit) + cost::CPU_TUPLE;
    let keys = keys.into_iter().map(|k| Item::owned(k.clone())).collect();
    let parts = Memoize { keys, binary, capacity, distinct, lookups: calls, hit };
    Memo { parts, rescan: (startup, total) }
}

/// `create_memoize_path`: the costs of the node, which are the costs of the node below with one `cpu_tuple_cost` for the first entry.
pub(super) fn path_cost(sub: Cost) -> Cost {
    Cost { startup: sub.startup + cost::CPU_TUPLE, total: sub.total + cost::CPU_TUPLE, ..sub }
}

/// `create_memoize_plan`: the `Memoize` node over the scan.
pub(super) fn node<'q>(child: Node<'q>, memo: &Memo<'q>) -> Node<'q> {
    let c = path_cost(child.cost);
    let mut node = Node::new(Kind::Memoize(Box::new(memo.parts.clone())), child.scope, vec![child]);
    node.cost = c;
    node
}

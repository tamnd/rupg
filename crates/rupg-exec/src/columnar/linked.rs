//! The join of the tables of a query with groups through keys that are unique on one side, on the columns of the tables (spec/14 section 14.8).
//!
//! Each table keeps the rows that pass its own conditions of `WHERE`, as [`super::selected`] finds them with the vectors. One table is the root: the other tables join it through a tree of `=` of integer keys, and the key of each `=` is unique in the rows of the table further from the root, as a primary key of a dimension table is. For each row of the root, a lookup in an array or in a hash table of the key of each table gives the one row of that table, or no row, and then the row of the root does not join. The join so gives the places of the rows of each table, and no tuple of values. The columns that the query reads are then gathered at these places into columns of the join, and the vector engine runs the groups and the aggregates on them as on the columns of one table, with the other conditions of `WHERE`.
//!
//! The groups of the vector engine do not depend on the order of the rows, and the query has the plan of the join, so that the groups come in the order of the hash table of the plan.

use std::sync::Arc;

use rupg_analyze::{BoolOp, Expr, ExprKind, FromItem, Query};
use rupg_column::builder::{self, Values};
use rupg_column::segment::Kind;
use rupg_column::stored::{Stored, VECTOR};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_types::{Value, oid};

use super::{
    Decoded, DecodedData, FxMap, Selected, Source, decode, kind_of, numeric_kind, plain,
    unsupported,
};
use crate::{Cache, reduce, scan};

/// No row.
const NONE: u32 = u32::MAX;

/// The rows of a query with groups whose tables join through keys that are unique on one side, or `None` when the query does not have this form.
///
/// # Errors
///
/// The error of a condition or of an expression of the query.
pub(crate) fn run(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    want: Option<usize>,
) -> Result<Option<Vec<Vec<Value>>>> {
    if !shape(query) {
        return Ok(None);
    }
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        scan::conjuncts(filter, &mut conds);
    }
    // The `OR` of two or more tables also gives a condition of each table.
    let derived = reduce::restrictions(&conds);
    let all: Vec<&Expr> = conds.iter().copied().chain(&derived).collect();
    let Some(plan) = reduce::plan(query, &all, session) else { return Ok(None) };
    let width = query.relations.len();
    let mut selected = Vec::with_capacity(width);
    for (index, relation) in query.relations.iter().enumerate() {
        match super::selected(relation, &plan.own[index], &plan.keys[index], session, cache)? {
            Some(found) if u32::try_from(found.places.len()).is_ok_and(|n| n < NONE) => {
                selected.push(found);
            }
            _ => return Ok(None),
        }
    }
    let Some(tree) = Tree::of(&plan.edges, &selected) else { return Ok(None) };
    let pos = tree.join(&selected);
    // The conditions of `WHERE` that the vectors test on the rows of the join: the conditions of two or more tables, except the `=` of the tree, and the conditions of no table.
    let residual: Vec<Expr> = conds
        .iter()
        .filter(|cond| {
            let relations = reduce::relations_of(cond);
            if relations.len() == 1 {
                return false;
            }
            !reduce::equal_columns(cond).is_some_and(|[(a, _), (b, _)]| tree.joins(a, b))
        })
        .map(|cond| (*cond).clone())
        .collect();
    let exprs = query.targets.iter().map(|t| &t.expr).chain(&query.having).chain(&residual);
    if exprs.chain(&query.offset).chain(&query.limit).any(|e| unsupported(e, false)) {
        return Ok(None);
    }
    // The columns of each table that the query reads after the join.
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    let exprs = query.targets.iter().map(|t| &t.expr).chain(&query.having).chain(&residual);
    for expr in exprs {
        expr.find(0, &mut |e, _| {
            if let ExprKind::Var(var) = e.kind
                && let Some(slot) = usize::try_from(var.attnum - 1)
                    .ok()
                    .and_then(|i| used.get_mut(var.relation)?.get_mut(i))
            {
                *slot = true;
            }
            None::<()>
        });
    }
    let Some(catalog) = session.catalog() else { return Ok(None) };
    let rows = pos[tree.root].len();
    let mut columns = Vec::new();
    let mut stored = Vec::new();
    let mut types = Vec::new();
    let mut map: Vec<Vec<i16>> = used.iter().map(|u| vec![0; u.len()]).collect();
    for (index, relation) in query.relations.iter().enumerate() {
        let Some(table) = catalog.relation(relation.oid) else { return Ok(None) };
        let mut wanted = Vec::new();
        for (column, _) in used[index].iter().enumerate().filter(|(_, u)| **u) {
            let def = &relation.columns[column];
            let Some(kind) = kind_of(def.ty).or_else(|| numeric_kind(def.ty, def.typmod)) else {
                return Ok(None);
            };
            if kind == Kind::Text && !table.columns.get(column).is_some_and(|c| plain(c.collation))
            {
                return Ok(None);
            }
            wanted.push((column, kind));
        }
        if wanted.is_empty() {
            continue;
        }
        let Some((_, read)) = session.table_columns(relation.oid, &wanted)? else {
            return Ok(None);
        };
        let at: Vec<usize> =
            pos[index].iter().map(|&p| selected[index].places[p as usize]).collect();
        for (&(column, _), read) in wanted.iter().zip(&read) {
            let (values, nulls) = gather(read, &at)?;
            let column_stored =
                builder::gathered(read.kind, &values, nulls.as_deref(), read.dict.clone())?;
            stored.push(Arc::new(column_stored));
            let def = &relation.columns[column];
            types.push(def.ty);
            columns.push(def.clone());
            map[index][column] = i16::try_from(columns.len()).map_err(|_| {
                Error::internal("a join reads more columns than a relation can have")
            })?;
        }
    }
    let mut virt = query.clone();
    let mut root = query.relations[tree.root].clone();
    root.columns = columns;
    virt.relations = vec![root];
    virt.from = vec![FromItem::Relation(0)];
    virt.filter = match residual.len() {
        0 => None,
        1 => residual.into_iter().next(),
        _ => Some(Expr {
            kind: ExprKind::Bool(BoolOp::And, residual),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        }),
    };
    virt.each_expr_mut(0, &mut |expr, depth| {
        expr.each_mut(depth, &mut |e, depth| {
            if let ExprKind::Var(var) = &mut e.kind
                && var.levels_up == depth
                && let Some(&to) =
                    usize::try_from(var.attnum - 1).ok().and_then(|i| map.get(var.relation)?.get(i))
            {
                var.attnum = to;
                var.relation = 0;
            }
            false
        });
    });
    let places = (0..stored.len()).collect();
    let tables = query
        .relations
        .iter()
        .zip(&selected)
        .map(|(relation, selected)| (relation.oid, selected.places.len()))
        .collect();
    let source = Source::new(rows, stored, places, types, tables);
    // The groups come in the order of the hash table of the plan of the join.
    session.share_plan(&virt, Some(query));
    let out = super::run_on(&virt, params, session, cache, want, source);
    session.share_plan(&virt, None);
    out
}

/// True when the query has groups over a list of relations in `FROM`, with no subquery, no set operation, no window and no write, and reads only the columns of its own relations.
fn shape(query: &Query) -> bool {
    if query.relations.len() < 2
        || !query.grouped
        || query.set_op.is_some()
        || !query.windows.is_empty()
        || !query.ctes.is_empty()
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || !query.grouping_sets.is_empty()
        || query.with_ties
        || query.has_target_srfs()
        || !query.barrier.is_empty()
        || query.from.len() != query.relations.len()
        || !query.from.iter().all(|item| matches!(item, FromItem::Relation(_)))
    {
        return false;
    }
    for item in &query.group {
        let expr = &query.targets[item.target].expr;
        // As in [`super::run`]: a `character` value with no length can differ from another in trailing spaces only.
        let padded = matches!(expr.kind, ExprKind::Var(_)) && expr.typmod > 4;
        if !plain(item.collation) || (expr.ty == oid::BPCHAR && !padded) {
            return false;
        }
    }
    let exprs = query
        .targets
        .iter()
        .map(|t| &t.expr)
        .chain(&query.filter)
        .chain(&query.having)
        .chain(&query.offset)
        .chain(&query.limit);
    for expr in exprs {
        let other = expr.find(0, &mut |e, depth| match &e.kind {
            ExprKind::SubLink(_) => Some(()),
            ExprKind::Var(var) if var.levels_up != depth || var.attnum <= 0 => Some(()),
            _ => None,
        });
        if other.is_some() {
            return false;
        }
    }
    true
}

/// The rows of a table by the value of a key that no two rows have.
enum Index {
    /// The row of each key from `min`, or [`NONE`].
    Dense {
        min: i64,
        rows: Vec<u32>,
    },
    One(FxMap<i64, u32>),
    Two(FxMap<(i64, i64), u32>),
}

impl Index {
    /// The index of the key `keys`, one vector of the rows for each column of the key, or `None` when two rows have the same key or the key has more than two columns. A row with a null in its key is not in the index, because it joins no row.
    fn of(keys: &[&[Option<i64>]]) -> Option<Index> {
        match keys {
            [one] => {
                let (mut min, mut max, mut count) = (i64::MAX, i64::MIN, 0usize);
                for &k in one.iter().flatten() {
                    min = min.min(k);
                    max = max.max(k);
                    count += 1;
                }
                let span = max.checked_sub(min).and_then(|s| usize::try_from(s).ok());
                if let Some(span) = span
                    && span < (count * 8).max(1 << 16)
                    && span < 1 << 27
                {
                    let mut rows = vec![NONE; span + 1];
                    for (row, k) in one.iter().enumerate() {
                        let Some(k) = k else { continue };
                        let slot = &mut rows[usize::try_from(k - min).ok()?];
                        if *slot != NONE {
                            return None;
                        }
                        *slot = u32::try_from(row).ok()?;
                    }
                    return Some(Index::Dense { min, rows });
                }
                let mut map = FxMap::default();
                map.reserve(count);
                for (row, k) in one.iter().enumerate() {
                    let Some(k) = k else { continue };
                    if map.insert(*k, u32::try_from(row).ok()?).is_some() {
                        return None;
                    }
                }
                Some(Index::One(map))
            }
            [a, b] => {
                let mut map = FxMap::default();
                map.reserve(a.len());
                for (row, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                    let (Some(x), Some(y)) = (x, y) else { continue };
                    if map.insert((*x, *y), u32::try_from(row).ok()?).is_some() {
                        return None;
                    }
                }
                Some(Index::Two(map))
            }
            _ => None,
        }
    }

    /// The row whose key is the key of the row `row` of `keys`, or [`NONE`].
    #[inline]
    fn find(&self, keys: &[&[Option<i64>]], row: usize) -> u32 {
        match (self, keys) {
            (Index::Dense { min, rows }, [one]) => one[row]
                .and_then(|k| usize::try_from(k.checked_sub(*min)?).ok())
                .and_then(|at| rows.get(at).copied())
                .unwrap_or(NONE),
            (Index::One(map), [one]) => one[row].and_then(|k| map.get(&k).copied()).unwrap_or(NONE),
            (Index::Two(map), [a, b]) => match (a[row], b[row]) {
                (Some(x), Some(y)) => map.get(&(x, y)).copied().unwrap_or(NONE),
                _ => NONE,
            },
            _ => NONE,
        }
    }
}

/// Two tables, or the places of the key columns of the two tables of an `=`.
type Pair = (usize, usize);

/// The join of a table to its parent in the tree through a key that is unique in the table.
struct Link {
    parent: usize,
    child: usize,
    /// The places in [`Selected::keys`] of the parent of the columns of the key.
    parent_keys: Vec<usize>,
    index: Index,
}

/// The tree of the joins of the tables.
struct Tree {
    root: usize,
    /// The links in the order of the join: the parent of each link comes before it.
    links: Vec<Link>,
    width: usize,
}

impl Tree {
    /// The tree of the joins of the `=` `edges` of the reduction, with each table as the root in turn from the table with the most rows, or `None` when no root gives a tree that reaches all the tables.
    fn of(edges: &[[(usize, usize); 2]], selected: &[Selected]) -> Option<Tree> {
        let width = selected.len();
        // The places of the key columns of the `=` of each pair of tables, with the table of the lower index first.
        let mut pairs: Vec<(Pair, Vec<Pair>)> = Vec::new();
        for &[(a, x), (b, y)] in edges {
            let (pair, columns) = if a < b { ((a, b), (x, y)) } else { ((b, a), (y, x)) };
            match pairs.iter_mut().find(|(p, _)| *p == pair) {
                Some((_, list)) if list.contains(&columns) => {}
                Some((_, list)) => list.push(columns),
                None => pairs.push((pair, vec![columns])),
            }
        }
        let mut order: Vec<usize> = (0..width).collect();
        order.sort_by_key(|&r| std::cmp::Reverse(selected[r].places.len()));
        for root in order {
            let mut done = vec![false; width];
            done[root] = true;
            let mut links = Vec::new();
            loop {
                let mut grew = false;
                for ((a, b), columns) in &pairs {
                    if done[*a] == done[*b] {
                        continue;
                    }
                    let (parent, child, parent_keys, child_keys): (usize, usize, Vec<_>, Vec<_>) =
                        if done[*a] {
                            (
                                *a,
                                *b,
                                columns.iter().map(|c| c.0).collect(),
                                columns.iter().map(|c| c.1).collect(),
                            )
                        } else {
                            (
                                *b,
                                *a,
                                columns.iter().map(|c| c.1).collect(),
                                columns.iter().map(|c| c.0).collect(),
                            )
                        };
                    let keys: Vec<&[Option<i64>]> =
                        child_keys.iter().map(|&k| selected[child].keys[k].as_slice()).collect();
                    let Some(index) = Index::of(&keys) else { continue };
                    links.push(Link { parent, child, parent_keys, index });
                    done[child] = true;
                    grew = true;
                }
                if !grew {
                    break;
                }
            }
            if done.iter().all(|d| *d) {
                return Some(Tree { root, links: ordered(links, selected), width });
            }
        }
        None
    }

    /// True when the tree joins the tables `a` and `b`, so that it tests all their `=`.
    fn joins(&self, a: usize, b: usize) -> bool {
        self.links
            .iter()
            .any(|l| (l.parent == a && l.child == b) || (l.parent == b && l.child == a))
    }

    /// For each table, the place in its [`Selected::places`] of its row in each tuple of the join.
    fn join(&self, selected: &[Selected]) -> Vec<Vec<u32>> {
        let mut pos: Vec<Vec<u32>> = vec![Vec::new(); self.width];
        #[allow(clippy::cast_possible_truncation)]
        let all: Vec<u32> = (0..selected[self.root].places.len() as u32).collect();
        pos[self.root] = all;
        let mut have = vec![false; self.width];
        have[self.root] = true;
        for link in &self.links {
            let keys: Vec<&[Option<i64>]> = link
                .parent_keys
                .iter()
                .map(|&k| selected[link.parent].keys[k].as_slice())
                .collect();
            let mut found: Vec<u32> =
                pos[link.parent].iter().map(|&p| link.index.find(&keys, p as usize)).collect();
            if found.contains(&NONE) {
                for (table, places) in pos.iter_mut().enumerate() {
                    if have[table] {
                        let mut at = found.iter();
                        places.retain(|_| at.next().is_some_and(|&f| f != NONE));
                    }
                }
                found.retain(|&f| f != NONE);
            }
            pos[link.child] = found;
            have[link.child] = true;
        }
        pos
    }
}

/// The links in the order of the join: from the links whose parent has its rows, the link to the table that keeps the fewest of its rows first, so that the first lookups remove the most tuples.
fn ordered(mut links: Vec<Link>, selected: &[Selected]) -> Vec<Link> {
    let mut out = Vec::with_capacity(links.len());
    let mut have: Vec<bool> = vec![false; selected.len()];
    if let Some(first) = links.first() {
        have[first.parent] = true;
    }
    while !links.is_empty() {
        let kept = |l: &Link| {
            let s = &selected[l.child];
            #[allow(clippy::cast_precision_loss)]
            let share = s.places.len() as f64 / s.rows.max(1) as f64;
            share
        };
        let next = links
            .iter()
            .enumerate()
            .filter(|(_, l)| have[l.parent])
            .min_by(|(_, x), (_, y)| kept(x).total_cmp(&kept(y)))
            .map_or(0, |(i, _)| i);
        let link = links.remove(next);
        have[link.child] = true;
        out.push(link);
    }
    out
}

/// The values of the column `stored` at the rows `at`, with the null flag of each row when a row is null.
fn gather(stored: &Stored, at: &[usize]) -> Result<(Values, Option<Vec<bool>>)> {
    let ascending = at.windows(2).all(|w| w[0] <= w[1]);
    let mut decoded: Vec<Option<Decoded>> = Vec::new();
    decoded.resize_with(stored.rows.div_ceil(VECTOR), || None);
    let mut last = usize::MAX;
    let mut out = match stored.kind.store() {
        rupg_column::segment::Store::Int => Values::Ints(Vec::with_capacity(at.len())),
        rupg_column::segment::Store::Float => Values::Floats(Vec::with_capacity(at.len())),
        rupg_column::segment::Store::Code => Values::Codes(Vec::with_capacity(at.len())),
    };
    let mut nulls = Vec::with_capacity(at.len());
    let mut any = false;
    for &row in at {
        let vector = row / VECTOR;
        let Some(slot) = decoded.get(vector) else {
            return Err(Error::internal("a row of a join is not in its table"));
        };
        if slot.is_none() {
            // Rows in their order read each vector one time, so only the last vector stays.
            if ascending && let Some(old) = decoded.get_mut(last) {
                *old = None;
            }
            let segment = row / rupg_column::segment::ROWS;
            let in_segment = (row % rupg_column::segment::ROWS) / VECTOR;
            decoded[vector] = Some(decode(stored, segment, in_segment)?);
            last = vector;
        }
        let Some(d) = &decoded[vector] else {
            return Err(Error::internal("a vector is not decoded"));
        };
        let i = row % VECTOR;
        let null = d.nulls.as_ref().is_some_and(|n| n[i]);
        nulls.push(null);
        any |= null;
        match (&mut out, &d.data) {
            (Values::Ints(o), DecodedData::Int(v)) => o.push(v[i]),
            (Values::Floats(o), DecodedData::Float(v)) => o.push(v[i]),
            (Values::Codes(o), DecodedData::Code(v)) => o.push(v[i]),
            _ => return Err(Error::internal("a vector has another store than its column")),
        }
    }
    Ok((out, any.then_some(nulls)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_gives_the_one_row_of_each_key() {
        let keys = [Some(5), None, Some(9), Some(6)];
        let index = Index::of(&[&keys]).unwrap();
        assert!(matches!(index, Index::Dense { .. }));
        let probe = [Some(9), Some(7), None, Some(5)];
        let found: Vec<u32> = (0..4).map(|r| index.find(&[&probe], r)).collect();
        assert_eq!(found, vec![2, NONE, NONE, 0]);
        let wide = [Some(1), Some(1 << 40)];
        let index = Index::of(&[&wide]).unwrap();
        assert!(matches!(index, Index::One(_)));
        assert_eq!(index.find(&[&[Some(1 << 40)]], 0), 1);
        assert!(Index::of(&[&[Some(3), Some(3)]]).is_none());
        let (a, b) = ([Some(1), Some(1)], [Some(2), Some(3)]);
        let index = Index::of(&[&a, &b]).unwrap();
        assert_eq!(index.find(&[&[Some(1)], &[Some(3)]], 0), 1);
        assert!(Index::of(&[&[Some(1), Some(1)], &[Some(2), Some(2)]]).is_none());
    }
}

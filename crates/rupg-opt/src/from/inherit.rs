//! `set_append_rel_size` and `set_append_rel_pathlist` for the members of an inheritance tree and the partitions of a partitioned table: each member is a table of its own, with its size, its statistics and its indexes, and the `Append` reads the cheapest scan of each member.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use rupg_plan::{Cost, Kind, Node, Statistic};

use super::{Qual, Rel, Stats, dummy, index, tid, variable_ranges};
use crate::{Index, Level, Member, Members, cost};

/// The column numbers of a member for the columns of its parent, as `make_inh_translation_list` gives them.
struct Columns<'m> {
    /// The column of the member for each column of the parent.
    attnums: &'m [i16],
    /// True when each column of the parent has the same number in the member.
    same: bool,
}

impl<'m> Columns<'m> {
    fn new(member: &'m Member) -> Columns<'m> {
        let same = member
            .attnums
            .iter()
            .enumerate()
            .all(|(at, &a)| a == 0 || usize::try_from(a).ok() == at.checked_add(1));
        Columns { attnums: &member.attnums, same }
    }

    /// The column of the parent with the column `attnum` of the member. A system column has the same number.
    fn parent(&self, attnum: i16) -> Option<i16> {
        if attnum <= 0 {
            return Some(attnum);
        }
        let at = self.attnums.iter().position(|&a| a == attnum)?;
        i16::try_from(at.checked_add(1)?).ok()
    }

    /// The index of the member with the columns of the parent. An index with a column that the parent does not have can not read a condition of the parent. An index with an expression or a predicate counts only when the columns have the same numbers.
    fn index(&self, index: &Index) -> Option<Index> {
        if !self.same && (!index.exprs.is_empty() || !index.predicate.is_empty()) {
            return None;
        }
        let map = |keys: &[i16]| -> Option<Vec<i16>> {
            keys.iter().map(|&k| if k == 0 { Some(0) } else { self.parent(k) }).collect()
        };
        let mut out = index.clone();
        out.keys = map(&index.keys)?;
        out.columns = map(&index.columns)?;
        Some(out)
    }

    /// The statistics of the columns of the member by the column numbers of the parent.
    fn statistics(&self, all: Vec<Statistic>) -> BTreeMap<i16, Statistic> {
        all.into_iter()
            .filter(|s| !s.inherit)
            .filter_map(|s| Some((self.parent(s.attnum)?, s)))
            .collect()
    }
}

/// `set_append_rel_size` and `create_append_plan`: the cheapest scan of each member of the inheritance tree of the relation `r`, under an `Append` when there are two or more, each with the conditions of the relation. Each member has the estimates of its own statistics, and its scans by its indexes and by `ctid`, as `set_plain_rel_pathlist`. The relation has the rows of its members, and the width of each column of the relation is the mean of its widths in the members, by their rows. With no member the relation is a `Result` with no rows, as `set_dummy_rel_pathlist`.
pub(super) fn scan<'q>(
    level: &Level<'q>,
    r: usize,
    (base, quals): (&[Qual<'q>], &[Qual<'q>]),
    members: Vec<Member>,
    hooks: Option<&dyn Members>,
    stats: &mut Stats,
) -> Rel<'q> {
    let relation = &level.query.relations[r];
    let (qual_startup, qual_cost) = stats.qual_costs(quals.iter().map(|q| &*q.expr));
    let per_row = cost::CPU_TUPLE + qual_cost;
    let unique = stats.unique.columns.clone();
    let (mut tuples, mut rows, mut size) = (0.0, 0.0, 0.0);
    let mut sizes: BTreeMap<i16, f64> = BTreeMap::new();
    let mut children = Vec::with_capacity(members.len());
    for member in &members {
        let columns = Columns::new(member);
        let indexes: Vec<Index> = hooks
            .map(|m| m.indexes(member.oid))
            .unwrap_or_default()
            .iter()
            .filter_map(|i| columns.index(i))
            .collect();
        let statistics =
            columns.statistics(hooks.map(|m| m.statistics(member.oid)).unwrap_or_default());
        let (pages, count) = cost::member_size(member);
        stats.columns.insert(r, Rc::new(statistics));
        stats.pages.insert(r, pages);
        stats.ranges.retain(|&(at, _), _| at != r);
        for index in &indexes {
            if let ([key], true, false) = (index.keys.as_slice(), index.unique, index.partial)
                && *key > 0
            {
                stats.unique.columns.insert((r, *key));
            }
        }
        // `get_actual_variable_range` reads the index of the member by the column numbers of the member.
        if columns.same {
            for q in base {
                variable_ranges(hooks, &q.expr, (r, member.oid), &indexes, stats);
            }
        }
        let eq = stats.estimate(r, count);
        let selectivity = cost::list_selectivity_by(base.iter().map(|q| &*q.expr), &eq);
        let n = cost::clamp_rows(count * selectivity);
        let mut seq = Node::new(Kind::SeqScan(r), level.scope, Vec::new());
        seq.filter = quals.iter().map(Qual::item).collect();
        let total = qual_startup + count * per_row + pages * cost::SEQ_PAGE;
        seq.cost = Cost { startup: qual_startup, total, rows: n, width: 0 };
        seq.disabled = !crate::enable().seqscan;
        let table = index::Table {
            r,
            pages,
            tuples: count,
            rows: n,
            quals: base.to_vec(),
            eq: &eq,
            outer: BTreeSet::new(),
            loops: 1.0,
        };
        let (tids, current) = tid::paths(level, &table, stats);
        let mut found: Vec<Node<'q>> = tids.into_iter().map(|rel| rel.node).collect();
        if !current {
            found.push(seq);
            found.extend(
                index::paths(level, &table, &indexes, stats).into_iter().map(|rel| rel.node),
            );
        }
        // `set_cheapest`: the scan with the fewest disabled nodes and then the least total cost.
        let mut node = found
            .into_iter()
            .reduce(|best, n| {
                let key = |n: &Node<'_>| (n.disabled_nodes(), n.cost.total);
                if key(&n) < key(&best) { n } else { best }
            })
            .unwrap_or_else(|| dummy(level.scope, &BTreeSet::from([r]), false));
        node.member = Some(member.oid);
        // `set_rel_width` of the member: the width of a column from the statistics of the member, or else from its type.
        let width = |attnum: i16| -> i32 {
            match attnum {
                0 => {
                    let row =
                        member.columns.iter().map(|&(ty, typmod)| cost::type_width(ty, typmod));
                    cost::WHOLE_ROW_HEADER + row.sum::<i32>()
                }
                a if a < 0 => cost::column_width(relation, a),
                a => {
                    let analyzed = stats.columns.get(&r).and_then(|c| c.get(&a)).map(|s| s.width);
                    let column = usize::try_from(a - 1).ok().and_then(|c| relation.columns.get(c));
                    analyzed
                        .filter(|w| *w > 0)
                        .unwrap_or_else(|| column.map_or(32, |c| cost::type_width(c.ty, c.typmod)))
                }
            }
        };
        let read = stats.read_columns(r);
        let mut node_width = stats.placeholder_width(r);
        for attnum in read {
            let w = width(attnum);
            node_width += w;
            *sizes.entry(attnum).or_default() += f64::from(w) * n;
        }
        node.cost.width = node_width;
        size += f64::from(node_width) * n;
        tuples += count;
        rows += n;
        children.push(node);
        stats.unique.columns.clone_from(&unique);
    }
    stats.columns.remove(&r);
    stats.pages.remove(&r);
    stats.ranges.retain(|&(at, _), _| at != r);
    stats.rows.insert(r, (tuples, rows));
    if rows > 0.0 {
        for (attnum, s) in &sizes {
            stats.set_width(r, *attnum, (s / rows).round_ties_even() as i32);
        }
    }
    let rels = BTreeSet::from([r]);
    let node = match children.len() {
        0 => dummy(level.scope, &rels, false),
        1 => children.remove(0),
        _ => {
            // `cost_append`: the start of the first member, and the costs of all the members with half of `cpu_tuple_cost` for each row.
            let startup = children[0].cost.startup;
            let total: f64 = children.iter().map(|c| c.cost.total).sum::<f64>()
                + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
            let width = if rows > 0.0 { (size / rows).round_ties_even() as i32 } else { 0 };
            let mut node = Node::new(Kind::Append, level.scope, children);
            node.cost = Cost { startup, total, rows, width };
            node
        }
    };
    Rel { node, rels, order: Vec::new() }
}

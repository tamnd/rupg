//! `set_append_rel_size` and `set_append_rel_pathlist` for the members of an inheritance tree and the partitions of a partitioned table: each member is a table of its own, with its size, its statistics and its indexes. The `Append` reads the cheapest scan of each member, and a `Merge Append` or an ordered `Append` reads the scans of the members in an order.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use rupg_analyze::{Expr, ExprKind};
use rupg_plan::{Cost, Item, Kind, Node, SortKey, Statistic};

use rupg_types::Value;

use super::classes::Joined;
use super::index::Param;
use super::paths::costs;
use super::search::{self, Semis};
use super::{Base, OrderKey, Paths, Qual, Rel, Stats, cheaper, dummy, index, tid, variable_ranges};
use crate::{Index, Level, Member, Members, cost, parallel};

/// The least and the greatest value of a column of a member, by the relation and the column, as `get_actual_variable_range` gives them.
type Ranges = Vec<((usize, i16), Option<(Value, Value)>)>;

/// A member of a table with members, with its table, its statistics and its scans, from which [`param_scans`] makes the parameterized scans of the member.
#[derive(Debug)]
pub(super) struct MemberBase<'q> {
    oid: u32,
    base: Base<'q>,
    columns: Rc<BTreeMap<i16, Statistic>>,
    ranges: Ranges,
    unique: BTreeSet<(usize, i16)>,
    plans: Vec<Rel<'q>>,
    width: i32,
}

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

/// `set_append_rel_size` and `add_paths_to_append_rel`: the scans of each member of the inheritance tree of the relation `r`, each with the conditions of the relation, under an `Append` of the cheapest scans when there are two or more, and the ordered plans of [`ordered`]. Each member has the estimates of its own statistics, and its scans by its indexes and by `ctid`, as `set_plain_rel_pathlist`. The relation has the rows of its members, and the width of each column of the relation is the mean of its widths in the members, by their rows. With no member the relation is a `Result` with no rows, as `set_dummy_rel_pathlist`, and with one member the relation has the scans of the member.
pub(super) fn scan<'q>(
    level: &Level<'q>,
    r: usize,
    (base, quals): (&[Qual<'q>], &[Qual<'q>]),
    members: Vec<Member>,
    hooks: Option<&dyn Members>,
    parallel: bool,
    stats: &mut Stats,
) -> (Vec<Rel<'q>>, Vec<MemberBase<'q>>, Vec<(Rel<'q>, i32)>) {
    let relation = &level.query.relations[r];
    let enable = crate::enable();
    let (qual_startup, qual_cost) = stats.qual_costs(quals.iter().map(|q| &*q.expr));
    let per_row = cost::CPU_TUPLE + qual_cost;
    let unique = stats.unique.columns.clone();
    let (mut tuples, mut rows, mut size) = (0.0, 0.0, 0.0);
    let mut sizes: BTreeMap<i16, f64> = BTreeMap::new();
    let rels = BTreeSet::from([r]);
    let mut lists: Vec<Vec<Rel<'q>>> = Vec::with_capacity(members.len());
    let mut bases = Vec::with_capacity(members.len());
    let mut partials = Vec::with_capacity(members.len());
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
        stats.all_visible.insert(r, cost::all_visible(member.size, pages));
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
        // `create_plain_partial_paths` of the member, with its own `parallel_workers` option and its pages.
        let option = hooks.and_then(|m| m.parallel(member.oid)).filter(|_| parallel);
        let mut partial = Vec::new();
        if let Some(option) = option {
            let workers = parallel::workers(&enable, pages, option);
            if workers > 0 {
                let divisor = parallel::divisor(&enable, workers);
                let mut node = seq.clone();
                node.parallel = true;
                node.cost.total = qual_startup + count * per_row / divisor + pages * cost::SEQ_PAGE;
                node.cost.rows = cost::clamp_rows(n / divisor);
                partial.push((Rel { node, rels: rels.clone(), order: Vec::new() }, workers));
            }
        }
        let (mut found, current) = tid::paths(level, &table, stats);
        if !current {
            found.push(Rel { node: seq, rels: rels.clone(), order: Vec::new() });
            let (scans, index_partials) =
                index::partial_paths(level, &table, &indexes, stats, option);
            found.extend(scans);
            partial.extend(index_partials);
        }
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
        // `add_path` and `set_cheapest` of the member: the cheapest scan comes first.
        let mut paths = Paths::new(stats.startup);
        for mut rel in found {
            rel.node.member = Some(member.oid);
            rel.node.cost.width = node_width;
            paths.add(costs(&rel.node), rel.order.clone(), rel);
        }
        for (rel, _) in &mut partial {
            rel.node.member = Some(member.oid);
            rel.node.cost.width = node_width;
        }
        partials.push(partial.into_iter().min_by(|(a, _), (b, _)| cheaper(&a.node, &b.node)));
        let mut kept = paths.kept();
        if kept.is_empty() {
            let mut node = dummy(level.scope, &rels, false);
            node.member = Some(member.oid);
            kept.push(Rel { node, rels: rels.clone(), order: Vec::new() });
        }
        let ranges = stats.ranges.range((r, i16::MIN)..=(r, i16::MAX));
        bases.push(MemberBase {
            oid: member.oid,
            base: Base {
                pages,
                tuples: count,
                rows: n,
                quals: base.to_vec(),
                indexes,
                members: Vec::new(),
            },
            columns: stats.columns.get(&r).cloned().unwrap_or_default(),
            ranges: ranges.map(|(k, v)| (*k, v.clone())).collect(),
            unique: stats.unique.columns.clone(),
            plans: kept.clone(),
            width: node_width,
        });
        lists.push(kept);
        size += f64::from(node_width) * n;
        tuples += count;
        rows += n;
        stats.unique.columns.clone_from(&unique);
    }
    // `examine_simple_variable`: the relation has the statistics with `stainherit` true of its table.
    let inherited = hooks.map(|m| m.statistics(relation.oid)).unwrap_or_default();
    let inherited: BTreeMap<i16, Statistic> =
        inherited.into_iter().filter(|s| s.inherit).map(|s| (s.attnum, s)).collect();
    stats.columns.insert(r, Rc::new(inherited));
    stats.pages.remove(&r);
    stats.all_visible.remove(&r);
    stats.ranges.retain(|&(at, _), _| at != r);
    stats.rows.insert(r, (tuples, rows));
    stats.cache.take();
    if rows > 0.0 {
        for (attnum, s) in &sizes {
            let width = (s / rows).round_ties_even() as i32;
            stats.set_width(r, *attnum, width);
            crate::set_width(relation, *attnum, width);
        }
    }
    match lists.len() {
        0 => {
            let node = dummy(level.scope, &rels, false);
            return (vec![Rel { node, rels, order: Vec::new() }], Vec::new(), Vec::new());
        }
        1 => return (lists.remove(0), Vec::new(), partials.into_iter().flatten().collect()),
        _ => {}
    }
    let width = if rows > 0.0 { (size / rows).round_ties_even() as i32 } else { 0 };
    let partials = if parallel {
        partial_appends(level.scope, &rels, &lists, partials, width)
    } else {
        Vec::new()
    };
    let cheapest = lists.iter().filter_map(|l| l.first()).map(|rel| rel.node.clone()).collect();
    let mut paths = Paths::new(stats.startup);
    let node = append(level.scope, cheapest, false, width);
    paths.add(costs(&node), Vec::new(), Rel { node, rels: rels.clone(), order: Vec::new() });
    let oids: Vec<u32> = members.iter().map(|m| m.oid).collect();
    crate::set_member_places(&oids);
    let partition = hooks.and_then(|m| m.ordered_partitions(relation.oid, &oids));
    let partition = partition.map(|keys| sort_keys(r, relation, base, keys, stats));
    let ordered = Ordered { level, r, lists: &lists, partition: partition.as_deref(), width };
    for rel in ordered.plans(stats) {
        paths.add(costs(&rel.node), rel.order.clone(), rel);
    }
    (paths.kept(), bases, partials)
}

/// The partial paths of `add_paths_to_append_rel` for a table whose members have the plans `lists` and the cheapest partial paths `partials`. When each member has a partial path, an `Append` of them is a partial path, a `Parallel Append` with `enable_parallel_append`. With `enable_parallel_append`, a `Parallel Append` reads for each member its cheapest partial path or its cheapest plan when that plan costs less, when one member has such a plan. A `Parallel Append` asks for one worker more than the base 2 logarithm of the members, as many as `max_parallel_workers_per_gather` allows.
fn partial_appends<'q>(
    scope: usize,
    rels: &BTreeSet<usize>,
    lists: &[Vec<Rel<'q>>],
    partials: Vec<Option<(Rel<'q>, i32)>>,
    width: i32,
) -> Vec<(Rel<'q>, i32)> {
    let enable = crate::enable();
    let log = i32::try_from(usize::BITS - lists.len().leading_zeros()).unwrap_or(i32::MAX);
    let spread = |workers: i32| workers.max(log).min(enable.max_parallel_workers_per_gather);
    let mut out = Vec::new();
    let mut partial_rows = None;
    if partials.iter().all(Option::is_some) {
        let children: Vec<(Node<'q>, i32)> =
            partials.iter().flatten().map(|(rel, w)| (rel.node.clone(), *w)).collect();
        let workers = children.iter().map(|c| c.1).max().unwrap_or(0);
        let (node, workers) = if enable.parallel_append {
            let workers = spread(workers);
            (parallel_append(scope, Vec::new(), children, workers, width), workers)
        } else {
            (append(scope, children.into_iter().map(|c| c.0).collect(), false, width), workers)
        };
        partial_rows = Some(node.cost.rows);
        out.push((Rel { node, rels: rels.clone(), order: Vec::new() }, workers));
    }
    if !enable.parallel_append {
        return out;
    }
    let mut whole = Vec::new();
    let mut parts = Vec::new();
    for (list, partial) in lists.iter().zip(&partials) {
        let plain = list.first();
        match partial {
            Some((rel, w)) if plain.is_none_or(|p| rel.node.cost.total < p.node.cost.total) => {
                parts.push((rel.node.clone(), *w));
            }
            _ => match plain {
                Some(plain) => whole.push(plain.node.clone()),
                None => return out,
            },
        }
    }
    if !whole.is_empty() {
        let workers = spread(parts.iter().map(|c| c.1).max().unwrap_or(0));
        let mut node = parallel_append(scope, whole, parts, workers, width);
        if let Some(rows) = partial_rows {
            node.cost.rows = rows;
        }
        out.push((Rel { node, rels: rels.clone(), order: Vec::new() }, workers));
    }
    out
}

/// `create_append_path` and `cost_append` of a `Parallel Append` with `workers` workers of the plans `whole`, which one process reads each, and the partial paths `parts`. The plans come first, in the order of their total costs from the greatest, and then the partial paths, in the order of their start costs from the greatest. The `Append` starts when the first of the plans that the first workers read starts. The rows of each plan are a share of the rows for each process, and a partial path has the rows of its own processes for the processes of the `Append`. The partial paths cost their total costs, and the plans cost the greatest sum that one worker reads when each worker takes the next plan as it ends, as `append_nonpartial_cost` gives it.
fn parallel_append<'q>(
    scope: usize,
    mut whole: Vec<Node<'q>>,
    mut parts: Vec<(Node<'q>, i32)>,
    workers: i32,
    width: i32,
) -> Node<'q> {
    let enable = crate::enable();
    let key = |n: &Node<'_>, first: f64, second: f64| (n.disabled_nodes(), first, second);
    whole.sort_by(|a, b| {
        let (a, b) = (key(a, a.cost.total, a.cost.startup), key(b, b.cost.total, b.cost.startup));
        b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
    });
    parts.sort_by(|(a, _), (b, _)| {
        let (a, b) = (key(a, a.cost.startup, a.cost.total), key(b, b.cost.startup, b.cost.total));
        b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
    });
    let divisor = parallel::divisor(&enable, workers);
    let first_partial = whole.len();
    let totals: Vec<f64> = whole.iter().map(|n| n.cost.total).collect();
    let (mut startup, mut total, mut rows) = (0.0, 0.0, 0.0);
    let all = whole.iter().map(|n| (n, 0)).chain(parts.iter().map(|(n, w)| (n, *w)));
    for (i, (child, child_workers)) in all.enumerate() {
        let c = child.cost;
        if i == 0 {
            startup = c.startup;
        } else if i32::try_from(i).is_ok_and(|i| i < workers) {
            startup = f64::min(startup, c.startup);
        }
        if i < first_partial {
            rows += c.rows / divisor;
        } else {
            rows += c.rows * (parallel::divisor(&enable, child_workers) / divisor);
            total += c.total;
        }
        rows = cost::clamp_rows(rows);
    }
    total += nonpartial_cost(&totals, workers);
    total += cost::CPU_TUPLE * cost::APPEND_CPU * rows;
    let children = whole.into_iter().chain(parts.into_iter().map(|(n, _)| n)).collect();
    let mut node = Node::new(Kind::Append, scope, children);
    node.cost = Cost { startup, total, rows, width };
    node.parallel = true;
    node
}

/// `append_nonpartial_cost`: the greatest cost that one of `workers` workers reads of the plans with the total costs `totals`, from the greatest. Each of the first workers takes one plan, and each next plan goes to the worker with the least cost.
fn nonpartial_cost(totals: &[f64], workers: i32) -> f64 {
    let len = usize::try_from(workers).unwrap_or(0).min(totals.len());
    if len == 0 {
        return 0.0;
    }
    let mut sums = totals[..len].to_vec();
    let mut min = len - 1;
    for &t in &totals[len..] {
        sums[min] += t;
        min = (0..len).fold(0, |m, i| if sums[i] < sums[m] { i } else { m });
    }
    sums.iter().copied().fold(sums[0], f64::max)
}

/// The parameterized `Append` paths of `add_paths_to_append_rel` for the table `r` with members, for the inner side of a nested loop. Each member has its parameterized scans, from its own statistics and indexes, as [`search::param_scans`]. For each set of outer relations of a scan of a member, the `Append` reads the scan of each member from [`child`], or the table has no `Append` for the set when a member has no such scan. The `Append` tests the conditions of the joins of the set, as `get_appendrel_parampathinfo`, and the scans that `add_path` keeps with the plans of the table are the parameterized scans of the table.
pub(super) fn param_scans<'q>(
    level: &Level<'q>,
    (r, parent, plans, semi): (usize, &Base<'q>, &[Rel<'q>], bool),
    (joins, semis): (&[Qual<'q>], &[Semis]),
    classes: &[Joined<'q>],
    prohibited: &BTreeSet<usize>,
    stats: &Stats,
) -> Vec<Param<'q>> {
    let mut lists = Vec::with_capacity(parent.members.len());
    let mut outers: Vec<BTreeSet<usize>> = Vec::new();
    for m in &parent.members {
        let mut s = stats.clone();
        s.cache.take();
        s.columns.insert(r, m.columns.clone());
        s.pages.insert(r, m.base.pages);
        s.ranges.retain(|&(at, _), _| at != r);
        s.ranges.extend(m.ranges.iter().cloned());
        s.unique.columns.clone_from(&m.unique);
        let own: Vec<Joined<'q>> = classes.iter().map(Joined::child).collect();
        let found = (r, &m.base, m.plans.as_slice(), semi);
        let mut params = search::param_scans(level, found, (joins, semis), &own, prohibited, &s);
        for p in &mut params {
            p.rel.node.member = Some(m.oid);
            p.rel.node.cost.width = m.width;
            if !outers.contains(&p.outer) {
                outers.push(p.outer.clone());
            }
        }
        lists.push((s, own, params));
    }
    // The width of the relation is the mean of the widths of the members, by their rows, as in [`scan`].
    let size: f64 = parent.members.iter().map(|m| f64::from(m.width) * m.base.rows).sum();
    let width = if parent.rows > 0.0 { (size / parent.rows).round_ties_even() as i32 } else { 0 };
    let eq = stats.estimate(r, parent.tuples);
    let mut found = Vec::new();
    for outer in outers {
        let children: Option<Vec<Node<'q>>> = parent
            .members
            .iter()
            .zip(&lists)
            .map(|(m, (s, own, params))| {
                child(level, (r, m, params), &outer, (joins, semis), own, s)
            })
            .collect();
        let Some(children) = children else { continue };
        let t = search::param_table(r, parent, &eq, (joins, semis), classes, stats, &outer);
        let enforced = t.quals.get(parent.quals.len()..).unwrap_or_default().to_vec();
        let node = append(level.scope, children, false, width);
        let rel = Rel { node, rels: BTreeSet::from([r]), order: Vec::new() };
        found.push(Param { rel, outer, enforced, indexed: false });
    }
    if found.is_empty() {
        return found;
    }
    let mut paths = Paths::with_params(stats.startup, semi);
    for rel in plans {
        let keys = rel.order.clone();
        paths.add_param(costs(&rel.node), keys, BTreeSet::new(), rel.node.cost.rows, None);
    }
    for p in found {
        let (keys, outer) = (p.rel.order.clone(), p.outer.clone());
        paths.add_param(costs(&p.rel.node), keys, outer, p.rel.node.cost.rows, Some(p));
    }
    paths.params().into_iter().flatten().collect()
}

/// `get_cheapest_parameterized_child_path`: the scan of the member `m` with the values of the relations `outer` that costs the least in total. When the cheapest scan with no more of these values has other values, each scan of the member gets the conditions of the other values, as `reparameterize_path`, and the cheapest of them wins. A sequential scan, and an index scan or a bitmap heap scan with no parameters, can get them.
fn child<'q>(
    level: &Level<'q>,
    (r, m, params): (usize, &MemberBase<'q>, &[Param<'q>]),
    outer: &BTreeSet<usize>,
    (joins, semis): (&[Qual<'q>], &[Semis]),
    classes: &[Joined<'q>],
    stats: &Stats,
) -> Option<Node<'q>> {
    let none = BTreeSet::new();
    let all: Vec<(&Node<'q>, &BTreeSet<usize>)> = m
        .plans
        .iter()
        .map(|p| (&p.node, &none))
        .chain(params.iter().map(|p| (&p.rel.node, &p.outer)))
        .filter(|(_, o)| o.is_subset(outer))
        .collect();
    // `compare_path_costs` by the total cost: the disabled nodes, then the total cost, then the start cost.
    let cheaper = |a: &Node<'q>, b: &Node<'q>| {
        let (a, b) = (costs(a), costs(b));
        (a.0, a.2, a.1).partial_cmp(&(b.0, b.2, b.1)).is_some_and(|o| o.is_lt())
    };
    let mut cheapest: Option<(&Node<'q>, &BTreeSet<usize>)> = None;
    for &(node, o) in &all {
        if cheapest.is_none_or(|(c, _)| cheaper(node, c)) {
            cheapest = Some((node, o));
        }
    }
    let (node, o) = cheapest?;
    if o == outer {
        return Some(node.clone());
    }
    let eq = stats.estimate(r, m.base.tuples);
    let mut t = search::param_table(r, &m.base, &eq, (joins, semis), classes, stats, outer);
    // `get_cheapest_parameterized_child_path` gives a `loop_count` of 1.
    t.loops = 1.0;
    let plain = index::Table {
        r,
        pages: m.base.pages,
        tuples: m.base.tuples,
        rows: m.base.rows,
        quals: m.base.quals.clone(),
        eq: &eq,
        outer: BTreeSet::new(),
        loops: 1.0,
    };
    let extra = t.quals.get(m.base.quals.len()..).unwrap_or_default();
    let mut best: Option<Node<'q>> = None;
    for (node, o) in all {
        if best.as_ref().is_some_and(|b| !cheaper(node, b)) {
            continue;
        }
        let node = if o == outer {
            node.clone()
        } else if matches!(node.kind, Kind::SeqScan(_)) {
            // `create_seqscan_path` with the parameters: the conditions of the joins in the filter, for each tuple of the member.
            let (startup, per_tuple) = stats.qual_costs(extra.iter().map(|q| &*q.expr));
            let mut seq = node.clone();
            // `order_qual_clauses` of the conditions of the member and of the joins.
            let mut quals = t.quals.clone();
            stats.sort_quals(&mut quals);
            seq.filter = quals.iter().map(Qual::item).collect();
            seq.cost.startup += startup;
            seq.cost.total += startup + per_tuple * m.base.tuples;
            seq.cost.rows = t.rows;
            if best.as_ref().is_some_and(|b| !cheaper(&seq, b)) {
                continue;
            }
            seq
        } else if o.is_empty()
            && let Some(mut scan) =
                index::reparameterize(level, (&plain, &t), &m.base.indexes, node, stats)
        {
            (scan.member, scan.cost.width) = (node.member, node.cost.width);
            if best.as_ref().is_some_and(|b| !cheaper(&scan, b)) {
                continue;
            }
            scan
        } else {
            continue;
        };
        best = Some(node);
    }
    best
}

/// `cost_append`: the rows and the costs of the members, with half of `cpu_tuple_cost` for each row. An `Append` with no order starts with its first member, and an ordered `Append` starts with all of them, so that a `LIMIT` does not seem to read only the first member at little cost.
fn append<'q>(scope: usize, children: Vec<Node<'q>>, ordered: bool, width: i32) -> Node<'q> {
    let rows: f64 = children.iter().map(|c| c.cost.rows).sum();
    let startup = if ordered {
        children.iter().map(|c| c.cost.startup).sum()
    } else {
        children.first().map_or(0.0, |c| c.cost.startup)
    };
    let total: f64 = children.iter().map(|c| c.cost.total).sum::<f64>()
        + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
    let mut node = Node::new(Kind::Append, scope, children);
    node.cost = Cost { startup, total, rows, width };
    node
}

/// `cost_merge_append`: the heap of the first row of each member, with two operator calls for each comparison, then a comparison in the heap for each row, with half of `cpu_tuple_cost` for each row.
fn merge_append<'q>(
    scope: usize,
    children: Vec<Node<'q>>,
    keys: Vec<SortKey<'q>>,
    width: i32,
) -> Node<'q> {
    let n = children.len().max(2) as f64;
    let log_n = n.log2();
    let comparison = 2.0 * cost::CPU_OPERATOR;
    let rows: f64 = children.iter().map(|c| c.cost.rows).sum();
    let first = comparison * n * log_n;
    let run = rows * comparison * log_n + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
    let input_startup: f64 = children.iter().map(|c| c.cost.startup).sum();
    let input_total: f64 = children.iter().map(|c| c.cost.total).sum();
    let mut node = Node::new(Kind::MergeAppend(keys), scope, children);
    node.cost =
        Cost { startup: first + input_startup, total: first + run + input_total, rows, width };
    node
}

/// `build_partition_pathkeys`: the columns of the key of the partitions with no column that `partkey_is_bool_constant_for_query` makes a constant. Such a column is a `boolean` column in no equivalence class of the query, and the condition of the table is the column or `NOT` of the column, as `matches_boolean_partition_clause`.
fn sort_keys(
    r: usize,
    relation: &rupg_analyze::Relation,
    base: &[Qual<'_>],
    keys: Vec<i16>,
    stats: &Stats,
) -> Vec<i16> {
    let constant = |attnum: i16| {
        let column = usize::try_from(attnum - 1).ok().and_then(|c| relation.columns.get(c));
        if column.is_none_or(|c| c.ty != rupg_types::oid::BOOL) {
            return false;
        }
        let var = |e: &Expr| matches!(e.kind, ExprKind::Var(v) if v.relation == r && v.levels_up == 0 && v.attnum == attnum);
        let Some(key) = base.iter().map(|q| &*q.expr).find_map(|e| match &e.kind {
            _ if var(e) => Some(e),
            ExprKind::Bool(rupg_analyze::BoolOp::Not, args) => match args.as_slice() {
                [arg] if var(arg) => Some(arg),
                _ => None,
            },
            _ => None,
        }) else {
            return false;
        };
        !stats.has_class(key)
    };
    keys.into_iter().filter(|&k| !constant(k)).collect()
}

/// True when the two orders have the same keys.
fn same_order(a: &[OrderKey], b: &[OrderKey]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| y.exprs.iter().any(|e| x.sorts(e, y.desc, y.nulls_first)))
}

/// `pathkeys_contained_in`: true when the order starts with the keys.
fn starts_with(order: &[OrderKey], keys: &[OrderKey]) -> bool {
    order.get(..keys.len()).is_some_and(|first| same_order(first, keys))
}

/// The plans of the members of an append relation, for its ordered plans.
struct Ordered<'a, 'q> {
    level: &'a Level<'q>,
    r: usize,
    /// The plans of each member that `add_path` keeps, the cheapest first.
    lists: &'a [Vec<Rel<'q>>],
    /// The columns of the key of a partitioned table whose partitions are in the order of the key.
    partition: Option<&'a [i16]>,
    width: i32,
}

impl<'q> Ordered<'_, 'q> {
    /// `generate_orderedappend_paths`: for each order of a plan of a member, a `Merge Append` of the plans of the members in that order, one of the plans that cost the least to start and one of the plans that cost the least in total. A member with no plan in the order gets a sort of its cheapest plan. When the order is the order of the partitions, or the reverse of it, an `Append` of the partitions in that order gives the rows in order.
    fn plans(&self, stats: &Stats) -> Vec<Rel<'q>> {
        let mut all: Vec<&[OrderKey]> = Vec::new();
        for rel in self.lists.iter().flatten() {
            if !rel.order.is_empty() && !all.iter().any(|o| same_order(o, &rel.order)) {
                all.push(&rel.order);
            }
        }
        let mut out = Vec::new();
        for keys in all {
            // `pathkeys_count_contained_in`: a member whose plans have only the first keys needs an incremental sort, which the planner does not cost yet.
            let partial = self.lists.iter().flatten().any(|rel| {
                let n =
                    (1..=keys.len()).take_while(|&n| starts_with(&rel.order, &keys[..n])).count();
                n > 0 && n < keys.len()
            });
            if partial {
                continue;
            }
            // `get_cheapest_path_for_pathkeys`: the plan in the order that costs the least to start and the one that costs the least in total, or the cheapest plan with a sort.
            let mut startup = Vec::new();
            let mut total = Vec::new();
            for list in self.lists {
                let sorted = || (0..list.len()).filter(|&i| starts_with(&list[i].order, keys));
                let cost = |i: usize| list[i].node.cost;
                let first = sorted().min_by(|&a, &b| {
                    let (a, b) = (cost(a), cost(b));
                    a.startup.total_cmp(&b.startup).then(a.total.total_cmp(&b.total))
                });
                let cheapest = sorted().min_by(|&a, &b| {
                    let (a, b) = (cost(a), cost(b));
                    a.total.total_cmp(&b.total).then(a.startup.total_cmp(&b.startup))
                });
                startup.push(first.unwrap_or(0));
                total.push(cheapest.unwrap_or(0));
            }
            let mut choices = vec![startup.clone()];
            if total != startup {
                choices.push(total);
            }
            for choice in choices {
                let node = self.plan(keys, &choice);
                let rels = BTreeSet::from([self.r]);
                let order = stats.useful(&rels, keys);
                out.push(Rel { node, rels, order });
            }
        }
        out
    }

    /// `match_partition_order`: true when the keys are the first keys of the partitions, each in the direction `desc` with the nulls last for an ascending order, or all the keys of the partitions and more.
    fn partition_order(&self, keys: &[OrderKey], desc: bool) -> bool {
        let Some(attnums) = self.partition else { return false };
        let var = |e: &Expr, attnum: i16| matches!(e.kind, ExprKind::Var(v) if v.relation == self.r && v.levels_up == 0 && v.attnum == attnum);
        keys.iter().zip(attnums).all(|(k, &a)| {
            k.desc == desc && k.nulls_first == desc && k.exprs.iter().any(|e| var(e, a))
        })
    }

    /// `create_merge_append_path`, or `create_append_path` with the order of the partitions: the plans `choice` of the members, each with a sort when it is not in the order `keys`.
    fn plan(&self, keys: &[OrderKey], choice: &[usize]) -> Node<'q> {
        let r = self.r;
        let own =
            |e: &&Expr| matches!(e.kind, ExprKind::Var(v) if v.relation == r && v.levels_up == 0);
        let sort_keys: Vec<SortKey<'q>> = keys
            .iter()
            .filter_map(|k| {
                let expr = k.exprs.iter().find(own).or_else(|| k.exprs.first())?;
                Some(SortKey {
                    item: Item::owned(expr.clone()),
                    desc: k.desc,
                    using: None,
                    nulls_first: k.nulls_first,
                    collation: 0,
                })
            })
            .collect();
        let bound = super::append::limit_tuples(self.level.query, r);
        let mut children: Vec<Node<'q>> = self
            .lists
            .iter()
            .zip(choice)
            .map(|(list, &at)| {
                let rel = &list[at];
                if starts_with(&rel.order, keys) {
                    return rel.node.clone();
                }
                let mut node = super::sort(rel.node.clone(), sort_keys.clone(), bound);
                node.cost.width = rel.node.cost.width;
                node
            })
            .collect();
        let scope = self.level.scope;
        if self.partition_order(keys, false) {
            return append(scope, children, true, self.width);
        }
        if self.partition_order(keys, true) {
            children.reverse();
            return append(scope, children, true, self.width);
        }
        merge_append(scope, children, sort_keys, self.width)
    }
}

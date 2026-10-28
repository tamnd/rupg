//! `remove_useless_self_joins` of `analyzejoins.c`: an inner join of a table with itself, where the conditions of the join make each row of one side match the same row of the other side or no row, gives the rows of one scan of the table. One of the two relations leaves the query, and the columns of the other take the place of its columns.

use std::collections::BTreeSet;

use rupg_analyze::{
    Expr, ExprKind, FromItem, JoinKind, PermInfo, Query, Relation, RowMark, SubLink,
};
use rupg_pgcatalog::builtin;
use rupg_types::oid;

use crate::{Members, exprs, from, pushdown};

/// `remove_useless_self_joins`: the self-joins of the query and of the queries in it that the relations can leave, as `remove_self_joins_one_group` finds them. The removed relation stays in the list of relations, and no item of `FROM` reads it. A removal can make another removal possible, so the search starts again after each removal, as `query_planner` does it.
pub fn remove_self_joins(query: &mut Query, members: &dyn Members) {
    if !members.enable().self_join_elimination {
        return;
    }
    let perms = query.perms.clone();
    level(query, &perms, members);
}

/// The removals of [`remove_self_joins`] in the query and in the queries in it. `perms` are the permissions of the statement, which the relations of all the queries point at.
fn level(query: &mut Query, perms: &[PermInfo], members: &dyn Members) {
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            level(sub, perms, members);
        }
    }
    for cte in &mut query.ctes {
        level(&mut cte.query, perms, members);
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                level(&mut link.query, perms, members);
            }
            false
        });
    }
    // The result relation of a write cannot go, and the other relations of a write keep their scans for `EvalPlanQual`. The conditions of the policies and of the security barrier views keep their order of evaluation with their relations.
    if query.write.is_some()
        || !query.security.is_empty()
        || !query.barrier.is_empty()
        || query.minmax.is_some()
    {
        return;
    }
    while let Some(done) = search(query, perms, members) {
        *query = done;
    }
}

/// `remove_self_joins_recurse`: the query with one self-join removed, or `None` when no pair of relations can go. The relations of each list of the join tree of `deconstruct_jointree` make groups of the same table and the same user of the permission checks, and each pair of a group is a candidate, the removed relation first. The semi joins that `reduce_unique_semijoins` makes inner joins are inner joins in the query that this gives.
fn search(query: &Query, perms: &[PermInfo], members: &dyn Members) -> Option<Query> {
    let mut reduced = query.clone();
    for item in &mut reduced.from {
        reduce_semijoins(item, query, members);
    }
    let mut lists = vec![Vec::new()];
    for item in &reduced.from {
        join_list(item, 0, &mut lists);
    }
    for list in lists {
        let mut candidates: Vec<(u32, u32, usize)> = list
            .into_iter()
            .filter(|&r| candidate(&reduced, r, members))
            .map(|r| {
                let relation = &reduced.relations[r];
                let user = relation.perm.and_then(|p| perms.get(p)).map_or(0, |p| p.check_as);
                (relation.oid, user, r)
            })
            .collect();
        candidates.sort_unstable();
        for group in candidates.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
            for (at, &(_, _, r)) in group.iter().enumerate() {
                for &(_, _, k) in &group[at + 1..] {
                    if let Some(done) = try_remove(&reduced, r, k, members) {
                        return Some(done);
                    }
                }
            }
        }
    }
    None
}

/// The lists of relations of the join tree, as `deconstruct_jointree` makes its joinlist: a full join keeps each side as a list of its own.
fn join_list(item: &FromItem, list: usize, lists: &mut Vec<Vec<usize>>) {
    match item {
        FromItem::Relation(r) => lists[list].push(*r),
        FromItem::Join(join) if join.kind == JoinKind::Full => {
            for side in [&join.left, &join.right] {
                lists.push(Vec::new());
                let own = lists.len() - 1;
                join_list(side, own, lists);
            }
        }
        FromItem::Join(join) => {
            join_list(&join.left, list, lists);
            join_list(&join.right, list, lists);
        }
    }
}

/// True for a relation that is a scan of one ordinary table with no `TABLESAMPLE`, the only kind of relation that a self-join removal considers.
fn candidate(query: &Query, r: usize, members: &dyn Members) -> bool {
    let Some(relation) = query.relations.get(r) else { return false };
    plain(relation, members) && members.members(query, r, &[]).is_none()
}

/// True for a relation of an ordinary table, `RTE_RELATION` with `RELKIND_RELATION`.
fn plain(relation: &Relation, members: &dyn Members) -> bool {
    relation.oid != 0
        && relation.subquery.is_none()
        && relation.function.is_none()
        && relation.values.is_none()
        && relation.cte.is_none()
        && relation.tablesample.is_none()
        && members.is_table(relation.oid)
}

/// `reduce_unique_semijoins`: a semi join whose inner side is one table that gives one row at most for each outer row, by a unique index whose key columns the `=` conditions of the join fix, is an inner join.
fn reduce_semijoins(item: &mut FromItem, query: &Query, members: &dyn Members) {
    let FromItem::Join(join) = item else { return };
    reduce_semijoins(&mut join.left, query, members);
    reduce_semijoins(&mut join.right, query, members);
    if join.kind != JoinKind::Semi {
        return;
    }
    let FromItem::Relation(x) = join.right else { return };
    if !candidate(query, x, members) {
        return;
    }
    let mut outer = Vec::new();
    pushdown::rels(&join.left, &mut outer);
    let outer: BTreeSet<usize> = outer.into_iter().collect();
    let mut parts = Vec::new();
    if let Some(on) = &join.on {
        exprs::and_parts(on, &mut parts);
    }
    let mut conds: Vec<(&Expr, Vec<u32>)> = Vec::new();
    for part in parts {
        let Some((op, left, right)) = exprs::binary(part) else { continue };
        if !exprs::can_merge(op) || exprs::volatile(part) {
            continue;
        }
        let families = from::families(op);
        for (inner, other) in [(left, right), (right, left)] {
            if exprs::relations(inner) == BTreeSet::from([x])
                && exprs::relations(other).is_subset(&outer)
            {
                conds.push((inner, families.clone()));
            }
        }
    }
    let unique = members
        .indexes(query.relations[x].oid)
        .iter()
        .any(|index| unique_by(index, x, &conds).is_some());
    if unique {
        join.kind = JoinKind::Inner;
    }
}

/// `relation_has_unique_index_for`: the places in `conds` of the conditions that fix the key columns of the index of the relation `k`, when the index is unique, immediate and has no predicate, and each of its key columns has a condition. Each condition is the expression of `k` that it compares, with the btree families of its operator. The first condition of a column in the list is its condition.
fn unique_by(index: &crate::Index, k: usize, conds: &[(&Expr, Vec<u32>)]) -> Option<Vec<usize>> {
    if !index.unique || !index.immediate || index.partial {
        return None;
    }
    let mut used = Vec::new();
    let mut next_expr = index.exprs.iter();
    for (c, &family) in index.families.iter().enumerate() {
        let key = *index.keys.get(c)?;
        let key_expr = if key == 0 {
            let mut e = next_expr.next()?.clone();
            renumber(&mut e, 0, k);
            Some(e)
        } else {
            None
        };
        let at = conds.iter().position(|(expr, families)| {
            families.contains(&family)
                && match &key_expr {
                    Some(e) => strip(expr).same(strip(e)),
                    None => from::plain_var(expr)
                        .is_some_and(|v| v.levels_up == 0 && v.relation == k && v.attnum == key),
                }
        })?;
        used.push(at);
    }
    Some(used)
}

/// The expression under its `RelabelType`.
fn strip(expr: &Expr) -> &Expr {
    match &expr.kind {
        ExprKind::Relabel(arg, _) => arg,
        _ => expr,
    }
}

/// The row mark of a relation, `None` for a relation with no mark of its own: a relation of a query with locks that no locking clause names has `ROW_MARK_REFERENCE`.
fn mark(query: &Query, r: usize) -> Option<&RowMark> {
    query.row_marks.iter().find(|m| m.relation == r)
}

/// The parts of the conditions of the query in the order of `deconstruct_jointree`, each with its join domain, and the join domain of each relation. The nullable side of an outer join is a domain of its own, and a part of the condition of an outer join that reads only the nullable side belongs to that domain.
#[derive(Default)]
struct Domains<'q> {
    parts: Vec<(usize, &'q Expr)>,
    of: Vec<usize>,
    count: usize,
}

impl<'q> Domains<'q> {
    fn of_query(query: &'q Query) -> Domains<'q> {
        let mut out =
            Domains { of: vec![usize::MAX; query.relations.len()], count: 1, ..Default::default() };
        for item in &query.from {
            out.walk(item, 0);
        }
        if let Some(filter) = &query.filter {
            out.push(filter, 0, None);
        }
        out
    }

    fn walk(&mut self, item: &'q FromItem, domain: usize) {
        let join = match item {
            FromItem::Relation(r) => {
                if let Some(d) = self.of.get_mut(*r) {
                    *d = domain;
                }
                return;
            }
            FromItem::Join(join) => join,
        };
        let mut fresh = || {
            self.count += 1;
            self.count - 1
        };
        let (left, right) = match join.kind {
            JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => (domain, domain),
            JoinKind::Left | JoinKind::Anti => (domain, fresh()),
            JoinKind::Right | JoinKind::RightAnti => (fresh(), domain),
            JoinKind::Full => (fresh(), fresh()),
        };
        self.walk(&join.left, left);
        self.walk(&join.right, right);
        let Some(on) = &join.on else { return };
        match join.kind {
            JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => self.push(on, domain, None),
            JoinKind::Left | JoinKind::Anti => self.push(on, right, Some(&join.right)),
            JoinKind::Right | JoinKind::RightAnti => self.push(on, left, Some(&join.left)),
            JoinKind::Full => {}
        }
    }

    /// The parts of the condition, in the domain. With `nullable`, only the parts that read no relation outside the item.
    fn push(&mut self, cond: &'q Expr, domain: usize, nullable: Option<&FromItem>) {
        let mut parts = Vec::new();
        exprs::and_parts(cond, &mut parts);
        let inside: Option<BTreeSet<usize>> = nullable.map(|item| {
            let mut rels = Vec::new();
            pushdown::rels(item, &mut rels);
            rels.into_iter().collect()
        });
        for part in parts {
            if inside.as_ref().is_none_or(|rels| exprs::relations(part).is_subset(rels)) {
                self.parts.push((domain, part));
            }
        }
    }
}

/// An `EquivalenceClass` of a join domain: its members and the btree families of its operators.
struct Class<'q> {
    members: Vec<&'q Expr>,
    families: Vec<u32>,
}

/// The equivalence classes of the `=` conditions of a domain, as `process_equivalence` merges them.
fn classes<'q>(parts: &[&'q Expr]) -> Vec<Class<'q>> {
    let mut out: Vec<Class<'q>> = Vec::new();
    for &part in parts {
        let Some((op, left, right)) = exprs::binary(part) else { continue };
        if !exprs::can_merge(op) || exprs::volatile(part) {
            continue;
        }
        let families = from::families(op);
        if families.is_empty() || left.same(right) {
            continue;
        }
        let has =
            |c: &Class<'_>, e: &Expr| c.families == families && c.members.iter().any(|m| m.same(e));
        let l = out.iter().position(|c| has(c, left));
        let r = out.iter().position(|c| has(c, right));
        match (l, r) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                let merged = out.remove(b);
                let a = if b < a { a - 1 } else { a };
                out[a].members.extend(merged.members);
            }
            (Some(a), None) => out[a].members.push(right),
            (None, Some(b)) => out[b].members.push(left),
            (None, None) => out.push(Class { members: vec![left, right], families }),
        }
    }
    out
}

impl<'q> Class<'q> {
    /// `generate_base_implied_equalities_const`: the member that the conditions of the class with a constant compare with, the first constant or else the first member that reads no relation.
    fn constant(&self) -> Option<&'q Expr> {
        let mut consts = self.members.iter().copied().filter(|m| exprs::relations(m).is_empty());
        let first = consts.next()?;
        Some(
            std::iter::once(first)
                .chain(consts)
                .find(|m| matches!(m.kind, ExprKind::Const(_)))
                .unwrap_or(first),
        )
    }
}

/// `remove_self_joins_one_group` for the relations `r` and `k` of one table: the query with `r` removed, as `remove_self_join_rel` removes it, or `None` when the join of the two relations cannot go.
fn try_remove(query: &Query, r: usize, k: usize, members: &dyn Members) -> Option<Query> {
    // The two relations are on the same side of each outer join.
    if !same_sides(&query.from, r, k) {
        return None;
    }
    if mark(query, r).map(|m| m.strength) != mark(query, k).map(|m| m.strength) {
        return None;
    }
    let domains = Domains::of_query(query);
    let domain = *domains.of.get(r)?;
    if domains.of.get(k) != Some(&domain) {
        return None;
    }
    let parts: Vec<&Expr> =
        domains.parts.iter().filter(|(d, _)| *d == domain).map(|(_, p)| *p).collect();
    let classes = classes(&parts);
    let only = |e: &Expr, x: usize| exprs::relations(e) == BTreeSet::from([x]);
    // `generate_join_implied_equalities` for the join of `r` and `k`, split into the conditions of a column with the same column of the other side and the others, as `split_selfjoin_quals` splits them.
    let mut self_quals: Vec<(&Expr, Vec<u32>)> = Vec::new();
    let mut others = 0;
    for class in classes.iter().filter(|c| c.constant().is_none()) {
        let outers: Vec<&Expr> = class.members.iter().copied().filter(|m| only(m, r)).collect();
        let inners: Vec<&Expr> = class.members.iter().copied().filter(|m| only(m, k)).collect();
        let both = class
            .members
            .iter()
            .filter(|m| {
                let rels = exprs::relations(m);
                rels.contains(&r) && rels.contains(&k) && rels.len() == 2
            })
            .count();
        if both > 0 && outers.len() + inners.len() + both > 1 {
            others += 1;
        }
        let mut best: Option<(usize, &Expr, &Expr, u32)> = None;
        'outer: for &o in &outers {
            for &i in &inners {
                let Some(expr) = from::equality(&class.families, o, i) else { continue };
                let Some((op, _, _)) = exprs::binary(&expr) else { continue };
                let column = |e: &Expr| usize::from(from::plain_var(e).is_some());
                let score = column(o) + column(i) + usize::from(exprs::can_hash(op));
                if best.is_none_or(|b| score > b.0) {
                    best = Some((score, o, i, op));
                    if score == 3 {
                        break 'outer;
                    }
                }
            }
        }
        let Some((_, o, i, op)) = best else { continue };
        let mut renamed = strip(i).clone();
        renumber(&mut renamed, k, r);
        if strip(o).same(&renamed) {
            self_quals.push((i, from::families(op)));
        } else {
            others += 1;
        }
    }
    if self_quals.is_empty() && others == 0 {
        return None;
    }
    // `baserestrictinfo`: the conditions `member = constant` of the classes with a constant.
    let restrictions = |x: usize| -> Vec<(&Expr, &Expr, Vec<u32>)> {
        let mut out = Vec::new();
        for class in &classes {
            let Some(constant) = class.constant() else { continue };
            for &m in &class.members {
                if only(m, x) {
                    out.push((m, constant, class.families.clone()));
                }
            }
        }
        out
    };
    let (kept, removed) = (restrictions(k), restrictions(r));
    let mut conds = self_quals.clone();
    conds.extend(kept.iter().map(|(m, _, f)| (*m, f.clone())));
    let used = members
        .indexes(query.relations[k].oid)
        .iter()
        .find_map(|index| unique_by(index, k, &conds))?;
    // `match_unique_clauses`: each condition with a constant that the uniqueness took has the same condition on `r`.
    for at in used.into_iter().filter(|&at| at >= self_quals.len()) {
        let (m, constant, _) = &kept[at - self_quals.len()];
        let mut m = (*m).clone();
        renumber(&mut m, k, r);
        if !removed.iter().any(|(o, c, _)| o.same(&m) && c.same(constant)) {
            return None;
        }
    }
    remove(query, r, k)
}

/// True when the relations `r` and `k` are on the same side of each join of the items that is not an inner join.
fn same_sides(items: &[FromItem], r: usize, k: usize) -> bool {
    fn walk(item: &FromItem, r: usize, k: usize) -> bool {
        let FromItem::Join(join) = item else { return true };
        if join.kind != JoinKind::Inner {
            for side in [&join.left, &join.right] {
                let mut rels = Vec::new();
                pushdown::rels(side, &mut rels);
                if rels.contains(&r) != rels.contains(&k) {
                    return false;
                }
            }
        }
        walk(&join.left, r, k) && walk(&join.right, r, k)
    }
    items.iter().all(|item| walk(item, r, k))
}

/// `remove_self_join_rel`: the query with the relation `r` out of the join tree, `k` in the place of `r` in each expression, and the conditions fixed by `fixup_selfjoin_jointree`.
fn remove(query: &Query, r: usize, k: usize) -> Option<Query> {
    let mut query = query.clone();
    let mut lifted = Vec::new();
    let mut at = None;
    for (place, item) in query.from.iter_mut().enumerate() {
        if detach(item, r, &mut lifted)? {
            at = Some(place);
        }
    }
    if let Some(place) = at {
        query.from.remove(place);
    }
    if !lifted.is_empty() {
        if let Some(filter) = query.filter.take() {
            pushdown::owned_parts(filter, &mut lifted);
        }
        query.filter = pushdown::and_all(lifted);
    }
    // `ChangeVarNodes`.
    query.each_expr_mut(0, &mut |expr, depth| rename(expr, depth, r, k));
    fn merged(item: &mut FromItem, r: usize, k: usize) {
        let FromItem::Join(join) = item else { return };
        for e in &mut join.merged {
            rename(e, 0, r, k);
        }
        merged(&mut join.left, r, k);
        merged(&mut join.right, r, k);
    }
    for item in &mut query.from {
        merged(item, r, k);
    }
    if let Some(at) = query.row_marks.iter().position(|m| m.relation == r) {
        if query.row_marks.iter().any(|m| m.relation == k) {
            query.row_marks.remove(at);
        } else {
            query.row_marks[at].relation = k;
        }
    }
    let mut hoist = Vec::new();
    let mut found = false;
    for item in &mut query.from {
        found |= fixup(item, k, &mut hoist)?;
    }
    if !found {
        return None;
    }
    if let Some(filter) = query.filter.take() {
        pushdown::owned_parts(filter, &mut hoist);
    }
    query.filter = pushdown::and_all(fixup_quals(hoist, k));
    Some(query)
}

/// `remove_rel_from_jointree`: the item without the relation `r`. True when the item is the relation, which its parent drops. A join of the relation with another item becomes that item with the conditions of the join, as a `FromExpr` of the item and the conditions: an inner join takes them in its condition, and another item gives them to the join above it in `lifted`. The join above takes them in its condition, or gives them on from the outer side of an outer join. `None` when a join that is not an inner join loses a side, or the conditions would leave a full join.
fn detach(item: &mut FromItem, r: usize, lifted: &mut Vec<Expr>) -> Option<bool> {
    let join = match item {
        FromItem::Relation(x) => return Some(*x == r),
        FromItem::Join(join) => join,
    };
    let mut from_left = Vec::new();
    let mut from_right = Vec::new();
    let left_gone = detach(&mut join.left, r, &mut from_left)?;
    let right_gone = detach(&mut join.right, r, &mut from_right)?;
    if left_gone || right_gone {
        if join.kind != JoinKind::Inner {
            return None;
        }
        let mut quals = from_left;
        quals.append(&mut from_right);
        if let Some(on) = join.on.take() {
            pushdown::owned_parts(on, &mut quals);
        }
        let surviving = if left_gone { &mut join.right } else { &mut join.left };
        let mut surviving = std::mem::replace(surviving, FromItem::Relation(r));
        match &mut surviving {
            FromItem::Join(inner) if inner.kind == JoinKind::Inner => {
                let mut parts = Vec::new();
                if let Some(on) = inner.on.take() {
                    pushdown::owned_parts(on, &mut parts);
                }
                parts.append(&mut quals);
                inner.on = pushdown::and_all(parts);
            }
            _ => lifted.extend(quals),
        }
        *item = surviving;
        return Some(false);
    }
    // The conditions of a side go to the condition of the join, or above it from the outer side of an outer join.
    let (into_on, up): (Vec<Expr>, Vec<Expr>) = match join.kind {
        JoinKind::Inner => (from_left.into_iter().chain(from_right).collect(), Vec::new()),
        JoinKind::Left | JoinKind::Anti | JoinKind::Semi => (from_right, from_left),
        JoinKind::Right | JoinKind::RightAnti | JoinKind::RightSemi => (from_left, from_right),
        JoinKind::Full if from_left.is_empty() && from_right.is_empty() => (Vec::new(), Vec::new()),
        JoinKind::Full => return None,
    };
    if !into_on.is_empty() {
        let mut parts = into_on;
        if let Some(on) = join.on.take() {
            pushdown::owned_parts(on, &mut parts);
        }
        join.on = pushdown::and_all(parts);
    }
    lifted.extend(up);
    Some(false)
}

/// `ChangeVarNodes`: the columns of the relation `r` of the query `depth` levels up become the columns of `k`, also in the `PlaceHolderVar`s and in the alternatives of the subqueries.
fn rename(expr: &mut Expr, depth: usize, r: usize, k: usize) {
    expr.each_mut(depth, &mut |e, depth| {
        match &mut e.kind {
            ExprKind::Var(var) if var.levels_up == depth && var.relation == r => var.relation = k,
            ExprKind::PlaceHolder(p) if p.levels_up == depth && p.relations.contains(&r) => {
                for rel in &mut p.relations {
                    if *rel == r {
                        *rel = k;
                    }
                }
                let mut seen = BTreeSet::new();
                p.relations.retain(|rel| seen.insert(*rel));
            }
            ExprKind::SubLink(link) => {
                if let Some(alternative) = &mut link.alternative {
                    rename_link(alternative, depth, r, k);
                }
            }
            _ => {}
        }
        false
    });
}

/// [`rename`] for a subquery.
fn rename_link(link: &mut SubLink, depth: usize, r: usize, k: usize) {
    if let Some(test) = &mut link.test {
        rename(test, depth, r, k);
    }
    link.query.each_expr_mut(depth + 1, &mut |e, depth| rename(e, depth, r, k));
    if let Some(alternative) = &mut link.alternative {
        rename_link(alternative, depth, r, k);
    }
}

/// The relation `from` of an expression on one relation becomes `to`, at the top level of the expression.
fn renumber(expr: &mut Expr, from: usize, to: usize) {
    rename(expr, 0, from, to);
}

/// True when the condition reads the relation `k`, as `pull_varnos` finds it.
fn mentions(expr: &Expr, k: usize) -> bool {
    exprs::relations(expr).contains(&k)
}

/// `fixup_selfjoin_jointree`: a condition that reads `k` in a join that does not have `k` goes up to the first join that has it, before the conditions of that join, and each list of conditions gets the changes of [`fixup_quals`]. True when the item has `k`. `None` when a condition would go up from a join that is not an inner join.
fn fixup(item: &mut FromItem, k: usize, hoist: &mut Vec<Expr>) -> Option<bool> {
    let join = match item {
        FromItem::Relation(x) => return Some(*x == k),
        FromItem::Join(join) => join,
    };
    let mut sub = Vec::new();
    let found_left = fixup(&mut join.left, k, &mut sub)?;
    let found_right = fixup(&mut join.right, k, &mut sub)?;
    let found = found_left || found_right;
    let mut parts = Vec::new();
    if let Some(on) = join.on.take() {
        pushdown::owned_parts(on, &mut parts);
    }
    if found {
        sub.append(&mut parts);
        parts = sub;
    } else {
        let (up, keep): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|p| mentions(p, k));
        parts = keep;
        sub.extend(up);
        if !sub.is_empty() && join.kind != JoinKind::Inner {
            return None;
        }
        sub.append(hoist);
        *hoist = sub;
    }
    join.on = pushdown::and_all(fixup_quals(parts, k));
    Some(found)
}

/// `fixup_selfjoin_quals`: each part that reads `k` and is `X = X` with a strict mergejoinable operator becomes `X IS NOT NULL`, and a part that reads `k` and is the same as a part before it goes.
fn fixup_quals(parts: Vec<Expr>, k: usize) -> Vec<Expr> {
    let mut out: Vec<Expr> = Vec::new();
    for part in parts {
        let part = if mentions(&part, k) {
            let part = replace_selfjoin_qual(part);
            if out.iter().any(|o| o.same(&part)) {
                continue;
            }
            part
        } else {
            part
        };
        out.push(part);
    }
    out
}

/// `replace_selfjoin_qual`: `X IS NOT NULL` for `X = X`.
fn replace_selfjoin_qual(part: Expr) -> Expr {
    let Some((op, left, right)) = exprs::binary(&part) else { return part };
    let ExprKind::Func(f) = &part.kind else { return part };
    if !left.same(right)
        || !builtin::proc_by_oid(f.oid).is_some_and(|p| p.strict)
        || !exprs::can_merge(op)
        || exprs::volatile(left)
    {
        return part;
    }
    let arg = left.clone();
    Expr::new(ExprKind::NullTest(Box::new(arg), false), oid::BOOL)
}

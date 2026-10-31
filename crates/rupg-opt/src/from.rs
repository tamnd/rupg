//! The plan of `FROM` and `WHERE`: the scan of each relation, the joins and the place of each condition, as `query_planner`, `make_one_rel` and `create_scan_plan` make them.

pub(crate) mod append;
mod classes;
mod index;
pub(crate) use index::implied_by;
mod inherit;
mod memoize;
pub(crate) mod paths;
mod predtest;
mod scansel;
mod search;
mod tid;

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use rupg_analyze::{
    BoolOp, BoolTest, Expr, ExprKind, FromItem, JoinKind, Query, Relation, SetKind, SetOp, SetTree,
    SortGroup, SubLinkKind, Var, WriteKind, expand_grouping_sets,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{
    Cost, Item, Kind, Node, Replaced, STATISTIC_KIND_HISTOGRAM, STATISTIC_KIND_MCV, SortKey, Split,
    Statistic, Strategy,
};
use rupg_types::{Value, oid};

use crate::{Builder, Index, Level, Size, cost, exprs, parallel, pullup, setop, upper};
pub(crate) use classes::{equality, families};
use index::Param;
use memoize::Memo;
use paths::{Paths, costs};

/// The plans of the rows of `FROM` that `add_path` keeps, each with the order of its rows.
pub(crate) type Plans<'q> = Vec<(Node<'q>, Vec<OrderKey>)>;

/// The columns and the expressions of the relations of a query level that have a unique index of one key column and no predicate, which `examine_variable` marks as `isunique`.
#[derive(Clone, Debug, Default)]
struct Unique {
    /// The relation and the column number of each column, as `has_unique_index`.
    columns: BTreeSet<(usize, i16)>,
    /// Each expression of the key column of an index, with the columns of its relation.
    exprs: Vec<Expr>,
}

impl Unique {
    /// True when the expression is a unique column, or an expression of a unique index with or without a `RelabelType` above it, as `examine_variable` finds it.
    fn has(&self, expr: &Expr) -> bool {
        if let Some(var) = plain_var(expr) {
            return var.levels_up == 0 && self.columns.contains(&(var.relation, var.attnum));
        }
        let base = match &expr.kind {
            ExprKind::Relabel(arg, _) => &**arg,
            _ => expr,
        };
        self.exprs.iter().any(|e| e.same(base))
    }
}

/// The key columns of a unique index with the operator family of each.
type KeyFamilies = (Vec<i16>, Vec<u32>);

/// The estimates of the relations of a query level for the costs of the joins and of the nodes above them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    /// The rows of each relation before and after its conditions, as `tuples` and `rows` of its `RelOptInfo`.
    rows: BTreeMap<usize, (f64, f64)>,
    /// Each column that the query reads, with its width and the relations of the places that read it, as `attr_needed`. The target list of the query is `usize::MAX`.
    needed: Vec<(Var, i32, BTreeSet<usize>)>,
    /// Each `PlaceHolderVar` that the query reads, with its width, the relations of the node that computes it and the relations of the places that read it, as `ph_eval_at` and `ph_needed`.
    placeholders: Vec<(Expr, i32, BTreeSet<usize>, BTreeSet<usize>)>,
    /// `sort_pathkeys`: the keys of `ORDER BY`, each with its directions.
    sort: Vec<(Expr, bool, bool)>,
    /// `window_pathkeys`: the keys of the sort of the first window, which the bottom `WindowAgg` reads.
    window: Vec<(Expr, bool, bool)>,
    /// `group_pathkeys` and `distinct_pathkeys`: the keys of `GROUP BY` and of `DISTINCT`, in any order.
    groups: Vec<Vec<(Expr, bool, bool)>>,
    /// `query_pathkeys`: the keys of `GROUP BY`, or else of the first window, or else of `DISTINCT`, or else of `ORDER BY`.
    query: Vec<(Expr, bool, bool)>,
    /// The two sides of each `=` condition of a join that a merge join can test, each with its relations.
    joins: Vec<[(Expr, BTreeSet<usize>); 2]>,
    /// `consider_startup`: true when the caller of the query or its `LIMIT` reads only a part of its rows, so that a plan with a lower start cost is a different plan.
    startup: bool,
    /// `isunique`: the columns and the expressions that have a unique index of one key column and no predicate.
    unique: Unique,
    /// The key columns of each unique index of each relation that is immediate and has no predicate, which `relation_has_unique_index_for` reads.
    distinct: BTreeMap<usize, Vec<Vec<i16>>>,
    /// The key columns of each unique index of each relation that shows that the other columns of a row are a function of the key columns, as `remove_useless_groupby_columns` chooses them: the index is immediate and has no predicate and no expression, and each key column is `NOT NULL` or the index has `NULLS NOT DISTINCT`. Each index has the operator family of each key column.
    functional: BTreeMap<usize, Vec<KeyFamilies>>,
    /// The members of each equivalence class of the query level, each class with true when it has a constant.
    equal: Vec<(Vec<Expr>, bool)>,
    /// The columns of the members of each equivalence class whose conditions the joins make, each with the relations of the class. `generate_base_implied_equalities_no_const` adds the relations to the `attr_needed` of the columns.
    spans: Vec<(Expr, BTreeSet<usize>)>,
    /// The costs of the subqueries of the expressions of the level that run as a `SubPlan` for each row.
    subplans: Vec<cost::SubCost>,
    /// The relations of the join that tests the conditions that run once, which is the join of all the relations, or none when there is no such condition.
    gated: BTreeSet<usize>,
    /// True when each relation of the level is a target of the statement or has a row mark, as `preprocess_rowmarks` gives a mark to each relation of a query with a lock and of an `UPDATE`, a `DELETE` or a `MERGE`. Its scans test again each condition that the predicate of a partial index implies, as `check_index_predicates` keeps them for a `is_target_rel`.
    pub(super) marked: bool,
    /// True when `FROM` of the level is one item that is not a join, so that each condition of `WHERE` on the relation is a condition of its scan.
    pub(super) alone: bool,
    /// The statistics of the columns of each table of the level, by column number, from `pg_statistic` with `stainherit` false.
    columns: BTreeMap<usize, Rc<BTreeMap<i16, Statistic>>>,
    /// The statistics of the expression columns of the indexes of each table of the level, as [`index_statistics`] reads them.
    exprs: BTreeMap<usize, Rc<Vec<(Expr, Statistic)>>>,
    /// `get_actual_variable_range`: the least and the greatest value of the columns of the relations that a `btree` index reads in order, which the conditions of the scans compare with a constant.
    ranges: BTreeMap<(usize, i16), Option<(Value, Value)>>,
    /// `pages` of each table of the level, which the estimate of an inequality of `ctid` reads.
    pages: BTreeMap<usize, f64>,
    /// `allvisfrac` of each table of the level, which the cost of an index only scan reads.
    all_visible: BTreeMap<usize, f64>,
    /// The estimates of the clauses of the joins that the join search finds again for each pair of sides.
    cache: RefCell<Cache>,
}

/// The estimates of the clauses of the joins of a level, as a `RestrictInfo` keeps `norm_selec` and `scansel_cache`. The join search reads them for each pair of sides and each kept path, so it computes each of them once.
#[derive(Clone, Debug, Default)]
struct Cache {
    /// `eqjoinsel_inner` of the two values of an `=`.
    eqjoinsel: Vec<(Expr, Expr, f64)>,
    /// `mergejoinscansel` of the two values of an `=` with the directions of the key.
    scansel: Vec<(Expr, Expr, bool, bool, ScanSels)>,
}

/// The `mergejoinscansel` results of the two sides of a merge key.
type ScanSels = (scansel::ScanSel, scansel::ScanSel);

impl Stats {
    /// `eqjoinsel_inner` of `left = right`, which the cache keeps.
    fn eqjoinsel(&self, left: &Expr, right: &Expr) -> f64 {
        let found = self.cache.borrow().eqjoinsel.iter().find_map(|(l, r, selectivity)| {
            (l.same(left) && r.same(right)).then_some(*selectivity)
        });
        if let Some(selectivity) = found {
            return selectivity;
        }
        let selectivity = eqjoinsel_inner(&JoinVar::new(left, self), &JoinVar::new(right, self));
        self.cache.borrow_mut().eqjoinsel.push((left.clone(), right.clone(), selectivity));
        selectivity
    }

    /// `mergejoinscansel` of `left = right` in the order `desc` with the nulls first when `nulls_first`, which the cache keeps.
    fn scansel(
        &self,
        left: &Expr,
        right: &Expr,
        desc: bool,
        nulls_first: bool,
    ) -> (scansel::ScanSel, scansel::ScanSel) {
        let found = self.cache.borrow().scansel.iter().find_map(|(l, r, d, n, found)| {
            (*d == desc && *n == nulls_first && l.same(left) && r.same(right)).then_some(*found)
        });
        if let Some(found) = found {
            return found;
        }
        let found = scansel::mergejoinscansel(self, left, right, desc, nulls_first);
        let key = (left.clone(), right.clone(), desc, nulls_first, found);
        self.cache.borrow_mut().scansel.push(key);
        found
    }

    /// The estimates of the conditions of the relation `r` with `tuples` tuples.
    fn estimate(&self, r: usize, tuples: f64) -> ScanEstimate {
        let columns = self.columns.get(&r).cloned().unwrap_or_default();
        let ranges = self.ranges.range((r, i16::MIN)..=(r, i16::MAX));
        let ranges = ranges.map(|(&(_, attnum), range)| (attnum, range.clone())).collect();
        let pages = self.pages.get(&r).copied();
        let exprs = self.exprs.get(&r).cloned().unwrap_or_default();
        ScanEstimate { r, tuples, pages, unique: self.unique.clone(), columns, exprs, ranges }
    }

    /// The columns of the members of the equivalence classes that a node of the join of `rels` gives: each column of the relations whose class has a relation that is not in `rels`.
    pub(crate) fn class_columns<'a>(
        &'a self,
        rels: &'a BTreeSet<usize>,
    ) -> impl Iterator<Item = &'a Expr> {
        self.spans
            .iter()
            .filter(|(column, span)| {
                matches!(column.kind, ExprKind::Var(v) if rels.contains(&v.relation))
                    && !span.is_subset(rels)
            })
            .map(|(column, _)| column)
    }

    /// `cost_qual_eval` of the conditions with the costs of the subplans of the level: the start cost and the cost for each row.
    pub(crate) fn qual_costs<'a>(&self, quals: impl IntoIterator<Item = &'a Expr>) -> (f64, f64) {
        cost::qual_costs(quals, &self.subplans)
    }

    /// `set_rel_width`: the start cost and the cost for each row of the `PlaceHolderVar`s that the scan of the relation `r` computes for the nodes above it.
    fn placeholder_costs(&self, r: usize) -> (f64, f64) {
        let computed = self.placeholders.iter().filter(|(_, _, eval, places)| {
            eval.len() == 1 && eval.contains(&r) && !places.is_subset(eval)
        });
        self.qual_costs(computed.filter_map(|(e, ..)| match &e.kind {
            ExprKind::PlaceHolder(p) => Some(&p.expr),
            _ => None,
        }))
    }

    /// The cost for each row of [`Stats::qual_costs`].
    pub(crate) fn qual_cost<'a>(&self, quals: impl IntoIterator<Item = &'a Expr>) -> f64 {
        self.qual_costs(quals).1
    }

    /// `order_qual_clauses`: the conditions in the order of their security levels, and in the order of their cost for each row in each level, and in their order for the same cost. A leakproof condition that costs less than 10 times `cpu_operator_cost` has the level 0.
    fn sort_quals(&self, quals: &mut [Qual<'_>]) {
        // The other conditions of a scan with policies come after the policies, and the conditions of a scan with no policies have the level 0.
        let top = if quals.iter().any(|q| q.security.is_some()) { usize::MAX } else { 0 };
        let key = |qual: &Qual<'_>| {
            let cost = self.qual_cost([&*qual.expr]);
            let level = qual.security.unwrap_or(top);
            let cheap = cost < 10.0 * cost::CPU_OPERATOR && !leaky(&qual.expr);
            (if cheap { 0 } else { level }, cost)
        };
        quals.sort_by(|a, b| {
            let ((a_level, a_cost), (b_level, b_cost)) = (key(a), key(b));
            a_level.cmp(&b_level).then(a_cost.total_cmp(&b_cost))
        });
    }

    /// `estimate_num_groups` with no statistics: the number of groups of rows by the values of the expressions. A `boolean` expression has 2 values, an expression of one relation with the statistics of an index or a unique index counts as one column, and each other expression has the values of its columns. Two columns that are equal or that an equivalence class makes equal count once. The columns of each relation have the product of their numbers of distinct values, at most a tenth of the tuples of the relation, or the largest number of one column when it is more. A relation with fewer rows after its conditions than its tuples has fewer values, as the formula of Yao. A column of a relation that the level does not know has 200 values.
    pub(crate) fn groups(&self, exprs: &[&Expr], input_rows: f64) -> f64 {
        self.groups_estimated(exprs, input_rows).0
    }

    /// [`Stats::groups`], and true when the number of distinct values of a column is the default, as `SELFLAG_USED_DEFAULT`.
    pub(crate) fn groups_estimated(&self, exprs: &[&Expr], input_rows: f64) -> (f64, bool) {
        let input_rows = cost::clamp_rows(input_rows);
        if exprs.is_empty() {
            return (1.0, false);
        }
        let mut numdistinct = 1.0;
        let mut used_default = false;
        // Each column with its relation, its number of distinct values and true when the number is the default, as a `GroupVarInfo`.
        let mut infos: Vec<(Expr, usize, f64, bool)> = Vec::new();
        for &expr in exprs {
            if expr.ty == oid::BOOL {
                numdistinct *= 2.0;
                continue;
            }
            // `examine_variable` finds the statistics of an index expression, or a unique index on it, and the expression counts as one column.
            if plain_var(expr).is_none()
                && exprs::relations(expr).len() == 1
                && (column_statistic(expr, self).is_some() || self.unique.has(expr))
            {
                self.add_group_var(&mut infos, expr.clone());
                continue;
            }
            let mut columns = Vec::new();
            exprs::vars_of(expr, &mut columns);
            if columns.is_empty() && exprs::volatile(expr) {
                return (input_rows, used_default);
            }
            for column in columns {
                self.add_group_var(&mut infos, column);
            }
        }
        while let Some((_, rel, ..)) = infos.first() {
            let rel = *rel;
            let (mut reldistinct, mut relmax) = (1.0f64, 1.0f64);
            let mut count = 0;
            for (_, _, nd, default) in infos.iter().filter(|i| i.1 == rel) {
                reldistinct *= nd;
                relmax = relmax.max(*nd);
                count += 1;
                used_default |= default;
            }
            infos.retain(|i| i.1 != rel);
            match self.rows.get(&rel) {
                Some(&(tuples, rows)) if tuples > 0.0 => {
                    let mut clamp = tuples;
                    if count > 1 {
                        clamp *= 0.1;
                        if clamp < relmax {
                            clamp = relmax.min(tuples);
                        }
                    }
                    reldistinct = reldistinct.min(clamp);
                    if reldistinct > 0.0 && rows < tuples {
                        reldistinct *= 1.0 - ((tuples - rows) / tuples).powf(tuples / reldistinct);
                    }
                    numdistinct *= cost::clamp_rows(reldistinct);
                }
                Some(_) => {}
                None => numdistinct *= reldistinct,
            }
        }
        (numdistinct.ceil().min(input_rows).max(1.0), used_default)
    }

    /// `add_unique_group_var`: adds a column, or an expression of one relation, to the columns of the groups, when it is not there. Of two columns of different relations that an equivalence class makes equal, the column with fewer distinct values stays.
    fn add_group_var(&self, infos: &mut Vec<(Expr, usize, f64, bool)>, column: Expr) {
        let relation = match &column.kind {
            ExprKind::Var(var) => var.relation,
            _ => match exprs::relations(&column).first() {
                Some(&r) => r,
                None => return,
            },
        };
        let (nd, default, _) = numdistinct(&column, self);
        let mut i = 0;
        while i < infos.len() {
            let (other, rel, other_nd, _) = &infos[i];
            if other.same(&column) {
                return;
            }
            if *rel != relation && self.known_equal(other, &column) {
                if *other_nd <= nd {
                    return;
                }
                infos.remove(i);
                continue;
            }
            i += 1;
        }
        infos.push((column, relation, nd, default));
    }

    /// `exprs_known_equal`: true when an equivalence class has the two expressions.
    fn known_equal(&self, a: &Expr, b: &Expr) -> bool {
        self.equal.iter().any(|(c, _)| c.iter().any(|m| m.same(a)) && c.iter().any(|m| m.same(b)))
    }

    /// `pathkey_is_redundant`: true when a key of a sort on the expression adds nothing after the keys on the expressions `before`. The expression is a constant, an equivalence class with a constant has it, or an earlier key is the expression or is in its class. A volatile expression has a class of its own.
    pub(crate) fn redundant(&self, expr: &Expr, before: &[&Expr]) -> bool {
        if exprs::volatile(expr) {
            return false;
        }
        if exprs::pseudo_constant(expr) {
            return true;
        }
        // `canonicalize_ec_expression` gives a key and a member of a class the same `RelabelType`, so the comparison ignores it.
        let same = |a: &Expr, b: &Expr| upper::strip_relabel(a).same(upper::strip_relabel(b));
        match self.equal.iter().find(|(members, _)| members.iter().any(|m| same(m, expr))) {
            Some((members, constant)) => {
                *constant || before.iter().any(|b| members.iter().any(|m| same(m, b)))
            }
            None => before.iter().any(|b| same(b, expr)),
        }
    }

    /// True when the expression is in an equivalence class of the query, as `make_pathkey_from_sortinfo` finds one with no `create_it`: a member of a class of `=`, an item of `ORDER BY`, of a window, of `GROUP BY` or of `DISTINCT`, or a side of a condition of a merge join.
    fn has_class(&self, expr: &Expr) -> bool {
        let same = |a: &Expr| upper::strip_relabel(a).same(upper::strip_relabel(expr));
        self.equal.iter().any(|(members, _)| members.iter().any(same))
            || self.sort.iter().chain(&self.window).any(|(e, _, _)| same(e))
            || self.groups.iter().flatten().any(|(e, _, _)| same(e))
            || self.joins.iter().flatten().any(|(e, _)| same(e))
    }

    /// `make_canonical_pathkey`: the key of an order by the expression, with the other members of its equivalence class.
    pub(crate) fn order_key(&self, expr: &Expr, desc: bool, nulls_first: bool) -> OrderKey {
        let mut exprs = vec![expr.clone()];
        if let Some((members, _)) = self.equal.iter().find(|(m, _)| m.iter().any(|e| e.same(expr)))
        {
            exprs.extend(members.iter().filter(|m| !m.same(expr)).cloned());
        }
        OrderKey { exprs, desc, nulls_first }
    }

    /// `make_pathkeys_for_sortclauses_extended` with `remove_redundant`: the items of a list of keys without the items whose keys are redundant, as `pathkey_is_redundant` finds them.
    pub(crate) fn canonical<T>(&self, items: Vec<T>, expr: impl Fn(&T) -> &Expr) -> Vec<T> {
        self.canonical_keeping(items, expr, |_| false)
    }

    /// [`Stats::canonical`] that keeps each item for which `keep` is true, as an item of grouping sets that a set does not have is not a member of an equivalence class.
    pub(crate) fn canonical_keeping<T>(
        &self,
        items: Vec<T>,
        expr: impl Fn(&T) -> &Expr,
        keep: impl Fn(&T) -> bool,
    ) -> Vec<T> {
        let mut out: Vec<T> = Vec::new();
        for item in items {
            let before: Vec<&Expr> = out.iter().map(&expr).collect();
            if keep(&item) || !self.redundant(expr(&item), &before) {
                out.push(item);
            }
        }
        out
    }

    /// `remove_useless_groupby_columns`: for each expression of `GROUP BY`, with the function of its `=` operator, true when the groups need it not, because the other columns of its relation in `GROUP BY` have all the key columns of a unique index of `functional`. The index proves that a key is unique only with the equality of its family, so an item of each key column must have an `=` operator of the family of the column, as `get_mergejoin_opfamilies` gives the families of the operator. The key columns must be fewer than the columns of the relation in `GROUP BY`, and the index with the fewest key columns wins. Only a column of the query level can go, and `GROUP BY` must have two items or more.
    pub(crate) fn surplus(&self, items: &[(&Expr, u32)]) -> Vec<bool> {
        let exprs: Vec<&Expr> = items.iter().map(|(e, _)| *e).collect();
        let column = |e: &Expr| match e.kind {
            ExprKind::Var(var) if var.levels_up == 0 => Some(var),
            _ => None,
        };
        let mut out = vec![false; exprs.len()];
        if exprs.len() < 2 {
            return out;
        }
        let mut columns: BTreeMap<usize, BTreeSet<i16>> = BTreeMap::new();
        for var in exprs.iter().filter_map(|e| column(e)) {
            columns.entry(var.relation).or_default().insert(var.attnum);
        }
        for (r, attnums) in &columns {
            let Some(indexes) = self.functional.get(r) else { continue };
            // An `=` function that no built-in operator has is taken as an operator of each family.
            let in_family = |attnum: i16, family: u32| {
                items.iter().any(|(e, equal)| {
                    column(e).is_some_and(|v| v.relation == *r && v.attnum == attnum) && {
                        let eqops = builtin::operators().iter().filter(|o| o.code == *equal);
                        let mut eqops = eqops.peekable();
                        eqops.peek().is_none() || eqops.any(|o| families(o.oid).contains(&family))
                    }
                })
            };
            let best = indexes
                .iter()
                .filter(|(keys, families)| {
                    keys.iter().zip(families).all(|(&k, &f)| in_family(k, f)) && {
                        let keys: BTreeSet<i16> = keys.iter().copied().collect();
                        keys.is_subset(attnums) && keys.len() < attnums.len()
                    }
                })
                .map(|(keys, _)| keys)
                .min_by_key(|keys| keys.len());
            let Some(best) = best else { continue };
            for (i, e) in exprs.iter().enumerate() {
                if let Some(var) = column(e)
                    && var.relation == *r
                    && !best.contains(&var.attnum)
                {
                    out[i] = true;
                }
            }
        }
        out
    }

    /// Adds the columns of an expression that a place with these relations reads.
    fn need(&mut self, expr: &Expr, depth: usize, at: &BTreeSet<usize>, relations: &[Relation]) {
        let mut columns = Vec::new();
        exprs::columns_at(expr, depth, &mut columns);
        for column in columns {
            if let ExprKind::PlaceHolder(p) = &column.kind {
                match self.placeholders.iter_mut().find(|(e, ..)| e.same(&column)) {
                    Some((.., places)) => places.extend(at.iter().copied()),
                    None => {
                        let (width, eval) =
                            (cost::expr_width(&column, relations), exprs::eval_at(p));
                        self.placeholders.push((column, width, eval, at.clone()));
                    }
                }
                continue;
            }
            let ExprKind::Var(var) = column.kind else { continue };
            match self.needed.iter_mut().find(|(v, _, _)| *v == var) {
                Some((_, _, places)) => places.extend(at.iter().copied()),
                None => {
                    let width = cost::expr_width(&column, relations);
                    self.needed.push((var, width, at.clone()));
                }
            }
        }
    }

    /// `get_expr_width`: the width of a column that the query reads, or else the width of the type of the expression.
    fn expr_width(&self, expr: &Expr) -> i32 {
        match &expr.kind {
            ExprKind::Var(var) => match self.needed.iter().find(|(v, ..)| v == var) {
                Some((_, width, _)) => *width,
                None => cost::type_width(expr.ty, expr.typmod),
            },
            _ => cost::type_width(expr.ty, expr.typmod),
        }
    }

    /// The columns of the relation `r` that a place outside the relation reads, as [`Stats::width`] counts them.
    fn read_columns(&self, r: usize) -> Vec<i16> {
        let rels = BTreeSet::from([r]);
        let read =
            self.needed.iter().filter(|(v, _, places)| v.relation == r && !places.is_subset(&rels));
        read.map(|(v, ..)| v.attnum).collect()
    }

    /// The width of the `PlaceHolderVar`s that the relation `r` computes and a place outside the relation reads, as [`Stats::width`] counts them.
    fn placeholder_width(&self, r: usize) -> i32 {
        let rels = BTreeSet::from([r]);
        let computed = self
            .placeholders
            .iter()
            .filter(|(_, _, eval, places)| eval.is_subset(&rels) && !places.is_subset(&rels));
        computed.map(|(_, width, ..)| width).sum()
    }

    /// Sets the width of the column `attnum` of the relation `r`, as `set_append_rel_size` sets `attr_widths` of a parent.
    fn set_width(&mut self, r: usize, attnum: i16, width: i32) {
        for (v, w, _) in &mut self.needed {
            if v.relation == r && v.attnum == attnum {
                *w = width;
            }
        }
    }

    /// The width of the rows of a join of relations, as `build_joinrel_tlist` and `set_rel_width` find it: the columns of the relations that a place outside the join reads.
    fn width(&self, rels: &BTreeSet<usize>) -> i32 {
        let columns: i32 = self
            .needed
            .iter()
            .filter(|(v, _, places)| rels.contains(&v.relation) && !places.is_subset(rels))
            .map(|(_, width, _)| width)
            .sum();
        let placeholders: i32 = self
            .placeholders
            .iter()
            .filter(|(_, _, eval, places)| eval.is_subset(rels) && !places.is_subset(rels))
            .map(|(_, width, ..)| width)
            .sum();
        columns + placeholders
    }

    /// `truncate_useless_pathkeys`: the first keys of the order of the rows of a join of relations that a node above can use. These are the first keys of `ORDER BY` or of the first window, the first keys that are keys of `GROUP BY` or of `DISTINCT`, or the first keys that a merge join with another relation can use.
    fn useful(&self, rels: &BTreeSet<usize>, order: &[OrderKey]) -> Vec<OrderKey> {
        let has = |list: &[(Expr, bool, bool)], k: &OrderKey| {
            list.iter().any(|(e, desc, nulls_first)| k.sorts(e, *desc, *nulls_first))
        };
        let leading = |list: &[(Expr, bool, bool)]| {
            order
                .iter()
                .zip(list)
                .take_while(|(k, (e, desc, nulls_first))| k.sorts(e, *desc, *nulls_first))
                .count()
        };
        let ordered = leading(&self.sort).max(leading(&self.window));
        let grouped =
            self.groups.iter().map(|g| order.iter().take_while(|k| has(g, k)).count()).max();
        // `pathkeys_useful_for_merging`: a merge join sorts its sides in ascending order.
        let merging = order
            .iter()
            .take_while(|k| {
                !k.desc
                    && !k.nulls_first
                    && self.joins.iter().any(|sides| {
                        sides.iter().zip(sides.iter().rev()).any(|((e, _), (_, other))| {
                            k.exprs.iter().any(|x| x.same(e)) && !other.is_subset(rels)
                        })
                    })
            })
            .count();
        let n = ordered.max(grouped.unwrap_or(0)).max(merging);
        order[..n].to_vec()
    }
}

/// A condition of `WHERE` or of `ON` with the relations that it reads, as a `RestrictInfo`.
#[derive(Clone, Debug)]
struct Qual<'q> {
    expr: Cow<'q, Expr>,
    rels: BTreeSet<usize>,
    /// True for a condition of `ON` of an outer join that goes to the nullable side, which is not an equivalence of PostgreSQL.
    outer: bool,
    /// The place of the condition in the order of `deconstruct_jointree`, or `None` for a condition that the planner makes from an equivalence class.
    order: Option<usize>,
    /// The selectivities of the conditions that `extract_restriction_or_clauses` made from the parts of this `OR` for one relation each. A join that tests the condition with `JOIN_INNER` divides the selectivity of the condition by each of them, as `consider_new_or_clause` changes `norm_selec`.
    divisors: Vec<f64>,
    /// `parent_ec`: the place of the equivalence class that made the condition of a join, or `None`.
    class: Option<usize>,
    /// `security_level` of a condition of a policy of row-level security, or `None` for a condition of the query, which has a higher level.
    security: Option<usize>,
}

impl<'q> Qual<'q> {
    /// A condition of the query.
    fn of(expr: &'q Expr, rels: BTreeSet<usize>, outer: bool) -> Qual<'q> {
        let divisors = Vec::new();
        Qual {
            expr: Cow::Borrowed(expr),
            rels,
            outer,
            order: None,
            divisors,
            class: None,
            security: None,
        }
    }

    /// A condition that the planner makes from an equivalence class.
    fn implied(expr: Expr, rels: BTreeSet<usize>) -> Qual<'q> {
        let divisors = Vec::new();
        Qual {
            expr: Cow::Owned(expr),
            rels,
            outer: false,
            order: None,
            divisors,
            class: None,
            security: None,
        }
    }

    /// The condition as an expression that the node computes.
    fn item(&self) -> Item<'q> {
        Item { expr: self.expr.clone(), below: false }
    }
}

/// The tree of the joins of `FROM`, as `deconstruct_jointree` sees it.
#[derive(Debug)]
enum Tree<'q> {
    Base { relation: usize, quals: Vec<Qual<'q>> },
    Join(Box<JoinTree<'q>>),
}

#[derive(Debug)]
struct JoinTree<'q> {
    kind: JoinKind,
    left: Tree<'q>,
    right: Tree<'q>,
    /// The parts of the condition of `ON`, before the planner gives each a place.
    on: Vec<&'q Expr>,
    /// The conditions of the join.
    quals: Vec<Qual<'q>>,
    /// The conditions above an outer join, which the join tests on its rows with the nulls.
    filter: Vec<Qual<'q>>,
    rels: BTreeSet<usize>,
    /// The relations that the condition of `ON` reads, as `pull_varnos` of the condition in `make_outerjoininfo`.
    clause: BTreeSet<usize>,
    /// For a semi join, the values of the inner side that make its rows unique, as `compute_semijoin_info` finds them.
    semi: Option<Semi<'q>>,
}

/// A left join, a semi join or an anti join of a join domain, as a `SpecialJoinInfo`. The inner side is one item of the domain, and the order of the joins of the domain can join the other items in any order that `join_is_legal` accepts.
#[derive(Debug)]
struct Special<'q> {
    kind: JoinKind,
    /// `min_lefthand`: the relations of the outer side that the condition reads, or each relation of the outer side when it reads none.
    left: BTreeSet<usize>,
    /// `min_righthand` and `syn_righthand`: the relations of the inner side.
    right: BTreeSet<usize>,
    /// `syn_lefthand` and `syn_righthand`: the relations of the two sides of the join in the query.
    syn: BTreeSet<usize>,
    /// The conditions of `ON` of a left join or an anti join, which only the join tests. The conditions of a semi join are conditions of the domain.
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    semi: Option<Semi<'q>>,
}

/// The values of the inner side of a semi join that its condition compares with the outer side, each with its operator with the outer value on the left, as `semi_rhs_exprs` and `semi_operators`. A sort makes the rows unique on them when each operator can test a merge join, as `semi_can_btree`, and a hash table does when each operator can test a hash join, as `semi_can_hash`.
#[derive(Debug)]
struct Semi<'q> {
    values: Vec<(&'q Expr, u32)>,
    sort: bool,
    hash: bool,
}

impl Tree<'_> {
    fn rels(&self) -> BTreeSet<usize> {
        match self {
            Tree::Base { relation, .. } => BTreeSet::from([*relation]),
            Tree::Join(join) => join.rels.clone(),
        }
    }
}

/// A key of the order of the rows of a node: the expressions that have the same value at the key, as a `PathKey` of an equivalence class.
#[derive(Clone, Debug)]
pub(crate) struct OrderKey {
    pub(crate) exprs: Vec<Expr>,
    pub(crate) desc: bool,
    pub(crate) nulls_first: bool,
}

impl OrderKey {
    /// True when the key orders the rows by the expression in this direction.
    pub(crate) fn sorts(&self, expr: &Expr, desc: bool, nulls_first: bool) -> bool {
        self.desc == desc
            && self.nulls_first == nulls_first
            && self.exprs.iter().any(|e| e.same(expr))
    }
}

/// The plan of a part of `FROM`, with the relations that it reads and the order of its rows.
#[derive(Clone, Debug)]
struct Rel<'q> {
    node: Node<'q>,
    rels: BTreeSet<usize>,
    order: Vec<OrderKey>,
}

impl<'q> Builder<'q> {
    /// `query_planner`: the plan of the rows of `FROM` that pass `WHERE`, with the order of the rows, the conditions of joins that the equivalence classes took the place of and the estimates of the relations.
    pub(crate) fn from(&mut self, level: &Level<'q>) -> (Plans<'q>, Vec<&'q Expr>, Stats) {
        let query = level.query;
        let mut parts = Vec::new();
        if let Some(filter) = &query.filter {
            exprs::and_parts(filter, &mut parts);
        }
        parts.extend(upper::having_split(query).0);
        let trees = query.from.iter().map(tree);
        let Some(mut tree) =
            trees.reduce(|left, right| join_tree(JoinKind::Inner, left, right, Vec::new()))
        else {
            return (vec![(result(level.scope, parts), Vec::new())], Vec::new(), Stats::default());
        };
        let all = tree.rels();
        let mut gating = Vec::new();
        let mut quals = Vec::new();
        for part in parts {
            let rels = exprs::relations(part);
            let volatile = exprs::volatile(part);
            if rels.is_empty() && !volatile {
                if exprs::is_false(part) {
                    return (
                        vec![(dummy(level.scope, &all, false), Vec::new())],
                        Vec::new(),
                        Stats::default(),
                    );
                }
                if !exprs::is_true(part) {
                    gating.push(Item::of(part));
                }
                continue;
            }
            // A volatile condition with no column runs once for each row of the join of all the relations, as `distribute_qual_to_rels`.
            let rels = if rels.is_empty() { all.clone() } else { rels };
            quals.push(Qual::of(part, rels, false));
        }
        let mut nonnullable = BTreeSet::new();
        for qual in &quals {
            nonnullable.extend(nonnullable_rels(&qual.expr, true));
        }
        reduce(&mut tree, &nonnullable);
        // `distribute_qual_to_rels`: a part of `ON` of the top join domain with no column is a condition of the join of all the relations, as a part of `WHERE` is. It comes before the parts of `WHERE`.
        let mut constants = Vec::new();
        top_constants(&mut tree, &mut constants);
        if constants.iter().any(|part| exprs::is_false(part)) {
            return (
                vec![(dummy(level.scope, &all, false), Vec::new())],
                Vec::new(),
                Stats::default(),
            );
        }
        let constants = constants.into_iter().filter(|part| !exprs::is_true(part)).map(Item::of);
        gating.splice(0..0, constants);
        let mut next = 0;
        place_below_sublinks(&mut tree, &mut next);
        for mut qual in quals {
            qual.order = Some(next);
            next += 1;
            place(&mut tree, qual);
        }
        place_on(&mut tree, &mut next);
        if !query.security.is_empty() {
            mark_security(&mut tree, &query.security);
        }
        classes::not_null(&mut tree, !query.security.is_empty(), &query.relations);
        let mut found = classes::Found::default();
        if !classes::domain(&mut tree, true, &mut found) {
            return (
                vec![(dummy(level.scope, &all, false), Vec::new())],
                Vec::new(),
                Stats::default(),
            );
        }
        extract_or_clauses(&mut tree);
        let equal = found
            .equal
            .iter()
            .map(|(c, constant)| (c.iter().map(|&m| m.clone()).collect(), *constant))
            .collect();
        let gated = if gating.is_empty() { BTreeSet::new() } else { all.clone() };
        let marked = !level.query.row_marks.is_empty()
            || level.write.is_some_and(|w| w.kind != WriteKind::Insert);
        let alone = matches!(&level.query.from[..], [item] if !matches!(item, FromItem::Join(_)));
        let mut stats = Stats {
            equal,
            subplans: level.subplans.clone(),
            gated,
            marked,
            alone,
            ..Stats::default()
        };
        let member =
            self.append.as_ref().filter(|a| self.scopes[level.scope].parent == Some(a.scope));
        let keys = member.map(|a| a.keys.clone());
        needed(level, &mut stats, keys.as_ref().map(append::ChildKeys::used));
        // A `SELECT` that is not a member of the append relation is a subquery, whose plan asks for no order.
        if let Some(keys) = &keys
            && pullup::member(level.query)
        {
            keys.apply(level, &mut stats);
        }
        // `standard_qp_callback`: a `SELECT` of a side of `INTERSECT` or `EXCEPT` with no other order to ask for asks for its rows in the order of its columns, as `setop_pathkeys`.
        if let Some(leaf) =
            self.setop.as_ref().filter(|s| self.scopes[level.scope].parent == Some(s.scope))
            && stats.query.is_empty()
            && level.query.windows.is_empty()
        {
            let keys = setop::keys(level.query, leaf.op, &stats);
            stats.query.clone_from(&keys);
            stats.sort = keys;
        }
        // `generate_base_implied_equalities_no_const`: each join of the relations of a class gives the members of the class, because a join above can test any two of them.
        for class in &found.classes {
            let span = class.members().iter().map(|&(_, r)| r).collect();
            for &(member, _) in class.members() {
                stats.need(member, 0, &span, &level.query.relations);
                let mut columns = Vec::new();
                exprs::columns_at(member, 0, &mut columns);
                stats.spans.extend(columns.into_iter().map(|c| (c, span.clone())));
            }
        }
        // `cost_qual_eval` of a condition that is `pseudoconstant`: it runs once, when the node of all the relations starts.
        let (startup, per_row) = stats.qual_costs(gating.iter().map(|i| &*i.expr));
        let once = startup + per_row;
        // The scan of the only relation counts the conditions with its own.
        let (rels, counted) = match tree {
            Tree::Base { relation, quals } => {
                let rels = self.scan(level, relation, quals, once, &mut stats);
                if let Some(partial) = self.partial.take()
                    && gating.is_empty()
                {
                    self.partials.insert(level.scope, partial);
                }
                (rels, true)
            }
            tree => (self.build(level, tree, &found, &mut stats), false),
        };
        let plans = rels
            .into_iter()
            .map(|rel| {
                let mut node = rel.node;
                if !gating.is_empty() {
                    // The node counts the conditions in its start cost, before it adds its run cost.
                    if !counted {
                        let run = node.cost.total - node.cost.startup;
                        node.cost.startup += once;
                        node.cost.total = node.cost.startup + run;
                    }
                    // `create_gating_plan`: the `Result` has the costs of the node below.
                    let cost = node.cost;
                    node = Node::new(
                        Kind::Result { replaces: None, one_time: gating.clone() },
                        level.scope,
                        vec![node],
                    );
                    node.cost = cost;
                }
                (node, rel.order)
            })
            .collect();
        (plans, found.joined, stats)
    }

    /// The plans of a part of the tree of the joins that `add_path` keeps, with the plan with the lowest total cost first. The inner joins of a join domain are one problem of the order of the joins, with the outer joins as items of the problem. `found` has the equivalence classes.
    fn build(
        &mut self,
        level: &Level<'q>,
        tree: Tree<'q>,
        found: &classes::Found<'q>,
        stats: &mut Stats,
    ) -> Vec<Rel<'q>> {
        match tree {
            Tree::Base { relation, quals } => self.scan(level, relation, quals, 0.0, stats),
            Tree::Join(join)
                if matches!(
                    join.kind,
                    JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti
                ) =>
            {
                let (mut items, mut pool, mut specials) = (Vec::new(), Vec::new(), Vec::new());
                flatten(Tree::Join(join), &mut items, &mut pool, &mut specials);
                pool.sort_by_key(|q| q.order.unwrap_or(usize::MAX));
                let mut rels: Vec<Vec<Rel<'q>>> =
                    items.into_iter().map(|t| self.build(level, t, found, stats)).collect();
                if let Some(rel) = dummy_domain(level, &mut rels, &pool, &specials, stats) {
                    return vec![rel];
                }
                let bases: Vec<Option<&Base<'q>>> = rels
                    .iter()
                    .map(|plans| match plans[0].rels.iter().collect::<Vec<_>>()[..] {
                        [&r] => self.bases.get(&(level.scope, r)),
                        _ => None,
                    })
                    .collect();
                search::search(level, rels, &pool, specials, found, &bases, stats)
            }
            Tree::Join(join) => {
                let JoinTree { kind, left, right, quals, filter, .. } = *join;
                let left = self.build(level, left, found, stats);
                let right = self.build(level, right, found, stats);
                join_rels(level, kind, left, right, quals, filter, stats)
            }
        }
    }

    /// The scans of a relation with its conditions that `add_path` keeps, as `set_rel_pathlist` and `create_scan_plan` make them, with the costs of `cost_seqscan` and the other functions of `costsize.c` for each kind of scan. A table has a scan by each of its indexes too. `once` is the cost of the conditions that read no relation, which the sequential scan of the only relation of `FROM` has.
    fn scan(
        &mut self,
        level: &Level<'q>,
        r: usize,
        quals: Vec<Qual<'q>>,
        once: f64,
        stats: &mut Stats,
    ) -> Vec<Rel<'q>> {
        let relation = &level.query.relations[r];
        self.partial = None;
        let base = restrictions(quals);
        let mut quals = base.clone();
        stats.sort_quals(&mut quals);
        let mut indexes = Vec::new();
        let mut size = Size::default();
        // `cost_seqscan` and the other scans: the start cost of the conditions, and their cost for each row.
        let (qual_startup, qual_cost) = stats.qual_costs(quals.iter().map(|q| &*q.expr));
        let qual_startup = qual_startup + once;
        let mut per_row = cost::CPU_TUPLE + qual_cost;
        let mut children = Vec::new();
        // `cost_subqueryscan`: the cost of the subquery as it is, to which the scan adds its own cost.
        let mut sub_total = None;
        let mut appended = None;
        let mut sub_paths = None;
        let mut cte_order = None;
        let (kind, startup, run, tuples) = match &relation.cte {
            Some(cte) if cte.work.is_some() => {
                // `cost_ctescan`: one more `cpu_tuple_cost` for each row of the tuplestore.
                per_row += cost::CPU_TUPLE;
                let rows = self.work.iter().rev().find(|(w, _)| Some(*w) == cte.work);
                (Kind::WorkTableScan(r), 0.0, 0.0, rows.map_or(10.0, |(_, rows)| rows * 10.0))
            }
            Some(cte)
                if relation.subquery.is_none()
                    || cte.write.is_some()
                    || self.cte(&cte.name).is_some_and(|c| !c.inline) =>
            {
                per_row += cost::CPU_TUPLE;
                let rows = self.cte(&cte.name).map_or(1000.0, |c| c.rows);
                cte_order = self.cte(&cte.name).map(|c| c.order.clone());
                (Kind::CteScan(r), 0.0, 0.0, rows)
            }
            _ => match (&relation.subquery, &relation.function, &relation.values) {
                (Some(sub), _, _) => {
                    // The planner keeps the plans of the members of a `UNION ALL` for its `Merge Append`.
                    let keys = if quals.is_empty() {
                        append::ChildKeys::new(self, level, r, sub, stats)
                    } else {
                        None
                    };
                    let rel = keys.map(|keys| append::AppendRel {
                        scope: self.scopes.len(),
                        keys,
                        plans: Vec::new(),
                    });
                    self.paths = rel.is_none();
                    let outer = std::mem::replace(&mut self.append, rel);
                    let start = self.current.len();
                    let child = self.query(sub, Some(level.scope));
                    appended = std::mem::replace(&mut self.append, outer);
                    sub_paths = self.sub_paths.take();
                    // `set_subquery_pathlist`: a set operation whose result is empty makes the relation dummy, and its relations take no names. The other subqueries that are empty here are those that `pull_up_subqueries` merges into the query.
                    if sub.set_op.is_some() && setop::is_dummy(&child) {
                        self.current.truncate(start);
                        let rels = BTreeSet::from([r]);
                        return vec![Rel {
                            node: dummy(level.scope, &rels, false),
                            rels,
                            order: Vec::new(),
                        }];
                    }
                    let c = child.cost;
                    children.push(child);
                    // `cost_subqueryscan`: a scan with no condition that gives the columns of its subquery as they are costs its subquery, because `setrefs.c` takes it out of the plan.
                    // The `Subquery Scan` that casts the columns of a `SELECT` of a set operation stays.
                    if quals.is_empty()
                        && !self.wrapper(level.scope)
                        && (simple_union_all(sub) || trivial_target(level.query, r, sub))
                    {
                        per_row = 0.0;
                    }
                    let keys = distinct_keys(sub);
                    if !keys.is_empty() {
                        stats.distinct.entry(r).or_default().extend(keys);
                    }
                    // `examine_simple_variable`: the one item of `DISTINCT`, or with no `DISTINCT` the one item of `GROUP BY`, is unique.
                    let items = if sub.distinct.is_empty() { &sub.group } else { &sub.distinct };
                    if let [item] = items.as_slice()
                        && sub.set_op.is_none()
                        && sub.grouping_sets.is_empty()
                        && let Ok(at) = i16::try_from(item.target + 1)
                    {
                        stats.unique.columns.insert((r, at));
                    }
                    sub_total = Some(c.total);
                    (Kind::SubqueryScan(r), c.startup, c.total - c.startup, c.rows)
                }
                (None, Some(function), _) => {
                    let startup = cost::qual_cost(function.calls.iter().map(|(call, _)| call));
                    if relation.json_table().is_some() {
                        (Kind::TableFunctionScan(r), startup, 0.0, 100.0)
                    } else {
                        let rows = function.calls.iter().map(|(call, _)| function_rows(call));
                        (Kind::FunctionScan(r), startup, 0.0, rows.fold(1.0, f64::max))
                    }
                }
                (None, None, Some(values)) => {
                    per_row += cost::CPU_OPERATOR;
                    (Kind::ValuesScan(r), 0.0, 0.0, values.len() as f64)
                }
                (None, None, None) => {
                    let members = match &relation.tablesample {
                        Some(_) => None,
                        None => self.members.and_then(|m| {
                            let conds: Vec<&Expr> = quals.iter().map(|q| &*q.expr).collect();
                            m.members(level.query, r, &conds)
                        }),
                    };
                    if let Some(members) = members {
                        // `remove_useless_groupby_columns`: a partitioned table, which is not one of its members, has no row twice, so its unique indexes count, unlike those of a parent of `INHERITS`.
                        if !members.iter().any(|m| m.oid == relation.oid) {
                            let indexes = self.members.map(|m| m.indexes(relation.oid));
                            for index in indexes.unwrap_or_default() {
                                if functional(relation, &index) {
                                    let keys = (index.keys, index.families);
                                    stats.functional.entry(r).or_default().push(keys);
                                }
                            }
                        }
                        let both = (base.as_slice(), quals.as_slice());
                        let (rels, members) =
                            inherit::scan(level, r, both, members, self.members, stats);
                        if !members.is_empty() {
                            let (tuples, rows) = stats.rows.get(&r).copied().unwrap_or_default();
                            let indexes = Vec::new();
                            let stored =
                                Base { pages: 0.0, tuples, rows, quals: base, indexes, members };
                            self.bases.insert((level.scope, r), stored);
                        }
                        return rels;
                    }
                    indexes = self.members.map(|m| m.indexes(relation.oid)).unwrap_or_default();
                    size = self.members.map_or_else(Size::default, |m| m.size(relation.oid));
                    stats.columns.insert(r, crate::statistics(relation.oid));
                    let exprs = index_statistics(self.members, r, &indexes);
                    if !exprs.is_empty() {
                        stats.exprs.insert(r, Rc::new(exprs));
                    }
                    for q in &quals {
                        variable_ranges(self.members, &q.expr, (r, relation.oid), &indexes, stats);
                    }
                    // `mergejoinscansel` reads the histograms of the columns of the conditions of the joins.
                    let joined: Vec<i16> = stats
                        .joins
                        .iter()
                        .flatten()
                        .map(|(side, _)| side)
                        .chain(stats.equal.iter().flat_map(|(members, _)| members))
                        .filter_map(plain_var)
                        .filter(|v| v.levels_up == 0 && v.relation == r)
                        .map(|v| v.attnum)
                        .collect();
                    for attnum in joined {
                        column_range(self.members, attnum, (r, relation.oid), &indexes, stats);
                    }
                    for index in &indexes {
                        if let ([key], true, false) =
                            (index.keys.as_slice(), index.unique, index.partial)
                        {
                            if *key > 0 {
                                stats.unique.columns.insert((r, *key));
                            } else if let Some(expr) = index::key_expr(index, r, 0) {
                                stats.unique.exprs.push(expr);
                            }
                        }
                        if functional(relation, index) {
                            let keys = (index.keys.clone(), index.families.clone());
                            stats.functional.entry(r).or_default().push(keys);
                        }
                        if index.unique && index.immediate && !index.partial {
                            stats.distinct.entry(r).or_default().push(index.keys.clone());
                        }
                    }
                    let (pages, tuples) = cost::table_size(relation, size);
                    stats.pages.insert(r, pages);
                    stats.all_visible.insert(r, cost::all_visible(size, pages));
                    match &relation.tablesample {
                        // `set_tablesample_rel_size` and `cost_samplescan`: `SYSTEM` reads the fraction of the pages, each at the cost of a random page, and `BERNOULLI` reads each page in order. The fraction is a `real`, and a percent out of range is a tenth.
                        Some(sample) => {
                            let fraction = sample
                                .args
                                .first()
                                .and_then(number)
                                .map(|p| p as f32)
                                .filter(|p| (0.0..=100.0).contains(p))
                                .map_or(0.1f32, |p| p / 100.0);
                            let fraction = f64::from(fraction);
                            let tuples = cost::clamp_rows(tuples * fraction);
                            let system = builtin::proc_by_oid(sample.handler)
                                .is_some_and(|p| p.name == "system");
                            let run = if system {
                                cost::clamp_rows(pages * fraction) * cost::RANDOM_PAGE
                            } else {
                                pages * cost::SEQ_PAGE
                            };
                            (Kind::SampleScan(r), 0.0, run, tuples)
                        }
                        None => (Kind::SeqScan(r), 0.0, pages * cost::SEQ_PAGE, tuples),
                    }
                }
            },
        };
        let eq = stats.estimate(r, tuples);
        let selectivity = cost::list_selectivity_by(quals.iter().map(|q| &*q.expr), &eq);
        let rows = cost::clamp_rows(tuples * selectivity);
        stats.rows.insert(r, (tuples, rows));
        stats.cache.take();
        let mut node = Node::new(kind, level.scope, children);
        node.filter = quals.iter().map(|q| q.item()).collect();
        let width = stats.width(&BTreeSet::from([r]));
        let startup = startup + qual_startup;
        // `cost_seqscan` adds the cost of the pages after the cost of the rows, and the other scans add the two costs before the start cost, which the rounding of the sums follows.
        let total = match sub_total {
            Some(t) => t + qual_startup + tuples * per_row,
            None if matches!(node.kind, Kind::SeqScan(_)) => startup + tuples * per_row + run,
            None => startup + (run + tuples * per_row),
        };
        node.cost = Cost { startup, total, rows, width };
        // `cost_seqscan` and the other scans: the targets cost their start cost, and their cost for each row that the scan gives.
        let targets = stats.placeholder_costs(r);
        let add_targets = |node: &mut Node<'_>| {
            node.cost.startup += targets.0;
            node.cost.total += targets.0 + targets.1 * node.cost.rows;
        };
        add_targets(&mut node);
        node.disabled = matches!(node.kind, Kind::SeqScan(_)) && !crate::enable().seqscan;
        let mut seq = Rel { node, rels: BTreeSet::from([r]), order: Vec::new() };
        if let Some(append) = appended {
            return append.plans(level, r, seq, stats);
        }
        // `set_cte_pathlist`: the scan of a query of `WITH` has the order of the rows of its plan.
        if let Some(columns) = cte_order {
            seq.order = sub_order(relation, r, &columns, stats);
            return vec![seq];
        }
        // `set_subquery_pathlist`: a `Subquery Scan` of each plan of the subquery, with the order of its rows in the columns of the scan.
        if let Some(paths) = sub_paths {
            let order = |columns: &[(usize, bool, bool)]| sub_order(relation, r, columns, stats);
            seq.order = order(&paths.order);
            let mut kept = Paths::new(stats.startup);
            let others: Vec<Rel<'q>> = paths
                .others
                .iter()
                .filter_map(|(plan, columns)| {
                    let order = order(columns);
                    if order.is_empty() {
                        return None;
                    }
                    let mut node = seq.node.clone();
                    let c = plan.cost;
                    node.cost.startup = c.startup + qual_startup;
                    node.cost.total = c.total + qual_startup + c.rows * per_row;
                    node.children = vec![plan.clone()];
                    Some(Rel { node, rels: seq.rels.clone(), order })
                })
                .collect();
            kept.add(costs(&seq.node), seq.order.clone(), seq);
            for rel in others {
                kept.add(costs(&rel.node), rel.order.clone(), rel);
            }
            return kept.kept();
        }
        if !matches!(seq.node.kind, Kind::SeqScan(_)) {
            return vec![seq];
        }
        let (pages, _) = cost::table_size(relation, size);
        // `create_plain_partial_paths`: a parallel sequential scan, with the costs of `cost_seqscan` for the share of the rows of one process, and a `Gather` of its rows, as `generate_gather_paths`.
        let enable = crate::enable();
        let option = self.members.and_then(|m| m.parallel(relation.oid));
        let gathered = match option {
            Some(option) if self.parallel && base.iter().all(|q| parallel::safe(&q.expr)) => {
                let workers = parallel::workers(&enable, pages, option);
                (workers > 0).then(|| {
                    let divisor = parallel::divisor(&enable, workers);
                    let mut node = seq.node.clone();
                    node.parallel = true;
                    let cpu_run = (tuples * per_row + targets.1 * rows) / divisor;
                    node.cost.startup = startup + targets.0;
                    node.cost.total = node.cost.startup + cpu_run + run;
                    node.cost.rows = cost::clamp_rows(rows / divisor);
                    self.partial = Some((node.clone(), workers));
                    let node = parallel::gather(&enable, node, workers, rows);
                    Rel { node, rels: seq.rels.clone(), order: Vec::new() }
                })
            }
            _ => None,
        };
        let quals = base.clone();
        let outer = BTreeSet::new();
        let table = index::Table { r, pages, tuples, rows, quals, eq: &eq, outer, loops: 1.0 };
        let stored = Base {
            pages,
            tuples,
            rows,
            quals: base,
            indexes: indexes.clone(),
            members: Vec::new(),
        };
        self.bases.insert((level.scope, r), stored);
        // `set_plain_rel_pathlist`: the scans by `ctid` come first, and a scan of `CURRENT OF` is the only scan.
        let (mut tids, current) = tid::paths(level, &table, stats);
        for rel in &mut tids {
            add_targets(&mut rel.node);
        }
        if current {
            return tids;
        }
        if indexes.is_empty() && tids.is_empty() && gathered.is_none() {
            return vec![seq];
        }
        let mut paths = Paths::new(stats.startup);
        for rel in tids {
            paths.add(costs(&rel.node), Vec::new(), rel);
        }
        paths.add(costs(&seq.node), Vec::new(), seq);
        for mut rel in index::paths(level, &table, &indexes, stats) {
            add_targets(&mut rel.node);
            paths.add(costs(&rel.node), rel.order.clone(), rel);
        }
        if let Some(rel) = gathered {
            paths.add(costs(&rel.node), Vec::new(), rel);
        }
        paths.kept()
    }
}

/// `convert_subquery_pathkeys`: the order of the rows of a subquery or a query of `WITH`, as the places of its columns, in the columns of the relation `r`, with the first keys that a node above can use, as `truncate_useless_pathkeys`.
fn sub_order(
    relation: &Relation,
    r: usize,
    columns: &[(usize, bool, bool)],
    stats: &Stats,
) -> Vec<OrderKey> {
    let keys: Vec<OrderKey> = columns
        .iter()
        .map_while(|&(at, desc, nulls_first)| {
            let column = relation.columns.get(at)?;
            let var = Var { relation: r, attnum: i16::try_from(at + 1).ok()?, levels_up: 0 };
            let mut expr = Expr::new(ExprKind::Var(var), column.ty);
            expr.typmod = column.typmod;
            Some(OrderKey { exprs: vec![expr], desc, nulls_first })
        })
        .collect();
    stats.useful(&BTreeSet::from([r]), &keys)
}

/// `var_eq_const` and `var_eq_non_const`: `=` of a unique column or a unique expression of the table `r` of `tuples` rows and a value that reads no column of the table passes one row.
/// The estimates of the conditions of the relation `r` of `tuples` tuples, as `examine_variable` finds the columns of the relation: `isunique` for a unique column, and the statistics of `pg_statistic` for a column of a table and for an expression of an index of the table.
#[derive(Debug)]
pub(crate) struct ScanEstimate {
    r: usize,
    tuples: f64,
    /// `pages` of a table, or `None` for another relation.
    pages: Option<f64>,
    unique: Unique,
    columns: Rc<BTreeMap<i16, Statistic>>,
    exprs: Rc<Vec<(Expr, Statistic)>>,
    ranges: BTreeMap<i16, Option<(Value, Value)>>,
}

impl ScanEstimate {
    /// `get_restriction_variable`: of the two sides of a condition, the side that reads the relation alone and the side that does not read it.
    fn sides<'e>(&self, a: &'e Expr, b: &'e Expr) -> Option<(&'e Expr, &'e Expr)> {
        [(a, b), (b, a)].into_iter().find(|(column, value)| {
            exprs::relations(column) == BTreeSet::from([self.r])
                && !exprs::relations(value).contains(&self.r)
        })
    }

    /// `var_eq_const` for a constant `value`, and `var_eq_non_const` for another value. A unique column matches one row. With statistics a constant that is a most common value has its frequency, and another constant has a part of the other values. A value that is not a constant has the part of one distinct value, at most the frequency of the first most common value. `negate` gives `<>`, which is the rest of the rows that are not null.
    fn var_eq(&self, column: &Expr, value: &Expr, negate: bool) -> f64 {
        let statistic = cost::Estimate::statistic(self, column);
        let nullfrac = statistic.map_or(0.0, |s| f64::from(s.null_frac));
        let unique = self.unique.has(column);
        let mut constant = value;
        while let ExprKind::Relabel(arg, _) = &constant.kind {
            constant = arg;
        }
        let constant = match &constant.kind {
            ExprKind::Const(Value::Null) => return 0.0,
            ExprKind::Const(v) => Some(v),
            _ => None,
        };
        let numdistinct = || cost::numdistinct(column, statistic, unique, Some(self.tuples)).0;
        let selec = if unique && self.tuples >= 1.0 {
            1.0 / self.tuples
        } else if let Some(s) = statistic {
            let mcv = s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV);
            let numbers = mcv.and_then(|m| m.numbers.as_deref()).unwrap_or_default();
            let values = mcv.and_then(|m| m.values.as_deref()).unwrap_or_default();
            match constant {
                Some(c) => {
                    let equal = |v: &Value| {
                        crate::compare(column.ty, v, value.ty, c).is_some_and(Ordering::is_eq)
                    };
                    match values.iter().position(equal).and_then(|at| numbers.get(at)) {
                        Some(&freq) => f64::from(freq),
                        None => {
                            let sumcommon: f64 = numbers.iter().map(|&n| f64::from(n)).sum();
                            let mut selec: f64 = (1.0 - sumcommon - nullfrac).clamp(0.0, 1.0);
                            let other = numdistinct() - numbers.len() as f64;
                            if other > 1.0 {
                                selec /= other;
                            }
                            match numbers.last() {
                                Some(&least) if selec > f64::from(least) => f64::from(least),
                                _ => selec,
                            }
                        }
                    }
                }
                None => {
                    let mut selec = 1.0 - nullfrac;
                    let nd = numdistinct();
                    if nd > 1.0 {
                        selec /= nd;
                    }
                    match numbers.first() {
                        Some(&most) if selec > f64::from(most) => f64::from(most),
                        _ => selec,
                    }
                }
            }
        } else {
            1.0 / numdistinct()
        };
        let selec = if negate { 1.0 - selec - nullfrac } else { selec };
        selec.clamp(0.0, 1.0)
    }

    /// `scalarineqsel` for `column op value`: the most common values that pass the operator, and the part of the histogram of the other values that is below or above the constant `value`. With no histogram half of the other values pass.
    fn scalar_ineq(&self, column: &Expr, value: &Expr, op: cost::Inequality) -> Option<f64> {
        let mut value = value;
        while let ExprKind::Relabel(arg, _) = &value.kind {
            value = arg;
        }
        let ExprKind::Const(constant) = &value.kind else { return None };
        if constant.is_null() {
            return Some(0.0);
        }
        if let (Some(var), Value::Int8(tid)) = (plain_var(column), constant)
            && var.attnum == tid::CTID
            && column.ty == oid::TID
            && value.ty == oid::TID
        {
            return self.ctid_ineq(*tid, op);
        }
        let s = cost::Estimate::statistic(self, column)?;
        let test = |v: &Value| crate::compare(column.ty, v, value.ty, constant).map(|o| op.test(o));
        let (mut mcv_selec, mut sumcommon) = (0.0, 0.0);
        let mut mcvs = 0;
        if let Some(mcv) = s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV) {
            let values = mcv.values.as_deref().unwrap_or_default();
            let numbers = mcv.numbers.as_deref().unwrap_or_default();
            mcvs = numbers.len();
            for (v, &n) in values.iter().zip(numbers) {
                if test(v)? {
                    mcv_selec += f64::from(n);
                }
                sumcommon += f64::from(n);
            }
        }
        let mut selec = 1.0 - f64::from(s.null_frac) - sumcommon;
        selec *= self.ineq_histogram(column, value, op, mcvs)?.unwrap_or(0.5);
        selec += mcv_selec;
        Some(selec.clamp(0.0, 1.0))
    }

    /// `scalarineqsel` for `ctid`, which has no statistics: the part of the pages of the table before the item pointer `tid`, where the last page has half the rows of the other pages. The item number of the pointer is a part of its page by the rows of a page. `<` has one row less and `>=` one row more than this estimate of `<=`. A table with no pages gives all its rows.
    fn ctid_ineq(&self, tid: i64, op: cost::Inequality) -> Option<f64> {
        let pages = self.pages?;
        if pages == 0.0 {
            return Some(1.0);
        }
        let mut block = f64::from(u32::try_from(tid >> 16).ok()?);
        let mut density = self.tuples / (pages - 0.5);
        if block >= pages - 1.0 {
            density *= 0.5;
        }
        if density > 0.0 {
            let offset = f64::from(u16::try_from(tid & 0xFFFF).ok()?);
            block += (offset / density).min(1.0);
        }
        let mut selec = block / (pages - 0.5);
        if op.iseq == op.isgt && self.tuples >= 1.0 {
            selec -= 1.0 / self.tuples;
        }
        if op.isgt {
            selec = 1.0 - selec;
        }
        Some(selec.clamp(0.0, 1.0))
    }

    /// `ineq_histogram_selectivity`: the part of the histogram of `column` that passes `column op value`, with `mcvs` most common values, or `None` in the result when there is no histogram. The binary search finds the bin of the constant, and the place of the constant in the bin comes from [`cost::convert_to_scalar`]. The first and the last bound are the least and the greatest value of the column when an index gives them. An operator of another order than the histogram takes the part of the bounds that pass it.
    fn ineq_histogram(
        &self,
        column: &Expr,
        value: &Expr,
        op: cost::Inequality,
        mcvs: usize,
    ) -> Option<Option<f64>> {
        let s = cost::Estimate::statistic(self, column)?;
        let ExprKind::Const(constant) = &value.kind else { return None };
        let Some(hist) = s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_HISTOGRAM) else {
            return Some(None);
        };
        // The values are borrowed. The search copies them only when it puts the known range of the column in place of an end.
        let mut values: Cow<'_, [Value]> =
            Cow::Borrowed(hist.values.as_deref().unwrap_or_default());
        let nvalues = values.len();
        if nvalues <= 1 {
            return Some(None);
        }
        let test = |v: &Value| crate::compare(column.ty, v, value.ty, constant).map(|o| op.test(o));
        let cutoff = 0.01 / (nvalues - 1) as f64;
        if !op.sorted {
            let mut nmatch = 0;
            for v in values.iter() {
                if test(v)? {
                    nmatch += 1;
                }
            }
            let hist_selec = f64::from(nmatch) / nvalues as f64;
            return Some(Some(hist_selec.clamp(cutoff, 1.0 - cutoff)));
        }
        let range =
            plain_var(column).and_then(|var| self.ranges.get(&var.attnum)).cloned().flatten();
        let mut have_end = false;
        if nvalues == 2
            && let Some((min, max)) = range.clone()
        {
            values = Cow::Owned(vec![min, max]);
            have_end = true;
        }
        let (mut lobound, mut hibound) = (0, nvalues);
        while lobound < hibound {
            let probe = (lobound + hibound) / 2;
            if nvalues > 2 && (probe == 0 || probe == nvalues - 1) {
                have_end = range.is_some();
                if let Some((min, max)) = &range {
                    values.to_mut()[probe] = if probe == 0 { min.clone() } else { max.clone() };
                }
            }
            let mut ltcmp = test(&values[probe])?;
            if op.isgt {
                ltcmp = !ltcmp;
            }
            if ltcmp {
                lobound = probe + 1;
            } else {
                hibound = probe;
            }
        }
        let histfrac = if lobound == 0 {
            0.0
        } else if lobound >= nvalues {
            1.0
        } else {
            let i = lobound;
            let mut eq_selec = 0.0;
            if i == 1 || op.isgt == op.iseq {
                let unique = self.unique.has(column);
                let otherdistinct =
                    cost::numdistinct(column, Some(s), unique, Some(self.tuples)).0 - mcvs as f64;
                if otherdistinct > 1.0 {
                    eq_selec = 1.0 / otherdistinct;
                }
            }
            let binfrac = match cost::convert_to_scalar(
                constant,
                value.ty,
                &values[i - 1],
                &values[i],
                column.ty,
            ) {
                Some((_, low, high)) if high <= low => 0.5,
                Some((val, low, _)) if val <= low => 0.0,
                Some((val, _, high)) if val >= high => 1.0,
                Some((val, low, high)) => {
                    let binfrac = (val - low) / (high - low);
                    if binfrac.is_nan() || !(0.0..=1.0).contains(&binfrac) { 0.5 } else { binfrac }
                }
                None => 0.5,
            };
            let mut histfrac = ((i - 1) as f64 + binfrac) / (nvalues - 1) as f64;
            if i == 1 {
                histfrac += eq_selec * (1.0 - binfrac);
            }
            if op.isgt == op.iseq {
                histfrac -= eq_selec;
            }
            histfrac
        };
        let hist_selec = if op.isgt { 1.0 - histfrac } else { histfrac };
        Some(Some(if have_end {
            hist_selec.clamp(0.0, 1.0)
        } else {
            hist_selec.clamp(cutoff, 1.0 - cutoff)
        }))
    }
}

impl cost::Estimate for ScanEstimate {
    fn eq(&self, left: &Expr, right: &Expr) -> Option<f64> {
        let (column, value) = self.sides(left, right)?;
        Some(self.var_eq(column, value, false))
    }

    fn neq(&self, left: &Expr, right: &Expr) -> Option<f64> {
        let (column, value) = self.sides(left, right)?;
        Some(self.var_eq(column, value, true))
    }

    /// `scalarineqsel_wrapper`: the column goes on the left, so that a column on the right turns the operator around.
    fn ineq(&self, left: &Expr, right: &Expr, op: cost::Inequality) -> Option<f64> {
        let (column, value) = self.sides(left, right)?;
        let op =
            if std::ptr::eq(column, left) { op } else { cost::Inequality { isgt: !op.isgt, ..op } };
        self.scalar_ineq(column, value, op)
    }

    fn statistic(&self, expr: &Expr) -> Option<&Statistic> {
        if plain_var(expr).is_none() {
            return (exprs::relations(expr) == BTreeSet::from([self.r]))
                .then(|| expr_statistic(&self.exprs, expr))
                .flatten();
        }
        let var = plain_var(expr).filter(|v| v.levels_up == 0 && v.relation == self.r)?;
        self.columns.get(&var.attnum)
    }

    fn current_of_tuples(&self, expr: &Expr) -> Option<f64> {
        (exprs::relations(expr) == BTreeSet::from([self.r])).then_some(self.tuples)
    }

    fn histogram_ineq(&self, column: &Expr, value: &Expr, op: cost::Inequality) -> Option<f64> {
        let s = cost::Estimate::statistic(self, column)?;
        let mcvs = s
            .slots
            .iter()
            .find(|slot| slot.kind == STATISTIC_KIND_MCV)
            .map_or(0, |slot| slot.numbers.as_deref().map_or(0, <[f32]>::len));
        self.ineq_histogram(column, value, op, mcvs).flatten()
    }
}

/// A table that a scan reads, with its estimates, its conditions and its indexes, which the parameterized scans of the table read, as the `RelOptInfo` of the table.
#[derive(Debug)]
pub(crate) struct Base<'q> {
    pages: f64,
    tuples: f64,
    rows: f64,
    /// `baserestrictinfo`.
    quals: Vec<Qual<'q>>,
    indexes: Vec<Index>,
    /// The members of a table with inheritance children or partitions, whose parameterized scans make the parameterized `Append` of the table.
    members: Vec<inherit::MemberBase<'q>>,
}

/// `trivial_pathtarget` of `set_subquery_pathlist`: the columns of the relation `r` that the plan reads above the scan are each column of the subquery in order. The plan reads them in the order that they first come in the targets and in `HAVING`, as `build_base_rel_tlists` adds them, then in the conditions of the joins.
fn trivial_target(query: &Query, r: usize, sub: &Query) -> bool {
    // `preprocess_targetlist` of a subquery with a lock adds the junk columns of its row marks.
    if !sub.row_marks.is_empty() {
        return false;
    }
    let mut columns = Vec::new();
    for target in &query.targets {
        exprs::columns_at(&target.expr, 0, &mut columns);
    }
    if let Some(having) = &query.having {
        exprs::columns_at(having, 0, &mut columns);
    }
    let mut quals = Vec::new();
    for item in &query.from {
        join_conditions(item, &mut quals);
    }
    if let Some(filter) = &query.filter {
        exprs::and_parts(filter, &mut quals);
    }
    for qual in quals.into_iter().filter(|q| exprs::relations(q).len() > 1) {
        exprs::columns_at(qual, 0, &mut columns);
    }
    let attnums: Vec<i16> = columns
        .iter()
        .filter_map(|c| match c.kind {
            ExprKind::Var(v) if v.relation == r => Some(v.attnum),
            _ => None,
        })
        .collect();
    attnums.len() == sub.targets.len() && attnums.iter().zip(1..).all(|(a, at)| *a == at)
}

/// `is_simple_union_all`: the subquery is a `UNION ALL` of `SELECT`s with the types of its columns, with no `ORDER BY`, no `LIMIT`, no `OFFSET`, no lock and no `WITH`. `pull_up_simple_union_all` makes it an append relation, whose `Append` the plan has in place of the scan of the subquery.
pub(crate) fn simple_union_all(sub: &Query) -> bool {
    fn union_all(op: &SetOp, sub: &Query, columns: &[(u32, i32)]) -> bool {
        op.kind == SetKind::Union
            && op.all
            && [&op.left, &op.right].into_iter().all(|tree| match tree {
                SetTree::Leaf(at) => {
                    sub.relations.get(*at).and_then(|r| r.subquery.as_deref()).is_some_and(|leaf| {
                        let mut types = leaf
                            .targets
                            .iter()
                            .filter(|t| !t.junk)
                            .map(|t| t.expr.strip_implicit().ty);
                        columns.iter().all(|(ty, _)| types.next() == Some(*ty))
                            && types.next().is_none()
                    })
                }
                SetTree::Op(op) => union_all(op, sub, columns),
            })
    }
    sub.set_op.as_ref().is_some_and(|op| {
        sub.sort.is_empty()
            && sub.limit.is_none()
            && sub.offset.is_none()
            && sub.row_marks.is_empty()
            && sub.ctes.is_empty()
            && op.work.is_none()
            && union_all(op, sub, &op.columns)
    })
}

/// True when the node is the scan of a `UNION ALL` that tests no condition, whose `Append` takes the place of the scan, as [`simple_union_all`].
pub(crate) fn append_rel(query: &Query, node: &Node<'_>) -> bool {
    let Kind::SubqueryScan(r) = node.kind else { return false };
    let sub = query.relations.get(r).and_then(|rel| rel.subquery.as_deref());
    node.filter.is_empty() && sub.is_some_and(simple_union_all)
}

/// The relation that a scan of a table reads.
pub(crate) fn scanned(kind: &Kind<'_>) -> Option<usize> {
    match kind {
        Kind::SeqScan(r)
        | Kind::IndexScan { relation: r, .. }
        | Kind::TidScan { relation: r, .. }
        | Kind::BitmapHeapScan { relation: r, .. } => Some(*r),
        _ => None,
    }
}

/// True when the node is the `Append` of the members of a table with children of `INHERITS`, which has the table itself as a member. `apply_scanjoin_target_to_paths` gives the targets to the members of a partitioned table, but not to the members of such a table.
pub(crate) fn inheritance_append(query: &Query, node: &Node<'_>) -> bool {
    let first = node.children.first().map(|c| match c.kind {
        Kind::Sort(_) => c.children.first().unwrap_or(c),
        _ => c,
    });
    let Some(r) = first.and_then(|c| scanned(&c.kind)) else { return false };
    let Some(oid) = query.relations.get(r).map(|rel| rel.oid) else { return false };
    members_append(node) && node.children.iter().any(|c| member_of(c) == Some(oid))
}

/// The table of the scan of a member of an inheritance tree, or of the sort of one below a `Merge Append` or an ordered `Append`.
pub(crate) fn member_of(node: &Node<'_>) -> Option<u32> {
    match (&node.kind, node.children.as_slice()) {
        (Kind::Sort(_), [child]) if node.member.is_none() => child.member,
        _ => node.member,
    }
}

/// True when the node is an `Append` or a `Merge Append` of the members of an inheritance tree.
pub(crate) fn members_append(node: &Node<'_>) -> bool {
    matches!(node.kind, Kind::Append | Kind::MergeAppend(_))
        && node.children.iter().all(|c| member_of(c).is_some())
}

/// The parts of the conditions of the joins of an item of `FROM`, the joins below first.
pub(crate) fn join_conditions<'q>(item: &'q FromItem, out: &mut Vec<&'q Expr>) {
    let FromItem::Join(join) = item else { return };
    join_conditions(&join.left, out);
    join_conditions(&join.right, out);
    if let Some(on) = &join.on {
        exprs::and_parts(on, out);
    }
}

/// `query_is_distinct_for`: the sets of the columns of a subquery that it gives once each, as column numbers. The query gives each row once on the items of `DISTINCT`, or of `DISTINCT ON` with no function that gives a set. With no function that gives a set, it also gives each row once on the items of `GROUP BY`, or on no column when it makes one group with no `GROUP BY` or with one grouping set or `GROUP BY DISTINCT` of empty sets, and a set operation with no `ALL` gives each row once on all its columns.
pub(crate) fn distinct_keys(sub: &Query) -> Vec<Vec<i16>> {
    let attnum = |at: usize| i16::try_from(at + 1).unwrap_or(i16::MAX);
    let mut out = Vec::new();
    let srfs = sub.has_target_srfs();
    if !sub.distinct.is_empty() && !(srfs && sub.distinct_on) {
        out.push(sub.distinct.iter().map(|d| attnum(d.target)).collect());
    }
    if srfs {
        return out;
    }
    if sub.grouping_sets.is_empty() {
        if !sub.group.is_empty() {
            out.push(sub.group.iter().map(|g| attnum(g.target)).collect());
        } else if sub.grouped {
            out.push(Vec::new());
        }
    } else if sub.group.is_empty()
        && (sub.group_distinct
            || expand_grouping_sets(&sub.grouping_sets, false, usize::MAX)
                .is_some_and(|sets| sets.len() == 1))
    {
        out.push(Vec::new());
    }
    if let Some(op) = &sub.set_op
        && !op.all
    {
        let columns = sub.targets.iter().enumerate().filter(|(_, t)| !t.junk);
        out.push(columns.map(|(at, _)| attnum(at)).collect());
    }
    out
}

/// `get_actual_variable_range` for the columns of the relation `r`, the table `table`, that the inequalities of `expr` compare with a constant, or with the items of an array as `scalararraysel` does, when the column has a histogram and is the first key of a `btree` index of the table with no predicate and with the default operator class of its type.
fn variable_ranges(
    members: Option<&dyn crate::Members>,
    expr: &Expr,
    (r, table): (usize, u32),
    indexes: &[Index],
    stats: &mut Stats,
) {
    for child in expr.children() {
        variable_ranges(members, child, (r, table), indexes, stats);
    }
    if let ExprKind::ScalarArrayOp { func, args, .. } = &expr.kind
        && let [left, array] = args.as_slice()
    {
        let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
        let inequality = cost::array_operator(*func, left.ty, element)
            .is_some_and(|op| cost::scalar_inequality(op.oid).is_some());
        let var = plain_var(left).filter(|v| v.levels_up == 0 && v.relation == r);
        if let (true, Some(var)) = (inequality, var) {
            column_range(members, var.attnum, (r, table), indexes, stats);
        }
        return;
    }
    let Some((op, left, right)) = exprs::binary(expr) else { return };
    if cost::scalar_inequality(op).is_none() {
        return;
    }
    let column = [left, right]
        .into_iter()
        .find_map(|side| plain_var(side).filter(|v| v.levels_up == 0 && v.relation == r));
    let Some(var) = column else { return };
    column_range(members, var.attnum, (r, table), indexes, stats);
}

/// `get_actual_variable_range` for the column `attnum` of the relation `r`, the table `table`, when the column has a histogram and is the first key of a `btree` index of the table with no predicate and with the default operator class of its type.
fn column_range(
    members: Option<&dyn crate::Members>,
    attnum: i16,
    (r, table): (usize, u32),
    indexes: &[Index],
    stats: &mut Stats,
) {
    let histogram = stats
        .columns
        .get(&r)
        .and_then(|c| c.get(&attnum))
        .is_some_and(|s| s.slots.iter().any(|slot| slot.kind == STATISTIC_KIND_HISTOGRAM));
    if !histogram || stats.ranges.contains_key(&(r, attnum)) {
        return;
    }
    let ordered = indexes.iter().any(|index| {
        index.method == index::BTREE_AM
            && !index.partial
            && index.keys.first() == Some(&attnum)
            && index.default_class.first() == Some(&true)
    });
    let range = members.filter(|_| ordered).and_then(|m| m.variable_range(table, attnum));
    stats.ranges.insert((r, attnum), range);
}

/// The columns that each place of a query level reads, as `build_base_rel_tlists`, `distribute_qual_to_rels` and `find_lateral_references` set `attr_needed`: the target list, the conditions of `WHERE` and of `ON`, and the lateral references. With `used`, the query is a member of an append relation, and only the targets that the query above reads count.
fn needed(level: &Level<'_>, stats: &mut Stats, used: Option<&[bool]>) {
    let query = level.query;
    let relations = &query.relations;
    let top = BTreeSet::from([usize::MAX]);
    for (at, target) in query.targets.iter().enumerate() {
        if used.is_some_and(|u| !u.get(at).copied().unwrap_or(true)) {
            continue;
        }
        stats.need(&target.expr, 0, &top, relations);
    }
    for mark in &level.marks {
        stats.need(mark, 0, &top, relations);
    }
    let (to_where, keep) = upper::having_split(query);
    for part in keep {
        stats.need(part, 0, &top, relations);
    }
    let mut parts = to_where;
    if let Some(filter) = &query.filter {
        exprs::and_parts(filter, &mut parts);
    }
    let mut items: Vec<&FromItem> = query.from.iter().collect();
    while let Some(item) = items.pop() {
        if let FromItem::Join(join) = item {
            if let Some(on) = &join.on {
                exprs::and_parts(on, &mut parts);
            }
            items.extend([&join.left, &join.right]);
        }
    }
    for part in parts {
        stats.need(part, 0, &exprs::relations(part), relations);
        if let Some((op, left, right)) = exprs::binary(part)
            && exprs::can_merge(op)
        {
            let (l, r) = (exprs::relations(left), exprs::relations(right));
            if !l.is_empty() && !r.is_empty() && l.is_disjoint(&r) {
                stats.joins.push([(left.clone(), l), (right.clone(), r)]);
            }
        }
    }
    let keys = |items: &[SortGroup]| -> Vec<(Expr, bool, bool)> {
        items
            .iter()
            .map(|item| {
                let desc = builtin::operator_by_oid(item.operator).is_some_and(|o| o.name == ">");
                (level.target(item.target).clone(), desc, item.nulls_first)
            })
            .collect()
    };
    let [group, distinct, sort] = [&query.group, &query.distinct, &query.sort]
        .map(|items| stats.canonical(keys(items), |k| &k.0));
    // `make_pathkeys_for_groupagg`: with no groups, the keys of an ordered aggregate call are the keys of the groups.
    let group = if group.is_empty()
        && query.grouping_sets.is_empty()
        && crate::enable().presorted_aggregate
    {
        let ordered = upper::aggregate_order(query, stats);
        let ordered =
            ordered.into_iter().map(|k| (k.item.expr.into_owned(), k.desc, k.nulls_first));
        stats.canonical(ordered.collect(), |k| &k.0)
    } else {
        group
    };
    // `standard_qp_callback`: only the first window, at the bottom, counts.
    let window = upper::first_window(level).map_or_else(Vec::new, |w| {
        let clause = &query.windows[w];
        let mut all = stats.canonical(keys(&clause.partition), |k| &k.0);
        all.extend(keys(&clause.order));
        stats.canonical(all, |k| &k.0)
    });
    stats.query = [&group, &window, &distinct, &sort]
        .into_iter()
        .find(|keys| !keys.is_empty())
        .cloned()
        .unwrap_or_default();
    stats.sort = sort;
    stats.window = window;
    stats.groups = vec![group, distinct];
    stats.startup = level.fraction > 0.0;
    for r in 0..relations.len() {
        for (expr, depth) in lateral_exprs_at(query, r) {
            let mut at = exprs::relations_at(expr, depth);
            at.insert(r);
            stats.need(expr, depth, &at, relations);
        }
    }
}

/// The tree of a part of `FROM`. A right join is a left join with the sides in the other order.
fn tree(item: &FromItem) -> Tree<'_> {
    match item {
        FromItem::Relation(r) => Tree::Base { relation: *r, quals: Vec::new() },
        FromItem::Join(join) => {
            let (mut left, mut right) = (tree(&join.left), tree(&join.right));
            let mut kind = join.kind;
            if kind == JoinKind::Right {
                std::mem::swap(&mut left, &mut right);
                kind = JoinKind::Left;
            }
            let mut on = Vec::new();
            if let Some(cond) = &join.on {
                exprs::and_parts(cond, &mut on);
            }
            join_tree(kind, left, right, on)
        }
    }
}

fn join_tree<'q>(kind: JoinKind, left: Tree<'q>, right: Tree<'q>, on: Vec<&'q Expr>) -> Tree<'q> {
    let mut rels = left.rels();
    rels.extend(right.rels());
    let semi = if kind == JoinKind::Semi { semi_info(&on, &right.rels()) } else { None };
    let clause = on.iter().flat_map(|part| exprs::relations(part)).collect();
    Tree::Join(Box::new(JoinTree {
        kind,
        left,
        right,
        on,
        quals: Vec::new(),
        filter: Vec::new(),
        rels,
        clause,
        semi,
    }))
}

/// `compute_semijoin_info`: the values of the inner side `inner` of a semi join that the parts of its condition compare with the outer side. A part that reads one side only adds no value. Each other part must compare a value of the outer side with a value of the inner side, with an operator that has a commutator when the inner value is on the left. The result is `None` when a part does not, when no operator can make the rows unique, when no part adds a value, or when a part of one side or a value calls a volatile function.
fn semi_info<'q>(on: &[&'q Expr], inner: &BTreeSet<usize>) -> Option<Semi<'q>> {
    let mut semi = Semi { values: Vec::new(), sort: true, hash: true };
    for &part in on {
        let all = exprs::relations(part);
        if all.is_disjoint(inner) || all.is_subset(inner) {
            if exprs::volatile(part) {
                return None;
            }
            continue;
        }
        let (op, left, right) = exprs::binary(part)?;
        let (l, r) = (exprs::relations(left), exprs::relations(right));
        let (op, value) = if !r.is_empty() && r.is_subset(inner) && l.is_disjoint(inner) {
            (op, right)
        } else if !l.is_empty() && l.is_subset(inner) && r.is_disjoint(inner) {
            let commutator = builtin::operator_by_oid(op)?.commutator;
            (Some(commutator).filter(|&c| c != 0)?, left)
        } else {
            return None;
        };
        semi.sort &= exprs::can_merge(op);
        semi.hash &= exprs::can_hash(op);
        if !semi.sort && !semi.hash {
            return None;
        }
        semi.values.push((value, op));
    }
    if semi.values.is_empty() || semi.values.iter().any(|(v, _)| exprs::volatile(v)) {
        return None;
    }
    Some(semi)
}

/// The `Result` of a query with no `FROM`, with the conditions of `WHERE` as its `One-Time Filter`. A condition that is never true does not make the relation dummy, so the `Result` has the cost and the row of `create_gating_plan`.
fn result<'q>(scope: usize, parts: Vec<&'q Expr>) -> Node<'q> {
    let parts: Vec<&Expr> = parts.into_iter().filter(|p| !exprs::is_true(p)).collect();
    let cost = cost::qual_cost(parts.iter().copied());
    let one_time = parts.into_iter().map(Item::of).collect();
    let mut node = Node::new(Kind::Result { replaces: None, one_time }, scope, Vec::new());
    node.cost = Cost { startup: 0.0, total: cost::CPU_TUPLE + cost, rows: 1.0, width: 0 };
    node
}

/// The `Result` with no rows that takes the place of the relations when a condition of `WHERE` is never true, as `mark_dummy_rel`. `kept` is true when the `Result` of one relation keeps the width of its columns.
fn dummy<'q>(scope: usize, rels: &BTreeSet<usize>, kept: bool) -> Node<'q> {
    let replaces = match rels.iter().collect::<Vec<_>>().as_slice() {
        [r] => Replaced::Scan(**r, kept),
        _ => Replaced::Join(rels.iter().copied().collect()),
    };
    Node::new(
        Kind::Result { replaces: Some(replaces), one_time: vec![false_item()] },
        scope,
        Vec::new(),
    )
}

/// `populate_joinrel_with_paths` for the joins of a join domain with no rows, as `mark_dummy_rel`. A condition that is never true makes the join of its relations dummy, and so does an item that is dummy, unless it is the inner side of a left join or an anti join. The joins above a dummy join are dummy, so the result is the dummy join of all the relations of the domain. A part of `ON` of a left join or an anti join that is never true makes its inner side dummy, with the width of its columns.
fn dummy_domain<'q>(
    level: &Level<'q>,
    items: &mut [Vec<Rel<'q>>],
    pool: &[Qual<'q>],
    specials: &[Special<'q>],
    stats: &Stats,
) -> Option<Rel<'q>> {
    let outer = |sp: &&Special<'_>| matches!(sp.kind, JoinKind::Left | JoinKind::Anti);
    for sp in specials.iter().filter(outer) {
        if sp.quals.iter().any(|q| exprs::is_false(&q.expr))
            && let Some(item) = items.iter_mut().find(|i| i[0].rels == sp.right)
        {
            let mut node = dummy(level.scope, &sp.right, true);
            node.cost.width = stats.width(&sp.right);
            *item = vec![Rel { node, rels: sp.right.clone(), order: Vec::new() }];
        }
    }
    let inner = |rels: &BTreeSet<usize>| specials.iter().filter(outer).any(|sp| sp.right == *rels);
    let is_dummy = |rel: &Rel<'_>| matches!(rel.node.kind, Kind::Result { replaces: Some(_), .. });
    if !pool.iter().any(|q| exprs::is_false(&q.expr))
        && !items.iter().any(|i| is_dummy(&i[0]) && !inner(&i[0].rels))
    {
        return None;
    }
    let rels: BTreeSet<usize> = items.iter().flat_map(|i| i[0].rels.iter().copied()).collect();
    let mut node = dummy(level.scope, &rels, true);
    node.cost.width = stats.width(&rels);
    Some(Rel { node, rels, order: Vec::new() })
}

/// The constant `false`.
fn false_item<'q>() -> Item<'q> {
    Item::owned(Expr::new(ExprKind::Const(Value::Bool(false)), oid::BOOL))
}

/// A number of a constant.
fn number(expr: &Expr) -> Option<f64> {
    match expr.strip_implicit().as_const()? {
        Value::Int2(v) => Some(f64::from(*v)),
        Value::Int4(v) => Some(f64::from(*v)),
        Value::Int8(v) => Some(*v as f64),
        Value::Float4(v) => Some(f64::from(*v)),
        Value::Float8(v) => Some(*v),
        _ => None,
    }
}

/// The number of rows of a function in `FROM`: the rows of `generate_series` with constant integers, as its support function gives them, or else `prorows`.
pub(crate) fn function_rows(call: &Expr) -> f64 {
    let ExprKind::Func(f) = &call.kind else { return 1.0 };
    let name = builtin::proc_by_oid(f.oid).map(|p| p.name);
    if name == Some("generate_series") {
        let ints: Option<Vec<f64>> = f
            .args
            .iter()
            .map(|a| matches!(a.ty, oid::INT4 | oid::INT8).then(|| number(a)).flatten())
            .collect();
        if let Some(ints) = ints {
            let step = ints.get(2).copied().unwrap_or(1.0);
            if let [start, stop, ..] = ints[..]
                && step != 0.0
            {
                return ((stop - start) / step).floor().max(-1.0) + 1.0;
            }
        }
    }
    // `array_unnest_support`: the number of the elements of the array, as `estimate_array_length`.
    if name == Some("unnest")
        && let [array] = f.args.as_slice()
    {
        return match &array.strip_implicit().kind {
            ExprKind::Const(Value::Array(a)) => a.values.len() as f64,
            ExprKind::Const(Value::Null) => 0.0,
            ExprKind::Array { multidims: false, elements, .. } => elements.len() as f64,
            _ => 10.0,
        };
    }
    match name {
        _ if !f.retset => 1.0,
        Some(
            "unnest" | "json_array_elements" | "jsonb_array_elements" | "json_each" | "jsonb_each",
        ) => 100.0,
        Some("regexp_matches") => 10.0,
        _ => 1000.0,
    }
}

/// `find_nonnullable_rels`: the relations whose columns cannot be null in a row that passes the condition. At the top of the condition, `IS NOT NULL` and the other tests that are false for a null count too.
pub(crate) fn nonnullable_rels(expr: &Expr, top: bool) -> BTreeSet<usize> {
    let strict = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.strict);
    let union = |args: &[Expr], top: bool| {
        let mut out = BTreeSet::new();
        for arg in args {
            out.extend(nonnullable_rels(arg, top));
        }
        out
    };
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 => BTreeSet::from([var.relation]),
        ExprKind::Bool(BoolOp::And, args) => union(args, top),
        ExprKind::Bool(BoolOp::Or, args) => {
            let mut sets = args.iter().map(|a| nonnullable_rels(a, top));
            let first = sets.next().unwrap_or_default();
            sets.fold(first, |acc, s| acc.intersection(&s).copied().collect())
        }
        ExprKind::Bool(BoolOp::Not, args) => union(args, false),
        ExprKind::Func(f) if strict(f.oid) => union(&f.args, false),
        ExprKind::ScalarArrayOp { func, args, .. } if strict(*func) => union(args, false),
        ExprKind::Relabel(arg, _) | ExprKind::CoerceViaIo(arg, _) => nonnullable_rels(arg, top),
        // A `PlaceHolderVar` of one relation is null when the relation gives nulls, as its column is.
        ExprKind::PlaceHolder(p) if p.levels_up == 0 => {
            let mut out = nonnullable_rels(&p.expr, top);
            if let [r] = p.relations[..]
                && !p.removed
            {
                out.insert(r);
            }
            out
        }
        ExprKind::NullTest(arg, false) if top => nonnullable_rels(arg, false),
        ExprKind::BooleanTest(
            arg,
            BoolTest::IsTrue | BoolTest::IsFalse | BoolTest::IsNotUnknown,
        ) if top => nonnullable_rels(arg, false),
        _ => BTreeSet::new(),
    }
}

/// `reduce_outer_joins`: an outer join is an inner join when a condition above it is false or null for each row with nulls for the relations of its nullable side.
fn reduce(tree: &mut Tree<'_>, above: &BTreeSet<usize>) {
    let Tree::Join(join) = tree else { return };
    let overlaps = |rels: BTreeSet<usize>| !rels.is_disjoint(above);
    match join.kind {
        JoinKind::Left if overlaps(join.right.rels()) => join.kind = JoinKind::Inner,
        JoinKind::Full => match (overlaps(join.left.rels()), overlaps(join.right.rels())) {
            (true, true) => join.kind = JoinKind::Inner,
            (true, false) => join.kind = JoinKind::Left,
            (false, true) => {
                join.kind = JoinKind::Left;
                std::mem::swap(&mut join.left, &mut join.right);
            }
            (false, false) => {}
        },
        _ => {}
    }
    let mut local = BTreeSet::new();
    if join.kind != JoinKind::Full {
        for part in &join.on {
            local.extend(nonnullable_rels(part, true));
        }
    }
    let (left, right) = match join.kind {
        JoinKind::Inner | JoinKind::Semi => {
            local.extend(above.iter().copied());
            (local.clone(), local)
        }
        JoinKind::Left | JoinKind::Anti => (above.clone(), local),
        _ => (BTreeSet::new(), BTreeSet::new()),
    };
    reduce(&mut join.left, &left);
    reduce(&mut join.right, &right);
}

/// The parts of `ON` of the inner joins and the semi joins of the top join domain that read no relation and call no volatile function, which the joins no longer have. The top join domain has the inner side of an inner join or a semi join, and the outer side of a left join or an anti join, as `deconstruct_recurse` finds it.
fn top_constants<'q>(tree: &mut Tree<'q>, out: &mut Vec<&'q Expr>) {
    let Tree::Join(join) = tree else { return };
    match join.kind {
        JoinKind::Inner | JoinKind::Semi => {
            top_constants(&mut join.left, out);
            top_constants(&mut join.right, out);
            let (constants, rest) = std::mem::take(&mut join.on)
                .into_iter()
                .partition(|part| exprs::relations(part).is_empty() && !exprs::volatile(part));
            join.on = rest;
            out.extend(constants);
        }
        JoinKind::Left | JoinKind::Anti => top_constants(&mut join.left, out),
        _ => {}
    }
}

/// `pull_up_sublinks`: the semi joins and the anti joins of the parts of `WHERE` are a chain of left sides at the top of the tree, and each one has the items of `FROM` with the conditions of `WHERE` as its left side. So `deconstruct_jointree` reads the conditions of `ON` of the items, then the conditions of `WHERE`, and then the inner side and the condition of each join of the chain. This gives the conditions of `ON` below the chain their places. `place_on` then gives the places of the rest.
fn place_below_sublinks<'q>(tree: &mut Tree<'q>, next: &mut usize) {
    match tree {
        Tree::Join(join) if matches!(join.kind, JoinKind::Semi | JoinKind::Anti) => {
            place_below_sublinks(&mut join.left, next);
        }
        _ => place_on(tree, next),
    }
}

/// Gives each part of the condition of `ON` of each join its place. A part of a left join that reads only the nullable side goes to that side. `next` is the place of the next condition in the order of `deconstruct_jointree`.
fn place_on<'q>(tree: &mut Tree<'q>, next: &mut usize) {
    let Tree::Join(join) = tree else { return };
    place_on(&mut join.left, next);
    place_on(&mut join.right, next);
    let right = join.right.rels();
    for part in std::mem::take(&mut join.on) {
        let rels = exprs::relations(part);
        let volatile = exprs::volatile(part);
        let order = Some(*next);
        *next += 1;
        if rels.is_empty() && !volatile {
            if !exprs::is_true(part) {
                join.quals.push(Qual { order, ..Qual::of(part, rels, false) });
            }
            continue;
        }
        let rels = if rels.is_empty() { join.rels.clone() } else { rels };
        match join.kind {
            JoinKind::Inner | JoinKind::Semi => {
                place_join(join, Qual { order, ..Qual::of(part, rels, false) });
            }
            JoinKind::Left | JoinKind::Anti if rels.is_subset(&right) => {
                place(&mut join.right, Qual { order, ..Qual::of(part, rels, true) });
            }
            _ => join.quals.push(Qual { order, ..Qual::of(part, rels, false) }),
        }
    }
}

/// `security_level` of the conditions of the policies of row-level security in the tree, from [`Query::security`], also for those of `ON` of an outer join.
fn mark_security(tree: &mut Tree<'_>, security: &[(Expr, usize)]) {
    let mark = |quals: &mut Vec<Qual<'_>>| {
        for qual in quals {
            let found = security.iter().find(|(e, _)| *e == *qual.expr);
            qual.security = found.map(|&(_, level)| level);
        }
    };
    match tree {
        Tree::Base { quals, .. } => mark(quals),
        Tree::Join(join) => {
            mark(&mut join.quals);
            mark(&mut join.filter);
            mark_security(&mut join.left, security);
            mark_security(&mut join.right, security);
        }
    }
}

/// Gives a condition from above its place in the tree: the scan of its relation, or the lowest join that has its relations. A condition on the nullable side of an outer join stays above the join.
fn place<'q>(tree: &mut Tree<'q>, qual: Qual<'q>) {
    match tree {
        Tree::Base { quals, .. } => quals.push(qual),
        Tree::Join(join) => place_join(join, qual),
    }
}

fn place_join<'q>(join: &mut JoinTree<'q>, qual: Qual<'q>) {
    match join.kind {
        JoinKind::Inner | JoinKind::Semi if qual.rels.is_subset(&join.left.rels()) => {
            place(&mut join.left, qual);
        }
        JoinKind::Inner | JoinKind::Semi if qual.rels.is_subset(&join.right.rels()) => {
            place(&mut join.right, qual);
        }
        JoinKind::Inner | JoinKind::Semi => join.quals.push(qual),
        JoinKind::Left | JoinKind::Anti if qual.rels.is_subset(&join.left.rels()) => {
            place(&mut join.left, qual);
        }
        _ => join.filter.push(qual),
    }
}

/// `contain_leaked_vars` of a condition after `SS_process_sublinks`: a subquery that reads no column of the query runs once as an `InitPlan`, and its `Param` leaks no value.
fn leaky(expr: &Expr) -> bool {
    if !expr.leaky() {
        return false;
    }
    let mut expr = expr.clone();
    expr.each_mut(0, &mut |e, _| {
        if let ExprKind::SubLink(link) = &e.kind
            && matches!(link.kind, SubLinkKind::Expr | SubLinkKind::Array | SubLinkKind::Exists)
            && !exprs::correlated(&link.query, 1)
        {
            *e = Expr::constant(Value::Null, e.ty, -1, None);
            return true;
        }
        false
    });
    expr.leaky()
}

/// True for a condition that PostgreSQL keeps in an equivalence class: an `=` that a merge join can use, with no volatile function, that is not a condition of an outer join.
fn equivalence(qual: &Qual<'_>) -> bool {
    !qual.outer
        && !exprs::volatile(&qual.expr)
        && exprs::binary(&qual.expr).is_some_and(|(op, _, _)| exprs::can_merge(op))
}

/// The conditions of a scan in the order of `baserestrictinfo`: the conditions that are not equivalences, then the equivalences, which the planner makes again. `order_qual_clauses` then sorts them by their cost for each row.
fn restrictions(quals: Vec<Qual<'_>>) -> Vec<Qual<'_>> {
    let (mut out, classes): (Vec<_>, Vec<_>) = quals.into_iter().partition(|q| !equivalence(q));
    out.extend(classes);
    out
}

/// A condition of a join that tests a value of the outer side and a value of the inner side with an operator. The values borrow the condition, so the join search makes no copy of them for each pair of sides.
#[derive(Clone, Copy)]
struct Clause<'a, 'q> {
    qual: &'a Qual<'q>,
    /// True when the condition has the value of the inner side on the left, so that the join tests the commuted condition.
    commuted: bool,
    outer: &'a Expr,
    inner: &'a Expr,
}

impl<'q> Clause<'_, 'q> {
    /// The condition with the value of the outer side on the left, which the plan of the join shows.
    fn item(&self) -> Item<'q> {
        match self.commuted.then(|| exprs::commute(&self.qual.expr)).flatten() {
            Some(commuted) => Item::owned(commuted),
            None => self.qual.item(),
        }
    }
}

/// The condition with the value of `outer` on the left, when the condition compares a value of each side with an operator that `usable` accepts.
fn clause<'a, 'q>(
    qual: &'a Qual<'q>,
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> Option<Clause<'a, 'q>> {
    let (op, left, right) = exprs::binary(&qual.expr)?;
    if !usable(op) || exprs::volatile(&qual.expr) {
        return None;
    }
    let (l, r) = (exprs::relations(left), exprs::relations(right));
    if l.is_empty() || r.is_empty() {
        return None;
    }
    if l.is_subset(outer) && r.is_subset(inner) {
        return Some(Clause { qual, commuted: false, outer: left, inner: right });
    }
    if l.is_subset(inner) && r.is_subset(outer) && exprs::can_commute(&qual.expr) {
        return Some(Clause { qual, commuted: true, outer: right, inner: left });
    }
    None
}

/// A column, or a column under a `RelabelType`, as `examine_variable` finds a `Var`.
pub(crate) fn plain_var(expr: &Expr) -> Option<Var> {
    match &expr.kind {
        ExprKind::Var(var) => Some(*var),
        ExprKind::Relabel(arg, _) => match arg.kind {
            ExprKind::Var(var) => Some(var),
            _ => None,
        },
        _ => None,
    }
}

/// The number of distinct values of an expression, as `get_variable_numdistinct`, and the tuples and the rows of the relation when the expression reads one relation. With statistics it is `stadistinct`. A unique column has as many values as tuples, a `boolean` has 2 values, `tableoid` has 1 and the other system columns are unique. A small table has as many values as tuples, and the others have 200. The flag is true for the default.
fn numdistinct(expr: &Expr, stats: &Stats) -> (f64, bool, Option<(f64, f64)>) {
    let rels = exprs::relations(expr);
    let rel = match (rels.len(), rels.first()) {
        (1, Some(r)) => stats.rows.get(r).copied(),
        _ => None,
    };
    let unique = stats.unique.has(expr);
    let statistic = column_statistic(expr, stats);
    let (nd, default) = cost::numdistinct(expr, statistic, unique, rel.map(|(tuples, _)| tuples));
    (nd, default, rel)
}

/// The statistics of the expression columns of the indexes `indexes` of the relation `r` that have no predicate, from `pg_statistic` of each index, each with its expression with no `RelabelType` above it, in the order of the indexes. `examine_variable` takes no statistics from a partial index, as they are not the statistics of all the rows.
fn index_statistics(
    members: Option<&dyn crate::Members>,
    r: usize,
    indexes: &[Index],
) -> Vec<(Expr, Statistic)> {
    let Some(members) = members else { return Vec::new() };
    let mut out = Vec::new();
    for index in indexes.iter().filter(|i| !i.partial && !i.exprs.is_empty()) {
        let statistics = members.statistics(index.oid);
        for (column, _) in index.keys.iter().enumerate().filter(|(_, key)| **key == 0) {
            let attnum = i16::try_from(column + 1).unwrap_or(i16::MAX);
            let found = statistics.iter().find(|s| s.attnum == attnum && !s.inherit);
            let (Some(statistic), Some(mut expr)) = (found, index::key_expr(index, r, column))
            else {
                continue;
            };
            if let ExprKind::Relabel(arg, _) = expr.kind {
                expr = *arg;
            }
            out.push((expr, statistic.clone()));
        }
    }
    out
}

/// The statistics of an expression of one relation that is not a column, as `examine_variable` finds them: the statistics of the first expression column of an index of the relation whose expression is the expression with no `RelabelType` above it.
fn expr_statistic<'s>(exprs: &'s [(Expr, Statistic)], expr: &Expr) -> Option<&'s Statistic> {
    let base = match &expr.kind {
        ExprKind::Relabel(arg, _) => &**arg,
        _ => expr,
    };
    exprs.iter().find(|(e, _)| e.same(base)).map(|(_, statistic)| statistic)
}

/// The statistics of a column of a table of the level, or of the column under a `RelabelType`, as `examine_variable` finds them for a `Var`, or of an expression of one relation of the level, from its indexes.
fn column_statistic<'s>(expr: &Expr, stats: &'s Stats) -> Option<&'s Statistic> {
    if plain_var(expr).is_none() {
        let rels = exprs::relations(expr);
        let r = rels.first().filter(|_| rels.len() == 1)?;
        return expr_statistic(stats.exprs.get(r)?, expr);
    }
    let var = plain_var(expr).filter(|v| v.levels_up == 0)?;
    stats.columns.get(&var.relation)?.get(&var.attnum)
}

/// The most common values of a column and their frequencies, when its statistics have both.
fn mcvs(statistic: Option<&Statistic>) -> Option<(&[Value], &[f32])> {
    let slot = statistic?.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV)?;
    Some((slot.values.as_deref()?, slot.numbers.as_deref()?))
}

/// A side of an `=` of a join, as `get_join_variables` finds it: the number of its distinct values, true when the number is the default, its statistics, the tuples and the rows of its relation, and its type.
struct JoinVar<'s> {
    nd: f64,
    default: bool,
    statistic: Option<&'s Statistic>,
    rel: Option<(f64, f64)>,
    ty: u32,
}

impl<'s> JoinVar<'s> {
    fn new(expr: &Expr, stats: &'s Stats) -> Self {
        let (nd, default, rel) = numdistinct(expr, stats);
        let mut ty = expr;
        while let ExprKind::Relabel(arg, _) = &ty.kind {
            ty = arg;
        }
        JoinVar { nd, default, statistic: column_statistic(expr, stats), rel, ty: ty.ty }
    }

    fn nullfrac(&self) -> f64 {
        self.statistic.map_or(0.0, |s| f64::from(s.null_frac))
    }
}

/// The matches of the most common values of two sides, as `eqjoinsel_find_matches` finds them for the first `n2` values of the second side: the flags of the values of each side that match, the number of the pairs that match and the sum of the products of the frequencies of the pairs. A value matches one value of the other side at most.
fn find_matches(
    (ty1, values1): (u32, &[Value]),
    (ty2, values2): (u32, &[Value]),
    (numbers1, numbers2): (&[f32], &[f32]),
    n2: usize,
) -> (Vec<bool>, Vec<bool>, usize, f64) {
    let n2 = n2.min(values2.len());
    let mut hasmatch1 = vec![false; values1.len()];
    let mut hasmatch2 = vec![false; n2];
    let (mut nmatches, mut matchprodfreq) = (0, 0.0);
    for (i, v1) in values1.iter().enumerate() {
        for (j, v2) in values2[..n2].iter().enumerate() {
            if hasmatch2[j] || !crate::compare(ty1, v1, ty2, v2).is_some_and(Ordering::is_eq) {
                continue;
            }
            hasmatch1[i] = true;
            hasmatch2[j] = true;
            matchprodfreq += f64::from(numbers1[i]) * f64::from(numbers2[j]);
            nmatches += 1;
            break;
        }
    }
    (hasmatch1, hasmatch2, nmatches, matchprodfreq)
}

/// The sums of the frequencies of the values that match and that do not match, each at most 1.
fn match_freqs(numbers: &[f32], hasmatch: &[bool]) -> (f64, f64) {
    let (mut matched, mut unmatched) = (0.0, 0.0);
    for (&n, &m) in numbers.iter().zip(hasmatch) {
        if m {
            matched += f64::from(n);
        } else {
            unmatched += f64::from(n);
        }
    }
    (matched.clamp(0.0, 1.0), unmatched.clamp(0.0, 1.0))
}

/// `eqjoinsel_inner`: the selectivity of an `=` of a value of each side for an inner join. With the most common values of both sides, the pairs of the values that match have the products of their frequencies, and the other values of a side match the values of the other side that are not most common values, or that have no match, as one of the other distinct values. The smaller estimate of the two sides is the selectivity. With no such values, it is `(1 - nullfrac1) * (1 - nullfrac2) / max(nd1, nd2)`.
fn eqjoinsel_inner(v1: &JoinVar<'_>, v2: &JoinVar<'_>) -> f64 {
    let (Some((values1, numbers1)), Some((values2, numbers2))) =
        (mcvs(v1.statistic), mcvs(v2.statistic))
    else {
        return (1.0 - v1.nullfrac()) * (1.0 - v2.nullfrac()) / v1.nd.max(v2.nd);
    };
    let (hasmatch1, hasmatch2, nmatches, matchprodfreq) =
        find_matches((v1.ty, values1), (v2.ty, values2), (numbers1, numbers2), values2.len());
    let matchprodfreq = matchprodfreq.clamp(0.0, 1.0);
    let (matchfreq1, unmatchfreq1) = match_freqs(numbers1, &hasmatch1);
    let (matchfreq2, unmatchfreq2) = match_freqs(numbers2, &hasmatch2);
    let otherfreq1 = (1.0 - v1.nullfrac() - matchfreq1 - unmatchfreq1).clamp(0.0, 1.0);
    let otherfreq2 = (1.0 - v2.nullfrac() - matchfreq2 - unmatchfreq2).clamp(0.0, 1.0);
    let (nvalues1, nvalues2, nmatches) =
        (numbers1.len() as f64, numbers2.len() as f64, nmatches as f64);
    let mut totalsel1 = matchprodfreq;
    if v2.nd > nvalues2 {
        totalsel1 += unmatchfreq1 * otherfreq2 / (v2.nd - nvalues2);
    }
    if v2.nd > nmatches {
        totalsel1 += otherfreq1 * (otherfreq2 + unmatchfreq2) / (v2.nd - nmatches);
    }
    let mut totalsel2 = matchprodfreq;
    if v1.nd > nvalues1 {
        totalsel2 += unmatchfreq2 * otherfreq1 / (v1.nd - nvalues1);
    }
    if v1.nd > nmatches {
        totalsel2 += otherfreq2 * (otherfreq1 + unmatchfreq1) / (v1.nd - nmatches);
    }
    if totalsel1 < totalsel2 { totalsel1 } else { totalsel2 }
}

/// `eqjoinsel_semi`: the fraction of the rows of the outer side `v1` of a semi join or an anti join that have a match on the inner side `v2`, whose `inner_rows` rows are the rows of the inner side. The number of the inner values is at most the rows of its relation and the rows of the inner side, and with the most common values of both sides only the first `nd2` inner values can match. The outer values that match have their frequencies, and of the other outer values that are not null all have a match when the outer side has no more distinct values than the inner side, or else the fraction `nd2 / nd1`, or half when a number of distinct values is the default.
fn eqjoinsel_semi(v1: &JoinVar<'_>, v2: &JoinVar<'_>, inner_rows: f64) -> f64 {
    let (mut nd1, mut nd2, mut default2) = (v1.nd, v2.nd, v2.default);
    if let Some((_, rows)) = v2.rel
        && nd2 >= rows
    {
        (nd2, default2) = (rows, false);
    }
    if nd2 >= inner_rows {
        (nd2, default2) = (inner_rows, false);
    }
    let nullfrac1 = v1.nullfrac();
    let (Some((values1, numbers1)), Some((values2, numbers2))) =
        (mcvs(v1.statistic), mcvs(v2.statistic))
    else {
        return if v1.default || default2 {
            0.5 * (1.0 - nullfrac1)
        } else if nd1 <= nd2 || nd2 < 0.0 {
            1.0 - nullfrac1
        } else {
            nd2 / nd1 * (1.0 - nullfrac1)
        };
    };
    // `Min(sslot2->nvalues, nd2)` goes to an `int`, which drops the fraction.
    let clamped = (values2.len() as f64).min(nd2) as usize;
    let (hasmatch1, _, nmatches, _) =
        find_matches((v1.ty, values1), (v2.ty, values2), (numbers1, numbers2), clamped);
    let (matchfreq1, _) = match_freqs(numbers1, &hasmatch1);
    let uncertainfrac = if !v1.default && !default2 {
        nd1 -= nmatches as f64;
        nd2 -= nmatches as f64;
        if nd1 <= nd2 || nd2 < 0.0 { 1.0 } else { nd2 / nd1 }
    } else {
        0.5
    };
    let uncertain = (1.0 - matchfreq1 - nullfrac1).clamp(0.0, 1.0);
    matchfreq1 + uncertainfrac * uncertain
}

/// The selectivity of a condition of a join, as `clause_selectivity` finds it for each part of the condition. An `=` of a value of each side has the selectivity of `eqjoinsel_inner`. An `=` of a unique column and a value that reads no relation, in a part of an `OR`, passes one tuple of the relation, as `var_eq_const`.
fn join_selectivity(qual: &Qual<'_>, stats: &Stats) -> f64 {
    let eq = |left: &Expr, right: &Expr| {
        if exprs::reads_columns(left) && exprs::reads_columns(right) {
            return Some(stats.eqjoinsel(left, right));
        }
        [(left, right), (right, left)].into_iter().find_map(|(column, value)| {
            let rels = exprs::relations(column);
            let (tuples, _) = match rels.first() {
                Some(r) if rels.len() == 1 => *stats.rows.get(r)?,
                _ => return None,
            };
            (stats.unique.has(column) && exprs::relations(value).is_empty() && tuples >= 1.0)
                .then(|| 1.0 / tuples)
        })
    };
    cost::selectivity_by(&qual.expr, &eq)
}

/// The selectivity of a condition of a join for `JOIN_INNER`, which `consider_new_or_clause` divides by the selectivity of each condition that it made from the condition for one relation, at most 1 after each division.
fn norm_selectivity(qual: &Qual<'_>, stats: &Stats) -> f64 {
    divide(join_selectivity(qual, stats), &qual.divisors)
}

fn divide(selectivity: f64, divisors: &[f64]) -> f64 {
    divisors.iter().fold(selectivity, |s, d| (s / d).min(1.0))
}

/// `extract_restriction_or_clauses`: for each relation, in the order of the relations, and each `OR` of a join that can move to the relation, in the order of the conditions, a condition of the relation alone from the parts of each arm of the `OR` that read the relation alone. The relation tests the condition when it passes 0.9 of its rows at most, and the selectivity of the `OR` for the join is divided by the selectivity of the condition.
fn extract_or_clauses(tree: &mut Tree<'_>) {
    let mut found = Vec::new();
    movable_ors(tree, &mut found);
    found.sort_by_key(|(order, ..)| *order);
    let all: BTreeSet<usize> = found.iter().flat_map(|(_, _, rels)| rels.iter().copied()).collect();
    let (mut adds, mut divisors) = (Vec::new(), Vec::new());
    for r in all {
        for (order, expr, rels) in &found {
            if !rels.contains(&r) {
                continue;
            }
            let Some(clause) = extract_or_clause(expr, r) else { continue };
            let selectivity = cost::selectivity(&clause);
            if selectivity > 0.9 {
                continue;
            }
            adds.push((r, clause));
            if selectivity > 0.0 {
                divisors.push((*order, selectivity));
            }
        }
    }
    add_extracted(tree, &mut adds, &divisors);
}

/// The `OR` conditions of the joins of the tree, with their places in the order of the conditions and the relations that each can move to, as `join_clause_is_movable_to`: a relation that the condition reads, that is not on the outer side of the outer join of the condition, and that no outer join below the condition can make null.
fn movable_ors(tree: &Tree<'_>, out: &mut Vec<(Option<usize>, Expr, BTreeSet<usize>)>) {
    let Tree::Join(join) = tree else { return };
    movable_ors(&join.left, out);
    movable_ors(&join.right, out);
    let below: BTreeSet<usize> =
        nullable(&join.left).union(&nullable(&join.right)).copied().collect();
    let mut add = |qual: &Qual<'_>, allowed: &dyn Fn(usize) -> bool| {
        if matches!(qual.expr.kind, ExprKind::Bool(BoolOp::Or, _)) {
            let rels =
                qual.rels.iter().copied().filter(|&r| !below.contains(&r) && allowed(r)).collect();
            out.push((qual.order, (*qual.expr).clone(), rels));
        }
    };
    let right = join.right.rels();
    match join.kind {
        JoinKind::Inner | JoinKind::Semi => join.quals.iter().for_each(|q| add(q, &|_| true)),
        JoinKind::Left | JoinKind::Anti => {
            join.quals.iter().for_each(|q| add(q, &|r| right.contains(&r)));
            join.filter.iter().for_each(|q| add(q, &|r| !right.contains(&r)));
        }
        _ => {}
    }
}

/// The relations of the tree that an outer join in the tree can make null.
fn nullable(tree: &Tree<'_>) -> BTreeSet<usize> {
    let Tree::Join(join) = tree else { return BTreeSet::new() };
    let mut out: BTreeSet<usize> =
        nullable(&join.left).union(&nullable(&join.right)).copied().collect();
    match join.kind {
        JoinKind::Left | JoinKind::Anti => out.extend(join.right.rels()),
        JoinKind::Full => out.extend(join.rels.iter().copied()),
        _ => {}
    }
    out
}

/// `extract_or_clause`: an `OR` of the parts of each arm of the condition that read the relation `r` alone, with an `AND` of the parts of an arm when it has two or more, or `None` when an arm has no such part.
fn extract_or_clause(expr: &Expr, r: usize) -> Option<Expr> {
    let ExprKind::Bool(BoolOp::Or, arms) = &expr.kind else { return None };
    let safe =
        |part: &Expr| exprs::relations(part) == BTreeSet::from([r]) && !exprs::volatile(part);
    let mut out = Vec::new();
    for arm in arms {
        let mut parts = Vec::new();
        match &arm.kind {
            ExprKind::Bool(BoolOp::And, args) => {
                for part in args {
                    if matches!(part.kind, ExprKind::Bool(BoolOp::Or, _)) {
                        parts.extend(extract_or_clause(part, r));
                    } else if safe(part) {
                        parts.push(part.clone());
                    }
                }
            }
            _ if safe(arm) => parts.push(arm.clone()),
            _ => {}
        }
        let part = match parts.len() {
            0 => return None,
            1 => parts.remove(0),
            _ => Expr::new(ExprKind::Bool(BoolOp::And, parts), oid::BOOL),
        };
        match part.kind {
            ExprKind::Bool(BoolOp::Or, args) => out.extend(args),
            _ => out.push(part),
        }
    }
    Some(Expr::new(ExprKind::Bool(BoolOp::Or, out), oid::BOOL))
}

/// Adds the conditions that `extract_or_clauses` made to the scans of their relations, and the selectivities that divide the selectivity of each `OR` of a join.
fn add_extracted(
    tree: &mut Tree<'_>,
    adds: &mut Vec<(usize, Expr)>,
    divisors: &[(Option<usize>, f64)],
) {
    match tree {
        Tree::Base { relation, quals } => {
            let (mine, rest) = std::mem::take(adds).into_iter().partition(|(r, _)| r == relation);
            *adds = rest;
            let mine: Vec<(usize, Expr)> = mine;
            quals.extend(mine.into_iter().map(|(r, e)| Qual::implied(e, BTreeSet::from([r]))));
        }
        Tree::Join(join) => {
            add_extracted(&mut join.left, adds, divisors);
            add_extracted(&mut join.right, adds, divisors);
            for qual in join.quals.iter_mut().chain(join.filter.iter_mut()) {
                if qual.order.is_some() {
                    let mine = divisors.iter().filter(|(o, _)| *o == qual.order).map(|&(_, d)| d);
                    qual.divisors.extend(mine);
                }
            }
        }
    }
}

/// The kinds of node that read the rows of the node below once and keep them, so that a scan again costs little, as `cost_rescan`.
fn keeps_rows(kind: &Kind<'_>) -> bool {
    matches!(
        kind,
        Kind::FunctionScan(_)
            | Kind::TableFunctionScan(_)
            | Kind::CteScan(_)
            | Kind::WorkTableScan(_)
            | Kind::Materialize
            | Kind::Sort(_)
    )
}

/// The costs of a scan of the inner side of a nested loop after the first, as `cost_rescan`: the start and the total.
fn rescan(node: &Node<'_>) -> (f64, f64) {
    let c = node.cost;
    match node.kind {
        Kind::Materialize | Kind::Sort(_) => {
            (0.0, cost::CPU_OPERATOR * c.rows + cost::spill(c.rows, c.width))
        }
        // The rows of the query of `WITH` are in a tuplestore, so a scan after the first costs only `cpu_tuple_cost` for each row.
        Kind::CteScan(_) | Kind::WorkTableScan(_) => {
            (0.0, cost::CPU_TUPLE * c.rows + cost::spill(c.rows, c.width))
        }
        Kind::FunctionScan(_) => (0.0, c.total - c.startup),
        _ => (c.startup, c.total),
    }
}

/// A `Material` node over a node, as `cost_material`. The rows go to disk when they do not fit in `work_mem`.
pub(crate) fn materialize(child: Node<'_>) -> Node<'_> {
    let c = child.cost;
    let total = c.total + 2.0 * cost::CPU_OPERATOR * c.rows + cost::spill(c.rows, c.width);
    let mut node = Node::new(Kind::Materialize, child.scope, vec![child]);
    node.cost = Cost { startup: c.startup, total, ..c };
    node
}

/// The `Material` node over the inner side of a merge join, as `create_mergejoin_plan` makes it: one `cpu_operator_cost` for each row.
fn merge_materialize(child: Node<'_>) -> Node<'_> {
    let c = child.cost;
    let mut node = Node::new(Kind::Materialize, child.scope, vec![child]);
    node.cost = Cost { total: c.total + cost::CPU_OPERATOR * c.rows, ..c };
    node
}

/// A `Sort` of a node on keys, as `cost_sort`.
pub(crate) fn sort<'q>(child: Node<'q>, keys: Vec<SortKey<'q>>, bound: Option<f64>) -> Node<'q> {
    let c = child.cost;
    let (startup, total) = cost::sort((c.startup, c.total), c.rows, c.width, bound);
    let mut node = Node::new(Kind::Sort(keys), child.scope, vec![child]);
    node.cost = Cost { startup, total, ..c };
    node.disabled = !crate::enable().sort;
    node
}

/// True when the relations of a node read the relations of another side, as a lateral reference.
fn lateral(level: &Level<'_>, rels: &BTreeSet<usize>, other: &BTreeSet<usize>) -> bool {
    rels.iter().any(|r| !lateral_rels(level, *r).is_disjoint(other))
}

/// The relations of the query that the expressions of a relation of `FROM` read: its function, its `VALUES` lists or the columns of the query in its subquery.
pub(crate) fn lateral_rels<'a>(level: &'a Level<'_>, r: usize) -> &'a BTreeSet<usize> {
    let all = level.lateral.get_or_init(|| {
        (0..level.query.relations.len())
            .map(|r| {
                let mut out = BTreeSet::new();
                for expr in lateral_exprs_at(level.query, r) {
                    out.extend(exprs::relations_at(expr.0, expr.1));
                }
                out.remove(&r);
                out
            })
            .collect()
    });
    &all[r]
}

/// [`lateral_exprs`] of the relation `r` of the query, with the expressions of the `PlaceHolderVar`s that the scan of `r` computes, whose columns of the other relations are its `ph_lateral`.
pub(crate) fn lateral_exprs_at(query: &Query, r: usize) -> Vec<(&Expr, usize)> {
    let mut out = lateral_exprs(&query.relations[r]);
    for (expr, depth) in query.all_exprs(0) {
        expr.find(0, &mut |e, d| {
            if let ExprKind::PlaceHolder(p) = &e.kind
                && p.levels_up == d + depth
            {
                let at = exprs::eval_at(p);
                if at.len() == 1 && at.contains(&r) {
                    out.push((&p.expr, p.levels_up));
                }
            }
            None::<()>
        });
    }
    out
}

/// The relations of the query below a node of the query, and the relations that a `Result` takes the place of.
pub(crate) fn node_rels(node: &Node<'_>) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    match &node.kind {
        Kind::SeqScan(r)
        | Kind::IndexScan { relation: r, .. }
        | Kind::TidScan { relation: r, .. }
        | Kind::BitmapHeapScan { relation: r, .. }
        | Kind::SampleScan(r)
        | Kind::FunctionScan(r)
        | Kind::TableFunctionScan(r)
        | Kind::ValuesScan(r)
        | Kind::SubqueryScan(r)
        | Kind::CteScan(r)
        | Kind::WorkTableScan(r) => {
            out.insert(*r);
            return out;
        }
        Kind::Result { replaces: Some(Replaced::Scan(r, _)), .. } => {
            out.insert(*r);
        }
        Kind::Result { replaces: Some(Replaced::Join(rels)), .. } => {
            out.extend(rels.iter().copied())
        }
        _ => {}
    }
    for child in &node.children {
        if child.scope == node.scope {
            out.extend(node_rels(child));
        }
    }
    out
}

/// The expressions of a relation of `FROM` that can read the other relations of the query, each with its depth below the query, as `extract_lateral_references` finds them. The arguments of `TABLESAMPLE` of a table count too.
pub(crate) fn lateral_exprs(relation: &Relation) -> Vec<(&Expr, usize)> {
    let mut out = Vec::new();
    if let Some(sample) = &relation.tablesample {
        out.extend(sample.args.iter().chain(&sample.repeatable).map(|e| (e, 0)));
    }
    if let Some(function) = &relation.function {
        out.extend(function.calls.iter().map(|(call, _)| (call, 0)));
    }
    if let Some(values) = &relation.values {
        out.extend(values.iter().flatten().map(|e| (e, 0)));
    }
    if relation.lateral
        && let Some(sub) = &relation.subquery
    {
        out.extend(sub.all_exprs(1));
    }
    out
}

/// A join of the plans of two sides, with the costs of each kind of join for each order of the sides, as `add_paths_to_joinrel`. The result is the plans that `add_path` keeps, with the plan with the lowest total cost first. Each side has its plans in the same order.
fn join_rels<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: Vec<Rel<'q>>,
    right: Vec<Rel<'q>>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let rows = join_rows(kind, &left[0], &right[0], &quals, &filter, stats);
    let mut quals = quals;
    stats.sort_quals(&mut quals);
    let mut paths = Paths::new(stats.startup);
    let found = choose(level, kind, &left, &right, &quals, &filter, stats, Part::Plain, (&[], &[]));
    for (candidate, keys) in found {
        paths.add((candidate.disabled, candidate.startup, candidate.total), keys, candidate);
    }
    let kept = paths.kept();
    let mut out: Vec<Rel<'q>> = if kept.is_empty() {
        let (left, right) = (left.into_iter().next(), right.into_iter().next());
        let (Some(left), Some(right)) = (left, right) else { unreachable!("each side has a plan") };
        let mut rels = left.rels.clone();
        rels.extend(right.rels.iter().copied());
        vec![nested_join(level, kind, left, right, quals, filter, rows, rels)]
    } else {
        kept.iter()
            .map(|best| {
                let (l, r) = if best.swapped { (best.paths.1, best.paths.0) } else { best.paths };
                finish(level, best, left[l].clone(), right[r].clone(), &quals, &filter, rows)
            })
            .collect()
    };
    for rel in &mut out {
        rel.node.cost.width = stats.width(&rel.rels);
    }
    out
}

/// A semi join or an anti join of two sides, as `populate_joinrel_with_paths` makes it: the join with each order of the sides, and for a semi join an inner join with the unique rows of the inner side, as `create_unique_paths` makes them, with that side as the inner side and as the outer side. The rows are the rows of the semi join or the anti join with the left side as the outer side, as `calc_joinrel_size_estimate` finds them once for the joins of the same relations.
#[allow(clippy::too_many_arguments)]
fn semi_rels<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    semi: Option<&Semi<'q>>,
    left: Vec<Rel<'q>>,
    right: Vec<Rel<'q>>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let fraction = semi_selectivity(&quals, &right[0], stats);
    let outer_rows = left[0].node.cost.rows;
    let rows = if kind == JoinKind::Anti {
        let pushed = cost::list_selectivity(filter.iter().map(|q| &*q.expr));
        outer_rows * (1.0 - fraction) * pushed
    } else {
        outer_rows * fraction
    };
    let rows = cost::clamp_rows(rows);
    let mut quals = quals;
    stats.sort_quals(&mut quals);
    let unique =
        semi.filter(|_| kind == JoinKind::Semi).and_then(|s| unique_rels(s, &right, stats));
    let mut paths = Paths::new(stats.startup);
    let part = Part::Semi(fraction);
    let found = choose(level, kind, &left, &right, &quals, &filter, stats, part, (&[], &[]));
    for (candidate, keys) in found {
        let cost = (candidate.disabled, candidate.startup, candidate.total);
        paths.add(cost, keys, (candidate, false));
    }
    if let Some(unique) = &unique {
        let part = Part::Unique(fraction, true);
        let inner = JoinKind::Inner;
        let found = choose(level, inner, &left, unique, &quals, &filter, stats, part, (&[], &[]));
        for (candidate, keys) in found {
            let cost = (candidate.disabled, candidate.startup, candidate.total);
            paths.add(cost, keys, (candidate, true));
        }
    }
    let kept = paths.kept();
    let mut out: Vec<Rel<'q>> = if kept.is_empty() {
        let (left, right) = (left.into_iter().next(), right.into_iter().next());
        let (Some(left), Some(right)) = (left, right) else { unreachable!("each side has a plan") };
        let mut rels = left.rels.clone();
        rels.extend(right.rels.iter().copied());
        vec![nested_join(level, kind, left, right, quals, filter, rows, rels)]
    } else {
        kept.iter()
            .map(|(best, made)| {
                let side = match &unique {
                    Some(unique) if *made => unique,
                    _ => &right,
                };
                let (l, r) = if best.swapped { (best.paths.1, best.paths.0) } else { best.paths };
                finish(level, best, left[l].clone(), side[r].clone(), &quals, &filter, rows)
            })
            .collect()
    };
    for rel in &mut out {
        rel.node.cost.width = stats.width(&rel.rels);
    }
    out
}

/// `create_final_unique_paths`: the plans of the unique rows of the inner side of a semi join on its values, with the plan with the lowest total cost first. When a sort can make them unique, a `Unique` reads each plan of the side in the order of the values, and the cheapest plan with a `Sort`. When a hash table can, a `HashAggregate` reads the cheapest plan. A value that the values before it make redundant is not a key, as `make_pathkeys_for_sortclauses` finds it. The rows are the groups of all the values. The result is `None` when no value is a key.
fn unique_rels<'q>(semi: &Semi<'q>, side: &[Rel<'q>], stats: &Stats) -> Option<Vec<Rel<'q>>> {
    let mut keys: Vec<&'q Expr> = Vec::new();
    for &(value, op) in &semi.values {
        // A value whose operator has no order is a key of the hash table only, and no sort makes it redundant.
        if exprs::can_merge(op) && stats.redundant(value, &keys) {
            continue;
        }
        keys.push(value);
    }
    if keys.is_empty() || !semi.sort && !semi.hash {
        return None;
    }
    let values: Vec<&Expr> = semi.values.iter().map(|&(v, _)| v).collect();
    // `create_projection_path`: a key that is not a column of the side is a new item of the target of the side, with its cost and its width.
    let mut extra: Vec<&Expr> = Vec::new();
    for &k in &keys {
        if !matches!(k.kind, ExprKind::Var(_)) && !extra.iter().any(|e| e.same(k)) {
            extra.push(k);
        }
    }
    let (extra_startup, extra_cost) = stats.qual_costs(extra.iter().copied());
    let extra_width: i32 = extra.iter().map(|e| cost::type_width(e.ty, e.typmod)).sum();
    let project = |mut node: Node<'q>| -> Node<'q> {
        node.cost.startup += extra_startup;
        node.cost.total += extra_startup + extra_cost * node.cost.rows;
        node.cost.width += extra_width;
        node
    };
    let cheapest = &side[0];
    let groups = stats.groups(&values, cheapest.node.cost.rows);
    let n = keys.len() as f64;
    let order: Vec<OrderKey> = keys.iter().map(|&k| stats.order_key(k, false, false)).collect();
    let mut paths = Paths::new(stats.startup);
    if semi.sort {
        for (at, rel) in side.iter().enumerate() {
            let sorted = keys.len() <= rel.order.len()
                && keys.iter().zip(&rel.order).all(|(k, o)| o.sorts(k, false, false));
            if !sorted && at != 0 {
                continue;
            }
            let mut input = project(rel.node.clone());
            if !sorted {
                let sort_keys = keys
                    .iter()
                    .map(|&k| SortKey {
                        item: Item::below(k),
                        desc: false,
                        using: None,
                        nulls_first: false,
                        collation: 0,
                    })
                    .collect();
                input = sort(input, sort_keys, None);
            }
            let c = input.cost;
            let mut node = Node::new(Kind::Unique, input.scope, vec![input]);
            let total = c.total + cost::CPU_OPERATOR * c.rows * n;
            node.cost = Cost { startup: c.startup, total, rows: groups, width: c.width };
            let rel = Rel { node, rels: rel.rels.clone(), order: order.clone() };
            paths.add(costs(&rel.node), order.clone(), rel);
        }
    }
    if semi.hash {
        let input = project(cheapest.node.clone());
        let c = input.cost;
        let startup = c.total + cost::CPU_OPERATOR * n * c.rows;
        let total = startup + cost::CPU_TUPLE * groups;
        let items = keys.iter().map(|&k| Item::below(k)).collect();
        let kind = Kind::Aggregate {
            strategy: Strategy::Hashed,
            keys: items,
            sets: Vec::new(),
            unique: true,
            split: Split::Simple,
            table: None,
        };
        // The target of the `Agg` is the target of the side without the new items, as `create_final_unique_paths` gives it.
        let width = cheapest.node.cost.width;
        let mut node = Node::new(kind, input.scope, vec![input]);
        node.cost = Cost { startup, total, rows: groups, width };
        let rel = Rel { node, rels: cheapest.rels.clone(), order: Vec::new() };
        paths.add(costs(&rel.node), Vec::new(), rel);
    }
    Some(paths.kept())
}

/// `clauselist_selectivity` for `JOIN_SEMI`: the fraction of the outer rows of a semi join or an anti join that have a match on the inner side. An `=` of a value of each side has the selectivity of `eqjoinsel_semi` with no statistics: 0.5 when a number of distinct values is the default, or else 1 when the inner side has as many values as the outer side, or the fraction of the outer values that it has. The number of the inner values is at most the rows of its relation and the rows of the inner side, and the selectivity is at most the rows of the inner side times the selectivity of the `=` for an inner join. Each other condition has its own selectivity.
fn semi_selectivity(quals: &[Qual<'_>], inner: &Rel<'_>, stats: &Stats) -> f64 {
    let inner_rows = inner.node.cost.rows;
    let one = |qual: &Qual<'_>| {
        let Some((op, left, right)) = exprs::binary(&qual.expr) else {
            return cost::selectivity(&qual.expr);
        };
        let equal = builtin::operator_by_oid(op).is_some_and(|o| o.name == "=");
        if !equal || !exprs::reads_columns(left) || !exprs::reads_columns(right) {
            return cost::selectivity(&qual.expr);
        }
        let (l, r) = (exprs::relations(left), exprs::relations(right));
        let (outer_value, inner_value) = if l.is_subset(&inner.rels) && r.is_disjoint(&inner.rels) {
            (right, left)
        } else {
            (left, right)
        };
        let (v1, v2) = (JoinVar::new(outer_value, stats), JoinVar::new(inner_value, stats));
        let selectivity = eqjoinsel_semi(&v1, &v2, inner_rows);
        selectivity.min(inner_rows * eqjoinsel_inner(&v1, &v2)).clamp(0.0, 1.0)
    };
    quals.iter().map(one).product()
}

/// `compute_semi_anti_join_factors` for a join of a semi join or an anti join: the fraction of the outer rows that have a match, which is the selectivity of the semi join, and the number of inner rows that each of them matches, which comes from the selectivity of the conditions for an inner join. Each outer row with a match matches one inner row at least.
fn semi_factors(fraction: f64, inner: &Rel<'_>, quals: &[Qual<'_>], stats: &Stats) -> (f64, f64) {
    let selectivity: f64 = quals.iter().map(|q| norm_selectivity(q, stats)).product();
    if fraction <= 0.0 {
        return (fraction, 1.0);
    }
    (fraction, (selectivity * inner.node.cost.rows / fraction).max(1.0))
}

/// What a join of `choose` does for a semi join, as the `jointype` of `add_paths_to_joinrel`.
#[derive(Clone, Copy, Debug)]
enum Part {
    /// A join that is not a part of a semi join.
    Plain,
    /// The semi join or the anti join itself, with the fraction of the outer rows that have a match.
    Semi(f64),
    /// An inner join with the unique rows of the inner side of a semi join, as `JOIN_UNIQUE_INNER` and `JOIN_UNIQUE_OUTER`, with the fraction of the semi join, and true when the outer side has all the outer relations of the semi join, so that the unique rows are unique for it.
    Unique(f64, bool),
}

/// The rows of a join of two sides, as `calc_joinrel_size_estimate`.
fn join_rows(
    kind: JoinKind,
    left: &Rel<'_>,
    right: &Rel<'_>,
    quals: &[Qual<'_>],
    filter: &[Qual<'_>],
    stats: &Stats,
) -> f64 {
    let one = |q: &Qual<'_>| {
        if kind == JoinKind::Inner {
            norm_selectivity(q, stats)
        } else {
            join_selectivity(q, stats)
        }
    };
    let selectivity: f64 = quals.iter().map(one).product();
    let mut rows = left.node.cost.rows * right.node.cost.rows * selectivity;
    match kind {
        JoinKind::Left => rows = rows.max(left.node.cost.rows),
        JoinKind::Full => rows = rows.max(left.node.cost.rows).max(right.node.cost.rows),
        _ => {}
    }
    // `calc_joinrel_size_estimate` takes the selectivity of the conditions above an outer join for the kind of the join, which `consider_new_or_clause` does not change.
    let filter_selectivity = cost::list_selectivity(filter.iter().map(|q| &*q.expr));
    cost::clamp_rows(rows * filter_selectivity)
}

/// Each kind of join for the two sides in each order that the join can have, in the order that `add_paths_to_joinrel` makes them, with the part of the order of its rows that a node above can use. A full join that no kind of join can do has none. A semi join with the sides in the other order is a right semi join, which only a hash join does, and an anti join is a right anti join.
#[allow(clippy::too_many_arguments)]
fn choose<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: &[Rel<'q>],
    right: &[Rel<'q>],
    quals: &[Qual<'q>],
    filter: &[Qual<'q>],
    stats: &Stats,
    part: Part,
    params: (&[Param<'q>], &[Param<'q>]),
) -> Vec<(Candidate<'q>, Vec<OrderKey>)> {
    let (filter_startup, filter_cost) = stats.qual_costs(filter.iter().map(|q| &*q.expr));
    let (left_rels, right_rels) = (&left[0].rels, &right[0].rels);
    let lateral_right = lateral(level, right_rels, left_rels);
    // The orders of the sides: the left side first, and the right side first when the join can have the sides in the other order.
    let mut orders = vec![false];
    if !lateral_right && kind != JoinKind::Left || kind == JoinKind::Left && !lateral_right {
        orders.push(true);
    }
    let mut rels = left_rels.clone();
    rels.extend(right_rels.iter().copied());
    let mut out = Vec::new();
    for swapped in orders {
        let (outers, inners) = if swapped { (right, left) } else { (left, right) };
        let join = match (kind, swapped) {
            (JoinKind::Left, true) => JoinKind::Right,
            (JoinKind::Semi, true) => JoinKind::RightSemi,
            (JoinKind::Anti, true) => JoinKind::RightAnti,
            (k, _) => k,
        };
        let index = semifactors(&outers[0].rels, &inners[0], quals, stats);
        // `inner_unique`: a semi join does not prove it, and the unique rows of the inner side of a semi join have it. The factors are for a semi join, an anti join and a join whose inner side is unique.
        let (inner_unique, unique) = match part {
            Part::Plain => (index.is_some(), index),
            Part::Semi(fraction) | Part::Unique(fraction, _) => {
                let inner_unique = match (join, part) {
                    (JoinKind::Semi, _) => false,
                    (JoinKind::Inner, Part::Unique(_, whole)) if !swapped => whole,
                    _ => index.is_some(),
                };
                let semi = matches!(join, JoinKind::Semi | JoinKind::Anti);
                let factors = semi_factors(fraction, &inners[0], quals, stats);
                (inner_unique, (inner_unique || semi).then_some(factors))
            }
        };
        let made = match part {
            Part::Unique(..) => (swapped, !swapped),
            _ => (false, false),
        };
        let input = Input {
            outer: &outers[0],
            inner: &inners[0],
            quals,
            filter_startup,
            filter_cost,
            filtered: !filter.is_empty(),
            unique,
            made,
            indexed: false,
            stats,
        };
        // A side that reads the other side as a lateral reference is the inner side of a nested loop, which runs it again for each outer row. A hash join and a merge join read the inner side once, so that `add_paths_to_joinrel` makes them only with an inner side that is not parameterized.
        let reads_outer =
            if swapped { lateral(level, left_rels, right_rels) } else { lateral_right };
        let nested =
            matches!(join, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti);
        let inner_params = match swapped {
            _ if !nested || made.1 => &[][..],
            true => params.0,
            false => params.1,
        };
        // `add_paths_to_joinrel`: a setting that turns off merge joins or hash joins takes them away, but a full join keeps them, as it may have no other plan. A nested loop with a `Material` node needs nested loops.
        let enable = crate::enable();
        let full = join == JoinKind::Full;
        let merge = join != JoinKind::RightSemi && (enable.mergejoin || full) && !reads_outer;
        let mut candidates: Vec<Candidate<'q>> = Vec::new();
        if merge {
            candidates.extend(sorted_merge(&input, join));
        }
        // `match_unsorted_outer`: each plan of the outer side, with the cheapest plan of the inner side or with an inner plan in the order of the merge.
        for (p, outer) in outers.iter().enumerate() {
            let input = Input { outer, ..input };
            let mut each = Vec::new();
            if nested {
                let mut loops = nested_loops(&input, join, reads_outer);
                let material =
                    (loops.len() > 1).then(|| loops.remove(1)).filter(|_| enable.nestloop);
                each.extend(loops);
                // `get_memoize_path` over the inner relation that reads the outer side, with the conditions that its scan tests as its parameters.
                let moved = (reads_outer && enable.memoize && enable.nestloop)
                    .then(|| movable(level, &outer.rels, &input.inner.rels, quals))
                    .flatten();
                if let Some((moved, _)) = moved
                    && let Some(&r) = input.inner.rels.first()
                {
                    let all = quals.len() + filter.len();
                    let unique = (join, inner_unique);
                    let memo = memoize::lateral(
                        level.query,
                        r,
                        outer,
                        input.inner,
                        unique,
                        &moved,
                        all,
                        stats,
                    );
                    if let Some(memo) = memo {
                        let wrapped =
                            Some((memoize::path_cost(input.inner.node.cost), memo.rescan));
                        let (startup, total) = nested_cost(&input, wrapped);
                        each.push(Candidate {
                            method: Method::Nested,
                            join,
                            swapped: false,
                            startup,
                            total,
                            materialize: false,
                            sort: (false, false),
                            keys: Vec::new(),
                            paths: (0, 0),
                            unique: false,
                            param: None,
                            disabled: 0,
                            memo: Some(memo),
                        });
                    }
                }
                for (i, param) in inner_params.iter().enumerate() {
                    if param.outer.is_subset(&outer.rels) {
                        let left = (quals, filter);
                        each.push(param_loop(&input, join, param, i, left, None));
                        // `get_memoize_path`, which `PGS_NESTLOOP_MEMOIZE` allows.
                        if enable.memoize && enable.nestloop {
                            let memo =
                                memoize::memoize(outer, param, join, inner_unique, left, stats);
                            if let Some(memo) = memo {
                                each.push(param_loop(&input, join, param, i, left, Some(memo)));
                            }
                        }
                    }
                }
                each.extend(material);
            }
            if merge {
                each.extend(presorted_merges(&input, inners, join));
            }
            candidates.extend(each.into_iter().map(|c| Candidate { paths: (p, c.paths.1), ..c }));
        }
        if (enable.hashjoin || full) && !reads_outer {
            candidates.extend(hash_join(&input, join));
        }
        for candidate in candidates {
            let (outer, inner) = (&outers[candidate.paths.0], &inners[candidate.paths.1]);
            let keys = stats.useful(&rels, &candidate.order(outer, inner, quals));
            let inner_node = match candidate.param {
                Some(i) => &inner_params[i].rel.node,
                None => &inner.node,
            };
            let disabled = candidate.own_disabled()
                + outer.node.disabled_nodes()
                + inner_node.disabled_nodes();
            out.push((Candidate { swapped, unique: inner_unique, disabled, ..candidate }, keys));
        }
    }
    out
}

/// The plan of the join of two sides with the kind of join that `choose` found.
fn finish<'q>(
    level: &Level<'q>,
    best: &Candidate<'q>,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: &[Qual<'q>],
    filter: &[Qual<'q>],
    rows: f64,
) -> Rel<'q> {
    let mut rels = left.rels.clone();
    rels.extend(right.rels.iter().copied());
    let (outer, mut inner) = if best.swapped { (right, left) } else { (left, right) };
    let quals = lateral_quals(level, best, &outer, &mut inner, quals);
    let mut out = best.make(level.scope, outer, inner, &quals, rows);
    out.node.filter = filter.iter().map(|q| q.item()).collect();
    out.rels = rels;
    out
}

/// The conditions of a nested loop whose inner relation reads the outer side as a lateral reference, after the inner scan takes the conditions that [`movable`] finds. The other conditions stay with the join.
fn lateral_quals<'q>(
    level: &Level<'q>,
    best: &Candidate<'q>,
    outer: &Rel<'q>,
    inner: &mut Rel<'q>,
    quals: &[Qual<'q>],
) -> Vec<Qual<'q>> {
    let found = (best.method == Method::Nested && best.param.is_none())
        .then(|| movable(level, &outer.rels, &inner.rels, quals))
        .flatten();
    let Some((moved, kept)) = found else { return quals.to_vec() };
    inner.node.filter.extend(moved.iter().map(|q| q.item()));
    kept
}

/// The conditions that the scan of the inner relation of a nested loop tests when the relation reads the outer side as a lateral reference, and the conditions that stay with the join, or `None` for an inner side of more than one relation or with no lateral reference to the outer side. The scan is parameterized by the relations that it reads, so that `join_clause_is_movable_into` lets it test each condition on its relation and those relations, as the `ppi_clauses` of its `ParamPathInfo`.
fn movable<'q>(
    level: &Level<'q>,
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    quals: &[Qual<'q>],
) -> Option<(Vec<Qual<'q>>, Vec<Qual<'q>>)> {
    if inner.len() != 1 || !lateral(level, inner, outer) {
        return None;
    }
    let mut reach: BTreeSet<usize> =
        inner.iter().flat_map(|r| lateral_rels(level, *r).iter().copied()).collect();
    reach.extend(inner.iter().copied());
    Some(
        quals.iter().cloned().partition(|q| q.rels.is_subset(&reach) && !q.rels.is_disjoint(inner)),
    )
}

/// The items of the problem of the order of the joins of a join domain, as the list of `deconstruct_jointree`, the conditions of its inner joins and its semi joins, and its left joins, semi joins and anti joins, as `make_outerjoininfo` makes them. The inner side of a left join, a semi join or an anti join is one item. A condition above a left join that reads its inner side is a condition of the domain, and the search tests it at the left join or above it. A condition with no column has the relations of its join.
fn flatten<'q>(
    tree: Tree<'q>,
    items: &mut Vec<Tree<'q>>,
    pool: &mut Vec<Qual<'q>>,
    specials: &mut Vec<Special<'q>>,
) {
    let Tree::Join(join) = tree else {
        items.push(tree);
        return;
    };
    let JoinTree { kind, left, right, quals, filter, rels, clause, semi, .. } = *join;
    let add = |quals: Vec<Qual<'q>>, pool: &mut Vec<Qual<'q>>| {
        for qual in quals {
            let rels = if qual.rels.is_empty() { rels.clone() } else { qual.rels.clone() };
            pool.push(Qual { rels, ..qual });
        }
    };
    match kind {
        JoinKind::Inner => {
            flatten(left, items, pool, specials);
            flatten(right, items, pool, specials);
            add(quals.into_iter().chain(filter).collect(), pool);
        }
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti => {
            let (outer, right_rels) = (left.rels(), right.rels());
            let lower = specials.len();
            flatten(left, items, pool, specials);
            items.push(right);
            let min_left = min_lefthand(&clause, &outer, &specials[lower..]);
            let (quals, filter) = match kind {
                JoinKind::Semi => {
                    add(quals, pool);
                    (Vec::new(), filter)
                }
                JoinKind::Left => {
                    add(filter, pool);
                    (quals, Vec::new())
                }
                _ => (quals, filter),
            };
            let (left, right) = (min_left, right_rels);
            specials.push(Special { kind, left, right, syn: rels, quals, filter, semi });
        }
        _ => items.push(Tree::Join(Box::new(JoinTree {
            kind,
            left,
            right,
            on: Vec::new(),
            quals,
            filter,
            rels,
            clause,
            semi,
        }))),
    }
}

/// `make_outerjoininfo`: the relations of the outer side of a special join that the order of the joins must join before it. These are the relations of the outer side that the condition reads, or each relation of the outer side when it reads none. When the condition reads the inner side of a lower special join of the outer side, they also have each relation of that join, so that the two joins keep their order. PostgreSQL can let two left joins change their order when the condition is strict, as identity 3 of `optimizer/README`, which the planner does not do yet.
fn min_lefthand(
    clause: &BTreeSet<usize>,
    outer: &BTreeSet<usize>,
    lower: &[Special<'_>],
) -> BTreeSet<usize> {
    let mut min: BTreeSet<usize> = clause.intersection(outer).copied().collect();
    for sp in lower {
        if !sp.right.is_disjoint(clause) {
            min.extend(sp.syn.iter().copied());
        }
    }
    if min.is_empty() { outer.clone() } else { min }
}

/// A full join with no condition that a merge join or a hash join can use, which PostgreSQL cannot plan. The plan is a nested loop.
#[allow(clippy::too_many_arguments)]
fn nested_join<'q>(
    level: &Level<'q>,
    kind: JoinKind,
    left: Rel<'q>,
    right: Rel<'q>,
    quals: Vec<Qual<'q>>,
    filter: Vec<Qual<'q>>,
    rows: f64,
    rels: BTreeSet<usize>,
) -> Rel<'q> {
    let c = (left.node.cost, right.node.cost);
    let inner = materialize(right.node);
    let join_filter = quals.iter().map(|q| q.item()).collect();
    let mut node = Node::new(
        Kind::NestedLoop { join: kind, join_filter },
        level.scope,
        vec![left.node, inner],
    );
    node.filter = filter.iter().map(|q| q.item()).collect();
    let total = c.0.total + c.1.total + c.0.rows * c.1.rows * cost::CPU_TUPLE;
    node.cost = Cost { startup: c.0.startup + c.1.startup, total, rows, width: 0 };
    Rel { node, rels, order: Vec::new() }
}

/// The parts of a join that each kind of join reads.
struct Input<'a, 'q> {
    outer: &'a Rel<'q>,
    inner: &'a Rel<'q>,
    quals: &'a [Qual<'q>],
    /// The start cost and the cost for each row of the conditions that the join tests after it joins the rows.
    filter_startup: f64,
    filter_cost: f64,
    /// True when the join has conditions that are not conditions of the join itself, which the join tests after it joins the rows.
    filtered: bool,
    /// `semifactors`: for a semi join, an anti join or an inner side with one row at most for each outer row, the fraction of the outer rows that have a match and the number of inner rows that each of them matches.
    unique: Option<(f64, f64)>,
    /// True for the outer side and for the inner side when it is the unique rows of the inner side of a semi join, as `RELATION_WAS_MADE_UNIQUE`.
    made: (bool, bool),
    /// `has_indexed_join_quals`: true when the inner side is a parameterized scan whose index tests each condition of the join.
    indexed: bool,
    stats: &'a Stats,
}

/// `innerrel_is_unique` and `compute_semi_anti_join_factors`: when the inner side is one table with a unique index that is immediate, has no predicate and has each of its key columns in an `=` condition of the join with a value of the outer side, the fraction of the outer rows that have a match and the number of inner rows that each of them matches. A join with no other condition is a join of the inner rows with no condition, so that the fraction is the selectivity of the conditions of the join.
fn semifactors(
    outer: &BTreeSet<usize>,
    inner: &Rel<'_>,
    quals: &[Qual<'_>],
    stats: &Stats,
) -> Option<(f64, f64)> {
    let [r] = inner.rels.iter().copied().collect::<Vec<_>>()[..] else { return None };
    if quals.is_empty() {
        return None;
    }
    let indexes = stats.distinct.get(&r)?;
    let (clauses, _) = split(quals, outer, &inner.rels, exprs::can_merge);
    let columns: BTreeSet<i16> = clauses
        .iter()
        .filter_map(|c| plain_var(c.inner))
        .filter(|v| v.levels_up == 0 && v.relation == r)
        .map(|v| v.attnum)
        .collect();
    if !indexes.iter().any(|keys| keys.iter().all(|k| columns.contains(k))) {
        return None;
    }
    let selectivity: f64 = quals.iter().map(|q| join_selectivity(q, stats)).product();
    Some((selectivity, inner.node.cost.rows.max(1.0)))
}

/// A key of a merge join: the index of its clause in the clauses of `split`, and the directions of the sort.
type MergeKey = (usize, bool, bool);

/// A kind of join for an order of the sides, with its cost.
#[derive(Clone, Debug)]
struct Candidate<'q> {
    method: Method,
    join: JoinKind,
    swapped: bool,
    startup: f64,
    total: f64,
    /// For a merge join, true when the inner side needs a `Material` node; for a nested loop, true when it gets one.
    materialize: bool,
    /// For a merge join, true when each side needs a `Sort`.
    sort: (bool, bool),
    /// For a merge join, its keys.
    keys: Vec<MergeKey>,
    /// The plans of the outer side and of the inner side that the join reads, as indexes into the plans of each side, where the plan with the lowest total cost is 0.
    paths: (usize, usize),
    /// `inner_unique`: each outer row matches one inner row at most.
    unique: bool,
    /// For a nested loop with a parameterized scan as its inner side, the place of the scan in the parameterized scans of the inner side.
    param: Option<usize>,
    /// `disabled_nodes`: the disabled nodes of the join and of its two sides.
    disabled: u32,
    /// For a nested loop, the `Memoize` node over its inner side.
    memo: Option<Memo<'q>>,
}

impl Candidate<'_> {
    /// True when a setting turns off the method of the join, as `initial_cost_nestloop`, `initial_cost_mergejoin` and `initial_cost_hashjoin` read the strategies of the join.
    fn method_disabled(&self) -> bool {
        let enable = crate::enable();
        match self.method {
            Method::Nested => !enable.nestloop,
            Method::Merge => !enable.mergejoin,
            Method::Hash => !enable.hashjoin,
        }
    }

    /// The disabled nodes of the join and of the sorts of its sides, without the nodes of the sides.
    fn own_disabled(&self) -> u32 {
        let sorts = u32::from(self.sort.0) + u32::from(self.sort.1);
        u32::from(self.method_disabled()) + if crate::enable().sort { 0 } else { sorts }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Nested,
    Merge,
    Hash,
}

/// The conditions of a join that a kind of join tests itself, with the outer value on the left, and the other conditions.
fn split<'a, 'q>(
    quals: &'a [Qual<'q>],
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> (Vec<Clause<'a, 'q>>, Vec<&'a Qual<'q>>) {
    let mut clauses = Vec::new();
    let mut other = Vec::new();
    for qual in quals {
        match clause(qual, outer, inner, usable) {
            Some(c) => clauses.push(((qual.class.is_none(), qual.class, qual.order), c)),
            None => other.push(qual),
        }
    }
    // `build_joinrel_restrictlist`: the conditions of the equivalence classes come first, in the order of the classes, then the other conditions in the order of `deconstruct_jointree`.
    clauses.sort_by_key(|(key, _)| *key);
    (clauses.into_iter().map(|(_, c)| c).collect(), other)
}

/// The number of rows of the join that pass the conditions, as `approx_tuple_count`.
fn tuple_count(input: &Input<'_, '_>, clauses: &[Clause<'_, '_>]) -> f64 {
    let mut selectivity = 1.0;
    for c in clauses {
        selectivity *= input.stats.eqjoinsel(c.outer, c.inner);
    }
    cost::clamp_rows(input.outer.node.cost.rows * input.inner.node.cost.rows * selectivity)
}

/// `estimate_hash_bucket_stats`: the fraction of the inner rows in a bucket of all the batches for a key of a hash join, and the frequency of the most common value of the key, or 0 when it is not known. A column with a histogram and no most common values is unique. The bucket has 0.1 of the rows, or the frequency when it is more, when the number of the distinct values is the default. The conditions of the relation take away values in the same fraction as rows, and the bucket has at least the frequency.
fn bucket_stats(key: &Expr, virtual_buckets: f64, stats: &Stats) -> (f64, f64) {
    let statistic = column_statistic(key, stats);
    let (nd, default, rel) = numdistinct(key, stats);
    let mcv_freq = match statistic {
        Some(s) => match s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV) {
            Some(mcv) => {
                mcv.numbers.as_deref().and_then(|n| n.first()).map_or(0.0, |&n| f64::from(n))
            }
            None if s.slots.iter().any(|slot| slot.kind == STATISTIC_KIND_HISTOGRAM) => match rel {
                Some((tuples, _)) if tuples > 0.0 => 1.0 / tuples,
                _ => 0.0,
            },
            None => 0.0,
        },
        None => 0.0,
    };
    if default {
        return (mcv_freq.max(0.1), mcv_freq);
    }
    let nd = match rel {
        Some((tuples, rows)) if tuples > 0.0 => cost::clamp_rows(nd * rows / tuples),
        _ => nd,
    };
    let estfract = if nd > virtual_buckets { 1.0 / virtual_buckets } else { 1.0 / nd };
    (estfract.max(mcv_freq), mcv_freq)
}

/// The nested loops of a join, with their costs as `initial_cost_nestloop` and `final_cost_nestloop`: with the plain inner side, and then with a `Material` node over the inner side, as `match_unsorted_outer` makes them. The inner side gets no `Material` node when it keeps its rows already, reads the outer side or `enable_material` is off.
fn nested_loops<'q>(input: &Input<'_, 'q>, join: JoinKind, lateral: bool) -> Vec<Candidate<'q>> {
    let mut out = Vec::new();
    let mut materialize = vec![false];
    if !lateral && !keeps_rows(&input.inner.node.kind) && crate::enable().material {
        materialize.push(true);
    }
    for materialize in materialize {
        let wrapped = materialize.then(|| material_costs(input.inner.node.cost));
        let (startup, total) = nested_cost(input, wrapped);
        out.push(Candidate {
            method: Method::Nested,
            join,
            swapped: false,
            startup,
            total,
            materialize,
            sort: (false, false),
            keys: Vec::new(),
            paths: (0, 0),
            unique: false,
            param: None,
            disabled: 0,
            memo: None,
        });
    }
    out
}

/// The nested loop with the parameterized scan `param` as its inner side, as `match_unsorted_outer` makes it with each plan of `cheapest_parameterized_paths`, or with a `Memoize` node over the scan. The join does not test the conditions that the scan tests, as `create_nestloop_path`. A `Memoize` node is not an index scan for `has_indexed_join_quals`.
fn param_loop<'q>(
    input: &Input<'_, 'q>,
    join: JoinKind,
    param: &Param<'q>,
    i: usize,
    (quals, filter): (&[Qual<'q>], &[Qual<'q>]),
    memo: Option<Memo<'q>>,
) -> Candidate<'q> {
    let left = |list: &[Qual<'q>]| -> Vec<Qual<'q>> {
        let enforced = |q: &Qual<'q>| param.enforced.iter().any(|e| e.expr.same(&q.expr));
        list.iter().filter(|q| !enforced(q)).cloned().collect()
    };
    let (quals, filter) = (left(quals), left(filter));
    let (filter_startup, filter_cost) = input.stats.qual_costs(filter.iter().map(|q| &*q.expr));
    // `has_indexed_join_quals`: a condition that runs once is a condition of the join of all the relations, which the index does not test.
    let gated = !input.stats.gated.is_empty()
        && input.outer.rels.union(&param.rel.rels).count() == input.stats.gated.len();
    let indexed =
        memo.is_none() && param.indexed && quals.is_empty() && filter.is_empty() && !gated;
    let input = Input {
        inner: &param.rel,
        quals: &quals,
        filter_startup,
        filter_cost,
        filtered: !filter.is_empty(),
        indexed,
        ..*input
    };
    let wrapped = memo.as_ref().map(|m| (memoize::path_cost(param.rel.node.cost), m.rescan));
    let (startup, total) = nested_cost(&input, wrapped);
    Candidate {
        method: Method::Nested,
        join,
        swapped: false,
        startup,
        total,
        materialize: false,
        sort: (false, false),
        keys: Vec::new(),
        paths: (0, 0),
        unique: false,
        param: Some(i),
        disabled: 0,
        memo,
    }
}

/// The costs of the inner side of a nested loop with a `Material` node over it, as `cost_material`, with the costs of a scan again.
fn material_costs(i: Cost) -> (Cost, (f64, f64)) {
    let spill = cost::spill(i.rows, i.width);
    let total = i.total + 2.0 * cost::CPU_OPERATOR * i.rows + spill;
    (Cost { total, ..i }, (0.0, cost::CPU_OPERATOR * i.rows + spill))
}

/// The start and the total cost of a nested loop, with the inner side that the input has or, with `wrapped`, with the costs of a node over it and the costs of a scan of that node again.
fn nested_cost(input: &Input<'_, '_>, wrapped: Option<(Cost, (f64, f64))>) -> (f64, f64) {
    let o = input.outer.node.cost;
    let (inner, rescan) =
        wrapped.unwrap_or_else(|| (input.inner.node.cost, rescan(&input.inner.node)));
    let startup = o.startup + inner.startup;
    let mut run = o.total - o.startup;
    if o.rows > 1.0 {
        run += (o.rows - 1.0) * rescan.0;
    }
    let (inner_run, rescan_run) = (inner.total - inner.startup, rescan.1 - rescan.0);
    // `final_cost_nestloop` counts a side with no rows, which is dummy, as one row.
    let (outer_rows, inner_rows) = (nonzero(o.rows), nonzero(inner.rows));
    let tuples = match input.unique {
        // The scan of the inner side stops at the first match. An outer row with no match scans all the inner rows, and the first scan costs all of its run cost once.
        // With an index that tests each condition of the join, an outer row with no match costs a scan of one inner row.
        Some((fraction, matches)) if input.indexed => {
            let matched = (outer_rows * fraction).round_ties_even();
            let unmatched = outer_rows - matched;
            let scan_fraction = 2.0 / (matches + 1.0);
            run += inner_run * scan_fraction;
            if matched > 1.0 {
                run += (matched - 1.0) * rescan_run * scan_fraction;
            }
            run += unmatched * rescan_run / inner_rows;
            matched * inner_rows * scan_fraction
        }
        Some((fraction, matches)) => {
            let mut matched = (outer_rows * fraction).round_ties_even();
            let mut unmatched = outer_rows - matched;
            let scan_fraction = 2.0 / (matches + 1.0);
            let tuples = matched * inner_rows * scan_fraction + unmatched * inner_rows;
            run += inner_run;
            if unmatched >= 1.0 {
                unmatched -= 1.0;
            } else {
                matched -= 1.0;
            }
            if matched > 0.0 {
                run += matched * rescan_run * scan_fraction;
            }
            if unmatched > 0.0 {
                run += unmatched * rescan_run;
            }
            tuples
        }
        None => {
            run += inner_run;
            if o.rows > 1.0 {
                run += (o.rows - 1.0) * rescan_run;
            }
            outer_rows * inner_rows
        }
    };
    // `final_cost_nestloop`: the start cost of the conditions counts in the start cost of the join.
    let (qual_startup, qual_cost) = input.stats.qual_costs(input.quals.iter().map(|q| &*q.expr));
    let startup = startup + qual_startup + input.filter_startup;
    let per_tuple = cost::CPU_TUPLE + qual_cost + input.filter_cost;
    run += per_tuple * tuples;
    (startup, startup + run)
}

/// A number of rows, or 1 for no rows.
fn nonzero(rows: f64) -> f64 {
    if rows <= 0.0 { 1.0 } else { rows }
}

/// The costs of a hash join with a hash table of the inner side, as `initial_cost_hashjoin` and `final_cost_hashjoin`.
fn hash_join<'q>(input: &Input<'_, 'q>, join: JoinKind) -> Option<Candidate<'q>> {
    let (clauses, other) =
        split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_hash);
    if clauses.is_empty() {
        return None;
    }
    let (o, i) = (input.outer.node.cost, input.inner.node.cost);
    let n = clauses.len() as f64;
    let mut startup = o.startup + i.total + (cost::CPU_OPERATOR * n + cost::CPU_TUPLE) * i.rows;
    let mut run = o.total - o.startup + cost::CPU_OPERATOR * n * o.rows;
    let (buckets, batches) = cost::hash_table(i.rows, i.width);
    if batches > 1.0 {
        // A join in batches writes the inner rows to disk and reads them again, and writes and reads the outer rows of the later batches.
        let (outer_pages, inner_pages) =
            (cost::page_count(o.rows, o.width), cost::page_count(i.rows, i.width));
        startup += cost::SEQ_PAGE * inner_pages;
        run += cost::SEQ_PAGE * (inner_pages + 2.0 * outer_pages);
    }
    let virtual_buckets = buckets * batches;
    // The unique rows of the inner side of a semi join fill the buckets evenly, and no value has more than one row.
    let (bucket, mcv_freq) = if input.made.1 {
        (1.0 / virtual_buckets, 0.0)
    } else {
        clauses.iter().map(|c| bucket_stats(c.inner, virtual_buckets, input.stats)).fold(
            (1.0_f64, 1.0_f64),
            |(bucket, freq), (this_bucket, this_freq)| {
                // A frequency of 0 is not known.
                let freq = if this_freq > 0.0 && freq > this_freq { this_freq } else { freq };
                (bucket.min(this_bucket), freq)
            },
        )
    };
    // The most common value of the inner side has the fraction `mcv_freq` of its rows, all of them with no statistics. The bucket of that value must fit in `hash_mem`, because more batches cannot split it. A hash join that does not fit costs `disable_cost` more, so that the planner uses it only when it has no other plan.
    if !input.made.1
        && cost::byte_size(cost::clamp_rows(i.rows * mcv_freq), i.width) > cost::hash_mem()
    {
        startup += cost::DISABLE;
    }
    // The conditions of the hash cost as `cost_qual_eval` finds them for each row that a bucket has.
    let qual_cost = cost::qual_cost(clauses.iter().map(|c| &*c.qual.expr));
    let tuples = match input.unique {
        // The scan of a bucket stops at the first match. An outer row with no match scans a bucket of the average size, and most of its rows fail the hash values first.
        Some((fraction, matches)) => {
            let matched = (o.rows * fraction).round_ties_even();
            let scan_fraction = 2.0 / (matches + 1.0);
            run += qual_cost * matched * cost::clamp_rows(i.rows * bucket * scan_fraction) * 0.5;
            run +=
                qual_cost * (o.rows - matched) * cost::clamp_rows(i.rows / virtual_buckets) * 0.05;
            if join == JoinKind::Anti { o.rows - matched } else { matched }
        }
        None => {
            run += qual_cost * o.rows * cost::clamp_rows(i.rows * bucket) * 0.5;
            tuple_count(input, &clauses)
        }
    };
    // The start cost of the conditions that are not the clauses of the join counts in the start cost of the join.
    let (qual_startup, qual_cost) = input.stats.qual_costs(other.iter().map(|q| &*q.expr));
    let startup = startup + qual_startup + input.filter_startup;
    let per_tuple = cost::CPU_TUPLE + qual_cost + input.filter_cost;
    run += per_tuple * tuples;
    Some(Candidate {
        method: Method::Hash,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize: false,
        sort: (false, false),
        keys: Vec::new(),
        paths: (0, 0),
        unique: false,
        param: None,
        disabled: 0,
        memo: None,
    })
}

/// True when the order of the rows of a plan is the order of the keys of a merge join, from the first key. `value` gives the value of the side for a clause.
fn ordered<'e>(order: &[OrderKey], keys: &[MergeKey], value: impl Fn(usize) -> &'e Expr) -> bool {
    keys.len() <= order.len()
        && keys
            .iter()
            .zip(order)
            .all(|(&(c, desc, nulls_first), k)| k.sorts(value(c), desc, nulls_first))
}

/// `select_outer_pathkeys_for_merge`: the order of the keys of a merge join that sorts its sides. The keys of the query come first, in its directions, when the conditions have all of them. The condition of an outer join has only the value of its outer side, and only when that side is not the side that gets nulls. When the conditions are all keys of the query, they are the keys. The other keys follow in ascending order, first the keys with more expressions of relations outside the join that a condition makes equal to them.
fn merge_order(input: &Input<'_, '_>, join: JoinKind, clauses: &[Clause<'_, '_>]) -> Vec<MergeKey> {
    let has = |c: &Clause<'_, '_>, e: &Expr| match join {
        JoinKind::Inner | JoinKind::Semi => c.outer.same(e) || c.inner.same(e),
        JoinKind::Left | JoinKind::Anti => c.outer.same(e),
        JoinKind::Right | JoinKind::Full | JoinKind::RightSemi | JoinKind::RightAnti => false,
    };
    let mut used = vec![false; clauses.len()];
    let mut out = Vec::new();
    let query = &input.stats.query;
    let matches = query.iter().take_while(|(e, _, _)| clauses.iter().any(|c| has(c, e))).count();
    if !query.is_empty() && (matches == query.len() || matches == clauses.len()) {
        for (e, desc, nulls_first) in &query[..matches] {
            if let Some(i) = (0..clauses.len()).find(|&i| !used[i] && has(&clauses[i], e)) {
                used[i] = true;
                out.push((i, *desc, *nulls_first));
            }
        }
        if matches < query.len() {
            return out;
        }
    }
    let mut rels = input.outer.rels.clone();
    rels.extend(input.inner.rels.iter().copied());
    let score = |c: &Clause<'_, '_>| {
        let mut partners: Vec<&Expr> = Vec::new();
        for sides in &input.stats.joins {
            for ((e, _), (other, other_rels)) in sides.iter().zip(sides.iter().rev()) {
                if has(c, e)
                    && other_rels.is_disjoint(&rels)
                    && !partners.iter().any(|p| p.same(other))
                {
                    partners.push(other);
                }
            }
        }
        partners.len()
    };
    let scores: Vec<usize> = clauses.iter().map(score).collect();
    while let Some(i) =
        (0..clauses.len()).filter(|&i| !used[i]).fold(None, |best: Option<usize>, i| match best {
            Some(b) if scores[b] >= scores[i] => Some(b),
            _ => Some(i),
        })
    {
        used[i] = true;
        out.push((i, false, false));
    }
    out
}

/// `find_mergeclauses_for_outer_pathkeys`: the keys of a merge join in the order of the rows of the outer side, from the clauses whose values are in the first keys of that order. The other clauses are conditions of the join. With `all`, the order must have the value of each clause.
fn outer_order(order: &[OrderKey], clauses: &[Clause<'_, '_>], all: bool) -> Option<Vec<MergeKey>> {
    let mut used = vec![false; clauses.len()];
    let mut out = Vec::new();
    for k in order {
        let before = out.len();
        for (i, c) in clauses.iter().enumerate() {
            if !used[i] && k.sorts(c.outer, k.desc, k.nulls_first) {
                used[i] = true;
                out.push((i, k.desc, k.nulls_first));
            }
        }
        if out.len() == before {
            break;
        }
    }
    (!out.is_empty() && (!all || out.len() == clauses.len())).then_some(out)
}

/// `sort_inner_and_outer`: the merge join of the cheapest plans of the two sides in the order of `merge_order`.
fn sorted_merge<'q>(input: &Input<'_, 'q>, join: JoinKind) -> Option<Candidate<'q>> {
    let (clauses, _) = split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return None;
    }
    let keys = merge_order(input, join, &clauses);
    merge_join(input, join, keys)
}

/// `generate_mergejoin_paths`: the merge joins of a plan of the outer side in the order of its rows, with the clauses whose values are in the first keys of that order. The cheapest plan of the inner side gets a `Sort` when it is not in that order. Then, for the keys and for each shorter list of their first keys, the inner plans in that order with the lowest total cost and with the lowest start cost are inner sides too, when they cost less than the inner sides of the longer lists. A right join, a right anti join and a full join use all their clauses.
fn presorted_merges<'q>(
    input: &Input<'_, 'q>,
    inners: &[Rel<'q>],
    join: JoinKind,
) -> Vec<Candidate<'q>> {
    let (clauses, _) = split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return Vec::new();
    }
    let all = matches!(join, JoinKind::Right | JoinKind::RightAnti | JoinKind::Full);
    let Some(keys) = outer_order(&input.outer.order, &clauses, all) else { return Vec::new() };
    let mut out: Vec<Candidate<'q>> = merge_join(input, join, keys.clone()).into_iter().collect();
    let costs = |j: usize| (inners[j].node.cost.startup, inners[j].node.cost.total);
    let by = |a: usize, b: usize, total: bool| {
        let ((sa, ta), (sb, tb)) = (costs(a), costs(b));
        if total {
            ta.total_cmp(&tb).then(sa.total_cmp(&sb))
        } else {
            sa.total_cmp(&sb).then(ta.total_cmp(&tb))
        }
    };
    // The cheapest inner plan needs no `Sort` when it is in the order of all the keys, and then a sorted plan must cost less.
    let presorted = ordered(&inners[0].order, &keys, |c| clauses[c].inner);
    let (mut total, mut startup) = if presorted { (Some(0), Some(0)) } else { (None, None) };
    for count in (1..=keys.len()).rev() {
        let trial = &keys[..count];
        let sorted: Vec<usize> = (0..inners.len())
            .filter(|&j| ordered(&inners[j].order, trial, |c| clauses[c].inner))
            .collect();
        // `get_cheapest_path_for_pathkeys` and `compare_path_costs`: the first plan with the lowest cost.
        let best = |total: bool| sorted.iter().copied().min_by(|&a, &b| by(a, b, total));
        let mut each = Vec::new();
        if let Some(j) = best(true)
            && total.is_none_or(|t| by(j, t, true).is_lt())
        {
            each.push(j);
            total = Some(j);
        }
        if let Some(j) = best(false)
            && startup.is_none_or(|t| by(j, t, false).is_lt())
        {
            if total != Some(j) {
                each.push(j);
            }
            startup = Some(j);
        }
        for j in each {
            let input = Input { inner: &inners[j], ..*input };
            out.extend(
                merge_join(&input, join, trial.to_vec()).map(|c| Candidate { paths: (0, j), ..c }),
            );
        }
        if all {
            break;
        }
    }
    out
}

/// The costs of a merge join of the two sides in the order of the keys, as `initial_cost_mergejoin` and `final_cost_mergejoin`. A side that is not in that order gets a `Sort`.
fn merge_join<'q>(
    input: &Input<'_, 'q>,
    join: JoinKind,
    keys: Vec<MergeKey>,
) -> Option<Candidate<'q>> {
    let (clauses, other) =
        split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    // `select_mergejoin_clauses`: a right join, a right anti join and a full join are merge joins only when each of their clauses is a merge clause.
    let all = matches!(join, JoinKind::Right | JoinKind::RightAnti | JoinKind::Full);
    if clauses.is_empty() || all && !other.is_empty() {
        return None;
    }
    let other = merge_other(input.quals, &input.outer.rels, &input.inner.rels, &keys);
    let used: Vec<Clause<'_, '_>> = keys.iter().map(|&(c, _, _)| clauses[c]).collect();
    let sort_outer = !ordered(&input.outer.order, &keys, |c| clauses[c].outer);
    let sort_inner = !ordered(&input.inner.order, &keys, |c| clauses[c].inner);
    let side = |rel: &Rel<'_>, sorted: bool| {
        let c = rel.node.cost;
        if sorted {
            cost::sort((c.startup, c.total), c.rows, c.width, None)
        } else {
            (c.startup, c.total)
        }
    };
    let (o, i) = (side(input.outer, sort_outer), side(input.inner, sort_inner));
    let (outer_path_rows, inner_path_rows) =
        (nonzero(input.outer.node.cost.rows), nonzero(input.inner.node.cost.rows));
    // The first clause gives the parts of the sides that the join reads. The outer side of a left join or an anti join, and the inner side of a right join, are read to the end.
    let (mut outer_sel, mut inner_sel) = match keys.first() {
        Some(&(c, desc, nulls_first)) if join != JoinKind::Full => {
            let clause = &clauses[c];
            input.stats.scansel(clause.outer, clause.inner, desc, nulls_first)
        }
        _ => (scansel::ALL, scansel::ALL),
    };
    if matches!(join, JoinKind::Left | JoinKind::Anti) {
        outer_sel = scansel::ALL;
    } else if matches!(join, JoinKind::Right | JoinKind::RightAnti) {
        inner_sel = scansel::ALL;
    }
    let outer_skip = (outer_path_rows * outer_sel.start).round_ties_even();
    let inner_skip = (inner_path_rows * inner_sel.start).round_ties_even();
    let outer_rows = cost::clamp_rows(outer_path_rows * outer_sel.end);
    let inner_rows = cost::clamp_rows(inner_path_rows * inner_sel.end);
    // The fractions follow the rounding of the rows, which matters for a side with few rows.
    let (outer_start, outer_end) = (outer_skip / outer_path_rows, outer_rows / outer_path_rows);
    let (inner_start, inner_end) = (inner_skip / inner_path_rows, inner_rows / inner_path_rows);
    let tuples = tuple_count(input, &used);
    // `skip_mark_restore`: when the inner side is unique and the join has no other condition, the join never goes back to an inner row. With the unique rows of a semi join as the outer side, no outer row has the value of the row before it.
    let skip_mark_restore = input.unique.is_some() && other.is_empty() && !input.filtered;
    let rescanned =
        if skip_mark_restore || input.made.0 { 0.0 } else { (tuples - inner_path_rows).max(0.0) };
    let ratio = 1.0 + rescanned / inner_rows;
    let inner_run = (i.1 - i.0) * (inner_end - inner_start);
    let mut startup = o.0 + (o.1 - o.0) * outer_start + i.0 + (i.1 - i.0) * inner_start;
    let mut run = (o.1 - o.0) * (outer_end - outer_start);
    let material = inner_run + cost::CPU_OPERATOR * inner_rows * ratio;
    let bare = inner_run * ratio;
    // The inner side needs a `Material` node when it is cheaper, when its node cannot go back to a row that it gave, as `ExecSupportsMarkRestore`, or when its sort does not fit in `work_mem`.
    let inner_cost = input.inner.node.cost;
    // `enable_material` and `enable_mergejoin` allow a `Material` node that is not needed, as `PGS_MERGEJOIN_MATERIALIZE`.
    let enable = crate::enable();
    let allowed = enable.material && enable.mergejoin;
    let spills = cost::byte_size(inner_cost.rows, inner_cost.width) > cost::work_mem();
    let materialize = !skip_mark_restore
        && (allowed && material < bare
            || !sort_inner
                && !matches!(
                    input.inner.node.kind,
                    Kind::Sort(_)
                        | Kind::Materialize
                        | Kind::Result { .. }
                        | Kind::IndexScan { .. }
                )
            || allowed && sort_inner && spills);
    run += if materialize { material } else { bare };
    let merge_qual = cost::qual_cost(used.iter().map(|c| &*c.qual.expr));
    startup += merge_qual * (outer_skip + inner_skip * ratio);
    run += merge_qual * ((outer_rows - outer_skip) + (inner_rows - inner_skip) * ratio);
    // The start cost of the conditions that are not the clauses of the join counts in the start cost of the join.
    let (qual_startup, qual_cost) = input.stats.qual_costs(other.iter().map(|q| &*q.expr));
    let startup = startup + qual_startup + input.filter_startup;
    let per_tuple = cost::CPU_TUPLE + qual_cost + input.filter_cost;
    run += per_tuple * tuples;
    Some(Candidate {
        method: Method::Merge,
        join,
        swapped: false,
        startup,
        total: startup + run,
        materialize,
        sort: (sort_outer, sort_inner),
        keys,
        paths: (0, 0),
        unique: false,
        param: None,
        disabled: 0,
        memo: None,
    })
}

/// The sort keys of the values of the clauses of a merge join, with the directions of the keys of the join.
fn merge_keys<'q>(values: Vec<(Expr, bool, bool)>) -> Vec<SortKey<'q>> {
    values
        .into_iter()
        .map(|(v, desc, nulls_first)| SortKey {
            item: Item { expr: Cow::Owned(v), below: true },
            desc,
            using: None,
            nulls_first,
            collation: 0,
        })
        .collect()
}

/// The conditions of a merge join that are not its merge clauses, in the order of the conditions.
fn merge_other<'a, 'q>(
    quals: &'a [Qual<'q>],
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    keys: &[MergeKey],
) -> Vec<&'a Qual<'q>> {
    let mut index = 0;
    let mut other = Vec::new();
    for qual in quals {
        if clause(qual, outer, inner, exprs::can_merge).is_some() {
            index += 1;
            if keys.iter().any(|&(c, _, _)| c == index - 1) {
                continue;
            }
        }
        other.push(qual);
    }
    other
}

/// The clauses of a merge join in the order of its keys, each with the directions of its key.
fn merge_clauses<'a, 'q>(
    clauses: Vec<Clause<'a, 'q>>,
    keys: &[MergeKey],
) -> Vec<(Clause<'a, 'q>, bool, bool)> {
    let mut slots: Vec<Option<Clause<'a, 'q>>> = clauses.into_iter().map(Some).collect();
    keys.iter()
        .filter_map(|&(c, desc, nulls_first)| Some((slots[c].take()?, desc, nulls_first)))
        .collect()
}

impl<'q> Candidate<'q> {
    /// The order of the rows of the join, as `build_join_pathkeys`: the order of the outer side for a nested loop, the order of the values of the conditions for a merge join that is not a right join, a right anti join or a full join, and no order for a hash join. A merge join whose outer side needs no sort keeps the rest of the order of the outer side after the keys of the join, as `match_unsorted_outer` gives the order of the outer path.
    fn order(&self, outer: &Rel<'q>, inner: &Rel<'q>, quals: &[Qual<'q>]) -> Vec<OrderKey> {
        match self.method {
            Method::Nested => outer.order.clone(),
            Method::Merge
                if !matches!(self.join, JoinKind::Right | JoinKind::RightAnti | JoinKind::Full) =>
            {
                let clauses = split(quals, &outer.rels, &inner.rels, exprs::can_merge).0;
                let mut order: Vec<OrderKey> = merge_clauses(clauses, &self.keys)
                    .into_iter()
                    .map(|(c, desc, nulls_first)| OrderKey {
                        exprs: vec![c.outer.clone(), c.inner.clone()],
                        desc,
                        nulls_first,
                    })
                    .collect();
                if !self.sort.0 {
                    order.extend(outer.order.iter().skip(order.len()).cloned());
                }
                order
            }
            Method::Merge | Method::Hash => Vec::new(),
        }
    }

    /// The plan of the join.
    fn make(
        &self,
        scope: usize,
        outer: Rel<'q>,
        inner: Rel<'q>,
        quals: &[Qual<'q>],
        rows: f64,
    ) -> Rel<'q> {
        let order = self.order(&outer, &inner, quals);
        let node = match self.method {
            Method::Nested => {
                let inner_node = match &self.memo {
                    Some(memo) => memoize::node(inner.node, memo),
                    None if self.materialize => materialize(inner.node),
                    None => inner.node,
                };
                let join_filter = quals.iter().map(|q| q.item()).collect();
                Node::new(
                    Kind::NestedLoop { join: self.join, join_filter },
                    scope,
                    vec![outer.node, inner_node],
                )
            }
            Method::Hash => {
                let (clauses, other) = split(quals, &outer.rels, &inner.rels, exprs::can_hash);
                let c = inner.node.cost;
                let mut hash = Node::new(Kind::Hash, scope, vec![inner.node]);
                hash.cost = Cost { startup: c.total, ..c };
                let cond = clauses.into_iter().map(|c| c.item()).collect();
                let join_filter = other.iter().map(|q| q.item()).collect();
                Node::new(
                    Kind::HashJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer.node, hash],
                )
            }
            Method::Merge => {
                let clauses = split(quals, &outer.rels, &inner.rels, exprs::can_merge).0;
                let other = merge_other(quals, &outer.rels, &inner.rels, &self.keys);
                let clauses = merge_clauses(clauses, &self.keys);
                let outer_values =
                    clauses.iter().map(|(c, d, n)| (c.outer.clone(), *d, *n)).collect();
                let inner_values =
                    clauses.iter().map(|(c, d, n)| (c.inner.clone(), *d, *n)).collect();
                let outer_node = if self.sort.0 {
                    sort(outer.node, merge_keys(outer_values), None)
                } else {
                    outer.node
                };
                let mut inner_node = if self.sort.1 {
                    sort(inner.node, merge_keys(inner_values), None)
                } else {
                    inner.node
                };
                if self.materialize {
                    inner_node = merge_materialize(inner_node);
                }
                let cond = clauses.into_iter().map(|(c, _, _)| c.item()).collect();
                let join_filter = other.iter().map(|q| q.item()).collect();
                Node::new(
                    Kind::MergeJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer_node, inner_node],
                )
            }
        };
        let mut node = node;
        node.cost = Cost { startup: self.startup, total: self.total, rows, width: 0 };
        node.inner_unique = self.unique;
        node.disabled = self.method_disabled();
        Rel { node, rels: BTreeSet::new(), order }
    }
}

/// `remove_useless_groupby_columns`: true when the keys of the index decide the other columns of its table, as a unique index that is not deferrable and not partial, on columns that are `NOT NULL` or with `NULLS NOT DISTINCT`.
fn functional(relation: &Relation, index: &Index) -> bool {
    let not_null = |k: &i16| {
        usize::try_from(*k - 1)
            .ok()
            .and_then(|c| relation.columns.get(c))
            .is_some_and(|c| c.not_null)
    };
    index.unique
        && index.immediate
        && !index.partial
        && (index.nulls_not_distinct || index.keys.iter().all(not_null))
}

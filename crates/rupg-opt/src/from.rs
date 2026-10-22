//! The plan of `FROM` and `WHERE`: the scan of each relation, the joins and the place of each condition, as `query_planner`, `make_one_rel` and `create_scan_plan` make them.

mod classes;
pub(crate) mod paths;
mod search;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use rupg_analyze::{
    BoolOp, BoolTest, Expr, ExprKind, FromItem, JoinKind, Query, Relation, SortGroup, Var,
    expand_grouping_sets,
};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node, Replaced, SortKey, Strategy};
use rupg_types::{Value, oid};

use crate::{Builder, Level, Member, cost, exprs, upper};
use paths::Paths;

/// The plans of the rows of `FROM` that `add_path` keeps, each with the order of its rows.
pub(crate) type Plans<'q> = Vec<(Node<'q>, Vec<OrderKey>)>;

/// The estimates of the relations of a query level for the costs of the joins and of the nodes above them.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    /// The rows of each relation before and after its conditions, as `tuples` and `rows` of its `RelOptInfo`.
    rows: BTreeMap<usize, (f64, f64)>,
    /// Each column that the query reads, with its width and the relations of the places that read it, as `attr_needed`. The target list of the query is `usize::MAX`.
    needed: Vec<(Var, i32, BTreeSet<usize>)>,
    /// `sort_pathkeys`: the keys of `ORDER BY`, each with its directions.
    sort: Vec<(Expr, bool, bool)>,
    /// `group_pathkeys` and `distinct_pathkeys`: the keys of `GROUP BY` and of `DISTINCT`, in any order.
    groups: Vec<Vec<(Expr, bool, bool)>>,
    /// `query_pathkeys`: the keys of `GROUP BY`, or else of `DISTINCT`, or else of `ORDER BY`.
    query: Vec<(Expr, bool, bool)>,
    /// The two sides of each `=` condition of a join that a merge join can test, each with its relations.
    joins: Vec<[(Expr, BTreeSet<usize>); 2]>,
    /// `consider_startup`: true when the caller of the query or its `LIMIT` reads only a part of its rows, so that a plan with a lower start cost is a different plan.
    startup: bool,
    /// `isunique`: the relation and the column number of each column that has a unique index of one key column and no predicate, as `has_unique_index`.
    unique: BTreeSet<(usize, i16)>,
    /// The key columns of each unique index of each relation that is immediate and has no predicate, which `relation_has_unique_index_for` reads.
    distinct: BTreeMap<usize, Vec<Vec<i16>>>,
    /// The key columns of each unique index of each relation that shows that the other columns of a row are a function of the key columns, as `remove_useless_groupby_columns` chooses them: the index is immediate and has no predicate and no expression, and each key column is `NOT NULL` or the index has `NULLS NOT DISTINCT`.
    functional: BTreeMap<usize, Vec<Vec<i16>>>,
    /// The members of each equivalence class of the query level, each class with true when it has a constant.
    equal: Vec<(Vec<Expr>, bool)>,
    /// The columns of the members of each equivalence class whose conditions the joins make, each with the relations of the class. `generate_base_implied_equalities_no_const` adds the relations to the `attr_needed` of the columns.
    spans: Vec<(Expr, BTreeSet<usize>)>,
    /// The costs of the subqueries of the expressions of the level that run as a `SubPlan` for each row.
    subplans: Vec<cost::SubCost>,
}

impl Stats {
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

    /// The cost for each row of [`Stats::qual_costs`].
    pub(crate) fn qual_cost<'a>(&self, quals: impl IntoIterator<Item = &'a Expr>) -> f64 {
        self.qual_costs(quals).1
    }

    /// `order_qual_clauses`: the conditions in the order of their cost for each row, and in their order for the same cost.
    fn sort_quals(&self, quals: &mut [Qual<'_>]) {
        quals.sort_by(|a, b| self.qual_cost([&*a.expr]).total_cmp(&self.qual_cost([&*b.expr])));
    }

    /// `estimate_num_groups` with no statistics: the number of groups of rows by the values of the expressions. A `boolean` expression has 2 values, and each other expression has the values of its columns. Two columns that are equal or that an equivalence class makes equal count once. The columns of each relation have the product of their numbers of distinct values, at most a tenth of the tuples of the relation, or the largest number of one column when it is more. A relation with fewer rows after its conditions than its tuples has fewer values, as the formula of Yao. A column of a relation that the level does not know has 200 values.
    pub(crate) fn groups(&self, exprs: &[&Expr], input_rows: f64) -> f64 {
        let input_rows = cost::clamp_rows(input_rows);
        if exprs.is_empty() {
            return 1.0;
        }
        let mut numdistinct = 1.0;
        // Each column with its relation and its number of distinct values, as a `GroupVarInfo`.
        let mut infos: Vec<(Expr, usize, f64)> = Vec::new();
        for &expr in exprs {
            if expr.ty == oid::BOOL {
                numdistinct *= 2.0;
                continue;
            }
            let mut columns = Vec::new();
            exprs::columns_at(expr, 0, &mut columns);
            if columns.is_empty() && exprs::volatile(expr) {
                return input_rows;
            }
            for column in columns {
                self.add_group_var(&mut infos, column);
            }
        }
        while let Some((_, rel, _)) = infos.first() {
            let rel = *rel;
            let (mut reldistinct, mut relmax) = (1.0f64, 1.0f64);
            let mut count = 0;
            for (_, _, nd) in infos.iter().filter(|i| i.1 == rel) {
                reldistinct *= nd;
                relmax = relmax.max(*nd);
                count += 1;
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
        numdistinct.ceil().min(input_rows).max(1.0)
    }

    /// `add_unique_group_var`: adds a column to the columns of the groups, when it is not there. Of two columns of different relations that an equivalence class makes equal, the column with fewer distinct values stays.
    fn add_group_var(&self, infos: &mut Vec<(Expr, usize, f64)>, column: Expr) {
        let ExprKind::Var(var) = column.kind else { return };
        let (nd, _, _) = numdistinct(&column, self);
        let mut i = 0;
        while i < infos.len() {
            let (other, rel, other_nd) = &infos[i];
            if other.same(&column) {
                return;
            }
            if *rel != var.relation && self.known_equal(other, &column) {
                if *other_nd <= nd {
                    return;
                }
                infos.remove(i);
                continue;
            }
            i += 1;
        }
        infos.push((column, var.relation, nd));
    }

    /// `exprs_known_equal`: true when an equivalence class has the two expressions.
    fn known_equal(&self, a: &Expr, b: &Expr) -> bool {
        self.equal.iter().any(|(c, _)| c.iter().any(|m| m.same(a)) && c.iter().any(|m| m.same(b)))
    }

    /// `pathkey_is_redundant`: true when a key of a sort on the expression adds nothing after the keys on the expressions `before`. The expression is a constant, an equivalence class with a constant has it, or an earlier key is the expression or is in its class. A volatile expression has a class of its own.
    fn redundant(&self, expr: &Expr, before: &[&Expr]) -> bool {
        if exprs::volatile(expr) {
            return false;
        }
        if exprs::pseudo_constant(expr) {
            return true;
        }
        match self.equal.iter().find(|(members, _)| members.iter().any(|m| m.same(expr))) {
            Some((members, constant)) => {
                *constant || before.iter().any(|b| members.iter().any(|m| m.same(b)))
            }
            None => before.iter().any(|b| b.same(expr)),
        }
    }

    /// `make_pathkeys_for_sortclauses_extended` with `remove_redundant`: the items of a list of keys without the items whose keys are redundant, as `pathkey_is_redundant` finds them.
    pub(crate) fn canonical<T>(&self, items: Vec<T>, expr: impl Fn(&T) -> &Expr) -> Vec<T> {
        let mut out: Vec<T> = Vec::new();
        for item in items {
            let before: Vec<&Expr> = out.iter().map(&expr).collect();
            if !self.redundant(expr(&item), &before) {
                out.push(item);
            }
        }
        out
    }

    /// `remove_useless_groupby_columns`: for each expression of `GROUP BY`, true when the groups need it not, because the other columns of its relation in `GROUP BY` have all the key columns of a unique index of `functional`. The key columns must be fewer than the columns of the relation in `GROUP BY`, and the index with the fewest key columns wins. Only a column of the query level can go, and `GROUP BY` must have two items or more.
    pub(crate) fn surplus(&self, exprs: &[&Expr]) -> Vec<bool> {
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
            let best = indexes
                .iter()
                .filter(|keys| {
                    let keys: BTreeSet<i16> = keys.iter().copied().collect();
                    keys.is_subset(attnums) && keys.len() < attnums.len()
                })
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

    /// The width of the rows of a join of relations, as `build_joinrel_tlist` and `set_rel_width` find it: the columns of the relations that a place outside the join reads.
    fn width(&self, rels: &BTreeSet<usize>) -> i32 {
        self.needed
            .iter()
            .filter(|(v, _, places)| rels.contains(&v.relation) && !places.is_subset(rels))
            .map(|(_, width, _)| width)
            .sum()
    }

    /// `truncate_useless_pathkeys`: the first keys of the order of the rows of a join of relations that a node above can use. These are the first keys of `ORDER BY`, the first keys that are keys of `GROUP BY` or of `DISTINCT`, or the first keys that a merge join with another relation can use.
    fn useful(&self, rels: &BTreeSet<usize>, order: &[OrderKey]) -> Vec<OrderKey> {
        let has = |list: &[(Expr, bool, bool)], k: &OrderKey| {
            list.iter().any(|(e, desc, nulls_first)| k.sorts(e, *desc, *nulls_first))
        };
        let ordered = order
            .iter()
            .zip(&self.sort)
            .take_while(|(k, (e, desc, nulls_first))| k.sorts(e, *desc, *nulls_first))
            .count();
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
}

impl<'q> Qual<'q> {
    /// A condition of the query.
    fn of(expr: &'q Expr, rels: BTreeSet<usize>, outer: bool) -> Qual<'q> {
        Qual { expr: Cow::Borrowed(expr), rels, outer, order: None, divisors: Vec::new() }
    }

    /// A condition that the planner makes from an equivalence class.
    fn implied(expr: Expr, rels: BTreeSet<usize>) -> Qual<'q> {
        Qual { expr: Cow::Owned(expr), rels, outer: false, order: None, divisors: Vec::new() }
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
        let mut stats = Stats { equal, subplans: level.subplans.clone(), ..Stats::default() };
        needed(level, &mut stats);
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
                (vec![self.scan(level, relation, quals, once, &mut stats)], true)
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
            Tree::Base { relation, quals } => vec![self.scan(level, relation, quals, 0.0, stats)],
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
                search::search(level, rels, &pool, specials, found, stats)
            }
            Tree::Join(join) => {
                let JoinTree { kind, left, right, quals, filter, .. } = *join;
                let left = self.build(level, left, found, stats);
                let right = self.build(level, right, found, stats);
                join_rels(level, kind, left, right, quals, filter, stats)
            }
        }
    }

    /// The scan of a relation with its conditions, as `create_scan_plan`, with the costs of `cost_seqscan` and the other functions of `costsize.c` for each kind of scan. `once` is the cost of the conditions that read no relation, which the scan of the only relation of `FROM` has.
    fn scan(
        &mut self,
        level: &Level<'q>,
        r: usize,
        quals: Vec<Qual<'q>>,
        once: f64,
        stats: &mut Stats,
    ) -> Rel<'q> {
        let relation = &level.query.relations[r];
        let quals = order_quals(quals, stats);
        // `cost_seqscan` and the other scans: the start cost of the conditions, and their cost for each row.
        let (qual_startup, qual_cost) = stats.qual_costs(quals.iter().map(|q| &*q.expr));
        let qual_startup = qual_startup + once;
        let mut per_row = cost::CPU_TUPLE + qual_cost;
        let mut children = Vec::new();
        // `cost_subqueryscan`: the cost of the subquery as it is, to which the scan adds its own cost.
        let mut sub_total = None;
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
                (Kind::CteScan(r), 0.0, 0.0, rows)
            }
            _ => match (&relation.subquery, &relation.function, &relation.values) {
                (Some(sub), _, _) => {
                    let child = self.query(sub, Some(level.scope));
                    let c = child.cost;
                    children.push(child);
                    // `cost_subqueryscan`: a scan with no condition that gives the columns of its subquery as they are costs its subquery, because `setrefs.c` takes it out of the plan.
                    if quals.is_empty() && trivial_target(level.query, r, sub) {
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
                        stats.unique.insert((r, at));
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
                        return members_scan(level, r, &quals, members, stats);
                    }
                    for index in self.members.map(|m| m.indexes(relation.oid)).unwrap_or_default() {
                        if let ([key], true, false) =
                            (index.keys.as_slice(), index.unique, index.partial)
                            && *key > 0
                        {
                            stats.unique.insert((r, *key));
                        }
                        if index.unique && index.immediate && !index.partial {
                            let not_null = |k: &i16| {
                                usize::try_from(*k - 1)
                                    .ok()
                                    .and_then(|c| relation.columns.get(c))
                                    .is_some_and(|c| c.not_null)
                            };
                            if index.nulls_not_distinct || index.keys.iter().all(not_null) {
                                stats.functional.entry(r).or_default().push(index.keys.clone());
                            }
                            stats.distinct.entry(r).or_default().push(index.keys);
                        }
                    }
                    let (pages, tuples) = cost::table_size(relation);
                    match &relation.tablesample {
                        Some(sample) => {
                            let fraction = sample
                                .args
                                .first()
                                .and_then(number)
                                .map_or(0.1, |p| (p / 100.0).clamp(0.0, 1.0));
                            (Kind::SampleScan(r), 0.0, pages * cost::SEQ_PAGE, tuples * fraction)
                        }
                        None => (Kind::SeqScan(r), 0.0, pages * cost::SEQ_PAGE, tuples),
                    }
                }
            },
        };
        // `var_eq_const` and `var_eq_non_const`: `=` of a unique column and a value that the scan knows passes one row.
        let eq = |a: &Expr, b: &Expr| {
            [(a, b), (b, a)].into_iter().find_map(|(column, value)| {
                let var = plain_var(column)?;
                (var.levels_up == 0
                    && var.relation == r
                    && stats.unique.contains(&(r, var.attnum))
                    && exprs::relations(value).is_empty()
                    && tuples >= 1.0)
                    .then(|| 1.0 / tuples)
            })
        };
        let selectivity = cost::list_selectivity_by(quals.iter().map(|q| &*q.expr), &eq);
        let rows = cost::clamp_rows(tuples * selectivity);
        stats.rows.insert(r, (tuples, rows));
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
        Rel { node, rels: BTreeSet::from([r]), order: Vec::new() }
    }
}

/// `trivial_pathtarget` of `set_subquery_pathlist`: the columns of the relation `r` that the plan reads above the scan are each column of the subquery in order. The plan reads them in the order that they first come in the targets and in `HAVING`, as `build_base_rel_tlists` adds them, then in the conditions of the joins.
fn trivial_target(query: &Query, r: usize, sub: &Query) -> bool {
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

/// The parts of the conditions of the joins of an item of `FROM`, the joins below first.
fn join_conditions<'q>(item: &'q FromItem, out: &mut Vec<&'q Expr>) {
    let FromItem::Join(join) = item else { return };
    join_conditions(&join.left, out);
    join_conditions(&join.right, out);
    if let Some(on) = &join.on {
        exprs::and_parts(on, out);
    }
}

/// `query_is_distinct_for`: the sets of the columns of a subquery that it gives once each, as column numbers. The query gives each row once on the items of `DISTINCT`, or of `DISTINCT ON` with no function that gives a set. With no function that gives a set, it also gives each row once on the items of `GROUP BY`, or on no column when it makes one group with no `GROUP BY` or with one grouping set or `GROUP BY DISTINCT` of empty sets, and a set operation with no `ALL` gives each row once on all its columns.
fn distinct_keys(sub: &Query) -> Vec<Vec<i16>> {
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

/// `set_append_rel_size` and `create_append_plan`: the scans of the members of the inheritance tree of the relation `r`, under an `Append` when there are two or more, each with the conditions of the relation. With no member the relation is a `Result` with no rows, as `set_dummy_rel_pathlist`.
fn members_scan<'q>(
    level: &Level<'q>,
    r: usize,
    quals: &[Qual<'q>],
    members: Vec<Member>,
    stats: &mut Stats,
) -> Rel<'q> {
    let selectivity = cost::list_selectivity(quals.iter().map(|q| &*q.expr));
    let per_row = cost::CPU_TUPLE + stats.qual_cost(quals.iter().map(|q| &*q.expr));
    let (mut tuples, mut rows) = (0.0, 0.0);
    let mut children = Vec::with_capacity(members.len());
    for member in &members {
        let (pages, count) = cost::member_size(member);
        let mut node = Node::new(Kind::SeqScan(r), level.scope, Vec::new());
        node.member = Some(member.oid);
        node.filter = quals.iter().map(|q| q.item()).collect();
        let n = cost::clamp_rows(count * selectivity);
        let total = pages * cost::SEQ_PAGE + count * per_row;
        node.cost = Cost { startup: 0.0, total, rows: n, width: 0 };
        tuples += count;
        rows += n;
        children.push(node);
    }
    stats.rows.insert(r, (tuples, rows));
    let rels = BTreeSet::from([r]);
    let width = stats.width(&rels);
    for child in &mut children {
        child.cost.width = width;
    }
    let node = match children.len() {
        0 => dummy(level.scope, &rels, false),
        1 => children.remove(0),
        _ => {
            // `cost_append`: the start of the first member, and the costs of all the members with half of `cpu_tuple_cost` for each row.
            let startup = children[0].cost.startup;
            let total: f64 = children.iter().map(|c| c.cost.total).sum::<f64>()
                + cost::CPU_TUPLE * cost::APPEND_CPU * rows;
            let mut node = Node::new(Kind::Append, level.scope, children);
            node.cost = Cost { startup, total, rows, width };
            node
        }
    };
    Rel { node, rels, order: Vec::new() }
}

/// The columns that each place of a query level reads, as `build_base_rel_tlists`, `distribute_qual_to_rels` and `find_lateral_references` set `attr_needed`: the target list, the conditions of `WHERE` and of `ON`, and the lateral references.
fn needed(level: &Level<'_>, stats: &mut Stats) {
    let query = level.query;
    let relations = &query.relations;
    let top = BTreeSet::from([usize::MAX]);
    for target in &query.targets {
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
    stats.query = [&group, &distinct, &sort]
        .into_iter()
        .find(|keys| !keys.is_empty())
        .cloned()
        .unwrap_or_default();
    stats.sort = sort;
    stats.groups = vec![group, distinct];
    stats.startup = level.fraction > 0.0;
    for (r, relation) in relations.iter().enumerate() {
        for (expr, depth) in lateral_exprs(relation) {
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

/// True for a condition that PostgreSQL keeps in an equivalence class: an `=` that a merge join can use, with no volatile function, that is not a condition of an outer join.
fn equivalence(qual: &Qual<'_>) -> bool {
    !qual.outer
        && !exprs::volatile(&qual.expr)
        && exprs::binary(&qual.expr).is_some_and(|(op, _, _)| exprs::can_merge(op))
}

/// The conditions of a scan in the order of `order_qual_clauses`: the conditions that are not equivalences, then the equivalences, which the planner makes again, then by their cost for each row.
fn order_quals<'q>(quals: Vec<Qual<'q>>, stats: &Stats) -> Vec<Qual<'q>> {
    let (mut out, classes): (Vec<_>, Vec<_>) = quals.into_iter().partition(|q| !equivalence(q));
    out.extend(classes);
    stats.sort_quals(&mut out);
    out
}

/// A condition of a join that tests a value of the outer side and a value of the inner side with an operator, with the outer value on the left.
struct Clause<'q> {
    item: Item<'q>,
    outer: Expr,
    inner: Expr,
}

/// The condition with the value of `outer` on the left, when the condition compares a value of each side with an operator that `usable` accepts.
fn clause<'q>(
    qual: &Qual<'q>,
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> Option<Clause<'q>> {
    let (op, left, right) = exprs::binary(&qual.expr)?;
    if !usable(op) || exprs::volatile(&qual.expr) {
        return None;
    }
    let (l, r) = (exprs::relations(left), exprs::relations(right));
    if l.is_empty() || r.is_empty() {
        return None;
    }
    if l.is_subset(outer) && r.is_subset(inner) {
        return Some(Clause { item: qual.item(), outer: left.clone(), inner: right.clone() });
    }
    if l.is_subset(inner) && r.is_subset(outer) {
        let commuted = exprs::commute(&qual.expr)?;
        return Some(Clause {
            item: Item::owned(commuted),
            outer: right.clone(),
            inner: left.clone(),
        });
    }
    None
}

/// A column, or a column under a `RelabelType`, as `examine_variable` finds a `Var`.
fn plain_var(expr: &Expr) -> Option<Var> {
    match &expr.kind {
        ExprKind::Var(var) => Some(*var),
        ExprKind::Relabel(arg, _) => match arg.kind {
            ExprKind::Var(var) => Some(var),
            _ => None,
        },
        _ => None,
    }
}

/// The number of distinct values of an expression with no statistics, as `get_variable_numdistinct`, and the tuples and the rows of the relation when the expression reads one relation. A unique column has as many values as tuples, a `boolean` has 2 values, `tableoid` has 1 and the other system columns are unique. A small table has as many values as tuples, and the others have 200. The flag is true for the default.
fn numdistinct(expr: &Expr, stats: &Stats) -> (f64, bool, Option<(f64, f64)>) {
    let rels = exprs::relations(expr);
    let rel = match (rels.len(), rels.first()) {
        (1, Some(r)) => stats.rows.get(r).copied(),
        _ => None,
    };
    let unique = plain_var(expr)
        .is_some_and(|v| v.levels_up == 0 && stats.unique.contains(&(v.relation, v.attnum)));
    let known = if unique {
        // A unique column has as many values as the relation has tuples, whatever its type.
        rel.map(|(tuples, _)| tuples)
    } else if expr.ty == oid::BOOL {
        Some(2.0)
    } else if let ExprKind::Var(var) = expr.strip_implicit().kind
        && var.attnum < 0
    {
        // `tableoid` has one value in a relation, and the other system columns are unique.
        if var.attnum == -6 { Some(1.0) } else { rel.map(|(tuples, _)| tuples) }
    } else {
        None
    };
    let nd = match (known, rel) {
        (Some(nd), _) => (cost::clamp_rows(nd), false),
        (None, Some((tuples, _))) if tuples > 0.0 && tuples < cost::NUM_DISTINCT => {
            (cost::clamp_rows(tuples), false)
        }
        _ => (cost::NUM_DISTINCT, true),
    };
    (nd.0, nd.1, rel)
}

/// The number of distinct values of an expression with no statistics for `eqjoinsel_inner`, which does not clamp it to the rows of its relation after the conditions. The flag is true for the default.
fn distinct(expr: &Expr, stats: &Stats) -> (f64, bool) {
    let (nd, default, _) = numdistinct(expr, stats);
    (nd, default)
}

/// The selectivity of a condition of a join, as `clause_selectivity` finds it for each part of the condition. An `=` of a value of each side is `1 / max(nd1, nd2)`, as `eqjoinsel` with no statistics. An `=` of a unique column and a value that reads no relation, in a part of an `OR`, passes one tuple of the relation, as `var_eq_const`.
fn join_selectivity(qual: &Qual<'_>, stats: &Stats) -> f64 {
    let eq = |left: &Expr, right: &Expr| {
        if exprs::reads_columns(left) && exprs::reads_columns(right) {
            let (l, _) = distinct(left, stats);
            let (r, _) = distinct(right, stats);
            return Some(1.0 / l.max(r));
        }
        [(left, right), (right, left)].into_iter().find_map(|(column, value)| {
            let var = plain_var(column)?;
            let (tuples, _) = *stats.rows.get(&var.relation)?;
            (var.levels_up == 0
                && stats.unique.contains(&(var.relation, var.attnum))
                && exprs::relations(value).is_empty()
                && tuples >= 1.0)
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
        Kind::FunctionScan(_)
        | Kind::TableFunctionScan(_)
        | Kind::CteScan(_)
        | Kind::WorkTableScan(_) => (0.0, c.total - c.startup),
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
    node
}

/// True when the relations of a node read the relations of another side, as a lateral reference.
fn lateral(level: &Level<'_>, rels: &BTreeSet<usize>, other: &BTreeSet<usize>) -> bool {
    rels.iter().any(|r| !lateral_rels(level, *r).is_disjoint(other))
}

/// The relations of the query that the expressions of a relation of `FROM` read: its function, its `VALUES` lists or the columns of the query in its subquery.
pub(crate) fn lateral_rels(level: &Level<'_>, r: usize) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for expr in lateral_exprs(&level.query.relations[r]) {
        out.extend(exprs::relations_at(expr.0, expr.1));
    }
    out.remove(&r);
    out
}

/// The relations of the query below a node of the query, and the relations that a `Result` takes the place of.
pub(crate) fn node_rels(node: &Node<'_>) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    match &node.kind {
        Kind::SeqScan(r)
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

/// The expressions of a relation of `FROM` that can read the other relations of the query, each with its depth below the query.
pub(crate) fn lateral_exprs(relation: &Relation) -> Vec<(&Expr, usize)> {
    let mut out = Vec::new();
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
    for (candidate, keys) in choose(level, kind, &left, &right, &quals, &filter, stats, Part::Plain)
    {
        paths.add(candidate.startup, candidate.total, keys, candidate);
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
    for (candidate, keys) in choose(level, kind, &left, &right, &quals, &filter, stats, part) {
        paths.add(candidate.startup, candidate.total, keys, (candidate, false));
    }
    if let Some(unique) = &unique {
        let part = Part::Unique(fraction, true);
        let inner = JoinKind::Inner;
        for (candidate, keys) in choose(level, inner, &left, unique, &quals, &filter, stats, part) {
            paths.add(candidate.startup, candidate.total, keys, (candidate, true));
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
    let order: Vec<OrderKey> = keys
        .iter()
        .map(|&k| OrderKey { exprs: vec![k.clone()], desc: false, nulls_first: false })
        .collect();
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
                    })
                    .collect();
                input = sort(input, sort_keys, None);
            }
            let c = input.cost;
            let mut node = Node::new(Kind::Unique, input.scope, vec![input]);
            let total = c.total + cost::CPU_OPERATOR * c.rows * n;
            node.cost = Cost { startup: c.startup, total, rows: groups, width: c.width };
            let rel = Rel { node, rels: rel.rels.clone(), order: order.clone() };
            paths.add(c.startup, total, order.clone(), rel);
        }
    }
    if semi.hash {
        let input = project(cheapest.node.clone());
        let c = input.cost;
        let startup = c.total + cost::CPU_OPERATOR * n * c.rows;
        let total = startup + cost::CPU_TUPLE * groups;
        let items = keys.iter().map(|&k| Item::below(k)).collect();
        let kind = Kind::Aggregate { strategy: Strategy::Hashed, keys: items, sets: Vec::new() };
        // The target of the `Agg` is the target of the side without the new items, as `create_final_unique_paths` gives it.
        let width = cheapest.node.cost.width;
        let mut node = Node::new(kind, input.scope, vec![input]);
        node.cost = Cost { startup, total, rows: groups, width };
        let rel = Rel { node, rels: cheapest.rels.clone(), order: Vec::new() };
        paths.add(startup, total, Vec::new(), rel);
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
        let (nd1, default1) = distinct(outer_value, stats);
        let (mut nd2, mut default2, rel) = numdistinct(inner_value, stats);
        let inner_selectivity = 1.0 / nd1.max(nd2);
        if let Some((_, rows)) = rel
            && nd2 >= rows
        {
            (nd2, default2) = (rows, false);
        }
        if nd2 >= inner_rows {
            (nd2, default2) = (inner_rows, false);
        }
        let selectivity = if default1 || default2 {
            0.5
        } else if nd1 <= nd2 {
            1.0
        } else {
            nd2 / nd1
        };
        selectivity.min(inner_rows * inner_selectivity)
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
            stats,
        };
        let nested =
            matches!(join, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti);
        let merge = join != JoinKind::RightSemi;
        let mut candidates: Vec<Candidate<'q>> = Vec::new();
        if merge {
            candidates.extend(sorted_merge(&input, join));
        }
        // `match_unsorted_outer`: each plan of the outer side, with the cheapest plan of the inner side or with an inner plan in the order of the merge.
        for (p, outer) in outers.iter().enumerate() {
            let input = Input { outer, ..input };
            let mut each = Vec::new();
            if nested {
                each.extend(nested_loops(&input, join, lateral_right));
            }
            if merge {
                each.extend(presorted_merges(&input, inners, join));
            }
            candidates.extend(each.into_iter().map(|c| Candidate { paths: (p, c.paths.1), ..c }));
        }
        candidates.extend(hash_join(&input, join));
        for candidate in candidates {
            let (outer, inner) = (&outers[candidate.paths.0], &inners[candidate.paths.1]);
            let keys = stats.useful(&rels, &candidate.order(outer, inner, quals));
            out.push((Candidate { swapped, unique: inner_unique, ..candidate }, keys));
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
    let (outer, inner) = if best.swapped { (right, left) } else { (left, right) };
    let mut out = best.make(level.scope, outer, inner, quals, rows);
    out.node.filter = filter.iter().map(|q| q.item()).collect();
    out.rels = rels;
    out
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
        .filter_map(|c| plain_var(&c.inner))
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
    _marker: std::marker::PhantomData<&'q ()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Nested,
    Merge,
    Hash,
}

/// The conditions of a join that a kind of join tests itself, with the outer value on the left, and the other conditions.
fn split<'q>(
    quals: &[Qual<'q>],
    outer: &BTreeSet<usize>,
    inner: &BTreeSet<usize>,
    usable: fn(u32) -> bool,
) -> (Vec<Clause<'q>>, Vec<Qual<'q>>) {
    let mut clauses = Vec::new();
    let mut other = Vec::new();
    for qual in quals {
        match clause(qual, outer, inner, usable) {
            Some(c) => clauses.push(c),
            None => other.push(qual.clone()),
        }
    }
    (clauses, other)
}

/// The number of rows of the join that pass the conditions, as `approx_tuple_count`.
fn tuple_count(input: &Input<'_, '_>, clauses: &[Clause<'_>]) -> f64 {
    let mut selectivity = 1.0;
    for c in clauses {
        let (l, _) = distinct(&c.outer, input.stats);
        let (r, _) = distinct(&c.inner, input.stats);
        selectivity /= l.max(r);
    }
    cost::clamp_rows(input.outer.node.cost.rows * input.inner.node.cost.rows * selectivity)
}

/// The nested loops of a join, with their costs as `initial_cost_nestloop` and `final_cost_nestloop`: with the plain inner side, and then with a `Material` node over the inner side, as `match_unsorted_outer` makes them. The inner side gets no `Material` node when it keeps its rows already or reads the outer side.
fn nested_loops<'q>(input: &Input<'_, 'q>, join: JoinKind, lateral: bool) -> Vec<Candidate<'q>> {
    let mut out = Vec::new();
    let mut materialize = vec![false];
    if !lateral && !keeps_rows(&input.inner.node.kind) {
        materialize.push(true);
    }
    for materialize in materialize {
        let (startup, total) = nested_cost(input, materialize);
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
            _marker: std::marker::PhantomData,
        });
    }
    out
}

/// The start and the total cost of a nested loop, with a `Material` node on the inner side or with none.
fn nested_cost(input: &Input<'_, '_>, materialize: bool) -> (f64, f64) {
    let o = input.outer.node.cost;
    let (inner, rescan) = if materialize {
        let i = input.inner.node.cost;
        let spill = cost::spill(i.rows, i.width);
        let total = i.total + 2.0 * cost::CPU_OPERATOR * i.rows + spill;
        (Cost { total, ..i }, (0.0, cost::CPU_OPERATOR * i.rows + spill))
    } else {
        (input.inner.node.cost, rescan(&input.inner.node))
    };
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
    // `estimate_hash_bucket_stats`: the fraction of the inner rows in a bucket of all the batches, 0.1 with no statistics.
    let virtual_buckets = buckets * batches;
    // The unique rows of the inner side of a semi join fill the buckets evenly, and no value has more than one row.
    let bucket = if input.made.1 {
        1.0 / virtual_buckets
    } else {
        clauses
            .iter()
            .map(|c| match numdistinct(&c.inner, input.stats) {
                (_, true, _) => 0.1,
                (nd, false, rel) => {
                    // The conditions of the relation take away values in the same fraction as rows.
                    let nd = match rel {
                        Some((tuples, rows)) if tuples > 0.0 => {
                            cost::clamp_rows(nd * rows / tuples)
                        }
                        _ => nd,
                    };
                    (1.0 / nd.clamp(1.0, virtual_buckets)).max(1.0e-6)
                }
            })
            .fold(1.0, f64::min)
    };
    // With no statistics, the most common value of the inner side can be all of its rows. The bucket of that value must fit in `hash_mem`, because more batches cannot split it. A hash join that does not fit costs `disable_cost` more, so that the planner uses it only when it has no other plan.
    if !input.made.1 && cost::byte_size(i.rows, i.width) > cost::HASH_MEM {
        startup += cost::DISABLE;
    }
    // The conditions of the hash cost as `cost_qual_eval` finds them for each row that a bucket has.
    let qual_cost = cost::qual_cost(clauses.iter().map(|c| &*c.item.expr));
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
        _marker: std::marker::PhantomData,
    })
}

/// True when the order of the rows of a plan is the order of the keys of a merge join, from the first key. `value` gives the value of the side for a clause.
fn ordered(order: &[OrderKey], keys: &[MergeKey], value: impl Fn(usize) -> Expr) -> bool {
    keys.len() <= order.len()
        && keys
            .iter()
            .zip(order)
            .all(|(&(c, desc, nulls_first), k)| k.sorts(&value(c), desc, nulls_first))
}

/// `select_outer_pathkeys_for_merge`: the order of the keys of a merge join that sorts its sides. The keys of the query come first, in its directions, when the conditions have all of them. The condition of an outer join has only the value of its outer side, and only when that side is not the side that gets nulls. When the conditions are all keys of the query, they are the keys. The other keys follow in ascending order, first the keys with more expressions of relations outside the join that a condition makes equal to them.
fn merge_order(input: &Input<'_, '_>, join: JoinKind, clauses: &[Clause<'_>]) -> Vec<MergeKey> {
    let has = |c: &Clause<'_>, e: &Expr| match join {
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
    let score = |c: &Clause<'_>| {
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

/// `find_mergeclauses_for_outer_pathkeys`: the keys of a merge join in the order of the rows of the outer side, when that order has the value of each clause.
fn outer_order(order: &[OrderKey], clauses: &[Clause<'_>]) -> Option<Vec<MergeKey>> {
    let mut used = vec![false; clauses.len()];
    let mut out = Vec::new();
    for k in order {
        let before = out.len();
        for (i, c) in clauses.iter().enumerate() {
            if !used[i] && k.sorts(&c.outer, k.desc, k.nulls_first) {
                used[i] = true;
                out.push((i, k.desc, k.nulls_first));
            }
        }
        if out.len() == before {
            break;
        }
    }
    (out.len() == clauses.len()).then_some(out)
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

/// `generate_mergejoin_paths`: the merge joins of a plan of the outer side in the order of its rows, when that order has the value of each condition. The cheapest plan of the inner side gets a `Sort` when it is not in that order. When it is not, the inner plans in that order with the lowest total cost and with the lowest start cost are inner sides too.
fn presorted_merges<'q>(
    input: &Input<'_, 'q>,
    inners: &[Rel<'q>],
    join: JoinKind,
) -> Vec<Candidate<'q>> {
    let (clauses, _) = split(input.quals, &input.outer.rels, &input.inner.rels, exprs::can_merge);
    if clauses.is_empty() {
        return Vec::new();
    }
    let Some(keys) = outer_order(&input.outer.order, &clauses) else { return Vec::new() };
    let mut out: Vec<Candidate<'q>> = merge_join(input, join, keys.clone()).into_iter().collect();
    let sorted: Vec<usize> = (0..inners.len())
        .filter(|&j| ordered(&inners[j].order, &keys, |c| clauses[c].inner.clone()))
        .collect();
    let costs = |j: usize| (inners[j].node.cost.startup, inners[j].node.cost.total);
    let by = |a: usize, b: usize, total: bool| {
        let ((sa, ta), (sb, tb)) = (costs(a), costs(b));
        if total {
            ta.total_cmp(&tb).then(sa.total_cmp(&sb))
        } else {
            sa.total_cmp(&sb).then(ta.total_cmp(&tb))
        }
    };
    // `get_cheapest_path_for_pathkeys` and `compare_path_costs`: the first plan with the lowest cost.
    let best = |total: bool| sorted.iter().copied().min_by(|&a, &b| by(a, b, total));
    let presorted = sorted.first() == Some(&0);
    let mut total = None;
    if let Some(j) = best(true)
        && (!presorted || by(j, 0, true).is_lt())
    {
        total = Some(j);
    }
    let mut startup = None;
    if let Some(j) = best(false)
        && (!presorted || by(j, 0, false).is_lt())
        && j != total.unwrap_or(0)
    {
        startup = Some(j);
    }
    for j in total.into_iter().chain(startup) {
        let input = Input { inner: &inners[j], ..*input };
        out.extend(
            merge_join(&input, join, keys.clone()).map(|c| Candidate { paths: (0, j), ..c }),
        );
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
    let sort_outer = !ordered(&input.outer.order, &keys, |c| clauses[c].outer.clone());
    let sort_inner = !ordered(&input.inner.order, &keys, |c| clauses[c].inner.clone());
    let side = |rel: &Rel<'_>, sorted: bool| {
        let c = rel.node.cost;
        if sorted {
            cost::sort((c.startup, c.total), c.rows, c.width, None)
        } else {
            (c.startup, c.total)
        }
    };
    let (o, i) = (side(input.outer, sort_outer), side(input.inner, sort_inner));
    let (outer_rows, inner_rows) =
        (nonzero(input.outer.node.cost.rows), nonzero(input.inner.node.cost.rows));
    let tuples = tuple_count(input, &clauses);
    // `skip_mark_restore`: when the inner side is unique and the join has no other condition, the join never goes back to an inner row. With the unique rows of a semi join as the outer side, no outer row has the value of the row before it.
    let skip_mark_restore = input.unique.is_some() && other.is_empty() && !input.filtered;
    let rescanned =
        if skip_mark_restore || input.made.0 { 0.0 } else { (tuples - inner_rows).max(0.0) };
    let ratio = 1.0 + rescanned / inner_rows.max(1.0);
    let inner_run = i.1 - i.0;
    let startup = o.0 + i.0;
    let mut run = o.1 - o.0;
    let material = inner_run + cost::CPU_OPERATOR * inner_rows * ratio;
    let bare = inner_run * ratio;
    // The inner side needs a `Material` node when it is cheaper, when its node cannot go back to a row that it gave, as `ExecSupportsMarkRestore`, or when its sort does not fit in `work_mem`.
    let inner_cost = input.inner.node.cost;
    let materialize = !skip_mark_restore
        && (material < bare
            || !sort_inner
                && !matches!(
                    input.inner.node.kind,
                    Kind::Sort(_) | Kind::Materialize | Kind::Result { .. }
                )
            || sort_inner && cost::byte_size(inner_cost.rows, inner_cost.width) > cost::WORK_MEM);
    run += if materialize { material } else { bare };
    run +=
        cost::qual_cost(clauses.iter().map(|c| &*c.item.expr)) * (outer_rows + inner_rows * ratio);
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
        _marker: std::marker::PhantomData,
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
        })
        .collect()
}

/// The clauses of a merge join in the order of its keys, each with the directions of its key.
fn merge_clauses<'q>(clauses: Vec<Clause<'q>>, keys: &[MergeKey]) -> Vec<(Clause<'q>, bool, bool)> {
    let mut slots: Vec<Option<Clause<'q>>> = clauses.into_iter().map(Some).collect();
    keys.iter()
        .filter_map(|&(c, desc, nulls_first)| Some((slots[c].take()?, desc, nulls_first)))
        .collect()
}

impl<'q> Candidate<'q> {
    /// The order of the rows of the join, as `build_join_pathkeys`: the order of the outer side for a nested loop, the order of the values of the conditions for a merge join that is not a right join, a right anti join or a full join, and no order for a hash join.
    fn order(&self, outer: &Rel<'q>, inner: &Rel<'q>, quals: &[Qual<'q>]) -> Vec<OrderKey> {
        match self.method {
            Method::Nested => outer.order.clone(),
            Method::Merge
                if !matches!(self.join, JoinKind::Right | JoinKind::RightAnti | JoinKind::Full) =>
            {
                let clauses = split(quals, &outer.rels, &inner.rels, exprs::can_merge).0;
                merge_clauses(clauses, &self.keys)
                    .into_iter()
                    .map(|(c, desc, nulls_first)| OrderKey {
                        exprs: vec![c.outer, c.inner],
                        desc,
                        nulls_first,
                    })
                    .collect()
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
                let inner_node =
                    if self.materialize { materialize(inner.node) } else { inner.node };
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
                let cond = clauses.into_iter().map(|c| c.item).collect();
                let join_filter = other.iter().map(|q| q.item()).collect();
                Node::new(
                    Kind::HashJoin { join: self.join, cond, join_filter },
                    scope,
                    vec![outer.node, hash],
                )
            }
            Method::Merge => {
                let (clauses, other) = split(quals, &outer.rels, &inner.rels, exprs::can_merge);
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
                let cond = clauses.into_iter().map(|(c, _, _)| c.item).collect();
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
        Rel { node, rels: BTreeSet::new(), order }
    }
}

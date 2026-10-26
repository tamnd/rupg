//! The equivalence classes of the `=` conditions, as `process_equivalence`, `reconsider_outer_join_clauses` and `generate_base_implied_equalities_const` make them. A class with a constant gives each of its expressions a condition of its own with the constant, and its conditions of joins go. A class with no constant gives each join one condition, as `generate_join_implied_equalities` makes it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, Func, FuncForm, JoinKind};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use super::{JoinTree, Qual, Tree, equivalence, place};
use crate::exprs;

/// The `amopmethod` of btree.
const BTREE: u32 = 403;

/// The `amopstrategy` of `=` in a btree family.
const EQUAL: i16 = 3;

/// An `EquivalenceClass`: the expressions that the `=` conditions of a join domain make equal.
#[derive(Debug)]
struct Class<'q> {
    members: Vec<&'q Expr>,
    /// `ec_opfamilies`: the btree families of the operators of the conditions.
    families: Vec<u32>,
    /// The places of the conditions of the class in the order of `deconstruct_jointree`.
    sources: Vec<usize>,
}

impl<'q> Class<'q> {
    /// The member that the conditions of the class compare with, as `generate_base_implied_equalities_const` chooses it: the first constant, or else the first member with no column.
    fn constant(&self) -> Option<&'q Expr> {
        let mut consts = self.members.iter().copied().filter(|m| is_const(m)).peekable();
        let first = *consts.peek()?;
        Some(consts.find(|m| matches!(m.kind, ExprKind::Const(_))).unwrap_or(first))
    }

    /// True when a member of the class is the expression.
    fn has(&self, expr: &Expr, families: &[u32]) -> bool {
        self.families == families && self.members.iter().any(|m| m.same(expr))
    }
}

/// What the equivalence classes of the join domains of a query give to the rest of the planner.
#[derive(Debug, Default)]
pub(super) struct Found<'q> {
    /// The conditions of joins that the classes took the place of.
    pub(super) joined: Vec<&'q Expr>,
    /// `ec_relids`: the relations of the members of each class.
    pub(super) spans: Vec<BTreeSet<usize>>,
    /// The classes with no constant whose conditions the joins make again.
    pub(super) classes: Vec<Joined<'q>>,
    /// The members of each class of the domain of the whole query, which `exprs_known_equal` reads, each class with `ec_has_const`: true when it has a constant.
    pub(super) equal: Vec<(Vec<&'q Expr>, bool)>,
}

/// A class with no constant whose conditions of joins the joins make again. Each member reads one relation, and no two members read the same relation.
#[derive(Debug)]
pub(super) struct Joined<'q> {
    /// The members, each with its relation.
    members: Vec<(&'q Expr, usize)>,
    families: Vec<u32>,
    /// The place of the class in [`Found::classes`], which each condition of the class has as its `parent_ec`.
    id: usize,
    /// `ec_derives`: the conditions that the class made, each with the places of its two members. The class makes the condition of two members once, with the members in the order of the first request, so that a join and a scan of an index test the same condition.
    derived: RefCell<Vec<(usize, usize, Expr)>>,
}

impl<'q> Joined<'q> {
    /// The members of the class, each with its relation.
    pub(super) fn members(&self) -> &[(&'q Expr, usize)] {
        &self.members
    }

    /// `generate_join_implied_equalities_normal`: the condition `outer = inner` of a join when each side has a member of the class. The members are the first pair with the best score: one point for each member that is a column, and one for an operator that a hash join can use.
    pub(super) fn condition(
        &self,
        outer: &BTreeSet<usize>,
        inner: &BTreeSet<usize>,
    ) -> Option<Qual<'q>> {
        let side = |rels: &BTreeSet<usize>| -> Vec<(usize, &'q Expr)> {
            let members = self.members.iter().enumerate();
            members.filter(|(_, (_, r))| rels.contains(r)).map(|(at, (m, _))| (at, *m)).collect()
        };
        let (outers, inners) = (side(outer), side(inner));
        let mut best: Option<(usize, usize, usize)> = None;
        'outer: for &(oa, o) in &outers {
            for &(ia, i) in &inners {
                let Some(expr) = equality(&self.families, o, i) else { continue };
                let hash = exprs::binary(&expr).is_some_and(|(op, _, _)| exprs::can_hash(op));
                let score =
                    usize::from(is_column(o)) + usize::from(is_column(i)) + usize::from(hash);
                if best.is_none_or(|b| score > b.0) {
                    best = Some((score, oa, ia));
                    if score == 3 {
                        break 'outer;
                    }
                }
            }
        }
        let (_, oa, ia) = best?;
        self.derive(oa, ia)
    }

    /// `generate_implied_equalities_for_column` for the class: when a member reads the relation `r` alone and `matches` accepts it with the families of the class, the condition of that member with each member of another relation, but not of the relations of `prohibited`, in the order of the members.
    pub(super) fn column(
        &self,
        r: usize,
        matches: impl Fn(&Expr, &[u32]) -> bool,
        prohibited: &BTreeSet<usize>,
    ) -> Vec<Qual<'q>> {
        let mut found = self.members.iter().enumerate();
        let Some((at, _)) = found.find(|(_, (m, o))| *o == r && matches(m, &self.families)) else {
            return Vec::new();
        };
        let others = self.members.iter().enumerate();
        others
            .filter(|(_, (_, o))| *o != r && !prohibited.contains(o))
            .filter_map(|(other, _)| self.derive(at, other))
            .collect()
    }

    /// `create_join_clause`: the condition `left = right` of two members, from `ec_derives` when the class made it before with the members in either order.
    fn derive(&self, left: usize, right: usize) -> Option<Qual<'q>> {
        let mut derived = self.derived.borrow_mut();
        let made =
            derived.iter().find(|(a, b, _)| (*a, *b) == (left, right) || (*a, *b) == (right, left));
        let expr = match made {
            Some((_, _, expr)) => expr.clone(),
            None => {
                let expr = equality(&self.families, self.members[left].0, self.members[right].0)?;
                derived.push((left, right, expr.clone()));
                expr
            }
        };
        let rels = BTreeSet::from([self.members[left].1, self.members[right].1]);
        Some(Qual { class: Some(self.id), ..Qual::implied(expr, rels) })
    }
}

/// True for a column, also with a `RelabelType` over it.
fn is_column(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Var(_) => true,
        ExprKind::Relabel(arg, _) => matches!(arg.kind, ExprKind::Var(_)),
        _ => false,
    }
}

/// The class as a class whose conditions of joins the joins make again: a class with no constant, where each member reads one relation of the domain. The joins also make the condition of a class of one condition, with the sides in the order of the join. Two members of one relation give the condition `previous = member` to the scan of the relation, as `generate_base_implied_equalities_no_const` makes it.
fn join_class<'q>(class: &Class<'q>, own: &BTreeSet<usize>) -> Option<(Joined<'q>, Vec<Qual<'q>>)> {
    if class.constant().is_some() || class.sources.is_empty() {
        return None;
    }
    let mut members: Vec<(&'q Expr, usize)> = Vec::new();
    let mut quals = Vec::new();
    for &member in &class.members {
        let rels = exprs::relations(member);
        let mut each = rels.iter();
        let (Some(&r), None) = (each.next(), each.next()) else { return None };
        if has_sublink(member) || !own.contains(&r) {
            return None;
        }
        if let Some(&(previous, _)) = members.iter().rev().find(|&&(_, o)| o == r) {
            let expr = equality(&class.families, previous, member)?;
            quals.push(Qual::implied(expr, rels));
        }
        members.push((member, r));
    }
    let derived = RefCell::new(Vec::new());
    Some((Joined { members, families: class.families.clone(), id: 0, derived }, quals))
}

/// Makes the conditions of the equivalence classes with a constant of the join domain of the tree, and then of the domains below it. `top` is true for the domain of the whole query. The result is false when two constants of a class of the top domain are not equal, so that no row passes.
pub(super) fn domain<'q>(tree: &mut Tree<'q>, top: bool, out: &mut Found<'q>) -> bool {
    let mut found = Vec::new();
    sources(tree, &mut found);
    found.sort_by_key(|(order, _)| *order);
    let classes = classes(&found);
    for class in &classes {
        out.spans.push(class.members.iter().flat_map(|m| exprs::relations(m)).collect());
        // A column above an outer join has the join in its `varnullingrels`, so it is not a member of a class of a domain below the join.
        if top {
            out.equal.push((class.members.clone(), class.constant().is_some()));
        }
    }
    let mut own = BTreeSet::new();
    own_rels(tree, &mut own);
    reconsider(tree, &classes, &mut out.joined);
    for class in &classes {
        match generate(class, &own, top) {
            Some(Generated::Never) => return false,
            Some(Generated::Quals(quals)) => {
                remove(tree, &class.sources, &mut out.joined);
                for qual in quals {
                    place(tree, qual);
                }
            }
            None => {
                if let Some((mut joins, quals)) = join_class(class, &own) {
                    remove(tree, &class.sources, &mut out.joined);
                    for qual in quals {
                        place(tree, qual);
                    }
                    joins.id = out.classes.len();
                    out.classes.push(joins);
                }
            }
        }
    }
    below(tree, out);
    true
}

/// The conditions of a join domain that can make an equivalence class, with their places: the conditions of the scans and of the inner joins and the semi joins of the domain, as a semi join is in the domain of its parent in `deconstruct_recurse`. The nullable side of an outer join is a domain of its own, and the conditions of an outer join are not in a class.
fn sources<'q>(tree: &Tree<'q>, out: &mut Vec<(usize, &'q Expr)>) {
    let quals = match tree {
        Tree::Base { quals, .. } => quals,
        Tree::Join(join) => {
            match join.kind {
                JoinKind::Inner | JoinKind::Semi => {
                    sources(&join.left, out);
                    sources(&join.right, out);
                }
                JoinKind::Left | JoinKind::Anti => sources(&join.left, out),
                _ => {}
            }
            if !matches!(join.kind, JoinKind::Inner | JoinKind::Semi) {
                return;
            }
            &join.quals
        }
    };
    for qual in quals {
        if let (Some(order), Cow::Borrowed(expr)) = (qual.order, &qual.expr)
            && equivalence(qual)
        {
            out.push((order, *expr));
        }
    }
}

/// The relations of a join domain, outside the nullable sides of its outer joins.
fn own_rels(tree: &Tree<'_>, out: &mut BTreeSet<usize>) {
    match tree {
        Tree::Base { relation, .. } => {
            out.insert(*relation);
        }
        Tree::Join(join) => match join.kind {
            JoinKind::Inner | JoinKind::Semi => {
                own_rels(&join.left, out);
                own_rels(&join.right, out);
            }
            JoinKind::Left | JoinKind::Anti => own_rels(&join.left, out),
            _ => {}
        },
    }
}

/// `process_equivalence`: the classes of the conditions in their order. A condition puts its two sides in one class, and joins the classes of the two sides when they are not the same.
fn classes<'q>(found: &[(usize, &'q Expr)]) -> Vec<Class<'q>> {
    let mut out: Vec<Class<'q>> = Vec::new();
    for &(order, expr) in found {
        let Some((op, left, right)) = exprs::binary(expr) else { continue };
        let families = families(op);
        if families.is_empty() || left.same(right) {
            continue;
        }
        let l = out.iter().position(|c| c.has(left, &families));
        let r = out.iter().position(|c| c.has(right, &families));
        match (l, r) {
            (Some(a), Some(b)) if a == b => out[a].sources.push(order),
            (Some(a), Some(b)) => {
                let merged = out.remove(b);
                let a = if b < a { a - 1 } else { a };
                out[a].members.extend(merged.members);
                out[a].sources.extend(merged.sources);
                out[a].sources.push(order);
            }
            (Some(a), None) => {
                out[a].members.push(right);
                out[a].sources.push(order);
            }
            (None, Some(b)) => {
                out[b].members.push(left);
                out[b].sources.push(order);
            }
            (None, None) => {
                out.push(Class { members: vec![left, right], families, sources: vec![order] });
            }
        }
    }
    out
}

/// `get_mergejoin_opfamilies`: the btree families that have the operator as their `=`.
fn families(op: u32) -> Vec<u32> {
    let mut out: Vec<u32> = builtin::amops_of_operator(op)
        .filter(|a| a.method == BTREE && a.strategy == EQUAL && a.purpose == b's')
        .map(|a| a.family)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// True for a member of a class that reads no column of the query, `em_is_const`.
fn is_const(expr: &Expr) -> bool {
    exprs::relations(expr).is_empty()
}

/// True when the expression has a subquery, which a copy of the expression cannot show as its subplan.
fn has_sublink(expr: &Expr) -> bool {
    expr.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
}

/// The conditions that a class with a constant gives.
enum Generated<'q> {
    /// The class has two constants that are not equal.
    Never,
    /// The conditions of each member with the constant, which take the place of the conditions of the class.
    Quals(Vec<Qual<'q>>),
}

/// `generate_base_implied_equalities_const`: a condition `member = constant` for each member of a class with a constant, or `None` when the conditions of the class stay as they are. A class of two members from one condition stays, and so does a class that the planner cannot make the conditions of, as an `ec_broken` class.
fn generate<'q>(class: &Class<'q>, own: &BTreeSet<usize>, top: bool) -> Option<Generated<'q>> {
    let constant = class.constant()?;
    if class.members.len() == 2 && class.sources.len() == 1 {
        return None;
    }
    if class.members.iter().any(|m| has_sublink(m) || !exprs::relations(m).is_subset(own)) {
        return None;
    }
    let mut quals = Vec::new();
    for &member in &class.members {
        if std::ptr::eq(member, constant) {
            continue;
        }
        if is_const(member) {
            if top && unequal(member, constant)? {
                return Some(Generated::Never);
            }
            return None;
        }
        let expr = equality(&class.families, member, constant)?;
        quals.push(Qual::implied(expr, exprs::relations(member)));
    }
    Some(Generated::Quals(quals))
}

/// True when two constants are not equal, or `None` when the planner cannot tell: they must have one type that compares the bytes of the values.
fn unequal(a: &Expr, b: &Expr) -> Option<bool> {
    let (ExprKind::Const(x), ExprKind::Const(y)) = (&a.kind, &b.kind) else { return None };
    let plain =
        matches!(a.ty, oid::INT2 | oid::INT4 | oid::INT8 | oid::BOOL | oid::TEXT | oid::TID);
    if a.ty != b.ty || !plain || matches!(x, Value::Null) || matches!(y, Value::Null) {
        return None;
    }
    Some(x != y)
}

/// `select_equality_operator` and `build_implied_join_equality`: the condition `left = right` with the `=` of the first family of the class for the two types.
fn equality(families: &[u32], left: &Expr, right: &Expr) -> Option<Expr> {
    let op =
        families.iter().find_map(|&f| builtin::opfamily_member(f, left.ty, right.ty, EQUAL))?;
    let row = builtin::operator_by_oid(op)?;
    let proc = builtin::proc_by_oid(row.code)?;
    let func = Func {
        oid: row.code,
        args: vec![left.clone(), right.clone()],
        form: FuncForm::Operator(op),
        variadic: false,
        retset: proc.retset,
    };
    Some(Expr::new(ExprKind::Func(func), oid::BOOL))
}

/// `reconsider_outer_join_clauses`: a condition `outer = inner` of a left join of the domain, where `outer` is a member of a class with a constant, gives the condition `inner = constant` to the nullable side. The condition of the join then goes.
fn reconsider<'q>(tree: &mut Tree<'q>, classes: &[Class<'q>], joined: &mut Vec<&'q Expr>) {
    let Tree::Join(join) = tree else { return };
    match join.kind {
        JoinKind::Inner | JoinKind::Semi => {
            reconsider(&mut join.left, classes, joined);
            reconsider(&mut join.right, classes, joined);
        }
        JoinKind::Left | JoinKind::Anti => {
            reconsider(&mut join.left, classes, joined);
            reconsider_join(join, classes, joined);
        }
        _ => {}
    }
}

/// The conditions of a left join that `reconsider_outer_join_clause` can take away.
fn reconsider_join<'q>(join: &mut JoinTree<'q>, classes: &[Class<'q>], joined: &mut Vec<&'q Expr>) {
    let (left, right) = (join.left.rels(), join.right.rels());
    let mut kept = Vec::new();
    for qual in std::mem::take(&mut join.quals) {
        match implied(&qual, &left, &right, classes) {
            Some(quals) => {
                if let Cow::Borrowed(expr) = qual.expr {
                    joined.push(expr);
                }
                for q in quals {
                    place(&mut join.right, q);
                }
            }
            None => kept.push(qual),
        }
    }
    join.quals = kept;
}

/// The conditions `inner = constant` of a condition of a left join, for each constant of the class of the outer side.
fn implied<'q>(
    qual: &Qual<'q>,
    left: &BTreeSet<usize>,
    right: &BTreeSet<usize>,
    classes: &[Class<'q>],
) -> Option<Vec<Qual<'q>>> {
    if qual.order.is_none() || !equivalence(qual) {
        return None;
    }
    let (op, a, b) = exprs::binary(&qual.expr)?;
    let (ra, rb) = (exprs::relations(a), exprs::relations(b));
    let side = |o: &BTreeSet<usize>, i: &BTreeSet<usize>| {
        !o.is_empty() && !i.is_empty() && o.is_subset(left) && i.is_subset(right)
    };
    let (outer, inner, rels) = if side(&ra, &rb) {
        (a, b, rb)
    } else if side(&rb, &ra) {
        (b, a, ra)
    } else {
        return None;
    };
    if has_sublink(inner) {
        return None;
    }
    let families = families(op);
    let class = classes.iter().find(|c| c.has(outer, &families))?;
    let consts: Vec<&Expr> = class.members.iter().copied().filter(|m| is_const(m)).collect();
    if consts.is_empty() || consts.iter().any(|c| has_sublink(c)) {
        return None;
    }
    consts
        .into_iter()
        .map(|c| Some(Qual::implied(equality(&class.families, inner, c)?, rels.clone())))
        .collect()
}

/// Takes the conditions with these places out of the scans and the inner joins of the domain. The conditions of the joins are added to `joined`.
fn remove<'q>(tree: &mut Tree<'q>, orders: &[usize], joined: &mut Vec<&'q Expr>) {
    let quals = match tree {
        Tree::Base { quals, .. } => quals,
        Tree::Join(join) => {
            match join.kind {
                JoinKind::Inner | JoinKind::Semi => {
                    remove(&mut join.left, orders, joined);
                    remove(&mut join.right, orders, joined);
                }
                JoinKind::Left | JoinKind::Anti => remove(&mut join.left, orders, joined),
                _ => {}
            }
            if !matches!(join.kind, JoinKind::Inner | JoinKind::Semi) {
                return;
            }
            for qual in &join.quals {
                if let (Some(order), Cow::Borrowed(expr)) = (qual.order, &qual.expr)
                    && orders.contains(&order)
                {
                    joined.push(*expr);
                }
            }
            &mut join.quals
        }
    };
    quals.retain(|q| q.order.is_none_or(|o| !orders.contains(&o)));
}

/// The classes of the join domains below the domain: the nullable side of each left join and the two sides of a full join.
fn below<'q>(tree: &mut Tree<'q>, out: &mut Found<'q>) {
    let Tree::Join(join) = tree else { return };
    match join.kind {
        JoinKind::Inner | JoinKind::Semi => {
            below(&mut join.left, out);
            below(&mut join.right, out);
        }
        JoinKind::Left | JoinKind::Anti => {
            below(&mut join.left, out);
            domain(&mut join.right, false, out);
        }
        _ => {
            domain(&mut join.left, false, out);
            domain(&mut join.right, false, out);
        }
    }
}

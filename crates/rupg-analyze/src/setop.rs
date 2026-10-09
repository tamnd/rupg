//! `transformSetOperationStmt` of `analyze.c`: `UNION`, `INTERSECT` and `EXCEPT`.
//!
//! Each `SELECT` of the tree is a subquery of the query, in the order of the tree from the left. The columns of the query are the columns of the leftmost `SELECT`, with the types that the operations give. `ORDER BY` can use only the names and the numbers of the columns.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{LockClauseStrength, SelectStmt, SetOperation};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{Column, Relation};
use crate::select::{Query, Target};
use crate::sort::{SortGroup, set_group};
use crate::typename::place;

/// The kinds of set operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetKind {
    Union,
    Intersect,
    Except,
}

/// A part of the tree of a set operation.
#[derive(Clone, Debug, PartialEq)]
pub enum SetTree {
    /// A `SELECT`, as the index of its subquery in [`Query::relations`].
    Leaf(usize),
    /// An operation on two parts.
    Op(Box<SetOp>),
}

/// An operation of the tree, as `SetOperationStmt`.
#[derive(Clone, Debug, PartialEq)]
pub struct SetOp {
    pub kind: SetKind,
    /// `ALL`: the operation keeps the rows that are equal.
    pub all: bool,
    pub left: SetTree,
    pub right: SetTree,
    /// The type and the typmod of each column of the result.
    pub columns: Vec<(u32, i32)>,
    /// The operators that find the equal rows, one for each column, as `groupClauses`. `UNION ALL` has none.
    pub groups: Vec<SortGroup>,
}

impl SetTree {
    /// The index of the leftmost `SELECT`.
    pub fn leftmost(&self) -> usize {
        match self {
            SetTree::Leaf(index) => *index,
            SetTree::Op(op) => op.left.leftmost(),
        }
    }

    /// Calls `f` for the index of each `SELECT` of the part.
    fn each_leaf(&self, f: &mut impl FnMut(usize)) {
        match self {
            SetTree::Leaf(index) => f(*index),
            SetTree::Op(op) => {
                op.left.each_leaf(f);
                op.right.each_leaf(f);
            }
        }
    }
}

/// `LCS_asString`.
pub(crate) fn strength_name(strength: LockClauseStrength) -> &'static str {
    match strength {
        LockClauseStrength::LCS_FORKEYSHARE => "FOR KEY SHARE",
        LockClauseStrength::LCS_FORSHARE => "FOR SHARE",
        LockClauseStrength::LCS_FORNOKEYUPDATE => "FOR NO KEY UPDATE",
        _ => "FOR UPDATE",
    }
}

/// The error of `FOR UPDATE` and the other locking clauses in a set operation.
fn locking_error(s: &SelectStmt) -> Result<()> {
    match s.lockingClause.first() {
        Some(Some(rupg_sql::nodes::Node::LockingClause(lock))) => Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{} is not allowed with UNION/INTERSECT/EXCEPT", strength_name(lock.strength)),
        )),
        _ => Ok(()),
    }
}

/// `select_common_type` gives the first expression with the type that it selects, and the error points at it.
fn best(exprs: [&Expr; 2], ty: u32) -> Option<usize> {
    let found = exprs.iter().find(|e| e.ty == ty);
    found.unwrap_or(&exprs[0]).place()
}

impl Analyzer<'_> {
    /// `transformSetOperationStmt`: the query tree of a set operation.
    pub(crate) fn set_operation(&mut self, s: &SelectStmt) -> Result<Query> {
        if s.withClause.is_some() {
            return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "WITH is not supported yet"));
        }
        let mut leftmost = s;
        while leftmost.op != SetOperation::SETOP_NONE
            && let Some(left) = leftmost.larg.as_deref()
        {
            leftmost = left;
        }
        if leftmost.intoClause.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "SELECT INTO is not supported yet",
            ));
        }
        let (tree, _) = self.set_tree(s, true, std::ptr::from_ref(leftmost))?;
        let SetTree::Op(op) = tree else {
            return Err(Error::internal("a set operation with no operation"));
        };
        let left = op.left.leftmost();
        let Some(query) = self.scope.relations[left].subquery.as_deref() else {
            return Err(Error::internal("a set operation with no leftmost SELECT"));
        };
        let mut targets = Vec::with_capacity(op.columns.len());
        for (i, (target, &(ty, typmod))) in
            query.targets.iter().filter(|t| !t.junk).zip(&op.columns).enumerate()
        {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let var = Var { relation: left, attnum, levels_up: 0 };
            let expr = Expr { kind: ExprKind::Var(var), ty, typmod, location: target.expr.place() };
            targets.push(Target { name: target.name.clone(), expr, origin: None, junk: false });
        }
        // The names of the columns are visible to `ORDER BY`, which cannot add a junk column.
        let columns = targets.iter().map(|t| (t.name.clone(), t.expr.clone())).collect();
        let saved = self.add_columns(columns);
        let width = targets.len();
        let sort = self.sort_clause(&s.sortClause, &mut targets);
        self.remove_columns(saved);
        let sort = sort?;
        if let Some(junk) = targets.get(width) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
            )
            .with_detail("Only result column names can be used, not expressions or functions.")
            .with_hint(
                "Add the expression/function to every SELECT, or move the UNION into a FROM clause.",
            )
            .at_opt(junk.expr.place()));
        }
        let with_ties = s.limitOption == rupg_sql::nodes::LimitOption::LIMIT_OPTION_WITH_TIES;
        let offset = self.with_kind(Kind::Offset, |a| {
            a.limit_clause(s.limitOffset.as_ref(), "OFFSET", with_ties)
        })?;
        let limit = self.with_kind(Kind::Limit, |a| {
            a.limit_clause(s.limitCount.as_ref(), "LIMIT", with_ties)
        })?;
        locking_error(s)?;
        let relations = std::mem::take(&mut self.scope.relations);
        Ok(Query {
            relations,
            from: Vec::new(),
            targets,
            filter: None,
            grouped: false,
            group: Vec::new(),
            having: None,
            sort,
            distinct: Vec::new(),
            distinct_on: false,
            offset,
            limit,
            with_ties,
            params: Vec::new(),
            notices: Vec::new(),
            set_op: Some(*op),
        })
    }

    /// `transformSetOperationTree`: a part of the tree and the expression of each of its columns. A part with `ORDER BY`, `LIMIT`, `OFFSET` or `WITH` under the top is a subquery of its own. `leftmost` is the leftmost `SELECT`, which can have `INTO`.
    fn set_tree(
        &mut self,
        s: &SelectStmt,
        top: bool,
        leftmost: *const SelectStmt,
    ) -> Result<(SetTree, Vec<Expr>)> {
        if let Some(into) = &s.intoClause
            && !std::ptr::eq(s, leftmost)
        {
            let at = into.rel.as_ref().and_then(|r| place(r.location));
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT",
            )
            .at_opt(at));
        }
        if !top {
            locking_error(s)?;
        }
        let leaf = s.op == SetOperation::SETOP_NONE
            || (!top
                && (!s.sortClause.is_empty()
                    || s.limitOffset.is_some()
                    || s.limitCount.is_some()
                    || s.withClause.is_some()));
        if leaf {
            self.keep_unknowns = true;
            let query = self.sub_select(s, false)?;
            let shown = query.targets.iter().filter(|t| !t.junk);
            let exprs: Vec<Expr> = shown.clone().map(|t| t.expr.clone()).collect();
            let columns = shown
                .map(|t| Column {
                    name: t.name.clone(),
                    ty: t.expr.ty,
                    typmod: t.expr.typmod,
                    not_null: false,
                })
                .collect();
            let index = self.scope.relations.len();
            let subquery = Some(Box::new(query));
            let mut relation = Relation::new(0, columns, subquery, None, None);
            relation.name = format!("*SELECT* {}", index + 1);
            self.scope.relations.push(relation);
            return Ok((SetTree::Leaf(index), exprs));
        }
        let (kind, context) = match s.op {
            SetOperation::SETOP_UNION => (SetKind::Union, "UNION"),
            SetOperation::SETOP_INTERSECT => (SetKind::Intersect, "INTERSECT"),
            _ => (SetKind::Except, "EXCEPT"),
        };
        let (Some(larg), Some(rarg)) = (s.larg.as_deref(), s.rarg.as_deref()) else {
            return Err(Error::internal("a set operation with no arguments"));
        };
        let (left, lexprs) = self.set_tree(larg, false, leftmost)?;
        let (right, rexprs) = self.set_tree(rarg, false, leftmost)?;
        if lexprs.len() != rexprs.len() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("each {context} query must have the same number of columns"),
            )
            .at_opt(rexprs.iter().find_map(Expr::place)));
        }
        let mut columns = Vec::with_capacity(lexprs.len());
        let mut groups = Vec::new();
        let mut exprs = Vec::with_capacity(lexprs.len());
        for (i, (l, r)) in lexprs.into_iter().zip(rexprs).enumerate() {
            let ty = self.common_type(&[l.clone(), r.clone()], context)?;
            let at = best([&l, &r], ty);
            let l = self.set_column(&left, i, l, ty, context)?;
            let r = self.set_column(&right, i, r, ty, context)?;
            // select_common_typmod: the typmod of the columns when they have the type and the same typmod.
            let typmod =
                if l.ty == ty && r.ty == ty && l.typmod == r.typmod { l.typmod } else { -1 };
            if kind != SetKind::Union || !s.all {
                groups.push(set_group(i, ty, at)?);
            }
            columns.push((ty, typmod));
            exprs.push(Expr { kind: ExprKind::CaseTest, ty, typmod, location: at });
        }
        let op = SetOp { kind, all: s.all, left, right, columns, groups };
        Ok((SetTree::Op(Box::new(op)), exprs))
    }

    /// `coerce_to_common_type` for a column of a part of a set operation. A constant or a parameter of type `unknown` in a `SELECT` gets the type, so that an error of its input points at it. The other columns of each `SELECT` of the part get a cast to the type, as `generate_setop_tlist` adds it.
    fn set_column(
        &mut self,
        part: &SetTree,
        column: usize,
        expr: Expr,
        ty: u32,
        context: &str,
    ) -> Result<Expr> {
        let unknown_leaf = expr.ty == oid::UNKNOWN
            && matches!(part, SetTree::Leaf(_))
            && matches!(expr.kind, ExprKind::Const(_) | ExprKind::Param(_));
        let coerced = if expr.ty != oid::UNKNOWN || unknown_leaf {
            self.coerce_to_common(expr.clone(), ty, context)?
        } else {
            expr.clone()
        };
        let mut leaves = Vec::new();
        part.each_leaf(&mut |index| leaves.push(index));
        for index in leaves {
            let relation = &mut self.scope.relations[index];
            let Some(query) = relation.subquery.as_deref_mut() else { continue };
            let Some(target) = query.targets.iter_mut().filter(|t| !t.junk).nth(column) else {
                continue;
            };
            if target.expr.ty == ty {
                continue;
            }
            let old = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
            let new = if unknown_leaf { coerced.clone() } else { self.coerce_leaf(old, ty)? };
            let Some(query) = self.scope.relations[index].subquery.as_deref_mut() else { continue };
            if let Some(target) = query.targets.iter_mut().filter(|t| !t.junk).nth(column) {
                target.expr = new;
            }
            if let Some(slot) = self.scope.relations[index].columns.get_mut(column) {
                slot.ty = ty;
                slot.typmod = -1;
            }
        }
        Ok(if unknown_leaf { coerced } else { Expr { ty, typmod: coerced.typmod, ..expr } })
    }

    /// The cast of a column of a `SELECT` to the type of the set operation.
    fn coerce_leaf(&mut self, expr: Expr, ty: u32) -> Result<Expr> {
        self.coerce(expr, ty, -1, Context::Implicit, None)
    }
}

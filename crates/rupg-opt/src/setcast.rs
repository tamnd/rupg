//! `generate_setop_tlist`: the casts of the columns of a `SELECT` of a set operation to the types of the operation.
//!
//! The analyzer puts the casts in the targets of the `SELECT`. PostgreSQL plans the `SELECT` with its own types, and a `Subquery Scan` above the plan adds the casts, as `recurse_set_operations` makes it when the target list of the `SELECT` is not trivial. Here the `SELECT` becomes the subquery of a query that reads it and adds the casts.

use rupg_analyze::{
    CastForm, Column, Expr, ExprKind, FromItem, Func, FuncForm, Query, Relation, SetOp, SetTree,
    Target, Var,
};

/// Puts each `SELECT` of a set operation whose columns need a cast into a query that adds the casts, also in the subqueries of the query. A `SELECT` of an operation below gets its casts in its own operation, and the recursive `UNION` of `WITH` keeps its casts in the `SELECT`.
pub(crate) fn wrap_all(query: &mut Query) {
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            wrap_all(sub);
        }
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                wrap_all(&mut link.query);
            }
            false
        });
    }
    let Some(op) = query.set_op.as_mut().filter(|op| op.work.is_none()) else { return };
    project(op, &mut query.relations);
    let mut leaves = Vec::new();
    direct_leaves(op, &mut leaves);
    for (index, columns) in leaves {
        let Some(relation) = query.relations.get_mut(index) else { continue };
        let Some(sub) = relation.subquery.as_deref_mut() else { continue };
        if let Some(wrapper) = wrap(sub, &columns, index) {
            relation.subquery = Some(Box::new(wrapper));
        }
    }
}

/// True for a query that [`wrap_all`] made: it reads one subquery and casts one of its columns or more.
pub(crate) fn is_wrapper(query: &Query) -> bool {
    query.set_op.is_none()
        && query.write.is_none()
        && query.filter.is_none()
        && matches!(query.from.as_slice(), [FromItem::Relation(0)])
        && query.relations.len() == 1
        && query.relations[0].subquery.is_some()
        && query.targets.iter().any(|t| cast_arg(&t.expr).is_some())
}

/// For each operation below `op` with other types than the operation above it, the casts that a `Result` above its plan computes, in [`SetOp::project`]. The `SELECT`s of the operation give its own types, without the casts of the operations above it.
fn project(op: &mut SetOp, relations: &mut [Relation]) {
    let columns = op.columns.clone();
    for side in [&mut op.left, &mut op.right] {
        let SetTree::Op(below) = side else { continue };
        if below.columns.iter().zip(&columns).any(|(b, c)| b.0 != c.0) {
            let mut leaves = Vec::new();
            below.left.each_leaf(&mut |at| leaves.push(at));
            below.right.each_leaf(&mut |at| leaves.push(at));
            let leftmost = below.left.leftmost();
            for index in leaves {
                let Some(relation) = relations.get_mut(index) else { continue };
                let Some(sub) = relation.subquery.as_deref_mut() else { continue };
                let shown = sub.targets.iter_mut().filter(|t| !t.junk);
                for (k, (target, &(ty, typmod))) in shown.zip(&below.columns).enumerate() {
                    let mut inner = &target.expr;
                    while inner.ty != ty
                        && let Some(arg) = cast_arg(inner)
                    {
                        inner = arg;
                    }
                    let inner = inner.clone();
                    if index == leftmost {
                        let attnum = i16::try_from(k + 1).unwrap_or(i16::MAX);
                        let var = Var { relation: leftmost, attnum, levels_up: 0 };
                        let mut var = Expr::new(ExprKind::Var(var), ty);
                        var.typmod = typmod;
                        let mut cast = target.expr.clone();
                        replace(&mut cast, &|e| e.ty == ty, var);
                        below.project.push(cast);
                    }
                    target.expr = inner;
                }
                for (column, &(ty, typmod)) in relation.columns.iter_mut().zip(&below.columns) {
                    column.ty = ty;
                    column.typmod = typmod;
                }
            }
        }
        project(below, relations);
    }
}

/// The index of each `SELECT` of the operation with the types of the columns of the operation that is directly above it. A part that is an operation gives its own `SELECT`s.
fn direct_leaves(op: &SetOp, out: &mut Vec<(usize, Vec<(u32, i32)>)>) {
    for side in [&op.left, &op.right] {
        match side {
            SetTree::Leaf(index) => out.push((*index, op.columns.clone())),
            SetTree::Op(below) => direct_leaves(below, out),
        }
    }
}

/// The query that reads the `SELECT` `sub` and casts its columns to the types `columns`, with `sub` that gives its columns with no casts. `None` when no column has a cast to the type of the operation. A constant keeps its place in the `SELECT`, and the query gives the constant with the cast. The relation of `sub` has the name of the `SELECT` in the range table of PostgreSQL, where each `SELECT` of the operation is an `unnamed_subquery` with its number.
fn wrap(sub: &mut Query, columns: &[(u32, i32)], index: usize) -> Option<Query> {
    let cast = |t: &Target, ty: u32| !t.junk && t.expr.ty == ty && cast_arg(&t.expr).is_some();
    let mut shown = sub.targets.iter().filter(|t| !t.junk);
    if !shown.by_ref().zip(columns).any(|(t, &(ty, _))| cast(t, ty)) {
        return None;
    }
    let mut targets = Vec::new();
    let mut column = 0;
    for (at, target) in sub.targets.iter_mut().enumerate() {
        if target.junk {
            continue;
        }
        let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
        let full = target.expr.clone();
        let inner = full.strip_implicit().clone();
        let mut var = Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), inner.ty);
        var.typmod = inner.typmod;
        let expr = match columns.get(column) {
            Some(&(ty, _)) if cast(target, ty) => {
                let is_const = matches!(inner.kind, ExprKind::Const(_));
                target.expr = inner;
                if is_const {
                    full
                } else {
                    let mut expr = full;
                    replace(&mut expr, &|_| false, var);
                    expr
                }
            }
            _ => var,
        };
        column += 1;
        targets.push(Target {
            name: target.name.clone(),
            expr,
            origin: target.origin,
            junk: false,
        });
    }
    let relation_columns = sub
        .targets
        .iter()
        .map(|t| Column {
            name: t.name.clone(),
            ty: t.expr.ty,
            typmod: t.expr.typmod,
            not_null: false,
        })
        .collect();
    let name = if index == 0 {
        "unnamed_subquery".to_string()
    } else {
        format!("unnamed_subquery_{index}")
    };
    let relation = Relation {
        oid: 0,
        columns: relation_columns,
        subquery: Some(Box::new(std::mem::take(sub))),
        function: None,
        values: None,
        name,
        alias: None,
        lateral: false,
        cte: None,
        tablesample: None,
        only: false,
    };
    Some(Query {
        relations: vec![relation],
        from: vec![FromItem::Relation(0)],
        targets,
        ..Query::default()
    })
}

/// Puts `var` in the place of the argument of the casts that the analyzer added at the top of `expr`, at the first argument that `stop` takes or under the last cast.
fn replace(expr: &mut Expr, stop: &dyn Fn(&Expr) -> bool, var: Expr) {
    if stop(expr) || cast_arg(expr).is_none() {
        *expr = var;
    } else if let Some(arg) = cast_arg_mut(expr) {
        replace(arg, stop, var);
    }
}

/// The argument of the cast that the analyzer added at the top of the expression, as [`Expr::strip_implicit`] takes it out.
fn cast_arg(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Func(Func { form: FuncForm::ImplicitCast, args, .. }) => args.first(),
        ExprKind::Relabel(arg, CastForm::Implicit)
        | ExprKind::CoerceViaIo(arg, CastForm::Implicit)
        | ExprKind::CoerceToDomain(arg, CastForm::Implicit)
        | ExprKind::ArrayCoerce { arg, form: CastForm::Implicit, .. } => Some(arg),
        _ => None,
    }
}

/// [`cast_arg`] to change.
fn cast_arg_mut(expr: &mut Expr) -> Option<&mut Expr> {
    match &mut expr.kind {
        ExprKind::Func(Func { form: FuncForm::ImplicitCast, args, .. }) => args.first_mut(),
        ExprKind::Relabel(arg, CastForm::Implicit)
        | ExprKind::CoerceViaIo(arg, CastForm::Implicit)
        | ExprKind::CoerceToDomain(arg, CastForm::Implicit)
        | ExprKind::ArrayCoerce { arg, form: CastForm::Implicit, .. } => Some(arg),
        _ => None,
    }
}

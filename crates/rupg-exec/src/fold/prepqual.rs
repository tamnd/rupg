//! `canonicalize_qual` of `prepqual.c`: a condition of `WHERE`, `ON` or `HAVING` after it folds, in the form that the planner reads.

use rupg_analyze::{BoolOp, Expr, ExprKind};
use rupg_types::{Value, oid};

/// `canonicalize_qual` with `is_check` false: the condition with no constant arm of an `AND` or an `OR`, with nested `AND` and `OR` made flat, and with the arms that each arm of an `OR` has taken out of the `OR`, as `find_duplicate_ors` makes it.
pub(super) fn canonicalize(qual: Expr) -> Expr {
    find_duplicate_ors(qual)
}

fn bool_const(value: bool) -> Expr {
    Expr::new(ExprKind::Const(Value::Bool(value)), oid::BOOL)
}

fn bool_expr(op: BoolOp, args: Vec<Expr>) -> Expr {
    Expr::new(ExprKind::Bool(op, args), oid::BOOL)
}

/// `find_duplicate_ors`: in `WHERE` a null is as false, so an `OR` drops an arm that is false or null and is true when an arm is true, and an `AND` drops an arm that is true and is false when an arm is false or null.
fn find_duplicate_ors(qual: Expr) -> Expr {
    match qual.kind {
        ExprKind::Bool(op @ (BoolOp::And | BoolOp::Or), args) => {
            let mut list = Vec::new();
            for arg in args {
                let arg = find_duplicate_ors(arg);
                let ExprKind::Const(value) = &arg.kind else {
                    list.push(arg);
                    continue;
                };
                let true_arm = *value == Value::Bool(true);
                match op {
                    BoolOp::Or if true_arm => return arg,
                    BoolOp::Or => {}
                    _ if true_arm => {}
                    _ => return bool_const(false),
                }
            }
            if op == BoolOp::Or {
                return process_duplicate_ors(pull(BoolOp::Or, list));
            }
            let mut list = pull(BoolOp::And, list);
            match list.len() {
                0 => bool_const(true),
                1 => list.remove(0),
                _ => bool_expr(BoolOp::And, list),
            }
        }
        kind => Expr { kind, ..qual },
    }
}

/// `pull_ands` and `pull_ors`: the arms with each arm that is an `AND`, or an `OR`, in the place of its arms.
fn pull(op: BoolOp, list: Vec<Expr>) -> Vec<Expr> {
    let mut out = Vec::with_capacity(list.len());
    for arm in list {
        match arm.kind {
            ExprKind::Bool(o, args) if o == op => out.extend(pull(op, args)),
            kind => out.push(Expr { kind, ..arm }),
        }
    }
    out
}

/// The arms of an `AND`, or `None` for another condition.
fn and_args(expr: &Expr) -> Option<&[Expr]> {
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => Some(args),
        _ => None,
    }
}

/// `process_duplicate_ors`: the `OR` of the arms, with each condition that is in each arm, the winners, taken out to an `AND` above the `OR`. `(a AND b) OR (a AND c)` is `a AND (b OR c)`. When an arm has only winners, the `OR` goes.
fn process_duplicate_ors(mut list: Vec<Expr>) -> Expr {
    match list.len() {
        0 => return bool_const(false),
        1 => return list.remove(0),
        _ => {}
    }
    // The shortest `AND`, or the first arm that is not an `AND`.
    let mut reference: &[Expr] = &[];
    for clause in &list {
        match and_args(clause) {
            Some(sub) if reference.is_empty() || sub.len() < reference.len() => reference = sub,
            Some(_) => {}
            None => {
                reference = std::slice::from_ref(clause);
                break;
            }
        }
    }
    let mut unique: Vec<&Expr> = Vec::new();
    for r in reference {
        if !unique.iter().any(|u| u.same(r)) {
            unique.push(r);
        }
    }
    let in_each = |r: &Expr| {
        list.iter().all(|c| match and_args(c) {
            Some(sub) => sub.iter().any(|s| s.same(r)),
            None => r.same(c),
        })
    };
    let mut winners: Vec<Expr> = unique.into_iter().filter(|r| in_each(r)).cloned().collect();
    if winners.is_empty() {
        return bool_expr(BoolOp::Or, list);
    }
    let mut rest = Vec::new();
    for clause in list {
        let is_winner = |e: &Expr| winners.iter().any(|w| w.same(e));
        match clause.kind {
            ExprKind::Bool(BoolOp::And, sub) => {
                let mut sub: Vec<Expr> = sub.into_iter().filter(|s| !is_winner(s)).collect();
                match sub.len() {
                    0 => {
                        rest.clear();
                        break;
                    }
                    1 => rest.push(sub.remove(0)),
                    _ => rest.push(bool_expr(BoolOp::And, sub)),
                }
            }
            kind => {
                let clause = Expr { kind, ..clause };
                if is_winner(&clause) {
                    rest.clear();
                    break;
                }
                rest.push(clause);
            }
        }
    }
    match rest.len() {
        0 => {}
        1 => winners.push(rest.remove(0)),
        _ => winners.push(bool_expr(BoolOp::Or, pull(BoolOp::Or, rest))),
    }
    match winners.len() {
        1 => winners.remove(0),
        _ => bool_expr(BoolOp::And, pull(BoolOp::And, winners)),
    }
}

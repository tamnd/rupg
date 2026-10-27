//! `predicate_implied_by_simple_clause` of `predtest.c`: the proofs that a condition that is not an `AND` or an `OR` makes a part of the predicate of a partial index true. The proofs read the operators of the B-tree operator families, as `operator_predicate_proof` reads them.

use std::cmp::Ordering;

use rupg_analyze::{BoolOp, Expr, ExprKind, FuncForm};
use rupg_pgcatalog::builtin;
use rupg_types::Value;

/// `BTREE_AM_OID`.
const BTREE_AM: u32 = 403;
/// `BooleanEqualOperator`, `=` of `boolean`.
const BOOLEAN_EQUAL: u32 = 91;
/// `COMPARE_EQ`, the strategy of `=` in a B-tree family.
const EQ: i16 = 3;
/// `COMPARE_NE`, which no B-tree family has: an operator whose negator is `=` of the family.
const NE: i16 = 6;

/// `RC_implies_table`: true when `x clause_op y` implies `x pred_op y`, by the strategies of the operators from 1 to 6, the clause first.
const IMPLIES: [[bool; 6]; 6] = [
    [true, true, false, false, false, true],
    [false, true, false, false, false, false],
    [false, true, true, true, false, false],
    [false, false, false, true, false, false],
    [false, false, false, true, true, true],
    [false, false, false, false, false, true],
];

/// `RC_implic_table`: the strategy of the test `const2 test_op const1` that proves `x pred_op const2` from `x clause_op const1`, or 0 when no test can, by the strategies of the operators, the clause first.
const IMPLIC: [[i16; 6]; 6] = [
    [4, 4, 0, 0, 0, 4],
    [5, 4, 0, 0, 0, 5],
    [5, 4, 3, 2, 1, 6],
    [0, 0, 0, 2, 1, 1],
    [0, 0, 0, 2, 2, 2],
    [0, 0, 0, 0, 0, 3],
];

/// `predicate_implied_by_simple_clause` with strong implication: true when the condition `clause` makes `pred` true. A condition proves itself, `x = true` proves `x` and `x = false` proves `NOT x`. A strict condition of `x` proves `x IS NOT NULL`. Two operators of a B-tree family on the same expressions, or on the same expression and two constants, give the proofs of `operator_predicate_proof`.
pub(super) fn implied(pred: &Expr, clause: &Expr) -> bool {
    if pred.same(clause) {
        return true;
    }
    if let Some((BOOLEAN_EQUAL, [left, right])) = operator(clause)
        && let ExprKind::Const(Value::Bool(value)) = right.kind
    {
        let proved = match &pred.kind {
            ExprKind::Bool(BoolOp::Not, args) if !value => {
                matches!(args.as_slice(), [arg] if arg.same(left))
            }
            _ => value && pred.same(left),
        };
        if proved {
            return true;
        }
    }
    if let ExprKind::NullTest(arg, false) = &pred.kind
        && !matches!(arg.kind, ExprKind::Row(..))
        && strict_for(clause, arg)
    {
        return true;
    }
    operator_proof(pred, clause)
}

/// The operator and the two arguments of a call of an infix operator.
fn operator(expr: &Expr) -> Option<(u32, &[Expr])> {
    match &expr.kind {
        ExprKind::Func(f) => match (f.form, f.args.as_slice()) {
            (FuncForm::Operator(op), args @ [_, _]) => Some((op, args)),
            _ => None,
        },
        _ => None,
    }
}

/// `clause_is_strict_for`: true when the condition is null or false when `sub` is null, because the condition is `sub` or a strict function of an argument that is.
fn strict_for(clause: &Expr, sub: &Expr) -> bool {
    if clause.same(sub) {
        return true;
    }
    match &clause.kind {
        ExprKind::Func(f) if !f.retset => {
            let code = match f.form {
                FuncForm::Operator(op) => builtin::operator_by_oid(op).map_or(0, |o| o.code),
                _ => f.oid,
            };
            builtin::proc_by_oid(code).is_some_and(|p| p.strict)
                && f.args.iter().any(|a| strict_for(a, sub))
        }
        ExprKind::Relabel(arg, _) => strict_for(arg, sub),
        _ => false,
    }
}

/// `operator_predicate_proof` for implication: both conditions are calls of infix operators with one argument the same. With the other arguments also the same, the operators prove it by their strategies. With constants as the other arguments, the test operator of the strategies compares the constants, with the constants on the right of the operators.
fn operator_proof(pred: &Expr, clause: &Expr) -> bool {
    let (Some((mut pred_op, [pl, pr])), Some((mut clause_op, [cl, cr]))) =
        (operator(pred), operator(clause))
    else {
        return false;
    };
    let commute = |op: u32| builtin::operator_by_oid(op).map_or(0, |o| o.commutator);
    let (pred_const, clause_const) = if pl.same(cl) {
        if pr.same(cr) {
            return same_operands(pred_op, clause_op);
        }
        (pr, cr)
    } else if pr.same(cr) {
        pred_op = commute(pred_op);
        clause_op = commute(clause_op);
        (pl, cl)
    } else if pl.same(cr) {
        if pr.same(cl) {
            return same_operands(commute(pred_op), clause_op);
        }
        clause_op = commute(clause_op);
        (pr, cl)
    } else if pr.same(cl) {
        pred_op = commute(pred_op);
        (pl, cr)
    } else {
        return false;
    };
    if pred_op == 0 || clause_op == 0 {
        return false;
    }
    let (ExprKind::Const(pred_value), ExprKind::Const(clause_value)) =
        (&pred_const.kind, &clause_const.kind)
    else {
        return false;
    };
    let strict = |op: u32| {
        builtin::operator_by_oid(op)
            .and_then(|o| builtin::proc_by_oid(o.code))
            .is_some_and(|p| p.strict)
    };
    // A strict clause of a null is null, and a false condition implies any predicate.
    if matches!(clause_value, Value::Null) {
        return strict(clause_op);
    }
    if matches!(pred_value, Value::Null) {
        return false;
    }
    let Some(test) = test_strategy(pred_op, clause_op) else { return false };
    let Some(order) = crate::compare(pred_const.ty, pred_value, clause_const.ty, clause_value)
    else {
        return false;
    };
    match test {
        1 => order == Ordering::Less,
        2 => order != Ordering::Greater,
        3 => order == Ordering::Equal,
        4 => order != Ordering::Less,
        5 => order == Ordering::Greater,
        _ => order != Ordering::Equal,
    }
}

/// `get_op_index_interpretation`: the B-tree families of the operator, each with the types of its arguments and its strategy. An operator whose negator is `=` of a family has the strategy [`NE`] in that family.
fn interpretations(op: u32) -> Vec<(u32, u32, u32, i16)> {
    let btree = |a: &&builtin::AmopRow| a.method == BTREE_AM && a.purpose == b's';
    let mut out: Vec<_> = builtin::amops_of_operator(op)
        .filter(btree)
        .map(|a| (a.family, a.left, a.right, a.strategy))
        .collect();
    if out.is_empty()
        && let Some(negator) = builtin::operator_by_oid(op).map(|o| o.negator).filter(|&n| n != 0)
    {
        out.extend(
            builtin::amops_of_operator(negator)
                .filter(btree)
                .filter(|a| a.strategy == EQ)
                .map(|a| (a.family, a.left, a.right, NE)),
        );
    }
    out
}

/// True when the operator calls an immutable function.
fn immutable(op: u32) -> bool {
    builtin::operator_by_oid(op)
        .and_then(|o| builtin::proc_by_oid(o.code))
        .is_some_and(|p| p.volatile == b'i')
}

/// `operator_same_subexprs_proof`: true when `x clause_op y` implies `x pred_op y`, because the operators are the same or by their strategies in a B-tree family.
fn same_operands(pred_op: u32, clause_op: u32) -> bool {
    if pred_op == 0 {
        return false;
    }
    if pred_op == clause_op {
        return true;
    }
    let clauses = interpretations(clause_op);
    immutable(clause_op)
        && interpretations(pred_op).iter().any(|p| {
            clauses.iter().any(|c| c.0 == p.0 && IMPLIES[(c.3 - 1) as usize][(p.3 - 1) as usize])
        })
}

/// `get_btree_test_op` for implication: the strategy of the immutable operator of a B-tree family of both operators that compares the constant of the predicate with the constant of the clause. The family must be the default family of its types, as [`crate::compare`] compares the values with the default order of their types.
fn test_strategy(pred_op: u32, clause_op: u32) -> Option<i16> {
    let clauses = interpretations(clause_op);
    let default = |family: u32| {
        builtin::opclasses().iter().any(|c| c.method == BTREE_AM && c.family == family && c.default)
    };
    interpretations(pred_op).into_iter().find_map(|(family, _, pred_right, pred_strategy)| {
        clauses.iter().find_map(|&(f, _, clause_right, clause_strategy)| {
            if f != family || !default(family) {
                return None;
            }
            let test = IMPLIC[(clause_strategy - 1) as usize][(pred_strategy - 1) as usize];
            let op = match test {
                0 => return None,
                NE => builtin::opfamily_member(family, pred_right, clause_right, EQ)
                    .and_then(builtin::operator_by_oid)
                    .map(|o| o.negator),
                _ => builtin::opfamily_member(family, pred_right, clause_right, test),
            }?;
            immutable(op).then_some(test)
        })
    })
}

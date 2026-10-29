//! The reasons of `ATRewriteTables` to write the rows of a table again, for the event triggers of `table_rewrite`.

use rupg_catalog::RelKind;
use rupg_pgcatalog::builtin;

use super::{Definer, Rewrite};
use crate::expr::{Expr, ExprKind};
use crate::jsonfunc::domain_has_constraints;

/// `AT_REWRITE_ALTER_PERSISTENCE`: `SET LOGGED` or `SET UNLOGGED`.
pub(super) const ALTER_PERSISTENCE: i32 = 0x01;
/// `AT_REWRITE_DEFAULT_VAL`: a new column whose values come from its default or its generation expression.
pub(super) const DEFAULT_VAL: i32 = 0x02;
/// `AT_REWRITE_COLUMN_REWRITE`: a new type of a column that changes the stored values.
pub(super) const COLUMN_REWRITE: i32 = 0x04;

/// The need of `ATColumnChangeRequiresRewrite` for the expression that gives the new values of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    /// The stored values stay as they are.
    None,
    /// The stored values change only when the zone of the session has an offset that is not 0, as `TimestampTimestampTzRequiresRewrite` checks it.
    Zoned,
    /// The stored values change.
    Rewrite,
}

impl Definer<'_, '_> {
    /// Adds the reason `reason` to the rewrite of the table `table`, as `tab->rewrite` keeps it. A relation with no storage, such as a partitioned table, has no rewrite.
    pub(super) fn rewrite(&mut self, table: u32, reason: i32, zoned: bool) {
        let kind = self.catalog.relation(table).map(|r| r.kind);
        if !matches!(kind, Some(RelKind::Table | RelKind::MatView | RelKind::Toast)) {
            return;
        }
        match self.rewrites.iter_mut().find(|r| r.table == table) {
            Some(found) => {
                found.reason |= reason;
                found.zoned |= zoned;
            }
            None => self.rewrites.push(Rewrite { table, reason, zoned }),
        }
    }

    /// `contain_volatile_functions`: true when the expression calls a volatile function. A function that the catalog does not have counts as volatile.
    pub(super) fn volatile(&self, expr: &Expr) -> bool {
        let volatile = |func: u32| match builtin::proc_by_oid(func) {
            Some(proc) => proc.volatile == b'v',
            None => self.catalog.function(func).is_none_or(|f| f.volatile == b'v'),
        };
        expr.find(0, &mut |e, _| match &e.kind {
            ExprKind::Func(f) if volatile(f.oid) => Some(()),
            ExprKind::NullIf { equal: func, .. }
            | ExprKind::Distinct { equal: func, .. }
            | ExprKind::ScalarArrayOp { func, .. }
                if volatile(*func) =>
            {
                Some(())
            }
            _ => None,
        })
        .is_some()
    }
}

/// `ATColumnChangeRequiresRewrite`: the need to write the rows again for the expression `expr` that gives the new values of the column `attnum`, after the simplification of the planner. A relabel, a domain with no constraints, a time zone conversion of `timestamp` and a length coercion that keeps all the values need no rewrite.
pub(super) fn column_change(expr: &Expr, attnum: i16) -> Change {
    let mut expr = expr;
    let mut zoned = false;
    loop {
        match &expr.kind {
            ExprKind::Var(var) if var.attnum == attnum && var.levels_up == 0 => {
                return if zoned { Change::Zoned } else { Change::None };
            }
            ExprKind::Relabel(arg, _) | ExprKind::Collate(arg, _) => expr = arg,
            ExprKind::CoerceToDomain(arg, _) => {
                if domain_has_constraints(expr.ty) {
                    return Change::Rewrite;
                }
                expr = arg;
            }
            ExprKind::Func(f) => {
                let src = builtin::proc_by_oid(f.oid).map_or("", |p| p.src);
                let Some(arg) = f.args.first() else { return Change::Rewrite };
                if matches!(src, "timestamptz_timestamp" | "timestamp_timestamptz") {
                    zoned = true;
                } else if !keeps_values(src, arg.typmod, f.args.get(1)) {
                    return Change::Rewrite;
                }
                expr = arg;
            }
            _ => return Change::Rewrite,
        }
    }
}

/// The support functions of the length coercions, such as `numeric_support` and `varchar_support`: true when the coercion `src` from the type modifier `old` to the constant type modifier of `new` keeps all the values, so that the planner takes the argument for the call.
fn keeps_values(src: &str, old: i32, new: Option<&Expr>) -> bool {
    let Some(ExprKind::Const(rupg_types::Value::Int4(new))) = new.map(|e| &e.kind) else {
        return false;
    };
    let new = *new;
    match src {
        // `varchar_support` and `varbit_support`: the new length is not less than the old length.
        "varchar" | "varbit" => new < 0 || (old >= 0 && old <= new),
        // `numeric_support`: the same scale and a precision that is not less.
        "numeric" => {
            const VARHDRSZ: i32 = 4;
            if new < VARHDRSZ {
                return true;
            }
            if old < VARHDRSZ {
                return false;
            }
            let (old, new) = (old - VARHDRSZ, new - VARHDRSZ);
            (old & 0x7ff) == (new & 0x7ff) && ((new >> 16) & 0xffff) >= ((old >> 16) & 0xffff)
        }
        // `TemporalSimplify` with `MAX_TIMESTAMP_PRECISION` and `MAX_TIME_PRECISION`.
        "timestamp_scale" | "timestamptz_scale" | "time_scale" | "timetz_scale" => {
            new < 0 || new == 6 || (old >= 0 && new >= old)
        }
        "interval_scale" => new < 0,
        _ => false,
    }
}

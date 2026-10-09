//! `get_expr_result_tupdesc`, `build_function_result_tupdesc_t` and `expandRecordVariable` of `funcapi.c` and `parse_target.c`: the fields of a value of type `record`, which a field selection such as `(x).a` reads.

use rupg_common::Result;
use rupg_pgcatalog::builtin;

use crate::expr::{Expr, ExprKind, Func};
use crate::from::{Column, Relation};
use crate::{Analyzer, poly, types};

/// `build_function_result_tupdesc_t` and `resolve_polymorphic_tupdesc`: the columns of the rows of a call of a function with two or more `OUT`, `INOUT` or `TABLE` parameters. A parameter without a name gives the name `columnN`, and a polymorphic type takes the actual type that the arguments of the call give. `None` for a function with fewer such parameters.
///
/// # Errors
///
/// The error of a polymorphic type that the arguments do not resolve.
pub fn out_columns(func: &Func) -> Result<Option<Vec<Column>>> {
    let Some(proc) = builtin::proc_by_oid(func.oid) else { return Ok(None) };
    let (Some(all), Some(modes)) = (proc.allargtypes, proc.argmodes) else { return Ok(None) };
    let mut columns: Vec<Column> = Vec::new();
    for (i, (&ty, mode)) in all.iter().zip(modes).enumerate() {
        if !matches!(mode, b'o' | b'b' | b't') {
            continue;
        }
        let name = proc.argnames.and_then(|n| n.get(i)).copied().filter(|n| !n.is_empty());
        let name = name.map_or_else(|| format!("column{}", columns.len() + 1), str::to_string);
        let ty = if types::is_polymorphic(ty) {
            let inputs: Vec<u32> = func.args.iter().map(|a| a.ty).collect();
            poly::resolve(&inputs, &mut proc.argtypes.to_vec(), ty)?
        } else {
            ty
        };
        columns.push(Column { name, ty, typmod: -1, not_null: false });
    }
    Ok((columns.len() > 1).then_some(columns))
}

/// `get_expr_result_tupdesc` and `expandRecordVariable`: the fields of an expression of type `record`. A call of a function gives its `OUT` parameters. A column of a subquery in `FROM` gives the fields of its expression in the subquery, also through more subqueries. `levels` has the relations of the query of the expression, then the relations of each query outside it. `None` when the fields are not known before the query runs.
///
/// # Errors
///
/// The error of a polymorphic type that the arguments of a call do not resolve.
pub fn record_fields(expr: &Expr, levels: &[&[Relation]]) -> Result<Option<Vec<Column>>> {
    match &expr.kind {
        ExprKind::Func(f) => out_columns(f),
        ExprKind::Var(var) => {
            let Some(relation) = levels.get(var.levels_up).and_then(|r| r.get(var.relation)) else {
                return Ok(None);
            };
            if var.attnum == 0 {
                let mut columns = relation.columns.clone();
                if let Some(names) = &relation.alias {
                    for (column, name) in columns.iter_mut().zip(names) {
                        column.name.clone_from(name);
                    }
                }
                return Ok(Some(columns));
            }
            // A view is a relation of the query, so a column of type `record` of a view has no known fields, as in PostgreSQL.
            let (0, Some(query)) = (relation.oid, &relation.subquery) else { return Ok(None) };
            let target = usize::try_from(var.attnum - 1).ok().and_then(|i| query.targets.get(i));
            let Some(target) = target else { return Ok(None) };
            let mut inner = vec![&query.relations[..]];
            inner.extend_from_slice(&levels[var.levels_up..]);
            record_fields(&target.expr, &inner)
        }
        _ => Ok(None),
    }
}

impl Analyzer<'_> {
    /// The fields of an expression of type `record` in the query that the analyzer reads now.
    pub(crate) fn record_columns(&self, expr: &Expr) -> Result<Option<Vec<Column>>> {
        let levels: Vec<&[Relation]> = self.levels().map(|(_, s)| &s.relations[..]).collect();
        record_fields(expr, &levels)
    }
}

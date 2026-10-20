//! `transformCallStmt` of `analyze.c`: the call of a procedure.

use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::CallStmt;
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{FromFunction, FromItem, Relation};
use crate::funcargs::{all_arguments, expand};
use crate::select::{Query, Target};
use crate::typename::{names, place};

/// An argument of `CALL` for an `INOUT` or `OUT` argument of the procedure. A `CALL` in PL/pgSQL writes the value of the argument back to it, so it must be a variable.
#[derive(Clone, Debug, PartialEq)]
pub struct OutArg {
    /// The argument after the coercion to the type of the argument of the procedure.
    pub expr: Expr,
    /// The name of the argument of the procedure, if it has one.
    pub name: Option<String>,
    /// The place of the argument in the arguments of the procedure, from 1.
    pub position: usize,
}

impl Analyzer<'_> {
    /// `transformCallStmt`: `CALL`, as a query that calls the procedure. The arguments of the call go in the order of the procedure, with the defaults, and the call keeps only the input arguments. The expressions of the output arguments are analyzed, but the call does not read them.
    ///
    /// A procedure with no output arguments gives a query with one target, which gives `void`. A procedure with output arguments gives a query that reads the call as a function in `FROM`, with a column for each output argument.
    pub(crate) fn call(&mut self, stmt: &CallStmt) -> Result<Query> {
        let Some(fc) = &stmt.funccall else { return Err(Error::internal("CALL with no call")) };
        let at = place(fc.location);
        let func_names = names(&fc.funcname);
        let args = self.with_kind(Kind::CallArgument, |a| a.transform_list(&fc.args))?;
        let mut call = self.make_func(&func_names, args, fc.func_variadic, None, true, at)?;
        let ExprKind::Func(func) = &mut call.kind else {
            return Err(Error::internal("CALL of a procedure that is not a function call"));
        };
        let proc = builtin::proc_by_oid(func.oid).expect("procedure in pg_proc");
        let args = expand(&func.args, &all_arguments(proc))?.into_owned();
        let out_args = args
            .iter()
            .zip(proc.argmodes.unwrap_or_default())
            .zip(1..)
            .filter(|((_, mode), _)| matches!(**mode, b'b' | b'o'))
            .map(|((arg, _), position)| OutArg {
                expr: arg.clone(),
                name: proc
                    .argnames
                    .and_then(|names| names.get(position - 1))
                    .filter(|name| !name.is_empty())
                    .map(|name| String::from(*name)),
                position,
            })
            .collect();
        func.args = match proc.argmodes {
            Some(modes) => args
                .into_iter()
                .zip(modes)
                .filter(|(_, mode)| **mode != b'o')
                .map(|(arg, _)| arg)
                .collect(),
            None => args,
        };
        let columns = crate::record::out_columns(func)?;
        let name = proc.name.to_string();
        let Some(columns) = columns.filter(|_| call.ty != oid::VOID) else {
            let target = Target { name, expr: call, origin: None, junk: false };
            return Ok(Query { targets: vec![target], out_args, ..Query::default() });
        };
        let targets = columns
            .iter()
            .zip(1..)
            .map(|(column, attnum)| {
                let var = Var { relation: 0, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), column.ty);
                expr.typmod = column.typmod;
                Target { name: column.name.clone(), expr, origin: None, junk: false }
            })
            .collect();
        let function = FromFunction {
            calls: vec![(call, columns.len())],
            ordinality: false,
            coldefs: vec![false],
        };
        let mut relation = Relation::new(0, columns, None, Some(function), None);
        relation.name = name;
        Ok(Query {
            relations: vec![relation],
            from: vec![FromItem::Relation(0)],
            targets,
            out_args,
            ..Query::default()
        })
    }
}

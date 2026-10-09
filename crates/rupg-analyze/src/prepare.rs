//! The parts of `PREPARE` and `EXECUTE` that the analyzer does, as `PrepareQuery` and `EvaluateParams` of `prepare.c` do them.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{List, Node};

use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::select::{Query, Target};
use crate::{Analyzer, Env, Params, types};

/// `PrepareQuery`: the type OIDs of the parameters that `PREPARE` declares, as `typenameTypeId` finds them.
///
/// # Errors
///
/// `42704` for a type that does not exist.
pub fn param_types(names: &List, env: &dyn Env) -> Result<Vec<u32>> {
    let mut analyzer = Analyzer::new(env, &Params::default());
    names
        .iter()
        .map(|node| match node {
            Some(Node::TypeName(name)) => analyzer.type_name(name).map(|(ty, _)| ty),
            _ => Err(Error::new(SqlState::INTERNAL_ERROR, "a parameter of PREPARE is not a type")),
        })
        .collect()
}

/// `EvaluateParams`: the query that gives the values of the parameters of `EXECUTE`. It has no `FROM` and one column for each expression, which is coerced to the type of its parameter in the assignment context. The caller checks the number of the expressions first.
///
/// # Errors
///
/// The errors of the expressions, such as a subquery or an aggregate call, and `42804` for an expression that cannot be coerced to the type of its parameter.
pub fn execute_params(exprs: &List, types: &[u32], env: &dyn Env) -> Result<Query> {
    let mut analyzer = Analyzer::new(env, &Params::default());
    let mut targets = Vec::with_capacity(exprs.len());
    for (i, (node, &ty)) in exprs.iter().zip(types).enumerate() {
        let expr = analyzer.with_kind(Kind::ExecuteParameter, |an| an.transform(node.as_ref()))?;
        let (given, at) = (expr.ty, expr.location);
        let Some(expr) = analyzer.coerce_to_target(expr, ty, -1, Context::Assignment, at)? else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "parameter ${} of type {} cannot be coerced to the expected type {}",
                    i + 1,
                    types::name(given),
                    types::name(ty)
                ),
            )
            .with_hint("You will need to rewrite or cast the expression.")
            .at_opt(at));
        };
        targets.push(Target { name: "?column?".into(), expr, origin: None, junk: false });
    }
    Ok(Query {
        relations: Vec::new(),
        from: Vec::new(),
        targets,
        filter: None,
        grouped: false,
        group: Vec::new(),
        having: None,
        sort: Vec::new(),
        distinct: Vec::new(),
        distinct_on: false,
        offset: None,
        limit: None,
        with_ties: false,
        params: Vec::new(),
        notices: analyzer.notices,
        set_op: None,
        windows: Vec::new(),
    })
}

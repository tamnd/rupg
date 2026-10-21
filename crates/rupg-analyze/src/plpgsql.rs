//! The hooks of PL/pgSQL in the analysis: the variables of a function that a query names, as `plpgsql_post_column_ref` and `plpgsql_param_ref` of `pl_comp.c` find them, and the casts of the values of the variables, as `exec_cast_value` of `pl_exec.c` makes them.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{ColumnRef, List, Node, RangeVar};
use rupg_types::oid;

use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::select::{Query, Target};
use crate::{Analyzer, Env, Params, types};

/// The target of an assignment of PL/pgSQL, whose type the value takes in the analysis.
pub(crate) struct Assign {
    pub(crate) ty: u32,
    pub(crate) typmod: i32,
    /// The name of the variable, for the errors.
    pub(crate) name: String,
    /// The variable and the subscripts after it, for an assignment to a part of the variable, as `x[1] := 5`.
    pub(crate) indirect: Option<(Expr, List)>,
}

/// A variable of a function that a query reads as a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variable {
    /// The number of the parameter, from 1.
    pub param: usize,
    pub ty: u32,
    pub typmod: i32,
}

/// The variables of a PL/pgSQL function that the queries of the function can name.
pub trait Variables {
    /// `resolve_column_ref`: the variable that the names of a column reference name, such as `x`, `r.f` or `block.x`, or `None` when no variable has the names. `*` is not a name here.
    ///
    /// # Errors
    ///
    /// The error of a field that a record does not have, and of a record with no value yet.
    fn column(&self, names: &[&str], at: Option<usize>) -> Result<Option<Variable>>;
    /// `plpgsql_param_ref`: the variable of `$n`, which is the argument `n` of the function, or `None`.
    fn param(&self, number: usize) -> Option<Variable>;
}

/// [`crate::analyze`] of a query of a PL/pgSQL function, whose names can be variables of the function. A name that is a column and a variable is an error, as `plpgsql.variable_conflict` with the value `error` gives it.
///
/// # Errors
///
/// The errors of the analysis, and `42702` for a name that is a column and a variable.
pub fn analyze_with(
    stmt: &Node,
    env: &dyn Env,
    params: &Params,
    variables: &dyn Variables,
) -> Result<Query> {
    crate::analyze_hooked(stmt, env, params, Some(variables))
}

/// `transformPLAssignStmt`: [`analyze_with`] of the value of an assignment to the variable `name` of the type `ty` with the typmod `typmod`. The value takes the type of the variable with the casts of PL/pgSQL, so that the error of a cast points into the assignment.
///
/// # Errors
///
/// The errors of [`analyze_with`], `42601` for a value that is not one column and `42804` for a value that has no cast to the type of the variable.
pub fn analyze_assign(
    stmt: &Node,
    env: &dyn Env,
    variables: &dyn Variables,
    (ty, typmod): (u32, i32),
    name: &str,
) -> Result<Query> {
    let assign = Assign { ty, typmod, name: name.to_owned(), indirect: None };
    crate::analyze_assigned(Analyzer::new(env, &Params::default()), stmt, variables, assign)
}

/// `transformPLAssignStmt` of an assignment to a part of a variable, as `x[i] := 5` and `r.f[1:2] := a`: [`analyze_with`] of the `PLAssignStmt` that [`rupg_sql::parse_plpgsql_assign`] gives. The value of the query is the new value of the variable, which is the array with the new value of the element or of the slice.
///
/// # Errors
///
/// The errors of [`analyze_with`], `42601` for a value that is not one column, and `42804` for a target that has no subscripts and for a value that has no cast to the type of the element or of the slice.
pub fn analyze_assign_part(stmt: &Node, env: &dyn Env, variables: &dyn Variables) -> Result<Query> {
    let Node::PLAssignStmt(s) = stmt else {
        return Err(Error::internal("an assignment is not a PLAssignStmt"));
    };
    let Some(val) = &s.val else { return Err(Error::internal("an assignment has no value")) };
    let name = s.name.as_deref().unwrap_or_default();
    // The names after the first name of the target are at the start of the indirection.
    let split = usize::try_from(s.nnames).unwrap_or(1).saturating_sub(1).min(s.indirection.len());
    let (names, indirection) = s.indirection.split_at(split);
    let mut fields = vec![Some(Node::String(name.into()))];
    fields.extend(names.iter().cloned());
    let cref = Node::ColumnRef(Box::new(ColumnRef { fields, location: s.location }));
    let mut analyzer = Analyzer::new(env, &Params::default());
    analyzer.variables = Some(variables);
    let target = analyzer.transform(Some(&cref))?;
    let (ty, typmod) = (target.ty, target.typmod);
    let indirect = Some((target, indirection.to_vec()));
    let assign = Assign { ty, typmod, name: name.to_owned(), indirect };
    crate::analyze_assigned(analyzer, &Node::SelectStmt(val.clone()), variables, assign)
}

impl Analyzer<'_> {
    /// `transformPLAssignStmtTarget`: the value of an assignment, coerced to the type of the variable. A row value for a row variable of another row type stays as it is, and the executor converts it.
    pub(crate) fn assign_target(&mut self, targets: &mut [Target], assign: Assign) -> Result<()> {
        let Assign { ty, typmod, name, indirect } = assign;
        let [target] = targets else {
            let n = targets.len();
            let word = if n == 1 { "column" } else { "columns" };
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("assignment source returned {n} {word}"),
            ));
        };
        if let Some((base, indirection)) = indirect {
            let rhs = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
            let at = base.place();
            let parts = (base, &name[..], (ty, typmod));
            target.expr =
                self.assignment_indirection(parts, &indirection, rhs, Context::Plpgsql, at)?;
            return Ok(());
        }
        let source = target.expr.ty;
        let composite = |t: u32| t == oid::RECORD || types::is_complex(t);
        if ty != source && composite(ty) && composite(source) {
            return Ok(());
        }
        let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
        let at = expr.location;
        let Some(expr) = self.coerce_to_target(expr, ty, typmod, Context::Plpgsql, None)? else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "variable \"{name}\" is of type {} but expression is of type {}",
                    types::name(ty),
                    types::name(source)
                ),
            )
            .with_hint("You will need to rewrite or cast the expression.")
            .at_opt(at));
        };
        target.expr = expr;
        Ok(())
    }
}

/// `get_cast_hashentry`: the query `SELECT $1` that casts a value of the type `source` to the type `target` with the typmod `typmod`, in the context of PL/pgSQL, where the output and the input functions cast a value when no other cast exists.
///
/// # Errors
///
/// `42846` when there is no cast, as for a type that is `internal`.
pub fn plpgsql_cast(env: &dyn Env, source: (u32, i32), target: u32, typmod: i32) -> Result<Query> {
    let mut analyzer = Analyzer::new(env, &Params { types: vec![source.0], variable: false });
    let param = Expr { kind: ExprKind::Param(1), ty: source.0, typmod: source.1, location: None };
    let Some(expr) = analyzer.coerce_to_target(param, target, typmod, Context::Plpgsql, None)?
    else {
        return Err(Error::new(
            SqlState::CANNOT_COERCE,
            format!("cannot cast type {} to {}", types::name(source.0), types::name(target)),
        ));
    };
    let target = Target { name: "?column?".into(), expr, origin: None, junk: false };
    Ok(Query { targets: vec![target], params: vec![source.0], ..Query::default() })
}

/// The name, the type and the typmod of each field of a row type, as `build_row_from_rowtype` reads them, or `None` for a type that is not the row type of a relation. A dropped column has no name and the type 0.
pub fn row_type_fields(env: &dyn Env, ty: u32) -> Option<Vec<(String, u32, i32)>> {
    let analyzer = Analyzer::new(env, &Params::default());
    let columns = analyzer.row_columns(ty)?;
    Some(columns.into_iter().map(|c| (c.name, c.ty, c.typmod)).collect())
}

/// The OID of the relation with the qualified name `names`, as `RangeVarGetRelid` finds it with the search path, or `None`.
///
/// # Errors
///
/// `0A000` for a name with a database that is not the current database.
pub fn relation_by_names(env: &dyn Env, names: &[&str]) -> Result<Option<u32>> {
    let mut rv = RangeVar::default();
    match names {
        [rel] => rv.relname = Some((*rel).into()),
        [schema, rel] => {
            rv.schemaname = Some((*schema).into());
            rv.relname = Some((*rel).into());
        }
        [database, schema, rel] => {
            rv.catalogname = Some((*database).into());
            rv.schemaname = Some((*schema).into());
            rv.relname = Some((*rel).into());
        }
        _ => return Ok(None),
    }
    Analyzer::new(env, &Params::default()).lookup_relation(&rv)
}

/// `plpgsql_parse_cwordtype`: the type and the typmod of the column `column` of the relation with the qualified name `names`, or `None` when the relation or the column does not exist.
///
/// # Errors
///
/// The errors of [`relation_by_names`].
pub fn column_type(env: &dyn Env, names: &[&str], column: &str) -> Result<Option<(u32, i32)>> {
    let Some(relid) = relation_by_names(env, names)? else { return Ok(None) };
    let analyzer = Analyzer::new(env, &Params::default());
    let Ok(columns) = analyzer.open_relation(relid, "", None) else { return Ok(None) };
    Ok(columns.into_iter().find(|c| c.name == column).map(|c| (c.ty, c.typmod)))
}

/// `plpgsql_parse_wordrowtype`: the row type of the relation with the qualified name `names`, or `None`.
///
/// # Errors
///
/// The errors of [`relation_by_names`].
pub fn relation_row_type(env: &dyn Env, names: &[&str]) -> Result<Option<u32>> {
    let Some(relid) = relation_by_names(env, names)? else { return Ok(None) };
    if let Some(rel) = env.catalog().and_then(|c| c.relation(relid)) {
        return Ok(Some(rel.row_type).filter(|&ty| ty != 0));
    }
    Ok(rupg_pgcatalog::builtin::types().iter().find(|t| t.relid == relid).map(|t| t.oid))
}

impl Analyzer<'_> {
    /// `plpgsql_post_column_ref` around `transformColumnRef`: the column, or the variable when no column has the name. A name that is both is an error.
    pub(crate) fn column_or_variable(
        &mut self,
        c: &ColumnRef,
        variables: &dyn Variables,
    ) -> Result<Expr> {
        let at = crate::typename::place(c.location);
        let names: Option<Vec<&str>> = c
            .fields
            .iter()
            .map(|f| match f {
                Some(Node::String(s)) => Some(&**s),
                _ => None,
            })
            .collect();
        let core = self.column_ref(c);
        let missing =
            |e: &Error| matches!(e.state(), SqlState::UNDEFINED_COLUMN | SqlState::UNDEFINED_TABLE);
        let Some(names) = names else {
            // `resolve_column_ref` for `r.*`: the whole value of a variable of a composite type.
            if let Some((Some(Node::A_Star(_)), prefix)) = c.fields.split_last()
                && let Err(error) = &core
                && missing(error)
            {
                let prefix: Option<Vec<&str>> = prefix
                    .iter()
                    .map(|f| match f {
                        Some(Node::String(s)) => Some(&**s),
                        _ => None,
                    })
                    .collect();
                if let Some(prefix) = prefix.filter(|p| !p.is_empty())
                    && let Some(variable) = self.composite_variable(&prefix, at)?
                {
                    return Ok(self.variable_param(variable, at));
                }
            }
            return core;
        };
        match core {
            Ok(expr) => match variables.column(&names, at)? {
                Some(_) => Err(Error::new(
                    SqlState::AMBIGUOUS_COLUMN,
                    format!("column reference \"{}\" is ambiguous", names.join(".")),
                )
                .with_detail("It could refer to either a PL/pgSQL variable or a table column.")
                .at_opt(at)),
                None => Ok(expr),
            },
            Err(error) if missing(&error) => match variables.column(&names, at)? {
                Some(variable) => Ok(self.variable_param(variable, at)),
                None => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// The variable of the PL/pgSQL function with the names `names` when its type is composite or `record`, as `resolve_column_ref` takes a row or a record for `r.*`, or `None`.
    pub(crate) fn composite_variable(
        &self,
        names: &[&str],
        at: Option<usize>,
    ) -> Result<Option<Variable>> {
        let Some(variables) = self.variables else { return Ok(None) };
        let Some(variable) = variables.column(names, at)? else { return Ok(None) };
        let composite =
            variable.ty == oid::RECORD || self.row_columns(types::base(variable.ty)).is_some();
        Ok(composite.then_some(variable))
    }

    /// The parameter of a variable. The parameters of the query grow to its number, and it gets the type of the variable.
    pub(crate) fn variable_param(&mut self, variable: Variable, at: Option<usize>) -> Expr {
        if self.params.len() < variable.param {
            self.params.resize(variable.param, 0);
        }
        self.params[variable.param - 1] = variable.ty;
        Expr {
            kind: ExprKind::Param(variable.param),
            ty: variable.ty,
            typmod: variable.typmod,
            location: at,
        }
    }
}

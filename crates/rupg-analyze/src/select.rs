//! `transformSelectStmt` of `analyze.c`, for a `SELECT` that has no `FROM`: the target list and the `WHERE` condition.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, SelectStmt, SetOperation};
use rupg_types::oid;

use crate::Analyzer;
use crate::coerce::Context;
use crate::colname::figure_colname;
use crate::expr::Expr;

/// The query tree of a `SELECT`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// The columns of the result.
    pub targets: Vec<Target>,
    /// The `WHERE` condition, of type `boolean`.
    pub filter: Option<Expr>,
    /// The type OID of each parameter, from `$1`.
    pub params: Vec<u32>,
    /// The warnings and notices of the analysis, which the session sends before the result.
    pub notices: Vec<Error>,
}

/// A column of the result.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The name of the column in `RowDescription`.
    pub name: String,
    pub expr: Expr,
}

/// The error of a clause that the analyzer does not take yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

impl Analyzer<'_> {
    /// The query tree of a `SELECT`.
    pub(crate) fn select(&mut self, s: &SelectStmt) -> Result<Query> {
        if s.op != SetOperation::SETOP_NONE {
            return Err(not_yet("UNION, INTERSECT and EXCEPT"));
        }
        if !s.valuesLists.is_empty() {
            return Err(not_yet("VALUES"));
        }
        if s.withClause.is_some() {
            return Err(not_yet("WITH"));
        }
        if s.intoClause.is_some() {
            return Err(not_yet("SELECT INTO"));
        }
        if !s.fromClause.is_empty() {
            return Err(not_yet("SELECT with FROM"));
        }
        let mut targets = Vec::with_capacity(s.targetList.len());
        for target in &s.targetList {
            let Some(Node::ResTarget(rt)) = target else {
                return Err(Error::internal("a target that is not ResTarget"));
            };
            let expr = self.transform(rt.val.as_ref())?;
            let name = match &rt.name {
                Some(name) => name.to_string(),
                None => figure_colname(rt.val.as_ref()),
            };
            targets.push(Target { name, expr });
        }
        let filter = match &s.whereClause {
            Some(node) => {
                let expr = self.transform(Some(node))?;
                Some(self.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        if s.havingClause.is_some() || !s.groupClause.is_empty() {
            return Err(not_yet("GROUP BY and HAVING"));
        }
        if !s.windowClause.is_empty() {
            return Err(not_yet("WINDOW"));
        }
        if !s.sortClause.is_empty() {
            return Err(not_yet("ORDER BY"));
        }
        if !s.distinctClause.is_empty() {
            return Err(not_yet("DISTINCT"));
        }
        if s.limitCount.is_some() || s.limitOffset.is_some() {
            return Err(not_yet("LIMIT and OFFSET"));
        }
        if !s.lockingClause.is_empty() {
            return Err(not_yet("FOR UPDATE and FOR SHARE"));
        }
        // `resolveTargetListUnknowns`: a column of type `unknown` becomes `text`.
        for target in &mut targets {
            if target.expr.ty == oid::UNKNOWN {
                let expr = std::mem::replace(
                    &mut target.expr,
                    Expr::new(crate::expr::ExprKind::CaseTest, 0),
                );
                target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
            }
        }
        Ok(Query { targets, filter, params: Vec::new(), notices: Vec::new() })
    }
}

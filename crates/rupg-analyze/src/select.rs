//! `transformSelectStmt` of `analyze.c`: `FROM`, the target list and the `WHERE` condition.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, SelectStmt, SetOperation};
use rupg_types::oid;

use crate::Analyzer;
use crate::coerce::Context;
use crate::colname::figure_colname;
use crate::expr::{Expr, ExprKind};
use crate::from::{FromItem, Relation};

/// The query tree of a `SELECT`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// The relations of `FROM`, which a [`crate::Var`] names by index.
    pub relations: Vec<Relation>,
    /// The parts of `FROM`. The query joins them with a cross join.
    pub from: Vec<FromItem>,
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
    /// The OID of the table and the attribute number of the column when the target is a plain column, as `resorigtbl` and `resorigcol`.
    pub origin: Option<(u32, i16)>,
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
        let from = self.transform_from(&s.fromClause)?;
        let mut targets = Vec::with_capacity(s.targetList.len());
        for target in &s.targetList {
            let Some(Node::ResTarget(rt)) = target else {
                return Err(Error::internal("a target that is not ResTarget"));
            };
            if let Some(Node::ColumnRef(c)) = &rt.val
                && let Some(columns) = self.expand_star(c)?
            {
                for (name, expr) in columns {
                    targets.push(Target { name, expr, origin: None });
                }
                continue;
            }
            let expr = self.transform(rt.val.as_ref())?;
            let name = match &rt.name {
                Some(name) => name.to_string(),
                None => figure_colname(rt.val.as_ref()),
            };
            targets.push(Target { name, expr, origin: None });
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
                let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
                target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
            }
        }
        // markTargetListOrigins: a target that is a column of a relation names the table and the column.
        let relations = std::mem::take(&mut self.scope.relations);
        for target in &mut targets {
            if let ExprKind::Var(var) = &target.expr.kind {
                target.origin = Some((relations[var.relation].oid, var.attnum));
            }
        }
        Ok(Query { relations, from, targets, filter, params: Vec::new(), notices: Vec::new() })
    }
}

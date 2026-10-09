//! `transformValuesClause` of `analyze.c`: a `VALUES` list as a query.
//!
//! The rows of the list become a relation of the query, and the targets read its columns `column1`, `column2` and so on. Each column has the type that `select_common_type` gives for the expressions of the column in all the rows.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{LimitOption, Node, SelectStmt};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::Expr;
use crate::from::{Column, FromItem, Relation};
use crate::select::{Query, Target};
use crate::setop::strength_name;

impl Analyzer<'_> {
    /// `transformValuesClause`: the query tree of a `VALUES` list, with its `ORDER BY`, `LIMIT` and `OFFSET`.
    pub(crate) fn values_clause(&mut self, s: &SelectStmt) -> Result<Query> {
        if s.withClause.is_some() {
            return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "WITH is not supported yet"));
        }
        let mut columns: Vec<Vec<Expr>> = Vec::new();
        for (row, list) in s.valuesLists.iter().enumerate() {
            let Some(Node::List(list)) = list else {
                return Err(Error::internal("a VALUES row that is not a list"));
            };
            let mut exprs = Vec::with_capacity(list.len());
            for node in list {
                exprs.push(self.with_kind(Kind::Values, |a| a.transform(node.as_ref()))?);
            }
            if row == 0 {
                columns = vec![Vec::with_capacity(s.valuesLists.len()); exprs.len()];
            } else if exprs.len() != columns.len() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "VALUES lists must all be the same length",
                )
                .at_opt(exprs.iter().filter_map(Expr::place).min()));
            }
            for (column, expr) in columns.iter_mut().zip(exprs) {
                column.push(expr);
            }
        }
        let mut types = Vec::with_capacity(columns.len());
        for column in &mut columns {
            let ty = self.common_type(column, "VALUES")?;
            for expr in column.iter_mut() {
                let old = std::mem::replace(expr, Expr::new(crate::ExprKind::CaseTest, 0));
                *expr = self.coerce_to_common(old, ty, "VALUES")?;
            }
            // select_common_typmod: the typmod of the expressions when they all have the same one.
            let first = column.first().map_or(-1, |e| e.typmod);
            let typmod = if column.iter().all(|e| e.typmod == first) { first } else { -1 };
            types.push((ty, typmod));
        }
        let mut rows: Vec<Vec<Expr>> = vec![Vec::with_capacity(columns.len()); s.valuesLists.len()];
        for column in columns {
            for (row, expr) in rows.iter_mut().zip(column) {
                row.push(expr);
            }
        }
        let columns: Vec<Column> = types
            .iter()
            .enumerate()
            .map(|(i, &(ty, typmod))| Column {
                name: format!("column{}", i + 1),
                ty,
                typmod,
                not_null: false,
            })
            .collect();
        let relation = Relation::new(0, columns, None, None, Some(rows));
        let (index, columns) = self.add_values(relation)?;
        // expandNSItemAttrs: the targets are the columns, with no location.
        let mut targets: Vec<Target> = columns
            .into_iter()
            .map(|(name, expr)| Target { name, expr, origin: None, junk: false })
            .collect();
        let sort = self.sort_clause(&s.sortClause, &mut targets)?;
        let with_ties = s.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES;
        let offset = self.with_kind(Kind::Offset, |a| {
            a.limit_clause(s.limitOffset.as_ref(), "OFFSET", with_ties)
        })?;
        let limit = self.with_kind(Kind::Limit, |a| {
            a.limit_clause(s.limitCount.as_ref(), "LIMIT", with_ties)
        })?;
        if let Some(Some(Node::LockingClause(lock))) = s.lockingClause.first() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("{} cannot be applied to VALUES", strength_name(lock.strength)),
            ));
        }
        let relations = std::mem::take(&mut self.scope.relations);
        Ok(Query {
            relations,
            from: vec![FromItem::Relation(index)],
            targets,
            filter: None,
            grouped: false,
            group: Vec::new(),
            having: None,
            sort,
            distinct: Vec::new(),
            distinct_on: false,
            offset,
            limit,
            with_ties,
            params: Vec::new(),
            notices: Vec::new(),
            set_op: None,
        })
    }
}

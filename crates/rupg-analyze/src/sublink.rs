//! `transformSubLink` of `parse_expr.c`: a subquery in an expression, as `EXISTS`, `ANY`, `ALL`, `IN`, `ARRAY` or a scalar subquery.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, SubLink as RawSubLink, SubLinkType};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, SubLink, SubLinkKind};
use crate::select::Query;
use crate::typename::{names, place};
use crate::types;

/// An error for a subquery that the analyzer does not take yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

impl Analyzer<'_> {
    /// `parse_sub_analyze`: the query tree of a subquery. The subquery sees the names of this query and of the queries outside it.
    fn sub_analyze(&mut self, node: Option<&Node>) -> Result<Query> {
        let Some(Node::SelectStmt(stmt)) = node else {
            return Err(Error::internal("a subquery that is not SelectStmt"));
        };
        let scope = std::mem::take(&mut self.scope);
        self.outer.push(scope);
        let kind = std::mem::replace(&mut self.kind, Kind::Other);
        let has_aggs = std::mem::replace(&mut self.has_aggs, false);
        let srfs = self.srfs;
        let result = self.select(stmt);
        self.srfs = srfs;
        self.has_aggs = has_aggs;
        self.kind = kind;
        self.scope = self.outer.pop().unwrap_or_default();
        result
    }

    /// `transformSubLink`.
    pub(crate) fn transform_sublink(&mut self, s: &RawSubLink) -> Result<Expr> {
        let at = place(s.location);
        if self.kind.is_standalone() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot use subquery in {}", self.kind.subquery_name()),
            )
            .at_opt(at));
        }
        let kind = match s.subLinkType {
            SubLinkType::EXISTS_SUBLINK => SubLinkKind::Exists,
            SubLinkType::ALL_SUBLINK => SubLinkKind::All,
            SubLinkType::ANY_SUBLINK => SubLinkKind::Any,
            SubLinkType::EXPR_SUBLINK => SubLinkKind::Expr,
            SubLinkType::ARRAY_SUBLINK => SubLinkKind::Array,
            SubLinkType::ROWCOMPARE_SUBLINK => {
                return Err(not_yet("a row comparison with a subquery", at));
            }
            _ => return Err(not_yet("this kind of subquery", at)),
        };
        let query = self.sub_analyze(s.subselect.as_ref())?;
        let columns: Vec<(u32, i32)> =
            query.targets.iter().filter(|t| !t.junk).map(|t| (t.expr.ty, t.expr.typmod)).collect();
        let (ty, typmod, test) = match kind {
            SubLinkKind::Exists => (oid::BOOL, -1, None),
            SubLinkKind::Expr | SubLinkKind::Array => {
                let [(ty, typmod)] = columns[..] else {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "subquery must return only one column",
                    )
                    .at_opt(at));
                };
                if kind == SubLinkKind::Expr {
                    (ty, typmod, None)
                } else if types::element(ty) != 0 {
                    return Err(not_yet("ARRAY() of an array", at));
                } else {
                    let array = types::array_of(ty);
                    if array == 0 {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("could not find array type for data type {}", types::name(ty)),
                        ));
                    }
                    (array, typmod, None)
                }
            }
            SubLinkKind::Any | SubLinkKind::All => {
                if let Some(Node::RowExpr(r)) = &s.testexpr {
                    return Err(not_yet("a row on the left of a subquery", place(r.location)));
                }
                let left = self.transform(s.testexpr.as_ref())?;
                let op_names = if s.operName.is_empty() { vec!["="] } else { names(&s.operName) };
                match columns.len() {
                    0 => {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            "subquery has too few columns",
                        )
                        .at_opt(at));
                    }
                    1 => {}
                    _ => {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            "subquery has too many columns",
                        )
                        .at_opt(at));
                    }
                }
                let (ty, typmod) = columns[0];
                let param = Expr { kind: ExprKind::SubColumn(0), ty, typmod, location: None };
                let test = self.make_op(&op_names, Some(left), param, at)?;
                if test.ty != oid::BOOL {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "row comparison operator must yield type boolean, not type {}",
                            types::name(test.ty)
                        ),
                    )
                    .at_opt(at));
                }
                (oid::BOOL, -1, Some(test))
            }
        };
        let sub = SubLink { kind, test, query };
        Ok(Expr { kind: ExprKind::SubLink(Box::new(sub)), ty, typmod, location: at })
    }
}

//! `FigureColname`: the name of a column of the result when the query gives no `AS` name, from the raw expression as `parse_target.c` finds it.

use rupg_sql::nodes::{
    A_Expr_Kind, MinMaxOp, Node, SQLValueFunctionOp, SelectStmt, SetOperation, SubLinkType,
};

/// The name of the column for the expression, or `?column?`.
pub fn figure_colname(node: Option<&Node>) -> String {
    match figure(node) {
        Some((name, _)) => name,
        None => "?column?".to_string(),
    }
}

/// `FigureIndexColname`: the name of a column of an index for an expression, or `None` when the caller uses `expr`.
pub(crate) fn figure_index_colname(node: Option<&Node>) -> Option<String> {
    figure(node).map(|(name, _)| name)
}

/// The last `String` node of a list.
fn last_string(list: &[Option<Node>]) -> Option<String> {
    list.iter().rev().find_map(|n| match n {
        Some(Node::String(s)) => Some(s.to_string()),
        _ => None,
    })
}

/// The leftmost `SELECT` of a set operation, or the `SELECT` itself.
fn leftmost(mut select: &SelectStmt) -> &SelectStmt {
    while select.op != SetOperation::SETOP_NONE
        && let Some(left) = select.larg.as_deref()
    {
        select = left;
    }
    select
}

/// `FigureColnameInternal`: the name with its strength. A name of strength 2 is a real name. A name of strength 1, such as the type of a cast, is used only when nothing gives a better one.
fn figure(node: Option<&Node>) -> Option<(String, u8)> {
    let strong = |name: &str| Some((name.to_string(), 2));
    match node? {
        Node::ColumnRef(c) => last_string(&c.fields).map(|n| (n, 2)),
        Node::A_Indirection(i) => match last_string(&i.indirection) {
            Some(n) => Some((n, 2)),
            None => figure(i.arg.as_ref()),
        },
        Node::FuncCall(f) => last_string(&f.funcname).map(|n| (n, 2)),
        Node::A_Expr(a) if a.kind == A_Expr_Kind::AEXPR_NULLIF => strong("nullif"),
        Node::TypeCast(t) => {
            let inner = figure(t.arg.as_ref());
            match (&inner, &t.typeName) {
                (Some((_, 2)), _) => inner,
                (_, Some(name)) => last_string(&name.names).map(|n| (n, 1)).or(inner),
                _ => inner,
            }
        }
        Node::CollateClause(c) => figure(c.arg.as_ref()),
        Node::GroupingFunc(_) => strong("grouping"),
        Node::MergeSupportFunc(_) => strong("merge_action"),
        Node::SubLink(s) => match s.subLinkType {
            SubLinkType::EXISTS_SUBLINK => strong("exists"),
            SubLinkType::ARRAY_SUBLINK => strong("array"),
            SubLinkType::EXPR_SUBLINK => match &s.subselect {
                // The query of a set operation has the names of its leftmost `SELECT`.
                Some(Node::SelectStmt(select)) => match leftmost(select).targetList.first() {
                    Some(Some(Node::ResTarget(t))) => match &t.name {
                        Some(name) => strong(name),
                        None => Some((figure_colname(t.val.as_ref()), 2)),
                    },
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        },
        Node::CaseExpr(c) => match figure(c.defresult.as_ref()) {
            Some((name, 2)) => Some((name, 2)),
            _ => Some(("case".to_string(), 1)),
        },
        Node::A_ArrayExpr(_) => strong("array"),
        Node::RowExpr(_) => strong("row"),
        Node::CoalesceExpr(_) => strong("coalesce"),
        Node::MinMaxExpr(m) => match m.op {
            MinMaxOp::IS_GREATEST => strong("greatest"),
            MinMaxOp::IS_LEAST => strong("least"),
            _ => None,
        },
        Node::SQLValueFunction(f) => strong(match f.op {
            SQLValueFunctionOp::SVFOP_CURRENT_DATE => "current_date",
            SQLValueFunctionOp::SVFOP_CURRENT_TIME | SQLValueFunctionOp::SVFOP_CURRENT_TIME_N => {
                "current_time"
            }
            SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP
            | SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N => "current_timestamp",
            SQLValueFunctionOp::SVFOP_LOCALTIME | SQLValueFunctionOp::SVFOP_LOCALTIME_N => {
                "localtime"
            }
            SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP
            | SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP_N => "localtimestamp",
            SQLValueFunctionOp::SVFOP_CURRENT_ROLE => "current_role",
            SQLValueFunctionOp::SVFOP_CURRENT_USER => "current_user",
            SQLValueFunctionOp::SVFOP_USER => "user",
            SQLValueFunctionOp::SVFOP_SESSION_USER => "session_user",
            SQLValueFunctionOp::SVFOP_CURRENT_CATALOG => "current_catalog",
            _ => "current_schema",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(sql: &str) -> String {
        let (stmts, _) = rupg_sql::parse(sql).expect("parse");
        let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement") };
        let Some(Node::SelectStmt(select)) = &raw.stmt else { panic!("not a SELECT") };
        let Some(Some(Node::ResTarget(target))) = select.targetList.first() else {
            panic!("no target")
        };
        figure_colname(target.val.as_ref())
    }

    #[test]
    fn names() {
        crate::tests::big_stack(names_cases);
    }

    fn names_cases() {
        assert_eq!(name("SELECT 1"), "?column?");
        assert_eq!(name("SELECT 1::int"), "int4");
        assert_eq!(name("SELECT 'a'::text::varchar"), "varchar");
        assert_eq!(name("SELECT lower('A')::text"), "lower");
        assert_eq!(name("SELECT pg_catalog.upper('a')"), "upper");
        assert_eq!(name("SELECT CASE WHEN true THEN 1 END"), "case");
        assert_eq!(name("SELECT CASE WHEN true THEN 1 ELSE abs(2) END"), "abs");
        assert_eq!(name("SELECT NULLIF(1, 2)"), "nullif");
        assert_eq!(name("SELECT COALESCE(1, 2)"), "coalesce");
        assert_eq!(name("SELECT GREATEST(1, 2)"), "greatest");
        assert_eq!(name("SELECT ARRAY[1]"), "array");
        assert_eq!(name("SELECT CURRENT_TIMESTAMP(2)"), "current_timestamp");
        assert_eq!(name("SELECT current_catalog"), "current_catalog");
        assert_eq!(name("SELECT x.y"), "y");
        assert_eq!(name("SELECT 1 + 2"), "?column?");
    }
}

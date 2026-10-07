//! The functions of `nodeFuncs.c` that the actions of `gram.y` use, for the node types of the raw parse tree.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/funcs.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::nodes::*;

/// `leftmostLoc`: the smaller of two locations, where -1 is unknown.
fn leftmostLoc(loc1: i32, loc2: i32) -> i32 {
    if loc1 < 0 {
        loc2
    } else if loc2 < 0 {
        loc1
    } else {
        loc1.min(loc2)
    }
}

/// `exprLocation` of a node that can be `NULL`.
pub(crate) fn exprLocation(expr: Option<&Node>) -> i32 {
    expr.map_or(-1, nodeLocation)
}

/// `exprLocation` of a list: the location of its first element that has one.
pub(super) fn listLocation(list: &List) -> i32 {
    list.iter().map(|item| exprLocation(item.as_ref())).find(|&loc| loc >= 0).unwrap_or(-1)
}

/// `exprLocation`: the location of the leftmost token of a node, or -1. The node types that are not in a raw parse tree are not here, and their location is -1, as for any other node type in C.
fn nodeLocation(expr: &Node) -> i32 {
    match expr {
        Node::RangeVar(n) => n.location,
        Node::GroupingFunc(n) => n.location,
        Node::MergeSupportFunc(n) => n.location,
        Node::NamedArgExpr(n) => leftmostLoc(n.location, exprLocation(n.arg.as_ref())),
        Node::BoolExpr(n) => leftmostLoc(n.location, listLocation(&n.args)),
        Node::SubLink(n) => leftmostLoc(exprLocation(n.testexpr.as_ref()), n.location),
        Node::CaseExpr(n) => n.location,
        Node::CaseWhen(n) => n.location,
        Node::RowExpr(n) => n.location,
        Node::CoalesceExpr(n) => n.location,
        Node::MinMaxExpr(n) => n.location,
        Node::SQLValueFunction(n) => n.location,
        Node::XmlExpr(n) => leftmostLoc(n.location, listLocation(&n.args)),
        Node::JsonFormat(n) => n.location,
        Node::JsonValueExpr(n) => exprLocation(n.raw_expr.as_ref()),
        Node::JsonIsPredicate(n) => n.location,
        Node::JsonBehavior(n) => exprLocation(n.expr.as_ref()),
        Node::NullTest(n) => leftmostLoc(n.location, exprLocation(n.arg.as_ref())),
        Node::BooleanTest(n) => leftmostLoc(n.location, exprLocation(n.arg.as_ref())),
        Node::SetToDefault(n) => n.location,
        Node::IntoClause(n) => n.rel.as_ref().map_or(-1, |rel| rel.location),
        Node::List(list) => listLocation(list),
        Node::A_Expr(n) => leftmostLoc(n.location, exprLocation(n.lexpr.as_ref())),
        Node::ColumnRef(n) => n.location,
        Node::ParamRef(n) => n.location,
        Node::A_Const(n) => n.location,
        Node::FuncCall(n) => leftmostLoc(n.location, listLocation(&n.args)),
        Node::A_ArrayExpr(n) => n.location,
        Node::ResTarget(n) => n.location,
        Node::MultiAssignRef(n) => exprLocation(n.source.as_ref()),
        Node::TypeCast(n) => {
            let loc = exprLocation(n.arg.as_ref());
            let loc = leftmostLoc(loc, n.typeName.as_ref().map_or(-1, |t| t.location));
            leftmostLoc(loc, n.location)
        }
        Node::CollateClause(n) => exprLocation(n.arg.as_ref()),
        Node::SortBy(n) => exprLocation(n.node.as_ref()),
        Node::WindowDef(n) => n.location,
        Node::RangeTableSample(n) => n.location,
        Node::TypeName(n) => n.location,
        Node::ColumnDef(n) => n.location,
        Node::IndexElem(n) => n.location,
        Node::Constraint(n) => n.location,
        Node::FunctionParameter(n) => n.location,
        Node::XmlSerialize(n) => n.location,
        Node::GroupingSet(n) => n.location,
        Node::WithClause(n) => n.location,
        Node::InferClause(n) => n.location,
        Node::OnConflictClause(n) => n.location,
        Node::CTESearchClause(n) => n.location,
        Node::CTECycleClause(n) => n.location,
        Node::CommonTableExpr(n) => n.location,
        Node::JsonKeyValue(n) => exprLocation(n.key.as_ref()),
        Node::JsonObjectConstructor(n) => n.location,
        Node::JsonArrayConstructor(n) => n.location,
        Node::JsonArrayQueryConstructor(n) => n.location,
        Node::JsonAggConstructor(n) => n.location,
        Node::JsonObjectAgg(n) => n.constructor.as_ref().map_or(-1, |c| c.location),
        Node::JsonArrayAgg(n) => n.constructor.as_ref().map_or(-1, |c| c.location),
        Node::PartitionElem(n) => n.location,
        Node::PartitionSpec(n) => n.location,
        Node::PartitionBoundSpec(n) => n.location,
        _ => -1,
    }
}

//! `raw_expression_tree_walker` of `nodeFuncs.c`: the walk of the children of a node of the raw parse tree, which the checks of `WITH RECURSIVE` use before the analysis.

use rupg_sql::nodes::{
    InferClause, JsonAggConstructor, JsonBehavior, JsonOutput, JsonValueExpr, List, Node,
    OnConflictClause, RangeVar, ReturningClause, SelectStmt, TypeName, WindowDef,
};

/// The callbacks of a walk. Each callback gives true to stop the walk.
pub(crate) trait Walker {
    /// A child that is a node.
    fn node(&mut self, node: &Node) -> bool;
    /// A child that is a `SELECT`, as `larg` and `rarg` of a set operation are.
    fn select(&mut self, stmt: &SelectStmt) -> bool;
}

/// Calls the walker for each node of a list.
pub(crate) fn list(w: &mut dyn Walker, list: &List) -> bool {
    list.iter().flatten().any(|node| w.node(node))
}

/// Calls the walker for a node that can be absent.
pub(crate) fn one(w: &mut dyn Walker, node: Option<&Node>) -> bool {
    node.is_some_and(|node| w.node(node))
}

fn type_name(w: &mut dyn Walker, t: Option<&TypeName>) -> bool {
    t.is_some_and(|t| list(w, &t.typmods) || list(w, &t.arrayBounds))
}

fn window(w: &mut dyn Walker, d: Option<&WindowDef>) -> bool {
    d.is_some_and(|d| {
        list(w, &d.partitionClause)
            || list(w, &d.orderClause)
            || one(w, d.startOffset.as_ref())
            || one(w, d.endOffset.as_ref())
    })
}

/// The target table of a write, which the walk sees as a `RangeVar` node.
fn target(w: &mut dyn Walker, rv: Option<&RangeVar>) -> bool {
    rv.is_some_and(|rv| w.node(&Node::RangeVar(Box::new(rv.clone()))))
}

fn returning(w: &mut dyn Walker, r: Option<&ReturningClause>) -> bool {
    r.is_some_and(|r| list(w, &r.options) || list(w, &r.exprs))
}

fn infer(w: &mut dyn Walker, i: Option<&InferClause>) -> bool {
    i.is_some_and(|i| list(w, &i.indexElems) || one(w, i.whereClause.as_ref()))
}

fn on_conflict(w: &mut dyn Walker, c: Option<&OnConflictClause>) -> bool {
    c.is_some_and(|c| {
        infer(w, c.infer.as_deref()) || list(w, &c.targetList) || one(w, c.whereClause.as_ref())
    })
}

fn json_value(w: &mut dyn Walker, v: Option<&JsonValueExpr>) -> bool {
    v.is_some_and(|v| one(w, v.raw_expr.as_ref()) || one(w, v.formatted_expr.as_ref()))
}

fn json_output(w: &mut dyn Walker, o: Option<&JsonOutput>) -> bool {
    o.is_some_and(|o| type_name(w, o.typeName.as_deref()))
}

fn json_behavior(w: &mut dyn Walker, b: Option<&JsonBehavior>) -> bool {
    b.is_some_and(|b| one(w, b.expr.as_ref()))
}

fn json_agg(w: &mut dyn Walker, c: Option<&JsonAggConstructor>) -> bool {
    c.is_some_and(|c| {
        json_output(w, c.output.as_deref())
            || list(w, &c.agg_order)
            || one(w, c.agg_filter.as_ref())
            || window(w, c.over.as_deref())
    })
}

/// The children of a `SELECT`. The walk does not go into the `WITH` clause, as the two walkers of `parse_cte.c` stop there and look at it on their own.
pub(crate) fn select(w: &mut dyn Walker, s: &SelectStmt) -> bool {
    list(w, &s.distinctClause)
        || s.intoClause.as_deref().is_some_and(|i| one(w, i.viewQuery.as_ref()))
        || list(w, &s.targetList)
        || list(w, &s.fromClause)
        || one(w, s.whereClause.as_ref())
        || list(w, &s.groupClause)
        || one(w, s.havingClause.as_ref())
        || list(w, &s.windowClause)
        || list(w, &s.valuesLists)
        || list(w, &s.sortClause)
        || one(w, s.limitOffset.as_ref())
        || one(w, s.limitCount.as_ref())
        || list(w, &s.lockingClause)
        || s.larg.as_deref().is_some_and(|l| w.select(l))
        || s.rarg.as_deref().is_some_and(|r| w.select(r))
}

/// `raw_expression_tree_walker`: calls the walker for each child of the node. A `WITH` clause has no children here, and a statement that changes data has none, because the analyzer does not take it yet.
pub(crate) fn walk(w: &mut dyn Walker, node: &Node) -> bool {
    match node {
        Node::List(l) => list(w, l),
        Node::GroupingFunc(g) => list(w, &g.args),
        Node::SubLink(s) => one(w, s.testexpr.as_ref()) || one(w, s.subselect.as_ref()),
        Node::CaseExpr(c) => {
            one(w, c.arg.as_ref()) || list(w, &c.args) || one(w, c.defresult.as_ref())
        }
        Node::CaseWhen(c) => one(w, c.expr.as_ref()) || one(w, c.result.as_ref()),
        Node::RowExpr(r) => list(w, &r.args),
        Node::CoalesceExpr(c) => list(w, &c.args),
        Node::MinMaxExpr(m) => list(w, &m.args),
        Node::XmlExpr(x) => list(w, &x.named_args) || list(w, &x.args),
        Node::NullTest(t) => one(w, t.arg.as_ref()),
        Node::BooleanTest(t) => one(w, t.arg.as_ref()),
        Node::JoinExpr(j) => {
            one(w, j.larg.as_ref()) || one(w, j.rarg.as_ref()) || one(w, j.quals.as_ref())
        }
        Node::IntoClause(i) => one(w, i.viewQuery.as_ref()),
        Node::SelectStmt(s) => select(w, s),
        // The `WITH` clause of a write is not a child, as the walk of `WITH RECURSIVE` reads it only under its own control.
        Node::InsertStmt(s) => {
            target(w, s.relation.as_deref())
                || list(w, &s.cols)
                || one(w, s.selectStmt.as_ref())
                || on_conflict(w, s.onConflictClause.as_deref())
                || returning(w, s.returningClause.as_deref())
        }
        Node::DeleteStmt(s) => {
            target(w, s.relation.as_deref())
                || list(w, &s.usingClause)
                || one(w, s.whereClause.as_ref())
                || returning(w, s.returningClause.as_deref())
        }
        Node::UpdateStmt(s) => {
            target(w, s.relation.as_deref())
                || list(w, &s.targetList)
                || one(w, s.whereClause.as_ref())
                || list(w, &s.fromClause)
                || returning(w, s.returningClause.as_deref())
        }
        Node::MergeStmt(s) => {
            target(w, s.relation.as_deref())
                || one(w, s.sourceRelation.as_ref())
                || one(w, s.joinCondition.as_ref())
                || list(w, &s.mergeWhenClauses)
                || returning(w, s.returningClause.as_deref())
        }
        Node::MergeWhenClause(c) => {
            one(w, c.condition.as_ref()) || list(w, &c.targetList) || list(w, &c.values)
        }
        Node::A_Expr(e) => one(w, e.lexpr.as_ref()) || one(w, e.rexpr.as_ref()),
        Node::BoolExpr(b) => list(w, &b.args),
        Node::FuncCall(f) => {
            list(w, &f.args)
                || list(w, &f.agg_order)
                || one(w, f.agg_filter.as_ref())
                || window(w, f.over.as_deref())
        }
        Node::NamedArgExpr(n) => one(w, n.arg.as_ref()),
        Node::A_Indices(i) => one(w, i.lidx.as_ref()) || one(w, i.uidx.as_ref()),
        Node::A_Indirection(i) => one(w, i.arg.as_ref()) || list(w, &i.indirection),
        Node::A_ArrayExpr(a) => list(w, &a.elements),
        Node::ResTarget(r) => list(w, &r.indirection) || one(w, r.val.as_ref()),
        Node::MultiAssignRef(m) => one(w, m.source.as_ref()),
        Node::TypeCast(t) => one(w, t.arg.as_ref()) || type_name(w, t.typeName.as_deref()),
        Node::CollateClause(c) => one(w, c.arg.as_ref()),
        Node::SortBy(s) => one(w, s.node.as_ref()),
        Node::WindowDef(d) => window(w, Some(d)),
        Node::RangeSubselect(r) => one(w, r.subquery.as_ref()),
        Node::RangeFunction(r) => list(w, &r.functions) || list(w, &r.coldeflist),
        Node::RangeTableSample(r) => {
            one(w, r.relation.as_ref()) || list(w, &r.args) || one(w, r.repeatable.as_ref())
        }
        Node::RangeTableFunc(r) => {
            one(w, r.docexpr.as_ref())
                || one(w, r.rowexpr.as_ref())
                || list(w, &r.namespaces)
                || list(w, &r.columns)
        }
        Node::RangeTableFuncCol(c) => one(w, c.colexpr.as_ref()) || one(w, c.coldefexpr.as_ref()),
        Node::TypeName(t) => type_name(w, Some(t)),
        Node::ColumnDef(c) => type_name(w, c.typeName.as_deref()) || one(w, c.raw_default.as_ref()),
        Node::IndexElem(i) => one(w, i.expr.as_ref()),
        Node::GroupingSet(g) => list(w, &g.content),
        Node::LockingClause(l) => list(w, &l.lockedRels),
        Node::XmlSerialize(x) => one(w, x.expr.as_ref()) || type_name(w, x.typeName.as_deref()),
        Node::CommonTableExpr(c) => one(w, c.ctequery.as_ref()),
        Node::JsonValueExpr(v) => json_value(w, Some(v)),
        Node::JsonParseExpr(p) => {
            json_value(w, p.expr.as_deref()) || json_output(w, p.output.as_deref())
        }
        Node::JsonScalarExpr(s) => one(w, s.expr.as_ref()) || json_output(w, s.output.as_deref()),
        Node::JsonSerializeExpr(s) => {
            json_value(w, s.expr.as_deref()) || json_output(w, s.output.as_deref())
        }
        Node::JsonIsPredicate(p) => one(w, p.expr.as_ref()),
        Node::JsonArgument(a) => json_value(w, a.val.as_deref()),
        Node::JsonFuncExpr(f) => {
            json_value(w, f.context_item.as_deref())
                || one(w, f.pathspec.as_ref())
                || list(w, &f.passing)
                || json_output(w, f.output.as_deref())
                || json_behavior(w, f.on_empty.as_deref())
                || json_behavior(w, f.on_error.as_deref())
        }
        Node::JsonBehavior(b) => json_behavior(w, Some(b)),
        Node::JsonTable(t) => {
            json_value(w, t.context_item.as_deref())
                || t.pathspec.as_deref().is_some_and(|p| one(w, p.string.as_ref()))
                || list(w, &t.passing)
                || list(w, &t.columns)
                || json_behavior(w, t.on_error.as_deref())
        }
        Node::JsonTableColumn(c) => {
            type_name(w, c.typeName.as_deref())
                || json_behavior(w, c.on_empty.as_deref())
                || json_behavior(w, c.on_error.as_deref())
                || list(w, &c.columns)
        }
        Node::JsonTablePathSpec(p) => one(w, p.string.as_ref()),
        Node::JsonOutput(o) => json_output(w, Some(o)),
        Node::JsonKeyValue(k) => one(w, k.key.as_ref()) || json_value(w, k.value.as_deref()),
        Node::JsonObjectConstructor(c) => json_output(w, c.output.as_deref()) || list(w, &c.exprs),
        Node::JsonArrayConstructor(c) => json_output(w, c.output.as_deref()) || list(w, &c.exprs),
        Node::JsonAggConstructor(c) => json_agg(w, Some(c)),
        Node::JsonObjectAgg(a) => {
            json_agg(w, a.constructor.as_deref())
                || a.arg
                    .as_deref()
                    .is_some_and(|k| one(w, k.key.as_ref()) || json_value(w, k.value.as_deref()))
        }
        Node::JsonArrayAgg(a) => {
            json_agg(w, a.constructor.as_deref()) || json_value(w, a.arg.as_deref())
        }
        Node::JsonArrayQueryConstructor(c) => {
            json_output(w, c.output.as_deref()) || one(w, c.query.as_ref())
        }
        _ => false,
    }
}

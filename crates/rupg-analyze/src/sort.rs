//! `ORDER BY`, `GROUP BY`, `DISTINCT`, `DISTINCT ON`, `LIMIT` and `OFFSET`, as `transformSortClause`, `transformGroupClause`, `transformDistinctClause`, `transformDistinctOnClause` and `transformLimitClause` of `parse_clause.c` make them.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CoercionForm, GroupingSetKind, List, Node, SortBy, SortByDir, SortByNulls};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::{Kind, grouping_error};
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::select::Target;
use crate::typcache::{self, is_binary_coercible};
use crate::typename::place;
use crate::types;

/// An item of `ORDER BY`, `GROUP BY`, `DISTINCT` or `DISTINCT ON`, as `SortGroupClause`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortGroup {
    /// The index of the column in [`crate::Query::targets`].
    pub target: usize,
    /// The OID of the sort operator, or 0.
    pub operator: u32,
    /// The function of the operator that puts a value first, `<` for `ASC` and `>` for `DESC`, or 0 when the type has no order.
    pub sort: u32,
    /// The function of the `=` operator.
    pub equal: u32,
    /// True when a null comes before the other values.
    pub nulls_first: bool,
    /// True when the `=` operator can use a hash table, as `op_hashjoinable` gives it.
    pub hashable: bool,
}

/// A grouping set of `GROUP BY`, as `GroupingSet` after `transformGroupClause`. A simple set names its items by the index of their targets in [`crate::Query::targets`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupingSet {
    /// `()`.
    Empty,
    /// A list of items.
    Simple(Vec<usize>),
    /// `ROLLUP`, of simple sets.
    Rollup(Vec<GroupingSet>),
    /// `CUBE`, of simple sets.
    Cube(Vec<GroupingSet>),
    /// `GROUPING SETS`, of sets of the other kinds.
    Sets(Vec<GroupingSet>),
}

impl GroupingSet {
    /// `expand_groupingset_node`: the lists of items of the set.
    fn expand(&self) -> Vec<Vec<usize>> {
        fn items(set: &GroupingSet) -> &[usize] {
            match set {
                GroupingSet::Simple(items) => items,
                _ => &[],
            }
        }
        match self {
            GroupingSet::Empty => vec![Vec::new()],
            GroupingSet::Simple(items) => vec![items.clone()],
            GroupingSet::Rollup(content) => {
                let mut out: Vec<Vec<usize>> = (1..=content.len())
                    .rev()
                    .map(|n| content[..n].iter().flat_map(|s| items(s).iter().copied()).collect())
                    .collect();
                out.push(Vec::new());
                out
            }
            GroupingSet::Cube(content) => (0..1u32 << content.len())
                .map(|mask| {
                    content
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << i) != 0)
                        .flat_map(|(_, s)| items(s).iter().copied())
                        .collect()
                })
                .collect(),
            GroupingSet::Sets(content) => content.iter().flat_map(GroupingSet::expand).collect(),
        }
    }
}

/// `expand_grouping_sets`: the grouping sets of `GROUP BY` as lists of items, with the shortest lists first. Each set is the product of the expansions of the items of `GROUP BY`. With `distinct` the same set comes once. The result is `None` when there are more than `limit` sets.
pub fn expand_grouping_sets(
    sets: &[GroupingSet],
    distinct: bool,
    limit: usize,
) -> Option<Vec<Vec<usize>>> {
    // `list_union_int`: the items of `a`, then the items of `b` that are not in it.
    fn union(a: &[usize], b: &[usize]) -> Vec<usize> {
        let mut out = a.to_vec();
        for &item in b {
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out
    }
    let mut count = 1.0;
    let mut expanded = Vec::with_capacity(sets.len());
    for set in sets {
        let lists = set.expand();
        count *= lists.len() as f64;
        if count > limit as f64 {
            return None;
        }
        expanded.push(lists);
    }
    let Some((first, rest)) = expanded.split_first() else { return Some(Vec::new()) };
    let mut result: Vec<Vec<usize>> = first.iter().map(|l| union(&[], l)).collect();
    for lists in rest {
        result = result.iter().flat_map(|q| lists.iter().map(|l| union(q, l))).collect();
    }
    if distinct && result.len() > 1 {
        for list in &mut result {
            list.sort_unstable();
        }
        result.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        result.dedup();
    } else {
        result.sort_by_key(Vec::len);
    }
    Some(result)
}

/// An item of `GROUP BY` after `flatten_grouping_sets`.
enum Flat<'a> {
    /// An expression.
    Item(Option<&'a Node>),
    /// The expressions of a row in parentheses in a grouping set.
    Row(Vec<Option<&'a Node>>),
    /// A grouping set, its flat content and its place.
    Set(GroupingSetKind, Vec<Flat<'a>>, Option<usize>),
}

/// True for a row in parentheses with no `ROW`, which `GROUP BY` takes as a list of items.
fn implicit_row(node: &Node) -> Option<&List> {
    match node {
        Node::RowExpr(row) if row.row_format == CoercionForm::COERCE_IMPLICIT_CAST => {
            Some(&row.args)
        }
        _ => None,
    }
}

/// The expressions of a row in parentheses, with the rows in it as their expressions.
fn row_items<'a>(list: &'a List, out: &mut Vec<Option<&'a Node>>) {
    for node in list {
        match node.as_ref().and_then(implicit_row) {
            Some(args) => row_items(args, out),
            None => out.push(node.as_ref()),
        }
    }
}

/// `flatten_grouping_sets` at the top of `GROUP BY`: a row adds its expressions as items, and the empty grouping set `()` adds nothing. `has_sets` becomes true when the list has a grouping set.
fn flatten_top<'a>(list: &'a List, has_sets: &mut bool) -> Vec<Flat<'a>> {
    let mut out = Vec::new();
    for node in list {
        match node {
            Some(Node::GroupingSet(set)) => {
                *has_sets = true;
                if set.kind != GroupingSetKind::GROUPING_SET_EMPTY {
                    out.push(Flat::Set(
                        set.kind,
                        flatten_content(&set.content),
                        place(set.location),
                    ));
                }
            }
            node => match node.as_ref().and_then(implicit_row) {
                Some(args) => {
                    let mut items = Vec::new();
                    row_items(args, &mut items);
                    out.extend(items.into_iter().map(Flat::Item));
                }
                None => out.push(Flat::Item(node.as_ref())),
            },
        }
    }
    out
}

/// `flatten_grouping_sets` in a grouping set: the content of `GROUPING SETS` in `GROUPING SETS` comes into the outer set.
fn flatten_content(list: &List) -> Vec<Flat<'_>> {
    let mut out = Vec::new();
    for node in list {
        match node {
            Some(Node::GroupingSet(set)) if set.kind == GroupingSetKind::GROUPING_SET_SETS => {
                out.extend(flatten_content(&set.content));
            }
            Some(Node::GroupingSet(set)) => {
                out.push(Flat::Set(set.kind, flatten_content(&set.content), place(set.location)));
            }
            node => match node.as_ref().and_then(implicit_row) {
                Some(args) => {
                    let mut items = Vec::new();
                    row_items(args, &mut items);
                    out.push(Flat::Row(items));
                }
                None => out.push(Flat::Item(node.as_ref())),
            },
        }
    }
    out
}

/// `leftmostLoc`.
fn leftmost(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `exprLocation` of the first item of a list that has a location.
pub(crate) fn list_location(list: &List) -> Option<usize> {
    list.iter().find_map(|node| raw_location(node.as_ref()))
}

/// `exprLocation` of a raw expression of the parser.
pub(crate) fn raw_location(node: Option<&Node>) -> Option<usize> {
    match node? {
        Node::ColumnRef(c) => place(c.location),
        Node::SortBy(s) => raw_location(s.node.as_ref()),
        Node::ParamRef(p) => place(p.location),
        Node::A_Const(c) => place(c.location),
        Node::A_ArrayExpr(a) => place(a.location),
        Node::CaseExpr(c) => place(c.location),
        Node::CoalesceExpr(c) => place(c.location),
        Node::MinMaxExpr(m) => place(m.location),
        Node::SQLValueFunction(f) => place(f.location),
        Node::RowExpr(r) => place(r.location),
        Node::A_Expr(a) => leftmost(place(a.location), raw_location(a.lexpr.as_ref())),
        Node::FuncCall(f) => leftmost(place(f.location), list_location(&f.args)),
        Node::BoolExpr(b) => leftmost(place(b.location), list_location(&b.args)),
        Node::NullTest(n) => leftmost(place(n.location), raw_location(n.arg.as_ref())),
        Node::BooleanTest(b) => leftmost(place(b.location), raw_location(b.arg.as_ref())),
        Node::SubLink(s) => leftmost(raw_location(s.testexpr.as_ref()), place(s.location)),
        Node::CollateClause(c) => raw_location(c.arg.as_ref()),
        Node::TypeCast(t) => {
            let at = raw_location(t.arg.as_ref());
            let at = leftmost(at, t.typeName.as_ref().and_then(|n| place(n.location)));
            leftmost(at, place(t.location))
        }
        _ => None,
    }
}

/// The error of a clause that names a column that is not in the select list.
pub(crate) fn invalid_reference(message: String, at: Option<usize>) -> Error {
    Error::new(SqlState::INVALID_COLUMN_REFERENCE, message).at_opt(at)
}

impl Analyzer<'_> {
    /// `findTargetlistEntrySQL92`: the column of the select list that an item of `ORDER BY`, `GROUP BY` or `DISTINCT ON` names. A bare name is an output column, and an integer is a position in the select list. In `GROUP BY` a column of `FROM` comes before an output column. Any other expression goes to [`Analyzer::target_sql99`].
    fn target_sql92(
        &mut self,
        node: Option<&Node>,
        targets: &mut Vec<Target>,
        kind: Kind,
    ) -> Result<usize> {
        if let Some(Node::ColumnRef(c)) = node
            && let [Some(Node::String(name))] = c.fields.as_slice()
            && (kind != Kind::GroupBy || self.column_by_name(name, place(c.location))?.is_none())
        {
            let mut found: Option<usize> = None;
            for (i, target) in targets.iter().enumerate() {
                if target.junk || target.name != **name {
                    continue;
                }
                match found {
                    Some(first) if !targets[first].expr.same(&target.expr) => {
                        return Err(Error::new(
                            SqlState::AMBIGUOUS_COLUMN,
                            format!("{} \"{name}\" is ambiguous", kind.name()),
                        )
                        .at_opt(place(c.location)));
                    }
                    Some(_) => {}
                    None => found = Some(i),
                }
            }
            if let Some(i) = found {
                self.check_sql92(&targets[i], kind)?;
                return Ok(i);
            }
        }
        if let Some(Node::A_Const(c)) = node {
            let at = place(c.location);
            let Some(Node::Integer(position)) = c.val.as_ref().filter(|_| !c.isnull) else {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("non-integer constant in {}", kind.name()),
                )
                .at_opt(at));
            };
            let visible = targets.iter().enumerate().filter(|(_, t)| !t.junk);
            let mut seen = 0;
            for (i, _) in visible {
                seen += 1;
                if seen == *position {
                    self.check_sql92(&targets[i], kind)?;
                    return Ok(i);
                }
            }
            return Err(invalid_reference(
                format!("{} position {position} is not in select list", kind.name()),
                at,
            ));
        }
        self.target_sql99(node, targets, kind)
    }

    /// `checkTargetlistEntrySQL92`: an item of `GROUP BY` that names a column of the select list cannot name an aggregate call.
    fn check_sql92(&self, target: &Target, kind: Kind) -> Result<()> {
        if kind == Kind::GroupBy
            && self.has_aggs
            && let Some(agg) = target.expr.first_agg()
        {
            return Err(grouping_error(
                format!("aggregate functions are not allowed in {}", kind.name()),
                agg.location,
            ));
        }
        Ok(())
    }

    /// `findTargetlistEntrySQL99`: the column of the select list with the same expression, or a new junk column that the result does not show. A cast that the analyzer added to a column does not count.
    fn target_sql99(
        &mut self,
        node: Option<&Node>,
        targets: &mut Vec<Target>,
        kind: Kind,
    ) -> Result<usize> {
        let expr = self.with_kind(kind, |a| a.transform(node))?;
        if let Some(i) = targets.iter().position(|t| t.expr.strip_implicit().same(&expr)) {
            return Ok(i);
        }
        targets.push(Target { name: String::new(), expr, origin: None, junk: true });
        Ok(targets.len() - 1)
    }

    /// The column as `text` when its type is `unknown`, as `addTargetToSortList` does.
    fn resolve_unknown(&mut self, target: &mut Target) -> Result<()> {
        if target.expr.ty == oid::UNKNOWN {
            let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
            target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
        }
        Ok(())
    }

    /// `transformSortClause` of the `ORDER BY` of a query.
    pub(crate) fn sort_clause(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
    ) -> Result<Vec<SortGroup>> {
        self.sort_list(list, targets, false, Kind::OrderBy)
    }

    /// `transformSortClause` of the `ORDER BY` of an aggregate call, which uses the rules of SQL99 only.
    pub(crate) fn sort_clause_sql99(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
    ) -> Result<Vec<SortGroup>> {
        self.sort_list(list, targets, true, Kind::OrderBy)
    }

    /// `transformSortClause` of the `ORDER BY` of a window, which uses the rules of SQL99 only.
    pub(crate) fn window_order(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
    ) -> Result<Vec<SortGroup>> {
        self.sort_list(list, targets, true, Kind::WindowOrder)
    }

    /// `transformGroupClause` of the `PARTITION BY` of a window, which uses the rules of SQL99 only. An item that is also an item of the `ORDER BY` of the window takes its operator.
    pub(crate) fn window_partition(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
        order: &[SortGroup],
    ) -> Result<Vec<SortGroup>> {
        let mut result = Vec::new();
        for node in list {
            let target = self.target_sql99(node.as_ref(), targets, Kind::WindowPartition)?;
            if result.iter().any(|g: &SortGroup| g.target == target) {
                continue;
            }
            if let Some(item) = order.iter().find(|s| s.target == target) {
                result.push(item.clone());
                continue;
            }
            self.add_group(target, targets, &mut result, raw_location(node.as_ref()))?;
        }
        Ok(result)
    }

    fn sort_list(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
        sql99: bool,
        kind: Kind,
    ) -> Result<Vec<SortGroup>> {
        let mut sort = Vec::new();
        for item in list {
            let Some(Node::SortBy(by)) = item else {
                return Err(Error::internal("an item of ORDER BY that is not SortBy"));
            };
            let target = if sql99 {
                self.target_sql99(by.node.as_ref(), targets, kind)?
            } else {
                self.target_sql92(by.node.as_ref(), targets, Kind::OrderBy)?
            };
            self.add_sort(by, target, targets, &mut sort)?;
        }
        Ok(sort)
    }

    /// `transformGroupClause`: the items of `GROUP BY`, each once, and the grouping sets. The list of grouping sets is empty when `GROUP BY` has no grouping set. With only empty grouping sets it is `()` and the list of items is empty.
    pub(crate) fn group_clause(
        &mut self,
        list: &List,
        targets: &mut Vec<Target>,
        sort: &[SortGroup],
    ) -> Result<(Vec<SortGroup>, Vec<GroupingSet>)> {
        let mut has_sets = false;
        let flat = flatten_top(list, &mut has_sets);
        let mut result = Vec::new();
        let mut sets = Vec::new();
        if flat.is_empty() && has_sets {
            sets.push(GroupingSet::Empty);
        }
        let mut seen = Vec::new();
        for item in &flat {
            match item {
                Flat::Set(kind, content, at) => {
                    sets.push(self.grouping_set(
                        *kind,
                        content,
                        *at,
                        targets,
                        sort,
                        &mut result,
                    )?);
                }
                Flat::Row(_) => return Err(Error::internal("a row at the top of GROUP BY")),
                Flat::Item(node) => {
                    let target =
                        self.group_item(*node, Some(&mut seen), targets, sort, &mut result, true)?;
                    if let Some(target) = target
                        && has_sets
                    {
                        sets.push(GroupingSet::Simple(vec![target]));
                    }
                }
            }
        }
        Ok((result, sets))
    }

    /// `transformGroupClauseExpr`: the target of an item of `GROUP BY`, which comes in `result` once. An item that is also an item of `ORDER BY` takes the operator of `ORDER BY`, but in a grouping set it puts nulls last. `seen` has the targets of the list that has the item, and an item that is in it again gives `None`.
    fn group_item(
        &mut self,
        node: Option<&Node>,
        seen: Option<&mut Vec<usize>>,
        targets: &mut Vec<Target>,
        sort: &[SortGroup],
        result: &mut Vec<SortGroup>,
        toplevel: bool,
    ) -> Result<Option<usize>> {
        let target = self.target_sql92(node, targets, Kind::GroupBy)?;
        if let Some(seen) = seen {
            if seen.contains(&target) {
                return Ok(None);
            }
            seen.push(target);
        }
        if result.iter().any(|g| g.target == target) {
            return Ok(Some(target));
        }
        match sort.iter().find(|s| s.target == target) {
            Some(item) => {
                result
                    .push(SortGroup { nulls_first: item.nulls_first && toplevel, ..item.clone() });
            }
            None => self.add_group(target, targets, result, raw_location(node))?,
        }
        Ok(Some(target))
    }

    /// `transformGroupingSet`: a grouping set with the targets of its items. A row in parentheses is a simple set with each item once.
    fn grouping_set(
        &mut self,
        kind: GroupingSetKind,
        content: &[Flat<'_>],
        at: Option<usize>,
        targets: &mut Vec<Target>,
        sort: &[SortGroup],
        result: &mut Vec<SortGroup>,
    ) -> Result<GroupingSet> {
        let mut sets = Vec::with_capacity(content.len());
        for item in content {
            sets.push(match item {
                Flat::Row(nodes) => {
                    let mut seen = Vec::new();
                    let mut items = Vec::with_capacity(nodes.len());
                    for &node in nodes {
                        let target =
                            self.group_item(node, Some(&mut seen), targets, sort, result, false)?;
                        items.extend(target);
                    }
                    GroupingSet::Simple(items)
                }
                Flat::Set(kind, content, at) => {
                    self.grouping_set(*kind, content, *at, targets, sort, result)?
                }
                Flat::Item(node) => {
                    let target = self.group_item(*node, None, targets, sort, result, false)?;
                    GroupingSet::Simple(target.into_iter().collect())
                }
            });
        }
        // The number of the sets of `CUBE` grows as a power of 2.
        if kind == GroupingSetKind::GROUPING_SET_CUBE && sets.len() > 12 {
            return Err(
                Error::new(SqlState::TOO_MANY_COLUMNS, "CUBE is limited to 12 elements").at_opt(at)
            );
        }
        Ok(match kind {
            GroupingSetKind::GROUPING_SET_EMPTY => GroupingSet::Empty,
            GroupingSetKind::GROUPING_SET_ROLLUP => GroupingSet::Rollup(sets),
            GroupingSetKind::GROUPING_SET_CUBE => GroupingSet::Cube(sets),
            _ => GroupingSet::Sets(sets),
        })
    }

    /// `addTargetToSortList`.
    pub(crate) fn add_sort(
        &mut self,
        by: &SortBy,
        target: usize,
        targets: &mut [Target],
        sort: &mut Vec<SortGroup>,
    ) -> Result<()> {
        self.resolve_unknown(&mut targets[target])?;
        let ty = targets[target].expr.ty;
        // An error points at the operator of `USING`, or else at the expression.
        let at = place(by.location).or_else(|| raw_location(by.node.as_ref()));
        let (sort_op, eq_op, reverse) = match by.sortby_dir {
            SortByDir::SORTBY_USING => self.using_operator(&by.useOp, ty, at)?,
            dir => {
                let reverse = dir == SortByDir::SORTBY_DESC;
                let ops = typcache::operators(ty);
                let sort_op = if reverse { ops.gt } else { ops.lt };
                if sort_op == 0 {
                    return Err(no_ordering(ty, at));
                }
                if ops.eq == 0 {
                    return Err(no_equality(ty, at));
                }
                (sort_op, ops.eq, reverse)
            }
        };
        let nulls_first = match by.sortby_nulls {
            SortByNulls::SORTBY_NULLS_FIRST => true,
            SortByNulls::SORTBY_NULLS_LAST => false,
            _ => reverse,
        };
        // `targetIsInSortList`: a column is in the list once with an operator and its commutator.
        let commutator = |op: u32| builtin::operator_by_oid(op).map_or(0, |o| o.commutator);
        if !sort.iter().any(|s| {
            s.target == target && (s.operator == sort_op || commutator(s.operator) == sort_op)
        }) {
            sort.push(SortGroup {
                target,
                operator: sort_op,
                sort: function_of(sort_op),
                equal: function_of(eq_op),
                nulls_first,
                hashable: typcache::hashable(eq_op, ty),
            });
        }
        Ok(())
    }

    /// The operator of `ORDER BY ... USING op`, its `=` operator and true when it sorts in descending order.
    fn using_operator(&self, names: &List, ty: u32, at: Option<usize>) -> Result<(u32, u32, bool)> {
        let names: Vec<&str> = names
            .iter()
            .map(|n| match n {
                Some(Node::String(s)) => Ok(&**s),
                _ => Err(Error::internal("an operator name that is not a string")),
            })
            .collect::<Result<_>>()?;
        let op = self.find_operator(&names, ty, ty, at)?;
        // `compatible_oper`: the inputs must have the types of the operator with no cast.
        if !is_binary_coercible(ty, op.left) || !is_binary_coercible(ty, op.right) {
            let name = names.join(".");
            let ty = types::name(ty);
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("operator requires run-time type coercion: {ty} {name} {ty}"),
            )
            .at_opt(at));
        }
        let Some((eq, reverse)) = typcache::equality_for_ordering(op.oid) else {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "operator {} is not a valid ordering operator",
                    names.last().copied().unwrap_or_default()
                ),
            )
            .with_hint(
                "Ordering operators must be \"<\" or \">\" members of btree operator families.",
            )
            .at_opt(at));
        };
        Ok((op.oid, eq, reverse))
    }

    /// `addTargetToGroupList`: a column of `DISTINCT` with the default operators of its type. A type with `=` and no order has no sort function.
    fn add_group(
        &mut self,
        target: usize,
        targets: &mut [Target],
        list: &mut Vec<SortGroup>,
        at: Option<usize>,
    ) -> Result<()> {
        self.resolve_unknown(&mut targets[target])?;
        if list.iter().any(|s| s.target == target) {
            return Ok(());
        }
        let ty = targets[target].expr.ty;
        let ops = typcache::operators(ty);
        if ops.eq == 0 {
            return Err(no_equality(ty, at));
        }
        list.push(SortGroup {
            target,
            operator: ops.lt,
            sort: function_of(ops.lt),
            equal: function_of(ops.eq),
            nulls_first: false,
            hashable: typcache::hashable(ops.eq, ty),
        });
        Ok(())
    }

    /// `transformDistinctClause`: the items of `ORDER BY`, then the other columns of the select list or the other arguments of the aggregate call.
    pub(crate) fn distinct_clause(
        &mut self,
        targets: &mut [Target],
        sort: &[SortGroup],
        is_agg: bool,
    ) -> Result<Vec<SortGroup>> {
        let mut list = Vec::new();
        for item in sort {
            let target = &targets[item.target];
            if target.junk {
                let message = if is_agg {
                    "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
                } else {
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                };
                return Err(invalid_reference(message.into(), target.expr.place()));
            }
            list.push(item.clone());
        }
        for i in 0..targets.len() {
            if !targets[i].junk {
                let at = targets[i].expr.place();
                self.add_group(i, targets, &mut list, at)?;
            }
        }
        if list.is_empty() {
            let message = if is_agg {
                "an aggregate with DISTINCT must have at least one argument"
            } else {
                "SELECT DISTINCT must have at least one column"
            };
            return Err(Error::new(SqlState::SYNTAX_ERROR, message));
        }
        Ok(list)
    }

    /// `transformDistinctOnClause`: the items of `ORDER BY` that `DISTINCT ON` names, then the other items of `DISTINCT ON`. The items of `DISTINCT ON` must come first in `ORDER BY`.
    pub(crate) fn distinct_on_clause(
        &mut self,
        on: &List,
        targets: &mut Vec<Target>,
        sort: &[SortGroup],
    ) -> Result<Vec<SortGroup>> {
        let mut refs = Vec::with_capacity(on.len());
        for node in on {
            refs.push(self.target_sql92(node.as_ref(), targets, Kind::DistinctOn)?);
        }
        let mismatch = |at| {
            invalid_reference(
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions".into(),
                at,
            )
        };
        let mut list = Vec::new();
        let mut skipped = false;
        for item in sort {
            match refs.iter().position(|&r| r == item.target) {
                Some(_) if skipped => {
                    let first = refs.iter().position(|&r| r == item.target);
                    return Err(mismatch(first.and_then(|i| raw_location(on[i].as_ref()))));
                }
                Some(_) => list.push(item.clone()),
                None => skipped = true,
            }
        }
        for (node, &target) in on.iter().zip(&refs) {
            if list.iter().any(|s| s.target == target) {
                continue;
            }
            let at = raw_location(node.as_ref());
            if skipped {
                return Err(mismatch(at));
            }
            self.add_group(target, targets, &mut list, at)?;
        }
        Ok(list)
    }

    /// `transformLimitClause`: the value of `LIMIT` or `OFFSET` as `bigint`. It cannot read a column.
    pub(crate) fn limit_clause(
        &mut self,
        node: Option<&Node>,
        construct: &str,
        with_ties: bool,
    ) -> Result<Option<Expr>> {
        let Some(node) = node else { return Ok(None) };
        let expr = self.transform(Some(node))?;
        let expr = self.coerce_to_specific(expr, oid::INT8, construct)?;
        if let Some(var) = expr.first_var() {
            return Err(invalid_reference(
                format!("argument of {construct} must not contain variables"),
                var.location,
            ));
        }
        if with_ties && construct == "LIMIT" && matches!(node, Node::A_Const(c) if c.isnull) {
            return Err(Error::new(
                SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                "row count cannot be null in FETCH FIRST ... WITH TIES clause",
            ));
        }
        Ok(Some(expr))
    }
}

/// The function of an operator, or 0.
fn function_of(operator: u32) -> u32 {
    builtin::operator_by_oid(operator).map_or(0, |op| op.code)
}

fn no_ordering(ty: u32, at: Option<usize>) -> Error {
    Error::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("could not identify an ordering operator for type {}", types::name(ty)),
    )
    .with_hint("Use an explicit ordering operator or modify the query.")
    .at_opt(at)
}

/// `makeSortGroupClauseForSetOp`: the operators that a set operation uses for a column, to find the rows that are equal.
pub(crate) fn set_group(target: usize, ty: u32, at: Option<usize>) -> Result<SortGroup> {
    let ops = typcache::operators(ty);
    if ops.eq == 0 {
        return Err(no_equality(ty, at));
    }
    Ok(SortGroup {
        target,
        operator: ops.lt,
        sort: function_of(ops.lt),
        equal: function_of(ops.eq),
        nulls_first: false,
        hashable: typcache::hashable(ops.eq, ty),
    })
}

fn no_equality(ty: u32, at: Option<usize>) -> Error {
    Error::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("could not identify an equality operator for type {}", types::name(ty)),
    )
    .at_opt(at)
}

//! `transformFromClause` of `parse_clause.c` and the name lookup of `parse_relation.c`: the relations of `FROM`, the joins, and the column that a name gives.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::Values;
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{
    Alias, CoercionForm, ColumnRef, FuncCall, JoinExpr, JoinType, Node, RangeFunction,
    RangeSubselect, RangeVar, SelectStmt,
};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::colname::figure_colname;
use crate::expr::{CastForm, Expr, ExprKind, Var};
use crate::typename::place;
use crate::types;

/// `MAX_FUZZY_DISTANCE` of `parse_relation.c`: the largest distance of a name that an error suggests.
const MAX_FUZZY_DISTANCE: usize = 3;

/// The attribute number of the system column `tableoid`.
pub const TABLE_OID_ATTNUM: i16 = -6;

/// The system columns other than `tableoid`, which rupg does not have yet.
const OTHER_SYSTEM_COLUMNS: [&str; 5] = ["ctid", "xmin", "cmin", "xmax", "cmax"];

/// A relation of `FROM`, as a `RangeTblEntry` of the kind `RTE_RELATION` or `RTE_SUBQUERY`.
#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    /// The OID of the `pg_class` row, or 0 for a subquery.
    pub oid: u32,
    /// The columns of the relation, with their real names. The columns of a subquery are the columns of its result.
    pub columns: Vec<Column>,
    /// The query of a subquery in `FROM`, or the query of a view, which the executor reads in place of the view.
    pub subquery: Option<Box<crate::Query>>,
    /// The calls of a function in `FROM`.
    pub function: Option<FromFunction>,
    /// The rows of a `VALUES` list, each with one expression for each column.
    pub values: Option<Vec<Vec<Expr>>>,
    /// `eref->aliasname`: the alias, or the name that the analyzer gives the relation, such as the name of the table, `unnamed_subquery` or the name of the function.
    pub name: String,
    /// The names of the columns of the alias when the query gives the relation an alias, as `alias->colnames`. [`Relation::name`] is then the name of the alias.
    pub alias: Option<Vec<String>>,
    /// True when the query writes `LATERAL` before a subquery or a function.
    pub lateral: bool,
}

impl Relation {
    /// A relation with no name, no alias and no `LATERAL`. `add_relation` gives it its name.
    pub(crate) fn new(
        oid: u32,
        columns: Vec<Column>,
        subquery: Option<Box<crate::Query>>,
        function: Option<FromFunction>,
        values: Option<Vec<Vec<Expr>>>,
    ) -> Relation {
        Relation {
            oid,
            columns,
            subquery,
            function,
            values,
            name: String::new(),
            alias: None,
            lateral: false,
        }
    }
}

/// A call of a function in `FROM` with its name and its column definition list.
type FunctionCall<'n> = (Expr, String, &'n [Option<Node>]);

/// A function in `FROM`, as the `functions` and `funcordinality` of a `RangeTblEntry` of the kind `RTE_FUNCTION`.
#[derive(Clone, Debug, PartialEq)]
pub struct FromFunction {
    /// The call of the function, or of each function of `ROWS FROM`, with the number of columns that it gives. The rows of the calls go side by side, and a call with fewer rows gives nulls.
    pub calls: Vec<(Expr, usize)>,
    /// `WITH ORDINALITY`: the last column is the number of the row, from 1.
    pub ordinality: bool,
    /// For each call, true when the query gives a column definition list for it, as `funccolnames` of `RangeTblFunction`.
    pub coldefs: Vec<bool>,
}

/// A column of a relation.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: u32,
    pub typmod: i32,
    /// `attnotnull`: the column has a not-null constraint.
    pub not_null: bool,
}

/// A part of `FROM`: a relation or a join.
#[derive(Clone, Debug, PartialEq)]
pub enum FromItem {
    /// The relation with this index in [`crate::Query::relations`].
    Relation(usize),
    Join(Box<Join>),
}

/// A join of two parts of `FROM`.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub left: FromItem,
    pub right: FromItem,
    /// The condition of `ON` or of `USING`, of type `boolean`, or `None` for a cross join.
    pub on: Option<Expr>,
    /// The names of the columns of `USING`, or the common columns of a natural join.
    pub using: Vec<String>,
    /// True when each merged column of `USING` is a column of one side with no cast. A merged column of a full join, or with a cast, is not a column of one side, as `has_dangerous_join_using` finds.
    pub plain_using: bool,
    /// The alias of the join.
    pub alias: Option<String>,
    /// The alias of `USING (...) AS name`.
    pub using_alias: Option<String>,
    /// The value of each column of `USING`, as `joinaliasvars` has it: a column of one side, a cast of it, or `COALESCE` of the two sides for a full join.
    pub merged: Vec<Expr>,
}

/// The kinds of join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// An entry of the range table, as `RangeTblEntry`: a relation or a join. The errors search all the entries, also the entries that a name cannot see.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// `eref->aliasname`: the alias, or the name of the relation.
    name: String,
    /// True when the query gives an alias.
    aliased: bool,
    /// `eref->colnames`.
    columns: Vec<String>,
    /// The index in the relations for a relation, or `None` for a join.
    relation: Option<usize>,
    /// The OID of the relation, or 0 for a join.
    oid: u32,
}

/// An item of the namespace, as `ParseNamespaceItem`: a name that the query can use and its columns.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    /// The index of the entry in the range table.
    entry: usize,
    /// The name that qualifies the columns.
    name: String,
    /// The name and the value of each column that the item gives.
    columns: Vec<(String, Expr)>,
    /// True when a qualified name can use the item.
    rel_visible: bool,
    /// True when an unqualified column name can use the item.
    cols_visible: bool,
}

/// The state of the names of `FROM`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope {
    pub(crate) relations: Vec<Relation>,
    entries: Vec<Entry>,
    namespace: Vec<Item>,
    /// The items that only a `LATERAL` subquery can see, as the items with `p_lateral_only`, each with `p_lateral_ok`: the parts of `FROM` before a subquery, and the left side of a join while the analyzer reads the right side.
    lateral: Vec<(Item, bool)>,
    /// `p_lateral_active`: true while the analyzer reads a `LATERAL` subquery of this query, which can see the items of `lateral`.
    lateral_active: bool,
}

impl Scope {
    /// The items that a name can find, each with `p_lateral_ok`: the namespace, then the items that only `LATERAL` can see when a `LATERAL` subquery is active.
    fn visible(&self) -> impl Iterator<Item = (&Item, bool)> {
        let lateral = self.lateral.iter().filter(|_| self.lateral_active);
        self.namespace.iter().map(|i| (i, true)).chain(lateral.map(|(i, ok)| (i, *ok)))
    }
}

/// `check_lateral_ref_ok`: an item on the left side of a full or a right join is visible in a `LATERAL` subquery on the right side, but a reference to it is an error.
fn check_lateral_ok(item: &Item, ok: bool, at: Option<usize>) -> Result<()> {
    if ok {
        return Ok(());
    }
    Err(Error::new(
        SqlState::INVALID_COLUMN_REFERENCE,
        format!("invalid reference to FROM-clause entry for table \"{}\"", item.name),
    )
    .with_detail("The combining JOIN type must be INNER or LEFT for a LATERAL reference.")
    .at_opt(at))
}

/// `varstr_levenshtein` with costs of 1: the edit distance of two names, by characters.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// `FuzzyAttrMatchState`: the best matches of a column name that does not exist.
struct Fuzzy {
    distance: usize,
    first: Option<(Place, usize)>,
    second: Option<(Place, usize)>,
    exact: Vec<Place>,
}

/// The place of an entry of the range table: the query level, 0 for the query of the name, and the index of the entry at that level.
type Place = (usize, usize);

impl Fuzzy {
    /// `updateFuzzyAttrMatchState`.
    fn update(&mut self, penalty: usize, entry: Place, actual: &str, wanted: &str, column: usize) {
        if penalty > self.distance || actual.is_empty() {
            return;
        }
        let distance = levenshtein(actual, wanted);
        if distance > wanted.len() / 2 {
            return;
        }
        let distance = distance + penalty;
        if distance < self.distance {
            self.distance = distance;
            self.first = Some((entry, column));
            self.second = None;
        } else if distance == self.distance {
            if self.second.is_some() {
                self.first = None;
                self.second = None;
            } else if self.first.is_some() {
                self.second = Some((entry, column));
            }
        }
    }
}

/// The names of an alias column list.
fn alias_columns(alias: &Alias) -> Vec<String> {
    alias
        .colnames
        .iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// The strings of a list of `String` nodes, where `None` is `*`.
fn fields(list: &[Option<Node>]) -> Vec<Option<&str>> {
    list.iter()
        .map(|f| match f {
            Some(Node::String(s)) => Some(&**s),
            _ => None,
        })
        .collect()
}

impl Analyzer<'_> {
    /// `transformFromClause`: the parts of `FROM`, with their names in the namespace.
    pub(crate) fn transform_from(&mut self, list: &[Option<Node>]) -> Result<Vec<FromItem>> {
        let mut items = Vec::with_capacity(list.len());
        for node in list {
            let node = node.as_ref().ok_or_else(|| Error::internal("an empty FROM item"))?;
            let (item, _, namespace) = self.transform_from_item(node)?;
            self.check_conflicts(&self.scope.namespace, &namespace)?;
            self.scope.namespace.extend(namespace);
            items.push(item);
        }
        Ok(items)
    }

    /// `transformFromClauseItem`: one part of `FROM`, the namespace item of its result, and the namespace that it adds.
    fn transform_from_item(&mut self, node: &Node) -> Result<(FromItem, Item, Vec<Item>)> {
        match node {
            Node::RangeVar(rv) => {
                let item = self.relation(rv)?;
                let index = self.scope.entries[item.entry].relation.unwrap_or_default();
                Ok((FromItem::Relation(index), item.clone(), vec![item]))
            }
            Node::JoinExpr(j) => self.join(j),
            Node::RangeSubselect(r) => {
                let item = self.range_subselect(r)?;
                let index = self.scope.entries[item.entry].relation.unwrap_or_default();
                Ok((FromItem::Relation(index), item.clone(), vec![item]))
            }
            Node::RangeFunction(r) => {
                let item = self.range_function(r)?;
                let index = self.scope.entries[item.entry].relation.unwrap_or_default();
                Ok((FromItem::Relation(index), item.clone(), vec![item]))
            }
            Node::RangeTableSample(_) => Err(not_yet("TABLESAMPLE", None)),
            Node::RangeTableFunc(_) | Node::JsonTable(_) => {
                Err(not_yet("XMLTABLE and JSON_TABLE", None))
            }
            _ => Err(Error::internal("unrecognized node type in FROM")),
        }
    }

    /// `addRangeTableEntry` for a table of the catalog: the entry and its namespace item.
    fn relation(&mut self, rv: &RangeVar) -> Result<Item> {
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default();
        let oid = self.lookup_relation(rv)?.ok_or_else(|| {
            let message = match &rv.schemaname {
                Some(schema) => format!("relation \"{schema}.{name}\" does not exist"),
                None => format!("relation \"{name}\" does not exist"),
            };
            Error::new(SqlState::UNDEFINED_TABLE, message).at_opt(at)
        })?;
        let columns = self.open_relation(oid, name, at)?;
        let subquery = self.view_query(oid)?.map(Box::new);
        let alias = rv.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(name).to_string();
        self.add_relation(Relation::new(oid, columns, subquery, None, None), refname, alias)
    }

    /// `ApplyRetrieveRule`: the query of a view of the user, which takes the place of the view as a subquery, or `None` for a relation that is not a view. The analyzer reads the statement that made the view again, with the schemas of `search_path` of that statement. An error in the query of the view has no position, and the warnings of the query go.
    pub(crate) fn view_query(&self, oid: u32) -> Result<Option<crate::Query>> {
        let Some(rel) = self.env.catalog().and_then(|c| c.relation(oid)) else { return Ok(None) };
        let Some(view) = &rel.view else { return Ok(None) };
        if self.views.contains(&oid) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("infinite recursion detected in rules for relation \"{}\"", rel.name),
            ));
        }
        let (list, _) = rupg_sql::parse(&view.text).map_err(Error::from)?;
        let query = list.iter().flatten().find_map(|node| match node {
            Node::RawStmt(raw) => match raw.stmt.as_ref() {
                Some(Node::ViewStmt(stmt)) => stmt.query.as_ref(),
                _ => None,
            },
            _ => None,
        });
        let Some(Node::SelectStmt(select)) = query else {
            return Err(Error::internal("the query of a view is not a SELECT"));
        };
        let mut an = Analyzer::new(self.env, &crate::Params::default());
        an.path.clone_from(&view.path);
        an.views.clone_from(&self.views);
        an.views.push(oid);
        an.select(select).map(Some).map_err(|error| error.with_position(None))
    }

    /// `transformRangeSubselect` and `addRangeTableEntryForSubquery`: a subquery in `FROM` and its namespace item. With no alias, the name of the entry is `unnamed_subquery`, and a qualified name cannot use it.
    fn range_subselect(&mut self, r: &RangeSubselect) -> Result<Item> {
        let query = self.subquery_in_from(r.subquery.as_ref(), r.lateral)?;
        let columns = query
            .targets
            .iter()
            .filter(|t| !t.junk)
            .map(|t| Column {
                name: t.name.clone(),
                ty: t.expr.ty,
                typmod: t.expr.typmod,
                not_null: false,
            })
            .collect();
        let alias = r.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or("unnamed_subquery");
        let mut relation = Relation::new(0, columns, Some(Box::new(query)), None, None);
        relation.lateral = r.lateral;
        let mut item = self.add_relation(relation, refname.to_string(), alias)?;
        item.rel_visible = alias.is_some();
        Ok(item)
    }

    /// `parse_sub_analyze` for a subquery in `FROM`: the subquery sees the queries outside this query. It sees the parts of `FROM` before it only with `LATERAL`.
    fn subquery_in_from(&mut self, node: Option<&Node>, lateral: bool) -> Result<crate::Query> {
        let Some(Node::SelectStmt(stmt)) = node else {
            return Err(Error::internal("a subquery in FROM that is not SelectStmt"));
        };
        self.sub_select(stmt, lateral)
    }

    /// `parse_sub_analyze`: the query tree of a subquery of this query, which sees the queries outside this query. It sees the parts of `FROM` of this query only with `LATERAL`.
    pub(crate) fn sub_select(&mut self, stmt: &SelectStmt, lateral: bool) -> Result<crate::Query> {
        let mut scope = std::mem::take(&mut self.scope);
        let before = scope.lateral.len();
        let hidden = std::mem::take(&mut scope.namespace);
        scope.lateral.extend(hidden.into_iter().map(|item| (item, true)));
        let active = std::mem::replace(&mut scope.lateral_active, lateral);
        self.outer.push(scope);
        let kind = std::mem::replace(&mut self.kind, Kind::Other);
        let has_aggs = std::mem::replace(&mut self.has_aggs, false);
        let result = self.select(stmt);
        self.has_aggs = has_aggs;
        self.kind = kind;
        let mut scope = self.outer.pop().unwrap_or_default();
        scope.lateral_active = active;
        scope.namespace =
            scope.lateral.split_off(before).into_iter().map(|(item, _)| item).collect();
        self.scope = scope;
        result
    }

    /// `transformRangeFunction` and `addRangeTableEntryForFunction`: a function in `FROM`, or the functions of `ROWS FROM`, and its namespace item. The arguments can read the parts of `FROM` before the function, also with no `LATERAL`.
    fn range_function(&mut self, r: &RangeFunction) -> Result<Item> {
        let before = self.scope.lateral.len();
        let hidden = std::mem::take(&mut self.scope.namespace);
        self.scope.lateral.extend(hidden.into_iter().map(|item| (item, true)));
        let active = std::mem::replace(&mut self.scope.lateral_active, true);
        let calls = self.function_calls(r);
        self.scope.lateral_active = active;
        self.scope.namespace =
            self.scope.lateral.split_off(before).into_iter().map(|(item, _)| item).collect();
        let mut calls = calls?;
        if let Some(Some(Node::ColumnDef(first))) = r.coldeflist.first() {
            let at = place(first.location);
            if calls.len() != 1 {
                let (message, hint) = if r.is_rowsfrom {
                    (
                        "ROWS FROM() with multiple functions cannot have a column definition list",
                        "Put a separate column definition list for each function inside ROWS FROM().",
                    )
                } else {
                    (
                        "UNNEST() with multiple arguments cannot have a column definition list",
                        "Use separate UNNEST() calls inside ROWS FROM(), and attach a column definition list to each one.",
                    )
                };
                return Err(Error::new(SqlState::SYNTAX_ERROR, message).with_hint(hint).at_opt(at));
            }
            if r.ordinality {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "WITH ORDINALITY cannot be used with a column definition list",
                )
                .with_hint("Put the column definition list inside ROWS FROM().")
                .at_opt(at));
            }
            calls[0].2 = &r.coldeflist;
        }
        let alias = r.alias.as_deref();
        let first = calls.first().map(|c| c.1.clone()).unwrap_or_default();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).map_or(first, str::to_string);
        let count = calls.len();
        let mut columns = Vec::new();
        let mut out = Vec::with_capacity(count);
        let mut defined = Vec::with_capacity(count);
        for (expr, name, coldefs) in calls {
            let given = self.function_columns(&expr, &name, coldefs, alias, count)?;
            defined.push(!coldefs.is_empty());
            out.push((expr, given.len()));
            columns.extend(given);
        }
        if r.ordinality {
            let name = "ordinality".to_string();
            columns.push(Column { name, ty: oid::INT8, typmod: -1, not_null: false });
        }
        let function = FromFunction { calls: out, ordinality: r.ordinality, coldefs: defined };
        let mut relation = Relation::new(0, columns, None, Some(function), None);
        relation.lateral = r.lateral;
        self.add_relation(relation, refname, alias)
    }

    /// The calls of a function in `FROM`, each with the name that `FigureColname` gives it and its column definition list. A call of `unnest` with more than one argument and no other parts becomes a call of `pg_catalog.unnest` for each argument, as `ROWS FROM` does.
    fn function_calls<'n>(&mut self, r: &'n RangeFunction) -> Result<Vec<FunctionCall<'n>>> {
        let mut out = Vec::with_capacity(r.functions.len());
        for pair in &r.functions {
            let Some(Node::List(pair)) = pair else {
                return Err(Error::internal("a function in FROM that is not a pair"));
            };
            let node = pair.first().and_then(Option::as_ref);
            let coldefs: &[Option<Node>] = match pair.get(1) {
                Some(Some(Node::List(list))) => list,
                _ => &[],
            };
            if let Some(Node::FuncCall(fc)) = node
                && fc.funcname.len() == 1
                && fields(&fc.funcname) == [Some("unnest")]
                && fc.args.len() > 1
                && fc.agg_order.is_empty()
                && fc.agg_filter.is_none()
                && fc.over.is_none()
                && !fc.agg_star
                && !fc.agg_distinct
                && !fc.func_variadic
                && coldefs.is_empty()
            {
                for arg in &fc.args {
                    let call = FuncCall {
                        funcname: vec![
                            Some(Node::String("pg_catalog".into())),
                            Some(Node::String("unnest".into())),
                        ],
                        args: vec![arg.clone()],
                        funcformat: CoercionForm::COERCE_EXPLICIT_CALL,
                        location: fc.location,
                        ..FuncCall::default()
                    };
                    let call = Node::FuncCall(Box::new(call));
                    let expr = self.function_expr(Some(&call))?;
                    out.push((expr, figure_colname(Some(&call)), &[][..]));
                }
                continue;
            }
            let expr = self.function_expr(node)?;
            if !coldefs.is_empty()
                && let Some(Some(Node::ColumnDef(first))) = r.coldeflist.first()
            {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "multiple column definition lists are not allowed for the same function",
                )
                .at_opt(place(first.location)));
            }
            out.push((expr, figure_colname(node), coldefs));
        }
        Ok(out)
    }

    /// The expression of a function in `FROM`. A set-returning function can only be at the top, because `nodeFunctionscan.c` calls only that function as a set.
    fn function_expr(&mut self, node: Option<&Node>) -> Result<Expr> {
        let expr = self.with_kind(Kind::FromFunction, |a| a.transform(node))?;
        let retset = |e: &Expr| match &e.kind {
            ExprKind::Func(f) => builtin::proc_by_oid(f.oid).is_some_and(|p| p.retset),
            _ => false,
        };
        let mut nested = |e: &Expr, depth: usize| (depth == 0 && retset(e)).then_some(e.location);
        let found = match &expr.kind {
            ExprKind::Func(f) if retset(&expr) => {
                f.args.iter().find_map(|a| a.find(0, &mut nested))
            }
            _ => expr.find(0, &mut nested),
        };
        if let Some(at) = found {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "set-returning functions must appear at top level of FROM",
            )
            .at_opt(at));
        }
        Ok(expr)
    }

    /// `get_expr_result_type` and the column definition list of `addRangeTableEntryForFunction`: the columns of one call of a function in `FROM`. A function with two or more `OUT` parameters gives a column for each, a function that gives `record` takes its columns from the column definition list, and a function of another type gives one column.
    fn function_columns(
        &mut self,
        expr: &Expr,
        name: &str,
        coldefs: &[Option<Node>],
        alias: Option<&Alias>,
        count: usize,
    ) -> Result<Vec<Column>> {
        let proc = match &expr.kind {
            ExprKind::Func(f) => builtin::proc_by_oid(f.oid),
            _ => None,
        };
        // The OUT, INOUT and TABLE parameters, as `build_function_result_tupdesc_d` reads them.
        let mut outs: Vec<(Option<&str>, u32)> = Vec::new();
        if let Some(p) = proc
            && let (Some(types), Some(modes)) = (p.allargtypes, p.argmodes)
        {
            for (i, (ty, mode)) in types.iter().zip(modes).enumerate() {
                if matches!(mode, b'o' | b'b' | b't') {
                    let named =
                        p.argnames.and_then(|n| n.get(i)).copied().filter(|n| !n.is_empty());
                    outs.push((named, *ty));
                }
            }
        }
        let at = coldefs.first().and_then(|c| match c {
            Some(Node::ColumnDef(def)) => place(def.location),
            _ => None,
        });
        if outs.len() > 1 && expr.ty == oid::RECORD {
            if !coldefs.is_empty() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "a column definition list is redundant for a function with OUT parameters",
                )
                .at_opt(at));
            }
            let mut columns = Vec::with_capacity(outs.len());
            for (i, (name, ty)) in outs.into_iter().enumerate() {
                let name = name.map_or_else(|| format!("column{}", i + 1), str::to_string);
                if types::is_polymorphic(ty) {
                    return Err(not_yet(
                        "a function in FROM with polymorphic OUT parameters",
                        expr.location,
                    ));
                }
                columns.push(Column { name, ty, typmod: -1, not_null: false });
            }
            return Ok(columns);
        }
        let row = types::row(expr.ty);
        if expr.ty == oid::RECORD {
            if coldefs.is_empty() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "a column definition list is required for functions returning \"record\"",
                )
                .at_opt(expr.place()));
            }
            return self.column_definitions(coldefs);
        }
        if row.is_some_and(|r| r.kind == b'c') {
            if !coldefs.is_empty() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "a column definition list is redundant for a function returning a named composite type",
                )
                .at_opt(at));
            }
            return Err(not_yet("a function in FROM that returns a composite type", expr.location));
        }
        if !coldefs.is_empty() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "a column definition list is only allowed for functions returning \"record\"",
            )
            .at_opt(at));
        }
        if row.is_some_and(|r| r.kind == b'p') && !matches!(expr.ty, oid::VOID | oid::CSTRING) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "function \"{name}\" in FROM has unsupported return type {}",
                    types::name(expr.ty)
                ),
            )
            .at_opt(expr.place()));
        }
        // chooseScalarFunctionAlias: the name of the one OUT parameter, or else the alias of a single function, or else the name of the function.
        let column = match outs.as_slice() {
            [(Some(n), _)] => (*n).to_string(),
            _ => match alias.and_then(|a| a.aliasname.as_deref()) {
                Some(a) if count == 1 => a.to_string(),
                _ => name.to_string(),
            },
        };
        Ok(vec![Column { name: column, ty: expr.ty, typmod: expr.typmod, not_null: false }])
    }

    /// The columns of a column definition list, with the checks of `CheckAttributeNamesTypes`: no name twice, and no pseudo-type other than `record` and `record[]`.
    fn column_definitions(&mut self, coldefs: &[Option<Node>]) -> Result<Vec<Column>> {
        let mut columns: Vec<Column> = Vec::with_capacity(coldefs.len());
        for def in coldefs {
            let Some(Node::ColumnDef(def)) = def else {
                return Err(Error::internal("a column definition that is not ColumnDef"));
            };
            let name = def.colname.as_deref().unwrap_or_default().to_string();
            let type_name =
                def.typeName.as_deref().ok_or_else(|| Error::internal("a column with no type"))?;
            if type_name.setof {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!("column \"{name}\" cannot be declared SETOF"),
                )
                .at_opt(place(def.location)));
            }
            let (ty, typmod) = self.type_name(type_name)?;
            columns.push(Column { name, ty, typmod, not_null: false });
        }
        for (i, column) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.name == column.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column name \"{}\" specified more than once", column.name),
                ));
            }
        }
        for column in &columns {
            if !matches!(column.ty, oid::RECORD | oid::RECORD_ARRAY) {
                crate::ddl::check_attribute_type(&column.name, column.ty, false)?;
            }
        }
        Ok(columns)
    }

    /// `buildNSItemFromLists`: adds a relation to the range table with the name `refname`, and gives its namespace item. The names of the alias replace the first names of the columns.
    fn add_relation(
        &mut self,
        relation: Relation,
        refname: String,
        alias: Option<&Alias>,
    ) -> Result<Item> {
        let columns = &relation.columns;
        let mut colnames: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        if let Some(alias) = alias {
            let given = alias_columns(alias);
            if given.len() > colnames.len() {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!(
                        "table \"{refname}\" has {} columns available but {} columns specified",
                        colnames.len(),
                        given.len()
                    ),
                ));
            }
            for (slot, name) in colnames.iter_mut().zip(given) {
                *slot = name;
            }
        }
        let index = self.scope.relations.len();
        let exprs: Vec<(String, Expr)> = colnames
            .iter()
            .zip(columns)
            .enumerate()
            .map(|(i, (n, c))| {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let var = Var { relation: index, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), c.ty);
                expr.typmod = c.typmod;
                (n.clone(), expr)
            })
            .collect();
        let oid = relation.oid;
        let mut relation = relation;
        relation.name.clone_from(&refname);
        relation.alias = alias.map(alias_columns);
        self.scope.relations.push(relation);
        let entry = self.scope.entries.len();
        self.scope.entries.push(Entry {
            name: refname.clone(),
            aliased: alias.is_some(),
            columns: colnames,
            relation: Some(index),
            oid,
        });
        Ok(Item { entry, name: refname, columns: exprs, rel_visible: true, cols_visible: true })
    }

    /// `addRangeTableEntryForValues` and `addNSItemToQuery`: adds the relation of a `VALUES` list with the name `*VALUES*`, and gives its index and its columns.
    pub(crate) fn add_values(
        &mut self,
        relation: Relation,
    ) -> Result<(usize, Vec<(String, Expr)>)> {
        let item = self.add_relation(relation, "*VALUES*".to_string(), None)?;
        let index = self.scope.entries[item.entry].relation.unwrap_or_default();
        let columns = item.columns.clone();
        self.scope.namespace.push(item);
        Ok((index, columns))
    }

    /// `addRangeTableEntryForJoin` and `addNSItemToQuery` for the columns of a set operation: an unqualified name in `ORDER BY` can find the columns, and a qualified name cannot. It gives the number of entries and of items before the change, for [`Analyzer::remove_columns`].
    pub(crate) fn add_columns(&mut self, columns: Vec<(String, Expr)>) -> (usize, usize) {
        let before = (self.scope.entries.len(), self.scope.namespace.len());
        let names = columns.iter().map(|(n, _)| n.clone()).collect();
        let entry = self.scope.entries.len();
        self.scope.entries.push(Entry {
            name: "unnamed_join".to_string(),
            aliased: false,
            columns: names,
            relation: None,
            oid: 0,
        });
        self.scope.namespace.push(Item {
            entry,
            name: "unnamed_join".to_string(),
            columns,
            rel_visible: false,
            cols_visible: true,
        });
        before
    }

    /// Removes the entry and the item that [`Analyzer::add_columns`] added.
    pub(crate) fn remove_columns(&mut self, (entries, items): (usize, usize)) {
        self.scope.entries.truncate(entries);
        self.scope.namespace.truncate(items);
    }

    /// `addRangeTableEntryForRelation` and `addNSItemToQuery` for a table of a statement that defines an object: the expressions of the statement can use the columns. Each column is a name, a type and a typmod.
    pub(crate) fn add_table(&mut self, oid: u32, name: &str, columns: &[(String, u32, i32)]) {
        let index = self.scope.relations.len();
        let exprs: Vec<(String, Expr)> = columns
            .iter()
            .enumerate()
            .map(|(i, (n, ty, typmod))| {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let var = Var { relation: index, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), *ty);
                expr.typmod = *typmod;
                (n.clone(), expr)
            })
            .collect();
        let columns: Vec<Column> = columns
            .iter()
            .map(|(n, ty, typmod)| Column {
                name: n.clone(),
                ty: *ty,
                typmod: *typmod,
                not_null: false,
            })
            .collect();
        let colnames = columns.iter().map(|c| c.name.clone()).collect();
        let mut relation = Relation::new(oid, columns, None, None, None);
        relation.name = name.to_string();
        self.scope.relations.push(relation);
        let entry = self.scope.entries.len();
        self.scope.entries.push(Entry {
            name: name.to_string(),
            aliased: false,
            columns: colnames,
            relation: Some(index),
            oid,
        });
        self.scope.namespace.push(Item {
            entry,
            name: name.to_string(),
            columns: exprs,
            rel_visible: true,
            cols_visible: true,
        });
    }

    /// `RangeVarGetRelid`: the OID of the relation that a name gives, or `None`. The relation can be of any kind.
    fn lookup_relation(&self, rv: &RangeVar) -> Result<Option<u32>> {
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default();
        if let Some(database) = &rv.catalogname
            && **database != self.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "cross-database references are not implemented: \"{database}.{}.{name}\"",
                    rv.schemaname.as_deref().unwrap_or_default()
                ),
            )
            .at_opt(at));
        }
        let in_schema = |namespace: u32| {
            builtin::named(Named::Class)
                .iter()
                .find(|r| r.namespace == namespace && r.name == name)
                .map(|r| r.oid)
                .or_else(|| self.env.catalog()?.relation_by_name(namespace, name).map(|r| r.oid))
        };
        match &rv.schemaname {
            // A schema that does not exist has no relation, as RangeVarGetRelid with missing_ok gives.
            Some(schema) => Ok(self.schema(schema).and_then(in_schema)),
            None => Ok(self.path.iter().find_map(|&ns| in_schema(ns))),
        }
    }

    /// The checks of `table_open` for a relation in `FROM`, and the columns of the relation. A table of the catalog, a table of the user and a sequence of the user can be in `FROM`. An index gives `42809`, and a built-in view gives `0A000`.
    fn open_relation(&self, oid: u32, name: &str, at: Option<usize>) -> Result<Vec<Column>> {
        let index_error = || {
            Error::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot open relation \"{name}\""))
                .with_detail("This operation is not supported for indexes.")
                .at_opt(at)
        };
        if let Some(catalog) = rupg_pgcatalog::catalog_by_oid(oid) {
            return Ok(catalog
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.to_string(),
                    ty: c.type_oid,
                    typmod: -1,
                    not_null: c.not_null,
                })
                .collect());
        }
        if let Some(rel) = self.env.catalog().and_then(|c| c.relation(oid)) {
            if rel.kind == RelKind::Index {
                return Err(index_error());
            }
            return Ok(rel
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: c.ty,
                    typmod: c.typmod,
                    not_null: c.not_null,
                })
                .collect());
        }
        if builtin_relkind(oid) == Some(b'i') {
            return Err(index_error());
        }
        Err(not_yet(&format!("the system relation \"{name}\""), at))
    }

    /// `checkNameSpaceConflicts`: two items that the same name qualifies are an error, except two relations with no alias that are not the same relation.
    fn check_conflicts(&self, first: &[Item], second: &[Item]) -> Result<()> {
        for a in first.iter().filter(|i| i.rel_visible) {
            for b in second.iter().filter(|i| i.rel_visible) {
                if a.name != b.name {
                    continue;
                }
                let (ea, eb) = (&self.scope.entries[a.entry], &self.scope.entries[b.entry]);
                let plain = |e: &Entry| e.relation.is_some() && !e.aliased;
                if plain(ea) && plain(eb) && ea.oid != eb.oid {
                    continue;
                }
                return Err(Error::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("table name \"{}\" specified more than once", a.name),
                ));
            }
        }
        Ok(())
    }

    /// The `JoinExpr` case of `transformFromClauseItem`.
    fn join(&mut self, j: &JoinExpr) -> Result<(FromItem, Item, Vec<Item>)> {
        let left_node = j.larg.as_ref().ok_or_else(|| Error::internal("a join with no left"))?;
        let right_node = j.rarg.as_ref().ok_or_else(|| Error::internal("a join with no right"))?;
        let kind = match j.jointype {
            JoinType::JOIN_INNER => JoinKind::Inner,
            JoinType::JOIN_LEFT => JoinKind::Left,
            JoinType::JOIN_RIGHT => JoinKind::Right,
            JoinType::JOIN_FULL => JoinKind::Full,
            _ => return Err(Error::internal("unrecognized join type")),
        };
        let (left, l_item, l_namespace) = self.transform_from_item(left_node)?;
        // The right side can see the left side only with LATERAL, and only for an inner or a left join.
        let before = self.scope.lateral.len();
        let lateral_ok = matches!(kind, JoinKind::Inner | JoinKind::Left);
        self.scope.lateral.extend(l_namespace.iter().map(|item| (item.clone(), lateral_ok)));
        let right = self.transform_from_item(right_node);
        self.scope.lateral.truncate(before);
        let (right, r_item, r_namespace) = right?;
        self.check_conflicts(&l_namespace, &r_namespace)?;
        let mut namespace = l_namespace;
        namespace.extend(r_namespace);
        let l_names: Vec<&str> = l_item.columns.iter().map(|(n, _)| n.as_str()).collect();
        let r_names: Vec<&str> = r_item.columns.iter().map(|(n, _)| n.as_str()).collect();
        let using: Vec<String> = if j.isNatural {
            l_names.iter().filter(|n| r_names.contains(n)).map(|n| (*n).to_string()).collect()
        } else {
            fields(&j.usingClause).into_iter().flatten().map(str::to_string).collect()
        };
        let mut merged: Vec<(String, Expr)> = Vec::new();
        let mut l_used = Vec::new();
        let mut r_used = Vec::new();
        let mut on = None;
        if !using.is_empty() {
            let mut pairs = Vec::new();
            for name in &using {
                if merged.iter().any(|(n, _)| n == name) || pairs.iter().any(|(n, _, _)| n == name)
                {
                    return Err(Error::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column name \"{name}\" appears more than once in USING clause"),
                    ));
                }
                let find = |names: &[&str], side: &str| -> Result<usize> {
                    let mut found = None;
                    for (i, n) in names.iter().enumerate() {
                        if n == name {
                            if found.is_some() {
                                return Err(Error::new(
                                    SqlState::AMBIGUOUS_COLUMN,
                                    format!(
                                        "common column name \"{name}\" appears more than once in {side} table"
                                    ),
                                ));
                            }
                            found = Some(i);
                        }
                    }
                    found.ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{name}\" specified in USING clause does not exist in {side} table"),
                        )
                    })
                };
                let l = find(&l_names, "left")?;
                let r = find(&r_names, "right")?;
                pairs.push((name.clone(), l, r));
            }
            // transformJoinUsingClause: `l = r` for each column, joined with AND.
            let mut conds = Vec::new();
            for (_, l, r) in &pairs {
                let (lv, rv) = (l_item.columns[*l].1.clone(), r_item.columns[*r].1.clone());
                conds.push(self.make_op(&["="], Some(lv), rv, None)?);
            }
            let cond = if conds.len() == 1 {
                conds.remove(0)
            } else {
                Expr::new(ExprKind::Bool(crate::expr::BoolOp::And, conds), oid::BOOL)
            };
            on = Some(self.coerce_to_boolean(cond, "JOIN/USING")?);
            for (name, l, r) in pairs {
                let expr = self.merged_column(
                    kind,
                    l_item.columns[l].1.clone(),
                    r_item.columns[r].1.clone(),
                )?;
                merged.push((name, expr));
                l_used.push(l);
                r_used.push(r);
            }
        } else if let Some(quals) = &j.quals {
            // transformJoinOnClause: the condition sees only the two sides of the join.
            let saved = std::mem::replace(&mut self.scope.namespace, namespace.clone());
            let lateral = std::mem::take(&mut self.scope.lateral);
            let result = self.with_kind(Kind::JoinOn, |a| a.transform(Some(quals)));
            self.scope.namespace = saved;
            self.scope.lateral = lateral;
            on = Some(self.coerce_to_boolean(result?, "JOIN/ON")?);
        }
        let plain_using = merged.iter().all(|(_, e)| matches!(e.kind, ExprKind::Var(_)));
        let merged_values: Vec<Expr> = merged.iter().map(|(_, e)| e.clone()).collect();
        let mut columns = merged;
        let using_count = columns.len();
        for (i, c) in l_item.columns.iter().enumerate() {
            if !l_used.contains(&i) {
                columns.push(c.clone());
            }
        }
        for (i, c) in r_item.columns.iter().enumerate() {
            if !r_used.contains(&i) {
                columns.push(c.clone());
            }
        }
        let alias = j.alias.as_deref();
        if let Some(alias) = alias {
            let given = alias_columns(alias);
            let name = alias.aliasname.as_deref().unwrap_or_default();
            if given.len() > columns.len() {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!(
                        "join expression \"{name}\" has {} columns available but {} columns specified",
                        columns.len(),
                        given.len()
                    ),
                ));
            }
            for (slot, n) in columns.iter_mut().zip(given) {
                slot.0 = n;
            }
        }
        let entry = self.scope.entries.len();
        let name = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or("unnamed_join");
        self.scope.entries.push(Entry {
            name: name.to_string(),
            aliased: alias.is_some(),
            columns: columns.iter().map(|(n, _)| n.clone()).collect(),
            relation: None,
            oid: 0,
        });
        if let Some(using_alias) = j.join_using_alias.as_deref() {
            let item = Item {
                entry,
                name: using_alias.aliasname.as_deref().unwrap_or_default().to_string(),
                columns: columns[..using_count].to_vec(),
                rel_visible: true,
                cols_visible: true,
            };
            self.check_conflicts(std::slice::from_ref(&item), &namespace)?;
            namespace.push(item);
        }
        if alias.is_some() {
            namespace.clear();
        } else {
            for item in &mut namespace {
                item.cols_visible = false;
            }
        }
        let item = Item {
            entry,
            name: name.to_string(),
            columns,
            rel_visible: alias.is_some(),
            cols_visible: true,
        };
        namespace.push(item.clone());
        let join = Join {
            kind,
            left,
            right,
            on,
            plain_using,
            using,
            alias: alias.and_then(|a| a.aliasname.as_deref()).map(str::to_string),
            using_alias: j
                .join_using_alias
                .as_deref()
                .and_then(|a| a.aliasname.as_deref())
                .map(str::to_string),
            merged: merged_values,
        };
        Ok((FromItem::Join(Box::new(join)), item, namespace))
    }

    /// `buildMergedJoinVar`: the value of a column of `USING`.
    fn merged_column(&mut self, kind: JoinKind, l: Expr, r: Expr) -> Result<Expr> {
        let pair = [l, r];
        let ty = self.common_type(&pair, "JOIN/USING")?;
        let [l, r] = pair;
        let typmod = if l.typmod == r.typmod { l.typmod } else { -1 };
        let mut fix = |e: Expr| -> Result<Expr> {
            if e.ty != ty {
                self.coerce(e, ty, typmod, Context::Implicit, None)
            } else if e.typmod != typmod {
                Ok(Expr {
                    kind: ExprKind::Relabel(Box::new(e), CastForm::Implicit),
                    ty,
                    typmod,
                    location: None,
                })
            } else {
                Ok(e)
            }
        };
        let (l, r) = (fix(l)?, fix(r)?);
        let is_var = |e: &Expr| matches!(e.kind, ExprKind::Var(_));
        Ok(match kind {
            JoinKind::Inner if !is_var(&l) && is_var(&r) => r,
            JoinKind::Inner | JoinKind::Left => l,
            JoinKind::Right => r,
            JoinKind::Full => {
                Expr { kind: ExprKind::Coalesce(vec![l, r]), ty, typmod: -1, location: None }
            }
        })
    }

    /// `transformColumnRef`: the value of a column name.
    pub(crate) fn column_ref(&mut self, c: &ColumnRef) -> Result<Expr> {
        let at = place(c.location);
        if self.kind == Kind::ColumnDefault {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot use column reference in DEFAULT expression",
            )
            .at_opt(at));
        }
        let names = fields(&c.fields);
        // `replace_domain_constraint_value`: in a check constraint of a domain, `value` is the value that the domain checks.
        if let Some((ty, typmod)) = self.domain_value
            && names.as_slice() == [Some("value")]
        {
            return Ok(Expr { kind: ExprKind::DomainValue, ty, typmod, location: at });
        }
        let (schema, table, column) = match names.as_slice() {
            [Some(column)] => {
                if let Some(expr) = self.column_by_name(column, at)? {
                    return Ok(expr);
                }
                if self.item_by_name(None, column, at)?.is_some() {
                    return Err(not_yet("a whole-row reference", at));
                }
                return Err(self.missing_column(None, column, at));
            }
            [None] => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "SELECT * with no tables specified is not valid",
                )
                .at_opt(at));
            }
            [Some(table), Some(column)] => (None, *table, *column),
            [Some(schema), Some(table), Some(column)] => (Some(*schema), *table, *column),
            [Some(database), Some(schema), Some(table), Some(column)] => {
                if *database != self.env.database() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.iter().map(|n| n.unwrap_or("*")).collect::<Vec<_>>().join(".")
                        ),
                    )
                    .at_opt(at));
                }
                (Some(*schema), *table, *column)
            }
            [.., None] if names.len() <= 4 => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "a row expansion with * is supported only in the target list",
                )
                .at_opt(at));
            }
            _ => {
                let text: Vec<&str> = names.iter().map(|f| f.unwrap_or("*")).collect();
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {}", text.join(".")),
                )
                .at_opt(at));
            }
        };
        let Some((level, item)) = self.item_by_name(schema, table, at)? else {
            return Err(self.missing_entry(schema, table, at));
        };
        match self.column_in(level, &item, column, at)? {
            Some(expr) => Ok(expr),
            None => Err(self.missing_column(Some(table), column, at)),
        }
    }

    /// The scopes of the query and of the queries outside it, with their levels: 0 for the query, 1 for the query outside it, and so on.
    fn levels(&self) -> impl Iterator<Item = (usize, &Scope)> {
        std::iter::once(&self.scope).chain(self.outer.iter().rev()).enumerate()
    }

    /// The scope of a level.
    pub(crate) fn level(&self, level: usize) -> &Scope {
        self.levels().nth(level).map_or(&self.scope, |(_, scope)| scope)
    }

    /// `scanNSItemForColumn`: the column of an item of a level with this name, or `None`. The columns of the expression are `levels_up` of the level.
    fn column_in(
        &self,
        level: usize,
        item: &Item,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<Expr>> {
        let mut found = None;
        for (n, expr) in &item.columns {
            if n == name {
                if found.is_some() {
                    return Err(ambiguous(name, at));
                }
                found = Some(expr);
            }
        }
        if let Some(expr) = found {
            let mut expr = expr.clone().at(at);
            expr.raise(level);
            return Ok(Some(expr));
        }
        let entry = &self.level(level).entries[item.entry];
        let Some(relation) = entry.relation else { return Ok(None) };
        let view = self.level(level).relations.get(relation).is_some_and(|r| r.subquery.is_some());
        if item.columns.len() != entry.columns.len() || entry.oid == 0 || view {
            // The item of a USING alias gives only the columns of USING, and a subquery and a view have no system columns.
            return Ok(None);
        }
        if name == "tableoid" {
            let var = Var { relation, attnum: TABLE_OID_ATTNUM, levels_up: level };
            return Ok(Some(Expr::new(ExprKind::Var(var), oid::OID).at(at)));
        }
        if OTHER_SYSTEM_COLUMNS.contains(&name) {
            if self.kind == Kind::Check {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!("system column \"{name}\" reference in check constraint is invalid"),
                )
                .at_opt(at));
            }
            return Err(not_yet(&format!("the system column \"{name}\""), at));
        }
        Ok(None)
    }

    /// The name of the relation and the name of the column of a `Var`, as `eref->aliasname` and `get_rte_attribute_name` give them for an error.
    pub(crate) fn column_names(&self, var: Var) -> (String, String) {
        let scope = self.level(var.levels_up);
        let Some(entry) = scope.entries.iter().find(|e| e.relation == Some(var.relation)) else {
            return (String::new(), String::new());
        };
        let column = match usize::try_from(var.attnum) {
            Ok(n) if n > 0 => entry.columns.get(n - 1).cloned().unwrap_or_default(),
            _ => "tableoid".to_string(),
        };
        (entry.name.clone(), column)
    }

    /// `colNameToVar`: the column that an unqualified name gives, or `None`. The search goes up the query levels and stops at the first level that has the name.
    pub(crate) fn column_by_name(&self, name: &str, at: Option<usize>) -> Result<Option<Expr>> {
        for (level, scope) in self.levels() {
            let mut result = None;
            for (item, ok) in scope.visible().filter(|(i, _)| i.cols_visible) {
                if let Some(expr) = self.column_in(level, item, name, at)? {
                    if result.is_some() {
                        return Err(ambiguous(name, at));
                    }
                    check_lateral_ok(item, ok, at)?;
                    result = Some(expr);
                }
            }
            if result.is_some() {
                return Ok(result);
            }
        }
        Ok(None)
    }

    /// `refnameNamespaceItem`: the item that a table name gives and its level, or `None`. The search goes up the query levels and stops at the first level that has the name.
    fn item_by_name(
        &self,
        schema: Option<&str>,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<(usize, Item)>> {
        for (level, scope) in self.levels() {
            if let Some(item) = self.item_in(scope, schema, name, at)? {
                return Ok(Some((level, item)));
            }
        }
        Ok(None)
    }

    /// `scanNameSpaceForRefname` and `scanNameSpaceForRelid`: the item of a scope that a table name gives, or `None`.
    fn item_in(
        &self,
        scope: &Scope,
        schema: Option<&str>,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<Item>> {
        if let Some(schema) = schema {
            // scanNameSpaceForRelid: a relation with no alias in that schema.
            let rv = RangeVar {
                schemaname: Some(schema.into()),
                relname: Some(name.into()),
                location: at.map_or(-1, |a| i32::try_from(a).unwrap_or(-1)),
                ..RangeVar::default()
            };
            let Some(oid) = self.lookup_relation(&rv)? else { return Ok(None) };
            let mut result = None;
            for (item, ok) in scope.visible().filter(|(i, _)| i.rel_visible) {
                let entry = &scope.entries[item.entry];
                if entry.relation.is_some() && !entry.aliased && entry.oid == oid {
                    if result.is_some() {
                        return Err(Error::new(
                            SqlState::AMBIGUOUS_ALIAS,
                            format!("table reference {oid} is ambiguous"),
                        )
                        .at_opt(at));
                    }
                    check_lateral_ok(item, ok, at)?;
                    result = Some(item.clone());
                }
            }
            return Ok(result);
        }
        let mut result = None;
        for (item, ok) in scope.visible().filter(|(i, _)| i.rel_visible) {
            if item.name == name {
                if result.is_some() {
                    return Err(Error::new(
                        SqlState::AMBIGUOUS_ALIAS,
                        format!("table reference \"{name}\" is ambiguous"),
                    )
                    .at_opt(at));
                }
                check_lateral_ok(item, ok, at)?;
                result = Some(item.clone());
            }
        }
        Ok(result)
    }

    /// `errorMissingRTE`.
    fn missing_entry(&self, schema: Option<&str>, name: &str, at: Option<usize>) -> Error {
        // searchRangeTableForRel: an entry for the relation or with the name.
        let rv = RangeVar {
            schemaname: schema.map(Into::into),
            relname: Some(name.into()),
            ..RangeVar::default()
        };
        let oid = self.lookup_relation(&rv).ok().flatten();
        let found = self.levels().find_map(|(level, scope)| {
            let index = scope
                .entries
                .iter()
                .position(|e| (e.relation.is_some() && Some(e.oid) == oid) || e.name == name)?;
            Some((level, index))
        });
        let Some((level, found)) = found else {
            return Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("missing FROM-clause entry for table \"{name}\""),
            )
            .at_opt(at);
        };
        let entry = &self.level(level).entries[found];
        let error = Error::new(
            SqlState::UNDEFINED_TABLE,
            format!("invalid reference to FROM-clause entry for table \"{name}\""),
        )
        .at_opt(at);
        if entry.aliased && entry.name != name {
            let visible = matches!(
                self.item_by_name(None, &entry.name, None),
                Ok(Some((l, item))) if l == level && item.entry == found
            );
            if visible {
                return error.with_hint(format!(
                    "Perhaps you meant to reference the table alias \"{}\".",
                    entry.name
                ));
            }
        }
        let error = error.with_detail(format!(
            "There is an entry for table \"{}\", but it cannot be referenced from this part of the query.",
            entry.name
        ));
        if self.visible_if_lateral((level, found)) {
            return error
                .with_hint("To reference that table, you must mark this subquery with LATERAL.");
        }
        error
    }

    /// `rte_visible_if_lateral`: true when a `LATERAL` subquery could see the entry.
    fn visible_if_lateral(&self, (level, index): Place) -> bool {
        if self.scope.lateral_active {
            return false;
        }
        self.level(level).lateral.iter().any(|(item, ok)| item.entry == index && *ok)
    }

    /// `errorMissingColumn`, with the hint of a close name.
    fn missing_column(&self, table: Option<&str>, column: &str, at: Option<usize>) -> Error {
        let mut fuzzy = Fuzzy {
            distance: MAX_FUZZY_DISTANCE + 1,
            first: None,
            second: None,
            exact: Vec::new(),
        };
        let entries = self.levels().flat_map(|(level, scope)| {
            scope.entries.iter().enumerate().map(move |(index, entry)| ((level, index), entry))
        });
        for (index, entry) in entries {
            if entry.relation.is_none() {
                continue;
            }
            let penalty =
                table.map_or(0, |t| levenshtein(t, &entry.name).min(MAX_FUZZY_DISTANCE + 1));
            let mut exact = false;
            for (i, name) in entry.columns.iter().enumerate() {
                if name == column {
                    exact = true;
                }
                fuzzy.update(penalty, index, name, column, i);
            }
            if (exact || (column == "tableoid" && entry.oid != 0)) && penalty == 0 {
                fuzzy.exact.push(index);
            }
        }
        let message = match table {
            Some(t) => format!("column {t}.{column} does not exist"),
            None => format!("column \"{column}\" does not exist"),
        };
        let error = Error::new(SqlState::UNDEFINED_COLUMN, message).at_opt(at);
        let entry = |(level, index): Place| &self.level(level).entries[index];
        if fuzzy.exact.len() > 1 {
            let error = error.with_detail(format!("There are columns named \"{column}\", but they are in tables that cannot be referenced from this part of the query."));
            return if table.is_none() {
                error.with_hint("Try using a table-qualified name.")
            } else {
                error
            };
        }
        if let Some(&index) = fuzzy.exact.first() {
            let error = error.with_detail(format!("There is a column named \"{column}\" in table \"{}\", but it cannot be referenced from this part of the query.", entry(index).name));
            // rte_visible_if_qualified: the item has a name that a query can qualify.
            let qualified = self.levels().any(|(level, scope)| {
                level == index.0
                    && scope.namespace.iter().any(|i| i.entry == index.1 && i.rel_visible)
            });
            return if self.visible_if_lateral(index) {
                error.with_hint(
                    "To reference that column, you must mark this subquery with LATERAL.",
                )
            } else if table.is_none() && qualified {
                error.with_hint("To reference that column, you must use a table-qualified name.")
            } else {
                error
            };
        }
        let name = |(e, c): (Place, usize)| format!("{}.{}", entry(e).name, entry(e).columns[c]);
        match (fuzzy.first, fuzzy.second) {
            (Some(first), None) => error.with_hint(format!(
                "Perhaps you meant to reference the column \"{}\".",
                name(first)
            )),
            (Some(first), Some(second)) => error.with_hint(format!(
                "Perhaps you meant to reference the column \"{}\" or the column \"{}\".",
                name(first),
                name(second)
            )),
            _ => error,
        }
    }

    /// `ExpandColumnRefStar`: the columns of `*` or `t.*` in a target list, with their names.
    pub(crate) fn expand_star(&mut self, c: &ColumnRef) -> Result<Option<Vec<(String, Expr)>>> {
        let at = place(c.location);
        let names = fields(&c.fields);
        let Some((None, qualifier)) = names.split_last() else { return Ok(None) };
        let items: Vec<Item> = match qualifier {
            [] => {
                let visible: Vec<Item> =
                    self.scope.namespace.iter().filter(|i| i.cols_visible).cloned().collect();
                if visible.is_empty() {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid",
                    )
                    .at_opt(at));
                }
                visible
            }
            [Some(table)] | [Some(_), Some(table)] | [Some(_), Some(_), Some(table)] => {
                let schema = match qualifier {
                    [Some(schema), _] | [_, Some(schema), _] => Some(*schema),
                    _ => None,
                };
                if let [Some(database), _, _] = qualifier
                    && *database != self.env.database()
                {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.iter().map(|n| n.unwrap_or("*")).collect::<Vec<_>>().join(".")
                        ),
                    )
                    .at_opt(at));
                }
                match self.item_by_name(schema, table, at)? {
                    Some((0, item)) => vec![item],
                    Some(_) => {
                        return Err(not_yet("a row expansion of a relation of an outer query", at));
                    }
                    None => return Err(self.missing_entry(schema, table, at)),
                }
            }
            _ => return Ok(None),
        };
        let mut columns = Vec::new();
        for item in items {
            for (name, expr) in item.columns {
                columns.push((name, expr.at(at)));
            }
        }
        Ok(Some(columns))
    }
}

/// The error of a name that two columns have.
fn ambiguous(name: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::AMBIGUOUS_COLUMN, format!("column reference \"{name}\" is ambiguous"))
        .at_opt(at)
}

/// The error of a part of `FROM` that the analyzer does not take yet.
/// The `relkind` of a built-in relation, from the static rows of `pg_class`.
fn builtin_relkind(oid: u32) -> Option<u8> {
    let class = rupg_pgcatalog::catalog("pg_class")?;
    let (Values::Oid(oids), Values::Char(kinds)) =
        (&class.rows[class.column("oid")?].values, &class.rows[class.column("relkind")?].values)
    else {
        return None;
    };
    oids.iter().position(|&o| o == oid).map(|row| kinds[row])
}

fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

#[cfg(test)]
mod tests {
    use super::levenshtein;

    #[test]
    fn distances() {
        assert_eq!(levenshtein("relname", "relnam"), 1);
        assert_eq!(levenshtein("oid", "iod"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}

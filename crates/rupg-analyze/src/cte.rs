//! `parse_cte.c`: the queries of `WITH` and `WITH RECURSIVE`. The queries of `WITH RECURSIVE` get an order with no forward reference, and each recursive query must have the form `non-recursive-term UNION [ALL] recursive-term` with one reference to itself in the recursive term.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    CTEMaterialize, CommonTableExpr, JoinType, List, Node, RangeVar, SelectStmt, SetOperation,
    WithClause,
};
use rupg_types::oid;

use crate::Analyzer;
use crate::coerce::AtOpt;
use crate::expr::Expr;
use crate::from::{Column, Item, Relation};
use crate::searchcycle::{self, CycleClause, SearchClause};
use crate::select::Query;
use crate::sort::{list_location, raw_location};
use crate::typename::place;
use crate::types;
use crate::walk::{self, Walker};

/// A query of `WITH` after the analysis, as a `CommonTableExpr` of the list `cteList` of a query.
#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    /// `ctename`.
    pub name: String,
    /// The names of the columns after the name of the query, as `aliascolnames`.
    pub aliases: Vec<String>,
    /// The columns of the query, with the names of [`Cte::aliases`] first, then the columns of [`Cte::search`] and [`Cte::cycle`].
    pub columns: Vec<Column>,
    /// The query as the statement gives it. With `SEARCH` or `CYCLE`, each reference reads the query after the rewrite of the clauses.
    pub query: Box<Query>,
    /// `cterecursive`: the query reads itself.
    pub recursive: bool,
    /// `Some(true)` for `MATERIALIZED`, `Some(false)` for `NOT MATERIALIZED`, `None` when the query gives neither.
    pub materialized: Option<bool>,
    /// `search_clause`.
    pub search: Option<SearchClause>,
    /// `cycle_clause`.
    pub cycle: Option<CycleClause>,
    /// For an `INSERT`, an `UPDATE`, a `DELETE` or a `MERGE`, the number of the statement among the statements of `WITH` of the statement that change data, in the order of their analysis. Then [`Cte::query`] is the statement, and a reference reads the rows of its `RETURNING`.
    pub write: Option<usize>,
}

/// The query of `WITH` that a relation of `FROM` reads, as a `RangeTblEntry` of the kind `RTE_CTE`.
#[derive(Clone, Debug, PartialEq)]
pub struct CteRef {
    /// `ctename`.
    pub name: String,
    /// For the reference of a recursive query to itself, as `self_reference`, the number of the work table that the recursive `UNION` fills at each step. The relation then has no subquery. Another reference has a copy of the query of `WITH` as its subquery.
    pub work: Option<usize>,
    /// For a reference to a statement that changes data, the number of [`Cte::write`]. The relation then has no subquery, and its rows are the rows of `RETURNING` of the statement.
    pub write: Option<usize>,
    /// The table and the column of each column of the reference to a statement that changes data, from the targets of its `RETURNING`.
    pub origins: Vec<Option<(u32, i16)>>,
    /// True for a transition table of the trigger that runs, as a `RangeTblEntry` of the kind `RTE_NAMEDTUPLESTORE`. The relation then has no subquery, and its rows are the old or the new rows of the write that fired the trigger.
    pub transition: bool,
}

/// A query of `WITH` in the scope of a query, as an item of `p_ctenamespace`.
#[derive(Clone, Debug)]
pub(crate) struct CteItem {
    name: String,
    /// The place of the name of the query.
    location: Option<usize>,
    aliases: Vec<String>,
    recursive: bool,
    materialized: Option<bool>,
    /// False for a query of `WITH` with no `RECURSIVE` before the analysis of its query ends. Then a name cannot find it, but the error of the name tells about it, as `p_future_ctes` does.
    visible: bool,
    /// The columns, after the analysis of the query, or after the non-recursive term of a recursive query.
    columns: Option<Vec<Column>>,
    /// The query, after its analysis, and after the rewrite of `SEARCH` and `CYCLE`.
    query: Option<Box<Query>>,
    /// The query before the rewrite of `SEARCH` and `CYCLE`.
    written: Option<Box<Query>>,
    /// The number of the work table of a recursive query.
    work: Option<usize>,
    search: Option<SearchClause>,
    cycle: Option<CycleClause>,
    /// The number of a statement that changes data, as [`Cte::write`].
    write: Option<usize>,
    /// True when the statement that changes data has `RETURNING`.
    returning: bool,
    /// The origins of the columns of `RETURNING`, as [`CteRef::origins`].
    origins: Vec<Option<(u32, i16)>>,
}

impl CteItem {
    fn new(cte: &CommonTableExpr, recursive: bool, visible: bool) -> CteItem {
        let materialized = match cte.ctematerialized {
            CTEMaterialize::CTEMaterializeAlways => Some(true),
            CTEMaterialize::CTEMaterializeNever => Some(false),
            _ => None,
        };
        CteItem {
            name: cte_name(cte).to_string(),
            location: place(cte.location),
            aliases: strings(&cte.aliascolnames),
            recursive,
            materialized,
            visible,
            columns: None,
            query: None,
            written: None,
            work: None,
            search: None,
            cycle: None,
            write: None,
            returning: false,
            origins: Vec::new(),
        }
    }

    /// The columns of a reference to the query: the columns of the query, then the columns of `SEARCH` and `CYCLE`.
    fn reference_columns(&self) -> Option<Vec<Column>> {
        let mut columns = self.columns.clone()?;
        columns.extend(searchcycle::extra_columns(self.search.as_ref(), self.cycle.as_ref()));
        Some(columns)
    }

    /// The query of `WITH` of the query tree.
    fn into_cte(self) -> Result<Cte> {
        let (Some(columns), Some(query)) = (self.reference_columns(), self.query) else {
            return Err(Error::internal("a query of WITH with no analysis"));
        };
        Ok(Cte {
            name: self.name,
            aliases: self.aliases,
            columns,
            query: self.written.unwrap_or(query),
            recursive: self.recursive,
            materialized: self.materialized,
            search: self.search,
            cycle: self.cycle,
            write: self.write,
        })
    }

    /// `analyzeCTETargetList`: the columns of the query from the name, the type and the typmod of each column of its result. The names of the alias come first. A column of type `unknown` of a recursive query becomes `text`, because the recursive term must know the type.
    fn columns(&self, shown: Vec<(String, u32, i32)>) -> Result<Vec<Column>> {
        let mut columns = Vec::with_capacity(shown.len());
        for (i, (name, ty, typmod)) in shown.into_iter().enumerate() {
            let name = self.aliases.get(i).cloned().unwrap_or(name);
            let (ty, typmod) =
                if self.recursive && ty == oid::UNKNOWN { (oid::TEXT, -1) } else { (ty, typmod) };
            columns.push(Column { name, ty, typmod, not_null: false });
        }
        if columns.len() < self.aliases.len() {
            return Err(Error::new(
                SqlState::INVALID_COLUMN_REFERENCE,
                format!(
                    "WITH query \"{}\" has {} columns available but {} columns specified",
                    self.name,
                    columns.len(),
                    self.aliases.len()
                ),
            )
            .at_opt(self.location));
        }
        Ok(columns)
    }
}

/// The name of a query of `WITH`.
fn cte_name(cte: &CommonTableExpr) -> &str {
    cte.ctename.as_deref().unwrap_or_default()
}

/// The strings of a list of `String` nodes.
fn strings(list: &List) -> Vec<String> {
    list.iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// The queries of a `WITH` clause.
fn ctes(with: &WithClause) -> Vec<&CommonTableExpr> {
    with.ctes
        .iter()
        .filter_map(|n| match n {
            Some(Node::CommonTableExpr(cte)) => Some(&**cte),
            _ => None,
        })
        .collect()
}

/// The name of a relation of `FROM` that can be a query of `WITH`: a name with no schema.
fn unqualified(rv: &RangeVar) -> Option<&str> {
    match rv.schemaname {
        Some(_) => None,
        None => Some(rv.relname.as_deref().unwrap_or_default()),
    }
}

/// The names of the queries of the `WITH` clauses inside the query that the walk reads, which hide the queries of the outer `WITH`, as `innerwiths`.
#[derive(Default)]
struct Inner(Vec<Vec<String>>);

impl Inner {
    fn hides(&self, name: &str) -> bool {
        self.0.iter().flatten().any(|n| n == name)
    }
}

/// `makeDependencyGraphWalker`: the queries of the same `WITH RECURSIVE` that a query reads, and if it reads itself.
struct Deps<'a> {
    names: &'a [&'a str],
    current: usize,
    inner: Inner,
    depends: Vec<usize>,
    recursive: bool,
}

impl Deps<'_> {
    /// `WalkInnerWith`: a `SELECT` with a `WITH` clause. With `RECURSIVE`, all its queries see all its names. Else each query sees the names before it, and the `SELECT` sees all of them.
    fn inner_with(&mut self, s: &SelectStmt, with: &WithClause) -> bool {
        let list = ctes(with);
        if with.recursive {
            self.inner.0.push(list.iter().map(|c| cte_name(c).to_string()).collect());
            for cte in &list {
                walk::one(self, cte.ctequery.as_ref());
            }
        } else {
            self.inner.0.push(Vec::new());
            for cte in &list {
                walk::one(self, cte.ctequery.as_ref());
                if let Some(level) = self.inner.0.last_mut() {
                    level.push(cte_name(cte).to_string());
                }
            }
        }
        walk::select(self, s);
        self.inner.0.pop();
        false
    }
}

impl Walker for Deps<'_> {
    fn node(&mut self, node: &Node) -> bool {
        match node {
            Node::RangeVar(rv) => {
                if let Some(name) = unqualified(rv)
                    && !self.inner.hides(name)
                    && let Some(i) = self.names.iter().position(|n| *n == name)
                {
                    if i == self.current {
                        self.recursive = true;
                    } else if !self.depends.contains(&i) {
                        self.depends.push(i);
                    }
                }
                false
            }
            Node::SelectStmt(s) => self.select(s),
            Node::WithClause(_) => false,
            _ => walk::walk(self, node),
        }
    }

    fn select(&mut self, s: &SelectStmt) -> bool {
        match s.withClause.as_deref() {
            Some(with) => self.inner_with(s, with),
            None => walk::select(self, s),
        }
    }
}

/// `RecursionContext`: the place of the walk in the query, where a reference to the recursive query can be an error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    Ok,
    NonRecursiveTerm,
    SubLink,
    OuterJoin,
    Intersect,
    Except,
}

impl Context {
    /// `recursion_errormsgs`.
    fn message(self, name: &str) -> String {
        let place = match self {
            Context::Ok => "",
            Context::NonRecursiveTerm => "its non-recursive term",
            Context::SubLink => "a subquery",
            Context::OuterJoin => "an outer join",
            Context::Intersect => "INTERSECT",
            Context::Except => "EXCEPT",
        };
        format!("recursive reference to query \"{name}\" must not appear within {place}")
    }
}

/// `checkWellFormedRecursionWalker`: the references of a recursive query to itself, which must be one, in the recursive term, not in a subquery or on the null side of an outer join.
struct Recursion<'a> {
    name: &'a str,
    inner: Inner,
    count: usize,
    context: Context,
    error: Option<Error>,
}

impl Recursion<'_> {
    fn new(name: &str, context: Context) -> Recursion<'_> {
        Recursion { name, inner: Inner::default(), count: 0, context, error: None }
    }

    /// Keeps the error and stops the walk.
    fn fail(&mut self, error: Error) -> bool {
        self.error = Some(error);
        true
    }

    /// `checkWellFormedSelectStmt`: in the recursive term, the reference cannot be on the right side of `EXCEPT`, or in `INTERSECT ALL` or `EXCEPT ALL`.
    fn select_stmt(&mut self, s: &SelectStmt) -> bool {
        let save = self.context;
        if save != Context::Ok {
            return walk::select(self, s);
        }
        let (left, right) = match s.op {
            SetOperation::SETOP_INTERSECT if s.all => (Context::Intersect, Context::Intersect),
            SetOperation::SETOP_INTERSECT => (save, save),
            SetOperation::SETOP_EXCEPT if s.all => (Context::Except, Context::Except),
            SetOperation::SETOP_EXCEPT => (save, Context::Except),
            _ => return walk::select(self, s),
        };
        self.context = left;
        if s.larg.as_deref().is_some_and(|l| self.select(l)) {
            return true;
        }
        self.context = right;
        if s.rarg.as_deref().is_some_and(|r| self.select(r)) {
            return true;
        }
        self.context = save;
        walk::list(self, &s.sortClause)
            || walk::one(self, s.limitOffset.as_ref())
            || walk::one(self, s.limitCount.as_ref())
            || walk::list(self, &s.lockingClause)
    }

    /// A walk of `node` with the context `context`, after which the context is `save` again.
    fn within(&mut self, context: Context, save: Context, node: Option<&Node>) -> bool {
        self.context = context;
        let stop = walk::one(self, node);
        self.context = save;
        stop
    }
}

impl Walker for Recursion<'_> {
    fn node(&mut self, node: &Node) -> bool {
        let save = self.context;
        let outer = if save == Context::Ok { Context::OuterJoin } else { save };
        match node {
            Node::RangeVar(rv) => {
                let Some(name) = unqualified(rv) else { return false };
                if self.inner.hides(name) || name != self.name {
                    return false;
                }
                let at = place(rv.location);
                if self.context != Context::Ok {
                    let message = self.context.message(self.name);
                    return self.fail(Error::new(SqlState::INVALID_RECURSION, message).at_opt(at));
                }
                self.count += 1;
                if self.count > 1 {
                    let message = format!(
                        "recursive reference to query \"{}\" must not appear more than once",
                        self.name
                    );
                    return self.fail(Error::new(SqlState::INVALID_RECURSION, message).at_opt(at));
                }
                false
            }
            Node::SelectStmt(s) => self.select(s),
            Node::WithClause(_) => false,
            Node::JoinExpr(j) => {
                let (larg, rarg) = (j.larg.as_ref(), j.rarg.as_ref());
                let sides = match j.jointype {
                    JoinType::JOIN_LEFT => walk::one(self, larg) || self.within(outer, save, rarg),
                    JoinType::JOIN_FULL => {
                        self.within(outer, save, larg) || self.within(outer, save, rarg)
                    }
                    JoinType::JOIN_RIGHT => self.within(outer, save, larg) || walk::one(self, rarg),
                    _ => walk::one(self, larg) || walk::one(self, rarg),
                };
                sides || walk::one(self, j.quals.as_ref())
            }
            Node::SubLink(s) => {
                self.within(Context::SubLink, save, s.subselect.as_ref())
                    || walk::one(self, s.testexpr.as_ref())
            }
            _ => walk::walk(self, node),
        }
    }

    fn select(&mut self, s: &SelectStmt) -> bool {
        let Some(with) = s.withClause.as_deref() else { return self.select_stmt(s) };
        let list = ctes(with);
        if with.recursive {
            self.inner.0.push(list.iter().map(|c| cte_name(c).to_string()).collect());
            for cte in &list {
                if walk::one(self, cte.ctequery.as_ref()) {
                    return true;
                }
            }
        } else {
            self.inner.0.push(Vec::new());
            for cte in &list {
                if walk::one(self, cte.ctequery.as_ref()) {
                    return true;
                }
                if let Some(level) = self.inner.0.last_mut() {
                    level.push(cte_name(cte).to_string());
                }
            }
        }
        let stop = self.select_stmt(s);
        self.inner.0.pop();
        stop
    }
}

/// Runs a walk of the recursion check and gives its error.
fn check_walk(
    name: &str,
    context: Context,
    f: impl FnOnce(&mut Recursion<'_>) -> bool,
) -> Result<usize> {
    let mut walker = Recursion::new(name, context);
    f(&mut walker);
    match walker.error {
        Some(error) => Err(error),
        None => Ok(walker.count),
    }
}

/// `checkWellFormedRecursion` for one recursive query.
fn check_recursion(cte: &CommonTableExpr) -> Result<()> {
    let name = cte_name(cte);
    let Some(Node::SelectStmt(stmt)) = &cte.ctequery else {
        return Err(Error::new(
            SqlState::INVALID_RECURSION,
            format!("recursive query \"{name}\" must not contain data-modifying statements"),
        )
        .at_opt(place(cte.location)));
    };
    if stmt.op != SetOperation::SETOP_UNION {
        return Err(Error::new(
            SqlState::INVALID_RECURSION,
            format!(
                "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
            ),
        )
        .at_opt(place(cte.location)));
    }
    // The query of a `WITH` clause on the `UNION` must not read the recursive query.
    if let Some(with) = stmt.withClause.as_deref() {
        check_walk(name, Context::SubLink, |w| walk::list(w, &with.ctes))?;
    }
    let not_implemented = |what: &str, at: Option<usize>| {
        Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{what} in a recursive query is not implemented"),
        )
        .at_opt(at))
    };
    if !stmt.sortClause.is_empty() {
        return not_implemented("ORDER BY", list_location(&stmt.sortClause));
    }
    if stmt.limitOffset.is_some() {
        return not_implemented("OFFSET", raw_location(stmt.limitOffset.as_ref()));
    }
    if stmt.limitCount.is_some() {
        return not_implemented("LIMIT", raw_location(stmt.limitCount.as_ref()));
    }
    if !stmt.lockingClause.is_empty() {
        return not_implemented("FOR UPDATE/SHARE", None);
    }
    let larg = stmt.larg.as_deref();
    check_walk(name, Context::NonRecursiveTerm, |w| larg.is_some_and(|l| w.select(l)))?;
    let rarg = stmt.rarg.as_deref();
    let count = check_walk(name, Context::Ok, |w| rarg.is_some_and(|r| w.select(r)))?;
    if count != 1 {
        return Err(Error::internal("missing recursive reference"));
    }
    Ok(())
}

/// `IncrementVarSublevelsUp` for a copy of the query of `WITH` that a query `levels` levels below the `WITH` reads: the columns of the queries outside the query of `WITH` are `levels` more levels up.
fn raise_outer(query: &mut Query, levels: usize) {
    query.each_expr_mut(0, &mut |expr, depth| expr.raise_from(levels, depth + 1));
}

impl Analyzer<'_> {
    /// `transformWithClause`: the queries of a `WITH` clause, which the query and the queries after them can read. The queries of `WITH RECURSIVE` all see each other, and their analysis has an order with no forward reference.
    pub(crate) fn transform_with(&mut self, with: &WithClause) -> Result<()> {
        let list = ctes(with);
        for (i, cte) in list.iter().enumerate() {
            if let Some(again) = list[i + 1..].iter().find(|c| cte_name(c) == cte_name(cte)) {
                return Err(Error::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("WITH query name \"{}\" specified more than once", cte_name(again)),
                )
                .at_opt(place(again.location)));
            }
            // A statement that changes data runs once, so only the statement at the top can have it.
            if !matches!(cte.ctequery, Some(Node::SelectStmt(_))) && !self.outer.is_empty() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "WITH clause containing a data-modifying statement must be at the top level",
                )
                .at_opt(place(cte.location)));
            }
        }
        let base = self.scope.ctes.len();
        if !with.recursive {
            self.scope.ctes.extend(list.iter().map(|c| CteItem::new(c, false, false)));
            for (i, cte) in list.iter().enumerate() {
                self.analyze_cte(base + i, cte)?;
                self.scope.ctes[base + i].visible = true;
            }
            return Ok(());
        }
        // makeDependencyGraph and TopologicalSort.
        let names: Vec<&str> = list.iter().map(|c| cte_name(c)).collect();
        let mut order = Vec::with_capacity(list.len());
        for (i, cte) in list.iter().enumerate() {
            let mut deps = Deps {
                names: &names,
                current: i,
                inner: Inner::default(),
                depends: Vec::new(),
                recursive: false,
            };
            walk::one(&mut deps, cte.ctequery.as_ref());
            order.push((i, deps.depends, deps.recursive));
        }
        for i in 0..order.len() {
            let Some(j) = (i..order.len()).find(|&j| order[j].1.is_empty()) else {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "mutual recursion between WITH items is not implemented",
                )
                .at_opt(place(list[order[i].0].location)));
            };
            order.swap(i, j);
            let id = order[i].0;
            for later in &mut order[i + 1..] {
                later.1.retain(|d| *d != id);
            }
        }
        for (id, _, recursive) in &order {
            if *recursive {
                check_recursion(list[*id])?;
            }
        }
        self.scope
            .ctes
            .extend(order.iter().map(|(id, _, rec)| CteItem::new(list[*id], *rec, true)));
        for (k, (id, _, _)) in order.iter().enumerate() {
            self.analyze_cte(base + k, list[*id])?;
        }
        Ok(())
    }

    /// `analyzeCTE`: the analysis of the query of `WITH` in the slot `slot` of the scope. The query of a recursive query must give the types of its non-recursive term.
    fn analyze_cte(&mut self, slot: usize, cte: &CommonTableExpr) -> Result<()> {
        let stmt = match &cte.ctequery {
            Some(Node::SelectStmt(stmt)) => stmt,
            Some(node) => return self.analyze_write_cte(slot, cte, node),
            None => return Err(Error::internal("a query of WITH with no statement")),
        };
        // The type of the mark comes before the analysis of the query, which can read the mark.
        self.scope.ctes[slot].search = cte.search_clause.as_deref().map(searchcycle::search_clause);
        if let Some(raw) = cte.cycle_clause.as_deref() {
            self.scope.ctes[slot].cycle = Some(self.cycle_clause(raw)?);
        }
        if self.scope.ctes[slot].recursive {
            self.scope.ctes[slot].work = Some(self.works);
            self.works += 1;
        }
        self.parent_cte = Some(slot);
        // The query sees the namespace of this query, as `parse_sub_analyze` with no `p_lateral_only`. The namespace is empty before `FROM`, except for `old` and `new` of a rule action.
        let query = self.sub_select(stmt, true);
        self.parent_cte = None;
        let query = query?;
        let item = &self.scope.ctes[slot];
        let shown = query.targets.iter().filter(|t| !t.junk);
        if item.recursive {
            let Some(columns) = &item.columns else {
                return Err(Error::internal("a recursive query with no columns"));
            };
            for (i, (target, column)) in shown.zip(columns).enumerate() {
                let expr = &target.expr;
                if expr.ty != column.ty || expr.typmod != column.typmod {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                            item.name,
                            i + 1,
                            types::with_typmod(column.ty, column.typmod),
                            types::with_typmod(expr.ty, expr.typmod)
                        ),
                    )
                    .with_hint("Cast the output of the non-recursive term to the correct type.")
                    .at_opt(expr.place()));
                }
            }
        } else {
            let shown = shown.map(|t| (t.name.clone(), t.expr.ty, t.expr.typmod)).collect();
            let columns = item.columns(shown)?;
            self.scope.ctes[slot].columns = Some(columns);
        }
        let item = &mut self.scope.ctes[slot];
        let (search, cycle) = (item.search.as_ref(), item.cycle.as_ref());
        if search.is_some() || cycle.is_some() {
            let columns = item.columns.as_deref().unwrap_or_default();
            searchcycle::check(columns, item.recursive, item.location, search, cycle, &query)?;
            let rewritten =
                searchcycle::rewrite(&item.name, columns, item.work, search, cycle, &query)?;
            item.written = Some(Box::new(query));
            item.query = Some(Box::new(rewritten));
            return Ok(());
        }
        item.query = Some(Box::new(query));
        Ok(())
    }

    /// `analyzeCTE` for an `INSERT`, an `UPDATE`, a `DELETE` or a `MERGE` of `WITH`. The columns are the columns of `RETURNING`, and a statement with no `RETURNING` has no columns.
    fn analyze_write_cte(&mut self, slot: usize, cte: &CommonTableExpr, node: &Node) -> Result<()> {
        let query = self.sub_level(true, |analyzer| match node {
            Node::InsertStmt(s) => analyzer.insert(s),
            Node::UpdateStmt(s) => analyzer.update(s),
            Node::DeleteStmt(s) => analyzer.delete(s),
            Node::MergeStmt(s) => analyzer.merge(s),
            _ => Err(Error::internal("a query of WITH that is not a statement of a query")),
        })?;
        let returning = query.write.as_ref().and_then(|w| w.returning.as_deref());
        let shown: Vec<&crate::select::Target> =
            returning.map(|r| r.targets.iter().filter(|t| !t.junk).collect()).unwrap_or_default();
        let item = &self.scope.ctes[slot];
        let columns = item
            .columns(shown.iter().map(|t| (t.name.clone(), t.expr.ty, t.expr.typmod)).collect())?;
        if cte.search_clause.is_some() || cte.cycle_clause.is_some() {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "WITH query is not recursive")
                .at_opt(item.location));
        }
        let item = &mut self.scope.ctes[slot];
        item.origins = shown.iter().map(|t| t.origin).collect();
        item.returning = returning.is_some();
        item.columns = Some(columns);
        item.query = Some(Box::new(query));
        item.write = Some(self.writes);
        self.writes += 1;
        Ok(())
    }

    /// True when the query of a set operation is the query of a recursive query of `WITH`, whose slot in the scope just outside the query is `parent`.
    pub(crate) fn recursive_parent(&self, parent: Option<usize>) -> Option<&CteItem> {
        let item = self.outer.last()?.ctes.get(parent?)?;
        item.recursive.then_some(item)
    }

    /// The number of the work table of the recursive query of `parent`, for the top `UNION` of its query.
    pub(crate) fn work_table(&self, parent: Option<usize>) -> Option<usize> {
        self.recursive_parent(parent).and_then(|item| item.work)
    }

    /// `determineRecursiveColTypes`: the columns of a recursive query from its non-recursive term, before the analysis of the recursive term. The names come from the leftmost `SELECT`, and the types from the left side of the `UNION`.
    pub(crate) fn recursive_columns(
        &mut self,
        parent: usize,
        leftmost: usize,
        exprs: &[Expr],
    ) -> Result<()> {
        let Some(query) = self.scope.relations[leftmost].subquery.as_deref() else {
            return Err(Error::internal("a set operation with no leftmost SELECT"));
        };
        let names = query.targets.iter().filter(|t| !t.junk).map(|t| t.name.clone());
        let shown = names.zip(exprs).map(|(n, e)| (n, e.ty, e.typmod)).collect();
        let Some(item) = self.outer.last_mut().and_then(|o| o.ctes.get_mut(parent)) else {
            return Err(Error::internal("a recursive query that is not in the scope"));
        };
        item.columns = Some(item.columns(shown)?);
        Ok(())
    }

    /// `scanNameSpaceForCTE` and `addRangeTableEntryForCTE`: a relation of `FROM` that names a query of `WITH` of the query or of a query outside it, or `None`. The relation has a copy of the query of `WITH` as its subquery, or no subquery and a work table for the reference of a recursive query to itself.
    pub(crate) fn cte_reference(&mut self, rv: &RangeVar) -> Result<Option<Item>> {
        let Some(name) = unqualified(rv) else { return Ok(None) };
        let found = self.levels().find_map(|(level, scope)| {
            scope.ctes.iter().find(|c| c.visible && c.name == name).map(|c| (level, c))
        });
        let Some((level, item)) = found else { return Ok(None) };
        let Some(columns) = item.reference_columns() else {
            return Err(Error::internal("a reference to a query of WITH with no columns"));
        };
        if let Some(write) = item.write {
            if !item.returning {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("WITH query \"{name}\" does not have a RETURNING clause"),
                )
                .at_opt(place(rv.location)));
            }
            let mut relation = Relation::new(0, columns, None, None, None);
            relation.cte = Some(CteRef {
                name: name.to_string(),
                work: None,
                write: Some(write),
                origins: item.origins.clone(),
                transition: false,
            });
            let alias = rv.alias.as_deref();
            let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(name).to_string();
            return self.add_relation(relation, refname, alias).map(Some);
        }
        let available = item.columns.as_ref().map_or(0, Vec::len);
        let extra = columns.len() - available;
        let mut relation = match &item.query {
            Some(query) => {
                let mut query = (**query).clone();
                if level > 0 {
                    raise_outer(&mut query, level);
                }
                Relation::new(0, columns, Some(Box::new(query)), None, None)
            }
            None => Relation::new(0, columns, None, None, None),
        };
        let work = if item.query.is_none() { item.work } else { None };
        relation.cte = Some(CteRef {
            name: name.to_string(),
            work,
            write: None,
            origins: Vec::new(),
            transition: false,
        });
        let alias = rv.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(name).to_string();
        // The alias cannot name the columns of `SEARCH` and `CYCLE`.
        let given = alias.map_or(0, |a| a.colnames.len());
        if extra > 0 && given > available {
            return Err(Error::new(
                SqlState::INVALID_COLUMN_REFERENCE,
                format!(
                    "table \"{refname}\" has {available} columns available but {given} columns specified"
                ),
            ));
        }
        // A query inside the query of `WITH` does not see the columns of `SEARCH` and `CYCLE` in `*`.
        let hidden = if level > 0 { extra } else { 0 };
        let mut item = self.add_relation(relation, refname, alias)?;
        item.hide_last(hidden);
        Ok(Some(item))
    }

    /// `scanNameSpaceForENR` and `addRangeTableEntryForENR`: a reference to a transition table of the trigger that runs, which has a name with no schema. Its columns are the columns of the table of the trigger.
    pub(crate) fn transition_reference(&mut self, rv: &RangeVar) -> Result<Option<Item>> {
        let Some(name) = unqualified(rv) else { return Ok(None) };
        let Some(table) = self.env.transition_table(name) else { return Ok(None) };
        let Some(rel) = self.env.catalog().and_then(|c| c.relation(table)) else {
            return Ok(None);
        };
        let columns = rel
            .columns
            .iter()
            .map(|c| Column {
                name: if c.dropped { String::new() } else { c.name.clone() },
                ty: if c.dropped { 0 } else { c.ty },
                typmod: c.typmod,
                not_null: false,
            })
            .collect();
        let mut relation = Relation::new(0, columns, None, None, None);
        relation.cte = Some(CteRef {
            name: name.to_string(),
            work: None,
            write: None,
            origins: Vec::new(),
            transition: true,
        });
        let alias = rv.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(name).to_string();
        self.add_relation(relation, refname, alias).map(Some)
    }

    /// `isFutureCTE`: true when a query of `WITH` that the query cannot read yet has the name.
    pub(crate) fn future_cte(&self, name: &str) -> bool {
        self.levels().any(|(_, scope)| scope.ctes.iter().any(|c| !c.visible && c.name == name))
    }

    /// `transformWithClause` for an `INSERT`, an `UPDATE`, a `DELETE` or a `MERGE`: the queries of `WITH` that all the parts of the statement can read.
    pub(crate) fn write_with(&mut self, with: Option<&WithClause>) -> Result<()> {
        match with {
            Some(with) => self.transform_with(with),
            None => Ok(()),
        }
    }

    /// The queries of `WITH` of a write, at the end of its analysis. Each reference has a copy of its query, so the list only names them.
    pub(crate) fn end_write_with(
        &mut self,
        query: &mut Query,
        with: Option<&WithClause>,
    ) -> Result<()> {
        query.recursive = with.is_some_and(|w| w.recursive);
        query.ctes = self.take_ctes()?;
        Ok(())
    }

    /// The queries of `WITH` of the query, at the end of its analysis.
    pub(crate) fn take_ctes(&mut self) -> Result<Vec<Cte>> {
        std::mem::take(&mut self.scope.ctes).into_iter().map(CteItem::into_cte).collect()
    }

    /// The check at the end of `parseCheckAggregates`: the recursive term of a recursive query cannot call an aggregate.
    pub(crate) fn check_recursive_aggs<'e>(
        &self,
        exprs: impl Iterator<Item = &'e Expr>,
    ) -> Result<()> {
        let self_reference =
            self.scope.relations.iter().any(|r| r.cte.as_ref().is_some_and(|c| c.work.is_some()));
        if !self.has_aggs || !self_reference {
            return Ok(());
        }
        let at = exprs.into_iter().find_map(Expr::first_agg).and_then(Expr::place);
        Err(Error::new(
            SqlState::INVALID_RECURSION,
            "aggregate functions are not allowed in a recursive query's recursive term",
        )
        .at_opt(at))
    }
}

//! The physical plan, its text form, `EXPLAIN` output (document 15 section 15.9).
//!
//! A [`Plan`] is a tree of [`Node`]s, as the `Plan` nodes of PostgreSQL. `rupg-opt` makes it from a query. The expressions of the nodes are the expressions of the query, so the text of an expression needs the names of the relations of the query and the deparse of `rupg-func`, which the trait [`Texts`] gives. [`explain`] writes the plan in the text, JSON, XML and YAML forms of `explain.c`.

#![forbid(unsafe_code)]

mod explain;
mod format;

use std::borrow::Cow;

use rupg_analyze::{Expr, ExprKind, JoinKind, Query, SubLink};
use rupg_common::Result;

pub use explain::{Format, Options, explain};

/// The relation of a scope that is the target table of an `INSERT`, which is not a relation of the query.
pub const TARGET: usize = usize::MAX;

/// A plan: the tree of nodes, the queries that the expressions of the nodes read, and the names of the subqueries in the expressions.
#[derive(Debug)]
pub struct Plan<'q> {
    pub root: Node<'q>,
    /// The queries of the plan, the query of the statement first. A query comes after the query outside it.
    pub scopes: Vec<Scope<'q>>,
    /// The name of the subplan of each subquery in an expression.
    pub sublinks: Vec<Label<'q>>,
    /// The scopes in the order of the range table of PostgreSQL, which gives each relation a name that is unique in the plan, as `set_rtable_names`: the scopes of the statement, then the scopes of each subplan after the subplans in it. A scope that is not in the list does not take part in the unique names.
    pub order: Vec<usize>,
}

impl Plan<'_> {
    /// The number of entries of the range table of PostgreSQL for the plan, as `list_length(es->rtable)`: the relations, the joins and the merged subqueries of each query, and the target of an `INSERT`. A query with no relation has one entry of the kind `RTE_RESULT`.
    pub fn rtable_size(&self) -> usize {
        self.scopes
            .iter()
            .map(|scope| {
                if scope.leaf.is_some() {
                    return 1;
                }
                let query = scope.query;
                let joins: usize = query.from.iter().map(join_count).sum();
                (query.relations.len() + joins + query.pulled).max(1) + usize::from(scope.target)
            })
            .sum()
    }

    /// The label of a subquery of an expression of the plan.
    pub fn label(&self, link: &SubLink) -> Option<&Label<'_>> {
        self.sublinks.iter().find(|l| std::ptr::eq(l.link, link))
    }
}

/// The number of joins in a part of `FROM`.
fn join_count(item: &rupg_analyze::FromItem) -> usize {
    match item {
        rupg_analyze::FromItem::Relation(_) => 0,
        rupg_analyze::FromItem::Join(join) => 1 + join_count(&join.left) + join_count(&join.right),
    }
}

/// A query of the plan.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'q> {
    pub query: &'q Query,
    /// The scope of the query outside it, where a `Var` with a `levels_up` of 1 finds its relation.
    pub parent: Option<usize>,
    /// True when the query gives the rows of an `INSERT`, whose target table is one more entry of the range table, [`TARGET`].
    pub target: bool,
    /// For the scope of the scan of a member of an inheritance tree, the relation of the query that the member takes the place of. The member is one more entry of the range table, as the child relation of `expand_inherited_rtentry`.
    pub leaf: Option<Leaf>,
}

/// The relation of the scope of a member of an inheritance tree.
#[derive(Clone, Copy, Debug)]
pub struct Leaf {
    /// The scope of the query of the relation, whose names the scope has.
    pub of: usize,
    /// The relation of the query.
    pub relation: usize,
    /// True when the member has a name of its own, as `r_1`. A member that is the only scan of the relation of a `SELECT` has the name of the relation, because no node of the plan names the relation itself.
    pub renamed: bool,
}

/// The name of the subplan of a subquery in an expression, as `SubPlan.plan_name`, such as `InitPlan expr_1` or `SubPlan any_1`.
#[derive(Debug)]
pub struct Label<'q> {
    pub link: &'q SubLink,
    pub name: String,
    /// True when the subplan is an `InitPlan`, which runs once and gives its value as a parameter.
    pub init: bool,
    /// `useHashTable`: the subplan keeps its rows in a hash table, which the test of `ANY` reads.
    pub hashed: bool,
    /// True when the subplan is the `ANY` of [`SubLink::alternative`], which runs in the place of `EXISTS`, as `fix_alternative_subplan` chooses it.
    pub alternative: bool,
}

/// A node of the plan.
#[derive(Clone, Debug)]
pub struct Node<'q> {
    pub kind: Kind<'q>,
    /// The index of the query of the expressions of the node in [`Plan::scopes`].
    pub scope: usize,
    pub cost: Cost,
    /// `targetlist`: the expressions of the rows that the node gives.
    pub output: Vec<Item<'q>>,
    /// `qual`: the conditions that each row of the node must pass.
    pub filter: Vec<Item<'q>>,
    /// The nodes that give the rows of the node: the outer node first, then the inner node, or the members of an `Append`.
    pub children: Vec<Node<'q>>,
    /// `initPlan`: the subplans that run once before the node.
    pub init_plans: Vec<SubPlan<'q>>,
    /// `subPlan`: the subplans that the expressions of the node run for each row.
    pub sub_plans: Vec<SubPlan<'q>>,
    /// The OID of the table of a scan of a member of an inheritance tree, which the scan reads in place of the table of its relation.
    pub member: Option<u32>,
    /// `inner_unique`: for a join, true when each row of the outer side matches one row of the inner side at most.
    pub inner_unique: bool,
}

impl<'q> Node<'q> {
    /// A node with no expressions and no subplans.
    pub fn new(kind: Kind<'q>, scope: usize, children: Vec<Node<'q>>) -> Node<'q> {
        Node {
            kind,
            scope,
            cost: Cost::default(),
            output: Vec::new(),
            filter: Vec::new(),
            children,
            init_plans: Vec::new(),
            sub_plans: Vec::new(),
            member: None,
            inner_unique: false,
        }
    }
}

/// A subplan with its name, such as `InitPlan expr_1`, `SubPlan any_1` or `CTE w`.
#[derive(Clone, Debug)]
pub struct SubPlan<'q> {
    pub name: String,
    pub plan: Node<'q>,
}

/// The estimates of the planner for a node.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// `startup_cost`: the cost before the first row.
    pub startup: f64,
    /// `total_cost`: the cost of all the rows.
    pub total: f64,
    /// `plan_rows`.
    pub rows: f64,
    /// `plan_width`: the average width of a row in bytes.
    pub width: i32,
}

/// An expression of a node.
#[derive(Clone, Debug)]
pub struct Item<'q> {
    pub expr: Cow<'q, Expr>,
    /// True when a node below computes the expression and the node reads it as a column of its input, as `OUTER_VAR`. The text of such an expression that is not a column is in parentheses.
    pub below: bool,
}

impl<'q> Item<'q> {
    /// An expression of the query that the node computes.
    pub fn of(expr: &'q Expr) -> Item<'q> {
        Item { expr: Cow::Borrowed(expr), below: false }
    }

    /// An expression that the node reads from the node below.
    pub fn below(expr: &'q Expr) -> Item<'q> {
        Item { expr: Cow::Borrowed(expr), below: true }
    }

    /// An expression that the planner makes.
    pub fn owned(expr: Expr) -> Item<'q> {
        Item { expr: Cow::Owned(expr), below: false }
    }

    /// The same expression as the input of the node above.
    pub fn passed(&self) -> Item<'q> {
        Item { expr: self.expr.clone(), below: !matches!(self.expr.kind, ExprKind::Var(_)) }
    }
}

/// A key of a sort.
#[derive(Clone, Debug)]
pub struct SortKey<'q> {
    pub item: Item<'q>,
    /// True when the operator is the `>` of the type, as `DESC`.
    pub desc: bool,
    /// The name of an operator of `USING` that is not the `<` or the `>` of the type.
    pub using: Option<String>,
    pub nulls_first: bool,
}

/// The kinds of node, with the parts that each kind shows.
#[derive(Clone, Debug)]
pub enum Kind<'q> {
    /// `Result`: a row with no relation, or the rows of the node below when a condition with no column, `resconstantqual`, is true.
    Result { replaces: Option<Replaced>, one_time: Vec<Item<'q>> },
    /// `SeqScan` of a relation of the scope.
    SeqScan(usize),
    /// `IndexScan`, or `IndexOnlyScan` when `only`: a relation of the scope by its index `index`, in the order of the index or, when `backward`, in the other order, with the conditions of the index, `indexqualorig`. `columns` are the columns of the relation that the index has, which an index only scan gives.
    IndexScan {
        relation: usize,
        index: u32,
        only: bool,
        backward: bool,
        cond: Vec<Item<'q>>,
        columns: Vec<i16>,
    },
    /// `BitmapHeapScan`: the rows of a relation of the scope at the places that the bitmap of the child gives, with the conditions of the indexes of the bitmap, `bitmapqualorig`, which the node tests again.
    BitmapHeapScan { relation: usize, recheck: Vec<Item<'q>> },
    /// `BitmapIndexScan`: the bitmap of the places of the rows of a relation of the scope that its index `index` finds with the conditions of the index.
    BitmapIndexScan { relation: usize, index: u32, cond: Vec<Item<'q>> },
    /// `BitmapAnd`: the places that are in each bitmap of the children.
    BitmapAnd,
    /// `BitmapOr`: the places that are in a bitmap of a child.
    BitmapOr,
    /// `SampleScan`: a table with `TABLESAMPLE`.
    SampleScan(usize),
    /// `FunctionScan`: a function in `FROM`.
    FunctionScan(usize),
    /// `TableFuncScan`: `JSON_TABLE` in `FROM`.
    TableFunctionScan(usize),
    /// `ValuesScan`: a `VALUES` list.
    ValuesScan(usize),
    /// `SubqueryScan`: a subquery in `FROM`, whose plan is the child.
    SubqueryScan(usize),
    /// `CteScan`: a query of `WITH` that the plan runs once.
    CteScan(usize),
    /// `WorkTableScan`: the reference of the recursive term of a query of `WITH` to itself.
    WorkTableScan(usize),
    /// `NestLoop`, with the condition of the join, `joinqual`.
    NestedLoop { join: JoinKind, join_filter: Vec<Item<'q>> },
    /// `MergeJoin`, with the `=` conditions of the merge of the two sorted inputs, `mergeclauses`, and the other conditions of the join.
    MergeJoin { join: JoinKind, cond: Vec<Item<'q>>, join_filter: Vec<Item<'q>> },
    /// `HashJoin`, with the `=` conditions that the hash table tests, `hashclauses`, and the other conditions of the join.
    HashJoin { join: JoinKind, cond: Vec<Item<'q>>, join_filter: Vec<Item<'q>> },
    /// `Hash`: the hash table of the inner side of a hash join.
    Hash,
    /// `Material`: the rows of the node below, which the node keeps for the next scans.
    Materialize,
    /// `Agg`, with the keys of the groups. Each grouping set has its keys, and a set with no keys makes one group.
    Aggregate { strategy: Strategy, keys: Vec<Item<'q>>, sets: Vec<(bool, Vec<Item<'q>>)> },
    /// `Sort`.
    Sort(Vec<SortKey<'q>>),
    /// `IncrementalSort`: a sort of rows whose first `presorted` keys are in order already.
    IncrementalSort { keys: Vec<SortKey<'q>>, presorted: usize },
    /// `Limit`: `LIMIT` and `OFFSET`.
    Limit,
    /// `Unique`: the first row of each run of equal rows of a sorted input.
    Unique,
    /// `Append`: the rows of each member.
    Append,
    /// `MergeAppend`: the rows of the members, which are each in the order of the keys, merged in that order.
    MergeAppend(Vec<SortKey<'q>>),
    /// `SetOp`: `INTERSECT` or `EXCEPT`, with a hash table or, with `sorted`, with the rows of both sides in the order of their columns.
    SetOp { except: bool, all: bool, sorted: bool },
    /// `RecursiveUnion`.
    RecursiveUnion,
    /// `WindowAgg`, with the name of the window, its index in the windows of the query, and the places of the items of its `PARTITION BY` that are not redundant, as `make_pathkeys_for_window` keeps them.
    WindowAgg { name: String, window: usize, rows_frame: bool, partition: Vec<usize> },
    /// `ProjectSet`: a target list with functions that give sets.
    ProjectSet,
    /// `LockRows`: `FOR UPDATE` and the other locks of rows.
    LockRows,
    /// `ModifyTable`, with the OID of the table and the relation of the scope that the table is. The scopes of `targets` are the members of an inheritance tree that the node changes, as `resultRelations`.
    Modify {
        operation: Operation,
        table: u32,
        relation: usize,
        conflict: Option<Conflict<'q>>,
        targets: Vec<usize>,
    },
}

/// The relation or the node that a `Result` takes the place of, as `result_type`.
#[derive(Clone, Debug)]
pub enum Replaced {
    /// A scan of a relation of the scope with a condition that is never true, and true when the `Result` keeps the width of the columns of the relation. A relation that `set_rel_size` finds dummy has a width of 0, and an inner side that a join finds dummy keeps its width.
    Scan(usize, bool),
    /// A join of the relations of the scope with a condition that is never true.
    Join(Vec<usize>),
    /// `RESULT_TYPE_MINMAX`: the `Agg` node of a query whose aggregate calls are each `min` or `max`, which reads their values from its `InitPlan` subplans.
    MinMax,
}

/// The strategy of an `Agg`, as `AggStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// `AGG_PLAIN`: one group of all the rows.
    Plain,
    /// `AGG_SORTED`: groups of a sorted input.
    Sorted,
    /// `AGG_HASHED`: groups in a hash table.
    Hashed,
    /// `AGG_MIXED`: grouping sets, some in hash tables and some of a sorted input.
    Mixed,
}

/// The command of a `ModifyTable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Insert,
    Update,
    Delete,
    Merge,
}

impl Operation {
    /// The name of the command, as `EXPLAIN` shows it.
    pub fn name(self) -> &'static str {
        match self {
            Operation::Insert => "Insert",
            Operation::Update => "Update",
            Operation::Delete => "Delete",
            Operation::Merge => "Merge",
        }
    }
}

/// The `ON CONFLICT` clause of an `INSERT`.
#[derive(Clone, Debug)]
pub struct Conflict<'q> {
    /// True for `DO UPDATE`, false for `DO NOTHING`.
    pub update: bool,
    /// The OIDs of the arbiter indexes.
    pub arbiters: Vec<u32>,
    /// The condition of `WHERE` of `DO UPDATE`, in the scope of the query of `DO UPDATE`.
    pub filter: Vec<Item<'q>>,
    /// The scope of the query of `DO UPDATE`.
    pub scope: Option<usize>,
}

/// The texts that `EXPLAIN` shows for the parts of a plan: the deparse of the expressions and the names of the relations. `rupg-func` gives them.
pub trait Texts {
    /// The text of an expression of the query of a scope, with the names of the relations before the columns when `prefix` is true. A column of a query outside the scope always shows the name of its relation.
    ///
    /// # Errors
    ///
    /// The errors of the deparse.
    fn expr(&mut self, scope: usize, expr: &Expr, prefix: bool) -> Result<String>;

    /// [`Self::expr`] for an expression that a node reads from the node below, which shows the casts that the analyzer added at its top, as `get_special_variable` writes it.
    ///
    /// # Errors
    ///
    /// The errors of the deparse.
    fn below_expr(&mut self, scope: usize, expr: &Expr, prefix: bool) -> Result<String> {
        self.expr(scope, expr, prefix)
    }

    /// [`Self::expr`] for a condition of a scan of the relation `own`, in which a column of another relation of the scope shows the name of its relation.
    ///
    /// # Errors
    ///
    /// The errors of the deparse.
    fn scan_expr(&mut self, scope: usize, expr: &Expr, prefix: bool, own: usize) -> Result<String> {
        let _ = own;
        self.expr(scope, expr, prefix)
    }

    /// The name of a relation of a scope in the plan, `refname`, which is unique in the plan. [`TARGET`] is the target table of an `INSERT`.
    fn refname(&mut self, scope: usize, relation: usize) -> String;

    /// The identifier in double quotes when it needs them, as `quote_identifier`.
    fn quote(&self, name: &str) -> String;

    /// The name and the schema of a relation or of a function.
    fn object(&mut self, oid: u32, function: bool) -> (String, String);

    /// The text of the definition of a window of the query of a scope, as `(PARTITION BY ...)`. With `rows_frame` the frame is `ROWS UNBOUNDED PRECEDING`, which the planner gives to the windows of `row_number` and the other functions of ranks. `PARTITION BY` shows the items at the places `partition`.
    ///
    /// # Errors
    ///
    /// The errors of the deparse.
    fn window(
        &mut self,
        scope: usize,
        window: usize,
        rows_frame: bool,
        partition: &[usize],
        prefix: bool,
    ) -> Result<String>;
}

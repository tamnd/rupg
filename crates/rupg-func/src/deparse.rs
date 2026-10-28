//! The deparse functions `pg_get_expr`, `pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef`, `pg_get_ruledef`, `pg_get_triggerdef`, the `pg_get_function_*` functions, `pg_get_partkeydef` and `pg_get_partition_constraintdef`: a port of the parts of `ruleutils.c` that show the expressions, the queries and the definitions of the catalog.
//!
//! The functions read the stored form that `rupg_analyze::node` writes, or the query that the analyzer makes from the text of a view, and show it as PostgreSQL shows its node trees. The rules for parentheses, casts and constants follow `get_rule_expr`. The functions show the objects of the user and the system views. An index or a constraint of a system catalog gives an error that says that the engine cannot show it yet.

mod explain;
mod function;
mod partqual;
mod query;

pub use explain::explain;
pub use query::view_statement;

use std::ptr;
use std::rc::Rc;

use rupg_analyze::node::{StoredBound, StoredDatum};
use rupg_analyze::{
    Aggref, BoolOp, BoolTest, Case, CastForm, Expr, ExprKind, Func, FuncForm, JsonBehavior,
    JsonBehaviorKind, JsonCtor, JsonCtorKind, JsonEncoding, JsonExpr, JsonExprOp, JsonFormat,
    JsonFormatKind, JsonItemType, JsonTable, JsonTablePlan, JsonTableScan, JsonWrapper, RowCompare,
    SortGroup, SqlValue, SubLink, SubLinkKind, Subscript, Var, WindowClause, WindowFunc, XmlExpr,
    XmlOp,
};
use rupg_analyze::{
    MERGE_ACTION, RETURNING_NEW, RETURNING_OLD, default_opclass, node, sort_operators,
};
use rupg_catalog::{ConKind, Constraint, IndexInfo, PartStrategy, PartitionKey, RelKind, Relation};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_plan::TARGET;
use rupg_types::{Value, oid, sql_name};

use crate::io::to_text;
use crate::reg;
use crate::text::quote_identifier;
use crate::{Call, Kernel, Session, bad_value, not_yet};

/// `PRETTYINDENT_VAR`: the indent of the lines of a `CASE`.
const INDENT_VAR: i32 = 4;
/// `PRETTYINDENT_STD`.
const INDENT_STD: i32 = 8;
/// `PRETTYINDENT_LIMIT`: the indent after which each level adds less.
const INDENT_LIMIT: i32 = 40;
/// The access method `btree`, the one access method that has an order.
const BTREE: u32 = 403;
/// The access method `hash`.
const HASH: u32 = 405;
/// The access method `gist`.
const GIST: u32 = 783;
/// The access method `gin`.
const GIN: u32 = 2742;
/// The access method `spgist`.
const SPGIST: u32 = 4000;
/// The access method `brin`.
const BRIN: u32 = 3580;
/// `INDOPTION_DESC`.
const DESC: i16 = 1;
/// `INDOPTION_NULLS_FIRST`.
const NULLS_FIRST: i16 = 2;
/// The first OID that a user object can have.
const FIRST_USER_OID: u32 = 16384;

/// The names of the relations of one query and of their columns, as `deparse_namespace` with its `deparse_columns`.
#[derive(Clone, Debug, Default)]
struct Namespace {
    /// The name of each relation of the query, by its index, as `rtable_names`. The name is unique in the query and in the queries outside it.
    names: Vec<String>,
    /// The names of the columns of each relation, by the column number from 1.
    columns: Vec<Vec<String>>,
    /// For each relation, true when `FROM` shows the names of the columns after the name of the relation, as `printaliases`.
    print_aliases: Vec<bool>,
    /// The value and the name of each column of `USING` of a join with no alias that is not a column of one side. PostgreSQL shows such a column by its name, as a column of the join.
    merged: Vec<(Expr, String)>,
    /// The relations of the query, where a field of a value of type `record` finds its name.
    relations: Vec<rupg_analyze::Relation>,
    /// The joins of `FROM`, an outer join before the joins in it, as the `deparse_columns` of the entries of the joins.
    joins: Vec<JoinNames>,
    /// The joins, by their index in `joins`, that the deparse shows now. A column of a relation of such a join shows the name of its relation, not the name of the join.
    inside: Vec<usize>,
    /// The subqueries of `FROM`, by the index of the relation, whose `Subquery Scan` the plan does not keep. A node above reads the target list of the plan of the subquery.
    removed: Vec<(usize, Rc<Removed>)>,
    /// The name of the target table of an `INSERT` and the names of its columns, which a `Var` of [`TARGET`] reads, as the entry of the target table in the range table.
    target: Option<(String, Vec<String>)>,
    /// The names of `old` and `new` of `RETURNING` and the names of the columns of the table, which a `Var` of [`RETURNING_OLD`] or [`RETURNING_NEW`] reads.
    returning: Option<(String, String, Vec<String>)>,
}

/// The target list of the plan of a subquery whose `Subquery Scan` the plan does not keep, as `trivial_subqueryscan` takes the scan out of the plan.
#[derive(Debug)]
struct Removed {
    /// The names of the subquery and of the queries outside it, the query outside first.
    namespaces: Vec<Namespace>,
    /// The targets of the subquery.
    targets: Vec<Expr>,
}

/// The names of a join and of its columns, as `deparse_columns` for an entry of a join.
#[derive(Clone, Debug, Default)]
struct JoinNames {
    /// The name of the join with an alias, which no other relation of the query has, or `None` for a join with no alias.
    name: Option<String>,
    /// The indexes of the relations in the join.
    relations: Vec<usize>,
    /// `colnames`: the name of each column of the join.
    columns: Vec<String>,
    /// `usingNames`: the names of the columns of `USING`.
    using: Vec<String>,
    /// The value of each column of the join.
    values: Vec<Expr>,
    /// `printaliases`: `FROM` shows the names of the columns after the alias.
    print_aliases: bool,
}

/// A deparse of an expression or of a query, as `deparse_context`. Each form of the functions sets `PRETTYFLAG_INDENT`, so a `CASE` always takes more than one line. `EXPLAIN` does not set it.
struct Deparser<'a> {
    session: &'a dyn Session,
    /// The names of the queries that have the expression, the outermost query first. A `Var` finds its relation in the namespace of its level.
    namespaces: Vec<Namespace>,
    /// `PRETTYFLAG_PAREN`: add parentheses only where the syntax needs them.
    paren: bool,
    /// `indentLevel`.
    indent: i32,
    /// For `EXPLAIN`, `PRETTYFLAG_INDENT` is not set, as `deparse_expression` makes the context: a keyword does not start a new line.
    pub(crate) flat: bool,
    /// `wrapColumn`: the length of a line after which the target list and `FROM` start a new line for the next item, or -1 to keep the items on one line.
    wrap: i32,
    /// `varprefix`: show the name of the relation before the name of each column.
    prefix: bool,
    /// `colNamesVisible`: the names of the columns of the query show in its result.
    names_visible: bool,
    /// `inGroupBy`.
    in_group_by: bool,
    /// The name of each column of the result of the query, with the column of `FROM` that the column shows. A column in `ORDER BY` with the name of another column of the result shows the name of its relation, as `get_variable` does.
    outputs: Vec<(String, Option<Var>)>,
    /// `windowClause`: the windows of the query, which a window function call names by number.
    windows: Vec<WindowClause>,
    /// The expressions of the targets of the query, which the items of the windows name.
    window_targets: Vec<Expr>,
    /// For `EXPLAIN`, the subplans of the subqueries in the expressions. A subquery shows the name of its subplan, as `get_rule_expr` shows a `SubPlan`.
    subplans: Vec<SubName<'a>>,
    /// For `EXPLAIN`, the subplan whose columns the test of `ANY` or `ALL` reads now, as `(SubPlan any_1)`.
    subplan: Option<String>,
    /// For `EXPLAIN`, the name of each window of the query by its number, which a call of a window function shows after `OVER`.
    window_names: Vec<Option<String>>,
    /// For `EXPLAIN`, a column of a query outside the query always shows the name of its relation, as `get_parameter` shows the value of a parameter.
    outer_prefix: bool,
    /// For `EXPLAIN` of a condition of a scan, the relation of the scan. A column of another relation is the value of a parameter of a nested loop, which shows the name of its relation, as `get_parameter`.
    pub(crate) own: Option<usize>,
    /// For `EXPLAIN` of a condition of a join, the expressions of the targets of the nodes below it. Such an expression that is not a column is the value that the node below gives, in parentheses, as `get_special_variable` shows an `OUTER_VAR` or an `INNER_VAR`.
    computed: Vec<Expr>,
    /// For the body of a function in SQL, the name of the function and the name of each argument, as the `funcname` and `argnames` of the outermost `deparse_namespace`. A parameter shows the name of its argument.
    function: Option<(String, Vec<String>)>,
    buf: String,
}

/// The name of the subplan of a subquery in an expression, for `EXPLAIN`.
#[derive(Clone, Debug)]
struct SubName<'a> {
    /// The subquery.
    link: &'a SubLink,
    name: String,
    /// The subplan is an `InitPlan`.
    init: bool,
    /// The subplan keeps its rows in a hash table.
    hashed: bool,
    /// The subplan is the `ANY` that runs in the place of `EXISTS`, which shows in its place.
    alternative: bool,
}

impl<'a> Deparser<'a> {
    /// A deparse of an expression of one relation, as `deparse_context_for`: a `Var` shows the name of its column with no name of the relation.
    fn for_columns(
        session: &'a dyn Session,
        name: &str,
        columns: Vec<String>,
        paren: bool,
    ) -> Deparser<'a> {
        let mut d = Deparser::new(session, paren, 0);
        d.namespaces.push(Namespace {
            names: vec![name.to_string()],
            columns: vec![columns],
            print_aliases: vec![false],
            merged: Vec::new(),
            relations: Vec::new(),
            joins: Vec::new(),
            inside: Vec::new(),
            removed: Vec::new(),
            target: None,
            returning: None,
        });
        d
    }

    /// A deparse with no query.
    fn new(session: &'a dyn Session, paren: bool, wrap: i32) -> Deparser<'a> {
        Deparser {
            session,
            namespaces: Vec::new(),
            paren,
            indent: 0,
            flat: false,
            wrap,
            prefix: false,
            names_visible: true,
            in_group_by: false,
            outputs: Vec::new(),
            windows: Vec::new(),
            window_targets: Vec::new(),
            subplans: Vec::new(),
            subplan: None,
            window_names: Vec::new(),
            outer_prefix: false,
            own: None,
            computed: Vec::new(),
            function: None,
            buf: String::new(),
        }
    }

    /// `get_parameter` for a parameter `$n` of the extended protocol or of a function. In the body of a function in SQL, an argument with a name shows the name, after the name of the function when a query has relations. Else it shows `$n`.
    fn parameter(&mut self, n: usize) {
        let named = self.function.as_ref().and_then(|(function, names)| {
            let name = names.get(n.checked_sub(1)?).filter(|name| !name.is_empty())?;
            Some((quote_identifier(function), quote_identifier(name)))
        });
        let Some((function, name)) = named else {
            self.buf.push_str(&format!("${n}"));
            return;
        };
        if self.namespaces.iter().any(|space| !space.names.is_empty()) {
            self.buf.push_str(&function);
            self.buf.push('.');
        }
        self.buf.push_str(&name);
    }

    /// `appendContextKeyword`: a new line with the indent and `plus` more spaces, then the keyword.
    fn keyword(&mut self, word: &str, before: i32, after: i32, plus: i32) {
        if self.flat {
            self.buf.push_str(word);
            return;
        }
        self.indent += before;
        let kept = self.buf.trim_end_matches(' ').len();
        self.buf.truncate(kept);
        self.buf.push('\n');
        let amount = if self.indent < INDENT_LIMIT {
            self.indent.max(0)
        } else {
            (INDENT_LIMIT + (self.indent - INDENT_LIMIT) / (INDENT_STD / 2)) % INDENT_LIMIT
        } + plus;
        for _ in 0..amount {
            self.buf.push(' ');
        }
        self.buf.push_str(word);
        self.indent = (self.indent + after).max(0);
    }

    /// The space that `get_rule_expr` adds before a keyword when the keyword does not start a new line.
    fn space(&mut self) {
        if self.flat {
            self.buf.push(' ');
        }
    }

    /// `get_variable`: the column with the name of its relation when the query needs it, and the name of the column. A column of `ORDER BY` also shows the name of its relation when a column of the result has the same name and shows another value.
    fn variable(&mut self, var: &Var, in_order_by: bool) -> Result<String> {
        let bogus = || Error::internal(format!("bogus varlevelsup: {}", var.levels_up));
        let depth = self.namespaces.len().checked_sub(var.levels_up + 1).ok_or_else(bogus)?;
        // `MergeSupportFunc` is not a `Var`, so that its column always shows its name.
        if var.relation == MERGE_ACTION {
            self.buf.push_str("MERGE_ACTION()");
            return Ok(String::new());
        }
        let space = &self.namespaces[depth];
        let target = match (&space.target, &space.returning, var.relation) {
            (Some((name, columns)), _, TARGET) => Some((name, columns)),
            (_, Some((old, _, columns)), RETURNING_OLD) => Some((old, columns)),
            (_, Some((_, new, columns)), RETURNING_NEW) => Some((new, columns)),
            _ => None,
        };
        let refname = match target {
            Some((name, _)) => name.clone(),
            None => space.names.get(var.relation).cloned().unwrap_or_default(),
        };
        let columns =
            target.map(|(_, columns)| columns).or_else(|| space.columns.get(var.relation));
        let attname = match usize::try_from(var.attnum) {
            Ok(n) if n > 0 => columns.and_then(|c| c.get(n - 1)).cloned(),
            _ => system_column(var.attnum).map(str::to_string),
        };
        let attname =
            attname.ok_or_else(|| Error::internal(format!("invalid attnum {}", var.attnum)))?;
        // A column of a `Subquery Scan` that the plan does not keep is the target of the plan of the subquery, as `get_variable` finds it for `OUTER_VAR`. A target that is not a column of the subquery gets parentheses, also a column of the query above, which is a `Param` of the plan.
        let removed = space.removed.iter().find(|(r, _)| *r == var.relation);
        if let Some(removed) = removed.map(|(_, removed)| Rc::clone(removed))
            && let Ok(at @ 1..) = usize::try_from(var.attnum)
            && let Some(target) = removed.targets.get(at - 1)
        {
            let outer = std::mem::replace(&mut self.namespaces, removed.namespaces.clone());
            let parens = !matches!(target.kind, ExprKind::Var(v) if v.levels_up == 0);
            if parens {
                self.buf.push('(');
            }
            let done = self.expr(target, true);
            if parens {
                self.buf.push(')');
            }
            self.namespaces = outer;
            done?;
            return Ok(attname);
        }
        let mut prefix = self.prefix
            || (self.outer_prefix && var.levels_up > 0)
            || self.own.is_some_and(|o| var.levels_up == 0 && var.relation != o)
            || [RETURNING_OLD, RETURNING_NEW].contains(&var.relation);
        if in_order_by && !self.in_group_by && !prefix {
            prefix = self.outputs.iter().any(|(name, v)| *name == attname && *v != Some(*var));
        }
        if prefix && !refname.is_empty() {
            self.buf.push_str(&quote_identifier(&refname));
            self.buf.push('.');
        }
        self.buf.push_str(&quote_identifier(&attname));
        Ok(attname)
    }

    /// `get_variable` for a whole-row `Var`: the name of the relation and `*`. At the top level of a target list the text has a cast to the row type.
    fn whole_row(&mut self, var: &Var, e: &Expr, top: bool) -> Result<()> {
        let bogus = || Error::internal(format!("bogus varlevelsup: {}", var.levels_up));
        let depth = self.namespaces.len().checked_sub(var.levels_up + 1).ok_or_else(bogus)?;
        let space = &self.namespaces[depth];
        let refname = match (&space.returning, var.relation) {
            (Some((old, ..)), RETURNING_OLD) => old.clone(),
            (Some((_, new, _)), RETURNING_NEW) => new.clone(),
            _ => space.names.get(var.relation).cloned().unwrap_or_default(),
        };
        if !refname.is_empty() {
            self.buf.push_str(&quote_identifier(&refname));
            self.buf.push('.');
        }
        self.buf.push('*');
        if top {
            self.buf.push_str("::");
            self.buf.push_str(&reg::type_text(e.ty, Some(e.typmod), self.session)?);
        }
        Ok(())
    }

    /// The `(` that a node writes around itself when the flags do not have `PRETTYFLAG_PAREN`.
    fn open(&mut self) {
        if !self.paren {
            self.buf.push('(');
        }
    }

    /// The `)` that goes with [`Deparser::open`].
    fn close(&mut self) {
        if !self.paren {
            self.buf.push(')');
        }
    }

    /// The `(` of `get_rule_expr_paren` for a node that is not simple, when the flags have `PRETTYFLAG_PAREN`.
    fn open_pretty(&mut self) {
        if self.paren {
            self.buf.push('(');
        }
    }

    /// The `)` that goes with [`Deparser::open_pretty`].
    fn close_pretty(&mut self) {
        if self.paren {
            self.buf.push(')');
        }
    }

    /// `get_rule_expr_paren`: the expression, in parentheses when `PRETTYFLAG_PAREN` is set and the expression is not simple in its parent.
    fn paren(&mut self, e: &Expr, implicit: bool, parent: &Expr) -> Result<()> {
        let need = self.paren && !is_simple(e, parent, self.paren);
        if need {
            self.buf.push('(');
        }
        self.expr(e, implicit)?;
        if need {
            self.buf.push(')');
        }
        Ok(())
    }

    /// The expressions of a list, with `, ` between them.
    fn list(&mut self, items: &[Expr], implicit: bool) -> Result<()> {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.expr(item, implicit)?;
        }
        Ok(())
    }

    /// `get_rule_expr_toplevel`: a whole-row `Var` gets the cast to its row type, so that the parser does not expand `*` in `ROW()` or `VALUES`.
    pub(crate) fn toplevel(&mut self, e: &Expr, implicit: bool) -> Result<()> {
        match &e.kind {
            ExprKind::Var(var)
                if var.attnum == 0
                    && self.join_column(e).is_none()
                    && self.merged_name(e).is_none() =>
            {
                self.whole_row(var, e, true)
            }
            _ => self.expr(e, implicit),
        }
    }

    /// `get_rule_list_toplevel`: [`Deparser::toplevel`] for each expression of a list, with `, ` between them.
    fn toplevel_list(&mut self, items: &[Expr], implicit: bool) -> Result<()> {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.toplevel(item, implicit)?;
        }
        Ok(())
    }

    /// `get_rule_expr`. `implicit` is `showimplicit`: true to show the casts that the analyzer added.
    fn expr(&mut self, e: &Expr, implicit: bool) -> Result<()> {
        if !self.computed.is_empty()
            && !matches!(e.kind, ExprKind::Var(_) | ExprKind::Const(_) | ExprKind::PlaceHolder(_))
            && self.computed.iter().any(|c| c.same(e))
        {
            let saved = std::mem::take(&mut self.computed);
            self.buf.push('(');
            let done = self.expr(e, true);
            self.buf.push(')');
            self.computed = saved;
            return done;
        }
        if let Some((join, column)) = self.join_column(e) {
            if self.prefix {
                self.buf.push_str(&quote_identifier(&join));
                self.buf.push('.');
            }
            self.buf.push_str(&quote_identifier(&column));
            return Ok(());
        }
        if let Some(name) = self.merged_name(e) {
            self.buf.push_str(&quote_identifier(&name));
            return Ok(());
        }
        match &e.kind {
            ExprKind::Const(value) => self.constant(e, value, 0)?,
            ExprKind::Param(n) => self.parameter(*n),
            ExprKind::Var(var) if var.attnum == 0 => self.whole_row(var, e, false)?,
            ExprKind::Var(var) => {
                self.variable(var, false)?;
            }
            ExprKind::FieldSelect(arg, field) => {
                // The argument needs parentheses unless it is a subscript or another field, also when it is a column.
                let parens =
                    !matches!(arg.kind, ExprKind::Subscript(_) | ExprKind::FieldSelect(..));
                if parens {
                    self.buf.push('(');
                }
                self.expr(arg, true)?;
                if parens {
                    self.buf.push(')');
                }
                let name = self.field_name(arg, *field)?;
                self.buf.push('.');
                self.buf.push_str(&quote_identifier(&name));
            }
            ExprKind::Func(f) => self.func(e, f, implicit)?,
            ExprKind::NamedArg(na) => {
                self.buf.push_str(&quote_identifier(&na.name));
                self.buf.push_str(" => ");
                self.expr(&na.arg, implicit)?;
            }
            ExprKind::Relabel(arg, form) => self.cast_node(e, arg, *form, e.typmod, implicit)?,
            ExprKind::Collate(arg, collation) => {
                self.open();
                self.paren(arg, implicit, e)?;
                self.buf.push_str(" COLLATE ");
                self.buf.push_str(&collation_name(*collation, self.session)?);
                self.close();
            }
            ExprKind::CoerceViaIo(arg, form) | ExprKind::ConvertRow { arg, form, .. } => {
                self.cast_node(e, arg, *form, -1, implicit)?;
            }
            ExprKind::ArrayCoerce { arg, form, .. } => {
                self.cast_node(e, arg, *form, e.typmod, implicit)?;
            }
            // An implicit cast to a domain shows its argument with no parentheses.
            ExprKind::CoerceToDomain(arg, CastForm::Implicit) if !implicit => {
                self.expr(arg, false)?;
            }
            ExprKind::CoerceToDomain(arg, _) => self.coercion(arg, e.ty, e.typmod, e)?,
            ExprKind::DomainValue => self.buf.push_str("VALUE"),
            // `processIndirection` shows an assignment to a field as the target of `SET`, so a `FieldStore` does not come here.
            ExprKind::FieldStore(_) => return Err(Error::internal("unexpected FieldStore")),
            ExprKind::Subscript(sub) => {
                // The array needs parentheses unless it is a column or a field, also when it is another subscript.
                let parens =
                    !matches!(sub.container.kind, ExprKind::Var(_) | ExprKind::FieldSelect(..));
                if parens {
                    self.buf.push('(');
                }
                self.expr(&sub.container, implicit)?;
                if parens {
                    self.buf.push(')');
                }
                self.subscripts(sub)?;
                // An assignment shows its new value after the subscripts.
                if let Some(assign) = &sub.assign {
                    self.buf.push_str(" := ");
                    self.expr(assign, false)?;
                }
            }
            ExprKind::Bool(op, args) => {
                self.open();
                if *op == BoolOp::Not {
                    self.buf.push_str("NOT ");
                }
                let word = if *op == BoolOp::And { " AND " } else { " OR " };
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(word);
                    }
                    self.paren(arg, false, e)?;
                }
                self.close();
            }
            ExprKind::NullTest(arg, is_null) => {
                self.open();
                self.paren(arg, true, e)?;
                self.buf.push_str(if *is_null { " IS NULL" } else { " IS NOT NULL" });
                self.close();
            }
            ExprKind::BooleanTest(arg, test) => {
                self.open();
                self.paren(arg, false, e)?;
                self.buf.push_str(match test {
                    BoolTest::IsTrue => " IS TRUE",
                    BoolTest::IsNotTrue => " IS NOT TRUE",
                    BoolTest::IsFalse => " IS FALSE",
                    BoolTest::IsNotFalse => " IS NOT FALSE",
                    BoolTest::IsUnknown => " IS UNKNOWN",
                    BoolTest::IsNotUnknown => " IS NOT UNKNOWN",
                });
                self.close();
            }
            ExprKind::Case(case) => self.case(case)?,
            ExprKind::CaseTest => self.buf.push_str("CASE_TEST_EXPR"),
            ExprKind::Coalesce(args) => {
                self.buf.push_str("COALESCE(");
                self.list(args, true)?;
                self.buf.push(')');
            }
            ExprKind::Row(args, form) => {
                self.buf.push_str("ROW(");
                self.toplevel_list(args, true)?;
                self.buf.push(')');
                if *form == CastForm::Explicit {
                    let ty = reg::type_text(e.ty, Some(-1), self.session)?;
                    self.buf.push_str(&format!("::{ty}"));
                }
            }
            ExprKind::JoinRow(row) => match &row.alias {
                Some(_) => {
                    let name = self.join_row_name(row);
                    self.buf.push_str(&quote_identifier(&name));
                    self.buf.push_str(".*");
                }
                None => {
                    self.buf.push_str("ROW(");
                    self.list(&row.fields, true)?;
                    self.buf.push(')');
                }
            },
            ExprKind::RowCompare(rc) => self.row_compare(rc)?,
            ExprKind::MinMax { greatest, args, .. } => {
                self.buf.push_str(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.list(args, true)?;
                self.buf.push(')');
            }
            ExprKind::NullIf { args, .. } => {
                self.buf.push_str("NULLIF(");
                self.list(args, true)?;
                self.buf.push(')');
            }
            ExprKind::Distinct { not, args, .. } => {
                // A `NOT` over the test is a `BoolExpr` in PostgreSQL.
                if *not {
                    self.open();
                    self.buf.push_str("NOT ");
                }
                self.open();
                self.paren(&args[0], true, e)?;
                self.buf.push_str(" IS DISTINCT FROM ");
                self.paren(&args[1], true, e)?;
                self.close();
                if *not {
                    self.close();
                }
            }
            ExprKind::ScalarArrayOp { func, any, args } => {
                self.open();
                self.paren(&args[0], true, e)?;
                let element = builtin::type_by_oid(args[1].ty).map_or(0, |t| t.elem);
                let op = builtin::operators()
                    .iter()
                    .filter(|op| op.code == *func)
                    .max_by_key(|op| (op.left == args[0].ty, op.right == element))
                    .ok_or_else(|| Error::internal(format!("no operator for function {func}")))?;
                self.buf.push_str(&format!(" {} {} (", op.name, if *any { "ANY" } else { "ALL" }));
                self.paren(&args[1], true, e)?;
                // A bare scalar subquery after `ANY` reads back as an `ANY` subquery, so the text has a cast that the analyzer takes away again.
                if let ExprKind::SubLink(sub) = &args[1].kind
                    && sub.kind == SubLinkKind::Expr
                {
                    let ty = reg::type_text(args[1].ty, Some(args[1].typmod), self.session)?;
                    self.buf.push_str(&format!("::{ty}"));
                }
                self.buf.push(')');
                self.close();
            }
            ExprKind::Array { elements, .. } => {
                self.buf.push_str("ARRAY[");
                self.list(elements, true)?;
                self.buf.push(']');
                if elements.is_empty() {
                    let ty = reg::type_text(e.ty, Some(-1), self.session)?;
                    self.buf.push_str(&format!("::{ty}"));
                }
            }
            ExprKind::SqlValue(value) => self.buf.push_str(&sql_value(*value)),
            ExprKind::Agg(agg) => self.aggregate(agg)?,
            ExprKind::Grouping(g) => {
                self.buf.push_str("GROUPING(");
                self.list(&g.args, true)?;
                self.buf.push(')');
            }
            // A `PlaceHolderVar` above the node that computes it reads it from below, as `get_variable` shows a target of the node below that is not a column.
            ExprKind::PlaceHolder(p) => {
                // A placeholder is the value that a node below gives already.
                let saved = std::mem::take(&mut self.computed);
                self.buf.push('(');
                let done = self.expr(&p.expr, true);
                self.buf.push(')');
                self.computed = saved;
                done?;
            }
            ExprKind::Window(w) => self.window_func(w)?,
            ExprKind::SubLink(sub) => {
                // A node above the node that runs the subplan shows a copy of the expression of the node below, as `get_special_variable` shows it, so a subquery that is not one of the plan shows the subplan of the same subquery.
                let named = self.subplans.iter().find(|s| ptr::eq(s.link, sub.as_ref()));
                let named = named.or_else(|| self.subplans.iter().find(|s| *s.link == **sub));
                match named.cloned() {
                    Some(name) => self.subplan_link(sub, &name)?,
                    None => self.sublink(sub)?,
                }
            }
            ExprKind::SubColumn(column) => {
                let Some(subplan) = &self.subplan else {
                    return Err(Error::internal("a column of a subquery outside its test"));
                };
                self.buf.push_str(&format!("{subplan}.col{}", column + 1));
            }
            ExprKind::JsonValue(v) => {
                self.expr(&v.raw, false)?;
                json_format(v.format, &mut self.buf);
            }
            ExprKind::JsonCtor(c) => self.json_ctor(c)?,
            ExprKind::Xml(x) => self.xml_expr(x)?,
            ExprKind::JsonExpr(j) => self.json_expr(j, implicit)?,
            ExprKind::JsonTable(_) => return Err(Error::internal("JSON_TABLE out of FROM")),
            ExprKind::CurrentOf(c) => match c.args.first().map(|a| &a.kind) {
                Some(ExprKind::Param(n)) => self.buf.push_str(&format!("CURRENT OF ${n}")),
                Some(ExprKind::Const(name)) => {
                    let name = name.as_str().unwrap_or_default();
                    self.buf.push_str(&format!("CURRENT OF {}", quote_identifier(name)));
                }
                _ => return Err(Error::internal("CURRENT OF with no cursor")),
            },
            ExprKind::IsJson(p) => {
                self.open();
                self.paren(&p.expr, true, e)?;
                self.buf.push_str(" IS JSON");
                self.buf.push_str(match p.item {
                    JsonItemType::Any => "",
                    JsonItemType::Object => " OBJECT",
                    JsonItemType::Array => " ARRAY",
                    JsonItemType::Scalar => " SCALAR",
                });
                if p.unique {
                    self.buf.push_str(" WITH UNIQUE KEYS");
                }
                self.close();
            }
        }
        Ok(())
    }

    /// The case of `get_rule_expr` for an `XmlExpr`.
    fn xml_expr(&mut self, x: &XmlExpr) -> Result<()> {
        self.buf.push_str(match x.op {
            XmlOp::Concat => "XMLCONCAT(",
            XmlOp::Element => "XMLELEMENT(",
            XmlOp::Forest => "XMLFOREST(",
            XmlOp::Parse => "XMLPARSE(",
            XmlOp::Pi => "XMLPI(",
            XmlOp::Root => "XMLROOT(",
            XmlOp::Serialize => "XMLSERIALIZE(",
            XmlOp::Document => "",
        });
        if matches!(x.op, XmlOp::Parse | XmlOp::Serialize) {
            self.buf.push_str(if x.document { "DOCUMENT " } else { "CONTENT " });
        }
        let mut comma = false;
        if let Some(name) = &x.name {
            self.buf.push_str("NAME ");
            self.buf.push_str(&quote_identifier(&sql_name(name)));
            comma = true;
        }
        if !x.named.is_empty() {
            let forest = x.op == XmlOp::Forest;
            if !forest {
                if comma {
                    self.buf.push_str(", ");
                }
                self.buf.push_str("XMLATTRIBUTES(");
                comma = false;
            }
            for (name, e) in &x.named {
                if comma {
                    self.buf.push_str(", ");
                }
                self.expr(e, true)?;
                self.buf.push_str(" AS ");
                self.buf.push_str(&quote_identifier(&sql_name(name)));
                comma = true;
            }
            if !forest {
                self.buf.push(')');
            }
        }
        if !x.args.is_empty() {
            if comma {
                self.buf.push_str(", ");
            }
            match x.op {
                XmlOp::Parse => {
                    self.expr(&x.args[0], true)?;
                    let preserve = matches!(
                        x.args.get(1).map(|e| &e.kind),
                        Some(ExprKind::Const(Value::Bool(true)))
                    );
                    self.buf.push_str(if preserve {
                        " PRESERVE WHITESPACE"
                    } else {
                        " STRIP WHITESPACE"
                    });
                }
                XmlOp::Root => {
                    self.expr(&x.args[0], true)?;
                    self.buf.push_str(", VERSION ");
                    match x.args.get(1) {
                        Some(Expr { kind: ExprKind::Const(Value::Null), .. }) | None => {
                            self.buf.push_str("NO VALUE");
                        }
                        Some(version) => self.expr(version, false)?,
                    }
                    self.buf.push_str(match x.args.get(2).map(|e| &e.kind) {
                        Some(ExprKind::Const(Value::Int4(0))) => ", STANDALONE YES",
                        Some(ExprKind::Const(Value::Int4(1))) => ", STANDALONE NO",
                        Some(ExprKind::Const(Value::Int4(2))) => ", STANDALONE NO VALUE",
                        _ => "",
                    });
                }
                // The argument is a list, which is never simple.
                XmlOp::Document => {
                    self.open_pretty();
                    self.list(&x.args, false)?;
                    self.close_pretty();
                }
                _ => self.list(&x.args, true)?,
            }
        }
        if x.op == XmlOp::Serialize {
            self.buf.push_str(" AS ");
            self.buf.push_str(&reg::type_text(x.target.0, Some(x.target.1), self.session)?);
            self.buf.push_str(if x.indent { " INDENT" } else { " NO INDENT" });
        }
        self.buf.push_str(if x.op == XmlOp::Document { " IS DOCUMENT" } else { ")" });
        Ok(())
    }

    /// The case of `get_rule_expr` for a `JsonExpr`, with `get_json_path_spec`, `get_json_returning` and `get_json_expr_options`.
    fn json_expr(&mut self, j: &JsonExpr, showimplicit: bool) -> Result<()> {
        self.buf.push_str(match j.op {
            JsonExprOp::Exists => "JSON_EXISTS(",
            JsonExprOp::Query => "JSON_QUERY(",
            JsonExprOp::Value => "JSON_VALUE(",
        });
        self.expr(&j.formatted, showimplicit)?;
        self.buf.push_str(", ");
        match &j.path.kind {
            ExprKind::Const(value) => self.constant(&j.path, value, -1)?,
            _ => self.expr(&j.path, showimplicit)?,
        }
        for (i, (name, value)) in j.passing.iter().enumerate() {
            self.buf.push_str(if i == 0 { " PASSING " } else { ", " });
            self.expr(value, showimplicit)?;
            self.buf.push_str(" AS ");
            self.buf.push_str(&quote_identifier(name));
        }
        let returning = j.returning;
        if j.op != JsonExprOp::Exists || returning.ty != oid::BOOL {
            let ty = reg::type_text(returning.ty, Some(returning.typmod), self.session)?;
            self.buf.push_str(&format!(" RETURNING {ty}"));
            let default = if returning.ty == oid::JSONB {
                JsonFormatKind::Jsonb
            } else {
                JsonFormatKind::Json
            };
            if j.op != JsonExprOp::Query || returning.format.kind != default {
                json_format(returning.format, &mut self.buf);
            }
        }
        if j.op == JsonExprOp::Query {
            self.buf.push_str(match j.wrapper {
                JsonWrapper::Conditional => " WITH CONDITIONAL WRAPPER",
                JsonWrapper::Unconditional => " WITH UNCONDITIONAL WRAPPER",
                JsonWrapper::None | JsonWrapper::Unspec => " WITHOUT WRAPPER",
            });
            self.buf.push_str(if j.omit_quotes { " OMIT QUOTES" } else { " KEEP QUOTES" });
        }
        let default = if j.op == JsonExprOp::Exists {
            JsonBehaviorKind::False
        } else {
            JsonBehaviorKind::Null
        };
        if let Some(on_empty) = &j.on_empty
            && on_empty.kind != default
        {
            self.json_behavior(on_empty, "EMPTY")?;
        }
        if j.on_error.kind != default {
            self.json_behavior(&j.on_error, "ERROR")?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_json_table`: a call of `JSON_TABLE` in `FROM`.
    pub(crate) fn json_table(&mut self, t: &JsonTable) -> Result<()> {
        let JsonTablePlan::Scan(root) = &t.plan else {
            return Err(Error::internal("a JSON_TABLE plan with no scan at the top"));
        };
        self.buf.push_str("JSON_TABLE(");
        self.indent += INDENT_VAR;
        self.keyword("", 0, 0, 0);
        self.expr(&t.context, false)?;
        self.buf.push_str(", ");
        self.json_table_path(root)?;
        if !t.passing.is_empty() {
            self.buf.push(' ');
            self.keyword("PASSING ", 0, 0, 0);
            self.indent += INDENT_VAR;
            for (i, (name, value)) in t.passing.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.keyword("", 0, 0, 0);
                self.expr(value, false)?;
                self.buf.push_str(" AS ");
                self.buf.push_str(&quote_identifier(name));
            }
            self.indent -= INDENT_VAR;
        }
        self.json_table_columns(t, root)?;
        match t.on_error {
            JsonBehaviorKind::EmptyArray => {}
            kind => {
                self.buf.push(' ');
                self.buf.push_str(kind.name());
                self.buf.push_str(" ON ERROR");
            }
        }
        self.indent -= INDENT_VAR;
        self.keyword(")", 0, 0, 0);
        Ok(())
    }

    /// The path of a scan and its name.
    fn json_table_path(&mut self, scan: &JsonTableScan) -> Result<()> {
        let path = Expr::new(ExprKind::Const(scan.path.clone()), oid::JSONPATH);
        self.constant(&path, &scan.path, -1)?;
        self.buf.push_str(" AS ");
        self.buf.push_str(&quote_identifier(&scan.name));
        Ok(())
    }

    /// `get_json_table_columns`: the columns of a scan, then its `NESTED` parts.
    fn json_table_columns(&mut self, t: &JsonTable, scan: &JsonTableScan) -> Result<()> {
        self.buf.push(' ');
        self.keyword("COLUMNS (", 0, 0, 0);
        self.indent += INDENT_VAR;
        for at in scan.columns.clone() {
            let column = &t.columns[at];
            if at > scan.columns.start {
                self.buf.push_str(", ");
            }
            self.keyword("", 0, 0, 0);
            self.buf.push_str(&quote_identifier(&column.name));
            self.buf.push(' ');
            let Some(ExprKind::JsonExpr(j)) = column.expr.as_ref().map(|e| &e.kind) else {
                self.buf.push_str("FOR ORDINALITY");
                continue;
            };
            self.buf.push_str(&reg::type_text(column.ty, Some(column.typmod), self.session)?);
            let default = if j.op == JsonExprOp::Exists {
                self.buf.push_str(" EXISTS");
                JsonBehaviorKind::False
            } else {
                if j.op == JsonExprOp::Query
                    && builtin::type_by_oid(column.ty).is_some_and(|row| row.category == b'S')
                {
                    self.buf.push_str(" FORMAT JSON");
                }
                JsonBehaviorKind::Null
            };
            self.buf.push_str(" PATH ");
            match &j.path.kind {
                ExprKind::Const(value) => self.constant(&j.path, value, -1)?,
                _ => self.expr(&j.path, false)?,
            }
            if j.op == JsonExprOp::Query {
                self.buf.push_str(match j.wrapper {
                    JsonWrapper::Conditional => " WITH CONDITIONAL WRAPPER",
                    JsonWrapper::Unconditional => " WITH UNCONDITIONAL WRAPPER",
                    JsonWrapper::None | JsonWrapper::Unspec => " WITHOUT WRAPPER",
                });
                self.buf.push_str(if j.omit_quotes { " OMIT QUOTES" } else { " KEEP QUOTES" });
            }
            if let Some(on_empty) = &j.on_empty
                && on_empty.kind != default
            {
                self.json_behavior(on_empty, "EMPTY")?;
            }
            if j.on_error.kind != default {
                self.json_behavior(&j.on_error, "ERROR")?;
            }
        }
        if let Some(child) = &scan.child {
            self.json_table_nested(t, child, !scan.columns.is_empty())?;
        }
        self.indent -= INDENT_VAR;
        self.keyword(")", 0, 0, 0);
        Ok(())
    }

    /// `get_json_table_nested_columns`.
    fn json_table_nested(
        &mut self,
        t: &JsonTable,
        plan: &JsonTablePlan,
        comma: bool,
    ) -> Result<()> {
        match plan {
            JsonTablePlan::Scan(scan) => {
                if comma {
                    self.buf.push(',');
                }
                self.buf.push(' ');
                self.keyword("NESTED PATH ", 0, 0, 0);
                self.json_table_path(scan)?;
                self.json_table_columns(t, scan)
            }
            JsonTablePlan::Sibling(left, right) => {
                self.json_table_nested(t, left, comma)?;
                self.json_table_nested(t, right, true)
            }
        }
    }

    /// `get_json_behavior`.
    fn json_behavior(&mut self, behavior: &JsonBehavior, on: &str) -> Result<()> {
        match (&behavior.expr, behavior.kind) {
            (Some(expr), JsonBehaviorKind::Default) => {
                self.buf.push_str(" DEFAULT ");
                self.expr(expr, false)?;
            }
            (_, kind) => {
                self.buf.push(' ');
                self.buf.push_str(kind.name());
            }
        }
        self.buf.push_str(" ON ");
        self.buf.push_str(on);
        Ok(())
    }

    /// `get_json_constructor` and `get_json_constructor_options`.
    fn json_ctor(&mut self, c: &JsonCtor) -> Result<()> {
        self.buf.push_str(match c.kind {
            JsonCtorKind::Object => "JSON_OBJECT(",
            JsonCtorKind::Array => "JSON_ARRAY(",
            JsonCtorKind::Parse => "JSON(",
            JsonCtorKind::Scalar => "JSON_SCALAR(",
            JsonCtorKind::Serialize => "JSON_SERIALIZE(",
            JsonCtorKind::ObjectAgg | JsonCtorKind::ArrayAgg => return self.json_agg_ctor(c),
            JsonCtorKind::ArrayQuery => return self.json_array_query(c),
        });
        for (i, arg) in c.args.iter().enumerate() {
            if i > 0 {
                let object = c.kind == JsonCtorKind::Object && i % 2 != 0;
                self.buf.push_str(if object { " : " } else { ", " });
            }
            self.expr(arg, true)?;
        }
        self.json_ctor_options(c)?;
        self.buf.push(')');
        Ok(())
    }

    /// `get_json_agg_constructor`: the call of the aggregate or of the window function with the name and the options of the constructor.
    fn json_agg_ctor(&mut self, c: &JsonCtor) -> Result<()> {
        let saved = std::mem::take(&mut self.buf);
        let done = self.json_ctor_options(c);
        let options = std::mem::replace(&mut self.buf, saved);
        done?;
        let object = c.kind == JsonCtorKind::ObjectAgg;
        let json =
            Some((if object { "JSON_OBJECTAGG" } else { "JSON_ARRAYAGG" }, &*options, object));
        match c.func.as_ref().map(|f| &f.kind) {
            Some(ExprKind::Agg(agg)) => self.aggregate_as(agg, json),
            Some(ExprKind::Window(w)) => self.window_func_as(w, json),
            _ => Err(Error::internal("invalid JsonConstructorExpr underlying node type")),
        }
    }

    /// `get_json_constructor_options`: `ABSENT ON NULL`, `NULL ON NULL`, `WITH UNIQUE KEYS` and `RETURNING`.
    fn json_ctor_options(&mut self, c: &JsonCtor) -> Result<()> {
        if c.absent_on_null {
            if matches!(c.kind, JsonCtorKind::Object | JsonCtorKind::ObjectAgg) {
                self.buf.push_str(" ABSENT ON NULL");
            }
        } else if matches!(c.kind, JsonCtorKind::Array | JsonCtorKind::ArrayAgg) {
            self.buf.push_str(" NULL ON NULL");
        }
        if c.unique {
            self.buf.push_str(" WITH UNIQUE KEYS");
        }
        if !matches!(c.kind, JsonCtorKind::Parse | JsonCtorKind::Scalar) {
            let returning = c.returning;
            let ty = reg::type_text(returning.ty, Some(returning.typmod), self.session)?;
            self.buf.push_str(&format!(" RETURNING {ty}"));
            let default = if returning.ty == oid::JSONB {
                JsonFormatKind::Jsonb
            } else {
                JsonFormatKind::Json
            };
            if returning.format.kind != default {
                json_format(returning.format, &mut self.buf);
            }
        }
        Ok(())
    }

    /// `get_const_expr`. `showtype` is -1 to show no type, 0 to show the type when the text alone does not give it, and 1 to always show it.
    fn constant(&mut self, e: &Expr, value: &Value, showtype: i32) -> Result<()> {
        let label = |this: &Self| reg::type_text(e.ty, Some(e.typmod), this.session);
        if value.is_null() {
            self.buf.push_str("NULL");
            if showtype >= 0 {
                let ty = label(self)?;
                self.buf.push_str(&format!("::{ty}"));
            }
            return Ok(());
        }
        let text = to_text(e.ty, value, self.session)?;
        let mut need = false;
        match e.ty {
            oid::INT4 if !text.starts_with('-') => self.buf.push_str(&text),
            oid::NUMERIC
                if text.starts_with(|c: char| c.is_ascii_digit())
                    && text.contains(['e', 'E', '.']) =>
            {
                self.buf.push_str(&text);
            }
            oid::INT4 | oid::NUMERIC => {
                self.buf.push_str(&format!("'{text}'"));
                need = true;
            }
            oid::BOOL => {
                self.buf.push_str(if value.as_bool() == Some(true) { "true" } else { "false" })
            }
            _ => {
                // `simple_quote_literal` with `standard_conforming_strings` on doubles only the quote.
                self.buf.push('\'');
                self.buf.push_str(&text.replace('\'', "''"));
                self.buf.push('\'');
            }
        }
        if showtype < 0 {
            return Ok(());
        }
        let need = match e.ty {
            oid::BOOL | oid::UNKNOWN => false,
            oid::INT4 => need,
            oid::NUMERIC => need || e.typmod >= 0,
            _ => true,
        };
        if need || showtype > 0 {
            let ty = label(self)?;
            self.buf.push_str(&format!("::{ty}"));
        }
        Ok(())
    }

    /// A `RelabelType` or a `CoerceViaIO`.
    fn cast_node(
        &mut self,
        e: &Expr,
        arg: &Expr,
        form: CastForm,
        typmod: i32,
        implicit: bool,
    ) -> Result<()> {
        if form == CastForm::Implicit && !implicit {
            self.paren(arg, false, e)
        } else {
            self.coercion(arg, e.ty, typmod, e)
        }
    }

    /// `get_coercion_expr`: `arg::type`. A constant of the same type shows no type of its own, because the cast shows it.
    fn coercion(&mut self, arg: &Expr, ty: u32, typmod: i32, parent: &Expr) -> Result<()> {
        match &arg.kind {
            ExprKind::Const(value) if arg.ty == ty && arg.typmod == -1 => {
                self.constant(arg, value, -1)?;
            }
            _ => {
                self.open();
                self.paren(arg, false, parent)?;
                self.close();
            }
        }
        let ty = reg::type_text(ty, Some(typmod), self.session)?;
        self.buf.push_str(&format!("::{ty}"));
        Ok(())
    }

    /// `get_func_expr` and `get_oper_expr`.
    fn func(&mut self, e: &Expr, f: &Func, implicit: bool) -> Result<()> {
        match f.form {
            FuncForm::Operator(opno) => {
                let op = builtin::operator_by_oid(opno).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for operator {opno}"))
                })?;
                self.open();
                if let [left, right] = f.args.as_slice() {
                    self.paren(left, true, e)?;
                    self.buf.push_str(&format!(" {} ", op.name));
                    self.paren(right, true, e)?;
                } else {
                    self.buf.push_str(&format!("{} ", op.name));
                    self.paren(&f.args[0], true, e)?;
                }
                self.close();
            }
            FuncForm::ImplicitCast if !implicit => self.paren(&f.args[0], false, e)?,
            FuncForm::ExplicitCast | FuncForm::ImplicitCast => {
                self.coercion(&f.args[0], e.ty, e.typmod, e)?;
            }
            FuncForm::SqlSyntax => {
                if !self.sql_syntax(e, f)? {
                    self.call(f)?;
                }
            }
            FuncForm::Call => self.call(f)?,
        }
        Ok(())
    }

    /// A `RowCompareExpr`: both rows with `ROW`, and the name of the operator of the first pair of fields.
    fn row_compare(&mut self, rc: &RowCompare) -> Result<()> {
        let mut pairs = Vec::with_capacity(rc.ops.len());
        let mut name = "";
        for op in &rc.ops {
            let ExprKind::Func(Func { form: FuncForm::Operator(opno), args, .. }) = &op.kind else {
                return Err(Error::internal("a row comparison that is not an operator"));
            };
            let [left, right] = args.as_slice() else {
                return Err(Error::internal("a row comparison operator needs two arguments"));
            };
            if name.is_empty() {
                name = builtin::operator_by_oid(*opno).map(|op| op.name).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for operator {opno}"))
                })?;
            }
            pairs.push((left, right));
        }
        self.buf.push_str("(ROW(");
        for (i, (left, _)) in pairs.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.toplevel(left, true)?;
        }
        self.buf.push_str(&format!(") {name} ROW("));
        for (i, (_, right)) in pairs.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.toplevel(right, true)?;
        }
        self.buf.push_str("))");
        Ok(())
    }

    /// A call in the form `name(args)`.
    fn call(&mut self, f: &Func) -> Result<()> {
        let proc = builtin::proc_by_oid(f.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", f.oid))
        })?;
        // `generate_function_name`: the name has its schema when the name alone does not find the function.
        if reg::function_visible(proc, self.session) {
            self.buf.push_str(&quote_identifier(proc.name));
        } else {
            let schema = reg::namespace_name(proc.namespace, self.session);
            self.buf.push_str(&reg::qualified(schema, proc.name));
        }
        self.buf.push('(');
        let variadic = f.variadic && proc.variadic != 0;
        for (i, arg) in f.args.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            if variadic && i + 1 == f.args.len() {
                self.buf.push_str("VARIADIC ");
            }
            self.expr(arg, true)?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// A `CASE`. With an argument, a condition `CaseTestExpr = value` shows only the value.
    fn case(&mut self, case: &Case) -> Result<()> {
        self.keyword("CASE", 0, INDENT_VAR, 0);
        if let Some(arg) = &case.arg {
            self.buf.push(' ');
            self.expr(arg, true)?;
        }
        for (when, result) in &case.whens {
            let mut shown = when;
            if case.arg.is_some()
                && let ExprKind::Func(Func { form: FuncForm::Operator(_), args, .. }) = &when.kind
                && let [left, right] = args.as_slice()
                && matches!(left.strip_implicit().kind, ExprKind::CaseTest)
            {
                shown = right;
            }
            self.space();
            self.keyword("WHEN ", 0, 0, 0);
            self.expr(shown, false)?;
            self.buf.push_str(" THEN ");
            self.expr(result, true)?;
        }
        self.space();
        self.keyword("ELSE ", 0, 0, 0);
        self.expr(&case.default, true)?;
        self.space();
        self.keyword("END", -INDENT_VAR, 0, 0);
        Ok(())
    }
}

/// The name of a system column.
fn system_column(attnum: i16) -> Option<&'static str> {
    Some(match attnum {
        -1 => "ctid",
        -2 => "xmin",
        -3 => "cmin",
        -4 => "xmax",
        -5 => "cmax",
        -6 => "tableoid",
        _ => return None,
    })
}

/// The text of a `SQLValueFunction`.
fn sql_value(value: SqlValue) -> String {
    let with = |name: &str, precision: Option<i32>| match precision {
        Some(p) => format!("{name}({p})"),
        None => name.to_string(),
    };
    match value {
        SqlValue::CurrentDate => "CURRENT_DATE".to_string(),
        SqlValue::CurrentTime(p) => with("CURRENT_TIME", p),
        SqlValue::CurrentTimestamp(p) => with("CURRENT_TIMESTAMP", p),
        SqlValue::LocalTime(p) => with("LOCALTIME", p),
        SqlValue::LocalTimestamp(p) => with("LOCALTIMESTAMP", p),
        SqlValue::CurrentRole => "CURRENT_ROLE".to_string(),
        SqlValue::CurrentUser => "CURRENT_USER".to_string(),
        SqlValue::User => "USER".to_string(),
        SqlValue::SessionUser => "SESSION_USER".to_string(),
        SqlValue::CurrentCatalog => "CURRENT_CATALOG".to_string(),
        SqlValue::CurrentSchema => "CURRENT_SCHEMA".to_string(),
    }
}

/// The name of an operator with one character, as `get_simple_binary_op_name` gives it for an operator with two arguments.
fn simple_binary_op(e: &Expr) -> Option<u8> {
    match &e.kind {
        ExprKind::Func(Func { form: FuncForm::Operator(opno), args, .. }) if args.len() == 2 => {
            builtin::operator_by_oid(*opno).and_then(|op| match op.name.as_bytes() {
                [c] => Some(*c),
                _ => None,
            })
        }
        _ => None,
    }
}

/// True for a node that shows its arguments between its own parentheses or keywords, the parents that the cases of `isSimpleNode` name for a test and a `BoolExpr`. A call shows a cast with `::`, which binds tighter than a test.
fn own_syntax(parent: &Expr) -> bool {
    match &parent.kind {
        ExprKind::Func(f) => f.form == FuncForm::Call,
        ExprKind::Bool(..)
        | ExprKind::Subscript(_)
        | ExprKind::Array { .. }
        | ExprKind::Row(..)
        | ExprKind::JoinRow(_)
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::NullIf { .. }
        | ExprKind::Agg(_)
        | ExprKind::Grouping(_)
        | ExprKind::PlaceHolder(_)
        | ExprKind::Window(_)
        | ExprKind::Xml(_)
        | ExprKind::Case(_) => true,
        _ => false,
    }
}

/// The case of `isSimpleNode` for a `BoolExpr` with the operator `op`.
fn bool_simple(op: BoolOp, parent: &Expr, paren: bool) -> bool {
    match &parent.kind {
        ExprKind::Bool(parent_op, _) => {
            paren
                && match op {
                    BoolOp::Not | BoolOp::And => matches!(parent_op, BoolOp::And | BoolOp::Or),
                    BoolOp::Or => *parent_op == BoolOp::Or,
                }
        }
        _ => own_syntax(parent),
    }
}

/// `get_json_format`: the `FORMAT` clause, or nothing for the default format.
fn json_format(format: JsonFormat, buf: &mut String) {
    match format.kind {
        JsonFormatKind::Default => return,
        JsonFormatKind::Json => buf.push_str(" FORMAT JSON"),
        JsonFormatKind::Jsonb => buf.push_str(" FORMAT JSONB"),
    }
    if format.encoding != JsonEncoding::Default {
        buf.push_str(" ENCODING ");
        buf.push_str(format.encoding.name());
    }
}

/// `isSimpleNode`: true when the node needs no parentheses in its parent.
fn is_simple(e: &Expr, parent: &Expr, paren: bool) -> bool {
    match &e.kind {
        ExprKind::Const(_)
        | ExprKind::Param(_)
        | ExprKind::Var(_)
        | ExprKind::SubColumn(_)
        | ExprKind::DomainValue => true,
        // The `.` binds the most, but a field of a field needs parentheses.
        ExprKind::FieldSelect(..) => !matches!(parent.kind, ExprKind::FieldSelect(..)),
        ExprKind::Array { .. }
        | ExprKind::Row(..)
        | ExprKind::JoinRow(_)
        | ExprKind::Subscript(_)
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::SqlValue(_)
        | ExprKind::NullIf { .. }
        | ExprKind::Agg(_)
        | ExprKind::Grouping(_)
        | ExprKind::PlaceHolder(_)
        | ExprKind::Window(_)
        | ExprKind::JsonCtor(_)
        | ExprKind::JsonExpr(_)
        | ExprKind::JsonTable(_)
        | ExprKind::Xml(_)
        | ExprKind::CurrentOf(_)
        | ExprKind::Case(_) => true,
        ExprKind::Func(f) if !matches!(f.form, FuncForm::Operator(_)) => true,
        ExprKind::JsonValue(v) => is_simple(&v.raw, e, paren),
        ExprKind::Relabel(arg, _)
        | ExprKind::CoerceViaIo(arg, _)
        | ExprKind::ConvertRow { arg, .. }
        | ExprKind::ArrayCoerce { arg, .. }
        | ExprKind::CoerceToDomain(arg, _) => is_simple(arg, e, paren),
        ExprKind::Func(f) => {
            if paren
                && matches!(&parent.kind, ExprKind::Func(p) if matches!(p.form, FuncForm::Operator(_)))
            {
                let (Some(op), Some(parent_op)) = (simple_binary_op(e), simple_binary_op(parent))
                else {
                    return false;
                };
                let (low, high) = (b"+-".contains(&op), b"*/%".contains(&op));
                let (parent_low, parent_high) =
                    (b"+-".contains(&parent_op), b"*/%".contains(&parent_op));
                if !(low || high) || !(parent_low || parent_high) {
                    return false;
                }
                if high && parent_low {
                    return true;
                }
                if low && parent_high {
                    return false;
                }
                // The same priority: `(a - b) - c` needs no parentheses, `a - (b - c)` does.
                let ExprKind::Func(p) = &parent.kind else { return false };
                return p.args.first().is_some_and(|first| ptr::eq(first, e)) && !f.args.is_empty();
            }
            own_syntax(parent)
        }
        ExprKind::SubLink(_)
        | ExprKind::NullTest(..)
        | ExprKind::BooleanTest(..)
        | ExprKind::IsJson(_) => own_syntax(parent),
        ExprKind::Distinct { not: false, .. } => own_syntax(parent),
        ExprKind::Distinct { not: true, .. } => bool_simple(BoolOp::Not, parent, paren),
        ExprKind::Bool(op, _) => bool_simple(*op, parent, paren),
        ExprKind::ScalarArrayOp { .. }
        | ExprKind::RowCompare(_)
        | ExprKind::CaseTest
        | ExprKind::FieldStore(_)
        | ExprKind::Collate(..)
        | ExprKind::NamedArg(_) => false,
    }
}

/// True when a `Var` is in the expression.
fn has_var(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Var(_)) || e.children().into_iter().any(has_var)
}

impl Deparser<'_> {
    /// `get_name_for_var_field`: the name of the field with the index from 0. A value of type `record` takes the name from the expression that gives it, as the analyzer finds the fields.
    fn field_name(&self, arg: &Expr, field: usize) -> Result<String> {
        if arg.ty == oid::RECORD {
            let levels: Vec<&[rupg_analyze::Relation]> =
                self.namespaces.iter().rev().map(|n| &n.relations[..]).collect();
            if let Some(columns) = rupg_analyze::record_fields(arg, &levels)?
                && let Some(column) = columns.get(field)
            {
                return Ok(column.name.clone());
            }
        }
        row_field_name(arg.ty, field, self.session)
    }

    /// `printSubscripts`: the subscripts of each dimension, with the bounds of a slice.
    fn subscripts(&mut self, sub: &Subscript) -> Result<()> {
        for (i, upper) in sub.upper.iter().enumerate() {
            self.buf.push('[');
            if let Some(lower) = &sub.lower {
                if let Some(Some(lower)) = lower.get(i) {
                    self.expr(lower, false)?;
                }
                self.buf.push(':');
            }
            if let Some(upper) = upper {
                self.expr(upper, false)?;
            }
            self.buf.push(']');
        }
        Ok(())
    }

    /// `processIndirection`: the fields and the subscripts of an assignment to a part of a column, after the name of the column, and the new value of the part, which the caller shows.
    pub(crate) fn indirection<'e>(&mut self, expr: &'e Expr) -> Result<&'e Expr> {
        let mut node = expr;
        loop {
            match &node.kind {
                ExprKind::FieldStore(store) => {
                    let Some((field, value)) = store.fields.first() else { break };
                    let name = row_field_name(node.ty, *field, self.session)?;
                    self.buf.push('.');
                    self.buf.push_str(&quote_identifier(&name));
                    node = value;
                }
                ExprKind::Subscript(sub) if sub.assign.is_some() => {
                    self.subscripts(sub)?;
                    let Some(value) = &sub.assign else { break };
                    node = value;
                }
                // An implicit domain check above a part steps down to the part.
                ExprKind::CoerceToDomain(arg, CastForm::Implicit)
                    if matches!(arg.kind, ExprKind::FieldStore(_) | ExprKind::Subscript(_)) =>
                {
                    node = arg;
                }
                _ => break,
            }
        }
        Ok(node)
    }
}

/// `get_name_for_var_field` for a value of a composite type: the name of the field with the index from 0.
fn row_field_name(ty: u32, field: usize, session: &dyn Session) -> Result<String> {
    row_names(ty, session).and_then(|names| names.get(field).cloned()).ok_or_else(|| {
        Error::internal(format!("could not identify column {} in a record", field + 1))
    })
}

/// The names of the fields of a composite type, also through a domain, or `None` when the type is not the row type of a relation. A value of the type has no field for a dropped column.
pub(crate) fn row_names(ty: u32, session: &dyn Session) -> Option<Vec<String>> {
    let mut ty = ty;
    let relid = loop {
        match builtin::type_by_oid(ty) {
            Some(row) if row.kind == b'd' && row.base != 0 => ty = row.base,
            Some(row) => break row.relid,
            None => {
                break session
                    .catalog()
                    .and_then(|c| c.relations().find(|r| r.row_type == ty))
                    .map_or(0, |r| r.oid);
            }
        }
    };
    if let Some(rel) = session.catalog().and_then(|c| c.relation(relid)) {
        return Some(rel.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect());
    }
    column_names(relid, session)
}

/// The names of the columns of a relation, by the column number from 1, or `None` when no relation has the OID.
fn column_names(relid: u32, session: &dyn Session) -> Option<Vec<String>> {
    if let Some(rel) = session.catalog().and_then(|c| c.relation(relid)) {
        return Some(rel.columns.iter().map(|c| c.name.clone()).collect());
    }
    builtin::class_by_oid(relid)?;
    let mut names = Vec::new();
    for attr in builtin::attributes(relid).iter().filter(|a| a.num > 0) {
        let at = usize::try_from(attr.num - 1).unwrap_or(0);
        if names.len() <= at {
            names.resize(at + 1, String::new());
        }
        names[at] = attr.name.to_string();
    }
    Some(names)
}

/// `deparse_expression_pretty` for a stored expression or a stored list of expressions, with the columns of the relation.
fn deparse(
    text: &str,
    name: &str,
    columns: &[String],
    paren: bool,
    session: &dyn Session,
) -> Result<String> {
    if node::is_bound(text) {
        return partition_bound(text, session);
    }
    let exprs = stored(text)?;
    let mut d = Deparser::for_columns(session, name, columns.to_vec(), paren);
    d.list(&exprs, false)?;
    Ok(d.buf)
}

/// The text of a constant of a stored bound of a partition, as `get_const_expr` with `showtype` -1 shows it.
fn bound_constant(d: &mut Deparser<'_>, e: &Expr) -> Result<()> {
    match &e.kind {
        ExprKind::Const(value) => d.constant(e, value, -1),
        _ => Err(Error::internal("a partition bound that is not a constant")),
    }
}

/// The `PartitionBoundSpec` part of `get_rule_expr`: the bound of a partition in `relpartbound`, as `FOR VALUES` shows it.
fn partition_bound(text: &str, session: &dyn Session) -> Result<String> {
    let mut d = Deparser::for_columns(session, "", Vec::new(), false);
    match node::read_bound(text)? {
        StoredBound::Default => d.buf.push_str("DEFAULT"),
        StoredBound::Hash { modulus, remainder } => {
            d.buf.push_str(&format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})"));
        }
        StoredBound::List(values) => {
            d.buf.push_str("FOR VALUES IN (");
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    d.buf.push_str(", ");
                }
                match value {
                    Some(e) => bound_constant(&mut d, e)?,
                    None => d.buf.push_str("NULL"),
                }
            }
            d.buf.push(')');
        }
        StoredBound::Range { lower, upper } => {
            d.buf.push_str("FOR VALUES FROM ");
            range_datums(&mut d, &lower)?;
            d.buf.push_str(" TO ");
            range_datums(&mut d, &upper)?;
        }
    }
    Ok(d.buf)
}

/// `get_range_partbound_string`: the datums of a bound of a range partition in parentheses.
fn range_datums(d: &mut Deparser<'_>, datums: &[StoredDatum]) -> Result<()> {
    d.buf.push('(');
    for (i, datum) in datums.iter().enumerate() {
        if i > 0 {
            d.buf.push_str(", ");
        }
        match datum {
            StoredDatum::MinValue => d.buf.push_str("MINVALUE"),
            StoredDatum::MaxValue => d.buf.push_str("MAXVALUE"),
            StoredDatum::Value(e) => bound_constant(d, e)?,
        }
    }
    d.buf.push(')');
    Ok(())
}

/// The text of a stored constant, as `get_const_expr` with `showtype` -1 shows it, for the detail of an error on the bound of a partition.
///
/// # Errors
///
/// An internal error for a stored form that is not a constant, and the errors of the output function of the type.
pub fn deparse_constant(text: &str, session: &dyn Session) -> Result<String> {
    let e = node::read(text)?;
    let mut d = Deparser::for_columns(session, "", Vec::new(), false);
    bound_constant(&mut d, &e)?;
    Ok(d.buf)
}

/// `deparse_expression` of a stored expression that has no column, such as the default of a domain in `typdefault`.
pub fn deparse_expression(text: &str, session: &dyn Session) -> Result<String> {
    deparse(text, "", &[], false, session)
}

/// `deparse_expression` of a stored expression of the relation `relid`, with the names of its columns, as `pg_get_expr` gives it. The relation 0 gives an expression with no columns, such as the check of a domain.
///
/// # Errors
///
/// An internal error for a relation that the catalog does not have, and the errors of the deparse.
pub fn deparse_relation_expression(
    text: &str,
    relid: u32,
    session: &dyn Session,
) -> Result<String> {
    if relid == 0 {
        return deparse_expression(text, session);
    }
    let columns = column_names(relid, session)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {relid}")))?;
    let name = reg::class_name(relid, session).map_or("", |(name, _)| name);
    deparse(text, name, &columns, false, session)
}

/// The expressions of a stored form. A list is `(...)`, and an expression is `{...}`.
fn stored(text: &str) -> Result<Vec<Expr>> {
    if text.trim_start().starts_with('(') {
        node::read_list(text)
    } else {
        Ok(vec![node::read(text)?])
    }
}

/// `generate_relation_name`: the name of the relation, with its schema when the name alone does not find it.
fn relation_name(relid: u32, session: &dyn Session) -> Result<String> {
    let (name, namespace) = reg::class_name(relid, session)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {relid}")))?;
    if reg::class_visible(relid, session) == Some(true) {
        Ok(quote_identifier(name))
    } else {
        Ok(reg::qualified(reg::namespace_name(namespace, session), name))
    }
}

/// `generate_qualified_relation_name`: the name of the relation with its schema.
fn qualified_relation_name(relid: u32, session: &dyn Session) -> Result<String> {
    let (name, namespace) = reg::class_name(relid, session)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {relid}")))?;
    Ok(reg::qualified(reg::namespace_name(namespace, session), name))
}

/// The pretty flags of a call: `PRETTYFLAG_PAREN` and `PRETTYFLAG_SCHEMA` when the argument at `index` is true. A call without the argument has only `PRETTYFLAG_INDENT`.
fn pretty_arg(args: &[Value], index: usize) -> Result<bool> {
    match args.get(index) {
        Some(value) => value.as_bool().ok_or_else(bad_value),
        None => Ok(false),
    }
}

/// The OID of the policy and true for `WITH CHECK` of the text that `pg_policy` gives for `polqual` and `polwithcheck`: `policy <oid> qual` or `policy <oid> check`. The catalog keeps the text of the statement of a policy, not a stored form.
fn policy_marker(text: &str) -> Option<(u32, bool)> {
    let rest = text.strip_prefix("policy ")?;
    let (oid, which) = rest.split_once(' ')?;
    let check = match which {
        "qual" => false,
        "check" => true,
        _ => return None,
    };
    Some((oid.parse().ok()?, check))
}

/// `pg_get_expr(pg_node_tree, oid [, bool])`.
fn get_expr(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let relid = args.get(1).and_then(Value::as_oid).ok_or_else(bad_value)?;
    let paren = pretty_arg(args, 2)?;
    let session = call.session;
    if let Some((oid, check)) = policy_marker(text) {
        return Ok(query::policy_expr(oid, check, paren, session)?.map_or(Value::Null, Value::text));
    }
    let columns = if relid == 0 {
        if stored(text)?.iter().any(has_var) {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "expression contains variables",
            ));
        }
        Vec::new()
    } else {
        match column_names(relid, session) {
            Some(columns) => columns,
            None => return Ok(Value::Null),
        }
    };
    let name = reg::class_name(relid, session).map_or("", |(name, _)| name);
    Ok(Value::text(deparse(text, name, &columns, paren, session)?))
}

/// The error for an object of a system catalog, which has no stored form that the engine can read yet.
fn system_object(what: &str) -> Error {
    not_yet(format!("{what} of a system catalog"))
}

/// The names of the columns, quoted, with `, ` between them, as `decompile_column_index_array`.
fn column_list(rel: &Relation, keys: &[i16]) -> Result<String> {
    let names: Result<Vec<String>> = keys
        .iter()
        .map(|&k| {
            rel.column(k).map(|c| quote_identifier(&c.name)).ok_or_else(|| {
                Error::internal(format!(
                    "cache lookup failed for attribute {k} of relation {}",
                    rel.oid
                ))
            })
        })
        .collect();
    Ok(names?.join(", "))
}

/// The text of a referential action, or `None` for `NO ACTION`.
fn action(code: char) -> Option<&'static str> {
    match code {
        'r' => Some("RESTRICT"),
        'c' => Some("CASCADE"),
        'n' => Some("SET NULL"),
        'd' => Some("SET DEFAULT"),
        _ => None,
    }
}

/// `pg_get_constraintdef_worker`.
fn constraint_def(con: &Constraint, paren: bool, session: &dyn Session) -> Result<String> {
    let catalog = session.catalog().ok_or_else(|| Error::internal("no catalog"))?;
    let relation = || {
        catalog.relation(con.relation).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for relation {}", con.relation))
        })
    };
    let mut out = String::new();
    match con.kind {
        ConKind::Foreign => {
            let fk = con
                .foreign
                .as_ref()
                .ok_or_else(|| Error::internal("a foreign key without its facts"))?;
            let target = catalog.relation(fk.table).ok_or_else(|| {
                Error::internal(format!("cache lookup failed for relation {}", fk.table))
            })?;
            // `decompile_column_index_array` with `withPeriod`: the last column is the period.
            let columns = |rel: &Relation, keys: &[i16]| -> Result<String> {
                match keys.split_last() {
                    Some((last, [])) if fk.period => {
                        Ok(format!("PERIOD {}", column_list(rel, &[*last])?))
                    }
                    Some((last, rest)) if fk.period => Ok(format!(
                        "{}, PERIOD {}",
                        column_list(rel, rest)?,
                        column_list(rel, &[*last])?
                    )),
                    _ => column_list(rel, keys),
                }
            };
            out.push_str(&format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                columns(relation()?, &con.keys)?,
                relation_name(fk.table, session)?,
                columns(target, &fk.keys)?
            ));
            out.push_str(match fk.match_type {
                'f' => " MATCH FULL",
                'p' => " MATCH PARTIAL",
                _ => "",
            });
            if let Some(update) = action(fk.update) {
                out.push_str(&format!(" ON UPDATE {update}"));
            }
            if let Some(delete) = action(fk.delete) {
                out.push_str(&format!(" ON DELETE {delete}"));
            }
            if !fk.del_set_cols.is_empty() {
                out.push_str(&format!(" ({})", column_list(relation()?, &fk.del_set_cols)?));
            }
        }
        ConKind::Primary | ConKind::Unique => {
            let index = catalog.relation(con.index).and_then(|r| r.index.as_ref());
            let index = index.ok_or_else(|| {
                Error::internal(format!("cache lookup failed for index {}", con.index))
            })?;
            out.push_str(if con.kind == ConKind::Primary { "PRIMARY KEY " } else { "UNIQUE " });
            if con.kind == ConKind::Unique && index.nulls_not_distinct {
                out.push_str("NULLS NOT DISTINCT ");
            }
            let rel = relation()?;
            let period = if con.period() { " WITHOUT OVERLAPS" } else { "" };
            out.push_str(&format!("({}{period})", column_list(rel, &con.keys)?));
            if index.keys.len() > con.keys.len() {
                out.push_str(&format!(
                    " INCLUDE ({})",
                    column_list(rel, &index.keys[con.keys.len()..])?
                ));
            }
        }
        ConKind::Check => {
            let text = con
                .expr
                .as_deref()
                .ok_or_else(|| Error::internal("a check constraint without its expression"))?;
            // The check of a domain has no relation and no column. It refers to the value with `VALUE`.
            let (name, columns): (&str, Vec<String>) = if con.domain == 0 {
                let rel = relation()?;
                (&rel.name, rel.columns.iter().map(|c| c.name.clone()).collect())
            } else {
                ("", Vec::new())
            };
            out.push_str(&format!("CHECK ({})", deparse(text, name, &columns, paren, session)?));
            if con.no_inherit {
                out.push_str(" NO INHERIT");
            }
        }
        // The `NOT NULL` constraint of a domain has no column.
        ConKind::NotNull if con.domain != 0 => out.push_str("NOT NULL"),
        ConKind::Trigger => out.push_str("TRIGGER"),
        ConKind::Exclusion => {
            let index = catalog.relation(con.index).and_then(|r| Some((r, r.index.as_ref()?)));
            let (index, info) = index.ok_or_else(|| {
                Error::internal(format!("cache lookup failed for index {}", con.index))
            })?;
            let exclude = Some(con.exclusion_ops.as_slice());
            out.push_str(&index_def(index, (info, exclude), 0, paren, session)?);
        }
        ConKind::NotNull => {
            out.push_str(&format!("NOT NULL {}", column_list(relation()?, &con.keys[..1])?));
            if con.no_inherit {
                out.push_str(" NO INHERIT");
            }
        }
    }
    if con.deferrable {
        out.push_str(" DEFERRABLE");
    }
    if con.deferred {
        out.push_str(" INITIALLY DEFERRED");
    }
    // The validated status does not matter for a constraint that is `NOT ENFORCED`.
    if !con.enforced {
        out.push_str(" NOT ENFORCED");
    } else if !con.validated {
        out.push_str(" NOT VALID");
    }
    Ok(out)
}

/// `pg_get_constraintdef(oid [, bool])`.
fn get_constraintdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let paren = pretty_arg(args, 1)?;
    match call.session.catalog().and_then(|c| c.constraint(oid)) {
        Some(con) => Ok(Value::text(constraint_def(con, paren, call.session)?)),
        None if oid < FIRST_USER_OID => {
            Ok(builtin_constraint_def(oid)?.map_or(Value::Null, Value::text))
        }
        None => Ok(Value::Null),
    }
}

/// The catalog and the row number of a static row, by its OID, or by `indexrelid` for `pg_index`.
fn builtin_position(catalog: &str, oid: u32) -> Option<(&'static rupg_pgcatalog::Catalog, usize)> {
    let cat = rupg_pgcatalog::catalog(catalog)?;
    let column = if catalog == "pg_index" { "indexrelid" } else { "oid" };
    match &cat.rows.get(cat.column(column)?)?.values {
        rupg_pgcatalog::Values::Oid(v) => Some((cat, v.iter().position(|&o| o == oid)?)),
        _ => None,
    }
}

/// The text form of a column of a static row.
fn builtin_field(cat: &rupg_pgcatalog::Catalog, row: usize, column: &str) -> Result<String> {
    cat.column(column)
        .and_then(|at| cat.rows.get(at)?.text(row))
        .ok_or_else(|| Error::internal(format!("no column {column} in {}", cat.name)))
}

/// The quoted names of columns of a built-in relation, with `, ` between them.
fn builtin_columns(relid: u32, keys: &[i16]) -> Result<String> {
    let names: Result<Vec<String>> = keys
        .iter()
        .map(|&k| {
            builtin::attributes(relid)
                .iter()
                .find(|a| a.num == k)
                .map(|a| quote_identifier(a.name))
                .ok_or_else(|| {
                    Error::internal(format!(
                        "cache lookup failed for attribute {k} of relation {relid}"
                    ))
                })
        })
        .collect();
    Ok(names?.join(", "))
}

/// The numbers in the text form of an `int2vector` or an `int2[]`.
fn int2_list(text: &str) -> Vec<i16> {
    text.trim_matches(['{', '}']).split([' ', ',']).filter_map(|n| n.parse().ok()).collect()
}

/// `pg_get_constraintdef` of a constraint of a system catalog: a primary key, a unique constraint or a `NOT NULL` constraint.
fn builtin_constraint_def(oid: u32) -> Result<Option<String>> {
    let Some((cat, row)) = builtin_position("pg_constraint", oid) else { return Ok(None) };
    let relid: u32 = builtin_field(cat, row, "conrelid")?.parse().map_err(|_| bad_value())?;
    let keys = int2_list(&builtin_field(cat, row, "conkey")?);
    let columns = builtin_columns(relid, &keys)?;
    Ok(Some(match builtin_field(cat, row, "contype")?.as_str() {
        "p" => format!("PRIMARY KEY ({columns})"),
        "u" => format!("UNIQUE ({columns})"),
        "n" => format!("NOT NULL {columns}"),
        _ => return Err(system_object("pg_get_constraintdef")),
    }))
}

/// `pg_get_indexdef` of an index of a system catalog. Each one is a B-tree on columns, with the default operator classes and no options.
fn builtin_index_def(
    oid: u32,
    colno: i32,
    paren: bool,
    session: &dyn Session,
) -> Result<Option<String>> {
    let Some((cat, row)) = builtin_position("pg_index", oid) else { return Ok(None) };
    let relid: u32 = builtin_field(cat, row, "indrelid")?.parse().map_err(|_| bad_value())?;
    let keys = int2_list(&builtin_field(cat, row, "indkey")?);
    if colno != 0 {
        let key = usize::try_from(colno - 1).ok().and_then(|at| keys.get(at));
        return Ok(Some(match key {
            Some(&key) => builtin_columns(relid, &[key])?,
            None => String::new(),
        }));
    }
    let index = builtin::class_by_oid(oid)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))?;
    let table = if paren {
        relation_name(relid, session)?
    } else {
        qualified_relation_name(relid, session)?
    };
    let unique = builtin_field(cat, row, "indisunique")? == "t";
    Ok(Some(format!(
        "CREATE {}INDEX {} ON {table} USING btree ({})",
        if unique { "UNIQUE " } else { "" },
        quote_identifier(index.name),
        builtin_columns(relid, &keys)?
    )))
}

/// The collation of an index expression, as `exprCollation` gives it: the collation of the columns in it when they agree, else the default collation for a type that has collations.
fn expr_collation(e: &Expr, rel: &Relation) -> u32 {
    fn walk(e: &Expr, rel: &Relation, found: &mut Vec<u32>) {
        if let ExprKind::Var(var) = &e.kind
            && let Some(column) = rel.column(var.attnum)
            && column.collation != 0
        {
            found.push(column.collation);
        }
        for child in e.children() {
            walk(child, rel, found);
        }
    }
    if !rupg_analyze::types::collatable(e.ty) {
        return 0;
    }
    let mut found = Vec::new();
    walk(e, rel, &mut found);
    match found.first() {
        Some(&first) if found.iter().all(|&c| c == first) => first,
        Some(_) => 0,
        None => DEFAULT_COLLATION,
    }
}

/// The OID of the collation `default`.
const DEFAULT_COLLATION: u32 = 100;

/// `generate_collation_name`: the name of the collation, with its schema when the name alone does not find it.
fn collation_name(collation: u32, session: &dyn Session) -> Result<String> {
    let row = builtin::named_by_oid(Named::Collation, collation)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for collation {collation}")))?;
    if reg::visible(Named::Collation, row, session) {
        Ok(quote_identifier(row.name))
    } else {
        Ok(reg::qualified(reg::namespace_name(row.namespace, session), row.name))
    }
}

/// `get_opclass_name`: ` name` of the operator class, or nothing for the default class of the type.
fn opclass_name(class: u32, ty: u32, session: &dyn Session) -> Result<String> {
    let (name, namespace, method) = reg::method_row(Named::Opclass, class, session)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for opclass {class}")))?;
    if default_opclass(ty, method).is_some_and(|d| d.oid == class) {
        return Ok(String::new());
    }
    if reg::method_visible(Named::Opclass, class, session) {
        Ok(format!(" {}", quote_identifier(&name)))
    } else {
        Ok(format!(" {}", reg::qualified(reg::namespace_name(namespace, session), &name)))
    }
}

/// `pg_get_indexdef_worker` for an index of the user. `colno` 0 gives the whole statement, and another number gives only the text of that column.
fn index_def(
    index: &Relation,
    (info, exclude): (&IndexInfo, Option<&[u32]>),
    colno: i32,
    paren: bool,
    session: &dyn Session,
) -> Result<String> {
    let catalog = session.catalog().ok_or_else(|| Error::internal("no catalog"))?;
    let rel = catalog.relation(info.table).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for relation {}", info.table))
    })?;
    let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
    let exprs = match &info.exprs {
        Some(text) => node::read_list(text)?,
        None => Vec::new(),
    };
    let mut exprs = exprs.iter();
    let whole = colno == 0;
    let mut out = String::new();
    if whole {
        let method = match info.method {
            BTREE => "btree",
            HASH => "hash",
            GIST => "gist",
            GIN => "gin",
            SPGIST => "spgist",
            BRIN => "brin",
            other => {
                return Err(Error::internal(format!(
                    "cache lookup failed for access method {other}"
                )));
            }
        };
        if exclude.is_some() {
            out.push_str(&format!("EXCLUDE USING {method} ("));
        }
        let table = if paren {
            relation_name(info.table, session)?
        } else {
            qualified_relation_name(info.table, session)?
        };
        if exclude.is_none() {
            out.push_str(&format!(
                "CREATE {}INDEX {} ON {}{table} USING {method} (",
                if info.unique { "UNIQUE " } else { "" },
                quote_identifier(&index.name),
                if index.kind == RelKind::PartitionedIndex { "ONLY " } else { "" }
            ));
        }
    }
    let key_count = usize::try_from(info.key_count).unwrap_or(0);
    for (keyno, &attnum) in info.keys.iter().enumerate() {
        let shown = whole || usize::try_from(colno).is_ok_and(|c| c == keyno + 1);
        if whole {
            if keyno == key_count {
                out.push_str(") INCLUDE (");
            } else if keyno > 0 {
                out.push_str(", ");
            }
        }
        let (ty, collation) = if attnum != 0 {
            let column = rel.column(attnum).ok_or_else(|| {
                Error::internal(format!("cache lookup failed for attribute {attnum}"))
            })?;
            if shown {
                out.push_str(&quote_identifier(&column.name));
            }
            (column.ty, column.collation)
        } else {
            let e =
                exprs.next().ok_or_else(|| Error::internal("too few entries in indexprs list"))?;
            if shown {
                let mut d = Deparser::for_columns(session, &rel.name, columns.clone(), paren);
                d.expr(e, false)?;
                if looks_like_function(e) {
                    out.push_str(&d.buf);
                } else {
                    out.push_str(&format!("({})", d.buf));
                }
            }
            (e.ty, expr_collation(e, rel))
        };
        if whole && keyno < key_count {
            let collation_used = info.collations.get(keyno).copied().unwrap_or(0);
            if collation_used != 0 && collation_used != collation {
                out.push_str(&format!(" COLLATE {}", collation_name(collation_used, session)?));
            }
            if let Some(&class) = info.classes.get(keyno) {
                out.push_str(&opclass_name(class, ty, session)?);
            }
            if info.method == BTREE {
                let option = info.options.get(keyno).copied().unwrap_or(0);
                if option & DESC != 0 {
                    out.push_str(" DESC");
                    if option & NULLS_FIRST == 0 {
                        out.push_str(" NULLS LAST");
                    }
                } else if option & NULLS_FIRST != 0 {
                    out.push_str(" NULLS FIRST");
                }
            }
            if let Some(&op) = exclude.and_then(|ops| ops.get(keyno)) {
                let name = builtin::operator_by_oid(op).map(|o| o.name).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for operator {op}"))
                })?;
                out.push_str(&format!(" WITH {name}"));
            }
        }
    }
    if whole {
        out.push(')');
        if info.nulls_not_distinct {
            out.push_str(" NULLS NOT DISTINCT");
        }
        if !index.options.is_empty() {
            out.push_str(" WITH (");
            get_reloptions(&mut out, &index.options, session);
            out.push(')');
        }
        if let Some(text) = &info.predicate {
            let predicate = deparse(text, &rel.name, &columns, paren, session)?;
            if exclude.is_some() {
                out.push_str(&format!(" WHERE ({predicate})"));
            } else {
                out.push_str(&format!(" WHERE {predicate}"));
            }
        }
    }
    Ok(out)
}

/// `get_reloptions`: the options as `name=value`, with the value in quotes unless it is an identifier that needs no quotes.
fn get_reloptions(out: &mut String, options: &[String], session: &dyn Session) {
    for (i, option) in options.iter().enumerate() {
        let (name, value) = option.split_once('=').unwrap_or((option, ""));
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_identifier(name));
        out.push('=');
        if quote_identifier(value) == value {
            out.push_str(value);
        } else {
            function::simple_quote_literal(out, value, session);
        }
    }
}

/// `looks_like_function`: true for an index expression that the syntax of an index column takes without parentheses.
fn looks_like_function(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Func(f) => matches!(f.form, FuncForm::Call | FuncForm::SqlSyntax),
        ExprKind::NullIf { .. }
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::SqlValue(_)
        | ExprKind::Xml(_) => true,
        _ => false,
    }
}

/// `pg_get_statisticsobj_worker`: the statement of a statistics object, or with `columns_only` only its columns and expressions. The statement has the schema of the object always, and the kinds only when the object does not have all of them. An expression that is not a call of a function is in parentheses. An OID that is not a statistics object gives `None`.
fn statisticsobj_def(
    oid: u32,
    columns_only: bool,
    session: &dyn Session,
) -> Result<Option<String>> {
    let catalog = session.catalog().ok_or_else(|| Error::internal("no catalog"))?;
    let Some(stat) = catalog.stat_ext(oid) else { return Ok(None) };
    let rel = catalog.relation(stat.relation).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for relation {}", stat.relation))
    })?;
    let exprs = match &stat.exprs {
        Some(text) => node::read_list(text)?,
        None => Vec::new(),
    };
    let mut out = String::new();
    if !columns_only {
        let schema = if reg::is_own_temp(stat.namespace, session) {
            Some("pg_temp")
        } else {
            reg::namespace_name(stat.namespace, session)
        };
        out.push_str(&format!("CREATE STATISTICS {}", reg::qualified(schema, &stat.name)));
        let kinds: Vec<&str> = [(b'd', "ndistinct"), (b'f', "dependencies"), (b'm', "mcv")]
            .into_iter()
            .filter(|(code, _)| stat.kinds.contains(code))
            .map(|(_, word)| word)
            .collect();
        if kinds.len() < 3 && stat.keys.len() + exprs.len() > 1 {
            out.push_str(&format!(" ({})", kinds.join(", ")));
        }
        out.push_str(" ON ");
    }
    let mut items = Vec::new();
    for &attnum in &stat.keys {
        let column = rel.column(attnum).ok_or_else(|| {
            Error::internal(format!(
                "cache lookup failed for attribute {attnum} of relation {}",
                stat.relation
            ))
        })?;
        items.push(quote_identifier(&column.name));
    }
    let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
    for e in &exprs {
        let mut d = Deparser::for_columns(session, &rel.name, columns.clone(), true);
        d.flat = true;
        d.expr(e, false)?;
        items.push(if looks_like_function(e) { d.buf } else { format!("({})", d.buf) });
    }
    out.push_str(&items.join(", "));
    if !columns_only {
        out.push_str(&format!(" FROM {}", relation_name(stat.relation, session)?));
    }
    Ok(Some(out))
}

/// `pg_get_statisticsobjdef`: the statement of a statistics object, or null for an OID that is not one.
fn get_statisticsobjdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    Ok(statisticsobj_def(oid, false, call.session)?.map_or(Value::Null, Value::text))
}

/// `pg_get_statisticsobjdef_columns`: the columns and the expressions of a statistics object.
fn get_statisticsobjdef_columns(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    Ok(statisticsobj_def(oid, true, call.session)?.map_or(Value::Null, Value::text))
}

/// `pg_get_statisticsobjdef_expressions`: the expressions of a statistics object as a `text[]`, each with `PRETTYFLAG_INDENT` only, or null for an object with no expressions.
fn get_statisticsobjdef_expressions(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let catalog = call.session.catalog().ok_or_else(|| Error::internal("no catalog"))?;
    let Some((stat, Some(text))) = catalog.stat_ext(oid).map(|s| (s, s.exprs.as_ref())) else {
        return Ok(Value::Null);
    };
    let rel = catalog.relation(stat.relation).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for relation {}", stat.relation))
    })?;
    let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
    let mut items = Vec::new();
    for e in node::read_list(text)? {
        let mut d = Deparser::for_columns(call.session, &rel.name, columns.clone(), false);
        d.expr(&e, false)?;
        items.push(Some(Value::text(d.buf)));
    }
    Ok(Value::Array(Box::new(rupg_types::Array::one(items))))
}

/// `pg_get_partkeydef_worker`: the partition key of a partitioned table, as `PARTITION BY` writes it.
fn partkey_def(
    rel: &Relation,
    key: &PartitionKey,
    attrs_only: bool,
    session: &dyn Session,
) -> Result<String> {
    let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
    let exprs = match &key.exprs {
        Some(text) => node::read_list(text)?,
        None => Vec::new(),
    };
    let mut exprs = exprs.iter();
    let strategy = match key.strategy {
        PartStrategy::Hash => "HASH",
        PartStrategy::List => "LIST",
        PartStrategy::Range => "RANGE",
    };
    let mut out = if attrs_only { String::new() } else { format!("{strategy} (") };
    for (keyno, &attnum) in key.attrs.iter().enumerate() {
        if keyno > 0 {
            out.push_str(", ");
        }
        let (ty, collation) = if attnum != 0 {
            let column = rel.column(attnum).ok_or_else(|| {
                Error::internal(format!("cache lookup failed for attribute {attnum}"))
            })?;
            out.push_str(&quote_identifier(&column.name));
            (column.ty, column.collation)
        } else {
            let e =
                exprs.next().ok_or_else(|| Error::internal("too few entries in partexprs list"))?;
            let mut d = Deparser::for_columns(session, &rel.name, columns.clone(), attrs_only);
            d.expr(e, false)?;
            if looks_like_function(e) {
                out.push_str(&d.buf);
            } else {
                out.push_str(&format!("({})", d.buf));
            }
            (e.ty, expr_collation(e, rel))
        };
        if attrs_only {
            continue;
        }
        let collation_used = key.collations.get(keyno).copied().unwrap_or(0);
        if collation_used != 0 && collation_used != collation {
            out.push_str(&format!(" COLLATE {}", collation_name(collation_used, session)?));
        }
        if let Some(&class) = key.classes.get(keyno) {
            out.push_str(&opclass_name(class, ty, session)?);
        }
    }
    if !attrs_only {
        out.push(')');
    }
    Ok(out)
}

/// `pg_get_partkeydef_columns(oid, true)`: the columns and the expressions of the partition key of a partitioned table, with no strategy, collations or operator classes, as the detail of an error of tuple routing shows them.
///
/// # Errors
///
/// The error of the deparse of an expression of the key.
pub fn partition_key_columns(rel: &Relation, session: &dyn Session) -> Result<String> {
    match &rel.partition_key {
        Some(key) => partkey_def(rel, key, true, session),
        None => Err(Error::internal(format!("relation {} is not partitioned", rel.oid))),
    }
}

/// `pg_get_indexdef_columns(oid, true)`: the key columns and the expressions of the key of an index, as the detail of an error of a duplicate key shows them.
///
/// # Errors
///
/// The error of the deparse of an expression of the key.
pub fn index_key_columns(index: u32, session: &dyn Session) -> Result<String> {
    let catalog = session.catalog().ok_or_else(|| Error::internal("no catalog"))?;
    let found = catalog.relation(index).and_then(|r| Some((r, r.index.as_ref()?)));
    let Some((rel, info)) = found else {
        return Err(Error::internal(format!("cache lookup failed for index {index}")));
    };
    let mut out = String::new();
    for colno in 1..=info.key_count {
        if colno > 1 {
            out.push_str(", ");
        }
        out.push_str(&index_def(rel, (info, None), i32::from(colno), true, session)?);
    }
    Ok(out)
}

/// `pg_get_partkeydef(oid)`: the partition key of a partitioned table, or null for another relation.
fn get_partkeydef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let found = call
        .session
        .catalog()
        .and_then(|c| c.relation(oid))
        .and_then(|r| Some((r, r.partition_key.as_ref()?)));
    match found {
        Some((rel, key)) => Ok(Value::text(partkey_def(rel, key, false, call.session)?)),
        None => Ok(Value::Null),
    }
}

/// `pg_get_partition_constraintdef(oid)`: the partition constraint of a partition, as `generate_partition_qual` makes it and `deparse_expression_pretty` shows it with the columns of the partition. A table that is not a partition gives null.
fn get_partition_constraintdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let Some(catalog) = call.session.catalog() else { return Ok(Value::Null) };
    let Some(rel) = catalog.relation(oid) else { return Ok(Value::Null) };
    let mut qual = partqual::partition_qual(catalog, rel)?;
    let expr = match qual.len() {
        0 => return Ok(Value::Null),
        1 => qual.remove(0),
        _ => Expr {
            kind: ExprKind::Bool(BoolOp::And, qual),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        },
    };
    let columns = rel.columns.iter().map(|c| c.name.clone()).collect();
    let mut d = Deparser::for_columns(call.session, &rel.name, columns, false);
    d.expr(&expr, false)?;
    Ok(Value::text(d.buf))
}

/// `pg_get_indexdef(oid)` and `pg_get_indexdef(oid, int4, bool)`.
fn get_indexdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let colno = match args.get(1) {
        Some(value) => {
            i32::try_from(value.as_i64().ok_or_else(bad_value)?).map_err(|_| bad_value())?
        }
        None => 0,
    };
    let paren = pretty_arg(args, 2)?;
    let found = call
        .session
        .catalog()
        .and_then(|c| c.relation(oid))
        .and_then(|r| Some((r, r.index.as_ref()?)));
    match found {
        Some((index, info)) => {
            Ok(Value::text(index_def(index, (info, None), colno, paren, call.session)?))
        }
        None if oid < FIRST_USER_OID => {
            Ok(builtin_index_def(oid, colno, paren, call.session)?.map_or(Value::Null, Value::text))
        }
        None => Ok(Value::Null),
    }
}

/// The kernels of this module, by the `prosrc` of the function.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_get_expr" | "pg_get_expr_ext" => get_expr,
        "pg_get_constraintdef" | "pg_get_constraintdef_ext" => get_constraintdef,
        "pg_get_indexdef" | "pg_get_indexdef_ext" => get_indexdef,
        "pg_get_statisticsobjdef" => get_statisticsobjdef,
        "pg_get_statisticsobjdef_columns" => get_statisticsobjdef_columns,
        "pg_get_statisticsobjdef_expressions" => get_statisticsobjdef_expressions,
        "pg_get_function_arguments" => function::get_function_arguments,
        "pg_get_function_identity_arguments" => function::get_function_identity_arguments,
        "pg_get_function_result" => function::get_function_result,
        "pg_get_function_arg_default" => function::get_function_arg_default,
        "pg_get_function_sqlbody" => function::get_function_sqlbody,
        "pg_get_functiondef" => function::get_functiondef,
        "pg_get_viewdef" | "pg_get_viewdef_ext" | "pg_get_viewdef_wrap" => query::get_viewdef,
        "pg_get_viewdef_name" | "pg_get_viewdef_name_ext" => query::get_viewdef_name,
        "pg_get_ruledef" | "pg_get_ruledef_ext" => query::get_ruledef,
        "pg_get_triggerdef" | "pg_get_triggerdef_ext" => query::get_triggerdef,
        "pg_get_partkeydef" => get_partkeydef,
        "pg_get_partition_constraintdef" => get_partition_constraintdef,
        _ => return None,
    })
}

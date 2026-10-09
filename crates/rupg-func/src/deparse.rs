//! The deparse functions `pg_get_expr`, `pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef` and the `pg_get_function_*` functions: a port of the parts of `ruleutils.c` that show the expressions, the queries and the definitions of the catalog.
//!
//! The functions read the stored form that `rupg_analyze::node` writes, or the query that the analyzer makes from the text of a view, and show it as PostgreSQL shows its node trees. The rules for parentheses, casts and constants follow `get_rule_expr`. The functions show the objects of the user and the system views. An index or a constraint of a system catalog gives an error that says that the engine cannot show it yet.

mod function;
mod query;

use std::ptr;

use rupg_analyze::{
    Aggref, BoolOp, BoolTest, Case, CastForm, Expr, ExprKind, Func, FuncForm, SortGroup, SqlValue,
    SubLink, SubLinkKind, Var,
};
use rupg_analyze::{default_opclass, node, sort_operators};
use rupg_catalog::{ConKind, Constraint, IndexInfo, Relation};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_types::{Value, oid};

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
/// `INDOPTION_DESC`.
const DESC: i16 = 1;
/// `INDOPTION_NULLS_FIRST`.
const NULLS_FIRST: i16 = 2;
/// The first OID that a user object can have.
const FIRST_USER_OID: u32 = 16384;

/// The names of the relations of one query and of their columns, as `deparse_namespace` with its `deparse_columns`.
#[derive(Debug, Default)]
struct Namespace {
    /// The name of each relation of the query, by its index, as `rtable_names`. The name is unique in the query and in the queries outside it.
    names: Vec<String>,
    /// The names of the columns of each relation, by the column number from 1.
    columns: Vec<Vec<String>>,
    /// For each relation, true when `FROM` shows the names of the columns after the name of the relation, as `printaliases`.
    print_aliases: Vec<bool>,
    /// The value and the name of each column of `USING` of a join with no alias that is not a column of one side. PostgreSQL shows such a column by its name, as a column of the join.
    merged: Vec<(Expr, String)>,
}

/// A deparse of an expression or of a query, as `deparse_context`. Each form of the functions sets `PRETTYFLAG_INDENT`, so a `CASE` always takes more than one line.
struct Deparser<'a> {
    session: &'a dyn Session,
    /// The names of the queries that have the expression, the outermost query first. A `Var` finds its relation in the namespace of its level.
    namespaces: Vec<Namespace>,
    /// `PRETTYFLAG_PAREN`: add parentheses only where the syntax needs them.
    paren: bool,
    /// `indentLevel`.
    indent: i32,
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
    buf: String,
}

impl<'a> Deparser<'a> {
    /// A deparse of an expression of one relation, as `deparse_context_for`: a `Var` shows the name of its column with no name of the relation.
    fn for_columns(session: &'a dyn Session, columns: Vec<String>, paren: bool) -> Deparser<'a> {
        let mut d = Deparser::new(session, paren, 0);
        d.namespaces.push(Namespace {
            names: vec![String::new()],
            columns: vec![columns],
            print_aliases: vec![false],
            merged: Vec::new(),
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
            wrap,
            prefix: false,
            names_visible: true,
            in_group_by: false,
            outputs: Vec::new(),
            buf: String::new(),
        }
    }

    /// `appendContextKeyword`: a new line with the indent and `plus` more spaces, then the keyword.
    fn keyword(&mut self, word: &str, before: i32, after: i32, plus: i32) {
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

    /// `get_variable`: the column with the name of its relation when the query needs it, and the name of the column. A column of `ORDER BY` also shows the name of its relation when a column of the result has the same name and shows another value.
    fn variable(&mut self, var: &Var, in_order_by: bool) -> Result<String> {
        let bogus = || Error::internal(format!("bogus varlevelsup: {}", var.levels_up));
        let depth = self.namespaces.len().checked_sub(var.levels_up + 1).ok_or_else(bogus)?;
        let space = &self.namespaces[depth];
        let refname = space.names.get(var.relation).cloned().unwrap_or_default();
        let attname = match usize::try_from(var.attnum) {
            Ok(n) if n > 0 => space.columns.get(var.relation).and_then(|c| c.get(n - 1)).cloned(),
            _ => system_column(var.attnum).map(str::to_string),
        };
        let attname =
            attname.ok_or_else(|| Error::internal(format!("invalid attnum {}", var.attnum)))?;
        let mut prefix = self.prefix;
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
        let refname = self.namespaces[depth].names.get(var.relation).cloned().unwrap_or_default();
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

    /// `get_rule_expr`. `implicit` is `showimplicit`: true to show the casts that the analyzer added.
    fn expr(&mut self, e: &Expr, implicit: bool) -> Result<()> {
        if let Some(name) = self.merged_name(e) {
            self.buf.push_str(&quote_identifier(&name));
            return Ok(());
        }
        match &e.kind {
            ExprKind::Const(value) => self.constant(e, value, 0)?,
            ExprKind::Param(n) => self.buf.push_str(&format!("${n}")),
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
                let name = field_name(arg.ty, *field, self.session)?;
                self.buf.push('.');
                self.buf.push_str(&quote_identifier(&name));
            }
            ExprKind::Func(f) => self.func(e, f, implicit)?,
            ExprKind::Relabel(arg, form) => self.cast_node(e, arg, *form, e.typmod, implicit)?,
            ExprKind::CoerceViaIo(arg, form) => self.cast_node(e, arg, *form, -1, implicit)?,
            ExprKind::ArrayCoerce { arg, form, .. } => {
                self.cast_node(e, arg, *form, e.typmod, implicit)?;
            }
            // An implicit cast to a domain shows its argument with no parentheses.
            ExprKind::CoerceToDomain(arg, CastForm::Implicit) if !implicit => {
                self.expr(arg, false)?;
            }
            ExprKind::CoerceToDomain(arg, _) => self.coercion(arg, e.ty, e.typmod, e)?,
            ExprKind::DomainValue => self.buf.push_str("VALUE"),
            ExprKind::Subscript(sub) => {
                // The array needs parentheses unless it is a column, also when it is another subscript.
                let parens = !matches!(sub.container.kind, ExprKind::Var(_));
                if parens {
                    self.buf.push('(');
                }
                self.expr(&sub.container, implicit)?;
                if parens {
                    self.buf.push(')');
                }
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
            ExprKind::SubLink(sub) => self.sublink(sub)?,
            ExprKind::SubColumn(_) => {
                return Err(Error::internal("a column of a subquery outside its test"));
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
            self.keyword("WHEN ", 0, 0, 0);
            self.expr(shown, false)?;
            self.buf.push_str(" THEN ");
            self.expr(result, true)?;
        }
        self.keyword("ELSE ", 0, 0, 0);
        self.expr(&case.default, true)?;
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
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::NullIf { .. }
        | ExprKind::Agg(_)
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
        | ExprKind::Subscript(_)
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::SqlValue(_)
        | ExprKind::NullIf { .. }
        | ExprKind::Agg(_)
        | ExprKind::Case(_) => true,
        ExprKind::Func(f) if !matches!(f.form, FuncForm::Operator(_)) => true,
        ExprKind::Relabel(arg, _)
        | ExprKind::CoerceViaIo(arg, _)
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
        ExprKind::SubLink(_) | ExprKind::NullTest(..) | ExprKind::BooleanTest(..) => {
            own_syntax(parent)
        }
        ExprKind::Distinct { not: false, .. } => own_syntax(parent),
        ExprKind::Distinct { not: true, .. } => bool_simple(BoolOp::Not, parent, paren),
        ExprKind::Bool(op, _) => bool_simple(*op, parent, paren),
        ExprKind::ScalarArrayOp { .. } | ExprKind::CaseTest => false,
    }
}

/// True when a `Var` is in the expression.
fn has_var(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Var(_)) || e.children().into_iter().any(has_var)
}

/// `get_name_for_var_field` for a value of a composite type: the name of the field with the index from 0.
fn field_name(ty: u32, field: usize, session: &dyn Session) -> Result<String> {
    let relid = match builtin::type_by_oid(ty) {
        Some(row) => row.relid,
        None => session
            .catalog()
            .and_then(|c| c.relations().find(|r| r.row_type == ty))
            .map_or(0, |r| r.oid),
    };
    column_names(relid, session).and_then(|names| names.get(field).cloned()).ok_or_else(|| {
        Error::internal(format!("could not identify column {} in a record", field + 1))
    })
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
fn deparse(text: &str, columns: &[String], paren: bool, session: &dyn Session) -> Result<String> {
    let exprs = stored(text)?;
    let mut d = Deparser::for_columns(session, columns.to_vec(), paren);
    d.list(&exprs, false)?;
    Ok(d.buf)
}

/// `deparse_expression` of a stored expression that has no column, such as the default of a domain in `typdefault`.
pub fn deparse_expression(text: &str, session: &dyn Session) -> Result<String> {
    deparse(text, &[], false, session)
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

/// `pg_get_expr(pg_node_tree, oid [, bool])`.
fn get_expr(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let relid = args.get(1).and_then(Value::as_oid).ok_or_else(bad_value)?;
    let paren = pretty_arg(args, 2)?;
    let session = call.session;
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
    Ok(Value::text(deparse(text, &columns, paren, session)?))
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
            out.push_str(&format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                column_list(relation()?, &con.keys)?,
                relation_name(fk.table, session)?,
                column_list(target, &fk.keys)?
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
            out.push_str(&format!("({})", column_list(rel, &con.keys)?));
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
            let columns: Vec<String> = if con.domain == 0 {
                relation()?.columns.iter().map(|c| c.name.clone()).collect()
            } else {
                Vec::new()
            };
            out.push_str(&format!("CHECK ({})", deparse(text, &columns, paren, session)?));
            if con.no_inherit {
                out.push_str(" NO INHERIT");
            }
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
    if !con.validated {
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
        None if oid < FIRST_USER_OID && builtin_row("pg_constraint", oid) => {
            Err(system_object("pg_get_constraintdef"))
        }
        None => Ok(Value::Null),
    }
}

/// True when the static rows of the catalog have a row with this OID.
fn builtin_row(catalog: &str, oid: u32) -> bool {
    let Some(cat) = rupg_pgcatalog::catalog(catalog) else { return false };
    let column = if catalog == "pg_index" { "indexrelid" } else { "oid" };
    let Some(at) = cat.column(column) else { return false };
    matches!(&cat.rows.get(at).map(|b| &b.values), Some(rupg_pgcatalog::Values::Oid(v)) if v.contains(&oid))
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
    let row = builtin::named_by_oid(Named::Opclass, class)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for opclass {class}")))?;
    if default_opclass(ty, row.method).is_some_and(|d| d.oid == class) {
        return Ok(String::new());
    }
    if reg::visible(Named::Opclass, row, session) {
        Ok(format!(" {}", quote_identifier(row.name)))
    } else {
        Ok(format!(" {}", reg::qualified(reg::namespace_name(row.namespace, session), row.name)))
    }
}

/// `pg_get_indexdef_worker` for an index of the user. `colno` 0 gives the whole statement, and another number gives only the text of that column.
fn index_def(
    index: &Relation,
    info: &IndexInfo,
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
            other => {
                return Err(Error::internal(format!(
                    "cache lookup failed for access method {other}"
                )));
            }
        };
        let table = if paren {
            relation_name(info.table, session)?
        } else {
            qualified_relation_name(info.table, session)?
        };
        out.push_str(&format!(
            "CREATE {}INDEX {} ON {table} USING {method} (",
            if info.unique { "UNIQUE " } else { "" },
            quote_identifier(&index.name)
        ));
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
                let mut d = Deparser::for_columns(session, columns.clone(), paren);
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
        }
    }
    if whole {
        out.push(')');
        if info.nulls_not_distinct {
            out.push_str(" NULLS NOT DISTINCT");
        }
        if let Some(text) = &info.predicate {
            out.push_str(&format!(" WHERE {}", deparse(text, &columns, paren, session)?));
        }
    }
    Ok(out)
}

/// `looks_like_function`: true for an index expression that the syntax of an index column takes without parentheses.
fn looks_like_function(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Func(f) => matches!(f.form, FuncForm::Call | FuncForm::SqlSyntax),
        ExprKind::NullIf { .. }
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::SqlValue(_) => true,
        _ => false,
    }
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
        Some((index, info)) => Ok(Value::text(index_def(index, info, colno, paren, call.session)?)),
        None if oid < FIRST_USER_OID && builtin_row("pg_index", oid) => {
            Err(system_object("pg_get_indexdef"))
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
        "pg_get_function_arguments" => function::get_function_arguments,
        "pg_get_function_identity_arguments" => function::get_function_identity_arguments,
        "pg_get_function_result" => function::get_function_result,
        "pg_get_function_arg_default" => function::get_function_arg_default,
        "pg_get_viewdef" | "pg_get_viewdef_ext" | "pg_get_viewdef_wrap" => query::get_viewdef,
        "pg_get_viewdef_name" | "pg_get_viewdef_name_ext" => query::get_viewdef_name,
        _ => return None,
    })
}

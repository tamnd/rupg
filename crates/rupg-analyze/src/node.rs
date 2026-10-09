//! The stored form of an expression, which the catalog keeps for a column default, a check constraint and an index expression.
//!
//! PostgreSQL stores these as `pg_node_tree` text that `nodeToString` writes. rupg stores its own text, which document 05 section 5.2 permits, because clients do not read it. The form looks like the form of PostgreSQL: `{NAME :field value ...}`. A constant keeps its value in a form that does not change with the settings of the session, so the expression reads back to the same tree.

use rupg_common::{Error, Result};
use rupg_types::{Array, ArrayDim, Inet, Interval, NetFamily, Recv, Value};

use crate::expr::{
    BoolOp, BoolTest, Case, CastForm, Expr, ExprKind, Func, FuncForm, SqlValue, Subscript, Var,
};

/// The stored form of an expression.
///
/// # Errors
///
/// An internal error for an aggregate call, a subquery or a parameter, which a stored expression cannot have.
pub fn write(expr: &Expr) -> Result<String> {
    let mut out = String::new();
    write_expr(expr, &mut out)?;
    Ok(out)
}

/// The expression of a stored form that [`write()`] made.
///
/// # Errors
///
/// An internal error when the text is not a stored form.
pub fn read(text: &str) -> Result<Expr> {
    let mut tokens = Tokens { text, at: 0 };
    let item = tokens.item()?;
    if tokens.next()?.is_some() {
        return Err(bad("text after the end"));
    }
    expr_of(&item)
}

/// The stored form of a list of expressions, such as the expressions of an index: `(e1 e2 ...)`.
///
/// # Errors
///
/// As [`write()`].
pub fn write_list(exprs: &[Expr]) -> Result<String> {
    let mut out = String::from("(");
    for (i, expr) in exprs.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_expr(expr, &mut out)?;
    }
    out.push(')');
    Ok(out)
}

/// The expressions of a stored form that [`write_list`] made.
///
/// # Errors
///
/// An internal error when the text is not a stored list.
pub fn read_list(text: &str) -> Result<Vec<Expr>> {
    let mut tokens = Tokens { text, at: 0 };
    let item = tokens.item()?;
    if tokens.next()?.is_some() {
        return Err(bad("text after the end"));
    }
    list(&item)?.iter().map(expr_of).collect()
}

/// The error of a bad stored form.
fn bad(what: &str) -> Error {
    Error::internal(format!("bad stored expression: {what}"))
}

fn write_expr(expr: &Expr, out: &mut String) -> Result<()> {
    let name = match &expr.kind {
        ExprKind::Const(_) => "CONST",
        ExprKind::Func(_) => "FUNC",
        ExprKind::Relabel(..) => "RELABEL",
        ExprKind::CoerceViaIo(..) => "COERCEVIAIO",
        ExprKind::ArrayCoerce { .. } => "ARRAYCOERCE",
        ExprKind::Subscript(_) => "SUBSCRIPT",
        ExprKind::Bool(..) => "BOOL",
        ExprKind::NullTest(..) => "NULLTEST",
        ExprKind::BooleanTest(..) => "BOOLEANTEST",
        ExprKind::Case(_) => "CASE",
        ExprKind::CaseTest => "CASETEST",
        ExprKind::Coalesce(_) => "COALESCE",
        ExprKind::MinMax { .. } => "MINMAX",
        ExprKind::NullIf { .. } => "NULLIF",
        ExprKind::Distinct { .. } => "DISTINCT",
        ExprKind::ScalarArrayOp { .. } => "SCALARARRAYOP",
        ExprKind::Array { .. } => "ARRAY",
        ExprKind::SqlValue(_) => "SQLVALUE",
        ExprKind::Var(_) => "VAR",
        ExprKind::Param(_) | ExprKind::Agg(_) | ExprKind::SubLink(_) | ExprKind::SubColumn(_) => {
            return Err(Error::internal("a stored expression cannot have this node"));
        }
    };
    let location = expr.location.map_or(-1, |l| i64::try_from(l).unwrap_or(-1));
    out.push('{');
    out.push_str(name);
    out.push_str(&format!(" :type {} :typmod {} :location {location}", expr.ty, expr.typmod));
    match &expr.kind {
        ExprKind::Const(value) => {
            out.push_str(" :value ");
            write_value(value, out);
        }
        ExprKind::Func(f) => {
            out.push_str(&format!(" :oid {}", f.oid));
            match f.form {
                FuncForm::Call => out.push_str(" :form call"),
                FuncForm::Operator(op) => out.push_str(&format!(" :form (operator {op})")),
                FuncForm::ExplicitCast => out.push_str(" :form explicit"),
                FuncForm::ImplicitCast => out.push_str(" :form implicit"),
                FuncForm::SqlSyntax => out.push_str(" :form sql"),
            }
            out.push_str(&format!(" :variadic {}", f.variadic));
            write_args(&f.args, out)?;
        }
        ExprKind::Relabel(arg, form) | ExprKind::CoerceViaIo(arg, form) => {
            out.push_str(match form {
                CastForm::Explicit => " :form explicit",
                CastForm::Implicit => " :form implicit",
            });
            write_field("arg", arg, out)?;
        }
        ExprKind::ArrayCoerce { arg, element, form } => {
            out.push_str(match form {
                CastForm::Explicit => " :form explicit",
                CastForm::Implicit => " :form implicit",
            });
            write_field("arg", arg, out)?;
            write_field("element", element, out)?;
        }
        ExprKind::Subscript(sub) => {
            write_field("container", &sub.container, out)?;
            out.push_str(" :upper ");
            write_bounds(&sub.upper, out)?;
            out.push_str(" :lower ");
            match &sub.lower {
                Some(lower) => write_bounds(lower, out)?,
                None => out.push_str("<>"),
            }
        }
        ExprKind::Bool(op, args) => {
            let op = match op {
                BoolOp::And => "and",
                BoolOp::Or => "or",
                BoolOp::Not => "not",
            };
            out.push_str(&format!(" :op {op}"));
            write_args(args, out)?;
        }
        ExprKind::NullTest(arg, is_null) => {
            write_field("arg", arg, out)?;
            out.push_str(&format!(" :isnull {is_null}"));
        }
        ExprKind::BooleanTest(arg, test) => {
            write_field("arg", arg, out)?;
            out.push_str(" :test ");
            out.push_str(test_name(*test));
        }
        ExprKind::Case(case) => {
            out.push_str(" :arg ");
            match &case.arg {
                Some(arg) => write_expr(arg, out)?,
                None => out.push_str("<>"),
            }
            out.push_str(" :whens (");
            for (i, (when, then)) in case.whens.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push('(');
                write_expr(when, out)?;
                out.push(' ');
                write_expr(then, out)?;
                out.push(')');
            }
            out.push(')');
            write_field("default", &case.default, out)?;
        }
        ExprKind::CaseTest => {}
        ExprKind::Coalesce(args) => write_args(args, out)?,
        ExprKind::MinMax { greatest, less, args } => {
            out.push_str(&format!(" :greatest {greatest} :less {less}"));
            write_args(args, out)?;
        }
        ExprKind::NullIf { equal, args } => {
            out.push_str(&format!(" :equal {equal}"));
            write_args(args, out)?;
        }
        ExprKind::Distinct { equal, not, args } => {
            out.push_str(&format!(" :equal {equal} :not {not}"));
            write_args(args, out)?;
        }
        ExprKind::ScalarArrayOp { func, any, args } => {
            out.push_str(&format!(" :func {func} :any {any}"));
            write_args(args, out)?;
        }
        ExprKind::Array { element, multidims, elements } => {
            out.push_str(&format!(" :element {element} :multidims {multidims}"));
            write_args(elements, out)?;
        }
        ExprKind::SqlValue(value) => {
            let (name, precision) = sql_value_name(*value);
            out.push_str(&format!(" :function {name} :precision "));
            match precision {
                Some(p) => out.push_str(&p.to_string()),
                None => out.push_str("<>"),
            }
        }
        ExprKind::Var(var) => {
            out.push_str(&format!(
                " :relation {} :attnum {} :levelsup {}",
                var.relation, var.attnum, var.levels_up
            ));
        }
        ExprKind::Param(_) | ExprKind::Agg(_) | ExprKind::SubLink(_) | ExprKind::SubColumn(_) => {}
    }
    out.push('}');
    Ok(())
}

fn write_field(name: &str, expr: &Expr, out: &mut String) -> Result<()> {
    out.push_str(&format!(" :{name} "));
    write_expr(expr, out)
}

fn write_args(args: &[Expr], out: &mut String) -> Result<()> {
    out.push_str(" :args (");
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_expr(arg, out)?;
    }
    out.push(')');
    Ok(())
}

/// The subscripts of one bound of a `SUBSCRIPT`, with `<>` for a bound that the query does not give.
fn write_bounds(bounds: &[Option<Expr>], out: &mut String) -> Result<()> {
    out.push('(');
    for (i, bound) in bounds.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match bound {
            Some(bound) => write_expr(bound, out)?,
            None => out.push_str("<>"),
        }
    }
    out.push(')');
    Ok(())
}

/// The subscripts of one bound of a `SUBSCRIPT`.
fn bounds_of(item: &Item) -> Result<Vec<Option<Expr>>> {
    list(item)?
        .iter()
        .map(|item| match item {
            Item::Atom(empty) if empty == "<>" => Ok(None),
            item => expr_of(item).map(Some),
        })
        .collect()
}

const TESTS: [(BoolTest, &str); 6] = [
    (BoolTest::IsTrue, "is_true"),
    (BoolTest::IsNotTrue, "is_not_true"),
    (BoolTest::IsFalse, "is_false"),
    (BoolTest::IsNotFalse, "is_not_false"),
    (BoolTest::IsUnknown, "is_unknown"),
    (BoolTest::IsNotUnknown, "is_not_unknown"),
];

fn test_name(test: BoolTest) -> &'static str {
    TESTS.iter().find(|(t, _)| *t == test).map_or("is_true", |(_, name)| name)
}

fn sql_value_name(value: SqlValue) -> (&'static str, Option<i32>) {
    match value {
        SqlValue::CurrentDate => ("current_date", None),
        SqlValue::CurrentTime(p) => ("current_time", p),
        SqlValue::CurrentTimestamp(p) => ("current_timestamp", p),
        SqlValue::LocalTime(p) => ("localtime", p),
        SqlValue::LocalTimestamp(p) => ("localtimestamp", p),
        SqlValue::CurrentRole => ("current_role", None),
        SqlValue::CurrentUser => ("current_user", None),
        SqlValue::User => ("user", None),
        SqlValue::SessionUser => ("session_user", None),
        SqlValue::CurrentCatalog => ("current_catalog", None),
        SqlValue::CurrentSchema => ("current_schema", None),
    }
}

fn sql_value_of(name: &str, precision: Option<i32>) -> Option<SqlValue> {
    Some(match name {
        "current_date" => SqlValue::CurrentDate,
        "current_time" => SqlValue::CurrentTime(precision),
        "current_timestamp" => SqlValue::CurrentTimestamp(precision),
        "localtime" => SqlValue::LocalTime(precision),
        "localtimestamp" => SqlValue::LocalTimestamp(precision),
        "current_role" => SqlValue::CurrentRole,
        "current_user" => SqlValue::CurrentUser,
        "user" => SqlValue::User,
        "session_user" => SqlValue::SessionUser,
        "current_catalog" => SqlValue::CurrentCatalog,
        "current_schema" => SqlValue::CurrentSchema,
        _ => return None,
    })
}

/// Writes the bytes as `x` and two hex digits for each byte.
fn hex(bytes: &[u8], out: &mut String) {
    out.push('x');
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
}

/// A value as a list of its kind and its parts, or `null`. A float keeps its bits, and a `numeric` keeps the bytes of its binary form.
fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(v) => out.push_str(&format!("(bool {v})")),
        Value::Int2(v) => out.push_str(&format!("(int2 {v})")),
        Value::Int4(v) => out.push_str(&format!("(int4 {v})")),
        Value::Int8(v) => out.push_str(&format!("(int8 {v})")),
        Value::Float4(v) => out.push_str(&format!("(float4 {})", v.to_bits())),
        Value::Float8(v) => out.push_str(&format!("(float8 {})", v.to_bits())),
        Value::Numeric(v) => {
            let mut bytes = Vec::new();
            rupg_types::numeric_send(v, &mut bytes);
            out.push_str("(numeric ");
            hex(&bytes, out);
            out.push(')');
        }
        Value::Oid(v) => out.push_str(&format!("(oid {v})")),
        Value::Char(v) => out.push_str(&format!("(char {v})")),
        Value::Text(v) => {
            out.push_str("(text \"");
            for c in v.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push_str("\")");
        }
        Value::Bytea(v) => {
            out.push_str("(bytea ");
            hex(v, out);
            out.push(')');
        }
        Value::Uuid(v) => {
            out.push_str("(uuid ");
            hex(v, out);
            out.push(')');
        }
        Value::Date(v) => out.push_str(&format!("(date {v})")),
        Value::Time(v) => out.push_str(&format!("(time {v})")),
        Value::TimeTz(time, zone) => out.push_str(&format!("(timetz {time} {zone})")),
        Value::Timestamp(v) => out.push_str(&format!("(timestamp {v})")),
        Value::TimestampTz(v) => out.push_str(&format!("(timestamptz {v})")),
        Value::Interval(v) => {
            out.push_str(&format!("(interval {} {} {})", v.time, v.day, v.month));
        }
        Value::Inet(v) => {
            let family = if v.family == NetFamily::V4 { 4 } else { 6 };
            out.push_str(&format!("(inet {family} {} ", v.bits));
            hex(v.bytes(), out);
            out.push(')');
        }
        Value::Array(array) => {
            out.push_str("(array (");
            for (i, dim) in array.dims.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format!("({} {})", dim.len, dim.lower));
            }
            out.push_str(") (");
            for (i, element) in array.values.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                match element {
                    Some(element) => write_value(element, out),
                    None => out.push_str("null"),
                }
            }
            out.push_str("))");
        }
    }
}

/// A part of a stored form.
#[derive(Debug)]
enum Item {
    /// A word or a number. `<>` is the empty value.
    Atom(String),
    /// A quoted string.
    Str(String),
    List(Vec<Item>),
    /// `{NAME :field value ...}`.
    Node(String, Vec<(String, Item)>),
}

/// The tokens of a stored form.
struct Tokens<'a> {
    text: &'a str,
    at: usize,
}

/// One token.
enum Token {
    Open(char),
    Close(char),
    Str(String),
    Atom(String),
}

impl Tokens<'_> {
    fn next(&mut self) -> Result<Option<Token>> {
        let rest = &self.text[self.at..];
        let trimmed = rest.trim_start();
        self.at += rest.len() - trimmed.len();
        let Some(c) = trimmed.chars().next() else { return Ok(None) };
        match c {
            '{' | '(' => {
                self.at += 1;
                Ok(Some(Token::Open(c)))
            }
            '}' | ')' => {
                self.at += 1;
                Ok(Some(Token::Close(c)))
            }
            '"' => {
                let mut value = String::new();
                let mut chars = trimmed[1..].char_indices();
                loop {
                    let Some((i, c)) = chars.next() else { return Err(bad("an open string")) };
                    match c {
                        '"' => {
                            self.at += i + 2;
                            return Ok(Some(Token::Str(value)));
                        }
                        '\\' => match chars.next() {
                            Some((_, c)) => value.push(c),
                            None => return Err(bad("an open string")),
                        },
                        _ => value.push(c),
                    }
                }
            }
            _ => {
                let end = trimmed
                    .find(|c: char| c.is_whitespace() || "{}()\"".contains(c))
                    .unwrap_or(trimmed.len());
                self.at += end;
                Ok(Some(Token::Atom(trimmed[..end].to_string())))
            }
        }
    }

    fn item(&mut self) -> Result<Item> {
        match self.next()? {
            Some(token) => self.item_from(token),
            None => Err(bad("an early end")),
        }
    }

    fn item_from(&mut self, token: Token) -> Result<Item> {
        match token {
            Token::Atom(atom) => Ok(Item::Atom(atom)),
            Token::Str(text) => Ok(Item::Str(text)),
            Token::Close(_) => Err(bad("a close bracket with no open bracket")),
            Token::Open('(') => {
                let mut items = Vec::new();
                loop {
                    match self.next()? {
                        Some(Token::Close(')')) => return Ok(Item::List(items)),
                        Some(token) => items.push(self.item_from(token)?),
                        None => return Err(bad("an open list")),
                    }
                }
            }
            Token::Open(_) => {
                let Some(Token::Atom(name)) = self.next()? else {
                    return Err(bad("a node with no name"));
                };
                let mut fields = Vec::new();
                loop {
                    match self.next()? {
                        Some(Token::Close('}')) => return Ok(Item::Node(name, fields)),
                        Some(Token::Atom(field)) if field.starts_with(':') => {
                            fields.push((field[1..].to_string(), self.item()?));
                        }
                        _ => return Err(bad("a node field")),
                    }
                }
            }
        }
    }
}

/// The fields of one node.
struct Fields<'a> {
    fields: &'a [(String, Item)],
}

impl<'a> Fields<'a> {
    fn get(&self, name: &str) -> Result<&'a Item> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, item)| item)
            .ok_or_else(|| bad(&format!("no field {name}")))
    }

    fn atom(&self, name: &str) -> Result<&'a str> {
        atom(self.get(name)?)
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<T> {
        number(self.atom(name)?)
    }

    fn flag(&self, name: &str) -> Result<bool> {
        match self.atom(name)? {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(bad(&format!("field {name} is not true or false"))),
        }
    }

    fn expr(&self, name: &str) -> Result<Box<Expr>> {
        Ok(Box::new(expr_of(self.get(name)?)?))
    }

    fn exprs(&self, name: &str) -> Result<Vec<Expr>> {
        list(self.get(name)?)?.iter().map(expr_of).collect()
    }
}

fn atom(item: &Item) -> Result<&str> {
    match item {
        Item::Atom(atom) => Ok(atom),
        _ => Err(bad("a list where a word must be")),
    }
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T> {
    text.parse().map_err(|_| bad(&format!("{text} is not a number")))
}

fn list(item: &Item) -> Result<&[Item]> {
    match item {
        Item::List(items) => Ok(items),
        _ => Err(bad("a word where a list must be")),
    }
}

fn expr_of(item: &Item) -> Result<Expr> {
    let Item::Node(name, fields) = item else { return Err(bad("no node")) };
    let f = Fields { fields };
    let ty = f.number("type")?;
    let typmod = f.number("typmod")?;
    let location: i64 = f.number("location")?;
    let location = usize::try_from(location).ok();
    let kind = match name.as_str() {
        "CONST" => ExprKind::Const(value_of(f.get("value")?)?),
        "FUNC" => {
            let form = match f.get("form")? {
                Item::Atom(form) if form == "call" => FuncForm::Call,
                Item::Atom(form) if form == "explicit" => FuncForm::ExplicitCast,
                Item::Atom(form) if form == "implicit" => FuncForm::ImplicitCast,
                Item::Atom(form) if form == "sql" => FuncForm::SqlSyntax,
                Item::List(parts) if parts.len() == 2 && atom(&parts[0])? == "operator" => {
                    FuncForm::Operator(number(atom(&parts[1])?)?)
                }
                _ => return Err(bad("a function form")),
            };
            ExprKind::Func(Func {
                oid: f.number("oid")?,
                args: f.exprs("args")?,
                form,
                variadic: f.flag("variadic")?,
            })
        }
        "RELABEL" => ExprKind::Relabel(f.expr("arg")?, cast_form(&f)?),
        "COERCEVIAIO" => ExprKind::CoerceViaIo(f.expr("arg")?, cast_form(&f)?),
        "ARRAYCOERCE" => ExprKind::ArrayCoerce {
            arg: f.expr("arg")?,
            element: f.expr("element")?,
            form: cast_form(&f)?,
        },
        "SUBSCRIPT" => ExprKind::Subscript(Box::new(Subscript {
            container: *f.expr("container")?,
            upper: bounds_of(f.get("upper")?)?,
            lower: match f.get("lower")? {
                Item::Atom(empty) if empty == "<>" => None,
                item => Some(bounds_of(item)?),
            },
        })),
        "BOOL" => {
            let op = match f.atom("op")? {
                "and" => BoolOp::And,
                "or" => BoolOp::Or,
                "not" => BoolOp::Not,
                _ => return Err(bad("a boolean operator")),
            };
            ExprKind::Bool(op, f.exprs("args")?)
        }
        "NULLTEST" => ExprKind::NullTest(f.expr("arg")?, f.flag("isnull")?),
        "BOOLEANTEST" => {
            let test = f.atom("test")?;
            let Some((test, _)) = TESTS.iter().find(|(_, name)| *name == test) else {
                return Err(bad("a boolean test"));
            };
            ExprKind::BooleanTest(f.expr("arg")?, *test)
        }
        "CASE" => {
            let arg = match f.get("arg")? {
                Item::Atom(empty) if empty == "<>" => None,
                item => Some(Box::new(expr_of(item)?)),
            };
            let mut whens = Vec::new();
            for when in list(f.get("whens")?)? {
                let [when, then] = list(when)? else { return Err(bad("a WHEN of a CASE")) };
                whens.push((expr_of(when)?, expr_of(then)?));
            }
            ExprKind::Case(Case { arg, whens, default: f.expr("default")? })
        }
        "CASETEST" => ExprKind::CaseTest,
        "COALESCE" => ExprKind::Coalesce(f.exprs("args")?),
        "MINMAX" => ExprKind::MinMax {
            greatest: f.flag("greatest")?,
            less: f.number("less")?,
            args: f.exprs("args")?,
        },
        "NULLIF" => ExprKind::NullIf { equal: f.number("equal")?, args: f.exprs("args")? },
        "DISTINCT" => ExprKind::Distinct {
            equal: f.number("equal")?,
            not: f.flag("not")?,
            args: f.exprs("args")?,
        },
        "SCALARARRAYOP" => ExprKind::ScalarArrayOp {
            func: f.number("func")?,
            any: f.flag("any")?,
            args: f.exprs("args")?,
        },
        "ARRAY" => ExprKind::Array {
            element: f.number("element")?,
            multidims: f.flag("multidims")?,
            elements: f.exprs("args")?,
        },
        "SQLVALUE" => {
            let precision = match f.atom("precision")? {
                "<>" => None,
                p => Some(number(p)?),
            };
            let value = sql_value_of(f.atom("function")?, precision)
                .ok_or_else(|| bad("a SQL value function"))?;
            ExprKind::SqlValue(value)
        }
        "VAR" => ExprKind::Var(Var {
            relation: f.number("relation")?,
            attnum: f.number("attnum")?,
            levels_up: f.number("levelsup")?,
        }),
        _ => return Err(bad(&format!("the node {name}"))),
    };
    Ok(Expr { kind, ty, typmod, location })
}

/// The form of a relabel or of an I/O cast.
fn cast_form(f: &Fields<'_>) -> Result<CastForm> {
    match f.atom("form")? {
        "explicit" => Ok(CastForm::Explicit),
        "implicit" => Ok(CastForm::Implicit),
        _ => Err(bad("a cast form")),
    }
}

/// The bytes of `x` and hex digits.
fn bytes_of(text: &str) -> Result<Vec<u8>> {
    let digits = text.strip_prefix('x').ok_or_else(|| bad("bytes with no x"))?;
    if digits.len() % 2 != 0 {
        return Err(bad("an odd number of hex digits"));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).map_err(|_| bad("a hex digit")))
        .collect()
}

fn value_of(item: &Item) -> Result<Value> {
    let parts = match item {
        Item::Atom(null) if null == "null" => return Ok(Value::Null),
        item => list(item)?,
    };
    let Some((kind, parts)) = parts.split_first() else { return Err(bad("an empty value")) };
    let part = |i: usize| -> Result<&str> {
        parts.get(i).ok_or_else(|| bad("a value with too few parts")).and_then(atom)
    };
    let value = match atom(kind)? {
        "bool" => Value::Bool(part(0)? == "true"),
        "int2" => Value::Int2(number(part(0)?)?),
        "int4" => Value::Int4(number(part(0)?)?),
        "int8" => Value::Int8(number(part(0)?)?),
        "float4" => Value::Float4(f32::from_bits(number(part(0)?)?)),
        "float8" => Value::Float8(f64::from_bits(number(part(0)?)?)),
        "numeric" => {
            let bytes = bytes_of(part(0)?)?;
            let mut recv = Recv::new(&bytes);
            let value = rupg_types::numeric_recv(&mut recv, -1)
                .map_err(|_| bad("the bytes of a numeric"))?;
            Value::Numeric(value)
        }
        "oid" => Value::Oid(number(part(0)?)?),
        "char" => Value::Char(number(part(0)?)?),
        "text" => match parts.first() {
            Some(Item::Str(text)) => Value::Text(text.clone()),
            _ => return Err(bad("a text value with no string")),
        },
        "bytea" => Value::Bytea(bytes_of(part(0)?)?),
        "uuid" => {
            let bytes: [u8; 16] =
                bytes_of(part(0)?)?.try_into().map_err(|_| bad("a uuid that is not 16 bytes"))?;
            Value::Uuid(bytes)
        }
        "date" => Value::Date(number(part(0)?)?),
        "time" => Value::Time(number(part(0)?)?),
        "timetz" => Value::TimeTz(number(part(0)?)?, number(part(1)?)?),
        "timestamp" => Value::Timestamp(number(part(0)?)?),
        "timestamptz" => Value::TimestampTz(number(part(0)?)?),
        "inet" => {
            let family = match part(0)? {
                "4" => NetFamily::V4,
                "6" => NetFamily::V6,
                _ => return Err(bad("an inet family that is not 4 or 6")),
            };
            let bytes = bytes_of(part(2)?)?;
            if bytes.len() != family.size() {
                return Err(bad("an inet address of the wrong size"));
            }
            let mut addr = [0u8; 16];
            addr[..bytes.len()].copy_from_slice(&bytes);
            Value::Inet(Inet { family, bits: number(part(1)?)?, addr })
        }
        "interval" => Value::Interval(Interval {
            time: number(part(0)?)?,
            day: number(part(1)?)?,
            month: number(part(2)?)?,
        }),
        "array" => {
            let [dims, values] = parts else { return Err(bad("an array value")) };
            let mut array = Array::empty();
            for dim in list(dims)? {
                let [len, lower] = list(dim)? else { return Err(bad("an array dimension")) };
                array
                    .dims
                    .push(ArrayDim { len: number(atom(len)?)?, lower: number(atom(lower)?)? });
            }
            for value in list(values)? {
                array.values.push(match value_of(value)? {
                    Value::Null => None,
                    value => Some(value),
                });
            }
            Value::Array(Box::new(array))
        }
        kind => return Err(bad(&format!("the value kind {kind}"))),
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use rupg_types::oid;

    use super::*;

    fn round_trip(expr: &Expr) -> String {
        let text = write(expr).expect("write");
        assert_eq!(&read(&text).expect("read"), expr, "{text}");
        text
    }

    fn constant(value: Value, ty: u32) -> Expr {
        Expr::constant(value, ty, -1, Some(3))
    }

    #[test]
    fn values() {
        let numeric = rupg_types::numeric_in("-12.3400", -1).expect("numeric");
        let values = [
            (Value::Null, oid::INT4),
            (Value::Bool(true), oid::BOOL),
            (Value::Int2(-7), oid::INT2),
            (Value::Int4(i32::MIN), oid::INT4),
            (Value::Int8(i64::MAX), oid::INT8),
            (Value::Float4(-0.0), oid::FLOAT4),
            (Value::Float8(0.1), oid::FLOAT8),
            (Value::Float8(f64::INFINITY), oid::FLOAT8),
            (Value::Numeric(numeric), oid::NUMERIC),
            (Value::Oid(u32::MAX), oid::OID),
            (Value::Char(0), oid::CHAR),
            (Value::text("a \"b\" \\c\n{d} (e) é"), oid::TEXT),
            (Value::text(""), oid::TEXT),
            (Value::Bytea(vec![]), oid::BYTEA),
            (Value::Bytea(vec![0, 255, 16]), oid::BYTEA),
            (Value::Uuid([7; 16]), oid::UUID),
            (Value::Date(-1), oid::DATE),
            (Value::Time(86_400_000_000), oid::TIME),
            (Value::TimeTz(1, -3600), oid::TIMETZ),
            (Value::Timestamp(i64::MIN), oid::TIMESTAMP),
            (Value::TimestampTz(5), oid::TIMESTAMPTZ),
            (Value::Interval(Interval { time: -1, day: 2, month: -3 }), oid::INTERVAL),
            (Value::Inet(rupg_types::inet_in("10.1.2.3/8", false).expect("inet")), oid::INET),
            (Value::Inet(rupg_types::inet_in("2001:db8::/32", true).expect("cidr")), oid::CIDR),
        ];
        for (value, ty) in values {
            round_trip(&constant(value, ty));
        }
        let mut array = Array::one(vec![Some(Value::Int4(1)), None, Some(Value::Int4(3))]);
        array.dims[0].lower = -2;
        round_trip(&constant(Value::Array(Box::new(array)), oid::INT4_ARRAY));
        round_trip(&constant(Value::Array(Box::new(Array::empty())), oid::TEXT_ARRAY));
    }

    #[test]
    fn nodes() {
        let var =
            |attnum| Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), oid::INT4);
        let call = Expr {
            kind: ExprKind::Func(Func {
                oid: 177,
                args: vec![var(1), constant(Value::Int4(1), oid::INT4)],
                form: FuncForm::Operator(551),
                variadic: false,
            }),
            ty: oid::INT4,
            typmod: -1,
            location: Some(12),
        };
        let text = round_trip(&call);
        assert_eq!(
            text,
            "{FUNC :type 23 :typmod -1 :location 12 :oid 177 :form (operator 551) :variadic false :args ({VAR :type 23 :typmod -1 :location -1 :relation 0 :attnum 1 :levelsup 0} {CONST :type 23 :typmod -1 :location 3 :value (int4 1)})}"
        );
        let test = Expr::new(ExprKind::NullTest(Box::new(var(2)), false), oid::BOOL);
        let case = Expr::new(
            ExprKind::Case(Case {
                arg: Some(Box::new(var(1))),
                whens: vec![(Expr::new(ExprKind::CaseTest, oid::INT4), var(2))],
                default: Box::new(constant(Value::Null, oid::INT4)),
            }),
            oid::INT4,
        );
        let nodes = [
            Expr::new(ExprKind::Bool(BoolOp::Not, vec![test.clone()]), oid::BOOL),
            Expr::new(ExprKind::BooleanTest(Box::new(test), BoolTest::IsNotUnknown), oid::BOOL),
            case,
            Expr::new(
                ExprKind::Case(Case { arg: None, whens: vec![], default: Box::new(var(1)) }),
                oid::INT4,
            ),
            Expr::new(ExprKind::Coalesce(vec![var(1), var(2)]), oid::INT4),
            Expr::new(ExprKind::MinMax { greatest: true, less: 66, args: vec![var(1)] }, oid::INT4),
            Expr::new(ExprKind::NullIf { equal: 65, args: vec![var(1), var(2)] }, oid::INT4),
            Expr::new(
                ExprKind::Distinct { equal: 65, not: true, args: vec![var(1), var(2)] },
                oid::BOOL,
            ),
            Expr::new(
                ExprKind::ScalarArrayOp { func: 65, any: false, args: vec![var(1)] },
                oid::BOOL,
            ),
            Expr::new(
                ExprKind::Array { element: oid::INT4, multidims: false, elements: vec![var(1)] },
                oid::INT4_ARRAY,
            ),
            Expr::new(ExprKind::SqlValue(SqlValue::CurrentTimestamp(Some(3))), oid::TIMESTAMPTZ),
            Expr::new(ExprKind::SqlValue(SqlValue::CurrentUser), oid::NAME),
            Expr::new(ExprKind::Relabel(Box::new(var(1)), CastForm::Explicit), oid::OID)
                .at(Some(0)),
            Expr::new(ExprKind::CoerceViaIo(Box::new(var(1)), CastForm::Implicit), oid::TEXT),
            Expr {
                kind: ExprKind::Func(Func {
                    oid: 1,
                    args: vec![],
                    form: FuncForm::ImplicitCast,
                    variadic: true,
                }),
                ty: oid::VARCHAR,
                typmod: 14,
                location: None,
            },
        ];
        for node in &nodes {
            round_trip(node);
        }
        let text = write_list(&nodes[..2]).expect("write");
        assert!(text.starts_with("({BOOL") && text.ends_with("})"), "{text}");
        assert_eq!(read_list(&text).expect("read"), nodes[..2]);
        assert_eq!(read_list("()").expect("read"), []);
    }

    #[test]
    fn bad_forms() {
        for text in [
            "",
            "{",
            "{CONST}",
            "{CONST :type 23 :typmod -1 :location -1 :value (int4 x)}",
            "{VAR :type 23} x",
            "(\"a)",
        ] {
            assert!(read(text).is_err(), "{text}");
        }
        let param = Expr::new(ExprKind::Param(1), oid::INT4);
        assert!(write(&param).is_err());
    }
}

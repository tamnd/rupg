//! The translation of the C of an action of `gram.y` to Rust.
//!
//! Most actions make a node, set its fields and give it as `$$`, or report an error. The translator reads the C of such an action and writes the Rust for `reduce`, so that a port by hand is necessary only for an action with a loop, a `switch` or a helper that `src/actions` does not have. It knows the types of the values `$k`, of the fields of the structs, of the constants and of the functions of `src/actions`, and it converts a value to the type that the C converts it to. For C that it does not know, it gives `Err` with the reason, and the glue makes the action a method of a trait.
//!
//! Lifted from `xtask/src/postgres/translate.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::nodes::{self, Kind, RUST_KEYWORDS};

/// The type of a value in the translation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Ty {
    /// An integer of the Rust type.
    Int(&'static str),
    Bool,
    /// A C `char`, a `u8`.
    Char,
    /// A C enum.
    Enum(String),
    /// `char *`, an `Option<Str>`.
    Text,
    /// A keyword, a `&'static str`.
    Keyword,
    /// A parameter `&str`.
    Str,
    /// A `String` that a function gives.
    String,
    List,
    /// A parameter `&List`.
    ListRef,
    /// `Node *`, an `Option<Node>`.
    Node,
    /// A parameter `Option<&Node>`.
    NodeRef,
    /// A `Node` that is not `NULL`, as a make function gives it.
    Bare,
    /// A pointer to a struct, an `Option<Box<T>>`.
    Boxed(String),
    /// A pointer to a struct that is not `NULL`, a `Box<T>`.
    Owned(String),
    /// A struct by value.
    Struct(String),
    /// `yyscanner`, a `&Parser<'_>`.
    Scanner,
    /// The result of a function that gives nothing.
    Unit,
    /// A parameter `&mut T`, for a pointer that the function writes through.
    Out(Box<Ty>),
    /// A parameter `Option<&mut T>`, for such a pointer that can be `NULL`.
    OutOrNull(Box<Ty>),
    /// `NULL`.
    Null,
    /// `NIL`.
    Nil,
    /// A number literal.
    Number,
    /// A string literal.
    Literal,
}

impl Ty {
    /// The type of a Rust type in the glue or in a signature of `src/actions`.
    pub(super) fn of(rust: &str, context: &Context) -> Option<Ty> {
        let pointee = |t: &str| context.structs.contains_key(t).then(|| t.to_string());
        Some(match rust {
            "i32" | "i16" | "u32" | "i64" | "u64" => Ty::Int(match rust {
                "i32" => "i32",
                "i16" => "i16",
                "u32" => "u32",
                "i64" => "i64",
                _ => "u64",
            }),
            "bool" => Ty::Bool,
            "u8" => Ty::Char,
            "Option<Str>" => Ty::Text,
            "&'static str" => Ty::Keyword,
            "&str" => Ty::Str,
            "String" => Ty::String,
            "List" => Ty::List,
            "&List" => Ty::ListRef,
            "Option<Node>" => Ty::Node,
            "Option<&Node>" => Ty::NodeRef,
            "Node" => Ty::Bare,
            "&Parser<'_>" => Ty::Scanner,
            "()" => Ty::Unit,
            t if context.enums.contains(t) => Ty::Enum(t.to_string()),
            t => {
                if let Some(inner) = t.strip_prefix("&mut ") {
                    Ty::Out(Box::new(Ty::of(inner, context)?))
                } else if let Some(inner) =
                    t.strip_prefix("Option<&mut ").and_then(|t| t.strip_suffix('>'))
                {
                    Ty::OutOrNull(Box::new(Ty::of(inner, context)?))
                } else if let Some(inner) =
                    t.strip_prefix("Option<Box<").and_then(|t| t.strip_suffix(">>"))
                {
                    Ty::Boxed(pointee(inner)?)
                } else if let Some(inner) = t.strip_prefix("Box<").and_then(|t| t.strip_suffix('>'))
                {
                    Ty::Owned(pointee(inner)?)
                } else {
                    Ty::Struct(pointee(t)?)
                }
            }
        })
    }

    /// The type of a field of a struct.
    pub(super) fn of_kind(kind: &Kind) -> Ty {
        match kind {
            Kind::Bool => Ty::Bool,
            Kind::Char => Ty::Char,
            Kind::Int(t) => Ty::Int(t),
            Kind::Location => Ty::Int("i32"),
            Kind::Text => Ty::Text,
            Kind::List => Ty::List,
            Kind::Node | Kind::Value => Ty::Node,
            Kind::Boxed(t) => Ty::Boxed(t.clone()),
            Kind::Embedded(t) => Ty::Struct(t.clone()),
            Kind::Enum(t) => Ty::Enum(t.clone()),
        }
    }

    /// A value of the type can be used again after a use.
    fn copy(&self) -> bool {
        matches!(
            self,
            Ty::Int(_)
                | Ty::Bool
                | Ty::Char
                | Ty::Enum(_)
                | Ty::Keyword
                | Ty::Str
                | Ty::Scanner
                | Ty::Null
                | Ty::Nil
                | Ty::Number
                | Ty::Literal
        )
    }
}

/// A function of `src/actions`.
pub(super) struct Function {
    parameters: Vec<Ty>,
    result: Ty,
    fallible: bool,
}

/// What the translator knows of the types.
#[derive(Default)]
pub(super) struct Context {
    /// The fields of each struct, by the C name of the field: the Rust name and the type.
    pub(super) structs: HashMap<String, HashMap<String, (String, Ty)>>,
    /// The node structs, which a `Node` can hold.
    pub(super) nodes: BTreeSet<String>,
    /// The C enums that the Rust has.
    pub(super) enums: BTreeSet<String>,
    /// The constants: the Rust expression and the type.
    pub(super) constants: HashMap<String, (String, Ty)>,
    /// The functions of `src/actions` that the translator can call.
    pub(super) functions: HashMap<String, Function>,
}

impl Context {
    /// Adds the fields of a struct.
    pub(super) fn add_struct<'a>(
        &mut self,
        name: &str,
        fields: impl IntoIterator<Item = (&'a str, Kind)>,
    ) {
        let fields = fields
            .into_iter()
            .map(|(field, kind)| {
                (field.to_string(), (nodes::field_name(field), Ty::of_kind(&kind)))
            })
            .collect();
        self.structs.insert(name.to_string(), fields);
    }

    /// Adds the functions of the text of `src/actions`: each `pub(crate) fn` at the start of a line, with no generic parameter and with types that the translator knows.
    pub(super) fn add_functions(&mut self, text: &str) {
        for (at, _) in text.match_indices("\npub(crate) fn ") {
            let rest = &text[at + "\npub(crate) fn ".len()..];
            let Some(open) = rest.find('(') else { continue };
            let name = &rest[..open];
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                continue;
            }
            let Some(close) = rest.find(')') else { continue };
            let Some(end) = rest.find('{') else { continue };
            let parameters = &rest[open + 1..close];
            let result = rest[close + 1..end].trim();
            let result = result.strip_prefix("->").unwrap_or("()").trim();
            let (result, fallible) =
                match result.strip_prefix("Result<").and_then(|r| r.strip_suffix(", Error>")) {
                    Some(inner) => (inner, true),
                    None => (result, false),
                };
            let Some(result) = Ty::of(result, self) else { continue };
            let parameters: Option<Vec<Ty>> = parameters
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| Ty::of(p.split_once(':')?.1.trim(), self))
                .collect();
            let Some(parameters) = parameters else { continue };
            self.functions.insert(name.to_string(), Function { parameters, result, fallible });
        }
    }
}

/// A value `$k` or the value `$$` of an action: the member of `%union`, its Rust type and its type in the translation.
pub(super) struct Slot {
    pub(super) member: String,
    pub(super) rust: String,
    pub(super) ty: Ty,
}

/// A token of C.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Value(usize),
    Location(usize),
    Result,
    Here,
    Name(String),
    Number(i64),
    /// The text of a string literal, with the escapes of C, which Rust has too.
    Text(String),
    Char(u8),
    Punct(&'static str),
}

/// The punctuation of C, with each one that starts another after it.
const PUNCTUATION: [&str; 39] = [
    "->", "==", "!=", "&&", "||", "<=", ">=", "<<", ">>", "++", "--", "+=", "-=", "|=", "&=", "(",
    ")", "{", "}", "[", "]", ";", ",", "=", "!", "<", ">", "*", "&", "|", "^", "~", "%", "/", ".",
    "-", "+", "?", ":",
];

fn tokens(code: &str) -> Result<Vec<Token>, String> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'$' || c == b'@' {
            if bytes.get(i + 1) == Some(&b'$') {
                out.push(if c == b'$' { Token::Result } else { Token::Here });
                i += 2;
                continue;
            }
            let digits = bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
            if digits == 0 {
                return Err("a $ or a @ that the translator does not know".into());
            }
            let k = code[i + 1..i + 1 + digits].parse().map_err(|e| format!("{e}"))?;
            out.push(if c == b'$' { Token::Value(k) } else { Token::Location(k) });
            i += 1 + digits;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let n =
                bytes[i..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
            out.push(Token::Name(code[i..i + n].to_string()));
            i += n;
            continue;
        }
        if c.is_ascii_digit() {
            let n = bytes[i..].iter().take_while(|b| b.is_ascii_alphanumeric()).count();
            let number =
                code[i..i + n].parse().map_err(|_| format!("the number {}", &code[i..i + n]))?;
            out.push(Token::Number(number));
            i += n;
            continue;
        }
        if c == b'"' {
            let mut text = String::new();
            let mut j = i + 1;
            loop {
                match bytes.get(j) {
                    None => return Err("a string that does not end".into()),
                    Some(b'"') => break,
                    Some(b'\\') => {
                        let next = *bytes.get(j + 1).ok_or("a string that does not end")?;
                        if !matches!(next, b'"' | b'\\' | b'n' | b't' | b'\'') {
                            return Err("an escape that the translator does not know".into());
                        }
                        text.push_str(&code[j..j + 2]);
                        j += 2;
                    }
                    Some(_) => {
                        let ch = code[j..].chars().next().ok_or("a bad string")?;
                        text.push(ch);
                        j += ch.len_utf8();
                    }
                }
            }
            // C joins the string literals that follow each other.
            match out.last_mut() {
                Some(Token::Text(previous)) => previous.push_str(&text),
                _ => out.push(Token::Text(text)),
            }
            i = j + 1;
            continue;
        }
        if c == b'\'' {
            match (bytes.get(i + 1), bytes.get(i + 2)) {
                (Some(&ch), Some(b'\'')) if ch != b'\\' => {
                    out.push(Token::Char(ch));
                    i += 3;
                    continue;
                }
                _ => return Err("a character literal that the translator does not know".into()),
            }
        }
        let Some(punct) = PUNCTUATION.iter().find(|p| code[i..].starts_with(**p)) else {
            return Err(format!("the character {}", char::from(c)));
        };
        out.push(Token::Punct(punct));
        i += punct.len();
    }
    Ok(out)
}

/// An expression of C.
#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Value(usize),
    Location(usize),
    Result,
    Here,
    Number(i64),
    Text(String),
    Char(u8),
    Name(String),
    Call(String, Vec<Expr>),
    /// `(type) expression`, with the type as `T*` or `int`.
    Cast(String, Box<Expr>),
    Not(Box<Expr>),
    Negative(Box<Expr>),
    /// `~e`.
    Complement(Box<Expr>),
    /// `&e`, which only an argument for a parameter `&mut T` can be.
    Address(Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `base->field` or `base.field`. The types tell them apart.
    Field(Box<Expr>, String),
    /// `(a, b, ...)`, as the second argument of `ereport` has it.
    Comma(Vec<Expr>),
}

/// The Rust of an `ereport`.
enum Report {
    /// The error that the action returns.
    Error(String),
    /// The call that adds a notice and lets the action continue.
    Warning(String),
}

/// A statement of C.
#[derive(Clone, Debug)]
enum Stmt {
    /// A declaration, with no value for `T name;`.
    Declare {
        ctype: String,
        name: String,
        init: Option<Expr>,
    },
    Assign(Expr, Expr),
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    Block(Vec<Stmt>),
}

/// The scalar types of C that a cast or a declaration can name.
const SCALARS: [&str; 8] = ["int", "bool", "char", "int16", "int32", "int64", "uint32", "Oid"];

/// The type of a scalar type of C.
fn scalar(ctype: &str) -> Option<Ty> {
    Some(match ctype {
        "int" | "int32" => Ty::Int("i32"),
        "int16" => Ty::Int("i16"),
        "int64" => Ty::Int("i64"),
        "uint32" | "Oid" => Ty::Int("u32"),
        "bool" => Ty::Bool,
        "char" => Ty::Char,
        _ => return None,
    })
}

struct Parse {
    tokens: Vec<Token>,
    at: usize,
}

impl Parse {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.at + n)
    }

    fn is(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Token::Punct(p)) if *p == punct)
    }

    fn eat(&mut self, punct: &str) -> bool {
        let is = self.is(punct);
        if is {
            self.at += 1;
        }
        is
    }

    fn expect(&mut self, punct: &str) -> Result<(), String> {
        if self.eat(punct) { Ok(()) } else { Err(format!("a {punct} is missing")) }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.peek() {
            Some(Token::Name(name)) => {
                let name = name.clone();
                self.at += 1;
                Ok(name)
            }
            _ => Err("a name is missing".into()),
        }
    }

    fn statements(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        while self.peek().is_some() && !self.is("}") {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    /// The type of a declaration at the current token, as `T*` or `int`, when the statement is a declaration.
    fn declaration(&self) -> Option<(String, usize)> {
        let Some(Token::Name(t)) = self.peek() else { return None };
        let mut n = 1;
        while matches!(self.peek_at(n), Some(Token::Punct("*"))) {
            n += 1;
        }
        let Some(Token::Name(_)) = self.peek_at(n) else { return None };
        Some((format!("{t}{}", "*".repeat(n - 1)), n))
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if self.eat("{") {
            let block = self.statements()?;
            self.expect("}")?;
            return Ok(Stmt::Block(block));
        }
        if let Some(Token::Name(word)) = self.peek() {
            match word.as_str() {
                "if" => {
                    self.at += 1;
                    self.expect("(")?;
                    let condition = self.expression()?;
                    self.expect(")")?;
                    let then = self.branch()?;
                    let otherwise = if matches!(self.peek(), Some(Token::Name(w)) if w == "else") {
                        self.at += 1;
                        self.branch()?
                    } else {
                        Vec::new()
                    };
                    return Ok(Stmt::If(condition, then, otherwise));
                }
                "else" | "for" | "while" | "do" | "switch" | "return" | "goto" | "break"
                | "const" | "struct" | "static" => {
                    return Err(format!("the C word {word}"));
                }
                _ => {}
            }
        }
        if let Some((ctype, n)) = self.declaration() {
            self.at += n;
            let name = self.name()?;
            let init = if self.eat("=") { Some(self.expression()?) } else { None };
            self.expect(";")?;
            return Ok(Stmt::Declare { ctype, name, init });
        }
        let target = self.expression()?;
        if self.eat("=") {
            let value = self.expression()?;
            self.expect(";")?;
            return Ok(Stmt::Assign(target, value));
        }
        self.expect(";")?;
        Ok(Stmt::Expr(target))
    }

    fn branch(&mut self) -> Result<Vec<Stmt>, String> {
        Ok(match self.statement()? {
            Stmt::Block(block) => block,
            statement => vec![statement],
        })
    }

    fn expression(&mut self) -> Result<Expr, String> {
        let condition = self.binary(0)?;
        if self.eat("?") {
            let then = self.expression()?;
            self.expect(":")?;
            let otherwise = self.expression()?;
            return Ok(Expr::Conditional(condition.into(), then.into(), otherwise.into()));
        }
        Ok(condition)
    }

    /// The binary operators of C by precedence, from the lowest.
    const LEVELS: [&'static [&'static str]; 10] = [
        &["||"],
        &["&&"],
        &["|"],
        &["^"],
        &["&"],
        &["==", "!="],
        &["<", ">", "<=", ">="],
        &["<<", ">>"],
        &["+", "-"],
        &["*", "/", "%"],
    ];

    fn binary(&mut self, level: usize) -> Result<Expr, String> {
        if level == Self::LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        while let Some(Token::Punct(p)) = self.peek() {
            let Some(&op) = Self::LEVELS[level].iter().find(|op| *op == p) else { break };
            self.at += 1;
            let right = self.binary(level + 1)?;
            left = Expr::Binary(op, left.into(), right.into());
        }
        Ok(left)
    }

    /// The type of a cast at the current `(`, with the number of its tokens.
    fn cast(&self) -> Option<(String, usize)> {
        if !self.is("(") {
            return None;
        }
        let Some(Token::Name(t)) = self.peek_at(1) else { return None };
        let mut n = 2;
        while matches!(self.peek_at(n), Some(Token::Punct("*"))) {
            n += 1;
        }
        let stars = n - 2;
        if !matches!(self.peek_at(n), Some(Token::Punct(")"))) {
            return None;
        }
        // `(Name) value` with no star is a cast to a typedef, such as an enum, because a parenthesized name is never next to a value in C.
        let value = matches!(
            self.peek_at(n + 1),
            Some(Token::Value(_) | Token::Location(_) | Token::Name(_) | Token::Number(_))
        );
        (stars > 0 || SCALARS.contains(&t.as_str()) || value)
            .then(|| (format!("{t}{}", "*".repeat(stars)), n + 1))
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat("!") {
            return Ok(Expr::Not(self.unary()?.into()));
        }
        if self.eat("-") {
            return Ok(Expr::Negative(self.unary()?.into()));
        }
        if self.eat("+") {
            return self.unary();
        }
        if self.eat("~") {
            return Ok(Expr::Complement(self.unary()?.into()));
        }
        if self.eat("&") {
            return Ok(Expr::Address(self.unary()?.into()));
        }
        if let Some((ctype, n)) = self.cast() {
            self.at += n;
            return Ok(Expr::Cast(ctype, self.unary()?.into()));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.is("(") {
                let Expr::Name(name) = e else { return Err("a call of an expression".into()) };
                self.at += 1;
                let mut arguments = Vec::new();
                if !self.eat(")") {
                    loop {
                        arguments.push(self.expression()?);
                        if self.eat(")") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                e = Expr::Call(name, arguments);
            } else if self.eat("->") || self.eat(".") {
                e = Expr::Field(e.into(), self.name()?);
            } else if self.is("[") || self.is("++") || self.is("--") {
                return Err("an index or an increment".into());
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let token = self.peek().cloned().ok_or("an expression is missing")?;
        self.at += 1;
        Ok(match token {
            Token::Value(k) => Expr::Value(k),
            Token::Location(k) => Expr::Location(k),
            Token::Result => Expr::Result,
            Token::Here => Expr::Here,
            Token::Number(n) => Expr::Number(n),
            Token::Text(t) => Expr::Text(t),
            Token::Char(c) => Expr::Char(c),
            Token::Name(n) => Expr::Name(n),
            Token::Punct("(") => {
                let mut list = vec![self.expression()?];
                while self.eat(",") {
                    list.push(self.expression()?);
                }
                self.expect(")")?;
                if list.len() == 1 { list.remove(0) } else { Expr::Comma(list) }
            }
            Token::Punct(p) => return Err(format!("the operator {p}")),
        })
    }
}

/// A Rust expression, its type and the precedence of its operator in Rust.
#[derive(Clone)]
struct Typed {
    code: String,
    ty: Ty,
    precedence: u8,
}

/// The precedence of an expression with no operator, as a name, a call or a literal.
const ATOM: u8 = 100;

/// The precedence of `as` in Rust.
const AS: u8 = 11;

/// The precedence of an expression that is a condition, which every operator binds tighter than.
const CONDITION: u8 = 1;

/// The precedence in Rust of a binary operator of C. Rust binds `&`, `^` and `|` tighter than a comparison, and C does not, so the Rust of a C expression can need parentheses that the C does not have, and the reverse.
fn precedence(op: &str) -> u8 {
    match op {
        "*" | "/" | "%" => 10,
        "+" | "-" => 9,
        "<<" | ">>" => 8,
        "&" => 7,
        "^" => 6,
        "|" => 5,
        _ => CONDITION,
    }
}

impl Typed {
    fn new(code: impl Into<String>, ty: Ty) -> Typed {
        Typed { code: code.into(), ty, precedence: ATOM }
    }

    /// The code as an operand of an operator of the precedence `outer`.
    fn operand(&self, outer: u8) -> String {
        if self.precedence < outer { format!("({})", self.code) } else { self.code.clone() }
    }
}

/// A value that the Rust moved into a field, `base->field`, as a pointer to the struct `structure`. C can change it through the pointer that it still has, and the Rust changes it through the field.
#[derive(Clone, Debug, PartialEq)]
struct Alias {
    base: Expr,
    field: String,
    structure: String,
}

/// The names that the Rust of `reduce` has, which a C local must not take.
const TAKEN: [&str; 6] = ["p", "at", "here", "rhs", "result", "rule"];

/// The state of the translation on a path of the C.
#[derive(Clone, Default)]
struct Path {
    /// The values and the locals that the Rust moved, and the fields that it took, as `v1.field`.
    moved: BTreeSet<String>,
    /// The values and the locals that the Rust moved into a field.
    aliases: BTreeMap<String, Alias>,
    /// `$$` has a value.
    assigned: bool,
    /// The path ends in an error.
    diverges: bool,
}

struct Translator<'a> {
    context: &'a Context,
    values: &'a [Option<Slot>],
    result: Option<&'a Slot>,
    locals: HashMap<String, Ty>,
    /// The line of the `let` of each local, to add `mut` when the Rust changes it.
    declared: HashMap<String, usize>,
    opened: BTreeSet<usize>,
    mutable: BTreeSet<String>,
    path: Path,
    /// The values and the locals that the current statement uses.
    statement: BTreeSet<String>,
    lines: Vec<String>,
    depth: usize,
    /// `$$` is a local `result`, because the action sets it before its end.
    late: bool,
    /// The number of the uses of each value that the translation has not come to yet, on the path that it is on.
    later: BTreeMap<usize, usize>,
    /// The places that the action changes, as `v1.rel` or `n.options`: the left sides of the assignments and the operands of `&`.
    written: BTreeSet<String>,
    /// The place that the current assignment sets, as `n.options`.
    target: Option<String>,
}

fn fail<T>(reason: impl Into<String>) -> Result<T, String> {
    Err(reason.into())
}

impl Translator<'_> {
    fn line(&mut self, text: impl AsRef<str>) {
        self.lines.push(format!("{}{}", "    ".repeat(self.depth), text.as_ref()));
    }

    fn slot(&self, k: usize) -> Result<&Slot, String> {
        match self.values.get(k.wrapping_sub(1)) {
            Some(Some(slot)) => Ok(slot),
            Some(None) => fail(format!("${k} has no type")),
            None => fail(format!("${k} is past the right side")),
        }
    }

    fn location(&self, k: usize) -> Result<Typed, String> {
        if k == 0 || k > self.values.len() {
            return fail(format!("@{k} is past the right side"));
        }
        Ok(Typed::new(format!("at[{}]", k - 1), Ty::Int("i32")))
    }

    /// A use of a value or a local by name. A move of a value of a type that is not `Copy` must be the only use.
    fn use_name(&mut self, name: &str, ty: &Ty, moves: bool) -> Result<(), String> {
        if self.path.moved.contains(name) {
            return fail(format!("{name} is used after a move"));
        }
        let field = format!("{name}.");
        if moves && self.path.moved.iter().any(|m| m.starts_with(&field)) {
            return fail(format!("{name} is moved after a move of a field"));
        }
        if moves && !ty.copy() {
            if self.statement.contains(name) {
                return fail(format!("{name} is used twice in a statement"));
            }
            self.path.moved.insert(name.to_string());
        }
        self.statement.insert(name.to_string());
        Ok(())
    }

    /// The action changes the place `name` or a place in it.
    fn changes(&self, name: &str) -> bool {
        let inside = format!("{name}.");
        self.written.iter().any(|w| w == name || w.starts_with(&inside))
    }

    /// `$k` or a local, as a value that the Rust moves or borrows.
    fn variable(&mut self, e: &Expr, moves: bool) -> Result<Option<Typed>, String> {
        match e {
            Expr::Value(k) => {
                let ty = self.slot(*k)?.ty.clone();
                self.opened.insert(*k);
                let name = format!("v{k}");
                let later = self.later.get_mut(k).map_or(0, |n| {
                    *n = n.saturating_sub(1);
                    *n
                });
                // C has the same pointer in two places, and the Rust has a copy for the move when a use comes after it. A value that the action changes cannot have a copy, because the change must show in each place.
                if moves && !ty.copy() && later > 0 && !self.changes(&name) {
                    self.use_name(&name, &ty, false)?;
                    return Ok(Some(Typed::new(format!("{name}.clone()"), ty)));
                }
                self.use_name(&name, &ty, moves)?;
                Ok(Some(Typed::new(name, ty)))
            }
            Expr::Name(name) if self.locals.contains_key(name) => {
                let ty = self.locals[name].clone();
                self.use_name(name, &ty, moves)?;
                Ok(Some(Typed::new(name.clone(), ty)))
            }
            _ => Ok(None),
        }
    }

    /// The alias of a value or a local that the Rust moved into a field.
    fn alias(&self, e: &Expr) -> Option<Alias> {
        let name = describe(e)?;
        if !self.path.moved.contains(&name) {
            return None;
        }
        self.path.aliases.get(&name).cloned()
    }

    /// The type of `$k` or of a local.
    fn variable_type(&self, e: &Expr) -> Option<Ty> {
        match e {
            Expr::Value(k) => self.slot(*k).ok().map(|slot| slot.ty.clone()),
            Expr::Name(name) => self.locals.get(name).cloned(),
            _ => None,
        }
    }

    /// Notes that `base->field` holds the value of `e` when the Rust moved a value or a local there, and that `base->field` no longer holds what it held before.
    fn moved_into(&mut self, base: &Expr, field: &str, field_ty: &Ty, e: &Expr) {
        self.path.aliases.retain(|_, a| !(a.base == *base && a.field == field));
        let mut inner = e;
        while let Expr::Cast(_, cast) = inner {
            inner = cast;
        }
        let (Some(name), Some(ty)) = (describe(inner), self.variable_type(inner)) else { return };
        let (Ty::Boxed(structure) | Ty::Owned(structure) | Ty::Struct(structure)) = ty else {
            return;
        };
        let holds = match field_ty {
            Ty::Boxed(t) => *t == structure,
            Ty::Node => self.context.nodes.contains(&structure),
            _ => false,
        };
        if holds && self.path.moved.contains(&name) {
            let alias = Alias { base: base.clone(), field: field.to_string(), structure };
            self.path.aliases.insert(name, alias);
        }
    }

    /// The struct that `e` is or points to, for a read of its fields: the Rust of the struct and its name.
    fn access(&mut self, e: &Expr) -> Result<(String, String), String> {
        if let Some(alias) = self.alias(e) {
            let (code, structure) = self.access(&alias.base)?;
            let (rust, ty) = self.field(&structure, &alias.field)?;
            let t = alias.structure;
            return Ok(match ty {
                Ty::Node => (format!("crate::actions::castRef::<{t}>({code}.{rust}.as_ref())?"), t),
                _ => (format!("crate::actions::pointee(&{code}.{rust})?"), t),
            });
        }
        match e {
            Expr::Field(base, field) => {
                let (code, structure) = self.access(base)?;
                let (rust, ty) = self.field(&structure, field)?;
                match ty {
                    Ty::Struct(s) => Ok((format!("{code}.{rust}"), s)),
                    Ty::Boxed(s) => Ok((format!("crate::actions::pointee(&{code}.{rust})?"), s)),
                    _ => fail(format!("a field of {structure}.{field}")),
                }
            }
            Expr::Result => {
                let Some(slot) = self.result else { return fail("$$ with no type") };
                if !self.path.assigned || !self.late {
                    return fail("a field of $$ before $$ has a value");
                }
                match &slot.ty {
                    Ty::Boxed(s) => Ok(("crate::actions::pointee(&result)?".into(), s.clone())),
                    _ => fail("a field of $$ that is not a pointer to a struct"),
                }
            }
            _ => match self.variable(e, false)? {
                Some(Typed { code, ty: Ty::Struct(s) | Ty::Owned(s), .. }) => Ok((code, s)),
                Some(Typed { code, ty: Ty::Boxed(s), .. }) => {
                    Ok((format!("crate::actions::pointee(&{code})?"), s))
                }
                _ => fail("a field of an expression"),
            },
        }
    }

    /// A field that the Rust reads. A field of a type that is not `Copy` is taken, and it is moved: C has the same pointer in two places, which the Rust cannot have.
    fn read_field(&mut self, base: &Expr, field: &str, moves: bool) -> Result<Typed, String> {
        let whole = Expr::Field(base.clone().into(), field.to_string());
        if let Some(name) = describe(&whole)
            && self.path.moved.contains(&name)
        {
            return fail(format!("{name} is used after a move"));
        }
        let (code, structure) = self.access(base)?;
        let (rust, ty) = self.field(&structure, field)?;
        if !moves || ty.copy() {
            return Ok(Typed::new(format!("{code}.{rust}"), ty));
        }
        let Some(name) = describe(&whole) else {
            return fail("a move of a field of an expression");
        };
        // The field of a local stays in the node that the action makes, so C has the pointer in two places, and the Rust has a copy. The Rust takes the field when the assignment sets it again, as `n->options = lappend(n->options, x)` does, or when the action changes a place in it, because the change must show in each place.
        let root = name.split('.').next().unwrap_or_default();
        let inside = format!("{name}.");
        if self.locals.contains_key(root)
            && self.target.as_ref() != Some(&name)
            && !self.written.iter().any(|w| w.starts_with(&inside))
        {
            return Ok(Typed::new(format!("{code}.{rust}.clone()"), ty));
        }
        let (code, _) = self.place(base)?;
        self.path.moved.insert(name);
        Ok(Typed::new(format!("std::mem::take(&mut {code}.{rust})"), ty))
    }

    fn field(&self, structure: &str, field: &str) -> Result<(String, Ty), String> {
        self.context
            .structs
            .get(structure)
            .and_then(|fields| fields.get(field))
            .cloned()
            .ok_or_else(|| format!("{structure} has no field {field}"))
    }

    /// The place of a struct that the Rust changes, and the name of the struct.
    fn place(&mut self, e: &Expr) -> Result<(String, String), String> {
        if let Some(alias) = self.alias(e) {
            let (code, structure) = self.place(&alias.base)?;
            let (rust, ty) = self.field(&structure, &alias.field)?;
            let t = alias.structure;
            return Ok(match ty {
                Ty::Node => (format!("crate::actions::castMut::<{t}>(&mut {code}.{rust})?"), t),
                _ => (format!("crate::actions::pointee_mut(&mut {code}.{rust})?"), t),
            });
        }
        match e {
            Expr::Value(k) => {
                let ty = self.slot(*k)?.ty.clone();
                self.opened.insert(*k);
                let name = format!("v{k}");
                self.use_name(&name, &ty, false)?;
                self.mutable.insert(name.clone());
                match ty {
                    Ty::Boxed(s) => Ok((format!("crate::actions::pointee_mut(&mut {name})?"), s)),
                    _ => fail("a field of a value that is not a pointer to a struct"),
                }
            }
            Expr::Name(name) => {
                let Some(ty) = self.locals.get(name).cloned() else {
                    return fail(format!("the name {name}"));
                };
                self.use_name(name, &ty, false)?;
                self.mutable.insert(name.clone());
                match ty {
                    Ty::Struct(s) | Ty::Owned(s) => Ok((name.clone(), s)),
                    Ty::Boxed(s) => Ok((format!("crate::actions::pointee_mut(&mut {name})?"), s)),
                    _ => fail("a field of a local that is not a struct"),
                }
            }
            Expr::Result => {
                let Some(slot) = self.result else { return fail("$$ with no type") };
                if !self.path.assigned || !self.late {
                    return fail("a field of $$ before $$ has a value");
                }
                self.mutable.insert("result".into());
                match &slot.ty {
                    Ty::Boxed(s) => {
                        Ok(("crate::actions::pointee_mut(&mut result)?".into(), s.clone()))
                    }
                    _ => fail("a field of $$ that is not a pointer to a struct"),
                }
            }
            Expr::Field(base, field) => {
                let (code, structure) = self.place(base)?;
                let (rust, ty) = self.field(&structure, field)?;
                match ty {
                    Ty::Struct(s) => Ok((format!("{code}.{rust}"), s)),
                    Ty::Boxed(s) => {
                        Ok((format!("crate::actions::pointee_mut(&mut {code}.{rust})?"), s))
                    }
                    _ => fail(format!("a field of {structure}.{field}")),
                }
            }
            _ => fail("an assignment to an expression"),
        }
    }

    /// `&e` for a parameter `&mut T` of the type `want`: a local, a value or a field. The Rust writes it, so a move of it before does not matter.
    fn out(&mut self, e: &Expr, want: &Ty) -> Result<String, String> {
        let (code, ty) = match e {
            Expr::Field(base, field) => {
                let (code, structure) = self.place(base)?;
                let (rust, ty) = self.field(&structure, field)?;
                (format!("{code}.{rust}"), ty)
            }
            _ => match self.variable_type(e) {
                Some(ty) => {
                    let name = describe(e).ok_or("an & of an expression")?;
                    if let Expr::Value(k) = e {
                        self.opened.insert(*k);
                    }
                    self.statement.insert(name.clone());
                    self.mutable.insert(name.clone());
                    (name, ty)
                }
                None => return fail("an & of an expression"),
            },
        };
        if ty != *want {
            return fail(format!("an & of a {ty:?} for a {want:?}"));
        }
        if let Some(name) = describe(e) {
            self.forget(&name);
        }
        Ok(format!("&mut {code}"))
    }

    /// The Rust gave a new value to the place `name`: it and its fields are no longer moved.
    fn forget(&mut self, name: &str) {
        let field = format!("{name}.");
        self.path.moved.retain(|m| m != name && !m.starts_with(&field));
        self.path.aliases.remove(name);
    }

    /// An expression that the Rust borrows: a value or a local is not moved.
    fn borrow(&mut self, e: &Expr) -> Result<Typed, String> {
        if let Expr::Field(base, field) = e {
            return self.read_field(base, field, false);
        }
        match self.variable(e, false)? {
            Some(typed) => Ok(typed),
            None => self.value(e),
        }
    }

    fn value(&mut self, e: &Expr) -> Result<Typed, String> {
        if let Some(typed) = self.variable(e, true)? {
            return Ok(typed);
        }
        Ok(match e {
            Expr::Value(_) => unreachable!("a value is a variable"),
            Expr::Location(k) => self.location(*k)?,
            Expr::Here => Typed::new("here", Ty::Int("i32")),
            Expr::Result => return fail("a read of $$"),
            Expr::Number(n) => Typed::new(n.to_string(), Ty::Number),
            Expr::Negative(inner) => match &**inner {
                Expr::Number(n) => Typed::new(format!("-{n}"), Ty::Number),
                _ => self.unary_integer("-", inner)?,
            },
            Expr::Complement(inner) => self.unary_integer("!", inner)?,
            Expr::Address(_) => return fail("an & that is not an argument for a &mut parameter"),
            Expr::Text(t) => Typed::new(format!("\"{t}\""), Ty::Literal),
            Expr::Char(c) => Typed::new(format!("b{:?}", char::from(*c)), Ty::Char),
            Expr::Name(name) => match name.as_str() {
                "NULL" => Typed::new("None", Ty::Null),
                "NIL" => Typed::new("List::new()", Ty::Nil),
                "true" | "false" => Typed::new(name.clone(), Ty::Bool),
                "yyscanner" => Typed::new("p", Ty::Scanner),
                _ => match self.context.constants.get(name) {
                    Some((code, ty)) => Typed::new(code.clone(), ty.clone()),
                    None => return fail(format!("the name {name}")),
                },
            },
            Expr::Call(name, arguments) => self.call(name, arguments)?,
            Expr::Cast(ctype, inner) => self.cast(ctype, inner)?,
            Expr::Not(_)
            | Expr::Binary("||" | "&&" | "==" | "!=" | "<" | ">" | "<=" | ">=", ..) => {
                Typed { code: self.condition(e, false)?, ty: Ty::Bool, precedence: CONDITION }
            }
            Expr::Binary(op, left, right) => self.arithmetic(op, left, right)?,
            Expr::Conditional(condition, then, otherwise) => {
                let condition = self.condition(condition, false)?;
                let then = self.value(then)?;
                let otherwise = self.value(otherwise)?;
                let ty = match (&then.ty, &otherwise.ty) {
                    (Ty::Number, Ty::Number) => Ty::Int("i32"),
                    (Ty::Null | Ty::Nil | Ty::Number | Ty::Literal, ty) => ty.clone(),
                    (ty, _) => ty.clone(),
                };
                let then = self.coerce(then, &ty)?;
                let otherwise = self.coerce(otherwise, &ty)?;
                let code = format!("if {condition} {{ {then} }} else {{ {otherwise} }}");
                Typed { code, ty, precedence: CONDITION }
            }
            Expr::Field(base, field) => self.read_field(base, field, true)?,
            Expr::Comma(_) => return fail("a comma expression"),
        })
    }

    /// The type of an integer operand of C after the integer promotions: a `char`, a `bool`, an enum and an `int16` are an `int`. A number literal has no type.
    fn promoted(ty: &Ty) -> Result<Option<&'static str>, String> {
        Ok(match ty {
            Ty::Number => None,
            Ty::Int("i16") | Ty::Char | Ty::Bool | Ty::Enum(_) => Some("i32"),
            Ty::Int(t) => Some(t),
            ty => return fail(format!("an integer operator of a {ty:?}")),
        })
    }

    /// A binary operator of C on integers, with the usual arithmetic conversions of C for the types that the actions have: `int`, `uint32` and `int64`.
    fn arithmetic(&mut self, op: &'static str, left: &Expr, right: &Expr) -> Result<Typed, String> {
        let left = self.value(left)?;
        let right = self.value(right)?;
        let outer = precedence(op);
        let rank = |t: &str| match t {
            "i64" | "u64" => 2,
            "u32" => 1,
            _ => 0,
        };
        let ty = match (Self::promoted(&left.ty)?, Self::promoted(&right.ty)?) {
            (None, None) => Ty::Number,
            (Some(t), None) | (None, Some(t)) => Ty::Int(t),
            (Some(a), Some(b)) => Ty::Int(if rank(b) > rank(a) { b } else { a }),
        };
        let mut operands = Vec::new();
        for (side, value) in [left, right].into_iter().enumerate() {
            let value = self.convert(value, &ty)?;
            // The right operand of an operator of the same precedence needs parentheses, as in `a - (b - c)`.
            operands.push(value.operand(outer + u8::from(side == 1)));
        }
        Ok(Typed { code: format!("{} {op} {}", operands[0], operands[1]), ty, precedence: outer })
    }

    /// `-e` or `~e` of C on an integer, which Rust writes `-e` and `!e`.
    fn unary_integer(&mut self, op: &str, inner: &Expr) -> Result<Typed, String> {
        let value = self.value(inner)?;
        let ty = match Self::promoted(&value.ty)? {
            Some(t) => Ty::Int(t),
            None => Ty::Number,
        };
        let value = self.convert(value, &ty)?;
        Ok(Typed { code: format!("{op}{}", value.operand(AS + 1)), ty, precedence: AS + 1 })
    }

    /// [`Self::coerce`], with the precedence of the result.
    fn convert(&self, value: Typed, want: &Ty) -> Result<Typed, String> {
        if value.ty == *want {
            return Ok(value);
        }
        let code = self.coerce(value, want)?;
        let precedence = if code.ends_with(" != 0") { CONDITION } else { AS };
        Ok(Typed { code, ty: want.clone(), precedence })
    }

    fn cast(&mut self, ctype: &str, inner: &Expr) -> Result<Typed, String> {
        let value = self.value(inner)?;
        let ty = match scalar(ctype) {
            Some(ty) => ty,
            None => match ctype {
                "Node*" => Ty::Node,
                "List*" => Ty::List,
                "char*" => Ty::Text,
                t if self.context.enums.contains(t) => Ty::Enum(t.to_string()),
                t => match t.strip_suffix('*') {
                    Some(s) if self.context.structs.contains_key(s) => match &value.ty {
                        Ty::Struct(v) | Ty::Owned(v) | Ty::Boxed(v) if v == s => return Ok(value),
                        _ => Ty::Boxed(s.to_string()),
                    },
                    _ => return fail(format!("a cast to {t}")),
                },
            },
        };
        self.convert(value, &ty)
    }

    fn call(&mut self, name: &str, arguments: &[Expr]) -> Result<Typed, String> {
        let structure = |e: &Expr| match e {
            Expr::Name(s) if self.context.structs.contains_key(s) => Ok(s.clone()),
            _ => fail(format!("{name} of a type that the translator does not know")),
        };
        match (name, arguments) {
            ("makeNode" | "palloc_object" | "palloc0_object", [t]) => {
                let t = structure(t)?;
                return Ok(Typed::new(format!("{t}::default()"), Ty::Struct(t)));
            }
            ("castNode", [t, e]) => {
                let t = structure(t)?;
                if !self.context.nodes.contains(&t) {
                    return fail(format!("castNode to {t}"));
                }
                let value = self.value(e)?;
                if value.ty == Ty::Boxed(t.clone()) {
                    return Ok(value);
                }
                let code = self.coerce(value, &Ty::Node)?;
                return Ok(Typed::new(
                    format!("crate::actions::castNode::<{t}>({code})?"),
                    Ty::Boxed(t),
                ));
            }
            ("pstrdup", [e]) => {
                let value = self.value(e)?;
                let code = self.coerce(value, &Ty::Text)?;
                return Ok(Typed::new(code, Ty::Text));
            }
            ("psprintf", [Expr::Text(format), arguments @ ..]) => {
                return self.format(format, arguments);
            }
            _ => {}
        }
        if let Some(n) = name.strip_prefix("list_make").and_then(|n| n.parse::<usize>().ok())
            && n == arguments.len()
        {
            let mut items = Vec::new();
            for argument in arguments {
                let value = self.value(argument)?;
                items.push(self.coerce(value, &Ty::Node)?);
            }
            return Ok(Typed::new(format!("vec![{}]", items.join(", ")), Ty::List));
        }
        let Some(function) = self.context.functions.get(name) else {
            return fail(format!("the function {name}"));
        };
        if function.parameters.len() != arguments.len() {
            return fail(format!("{name} with {} arguments", arguments.len()));
        }
        let mut codes = Vec::new();
        for (parameter, argument) in function.parameters.iter().zip(arguments) {
            let code = match parameter {
                Ty::Scanner => match argument {
                    Expr::Name(n) if n == "yyscanner" => "p".to_string(),
                    _ => return fail("a scanner that is not yyscanner"),
                },
                Ty::OutOrNull(_) if matches!(argument, Expr::Name(n) if n == "NULL") => {
                    "None".to_string()
                }
                Ty::Out(want) | Ty::OutOrNull(want) => {
                    let code = match argument {
                        Expr::Address(target) => self.out(target, want)?,
                        // A list is a pointer in C, and the function changes the list.
                        e if **want == Ty::List => self.out(e, want)?,
                        _ => return fail("an argument for a &mut parameter that is not an &"),
                    };
                    if matches!(parameter, Ty::OutOrNull(_)) {
                        format!("Some({code})")
                    } else {
                        code
                    }
                }
                Ty::ListRef | Ty::NodeRef | Ty::Str => {
                    let value = self.borrow(argument)?;
                    self.coerce(value, parameter)?
                }
                _ => {
                    let value = self.value(argument)?;
                    self.coerce(value, parameter)?
                }
            };
            codes.push(code);
        }
        let question = if function.fallible { "?" } else { "" };
        let mut code = format!("crate::actions::{name}({}){question}", codes.join(", "));
        // C passes `&c->a` and `&c->b` to one call. A place through a cast or a pointer borrows all the node, so the cast is bound once and the fields are borrowed from the binding.
        let mut bases: Vec<&str> = Vec::new();
        for (parameter, code) in function.parameters.iter().zip(&codes) {
            if matches!(parameter, Ty::Out(_) | Ty::OutOrNull(_))
                && let Some(base) = out_base(code)
            {
                bases.push(base);
            }
        }
        let mut bindings = String::new();
        let mut done = Vec::new();
        for base in &bases {
            if done.contains(base) || bases.iter().filter(|b| *b == base).count() < 2 {
                continue;
            }
            let binding = format!("place{}", done.len());
            code = code.replace(&format!("&mut {base}."), &format!("&mut {binding}."));
            bindings.push_str(&format!("let {binding} = {base}; "));
            done.push(base);
        }
        if !bindings.is_empty() {
            code = format!("{{ {bindings}{code} }}");
        }
        Ok(Typed::new(code, function.result.clone()))
    }

    /// The Rust of `value` as a value of the type `want`, as C converts it.
    fn coerce(&self, value: Typed, want: &Ty) -> Result<String, String> {
        let atom = value.operand(ATOM);
        let operand = value.operand(AS);
        let Typed { code: c, ty, .. } = value;
        let node = |t: &str| self.context.nodes.contains(t);
        Ok(match (&ty, want) {
            (a, b) if a == b => c,
            (Ty::Number, Ty::Int(_)) => c,
            (Ty::Number, Ty::Char) if c.parse::<u8>().is_ok() => c,
            (Ty::Number, Ty::Bool) if c == "0" => "false".into(),
            (Ty::Number, Ty::Bool) if c == "1" => "true".into(),
            (Ty::Number, Ty::Enum(e)) => format!("{e}({c})"),
            (Ty::Int("i32"), Ty::Enum(e)) => format!("{e}({c})"),
            (Ty::Enum(_), Ty::Int("i32")) => format!("{atom}.0"),
            // The conversions of C between the integer types. One that can lose bits keeps the low bits, as `as` does.
            (Ty::Enum(_), Ty::Int(b)) => format!("{atom}.0 as {b}"),
            (Ty::Enum(_), Ty::Char) => format!("{atom}.0 as u8"),
            (Ty::Char | Ty::Bool, Ty::Int(b)) => format!("{b}::from({c})"),
            (Ty::Int(a), Ty::Int(b))
                if matches!(
                    (*a, *b),
                    ("i16", "i32" | "i64") | ("i32", "i64") | ("u32", "i64" | "u64")
                ) =>
            {
                format!("{b}::from({c})")
            }
            (Ty::Int(_), Ty::Int(b)) => format!("{operand} as {b}"),
            (Ty::Int(_), Ty::Char) => format!("{operand} as u8"),
            (Ty::Int(_) | Ty::Char, Ty::Bool) => format!("{operand} != 0"),
            (Ty::Null, Ty::Text | Ty::Node | Ty::NodeRef | Ty::Boxed(_)) => c,
            (Ty::Null | Ty::Nil, Ty::List) => "List::new()".into(),
            (Ty::Nil, Ty::Node) => "None".into(),
            (Ty::Literal | Ty::Keyword | Ty::String, Ty::Text) => format!("Some({c}.into())"),
            (Ty::Literal | Ty::Keyword, Ty::Str) | (Ty::Literal, Ty::Keyword) => c,
            (Ty::Bare, Ty::Node) => format!("Some({c})"),
            (Ty::Bare, Ty::NodeRef) => format!("Some(&{c})"),
            (Ty::Bare, Ty::Boxed(t)) => format!("crate::actions::castNode::<{t}>(Some({c}))?"),
            (Ty::Struct(t), Ty::Node) if node(t) => format!("Some({c}.into())"),
            (Ty::Struct(t), Ty::Bare) if node(t) => format!("Node::from({c})"),
            (Ty::Struct(t), Ty::Boxed(u)) if t == u => format!("Some(Box::new({c}))"),
            (Ty::Struct(t), Ty::Owned(u)) if t == u => format!("Box::new({c})"),
            (Ty::Owned(t), Ty::Boxed(u)) if t == u => format!("Some({c})"),
            (Ty::Owned(t), Ty::Node) if node(t) => format!("Some({c}.into_node())"),
            (Ty::Owned(t), Ty::Bare) if node(t) => format!("{c}.into_node()"),
            (Ty::Boxed(t), Ty::Node) if node(t) => format!("{c}.map(NodeType::into_node)"),
            (Ty::Boxed(t), Ty::Owned(u)) if t == u => format!("crate::actions::pointer({c})?"),
            (Ty::Node, Ty::Boxed(t)) if node(t) => {
                format!("crate::actions::castNode::<{t}>({c})?")
            }
            (Ty::Node, Ty::Owned(t)) if node(t) => {
                format!("crate::actions::pointer(crate::actions::castNode::<{t}>({c})?)?")
            }
            (Ty::Node, Ty::List) => format!("crate::actions::castList({c})?"),
            (Ty::List, Ty::Node) => format!("crate::actions::listNode({c})"),
            (Ty::Node, Ty::NodeRef) => format!("{c}.as_ref()"),
            // C has the same node in two places, which the Rust cannot have, so it copies it.
            (Ty::NodeRef, Ty::Node) => format!("{c}.cloned()"),
            (Ty::NodeRef, Ty::List) => format!("crate::actions::castList({c}.cloned())?"),
            (Ty::NodeRef, Ty::Boxed(t)) if node(t) => {
                format!("crate::actions::castNode::<{t}>({c}.cloned())?")
            }
            (Ty::String, Ty::Str) => format!("&{c}"),
            (Ty::List, Ty::ListRef) => format!("&{c}"),
            _ => return fail(format!("a {ty:?} as a {want:?}")),
        })
    }

    /// A test of a value: `NULL`, `NIL`, `false` or zero is false.
    fn test(&mut self, e: &Expr, negate: bool) -> Result<String, String> {
        let value = self.borrow(e)?;
        let c = value.code;
        Ok(match (value.ty, negate) {
            (Ty::Bool, false) => c,
            (Ty::Bool, true) => format!("!{c}"),
            (Ty::Text | Ty::Node | Ty::NodeRef | Ty::Boxed(_), false) => format!("{c}.is_some()"),
            (Ty::Text | Ty::Node | Ty::NodeRef | Ty::Boxed(_), true) => format!("{c}.is_none()"),
            (Ty::List, false) => format!("!{c}.is_empty()"),
            (Ty::List, true) => format!("{c}.is_empty()"),
            (Ty::Int(_) | Ty::Char, false) => format!("{c} != 0"),
            (Ty::Int(_) | Ty::Char, true) => format!("{c} == 0"),
            (ty, _) => return fail(format!("a test of a {ty:?}")),
        })
    }

    /// The text of a value, as an `Option<&str>`.
    fn text(&mut self, e: &Expr) -> Result<String, String> {
        let value = self.borrow(e)?;
        Ok(match value.ty {
            Ty::Text => format!("{}.as_deref()", value.code),
            Ty::Keyword | Ty::Literal => format!("Some({})", value.code),
            ty => return fail(format!("strcmp of a {ty:?}")),
        })
    }

    fn condition(&mut self, e: &Expr, negate: bool) -> Result<String, String> {
        match e {
            Expr::Not(inner) => self.condition(inner, !negate),
            Expr::Binary(op @ ("&&" | "||"), left, right) => {
                let left = self.condition(left, negate)?;
                let right = self.condition(right, negate)?;
                // De Morgan: a negated `&&` is an `||` of the negations.
                let and = (*op == "&&") != negate;
                if and {
                    let wrap = |c: String| if c.contains(" || ") { format!("({c})") } else { c };
                    Ok(format!("{} && {}", wrap(left), wrap(right)))
                } else {
                    Ok(format!("{left} || {right}"))
                }
            }
            Expr::Binary(op @ ("==" | "!="), left, right) => {
                let equal = (*op == "==") != negate;
                if let (Expr::Call(name, arguments), Expr::Number(0)) = (&**left, &**right)
                    && name == "strcmp"
                    && let [a, b] = &arguments[..]
                {
                    let (a, b) = (self.text(a)?, self.text(b)?);
                    return Ok(format!("{a} {} {b}", if equal { "==" } else { "!=" }));
                }
                let null = |e: &Expr| matches!(e, Expr::Name(n) if n == "NULL" || n == "NIL");
                if null(right) {
                    return self.test(left, equal);
                }
                if null(left) {
                    return self.test(right, equal);
                }
                let left = self.borrow(left)?;
                let right = self.borrow(right)?;
                let (left, right) = match (&left.ty, &right.ty) {
                    (Ty::Number, Ty::Number) => return fail("a comparison of two numbers"),
                    (Ty::Number, ty) => {
                        let ty = ty.clone();
                        (self.coerce(left, &ty)?, right.code)
                    }
                    (ty, _) => {
                        let ty = ty.clone();
                        if !matches!(ty, Ty::Int(_) | Ty::Enum(_) | Ty::Char | Ty::Bool) {
                            return fail(format!("a comparison of a {ty:?}"));
                        }
                        (left.code, self.coerce(right, &ty)?)
                    }
                };
                Ok(format!("{left} {} {right}", if equal { "==" } else { "!=" }))
            }
            Expr::Binary(op @ ("<" | ">" | "<=" | ">="), left, right) => {
                let op = if negate {
                    match *op {
                        "<" => ">=",
                        ">" => "<=",
                        "<=" => ">",
                        _ => "<",
                    }
                } else {
                    op
                };
                let left = self.borrow(left)?;
                let right = self.borrow(right)?;
                let ty = if left.ty == Ty::Number { right.ty.clone() } else { left.ty.clone() };
                if !matches!(ty, Ty::Int(_)) {
                    return fail(format!("an order of a {ty:?}"));
                }
                Ok(format!("{} {op} {}", self.coerce(left, &ty)?, self.coerce(right, &ty)?))
            }
            _ => self.test(e, negate),
        }
    }

    fn block(&mut self, statements: &[Stmt]) -> Result<(), String> {
        let mut i = 0;
        while i < statements.len() {
            // `ereport(ERROR)` does not return, so the statements after it never run.
            if self.path.diverges {
                break;
            }
            i += self.statement(&statements[i..])?;
            self.statement.clear();
        }
        Ok(())
    }

    /// Translates the first statement of `statements`, and gives the number of statements that it takes: a declaration of a new node takes the assignments to its fields that follow.
    fn statement(&mut self, statements: &[Stmt]) -> Result<usize, String> {
        match &statements[0] {
            Stmt::Block(block) => self.block(block)?,
            Stmt::Declare { ctype, name, init: Some(init) } => {
                return self.declare(ctype, name, init, statements);
            }
            Stmt::Declare { ctype, name, init: None } => {
                // `T name; name = value;` is a declaration with the value.
                if let Some(Stmt::Assign(Expr::Name(target), init)) = statements.get(1)
                    && target == name
                {
                    let taken = self.declare(ctype, name, init, &statements[1..])?;
                    return Ok(taken + 1);
                }
                // A scalar that the C writes through a pointer before it reads it gets the zero value, which the Rust needs and the C never reads.
                let zero = match scalar(ctype) {
                    Some(Ty::Bool) => Expr::Name("false".into()),
                    Some(_) => Expr::Number(0),
                    None => return fail("a declaration with no value"),
                };
                return self.declare(ctype, name, &zero, statements);
            }
            Stmt::Assign(Expr::Result, e) => {
                let Some(slot) = self.result else { return fail("$$ with no type") };
                let value = self.value(e)?;
                let code = self.coerce(value, &slot.ty)?;
                self.line(format!("result = {code};"));
                self.path.assigned = true;
            }
            Stmt::Assign(Expr::Name(name), e) => {
                let Some(ty) = self.locals.get(name).cloned() else {
                    return fail(format!("an assignment to {name}"));
                };
                let value = self.value(e)?;
                let code = self.coerce(value, &ty)?;
                self.line(format!("{name} = {code};"));
                self.mutable.insert(name.clone());
                self.forget(name);
            }
            Stmt::Assign(target @ Expr::Field(base, field), e) => {
                self.target = describe(target);
                let value = self.value(e);
                self.target = None;
                let value = value?;
                let (place, structure) = self.place(base)?;
                let (rust, ty) = self.field(&structure, field)?;
                let code = self.coerce(value, &ty)?;
                self.line(format!("{place}.{rust} = {code};"));
                if let Some(name) = describe(target) {
                    self.forget(&name);
                }
                self.moved_into(base, field, &ty, e);
            }
            Stmt::Assign(..) => return fail("an assignment to an expression"),
            Stmt::Expr(Expr::Call(name, arguments)) if name == "ereport" => {
                match self.ereport(arguments)? {
                    Report::Error(error) => {
                        self.line(format!("return Err({error});"));
                        self.path.diverges = true;
                    }
                    Report::Warning(warning) => self.line(format!("{warning};")),
                }
            }
            Stmt::Expr(Expr::Call(name, arguments)) if name == "parser_yyerror" => {
                let [Expr::Text(message)] = &arguments[..] else {
                    return fail("parser_yyerror of an expression");
                };
                self.line(format!("return Err(p.yyerror(\"{message}\"));"));
                self.path.diverges = true;
            }
            Stmt::Expr(e @ Expr::Call(..)) => {
                let value = self.value(e)?;
                if value.ty != Ty::Unit {
                    return fail("a call whose value is not used");
                }
                self.line(format!("{};", value.code));
            }
            Stmt::Expr(_) => return fail("an expression statement"),
            Stmt::If(condition, then, otherwise) => {
                self.branches(condition, then, otherwise, false, false)?
            }
        }
        Ok(1)
    }

    /// Translates `statements` that [`tail_shaped`] accepts, so that the value of `$$` is the value of the last line.
    fn tail_block(&mut self, statements: &[Stmt]) -> Result<(), String> {
        let Some((last, rest)) = statements.split_last() else { return fail("an empty block") };
        self.block(rest)?;
        if self.path.diverges {
            return Ok(());
        }
        match last {
            Stmt::Assign(Expr::Result, e) => {
                let Some(slot) = self.result else { return fail("$$ with no type") };
                let value = self.value(e)?;
                let code = self.coerce(value, &slot.ty)?;
                self.line(code);
                self.statement.clear();
                self.path.assigned = true;
            }
            Stmt::If(condition, then, otherwise) => {
                self.branches(condition, then, otherwise, false, true)?;
            }
            Stmt::Block(block) => self.tail_block(block)?,
            _ => return fail("a last statement that does not set $$"),
        }
        Ok(())
    }

    /// The statements of a branch of an `if`. In a `tail` `if`, a branch that does not end in an error gives the value of `$$`.
    fn branch(&mut self, statements: &[Stmt], tail: bool) -> Result<(), String> {
        if tail && !diverging(statements) {
            self.tail_block(statements)
        } else {
            self.block(statements)
        }
    }

    /// `if`, or `else if` when `chained`. A `tail` `if` is an expression that gives `$$`.
    fn branches(
        &mut self,
        condition: &Expr,
        then: &[Stmt],
        otherwise: &[Stmt],
        chained: bool,
        tail: bool,
    ) -> Result<(), String> {
        let condition = self.condition(condition, false)?;
        self.statement.clear();
        if chained {
            let last = self.lines.pop().ok_or("an else with no if")?;
            self.lines.push(format!("{last} else if {condition} {{"));
        } else {
            self.line(format!("if {condition} {{"));
        }
        let before = self.path.clone();
        // The uses in one branch do not come after the uses in the other.
        let later = self.later.clone();
        subtract(&mut self.later, otherwise);
        self.depth += 1;
        self.branch(then, tail)?;
        self.depth -= 1;
        let after_then = std::mem::replace(&mut self.path, before);
        self.later = later;
        subtract(&mut self.later, then);
        self.line("}");
        match otherwise {
            [] => {}
            [Stmt::If(condition, then, otherwise)] => {
                self.branches(condition, then, otherwise, true, tail)?;
            }
            _ => {
                let last = self.lines.pop().ok_or("an else with no if")?;
                self.lines.push(format!("{last} else {{"));
                self.depth += 1;
                self.branch(otherwise, tail)?;
                self.depth -= 1;
                self.line("}");
            }
        }
        let after_else = &self.path;
        subtract(&mut self.later, otherwise);
        let done = |p: &Path| p.assigned || p.diverges;
        let aliases = after_then
            .aliases
            .iter()
            .filter(|(name, alias)| after_else.aliases.get(*name) == Some(alias))
            .map(|(name, alias)| (name.clone(), alias.clone()))
            .collect();
        self.path = Path {
            moved: after_then.moved.union(&after_else.moved).cloned().collect(),
            aliases,
            assigned: done(&after_then) && done(after_else),
            diverges: after_then.diverges && after_else.diverges,
        };
        Ok(())
    }

    fn declare(
        &mut self,
        ctype: &str,
        name: &str,
        init: &Expr,
        statements: &[Stmt],
    ) -> Result<usize, String> {
        let reserved = |n: &str| {
            TAKEN.contains(&n)
                || RUST_KEYWORDS.contains(&n)
                || (n.starts_with('v') && n[1..].bytes().all(|b| b.is_ascii_digit()))
        };
        if reserved(name) || self.locals.contains_key(name) {
            return fail(format!("the local {name}"));
        }
        let ty = match (scalar(ctype), ctype) {
            (Some(ty), _) => ty,
            (None, "Node*") => Ty::Node,
            (None, "List*") => Ty::List,
            (None, "char*") => Ty::Text,
            (None, t) if self.context.enums.contains(t) => Ty::Enum(t.to_string()),
            (None, t) => match t.strip_suffix('*') {
                Some(s) if self.context.structs.contains_key(s) => Ty::Owned(s.to_string()),
                _ => return fail(format!("a local of the type {t}")),
            },
        };
        // A new struct takes the assignments to its fields that follow, as a struct literal.
        if let Expr::Call(make, arguments) = init
            && matches!(make.as_str(), "makeNode" | "palloc_object" | "palloc0_object")
            && let [Expr::Name(structure)] = &arguments[..]
            && ty == Ty::Owned(structure.clone())
        {
            let mut fields: Vec<(String, String)> = Vec::new();
            let mut taken = 1;
            for statement in &statements[1..] {
                let Stmt::Assign(Expr::Field(base, field), e) = statement else { break };
                let Expr::Name(base) = &**base else { break };
                if base != name {
                    break;
                }
                let (rust, ty) = self.field(structure, field)?;
                if fields.iter().any(|(f, _)| *f == rust) {
                    break;
                }
                let value = self.value(e)?;
                let code = self.coerce(value, &ty)?;
                self.statement.clear();
                self.moved_into(&Expr::Name(name.to_string()), field, &ty, e);
                fields.push((rust, code));
                taken += 1;
            }
            let count = self.context.structs[structure].len();
            let code = if fields.is_empty() {
                format!("{structure}::default()")
            } else {
                let mut parts: Vec<String> = fields
                    .into_iter()
                    .map(|(f, c)| if f == c { f } else { format!("{f}: {c}") })
                    .collect();
                if parts.len() < count {
                    parts.push("..Default::default()".into());
                }
                format!("{structure} {{ {} }}", parts.join(", "))
            };
            self.declared.insert(name.to_string(), self.lines.len());
            self.line(format!("let {name} = {code};"));
            self.locals.insert(name.to_string(), Ty::Struct(structure.clone()));
            return Ok(taken);
        }
        let value = self.value(init)?;
        let ty = match (&value.ty, &ty) {
            (Ty::Struct(v), Ty::Owned(s)) if v == s => value.ty.clone(),
            _ => ty,
        };
        let code = self.coerce(value, &ty)?;
        self.declared.insert(name.to_string(), self.lines.len());
        self.line(format!("let {name} = {code};"));
        self.locals.insert(name.to_string(), ty);
        Ok(1)
    }

    /// The Rust of the error of `ereport(ERROR, ...)` or of the notice of `ereport(WARNING, ...)`.
    fn ereport(&mut self, arguments: &[Expr]) -> Result<Report, String> {
        let [Expr::Name(level), rest @ ..] = arguments else { return fail("an ereport") };
        if level != "ERROR" && level != "WARNING" {
            return fail(format!("an ereport of {level}"));
        }
        let mut parts = Vec::new();
        for argument in rest {
            match argument {
                Expr::Comma(list) => parts.extend(list.iter().cloned()),
                other => parts.push(other.clone()),
            }
        }
        let (mut code, mut message, mut location) = (None, None, None);
        let mut fields = String::new();
        for part in &parts {
            let Expr::Call(name, arguments) = part else { return fail("an ereport part") };
            match (name.as_str(), &arguments[..]) {
                ("errcode", [e]) => {
                    let value = self.value(e)?;
                    if value.ty != Ty::Keyword {
                        return fail("an errcode that is not a constant");
                    }
                    code = Some(value.code);
                }
                ("errmsg", [Expr::Text(format), arguments @ ..]) => {
                    let text = self.format(format, arguments)?;
                    message = Some(match text.ty {
                        Ty::Literal => text.code,
                        _ => format!("&{}", text.code),
                    });
                }
                ("errdetail", [Expr::Text(text)]) => {
                    fields.push_str(&format!("detail: Some(\"{text}\"), "));
                }
                ("errhint", [Expr::Text(text)]) => {
                    fields.push_str(&format!("hint: Some(\"{text}\"), "));
                }
                ("parser_errposition", [e]) => {
                    let value = self.value(e)?;
                    location = Some(self.coerce(value, &Ty::Int("i32"))?);
                }
                (name, _) => return fail(format!("the ereport part {name}")),
            }
        }
        let location = location.unwrap_or_else(|| "-1".into());
        if level == "WARNING" {
            let (None, Some(message), true) = (code, message, fields.is_empty()) else {
                return fail("a warning with an errcode, an errdetail or an errhint");
            };
            return Ok(Report::Warning(format!("p.warning({message}, {location})")));
        }
        let (Some(code), Some(message)) = (code, message) else {
            return fail("an ereport with no errcode or no errmsg");
        };
        let error = format!("p.error({code}, {message}, {location})");
        Ok(Report::Error(if fields.is_empty() {
            error
        } else {
            format!("Error {{ {fields}..{error} }}")
        }))
    }

    /// The text of `errmsg` or `psprintf`: a literal, or a `format!` of `%s` and `%d`.
    fn format(&mut self, format: &str, arguments: &[Expr]) -> Result<Typed, String> {
        let mut out = String::new();
        let mut count = 0;
        let mut chars = format.chars();
        while let Some(c) = chars.next() {
            match c {
                '%' => match chars.next() {
                    Some('%') => out.push('%'),
                    Some('s' | 'd' | 'u' | 'i') => {
                        out.push_str("{}");
                        count += 1;
                    }
                    _ => return fail("a format that the translator does not know"),
                },
                '{' => out.push_str("{{"),
                '}' => out.push_str("}}"),
                c => out.push(c),
            }
        }
        if count != arguments.len() {
            return fail("a format with the wrong number of arguments");
        }
        if count == 0 {
            return Ok(Typed::new(format!("\"{format}\""), Ty::Literal));
        }
        let mut codes = Vec::new();
        for argument in arguments {
            let value = self.borrow(argument)?;
            codes.push(match value.ty {
                Ty::Text => format!("{}.as_deref().unwrap_or_default()", value.code),
                Ty::Keyword | Ty::Literal | Ty::String | Ty::Int(_) | Ty::Number => value.code,
                ty => return fail(format!("a format argument of a {ty:?}")),
            });
        }
        Ok(Typed::new(format!("format!(\"{out}\", {})", codes.join(", ")), Ty::String))
    }
}

/// The base of an argument `&mut base.field` or `Some(&mut base.field)` when the base is a cast or a pointer, which ends in `?`.
fn out_base(code: &str) -> Option<&str> {
    let code = code.strip_prefix("Some(").and_then(|c| c.strip_suffix(')')).unwrap_or(code);
    let (base, field) = code.strip_prefix("&mut ")?.rsplit_once('.')?;
    let simple = field.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    (simple && base.ends_with(")?")).then_some(base)
}

/// Takes the uses of the values in `statements` from `later`. The uses of a branch that the translation leaves out stay in `later` until the end of the `if`, where they no longer come later.
fn subtract(later: &mut BTreeMap<usize, usize>, statements: &[Stmt]) {
    for statement in statements {
        statement.walk(&mut |e, _| {
            if let Expr::Value(k) = e
                && let Some(n) = later.get_mut(k)
            {
                *n = n.saturating_sub(1);
            }
        });
    }
}

impl Stmt {
    /// Calls `f` on each expression in the statement and in the expressions in it, with `true` for a place that the statement changes: the left side of an assignment, or the operand of `&`.
    fn walk(&self, f: &mut impl FnMut(&Expr, bool)) {
        match self {
            Stmt::Declare { init, .. } => init.iter().for_each(|e| e.walk(false, f)),
            Stmt::Assign(left, right) => {
                left.walk(true, f);
                right.walk(false, f);
            }
            Stmt::Expr(e) => e.walk(false, f),
            Stmt::If(condition, then, otherwise) => {
                condition.walk(false, f);
                then.iter().chain(otherwise).for_each(|s| s.walk(f));
            }
            Stmt::Block(block) => block.iter().for_each(|s| s.walk(f)),
        }
    }
}

impl Expr {
    fn walk(&self, write: bool, f: &mut impl FnMut(&Expr, bool)) {
        f(self, write);
        match self {
            Expr::Cast(_, e) | Expr::Field(e, _) => e.walk(write, f),
            Expr::Address(e) => e.walk(true, f),
            Expr::Not(e) | Expr::Negative(e) | Expr::Complement(e) => e.walk(false, f),
            Expr::Binary(_, left, right) => {
                left.walk(false, f);
                right.walk(false, f);
            }
            Expr::Conditional(a, b, c) => [a, b, c].iter().for_each(|e| e.walk(false, f)),
            Expr::Call(_, list) | Expr::Comma(list) => list.iter().for_each(|e| e.walk(false, f)),
            Expr::Value(_)
            | Expr::Location(_)
            | Expr::Result
            | Expr::Here
            | Expr::Number(_)
            | Expr::Text(_)
            | Expr::Char(_)
            | Expr::Name(_) => {}
        }
    }
}

/// The name of a value, a local or a field of one, as `v1.rel.relname`, for the moves.
fn describe(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(k) => Some(format!("v{k}")),
        Expr::Name(name) => Some(name.clone()),
        Expr::Result => Some("result".into()),
        Expr::Field(base, field) => Some(format!("{}.{field}", describe(base)?)),
        _ => None,
    }
}

/// The number of the assignments to `$$` in `statements`.
fn assignments(statements: &[Stmt]) -> usize {
    statements
        .iter()
        .map(|s| match s {
            Stmt::Assign(Expr::Result, _) => 1,
            Stmt::If(_, then, otherwise) => assignments(then) + assignments(otherwise),
            Stmt::Block(block) => assignments(block),
            _ => 0,
        })
        .sum()
}

/// `statements` end in an error, and do not set `$$`.
fn diverging(statements: &[Stmt]) -> bool {
    assignments(statements) == 0
        && matches!(
            statements.last(),
            Some(Stmt::Expr(Expr::Call(name, _))) if name == "ereport" || name == "parser_yyerror"
        )
}

/// `statements` set `$$` only at their end: in the last statement, or at the end of each branch of a last `if` with an `else`, where a branch can end in an error instead.
fn tail_shaped(statements: &[Stmt]) -> bool {
    let Some((last, rest)) = statements.split_last() else { return false };
    if assignments(rest) != 0 {
        return false;
    }
    match last {
        Stmt::Assign(Expr::Result, _) => true,
        Stmt::If(_, then, otherwise) => {
            let branch = |b: &[Stmt]| tail_shaped(b) || diverging(b);
            !otherwise.is_empty()
                && branch(then)
                && branch(otherwise)
                && !(diverging(then) && diverging(otherwise))
        }
        Stmt::Block(block) => tail_shaped(block),
        _ => false,
    }
}

/// Translates the C of an action, without its braces and its comments. `values` has the slot of each symbol of the right side, and `result` the slot of the left side. The Rust is statements, one on each line, and then an expression of type `Value`, with the values that it uses.
pub(super) fn translate(
    context: &Context,
    body: &str,
    values: &[Option<Slot>],
    result: Option<&Slot>,
) -> Result<(String, BTreeSet<usize>), String> {
    let mut parse = Parse { tokens: tokens(body)?, at: 0 };
    let statements = parse.statements()?;
    if parse.peek().is_some() {
        return fail("a } with no {");
    }
    let mut translator = Translator {
        context,
        values,
        result,
        locals: HashMap::new(),
        declared: HashMap::new(),
        opened: BTreeSet::new(),
        mutable: BTreeSet::new(),
        path: Path::default(),
        statement: BTreeSet::new(),
        lines: Vec::new(),
        depth: 0,
        late: false,
        later: BTreeMap::new(),
        written: BTreeSet::new(),
        target: None,
    };
    for statement in &statements {
        statement.walk(&mut |e, write| {
            if let Expr::Value(k) = e {
                *translator.later.entry(*k).or_default() += 1;
            }
            if write && let Some(place) = describe(e) {
                translator.written.insert(place);
            }
        });
    }
    let count = assignments(&statements);
    let tail = result.is_some() && tail_shaped(&statements);
    translator.late = count > 0 && !tail;
    if let (true, Some(slot), Some((last, rest))) = (tail, result, statements.split_last()) {
        // The last statement gives the value of `$$`, which is the value of the block.
        translator.block(rest)?;
        // After an error the last statement never runs.
        if !translator.path.diverges {
            let start = translator.lines.len();
            translator.tail_block(std::slice::from_ref(last))?;
            let lines = &mut translator.lines;
            lines[start] = format!("Value::{}({}", slot.member, lines[start]);
            if let Some(end) = lines.last_mut() {
                end.push(')');
            }
        }
    } else {
        translator.block(&statements)?;
    }
    let Translator { path, mut lines, opened, mutable, declared, late, .. } = translator;
    if !path.diverges && result.is_some() && !path.assigned {
        return fail("a path that does not set $$");
    }
    for (name, &line) in &declared {
        if mutable.contains(name) {
            lines[line] = lines[line].replacen("let ", "let mut ", 1);
        }
    }
    let mut out = Vec::new();
    for &k in &opened {
        let slot = translator_slot(values, k)?;
        let mutable = if mutable.contains(&format!("v{k}")) { "mut " } else { "" };
        out.push(format!("let {mutable}v{k} = v{k}.into_{}()?;", slot.member));
    }
    if late && let Some(slot) = result {
        let mutable = if mutable.contains("result") { "mut " } else { "" };
        out.push(format!("let {mutable}result: {};", slot.rust));
    }
    out.extend(lines);
    if path.diverges {
        // The last line returns, and it is the value of the block without its `;`.
        if let Some(last) = out.last_mut()
            && let Some(stripped) = last.strip_suffix(';')
        {
            *last = stripped.to_string();
        }
    } else if !tail {
        out.push(match result {
            Some(slot) => format!("Value::{}(result)", slot.member),
            None => "Value::None".into(),
        });
    }
    Ok((out.join("\n"), opened))
}

fn translator_slot(values: &[Option<Slot>], k: usize) -> Result<&Slot, String> {
    values.get(k - 1).and_then(Option::as_ref).ok_or_else(|| format!("${k} has no type"))
}

#[cfg(test)]
mod tests {
    use super::{Context, Kind, Slot, Ty, out_base, translate};

    const FUNCTIONS: &str = "
pub(crate) fn take(list: List) -> List {
pub(crate) fn peek(list: &List) -> Option<&Node> {
pub(crate) fn named(name: Option<Str>) -> Node {
pub(crate) fn flags(a: &mut bool, b: &mut bool, yyscanner: &Parser<'_>) -> Result<(), Error> {
pub(crate) fn wrap(node: Node) -> List {
";

    fn context() -> Context {
        let mut context = Context::default();
        let fields = [
            ("name", Kind::Text),
            ("args", Kind::List),
            ("more", Kind::List),
            ("flag", Kind::Bool),
            ("other", Kind::Bool),
            ("kind", Kind::Enum("Kind".into())),
            ("location", Kind::Location),
        ];
        context.add_struct("Thing", fields);
        context.nodes.insert("Thing".into());
        context.enums.insert("Kind".into());
        let syntax = ("ERRCODE_SYNTAX_ERROR".to_string(), Ty::Keyword);
        context.constants.insert("ERRCODE_SYNTAX_ERROR".into(), syntax);
        context.add_functions(FUNCTIONS);
        context
    }

    fn slot(member: &str, rust: &str, ty: Ty) -> Option<Slot> {
        Some(Slot { member: member.into(), rust: rust.into(), ty })
    }

    /// The Rust of an action on the values `$1 List`, `$2 char *` and `$3 int`, with a `Node *` for `$$`.
    fn translated(body: &str) -> Result<String, String> {
        let values = [
            slot("list", "List", Ty::List),
            slot("str", "Option<Str>", Ty::Text),
            slot("ival", "i32", Ty::Int("i32")),
        ];
        let result = slot("node", "Option<Node>", Ty::Node);
        translate(&context(), body, &values, result.as_ref()).map(|(rust, _)| rust)
    }

    #[test]
    fn a_new_node_is_a_struct_literal() {
        let body = "Thing *n = makeNode(Thing); n->args = $1; n->location = @1; $$ = (Node *) n;";
        let rust = translated(body).expect("the action translates");
        assert!(
            rust.contains("let n = Thing { args: v1, location: at[0], ..Default::default() };")
        );
        assert!(rust.ends_with("Value::node(Some(n.into()))"));
    }

    #[test]
    fn an_error_has_its_detail() {
        let body = "if ($3 == 0) ereport(ERROR, (errcode(ERRCODE_SYNTAX_ERROR), errmsg(\"zero\"), \
                    errdetail(\"Not zero.\"), parser_errposition(@3))); $$ = NULL;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains(
            "return Err(Error { detail: Some(\"Not zero.\"), \
             ..p.error(ERRCODE_SYNTAX_ERROR, \"zero\", at[2]) });"
        ));
    }

    #[test]
    fn a_warning_does_not_end_the_action() {
        let body = "ereport(WARNING, (errmsg(\"old\"), parser_errposition(@2))); $$ = NULL;";
        assert_eq!(
            translated(body).as_deref(),
            Ok("p.warning(\"old\", at[1]);\nValue::node(None)")
        );
    }

    #[test]
    fn the_statements_after_an_error_are_left_out() {
        let body = "ereport(ERROR, (errcode(ERRCODE_SYNTAX_ERROR), errmsg(\"no\"))); $$ = NULL;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.starts_with("return Err(p.error(ERRCODE_SYNTAX_ERROR, \"no\", -1))"));
        assert!(!rust.contains("Value::"));
    }

    #[test]
    fn a_cast_to_an_enum_converts_the_integer() {
        let body = "Thing *n = makeNode(Thing); n->kind = (Kind) $3; $$ = (Node *) n;";
        assert!(translated(body).expect("the action translates").contains("kind: Kind(v3)"));
    }

    #[test]
    fn a_declaration_with_no_value_gets_the_value_after_it_or_zero() {
        let body = "Thing *n = makeNode(Thing); bool dummy; flags(&n->flag, &dummy, yyscanner); \
                    $$ = (Node *) n;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains("let mut dummy = false;"));
        assert!(rust.contains("crate::actions::flags(&mut n.flag, &mut dummy, p)?;"));
        let body = "Thing *n; n = makeNode(Thing); $$ = (Node *) n;";
        assert!(
            translated(body)
                .expect("the action translates")
                .starts_with("let n = Thing::default();")
        );
        assert!(translated("Thing *n; $$ = NULL;").is_err());
    }

    #[test]
    fn a_borrowed_node_is_tested_for_null() {
        let body = "if (peek($1)) $$ = NULL; else $$ = named($2);";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains("if crate::actions::peek(&v1).is_some() {"));
    }

    #[test]
    fn a_value_that_is_used_after_a_move_is_copied() {
        let body = "Thing *n = makeNode(Thing); n->args = take($1); n->more = $1; $$ = (Node *) n;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains("args: crate::actions::take(v1.clone()), more: v1,"));
        // The copy would not show a change through the pointer that C has in two places.
        let body = "Thing *n = makeNode(Thing); n->args = take($1); n->more = $1; \
                    flags(&$1->flag, &n->other, yyscanner); $$ = (Node *) n;";
        assert!(translated(body).is_err());
    }

    #[test]
    fn a_field_of_a_local_that_a_function_takes_is_copied() {
        let body = "Thing *n = makeNode(Thing); n->name = $2; flags(&n->flag, &n->other, yyscanner); \
                    n->args = wrap(named(n->name)); $$ = (Node *) n;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains("crate::actions::named(n.name.clone())"));
    }

    #[test]
    fn two_places_in_one_node_bind_the_node_once() {
        let body = "Thing *n = makeNode(Thing); flags(&n->flag, &n->other, yyscanner); \
                    $$ = (Node *) n;";
        let rust = translated(body).expect("the action translates");
        assert!(rust.contains("crate::actions::flags(&mut n.flag, &mut n.other, p)?;"));
        let cast = "crate::actions::castMut::<Thing>(&mut v1)?";
        assert_eq!(out_base(&format!("&mut {cast}.flag")), Some(cast));
        assert_eq!(out_base(&format!("Some(&mut {cast}.flag)")), Some(cast));
        assert_eq!(out_base("&mut n.flag"), None);
        assert_eq!(out_base(&format!("&mut {cast}.a.b()")), None);
    }

    #[test]
    fn unknown_c_is_refused_with_the_reason() {
        assert_eq!(
            translated("switch ($3) { case 1: break; }"),
            Err("the C word switch".to_string())
        );
    }
}

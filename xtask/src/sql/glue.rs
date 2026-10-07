//! The glue between the parse tables and the actions of `gram.y`.
//!
//! The parser keeps a stack of values, one for each symbol, as the parser of bison does. The value of a symbol has the type of the `%union` member that its `%token` or `%type` tag names. This generator writes that union as a Rust enum, `Value`, and writes `reduce`, which runs the action of a rule on the values of its right side and gives the value of its left side.
//!
//! An action that only moves a value or makes a constant, for example `{ $$ = $1; }`, `{ $$ = NIL; }` or `{ $$ = lappend($1, $3); }`, is written into `reduce` here. The translator of `translate.rs` writes most other actions into `reduce` too. An action that it cannot write is a method of a trait, one trait for each rule of `gram.y`, with the values that the action uses as typed arguments. The default of each method gives the error that the action is not ported. `src/actions` has the ported methods, with the C of `gram.y` ported line by line.
//!
//! The plan is `spec/07-sql-types-and-catalog.md` section 7.2.
//!
//! Lifted from `xtask/src/postgres/glue.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use super::gram::{self, Grammar};
use super::nodes::{self, Headers, Model, RUST_KEYWORDS};
use super::translate::{self, Context, Slot, Ty};

/// One member of `%union`.
struct Member {
    name: String,
    /// The Rust type of a value of the member.
    rust: String,
    /// The same type with paths, for the module `rules`, where a trait can have the name of a node type.
    qualified: String,
    /// How a value of another member converts to this member, as C converts one pointer type to another.
    convert: Convert,
}

/// The conversions to a member.
#[derive(Clone, PartialEq, Eq)]
enum Convert {
    /// Only a value of the same member.
    Exact,
    /// `Node *`: a value of any member that points to a node.
    Node,
    /// `List *`: a list, or a node that is a list or `NULL`.
    List,
    /// A pointer to a node type: a node of that type, or `NULL`.
    Boxed(String),
}

/// The members of `%union`, in order, without `core_yystype`.
fn members(grammar: &Grammar, model: &Model, local: &Headers) -> Result<Vec<Member>, String> {
    let body = nodes::strip_comments(&grammar.union);
    let body = body.trim().trim_start_matches('{').trim_end_matches('}');
    let locals: BTreeSet<&str> = local.structs.iter().map(|s| s.name.as_str()).collect();
    let mut members = Vec::new();
    for declaration in body.split(';') {
        let declaration: String = declaration.split_whitespace().collect::<Vec<_>>().join(" ");
        if declaration.is_empty() {
            continue;
        }
        let split = declaration
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .ok_or_else(|| format!("%union: bad member {declaration}"))?;
        let name = declaration[split + 1..].to_string();
        let ctype = declaration[..=split].trim().replace(" *", "*");
        let ctype = ctype.strip_prefix("struct ").unwrap_or(&ctype).to_string();
        if ctype == "core_YYSTYPE" {
            continue;
        }
        let pointee = ctype.strip_suffix('*');
        let plain = |rust: &str, convert| (rust.to_string(), rust.to_string(), convert);
        let (rust, qualified, convert) = match ctype.as_str() {
            "int" => plain("i32", Convert::Exact),
            "char*" => plain("Option<Str>", Convert::Exact),
            // A keyword is a static string of `kwlist.h`.
            "const char*" => plain("&'static str", Convert::Exact),
            "char" => plain("u8", Convert::Exact),
            "bool" => plain("bool", Convert::Exact),
            "List*" => plain("List", Convert::List),
            "Node*" => plain("Option<Node>", Convert::Node),
            t if model.used_enums.contains(t) => {
                (t.to_string(), format!("crate::nodes::{t}"), Convert::Exact)
            }
            _ if pointee.is_some_and(|p| model.emitted.contains(p)) => {
                let pointee = pointee.unwrap_or_default().to_string();
                let qualified = format!("Option<Box<crate::nodes::{pointee}>>");
                (format!("Option<Box<{pointee}>>"), qualified, Convert::Boxed(pointee))
            }
            _ if pointee.is_some_and(|p| locals.contains(p)) => {
                let pointee = pointee.unwrap_or_default();
                let qualified = format!("Option<Box<super::{pointee}>>");
                (format!("Option<Box<{pointee}>>"), qualified, Convert::Exact)
            }
            t => return Err(format!("%union: the generator does not know the type {t}")),
        };
        members.push(Member { name, rust, qualified, convert });
    }
    Ok(members)
}

/// What an action uses: the values `$k` and the locations `@k` of its right side, from 1, and `@$`.
#[derive(Default)]
struct Uses {
    values: BTreeSet<usize>,
    locations: BTreeSet<usize>,
    here: bool,
}

/// The C of an action without its braces and its comments.
fn body(action: &str) -> String {
    let text = nodes::strip_comments(action);
    let text = text.trim();
    text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(text).to_owned()
}

/// The C of `body` without the contents of its string and character literals, so that a `$` in a message is not a value.
fn code(body: &str) -> String {
    let text = body;
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    for c in text.chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                    out.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
    }
    out
}

fn uses(code: &str) -> Result<Uses, String> {
    let mut uses = Uses::default();
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if (c == b'$' || c == b'@') && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'$' {
                if c == b'@' {
                    uses.here = true;
                }
                i += 2;
                continue;
            }
            if next == b'<' {
                return Err("an action uses $<tag>, which the generator does not know".to_string());
            }
            let digits = bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 {
                let k: usize = code[i + 1..i + 1 + digits].parse().map_err(|e| format!("{e}"))?;
                if c == b'$' {
                    uses.values.insert(k);
                } else {
                    uses.locations.insert(k);
                }
                i += 1 + digits;
                continue;
            }
        }
        i += 1;
    }
    Ok(uses)
}

/// A constant of the headers or of the prologue: its Rust expression and its type.
enum Constant {
    /// A constant of a C enum, with the name of the enum.
    Enum(String),
    /// A `#define`, with the Rust type of its value.
    Define(&'static str),
}

/// The action of a rule, as the generator writes it.
enum Action {
    /// The value of the left side: Rust statements, one on each line, and then an expression of type `Value`. `values` is the right side values that they use, by number from 1.
    Inline { expression: String, values: BTreeSet<usize> },
    /// A method of the trait of the rule.
    Method(Uses),
}

/// The text of `$k` in an expression, when the expression is only that.
fn value_number(text: &str) -> Option<usize> {
    let digits = text.strip_prefix('$')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())?
}

/// The arguments of a call `name(...)` in `text` with no spaces, when `text` is that call.
fn call<'a>(text: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let inner = text.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?;
    let mut arguments = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            // A `)` at depth 0 closes the call before the end, as in `f(a)+g(b)`.
            ')' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                arguments.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    (depth == 0).then_some(())?;
    arguments.push(&inner[start..]);
    Some(arguments)
}

/// The generator of the glue.
struct Glue<'a> {
    grammar: &'a Grammar,
    members: Vec<Member>,
    member_of: HashMap<String, usize>,
    constants: HashMap<String, Constant>,
    context: Context,
}

impl Glue<'_> {
    /// The member of a symbol, or `None` when it has no tag.
    fn member(&self, symbol: usize) -> Result<Option<&Member>, String> {
        match &self.grammar.tags[symbol] {
            None => Ok(None),
            Some(tag) => match self.member_of.get(tag.as_str()) {
                Some(&m) => Ok(Some(&self.members[m])),
                None => {
                    Err(format!("{} has the tag {tag}, which %union has not", self.name(symbol)))
                }
            },
        }
    }

    fn name(&self, symbol: usize) -> &str {
        &self.grammar.symbols[symbol]
    }

    /// The expression that converts `value`, a `Value` of `from`, to a `Value` of `to`.
    fn convert(&self, value: &str, from: &Member, to: &Member) -> Option<String> {
        if from.name == to.name {
            return Some(value.to_string());
        }
        let converts = match &to.convert {
            Convert::Exact => false,
            Convert::Node => from.convert != Convert::Exact,
            Convert::List | Convert::Boxed(_) => from.convert == Convert::Node,
        };
        converts.then(|| format!("Value::{}({value}.into_{}()?)", to.name, to.name))
    }

    /// The action of a rule as the generator writes it.
    fn action(&self, rule: &gram::Rule) -> Result<Action, String> {
        let lhs = self.member(rule.lhs)?;
        let Some(action) = &rule.action else {
            // Bison runs `$$ = $1` for an alternative without an action.
            let (Some(lhs), Some(&first)) = (lhs, rule.rhs.first()) else {
                return Ok(Action::Inline { expression: "Value::None".into(), values: [].into() });
            };
            let Some(from) = self.member(first)? else {
                return Err(format!("{}: $1 has no type for the default action", rule.name));
            };
            let expression = self
                .convert("v1", from, lhs)
                .ok_or_else(|| format!("{}: the default action has a type clash", rule.name))?;
            return Ok(Action::Inline { expression, values: [1].into() });
        };
        let body = body(action);
        let uses = uses(&code(&body))?;
        // The inline forms keep the literals, as `$$ = "+";` needs its text.
        let normal: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let inline = normal
            .strip_prefix("$$ = ")
            .and_then(|e| e.strip_suffix(';'))
            .filter(|e| !e.contains(';'))
            .zip(lhs)
            .and_then(|(e, lhs)| self.inline(rule, e, lhs).transpose());
        match inline {
            Some(Ok(expression)) => {
                let values = uses.values;
                Ok(Action::Inline { expression, values })
            }
            Some(Err(e)) => Err(e),
            None => match translate::translate(
                &self.context,
                &body,
                &self.slots(rule)?,
                lhs.map(|m| self.slot(m)).as_ref(),
            ) {
                Ok((expression, values)) => Ok(Action::Inline { expression, values }),
                Err(_) => Ok(Action::Method(uses)),
            },
        }
    }

    /// The slot of a value of a member, for the translator.
    fn slot(&self, member: &Member) -> Slot {
        let ty = Ty::of(&member.rust, &self.context);
        Slot { member: member.name.clone(), rust: member.rust.clone(), ty: ty.unwrap_or(Ty::Unit) }
    }

    /// The slots of the right side of a rule. A symbol with no type has none.
    fn slots(&self, rule: &gram::Rule) -> Result<Vec<Option<Slot>>, String> {
        rule.rhs.iter().map(|&s| Ok(self.member(s)?.map(|m| self.slot(m)))).collect()
    }

    /// The expression for `$$ = expression;`, or `None` when the action needs a method.
    fn inline(&self, rule: &gram::Rule, e: &str, lhs: &Member) -> Result<Option<String>, String> {
        let typed = |k: usize| -> Result<Option<&Member>, String> {
            match rule.rhs.get(k.wrapping_sub(1)) {
                Some(&symbol) => self.member(symbol),
                None => Err(format!("{}: ${k} is past the right side", rule.name)),
            }
        };
        let to = &lhs.name;
        let compact: String = e.chars().filter(|c| !c.is_whitespace()).collect();
        let single = compact.strip_prefix("(Node*)").unwrap_or(&compact);
        if let Some(k) = value_number(single) {
            let Some(from) = typed(k)? else { return Ok(None) };
            return Ok(self.convert(&format!("v{k}"), from, lhs));
        }
        let simple = match (e, lhs.rust.as_str()) {
            ("NIL", "List") => Some("Value::list(List::new())".to_string()),
            ("NULL", t) if t.starts_with("Option<") => Some(format!("Value::{to}(None)")),
            ("true" | "false", "bool") => Some(format!("Value::{to}({e})")),
            _ => None,
        };
        if simple.is_some() {
            return Ok(simple);
        }
        if let Ok(number) = e.parse::<i32>() {
            return Ok((lhs.rust == "i32").then(|| format!("Value::{to}({number})")));
        }
        if let Some(text) = e.strip_prefix('"').and_then(|t| t.strip_suffix('"'))
            && !text.contains(['"', '\\'])
        {
            return Ok(match lhs.rust.as_str() {
                "Option<Str>" => Some(format!("Value::{to}(Some(\"{text}\".into()))")),
                "&'static str" => Some(format!("Value::{to}(\"{text}\")")),
                _ => None,
            });
        }
        if let Some(constant) = self.constants.get(e) {
            return Ok(match (constant, lhs.rust.as_str()) {
                (Constant::Enum(t), rust) if t == rust => Some(format!("Value::{to}({t}::{e})")),
                (Constant::Enum(t), "i32") => Some(format!("Value::{to}({t}::{e}.0)")),
                (Constant::Define(t), rust) if *t == rust => Some(format!("Value::{to}({e})")),
                (Constant::Define("u8"), "i32") => Some(format!("Value::{to}(i32::from({e}))")),
                (Constant::Define("&str"), "Option<Str>") => {
                    Some(format!("Value::{to}(Some({e}.into()))"))
                }
                (Constant::Define("&str"), "&'static str") => Some(format!("Value::{to}({e})")),
                _ => None,
            });
        }
        // `$k` values that go into a list must point to nodes.
        let node = |k: usize| -> Result<Option<String>, String> {
            Ok(typed(k)?
                .filter(|m| m.convert != Convert::Exact)
                .map(|_| format!("v{k}.into_node()?")))
        };
        let numbers = |arguments: &[&str]| -> Option<Vec<usize>> {
            arguments.iter().map(|a| value_number(a)).collect()
        };
        // An item of a list: `$k`, `NULL`, `makeString($k)` or `makeString("text")`. An item `NIL` is an empty list, which is `NULL` too.
        let item = |argument: &str| -> Result<Option<String>, String> {
            if argument == "NULL" || argument == "NIL" {
                return Ok(Some("None".to_string()));
            }
            if let Some(k) = value_number(argument) {
                return node(k);
            }
            let Some(arguments) = call(argument, "makeString") else { return Ok(None) };
            let [k] = arguments[..] else { return Ok(None) };
            if let Some(text) = k.strip_prefix('"').and_then(|t| t.strip_suffix('"'))
                && !text.contains(['"', '\\'])
            {
                return Ok(Some(format!(
                    "Some(crate::actions::makeString(Some(\"{text}\".into())))"
                )));
            }
            let Some(k) = value_number(k) else { return Ok(None) };
            Ok(typed(k)?
                .filter(|m| m.rust == "Option<Str>")
                .map(|m| format!("Some(crate::actions::makeString(v{k}.into_{}()?))", m.name)))
        };
        let list = |argument: &str| -> Result<Option<usize>, String> {
            let k = value_number(argument);
            Ok(k.filter(|&k| typed(k).ok().flatten().is_some_and(|m| m.rust == "List")))
        };
        if lhs.rust == "List" {
            if let Some(arguments) = call(&compact, "lappend")
                && let [l, i] = arguments[..]
                && let Some(l) = list(l)?
                && let Some(i) = item(i)?
            {
                return Ok(Some(format!(
                    "let mut list = v{l}.into_list()?;\nlist.push({i});\nValue::list(list)"
                )));
            }
            if let Some(arguments) = call(&compact, "lcons")
                && let [i, l] = arguments[..]
                && let Some(l) = list(l)?
                && let Some(i) = item(i)?
            {
                return Ok(Some(format!(
                    "let mut list = v{l}.into_list()?;\nlist.insert(0, {i});\nValue::list(list)"
                )));
            }
            for name in ["list_make1", "list_make2"] {
                if let Some(arguments) = call(&compact, name) {
                    let items: Option<Vec<String>> =
                        arguments.iter().map(|a| item(a)).collect::<Result<_, _>>()?;
                    if let Some(items) = items {
                        return Ok(Some(format!("Value::list(vec![{}])", items.join(", "))));
                    }
                }
            }
        }
        if lhs.rust == "Option<Str>"
            && let Some(arguments) = call(&compact, "pstrdup")
            && let Some([k]) = numbers(&arguments).as_deref()
        {
            return Ok(match typed(*k)?.map(|m| m.rust.as_str()) {
                Some("&'static str") => {
                    Some(format!("Value::{to}(Some(v{k}.into_keyword()?.into()))"))
                }
                Some("Option<Str>") => Some(format!("v{k}")),
                _ => None,
            });
        }
        Ok(None)
    }
}

/// The type of a parameter or a result of a member in the module `rules`, or `()` for no member.
fn qualified(member: Option<&Member>) -> &str {
    member.map_or("()", |m| m.qualified.as_str())
}

/// The names of the traits that `src/actions` implements for `Parser`, from the text of its files.
fn ported(text: &str) -> BTreeSet<String> {
    let mut ported = BTreeSet::new();
    for (at, _) in text.match_indices("impl rules::") {
        let rest = &text[at + "impl rules::".len()..];
        let name: String =
            rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if rest[name.len()..].trim_start().starts_with("for Parser") {
            ported.insert(name);
        }
    }
    ported
}

/// Writes `src/generated/glue.rs` from the vendored headers, `gram.y` and the ported actions. The texts are, in order, the headers of [`nodes::HEADERS`], the [`nodes::DATA`] files, `gram.y` and the files of `src/actions`.
pub(super) fn glue(texts: &[String]) -> Result<String, String> {
    let model = nodes::model(&texts[..=nodes::GRAM])?;
    let ported = ported(&texts[nodes::GRAM + 1]);
    let grammar = gram::read(&texts[nodes::GRAM])?;
    let mut local = Headers::default();
    nodes::read_header(
        "the prologue of gram.y",
        &grammar.prologue,
        nodes::Read::Nodes,
        &mut local,
    )?;
    // A member that no symbol has is never a value, as `aind`.
    let tags: BTreeSet<&str> = grammar.tags.iter().flatten().map(String::as_str).collect();
    let members: Vec<Member> = members(&grammar, &model, &local)?
        .into_iter()
        .filter(|m| tags.contains(m.name.as_str()))
        .collect();
    let member_of = members.iter().enumerate().map(|(i, m)| (m.name.clone(), i)).collect();
    let mut constants = HashMap::new();
    for e in model.headers.enums.iter().filter(|e| model.used_enums.contains(&e.name)) {
        for (constant, _) in &e.constants {
            constants.insert(constant.clone(), Constant::Enum(e.name.clone()));
        }
    }
    let defines = model.headers.defines.iter().filter(|d| model.used_defines.contains(&d.name));
    for d in defines.chain(&local.defines) {
        constants.insert(d.name.clone(), Constant::Define(d.rust().0));
    }
    // What the translator knows: the fields of the structs, the constants and the functions.
    let mut context = Context::default();
    for (name, fields) in &model.layouts {
        context.add_struct(name, fields.iter().map(|(f, k)| (f.name.as_str(), k.clone())));
    }
    let locals: BTreeSet<String> = local.structs.iter().map(|s| s.name.clone()).collect();
    let pointees: BTreeSet<String> = model.emitted.union(&locals).cloned().collect();
    let mut local_kinds = Vec::new();
    for s in &local.structs {
        let mut kinds = Vec::new();
        for field in &s.fields {
            let kind = nodes::kind(&field.ctype, &pointees, &model.enum_names, &model.nodes)
                .map_err(|e| format!("{}.{}: {e}", s.name, field.name))?;
            kinds.push((field.name.as_str(), kind));
        }
        context.add_struct(&s.name, kinds.iter().cloned());
        local_kinds.push((s.name.as_str(), kinds));
    }
    context.nodes = model.emitted.clone();
    context.enums = model.used_enums.clone();
    for (name, constant) in &constants {
        context.constants.insert(
            name.clone(),
            match constant {
                Constant::Enum(e) => (format!("{e}::{name}"), Ty::Enum(e.clone())),
                Constant::Define(rust) => {
                    let ty = match *rust {
                        "u8" => Ty::Char,
                        "&str" | "SqlState" => Ty::Keyword,
                        "i64" => Ty::Int("i64"),
                        "u32" => Ty::Int("u32"),
                        _ => Ty::Int("i32"),
                    };
                    (name.clone(), ty)
                }
            },
        );
    }
    context.add_functions(&texts[nodes::GRAM + 1]);
    let glue = Glue { grammar: &grammar, members, member_of, constants, context };

    let mut out = String::new();
    out.push_str(
        "//! The values of the symbols of `gram.y` and the actions of its rules.\n\
         //!\n\
         //! @generated by `cargo xtask sql` from `vendor/postgres-19/src/backend/parser/gram.y`, the vendored headers and the ported actions in `src/actions`. Do not edit. `cargo xtask sql --check` runs in CI and fails if this file and its inputs disagree.\n\
         //!\n\
         //! [`Value`] has one variant for each member of `%union`. [`reduce`] runs the action of a rule. An action that only moves a value or makes a constant is in [`reduce`]. Every other action is a method of the trait of its rule in [`rules`], and `src/actions` has the methods that are ported.\n\
         \n\
         #![allow(non_camel_case_types, non_snake_case, missing_docs)]\n\
         \n\
         use std::vec::Drain;\n\
         \n\
         use crate::actions::{Parser, take};\n\
         use crate::error::Error;\n\
         use crate::nodes::*;\n",
    );

    // The structs and the constants of the prologue.
    for (name, kinds) in &local_kinds {
        let _ = write!(
            out,
            "\n/// The struct `{name}` of the prologue of `gram.y`.\n#[derive(Clone, Debug, Default, PartialEq)]\npub(crate) struct {name} {{\n",
        );
        for (field, kind) in kinds {
            let _ = writeln!(out, "    pub(crate) {}: {},", nodes::field_name(field), kind.rust());
        }
        out.push_str("}\n");
    }
    if !local.defines.is_empty() {
        out.push('\n');
    }
    for d in &local.defines {
        let (rust, value) = d.rust();
        let _ = writeln!(out, "pub(crate) const {}: {rust} = {value};", d.name);
    }

    // The actions.
    let mut methods: BTreeMap<usize, Vec<(usize, Uses)>> = BTreeMap::new();
    let mut arms = String::new();
    for (number, rule) in grammar.rules.iter().enumerate().skip(1) {
        let lhs = glue.member(rule.lhs)?;
        let action = glue.action(rule).map_err(|e| format!("gram.y line {}: {e}", rule.line))?;
        let (used, body) = match action {
            Action::Inline { expression, values } => (values, expression),
            Action::Method(uses) => {
                let name = glue.name(rule.lhs);
                let method = rule.name.replace('.', "_");
                let mut arguments = vec!["p".to_string()];
                for &k in &uses.values {
                    let symbol = *rule.rhs.get(k - 1).ok_or_else(|| {
                        format!("gram.y line {}: ${k} is past the right side", rule.line)
                    })?;
                    let member = glue
                        .member(symbol)?
                        .ok_or_else(|| format!("gram.y line {}: ${k} has no type", rule.line))?;
                    arguments.push(format!("v{k}.into_{}()?", member.name));
                }
                for &k in &uses.locations {
                    arguments.push(format!("at[{}]", k - 1));
                }
                if uses.here {
                    arguments.push("here".to_string());
                }
                let call = format!("rules::{name}::{method}({})?", arguments.join(", "));
                let body = match lhs {
                    Some(m) => format!("Value::{}({call})", m.name),
                    None => format!("{call};\nValue::None"),
                };
                let values = uses.values.clone();
                methods.entry(rule.lhs).or_default().push((number, uses));
                (values, body)
            }
        };
        let _ = write!(arms, "        // {}\n        {number} => ", rule.name);
        if used.is_empty() && !body.contains('\n') {
            let _ = writeln!(arms, "{body},");
            continue;
        }
        arms.push_str("{\n");
        if !used.is_empty() {
            let pattern: Vec<String> = (1..=rule.rhs.len())
                .map(|k| if used.contains(&k) { format!("v{k}") } else { "_".to_string() })
                .collect();
            let _ = writeln!(arms, "            let [{}] = take(rhs);", pattern.join(", "));
        }
        for line in body.lines() {
            let _ = writeln!(arms, "            {line}");
        }
        arms.push_str("        }\n");
    }
    // The value enum and its conversions.
    out.push_str(
        "\n/// The value of a symbol: a member of `%union`, or nothing for a symbol with no type.\n\
         #[derive(Debug, Default)]\npub(crate) enum Value {\n    #[default]\n    None,\n",
    );
    for m in &glue.members {
        let _ = writeln!(out, "    {}({}),", m.name, m.rust);
    }
    out.push_str(
        "}\n\nimpl Value {\n    /// The name of the member, for the error of a value of the wrong member.\n    fn member(&self) -> &'static str {\n        match self {\n            Value::None => \"nothing\",\n",
    );
    for m in &glue.members {
        let _ = writeln!(out, "            Value::{}(_) => \"{}\",", m.name, m.name);
    }
    out.push_str("        }\n    }\n");
    // Only the members that `reduce` takes need an accessor. A value that goes into a list or a node takes the accessor of `node`.
    for m in glue.members.iter().filter(|m| arms.contains(&format!(".into_{}()", m.name))) {
        let name = &m.name;
        let _ = write!(
            out,
            "\n    /// The value as the member `{name}`.\n    pub(crate) fn into_{name}(self) -> Result<{}, Error> {{\n        match self {{\n            Value::{name}(value) => Ok(value),\n",
            m.rust
        );
        match &m.convert {
            Convert::Exact => {}
            Convert::Node => {
                out.push_str(
                    "            Value::list(list) => Ok((!list.is_empty()).then_some(Node::List(list))),\n",
                );
                for other in glue.members.iter().filter(|o| matches!(o.convert, Convert::Boxed(_))) {
                    let _ = writeln!(
                        out,
                        "            Value::{}(value) => Ok(value.map(NodeType::into_node)),",
                        other.name
                    );
                }
            }
            Convert::List => out.push_str(
                "            Value::node(None) => Ok(List::new()),\n            Value::node(Some(Node::List(list))) => Ok(list),\n",
            ),
            Convert::Boxed(t) => {
                let _ = writeln!(
                    out,
                    "            Value::node(None) => Ok(None),\n            Value::node(Some(node)) => {t}::from_node(node).map(Some).map_err(|_| Error::internal(\"a node of the wrong type for {name}\")),"
                );
            }
        }
        let _ = write!(
            out,
            "            other => Err(Error::internal(&format!(\"a value of {{}} for {name}\", other.member()))),\n        }}\n    }}\n"
        );
    }
    out.push_str("}\n");

    // The token values.
    out.push_str(
        "\n/// The value of a token, from the value that the lexer gives it.\n\
         pub(crate) fn token(value: crate::lexer::Value<'_>) -> Value {\n    match value {\n        \
         crate::lexer::Value::None => Value::None,\n        crate::lexer::Value::Integer(value) => Value::ival(value),\n        \
         crate::lexer::Value::Text(text) => Value::str(Some(text.into())),\n        \
         crate::lexer::Value::Keyword(keyword) => Value::keyword(keyword.name),\n    }\n}\n",
    );

    out.push_str(
        "\n/// Runs the action of `rule`. `rhs` is the values of its right side, `at` their locations\n\
         /// and `here` the location of the left side, which is the first location in `at` that is\n\
         /// not -1, as `YYLLOC_DEFAULT` of `gram.y` gives it.\n\
         pub(crate) fn reduce(\n    p: &mut Parser<'_>,\n    rule: usize,\n    rhs: Drain<'_, Value>,\n    at: &[i32],\n    here: i32,\n) -> Result<Value, Error> {\n    let _ = (&at, here);\n    Ok(match rule {\n",
    );
    out.push_str(&arms);
    out.push_str("        _ => return Err(Error::internal(\"no such rule\")),\n    })\n}\n");

    // The traits.
    out.push_str(
        "\n/// The actions that are not a move or a constant, one trait for each rule of `gram.y` with\n\
         /// one method for each such alternative. The default of each method gives the error that\n\
         /// the action is not ported.\n\
         pub(crate) mod rules {\n    // The names of the methods are those of the rules, as `into_clause_1`.\n    #![allow(clippy::too_many_arguments, clippy::wrong_self_convention)]\n\n    use crate::error::Error;\n    use crate::nodes::{List, Node, Str};\n",
    );
    let names = ["Error", "List", "Node", "Str"];
    for (&lhs, alternatives) in &methods {
        let name = glue.name(lhs);
        if RUST_KEYWORDS.contains(&name) || names.contains(&name) {
            return Err(format!("the rule {name} has the name of a Rust keyword or a type"));
        }
        let result = qualified(glue.member(lhs)?);
        let _ = write!(out, "\n    /// The actions of `{name}`.\n    pub(crate) trait {name} {{\n");
        for (i, (number, uses)) in alternatives.iter().enumerate() {
            let rule = &grammar.rules[*number];
            let rhs: Vec<&str> = rule.rhs.iter().map(|&s| glue.name(s)).collect();
            let rhs = if rhs.is_empty() { "/* empty */".to_string() } else { rhs.join(" ") };
            let mut parameters = vec!["&mut self".to_string()];
            for &k in &uses.values {
                parameters.push(format!("_{k}: {}", qualified(glue.member(rule.rhs[k - 1])?)));
            }
            for &k in &uses.locations {
                parameters.push(format!("_at{k}: i32"));
            }
            if uses.here {
                parameters.push("_here: i32".to_string());
            }
            if i > 0 {
                out.push('\n');
            }
            let _ = write!(
                out,
                "        /// `{name}: {rhs}`, line {} of `gram.y`.\n        fn {}({}) -> Result<{result}, Error> {{\n            Err(Error::not_ported(\"{}\"))\n        }}\n",
                rule.line,
                rule.name.replace('.', "_"),
                parameters.join(", "),
                rule.name
            );
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");

    // The parser has every trait. A trait that `src/actions` does not implement has only its defaults.
    let traits: BTreeSet<&str> = methods.keys().map(|&lhs| glue.name(lhs)).collect();
    if let Some(name) = ported.iter().find(|name| !traits.contains(name.as_str())) {
        return Err(format!("src/actions implements rules::{name}, and there is no such trait"));
    }
    out.push_str("\n// The rules with no ported action.\n");
    for name in traits.iter().filter(|name| !ported.contains(**name)) {
        let _ = writeln!(out, "impl rules::{name} for Parser<'_> {{}}");
    }
    Ok(out)
}

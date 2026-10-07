//! The node types of the raw parse tree, made from the vendored PostgreSQL headers.
//!
//! The parser of `rupg-sql` builds the tree that the C actions of `gram.y` build, with the node types of `parsenodes.h`, `primnodes.h` and `value.h`. This generator reads those headers as `src/backend/nodes/gen_node_support.pl` reads them, and writes one Rust struct for each node type that a raw parse tree can hold, one Rust type for each enum, and for each struct the writer that gives the text of `nodeToString` in `outfuncs.c`. The differential test compares that text with the raw parse tree that PostgreSQL logs when `debug_print_raw_parse` is on.
//!
//! A node type is in the raw tree when `gram.y` names it, when a `make` function of `makefuncs.c` that `gram.y` calls builds it, or when a field of such a node points to it. The nodes that only parse analysis builds, for example `Query`, are not generated. A field that points to one of them is a plain `Node` field, and the parser leaves it empty.
//!
//! The plan is `spec/07-sql-types-and-catalog.md` section 7.2.
//!
//! Lifted from `xtask/src/postgres/nodes.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

/// The node types that the `make` functions of `makefuncs.c` build for `gram.y`, in addition to the node types that `gram.y` names.
const MADE: [&str; 16] = [
    "A_Expr",
    "Alias",
    "BoolExpr",
    "ColumnDef",
    "DefElem",
    "FuncCall",
    "GroupingSet",
    "JsonBehavior",
    "JsonFormat",
    "JsonIsPredicate",
    "JsonKeyValue",
    "JsonTablePathSpec",
    "JsonValueExpr",
    "RangeVar",
    "TypeName",
    "VacuumRelation",
];

/// The node types that only parse analysis or the planner build. A field that points to one of them is a plain `Node` field.
const ANALYZED: [&str; 4] = ["Query", "FromExpr", "OnConflictExpr", "FuncExpr"];

/// The node types of `value.h`. They are variants of `Node` with the value in them, and not structs, because `outfuncs.c` writes them without braces.
const VALUES: [&str; 5] = ["Integer", "Float", "Boolean", "String", "BitString"];

/// The node types that `outfuncs.c` writes by hand. The generator writes the same text.
const CUSTOM: [&str; 3] = ["A_Const", "A_Expr", "BoolExpr"];

/// The keywords and the reserved words of Rust. A C field with one of these names is a raw identifier in Rust.
pub(super) const RUST_KEYWORDS: [&str; 49] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "static", "struct", "trait", "true", "try", "type", "typeof", "unsafe", "unsized",
    "use", "virtual", "where", "while", "yield",
];

/// One C enum: its name and its constants with their values, in order.
pub(super) struct Enum {
    pub(super) name: String,
    pub(super) constants: Vec<(String, i64)>,
}

/// One `#define` of a number, of a character or of a string.
pub(super) struct Define {
    pub(super) name: String,
    pub(super) value: DefineValue,
}

/// The value of a `#define`.
pub(super) enum DefineValue {
    Number(i64),
    /// A character literal, as `'a'`.
    Char(u8),
    /// A string literal, as `"btree"`.
    Text(String),
    /// An `Oid`, as the type OIDs of `pg_type_d.h` are.
    Oid(u32),
    /// A SQLSTATE of `errcodes.txt`, as the constant of `rupg_common::SqlState` with the name without `ERRCODE_`.
    SqlState(String),
}

impl Define {
    /// The Rust type and the Rust expression of the value. A number that does not fit `i32` is an `i64`, as `LONG_MAX` is on the 64-bit platforms.
    pub(super) fn rust(&self) -> (&'static str, String) {
        match &self.value {
            DefineValue::Number(v) if i32::try_from(*v).is_ok() => ("i32", v.to_string()),
            DefineValue::Number(v) => ("i64", v.to_string()),
            DefineValue::Char(c) => ("u8", format!("b{:?}", char::from(*c))),
            DefineValue::Text(t) => ("&str", format!("{t:?}")),
            DefineValue::Oid(v) => ("u32", v.to_string()),
            DefineValue::SqlState(name) => ("SqlState", format!("SqlState::{name}")),
        }
    }
}

/// The limits of `c.h` and `limits.h` that the headers and `gram.y` use.
const LIMITS: [(&str, i64); 3] =
    [("PG_INT16_MAX", i16::MAX as i64), ("PG_INT32_MAX", i32::MAX as i64), ("LONG_MAX", i64::MAX)];

/// The invalid values of the `uint32` ID types of `c.h` that `gram.y` uses. `c.h` is not in the vendored headers, because it has nothing else that the parser needs.
const INVALID_IDS: [&str; 1] = ["InvalidSubTransactionId"];

/// One field of a C struct, with the type as `gen_node_support.pl` normalizes it.
#[derive(Clone)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) ctype: String,
    ignored: bool,
    /// What `equal()` of `equalfuncs.c` does with the field.
    equal: Equality,
}

/// What `equal()` does with a field, from `pg_node_attr`. It never compares a location.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Equality {
    Compare,
    /// `equal_ignore`.
    Ignore,
    /// `equal_ignore_if_zero`: the field is the same when one of the two is zero.
    IgnoreIfZero,
}

/// One C struct and the attributes that `pg_node_attr` gives it.
pub(super) struct Struct {
    pub(super) name: String,
    pub(super) fields: Vec<Field>,
    attributes: Vec<String>,
}

/// What the headers declare.
#[derive(Default)]
pub(super) struct Headers {
    pub(super) enums: Vec<Enum>,
    pub(super) structs: Vec<Struct>,
    pub(super) defines: Vec<Define>,
}

/// Removes the C comments and keeps the line breaks.
pub(super) fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("/*") {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find("*/") else { break };
        out.extend(rest[open..open + close].chars().filter(|&c| c == '\n'));
        rest = &rest[open + close + 2..];
    }
    out.push_str(rest);
    out
}

/// The value of a define of an OID, as `((RelFileNumber) InvalidOid)`: a cast to an OID type of a number or of `InvalidOid`.
fn oid(text: &str) -> Option<u32> {
    let inner = text.strip_prefix("((")?.strip_suffix(')')?;
    let (ctype, value) = inner.split_once(')')?;
    if !matches!(ctype.trim(), "Oid" | "RelFileNumber") {
        return None;
    }
    match value.trim() {
        "InvalidOid" => Some(0),
        value => value.parse().ok(),
    }
}

/// Evaluates the value of an enum constant: a number, a character, a constant before it, or the operators `<<` and `|` on them, in parentheses or not.
fn evaluate(text: &str, known: &HashMap<String, i64>) -> Result<i64, String> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')'))
        && balanced(inner)
    {
        return evaluate(inner, known);
    }
    for operator in ["|", "<<"] {
        if let Some(at) = top_level(text, operator) {
            let left = evaluate(&text[..at], known)?;
            let right = evaluate(&text[at + operator.len()..], known)?;
            return Ok(if operator == "|" { left | right } else { left << right });
        }
    }
    if let Some(c) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return match c.as_bytes() {
            [c] => Ok(i64::from(*c)),
            _ => Err(format!("the character {text} is not known")),
        };
    }
    if let Ok(value) = text.parse::<i64>() {
        return Ok(value);
    }
    if let Some(hex) = text.strip_prefix("0x")
        && let Ok(value) = i64::from_str_radix(hex, 16)
    {
        return Ok(value);
    }
    known.get(text).copied().ok_or_else(|| format!("the enum value {text} is not known"))
}

/// Whether the parentheses of `text` balance.
fn balanced(text: &str) -> bool {
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

/// The offset of the last `operator` in `text` that is not in parentheses.
fn top_level(text: &str, operator: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut found = None;
    let bytes = text.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && text[i..].starts_with(operator) => {
                // `<<` is not two `|`, and `|` is not part of `||`.
                if operator == "|"
                    && (bytes.get(i + 1) == Some(&b'|') || i > 0 && bytes[i - 1] == b'|')
                {
                    continue;
                }
                found.get_or_insert(i);
            }
            _ => {}
        }
    }
    found
}

/// Reads the `ERRCODE_` names of `errcodes.txt` as defines of their SQLSTATE codes, as `errcodes.h` defines them with `MAKE_SQLSTATE`. The value of each is the constant of `rupg_common::SqlState` with the same name.
fn read_errcodes(text: &str, headers: &mut Headers) -> Result<(), String> {
    for (number, line) in text.lines().enumerate() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if line.starts_with('#') || line.starts_with("Section:") || fields.is_empty() {
            continue;
        }
        match fields[..] {
            [_, _, name, ..] if name.starts_with("ERRCODE_") => {
                let value = DefineValue::SqlState(name["ERRCODE_".len()..].to_string());
                headers.defines.push(Define { name: name.to_string(), value });
            }
            _ => return Err(format!("errcodes.txt line {}: not a code line: {line}", number + 1)),
        }
    }
    Ok(())
}

/// Reads the type OIDs of `pg_type.dat` as `Oid` defines, with the names that `genbki.pl` gives them in `pg_type_d.h`: the `oid_symbol` of the entry, or else the type name in upper case and `OID`, and the name, `ARRAY` and `OID` for the array type. `InvalidOid` of `postgres_ext.h` comes first, because it is the type OID for no type.
fn read_types(text: &str, headers: &mut Headers) -> Result<(), String> {
    headers.defines.push(Define { name: "InvalidOid".to_string(), value: DefineValue::Oid(0) });
    for entry in crate::dat::dat_entries("pg_type.dat", text)? {
        let name = entry.get("typname").ok_or("pg_type.dat: an entry with no typname")?;
        let oid = |key: &str| {
            let value = entry.get(key)?;
            value.parse().ok().map(DefineValue::Oid)
        };
        let upper = name.to_uppercase();
        let symbol = entry.get("oid_symbol").cloned().unwrap_or_else(|| format!("{upper}OID"));
        let value = oid("oid").ok_or_else(|| format!("pg_type.dat: {name} has a bad oid"))?;
        headers.defines.push(Define { name: symbol, value });
        if let Some(value) = oid("array_type_oid") {
            headers.defines.push(Define { name: format!("{upper}ARRAYOID"), value });
        }
    }
    Ok(())
}

/// Reads the enums, the defines and, for [`Read::Nodes`], the structs of one header.
pub(super) fn read_header(
    file: &str,
    text: &str,
    read: Read,
    headers: &mut Headers,
) -> Result<(), String> {
    let text = strip_comments(text);
    let mut rest = text.as_str();
    let mut known: HashMap<String, i64> =
        LIMITS.iter().map(|(name, value)| (name.to_string(), *value)).collect();
    for e in &headers.enums {
        for (name, value) in &e.constants {
            known.insert(name.clone(), *value);
        }
    }
    for d in &headers.defines {
        if let DefineValue::Number(v) = d.value {
            known.insert(d.name.clone(), v);
        }
    }
    // A `#define` of a number can be the value of an enum constant. A define can go on over more lines, each one with a backslash at the end.
    for line in text.replace("\\\n", " ").lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("#define")
            && let Some(name) = words.next()
            && !name.contains('(')
        {
            let value = words.collect::<Vec<&str>>().join(" ");
            let value = if let Some(t) = value.strip_prefix('"').and_then(|t| t.strip_suffix('"'))
                && !t.contains(['"', '\\'])
            {
                DefineValue::Text(t.to_string())
            } else if let Some(oid) = oid(&value) {
                DefineValue::Oid(oid)
            } else if let Ok(number) = evaluate(&value, &known) {
                known.insert(name.to_string(), number);
                match u8::try_from(number) {
                    Ok(c) if value.starts_with('\'') => DefineValue::Char(c),
                    _ => DefineValue::Number(number),
                }
            } else {
                continue;
            };
            headers.defines.push(Define { name: name.to_string(), value });
        }
    }
    // A definition is `typedef enum|struct Name { ... } Name;`, or `struct Name { ... };` at the start of a line for a struct that a `typedef` declared before.
    loop {
        let typedef = rest.find("typedef ");
        let plain = rest.find("\nstruct ");
        let (is_typedef, at) = match (typedef, plain) {
            (Some(t), Some(p)) if p < t => (false, p + 1),
            (Some(t), _) => (true, t + "typedef ".len()),
            (None, Some(p)) => (false, p + 1),
            (None, None) => break,
        };
        rest = &rest[at..];
        // The keyword can have a line break after it, as in `typedef enum\n{`.
        let keyword = |word: &str| {
            rest.strip_prefix(word)
                .is_some_and(|r| r.starts_with(|c: char| c.is_whitespace() || c == '{'))
        };
        let is_enum = is_typedef && keyword("enum");
        if !is_enum && !keyword("struct") {
            continue;
        }
        let Some(open) = rest.find(['{', ';']) else { break };
        if rest.as_bytes()[open] == b';' {
            // A declaration like `typedef struct Foo Foo;`.
            continue;
        }
        let close =
            rest[open..].find('}').ok_or_else(|| format!("{file}: a block does not end"))? + open;
        let body = &rest[open + 1..close];
        let tail = &rest[close + 1..];
        let end = tail.find(';').ok_or_else(|| format!("{file}: a typedef does not end"))?;
        let name = if is_typedef {
            tail[..end].trim().to_string()
        } else {
            rest["struct ".len()..open].trim().to_string()
        };
        rest = &tail[end..];
        if is_enum {
            let mut constants = Vec::new();
            let mut next = 0i64;
            for item in body.split(',') {
                let item = item.trim();
                if item.is_empty() || item.starts_with('#') {
                    continue;
                }
                let (constant, value) = match item.split_once('=') {
                    Some((constant, value)) => (constant.trim(), evaluate(value, &known)?),
                    None => (item, next),
                };
                known.insert(constant.to_string(), value);
                constants.push((constant.to_string(), value));
                next = value + 1;
            }
            headers.enums.push(Enum { name, constants });
        } else if read == Read::Nodes {
            let mut fields = Vec::new();
            let mut attributes = Vec::new();
            for line in body.split(';') {
                let mut line = line.trim().to_string();
                // A `pg_node_attr` of the struct comes before its first field.
                while let Some(rest) = line.strip_prefix("pg_node_attr(") {
                    let close =
                        rest.find(')').ok_or_else(|| format!("{file}: bad pg_node_attr"))?;
                    let mut inner = &rest[..close];
                    let mut after = &rest[close + 1..];
                    // `array_size(x)` has its own parentheses.
                    if inner.contains('(') {
                        let close = rest.find("))").ok_or_else(|| format!("{file}: bad attr"))?;
                        inner = &rest[..=close];
                        after = &rest[close + 2..];
                    }
                    attributes.extend(inner.split(',').map(|a| a.trim().to_string()));
                    line = after.trim().to_string();
                }
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (declaration, field_attributes) = match line.find("pg_node_attr(") {
                    Some(at) => (line[..at].trim().to_string(), line[at..].to_string()),
                    None => (line.clone(), String::new()),
                };
                if declaration.contains(',') || declaration.contains('(') {
                    return Err(format!(
                        "{file}: struct {name} has a field the generator cannot read: {line}"
                    ));
                }
                let split = declaration
                    .rfind(|c: char| {
                        !(c.is_ascii_alphanumeric() || c == '_' || c == '[' || c == ']')
                    })
                    .ok_or_else(|| format!("{file}: bad field {line}"))?;
                let field = declaration[split + 1..].to_string();
                let mut ctype = declaration[..=split].trim().to_string();
                if let Some(stripped) = ctype.strip_prefix("const ") {
                    ctype = stripped.to_string();
                }
                if let Some(stripped) = ctype.strip_prefix("struct ") {
                    ctype = stripped.to_string();
                }
                let ctype: String = ctype.split_whitespace().collect::<Vec<_>>().join(" ");
                let ctype = ctype.replace(" *", "*");
                fields.push(Field {
                    name: field,
                    ctype,
                    ignored: field_attributes.contains("read_write_ignore"),
                    equal: if field_attributes.contains("equal_ignore_if_zero") {
                        Equality::IgnoreIfZero
                    } else if field_attributes.contains("equal_ignore") {
                        Equality::Ignore
                    } else {
                        Equality::Compare
                    },
                });
            }
            headers.structs.push(Struct { name, fields, attributes });
        }
    }
    Ok(())
}

/// How a field is stored and written.
#[derive(Clone)]
pub(super) enum Kind {
    Bool,
    Char,
    Int(&'static str),
    Location,
    Text,
    List,
    Node,
    Boxed(String),
    Embedded(String),
    Enum(String),
    Value,
}

impl Kind {
    /// The Rust type of a field of the kind.
    pub(super) fn rust(&self) -> String {
        match self {
            Kind::Bool => "bool".to_string(),
            Kind::Char => "u8".to_string(),
            Kind::Int(t) => (*t).to_string(),
            Kind::Location => "i32".to_string(),
            Kind::Text => "Option<Str>".to_string(),
            Kind::List => "List".to_string(),
            Kind::Node | Kind::Value => "Option<Node>".to_string(),
            Kind::Boxed(t) => format!("Option<Box<{t}>>"),
            Kind::Embedded(t) | Kind::Enum(t) => t.clone(),
        }
    }
}

/// The kind of a field of a C type.
pub(super) fn kind(
    ctype: &str,
    emitted: &BTreeSet<String>,
    enums: &BTreeSet<String>,
    nodes: &BTreeSet<String>,
) -> Result<Kind, String> {
    Ok(match ctype {
        "bool" => Kind::Bool,
        "char" => Kind::Char,
        "int" | "int32" => Kind::Int("i32"),
        "int16" | "AttrNumber" => Kind::Int("i16"),
        "uint32" | "Index" | "Oid" | "RelFileNumber" | "SubTransactionId" | "bits32" => {
            Kind::Int("u32")
        }
        "int64" => Kind::Int("i64"),
        "uint64" | "AclMode" => Kind::Int("u64"),
        "long" => Kind::Int("i64"),
        "ParseLoc" => Kind::Location,
        "char*" => Kind::Text,
        "List*" => Kind::List,
        "Node*" | "Expr*" => Kind::Node,
        "union ValUnion" => Kind::Value,
        t if enums.contains(t) => Kind::Enum(t.to_string()),
        t if t.ends_with('*') && emitted.contains(&t[..t.len() - 1]) => {
            Kind::Boxed(t[..t.len() - 1].to_string())
        }
        t if t.ends_with('*') && nodes.contains(&t[..t.len() - 1]) => Kind::Node,
        t if emitted.contains(t) => Kind::Embedded(t.to_string()),
        t => return Err(format!("the generator does not know the field type {t}")),
    })
}

/// The Rust name of a C field.
pub(super) fn field_name(name: &str) -> String {
    if RUST_KEYWORDS.contains(&name) { format!("r#{name}") } else { name.to_string() }
}

/// The node types, the enums and the constants that the raw parse tree and the actions of `gram.y` use.
pub(super) struct Model {
    pub(super) headers: Headers,
    /// Every node struct of the headers.
    pub(super) nodes: BTreeSet<String>,
    /// The node structs of the raw tree, which the generator writes.
    pub(super) emitted: BTreeSet<String>,
    /// Every enum of the headers.
    pub(super) enum_names: BTreeSet<String>,
    /// The enums that the generator writes: those of the fields of the emitted structs, and those that `gram.y` names or whose constants it names.
    pub(super) used_enums: BTreeSet<String>,
    /// The `#define` constants that `gram.y` names.
    pub(super) used_defines: BTreeSet<String>,
    /// The fields of each emitted struct, with their kinds.
    pub(super) layouts: BTreeMap<String, Vec<(Field, Kind)>>,
}

/// What the generator reads in a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Read {
    /// The node structs, the enums and the defines.
    Nodes,
    /// The enums and the defines only, for a catalog or a utility header whose structs are not nodes.
    Constants,
}

/// The vendored headers, in the order that `GENERATED` gives them.
pub(super) const HEADERS: [(&str, Read); 16] = [
    ("nodes.h", Read::Nodes),
    ("lockoptions.h", Read::Nodes),
    ("primnodes.h", Read::Nodes),
    ("parsenodes.h", Read::Nodes),
    ("value.h", Read::Nodes),
    ("pg_class.h", Read::Constants),
    ("pg_am.h", Read::Constants),
    ("pg_attribute.h", Read::Constants),
    ("pg_trigger.h", Read::Constants),
    ("index.h", Read::Constants),
    ("trigger.h", Read::Constants),
    ("datetime.h", Read::Constants),
    ("timestamp.h", Read::Constants),
    ("xml.h", Read::Constants),
    ("lockdefs.h", Read::Constants),
    ("relpath.h", Read::Constants),
];

/// The vendored data files that give constants, after the headers: `errcodes.txt` gives the `ERRCODE_` names of `errcodes.h`, and `pg_type.dat` gives the type OIDs of `pg_type_d.h`.
pub(super) const DATA: [&str; 2] = ["errcodes.txt", "pg_type.dat"];

/// The index of `gram.y` in the texts of [`model`].
pub(super) const GRAM: usize = HEADERS.len() + DATA.len();

/// Reads the headers, the data files and `gram.y`. The texts are, in order, the [`HEADERS`], the [`DATA`] files and `gram.y`.
pub(super) fn model(texts: &[String]) -> Result<Model, String> {
    let mut headers = Headers::default();
    for (name, value) in LIMITS {
        headers.defines.push(Define { name: name.to_string(), value: DefineValue::Number(value) });
    }
    for ((file, read), text) in HEADERS.iter().zip(texts) {
        read_header(file, text, *read, &mut headers)?;
    }
    read_errcodes(&texts[HEADERS.len()], &mut headers)?;
    read_types(&texts[HEADERS.len() + 1], &mut headers)?;
    for name in INVALID_IDS {
        headers.defines.push(Define { name: name.to_string(), value: DefineValue::Oid(0) });
    }
    let gram = strip_comments(&texts[GRAM]);

    let structs: HashMap<&str, &Struct> =
        headers.structs.iter().map(|s| (s.name.as_str(), s)).collect();
    // A node struct starts with `NodeTag type`, or with a node struct by value, its supertype.
    let mut nodes = BTreeSet::new();
    for s in &headers.structs {
        let first = s.fields.first().map(|f| f.ctype.as_str());
        let is_node = s.name != "Node" && first == Some("NodeTag")
            || first.is_some_and(|t| nodes.contains(t));
        if is_node {
            nodes.insert(s.name.clone());
        }
    }
    let abstract_or_tag_only =
        |s: &Struct| s.attributes.iter().any(|a| a == "abstract" || a == "nodetag_only");

    // The node types of the raw tree: those that gram.y names, those the make functions build, and those that a field of one of them points to.
    let words: BTreeSet<&str> = gram
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut emitted = BTreeSet::new();
    let mut todo: Vec<String> = nodes
        .iter()
        .filter(|n| words.contains(n.as_str()))
        .cloned()
        .chain(MADE.iter().map(|n| (*n).to_string()))
        .collect();
    while let Some(name) = todo.pop() {
        let Some(s) = structs.get(name.as_str()) else {
            return Err(format!("the node type {name} is not in the headers"));
        };
        if emitted.contains(&name)
            || ANALYZED.contains(&name.as_str())
            || VALUES.contains(&name.as_str())
            || abstract_or_tag_only(s)
        {
            continue;
        }
        emitted.insert(name.clone());
        for field in &s.fields {
            let t = field.ctype.trim_end_matches('*');
            if nodes.contains(t) {
                todo.push(t.to_string());
            }
        }
    }
    let enum_names: BTreeSet<String> = headers.enums.iter().map(|e| e.name.clone()).collect();

    // The fields of each emitted struct, without `NodeTag type` and with the abstract supertype `Expr xpr` left out, because it has no field to write.
    let mut layouts: BTreeMap<String, Vec<(Field, Kind)>> = BTreeMap::new();
    for name in &emitted {
        let s = structs[name.as_str()];
        let mut fields = Vec::new();
        for field in &s.fields {
            if field.ctype == "NodeTag" || field.ignored {
                continue;
            }
            if structs.get(field.ctype.as_str()).is_some_and(|s| abstract_or_tag_only(s)) {
                continue;
            }
            let kind = kind(&field.ctype, &emitted, &enum_names, &nodes)
                .map_err(|e| format!("{name}.{}: {e}", field.name))?;
            fields.push((field.clone(), kind));
        }
        layouts.insert(name.clone(), fields);
    }

    // The enums that an emitted struct uses, and those that gram.y names, as the type of a `%union` member or by one of their constants.
    let mut used_enums = BTreeSet::new();
    for fields in layouts.values() {
        for (_, kind) in fields {
            if let Kind::Enum(e) = kind {
                used_enums.insert(e.clone());
            }
        }
    }
    for e in &headers.enums {
        let named = words.contains(e.name.as_str())
            || e.constants.iter().any(|(c, _)| words.contains(c.as_str()));
        if named {
            used_enums.insert(e.name.clone());
        }
    }
    let used_defines =
        headers.defines.iter().filter(|d| words.contains(d.name.as_str())).map(|d| d.name.clone());
    let used_defines = used_defines.collect();
    Ok(Model { headers, nodes, emitted, enum_names, used_enums, used_defines, layouts })
}

/// Writes `src/generated/nodes.rs` from the vendored headers and `gram.y`. The texts are, in order, the [`HEADERS`] and `gram.y`.
pub(super) fn nodes(texts: &[String]) -> Result<String, String> {
    let Model { headers, used_enums, used_defines, layouts, .. } = model(texts)?;

    let mut out = String::new();
    out.push_str(
        "//! The node types of the raw parse tree, with the names and the fields of the PostgreSQL headers.\n\
         //!\n\
         //! @generated by `cargo xtask sql` from the vendored headers in `vendor/postgres-19/src/include`, `errcodes.txt`, `pg_type.dat` and `gram.y`. Do not edit. `cargo xtask sql --check` runs in CI and fails if this file and the vendored files disagree.\n\
         //!\n\
         //! A `Node *` field is an `Option<Node>`, a `List *` field is a [`List`], which is empty for `NIL`, a `char *` field is an `Option<Str>`, and a field that points to a node type is an `Option<Box<..>>` of that type. An enum is a number with a constant for each value, so that the zero of `makeNode` is a value of every enum. The writer of each type gives the text of `nodeToString` with the locations. The `#define` constants that `gram.y` names come first.\n\
         \n\
         #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, missing_docs)]\n\
         \n\
         use rupg_common::SqlState;\n\
         \n\
         use crate::nodes::{Equal, List, NodeType, Out, Str, w};\n",
    );

    if !used_defines.is_empty() {
        out.push('\n');
    }
    for d in headers.defines.iter().filter(|d| used_defines.contains(&d.name)) {
        let (rust, value) = d.rust();
        let _ = writeln!(out, "pub const {}: {rust} = {value};", d.name);
    }

    for e in headers.enums.iter().filter(|e| used_enums.contains(&e.name)) {
        let _ = write!(
            out,
            "\n/// The C enum `{}`.\n#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]\npub struct {}(pub i32);\n\nimpl {} {{\n",
            e.name, e.name, e.name
        );
        for (constant, value) in &e.constants {
            // The line breaks where rustfmt breaks it, so the file is the same after `cargo fmt`.
            let line = format!("    pub const {constant}: {} = {}({value});", e.name, e.name);
            if line.len() <= 100 {
                let _ = writeln!(out, "{line}");
            } else {
                let _ = writeln!(
                    out,
                    "    pub const {constant}: {} =\n        {}({value});",
                    e.name, e.name
                );
            }
        }
        out.push_str("}\n");
    }

    for (name, fields) in &layouts {
        let _ = write!(
            out,
            "\n/// The node `{name}`.\n#[derive(Clone, Debug, Default, PartialEq)]\npub struct {name} {{"
        );
        if !fields.is_empty() {
            out.push('\n');
        }
        for (field, kind) in fields {
            let _ = writeln!(out, "    pub {}: {},", field_name(&field.name), kind.rust());
        }
        out.push_str("}\n");
    }

    // The writers.
    for (name, fields) in &layouts {
        if CUSTOM.contains(&name.as_str()) {
            continue;
        }
        let upper = name.to_ascii_uppercase();
        let _ = write!(
            out,
            "\nimpl Out for {name} {{\n    fn out(&self, s: &mut String) {{\n        s.push_str(\"{upper}\");\n        self.fields(s, \"\");\n    }}\n\n    fn fields(&self, s: &mut String, prefix: &str) {{\n"
        );
        if fields.is_empty() {
            out.push_str("        let _ = (s, prefix);\n");
        }
        for (field, kind) in fields {
            let f = field_name(&field.name);
            let label = &field.name;
            let line = match kind {
                Kind::Bool => format!("w::bool(s, prefix, \"{label}\", self.{f});"),
                Kind::Char => format!("w::char(s, prefix, \"{label}\", self.{f});"),
                Kind::Int(_) | Kind::Location => {
                    format!("w::int(s, prefix, \"{label}\", self.{f});")
                }
                Kind::Enum(_) => {
                    format!("w::int(s, prefix, \"{label}\", self.{f}.0);")
                }
                Kind::Text => format!("w::text(s, prefix, \"{label}\", self.{f}.as_deref());"),
                Kind::List => {
                    format!("w::list(s, prefix, \"{label}\", &self.{f});")
                }
                Kind::Node | Kind::Value => {
                    format!("w::node(s, prefix, \"{label}\", self.{f}.as_ref());")
                }
                Kind::Boxed(_) => format!("w::boxed(s, prefix, \"{label}\", self.{f}.as_deref());"),
                Kind::Embedded(_) => {
                    format!("self.{f}.fields(s, &format!(\"{{prefix}}{label}.\"));")
                }
            };
            let _ = writeln!(out, "        {line}");
        }
        out.push_str("    }\n}\n");
    }

    // The equality of `equalfuncs.c`, which does not compare the locations. The lines break where rustfmt breaks them.
    for (name, fields) in &layouts {
        let mut terms = Vec::new();
        for (field, kind) in fields {
            let f = field_name(&field.name);
            let term = match (kind, field.equal) {
                (Kind::Location, _) | (_, Equality::Ignore) => continue,
                (_, Equality::IgnoreIfZero) => {
                    format!("(self.{f} == other.{f} || self.{f} == 0 || other.{f} == 0)")
                }
                (Kind::List | Kind::Node | Kind::Value | Kind::Boxed(_) | Kind::Embedded(_), _) => {
                    format!("self.{f}.equal(&other.{f})")
                }
                _ => format!("self.{f} == other.{f}"),
            };
            terms.push(term);
        }
        let other = if terms.is_empty() { "_" } else { "other" };
        let _ = write!(
            out,
            "\nimpl Equal for {name} {{\n    fn equal(&self, {other}: &Self) -> bool {{\n"
        );
        let line = format!("        {}", terms.join(" && "));
        if terms.is_empty() {
            out.push_str("        true\n");
        } else if line.len() <= 100 {
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(out, "        {}", terms.join("\n            && "));
        }
        out.push_str("    }\n}\n");
    }

    // The node enum, its writer and the conversions.
    out.push_str(
        "\n/// A node of the raw parse tree. The nodes of `value.h` hold their value, and the other\n\
         /// node types are boxed.\n#[derive(Clone, Debug, PartialEq)]\npub enum Node {\n    \
         List(List),\n    Integer(i32),\n    Float(Str),\n    Boolean(bool),\n    String(Str),\n    BitString(Str),\n",
    );
    for name in layouts.keys() {
        let _ = writeln!(out, "    {name}(Box<{name}>),");
    }
    out.push_str(
        "}\n\nimpl Node {\n    /// Writes the node as `outNode` writes it.\n    pub(crate) fn write(&self, s: &mut String) {\n        match self {\n            \
         Node::List(list) => w::items(s, list),\n            Node::Integer(value) => w::integer(s, *value),\n            \
         Node::Float(value) => s.push_str(value),\n            Node::Boolean(value) => s.push_str(if *value { \"true\" } else { \"false\" }),\n            \
         Node::String(value) => w::string(s, value),\n            Node::BitString(value) => w::token(s, Some(value)),\n",
    );
    for name in layouts.keys() {
        let _ = writeln!(out, "            Node::{name}(node) => w::braced(s, &**node),");
    }
    out.push_str("        }\n    }\n}\n");
    out.push_str(
        "\nimpl Equal for Node {\n    fn equal(&self, other: &Node) -> bool {\n        match (self, other) {\n            \
         (Node::List(a), Node::List(b)) => a.equal(b),\n            (Node::Integer(a), Node::Integer(b)) => a == b,\n            \
         (Node::Float(a), Node::Float(b)) => a == b,\n            (Node::Boolean(a), Node::Boolean(b)) => a == b,\n            \
         (Node::String(a), Node::String(b)) => a == b,\n            (Node::BitString(a), Node::BitString(b)) => a == b,\n",
    );
    for name in layouts.keys() {
        let arm = format!("            (Node::{name}(a), Node::{name}(b)) => a.equal(b),");
        if arm.len() <= 100 {
            let _ = writeln!(out, "{arm}");
        } else {
            let _ = writeln!(
                out,
                "            (Node::{name}(a), Node::{name}(b)) => {{\n                a.equal(b)\n            }}"
            );
        }
    }
    out.push_str("            _ => false,\n        }\n    }\n}\n");
    for name in layouts.keys() {
        let _ = write!(
            out,
            "\nimpl NodeType for {name} {{\n    fn into_node(self: Box<Self>) -> Node {{\n        Node::{name}(self)\n    }}\n\n    \
             fn from_node(node: Node) -> Result<Box<Self>, Node> {{\n        match node {{\n            Node::{name}(node) => Ok(node),\n            other => Err(other),\n        }}\n    }}\n\n    \
             fn peek(node: &Node) -> Option<&Self> {{\n        match node {{\n            Node::{name}(node) => Some(node),\n            _ => None,\n        }}\n    }}\n\n    \
             fn peek_mut(node: &mut Node) -> Option<&mut Self> {{\n        match node {{\n            Node::{name}(node) => Some(node),\n            _ => None,\n        }}\n    }}\n}}\n\n\
             impl From<{name}> for Node {{\n    fn from(node: {name}) -> Node {{\n        Node::{name}(Box::new(node))\n    }}\n}}\n"
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_takes_enums_structs_and_defines() {
        let text = "#define FLAG 0x02\n\
            typedef enum Kind\n{\n\tK_A,\t\t/* a */\n\tK_B = FLAG | 1,\n\tK_C\n} Kind;\n\
            typedef struct Thing\n{\n\tpg_node_attr(no_equal)\n\tNodeTag type;\n\tconst char *name;\n\
            \tList\t   *items pg_node_attr(read_write_ignore);\n} Thing;\n\
            typedef struct Later Later;\n\
            struct Later\n{\n\tNodeTag type;\n\tThing *thing;\n};\n";
        let mut headers = Headers::default();
        read_header("test.h", text, Read::Nodes, &mut headers).unwrap();
        let kind = &headers.enums[0];
        assert_eq!(kind.name, "Kind");
        let constants: Vec<(&str, i64)> =
            kind.constants.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        assert_eq!(constants, [("K_A", 0), ("K_B", 3), ("K_C", 4)]);
        let thing = &headers.structs[0];
        assert_eq!(thing.name, "Thing");
        assert_eq!(thing.attributes, ["no_equal"]);
        let fields: Vec<(&str, &str, bool)> =
            thing.fields.iter().map(|f| (f.name.as_str(), f.ctype.as_str(), f.ignored)).collect();
        assert_eq!(
            fields,
            [("type", "NodeTag", false), ("name", "char*", false), ("items", "List*", true)]
        );
        assert_eq!(headers.structs[1].name, "Later");
        assert_eq!(headers.structs[1].fields[1].ctype, "Thing*");
    }
}

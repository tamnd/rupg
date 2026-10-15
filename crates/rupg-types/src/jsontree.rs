//! A `json` or `jsonb` value as a tree, for the functions and the operators that read into a value.
//!
//! Each node keeps the place of its text in the input. A function of `json` gives the text of a part of the value as it is in the input, as `get_worker` in `src/backend/utils/adt/jsonfuncs.c` does. The text of `jsonb` is in the one form that `jsonb_out` prints, so the text of a part of it is in that form too.
//!
//! The nodes are in one vector in the order of the input, and each node refers to its children by index. The parse takes no recursion, so a value nested to any depth does not use the stack.

use std::cmp::Ordering;

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::json::{self, Kind, Open, Sink, Stop};
use crate::jsonb::escape;
use crate::numeric::numeric_in;
use crate::tsquery::too_deep;

/// The depth of the containers that [`JsonTree::contains`] takes before it stops with the error of `check_stack_depth`.
const MAX_DEPTH: usize = 10_000;

/// The kind of a node, with its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonItem {
    Null,
    Bool(bool),
    /// A number. Its text is the text of the node.
    Number,
    /// A string with its escapes decoded.
    String(String),
    /// The nodes of the elements.
    Array(Vec<usize>),
    /// The decoded keys and the nodes of the values, in the order of the input.
    Object(Vec<(String, usize)>),
}

/// A node and the place of its text in the input.
#[derive(Debug, Clone)]
pub struct JsonNode {
    pub item: JsonItem,
    pub start: usize,
    pub end: usize,
}

/// A parsed value. The root is node 0.
#[derive(Debug)]
pub struct JsonTree<'a> {
    text: &'a str,
    nodes: Vec<JsonNode>,
}

/// The sink that builds the nodes.
#[derive(Default)]
struct Builder {
    nodes: Vec<JsonNode>,
    /// The open arrays and objects, innermost last.
    open: Vec<usize>,
    /// The key of the next field of the innermost object.
    key: Option<String>,
}

impl Builder {
    /// Adds a node and puts it into the array or the object around it.
    fn push(&mut self, item: JsonItem, start: usize, end: usize) -> usize {
        let at = self.nodes.len();
        self.nodes.push(JsonNode { item, start, end });
        if let Some(&parent) = self.open.last() {
            match &mut self.nodes[parent].item {
                JsonItem::Array(items) => items.push(at),
                JsonItem::Object(fields) => fields.push((self.key.take().unwrap_or_default(), at)),
                _ => {}
            }
        }
        at
    }
}

impl Sink for Builder {
    fn open(&mut self, open: Open) -> Result<(), TypeError> {
        self.open_at(open, 0)
    }

    fn close(&mut self, open: Open) -> Result<(), TypeError> {
        self.close_at(open, 0)
    }

    fn key(&mut self, key: &str) -> Result<(), TypeError> {
        self.key = Some(key.to_owned());
        Ok(())
    }

    fn scalar(&mut self, kind: Kind, token: &str, text: &str) -> Result<(), TypeError> {
        self.scalar_at(kind, 0, token, text)
    }

    fn open_at(&mut self, open: Open, start: usize) -> Result<(), TypeError> {
        let item = match open {
            Open::Array => JsonItem::Array(Vec::new()),
            Open::Object => JsonItem::Object(Vec::new()),
        };
        let at = self.push(item, start, start);
        self.open.push(at);
        Ok(())
    }

    fn close_at(&mut self, _: Open, end: usize) -> Result<(), TypeError> {
        if let Some(at) = self.open.pop() {
            self.nodes[at].end = end;
        }
        Ok(())
    }

    fn scalar_at(
        &mut self,
        kind: Kind,
        start: usize,
        token: &str,
        text: &str,
    ) -> Result<(), TypeError> {
        let item = match kind {
            Kind::String => JsonItem::String(text.to_owned()),
            Kind::Number => JsonItem::Number,
            Kind::True => JsonItem::Bool(true),
            Kind::False => JsonItem::Bool(false),
            _ => JsonItem::Null,
        };
        self.push(item, start, start + token.len());
        Ok(())
    }
}

impl<'a> JsonTree<'a> {
    /// Parses the text of a `json` or a `jsonb` value. The lexer decodes the escapes, as `makeJsonLexContext` with `need_escapes` does, so a `json` value with `\u0000` or a lone surrogate gives the error of `jsonb_in`.
    ///
    /// # Errors
    ///
    /// The error of the input of `jsonb` for the text.
    pub fn parse(text: &'a str) -> Result<JsonTree<'a>, TypeError> {
        JsonTree::parse_with(text, true)
    }

    /// Parses the text with the escapes decoded or not. Without the escapes, the text of a string node is not decoded and a `json` value always parses.
    ///
    /// # Errors
    ///
    /// The error of the input of `json` or `jsonb` for the text.
    pub fn parse_with(text: &'a str, escapes: bool) -> Result<JsonTree<'a>, TypeError> {
        let mut builder = Builder::default();
        match json::parse(text, escapes, &mut builder) {
            Ok(()) => Ok(JsonTree { text, nodes: builder.nodes }),
            Err(Stop::Syntax(fail)) => Err(json::syntax(text, fail)),
            Err(Stop::Action(error)) => Err(error),
        }
    }

    /// The whole text.
    pub fn source(&self) -> &'a str {
        self.text
    }

    pub fn node(&self, at: usize) -> &JsonNode {
        &self.nodes[at]
    }

    pub fn item(&self, at: usize) -> &JsonItem {
        &self.nodes[at].item
    }

    /// The text of a node in the input.
    pub fn text(&self, at: usize) -> &'a str {
        let node = &self.nodes[at];
        &self.text[node.start..node.end]
    }

    /// The value of the last field with the key `key`, when the node is an object.
    pub fn field(&self, at: usize, key: &str) -> Option<usize> {
        match self.item(at) {
            JsonItem::Object(fields) => fields.iter().rev().find(|(k, _)| k == key).map(|f| f.1),
            _ => None,
        }
    }

    /// The element at `index`, when the node is an array. A negative index counts from the end.
    pub fn element(&self, at: usize, index: i64) -> Option<usize> {
        let JsonItem::Array(items) = self.item(at) else { return None };
        let index = if index < 0 { index + items.len() as i64 } else { index };
        usize::try_from(index).ok().and_then(|i| items.get(i).copied())
    }

    /// `JsonbValueAsText` and the text variants of `get_worker`: a string decoded, null as `None`, and the text of the node for the other kinds.
    pub fn as_text(&self, at: usize) -> Option<String> {
        match self.item(at) {
            JsonItem::Null => None,
            JsonItem::String(s) => Some(s.clone()),
            _ => Some(self.text(at).to_owned()),
        }
    }

    /// The type name of a node, as `json_typeof` and `jsonb_typeof` give it.
    pub fn type_name(&self, at: usize) -> &'static str {
        match self.item(at) {
            JsonItem::Null => "null",
            JsonItem::Bool(_) => "boolean",
            JsonItem::Number => "number",
            JsonItem::String(_) => "string",
            JsonItem::Array(_) => "array",
            JsonItem::Object(_) => "object",
        }
    }

    /// The tokens of the value in the order of `JsonbIteratorNext`. A scalar at the root is in an array with the raw mark, as `jsonb` keeps it.
    pub fn tokens(&self) -> Tokens<'_, 'a> {
        Tokens { tree: self, stack: Vec::new(), start: true, value: None }
    }

    /// The text of the value with the null fields of the objects removed and, with `arrays`, the null elements of the arrays removed. With `spaces`, there is one space after each comma and each colon, as `jsonb` prints. The strings and the keys have the escapes of `escape_json`, and the other scalars are their text in the input. This is `json_strip_nulls` and `jsonb_strip_nulls`.
    pub fn strip_nulls(&self, arrays: bool, spaces: bool) -> String {
        let (comma, colon) = if spaces { (", ", ": ") } else { (",", ":") };
        let mut out = String::with_capacity(self.text.len());
        // True when the next value is the first one in its array or object, or follows a key.
        let mut first = true;
        // True when the next value is the null value of a field that the output skips.
        let mut skip = false;
        for token in self.tokens() {
            match token {
                Token::Key(name, value) => {
                    if self.item(value) == &JsonItem::Null {
                        skip = true;
                        continue;
                    }
                    if !first {
                        out.push_str(comma);
                    }
                    escape(name, &mut out);
                    out.push_str(colon);
                    first = true;
                }
                Token::Scalar { node, elem } => {
                    if std::mem::take(&mut skip) {
                        continue;
                    }
                    if elem {
                        if arrays && self.item(node) == &JsonItem::Null {
                            continue;
                        }
                        if !first {
                            out.push_str(comma);
                        }
                    }
                    self.put_scalar(node, &mut out);
                    first = false;
                }
                Token::Begin { node, raw, elem } => {
                    if raw {
                        continue;
                    }
                    if elem && !first {
                        out.push_str(comma);
                    }
                    out.push(if self.is_array(node) { '[' } else { '{' });
                    first = true;
                }
                Token::End { node, raw } => {
                    if raw {
                        continue;
                    }
                    out.push(if self.is_array(node) { ']' } else { '}' });
                    first = false;
                }
            }
        }
        out
    }

    /// `JsonbToCStringIndent`: the text of `jsonb_pretty`, with each element and each field on its own line and four spaces for each level.
    pub fn pretty(&self) -> String {
        fn indent(out: &mut String, on: bool, level: usize) {
            if on {
                out.push('\n');
                out.extend(std::iter::repeat_n(' ', level * 4));
            }
        }
        let mut out = String::with_capacity(self.text.len() * 2);
        let (mut first, mut use_indent, mut last_was_key) = (true, false, false);
        let mut level = 0;
        for token in self.tokens() {
            let mut key = false;
            match token {
                Token::Begin { node, raw, .. } => {
                    if !first {
                        out.push(',');
                    }
                    if !raw {
                        indent(&mut out, use_indent && !last_was_key, level);
                        out.push(if self.is_array(node) { '[' } else { '{' });
                    }
                    first = true;
                    level += 1;
                }
                Token::Key(name, value) => {
                    if !first {
                        out.push(',');
                    }
                    indent(&mut out, use_indent, level);
                    escape(name, &mut out);
                    out.push_str(": ");
                    first = true;
                    key = !self.is_scalar(value);
                }
                Token::Scalar { node, elem } => {
                    if elem {
                        if !first {
                            out.push(',');
                        }
                        indent(&mut out, use_indent, level);
                    }
                    self.put_scalar(node, &mut out);
                    first = false;
                }
                Token::End { node, raw } => {
                    level -= 1;
                    if !raw {
                        indent(&mut out, use_indent, level);
                        out.push(if self.is_array(node) { ']' } else { '}' });
                    }
                    first = false;
                }
            }
            use_indent = true;
            last_was_key = key;
        }
        out
    }

    /// `compareJsonbContainers`: the order of two `jsonb` values for the B-tree operators.
    pub fn compare(&self, other: &JsonTree<'_>) -> Ordering {
        let (mut a, mut b) = (self.tokens(), other.tokens());
        loop {
            let (ta, tb) = match (a.next(), b.next()) {
                (Some(ta), Some(tb)) => (ta, tb),
                (None, None) => return Ordering::Equal,
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
            };
            let (ra, rb) = (self.token_rank(ta), other.token_rank(tb));
            let order = match (ta, tb) {
                _ if ra != rb => ra.cmp(&rb),
                (Token::Key(x, _), Token::Key(y, _)) => x.as_bytes().cmp(y.as_bytes()),
                (Token::Begin { node: x, raw: rx, .. }, Token::Begin { node: y, raw: ry, .. }) => {
                    let mut order = Ordering::Equal;
                    if rx != ry {
                        order = if rx { Ordering::Less } else { Ordering::Greater };
                    }
                    // `compareJsonbContainers` lets the count override the raw mark, so an empty array at the root sorts before a scalar.
                    let (nx, ny) = (self.count(x, rx), other.count(y, ry));
                    if nx != ny {
                        order = nx.cmp(&ny);
                    }
                    order
                }
                (Token::Scalar { node: x, .. }, Token::Scalar { node: y, .. }) => {
                    self.compare_scalar(x, other, y)
                }
                _ => Ordering::Equal,
            };
            if order != Ordering::Equal {
                return order;
            }
        }
    }

    /// `JsonbDeepContains`: true when the value contains `other`, as the operator `@>` of `jsonb` finds.
    ///
    /// # Errors
    ///
    /// `stack depth limit exceeded` for a value with too many levels.
    pub fn contains(&self, other: &JsonTree<'_>) -> Result<bool, TypeError> {
        if self.is_scalar(0) && !other.is_scalar(0) {
            return Ok(false);
        }
        self.contains_at(0, other, 0, 0)
    }

    /// `findJsonbValueFromContainer` for a string: true when the value is an object with the key `key`, or an array with the string `key` as an element. A scalar at the root is an array.
    pub fn exists(&self, key: &str) -> bool {
        match self.item(0) {
            JsonItem::Object(fields) => fields.iter().any(|(k, _)| k == key),
            _ => self
                .members(0)
                .iter()
                .any(|&x| matches!(self.item(x), JsonItem::String(s) if s == key)),
        }
    }

    /// `setPath`: the text of the value with the part at `path` replaced, created, deleted or inserted. `new` is the text of the new part, which [`PathOp::Delete`] does not use. The keys of a new field are not in their sorted place, so give the text to `jsonb_in`. The root must be an array or an object, and `path` must not be empty.
    ///
    /// # Errors
    ///
    /// A null element or an element that is not an integer in the path where the value has an array, `cannot replace existing key` for an insert into an object, and `stack depth limit exceeded`.
    pub fn set_path(
        &self,
        path: &[Option<&str>],
        new: &str,
        op: PathOp,
    ) -> Result<String, TypeError> {
        let mut out = String::with_capacity(self.text.len() + new.len());
        self.set_at(0, path, 0, new, op, &mut out)?;
        Ok(out)
    }

    /// `setPath` for the node `at` at the level `level` of the path.
    fn set_at(
        &self,
        at: usize,
        path: &[Option<&str>],
        level: usize,
        new: &str,
        op: PathOp,
        out: &mut String,
    ) -> Result<(), TypeError> {
        if level > MAX_DEPTH {
            return Err(too_deep());
        }
        let Some(step) = path[level] else {
            return Err(TypeError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!("path element at position {} is null", level + 1),
            ));
        };
        match self.item(at) {
            JsonItem::Object(fields) => self.set_object(fields, step, path, level, new, op, out),
            JsonItem::Array(items) => self.set_array(items, step, path, level, new, op, out),
            _ => {
                out.push_str(self.text(at));
                Ok(())
            }
        }
    }

    /// `setPathObject`.
    #[allow(clippy::too_many_arguments)]
    fn set_object(
        &self,
        fields: &[(String, usize)],
        step: &str,
        path: &[Option<&str>],
        level: usize,
        new: &str,
        op: PathOp,
        out: &mut String,
    ) -> Result<(), TypeError> {
        let last = level + 1 == path.len();
        let mut first = true;
        out.push('{');
        if fields.is_empty() && op.creates() && last {
            put_field(out, &mut first, step, new);
        }
        let mut done = false;
        for (i, (name, value)) in fields.iter().enumerate() {
            if !done && name == step {
                done = true;
                if !last {
                    put_key(out, &mut first, name);
                    self.set_at(*value, path, level + 1, new, op, out)?;
                } else if op.inserts() {
                    let mut error = TypeError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "cannot replace existing key".to_string(),
                    );
                    error.hint =
                        Some("Try using the function jsonb_set to replace key value.".to_string());
                    return Err(error);
                } else if op != PathOp::Delete {
                    put_field(out, &mut first, name, new);
                }
                continue;
            }
            if op.creates() && !done && last && i + 1 == fields.len() {
                put_field(out, &mut first, step, new);
            }
            put_field(out, &mut first, name, self.text(*value));
        }
        out.push('}');
        Ok(())
    }

    /// `setPathArray`. An index before the first element puts a new element at the start.
    #[allow(clippy::too_many_arguments)]
    fn set_array(
        &self,
        items: &[usize],
        step: &str,
        path: &[Option<&str>],
        level: usize,
        new: &str,
        op: PathOp,
        out: &mut String,
    ) -> Result<(), TypeError> {
        let Some(index) = json_index(step) else {
            return Err(TypeError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("path element at position {} is not an integer: \"{step}\"", level + 1),
            ));
        };
        let count = items.len() as i64;
        let mut index = i64::from(index);
        // True for the index `INT_MIN` of `setPathArray`, which is before the first element.
        let mut front = false;
        if index < 0 {
            if -index > count {
                front = true;
            } else {
                index += count;
            }
        }
        index = index.min(count);
        let last = level + 1 == path.len();
        let mut first = true;
        let mut done = false;
        out.push('[');
        if (front || items.is_empty()) && last && op.creates() {
            put_element(out, &mut first, new);
            done = true;
        }
        for (i, &item) in items.iter().enumerate() {
            if front || i as i64 != index {
                put_element(out, &mut first, self.text(item));
                continue;
            }
            done = true;
            if !last {
                put_element(out, &mut first, "");
                self.set_at(item, path, level + 1, new, op, out)?;
                continue;
            }
            if matches!(op, PathOp::InsertBefore | PathOp::Create) {
                put_element(out, &mut first, new);
            }
            if op.inserts() {
                put_element(out, &mut first, self.text(item));
            }
            if matches!(op, PathOp::InsertAfter | PathOp::Replace) {
                put_element(out, &mut first, new);
            }
        }
        if op.creates() && !done && last {
            put_element(out, &mut first, new);
        }
        out.push(']');
        Ok(())
    }

    /// The elements of an array. A scalar at the root is the one element of its array.
    fn members(&self, at: usize) -> &[usize] {
        match self.item(at) {
            JsonItem::Array(items) => items,
            _ if at == 0 && self.is_scalar(0) => ROOT,
            _ => &[],
        }
    }

    /// The containment of two arrays or two objects, where a scalar at the root is an array.
    fn contains_at(
        &self,
        at: usize,
        other: &JsonTree<'_>,
        other_at: usize,
        depth: usize,
    ) -> Result<bool, TypeError> {
        if depth > MAX_DEPTH {
            return Err(too_deep());
        }
        let object = matches!(self.item(at), JsonItem::Object(_));
        if object != matches!(other.item(other_at), JsonItem::Object(_)) {
            return Ok(false);
        }
        if let (JsonItem::Object(mine), JsonItem::Object(theirs)) =
            (self.item(at), other.item(other_at))
        {
            if mine.len() < theirs.len() {
                return Ok(false);
            }
            for (name, want) in theirs {
                let Some(have) = self.field(at, name) else { return Ok(false) };
                if self.is_scalar(have) || other.is_scalar(*want) {
                    if !self.scalar_equal(have, other, *want) {
                        return Ok(false);
                    }
                } else if !self.contains_at(have, other, *want, depth + 1)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let mine = self.members(at);
        for &want in other.members(other_at) {
            if other.is_scalar(want) {
                if !mine.iter().any(|&have| self.scalar_equal(have, other, want)) {
                    return Ok(false);
                }
                continue;
            }
            let mut found = false;
            for &have in mine.iter().filter(|&&have| !self.is_scalar(have)) {
                if self.contains_at(have, other, want, depth + 1)? {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn is_scalar(&self, at: usize) -> bool {
        !matches!(self.item(at), JsonItem::Array(_) | JsonItem::Object(_))
    }

    fn is_array(&self, at: usize) -> bool {
        matches!(self.item(at), JsonItem::Array(_))
    }

    /// The number of the elements or the fields of a container.
    fn count(&self, at: usize, raw: bool) -> usize {
        match self.item(at) {
            _ if raw => 1,
            JsonItem::Array(items) => items.len(),
            JsonItem::Object(fields) => fields.len(),
            _ => 0,
        }
    }

    /// The order of the types in `enum jbvType`: null, string, numeric, boolean, array and object.
    fn rank(&self, at: usize) -> u8 {
        match self.item(at) {
            JsonItem::Null => 0,
            JsonItem::String(_) => 1,
            JsonItem::Number => 2,
            JsonItem::Bool(_) => 3,
            JsonItem::Array(_) => 16,
            JsonItem::Object(_) => 17,
        }
    }

    /// The type of the value of a token. A key is a string, and the start of the array around a scalar at the root is an array.
    fn token_rank(&self, token: Token<'_>) -> u8 {
        match token {
            Token::Key(..) => 1,
            Token::Begin { raw: true, .. } => 16,
            Token::Begin { node, .. } | Token::Scalar { node, .. } | Token::End { node, .. } => {
                self.rank(node)
            }
        }
    }

    /// `compareJsonbScalarValue` for two scalars of one type. The strings compare by their bytes, as in the C collation.
    fn compare_scalar(&self, at: usize, other: &JsonTree<'_>, other_at: usize) -> Ordering {
        match (self.item(at), other.item(other_at)) {
            (JsonItem::String(x), JsonItem::String(y)) => x.as_bytes().cmp(y.as_bytes()),
            (JsonItem::Bool(x), JsonItem::Bool(y)) => x.cmp(y),
            (JsonItem::Number, JsonItem::Number) => {
                match (numeric_in(self.text(at), -1), numeric_in(other.text(other_at), -1)) {
                    (Ok(x), Ok(y)) => x.compare(&y),
                    _ => self.text(at).cmp(other.text(other_at)),
                }
            }
            _ => Ordering::Equal,
        }
    }

    /// `equalsJsonbScalarValue`: two scalars of one type with one value.
    fn scalar_equal(&self, at: usize, other: &JsonTree<'_>, other_at: usize) -> bool {
        self.is_scalar(at)
            && other.is_scalar(other_at)
            && self.rank(at) == other.rank(other_at)
            && self.compare_scalar(at, other, other_at) == Ordering::Equal
    }

    /// A scalar as `jsonb` prints it: a string with the escapes of `escape_json` and the other kinds as their text.
    fn put_scalar(&self, at: usize, out: &mut String) {
        match self.item(at) {
            JsonItem::String(s) => escape(s, out),
            _ => out.push_str(self.text(at)),
        }
    }
}

/// The elements of the array around a scalar at the root.
const ROOT: &[usize] = &[0];

/// An operation of [`JsonTree::set_path`], as the flags `JB_PATH_*` of `jsonfuncs.c` give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOp {
    /// Replace the part, or add it when the last step of the path is not in the value: `jsonb_set` with `create`.
    Create,
    /// Replace the part only: `jsonb_set` without `create`.
    Replace,
    /// Remove the part: `#-`.
    Delete,
    /// `jsonb_insert` before the element.
    InsertBefore,
    /// `jsonb_insert` after the element.
    InsertAfter,
}

impl PathOp {
    /// `JB_PATH_CREATE_OR_INSERT`.
    fn creates(self) -> bool {
        matches!(self, PathOp::Create | PathOp::InsertBefore | PathOp::InsertAfter)
    }

    fn inserts(self) -> bool {
        matches!(self, PathOp::InsertBefore | PathOp::InsertAfter)
    }
}

/// `strtoint` in base 10 with nothing after the number, as the functions of `jsonfuncs.c` read an index in a path. The leading white space of `isspace` and a sign are permitted.
pub fn json_index(s: &str) -> Option<i32> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']);
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Puts the separator before an element or a field when it is not the first one.
fn put_comma(out: &mut String, first: &mut bool) {
    if !std::mem::take(first) {
        out.push_str(", ");
    }
}

/// Puts the separator and the text of an element.
fn put_element(out: &mut String, first: &mut bool, text: &str) {
    put_comma(out, first);
    out.push_str(text);
}

/// Puts the separator and a key with its colon.
fn put_key(out: &mut String, first: &mut bool, name: &str) {
    put_comma(out, first);
    escape(name, out);
    out.push_str(": ");
}

/// Puts the separator and a field.
fn put_field(out: &mut String, first: &mut bool, name: &str, value: &str) {
    put_key(out, first, name);
    out.push_str(value);
}

/// A token of [`JsonTree::tokens`].
#[derive(Debug, Clone, Copy)]
pub enum Token<'t> {
    /// The start of an array or an object, with true in `elem` when it is an element of an array. `raw` marks the array around a scalar at the root.
    Begin {
        node: usize,
        raw: bool,
        elem: bool,
    },
    /// A key, with the node of its value.
    Key(&'t str, usize),
    /// A scalar, with true when it is an element of an array. The scalar at the root is not an element.
    Scalar {
        node: usize,
        elem: bool,
    },
    End {
        node: usize,
        raw: bool,
    },
}

#[derive(Debug)]
struct Frame {
    node: usize,
    at: usize,
    raw: bool,
}

/// The tokens of a value, in the order of `JsonbIteratorNext` with no nested values skipped.
#[derive(Debug)]
pub struct Tokens<'t, 'a> {
    tree: &'t JsonTree<'a>,
    stack: Vec<Frame>,
    start: bool,
    /// The value of the last key, which is the next token.
    value: Option<usize>,
}

impl<'t> Tokens<'t, '_> {
    fn enter(&mut self, node: usize, elem: bool) -> Token<'t> {
        if self.tree.is_scalar(node) {
            Token::Scalar { node, elem }
        } else {
            self.stack.push(Frame { node, at: 0, raw: false });
            Token::Begin { node, raw: false, elem }
        }
    }
}

impl<'t> Iterator for Tokens<'t, '_> {
    type Item = Token<'t>;

    fn next(&mut self) -> Option<Token<'t>> {
        if let Some(value) = self.value.take() {
            return Some(self.enter(value, false));
        }
        if self.start {
            self.start = false;
            if self.tree.is_scalar(0) {
                self.stack.push(Frame { node: 0, at: 0, raw: true });
                return Some(Token::Begin { node: 0, raw: true, elem: false });
            }
            return Some(self.enter(0, false));
        }
        let tree = self.tree;
        let frame = self.stack.last_mut()?;
        let (node, at, raw) = (frame.node, frame.at, frame.raw);
        frame.at += 1;
        if raw {
            if at == 0 {
                return Some(Token::Scalar { node, elem: false });
            }
        } else {
            match tree.item(node) {
                JsonItem::Array(items) => {
                    if let Some(&child) = items.get(at) {
                        return Some(self.enter(child, true));
                    }
                }
                JsonItem::Object(fields) => {
                    if let Some((name, value)) = fields.get(at) {
                        self.value = Some(*value);
                        return Some(Token::Key(name, *value));
                    }
                }
                _ => {}
            }
        }
        self.stack.pop();
        Some(Token::End { node, raw })
    }
}

//! `jsonb` in the text and the binary formats.
//!
//! PostgreSQL keeps a `jsonb` value as a tree, with the keys of each object in one order and no key twice, and prints the tree in one form. rupg keeps the text of that form. Then the output is a copy of the value, and the functions of `json` can read it. This is a port of `jsonb_in` in `src/backend/utils/adt/jsonb.c`, of the order of keys of `lengthCompareJsonbStringValue` in `jsonb_util.c` and of the output of `JsonbToCString`.
//!
//! The tree is in one vector and refers to its nodes by index, so a value nested to any depth takes no recursion to build, to print or to drop.
//!
//! Lifted from `crates/rudb-pgtypes/src/jsonb.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::TypeError;
use crate::json::{self, Kind, Open, Sink, Stop};
use crate::numeric::{numeric_in, numeric_out};

/// The version byte at the start of the binary format, the only one that PostgreSQL knows.
const VERSION: u8 = 1;

enum Node {
    /// A scalar, as its output text.
    Scalar(String),
    Array(Vec<usize>),
    /// The fields, each a decoded key and the node of its value.
    Object(Vec<(String, usize)>),
}

#[derive(Default)]
struct Tree {
    nodes: Vec<Node>,
    /// The open arrays and objects, innermost last, each with the key of its next field.
    open: Vec<(usize, Option<String>)>,
    root: Option<usize>,
}

impl Tree {
    /// Puts a finished node into the array or the object around it.
    fn attach(&mut self, node: usize) {
        let Some((parent, key)) = self.open.last_mut() else {
            self.root = Some(node);
            return;
        };
        match &mut self.nodes[*parent] {
            Node::Array(items) => items.push(node),
            Node::Object(fields) => fields.push((key.take().unwrap_or_default(), node)),
            Node::Scalar(_) => {}
        }
    }

    /// The text of the tree, with one space after each colon and each comma.
    fn print(&self, out: &mut String) {
        let Some(root) = self.root else { return };
        // Each entry is a node and the place of its next child.
        let mut stack = vec![(root, 0usize)];
        while let Some((node, at)) = stack.pop() {
            let child = match &self.nodes[node] {
                Node::Scalar(text) => {
                    out.push_str(text);
                    continue;
                }
                Node::Array(items) => {
                    if at == 0 {
                        out.push('[');
                    }
                    let Some(&child) = items.get(at) else {
                        out.push(']');
                        continue;
                    };
                    if at > 0 {
                        out.push_str(", ");
                    }
                    child
                }
                Node::Object(fields) => {
                    if at == 0 {
                        out.push('{');
                    }
                    let Some((key, child)) = fields.get(at) else {
                        out.push('}');
                        continue;
                    };
                    if at > 0 {
                        out.push_str(", ");
                    }
                    escape(key, out);
                    out.push_str(": ");
                    *child
                }
            };
            stack.push((node, at + 1));
            stack.push((child, 0));
        }
    }
}

impl Sink for Tree {
    fn open(&mut self, open: Open) {
        let node = self.nodes.len();
        self.nodes.push(match open {
            Open::Array => Node::Array(Vec::new()),
            Open::Object => Node::Object(Vec::new()),
        });
        self.open.push((node, None));
    }

    fn close(&mut self) {
        let Some((node, _)) = self.open.pop() else { return };
        if let Node::Object(fields) = &mut self.nodes[node] {
            unique(fields);
        }
        self.attach(node);
    }

    fn key(&mut self, key: &str) {
        if let Some((_, slot)) = self.open.last_mut() {
            *slot = Some(key.to_owned());
        }
    }

    fn scalar(&mut self, kind: Kind, token: &str, text: &str) -> Result<(), TypeError> {
        let mut out = String::new();
        match kind {
            Kind::String => escape(text, &mut out),
            Kind::Number => {
                let mut bytes = Vec::new();
                numeric_out(&numeric_in(token, -1)?, &mut bytes);
                out = String::from_utf8(bytes).unwrap_or_default();
            }
            _ => out.push_str(token),
        }
        let node = self.nodes.len();
        self.nodes.push(Node::Scalar(out));
        self.attach(node);
        Ok(())
    }
}

/// `uniqueifyJsonbObject`: the fields in the order of their keys, shorter keys first and keys of one length in the order of their bytes. Of the fields with one key, the last one stays.
fn unique(fields: &mut Vec<(String, usize)>) {
    let order = |a: &str, b: &str| a.len().cmp(&b.len()).then_with(|| a.cmp(b));
    // A stable sort keeps the fields of one key in the order of the input.
    fields.sort_by(|a, b| order(&a.0, &b.0));
    let mut kept: Vec<(String, usize)> = Vec::with_capacity(fields.len());
    for field in fields.drain(..) {
        match kept.last_mut() {
            Some(last) if last.0 == field.0 => *last = field,
            _ => kept.push(field),
        }
    }
    *fields = kept;
}

/// `escape_json`: a string in quotes, with the escapes that PostgreSQL writes.
fn escape(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The text input of `jsonb`: the text of the value in the one form that PostgreSQL prints.
pub fn jsonb_in(s: &str) -> Result<String, TypeError> {
    let mut tree = Tree::default();
    match json::parse(s, true, &mut tree) {
        Ok(()) => {}
        Err(Stop::Syntax(fail)) => return Err(json::syntax(s, fail)),
        Err(Stop::Action(error)) => return Err(error),
    }
    let mut out = String::with_capacity(s.len());
    tree.print(&mut out);
    Ok(out)
}

/// The binary input of `jsonb`: the version byte and then the text.
pub fn jsonb_recv(bytes: &[u8]) -> Result<String, TypeError> {
    match bytes.split_first() {
        Some((&VERSION, text)) => match std::str::from_utf8(text) {
            Ok(text) => jsonb_in(text),
            Err(_) => Err(TypeError::new(
                rupg_common::SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                "invalid byte sequence for encoding \"UTF8\"".to_string(),
            )),
        },
        Some((&version, _)) => Err(TypeError::new(
            rupg_common::SqlState::INTERNAL_ERROR,
            format!("unsupported jsonb version number {version}"),
        )),
        None => Err(TypeError::new(
            rupg_common::SqlState::PROTOCOL_VIOLATION,
            "insufficient data left in message".to_string(),
        )),
    }
}

/// The binary output of `jsonb`: the version byte and then the text.
pub fn jsonb_send(text: &str, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(s: &str) -> String {
        jsonb_in(s).unwrap()
    }

    fn fails(s: &str) -> (String, String, String) {
        let error = jsonb_in(s).unwrap_err();
        (error.sqlstate.as_str().to_owned(), error.message, error.detail.unwrap_or_default())
    }

    #[test]
    fn a_value_prints_in_the_form_of_postgres() {
        assert_eq!(out(r#"{"b":1,"a":[1,2]}"#), r#"{"a": [1, 2], "b": 1}"#);
        assert_eq!(out(r#"{"aa":1,"b":2,"a":3}"#), r#"{"a": 3, "b": 2, "aa": 1}"#);
        assert_eq!(out(r#"{"a":1,"a":2,"b":{"c":1,"c":[]}}"#), r#"{"a": 2, "b": {"c": []}}"#);
        assert_eq!(out(" [ ] "), "[]");
        assert_eq!(out("{}"), "{}");
        assert_eq!(out("[1.50, 1e2, -0, 1E-3, -0.0, 2.5e+1]"), "[1.50, 100, 0, 0.001, 0.0, 25]");
        assert_eq!(out(" true "), "true");
        assert_eq!(out("null"), "null");
        assert_eq!(out(r#""aé\/\"\n\u0001😀""#), "\"a\u{e9}/\\\"\\n\\u0001\u{1f600}\"");
        assert_eq!(out(r#"[{"é":1,"z":2}]"#), r#"[{"z": 2, "é": 1}]"#);
    }

    #[test]
    fn the_errors_are_the_errors_of_postgres() {
        let syntax = |detail: &str| {
            ("22P02".to_owned(), "invalid input syntax for type json".to_owned(), detail.to_owned())
        };
        assert_eq!(fails("[1 2]"), syntax(r#"Expected "," or "]", but found "2"."#));
        assert_eq!(
            fails(r#"["\ud800"]"#),
            syntax("Unicode low surrogate must follow a high surrogate.")
        );
        assert_eq!(
            fails(r#""\ude00""#),
            syntax("Unicode low surrogate must follow a high surrogate.")
        );
        assert_eq!(
            fails(r#""\ud800\ud800""#),
            syntax("Unicode high surrogate must not follow a high surrogate.")
        );
        assert_eq!(
            fails(r#""\ud800x""#),
            syntax("Unicode low surrogate must follow a high surrogate.")
        );
        assert_eq!(
            fails(r#"["\u0000"]"#),
            (
                "22P05".to_owned(),
                "unsupported Unicode escape sequence".to_owned(),
                "\\u0000 cannot be converted to text.".to_owned()
            )
        );
        // The lexer decodes a string before the parser looks at where it is.
        assert_eq!(fails(r#"{"a": 1 "\u0000"}"#).0, "22P05");
        assert!(json::json_in(r#"["\u0000", "\ud800"]"#).is_ok());
    }

    #[test]
    fn deep_nesting_needs_no_recursion() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(out(&deep), deep);
    }

    #[test]
    fn the_binary_format_is_a_version_byte_and_the_text() {
        let mut bytes = Vec::new();
        jsonb_send(r#"{"a": 1}"#, &mut bytes);
        assert_eq!(bytes, b"\x01{\"a\": 1}");
        assert_eq!(jsonb_recv(b"\x01{\"b\":1,\"a\":2}").unwrap(), r#"{"a": 2, "b": 1}"#);
        assert_eq!(
            jsonb_recv(b"\x02{}").unwrap_err().message,
            "unsupported jsonb version number 2"
        );
        let empty = jsonb_recv(b"").unwrap_err();
        assert_eq!(empty.sqlstate, rupg_common::SqlState::PROTOCOL_VIOLATION);
    }
}

//! The forms of the output of `EXPLAIN`, as `explain_format.c`: the groups and the properties in text, XML, JSON and YAML.

use crate::Format;

/// The output of `EXPLAIN` while it grows, as the parts of `ExplainState` that the forms use.
#[derive(Debug)]
pub(crate) struct Writer {
    pub(crate) format: Format,
    pub(crate) buf: String,
    /// `indent`: the level of the current line.
    pub(crate) indent: usize,
    /// `grouping_stack`: for JSON, true when the group has an item; for YAML, true when the group has a label.
    stack: Vec<bool>,
}

impl Writer {
    pub(crate) fn new(format: Format) -> Writer {
        Writer { format, buf: String::new(), indent: 0, stack: Vec::new() }
    }

    /// `ExplainBeginOutput`.
    pub(crate) fn begin(&mut self) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.buf.push_str("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n");
                self.indent += 1;
            }
            Format::Json => {
                self.buf.push('[');
                self.stack.push(false);
                self.indent += 1;
            }
            Format::Yaml => self.stack.push(false),
        }
    }

    /// `ExplainEndOutput`.
    pub(crate) fn end(&mut self) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.buf.push_str("</explain>");
            }
            Format::Json => {
                self.indent -= 1;
                self.buf.push_str("\n]");
                self.stack.pop();
            }
            Format::Yaml => {
                self.stack.pop();
            }
        }
    }

    /// `ExplainOpenGroup`: a group of properties of the type `objtype`, with the label `label` in JSON and YAML. A group is an object when `labeled` is true, or else a list.
    pub(crate) fn open_group(&mut self, objtype: &str, label: Option<&str>, labeled: bool) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.xml_tag(objtype, false, true);
                self.indent += 1;
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                if let Some(label) = label {
                    escape_json(&mut self.buf, label);
                    self.buf.push_str(": ");
                }
                self.buf.push(if labeled { '{' } else { '[' });
                self.stack.push(false);
                self.indent += 1;
            }
            Format::Yaml => {
                self.yaml_line_starting();
                if let Some(label) = label {
                    self.buf.push_str(label);
                    self.buf.push_str(": ");
                    self.stack.push(true);
                } else {
                    self.buf.push_str("- ");
                    self.stack.push(false);
                }
                self.indent += 1;
            }
        }
    }

    /// `ExplainCloseGroup`.
    pub(crate) fn close_group(&mut self, objtype: &str, labeled: bool) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.xml_tag(objtype, true, true);
            }
            Format::Json => {
                self.indent -= 1;
                self.buf.push('\n');
                self.spaces(2 * self.indent);
                self.buf.push(if labeled { '}' } else { ']' });
                self.stack.pop();
            }
            Format::Yaml => {
                self.indent -= 1;
                self.stack.pop();
            }
        }
    }

    /// `ExplainProperty`: a property with a text, or a number or a Boolean value when `numeric` is true, which JSON and YAML do not quote. Text adds the unit after the value.
    pub(crate) fn property(&mut self, label: &str, unit: Option<&str>, value: &str, numeric: bool) {
        match self.format {
            Format::Text => {
                self.indent_text();
                self.buf.push_str(label);
                self.buf.push_str(": ");
                self.buf.push_str(value);
                if let Some(unit) = unit {
                    self.buf.push(' ');
                    self.buf.push_str(unit);
                }
                self.buf.push('\n');
            }
            Format::Xml => {
                self.spaces(2 * self.indent);
                self.xml_tag(label, false, false);
                escape_xml(&mut self.buf, value);
                self.xml_tag(label, true, false);
                self.buf.push('\n');
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                escape_json(&mut self.buf, label);
                self.buf.push_str(": ");
                if numeric {
                    self.buf.push_str(value);
                } else {
                    escape_json(&mut self.buf, value);
                }
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.buf.push_str(label);
                self.buf.push_str(": ");
                if numeric {
                    self.buf.push_str(value);
                } else {
                    escape_json(&mut self.buf, value);
                }
            }
        }
    }

    /// `ExplainPropertyText`.
    pub(crate) fn text(&mut self, label: &str, value: &str) {
        self.property(label, None, value, false);
    }

    /// `ExplainPropertyBool`.
    pub(crate) fn boolean(&mut self, label: &str, value: bool) {
        self.property(label, None, if value { "true" } else { "false" }, true);
    }

    /// `ExplainPropertyFloat`: a number with `digits` digits after the point.
    pub(crate) fn float(&mut self, label: &str, unit: Option<&str>, value: f64, digits: usize) {
        self.property(label, unit, &format!("{value:.digits$}"), true);
    }

    /// `ExplainPropertyInteger`.
    pub(crate) fn integer(&mut self, label: &str, unit: Option<&str>, value: i64) {
        self.property(label, unit, &value.to_string(), true);
    }

    /// `ExplainPropertyList`: a property with a list of texts.
    pub(crate) fn list(&mut self, label: &str, items: &[String]) {
        match self.format {
            Format::Text => {
                self.indent_text();
                self.buf.push_str(label);
                self.buf.push_str(": ");
                self.buf.push_str(&items.join(", "));
                self.buf.push('\n');
            }
            Format::Xml => {
                self.xml_tag(label, false, true);
                for item in items {
                    self.spaces(2 * self.indent + 2);
                    self.buf.push_str("<Item>");
                    escape_xml(&mut self.buf, item);
                    self.buf.push_str("</Item>\n");
                }
                self.xml_tag(label, true, true);
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                escape_json(&mut self.buf, label);
                self.buf.push_str(": [");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    escape_json(&mut self.buf, item);
                }
                self.buf.push(']');
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.buf.push_str(label);
                self.buf.push_str(": ");
                for item in items {
                    self.buf.push('\n');
                    self.spaces(2 * self.indent + 2);
                    self.buf.push_str("- ");
                    escape_json(&mut self.buf, item);
                }
            }
        }
    }

    /// `ExplainPropertyListNested`: a list of texts that is an item of a list, with no label in JSON and YAML.
    pub(crate) fn list_nested(&mut self, label: &str, items: &[String]) {
        let start = match self.format {
            Format::Text | Format::Xml => return self.list(label, items),
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                "["
            }
            Format::Yaml => {
                self.yaml_line_starting();
                "- ["
            }
        };
        self.buf.push_str(start);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            escape_json(&mut self.buf, item);
        }
        self.buf.push(']');
    }

    /// `ExplainIndentText`: the spaces of the level at the start of a line of text.
    pub(crate) fn indent_text(&mut self) {
        if self.buf.is_empty() || self.buf.ends_with('\n') {
            self.spaces(2 * self.indent);
        }
    }

    /// `appendStringInfoSpaces`.
    fn spaces(&mut self, count: usize) {
        for _ in 0..count {
            self.buf.push(' ');
        }
    }

    /// `ExplainXMLTag`: an opening or a closing tag. Each character of the name that is not a letter, a digit, `-`, `_` or `.` becomes `-`. With `whitespace` the tag has its own line.
    fn xml_tag(&mut self, name: &str, closing: bool, whitespace: bool) {
        if whitespace {
            self.spaces(2 * self.indent);
        }
        self.buf.push('<');
        if closing {
            self.buf.push('/');
        }
        for c in name.chars() {
            let keep = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
            self.buf.push(if keep { c } else { '-' });
        }
        self.buf.push('>');
        if whitespace {
            self.buf.push('\n');
        }
    }

    /// `ExplainJSONLineEnding`: a comma after the item before, and a new line.
    fn json_line_ending(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            if *top {
                self.buf.push(',');
            } else {
                *top = true;
            }
        }
        self.buf.push('\n');
    }

    /// `ExplainYAMLLineStarting`: a new line with the indent, except for the first item of a group with no label, which goes after its `- `.
    fn yaml_line_starting(&mut self) {
        match self.stack.last_mut() {
            Some(top) if !*top => *top = true,
            _ => {
                self.buf.push('\n');
                self.spaces(2 * self.indent);
            }
        }
    }
}

/// `escape_json`: the text in double quotes, with the escapes of JSON.
pub(crate) fn escape_json(buf: &mut String, text: &str) {
    buf.push('"');
    for c in text.chars() {
        match c {
            '\u{8}' => buf.push_str("\\b"),
            '\u{c}' => buf.push_str("\\f"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            c if u32::from(c) < 0x20 => buf.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => buf.push(c),
        }
    }
    buf.push('"');
}

/// `escape_xml`: the text with the entities of `&`, `<`, `>` and a carriage return.
fn escape_xml(buf: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '>' => buf.push_str("&gt;"),
            '\r' => buf.push_str("&#x0d;"),
            c => buf.push(c),
        }
    }
}

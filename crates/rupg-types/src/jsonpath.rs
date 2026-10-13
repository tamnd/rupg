//! The `jsonpath` type, from `jsonpath_scan.l`, `jsonpath_gram.y` and `jsonpath.c`. A value is the tree of the path, as `flattenJsonPathParseItem` writes it. The text form is the text of `printJsonPathItem`, and the binary form is the version byte 1 and the text form.
//!
//! The scanner gives the tokens that the scanner of flex gives: the longest match wins, and the first rule wins a tie. The text of a token is the text that flex has in `yytext`, so a syntax error names the same text as PostgreSQL. The parser follows the grammar, and it stops at the first token that the grammar cannot take, as bison does.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::jsonb::escape;
use crate::number::int4_in;
use crate::numeric::{Numeric, numeric_in, numeric_out};
use crate::tsquery::too_deep;

/// `JSP_REGEX_ICASE`: the flag `i` of `like_regex`.
pub const JSONPATH_REGEX_ICASE: u8 = 0x01;

/// `JSP_REGEX_DOTALL`: the flag `s` of `like_regex`.
pub const JSONPATH_REGEX_DOTALL: u8 = 0x02;

/// `JSP_REGEX_MLINE`: the flag `m` of `like_regex`.
pub const JSONPATH_REGEX_MLINE: u8 = 0x04;

/// `JSP_REGEX_WSPACE`: the flag `x` of `like_regex`.
pub const JSONPATH_REGEX_WSPACE: u8 = 0x08;

/// `JSP_REGEX_QUOTE`: the flag `q` of `like_regex`.
pub const JSONPATH_REGEX_QUOTE: u8 = 0x10;

/// The bits of `cflags` in `regex.h` that [`jsonpath_regex_flags`] gives.
const REG_ADVANCED: u32 = 0o3;
const REG_QUOTE: u32 = 0o4;
const REG_ICASE: u32 = 0o10;
const REG_NLSTOP: u32 = 0o100;
const REG_NLANCH: u32 = 0o200;

/// The deepest nesting of parentheses, operators and filters. PostgreSQL stops a deep path when it reaches `max_stack_depth`. The parser here works by recursion, and this limit keeps it on the stack. It is the limit of the analyzer.
const MAX_DEPTH: usize = 1000;

/// The `last` bound of `.**{}`, as `PG_UINT32_MAX`.
pub const JSONPATH_LAST: u32 = u32::MAX;

/// An operator or a method of a path, as `JsonPathItemType` has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOp {
    And,
    Or,
    Not,
    IsUnknown,
    Equal,
    NotEqual,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Plus,
    Minus,
    Filter,
    Exists,
    StartsWith,
    Type,
    Size,
    Abs,
    Floor,
    Ceiling,
    Double,
    Datetime,
    KeyValue,
    Bigint,
    Boolean,
    Date,
    Decimal,
    Integer,
    Number,
    StringFunc,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Replace,
    Lower,
    Upper,
    Ltrim,
    Rtrim,
    Btrim,
    Initcap,
    SplitPart,
}

impl PathOp {
    /// `jspOperationName`: the name of the operator or the method in the text form and in the errors of the executor. The operators `!`, `is unknown`, `?` and `exists` have no name there, and the name here is their text.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PathOp::And => "&&",
            PathOp::Or => "||",
            PathOp::Not => "!",
            PathOp::IsUnknown => "is unknown",
            PathOp::Equal => "==",
            PathOp::NotEqual => "!=",
            PathOp::Less => "<",
            PathOp::Greater => ">",
            PathOp::LessOrEqual => "<=",
            PathOp::GreaterOrEqual => ">=",
            PathOp::Add | PathOp::Plus => "+",
            PathOp::Sub | PathOp::Minus => "-",
            PathOp::Mul => "*",
            PathOp::Div => "/",
            PathOp::Mod => "%",
            PathOp::Filter => "?",
            PathOp::Exists => "exists",
            PathOp::StartsWith => "starts with",
            PathOp::Type => "type",
            PathOp::Size => "size",
            PathOp::Abs => "abs",
            PathOp::Floor => "floor",
            PathOp::Ceiling => "ceiling",
            PathOp::Double => "double",
            PathOp::Datetime => "datetime",
            PathOp::KeyValue => "keyvalue",
            PathOp::Bigint => "bigint",
            PathOp::Boolean => "boolean",
            PathOp::Date => "date",
            PathOp::Decimal => "decimal",
            PathOp::Integer => "integer",
            PathOp::Number => "number",
            PathOp::StringFunc => "string",
            PathOp::Time => "time",
            PathOp::TimeTz => "time_tz",
            PathOp::Timestamp => "timestamp",
            PathOp::TimestampTz => "timestamp_tz",
            PathOp::Replace => "replace",
            PathOp::Lower => "lower",
            PathOp::Upper => "upper",
            PathOp::Ltrim => "ltrim",
            PathOp::Rtrim => "rtrim",
            PathOp::Btrim => "btrim",
            PathOp::Initcap => "initcap",
            PathOp::SplitPart => "split_part",
        }
    }

    /// `operationPriority`: an operand with a priority that is not higher than the priority of its operator is in parentheses in the text form.
    fn priority(self) -> u8 {
        match self {
            PathOp::Or => 0,
            PathOp::And => 1,
            PathOp::Equal
            | PathOp::NotEqual
            | PathOp::Less
            | PathOp::Greater
            | PathOp::LessOrEqual
            | PathOp::GreaterOrEqual
            | PathOp::StartsWith => 2,
            PathOp::Add | PathOp::Sub => 3,
            PathOp::Mul | PathOp::Div | PathOp::Mod => 4,
            PathOp::Plus | PathOp::Minus => 5,
            _ => 6,
        }
    }
}

/// One item of a path. The items that follow an item, its `next` items in PostgreSQL, are the items after it in its [`Chain`].
#[derive(Clone, Debug, PartialEq)]
pub enum PathItem {
    /// `null`.
    Null,
    /// A string literal.
    String(String),
    /// A number literal.
    Numeric(Numeric),
    /// `true` or `false`.
    Bool(bool),
    /// `&&`, `||`, a comparison, an arithmetic operator or `starts with`, with the left and the right operand.
    Binary(PathOp, Chain, Chain),
    /// `!`, `is unknown`, unary `+` and `-`, the filter `?()` and `exists`, with the operand.
    Unary(PathOp, Chain),
    /// `like_regex`: the operand, the pattern and the bits of the flags.
    LikeRegex { expr: Chain, pattern: String, flags: u8 },
    /// `[*]`.
    AnyArray,
    /// `.*`.
    AnyKey,
    /// `[...]`: each subscript, with the end of a range `to`.
    IndexArray(Vec<(Chain, Option<Chain>)>),
    /// `.**`, with the first and the last level. [`JSONPATH_LAST`] is `last`.
    Any { first: u32, last: u32 },
    /// `.key`.
    Key(String),
    /// `@`.
    Current,
    /// `$`.
    Root,
    /// `$name`.
    Variable(String),
    /// `last`.
    Last,
    /// A method such as `.type()`, with its arguments. An argument is a string or a number.
    Method(PathOp, Vec<PathItem>),
}

impl PathItem {
    /// The priority of the item as an operand.
    fn priority(&self) -> u8 {
        match self {
            PathItem::Binary(op, ..) | PathItem::Unary(op, _) => op.priority(),
            _ => 6,
        }
    }
}

/// An item and the items that follow it. A chain is never empty.
pub type Chain = Vec<PathItem>;

/// A value of `jsonpath`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonPath {
    /// False for `strict`.
    pub lax: bool,
    /// The path.
    pub expr: Chain,
}

/// `jspConvertRegexFlags`: the `cflags` of the regular expression of `like_regex` with the flags `flags`.
///
/// # Errors
///
/// `0A000` for the flag `x` with no flag `q`.
pub fn jsonpath_regex_flags(flags: u8) -> Result<u32, TypeError> {
    let mut cflags = REG_ADVANCED;
    if flags & JSONPATH_REGEX_ICASE != 0 {
        cflags |= REG_ICASE;
    }
    if flags & JSONPATH_REGEX_QUOTE != 0 {
        cflags &= !REG_ADVANCED;
        cflags |= REG_QUOTE;
    } else {
        if flags & JSONPATH_REGEX_DOTALL == 0 {
            cflags |= REG_NLSTOP;
        }
        if flags & JSONPATH_REGEX_MLINE != 0 {
            cflags |= REG_NLANCH;
        }
        if flags & JSONPATH_REGEX_WSPACE != 0 {
            return Err(TypeError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "XQuery \"x\" flag (expanded regular expressions) is not implemented".to_string(),
            ));
        }
    }
    Ok(cflags)
}

/// The check of a regular expression: the pattern and the `cflags`, and the message of `pg_regerror` for a pattern that is not valid.
pub type RegexCheck<'a> = &'a dyn Fn(&str, u32) -> Result<(), String>;

/// The error `22P02` of the type with a detail.
fn bad_input(detail: &str) -> TypeError {
    let mut error = TypeError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type jsonpath".to_string(),
    );
    error.detail = Some(detail.to_string());
    error
}

/// `jsonpath_yyerror`: the message and the text of the token, or the end of the input when the text is empty.
fn yyerror(message: &str, text: &[u8]) -> TypeError {
    let message = if text.is_empty() {
        format!("{message} at end of jsonpath input")
    } else {
        format!("{message} at or near \"{}\" of jsonpath input", String::from_utf8_lossy(text))
    };
    TypeError::new(SqlState::SYNTAX_ERROR, message)
}

/// `special`: a character that is a token by itself.
fn is_special(b: u8) -> bool {
    b"?%$.[]{}()|&!=<>@#,*:-+/".contains(&b)
}

/// `blank`.
fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// `other`: a byte that is not special, not blank and not `\` or `"`.
fn is_other(b: u8) -> bool {
    !is_special(b) && !is_blank(b) && b != b'\\' && b != b'"'
}

/// A key word of the scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kw {
    Is,
    To,
    Lax,
    Flag,
    Last,
    Null,
    True,
    With,
    False,
    Exists,
    Starts,
    Strict,
    Unknown,
    LikeRegex,
    /// A key word that names a method.
    Method(PathOp),
}

/// `keywords`: the key words, and true for a key word that matches only in lower case.
const KEYWORDS: [(&str, bool, Kw); 41] = [
    ("is", false, Kw::Is),
    ("to", false, Kw::To),
    ("abs", false, Kw::Method(PathOp::Abs)),
    ("lax", false, Kw::Lax),
    ("date", false, Kw::Method(PathOp::Date)),
    ("flag", false, Kw::Flag),
    ("last", false, Kw::Last),
    ("null", true, Kw::Null),
    ("size", false, Kw::Method(PathOp::Size)),
    ("time", false, Kw::Method(PathOp::Time)),
    ("true", true, Kw::True),
    ("type", false, Kw::Method(PathOp::Type)),
    ("with", false, Kw::With),
    ("btrim", false, Kw::Method(PathOp::Btrim)),
    ("false", true, Kw::False),
    ("floor", false, Kw::Method(PathOp::Floor)),
    ("lower", false, Kw::Method(PathOp::Lower)),
    ("ltrim", false, Kw::Method(PathOp::Ltrim)),
    ("rtrim", false, Kw::Method(PathOp::Rtrim)),
    ("upper", false, Kw::Method(PathOp::Upper)),
    ("bigint", false, Kw::Method(PathOp::Bigint)),
    ("double", false, Kw::Method(PathOp::Double)),
    ("exists", false, Kw::Exists),
    ("number", false, Kw::Method(PathOp::Number)),
    ("starts", false, Kw::Starts),
    ("strict", false, Kw::Strict),
    ("string", false, Kw::Method(PathOp::StringFunc)),
    ("boolean", false, Kw::Method(PathOp::Boolean)),
    ("ceiling", false, Kw::Method(PathOp::Ceiling)),
    ("decimal", false, Kw::Method(PathOp::Decimal)),
    ("initcap", false, Kw::Method(PathOp::Initcap)),
    ("integer", false, Kw::Method(PathOp::Integer)),
    ("replace", false, Kw::Method(PathOp::Replace)),
    ("time_tz", false, Kw::Method(PathOp::TimeTz)),
    ("unknown", false, Kw::Unknown),
    ("datetime", false, Kw::Method(PathOp::Datetime)),
    ("keyvalue", false, Kw::Method(PathOp::KeyValue)),
    ("timestamp", false, Kw::Method(PathOp::Timestamp)),
    ("like_regex", false, Kw::LikeRegex),
    ("split_part", false, Kw::Method(PathOp::SplitPart)),
    ("timestamp_tz", false, Kw::Method(PathOp::TimestampTz)),
];

/// `checkKeyword`: the key word that a name is, if any.
fn keyword(name: &[u8]) -> Option<Kw> {
    KEYWORDS.iter().find_map(|&(word, lower, kw)| {
        let found = if lower {
            name == word.as_bytes()
        } else {
            name.eq_ignore_ascii_case(word.as_bytes())
        };
        found.then_some(kw)
    })
}

/// The kind of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    End,
    And,
    Or,
    Not,
    /// `**`.
    AnyLevels,
    Less,
    LessEqual,
    Equal,
    NotEqual,
    GreaterEqual,
    Greater,
    /// A special character.
    Char(u8),
    Variable,
    Str,
    Numeric,
    Int,
    Ident,
    Kw(Kw),
}

/// A token: the kind, the bounds of the text of flex in the input, and the string of a name, a string, a variable or a number.
struct Token {
    tok: Tok,
    text: (usize, usize),
    value: Vec<u8>,
}

/// The rules of the start state of the scanner that can match at a position, in the order of `jsonpath_scan.l`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    Op(Tok, usize),
    Variable,
    QuotedVariable,
    Special,
    Blank,
    Comment,
    Numeric,
    Int,
    RealFail,
    Junk,
    Quote,
    Backslash,
    Name,
}

/// The rules of an escape in a name or a string, in the order of `jsonpath_scan.l`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Char(u8),
    Unicode,
    Hex,
    UnicodeFail,
    HexFail,
    UnicodeBackslash,
    Any,
    Lone,
}

/// The scanner, at a position of the input.
struct Lexer<'a> {
    s: &'a [u8],
    at: usize,
    /// True for the soft input of `pg_input_is_valid`, which changes the error for a code point that is too large.
    soft: bool,
}

impl Lexer<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.s.get(i).copied().unwrap_or(0)
    }

    fn has(&self, i: usize, f: fn(u8) -> bool) -> bool {
        i < self.s.len() && f(self.s[i])
    }

    /// The ends of the matches of `(_?{digit})*` after a digit that ends at `q`.
    fn digits_from(&self, mut q: usize, digit: fn(u8) -> bool, ends: &mut Vec<usize>) {
        ends.push(q);
        loop {
            if self.has(q, digit) {
                q += 1;
            } else if self.byte(q) == b'_' && self.has(q + 1, digit) {
                q += 2;
            } else {
                return;
            }
            ends.push(q);
        }
    }

    /// The ends of the matches of `decinteger` at `p`.
    fn decinteger(&self, p: usize) -> Vec<usize> {
        let mut ends = Vec::new();
        match self.byte(p) {
            b'0' => ends.push(p + 1),
            b'1'..=b'9' => self.digits_from(p + 1, |b| b.is_ascii_digit(), &mut ends),
            _ => {}
        }
        ends
    }

    /// The ends of the matches of `decdigits` at `p`.
    fn decdigits(&self, p: usize, ends: &mut Vec<usize>) {
        if self.has(p, |b| b.is_ascii_digit()) {
            self.digits_from(p + 1, |b| b.is_ascii_digit(), ends);
        }
    }

    /// The ends of the matches of `decimal` at `p`.
    fn decimal(&self, p: usize) -> Vec<usize> {
        let mut ends = Vec::new();
        for end in self.decinteger(p) {
            if self.byte(end) == b'.' {
                ends.push(end + 1);
                self.decdigits(end + 1, &mut ends);
            }
        }
        if self.byte(p) == b'.' {
            self.decdigits(p + 1, &mut ends);
        }
        ends
    }

    /// The ends of the matches of `real` at `p`, and of `realfail`.
    fn real(&self, p: usize) -> (Vec<usize>, Vec<usize>) {
        let (mut ends, mut fails) = (Vec::new(), Vec::new());
        let mut bases = self.decinteger(p);
        bases.extend(self.decimal(p));
        for base in bases {
            if matches!(self.byte(base), b'e' | b'E') {
                self.decdigits(base + 1, &mut ends);
                if matches!(self.byte(base + 1), b'+' | b'-') {
                    fails.push(base + 2);
                    self.decdigits(base + 2, &mut ends);
                }
            }
        }
        (ends, fails)
    }

    /// The end of the match of `0x`, `0o` or `0b` and its digits at `p`.
    fn radix(&self, p: usize, letter: u8, digit: fn(u8) -> bool) -> Option<usize> {
        if self.byte(p) == b'0'
            && self.byte(p + 1).to_ascii_lowercase() == letter
            && self.has(p + 2, digit)
        {
            let mut ends = Vec::new();
            self.digits_from(p + 3, digit, &mut ends);
            ends.last().copied()
        } else {
            None
        }
    }

    /// The end of a match of a rule `{x}_junk`: the longest match of `x` that one `other` byte follows.
    fn junk(&self, ends: &[usize]) -> Option<usize> {
        ends.iter().filter(|&&end| self.has(end, is_other)).map(|&end| end + 1).max()
    }

    /// The end of a run of bytes from `p` that `f` takes.
    fn run(&self, mut p: usize, f: fn(u8) -> bool) -> usize {
        while self.has(p, f) {
            p += 1;
        }
        p
    }

    /// The longest rule of the start state at `p`, and the end of its match. The first rule wins a tie.
    fn longest(&self, p: usize) -> (Rule, usize) {
        let mut best: Option<(Rule, usize)> = None;
        let mut offer = |rule: Rule, end: Option<usize>| {
            if let Some(end) = end
                && best.is_none_or(|(_, e)| end > e)
            {
                best = Some((rule, end));
            }
        };
        let ops: [(&[u8], Tok); 11] = [
            (b"&&", Tok::And),
            (b"||", Tok::Or),
            (b"!", Tok::Not),
            (b"**", Tok::AnyLevels),
            (b"<", Tok::Less),
            (b"<=", Tok::LessEqual),
            (b"==", Tok::Equal),
            (b"<>", Tok::NotEqual),
            (b"!=", Tok::NotEqual),
            (b">=", Tok::GreaterEqual),
            (b">", Tok::Greater),
        ];
        for (text, tok) in ops {
            offer(
                Rule::Op(tok, text.len()),
                self.s[p..].starts_with(text).then_some(p + text.len()),
            );
        }
        let dollar = self.byte(p) == b'$';
        offer(
            Rule::Variable,
            (dollar && self.has(p + 1, is_other)).then(|| self.run(p + 1, is_other)),
        );
        offer(Rule::QuotedVariable, (dollar && self.byte(p + 1) == b'"').then_some(p + 2));
        offer(Rule::Special, self.has(p, is_special).then_some(p + 1));
        offer(Rule::Blank, self.has(p, is_blank).then(|| self.run(p, is_blank)));
        offer(Rule::Comment, self.s[p..].starts_with(b"/*").then_some(p + 2));
        let (real, realfail) = self.real(p);
        let decimal = self.decimal(p);
        let decinteger = self.decinteger(p);
        offer(Rule::Numeric, real.iter().max().copied());
        offer(Rule::Numeric, decimal.iter().max().copied());
        offer(Rule::Int, decinteger.iter().max().copied());
        offer(Rule::Int, self.radix(p, b'x', |b| b.is_ascii_hexdigit()));
        offer(Rule::Int, self.radix(p, b'o', |b| matches!(b, b'0'..=b'7')));
        offer(Rule::Int, self.radix(p, b'b', |b| matches!(b, b'0' | b'1')));
        offer(Rule::RealFail, realfail.iter().max().copied());
        offer(Rule::Junk, self.junk(&decinteger));
        offer(Rule::Junk, self.junk(&decimal));
        offer(Rule::Junk, self.junk(&real));
        offer(Rule::Quote, (self.byte(p) == b'"' && p < self.s.len()).then_some(p + 1));
        // The rule `\\` gives back its match, so it ties with nothing.
        offer(Rule::Backslash, (self.byte(p) == b'\\' && p < self.s.len()).then_some(p + 1));
        offer(Rule::Name, self.has(p, is_other).then(|| self.run(p, is_other)));
        best.unwrap_or((Rule::Special, p))
    }

    /// The next token. A name that a comment follows gives no token, as in PostgreSQL.
    fn next(&mut self) -> Result<Token, TypeError> {
        loop {
            let p = self.at;
            if p >= self.s.len() {
                return Ok(Token { tok: Tok::End, text: (p, p), value: Vec::new() });
            }
            let (rule, end) = self.longest(p);
            let token = |tok| Token { tok, text: (p, end), value: self.s[p..end].to_vec() };
            self.at = end;
            match rule {
                Rule::Op(tok, _) => return Ok(token(tok)),
                Rule::Variable => {
                    return Ok(Token {
                        tok: Tok::Variable,
                        text: (p, end),
                        value: self.s[p + 1..end].to_vec(),
                    });
                }
                Rule::QuotedVariable => return self.quoted(Tok::Variable),
                Rule::Special => return Ok(token(Tok::Char(self.s[p]))),
                Rule::Blank => {}
                Rule::Comment => self.comment()?,
                Rule::Numeric => return Ok(token(Tok::Numeric)),
                Rule::Int => return Ok(token(Tok::Int)),
                Rule::RealFail => return Err(yyerror("invalid numeric literal", &self.s[p..end])),
                Rule::Junk => {
                    return Err(yyerror("trailing junk after numeric literal", &self.s[p..end]));
                }
                Rule::Quote => return self.quoted(Tok::Str),
                Rule::Backslash => {
                    self.at = p;
                    if let Some(token) = self.name(Vec::new())? {
                        return Ok(token);
                    }
                }
                Rule::Name => {
                    if let Some(token) = self.name(self.s[p..end].to_vec())? {
                        return Ok(token);
                    }
                }
            }
        }
    }

    /// The state `xc`: the rest of a comment.
    fn comment(&mut self) -> Result<(), TypeError> {
        match self.s[self.at..].windows(2).position(|w| w == b"*/") {
            Some(at) => {
                self.at += at + 2;
                Ok(())
            }
            None => {
                self.at = self.s.len();
                Err(yyerror("unexpected end of comment", b""))
            }
        }
    }

    /// The state `xnq`: the rest of a name. The result is `None` when a comment follows the name, as the scanner of PostgreSQL then drops the name.
    fn name(&mut self, mut value: Vec<u8>) -> Result<Option<Token>, TypeError> {
        loop {
            let p = self.at;
            let end_token = |value: Vec<u8>, text| {
                let tok = keyword(&value).map_or(Tok::Ident, Tok::Kw);
                Some(Token { tok, text, value })
            };
            if p >= self.s.len() {
                return Ok(end_token(value, (p, p)));
            }
            let b = self.s[p];
            if is_other(b) {
                self.at = self.run(p, is_other);
                value.extend_from_slice(&self.s[p..self.at]);
            } else if is_blank(b) {
                self.at = self.run(p, is_blank);
                return Ok(end_token(value, (p, self.at)));
            } else if self.s[p..].starts_with(b"/*") {
                self.at = p + 2;
                self.comment()?;
                return Ok(None);
            } else if b == b'\\' {
                self.escape(&mut value)?;
            } else {
                return Ok(end_token(value, (p, p)));
            }
        }
    }

    /// The states `xq` and `xvq`: the rest of a quoted string or variable.
    fn quoted(&mut self, tok: Tok) -> Result<Token, TypeError> {
        let mut value = Vec::new();
        loop {
            let p = self.at;
            match self.s.get(p) {
                None => return Err(yyerror("unterminated quoted string", b"")),
                Some(b'"') => {
                    self.at = p + 1;
                    return Ok(Token { tok, text: (p, p + 1), value });
                }
                Some(b'\\') => self.escape(&mut value)?,
                Some(_) => {
                    self.at = self.run(p, |b| b != b'\\' && b != b'"');
                    value.extend_from_slice(&self.s[p..self.at]);
                }
            }
        }
    }

    /// The end of a match of `unicode` at `p`.
    fn unicode(&self, p: usize) -> Option<usize> {
        if self.byte(p) != b'\\' || self.byte(p + 1) != b'u' {
            return None;
        }
        if self.byte(p + 2) == b'{' {
            let end = self.run(p + 3, |b| b.is_ascii_hexdigit()).min(p + 9);
            (end > p + 3 && self.byte(end) == b'}').then_some(end + 1)
        } else {
            (p + 2..p + 6).all(|i| self.has(i, |b| b.is_ascii_hexdigit())).then_some(p + 6)
        }
    }

    /// The end of the longest match of `unicodefail` at `p`.
    fn unicode_fail(&self, p: usize) -> Option<usize> {
        if self.byte(p) != b'\\' || self.byte(p + 1) != b'u' {
            return None;
        }
        Some(if self.byte(p + 2) == b'{' {
            self.run(p + 3, |b| b.is_ascii_hexdigit()).min(p + 9)
        } else {
            self.run(p + 2, |b| b.is_ascii_hexdigit()).min(p + 5)
        })
    }

    /// An escape at the backslash at `self.at`, with the rules of `jsonpath_scan.l`.
    fn escape(&mut self, value: &mut Vec<u8>) -> Result<(), TypeError> {
        let p = self.at;
        let mut bounds = vec![p];
        while let Some(end) = self.unicode(bounds[bounds.len() - 1]) {
            bounds.push(end);
        }
        let chain = bounds[bounds.len() - 1];
        let next = self.s.get(p + 1).copied();
        let mut best: Option<(Escape, usize)> = None;
        let mut offer = |rule: Escape, end: Option<usize>| {
            if let Some(end) = end
                && best.is_none_or(|(_, e)| end > e)
            {
                best = Some((rule, end));
            }
        };
        let simple = match next {
            Some(b'b') => Some(0x08),
            Some(b'f') => Some(0x0c),
            Some(b'n') => Some(b'\n'),
            Some(b'r') => Some(b'\r'),
            Some(b't') => Some(b'\t'),
            Some(b'v') => Some(0x0b),
            _ => None,
        };
        if let Some(c) = simple {
            offer(Escape::Char(c), Some(p + 2));
        }
        offer(Escape::Unicode, (chain > p).then_some(chain));
        let hex = next == Some(b'x');
        offer(
            Escape::Hex,
            (hex && (p + 2..p + 4).all(|i| self.has(i, |b| b.is_ascii_hexdigit())))
                .then_some(p + 4),
        );
        offer(Escape::UnicodeFail, bounds.iter().filter_map(|&at| self.unicode_fail(at)).max());
        offer(Escape::HexFail, hex.then(|| self.run(p + 2, |b| b.is_ascii_hexdigit()).min(p + 3)));
        offer(
            Escape::UnicodeBackslash,
            (chain > p && self.byte(chain) == b'\\').then_some(chain + 1),
        );
        offer(Escape::Any, next.filter(|&b| b != b'\n').map(|_| p + 2));
        offer(Escape::Lone, Some(p + 1));
        let (rule, end) = best.unwrap_or((Escape::Lone, p + 1));
        let text = &self.s[p..end];
        match rule {
            Escape::Char(c) => value.push(c),
            Escape::Unicode | Escape::UnicodeBackslash => {
                self.unicode_chain(&bounds, value)?;
                self.at = chain;
                return Ok(());
            }
            Escape::Hex => {
                let code = u32::from_str_radix(std::str::from_utf8(&text[2..]).unwrap_or("0"), 16)
                    .unwrap_or(0);
                self.add_char(code, value)?;
            }
            Escape::UnicodeFail => return Err(yyerror("invalid Unicode escape sequence", text)),
            Escape::HexFail => return Err(yyerror("invalid hexadecimal character sequence", text)),
            Escape::Any => value.push(text[1]),
            Escape::Lone => return Err(yyerror("unexpected end after backslash", text)),
        }
        self.at = end;
        Ok(())
    }

    /// `parseUnicode`: the escapes `\u` between the bounds, with the pairs of surrogates.
    fn unicode_chain(&self, bounds: &[usize], value: &mut Vec<u8>) -> Result<(), TypeError> {
        let mut high: Option<u32> = None;
        let low_must_follow = || bad_input("Unicode low surrogate must follow a high surrogate.");
        for pair in bounds.windows(2) {
            let digits = &self.s[pair[0] + 2..pair[1]];
            let digits = digits.strip_prefix(b"{").map_or(digits, |d| &d[..d.len() - 1]);
            let mut code =
                u32::from_str_radix(std::str::from_utf8(digits).unwrap_or("0"), 16).unwrap_or(0);
            if (0xd800..=0xdbff).contains(&code) {
                if high.is_some() {
                    return Err(bad_input(
                        "Unicode high surrogate must not follow a high surrogate.",
                    ));
                }
                high = Some(code);
                continue;
            }
            if (0xdc00..=0xdfff).contains(&code) {
                let Some(first) = high.take() else { return Err(low_must_follow()) };
                code = ((first & 0x3ff) << 10) + 0x10000 + (code & 0x3ff);
            } else if high.is_some() {
                return Err(low_must_follow());
            }
            self.add_char(code, value)?;
        }
        if high.is_some() {
            return Err(low_must_follow());
        }
        Ok(())
    }

    /// `addUnicodeChar`: one code point in UTF-8.
    fn add_char(&self, code: u32, value: &mut Vec<u8>) -> Result<(), TypeError> {
        if code == 0 {
            let mut error = TypeError::new(
                SqlState::UNTRANSLATABLE_CHARACTER,
                "unsupported Unicode escape sequence".to_string(),
            );
            error.detail = Some("\\u0000 cannot be converted to text.".to_string());
            return Err(error);
        }
        let Some(c) = char::from_u32(code) else {
            let message = if self.soft {
                "could not convert Unicode to server encoding"
            } else {
                "invalid Unicode code point"
            };
            return Err(TypeError::new(SqlState::SYNTAX_ERROR, message.to_string()));
        };
        value.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        Ok(())
    }
}

/// The parser, with the token that it reads ahead.
struct Parser<'a> {
    lexer: Lexer<'a>,
    ahead: Option<Token>,
    depth: usize,
    regex: RegexCheck<'a>,
}

/// The text of a token as a string.
fn text_of(value: Vec<u8>) -> String {
    String::from_utf8(value).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

impl Parser<'_> {
    fn peek(&mut self) -> Result<Tok, TypeError> {
        if self.ahead.is_none() {
            self.ahead = Some(self.lexer.next()?);
        }
        Ok(self.ahead.as_ref().map_or(Tok::End, |t| t.tok))
    }

    fn next(&mut self) -> Result<Token, TypeError> {
        self.peek()?;
        Ok(self.ahead.take().unwrap_or(Token { tok: Tok::End, text: (0, 0), value: Vec::new() }))
    }

    /// The syntax error at the token that the parser reads ahead.
    fn error(&mut self) -> TypeError {
        match &self.ahead {
            Some(token) => yyerror("syntax error", &self.lexer.s[token.text.0..token.text.1]),
            None => yyerror("syntax error", b""),
        }
    }

    fn expect(&mut self, tok: Tok) -> Result<Token, TypeError> {
        if self.peek()? == tok { self.next() } else { Err(self.error()) }
    }

    /// The guard of the depth of the recursion.
    fn deeper<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, TypeError>,
    ) -> Result<T, TypeError> {
        if self.depth >= MAX_DEPTH {
            return Err(too_deep());
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// `expr_or_predicate`, or `predicate` when `pred` is true. The flag of the result is true for a predicate.
    fn or(&mut self, pred: bool) -> Result<(Chain, bool), TypeError> {
        let (mut left, mut is_pred) = self.and(pred)?;
        while self.peek()? == Tok::Or {
            if !is_pred {
                return Err(self.error());
            }
            self.next()?;
            let right = self.and(true)?.0;
            left = vec![PathItem::Binary(PathOp::Or, left, right)];
            is_pred = true;
        }
        Ok((left, is_pred))
    }

    fn and(&mut self, pred: bool) -> Result<(Chain, bool), TypeError> {
        let (mut left, mut is_pred) = self.not(pred)?;
        while self.peek()? == Tok::And {
            if !is_pred {
                return Err(self.error());
            }
            self.next()?;
            let right = self.not(true)?.0;
            left = vec![PathItem::Binary(PathOp::And, left, right)];
            is_pred = true;
        }
        Ok((left, is_pred))
    }

    fn not(&mut self, pred: bool) -> Result<(Chain, bool), TypeError> {
        if self.peek()? != Tok::Not {
            return self.comparison(pred);
        }
        self.next()?;
        self.deeper(|p| {
            let arg = match p.peek()? {
                Tok::Char(b'(') => {
                    p.next()?;
                    let arg = p.or(true)?.0;
                    p.expect(Tok::Char(b')'))?;
                    arg
                }
                Tok::Kw(Kw::Exists) => p.exists()?,
                _ => return Err(p.error()),
            };
            Ok((vec![PathItem::Unary(PathOp::Not, arg)], true))
        })
    }

    /// `EXISTS '(' expr ')'`.
    fn exists(&mut self) -> Result<Chain, TypeError> {
        self.next()?;
        self.expect(Tok::Char(b'('))?;
        let arg = self.expr()?;
        self.expect(Tok::Char(b')'))?;
        Ok(vec![PathItem::Unary(PathOp::Exists, arg)])
    }

    /// An `expr`, where a predicate cannot be.
    fn expr(&mut self) -> Result<Chain, TypeError> {
        Ok(self.additive(true)?.0)
    }

    fn comparison(&mut self, pred: bool) -> Result<(Chain, bool), TypeError> {
        if self.peek()? == Tok::Kw(Kw::Exists) {
            return Ok((self.exists()?, true));
        }
        let (left, is_pred) = self.additive(false)?;
        if is_pred {
            return Ok((left, true));
        }
        let op = match self.peek()? {
            Tok::Equal => PathOp::Equal,
            Tok::NotEqual => PathOp::NotEqual,
            Tok::Less => PathOp::Less,
            Tok::Greater => PathOp::Greater,
            Tok::LessEqual => PathOp::LessOrEqual,
            Tok::GreaterEqual => PathOp::GreaterOrEqual,
            Tok::Kw(Kw::Starts) => {
                self.next()?;
                self.expect(Tok::Kw(Kw::With))?;
                let item = match self.peek()? {
                    Tok::Str => PathItem::String(text_of(self.next()?.value)),
                    Tok::Variable => PathItem::Variable(text_of(self.next()?.value)),
                    _ => return Err(self.error()),
                };
                return Ok((vec![PathItem::Binary(PathOp::StartsWith, left, vec![item])], true));
            }
            Tok::Kw(Kw::LikeRegex) => {
                self.next()?;
                let pattern = text_of(self.expect(Tok::Str)?.value);
                let flags = if self.peek()? == Tok::Kw(Kw::Flag) {
                    self.next()?;
                    Some(text_of(self.expect(Tok::Str)?.value))
                } else {
                    None
                };
                return Ok((vec![self.like_regex(left, pattern, flags.as_deref())?], true));
            }
            _ if pred => return Err(self.error()),
            _ => return Ok((left, false)),
        };
        self.next()?;
        let right = self.expr()?;
        Ok((vec![PathItem::Binary(op, left, right)], true))
    }

    /// `makeItemLikeRegex`: the flags and the check of the pattern.
    fn like_regex(
        &self,
        expr: Chain,
        pattern: String,
        flags: Option<&str>,
    ) -> Result<PathItem, TypeError> {
        let mut bits = 0;
        for c in flags.unwrap_or_default().chars() {
            bits |= match c {
                'i' => JSONPATH_REGEX_ICASE,
                's' => JSONPATH_REGEX_DOTALL,
                'm' => JSONPATH_REGEX_MLINE,
                'x' => JSONPATH_REGEX_WSPACE,
                'q' => JSONPATH_REGEX_QUOTE,
                c => {
                    let mut error = bad_input("");
                    error.sqlstate = SqlState::SYNTAX_ERROR;
                    error.detail = Some(format!(
                        "Unrecognized flag character \"{c}\" in LIKE_REGEX predicate."
                    ));
                    return Err(error);
                }
            };
        }
        let cflags = jsonpath_regex_flags(bits)?;
        (self.regex)(&pattern, cflags).map_err(|message| {
            TypeError::new(
                SqlState::INVALID_REGULAR_EXPRESSION,
                format!("invalid regular expression: {message}"),
            )
        })?;
        Ok(PathItem::LikeRegex { expr, pattern, flags: bits })
    }

    fn additive(&mut self, expr: bool) -> Result<(Chain, bool), TypeError> {
        let (mut left, is_pred) = self.multiplicative(expr)?;
        if is_pred {
            return Ok((left, true));
        }
        loop {
            let op = match self.peek()? {
                Tok::Char(b'+') => PathOp::Add,
                Tok::Char(b'-') => PathOp::Sub,
                _ => return Ok((left, false)),
            };
            self.next()?;
            let right = self.multiplicative(true)?.0;
            left = vec![PathItem::Binary(op, left, right)];
        }
    }

    fn multiplicative(&mut self, expr: bool) -> Result<(Chain, bool), TypeError> {
        let (mut left, is_pred) = self.unary(expr)?;
        if is_pred {
            return Ok((left, true));
        }
        loop {
            let op = match self.peek()? {
                Tok::Char(b'*') => PathOp::Mul,
                Tok::Char(b'/') => PathOp::Div,
                Tok::Char(b'%') => PathOp::Mod,
                _ => return Ok((left, false)),
            };
            self.next()?;
            let right = self.unary(true)?.0;
            left = vec![PathItem::Binary(op, left, right)];
        }
    }

    /// Unary `+` and `-`, which `makeItemUnary` folds into a number.
    fn unary(&mut self, expr: bool) -> Result<(Chain, bool), TypeError> {
        self.deeper(|p| {
            let op = match p.peek()? {
                Tok::Char(b'+') => PathOp::Plus,
                Tok::Char(b'-') => PathOp::Minus,
                _ => return p.accessor_expr(expr),
            };
            p.next()?;
            let arg = p.unary(true)?.0;
            Ok((unary(op, arg), false))
        })
    }

    /// `accessor_expr`, or a predicate in parentheses with no accessor. When `expr` is true, a predicate in parentheses must have an accessor.
    fn accessor_expr(&mut self, expr: bool) -> Result<(Chain, bool), TypeError> {
        let item = match self.peek()? {
            Tok::Char(b'(') => {
                self.next()?;
                let (mut inner, is_pred) = self.or(false)?;
                self.expect(Tok::Char(b')'))?;
                if is_pred && !matches!(self.peek()?, Tok::Char(b'.' | b'[' | b'?')) {
                    if expr {
                        return Err(self.error());
                    }
                    if self.peek()? == Tok::Kw(Kw::Is) {
                        self.next()?;
                        self.expect(Tok::Kw(Kw::Unknown))?;
                        return Ok((vec![PathItem::Unary(PathOp::IsUnknown, inner)], true));
                    }
                    return Ok((inner, true));
                }
                self.accessors(&mut inner)?;
                return Ok((inner, false));
            }
            Tok::Str => PathItem::String(text_of(self.next()?.value)),
            Tok::Kw(Kw::Null) => {
                self.next()?;
                PathItem::Null
            }
            Tok::Kw(Kw::True | Kw::False) => PathItem::Bool(self.next()?.tok == Tok::Kw(Kw::True)),
            Tok::Numeric | Tok::Int => PathItem::Numeric(number(self.next()?)?),
            Tok::Variable => PathItem::Variable(text_of(self.next()?.value)),
            Tok::Char(b'$') => {
                self.next()?;
                PathItem::Root
            }
            Tok::Char(b'@') => {
                self.next()?;
                PathItem::Current
            }
            Tok::Kw(Kw::Last) => {
                self.next()?;
                PathItem::Last
            }
            _ => return Err(self.error()),
        };
        let mut chain = vec![item];
        self.accessors(&mut chain)?;
        Ok((chain, false))
    }

    /// The `accessor_op` items after an item.
    fn accessors(&mut self, chain: &mut Chain) -> Result<(), TypeError> {
        loop {
            match self.peek()? {
                Tok::Char(b'.') => {
                    self.next()?;
                    let item = self.after_dot()?;
                    chain.push(item);
                }
                Tok::Char(b'[') => {
                    self.next()?;
                    if self.peek()? == Tok::Char(b'*') {
                        self.next()?;
                        self.expect(Tok::Char(b']'))?;
                        chain.push(PathItem::AnyArray);
                        continue;
                    }
                    let mut elems = Vec::new();
                    loop {
                        let from = self.expr()?;
                        let to = if self.peek()? == Tok::Kw(Kw::To) {
                            self.next()?;
                            Some(self.expr()?)
                        } else {
                            None
                        };
                        elems.push((from, to));
                        if self.peek()? != Tok::Char(b',') {
                            break;
                        }
                        self.next()?;
                    }
                    self.expect(Tok::Char(b']'))?;
                    chain.push(PathItem::IndexArray(elems));
                }
                Tok::Char(b'?') => {
                    self.next()?;
                    self.expect(Tok::Char(b'('))?;
                    let arg = self.deeper(|p| p.or(true))?.0;
                    self.expect(Tok::Char(b')'))?;
                    chain.push(PathItem::Unary(PathOp::Filter, arg));
                }
                _ => return Ok(()),
            }
        }
    }

    /// The accessor after `.`: a key, `*`, `**` or a method.
    fn after_dot(&mut self) -> Result<PathItem, TypeError> {
        match self.peek()? {
            Tok::Char(b'*') => {
                self.next()?;
                Ok(PathItem::AnyKey)
            }
            Tok::AnyLevels => {
                self.next()?;
                if self.peek()? != Tok::Char(b'{') {
                    return Ok(PathItem::Any { first: 0, last: JSONPATH_LAST });
                }
                self.next()?;
                let first = self.level()?;
                let last = if self.peek()? == Tok::Kw(Kw::To) {
                    self.next()?;
                    self.level()?
                } else {
                    first
                };
                self.expect(Tok::Char(b'}'))?;
                Ok(PathItem::Any { first, last })
            }
            Tok::Ident | Tok::Str => Ok(PathItem::Key(text_of(self.next()?.value))),
            Tok::Kw(kw) => {
                let token = self.next()?;
                match kw {
                    Kw::Method(op) if self.peek()? == Tok::Char(b'(') => {
                        self.next()?;
                        self.method(op)
                    }
                    _ => Ok(PathItem::Key(text_of(token.value))),
                }
            }
            _ => Err(self.error()),
        }
    }

    /// `any_level`: a number or `last`. The number is an `int4`.
    fn level(&mut self) -> Result<u32, TypeError> {
        match self.peek()? {
            Tok::Int => {
                let value = text_of(self.next()?.value);
                Ok(u32::try_from(int4_in(&value)?).unwrap_or(0))
            }
            Tok::Kw(Kw::Last) => {
                self.next()?;
                Ok(JSONPATH_LAST)
            }
            _ => Err(self.error()),
        }
    }

    /// The arguments of a method after `(`, and the `)`.
    fn method(&mut self, op: PathOp) -> Result<PathItem, TypeError> {
        let mut args = Vec::new();
        match op {
            PathOp::Decimal => {
                if self.peek()? != Tok::Char(b')') {
                    args.push(self.int_elem()?);
                    while self.peek()? == Tok::Char(b',') {
                        self.next()?;
                        args.push(self.int_elem()?);
                    }
                }
                self.expect(Tok::Char(b')'))?;
                if args.len() > 2 {
                    let mut error =
                        bad_input(".decimal() can only have an optional precision[,scale].");
                    error.sqlstate = SqlState::SYNTAX_ERROR;
                    return Err(error);
                }
                return Ok(PathItem::Method(op, args));
            }
            PathOp::Datetime | PathOp::Ltrim | PathOp::Rtrim | PathOp::Btrim => {
                if self.peek()? == Tok::Str {
                    args.push(PathItem::String(text_of(self.next()?.value)));
                }
            }
            PathOp::Time | PathOp::TimeTz | PathOp::Timestamp | PathOp::TimestampTz => {
                if self.peek()? == Tok::Int {
                    args.push(PathItem::Numeric(number(self.next()?)?));
                }
            }
            PathOp::Replace | PathOp::SplitPart => {
                args.push(PathItem::String(text_of(self.expect(Tok::Str)?.value)));
                self.expect(Tok::Char(b','))?;
                args.push(if op == PathOp::Replace {
                    PathItem::String(text_of(self.expect(Tok::Str)?.value))
                } else {
                    self.int_elem()?
                });
            }
            _ => {}
        }
        self.expect(Tok::Char(b')'))?;
        Ok(PathItem::Method(op, args))
    }

    /// `int_elem`: an integer with an optional sign.
    fn int_elem(&mut self) -> Result<PathItem, TypeError> {
        let negate = match self.peek()? {
            Tok::Char(b'+') => false,
            Tok::Char(b'-') => true,
            Tok::Int => return Ok(PathItem::Numeric(number(self.next()?)?)),
            _ => return Err(self.error()),
        };
        self.next()?;
        let value = number(self.expect(Tok::Int)?)?;
        Ok(PathItem::Numeric(if negate { value.negate() } else { value }))
    }
}

/// `makeItemNumeric`: the number of a token. An error of `numeric_in` is a hard error.
fn number(token: Token) -> Result<Numeric, TypeError> {
    numeric_in(&text_of(token.value), -1)
}

/// `makeItemUnary` for `+` and `-`: a number with no accessor takes the sign.
fn unary(op: PathOp, arg: Chain) -> Chain {
    if let [PathItem::Numeric(value)] = arg.as_slice() {
        if op == PathOp::Minus {
            return vec![PathItem::Numeric(value.negate())];
        }
        return arg;
    }
    vec![PathItem::Unary(op, arg)]
}

/// The checks of `flattenJsonPathParseItem`: `@` must be in a filter and `last` must be in a subscript. The order of the checks is the order of PostgreSQL: an item, then its operands, then the next item.
fn check(chain: &[PathItem], nesting: u32, subscript: bool) -> Result<(), TypeError> {
    for item in chain {
        match item {
            PathItem::Current if nesting == 0 => {
                return Err(TypeError::new(
                    SqlState::SYNTAX_ERROR,
                    "@ is not allowed in root expressions".to_string(),
                ));
            }
            PathItem::Last if !subscript => {
                return Err(TypeError::new(
                    SqlState::SYNTAX_ERROR,
                    "LAST is allowed only in array subscripts".to_string(),
                ));
            }
            PathItem::Binary(_, left, right) => {
                check(left, nesting, subscript)?;
                check(right, nesting, subscript)?;
            }
            PathItem::Unary(op, arg) => {
                check(arg, nesting + u32::from(*op == PathOp::Filter), subscript)?
            }
            PathItem::LikeRegex { expr, .. } => check(expr, nesting, subscript)?,
            PathItem::IndexArray(elems) => {
                for (from, to) in elems {
                    check(from, nesting, true)?;
                    if let Some(to) = to {
                        check(to, nesting, true)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `jsonpath_in`: a path from its text. `soft` is true for the soft input of `pg_input_is_valid`, and `regex` checks the pattern of `like_regex`.
///
/// # Errors
///
/// `42601` and `22P02` for text that is not a path, `22P05` for `\u0000`, `0A000` for the flag `x`, `2201B` for a pattern that is not valid, `22003` for a number that is too large and `54001` for a path that is too deep. The last two are hard errors in PostgreSQL.
pub fn jsonpath_parse(
    text: &str,
    soft: bool,
    regex: RegexCheck<'_>,
) -> Result<JsonPath, TypeError> {
    let mut parser =
        Parser { lexer: Lexer { s: text.as_bytes(), at: 0, soft }, ahead: None, depth: 0, regex };
    let lax = match parser.peek()? {
        Tok::End => return Err(TypeError::syntax("jsonpath", text)),
        Tok::Kw(Kw::Strict) => {
            parser.next()?;
            false
        }
        Tok::Kw(Kw::Lax) => {
            parser.next()?;
            true
        }
        _ => true,
    };
    let expr = parser.or(false)?.0;
    if parser.peek()? != Tok::End {
        return Err(parser.error());
    }
    check(&expr, 0, false)?;
    Ok(JsonPath { lax, expr })
}

/// `printJsonPathItem` for a chain: the first item, then the items that follow it.
fn print_chain(chain: &[PathItem], brackets: bool, out: &mut String) {
    for (i, item) in chain.iter().enumerate() {
        print_item(item, i > 0, i > 0 || brackets, i + 1 < chain.len(), out);
    }
}

/// An operand, in parentheses when its priority is not higher than `priority`.
fn print_operand(chain: &[PathItem], priority: u8, out: &mut String) {
    print_chain(chain, chain[0].priority() <= priority, out);
}

/// `printJsonPathItem` for one item.
fn print_item(item: &PathItem, in_key: bool, brackets: bool, has_next: bool, out: &mut String) {
    let open = |out: &mut String| {
        if brackets {
            out.push('(');
        }
    };
    let close = |out: &mut String| {
        if brackets {
            out.push(')');
        }
    };
    let dot = |out: &mut String| {
        if in_key {
            out.push('.');
        }
    };
    match item {
        PathItem::Null => out.push_str("null"),
        PathItem::String(s) => escape(s, out),
        PathItem::Numeric(value) => {
            let mut bytes = Vec::new();
            numeric_out(value, &mut bytes);
            let text = String::from_utf8_lossy(&bytes);
            if has_next {
                out.push('(');
                out.push_str(&text);
                out.push(')');
            } else {
                out.push_str(&text);
            }
        }
        PathItem::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PathItem::Binary(op, left, right) => {
            open(out);
            print_operand(left, op.priority(), out);
            out.push(' ');
            out.push_str(op.name());
            out.push(' ');
            print_operand(right, op.priority(), out);
            close(out);
        }
        PathItem::Unary(op @ (PathOp::Plus | PathOp::Minus), arg) => {
            open(out);
            out.push_str(op.name());
            print_operand(arg, op.priority(), out);
            close(out);
        }
        PathItem::Unary(op, arg) => {
            let (before, after) = match op {
                PathOp::Not => ("!(", ")"),
                PathOp::IsUnknown => ("(", ") is unknown"),
                PathOp::Filter => ("?(", ")"),
                _ => ("exists (", ")"),
            };
            out.push_str(before);
            print_chain(arg, false, out);
            out.push_str(after);
        }
        PathItem::LikeRegex { expr, pattern, flags } => {
            open(out);
            print_operand(expr, 6, out);
            out.push_str(" like_regex ");
            escape(pattern, out);
            if *flags != 0 {
                out.push_str(" flag \"");
                for (bit, c) in [
                    (JSONPATH_REGEX_ICASE, 'i'),
                    (JSONPATH_REGEX_DOTALL, 's'),
                    (JSONPATH_REGEX_MLINE, 'm'),
                    (JSONPATH_REGEX_WSPACE, 'x'),
                    (JSONPATH_REGEX_QUOTE, 'q'),
                ] {
                    if flags & bit != 0 {
                        out.push(c);
                    }
                }
                out.push('"');
            }
            close(out);
        }
        PathItem::AnyArray => out.push_str("[*]"),
        PathItem::AnyKey => {
            dot(out);
            out.push('*');
        }
        PathItem::IndexArray(elems) => {
            out.push('[');
            for (i, (from, to)) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                print_chain(from, false, out);
                if let Some(to) = to {
                    out.push_str(" to ");
                    print_chain(to, false, out);
                }
            }
            out.push(']');
        }
        PathItem::Any { first, last } => {
            dot(out);
            let bound =
                |n: u32| if n == JSONPATH_LAST { "last".to_string() } else { n.to_string() };
            if *first == 0 && *last == JSONPATH_LAST {
                out.push_str("**");
            } else if first == last {
                out.push_str(&format!("**{{{}}}", bound(*first)));
            } else {
                out.push_str(&format!("**{{{} to {}}}", bound(*first), bound(*last)));
            }
        }
        PathItem::Key(key) => {
            dot(out);
            escape(key, out);
        }
        PathItem::Current => out.push('@'),
        PathItem::Root => out.push('$'),
        PathItem::Variable(name) => {
            out.push('$');
            escape(name, out);
        }
        PathItem::Last => out.push_str("last"),
        PathItem::Method(op, args) => {
            out.push('.');
            out.push_str(op.name());
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                print_item(arg, false, false, false, out);
            }
            out.push(')');
        }
    }
}

/// `jsonpath_out`: the text form of a path.
pub fn jsonpath_deparse(path: &JsonPath, out: &mut Vec<u8>) {
    let mut text = String::new();
    if !path.lax {
        text.push_str("strict ");
    }
    print_chain(&path.expr, true, &mut text);
    out.extend_from_slice(text.as_bytes());
}

/// `jsonpath_send`: the version 1 and the text form.
pub fn jsonpath_send(path: &JsonPath, out: &mut Vec<u8>) {
    out.push(1);
    jsonpath_deparse(path, out);
}

/// `jsonpath_recv`: a path from the version 1 and the text form. Each error is a hard error.
///
/// # Errors
///
/// `XX000` for another version, and the errors of [`jsonpath_parse`].
pub fn jsonpath_recv(recv: &mut Recv<'_>, regex: RegexCheck<'_>) -> Result<JsonPath, TypeError> {
    let version = recv.byte()?;
    if version != 1 {
        return Err(TypeError::new(
            SqlState::INTERNAL_ERROR,
            format!("unsupported jsonpath version number: {version}"),
        ));
    }
    jsonpath_parse(recv.text()?, false, regex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_regex(_: &str, _: u32) -> Result<(), String> {
        Ok(())
    }

    fn paren_regex(pattern: &str, _: u32) -> Result<(), String> {
        if pattern.contains('(') { Err("parentheses () not balanced".to_string()) } else { Ok(()) }
    }

    fn out(text: &str) -> String {
        let path =
            jsonpath_parse(text, false, &any_regex).unwrap_or_else(|e| panic!("{text}: {e}"));
        let mut bytes = Vec::new();
        jsonpath_deparse(&path, &mut bytes);
        String::from_utf8(bytes).expect("utf8")
    }

    fn err(text: &str) -> String {
        match jsonpath_parse(text, false, &paren_regex) {
            Ok(_) => panic!("{text} is valid"),
            Err(e) => format!("{} {}", e.sqlstate.as_str(), e.message),
        }
    }

    #[test]
    fn text_form() {
        let cases = [
            ("1 + 2", "(1 + 2)"),
            ("1 + 2 + 3", "((1 + 2) + 3)"),
            ("1 * 2 + 3", "(1 * 2 + 3)"),
            ("- -1", "1"),
            ("-$.a", "(-$.\"a\")"),
            ("-(1).a", "(-(1).\"a\")"),
            ("-$.a * 2", "(-$.\"a\" * 2)"),
            ("1 / 2 % 3", "((1 / 2) % 3)"),
            ("$ ? (@ > 1)", "$?(@ > 1)"),
            ("$ ? ((@ > 1) is unknown)", "$?((@ > 1) is unknown)"),
            ("$ ? (exists (@.a))", "$?(exists (@.\"a\"))"),
            ("$ ? (!exists(@.a))", "$?(!(exists (@.\"a\")))"),
            ("$.a[1,2 to 3,last]", "$.\"a\"[1,2 to 3,last]"),
            ("$.**{last to 2}", "$.**{last to 2}"),
            ("$.**{0 to last}", "$.**"),
            ("$.**{last to last}", "$.**{last}"),
            ("$.decimal(4,-2)", "$.decimal(4,-2)"),
            ("$.Type()", "$.type()"),
            ("$.TYPE", "$.\"TYPE\""),
            ("STRICT $", "strict $"),
            ("lax 1", "1"),
            (".5", "0.5"),
            ("1e3", "1000"),
            ("0x1F", "31"),
            ("1_000.5_5", "1000.55"),
            ("$var.a", "$\"var\".\"a\""),
            ("(1 + $).a", "(1 + $).\"a\""),
            ("(($ > 1)).a", "($ > 1).\"a\""),
            ("\"\\v\\x41\\u{1F600}\"", "\"\\u000bA\u{1F600}\""),
            ("\"\\ud83d\\ude00\"", "\"\u{1F600}\""),
            ("$.\\x41", "$.\"A\""),
            ("$ /* c */ .a", "$.\"a\""),
            ("$ ? (@ like_regex \"a\" flag \"qx\")", "$?(@ like_regex \"a\" flag \"xq\")"),
            ("$ ? (-@ like_regex \"a\")", "$?((-@) like_regex \"a\")"),
            ("$[last ? (@ > last)]", "$[last?(@ > last)]"),
            ("$.split_part(\"\", +1)", "$.split_part(\"\",1)"),
            ("$.a.type ( )", "$.\"a\".type()"),
        ];
        for (text, want) in cases {
            assert_eq!(out(text), want, "{text}");
        }
    }

    #[test]
    fn errors() {
        let cases = [
            ("", "22P02 invalid input syntax for type jsonpath: \"\""),
            ("/* c */", "22P02 invalid input syntax for type jsonpath: \"/* c */\""),
            ("$ abc def", "42601 syntax error at or near \" \" of jsonpath input"),
            ("$.a/*c*/", "42601 syntax error at end of jsonpath input"),
            ("$.a/*c*/.b", "42601 syntax error at or near \".\" of jsonpath input"),
            ("$.1", "42601 syntax error at or near \".1\" of jsonpath input"),
            ("1a", "42601 trailing junk after numeric literal at or near \"1a\" of jsonpath input"),
            ("00", "42601 trailing junk after numeric literal at or near \"00\" of jsonpath input"),
            ("1e5xy", "42601 syntax error at end of jsonpath input"),
            (
                "1.5e3abc",
                "42601 trailing junk after numeric literal at or near \"1.5e3a\" of jsonpath input",
            ),
            ("1e+", "42601 invalid numeric literal at or near \"1e+\" of jsonpath input"),
            ("\"a", "42601 unterminated quoted string at end of jsonpath input"),
            ("$ /* a", "42601 unexpected end of comment at end of jsonpath input"),
            (
                "\"\\u12\"",
                "42601 invalid Unicode escape sequence at or near \"\\u12\" of jsonpath input",
            ),
            (
                "\"\\u{1234567}\"",
                "42601 invalid Unicode escape sequence at or near \"\\u{123456\" of jsonpath input",
            ),
            (
                "\"\\xZ\"",
                "42601 invalid hexadecimal character sequence at or near \"\\x\" of jsonpath input",
            ),
            ("\"\\u0000\"", "22P05 unsupported Unicode escape sequence"),
            ("\"\\ud83d\"", "22P02 invalid input syntax for type jsonpath"),
            ("\"\\u{110000}\"", "42601 invalid Unicode code point"),
            ("@", "42601 @ is not allowed in root expressions"),
            ("$.a[@]", "42601 @ is not allowed in root expressions"),
            ("last + @", "42601 LAST is allowed only in array subscripts"),
            ("$ > 1 > 2", "42601 syntax error at or near \">\" of jsonpath input"),
            ("($ > 1) + 2", "42601 syntax error at or near \"+\" of jsonpath input"),
            ("!($.a)", "42601 syntax error at or near \")\" of jsonpath input"),
            ("!($ > 1) is unknown", "42601 syntax error at or near \" \" of jsonpath input"),
            ("exists($ > 1)", "42601 syntax error at or near \">\" of jsonpath input"),
            ("exists(($ > 1))", "42601 syntax error at or near \")\" of jsonpath input"),
            ("-($ > 1)", "42601 syntax error at end of jsonpath input"),
            ("$ ? (true)", "42601 syntax error at or near \")\" of jsonpath input"),
            ("$ ? (@ == TRUE)", "42601 syntax error at end of jsonpath input"),
            ("$.time(\"x\")", "42601 syntax error at or near \"\"\" of jsonpath input"),
            ("$[1, *]", "42601 syntax error at or near \"*\" of jsonpath input"),
            ("$.decimal(1, 2, 3) junk", "42601 invalid input syntax for type jsonpath"),
            ("$ like_regex \"(a\" flag \"z\"", "42601 invalid input syntax for type jsonpath"),
            (
                "$ like_regex \"(a\" junk",
                "2201B invalid regular expression: parentheses () not balanced",
            ),
            (
                "$ like_regex \"a\" flag \"x\"",
                "0A000 XQuery \"x\" flag (expanded regular expressions) is not implemented",
            ),
            ("$.**{99999999999}", "22003 value \"99999999999\" is out of range for type integer"),
            ("lax strict $", "42601 syntax error at or near \" \" of jsonpath input"),
            ("strict", "42601 syntax error at end of jsonpath input"),
        ];
        for (text, want) in cases {
            assert_eq!(err(text), want, "{text}");
        }
        let soft = jsonpath_parse("\"\\u{110000}\"", true, &any_regex).expect_err("soft");
        assert_eq!(soft.message, "could not convert Unicode to server encoding");
    }

    #[test]
    fn binary_form() {
        let path = jsonpath_parse("strict $.a ? (@ > 1)", false, &any_regex).expect("path");
        let mut bytes = Vec::new();
        jsonpath_send(&path, &mut bytes);
        assert_eq!(bytes, b"\x01strict $.\"a\"?(@ > 1)");
        assert_eq!(jsonpath_recv(&mut Recv::new(&bytes), &any_regex).expect("recv"), path);
        let error = jsonpath_recv(&mut Recv::new(b"\x02$"), &any_regex).expect_err("version");
        assert_eq!(error.message, "unsupported jsonpath version number: 2");
    }
}

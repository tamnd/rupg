//! The regular expressions of PostgreSQL, the engine of Henry Spencer in `src/backend/regex`, for the C locale.
//!
//! The lexer and the parser are ports of `regc_lex.c` and `regcomp.c`, so that a pattern gives the same error as in PostgreSQL. The matcher is not a port: it is a backtracking machine that tells if a match is in the string and where the leftmost match starts. The positions of the match and of its groups follow the rules of preference of Spencer: the tree of subexpressions of `regcomp.c` and the dissection of `regexec.c` find them.
//!
//! The functions here are the operators `~`, `~*`, `!~` and `!~*`, `substring` with a pattern, `similar_to_escape`, `regexp_replace`, `regexp_match`, `regexp_matches`, `regexp_like`, `regexp_count`, `regexp_instr`, `regexp_substr` and the functions that split a string at the matches. [`auth_compile`] and [`auth_search`] are the regular expressions of `pg_hba.conf` and `pg_ident.conf`.

mod exec;
mod lex;
mod parse;
mod tree;

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Value};

use crate::{Call, Kernel, bad_value};

/// The bits of `cflags`, as in `regex.h`.
pub(super) mod flags {
    pub(crate) const EXTENDED: u32 = 0o1;
    pub(crate) const ADVF: u32 = 0o2;
    pub(crate) const ADVANCED: u32 = 0o3;
    pub(crate) const QUOTE: u32 = 0o4;
    pub(crate) const ICASE: u32 = 0o10;
    pub(crate) const EXPANDED: u32 = 0o40;
    pub(crate) const NLSTOP: u32 = 0o100;
    pub(crate) const NLANCH: u32 = 0o200;
    pub(crate) const NEWLINE: u32 = 0o300;
}

/// The error codes of `regex.h` that the compiler can give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Code {
    BadPat,
    Collate,
    Ctype,
    Escape,
    Subreg,
    Brack,
    Paren,
    Brace,
    BadBr,
    Range,
    BadRpt,
    Assert,
    BadOpt,
    TooBig,
}

impl Code {
    /// The message of `regerrs.h`.
    fn message(self) -> &'static str {
        match self {
            Code::BadPat => "invalid regexp (reg version 0.8)",
            Code::Collate => "invalid collating element",
            Code::Ctype => "invalid character class",
            Code::Escape => "invalid escape \\ sequence",
            Code::Subreg => "invalid backreference number",
            Code::Brack => "brackets [] not balanced",
            Code::Paren => "parentheses () not balanced",
            Code::Brace => "braces {} not balanced",
            Code::BadBr => "invalid repetition count(s)",
            Code::Range => "invalid character range",
            Code::BadRpt => "quantifier operand invalid",
            Code::Assert => "\"cannot happen\" -- you found a bug",
            Code::BadOpt => "invalid embedded option",
            Code::TooBig => "regular expression is too complex",
        }
    }

    /// The error of `RE_compile_and_cache`.
    fn error(self) -> Error {
        Error::new(
            SqlState::INVALID_REGULAR_EXPRESSION,
            format!("invalid regular expression: {}", self.message()),
        )
    }
}

/// A compiled pattern.
pub(crate) struct Regex {
    parsed: parse::Parsed,
    prog: exec::Program,
    /// The tree of subexpressions, which the first search for the positions of a match builds.
    tree: OnceCell<std::result::Result<tree::Tree, Code>>,
}

impl Regex {
    /// `pg_regcomp` for a pattern and its flags.
    fn compile(pattern: &str, cflags: u32) -> std::result::Result<Self, Code> {
        let chars: Vec<u32> = pattern.chars().map(u32::from).collect();
        let parsed = parse::Parser::run(&chars, cflags)?;
        let prog = exec::Program::new(&parsed.node, parsed.groups, parsed.cflags)?;
        Ok(Regex { parsed, prog, tree: OnceCell::new() })
    }

    /// True when the pattern matches a part of `s`.
    pub(crate) fn is_match(&self, s: &str) -> bool {
        let chars: Vec<u32> = s.chars().map(u32::from).collect();
        self.prog.first(&chars, 0).is_some()
    }

    /// `re_nsub`: the number of the groups that capture.
    pub(crate) fn groups(&self) -> usize {
        self.parsed.groups
    }

    /// `pg_regexec`: the positions in characters of the leftmost match in `s`, first the whole match and then each group, or `None` when the pattern does not match.
    ///
    /// # Errors
    ///
    /// `2201B` for a pattern that is too complex.
    pub(crate) fn find(&self, s: &str) -> Result<Option<tree::Groups>> {
        let chars: Vec<u32> = s.chars().map(u32::from).collect();
        self.find_chars(&chars, 0)
    }

    /// [`Regex::find`] in the characters `chars` from the character `from`. The constraints see all the characters, also the characters before `from`.
    ///
    /// # Errors
    ///
    /// `2201B` for a pattern that is too complex.
    pub(crate) fn find_chars(&self, chars: &[u32], from: usize) -> Result<Option<tree::Groups>> {
        self.search(chars, from).map_err(Code::error)
    }

    /// [`Regex::find_chars`] with the code of `regerror` for the error.
    fn search(
        &self,
        chars: &[u32],
        from: usize,
    ) -> std::result::Result<Option<tree::Groups>, Code> {
        let tree = self.tree.get_or_init(|| tree::Tree::new(&self.parsed));
        let tree = tree.as_ref().map_err(|code| *code)?;
        let mut from = from;
        while let Some(begin) = self.prog.first(chars, from) {
            if let Some(groups) = tree.find(chars, begin) {
                return Ok(Some(groups));
            }
            from = begin + 1;
        }
        Ok(None)
    }
}

/// The number of patterns that `RE_compile_and_cache` keeps, `MAX_CACHED_RES`.
const MAX_CACHED: usize = 32;

/// A compiled pattern with its source and its flags.
type Cached = (String, u32, Rc<Regex>);

thread_local! {
    /// The patterns that the session compiled last, the newest first.
    static CACHE: RefCell<Vec<Cached>> = const { RefCell::new(Vec::new()) };
}

/// `RE_compile_and_cache`: the compiled pattern, from the cache when it is there.
///
/// # Errors
///
/// `2201B` for a pattern that is not valid.
pub(crate) fn compile(pattern: &str, cflags: u32) -> Result<Rc<Regex>> {
    if let Some(found) = CACHE.with_borrow_mut(|cache| {
        let at = cache.iter().position(|(p, f, _)| *f == cflags && p == pattern)?;
        let entry = cache.remove(at);
        let regex = Rc::clone(&entry.2);
        cache.insert(0, entry);
        Some(regex)
    }) {
        return Ok(found);
    }
    let regex = Rc::new(Regex::compile(pattern, cflags).map_err(Code::error)?);
    CACHE.with_borrow_mut(|cache| {
        cache.truncate(MAX_CACHED - 1);
        cache.insert(0, (pattern.to_owned(), cflags, Rc::clone(&regex)));
    });
    Ok(regex)
}

/// A match of [`auth_search`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthMatch {
    /// The text of the first group, or `None` when the pattern has no group or the first group did not match.
    pub group: Option<String>,
}

/// `regcomp_auth_token`: compile a regular expression of `pg_hba.conf` or `pg_ident.conf` with the flags `REG_ADVANCED`.
///
/// # Errors
///
/// The message of `pg_regerror` for a pattern that is not valid.
pub fn auth_compile(pattern: &str) -> std::result::Result<(), &'static str> {
    Regex::compile(pattern, flags::ADVANCED).map(drop).map_err(Code::message)
}

/// `regexec_auth_token`: find the regular expression `pattern` of [`auth_compile`] in `s`. The result is `None` when the pattern does not match.
///
/// # Errors
///
/// The message of `pg_regerror` for a pattern that is not valid or that is too complex.
pub fn auth_search(pattern: &str, s: &str) -> std::result::Result<Option<AuthMatch>, &'static str> {
    let regex = Regex::compile(pattern, flags::ADVANCED).map_err(Code::message)?;
    let points: Vec<u32> = s.chars().map(u32::from).collect();
    let Some(groups) = regex.search(&points, 0).map_err(Code::message)? else { return Ok(None) };
    let group = groups.get(1).copied().flatten().map(|span| chars(s, span));
    Ok(Some(AuthMatch { group }))
}

/// The string of an argument.
fn arg(args: &[Value], at: usize) -> Result<&str> {
    args.get(at).and_then(Value::as_str).ok_or_else(bad_value)
}

/// `RE_compile_and_execute` for an operator: the string and the pattern of the call.
fn matches(args: &[Value], cflags: u32) -> Result<bool> {
    Ok(compile(arg(args, 1)?, cflags)?.is_match(arg(args, 0)?))
}

fn regexeq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(matches(args, flags::ADVANCED)?))
}

fn regexne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(!matches(args, flags::ADVANCED)?))
}

fn icregexeq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(matches(args, flags::ADVANCED | flags::ICASE)?))
}

fn icregexne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(!matches(args, flags::ADVANCED | flags::ICASE)?))
}

/// The text of `s` from the character `so` up to the character `eo`.
fn chars(s: &str, (so, eo): (usize, usize)) -> String {
    s.chars().skip(so).take(eo - so).collect()
}

/// `textregexsubstr` for a compiled pattern: the text of the first group, or of the whole match when the pattern has no group.
fn substring(s: &str, re: &Regex) -> Result<Value> {
    let Some(groups) = re.find(s)? else { return Ok(Value::Null) };
    let at = usize::from(re.groups() > 0);
    Ok(groups[at].map_or(Value::Null, |span| Value::Text(chars(s, span))))
}

/// `textregexsubstr`: `substring(string FROM pattern)`.
fn textregexsubstr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    substring(arg(args, 0)?, &*compile(arg(args, 1)?, flags::ADVANCED)?)
}

/// `substring(string SIMILAR pattern ESCAPE escape)`, the function in SQL of `system_functions.sql` that calls `substring` with `similar_to_escape`.
pub(crate) fn similar_substring(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let run = || {
        let pattern = similar_escape_internal(arg(args, 1)?, Some(arg(args, 2)?))?;
        substring(arg(args, 0)?, &*compile(&pattern, flags::ADVANCED)?)
    };
    // `fmgr_sql` runs the body, and its error context names the statement.
    run().map_err(|e| e.with_context("SQL function \"substring\" statement 1"))
}

/// `similar_escape_internal`: the pattern of `SIMILAR TO` as a regular expression. The escape is `\` when it is `None`, and an empty escape means no escape.
fn similar_escape_internal(pattern: &str, escape: Option<&str>) -> Result<String> {
    let escape = match escape {
        None => Some('\\'),
        Some(e) => {
            let mut chars = e.chars();
            let first = chars.next();
            if chars.next().is_some() {
                return Err(Error::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid escape string")
                    .with_hint("Escape string must be empty or one character."));
            }
            first
        }
    };
    // PostgreSQL reads the pattern by bytes when the escape has one byte, and by characters when it has more. A character of more than one byte then does not change the place in a bracket expression.
    let wide_escape = escape.is_some_and(|e| e.len_utf8() > 1);
    let mut out = String::from("^(?:");
    let mut after_escape = false;
    let mut quotes = 0;
    let mut bracket_depth = 0;
    let mut class_pos = 0;
    for c in pattern.chars() {
        if wide_escape && c.len_utf8() > 1 {
            if after_escape {
                out.push('\\');
                out.push(c);
                after_escape = false;
            } else if Some(c) == escape {
                after_escape = true;
            } else {
                out.push(c);
            }
            continue;
        }
        if after_escape {
            if c == '"' && bracket_depth < 1 {
                match quotes {
                    0 => out.push_str("){1,1}?("),
                    1 => out.push_str("){1,1}(?:"),
                    _ => {
                        return Err(Error::new(
                            SqlState::INVALID_USE_OF_ESCAPE_CHARACTER,
                            "SQL regular expression may not contain more than two escape-double-quote separators",
                        ));
                    }
                }
                quotes += 1;
            } else {
                out.push('\\');
                out.push(c);
                class_pos = 3;
            }
            after_escape = false;
        } else if Some(c) == escape {
            after_escape = true;
        } else if bracket_depth > 0 {
            if c == '\\' {
                out.push('\\');
            }
            out.push(c);
            if c == ']' && class_pos > 2 {
                bracket_depth -= 1;
            } else if c == '[' {
                bracket_depth += 1;
                class_pos = 3;
            } else if c == '^' {
                class_pos += 1;
            } else {
                class_pos = 3;
            }
        } else {
            match c {
                '[' => {
                    out.push(c);
                    bracket_depth = 1;
                    class_pos = 1;
                }
                '%' => out.push_str(".*"),
                '_' => out.push('.'),
                '(' => out.push_str("(?:"),
                '\\' | '.' | '^' | '$' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    out.push_str(")$");
    Ok(out)
}

/// `similar_to_escape(pattern)`, with `\` as the escape.
fn similar_to_escape_1(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(similar_escape_internal(arg(args, 0)?, None)?))
}

/// `similar_to_escape(pattern, escape)`.
fn similar_to_escape_2(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(similar_escape_internal(arg(args, 0)?, Some(arg(args, 1)?))?))
}

/// `similar_escape(pattern, escape)`, the old form that is not strict: a null escape is `\`.
fn similar_escape(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    if matches!(args.first(), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let escape = match args.get(1) {
        Some(Value::Null) | None => None,
        Some(_) => Some(arg(args, 1)?),
    };
    Ok(Value::Text(similar_escape_internal(arg(args, 0)?, escape)?))
}

/// `parse_re_flags`: the compile flags of the options of a function, and true for the option `g`.
fn re_flags(options: Option<&str>) -> Result<(u32, bool)> {
    let mut cflags = flags::ADVANCED;
    let mut glob = false;
    for c in options.unwrap_or_default().chars() {
        match c {
            'g' => glob = true,
            'b' => cflags &= !(flags::ADVANCED | flags::EXTENDED | flags::QUOTE),
            'c' => cflags &= !flags::ICASE,
            'e' => {
                cflags |= flags::EXTENDED;
                cflags &= !(flags::ADVANCED | flags::QUOTE);
            }
            'i' => cflags |= flags::ICASE,
            'm' | 'n' => cflags |= flags::NEWLINE,
            'p' => {
                cflags |= flags::NLSTOP;
                cflags &= !flags::NLANCH;
            }
            'q' => {
                cflags |= flags::QUOTE;
                cflags &= !(flags::ADVANCED | flags::EXTENDED);
            }
            's' => cflags &= !flags::NEWLINE,
            't' => cflags &= !flags::EXPANDED,
            'w' => {
                cflags &= !flags::NLSTOP;
                cflags |= flags::NLANCH;
            }
            'x' => cflags |= flags::EXPANDED,
            _ => {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid regular expression option: \"{c}\""),
                ));
            }
        }
    }
    Ok((cflags, glob))
}

/// The options of a function as its argument at `at`, when it has one.
fn flags_at(args: &[Value], at: usize) -> Result<(u32, bool)> {
    re_flags(if args.len() > at { Some(arg(args, at)?) } else { None })
}

/// The options of a function as its third argument, when it has one.
fn options(args: &[Value]) -> Result<(u32, bool)> {
    flags_at(args, 2)
}

/// The error for the option `g` of a function that finds one match.
fn no_global(function: &str) -> Error {
    Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("{function} does not support the \"global\" option"),
    )
}

/// `regexp_match(string, pattern [, flags])`: the text of each group of the first match, or of the whole match when the pattern has no group.
fn regexp_match(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (cflags, glob) = options(args)?;
    if glob {
        return Err(
            no_global("regexp_match()").with_hint("Use the regexp_matches function instead.")
        );
    }
    let s = arg(args, 0)?;
    let re = compile(arg(args, 1)?, cflags)?;
    let Some(groups) = re.find(s)? else { return Ok(Value::Null) };
    let spans = if re.groups() > 0 { &groups[1..] } else { &groups[..1] };
    let values = spans.iter().map(|span| span.map(|span| Value::Text(chars(s, span))));
    Ok(Value::Array(Box::new(Array::one(values.collect()))))
}

/// `regexp_like(string, pattern [, flags])`.
fn regexp_like(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (cflags, glob) = options(args)?;
    if glob {
        return Err(no_global("regexp_like()"));
    }
    Ok(Value::Bool(compile(arg(args, 1)?, cflags)?.is_match(arg(args, 0)?)))
}

/// The text of the characters `chars` from `so` up to `eo`.
fn text(chars: &[u32], (so, eo): (usize, usize)) -> String {
    chars[so..eo].iter().filter_map(|&c| char::from_u32(c)).collect()
}

/// The position after a match that starts at `so` and ends at `eo`, where the next search starts. The search after an empty match starts one character later, or it finds the same match again.
fn next_search((so, eo): (usize, usize)) -> usize {
    if so == eo { eo + 1 } else { eo }
}

/// The span of the whole match.
fn whole(groups: &tree::Groups) -> Result<(usize, usize)> {
    groups.first().copied().flatten().ok_or_else(|| Error::internal("a match without a span"))
}

/// `setup_regexp_matches`: the spans of each match from the character `start`, or of the first match only when `glob` is false. With `subpatterns`, a match has the span of each group when the pattern has groups. Else it has the span of the whole match. With `ignore_degenerate`, an empty match at the end of the string or just after a match does not count.
fn setup_matches(
    chars: &[u32],
    re: &Regex,
    start: usize,
    glob: bool,
    subpatterns: bool,
    ignore_degenerate: bool,
) -> Result<Vec<tree::Groups>> {
    let subpatterns = subpatterns && re.groups() > 0;
    let mut out = Vec::new();
    let mut search = start;
    let mut prev_end = 0;
    while search <= chars.len() {
        let Some(groups) = re.find_chars(chars, search)? else { break };
        let (so, eo) = whole(&groups)?;
        if !ignore_degenerate || (so < chars.len() && eo > prev_end) {
            out.push(if subpatterns { groups[1..].to_vec() } else { groups[..1].to_vec() });
        }
        prev_end = eo;
        if !glob {
            break;
        }
        search = next_search((so, eo));
    }
    Ok(out)
}

/// The error for a bad value of an integer parameter.
fn bad_parameter(name: &str, value: i64) -> Error {
    Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{name}\": {value}"),
    )
}

/// The integer parameter `name` at `at`, or `default` when the call does not have it. `valid` tells the values that the parameter takes.
fn param(
    args: &[Value],
    (at, name): (usize, &str),
    default: i64,
    valid: impl Fn(i64) -> bool,
) -> Result<i64> {
    if args.len() <= at {
        return Ok(default);
    }
    let value = args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)?;
    if valid(value) { Ok(value) } else { Err(bad_parameter(name, value)) }
}

/// The character where the search starts, from the parameter `start` that counts from 1.
fn start_at(start: i64) -> usize {
    usize::try_from(start - 1).unwrap_or(0)
}

/// `appendStringInfoRegexpSubstr`: the replacement with `\1` to `\9` as the text of each group, `\&` as the text of the match and `\\` as one backslash. A backslash before another character stays.
fn substitute(out: &mut String, replacement: &str, groups: &tree::Groups, chars: &[u32]) {
    let mut it = replacement.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let span = match it.peek() {
            Some(&d @ '1'..='9') => groups.get(d as usize - '0' as usize).copied().flatten(),
            Some('&') => groups.first().copied().flatten(),
            Some('\\') => {
                it.next();
                out.push('\\');
                continue;
            }
            _ => {
                out.push('\\');
                continue;
            }
        };
        it.next();
        if let Some(span) = span {
            out.push_str(&text(chars, span));
        }
    }
}

/// `replace_text_regexp`: `s` with the match number `n` from the character `start` replaced, or each match when `n` is 0.
fn replace(s: &str, re: &Regex, replacement: &str, start: usize, n: i64) -> Result<Value> {
    let chars: Vec<u32> = s.chars().map(u32::from).collect();
    let mut out = String::new();
    let mut done = 0;
    let mut search = start;
    let mut count = 0;
    while search <= chars.len() {
        let Some(groups) = re.find_chars(&chars, search)? else { break };
        let span = whole(&groups)?;
        count += 1;
        if n > 0 && count != n {
            search = next_search(span);
            continue;
        }
        out.push_str(&text(&chars, (done, span.0)));
        substitute(&mut out, replacement, &groups, &chars);
        done = span.1;
        if n > 0 {
            break;
        }
        search = next_search(span);
    }
    out.push_str(&text(&chars, (done, chars.len())));
    Ok(Value::Text(out))
}

/// `textregexreplace_noopt`: `regexp_replace(string, pattern, replacement)`, which replaces the first match.
fn textregexreplace_noopt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let re = compile(arg(args, 1)?, flags::ADVANCED)?;
    replace(arg(args, 0)?, &re, arg(args, 2)?, 0, 1)
}

/// `textregexreplace`: `regexp_replace(string, pattern, replacement, flags)`. Options that start with a digit give an error with a hint, as the call may need the form with a start.
fn textregexreplace(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let options = arg(args, 3)?;
    if let Some(first) = options.chars().next().filter(char::is_ascii_digit) {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid regular expression option: \"{first}\""),
        )
        .with_hint("If you meant to use regexp_replace() with a start parameter, cast the fourth argument to integer explicitly."));
    }
    let (cflags, glob) = re_flags(Some(options))?;
    let re = compile(arg(args, 1)?, cflags)?;
    replace(arg(args, 0)?, &re, arg(args, 2)?, 0, i64::from(!glob))
}

/// `textregexreplace_extended`: `regexp_replace(string, pattern, replacement, start [, n [, flags]])`. Without `n`, the option `g` tells if each match is replaced.
fn textregexreplace_extended(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let start = param(args, (3, "start"), 1, |v| v > 0)?;
    let n = param(args, (4, "n"), 1, |v| v >= 0)?;
    let (cflags, glob) = flags_at(args, 5)?;
    let n = if args.len() <= 4 { i64::from(!glob) } else { n };
    let re = compile(arg(args, 1)?, cflags)?;
    replace(arg(args, 0)?, &re, arg(args, 2)?, start_at(start), n)
}

/// The options at `at` of a function that finds each match, which do not take `g`.
fn all_flags(args: &[Value], at: usize, function: &str) -> Result<u32> {
    let (cflags, glob) = flags_at(args, at)?;
    if glob {
        return Err(no_global(function));
    }
    Ok(cflags)
}

/// `regexp_count(string, pattern [, start [, flags]])`: the number of the matches.
fn regexp_count(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let start = param(args, (2, "start"), 1, |v| v > 0)?;
    let cflags = all_flags(args, 3, "regexp_count()")?;
    let re = compile(arg(args, 1)?, cflags)?;
    let chars: Vec<u32> = arg(args, 0)?.chars().map(u32::from).collect();
    let found = setup_matches(&chars, &re, start_at(start), true, false, false)?;
    Ok(Value::Int4(i32::try_from(found.len()).unwrap_or(i32::MAX)))
}

/// The span of the group `subexpr` of the match number `n` from the character `start` for `regexp_instr` and `regexp_substr`, or of the whole match when `subexpr` is 0. `None` when the string has fewer matches or the pattern fewer groups.
fn nth_span(
    chars: &[u32],
    re: &Regex,
    start: i64,
    n: i64,
    subexpr: i64,
) -> Result<Option<(usize, usize)>> {
    let found = setup_matches(chars, re, start_at(start), true, subexpr > 0, false)?;
    let npatterns = if subexpr > 0 && re.groups() > 0 { re.groups() } else { 1 };
    let (Ok(n), Ok(subexpr)) = (usize::try_from(n), usize::try_from(subexpr)) else {
        return Ok(None);
    };
    if n > found.len() || subexpr > npatterns {
        return Ok(None);
    }
    Ok(found[n - 1][subexpr.saturating_sub(1)])
}

/// `regexp_instr(string, pattern [, start [, n [, endoption [, flags [, subexpr]]]]])`: the position from 1 of the start of the match, or of the character after it when `endoption` is 1, or 0.
fn regexp_instr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let start = param(args, (2, "start"), 1, |v| v > 0)?;
    let n = param(args, (3, "n"), 1, |v| v > 0)?;
    let endoption = param(args, (4, "endoption"), 0, |v| v == 0 || v == 1)?;
    let subexpr = param(args, (6, "subexpr"), 0, |v| v >= 0)?;
    let cflags = all_flags(args, 5, "regexp_instr()")?;
    let re = compile(arg(args, 1)?, cflags)?;
    let chars: Vec<u32> = arg(args, 0)?.chars().map(u32::from).collect();
    let pos = nth_span(&chars, &re, start, n, subexpr)?
        .map_or(0, |(so, eo)| if endoption == 1 { eo + 1 } else { so + 1 });
    Ok(Value::Int4(i32::try_from(pos).unwrap_or(i32::MAX)))
}

/// `regexp_substr(string, pattern [, start [, n [, flags [, subexpr]]]])`: the text of the match, or null.
fn regexp_substr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let start = param(args, (2, "start"), 1, |v| v > 0)?;
    let n = param(args, (3, "n"), 1, |v| v > 0)?;
    let subexpr = param(args, (5, "subexpr"), 0, |v| v >= 0)?;
    let cflags = all_flags(args, 4, "regexp_substr()")?;
    let re = compile(arg(args, 1)?, cflags)?;
    let chars: Vec<u32> = arg(args, 0)?.chars().map(u32::from).collect();
    Ok(nth_span(&chars, &re, start, n, subexpr)?
        .map_or(Value::Null, |span| Value::Text(text(&chars, span))))
}

/// `regexp_matches(string, pattern [, flags])`: one row for each match, or for the first match without the option `g`. Each row is an array with the text of each group, or of the match when the pattern has no group.
pub(crate) fn regexp_matches(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let (cflags, glob) = options(args)?;
    let re = compile(arg(args, 1)?, cflags)?;
    let chars: Vec<u32> = arg(args, 0)?.chars().map(u32::from).collect();
    let found = setup_matches(&chars, &re, 0, glob, true, false)?;
    Ok(found
        .iter()
        .map(|spans| {
            let values = spans.iter().map(|span| span.map(|span| Value::Text(text(&chars, span))));
            vec![Value::Array(Box::new(Array::one(values.collect())))]
        })
        .collect())
}

/// The parts of the string between the matches, for `regexp_split_to_table` and `regexp_split_to_array`. An empty match at the end of the string or just after a match does not split.
fn split(args: &[Value], function: &str) -> Result<Vec<String>> {
    let cflags = all_flags(args, 2, function)?;
    let re = compile(arg(args, 1)?, cflags)?;
    let chars: Vec<u32> = arg(args, 0)?.chars().map(u32::from).collect();
    let found = setup_matches(&chars, &re, 0, true, false, true)?;
    let mut parts = Vec::with_capacity(found.len() + 1);
    let mut from = 0;
    for groups in &found {
        let (so, eo) = whole(groups)?;
        parts.push(text(&chars, (from, so)));
        from = eo;
    }
    parts.push(text(&chars, (from, chars.len())));
    Ok(parts)
}

/// `regexp_split_to_table(string, pattern [, flags])`: one row for each part.
pub(crate) fn regexp_split_to_table(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(split(args, "regexp_split_to_table()")?
        .into_iter()
        .map(|part| vec![Value::Text(part)])
        .collect())
}

/// `regexp_split_to_array(string, pattern [, flags])`: the parts as an array.
fn regexp_split_to_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let parts = split(args, "regexp_split_to_array()")?;
    let values = parts.into_iter().map(|part| Some(Value::Text(part))).collect();
    Ok(Value::Array(Box::new(Array::one(values))))
}

/// The kernels of the regular expression functions by `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "textregexeq" | "nameregexeq" => regexeq,
        "textregexne" | "nameregexne" => regexne,
        "texticregexeq" | "nameicregexeq" => icregexeq,
        "texticregexne" | "nameicregexne" => icregexne,
        "textregexsubstr" => textregexsubstr,
        "similar_to_escape_1" => similar_to_escape_1,
        "similar_to_escape_2" => similar_to_escape_2,
        "similar_escape" => similar_escape,
        "regexp_match" | "regexp_match_no_flags" => regexp_match,
        "regexp_like" | "regexp_like_no_flags" => regexp_like,
        "textregexreplace_noopt" => textregexreplace_noopt,
        "textregexreplace" => textregexreplace,
        "textregexreplace_extended"
        | "textregexreplace_extended_no_n"
        | "textregexreplace_extended_no_flags" => textregexreplace_extended,
        "regexp_count" | "regexp_count_no_start" | "regexp_count_no_flags" => regexp_count,
        "regexp_instr"
        | "regexp_instr_no_start"
        | "regexp_instr_no_n"
        | "regexp_instr_no_endoption"
        | "regexp_instr_no_flags"
        | "regexp_instr_no_subexpr" => regexp_instr,
        "regexp_substr"
        | "regexp_substr_no_start"
        | "regexp_substr_no_n"
        | "regexp_substr_no_flags"
        | "regexp_substr_no_subexpr" => regexp_substr,
        "regexp_split_to_array" | "regexp_split_to_array_no_flags" => regexp_split_to_array,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(pattern: &str, s: &str) -> bool {
        Regex::compile(pattern, flags::ADVANCED)
            .map(|r| r.is_match(s))
            .unwrap_or_else(|code| panic!("{pattern}: {code:?}"))
    }

    fn err(pattern: &str) -> Code {
        Regex::compile(pattern, flags::ADVANCED)
            .err()
            .unwrap_or_else(|| panic!("{pattern} compiled"))
    }

    #[test]
    fn matches_like_postgres() {
        assert!(is("^(pg_class)$", "pg_class"));
        assert!(!is("^(pg_class)$", "pg_classes"));
        assert!(is("a.c", "xabcx"));
        assert!(is("^a{2,3}$", "aaa"));
        assert!(!is("^a{2,3}$", "aaaa"));
        assert!(is("(a*)*b", "aaab"));
        assert!(is("^(a+)\\1$", "aaaa"));
        assert!(!is("^(a+)\\1$", "aaa"));
        assert!(is("foo(?=bar)", "foobar"));
        assert!(!is("foo(?!bar)", "foobar"));
        assert!(is("(?<=a)b", "ab"));
        assert!(!is("(?<!a)b", "ab"));
        assert!(is("\\mfoo\\M", "a foo b"));
        assert!(!is("\\mfoo\\M", "afoob"));
        assert!(is("[[:digit:]]+", "x12"));
        assert!(is("[^a-c]", "abcd"));
        assert!(!is("[^a-c]", "abc"));
    }

    #[test]
    fn errors_like_postgres() {
        assert_eq!(err("*a"), Code::BadRpt);
        assert_eq!(err("a)"), Code::Paren);
        assert_eq!(err("(a"), Code::Paren);
        assert_eq!(err("[a"), Code::Brack);
        assert_eq!(err("a{3,2}"), Code::BadBr);
        assert_eq!(err("[z-a]"), Code::Range);
        assert_eq!(err("[[:foo:]]"), Code::Ctype);
        assert_eq!(err("\\1(a)"), Code::Subreg);
    }
}

//! The regular expressions of PostgreSQL, the engine of Henry Spencer in `src/backend/regex`, for the C locale.
//!
//! The lexer and the parser are ports of `regc_lex.c` and `regcomp.c`, so that a pattern gives the same error as in PostgreSQL. The matcher is not a port: it is a backtracking machine that tells if a match is in the string, which is all that the operators `~`, `~*`, `!~` and `!~*` need. The positions of the groups of a match, which `regexp_match` and `substring` need, follow the rules of preference of Spencer and are not done yet.

mod exec;
mod lex;
mod parse;

use std::cell::RefCell;
use std::rc::Rc;

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

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
    prog: exec::Program,
}

impl Regex {
    /// `pg_regcomp` for a pattern and its flags.
    fn compile(pattern: &str, cflags: u32) -> std::result::Result<Self, Code> {
        let chars: Vec<u32> = pattern.chars().map(u32::from).collect();
        let parsed = parse::Parser::run(&chars, cflags)?;
        Ok(Regex { prog: exec::Program::compile(&parsed)? })
    }

    /// True when the pattern matches a part of `s`.
    pub(crate) fn is_match(&self, s: &str) -> bool {
        let chars: Vec<u32> = s.chars().map(u32::from).collect();
        self.prog.search(&chars)
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

/// The kernels of the regular expression operators by `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "textregexeq" | "nameregexeq" => regexeq,
        "textregexne" | "nameregexne" => regexne,
        "texticregexeq" | "nameicregexeq" => icregexeq,
        "texticregexne" | "nameicregexne" => icregexne,
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

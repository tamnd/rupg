//! The LR parser that runs the generated tables.
//!
//! Lifted from `crates/rudb-pgparse/src/parser.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::vec::Drain;

use crate::actions::Parser;
use crate::error::{Error, Notice};
use crate::filter::Tokens;
use crate::generated::keywords::{self, Keyword};
use crate::generated::tables::{
    ACTION_BASE, CHARACTER, CHECK, DEFAULT_GOTO, DEFAULT_REDUCTION, ENTRIES, ERROR, FINAL,
    GOTO_BASE, NO_BASE, RULE_LENGTH, RULE_LHS, RULE_NAME, SYMBOL_NAME,
};
use crate::lexer::Lexer;

/// The token of an ASCII character that is a token by itself, such as `;` or `+`.
pub fn character(c: u8) -> Option<u16> {
    let token = *CHARACTER.get(usize::from(c))?;
    (token != 0).then_some(token)
}

/// The keyword that a word spells, with ASCII letters in any case, as `kwlist.h` has it.
pub fn keyword(word: &str) -> Option<&'static Keyword> {
    keywords::lookup(word.as_bytes())
}

/// The place where the grammar stops accepting the input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyntaxError {
    /// The index of the token that the grammar does not accept. It is the number of tokens when the input ends too early.
    pub token: usize,
}

/// The name of a rule as `productions.txt` has it, for example `a_expr.17`, or `None` when there is no such rule.
pub fn rule_name(rule: usize) -> Option<&'static str> {
    RULE_NAME.get(rule).copied()
}

/// The name of a symbol as `gram.y` has it, for example `IDENT`, `'+'` or `a_expr`, or `None` when there is no such symbol. The tokens come first.
pub fn symbol_name(symbol: usize) -> Option<&'static str> {
    SYMBOL_NAME.get(symbol).copied()
}

/// The entry of the action row of a state for a token, or the default of the state.
fn action(state: usize, token: u16) -> i16 {
    let base = ACTION_BASE[state];
    if base != NO_BASE {
        let at = base + i32::from(token);
        if let Ok(at) = usize::try_from(at)
            && CHECK.get(at) == Some(&(token as i16))
        {
            return ENTRIES[at];
        }
    }
    match DEFAULT_REDUCTION[state] {
        0 => ERROR,
        rule => -(rule as i16),
    }
}

/// The state that a nonterminal goes to from a state.
fn goto(state: usize, nonterminal: usize) -> usize {
    let base = GOTO_BASE[nonterminal];
    if base != NO_BASE {
        let at = base + state as i32;
        if let Ok(at) = usize::try_from(at)
            && CHECK.get(at) == Some(&(state as i16))
        {
            return ENTRIES[at] as usize;
        }
    }
    usize::from(DEFAULT_GOTO[nonterminal])
}

/// What the parser does with the tokens and the rules: [`run`] asks it for the tokens and gives it each reduction.
pub(crate) trait Machine {
    /// The value of a symbol.
    type Value;
    /// The error of the lexer or of an action.
    type Error;

    /// The next token, its value and its location. The end of the input is token 0.
    fn token(&mut self) -> Result<(u16, Self::Value, i32), Self::Error>;

    /// Runs the action of `rule` on the values of its right side, with their locations and the location of the rule, and gives the value of the rule.
    fn reduce(
        &mut self,
        rule: usize,
        rhs: Drain<'_, Self::Value>,
        at: &[i32],
        here: i32,
    ) -> Result<Self::Value, Self::Error>;
}

/// Runs the grammar. The parser asks for a token only when the state needs one, as the parser of bison does, so an error of the lexer after a syntax error does not come first. Gives the value of the start symbol, or `None` at a syntax error, which is at the last token that the machine gave.
pub(crate) fn run<M: Machine>(machine: &mut M) -> Result<Option<M::Value>, M::Error> {
    let mut stack: Vec<usize> = vec![0];
    // The value and the location of each symbol on the stack. The first state has no symbol.
    let mut values: Vec<M::Value> = Vec::new();
    let mut locations: Vec<i32> = Vec::new();
    let mut ahead: Option<(u16, M::Value, i32)> = None;
    loop {
        // The stack is never empty: a reduction pops the symbols of its right side, and the first state is under all of them.
        let state = stack[stack.len() - 1];
        let entry = if ACTION_BASE[state] == NO_BASE {
            match DEFAULT_REDUCTION[state] {
                0 => ERROR,
                rule => -(rule as i16),
            }
        } else {
            let token = match &ahead {
                Some((token, _, _)) => *token,
                None => ahead.insert(machine.token()?).0,
            };
            action(state, token)
        };
        if entry == ERROR {
            if ahead.is_none() {
                machine.token()?;
            }
            return Ok(None);
        }
        if entry > 0 {
            if entry == FINAL {
                return Ok(values.pop());
            }
            let Some((_, value, location)) = ahead.take() else {
                // A shift is only in a state that reads a token, so there is a token.
                return Ok(None);
            };
            stack.push(entry as usize);
            values.push(value);
            locations.push(location);
            continue;
        }
        let rule = usize::from(entry.unsigned_abs());
        let from = stack.len() - 1 - usize::from(RULE_LENGTH[rule]);
        let at = &locations[from..];
        // `YYLLOC_DEFAULT` of `gram.y`: the first location of the right side that is known.
        let here = at.iter().copied().find(|&location| location >= 0).unwrap_or(-1);
        let value = machine.reduce(rule, values.drain(from..), at, here)?;
        locations.truncate(from);
        values.push(value);
        locations.push(here);
        stack.truncate(from + 1);
        let state = stack[stack.len() - 1];
        stack.push(goto(state, usize::from(RULE_LHS[rule])));
    }
}

/// The machine of [`recognize`]: a list of tokens and no actions.
struct List<'a> {
    tokens: &'a [u16],
    at: usize,
}

impl Machine for List<'_> {
    type Value = ();
    type Error = std::convert::Infallible;

    fn token(&mut self) -> Result<(u16, (), i32), Self::Error> {
        self.at += 1;
        Ok((self.tokens.get(self.at - 1).copied().unwrap_or(0), (), -1))
    }

    fn reduce(&mut self, _: usize, _: Drain<'_, ()>, _: &[i32], _: i32) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Runs the grammar over a list of tokens. The list does not end with the end token; the end of the slice is the end of the input.
pub fn recognize(tokens: &[u16]) -> Result<(), SyntaxError> {
    let mut list = List { tokens, at: 0 };
    match run(&mut list) {
        Ok(Some(())) => Ok(()),
        Ok(None) | Err(_) => Err(SyntaxError { token: list.at - 1 }),
    }
}

/// The machine of [`check`]: the tokens of a text and no actions.
struct Check<'a> {
    tokens: Tokens<'a>,
    last: (usize, usize),
}

impl Machine for Check<'_> {
    type Value = ();
    type Error = Error;

    fn token(&mut self) -> Result<(u16, (), i32), Error> {
        let token = self.tokens.next_token()?;
        self.last = (token.start, token.end);
        Ok((token.kind, (), -1))
    }

    fn reduce(&mut self, _: usize, _: Drain<'_, ()>, _: &[i32], _: i32) -> Result<(), Error> {
        Ok(())
    }
}

/// Lexes and parses `text` as PostgreSQL does, and gives the first error of the lexer or the grammar, or the notices when there is no error.
pub fn check(text: &str) -> Result<Vec<Notice>, Error> {
    let mut check = Check { tokens: Tokens::new(text), last: (0, 0) };
    if run(&mut check)?.is_some() {
        Ok(check.tokens.take_notices())
    } else {
        let text = Lexer::new(text).text().as_bytes();
        Err(Error::syntax(text, "syntax error", check.last.0, check.last.1))
    }
}

/// The raw parse tree of `text` and the notices of the parse, as `raw_parser` of PostgreSQL gives them: a list with a `RawStmt` for each statement. An error is the first error of the lexer, the grammar or an action.
pub fn parse(text: &str) -> Result<(crate::nodes::List, Vec<Notice>), Error> {
    let mut parser = Parser::new(text);
    if run(&mut parser)?.is_none() {
        return Err(parser.yyerror("syntax error"));
    }
    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::*;

    fn c(ch: u8) -> u16 {
        character(ch).unwrap()
    }

    #[test]
    fn statements() {
        assert_eq!(recognize(&[]), Ok(()));
        assert_eq!(recognize(&[SELECT, ICONST]), Ok(()));
        assert_eq!(recognize(&[SELECT, ICONST, c(b'+'), ICONST, c(b';'), SELECT]), Ok(()));
        assert_eq!(recognize(&[SELECT, FROM, IDENT]), Ok(()));
        let create = [CREATE, TABLE, IDENT, c(b'('), IDENT, INT_P, c(b','), IDENT, IDENT, c(b')')];
        assert_eq!(recognize(&create), Ok(()));
        let not_in = [SELECT, IDENT, NOT_LA, IN_P, c(b'('), ICONST, c(b')')];
        assert_eq!(recognize(&not_in), Ok(()));
    }

    #[test]
    fn names() {
        assert_eq!(rule_name(1), Some("parse_toplevel.1"));
        assert_eq!(rule_name(crate::RULES), None);
        assert_eq!(symbol_name(usize::from(SELECT)), Some("SELECT"));
        assert_eq!(symbol_name(usize::from(c(b'+'))), Some("'+'"));
        assert_eq!(symbol_name(crate::TOKENS), Some("$accept"));
    }

    #[test]
    fn errors() {
        assert_eq!(recognize(&[SELECT, ICONST, c(b'+')]), Err(SyntaxError { token: 3 }));
        assert_eq!(recognize(&[SELECT, ICONST, ICONST]), Err(SyntaxError { token: 2 }));
        assert_eq!(recognize(&[CREATE, SELECT]), Err(SyntaxError { token: 1 }));
        // `a = b = c` is an error because `=` does not associate.
        let chain = [SELECT, IDENT, c(b'='), IDENT, c(b'='), IDENT];
        assert_eq!(recognize(&chain), Err(SyntaxError { token: 4 }));
    }

    #[test]
    fn keywords() {
        use crate::Category;
        use crate::generated::keywords::KEYWORDS;
        assert_eq!(
            keyword("SELECT").map(|k| (k.token, k.category)),
            Some((SELECT, Category::Reserved))
        );
        assert_eq!(keyword("abort").map(|k| k.token), Some(ABORT_P));
        assert_eq!(keyword("zone").map(|k| k.bare_label), Some(true));
        assert!(keyword("selectx").is_none());
        assert!(keyword("").is_none());
        for k in &KEYWORDS {
            assert!(std::ptr::eq(keyword(k.name).unwrap(), k));
            assert!(std::ptr::eq(keyword(&k.name.to_uppercase()).unwrap(), k));
        }
    }

    /// The first error of each text, as the PostgreSQL 19 server gives it: the SQLSTATE, the position and the message.
    #[test]
    fn check_errors() {
        let cases = [
            ("SELEC 1", "42601", 1, "syntax error at or near \"SELEC\""),
            ("SELECT 1 +", "42601", 11, "syntax error at end of input"),
            ("SELECT 'abc", "42601", 8, "unterminated quoted string at or near \"'abc\""),
            (
                "SELECT /* x /* y */",
                "42601",
                8,
                "unterminated /* comment at or near \"/* x /* y */\"",
            ),
            ("SELECT 1abc", "42601", 8, "trailing junk after numeric literal at or near \"1abc\""),
            ("SELECT 0x", "42601", 8, "invalid hexadecimal integer at or near \"0x\""),
            ("SELECT 0b2", "42601", 8, "trailing junk after numeric literal at or near \"0b2\""),
            (
                "SELECT 1_000$1",
                "42601",
                8,
                "trailing junk after numeric literal at or near \"1_000$1\"",
            ),
            ("SELECT $1a", "42601", 8, "trailing junk after parameter at or near \"$1a\""),
            ("SELECT \"\"", "42601", 8, "zero-length delimited identifier at or near \"\"\"\""),
            ("SELECT E'\\u12'", "22025", 10, "invalid Unicode escape"),
            ("SELECT E'\\0'", "22021", 0, "invalid byte sequence for encoding \"UTF8\": 0x00"),
            ("SELECT E'\\xff'", "22021", 0, "invalid byte sequence for encoding \"UTF8\": 0xff"),
            ("SELECT U&'\\0000'", "42601", 11, "invalid Unicode escape value"),
            (
                "SELECT U&'x' UESCAPE 'ab'",
                "42601",
                22,
                "invalid Unicode escape character at or near \"'ab'\"",
            ),
            (
                "SELECT U&'x' UESCAPE 1",
                "42601",
                22,
                "UESCAPE must be followed by a simple string literal at or near \"1\"",
            ),
            (
                "SELECT U&'x' UESCAPE",
                "42601",
                21,
                "UESCAPE must be followed by a simple string literal at end of input",
            ),
            (
                "SELECT $99999999999",
                "42601",
                8,
                "parameter number too large at or near \"$99999999999\"",
            ),
            ("SELECT 'a' 'b'", "42601", 12, "syntax error at or near \"'b'\""),
            (
                "SELECT $a$abc$b$",
                "42601",
                8,
                "unterminated dollar-quoted string at or near \"$a$abc$b$\"",
            ),
            ("SELECT B'01", "42601", 8, "unterminated bit string literal at or near \"B'01\""),
            (
                "SELECT X'0f",
                "42601",
                8,
                "unterminated hexadecimal string literal at or near \"X'0f\"",
            ),
            ("SELECT E'\\ud83d'", "42601", 16, "invalid Unicode surrogate pair at or near \"'\""),
            (
                "SELECT E'\\ud83d\\u0041'",
                "42601",
                16,
                "invalid Unicode surrogate pair at or near \"\\u0041\"",
            ),
            (
                "SELECT E'\\udc00'",
                "42601",
                10,
                "invalid Unicode surrogate pair at or near \"\\udc00\"",
            ),
            (
                "SELECT E'\\U00110000'",
                "42601",
                10,
                "invalid Unicode escape value at or near \"\\U00110000\"",
            ),
            ("SELECT U&'\\D83Dx'", "42601", 16, "invalid Unicode surrogate pair"),
            ("SELECT U&'\\12'", "42601", 11, "invalid Unicode escape"),
            ("SELECT U&'!0041' UESCAPE '!' 1", "42601", 30, "syntax error at or near \"1\""),
            ("SELECT 1e+", "42601", 8, "trailing junk after numeric literal at or near \"1e+\""),
            ("SELECT 1..2", "42601", 9, "syntax error at or near \"..\""),
            ("SELECT é FROM 1", "42601", 15, "syntax error at or near \"1\""),
            ("SELECT 'a'\n-- c\n'b' 1", "42601", 21, "syntax error at or near \"1\""),
            ("SELECT 1 =- 1 1", "42601", 15, "syntax error at or near \"1\""),
            ("SELECT 1 <=> 1 1", "42601", 16, "syntax error at or near \"1\""),
            ("SELECT 0x_", "42601", 8, "invalid hexadecimal integer at or near \"0x_\""),
            ("SELECT E'\\", "42601", 8, "unterminated quoted string at or near \"E'\\\""),
            ("SELECT 1 WITH ORDINALITY", "42601", 10, "syntax error at or near \"WITH\""),
            ("SELECT 1 /* a */ /* b", "42601", 18, "unterminated /* comment at or near \"/* b\""),
            ("SELECT $1 $2", "42601", 11, "syntax error at or near \"$2\""),
        ];
        for (text, code, position, message) in cases {
            let error = check(text).unwrap_err();
            let got =
                (error.code.as_str(), error.position(text).unwrap_or(0), error.message.as_str());
            assert_eq!(got, (code, position, message), "{text}");
        }
        assert_eq!(check("SELECT 1 NOT IN (1); SELECT 'a'\n'b'"), Ok(Vec::new()));
        // The lexer does not read past a syntax error, so the open string after it is no error.
        assert_eq!(check("SELEC 1 'abc").unwrap_err().message, "syntax error at or near \"SELEC\"");
        assert_eq!(check(&"a".repeat(64)).unwrap_err().code.as_str(), "42601");
        let notices = check(&format!("SELECT {}", "a".repeat(64))).unwrap();
        assert_eq!(notices[0].code.as_str(), "42622");
    }
}

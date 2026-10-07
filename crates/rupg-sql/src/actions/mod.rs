//! The actions of `gram.y` that build the raw parse tree, with the functions that the actions call.
//!
//! `cargo xtask sql` translates most actions from C to Rust and writes them into `src/generated/glue.rs`. An action that the translator cannot write, for example one with a loop or a `switch`, is a method of a trait, one trait for each rule of `gram.y`. The default of a method gives the error that the action is not ported. A file here implements the trait of a rule for [`Parser`] and ports the C of each such alternative by hand, line by line. After a new `impl`, `cargo xtask sql` removes the empty `impl` of that trait from the glue, and when the translator can write an action that has a port here, it fails until the port is removed.
//!
//! The arguments of a method are the values `$k` that the C uses, then the locations `@k`, then `@$`, and a port names them `vk`, `atk` and `here`. Bison sets `$$` to `$1` before an action, so a port of an action that does not always set `$$` gives `$1` where the C sets nothing.
//!
//! [`make`] has the functions of `makefuncs.c` and `value.c` that the actions use, [`funcs`] those of `nodeFuncs.c`, [`list`] the list and cast macros of `pg_list.h` and `nodes.h`, and [`gram`] the static functions of the prologue and the epilogue of `gram.y`. They keep the C names, so that a port reads as the C does. The translated actions call them too: a `pub(crate) fn` at the start of a line in these files, with no generic parameters, is a function that the translator knows by its signature. A result of `Result<T, Error>` tells it that the function can fail.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/mod.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

#![allow(non_snake_case)]

mod expr;
mod funcs;
mod gram;
mod json;
mod list;
mod make;
mod names;
mod select;
mod stmt;
mod types;

pub(crate) use gram::*;
pub(crate) use list::*;
pub(crate) use make::*;

use std::vec::Drain;

use rupg_common::SqlState;

use crate::error::{Error, Notice, Severity};
use crate::filter::Tokens;
use crate::generated::glue::{self, Value};
use crate::lexer::Lexer;
use crate::nodes::List;
use crate::parser::Machine;

/// The state of a parse with actions, as `base_yy_extra_type` and the scanner have it in C.
pub(crate) struct Parser<'a> {
    /// The text that the parser reads, up to the first zero byte.
    text: &'a str,
    tokens: Tokens<'a>,
    /// The start and the end of the last token that the parser read, for `parser_yyerror`.
    last: (usize, usize),
    /// The notices of the lexer and of the actions, in order.
    notices: Vec<Notice>,
    /// The `parsetree` of `base_yy_extra_type`: the list that the start rule gives.
    pub(crate) parsetree: List,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(text: &'a str) -> Parser<'a> {
        let text = Lexer::new(text).text();
        Parser {
            text,
            tokens: Tokens::new(text),
            last: (0, 0),
            notices: Vec::new(),
            parsetree: List::new(),
        }
    }

    /// `parser_yyerror`: an error at the last token that the parser read.
    pub(crate) fn yyerror(&self, message: &str) -> Error {
        Error::syntax(self.text.as_bytes(), message, self.last.0, self.last.1)
    }

    /// `ereport(WARNING, errmsg(message), parser_errposition(location))`.
    pub(crate) fn warning(&mut self, message: &str, location: i32) {
        self.notices.push(Notice {
            severity: Severity::Warning,
            code: SqlState::WARNING,
            message: message.to_owned(),
            location: usize::try_from(location).ok(),
        });
    }

    /// The tree and all the notices.
    pub(crate) fn finish(mut self) -> (List, Vec<Notice>) {
        self.notices.append(&mut self.tokens.take_notices());
        (self.parsetree, self.notices)
    }
}

impl Machine for Parser<'_> {
    type Value = Value;
    type Error = Error;

    fn token(&mut self) -> Result<(u16, Value, i32), Error> {
        let token = self.tokens.next_token()?;
        // The notices of the lexer come before those of the actions that run after this token.
        self.notices.append(&mut self.tokens.take_notices());
        self.last = (token.start, token.end);
        let location = i32::try_from(token.start).unwrap_or(i32::MAX);
        Ok((token.kind, glue::token(token.value), location))
    }

    fn reduce(
        &mut self,
        rule: usize,
        rhs: Drain<'_, Value>,
        at: &[i32],
        here: i32,
    ) -> Result<Value, Error> {
        glue::reduce(self, rule, rhs, at, here)
    }
}

/// The values of the right side of a rule, in order. The parser gives as many values as the rule has symbols, so the array is always full.
pub(crate) fn take<const N: usize>(mut rhs: Drain<'_, Value>) -> [Value; N] {
    std::array::from_fn(|_| rhs.next().unwrap_or_default())
}

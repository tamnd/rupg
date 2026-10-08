//! The lexer, the parser from the vendored `gram.y`, the parse tree and its printer.
//!
//! This crate first ships in milestone M2. See `spec/07-sql-types-and-catalog.md` section 7.2, `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md` section 23.5.
//!
//! Lifted from `crates/rudb-pgparse/src/lib.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).
//!
//! The grammar is not written by hand. `cargo xtask vendor postgres-19` copies `gram.y`, `scan.l`, `parser.c`, `kwlist.h` and the node headers from PostgreSQL at the pin into `vendor/postgres-19`. `cargo xtask sql` makes the LALR(1) tables in `src/generated/` from them, with the state numbers that `bison -v` gives for the same grammar. So a statement that PostgreSQL parses is a statement that this crate parses, and a syntax error stops at the same token.
//!
//! # What is here
//!
//! [`keyword`], which says whether a word is a keyword and of which [`Category`], and [`token`], the token numbers.
//!
//! [`Lexer`], a port of `scan.l`, and [`Tokens`], the filter of `parser.c` after it, which looks one token further for the few places where the grammar needs two tokens.
//!
//! [`check`], which lexes and parses a text and gives the same first error as PostgreSQL, with the same message and position. [`recognize`] runs the tables over a list of tokens.
//!
//! [`nodes`], the node types of the raw parse tree of PostgreSQL, generated from the vendored headers with the names and the fields of C. Each node writes the text of `nodeToString`, which PostgreSQL logs for a statement when `debug_print_raw_parse` is on, so a test compares the two trees. [`parse`] runs the actions of `gram.y` and gives this tree.

#![forbid(unsafe_code)]

mod actions;
mod error;
mod filter;
mod generated;
mod lexer;
pub mod nodes;
mod parser;

pub use error::{Error, Notice, Severity};
pub use filter::Tokens;
pub use generated::keywords::{Category, Keyword};
pub use generated::tables::{RULES, STATES, TOKENS, token};
pub use lexer::{Lexer, Token, Value};
pub use parser::{
    SyntaxError, character, check, keyword, parse, parse_type_name, recognize, rule_name,
    symbol_name,
};

//! The raw parse trees of `parse` against those of PostgreSQL 19.
//!
//! `raw.txt` has a case for each statement: a line `> ` and the text, then the tree that PostgreSQL logs for the text with `debug_print_raw_parse` on and `debug_pretty_print` off, on one line, then an empty line. For a text that the raw parser of PostgreSQL rejects, the line after the text is `ERROR`, the SQLSTATE, the position and the message, then a line `DETAIL` and a line `HINT` when the error has them. The trees and the errors come from the oracle server of `tamnd/rudb-postgres`.
//!
//! Lifted from `crates/rudb-pgparse/tests/raw.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_sql::nodes::list_text;
use rupg_sql::{Error, parse};

/// The text of an error in `raw.txt`.
fn error_text(error: &Error, text: &str) -> String {
    let position = error.position(text).unwrap_or(0);
    let mut out = format!("ERROR {} {position} {}", error.code, error.message);
    if let Some(detail) = error.detail {
        out.push_str(&format!("\nDETAIL {detail}"));
    }
    if let Some(hint) = error.hint {
        out.push_str(&format!("\nHINT {hint}"));
    }
    out
}

/// The stack of the thread that parses the cases. In a debug build, the action functions of the glue have large frames, and the cases of `raw.txt` need more than the 2 MiB of a test thread. 8 MiB is the stack of the main thread on Linux.
const STACK: usize = 8 << 20;

#[test]
// The test needs a thread with a larger stack than the test thread. It is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
#[allow(clippy::disallowed_methods)]
fn raw_trees() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(compare)
        .expect("the test thread starts")
        .join()
        .expect("the cases of raw.txt parse");
}

fn compare() {
    let cases = include_str!("raw.txt");
    let mut failures = Vec::new();
    let mut count = 0;
    for case in cases.split("\n\n").filter(|case| !case.trim().is_empty()) {
        let Some((text, expected)) = case.trim_start_matches('\n').split_once('\n') else {
            panic!("a case of raw.txt has no tree: {case}");
        };
        let text = text.strip_prefix("> ").expect("a case of raw.txt starts with `> `");
        count += 1;
        let actual = match parse(text) {
            Ok((tree, _)) => list_text(&tree),
            Err(error) => error_text(&error, text),
        };
        if actual != expected {
            failures.push(format!("{text}\n  expected {expected}\n  actual   {actual}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

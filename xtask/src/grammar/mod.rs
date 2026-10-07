//! The LALR(1) generator for the SQL grammar (spec/22 section 22.8).
//!
//! `cargo xtask grammar` reads the vendored `gram.y` and `pl_gram.y`, builds the LALR(1) automaton of each, and fails if the conflict counts differ from the `%expect` and `%expect-rr` of the file. A grammar without these directives must have no conflicts. `cargo xtask grammar --compare <file.y> <file.output>` compares the full tables with a bison report.

use std::collections::BTreeSet;
use std::path::Path;

mod lalr;
mod reader;
mod report;

const GRAMMARS: [&str; 2] = [
    "vendor/postgres-19/src/backend/parser/gram.y",
    "vendor/postgres-19/src/pl/plpgsql/src/pl_gram.y",
];

/// Runs the task. With no arguments it checks the vendored grammars.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [] => {
            for path in GRAMMARS {
                check(&root.join(path))?;
            }
            Ok(())
        }
        [flag, grammar, output] if flag == "--compare" => {
            compare(Path::new(grammar), Path::new(output))
        }
        files if files.iter().all(|f| !f.starts_with('-')) => {
            for file in files {
                check(Path::new(file))?;
            }
            Ok(())
        }
        _ => {
            Err("usage: cargo xtask grammar [file.y...] | --compare <file.y> <file.output>"
                .to_string())
        }
    }
}

fn load(path: &Path) -> Result<(reader::Grammar, lalr::Automaton), String> {
    let grammar =
        reader::read(&crate::read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let automaton = lalr::build(&grammar);
    Ok((grammar, automaton))
}

fn check(path: &Path) -> Result<(), String> {
    let (g, a) = load(path)?;
    let (sr, rr) = (a.sr_conflicts(), a.rr_conflicts());
    println!(
        "{}: {} terminals, {} nonterminals, {} rules, {} states, {sr} shift/reduce, {rr} reduce/reduce",
        path.display(),
        g.terminals,
        g.names.len() - g.terminals,
        g.rules.len(),
        a.states.len(),
    );
    let (want_sr, want_rr) = (g.expect.unwrap_or(0), g.expect_rr.unwrap_or(0));
    if sr != want_sr || rr != want_rr {
        for (i, state) in a.states.iter().enumerate() {
            if state.sr_conflicts + state.rr_conflicts > 0 {
                let lines: BTreeSet<usize> =
                    state.reductions.iter().map(|&r| g.rules[r as usize].line).collect();
                let lines: Vec<String> = lines.iter().map(usize::to_string).collect();
                eprintln!(
                    "  state {i}: {} shift/reduce, {} reduce/reduce, reductions of the rules at line {}",
                    state.sr_conflicts,
                    state.rr_conflicts,
                    lines.join(", ")
                );
            }
        }
        return Err(format!(
            "{}: expected {want_sr} shift/reduce and {want_rr} reduce/reduce conflicts",
            path.display()
        ));
    }
    Ok(())
}

fn compare(grammar: &Path, output: &Path) -> Result<(), String> {
    let (g, a) = load(grammar)?;
    let problems = report::compare(&g, &a, &crate::read(output)?)?;
    if problems.is_empty() {
        println!(
            "{}: the {} states are the same as in {}",
            grammar.display(),
            a.states.len(),
            output.display()
        );
        return Ok(());
    }
    for problem in problems.iter().take(50) {
        eprintln!("  {problem}");
    }
    Err(format!("{} differences from {}", problems.len(), output.display()))
}

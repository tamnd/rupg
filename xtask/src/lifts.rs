//! The check of the files that rupg lifts from rudb. See `spec/22-crate-layout.md` section 22.7.
//!
//! A lifted file starts with a provenance header:
//!
//! ```text
//! // Lifted from rudb crates/rudb-encoding/src/bitpack.rs at cd9f9676 (2026-10-05).
//! // Changes: FOR base is i64 for int8.
//! // Reviewed at 1a2b3c4d (2026-11-02): the fix of the bit width is ported.
//! ```
//!
//! The task finds each header in `crates/`, and reads the history of the source file in a rudb checkout. A commit to the source file after the last `Reviewed at` line, or after the lift when there is no such line, is a change that nobody decided on. The task lists these changes and fails. After the decision to port a change or to decline it, add a `Reviewed at` line with the last commit that you read and the decision.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{crate_dirs, read};

/// The provenance header of one lifted file.
#[derive(Debug, PartialEq, Eq)]
struct Lift {
    /// The path of the source file in rudb.
    source: String,
    /// The rudb commit of the lift.
    commit: String,
    /// The rudb commit of the last review, if there is one.
    reviewed: Option<String>,
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let rudb = match args {
        [] => root.join("../rudb"),
        [dir] => PathBuf::from(dir),
        _ => return Err("usage: cargo xtask lifts [rudb checkout]".to_string()),
    };
    let mut lifts = Vec::new();
    let mut problems = Vec::new();
    for dir in crate_dirs(root)? {
        let mut files = Vec::new();
        rust_files(&dir.join("src"), &mut files)?;
        for file in files {
            let shown = file.strip_prefix(root).unwrap_or(&file).display().to_string();
            match header(&read(&file)?) {
                Ok(Some(lift)) => lifts.push((shown, lift)),
                Ok(None) => {}
                Err(problem) => problems.push(format!("{shown}: {problem}")),
            }
        }
    }
    for (file, lift) in &lifts {
        let base = lift.reviewed.as_deref().unwrap_or(&lift.commit);
        if git(&rudb, &["cat-file", "-e", &format!("{base}^{{commit}}")]).is_err() {
            problems.push(format!(
                "{file}: the rudb checkout {} has no commit {base}. A shallow clone has no history, so fetch the full history.",
                rudb.display()
            ));
            continue;
        }
        if git(&rudb, &["cat-file", "-e", &format!("HEAD:{}", lift.source)]).is_err() {
            problems.push(format!("{file}: rudb has no file {} at HEAD", lift.source));
        }
        let range = format!("{base}..HEAD");
        let log = git(&rudb, &["log", "--format=%h %as %s", &range, "--", &lift.source])?;
        for line in log.lines() {
            problems.push(format!("{file}: rudb {} changed: {line}", lift.source));
        }
    }
    if problems.is_empty() {
        println!("lifts: {} lifted files, no rudb change that is not reviewed", lifts.len());
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        Err(format!("lifts: {} problems in {} lifted files", problems.len(), lifts.len()))
    }
}

/// The `.rs` files under a directory, in a fixed order.
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// Runs git in the rudb checkout and gives its output.
fn git(rudb: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(rudb)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            rudb.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The provenance header of a file, or `None` when the file is not lifted.
fn header(text: &str) -> Result<Option<Lift>, String> {
    let mut lift: Option<Lift> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("// Lifted from rudb ") {
            if lift.is_some() {
                return Err("two Lifted from lines".to_string());
            }
            let (source, rest) =
                rest.split_once(" at ").ok_or("the Lifted from line has no commit")?;
            let (commit, date) = commit_and_date(rest)?;
            if !date.ends_with(").") {
                return Err(
                    "the Lifted from line must end with the date and a full stop".to_string()
                );
            }
            lift = Some(Lift { source: source.to_string(), commit, reviewed: None });
        } else if let Some(rest) = line.strip_prefix("// Reviewed at ") {
            let Some(lift) = lift.as_mut() else {
                return Err("a Reviewed at line before the Lifted from line".to_string());
            };
            let (commit, date) = commit_and_date(rest)?;
            if !date.contains("): ") {
                return Err(
                    "the Reviewed at line must give the decision after the date".to_string()
                );
            }
            lift.reviewed = Some(commit);
        }
    }
    Ok(lift)
}

/// The commit at the start of the text, and the rest of the text from the date in brackets.
fn commit_and_date(text: &str) -> Result<(String, &str), String> {
    let (commit, rest) = text.split_once(" (").ok_or("no date in brackets after the commit")?;
    if commit.len() < 7 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{commit:?} is not a commit"));
    }
    Ok((commit.to_string(), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let text = "//! Docs.\n\n// Lifted from rudb crates/a/src/lib.rs at cd9f9676 (2026-10-05).\n// Changes: none.\n";
        assert_eq!(
            header(text).unwrap(),
            Some(Lift {
                source: "crates/a/src/lib.rs".to_string(),
                commit: "cd9f9676".to_string(),
                reviewed: None
            })
        );
        let reviewed = format!(
            "{text}// Reviewed at 1a2b3c4d (2026-11-01): declined.\n// Reviewed at 5e6f7a8b (2026-12-01): the fix is ported.\n"
        );
        assert_eq!(header(&reviewed).unwrap().unwrap().reviewed.as_deref(), Some("5e6f7a8b"));
        assert_eq!(header("fn main() {}\n").unwrap(), None);
        for bad in [
            "// Lifted from rudb crates/a.rs\n",
            "// Lifted from rudb crates/a.rs at main (2026-10-05).\n",
            "// Lifted from rudb crates/a.rs at cd9f9676 (2026-10-05)\n",
            "// Reviewed at cd9f9676 (2026-10-05): declined.\n",
            "// Lifted from rudb a.rs at cd9f9676 (2026-10-05).\n// Reviewed at cd9f9676 (2026-10-05).\n",
            "// Lifted from rudb a.rs at cd9f9676 (2026-10-05).\n// Lifted from rudb b.rs at cd9f9676 (2026-10-05).\n",
        ] {
            assert!(header(bad).is_err(), "{bad}");
        }
    }
}

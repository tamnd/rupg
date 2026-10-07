//! The house rules that rustfmt and clippy do not check.
//!
//! 1. A crate that is not in `xtask/unsafe.toml` has `#![forbid(unsafe_code)]`.
//! 2. In a crate that may contain `unsafe`, each `unsafe` block has a `// SAFETY:` comment directly above it.
//! 3. The Markdown files follow the writing rules of `spec/00-README.md`: no em dash, no en dash, no horizontal rule, and one line for each paragraph.

use std::path::{Path, PathBuf};

use crate::{crate_dirs, name_of, read};

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let allowed = unsafe_crates(&read(&root.join("xtask/unsafe.toml"))?);
    let mut problems = Vec::new();

    for dir in crate_dirs(root)? {
        let name = name_of(&dir);
        let entry = ["src/lib.rs", "src/main.rs"].iter().map(|f| dir.join(f)).find(|p| p.is_file());
        let Some(entry) = entry else {
            problems.push(format!("{name}: no src/lib.rs or src/main.rs"));
            continue;
        };
        let may_be_unsafe = allowed.contains(&name);
        let text = read(&entry)?;
        if !may_be_unsafe && !text.contains("#![forbid(unsafe_code)]") {
            problems.push(format!("{}: missing #![forbid(unsafe_code)]", entry.display()));
        }
        if may_be_unsafe {
            for file in files(&dir.join("src"), "rs") {
                safety_comments(&file, &read(&file)?, &mut problems);
            }
        }
    }

    let mut docs = files(&root.join("spec"), "md");
    for name in ["README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md"] {
        docs.push(root.join(name));
    }
    docs.extend(files(&root.join(".github"), "md"));
    for file in docs.iter().filter(|f| f.is_file()) {
        prose(file, &read(file)?, &mut problems);
    }

    if problems.is_empty() {
        println!("style: {} Markdown files and every crate root pass", docs.len());
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        Err(format!("style: {} problems", problems.len()))
    }
}

/// Reads the `crates = [...]` list of `xtask/unsafe.toml`.
fn unsafe_crates(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"'))
        .map(|l| l.trim_end_matches(',').trim_matches('"').to_string())
        .collect()
}

/// Every file under `dir` with the extension `ext`, sorted.
fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn safety_comments(file: &Path, text: &str, problems: &mut Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let code = line.split("//").next().unwrap_or_default();
        if !code.contains("unsafe {") {
            continue;
        }
        let above = lines[..i].iter().rev().map(|l| l.trim()).take_while(|l| l.starts_with("//"));
        if !above.into_iter().any(|l| l.starts_with("// SAFETY:")) {
            problems.push(format!(
                "{}:{}: unsafe block without a SAFETY comment",
                file.display(),
                i + 1
            ));
        }
    }
}

/// The writing rules for Markdown. Code blocks and tables are skipped where the rule does not apply.
fn prose(file: &Path, text: &str, problems: &mut Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_code = false;
    for (i, line) in lines.iter().enumerate() {
        let at = || format!("{}:{}", file.display(), i + 1);
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if line.contains('\u{2014}') || line.contains('\u{2013}') {
            problems.push(format!("{}: em dash or en dash", at()));
        }
        if in_code {
            continue;
        }
        let t = line.trim();
        if i > 0
            && t.len() >= 3
            && ["-", "*", "_"].iter().any(|c| t.chars().all(|x| x.to_string() == *c))
        {
            problems.push(format!("{}: horizontal rule", at()));
        }
        if wrapped(line, lines.get(i + 1).copied()) {
            problems.push(format!("{}: a paragraph continues on the next line", at()));
        }
    }
}

/// True if `line` is prose and `next` continues the same sentence.
fn wrapped(line: &str, next: Option<&str>) -> bool {
    let Some(next) = next else { return false };
    let t = line.trim_start();
    if t.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    let block = ["#", "|", "-", "*", ">", "<", "!["];
    if block.iter().any(|b| t.starts_with(b))
        || t.split_once(". ").is_some_and(|(n, _)| n.parse::<u32>().is_ok())
    {
        return false;
    }
    next.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_wrapped_paragraph() {
        assert!(wrapped("This sentence is", Some("cut in two.")));
        assert!(!wrapped("One sentence.", Some("")));
        assert!(!wrapped("| a | b |", Some("x")));
        assert!(!wrapped("1. Step one", Some("next")));
    }

    #[test]
    fn finds_a_rule_and_a_dash() {
        let mut p = Vec::new();
        prose(Path::new("x.md"), "# T\n\n---\n\nA \u{2014} b.\n", &mut p);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn finds_unsafe_without_safety() {
        let mut p = Vec::new();
        safety_comments(Path::new("x.rs"), "fn f() {\n    unsafe { g() }\n}\n", &mut p);
        assert_eq!(p.len(), 1);
        p.clear();
        safety_comments(
            Path::new("x.rs"),
            "// SAFETY: g has no precondition.\nunsafe { g() }\n",
            &mut p,
        );
        assert!(p.is_empty());
    }
}

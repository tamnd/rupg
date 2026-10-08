//! The instruction count set of spec/21 section 21.14.
//!
//! The program `crates/rupg/examples/icount.rs` runs one case. This task runs each case under `perf stat -e instructions:u`, once with only the setup of the case and once with the setup and the work. The count of the case is the difference. Each run is done three times and the task takes the median, so one slow run does not change the result. The count is of user space instructions in all the threads of the process.
//!
//! `--save <file>` writes the counts. `--check <file>` compares the counts with a saved file and fails when one case is more than 3 percent higher or the total is more than 1 percent higher. Counts change with the processor, the compiler and the build options, so compare only counts from the same machine and the same toolchain.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The highest increase of one case, in percent.
const CASE_BUDGET: f64 = 3.0;
/// The highest increase of the total, in percent.
const TOTAL_BUDGET: f64 = 1.0;
/// The runs of each mode of each case.
const RUNS: usize = 3;

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let mut binary = None;
    let mut save = None;
    let mut check = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = || -> Result<PathBuf, String> { Err(format!("{arg} needs a value")) };
        match arg.as_str() {
            "--binary" => binary = Some(args.next().map(PathBuf::from).map_or_else(value, Ok)?),
            "--save" => save = Some(args.next().map(PathBuf::from).map_or_else(value, Ok)?),
            "--check" => check = Some(args.next().map(PathBuf::from).map_or_else(value, Ok)?),
            _ => {
                return Err(
                    "usage: cargo xtask bench [--binary <icount>] [--save <file>] [--check <file>]"
                        .to_string(),
                );
            }
        }
    }
    let binary = match binary {
        Some(binary) => binary,
        None => build(root)?,
    };
    let list = output(Command::new(&binary).arg("list"))?;
    let dir = std::env::temp_dir().join(format!("rupg-icount-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let result = measure(&binary, &dir, list.lines());
    let _ = std::fs::remove_dir_all(&dir);
    let counts = result?;
    let total: u64 = counts.iter().map(|(_, count)| count).sum();
    for (case, count) in &counts {
        println!("{case:<10} {count:>14}");
    }
    println!("{:<10} {total:>14}", "total");
    if let Some(file) = save {
        let mut text = String::from(
            "# The instruction counts of `cargo xtask bench`. Compare only with counts from the same machine and the same toolchain.\n",
        );
        for (case, count) in &counts {
            text.push_str(&format!("{case} {count}\n"));
        }
        std::fs::write(&file, text)
            .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    }
    if let Some(file) = check {
        compare(&counts, &crate::read(&file)?)?;
    }
    Ok(())
}

/// Builds the program of the cases and gives its path.
fn build(root: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["build", "--release", "-p", "rupg", "--example", "icount"])
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !status.success() {
        return Err("the build of the icount example failed".to_string());
    }
    Ok(root.join("target/release/examples/icount"))
}

/// The standard output of a command that must succeed.
fn output(command: &mut Command) -> Result<String, String> {
    let out = command.output().map_err(|e| format!("could not run {command:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{command:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The count of each case.
fn measure<'a>(
    binary: &Path,
    dir: &Path,
    cases: impl Iterator<Item = &'a str>,
) -> Result<Vec<(String, u64)>, String> {
    let file = dir.join("icount.rupg");
    let mut counts = Vec::new();
    for case in cases {
        let median = |mode: &str| -> Result<u64, String> {
            let mut runs = Vec::new();
            for _ in 0..RUNS {
                let _ = std::fs::remove_file(&file);
                let mut command = Command::new("perf");
                command
                    .args(["stat", "-x", ",", "-e", "instructions:u", "--"])
                    .arg(binary)
                    .args([case, mode])
                    .arg(&file);
                let out = command.output().map_err(|e| format!("could not run perf: {e}"))?;
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() {
                    return Err(format!("{case} {mode} failed: {}", stderr.trim()));
                }
                runs.push(perf_count(&stderr)?);
            }
            runs.sort_unstable();
            Ok(runs[RUNS / 2])
        };
        let setup = median("setup")?;
        let work = median("run")?;
        counts.push((case.to_string(), work.saturating_sub(setup)));
    }
    let _ = std::fs::remove_file(&file);
    Ok(counts)
}

/// The count in the CSV output of `perf stat -x ,`.
fn perf_count(stderr: &str) -> Result<u64, String> {
    let line = stderr
        .lines()
        .find(|line| line.contains(",instructions"))
        .ok_or_else(|| format!("perf gave no instruction count: {}", stderr.trim()))?;
    let field = line.split(',').next().unwrap_or_default();
    field.parse().map_err(|_| {
        format!("perf cannot count instructions here ({field}). A virtual machine may hide the counters.")
    })
}

/// The counts of a saved file.
fn saved(text: &str) -> Result<Vec<(String, u64)>, String> {
    let mut counts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (case, count) = line.split_once(' ').ok_or_else(|| format!("bad line {line:?}"))?;
        let count = count.trim().parse().map_err(|_| format!("bad count in {line:?}"))?;
        counts.push((case.to_string(), count));
    }
    Ok(counts)
}

/// The change from `old` to `new` in percent.
fn change(old: u64, new: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let (old, new) = (old as f64, new as f64);
    if old == 0.0 { 0.0 } else { (new - old) / old * 100.0 }
}

/// Compares the counts with a saved file.
fn compare(counts: &[(String, u64)], text: &str) -> Result<(), String> {
    let old = saved(text)?;
    let mut problems = Vec::new();
    for (case, count) in counts {
        let Some((_, before)) = old.iter().find(|(name, _)| name == case) else {
            println!("{case}: new case, {count}");
            continue;
        };
        let percent = change(*before, *count);
        println!("{case}: {before} to {count}, {percent:+.2}%");
        if percent > CASE_BUDGET {
            problems.push(format!("{case} is {percent:.2}% higher, more than {CASE_BUDGET}%"));
        }
    }
    for (case, _) in &old {
        if !counts.iter().any(|(name, _)| name == case) {
            problems.push(format!("the case {case} is in the saved file and not in the set"));
        }
    }
    let shared = |list: &[(String, u64)], other: &[(String, u64)]| -> u64 {
        list.iter().filter(|(name, _)| other.iter().any(|(o, _)| o == name)).map(|(_, c)| c).sum()
    };
    let percent = change(shared(&old, counts), shared(counts, &old));
    println!("total: {percent:+.2}%");
    if percent > TOTAL_BUDGET {
        problems.push(format!("the total is {percent:.2}% higher, more than {TOTAL_BUDGET}%"));
    }
    if problems.is_empty() { Ok(()) } else { Err(format!("bench: {}", problems.join("; "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_output() {
        let stderr = "914794959,,instructions:u,2313127,100.00,,\n";
        assert_eq!(perf_count(stderr), Ok(914_794_959));
        assert!(perf_count("<not supported>,,instructions:u,0,100.00,,\n").is_err());
        assert!(perf_count("nothing\n").is_err());
    }

    #[test]
    fn budgets() {
        let text = "# header\ninsert 1000\nget 100\n";
        let same = [("insert".to_string(), 1000), ("get".to_string(), 100)];
        assert!(compare(&same, text).is_ok());
        let case_up = [("insert".to_string(), 1000), ("get".to_string(), 104)];
        assert!(compare(&case_up, text).is_err());
        let total_up = [("insert".to_string(), 1015), ("get".to_string(), 100)];
        assert!(compare(&total_up, text).is_err());
        let lower = [("insert".to_string(), 900), ("get".to_string(), 102)];
        assert!(compare(&lower, text).is_ok());
        let missing = [("insert".to_string(), 1000)];
        assert!(compare(&missing, text).is_err());
        assert!(saved("insert x\n").is_err());
    }
}

//! Compares the automaton with the report that bison writes with `-v`.
//!
//! Run bison with `-Dlr.default-reduction=accepting`. Then the report lists the lookahead tokens of each reduction, and the comparison covers the full action table. The states are matched by their kernel items, because the two programs number the states in different orders.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::lalr::{Action, Automaton, Item};
use super::reader::Grammar;

/// One state of the bison report.
#[derive(Default)]
struct ReportState {
    kernel: BTreeSet<String>,
    /// The action on each terminal, as text: `shift <state>`, `reduce <rule text>`, `error` or `accept`.
    actions: BTreeMap<String, String>,
    rejected: BTreeSet<(String, String)>,
    gotos: BTreeMap<String, u32>,
}

/// Compares and gives the differences. An empty list means that the tables are the same.
pub(crate) fn compare(g: &Grammar, a: &Automaton, report: &str) -> Result<Vec<String>, String> {
    let (rules, states) = parse(report)?;
    let mut problems = Vec::new();
    if states.len() != a.states.len() {
        problems.push(format!(
            "bison has {} states and xtask has {}",
            states.len(),
            a.states.len()
        ));
    }

    let ours: HashMap<BTreeSet<String>, usize> =
        a.states.iter().enumerate().map(|(i, s)| (kernel_text(g, &s.kernel), i)).collect();
    // bison state number to our state number.
    let mut map: HashMap<u32, usize> = HashMap::new();
    for (number, state) in &states {
        match ours.get(&state.kernel) {
            Some(&i) => {
                map.insert(*number, i);
            }
            None => {
                problems.push(format!("bison state {number} has a kernel that xtask does not have"))
            }
        }
    }
    if !problems.is_empty() {
        return Ok(problems);
    }

    for (number, state) in &states {
        let i = map[number];
        let our = &a.states[i];
        let mut our_actions = BTreeMap::new();
        for &(sym, action) in &our.actions {
            our_actions.insert(g.name(sym).to_string(), action_text(g, action));
        }
        let theirs: BTreeMap<String, String> = state
            .actions
            .iter()
            .map(|(token, action)| {
                let action = match action.strip_prefix("shift ") {
                    Some(n) => format!(
                        "shift {}",
                        n.parse::<u32>().ok().and_then(|n| map.get(&n)).map_or(usize::MAX, |&s| s)
                    ),
                    None => match action.strip_prefix("reduce ") {
                        Some(n) => format!(
                            "reduce {}",
                            n.parse::<usize>()
                                .ok()
                                .and_then(|n| rules.get(n))
                                .map_or("?", String::as_str)
                        ),
                        None => action.clone(),
                    },
                };
                (token.clone(), action)
            })
            .collect();
        if theirs != our_actions {
            let tokens: BTreeSet<&String> = theirs.keys().chain(our_actions.keys()).collect();
            for token in tokens {
                let (t, o) = (theirs.get(token), our_actions.get(token));
                if t != o {
                    problems
                        .push(format!("bison state {number} on {token}: bison {t:?}, xtask {o:?}"));
                }
            }
        }
        let our_rejected: BTreeSet<(String, String)> = our
            .rejected
            .iter()
            .map(|&(sym, action)| (g.name(sym).to_string(), action_text(g, action)))
            .collect();
        let their_rejected: BTreeSet<(String, String)> = state
            .rejected
            .iter()
            .map(|(token, n)| {
                (
                    token.clone(),
                    format!(
                        "reduce {}",
                        n.parse::<usize>()
                            .ok()
                            .and_then(|n| rules.get(n))
                            .map_or("?", String::as_str)
                    ),
                )
            })
            .collect();
        if our_rejected != their_rejected {
            problems.push(format!("bison state {number}: the rejected reductions differ"));
        }
        for (nt, target) in &state.gotos {
            let ours =
                our.transitions.iter().find(|&&(s, _)| g.name(s) == nt).map(|&(_, t)| t as usize);
            if ours != map.get(target).copied() {
                problems.push(format!("bison state {number}: the goto on {nt} differs"));
            }
        }
    }
    Ok(problems)
}

fn action_text(g: &Grammar, action: Action) -> String {
    match action {
        Action::Shift(s) => format!("shift {s}"),
        Action::Reduce(r) => format!("reduce {}", rule_text(g, r as usize)),
        Action::Error => "error".to_string(),
        Action::Accept => "accept".to_string(),
    }
}

fn rule_text(g: &Grammar, r: usize) -> String {
    let rule = &g.rules[r];
    let rhs: Vec<&str> = rule.rhs.iter().map(|&s| g.name(s)).collect();
    format!("{}: {}", g.name(rule.lhs), rhs.join(" ")).trim_end().to_string()
}

fn kernel_text(g: &Grammar, kernel: &[Item]) -> BTreeSet<String> {
    kernel
        .iter()
        .map(|item| {
            let rule = &g.rules[item.rule as usize];
            let mut parts: Vec<&str> = rule.rhs.iter().map(|&s| g.name(s)).collect();
            parts.insert(item.dot as usize, "•");
            format!("{}: {}", g.name(rule.lhs), parts.join(" "))
        })
        .collect()
}

/// Splits `lhs: a b • c` or `| a b • c` into the left side, if given, and the normalized right side.
fn split_rule_line(text: &str) -> (Option<&str>, String) {
    let (lhs, rhs) = match text.strip_prefix("| ") {
        Some(rhs) => (None, rhs),
        None => match text.split_once(": ") {
            Some((lhs, rhs)) => (Some(lhs), rhs),
            None => (text.strip_suffix(':'), ""),
        },
    };
    // bison writes an empty right side as ε.
    let rhs: Vec<&str> = rhs.split_whitespace().filter(|&s| s != "ε" && s != "%empty").collect();
    (lhs, rhs.join(" "))
}

type Parsed = (Vec<String>, BTreeMap<u32, ReportState>);

fn parse(report: &str) -> Result<Parsed, String> {
    let mut rules: Vec<String> = Vec::new();
    let mut states: BTreeMap<u32, ReportState> = BTreeMap::new();
    let mut section = "";
    let mut current: Option<u32> = None;
    let mut lhs = String::new();
    for line in report.lines() {
        // bison names a mid-rule action `@N` when the action gives a value, and `$@N` when it does not. The reader always uses `$@N`. The number is the same.
        let line = line.replace(" @", " $@");
        let line = line.as_str();
        if !line.starts_with(' ') && !line.is_empty() {
            if line == "Grammar" {
                section = "grammar";
            } else if let Some(n) = line.strip_prefix("State ").and_then(|n| n.parse::<u32>().ok())
            {
                section = "state";
                current = Some(n);
                states.insert(n, ReportState::default());
            } else {
                section = "";
            }
            continue;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match section {
            "grammar" => {
                let (number, rest) =
                    text.split_once(' ').ok_or("a grammar line has no rule number")?;
                let number: usize =
                    number.parse().map_err(|_| format!("bad rule number in {text:?}"))?;
                let (left, rhs) = split_rule_line(rest.trim_start());
                if let Some(left) = left {
                    lhs = left.to_string();
                }
                if number != rules.len() {
                    return Err(format!("the rules of the report are not in order at {number}"));
                }
                rules.push(format!("{lhs}: {rhs}").trim_end().to_string());
            }
            "state" => {
                let state = states
                    .get_mut(&current.expect("a state line follows a State header"))
                    .expect("inserted");
                let mut fields = text.split_whitespace();
                let first = fields.next().unwrap_or_default();
                if first.parse::<u32>().is_ok() && text.contains('•') {
                    let rest = text[first.len()..].trim_start();
                    let (left, rhs) = split_rule_line(rest);
                    if let Some(left) = left {
                        lhs = left.to_string();
                    }
                    state.kernel.insert(format!("{lhs}: {rhs}"));
                    continue;
                }
                if text.starts_with("Conflict between") {
                    continue;
                }
                let rest = text[first.len()..].trim_start();
                let token = first.to_string();
                if let Some(n) = rest.strip_prefix("shift, and go to state ") {
                    state.actions.insert(token, format!("shift {n}"));
                } else if let Some(n) = rest.strip_prefix("go to state ") {
                    state
                        .gotos
                        .insert(token, n.parse().map_err(|_| format!("bad state in {text:?}"))?);
                } else if let Some(r) = rest.strip_prefix("reduce using rule ") {
                    if token == "$default" {
                        return Err("the report has a default reduction. Run bison with -Dlr.default-reduction=accepting".to_string());
                    }
                    let n = r.split_whitespace().next().unwrap_or_default();
                    state.actions.insert(token, format!("reduce {n}"));
                } else if let Some(r) = rest.strip_prefix("[reduce using rule ") {
                    let n = r.split_whitespace().next().unwrap_or_default();
                    state.rejected.insert((token, n.to_string()));
                } else if rest.starts_with("error") {
                    state.actions.insert(token, "error".to_string());
                } else if rest == "accept" {
                    // bison writes `$default accept`. The accept action is on $end.
                    state.actions.insert("$end".to_string(), "accept".to_string());
                } else {
                    return Err(format!("a report line that xtask does not know: {text:?}"));
                }
            }
            _ => {}
        }
    }
    if rules.is_empty() || states.is_empty() {
        return Err("the file is not a bison report".to_string());
    }
    Ok((rules, states))
}

#[cfg(test)]
mod tests {
    use super::super::{lalr, reader};
    use super::compare;

    // The reports were made by bison 3.8.2 with `bison -Dlr.default-reduction=accepting -v`.
    const FIXTURES: [(&str, &str, (usize, usize)); 3] = [
        (include_str!("testdata/expr.y"), include_str!("testdata/expr.output"), (9, 2)),
        (include_str!("testdata/lr1.y"), include_str!("testdata/lr1.output"), (0, 2)),
        (include_str!("testdata/midrule.y"), include_str!("testdata/midrule.output"), (1, 0)),
    ];

    #[test]
    fn the_tables_are_the_same_as_in_bison() {
        for (grammar, output, conflicts) in FIXTURES {
            let g = reader::read(grammar).unwrap();
            let a = lalr::build(&g);
            assert_eq!((a.sr_conflicts(), a.rr_conflicts()), conflicts);
            assert_eq!(compare(&g, &a, output).unwrap(), Vec::<String>::new());
        }
    }

    #[test]
    fn a_changed_report_gives_differences() {
        let (grammar, output, _) = FIXTURES[0];
        let g = reader::read(grammar).unwrap();
        let a = lalr::build(&g);
        // Remove one action line. Then the action table of that state is different.
        let line = output.lines().find(|l| l.contains("shift, and go to state")).unwrap();
        let changed = output.replacen(&format!("{line}\n"), "", 1);
        assert!(!compare(&g, &a, &changed).unwrap().is_empty());
        // A default reduction hides the lookahead tokens, so the comparison refuses it.
        let changed =
            output.replacen("    $end  reduce using rule", "    $default  reduce using rule", 1);
        assert!(compare(&g, &a, &changed).is_err());
    }
}

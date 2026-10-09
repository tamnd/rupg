//! The wait events of PostgreSQL, from `wait_event_names.txt`.
//!
//! `cargo xtask waitevents [--check]` writes `crates/rupg-func/src/generated/wait_events.rs`, the table that `pg_get_wait_events` gives. The code is a port of the part of `generate-wait_event_types.pl` that writes `wait_event_funcs_data.c`, so the order, the names and the descriptions are those of PostgreSQL: the classes in the order of their names, the events of a class in the order of their names, then the events of its `ABI_compatibility` part in the order of the file.

use std::fmt::Write as _;
use std::path::Path;

use crate::read;

const INPUT: &str = "vendor/postgres-19/src/backend/utils/activity/wait_event_names.txt";
const OUTPUT: &str = "crates/rupg-func/src/generated/wait_events.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("waitevents: unknown argument {other:?}"));
    }
    let text = wait_events(&read(&root.join(INPUT))?)?;
    let file = root.join(OUTPUT);
    if std::fs::read_to_string(&file).unwrap_or_default() == text {
        println!("waitevents: {OUTPUT} matches {INPUT}");
        return Ok(());
    }
    if check {
        return Err(format!(
            "waitevents: {OUTPUT} is not up to date, run `cargo xtask waitevents`"
        ));
    }
    std::fs::write(&file, text).map_err(|e| format!("could not write {OUTPUT}: {e}"))?;
    println!("waitevents: wrote {OUTPUT}");
    Ok(())
}

/// One event line of `wait_event_names.txt`.
struct Event<'a> {
    class: &'a str,
    name: &'a str,
    sentence: &'a str,
}

/// Makes the table of the wait events from the text of `wait_event_names.txt`.
fn wait_events(text: &str) -> Result<String, String> {
    let mut sorted = Vec::new();
    let mut kept = Vec::new();
    let mut class = None;
    let mut abi = false;
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(section) = line.strip_prefix("Section: ClassName") {
            let name = section.rsplit_once("- ").map_or(section, |(_, name)| name);
            class = Some(name);
            abi = false;
            continue;
        }
        if line == "ABI_compatibility:" {
            abi = true;
            continue;
        }
        let bad = || format!("wait_event_names.txt line {}: not an event line: {line}", number + 1);
        let class = class.ok_or_else(bad)?;
        let (name, sentence) = line.split_once('\t').ok_or_else(bad)?;
        let sentence = sentence.trim_start_matches('\t');
        let word =
            |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !word(class) || !word(name) || !sentence.starts_with('"') || !sentence.ends_with(".\"") {
            return Err(bad());
        }
        let event = Event { class, name, sentence };
        if abi { kept.push(event) } else { sorted.push(event) }
    }
    sorted.sort_by_key(|event| event.name.to_ascii_uppercase());
    sorted.extend(kept);

    let mut classes: Vec<&str> = Vec::new();
    for event in &sorted {
        if !classes.contains(&event.class) {
            classes.push(event.class);
        }
    }
    classes.sort_by_key(|class| class.to_ascii_uppercase());

    let mut out = String::from(
        "//! The wait events of PostgreSQL, one row of `pg_wait_events` for each event line of `wait_event_names.txt`: the type, the name and the description.\n//!\n//! `cargo xtask waitevents` makes this file from `vendor/postgres-19/src/backend/utils/activity/wait_event_names.txt`. Do not edit it.\n\npub(crate) const WAIT_EVENTS: &[(&str, &str, &str)] = &[\n",
    );
    for class in classes {
        let kind = class.strip_prefix("WaitEvent").unwrap_or(class);
        for event in sorted.iter().filter(|event| event.class == class) {
            let name = if matches!(class, "WaitEventLWLock" | "WaitEventLock") {
                event.name.to_string()
            } else {
                camel_case(event.name)
            };
            let text = description(event.sentence);
            writeln!(out, "    ({kind:?}, {name:?}, {text:?}),").map_err(|e| e.to_string())?;
        }
    }
    out.push_str("];\n");
    Ok(out)
}

/// The name of an event in camel case: `CLIENT_READ` is `ClientRead`.
fn camel_case(name: &str) -> String {
    let mut out = String::new();
    for part in name.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first);
            out.extend(chars.map(|c| c.to_ascii_lowercase()));
        }
    }
    out
}

/// The description of an event from its sentence in the file, as the Perl script makes it: no quotes and no last period, `<quote>` as quotes, no SGML tags, the name of a parameter for a link to it, and no text from `; see`.
fn description(sentence: &str) -> String {
    let mut text = sentence[1..sentence.len() - 2].to_string();
    text = quotes(&text);
    text = untagged(&text);
    text = parameters(&text);
    if let Some(at) = text.find("; see") {
        text.truncate(at);
    }
    text
}

/// `s/<quote>(.*?)<\/quote>/"$1"/g`.
fn quotes(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<quote>") {
        let inner = &rest[start + "<quote>".len()..];
        let Some(end) = inner.find("</quote>") else { break };
        out.push_str(&rest[..start]);
        out.push('"');
        out.push_str(&inner[..end]);
        out.push('"');
        rest = &inner[end + "</quote>".len()..];
    }
    out.push_str(rest);
    out
}

/// `s/<.*?>(.*?)<.*?>/$1/g`: a tag, the text, and the next tag, which becomes the text.
fn untagged(text: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < text.len() {
        let tag = text[at..].find('<').map(|i| at + i);
        let Some(open) = tag else { break };
        let matched = text[open..].find('>').map(|i| open + i).and_then(|close| {
            let next = text[close + 1..].find('<').map(|i| close + 1 + i)?;
            let end = text[next..].find('>').map(|i| next + i)?;
            Some((close, next, end))
        });
        match matched {
            Some((close, next, end)) => {
                out.push_str(&text[at..open]);
                out.push_str(&text[close + 1..next]);
                at = end + 1;
            }
            None => {
                out.push_str(&text[at..=open]);
                at = open + 1;
            }
        }
    }
    out.push_str(&text[at.min(text.len())..]);
    out
}

/// The loop over `<xref linkend="guc-(.*?)"/>`: each link to a parameter becomes the name of the first parameter, with `_` for `-`.
fn parameters(text: &str) -> String {
    const OPEN: &str = "<xref linkend=\"guc-";
    const CLOSE: &str = "\"/>";
    let Some(start) = text.find(OPEN) else { return text.to_string() };
    let Some(len) = text[start + OPEN.len()..].find(CLOSE) else { return text.to_string() };
    let name = text[start + OPEN.len()..start + OPEN.len() + len].replace('-', "_");
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let inner = &rest[start + OPEN.len()..];
        let Some(end) = inner.find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        out.push_str(&name);
        rest = &inner[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        assert_eq!(
            description("\"Waiting for <xref linkend=\"guc-archive-command\"/> to complete.\""),
            "Waiting for archive_command to complete"
        );
        assert_eq!(
            description(
                "\"Waiting to update <structname>pg_database</structname>.<structfield>datfrozenxid</structfield>.\""
            ),
            "Waiting to update pg_database.datfrozenxid"
        );
        assert_eq!(
            description(
                "\"Waiting to read or update information about <quote>heavyweight</quote> locks.\""
            ),
            "Waiting to read or update information about \"heavyweight\" locks"
        );
        assert_eq!(
            description(
                "\"Waiting to acquire a virtual transaction ID lock; see <xref linkend=\"transaction-id\"/>.\""
            ),
            "Waiting to acquire a virtual transaction ID lock"
        );
        assert_eq!(camel_case("CLIENT_READ"), "ClientRead");
    }
}

//! The vendored files.
//!
//! Each directory under `vendor/` has a `PIN` file. The `PIN` file is the manifest: it names the source, the revision, the URL of each file, the license, and the SHA-256 of each file. See `spec/22-crate-layout.md` section 22.8.
//!
//! `cargo xtask vendor` fetches each file with `curl` and writes the hashes into `PIN`. To move a pin, change `rev` and run it. To add a file, add a line with `-` as the hash and run it.
//!
//! `cargo xtask vendor --check` needs no network. It fails if a file is missing, if a hash is different, if a file is not in `PIN`, or if a license is not in the allowed list.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{name_of, read, sha256};

/// The licenses that a vendored file may have. The names are SPDX identifiers.
const ALLOWED_LICENSES: &[&str] = &["PostgreSQL", "Unicode-3.0", "LicenseRef-tz-public-domain"];

/// One parsed `PIN` file.
#[derive(Clone, Debug, PartialEq)]
struct Pin {
    /// The comment lines at the top, kept as written.
    header: Vec<String>,
    source: String,
    rev: String,
    /// The URL of a file. `{rev}` and `{path}` are replaced.
    url: String,
    license: String,
    license_file: String,
    /// The URL of the license file, if it is not at `url`.
    license_url: Option<String>,
    /// The files, as (hash, path). The hash is `None` before the first fetch.
    files: Vec<(Option<String>, String)>,
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    let only: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let mut dirs = pin_dirs(&root.join("vendor"))?;
    if !only.is_empty() {
        dirs.retain(|d| only.iter().any(|o| **o == name_of(d)));
        if dirs.len() != only.len() {
            return Err("vendor: a named directory has no PIN file".to_string());
        }
    }
    let mut problems = Vec::new();
    let mut files = 0usize;
    for dir in &dirs {
        let text = read(&dir.join("PIN"))?;
        let pin = parse(&text).map_err(|e| format!("{}/PIN: {e}", dir.display()))?;
        if !check {
            let fetched = fetch(dir, &pin)?;
            std::fs::write(dir.join("PIN"), format_pin(&fetched))
                .map_err(|e| format!("could not write {}/PIN: {e}", dir.display()))?;
        }
        let text = read(&dir.join("PIN"))?;
        let pin = parse(&text).map_err(|e| format!("{}/PIN: {e}", dir.display()))?;
        files += pin.files.len();
        for problem in verify(dir, &pin)? {
            problems.push(format!("{}: {problem}", name_of(dir)));
        }
    }
    if problems.is_empty() {
        let plural = if dirs.len() == 1 { "directory" } else { "directories" };
        println!("vendor: {files} files in {} {plural} match their PIN files", dirs.len());
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        Err(format!("vendor: {} problems", problems.len()))
    }
}

/// The directories under `vendor/` that have a `PIN` file, sorted by name.
fn pin_dirs(vendor: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(vendor)
        .map_err(|e| format!("could not read {}: {e}", vendor.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("PIN").is_file())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn parse(text: &str) -> Result<Pin, String> {
    let mut header = Vec::new();
    let (mut source, mut rev, mut url, mut license) = (None, None, None, None);
    let mut files = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let at = |message: &str| format!("line {}: {message}", number + 1);
        if line.starts_with('#') {
            if source.is_none() && files.is_empty() {
                header.push(line.to_string());
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some((hash, path)) = line.split_once("  ") {
            let hash = match hash {
                "-" => None,
                h if h.len() == 64
                    && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) =>
                {
                    Some(h.to_string())
                }
                _ => return Err(at("a file line is a SHA-256 or -, two spaces, and a path")),
            };
            if path.starts_with('/') || path.split('/').any(|part| part == ".." || part.is_empty())
            {
                return Err(at("a path must be relative and must not contain .."));
            }
            files.push((hash, path.to_string()));
            continue;
        }
        let (key, value) = line.split_once(' ').ok_or_else(|| at("expected a key and a value"))?;
        let slot = match key {
            "source" => &mut source,
            "rev" => &mut rev,
            "url" => &mut url,
            "license" => &mut license,
            _ => return Err(at(&format!("unknown key {key:?}"))),
        };
        if slot.replace(value.to_string()).is_some() {
            return Err(at(&format!("{key} is given two times")));
        }
    }
    let missing = |key: &str| format!("{key} is missing");
    let license = license.ok_or_else(|| missing("license"))?;
    let mut parts = license.split(' ');
    let (Some(spdx), Some(license_file)) = (parts.next(), parts.next()) else {
        return Err("license is an SPDX identifier, a file and an optional URL".to_string());
    };
    let license_url = parts.next().map(str::to_string);
    if parts.next().is_some() {
        return Err("license has too many fields".to_string());
    }
    let mut seen = BTreeSet::new();
    for (_, path) in &files {
        if !seen.insert(path) {
            return Err(format!("{path} is listed two times"));
        }
    }
    Ok(Pin {
        header,
        source: source.ok_or_else(|| missing("source"))?,
        rev: rev.ok_or_else(|| missing("rev"))?,
        url: url.ok_or_else(|| missing("url"))?,
        license: spdx.to_string(),
        license_file: license_file.to_string(),
        license_url,
        files,
    })
}

fn format_pin(pin: &Pin) -> String {
    let mut out = String::new();
    for line in &pin.header {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("source {}\nrev {}\nurl {}\n", pin.source, pin.rev, pin.url));
    out.push_str(&format!("license {} {}", pin.license, pin.license_file));
    if let Some(url) = &pin.license_url {
        out.push_str(&format!(" {url}"));
    }
    out.push_str("\n\n");
    for (hash, path) in &pin.files {
        out.push_str(&format!("{}  {path}\n", hash.as_deref().unwrap_or("-")));
    }
    out
}

/// Fetches each file of `pin` into `dir`, removes the files that `pin` does not list, and gives the pin with the new hashes.
fn fetch(dir: &Path, pin: &Pin) -> Result<Pin, String> {
    let mut files = Vec::new();
    for (old, path) in &pin.files {
        let url = if *path == pin.license_file {
            pin.license_url.clone().unwrap_or_else(|| file_url(pin, path))
        } else {
            file_url(pin, path)
        };
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let status = Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location", "--retry", "3", "--output"])
            .arg(&target)
            .arg(&url)
            .status()
            .map_err(|e| format!("could not run curl: {e}"))?;
        if !status.success() {
            return Err(format!("curl could not fetch {url}"));
        }
        let bytes = std::fs::read(&target)
            .map_err(|e| format!("could not read {}: {e}", target.display()))?;
        let hash = sha256::hex(&bytes);
        match old {
            Some(old) if *old != hash => println!("  changed {}/{path}", name_of(dir)),
            None => println!("  added {}/{path}", name_of(dir)),
            Some(_) => {}
        }
        files.push((Some(hash), path.clone()));
    }
    let listed: BTreeSet<&str> = pin.files.iter().map(|(_, p)| p.as_str()).collect();
    for extra in files_under(dir)? {
        if !listed.contains(extra.as_str()) {
            std::fs::remove_file(dir.join(&extra))
                .map_err(|e| format!("could not remove {extra}: {e}"))?;
            println!("  removed {}/{extra}", name_of(dir));
        }
    }
    Ok(Pin { files, ..pin.clone() })
}

fn file_url(pin: &Pin, path: &str) -> String {
    pin.url.replace("{rev}", &pin.rev).replace("{path}", path)
}

/// Checks the files in `dir` against `pin` and gives the problems.
fn verify(dir: &Path, pin: &Pin) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    if !ALLOWED_LICENSES.contains(&pin.license.as_str()) {
        problems.push(format!(
            "license {} is not in the allowed list of xtask/src/vendor.rs",
            pin.license
        ));
    }
    if !pin.files.iter().any(|(_, p)| *p == pin.license_file) {
        problems.push(format!("the license file {} is not a listed file", pin.license_file));
    }
    for (hash, path) in &pin.files {
        let Some(hash) = hash else {
            problems.push(format!("{path} has no hash. Run cargo xtask vendor"));
            continue;
        };
        match std::fs::read(dir.join(path)) {
            Ok(bytes) if sha256::hex(&bytes) == *hash => {}
            Ok(_) => problems.push(format!("{path} does not match its hash")),
            Err(_) => problems.push(format!("{path} is missing")),
        }
    }
    let listed: BTreeSet<&str> = pin.files.iter().map(|(_, p)| p.as_str()).collect();
    for extra in files_under(dir)? {
        if !listed.contains(extra.as_str()) {
            problems.push(format!("{extra} is not in PIN"));
        }
    }
    Ok(problems)
}

/// The files under `dir` except `PIN`, as paths relative to `dir` with `/` as the separator.
fn files_under(dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = std::fs::read_dir(&next)
            .map_err(|e| format!("could not read {}: {e}", next.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path != dir.join("PIN") {
                let relative = path.strip_prefix(dir).expect("the walk stays under dir");
                let parts: Vec<String> = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{format_pin, parse};

    const PIN: &str = "# A comment.\nsource https://example.org/repo\nrev abc\nurl https://example.org/{rev}/{path}\nlicense PostgreSQL COPYRIGHT\n\n-  COPYRIGHT\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  src/a.y\n";

    const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn parse_and_format_round_trip() {
        let pin = parse(PIN).unwrap();
        assert_eq!(pin.rev, "abc");
        assert_eq!(pin.files.len(), 2);
        assert_eq!(pin.files[0].0, None);
        assert_eq!(format_pin(&pin), PIN);
    }

    #[test]
    fn license_url_is_kept() {
        let text = PIN.replace("COPYRIGHT\n\n", "COPYRIGHT https://example.org/LICENSE\n\n");
        let pin = parse(&text).unwrap();
        assert_eq!(pin.license_url.as_deref(), Some("https://example.org/LICENSE"));
        assert_eq!(format_pin(&pin), text);
    }

    #[test]
    fn bad_lines_are_rejected() {
        assert!(parse(&PIN.replace("rev abc", "rev abc\nrev def")).is_err());
        assert!(parse(&PIN.replace("src/a.y", "../a.y")).is_err());
        assert!(parse(&PIN.replace("src/a.y", "/a.y")).is_err());
        assert!(parse(&PIN.replace(HASH, &HASH.to_uppercase())).is_err());
        assert!(parse(&PIN.replace("-  COPYRIGHT", "-  src/a.y")).is_err());
        assert!(parse(&PIN.replace("source https://example.org/repo\n", "")).is_err());
        assert!(parse(&PIN.replace("rev abc", "branch abc")).is_err());
    }
}

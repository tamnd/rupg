//! The versions of an extension and the paths between them: `get_ext_ver_list`, `find_update_path` and `find_install_path` of `extension.c`.

use rupg_common::{Error, Result, SqlState};

/// A version, `ExtensionVersionInfo` of `extension.c`.
#[derive(Clone, Debug)]
struct Vertex {
    name: String,
    /// The versions that an update script `name--from--to.sql` reaches from this one, as indexes.
    reachable: Vec<usize>,
    /// True when an install script `name--version.sql` makes this version.
    installable: bool,
}

/// The graph of the versions of an extension, from the names of its scripts.
#[derive(Clone, Debug, Default)]
pub struct Versions {
    list: Vec<Vertex>,
}

impl Versions {
    /// `get_ext_ver_list`: the versions of the extension `name` from the names of the files of its script directory, in the order of the directory. A name that is not `name--version.sql` or `name--from--to.sql` does not count.
    pub fn new<'f>(name: &str, files: impl IntoIterator<Item = &'f str>) -> Versions {
        let mut versions = Versions::default();
        for file in files {
            let Some(stem) = file.strip_suffix(".sql") else { continue };
            let Some(rest) = stem.strip_prefix(name).and_then(|rest| rest.strip_prefix("--"))
            else {
                continue;
            };
            match rest.split_once("--") {
                None => {
                    let at = versions.vertex(rest);
                    versions.list[at].installable = true;
                }
                Some((from, to)) => {
                    // A file with a third `--` is not a script.
                    if to.contains("--") {
                        continue;
                    }
                    let from = versions.vertex(from);
                    let to = versions.vertex(to);
                    versions.list[from].reachable.push(to);
                }
            }
        }
        versions
    }

    /// `get_ext_ver_info`: the index of the version `name`, which this adds when it is not in the list.
    fn vertex(&mut self, name: &str) -> usize {
        if let Some(at) = self.list.iter().position(|v| v.name == name) {
            return at;
        }
        self.list.push(Vertex {
            name: name.to_string(),
            reachable: Vec::new(),
            installable: false,
        });
        self.list.len() - 1
    }

    /// The versions that an install script makes, in the order of the list.
    pub fn installable(&self) -> impl Iterator<Item = &str> {
        self.list.iter().filter(|v| v.installable).map(|v| v.name.as_str())
    }

    /// The versions that no install script makes, in the order of the list.
    pub fn indirect(&self) -> impl Iterator<Item = &str> {
        self.list.iter().filter(|v| !v.installable).map(|v| v.name.as_str())
    }

    /// True when an install script makes the version `name`.
    pub fn is_installable(&self, name: &str) -> bool {
        self.list.iter().any(|v| v.installable && v.name == name)
    }

    /// `find_update_path`: the shortest list of update steps from `start` to `target`, by Dijkstra's algorithm, without `start` and with `target`, or `None` when no path goes there. With `reject_indirect`, the path does not go through a version that an install script makes. Of two paths of the same length, the one through the predecessor with the smaller name wins.
    fn update_path(
        &self,
        start: usize,
        target: usize,
        reject_indirect: bool,
    ) -> Option<Vec<String>> {
        debug_assert_ne!(start, target);
        let n = self.list.len();
        let mut distance = vec![usize::MAX; n];
        let mut known = vec![false; n];
        let mut previous: Vec<Option<usize>> = vec![None; n];
        distance[start] = 0;
        loop {
            // `get_nearest_unprocessed_vertex`: the first one of the smallest distance.
            let mut nearest: Option<usize> = None;
            for at in 0..n {
                if known[at] {
                    continue;
                }
                if nearest.is_none_or(|best| distance[best] > distance[at]) {
                    nearest = Some(at);
                }
            }
            let Some(at) = nearest else { break };
            if distance[at] == usize::MAX {
                break;
            }
            known[at] = true;
            if at == target {
                break;
            }
            for &next in &self.list[at].reachable {
                if reject_indirect && self.list[next].installable {
                    continue;
                }
                let new = distance[at] + 1;
                if new < distance[next] {
                    distance[next] = new;
                    previous[next] = Some(at);
                } else if new == distance[next]
                    && previous[next].is_some_and(|p| self.list[at].name < self.list[p].name)
                {
                    previous[next] = Some(at);
                }
            }
        }
        if !known[target] {
            return None;
        }
        let mut path = Vec::new();
        let mut at = target;
        while at != start {
            path.push(self.list[at].name.clone());
            at = previous[at]?;
        }
        path.reverse();
        Some(path)
    }

    /// `identify_update_path`: the update steps from the version `from` to the version `to` of the extension `name`.
    ///
    /// # Errors
    ///
    /// `22023` when no update path goes from `from` to `to`.
    pub fn identify_update_path(&self, name: &str, from: &str, to: &str) -> Result<Vec<String>> {
        let mut graph = self.clone();
        let start = graph.vertex(from);
        let target = graph.vertex(to);
        let path = if start == target { None } else { graph.update_path(start, target, false) };
        path.ok_or_else(|| {
            Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "extension \"{name}\" has no update path from version \"{from}\" to version \"{to}\""
                ),
            )
        })
    }

    /// `find_install_path`: the version to install first to reach `target`, and the update steps after it, or `None` when no path reaches `target`. A version that an install script makes needs no step. Else the shortest path wins, and of two paths of the same length, the one from the start with the greater name.
    pub fn install_path(&self, target: &str) -> Option<(String, Vec<String>)> {
        let mut graph = self.clone();
        let target = graph.vertex(target);
        if graph.list[target].installable {
            return Some((graph.list[target].name.clone(), Vec::new()));
        }
        let mut best: Option<(usize, Vec<String>)> = None;
        for start in 0..graph.list.len() {
            if !graph.list[start].installable {
                continue;
            }
            let Some(path) = graph.update_path(start, target, true) else { continue };
            let better = match &best {
                None => true,
                Some((at, best_path)) => {
                    path.len() < best_path.len()
                        || (path.len() == best_path.len()
                            && graph.list[*at].name < graph.list[start].name)
                }
            };
            if better {
                best = Some((start, path));
            }
        }
        best.map(|(start, path)| (graph.list[start].name.clone(), path))
    }
}

/// `check_valid_extension_name`.
///
/// # Errors
///
/// `22023` for an empty name, a name with `--` or a directory separator, or a name that begins or ends with `-`.
pub fn check_extension_name(name: &str) -> Result<()> {
    check_name(name, "invalid extension name", "Extension names")
}

/// `check_valid_version_name`.
///
/// # Errors
///
/// As [`check_extension_name`], for a version.
pub fn check_version_name(name: &str) -> Result<()> {
    check_name(name, "invalid extension version name", "Version names")
}

fn check_name(name: &str, what: &str, names: &str) -> Result<()> {
    let detail = if name.is_empty() {
        "must not be empty."
    } else if name.contains("--") {
        "must not contain \"--\"."
    } else if name.starts_with('-') || name.ends_with('-') {
        "must not begin or end with \"-\"."
    } else if name.contains('/') {
        "must not contain directory separator characters."
    } else {
        return Ok(());
    };
    Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("{what}: \"{name}\""))
        .with_detail(format!("{names} {detail}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRGM: &[&str] = &[
        "pg_trgm--1.3.sql",
        "pg_trgm--1.0--1.1.sql",
        "pg_trgm--1.1--1.2.sql",
        "pg_trgm--1.2--1.3.sql",
        "pg_trgm--1.3--1.4.sql",
        "pg_trgm--1.4--1.5.sql",
        "pg_trgm--1.5--1.6.sql",
        "pg_trgm.control",
    ];

    #[test]
    fn install() {
        let versions = Versions::new("pg_trgm", TRGM.iter().copied());
        let (start, path) = versions.install_path("1.6").unwrap();
        assert_eq!(start, "1.3");
        assert_eq!(path, ["1.4", "1.5", "1.6"]);
        assert_eq!(versions.install_path("1.3").unwrap(), ("1.3".to_string(), Vec::new()));
        // 1.1 is reachable only through versions before the install script.
        assert!(versions.install_path("1.1").is_none());
        assert!(versions.install_path("9.9").is_none());
    }

    #[test]
    fn update() {
        let versions = Versions::new("pg_trgm", TRGM.iter().copied());
        assert_eq!(
            versions.identify_update_path("pg_trgm", "1.0", "1.4").unwrap(),
            ["1.1", "1.2", "1.3", "1.4"]
        );
        let err = versions.identify_update_path("pg_trgm", "1.6", "1.3").unwrap_err();
        assert_eq!(
            err.message(),
            "extension \"pg_trgm\" has no update path from version \"1.6\" to version \"1.3\""
        );
    }

    #[test]
    fn ties() {
        // Two install scripts reach 3 in one step: the start with the greater name wins.
        let files = ["e--1.sql", "e--2.sql", "e--1--3.sql", "e--2--3.sql", "e--1--2--3.sql"];
        let versions = Versions::new("e", files);
        assert_eq!(versions.install_path("3").unwrap(), ("2".to_string(), vec!["3".to_string()]));
        // Two paths of two steps: the one through the smaller predecessor wins.
        let files = ["e--1.sql", "e--1--b.sql", "e--1--a.sql", "e--b--z.sql", "e--a--z.sql"];
        let versions = Versions::new("e", files);
        assert_eq!(versions.identify_update_path("e", "1", "z").unwrap(), ["a", "z"]);
    }

    #[test]
    fn names() {
        assert!(check_extension_name("pg_trgm").is_ok());
        let err = check_extension_name("a--b").unwrap_err();
        assert_eq!(err.message(), "invalid extension name: \"a--b\"");
        assert_eq!(err.detail(), Some("Extension names must not contain \"--\"."));
        let err = check_version_name("-1").unwrap_err();
        assert_eq!(err.detail(), Some("Version names must not begin or end with \"-\"."));
        assert_eq!(
            check_version_name("").unwrap_err().detail(),
            Some("Version names must not be empty.")
        );
    }
}

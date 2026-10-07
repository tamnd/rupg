//! The Rust API, the facade crate that a user adds to `Cargo.toml`.
//!
//! This crate first ships in milestone M1. Until then it gives only the version and the build configuration. See `spec/17-embedding.md` and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

pub use rupg_common::{CompatVersion, POSTGRES_COMMIT, POSTGRES_PIN};

/// The version of rupg. It is 0.M.patch, where M is the last milestone that closed.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build configuration, as `key: value` lines.
///
/// The binary prints this for `rupg --print-config`. CI runs it on each host to show that the binary builds and runs.
pub fn build_config() -> String {
    let mut out = String::new();
    let mut line = |key: &str, value: &str| {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    };
    line("rupg", VERSION);
    line("postgres", POSTGRES_PIN);
    line("postgres commit", POSTGRES_COMMIT);
    let shims: Vec<String> = CompatVersion::ALL.iter().map(ToString::to_string).collect();
    line("compat versions", &shims.join(" "));
    line("os", std::env::consts::OS);
    line("arch", std::env::consts::ARCH);
    line("debug assertions", if cfg!(debug_assertions) { "on" } else { "off" });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_names_the_pin() {
        let config = build_config();
        assert!(config.contains("postgres: 19beta4"));
        assert!(config.contains("compat versions: 14 15 16 17 18 19"));
    }
}

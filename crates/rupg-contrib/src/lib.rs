//! The contrib extensions that document 16 lists, in Rust.
//!
//! Each extension is a [`Builtin`] of `rupg-ext`: the control file and the scripts of the pin, unchanged, in [`generated`], and the table that binds each C symbol of the scripts to a Rust kernel. Each extension is behind a cargo feature of its name, on by default. See `spec/16-procedures-and-extensions.md` sections 16.7 and 16.8.

#![forbid(unsafe_code)]

mod generated;
mod support;

#[cfg(feature = "btree_gin")]
mod btree_gin;
#[cfg(feature = "btree_gist")]
mod btree_gist;
#[cfg(feature = "pg_trgm")]
mod pg_trgm;

use rupg_ext::Builtin;
use rupg_func::Kernel;

/// The extensions that rupg ships, in the order of their names.
static BUILTINS: &[Builtin] = &[
    #[cfg(feature = "btree_gin")]
    Builtin {
        name: "btree_gin",
        files: generated::files::BTREE_GIN,
        module: "btree_gin",
        symbols: btree_gin::SYMBOLS,
    },
    #[cfg(feature = "btree_gist")]
    Builtin {
        name: "btree_gist",
        files: generated::files::BTREE_GIST,
        module: "btree_gist",
        symbols: btree_gist::SYMBOLS,
    },
    #[cfg(feature = "pg_trgm")]
    Builtin {
        name: "pg_trgm",
        files: generated::files::PG_TRGM,
        module: "pg_trgm",
        symbols: pg_trgm::SYMBOLS,
    },
];

/// The extensions that rupg ships, in the order of their names.
pub fn builtins() -> &'static [Builtin] {
    BUILTINS
}

/// The extension `name` that rupg ships.
pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|builtin| builtin.name == name)
}

/// The library of a function in C: `None` when no extension that rupg ships has the library named by `bin`, the `probin` of the function, else the kernel bound to `symbol`, or `None` for a symbol that the library does not bind.
pub fn library_symbol(bin: &str, symbol: &str) -> Option<Option<Kernel>> {
    let module = rupg_ext::module_name(bin);
    let builtin = BUILTINS.iter().find(|builtin| builtin.module == module)?;
    Some(builtin.symbol(symbol))
}

/// The settings that the library of each extension defines when it loads, as `DefineCustomRealVariable` in `_PG_init` defines them.
pub fn settings() -> impl Iterator<Item = &'static rupg_ext::CustomSetting> {
    SETTINGS.iter().flat_map(|list| list.iter())
}

/// The lists of [`settings`].
static SETTINGS: &[&[rupg_ext::CustomSetting]] = &[
    #[cfg(feature = "pg_trgm")]
    pg_trgm::SETTINGS,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each C symbol that a script of an extension names is bound, and each bound symbol is in a script.
    #[test]
    fn bindings() {
        for builtin in builtins() {
            let mut named = std::collections::BTreeSet::new();
            for (file, text) in builtin.files {
                if file.ends_with(".sql") {
                    named.extend(support::c_symbols(text));
                }
            }
            for symbol in &named {
                assert!(
                    builtin.symbol(symbol).is_some(),
                    "{}: {symbol} is not bound",
                    builtin.name
                );
            }
            for (symbol, _) in builtin.symbols {
                assert!(named.contains(*symbol), "{}: {symbol} is in no script", builtin.name);
            }
        }
    }

    /// The control file of each extension parses, and its default version has an install path.
    #[test]
    fn controls() {
        for builtin in builtins() {
            let control = builtin.control().unwrap();
            let version = control.default_version.as_deref().unwrap();
            let (start, _) = builtin.versions().install_path(version).unwrap();
            assert!(builtin.script(None, &start).is_some(), "{}", builtin.name);
        }
    }
}

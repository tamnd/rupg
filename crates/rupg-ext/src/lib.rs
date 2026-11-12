//! The extension API, `CREATE EXTENSION` for built-in extensions.
//!
//! An extension that rupg ships is a [`Builtin`]: its control file and its scripts, vendored from the pin unchanged, and the table that binds the C symbols of its scripts to Rust kernels. This crate reads the control files, finds the install and update paths as PostgreSQL does, and prepares the text of a script. The session runs the scripts. See `spec/16-procedures-and-extensions.md` section 16.7.

#![forbid(unsafe_code)]

mod control;
mod script;
mod version;

use rupg_common::{Error, Result, SqlState};
use rupg_func::Kernel;

pub use control::{Control, split_identifiers};
pub use script::{Macros, prepare};
pub use version::{Versions, check_extension_name, check_version_name};

/// The location that `pg_available_extensions` shows for an extension that rupg ships, the `$system` of `extension_control_path`.
pub const SYSTEM_LOCATION: &str = "$system";

/// The directory of the control files of the extensions that rupg ships, for the names of files in messages.
const SYSTEM_DIRECTORY: &str = "$system/extension";

/// An extension that rupg ships.
#[derive(Clone, Copy, Debug)]
pub struct Builtin {
    /// The name of the extension.
    pub name: &'static str,
    /// The files of the extension, each a name and a text: `name.control`, the secondary control files and the scripts, in the order of a directory.
    pub files: &'static [(&'static str, &'static str)],
    /// The name of the library of the extension, the last part of `module_pathname`.
    pub module: &'static str,
    /// The C symbols of the library, each with the Rust kernel that runs it.
    pub symbols: &'static [(&'static str, Kernel)],
}

impl Builtin {
    /// The text of the file `name` of the extension.
    pub fn file(&self, name: &str) -> Option<&'static str> {
        self.files.iter().find(|(file, _)| *file == name).map(|(_, text)| *text)
    }

    /// The path of the file `name` of the extension, as a message names it.
    pub fn path(name: &str) -> String {
        format!("{SYSTEM_DIRECTORY}/{name}")
    }

    /// `read_extension_control_file`: the primary control file.
    ///
    /// # Errors
    ///
    /// The errors of [`Control::parse`].
    pub fn control(&self) -> Result<Control> {
        let file = format!("{}.control", self.name);
        let text = self.file(&file).ok_or_else(|| not_available(self.name))?;
        Control::parse(self.name, text, &Builtin::path(&file))
    }

    /// `read_extension_aux_control_file`: `control` with the secondary control file of `version`, if the extension has one.
    ///
    /// # Errors
    ///
    /// The errors of [`Control::with_secondary`].
    pub fn secondary(&self, control: &Control, version: &str) -> Result<Control> {
        let file = format!("{}--{version}.control", self.name);
        match self.file(&file) {
            Some(text) => control.with_secondary(version, text, &Builtin::path(&file)),
            None => Ok(control.clone()),
        }
    }

    /// `get_ext_ver_list`: the versions of the extension from the names of its scripts.
    pub fn versions(&self) -> Versions {
        Versions::new(self.name, self.files.iter().map(|(file, _)| *file))
    }

    /// The script that installs `to`, or that updates `from` to `to`.
    pub fn script(&self, from: Option<&str>, to: &str) -> Option<(String, &'static str)> {
        let file = match from {
            Some(from) => format!("{}--{from}--{to}.sql", self.name),
            None => format!("{}--{to}.sql", self.name),
        };
        let text = self.file(&file)?;
        Some((Builtin::path(&file), text))
    }

    /// The kernel bound to the C symbol `symbol` of the library.
    pub fn symbol(&self, symbol: &str) -> Option<Kernel> {
        self.symbols.iter().find(|(name, _)| *name == symbol).map(|(_, kernel)| *kernel)
    }
}

/// A setting of type `real` that the library of an extension defines when it loads, as `DefineCustomRealVariable` defines it with the context `PGC_USERSET`. The library also reserves the prefix of the name, as `MarkGUCPrefixReserved` does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomSetting {
    /// The name, with the prefix of the library and a dot.
    pub name: &'static str,
    /// `short_desc`, which `pg_settings.short_desc` shows.
    pub short_desc: &'static str,
    /// `long_desc`, which `pg_settings.extra_desc` shows.
    pub long_desc: &'static str,
    /// The default value.
    pub default: f64,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
}

/// The name of the library in the `probin` of a function in C: the last part of the path, without the suffix of a shared library.
pub fn module_name(bin: &str) -> &str {
    let file = bin.rsplit_once('/').map_or(bin, |(_, file)| file);
    file.strip_suffix(".so").or_else(|| file.strip_suffix(".dylib")).unwrap_or(file)
}

/// The error of `parse_extension_control_file` for an extension with no control file.
pub fn not_available(name: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("extension \"{name}\" is not available"))
        .with_hint(
            "The extension must first be installed on the system where PostgreSQL is running.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules() {
        assert_eq!(module_name("$libdir/pg_trgm"), "pg_trgm");
        assert_eq!(module_name("/opt/lib/regress.dylib"), "regress");
        assert_eq!(module_name("regress.so"), "regress");
    }
}

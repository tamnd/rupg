//! The Rust API, the facade crate that a user adds to `Cargo.toml`.
//!
//! At M1 the API is a key-value API over typed tables (spec/23 section 23.4). [`Database::open`] opens a file and replays the log when the file was not closed cleanly. [`Database::create_table`] makes a table with typed columns, and a [`Transaction`] does insert, update, delete, get and range scan by row id. SQL and the API of spec/17 section 17.3 come at M2 and later.
//!
//! ```no_run
//! use rupg::{ColumnDef, Database, Datum, Options, TypeId};
//!
//! # fn main() -> rupg::Result<()> {
//! let db = Database::open("app.rupg", Options::default())?;
//! let t = db.create_table("t", vec![
//!     ColumnDef { name: "id".into(), ty: TypeId::INT4, nullable: false },
//!     ColumnDef { name: "name".into(), ty: TypeId::TEXT, nullable: true },
//! ])?;
//! let mut tx = db.transaction()?;
//! let row = tx.insert(&t, &[Datum::Int4(1), Datum::Text("a".into())])?;
//! tx.commit()?;
//! let tx = db.transaction()?;
//! assert_eq!(tx.get(&t, row)?, Some(vec![Datum::Int4(1), Datum::Text("a".into())]));
//! db.close()?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod catalog;
mod database;

pub use database::{Database, Options, Platform, Table, Transaction};
pub use rupg_common::{
    CompatVersion, Error, Oid, POSTGRES_COMMIT, POSTGRES_PIN, Result, RowId, SqlState,
};
pub use rupg_table::ColumnDef;
pub use rupg_txn::Isolation;
pub use rupg_types::{Datum, TypeId};

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

//! Tables made from vendored PostgreSQL files.
//!
//! Nothing in here is written by hand except this file. `cargo xtask sql` writes the tables from the files in `vendor/postgres-19`, and `cargo xtask sql --check` fails CI if the two disagree. The same command writes `gram.rules`, the grammar without its C, and `productions.txt`, the name of each alternative.
//!
//! Lifted from `crates/rudb-pgparse/src/generated/mod.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

// The glue is long and has the layout of its generator. rustfmt does not change it, so that `cargo xtask sql --check` can compare it byte for byte.
#[rustfmt::skip]
pub(crate) mod glue;
pub(crate) mod keywords;
pub(crate) mod nodes;
pub(crate) mod tables;

//! Tables made from vendored PostgreSQL files.
//!
//! The tables are made from the files in `vendor/postgres-19`, and a test checks each table against its file.
//!
//! Lifted from `crates/rudb-pgwire/src/generated/mod.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

pub(crate) mod cmdtag;

// `cargo xtask unicode` makes this table from `vendor/postgres-19/src/common/saslprep.c`.
#[rustfmt::skip]
pub(crate) mod saslprep;

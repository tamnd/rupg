//! The generated schemas and static rows.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

#![allow(clippy::unreadable_literal, clippy::approx_constant, clippy::byte_char_slices)]

// The files have the layout of the generator. rustfmt does not change them, so that `cargo xtask pgcatalog --check` can compare them byte for byte.
#[rustfmt::skip]
pub(crate) mod catalogs;
#[rustfmt::skip]
pub(crate) mod pg_aggregate;
#[rustfmt::skip]
pub(crate) mod pg_am;
#[rustfmt::skip]
pub(crate) mod pg_amop;
#[rustfmt::skip]
pub(crate) mod pg_amproc;
#[rustfmt::skip]
pub(crate) mod pg_auth_members;
#[rustfmt::skip]
pub(crate) mod pg_authid;
#[rustfmt::skip]
pub(crate) mod pg_cast;
#[rustfmt::skip]
pub(crate) mod pg_class;
#[rustfmt::skip]
pub(crate) mod pg_collation;
#[rustfmt::skip]
pub(crate) mod pg_conversion;
#[rustfmt::skip]
pub(crate) mod pg_database;
#[rustfmt::skip]
pub(crate) mod pg_description;
#[rustfmt::skip]
pub(crate) mod pg_language;
#[rustfmt::skip]
pub(crate) mod pg_namespace;
#[rustfmt::skip]
pub(crate) mod pg_opclass;
#[rustfmt::skip]
pub(crate) mod pg_operator;
#[rustfmt::skip]
pub(crate) mod pg_opfamily;
#[rustfmt::skip]
pub(crate) mod pg_proc;
#[rustfmt::skip]
pub(crate) mod pg_range;
#[rustfmt::skip]
pub(crate) mod pg_shdescription;
#[rustfmt::skip]
pub(crate) mod pg_tablespace;
#[rustfmt::skip]
pub(crate) mod pg_ts_config;
#[rustfmt::skip]
pub(crate) mod pg_ts_config_map;
#[rustfmt::skip]
pub(crate) mod pg_ts_dict;
#[rustfmt::skip]
pub(crate) mod pg_ts_parser;
#[rustfmt::skip]
pub(crate) mod pg_ts_template;
#[rustfmt::skip]
pub(crate) mod pg_type;

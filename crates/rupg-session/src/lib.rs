//! Sessions, the 438 settings, portals, prepared statements, the plan cache, `LISTEN` and `NOTIFY`.
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.
//!
//! [`guc`] has the configuration parameters of PostgreSQL, the rules that read and print their values, and [`guc::Settings`], the values of one session with `SET`, `RESET`, `SET LOCAL` and the stack of the transaction (spec/06 section 6.13).

#![forbid(unsafe_code)]

mod generated;
pub mod guc;

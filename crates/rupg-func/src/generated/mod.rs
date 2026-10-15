//! Tables made from vendored files.

// `cargo xtask waitevents` makes this file from `vendor/postgres-19/src/backend/utils/activity/wait_event_names.txt`.
#[rustfmt::skip]
pub(crate) mod wait_events;

// `cargo xtask encodings` makes this file from the maps in `vendor/postgres-19/src/backend/utils/mb/Unicode`.
#[rustfmt::skip]
pub(crate) mod single_byte;

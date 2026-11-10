//! The clock of the changes of the row types: a composite datum keeps the time when it was formed, and a row type keeps the time when the type of one of its columns last changed, so that a datum formed before the change is not read with the new descriptor.

use std::sync::atomic::{AtomicU64, Ordering};

/// The time now, which [`tick`] moves on.
static CLOCK: AtomicU64 = AtomicU64::new(0);

/// The time now: the time of a datum formed now.
pub fn now() -> u64 {
    CLOCK.load(Ordering::Relaxed)
}

/// Moves the clock on, for a row type whose columns change type, and gives the new time. Each datum formed before has an earlier time.
pub fn tick() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed) + 1
}

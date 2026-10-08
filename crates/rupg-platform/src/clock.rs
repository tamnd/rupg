//! The `Clock` trait.

use std::fmt;
use std::time::Duration;

use rupg_common::Hlc;

/// The source of time.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The wall clock as a Unix time in milliseconds. It can go back, for example after an NTP step.
    fn wall_ms(&self) -> u64;

    /// The wall clock as a Unix time in microseconds, for the timestamps of SQL such as `now()`. A clock that has only milliseconds gives whole milliseconds.
    fn wall_us(&self) -> u64 {
        self.wall_ms().saturating_mul(1000)
    }

    /// The time since a fixed point in the past. It never goes back. Use it for timeouts and durations.
    fn monotonic(&self) -> Duration;

    /// The wall clock as the physical part of an [`Hlc`]: milliseconds since 2020-01-01 UTC.
    fn hlc_wall(&self) -> u64 {
        Hlc::physical_from_unix_ms(self.wall_ms())
    }
}

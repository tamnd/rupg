//! `Error` with its `SqlState`, the identifiers `Oid`, `ShardId`, `RowId`, `Hlc`, `Xid` and `Lsn` of document 04 section 4.7, `CompatVersion`.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

mod error;
mod ids;

pub use error::{Error, Result, SqlState};
pub use ids::{Hlc, Lsn, Oid, RowId, ShardId, Xid};

use std::fmt;
use std::str::FromStr;

/// The PostgreSQL release that rupg follows. See `spec/05-compatibility.md`.
pub const POSTGRES_PIN: &str = "19beta4";

/// The commit of the PostgreSQL pin on `REL_19_STABLE`.
pub const POSTGRES_COMMIT: &str = "7d3d2db7";

/// A PostgreSQL major version that `rupg.compat_version` can name.
///
/// Version 19 is the native answer. Versions 14 to 18 are shims over the answer of version 19. See `spec/05-compatibility.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompatVersion {
    /// PostgreSQL 14.
    V14,
    /// PostgreSQL 15.
    V15,
    /// PostgreSQL 16.
    V16,
    /// PostgreSQL 17.
    V17,
    /// PostgreSQL 18.
    V18,
    /// PostgreSQL 19, the native version.
    V19,
}

impl CompatVersion {
    /// All versions, oldest first.
    pub const ALL: [CompatVersion; 6] = [
        CompatVersion::V14,
        CompatVersion::V15,
        CompatVersion::V16,
        CompatVersion::V17,
        CompatVersion::V18,
        CompatVersion::V19,
    ];

    /// The native version. A shim maps the answer of this version to an older one.
    pub const NATIVE: CompatVersion = CompatVersion::V19;

    /// The major version number.
    pub const fn major(self) -> u32 {
        match self {
            CompatVersion::V14 => 14,
            CompatVersion::V15 => 15,
            CompatVersion::V16 => 16,
            CompatVersion::V17 => 17,
            CompatVersion::V18 => 18,
            CompatVersion::V19 => 19,
        }
    }

    /// The value of `server_version_num` for this version. The shims report the minor release of the shim pin.
    pub const fn server_version_num(self) -> u32 {
        match self {
            CompatVersion::V14 => 140024,
            CompatVersion::V15 => 150019,
            CompatVersion::V16 => 160015,
            CompatVersion::V17 => 170011,
            CompatVersion::V18 => 180006,
            CompatVersion::V19 => 190000,
        }
    }
}

impl fmt::Display for CompatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major())
    }
}

/// The error when a string does not name a supported version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownVersion(pub String);

impl fmt::Display for UnknownVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown PostgreSQL version {:?}, expected 14 to 19", self.0)
    }
}

impl std::error::Error for UnknownVersion {}

impl FromStr for CompatVersion {
    type Err = UnknownVersion;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        CompatVersion::ALL
            .into_iter()
            .find(|v| v.major().to_string() == s.trim())
            .ok_or_else(|| UnknownVersion(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every_version() {
        for v in CompatVersion::ALL {
            assert_eq!(v.to_string().parse::<CompatVersion>(), Ok(v));
        }
    }

    #[test]
    fn reject_unknown_versions() {
        assert!("13".parse::<CompatVersion>().is_err());
        assert!("20".parse::<CompatVersion>().is_err());
        assert!("".parse::<CompatVersion>().is_err());
    }

    #[test]
    fn version_numbers_match_the_major() {
        for v in CompatVersion::ALL {
            assert_eq!(v.server_version_num() / 10000, v.major());
        }
    }
}

//! The identifiers of spec/04 section 4.7. The file format stores them, so their widths are fixed from M1.
//!
//! Each identifier has a text form that parses back (spec/04 section 4.8).

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, SqlState};

fn parse_error(what: &str, text: &str) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid {what}: {text:?}"))
}

/// An object id, 32 bits, as in PostgreSQL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub u32);

impl Oid {
    /// The invalid OID, 0.
    pub const INVALID: Oid = Oid(0);
    /// The first OID for a user object. Built-in objects have the OIDs of the pin, below this value.
    pub const FIRST_USER: Oid = Oid(16384);
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Oid, Error> {
        s.parse().map(Oid).map_err(|_| parse_error("OID", s))
    }
}

/// A shard number, 16 bits. A single node has shard 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(pub u16);

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A row id, 64 bits: the shard in the high 16 bits and a local number in the low 48 bits.
///
/// A row keeps its row id for its life. An update keeps it, and a move from the hot store to the cold store keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(u64);

impl RowId {
    /// The largest local number, 2^48 - 1.
    pub const MAX_LOCAL: u64 = (1 << 48) - 1;

    /// Makes a row id. Returns `None` if `local` does not fit in 48 bits.
    pub const fn new(shard: ShardId, local: u64) -> Option<RowId> {
        if local > RowId::MAX_LOCAL {
            return None;
        }
        Some(RowId(((shard.0 as u64) << 48) | local))
    }

    /// The row id with this stored value.
    pub const fn from_bits(bits: u64) -> RowId {
        RowId(bits)
    }

    /// The stored value.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The shard.
    pub const fn shard(self) -> ShardId {
        ShardId((self.0 >> 48) as u16)
    }

    /// The local number.
    pub const fn local(self) -> u64 {
        self.0 & RowId::MAX_LOCAL
    }
}

impl fmt::Display for RowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.shard(), self.local())
    }
}

impl FromStr for RowId {
    type Err = Error;

    fn from_str(s: &str) -> Result<RowId, Error> {
        let (shard, local) = s.split_once(':').ok_or_else(|| parse_error("row id", s))?;
        let shard = shard.parse().map_err(|_| parse_error("row id", s))?;
        let local = local.parse().map_err(|_| parse_error("row id", s))?;
        RowId::new(ShardId(shard), local).ok_or_else(|| parse_error("row id", s))
    }
}

/// A hybrid logical clock value, 64 bits: 48 bits of milliseconds since 1 January 2020 UTC in the high bits and a 16-bit logical counter in the low bits.
///
/// Commit timestamps, snapshots and page timestamps are `Hlc` values. The order of the `u64` is the order of the clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc(u64);

impl Hlc {
    /// The Unix time of the epoch, 2020-01-01 00:00:00 UTC, in milliseconds.
    pub const EPOCH_UNIX_MS: u64 = 1_577_836_800_000;
    /// The largest physical part, 2^48 - 1 milliseconds, about 8,900 years.
    pub const MAX_PHYSICAL: u64 = (1 << 48) - 1;
    /// The zero value. No commit has it.
    pub const ZERO: Hlc = Hlc(0);
    /// The largest value.
    pub const MAX: Hlc = Hlc(u64::MAX);

    /// Makes a value from its two parts. Returns `None` if `physical` does not fit in 48 bits.
    pub const fn new(physical: u64, logical: u16) -> Option<Hlc> {
        if physical > Hlc::MAX_PHYSICAL {
            return None;
        }
        Some(Hlc((physical << 16) | logical as u64))
    }

    /// The value with these stored bits.
    pub const fn from_bits(bits: u64) -> Hlc {
        Hlc(bits)
    }

    /// The stored bits.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Milliseconds since the epoch of 2020.
    pub const fn physical(self) -> u64 {
        self.0 >> 16
    }

    /// The logical counter.
    pub const fn logical(self) -> u16 {
        self.0 as u16
    }

    /// The physical part as a Unix time in milliseconds.
    pub const fn unix_ms(self) -> u64 {
        self.physical() + Hlc::EPOCH_UNIX_MS
    }

    /// Converts a Unix time in milliseconds to the physical part. Times before the epoch become 0.
    pub const fn physical_from_unix_ms(unix_ms: u64) -> u64 {
        unix_ms.saturating_sub(Hlc::EPOCH_UNIX_MS)
    }

    /// The next local value after `self`, for a local event at wall time `wall` (milliseconds since the epoch).
    ///
    /// This is the send rule of Kulkarni and others (2014). If the wall clock is ahead, the value takes it with logical 0. Otherwise the logical counter goes up by one, and when it overflows the physical part goes forward by one millisecond.
    pub fn tick(self, wall: u64) -> Hlc {
        let wall = wall.min(Hlc::MAX_PHYSICAL);
        if wall > self.physical() {
            return Hlc(wall << 16);
        }
        Hlc(self.0 + 1)
    }

    /// The next local value after `self` that is also above `remote`, a value received from another node or ring.
    pub fn merge(self, remote: Hlc, wall: u64) -> Hlc {
        self.max(remote).tick(wall)
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.physical(), self.logical())
    }
}

impl FromStr for Hlc {
    type Err = Error;

    fn from_str(s: &str) -> Result<Hlc, Error> {
        let (physical, logical) = s.split_once('.').ok_or_else(|| parse_error("HLC value", s))?;
        let physical = physical.parse().map_err(|_| parse_error("HLC value", s))?;
        let logical = logical.parse().map_err(|_| parse_error("HLC value", s))?;
        Hlc::new(physical, logical).ok_or_else(|| parse_error("HLC value", s))
    }
}

/// A transaction id, 64 bits, local to the node, assigned at the first write.
///
/// PostgreSQL clients see the low 32 bits as `xid` and the full value as `xid8`. PostgreSQL reserves the 32-bit values 0, 1 and 2, so the counter skips each value whose low 32 bits are one of them (spec/11 section 11.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid(u64);

impl Xid {
    /// The first id.
    pub const FIRST: Xid = Xid(3);
    /// The 32-bit id that PostgreSQL uses for a frozen row.
    pub const FROZEN_XID: u32 = 2;

    /// The id with this stored value. Returns `None` for a value that the counter skips.
    pub const fn from_bits(bits: u64) -> Option<Xid> {
        if (bits as u32) < 3 {
            return None;
        }
        Some(Xid(bits))
    }

    /// The stored value, which is also `xid8`.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The 32-bit `xid`.
    pub const fn xid32(self) -> u32 {
        self.0 as u32
    }

    /// The next id.
    pub const fn next(self) -> Xid {
        let mut bits = self.0 + 1;
        if (bits as u32) < 3 {
            bits = (bits & !0xffff_ffff) | 3;
        }
        Xid(bits)
    }
}

impl fmt::Display for Xid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Xid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Xid, Error> {
        let bits = s.parse().map_err(|_| parse_error("transaction id", s))?;
        Xid::from_bits(bits).ok_or_else(|| parse_error("transaction id", s))
    }
}

/// A `pg_lsn` value. rupg has no single log, so the LSN that clients see is the commit timestamp of the newest durable commit (spec/04 section 4.7). It is monotonic and 64 bits wide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

impl From<Hlc> for Lsn {
    fn from(hlc: Hlc) -> Lsn {
        Lsn(hlc.bits())
    }
}

impl fmt::Display for Lsn {
    /// The `pg_lsn` text form: the high and low 32 bits in upper case hex, separated by a slash.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 as u32)
    }
}

impl FromStr for Lsn {
    type Err = Error;

    fn from_str(s: &str) -> Result<Lsn, Error> {
        let (high, low) = s.split_once('/').ok_or_else(|| parse_error("LSN", s))?;
        let ok =
            |p: &str| !p.is_empty() && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok(high) || !ok(low) {
            return Err(parse_error("LSN", s));
        }
        let high = u64::from_str_radix(high, 16).map_err(|_| parse_error("LSN", s))?;
        let low = u64::from_str_radix(low, 16).map_err(|_| parse_error("LSN", s))?;
        Ok(Lsn((high << 32) | low))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_id_fields() {
        let id = RowId::new(ShardId(7), 42).unwrap();
        assert_eq!(id.bits(), (7 << 48) | 42);
        assert_eq!((id.shard(), id.local()), (ShardId(7), 42));
        assert_eq!(id.to_string(), "7:42");
        assert_eq!("7:42".parse::<RowId>().unwrap(), id);
        assert!(RowId::new(ShardId(0), 1 << 48).is_none());
        assert!("0:281474976710656".parse::<RowId>().is_err());
        assert!("65536:1".parse::<RowId>().is_err());
    }

    #[test]
    fn hlc_parts_and_order() {
        let a = Hlc::new(1000, 5).unwrap();
        assert_eq!((a.physical(), a.logical()), (1000, 5));
        assert!(Hlc::new(1001, 0).unwrap() > Hlc::new(1000, u16::MAX).unwrap());
        assert_eq!(a.to_string(), "1000.5");
        assert_eq!("1000.5".parse::<Hlc>().unwrap(), a);
        assert!(Hlc::new(1 << 48, 0).is_none());
        // 2026-10-07 00:00:00 UTC.
        let unix = 1_791_331_200_000;
        assert_eq!(Hlc::new(Hlc::physical_from_unix_ms(unix), 0).unwrap().unix_ms(), unix);
    }

    #[test]
    fn hlc_tick() {
        let a = Hlc::new(1000, 5).unwrap();
        // The wall clock is ahead: take it.
        assert_eq!(a.tick(2000), Hlc::new(2000, 0).unwrap());
        // The wall clock is behind or equal: count.
        assert_eq!(a.tick(1000), Hlc::new(1000, 6).unwrap());
        assert_eq!(a.tick(10), Hlc::new(1000, 6).unwrap());
        // The counter overflows into the next millisecond.
        let full = Hlc::new(1000, u16::MAX).unwrap();
        assert_eq!(full.tick(1000), Hlc::new(1001, 0).unwrap());
        // A remote value ahead of both.
        assert_eq!(a.merge(Hlc::new(5000, 9).unwrap(), 2000), Hlc::new(5000, 10).unwrap());
    }

    #[test]
    fn hlc_is_monotonic_under_a_clock_that_jumps() {
        let mut h = Hlc::ZERO;
        for wall in [10, 20, 5, 5, 0, 30, 29, 1_000_000, 3] {
            let next = h.tick(wall);
            assert!(next > h);
            h = next;
        }
    }

    #[test]
    fn xid_skips_the_reserved_values() {
        assert_eq!(Xid::FIRST.xid32(), 3);
        let last = Xid::from_bits(0xffff_ffff).unwrap();
        let next = last.next();
        assert_eq!(next.bits(), (1 << 32) | 3);
        assert_eq!(next.xid32(), 3);
        assert!(Xid::from_bits(1 << 32).is_none());
        assert!(Xid::from_bits(2).is_none());
        assert_eq!("4294967299".parse::<Xid>().unwrap(), next);
    }

    #[test]
    fn lsn_text() {
        let lsn = Lsn(0x16_B374_D848);
        assert_eq!(lsn.to_string(), "16/B374D848");
        assert_eq!("16/B374D848".parse::<Lsn>().unwrap(), lsn);
        assert_eq!("0/0".parse::<Lsn>().unwrap(), Lsn(0));
        assert!("16".parse::<Lsn>().is_err());
        assert!("1/123456789".parse::<Lsn>().is_err());
        assert_eq!(Lsn::from(Hlc::new(1, 2).unwrap()), Lsn(0x1_0002));
    }
}

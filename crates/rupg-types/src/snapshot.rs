//! `pg_snapshot` and `txid_snapshot`, in the text and binary formats of `src/backend/utils/adt/xid8funcs.c`. A value is the binary form: the number of transactions in progress as an `int4`, then `xmin`, `xmax` and each transaction in progress as an `int8`.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::number::is_space;

/// `PG_SNAPSHOT_MAX_NXIP`: the most transactions in progress that fit in a value of `MaxAllocSize` bytes.
const MAX_NXIP: usize = (0x3fff_ffff - 20) / 8;

/// A snapshot: the first transaction that is still in progress, the first transaction that did not start, and the transactions in progress between them, in order and with no duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub xmin: u64,
    pub xmax: u64,
    pub xip: Vec<u64>,
}

impl Snapshot {
    /// The binary form, which is also the value.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + 8 * self.xip.len());
        let nxip = i32::try_from(self.xip.len()).unwrap_or(i32::MAX);
        out.extend_from_slice(&nxip.to_be_bytes());
        out.extend_from_slice(&self.xmin.to_be_bytes());
        out.extend_from_slice(&self.xmax.to_be_bytes());
        for xid in &self.xip {
            out.extend_from_slice(&xid.to_be_bytes());
        }
        out
    }

    /// The snapshot in the bytes that [`Snapshot::to_bytes`] gives, or `None` for bytes of another form.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Snapshot> {
        let mut recv = Recv::new(bytes);
        let nxip = usize::try_from(recv.i32().ok()?).ok()?;
        let xmin = recv.i64().ok()?.cast_unsigned();
        let xmax = recv.i64().ok()?.cast_unsigned();
        if recv.remaining() != nxip * 8 {
            return None;
        }
        let xip =
            (0..nxip).map(|_| recv.i64().map(i64::cast_unsigned)).collect::<Result<_, _>>().ok()?;
        Some(Snapshot { xmin, xmax, xip })
    }

    /// `is_visible_fxid`: true when the transaction ended before the snapshot. A transaction before `xmin` ended, a transaction at `xmax` or after it did not start, and a transaction between them ended when it is not in progress.
    #[must_use]
    pub fn is_visible(&self, xid: u64) -> bool {
        if xid < self.xmin {
            return true;
        }
        xid < self.xmax && self.xip.binary_search(&xid).is_err()
    }
}

/// `strtou64(s, &end, 10)`: the value and the length of the number at the start of `b`, which is 0 when there is no number. `strtoull` skips the spaces before the number, gives `u64::MAX` for a number that overflows and negates a number after a `-`.
fn strtou64(b: &[u8]) -> (u64, usize) {
    let mut i = 0;
    while b.get(i).is_some_and(|&c| is_space(c)) {
        i += 1;
    }
    let neg = b.get(i) == Some(&b'-');
    if neg || b.get(i) == Some(&b'+') {
        i += 1;
    }
    let first = i;
    let mut value = Some(0u64);
    while let Some(&c) = b.get(i).filter(|c| c.is_ascii_digit()) {
        value =
            value.and_then(|v| v.checked_mul(10)).and_then(|v| v.checked_add(u64::from(c - b'0')));
        i += 1;
    }
    if i == first {
        return (0, 0);
    }
    match value {
        None => (u64::MAX, i),
        Some(v) if neg => (v.wrapping_neg(), i),
        Some(v) => (v, i),
    }
}

/// `pg_snapshot_in`: `xmin:xmax:` and the transactions in progress with a `,` between them. `xmin` is not 0, `xmax` is not before `xmin`, and each transaction in progress is in order from `xmin` up to `xmax`. A duplicate is skipped. The error names `pg_snapshot` also for `txid_snapshot`.
///
/// # Errors
///
/// `22P02` for text that does not have this form.
pub fn snapshot_in(s: &str) -> Result<Snapshot, TypeError> {
    let syntax = || TypeError::syntax("pg_snapshot", s);
    let b = s.as_bytes();
    let (xmin, end) = strtou64(b);
    if b.get(end) != Some(&b':') {
        return Err(syntax());
    }
    let mut at = end + 1;
    let (xmax, end) = strtou64(&b[at..]);
    if b.get(at + end) != Some(&b':') {
        return Err(syntax());
    }
    at += end + 1;
    if !valid(xmin) || !valid(xmax) || xmax < xmin {
        return Err(syntax());
    }
    let mut xip = Vec::new();
    let mut last = 0;
    while at < b.len() {
        let (xid, end) = strtou64(&b[at..]);
        at += end;
        if xid < xmin || xid >= xmax || xid < last {
            return Err(syntax());
        }
        if xid != last {
            xip.push(xid);
        }
        last = xid;
        match b.get(at) {
            Some(b',') => at += 1,
            Some(_) => return Err(syntax()),
            None => {}
        }
    }
    Ok(Snapshot { xmin, xmax, xip })
}

/// `FullTransactionIdIsValid`: the transaction ID in the low 32 bits is not 0. A value with an epoch and the ID 0, such as 2^63, is not valid.
fn valid(full: u64) -> bool {
    full & u64::from(u32::MAX) != 0
}

/// `pg_snapshot_out`: `xmin:xmax:` and the transactions in progress with a `,` between them.
pub fn snapshot_out(snapshot: &Snapshot, out: &mut Vec<u8>) {
    let mut text = format!("{}:{}:", snapshot.xmin, snapshot.xmax);
    for (i, xid) in snapshot.xip.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&xid.to_string());
    }
    out.extend_from_slice(text.as_bytes());
}

/// `pg_snapshot_recv`: the binary form, with the checks of the text input. A transaction in progress can be equal to `xmax` here, as in PostgreSQL.
///
/// # Errors
///
/// `22P03` for a value that does not have this form, and `08P01` for a value with too few bytes.
pub fn snapshot_recv(recv: &mut Recv<'_>) -> Result<Snapshot, TypeError> {
    let bad = || {
        TypeError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "invalid external pg_snapshot data".to_string(),
        )
    };
    let Some(nxip) = usize::try_from(recv.i32()?).ok().filter(|&n| n <= MAX_NXIP) else {
        return Err(bad());
    };
    let xmin = recv.i64()?.cast_unsigned();
    let xmax = recv.i64()?.cast_unsigned();
    if !valid(xmin) || !valid(xmax) || xmax < xmin {
        return Err(bad());
    }
    let mut xip = Vec::new();
    let mut last = 0;
    for _ in 0..nxip {
        let xid = recv.i64()?.cast_unsigned();
        if xid < last || xid < xmin || xmax < xid {
            return Err(bad());
        }
        if xid != last {
            xip.push(xid);
        }
        last = xid;
    }
    Ok(Snapshot { xmin, xmax, xip })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> String {
        let mut out = Vec::new();
        snapshot_out(&snapshot_in(s).unwrap(), &mut out);
        String::from_utf8(out).unwrap()
    }

    /// The values and the errors of PostgreSQL 19 for the same text.
    #[test]
    fn text_forms() {
        assert_eq!(text("10:20:10,14,15"), "10:20:10,14,15");
        assert_eq!(text("10:20:10,10,14"), "10:20:10,14");
        assert_eq!(text("10:20:10,"), "10:20:10");
        assert_eq!(text(" 10:20:"), "10:20:");
        assert_eq!(text("00010:020:"), "10:20:");
        assert_eq!(text("10:10:"), "10:10:");
        assert_eq!(text("1:9223372036854775807:3"), "1:9223372036854775807:3");
        assert_eq!(
            text("18446744073709551616:18446744073709551616:"),
            "18446744073709551615:18446744073709551615:"
        );
        for bad in [
            "20:10:",
            "10:20:9",
            "10:20:20",
            "10:20:14,12",
            "0:20:",
            "a:20:",
            "10:20: ",
            "10 :20:",
            "10:20",
            "-1:20:",
            "1:9223372036854775808:3",
            "4294967296:4294967297:",
        ] {
            let error = snapshot_in(bad).unwrap_err();
            assert_eq!(
                error.message,
                format!("invalid input syntax for type pg_snapshot: \"{bad}\"")
            );
        }
    }

    #[test]
    fn binary_forms() {
        let snapshot = snapshot_in("10:20:10,14,15").unwrap();
        let bytes = snapshot.to_bytes();
        assert_eq!(bytes.len(), 44);
        assert_eq!(&bytes[..4], &[0, 0, 0, 3]);
        assert_eq!(snapshot_recv(&mut Recv::new(&bytes)).unwrap(), snapshot);
        assert_eq!(Snapshot::from_bytes(&bytes).unwrap(), snapshot);
        let mut bad = bytes.clone();
        bad[4..12].copy_from_slice(&30u64.to_be_bytes());
        assert_eq!(
            snapshot_recv(&mut Recv::new(&bad)).unwrap_err().message,
            "invalid external pg_snapshot data"
        );
    }

    #[test]
    fn visibility() {
        let snapshot = snapshot_in("10:20:10,14,15").unwrap();
        let visible: Vec<bool> =
            [9, 10, 11, 14, 19, 20].iter().map(|&xid| snapshot.is_visible(xid)).collect();
        assert_eq!(visible, [true, false, true, false, true, false]);
    }
}

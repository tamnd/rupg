//! Variable-length integers: LEB128 for unsigned values and the zigzag form for the deltas of a record.

use rupg_common::{Error, Result};

pub(crate) fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn put_signed(out: &mut Vec<u8>, v: i64) {
    put(out, ((v << 1) ^ (v >> 63)) as u64);
}

/// Reads a value at `*at` and moves `*at` past it. A value that does not end, that is longer than 10 bytes, or that has more than 64 bits gives SQLSTATE `XX001`.
pub(crate) fn take(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = *bytes.get(*at).ok_or_else(|| bad("a number does not end"))?;
        *at += 1;
        if i == 9 && b > 1 {
            return Err(bad("a number has more than 64 bits"));
        }
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err(bad("a number has a byte that is not necessary"));
            }
            return Ok(v);
        }
    }
    Err(bad("a number is longer than 10 bytes"))
}

pub(crate) fn take_signed(bytes: &[u8], at: &mut usize) -> Result<i64> {
    let v = take(bytes, at)?;
    Ok((v >> 1) as i64 ^ -((v & 1) as i64))
}

pub(crate) fn bad(what: &str) -> Error {
    Error::corrupted(format!("bad log record: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX - 1, u64::MAX] {
            let mut out = Vec::new();
            put(&mut out, v);
            let mut at = 0;
            assert_eq!((take(&out, &mut at).unwrap(), at), (v, out.len()));
        }
        for v in [0, 1, -1, 63, -64, 64, i64::MAX, i64::MIN] {
            let mut out = Vec::new();
            put_signed(&mut out, v);
            let mut at = 0;
            assert_eq!(take_signed(&out, &mut at).unwrap(), v);
        }
        let mut out = Vec::new();
        put_signed(&mut out, -1);
        assert_eq!(out, [1]);
    }

    #[test]
    fn bad_numbers() {
        for bytes in [
            &[0x80][..],
            &[0x80, 0],
            &[0xff; 11],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2],
        ] {
            assert!(take(bytes, &mut 0).is_err(), "{bytes:?}");
        }
    }
}

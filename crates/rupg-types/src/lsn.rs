//! `pg_lsn`, in the text format of `src/backend/utils/adt/pg_lsn.c`. A value is the position in the write-ahead log as a `u64`, and the binary form is that number as an `int8`.

use crate::error::TypeError;

/// `MAXPG_LSNCOMPONENT`: the most hex digits in each half of the text.
const MAX_DIGITS: usize = 8;

/// The number of hex digits at the start of `b`, as `strspn` counts them.
fn hex_len(b: &[u8]) -> usize {
    b.iter().take_while(|c| c.is_ascii_hexdigit()).count()
}

/// `pg_lsn_in`: two groups of 1 to 8 hex digits with a `/` between them. The text has no spaces and nothing after the second group.
///
/// # Errors
///
/// `22P02` for text that does not have this form.
pub fn pg_lsn_in(s: &str) -> Result<u64, TypeError> {
    let b = s.as_bytes();
    let syntax = || TypeError::syntax("pg_lsn", s);
    let high = hex_len(b);
    if !(1..=MAX_DIGITS).contains(&high) || b.get(high) != Some(&b'/') {
        return Err(syntax());
    }
    let low = hex_len(&b[high + 1..]);
    if !(1..=MAX_DIGITS).contains(&low) || high + 1 + low != b.len() {
        return Err(syntax());
    }
    let part = |text: &str| u64::from_str_radix(text, 16).map_err(|_| syntax());
    Ok((part(&s[..high])? << 32) | part(&s[high + 1..])?)
}

/// `pg_lsn_out`: the high and the low 32 bits in hex, with 8 digits for the low bits, as `%X/%08X` gives them.
pub fn pg_lsn_out(lsn: u64, out: &mut Vec<u8>) {
    let text = format!("{:X}/{:08X}", lsn >> 32, lsn & 0xFFFF_FFFF);
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lsn: u64) -> String {
        let mut out = Vec::new();
        pg_lsn_out(lsn, &mut out);
        String::from_utf8(out).unwrap()
    }

    /// The values and the errors of PostgreSQL 19 for the same text.
    #[test]
    fn text_forms() {
        assert_eq!(text(pg_lsn_in("16/B374D848").unwrap()), "16/B374D848");
        assert_eq!(text(pg_lsn_in("0/0").unwrap()), "0/00000000");
        assert_eq!(text(pg_lsn_in("ff/ab").unwrap()), "FF/000000AB");
        assert_eq!(pg_lsn_in("FFFFFFFF/FFFFFFFF").unwrap(), u64::MAX);
        for bad in ["16/B374D848x", "1/100000000", " 1/1", "/1", "1/", "1", ""] {
            let error = pg_lsn_in(bad).unwrap_err();
            assert_eq!(error.message, format!("invalid input syntax for type pg_lsn: \"{bad}\""));
        }
    }
}

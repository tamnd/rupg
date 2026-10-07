//! Base64 as `src/common/base64.c` does it. SCRAM sends the salt, the proof and the signature in base64, and a client that sends a value the server rejects must be rejected by rupg too.
//!
//! Lifted from `crates/rudb-pgwire/src/base64.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `pg_b64_encode`, with padding.
pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let word = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(word >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `pg_b64_decode`. It fails on white space, on a byte outside the alphabet, on `=` in the first two places of a group, and on a last group that is not complete. Like PostgreSQL, it takes letters after the first `=` and drops the bytes they make.
pub(crate) fn decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut word, mut pos, mut end) = (0u32, 0, 0);
    for &c in text {
        let bits = if c == b'=' {
            if end == 0 {
                end = match pos {
                    2 => 1,
                    3 => 2,
                    _ => return None,
                };
            }
            0
        } else {
            ALPHABET.iter().position(|&a| a == c)? as u32
        };
        word = word << 6 | bits;
        pos += 1;
        if pos == 4 {
            out.push((word >> 16) as u8);
            if end == 0 || end > 1 {
                out.push((word >> 8) as u8);
            }
            if end == 0 || end > 2 {
                out.push(word as u8);
            }
            (word, pos) = (0, 0);
        }
    }
    (pos == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // The prefixes of "foobar" are the test vectors of RFC 4648 section 10.
        let all = b"foobar";
        for n in 0..=all.len() {
            let data = &all[..n];
            assert_eq!(decode(encode(data).as_bytes()).as_deref(), Some(data));
        }
        assert_eq!(encode(b"n,,"), "biws");
        assert_eq!(encode(b"y,,"), "eSws");
    }

    #[test]
    fn what_postgres_rejects() {
        for text in [&b"Zm9v YmFy"[..], b"Zm9", b"Z===", b"Zm9v\n", b"Zm*v", b"=AAA"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
        // A letter after the padding is taken and its bits are dropped.
        assert_eq!(decode(b"Zg=A").as_deref(), Some(&b"f"[..]));
    }
}

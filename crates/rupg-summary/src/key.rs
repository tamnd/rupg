//! The vector key: a value as a `u64` in an order that never goes against the order of the type.
//!
//! The bounds of a vector are two keys (spec/09 section 9.7.1). A key map is monotone: when `a <= b` in the order of the type, `key(a) <= key(b)`. Two values with different keys therefore compare as their keys do, and a test on keys is sound when it reads equal keys as "do not know". A map that keeps every value apart is exact, and on exact keys equal keys mean equal values.

use std::cmp::Ordering;

/// The key of an integer. Exact.
#[must_use]
pub fn int(value: i64) -> u64 {
    (value as u64) ^ (1 << 63)
}

/// The integer of a key from [`int`].
#[must_use]
pub fn int_of(key: u64) -> i64 {
    (key ^ (1 << 63)) as i64
}

/// The key of a 128-bit integer, the scaled form of a `numeric` (spec/09 section 9.8). Values outside the range of `i64` get the key of the nearest end, so the map is monotone and is exact only inside that range.
#[must_use]
pub fn wide(value: i128) -> u64 {
    int(value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// True when [`wide`] keeps `value` apart from every other value.
#[must_use]
pub fn wide_is_exact(value: i128) -> bool {
    i64::try_from(value).is_ok_and(|v| v != i64::MIN && v != i64::MAX)
}

/// The key of a `double precision` in the order of PostgreSQL: negative infinity, the numbers, positive infinity, then NaN. Negative zero and zero are equal in PostgreSQL and get the same key. Every NaN gets the highest key, because PostgreSQL makes every NaN equal. Exact for the order of PostgreSQL.
#[must_use]
pub fn float(value: f64) -> u64 {
    if value.is_nan() {
        return u64::MAX;
    }
    let value = if value == 0.0 { 0.0 } else { value };
    let bits = value.to_bits();
    if bits >> 63 == 1 { !bits } else { bits | (1 << 63) }
}

/// The comparison of `double precision` in PostgreSQL, `float8_cmp_internal`: NaN is equal to NaN and above every other value, and negative zero is equal to zero.
#[must_use]
pub fn float_cmp(a: f64, b: f64) -> Ordering {
    float(a).cmp(&float(b))
}

/// The key of a byte string: its first 8 bytes in big endian order, with zero bytes after a shorter string. The map is monotone in byte order, which is the order of the `C` collation, and it is not exact.
#[must_use]
pub fn bytes(value: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    let len = value.len().min(8);
    word[..len].copy_from_slice(&value[..len]);
    u64::from_be_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_keys_keep_the_order() {
        let values = [i64::MIN, -5, -1, 0, 1, 7, i64::MAX];
        for pair in values.windows(2) {
            assert!(int(pair[0]) < int(pair[1]));
            assert_eq!(int_of(int(pair[0])), pair[0]);
        }
    }

    #[test]
    fn float_keys_follow_postgresql() {
        let values = [
            f64::NEG_INFINITY,
            -1e300,
            -1.0,
            -f64::MIN_POSITIVE,
            0.0,
            5e-324,
            1.0,
            f64::MAX,
            f64::INFINITY,
        ];
        for pair in values.windows(2) {
            assert!(float(pair[0]) < float(pair[1]), "{} {}", pair[0], pair[1]);
        }
        assert_eq!(float(-0.0), float(0.0));
        assert_eq!(float(f64::NAN), float(-f64::NAN));
        assert!(float(f64::INFINITY) < float(f64::NAN));
        assert_eq!(float_cmp(f64::NAN, f64::NAN), Ordering::Equal);
    }

    #[test]
    fn byte_keys_are_monotone() {
        let values: [&[u8]; 6] = [b"", b"\0", b"a", b"ab", b"abcdefgh", b"abcdefghz"];
        for pair in values.windows(2) {
            assert!(bytes(pair[0]) <= bytes(pair[1]));
        }
        assert!(bytes(b"abcdefgh") == bytes(b"abcdefghz"));
    }

    #[test]
    fn wide_keys_clamp() {
        assert_eq!(wide(i128::MAX), int(i64::MAX));
        assert_eq!(wide(-5), int(-5));
        assert!(wide_is_exact(5));
        assert!(!wide_is_exact(i128::from(i64::MAX) + 1));
    }
}

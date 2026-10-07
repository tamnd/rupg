//! The values of `NUMERIC`, the decimal number of PostgreSQL with no fixed width and no fixed scale, as the bytes that a vector holds.
//!
//! The number is the one `NumericVar` holds in `src/backend/utils/adt/numeric.c`: a sign, a weight, base-10000 digits and a display scale. [`crate::Numeric`] does the arithmetic, the input and the output on that form, and this module only packs it.
//!
//! The first byte is the class of the value, in the order of the values: negative infinity, a negative number, zero, a positive number, positive infinity and `NaN`, as PostgreSQL sorts them. A number then has its weight and its digits, with no zero digit at either end. A negative number has them flipped and a last word of `0xffff`, so that a longer negative number sorts first. The last two bytes are the display scale. Thus the bytes before the last two order as the numbers do, and `1.0` and `1.00` have the same [`key`] and different bytes.
//!
//! Lifted from `crates/rudb-common/src/numeric.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

/// `NUMERIC_POS`, `NUMERIC_NEG`, `NUMERIC_NAN`, `NUMERIC_PINF` and `NUMERIC_NINF`, the sign codes of PostgreSQL.
pub const POSITIVE: u16 = 0x0000;
pub const NEGATIVE: u16 = 0x4000;
pub const NAN: u16 = 0xc000;
pub const INFINITY: u16 = 0xd000;
pub const NEGATIVE_INFINITY: u16 = 0xf000;

const CLASS_NEGATIVE_INFINITY: u8 = 1;
const CLASS_NEGATIVE: u8 = 2;
const CLASS_ZERO: u8 = 3;
const CLASS_POSITIVE: u8 = 4;
const CLASS_INFINITY: u8 = 5;
const CLASS_NAN: u8 = 6;

/// A value taken apart. A special value has no digits, a weight of 0 and a display scale of 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// One of the sign codes above.
    pub sign: u16,
    /// The power of 10000 of the first digit.
    pub weight: i16,
    /// The number of decimal digits that the text shows after the point.
    pub dscale: u16,
    /// The base-10000 digits, the most significant first, with no zero at either end.
    pub digits: Vec<i16>,
}

/// The bytes of a value. Zero digits at either end are dropped, and a number with no digits left is zero.
#[must_use]
pub fn encode(sign: u16, weight: i16, dscale: u16, digits: &[i16]) -> Vec<u8> {
    let lead = digits.iter().take_while(|&&digit| digit == 0).count();
    let digits = &digits[lead..];
    let tail = digits.iter().rev().take_while(|&&digit| digit == 0).count();
    let digits = &digits[..digits.len() - tail];
    #[expect(clippy::cast_possible_truncation, reason = "a weight is far below 2^15 digits")]
    let weight = weight.wrapping_sub(lead as i16);
    let class = match sign {
        NAN => CLASS_NAN,
        INFINITY => CLASS_INFINITY,
        NEGATIVE_INFINITY => CLASS_NEGATIVE_INFINITY,
        _ if digits.is_empty() => CLASS_ZERO,
        NEGATIVE => CLASS_NEGATIVE,
        _ => CLASS_POSITIVE,
    };
    let mut bytes = Vec::with_capacity(5 + 2 * digits.len() + 2);
    bytes.push(class);
    let words = std::iter::once(weight as u16 ^ 0x8000).chain(digits.iter().map(|&d| d as u16));
    match class {
        CLASS_POSITIVE => words.for_each(|word| bytes.extend_from_slice(&word.to_be_bytes())),
        CLASS_NEGATIVE => {
            // The weight flips, and each digit is stored as 0xfffe less the digit so that the end word 0xffff is above every digit.
            let mut words = words;
            let weight = words.next().unwrap_or_default();
            bytes.extend_from_slice(&(!weight).to_be_bytes());
            for digit in words {
                bytes.extend_from_slice(&(0xfffe - digit).to_be_bytes());
            }
            bytes.extend_from_slice(&0xffffu16.to_be_bytes());
        }
        _ => {}
    }
    let dscale =
        if matches!(class, CLASS_POSITIVE | CLASS_NEGATIVE | CLASS_ZERO) { dscale } else { 0 };
    bytes.extend_from_slice(&dscale.to_be_bytes());
    bytes
}

/// The value of bytes that [`encode`] wrote. Bytes that it cannot have written read as `NaN`.
#[must_use]
pub fn decode(bytes: &[u8]) -> Unpacked {
    let special = |sign| Unpacked { sign, weight: 0, dscale: 0, digits: Vec::new() };
    let Some((&class, rest)) = bytes.split_first() else {
        return special(NAN);
    };
    let dscale = match rest.len().checked_sub(2) {
        Some(at) => u16::from_be_bytes([rest[at], rest[at + 1]]),
        None => return special(NAN),
    };
    let body = &rest[..rest.len() - 2];
    let words = || body.as_chunks::<2>().0.iter().map(|&pair| u16::from_be_bytes(pair));
    match class {
        CLASS_ZERO => Unpacked { sign: POSITIVE, weight: 0, dscale, digits: Vec::new() },
        CLASS_POSITIVE => {
            let mut words = words();
            let weight = (words.next().unwrap_or(0x8000) ^ 0x8000) as i16;
            let digits = words.map(|word| word as i16).collect();
            Unpacked { sign: POSITIVE, weight, dscale, digits }
        }
        CLASS_NEGATIVE => {
            let mut words = words();
            let weight = (!words.next().unwrap_or(0x7fff) ^ 0x8000) as i16;
            let digits =
                words.take_while(|&word| word != 0xffff).map(|word| (0xfffe - word) as i16);
            Unpacked { sign: NEGATIVE, weight, dscale, digits: digits.collect() }
        }
        CLASS_INFINITY => special(INFINITY),
        CLASS_NEGATIVE_INFINITY => special(NEGATIVE_INFINITY),
        _ => special(NAN),
    }
}

/// The bytes that order and compare as the number does, which leave out the display scale.
#[must_use]
pub fn key(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len().saturating_sub(2)]
}

/// The text of a value, as `numeric_out` writes it: the digits with exactly the display scale.
#[must_use]
pub fn to_text(bytes: &[u8]) -> String {
    let value = decode(bytes);
    match value.sign {
        NAN => return "NaN".to_owned(),
        INFINITY => return "Infinity".to_owned(),
        NEGATIVE_INFINITY => return "-Infinity".to_owned(),
        _ => {}
    }
    let mut out = String::new();
    if value.sign == NEGATIVE {
        out.push('-');
    }
    let digit = |weight: i32| -> i32 {
        let at = i32::from(value.weight) - weight;
        usize::try_from(at).ok().and_then(|at| value.digits.get(at)).map_or(0, |&d| i32::from(d))
    };
    // The digits before the point, the first with no leading zeros.
    if value.weight < 0 {
        out.push('0');
    } else {
        for weight in (0..=i32::from(value.weight)).rev() {
            let d = digit(weight);
            if weight == i32::from(value.weight) {
                out.push_str(&d.to_string());
            } else {
                out.push_str(&format!("{d:04}"));
            }
        }
    }
    if value.dscale > 0 {
        out.push('.');
        let mut written = 0;
        let mut weight = -1;
        while written < usize::from(value.dscale) {
            let group = format!("{:04}", digit(weight));
            let take = (usize::from(value.dscale) - written).min(4);
            out.push_str(&group[..take]);
            written += take;
            weight -= 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(sign: u16, weight: i16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        encode(sign, weight, dscale, digits)
    }

    #[test]
    fn the_bytes_order_as_the_numbers_do() {
        let values = [
            bytes(NEGATIVE_INFINITY, 0, 0, &[]),
            bytes(NEGATIVE, 1, 0, &[1, 0]),
            bytes(NEGATIVE, 0, 1, &[1, 5000]),
            bytes(NEGATIVE, 0, 0, &[1]),
            bytes(NEGATIVE, -1, 4, &[1]),
            bytes(POSITIVE, 0, 2, &[]),
            bytes(POSITIVE, -1, 4, &[1]),
            bytes(POSITIVE, 0, 0, &[1]),
            bytes(POSITIVE, 0, 1, &[1, 5000]),
            bytes(POSITIVE, 1, 0, &[1, 0]),
            bytes(INFINITY, 0, 0, &[]),
            bytes(NAN, 0, 0, &[]),
        ];
        for pair in values.windows(2) {
            assert!(key(&pair[0]) < key(&pair[1]), "{} < {}", to_text(&pair[0]), to_text(&pair[1]));
        }
        assert_eq!(key(&bytes(POSITIVE, 0, 1, &[1])), key(&bytes(POSITIVE, 0, 2, &[1, 0])));
    }

    #[test]
    fn a_value_reads_back_and_prints_with_its_scale() {
        for (sign, weight, dscale, digits, text) in [
            (POSITIVE, 0, 20, &[0, 3333, 3333, 3333, 3333, 3333][..], "0.33333333333333333333"),
            (NEGATIVE, 1, 2, &[12, 3456, 7800][..], "-123456.78"),
            (POSITIVE, 0, 3, &[][..], "0.000"),
            (POSITIVE, -2, 6, &[33][..], "0.000000"),
            (POSITIVE, -2, 8, &[33][..], "0.00000033"),
            (NAN, 0, 0, &[][..], "NaN"),
            (NEGATIVE_INFINITY, 0, 0, &[][..], "-Infinity"),
        ] {
            let packed = bytes(sign, weight, dscale, digits);
            assert_eq!(to_text(&packed), text);
            let back = decode(&packed);
            assert_eq!(encode(back.sign, back.weight, back.dscale, &back.digits), packed);
        }
    }
}

//! `money` in the text format, as `cash_in`, `cash_out` and `cash_words` in `src/backend/utils/adt/cash.c` give it.
//!
//! A value is an `i64` count of cents. PostgreSQL takes the symbols from `lc_monetary`, and rupg has only the `C` locale, so the currency symbol is `$`, the decimal point is `.`, the group separator is `,`, the signs are `+` and `-`, and a value has 2 decimal places.

use crate::error::TypeError;
use crate::number::is_space;

/// The decimal places of a value, `frac_digits` of the `C` locale after the check in `cash_in`.
const PLACES: usize = 2;

/// The text input of `money`, as `cash_in` reads it. Digits after the second decimal place round the value half away from zero by the next digit, and the other digits are ignored.
///
/// # Errors
///
/// `22P02` for text that is not an amount, and `22003` for an amount out of the range of `i64`.
pub fn cash_in(s: &str) -> Result<i64, TypeError> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let range = || TypeError::range(s, "money");
    let skip = |mut i: usize| {
        while is_space(at(i)) {
            i += 1;
        }
        if at(i) == b'$' {
            i += 1;
        }
        while is_space(at(i)) {
            i += 1;
        }
        i
    };
    let mut i = skip(0);
    let mut negative = false;
    match at(i) {
        b'-' | b'(' => {
            negative = true;
            i += 1;
        }
        b'+' => i += 1,
        _ => {}
    }
    i = skip(i);
    // The amount is built as a negative number, so that the most negative value fits.
    let mut value: i64 = 0;
    let mut places = 0;
    let mut seen_dot = false;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() && (!seen_dot || places < PLACES) {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_sub(i64::from(c - b'0')))
                .ok_or_else(range)?;
            if seen_dot {
                places += 1;
            }
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if c != b',' {
            break;
        }
        i += 1;
    }
    if at(i) >= b'5' && at(i).is_ascii_digit() {
        value = value.checked_sub(1).ok_or_else(range)?;
    }
    for _ in places..PLACES {
        value = value.checked_mul(10).ok_or_else(range)?;
    }
    while at(i).is_ascii_digit() {
        i += 1;
    }
    while i < b.len() {
        match b[i] {
            b'-' => negative = true,
            c if is_space(c) || matches!(c, b')' | b'+' | b'$') => {}
            _ => return Err(TypeError::syntax("money", s)),
        }
        i += 1;
    }
    if negative { Ok(value) } else { value.checked_neg().ok_or_else(range) }
}

/// The text output of `money`, as `cash_out` writes it in the `C` locale: a minus sign for a negative value, `$`, the whole part with a `,` between groups of 3 digits, `.` and 2 decimal places.
pub fn cash_out(value: i64, out: &mut Vec<u8>) {
    let mut digits = value.unsigned_abs().to_string().into_bytes();
    if digits.len() <= PLACES {
        let mut padded = vec![b'0'; PLACES + 1 - digits.len()];
        padded.extend_from_slice(&digits);
        digits = padded;
    }
    let (whole, fraction) = digits.split_at(digits.len() - PLACES);
    if value < 0 {
        out.push(b'-');
    }
    out.push(b'$');
    for (n, d) in whole.iter().enumerate() {
        if n > 0 && (whole.len() - n) % 3 == 0 {
            out.push(b',');
        }
        out.push(*d);
    }
    out.push(b'.');
    out.extend_from_slice(fraction);
}

/// The words of a number from 0 to 999, as `append_num_word` writes them.
fn num_words(value: u64, out: &mut String) {
    const SMALL: [&str; 28] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
        "thirty",
        "forty",
        "fifty",
        "sixty",
        "seventy",
        "eighty",
        "ninety",
    ];
    let small = |n: u64| SMALL[usize::try_from(n).unwrap_or(0)];
    // `big[n]` is the word for `n` tens, from 2.
    let big = |n: u64| SMALL[usize::try_from(n + 18).unwrap_or(0)];
    let tu = value % 100;
    if value <= 20 {
        out.push_str(small(value));
        return;
    }
    if tu == 0 {
        out.push_str(small(value / 100));
        out.push_str(" hundred");
        return;
    }
    if value > 99 {
        out.push_str(small(value / 100));
        out.push_str(" hundred ");
        if tu < 20 {
            out.push_str("and ");
        }
    }
    if value.is_multiple_of(10) && tu > 10 {
        out.push_str(big(tu / 10));
    } else if tu < 20 {
        out.push_str(small(tu));
    } else {
        out.push_str(big(tu / 10));
        out.push(' ');
        out.push_str(small(tu % 10));
    }
}

/// `cash_words`: the amount in English words, such as `One hundred dollars and zero cents`.
pub fn cash_words(value: i64) -> String {
    let mut out = String::new();
    if value < 0 {
        out.push_str("minus ");
    }
    let val = value.unsigned_abs();
    let dollars = val / 100;
    let units = [
        (100_000_000_000_000_000, " quadrillion "),
        (100_000_000_000_000, " trillion "),
        (100_000_000_000, " billion "),
        (100_000_000, " million "),
        (100_000, " thousand "),
    ];
    for (divisor, word) in units {
        let part = (val / divisor) % 1000;
        if part != 0 {
            num_words(part, &mut out);
            out.push_str(word);
        }
    }
    let hundreds = dollars % 1000;
    if hundreds != 0 {
        num_words(hundreds, &mut out);
    }
    if dollars == 0 {
        out.push_str("zero");
    }
    out.push_str(if dollars == 1 { " dollar and " } else { " dollars and " });
    let cents = val % 100;
    num_words(cents, &mut out);
    out.push_str(if cents == 1 { " cent" } else { " cents" });
    out[..1].make_ascii_uppercase();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(value: i64) -> String {
        let mut buf = Vec::new();
        cash_out(value, &mut buf);
        String::from_utf8(buf).unwrap()
    }

    /// Each pair is the result of PostgreSQL 19 for the input and the output of the value.
    #[test]
    fn text_forms() {
        for (input, output) in [
            ("12.34", "$12.34"),
            ("-12.34", "-$12.34"),
            ("0", "$0.00"),
            ("$1,234,567.89", "$1,234,567.89"),
            ("1234567.891", "$1,234,567.89"),
            ("1234567.895", "$1,234,567.90"),
            (" $ 12", "$12.00"),
            ("-$12", "-$12.00"),
            ("$-12", "-$12.00"),
            ("(12.5)", "-$12.50"),
            ("12-", "-$12.00"),
            ("+12", "$12.00"),
            ("12 $", "$12.00"),
            ("1,2,3.4", "$123.40"),
            (".5", "$0.50"),
            ("$", "$0.00"),
            ("", "$0.00"),
            ("92233720368547758.07", "$92,233,720,368,547,758.07"),
            ("-92233720368547758.08", "-$92,233,720,368,547,758.08"),
            ("1.2345", "$1.23"),
            ("1.2355", "$1.24"),
            ("0.005", "$0.01"),
            ("-0.005", "-$0.01"),
            ("0.004", "$0.00"),
        ] {
            assert_eq!(out(cash_in(input).unwrap()), output, "{input}");
        }
        for (input, sqlstate) in [
            ("92233720368547758.08", "22003"),
            ("-92233720368547758.09", "22003"),
            ("92233720368547758.075", "22003"),
            ("12.3.4", "22P02"),
            ("abc", "22P02"),
            ("12 34", "22P02"),
        ] {
            assert_eq!(cash_in(input).unwrap_err().sqlstate.as_str(), sqlstate, "{input}");
        }
    }

    /// The results of PostgreSQL 19 for `cash_words`.
    #[test]
    fn words() {
        assert_eq!(
            cash_words(123_456),
            "One thousand two hundred thirty four dollars and fifty six cents"
        );
        assert_eq!(cash_words(-1), "Minus zero dollars and one cent");
        assert_eq!(cash_words(0), "Zero dollars and zero cents");
        assert_eq!(cash_words(100_000_000), "One million  dollars and zero cents");
    }
}

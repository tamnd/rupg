//! `tid`, the address of a row as a block number and an offset in the block, in the text format of `tidin` and `tidout` in `src/backend/utils/adt/tid.c`.

use crate::error::TypeError;
use crate::number::is_space;

/// `strtoul` with base 10: white space, a sign and decimal digits. The result is the value, the length of the text that it read, 0 when there is no digit, and a flag for an overflow. A minus sign gives the value negated modulo 2^64.
fn strtoul10(b: &[u8]) -> (u64, usize, bool) {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let negative = at(i) == b'-';
    if negative || at(i) == b'+' {
        i += 1;
    }
    let first = i;
    let mut value = 0u64;
    let mut overflow = false;
    while at(i).is_ascii_digit() {
        match value.checked_mul(10).and_then(|v| v.checked_add(u64::from(at(i) - b'0'))) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == first {
        return (0, 0, false);
    }
    (if negative { value.wrapping_neg() } else { value }, i, overflow)
}

/// The text input of `tid`, as `tidin` reads it: the block number and the offset after the first `(` or `,`, and after the next `,`. The block number must end at a `,` and the offset must end at a `)`. The text before the `(` and after the `)` is ignored.
///
/// # Errors
///
/// `22P02` for text that is not a `tid`.
pub fn tid_in(s: &str) -> Result<(u32, u16), TypeError> {
    let syntax = || TypeError::syntax("tid", s);
    let b = s.as_bytes();
    let mut coords = [0usize; 2];
    let mut found = 0;
    for (i, &c) in b.iter().enumerate() {
        if found == 2 || c == b')' {
            break;
        }
        if c == b',' || (c == b'(' && found == 0) {
            coords[found] = i + 1;
            found += 1;
        }
    }
    if found < 2 {
        return Err(syntax());
    }
    let (block, end, overflow) = strtoul10(&b[coords[0]..]);
    if overflow || b.get(coords[0] + end) != Some(&b',') {
        return Err(syntax());
    }
    // The block number is 32 bits. A value from the negative range of `int32` is taken as its bits, as `uint32in_subr` takes it.
    let block32 = u32::try_from(block)
        .or_else(|_| i32::try_from(block.cast_signed()).map(i32::cast_unsigned))
        .map_err(|_| syntax())?;
    let (offset, end, overflow) = strtoul10(&b[coords[1]..]);
    if overflow || b.get(coords[1] + end) != Some(&b')') {
        return Err(syntax());
    }
    let offset = u16::try_from(offset).map_err(|_| syntax())?;
    Ok((block32, offset))
}

/// The text output of `tid`, as `tidout` writes it: `(block,offset)`.
pub fn tid_out(block: u32, offset: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("({block},{offset})").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each case is the result of PostgreSQL 19 for the input.
    #[test]
    fn text_forms() {
        for (input, output) in [
            ("(0,1)", "(0,1)"),
            ("(4294967295,65535)", "(4294967295,65535)"),
            ("(-1,1)", "(4294967295,1)"),
            ("ab(1,2)cd", "(1,2)"),
            ("(,2)", "(0,2)"),
            ("( 1, 2)", "(1,2)"),
            ("(+1,+2)", "(1,2)"),
        ] {
            let (block, offset) = tid_in(input).unwrap();
            let mut out = Vec::new();
            tid_out(block, offset, &mut out);
            assert_eq!(String::from_utf8(out).unwrap(), output, "{input}");
        }
        for input in ["(1,65536)", "(4294967296,1)", "(1,2", "1,2)", "(1 ,2)"] {
            assert_eq!(tid_in(input).unwrap_err().sqlstate.as_str(), "22P02", "{input}");
        }
    }
}

//! The Unicode normalization forms NFC, NFD, NFKC and NFKD (Unicode Standard Annex 15).
//!
//! PostgreSQL uses them in `normalize`, in `IS NORMALIZED` and in SASLprep. The tables come from the vendored Unicode data of the PostgreSQL pin, so the result is the same as in PostgreSQL. See `spec/07-sql-types-and-catalog.md` and `spec/22-crate-layout.md` section 22.8.

use crate::generated::unicode_norm::{
    COMBINING_CLASS, COMPOSITION, DECOMPOSITION, DECOMPOSITION_CHARS,
};

/// A Unicode normalization form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

// The constants of the Hangul syllables, from section 3.12 of the Unicode Standard.
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of a code point.
pub fn combining_class(c: u32) -> u8 {
    match COMBINING_CLASS.binary_search_by_key(&c, |e| e.0) {
        Ok(i) => COMBINING_CLASS[i].1,
        Err(_) => 0,
    }
}

/// Normalizes a string of code points to a form. The input can hold any `u32`, as `unicode_normalize` of PostgreSQL can.
pub fn normalize(form: NormalForm, input: &[u32]) -> Vec<u32> {
    let compat = matches!(form, NormalForm::Nfkc | NormalForm::Nfkd);
    let mut out = Vec::with_capacity(input.len());
    for &c in input {
        decompose(c, compat, &mut out);
    }
    reorder(&mut out);
    if matches!(form, NormalForm::Nfc | NormalForm::Nfkc) {
        compose(&mut out);
    }
    out
}

/// Normalizes a `str` to a form.
pub fn normalize_str(form: NormalForm, input: &str) -> String {
    let codes: Vec<u32> = input.chars().map(u32::from).collect();
    // A code point that the tables give is a Unicode scalar value, so the conversion does not fail.
    normalize(form, &codes).into_iter().filter_map(char::from_u32).collect()
}

fn decompose(c: u32, compat: bool, out: &mut Vec<u32>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&c) {
        let s = c - S_BASE;
        out.push(L_BASE + s / N_COUNT);
        out.push(V_BASE + (s % N_COUNT) / T_COUNT);
        if !s.is_multiple_of(T_COUNT) {
            out.push(T_BASE + s % T_COUNT);
        }
        return;
    }
    if let Ok(i) = DECOMPOSITION.binary_search_by_key(&c, |e| e.0) {
        let (_, is_compat, start, len) = DECOMPOSITION[i];
        if compat || !is_compat {
            let start = usize::from(start);
            for &d in &DECOMPOSITION_CHARS[start..start + usize::from(len)] {
                decompose(d, compat, out);
            }
            return;
        }
    }
    out.push(c);
}

/// The canonical ordering: a stable sort of each run of characters with a combining class above zero.
fn reorder(s: &mut [u32]) {
    let mut i = 0;
    while i < s.len() {
        if combining_class(s[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < s.len() && combining_class(s[i]) != 0 {
            i += 1;
        }
        s[start..i].sort_by_key(|&c| combining_class(c));
    }
}

fn pair(first: u32, second: u32) -> Option<u32> {
    if (L_BASE..L_BASE + L_COUNT).contains(&first) && (V_BASE..V_BASE + V_COUNT).contains(&second) {
        return Some(S_BASE + ((first - L_BASE) * V_COUNT + (second - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&first)
        && (first - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&second)
    {
        return Some(first + (second - T_BASE));
    }
    COMPOSITION
        .binary_search_by_key(&(first, second), |e| (e.0, e.1))
        .ok()
        .map(|i| COMPOSITION[i].2)
}

/// The canonical composition. A character joins the last starter if no character between them blocks it.
fn compose(s: &mut Vec<u32>) {
    let mut starter: Option<usize> = None;
    let mut last_class = 0u8;
    let mut out = 0;
    for i in 0..s.len() {
        let c = s[i];
        let class = combining_class(c);
        if let Some(st) = starter
            && !(out > st + 1 && (last_class == 0 || last_class >= class))
            && let Some(composite) = pair(s[st], c)
        {
            s[st] = composite;
            continue;
        }
        if class == 0 {
            starter = Some(out);
        }
        last_class = class;
        s[out] = c;
        out += 1;
    }
    s.truncate(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hangul_and_compatibility() {
        assert_eq!(normalize_str(NormalForm::Nfd, "\u{AC00}"), "\u{1100}\u{1161}");
        assert_eq!(normalize_str(NormalForm::Nfc, "\u{1100}\u{1161}\u{11A8}"), "\u{AC01}");
        assert_eq!(normalize_str(NormalForm::Nfkc, "\u{2168}"), "IX");
        assert_eq!(normalize_str(NormalForm::Nfc, "\u{2168}"), "\u{2168}");
        assert_eq!(normalize_str(NormalForm::Nfc, "e\u{301}"), "\u{E9}");
        // A character that is not a scalar value passes through.
        assert_eq!(normalize(NormalForm::Nfkc, &[0x11_0000, 0xD800]), [0x11_0000, 0xD800]);
    }
}

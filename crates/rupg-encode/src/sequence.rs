//! Whether a string holds some pieces in order, answered on its FSST codes without decompressing it.
//!
//! `LIKE '%special%requests%'` asks whether `special` is somewhere in a string and `requests` is
//! somewhere after it. Finding each piece as soon as it can be found is never worse than finding it
//! later, so the question is a walk over the bytes with one automaton per piece, each one handing
//! over to the next when its piece is complete. Laid end to end that is one automaton with a state
//! for every byte of every piece and one more for having found them all, and a string holds the
//! pieces when its walk ends in that last state.
//!
//! A compressed string is codes, and each code stands for up to eight bytes. Walking a code is
//! walking its bytes, and since the bytes a code stands for do not change within a chunk, where a
//! code takes each state can be worked out once per chunk and then looked up. A string is then one
//! lookup per code rather than a decompression, a copy and a search over the bytes, and the lookups
//! are filled in only for the states a string actually reached. See `spec/perf/44-like-on-codes.md`.
//!
//! Walking is still a step per code for every string, and a filter that keeps nearly every row
//! walks nearly every string to the end. [`grams`] is the sketch that saves the walk: a bit for
//! each run of three bytes a string holds, hashed into sixty four. A string that holds a piece
//! holds every run of three in it, so a string whose sketch lacks one of the piece's bits cannot
//! hold the piece, and only the strings whose sketch has all of them are walked. The writer keeps
//! one sketch per row of a long text column, see `spec/graph/12-the-order-the-suite-asks-for.md`.

// Lifted from rudb crates/rudb-encoding/src/sequence.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error.

use rupg_common::{Error, Result};

use crate::fsst::{ESCAPE, SymbolTable};

/// Marks a code whose step from a state has not been worked out yet. Never a state, because
/// [`Sequence::new`] refuses pieces that would need this many.
const UNKNOWN: u8 = u8::MAX;

/// The automaton for some pieces that have to appear in order.
#[derive(Debug, Clone)]
pub struct Sequence {
    /// Where a byte takes each state, 256 entries a state. The last state is the one every piece
    /// has been found in, and every byte leaves it where it is.
    next: Vec<u8>,
    /// That last state.
    done: u8,
    /// The [`grams`] bits every string that holds the pieces has.
    needs: u64,
}

/// The bit a run of three bytes sets in a sketch.
#[inline]
fn gram(a: u8, b: u8, c: u8) -> u64 {
    let run = u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c);
    1 << (run.wrapping_mul(0x9E37_79B1) >> 26)
}

/// The sketch of `text`: a bit for every run of three bytes in it, hashed into sixty four.
///
/// Stored by the writer, one per row, and read against [`Sequence::needs`]. The hash is part of the
/// file format, since a sketch written by one build is read by the next.
#[must_use]
pub fn grams(text: &[u8]) -> u64 {
    text.windows(3).fold(0, |bits, run| bits | gram(run[0], run[1], run[2]))
}

impl Sequence {
    /// The automaton for `pieces` in this order, or `None` when there is nothing to find or too much.
    ///
    /// Empty pieces are dropped, since finding nothing is always done. With nothing left every string
    /// holds the pieces, which is not a question worth a walk, and pieces longer than 254 bytes
    /// between them would need more states than a byte counts.
    #[must_use]
    pub fn new(pieces: &[&[u8]]) -> Option<Self> {
        let pieces: Vec<&[u8]> = pieces.iter().copied().filter(|piece| !piece.is_empty()).collect();
        let total: usize = pieces.iter().map(|piece| piece.len()).sum();
        if pieces.is_empty() || total >= usize::from(UNKNOWN) {
            return None;
        }
        let states = total + 1;
        let mut next = vec![0_u8; states * 256];
        // The usual table for finding one string in another, one piece at a time, with each piece's
        // states numbered after the ones before it. The state one past a piece's last is the next
        // piece's first, so completing a piece hands over to the next without a case of its own.
        let mut base = 0;
        for piece in &pieces {
            let first = usize::from(piece[0]);
            for byte in 0..256 {
                next[base * 256 + byte] = base as u8;
            }
            next[base * 256 + first] = (base + 1) as u8;
            let mut restart = base;
            for (offset, &byte) in piece.iter().enumerate().skip(1) {
                let state = base + offset;
                let (before, row) = next.split_at_mut(state * 256);
                row[..256].copy_from_slice(&before[restart * 256..restart * 256 + 256]);
                row[usize::from(byte)] = (state + 1) as u8;
                restart = usize::from(next[restart * 256 + usize::from(byte)]);
            }
            base += piece.len();
        }
        for byte in 0..256 {
            next[total * 256 + byte] = total as u8;
        }
        let needs = pieces.iter().fold(0, |bits, piece| bits | grams(piece));
        Some(Self { next, done: total as u8, needs })
    }

    /// The sketch bits a string has to have to hold the pieces. A string whose [`grams`] lack any
    /// of them does not hold the pieces, and one that has them all may.
    ///
    /// Zero when no piece is three bytes long, which every string passes.
    #[must_use]
    pub fn needs(&self) -> u64 {
        self.needs
    }

    fn states(&self) -> usize {
        usize::from(self.done) + 1
    }

    /// Whether `text` holds the pieces in order.
    #[must_use]
    pub fn holds(&self, text: &[u8]) -> bool {
        let mut state = 0_u8;
        for &byte in text {
            state = self.next[usize::from(state) * 256 + usize::from(byte)];
            if state == self.done {
                return true;
            }
        }
        false
    }

    /// A walker over strings compressed against `table`.
    #[must_use]
    pub fn over<'a>(&'a self, table: &'a SymbolTable) -> Coded<'a> {
        Coded { sequence: self, table, steps: vec![UNKNOWN; self.states() * 256] }
    }
}

/// [`Sequence`] over the codes of one symbol table, with where each code takes each state worked
/// out the first time it is needed.
#[derive(Debug)]
pub struct Coded<'a> {
    sequence: &'a Sequence,
    table: &'a SymbolTable,
    /// Where a code takes each state, 256 entries a state, or [`UNKNOWN`] for not yet worked out.
    /// The entry for [`ESCAPE`] is never used, since what an escape does depends on the byte after it.
    steps: Vec<u8>,
}

impl Coded<'_> {
    /// Whether the string compressed to `codes` holds the pieces in order.
    ///
    /// # Errors
    ///
    /// If a code is not in the table or an escape is the last code.
    pub fn holds(&mut self, codes: &[u8]) -> Result<bool> {
        let done = self.sequence.done;
        let mut state = 0_u8;
        let mut at = 0;
        while at < codes.len() {
            let code = codes[at];
            at += 1;
            let slot = usize::from(state) * 256 + usize::from(code);
            state = if code == ESCAPE {
                let Some(&byte) = codes.get(at) else {
                    return Err(Error::internal("a compressed string ends in an escape"));
                };
                at += 1;
                self.sequence.next[usize::from(state) * 256 + usize::from(byte)]
            } else if self.steps[slot] != UNKNOWN {
                self.steps[slot]
            } else {
                self.learn(state, code)?
            };
            if state == done {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cold]
    fn learn(&mut self, from: u8, code: u8) -> Result<u8> {
        let Some((bytes, len)) = self.table.symbol(code) else {
            return Err(Error::internal(format!("code {code} is not in the table")));
        };
        let mut state = from;
        for &byte in &bytes[..len] {
            state = self.sequence.next[usize::from(state) * 256 + usize::from(byte)];
        }
        self.steps[usize::from(from) * 256 + usize::from(code)] = state;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces found one after another with a plain search, which is what `LIKE` does.
    fn searched(text: &[u8], pieces: &[&[u8]]) -> bool {
        let mut rest = text;
        for piece in pieces {
            if piece.is_empty() {
                continue;
            }
            match rest.windows(piece.len()).position(|window| window == *piece) {
                Some(at) => rest = &rest[at + piece.len()..],
                None => return false,
            }
        }
        true
    }

    /// A string that holds the pieces always has the bits they need, so the sketch never turns
    /// away a string the walk would have kept.
    #[test]
    fn a_string_that_holds_the_pieces_has_every_bit_they_need() {
        let cases: [&[&[u8]]; 4] =
            [&[b"special", b"requests"], &[b"furiously"], &[b"ab"], &[b"aab", b"sts", b"\xc3\xa9"]];
        for pieces in cases {
            let sequence = Sequence::new(pieces).expect("an automaton");
            let mut kept = 0;
            for text in texts() {
                let has = grams(&text) & sequence.needs() == sequence.needs();
                if searched(&text, pieces) {
                    assert!(has, "{:?} holds {pieces:?} and its sketch says not", text);
                    kept += 1;
                }
            }
            assert!(kept > 0, "{pieces:?} is held somewhere, so the test tests something");
        }
        assert_eq!(Sequence::new(&[b"ab"]).expect("an automaton").needs(), 0);
        assert_eq!(grams(b"ab"), 0, "no run of three");
        assert_ne!(grams(b"special") & grams(b"requests"), grams(b"special"));
    }

    fn texts() -> Vec<Vec<u8>> {
        let words = [
            "special",
            "requests",
            "spec",
            "specia",
            "ial",
            "requ",
            "sts",
            "the",
            "furiously",
            "aaa",
            "aab",
            "ab",
            "é",
            "ü",
            "",
            "s",
        ];
        let mut texts = Vec::new();
        let mut seed = 7_u64;
        for _ in 0..3000 {
            let mut text = Vec::new();
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            for step in 0..(seed >> 60) {
                let pick = (seed >> (step * 4 % 56)) as usize % words.len();
                text.extend_from_slice(words[pick].as_bytes());
                if (seed >> (step % 60)) & 1 == 1 {
                    text.push(b' ');
                }
            }
            texts.push(text);
        }
        texts
    }

    #[test]
    fn the_walk_over_bytes_and_the_walk_over_codes_agree_with_a_search() {
        let texts = texts();
        let samples: Vec<&[u8]> = texts.iter().map(Vec::as_slice).collect();
        let table = SymbolTable::train(&samples);
        let patterns: [&[&[u8]]; 7] = [
            &[b"special", b"requests"],
            &[b"aab"],
            &[b"ab", b"ab", b"ab"],
            &[b"s", b"s"],
            &["é".as_bytes(), b"ial"],
            &[b"", b"spec", b""],
            &[b"furiously the", b"sts"],
        ];
        let mut found = 0;
        for pieces in patterns {
            let sequence = Sequence::new(pieces).expect("something to find");
            let mut coded = sequence.over(&table);
            for text in &texts {
                let wanted = searched(text, pieces);
                found += usize::from(wanted);
                assert_eq!(sequence.holds(text), wanted, "{pieces:?} in {text:?}");
                let mut codes = Vec::new();
                table.compress(text, &mut codes);
                assert_eq!(coded.holds(&codes).expect("codes"), wanted, "{pieces:?} in {text:?}");
            }
        }
        assert!(found > 1000, "only {found} matches");
    }

    #[test]
    fn nothing_to_find_or_too_much_is_no_automaton() {
        assert!(Sequence::new(&[]).is_none());
        assert!(Sequence::new(&[b"", b""]).is_none());
        assert!(Sequence::new(&[&[b'x'; 255]]).is_none());
        assert!(Sequence::new(&[&[b'x'; 254]]).is_some());
    }
}

// Lifted from rudb crates/rudb-qc-rt/src/like.rs at cd9f9676 (2026-10-05).
// Changes: none.

//! `LIKE` and `ILIKE` against a constant pattern, and a comparison of text against a constant,
//! which the compiled engine answers for a whole morsel or dictionary the same way.
//!
//! The same shapes and the same walk as the first engine's kernel in `rudb-kernels`, so the two
//! engines answer alike. There is no escape character, because the first engine has none.

use memchr::memmem;

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Like {
    pattern: Pattern,
    fold: bool,
}

#[derive(Clone, Debug)]
enum Pattern {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Box<memmem::Finder<'static>>),
    Segments {
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        middles: Vec<memmem::Finder<'static>>,
    },
    General(Vec<char>),
    /// A comparison against a constant, true when the text's order against it, less, equal or
    /// greater, is one `keep` holds. Text is ordered by its bytes, as the first engine orders it.
    Ordered {
        literal: Vec<u8>,
        keep: [bool; 3],
    },
}

impl Like {
    /// Compiles `spelling`. `fold` is `ILIKE`.
    #[must_use]
    pub fn new(spelling: &str, fold: bool) -> Like {
        let spelling = if fold { spelling.to_lowercase() } else { spelling.to_owned() };
        Like { pattern: Pattern::compile(&spelling), fold }
    }

    /// A comparison against `literal`, true for the text that orders before it when `keep[0]`, the
    /// same as it when `keep[1]` and after it when `keep[2]`.
    #[must_use]
    pub fn ordered(literal: &str, keep: [bool; 3]) -> Like {
        Like {
            pattern: Pattern::Ordered { literal: literal.as_bytes().to_vec(), keep },
            fold: false,
        }
    }

    /// Whether `text` matches.
    #[must_use]
    pub fn matches(&self, text: &[u8]) -> bool {
        if self.fold {
            let lower = String::from_utf8_lossy(text).to_lowercase();
            return self.pattern.holds(lower.as_bytes());
        }
        self.pattern.holds(text)
    }

    /// Answers `rows` strings, one byte each into `out`, 1 for a match. Row `at` is
    /// `arena[place(at)]` when `place` has it, and `text(at)` when it does not.
    ///
    /// A pattern with a piece to look for inside the string finds it with one search over the
    /// arena, the way the first engine's kernel searches a chunk's strings laid end to end.
    /// Setting a search up costs about what running it over a short string does, so one search a
    /// row pays that setup for every URL, and one search over the arena pays it once. A match
    /// that runs across the end of one string has no match in that string after it, since that
    /// would end later still, so the search goes on from the start of the next string either way.
    /// A pattern of several pieces looks for its longest middle piece and asks the whole pattern
    /// only of the strings that hold it. The arena is searched as it is when the strings in it
    /// come in row order, which is how a column is decoded, and copied into that order when they
    /// do not. Every other pattern is asked row by row.
    pub fn answer<'t>(
        &self,
        rows: usize,
        arena: &'t [u8],
        place: impl Fn(usize) -> Option<std::ops::Range<usize>>,
        text: impl Fn(usize) -> &'t [u8],
        out: &mut [u8],
    ) {
        let out = &mut out[..rows];
        let bytes = |at: usize| place(at).and_then(|r| arena.get(r)).unwrap_or_else(|| text(at));
        let finder = match (&self.pattern, self.fold) {
            (Pattern::Contains(f), false) => Some(&**f),
            (Pattern::Segments { middles, .. }, false) => {
                middles.iter().max_by_key(|f| f.needle().len())
            }
            _ => None,
        };
        let Some(finder) = finder.filter(|f| !f.needle().is_empty()) else {
            for (at, slot) in out.iter_mut().enumerate() {
                *slot = u8::from(self.matches(bytes(at)));
            }
            return;
        };
        let whole = matches!(self.pattern, Pattern::Contains(_));
        let holds = |text: &[u8]| whole || self.pattern.holds(text);
        // The strings the arena holds in order, and every other one answered on its own.
        let mut spans = Vec::with_capacity(rows);
        let mut ordered = true;
        for (at, slot) in out.iter_mut().enumerate() {
            *slot = 0;
            match place(at).filter(|r| r.end <= arena.len()) {
                Some(r) => {
                    ordered &= spans.last().is_none_or(|&(_, end, _)| end <= r.start);
                    spans.push((r.start, r.end, at));
                }
                None => *slot = u8::from(self.pattern.holds(text(at))),
            }
        }
        let joined;
        let haystack = if ordered {
            arena
        } else {
            let mut all =
                Vec::with_capacity(spans.iter().map(|&(start, end, _)| end - start).sum());
            for span in &mut spans {
                let from = all.len();
                all.extend_from_slice(&arena[span.0..span.1]);
                *span = (from, all.len(), span.2);
            }
            joined = all;
            &joined[..]
        };
        let needle = finder.needle().len();
        let (mut next, mut from) = (0, spans.first().map_or(0, |s| s.0));
        let last = spans.last().map_or(0, |s| s.1);
        while next < spans.len()
            && let Some(found) = finder.find(&haystack[from..last])
        {
            let at = from + found;
            while spans[next].1 <= at {
                next += 1;
            }
            let (start, end, row) = spans[next];
            if at < start {
                // A match in the gap before a string, or across into it, says nothing of it.
                from = start;
                continue;
            }
            if at + needle <= end {
                out[row] = u8::from(holds(&haystack[start..end]));
            }
            from = end;
            next += 1;
            if let Some(&(start, ..)) = spans.get(next) {
                from = start;
            }
        }
    }
}

impl Pattern {
    fn compile(spelling: &str) -> Pattern {
        let plain = |text: &str| !text.contains('%') && !text.contains('_');
        if plain(spelling) {
            return Pattern::Exact(spelling.as_bytes().to_vec());
        }
        if let Some(inner) = spelling.strip_prefix('%').and_then(|rest| rest.strip_suffix('%'))
            && plain(inner)
        {
            return Pattern::Contains(Box::new(memmem::Finder::new(inner).into_owned()));
        }
        if let Some(rest) = spelling.strip_prefix('%')
            && plain(rest)
        {
            return Pattern::Suffix(rest.as_bytes().to_vec());
        }
        if let Some(head) = spelling.strip_suffix('%')
            && plain(head)
        {
            return Pattern::Prefix(head.as_bytes().to_vec());
        }
        if !spelling.contains('_') {
            let mut pieces: Vec<&str> = spelling.split('%').collect();
            if pieces.len() >= 2 {
                let suffix = pieces.pop().unwrap_or_default().as_bytes().to_vec();
                let prefix = pieces.remove(0).as_bytes().to_vec();
                let middles = pieces
                    .into_iter()
                    .filter(|piece| !piece.is_empty())
                    .map(|piece| memmem::Finder::new(piece).into_owned())
                    .collect();
                return Pattern::Segments { prefix, suffix, middles };
            }
        }
        Pattern::General(spelling.chars().collect())
    }

    fn holds(&self, text: &[u8]) -> bool {
        match self {
            Pattern::Exact(p) => text == &p[..],
            Pattern::Prefix(p) => text.starts_with(p),
            Pattern::Suffix(p) => text.ends_with(p),
            Pattern::Contains(f) => f.find(text).is_some(),
            Pattern::Segments { prefix, suffix, middles } => {
                if !text.starts_with(prefix) || !text.ends_with(suffix) {
                    return false;
                }
                let Some(end) = text.len().checked_sub(suffix.len()) else { return false };
                if prefix.len() > end {
                    return false;
                }
                let mut rest = &text[prefix.len()..end];
                for f in middles {
                    let Some(at) = f.find(rest) else { return false };
                    rest = &rest[at + f.needle().len()..];
                }
                true
            }
            Pattern::General(p) => {
                let text: Vec<char> = String::from_utf8_lossy(text).chars().collect();
                walk(&text, p)
            }
            Pattern::Ordered { literal, keep } => keep[(text.cmp(literal) as i8 + 1) as usize],
        }
    }
}

/// The backtracking walk with one resume point, tried wildcard first so a `%` in the text is not
/// taken for the pattern's.
fn walk(text: &[char], pattern: &[char]) -> bool {
    let (mut at, mut against) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while at < text.len() {
        if against < pattern.len() && pattern[against] == '%' {
            star = Some(against);
            resume = at;
            against += 1;
        } else if against < pattern.len()
            && (pattern[against] == '_' || pattern[against] == text[at])
        {
            at += 1;
            against += 1;
        } else if let Some(s) = star {
            against = s + 1;
            resume += 1;
            at = resume;
        } else {
            return false;
        }
    }
    while against < pattern.len() && pattern[against] == '%' {
        against += 1;
    }
    against == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_answers_like_sql() {
        let cases = [
            ("%google%", "http://www.google.com/", true),
            ("%google%", "http://www.yandex.ru/", false),
            ("%.google.%", "http://www.google.com/", true),
            ("abc", "abc", true),
            ("abc%", "abcdef", true),
            ("%def", "abcdef", true),
            ("a%c%e", "abcde", true),
            ("a%c%e", "ace", true),
            ("ab%bc", "abc", false),
            ("a_c", "abc", true),
            ("a_c", "ac", false),
            ("%a%", "b%a", true),
            ("_é_", "aéb", true),
        ];
        for (p, t, want) in cases {
            assert_eq!(Like::new(p, false).matches(t.as_bytes()), want, "{t} LIKE {p}");
        }
        assert!(Like::new("%GOOGLE%", true).matches(b"www.Google.com"));
    }

    #[test]
    fn a_comparison_orders_by_bytes() {
        let not_empty = Like::ordered("", [false, false, true]);
        assert!(!not_empty.matches(b"") && not_empty.matches(b"a"));
        let at_most = Like::ordered("b%", [true, true, false]);
        assert!(at_most.matches(b"b%") && at_most.matches(b"a") && at_most.matches(b"b"));
        assert!(
            !at_most.matches(b"b%a") && !at_most.matches(b"c") && !at_most.matches("é".as_bytes())
        );
    }

    #[test]
    fn a_morsel_answers_as_its_rows_do() {
        let texts = ["", "google", "xgoog", "le.com", "a.google.com/x", "googl", "egoogle", "go"];
        for p in ["%google%", "%goo%le%", "g%e", "%", "%%", "google", "%le", "go%", "_o%"] {
            let like = Like::new(p, false);
            let mut out = vec![9u8; texts.len()];
            // The long ones in an arena in order, with a gap, and then out of order.
            let arena = b"..google...xgoog.le.com..a.google.com/x..egoogle";
            let places =
                [None, Some(2..8), Some(11..16), None, Some(25..39), None, Some(41..48), None];
            let shuffled =
                [None, Some(41..48), Some(2..8), None, Some(25..39), None, Some(11..16), None];
            for (places, texts) in [(&places, texts), (&shuffled, texts)] {
                let texts: Vec<&[u8]> = texts
                    .iter()
                    .zip(places.iter())
                    .map(|(t, place)| place.clone().map_or(t.as_bytes(), |r| &arena[r]))
                    .collect();
                let rows = texts.len();
                like.answer(rows, arena, |at| places[at].clone(), |at| texts[at], &mut out);
                for (t, got) in texts.iter().zip(&out) {
                    assert_eq!(*got == 1, like.matches(t), "{t:?} LIKE {p}");
                }
            }
        }
    }
}

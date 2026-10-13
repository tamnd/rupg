//! The `tsvector` type of text search, from `tsvectorin`, `tsvectorout`, `tsvectorrecv` and `tsvectorsend` of `tsvector.c` and the parser of `tsvector_parser.c`. A value is a sorted list of lexemes with no two equal lexemes, and each lexeme has up to 256 positions in order. A position is a `WordEntryPos` of PostgreSQL: the weight in the two high bits and the position in the 14 low bits.

use std::cmp::Ordering;

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;

/// `MAXSTRLEN`: the maximum number of bytes in a lexeme.
pub const TSVECTOR_MAX_WORD: usize = (1 << 11) - 1;

/// `MAXSTRPOS`: the maximum number of bytes in the lexemes and the positions of a value.
pub const TSVECTOR_MAX_DATA: usize = (1 << 20) - 1;

/// `MAXNUMPOS`: the maximum number of positions of a lexeme.
pub const TSVECTOR_MAX_POSITIONS: usize = 256;

/// `MAXENTRYPOS`: a position is less than this value.
const MAX_ENTRY_POS: u16 = 1 << 14;

/// One lexeme of a `tsvector`, with its positions. A lexeme with no positions has an empty list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lexeme {
    pub word: String,
    pub positions: Vec<u16>,
}

/// `WEP_GETPOS`: the position of a `WordEntryPos`.
pub fn position_of(pos: u16) -> u16 {
    pos & 0x3fff
}

/// `WEP_GETWEIGHT`: the weight of a `WordEntryPos`, 3 for `A` down to 0 for `D`.
pub fn weight_of(pos: u16) -> u16 {
    pos >> 14
}

/// `WEP_SETWEIGHT`: the `WordEntryPos` with a different weight.
pub fn with_weight(pos: u16, weight: u16) -> u16 {
    position_of(pos) | ((weight & 3) << 14)
}

/// `LIMITPOS`: a position above the maximum is the maximum.
pub fn limit_position(pos: i32) -> u16 {
    let limited = if pos >= i32::from(MAX_ENTRY_POS) { i32::from(MAX_ENTRY_POS) - 1 } else { pos };
    u16::try_from(limited & 0x3fff).unwrap_or(0)
}

/// `tsCompareString` with no prefix match: the bytes of the two lexemes. The result is the difference of the first two bytes that are not equal, as the `memcmp` of the C library gives for one byte, or -1 or 1 when one lexeme starts with the other.
pub fn ts_compare_string(a: &[u8], b: &[u8]) -> i32 {
    if a.is_empty() {
        return if b.is_empty() { 0 } else { -1 };
    }
    if b.is_empty() {
        return 1;
    }
    if let Some((x, y)) = a.iter().zip(b).find(|(x, y)| x != y) {
        return i32::from(*x) - i32::from(*y);
    }
    match a.len().cmp(&b.len()) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// The order of two lexemes in a value.
fn word_order(a: &str, b: &str) -> Ordering {
    ts_compare_string(a.as_bytes(), b.as_bytes()).cmp(&0)
}

/// The number of bytes after the lexeme entries of a value: the bytes of each lexeme and, for a lexeme with positions, a pad byte to an even offset, a count and the positions.
pub fn tsvector_data_len(lexemes: &[Lexeme]) -> usize {
    lexemes.iter().fold(0, |len, lexeme| {
        let len = len + lexeme.word.len();
        if lexeme.positions.is_empty() {
            len
        } else {
            len.next_multiple_of(2) + 2 + 2 * lexeme.positions.len()
        }
    })
}

/// `VARSIZE` of a value: the header of 4 bytes, the count, an entry of 4 bytes for each lexeme and the data.
pub fn tsvector_size(lexemes: &[Lexeme]) -> usize {
    8 + 4 * lexemes.len() + tsvector_data_len(lexemes)
}

/// `silly_cmp_tsvector`: the size, the number of lexemes, then for each lexeme in order whether it has positions, the bytes, the number of positions, each position and each weight. A lexeme with positions, more positions, a larger position and a larger weight come first. The result is the value of `tsvector_cmp`.
pub fn tsvector_cmp(a: &[Lexeme], b: &[Lexeme]) -> i32 {
    let sign = |order: Ordering| match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    let order = tsvector_size(a).cmp(&tsvector_size(b)).then(a.len().cmp(&b.len()));
    if order.is_ne() {
        return sign(order);
    }
    for (x, y) in a.iter().zip(b) {
        let has = !x.positions.is_empty();
        if has != !y.positions.is_empty() {
            return if has { -1 } else { 1 };
        }
        let words = ts_compare_string(x.word.as_bytes(), y.word.as_bytes());
        if words != 0 {
            return words;
        }
        let order = y.positions.len().cmp(&x.positions.len());
        if order.is_ne() {
            return sign(order);
        }
        for (&p, &q) in x.positions.iter().zip(&y.positions) {
            let order =
                position_of(q).cmp(&position_of(p)).then_with(|| weight_of(q).cmp(&weight_of(p)));
            if order.is_ne() {
                return sign(order);
            }
        }
    }
    0
}

/// `uniquePos`: the positions in order with no two equal positions. Of two equal positions the larger weight stays. The list stops at 256 positions or at the maximum position.
pub fn unique_positions(mut positions: Vec<u16>) -> Vec<u16> {
    if positions.len() <= 1 {
        return positions;
    }
    positions.sort_by_key(|&pos| position_of(pos));
    let mut out: Vec<u16> = Vec::with_capacity(positions.len());
    for pos in positions {
        match out.last_mut() {
            Some(last) if position_of(*last) == position_of(pos) => {
                if weight_of(pos) > weight_of(*last) {
                    *last = with_weight(*last, weight_of(pos));
                }
            }
            _ => {
                out.push(pos);
                if out.len() >= TSVECTOR_MAX_POSITIONS || position_of(pos) == MAX_ENTRY_POS - 1 {
                    break;
                }
            }
        }
    }
    out
}

/// `uniqueentry`: the lexemes in order, with the positions of equal lexemes in one lexeme.
fn unique_entries(mut lexemes: Vec<Lexeme>) -> Vec<Lexeme> {
    lexemes.sort_by(|a, b| word_order(&a.word, &b.word));
    let mut out: Vec<Lexeme> = Vec::with_capacity(lexemes.len());
    for lexeme in lexemes {
        match out.last_mut() {
            Some(last) if last.word == lexeme.word => last.positions.extend(lexeme.positions),
            _ => out.push(lexeme),
        }
    }
    for lexeme in &mut out {
        lexeme.positions = unique_positions(std::mem::take(&mut lexeme.positions));
    }
    out
}

/// The error of a value that is too large.
fn too_long(len: usize) -> TypeError {
    TypeError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("string is too long for tsvector ({len} bytes, max {TSVECTOR_MAX_DATA} bytes)"),
    )
}

/// The error of a lexeme that is too long.
pub fn word_too_long(len: usize) -> TypeError {
    TypeError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("word is too long ({len} bytes, max {TSVECTOR_MAX_WORD} bytes)"),
    )
}

/// `isspace` of C for the byte of a character. A character that is not ASCII is not a space.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}')
}

/// The states of `gettoken_tsvector`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    WaitWord,
    WaitEndWord,
    WaitNextChar,
    WaitEndComplex,
    WaitCharComplex,
    WaitPosInfo,
    InPosInfo,
    WaitPosDelim,
}

/// The parser of the text of a `tsvector`.
struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn syntax(&self) -> TypeError {
        TypeError::new(
            SqlState::SYNTAX_ERROR,
            format!("syntax error in tsvector: \"{}\"", self.text),
        )
    }

    /// `atoi` of the digits at the cursor: the number saturates at the range of `long`, then the conversion to `int` keeps the low 32 bits.
    fn atoi(&self) -> i32 {
        let digits = self.text[self.at..].bytes().take_while(u8::is_ascii_digit);
        let value =
            digits.fold(0i64, |v, d| v.saturating_mul(10).saturating_add(i64::from(d - b'0')));
        u32::try_from(value & 0xffff_ffff).unwrap_or(0).cast_signed()
    }

    /// `gettoken_tsvector`: the next lexeme and its positions, or `None` at the end of the text.
    fn token(&mut self) -> Result<Option<Lexeme>, TypeError> {
        let mut word = String::new();
        let mut positions: Vec<u16> = Vec::new();
        let mut state = State::WaitWord;
        let mut old = State::WaitEndWord;
        loop {
            let c = self.text[self.at..].chars().next();
            match state {
                State::WaitWord => match c {
                    None => return Ok(None),
                    Some('\'') => state = State::WaitEndComplex,
                    Some('\\') => {
                        state = State::WaitNextChar;
                        old = State::WaitEndWord;
                    }
                    Some(c) if is_space(c) => {}
                    Some(c) => {
                        word.push(c);
                        state = State::WaitEndWord;
                    }
                },
                State::WaitNextChar => match c {
                    None => {
                        return Err(TypeError::new(
                            SqlState::SYNTAX_ERROR,
                            format!("there is no escaped character: \"{}\"", self.text),
                        ));
                    }
                    Some(c) => {
                        word.push(c);
                        state = old;
                    }
                },
                State::WaitEndWord => match c {
                    Some('\\') => {
                        state = State::WaitNextChar;
                        old = State::WaitEndWord;
                    }
                    None => break,
                    Some(c) if is_space(c) => break,
                    Some(':') => state = State::InPosInfo,
                    Some(c) => word.push(c),
                },
                State::WaitEndComplex => match c {
                    Some('\'') => state = State::WaitCharComplex,
                    Some('\\') => {
                        state = State::WaitNextChar;
                        old = State::WaitEndComplex;
                    }
                    None => return Err(self.syntax()),
                    Some(c) => word.push(c),
                },
                State::WaitCharComplex => {
                    if c == Some('\'') {
                        word.push('\'');
                        state = State::WaitEndComplex;
                    } else {
                        if word.is_empty() {
                            return Err(self.syntax());
                        }
                        // The character after the quote is the first character of the next state.
                        state = State::WaitPosInfo;
                        continue;
                    }
                }
                State::WaitPosInfo => {
                    if c != Some(':') {
                        break;
                    }
                    state = State::InPosInfo;
                }
                State::InPosInfo => match c {
                    Some(c) if c.is_ascii_digit() => {
                        let pos = limit_position(self.atoi());
                        if pos == 0 {
                            return Err(TypeError::new(
                                SqlState::SYNTAX_ERROR,
                                format!("wrong position info in tsvector: \"{}\"", self.text),
                            ));
                        }
                        positions.push(pos);
                        state = State::WaitPosDelim;
                    }
                    _ => return Err(self.syntax()),
                },
                State::WaitPosDelim => {
                    let weight = match c {
                        Some(',') => {
                            state = State::InPosInfo;
                            None
                        }
                        Some('a' | 'A' | '*') => Some(3),
                        Some('b' | 'B') => Some(2),
                        Some('c' | 'C') => Some(1),
                        Some('d' | 'D') => Some(0),
                        None => break,
                        Some(c) if is_space(c) => break,
                        Some(c) if c.is_ascii_digit() => None,
                        Some(_) => return Err(self.syntax()),
                    };
                    if let (Some(weight), Some(last)) = (weight, positions.last_mut()) {
                        if weight_of(*last) != 0 {
                            return Err(self.syntax());
                        }
                        *last = with_weight(*last, weight);
                    }
                }
            }
            self.at += c.map_or(0, char::len_utf8);
        }
        Ok(Some(Lexeme { word, positions }))
    }
}

/// `tsvectorin`: lexemes with spaces between them. A lexeme in quotes can have spaces, two quotes are one quote, and a backslash takes the next character as it is. After a colon come the positions with commas between them, each a number from 1 with an optional weight `A`, `B`, `C` or `D`. A position above 16383 is 16383.
///
/// # Errors
///
/// `42601` for bad syntax and a position of 0, `54000` for a lexeme or a value that is too long.
pub fn tsvector_parse(text: &str) -> Result<Vec<Lexeme>, TypeError> {
    let mut parser = Parser { text, at: 0 };
    let mut lexemes = Vec::new();
    let mut total = 0;
    while let Some(lexeme) = parser.token()? {
        if lexeme.word.len() > TSVECTOR_MAX_WORD {
            return Err(word_too_long(lexeme.word.len()));
        }
        if total > TSVECTOR_MAX_DATA {
            return Err(too_long(total));
        }
        total += lexeme.word.len();
        lexemes.push(lexeme);
    }
    let lexemes = unique_entries(lexemes);
    let len = tsvector_data_len(&lexemes);
    if len > TSVECTOR_MAX_DATA {
        return Err(too_long(len));
    }
    Ok(lexemes)
}

/// `tsvectorout`: each lexeme in quotes with a quote and a backslash written twice, then a colon and the positions with their weights, with spaces between the lexemes.
pub fn tsvector_deparse(lexemes: &[Lexeme], out: &mut Vec<u8>) {
    for (i, lexeme) in lexemes.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.push(b'\'');
        for &b in lexeme.word.as_bytes() {
            if b == b'\'' || b == b'\\' {
                out.push(b);
            }
            out.push(b);
        }
        out.push(b'\'');
        for (j, &pos) in lexeme.positions.iter().enumerate() {
            out.push(if j == 0 { b':' } else { b',' });
            out.extend_from_slice(position_of(pos).to_string().as_bytes());
            match weight_of(pos) {
                3 => out.push(b'A'),
                2 => out.push(b'B'),
                1 => out.push(b'C'),
                _ => {}
            }
        }
    }
}

/// `tsvectorsend`: the number of lexemes, then each lexeme with a zero byte after it, the number of its positions and the positions.
pub fn tsvector_send(lexemes: &[Lexeme], out: &mut Vec<u8>) {
    let count = i32::try_from(lexemes.len()).unwrap_or(i32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for lexeme in lexemes {
        out.extend_from_slice(lexeme.word.as_bytes());
        out.push(0);
        let npos = u16::try_from(lexeme.positions.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&npos.to_be_bytes());
        for pos in &lexeme.positions {
            out.extend_from_slice(&pos.to_be_bytes());
        }
    }
}

/// The error of `elog` in `tsvectorrecv`.
fn invalid(message: &str) -> TypeError {
    TypeError::new(SqlState::INTERNAL_ERROR, message.to_string())
}

/// `tsvectorrecv`: the binary form of [`tsvector_send`]. The lexemes are put in order, and two equal lexemes stay as they are.
///
/// # Errors
///
/// `08P01` for a value that is too short or a lexeme with no zero byte after it, `22021` for a lexeme that is not UTF-8, and `XX000` for a bad count, an empty or a long lexeme, too many positions or positions out of order.
pub fn tsvector_recv(recv: &mut Recv<'_>) -> Result<Vec<Lexeme>, TypeError> {
    let count = recv.i32()?;
    let count = match usize::try_from(count) {
        Ok(count) if count <= TSVECTOR_MAX_DATA => count,
        _ => return Err(invalid("invalid size of tsvector")),
    };
    let mut lexemes: Vec<Lexeme> = Vec::with_capacity(count.min(recv.remaining()));
    let mut len = 0;
    let mut sorted = true;
    for _ in 0..count {
        let word = recv.cstring()?;
        let npos = usize::from(recv.i16()?.cast_unsigned());
        if word.is_empty() {
            return Err(invalid("invalid tsvector: empty lexeme"));
        }
        if word.len() > TSVECTOR_MAX_WORD {
            return Err(invalid("invalid tsvector: lexeme too long"));
        }
        if len > TSVECTOR_MAX_DATA {
            return Err(invalid("invalid tsvector: maximum total lexeme length exceeded"));
        }
        if npos > TSVECTOR_MAX_POSITIONS {
            return Err(invalid("unexpected number of tsvector positions"));
        }
        len += word.len();
        if lexemes.last().is_some_and(|last| word_order(word, &last.word).is_le()) {
            sorted = false;
        }
        let mut positions: Vec<u16> = Vec::with_capacity(npos);
        for _ in 0..npos {
            let pos = recv.i16()?.cast_unsigned();
            if positions.last().is_some_and(|&last| position_of(pos) <= position_of(last)) {
                return Err(invalid("position information is misordered"));
            }
            positions.push(pos);
        }
        if npos > 0 {
            len = len.next_multiple_of(2) + 2 + 2 * npos;
        }
        lexemes.push(Lexeme { word: word.to_string(), positions });
    }
    if len > TSVECTOR_MAX_DATA {
        return Err(invalid("invalid tsvector: maximum total lexeme length exceeded"));
    }
    if !sorted {
        lexemes.sort_by(|a, b| word_order(&a.word, &b.word));
    }
    Ok(lexemes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> String {
        let lexemes = tsvector_parse(input).expect("parse");
        let mut out = Vec::new();
        tsvector_deparse(&lexemes, &mut out);
        String::from_utf8(out).expect("utf8")
    }

    fn error(input: &str) -> (String, String) {
        let error = tsvector_parse(input).expect_err("error");
        (error.sqlstate.to_string(), error.message)
    }

    #[test]
    fn parse_and_deparse() {
        assert_eq!(
            text("a fat cat sat on a mat and ate a fat rat"),
            "'a' 'and' 'ate' 'cat' 'fat' 'mat' 'on' 'rat' 'sat'"
        );
        assert_eq!(
            text(r"the 'quick brown' fox's \\ tail"),
            r"'\\' 'fox''s' 'quick brown' 'tail' 'the'"
        );
        assert_eq!(
            text("a:1 fat:2,4 cat:3A b:5B,6C,7D x:3,1,2,1"),
            "'a':1 'b':5B,6C,7 'cat':3A 'fat':2,4 'x':1,2,3"
        );
        assert_eq!(text("a:1A,1B,1C a:1 b:2C,2A"), "'a':1A 'b':2A");
        assert_eq!(text("z:5 y z:1,3B"), "'y' 'z':1,3B,5");
        assert_eq!(text("   "), "");
        assert_eq!(text(r"'it''s' 'a\'b' 'c\\d' e\ f"), r"'a''b' 'c\\d' 'e f' 'it''s'");
        assert_eq!(text("a:1*,2a,3b"), "'a':1A,2A,3B");
        assert_eq!(
            text("a:20000 b:16383 c:16384 d:99999999999"),
            "'a':16383 'b':16383 'c':16383 'd':16383"
        );
        assert_eq!(text("a:1A2"), "'a':1A");
        assert_eq!(text("b:3DA"), "'b':3A");
        assert_eq!(text("a:1DD"), "'a':1");
        assert_eq!(text("d:5,5A,5,4C"), "'d':4C,5A");
        assert_eq!(text("'a'b"), "'a' 'b'");
        assert_eq!(text("a'b'"), "'a''b'''");
        assert_eq!(text("'a':1 'a'"), "'a':1");
        assert_eq!(text("'a'  :1"), "':1' 'a'");
        assert_eq!(text(":1"), "':1'");
        assert_eq!(text("а б в ä é 中文:1"), "'ä' 'é' 'а' 'б' 'в' '中文':1");
    }

    #[test]
    fn parse_errors() {
        let syntax =
            |input: &str| ("42601".to_string(), format!("syntax error in tsvector: \"{input}\""));
        for input in ["a:1A2 b:3DA c:4,5x", "a:", "a:1AB", "'abc", "a: 1", "''", "a:1,2,3x", "a:1,"]
        {
            assert_eq!(error(input), syntax(input), "{input}");
        }
        for input in ["a:0", "a:4294967296"] {
            assert_eq!(
                error(input),
                ("42601".to_string(), format!("wrong position info in tsvector: \"{input}\""))
            );
        }
        assert_eq!(
            error(r"abc\"),
            ("42601".to_string(), r#"there is no escaped character: "abc\""#.to_string())
        );
        assert_eq!(
            error(&"x".repeat(2048)),
            ("54000".to_string(), "word is too long (2048 bytes, max 2047 bytes)".to_string())
        );
        assert!(tsvector_parse(&"x".repeat(2047)).is_ok());
    }

    #[test]
    fn positions() {
        let many: Vec<String> = (1..=300).map(|i| i.to_string()).collect();
        let lexemes = tsvector_parse(&format!("a:{}", many.join(","))).expect("parse");
        assert_eq!(lexemes[0].positions.len(), 256);
        assert_eq!(lexemes[0].positions[255], 256);
    }

    #[test]
    fn order() {
        let v = |input: &str| tsvector_parse(input).expect("parse");
        let mut values: Vec<Vec<Lexeme>> =
            ["b", "a:1", "ab", "a b", "", "a:1A", "a:1,2", "aaa"].iter().map(|s| v(s)).collect();
        values.sort_by(|a, b| tsvector_cmp(a, b).cmp(&0));
        let texts: Vec<String> = values
            .iter()
            .map(|value| {
                let mut out = Vec::new();
                tsvector_deparse(value, &mut out);
                String::from_utf8(out).expect("utf8")
            })
            .collect();
        assert_eq!(texts, ["", "'b'", "'ab'", "'aaa'", "'a':1A", "'a':1", "'a' 'b'", "'a':1,2"]);
        assert_eq!(tsvector_cmp(&v("a"), &v("c")), -2);
        assert_eq!(tsvector_cmp(&v("c"), &v("a")), 2);
        assert_eq!(tsvector_cmp(&v("a:1"), &v("a:3")), 1);
        assert_eq!(tsvector_cmp(&v("a:1A"), &v("a:1C")), -1);
        assert_eq!(tsvector_cmp(&v("a:1"), &v("a")), 1);
        assert_eq!(tsvector_cmp(&v("x y"), &v("xy z")), -1);
        assert_eq!(tsvector_cmp(&v("b a c"), &v("c b a")), 0);
    }

    #[test]
    fn binary() {
        let lexemes = tsvector_parse("a:1 fat:2,4A cat").expect("parse");
        let mut out = Vec::new();
        tsvector_send(&lexemes, &mut out);
        assert_eq!(
            out,
            b"\x00\x00\x00\x03a\x00\x00\x01\x00\x01cat\x00\x00\x00fat\x00\x00\x02\x00\x02\xc0\x04"
        );
        assert_eq!(tsvector_recv(&mut Recv::new(&out)).expect("recv"), lexemes);
        let mut out = Vec::new();
        tsvector_send(&tsvector_parse("x:16383C").expect("parse"), &mut out);
        assert_eq!(out, b"\x00\x00\x00\x01x\x00\x00\x01\x7f\xff");
        let recv = |bytes: &[u8]| tsvector_recv(&mut Recv::new(bytes)).map_err(|e| e.message);
        assert_eq!(recv(b"\xff\xff\xff\xff"), Err("invalid size of tsvector".to_string()));
        assert_eq!(
            recv(b"\x00\x00\x00\x01\x00\x00\x00"),
            Err("invalid tsvector: empty lexeme".to_string())
        );
        assert_eq!(recv(b"\x00\x00\x00\x01ab"), Err("invalid string in message".to_string()));
        assert_eq!(
            recv(b"\x00\x00\x00\x01a\x00\x00\x02\x00\x02\x00\x01"),
            Err("position information is misordered".to_string())
        );
        assert_eq!(
            recv(b"\x00\x00\x00\x01a\x00\x01\x01"),
            Err("unexpected number of tsvector positions".to_string())
        );
        let sorted = recv(b"\x00\x00\x00\x02b\x00\x00\x00a\x00\x00\x00").expect("recv");
        assert_eq!(sorted.iter().map(|l| l.word.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }
}

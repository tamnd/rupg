//! The lexer of `regc_lex.c`: the prefixes `***=`, `***:` and the embedded options, then one token at a time in the context of the parser.

use super::{Code, flags};

/// The largest count of a bound, `DUPMAX`.
pub(super) const DUPMAX: u32 = 255;

/// The largest value of a character, `CHR_MAX`.
const CHR_MAX: u32 = 0x7fff_fffe;

/// A token, as the `nexttype` values of `regcomp.c`. The value of the token is in `Lexer::value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// No token yet, at the start of the pattern.
    Empty,
    /// The end of the pattern.
    Eos,
    /// A plain character.
    Plain,
    /// A digit in a bound.
    Digit,
    /// A `-` between the ends of a range in brackets.
    Range,
    /// The start of a collating element, `[.`.
    Collel,
    /// The start of an equivalence class, `[=`.
    Eclass,
    /// The start of a character class, `[:`.
    Cclass,
    /// The end of a collating element, an equivalence class or a character class.
    End,
    /// The start of a lookaround constraint.
    Lacon,
    /// A back reference.
    Backref,
    /// `\y`, a word boundary.
    Wbdry,
    /// `\Y`, not a word boundary.
    Nwbdry,
    /// `\A`, the start of the string.
    Sbegin,
    /// `\Z`, the end of the string.
    Send,
    /// A class escape such as `\d`.
    Cclasss,
    /// A complemented class escape such as `\D`.
    Cclassc,
    /// An operator character, such as `|`, `*`, `(` or `[`.
    Op(u8),
}

/// The kinds of lookaround constraints, as the value of `Tok::Lacon`.
pub(super) const AHEAD_POS: u32 = 3;
pub(super) const AHEAD_NEG: u32 = 2;
pub(super) const BEHIND_POS: u32 = 1;
pub(super) const BEHIND_NEG: u32 = 0;

/// The lexical contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Con {
    Extended,
    Basic,
    Quote,
    ExtendedBound,
    BasicBound,
    Brack,
    Cel,
    Ecl,
    Ccl,
}

/// The names of the character classes, in the order of `enum char_classes`.
pub(super) const CLASS_NAMES: [&str; 14] = [
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "xdigit", "word",
];

/// The numbers of the classes that the escapes give.
pub(super) const CC_DIGIT: u32 = 5;
pub(super) const CC_SPACE: u32 = 10;
pub(super) const CC_WORD: u32 = 13;

/// The state of the lexer, the scanning part of `struct vars`.
pub(super) struct Lexer<'a> {
    pat: &'a [u32],
    pub(super) now: usize,
    pub(super) cflags: u32,
    con: Con,
    pub(super) tok: Tok,
    pub(super) value: u32,
    last: Tok,
    /// The number of capturing groups that the parser has opened, for the guess between a back reference and an octal escape.
    pub(super) nsubexp: usize,
}

type Step = Result<(), Code>;

fn is_alpha(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_ascii_alphabetic())
}

fn is_digit(c: u32) -> bool {
    (u32::from(b'0')..=u32::from(b'9')).contains(&c)
}

fn is_alnum(c: u32) -> bool {
    is_alpha(c) || is_digit(c)
}

/// `iscspace` in the C locale.
pub(super) fn is_space(c: u32) -> bool {
    matches!(c, 0x09..=0x0d | 0x20)
}

const fn chr(c: u8) -> u32 {
    c as u32
}

impl<'a> Lexer<'a> {
    /// `lexstart`: the prefixes, the context and the first token.
    pub(super) fn start(pat: &'a [u32], cflags: u32) -> Result<Self, Code> {
        let mut lexer = Lexer {
            pat,
            now: 0,
            cflags,
            con: Con::Extended,
            tok: Tok::Empty,
            value: 0,
            last: Tok::Empty,
            nsubexp: 0,
        };
        lexer.prefixes()?;
        lexer.con = if lexer.cflags & flags::QUOTE != 0 {
            Con::Quote
        } else if lexer.cflags & flags::EXTENDED != 0 {
            Con::Extended
        } else {
            Con::Basic
        };
        lexer.tok = Tok::Empty;
        lexer.next()?;
        Ok(lexer)
    }

    fn at_eos(&self) -> bool {
        self.now >= self.pat.len()
    }

    fn have(&self, n: usize) -> bool {
        self.pat.len().saturating_sub(self.now) >= n
    }

    fn peek(&self, at: usize) -> u32 {
        self.pat[self.now + at]
    }

    fn next1(&self, c: u8) -> bool {
        !self.at_eos() && self.peek(0) == chr(c)
    }

    fn next2(&self, a: u8, b: u8) -> bool {
        self.have(2) && self.peek(0) == chr(a) && self.peek(1) == chr(b)
    }

    fn advf(&self) -> bool {
        self.cflags & flags::ADVF != 0
    }

    fn ret(&mut self, tok: Tok) -> Step {
        self.tok = tok;
        Ok(())
    }

    fn retv(&mut self, tok: Tok, value: u32) -> Step {
        self.tok = tok;
        self.value = value;
        Ok(())
    }

    fn op(&mut self, c: u8) -> Step {
        self.ret(Tok::Op(c))
    }

    fn opv(&mut self, c: u8, value: u32) -> Step {
        self.retv(Tok::Op(c), value)
    }

    /// True when the token is `t`.
    pub(super) fn see(&self, t: Tok) -> bool {
        self.tok == t
    }

    /// True when the token is the operator `c`.
    pub(super) fn see_op(&self, c: u8) -> bool {
        self.tok == Tok::Op(c)
    }

    /// `prefixes`: `***?`, `***=`, `***:` and the embedded options of an ARE.
    fn prefixes(&mut self) -> Step {
        if self.cflags & flags::QUOTE != 0 {
            return Ok(());
        }
        if self.have(4)
            && self.peek(0) == chr(b'*')
            && self.peek(1) == chr(b'*')
            && self.peek(2) == chr(b'*')
        {
            match self.peek(3) {
                c if c == chr(b'?') => return Err(Code::BadPat),
                c if c == chr(b'=') => {
                    self.cflags |= flags::QUOTE;
                    self.cflags &= !(flags::ADVANCED | flags::EXPANDED | flags::NEWLINE);
                    self.now += 4;
                    return Ok(());
                }
                c if c == chr(b':') => {
                    self.cflags |= flags::ADVANCED;
                    self.now += 4;
                }
                _ => return Err(Code::BadRpt),
            }
        }
        if self.cflags & flags::ADVANCED != flags::ADVANCED {
            return Ok(());
        }
        if self.have(3) && self.next2(b'(', b'?') && is_alpha(self.peek(2)) {
            self.now += 2;
            while !self.at_eos() && is_alpha(self.peek(0)) {
                match char::from_u32(self.peek(0)).unwrap_or('\0') {
                    'b' => self.cflags &= !(flags::ADVANCED | flags::QUOTE),
                    'c' => self.cflags &= !flags::ICASE,
                    'e' => {
                        self.cflags |= flags::EXTENDED;
                        self.cflags &= !(flags::ADVF | flags::QUOTE);
                    }
                    'i' => self.cflags |= flags::ICASE,
                    'm' | 'n' => self.cflags |= flags::NEWLINE,
                    'p' => {
                        self.cflags |= flags::NLSTOP;
                        self.cflags &= !flags::NLANCH;
                    }
                    'q' => {
                        self.cflags |= flags::QUOTE;
                        self.cflags &= !flags::ADVANCED;
                    }
                    's' => self.cflags &= !flags::NEWLINE,
                    't' => self.cflags &= !flags::EXPANDED,
                    'w' => {
                        self.cflags &= !flags::NLSTOP;
                        self.cflags |= flags::NLANCH;
                    }
                    'x' => self.cflags |= flags::EXPANDED,
                    _ => return Err(Code::BadOpt),
                }
                self.now += 1;
            }
            if !self.next1(b')') {
                return Err(Code::BadOpt);
            }
            self.now += 1;
            if self.cflags & flags::QUOTE != 0 {
                self.cflags &= !(flags::EXPANDED | flags::NEWLINE);
            }
        }
        Ok(())
    }

    /// `next`: the next token.
    pub(super) fn next(&mut self) -> Step {
        loop {
            self.last = self.tok;
            if self.cflags & flags::EXPANDED != 0
                && matches!(
                    self.con,
                    Con::Extended | Con::Basic | Con::ExtendedBound | Con::BasicBound
                )
            {
                self.skip();
            }
            if self.at_eos() {
                return match self.con {
                    Con::Extended | Con::Basic | Con::Quote => self.ret(Tok::Eos),
                    Con::ExtendedBound | Con::BasicBound => Err(Code::Brace),
                    Con::Brack | Con::Cel | Con::Ecl | Con::Ccl => Err(Code::Brack),
                };
            }
            let c = self.peek(0);
            self.now += 1;
            match self.con {
                Con::Basic => return self.brenext(c),
                Con::Extended => {}
                Con::Quote => return self.retv(Tok::Plain, c),
                Con::ExtendedBound | Con::BasicBound => return self.bound(c),
                Con::Brack => return self.bracket(c),
                Con::Cel => return self.end_of(c, b'.'),
                Con::Ecl => return self.end_of(c, b'='),
                Con::Ccl => return self.end_of(c, b':'),
            }
            match char::from_u32(c).unwrap_or('\0') {
                '|' => return self.op(b'|'),
                ch @ ('*' | '+' | '?') => {
                    let op = ch as u8;
                    if self.advf() && self.next1(b'?') {
                        self.now += 1;
                        return self.opv(op, 0);
                    }
                    return self.opv(op, 1);
                }
                '{' => {
                    if self.cflags & flags::EXPANDED != 0 {
                        self.skip();
                    }
                    if self.at_eos() || !is_digit(self.peek(0)) {
                        return self.retv(Tok::Plain, c);
                    }
                    self.con = Con::ExtendedBound;
                    return self.op(b'{');
                }
                '(' => {
                    if self.advf() && self.next1(b'?') {
                        self.now += 1;
                        if self.at_eos() {
                            return Err(Code::BadRpt);
                        }
                        let kind = self.peek(0);
                        self.now += 1;
                        match char::from_u32(kind).unwrap_or('\0') {
                            ':' => return self.opv(b'(', 0),
                            '#' => {
                                while !self.at_eos() && self.peek(0) != chr(b')') {
                                    self.now += 1;
                                }
                                if !self.at_eos() {
                                    self.now += 1;
                                }
                                // A comment is no token, so the last token stays.
                                self.tok = self.last;
                                continue;
                            }
                            '=' => return self.retv(Tok::Lacon, AHEAD_POS),
                            '!' => return self.retv(Tok::Lacon, AHEAD_NEG),
                            '<' => {
                                if self.at_eos() {
                                    return Err(Code::BadRpt);
                                }
                                let kind = self.peek(0);
                                self.now += 1;
                                return match char::from_u32(kind).unwrap_or('\0') {
                                    '=' => self.retv(Tok::Lacon, BEHIND_POS),
                                    '!' => self.retv(Tok::Lacon, BEHIND_NEG),
                                    _ => Err(Code::BadRpt),
                                };
                            }
                            _ => return Err(Code::BadRpt),
                        }
                    }
                    return self.opv(b'(', 1);
                }
                ')' => return self.opv(b')', c),
                '[' => {
                    if let Some(op) = self.word_bracket() {
                        return self.op(op);
                    }
                    self.con = Con::Brack;
                    if self.next1(b'^') {
                        self.now += 1;
                        return self.opv(b'[', 0);
                    }
                    return self.opv(b'[', 1);
                }
                '.' => return self.op(b'.'),
                '^' => return self.op(b'^'),
                '$' => return self.op(b'$'),
                '\\' => {
                    if self.at_eos() {
                        return Err(Code::Escape);
                    }
                }
                _ => return self.retv(Tok::Plain, c),
            }
            // A backslash in an ERE or an ARE.
            if !self.advf() {
                let c = self.peek(0);
                self.now += 1;
                return self.retv(Tok::Plain, c);
            }
            return self.escape();
        }
    }

    /// `[[:<:]]` and `[[:>:]]` after a `[`, the start and the end of a word.
    fn word_bracket(&mut self) -> Option<u8> {
        if self.have(6)
            && self.peek(0) == chr(b'[')
            && self.peek(1) == chr(b':')
            && (self.peek(2) == chr(b'<') || self.peek(2) == chr(b'>'))
            && self.peek(3) == chr(b':')
            && self.peek(4) == chr(b']')
            && self.peek(5) == chr(b']')
        {
            let op = if self.peek(2) == chr(b'<') { b'<' } else { b'>' };
            self.now += 6;
            return Some(op);
        }
        None
    }

    /// A character in a bound.
    fn bound(&mut self, c: u32) -> Step {
        if is_digit(c) {
            return self.retv(Tok::Digit, c - chr(b'0'));
        }
        if c == chr(b',') {
            return self.op(b',');
        }
        if c == chr(b'}') {
            if self.con != Con::ExtendedBound {
                return Err(Code::BadBr);
            }
            self.con = Con::Extended;
            if self.advf() && self.next1(b'?') {
                self.now += 1;
                return self.opv(b'}', 0);
            }
            return self.opv(b'}', 1);
        }
        if c == chr(b'\\') && self.con == Con::BasicBound && self.next1(b'}') {
            self.now += 1;
            self.con = Con::Basic;
            return self.opv(b'}', 1);
        }
        Err(Code::BadBr)
    }

    /// A character in brackets.
    fn bracket(&mut self, c: u32) -> Step {
        match char::from_u32(c).unwrap_or('\0') {
            ']' => {
                if self.last == Tok::Op(b'[') {
                    return self.retv(Tok::Plain, c);
                }
                self.con =
                    if self.cflags & flags::EXTENDED != 0 { Con::Extended } else { Con::Basic };
                self.op(b']')
            }
            '\\' => {
                if !self.advf() {
                    return self.retv(Tok::Plain, c);
                }
                if self.at_eos() {
                    return Err(Code::Escape);
                }
                self.escape()?;
                match self.tok {
                    Tok::Plain | Tok::Cclasss | Tok::Cclassc => Ok(()),
                    _ => Err(Code::Escape),
                }
            }
            '-' => {
                if self.last == Tok::Op(b'[') || self.next1(b']') {
                    self.retv(Tok::Plain, c)
                } else {
                    self.retv(Tok::Range, c)
                }
            }
            '[' => {
                if self.at_eos() {
                    return Err(Code::Brack);
                }
                let kind = self.peek(0);
                self.now += 1;
                match char::from_u32(kind).unwrap_or('\0') {
                    '.' => {
                        self.con = Con::Cel;
                        self.ret(Tok::Collel)
                    }
                    '=' => {
                        self.con = Con::Ecl;
                        self.ret(Tok::Eclass)
                    }
                    ':' => {
                        self.con = Con::Ccl;
                        self.ret(Tok::Cclass)
                    }
                    _ => {
                        self.now -= 1;
                        self.retv(Tok::Plain, c)
                    }
                }
            }
            _ => self.retv(Tok::Plain, c),
        }
    }

    /// A character of a collating element, an equivalence class or a character class, which ends with `close` and `]`.
    fn end_of(&mut self, c: u32, close: u8) -> Step {
        if c == chr(close) && self.next1(b']') {
            self.now += 1;
            self.con = Con::Brack;
            return self.retv(Tok::End, chr(close));
        }
        self.retv(Tok::Plain, c)
    }

    /// `lexescape`: an ARE escape after the backslash.
    fn escape(&mut self) -> Step {
        let c = self.peek(0);
        self.now += 1;
        if !is_alnum(c) {
            return self.retv(Tok::Plain, c);
        }
        match char::from_u32(c).unwrap_or('\0') {
            'a' => self.retv(Tok::Plain, 0x07),
            'A' => self.retv(Tok::Sbegin, 0),
            'b' => self.retv(Tok::Plain, 0x08),
            'B' => self.retv(Tok::Plain, chr(b'\\')),
            'c' => {
                if self.at_eos() {
                    return Err(Code::Escape);
                }
                let c = self.peek(0) & 0o37;
                self.now += 1;
                self.retv(Tok::Plain, c)
            }
            'd' => self.retv(Tok::Cclasss, CC_DIGIT),
            'D' => self.retv(Tok::Cclassc, CC_DIGIT),
            'e' => self.retv(Tok::Plain, 0x1b),
            'f' => self.retv(Tok::Plain, 0x0c),
            'm' => self.op(b'<'),
            'M' => self.op(b'>'),
            'n' => self.retv(Tok::Plain, chr(b'\n')),
            'r' => self.retv(Tok::Plain, chr(b'\r')),
            's' => self.retv(Tok::Cclasss, CC_SPACE),
            'S' => self.retv(Tok::Cclassc, CC_SPACE),
            't' => self.retv(Tok::Plain, chr(b'\t')),
            'u' => self.code(4, 4),
            'U' => self.code(8, 8),
            'v' => self.retv(Tok::Plain, 0x0b),
            'w' => self.retv(Tok::Cclasss, CC_WORD),
            'W' => self.retv(Tok::Cclassc, CC_WORD),
            'x' => self.code(1, 255),
            'y' => self.retv(Tok::Wbdry, 0),
            'Y' => self.retv(Tok::Nwbdry, 0),
            'Z' => self.retv(Tok::Send, 0),
            '1'..='9' => {
                let save = self.now;
                self.now -= 1;
                let n = self.digits(10, 1, 255)?;
                // The guess of regc_lex.c: one digit, or a number that is not more than the groups so far, is a back reference.
                if self.now == save
                    || (n > 0 && usize::try_from(n).is_ok_and(|n| n <= self.nsubexp))
                {
                    return self.retv(Tok::Backref, n);
                }
                self.now = save;
                self.octal()
            }
            '0' => self.octal(),
            _ => Err(Code::Escape),
        }
    }

    /// A character by its hexadecimal code, with `min` to `max` digits.
    fn code(&mut self, min: usize, max: usize) -> Step {
        let c = self.digits(16, min, max)?;
        if c > CHR_MAX {
            return Err(Code::Escape);
        }
        self.retv(Tok::Plain, c)
    }

    /// An octal escape, from the digit before `now`.
    fn octal(&mut self) -> Step {
        self.now -= 1;
        let mut c = self.digits(8, 1, 3)?;
        if c > 0xff {
            // Out of range, so the last digit is not part of the escape.
            self.now -= 1;
            c >>= 3;
        }
        self.retv(Tok::Plain, c)
    }

    /// `lexdigits`: a number in `base` with `min` to `max` digits.
    fn digits(&mut self, base: u32, min: usize, max: usize) -> Result<u32, Code> {
        let mut n: u32 = 0;
        let mut len = 0;
        while len < max && !self.at_eos() {
            let c = self.peek(0);
            self.now += 1;
            let d = char::from_u32(c).and_then(|c| c.to_digit(16));
            match d {
                Some(d) if d < base => n = n.wrapping_mul(base).wrapping_add(d),
                _ => {
                    self.now -= 1;
                    break;
                }
            }
            len += 1;
        }
        if len < min {
            return Err(Code::Escape);
        }
        Ok(n)
    }

    /// `brenext`: the next token of a basic expression.
    fn brenext(&mut self, c: u32) -> Step {
        match char::from_u32(c).unwrap_or('\0') {
            '*' => {
                if matches!(self.last, Tok::Empty | Tok::Op(b'(' | b'^')) {
                    return self.retv(Tok::Plain, c);
                }
                return self.opv(b'*', 1);
            }
            '[' => {
                if let Some(op) = self.word_bracket() {
                    return self.op(op);
                }
                self.con = Con::Brack;
                if self.next1(b'^') {
                    self.now += 1;
                    return self.opv(b'[', 0);
                }
                return self.opv(b'[', 1);
            }
            '.' => return self.op(b'.'),
            '^' => {
                if matches!(self.last, Tok::Empty | Tok::Op(b'(')) {
                    return self.op(b'^');
                }
                return self.retv(Tok::Plain, c);
            }
            '$' => {
                if self.cflags & flags::EXPANDED != 0 {
                    self.skip();
                }
                if self.at_eos() || self.next2(b'\\', b')') {
                    return self.op(b'$');
                }
                return self.retv(Tok::Plain, c);
            }
            '\\' => {}
            _ => return self.retv(Tok::Plain, c),
        }
        if self.at_eos() {
            return Err(Code::Escape);
        }
        let c = self.peek(0);
        self.now += 1;
        match char::from_u32(c).unwrap_or('\0') {
            '{' => {
                self.con = Con::BasicBound;
                self.op(b'{')
            }
            '(' => self.opv(b'(', 1),
            ')' => self.opv(b')', c),
            '<' => self.op(b'<'),
            '>' => self.op(b'>'),
            '1'..='9' => self.retv(Tok::Backref, c - chr(b'0')),
            _ => self.retv(Tok::Plain, c),
        }
    }

    /// `skip`: the white space and the comments of the expanded syntax.
    fn skip(&mut self) {
        loop {
            while !self.at_eos() && is_space(self.peek(0)) {
                self.now += 1;
            }
            if self.at_eos() || self.peek(0) != chr(b'#') {
                break;
            }
            while !self.at_eos() && self.peek(0) != chr(b'\n') {
                self.now += 1;
            }
        }
    }

    /// `scanplain`: the characters of a collating element, an equivalence class or a character class, up to its end token. The result is the position after the last character. An empty name gives the position after the end token, as in `regcomp.c`.
    pub(super) fn scan_plain(&mut self) -> Result<usize, Code> {
        self.next()?;
        let mut end = self.now;
        while self.see(Tok::Plain) {
            end = self.now;
            self.next()?;
        }
        self.next()?;
        Ok(end)
    }

    /// The characters of the pattern from `start` to `end`.
    pub(super) fn slice(&self, start: usize, end: usize) -> &'a [u32] {
        &self.pat[start..end]
    }
}

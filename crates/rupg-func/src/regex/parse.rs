//! The parser of `regcomp.c`: `parse`, `parsebranch`, `parseqatom` and the bracket expressions. It builds a tree of nodes in place of the NFA, and it finds the errors in the same order as `regcomp.c`.

use super::lex::{
    AHEAD_NEG, AHEAD_POS, BEHIND_NEG, BEHIND_POS, CLASS_NAMES, DUPMAX, Lexer, Tok, is_space,
};
use super::tree::{LONGER, SHORTER};
use super::{Code, flags};

/// The count of a bound with no upper limit, `DUPINF`.
pub(super) const DUPINF: u32 = DUPMAX + 1;

/// The kinds of lookaround constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Look {
    Ahead,
    NotAhead,
    Behind,
    NotBehind,
}

/// The constraints that match no character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Assert {
    /// `^`.
    Bol,
    /// `$`.
    Eol,
    /// `\A`.
    Bos,
    /// `\Z`.
    Eos,
    /// `\m` and `[[:<:]]`.
    WordStart,
    /// `\M` and `[[:>:]]`.
    WordEnd,
    /// `\y`.
    Boundary,
    /// `\Y`.
    NotBoundary,
}

/// A node of the tree of a pattern.
#[derive(Clone, Debug)]
pub(super) enum Node {
    /// The empty string.
    Empty,
    /// One character of a set.
    Set(Box<Set>),
    /// A constraint.
    Assert(Assert),
    /// A lookaround constraint.
    Look(Look, Box<Node>),
    /// A group, with the number of its capture, or 0 for a group that does not capture.
    Group(usize, Box<Node>),
    /// A back reference to a capture.
    Backref(usize),
    /// The nodes one after the other.
    Concat(Vec<Node>),
    /// One of the nodes.
    Alt(Vec<Node>),
    /// `min` to `max` times the node, where `max` is `DUPINF` for no limit, with the preference of the quantifier: `LONGER`, `SHORTER` or 0 for `{m}`.
    Repeat(Box<Node>, u32, u32, u8),
}

/// A set of characters, as the colors of the arcs of `regcomp.c` give it.
#[derive(Clone, Debug, Default)]
pub(super) struct Set {
    chars: Vec<u32>,
    ranges: Vec<(u32, u32)>,
    /// The bits of the classes in the set.
    classes: u16,
    /// The bits of the classes whose complement is in the set.
    not_classes: u16,
    /// True for the complement of the set.
    complement: bool,
    /// True when the complement does not include a newline.
    no_newline: bool,
}

/// The class numbers of `enum char_classes`.
const CC_ALPHA: u32 = 1;
const CC_LOWER: u32 = 7;
const CC_UPPER: u32 = 11;

/// True when `c` is in the class `class` in the C locale, as `pg_char_properties` and `cclasscvec` give it.
fn in_class(class: u32, c: u32) -> bool {
    let Some(b) = u8::try_from(c).ok().filter(u8::is_ascii) else { return false };
    match CLASS_NAMES.get(class as usize).copied().unwrap_or("") {
        "alnum" => b.is_ascii_alphanumeric(),
        "alpha" => b.is_ascii_alphabetic(),
        "ascii" => true,
        "blank" => b == b' ' || b == b'\t',
        "cntrl" => b < 0x20 || b == 0x7f,
        "digit" => b.is_ascii_digit(),
        "graph" => b.is_ascii_graphic(),
        "lower" => b.is_ascii_lowercase(),
        "print" => b.is_ascii_graphic() || b == b' ',
        "punct" => b.is_ascii_punctuation(),
        "space" => is_space(c),
        "upper" => b.is_ascii_uppercase(),
        "xdigit" => b.is_ascii_hexdigit(),
        "word" => b.is_ascii_alphanumeric() || b == b'_',
        _ => false,
    }
}

/// `regc_wc_tolower` in the C locale.
pub(super) fn to_lower(c: u32) -> u32 {
    if (u32::from(b'A')..=u32::from(b'Z')).contains(&c) { c + 32 } else { c }
}

/// `regc_wc_toupper` in the C locale.
fn to_upper(c: u32) -> u32 {
    if (u32::from(b'a')..=u32::from(b'z')).contains(&c) { c - 32 } else { c }
}

impl Set {
    /// True when the set has `c`.
    pub(super) fn contains(&self, c: u32) -> bool {
        let inside = self.chars.contains(&c)
            || self.ranges.iter().any(|&(a, b)| a <= c && c <= b)
            || (0..14).any(|class| self.classes & (1 << class) != 0 && in_class(class, c))
            || (0..14).any(|class| self.not_classes & (1 << class) != 0 && !in_class(class, c));
        if self.complement {
            !inside && !(self.no_newline && c == u32::from(b'\n'))
        } else {
            inside
        }
    }

    /// `allcases`: a character and its other case.
    fn add_cases(&mut self, c: u32) {
        let (lower, upper) = (to_lower(c), to_upper(c));
        self.chars.push(lower);
        if lower != upper {
            self.chars.push(upper);
        }
    }

    /// `onechr`.
    fn add_char(&mut self, c: u32, icase: bool) {
        if icase {
            self.add_cases(c);
        } else {
            self.chars.push(c);
        }
    }

    /// `range`: the characters from `a` to `b`, and their other cases when `icase` is true.
    fn add_range(&mut self, a: u32, b: u32, icase: bool) -> Result<(), Code> {
        if a > b {
            return Err(Code::Range);
        }
        self.ranges.push((a, b));
        if icase {
            // Only the ASCII letters have another case in the C locale.
            for c in a.max(u32::from(b'A'))..=b.min(u32::from(b'z')) {
                for other in [to_lower(c), to_upper(c)] {
                    if other != c && (other < a || b < other) {
                        self.chars.push(other);
                    }
                }
            }
        }
        Ok(())
    }

    /// `cclasscvec`: a class, where `lower` and `upper` are `alpha` when `icase` is true.
    fn add_class(&mut self, class: u32, icase: bool) {
        self.classes |= 1 << case_class(class, icase);
    }

    /// The complement of a class, from an escape such as `\D`.
    fn add_not_class(&mut self, class: u32, icase: bool) {
        self.not_classes |= 1 << case_class(class, icase);
    }
}

fn case_class(class: u32, icase: bool) -> u32 {
    if icase && (class == CC_LOWER || class == CC_UPPER) { CC_ALPHA } else { class }
}

/// The ASCII names of the characters, the table `cnames` of `regc_locale.c`.
const CHAR_NAMES: [(&str, u8); 95] = [
    ("NUL", 0),
    ("SOH", 1),
    ("STX", 2),
    ("ETX", 3),
    ("EOT", 4),
    ("ENQ", 5),
    ("ACK", 6),
    ("BEL", 7),
    ("alert", 7),
    ("BS", 8),
    ("backspace", 8),
    ("HT", 9),
    ("tab", 9),
    ("LF", 10),
    ("newline", 10),
    ("VT", 11),
    ("vertical-tab", 11),
    ("FF", 12),
    ("form-feed", 12),
    ("CR", 13),
    ("carriage-return", 13),
    ("SO", 14),
    ("SI", 15),
    ("DLE", 16),
    ("DC1", 17),
    ("DC2", 18),
    ("DC3", 19),
    ("DC4", 20),
    ("NAK", 21),
    ("SYN", 22),
    ("ETB", 23),
    ("CAN", 24),
    ("EM", 25),
    ("SUB", 26),
    ("ESC", 27),
    ("IS4", 28),
    ("FS", 28),
    ("IS3", 29),
    ("GS", 29),
    ("IS2", 30),
    ("RS", 30),
    ("IS1", 31),
    ("US", 31),
    ("space", b' '),
    ("exclamation-mark", b'!'),
    ("quotation-mark", b'"'),
    ("number-sign", b'#'),
    ("dollar-sign", b'$'),
    ("percent-sign", b'%'),
    ("ampersand", b'&'),
    ("apostrophe", b'\''),
    ("left-parenthesis", b'('),
    ("right-parenthesis", b')'),
    ("asterisk", b'*'),
    ("plus-sign", b'+'),
    ("comma", b','),
    ("hyphen", b'-'),
    ("hyphen-minus", b'-'),
    ("period", b'.'),
    ("full-stop", b'.'),
    ("slash", b'/'),
    ("solidus", b'/'),
    ("zero", b'0'),
    ("one", b'1'),
    ("two", b'2'),
    ("three", b'3'),
    ("four", b'4'),
    ("five", b'5'),
    ("six", b'6'),
    ("seven", b'7'),
    ("eight", b'8'),
    ("nine", b'9'),
    ("colon", b':'),
    ("semicolon", b';'),
    ("less-than-sign", b'<'),
    ("equals-sign", b'='),
    ("greater-than-sign", b'>'),
    ("question-mark", b'?'),
    ("commercial-at", b'@'),
    ("left-square-bracket", b'['),
    ("backslash", b'\\'),
    ("reverse-solidus", b'\\'),
    ("right-square-bracket", b']'),
    ("circumflex", b'^'),
    ("circumflex-accent", b'^'),
    ("underscore", b'_'),
    ("low-line", b'_'),
    ("grave-accent", b'`'),
    ("left-brace", b'{'),
    ("left-curly-bracket", b'{'),
    ("vertical-line", b'|'),
    ("right-brace", b'}'),
    ("right-curly-bracket", b'}'),
    ("tilde", b'~'),
    ("DEL", 0x7f),
];

/// True when the characters of the pattern spell `name`.
fn spells(chars: &[u32], name: &str) -> bool {
    chars.len() == name.len() && chars.iter().zip(name.bytes()).all(|(&c, b)| c == u32::from(b))
}

/// `element`: the character of a collating element. A name of one character is that character.
fn element(chars: &[u32]) -> Result<u32, Code> {
    if let [c] = chars {
        return Ok(*c);
    }
    CHAR_NAMES
        .iter()
        .find(|(name, _)| spells(chars, name))
        .map(|&(_, code)| u32::from(code))
        .ok_or(Code::Collate)
}

/// `lookupcclass`: the number of a class by its name.
fn lookup_class(chars: &[u32]) -> Result<u32, Code> {
    CLASS_NAMES
        .iter()
        .position(|name| spells(chars, name))
        .and_then(|at| u32::try_from(at).ok())
        .ok_or(Code::Ctype)
}

/// The token that ends a part of the pattern: the end of the pattern or `)`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Eos,
    Paren,
}

/// The state of the parser.
pub(super) struct Parser<'a> {
    lex: Lexer<'a>,
    /// The captures whose group is complete, which a back reference can use.
    done: Vec<bool>,
}

/// The tree of a pattern, with the flags after the embedded options.
pub(super) struct Parsed {
    pub(super) node: Node,
    pub(super) cflags: u32,
    pub(super) groups: usize,
}

impl<'a> Parser<'a> {
    /// `pg_regcomp` up to the tree.
    pub(super) fn run(pattern: &'a [u32], cflags: u32) -> Result<Parsed, Code> {
        let lex = Lexer::start(pattern, cflags)?;
        let mut parser = Parser { lex, done: vec![false] };
        let node = parser.parse(Stop::Eos, false)?;
        Ok(Parsed { node, cflags: parser.lex.cflags, groups: parser.lex.nsubexp })
    }

    fn icase(&self) -> bool {
        self.lex.cflags & flags::ICASE != 0
    }

    fn see_stop(&self, stop: Stop) -> bool {
        match stop {
            Stop::Eos => self.lex.see(Tok::Eos),
            Stop::Paren => self.lex.see_op(b')'),
        }
    }

    /// `parse`: the branches between `|`.
    fn parse(&mut self, stop: Stop, lacon: bool) -> Result<Node, Code> {
        let mut branches = Vec::new();
        loop {
            branches.push(self.branch(stop, lacon)?);
            if !self.lex.see_op(b'|') {
                break;
            }
            self.lex.next()?;
        }
        if !self.see_stop(stop) {
            return Err(Code::Paren);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alt(branches)
        })
    }

    /// `parsebranch`: the quantified atoms of one branch.
    fn branch(&mut self, stop: Stop, lacon: bool) -> Result<Node, Code> {
        let mut items = Vec::new();
        while !self.lex.see_op(b'|') && !self.see_stop(stop) && !self.lex.see(Tok::Eos) {
            items.push(self.qatom(lacon)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    /// A node of one set, after which the lexer moves to the next token.
    fn set(&mut self, set: Set) -> Result<Node, Code> {
        self.lex.next()?;
        Ok(Node::Set(Box::new(set)))
    }

    /// `parseqatom`: one atom with its quantifier, or one constraint.
    fn qatom(&mut self, lacon: bool) -> Result<Node, Code> {
        let icase = self.icase();
        let value = self.lex.value;
        let constraint = match self.lex.tok {
            Tok::Op(b'^') => Some(Assert::Bol),
            Tok::Op(b'$') => Some(Assert::Eol),
            Tok::Sbegin => Some(Assert::Bos),
            Tok::Send => Some(Assert::Eos),
            Tok::Op(b'<') => Some(Assert::WordStart),
            Tok::Op(b'>') => Some(Assert::WordEnd),
            Tok::Wbdry => Some(Assert::Boundary),
            Tok::Nwbdry => Some(Assert::NotBoundary),
            _ => None,
        };
        if let Some(constraint) = constraint {
            self.lex.next()?;
            return Ok(Node::Assert(constraint));
        }
        let atom = match self.lex.tok {
            Tok::Lacon => {
                let look = match value {
                    AHEAD_POS => Look::Ahead,
                    AHEAD_NEG => Look::NotAhead,
                    BEHIND_POS => Look::Behind,
                    BEHIND_NEG => Look::NotBehind,
                    _ => return Err(Code::Assert),
                };
                self.lex.next()?;
                let inner = self.parse(Stop::Paren, true)?;
                self.lex.next()?;
                return Ok(Node::Look(look, Box::new(inner)));
            }
            Tok::Op(b'*' | b'+' | b'?' | b'{') => return Err(Code::BadRpt),
            Tok::Op(b')') => {
                if self.lex.cflags & flags::ADVANCED != flags::EXTENDED {
                    return Err(Code::Paren);
                }
                let mut set = Set::default();
                set.add_char(u32::from(b')'), icase);
                self.set(set)?
            }
            Tok::Plain => {
                let mut set = Set::default();
                set.add_char(value, icase);
                self.set(set)?
            }
            Tok::Op(b'[') => {
                let mut set = self.bracket()?;
                if value == 0 {
                    set.complement = true;
                    set.no_newline = self.lex.cflags & flags::NLSTOP != 0;
                }
                self.set(set)?
            }
            Tok::Cclasss => {
                let mut set = Set::default();
                set.add_class(value, icase);
                self.set(set)?
            }
            Tok::Cclassc => {
                let mut set = Set::default();
                set.add_not_class(value, icase);
                self.set(set)?
            }
            Tok::Op(b'.') => {
                let no_newline = self.lex.cflags & flags::NLSTOP != 0;
                self.set(Set { complement: true, no_newline, ..Set::default() })?
            }
            Tok::Op(b'(') => {
                let cap = !lacon && value != 0;
                let number = if cap {
                    self.lex.nsubexp += 1;
                    self.lex.nsubexp
                } else {
                    0
                };
                self.lex.next()?;
                let inner = self.parse(Stop::Paren, lacon)?;
                self.lex.next()?;
                if cap {
                    if self.done.len() <= number {
                        self.done.resize(number + 1, false);
                    }
                    self.done[number] = true;
                }
                Node::Group(number, Box::new(inner))
            }
            Tok::Backref => {
                let number = usize::try_from(value).map_err(|_| Code::Subreg)?;
                if lacon || !self.done.get(number).copied().unwrap_or(false) {
                    return Err(Code::Subreg);
                }
                self.lex.next()?;
                Node::Backref(number)
            }
            _ => return Err(Code::Assert),
        };
        let prefer = |value: u32| if value != 0 { LONGER } else { SHORTER };
        let (min, max, qprefer) = match self.lex.tok {
            Tok::Op(b'*') => {
                let qprefer = prefer(self.lex.value);
                self.lex.next()?;
                (0, DUPINF, qprefer)
            }
            Tok::Op(b'+') => {
                let qprefer = prefer(self.lex.value);
                self.lex.next()?;
                (1, DUPINF, qprefer)
            }
            Tok::Op(b'?') => {
                let qprefer = prefer(self.lex.value);
                self.lex.next()?;
                (0, 1, qprefer)
            }
            Tok::Op(b'{') => {
                self.lex.next()?;
                let min = self.scan_num()?;
                let (max, qprefer) = if self.lex.see_op(b',') {
                    self.lex.next()?;
                    let max = if self.lex.see(Tok::Digit) { self.scan_num()? } else { DUPINF };
                    if min > max {
                        return Err(Code::BadBr);
                    }
                    // `{m,n}` has a preference, also when `m` and `n` are the same.
                    (max, prefer(self.lex.value))
                } else {
                    (min, 0)
                };
                if !self.lex.see_op(b'}') {
                    return Err(Code::BadBr);
                }
                self.lex.next()?;
                (min, max, qprefer)
            }
            _ => (1, 1, 0),
        };
        Ok(match (min, max, qprefer) {
            (0, 0, _) => Node::Empty,
            (1, 1, 0) => atom,
            _ => Node::Repeat(Box::new(atom), min, max, qprefer),
        })
    }

    /// `scannum`: the number of a bound.
    fn scan_num(&mut self) -> Result<u32, Code> {
        let mut n = 0;
        while self.lex.see(Tok::Digit) && n < DUPMAX {
            n = n * 10 + self.lex.value;
            self.lex.next()?;
        }
        if self.lex.see(Tok::Digit) || n > DUPMAX {
            return Err(Code::BadBr);
        }
        Ok(n)
    }

    /// `bracket`: the parts of a bracket expression, up to the `]`.
    fn bracket(&mut self) -> Result<Set, Code> {
        let mut set = Set::default();
        self.lex.next()?;
        while !self.lex.see_op(b']') && !self.lex.see(Tok::Eos) {
            self.bracket_part(&mut set)?;
        }
        Ok(set)
    }

    /// The name of a collating element, an equivalence class or a character class, with the error for an empty name.
    fn name(&mut self, empty: Code) -> Result<&'a [u32], Code> {
        let start = self.lex.now;
        let end = self.lex.scan_plain()?;
        if start >= end {
            return Err(empty);
        }
        Ok(self.lex.slice(start, end))
    }

    /// `brackpart`: one part of a bracket expression.
    fn bracket_part(&mut self, set: &mut Set) -> Result<(), Code> {
        let icase = self.icase();
        let start = match self.lex.tok {
            Tok::Range => return Err(Code::Range),
            Tok::Plain => {
                let c = self.lex.value;
                self.lex.next()?;
                if !self.lex.see(Tok::Range) {
                    set.add_char(c, icase);
                    return Ok(());
                }
                c
            }
            Tok::Collel => element(self.name(Code::Collate)?)?,
            Tok::Eclass => {
                let c = element(self.name(Code::Collate)?)?;
                set.add_char(c, icase);
                return Ok(());
            }
            Tok::Cclass => {
                let class = lookup_class(self.name(Code::Ctype)?)?;
                set.add_class(class, icase);
                return Ok(());
            }
            Tok::Cclasss => {
                set.add_class(self.lex.value, icase);
                self.lex.next()?;
                return Ok(());
            }
            Tok::Cclassc => {
                set.add_not_class(self.lex.value, icase);
                self.lex.next()?;
                return Ok(());
            }
            _ => return Err(Code::Assert),
        };
        let end = if self.lex.see(Tok::Range) {
            self.lex.next()?;
            match self.lex.tok {
                Tok::Plain | Tok::Range => {
                    let c = self.lex.value;
                    self.lex.next()?;
                    c
                }
                Tok::Collel => element(self.name(Code::Collate)?)?,
                _ => return Err(Code::Range),
            }
        } else {
            start
        };
        set.add_range(start, end, icase)
    }
}

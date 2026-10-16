//! The default text search parser of `wparser_def.c`, which `ts_parse`, `to_tsvector` and the other functions of text search use. It is a state machine that splits a text into tokens of 23 types. Each state has a list of actions, and the parser takes the first action whose test is true for the character at the position. An action can store the position on a stack and try a longer token, and go back to the stored position when the longer token does not match.
//!
//! The database has the encoding UTF8 and the locale C, so a character that is not ASCII is a letter for the tests of letters and is not in any other class, as `p_iswhat` makes it.

use rupg_types::unicode::zero_width;

use table::ACTIONS;

/// The token types of `wparser_def.c`, from 1. The names and the descriptions are in `rupg_pgcatalog::TOKEN_TYPES`.
pub(crate) const ASCII_WORD: u8 = 1;
pub(crate) const WORD: u8 = 2;
pub(crate) const NUM_WORD: u8 = 3;
pub(crate) const EMAIL: u8 = 4;
pub(crate) const URL: u8 = 5;
pub(crate) const HOST: u8 = 6;
pub(crate) const SCIENTIFIC: u8 = 7;
pub(crate) const VERSION: u8 = 8;
pub(crate) const NUM_PART_HWORD: u8 = 9;
pub(crate) const PART_HWORD: u8 = 10;
pub(crate) const ASCII_PART_HWORD: u8 = 11;
pub(crate) const SPACE: u8 = 12;
pub(crate) const TAG: u8 = 13;
pub(crate) const PROTOCOL: u8 = 14;
pub(crate) const NUM_HWORD: u8 = 15;
pub(crate) const ASCII_HWORD: u8 = 16;
pub(crate) const HWORD: u8 = 17;
pub(crate) const URL_PATH: u8 = 18;
pub(crate) const FILE: u8 = 19;
pub(crate) const DECIMAL: u8 = 20;
pub(crate) const SIGNED_INT: u8 = 21;
pub(crate) const UNSIGNED_INT: u8 = 22;
pub(crate) const XML_ENTITY: u8 = 23;

// The flags of an action. With `NEXT`, the parser goes to the next character.
const NEXT: u8 = 0x00;
/// The token is complete.
const BINGO: u8 = 0x01;
/// Go back to the position on the top of the stack, and try the action after the action that stored it.
const POP: u8 = 0x02;
/// Store the position on the stack.
const PUSH: u8 = 0x04;
/// Run the actions of the new state on the same character.
const RERUN: u8 = 0x08;
/// Remove the position on the top of the stack.
const CLEAR: u8 = 0x10;
/// Remove the position on the top of the stack, and keep the current position in its place.
const MERGE: u8 = 0x20;
/// Remove all the positions of the stack.
const CLRALL: u8 = 0x40;

/// The deepest nesting of the parsers that the tests `Host` and `UrlPath` make. PostgreSQL has the limit of `check_stack_depth`, but a text does not make more than a few levels.
const MAX_NESTING: usize = 100;

/// The test of an action, one of the functions `p_is*` of `wparser_def.c`.
#[derive(Clone, Copy, Debug)]
enum Test {
    /// The end of the text.
    Eof,
    /// The ASCII character.
    Is(u8),
    Alnum,
    NotAlnum,
    Alpha,
    /// An ASCII letter.
    AsciiLetter,
    Digit,
    Space,
    Xdigit,
    /// A character with no display width, or a sign of the table `strange_letter`. It does not end a word.
    Special,
    /// True once after the parser starts to look for a host.
    StopHost,
    /// An ASCII character that RFC 3986 allows in a URL.
    UrlChar,
    /// A host at the position, which a new parser finds.
    Host,
    /// A URL path at the position, which a new parser finds.
    UrlPath,
    /// The parser is in a `script` or `style` tag.
    Ignore,
    /// The last action of each state.
    Any,
}

/// A special handler of an action.
#[derive(Clone, Copy, Debug)]
enum Handler {
    /// `SpecialTags`: the start and the end of the `script` and `style` tags.
    Tags,
    /// `SpecialFURL`: look for a host at the start of the token.
    FUrl,
    /// `SpecialHyphen`: go back to the start of the token.
    Hyphen,
    /// `SpecialVerVersion`: go back to the start of the token, which becomes empty.
    VerVersion,
}

/// An action of a state, `TParserStateActionItem`.
#[derive(Clone, Copy, Debug)]
struct Item {
    test: Test,
    flags: u8,
    /// The new state, or `None` to keep the state.
    to: Option<State>,
    /// The type of the token for `BINGO`.
    token: u8,
    handler: Option<Handler>,
}

const fn it(test: Test, flags: u8, to: Option<State>, token: u8, handler: Option<Handler>) -> Item {
    Item { test, flags, to, token, handler }
}

/// A position of the parser, `TParserPosition`.
#[derive(Clone, Copy, Debug)]
struct Position {
    byte: usize,
    char: usize,
    /// The length in bytes of the character at the position, or 0 at the end.
    charlen: usize,
    token_bytes: usize,
    token_chars: usize,
    state: State,
    /// The state and the index of the action that stored the next position on the stack. After `POP`, the parser tries the action after it.
    pushed: Option<(State, usize)>,
}

impl Position {
    fn new(state: State) -> Position {
        Position {
            byte: 0,
            char: 0,
            charlen: 0,
            token_bytes: 0,
            token_chars: 0,
            state,
            pushed: None,
        }
    }
}

/// The parser of a text, `TParser`.
#[derive(Debug)]
struct Parser<'a> {
    bytes: &'a [u8],
    /// The code points of the text.
    wide: &'a [u32],
    pos: Position,
    /// The positions that `PUSH` stored.
    stack: Vec<Position>,
    ignore: bool,
    want_host: bool,
    /// The parsers that made this parser.
    nesting: usize,
    /// The start of the token in bytes.
    start: usize,
    /// The length of the last token in bytes and in characters, and its type.
    token_bytes: usize,
    token_chars: usize,
    token: u8,
}

/// The tokens of a text: the type and the text of each token in order.
pub(crate) fn tokens(text: &str) -> Vec<(u8, &str)> {
    let wide: Vec<u32> = text.chars().map(u32::from).collect();
    let mut parser = Parser::new(text.as_bytes(), &wide, State::Base, 0);
    let mut out = Vec::new();
    while parser.next() {
        let end = parser.start + parser.token_bytes;
        out.push((parser.token, text.get(parser.start..end).unwrap_or_default()));
    }
    out
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8], wide: &'a [u32], state: State, nesting: usize) -> Parser<'a> {
        Parser {
            bytes,
            wide,
            pos: Position::new(state),
            stack: Vec::new(),
            ignore: false,
            want_host: false,
            nesting,
            start: 0,
            token_bytes: 0,
            token_chars: 0,
            token: 0,
        }
    }

    /// `TParserCopyInit`: a parser of the text after the position.
    fn copy(&self) -> Parser<'a> {
        let bytes = self.bytes.get(self.pos.byte..).unwrap_or_default();
        let wide = self.wide.get(self.pos.char..).unwrap_or_default();
        Parser::new(bytes, wide, State::Base, self.nesting + 1)
    }

    /// The code point at the position, or 0 at the end.
    fn wide_char(&self) -> u32 {
        self.wide.get(self.pos.char).copied().unwrap_or(0)
    }

    /// The ASCII character at the position, or `None` for another character or the end.
    fn ascii(&self) -> Option<u8> {
        if self.pos.charlen != 1 {
            return None;
        }
        self.bytes.get(self.pos.byte).copied().filter(u8::is_ascii)
    }

    /// A class test of `p_iswhat` with the C locale: `wide` is the result for a character that is not ASCII.
    fn class(&self, wide: bool, ascii: fn(u8) -> bool) -> bool {
        match u8::try_from(self.wide_char()) {
            Ok(c) if c.is_ascii() => ascii(c),
            _ => wide,
        }
    }

    fn test(&mut self, test: Test) -> bool {
        match test {
            Test::Eof => self.pos.byte == self.bytes.len() || self.pos.charlen == 0,
            Test::Is(c) => self.pos.charlen == 1 && self.bytes.get(self.pos.byte) == Some(&c),
            Test::Alnum => self.class(true, |c| c.is_ascii_alphanumeric()),
            Test::NotAlnum => !self.class(true, |c| c.is_ascii_alphanumeric()),
            Test::Alpha => self.class(true, |c| c.is_ascii_alphabetic()),
            Test::AsciiLetter => self.ascii().is_some_and(|c| c.is_ascii_alphabetic()),
            Test::Digit => self.class(false, |c| c.is_ascii_digit()),
            Test::Space => self.class(false, |c| matches!(c, b' ' | b'\t'..=b'\r')),
            Test::Xdigit => self.class(false, |c| c.is_ascii_hexdigit()),
            Test::Special => {
                let c = self.wide_char();
                zero_width(c) || STRANGE_LETTERS.binary_search(&c).is_ok()
            }
            Test::StopHost => std::mem::take(&mut self.want_host),
            Test::UrlChar => {
                self.ascii().is_some_and(|c| c > 0x20 && c < 0x7F && !b"\"<>\\^`{|}".contains(&c))
            }
            Test::Host => {
                let mut sub = self.copy();
                sub.want_host = true;
                self.take(&mut sub, HOST)
            }
            Test::UrlPath => {
                let mut sub = self.copy();
                sub.stack.push(sub.pos);
                sub.pos.state = State::InURLPathFirst;
                self.take(&mut sub, URL_PATH)
            }
            Test::Ignore => self.ignore,
            Test::Any => true,
        }
    }

    /// The end of `p_ishost` and `p_isURLPath`: when the parser `sub` finds a token of the type `token`, the token goes into the token of this parser.
    fn take(&mut self, sub: &mut Parser<'_>, token: u8) -> bool {
        if self.nesting >= MAX_NESTING || !sub.next() || sub.token != token {
            return false;
        }
        self.pos.byte += sub.token_bytes;
        self.pos.char += sub.token_chars;
        self.pos.token_bytes += sub.token_bytes;
        self.pos.token_chars += sub.token_chars;
        self.pos.charlen = sub.pos.charlen;
        true
    }

    fn handle(&mut self, handler: Handler) {
        let back = |pos: &mut Position| {
            pos.byte -= pos.token_bytes;
            pos.char -= pos.token_chars;
        };
        match handler {
            Handler::Tags => {
                let token = self.bytes.get(self.start..).unwrap_or_default();
                let is = |tag: &str| {
                    token.get(..tag.len()).is_some_and(|t| t.eq_ignore_ascii_case(tag.as_bytes()))
                };
                match self.pos.token_chars {
                    8 if is("</script") => self.ignore = false,
                    7 if is("</style") => self.ignore = false,
                    7 if is("<script") => self.ignore = true,
                    6 if is("<style") => self.ignore = true,
                    _ => {}
                }
            }
            Handler::FUrl => {
                self.want_host = true;
                back(&mut self.pos);
            }
            Handler::Hyphen => back(&mut self.pos),
            Handler::VerVersion => {
                back(&mut self.pos);
                self.pos.token_bytes = 0;
                self.pos.token_chars = 0;
            }
        }
    }

    /// The length in bytes of the character at a byte of the text.
    fn char_len(&self, byte: usize) -> usize {
        match self.bytes.get(byte) {
            None => 0,
            Some(&b) if b < 0xC0 => 1,
            Some(&b) if b < 0xE0 => 2,
            Some(&b) if b < 0xF0 => 3,
            Some(_) => 4,
        }
    }

    /// `TParserGet`: true when the parser finds the next token.
    fn next(&mut self) -> bool {
        let len = self.bytes.len();
        if self.pos.byte >= len {
            return false;
        }
        self.start = self.pos.byte;
        self.pos.pushed = None;
        let mut found = false;
        while self.pos.byte <= len {
            self.pos.charlen = self.char_len(self.pos.byte);
            let (table, mut index) = match self.pos.pushed.take() {
                Some((state, index)) => (state, index + 1),
                None => (self.pos.state, 0),
            };
            let actions = ACTIONS[table as usize];
            let item = loop {
                let Some(&item) = actions.get(index) else { return false };
                if self.test(item.test) {
                    break item;
                }
                index += 1;
            };
            if let Some(handler) = item.handler {
                self.handle(handler);
            }
            found = item.flags & BINGO != 0;
            if found {
                self.token_bytes = self.pos.token_bytes;
                self.token_chars = self.pos.token_chars;
                self.pos.token_bytes = 0;
                self.pos.token_chars = 0;
                self.token = item.token;
            }
            if item.flags & POP != 0 {
                if let Some(pos) = self.stack.pop() {
                    self.pos = pos;
                }
            } else if item.flags & PUSH != 0 {
                self.pos.pushed = Some((table, index));
                self.stack.push(self.pos);
                self.pos.pushed = None;
            } else if item.flags & CLEAR != 0 {
                self.stack.pop();
            } else if item.flags & CLRALL != 0 {
                self.stack.clear();
            } else if item.flags & MERGE != 0
                && let Some(mut pos) = self.stack.pop()
            {
                pos.byte = self.pos.byte;
                pos.char = self.pos.char;
                pos.charlen = self.pos.charlen;
                pos.token_bytes = self.pos.token_bytes;
                pos.token_chars = self.pos.token_chars;
                self.pos = pos;
            }
            if let Some(state) = item.to {
                self.pos.state = state;
            }
            if found || (self.pos.byte >= len && item.flags & RERUN == 0) {
                break;
            }
            if item.flags & (RERUN | POP) != 0 {
                continue;
            }
            if self.pos.charlen != 0 {
                self.pos.byte += self.pos.charlen;
                self.pos.token_bytes += self.pos.charlen;
                self.pos.char += 1;
                self.pos.token_chars += 1;
            }
        }
        found
    }
}

/// The states of the parser, in the order of `TParserState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Base,
    InNumWord,
    InAsciiWord,
    InWord,
    InUnsignedInt,
    InSignedIntFirst,
    InSignedInt,
    InSpace,
    InUDecimalFirst,
    InUDecimal,
    InDecimalFirst,
    InDecimal,
    InVerVersion,
    InSVerVersion,
    InVersionFirst,
    InVersion,
    InMantissaFirst,
    InMantissaSign,
    InMantissa,
    InXMLEntityFirst,
    InXMLEntity,
    InXMLEntityNumFirst,
    InXMLEntityNum,
    InXMLEntityHexNumFirst,
    InXMLEntityHexNum,
    InXMLEntityEnd,
    InTagFirst,
    InXMLBegin,
    InTagCloseFirst,
    InTagName,
    InTagBeginEnd,
    InTag,
    InTagEscapeK,
    InTagEscapeKK,
    InTagBackslashed,
    InTagEnd,
    InCommentFirst,
    InCommentLast,
    InComment,
    InCloseCommentFirst,
    InCloseCommentLast,
    InCommentEnd,
    InHostFirstDomain,
    InHostDomainSecond,
    InHostDomain,
    InPortFirst,
    InPort,
    InHostFirstAN,
    InHost,
    InEmail,
    InFileFirst,
    InFileTwiddle,
    InPathFirst,
    InPathFirstFirst,
    InPathSecond,
    InFile,
    InFileNext,
    InURLPathFirst,
    InURLPathStart,
    InURLPath,
    InFURL,
    InProtocolFirst,
    InProtocolSecond,
    InProtocolEnd,
    InHyphenAsciiWordFirst,
    InHyphenAsciiWord,
    InHyphenWordFirst,
    InHyphenWord,
    InHyphenNumWordFirst,
    InHyphenNumWord,
    InHyphenDigitLookahead,
    InParseHyphen,
    InParseHyphenHyphen,
    InHyphenWordPart,
    InHyphenAsciiWordPart,
    InHyphenNumWordPart,
    InHyphenUnsignedInt,
}

/// The signs of `p_isspecial` that are not letters and do not end a word, such as the vowel signs of Devanagari, sorted.
static STRANGE_LETTERS: [u32; 228] = [
    0x0903, 0x093E, 0x093F, 0x0940, 0x0949, 0x094A, 0x094B, 0x094C, 0x0982, 0x0983, 0x09BE, 0x09BF,
    0x09C0, 0x09C7, 0x09C8, 0x09CB, 0x09CC, 0x09D7, 0x0A03, 0x0A3E, 0x0A3F, 0x0A40, 0x0A83, 0x0ABE,
    0x0ABF, 0x0AC0, 0x0AC9, 0x0ACB, 0x0ACC, 0x0B02, 0x0B03, 0x0B3E, 0x0B40, 0x0B47, 0x0B48, 0x0B4B,
    0x0B4C, 0x0B57, 0x0BBE, 0x0BBF, 0x0BC1, 0x0BC2, 0x0BC6, 0x0BC7, 0x0BC8, 0x0BCA, 0x0BCB, 0x0BCC,
    0x0BD7, 0x0C01, 0x0C02, 0x0C03, 0x0C41, 0x0C42, 0x0C43, 0x0C44, 0x0C82, 0x0C83, 0x0CBE, 0x0CC0,
    0x0CC1, 0x0CC2, 0x0CC3, 0x0CC4, 0x0CC7, 0x0CC8, 0x0CCA, 0x0CCB, 0x0CD5, 0x0CD6, 0x0D02, 0x0D03,
    0x0D3E, 0x0D3F, 0x0D40, 0x0D46, 0x0D47, 0x0D48, 0x0D4A, 0x0D4B, 0x0D4C, 0x0D57, 0x0D82, 0x0D83,
    0x0DCF, 0x0DD0, 0x0DD1, 0x0DD8, 0x0DD9, 0x0DDA, 0x0DDB, 0x0DDC, 0x0DDD, 0x0DDE, 0x0DDF, 0x0DF2,
    0x0DF3, 0x0F3E, 0x0F3F, 0x0F7F, 0x102B, 0x102C, 0x1031, 0x1038, 0x103B, 0x103C, 0x1056, 0x1057,
    0x1062, 0x1063, 0x1064, 0x1067, 0x1068, 0x1069, 0x106A, 0x106B, 0x106C, 0x106D, 0x1083, 0x1084,
    0x1087, 0x1088, 0x1089, 0x108A, 0x108B, 0x108C, 0x108F, 0x17B6, 0x17BE, 0x17BF, 0x17C0, 0x17C1,
    0x17C2, 0x17C3, 0x17C4, 0x17C5, 0x17C7, 0x17C8, 0x1923, 0x1924, 0x1925, 0x1926, 0x1929, 0x192A,
    0x192B, 0x1930, 0x1931, 0x1933, 0x1934, 0x1935, 0x1936, 0x1937, 0x1938, 0x19B0, 0x19B1, 0x19B2,
    0x19B3, 0x19B4, 0x19B5, 0x19B6, 0x19B7, 0x19B8, 0x19B9, 0x19BA, 0x19BB, 0x19BC, 0x19BD, 0x19BE,
    0x19BF, 0x19C0, 0x19C8, 0x19C9, 0x1A19, 0x1A1A, 0x1A1B, 0x1B04, 0x1B35, 0x1B3B, 0x1B3D, 0x1B3E,
    0x1B3F, 0x1B40, 0x1B41, 0x1B43, 0x1B44, 0x1B82, 0x1BA1, 0x1BA6, 0x1BA7, 0x1BAA, 0x1C24, 0x1C25,
    0x1C26, 0x1C27, 0x1C28, 0x1C29, 0x1C2A, 0x1C2B, 0x1C34, 0x1C35, 0xA823, 0xA824, 0xA827, 0xA880,
    0xA881, 0xA8B4, 0xA8B5, 0xA8B6, 0xA8B7, 0xA8B8, 0xA8B9, 0xA8BA, 0xA8BB, 0xA8BC, 0xA8BD, 0xA8BE,
    0xA8BF, 0xA8C0, 0xA8C1, 0xA8C2, 0xA8C3, 0xA952, 0xA953, 0xAA2F, 0xAA30, 0xAA33, 0xAA34, 0xAA4D,
];

/// The tables of the actions. The names of the states, the tests and the handlers are short here.
mod table {
    use super::Handler::{FUrl, Hyphen, Tags, VerVersion};
    use super::State::*;
    use super::Test::*;
    use super::*;

    /// The actions of each state, in the order of [`State`], as the tables `actionTPS_*` of `wparser_def.c`. The parser takes the first item whose test is true.
    pub(super) static ACTIONS: [&[Item]; 77] = [
        // Base
        &[
            it(Eof, NEXT, None, 0, None),
            it(Is(b'<'), PUSH, Some(InTagFirst), 0, None),
            it(Ignore, NEXT, Some(InSpace), 0, None),
            it(AsciiLetter, NEXT, Some(InAsciiWord), 0, None),
            it(Alpha, NEXT, Some(InWord), 0, None),
            it(Digit, NEXT, Some(InUnsignedInt), 0, None),
            it(Is(b'-'), PUSH, Some(InSignedIntFirst), 0, None),
            it(Is(b'+'), PUSH, Some(InSignedIntFirst), 0, None),
            it(Is(b'&'), PUSH, Some(InXMLEntityFirst), 0, None),
            it(Is(b'~'), PUSH, Some(InFileTwiddle), 0, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Is(b'.'), PUSH, Some(InPathFirstFirst), 0, None),
            it(Any, NEXT, Some(InSpace), 0, None),
        ],
        // InNumWord
        &[
            it(Eof, BINGO, Some(Base), NUM_WORD, None),
            it(Alnum, NEXT, Some(InNumWord), 0, None),
            it(Special, NEXT, Some(InNumWord), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Is(b'.'), PUSH, Some(InFileNext), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenNumWordFirst), 0, None),
            it(Any, BINGO, Some(Base), NUM_WORD, None),
        ],
        // InAsciiWord
        &[
            it(Eof, BINGO, Some(Base), ASCII_WORD, None),
            it(AsciiLetter, NEXT, None, 0, None),
            it(Is(b'.'), PUSH, Some(InHostFirstDomain), 0, None),
            it(Is(b'.'), PUSH, Some(InFileNext), 0, None),
            it(Is(b'-'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenAsciiWordFirst), 0, None),
            it(Is(b'_'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(Is(b':'), PUSH, Some(InProtocolFirst), 0, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Digit, PUSH, Some(InHost), 0, None),
            it(Digit, NEXT, Some(InNumWord), 0, None),
            it(Alpha, NEXT, Some(InWord), 0, None),
            it(Special, NEXT, Some(InWord), 0, None),
            it(Any, BINGO, Some(Base), ASCII_WORD, None),
        ],
        // InWord
        &[
            it(Eof, BINGO, Some(Base), WORD, None),
            it(Alpha, NEXT, None, 0, None),
            it(Special, NEXT, None, 0, None),
            it(Digit, NEXT, Some(InNumWord), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenWordFirst), 0, None),
            it(Any, BINGO, Some(Base), WORD, None),
        ],
        // InUnsignedInt
        &[
            it(Eof, BINGO, Some(Base), UNSIGNED_INT, None),
            it(Digit, NEXT, None, 0, None),
            it(Is(b'.'), PUSH, Some(InHostFirstDomain), 0, None),
            it(Is(b'.'), PUSH, Some(InUDecimalFirst), 0, None),
            it(Is(b'e'), PUSH, Some(InMantissaFirst), 0, None),
            it(Is(b'E'), PUSH, Some(InMantissaFirst), 0, None),
            it(Is(b'-'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'_'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(AsciiLetter, PUSH, Some(InHost), 0, None),
            it(Alpha, NEXT, Some(InNumWord), 0, None),
            it(Special, NEXT, Some(InNumWord), 0, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Any, BINGO, Some(Base), UNSIGNED_INT, None),
        ],
        // InSignedIntFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InSignedInt), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InSignedInt
        &[
            it(Eof, BINGO, Some(Base), SIGNED_INT, None),
            it(Digit, NEXT, None, 0, None),
            it(Is(b'.'), PUSH, Some(InDecimalFirst), 0, None),
            it(Is(b'e'), PUSH, Some(InMantissaFirst), 0, None),
            it(Is(b'E'), PUSH, Some(InMantissaFirst), 0, None),
            it(Any, BINGO, Some(Base), SIGNED_INT, None),
        ],
        // InSpace
        &[
            it(Eof, BINGO, Some(Base), SPACE, None),
            it(Is(b'<'), BINGO, Some(Base), SPACE, None),
            it(Ignore, NEXT, None, 0, None),
            it(Is(b'-'), BINGO, Some(Base), SPACE, None),
            it(Is(b'+'), BINGO, Some(Base), SPACE, None),
            it(Is(b'&'), BINGO, Some(Base), SPACE, None),
            it(Is(b'/'), BINGO, Some(Base), SPACE, None),
            it(NotAlnum, NEXT, Some(InSpace), 0, None),
            it(Any, BINGO, Some(Base), SPACE, None),
        ],
        // InUDecimalFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InUDecimal), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InUDecimal
        &[
            it(Eof, BINGO, Some(Base), DECIMAL, None),
            it(Digit, NEXT, Some(InUDecimal), 0, None),
            it(Is(b'.'), PUSH, Some(InVersionFirst), 0, None),
            it(Is(b'e'), PUSH, Some(InMantissaFirst), 0, None),
            it(Is(b'E'), PUSH, Some(InMantissaFirst), 0, None),
            it(Any, BINGO, Some(Base), DECIMAL, None),
        ],
        // InDecimalFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InDecimal), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InDecimal
        &[
            it(Eof, BINGO, Some(Base), DECIMAL, None),
            it(Digit, NEXT, Some(InDecimal), 0, None),
            it(Is(b'.'), PUSH, Some(InVerVersion), 0, None),
            it(Is(b'e'), PUSH, Some(InMantissaFirst), 0, None),
            it(Is(b'E'), PUSH, Some(InMantissaFirst), 0, None),
            it(Any, BINGO, Some(Base), DECIMAL, None),
        ],
        // InVerVersion
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, RERUN, Some(InSVerVersion), 0, Some(VerVersion)),
            it(Any, POP, None, 0, None),
        ],
        // InSVerVersion
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, BINGO | CLRALL, Some(InUnsignedInt), SPACE, None),
            it(Any, NEXT, None, 0, None),
        ],
        // InVersionFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InVersion), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InVersion
        &[
            it(Eof, BINGO, Some(Base), VERSION, None),
            it(Digit, NEXT, Some(InVersion), 0, None),
            it(Is(b'.'), PUSH, Some(InVersionFirst), 0, None),
            it(Any, BINGO, Some(Base), VERSION, None),
        ],
        // InMantissaFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InMantissa), 0, None),
            it(Is(b'+'), NEXT, Some(InMantissaSign), 0, None),
            it(Is(b'-'), NEXT, Some(InMantissaSign), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InMantissaSign
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, CLEAR, Some(InMantissa), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InMantissa
        &[
            it(Eof, BINGO, Some(Base), SCIENTIFIC, None),
            it(Digit, NEXT, Some(InMantissa), 0, None),
            it(Any, BINGO, Some(Base), SCIENTIFIC, None),
        ],
        // InXMLEntityFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'#'), NEXT, Some(InXMLEntityNumFirst), 0, None),
            it(AsciiLetter, NEXT, Some(InXMLEntity), 0, None),
            it(Is(b':'), NEXT, Some(InXMLEntity), 0, None),
            it(Is(b'_'), NEXT, Some(InXMLEntity), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntity
        &[
            it(Eof, POP, None, 0, None),
            it(Alnum, NEXT, Some(InXMLEntity), 0, None),
            it(Is(b':'), NEXT, Some(InXMLEntity), 0, None),
            it(Is(b'_'), NEXT, Some(InXMLEntity), 0, None),
            it(Is(b'.'), NEXT, Some(InXMLEntity), 0, None),
            it(Is(b'-'), NEXT, Some(InXMLEntity), 0, None),
            it(Is(b';'), NEXT, Some(InXMLEntityEnd), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntityNumFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'x'), NEXT, Some(InXMLEntityHexNumFirst), 0, None),
            it(Is(b'X'), NEXT, Some(InXMLEntityHexNumFirst), 0, None),
            it(Digit, NEXT, Some(InXMLEntityNum), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntityNum
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, Some(InXMLEntityNum), 0, None),
            it(Is(b';'), NEXT, Some(InXMLEntityEnd), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntityHexNumFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Xdigit, NEXT, Some(InXMLEntityHexNum), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntityHexNum
        &[
            it(Eof, POP, None, 0, None),
            it(Xdigit, NEXT, Some(InXMLEntityHexNum), 0, None),
            it(Is(b';'), NEXT, Some(InXMLEntityEnd), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLEntityEnd
        &[it(Any, BINGO | CLEAR, Some(Base), XML_ENTITY, None)],
        // InTagFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'/'), PUSH, Some(InTagCloseFirst), 0, None),
            it(Is(b'!'), PUSH, Some(InCommentFirst), 0, None),
            it(Is(b'?'), PUSH, Some(InXMLBegin), 0, None),
            it(AsciiLetter, PUSH, Some(InTagName), 0, None),
            it(Is(b':'), PUSH, Some(InTagName), 0, None),
            it(Is(b'_'), PUSH, Some(InTagName), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InXMLBegin
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'x'), NEXT, Some(InTag), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InTagCloseFirst
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InTagName), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InTagName
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'/'), NEXT, Some(InTagBeginEnd), 0, None),
            it(Is(b'>'), NEXT, Some(InTagEnd), 0, Some(Tags)),
            it(Space, NEXT, Some(InTag), 0, Some(Tags)),
            it(Alnum, NEXT, None, 0, None),
            it(Is(b':'), NEXT, None, 0, None),
            it(Is(b'_'), NEXT, None, 0, None),
            it(Is(b'.'), NEXT, None, 0, None),
            it(Is(b'-'), NEXT, None, 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InTagBeginEnd
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'>'), NEXT, Some(InTagEnd), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InTag
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'>'), NEXT, Some(InTagEnd), 0, Some(Tags)),
            it(Is(b'\''), NEXT, Some(InTagEscapeK), 0, None),
            it(Is(b'"'), NEXT, Some(InTagEscapeKK), 0, None),
            it(AsciiLetter, NEXT, None, 0, None),
            it(Digit, NEXT, None, 0, None),
            it(Is(b'='), NEXT, None, 0, None),
            it(Is(b'-'), NEXT, None, 0, None),
            it(Is(b'_'), NEXT, None, 0, None),
            it(Is(b'#'), NEXT, None, 0, None),
            it(Is(b'/'), NEXT, None, 0, None),
            it(Is(b':'), NEXT, None, 0, None),
            it(Is(b'.'), NEXT, None, 0, None),
            it(Is(b'&'), NEXT, None, 0, None),
            it(Is(b'?'), NEXT, None, 0, None),
            it(Is(b'%'), NEXT, None, 0, None),
            it(Is(b'~'), NEXT, None, 0, None),
            it(Space, NEXT, None, 0, Some(Tags)),
            it(Any, POP, None, 0, None),
        ],
        // InTagEscapeK
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'\\'), PUSH, Some(InTagBackslashed), 0, None),
            it(Is(b'\''), NEXT, Some(InTag), 0, None),
            it(Any, NEXT, Some(InTagEscapeK), 0, None),
        ],
        // InTagEscapeKK
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'\\'), PUSH, Some(InTagBackslashed), 0, None),
            it(Is(b'"'), NEXT, Some(InTag), 0, None),
            it(Any, NEXT, Some(InTagEscapeKK), 0, None),
        ],
        // InTagBackslashed
        &[it(Eof, POP, None, 0, None), it(Any, MERGE, None, 0, None)],
        // InTagEnd
        &[it(Any, BINGO | CLRALL, Some(Base), TAG, None)],
        // InCommentFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'-'), NEXT, Some(InCommentLast), 0, None),
            it(Is(b'D'), NEXT, Some(InTag), 0, None),
            it(Is(b'd'), NEXT, Some(InTag), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InCommentLast
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'-'), NEXT, Some(InComment), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InComment
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'-'), NEXT, Some(InCloseCommentFirst), 0, None),
            it(Any, NEXT, None, 0, None),
        ],
        // InCloseCommentFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'-'), NEXT, Some(InCloseCommentLast), 0, None),
            it(Any, NEXT, Some(InComment), 0, None),
        ],
        // InCloseCommentLast
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'-'), NEXT, None, 0, None),
            it(Is(b'>'), NEXT, Some(InCommentEnd), 0, None),
            it(Any, NEXT, Some(InComment), 0, None),
        ],
        // InCommentEnd
        &[it(Any, BINGO | CLRALL, Some(Base), TAG, None)],
        // InHostFirstDomain
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InHostDomainSecond), 0, None),
            it(Digit, NEXT, Some(InHost), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHostDomainSecond
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InHostDomain), 0, None),
            it(Digit, PUSH, Some(InHost), 0, None),
            it(Is(b'-'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'_'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'.'), PUSH, Some(InHostFirstDomain), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHostDomain
        &[
            it(Eof, BINGO | CLRALL, Some(Base), HOST, None),
            it(AsciiLetter, NEXT, Some(InHostDomain), 0, None),
            it(Digit, PUSH, Some(InHost), 0, None),
            it(Is(b':'), PUSH, Some(InPortFirst), 0, None),
            it(Is(b'-'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'_'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'.'), PUSH, Some(InHostFirstDomain), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(Digit, POP, None, 0, None),
            it(StopHost, BINGO | CLRALL, Some(InURLPathStart), HOST, None),
            it(Is(b'/'), PUSH, Some(InFURL), 0, None),
            it(Any, BINGO | CLRALL, Some(Base), HOST, None),
        ],
        // InPortFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, Some(InPort), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InPort
        &[
            it(Eof, BINGO | CLRALL, Some(Base), HOST, None),
            it(Digit, NEXT, Some(InPort), 0, None),
            it(StopHost, BINGO | CLRALL, Some(InURLPathStart), HOST, None),
            it(Is(b'/'), PUSH, Some(InFURL), 0, None),
            it(Any, BINGO | CLRALL, Some(Base), HOST, None),
        ],
        // InHostFirstAN
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, Some(InHost), 0, None),
            it(AsciiLetter, NEXT, Some(InHost), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHost
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, Some(InHost), 0, None),
            it(AsciiLetter, NEXT, Some(InHost), 0, None),
            it(Is(b'@'), PUSH, Some(InEmail), 0, None),
            it(Is(b'.'), PUSH, Some(InHostFirstDomain), 0, None),
            it(Is(b'-'), PUSH, Some(InHostFirstAN), 0, None),
            it(Is(b'_'), PUSH, Some(InHostFirstAN), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InEmail
        &[
            it(StopHost, POP, None, 0, None),
            it(Host, BINGO | CLRALL, Some(Base), EMAIL, None),
            it(Any, POP, None, 0, None),
        ],
        // InFileFirst
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InFile), 0, None),
            it(Digit, NEXT, Some(InFile), 0, None),
            it(Is(b'.'), NEXT, Some(InPathFirst), 0, None),
            it(Is(b'_'), NEXT, Some(InFile), 0, None),
            it(Is(b'~'), PUSH, Some(InFileTwiddle), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InFileTwiddle
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InFile), 0, None),
            it(Digit, NEXT, Some(InFile), 0, None),
            it(Is(b'_'), NEXT, Some(InFile), 0, None),
            it(Is(b'/'), NEXT, Some(InFileFirst), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InPathFirst
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InFile), 0, None),
            it(Digit, NEXT, Some(InFile), 0, None),
            it(Is(b'_'), NEXT, Some(InFile), 0, None),
            it(Is(b'.'), NEXT, Some(InPathSecond), 0, None),
            it(Is(b'/'), NEXT, Some(InFileFirst), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InPathFirstFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'.'), NEXT, Some(InPathSecond), 0, None),
            it(Is(b'/'), NEXT, Some(InFileFirst), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InPathSecond
        &[
            it(Eof, BINGO | CLEAR, Some(Base), FILE, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Is(b'/'), BINGO | CLEAR, Some(Base), FILE, None),
            it(Space, BINGO | CLEAR, Some(Base), FILE, None),
            it(Any, POP, None, 0, None),
        ],
        // InFile
        &[
            it(Eof, BINGO, Some(Base), FILE, None),
            it(AsciiLetter, NEXT, Some(InFile), 0, None),
            it(Digit, NEXT, Some(InFile), 0, None),
            it(Is(b'.'), PUSH, Some(InFileNext), 0, None),
            it(Is(b'_'), NEXT, Some(InFile), 0, None),
            it(Is(b'-'), NEXT, Some(InFile), 0, None),
            it(Is(b'/'), PUSH, Some(InFileFirst), 0, None),
            it(Any, BINGO, Some(Base), FILE, None),
        ],
        // InFileNext
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, CLEAR, Some(InFile), 0, None),
            it(Digit, CLEAR, Some(InFile), 0, None),
            it(Is(b'_'), CLEAR, Some(InFile), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InURLPathFirst
        &[
            it(Eof, POP, None, 0, None),
            it(UrlChar, NEXT, Some(InURLPath), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InURLPathStart
        &[it(Any, NEXT, Some(InURLPath), 0, None)],
        // InURLPath
        &[
            it(Eof, BINGO, Some(Base), URL_PATH, None),
            it(UrlChar, NEXT, Some(InURLPath), 0, None),
            it(Any, BINGO, Some(Base), URL_PATH, None),
        ],
        // InFURL
        &[
            it(Eof, POP, None, 0, None),
            it(UrlPath, BINGO | CLRALL, Some(Base), URL, Some(FUrl)),
            it(Any, POP, None, 0, None),
        ],
        // InProtocolFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'/'), NEXT, Some(InProtocolSecond), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InProtocolSecond
        &[
            it(Eof, POP, None, 0, None),
            it(Is(b'/'), NEXT, Some(InProtocolEnd), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InProtocolEnd
        &[it(Any, BINGO | CLRALL, Some(Base), PROTOCOL, None)],
        // InHyphenAsciiWordFirst
        &[
            it(Eof, POP, None, 0, None),
            it(AsciiLetter, NEXT, Some(InHyphenAsciiWord), 0, None),
            it(Alpha, NEXT, Some(InHyphenWord), 0, None),
            it(Digit, NEXT, Some(InHyphenDigitLookahead), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHyphenAsciiWord
        &[
            it(Eof, BINGO | CLRALL, Some(InParseHyphen), ASCII_HWORD, Some(Hyphen)),
            it(AsciiLetter, NEXT, Some(InHyphenAsciiWord), 0, None),
            it(Alpha, NEXT, Some(InHyphenWord), 0, None),
            it(Special, NEXT, Some(InHyphenWord), 0, None),
            it(Digit, NEXT, Some(InHyphenNumWord), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenAsciiWordFirst), 0, None),
            it(Any, BINGO | CLRALL, Some(InParseHyphen), ASCII_HWORD, Some(Hyphen)),
        ],
        // InHyphenWordFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Alpha, NEXT, Some(InHyphenWord), 0, None),
            it(Digit, NEXT, Some(InHyphenDigitLookahead), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHyphenWord
        &[
            it(Eof, BINGO | CLRALL, Some(InParseHyphen), HWORD, Some(Hyphen)),
            it(Alpha, NEXT, Some(InHyphenWord), 0, None),
            it(Special, NEXT, Some(InHyphenWord), 0, None),
            it(Digit, NEXT, Some(InHyphenNumWord), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenWordFirst), 0, None),
            it(Any, BINGO | CLRALL, Some(InParseHyphen), HWORD, Some(Hyphen)),
        ],
        // InHyphenNumWordFirst
        &[
            it(Eof, POP, None, 0, None),
            it(Alpha, NEXT, Some(InHyphenNumWord), 0, None),
            it(Digit, NEXT, Some(InHyphenDigitLookahead), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InHyphenNumWord
        &[
            it(Eof, BINGO | CLRALL, Some(InParseHyphen), NUM_HWORD, Some(Hyphen)),
            it(Alnum, NEXT, Some(InHyphenNumWord), 0, None),
            it(Special, NEXT, Some(InHyphenNumWord), 0, None),
            it(Is(b'-'), PUSH, Some(InHyphenNumWordFirst), 0, None),
            it(Any, BINGO | CLRALL, Some(InParseHyphen), NUM_HWORD, Some(Hyphen)),
        ],
        // InHyphenDigitLookahead
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, Some(InHyphenDigitLookahead), 0, None),
            it(Alpha, NEXT, Some(InHyphenNumWord), 0, None),
            it(Special, NEXT, Some(InHyphenNumWord), 0, None),
            it(Any, POP, None, 0, None),
        ],
        // InParseHyphen
        &[
            it(Eof, RERUN, Some(Base), 0, None),
            it(AsciiLetter, NEXT, Some(InHyphenAsciiWordPart), 0, None),
            it(Alpha, NEXT, Some(InHyphenWordPart), 0, None),
            it(Digit, PUSH, Some(InHyphenUnsignedInt), 0, None),
            it(Is(b'-'), PUSH, Some(InParseHyphenHyphen), 0, None),
            it(Any, RERUN, Some(Base), 0, None),
        ],
        // InParseHyphenHyphen
        &[
            it(Eof, POP, None, 0, None),
            it(Alnum, BINGO | CLEAR, Some(InParseHyphen), SPACE, None),
            it(Special, BINGO | CLEAR, Some(InParseHyphen), SPACE, None),
            it(Any, POP, None, 0, None),
        ],
        // InHyphenWordPart
        &[
            it(Eof, BINGO, Some(Base), PART_HWORD, None),
            it(Alpha, NEXT, Some(InHyphenWordPart), 0, None),
            it(Special, NEXT, Some(InHyphenWordPart), 0, None),
            it(Digit, NEXT, Some(InHyphenNumWordPart), 0, None),
            it(Any, BINGO, Some(InParseHyphen), PART_HWORD, None),
        ],
        // InHyphenAsciiWordPart
        &[
            it(Eof, BINGO, Some(Base), ASCII_PART_HWORD, None),
            it(AsciiLetter, NEXT, Some(InHyphenAsciiWordPart), 0, None),
            it(Alpha, NEXT, Some(InHyphenWordPart), 0, None),
            it(Special, NEXT, Some(InHyphenWordPart), 0, None),
            it(Digit, NEXT, Some(InHyphenNumWordPart), 0, None),
            it(Any, BINGO, Some(InParseHyphen), ASCII_PART_HWORD, None),
        ],
        // InHyphenNumWordPart
        &[
            it(Eof, BINGO, Some(Base), NUM_PART_HWORD, None),
            it(Alnum, NEXT, Some(InHyphenNumWordPart), 0, None),
            it(Special, NEXT, Some(InHyphenNumWordPart), 0, None),
            it(Any, BINGO, Some(InParseHyphen), NUM_PART_HWORD, None),
        ],
        // InHyphenUnsignedInt
        &[
            it(Eof, POP, None, 0, None),
            it(Digit, NEXT, None, 0, None),
            it(Alpha, CLEAR, Some(InHyphenNumWordPart), 0, None),
            it(Special, CLEAR, Some(InHyphenNumWordPart), 0, None),
            it(Any, POP, None, 0, None),
        ],
    ];
}

//! The number patterns of `formatting.c`: `to_char` of `int4`, `int8`, `float4`, `float8` and `numeric`, and `to_number`. rupg has only the C locale, so `D` is a point, `G` is a comma, `L` is a space and `S` writes `+` and `-`, as `NUM_prepare_locale` takes them from the C locale.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Numeric, NumericSign, Value, numeric_in, numeric_out, numeric_out_sci};

use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "int4_to_char" => int4_to_char,
        "int8_to_char" => int8_to_char,
        "float4_to_char" => float4_to_char,
        "float8_to_char" => float8_to_char,
        "numeric_to_char" => numeric_to_char,
        "numeric_to_number" => to_number,
        _ => return None,
    })
}

/// A pattern of `NUM_keywords`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Comma,
    Point,
    Zero,
    Nine,
    B,
    C,
    D,
    Eeee,
    Fm,
    G,
    L,
    Mi,
    Pl,
    Pr,
    Rn,
    RnLower,
    Sg,
    Sp,
    S,
    Th,
    ThLower,
    V,
}

/// `NUM_keywords` in the order of the table. The first name that the rest of the format starts with is the pattern, so `SG` and `SP` come before `S`.
const KEYWORDS: [(&str, Key); 36] = [
    (",", Key::Comma),
    (".", Key::Point),
    ("0", Key::Zero),
    ("9", Key::Nine),
    ("B", Key::B),
    ("C", Key::C),
    ("D", Key::D),
    ("EEEE", Key::Eeee),
    ("FM", Key::Fm),
    ("G", Key::G),
    ("L", Key::L),
    ("MI", Key::Mi),
    ("PL", Key::Pl),
    ("PR", Key::Pr),
    ("RN", Key::Rn),
    ("SG", Key::Sg),
    ("SP", Key::Sp),
    ("S", Key::S),
    ("TH", Key::Th),
    ("V", Key::V),
    ("b", Key::B),
    ("c", Key::C),
    ("d", Key::D),
    ("eeee", Key::Eeee),
    ("fm", Key::Fm),
    ("g", Key::G),
    ("l", Key::L),
    ("mi", Key::Mi),
    ("pl", Key::Pl),
    ("pr", Key::Pr),
    ("rn", Key::RnLower),
    ("sg", Key::Sg),
    ("sp", Key::Sp),
    ("s", Key::S),
    ("th", Key::ThLower),
    ("v", Key::V),
];

/// A node of the parsed format: a pattern, or a character that `to_char` copies and `to_number` skips.
#[derive(Debug, Clone, Copy)]
enum Node {
    Action(Key),
    Char(char),
}

// The flags of `NUMDesc`.
const DECIMAL: u32 = 1 << 1;
const LDECIMAL: u32 = 1 << 2;
const ZERO: u32 = 1 << 3;
const BLANK: u32 = 1 << 4;
const FILLMODE: u32 = 1 << 5;
const LSIGN: u32 = 1 << 6;
const BRACKET: u32 = 1 << 7;
const MINUS: u32 = 1 << 8;
const PLUS: u32 = 1 << 9;
const ROMAN: u32 = 1 << 10;
const MULTI: u32 = 1 << 11;
const PLUS_POST: u32 = 1 << 12;
const MINUS_POST: u32 = 1 << 13;
const EEEE: u32 = 1 << 14;

/// Where `S` writes the sign: before the first digit or after the last digit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Lsign {
    Pre,
    Post,
    #[default]
    None,
}

/// `NUMDesc`: the counts and the flags of the format.
#[derive(Debug, Clone, Copy, Default)]
struct Desc {
    /// The digits before the point.
    pre: i32,
    /// The digits after the point.
    post: i32,
    lsign: Lsign,
    flag: u32,
    /// The digits before `S` when `S` comes before the point.
    pre_lsign_num: i32,
    /// The digits after `V`.
    multi: i32,
    /// The position of the first `0`, from 1.
    zero_start: i32,
    /// The position of the last `0`.
    zero_end: i32,
}

fn syntax(message: &str) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, message.to_string())
}

impl Desc {
    fn is(&self, flag: u32) -> bool {
        self.flag & flag != 0
    }

    /// `NUMDesc_prepare`: counts a pattern of the format and checks that it goes with the patterns before it.
    fn prepare(&mut self, key: Key) -> Result<()> {
        if self.is(EEEE) && key != Key::Eeee {
            return Err(syntax("\"EEEE\" must be the last pattern used"));
        }
        match key {
            Key::Nine => {
                if self.is(BRACKET) {
                    return Err(syntax("\"9\" must be ahead of \"PR\""));
                }
                if self.is(MULTI) {
                    self.multi += 1;
                } else if self.is(DECIMAL) {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
            }
            Key::Zero => {
                if self.is(BRACKET) {
                    return Err(syntax("\"0\" must be ahead of \"PR\""));
                }
                if !self.is(ZERO) && !self.is(DECIMAL) {
                    self.flag |= ZERO;
                    self.zero_start = self.pre + 1;
                }
                if self.is(DECIMAL) {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
                self.zero_end = self.pre + self.post;
            }
            Key::B => {
                if self.pre == 0 && self.post == 0 && !self.is(ZERO) {
                    self.flag |= BLANK;
                }
            }
            Key::D | Key::Point => {
                if key == Key::D {
                    self.flag |= LDECIMAL;
                }
                if self.is(DECIMAL) {
                    return Err(syntax("multiple decimal points"));
                }
                if self.is(MULTI) {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= DECIMAL;
            }
            Key::Fm => self.flag |= FILLMODE,
            Key::S => {
                if self.is(LSIGN) {
                    return Err(syntax("cannot use \"S\" twice"));
                }
                if self.is(PLUS | MINUS | BRACKET) {
                    return Err(syntax(
                        "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                    ));
                }
                if !self.is(DECIMAL) {
                    self.lsign = Lsign::Pre;
                    self.pre_lsign_num = self.pre;
                    self.flag |= LSIGN;
                } else if self.lsign == Lsign::None {
                    self.lsign = Lsign::Post;
                    self.flag |= LSIGN;
                }
            }
            Key::Mi => {
                if self.is(LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"MI\" together"));
                }
                self.flag |= MINUS;
                if self.is(DECIMAL) {
                    self.flag |= MINUS_POST;
                }
            }
            Key::Pl => {
                if self.is(LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"PL\" together"));
                }
                self.flag |= PLUS;
                if self.is(DECIMAL) {
                    self.flag |= PLUS_POST;
                }
            }
            Key::Sg => {
                if self.is(LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"SG\" together"));
                }
                self.flag |= MINUS | PLUS;
            }
            Key::Pr => {
                if self.is(LSIGN | PLUS | MINUS) {
                    return Err(syntax(
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                    ));
                }
                self.flag |= BRACKET;
            }
            Key::Rn | Key::RnLower => {
                if self.is(ROMAN) {
                    return Err(syntax("cannot use \"RN\" twice"));
                }
                self.flag |= ROMAN;
            }
            Key::V => {
                if self.is(DECIMAL) {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= MULTI;
            }
            Key::Eeee => {
                if self.is(EEEE) {
                    return Err(syntax("cannot use \"EEEE\" twice"));
                }
                if self.is(BLANK | FILLMODE | LSIGN | BRACKET | MINUS | PLUS | ROMAN | MULTI) {
                    return Err(syntax("\"EEEE\" is incompatible with other formats").with_detail(
                        "\"EEEE\" may only be used together with digit and decimal point patterns.",
                    ));
                }
                self.flag |= EEEE;
            }
            Key::C | Key::G | Key::L | Key::Sp | Key::Th | Key::ThLower | Key::Comma => {}
        }
        if self.is(ROMAN) && self.flag & !(ROMAN | FILLMODE) != 0 {
            return Err(syntax("\"RN\" is incompatible with other formats")
                .with_detail("\"RN\" may only be used together with \"FM\"."));
        }
        Ok(())
    }
}

/// `parse_format` for the number patterns. A double quote starts a text that ends at the next double quote, and a backslash in it quotes the next character. Outside a quoted text, a backslash is special only before a double quote.
fn parse(format: &str) -> Result<(Vec<Node>, Desc)> {
    let mut nodes = Vec::new();
    let mut desc = Desc::default();
    let mut i = 0;
    while let Some(c) = format[i..].chars().next() {
        let rest = &format[i..];
        if let Some(&(name, key)) = KEYWORDS.iter().find(|(name, _)| rest.starts_with(name)) {
            desc.prepare(key)?;
            nodes.push(Node::Action(key));
            i += name.len();
        } else if c == '"' {
            i += 1;
            while let Some(mut c) = format[i..].chars().next() {
                if c == '"' {
                    i += 1;
                    break;
                }
                if c == '\\'
                    && let Some(next) = format[i + 1..].chars().next()
                {
                    i += 1;
                    c = next;
                }
                nodes.push(Node::Char(c));
                i += c.len_utf8();
            }
        } else {
            let mut c = c;
            if rest.starts_with("\\\"") {
                i += 1;
                c = '"';
            }
            nodes.push(Node::Char(c));
            i += c.len_utf8();
        }
    }
    Ok((nodes, desc))
}

/// `get_last_relevant_decnum`: the position of the last digit after the point that is not 0, or of the point when all of them are 0. `None` when there is no point.
fn last_relevant_decnum(number: &[u8]) -> Option<usize> {
    let point = number.iter().position(|&b| b == b'.')?;
    Some(
        number
            .iter()
            .enumerate()
            .skip(point + 1)
            .fold(point, |r, (i, &b)| if b != b'0' { i } else { r }),
    )
}

/// `get_th` for a number: the ordinal suffix of the last digit, with `th` for 11, 12 and 13.
fn ordinal(number: &[u8], upper: bool) -> Result<&'static str> {
    let len = number.len();
    let last = number.last().copied().unwrap_or(0);
    if !last.is_ascii_digit() {
        return Err(Error::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("\"{}\" is not a number", String::from_utf8_lossy(number)),
        ));
    }
    let last = if len > 1 && number[len - 2] == b'1' { b'0' } else { last };
    Ok(match (last, upper) {
        (b'1', false) => "st",
        (b'2', false) => "nd",
        (b'3', false) => "rd",
        (_, false) => "th",
        (b'1', true) => "ST",
        (b'2', true) => "ND",
        (b'3', true) => "RD",
        (_, true) => "TH",
    })
}

/// The state of `NUM_processor` for `to_char`.
struct ToChar<'a> {
    desc: Desc,
    /// The digits of the value with no sign, `#` for an overflow, or the Roman numeral.
    number: &'a [u8],
    /// `number_p`: the next character of `number`.
    at: usize,
    /// `+` or `-`, or 0 for a Roman numeral.
    sign: u8,
    sign_wrote: bool,
    num_count: i32,
    num_in: bool,
    num_curr: i32,
    out_pre_spaces: i32,
    last_relevant: Option<usize>,
    out: Vec<u8>,
}

impl ToChar<'_> {
    /// The character at `number_p`. The digits end with a NUL in C, and a read past them gives that NUL.
    fn current(&self) -> u8 {
        self.number.get(self.at).copied().unwrap_or(0)
    }

    fn relevant_is_point(&self) -> bool {
        self.last_relevant.is_some_and(|i| self.number.get(i) == Some(&b'.'))
    }

    /// `IS_PREDEC_SPACE`: the value is less than 1 and the format has digits after the point, so the 0 before the point is a space.
    fn predec_space(&self) -> bool {
        !self.desc.is(ZERO)
            && self.at == 0
            && self.number.first() == Some(&b'0')
            && self.desc.post != 0
    }

    fn sign_symbol(&self) -> u8 {
        if self.sign == b'-' { b'-' } else { b'+' }
    }

    /// `NUM_numpart_to_char`: writes the sign before the first digit, then a space, a zero, a digit or the point, then the sign after the last digit.
    fn numpart(&mut self, key: Key) {
        if self.desc.is(ROMAN) {
            return;
        }
        self.num_in = false;
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (self.desc.is(ZERO) && self.desc.zero_start == self.num_curr))
            && (!self.predec_space() || self.relevant_is_point())
        {
            if self.desc.is(LSIGN) {
                if self.desc.lsign == Lsign::Pre {
                    self.out.push(self.sign_symbol());
                    self.sign_wrote = true;
                }
            } else if self.desc.is(BRACKET) {
                self.out.push(if self.sign == b'+' { b' ' } else { b'<' });
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !self.desc.is(FILLMODE) {
                    self.out.push(b' ');
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.out.push(b'-');
                self.sign_wrote = true;
            }
        }
        if matches!(key, Key::Nine | Key::Zero | Key::D | Key::Point) {
            if self.num_curr < self.out_pre_spaces
                && (self.desc.zero_start > self.num_curr || !self.desc.is(ZERO))
            {
                if !self.desc.is(FILLMODE) {
                    self.out.push(b' ');
                }
            } else if self.desc.is(ZERO)
                && self.num_curr < self.out_pre_spaces
                && self.desc.zero_start <= self.num_curr
            {
                self.out.push(b'0');
                self.num_in = true;
            } else {
                if self.current() == b'.' {
                    if !self.relevant_is_point() || self.desc.is(FILLMODE) {
                        self.out.push(b'.');
                    }
                } else if self.last_relevant.is_some_and(|r| self.at > r) && key != Key::Zero {
                } else if self.predec_space() {
                    if !self.desc.is(FILLMODE) {
                        self.out.push(b' ');
                    } else if self.relevant_is_point() {
                        self.out.push(b'0');
                    }
                } else {
                    self.out.push(self.current());
                    self.num_in = true;
                }
                if self.current() != 0 {
                    self.at += 1;
                }
            }
            let mut end = self.num_count
                + i32::from(self.out_pre_spaces != 0)
                + i32::from(self.desc.is(DECIMAL));
            if self.last_relevant == Some(self.at) {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.desc.is(BRACKET) {
                    self.out.push(if self.sign == b'+' { b' ' } else { b'>' });
                } else if self.desc.is(LSIGN) && self.desc.lsign == Lsign::Post {
                    self.out.push(self.sign_symbol());
                }
            }
        }
        self.num_curr += 1;
    }
}

/// `NUM_processor` for `to_char`. `number` has the digits of the value with no sign, and `out_pre_spaces` is the count of the digits of the format before the point that the value does not use.
fn to_char(
    nodes: &[Node],
    desc: Desc,
    number: &[u8],
    out_pre_spaces: i32,
    sign: u8,
) -> Result<Value> {
    let mut desc = desc;
    if desc.zero_start > 0 {
        desc.zero_start -= 1;
    }
    if desc.is(EEEE) {
        return Ok(Value::text(String::from_utf8_lossy(number)));
    }
    let sign_wrote = if desc.is(PLUS | MINUS) {
        !(desc.is(PLUS) && !desc.is(MINUS))
    } else {
        if sign != b'-' && desc.is(FILLMODE) {
            desc.flag &= !BRACKET;
        }
        if desc.lsign == Lsign::Pre && desc.pre == desc.pre_lsign_num {
            desc.lsign = Lsign::Post;
        }
        sign == b'+' && desc.is(FILLMODE) && !desc.is(LSIGN)
    };
    let mut last_relevant = None;
    if desc.is(FILLMODE) && desc.is(DECIMAL) {
        last_relevant = last_relevant_decnum(number);
        // The digits of `0` stay, but not past the end of the digits.
        if let Some(r) = last_relevant
            && desc.zero_end > out_pre_spaces
        {
            let last_zero =
                number.len().saturating_sub(1).min((desc.zero_end - out_pre_spaces) as usize);
            if r < last_zero {
                last_relevant = Some(last_zero);
            }
        }
    }
    let mut num_count = desc.post + desc.pre - 1;
    if !sign_wrote && out_pre_spaces == 0 {
        num_count += 1;
    }
    let mut np = ToChar {
        desc,
        number,
        at: 0,
        sign,
        sign_wrote,
        num_count,
        num_in: false,
        num_curr: 0,
        out_pre_spaces,
        last_relevant,
        out: Vec::new(),
    };
    for node in nodes {
        let key = match *node {
            Node::Char(c) => {
                let mut buf = [0; 4];
                np.out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                continue;
            }
            Node::Action(key) => key,
        };
        let fill = np.desc.is(FILLMODE);
        match key {
            Key::Nine | Key::Zero | Key::Point | Key::D => np.numpart(key),
            Key::Comma | Key::G => match np.num_in {
                true => np.out.push(b','),
                false if !fill => np.out.push(b' '),
                false => {}
            },
            Key::L => np.out.push(b' '),
            Key::Rn | Key::RnLower => {
                let rest = &np.number[np.at.min(np.number.len())..];
                let mut roman = String::from_utf8_lossy(rest).into_owned();
                if key == Key::RnLower {
                    roman.make_ascii_lowercase();
                }
                if !fill {
                    roman = format!("{roman:>15}");
                }
                np.out.extend_from_slice(roman.as_bytes());
            }
            Key::Th | Key::ThLower => {
                if np.desc.is(ROMAN | DECIMAL)
                    || np.number.first() == Some(&b'#')
                    || np.sign == b'-'
                {
                    continue;
                }
                np.out.extend_from_slice(ordinal(np.number, key == Key::Th)?.as_bytes());
            }
            Key::Mi => match np.sign {
                b'-' => np.out.push(b'-'),
                _ if fill => {}
                _ => np.out.push(b' '),
            },
            Key::Pl => match np.sign {
                b'+' => np.out.push(b'+'),
                _ if fill => {}
                _ => np.out.push(b' '),
            },
            Key::Sg => np.out.push(np.sign),
            Key::B | Key::C | Key::Eeee | Key::Fm | Key::Pr | Key::Sp | Key::S | Key::V => {}
        }
    }
    // A digit past the end of the digits writes the NUL of C, and the text ends there.
    let mut out = np.out;
    if let Some(end) = out.iter().position(|&b| b == 0) {
        out.truncate(end);
    }
    Ok(Value::text(String::from_utf8_lossy(&out)))
}

/// `pg_utf_mblen`: the length of the character that starts with the byte, and 1 for a byte that cannot start one.
fn mblen(input: &[u8], at: usize) -> usize {
    let b = input[at];
    let len = match b {
        _ if b & 0x80 == 0 => 1,
        _ if b & 0xe0 == 0xc0 => 2,
        _ if b & 0xf0 == 0xe0 => 3,
        _ if b & 0xf8 == 0xf0 => 4,
        _ => 1,
    };
    len.min(input.len() - at)
}

/// The value of a Roman numeral, or 0 for another character.
fn roman_value(c: u8) -> i32 {
    match c {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    }
}

/// `MMMDCCCLXXXVIII`, the longest Roman numeral.
const MAX_ROMAN_LEN: usize = 15;

/// The state of `NUM_processor` for `to_number`.
struct FromChar<'a> {
    desc: Desc,
    input: &'a [u8],
    /// `inout_p`: the next byte of the input.
    at: usize,
    /// The sign, then the digits and the point that the input gives.
    number: Vec<u8>,
    read_dec: bool,
    read_pre: i32,
    read_post: i32,
}

impl FromChar<'_> {
    fn at_end(&self) -> bool {
        self.at >= self.input.len()
    }

    /// `NUM_eat_non_data_chars`: skips up to `n` characters that are not digits, points, commas or signs.
    fn eat(&mut self, n: usize) {
        for _ in 0..n {
            if self.at_end() || b"0123456789.,+-".contains(&self.input[self.at]) {
                break;
            }
            self.at += mblen(self.input, self.at);
        }
    }

    /// `NUM_numpart_from_char`: reads the sign before the first digit, then a digit or the point, then the sign after the last digit.
    fn numpart(&mut self, key: Key) {
        if self.at_end() {
            return;
        }
        if self.input[self.at] == b' ' {
            self.at += 1;
        }
        if self.at_end() {
            return;
        }
        if self.number[0] == b' '
            && matches!(key, Key::Zero | Key::Nine)
            && self.read_pre + self.read_post == 0
        {
            let c = self.input[self.at];
            if self.desc.is(LSIGN) && self.desc.lsign == Lsign::Pre {
                if c == b'-' || c == b'+' {
                    self.at += 1;
                    self.number[0] = c;
                }
            } else if c == b'-' || (self.desc.is(BRACKET) && c == b'<') {
                self.number[0] = b'-';
                self.at += 1;
            } else if c == b'+' {
                self.number[0] = b'+';
                self.at += 1;
            }
        }
        if self.at_end() {
            return;
        }
        let mut isread = false;
        let c = self.input[self.at];
        if c.is_ascii_digit() {
            if self.read_dec && self.read_post == self.desc.post {
                return;
            }
            self.number.push(c);
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.desc.is(DECIMAL) && !self.read_dec && c == b'.' {
            self.number.push(b'.');
            self.read_dec = true;
            isread = true;
        }
        if self.at_end() {
            return;
        }
        // The sign after the last digit, which `S` anchors to the digits and `MI`, `PL` and `SG` read here when the format is not exact, as for `FM9.999999MI` and `5.01-`.
        if self.number[0] == b' ' && self.read_pre + self.read_post > 0 {
            if self.desc.is(LSIGN)
                && isread
                && self.at + 1 < self.input.len()
                && !self.input[self.at + 1].is_ascii_digit()
            {
                let next = self.input[self.at + 1];
                if next == b'-' || next == b'+' {
                    self.at += 1;
                    self.number[0] = next;
                }
            } else if !isread
                && !self.desc.is(LSIGN)
                && self.desc.is(PLUS | MINUS)
                && (c == b'-' || c == b'+')
            {
                self.number[0] = c;
            }
        }
    }

    /// `roman_to_int`: the value of the Roman numeral at the input, from 1 to 3999, or `None` when it is not valid.
    fn roman(&mut self) -> Option<i32> {
        while !self.at_end() && crate::formatting::is_space(self.input[self.at]) {
            self.at += 1;
        }
        let mut chars = Vec::new();
        while chars.len() < MAX_ROMAN_LEN && !self.at_end() {
            let c = self.input[self.at].to_ascii_uppercase();
            let value = roman_value(c);
            if value == 0 {
                break;
            }
            chars.push((c, value));
            self.at += 1;
        }
        if chars.is_empty() {
            return None;
        }
        let (mut result, mut repeat, mut last_subtracted) = (0, 1, 0);
        let (mut v, mut l, mut d) = (0, 0, 0);
        let mut subtracted = false;
        let mut i = 0;
        while i < chars.len() {
            let (c, value) = chars[i];
            // No numeral as large as a subtracted numeral after it.
            if subtracted && value >= last_subtracted {
                return None;
            }
            // V, L and D come once and not before a larger numeral.
            let seen = |v: i32, l: i32, d: i32, value: i32| {
                (v > 0 && value >= 5) || (l > 0 && value >= 50) || (d > 0 && value >= 500)
            };
            if seen(v, l, d, value) {
                return None;
            }
            match c {
                b'V' => v += 1,
                b'L' => l += 1,
                b'D' => d += 1,
                _ => {}
            }
            let Some(&(next, next_value)) = chars.get(i + 1) else {
                result += value;
                break;
            };
            if value < next_value {
                let valid = matches!(
                    (c, next),
                    (b'I', b'V' | b'X') | (b'X', b'L' | b'C') | (b'C', b'D' | b'M')
                );
                if !valid || repeat > 1 || seen(v, l, d, next_value) {
                    return None;
                }
                match next {
                    b'V' => v += 1,
                    b'L' => l += 1,
                    b'D' => d += 1,
                    _ => {}
                }
                i += 1;
                repeat = 1;
                subtracted = true;
                last_subtracted = value;
                result += next_value - value;
            } else {
                if c == next {
                    repeat += 1;
                    if repeat > 3 {
                        return None;
                    }
                } else {
                    repeat = 1;
                }
                result += value;
            }
            i += 1;
        }
        Some(result)
    }
}

/// `NUM_processor` for `to_number`: the sign and the digits that the input gives, and the format with the count of the digits after the point that the input has.
fn from_char(nodes: &[Node], desc: Desc, input: &[u8]) -> Result<(Vec<u8>, Desc)> {
    if desc.is(EEEE) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "\"EEEE\" not supported for input".to_string(),
        ));
    }
    let mut np = FromChar {
        desc,
        input,
        at: 0,
        number: vec![b' '],
        read_dec: false,
        read_pre: 0,
        read_post: 0,
    };
    for node in nodes {
        if np.at_end() {
            break;
        }
        let key = match *node {
            // Each character of the format that is not a pattern skips one character of the input.
            Node::Char(_) => {
                np.at += mblen(input, np.at);
                continue;
            }
            Node::Action(key) => key,
        };
        let c = input[np.at];
        match key {
            Key::Nine | Key::Zero | Key::Point | Key::D => np.numpart(key),
            Key::Comma | Key::G => {
                if np.desc.is(FILLMODE) || c != b',' {
                    continue;
                }
            }
            Key::L => {
                np.eat(1);
                continue;
            }
            Key::Rn | Key::RnLower => {
                let value = np.roman().ok_or_else(|| {
                    Error::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        "invalid Roman numeral".to_string(),
                    )
                })?;
                let digits = value.to_string();
                np.number.extend_from_slice(digits.as_bytes());
                np.desc.pre = digits.len() as i32;
                np.desc.post = 0;
                continue;
            }
            Key::Th | Key::ThLower => {
                if !np.desc.is(ROMAN | DECIMAL) && np.number[0] != b'#' {
                    np.eat(2);
                }
                continue;
            }
            Key::Mi | Key::Pl | Key::Sg => {
                let sign = match key {
                    Key::Mi => c == b'-',
                    Key::Pl => c == b'+',
                    _ => c == b'-' || c == b'+',
                };
                if !sign {
                    np.eat(1);
                    continue;
                }
                np.number[0] = c;
            }
            Key::B | Key::C | Key::Eeee | Key::Fm | Key::Pr | Key::Sp | Key::S | Key::V => continue,
        }
        np.at += 1;
    }
    if np.number.last() == Some(&b'.') {
        np.number.pop();
    }
    np.desc.post = np.read_post;
    Ok((np.number, np.desc))
}

/// `numeric_to_number`: the value of the text in the format, as a `numeric` with the precision and the scale of the format. Null for an empty format.
fn to_number(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let format = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    if format.is_empty() {
        return Ok(Value::Null);
    }
    let (nodes, desc) = parse(format)?;
    let (number, desc) = from_char(&nodes, desc, text.as_bytes())?;
    let scale = desc.post;
    let precision = desc.pre + desc.multi + scale;
    let number = String::from_utf8_lossy(&number);
    let mut result = numeric_in(&number, ((precision << 16) | scale) + 4).map_err(type_error)?;
    if desc.is(MULTI) {
        let power = Numeric::from_integer(10)
            .power(&Numeric::from_integer(-i128::from(desc.multi)))
            .map_err(type_error)?;
        result = result.mul(&power).map_err(type_error)?;
    }
    Ok(Value::Numeric(result))
}

/// The format of the second argument, or `None` when it is empty: `to_char` then gives an empty text.
fn format_of(args: &[Value]) -> Result<Option<(Vec<Node>, Desc)>> {
    let format = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    if format.is_empty() {
        return Ok(None);
    }
    parse(format).map(Some)
}

/// `int_to_roman`: the Roman numeral in upper case, or 15 `#` for a value that is not from 1 to 3999.
fn int_to_roman(number: i32) -> Vec<u8> {
    const RM1: [&str; 9] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
    const RM10: [&str; 9] = ["X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
    const RM100: [&str; 9] = ["C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
    if !(1..=3999).contains(&number) {
        return vec![b'#'; MAX_ROMAN_LEN];
    }
    let digits = number.to_string();
    let mut out = String::new();
    for (k, d) in digits.bytes().enumerate() {
        let Some(num) = (d - b'0').checked_sub(1) else { continue };
        let num = usize::from(num);
        match digits.len() - k {
            4 => out.push_str(&"M".repeat(num + 1)),
            3 => out.push_str(RM100[num]),
            2 => out.push_str(RM10[num]),
            _ => out.push_str(RM1[num]),
        }
    }
    out.into_bytes()
}

/// The digits of an overflow: a `#` for each digit of the format, with the point after the digits before it.
fn overflow(desc: &Desc) -> Vec<u8> {
    let mut number = vec![b'#'; (desc.pre + desc.post + 1) as usize];
    number[desc.pre as usize] = b'.';
    number
}

/// The digits of `NaN` or an infinity with `EEEE`: a `#` for each place of the text, with the space of the sign and the point.
fn sci_overflow(desc: &Desc) -> Vec<u8> {
    let mut number = vec![b'#'; (desc.pre + desc.post + 6) as usize];
    number[0] = b' ';
    number[desc.pre as usize + 1] = b'.';
    number
}

/// The digits and the spaces before them for the digits of the format: the spaces when the value has fewer digits before the point than the format, and an overflow when it has more.
fn fit(desc: &Desc, number: Vec<u8>) -> (Vec<u8>, i32) {
    let pre_len = number.iter().position(|&b| b == b'.').unwrap_or(number.len()) as i32;
    match pre_len.cmp(&desc.pre) {
        std::cmp::Ordering::Less => (number, desc.pre - pre_len),
        std::cmp::Ordering::Greater => (overflow(desc), 0),
        std::cmp::Ordering::Equal => (number, 0),
    }
}

/// The sign and the rest of a text of a number.
fn split_sign(text: String) -> (Vec<u8>, u8) {
    match text.strip_prefix('-') {
        Some(rest) => (rest.as_bytes().to_vec(), b'-'),
        None => (text.into_bytes(), b'+'),
    }
}

/// `%.*f` of C, with `NaN` and `Infinity` as the `snprintf` of PostgreSQL writes them.
fn c_fixed(v: f64, precision: usize) -> String {
    match v {
        _ if v.is_nan() => "NaN".to_string(),
        _ if v.is_infinite() => if v < 0.0 { "-Infinity" } else { "Infinity" }.to_string(),
        _ => format!("{v:.precision$}"),
    }
}

/// `%+.*e` of C with a space for the plus sign, as `to_char` with `EEEE` writes a float.
fn c_sci(v: f64, precision: usize) -> Vec<u8> {
    let text = format!("{v:+.precision$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mantissa =
        mantissa.strip_prefix('+').map_or_else(|| mantissa.to_string(), |m| format!(" {m}"));
    format!("{mantissa}e{exponent:+03}").into_bytes()
}

/// The text of an integer with the zeros after the point that the format has.
fn integer_number(desc: &Desc, value: i64) -> (Vec<u8>, i32, u8) {
    let sign = if value < 0 { b'-' } else { b'+' };
    let mut number = value.unsigned_abs().to_string().into_bytes();
    if desc.post > 0 {
        number.push(b'.');
        number.resize(number.len() + desc.post as usize, b'0');
    }
    let (number, spaces) = fit(desc, number);
    (number, spaces, sign)
}

fn int4_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range".to_string())
}

fn int8_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range".to_string())
}

/// The space of the plus sign before a text of `numeric_out_sci`.
fn sci_sign(text: String) -> Vec<u8> {
    match text.starts_with('-') {
        true => text.into_bytes(),
        false => format!(" {text}").into_bytes(),
    }
}

/// `int4_to_char`.
fn int4_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Int4(value)) = args.first() else { return Err(bad_value()) };
    let Some((nodes, mut desc)) = format_of(args)? else { return Ok(Value::text("")) };
    let (number, spaces, sign) = if desc.is(ROMAN) {
        (int_to_roman(value), 0, 0)
    } else if desc.is(EEEE) {
        (c_sci(f64::from(value), desc.post as usize), 0, 0)
    } else {
        let mut value = value;
        if desc.is(MULTI) {
            let multi = 10f64.powf(f64::from(desc.multi)).round_ties_even();
            if !(-2147483648.0..2147483648.0).contains(&multi) {
                return Err(int4_range());
            }
            value = value.checked_mul(multi as i32).ok_or_else(int4_range)?;
            desc.pre += desc.multi;
        }
        integer_number(&desc, i64::from(value))
    };
    to_char(&nodes, desc, &number, spaces, sign)
}

/// `int8_to_char`. `EEEE` goes through `numeric` so that no digit is lost.
fn int8_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Int8(value)) = args.first() else { return Err(bad_value()) };
    let Some((nodes, mut desc)) = format_of(args)? else { return Ok(Value::text("")) };
    let (number, spaces, sign) = if desc.is(ROMAN) {
        (int_to_roman(i32::try_from(value).unwrap_or(i32::MAX)), 0, 0)
    } else if desc.is(EEEE) {
        (sci_sign(numeric_out_sci(&Numeric::from_integer(i128::from(value)), desc.post)), 0, 0)
    } else {
        let mut value = value;
        if desc.is(MULTI) {
            let multi = 10f64.powf(f64::from(desc.multi)).round_ties_even();
            if !(-9223372036854775808.0..9223372036854775808.0).contains(&multi) {
                return Err(int8_range());
            }
            value = value.checked_mul(multi as i64).ok_or_else(int8_range)?;
            desc.pre += desc.multi;
        }
        integer_number(&desc, value)
    };
    to_char(&nodes, desc, &number, spaces, sign)
}

/// The text of a float for the digits of the format, with no more significant digits than `digits`, which is `FLT_DIG` or `DBL_DIG`.
fn float_number(desc: &mut Desc, val: f64, digits: i32) -> (Vec<u8>, i32, u8) {
    let pre_len = c_fixed(val.abs(), 0).len() as i32;
    if pre_len >= digits {
        desc.post = 0;
    } else if pre_len + desc.post > digits {
        desc.post = digits - pre_len;
    }
    let (number, sign) = split_sign(c_fixed(val, desc.post as usize));
    let (number, spaces) = fit(desc, number);
    (number, spaces, sign)
}

/// `float4_to_char`.
fn float4_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Float4(value)) = args.first() else { return Err(bad_value()) };
    let Some((nodes, mut desc)) = format_of(args)? else { return Ok(Value::text("")) };
    let (number, spaces, sign) = if desc.is(ROMAN) {
        let value = f64::from(value).round_ties_even() as f32;
        let fits = (-2147483648.0..2147483648.0).contains(&value);
        (int_to_roman(if fits { value as i32 } else { i32::MAX }), 0, 0)
    } else if desc.is(EEEE) {
        let number = match value.is_finite() {
            true => c_sci(f64::from(value), desc.post as usize),
            false => sci_overflow(&desc),
        };
        (number, 0, 0)
    } else {
        let mut val = value;
        if desc.is(MULTI) {
            val = value * 10f64.powf(f64::from(desc.multi)) as f32;
            desc.pre += desc.multi;
        }
        float_number(&mut desc, f64::from(val), 6)
    };
    to_char(&nodes, desc, &number, spaces, sign)
}

/// `float8_to_char`.
fn float8_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(&Value::Float8(value)) = args.first() else { return Err(bad_value()) };
    let Some((nodes, mut desc)) = format_of(args)? else { return Ok(Value::text("")) };
    let (number, spaces, sign) = if desc.is(ROMAN) {
        let value = value.round_ties_even();
        let fits = (-2147483648.0..2147483648.0).contains(&value);
        (int_to_roman(if fits { value as i32 } else { i32::MAX }), 0, 0)
    } else if desc.is(EEEE) {
        let number = match value.is_finite() {
            true => c_sci(value, desc.post as usize),
            false => sci_overflow(&desc),
        };
        (number, 0, 0)
    } else {
        let mut val = value;
        if desc.is(MULTI) {
            val = value * 10f64.powf(f64::from(desc.multi));
            desc.pre += desc.multi;
        }
        float_number(&mut desc, val, 15)
    };
    to_char(&nodes, desc, &number, spaces, sign)
}

/// `numeric_to_char`.
fn numeric_to_char(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Numeric(value)) = args.first() else { return Err(bad_value()) };
    let Some((nodes, mut desc)) = format_of(args)? else { return Ok(Value::text("")) };
    let (number, spaces, sign) = if desc.is(ROMAN) {
        // `numeric_int4_safe` rounds, and a value that does not fit is `INT_MAX`.
        let value = value.to_integer("integer").ok().and_then(|v| i32::try_from(v).ok());
        (int_to_roman(value.unwrap_or(i32::MAX)), 0, 0)
    } else if desc.is(EEEE) {
        let number = match value.sign() {
            NumericSign::Positive | NumericSign::Negative => {
                sci_sign(numeric_out_sci(value, desc.post))
            }
            _ => sci_overflow(&desc),
        };
        (number, 0, 0)
    } else {
        let mut val = value.clone();
        if desc.is(MULTI) {
            let power = Numeric::from_integer(10)
                .power(&Numeric::from_integer(i128::from(desc.multi)))
                .map_err(type_error)?;
            val = val.mul(&power).map_err(type_error)?;
            desc.pre += desc.multi;
        }
        let rounded = val.round(desc.post).map_err(type_error)?;
        let mut text = Vec::new();
        numeric_out(&rounded, &mut text);
        let (number, sign) = split_sign(String::from_utf8_lossy(&text).into_owned());
        let (number, spaces) = fit(&desc, number);
        (number, spaces, sign)
    };
    to_char(&nodes, desc, &number, spaces, sign)
}

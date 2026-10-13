//! The `tsquery` type of text search, from `tsqueryin`, `tsqueryout`, `tsqueryrecv` and `tsquerysend` of `tsquery.c`, with the order of `tsquery_op.c` and `tsquery_util.c`. A value is the list of its operands and operators in prefix order, as PostgreSQL keeps it: an operator comes first, then its right operand, then its left operand. The operator `!` has only the right operand.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::tsvector::{TSVECTOR_MAX_DATA, TSVECTOR_MAX_WORD, is_space, ts_compare_string};

/// `OP_NOT`: the operator `!`.
pub const TSQUERY_NOT: u8 = 1;

/// `OP_AND`: the operator `&`.
pub const TSQUERY_AND: u8 = 2;

/// `OP_OR`: the operator `|`.
pub const TSQUERY_OR: u8 = 3;

/// `OP_PHRASE`: the operator `<N>`, which `<->` writes when the distance is 1.
pub const TSQUERY_PHRASE: u8 = 4;

/// `MAXENTRYPOS`: the largest distance of the operator `<N>`.
pub const TSQUERY_MAX_DISTANCE: u16 = 1 << 14;

/// `STACKDEPTH`: the number of operators that can wait for their right operand in one level of parentheses.
const STACK_DEPTH: usize = 32;

/// The deepest tree that a value can have. PostgreSQL stops a deep tree when it reaches `max_stack_depth`, so its limit changes with the size of the frames of the functions. The functions here walk a tree by recursion, and this limit keeps them on the stack. It is the limit of the analyzer.
const MAX_DEPTH: usize = 1000;

/// The largest number of items in a binary value: `MaxAllocSize` divided by the size of a `QueryItem`.
const MAX_ITEMS: u32 = 0x3fff_ffff / 12;

/// One item of a `tsquery`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryItem {
    /// `QI_VAL`: a lexeme, the bits of its weights, 8 for `A` down to 1 for `D`, and true for a prefix match.
    Operand { word: String, weight: u8, prefix: bool },
    /// `QI_OPR`: one of the `TSQUERY_` operators. The right operand is the next item, and the left operand is `left` items after the operator. `distance` is the distance of `<N>` and 0 for the other operators.
    Operator { op: u8, distance: u16, left: u32 },
}

/// `tsearch_op_priority`: `!` binds the most, then `<N>`, `&` and `|`.
fn priority(op: u8) -> i32 {
    match op {
        TSQUERY_NOT => 4,
        TSQUERY_PHRASE => 3,
        TSQUERY_AND => 2,
        _ => 1,
    }
}

/// `ISOPERATOR`: a character that ends a lexeme of a query.
fn is_operator(c: char) -> bool {
    matches!(c, '!' | '&' | '|' | '(' | ')' | '<')
}

/// The error of `check_stack_depth`.
pub(crate) fn too_deep() -> TypeError {
    let mut error =
        TypeError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded".to_string());
    error.hint = Some("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.".to_string());
    error
}

/// The error of `elog`.
fn internal(message: String) -> TypeError {
    TypeError::new(SqlState::INTERNAL_ERROR, message)
}

/// The error of a distance of `<N>` that is out of range.
pub fn tsquery_bad_distance() -> TypeError {
    TypeError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "distance in phrase operator must be an integer value between zero and {TSQUERY_MAX_DISTANCE} inclusive"
        ),
    )
}

/// The table of CRC-32, `pg_crc32_table`.
fn crc_table(index: u32) -> u32 {
    let mut c = index;
    for _ in 0..8 {
        c = if c & 1 == 0 { c >> 1 } else { (c >> 1) ^ 0xedb8_8320 };
    }
    c
}

/// `COMP_LEGACY_CRC32`, the first key of the order of two operands. It uses the table of CRC-32 with the steps of the other bit order, so it is not the CRC-32 of zlib.
pub fn legacy_crc32(bytes: &[u8]) -> i32 {
    let mut crc = u32::MAX;
    for &b in bytes {
        crc = crc_table(((crc >> 24) ^ u32::from(b)) & 0xff) ^ (crc << 8);
    }
    (!crc).cast_signed()
}

/// `findoprnd`: sets `left` of each operator and checks that the items make one tree.
///
/// # Errors
///
/// `XX000` for items that are not one tree, and `54001` for a tree that is too deep.
fn link(items: &mut [QueryItem]) -> Result<(), TypeError> {
    // The operators that wait for an operand, with the number of operands that they wait for.
    let mut open: Vec<(usize, u8)> = Vec::new();
    let mut lefts = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 && open.is_empty() {
            return Err(internal("malformed tsquery: extra nodes".to_string()));
        }
        if open.len() >= MAX_DEPTH {
            return Err(too_deep());
        }
        match item {
            QueryItem::Operator { op, .. } => {
                open.push((i, if *op == TSQUERY_NOT { 1 } else { 2 }))
            }
            QueryItem::Operand { .. } => {
                // The operand ends the operators that have all their operands now.
                while let Some((at, wait)) = open.last_mut() {
                    *wait -= 1;
                    if *wait == 1 {
                        lefts.push((*at, i + 1 - *at));
                        break;
                    }
                    open.pop();
                }
            }
        }
    }
    if items.is_empty() || !open.is_empty() {
        return Err(internal("malformed tsquery: operand not found".to_string()));
    }
    for (at, offset) in lefts {
        if let QueryItem::Operator { left, .. } = &mut items[at] {
            *left = u32::try_from(offset).unwrap_or(u32::MAX);
        }
    }
    Ok(())
}

/// The states of `gettoken_query_standard`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    FirstOperand,
    Operand,
    Operator,
}

/// The states of `gettoken_tsvector` for the lexeme of a query.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    Start,
    End,
    Next,
    EndComplex,
    CharComplex,
}

/// A token of a query.
enum Token {
    End,
    /// `PT_ERR`, which is a syntax error.
    Bad,
    Operand(String, u8, bool),
    Operator(u8, u16),
    Open,
    Close,
}

/// The parser of the text of a `tsquery`.
struct Parser<'a> {
    text: &'a str,
    at: usize,
    /// The number of parentheses that are open.
    count: i32,
    wait: Wait,
    /// The items in postfix order.
    items: Vec<QueryItem>,
    /// The bytes of the operands with a zero byte after each one.
    data: usize,
}

impl Parser<'_> {
    fn syntax(&self) -> TypeError {
        TypeError::new(
            SqlState::SYNTAX_ERROR,
            format!("syntax error in tsquery: \"{}\"", self.text),
        )
    }

    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    /// `gettoken_tsvector` with the flags of a query: a lexeme ends at an operator or a colon, and has no positions. `None` at the end of the text.
    fn word(&mut self) -> Result<Option<String>, TypeError> {
        let mut word = String::new();
        let mut state = Word::Start;
        let mut old = Word::End;
        loop {
            let c = self.peek();
            match state {
                Word::Start => match c {
                    None => return Ok(None),
                    Some('\'') => state = Word::EndComplex,
                    Some('\\') => {
                        state = Word::Next;
                        old = Word::End;
                    }
                    Some(c) if is_operator(c) => return Err(self.syntax()),
                    Some(c) if is_space(c) => {}
                    Some(c) => {
                        word.push(c);
                        state = Word::End;
                    }
                },
                Word::Next => match c {
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
                Word::End => match c {
                    Some('\\') => {
                        state = Word::Next;
                        old = Word::End;
                    }
                    None => break,
                    Some(c) if is_space(c) || is_operator(c) || c == ':' => break,
                    Some(c) => word.push(c),
                },
                Word::EndComplex => match c {
                    Some('\'') => state = Word::CharComplex,
                    Some('\\') => {
                        state = Word::Next;
                        old = Word::EndComplex;
                    }
                    None => return Err(self.syntax()),
                    Some(c) => word.push(c),
                },
                Word::CharComplex => {
                    if c != Some('\'') {
                        if word.is_empty() {
                            return Err(self.syntax());
                        }
                        break;
                    }
                    word.push('\'');
                    state = Word::EndComplex;
                }
            }
            self.at += c.map_or(0, char::len_utf8);
        }
        Ok(Some(word))
    }

    /// `get_modifiers`: the weights and the prefix flag after a colon, such as `:AB*`. Any other byte ends them.
    fn modifiers(&mut self) -> (u8, bool) {
        let bytes = self.text.as_bytes();
        let (mut weight, mut prefix) = (0, false);
        if bytes.get(self.at) != Some(&b':') {
            return (weight, prefix);
        }
        self.at += 1;
        while let Some(&b) = bytes.get(self.at) {
            match b {
                b'a' | b'A' => weight |= 8,
                b'b' | b'B' => weight |= 4,
                b'c' | b'C' => weight |= 2,
                b'd' | b'D' => weight |= 1,
                b'*' => prefix = true,
                _ => break,
            }
            self.at += 1;
        }
        (weight, prefix)
    }

    /// `parse_phrase_operator` at a `<`: the distance of `<->` or `<N>`, or `None` when the text is not one of them. The operator must have a character after it.
    fn phrase(&mut self) -> Result<Option<u16>, TypeError> {
        let bytes = self.text.as_bytes();
        let mut p = self.at + 1;
        let distance = match bytes.get(p) {
            Some(b'-') => {
                p += 1;
                1
            }
            Some(d) if d.is_ascii_digit() => {
                let digits = bytes[p..].iter().take_while(|b| b.is_ascii_digit()).count();
                // strtol gives ERANGE above the range of long, and the check takes that as out of range too.
                let value = bytes[p..p + digits].iter().try_fold(0u64, |v, d| {
                    v.checked_mul(10).and_then(|v| v.checked_add(u64::from(d - b'0')))
                });
                p += digits;
                match value.and_then(|v| u16::try_from(v).ok()) {
                    Some(v) if v <= TSQUERY_MAX_DISTANCE => v,
                    _ => return Err(tsquery_bad_distance()),
                }
            }
            _ => return Ok(None),
        };
        if bytes.get(p) != Some(&b'>') || p + 1 >= bytes.len() {
            return Ok(None);
        }
        self.at = p + 1;
        Ok(Some(distance))
    }

    /// `gettoken_query_standard`.
    fn token(&mut self) -> Result<Token, TypeError> {
        loop {
            let c = self.peek();
            match self.wait {
                Wait::FirstOperand | Wait::Operand => match c {
                    Some('!') => {
                        self.at += 1;
                        self.wait = Wait::Operand;
                        return Ok(Token::Operator(TSQUERY_NOT, 0));
                    }
                    Some('(') => {
                        self.at += 1;
                        self.wait = Wait::Operand;
                        self.count += 1;
                        return Ok(Token::Open);
                    }
                    Some(':') => return Ok(Token::Bad),
                    Some(c) if is_space(c) => {}
                    _ => {
                        return match self.word()? {
                            Some(word) => {
                                let (weight, prefix) = self.modifiers();
                                self.wait = Wait::Operator;
                                Ok(Token::Operand(word, weight, prefix))
                            }
                            None if self.wait == Wait::FirstOperand => Ok(Token::End),
                            None => Err(TypeError::new(
                                SqlState::SYNTAX_ERROR,
                                format!("no operand in tsquery: \"{}\"", self.text),
                            )),
                        };
                    }
                },
                Wait::Operator => match c {
                    Some('&') => {
                        self.at += 1;
                        self.wait = Wait::Operand;
                        return Ok(Token::Operator(TSQUERY_AND, 0));
                    }
                    Some('|') => {
                        self.at += 1;
                        self.wait = Wait::Operand;
                        return Ok(Token::Operator(TSQUERY_OR, 0));
                    }
                    Some('<') => {
                        let Some(distance) = self.phrase()? else { return Ok(Token::Bad) };
                        self.wait = Wait::Operand;
                        return Ok(Token::Operator(TSQUERY_PHRASE, distance));
                    }
                    Some(')') => {
                        self.at += 1;
                        self.count -= 1;
                        return Ok(if self.count < 0 { Token::Bad } else { Token::Close });
                    }
                    None => return Ok(if self.count == 0 { Token::End } else { Token::Bad }),
                    Some(c) if is_space(c) => {}
                    Some(_) => return Ok(Token::Bad),
                },
            }
            self.at += c.map_or(0, char::len_utf8);
        }
    }

    /// `pushValue`.
    fn push_operand(&mut self, word: String, weight: u8, prefix: bool) -> Result<(), TypeError> {
        if word.len() > TSVECTOR_MAX_WORD {
            return Err(TypeError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("word is too long in tsquery: \"{}\"", self.text),
            ));
        }
        if self.data > TSVECTOR_MAX_DATA {
            return Err(TypeError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("value is too big in tsquery: \"{}\"", self.text),
            ));
        }
        self.data += word.len() + 1;
        self.items.push(QueryItem::Operand { word, weight, prefix });
        Ok(())
    }

    /// `cleanOpStack`: moves the operators that bind at least as much as `op` to the items. `!` binds to the right, so it moves only the operators that bind more.
    fn clean(&mut self, stack: &mut Vec<(u8, u16)>, op: u8) {
        let p = priority(op);
        while let Some(&(top, distance)) = stack.last() {
            let q = priority(top);
            if (op != TSQUERY_NOT && p > q) || (op == TSQUERY_NOT && p >= q) {
                break;
            }
            stack.pop();
            self.items.push(QueryItem::Operator { op: top, distance, left: 0 });
        }
    }

    /// `makepol`: the items of one level of parentheses in postfix order.
    fn makepol(&mut self, depth: usize) -> Result<(), TypeError> {
        if depth >= MAX_DEPTH {
            return Err(too_deep());
        }
        let mut stack: Vec<(u8, u16)> = Vec::new();
        loop {
            match self.token()? {
                Token::End => break,
                Token::Operand(word, weight, prefix) => self.push_operand(word, weight, prefix)?,
                Token::Operator(op, distance) => {
                    self.clean(&mut stack, op);
                    if stack.len() == STACK_DEPTH {
                        return Err(internal("tsquery stack too small".to_string()));
                    }
                    stack.push((op, distance));
                }
                Token::Open => self.makepol(depth + 1)?,
                Token::Close => break,
                Token::Bad => return Err(self.syntax()),
            }
        }
        self.clean(&mut stack, TSQUERY_OR);
        Ok(())
    }
}

/// `tsqueryin`: lexemes with the operators `!`, `&`, `|`, `<->` and `<N>` and parentheses. A lexeme can be in quotes, as in a `tsvector`, and can have weights and `*` after a colon. A query with no lexemes is empty, and the caller sends the notice of PostgreSQL for it.
///
/// # Errors
///
/// `42601` for bad syntax, `22023` for a distance out of range, `54000` for a lexeme or a query that is too long, `XX000` for more than 32 operators that wait in one level of parentheses, and `54001` for a query that is too deep.
pub fn tsquery_parse(text: &str) -> Result<Vec<QueryItem>, TypeError> {
    let mut parser =
        Parser { text, at: 0, count: 0, wait: Wait::FirstOperand, items: Vec::new(), data: 0 };
    parser.makepol(0)?;
    let mut items = parser.items;
    if items.is_empty() {
        return Ok(items);
    }
    items.reverse();
    link(&mut items)?;
    Ok(items)
}

/// `infix`: the item at `at` and its operands. An operator that binds less than its parent has parentheses, and so has a `<N>` on the right side of a `<N>`.
fn infix(items: &[QueryItem], at: usize, parent: i32, right_phrase: bool, out: &mut Vec<u8>) {
    match &items[at] {
        QueryItem::Operand { word, weight, prefix } => {
            out.push(b'\'');
            for &b in word.as_bytes() {
                if b == b'\'' || b == b'\\' {
                    out.push(b);
                }
                out.push(b);
            }
            out.push(b'\'');
            if *weight != 0 || *prefix {
                out.push(b':');
                if *prefix {
                    out.push(b'*');
                }
                for (bit, letter) in [(8, b'A'), (4, b'B'), (2, b'C'), (1, b'D')] {
                    if weight & bit != 0 {
                        out.push(letter);
                    }
                }
            }
        }
        QueryItem::Operator { op, distance, left } => {
            let p = priority(*op);
            let paren = p < parent || (*op == TSQUERY_PHRASE && right_phrase);
            if paren {
                out.extend_from_slice(b"( ");
            }
            if *op == TSQUERY_NOT {
                out.push(b'!');
            } else {
                infix(items, at + *left as usize, p, false, out);
                match *op {
                    TSQUERY_OR => out.extend_from_slice(b" | "),
                    TSQUERY_AND => out.extend_from_slice(b" & "),
                    _ if *distance == 1 => out.extend_from_slice(b" <-> "),
                    _ => out.extend_from_slice(format!(" <{distance}> ").as_bytes()),
                }
            }
            infix(items, at + 1, p, *op == TSQUERY_PHRASE, out);
            if paren {
                out.extend_from_slice(b" )");
            }
        }
    }
}

/// `tsqueryout`: the query with the operators between their operands, each lexeme in quotes, and parentheses where the order of the operators needs them. An empty query is an empty string.
pub fn tsquery_deparse(items: &[QueryItem], out: &mut Vec<u8>) {
    if !items.is_empty() {
        infix(items, 0, -1, false, out);
    }
}

/// `tsquerysend`: the number of items, then each item in prefix order. An operand is the type 1, its weights, its prefix flag and the lexeme with a zero byte after it. An operator is the type 2 and the operator, with the distance as two bytes for `<N>`.
pub fn tsquery_send(items: &[QueryItem], out: &mut Vec<u8>) {
    let count = i32::try_from(items.len()).unwrap_or(i32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for item in items {
        match item {
            QueryItem::Operand { word, weight, prefix } => {
                out.extend_from_slice(&[1, *weight, u8::from(*prefix)]);
                out.extend_from_slice(word.as_bytes());
                out.push(0);
            }
            QueryItem::Operator { op, distance, .. } => {
                out.extend_from_slice(&[2, *op]);
                if *op == TSQUERY_PHRASE {
                    out.extend_from_slice(&distance.to_be_bytes());
                }
            }
        }
    }
}

/// `tsqueryrecv`: the binary form of [`tsquery_send`].
///
/// # Errors
///
/// `08P01` for a value that is too short or a lexeme with no zero byte after it, `22021` for a lexeme that is not UTF-8, `XX000` for a bad item or items that are not one tree, which includes an empty query, and `54001` for a query that is too deep.
pub fn tsquery_recv(recv: &mut Recv<'_>) -> Result<Vec<QueryItem>, TypeError> {
    let size = recv.u32()?;
    if size > MAX_ITEMS {
        return Err(internal("invalid size of tsquery".to_string()));
    }
    let mut items = Vec::with_capacity(recv.remaining().min(size as usize));
    let mut data = 0;
    for i in 0..size {
        let kind = recv.bytes(1)?[0].cast_signed();
        match kind {
            1 => {
                let weight = recv.bytes(1)?[0];
                let prefix = recv.bytes(1)?[0] != 0;
                let word = recv.cstring()?;
                let error = if weight > 0xf {
                    "invalid tsquery: invalid weight bitmap"
                } else if word.is_empty() {
                    "invalid tsquery: empty operand"
                } else if word.len() > TSVECTOR_MAX_WORD {
                    "invalid tsquery: operand too long"
                } else if data > TSVECTOR_MAX_DATA {
                    "invalid tsquery: total operand length exceeded"
                } else {
                    ""
                };
                if !error.is_empty() {
                    return Err(internal(error.to_string()));
                }
                data += word.len() + 1;
                items.push(QueryItem::Operand { word: word.to_string(), weight, prefix });
            }
            2 => {
                let op = recv.bytes(1)?[0].cast_signed();
                let Ok(op @ TSQUERY_NOT..=TSQUERY_PHRASE) = u8::try_from(op) else {
                    return Err(internal(format!(
                        "invalid tsquery: unrecognized operator type {op}"
                    )));
                };
                if i == size - 1 {
                    return Err(internal("invalid pointer to right operand".to_string()));
                }
                let mut distance = 0;
                if op == TSQUERY_PHRASE {
                    distance = recv.i16()?.cast_unsigned();
                    if distance > TSQUERY_MAX_DISTANCE {
                        return Err(internal(format!(
                            "invalid tsquery: invalid phrase distance {distance}"
                        )));
                    }
                }
                items.push(QueryItem::Operator { op, distance, left: 0 });
            }
            _ => return Err(internal(format!("unrecognized tsquery node type: {kind}"))),
        }
    }
    link(&mut items)?;
    Ok(items)
}

/// The bytes of the lexemes of a query with a zero byte after each one.
fn data_len(items: &[QueryItem]) -> usize {
    let words = items.iter().filter_map(|item| match item {
        QueryItem::Operand { word, .. } => Some(word.len() + 1),
        QueryItem::Operator { .. } => None,
    });
    words.sum()
}

/// `QTNodeCompare`: an operator comes before an operand, and a larger operator code first. The operands of two equal operators compare from the right one, then a larger distance comes first. Two operands compare by the CRC of the lexeme, the larger first, then by the bytes. The weights and the prefix flag do not count.
fn node_cmp(a: &[QueryItem], i: usize, b: &[QueryItem], j: usize) -> i32 {
    match (&a[i], &b[j]) {
        (QueryItem::Operator { .. }, QueryItem::Operand { .. }) => -1,
        (QueryItem::Operand { .. }, QueryItem::Operator { .. }) => 1,
        (
            QueryItem::Operator { op: x, distance: dx, left: lx },
            QueryItem::Operator { op: y, distance: dy, left: ly },
        ) => {
            if x != y {
                return if x > y { -1 } else { 1 };
            }
            let right = node_cmp(a, i + 1, b, j + 1);
            if right != 0 {
                return right;
            }
            if *x != TSQUERY_NOT {
                let left = node_cmp(a, i + *lx as usize, b, j + *ly as usize);
                if left != 0 {
                    return left;
                }
            }
            if *x == TSQUERY_PHRASE && dx != dy {
                return if dx > dy { -1 } else { 1 };
            }
            0
        }
        (QueryItem::Operand { word: x, .. }, QueryItem::Operand { word: y, .. }) => {
            let (cx, cy) = (legacy_crc32(x.as_bytes()), legacy_crc32(y.as_bytes()));
            if cx != cy {
                return if cx > cy { -1 } else { 1 };
            }
            ts_compare_string(x.as_bytes(), y.as_bytes())
        }
    }
}

/// `CompareTSQ`, the order of the B-tree operator class: the number of items, the size of the value, then the trees.
pub fn tsquery_cmp(a: &[QueryItem], b: &[QueryItem]) -> i32 {
    if a.len() != b.len() {
        return if a.len() < b.len() { -1 } else { 1 };
    }
    let (x, y) = (data_len(a), data_len(b));
    if x != y {
        return if x < y { -1 } else { 1 };
    }
    if a.is_empty() { 0 } else { node_cmp(a, 0, b, 0) }
}

/// The items of an operator with its right and left operands.
fn join(op: u8, distance: u16, right: Vec<QueryItem>, left: Vec<QueryItem>) -> Vec<QueryItem> {
    let offset = u32::try_from(right.len() + 1).unwrap_or(u32::MAX);
    let mut items = Vec::with_capacity(1 + right.len() + left.len());
    items.push(QueryItem::Operator { op, distance, left: offset });
    items.extend(right);
    items.extend(left);
    items
}

/// `join_tsqueries` and `QTN2QT`: the query `left op right`, or `!right` for `TSQUERY_NOT` with an empty `left`.
///
/// # Errors
///
/// `54000` when the lexemes are too long for one query, and `54001` for a query that is too deep.
pub fn tsquery_join(
    op: u8,
    distance: u16,
    left: &[QueryItem],
    right: &[QueryItem],
) -> Result<Vec<QueryItem>, TypeError> {
    let mut items = if op == TSQUERY_NOT {
        let mut items = vec![QueryItem::Operator { op, distance: 0, left: 1 }];
        items.extend_from_slice(right);
        items
    } else {
        join(op, distance, right.to_vec(), left.to_vec())
    };
    // `fillQT` checks the place of each lexeme in the data of the new value.
    let mut data = 0;
    for item in &items {
        if let QueryItem::Operand { word, .. } = item {
            if data > TSVECTOR_MAX_DATA {
                return Err(TypeError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "tsquery is too large".to_string(),
                ));
            }
            data += word.len() + 1;
        }
    }
    link(&mut items)?;
    Ok(items)
}

/// `clean_NOT_intree`: the tree with no `!`. A `|` with a `!` under it goes, and a `&` or a `<N>` keeps the operand that has no `!`.
fn clean_not(items: &[QueryItem], at: usize) -> Option<Vec<QueryItem>> {
    match &items[at] {
        QueryItem::Operand { .. } => Some(vec![items[at].clone()]),
        QueryItem::Operator { op: TSQUERY_NOT, .. } => None,
        QueryItem::Operator { op: TSQUERY_OR, left, .. } => {
            let left = clean_not(items, at + *left as usize)?;
            let right = clean_not(items, at + 1)?;
            Some(join(TSQUERY_OR, 0, right, left))
        }
        QueryItem::Operator { op, distance, left } => {
            let left = clean_not(items, at + *left as usize);
            let right = clean_not(items, at + 1);
            match (left, right) {
                (Some(left), Some(right)) => Some(join(*op, *distance, right, left)),
                (left, right) => left.or(right),
            }
        }
    }
}

/// `tsquerytree`: the text of the query with no `!`, as an index uses it. `T` when nothing is left, and an empty string for an empty query.
pub fn tsquery_tree(items: &[QueryItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let Some(tree) = clean_not(items, 0) else { return "T".to_string() };
    let mut out = Vec::new();
    tsquery_deparse(&tree, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// The lexemes of a query in byte order with no two equal lexemes.
fn sorted_words(items: &[QueryItem]) -> Vec<&[u8]> {
    let mut words: Vec<&[u8]> = items
        .iter()
        .filter_map(|item| match item {
            QueryItem::Operand { word, .. } => Some(word.as_bytes()),
            QueryItem::Operator { .. } => None,
        })
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

/// `tsq_mcontains`: true when each lexeme of `ex` is a lexeme of `query`. The operators, the weights and the prefix flags do not count.
pub fn tsquery_contains(query: &[QueryItem], ex: &[QueryItem]) -> bool {
    let (query, ex) = (sorted_words(query), sorted_words(ex));
    ex.iter().all(|word| query.binary_search(word).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> String {
        let mut out = Vec::new();
        tsquery_deparse(&tsquery_parse(input).unwrap(), &mut out);
        String::from_utf8(out).unwrap()
    }

    fn error(input: &str) -> (String, String) {
        let error = tsquery_parse(input).unwrap_err();
        (error.sqlstate.as_str().to_string(), error.message)
    }

    #[test]
    fn parse_and_deparse() {
        assert_eq!(text("a & b | c & d"), "'a' & 'b' | 'c' & 'd'");
        assert_eq!(text("(a | b) & (c | d)"), "( 'a' | 'b' ) & ( 'c' | 'd' )");
        assert_eq!(text("a & !(b | c)"), "'a' & !( 'b' | 'c' )");
        assert_eq!(text("a <-> (b <-> c)"), "'a' <-> ( 'b' <-> 'c' )");
        assert_eq!(text("(a <-> b) <-> c"), "'a' <-> 'b' <-> 'c'");
        assert_eq!(text("a | b & !c <-> d"), "'a' | 'b' & !'c' <-> 'd'");
        assert_eq!(text("a <0> b & c <1> d"), "'a' <0> 'b' & 'c' <-> 'd'");
        assert_eq!(text("a:dcba* & b:"), "'a':*ABCD & 'b'");
        assert_eq!(text(r"'it''s' & c\'d & e\\f"), r"'it''s' & 'c''d' & 'e\\f'");
        assert_eq!(text("aé&ß"), "'aé' & 'ß'");
        assert_eq!(tsquery_parse("   ").unwrap(), Vec::new());
    }

    #[test]
    fn parse_errors() {
        let syntax =
            |input: &str| ("42601".to_string(), format!("syntax error in tsquery: \"{input}\""));
        for input in [
            "& a", "a b", "a & (b", "a & b)", "()", "a <-", "a <->", "a <x> b", "a:x", ":a", "'a",
            "''", "a<b",
        ] {
            assert_eq!(error(input), syntax(input), "{input}");
        }
        assert_eq!(
            error("a &"),
            ("42601".to_string(), "no operand in tsquery: \"a &\"".to_string())
        );
        assert_eq!(error("!").1, "no operand in tsquery: \"!\"");
        assert_eq!(error(r"a\").1, r#"there is no escaped character: "a\""#);
        assert_eq!(error("a <16385> b").0, "22023");
        assert_eq!(error("a <99999999999999999999> b").0, "22023");
        assert_eq!(text("a <00016384> b"), "'a' <16384> 'b'");
        assert_eq!(error(&"x".repeat(2048)).0, "54000");
        assert_eq!(
            error(&format!("{}a", "!".repeat(33))),
            ("XX000".to_string(), "tsquery stack too small".to_string())
        );
        assert!(tsquery_parse(&format!("{}a", "!".repeat(32))).is_ok());
        assert_eq!(error(&format!("{}a", "(".repeat(1000))).0, "54001");
        assert!(tsquery_parse(&format!("{}a{}", "(".repeat(999), ")".repeat(999))).is_ok());
    }

    #[test]
    fn order() {
        let cmp =
            |a: &str, b: &str| tsquery_cmp(&tsquery_parse(a).unwrap(), &tsquery_parse(b).unwrap());
        assert_eq!(legacy_crc32(b"a"), 397_917_884);
        assert_eq!(legacy_crc32(b"b"), -1_900_089_594);
        assert_eq!(
            (cmp("a", "b"), cmp("b", "a"), cmp("a", "a & b"), cmp("ab", "a")),
            (-1, 1, -1, 1)
        );
        assert_eq!((cmp("a & b", "a | b"), cmp("a <1> b", "a <2> b"), cmp("!a", "b")), (1, 1, 1));
        assert_eq!((cmp("a:A", "a"), cmp("a:*", "a"), cmp("a & b", "b & a")), (0, 0, 1));
    }

    #[test]
    fn functions() {
        let q = |input: &str| tsquery_parse(input).unwrap();
        let show = |items: &[QueryItem]| {
            let mut out = Vec::new();
            tsquery_deparse(items, &mut out);
            String::from_utf8(out).unwrap()
        };
        let tree = |input: &str| tsquery_tree(&q(input));
        assert_eq!(
            [tree("a & !b"), tree("!a"), tree("!a | b"), tree("a <-> !b"), tree("(a | !b) & c")],
            ["'a'", "T", "T", "'a'", "'c'"]
        );
        assert_eq!(
            show(&tsquery_join(TSQUERY_AND, 0, &q("a | b"), &q("c")).unwrap()),
            "( 'a' | 'b' ) & 'c'"
        );
        assert_eq!(
            show(&tsquery_join(TSQUERY_PHRASE, 1, &q("a"), &q("b <-> c")).unwrap()),
            "'a' <-> ( 'b' <-> 'c' )"
        );
        assert_eq!(
            show(&tsquery_join(TSQUERY_NOT, 0, &[], &q("a & b")).unwrap()),
            "!( 'a' & 'b' )"
        );
        assert!(tsquery_contains(&q("a & b"), &q("a")));
        assert!(!tsquery_contains(&q("a"), &q("a & b")));
        assert!(tsquery_contains(&q("a & a"), &q("a & a & a")));
    }

    #[test]
    fn binary() {
        let items = tsquery_parse("x <3> !y:*").unwrap();
        let mut out = Vec::new();
        tsquery_send(&items, &mut out);
        assert_eq!(out, b"\0\0\0\x04\x02\x04\x00\x03\x02\x01\x01\x00\x01y\x00\x01\x00\x00x\x00");
        assert_eq!(tsquery_recv(&mut Recv::new(&out)).unwrap(), items);
        let message = |bytes: &[u8]| tsquery_recv(&mut Recv::new(bytes)).unwrap_err().message;
        assert_eq!(message(b"\0\0\0\0"), "malformed tsquery: operand not found");
        assert_eq!(message(b"\0\0\0\x02\x01\0\0a\0\x01\0\0b\0"), "malformed tsquery: extra nodes");
        assert_eq!(message(b"\0\0\0\x01\x02\x02"), "invalid pointer to right operand");
        assert_eq!(message(b"\0\0\0\x02\x02\x09"), "invalid tsquery: unrecognized operator type 9");
        assert_eq!(message(b"\0\0\0\x01\x01\x10\0a\0"), "invalid tsquery: invalid weight bitmap");
        assert_eq!(message(b"\0\0\0\x01\x01\0\0\0"), "invalid tsquery: empty operand");
        assert_eq!(message(b"\0\0\0\x01\xff"), "unrecognized tsquery node type: -1");
        assert_eq!(message(b"\x7f\0\0\0"), "invalid size of tsquery");
        assert_eq!(
            message(b"\0\0\0\x03\x02\x04\x40\x01"),
            "invalid tsquery: invalid phrase distance 16385"
        );
    }
}

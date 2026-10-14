//! The content parser of the `xml` checker, from libxml2 2.9.14 `parser.c` and the SAX2 handlers of `SAX2.c`: character references, character data, comments, processing instructions, entity references, tags and attributes.

use std::cell::RefCell;
use std::rc::Rc;

use super::ctxt::{
    BIG_BUFFER_SIZE, BUFFER_SIZE, Code, Ctxt, Kind, MAX_DEPTH, MAX_TEXT_LENGTH, Node,
    PARSE_DTDVALID, PARSE_NOENT, StartTag, State, at, copy_buf, cstr, is_blank, is_byte_char,
    is_char,
};
use super::dtd::{Entity, EntityType, zero_content};

/// `XML_SUBSTITUTE_REF`.
pub(super) const SUBSTITUTE_REF: u32 = 1;
/// `XML_SUBSTITUTE_PEREF`.
pub(super) const SUBSTITUTE_PEREF: u32 = 2;

/// A name, a prefix or a value that can be missing.
type Name = Option<Vec<u8>>;

/// `xmlW3CPIs`.
const W3C_PIS: [&[u8]; 2] = [b"xml-stylesheet", b"xml-model"];

/// `test_char_data`: the bytes that the fast path of `xmlParseCharData` copies.
fn test_char_data(c: u8) -> bool {
    c == 0x09 || ((0x20..=0x7F).contains(&c) && c != b'&' && c != b'<' && c != b']')
}

/// The length of the UTF-8 form of a character, as `COPY_BUF` writes it.
fn utf8_len(c: u32) -> usize {
    if c < 0x80 {
        1
    } else if c < 0x800 {
        2
    } else if c < 0x10000 {
        3
    } else {
        4
    }
}

/// Appends a summary of entity children to a node, as `xmlAddChild` does for each copied child. Only the first child can merge with the last text child of the node.
fn append_children(node: &mut Node, children: &Node) {
    if children.count == 0 {
        return;
    }
    match children.first {
        Kind::Text(len) => node.add_text(len),
        kind => node.add(kind),
    }
    if children.count > 1 {
        node.last = children.last;
        node.count += children.count - 1;
    }
}

impl Ctxt {
    // Character references.

    /// `xmlParseCharRef`.
    pub(super) fn parse_char_ref(&mut self) -> u32 {
        let mut val: u32 = 0;
        let mut count = 0;
        if self.cur() == b'&' && self.nxt(1) == b'#' && self.nxt(2) == b'x' {
            self.skip(3);
            while self.cur() != b';' {
                let over = count > 20;
                count += 1;
                if over {
                    count = 0;
                    if self.instate == State::Eof {
                        return 0;
                    }
                }
                let c = self.cur();
                if c.is_ascii_digit() {
                    val = val * 16 + u32::from(c - b'0');
                } else if (b'a'..=b'f').contains(&c) && count < 20 {
                    val = val * 16 + u32::from(c - b'a') + 10;
                } else if (b'A'..=b'F').contains(&c) && count < 20 {
                    val = val * 16 + u32::from(c - b'A') + 10;
                } else {
                    self.fatal_err(Code::InvalidHexCharref, None);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                self.next();
                count += 1;
            }
            if self.cur() == b';' {
                self.next1();
            }
        } else if self.cur() == b'&' && self.nxt(1) == b'#' {
            self.skip(2);
            while self.cur() != b';' {
                let over = count > 20;
                count += 1;
                if over {
                    count = 0;
                    if self.instate == State::Eof {
                        return 0;
                    }
                }
                let c = self.cur();
                if c.is_ascii_digit() {
                    val = val * 10 + u32::from(c - b'0');
                } else {
                    self.fatal_err(Code::InvalidDecCharref, None);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                self.next();
                count += 1;
            }
            if self.cur() == b';' {
                self.next1();
            }
        } else {
            self.fatal_err(Code::InvalidCharref, None);
        }
        if val >= 0x110000 {
            self.fatal_msg(
                Code::InvalidChar,
                b"xmlParseCharRef: character reference out of bounds\n".to_vec(),
            );
        } else if is_char(val) {
            return val;
        } else {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlParseCharRef: invalid xmlChar value {val}\n").into_bytes(),
            );
        }
        0
    }

    /// `xmlParseStringCharRef`. It returns the value and the position after the reference.
    pub(super) fn parse_string_char_ref(&mut self, s: &[u8], i: usize) -> (u32, usize) {
        let mut val: u32 = 0;
        let mut p = i;
        if at(s, p) == b'&' && at(s, p + 1) == b'#' && at(s, p + 2) == b'x' {
            p += 3;
            let mut c = at(s, p);
            while c != b';' {
                if c.is_ascii_digit() {
                    val = val * 16 + u32::from(c - b'0');
                } else if (b'a'..=b'f').contains(&c) {
                    val = val * 16 + u32::from(c - b'a') + 10;
                } else if (b'A'..=b'F').contains(&c) {
                    val = val * 16 + u32::from(c - b'A') + 10;
                } else {
                    self.fatal_err(Code::InvalidHexCharref, None);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                p += 1;
                c = at(s, p);
            }
            if c == b';' {
                p += 1;
            }
        } else if at(s, p) == b'&' && at(s, p + 1) == b'#' {
            p += 2;
            let mut c = at(s, p);
            while c != b';' {
                if c.is_ascii_digit() {
                    val = val * 10 + u32::from(c - b'0');
                } else {
                    self.fatal_err(Code::InvalidDecCharref, None);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                p += 1;
                c = at(s, p);
            }
            if c == b';' {
                p += 1;
            }
        } else {
            self.fatal_err(Code::InvalidCharref, None);
            return (0, i);
        }
        if val >= 0x110000 {
            self.fatal_msg(
                Code::InvalidChar,
                b"xmlParseStringCharRef: character reference out of bounds\n".to_vec(),
            );
        } else if is_char(val) {
            return (val, p);
        } else {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlParseStringCharRef: invalid xmlChar value {val}\n").into_bytes(),
            );
        }
        (0, p)
    }

    // Entity checks and the entity decoder.

    /// `xmlParserEntityCheck`. It returns true if the entity expansion is too large.
    pub(super) fn entity_check(
        &mut self,
        size: usize,
        ent: Option<&Rc<RefCell<Entity>>>,
        replacement: u64,
    ) -> bool {
        if self.last_error == Code::EntityLoop {
            return true;
        }
        if let Some(ent) = ent {
            let (etype, content, checked) = {
                let e = ent.borrow();
                (e.etype, e.content.clone(), e.checked)
            };
            if let Some(content) = content
                && etype != EntityType::InternalPredefined
                && checked == 0
                && self.err_no != Code::EntityLoop
            {
                let old = self.nbentities;
                ent.borrow_mut().checked = 1;
                self.depth += 1;
                let rep = self.string_decode_entities(cstr(&content), SUBSTITUTE_REF);
                self.depth -= 1;
                if rep.is_none() || self.err_no == Code::EntityLoop {
                    zero_content(ent);
                }
                let diff = (self.nbentities - old + 1).min(i32::MAX as u64 / 2);
                let mut checked = (diff * 2) as i32;
                if rep.is_some_and(|r| r.contains(&b'<')) {
                    checked |= 1;
                }
                ent.borrow_mut().checked = checked;
            }
        }
        if self.instate == State::Dtd
            && self.nbentities > 10000
            && self.nbentities.is_multiple_of(1024)
        {
            let consumed: u64 = self.inputs.iter().map(|i| i.cur as u64).sum();
            if self.nbentities > consumed * 10 {
                self.fatal_err(Code::EntityLoop, None);
                self.instate = State::Eof;
                return true;
            }
        }
        let consumed = self.input().cur as u64 + self.sizeentities;
        if replacement != 0 {
            if replacement < MAX_TEXT_LENGTH as u64 || replacement < 10 * consumed {
                return false;
            }
        } else if size != 0 {
            let size = size as u64;
            if size < 1000 || (size < 10 * consumed && self.nbentities * 3 < 10 * consumed) {
                return false;
            }
        } else if let Some(ent) = ent {
            let size = (ent.borrow().checked / 2) as u64;
            if size * 3 < consumed * 10 {
                return false;
            }
        } else if (self.last_error != Code::UndeclaredEntity
            && self.last_error != Code::WarUndeclaredEntity)
            || self.nbentities <= 10000
        {
            return false;
        }
        self.fatal_err(Code::EntityLoop, None);
        true
    }

    /// `xmlStringDecodeEntities` on a C string, with no end characters.
    pub(super) fn string_decode_entities(&mut self, s: &[u8], what: u32) -> Option<Vec<u8>> {
        if self.depth > 40 {
            self.fatal_err(Code::EntityLoop, None);
            return None;
        }
        let s = cstr(s);
        let last = s.len();
        let mut size = BIG_BUFFER_SIZE;
        let mut buf: Vec<u8> = Vec::new();
        let mut i = 0;
        let mut l = 0;
        let mut c = if i < last { self.string_current_char(s, i, &mut l) } else { 0 };
        while c != 0 && self.instate != State::Eof {
            if c == u32::from(b'&') && at(s, i + 1) == b'#' {
                let (val, next) = self.parse_string_char_ref(s, i);
                i = next;
                if val == 0 {
                    return None;
                }
                copy_buf(&mut buf, val);
                if buf.len() + BUFFER_SIZE > size {
                    size = size * 2 + BUFFER_SIZE;
                }
            } else if c == u32::from(b'&') && what & SUBSTITUTE_REF != 0 {
                let (ent, next) = self.parse_string_entity_ref(s, i);
                i = next;
                self.entity_check(0, ent.as_ref(), 0);
                if let Some(ent) = &ent {
                    self.nbentities += (ent.borrow().checked / 2) as u64;
                }
                match ent {
                    Some(ent) if ent.borrow().etype == EntityType::InternalPredefined => {
                        let first = ent.borrow().content.as_ref().map(|c| at(c, 0));
                        match first {
                            Some(b) => {
                                copy_buf(&mut buf, u32::from(b));
                                if buf.len() + BUFFER_SIZE > size {
                                    size = size * 2 + BUFFER_SIZE;
                                }
                            }
                            None => {
                                self.fatal_msg(
                                    Code::InternalError,
                                    b"predefined entity has no content\n".to_vec(),
                                );
                                return None;
                            }
                        }
                    }
                    Some(ent) if ent.borrow().content.is_some() => {
                        let content = ent.borrow().content.clone().unwrap_or_default();
                        self.depth += 1;
                        let rep = self.string_decode_entities(&content, what);
                        self.depth -= 1;
                        let Some(rep) = rep else {
                            zero_content(&ent);
                            return None;
                        };
                        for &b in &rep {
                            buf.push(b);
                            if buf.len() + BUFFER_SIZE > size {
                                if self.entity_check(buf.len(), Some(&ent), 0) {
                                    return None;
                                }
                                size = size * 2 + BUFFER_SIZE;
                            }
                        }
                    }
                    Some(ent) => {
                        let name = ent.borrow().name.clone();
                        buf.push(b'&');
                        if buf.len() + name.len() + BUFFER_SIZE > size {
                            size = size * 2 + name.len() + BUFFER_SIZE;
                        }
                        buf.extend_from_slice(&name);
                        buf.push(b';');
                    }
                    None => {}
                }
            } else if c == u32::from(b'%') && what & SUBSTITUTE_PEREF != 0 {
                let (ent, next) = self.parse_string_pe_reference(s, i);
                i = next;
                self.entity_check(0, ent.as_ref(), 0);
                if let Some(ent) = &ent {
                    self.nbentities += (ent.borrow().checked / 2) as u64;
                }
                if let Some(ent) = ent {
                    if ent.borrow().content.is_none() {
                        if self.options & (PARSE_NOENT | PARSE_DTDVALID) != 0 || self.validate {
                            self.load_entity_content(&ent);
                        } else {
                            let name = ent.borrow().name.clone();
                            self.warning_msg(
                                Code::EntityProcessing,
                                [
                                    &b"not validating will not read content for PE entity "[..],
                                    &name,
                                    b"\n",
                                ]
                                .concat(),
                            );
                        }
                    }
                    let content = ent.borrow().content.clone();
                    self.depth += 1;
                    let rep = match &content {
                        Some(content) => self.string_decode_entities(content, what),
                        None => None,
                    };
                    self.depth -= 1;
                    let Some(rep) = rep else {
                        zero_content(&ent);
                        return None;
                    };
                    for &b in &rep {
                        buf.push(b);
                        if buf.len() + BUFFER_SIZE > size {
                            if self.entity_check(buf.len(), Some(&ent), 0) {
                                return None;
                            }
                            size = size * 2 + BUFFER_SIZE;
                        }
                    }
                }
            } else {
                copy_buf(&mut buf, c);
                i += l;
                if buf.len() + BUFFER_SIZE > size {
                    size = size * 2 + BUFFER_SIZE;
                }
            }
            c = if i < last { self.string_current_char(s, i, &mut l) } else { 0 };
        }
        Some(buf)
    }

    // Character data.

    /// The `characters` SAX handler, that is `xmlSAX2Characters`.
    fn sax_characters(&mut self, len: usize) {
        self.sax2_text(len, false);
    }

    /// `xmlParseCharData` with `cdata` 0.
    pub(super) fn parse_char_data(&mut self) {
        let mut line = self.input().line;
        self.shrink();
        let mut i = self.pos();
        loop {
            loop {
                while self.input().at(i) == 0x20 {
                    i += 1;
                }
                if self.input().at(i) == 0xA {
                    while self.input().at(i) == 0xA {
                        self.input_mut().line += 1;
                        i += 1;
                    }
                    continue;
                }
                break;
            }
            if self.input().at(i) == b'<' {
                let nbchar = i - self.pos();
                if nbchar > 0 {
                    self.input_mut().cur = i;
                    self.sax_characters(nbchar);
                }
                return;
            }
            loop {
                while test_char_data(self.input().at(i)) {
                    i += 1;
                }
                if self.input().at(i) == 0xA {
                    while self.input().at(i) == 0xA {
                        self.input_mut().line += 1;
                        i += 1;
                    }
                    continue;
                }
                if self.input().at(i) == b']' {
                    if self.input().at(i + 1) == b']' && self.input().at(i + 2) == b'>' {
                        self.fatal_err(Code::MisplacedCdataEnd, None);
                        self.input_mut().cur = i + 1;
                        return;
                    }
                    i += 1;
                    continue;
                }
                break;
            }
            let nbchar = i - self.pos();
            if nbchar > 0 {
                self.sax_characters(nbchar);
                line = self.input().line;
                if self.instate != State::Content {
                    return;
                }
            }
            self.input_mut().cur = i;
            if self.input().at(i) == 0xD && self.input().at(i + 1) == 0xA {
                self.input_mut().cur = i + 1;
                i += 2;
                self.input_mut().line += 1;
            } else {
                let c = self.input().at(i);
                if c == b'<' || c == b'&' {
                    return;
                }
                self.shrink();
                if self.instate == State::Eof {
                    return;
                }
                i = self.pos();
            }
            let c = self.input().at(i);
            if !((0x20..=0x7F).contains(&c) || c == 0x09 || c == 0x0A) {
                break;
            }
        }
        self.input_mut().line = line;
        self.parse_char_data_complex();
    }

    /// `xmlParseCharDataComplex` with `cdata` 0.
    fn parse_char_data_complex(&mut self) {
        let mut nbchar = 0;
        let mut count = 0;
        let mut l = 0;
        self.shrink();
        let mut cur = self.cur_char(&mut l);
        while cur != u32::from(b'<') && cur != u32::from(b'&') && is_char(cur) {
            if cur == u32::from(b']') && self.nxt(1) == b']' && self.nxt(2) == b'>' {
                self.fatal_err(Code::MisplacedCdataEnd, None);
            }
            nbchar += utf8_len(cur);
            self.nextl(l);
            cur = self.cur_char(&mut l);
            if nbchar >= BIG_BUFFER_SIZE {
                if !self.disable_sax {
                    self.sax_characters(nbchar);
                }
                nbchar = 0;
                if self.instate != State::Content {
                    return;
                }
            }
            count += 1;
            if count > 50 {
                self.shrink();
                count = 0;
                if self.instate == State::Eof {
                    return;
                }
            }
        }
        if nbchar != 0 && !self.disable_sax {
            self.sax_characters(nbchar);
        }
        if cur != 0 && !is_char(cur) {
            self.fatal_msg(
                Code::InvalidChar,
                format!("PCDATA invalid Char value {cur}\n").into_bytes(),
            );
            self.nextl(l);
        }
    }

    // Comments and processing instructions.

    /// The `comment` and `processingInstruction` SAX handlers. In a subset the node goes to the DTD. Else it goes to the current node.
    fn sax_other(&mut self) {
        if self.in_subset != 1 && self.in_subset != 2 {
            self.add_other();
        }
    }

    /// `xmlParseComment`.
    pub(super) fn parse_comment(&mut self) {
        if !self.looking_at(b"<!--") {
            return;
        }
        let state = self.instate;
        self.instate = State::Comment;
        let inputid = self.input().id;
        self.skip(4);
        self.shrink();
        let mut buf: Option<Vec<u8>> = None;
        let mut i = self.pos();
        loop {
            if self.input().at(i) == 0xA {
                while self.input().at(i) == 0xA {
                    self.input_mut().line += 1;
                    i += 1;
                }
            }
            'get_more: loop {
                loop {
                    let c = self.input().at(i);
                    if (c > b'-' && c <= 0x7F) || (0x20..b'-').contains(&c) || c == 0x09 {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if self.input().at(i) == 0xA {
                    while self.input().at(i) == 0xA {
                        self.input_mut().line += 1;
                        i += 1;
                    }
                    continue 'get_more;
                }
                let cur = self.pos();
                if i > cur {
                    let chunk = self.slice(cur, i);
                    buf.get_or_insert_with(Vec::new).extend_from_slice(&chunk);
                }
                if buf.as_ref().is_some_and(|b| b.len() > MAX_TEXT_LENGTH) {
                    self.fatal_msg(Code::CommentNotFinished, b"Comment too big found".to_vec());
                    return;
                }
                self.input_mut().cur = i;
                if self.input().at(i) == 0xA {
                    i += 1;
                    self.input_mut().line += 1;
                }
                if self.input().at(i) == 0xD && self.input().at(i + 1) == 0xA {
                    self.input_mut().cur = i + 1;
                    i += 2;
                    self.input_mut().line += 1;
                    continue 'get_more;
                }
                self.shrink();
                if self.instate == State::Eof {
                    return;
                }
                i = self.pos();
                if self.input().at(i) == b'-' {
                    if self.input().at(i + 1) == b'-' {
                        if self.input().at(i + 2) == b'>' {
                            if self.input().id != inputid {
                                self.fatal_msg(
                                    Code::EntityBoundary,
                                    b"comment doesn't start and stop in the same entity\n".to_vec(),
                                );
                            }
                            self.skip(3);
                            if !self.disable_sax {
                                self.sax_other();
                            }
                            if self.instate != State::Eof {
                                self.instate = state;
                            }
                            return;
                        }
                        match &buf {
                            Some(b) => {
                                let msg = [
                                    &b"Double hyphen within comment: <!--"[..],
                                    &b[..b.len().min(50)],
                                    b"\n",
                                ]
                                .concat();
                                self.fatal_msg(Code::HyphenInComment, msg);
                            }
                            None => {
                                self.fatal_msg(
                                    Code::HyphenInComment,
                                    b"Double hyphen within comment\n".to_vec(),
                                );
                            }
                        }
                        if self.instate == State::Eof {
                            return;
                        }
                        i += 1;
                    }
                    i += 1;
                    continue 'get_more;
                }
                break;
            }
            let c = self.input().at(i);
            if !((0x20..=0x7F).contains(&c) || c == 0x09 || c == 0x0A) {
                break;
            }
        }
        self.parse_comment_complex(buf.unwrap_or_default());
        self.instate = state;
    }

    /// `xmlParseCommentComplex`.
    fn parse_comment_complex(&mut self, mut buf: Vec<u8>) {
        let inputid = self.input().id;
        let mut ql = 0;
        let mut rl = 0;
        let mut l = 0;
        let mut count = 0;
        let mut q = self.cur_char(&mut ql);
        if q == 0 {
            self.comment_not_terminated();
            return;
        }
        if !is_char(q) {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlParseComment: invalid xmlChar value {q}\n").into_bytes(),
            );
            return;
        }
        self.nextl(ql);
        let mut r = self.cur_char(&mut rl);
        if r == 0 {
            self.comment_not_terminated();
            return;
        }
        if !is_char(r) {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlParseComment: invalid xmlChar value {q}\n").into_bytes(),
            );
            return;
        }
        self.nextl(rl);
        let mut cur = self.cur_char(&mut l);
        if cur == 0 {
            self.comment_not_terminated();
            return;
        }
        let dash = u32::from(b'-');
        while is_char(cur) && (cur != u32::from(b'>') || r != dash || q != dash) {
            if r == dash && q == dash {
                self.fatal_err(Code::HyphenInComment, None);
            }
            if buf.len() > MAX_TEXT_LENGTH {
                self.fatal_msg(Code::CommentNotFinished, b"Comment too big found".to_vec());
                return;
            }
            copy_buf(&mut buf, q);
            q = r;
            r = cur;
            rl = l;
            count += 1;
            if count > 50 {
                self.shrink();
                count = 0;
                if self.instate == State::Eof {
                    return;
                }
            }
            let _ = rl;
            self.nextl(l);
            cur = self.cur_char(&mut l);
        }
        if cur == 0 {
            let msg =
                [&b"Comment not terminated \n<!--"[..], &buf[..buf.len().min(50)], b"\n"].concat();
            self.fatal_msg(Code::CommentNotFinished, msg);
        } else if !is_char(cur) {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlParseComment: invalid xmlChar value {cur}\n").into_bytes(),
            );
        } else {
            if inputid != self.input().id {
                self.fatal_msg(
                    Code::EntityBoundary,
                    b"Comment doesn't start and stop in the same entity\n".to_vec(),
                );
            }
            self.next();
            if !self.disable_sax {
                self.sax_other();
            }
        }
    }

    fn comment_not_terminated(&mut self) {
        self.fatal_msg(Code::CommentNotFinished, b"Comment not terminated\n".to_vec());
    }

    /// `xmlParsePITarget`.
    fn parse_pi_target(&mut self) -> Option<Vec<u8>> {
        let name = self.parse_name()?;
        if name.len() >= 3 && name[..3].eq_ignore_ascii_case(b"xml") {
            if &name[..] == b"xml" {
                self.fatal_msg(
                    Code::ReservedXmlName,
                    b"XML declaration allowed only at the start of the document\n".to_vec(),
                );
                return Some(name);
            } else if name.len() == 3 {
                self.fatal_err(Code::ReservedXmlName, None);
                return Some(name);
            }
            if W3C_PIS.contains(&&name[..]) {
                return Some(name);
            }
            self.warning_msg(
                Code::ReservedXmlName,
                b"xmlParsePITarget: invalid name prefix 'xml'\n".to_vec(),
            );
        }
        if name.contains(&b':') {
            self.ns_err();
        }
        Some(name)
    }

    /// `xmlParseCatalogPI`.
    fn parse_catalog_pi(&mut self, catalog: &[u8]) {
        let catalog = cstr(catalog);
        let mut i = 0;
        let ok = 'check: {
            while is_blank(u32::from(at(catalog, i))) {
                i += 1;
            }
            if !catalog[i..].starts_with(b"catalog") {
                break 'check false;
            }
            i += 7;
            while is_blank(u32::from(at(catalog, i))) {
                i += 1;
            }
            if at(catalog, i) != b'=' {
                return;
            }
            i += 1;
            while is_blank(u32::from(at(catalog, i))) {
                i += 1;
            }
            let marker = at(catalog, i);
            if marker != b'\'' && marker != b'"' {
                break 'check false;
            }
            i += 1;
            while at(catalog, i) != 0 && at(catalog, i) != marker {
                i += 1;
            }
            if at(catalog, i) == 0 {
                break 'check false;
            }
            i += 1;
            while is_blank(u32::from(at(catalog, i))) {
                i += 1;
            }
            at(catalog, i) == 0
        };
        if !ok {
            self.warning_msg(
                Code::WarCatalogPi,
                [&b"Catalog PI syntax error: "[..], catalog, b"\n"].concat(),
            );
        }
    }

    /// `xmlParsePI`.
    pub(super) fn parse_pi(&mut self) {
        if !(self.cur() == b'<' && self.nxt(1) == b'?') {
            return;
        }
        let inputid = self.input().id;
        let state = self.instate;
        self.instate = State::Pi;
        self.skip(2);
        self.shrink();
        match self.parse_pi_target() {
            Some(target) => {
                if self.cur() == b'?' && self.nxt(1) == b'>' {
                    if inputid != self.input().id {
                        self.fatal_msg(
                            Code::EntityBoundary,
                            b"PI declaration doesn't start and stop in the same entity\n".to_vec(),
                        );
                    }
                    self.skip(2);
                    if !self.disable_sax {
                        self.sax_other();
                    }
                    if self.instate != State::Eof {
                        self.instate = state;
                    }
                    return;
                }
                if self.skip_blanks() == 0 {
                    self.fatal_msg(
                        Code::SpaceRequired,
                        [&b"ParsePI: PI "[..], &target, b" space expected\n"].concat(),
                    );
                }
                let mut buf = Vec::new();
                let mut count = 0;
                let mut l = 0;
                let mut cur = self.cur_char(&mut l);
                while is_char(cur) && (cur != u32::from(b'?') || self.nxt(1) != b'>') {
                    count += 1;
                    if count > 50 {
                        self.shrink();
                        if self.instate == State::Eof {
                            return;
                        }
                        count = 0;
                        if buf.len() > MAX_TEXT_LENGTH {
                            self.fatal_msg(
                                Code::PiNotFinished,
                                [&b"PI "[..], &target, b" too big found"].concat(),
                            );
                            self.instate = state;
                            return;
                        }
                    }
                    copy_buf(&mut buf, cur);
                    self.nextl(l);
                    cur = self.cur_char(&mut l);
                }
                if buf.len() > MAX_TEXT_LENGTH {
                    self.fatal_msg(
                        Code::PiNotFinished,
                        [&b"PI "[..], &target, b" too big found"].concat(),
                    );
                    self.instate = state;
                    return;
                }
                if cur != u32::from(b'?') {
                    self.fatal_msg(
                        Code::PiNotFinished,
                        [&b"ParsePI: PI "[..], &target, b" never end ...\n"].concat(),
                    );
                } else {
                    if inputid != self.input().id {
                        self.fatal_msg(
                            Code::EntityBoundary,
                            b"PI declaration doesn't start and stop in the same entity\n".to_vec(),
                        );
                    }
                    self.skip(2);
                    if (state == State::Misc || state == State::Start)
                        && &target[..] == b"oasis-xml-catalog"
                    {
                        self.parse_catalog_pi(&buf);
                    }
                    if !self.disable_sax {
                        self.sax_other();
                    }
                }
            }
            None => self.fatal_err(Code::PiNotStarted, None),
        }
        if self.instate != State::Eof {
            self.instate = state;
        }
    }

    // Entity references.

    /// The `reference` SAX handler, that is `xmlSAX2Reference`.
    fn sax_reference(&mut self) {
        self.add_other();
    }

    /// `xmlParseReference`.
    pub(super) fn parse_reference(&mut self) {
        if self.cur() != b'&' {
            return;
        }
        if self.nxt(1) == b'#' {
            let value = self.parse_char_ref();
            if value == 0 {
                return;
            }
            if !self.disable_sax {
                self.sax_characters(utf8_len(value));
            }
            return;
        }
        let Some(ent) = self.parse_entity_ref() else {
            return;
        };
        if !self.well_formed {
            return;
        }
        let mut was_checked = ent.borrow().checked;
        let etype = ent.borrow().etype;
        if etype == EntityType::InternalPredefined {
            let len = ent.borrow().content.as_ref().map(|c| cstr(c).len());
            if let Some(len) = len
                && !self.disable_sax
            {
                self.sax_characters(len);
            }
            return;
        }
        let mut list: Option<Node> = None;
        let (checked, has_children) = {
            let e = ent.borrow();
            (e.checked, e.children.is_some())
        };
        if (checked == 0 || (!has_children && self.options & PARSE_NOENT != 0))
            && (etype != EntityType::ExternalGeneralParsed
                || self.options & (PARSE_NOENT | PARSE_DTDVALID) != 0)
        {
            let old = self.nbentities;
            let ret;
            if etype == EntityType::InternalGeneral {
                let content = ent.borrow().content.clone();
                self.depth += 1;
                let (r, l) = self.parse_balanced_chunk_memory_internal(content.as_deref());
                self.depth -= 1;
                ret = r;
                list = l;
            } else if etype == EntityType::ExternalGeneralParsed {
                let (uri, id) = {
                    let e = ent.borrow();
                    (e.uri.clone(), e.external_id.clone())
                };
                self.depth += 1;
                let (r, l) = self.parse_external_entity_private(uri.as_deref(), id.as_deref());
                self.depth -= 1;
                ret = r;
                list = l;
            } else {
                ret = Code::EntityPeInternal;
                self.err_msg_str(Code::InternalError, b"invalid entity type found\n".to_vec());
            }
            let diff = (self.nbentities - old + 1).min(i32::MAX as u64 / 2);
            {
                let mut e = ent.borrow_mut();
                e.checked = (diff * 2) as i32;
                if e.content.as_ref().is_some_and(|c| cstr(c).contains(&b'<')) {
                    e.checked |= 1;
                }
            }
            if ret == Code::EntityLoop {
                self.fatal_err(Code::EntityLoop, None);
                self.halt();
                return;
            }
            if self.entity_check(0, Some(&ent), 0) {
                return;
            }
            if ret == Code::Ok && list.is_some() {
                let mut e = ent.borrow_mut();
                if (e.etype == EntityType::InternalGeneral
                    || e.etype == EntityType::ExternalGeneralParsed)
                    && e.children.is_none()
                {
                    let l = list.expect("a list");
                    e.children = Some(l);
                    if !self.replace_entities || (l.count == 1 && matches!(l.first, Kind::Text(_)))
                    {
                        e.owner = 1;
                        list = None;
                    } else {
                        e.owner = 0;
                    }
                } else {
                    list = None;
                }
            } else if ret != Code::Ok && ret != Code::WarUndeclaredEntity {
                let name = ent.borrow().name.clone();
                self.fatal_msg(
                    Code::UndeclaredEntity,
                    [&b"Entity '"[..], &name, b"' failed to parse\n"].concat(),
                );
                zero_content(&ent);
                self.entity_check(0, Some(&ent), 0);
            } else {
                list = None;
            }
            if ent.borrow().checked == 0 {
                ent.borrow_mut().checked = 2;
            }
            was_checked = 0;
        } else if checked != 1 {
            self.nbentities += (checked / 2) as u64;
        }
        let _ = list;
        if ent.borrow().children.is_none() {
            if was_checked != 0 {
                let ret;
                if etype == EntityType::InternalGeneral {
                    let content = ent.borrow().content.clone();
                    self.depth += 1;
                    ret = self.parse_balanced_chunk_memory_internal(content.as_deref()).0;
                    self.depth -= 1;
                } else if etype == EntityType::ExternalGeneralParsed {
                    let (uri, id) = {
                        let e = ent.borrow();
                        (e.uri.clone(), e.external_id.clone())
                    };
                    self.depth += 1;
                    ret = self.parse_external_entity_private(uri.as_deref(), id.as_deref()).0;
                    self.depth -= 1;
                } else {
                    ret = Code::EntityPeInternal;
                    self.err_msg_str(Code::InternalError, b"invalid entity type found\n".to_vec());
                }
                if ret == Code::EntityLoop {
                    self.fatal_err(Code::EntityLoop, None);
                    return;
                }
            }
            if !self.replace_entities && !self.disable_sax {
                self.sax_reference();
            }
            return;
        }
        if !self.replace_entities {
            if !self.disable_sax {
                self.sax_reference();
            }
            return;
        }
        if !self.nodes.is_empty() {
            let length = ent.borrow().length as u64;
            self.sizeentcopy += length + 5;
            if self.entity_check(0, Some(&ent), self.sizeentcopy) {
                return;
            }
            let children = ent.borrow().children.expect("entity children");
            if let Some(node) = self.nodes.last_mut() {
                append_children(node, &children);
            }
            if ent.borrow().owner == 0 {
                ent.borrow_mut().owner = 1;
            }
            self.nodemem = 0;
            self.nodelen = 0;
        }
    }

    /// `xmlParseEntityRef`.
    pub(super) fn parse_entity_ref(&mut self) -> Option<Rc<RefCell<Entity>>> {
        if self.instate == State::Eof {
            return None;
        }
        if self.cur() != b'&' {
            return None;
        }
        self.next();
        let Some(name) = self.parse_name() else {
            self.fatal_msg(Code::NameRequired, b"xmlParseEntityRef: no name\n".to_vec());
            return None;
        };
        if self.cur() != b';' {
            self.fatal_err(Code::EntityRefSemicolMissing, None);
            return None;
        }
        self.next();
        if let Some(ent) = super::dtd::predefined_entity(&name) {
            return Some(ent);
        }
        self.nbentities += 1;
        let mut ent = self.sax_get_entity(&name);
        if self.well_formed && ent.is_none() {
            ent = self.sax_get_entity(&name);
        }
        if self.instate == State::Eof {
            return None;
        }
        self.check_entity_ref(&name, ent.as_ref(), true);
        ent
    }

    /// The checks at the end of `xmlParseEntityRef` and `xmlParseStringEntityRef`.
    fn check_entity_ref(
        &mut self,
        name: &[u8],
        ent: Option<&Rc<RefCell<Entity>>>,
        in_content: bool,
    ) {
        match ent {
            None => {
                let msg = [&b"Entity '"[..], name, b"' not defined\n"].concat();
                if self.standalone == 1 || (!self.has_external_subset && !self.has_perefs) {
                    self.fatal_msg(Code::UndeclaredEntity, msg);
                } else {
                    self.err_msg_str(Code::WarUndeclaredEntity, msg);
                    if in_content && self.in_subset == 0 {
                        self.sax_reference();
                    }
                }
                self.entity_check(0, None, 0);
            }
            Some(ent) => {
                let (etype, checked, has_lt) = {
                    let e = ent.borrow();
                    (
                        e.etype,
                        e.checked,
                        e.content.as_ref().is_some_and(|c| cstr(c).contains(&b'<')),
                    )
                };
                if etype == EntityType::ExternalGeneralUnparsed {
                    self.fatal_msg(
                        Code::UnparsedEntity,
                        [&b"Entity reference to unparsed entity "[..], name, b"\n"].concat(),
                    );
                } else if self.instate == State::AttributeValue
                    && etype == EntityType::ExternalGeneralParsed
                {
                    self.fatal_msg(
                        Code::EntityIsExternal,
                        [&b"Attribute references external entity '"[..], name, b"'\n"].concat(),
                    );
                } else if self.instate == State::AttributeValue
                    && etype != EntityType::InternalPredefined
                {
                    let checked_ok = !in_content || checked & 1 != 0 || checked == 0;
                    if checked_ok && has_lt {
                        self.fatal_msg(
                            Code::LtInAttribute,
                            [
                                &b"'<' in entity '"[..],
                                name,
                                b"' is not allowed in attributes values\n",
                            ]
                            .concat(),
                        );
                    }
                } else if matches!(
                    etype,
                    EntityType::InternalParameter | EntityType::ExternalParameter
                ) {
                    self.fatal_msg(
                        Code::EntityIsParameter,
                        [&b"Attempt to reference the parameter entity '"[..], name, b"'\n"]
                            .concat(),
                    );
                }
            }
        }
    }

    /// `xmlParseStringEntityRef`. It returns the entity and the position after the reference.
    pub(super) fn parse_string_entity_ref(
        &mut self,
        s: &[u8],
        i: usize,
    ) -> (Option<Rc<RefCell<Entity>>>, usize) {
        if at(s, i) != b'&' {
            return (None, i);
        }
        let p = i + 1;
        let Some((name, p)) = self.parse_string_name(s, p) else {
            self.fatal_msg(Code::NameRequired, b"xmlParseStringEntityRef: no name\n".to_vec());
            return (None, p);
        };
        if at(s, p) != b';' {
            self.fatal_err(Code::EntityRefSemicolMissing, None);
            return (None, p);
        }
        let p = p + 1;
        if let Some(ent) = super::dtd::predefined_entity(&name) {
            return (Some(ent), p);
        }
        self.nbentities += 1;
        let mut ent = self.sax_get_entity(&name);
        if ent.is_none() {
            ent = self.sax_get_entity(&name);
        }
        if self.instate == State::Eof {
            return (None, i);
        }
        self.check_entity_ref(&name, ent.as_ref(), false);
        (ent, p)
    }
}

/// `XML_XML_NAMESPACE`.
pub(super) const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";
/// The namespace name of `xmlns`.
const XMLNS_NS: &[u8] = b"http://www.w3.org/2000/xmlns/";

/// `xmlAttrNormalizeSpace` on a copy.
fn normalize_space(src: &[u8]) -> Vec<u8> {
    let src = cstr(src);
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while at(src, i) == 0x20 {
        i += 1;
    }
    while i < src.len() {
        if src[i] == 0x20 {
            while at(src, i) == 0x20 {
                i += 1;
            }
            if i < src.len() {
                out.push(0x20);
            }
        } else {
            out.push(src[i]);
            i += 1;
        }
    }
    out
}

/// An attribute of a start tag: the local name, the prefix and the namespace name.
struct Att {
    name: Vec<u8>,
    prefix: Option<Vec<u8>>,
    nsname: Option<Vec<u8>>,
}

impl Ctxt {
    // Names with a prefix.

    /// `xmlParseQName`. It returns the local name and the prefix.
    pub(super) fn parse_qname(&mut self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let Some(l) = self.parse_ncname() else {
            if self.cur() == b':'
                && let Some(l) = self.parse_name()
            {
                self.ns_err();
                return (Some(l), None);
            }
            return (None, None);
        };
        if self.cur() != b':' {
            return (Some(l), None);
        }
        self.next();
        let p = l;
        let Some(l) = self.parse_ncname() else {
            if self.instate == State::Eof {
                return (None, None);
            }
            self.ns_err();
            let tmp = match self.parse_nmtoken() {
                Some(l) => [&p[..], b":", &l].concat(),
                None => {
                    if self.instate == State::Eof {
                        return (None, None);
                    }
                    [&p[..], b":"].concat()
                }
            };
            return (Some(tmp), None);
        };
        if self.cur() == b':' {
            self.ns_err();
            self.next();
            if let Some(tmp) = self.parse_name() {
                return (Some([&l[..], b":", &tmp].concat()), Some(p));
            }
            if self.instate == State::Eof {
                return (None, None);
            }
            return (Some([&l[..], b":"].concat()), Some(p));
        }
        (Some(l), Some(p))
    }

    /// `xmlParseQNameAndCompare`.
    fn parse_qname_and_compare(
        &mut self,
        name: &[u8],
        prefix: Option<&[u8]>,
    ) -> Result<(), Option<Vec<u8>>> {
        let Some(prefix) = prefix else {
            return self.parse_name_and_compare(name);
        };
        let input = self.input();
        let mut i = input.cur;
        let mut k = 0;
        while input.at(i) != 0 && input.at(i) == at(prefix, k) {
            i += 1;
            k += 1;
        }
        if k >= prefix.len() && input.at(i) == b':' {
            i += 1;
            let mut k = 0;
            while input.at(i) != 0 && input.at(i) == at(name, k) {
                i += 1;
                k += 1;
            }
            if k >= name.len() && (input.at(i) == b'>' || is_blank(u32::from(input.at(i)))) {
                self.input_mut().cur = i;
                return Ok(());
            }
        }
        let (ret, prefix2) = self.parse_qname();
        if ret.as_deref() == Some(name) && prefix2.as_deref() == Some(prefix) {
            return Ok(());
        }
        Err(ret)
    }

    // Attribute values.

    /// `xmlParseAttValueInternal`. It returns the value.
    pub(super) fn parse_att_value_internal(&mut self, normalize: bool) -> Option<Vec<u8>> {
        let mut in_ = self.pos();
        let mut line = self.input().line;
        let quote = self.input().at(in_);
        if quote != b'"' && quote != b'\'' {
            self.fatal_err(Code::AttributeNotStarted, None);
            return None;
        }
        self.instate = State::AttributeValue;
        let limit = quote;
        in_ += 1;
        let end = self.input().end;
        let mut start = in_;
        let last;
        if normalize {
            loop {
                let c = self.input().at(in_);
                if in_ < end && c != limit && (c == 0x20 || c == 0x9 || c == 0xA || c == 0xD) {
                    if c == 0xA {
                        line += 1;
                    }
                    in_ += 1;
                    start = in_;
                } else {
                    break;
                }
            }
            loop {
                let c = self.input().at(in_);
                if in_ < end && c != limit && (0x20..=0x7F).contains(&c) && c != b'&' && c != b'<' {
                    in_ += 1;
                    if c == 0x20 && self.input().at(in_) == 0x20 {
                        break;
                    }
                } else {
                    break;
                }
            }
            let mut l = in_;
            while l > start && self.input().at(l - 1) == 0x20 {
                l -= 1;
            }
            last = l;
            loop {
                let c = self.input().at(in_);
                if in_ < end && c != limit && (c == 0x20 || c == 0x9 || c == 0xA || c == 0xD) {
                    if c == 0xA {
                        line += 1;
                    }
                    in_ += 1;
                } else {
                    break;
                }
            }
            if in_ - start > MAX_TEXT_LENGTH {
                self.fatal_msg(Code::AttributeNotFinished, b"AttValue length too long\n".to_vec());
                return None;
            }
            if self.input().at(in_) != limit {
                return self.parse_att_value_complex(normalize);
            }
        } else {
            loop {
                let c = self.input().at(in_);
                if in_ < end && c != limit && (0x20..=0x7F).contains(&c) && c != b'&' && c != b'<' {
                    in_ += 1;
                } else {
                    break;
                }
            }
            last = in_;
            if in_ - start > MAX_TEXT_LENGTH {
                self.fatal_msg(Code::AttributeNotFinished, b"AttValue length too long\n".to_vec());
                return None;
            }
            if self.input().at(in_) != limit {
                return self.parse_att_value_complex(normalize);
            }
        }
        in_ += 1;
        let ret = self.slice(start, last);
        let input = self.input_mut();
        input.cur = in_;
        input.line = line;
        Some(ret)
    }

    /// `xmlParseAttValueComplex`.
    fn parse_att_value_complex(&mut self, normalize: bool) -> Option<Vec<u8>> {
        let limit = self.cur();
        if limit == b'"' || limit == b'\'' {
            self.instate = State::AttributeValue;
            self.next();
        } else {
            self.fatal_err(Code::AttributeNotStarted, None);
            return None;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut in_space = false;
        let mut l = 0;
        let mut c = self.cur_char(&mut l);
        while self.cur() != limit
            && is_char(c)
            && c != u32::from(b'<')
            && self.instate != State::Eof
        {
            if buf.len() > MAX_TEXT_LENGTH {
                self.fatal_msg(Code::AttributeNotFinished, b"AttValue length too long\n".to_vec());
                self.err_memory(None);
                return None;
            }
            if c == u32::from(b'&') {
                in_space = false;
                if self.nxt(1) == b'#' {
                    let val = self.parse_char_ref();
                    if val == u32::from(b'&') {
                        if self.replace_entities {
                            buf.push(b'&');
                        } else {
                            buf.extend_from_slice(b"&#38;");
                        }
                    } else if val != 0 {
                        copy_buf(&mut buf, val);
                    }
                } else {
                    let ent = self.parse_entity_ref();
                    self.nbentities += 1;
                    if let Some(ent) = &ent {
                        self.nbentities += ent.borrow().owner as u64;
                    }
                    match ent {
                        Some(ent) if ent.borrow().etype == EntityType::InternalPredefined => {
                            let first = ent.borrow().content.as_ref().map_or(0, |c| at(c, 0));
                            if !self.replace_entities && first == b'&' {
                                buf.extend_from_slice(b"&#38;");
                            } else {
                                buf.push(first);
                            }
                        }
                        Some(ent) if self.replace_entities => {
                            let content = ent.borrow().content.clone().unwrap_or_default();
                            self.depth += 1;
                            let rep = self.string_decode_entities(&content, SUBSTITUTE_REF);
                            self.depth -= 1;
                            if let Some(rep) = rep {
                                for b in rep {
                                    if b == 0xD || b == 0xA || b == 0x9 {
                                        buf.push(0x20);
                                    } else {
                                        buf.push(b);
                                    }
                                }
                            }
                        }
                        Some(ent) => {
                            let (etype, content, checked, name) = {
                                let e = ent.borrow();
                                (e.etype, e.content.clone(), e.checked, e.name.clone())
                            };
                            if etype != EntityType::InternalPredefined
                                && checked == 0
                                && let Some(content) = content
                            {
                                let old = self.nbentities;
                                self.depth += 1;
                                let rep = self.string_decode_entities(&content, SUBSTITUTE_REF);
                                self.depth -= 1;
                                let diff = (self.nbentities - old + 1).min(i32::MAX as u64 / 2);
                                ent.borrow_mut().checked = (diff * 2) as i32;
                                match rep {
                                    Some(rep) => {
                                        if rep.contains(&b'<') {
                                            ent.borrow_mut().checked |= 1;
                                        }
                                    }
                                    None => zero_content(&ent),
                                }
                            }
                            buf.push(b'&');
                            buf.extend_from_slice(&name);
                            buf.push(b';');
                        }
                        None => {}
                    }
                }
            } else {
                if c == 0x20 || c == 0xD || c == 0xA || c == 0x9 {
                    if !buf.is_empty() || !normalize {
                        if !normalize || !in_space {
                            buf.push(0x20);
                        }
                        in_space = true;
                    }
                } else {
                    in_space = false;
                    copy_buf(&mut buf, c);
                }
                self.nextl(l);
            }
            c = self.cur_char(&mut l);
        }
        if self.instate == State::Eof {
            return None;
        }
        if in_space && normalize {
            while buf.last() == Some(&0x20) {
                buf.pop();
            }
        }
        if self.cur() == b'<' {
            self.fatal_err(Code::LtInAttribute, None);
        } else if self.cur() != limit {
            if c != 0 && !is_char(c) {
                self.fatal_msg(
                    Code::InvalidChar,
                    b"invalid character in attribute value\n".to_vec(),
                );
            } else {
                self.fatal_msg(Code::AttributeNotFinished, b"AttValue: ' expected\n".to_vec());
            }
        } else {
            self.next();
        }
        Some(buf)
    }

    /// `xmlParseAttValue`.
    pub(super) fn parse_att_value(&mut self) -> Option<Vec<u8>> {
        self.parse_att_value_internal(false)
    }

    // Tags.

    /// `xmlParseAttribute2`. It returns the name, the prefix and the value.
    fn parse_attribute2(&mut self, pref: Option<&[u8]>, elem: &[u8]) -> (Name, Name, Name) {
        let (name, prefix) = self.parse_qname();
        let Some(name) = name else {
            self.fatal_msg(Code::NameRequired, b"error parsing attribute name\n".to_vec());
            return (None, prefix, None);
        };
        let normalize = {
            let qelem = match pref {
                Some(p) => [p, b":", elem].concat(),
                None => elem.to_vec(),
            };
            let qname = match &prefix {
                Some(p) => [&p[..], b":", &name].concat(),
                None => name.clone(),
            };
            self.atts_special.borrow().get(&(qelem, qname)).is_some_and(|&t| t != 0)
        };
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal_msg(
                Code::AttributeWithoutValue,
                [&b"Specification mandates value for attribute "[..], &name, b"\n"].concat(),
            );
            return (None, prefix, None);
        }
        self.next();
        self.skip_blanks();
        let mut val = self.parse_att_value_internal(normalize);
        if normalize && let Some(v) = &val {
            val = Some(normalize_space(v));
        }
        self.instate = State::Content;
        if prefix.as_deref() == Some(b"xml") && &name[..] == b"space" {
            let v = val.as_deref().map(cstr).unwrap_or_default();
            if v != b"default" && v != b"preserve" {
                let msg = [
                    &b"Invalid value \""[..],
                    v,
                    b"\" for xml:space : \"default\" or \"preserve\" expected\n",
                ]
                .concat();
                self.warning_msg(Code::WarSpaceValue, msg);
            }
        }
        (Some(name), prefix, val)
    }

    /// `xmlGetNamespace`.
    fn get_namespace(&self, prefix: Option<&[u8]>) -> Option<Vec<u8>> {
        if prefix == Some(b"xml") {
            return Some(XML_NS.to_vec());
        }
        for (p, url) in self.ns_tab.iter().rev() {
            if p.as_deref() == prefix {
                if prefix.is_none() && url.is_empty() {
                    return None;
                }
                return Some(url.clone());
            }
        }
        None
    }

    /// `xmlParseStartTag2`. It returns the local name, the prefix, the namespace name and the number of namespaces that it pushes.
    fn parse_start_tag2(&mut self) -> Option<(Vec<u8>, Name, Name, usize)> {
        if self.cur() != b'<' {
            return None;
        }
        self.next1();
        self.shrink();
        let inputid = self.input().id;
        let mut nb_ns = 0;
        let mut atts: Vec<Att> = Vec::new();
        let (localname, prefix) = self.parse_qname();
        let Some(localname) = localname else {
            self.fatal_msg(Code::NameRequired, b"StartTag: invalid element name\n".to_vec());
            return None;
        };
        self.skip_blanks();
        while self.cur() != b'>'
            && (self.cur() != b'/' || self.nxt(1) != b'>')
            && is_byte_char(self.cur())
            && self.instate != State::Eof
        {
            let q = self.pos();
            let (attname, aprefix, attvalue) = self.parse_attribute2(prefix.as_deref(), &localname);
            if let (Some(attname), Some(attvalue)) = (&attname, &attvalue) {
                let url = attvalue.clone();
                if &attname[..] == b"xmlns" && aprefix.is_none() {
                    if url == XML_NS || url == XMLNS_NS {
                        self.ns_err();
                    } else {
                        let n = self.ns_tab.len();
                        if self.ns_tab[n - nb_ns..].iter().any(|(p, _)| p.is_none()) {
                            self.err_attribute_dup(None, attname);
                        } else {
                            self.ns_tab.push((None, url));
                            nb_ns += 1;
                        }
                    }
                } else if aprefix.as_deref() == Some(b"xmlns") {
                    let skip = &attname[..] == b"xml"
                        || url == XML_NS
                        || &attname[..] == b"xmlns"
                        || url == XMLNS_NS
                        || url.is_empty();
                    if skip {
                        self.ns_err();
                    } else {
                        let n = self.ns_tab.len();
                        if self.ns_tab[n - nb_ns..]
                            .iter()
                            .any(|(p, _)| p.as_deref() == Some(&attname[..]))
                        {
                            self.err_attribute_dup(aprefix.as_deref(), attname);
                        } else {
                            self.ns_tab.push((Some(attname.clone()), url));
                            nb_ns += 1;
                        }
                    }
                } else {
                    atts.push(Att { name: attname.clone(), prefix: aprefix.clone(), nsname: None });
                }
            }
            if self.instate == State::Eof {
                break;
            }
            if self.cur() == b'>' || (self.cur() == b'/' && self.nxt(1) == b'>') {
                break;
            }
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"attributes construct error\n".to_vec());
                break;
            }
            if q == self.pos() && attname.is_none() && attvalue.is_none() {
                self.fatal_msg(
                    Code::InternalError,
                    b"xmlParseStartTag: problem parsing attributes\n".to_vec(),
                );
                break;
            }
        }
        if self.input().id != inputid {
            self.fatal_msg(Code::InternalError, b"Unexpected change of input\n".to_vec());
            return None;
        }
        for i in 0..atts.len() {
            let nsname = match &atts[i].prefix {
                Some(p) => {
                    let ns = self.get_namespace(Some(p));
                    if ns.is_none() {
                        self.ns_err();
                    }
                    ns
                }
                None => None,
            };
            atts[i].nsname = nsname.clone();
            for j in 0..i {
                if atts[i].name == atts[j].name {
                    if atts[i].prefix == atts[j].prefix {
                        let (p, n) = (atts[i].prefix.clone(), atts[i].name.clone());
                        self.err_attribute_dup(p.as_deref(), &n);
                        break;
                    }
                    if nsname.is_some() && atts[j].nsname == nsname {
                        self.ns_err();
                        break;
                    }
                }
            }
        }
        let nsname = self.get_namespace(prefix.as_deref());
        if prefix.is_some() && nsname.is_none() {
            self.ns_err();
        }
        if !self.disable_sax {
            self.sax_start_element();
        }
        Some((localname, prefix, nsname, nb_ns))
    }

    /// `xmlSAX2StartElementNs`.
    fn sax_start_element(&mut self) {
        let has_parent = !self.nodes.is_empty();
        self.nodemem = -1;
        if self.node_push(Node::new()) < 0 {
            return;
        }
        if has_parent {
            let n = self.nodes.len();
            self.nodes[n - 2].add(Kind::Other);
        }
    }

    /// `xmlSAX2EndElementNs`.
    fn sax_end_element(&mut self) {
        self.nodemem = -1;
        self.node_pop();
    }

    /// `nsPop` of a number of namespaces.
    fn ns_pop(&mut self, nr: usize) {
        let n = self.ns_tab.len();
        self.ns_tab.truncate(n - nr.min(n));
    }

    /// `xmlParseEndTag2` on the top of the name stack.
    fn parse_end_tag2(&mut self) {
        if self.cur() != b'<' || self.nxt(1) != b'/' {
            self.fatal_err(Code::LtslashRequired, None);
            return;
        }
        self.skip(2);
        let tag = self.names.last().cloned().expect("a start tag");
        let name = match &tag.prefix {
            None => self.parse_name_and_compare(&tag.name),
            Some(p) => self.parse_qname_and_compare(&tag.name, Some(p)),
        };
        if self.instate == State::Eof {
            return;
        }
        self.skip_blanks();
        if !is_byte_char(self.cur()) || self.cur() != b'>' {
            self.fatal_err(Code::GtRequired, None);
        } else {
            self.next1();
        }
        if let Err(name) = name {
            let name = name.unwrap_or_else(|| b"unparsable".to_vec());
            let msg = [
                &b"Opening and ending tag mismatch: "[..],
                &tag.name,
                format!(" line {} and ", tag.line).as_bytes(),
                &name,
                b"\n",
            ]
            .concat();
            self.fatal_msg(Code::TagNameMismatch, msg);
        }
        if !self.disable_sax {
            self.sax_end_element();
        }
        if tag.ns_nr != 0 {
            self.ns_pop(tag.ns_nr);
        }
    }

    /// `xmlParseCDSect`.
    fn parse_cdsect(&mut self) {
        if !self.looking_at(b"<![CDATA[") {
            return;
        }
        self.skip(9);
        self.instate = State::CdataSection;
        let mut rl = 0;
        let mut sl = 0;
        let mut l = 0;
        let mut r = self.cur_char(&mut rl);
        if !is_char(r) {
            self.fatal_err(Code::CdataNotFinished, None);
            self.instate = State::Content;
            return;
        }
        self.nextl(rl);
        let mut s = self.cur_char(&mut sl);
        if !is_char(s) {
            self.fatal_err(Code::CdataNotFinished, None);
            self.instate = State::Content;
            return;
        }
        self.nextl(sl);
        let mut cur = self.cur_char(&mut l);
        let mut buf = Vec::new();
        let mut size = BUFFER_SIZE;
        let mut count = 0;
        let rb = u32::from(b']');
        while is_char(cur) && (r != rb || s != rb || cur != u32::from(b'>')) {
            if buf.len() + 5 >= size {
                if size > MAX_TEXT_LENGTH {
                    self.fatal_msg(Code::CdataNotFinished, b"CData section too big found".to_vec());
                    return;
                }
                size *= 2;
            }
            copy_buf(&mut buf, r);
            r = s;
            s = cur;
            sl = l;
            count += 1;
            if count > 50 {
                self.shrink();
                if self.instate == State::Eof {
                    return;
                }
                count = 0;
            }
            let _ = sl;
            self.nextl(l);
            cur = self.cur_char(&mut l);
        }
        self.instate = State::Content;
        if cur != u32::from(b'>') {
            let msg =
                [&b"CData section not finished\n"[..], &buf[..buf.len().min(50)], b"\n"].concat();
            self.fatal_msg(Code::CdataNotFinished, msg);
            return;
        }
        self.nextl(l);
        if !self.disable_sax {
            self.sax2_text(buf.len(), true);
        }
    }

    // Content and elements.

    /// `xmlParseContentInternal`.
    fn parse_content_internal(&mut self) {
        let name_nr = self.names.len();
        while self.cur() != 0 && self.instate != State::Eof {
            let test = self.pos();
            let id = self.input().id;
            let depth = self.inputs.len();
            if self.cur() == b'<' && self.nxt(1) == b'?' {
                self.parse_pi();
            } else if self.looking_at(b"<![CDATA[") {
                self.parse_cdsect();
            } else if self.looking_at(b"<!--") {
                self.parse_comment();
                self.instate = State::Content;
            } else if self.cur() == b'<' {
                if self.nxt(1) == b'/' {
                    if self.names.len() <= name_nr {
                        break;
                    }
                    self.parse_element_end();
                } else {
                    self.parse_element_start();
                }
            } else if self.cur() == b'&' {
                self.parse_reference();
            } else {
                self.parse_char_data();
            }
            self.shrink();
            if test == self.pos() && id == self.input().id && depth == self.inputs.len() {
                self.fatal_msg(
                    Code::InternalError,
                    b"detected an error in element content\n".to_vec(),
                );
                self.halt();
                break;
            }
        }
    }

    /// `xmlParseContent`.
    pub(super) fn parse_content(&mut self) {
        let name_nr = self.names.len();
        self.parse_content_internal();
        if self.instate != State::Eof && self.names.len() > name_nr {
            self.premature_end();
        }
    }

    /// The "Premature end of data" error on the top of the name stack.
    fn premature_end(&mut self) {
        let tag = self.names.last().expect("a start tag");
        let msg = [
            &b"Premature end of data in tag "[..],
            &tag.name,
            format!(" line {}\n", tag.line).as_bytes(),
        ]
        .concat();
        self.fatal_msg(Code::TagNotFinished, msg);
    }

    /// `xmlParseElement`.
    pub(super) fn parse_element(&mut self) {
        if self.parse_element_start() != 0 {
            return;
        }
        self.parse_content_internal();
        if self.instate == State::Eof {
            return;
        }
        if self.cur() == 0 {
            self.premature_end();
            return;
        }
        self.parse_element_end();
    }

    /// `xmlParseElementStart`.
    fn parse_element_start(&mut self) -> i32 {
        if self.names.len() > MAX_DEPTH {
            self.fatal_msg(
                Code::InternalError,
                format!("Excessive depth in document: {MAX_DEPTH} use XML_PARSE_HUGE option\n")
                    .into_bytes(),
            );
            self.halt();
            return -1;
        }
        let line = self.input().line;
        let tag = self.parse_start_tag2();
        if self.instate == State::Eof {
            return -1;
        }
        let Some((name, prefix, _, ns_nr)) = tag else {
            return -1;
        };
        self.names.push(StartTag { name: name.clone(), prefix, line, ns_nr });
        if self.cur() == b'/' && self.nxt(1) == b'>' {
            self.skip(2);
            if !self.disable_sax {
                self.sax_end_element();
            }
            self.names.pop();
            if ns_nr != 0 {
                self.ns_pop(ns_nr);
            }
            return 1;
        }
        if self.cur() == b'>' {
            self.next1();
        } else {
            let msg = [
                &b"Couldn't find end of Start Tag "[..],
                &name,
                format!(" line {line}\n").as_bytes(),
            ]
            .concat();
            self.fatal_msg(Code::GtRequired, msg);
            self.node_pop();
            self.names.pop();
            if ns_nr != 0 {
                self.ns_pop(ns_nr);
            }
            return -1;
        }
        0
    }

    /// `xmlParseElementEnd`.
    fn parse_element_end(&mut self) {
        if self.names.is_empty() {
            return;
        }
        self.parse_end_tag2();
        self.names.pop();
    }
}

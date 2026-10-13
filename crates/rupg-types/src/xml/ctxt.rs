//! The parser state of the `xml` checker, from libxml2 2.9.14 `parser.c`, `parserInternals.c` and `error.c`.
//!
//! PostgreSQL checks an `xml` value with libxml2 and copies the libxml2 error messages into the error detail. The checker in this module follows the libxml2 code path by path, so that the same input gives the same messages, the same line numbers and the same context lines. The names of the functions follow the libxml2 names, so that the two can be compared.
//!
//! This file holds the parser context: the input stack, the error helpers, the character decoder and the name parsers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::dtd::{Doc, Entity};

/// `XML_MAX_TEXT_LENGTH`.
pub(super) const MAX_TEXT_LENGTH: usize = 10_000_000;
/// `XML_MAX_NAME_LENGTH`.
pub(super) const MAX_NAME_LENGTH: usize = 50_000;
/// `XML_MAX_NAMELEN`.
pub(super) const MAX_NAMELEN: usize = 100;
/// `XML_PARSER_BUFFER_SIZE`.
pub(super) const BUFFER_SIZE: usize = 100;
/// `XML_PARSER_BIG_BUFFER_SIZE`.
pub(super) const BIG_BUFFER_SIZE: usize = 300;
/// `XML_PARSER_CHUNK_SIZE`.
pub(super) const CHUNK_SIZE: usize = 100;
/// `xmlParserMaxDepth`.
pub(super) const MAX_DEPTH: usize = 256;
/// `INPUT_CHUNK`.
const INPUT_CHUNK: usize = 250;
/// `LINE_LEN`.
const LINE_LEN: usize = 80;

/// `XML_PARSE_NOENT`.
pub(super) const PARSE_NOENT: u32 = 1 << 1;
/// `XML_PARSE_DTDLOAD`.
pub(super) const PARSE_DTDLOAD: u32 = 1 << 2;
/// `XML_PARSE_DTDATTR`.
pub(super) const PARSE_DTDATTR: u32 = 1 << 3;
/// `XML_PARSE_DTDVALID`.
pub(super) const PARSE_DTDVALID: u32 = 1 << 4;
/// `XML_PARSE_NODICT`.
pub(super) const PARSE_NODICT: u32 = 1 << 12;

/// `XML_COMPLETE_ATTRS`.
pub(super) const COMPLETE_ATTRS: i32 = 4;
/// `XML_SKIP_IDS`.
pub(super) const SKIP_IDS: i32 = 8;

/// The libxml2 error codes that the checker raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Code {
    Ok,
    InternalError,
    NoMemory,
    DocumentEmpty,
    DocumentEnd,
    InvalidHexCharref,
    InvalidDecCharref,
    InvalidCharref,
    InvalidChar,
    PerefNoName,
    PerefSemicolMissing,
    EntityRefSemicolMissing,
    UndeclaredEntity,
    WarUndeclaredEntity,
    UnparsedEntity,
    EntityIsExternal,
    EntityIsParameter,
    UnsupportedEncoding,
    EntityNotStarted,
    EntityNotFinished,
    EntityCharError,
    ValueRequired,
    LtInAttribute,
    AttributeNotStarted,
    AttributeNotFinished,
    AttributeWithoutValue,
    AttributeRedefined,
    LiteralNotStarted,
    LiteralNotFinished,
    CommentNotFinished,
    PiNotStarted,
    PiNotFinished,
    NotationNotStarted,
    NotationNotFinished,
    AttlistNotStarted,
    AttlistNotFinished,
    MixedNotStarted,
    ElemcontentNotStarted,
    ElemcontentNotFinished,
    XmldeclNotFinished,
    DoctypeNotFinished,
    MisplacedCdataEnd,
    CdataNotFinished,
    ReservedXmlName,
    SpaceRequired,
    SeparatorRequired,
    NmtokenRequired,
    NameRequired,
    PcdataRequired,
    UriRequired,
    PubidRequired,
    GtRequired,
    LtslashRequired,
    EqualRequired,
    TagNameMismatch,
    TagNotFinished,
    EntityPeInternal,
    EntityLoop,
    EntityBoundary,
    InvalidUri,
    UriFragment,
    WarCatalogPi,
    VersionMissing,
    WarUnknownVersion,
    UnknownVersion,
    WarSpaceValue,
    EntityProcessing,
    NotationProcessing,
    WarNsUri,
    StringNotStarted,
    StringNotClosed,
    EncodingName,
    StandaloneValue,
    HyphenInComment,
    NotWellBalanced,
    ExtraContent,
    PerefInIntSubset,
    NameTooLong,
    /// Any error of the namespace domain. PostgreSQL ignores them all.
    Ns,
    /// Any validity error. PostgreSQL ignores them all.
    Valid,
}

impl Code {
    /// The message table of `xmlFatalErr`.
    fn fatal_message(self) -> &'static str {
        match self {
            Code::InvalidHexCharref => "CharRef: invalid hexadecimal value",
            Code::InvalidDecCharref => "CharRef: invalid decimal value",
            Code::InvalidCharref => "CharRef: invalid value",
            Code::InternalError => "internal error",
            Code::PerefNoName => "PEReference: no name",
            Code::PerefSemicolMissing => "PEReference: expecting ';'",
            Code::EntityLoop => "Detected an entity reference loop",
            Code::EntityNotStarted => "EntityValue: \" or ' expected",
            Code::EntityPeInternal => "PEReferences forbidden in internal subset",
            Code::EntityNotFinished => "EntityValue: \" or ' expected",
            Code::AttributeNotStarted => "AttValue: \" or ' expected",
            Code::LtInAttribute => "Unescaped '<' not allowed in attributes values",
            Code::LiteralNotStarted => "SystemLiteral \" or ' expected",
            Code::LiteralNotFinished => "Unfinished System or Public ID \" or ' expected",
            Code::MisplacedCdataEnd => "Sequence ']]>' not allowed in content",
            Code::UriRequired => "SYSTEM or PUBLIC, the URI is missing",
            Code::PubidRequired => "PUBLIC, the Public Identifier is missing",
            Code::HyphenInComment => "Comment must not contain '--' (double-hyphen)",
            Code::PiNotStarted => "xmlParsePI : no target name",
            Code::ReservedXmlName => "Invalid PI name",
            Code::NotationNotStarted => "NOTATION: Name expected here",
            Code::NotationNotFinished => "'>' required to close NOTATION declaration",
            Code::UriFragment => "Fragment not allowed",
            Code::AttlistNotStarted => "'(' required to start ATTLIST enumeration",
            Code::NmtokenRequired => "NmToken expected in ATTLIST enumeration",
            Code::AttlistNotFinished => "')' required to finish ATTLIST enumeration",
            Code::MixedNotStarted => "MixedContentDecl : '|' or ')*' expected",
            Code::PcdataRequired => "MixedContentDecl : '#PCDATA' expected",
            Code::ElemcontentNotStarted => "ContentDecl : Name or '(' expected",
            Code::ElemcontentNotFinished => "ContentDecl : ',' '|' or ')' expected",
            Code::PerefInIntSubset => {
                "PEReference: forbidden within markup decl in internal subset"
            }
            Code::GtRequired => "expected '>'",
            Code::ValueRequired => "Entity value required",
            Code::XmldeclNotFinished => "parsing XML declaration: '?>' expected",
            Code::EntityRefSemicolMissing => "EntityRef: expecting ';'",
            Code::DoctypeNotFinished => "DOCTYPE improperly terminated",
            Code::LtslashRequired => "EndTag: '</' not found",
            Code::EqualRequired => "expected '='",
            Code::StringNotClosed => "String not closed expecting \" or '",
            Code::StringNotStarted => "String not started expecting ' or \"",
            Code::EncodingName => "Invalid XML encoding name",
            Code::StandaloneValue => "standalone accepts only 'yes' or 'no'",
            Code::DocumentEmpty => "Document is empty",
            Code::DocumentEnd => "Extra content at the end of the document",
            Code::NotWellBalanced => "chunk is not well balanced",
            Code::ExtraContent => "extra content at the end of well balanced chunk",
            Code::VersionMissing => "Malformed declaration expecting version",
            Code::NameTooLong => "Name too long use XML_PARSE_HUGE option",
            _ => "Unregistered error message",
        }
    }
}

/// `xmlParserInputState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Eof,
    Start,
    Misc,
    Pi,
    Dtd,
    Prolog,
    Comment,
    Content,
    CdataSection,
    EntityDecl,
    EntityValue,
    AttributeValue,
    SystemLiteral,
    Epilog,
    PublicLiteral,
}

/// `xmlErrorDomain`, for the domains that the checker uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Domain {
    Parser,
    Namespace,
    Dtd,
}

/// `xmlErrorLevel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Level {
    Warning,
    Error,
    Fatal,
}

/// The errors and warnings that PostgreSQL keeps from one parse. This is the state of `PgXmlErrorContext`.
#[derive(Default)]
pub(super) struct Report {
    pub(super) err_buf: Vec<u8>,
    pub(super) err_occurred: bool,
    pub(super) warnings: Vec<String>,
}

/// `xmlParserInput`. The bytes past `end` read as zero, as the C string terminator does.
#[derive(Clone)]
pub(super) struct Input {
    pub(super) buf: Rc<[u8]>,
    pub(super) base: usize,
    pub(super) cur: usize,
    pub(super) end: usize,
    pub(super) line: i32,
    pub(super) id: i32,
    pub(super) filename: bool,
    /// The input has a buffer that `xmlParserInputShrink` can shrink.
    pub(super) shrinkable: bool,
    pub(super) standalone: i32,
    pub(super) encoding: bool,
}

impl Input {
    pub(super) fn new(buf: Rc<[u8]>, end: usize, id: i32) -> Input {
        Input {
            buf,
            base: 0,
            cur: 0,
            end,
            line: 1,
            id,
            filename: false,
            shrinkable: false,
            standalone: -1,
            encoding: false,
        }
    }

    #[inline]
    pub(super) fn at(&self, i: usize) -> u8 {
        if i < self.end { self.buf[i] } else { 0 }
    }
}

/// The kind of the last or first child of a node. The tree that libxml2 builds is not kept, but these facts change the errors that it raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    None,
    Text(usize),
    Cdata(usize),
    Other,
}

/// A node of the node stack, with a summary of its children.
#[derive(Clone, Copy, Debug)]
pub(super) struct Node {
    pub(super) first: Kind,
    pub(super) last: Kind,
    pub(super) count: usize,
}

impl Node {
    pub(super) fn new() -> Node {
        Node { first: Kind::None, last: Kind::None, count: 0 }
    }

    /// `xmlAddChild` of a node that does not merge.
    pub(super) fn add(&mut self, kind: Kind) {
        if self.count == 0 {
            self.first = kind;
        }
        self.last = kind;
        self.count += 1;
    }

    /// `xmlAddChild` of a text node, which merges with a last text child.
    pub(super) fn add_text(&mut self, len: usize) {
        if let Kind::Text(old) = self.last {
            self.last = Kind::Text(old + len);
            if self.count == 1 {
                self.first = self.last;
            }
        } else {
            self.add(Kind::Text(len));
        }
    }
}

/// An entry of `nameTab` and `pushTab`.
#[derive(Clone)]
pub(super) struct StartTag {
    pub(super) name: Vec<u8>,
    pub(super) prefix: Option<Vec<u8>>,
    pub(super) line: i32,
    pub(super) ns_nr: usize,
}

/// The attribute types of `attsSpecial`, keyed by element and attribute.
pub(super) type SpecialAttrs = HashMap<(Vec<u8>, Vec<u8>), i32>;

/// `xmlParserCtxt`, with the fields that change the errors.
pub(super) struct Ctxt {
    pub(super) inputs: Vec<Input>,
    pub(super) instate: State,
    pub(super) well_formed: bool,
    pub(super) disable_sax: bool,
    pub(super) err_no: Code,
    pub(super) last_error: Code,
    pub(super) standalone: i32,
    pub(super) has_external_subset: bool,
    pub(super) has_perefs: bool,
    pub(super) in_subset: i32,
    pub(super) external: bool,
    pub(super) replace_entities: bool,
    pub(super) loadsubset: i32,
    pub(super) validate: bool,
    pub(super) options: u32,
    pub(super) depth: i32,
    pub(super) nbentities: u64,
    pub(super) sizeentities: u64,
    pub(super) sizeentcopy: u64,
    pub(super) input_id: i32,
    pub(super) version: Option<Vec<u8>>,
    pub(super) encoding: Option<Vec<u8>>,
    pub(super) names: Vec<StartTag>,
    pub(super) ns_tab: Vec<(Option<Vec<u8>>, Vec<u8>)>,
    pub(super) nodes: Vec<Node>,
    pub(super) nodelen: i64,
    pub(super) nodemem: i64,
    pub(super) my_doc: Option<Rc<RefCell<Doc>>>,
    pub(super) report: Rc<RefCell<Report>>,
    pub(super) atts_special: Rc<RefCell<SpecialAttrs>>,
}

/// Builds a message from pieces of bytes.
pub(super) fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// The bytes of a C string: the part before the first zero byte.
pub(super) fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&b| b == 0) {
        Some(n) => &s[..n],
        None => s,
    }
}

/// `IS_CHAR`.
#[inline]
pub(super) fn is_char(c: u32) -> bool {
    if c < 0x100 {
        c == 0x9 || c == 0xA || c == 0xD || c >= 0x20
    } else {
        (0x100..=0xD7FF).contains(&c)
            || (0xE000..=0xFFFD).contains(&c)
            || (0x10000..=0x10FFFF).contains(&c)
    }
}

/// `IS_BYTE_CHAR`.
#[inline]
pub(super) fn is_byte_char(c: u8) -> bool {
    c == 0x9 || c == 0xA || c == 0xD || c >= 0x20
}

/// `IS_BLANK_CH`.
#[inline]
pub(super) fn is_blank(c: u32) -> bool {
    c == 0x20 || c == 0x9 || c == 0xA || c == 0xD
}

/// `IS_PUBIDCHAR_CH`.
#[inline]
pub(super) fn is_pubid_char(c: u32) -> bool {
    matches!(c, 0xA | 0xD | 0x20 | 0x21 | 0x23..=0x25 | 0x27..=0x3B | 0x3D | 0x3F..=0x5A | 0x5F | 0x61..=0x7A)
}

/// `xmlIsNameStartChar` without `XML_PARSE_OLD10`.
pub(super) fn is_name_start_char(c: u32) -> bool {
    c != u32::from(b' ')
        && c != u32::from(b'>')
        && c != u32::from(b'/')
        && ((c >= u32::from(b'a') && c <= u32::from(b'z'))
            || (c >= u32::from(b'A') && c <= u32::from(b'Z'))
            || c == u32::from(b'_')
            || c == u32::from(b':')
            || (0xC0..=0xD6).contains(&c)
            || (0xD8..=0xF6).contains(&c)
            || (0xF8..=0x2FF).contains(&c)
            || (0x370..=0x37D).contains(&c)
            || (0x37F..=0x1FFF).contains(&c)
            || (0x200C..=0x200D).contains(&c)
            || (0x2070..=0x218F).contains(&c)
            || (0x2C00..=0x2FEF).contains(&c)
            || (0x3001..=0xD7FF).contains(&c)
            || (0xF900..=0xFDCF).contains(&c)
            || (0xFDF0..=0xFFFD).contains(&c)
            || (0x10000..=0xEFFFF).contains(&c))
}

/// `xmlIsNameChar` without `XML_PARSE_OLD10`.
pub(super) fn is_name_char(c: u32) -> bool {
    is_name_start_char(c)
        || (c >= u32::from(b'0') && c <= u32::from(b'9'))
        || c == u32::from(b'-')
        || c == u32::from(b'.')
        || c == 0xB7
        || (0x300..=0x36F).contains(&c)
        || (0x203F..=0x2040).contains(&c)
}

/// `COPY_BUF`: appends a character as UTF-8.
pub(super) fn copy_buf(buf: &mut Vec<u8>, c: u32) {
    if c < 0x80 {
        buf.push(c as u8);
    } else if c < 0x800 {
        buf.push(0xC0 | (c >> 6) as u8);
        buf.push(0x80 | (c & 0x3F) as u8);
    } else if c < 0x10000 {
        buf.push(0xE0 | (c >> 12) as u8);
        buf.push(0x80 | ((c >> 6) & 0x3F) as u8);
        buf.push(0x80 | (c & 0x3F) as u8);
    } else {
        buf.push(0xF0 | (c >> 18) as u8);
        buf.push(0x80 | ((c >> 12) & 0x3F) as u8);
        buf.push(0x80 | ((c >> 6) & 0x3F) as u8);
        buf.push(0x80 | (c & 0x3F) as u8);
    }
}

/// The byte at `i` of a C string, zero past the end.
#[inline]
pub(super) fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `XML_GET_VAR_STR`: libxml2 cuts a very long message to 149 bytes.
fn var_str(mut msg: Vec<u8>) -> Vec<u8> {
    if msg.len() >= 150 && msg.len() + 151 >= 64000 {
        msg.truncate(149);
    }
    msg
}

/// `chopStringInfoNewlines`.
fn chop_newlines(buf: &mut Vec<u8>) {
    while buf.last() == Some(&b'\n') {
        buf.pop();
    }
}

/// `appendStringInfoLineSeparator`.
fn line_separator(buf: &mut Vec<u8>) {
    chop_newlines(buf);
    if !buf.is_empty() {
        buf.push(b'\n');
    }
}

/// `xmlParserPrintFileContextInternal`.
fn print_file_context(input: &Input, out: &mut Vec<u8>) {
    let base = input.base;
    let mut cur = input.cur;
    while cur > base && (input.at(cur) == b'\n' || input.at(cur) == b'\r') {
        cur -= 1;
    }
    let mut n = 0;
    loop {
        let go = n < 80;
        n += 1;
        if !(go && cur > base && input.at(cur) != b'\n' && input.at(cur) != b'\r') {
            break;
        }
        cur -= 1;
    }
    if input.at(cur) == b'\n' || input.at(cur) == b'\r' {
        cur += 1;
    }
    let col = input.cur - cur;
    let mut content = Vec::with_capacity(81);
    while input.at(cur) != 0
        && input.at(cur) != b'\n'
        && input.at(cur) != b'\r'
        && content.len() < 80
    {
        content.push(input.at(cur));
        cur += 1;
    }
    out.extend_from_slice(&content);
    out.push(b'\n');
    let mut n = 0;
    let mut i = 0;
    loop {
        if n >= col {
            break;
        }
        let go = n < 79;
        n += 1;
        if !go || i >= content.len() {
            break;
        }
        if content[i] != b'\t' {
            content[i] = b' ';
        }
        i += 1;
    }
    content.truncate(i);
    content.push(b'^');
    out.extend_from_slice(&content);
    out.push(b'\n');
}

impl Ctxt {
    pub(super) fn new(
        report: Rc<RefCell<Report>>,
        atts_special: Rc<RefCell<SpecialAttrs>>,
    ) -> Ctxt {
        Ctxt {
            inputs: Vec::new(),
            instate: State::Start,
            well_formed: true,
            disable_sax: false,
            err_no: Code::Ok,
            last_error: Code::Ok,
            standalone: -1,
            has_external_subset: false,
            has_perefs: false,
            in_subset: 0,
            external: false,
            replace_entities: false,
            loadsubset: 0,
            validate: false,
            options: 0,
            depth: 0,
            nbentities: 0,
            sizeentities: 0,
            sizeentcopy: 0,
            input_id: 1,
            version: None,
            encoding: None,
            names: Vec::new(),
            ns_tab: Vec::new(),
            nodes: Vec::new(),
            nodelen: 0,
            nodemem: 0,
            my_doc: None,
            report,
            atts_special,
        }
    }

    /// `xmlCtxtUseOptionsInternal`, for the fields that matter here.
    pub(super) fn use_options(&mut self, options: u32) {
        self.loadsubset = 0;
        if options & PARSE_DTDLOAD != 0 {
            self.loadsubset = 2;
        }
        if options & PARSE_DTDATTR != 0 {
            self.loadsubset |= COMPLETE_ATTRS;
        }
        self.replace_entities = options & PARSE_NOENT != 0;
        self.validate = options & PARSE_DTDVALID != 0;
        self.options = options;
    }

    // The input stack.

    #[inline]
    pub(super) fn input(&self) -> &Input {
        self.inputs.last().expect("the parser has an input")
    }

    #[inline]
    pub(super) fn input_mut(&mut self) -> &mut Input {
        self.inputs.last_mut().expect("the parser has an input")
    }

    /// A new input from bytes, as `xmlNewStringInputStream` and `xmlNewInputStream` make it.
    pub(super) fn new_input(&mut self, buf: Rc<[u8]>, end: usize) -> Input {
        let input = Input::new(buf, end, self.input_id);
        self.input_id += 1;
        input
    }

    /// `xmlNewEntityInputStream`.
    pub(super) fn new_entity_input(&mut self, ent: &Rc<RefCell<Entity>>) -> Option<Input> {
        let (etype, has_content) = {
            let e = ent.borrow();
            (e.etype, e.content.is_some())
        };
        if !has_content {
            use super::dtd::EntityType as T;
            let name = ent.borrow().name.clone();
            match etype {
                T::ExternalGeneralUnparsed => {
                    self.err_internal(cat(&[b"Cannot parse entity ", &name, b"\n"]));
                }
                T::ExternalGeneralParsed | T::ExternalParameter => {
                    // The PostgreSQL entity loader returns an empty string stream.
                    return Some(self.new_input(Rc::from(&b""[..]), 0));
                }
                T::InternalGeneral => {
                    self.err_internal(cat(&[b"Internal entity ", &name, b" without content !\n"]));
                }
                T::InternalParameter => {
                    self.err_internal(cat(&[
                        b"Internal parameter entity ",
                        &name,
                        b" without content !\n",
                    ]));
                }
                T::InternalPredefined => {
                    self.err_internal(cat(&[
                        b"Predefined entity ",
                        &name,
                        b" without content !\n",
                    ]));
                }
            }
            return None;
        }
        let mut e = ent.borrow_mut();
        let content = e.content.clone().unwrap_or_default();
        if e.length == 0 {
            e.length = cstr(&content).len();
        }
        let end = e.length.min(content.len());
        let filename = e.uri.is_some();
        drop(e);
        let mut input = self.new_input(Rc::from(&content[..end]), end);
        input.filename = filename;
        Some(input)
    }

    /// `xmlPushInput`.
    pub(super) fn push_input(&mut self, input: Option<Input>) -> i32 {
        let Some(input) = input else {
            return -1;
        };
        if self.inputs.len() > 40 {
            self.fatal_err(Code::EntityLoop, None);
            self.inputs.truncate(1);
            return -1;
        }
        self.inputs.push(input);
        if self.instate == State::Eof {
            return -1;
        }
        (self.inputs.len() - 1) as i32
    }

    /// `xmlPopInput`.
    pub(super) fn pop_input(&mut self) -> u8 {
        if self.inputs.len() <= 1 {
            return 0;
        }
        if self.in_subset == 0 && self.instate != State::Eof {
            self.fatal_err(Code::InternalError, Some(b"Unfinished entity outside the DTD"));
        }
        self.inputs.pop();
        self.cur()
    }

    /// `xmlHaltParser`.
    pub(super) fn halt(&mut self) {
        self.instate = State::Eof;
        self.disable_sax = true;
        self.inputs.truncate(1);
        if let Some(input) = self.inputs.last_mut() {
            input.buf = Rc::from(&b""[..]);
            input.base = 0;
            input.cur = 0;
            input.end = 0;
            input.shrinkable = false;
        }
    }

    /// The `SHRINK` macro. Only a memory buffer shrinks, and only the lower bound of the context line changes.
    pub(super) fn shrink(&mut self) {
        let input = self.input_mut();
        if input.shrinkable
            && input.cur - input.base > 2 * INPUT_CHUNK
            && input.end - input.cur < 2 * INPUT_CHUNK
        {
            let used = input.cur - input.base;
            if used > INPUT_CHUNK {
                input.base = input.cur - LINE_LEN;
            }
        }
    }

    // The byte access macros.

    /// `RAW` and `CUR`.
    #[inline]
    pub(super) fn cur(&self) -> u8 {
        let input = self.input();
        input.at(input.cur)
    }

    /// `NXT(n)`.
    #[inline]
    pub(super) fn nxt(&self, n: usize) -> u8 {
        let input = self.input();
        input.at(input.cur + n)
    }

    /// `CMPn`: the next bytes are `s`.
    #[inline]
    pub(super) fn looking_at(&self, s: &[u8]) -> bool {
        let input = self.input();
        s.iter().enumerate().all(|(i, &b)| input.at(input.cur + i) == b)
    }

    /// `SKIP(n)`.
    #[inline]
    pub(super) fn skip(&mut self, n: usize) {
        self.input_mut().cur += n;
    }

    /// `NEXT1`.
    #[inline]
    pub(super) fn next1(&mut self) {
        self.input_mut().cur += 1;
    }

    /// `NEXTL(l)`.
    #[inline]
    pub(super) fn nextl(&mut self, l: usize) {
        let input = self.input_mut();
        if input.at(input.cur) == b'\n' {
            input.line += 1;
        }
        input.cur += l;
    }

    /// `CUR_PTR` as an offset in the current input.
    #[inline]
    pub(super) fn pos(&self) -> usize {
        self.input().cur
    }

    /// The bytes of the current input from `from` to the current position.
    pub(super) fn slice(&self, from: usize, to: usize) -> Vec<u8> {
        let input = self.input();
        (from..to).map(|i| input.at(i)).collect()
    }

    // The character decoder.

    /// `xmlCurrentChar`. The input is valid UTF-8, so the encoding errors do not occur. At the end of the parse `l` keeps its value.
    pub(super) fn current_char(&mut self, l: &mut usize) -> u32 {
        if self.instate == State::Eof {
            return 0;
        }
        let input = self.input();
        let cur = input.cur;
        let c = input.at(cur);
        if (0x20..=0x7F).contains(&c) {
            *l = 1;
            return u32::from(c);
        }
        if c & 0x80 != 0 {
            let (val, len) = decode(input, cur);
            *l = len;
            if !is_char(val) {
                self.err_encoding_int(Code::InvalidChar, val);
            }
            return val;
        }
        *l = 1;
        if c == 0 && input.end > cur {
            self.err_encoding_int(Code::InvalidChar, 0);
        }
        if c == 0xD {
            if self.input().at(cur + 1) == 0xA {
                self.input_mut().cur += 1;
            }
            return 0xA;
        }
        u32::from(c)
    }

    /// `CUR_CHAR(l)`, with a fresh length.
    #[inline]
    pub(super) fn cur_char(&mut self, l: &mut usize) -> u32 {
        self.current_char(l)
    }

    /// `xmlStringCurrentChar` on a C string.
    pub(super) fn string_current_char(&mut self, s: &[u8], i: usize, l: &mut usize) -> u32 {
        let c = at(s, i);
        if c & 0x80 != 0 {
            let (val, len) = decode_bytes(|k| at(s, i + k));
            *l = len;
            if !is_char(val) {
                self.err_encoding_int(Code::InvalidChar, val);
            }
            return val;
        }
        *l = 1;
        u32::from(c)
    }

    /// `xmlNextChar`.
    pub(super) fn next_char(&mut self) {
        if self.instate == State::Eof {
            return;
        }
        let input = self.input();
        let cur = input.cur;
        let c = input.at(cur);
        if c == 0 {
            return;
        }
        let input = self.input_mut();
        if c == b'\n' {
            input.line += 1;
        }
        if c & 0x80 != 0 {
            let (val, len) = decode(input, cur);
            input.cur += len;
            if len >= 3
                && ((0xD800..0xE000).contains(&val)
                    || (0xFFFE..0x10000).contains(&val)
                    || val >= 0x110000)
            {
                self.err_encoding_int(Code::InvalidChar, val);
            }
        } else {
            input.cur += 1;
        }
    }

    /// `NEXT`.
    #[inline]
    pub(super) fn next(&mut self) {
        self.next_char();
    }

    /// `SKIP_BLANKS`, that is `xmlSkipBlankChars`.
    pub(super) fn skip_blanks(&mut self) -> usize {
        let mut res = 0;
        if (self.inputs.len() == 1 && self.instate != State::Dtd) || self.instate == State::Start {
            let input = self.input_mut();
            let mut cur = input.cur;
            while is_blank(u32::from(input.at(cur))) {
                if input.at(cur) == b'\n' {
                    input.line += 1;
                }
                cur += 1;
                res += 1;
            }
            input.cur = cur;
        } else {
            let expand_pe = self.external || self.inputs.len() != 1;
            loop {
                let c = self.cur();
                if is_blank(u32::from(c)) {
                    self.next();
                } else if c == b'%' {
                    if !expand_pe || is_blank(u32::from(self.nxt(1))) || self.nxt(1) == 0 {
                        break;
                    }
                    self.parse_pe_reference();
                } else if c == 0 {
                    if self.inputs.len() <= 1 {
                        break;
                    }
                    self.pop_input();
                } else {
                    break;
                }
                res += 1;
            }
        }
        res
    }

    // The error helpers.

    /// `__xmlRaiseError` with the PostgreSQL error handler.
    pub(super) fn raise(&mut self, domain: Domain, code: Code, level: Level, msg: Vec<u8>) {
        self.last_error = code;
        let msg = var_str(msg);
        let line = {
            let n = self.inputs.len();
            let mut input = &self.inputs[n - 1];
            if !input.filename && n > 1 {
                input = &self.inputs[n - 2];
            }
            input.line
        };
        let mut level = level;
        let mut domain = domain;
        if code == Code::WarNsUri {
            level = Level::Error;
            domain = Domain::Namespace;
        }
        let mut report = self.report.borrow_mut();
        match domain {
            Domain::Parser => {
                if code == Code::NotWellBalanced && report.err_occurred {
                    return;
                }
                if code == Code::WarUndeclaredEntity {
                    return;
                }
            }
            Domain::Namespace | Domain::Dtd => return,
        }
        let mut buf = Vec::new();
        if line > 0 {
            buf.extend_from_slice(format!("line {line}: ").as_bytes());
        }
        buf.extend_from_slice(&msg);
        line_separator(&mut buf);
        print_file_context(self.input(), &mut buf);
        chop_newlines(&mut buf);
        if level >= Level::Error {
            line_separator(&mut report.err_buf);
            report.err_buf.extend_from_slice(&buf);
            report.err_occurred = true;
        } else {
            report.warnings.push(String::from_utf8_lossy(&buf).into_owned());
        }
    }

    /// The test at the start of each error helper.
    #[inline]
    fn stopped(&self) -> bool {
        self.disable_sax && self.instate == State::Eof
    }

    /// The common part of the fatal error helpers.
    fn fatal(&mut self, code: Code, msg: Vec<u8>) {
        if self.stopped() {
            return;
        }
        self.err_no = code;
        self.raise(Domain::Parser, code, Level::Fatal, msg);
        self.well_formed = false;
        self.disable_sax = true;
    }

    /// `xmlFatalErr`.
    pub(super) fn fatal_err(&mut self, code: Code, info: Option<&[u8]>) {
        let errmsg = code.fatal_message().as_bytes();
        let msg = match info {
            None => cat(&[errmsg, b"\n"]),
            Some(info) => cat(&[errmsg, b": ", info, b"\n"]),
        };
        self.fatal(code, msg);
    }

    /// `xmlFatalErrMsg` and the other fatal helpers that take a formatted message: `xmlFatalErrMsgInt`, `xmlFatalErrMsgStr`, `xmlFatalErrMsgStrIntStr`, `__xmlErrEncoding` and `xmlErrAttributeDup`.
    pub(super) fn fatal_msg(&mut self, code: Code, msg: Vec<u8>) {
        self.fatal(code, msg);
    }

    /// `xmlErrAttributeDup`.
    pub(super) fn err_attribute_dup(&mut self, prefix: Option<&[u8]>, localname: &[u8]) {
        let msg = match prefix {
            None => cat(&[b"Attribute ", localname, b" redefined\n"]),
            Some(p) => cat(&[b"Attribute ", p, b":", localname, b" redefined\n"]),
        };
        self.fatal(Code::AttributeRedefined, msg);
    }

    /// `xmlErrInternal`.
    pub(super) fn err_internal(&mut self, msg: Vec<u8>) {
        self.fatal(Code::InternalError, msg);
    }

    /// `xmlErrEncodingInt` with the message "Char 0x%X out of allowed range".
    fn err_encoding_int(&mut self, code: Code, val: u32) {
        self.fatal(code, format!("Char 0x{val:X} out of allowed range\n").into_bytes());
    }

    /// `xmlErrMsgStr`.
    pub(super) fn err_msg_str(&mut self, code: Code, msg: Vec<u8>) {
        if self.stopped() {
            return;
        }
        self.err_no = code;
        self.raise(Domain::Parser, code, Level::Error, msg);
    }

    /// `xmlWarningMsg`.
    pub(super) fn warning_msg(&mut self, code: Code, msg: Vec<u8>) {
        if self.stopped() {
            return;
        }
        self.raise(Domain::Parser, code, Level::Warning, msg);
    }

    /// `xmlValidityError`.
    pub(super) fn validity_error(&mut self) {
        if self.stopped() {
            return;
        }
        self.err_no = Code::Valid;
        self.raise(Domain::Dtd, Code::Valid, Level::Error, Vec::new());
    }

    /// `xmlNsErr`.
    pub(super) fn ns_err(&mut self) {
        if self.stopped() {
            return;
        }
        self.err_no = Code::Ns;
        self.raise(Domain::Namespace, Code::Ns, Level::Error, Vec::new());
    }

    /// `xmlErrMemory`.
    pub(super) fn err_memory(&mut self, extra: Option<&[u8]>) {
        if self.stopped() {
            return;
        }
        self.err_no = Code::NoMemory;
        self.instate = State::Eof;
        self.disable_sax = true;
        let msg = match extra {
            Some(e) => cat(&[b"Memory allocation failed : ", e, b"\n"]),
            None => b"Memory allocation failed\n".to_vec(),
        };
        self.raise(Domain::Parser, Code::NoMemory, Level::Fatal, msg);
    }

    /// `xmlSAX2ErrMemory`.
    pub(super) fn sax2_err_memory(&mut self, msg: &[u8]) {
        self.err_no = Code::NoMemory;
        self.raise(Domain::Parser, Code::NoMemory, Level::Error, msg.to_vec());
        self.instate = State::Eof;
        self.disable_sax = true;
    }

    // The node stack and the text nodes of SAX2.

    /// `nodePush`.
    pub(super) fn node_push(&mut self, node: Node) -> i32 {
        if self.nodes.len() > MAX_DEPTH {
            self.fatal_msg(
                Code::InternalError,
                format!("Excessive depth in document: {MAX_DEPTH} use XML_PARSE_HUGE option\n")
                    .into_bytes(),
            );
            self.halt();
            return -1;
        }
        self.nodes.push(node);
        (self.nodes.len() - 1) as i32
    }

    /// `nodePop`.
    pub(super) fn node_pop(&mut self) {
        self.nodes.pop();
    }

    /// `xmlSAX2Text`.
    pub(super) fn sax2_text(&mut self, len: usize, cdata: bool) {
        let Some(node) = self.nodes.last() else {
            return;
        };
        let last = node.last;
        if last == Kind::None {
            let kind = if cdata { Kind::Cdata(len) } else { Kind::Text(len) };
            if let Some(n) = self.nodes.last_mut() {
                n.add(kind);
            }
            self.nodelen = len as i64;
            self.nodemem = len as i64 + 1;
            return;
        }
        let coalesce = matches!((last, cdata), (Kind::Text(_), false) | (Kind::Cdata(_), true));
        if coalesce && self.nodemem != 0 {
            if self.nodelen as u64 + len as u64 > MAX_TEXT_LENGTH as u64 {
                self.sax2_err_memory(b"xmlSAX2Characters: huge text node");
                return;
            }
            self.nodelen += len as i64;
            let node = self.nodes.last_mut().expect("a node");
            node.last = match last {
                Kind::Text(n) => Kind::Text(n + len),
                Kind::Cdata(n) => Kind::Cdata(n + len),
                k => k,
            };
            if node.count == 1 {
                node.first = node.last;
            }
        } else if coalesce {
            let node = self.nodes.last_mut().expect("a node");
            let total = match last {
                Kind::Text(n) => {
                    node.last = Kind::Text(n + len);
                    n + len
                }
                Kind::Cdata(n) => {
                    node.last = Kind::Cdata(n + len);
                    n + len
                }
                _ => len,
            };
            if node.count == 1 {
                node.first = node.last;
            }
            self.nodelen = total as i64;
            self.nodemem = total as i64 + 1;
        } else {
            let node = self.nodes.last_mut().expect("a node");
            if cdata {
                node.add(Kind::Cdata(len));
            } else {
                node.add_text(len);
            }
            self.nodelen = len as i64;
            self.nodemem = len as i64 + 1;
        }
    }

    /// Adds a comment, a PI or another node that is not text to the current node.
    pub(super) fn add_other(&mut self) {
        if let Some(node) = self.nodes.last_mut() {
            node.add(Kind::Other);
        }
    }

    // The names.

    /// `xmlParseName`.
    pub(super) fn parse_name(&mut self) -> Option<Vec<u8>> {
        let input = self.input();
        let start = input.cur;
        let mut i = start;
        let c = input.at(i);
        if c.is_ascii_alphabetic() || c == b'_' || c == b':' {
            i += 1;
            while {
                let c = input.at(i);
                c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b':' || c == b'.'
            } {
                i += 1;
            }
            let c = input.at(i);
            if c > 0 && c < 0x80 {
                let count = i - start;
                if count > MAX_NAME_LENGTH {
                    self.fatal_err(Code::NameTooLong, Some(b"Name"));
                    return None;
                }
                let name = self.slice(start, i);
                self.input_mut().cur = i;
                return Some(name);
            }
        }
        self.parse_name_complex()
    }

    /// `xmlParseNameComplex`.
    fn parse_name_complex(&mut self) -> Option<Vec<u8>> {
        if self.instate == State::Eof {
            return None;
        }
        let mut l = 0;
        let mut len = 0;
        let mut c = self.cur_char(&mut l);
        if !is_name_start_char(c) {
            return None;
        }
        len += l;
        self.nextl(l);
        c = self.cur_char(&mut l);
        while is_name_char(c) {
            len += l;
            self.nextl(l);
            c = self.cur_char(&mut l);
        }
        if len > MAX_NAME_LENGTH {
            self.fatal_err(Code::NameTooLong, Some(b"Name"));
            return None;
        }
        let input = self.input();
        let cur = input.cur;
        if cur - input.base < len {
            self.fatal_err(Code::InternalError, Some(b"unexpected change of input buffer"));
            return None;
        }
        if input.at(cur) == b'\n' && cur > 0 && input.at(cur - 1) == b'\r' {
            return Some(self.slice(cur - (len + 1), cur - 1));
        }
        Some(self.slice(cur - len, cur))
    }

    /// `xmlParseNCNameComplex`.
    fn parse_ncname_complex(&mut self) -> Option<Vec<u8>> {
        let start = self.pos();
        let mut l = 0;
        let mut len = 0;
        let mut count = 0;
        let mut c = self.cur_char(&mut l);
        if !is_name_start_char(c) || c == u32::from(b':') {
            return None;
        }
        while is_name_char(c) && c != u32::from(b':') {
            if count > CHUNK_SIZE {
                if len > MAX_NAME_LENGTH {
                    self.fatal_err(Code::NameTooLong, Some(b"NCName"));
                    return None;
                }
                count = 0;
                if self.instate == State::Eof {
                    return None;
                }
            } else {
                count += 1;
            }
            len += l;
            self.nextl(l);
            c = self.cur_char(&mut l);
            if c == 0 {
                count = 0;
                if self.instate == State::Eof {
                    return None;
                }
                c = self.cur_char(&mut l);
            }
        }
        if len > MAX_NAME_LENGTH {
            self.fatal_err(Code::NameTooLong, Some(b"NCName"));
            return None;
        }
        Some(self.slice(start, start + len))
    }

    /// `xmlParseNCName`.
    pub(super) fn parse_ncname(&mut self) -> Option<Vec<u8>> {
        let input = self.input();
        let start = input.cur;
        let e = input.end;
        let mut i = start;
        let c = input.at(i);
        if (c.is_ascii_alphabetic() || c == b'_') && i < e {
            i += 1;
            while {
                let c = input.at(i);
                (c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.') && i < e
            } {
                i += 1;
            }
            if i < e {
                let c = input.at(i);
                if c > 0 && c < 0x80 {
                    let count = i - start;
                    if count > MAX_NAME_LENGTH {
                        self.fatal_err(Code::NameTooLong, Some(b"NCName"));
                        return None;
                    }
                    let name = self.slice(start, i);
                    self.input_mut().cur = i;
                    return Some(name);
                }
            }
        }
        self.parse_ncname_complex()
    }

    /// `xmlParseNameAndCompare`. `Ok(())` is the match, `Err(name)` the name that does not match.
    pub(super) fn parse_name_and_compare(&mut self, other: &[u8]) -> Result<(), Option<Vec<u8>>> {
        if self.instate == State::Eof {
            return Err(None);
        }
        let input = self.input();
        let mut i = input.cur;
        let mut k = 0;
        while input.at(i) != 0 && k < other.len() && input.at(i) == other[k] {
            i += 1;
            k += 1;
        }
        if k == other.len() && (input.at(i) == b'>' || is_blank(u32::from(input.at(i)))) {
            self.input_mut().cur = i;
            return Ok(());
        }
        match self.parse_name() {
            Some(name) if name == other => Ok(()),
            name => Err(name),
        }
    }

    /// `xmlParseStringName`. It returns the name and the position after it.
    pub(super) fn parse_string_name(&mut self, s: &[u8], mut i: usize) -> Option<(Vec<u8>, usize)> {
        let mut l = 0;
        let mut c = self.string_current_char(s, i, &mut l);
        if !is_name_start_char(c) {
            return None;
        }
        let mut buf = Vec::new();
        copy_buf(&mut buf, c);
        i += l;
        c = self.string_current_char(s, i, &mut l);
        while is_name_char(c) {
            copy_buf(&mut buf, c);
            i += l;
            c = self.string_current_char(s, i, &mut l);
            if buf.len() >= MAX_NAMELEN {
                let mut max = buf.len() * 2;
                while is_name_char(c) {
                    if buf.len() + 10 > max {
                        if buf.len() > MAX_NAME_LENGTH {
                            self.fatal_err(Code::NameTooLong, Some(b"NCName"));
                            return None;
                        }
                        max *= 2;
                    }
                    copy_buf(&mut buf, c);
                    i += l;
                    c = self.string_current_char(s, i, &mut l);
                }
                return Some((buf, i));
            }
        }
        if buf.len() > MAX_NAME_LENGTH {
            self.fatal_err(Code::NameTooLong, Some(b"NCName"));
            return None;
        }
        Some((buf, i))
    }

    /// `xmlParseNmtoken`.
    pub(super) fn parse_nmtoken(&mut self) -> Option<Vec<u8>> {
        if self.instate == State::Eof {
            return None;
        }
        let mut l = 0;
        let mut buf = Vec::new();
        let mut c = self.cur_char(&mut l);
        while is_name_char(c) {
            copy_buf(&mut buf, c);
            self.nextl(l);
            c = self.cur_char(&mut l);
            if c == 0 {
                if self.instate == State::Eof {
                    return None;
                }
                c = self.cur_char(&mut l);
            }
            if buf.len() >= MAX_NAMELEN {
                let mut max = buf.len() * 2;
                while is_name_char(c) {
                    if self.instate == State::Eof {
                        return None;
                    }
                    if buf.len() + 10 > max {
                        if max > MAX_NAME_LENGTH {
                            self.fatal_err(Code::NameTooLong, Some(b"NmToken"));
                            return None;
                        }
                        max *= 2;
                    }
                    copy_buf(&mut buf, c);
                    self.nextl(l);
                    c = self.cur_char(&mut l);
                }
                return Some(buf);
            }
        }
        if buf.is_empty() {
            return None;
        }
        if buf.len() > MAX_NAME_LENGTH {
            self.fatal_err(Code::NameTooLong, Some(b"NmToken"));
            return None;
        }
        Some(buf)
    }
}

/// Decodes the UTF-8 sequence at `i` of an input.
fn decode(input: &Input, i: usize) -> (u32, usize) {
    decode_bytes(|k| input.at(i + k))
}

/// Decodes one UTF-8 sequence. The input is valid UTF-8.
fn decode_bytes(b: impl Fn(usize) -> u8) -> (u32, usize) {
    let c = b(0);
    if c & 0xE0 == 0xE0 {
        if c & 0xF0 == 0xF0 {
            let val = (u32::from(c) & 0x7) << 18
                | (u32::from(b(1)) & 0x3F) << 12
                | (u32::from(b(2)) & 0x3F) << 6
                | (u32::from(b(3)) & 0x3F);
            (val, 4)
        } else {
            let val = (u32::from(c) & 0xF) << 12
                | (u32::from(b(1)) & 0x3F) << 6
                | (u32::from(b(2)) & 0x3F);
            (val, 3)
        }
    } else {
        let val = (u32::from(c) & 0x1F) << 6 | (u32::from(b(1)) & 0x3F);
        (val, 2)
    }
}

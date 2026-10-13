//! The XML declaration, the document and the balanced chunks, from `xmlParseXMLDecl`, `xmlParseDocument`, `xmlParseBalancedChunkMemoryRecover`, `xmlParseBalancedChunkMemoryInternal` and `xmlParseExternalEntityPrivate` of libxml2 2.9.14.

use std::cell::RefCell;
use std::rc::Rc;

use super::ctxt::{
    Code, Ctxt, Node, PARSE_DTDATTR, PARSE_NODICT, PARSE_NOENT, Report, SKIP_IDS, State, cat, cstr,
    is_blank,
};
use super::dtd::Doc;
use super::encodings;

/// `xmlCtxtReadDoc` with the options of PostgreSQL for a document: `XML_PARSE_NOENT | XML_PARSE_DTDATTR` and the encoding `UTF-8`. It returns true if the document is well formed.
pub(super) fn read_doc(text: &[u8], report: &Rc<RefCell<Report>>) -> bool {
    let mut ctxt = Ctxt::new(report.clone(), Rc::default());
    let input = ctxt.new_input(Rc::from(text), text.len());
    ctxt.inputs.push(input);
    ctxt.use_options(PARSE_NOENT | PARSE_DTDATTR);
    ctxt.encoding = Some(b"UTF-8".to_vec());
    // `xmlSwitchToEncoding` with the handler of UTF-8 has no effect on a string input, but it fails on an empty one.
    if text.is_empty() {
        ctxt.err_internal(b"switching encoding : no input\n".to_vec());
    }
    ctxt.parse_document();
    ctxt.well_formed
}

/// `xmlParseBalancedChunkMemory` on a new document with no dictionary, as PostgreSQL calls it for content. It returns 0 if the chunk is well balanced.
pub(super) fn balanced_chunk(text: &[u8], report: &Rc<RefCell<Report>>) -> i32 {
    let text = cstr(text);
    if text.is_empty() {
        return -1;
    }
    let mut ctxt = Ctxt::new(report.clone(), Rc::default());
    let mut input = ctxt.new_input(Rc::from(text), text.len());
    input.shrinkable = true;
    ctxt.inputs.push(input);
    ctxt.use_options(PARSE_NODICT);
    ctxt.my_doc = Some(Rc::new(RefCell::new(Doc::new())));
    ctxt.node_push(Node::new());
    ctxt.instate = State::Content;
    ctxt.input_id = 2;
    ctxt.depth = 0;
    ctxt.validate = false;
    ctxt.loadsubset = 0;
    ctxt.parse_content();
    ctxt.chunk_end();
    if ctxt.well_formed {
        0
    } else if ctxt.err_no == Code::Ok {
        1
    } else {
        // Only the zero test of the caller reads the code.
        2
    }
}

impl Ctxt {
    // The XML declaration.

    /// `xmlParseVersionNum`.
    fn parse_version_num(&mut self) -> Option<Vec<u8>> {
        let c = self.cur();
        if !c.is_ascii_digit() {
            return None;
        }
        let mut buf = vec![c];
        self.next();
        if self.cur() != b'.' {
            return None;
        }
        buf.push(b'.');
        self.next();
        while self.cur().is_ascii_digit() {
            buf.push(self.cur());
            self.next();
        }
        Some(buf)
    }

    /// `xmlParseVersionInfo`.
    fn parse_version_info(&mut self) -> Option<Vec<u8>> {
        if !self.looking_at(b"version") {
            return None;
        }
        self.skip(7);
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal_err(Code::EqualRequired, None);
            return None;
        }
        self.next();
        self.skip_blanks();
        let quote = self.cur();
        if quote != b'"' && quote != b'\'' {
            self.fatal_err(Code::StringNotStarted, None);
            return None;
        }
        self.next();
        let version = self.parse_version_num();
        if self.cur() == quote {
            self.next();
        } else {
            self.fatal_err(Code::StringNotClosed, None);
        }
        version
    }

    /// `xmlParseEncName`.
    fn parse_enc_name(&mut self) -> Option<Vec<u8>> {
        let c = self.cur();
        if !c.is_ascii_alphabetic() {
            self.fatal_err(Code::EncodingName, None);
            return None;
        }
        let mut buf = vec![c];
        self.next();
        loop {
            let c = self.cur();
            if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-') {
                break;
            }
            buf.push(c);
            self.next();
        }
        Some(buf)
    }

    /// `xmlParseEncodingDecl`. A string input does not change when the parser switches its encoding, so only the lookup of the handler has an effect.
    fn parse_encoding_decl(&mut self) {
        self.skip_blanks();
        if !self.looking_at(b"encoding") {
            return;
        }
        self.skip(8);
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal_err(Code::EqualRequired, None);
            return;
        }
        self.next();
        self.skip_blanks();
        let quote = self.cur();
        let encoding = if quote == b'"' || quote == b'\'' {
            self.next();
            let encoding = self.parse_enc_name();
            if self.cur() != quote {
                self.fatal_err(Code::StringNotClosed, None);
                return;
            }
            self.next();
            encoding
        } else {
            self.fatal_err(Code::StringNotStarted, None);
            None
        };
        let Some(encoding) = encoding else {
            return;
        };
        let is = |name: &[u8]| encoding.eq_ignore_ascii_case(name);
        if is(b"UTF-16") || is(b"UTF16") || is(b"UTF-8") || is(b"UTF8") {
            // The input is a string, so it has no buffer and no error for UTF-16 with UTF-8 content.
            self.encoding = Some(encoding);
            return;
        }
        self.input_mut().encoding = true;
        if !encodings::supported(&encoding) {
            self.fatal_msg(
                Code::UnsupportedEncoding,
                cat(&[b"Unsupported encoding ", &encoding, b"\n"]),
            );
        }
    }

    /// `xmlParseSDDecl`.
    fn parse_sd_decl(&mut self) -> i32 {
        let mut standalone = -2;
        self.skip_blanks();
        if !self.looking_at(b"standalone") {
            return standalone;
        }
        self.skip(10);
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal_err(Code::EqualRequired, None);
            return standalone;
        }
        self.next();
        self.skip_blanks();
        let quote = self.cur();
        if quote != b'\'' && quote != b'"' {
            self.fatal_err(Code::StringNotStarted, None);
            return standalone;
        }
        self.next();
        if self.cur() == b'n' && self.nxt(1) == b'o' {
            standalone = 0;
            self.skip(2);
        } else if self.cur() == b'y' && self.nxt(1) == b'e' && self.nxt(2) == b's' {
            standalone = 1;
            self.skip(3);
        } else {
            self.fatal_err(Code::StandaloneValue, None);
        }
        if self.cur() == quote {
            self.next();
        } else {
            self.fatal_err(Code::StringNotClosed, None);
        }
        standalone
    }

    /// The test of `IS_BLANK_CH(RAW)`.
    fn blank_next(&self) -> bool {
        is_blank(u32::from(self.cur()))
    }

    /// `xmlParseXMLDecl`.
    fn parse_xml_decl(&mut self) {
        self.input_mut().standalone = -2;
        self.skip(5);
        if !self.blank_next() {
            self.fatal_msg(Code::SpaceRequired, b"Blank needed after '<?xml'\n".to_vec());
        }
        self.skip_blanks();
        match self.parse_version_info() {
            None => self.fatal_err(Code::VersionMissing, None),
            Some(version) => {
                if version != b"1.0" {
                    let msg = cat(&[b"Unsupported version '", &version, b"'\n"]);
                    if version.starts_with(b"1.") {
                        self.warning_msg(Code::WarUnknownVersion, msg);
                    } else {
                        self.fatal_msg(Code::UnknownVersion, msg);
                    }
                }
                self.version = Some(version);
            }
        }
        if !self.blank_next() {
            if self.looking_at(b"?>") {
                self.skip(2);
                return;
            }
            self.fatal_msg(Code::SpaceRequired, b"Blank needed here\n".to_vec());
        }
        self.parse_encoding_decl();
        if self.err_no == Code::UnsupportedEncoding || self.instate == State::Eof {
            return;
        }
        if self.input().encoding && !self.blank_next() {
            if self.looking_at(b"?>") {
                self.skip(2);
                return;
            }
            self.fatal_msg(Code::SpaceRequired, b"Blank needed here\n".to_vec());
        }
        self.skip_blanks();
        let standalone = self.parse_sd_decl();
        self.input_mut().standalone = standalone;
        self.skip_blanks();
        if self.looking_at(b"?>") {
            self.skip(2);
        } else if self.cur() == b'>' {
            self.fatal_err(Code::XmldeclNotFinished, None);
            self.next();
        } else {
            self.fatal_err(Code::XmldeclNotFinished, None);
            // `MOVETO_ENDTAG`.
            while self.cur() != 0 && self.cur() != b'>' {
                self.skip(1);
            }
            self.next();
        }
    }

    // The document.

    /// `xmlParseMisc`.
    fn parse_misc(&mut self) {
        while self.instate != State::Eof {
            self.skip_blanks();
            if self.cur() == b'<' && self.nxt(1) == b'?' {
                self.parse_pi();
            } else if self.looking_at(b"<!--") {
                self.parse_comment();
            } else {
                break;
            }
        }
    }

    /// `xmlSAX2StartDocument`.
    fn sax_start_document(&mut self) {
        let mut doc = Doc::new();
        doc.standalone = self.standalone;
        self.my_doc = Some(Rc::new(RefCell::new(doc)));
    }

    /// `xmlParseDocument`. The caller sets the encoding, so the parser does not detect it.
    fn parse_document(&mut self) {
        if self.cur() == 0 {
            self.fatal_err(Code::DocumentEmpty, None);
            return;
        }
        if self.looking_at(b"<?xml") && is_blank(u32::from(self.nxt(5))) {
            self.parse_xml_decl();
            if self.err_no == Code::UnsupportedEncoding || self.instate == State::Eof {
                return;
            }
            self.standalone = self.input().standalone;
            self.skip_blanks();
        } else {
            self.version = Some(b"1.0".to_vec());
        }
        if !self.disable_sax {
            self.sax_start_document();
        }
        if self.instate == State::Eof {
            return;
        }
        self.parse_misc();
        if self.looking_at(b"<!DOCTYPE") {
            self.in_subset = 1;
            let (uri, external_id) = self.parse_doctype_decl();
            if self.cur() == b'[' {
                self.instate = State::Dtd;
                self.parse_internal_subset();
                if self.instate == State::Eof {
                    return;
                }
            }
            self.in_subset = 2;
            if !self.disable_sax {
                self.sax_external_subset(external_id.as_deref(), uri.as_deref());
            }
            if self.instate == State::Eof {
                return;
            }
            self.in_subset = 0;
            self.clean_special_attr();
            self.instate = State::Prolog;
            self.parse_misc();
        }
        if self.cur() != b'<' {
            self.fatal_msg(Code::DocumentEmpty, b"Start tag expected, '<' not found\n".to_vec());
        } else {
            self.instate = State::Content;
            self.parse_element();
            self.instate = State::Epilog;
            self.parse_misc();
            if self.cur() != 0 {
                self.fatal_err(Code::DocumentEnd, None);
            }
            self.instate = State::Eof;
        }
    }

    // The balanced chunks.

    /// The checks after `xmlParseContent` in each balanced chunk parser. The node stack holds only the pseudo root when the chunk is balanced.
    fn chunk_end(&mut self) {
        if self.cur() == b'<' && self.nxt(1) == b'/' {
            self.fatal_err(Code::NotWellBalanced, None);
        } else if self.cur() != 0 {
            self.fatal_err(Code::ExtraContent, None);
        }
        if self.nodes.len() != 1 {
            self.fatal_err(Code::NotWellBalanced, None);
        }
    }

    /// The result code of a sub parser.
    fn chunk_result(&self) -> Code {
        if self.well_formed {
            Code::Ok
        } else if self.err_no == Code::Ok {
            Code::InternalError
        } else {
            self.err_no
        }
    }

    /// A parser for an entity, with the state that each sub parser shares with its parent.
    fn sub_ctxt(&self) -> Ctxt {
        let mut sub = Ctxt::new(self.report.clone(), self.atts_special.clone());
        sub.options = self.options;
        sub
    }

    /// The end of each sub parser: the parent counts the entities of the sub parser and keeps its last error.
    fn merge(&mut self, sub: &Ctxt) {
        self.nbentities += sub.nbentities;
        if sub.last_error != Code::Ok {
            self.last_error = sub.last_error;
        }
    }

    /// `xmlParseBalancedChunkMemoryInternal`, for the content of an internal entity. The list is the pseudo root node when it has children.
    pub(super) fn parse_balanced_chunk_memory_internal(
        &mut self,
        string: Option<&[u8]>,
    ) -> (Code, Option<Node>) {
        if self.depth > 40 {
            return (Code::EntityLoop, None);
        }
        let Some(string) = string else {
            return (Code::InternalError, None);
        };
        let string = cstr(string);
        if string.is_empty() {
            // `xmlCreateMemoryParserCtxt` refuses an empty buffer.
            return (Code::WarUndeclaredEntity, None);
        }
        let mut sub = self.sub_ctxt();
        let mut input = sub.new_input(Rc::from(string), string.len());
        input.shrinkable = true;
        sub.inputs.push(input);
        sub.input_id = self.input_id + 1;
        sub.ns_tab = self.ns_tab.clone();
        sub.replace_entities = self.replace_entities;
        sub.my_doc = Some(self.my_doc.clone().unwrap_or_else(|| Rc::new(RefCell::new(Doc::new()))));
        sub.node_push(Node::new());
        sub.instate = State::Content;
        sub.depth = self.depth + 1;
        sub.validate = false;
        sub.loadsubset = self.loadsubset;
        if self.validate || self.replace_entities {
            sub.loadsubset |= SKIP_IDS;
        }
        sub.parse_content();
        sub.chunk_end();
        let ret = sub.chunk_result();
        let list =
            if ret == Code::Ok { sub.nodes.first().copied().filter(|n| n.count > 0) } else { None };
        self.merge(&sub);
        (ret, list)
    }

    /// `xmlParseExternalEntityPrivate`. The PostgreSQL entity loader gives an empty input for each external entity, so the content is empty.
    pub(super) fn parse_external_entity_private(
        &mut self,
        uri: Option<&[u8]>,
        id: Option<&[u8]>,
    ) -> (Code, Option<Node>) {
        if self.depth > 40 {
            return (Code::EntityLoop, None);
        }
        if uri.is_none() && id.is_none() {
            return (Code::InternalError, None);
        }
        if self.my_doc.is_none() {
            return (Code::InternalError, None);
        }
        let mut sub = self.sub_ctxt();
        sub.input_id = self.input_id + 1;
        let input = sub.new_input(Rc::from(&b""[..]), 0);
        sub.inputs.push(input);
        sub.node_push(Node::new());
        sub.my_doc = self.my_doc.clone();
        sub.instate = State::Content;
        sub.depth = self.depth;
        sub.loadsubset = self.loadsubset;
        sub.validate = self.validate;
        sub.replace_entities = self.replace_entities;
        sub.external = self.external;
        sub.parse_content();
        sub.chunk_end();
        let ret = sub.chunk_result();
        let list =
            if ret == Code::Ok { sub.nodes.first().copied().filter(|n| n.count > 0) } else { None };
        self.merge(&sub);
        (ret, list)
    }
}

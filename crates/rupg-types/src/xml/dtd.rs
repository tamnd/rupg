//! The DTD part of the XML parser, from `parser.c`, `SAX2.c` and `entities.c` of libxml2 2.9.14.
//!
//! The declarations are parsed for their errors. Only the entities and the attribute types are kept, because they change how the content parses. The element and notation declarations are checked and dropped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::content::SUBSTITUTE_PEREF;
use super::ctxt::{
    BUFFER_SIZE, Code, Ctxt, MAX_NAME_LENGTH, Node, State, at, cat, copy_buf, cstr, is_char,
    is_pubid_char,
};
use super::uri;

/// `xmlEntityType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntityType {
    InternalGeneral,
    ExternalGeneralParsed,
    ExternalGeneralUnparsed,
    InternalParameter,
    ExternalParameter,
    InternalPredefined,
}

/// `xmlEntity`, for the fields that the parser reads.
pub(super) struct Entity {
    pub(super) name: Vec<u8>,
    pub(super) etype: EntityType,
    pub(super) content: Option<Vec<u8>>,
    pub(super) length: usize,
    pub(super) uri: Option<Vec<u8>>,
    pub(super) external_id: Option<Vec<u8>>,
    pub(super) checked: i32,
    pub(super) owner: i32,
    pub(super) children: Option<Node>,
}

/// The entity tables of `xmlDtd`.
#[derive(Default)]
pub(super) struct Dtd {
    pub(super) entities: HashMap<Vec<u8>, Rc<RefCell<Entity>>>,
    pub(super) pentities: HashMap<Vec<u8>, Rc<RefCell<Entity>>>,
}

/// `xmlDoc`, for the fields that the parser reads.
pub(super) struct Doc {
    /// The document is the `SAX compatibility mode document` that the entity declarations make when SAX is off.
    pub(super) compat: bool,
    pub(super) standalone: i32,
    pub(super) int_subset: Option<Dtd>,
    pub(super) ext_subset: Option<Dtd>,
}

impl Doc {
    /// `xmlNewDoc`.
    pub(super) fn new() -> Doc {
        Doc { compat: false, standalone: -1, int_subset: None, ext_subset: None }
    }

    /// `xmlGetDocEntity`.
    fn entity(&self, name: &[u8]) -> Option<Rc<RefCell<Entity>>> {
        if let Some(e) = self.int_subset.as_ref().and_then(|d| d.entities.get(name)) {
            return Some(e.clone());
        }
        if self.standalone != 1
            && let Some(e) = self.ext_subset.as_ref().and_then(|d| d.entities.get(name))
        {
            return Some(e.clone());
        }
        predefined_entity(name)
    }

    /// `xmlGetParameterEntity`.
    fn parameter_entity(&self, name: &[u8]) -> Option<Rc<RefCell<Entity>>> {
        if let Some(e) = self.int_subset.as_ref().and_then(|d| d.pentities.get(name)) {
            return Some(e.clone());
        }
        self.ext_subset.as_ref().and_then(|d| d.pentities.get(name)).cloned()
    }
}

/// `xmlGetPredefinedEntity`.
pub(super) fn predefined_entity(name: &[u8]) -> Option<Rc<RefCell<Entity>>> {
    let c = match name {
        b"lt" => b'<',
        b"gt" => b'>',
        b"amp" => b'&',
        b"apos" => b'\'',
        b"quot" => b'"',
        _ => return None,
    };
    Some(Rc::new(RefCell::new(Entity {
        name: name.to_vec(),
        etype: EntityType::InternalPredefined,
        content: Some(vec![c]),
        length: 1,
        uri: None,
        external_id: None,
        checked: 1,
        owner: 0,
        children: None,
    })))
}

/// `ent->content[0] = 0`, which libxml2 does to an entity that failed to parse.
pub(super) fn zero_content(ent: &Rc<RefCell<Entity>>) {
    if let Some(c) = ent.borrow_mut().content.as_mut()
        && !c.is_empty()
    {
        c[0] = 0;
    }
}

/// The check of `xmlAddEntity` on a redeclaration of a predefined entity.
fn valid_predefined(etype: EntityType, content: Option<&[u8]>, c: u8) -> bool {
    let Some(content) = content else {
        return false;
    };
    if etype != EntityType::InternalGeneral {
        return false;
    }
    if at(content, 0) == c && at(content, 1) == 0 && matches!(c, b'>' | b'\'' | b'"') {
        return true;
    }
    if at(content, 0) == b'&' && at(content, 1) == b'#' {
        let rest = cstr(&content[2.min(content.len())..]);
        if at(content, 2) == b'x' {
            const HEX: &[u8] = b"0123456789ABCDEF";
            let r = [HEX[usize::from(c / 16 % 16)], HEX[usize::from(c % 16)], b';'];
            return rest[1..].eq_ignore_ascii_case(&r);
        }
        let r = [b'0' + c / 10 % 10, b'0' + c % 10, b';'];
        return rest == r;
    }
    false
}

impl Dtd {
    /// `xmlAddEntity`. A redeclaration returns `None` and keeps the first.
    fn add_entity(
        &mut self,
        name: &[u8],
        etype: EntityType,
        external_id: Option<&[u8]>,
        content: Option<&[u8]>,
    ) -> Option<Rc<RefCell<Entity>>> {
        let table = match etype {
            EntityType::InternalGeneral
            | EntityType::ExternalGeneralParsed
            | EntityType::ExternalGeneralUnparsed => {
                if let Some(p) = predefined_entity(name) {
                    let c = p.borrow().content.as_ref().map_or(0, |c| c[0]);
                    if !valid_predefined(etype, content, c) {
                        return None;
                    }
                }
                &mut self.entities
            }
            EntityType::InternalParameter | EntityType::ExternalParameter => &mut self.pentities,
            EntityType::InternalPredefined => return None,
        };
        if table.contains_key(name) {
            return None;
        }
        let content = content.map(|c| cstr(c).to_vec());
        let ent = Rc::new(RefCell::new(Entity {
            name: name.to_vec(),
            etype,
            length: content.as_ref().map_or(0, Vec::len),
            content,
            uri: None,
            external_id: external_id.map(<[u8]>::to_vec),
            checked: 0,
            owner: 0,
            children: None,
        }));
        table.insert(name.to_vec(), ent.clone());
        Some(ent)
    }
}

/// `XML_ATTRIBUTE_CDATA`, the attribute type that does not normalize.
const ATTRIBUTE_CDATA: i32 = 1;
const ATTRIBUTE_ID: i32 = 2;
const ATTRIBUTE_IDREF: i32 = 3;
const ATTRIBUTE_IDREFS: i32 = 4;
const ATTRIBUTE_ENTITY: i32 = 5;
const ATTRIBUTE_ENTITIES: i32 = 6;
const ATTRIBUTE_NMTOKEN: i32 = 7;
const ATTRIBUTE_NMTOKENS: i32 = 8;
const ATTRIBUTE_ENUMERATION: i32 = 9;
const ATTRIBUTE_NOTATION: i32 = 10;

const ATTRIBUTE_NONE: i32 = 1;
const ATTRIBUTE_REQUIRED: i32 = 2;
const ATTRIBUTE_IMPLIED: i32 = 3;
const ATTRIBUTE_FIXED: i32 = 4;

const ELEMENT_TYPE_EMPTY: i32 = 1;
const ELEMENT_TYPE_ANY: i32 = 2;
const ELEMENT_TYPE_MIXED: i32 = 3;
const ELEMENT_TYPE_ELEMENT: i32 = 4;

const BOUNDARY_ELEMENT_CONTENT: &[u8] =
    b"Element content declaration doesn't start and stop in the same entity\n";

impl Ctxt {
    // The SAX2 handlers of the DTD.

    /// `xmlSAX2GetEntity`. The standalone branch of libxml2 finds the same entity here, because the external subset is always empty.
    pub(super) fn sax_get_entity(&mut self, name: &[u8]) -> Option<Rc<RefCell<Entity>>> {
        if self.in_subset == 0
            && let Some(e) = predefined_entity(name)
        {
            return Some(e);
        }
        match &self.my_doc {
            Some(doc) => doc.borrow().entity(name),
            None => predefined_entity(name),
        }
    }

    /// `xmlSAX2GetParameterEntity`.
    pub(super) fn sax_get_parameter_entity(&mut self, name: &[u8]) -> Option<Rc<RefCell<Entity>>> {
        self.my_doc.as_ref()?.borrow().parameter_entity(name)
    }

    /// `xmlAddDocEntity` and the URI of `xmlSAX2EntityDecl`.
    fn add_doc_entity(
        &mut self,
        name: &[u8],
        etype: EntityType,
        public_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
        content: Option<&[u8]>,
    ) {
        let in_ext = self.in_subset == 2;
        let ent = {
            let Some(doc) = &self.my_doc else {
                return;
            };
            let mut doc = doc.borrow_mut();
            let dtd = if in_ext { doc.ext_subset.as_mut() } else { doc.int_subset.as_mut() };
            let Some(dtd) = dtd else {
                return;
            };
            dtd.add_entity(name, etype, public_id, content)
        };
        if let (Some(ent), Some(sys)) = (ent, system_id) {
            let mut e = ent.borrow_mut();
            if e.uri.is_none() {
                e.uri = uri::build(sys);
            }
        }
    }

    /// `xmlSAX2EntityDecl`.
    fn sax_entity_decl(
        &mut self,
        name: &[u8],
        etype: EntityType,
        public_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
        content: Option<&[u8]>,
    ) {
        if self.in_subset == 1 || self.in_subset == 2 {
            self.add_doc_entity(name, etype, public_id, system_id, content);
        } else {
            self.fatal_msg(
                Code::EntityProcessing,
                cat(&[b"SAX.xmlSAX2EntityDecl(", name, b") called while not in subset\n"]),
            );
        }
    }

    /// `xmlSAX2UnparsedEntityDecl`.
    fn sax_unparsed_entity_decl(
        &mut self,
        name: &[u8],
        public_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
        notation: Option<&[u8]>,
    ) {
        if self.in_subset == 1 || self.in_subset == 2 {
            self.add_doc_entity(
                name,
                EntityType::ExternalGeneralUnparsed,
                public_id,
                system_id,
                notation,
            );
        } else {
            self.fatal_msg(
                Code::InternalError,
                cat(&[b"SAX.xmlSAX2UnparsedEntityDecl(", name, b") called while not in subset\n"]),
            );
        }
    }

    /// `xmlSAX2NotationDecl`. The notation table is not kept.
    fn sax_notation_decl(
        &mut self,
        name: &[u8],
        public_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
    ) {
        if self.my_doc.is_none() {
            return;
        }
        if public_id.is_none() && system_id.is_none() {
            self.fatal_msg(
                Code::NotationProcessing,
                cat(&[b"SAX.xmlSAX2NotationDecl(", name, b") externalID or PublicID missing\n"]),
            );
        } else if self.in_subset != 1 && self.in_subset != 2 {
            self.fatal_msg(
                Code::NotationProcessing,
                cat(&[b"SAX.xmlSAX2NotationDecl(", name, b") called while not in subset\n"]),
            );
        }
    }

    /// `xmlSAX2ElementDecl`. The element table only raises validity errors, which PostgreSQL ignores.
    fn sax_element_decl(&mut self, name: &[u8]) {
        if self.my_doc.is_none() {
            return;
        }
        if self.in_subset != 1 && self.in_subset != 2 {
            self.fatal_msg(
                Code::InternalError,
                cat(&[b"SAX.xmlSAX2ElementDecl(", name, b") called while not in subset\n"]),
            );
        }
    }

    /// `xmlSAX2AttributeDecl`.
    fn sax_attribute_decl(&mut self, fullname: &[u8], typ: i32) {
        if self.my_doc.is_none() {
            return;
        }
        if fullname == b"xml:id" && typ != ATTRIBUTE_ID {
            self.validity_error();
        }
        if self.in_subset != 1 && self.in_subset != 2 {
            let name = match fullname.iter().position(|&b| b == b':') {
                Some(i) if i > 0 && i + 1 < fullname.len() => &fullname[i + 1..],
                _ => fullname,
            };
            self.fatal_msg(
                Code::InternalError,
                cat(&[b"SAX.xmlSAX2AttributeDecl(", name, b") called while not in subset\n"]),
            );
        }
    }

    /// `xmlSAX2InternalSubset`.
    fn sax_internal_subset(&mut self) {
        if let Some(doc) = &self.my_doc {
            doc.borrow_mut().int_subset = Some(Dtd::default());
        }
    }

    /// `xmlSAX2ExternalSubset`. The PostgreSQL entity loader gives an empty subset, so the only effects of `xmlParseExternalSubset` are on the state of the parser.
    pub(super) fn sax_external_subset(
        &mut self,
        external_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
    ) {
        if (external_id.is_none() && system_id.is_none())
            || !(self.validate || self.loadsubset != 0)
            || !self.well_formed
            || self.my_doc.is_none()
        {
            return;
        }
        // The input that the loader returns.
        self.input_id += 1;
        if let Some(doc) = &self.my_doc {
            let mut doc = doc.borrow_mut();
            if doc.ext_subset.is_none() {
                doc.ext_subset = Some(Dtd::default());
            }
        }
        self.instate = State::Dtd;
        self.external = true;
    }

    /// `xmlCleanSpecialAttr`.
    pub(super) fn clean_special_attr(&mut self) {
        self.atts_special.borrow_mut().retain(|_, t| *t != ATTRIBUTE_CDATA);
    }

    // The literals.

    /// `xmlParseEntityValue`.
    fn parse_entity_value(&mut self) -> Option<Vec<u8>> {
        let stop = match self.cur() {
            b'"' => b'"',
            b'\'' => b'\'',
            _ => {
                self.fatal_err(Code::EntityNotStarted, None);
                return None;
            }
        };
        self.instate = State::EntityValue;
        let input_id = self.input().id;
        self.next();
        let mut buf = Vec::new();
        let mut l = 0;
        let mut c = self.cur_char(&mut l);
        while is_char(c)
            && (c != u32::from(stop) || self.input().id != input_id)
            && self.instate != State::Eof
        {
            copy_buf(&mut buf, c);
            self.nextl(l);
            c = self.cur_char(&mut l);
            if c == 0 {
                c = self.cur_char(&mut l);
            }
        }
        if self.instate == State::Eof {
            return None;
        }
        if c != u32::from(stop) {
            self.fatal_err(Code::EntityNotFinished, None);
            return None;
        }
        self.next();
        let mut i = 0;
        while at(&buf, i) != 0 {
            let tmp = buf[i];
            if tmp == b'%' || (tmp == b'&' && at(&buf, i + 1) != b'#') {
                i += 1;
                let name_ok = match self.parse_string_name(&buf, i) {
                    Some((_, next)) => {
                        i = next;
                        true
                    }
                    None => false,
                };
                if !name_ok || at(&buf, i) != b';' {
                    self.fatal_msg(
                        Code::EntityCharError,
                        cat(&[
                            b"EntityValue: '",
                            &[tmp],
                            b"' forbidden except for entities references\n",
                        ]),
                    );
                    return None;
                }
                if tmp == b'%' && self.in_subset == 1 && self.inputs.len() == 1 {
                    self.fatal_err(Code::EntityPeInternal, None);
                    return None;
                }
                if at(&buf, i) == 0 {
                    break;
                }
            }
            i += 1;
        }
        self.depth += 1;
        let ret = self.string_decode_entities(&buf, SUBSTITUTE_PEREF);
        self.depth -= 1;
        ret
    }

    /// `xmlParseSystemLiteral`.
    fn parse_system_literal(&mut self) -> Option<Vec<u8>> {
        let state = self.instate;
        self.shrink();
        let stop = match self.cur() {
            b'"' => b'"',
            b'\'' => b'\'',
            _ => {
                self.fatal_err(Code::LiteralNotStarted, None);
                return None;
            }
        };
        self.next();
        self.instate = State::SystemLiteral;
        let mut buf = Vec::new();
        let mut size = BUFFER_SIZE;
        let mut count = 0;
        let mut l = 0;
        let mut cur = self.cur_char(&mut l);
        while is_char(cur) && cur != u32::from(stop) {
            if buf.len() + 5 >= size {
                if size > MAX_NAME_LENGTH {
                    self.fatal_err(Code::NameTooLong, Some(b"SystemLiteral"));
                    self.instate = state;
                    return None;
                }
                size *= 2;
            }
            count += 1;
            if count > 50 {
                self.shrink();
                count = 0;
                if self.instate == State::Eof {
                    return None;
                }
            }
            copy_buf(&mut buf, cur);
            self.nextl(l);
            cur = self.cur_char(&mut l);
            if cur == 0 {
                self.shrink();
                cur = self.cur_char(&mut l);
            }
        }
        self.instate = state;
        if !is_char(cur) {
            self.fatal_err(Code::LiteralNotFinished, None);
        } else {
            self.next();
        }
        Some(buf)
    }

    /// `xmlParsePubidLiteral`.
    fn parse_pubid_literal(&mut self) -> Option<Vec<u8>> {
        let oldstate = self.instate;
        self.shrink();
        let stop = match self.cur() {
            b'"' => b'"',
            b'\'' => b'\'',
            _ => {
                self.fatal_err(Code::LiteralNotStarted, None);
                return None;
            }
        };
        self.next();
        self.instate = State::PublicLiteral;
        let mut buf = Vec::new();
        let mut size = BUFFER_SIZE;
        let mut count = 0;
        let mut cur = self.cur();
        while is_pubid_char(u32::from(cur)) && cur != stop {
            if buf.len() + 1 >= size {
                if size > MAX_NAME_LENGTH {
                    self.fatal_err(Code::NameTooLong, Some(b"Public ID"));
                    return None;
                }
                size *= 2;
            }
            buf.push(cur);
            count += 1;
            if count > 50 {
                self.shrink();
                count = 0;
                if self.instate == State::Eof {
                    return None;
                }
            }
            self.next();
            cur = self.cur();
            if cur == 0 {
                self.shrink();
                cur = self.cur();
            }
        }
        if cur != stop {
            self.fatal_err(Code::LiteralNotFinished, None);
        } else {
            self.next();
        }
        self.instate = oldstate;
        Some(buf)
    }

    /// `xmlParseExternalID`. It returns the system literal and the public identifier.
    fn parse_external_id(&mut self, strict: bool) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        self.shrink();
        let mut uri = None;
        let mut public_id = None;
        if self.looking_at(b"SYSTEM") {
            self.skip(6);
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required after 'SYSTEM'\n".to_vec());
            }
            uri = self.parse_system_literal();
            if uri.is_none() {
                self.fatal_err(Code::UriRequired, None);
            }
        } else if self.looking_at(b"PUBLIC") {
            self.skip(6);
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required after 'PUBLIC'\n".to_vec());
            }
            public_id = self.parse_pubid_literal();
            if public_id.is_none() {
                self.fatal_err(Code::PubidRequired, None);
            }
            if strict {
                if self.skip_blanks() == 0 {
                    self.fatal_msg(
                        Code::SpaceRequired,
                        b"Space required after the Public Identifier\n".to_vec(),
                    );
                }
            } else {
                if self.skip_blanks() == 0 {
                    return (None, public_id);
                }
                if self.cur() != b'\'' && self.cur() != b'"' {
                    return (None, public_id);
                }
            }
            uri = self.parse_system_literal();
            if uri.is_none() {
                self.fatal_err(Code::UriRequired, None);
            }
        }
        (uri, public_id)
    }

    /// The check that a declaration ends in the input where it starts.
    fn check_boundary(&mut self, inputid: i32, msg: &[u8]) {
        if self.input().id != inputid {
            self.fatal_msg(Code::EntityBoundary, msg.to_vec());
        }
    }

    // The markup declarations.

    /// `xmlParseNotationDecl`.
    fn parse_notation_decl(&mut self) {
        if !self.looking_at(b"<!NOTATION") {
            return;
        }
        let inputid = self.input().id;
        self.shrink();
        self.skip(10);
        if self.skip_blanks() == 0 {
            self.fatal_msg(Code::SpaceRequired, b"Space required after '<!NOTATION'\n".to_vec());
            return;
        }
        let Some(name) = self.parse_name() else {
            self.fatal_err(Code::NotationNotStarted, None);
            return;
        };
        if name.contains(&b':') {
            self.ns_err();
        }
        if self.skip_blanks() == 0 {
            self.fatal_msg(
                Code::SpaceRequired,
                b"Space required after the NOTATION name'\n".to_vec(),
            );
            return;
        }
        let (system_id, public_id) = self.parse_external_id(false);
        self.skip_blanks();
        if self.cur() == b'>' {
            self.check_boundary(
                inputid,
                b"Notation declaration doesn't start and stop in the same entity\n",
            );
            self.next();
            if !self.disable_sax {
                self.sax_notation_decl(&name, public_id.as_deref(), system_id.as_deref());
            }
        } else {
            self.fatal_err(Code::NotationNotFinished, None);
        }
    }

    /// The URI checks of `xmlParseEntityDecl`. It returns true if the URI is valid and has no fragment.
    fn check_entity_uri(&mut self, uri: &[u8]) -> bool {
        match uri::parse(uri) {
            None => {
                self.err_msg_str(Code::InvalidUri, cat(&[b"Invalid URI: ", cstr(uri), b"\n"]));
                false
            }
            Some(u) if u.fragment => {
                self.fatal_err(Code::UriFragment, None);
                false
            }
            Some(_) => true,
        }
    }

    /// The SAX compatibility path of `xmlParseEntityDecl`. If there is no document, or the document is the compatibility document, it makes sure that the compatibility document and its internal subset exist and returns true. The caller then declares the entity directly.
    fn compat_doc(&mut self) -> bool {
        let doc = self.my_doc.get_or_insert_with(|| {
            let mut doc = Doc::new();
            doc.compat = true;
            Rc::new(RefCell::new(doc))
        });
        let mut doc = doc.borrow_mut();
        if !doc.compat {
            return false;
        }
        if doc.int_subset.is_none() {
            doc.int_subset = Some(Dtd::default());
        }
        true
    }

    /// `xmlParseEntityDecl`.
    fn parse_entity_decl(&mut self) {
        if !self.looking_at(b"<!ENTITY") {
            return;
        }
        let inputid = self.input().id;
        self.shrink();
        self.skip(8);
        if self.skip_blanks() == 0 {
            self.fatal_msg(Code::SpaceRequired, b"Space required after '<!ENTITY'\n".to_vec());
        }
        let mut is_parameter = false;
        if self.cur() == b'%' {
            self.next();
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required after '%%'\n".to_vec());
            }
            is_parameter = true;
        }
        let Some(name) = self.parse_name() else {
            self.fatal_msg(Code::NameRequired, b"xmlParseEntityDecl: no name\n".to_vec());
            return;
        };
        if name.contains(&b':') {
            self.ns_err();
        }
        if self.skip_blanks() == 0 {
            self.fatal_msg(Code::SpaceRequired, b"Space required after the entity name\n".to_vec());
        }
        self.instate = State::EntityDecl;
        let quoted = self.cur() == b'"' || self.cur() == b'\'';
        if is_parameter {
            if quoted {
                let value = self.parse_entity_value();
                if let Some(value) = value
                    && !self.disable_sax
                {
                    self.sax_entity_decl(
                        &name,
                        EntityType::InternalParameter,
                        None,
                        None,
                        Some(&value),
                    );
                }
            } else {
                let (uri, literal) = self.parse_external_id(true);
                if uri.is_none() && literal.is_none() {
                    self.fatal_err(Code::ValueRequired, None);
                }
                if let Some(uri) = &uri
                    && self.check_entity_uri(uri)
                    && !self.disable_sax
                {
                    self.sax_entity_decl(
                        &name,
                        EntityType::ExternalParameter,
                        literal.as_deref(),
                        Some(uri),
                        None,
                    );
                }
            }
        } else if quoted {
            let value = self.parse_entity_value();
            if !self.disable_sax {
                self.sax_entity_decl(
                    &name,
                    EntityType::InternalGeneral,
                    None,
                    None,
                    value.as_deref(),
                );
            }
            if self.compat_doc() {
                self.sax_entity_decl(
                    &name,
                    EntityType::InternalGeneral,
                    None,
                    None,
                    value.as_deref(),
                );
            }
        } else {
            let (uri, literal) = self.parse_external_id(true);
            if uri.is_none() && literal.is_none() {
                self.fatal_err(Code::ValueRequired, None);
            }
            if let Some(uri) = &uri {
                self.check_entity_uri(uri);
            }
            if self.cur() != b'>' && self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required before 'NDATA'\n".to_vec());
            }
            if self.looking_at(b"NDATA") {
                self.skip(5);
                if self.skip_blanks() == 0 {
                    self.fatal_msg(Code::SpaceRequired, b"Space required after 'NDATA'\n".to_vec());
                }
                let ndata = self.parse_name();
                if !self.disable_sax {
                    self.sax_unparsed_entity_decl(
                        &name,
                        literal.as_deref(),
                        uri.as_deref(),
                        ndata.as_deref(),
                    );
                }
            } else {
                if !self.disable_sax {
                    self.sax_entity_decl(
                        &name,
                        EntityType::ExternalGeneralParsed,
                        literal.as_deref(),
                        uri.as_deref(),
                        None,
                    );
                }
                if self.replace_entities && self.compat_doc() {
                    self.sax_entity_decl(
                        &name,
                        EntityType::ExternalGeneralParsed,
                        literal.as_deref(),
                        uri.as_deref(),
                        None,
                    );
                }
            }
        }
        if self.instate == State::Eof {
            return;
        }
        self.skip_blanks();
        if self.cur() != b'>' {
            self.fatal_msg(
                Code::EntityNotFinished,
                cat(&[b"xmlParseEntityDecl: entity ", &name, b" not terminated\n"]),
            );
            self.halt();
        } else {
            self.check_boundary(
                inputid,
                b"Entity declaration doesn't start and stop in the same entity\n",
            );
            self.next();
        }
    }

    /// `xmlParseDefaultDecl`. It returns the default type and the value.
    fn parse_default_decl(&mut self) -> (i32, Option<Vec<u8>>) {
        if self.looking_at(b"#REQUIRED") {
            self.skip(9);
            return (ATTRIBUTE_REQUIRED, None);
        }
        if self.looking_at(b"#IMPLIED") {
            self.skip(8);
            return (ATTRIBUTE_IMPLIED, None);
        }
        let mut val = ATTRIBUTE_NONE;
        if self.looking_at(b"#FIXED") {
            self.skip(6);
            val = ATTRIBUTE_FIXED;
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required after '#FIXED'\n".to_vec());
            }
        }
        let ret = self.parse_att_value();
        self.instate = State::Dtd;
        if ret.is_none() {
            self.fatal_msg(self.err_no, b"Attribute default value declaration error\n".to_vec());
        }
        (val, ret)
    }

    /// `xmlParseNotationType`. It returns true for a list.
    fn parse_notation_type(&mut self) -> bool {
        if self.cur() != b'(' {
            self.fatal_err(Code::NotationNotStarted, None);
            return false;
        }
        self.shrink();
        let mut names: Vec<Vec<u8>> = Vec::new();
        loop {
            self.next();
            self.skip_blanks();
            let Some(name) = self.parse_name() else {
                self.fatal_msg(
                    Code::NameRequired,
                    b"Name expected in NOTATION declaration\n".to_vec(),
                );
                return false;
            };
            if names.contains(&name) {
                self.validity_error();
            } else {
                names.push(name);
            }
            self.skip_blanks();
            if self.cur() != b'|' {
                break;
            }
        }
        if self.cur() != b')' {
            self.fatal_err(Code::NotationNotFinished, None);
            return false;
        }
        self.next();
        true
    }

    /// `xmlParseEnumerationType`. It returns true if the list has a token, even after an error.
    fn parse_enumeration_type(&mut self) -> bool {
        if self.cur() != b'(' {
            self.fatal_err(Code::AttlistNotStarted, None);
            return false;
        }
        self.shrink();
        let mut names: Vec<Vec<u8>> = Vec::new();
        loop {
            self.next();
            self.skip_blanks();
            let Some(name) = self.parse_nmtoken() else {
                self.fatal_err(Code::NmtokenRequired, None);
                return !names.is_empty();
            };
            if names.contains(&name) {
                self.validity_error();
            } else {
                names.push(name);
            }
            self.skip_blanks();
            if self.cur() != b'|' {
                break;
            }
        }
        if self.cur() != b')' {
            self.fatal_err(Code::AttlistNotFinished, None);
            return !names.is_empty();
        }
        self.next();
        !names.is_empty()
    }

    /// `xmlParseEnumeratedType`.
    fn parse_enumerated_type(&mut self) -> i32 {
        if self.looking_at(b"NOTATION") {
            self.skip(8);
            if self.skip_blanks() == 0 {
                self.fatal_msg(Code::SpaceRequired, b"Space required after 'NOTATION'\n".to_vec());
                return 0;
            }
            return if self.parse_notation_type() { ATTRIBUTE_NOTATION } else { 0 };
        }
        if self.parse_enumeration_type() { ATTRIBUTE_ENUMERATION } else { 0 }
    }

    /// `xmlParseAttributeType`.
    fn parse_attribute_type(&mut self) -> i32 {
        self.shrink();
        let words: [(&[u8], i32); 8] = [
            (b"CDATA", ATTRIBUTE_CDATA),
            (b"IDREFS", ATTRIBUTE_IDREFS),
            (b"IDREF", ATTRIBUTE_IDREF),
            (b"ID", ATTRIBUTE_ID),
            (b"ENTITY", ATTRIBUTE_ENTITY),
            (b"ENTITIES", ATTRIBUTE_ENTITIES),
            (b"NMTOKENS", ATTRIBUTE_NMTOKENS),
            (b"NMTOKEN", ATTRIBUTE_NMTOKEN),
        ];
        for (word, typ) in words {
            if self.looking_at(word) {
                self.skip(word.len());
                return typ;
            }
        }
        self.parse_enumerated_type()
    }

    /// `xmlParseAttributeListDecl`.
    fn parse_attribute_list_decl(&mut self) {
        if !self.looking_at(b"<!ATTLIST") {
            return;
        }
        let inputid = self.input().id;
        self.skip(9);
        if self.skip_blanks() == 0 {
            self.fatal_msg(Code::SpaceRequired, b"Space required after '<!ATTLIST'\n".to_vec());
        }
        let Some(elem) = self.parse_name() else {
            self.fatal_msg(Code::NameRequired, b"ATTLIST: no name for Element\n".to_vec());
            return;
        };
        self.skip_blanks();
        while self.cur() != b'>' && self.instate != State::Eof {
            let Some(attr) = self.parse_name() else {
                self.fatal_msg(Code::NameRequired, b"ATTLIST: no name for Attribute\n".to_vec());
                break;
            };
            if self.skip_blanks() == 0 {
                self.fatal_msg(
                    Code::SpaceRequired,
                    b"Space required after the attribute name\n".to_vec(),
                );
                break;
            }
            let typ = self.parse_attribute_type();
            if typ <= 0 {
                break;
            }
            if self.skip_blanks() == 0 {
                self.fatal_msg(
                    Code::SpaceRequired,
                    b"Space required after the attribute type\n".to_vec(),
                );
                break;
            }
            let (def, _default) = self.parse_default_decl();
            if def <= 0 {
                break;
            }
            if self.cur() != b'>' && self.skip_blanks() == 0 {
                self.fatal_msg(
                    Code::SpaceRequired,
                    b"Space required after the attribute default value\n".to_vec(),
                );
                break;
            }
            if !self.disable_sax {
                self.sax_attribute_decl(&attr, typ);
            }
            // `xmlAddSpecialAttr`: the first declaration wins.
            self.atts_special.borrow_mut().entry((elem.clone(), attr)).or_insert(typ);
        }
        if self.cur() == b'>' {
            self.check_boundary(
                inputid,
                b"Attribute list declaration doesn't start and stop in the same entity\n",
            );
            self.next();
        }
    }

    /// `xmlParseElementMixedContentDecl`. The content model is not kept.
    fn parse_element_mixed_content_decl(&mut self, inputchk: i32) {
        if !self.looking_at(b"#PCDATA") {
            self.fatal_err(Code::PcdataRequired, None);
            return;
        }
        self.skip(7);
        self.skip_blanks();
        self.shrink();
        if self.cur() == b')' {
            self.check_boundary(inputchk, BOUNDARY_ELEMENT_CONTENT);
            self.next();
            if self.cur() == b'*' {
                self.next();
            }
            return;
        }
        while self.cur() == b'|' && self.instate != State::Eof {
            self.next();
            self.skip_blanks();
            if self.parse_name().is_none() {
                self.fatal_msg(
                    Code::NameRequired,
                    b"xmlParseElementMixedContentDecl : Name expected\n".to_vec(),
                );
                return;
            }
            self.skip_blanks();
        }
        if self.cur() == b')' && self.nxt(1) == b'*' {
            self.check_boundary(inputchk, BOUNDARY_ELEMENT_CONTENT);
            self.skip(2);
        } else {
            self.fatal_err(Code::MixedNotStarted, None);
        }
    }

    /// The `?`, `*` or `+` after a content particle.
    fn skip_occurrence(&mut self) {
        if matches!(self.cur(), b'?' | b'*' | b'+') {
            self.next();
        }
    }

    /// One particle of `xmlParseElementChildrenContentDeclPriv`: a group or a name with its occurrence.
    fn parse_content_particle(&mut self, depth: i32) -> bool {
        if self.cur() == b'(' {
            let inputid = self.input().id;
            self.next();
            self.skip_blanks();
            if !self.parse_element_children_content_decl(inputid, depth + 1) {
                return false;
            }
            self.skip_blanks();
        } else {
            if self.parse_name().is_none() {
                self.fatal_err(Code::ElemcontentNotStarted, None);
                return false;
            }
            self.skip_occurrence();
        }
        true
    }

    /// `xmlParseElementChildrenContentDeclPriv`. It returns false where libxml2 returns NULL.
    fn parse_element_children_content_decl(&mut self, inputchk: i32, depth: i32) -> bool {
        if depth > 128 {
            self.fatal_msg(
                Code::ElemcontentNotFinished,
                format!("xmlParseElementChildrenContentDecl : depth {depth} too deep, use XML_PARSE_HUGE\n")
                    .into_bytes(),
            );
            return false;
        }
        self.skip_blanks();
        if !self.parse_content_particle(depth) {
            return false;
        }
        self.skip_blanks();
        self.shrink();
        let mut typ = 0u8;
        while self.cur() != b')' && self.instate != State::Eof {
            let c = self.cur();
            if c == b',' || c == b'|' {
                if typ == 0 {
                    typ = c;
                } else if typ != c {
                    self.fatal_msg(
                        Code::SeparatorRequired,
                        cat(&[b"xmlParseElementChildrenContentDecl : '", &[typ], b"' expected\n"]),
                    );
                    return false;
                }
                self.next();
            } else {
                self.fatal_err(Code::ElemcontentNotFinished, None);
                return false;
            }
            self.skip_blanks();
            if !self.parse_content_particle(depth) {
                return false;
            }
            self.skip_blanks();
        }
        self.check_boundary(inputchk, BOUNDARY_ELEMENT_CONTENT);
        self.next();
        self.skip_occurrence();
        true
    }

    /// `xmlParseElementContentDecl`.
    fn parse_element_content_decl(&mut self, name: &[u8]) -> i32 {
        let inputid = self.input().id;
        if self.cur() != b'(' {
            self.fatal_msg(
                Code::ElemcontentNotStarted,
                cat(&[b"xmlParseElementContentDecl : ", name, b" '(' expected\n"]),
            );
            return -1;
        }
        self.next();
        if self.instate == State::Eof {
            return -1;
        }
        self.skip_blanks();
        let res = if self.looking_at(b"#PCDATA") {
            self.parse_element_mixed_content_decl(inputid);
            ELEMENT_TYPE_MIXED
        } else {
            self.parse_element_children_content_decl(inputid, 1);
            ELEMENT_TYPE_ELEMENT
        };
        self.skip_blanks();
        res
    }

    /// `xmlParseElementDecl`.
    fn parse_element_decl(&mut self) -> i32 {
        if !self.looking_at(b"<!ELEMENT") {
            return -1;
        }
        let inputid = self.input().id;
        self.skip(9);
        if self.skip_blanks() == 0 {
            self.fatal_msg(Code::SpaceRequired, b"Space required after 'ELEMENT'\n".to_vec());
            return -1;
        }
        let Some(name) = self.parse_name() else {
            self.fatal_msg(
                Code::NameRequired,
                b"xmlParseElementDecl: no name for Element\n".to_vec(),
            );
            return -1;
        };
        if self.skip_blanks() == 0 {
            self.fatal_msg(
                Code::SpaceRequired,
                b"Space required after the element name\n".to_vec(),
            );
        }
        let ret = if self.looking_at(b"EMPTY") {
            self.skip(5);
            ELEMENT_TYPE_EMPTY
        } else if self.looking_at(b"ANY") {
            self.skip(3);
            ELEMENT_TYPE_ANY
        } else if self.cur() == b'(' {
            self.parse_element_content_decl(&name)
        } else {
            if self.cur() == b'%' && !self.external && self.inputs.len() == 1 {
                self.fatal_msg(
                    Code::PerefInIntSubset,
                    b"PEReference: forbidden within markup decl in internal subset\n".to_vec(),
                );
            } else {
                self.fatal_msg(
                    Code::ElemcontentNotStarted,
                    b"xmlParseElementDecl: 'EMPTY', 'ANY' or '(' expected\n".to_vec(),
                );
            }
            return -1;
        };
        self.skip_blanks();
        if self.cur() != b'>' {
            self.fatal_err(Code::GtRequired, None);
        } else {
            self.check_boundary(
                inputid,
                b"Element declaration doesn't start and stop in the same entity\n",
            );
            self.next();
            if !self.disable_sax {
                self.sax_element_decl(&name);
            }
        }
        ret
    }

    /// `xmlParseMarkupDecl`.
    fn parse_markup_decl(&mut self) {
        if self.cur() == b'<' {
            if self.nxt(1) == b'!' {
                match self.nxt(2) {
                    b'E' => {
                        if self.nxt(3) == b'L' {
                            self.parse_element_decl();
                        } else if self.nxt(3) == b'N' {
                            self.parse_entity_decl();
                        }
                    }
                    b'A' => self.parse_attribute_list_decl(),
                    b'N' => self.parse_notation_decl(),
                    b'-' => self.parse_comment(),
                    _ => {}
                }
            } else if self.nxt(1) == b'?' {
                self.parse_pi();
            }
        }
        if self.instate == State::Eof {
            return;
        }
        self.instate = State::Dtd;
    }

    // The parameter entities.

    /// `xmlParsePEReference`.
    pub(super) fn parse_pe_reference(&mut self) {
        if self.cur() != b'%' {
            return;
        }
        self.next();
        let Some(name) = self.parse_name() else {
            self.fatal_msg(Code::PerefNoName, b"PEReference: no name\n".to_vec());
            return;
        };
        if self.cur() != b';' {
            self.fatal_err(Code::PerefSemicolMissing, None);
            return;
        }
        self.next();
        self.nbentities += 1;
        let entity = self.sax_get_parameter_entity(&name);
        if self.instate == State::Eof {
            return;
        }
        match entity {
            None => {
                let msg = cat(&[b"PEReference: %", &name, b"; not found\n"]);
                if self.standalone == 1 || (!self.has_external_subset && !self.has_perefs) {
                    self.fatal_msg(Code::UndeclaredEntity, msg);
                } else {
                    self.warning_msg(Code::WarUndeclaredEntity, msg);
                }
                self.entity_check(0, None, 0);
            }
            Some(entity) => {
                let etype = entity.borrow().etype;
                if etype != EntityType::InternalParameter && etype != EntityType::ExternalParameter
                {
                    self.warning_msg(
                        Code::WarUndeclaredEntity,
                        cat(&[b"Internal: %", &name, b"; is not a parameter entity\n"]),
                    );
                } else {
                    if self.entity_check(0, Some(&entity), 0) {
                        return;
                    }
                    if etype == EntityType::ExternalParameter
                        && self.options & (super::ctxt::PARSE_NOENT | super::ctxt::PARSE_DTDVALID)
                            == 0
                        && self.options & (super::ctxt::PARSE_DTDLOAD | super::ctxt::PARSE_DTDATTR)
                            == 0
                        && !self.replace_entities
                        && !self.validate
                    {
                        return;
                    }
                    let input = self.new_entity_input(&entity);
                    if self.push_input(input) < 0 {
                        return;
                    }
                    // An external parameter entity is empty, so it has no text declaration.
                }
            }
        }
        self.has_perefs = true;
    }

    /// `xmlParseStringPEReference`. It returns the entity and the position after the reference.
    pub(super) fn parse_string_pe_reference(
        &mut self,
        s: &[u8],
        i: usize,
    ) -> (Option<Rc<RefCell<Entity>>>, usize) {
        if at(s, i) != b'%' {
            return (None, i);
        }
        let Some((name, ptr)) = self.parse_string_name(s, i + 1) else {
            self.fatal_msg(Code::NameRequired, b"xmlParseStringPEReference: no name\n".to_vec());
            return (None, i + 1);
        };
        if at(s, ptr) != b';' {
            self.fatal_err(Code::EntityRefSemicolMissing, None);
            return (None, ptr);
        }
        let ptr = ptr + 1;
        self.nbentities += 1;
        let entity = self.sax_get_parameter_entity(&name);
        if self.instate == State::Eof {
            return (None, ptr);
        }
        match &entity {
            None => {
                let msg = cat(&[b"PEReference: %", &name, b"; not found\n"]);
                if self.standalone == 1 || (!self.has_external_subset && !self.has_perefs) {
                    self.fatal_msg(Code::UndeclaredEntity, msg);
                } else {
                    self.warning_msg(Code::WarUndeclaredEntity, msg);
                }
                self.entity_check(0, None, 0);
            }
            Some(e) => {
                let etype = e.borrow().etype;
                if etype != EntityType::InternalParameter && etype != EntityType::ExternalParameter
                {
                    self.warning_msg(
                        Code::WarUndeclaredEntity,
                        cat(&[b"%", &name, b"; is not a parameter entity\n"]),
                    );
                }
            }
        }
        self.has_perefs = true;
        (entity, ptr)
    }

    /// `xmlLoadEntityContent`.
    pub(super) fn load_entity_content(&mut self, ent: &Rc<RefCell<Entity>>) -> i32 {
        {
            let e = ent.borrow();
            if (e.etype != EntityType::ExternalParameter
                && e.etype != EntityType::ExternalGeneralParsed)
                || e.content.is_some()
            {
                drop(e);
                self.fatal_err(Code::InternalError, Some(b"xmlLoadEntityContent parameter error"));
                return -1;
            }
        }
        let Some(input) = self.new_entity_input(ent) else {
            self.fatal_err(Code::InternalError, Some(b"xmlLoadEntityContent input error"));
            return -1;
        };
        let id = input.id;
        if self.push_input(Some(input)) < 0 {
            return -1;
        }
        let mut buf = Vec::new();
        let mut l = 0;
        let mut c = self.cur_char(&mut l);
        while self.input().id == id && self.input().cur < self.input().end && is_char(c) {
            let cur = self.pos();
            buf.extend_from_slice(&self.slice(cur, cur + l));
            self.nextl(l);
            c = self.cur_char(&mut l);
            if c == 0 {
                if self.instate == State::Eof {
                    return -1;
                }
                c = self.cur_char(&mut l);
            }
        }
        if self.input().id == id && self.input().cur >= self.input().end {
            self.pop_input();
        } else if !is_char(c) {
            self.fatal_msg(
                Code::InvalidChar,
                format!("xmlLoadEntityContent: invalid char value {c}\n").into_bytes(),
            );
            return -1;
        }
        ent.borrow_mut().content = Some(buf);
        0
    }

    // The document type declaration.

    /// `xmlParseDocTypeDecl`. It returns the system literal and the public identifier of the external subset.
    pub(super) fn parse_doctype_decl(&mut self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        self.skip(9);
        self.skip_blanks();
        if self.parse_name().is_none() {
            self.fatal_msg(
                Code::NameRequired,
                b"xmlParseDocTypeDecl : no DOCTYPE name !\n".to_vec(),
            );
        }
        self.skip_blanks();
        let (uri, external_id) = self.parse_external_id(true);
        if uri.is_some() || external_id.is_some() {
            self.has_external_subset = true;
        }
        self.skip_blanks();
        if !self.disable_sax {
            self.sax_internal_subset();
        }
        if self.instate == State::Eof || self.cur() == b'[' {
            return (uri, external_id);
        }
        if self.cur() != b'>' {
            self.fatal_err(Code::DoctypeNotFinished, None);
        }
        self.next();
        (uri, external_id)
    }

    /// `xmlParseInternalSubset`. The conditional sections need a parameter entity with a file name and content, and the PostgreSQL entity loader makes all external entities empty, so they do not occur.
    pub(super) fn parse_internal_subset(&mut self) {
        if self.cur() == b'[' {
            let base = self.inputs.len();
            self.instate = State::Dtd;
            self.next();
            while (self.cur() != b']' || self.inputs.len() > base) && self.instate != State::Eof {
                let check = (self.input().id, self.pos());
                self.skip_blanks();
                self.parse_markup_decl();
                self.parse_pe_reference();
                if (self.input().id, self.pos()) == check {
                    self.fatal_err(
                        Code::InternalError,
                        Some(b"xmlParseInternalSubset: error detected in Markup declaration\n"),
                    );
                    if self.inputs.len() > base {
                        self.pop_input();
                    } else {
                        break;
                    }
                }
            }
            if self.cur() == b']' {
                self.next();
                self.skip_blanks();
            }
        }
        if self.cur() != b'>' {
            self.fatal_err(Code::DoctypeNotFinished, None);
            return;
        }
        self.next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predefined_redeclaration() {
        let v = |c: &[u8], ch: u8| valid_predefined(EntityType::InternalGeneral, Some(c), ch);
        assert!(v(b">", b'>'));
        assert!(!v(b"<", b'<'));
        assert!(v(b"&#60;", b'<'));
        assert!(v(b"&#x3c;", b'<'));
        assert!(!v(b"&#X3C;", b'<'));
        assert!(!v(b"&#61;", b'<'));
        assert!(!valid_predefined(EntityType::ExternalGeneralParsed, Some(b">"), b'>'));
    }
}

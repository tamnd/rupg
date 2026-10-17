//! The type `xml`: the input, output, receive and send functions of `xml.c` of PostgreSQL, with the check of libxml2 2.9.14 that PostgreSQL uses for a well formed value.
//!
//! PostgreSQL keeps the text of an `xml` value as it is, and only checks it. The checker in the modules below follows the parser of libxml2 with the options of PostgreSQL, so it accepts the same values and gives the same error text. It does not build a tree, because PostgreSQL does not keep one.

mod build;
mod content;
mod ctxt;
mod document;
mod dtd;
mod encodings;
mod uri;

use std::cell::RefCell;
use std::rc::Rc;

use rupg_common::SqlState;

use crate::TypeError;
use ctxt::{Report, cstr};

pub use build::{
    XmlStandalone, escape_xml, sql_name, xml_base64, xml_binhex, xml_comment, xml_concat,
    xml_element, xml_name, xml_pi, xml_root, xml_text,
};

/// The error codes of `parse_xml_decl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeclError {
    InvalidChar,
    SpaceRequired,
    StandaloneValue,
    VersionMissing,
    MissingEncoding,
    XmldeclNotFinished,
}

impl DeclError {
    /// `errdetail_for_xml_code`.
    fn detail(self) -> &'static str {
        match self {
            DeclError::InvalidChar => "Invalid character value.",
            DeclError::SpaceRequired => "Space required.",
            DeclError::StandaloneValue => "standalone accepts only 'yes' or 'no'.",
            DeclError::VersionMissing => "Malformed declaration: missing version.",
            DeclError::MissingEncoding => "Missing encoding in text declaration.",
            DeclError::XmldeclNotFinished => "Parsing XML declaration: '?>' expected.",
        }
    }
}

/// The parts of an XML declaration that `parse_xml_decl` reads.
#[derive(Debug, Default)]
struct Decl<'a> {
    /// The length of the declaration, or 0 if there is none.
    len: usize,
    version: Option<&'a [u8]>,
    encoding: Option<&'a [u8]>,
    /// 1 for `yes`, 0 for `no` and -1 if it is not there.
    standalone: i32,
}

/// `xmlIsBlank_ch`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// The byte at `i`, or 0 after the end, as in a C string.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `xmlGetUTF8Char` on at most 4 bytes. It gives -1 for a sequence that is not valid.
fn utf8_char(s: &[u8]) -> i64 {
    let len = s.len().min(4);
    let b = |i: usize| i64::from(s[i]);
    if len < 1 {
        return -1;
    }
    let c = s[0];
    if c & 0x80 == 0 {
        return b(0);
    }
    if len < 2 || s[1] & 0xc0 != 0x80 {
        return -1;
    }
    if c & 0xe0 != 0xe0 {
        return (b(0) & 0x1f) << 6 | (b(1) & 0x3f);
    }
    if len < 3 || s[2] & 0xc0 != 0x80 {
        return -1;
    }
    if c & 0xf0 != 0xf0 {
        return (b(0) & 0x0f) << 12 | (b(1) & 0x3f) << 6 | (b(2) & 0x3f);
    }
    if c & 0xf8 != 0xf0 || len < 4 || s[3] & 0xc0 != 0x80 {
        return -1;
    }
    (b(0) & 0x07) << 18 | (b(1) & 0x3f) << 12 | (b(2) & 0x3f) << 6 | (b(3) & 0x3f)
}

/// `PG_XMLISNAMECHAR`. The `_ch` forms of the libxml2 macros take each code point from 0xF8 up as a base char.
fn is_name_char(c: i64) -> bool {
    c.is_positive()
        && (c >= 0xf8
            || (0xc0..=0xd6).contains(&c)
            || (0xd8..=0xf6).contains(&c)
            || c == 0xb7
            || u8::try_from(c)
                .is_ok_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b':')))
}

/// `parse_xml_decl`. The string ends at the first zero byte.
fn parse_xml_decl(s: &[u8]) -> Result<Decl<'_>, DeclError> {
    let s = cstr(s);
    let mut decl = Decl { standalone: -1, ..Decl::default() };
    let mut p = 0;
    let skip = |p: &mut usize| {
        while is_space(at(s, *p)) {
            *p += 1;
        }
    };
    let quoted = |p: &mut usize| -> Option<&[u8]> {
        let q = at(s, *p);
        if q != b'\'' && q != b'"' {
            return None;
        }
        let end = s[*p + 1..].iter().position(|&c| c == q)? + *p + 1;
        let value = &s[*p + 1..end];
        *p = end + 1;
        Some(value)
    };
    if s.starts_with(b"<?xml") && !is_name_char(utf8_char(&s[5..])) {
        p = 5;
        if !is_space(at(s, p)) {
            return Err(DeclError::SpaceRequired);
        }
        skip(&mut p);
        if !s[p..].starts_with(b"version") {
            return Err(DeclError::VersionMissing);
        }
        p += 7;
        skip(&mut p);
        if at(s, p) != b'=' {
            return Err(DeclError::VersionMissing);
        }
        p += 1;
        skip(&mut p);
        decl.version = Some(quoted(&mut p).ok_or(DeclError::VersionMissing)?);

        let save = p;
        skip(&mut p);
        if s[p..].starts_with(b"encoding") {
            if !is_space(at(s, save)) {
                return Err(DeclError::SpaceRequired);
            }
            p += 8;
            skip(&mut p);
            if at(s, p) != b'=' {
                return Err(DeclError::MissingEncoding);
            }
            p += 1;
            skip(&mut p);
            decl.encoding = Some(quoted(&mut p).ok_or(DeclError::MissingEncoding)?);
        } else {
            p = save;
        }

        let save = p;
        skip(&mut p);
        if s[p..].starts_with(b"standalone") {
            if !is_space(at(s, save)) {
                return Err(DeclError::SpaceRequired);
            }
            p += 10;
            skip(&mut p);
            if at(s, p) != b'=' {
                return Err(DeclError::StandaloneValue);
            }
            p += 1;
            skip(&mut p);
            let rest = &s[p..];
            if rest.starts_with(b"'yes'") || rest.starts_with(b"\"yes\"") {
                decl.standalone = 1;
                p += 5;
            } else if rest.starts_with(b"'no'") || rest.starts_with(b"\"no\"") {
                decl.standalone = 0;
                p += 4;
            } else {
                return Err(DeclError::StandaloneValue);
            }
        } else {
            p = save;
        }

        skip(&mut p);
        if !s[p..].starts_with(b"?>") {
            return Err(DeclError::XmldeclNotFinished);
        }
        p += 2;
    }
    if !s[..p].is_ascii() {
        return Err(DeclError::InvalidChar);
    }
    decl.len = p;
    Ok(decl)
}

/// `print_xml_decl` for a client in UTF-8, so with no encoding. It returns false if it writes nothing.
fn print_xml_decl(out: &mut Vec<u8>, version: Option<&[u8]>, standalone: i32) -> bool {
    if version.is_some_and(|v| v != b"1.0") || standalone != -1 {
        out.extend_from_slice(b"<?xml version=\"");
        out.extend_from_slice(version.unwrap_or(b"1.0"));
        out.push(b'"');
        match standalone {
            1 => out.extend_from_slice(b" standalone=\"yes\""),
            0 => out.extend_from_slice(b" standalone=\"no\""),
            _ => {}
        }
        out.extend_from_slice(b"?>");
        true
    } else {
        false
    }
}

/// `xml_doctype_in_content`: true if the content is a sequence of blanks, comments and processing instructions before `<!DOCTYPE`.
fn doctype_in_content(s: &[u8]) -> bool {
    let find = |s: &[u8], from: usize, pat: &[u8]| {
        s.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|i| i + from)
    };
    let mut p = 0;
    loop {
        while is_space(at(s, p)) {
            p += 1;
        }
        if at(s, p) != b'<' {
            return false;
        }
        p += 1;
        if at(s, p) == b'!' {
            p += 1;
            if s[p..].starts_with(b"DOCTYPE") {
                return true;
            }
            if !s[p..].starts_with(b"--") {
                return false;
            }
            match find(s, p + 2, b"--") {
                Some(e) if at(s, e + 2) == b'>' => p = e + 3,
                _ => return false,
            }
            continue;
        }
        if at(s, p) != b'?' {
            return false;
        }
        p += 1;
        match find(s, p, b"?>") {
            Some(e) => p = e + 2,
            None => return false,
        }
    }
}

/// The error of `xml_errsave`, with the text of libxml2 as the detail.
fn parse_error(sqlstate: SqlState, message: &str, report: &Report) -> TypeError {
    let mut error = TypeError::new(sqlstate, message.to_string());
    if !report.err_buf.is_empty() {
        error.detail = Some(String::from_utf8_lossy(&report.err_buf).into_owned());
    }
    error
}

/// `xml_parse` with the encoding UTF-8. `document` is true for `XMLOPTION_DOCUMENT`.
fn xml_parse(s: &[u8], document: bool, report: &Rc<RefCell<Report>>) -> Result<(), TypeError> {
    let s = cstr(s);
    let mut as_document = document;
    let mut count = 0;
    if !document {
        let decl = parse_xml_decl(s).map_err(|code| {
            let mut error = TypeError::new(
                SqlState::INVALID_XML_CONTENT,
                "invalid XML content: invalid XML declaration".to_string(),
            );
            error.detail = Some(code.detail().to_string());
            error
        })?;
        count = decl.len;
        as_document = doctype_in_content(&s[count..]);
    }
    if as_document {
        let well_formed = document::read_doc(s, report);
        let report = report.borrow();
        if !well_formed || report.err_occurred {
            return Err(if document {
                parse_error(SqlState::INVALID_XML_DOCUMENT, "invalid XML document", &report)
            } else {
                parse_error(SqlState::INVALID_XML_CONTENT, "invalid XML content", &report)
            });
        }
    } else if count < s.len() {
        let ret = document::balanced_chunk(&s[count..], report);
        let report = report.borrow();
        if ret != 0 || report.err_occurred {
            return Err(parse_error(SqlState::INVALID_XML_CONTENT, "invalid XML content", &report));
        }
    }
    Ok(())
}

/// `xml_in`, which checks that the text is a well formed document when `document` is true, or well formed content when it is false. The text ends at the first zero byte, as libxml2 sees it. `warn` gets the text of each warning of libxml2, which PostgreSQL reports with the SQLSTATE `01000` before the error.
///
/// # Errors
///
/// `2200M` for a document and `2200N` for content that is not well formed, with the messages of libxml2 as the detail.
pub fn xml_in(text: &[u8], document: bool, warn: &mut dyn FnMut(String)) -> Result<(), TypeError> {
    let report = Rc::new(RefCell::new(Report::default()));
    let result = xml_parse(text, document, &report);
    for warning in std::mem::take(&mut report.borrow_mut().warnings) {
        warn(warning);
    }
    result
}

/// The encoding name in the XML declaration of the bytes of `xml_recv`, if there is one. A declaration that is not valid has no encoding here, because `xml_recv` does not check it.
#[must_use]
pub fn xml_encoding(bytes: &[u8]) -> Option<String> {
    parse_xml_decl(bytes).ok()?.encoding.map(|e| String::from_utf8_lossy(e).into_owned())
}

/// `xml_out_internal` for a client in UTF-8: the text with a declaration only if it has a version that is not 1.0 or a standalone part. The encoding is not written. If the declaration does not parse, the text is written as it is and the result is the warning `XX001` of PostgreSQL.
pub fn xml_out(text: &str, out: &mut Vec<u8>) -> Option<TypeError> {
    let bytes = text.as_bytes();
    match parse_xml_decl(bytes) {
        Ok(decl) => {
            let mut len = decl.len;
            if !print_xml_decl(out, decl.version, decl.standalone) && at(bytes, len) == b'\n' {
                len += 1;
            }
            out.extend_from_slice(&bytes[len..]);
            None
        }
        Err(code) => {
            out.extend_from_slice(bytes);
            let mut error = TypeError::new(
                SqlState::DATA_CORRUPTED,
                "could not parse XML declaration in stored value".to_string(),
            );
            error.detail = Some(code.detail().to_string());
            Some(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str, document: bool) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        xml_in(text.as_bytes(), document, &mut |w| warnings.push(w))
            .map(|()| warnings)
            .map_err(|e| format!("{}: {}", e.message, e.detail.unwrap_or_default()))
    }

    fn out(text: &str) -> String {
        let mut buf = Vec::new();
        assert!(xml_out(text, &mut buf).is_none());
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn declarations() {
        assert_eq!(parse_xml_decl(b"<a/>").map(|d| d.len), Ok(0));
        assert_eq!(parse_xml_decl(b"<?xml-stylesheet?>").map(|d| d.len), Ok(0));
        assert_eq!(parse_xml_decl(b"<?xmlversion='1.0'?>").map(|d| d.len), Ok(0));
        assert_eq!(parse_xml_decl(b"<?xml version='1.0'?>").map(|d| d.len), Ok(21));
        assert_eq!(parse_xml_decl(b"<?xml?>").map(|d| d.len), Err(DeclError::SpaceRequired));
        assert_eq!(parse_xml_decl(b"<?xml ?>").map(|d| d.len), Err(DeclError::VersionMissing));
        assert_eq!(
            parse_xml_decl(b"<?xml version='1.0'encoding='x'?>").map(|d| d.len),
            Err(DeclError::SpaceRequired)
        );
        assert_eq!(
            parse_xml_decl(b"<?xml version='1.0' standalone='maybe'?>").map(|d| d.len),
            Err(DeclError::StandaloneValue)
        );
        assert_eq!(
            parse_xml_decl(b"<?xml version='1.0'").map(|d| d.len),
            Err(DeclError::XmldeclNotFinished)
        );
        assert_eq!(
            parse_xml_decl("<?xml version='\u{e9}'?>".as_bytes()).map(|d| d.len),
            Err(DeclError::InvalidChar)
        );
        assert_eq!(
            xml_encoding(b"<?xml version='1.0' encoding=\"LATIN1\"?>").as_deref(),
            Some("LATIN1")
        );
    }

    #[test]
    fn input() {
        assert_eq!(check("<a/>", false), Ok(vec![]));
        assert_eq!(check("", false), Ok(vec![]));
        assert_eq!(check("text <b>x</b> &amp;", false), Ok(vec![]));
        assert!(check("<a", false).is_err_and(|e| e.starts_with("invalid XML content: ")));
        assert!(check("<a/>", true).is_ok());
        assert!(check("a", true).is_err_and(|e| e.starts_with("invalid XML document: ")));
        assert!(check("<!DOCTYPE a><a/>", false).is_ok());
        assert!(check("<?xml version=\"1.1\"?><a/>", true).is_ok_and(|w| w.len() == 1));
    }

    #[test]
    fn output() {
        assert_eq!(out("<a/>"), "<a/>");
        assert_eq!(out("<?xml version=\"1.0\"?>\n<a/>"), "<a/>");
        assert_eq!(
            out("<?xml version='1.1' encoding='UTF-8'?><a/>"),
            "<?xml version=\"1.1\"?><a/>"
        );
        assert_eq!(
            out("<?xml version='1.0' standalone='no'?><a/>"),
            "<?xml version=\"1.0\" standalone=\"no\"?><a/>"
        );
    }
}

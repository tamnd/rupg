//! The URI checks of the XML parser, from `uri.c` of libxml2 2.9.14.
//!
//! The parser only needs to know if a system literal is a valid RFC 3986 reference and if it has a fragment. The parts of the URI are not kept.

use super::ctxt::{at, cstr};

/// The facts that the parser uses from a parsed URI.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Uri {
    pub(super) scheme: bool,
    pub(super) fragment: bool,
}

/// `xmlParseURI`. It returns `None` for a string that is not a valid URI reference.
pub(super) fn parse(s: &[u8]) -> Option<Uri> {
    let s = cstr(s);
    let mut p = Parser { s, uri: Uri::default() };
    if p.uri_absolute(0) == 0 {
        return Some(p.uri);
    }
    p.uri = Uri::default();
    if p.relative_ref(0) == 0 {
        return Some(p.uri);
    }
    None
}

/// `xmlBuildURI` with no base. The result is `None` for an empty or an invalid reference. Otherwise the parser only checks that it is set, so the reference itself is returned.
pub(super) fn build(s: &[u8]) -> Option<Vec<u8>> {
    let s = cstr(s);
    if s.is_empty() {
        return None;
    }
    parse(s)?;
    Some(s.to_vec())
}

struct Parser<'a> {
    s: &'a [u8],
    uri: Uri,
}

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_hexdig(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn is_sub_delim(c: u8) -> bool {
    matches!(c, b'!' | b'$' | b'&' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b'\'')
}

fn is_unreserved(c: u8) -> bool {
    is_alpha(c) || is_digit(c) || matches!(c, b'-' | b'.' | b'_' | b'~')
}

impl Parser<'_> {
    fn c(&self, i: usize) -> u8 {
        at(self.s, i)
    }

    fn pct_encoded(&self, i: usize) -> bool {
        self.c(i) == b'%' && is_hexdig(self.c(i + 1)) && is_hexdig(self.c(i + 2))
    }

    fn pchar(&self, i: usize) -> bool {
        let c = self.c(i);
        is_unreserved(c) || self.pct_encoded(i) || is_sub_delim(c) || c == b':' || c == b'@'
    }

    /// The `NEXT` macro of `uri.c`.
    fn next(&self, i: usize) -> usize {
        if self.c(i) == b'%' { i + 3 } else { i + 1 }
    }

    /// `xmlParse3986Scheme`.
    fn scheme(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        if !is_alpha(self.c(cur)) {
            return 2;
        }
        cur += 1;
        while is_alpha(self.c(cur))
            || is_digit(self.c(cur))
            || matches!(self.c(cur), b'+' | b'-' | b'.')
        {
            cur += 1;
        }
        self.uri.scheme = true;
        *i = cur;
        0
    }

    /// `xmlParse3986Fragment`.
    fn fragment(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        while self.pchar(cur) || matches!(self.c(cur), b'/' | b'?' | b'[' | b']') {
            cur = self.next(cur);
        }
        self.uri.fragment = true;
        *i = cur;
        0
    }

    /// `xmlParse3986Query`.
    fn query(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        while self.pchar(cur) || matches!(self.c(cur), b'/' | b'?') {
            cur = self.next(cur);
        }
        *i = cur;
        0
    }

    /// `xmlParse3986Port`.
    fn port(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        let mut port: i32 = 0;
        if !is_digit(self.c(cur)) {
            return 1;
        }
        while is_digit(self.c(cur)) {
            let digit = i32::from(self.c(cur) - b'0');
            if port > i32::MAX / 10 {
                return 1;
            }
            port *= 10;
            if port > i32::MAX - digit {
                return 1;
            }
            port += digit;
            cur += 1;
        }
        *i = cur;
        0
    }

    /// `xmlParse3986Userinfo`.
    fn userinfo(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        while is_unreserved(self.c(cur))
            || self.pct_encoded(cur)
            || is_sub_delim(self.c(cur))
            || self.c(cur) == b':'
        {
            cur = self.next(cur);
        }
        if self.c(cur) == b'@' {
            *i = cur;
            return 0;
        }
        1
    }

    /// `xmlParse3986DecOctet`, with the checks of libxml2 as they are.
    fn dec_octet(&self, i: &mut usize) -> i32 {
        let mut cur = *i;
        let c = |k: usize| self.c(cur + k);
        if !is_digit(c(0)) {
            return 1;
        }
        if !is_digit(c(1)) {
            cur += 1;
        } else if c(0) != b'0' && is_digit(c(1)) && !is_digit(c(2)) {
            cur += 2;
        } else if (c(0) == b'1' && is_digit(c(1)) && is_digit(c(2)))
            || (c(0) == b'2' && (b'0'..=b'4').contains(&c(1)) && is_digit(c(2)))
            || (c(0) == b'2' && c(1) == b'5' && c(2) >= b'0' && c(1) <= b'5')
        {
            cur += 3;
        } else {
            return 1;
        }
        *i = cur;
        0
    }

    /// The IPv4 form of `xmlParse3986Host`. As in libxml2, the third octet starts at the dot, so this form never matches.
    fn ipv4(&self, start: usize) -> Option<usize> {
        let mut cur = start;
        if self.dec_octet(&mut cur) != 0 || self.c(cur) != b'.' {
            return None;
        }
        cur += 1;
        if self.dec_octet(&mut cur) != 0 || self.c(cur) != b'.' {
            return None;
        }
        if self.dec_octet(&mut cur) != 0 || self.c(cur) != b'.' {
            return None;
        }
        if self.dec_octet(&mut cur) != 0 {
            return None;
        }
        Some(cur)
    }

    /// `xmlParse3986Host`.
    fn host(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        if self.c(cur) == b'[' {
            cur += 1;
            while self.c(cur) != b']' && self.c(cur) != 0 {
                cur += 1;
            }
            if self.c(cur) != b']' {
                return 1;
            }
            *i = cur + 1;
            return 0;
        }
        if is_digit(self.c(cur))
            && let Some(end) = self.ipv4(cur)
        {
            *i = end;
            return 0;
        }
        while is_unreserved(self.c(cur)) || self.pct_encoded(cur) || is_sub_delim(self.c(cur)) {
            cur = self.next(cur);
        }
        *i = cur;
        0
    }

    /// `xmlParse3986Authority`.
    fn authority(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        if self.userinfo(&mut cur) != 0 || self.c(cur) != b'@' {
            cur = *i;
        } else {
            cur += 1;
        }
        let ret = self.host(&mut cur);
        if ret != 0 {
            return ret;
        }
        if self.c(cur) == b':' {
            cur += 1;
            let ret = self.port(&mut cur);
            if ret != 0 {
                return ret;
            }
        }
        *i = cur;
        0
    }

    /// `xmlParse3986Segment`.
    fn segment(&self, i: &mut usize, forbid: u8, empty: bool) -> i32 {
        let mut cur = *i;
        if !self.pchar(cur) {
            return if empty { 0 } else { 1 };
        }
        while self.pchar(cur) && self.c(cur) != forbid {
            cur = self.next(cur);
        }
        *i = cur;
        0
    }

    /// The `*( "/" segment )` loop of the path forms.
    fn more_segments(&self, cur: &mut usize) -> i32 {
        while self.c(*cur) == b'/' {
            *cur += 1;
            let ret = self.segment(cur, 0, true);
            if ret != 0 {
                return ret;
            }
        }
        0
    }

    /// `xmlParse3986PathAbsolute`.
    fn path_absolute(&self, i: &mut usize) -> i32 {
        let mut cur = *i;
        if self.c(cur) != b'/' {
            return 1;
        }
        cur += 1;
        if self.segment(&mut cur, 0, false) == 0 {
            let ret = self.more_segments(&mut cur);
            if ret != 0 {
                return ret;
            }
        }
        *i = cur;
        0
    }

    /// `xmlParse3986PathRootless` and `xmlParse3986PathNoScheme`.
    fn path_first(&self, i: &mut usize, forbid: u8) -> i32 {
        let mut cur = *i;
        let ret = self.segment(&mut cur, forbid, false);
        if ret != 0 {
            return ret;
        }
        let ret = self.more_segments(&mut cur);
        if ret != 0 {
            return ret;
        }
        *i = cur;
        0
    }

    /// `xmlParse3986HierPart`.
    fn hier_part(&mut self, i: &mut usize) -> i32 {
        let mut cur = *i;
        if self.c(cur) == b'/' && self.c(cur + 1) == b'/' {
            cur += 2;
            let ret = self.authority(&mut cur);
            if ret != 0 {
                return ret;
            }
            let ret = self.more_segments(&mut cur);
            if ret != 0 {
                return ret;
            }
        } else if self.c(cur) == b'/' {
            let ret = self.path_absolute(&mut cur);
            if ret != 0 {
                return ret;
            }
        } else if self.pchar(cur) {
            let ret = self.path_first(&mut cur, 0);
            if ret != 0 {
                return ret;
            }
        }
        *i = cur;
        0
    }

    /// The optional query and fragment, and the end of the string.
    fn tail(&mut self, mut i: usize) -> i32 {
        if self.c(i) == b'?' {
            i += 1;
            let ret = self.query(&mut i);
            if ret != 0 {
                return ret;
            }
        }
        if self.c(i) == b'#' {
            i += 1;
            let ret = self.fragment(&mut i);
            if ret != 0 {
                return ret;
            }
        }
        if self.c(i) != 0 {
            return 1;
        }
        0
    }

    /// `xmlParse3986URI`.
    fn uri_absolute(&mut self, mut i: usize) -> i32 {
        let ret = self.scheme(&mut i);
        if ret != 0 {
            return ret;
        }
        if self.c(i) != b':' {
            return 1;
        }
        i += 1;
        let ret = self.hier_part(&mut i);
        if ret != 0 {
            return ret;
        }
        self.tail(i)
    }

    /// `xmlParse3986RelativeRef`.
    fn relative_ref(&mut self, mut i: usize) -> i32 {
        if self.c(i) == b'/' && self.c(i + 1) == b'/' {
            i += 2;
            let ret = self.authority(&mut i);
            if ret != 0 {
                return ret;
            }
            let ret = self.more_segments(&mut i);
            if ret != 0 {
                return ret;
            }
        } else if self.c(i) == b'/' {
            let ret = self.path_absolute(&mut i);
            if ret != 0 {
                return ret;
            }
        } else if self.pchar(i) {
            let ret = self.path_first(&mut i, b':');
            if ret != 0 {
                return ret;
            }
        }
        self.tail(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        assert!(parse(b"http://example.com/a.dtd").is_some_and(|u| u.scheme && !u.fragment));
        assert!(parse(b"a.dtd#frag").is_some_and(|u| u.fragment));
        assert!(parse(b"a b").is_none());
        assert!(parse(b"%zz").is_none());
        assert!(parse(b"").is_some());
        assert!(build(b"").is_none());
        assert!(build(b"x.dtd").is_some());
        assert!(build(b"<x>").is_none());
    }
}

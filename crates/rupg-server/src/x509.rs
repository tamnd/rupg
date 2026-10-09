//! The subject of a client certificate, as PostgreSQL gets it from OpenSSL: the first common name, `peer_cn`, and the distinguished name in the RFC 2253 form of `X509_NAME_print_ex`, `peer_dn`. The `cert` method and `clientcert=verify-full` compare one of them with the user.
//!
//! The table of the sessions shows the subject and the issuer in the form of `X509_NAME_to_cstring` of PostgreSQL, and the serial number in decimal, as `BN_bn2dec` gives it. That form has `/`, the short name and `=` before each field, in the order of the certificate. A field with a type that has no short name in this module shows its type as the dotted numbers.
//!
//! The certificate is DER, and the TLS library already checked it, so this module only walks to the subject. The printer is a port of `do_name_ex`, `do_print_ex` and `do_buf` of OpenSSL with the flags of `XN_FLAG_RFC2253`: the fields come in reverse order, `,` separates the relative names and `+` the parts of one, a field has its short name, and a field with an unknown name or an unknown string type is `#` and the hex of its DER.
//!
//! Lifted from `crates/rudb-server/src/x509.rs` of tamnd/rudb at f5f7065a.

/// The subject of a client certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subject {
    /// The value of the first `commonName`, as its bytes in the certificate.
    pub(crate) cn: Option<String>,
    /// The subject in the RFC 2253 form of OpenSSL.
    pub(crate) dn: String,
}

/// The client certificate as the table of the sessions shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    /// The subject in the form of `X509_NAME_to_cstring`.
    pub(crate) subject: String,
    /// The serial number in decimal.
    pub(crate) serial: String,
    /// The issuer in the form of `X509_NAME_to_cstring`.
    pub(crate) issuer: String,
}

/// The short names of OpenSSL for the attribute types of a subject, from `objects.txt`.
const NAMES: &[(&str, &str)] = &[
    ("0.9.2342.19200300.100.1.1", "UID"),
    ("0.9.2342.19200300.100.1.2", "textEncodedORAddress"),
    ("0.9.2342.19200300.100.1.3", "mail"),
    ("0.9.2342.19200300.100.1.4", "info"),
    ("0.9.2342.19200300.100.1.5", "favouriteDrink"),
    ("0.9.2342.19200300.100.1.6", "roomNumber"),
    ("0.9.2342.19200300.100.1.7", "photo"),
    ("0.9.2342.19200300.100.1.8", "userClass"),
    ("0.9.2342.19200300.100.1.9", "host"),
    ("0.9.2342.19200300.100.1.10", "manager"),
    ("0.9.2342.19200300.100.1.11", "documentIdentifier"),
    ("0.9.2342.19200300.100.1.12", "documentTitle"),
    ("0.9.2342.19200300.100.1.13", "documentVersion"),
    ("0.9.2342.19200300.100.1.14", "documentAuthor"),
    ("0.9.2342.19200300.100.1.15", "documentLocation"),
    ("0.9.2342.19200300.100.1.20", "homeTelephoneNumber"),
    ("0.9.2342.19200300.100.1.21", "secretary"),
    ("0.9.2342.19200300.100.1.22", "otherMailbox"),
    ("0.9.2342.19200300.100.1.23", "lastModifiedTime"),
    ("0.9.2342.19200300.100.1.24", "lastModifiedBy"),
    ("0.9.2342.19200300.100.1.25", "DC"),
    ("0.9.2342.19200300.100.1.26", "aRecord"),
    ("0.9.2342.19200300.100.1.27", "pilotAttributeType27"),
    ("0.9.2342.19200300.100.1.28", "mXRecord"),
    ("0.9.2342.19200300.100.1.29", "nSRecord"),
    ("0.9.2342.19200300.100.1.30", "sOARecord"),
    ("0.9.2342.19200300.100.1.31", "cNAMERecord"),
    ("0.9.2342.19200300.100.1.37", "associatedDomain"),
    ("0.9.2342.19200300.100.1.38", "associatedName"),
    ("0.9.2342.19200300.100.1.39", "homePostalAddress"),
    ("0.9.2342.19200300.100.1.40", "personalTitle"),
    ("0.9.2342.19200300.100.1.41", "mobileTelephoneNumber"),
    ("0.9.2342.19200300.100.1.42", "pagerTelephoneNumber"),
    ("0.9.2342.19200300.100.1.43", "friendlyCountryName"),
    ("0.9.2342.19200300.100.1.44", "uid"),
    ("0.9.2342.19200300.100.1.45", "organizationalStatus"),
    ("0.9.2342.19200300.100.1.46", "janetMailbox"),
    ("0.9.2342.19200300.100.1.47", "mailPreferenceOption"),
    ("0.9.2342.19200300.100.1.48", "buildingName"),
    ("0.9.2342.19200300.100.1.49", "dSAQuality"),
    ("0.9.2342.19200300.100.1.50", "singleLevelQuality"),
    ("0.9.2342.19200300.100.1.51", "subtreeMinimumQuality"),
    ("0.9.2342.19200300.100.1.52", "subtreeMaximumQuality"),
    ("0.9.2342.19200300.100.1.53", "personalSignature"),
    ("0.9.2342.19200300.100.1.54", "dITRedirect"),
    ("0.9.2342.19200300.100.1.55", "audio"),
    ("0.9.2342.19200300.100.1.56", "documentPublisher"),
    ("1.2.840.113549.1.9.1", "emailAddress"),
    ("1.2.840.113549.1.9.2", "unstructuredName"),
    ("1.2.840.113549.1.9.3", "contentType"),
    ("1.2.840.113549.1.9.4", "messageDigest"),
    ("1.2.840.113549.1.9.5", "signingTime"),
    ("1.2.840.113549.1.9.6", "countersignature"),
    ("1.2.840.113549.1.9.7", "challengePassword"),
    ("1.2.840.113549.1.9.8", "unstructuredAddress"),
    ("1.2.840.113549.1.9.9", "extendedCertificateAttributes"),
    ("1.2.840.113549.1.9.14", "extReq"),
    ("1.2.840.113549.1.9.15", "SMIME-CAPS"),
    ("1.2.840.113549.1.9.16", "SMIME"),
    ("1.2.840.113549.1.9.20", "friendlyName"),
    ("1.2.840.113549.1.9.21", "localKeyID"),
    ("1.2.840.113549.1.9.52", "id-aa-CMSAlgorithmProtection"),
    ("1.3.6.1.4.1.311.60.2.1.1", "jurisdictionL"),
    ("1.3.6.1.4.1.311.60.2.1.2", "jurisdictionST"),
    ("1.3.6.1.4.1.311.60.2.1.3", "jurisdictionC"),
    ("2.5.4.3", "CN"),
    ("2.5.4.4", "SN"),
    ("2.5.4.5", "serialNumber"),
    ("2.5.4.6", "C"),
    ("2.5.4.7", "L"),
    ("2.5.4.8", "ST"),
    ("2.5.4.9", "street"),
    ("2.5.4.10", "O"),
    ("2.5.4.11", "OU"),
    ("2.5.4.12", "title"),
    ("2.5.4.13", "description"),
    ("2.5.4.14", "searchGuide"),
    ("2.5.4.15", "businessCategory"),
    ("2.5.4.16", "postalAddress"),
    ("2.5.4.17", "postalCode"),
    ("2.5.4.18", "postOfficeBox"),
    ("2.5.4.19", "physicalDeliveryOfficeName"),
    ("2.5.4.20", "telephoneNumber"),
    ("2.5.4.21", "telexNumber"),
    ("2.5.4.22", "teletexTerminalIdentifier"),
    ("2.5.4.23", "facsimileTelephoneNumber"),
    ("2.5.4.24", "x121Address"),
    ("2.5.4.25", "internationaliSDNNumber"),
    ("2.5.4.26", "registeredAddress"),
    ("2.5.4.27", "destinationIndicator"),
    ("2.5.4.28", "preferredDeliveryMethod"),
    ("2.5.4.29", "presentationAddress"),
    ("2.5.4.30", "supportedApplicationContext"),
    ("2.5.4.31", "member"),
    ("2.5.4.32", "owner"),
    ("2.5.4.33", "roleOccupant"),
    ("2.5.4.34", "seeAlso"),
    ("2.5.4.35", "userPassword"),
    ("2.5.4.36", "userCertificate"),
    ("2.5.4.37", "cACertificate"),
    ("2.5.4.38", "authorityRevocationList"),
    ("2.5.4.39", "certificateRevocationList"),
    ("2.5.4.40", "crossCertificatePair"),
    ("2.5.4.41", "name"),
    ("2.5.4.42", "GN"),
    ("2.5.4.43", "initials"),
    ("2.5.4.44", "generationQualifier"),
    ("2.5.4.45", "x500UniqueIdentifier"),
    ("2.5.4.46", "dnQualifier"),
    ("2.5.4.47", "enhancedSearchGuide"),
    ("2.5.4.48", "protocolInformation"),
    ("2.5.4.49", "distinguishedName"),
    ("2.5.4.50", "uniqueMember"),
    ("2.5.4.51", "houseIdentifier"),
    ("2.5.4.52", "supportedAlgorithms"),
    ("2.5.4.53", "deltaRevocationList"),
    ("2.5.4.54", "dmdName"),
    ("2.5.4.65", "pseudonym"),
    ("2.5.4.72", "role"),
    ("2.5.4.97", "organizationIdentifier"),
    ("2.5.4.98", "c3"),
    ("2.5.4.99", "n3"),
    ("2.5.4.100", "dnsName"),
];

/// One field of a subject: its type, its value as DER, and the relative name that holds it.
struct Field<'a> {
    oid: &'a [u8],
    value: Tlv<'a>,
    set: usize,
}

/// One DER element.
#[derive(Clone, Copy)]
struct Tlv<'a> {
    /// The class and the constructed bit of the first byte.
    class: u8,
    /// The number of the tag.
    number: u32,
    content: &'a [u8],
    /// The whole element: the tag, the length and the content.
    whole: &'a [u8],
}

const UNIVERSAL: u8 = 0x00;
const CONSTRUCTED: u8 = 0x20;
const SEQUENCE: u32 = 16;
const SET: u32 = 17;
const OID: u32 = 6;
const INTEGER: u32 = 2;

/// Reads one DER element from the start of `input`, and gives the rest.
fn tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let (&first, mut rest) = input.split_first()?;
    let mut number = u32::from(first & 0x1f);
    if number == 0x1f {
        number = 0;
        loop {
            let (&byte, next) = rest.split_first()?;
            rest = next;
            number = number.checked_mul(128)? | u32::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
    }
    let (&length, mut rest) = rest.split_first()?;
    let length = if length & 0x80 == 0 {
        usize::from(length)
    } else {
        let count = usize::from(length & 0x7f);
        if count == 0 || count > size_of::<usize>() || rest.len() < count {
            return None;
        }
        let (bytes, next) = rest.split_at(count);
        rest = next;
        bytes.iter().fold(0usize, |length, &byte| length << 8 | usize::from(byte))
    };
    if rest.len() < length {
        return None;
    }
    let (content, rest) = rest.split_at(length);
    let whole = &input[..input.len() - rest.len()];
    Some((Tlv { class: first & 0xe0, number, content, whole }, rest))
}

/// Reads one element with the given universal tag.
fn expect(input: &[u8], constructed: bool, number: u32) -> Option<(Tlv<'_>, &[u8])> {
    let (element, rest) = tlv(input)?;
    let class = UNIVERSAL | if constructed { CONSTRUCTED } else { 0 };
    (element.class == class && element.number == number).then_some((element, rest))
}

/// The subject of a certificate in DER. `Err` holds the text of the log line of PostgreSQL, or `None` when the certificate cannot be read, and then the connection ends with no log line.
pub(crate) fn subject(certificate: &[u8]) -> Result<Subject, Option<&'static str>> {
    let fields = fields(certificate).ok_or(None)?;
    let cn = fields.iter().find(|field| oid_text(field.oid) == "2.5.4.3");
    let cn = match cn {
        Some(field) if field.value.content.contains(&0) => {
            return Err(Some("SSL certificate's common name contains embedded null"));
        }
        Some(field) => Some(String::from_utf8_lossy(field.value.content).into_owned()),
        None => None,
    };
    Ok(Subject { cn, dn: distinguished_name(&fields).ok_or(None)? })
}

/// The client certificate as the table of the sessions shows it, or `None` when the certificate cannot be read.
pub(crate) fn names(certificate: &[u8]) -> Option<Names> {
    let parts = parts(certificate)?;
    Some(Names {
        subject: slashed(&fields_of(parts.subject)?),
        serial: decimal(parts.serial.content),
        issuer: slashed(&fields_of(parts.issuer)?),
    })
}

/// The parts of a certificate that this module reads.
struct Parts<'a> {
    serial: Tlv<'a>,
    issuer: Tlv<'a>,
    subject: Tlv<'a>,
}

/// Walks to the serial number, the issuer and the subject of a certificate in DER.
fn parts(certificate: &[u8]) -> Option<Parts<'_>> {
    let (certificate, _) = expect(certificate, true, SEQUENCE)?;
    let (tbs, _) = expect(certificate.content, true, SEQUENCE)?;
    let mut rest = tbs.content;
    // The version is `[0] EXPLICIT` and can be left out.
    let (first, next) = tlv(rest)?;
    if first.class == 0x80 | CONSTRUCTED && first.number == 0 {
        rest = next;
    }
    let (serial, rest) = expect(rest, false, INTEGER)?;
    // The signature algorithm.
    let rest = tlv(rest)?.1;
    let (issuer, rest) = expect(rest, true, SEQUENCE)?;
    // The validity.
    let rest = tlv(rest)?.1;
    let (subject, _) = expect(rest, true, SEQUENCE)?;
    Some(Parts { serial, issuer, subject })
}

/// The fields of the subject, in the order of the certificate.
fn fields(certificate: &[u8]) -> Option<Vec<Field<'_>>> {
    fields_of(parts(certificate)?.subject)
}

/// The fields of a name, in the order of the certificate.
fn fields_of(name: Tlv<'_>) -> Option<Vec<Field<'_>>> {
    let mut fields = Vec::new();
    let mut names = name.content;
    let mut set = 0;
    while !names.is_empty() {
        let (name, next) = expect(names, true, SET)?;
        names = next;
        let mut parts = name.content;
        while !parts.is_empty() {
            let (part, next) = expect(parts, true, SEQUENCE)?;
            parts = next;
            let (oid, rest) = expect(part.content, false, OID)?;
            let (value, rest) = tlv(rest)?;
            if !rest.is_empty() {
                return None;
            }
            fields.push(Field { oid: oid.content, value, set });
        }
        set += 1;
    }
    Some(fields)
}

/// The dotted text of an object identifier, as `OBJ_obj2txt` with numbers only.
fn oid_text(oid: &[u8]) -> String {
    let mut arcs = Vec::new();
    let mut arc: u128 = 0;
    for &byte in oid {
        arc = arc << 7 | u128::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            arcs.push(arc);
            arc = 0;
        }
    }
    let mut text = String::new();
    for (i, arc) in arcs.into_iter().enumerate() {
        if i == 0 {
            let first = arc.min(80) / 40;
            text.push_str(&format!("{first}.{}", arc - first * 40));
        } else {
            text.push_str(&format!(".{arc}"));
        }
    }
    text
}

/// The short name of OpenSSL for a field type, or the dotted numbers.
fn short_name(oid: &[u8]) -> String {
    let oid = oid_text(oid);
    NAMES.iter().find(|(dotted, _)| *dotted == oid).map_or(oid, |(_, name)| (*name).to_owned())
}

/// `X509_NAME_to_cstring` of PostgreSQL: each field as `/`, its short name, `=` and its value as `ASN1_STRING_print_ex` prints it with the flags of RFC 2253 but not `ASN1_STRFLGS_ESC_MSB`. A value that cannot be printed is dumped.
fn slashed(fields: &[Field<'_>]) -> String {
    let mut out = Vec::new();
    for field in fields {
        out.push(b'/');
        out.extend_from_slice(short_name(field.oid).as_bytes());
        out.push(b'=');
        let at = out.len();
        if value(&mut out, field.value, false).is_none() {
            out.truncate(at);
            dump(&mut out, field.value.whole);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `BN_bn2dec` of a DER `INTEGER`, which is in two's complement.
fn decimal(bytes: &[u8]) -> String {
    let negative = bytes.first().is_some_and(|&byte| byte & 0x80 != 0);
    let mut magnitude = bytes.to_vec();
    if negative {
        let mut carry = true;
        for byte in magnitude.iter_mut().rev() {
            *byte = !*byte;
            if carry {
                let (sum, over) = byte.overflowing_add(1);
                *byte = sum;
                carry = over;
            }
        }
    }
    let mut digits = Vec::new();
    while magnitude.iter().any(|&byte| byte != 0) {
        let mut rest = 0u16;
        for byte in &mut magnitude {
            let current = rest << 8 | u16::from(*byte);
            *byte = u8::try_from(current / 10).unwrap_or(u8::MAX);
            rest = current % 10;
        }
        digits.push(char::from(b'0' + u8::try_from(rest).unwrap_or(0)));
    }
    if digits.is_empty() {
        digits.push('0');
    }
    if negative {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// `do_name_ex` with `XN_FLAG_RFC2253`.
fn distinguished_name(fields: &[Field<'_>]) -> Option<String> {
    let mut out = Vec::new();
    let mut prev = None;
    for field in fields.iter().rev() {
        match prev {
            Some(set) if set == field.set => out.push(b'+'),
            Some(_) => out.push(b','),
            None => {}
        }
        prev = Some(field.set);
        let oid = oid_text(field.oid);
        match NAMES.iter().find(|(dotted, _)| *dotted == oid) {
            Some((_, name)) => {
                out.extend_from_slice(name.as_bytes());
                out.push(b'=');
                value(&mut out, field.value, true)?;
            }
            None => {
                out.extend_from_slice(oid.as_bytes());
                out.push(b'=');
                dump(&mut out, field.value.whole);
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `do_print_ex` with `ASN1_STRFLGS_RFC2253`: a string type is printed with escapes, and any other type is dumped. `msb` is `ASN1_STRFLGS_ESC_MSB`, the escape of the bytes over 127.
fn value(out: &mut Vec<u8>, value: Tlv<'_>, msb: bool) -> Option<()> {
    // `tag2nbyte`: the width of a character, and whether it goes through UTF-8.
    let width = match (value.class, value.number) {
        (UNIVERSAL, 12) => Some((1, false)),
        (UNIVERSAL, 18..=20 | 22..=24 | 26) => Some((1, true)),
        (UNIVERSAL, 28) => Some((4, true)),
        (UNIVERSAL, 30) => Some((2, true)),
        _ => None,
    };
    match width {
        Some((width, convert)) => string(out, value.content, width, convert, msb),
        None => {
            dump(out, value.whole);
            Some(())
        }
    }
}

/// `do_dump` with `ASN1_STRFLGS_DUMP_DER`: `#` and the hex of the whole DER element.
fn dump(out: &mut Vec<u8>, der: &[u8]) {
    out.push(b'#');
    for byte in der {
        out.extend_from_slice(format!("{byte:02X}").as_bytes());
    }
}

/// The escape of the first character of a value.
const FIRST: u8 = 1;
/// The escape of the last character of a value. A value of one character only has this one, as in OpenSSL.
const LAST: u8 = 2;

/// `do_buf`: the characters of a string of `width` bytes each, as UTF-8 when `convert` is set.
fn string(out: &mut Vec<u8>, bytes: &[u8], width: usize, convert: bool, msb: bool) -> Option<()> {
    if !bytes.len().is_multiple_of(width) {
        return None;
    }
    let count = bytes.len() / width;
    for (i, unit) in bytes.chunks(width).enumerate() {
        let c = unit.iter().fold(0u32, |c, &byte| c << 8 | u32::from(byte));
        let edge = if i + 1 == count {
            LAST
        } else if i == 0 {
            FIRST
        } else {
            0
        };
        if convert {
            let mut buf = [0u8; 4];
            for &byte in char::from_u32(c)?.encode_utf8(&mut buf).as_bytes() {
                escape(out, byte, edge, msb);
            }
        } else {
            escape(out, u8::try_from(c).ok()?, edge, msb);
        }
    }
    Some(())
}

/// `do_esc_char` with the escapes of RFC 2253, of control characters, and of bytes over 127 when `msb` is set.
fn escape(out: &mut Vec<u8>, c: u8, edge: u8, msb: bool) {
    let backslash = match c {
        b'"' | b'+' | b',' | b';' | b'<' | b'>' | b'\\' => true,
        b' ' => edge != 0,
        b'#' => edge == FIRST,
        _ => false,
    };
    if backslash {
        out.extend_from_slice(&[b'\\', c]);
    } else if c < 0x20 || c == 0x7f || (c > 0x7f && msb) {
        out.extend_from_slice(format!("\\{c:02X}").as_bytes());
    } else {
        out.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DER element with a short or a long length.
    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match content.len() {
            n if n < 128 => out.push(u8::try_from(n).unwrap()),
            n if n < 256 => out.extend_from_slice(&[0x81, u8::try_from(n).unwrap()]),
            n => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
        }
        out.extend_from_slice(content);
        out
    }

    /// One field: the OID content, the string tag and the value.
    type Field<'a> = (&'a [u8], u8, &'a [u8]);

    /// A certificate with the given relative names and an empty signature.
    fn certificate(names: &[&[Field<'_>]]) -> Vec<u8> {
        let mut subject = Vec::new();
        for name in names {
            let mut set = Vec::new();
            for (oid, tag, value) in *name {
                let mut part = der(0x06, oid);
                part.extend(der(*tag, value));
                set.extend(der(0x30, &part));
            }
            subject.extend(der(0x31, &set));
        }
        let mut tbs = der(0xa0, &der(0x02, &[2]));
        tbs.extend(der(0x02, &[1]));
        tbs.extend(der(0x30, &der(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02])));
        tbs.extend(der(0x30, &[]));
        tbs.extend(der(0x30, &[]));
        tbs.extend(der(0x30, &subject));
        let mut certificate = der(0x30, &tbs);
        certificate.extend(der(0x30, &[]));
        certificate.extend(der(0x03, &[0]));
        der(0x30, &certificate)
    }

    const CN: &[u8] = &[0x55, 0x04, 0x03];
    const O: &[u8] = &[0x55, 0x04, 0x0a];
    const OU: &[u8] = &[0x55, 0x04, 0x0b];
    const DC: &[u8] = &[0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x19];
    const UNKNOWN: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x81, 0x19, 0x01];

    #[test]
    fn the_order_and_the_names_of_openssl() {
        let certificate = certificate(&[
            &[(DC, 0x16, b"org")],
            &[(O, 0x0c, b"Acme, Inc.")],
            &[(OU, 0x13, b"dev"), (CN, 0x0c, b"alice")],
        ]);
        let subject = subject(&certificate).unwrap();
        assert_eq!(subject.cn.as_deref(), Some("alice"));
        assert_eq!(subject.dn, "CN=alice+OU=dev,O=Acme\\, Inc.,DC=org");
    }

    #[test]
    fn the_escapes_of_rfc_2253() {
        // The values go in `O`, as a common name with a zero byte is refused.
        let dn = |value: &[u8], tag: u8| subject(&certificate(&[&[(O, tag, value)]])).unwrap().dn;
        assert_eq!(dn(b" a b ", 0x0c), "O=\\ a b\\ ");
        assert_eq!(dn(b"#a#", 0x0c), "O=\\#a#");
        assert_eq!(dn(b"#", 0x0c), "O=#");
        assert_eq!(dn(b" ", 0x0c), "O=\\ ");
        assert_eq!(dn(b"a\"+;<>\\=b", 0x0c), "O=a\\\"\\+\\;\\<\\>\\\\=b");
        assert_eq!(dn(b"a\tb\x7f", 0x0c), "O=a\\09b\\7F");
        assert_eq!(dn("é".as_bytes(), 0x0c), "O=\\C3\\A9");
        // A T.61 byte over 127 is a character, so it becomes two bytes of UTF-8.
        assert_eq!(dn(b"\xe9", 0x14), "O=\\C3\\A9");
        assert_eq!(dn(&[0x00, 0xe9, 0x00, 0x61], 0x1e), "O=\\C3\\A9a");
        assert_eq!(dn(&[0, 0, 0, 0x61], 0x1c), "O=a");
        // A type that is not a string is dumped.
        assert_eq!(dn(&[0x05], 0x02), "O=#020105");
    }

    #[test]
    fn an_unknown_field_is_dumped() {
        let certificate = certificate(&[&[(UNKNOWN, 0x0c, b"x")], &[(CN, 0x0c, b"bob")]]);
        let subject = subject(&certificate).unwrap();
        assert_eq!(subject.dn, "CN=bob,1.3.6.1.4.1.311.153.1=#0C0178");
    }

    #[test]
    fn the_common_name_with_a_null_or_none() {
        let null = certificate(&[&[(CN, 0x0c, b"a\0b")]]);
        assert_eq!(
            subject(&null),
            Err(Some("SSL certificate's common name contains embedded null"))
        );
        let other = certificate(&[&[(O, 0x0c, b"x")]]);
        assert_eq!(subject(&other), Ok(Subject { cn: None, dn: "O=x".to_owned() }));
        assert_eq!(subject(&[0x30, 0x03, 0x30]), Err(None));
    }

    #[test]
    fn the_serial_in_decimal() {
        assert_eq!(decimal(&[0x00]), "0");
        assert_eq!(decimal(&[0x01, 0x00]), "256");
        assert_eq!(decimal(&[0x00, 0xff]), "255");
        assert_eq!(decimal(&[0xff]), "-1");
        assert_eq!(decimal(&[0x80]), "-128");
        assert_eq!(decimal(&[0xfe, 0xe0, 0x8e, 0x04, 0xfb, 0x35]), "-1234567890123");
    }
}

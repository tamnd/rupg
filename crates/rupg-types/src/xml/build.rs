//! The XML text that the SQL/XML functions of `xml.c` build: the names of elements and attributes from SQL names, the escapes of text, and the declarations of `XMLCONCAT` and `XMLROOT`.

use rupg_common::SqlState;

use super::{parse_xml_decl, print_xml_decl};
use crate::TypeError;

/// The code points that `is_valid_xml_namefirst` takes: `Letter`, `_` and `:` of XML 1.0, fourth edition, as the tables of libxml2 have them.
const NAME_FIRST: &[(u32, u32)] = &[
    (0x003A, 0x003A),
    (0x0041, 0x005A),
    (0x005F, 0x005F),
    (0x0061, 0x007A),
    (0x00C0, 0x00D6),
    (0x00D8, 0x00F6),
    (0x00F8, 0x0131),
    (0x0134, 0x013E),
    (0x0141, 0x0148),
    (0x014A, 0x017E),
    (0x0180, 0x01C3),
    (0x01CD, 0x01F0),
    (0x01F4, 0x01F5),
    (0x01FA, 0x0217),
    (0x0250, 0x02A8),
    (0x02BB, 0x02C1),
    (0x0386, 0x0386),
    (0x0388, 0x038A),
    (0x038C, 0x038C),
    (0x038E, 0x03A1),
    (0x03A3, 0x03CE),
    (0x03D0, 0x03D6),
    (0x03DA, 0x03DA),
    (0x03DC, 0x03DC),
    (0x03DE, 0x03DE),
    (0x03E0, 0x03E0),
    (0x03E2, 0x03F3),
    (0x0401, 0x040C),
    (0x040E, 0x044F),
    (0x0451, 0x045C),
    (0x045E, 0x0481),
    (0x0490, 0x04C4),
    (0x04C7, 0x04C8),
    (0x04CB, 0x04CC),
    (0x04D0, 0x04EB),
    (0x04EE, 0x04F5),
    (0x04F8, 0x04F9),
    (0x0531, 0x0556),
    (0x0559, 0x0559),
    (0x0561, 0x0586),
    (0x05D0, 0x05EA),
    (0x05F0, 0x05F2),
    (0x0621, 0x063A),
    (0x0641, 0x064A),
    (0x0671, 0x06B7),
    (0x06BA, 0x06BE),
    (0x06C0, 0x06CE),
    (0x06D0, 0x06D3),
    (0x06D5, 0x06D5),
    (0x06E5, 0x06E6),
    (0x0905, 0x0939),
    (0x093D, 0x093D),
    (0x0958, 0x0961),
    (0x0985, 0x098C),
    (0x098F, 0x0990),
    (0x0993, 0x09A8),
    (0x09AA, 0x09B0),
    (0x09B2, 0x09B2),
    (0x09B6, 0x09B9),
    (0x09DC, 0x09DD),
    (0x09DF, 0x09E1),
    (0x09F0, 0x09F1),
    (0x0A05, 0x0A0A),
    (0x0A0F, 0x0A10),
    (0x0A13, 0x0A28),
    (0x0A2A, 0x0A30),
    (0x0A32, 0x0A33),
    (0x0A35, 0x0A36),
    (0x0A38, 0x0A39),
    (0x0A59, 0x0A5C),
    (0x0A5E, 0x0A5E),
    (0x0A72, 0x0A74),
    (0x0A85, 0x0A8B),
    (0x0A8D, 0x0A8D),
    (0x0A8F, 0x0A91),
    (0x0A93, 0x0AA8),
    (0x0AAA, 0x0AB0),
    (0x0AB2, 0x0AB3),
    (0x0AB5, 0x0AB9),
    (0x0ABD, 0x0ABD),
    (0x0AE0, 0x0AE0),
    (0x0B05, 0x0B0C),
    (0x0B0F, 0x0B10),
    (0x0B13, 0x0B28),
    (0x0B2A, 0x0B30),
    (0x0B32, 0x0B33),
    (0x0B36, 0x0B39),
    (0x0B3D, 0x0B3D),
    (0x0B5C, 0x0B5D),
    (0x0B5F, 0x0B61),
    (0x0B85, 0x0B8A),
    (0x0B8E, 0x0B90),
    (0x0B92, 0x0B95),
    (0x0B99, 0x0B9A),
    (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F),
    (0x0BA3, 0x0BA4),
    (0x0BA8, 0x0BAA),
    (0x0BAE, 0x0BB5),
    (0x0BB7, 0x0BB9),
    (0x0C05, 0x0C0C),
    (0x0C0E, 0x0C10),
    (0x0C12, 0x0C28),
    (0x0C2A, 0x0C33),
    (0x0C35, 0x0C39),
    (0x0C60, 0x0C61),
    (0x0C85, 0x0C8C),
    (0x0C8E, 0x0C90),
    (0x0C92, 0x0CA8),
    (0x0CAA, 0x0CB3),
    (0x0CB5, 0x0CB9),
    (0x0CDE, 0x0CDE),
    (0x0CE0, 0x0CE1),
    (0x0D05, 0x0D0C),
    (0x0D0E, 0x0D10),
    (0x0D12, 0x0D28),
    (0x0D2A, 0x0D39),
    (0x0D60, 0x0D61),
    (0x0E01, 0x0E2E),
    (0x0E30, 0x0E30),
    (0x0E32, 0x0E33),
    (0x0E40, 0x0E45),
    (0x0E81, 0x0E82),
    (0x0E84, 0x0E84),
    (0x0E87, 0x0E88),
    (0x0E8A, 0x0E8A),
    (0x0E8D, 0x0E8D),
    (0x0E94, 0x0E97),
    (0x0E99, 0x0E9F),
    (0x0EA1, 0x0EA3),
    (0x0EA5, 0x0EA5),
    (0x0EA7, 0x0EA7),
    (0x0EAA, 0x0EAB),
    (0x0EAD, 0x0EAE),
    (0x0EB0, 0x0EB0),
    (0x0EB2, 0x0EB3),
    (0x0EBD, 0x0EBD),
    (0x0EC0, 0x0EC4),
    (0x0F40, 0x0F47),
    (0x0F49, 0x0F69),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3041, 0x3094),
    (0x30A1, 0x30FA),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];
/// The code points that `is_valid_xml_namechar` takes: `Letter`, `Digit`, `.`, `-`, `_`, `:`, `CombiningChar` and `Extender` of XML 1.0, fourth edition.
const NAME_CHAR: &[(u32, u32)] = &[
    (0x002D, 0x002E),
    (0x0030, 0x003A),
    (0x0041, 0x005A),
    (0x005F, 0x005F),
    (0x0061, 0x007A),
    (0x00B7, 0x00B7),
    (0x00C0, 0x00D6),
    (0x00D8, 0x00F6),
    (0x00F8, 0x0131),
    (0x0134, 0x013E),
    (0x0141, 0x0148),
    (0x014A, 0x017E),
    (0x0180, 0x01C3),
    (0x01CD, 0x01F0),
    (0x01F4, 0x01F5),
    (0x01FA, 0x0217),
    (0x0250, 0x02A8),
    (0x02BB, 0x02C1),
    (0x02D0, 0x02D1),
    (0x0300, 0x0345),
    (0x0360, 0x0361),
    (0x0386, 0x038A),
    (0x038C, 0x038C),
    (0x038E, 0x03A1),
    (0x03A3, 0x03CE),
    (0x03D0, 0x03D6),
    (0x03DA, 0x03DA),
    (0x03DC, 0x03DC),
    (0x03DE, 0x03DE),
    (0x03E0, 0x03E0),
    (0x03E2, 0x03F3),
    (0x0401, 0x040C),
    (0x040E, 0x044F),
    (0x0451, 0x045C),
    (0x045E, 0x0481),
    (0x0483, 0x0486),
    (0x0490, 0x04C4),
    (0x04C7, 0x04C8),
    (0x04CB, 0x04CC),
    (0x04D0, 0x04EB),
    (0x04EE, 0x04F5),
    (0x04F8, 0x04F9),
    (0x0531, 0x0556),
    (0x0559, 0x0559),
    (0x0561, 0x0586),
    (0x0591, 0x05A1),
    (0x05A3, 0x05B9),
    (0x05BB, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C4),
    (0x05D0, 0x05EA),
    (0x05F0, 0x05F2),
    (0x0621, 0x063A),
    (0x0640, 0x0652),
    (0x0660, 0x0669),
    (0x0670, 0x06B7),
    (0x06BA, 0x06BE),
    (0x06C0, 0x06CE),
    (0x06D0, 0x06D3),
    (0x06D5, 0x06E8),
    (0x06EA, 0x06ED),
    (0x06F0, 0x06F9),
    (0x0901, 0x0903),
    (0x0905, 0x0939),
    (0x093C, 0x094D),
    (0x0951, 0x0954),
    (0x0958, 0x0963),
    (0x0966, 0x096F),
    (0x0981, 0x0983),
    (0x0985, 0x098C),
    (0x098F, 0x0990),
    (0x0993, 0x09A8),
    (0x09AA, 0x09B0),
    (0x09B2, 0x09B2),
    (0x09B6, 0x09B9),
    (0x09BC, 0x09BC),
    (0x09BE, 0x09C4),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CD),
    (0x09D7, 0x09D7),
    (0x09DC, 0x09DD),
    (0x09DF, 0x09E3),
    (0x09E6, 0x09F1),
    (0x0A02, 0x0A02),
    (0x0A05, 0x0A0A),
    (0x0A0F, 0x0A10),
    (0x0A13, 0x0A28),
    (0x0A2A, 0x0A30),
    (0x0A32, 0x0A33),
    (0x0A35, 0x0A36),
    (0x0A38, 0x0A39),
    (0x0A3C, 0x0A3C),
    (0x0A3E, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A59, 0x0A5C),
    (0x0A5E, 0x0A5E),
    (0x0A66, 0x0A74),
    (0x0A81, 0x0A83),
    (0x0A85, 0x0A8B),
    (0x0A8D, 0x0A8D),
    (0x0A8F, 0x0A91),
    (0x0A93, 0x0AA8),
    (0x0AAA, 0x0AB0),
    (0x0AB2, 0x0AB3),
    (0x0AB5, 0x0AB9),
    (0x0ABC, 0x0AC5),
    (0x0AC7, 0x0AC9),
    (0x0ACB, 0x0ACD),
    (0x0AE0, 0x0AE0),
    (0x0AE6, 0x0AEF),
    (0x0B01, 0x0B03),
    (0x0B05, 0x0B0C),
    (0x0B0F, 0x0B10),
    (0x0B13, 0x0B28),
    (0x0B2A, 0x0B30),
    (0x0B32, 0x0B33),
    (0x0B36, 0x0B39),
    (0x0B3C, 0x0B43),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4D),
    (0x0B56, 0x0B57),
    (0x0B5C, 0x0B5D),
    (0x0B5F, 0x0B61),
    (0x0B66, 0x0B6F),
    (0x0B82, 0x0B83),
    (0x0B85, 0x0B8A),
    (0x0B8E, 0x0B90),
    (0x0B92, 0x0B95),
    (0x0B99, 0x0B9A),
    (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F),
    (0x0BA3, 0x0BA4),
    (0x0BA8, 0x0BAA),
    (0x0BAE, 0x0BB5),
    (0x0BB7, 0x0BB9),
    (0x0BBE, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCD),
    (0x0BD7, 0x0BD7),
    (0x0BE7, 0x0BEF),
    (0x0C01, 0x0C03),
    (0x0C05, 0x0C0C),
    (0x0C0E, 0x0C10),
    (0x0C12, 0x0C28),
    (0x0C2A, 0x0C33),
    (0x0C35, 0x0C39),
    (0x0C3E, 0x0C44),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C60, 0x0C61),
    (0x0C66, 0x0C6F),
    (0x0C82, 0x0C83),
    (0x0C85, 0x0C8C),
    (0x0C8E, 0x0C90),
    (0x0C92, 0x0CA8),
    (0x0CAA, 0x0CB3),
    (0x0CB5, 0x0CB9),
    (0x0CBE, 0x0CC4),
    (0x0CC6, 0x0CC8),
    (0x0CCA, 0x0CCD),
    (0x0CD5, 0x0CD6),
    (0x0CDE, 0x0CDE),
    (0x0CE0, 0x0CE1),
    (0x0CE6, 0x0CEF),
    (0x0D02, 0x0D03),
    (0x0D05, 0x0D0C),
    (0x0D0E, 0x0D10),
    (0x0D12, 0x0D28),
    (0x0D2A, 0x0D39),
    (0x0D3E, 0x0D43),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4D),
    (0x0D57, 0x0D57),
    (0x0D60, 0x0D61),
    (0x0D66, 0x0D6F),
    (0x0E01, 0x0E2E),
    (0x0E30, 0x0E3A),
    (0x0E40, 0x0E4E),
    (0x0E50, 0x0E59),
    (0x0E81, 0x0E82),
    (0x0E84, 0x0E84),
    (0x0E87, 0x0E88),
    (0x0E8A, 0x0E8A),
    (0x0E8D, 0x0E8D),
    (0x0E94, 0x0E97),
    (0x0E99, 0x0E9F),
    (0x0EA1, 0x0EA3),
    (0x0EA5, 0x0EA5),
    (0x0EA7, 0x0EA7),
    (0x0EAA, 0x0EAB),
    (0x0EAD, 0x0EAE),
    (0x0EB0, 0x0EB9),
    (0x0EBB, 0x0EBD),
    (0x0EC0, 0x0EC4),
    (0x0EC6, 0x0EC6),
    (0x0EC8, 0x0ECD),
    (0x0ED0, 0x0ED9),
    (0x0F18, 0x0F19),
    (0x0F20, 0x0F29),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F3E, 0x0F47),
    (0x0F49, 0x0F69),
    (0x0F71, 0x0F84),
    (0x0F86, 0x0F8B),
    (0x0F90, 0x0F95),
    (0x0F97, 0x0F97),
    (0x0F99, 0x0FAD),
    (0x0FB1, 0x0FB7),
    (0x0FB9, 0x0FB9),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x20D0, 0x20DC),
    (0x20E1, 0x20E1),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3005, 0x3005),
    (0x3007, 0x3007),
    (0x3021, 0x302F),
    (0x3031, 0x3035),
    (0x3041, 0x3094),
    (0x3099, 0x309A),
    (0x309D, 0x309E),
    (0x30A1, 0x30FA),
    (0x30FC, 0x30FE),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];

/// True when the code point is in one of the ranges of the table.
fn in_table(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `map_sql_identifier_to_xml_name`: the XML name for a SQL name, with `_xHHHH_` for each character that the name cannot have. `fully_escaped` also escapes each `:` and a name that starts with `xml`, and `escape_period` escapes each `.`.
#[must_use]
pub fn xml_name(ident: &str, fully_escaped: bool, escape_period: bool) -> String {
    let mut out = String::with_capacity(ident.len());
    let mut chars = ident.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let first = i == 0;
        let next = chars.peek().map(|&(_, n)| n);
        if c == ':' && (first || fully_escaped) {
            out.push_str("_x003A_");
        } else if c == '_' && next == Some('x') {
            out.push_str("_x005F_");
        } else if fully_escaped
            && first
            && ident.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("xml"))
        {
            out.push_str(if c == 'x' { "_x0078_" } else { "_x0058_" });
        } else if escape_period && c == '.' {
            out.push_str("_x002E_");
        } else {
            let table = if first { NAME_FIRST } else { NAME_CHAR };
            if in_table(table, u32::from(c)) {
                out.push(c);
            } else {
                out.push_str(&format!("_x{:04X}_", u32::from(c)));
            }
        }
    }
    out
}

/// `map_xml_name_to_sql_identifier`: the SQL name for an XML name, with the character for each `_xHHHH_`.
#[must_use]
pub fn sql_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = bytes.get(i..i + 7).filter(|w| {
            w[0] == b'_'
                && w[1] == b'x'
                && w[2..6].iter().all(u8::is_ascii_hexdigit)
                && w[6] == b'_'
        });
        if let Some(w) = escape {
            let hex = std::str::from_utf8(&w[2..6]).unwrap_or("0");
            let code = u32::from_str_radix(hex, 16).unwrap_or(0);
            out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
            i += 7;
        } else {
            let len = name[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&name[i..i + len]);
            i += len;
        }
    }
    out
}

/// `escape_xml`: the text with `&`, `<`, `>` and carriage return as references.
#[must_use]
pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#x0d;"),
            _ => out.push(c),
        }
    }
    out
}

/// `xmlEncodeSpecialChars` of libxml2, for `xmltext`: the text with `&`, `<`, `>`, `"` and carriage return as references.
#[must_use]
pub fn xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

/// The value of an attribute as the text writer of libxml2 writes it with no document: the markup characters, the white space other than the space and each character that is not ASCII as references. It drops the other control characters.
fn attribute_value(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c if u32::from(c) < 0x20 => {}
            c if c.is_ascii() => out.push(c),
            c => out.push_str(&format!("&#x{:X};", u32::from(c))),
        }
    }
}

/// `xmlelement`: the element with the attributes that have a value, and the content as it is. An element with no content is an empty element tag.
#[must_use]
pub fn xml_element(name: &str, attributes: &[(&str, &str)], content: &[String]) -> String {
    let mut out = String::new();
    out.push('<');
    out.push_str(name);
    for (attr, value) in attributes {
        out.push(' ');
        out.push_str(attr);
        out.push_str("=\"");
        attribute_value(value, &mut out);
        out.push('"');
    }
    if content.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        for part in content {
            out.push_str(part);
        }
        out.push_str("</");
        out.push_str(name);
        out.push('>');
    }
    out
}

/// `xmlconcat`: the values after their XML declarations, with one declaration for all. The declaration has the version when all the values have the same version, and the standalone part when all the values have it.
#[must_use]
pub fn xml_concat(values: &[&str]) -> String {
    let mut standalone_all = 1;
    let mut version_all: Option<&[u8]> = None;
    let mut no_version = false;
    let mut body = Vec::new();
    for value in values {
        let bytes = value.as_bytes();
        let (len, version, standalone) = match parse_xml_decl(bytes) {
            Ok(decl) => (decl.len, decl.version, decl.standalone),
            Err(_) => (0, None, -1),
        };
        if standalone == 0 && standalone_all == 1 {
            standalone_all = 0;
        }
        if standalone < 0 {
            standalone_all = -1;
        }
        match (version, version_all) {
            (None, _) => no_version = true,
            (Some(v), None) => version_all = Some(v),
            (Some(v), Some(all)) if v != all => no_version = true,
            _ => {}
        }
        body.extend_from_slice(&bytes[len..]);
    }
    let mut out = Vec::new();
    if !no_version || standalone_all >= 0 {
        print_xml_decl(&mut out, if no_version { None } else { version_all }, standalone_all);
    }
    out.extend_from_slice(&body);
    String::from_utf8(out).unwrap_or_default()
}

/// The `STANDALONE` clause of `XMLROOT`, as `XmlStandaloneType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlStandalone {
    Yes,
    No,
    NoValue,
    /// The query does not give the clause, and the value keeps its own.
    Omitted,
}

/// `xmlroot`: the value with a new XML declaration. A version of `None` is `NO VALUE`.
#[must_use]
pub fn xml_root(data: &str, version: Option<&str>, standalone: XmlStandalone) -> String {
    let bytes = data.as_bytes();
    let (len, old) = match parse_xml_decl(bytes) {
        Ok(decl) => (decl.len, decl.standalone),
        Err(_) => (0, -1),
    };
    let standalone = match standalone {
        XmlStandalone::Yes => 1,
        XmlStandalone::No => 0,
        XmlStandalone::NoValue => -1,
        XmlStandalone::Omitted => old,
    };
    let mut out = Vec::new();
    print_xml_decl(&mut out, version.map(str::as_bytes), standalone);
    out.extend_from_slice(&bytes[len..]);
    String::from_utf8(out).unwrap_or_default()
}

/// The error `2200T` of `xmlpi`.
fn invalid_pi(detail: String) -> TypeError {
    let mut error = TypeError::new(
        SqlState::INVALID_XML_PROCESSING_INSTRUCTION,
        "invalid XML processing instruction".to_string(),
    );
    error.detail = Some(detail);
    error
}

/// `xmlpi`: the processing instruction, or `None` when the argument is null. The check of the target comes before the check for null, as the standard says.
///
/// # Errors
///
/// `2200T` for the target `xml` in any case and for an argument with `?>`.
pub fn xml_pi(target: &str, arg: Option<&str>, null: bool) -> Result<Option<String>, TypeError> {
    if target.eq_ignore_ascii_case("xml") {
        return Err(invalid_pi(format!(
            "XML processing instruction target name cannot be \"{target}\"."
        )));
    }
    if null {
        return Ok(None);
    }
    let mut out = format!("<?{target}");
    if let Some(arg) = arg {
        if arg.contains("?>") {
            return Err(invalid_pi(
                "XML processing instruction cannot contain \"?>\".".to_string(),
            ));
        }
        out.push(' ');
        out.push_str(arg.trim_start_matches(' '));
    }
    out.push_str("?>");
    Ok(Some(out))
}

/// `xmlcomment`: the comment with the text.
///
/// # Errors
///
/// `2200S` for a text with `--` or with `-` at the end.
pub fn xml_comment(text: &str) -> Result<String, TypeError> {
    if text.contains("--") || text.ends_with('-') {
        return Err(TypeError::new(
            SqlState::INVALID_XML_COMMENT,
            "invalid XML comment".to_string(),
        ));
    }
    Ok(format!("<!--{text}-->"))
}

/// `xmlTextWriterWriteBase64` of libxml2: base64 with a line break after each 72 characters.
#[must_use]
pub fn xml_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    let mut line = 0;
    for chunk in bytes.chunks(3) {
        if line >= 72 {
            out.push_str("\r\n");
            line = 0;
        }
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * k)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
        line += 4;
    }
    out
}

/// `xmlTextWriterWriteBinHex` of libxml2: two hexadecimal digits in upper case for each byte.
#[must_use]
pub fn xml_binhex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(xml_name("a b", false, false), "a_x0020_b");
        assert_eq!(xml_name(":x", false, false), "_x003A_x");
        assert_eq!(xml_name("c:d", false, false), "c:d");
        assert_eq!(xml_name("c:d", true, false), "c_x003A_d");
        assert_eq!(xml_name("_x", false, false), "_x005F_x");
        assert_eq!(xml_name("1a", false, false), "_x0031_a");
        assert_eq!(xml_name("xmlfoo", false, false), "xmlfoo");
        assert_eq!(xml_name("XmLfoo", true, false), "_x0058_mLfoo");
        assert_eq!(xml_name("a.b", true, true), "a_x002E_b");
        assert_eq!(xml_name("\u{e9}", false, false), "\u{e9}");
        assert_eq!(xml_name("\u{101d0}", false, false), "_x101D0_");
        assert_eq!(sql_name("a_x0020_b"), "a b");
        assert_eq!(sql_name("_x005F_x"), "_x");
        assert_eq!(sql_name("_x00"), "_x00");
    }

    #[test]
    fn text() {
        assert_eq!(escape_xml("<&>\r\"'"), "&lt;&amp;&gt;&#x0d;\"'");
        assert_eq!(xml_text("<a & b>\r\""), "&lt;a &amp; b&gt;&#13;&quot;");
        assert_eq!(xml_element("a", &[], &[]), "<a/>");
        assert_eq!(xml_element("a", &[], &[String::new()]), "<a></a>");
        assert_eq!(
            xml_element("a", &[("b", "<\"\r\n\t'\u{e9}\u{1}")], &["x".to_string()]),
            "<a b=\"&lt;&quot;&#13;&#10;&#9;'&#xE9;\">x</a>"
        );
    }

    #[test]
    fn declarations() {
        assert_eq!(xml_concat(&["<a/>", "<b/>"]), "<a/><b/>");
        assert_eq!(
            xml_concat(&[
                "<?xml version=\"1.1\"?><a/>",
                "<?xml version=\"1.1\" standalone=\"no\"?><b/>"
            ]),
            "<?xml version=\"1.1\"?><a/><b/>"
        );
        assert_eq!(
            xml_concat(&["<?xml version=\"1.0\" standalone=\"yes\"?><a/>", "<b/>"]),
            "<a/><b/>"
        );
        assert_eq!(
            xml_root("<a/>", Some("1.1"), XmlStandalone::Omitted),
            "<?xml version=\"1.1\"?><a/>"
        );
        assert_eq!(
            xml_root(
                "<?xml version=\"1.1\" standalone=\"yes\"?><a/>",
                None,
                XmlStandalone::Omitted
            ),
            "<?xml version=\"1.0\" standalone=\"yes\"?><a/>"
        );
        assert_eq!(xml_root("<a/>", Some("1.0"), XmlStandalone::NoValue), "<a/>");
    }

    #[test]
    fn others() {
        assert_eq!(xml_pi("foo", Some("   bar"), false).unwrap().as_deref(), Some("<?foo bar?>"));
        assert_eq!(xml_pi("foo", None, true).unwrap(), None);
        assert!(xml_pi("XmL", None, true).is_err());
        assert!(xml_pi("foo", Some("a?>"), false).is_err());
        assert_eq!(xml_comment("a-b").unwrap(), "<!--a-b-->");
        assert!(xml_comment("a--b").is_err());
        assert!(xml_comment("a-").is_err());
        assert_eq!(xml_base64(&[0xab]), "qw==");
        assert_eq!(xml_base64(&[0xab, 0xcd]), "q80=");
        assert_eq!(xml_base64(&[0xab; 54]).len(), 72);
        assert_eq!(&xml_base64(&[0xab; 55])[72..], "\r\nqw==");
        assert_eq!(xml_binhex(&[1, 2, 0xff]), "0102FF");
    }
}

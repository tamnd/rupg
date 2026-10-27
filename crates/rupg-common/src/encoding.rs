//! The names and the numbers of the encodings, as `encnames.c` gives them: `pg_char_to_encoding` and `pg_encoding_to_char`.

/// `NAMEDATALEN`: a name of this length or longer is not in the table.
const NAMEDATALEN: usize = 64;

/// `pg_enc2name_tbl`: the name of each encoding by its number. The number 7, which was `MULE_INTERNAL`, has no encoding.
pub const NAMES: [&str; 42] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

/// `pg_encname_tbl`: each name and alias of an encoding, cleaned as `clean_encoding_name` does, with the name of the encoding.
const ALIASES: &[(&str, &str)] = &[
    ("abc", "WIN1258"),
    ("alt", "WIN866"),
    ("big5", "BIG5"),
    ("euccn", "EUC_CN"),
    ("eucjis2004", "EUC_JIS_2004"),
    ("eucjp", "EUC_JP"),
    ("euckr", "EUC_KR"),
    ("euctw", "EUC_TW"),
    ("gb18030", "GB18030"),
    ("gbk", "GBK"),
    ("iso88591", "LATIN1"),
    ("iso885910", "LATIN6"),
    ("iso885913", "LATIN7"),
    ("iso885914", "LATIN8"),
    ("iso885915", "LATIN9"),
    ("iso885916", "LATIN10"),
    ("iso88592", "LATIN2"),
    ("iso88593", "LATIN3"),
    ("iso88594", "LATIN4"),
    ("iso88595", "ISO_8859_5"),
    ("iso88596", "ISO_8859_6"),
    ("iso88597", "ISO_8859_7"),
    ("iso88598", "ISO_8859_8"),
    ("iso88599", "LATIN5"),
    ("johab", "JOHAB"),
    ("koi8", "KOI8R"),
    ("koi8r", "KOI8R"),
    ("koi8u", "KOI8U"),
    ("latin1", "LATIN1"),
    ("latin10", "LATIN10"),
    ("latin2", "LATIN2"),
    ("latin3", "LATIN3"),
    ("latin4", "LATIN4"),
    ("latin5", "LATIN5"),
    ("latin6", "LATIN6"),
    ("latin7", "LATIN7"),
    ("latin8", "LATIN8"),
    ("latin9", "LATIN9"),
    ("mskanji", "SJIS"),
    ("shiftjis", "SJIS"),
    ("shiftjis2004", "SHIFT_JIS_2004"),
    ("sjis", "SJIS"),
    ("sqlascii", "SQL_ASCII"),
    ("tcvn", "WIN1258"),
    ("tcvn5712", "WIN1258"),
    ("uhc", "UHC"),
    ("unicode", "UTF8"),
    ("utf8", "UTF8"),
    ("vscii", "WIN1258"),
    ("win", "WIN1251"),
    ("win1250", "WIN1250"),
    ("win1251", "WIN1251"),
    ("win1252", "WIN1252"),
    ("win1253", "WIN1253"),
    ("win1254", "WIN1254"),
    ("win1255", "WIN1255"),
    ("win1256", "WIN1256"),
    ("win1257", "WIN1257"),
    ("win1258", "WIN1258"),
    ("win866", "WIN866"),
    ("win874", "WIN874"),
    ("win932", "SJIS"),
    ("win936", "GBK"),
    ("win949", "UHC"),
    ("win950", "BIG5"),
    ("windows1250", "WIN1250"),
    ("windows1251", "WIN1251"),
    ("windows1252", "WIN1252"),
    ("windows1253", "WIN1253"),
    ("windows1254", "WIN1254"),
    ("windows1255", "WIN1255"),
    ("windows1256", "WIN1256"),
    ("windows1257", "WIN1257"),
    ("windows1258", "WIN1258"),
    ("windows866", "WIN866"),
    ("windows874", "WIN874"),
    ("windows932", "SJIS"),
    ("windows936", "GBK"),
    ("windows949", "UHC"),
    ("windows950", "BIG5"),
];

/// `pg_char_to_encoding` with the name of the encoding: the canonical name of an encoding name or alias, without regard to case and to the characters that are not letters or digits.
#[must_use]
pub fn encoding(name: &str) -> Option<&'static str> {
    if name.len() >= NAMEDATALEN {
        return None;
    }
    let key: String =
        name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    ALIASES.iter().find(|(alias, _)| *alias == key).map(|(_, name)| *name)
}

/// `pg_char_to_encoding`: the number of an encoding name or alias, or -1.
#[must_use]
pub fn char_to_encoding(name: &str) -> i32 {
    encoding(name)
        .and_then(|name| NAMES.iter().position(|n| *n == name))
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(-1)
}

/// `pg_encoding_to_char`: the name of an encoding by its number, or an empty string for a number that has no encoding.
#[must_use]
pub fn encoding_to_char(number: i64) -> &'static str {
    usize::try_from(number).ok().and_then(|n| NAMES.get(n)).copied().unwrap_or_default()
}

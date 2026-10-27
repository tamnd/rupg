//! The locales of the C library, which `pg_import_system_collations` makes collations for.

use std::process::Command;

/// The locale names that `locale -a` prints, in its order. The list is empty when the command does not run, as `pg_import_system_collations` allows on a system without the command.
pub fn system_locales() -> Vec<String> {
    let Ok(output) = Command::new("locale").arg("-a").output() else { return Vec::new() };
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect()
}

/// The `CODESET` of `nl_langinfo_l` for the `LC_CTYPE` of a locale, as `pg_get_encoding_from_locale` reads it. `None` when the C library does not know the locale.
#[cfg(target_os = "linux")]
pub fn locale_codeset(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let base = std::ptr::null_mut();
    // SAFETY: `name` is a string that ends with a NUL, and a null base asks for a new locale object.
    let locale = unsafe { libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), base) };
    if locale.is_null() {
        return None;
    }
    // SAFETY: `locale` is a valid locale object until `freelocale` below.
    let codeset = unsafe { libc::nl_langinfo_l(libc::CODESET, locale) };
    let text = (!codeset.is_null()).then(|| {
        // SAFETY: `codeset` is not null and points to a string with a NUL at its end, which lives until `freelocale` below.
        unsafe { std::ffi::CStr::from_ptr(codeset) }.to_string_lossy().into_owned()
    });
    // SAFETY: `locale` came from `newlocale`, and nothing uses it after this call.
    unsafe { libc::freelocale(locale) };
    text
}

/// The `CODESET` of a locale. Only Linux is done, so the other systems give `None` and get no collations of the C library.
#[cfg(not(target_os = "linux"))]
pub fn locale_codeset(_name: &str) -> Option<String> {
    None
}

/// `gnu_get_libc_version`: the version of the GNU C library, which is the version of each collation of the C library. `None` for another C library.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn libc_version() -> Option<String> {
    // SAFETY: `gnu_get_libc_version` returns a pointer to a static string with a NUL at its end.
    let version = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
    Some(version.to_string_lossy().into_owned())
}

/// The version of the C library. Only the GNU C library has one.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn libc_version() -> Option<String> {
    None
}

/// `encoding_match_list` of `chklocale.c`: the names of the codesets of the C library, with the encoding of each. The codesets of the encodings that only a client can use are not here, because `pg_import_system_collations` skips their locales.
const CODESETS: [(&str, &str); 92] = [
    ("EUC-JP", "EUC_JP"),
    ("eucJP", "EUC_JP"),
    ("IBM-eucJP", "EUC_JP"),
    ("sdeckanji", "EUC_JP"),
    ("CP20932", "EUC_JP"),
    ("EUC-CN", "EUC_CN"),
    ("eucCN", "EUC_CN"),
    ("IBM-eucCN", "EUC_CN"),
    ("GB2312", "EUC_CN"),
    ("dechanzi", "EUC_CN"),
    ("CP20936", "EUC_CN"),
    ("EUC-KR", "EUC_KR"),
    ("eucKR", "EUC_KR"),
    ("IBM-eucKR", "EUC_KR"),
    ("deckorean", "EUC_KR"),
    ("5601", "EUC_KR"),
    ("CP51949", "EUC_KR"),
    ("EUC-TW", "EUC_TW"),
    ("eucTW", "EUC_TW"),
    ("IBM-eucTW", "EUC_TW"),
    ("cns11643", "EUC_TW"),
    ("UTF-8", "UTF8"),
    ("utf8", "UTF8"),
    ("CP65001", "UTF8"),
    ("ISO-8859-1", "LATIN1"),
    ("ISO8859-1", "LATIN1"),
    ("iso88591", "LATIN1"),
    ("CP28591", "LATIN1"),
    ("ISO-8859-2", "LATIN2"),
    ("ISO8859-2", "LATIN2"),
    ("iso88592", "LATIN2"),
    ("CP28592", "LATIN2"),
    ("ISO-8859-3", "LATIN3"),
    ("ISO8859-3", "LATIN3"),
    ("iso88593", "LATIN3"),
    ("CP28593", "LATIN3"),
    ("ISO-8859-4", "LATIN4"),
    ("ISO8859-4", "LATIN4"),
    ("iso88594", "LATIN4"),
    ("CP28594", "LATIN4"),
    ("ISO-8859-9", "LATIN5"),
    ("ISO8859-9", "LATIN5"),
    ("iso88599", "LATIN5"),
    ("CP28599", "LATIN5"),
    ("ISO-8859-10", "LATIN6"),
    ("ISO8859-10", "LATIN6"),
    ("iso885910", "LATIN6"),
    ("ISO-8859-13", "LATIN7"),
    ("ISO8859-13", "LATIN7"),
    ("iso885913", "LATIN7"),
    ("ISO-8859-14", "LATIN8"),
    ("ISO8859-14", "LATIN8"),
    ("iso885914", "LATIN8"),
    ("ISO-8859-15", "LATIN9"),
    ("ISO8859-15", "LATIN9"),
    ("iso885915", "LATIN9"),
    ("CP28605", "LATIN9"),
    ("ISO-8859-16", "LATIN10"),
    ("ISO8859-16", "LATIN10"),
    ("iso885916", "LATIN10"),
    ("KOI8-R", "KOI8R"),
    ("CP20866", "KOI8R"),
    ("KOI8-U", "KOI8U"),
    ("CP21866", "KOI8U"),
    ("CP866", "WIN866"),
    ("CP874", "WIN874"),
    ("CP1250", "WIN1250"),
    ("CP1251", "WIN1251"),
    ("ansi-1251", "WIN1251"),
    ("CP1252", "WIN1252"),
    ("CP1253", "WIN1253"),
    ("CP1254", "WIN1254"),
    ("CP1255", "WIN1255"),
    ("CP1256", "WIN1256"),
    ("CP1257", "WIN1257"),
    ("CP1258", "WIN1258"),
    ("ISO-8859-5", "ISO_8859_5"),
    ("ISO8859-5", "ISO_8859_5"),
    ("iso88595", "ISO_8859_5"),
    ("CP28595", "ISO_8859_5"),
    ("ISO-8859-6", "ISO_8859_6"),
    ("ISO8859-6", "ISO_8859_6"),
    ("iso88596", "ISO_8859_6"),
    ("CP28596", "ISO_8859_6"),
    ("ISO-8859-7", "ISO_8859_7"),
    ("ISO8859-7", "ISO_8859_7"),
    ("iso88597", "ISO_8859_7"),
    ("CP28597", "ISO_8859_7"),
    ("ISO-8859-8", "ISO_8859_8"),
    ("ISO8859-8", "ISO_8859_8"),
    ("iso88598", "ISO_8859_8"),
    ("CP28598", "ISO_8859_8"),
];

/// `pg_get_encoding_from_locale`: the encoding of a locale of the C library. `None` when the locale does not exist or its codeset is not in [`CODESETS`].
pub fn locale_encoding(name: &str) -> Option<i32> {
    if name.eq_ignore_ascii_case("C") || name.eq_ignore_ascii_case("POSIX") {
        return Some(0);
    }
    let codeset = locale_codeset(name)?;
    let (_, encoding) = CODESETS.iter().find(|(c, _)| c.eq_ignore_ascii_case(&codeset))?;
    Some(rupg_common::encoding::char_to_encoding(encoding))
}

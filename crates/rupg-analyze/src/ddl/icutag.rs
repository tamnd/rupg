//! The names of ICU locales, as `icu_language_tag` and `icu_validate_locale` of `pg_locale_icu.c` read them with `uloc_toLanguageTag` and `uloc_getLanguage` of ICU. rupg has no ICU library, so this module converts an ICU locale ID to a BCP 47 language tag itself and lets ICU4X check and canonicalize the tag.

use icu_locale_core::Locale;

/// `ULOC_LANG_CAPACITY`: the size of the buffer of `uloc_getLanguage`.
const LANG_CAPACITY: usize = 12;

/// The languages that `uloc_toLanguageTag` replaces, as `DEPRECATED_LANGUAGES` of `uloc.cpp`.
const DEPRECATED: [(&str, &str); 5] =
    [("in", "id"), ("iw", "he"), ("ji", "yi"), ("jw", "jv"), ("mo", "ro")];

/// The BCP 47 key of a keyword of an ICU locale ID, as `uloc_toUnicodeLocaleKey` gives it.
fn key(keyword: &str) -> String {
    let keyword = keyword.to_ascii_lowercase();
    let key = match keyword.as_str() {
        "collation" => "co",
        "colalternate" => "ka",
        "colbackwards" => "kb",
        "colcaselevel" => "kc",
        "colcasefirst" => "kf",
        "colhiraganaquaternary" => "kh",
        "colnormalization" => "kk",
        "colnumeric" => "kn",
        "colreorder" => "kr",
        "colstrength" => "ks",
        "maxvariable" => "kv",
        "calendar" => "ca",
        "currency" => "cu",
        "numbers" => "nu",
        "timezone" => "tz",
        "hours" => "hc",
        "measure" => "ms",
        _ => return keyword,
    };
    key.to_string()
}

/// The BCP 47 value of a keyword, as `uloc_toUnicodeLocaleType` gives it.
fn value(key: &str, value: &str) -> String {
    let value = value.to_ascii_lowercase();
    let mapped = match (key, value.as_str()) {
        ("co", "phonebook") => "phonebk",
        ("co", "traditional") => "trad",
        ("co", "dictionary") => "dict",
        ("co", "gb2312han") => "gb2312",
        ("ks", "primary") => "level1",
        ("ks", "secondary") => "level2",
        ("ks", "tertiary") => "level3",
        ("ks", "quaternary") => "level4",
        ("ks", "identical") => "identic",
        ("ka", "non-ignorable") => "noignore",
        ("kv", "punctuation") => "punct",
        ("kv", "currency") => "currency",
        (_, "yes") => "true",
        (_, "no") => "false",
        _ => return value,
    };
    mapped.to_string()
}

/// `uloc_toLanguageTag` with `strict`: the language tag of an ICU locale ID, or the name of the error of ICU.
pub(crate) fn language_tag(locale: &str) -> Result<String, &'static str> {
    let (base, keywords) = locale.split_once('@').unwrap_or((locale, ""));
    // The part after a `.` is the charset of a POSIX locale name, which ICU drops.
    let base = base.split_once('.').map_or(base, |(base, _)| base);
    let mut subtags: Vec<String> =
        base.split(['_', '-']).filter(|s| !s.is_empty()).map(str::to_string).collect();
    match subtags.first().map(|s| s.to_ascii_lowercase()) {
        None => subtags.push("und".into()),
        Some(first) if first == "root" => subtags[0] = "und".into(),
        Some(first) => {
            if let Some(&(_, new)) = DEPRECATED.iter().find(|(old, _)| *old == first) {
                subtags[0] = new.into();
            }
        }
    }
    let mut unicode: Vec<(String, String)> = Vec::new();
    if let Some(i) = subtags.iter().skip(1).position(|s| s.eq_ignore_ascii_case("posix")) {
        subtags.remove(i + 1);
        unicode.push(("va".into(), "posix".into()));
    }
    for keyword in keywords.split(';').filter(|k| !k.is_empty()) {
        let (k, v) = keyword.split_once('=').ok_or("U_ILLEGAL_ARGUMENT_ERROR")?;
        let k = key(k.trim());
        let v = value(&k, v.trim());
        unicode.push((k, v));
    }
    unicode.sort();
    let mut tag = subtags.join("-");
    if !unicode.is_empty() {
        tag.push_str("-u");
        for (k, v) in &unicode {
            tag.push('-');
            tag.push_str(k);
            tag.push('-');
            tag.push_str(v);
        }
    }
    Locale::try_from_str(&tag).map(|l| l.to_string()).map_err(|_| "U_ILLEGAL_ARGUMENT_ERROR")
}

/// `uloc_getLanguage`: the language of an ICU locale ID in lower case, or the name of the error or warning of ICU when it does not fit the buffer.
pub(crate) fn language(locale: &str) -> Result<String, &'static str> {
    let end = locale.find(['-', '_', '@', '.']).unwrap_or(locale.len());
    let lang = locale[..end].to_ascii_lowercase();
    match lang.len() {
        n if n > LANG_CAPACITY => Err("U_BUFFER_OVERFLOW_ERROR"),
        LANG_CAPACITY => Err("U_STRING_NOT_TERMINATED_WARNING"),
        _ => Ok(lang),
    }
}

/// True when ICU has a language: the languages of the locales of `uloc_getAvailable`, and the root locale.
pub(crate) fn known_language(lang: &str) -> bool {
    matches!(lang, "" | "root" | "und")
        || rupg_pgcatalog::icu::locales().any(|l| l.tag.split('-').next() == Some(lang))
}

/// True when the collator of a locale has a tailoring of its own, as the version of `ucol_getVersion` shows it: a collation type other than `standard`, or a locale that the table of `rupg_pgcatalog::icu` or a parent of it in the table marks so.
pub(crate) fn tailored(locale: &str) -> bool {
    let Ok(parsed) = Locale::try_from_str(locale) else {
        return false;
    };
    let mut posix = false;
    for (key, value) in parsed.extensions.unicode.keywords.iter() {
        match (key.as_str(), value.to_string().as_str()) {
            ("co", "standard") => {}
            ("co", _) => return true,
            ("va", "posix") => posix = true,
            _ => {}
        }
    }
    let mut tag = parsed.id.to_string();
    if posix {
        tag.push_str("-u-va-posix");
    }
    let table = |tag: &str| rupg_pgcatalog::icu::locales().find(|l| l.tag == tag);
    loop {
        if let Some(found) = table(&tag) {
            return found.version == rupg_pgcatalog::icu::TAILORED_VERSION;
        }
        match tag.rfind('-') {
            Some(i) => tag.truncate(i),
            None => return false,
        }
    }
}

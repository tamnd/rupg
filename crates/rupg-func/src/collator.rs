//! The order of strings in a collation of the ICU provider, as `pg_newlocale_from_collation` and `strncoll_icu` give it. rupg uses ICU4X, the Rust implementation of the Unicode collation algorithm with the data of CLDR, where PostgreSQL uses ICU4C.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use icu_collator::CollatorPreferences;
use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, MaxVariable, Strength};
use icu_collator::provider::{
    CollationDiacriticsV1, CollationJamoV1, CollationMetadataV1, CollationReorderingV1,
    CollationRootV1, CollationSpecialPrimariesV1, CollationTailoringV1,
};
use icu_locale_core::{LanguageIdentifier, Locale};
use icu_locale_fallback::LocaleFallbacker;
use icu_normalizer::provider::{NormalizerNfdDataV1, NormalizerNfdTablesV1};
use icu_provider::buf::AsDeserializingBufferProvider;
use icu_provider::prelude::*;
use icu_provider_adapters::fallback::LocaleFallbackProvider;
use icu_provider_blob::BlobDataProvider;

use crate::Session;

/// `UNICODE_COLLATION_OID`: the collation `unicode` of the ICU provider, with the root locale.
const UNICODE_COLLATION_OID: u32 = 963;

/// The collator of a locale of ICU, which compares two strings.
#[derive(Clone, Copy)]
pub struct Collator(&'static icu_collator::Collator);

impl std::fmt::Debug for Collator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collator")
    }
}

impl Collator {
    /// `varstr_cmp` with a deterministic collation: the order of the collator, and the order of the bytes for two strings that the collator finds equal, as `strcmp` breaks the tie.
    #[must_use]
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        self.0.as_borrowed().compare(a, b).then_with(|| a.as_bytes().cmp(b.as_bytes()))
    }
}

/// The collators by locale. A collator lives as long as the process, as the cache of `pg_locale_t` lives as long as the backend.
fn cache() -> &'static Mutex<HashMap<String, Option<Collator>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Collator>>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// The collator of a locale of ICU, or `None` for a locale that ICU4X cannot read.
fn by_locale(locale: &str) -> Option<Collator> {
    let mut cache = cache().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&collator) = cache.get(locale) {
        return collator;
    }
    // ICU opens the root collator for a name that is not a language tag, such as `C`.
    let mut parsed = Locale::try_from_str(locale).unwrap_or(Locale::UNKNOWN);
    // `uloc_forLanguageTag` reads the keyword `va-posix` as the variant `POSIX`, which has a collation of its own.
    if parsed
        .extensions
        .unicode
        .keywords
        .get(&icu_locale_core::extensions::unicode::key!("va"))
        .is_some_and(|v| v.to_string() == "posix")
    {
        parsed.id.variants = icu_locale_core::subtags::Variants::from_variant(
            icu_locale_core::subtags::variant!("posix"),
        );
    }
    let prefs = CollatorPreferences::from(&parsed);
    let collator = icu_collator::Collator::try_new_unstable(icu74(), prefs, options(&parsed)).ok();
    let collator = collator.map(|c| Collator(Box::leak(Box::new(c))));
    cache.insert(locale.to_owned(), collator);
    collator
}

/// The collation data of ICU 74, which the ICU4C of the PostgreSQL that rupg follows reads: CLDR 44.1, with the Han characters of the root in the radical-stroke order of Unihan, as ICU4C builds its root. The compiled data of ICU4X is a later CLDR, and it puts Han in the implicit order of the code points. `icu74.postcard` is the export of the collation markers and the locale fallback markers of ICU4X from `icuexportdata_release-74-1` and `cldr-44.1.0-json-full`, with `CollationRootHan::Unihan`.
struct Icu74(LocaleFallbackProvider<BlobDataProvider>);

fn icu74() -> &'static Icu74 {
    static PROVIDER: OnceLock<Icu74> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        let blob =
            BlobDataProvider::try_new_from_static_blob(include_bytes!("collator/icu74.postcard"))
                .expect("the collation data of ICU 74");
        let fallbacker = LocaleFallbacker::try_new_unstable(&blob.as_deserializing())
            .expect("the locale fallback data of ICU 74");
        Icu74(LocaleFallbackProvider::new(blob, fallbacker))
    })
}

/// The data of a marker of a single datum, which the blob of ICU 74 holds.
macro_rules! icu74_singletons {
    ($($marker:ty),*) => {$(
        impl DataProvider<$marker> for Icu74 {
            fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<$marker>, DataError> {
                self.0.as_deserializing().load(req)
            }
        }
    )*};
}

/// The data of a marker of a tailoring, found as ICU4C finds the collation of a locale: `ures_open` of the tree `coll` truncates the locale until it finds a file, so `az-Cyrl` falls back to `az` and `hi-Latn` to `hi`, where the parent locales of CLDR send them to the root. A locale that no truncation finds goes to the fallback of CLDR, which finds the Han tailorings that ICU4X keeps under `und-Hani`.
macro_rules! icu74_tailorings {
    ($($marker:ty),*) => {$(
        impl DataProvider<$marker> for Icu74 {
            fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<$marker>, DataError> {
                let mut locale = *req.id.locale;
                while !locale.is_unknown() {
                    let id = DataIdentifierBorrowed::for_marker_attributes_and_locale(req.id.marker_attributes, &locale);
                    let mut metadata = req.metadata;
                    metadata.silent = true;
                    match self.0.inner().as_deserializing().load(DataRequest { id, metadata }) {
                        Err(e) if e.kind == DataErrorKind::IdentifierNotFound => truncate(&mut locale),
                        found => return found,
                    }
                }
                self.0.as_deserializing().load(req)
            }
        }
    )*};
}

/// The locale less its last subtag, as `uloc_getParent` gives it.
fn truncate(locale: &mut DataLocale) {
    if locale.variant.take().is_none()
        && locale.region.take().is_none()
        && locale.script.take().is_none()
    {
        *locale = DataLocale::default();
    }
}

icu74_singletons!(CollationRootV1, CollationJamoV1, CollationSpecialPrimariesV1);
icu74_tailorings!(
    CollationTailoringV1,
    CollationDiacriticsV1,
    CollationMetadataV1,
    CollationReorderingV1
);

impl DataProvider<NormalizerNfdDataV1> for Icu74 {
    fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<NormalizerNfdDataV1>, DataError> {
        icu_normalizer::provider::Baked.load(req)
    }
}

impl DataProvider<NormalizerNfdTablesV1> for Icu74 {
    fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<NormalizerNfdTablesV1>, DataError> {
        icu_normalizer::provider::Baked.load(req)
    }
}

/// The options of the collator that the keywords `ks`, `ka`, `kc` and `kv` of a locale give, as `ucol_open` reads them. `co`, `kf` and `kn` are preferences of the locale.
fn options(locale: &Locale) -> CollatorOptions {
    let mut options = CollatorOptions::default();
    for (key, value) in locale.extensions.unicode.keywords.iter() {
        let value = value.to_string();
        match (key.as_str(), value.as_str()) {
            ("ks", "level1") => options.strength = Some(Strength::Primary),
            ("ks", "level2") => options.strength = Some(Strength::Secondary),
            ("ks", "level3") => options.strength = Some(Strength::Tertiary),
            ("ks", "level4") => options.strength = Some(Strength::Quaternary),
            ("ks", "identic") => options.strength = Some(Strength::Identical),
            ("ka", "shifted") => options.alternate_handling = Some(AlternateHandling::Shifted),
            ("ka", "noignore") => {
                options.alternate_handling = Some(AlternateHandling::NonIgnorable);
            }
            ("kc", "" | "true") => options.case_level = Some(CaseLevel::On),
            ("kc", "false") => options.case_level = Some(CaseLevel::Off),
            ("kv", "space") => options.max_variable = Some(MaxVariable::Space),
            ("kv", "punct") => options.max_variable = Some(MaxVariable::Punctuation),
            ("kv", "symbol") => options.max_variable = Some(MaxVariable::Symbol),
            ("kv", "currency") => options.max_variable = Some(MaxVariable::Currency),
            _ => {}
        }
    }
    options
}

/// The collator of the collation `collation`, or `None` when the collation orders strings by their bytes or is not a deterministic collation of the ICU provider.
pub fn collator(session: &dyn Session, collation: u32) -> Option<Collator> {
    if collation == UNICODE_COLLATION_OID {
        return by_locale("und");
    }
    let row = session.catalog()?.collation(collation)?;
    if row.provider != b'i' || !row.deterministic || row.icu_rules.is_some() {
        return None;
    }
    // ICU reads the empty locale and `root` as the root locale.
    match row.locale.as_deref()? {
        "" | "root" => by_locale("und"),
        locale => by_locale(locale),
    }
}

/// The language of the collation `collation` when it is a collation of the ICU provider, for `u_strToUpper`, `u_strToLower`, `u_strToTitle` and `u_strFoldCase`, which read the language of the locale of the collation. `None` for a collation of another provider.
pub(crate) fn case_language(session: &dyn Session, collation: u32) -> Option<LanguageIdentifier> {
    if collation == 0 {
        return None;
    }
    if collation == UNICODE_COLLATION_OID {
        return Some(LanguageIdentifier::UNKNOWN);
    }
    let row = session.catalog()?.collation(collation)?;
    if row.provider != b'i' {
        return None;
    }
    let locale = row.locale.as_deref().unwrap_or("");
    Some(Locale::try_from_str(locale).map_or(LanguageIdentifier::UNKNOWN, |l| l.id))
}

#[cfg(test)]
mod tests {
    use super::by_locale;

    /// The strings of `q18.corpus`, one to a line, where `\s` is a space and `\\` a backslash.
    fn corpus() -> Vec<String> {
        let text = include_str!("collator/q18.corpus");
        let lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
        lines
            .map(|line| {
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(c) = chars.next() {
                    match (c, c == '\\') {
                        (_, true) => match chars.next() {
                            Some('s') => out.push(' '),
                            Some(other) => out.push(other),
                            None => out.push('\\'),
                        },
                        (c, false) => out.push(c),
                    }
                }
                out
            })
            .collect()
    }

    /// The orders of `q18.orders` and the order of each locale.
    fn orders() -> (Vec<Vec<usize>>, Vec<(&'static str, usize)>) {
        let number = |s: &str| s.parse::<usize>().expect("a number of a string");
        let mut orders: Vec<Vec<usize>> = Vec::new();
        let mut locales = Vec::new();
        for line in include_str!("collator/q18.orders").lines() {
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            match kind {
                "root" => orders.push(rest.split(' ').map(number).collect()),
                "order" => {
                    let (n, moves) = rest.split_once(' ').unwrap_or((rest, ""));
                    assert_eq!(
                        number(n),
                        orders.len(),
                        "the orders are in the order of their numbers"
                    );
                    let moves: Vec<(usize, Option<usize>)> = moves
                        .split(' ')
                        .filter(|m| !m.is_empty())
                        .map(|m| {
                            let (s, after) = m.split_once('>').expect("a move");
                            (number(s), (after != "-1").then(|| number(after)))
                        })
                        .collect();
                    let mut order = orders[0].clone();
                    order.retain(|s| !moves.iter().any(|m| m.0 == *s));
                    for (s, after) in moves {
                        let at = after.map_or(0, |a| {
                            order
                                .iter()
                                .position(|&x| x == a)
                                .expect("a string that is in the order")
                                + 1
                        });
                        order.insert(at, s);
                    }
                    orders.push(order);
                }
                "locale" => {
                    let (tag, n) = rest.split_once(' ').expect("a locale and its order");
                    locales.push((tag, number(n)));
                }
                _ => {}
            }
        }
        (orders, locales)
    }

    /// A string that ICU4X orders unlike ICU4C, in a locale, for a reason that rupg does not answer.
    ///
    /// - A string that is not in FCD. ICU4C with normalization off, the default of nearly every locale, reads such a string as it stands, and ICU4X always reads its NFD; the corpus holds one, `a` with U+0301 before U+0328.
    /// - A contraction of a starter, a non-starter and a starter, which ICU4X 2.4 does not match: it matches the virama of `क्ष` as the start of the contraction and then never looks ahead to `ष`. The tailorings of Marathi, Konkani, Assamese and Odia put `क्ष` and `ज्ञ` after `ह`.
    fn known_difference(tag: &str, s: &str) -> bool {
        let not_fcd = s == "a\u{301}\u{328}";
        let language = tag.split('-').next().unwrap_or(tag);
        let conjunct = match language {
            "mr" | "kok" => s.contains("क्ष") || s.contains("ज्ञ"),
            "as" => s.contains("ক্ষ"),
            "or" => s.contains("କ୍ଷ"),
            _ => false,
        };
        not_fcd || conjunct
    }

    /// Q18 (#31): ICU4X orders the strings of the corpus as ICU4C does, in the collation of each locale that `initdb` imports. `q18.orders` holds the orders of PostgreSQL 19 with ICU 74, where `ORDER BY s COLLATE "tag-x-icu"` breaks a tie of the collator with `strcmp`.
    #[test]
    fn icu4x_orders_the_corpus_as_icu4c() {
        let corpus = corpus();
        let (orders, locales) = orders();
        let imported: Vec<&str> = rupg_pgcatalog::icu::locales().map(|l| l.tag).collect();
        let tested: Vec<&str> = locales.iter().map(|l| l.0).collect();
        assert_eq!(tested, imported, "the corpus has the order of each locale that initdb imports");
        let mut failures = Vec::new();
        for (tag, n) in locales {
            let Some(collator) = by_locale(tag) else {
                failures.push(format!("{tag}: no collator"));
                continue;
            };
            let mut order: Vec<usize> = (0..corpus.len()).collect();
            order.sort_by(|&a, &b| collator.compare(&corpus[a], &corpus[b]));
            let compared = |&s: &usize| !known_difference(tag, &corpus[s]);
            order.retain(compared);
            let expected: Vec<usize> = orders[n].iter().copied().filter(compared).collect();
            let expected = &expected;
            assert_eq!(order.len(), expected.len(), "{tag}: an order of each string");
            if let Some(at) = order.iter().zip(expected).position(|(a, b)| a != b) {
                let (got, want) = (&corpus[order[at]], &corpus[expected[at]]);
                failures.push(format!("{tag}: at {at}, {got:?} where ICU4C has {want:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of the locales differ:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

//! The order of strings in a collation of the ICU provider, as `pg_newlocale_from_collation` and `strncoll_icu` give it. rupg uses ICU4X, the Rust implementation of the Unicode collation algorithm with the data of CLDR, where PostgreSQL uses ICU4C.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use icu_collator::options::CollatorOptions;
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale_core::Locale;

use crate::Session;

/// `UNICODE_COLLATION_OID`: the collation `unicode` of the ICU provider, with the root locale.
const UNICODE_COLLATION_OID: u32 = 963;

/// The collator of a locale of ICU, which compares two strings.
#[derive(Clone, Copy)]
pub struct Collator(&'static CollatorBorrowed<'static>);

impl std::fmt::Debug for Collator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collator")
    }
}

impl Collator {
    /// `varstr_cmp` with a deterministic collation: the order of the collator, and the order of the bytes for two strings that the collator finds equal, as `strcmp` breaks the tie.
    #[must_use]
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        self.0.compare(a, b).then_with(|| a.as_bytes().cmp(b.as_bytes()))
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
    let collator = Locale::try_from_str(locale).ok().and_then(|locale| {
        let prefs = CollatorPreferences::from(&locale);
        CollatorBorrowed::try_new(prefs, CollatorOptions::default()).ok()
    });
    let collator = collator.map(|c| Collator(Box::leak(Box::new(c))));
    cache.insert(locale.to_owned(), collator);
    collator
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

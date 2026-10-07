//! The time zones of the vendored IANA data (spec/07 section 7.9).
//!
//! The names come from `vendor/tzdata`, so `TimeZone` takes the same names as the PostgreSQL pin. The rules of each zone come later.

use crate::generated::tznames::ZONE_NAMES;

/// The name of a zone or a link as the data spells it, found without regard to case, as `pg_tzset` finds it.
pub fn zone_name(name: &str) -> Option<&'static str> {
    let key = name.to_ascii_lowercase();
    ZONE_NAMES
        .binary_search_by(|n| n.to_ascii_lowercase().as_str().cmp(&key))
        .ok()
        .map(|i| ZONE_NAMES[i])
}

/// Every name of a zone or a link, sorted by the name in lower case.
pub fn zone_names() -> &'static [&'static str] {
    &ZONE_NAMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_found_without_regard_to_case() {
        assert_eq!(zone_name("europe/paris"), Some("Europe/Paris"));
        assert_eq!(zone_name("UTC"), Some("UTC"));
        assert_eq!(zone_name("us/eastern"), Some("US/Eastern"));
        assert_eq!(zone_name("Mars/Olympus"), None);
        assert_eq!(zone_names().len(), 598);
    }
}

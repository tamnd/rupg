// Lifted from rudb crates/rudb-qc-rt/src/ablate.rs at cd9f9676 (2026-10-05).
// Changes: none.

//! The ablation switches of section 17.8 of `spec/compiler/17-measurement.md`: techniques the
//! compiled engine can be told to leave out, so that what each one is worth can be measured on the
//! same build. Leaving one out never changes an answer, only how it is reached.

use std::fmt;

/// A set of techniques to leave out, empty for every run that is not measuring one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ablate(u32);

impl Ablate {
    /// Nothing left out.
    pub const NONE: Ablate = Ablate(0);
    /// The group table probe in compiled code, which calls `ht_insert` only for a new key.
    pub const PROBE: Ablate = Ablate(1);
    /// A `LIKE` in the first filter answered for the whole morsel with one search over the arena.
    pub const LIKE: Ablate = Ablate(1 << 1);
    /// A top N by a count cut from the group rows before they are made into values.
    pub const TOP: Ablate = Ablate(1 << 2);
    /// A worker's group rows kept in lanes by hash, so the merge folds a lane at a time.
    pub const LANES: Ablate = Ablate(1 << 3);
    /// An aggregate with no groups answered from the table's statistics.
    pub const STATS: Ablate = Ablate(1 << 4);
    /// A `LIKE` over a column coded into a dictionary answered once for each of its values.
    pub const CODES: Ablate = Ablate(1 << 5);
    /// Overflow checks left out where the ranges in the table's statistics rule them out.
    pub const RANGES: Ablate = Ablate(1 << 6);
    /// Group rows found by the index of each key in its column's values, in front of the hash.
    pub const DENSE: Ablate = Ablate(1 << 7);
    /// A scan under a join probe reading only the rows whose key the join's table holds.
    pub const HANDOFF: Ablate = Ablate(1 << 8);
    /// A `COUNT(DISTINCT)` of a number appended in compiled code and taken in many rows at a time.
    pub const PAIRS: Ablate = Ablate(1 << 9);
    /// The groups of a key that arrives in runs closed as the key moves on, with no hash table.
    pub const RUNS: Ablate = Ablate(1 << 10);
    /// Rows for new keys appended to a table's lanes in compiled code while the table has stopped
    /// looking for keys, with no `ht_insert` call.
    pub const APPEND: Ablate = Ablate(1 << 11);
    /// A number column coded into a dictionary read through its codes rather than made flat.
    pub const CODED: Ablate = Ablate(1 << 12);
    /// Group rows found through an array indexed by the code of a key coded into a dictionary, in
    /// front of the hash.
    pub const KEYED: Ablate = Ablate(1 << 13);

    /// Every switch with its name, in the order a table of them is printed.
    pub const ALL: [(&'static str, Ablate); 14] = [
        ("probe", Ablate::PROBE),
        ("like", Ablate::LIKE),
        ("top", Ablate::TOP),
        ("lanes", Ablate::LANES),
        ("stats", Ablate::STATS),
        ("codes", Ablate::CODES),
        ("ranges", Ablate::RANGES),
        ("dense", Ablate::DENSE),
        ("handoff", Ablate::HANDOFF),
        ("pairs", Ablate::PAIRS),
        ("runs", Ablate::RUNS),
        ("append", Ablate::APPEND),
        ("coded", Ablate::CODED),
        ("keyed", Ablate::KEYED),
    ];

    /// Reads a comma separated list of names, where an empty list or `none` is nothing and `all`
    /// is everything. `None` for a name that is not a switch.
    #[must_use]
    pub fn parse(written: &str) -> Option<Ablate> {
        let mut set = Ablate::NONE;
        for name in written.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let name = name.to_ascii_lowercase();
            set = match name.as_str() {
                "none" => set,
                "all" => Ablate::ALL.iter().fold(set, |s, &(_, a)| s.with(a)),
                _ => set.with(Ablate::ALL.iter().find(|(n, _)| *n == name)?.1),
            };
        }
        Some(set)
    }

    /// This set and `other` too.
    #[must_use]
    pub const fn with(self, other: Ablate) -> Ablate {
        Ablate(self.0 | other.0)
    }

    /// Whether `technique` is left out.
    #[must_use]
    pub const fn off(self, technique: Ablate) -> bool {
        self.0 & technique.0 != 0
    }

    /// Whether nothing is left out.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Ablate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> =
            Ablate::ALL.iter().filter(|&&(_, a)| self.off(a)).map(|&(n, _)| n).collect();
        if names.is_empty() { f.write_str("none") } else { f.write_str(&names.join(",")) }
    }
}

#[cfg(test)]
mod tests {
    use super::Ablate;

    #[test]
    fn names_read_back_as_they_print() {
        assert_eq!(Ablate::parse(""), Some(Ablate::NONE));
        assert_eq!(Ablate::parse("none"), Some(Ablate::NONE));
        let two = Ablate::parse(" Probe, like ").unwrap();
        assert!(two.off(Ablate::PROBE) && two.off(Ablate::LIKE) && !two.off(Ablate::TOP));
        assert_eq!(two.to_string(), "probe,like");
        assert_eq!(Ablate::parse(&Ablate::parse("all").unwrap().to_string()), Ablate::parse("all"));
        assert_eq!(Ablate::parse("probe,fast"), None);
        assert_eq!(Ablate::NONE.to_string(), "none");
    }
}

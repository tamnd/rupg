//! The plans of the statements of the `Query` messages that a connection ran, so that a statement that comes again does not parse, analyze and prepare again.
//!
//! PostgreSQL plans each statement of a `Query` message again, and the plans of `PREPARE` stay until the catalog or the search path changes. A plan here stays while all that its analysis read stays: the catalog, the roles, the current user and the values of the settings. Each change of the catalog or of the roles makes a new allocation, as `Arc::make_mut` does with a `Weak` to the value, so a `Weak` that points to the current value shows that nothing changed. The settings have a number that changes with each value.
//!
//! The analysis takes the value of a literal of a type of date and time with the clock, as for `'now'::timestamptz`, so a statement with a word of the clock in its text gets no cached plan.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Weak};

use rupg_catalog::Catalog;
use rupg_exec::Plan;
use rupg_pgcatalog::roles::Roles;
use rupg_sql::nodes::List;

/// The longest text that the caches keep. A longer statement, such as a large `INSERT` of many rows, seldom comes again.
const LONGEST: usize = 4096;
/// The number of texts that each cache keeps. A full cache drops all its texts.
const ENTRIES: usize = 256;

/// What the analysis of a statement read, which must be the same for its plan to stay.
#[derive(Debug)]
pub(super) struct Key {
    pub(super) catalog: Weak<Catalog>,
    pub(super) roles: Weak<Roles>,
    pub(super) role: u32,
    pub(super) generation: u64,
}

impl Key {
    fn matches(
        &self,
        catalog: &Arc<Catalog>,
        roles: &Arc<Roles>,
        role: u32,
        generation: u64,
    ) -> bool {
        self.role == role
            && self.generation == generation
            && std::ptr::eq(self.catalog.as_ptr(), Arc::as_ptr(catalog))
            && std::ptr::eq(self.roles.as_ptr(), Arc::as_ptr(roles))
    }
}

/// The parsed texts and the plans of their statements. A text goes into a cache the second time that it comes, so the texts that come once, such as an `INSERT` of new values, cost only a hash.
#[derive(Debug, Default)]
pub(super) struct Plans {
    parsed: HashMap<Box<str>, Arc<List>>,
    plans: HashMap<Box<str>, (Key, Arc<Plan>)>,
    /// The hashes of the texts that came once, for each cache.
    seen_parsed: HashSet<u64>,
    seen_plans: HashSet<u64>,
    hasher: RandomState,
}

impl Plans {
    /// The statements of `text`, when the parse of the same text gave no notices before.
    pub(super) fn parsed(&self, text: &str) -> Option<Arc<List>> {
        self.parsed.get(text).cloned()
    }

    /// Keeps the statements of `text`, which the parse gave with no notices. The parse reads no setting, so the statements stay while the text is the same.
    pub(super) fn keep_parsed(&mut self, text: &str, list: &Arc<List>) {
        if text.len() > LONGEST || first_time(&mut self.seen_parsed, &self.hasher, text) {
            return;
        }
        if self.parsed.len() >= ENTRIES {
            self.parsed.clear();
        }
        self.parsed.insert(text.into(), Arc::clone(list));
    }

    /// The plan of the statement `text` that the analysis made with the same catalog, roles, current user and settings.
    pub(super) fn plan(
        &self,
        text: &str,
        catalog: &Arc<Catalog>,
        roles: &Arc<Roles>,
        role: u32,
        generation: u64,
    ) -> Option<Arc<Plan>> {
        let (key, plan) = self.plans.get(text)?;
        key.matches(catalog, roles, role, generation).then(|| Arc::clone(plan))
    }

    /// Keeps the plan of the statement `text`, unless the text is long or names the clock.
    pub(super) fn keep_plan(&mut self, text: &str, key: Key, plan: &Arc<Plan>) {
        if text.len() > LONGEST
            || reads_clock(text)
            || first_time(&mut self.seen_plans, &self.hasher, text)
        {
            return;
        }
        if self.plans.len() >= ENTRIES {
            self.plans.clear();
        }
        self.plans.insert(text.into(), (key, Arc::clone(plan)));
    }
}

/// True when `text` is not in the hashes `seen` of the texts that came before, which then has it.
fn first_time(seen: &mut HashSet<u64>, hasher: &RandomState, text: &str) -> bool {
    if seen.len() >= ENTRIES * 16 {
        seen.clear();
    }
    seen.insert(hasher.hash_one(text))
}

/// True when the text has a word that a literal of date and time takes from the clock: `now`, `today`, `tomorrow` or `yesterday`, in any case.
fn reads_clock(text: &str) -> bool {
    let text = text.as_bytes();
    ["now", "today", "tomorrow", "yesterday"]
        .iter()
        .any(|word| text.windows(word.len()).any(|w| w.eq_ignore_ascii_case(word.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_of_the_clock_keeps_no_plan() {
        assert!(reads_clock("select 'NOW'::timestamptz"));
        assert!(reads_clock("select 'today'::date"));
        assert!(!reads_clock("select 1"));
    }
}

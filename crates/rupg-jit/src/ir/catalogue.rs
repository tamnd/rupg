// Lifted from rudb crates/rudb-qc-ir/src/catalogue.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The runtime functions an `rtcall` may reach, per `spec/compiler/06-qir.md` section 6.5.
//!
//! The catalogue is static data so that every tier agrees on it without a registry: an `rtcall`
//! operand is an index into [`CATALOGUE`], the interpreter dispatches on the same index, and
//! `direct` puts the same index into its veneer table. `rudb-qc-rt` implements each entry, and
//! a test there checks that it implements all of them with these signatures.

use crate::ir::Ty;

/// One runtime function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proxy {
    /// The name the text form uses after `@`.
    pub name: &'static str,
    /// The argument types.
    pub args: &'static [Ty],
    /// The result type, `void` for none.
    pub ret: Ty,
    /// The function returns a status first, and the call site returns it when it is not `Ok`.
    pub mayfail: bool,
    /// No side effects, so equal calls may be merged.
    pub pure: bool,
    /// Writes state shared beyond the morsel, which matters to rule V9.
    pub effect: bool,
}

macro_rules! proxies {
    ($($name:literal ($($arg:ident),*) -> $ret:ident $(, $attr:ident)*;)*) => {
        /// Every runtime function, in id order. Ids are stable within a build, and the code cache
        /// keys on the build.
        pub const CATALOGUE: &[Proxy] = &[$(
            Proxy {
                name: $name,
                args: &[$(Ty::$arg),*],
                ret: Ty::$ret,
                mayfail: proxies!(@has mayfail $($attr)*),
                pure: proxies!(@has pure $($attr)*),
                effect: proxies!(@has effect $($attr)*),
            },
        )*];
    };
    (@has $want:ident) => { false };
    (@has mayfail mayfail $($rest:ident)*) => { true };
    (@has pure pure $($rest:ident)*) => { true };
    (@has effect effect $($rest:ident)*) => { true };
    (@has $want:ident $other:ident $($rest:ident)*) => { proxies!(@has $want $($rest)*) };
}

proxies! {
    // Strings. A `ptr` first argument is a handle on an object the runtime built for the query
    // before it started: a compiled pattern, a regular expression and its rewrite.
    "str_promote" (Ptr, Str16) -> Str16, mayfail;
    "str_eq" (Str16, Str16) -> I1, pure;
    "str_cmp" (Str16, Str16) -> I32, pure;
    "str_hash" (Str16, I64) -> I64, pure;
    "str_like" (Ptr, Str16) -> I1, pure;
    "str_regex" (Ptr, Str16) -> I1, pure;
    "str_regex_replace" (Ptr, Str16) -> Str16, mayfail;
    "str_lower" (Str16) -> Str16, mayfail;
    "str_upper" (Str16) -> Str16, mayfail;
    "str_length" (Str16) -> I64, pure;
    "str_concat" (Str16, Str16) -> Str16, mayfail;
    // Hash tables and aggregation state. The first argument is the handle of the table or the
    // distinct set, the second the group row `ht_insert` returned. The last argument of
    // `ht_insert` is where it publishes the table for compiled code's own probe, or zero.
    "ht_insert" (Ptr, Ptr, I64, Ptr) -> Ptr, mayfail, effect;
    // A table of runs adds a page and returns its address, when the last page is full.
    "ht_page" (Ptr) -> Ptr, mayfail, effect;
    // A join build appends the record in its state and the record's hash to the table.
    "jt_append" (Ptr, Ptr, I64) -> Void, mayfail, effect;
    "agg_distinct" (Ptr, Ptr, Str16) -> Void, mayfail, effect;
    "agg_distinct_int" (Ptr, Ptr, I128) -> Void, mayfail, effect;
    // Takes in the pairs compiled code appended to a distinct set and gives it room for more,
    // through the two words at the second argument.
    "agg_distinct_room" (Ptr, Ptr) -> Void, mayfail, effect;
    "agg_min_str" (Ptr, Str16) -> Void, mayfail, effect;
    "agg_max_str" (Ptr, Str16) -> Void, mayfail, effect;
    // Decimals and dates.
    "i128_div" (I128, I128) -> I128, mayfail, pure;
    "date_trunc_minute" (I64) -> I64, pure;
    "date_extract_minute" (I64) -> I64, pure;
    "date_extract_year" (I32) -> I64, pure;
    "date_trunc_month" (I32) -> I32, pure;
}

/// The id of the runtime function with this name.
#[must_use]
pub fn proxy(name: &str) -> Option<u32> {
    CATALOGUE.iter().position(|p| p.name == name).map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_attributes_parse() {
        for (i, p) in CATALOGUE.iter().enumerate() {
            assert_eq!(proxy(p.name), Some(i as u32), "{} is listed twice", p.name);
        }
        let insert = CATALOGUE[proxy("ht_insert").unwrap() as usize];
        assert!(insert.mayfail && insert.effect && !insert.pure);
        let eq = CATALOGUE[proxy("str_eq").unwrap() as usize];
        assert!(eq.pure && !eq.mayfail && !eq.effect);
    }
}

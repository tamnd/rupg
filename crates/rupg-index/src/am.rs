//! The `IndexAmRoutine` of each index access method that rupg has: the six core families of PostgreSQL 19, as `bthandler`, `hashhandler`, `gisthandler`, `ginhandler`, `spghandler` and `brinhandler` fill it, and the two of pgvector, as `ivfflathandler` and `hnswhandler` fill it. `pg_indexam_has_property`, `CREATE INDEX`, `CREATE OPERATOR CLASS` and the planner read the flags here, so each family reports the values of its PostgreSQL namesake.

/// `BTREE_AM_OID`.
pub const BTREE: u32 = 403;
/// `HASH_AM_OID`.
pub const HASH: u32 = 405;
/// `GIST_AM_OID`.
pub const GIST: u32 = 783;
/// `GIN_AM_OID`.
pub const GIN: u32 = 2742;
/// `SPGIST_AM_OID`.
pub const SPGIST: u32 = 4000;
/// `BRIN_AM_OID`.
pub const BRIN: u32 = 3580;

/// The fields of an `IndexAmRoutine`. The callbacks are flags: true when the routine sets the callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Routine {
    /// The name of the row of `pg_am` that the handler of PostgreSQL has.
    pub name: &'static str,
    /// The symbol of the handler function of the row of `pg_am`, which `CREATE ACCESS METHOD ... HANDLER` names.
    pub handler: &'static str,
    /// `amstrategies`, or 0 when the operators have no fixed numbers.
    pub strategies: i32,
    /// `amsupport`.
    pub support: i32,
    /// `amoptsprocnum`, or 0 when the family has no options function.
    pub options: i32,
    /// `amcanorder`.
    pub can_order: bool,
    /// `amcanorderbyop`.
    pub can_order_by_op: bool,
    /// `amcanhash`.
    pub can_hash: bool,
    /// `amconsistentequality`.
    pub consistent_equality: bool,
    /// `amconsistentordering`.
    pub consistent_ordering: bool,
    /// `amcanbackward`.
    pub can_backward: bool,
    /// `amcanunique`.
    pub can_unique: bool,
    /// `amcanmulticol`.
    pub can_multi_col: bool,
    /// `amoptionalkey`.
    pub optional_key: bool,
    /// `amsearcharray`.
    pub search_array: bool,
    /// `amsearchnulls`.
    pub search_nulls: bool,
    /// `amstorage`.
    pub storage: bool,
    /// `amclusterable`.
    pub clusterable: bool,
    /// `ampredlocks`.
    pub pred_locks: bool,
    /// `amcanparallel`.
    pub can_parallel: bool,
    /// `amcanbuildparallel`.
    pub can_build_parallel: bool,
    /// `amcaninclude`.
    pub can_include: bool,
    /// `amusemaintenanceworkmem`.
    pub use_maintenance_work_mem: bool,
    /// `amsummarizing`.
    pub summarizing: bool,
    /// `amkeytype`, or 0 when the key has the type of the column.
    pub key_type: u32,
    /// `amgettuple` is set.
    pub get_tuple: bool,
    /// `amgetbitmap` is set.
    pub get_bitmap: bool,
    /// `amcanreturn` is set.
    pub can_return: bool,
}

/// A routine with every flag false, for the fields that each handler below sets.
const NONE: Routine = Routine {
    name: "",
    handler: "",
    strategies: 0,
    support: 0,
    options: 0,
    can_order: false,
    can_order_by_op: false,
    can_hash: false,
    consistent_equality: false,
    consistent_ordering: false,
    can_backward: false,
    can_unique: false,
    can_multi_col: false,
    optional_key: false,
    search_array: false,
    search_nulls: false,
    storage: false,
    clusterable: false,
    pred_locks: false,
    can_parallel: false,
    can_build_parallel: false,
    can_include: false,
    use_maintenance_work_mem: false,
    summarizing: false,
    key_type: 0,
    get_tuple: false,
    get_bitmap: false,
    can_return: false,
};

/// The routines of the core index access methods, by the OID of the row of `pg_am`.
static CORE: [(u32, Routine); 6] = [
    (
        BTREE,
        // `bthandler`: `BTMaxStrategyNumber`, `BTNProcs` and `BTOPTIONS_PROC`.
        Routine {
            name: "btree",
            handler: "bthandler",
            strategies: 5,
            support: 6,
            options: 5,
            can_order: true,
            consistent_equality: true,
            consistent_ordering: true,
            can_backward: true,
            can_unique: true,
            can_multi_col: true,
            optional_key: true,
            search_array: true,
            search_nulls: true,
            clusterable: true,
            pred_locks: true,
            can_parallel: true,
            can_build_parallel: true,
            can_include: true,
            get_tuple: true,
            get_bitmap: true,
            can_return: true,
            ..NONE
        },
    ),
    (
        HASH,
        // `hashhandler`: `HTMaxStrategyNumber`, `HASHNProcs` and `HASHOPTIONS_PROC`. The key of the index is the `int4` hash of the column.
        Routine {
            name: "hash",
            handler: "hashhandler",
            strategies: 1,
            support: 3,
            options: 3,
            can_hash: true,
            consistent_equality: true,
            can_backward: true,
            pred_locks: true,
            key_type: 23,
            get_tuple: true,
            get_bitmap: true,
            ..NONE
        },
    ),
    (
        GIST,
        // `gisthandler`: `GISTNProcs` and `GIST_OPTIONS_PROC`.
        Routine {
            name: "gist",
            handler: "gisthandler",
            support: 12,
            options: 10,
            can_order_by_op: true,
            can_multi_col: true,
            optional_key: true,
            search_nulls: true,
            storage: true,
            clusterable: true,
            pred_locks: true,
            can_include: true,
            get_tuple: true,
            get_bitmap: true,
            can_return: true,
            ..NONE
        },
    ),
    (
        GIN,
        // `ginhandler`: `GINNProcs` and `GIN_OPTIONS_PROC`.
        Routine {
            name: "gin",
            handler: "ginhandler",
            support: 7,
            options: 7,
            can_multi_col: true,
            optional_key: true,
            storage: true,
            pred_locks: true,
            can_build_parallel: true,
            use_maintenance_work_mem: true,
            get_bitmap: true,
            ..NONE
        },
    ),
    (
        SPGIST,
        // `spghandler`: `SPGISTNProc` and `SPGIST_OPTIONS_PROC`.
        Routine {
            name: "spgist",
            handler: "spghandler",
            support: 7,
            options: 7,
            can_order_by_op: true,
            optional_key: true,
            search_nulls: true,
            storage: true,
            can_include: true,
            get_tuple: true,
            get_bitmap: true,
            can_return: true,
            ..NONE
        },
    ),
    (
        BRIN,
        // `brinhandler`: `BRIN_LAST_OPTIONAL_PROCNUM` and `BRIN_PROCNUM_OPTIONS`.
        Routine {
            name: "brin",
            handler: "brinhandler",
            support: 15,
            options: 5,
            can_multi_col: true,
            optional_key: true,
            search_nulls: true,
            storage: true,
            can_build_parallel: true,
            summarizing: true,
            get_bitmap: true,
            ..NONE
        },
    ),
];

/// The routines of the index access methods of the libraries of the extensions that rupg ships, by the symbol of the handler. The script of the extension makes the row of `pg_am`, so its OID is not fixed.
static LIBRARY: [(&str, Routine); 2] = [
    (
        "ivfflathandler",
        // `ivfflathandler` of pgvector: `IVFFLAT_FUNC_COUNT`. The scan needs an ordering operator and gives no bitmap.
        Routine {
            name: "ivfflat",
            handler: "ivfflathandler",
            support: 5,
            can_order_by_op: true,
            optional_key: true,
            can_build_parallel: true,
            get_tuple: true,
            ..NONE
        },
    ),
    (
        "hnswhandler",
        // `hnswhandler` of pgvector: `HNSW_FUNC_COUNT`.
        Routine {
            name: "hnsw",
            handler: "hnswhandler",
            support: 3,
            can_order_by_op: true,
            optional_key: true,
            can_build_parallel: true,
            get_tuple: true,
            ..NONE
        },
    ),
];

/// `GetIndexAmRoutineByAmId` for a core access method: the routine of the OID, or `None` for an OID that is not a core index access method.
#[must_use]
pub fn core(oid: u32) -> Option<&'static Routine> {
    CORE.iter().find(|(o, _)| *o == oid).map(|(_, r)| r)
}

/// The core access method of the name, with its OID.
#[must_use]
pub fn core_by_name(name: &str) -> Option<(u32, &'static Routine)> {
    CORE.iter().find(|(_, r)| r.name == name).map(|(o, r)| (*o, r))
}

/// The routine that the handler function of a library gives, by the symbol of the function.
#[must_use]
pub fn library(symbol: &str) -> Option<&'static Routine> {
    LIBRARY.iter().find(|(s, _)| *s == symbol).map(|(_, r)| r)
}

/// The routine that a handler function gives, by its symbol: the routine of a core access method with the OID of its row of `pg_am`, as `CREATE ACCESS METHOD gist2 TYPE INDEX HANDLER gisthandler` makes a synonym of `gist`, or the routine of a library with no OID.
#[must_use]
pub fn by_handler(symbol: &str) -> Option<(Option<u32>, &'static Routine)> {
    match CORE.iter().find(|(_, r)| r.handler == symbol) {
        Some((oid, routine)) => Some((Some(*oid), routine)),
        None => library(symbol).map(|r| (None, r)),
    }
}

/// The routine of the access method of a library that has the name of its row of `pg_am` in the script of the extension, as `hnsw` and `ivfflat`.
#[must_use]
pub fn library_by_name(name: &str) -> Option<&'static Routine> {
    LIBRARY.iter().find(|(_, r)| r.name == name).map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_of_the_core_methods() {
        let btree = core(BTREE).unwrap();
        assert!(btree.can_order && btree.can_unique && btree.can_include && btree.can_return);
        let hash = core(HASH).unwrap();
        assert!(hash.can_hash && !hash.can_multi_col && hash.key_type == 23);
        let gin = core(GIN).unwrap();
        assert!(!gin.get_tuple && gin.get_bitmap && gin.use_maintenance_work_mem);
        let brin = core(BRIN).unwrap();
        assert!(brin.summarizing && !brin.get_tuple);
        assert_eq!(core_by_name("spgist").map(|(o, _)| o), Some(SPGIST));
        assert!(core(2).is_none());
    }

    #[test]
    fn library_methods() {
        let hnsw = library("hnswhandler").unwrap();
        assert!(hnsw.can_order_by_op && !hnsw.get_bitmap);
        assert_eq!(library_by_name("ivfflat").map(|r| r.support), Some(5));
        assert!(library("bthandler").is_none());
        assert_eq!(by_handler("gisthandler").map(|(o, r)| (o, r.name)), Some((Some(GIST), "gist")));
        assert_eq!(by_handler("hnswhandler").map(|(o, r)| (o, r.name)), Some((None, "hnsw")));
        assert!(by_handler("int4in").is_none());
    }
}

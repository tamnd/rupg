//! The 4 static rows of `pg_class`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 34] = [
    // oid
    Batch {
        values: Values::Oid(&[
            1247, 1249, 1255, 1259,
        ]),
        nulls: &[],
    },
    // relname
    Batch {
        values: Values::Text(&[
            "pg_type", "pg_attribute", "pg_proc", "pg_class",
        ]),
        nulls: &[],
    },
    // relnamespace
    Batch {
        values: Values::Oid(&[
            11, 11, 11, 11,
        ]),
        nulls: &[],
    },
    // reltype
    Batch {
        values: Values::Oid(&[
            71, 75, 81, 83,
        ]),
        nulls: &[],
    },
    // reloftype
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relowner
    Batch {
        values: Values::Oid(&[
            10, 10, 10, 10,
        ]),
        nulls: &[],
    },
    // relam
    Batch {
        values: Values::Oid(&[
            2, 2, 2, 2,
        ]),
        nulls: &[],
    },
    // relfilenode
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // reltablespace
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relpages
    Batch {
        values: Values::Int4(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // reltuples
    Batch {
        values: Values::Float4(&[
            -1.0, -1.0, -1.0, -1.0,
        ]),
        nulls: &[],
    },
    // relallvisible
    Batch {
        values: Values::Int4(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relallfrozen
    Batch {
        values: Values::Int4(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // reltoastrelid
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relhasindex
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relisshared
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relpersistence
    Batch {
        values: Values::Char(&[
            b'p', b'p', b'p', b'p',
        ]),
        nulls: &[],
    },
    // relkind
    Batch {
        values: Values::Char(&[
            b'r', b'r', b'r', b'r',
        ]),
        nulls: &[],
    },
    // relnatts
    Batch {
        values: Values::Int2(&[
            32, 25, 30, 34,
        ]),
        nulls: &[],
    },
    // relchecks
    Batch {
        values: Values::Int2(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relhasrules
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relhastriggers
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relhassubclass
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relrowsecurity
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relforcerowsecurity
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relispopulated
    Batch {
        values: Values::Bool(&[
            true, true, true, true,
        ]),
        nulls: &[],
    },
    // relreplident
    Batch {
        values: Values::Char(&[
            b'n', b'n', b'n', b'n',
        ]),
        nulls: &[],
    },
    // relispartition
    Batch {
        values: Values::Bool(&[
            false, false, false, false,
        ]),
        nulls: &[],
    },
    // relrewrite
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // relfrozenxid
    Batch {
        values: Values::Oid(&[
            3, 3, 3, 3,
        ]),
        nulls: &[],
    },
    // relminmxid
    Batch {
        values: Values::Oid(&[
            1, 1, 1, 1,
        ]),
        nulls: &[],
    },
    // relacl
    Batch::NULL,
    // reloptions
    Batch::NULL,
    // relpartbound
    Batch::NULL,
];

//! The 7 static rows of `pg_am`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 4] = [
    // oid
    Batch {
        values: Values::Oid(&[
            2, 403, 405, 783, 2742, 4000, 3580,
        ]),
        nulls: &[],
    },
    // amname
    Batch {
        values: Values::Text(&[
            "heap", "btree", "hash", "gist", "gin", "spgist", "brin",
        ]),
        nulls: &[],
    },
    // amhandler
    Batch {
        values: Values::Oid(&[
            3, 330, 331, 332, 333, 334, 335,
        ]),
        nulls: &[],
    },
    // amtype
    Batch {
        values: Values::Char(&[
            b't', b'i', b'i', b'i', b'i', b'i', b'i',
        ]),
        nulls: &[],
    },
];

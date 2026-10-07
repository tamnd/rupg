//! The 1 static rows of `pg_ts_parser`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 8] = [
    // oid
    Batch {
        values: Values::Oid(&[
            3722,
        ]),
        nulls: &[],
    },
    // prsname
    Batch {
        values: Values::Text(&[
            "default",
        ]),
        nulls: &[],
    },
    // prsnamespace
    Batch {
        values: Values::Oid(&[
            11,
        ]),
        nulls: &[],
    },
    // prsstart
    Batch {
        values: Values::Oid(&[
            3717,
        ]),
        nulls: &[],
    },
    // prstoken
    Batch {
        values: Values::Oid(&[
            3718,
        ]),
        nulls: &[],
    },
    // prsend
    Batch {
        values: Values::Oid(&[
            3719,
        ]),
        nulls: &[],
    },
    // prsheadline
    Batch {
        values: Values::Oid(&[
            3720,
        ]),
        nulls: &[],
    },
    // prslextype
    Batch {
        values: Values::Oid(&[
            3721,
        ]),
        nulls: &[],
    },
];

//! The 2 static rows of `pg_tablespace`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 5] = [
    // oid
    Batch {
        values: Values::Oid(&[
            1663, 1664,
        ]),
        nulls: &[],
    },
    // spcname
    Batch {
        values: Values::Text(&[
            "pg_default", "pg_global",
        ]),
        nulls: &[],
    },
    // spcowner
    Batch {
        values: Values::Oid(&[
            10, 10,
        ]),
        nulls: &[],
    },
    // spcacl
    Batch::NULL,
    // spcoptions
    Batch::NULL,
];

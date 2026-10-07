//! The 3 static rows of `pg_namespace`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 4] = [
    // oid
    Batch {
        values: Values::Oid(&[
            11, 99, 2200,
        ]),
        nulls: &[],
    },
    // nspname
    Batch {
        values: Values::Text(&[
            "pg_catalog", "pg_toast", "public",
        ]),
        nulls: &[],
    },
    // nspowner
    Batch {
        values: Values::Oid(&[
            10, 10, 6171,
        ]),
        nulls: &[],
    },
    // nspacl
    Batch::NULL,
];

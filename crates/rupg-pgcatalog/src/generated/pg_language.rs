//! The 3 static rows of `pg_language`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 9] = [
    // oid
    Batch {
        values: Values::Oid(&[
            12, 13, 14,
        ]),
        nulls: &[],
    },
    // lanname
    Batch {
        values: Values::Text(&[
            "internal", "c", "sql",
        ]),
        nulls: &[],
    },
    // lanowner
    Batch {
        values: Values::Oid(&[
            10, 10, 10,
        ]),
        nulls: &[],
    },
    // lanispl
    Batch {
        values: Values::Bool(&[
            false, false, false,
        ]),
        nulls: &[],
    },
    // lanpltrusted
    Batch {
        values: Values::Bool(&[
            false, false, true,
        ]),
        nulls: &[],
    },
    // lanplcallfoid
    Batch {
        values: Values::Oid(&[
            0, 0, 0,
        ]),
        nulls: &[],
    },
    // laninline
    Batch {
        values: Values::Oid(&[
            0, 0, 0,
        ]),
        nulls: &[],
    },
    // lanvalidator
    Batch {
        values: Values::Oid(&[
            2246, 2247, 2248,
        ]),
        nulls: &[],
    },
    // lanacl
    Batch::NULL,
];

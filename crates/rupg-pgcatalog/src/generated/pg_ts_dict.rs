//! The 1 static rows of `pg_ts_dict`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 6] = [
    // oid
    Batch {
        values: Values::Oid(&[
            3765,
        ]),
        nulls: &[],
    },
    // dictname
    Batch {
        values: Values::Text(&[
            "simple",
        ]),
        nulls: &[],
    },
    // dictnamespace
    Batch {
        values: Values::Oid(&[
            11,
        ]),
        nulls: &[],
    },
    // dictowner
    Batch {
        values: Values::Oid(&[
            10,
        ]),
        nulls: &[],
    },
    // dicttemplate
    Batch {
        values: Values::Oid(&[
            3727,
        ]),
        nulls: &[],
    },
    // dictinitoption
    Batch::NULL,
];

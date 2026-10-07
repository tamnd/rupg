//! The 1 static rows of `pg_ts_config`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 5] = [
    // oid
    Batch {
        values: Values::Oid(&[
            3748,
        ]),
        nulls: &[],
    },
    // cfgname
    Batch {
        values: Values::Text(&[
            "simple",
        ]),
        nulls: &[],
    },
    // cfgnamespace
    Batch {
        values: Values::Oid(&[
            11,
        ]),
        nulls: &[],
    },
    // cfgowner
    Batch {
        values: Values::Oid(&[
            10,
        ]),
        nulls: &[],
    },
    // cfgparser
    Batch {
        values: Values::Oid(&[
            3722,
        ]),
        nulls: &[],
    },
];

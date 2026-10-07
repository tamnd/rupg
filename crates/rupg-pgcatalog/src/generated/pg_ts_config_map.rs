//! The 19 static rows of `pg_ts_config_map`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 4] = [
    // mapcfg
    Batch {
        values: Values::Oid(&[
            3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748, 3748,
            3748, 3748, 3748, 3748, 3748,
        ]),
        nulls: &[],
    },
    // maptokentype
    Batch {
        values: Values::Int4(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 15, 16, 17, 18, 19, 20, 21, 22,
        ]),
        nulls: &[],
    },
    // mapseqno
    Batch {
        values: Values::Int4(&[
            1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        ]),
        nulls: &[],
    },
    // mapdict
    Batch {
        values: Values::Oid(&[
            3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765, 3765,
            3765, 3765, 3765, 3765, 3765,
        ]),
        nulls: &[],
    },
];

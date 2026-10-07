//! The 4 static rows of `pg_ts_template`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 5] = [
    // oid
    Batch {
        values: Values::Oid(&[
            3727, 3730, 3733, 3742,
        ]),
        nulls: &[],
    },
    // tmplname
    Batch {
        values: Values::Text(&[
            "simple", "synonym", "ispell", "thesaurus",
        ]),
        nulls: &[],
    },
    // tmplnamespace
    Batch {
        values: Values::Oid(&[
            11, 11, 11, 11,
        ]),
        nulls: &[],
    },
    // tmplinit
    Batch {
        values: Values::Oid(&[
            3725, 3728, 3731, 3740,
        ]),
        nulls: &[],
    },
    // tmpllexize
    Batch {
        values: Values::Oid(&[
            3726, 3729, 3732, 3741,
        ]),
        nulls: &[],
    },
];

//! The 1 static rows of `pg_shdescription`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 3] = [
    // objoid
    Batch {
        values: Values::Oid(&[
            1,
        ]),
        nulls: &[],
    },
    // classoid
    Batch {
        values: Values::Oid(&[
            1262,
        ]),
        nulls: &[],
    },
    // description
    Batch {
        values: Values::Text(&[
            "default template for new databases",
        ]),
        nulls: &[],
    },
];

//! The 3 static rows of `pg_auth_members`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 7] = [
    // oid
    Batch {
        values: Values::Oid(&[
            10000, 10001, 10002,
        ]),
        nulls: &[],
    },
    // roleid
    Batch {
        values: Values::Oid(&[
            3374, 3375, 3377,
        ]),
        nulls: &[],
    },
    // member
    Batch {
        values: Values::Oid(&[
            3373, 3373, 3373,
        ]),
        nulls: &[],
    },
    // grantor
    Batch {
        values: Values::Oid(&[
            10, 10, 10,
        ]),
        nulls: &[],
    },
    // admin_option
    Batch {
        values: Values::Bool(&[
            false, false, false,
        ]),
        nulls: &[],
    },
    // inherit_option
    Batch {
        values: Values::Bool(&[
            true, true, true,
        ]),
        nulls: &[],
    },
    // set_option
    Batch {
        values: Values::Bool(&[
            true, true, true,
        ]),
        nulls: &[],
    },
];

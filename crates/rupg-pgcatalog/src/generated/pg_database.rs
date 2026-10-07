//! The 1 static rows of `pg_database`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 18] = [
    // oid
    Batch {
        values: Values::Oid(&[
            1,
        ]),
        nulls: &[],
    },
    // datname
    Batch {
        values: Values::Text(&[
            "template1",
        ]),
        nulls: &[],
    },
    // datdba
    Batch {
        values: Values::Oid(&[
            10,
        ]),
        nulls: &[],
    },
    // encoding
    Batch {
        values: Values::Int4(&[
            6,
        ]),
        nulls: &[],
    },
    // datlocprovider
    Batch {
        values: Values::Char(&[
            b'c',
        ]),
        nulls: &[],
    },
    // datistemplate
    Batch {
        values: Values::Bool(&[
            true,
        ]),
        nulls: &[],
    },
    // datallowconn
    Batch {
        values: Values::Bool(&[
            true,
        ]),
        nulls: &[],
    },
    // dathasloginevt
    Batch {
        values: Values::Bool(&[
            false,
        ]),
        nulls: &[],
    },
    // datconnlimit
    Batch {
        values: Values::Int4(&[
            -1,
        ]),
        nulls: &[],
    },
    // datfrozenxid
    Batch {
        values: Values::Oid(&[
            0,
        ]),
        nulls: &[],
    },
    // datminmxid
    Batch {
        values: Values::Oid(&[
            1,
        ]),
        nulls: &[],
    },
    // dattablespace
    Batch {
        values: Values::Oid(&[
            1663,
        ]),
        nulls: &[],
    },
    // datcollate
    Batch {
        values: Values::Text(&[
            "C",
        ]),
        nulls: &[],
    },
    // datctype
    Batch {
        values: Values::Text(&[
            "C",
        ]),
        nulls: &[],
    },
    // datlocale
    Batch::NULL,
    // daticurules
    Batch::NULL,
    // datcollversion
    Batch::NULL,
    // datacl
    Batch::NULL,
];

//! The 7 static rows of `pg_collation`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 12] = [
    // oid
    Batch {
        values: Values::Oid(&[
            100, 950, 951, 962, 963, 811, 6411,
        ]),
        nulls: &[],
    },
    // collname
    Batch {
        values: Values::Text(&[
            "default", "C", "POSIX", "ucs_basic", "unicode", "pg_c_utf8", "pg_unicode_fast",
        ]),
        nulls: &[],
    },
    // collnamespace
    Batch {
        values: Values::Oid(&[
            11, 11, 11, 11, 11, 11, 11,
        ]),
        nulls: &[],
    },
    // collowner
    Batch {
        values: Values::Oid(&[
            10, 10, 10, 10, 10, 10, 10,
        ]),
        nulls: &[],
    },
    // collprovider
    Batch {
        values: Values::Char(&[
            b'd', b'c', b'c', b'b', b'i', b'b', b'b',
        ]),
        nulls: &[],
    },
    // collisdeterministic
    Batch {
        values: Values::Bool(&[
            true, true, true, true, true, true, true,
        ]),
        nulls: &[],
    },
    // collencoding
    Batch {
        values: Values::Int4(&[
            -1, -1, -1, 6, -1, 6, 6,
        ]),
        nulls: &[],
    },
    // collcollate
    Batch {
        values: Values::Text(&[
            "", "C", "POSIX", "", "", "", "",
        ]),
        nulls: &[
            0x0000000000000079,
        ],
    },
    // collctype
    Batch {
        values: Values::Text(&[
            "", "C", "POSIX", "", "", "", "",
        ]),
        nulls: &[
            0x0000000000000079,
        ],
    },
    // colllocale
    Batch {
        values: Values::Text(&[
            "", "", "", "C", "und", "C.UTF-8", "PG_UNICODE_FAST",
        ]),
        nulls: &[
            0x0000000000000007,
        ],
    },
    // collicurules
    Batch::NULL,
    // collversion
    Batch {
        values: Values::Text(&[
            "", "", "", "1", "", "1", "1",
        ]),
        nulls: &[
            0x0000000000000017,
        ],
    },
];

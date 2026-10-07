//! The 6 static rows of `pg_range`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 12] = [
    // rngtypid
    Batch {
        values: Values::Oid(&[
            3904, 3906, 3908, 3910, 3912, 3926,
        ]),
        nulls: &[],
    },
    // rngsubtype
    Batch {
        values: Values::Oid(&[
            23, 1700, 1114, 1184, 1082, 20,
        ]),
        nulls: &[],
    },
    // rngmultitypid
    Batch {
        values: Values::Oid(&[
            4451, 4532, 4533, 4534, 4535, 4536,
        ]),
        nulls: &[],
    },
    // rngcollation
    Batch {
        values: Values::Oid(&[
            0, 0, 0, 0, 0, 0,
        ]),
        nulls: &[],
    },
    // rngsubopc
    Batch {
        values: Values::Oid(&[
            1978, 3125, 3128, 3127, 3122, 3124,
        ]),
        nulls: &[],
    },
    // rngconstruct2
    Batch {
        values: Values::Oid(&[
            3840, 3844, 3933, 3937, 3941, 3945,
        ]),
        nulls: &[],
    },
    // rngconstruct3
    Batch {
        values: Values::Oid(&[
            3841, 3845, 3934, 3938, 3942, 3946,
        ]),
        nulls: &[],
    },
    // rngmltconstruct0
    Batch {
        values: Values::Oid(&[
            4280, 4283, 4286, 4289, 4292, 4295,
        ]),
        nulls: &[],
    },
    // rngmltconstruct1
    Batch {
        values: Values::Oid(&[
            4281, 4284, 4287, 4290, 4293, 4296,
        ]),
        nulls: &[],
    },
    // rngmltconstruct2
    Batch {
        values: Values::Oid(&[
            4282, 4285, 4288, 4291, 4294, 4297,
        ]),
        nulls: &[],
    },
    // rngcanonical
    Batch {
        values: Values::Oid(&[
            3914, 0, 0, 0, 3915, 3928,
        ]),
        nulls: &[],
    },
    // rngsubdiff
    Batch {
        values: Values::Oid(&[
            3922, 3924, 3929, 3930, 3925, 3923,
        ]),
        nulls: &[],
    },
];

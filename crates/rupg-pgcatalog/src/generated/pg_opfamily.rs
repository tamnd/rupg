//! The 148 static rows of `pg_opfamily`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 5] = [
    // oid
    Batch {
        values: Values::Oid(&[
            397, 627, 423, 424, 426, 427, 428, 429, 431, 434, 435, 1970, 1971, 1974, 1975, 3550,
            3794, 1976, 1977, 1982, 1983, 1984, 1985, 3371, 3372, 1988, 1998, 1989, 1990, 1991,
            1992, 2994, 6194, 3194, 1994, 1995, 1996, 1997, 1999, 2000, 2001, 2002, 2040, 2095,
            2097, 2099, 2222, 2223, 2789, 2225, 5032, 5067, 6459, 6460, 2226, 2227, 2229, 2231,
            2235, 2593, 2594, 2595, 1029, 2745, 2968, 2969, 3253, 3254, 3522, 3523, 3626, 3655,
            3659, 3683, 3702, 3901, 3903, 3919, 3474, 4015, 4016, 4017, 4033, 4034, 4036, 4037,
            4054, 4602, 4572, 4055, 4603, 4056, 4573, 4574, 4058, 4604, 4575, 4059, 4605, 4576,
            4062, 4577, 4064, 4578, 4065, 4579, 4068, 4606, 4580, 4069, 4581, 4607, 4070, 4608,
            4582, 4074, 4609, 4583, 4109, 4610, 4584, 4075, 4611, 4102, 4585, 4076, 4586, 4077,
            4612, 4587, 4078, 4613, 4588, 4079, 4080, 4081, 4614, 4589, 4103, 4082, 4615, 4590,
            4104, 5000, 5008, 4199, 4225, 6158,
        ]),
        nulls: &[],
    },
    // opfmethod
    Batch {
        values: Values::Oid(&[
            403, 405, 403, 403, 403, 405, 403, 403, 405, 403, 405, 403, 405, 403, 405, 783, 4000,
            403, 405, 403, 405, 403, 405, 403, 405, 403, 405, 403, 405, 403, 405, 403, 405, 403,
            403, 405, 403, 405, 405, 403, 405, 403, 405, 403, 403, 403, 405, 405, 403, 405, 405,
            403, 405, 403, 405, 405, 405, 405, 405, 783, 783, 783, 783, 2742, 403, 405, 403, 405,
            403, 405, 403, 783, 2742, 403, 783, 403, 405, 783, 4000, 4000, 4000, 4000, 403, 405,
            2742, 2742, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580,
            3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580,
            3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580,
            3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580, 3580,
            3580, 3580, 3580, 4000, 4000, 403, 405, 783,
        ]),
        nulls: &[],
    },
    // opfname
    Batch {
        values: Values::Text(&[
            "array_ops", "array_ops", "bit_ops", "bool_ops", "bpchar_ops", "bpchar_ops",
            "bytea_ops", "char_ops", "char_ops", "datetime_ops", "date_ops", "float_ops",
            "float_ops", "network_ops", "network_ops", "network_ops", "network_ops", "integer_ops",
            "integer_ops", "interval_ops", "interval_ops", "macaddr_ops", "macaddr_ops",
            "macaddr8_ops", "macaddr8_ops", "numeric_ops", "numeric_ops", "oid_ops", "oid_ops",
            "oidvector_ops", "oidvector_ops", "record_ops", "record_ops", "record_image_ops",
            "text_ops", "text_ops", "time_ops", "time_ops", "timestamptz_ops", "timetz_ops",
            "timetz_ops", "varbit_ops", "timestamp_ops", "text_pattern_ops", "bpchar_pattern_ops",
            "money_ops", "bool_ops", "bytea_ops", "tid_ops", "xid_ops", "xid8_ops", "xid8_ops",
            "oid8_ops", "oid8_ops", "cid_ops", "tid_ops", "text_pattern_ops", "bpchar_pattern_ops",
            "aclitem_ops", "box_ops", "poly_ops", "circle_ops", "point_ops", "array_ops",
            "uuid_ops", "uuid_ops", "pg_lsn_ops", "pg_lsn_ops", "enum_ops", "enum_ops",
            "tsvector_ops", "tsvector_ops", "tsvector_ops", "tsquery_ops", "tsquery_ops",
            "range_ops", "range_ops", "range_ops", "range_ops", "quad_point_ops", "kd_point_ops",
            "text_ops", "jsonb_ops", "jsonb_ops", "jsonb_ops", "jsonb_path_ops",
            "integer_minmax_ops", "integer_minmax_multi_ops", "integer_bloom_ops",
            "numeric_minmax_ops", "numeric_minmax_multi_ops", "text_minmax_ops", "text_bloom_ops",
            "numeric_bloom_ops", "timetz_minmax_ops", "timetz_minmax_multi_ops", "timetz_bloom_ops",
            "datetime_minmax_ops", "datetime_minmax_multi_ops", "datetime_bloom_ops",
            "char_minmax_ops", "char_bloom_ops", "bytea_minmax_ops", "bytea_bloom_ops",
            "name_minmax_ops", "name_bloom_ops", "oid_minmax_ops", "oid_minmax_multi_ops",
            "oid_bloom_ops", "tid_minmax_ops", "tid_bloom_ops", "tid_minmax_multi_ops",
            "float_minmax_ops", "float_minmax_multi_ops", "float_bloom_ops", "macaddr_minmax_ops",
            "macaddr_minmax_multi_ops", "macaddr_bloom_ops", "macaddr8_minmax_ops",
            "macaddr8_minmax_multi_ops", "macaddr8_bloom_ops", "network_minmax_ops",
            "network_minmax_multi_ops", "network_inclusion_ops", "network_bloom_ops",
            "bpchar_minmax_ops", "bpchar_bloom_ops", "time_minmax_ops", "time_minmax_multi_ops",
            "time_bloom_ops", "interval_minmax_ops", "interval_minmax_multi_ops",
            "interval_bloom_ops", "bit_minmax_ops", "varbit_minmax_ops", "uuid_minmax_ops",
            "uuid_minmax_multi_ops", "uuid_bloom_ops", "range_inclusion_ops", "pg_lsn_minmax_ops",
            "pg_lsn_minmax_multi_ops", "pg_lsn_bloom_ops", "box_inclusion_ops", "box_ops",
            "poly_ops", "multirange_ops", "multirange_ops", "multirange_ops",
        ]),
        nulls: &[],
    },
    // opfnamespace
    Batch {
        values: Values::Oid(&[
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
        ]),
        nulls: &[],
    },
    // opfowner
    Batch {
        values: Values::Oid(&[
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        ]),
        nulls: &[],
    },
];

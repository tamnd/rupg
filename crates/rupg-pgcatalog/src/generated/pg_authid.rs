//! The 17 static rows of `pg_authid`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 12] = [
    // oid
    Batch {
        values: Values::Oid(&[
            10, 6171, 6181, 6182, 3373, 3374, 3375, 3377, 4569, 4570, 4571, 4200, 4544, 6337, 4550,
            6304, 6392,
        ]),
        nulls: &[],
    },
    // rolname
    Batch {
        values: Values::Text(&[
            "postgres", "pg_database_owner", "pg_read_all_data", "pg_write_all_data", "pg_monitor",
            "pg_read_all_settings", "pg_read_all_stats", "pg_stat_scan_tables",
            "pg_read_server_files", "pg_write_server_files", "pg_execute_server_program",
            "pg_signal_backend", "pg_checkpoint", "pg_maintain", "pg_use_reserved_connections",
            "pg_create_subscription", "pg_signal_autovacuum_worker",
        ]),
        nulls: &[],
    },
    // rolsuper
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolinherit
    Batch {
        values: Values::Bool(&[
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true,
        ]),
        nulls: &[],
    },
    // rolcreaterole
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolcreatedb
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolcanlogin
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolreplication
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolbypassrls
    Batch {
        values: Values::Bool(&[
            true, false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, false, false,
        ]),
        nulls: &[],
    },
    // rolconnlimit
    Batch {
        values: Values::Int4(&[
            -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        ]),
        nulls: &[],
    },
    // rolpassword
    Batch::NULL,
    // rolvaliduntil
    Batch::NULL,
];

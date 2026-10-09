//! The functions of the views of the parts of the server that rupg does not have: asynchronous I/O, the registry of dynamic shared memory, the NUMA nodes, the replication origins, recovery, the publications, the cursors of `DECLARE`, the lists of the most common values of the extended statistics, the extensions, the configuration file and the install tree of a C build.
//!
//! These functions give what PostgreSQL gives on a primary server with none of these objects: no rows, one row of nulls for `pg_stat_get_recovery`, and the error of a build with no NUMA support for `pg_get_shmem_allocations_numa`. A publication name is always unknown, because rupg has no publications. `pg_config` gives its 23 names, with `not recorded` for each value that rupg does not have.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{Call, bad_value};

/// The 9 columns of `pg_stat_get_recovery`.
const RECOVERY_COLUMNS: usize = 9;

/// A set function with no rows: `pg_get_aios`, `pg_get_dsm_registry_allocations`, `pg_show_replication_origin_status` and `pg_cursor`.
pub(crate) fn none(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// `pg_available_extensions` and `pg_available_extension_versions`: rupg has no extensions yet, so no extension is available.
pub(crate) fn extensions(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// `pg_show_all_file_settings`: rupg reads no configuration file, so no line of a file gives a setting. The setting `config_file` is empty for the same reason.
pub(crate) fn file_settings(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// The names of `pg_config`, in the order of `get_configdata`.
const CONFIG_NAMES: [&str; 23] = [
    "BINDIR",
    "DOCDIR",
    "HTMLDIR",
    "INCLUDEDIR",
    "PKGINCLUDEDIR",
    "INCLUDEDIR-SERVER",
    "LIBDIR",
    "PKGLIBDIR",
    "LOCALEDIR",
    "MANDIR",
    "SHAREDIR",
    "SYSCONFDIR",
    "PGXS",
    "CONFIGURE",
    "CC",
    "CPPFLAGS",
    "CFLAGS",
    "CFLAGS_SL",
    "LDFLAGS",
    "LDFLAGS_EX",
    "LDFLAGS_SL",
    "LIBS",
    "VERSION",
];

/// `pg_config`: the 23 names of `get_configdata`. rupg is one program with no install tree and no C build, so each directory and each flag of the compiler is `not recorded`, which is the value that PostgreSQL gives for a flag that its build did not record. `VERSION` is `PostgreSQL` and the value of `server_version`.
pub(crate) fn config(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let version = call.session.setting("server_version").unwrap_or_default();
    Ok(CONFIG_NAMES
        .iter()
        .map(|&name| {
            let setting = match name {
                "VERSION" => format!("PostgreSQL {version}"),
                _ => "not recorded".to_owned(),
            };
            vec![Value::text(name), Value::text(setting)]
        })
        .collect())
}

/// `pg_stat_get_recovery`: a null record when the server is not in recovery, which is one row of nulls in `FROM`.
pub(crate) fn recovery(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(vec![vec![Value::Null; RECOVERY_COLUMNS]])
}

/// `pg_get_shmem_allocations_numa`: the error of a server with no NUMA support.
pub(crate) fn shmem_numa(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Err(Error::new(
        SqlState::INTERNAL_ERROR,
        "libnuma initialization failed or NUMA is not supported on this platform",
    ))
}

/// `pg_mcv_list_items`: rupg has no extended statistics, so no `pg_mcv_list` value exists. The function is strict, so a null list gives no rows with no call.
pub(crate) fn mcv_list_items(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Err(Error::internal("pg_mcv_list_items got a value, but no pg_mcv_list value can exist"))
}

/// The error of `GetPublicationByName` for a name that is not a publication.
fn no_publication(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
}

/// `pg_get_publication_tables` with the `VARIADIC` array of names: the error for the first name, as no publication exists. An empty array gives no rows, and a null element is an error.
pub(crate) fn publication_tables(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let names: Vec<Option<&Value>> = match args {
        [Value::Array(array)] => array.values.iter().map(Option::as_ref).collect(),
        _ => args.iter().map(Some).collect(),
    };
    match names.first() {
        None => Ok(Vec::new()),
        Some(Some(Value::Text(name))) => Err(no_publication(name)),
        Some(Some(_)) => Err(bad_value()),
        Some(None) => Err(Error::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "null array element not allowed in this context",
        )),
    }
}

/// `pg_get_publication_tables` with a table: no rows, as no publication that the array names exists and this form skips the names that are not publications.
pub(crate) fn publication_tables_of(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// `pg_get_publication_sequences`: the error for the name, as no publication exists.
pub(crate) fn publication_sequences(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Text(name)) = args.first() else { return Err(bad_value()) };
    Err(no_publication(name))
}

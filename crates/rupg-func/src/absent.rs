//! The functions of the views of the parts of the server that rupg does not have: asynchronous I/O, the registry of dynamic shared memory, the NUMA nodes, the replication origins, recovery, the publications and the cursors of `DECLARE`.
//!
//! These functions give what PostgreSQL gives on a primary server with none of these objects: no rows, one row of nulls for `pg_stat_get_recovery`, and the error of a build with no NUMA support for `pg_get_shmem_allocations_numa`. A publication name is always unknown, because rupg has no publications.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{Call, bad_value};

/// The 9 columns of `pg_stat_get_recovery`.
const RECOVERY_COLUMNS: usize = 9;

/// A set function with no rows: `pg_get_aios`, `pg_get_dsm_registry_allocations`, `pg_show_replication_origin_status` and `pg_cursor`.
pub(crate) fn none(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
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

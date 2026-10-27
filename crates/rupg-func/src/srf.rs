//! The set-returning functions that a query can call in `FROM` or in its target list: `generate_series` of the integer types, of the timestamps and of `numeric`, `unnest`, `generate_subscripts`, `string_to_table`, `regexp_matches`, `regexp_split_to_table`, `pg_options_to_table`, `aclexplode`, `range_minus_multi`, `multirange_unnest`, `multirange_minus_multi`, `unnest` of a `tsvector`, `ts_parse`, `ts_token_type`, `ts_stat`, `pg_partition_tree`, `pg_partition_ancestors`, `pg_get_keywords`, `pg_show_all_settings`, `pg_stat_get_activity`, `pg_stat_get_backend_idset`, `pg_stat_get_autovacuum_scores`, `pg_prepared_statement`, `pg_listening_channels`, `pg_hba_file_rules`, `pg_ident_file_mappings`, the functions of the views of replication, of progress and of the prepared transactions, the functions of the statistics of the whole server that give rows, the functions of the views of asynchronous I/O, shared memory, extended statistics, replication origins, recovery and publications, `pg_cursor`, and the functions of the views of the time zones.
//!
//! A set kernel gives all the rows of the call at once, as a function of PostgreSQL in the materialize mode gives a tuplestore. Each row has one value for each column of the result: one value for a function of a scalar type, or one value for each `OUT` parameter.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::Category;
use rupg_types::Value;

use crate::{Call, INTERNAL_LANGUAGE, bad_value};

/// The code of a set-returning function. It gets the values of the arguments and gives the rows of the result. A strict function does not get a null value, because the executor gives no rows without the call.
pub type SetKernel = fn(&Call<'_>, &[Value]) -> Result<Vec<Vec<Value>>>;

/// The set kernel of the function with this `pg_proc` OID, or `None` if the engine does not have it.
pub fn set_kernel(func: u32) -> Option<SetKernel> {
    let proc = builtin::proc_by_oid(func)?;
    if proc.lang != INTERNAL_LANGUAGE {
        return None;
    }
    let kernel: SetKernel = match proc.src {
        "generate_series_int4" | "generate_series_step_int4" => series_int4,
        "generate_series_int8" | "generate_series_step_int8" => series_int8,
        "array_unnest" => unnest,
        "generate_subscripts" | "generate_subscripts_nodir" => subscripts,
        "text_to_table" | "text_to_table_null" => text_to_table,
        "regexp_matches" | "regexp_matches_no_flags" => crate::regex::regexp_matches,
        "regexp_split_to_table" | "regexp_split_to_table_no_flags" => {
            crate::regex::regexp_split_to_table
        }
        "pg_options_to_table" => options_to_table,
        "aclexplode" => crate::acl::aclexplode,
        "pg_snapshot_xip" => crate::snapshot::snapshot_xip,
        "range_minus_multi" => crate::range::range_minus_multi,
        "multirange_unnest" => crate::multirange::unnest,
        "multirange_minus_multi" => crate::multirange::minus_multi,
        "tsvector_unnest" => crate::tsvector::unnest,
        "ts_parse_byid" | "ts_parse_byname" => crate::tsearch::parse,
        "ts_token_type_byid" | "ts_token_type_byname" => crate::tsearch::token_types,
        "ts_stat1" => crate::tsstat::stat1,
        "ts_stat2" => crate::tsstat::stat2,
        "json_each" => crate::json::json_each,
        "json_each_text" => crate::json::json_each_text,
        "jsonb_each" => crate::json::jsonb_each,
        "jsonb_each_text" => crate::json::jsonb_each_text,
        "json_object_keys" => crate::json::json_object_keys,
        "jsonb_object_keys" => crate::json::jsonb_object_keys,
        "json_array_elements" => crate::json::json_array_elements,
        "json_array_elements_text" => crate::json::json_array_elements_text,
        "jsonb_array_elements" => crate::json::jsonb_array_elements,
        "jsonb_path_query" => crate::jsonpath::jsonb_path_query,
        "jsonb_path_query_tz" => crate::jsonpath::jsonb_path_query_tz,
        "jsonb_array_elements_text" => crate::json::jsonb_array_elements_text,
        "pg_partition_tree" | "pg_partition_ancestors" => crate::catinfo::partition_members,
        "pg_get_keywords" => keywords,
        "pg_get_wait_events" => wait_events,
        "show_all_settings" => all_settings,
        "pg_prepared_statement" => prepared_statements,
        "pg_cursor" => cursors,
        "pg_listening_channels" => listening_channels,
        "pg_hba_file_rules" => hba_rules,
        "pg_ident_file_mappings" => ident_mappings,
        "pg_stat_get_activity" => crate::activity::activity,
        "pg_stat_get_backend_idset" => crate::activity::backend_idset,
        "pg_stat_get_progress_info" => crate::replication::progress_info,
        "pg_stat_get_wal_senders"
        | "pg_stat_get_subscription"
        | "pg_get_replication_slots"
        | "pg_prepared_xact" => crate::replication::none,
        "pg_stat_get_wal_receiver" => crate::replication::wal_receiver,
        "pg_stat_get_subscription_stats" => crate::replication::subscription_stats,
        "pg_stat_get_replication_slot" => crate::replication::replication_slot,
        "pg_get_aios" | "pg_get_dsm_registry_allocations" | "pg_show_replication_origin_status" => {
            crate::absent::none
        }
        "pg_available_extensions" => crate::absent::extensions,
        "pg_available_extension_versions" => crate::absent::extension_versions,
        "show_all_file_settings" => file_settings,
        "pg_config" => crate::absent::config,
        "pg_stat_get_recovery" => crate::absent::recovery,
        "pg_get_shmem_allocations" => crate::shmem::allocations,
        "pg_get_shmem_allocations_numa" => crate::absent::shmem_numa,
        "pg_stats_ext_mcvlist_items" => crate::absent::mcv_list_items,
        "pg_get_publication_tables_a" => crate::absent::publication_tables,
        "pg_get_publication_tables_b" => crate::absent::publication_tables_of,
        "pg_get_publication_sequences" => crate::absent::publication_sequences,
        "pg_stat_get_wal" => crate::shared::wal,
        "pg_stat_get_io" => crate::shared::io,
        "pg_stat_get_archiver" => crate::shared::archiver,
        "pg_stat_get_slru" => crate::shared::slru,
        "pg_stat_get_recovery_prefetch" => crate::shared::recovery_prefetch,
        "pg_stat_get_lock" => crate::shared::lock,
        "pg_stat_get_autovacuum_scores" => crate::autovacuum::scores,
        "pg_tablespace_databases" => crate::dbsize::tablespace_databases,
        "pg_timezone_names" => crate::zones::names,
        "pg_timezone_abbrevs_zone" => crate::zones::abbrevs_zone,
        "pg_timezone_abbrevs_abbrevs" => crate::zones::abbrevs_abbrevs,
        src => return crate::calendar::set_kernel(src),
    };
    Some(kernel)
}

/// The integer value of an argument.
fn int(args: &[Value], at: usize) -> Result<i64> {
    args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)
}

/// `generate_series_step_int8`: the numbers from the start to the stop, with the step or 1. The series stops before a number that overflows `kind`, or after the rows that the caller reads.
fn series(
    call: &Call<'_>,
    args: &[Value],
    min: i64,
    max: i64,
    kind: fn(i64) -> Value,
) -> Result<Vec<Vec<Value>>> {
    let (mut current, finish) = (int(args, 0)?, int(args, 1)?);
    let step = if args.len() > 2 { int(args, 2)? } else { 1 };
    if step == 0 {
        return Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
    }
    let most = call.want.unwrap_or(usize::MAX);
    let mut rows = Vec::new();
    while rows.len() < most && ((step > 0 && current <= finish) || (step < 0 && current >= finish))
    {
        rows.push(vec![kind(current)]);
        match current.checked_add(step).filter(|n| (min..=max).contains(n)) {
            Some(next) => current = next,
            None => break,
        }
    }
    Ok(rows)
}

/// `generate_series_int4` and `generate_series_step_int4`.
fn series_int4(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    series(call, args, i64::from(i32::MIN), i64::from(i32::MAX), |n| {
        Value::Int4(i32::try_from(n).unwrap_or_default())
    })
}

/// `generate_series_int8` and `generate_series_step_int8`.
fn series_int8(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    series(call, args, i64::MIN, i64::MAX, Value::Int8)
}

/// `array_unnest`: the elements of the array in the order of storage, for all the dimensions.
fn unnest(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Array(array)) = args.first() else { return Err(bad_value()) };
    Ok(array.values.iter().map(|v| vec![v.clone().unwrap_or(Value::Null)]).collect())
}

/// `generate_subscripts`: the subscripts of one dimension of the array, from the lower bound up, or down with the third argument true. A dimension that the array does not have gives no rows.
fn subscripts(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Array(array)) = args.first() else { return Err(bad_value()) };
    let reverse = args.get(2).and_then(Value::as_bool).unwrap_or(false);
    let dim = int(args, 1)?;
    let Some(dim) = usize::try_from(dim - 1).ok().and_then(|d| array.dims.get(d)) else {
        return Ok(Vec::new());
    };
    let (lower, upper) = (i64::from(dim.lower), i64::from(dim.lower) + i64::from(dim.len) - 1);
    let mut rows: Vec<Vec<Value>> =
        (lower..=upper).map(|n| vec![Value::Int4(i32::try_from(n).unwrap_or_default())]).collect();
    if reverse {
        rows.reverse();
    }
    Ok(rows)
}

/// `text_to_table`: the parts of the string between the separators. The function is not strict: a null string gives no rows, and a null separator gives each character. An empty separator gives the whole string, and an empty string gives no rows. A part equal to the third argument is null.
fn text_to_table(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(input) = args.first().and_then(Value::as_str) else { return Ok(Vec::new()) };
    let separator = args.get(1).and_then(Value::as_str);
    let null = args.get(2).and_then(Value::as_str);
    let parts = crate::array::split(input, separator, null);
    Ok(parts.into_iter().map(|v| vec![v.unwrap_or(Value::Null)]).collect())
}

/// `pg_options_to_table` with `untransformRelOptions`: each option of the form `name=value` gives its name and its value. An option with no `=` has a null value.
fn options_to_table(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Array(array)) = args.first() else { return Err(bad_value()) };
    let mut rows = Vec::with_capacity(array.values.len());
    for option in array.values.iter().flatten() {
        let option = option.as_str().ok_or_else(bad_value)?;
        rows.push(match option.split_once('=') {
            Some((name, value)) => vec![Value::text(name), Value::text(value)],
            None => vec![Value::text(option), Value::Null],
        });
    }
    Ok(rows)
}

/// `show_all_settings`: the parameters of the session with their values, which the view `pg_settings` reads.
fn all_settings(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call.session.all_settings())
}

/// `pg_prepared_statement`: the named prepared statements of the session, which the view `pg_prepared_statements` reads.
fn prepared_statements(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call.session.prepared_statements())
}

/// `pg_cursor`: the open cursors of the session, which the view `pg_cursors` reads.
fn cursors(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call.session.cursors())
}

/// `pg_listening_channels`: the channels that the session listens on.
fn listening_channels(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call
        .session
        .listening_channels()
        .into_iter()
        .map(|channel| vec![Value::Text(channel)])
        .collect())
}

/// `pg_hba_file_rules`: the lines of the host rules, which the view `pg_hba_file_rules` reads.
fn hba_rules(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    call.session.hba_rules()
}

/// `pg_show_all_file_settings`: the lines of the configuration files, which the view `pg_file_settings` reads.
fn file_settings(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(call.session.file_settings())
}

/// `pg_ident_file_mappings`: the lines of the user maps, which the view `pg_ident_file_mappings` reads.
fn ident_mappings(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    call.session.ident_mappings()
}

/// `pg_get_wait_events`: the wait events of `wait_event_names.txt`. PostgreSQL also gives the events that extensions add, and rupg has no extensions.
fn wait_events(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(crate::generated::wait_events::WAIT_EVENTS
        .iter()
        .map(|&(kind, name, description)| {
            vec![Value::text(kind), Value::text(name), Value::text(description)]
        })
        .collect())
}

/// `pg_get_keywords`: each keyword of the parser with its category and its description, and whether it can be a column label without `AS`.
fn keywords(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(rupg_sql::keywords()
        .iter()
        .map(|k| {
            let (code, desc) = match k.category {
                Category::Unreserved => (b'U', "unreserved"),
                Category::ColumnName => (b'C', "unreserved (cannot be function or type name)"),
                Category::TypeFuncName => (b'T', "reserved (can be function or type name)"),
                Category::Reserved => (b'R', "reserved"),
            };
            let bare = if k.bare_label { "can be bare label" } else { "requires AS" };
            vec![
                Value::text(k.name),
                Value::Char(code),
                Value::Bool(k.bare_label),
                Value::text(desc),
                Value::text(bare),
            ]
        })
        .collect())
}

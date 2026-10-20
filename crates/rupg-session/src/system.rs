//! The objects that `initdb` makes with SQL after the bootstrap: the system views of `system_views.sql`, the Snowball dictionaries of `snowball_create.sql` and the schema `information_schema` of `information_schema.sql`.
//!
//! `initdb` runs the script in one backend as the bootstrap superuser, with `search_path` set to `pg_catalog` and `allow_system_table_mods` on. rupg runs each `CREATE VIEW` of the script through the analyzer in the same way, once in each process, and each new store starts from the result.
//!
//! A view takes four OIDs in a row: the view, its array type, its row type and its `_RETURN` rule. When the rule is long, PostgreSQL moves its query to the TOAST table of `pg_rewrite`, and the TOAST value takes the next OID. The text of the rule in rupg is not the text of PostgreSQL, so [`VIEW_OIDS`] holds the OID of each view in PostgreSQL 19, and the views get the same OIDs as in PostgreSQL.
//!
//! The rules `pg_settings_u` and `pg_settings_n` on `pg_settings` come after the view, and [`RULE_OID`] holds the OID of the first rule in PostgreSQL 19.
//!
//! The `GRANT` and `REVOKE` statements of the script on views set the privileges of these views. Then each view without privileges gets `SELECT` for PUBLIC, as `setup_privileges` of `initdb` gives it. The statements on catalogs changed the static rows already.
//!
//! `snowball_create.sql` runs next. It makes the two functions in C of the Snowball dictionaries, the template `snowball`, and a dictionary and a configuration for each of the 31 languages, with their comments and the maps of the configurations. rupg makes the functions as `plpgsql` below, and runs the other statements of the script through the analyzer.
//!
//! `information_schema.sql` runs next, with `search_path` set to `information_schema` by the script. rupg makes the schema with its privileges, the five domains and the eleven functions in SQL, with the OIDs of PostgreSQL 19. The tables and the views of the script are not done yet.
//!
//! `setup_collation` runs before `snowball_create.sql`. It runs `pg_import_system_collations`, which makes a collation for each locale that `locale -a` prints, then a collation without the encoding tag for each name that has one, then a collation for each locale of ICU. The first collation gets the OID 12396 in PostgreSQL 19, and the OIDs of the next collations come from the locales of the system. The objects of `snowball_create.sql` have fixed OIDs in rupg, so rupg makes the collations last, with the same OIDs.
//!
//! Last, `initdb` runs `CREATE EXTENSION plpgsql`. rupg makes the extension, the three functions in C of `plpgsql--1.0.sql`, the language and their comments, with the OIDs of PostgreSQL 19.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rupg_catalog::{
    Catalog, Collation, Extension, FIRST_NORMAL_OID, FIRST_UNPINNED_OID, Function, Language,
    ObjRef, PG_EXTENSION, PG_LANGUAGE, PG_PROC, RelKind, collation_conflict,
};
use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_platform::os::OsClock;
use rupg_sql::nodes::{GrantStmt, Node, ObjectType, RoleSpecType};

use crate::connection::base_settings;
use crate::guc::{Action, Origin, Settings};
use crate::query::Reader;
use crate::utility::flatten;

/// The OID of the schema `pg_catalog`.
const PG_CATALOG: u32 = 11;

/// The bootstrap superuser, `BOOTSTRAP_SUPERUSERID`, which owns the system views.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// The privilege letters of a relation, in the order of `aclitemout`.
const RIGHTS: &str = "arwdDxtm";

/// The OID of the extension `plpgsql` in PostgreSQL 19. Its three functions and its language take the next four OIDs.
const PLPGSQL: u32 = 13704;

/// The OID of `dsnowball_init`, the first object of `snowball_create.sql` in PostgreSQL 19. The objects of the script take the next OIDs in order.
const SNOWBALL: u32 = 13285;

/// The languages of `snowball_create.pl` whose words in ASCII go to the dictionary of another language, and that language.
const ASCII_LANGUAGES: [(&str, &str); 2] = [("hindi", "english"), ("russian", "english")];

/// The stemmers of `dict_snowball` that `snowball_create.pl` makes no dictionary for.
const NO_DICTIONARY: [&str; 2] = ["dutch_porter", "porter"];

/// `ClanguageId`, the OID of the language `c`.
const C_LANGUAGE: u32 = 13;

/// The OID of the type `internal`.
const INTERNAL: u32 = 2281;

/// The OID of the first collation of `pg_import_system_collations` in PostgreSQL 19.
const COLLATION_OID: u32 = 12396;

/// The number of the encoding `SQL_ASCII`, the encoding of the locales `C` and `POSIX`.
const SQL_ASCII: i32 = 0;

/// `KOI8U`, the last encoding that a server can use, `PG_ENCODING_BE_LAST`.
const LAST_SERVER_ENCODING: i32 = 34;

/// `encoding_match_list` of `chklocale.c`: the names of the codesets of the C library, with the encoding of each. The codesets of the encodings that only a client can use are not here, because `pg_import_system_collations` skips their locales.
const CODESETS: [(&str, &str); 92] = [
    ("EUC-JP", "EUC_JP"),
    ("eucJP", "EUC_JP"),
    ("IBM-eucJP", "EUC_JP"),
    ("sdeckanji", "EUC_JP"),
    ("CP20932", "EUC_JP"),
    ("EUC-CN", "EUC_CN"),
    ("eucCN", "EUC_CN"),
    ("IBM-eucCN", "EUC_CN"),
    ("GB2312", "EUC_CN"),
    ("dechanzi", "EUC_CN"),
    ("CP20936", "EUC_CN"),
    ("EUC-KR", "EUC_KR"),
    ("eucKR", "EUC_KR"),
    ("IBM-eucKR", "EUC_KR"),
    ("deckorean", "EUC_KR"),
    ("5601", "EUC_KR"),
    ("CP51949", "EUC_KR"),
    ("EUC-TW", "EUC_TW"),
    ("eucTW", "EUC_TW"),
    ("IBM-eucTW", "EUC_TW"),
    ("cns11643", "EUC_TW"),
    ("UTF-8", "UTF8"),
    ("utf8", "UTF8"),
    ("CP65001", "UTF8"),
    ("ISO-8859-1", "LATIN1"),
    ("ISO8859-1", "LATIN1"),
    ("iso88591", "LATIN1"),
    ("CP28591", "LATIN1"),
    ("ISO-8859-2", "LATIN2"),
    ("ISO8859-2", "LATIN2"),
    ("iso88592", "LATIN2"),
    ("CP28592", "LATIN2"),
    ("ISO-8859-3", "LATIN3"),
    ("ISO8859-3", "LATIN3"),
    ("iso88593", "LATIN3"),
    ("CP28593", "LATIN3"),
    ("ISO-8859-4", "LATIN4"),
    ("ISO8859-4", "LATIN4"),
    ("iso88594", "LATIN4"),
    ("CP28594", "LATIN4"),
    ("ISO-8859-9", "LATIN5"),
    ("ISO8859-9", "LATIN5"),
    ("iso88599", "LATIN5"),
    ("CP28599", "LATIN5"),
    ("ISO-8859-10", "LATIN6"),
    ("ISO8859-10", "LATIN6"),
    ("iso885910", "LATIN6"),
    ("ISO-8859-13", "LATIN7"),
    ("ISO8859-13", "LATIN7"),
    ("iso885913", "LATIN7"),
    ("ISO-8859-14", "LATIN8"),
    ("ISO8859-14", "LATIN8"),
    ("iso885914", "LATIN8"),
    ("ISO-8859-15", "LATIN9"),
    ("ISO8859-15", "LATIN9"),
    ("iso885915", "LATIN9"),
    ("CP28605", "LATIN9"),
    ("ISO-8859-16", "LATIN10"),
    ("ISO8859-16", "LATIN10"),
    ("iso885916", "LATIN10"),
    ("KOI8-R", "KOI8R"),
    ("CP20866", "KOI8R"),
    ("KOI8-U", "KOI8U"),
    ("CP21866", "KOI8U"),
    ("CP866", "WIN866"),
    ("CP874", "WIN874"),
    ("CP1250", "WIN1250"),
    ("CP1251", "WIN1251"),
    ("ansi-1251", "WIN1251"),
    ("CP1252", "WIN1252"),
    ("CP1253", "WIN1253"),
    ("CP1254", "WIN1254"),
    ("CP1255", "WIN1255"),
    ("CP1256", "WIN1256"),
    ("CP1257", "WIN1257"),
    ("CP1258", "WIN1258"),
    ("ISO-8859-5", "ISO_8859_5"),
    ("ISO8859-5", "ISO_8859_5"),
    ("iso88595", "ISO_8859_5"),
    ("CP28595", "ISO_8859_5"),
    ("ISO-8859-6", "ISO_8859_6"),
    ("ISO8859-6", "ISO_8859_6"),
    ("iso88596", "ISO_8859_6"),
    ("CP28596", "ISO_8859_6"),
    ("ISO-8859-7", "ISO_8859_7"),
    ("ISO8859-7", "ISO_8859_7"),
    ("iso88597", "ISO_8859_7"),
    ("CP28597", "ISO_8859_7"),
    ("ISO-8859-8", "ISO_8859_8"),
    ("ISO8859-8", "ISO_8859_8"),
    ("iso88598", "ISO_8859_8"),
    ("CP28598", "ISO_8859_8"),
];

/// The first rule of the script and its OID in PostgreSQL 19. The view `pg_settings` takes the four OIDs before it, and the rule `pg_settings_n` takes the next OID.
const RULE_OID: (&str, u32) = ("pg_settings_u", 12114);

/// The OID of each system view in PostgreSQL 19, in the order of `system_views.sql`.
const VIEW_OIDS: [(&str, u32); 86] = [
    ("pg_roles", 12000),
    ("pg_shadow", 12005),
    ("pg_group", 12010),
    ("pg_user", 12014),
    ("pg_policies", 12018),
    ("pg_rules", 12023),
    ("pg_views", 12028),
    ("pg_tables", 12033),
    ("pg_matviews", 12038),
    ("pg_indexes", 12043),
    ("pg_sequences", 12048),
    ("pg_stats", 12053),
    ("pg_stats_ext", 12058),
    ("pg_stats_ext_exprs", 12063),
    ("pg_publication_tables", 12068),
    ("pg_publication_sequences", 12073),
    ("pg_locks", 12078),
    ("pg_cursors", 12082),
    ("pg_available_extensions", 12086),
    ("pg_available_extension_versions", 12091),
    ("pg_prepared_xacts", 12096),
    ("pg_prepared_statements", 12101),
    ("pg_seclabels", 12105),
    ("pg_settings", 12110),
    ("pg_file_settings", 12116),
    ("pg_hba_file_rules", 12120),
    ("pg_ident_file_mappings", 12124),
    ("pg_timezone_abbrevs", 12128),
    ("pg_timezone_names", 12132),
    ("pg_config", 12136),
    ("pg_shmem_allocations", 12140),
    ("pg_shmem_allocations_numa", 12144),
    ("pg_dsm_registry_allocations", 12148),
    ("pg_backend_memory_contexts", 12152),
    ("pg_stat_all_tables", 12156),
    ("pg_stat_xact_all_tables", 12161),
    ("pg_stat_sys_tables", 12166),
    ("pg_stat_xact_sys_tables", 12171),
    ("pg_stat_user_tables", 12176),
    ("pg_stat_xact_user_tables", 12181),
    ("pg_stat_autovacuum_scores", 12186),
    ("pg_statio_all_tables", 12191),
    ("pg_statio_sys_tables", 12196),
    ("pg_statio_user_tables", 12201),
    ("pg_stat_all_indexes", 12206),
    ("pg_stat_sys_indexes", 12211),
    ("pg_stat_user_indexes", 12216),
    ("pg_statio_all_indexes", 12221),
    ("pg_statio_sys_indexes", 12226),
    ("pg_statio_user_indexes", 12230),
    ("pg_statio_all_sequences", 12234),
    ("pg_statio_sys_sequences", 12239),
    ("pg_statio_user_sequences", 12243),
    ("pg_stat_activity", 12247),
    ("pg_stat_replication", 12252),
    ("pg_stat_slru", 12257),
    ("pg_stat_lock", 12261),
    ("pg_stat_wal_receiver", 12265),
    ("pg_stat_recovery", 12269),
    ("pg_stat_recovery_prefetch", 12273),
    ("pg_stat_subscription", 12277),
    ("pg_stat_ssl", 12282),
    ("pg_stat_gssapi", 12286),
    ("pg_replication_slots", 12290),
    ("pg_stat_replication_slots", 12295),
    ("pg_stat_database", 12300),
    ("pg_stat_database_conflicts", 12305),
    ("pg_stat_user_functions", 12309),
    ("pg_stat_xact_user_functions", 12314),
    ("pg_stat_archiver", 12319),
    ("pg_stat_bgwriter", 12323),
    ("pg_stat_checkpointer", 12327),
    ("pg_stat_io", 12331),
    ("pg_stat_wal", 12335),
    ("pg_stat_progress_analyze", 12339),
    ("pg_stat_progress_vacuum", 12344),
    ("pg_stat_progress_repack", 12349),
    ("pg_stat_progress_cluster", 12354),
    ("pg_stat_progress_create_index", 12359),
    ("pg_stat_progress_basebackup", 12364),
    ("pg_stat_progress_copy", 12369),
    ("pg_user_mappings", 12374),
    ("pg_replication_origin_status", 12379),
    ("pg_stat_subscription_stats", 12383),
    ("pg_wait_events", 12388),
    ("pg_aios", 12392),
];

/// The OID of the schema `information_schema` in PostgreSQL 19.
const INFORMATION_SCHEMA: u32 = 13350;

/// The OID of each view and each table of `information_schema` in PostgreSQL 19, in the order of the script. The array type takes the next OID, the row type the OID after that, and then the `_RETURN` rule of a view or the TOAST table and its index of a table. Most views use one more OID after their rule, because PostgreSQL keeps the long query tree of the rule in TOAST, and each value in TOAST takes an OID.
const INFORMATION_SCHEMA_OIDS: [(&str, u32); 69] = [
    ("information_schema_catalog_name", 13370),
    ("applicable_roles", 13379),
    ("administrable_role_authorizations", 13384),
    ("attributes", 13388),
    ("character_sets", 13393),
    ("check_constraint_routine_usage", 13398),
    ("check_constraints", 13403),
    ("collations", 13408),
    ("collation_character_set_applicability", 13413),
    ("column_column_usage", 13418),
    ("column_domain_usage", 13423),
    ("column_privileges", 13428),
    ("column_udt_usage", 13433),
    ("columns", 13438),
    ("constraint_column_usage", 13443),
    ("constraint_table_usage", 13448),
    ("domain_constraints", 13453),
    ("domain_udt_usage", 13458),
    ("domains", 13463),
    ("enabled_roles", 13468),
    ("key_column_usage", 13472),
    ("parameters", 13477),
    ("referential_constraints", 13482),
    ("role_column_grants", 13487),
    ("routine_column_usage", 13491),
    ("routine_privileges", 13496),
    ("role_routine_grants", 13501),
    ("routine_routine_usage", 13505),
    ("routine_sequence_usage", 13510),
    ("routine_table_usage", 13515),
    ("routines", 13520),
    ("schemata", 13525),
    ("sequences", 13530),
    ("sql_features", 13535),
    ("sql_implementation_info", 13540),
    ("sql_parts", 13545),
    ("sql_sizing", 13550),
    ("table_constraints", 13555),
    ("table_privileges", 13560),
    ("role_table_grants", 13565),
    ("tables", 13569),
    ("transforms", 13574),
    ("triggered_update_columns", 13579),
    ("triggers", 13584),
    ("udt_privileges", 13589),
    ("role_udt_grants", 13594),
    ("usage_privileges", 13598),
    ("role_usage_grants", 13603),
    ("user_defined_types", 13607),
    ("view_column_usage", 13612),
    ("view_routine_usage", 13617),
    ("view_table_usage", 13622),
    ("views", 13627),
    ("data_type_privileges", 13632),
    ("element_types", 13637),
    ("_pg_foreign_table_columns", 13642),
    ("column_options", 13647),
    ("_pg_foreign_data_wrappers", 13651),
    ("foreign_data_wrapper_options", 13656),
    ("foreign_data_wrappers", 13660),
    ("_pg_foreign_servers", 13664),
    ("foreign_server_options", 13669),
    ("foreign_servers", 13673),
    ("_pg_foreign_tables", 13677),
    ("foreign_table_options", 13682),
    ("foreign_tables", 13686),
    ("_pg_user_mappings", 13690),
    ("user_mapping_options", 13695),
    ("user_mappings", 13700),
];

/// The OID of the array type of each domain of `information_schema` in PostgreSQL 19. The domain takes the next OID, and its check constraint the OID after that.
const DOMAIN_OIDS: [(&str, u32); 5] = [
    ("cardinal_number", 13363),
    ("character_data", 13366),
    ("sql_identifier", 13368),
    ("time_stamp", 13374),
    ("yes_or_no", 13376),
];

/// The OID of each function of `information_schema` in PostgreSQL 19. The OID 13357 is the TOAST value of the long body of `_pg_char_octet_length`.
const FUNCTION_OIDS: [(&str, u32); 11] = [
    ("_pg_expandarray", 13351),
    ("_pg_index_position", 13352),
    ("_pg_truetypid", 13353),
    ("_pg_truetypmod", 13354),
    ("_pg_char_max_length", 13355),
    ("_pg_char_octet_length", 13356),
    ("_pg_numeric_precision", 13358),
    ("_pg_numeric_precision_radix", 13359),
    ("_pg_numeric_scale", 13360),
    ("_pg_datetime_precision", 13361),
    ("_pg_interval_type", 13362),
];

/// The catalog of a new cluster: the objects that `initdb` makes with SQL. The first call makes it on a thread with a large stack, because the queries of some views are deep.
// The thread runs once in the process before any session, as `initdb` runs before the server. It is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
#[allow(clippy::disallowed_methods)]
pub(crate) fn catalog() -> Arc<Catalog> {
    static CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();
    let made = CATALOG.get_or_init(|| {
        let run = || Arc::new(make().0);
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(run)
            .ok()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    });
    Arc::clone(made)
}

/// Runs `system_views.sql` on a catalog of the bootstrap. Also gives each statement that failed, with its error.
pub(crate) fn make() -> (Catalog, Vec<(String, Error)>) {
    // A test can make a second catalog. It runs after the first one, so that it finds the rows of `rupg-pgcatalog` that the first one added and does not make them again.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut catalog = Catalog::new();
    let mut failed = Vec::new();
    let text = rupg_pgcatalog::system_views();
    let Ok((list, _)) = rupg_sql::parse(text) else {
        return (catalog, vec![("system_views.sql".to_string(), Error::internal("no parse"))]);
    };
    let settings = match base_settings(&[
        ("search_path".to_string(), "pg_catalog".to_string()),
        ("allow_system_table_mods".to_string(), "on".to_string()),
    ]) {
        Ok(settings) => RefCell::new(settings),
        Err(error) => return (catalog, vec![("search_path".to_string(), error)]),
    };
    let clock = OsClock::new();
    let user = owner_name();
    catalog.set_next_oid(FIRST_UNPINNED_OID);
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else { continue };
        let Some(stmt) = raw.stmt.as_ref() else { continue };
        let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(text.len());
        let end =
            usize::try_from(raw.stmt_len).map_or(text.len(), |len| (start + len).min(text.len()));
        let statement = text.get(start..end).unwrap_or_default();
        match stmt {
            Node::ViewStmt(_) | Node::RuleStmt(_) => {
                let name = match stmt {
                    Node::ViewStmt(view) => view.view.as_ref().and_then(|rv| rv.relname.as_deref()),
                    Node::RuleStmt(rule) => rule.rulename.as_deref(),
                    _ => None,
                };
                let name = name.unwrap_or_default();
                if let Some(&(_, oid)) = VIEW_OIDS.iter().find(|(n, _)| *n == name) {
                    catalog.set_next_oid(oid);
                }
                if name == RULE_OID.0 {
                    catalog.set_next_oid(RULE_OID.1);
                }
                let reader = Reader::new(
                    &settings,
                    &user,
                    "postgres",
                    (0, 0),
                    &clock,
                    0,
                    Arc::new(catalog.clone()),
                );
                let mut work = catalog.clone();
                let defined =
                    rupg_analyze::define(stmt, statement, &reader, &mut work, BOOTSTRAP_SUPERUSER);
                match defined.result {
                    Ok(()) => catalog = work,
                    Err(error) => failed.push((name.to_string(), error)),
                }
            }
            Node::GrantStmt(grant) => {
                if let Err(error) = privileges(&mut catalog, grant, &user, PG_CATALOG) {
                    failed.push((statement.to_string(), error));
                }
            }
            _ => {}
        }
    }
    let views: Vec<u32> = catalog
        .relations()
        .filter(|r| r.kind == RelKind::View && r.acl.is_none())
        .map(|r| r.oid)
        .collect();
    for oid in views {
        let acl = vec![format!("{user}={RIGHTS}/{user}"), format!("=r/{user}")];
        if let Err(error) = catalog.set_acl(oid, Some(acl)) {
            failed.push((oid.to_string(), error));
        }
    }
    snowball(&mut catalog, &mut failed, &clock, &user);
    information_schema(&mut catalog, &mut failed, &clock, &user);
    if let Err(error) = plpgsql(&mut catalog) {
        failed.push(("CREATE EXTENSION plpgsql".to_string(), error));
    }
    collations(&mut catalog);
    catalog.set_next_oid(FIRST_NORMAL_OID);
    (catalog, failed)
}

/// `normalize_libc_locale_name`: the name of a locale without its encoding tags, such as `en_US` for `en_US.utf8` and `br_FR@euro` for `br_FR.iso885915@euro`. `None` when the name has no tag.
fn without_encoding(name: &str) -> Option<String> {
    if !name.contains('.') {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '.' {
            while chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '-').is_some() {}
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// `pg_get_encoding_from_locale`: the encoding of a locale of the C library. `None` when the locale does not exist or its codeset is not in [`CODESETS`].
fn locale_encoding(name: &str) -> Option<i32> {
    if name.eq_ignore_ascii_case("C") || name.eq_ignore_ascii_case("POSIX") {
        return Some(SQL_ASCII);
    }
    let codeset = rupg_platform::os::locale_codeset(name)?;
    let (_, encoding) = CODESETS.iter().find(|(c, _)| c.eq_ignore_ascii_case(&codeset))?;
    Some(rupg_func::char_to_encoding(encoding))
}

/// `pg_import_system_collations('pg_catalog')`, which `setup_collation` of `initdb` runs. Each locale of `locale -a` with an ASCII name and an encoding that a server can use gets a collation of the C library. `C`, `POSIX` and the other locales of `SQL_ASCII` get none, because the static rows have `C` and `POSIX`. A name with an encoding tag also gives an alias without the tag, after all the names, in the order of the locale names. Last, each locale of ICU gets a collation for any encoding, with its comment. A name that the schema has already for the encoding makes no collation.
fn collations(catalog: &mut Catalog) {
    catalog.set_next_oid(COLLATION_OID);
    let version = rupg_platform::os::libc_version();
    let libc = |name: &str, locale: &str, encoding: i32| {
        // `get_collation_actual_version_libc`: `C`, `POSIX` and the names that start with `C.` have no version.
        let plain = locale.eq_ignore_ascii_case("C")
            || locale.eq_ignore_ascii_case("POSIX")
            || locale.get(..2).is_some_and(|start| start.eq_ignore_ascii_case("C."));
        Collation {
            oid: 0,
            name: name.to_string(),
            namespace: PG_CATALOG,
            owner: BOOTSTRAP_SUPERUSER,
            provider: b'c',
            encoding,
            libc_locale: Some(locale.to_string()),
            locale: None,
            version: if plain { None } else { version.clone() },
        }
    };
    // The built-in collations, such as `C`, are not in the catalog, so their names are checked here.
    let create = |catalog: &mut Catalog, collation: Collation, comment: Option<&str>| {
        let taken = builtin::named(Named::Collation).iter().any(|row| {
            row.namespace == collation.namespace
                && row.name == collation.name
                && collation_conflict(row.encoding, collation.encoding)
        });
        if !taken {
            catalog.create_collation(collation, comment);
        }
    };
    let mut aliases = Vec::new();
    for locale in rupg_platform::os::system_locales() {
        if !locale.is_ascii() {
            continue;
        }
        let Some(encoding) = locale_encoding(&locale) else { continue };
        if !(SQL_ASCII + 1..=LAST_SERVER_ENCODING).contains(&encoding) {
            continue;
        }
        create(catalog, libc(&locale, &locale, encoding), None);
        if let Some(alias) = without_encoding(&locale) {
            aliases.push((locale, alias, encoding));
        }
    }
    aliases.sort_by(|a, b| a.0.cmp(&b.0));
    for (locale, alias, encoding) in &aliases {
        create(catalog, libc(alias, locale, *encoding), None);
    }
    for icu in rupg_pgcatalog::icu::locales() {
        let collation = Collation {
            oid: 0,
            name: format!("{}-x-icu", icu.tag),
            namespace: PG_CATALOG,
            owner: BOOTSTRAP_SUPERUSER,
            provider: b'i',
            encoding: -1,
            libc_locale: None,
            locale: Some(icu.tag.to_string()),
            version: Some(icu.version.to_string()),
        };
        create(catalog, collation, icu.comment);
    }
}

/// The text of `snowball_create.sql`, as `snowball_create.pl` makes it: `snowball_func.sql.in`, then `snowball.sql.in` for each language with its words in place. A language with a stop word file gets the option `StopWords`.
fn snowball_script() -> String {
    let mut script = rupg_pgcatalog::snowball_func().to_string();
    for stemmer in rupg_snowball::Stemmer::all() {
        let lang = stemmer.name();
        if NO_DICTIONARY.contains(&lang) {
            continue;
        }
        let ascii = ASCII_LANGUAGES.iter().find(|(l, _)| *l == lang).map_or(lang, |(_, a)| *a);
        let stop = match rupg_pgcatalog::stop_words(lang) {
            Some(_) => format!(", StopWords={lang}"),
            None => String::new(),
        };
        script.push_str(
            &rupg_pgcatalog::snowball_language()
                .replace("_LANGNAME_", lang)
                .replace("_DICTNAME_", &format!("{lang}_stem"))
                .replace("_CFGNAME_", lang)
                .replace("_ASCDICTNAME_", &format!("{ascii}_stem"))
                .replace("_NONASCDICTNAME_", &format!("{lang}_stem"))
                .replace("_STOPWORDS_", &stop),
        );
    }
    script
}

/// Runs `snowball_create.sql`. The two functions in C are made as the functions of `plpgsql` are made, and the other statements go through the analyzer.
fn snowball(catalog: &mut Catalog, failed: &mut Vec<(String, Error)>, clock: &OsClock, user: &str) {
    catalog.set_next_oid(SNOWBALL);
    for (name, nargs) in [("dsnowball_init", 1), ("dsnowball_lexize", 4)] {
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace: PG_CATALOG,
            owner: BOOTSTRAP_SUPERUSER,
            lang: C_LANGUAGE,
            cost: 1,
            rows: 0,
            support: 0,
            kind: b'f',
            strict: true,
            volatile: b'v',
            parallel: b'u',
            rettype: INTERNAL,
            retset: false,
            argtypes: vec![INTERNAL; nargs],
            allargtypes: Vec::new(),
            argmodes: Vec::new(),
            argnames: Vec::new(),
            argdefaults: None,
            nargdefaults: 0,
            src: name.to_string(),
            bin: "$libdir/dict_snowball".to_string(),
            body: None,
        };
        let made =
            catalog.create_function(function, &[]).and_then(|oid| add_system_proc(catalog, oid));
        if let Err(error) = made {
            failed.push((name.to_string(), error));
        }
    }
    let text = snowball_script();
    let Ok((list, _)) = rupg_sql::parse(&text) else {
        failed.push(("snowball_create.sql".to_string(), Error::internal("no parse")));
        return;
    };
    let settings = match base_settings(&[
        ("search_path".to_string(), "pg_catalog".to_string()),
        ("allow_system_table_mods".to_string(), "on".to_string()),
    ]) {
        Ok(settings) => RefCell::new(settings),
        Err(error) => {
            failed.push(("search_path".to_string(), error));
            return;
        }
    };
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else { continue };
        let Some(stmt) = raw.stmt.as_ref() else { continue };
        if matches!(stmt, Node::CreateFunctionStmt(_) | Node::VariableSetStmt(_)) {
            continue;
        }
        let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(text.len());
        let end =
            usize::try_from(raw.stmt_len).map_or(text.len(), |len| (start + len).min(text.len()));
        let statement = text.get(start..end).unwrap_or_default();
        if let Err(error) = define(catalog, &settings, (user, clock), stmt, statement) {
            failed.push((statement.to_string(), error));
        }
    }
}

/// Runs the parts of `information_schema.sql` that rupg can run: the schema, its privileges, the `search_path` of the script, the domains, the functions, the views, the tables and the privileges on the views and the tables. Each domain and each function also goes into the rows of `rupg-pgcatalog`, so that the analyzer and the executor find it as they find a built-in object. The rows of the tables are not there yet, because rupg cannot run `INSERT`.
fn information_schema(
    catalog: &mut Catalog,
    failed: &mut Vec<(String, Error)>,
    clock: &OsClock,
    user: &str,
) {
    let text = rupg_pgcatalog::information_schema();
    let Ok((list, _)) = rupg_sql::parse(text) else {
        failed.push(("information_schema.sql".to_string(), Error::internal("no parse")));
        return;
    };
    let settings = match base_settings(&[
        ("search_path".to_string(), "pg_catalog".to_string()),
        ("allow_system_table_mods".to_string(), "on".to_string()),
    ]) {
        Ok(settings) => RefCell::new(settings),
        Err(error) => {
            failed.push(("search_path".to_string(), error));
            return;
        }
    };
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else { continue };
        let Some(stmt) = raw.stmt.as_ref() else { continue };
        let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(text.len());
        let end =
            usize::try_from(raw.stmt_len).map_or(text.len(), |len| (start + len).min(text.len()));
        let statement = text.get(start..end).unwrap_or_default();
        let done = match stmt {
            Node::CreateSchemaStmt(schema) => {
                catalog.set_next_oid(INFORMATION_SCHEMA);
                let name = schema.schemaname.as_deref().unwrap_or_default();
                catalog.create_schema(name, BOOTSTRAP_SUPERUSER).map(|_| ())
            }
            Node::GrantStmt(grant) if grant.objtype == ObjectType::OBJECT_SCHEMA => {
                schema_privileges(catalog, grant, user)
            }
            Node::VariableSetStmt(set) => {
                let name = set.name.as_deref().unwrap_or_default();
                flatten(name, &set.args).and_then(|value| {
                    settings.borrow_mut().set(
                        name,
                        value.as_deref(),
                        Action::Set,
                        Origin::Statement,
                    )
                })
            }
            Node::CreateDomainStmt(domain) => {
                let name = names(&domain.domainname);
                match DOMAIN_OIDS.iter().find(|(n, _)| Some(*n) == name.last().copied()) {
                    Some(&(name, array)) => {
                        catalog.set_next_oid(array);
                        define(catalog, &settings, (user, clock), stmt, statement)
                            .and_then(|()| add_system_domain(catalog, name))
                    }
                    None => Err(Error::internal("a domain that PostgreSQL 19 does not have")),
                }
            }
            Node::CreateFunctionStmt(function) => {
                let name = names(&function.funcname);
                match FUNCTION_OIDS.iter().find(|(n, _)| Some(*n) == name.last().copied()) {
                    Some(&(_, oid)) => {
                        catalog.set_next_oid(oid);
                        define(catalog, &settings, (user, clock), stmt, statement)
                            .and_then(|()| add_system_proc(catalog, oid))
                    }
                    None => Ok(()),
                }
            }
            Node::ViewStmt(_) | Node::CreateStmt(_) => {
                let relation = match stmt {
                    Node::ViewStmt(view) => view.view.as_ref(),
                    Node::CreateStmt(table) => table.relation.as_ref(),
                    _ => None,
                };
                let name = relation.and_then(|rv| rv.relname.as_deref()).unwrap_or_default();
                match INFORMATION_SCHEMA_OIDS.iter().find(|(n, _)| *n == name) {
                    Some(&(_, oid)) => {
                        catalog.set_next_oid(oid);
                        define(catalog, &settings, (user, clock), stmt, statement)
                    }
                    None => Err(Error::internal("a relation that PostgreSQL 19 does not have")),
                }
            }
            Node::GrantStmt(grant) => privileges(catalog, grant, user, INFORMATION_SCHEMA),
            _ => Ok(()),
        };
        if let Err(error) = done {
            failed.push((statement.to_string(), error));
        }
    }
}

/// `load_plpgsql` of `initdb`: `CREATE EXTENSION plpgsql`. The extension gets the comment of its control file. Then `plpgsql--1.0.sql` makes the call handler, the handler of `DO`, the validator and the trusted language, each as a member of the extension, and gives the language the same comment.
fn plpgsql(catalog: &mut Catalog) -> Result<()> {
    const COMMENT: &str = "PL/pgSQL procedural language";
    catalog.set_next_oid(PLPGSQL);
    let extension = catalog.create_extension(Extension {
        oid: 0,
        name: "plpgsql".to_string(),
        owner: BOOTSTRAP_SUPERUSER,
        namespace: PG_CATALOG,
        relocatable: false,
        version: "1.0".to_string(),
    })?;
    catalog.set_description(ObjRef::new(PG_EXTENSION, extension), Some(COMMENT.to_string()));
    // The result types `language_handler` and `void`, and the argument types `internal` and `oid`.
    let mut handlers = [0; 3];
    for (slot, (name, rettype, argtypes, strict)) in handlers.iter_mut().zip([
        ("plpgsql_call_handler", 2280, vec![], false),
        ("plpgsql_inline_handler", 2278, vec![2281], true),
        ("plpgsql_validator", 2278, vec![26], true),
    ]) {
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace: PG_CATALOG,
            owner: BOOTSTRAP_SUPERUSER,
            lang: C_LANGUAGE,
            cost: 1,
            rows: 0,
            support: 0,
            kind: b'f',
            strict,
            volatile: b'v',
            parallel: b'u',
            rettype,
            retset: false,
            argtypes,
            allargtypes: Vec::new(),
            argmodes: Vec::new(),
            argnames: Vec::new(),
            argdefaults: None,
            nargdefaults: 0,
            src: name.to_string(),
            bin: "$libdir/plpgsql".to_string(),
            body: None,
        };
        *slot = catalog.create_function(function, &[])?;
        catalog.add_to_extension(ObjRef::new(PG_PROC, *slot), extension);
        // The casts to `regproc` and `regprocedure` find the handlers by the rows of `rupg-pgcatalog`.
        add_system_proc(catalog, *slot)?;
    }
    let [handler, inline, validator] = handlers;
    let language = catalog.create_language(Language {
        oid: 0,
        name: "plpgsql".to_string(),
        owner: BOOTSTRAP_SUPERUSER,
        trusted: true,
        handler,
        inline,
        validator,
    })?;
    catalog.add_to_extension(ObjRef::new(PG_LANGUAGE, language), extension);
    catalog.set_description(ObjRef::new(PG_LANGUAGE, language), Some(COMMENT.to_string()));
    Ok(())
}

/// Runs a statement of the script that defines an object, as the bootstrap superuser. The catalog changes only when the statement succeeds.
fn define(
    catalog: &mut Catalog,
    settings: &RefCell<Settings>,
    (user, clock): (&str, &OsClock),
    stmt: &Node,
    statement: &str,
) -> Result<()> {
    let reader =
        Reader::new(settings, user, "postgres", (0, 0), clock, 0, Arc::new(catalog.clone()));
    let mut work = catalog.clone();
    rupg_analyze::define(stmt, statement, &reader, &mut work, BOOTSTRAP_SUPERUSER).result?;
    *catalog = work;
    Ok(())
}

/// Adds the row of a function of `information_schema` or of `plpgsql` to the rows of `rupg-pgcatalog`, so that the analyzer and the casts to `regproc` find the function.
fn add_system_proc(catalog: &Catalog, oid: u32) -> Result<()> {
    let f = catalog.function(oid).ok_or_else(|| Error::internal(format!("no function {oid}")))?;
    if builtin::proc_by_oid(oid).is_some() {
        return Ok(());
    }
    // The rows live as long as the process, as the built-in rows do.
    let argnames: Option<&'static [&'static str]> = (!f.argnames.is_empty()).then(|| {
        let names: Vec<&'static str> = f.argnames.iter().map(|n| &*n.clone().leak()).collect();
        &*names.leak()
    });
    let allargtypes: Option<&'static [u32]> =
        (!f.allargtypes.is_empty()).then(|| &*f.allargtypes.clone().leak());
    let argmodes: Option<&'static [u8]> =
        (!f.argmodes.is_empty()).then(|| &*f.argmodes.clone().leak());
    builtin::add_system_proc(builtin::ProcRow {
        oid,
        name: f.name.clone().leak(),
        namespace: f.namespace,
        lang: f.lang,
        variadic: rupg_exec::variadic_type(catalog, f),
        kind: b'f',
        strict: f.strict,
        retset: f.retset,
        volatile: f.volatile,
        nargs: i16::try_from(f.argtypes.len()).unwrap_or(i16::MAX),
        nargdefaults: f.nargdefaults,
        rettype: f.rettype,
        argtypes: f.argtypes.clone().leak(),
        allargtypes,
        argmodes,
        argnames,
        argdefaults: f.argdefaults.clone().map(|text| &*text.leak()),
        src: "",
        sqlbody: None,
    });
    Ok(())
}

/// The names of a qualified name.
fn names(list: &[Option<Node>]) -> Vec<&str> {
    list.iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(&**s),
            _ => None,
        })
        .collect()
}

/// Adds the rows of a domain of `information_schema` and of its array type to the rows of `rupg-pgcatalog`, with its check constraints.
fn add_system_domain(catalog: &Catalog, name: &str) -> Result<()> {
    let namespace = INFORMATION_SCHEMA;
    let missing = || Error::internal(format!("no domain {name}"));
    let ty = catalog.type_by_name(namespace, name).ok_or_else(missing)?;
    let domain = ty.domain.as_ref().ok_or_else(missing)?;
    if builtin::type_by_oid(ty.oid).is_some() {
        return Ok(());
    }
    let array = catalog.type_by_oid(ty.array).ok_or_else(missing)?;
    let rows = builtin::domain_rows(builtin::NewDomainRows {
        oid: ty.oid,
        name: ty.name.clone(),
        array: array.oid,
        array_name: array.name.clone(),
        namespace,
        base: domain.base,
        typmod: domain.typmod,
        collation: domain.collation,
    });
    let (row, array_row) = rows.ok_or_else(missing)?;
    builtin::add_system_type(row);
    builtin::add_system_type(array_row);
    let mut checks: Vec<_> = catalog.constraints_of_domain(ty.oid).collect();
    checks.sort_by_key(|c| c.oid);
    builtin::add_system_domain(builtin::DomainRow {
        oid: ty.oid,
        not_null: domain.not_null,
        checks: checks
            .iter()
            .map(|c| (c.name.clone(), c.expr.clone().unwrap_or_default()))
            .collect(),
    });
    Ok(())
}

/// `ExecGrant_Namespace` for a `GRANT` of the script on a schema. The superuser runs it, so the owner is the grantor.
fn schema_privileges(catalog: &mut Catalog, grant: &GrantStmt, owner: &str) -> Result<()> {
    let mut letters = String::new();
    for privilege in grant.privileges.iter().flatten() {
        let Node::AccessPriv(privilege) = privilege else { continue };
        match privilege.priv_name.as_deref().unwrap_or_default() {
            "usage" => letters.push('U'),
            "create" => letters.push('C'),
            name => return Err(Error::internal(format!("unknown privilege {name}"))),
        }
    }
    for object in grant.objects.iter().flatten() {
        let Node::String(name) = object else { continue };
        let name: &str = name;
        let schema = catalog
            .schema_by_name(name)
            .ok_or_else(|| Error::internal(format!("no schema {name}")))?;
        let mut acl = vec![format!("{owner}=UC/{owner}")];
        for grantee in grant.grantees.iter().flatten() {
            let Node::RoleSpec(spec) = grantee else { continue };
            let grantee = if spec.roletype == RoleSpecType::ROLESPEC_PUBLIC {
                ""
            } else {
                spec.rolename.as_deref().unwrap_or_default()
            };
            acl.push(format!("{grantee}={letters}/{owner}"));
        }
        let oid = schema.oid;
        catalog.set_schema_acl(oid, Some(acl))?;
    }
    Ok(())
}

/// The name of the bootstrap superuser.
fn owner_name() -> String {
    builtin::roles()
        .iter()
        .find(|r| r.oid == BOOTSTRAP_SUPERUSER)
        .map_or_else(|| "postgres".to_string(), |r| r.name.to_string())
}

/// `ExecGrant_Relation` for a `GRANT` or `REVOKE` of a script on the relations of the schema `namespace`. The superuser runs it, so the owner is the grantor. A statement on a catalog changes nothing here.
fn privileges(catalog: &mut Catalog, grant: &GrantStmt, owner: &str, namespace: u32) -> Result<()> {
    let mut bits = 0;
    for privilege in grant.privileges.iter().flatten() {
        let Node::AccessPriv(privilege) = privilege else { continue };
        if !privilege.cols.is_empty() {
            return Ok(());
        }
        let name = privilege.priv_name.as_deref().unwrap_or_default();
        let letter = match name {
            "select" => 'r',
            "insert" => 'a',
            "update" => 'w',
            "delete" => 'd',
            "truncate" => 'D',
            "references" => 'x',
            "trigger" => 't',
            "maintain" => 'm',
            _ => return Err(Error::internal(format!("unknown privilege {name}"))),
        };
        bits |= 1 << RIGHTS.find(letter).unwrap_or(0);
    }
    if grant.privileges.is_empty() {
        bits = (1 << RIGHTS.len()) - 1;
    }
    let mut grantees = Vec::new();
    for grantee in grant.grantees.iter().flatten() {
        let Node::RoleSpec(spec) = grantee else { continue };
        grantees.push(if spec.roletype == RoleSpecType::ROLESPEC_PUBLIC {
            String::new()
        } else {
            spec.rolename.as_deref().unwrap_or_default().to_string()
        });
    }
    for object in grant.objects.iter().flatten() {
        let Node::RangeVar(rv) = object else { continue };
        let name = rv.relname.as_deref().unwrap_or_default();
        let Some(view) = catalog.relation_by_name(namespace, name) else { continue };
        let oid = view.oid;
        let mut items = match &view.acl {
            Some(acl) => acl.iter().map(|item| parse_item(item)).collect::<Result<Vec<_>>>()?,
            None => vec![(owner.to_string(), (1 << RIGHTS.len()) - 1)],
        };
        for grantee in &grantees {
            match items.iter_mut().find(|(g, _)| g == grantee) {
                Some(item) if grant.is_grant => item.1 |= bits,
                Some(item) => item.1 &= !bits,
                None if grant.is_grant => items.push((grantee.clone(), bits)),
                None => {}
            }
        }
        items.retain(|(_, b)| *b != 0);
        let acl = items
            .iter()
            .map(|(grantee, bits)| {
                let letters: String = RIGHTS
                    .chars()
                    .enumerate()
                    .filter(|(i, _)| bits & (1 << i) != 0)
                    .map(|(_, c)| c)
                    .collect();
                format!("{grantee}={letters}/{owner}")
            })
            .collect();
        catalog.set_acl(oid, Some(acl))?;
    }
    Ok(())
}

/// The grantee and the privilege bits of an item of `aclitem[]` that [`privileges`] wrote.
fn parse_item(item: &str) -> Result<(String, u32)> {
    let bad = || Error::internal(format!("bad aclitem {item}"));
    let (grantee, rest) = item.split_once('=').ok_or_else(bad)?;
    let (letters, _) = rest.split_once('/').ok_or_else(bad)?;
    let mut bits = 0;
    for c in letters.chars() {
        bits |= 1 << RIGHTS.find(c).ok_or_else(bad)?;
    }
    Ok((grantee.to_string(), bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::disallowed_methods)] // A test thread, not a task of the engine.
    fn makes_the_views_of_system_views_sql() {
        let (catalog, failed) =
            std::thread::Builder::new().stack_size(64 << 20).spawn(make).unwrap().join().unwrap();
        let failed: Vec<(&str, &str)> =
            failed.iter().map(|(name, error)| (name.as_str(), error.message())).collect();
        assert_eq!(failed, []);
        assert_eq!(catalog.next_oid(), FIRST_NORMAL_OID);
        let views: Vec<(&str, u32)> = catalog
            .relations()
            .filter(|r| r.kind == RelKind::View && r.namespace == PG_CATALOG)
            .map(|r| (r.name.as_str(), r.oid))
            .collect();
        assert_eq!(views, VIEW_OIDS);
        let relations: Vec<(&str, u32)> = catalog
            .relations()
            .filter(|r| r.namespace == INFORMATION_SCHEMA)
            .map(|r| (r.name.as_str(), r.oid))
            .collect();
        assert_eq!(relations, INFORMATION_SCHEMA_OIDS);
        let dicts: Vec<(&str, u32)> =
            catalog.ts_dictionaries().map(|d| (d.name.as_str(), d.oid)).take(2).collect();
        assert_eq!(dicts, [("arabic_stem", 13288), ("armenian_stem", 13290)]);
        assert_eq!(
            catalog.ts_configs().last().map(|c| (c.name.as_str(), c.oid)),
            Some(("yiddish", 13349))
        );
        assert_eq!(catalog.ts_mappings().len(), 31 * 19);
    }
}

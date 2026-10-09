//! The objects that `initdb` makes with SQL after the bootstrap: the system views of `system_views.sql` and the schema `information_schema` of `information_schema.sql`.
//!
//! `initdb` runs the script in one backend as the bootstrap superuser, with `search_path` set to `pg_catalog` and `allow_system_table_mods` on. rupg runs each `CREATE VIEW` of the script through the analyzer in the same way, once in each process, and each new store starts from the result.
//!
//! A view takes four OIDs in a row: the view, its array type, its row type and its `_RETURN` rule. When the rule is long, PostgreSQL moves its query to the TOAST table of `pg_rewrite`, and the TOAST value takes the next OID. The text of the rule in rupg is not the text of PostgreSQL, so [`VIEW_OIDS`] holds the OID of each view in PostgreSQL 19, and the views get the same OIDs as in PostgreSQL.
//!
//! The `GRANT` and `REVOKE` statements of the script on views set the privileges of these views. Then each view without privileges gets `SELECT` for PUBLIC, as `setup_privileges` of `initdb` gives it. The statements on catalogs changed the static rows already.
//!
//! `information_schema.sql` runs next, with `search_path` set to `information_schema` by the script. rupg makes the schema with its privileges, the five domains and the functions in SQL with a `RETURN` body, with the OIDs of PostgreSQL 19. The other functions, the tables and the views of the script are not done yet.
//!
//! These parts of `system_views.sql` are not done yet: the view `pg_stats_ext_exprs`, which has a set-returning function in its target list, and the rules `pg_settings_u` and `pg_settings_n` on `pg_settings`. Their OIDs stay unused.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use rupg_catalog::{Catalog, FIRST_NORMAL_OID, FIRST_UNPINNED_OID, RelKind};
use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin;
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

/// The OID of the array type of each domain of `information_schema` in PostgreSQL 19. The domain takes the next OID, and its check constraint the OID after that.
const DOMAIN_OIDS: [(&str, u32); 5] = [
    ("cardinal_number", 13363),
    ("character_data", 13366),
    ("sql_identifier", 13368),
    ("time_stamp", 13374),
    ("yes_or_no", 13376),
];

/// The OID of each function of `information_schema` in PostgreSQL 19 that rupg makes. The OID 13357 is the TOAST value of the long body of `_pg_char_octet_length`. The functions `_pg_expandarray`, `_pg_index_position`, `_pg_truetypid` and `_pg_truetypmod` take the OIDs 13351 to 13354 and are not done yet.
const FUNCTION_OIDS: [(&str, u32); 7] = [
    ("_pg_char_max_length", 13355),
    ("_pg_char_octet_length", 13356),
    ("_pg_numeric_precision", 13358),
    ("_pg_numeric_precision_radix", 13359),
    ("_pg_numeric_scale", 13360),
    ("_pg_datetime_precision", 13361),
    ("_pg_interval_type", 13362),
];

/// The views of the script that rupg cannot make yet.
const NOT_YET: [&str; 1] = ["pg_stats_ext_exprs"];

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

/// Runs `system_views.sql` on a catalog of the bootstrap. Also gives each statement that failed, except the views of [`NOT_YET`], with its error.
pub(crate) fn make() -> (Catalog, Vec<(String, Error)>) {
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
            Node::ViewStmt(view) => {
                let name =
                    view.view.as_ref().and_then(|rv| rv.relname.as_deref()).unwrap_or_default();
                if let Some(&(_, oid)) = VIEW_OIDS.iter().find(|(n, _)| *n == name) {
                    catalog.set_next_oid(oid);
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
                    Err(_) if NOT_YET.contains(&name) => {}
                    Err(error) => failed.push((name.to_string(), error)),
                }
            }
            Node::GrantStmt(grant) => {
                if let Err(error) = privileges(&mut catalog, grant, &user) {
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
    information_schema(&mut catalog, &mut failed, &clock, &user);
    catalog.set_next_oid(FIRST_NORMAL_OID);
    (catalog, failed)
}

/// Runs the parts of `information_schema.sql` that rupg can run: the schema, its privileges, the `search_path` of the script and the domains. Each domain also goes into the rows of `rupg-pgcatalog`, so that the analyzer and the executor find it as they find a built-in type.
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
            _ => Ok(()),
        };
        if let Err(error) = done {
            failed.push((statement.to_string(), error));
        }
    }
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

/// Adds the row of a function of `information_schema` to the rows of `rupg-pgcatalog`, so that the analyzer finds the function.
fn add_system_proc(catalog: &Catalog, oid: u32) -> Result<()> {
    let f = catalog.function(oid).ok_or_else(|| Error::internal(format!("no function {oid}")))?;
    // The rows live as long as the process, as the built-in rows do.
    let argnames: Option<&'static [&'static str]> = (!f.argnames.is_empty()).then(|| {
        let names: Vec<&'static str> = f.argnames.iter().map(|n| &*n.clone().leak()).collect();
        &*names.leak()
    });
    builtin::add_system_proc(builtin::ProcRow {
        oid,
        name: f.name.clone().leak(),
        namespace: f.namespace,
        lang: f.lang,
        variadic: 0,
        kind: b'f',
        strict: f.strict,
        retset: false,
        volatile: f.volatile,
        nargs: i16::try_from(f.argtypes.len()).unwrap_or(i16::MAX),
        nargdefaults: 0,
        rettype: f.rettype,
        argtypes: f.argtypes.clone().leak(),
        allargtypes: None,
        argmodes: None,
        argnames,
        argdefaults: None,
        src: "",
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

/// `ExecGrant_Relation` for a `GRANT` or `REVOKE` of the script on views. The superuser runs it, so the owner is the grantor. A statement on a catalog changes nothing here.
fn privileges(catalog: &mut Catalog, grant: &GrantStmt, owner: &str) -> Result<()> {
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
        let Some(view) = catalog.relation_by_name(PG_CATALOG, name) else { continue };
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
            .filter(|r| r.kind == RelKind::View)
            .map(|r| (r.name.as_str(), r.oid))
            .collect();
        let expected: Vec<(&str, u32)> =
            VIEW_OIDS.iter().copied().filter(|(name, _)| !NOT_YET.contains(name)).collect();
        assert_eq!(views, expected);
    }
}

//! The access privilege inquiry functions: `has_table_privilege` and the other `has_*_privilege` functions, `pg_has_role`, and `acldefault` and `aclexplode`, which give the items of an `aclitem[]` value. The module also has the input of `aclitem`, `makeaclitem`, `aclcontains` and the hash functions of `aclitem`.
//!
//! A port of the functions of `src/backend/utils/adt/acl.c` and of the checks of `aclchk.c` that they call. The privileges are the `aclitem[]` values of the static rows, which have the privileges that `initdb` gives. Each function has forms with a role name or OID first, or no role for the current user, and with the object as text or as an OID. The form of a call is clear from its values: a name is text and an OID is an OID. A form with the OID of an object that does not exist gives null.

use rupg_catalog::{ObjRef, PG_CLASS, PG_LANGUAGE, PG_NAMESPACE, PG_PROC, PG_TYPE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned};
use rupg_types::{RegKind, Value, qualified_name_list};

use crate::reg;
use crate::{Call, Kernel, Session, bad_value, not_yet, type_error};

const ACL_INSERT: u32 = 1 << 0;
const ACL_SELECT: u32 = 1 << 1;
const ACL_UPDATE: u32 = 1 << 2;
const ACL_DELETE: u32 = 1 << 3;
const ACL_TRUNCATE: u32 = 1 << 4;
const ACL_REFERENCES: u32 = 1 << 5;
const ACL_TRIGGER: u32 = 1 << 6;
const ACL_EXECUTE: u32 = 1 << 7;
const ACL_USAGE: u32 = 1 << 8;
pub(crate) const ACL_CREATE: u32 = 1 << 9;
const ACL_CREATE_TEMP: u32 = 1 << 10;
pub(crate) const ACL_CONNECT: u32 = 1 << 11;
const ACL_SET: u32 = 1 << 12;
const ACL_ALTER_SYSTEM: u32 = 1 << 13;
const ACL_MAINTAIN: u32 = 1 << 14;
/// The grant option bits of all privileges, `ACLITEM_ALL_GOPTION_BITS`.
const ALL_GRANT_OPTIONS: u32 = 0xFFFF << 16;

/// `ACL_ALL_RIGHTS_STR`: the letter of each privilege bit, from bit 0.
const RIGHTS: &str = "arwdDxtXUCTcsAm";

/// `ACL_ID_PUBLIC`, the grantee of an item for all roles.
const PUBLIC: u32 = 0;
/// `BOOTSTRAP_SUPERUSERID`.
const BOOTSTRAP_SUPERUSER: u32 = 10;
/// `ROLE_PG_DATABASE_OWNER`.
const PG_DATABASE_OWNER: u32 = 6171;
/// `ROLE_PG_READ_ALL_DATA`.
const PG_READ_ALL_DATA: u32 = 6181;
/// `ROLE_PG_WRITE_ALL_DATA`.
const PG_WRITE_ALL_DATA: u32 = 6182;
/// `ROLE_PG_MAINTAIN`.
const PG_MAINTAIN: u32 = 6337;
/// `FirstUnpinnedObjectId`: a relation with a lower OID is a system catalog.
const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;
/// `PG_TOAST_NAMESPACE`.
const PG_TOAST: u32 = 99;
/// `F_ARRAY_SUBSCRIPT_HANDLER`, the subscript handler of a true array type.
const ARRAY_SUBSCRIPT_HANDLER: u32 = 6179;

/// A privilege name of a function and its bits, as the `priv_map` arrays of `acl.c` give them.
type PrivMap = &'static [(&'static str, u32)];

/// `ACL_GRANT_OPTION_FOR`.
const fn go(bits: u32) -> u32 {
    bits << 16
}

const TABLE_PRIVS: PrivMap = &[
    ("SELECT", ACL_SELECT),
    ("SELECT WITH GRANT OPTION", go(ACL_SELECT)),
    ("INSERT", ACL_INSERT),
    ("INSERT WITH GRANT OPTION", go(ACL_INSERT)),
    ("UPDATE", ACL_UPDATE),
    ("UPDATE WITH GRANT OPTION", go(ACL_UPDATE)),
    ("DELETE", ACL_DELETE),
    ("DELETE WITH GRANT OPTION", go(ACL_DELETE)),
    ("TRUNCATE", ACL_TRUNCATE),
    ("TRUNCATE WITH GRANT OPTION", go(ACL_TRUNCATE)),
    ("REFERENCES", ACL_REFERENCES),
    ("REFERENCES WITH GRANT OPTION", go(ACL_REFERENCES)),
    ("TRIGGER", ACL_TRIGGER),
    ("TRIGGER WITH GRANT OPTION", go(ACL_TRIGGER)),
    ("MAINTAIN", ACL_MAINTAIN),
    ("MAINTAIN WITH GRANT OPTION", go(ACL_MAINTAIN)),
];
const SEQUENCE_PRIVS: PrivMap = &[
    ("USAGE", ACL_USAGE),
    ("USAGE WITH GRANT OPTION", go(ACL_USAGE)),
    ("SELECT", ACL_SELECT),
    ("SELECT WITH GRANT OPTION", go(ACL_SELECT)),
    ("UPDATE", ACL_UPDATE),
    ("UPDATE WITH GRANT OPTION", go(ACL_UPDATE)),
];
const COLUMN_PRIVS: PrivMap = &[
    ("SELECT", ACL_SELECT),
    ("SELECT WITH GRANT OPTION", go(ACL_SELECT)),
    ("INSERT", ACL_INSERT),
    ("INSERT WITH GRANT OPTION", go(ACL_INSERT)),
    ("UPDATE", ACL_UPDATE),
    ("UPDATE WITH GRANT OPTION", go(ACL_UPDATE)),
    ("REFERENCES", ACL_REFERENCES),
    ("REFERENCES WITH GRANT OPTION", go(ACL_REFERENCES)),
];
const DATABASE_PRIVS: PrivMap = &[
    ("CREATE", ACL_CREATE),
    ("CREATE WITH GRANT OPTION", go(ACL_CREATE)),
    ("TEMPORARY", ACL_CREATE_TEMP),
    ("TEMPORARY WITH GRANT OPTION", go(ACL_CREATE_TEMP)),
    ("TEMP", ACL_CREATE_TEMP),
    ("TEMP WITH GRANT OPTION", go(ACL_CREATE_TEMP)),
    ("CONNECT", ACL_CONNECT),
    ("CONNECT WITH GRANT OPTION", go(ACL_CONNECT)),
];
const USAGE_PRIVS: PrivMap = &[("USAGE", ACL_USAGE), ("USAGE WITH GRANT OPTION", go(ACL_USAGE))];
const FUNCTION_PRIVS: PrivMap =
    &[("EXECUTE", ACL_EXECUTE), ("EXECUTE WITH GRANT OPTION", go(ACL_EXECUTE))];
const SCHEMA_PRIVS: PrivMap = &[
    ("CREATE", ACL_CREATE),
    ("CREATE WITH GRANT OPTION", go(ACL_CREATE)),
    ("USAGE", ACL_USAGE),
    ("USAGE WITH GRANT OPTION", go(ACL_USAGE)),
];
const TABLESPACE_PRIVS: PrivMap =
    &[("CREATE", ACL_CREATE), ("CREATE WITH GRANT OPTION", go(ACL_CREATE))];
const PARAMETER_PRIVS: PrivMap = &[
    ("SET", ACL_SET),
    ("SET WITH GRANT OPTION", go(ACL_SET)),
    ("ALTER SYSTEM", ACL_ALTER_SYSTEM),
    ("ALTER SYSTEM WITH GRANT OPTION", go(ACL_ALTER_SYSTEM)),
];
const LARGEOBJECT_PRIVS: PrivMap = &[
    ("SELECT", ACL_SELECT),
    ("SELECT WITH GRANT OPTION", go(ACL_SELECT)),
    ("UPDATE", ACL_UPDATE),
    ("UPDATE WITH GRANT OPTION", go(ACL_UPDATE)),
];
/// The privileges of `pg_has_role`. `MEMBER` is `ACL_CREATE`, and each form with an option is the grant option of `ACL_CREATE`.
const ROLE_PRIVS: PrivMap = &[
    ("USAGE", ACL_USAGE),
    ("MEMBER", ACL_CREATE),
    ("SET", ACL_SET),
    ("USAGE WITH GRANT OPTION", go(ACL_CREATE)),
    ("USAGE WITH ADMIN OPTION", go(ACL_CREATE)),
    ("MEMBER WITH GRANT OPTION", go(ACL_CREATE)),
    ("MEMBER WITH ADMIN OPTION", go(ACL_CREATE)),
    ("SET WITH GRANT OPTION", go(ACL_CREATE)),
    ("SET WITH ADMIN OPTION", go(ACL_CREATE)),
];

/// The white space of `isspace` in the C locale.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// `convert_any_priv_string`: the bits of a list of privilege names with commas between them. Each name is compared without case after the white space at its ends is removed.
///
/// # Errors
///
/// `22023` for a name that the map does not have, which includes an empty name.
fn privileges(text: &str, map: PrivMap) -> Result<u32> {
    let mut bits = 0;
    for chunk in text.split(',') {
        let chunk = chunk.trim_matches(is_space);
        let Some(found) = map.iter().find(|p| p.0.eq_ignore_ascii_case(chunk)) else {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized privilege type: \"{chunk}\""),
            ));
        };
        bits |= found.1;
    }
    Ok(bits)
}

/// The OID of the role with this name. The login role of the server is the bootstrap superuser, so the name of the current user also gives its OID.
fn role_oid(name: &str, session: &dyn Session) -> Option<u32> {
    rupg_pgcatalog::roles::current().oid(name).or_else(|| {
        (name == session.user() || name == session.session_user()).then_some(BOOTSTRAP_SUPERUSER)
    })
}

/// `get_role_oid`.
///
/// # Errors
///
/// `42704` for a role that does not exist.
fn get_role_oid(name: &str, session: &dyn Session) -> Result<u32> {
    role_oid(name, session).ok_or_else(|| {
        Error::new(SqlState::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist"))
    })
}

/// `get_role_oid_or_public`: [`get_role_oid`], with `public` for all roles.
fn get_role_oid_or_public(name: &str, session: &dyn Session) -> Result<u32> {
    if name == "public" { Ok(PUBLIC) } else { get_role_oid(name, session) }
}

/// `GetUserId`: the OID of the current user.
fn current_user(session: &dyn Session) -> u32 {
    role_oid(session.user(), session).unwrap_or(BOOTSTRAP_SUPERUSER)
}

/// `superuser_arg`. A role that does not exist is not a superuser.
fn superuser(role: u32) -> bool {
    rupg_pgcatalog::roles::current().superuser(role)
}

/// The walks of `roles_is_member_of`.
#[derive(Clone, Copy, PartialEq)]
enum Recurse {
    /// Each membership.
    Members,
    /// The memberships with `inherit_option`.
    Privs,
    /// The memberships with `set_option`.
    SetRole,
}

/// `roles_is_member_of`: the roles that `role` is a member of, directly or indirectly, with `role` first. The owner of the current database is a member of `pg_database_owner`, and `initdb` makes the bootstrap superuser the owner of each database. The second value is true when a role of the walk has the admin option on `admin_of`.
fn roles_of(role: u32, how: Recurse, admin_of: Option<u32>) -> (Vec<u32>, bool) {
    let created = rupg_pgcatalog::roles::current();
    let mut roles = vec![role];
    let mut admin = false;
    let mut at = 0;
    while let Some(&member) = roles.get(at) {
        at += 1;
        for edge in created.edges().filter(|m| m.member == member) {
            if Some(edge.role) == admin_of && edge.admin {
                admin = true;
            }
            if (how == Recurse::Privs && !edge.inherit) || (how == Recurse::SetRole && !edge.set) {
                continue;
            }
            if !roles.contains(&edge.role) {
                roles.push(edge.role);
            }
        }
        if member == BOOTSTRAP_SUPERUSER && !roles.contains(&PG_DATABASE_OWNER) {
            roles.push(PG_DATABASE_OWNER);
        }
    }
    (roles, admin)
}

/// `has_privs_of_role`, `is_member_of_role` and `member_can_set_role`: true when `member` is `role`, is a superuser, or reaches `role` with the walk.
fn reaches(member: u32, role: u32, how: Recurse) -> bool {
    member == role || superuser(member) || roles_of(member, how, None).0.contains(&role)
}

/// `has_privs_of_role`.
fn has_privs_of_role(member: u32, role: u32) -> bool {
    reaches(member, role, Recurse::Privs)
}

/// `HAS_PGSTAT_PERMISSIONS`: the current user has the privileges of `pg_read_all_stats` or of `role`, so it sees all the columns of a session of `role`.
pub(crate) fn has_pgstat_permissions(session: &dyn Session, role: u32) -> bool {
    reads_all_stats(session) || has_privs_of_role(current_user(session), role)
}

/// True when the current user has the privileges of `pg_read_all_stats`.
pub(crate) fn reads_all_stats(session: &dyn Session) -> bool {
    let user = current_user(session);
    role_oid("pg_read_all_stats", session).is_some_and(|stats| has_privs_of_role(user, stats))
}

/// `object_aclcheck` for the current user: true when the user has a privilege of `mask` on the object.
pub(crate) fn current_user_has(
    kind: Owned,
    oid: u32,
    mask: u32,
    session: &dyn Session,
) -> Result<bool> {
    Ok(object_check(kind, oid, current_user(session), mask, session)?.unwrap_or(false))
}

/// `is_admin_of_role`: true when `member` is a superuser, or when a role that `member` is a member of has the admin option on `role`. A role is not an admin of itself.
fn is_admin_of_role(member: u32, role: u32) -> bool {
    if superuser(member) {
        return true;
    }
    member != role && roles_of(member, Recurse::Members, Some(role)).1
}

/// The kinds of object of `acldefault`.
#[derive(Clone, Copy)]
enum AclKind {
    Table,
    Sequence,
    Database,
    Function,
    Language,
    Schema,
    Tablespace,
    Type,
    Other,
    Column,
    LargeObject,
    Parameter,
}

/// `acldefault`: the privileges of an object with a null ACL, as (grantee, bits). The owner has all the privileges of the kind, and PUBLIC has some of them on some kinds.
fn acl_default(kind: AclKind, owner: u32) -> Vec<(u32, u32)> {
    let (world, all) = match kind {
        AclKind::Table => (0, "arwdDxtm"),
        AclKind::Sequence => (0, "rwU"),
        AclKind::Database => (ACL_CREATE_TEMP | ACL_CONNECT, "CTc"),
        AclKind::Function => (ACL_EXECUTE, "X"),
        AclKind::Language | AclKind::Type => (ACL_USAGE, "U"),
        AclKind::Schema => (0, "UC"),
        AclKind::Tablespace => (0, "C"),
        AclKind::Other => (0, "U"),
        AclKind::Column => (0, ""),
        AclKind::LargeObject => (0, "rw"),
        AclKind::Parameter => (0, "sA"),
    };
    let all = all.chars().filter_map(|c| RIGHTS.find(c)).fold(0, |bits, at| bits | 1 << at);
    let mut items = Vec::new();
    if world != 0 {
        items.push((PUBLIC, world));
    }
    if all != 0 {
        items.push((owner, all));
    }
    items
}

/// Reads an `aclitem[]` value as (grantee, bits).
fn parse_acl(acl: &[&str]) -> Result<Vec<(u32, u32)>> {
    let bad = || Error::internal("a static aclitem that the engine cannot read");
    let mut items = Vec::with_capacity(acl.len());
    for item in acl {
        let (grantee, rest) = item.split_once('=').ok_or_else(bad)?;
        let (letters, _grantor) = rest.split_once('/').ok_or_else(bad)?;
        let grantee = if grantee.is_empty() {
            PUBLIC
        } else {
            let name = match grantee.strip_prefix('"').and_then(|g| g.strip_suffix('"')) {
                Some(quoted) => quoted.replace("\"\"", "\""),
                None => grantee.to_string(),
            };
            // A role that a session dropped or renamed after the grant has no row.
            let Some(oid) = rupg_pgcatalog::roles::current().oid(&name) else { continue };
            oid
        };
        let mut bits = 0;
        let mut last = 0;
        for c in letters.chars() {
            if c == '*' {
                bits |= last << 16;
            } else {
                last = 1 << RIGHTS.find(c).ok_or_else(bad)?;
                bits |= last;
            }
        }
        items.push((grantee, bits));
    }
    Ok(items)
}

/// The privileges of an object: its `aclitem[]` value, or the default of its kind.
fn acl_of(acl: Option<&[&str]>, kind: AclKind, owner: u32) -> Result<Vec<(u32, u32)>> {
    match acl {
        Some(acl) => parse_acl(acl),
        None => Ok(acl_default(kind, owner)),
    }
}

/// `aclmask` with `ACLMASK_ANY`: the bits of `mask` that `role` has. The owner, and each role with the privileges of the owner, has all grant options. Then the items of PUBLIC and of `role` count, then the items of the roles whose privileges `role` has.
fn aclmask(acl: &[(u32, u32)], role: u32, owner: u32, mask: u32) -> u32 {
    if mask & ALL_GRANT_OPTIONS != 0 && has_privs_of_role(role, owner) {
        return mask & ALL_GRANT_OPTIONS;
    }
    let mut result = 0;
    for &(grantee, bits) in acl {
        if grantee == PUBLIC || grantee == role {
            result |= bits & mask;
            if result != 0 {
                return result;
            }
        }
    }
    for &(grantee, bits) in acl {
        if grantee == PUBLIC || grantee == role || bits & mask & !result == 0 {
            continue;
        }
        if has_privs_of_role(role, grantee) {
            result |= bits & mask;
            if result != 0 {
                return result;
            }
        }
    }
    result
}

/// The result of a check: `None` when the object does not exist, as the `is_missing` of PostgreSQL gives it.
type Check = Result<Option<bool>>;

/// A relation as the checks of its privileges read it.
struct Class<'a> {
    oid: u32,
    name: &'a str,
    namespace: u32,
    /// The `relkind` letter.
    kind: char,
    /// The number of user columns.
    natts: i16,
    owner: u32,
    /// `relacl`, or `None` for the default privileges of the owner.
    acl: Option<Vec<&'a str>>,
}

/// The ACL that `GRANT` or `REVOKE` gave to an object of the catalog of the session or a built-in object, or `None` when the object keeps the ACL that it was made with. The ACL of a column is empty when it is null.
fn granted(object: ObjRef, session: &dyn Session) -> Option<Vec<&str>> {
    let acl = session.catalog()?.object_acl(object)?;
    Some(acl.iter().map(String::as_str).collect())
}

/// The ACL of a column: the ACL that `GRANT` gave it, or else the ACL that it was made with.
fn column_acl<'a>(
    relid: u32,
    num: i16,
    acl: Option<&'a [&'a str]>,
    session: &'a dyn Session,
) -> Option<Vec<&'a str>> {
    match granted(ObjRef::column(relid, num), session) {
        Some(acl) => Some(acl).filter(|acl| !acl.is_empty()),
        None => acl.map(<[&str]>::to_vec),
    }
}

/// The relation with this OID, from the built-in catalogs or from the catalog of the session. The system views and the relations that the user makes are in the catalog of the session.
fn class_of(relid: u32, session: &dyn Session) -> Option<Class<'_>> {
    if let Some(class) = builtin::class_by_oid(relid) {
        return Some(Class {
            oid: class.oid,
            name: class.name,
            namespace: class.namespace,
            kind: char::from(class.kind),
            natts: class.natts,
            owner: class.owner,
            acl: granted(ObjRef::new(PG_CLASS, relid), session)
                .or_else(|| class.acl.map(<[&str]>::to_vec)),
        });
    }
    let rel = session.catalog()?.relation(relid)?;
    Some(Class {
        oid: rel.oid,
        name: &rel.name,
        namespace: rel.namespace,
        kind: rel.kind.code(),
        natts: i16::try_from(rel.columns.len()).unwrap_or(i16::MAX),
        owner: rel.owner,
        acl: rel.acl.as_ref().map(|acl| acl.iter().map(String::as_str).collect()),
    })
}

/// A column of a relation as the checks of its privileges read it.
struct Attribute<'a> {
    name: &'a str,
    num: i16,
    dropped: bool,
    /// `attacl`, or `None` for a column with no privileges of its own.
    acl: Option<Vec<&'a str>>,
}

/// The system columns of a relation with storage, with their numbers.
const SYSTEM_COLUMNS: [(&str, i16); 6] =
    [("ctid", -1), ("xmin", -2), ("cmin", -3), ("xmax", -4), ("cmax", -5), ("tableoid", -6)];

/// The columns of a relation, with the system columns. A view or an index of the catalog of the session has no system columns.
fn attributes_of(relid: u32, session: &dyn Session) -> Vec<Attribute<'_>> {
    if builtin::class_by_oid(relid).is_some() {
        return builtin::attributes(relid)
            .iter()
            .map(|a| Attribute {
                name: a.name,
                num: a.num,
                dropped: a.dropped,
                acl: column_acl(relid, a.num, a.acl, session),
            })
            .collect();
    }
    let Some(rel) = session.catalog().and_then(|c| c.relation(relid)) else { return Vec::new() };
    let system: &[(&str, i16)] = match rel.kind {
        RelKind::Table
        | RelKind::Sequence
        | RelKind::Toast
        | RelKind::Partitioned
        | RelKind::MatView => &SYSTEM_COLUMNS,
        RelKind::Index | RelKind::PartitionedIndex | RelKind::View | RelKind::Composite => &[],
    };
    let system = system.iter().map(|&(name, num)| Attribute {
        name,
        num,
        dropped: false,
        acl: column_acl(relid, num, None, session),
    });
    let user = rel.columns.iter().zip(1..).map(|(column, num)| Attribute {
        name: &column.name,
        num,
        dropped: column.dropped,
        acl: column_acl(relid, num, None, session),
    });
    system.chain(user).collect()
}

/// `IsSystemClass`: a toast table or a relation with an OID below `FirstUnpinnedObjectId`.
fn is_system_class(class: &Class<'_>) -> bool {
    class.namespace == PG_TOAST || class.oid < FIRST_UNPINNED_OBJECT_ID
}

/// `pg_class_aclmask_ext` with `ACLMASK_ANY`. Only a superuser can change a system catalog. The roles `pg_read_all_data`, `pg_write_all_data` and `pg_maintain` add their privileges to the result.
fn class_check(relid: u32, role: u32, mask: u32, session: &dyn Session) -> Check {
    let Some(class) = class_of(relid, session) else { return Ok(None) };
    let mut mask = mask;
    let changes = ACL_INSERT | ACL_UPDATE | ACL_DELETE | ACL_TRUNCATE | ACL_USAGE;
    if mask & changes != 0 && is_system_class(&class) && class.kind != 'v' && !superuser(role) {
        mask &= !changes;
    }
    if superuser(role) {
        return Ok(Some(mask != 0));
    }
    let kind = if class.kind == 'S' { AclKind::Sequence } else { AclKind::Table };
    let acl = acl_of(class.acl.as_deref(), kind, class.owner)?;
    let mut result = aclmask(&acl, role, class.owner, mask);
    if mask & ACL_SELECT != 0
        && result & ACL_SELECT == 0
        && has_privs_of_role(role, PG_READ_ALL_DATA)
    {
        result |= ACL_SELECT;
    }
    let writes = ACL_INSERT | ACL_UPDATE | ACL_DELETE;
    if mask & writes != 0 && result & writes == 0 && has_privs_of_role(role, PG_WRITE_ALL_DATA) {
        result |= mask & writes;
    }
    if mask & ACL_MAINTAIN != 0
        && result & ACL_MAINTAIN == 0
        && has_privs_of_role(role, PG_MAINTAIN)
    {
        result |= ACL_MAINTAIN;
    }
    Ok(Some(result != 0))
}

/// `pg_attribute_aclmask_ext` with `ACLMASK_ANY`. A column with a null ACL has no privileges of its own, also for a superuser.
fn attribute_check(relid: u32, attnum: i16, role: u32, mask: u32, session: &dyn Session) -> Check {
    let attributes = attributes_of(relid, session);
    let found = attributes.iter().find(|a| a.num == attnum && !a.dropped);
    let (Some(attribute), Some(class)) = (found, class_of(relid, session)) else {
        return Ok(None);
    };
    let Some(acl) = &attribute.acl else { return Ok(Some(false)) };
    Ok(Some(aclmask(&parse_acl(acl)?, role, class.owner, mask) != 0))
}

/// `pg_attribute_aclcheck_all_ext` with `ACLMASK_ANY`: true when a user column that is not dropped has a privilege of `mask`.
fn all_attributes_check(relid: u32, role: u32, mask: u32, session: &dyn Session) -> Check {
    let Some(class) = class_of(relid, session) else { return Ok(None) };
    for attribute in attributes_of(relid, session) {
        if attribute.num < 1 || attribute.num > class.natts || attribute.dropped {
            continue;
        }
        let Some(acl) = &attribute.acl else { continue };
        if aclmask(&parse_acl(acl)?, role, class.owner, mask) != 0 {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// `column_privilege_check`: null for a column that does not exist or is dropped, else true when the role has a privilege on the column or on the table.
fn column_check(relid: u32, attnum: i16, role: u32, mask: u32, session: &dyn Session) -> Check {
    if attnum == 0 {
        return Ok(None);
    }
    match attribute_check(relid, attnum, role, mask, session)? {
        Some(true) => return Ok(Some(true)),
        None => return Ok(None),
        Some(false) => {}
    }
    class_check(relid, role, mask, session)
}

/// The privileges and the owner of a database: a static row, or a database that `CREATE DATABASE` made.
fn database_acl(oid: u32, session: &dyn Session) -> Result<Option<Privileges>> {
    if let Some(row) = builtin::owned_by_oid(Owned::Database, oid) {
        return Ok(Some((acl_of(row.acl, AclKind::Database, row.owner)?, row.owner)));
    }
    let created = session.databases();
    let Some(database) = created.iter().find(|d| d.oid == oid) else { return Ok(None) };
    let acl = match &database.acl {
        Some(items) => parse_acl(&items.iter().map(String::as_str).collect::<Vec<_>>())?,
        None => acl_default(AclKind::Database, database.owner),
    };
    Ok(Some((acl, database.owner)))
}

/// The kind of `acldefault` of an object kind.
fn owned_kind(kind: Owned) -> AclKind {
    match kind {
        Owned::Namespace => AclKind::Schema,
        Owned::Database => AclKind::Database,
        Owned::Function => AclKind::Function,
        Owned::Type => AclKind::Type,
        Owned::Language => AclKind::Language,
        Owned::Tablespace => AclKind::Tablespace,
        Owned::ForeignDataWrapper | Owned::ForeignServer => AclKind::Other,
        Owned::LargeObject => AclKind::LargeObject,
    }
}

/// The items of a parsed `aclitem[]` value with the owner of the object.
type Privileges = (Vec<(u32, u32)>, u32);

/// The OID of the catalog that holds the objects of a kind that `GRANT` can give an ACL to.
fn owned_class(kind: Owned) -> Option<u32> {
    match kind {
        Owned::Namespace => Some(PG_NAMESPACE),
        Owned::Type => Some(PG_TYPE),
        Owned::Function => Some(PG_PROC),
        Owned::Language => Some(PG_LANGUAGE),
        _ => None,
    }
}

/// The ACL that `GRANT` gave to an object of a kind, or `None` when the object keeps the ACL that it was made with.
fn granted_object(kind: Owned, oid: u32, session: &dyn Session) -> Option<Vec<&str>> {
    granted(ObjRef::new(owned_class(kind)?, oid), session)
}

/// The privileges and the owner of a schema, a type, a function or a language of the catalog of the session, or `None` when the catalog has no such object.
fn session_object(kind: Owned, oid: u32, session: &dyn Session) -> Result<Option<Privileges>> {
    let Some(catalog) = session.catalog() else { return Ok(None) };
    let (acl, owner) = match kind {
        Owned::Namespace => match catalog.schema(oid) {
            Some(schema) => (schema.acl.as_ref(), schema.owner),
            None => return Ok(None),
        },
        Owned::Type => match catalog.type_by_oid(oid) {
            Some(ty) => (None, ty.owner),
            None => return Ok(None),
        },
        Owned::Function => match catalog.function(oid) {
            Some(function) => (None, function.owner),
            None => return Ok(None),
        },
        Owned::Language => match catalog.languages().find(|l| l.oid == oid) {
            Some(language) => (None, language.owner),
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    let acl: Option<Vec<&str>> = granted_object(kind, oid, session)
        .or_else(|| acl.map(|acl| acl.iter().map(String::as_str).collect()));
    Ok(Some((acl_of(acl.as_deref(), owned_kind(kind), owner)?, owner)))
}

/// `object_aclmask_ext` with `ACLMASK_ANY`, with `pg_namespace_aclmask_ext` and `pg_type_aclmask_ext`. A superuser has each privilege, also on an object that does not exist. A true array type and a multirange type have the privileges of their element type and their range type. `pg_read_all_data` and `pg_write_all_data` have `USAGE` on each schema. An object that is not in the built-in catalogs comes from the catalog of the session.
fn object_check(kind: Owned, oid: u32, role: u32, mask: u32, session: &dyn Session) -> Check {
    if superuser(role) {
        return Ok(Some(mask != 0));
    }
    let mut oid = oid;
    if kind == Owned::Type {
        if let Some(ty) = builtin::type_by_oid(oid) {
            if ty.elem != 0 && ty.subscript == ARRAY_SUBSCRIPT_HANDLER {
                oid = ty.elem;
            }
            if builtin::type_by_oid(oid).is_some_and(|t| t.kind == b'm') {
                let Some(range) = builtin::multirange_range(oid) else { return Ok(None) };
                oid = range;
            }
        } else {
            let found = session.catalog().and_then(|c| c.type_by_oid(oid));
            let Some(ty) = found else { return Ok(None) };
            if ty.element != 0 {
                oid = ty.element;
            }
        }
    }
    let (acl, owner) = if kind == Owned::Database {
        let Some(found) = database_acl(oid, session)? else { return Ok(None) };
        found
    } else if let Some(row) = builtin::owned_by_oid(kind, oid) {
        let acl = granted_object(kind, oid, session).or_else(|| row.acl.map(<[&str]>::to_vec));
        (acl_of(acl.as_deref(), owned_kind(kind), row.owner)?, row.owner)
    } else {
        let Some(object) = session_object(kind, oid, session)? else { return Ok(None) };
        object
    };
    let mut result = aclmask(&acl, role, owner, mask);
    if kind == Owned::Namespace
        && mask & ACL_USAGE != 0
        && result & ACL_USAGE == 0
        && (has_privs_of_role(role, PG_READ_ALL_DATA) || has_privs_of_role(role, PG_WRITE_ALL_DATA))
    {
        result |= ACL_USAGE;
    }
    Ok(Some(result != 0))
}

/// `pg_role_aclcheck`: the admin option, then the membership, then the privileges, then `SET ROLE`.
fn role_check(role: u32, member: u32, mask: u32) -> bool {
    (mask & go(ACL_CREATE) != 0 && is_admin_of_role(member, role))
        || (mask & ACL_CREATE != 0 && reaches(member, role, Recurse::Members))
        || (mask & ACL_USAGE != 0 && reaches(member, role, Recurse::Privs))
        || (mask & ACL_SET != 0 && reaches(member, role, Recurse::SetRole))
}

/// `pg_parameter_aclmask` with `ACLMASK_ANY`. A superuser has each privilege on each name. The catalog `pg_parameter_acl` has no rows after `initdb`, so no other role has a privilege.
fn parameter_check(role: u32, mask: u32) -> bool {
    superuser(role) && mask != 0
}

/// The text value at `index`.
fn text(args: &[Value], index: usize) -> Result<&str> {
    args.get(index).and_then(Value::as_str).ok_or_else(bad_value)
}

/// The role of a call: the first value when the call has `with_role` values, else the current user. A role name may be `public`.
fn role_arg(call: &Call<'_>, args: &[Value], with_role: bool, public: bool) -> Result<u32> {
    if !with_role {
        return Ok(current_user(call.session));
    }
    match &args[0] {
        Value::Oid(oid) => Ok(*oid),
        Value::Text(name) if public => get_role_oid_or_public(name, call.session),
        Value::Text(name) => get_role_oid(name, call.session),
        _ => Err(bad_value()),
    }
}

/// `convert_table_name`: the relation of a qualified name, as `RangeVarGetRelid` finds it.
fn table_name(text: &str, session: &dyn Session) -> Result<u32> {
    let names = qualified_name_list(text).map_err(type_error)?;
    reg::relation_oid(&names, session)
}

/// The object of a function by name: the input of an OID alias type, with the error of `acl.c` for an OID of 0.
fn reg_name(kind: RegKind, text: &str, session: &dyn Session, what: &str) -> Result<u32> {
    match reg::input(kind, text, session)?? {
        0 => Err(Error::new(
            if kind == RegKind::Procedure {
                SqlState::UNDEFINED_FUNCTION
            } else {
                SqlState::UNDEFINED_OBJECT
            },
            format!("{what} \"{text}\" does not exist"),
        )),
        oid => Ok(oid),
    }
}

/// The OID of an object of a kind without a schema by its name, with the error of `get_*_oid`.
pub(crate) fn simple_name(kind: Owned, name: &str, session: &dyn Session) -> Result<u32> {
    let mut found = builtin::owned(kind).iter().find(|r| r.name == name).map(|r| r.oid);
    if found.is_none() && kind == Owned::Namespace {
        found = session.catalog().and_then(|c| c.schema_by_name(name)).map(|s| s.oid);
    }
    if found.is_none() && kind == Owned::Database {
        found = session.databases().iter().find(|d| d.name == name).map(|d| d.oid);
    }
    found.ok_or_else(|| {
        let (state, what) = match kind {
            Owned::Namespace => (SqlState::UNDEFINED_SCHEMA, "schema"),
            Owned::Database => (SqlState::UNDEFINED_DATABASE, "database"),
            Owned::Language => (SqlState::UNDEFINED_OBJECT, "language"),
            Owned::Tablespace => (SqlState::UNDEFINED_OBJECT, "tablespace"),
            Owned::ForeignDataWrapper => (SqlState::UNDEFINED_OBJECT, "foreign-data wrapper"),
            _ => (SqlState::UNDEFINED_OBJECT, "server"),
        };
        Error::new(state, format!("{what} \"{name}\" does not exist"))
    })
}

/// The privilege map of an object kind.
fn owned_privs(kind: Owned) -> PrivMap {
    match kind {
        Owned::Namespace => SCHEMA_PRIVS,
        Owned::Database => DATABASE_PRIVS,
        Owned::Function => FUNCTION_PRIVS,
        Owned::Tablespace => TABLESPACE_PRIVS,
        Owned::LargeObject => LARGEOBJECT_PRIVS,
        Owned::Type | Owned::Language | Owned::ForeignDataWrapper | Owned::ForeignServer => {
            USAGE_PRIVS
        }
    }
}

/// The result of a check as a value.
fn result(check: Option<bool>) -> Value {
    check.map_or(Value::Null, Value::Bool)
}

/// `has_table_privilege`. The forms with a name take the role, then the relation, then the privileges. The forms with an OID take the privileges first.
fn has_table_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let relid = match &args[at] {
        Value::Text(name) => table_name(name, call.session)?,
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let mask = privileges(text(args, at + 1)?, TABLE_PRIVS)?;
    Ok(result(class_check(relid, role, mask, call.session)?))
}

/// `LockTableAclCheck`: true when `role` can lock the relation `relid` in the lock mode `mode`, where `AccessShareLock` is 1. `MAINTAIN`, `UPDATE`, `DELETE` and `TRUNCATE` allow each mode, `SELECT` allows `ACCESS SHARE`, and `INSERT` allows the modes up to `ROW EXCLUSIVE`.
///
/// # Errors
///
/// The error of an ACL that is not correct.
pub fn lock_table_allowed(relid: u32, role: u32, mode: i32, session: &dyn Session) -> Result<bool> {
    let mut mask = ACL_MAINTAIN | ACL_UPDATE | ACL_DELETE | ACL_TRUNCATE;
    if mode <= 1 {
        mask |= ACL_SELECT;
    }
    if mode <= 3 {
        mask |= ACL_INSERT;
    }
    Ok(class_check(relid, role, mask, session)?.unwrap_or(false))
}

/// `ExecCheckOneRelPerms` for `INSERT` into the columns `attnums`: true when the role has `INSERT` on the table, or on each of the columns. With no columns, `INSERT` on any column is enough.
///
/// # Errors
///
/// The error of an ACL that is not correct.
pub fn insert_allowed(
    relid: u32,
    role: u32,
    attnums: &[i16],
    session: &dyn Session,
) -> Result<bool> {
    if class_check(relid, role, ACL_INSERT, session)?.unwrap_or(false) {
        return Ok(true);
    }
    if attnums.is_empty() {
        return Ok(all_attributes_check(relid, role, ACL_INSERT, session)?.unwrap_or(false));
    }
    for &attnum in attnums {
        if !attribute_check(relid, attnum, role, ACL_INSERT, session)?.unwrap_or(false) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The privileges that `nextval` needs: `USAGE` or `UPDATE`.
pub(crate) const USAGE_UPDATE: u32 = ACL_USAGE | ACL_UPDATE;
/// The privileges that `currval` and `lastval` need: `SELECT` or `USAGE`.
pub(crate) const SELECT_USAGE: u32 = ACL_SELECT | ACL_USAGE;
/// The privilege that `setval` needs.
pub(crate) const UPDATE: u32 = ACL_UPDATE;

/// `pg_class_aclcheck` for `SELECT` or `USAGE` on a sequence as the current user, as `pg_get_sequence_data` does it.
pub(crate) fn sequence_readable(relid: u32, session: &dyn Session) -> Result<bool> {
    sequence_allowed(relid, SELECT_USAGE, session)
}

/// `pg_class_aclcheck` on a sequence as the current user: true when the user has one of the privileges of `mask`.
pub(crate) fn sequence_allowed(relid: u32, mask: u32, session: &dyn Session) -> Result<bool> {
    Ok(class_check(relid, current_user(session), mask, session)?.unwrap_or(false))
}

/// `has_sequence_privilege`: the privileges come before the relation. A relation that is not a sequence is an error.
fn has_sequence_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let mask = privileges(text(args, at + 1)?, SEQUENCE_PRIVS)?;
    let (relid, shown) = match &args[at] {
        Value::Text(name) => (table_name(name, call.session)?, name.clone()),
        Value::Oid(oid) => match class_of(*oid, call.session) {
            Some(class) => (*oid, class.name.to_string()),
            None => return Ok(Value::Null),
        },
        _ => return Err(bad_value()),
    };
    if class_of(relid, call.session).is_none_or(|c| c.kind != 'S') {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{shown}\" is not a sequence"),
        ));
    }
    Ok(result(class_check(relid, role, mask, call.session)?))
}

/// `convert_column_name`: the number of a column by name. A dropped column gives 0, and so does a relation that does not exist.
fn column_number(relid: u32, name: &str, session: &dyn Session) -> Result<i16> {
    if let Some(attribute) = attributes_of(relid, session).iter().find(|a| a.name == name) {
        return Ok(if attribute.dropped { 0 } else { attribute.num });
    }
    match class_of(relid, session) {
        Some(class) => Err(Error::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{name}\" of relation \"{}\" does not exist", class.name),
        )),
        None => Ok(0),
    }
}

/// `get_attnum`: the number of a column of a relation by name, with the system columns. A dropped column has no number.
pub(crate) fn attnum(relid: u32, name: &str, session: &dyn Session) -> Option<i16> {
    attributes_of(relid, session).iter().find(|a| a.name == name && !a.dropped).map(|a| a.num)
}

/// `has_column_privilege`: the role, the relation, the column, then the privileges.
fn has_column_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 4;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let relid = match &args[at] {
        Value::Text(name) => table_name(name, call.session)?,
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let attnum = match &args[at + 1] {
        Value::Text(name) => column_number(relid, name, call.session)?,
        value => value.as_i64().and_then(|n| i16::try_from(n).ok()).ok_or_else(bad_value)?,
    };
    let mask = privileges(text(args, at + 2)?, COLUMN_PRIVS)?;
    Ok(result(column_check(relid, attnum, role, mask, call.session)?))
}

/// `has_any_column_privilege`: the privilege on the relation, or on any column of it.
fn has_any_column_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let relid = match &args[at] {
        Value::Text(name) => table_name(name, call.session)?,
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let mask = privileges(text(args, at + 1)?, COLUMN_PRIVS)?;
    Ok(result(match class_check(relid, role, mask, call.session)? {
        Some(false) => all_attributes_check(relid, role, mask, call.session)?,
        other => other,
    }))
}

/// The `has_*_privilege` functions of the objects that `object_aclcheck` checks. The name of a function or a type is read as `regprocedure` or `regtype` reads it.
fn has_object_privilege(kind: Owned, call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let oid = match &args[at] {
        Value::Text(name) => match kind {
            Owned::Function => reg_name(RegKind::Procedure, name, call.session, "function")?,
            Owned::Type => reg_name(RegKind::Type, name, call.session, "type")?,
            _ => simple_name(kind, name, call.session)?,
        },
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let mask = privileges(text(args, at + 1)?, owned_privs(kind))?;
    Ok(result(object_check(kind, oid, role, mask, call.session)?))
}

/// `has_largeobject_privilege`: null for a large object that does not exist. The static rows have no large objects.
fn has_largeobject_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let oid = args[at].as_oid().ok_or_else(bad_value)?;
    let mask = privileges(text(args, at + 1)?, LARGEOBJECT_PRIVS)?;
    if builtin::owned_by_oid(Owned::LargeObject, oid).is_none() {
        return Ok(Value::Null);
    }
    Ok(result(object_check(Owned::LargeObject, oid, role, mask, call.session)?))
}

/// `has_parameter_privilege`: the privileges come before the role.
fn has_parameter_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let at = usize::from(with_role);
    let mask = privileges(text(args, at + 1)?, PARAMETER_PRIVS)?;
    let role = role_arg(call, args, with_role, true)?;
    text(args, at)?;
    Ok(Value::Bool(parameter_check(role, mask)))
}

/// `pg_has_role`: the member, then the role, then the privileges. A role name may not be `public`.
fn pg_has_role(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_member = args.len() == 3;
    let member = role_arg(call, args, with_member, false)?;
    let at = usize::from(with_member);
    let role = match &args[at] {
        Value::Text(name) => get_role_oid(name, call.session)?,
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let mask = privileges(text(args, at + 1)?, ROLE_PRIVS)?;
    Ok(Value::Bool(role_check(role, member, mask)))
}

/// The kernel of a function of this module by its `prosrc`, such as `has_table_privilege_name_id`. One kernel serves each form of a function.
/// `convert_aclright_to_string`: the name of each privilege bit, from bit 0.
const RIGHT_NAMES: [&str; 15] = [
    "INSERT",
    "SELECT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "EXECUTE",
    "USAGE",
    "CREATE",
    "TEMPORARY",
    "CONNECT",
    "SET",
    "ALTER SYSTEM",
    "MAINTAIN",
];

/// `aclexplode`: a row for each privilege of each item of an `aclitem[]` value, with the grantor, the grantee, the name of the privilege and its grant option. The items come in order, and the privileges of an item come in the order of their bits.
///
/// # Errors
///
/// `42704` for an item with a role that does not exist.
pub(crate) fn aclexplode(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Array(acl)) = args.first() else { return Err(bad_value()) };
    let mut rows = Vec::new();
    for item in acl.values.iter().flatten() {
        let item = stored_item(item, call.session)?;
        for (bit, name) in RIGHT_NAMES.iter().enumerate() {
            if item.bits & (1 << bit) != 0 {
                rows.push(vec![
                    Value::Oid(item.grantor),
                    Value::Oid(item.grantee),
                    Value::text(*name),
                    Value::Bool(item.bits & go(1 << bit) != 0),
                ]);
            }
        }
    }
    Ok(rows)
}

/// An `aclitem` value: the grantee, the privileges with their grant options in the bits of [`go`], and the grantor.
#[derive(Debug, Clone, Copy)]
struct AclItem {
    grantee: u32,
    bits: u32,
    grantor: u32,
}

/// `NAMEDATALEN`: a role name has fewer bytes than this.
const NAMEDATALEN: usize = 64;

/// `is_safe_acl_char` of `getid`: a byte of a role name with no quotes. A byte with the high bit set counts.
fn is_name_byte(c: u8) -> bool {
    c >= 0x80 || c.is_ascii_alphanumeric() || c == b'_'
}

/// The text after the white space at its start.
fn skip_spaces(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| is_space(char::from(c))).count();
    &s[n..]
}

/// `getid`: a role name after any white space, with double quotes around all or part of it or with no quotes. In quotes, `""` is a quote. It gives the name and the text after the name and any white space.
///
/// # Errors
///
/// `42622` for a name of `NAMEDATALEN` bytes or more.
fn getid(s: &[u8]) -> Result<(String, &[u8])> {
    let mut s = skip_spaces(s);
    let mut name = Vec::new();
    let mut in_quotes = false;
    while let Some((&c, rest)) = s.split_first() {
        if !(in_quotes || c == b'"' || is_name_byte(c)) {
            break;
        }
        s = rest;
        if c == b'"' {
            if !in_quotes {
                in_quotes = true;
                continue;
            }
            if s.first() != Some(&b'"') {
                in_quotes = false;
                continue;
            }
            s = &s[1..];
        }
        if name.len() >= NAMEDATALEN - 1 {
            return Err(Error::new(SqlState::NAME_TOO_LONG, "identifier too long")
                .with_detail(format!("Identifier must be less than {NAMEDATALEN} characters.")));
        }
        name.push(c);
    }
    Ok((String::from_utf8_lossy(&name).into_owned(), skip_spaces(s)))
}

/// The error `22P02` of a bad `aclitem` text.
fn acl_syntax(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_TEXT_REPRESENTATION, message)
}

/// The OID of a role of an `aclitem` text. In the text of a stored value, a role that does not exist shows as its OID, as `aclitemout` writes it.
///
/// # Errors
///
/// `42704` for a role that does not exist.
fn acl_role(name: &str, session: &dyn Session, stored: bool) -> Result<u32> {
    if stored
        && role_oid(name, session).is_none()
        && let Ok(oid) = name.parse::<u32>()
    {
        return Ok(oid);
    }
    get_role_oid(name, session)
}

/// `aclparse`: an item in the form `grantee=privileges/grantor`, and the text after it. The key word `group` or `user` can come before the grantee, and an empty grantee is PUBLIC. A `*` after a privilege is its grant option. An item with no grantor has the bootstrap superuser as its grantor, with a warning. `stored` reads the text of a stored value.
///
/// # Errors
///
/// `22P02` for a bad item, `42622` for a name that is too long, and `42704` for a role that does not exist.
fn aclparse<'a>(s: &'a [u8], session: &dyn Session, stored: bool) -> Result<(AclItem, &'a [u8])> {
    let (mut name, mut s) = getid(s)?;
    if s.first() != Some(&b'=') {
        if name != "group" && name != "user" {
            return Err(acl_syntax(format!("unrecognized key word: \"{name}\""))
                .with_hint("ACL key word must be \"group\" or \"user\"."));
        }
        (name, s) = getid(s)?;
        if name.is_empty() {
            return Err(acl_syntax("missing name")
                .with_hint("A name must follow the \"group\" or \"user\" key word."));
        }
    }
    let Some(mut s) = s.strip_prefix(b"=") else {
        return Err(acl_syntax("missing \"=\" sign"));
    };
    let mut bits = 0;
    let mut read = 0;
    while let Some((&c, rest)) = s.split_first() {
        if c == b'*' {
            bits |= go(read);
        } else if c.is_ascii_alphabetic() {
            let Some(bit) = RIGHTS.find(char::from(c)) else {
                return Err(acl_syntax(format!(
                    "invalid mode character: must be one of \"{RIGHTS}\""
                )));
            };
            read = 1 << bit;
            bits |= read;
        } else {
            break;
        }
        s = rest;
    }
    let grantee = if name.is_empty() { PUBLIC } else { acl_role(&name, session, stored)? };
    let grantor = if let Some(rest) = s.strip_prefix(b"/") {
        let (name, rest) = getid(rest)?;
        s = rest;
        if name.is_empty() {
            return Err(acl_syntax("a name must follow the \"/\" sign"));
        }
        acl_role(&name, session, stored)?
    } else {
        session.warn(Error::new(
            SqlState::INVALID_GRANTOR,
            format!("defaulting grantor to user ID {BOOTSTRAP_SUPERUSER}"),
        ));
        BOOTSTRAP_SUPERUSER
    };
    Ok((AclItem { grantee, bits, grantor }, s))
}

/// `aclitemin`: an `aclitem` value as the text of `aclitemout`.
///
/// # Errors
///
/// The errors of [`aclparse`], and `22P02` for text after the item.
pub(crate) fn aclitem_in(text: &str, session: &dyn Session) -> Result<String> {
    let (item, rest) = aclparse(text.as_bytes(), session, false)?;
    if !skip_spaces(rest).is_empty() {
        return Err(acl_syntax("extra garbage at the end of the ACL specification"));
    }
    Ok(acl_item(item.grantee, item.bits, item.grantor))
}

/// The parts of an `aclitem` value, which has the text of `aclitemout`.
fn stored_item(value: &Value, session: &dyn Session) -> Result<AclItem> {
    let text = value.as_str().ok_or_else(bad_value)?;
    Ok(aclparse(text.as_bytes(), session, true)?.0)
}

/// `check_acl`: the items of an `aclitem[]` value, which must have one dimension and no null items.
///
/// # Errors
///
/// `22023` for an array that does not have one dimension, and `22004` for a null item.
fn acl_items(value: &Value, session: &dyn Session) -> Result<Vec<AclItem>> {
    let Value::Array(acl) = value else { return Err(bad_value()) };
    if acl.dims.len() != 1 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "ACL arrays must be one-dimensional",
        ));
    }
    if acl.values.iter().any(Option::is_none) {
        return Err(Error::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "ACL arrays must not contain null values",
        ));
    }
    acl.values.iter().flatten().map(|item| stored_item(item, session)).collect()
}

/// `aclitem_eq`: the grantees, the privileges and the grantors are equal. The text of a value has one form, so the texts are equal.
fn aclitem_eq(_call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(text(args, 0)? == text(args, 1)?))
}

/// `aclcontains`: true when the array has an item with the grantee and the grantor of the item, and with all its privileges and grant options.
///
/// # Errors
///
/// The errors of [`acl_items`].
fn aclcontains(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [acl, item] = args else { return Err(bad_value()) };
    let items = acl_items(acl, call.session)?;
    let item = stored_item(item, call.session)?;
    Ok(Value::Bool(items.iter().any(|a| {
        a.grantee == item.grantee && a.grantor == item.grantor && (a.bits & item.bits) == item.bits
    })))
}

/// The privileges of `makeaclitem`: the privileges of all kinds of objects.
const ANY_PRIVS: PrivMap = &[
    ("SELECT", ACL_SELECT),
    ("INSERT", ACL_INSERT),
    ("UPDATE", ACL_UPDATE),
    ("DELETE", ACL_DELETE),
    ("TRUNCATE", ACL_TRUNCATE),
    ("REFERENCES", ACL_REFERENCES),
    ("TRIGGER", ACL_TRIGGER),
    ("EXECUTE", ACL_EXECUTE),
    ("USAGE", ACL_USAGE),
    ("CREATE", ACL_CREATE),
    ("TEMP", ACL_CREATE_TEMP),
    ("TEMPORARY", ACL_CREATE_TEMP),
    ("CONNECT", ACL_CONNECT),
    ("SET", ACL_SET),
    ("ALTER SYSTEM", ACL_ALTER_SYSTEM),
    ("MAINTAIN", ACL_MAINTAIN),
];

/// `makeaclitem(grantee, grantor, privileges, is_grantable)`: an item with the privileges of a list of names, with their grant options when `is_grantable` is true.
///
/// # Errors
///
/// `22023` for a name that is not the name of a privilege.
fn makeaclitem(_call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Oid(grantee)), Some(Value::Oid(grantor)), Some(Value::Bool(grantable))) =
        (args.first(), args.get(1), args.get(3))
    else {
        return Err(bad_value());
    };
    let privs = privileges(text(args, 2)?, ANY_PRIVS)?;
    let bits = if *grantable { privs | go(privs) } else { privs };
    Ok(Value::text(acl_item(*grantee, bits, *grantor)))
}

/// The hash of `hash_aclitem`: the sum of the privileges with no grant options, the grantee and the grantor, in 32 bits.
fn aclitem_sum(item: AclItem) -> u32 {
    (item.bits & 0xFFFF).wrapping_add(item.grantee).wrapping_add(item.grantor)
}

/// `hash_aclitem`.
fn hash_aclitem(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [item] = args else { return Err(bad_value()) };
    Ok(Value::Int4(aclitem_sum(stored_item(item, call.session)?).cast_signed()))
}

/// `hash_aclitem_extended`: the sum of `hash_aclitem` for the seed 0. The engine does not have the hash of another seed yet.
fn hash_aclitem_extended(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [item, Value::Int8(seed)] = args else { return Err(bad_value()) };
    if *seed != 0 {
        return Err(not_yet("hash_aclitem_extended with a seed that is not 0"));
    }
    Ok(Value::Int8(i64::from(aclitem_sum(stored_item(item, call.session)?))))
}

/// `aclinsert`, which PostgreSQL no longer has.
fn aclinsert(_call: &Call<'_>, _args: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "aclinsert is no longer supported"))
}

/// `aclremove`, which PostgreSQL no longer has.
fn aclremove(_call: &Call<'_>, _args: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "aclremove is no longer supported"))
}

/// `putid`: the name of a role in an `aclitem` value, in double quotes when it has a character that is not an ASCII letter, an ASCII digit or `_`. A role that does not exist shows as its OID.
fn put_role(out: &mut String, role: u32) {
    let roles = rupg_pgcatalog::roles::current();
    let Some(name) = roles.name(role) else {
        out.push_str(&role.to_string());
        return;
    };
    if name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        out.push_str(name);
        return;
    }
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

/// `aclitemout`: the text of an item with no grant options, as `grantee=privileges/grantor`. The grantee of PUBLIC is empty.
fn acl_item(grantee: u32, bits: u32, grantor: u32) -> String {
    let mut out = String::new();
    if grantee != PUBLIC {
        put_role(&mut out, grantee);
    }
    out.push('=');
    for (bit, letter) in RIGHTS.chars().enumerate() {
        if bits & (1 << bit) != 0 {
            out.push(letter);
            if bits & go(1 << bit) != 0 {
                out.push('*');
            }
        }
    }
    out.push('/');
    put_role(&mut out, grantor);
    out
}

/// `acldefault(char, oid)`: the privileges of an object of a kind with a null ACL and this owner, as an `aclitem[]` value.
///
/// # Errors
///
/// `XX000` for a letter that is not the letter of a kind.
fn acldefault(_call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Char(kind)), Some(Value::Oid(owner))) = (args.first(), args.get(1)) else {
        return Err(bad_value());
    };
    let kind = match kind {
        b'c' => AclKind::Column,
        b'r' => AclKind::Table,
        b's' => AclKind::Sequence,
        b'd' => AclKind::Database,
        b'f' => AclKind::Function,
        b'l' => AclKind::Language,
        b'L' => AclKind::LargeObject,
        b'n' => AclKind::Schema,
        b'p' => AclKind::Parameter,
        b't' => AclKind::Tablespace,
        b'F' | b'S' => AclKind::Other,
        b'T' => AclKind::Type,
        other => {
            return Err(Error::internal(format!(
                "unrecognized object type abbreviation: {}",
                char::from(*other)
            )));
        }
    };
    let items = acl_default(kind, *owner)
        .into_iter()
        .map(|(grantee, bits)| Some(Value::text(acl_item(grantee, bits, *owner))))
        .collect();
    Ok(Value::Array(Box::new(rupg_types::Array::one(items))))
}

/// `row_security_active(oid)` and `row_security_active(text)`: true when row security applies to the relation for the current user. No relation of rupg has row security, so the result is false. A name of a relation that does not exist is an error.
fn row_security_active(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    if let Some(Value::Text(name)) = args.first() {
        table_name(name, call.session)?;
    }
    Ok(Value::Bool(false))
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    if matches!(src, "row_security_active" | "row_security_active_name") {
        return Some(row_security_active);
    }
    let kernel: Kernel = match src {
        "acldefault_sql" => acldefault,
        "aclitem_eq" => aclitem_eq,
        "aclcontains" => aclcontains,
        "makeaclitem" => makeaclitem,
        "hash_aclitem" => hash_aclitem,
        "hash_aclitem_extended" => hash_aclitem_extended,
        "aclinsert" => aclinsert,
        "aclremove" => aclremove,
        _ => return privilege_kernel(src),
    };
    Some(kernel)
}

/// The kernel of a `has_*_privilege` function or of `pg_has_role`.
fn privilege_kernel(src: &str) -> Option<Kernel> {
    let family = if src.starts_with("pg_has_role") {
        "pg_has_role"
    } else {
        let end = src.find("_privilege")? + "_privilege".len();
        &src[..end]
    };
    let kernel: Kernel = match family {
        "has_table_privilege" => has_table_privilege,
        "has_sequence_privilege" => has_sequence_privilege,
        "has_column_privilege" => has_column_privilege,
        "has_any_column_privilege" => has_any_column_privilege,
        "has_database_privilege" => |c, a| has_object_privilege(Owned::Database, c, a),
        "has_foreign_data_wrapper_privilege" => {
            |c, a| has_object_privilege(Owned::ForeignDataWrapper, c, a)
        }
        "has_function_privilege" => |c, a| has_object_privilege(Owned::Function, c, a),
        "has_language_privilege" => |c, a| has_object_privilege(Owned::Language, c, a),
        "has_schema_privilege" => |c, a| has_object_privilege(Owned::Namespace, c, a),
        "has_server_privilege" => |c, a| has_object_privilege(Owned::ForeignServer, c, a),
        "has_tablespace_privilege" => |c, a| has_object_privilege(Owned::Tablespace, c, a),
        "has_type_privilege" => |c, a| has_object_privilege(Owned::Type, c, a),
        "has_largeobject_privilege" => has_largeobject_privilege,
        "has_parameter_privilege" => has_parameter_privilege,
        "pg_has_role" => pg_has_role,
        _ => return None,
    };
    Some(kernel)
}

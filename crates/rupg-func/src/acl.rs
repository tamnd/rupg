//! The access privilege inquiry functions: `has_table_privilege` and the other `has_*_privilege` functions, and `pg_has_role`.
//!
//! A port of the functions of `src/backend/utils/adt/acl.c` and of the checks of `aclchk.c` that they call. The privileges are the `aclitem[]` values of the static rows, which have the privileges that `initdb` gives. Each function has forms with a role name or OID first, or no role for the current user, and with the object as text or as an OID. The form of a call is clear from its values: a name is text and an OID is an OID. A form with the OID of an object that does not exist gives null.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ClassRow, Owned};
use rupg_types::{RegKind, Value, qualified_name_list};

use crate::reg;
use crate::{Call, Kernel, Session, bad_value, type_error};

const ACL_INSERT: u32 = 1 << 0;
const ACL_SELECT: u32 = 1 << 1;
const ACL_UPDATE: u32 = 1 << 2;
const ACL_DELETE: u32 = 1 << 3;
const ACL_TRUNCATE: u32 = 1 << 4;
const ACL_REFERENCES: u32 = 1 << 5;
const ACL_TRIGGER: u32 = 1 << 6;
const ACL_EXECUTE: u32 = 1 << 7;
const ACL_USAGE: u32 = 1 << 8;
const ACL_CREATE: u32 = 1 << 9;
const ACL_CREATE_TEMP: u32 = 1 << 10;
const ACL_CONNECT: u32 = 1 << 11;
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
    builtin::roles().iter().find(|r| r.name == name).map(|r| r.oid).or_else(|| {
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
    builtin::roles().iter().any(|r| r.oid == role && r.superuser)
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
    let mut roles = vec![role];
    let mut admin = false;
    let mut at = 0;
    while let Some(&member) = roles.get(at) {
        at += 1;
        for edge in builtin::members().iter().filter(|m| m.member == member) {
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
    let user = current_user(session);
    role_oid("pg_read_all_stats", session).is_some_and(|stats| has_privs_of_role(user, stats))
        || has_privs_of_role(user, role)
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
    };
    let all = all.chars().filter_map(|c| RIGHTS.find(c)).fold(0, |bits, at| bits | 1 << at);
    let mut items = Vec::new();
    if world != 0 {
        items.push((PUBLIC, world));
    }
    items.push((owner, all));
    items
}

/// Reads an `aclitem[]` value of the static rows as (grantee, bits).
fn parse_acl(acl: &[&str]) -> Result<Vec<(u32, u32)>> {
    let bad = || Error::internal("a static aclitem that the engine cannot read");
    let mut items = Vec::with_capacity(acl.len());
    for item in acl {
        let (grantee, rest) = item.split_once('=').ok_or_else(bad)?;
        let (letters, _grantor) = rest.split_once('/').ok_or_else(bad)?;
        let grantee = if grantee.is_empty() {
            PUBLIC
        } else {
            builtin::roles().iter().find(|r| r.name == grantee).ok_or_else(bad)?.oid
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

/// `IsSystemClass`: a toast table or a relation with an OID below `FirstUnpinnedObjectId`.
fn is_system_class(class: &ClassRow) -> bool {
    class.namespace == PG_TOAST || class.oid < FIRST_UNPINNED_OBJECT_ID
}

/// `pg_class_aclmask_ext` with `ACLMASK_ANY`. Only a superuser can change a system catalog. The roles `pg_read_all_data`, `pg_write_all_data` and `pg_maintain` add their privileges to the result.
fn class_check(relid: u32, role: u32, mask: u32) -> Check {
    let Some(class) = builtin::class_by_oid(relid) else { return Ok(None) };
    let mut mask = mask;
    let changes = ACL_INSERT | ACL_UPDATE | ACL_DELETE | ACL_TRUNCATE | ACL_USAGE;
    if mask & changes != 0 && is_system_class(class) && class.kind != b'v' && !superuser(role) {
        mask &= !changes;
    }
    if superuser(role) {
        return Ok(Some(mask != 0));
    }
    let kind = if class.kind == b'S' { AclKind::Sequence } else { AclKind::Table };
    let acl = acl_of(class.acl, kind, class.owner)?;
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
fn attribute_check(relid: u32, attnum: i16, role: u32, mask: u32) -> Check {
    let found = builtin::attributes(relid).iter().find(|a| a.num == attnum && !a.dropped);
    let (Some(attribute), Some(class)) = (found, builtin::class_by_oid(relid)) else {
        return Ok(None);
    };
    let Some(acl) = attribute.acl else { return Ok(Some(false)) };
    Ok(Some(aclmask(&parse_acl(acl)?, role, class.owner, mask) != 0))
}

/// `pg_attribute_aclcheck_all_ext` with `ACLMASK_ANY`: true when a user column that is not dropped has a privilege of `mask`.
fn all_attributes_check(relid: u32, role: u32, mask: u32) -> Check {
    let Some(class) = builtin::class_by_oid(relid) else { return Ok(None) };
    for attribute in builtin::attributes(relid) {
        if attribute.num < 1 || attribute.num > class.natts || attribute.dropped {
            continue;
        }
        let Some(acl) = attribute.acl else { continue };
        if aclmask(&parse_acl(acl)?, role, class.owner, mask) != 0 {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// `column_privilege_check`: null for a column that does not exist or is dropped, else true when the role has a privilege on the column or on the table.
fn column_check(relid: u32, attnum: i16, role: u32, mask: u32) -> Check {
    if attnum == 0 {
        return Ok(None);
    }
    match attribute_check(relid, attnum, role, mask)? {
        Some(true) => return Ok(Some(true)),
        None => return Ok(None),
        Some(false) => {}
    }
    class_check(relid, role, mask)
}

/// The privileges of a database.
fn database_acl(oid: u32) -> Result<Option<Vec<(u32, u32)>>> {
    let Some(row) = builtin::owned_by_oid(Owned::Database, oid) else { return Ok(None) };
    acl_of(row.acl, AclKind::Database, row.owner).map(Some)
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
        Owned::LargeObject => AclKind::Sequence,
    }
}

/// `object_aclmask_ext` with `ACLMASK_ANY`, with `pg_namespace_aclmask_ext` and `pg_type_aclmask_ext`. A superuser has each privilege, also on an object that does not exist. A true array type and a multirange type have the privileges of their element type and their range type. `pg_read_all_data` and `pg_write_all_data` have `USAGE` on each schema.
fn object_check(kind: Owned, oid: u32, role: u32, mask: u32) -> Check {
    if superuser(role) {
        return Ok(Some(mask != 0));
    }
    let mut oid = oid;
    if kind == Owned::Type {
        let Some(ty) = builtin::type_by_oid(oid) else { return Ok(None) };
        if ty.elem != 0 && ty.subscript == ARRAY_SUBSCRIPT_HANDLER {
            oid = ty.elem;
        }
        if builtin::type_by_oid(oid).is_some_and(|t| t.kind == b'm') {
            let Some(range) = builtin::multirange_range(oid) else { return Ok(None) };
            oid = range;
        }
    }
    let (acl, owner) = if kind == Owned::Database {
        let Some(acl) = database_acl(oid)? else { return Ok(None) };
        (acl, BOOTSTRAP_SUPERUSER)
    } else {
        let Some(row) = builtin::owned_by_oid(kind, oid) else { return Ok(None) };
        (acl_of(row.acl, owned_kind(kind), row.owner)?, row.owner)
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
fn simple_name(kind: Owned, name: &str) -> Result<u32> {
    let found = builtin::owned(kind).iter().find(|r| r.name == name).map(|r| r.oid);
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
    Ok(result(class_check(relid, role, mask)?))
}

/// `has_sequence_privilege`: the privileges come before the relation. A relation that is not a sequence is an error.
fn has_sequence_privilege(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let with_role = args.len() == 3;
    let role = role_arg(call, args, with_role, true)?;
    let at = usize::from(with_role);
    let mask = privileges(text(args, at + 1)?, SEQUENCE_PRIVS)?;
    let (relid, shown) = match &args[at] {
        Value::Text(name) => (table_name(name, call.session)?, name.clone()),
        Value::Oid(oid) => match builtin::class_by_oid(*oid) {
            Some(class) => (*oid, class.name.to_string()),
            None => return Ok(Value::Null),
        },
        _ => return Err(bad_value()),
    };
    if builtin::class_by_oid(relid).is_none_or(|c| c.kind != b'S') {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{shown}\" is not a sequence"),
        ));
    }
    Ok(result(class_check(relid, role, mask)?))
}

/// `convert_column_name`: the number of a column by name. A dropped column gives 0, and so does a relation that does not exist.
fn column_number(relid: u32, name: &str) -> Result<i16> {
    if let Some(attribute) = builtin::attributes(relid).iter().find(|a| a.name == name) {
        return Ok(if attribute.dropped { 0 } else { attribute.num });
    }
    match builtin::class_by_oid(relid) {
        Some(class) => Err(Error::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{name}\" of relation \"{}\" does not exist", class.name),
        )),
        None => Ok(0),
    }
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
        Value::Text(name) => column_number(relid, name)?,
        value => value.as_i64().and_then(|n| i16::try_from(n).ok()).ok_or_else(bad_value)?,
    };
    let mask = privileges(text(args, at + 2)?, COLUMN_PRIVS)?;
    Ok(result(column_check(relid, attnum, role, mask)?))
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
    Ok(result(match class_check(relid, role, mask)? {
        Some(false) => all_attributes_check(relid, role, mask)?,
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
            _ => simple_name(kind, name)?,
        },
        Value::Oid(oid) => *oid,
        _ => return Err(bad_value()),
    };
    let mask = privileges(text(args, at + 1)?, owned_privs(kind))?;
    Ok(result(object_check(kind, oid, role, mask)?))
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
    Ok(result(object_check(Owned::LargeObject, oid, role, mask)?))
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
        let item = item.as_str().ok_or_else(bad_value)?;
        let bad = || Error::internal(format!("an aclitem that the engine cannot read: {item}"));
        let (grantee, rest) = item.split_once('=').ok_or_else(bad)?;
        let (letters, grantor) = rest.split_once('/').ok_or_else(bad)?;
        let grantee =
            if grantee.is_empty() { PUBLIC } else { get_role_oid(grantee, call.session)? };
        let grantor = get_role_oid(grantor, call.session)?;
        let mut bits: u32 = 0;
        let mut last = 0;
        for c in letters.chars() {
            if c == '*' {
                bits |= last << 16;
            } else {
                last = 1 << RIGHTS.find(c).ok_or_else(bad)?;
                bits |= last;
            }
        }
        for (bit, name) in RIGHT_NAMES.iter().enumerate() {
            if bits & (1 << bit) != 0 {
                rows.push(vec![
                    Value::Oid(grantor),
                    Value::Oid(grantee),
                    Value::text(*name),
                    Value::Bool(bits & go(1 << bit) != 0),
                ]);
            }
        }
    }
    Ok(rows)
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

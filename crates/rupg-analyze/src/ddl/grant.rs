//! `ExecuteGrantStmt`: `GRANT` and `REVOKE` on the tables, the views, the sequences, the columns, the schemas, the functions, the procedures, the types, the domains, the languages, the foreign-data wrappers, the foreign servers, the large objects and the databases, as `aclchk.c` runs them, with the ACL functions of `acl.c`. An ACL is a list of `aclitem` texts. A relation or a schema of the catalog keeps its ACL in the catalog, a database keeps its ACL in its row of `pg_database`, which the caller keeps, and the other objects keep the ACL that `GRANT` gives them in [`Catalog::set_object_acl`](rupg_catalog::Catalog::set_object_acl).

use std::collections::BTreeMap;

use rupg_catalog::{
    ObjRef, PG_CLASS, PG_FOREIGN_DATA_WRAPPER, PG_FOREIGN_SERVER, PG_LANGUAGE, PG_LARGEOBJECT,
    PG_NAMESPACE, PG_PROC, PG_TYPE, RangePart,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_pgcatalog::roles::Roles;
use rupg_sql::nodes::{
    AccessPriv, DropBehavior, GrantStmt, GrantTargetType, Node, ObjectType, RoleSpec, RoleSpecType,
    TypeName,
};

use super::index::system_column;
use super::{Definer, Found, not_yet};
use crate::typename::names;

mod defaults;

const INSERT: u64 = 1;
const SELECT: u64 = 1 << 1;
const UPDATE: u64 = 1 << 2;
const DELETE: u64 = 1 << 3;
const TRUNCATE: u64 = 1 << 4;
const REFERENCES: u64 = 1 << 5;
const TRIGGER: u64 = 1 << 6;
const EXECUTE: u64 = 1 << 7;
const USAGE: u64 = 1 << 8;
const CREATE: u64 = 1 << 9;
const TEMP: u64 = 1 << 10;
const CONNECT: u64 = 1 << 11;
const SET: u64 = 1 << 12;
const ALTER_SYSTEM: u64 = 1 << 13;
const MAINTAIN: u64 = 1 << 14;

/// `ACL_ALL_RIGHTS_COLUMN`.
const ALL_COLUMN: u64 = INSERT | SELECT | UPDATE | REFERENCES;
/// `ACL_ALL_RIGHTS_RELATION`.
const ALL_RELATION: u64 =
    INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN;
/// `ACL_ALL_RIGHTS_SEQUENCE`.
const ALL_SEQUENCE: u64 = USAGE | SELECT | UPDATE;
/// `ACL_ALL_RIGHTS_FUNCTION`.
const ALL_FUNCTION: u64 = EXECUTE;
/// `ACL_ALL_RIGHTS_LANGUAGE`.
const ALL_LANGUAGE: u64 = USAGE;
/// `ACL_ALL_RIGHTS_SCHEMA`.
const ALL_SCHEMA: u64 = USAGE | CREATE;
/// `ACL_ALL_RIGHTS_TYPE`.
const ALL_TYPE: u64 = USAGE;
/// `ACL_ALL_RIGHTS_FDW`.
const ALL_FDW: u64 = USAGE;
/// `ACL_ALL_RIGHTS_FOREIGN_SERVER`.
const ALL_FOREIGN_SERVER: u64 = USAGE;
/// `ACL_ALL_RIGHTS_LARGEOBJECT`.
const ALL_LARGEOBJECT: u64 = SELECT | UPDATE;
/// `ACL_ALL_RIGHTS_DATABASE`.
const ALL_DATABASE: u64 = CREATE | TEMP | CONNECT;

/// The bits of the grant options of an `aclitem`.
const GOPTION_BITS: u64 = 0xFFFF_FFFF << 32;

/// `ACL_ALL_RIGHTS_STR`: the letters of the privileges, in the order of their bits.
const LETTERS: &[u8] = b"arwdDxtXUCTcsAm";

/// `ACL_ID_PUBLIC`.
const PUBLIC: u32 = 0;

/// `ROLE_PG_READ_ALL_DATA`.
const PG_READ_ALL_DATA: u32 = 6181;
/// `ROLE_PG_WRITE_ALL_DATA`.
const PG_WRITE_ALL_DATA: u32 = 6182;
/// `ROLE_PG_MAINTAIN`.
const PG_MAINTAIN: u32 = 6337;

/// `ACL_GRANT_OPTION_FOR`.
fn option_for(privileges: u64) -> u64 {
    (privileges & 0xFFFF_FFFF) << 32
}

/// `ACL_OPTION_TO_PRIVS`.
fn option_to_privs(privileges: u64) -> u64 {
    privileges >> 32
}

/// An `aclitem`: the privileges in the low 32 bits and the grant options in the high 32 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AclItem {
    grantee: u32,
    grantor: u32,
    privs: u64,
}

/// `getid`: a role name at the start of an `aclitem` text, which can be in double quotes, and the rest of the text.
fn take_name(text: &str) -> (String, &str) {
    let mut name = String::new();
    let mut quoted = false;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c == '"' {
            if quoted && chars.peek().is_some_and(|&(_, next)| next == '"') {
                chars.next();
                name.push('"');
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if !quoted && (c == '=' || c == '/') {
            return (name, &text[at..]);
        }
        name.push(c);
    }
    (name, "")
}

/// `aclitemin`: the `aclitem` of a text, or `None` for a role that does not exist.
fn parse_item(roles: &Roles, text: &str) -> Option<AclItem> {
    let (grantee, rest) = take_name(text);
    let rest = rest.strip_prefix('=')?;
    let slash = rest.find('/')?;
    let (grantor, _) = take_name(&rest[slash + 1..]);
    let mut privs = 0;
    let mut last = 0;
    for letter in rest[..slash].bytes() {
        if letter == b'*' {
            privs |= option_for(last);
            continue;
        }
        last = 1 << LETTERS.iter().position(|&l| l == letter)?;
        privs |= last;
    }
    let grantee = if grantee.is_empty() { PUBLIC } else { roles.oid(&grantee)? };
    Some(AclItem { grantee, grantor: roles.oid(&grantor)?, privs })
}

/// The `aclitem`s of the texts of an ACL, without the items of roles that do not exist.
fn parse_acl<S: AsRef<str>>(roles: &Roles, texts: &[S]) -> Vec<AclItem> {
    texts.iter().filter_map(|text| parse_item(roles, text.as_ref())).collect()
}

/// `putid`: a role name, in double quotes when it has a character that is not a letter, a digit or an underscore.
fn put_name(out: &mut String, name: &str) {
    if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
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

/// `aclitemout`.
fn item_text(roles: &Roles, item: AclItem) -> String {
    let name = |oid: u32| roles.name(oid).map_or_else(|| oid.to_string(), str::to_string);
    let mut out = String::new();
    if item.grantee != PUBLIC {
        put_name(&mut out, &name(item.grantee));
    }
    out.push('=');
    for (bit, &letter) in LETTERS.iter().enumerate() {
        let privilege = 1 << bit;
        if item.privs & privilege != 0 {
            out.push(char::from(letter));
        }
        if item.privs & option_for(privilege) != 0 {
            out.push('*');
        }
    }
    out.push('/');
    put_name(&mut out, &name(item.grantor));
    out
}

/// The texts of an ACL.
fn acl_texts(roles: &Roles, acl: &[AclItem]) -> Vec<String> {
    acl.iter().map(|&item| item_text(roles, item)).collect()
}

/// `aclmembers`: the grantees and the grantors of the texts of an ACL, without `PUBLIC`, in order and without duplicates.
pub fn acl_roles(texts: &[String]) -> Vec<u32> {
    let roles = rupg_pgcatalog::roles::current();
    let mut found: Vec<u32> = parse_acl(&roles, texts)
        .into_iter()
        .flat_map(|item| [item.grantee, item.grantor])
        .filter(|&role| role != PUBLIC)
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// The kind of an object that has an ACL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Table,
    Sequence,
    Column,
    Schema,
    Function,
    Type,
    Language,
    Fdw,
    Server,
    LargeObject,
    Database,
}

impl Kind {
    /// The mask of `restrict_and_check_grant` for the kind.
    fn whole(self) -> u64 {
        match self {
            Kind::Table => ALL_RELATION,
            Kind::Sequence => ALL_SEQUENCE,
            Kind::Column => ALL_COLUMN,
            Kind::Schema => ALL_SCHEMA,
            Kind::Function => ALL_FUNCTION,
            Kind::Type => ALL_TYPE,
            Kind::Language => ALL_LANGUAGE,
            Kind::Fdw => ALL_FDW,
            Kind::Server => ALL_FOREIGN_SERVER,
            Kind::LargeObject => ALL_LARGEOBJECT,
            Kind::Database => ALL_DATABASE,
        }
    }

    /// `acldefault`: the privileges of `PUBLIC` come first, then the privileges of the owner.
    fn default_acl(self, owner: u32) -> Vec<AclItem> {
        let (world, own) = match self {
            Kind::Table => (0, ALL_RELATION),
            Kind::Sequence => (0, ALL_SEQUENCE),
            Kind::Column => (0, 0),
            Kind::Schema => (0, ALL_SCHEMA),
            Kind::Function => (EXECUTE, ALL_FUNCTION),
            Kind::Type => (USAGE, ALL_TYPE),
            Kind::Language => (USAGE, ALL_LANGUAGE),
            Kind::Fdw => (0, ALL_FDW),
            Kind::Server => (0, ALL_FOREIGN_SERVER),
            Kind::LargeObject => (0, ALL_LARGEOBJECT),
            Kind::Database => (TEMP | CONNECT, ALL_DATABASE),
        };
        let mut acl = Vec::new();
        if world != 0 {
            acl.push(AclItem { grantee: PUBLIC, grantor: owner, privs: world });
        }
        if own != 0 {
            acl.push(AclItem { grantee: owner, grantor: owner, privs: own });
        }
        acl
    }

    /// The word of `aclcheck_error` for the kind.
    fn word(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::Sequence => "sequence",
            Kind::Column => "column",
            Kind::Schema => "schema",
            Kind::Function => "function",
            Kind::Type => "type",
            Kind::Language => "language",
            Kind::Fdw => "foreign-data wrapper",
            Kind::Server => "foreign server",
            Kind::LargeObject => "large object",
            Kind::Database => "database",
        }
    }
}

/// An object that has an ACL. A column is the column `column` of the relation `oid`.
#[derive(Clone, Copy, Debug)]
struct Object {
    kind: Kind,
    oid: u32,
    column: i16,
}

impl Object {
    fn new(kind: Kind, oid: u32) -> Object {
        Object { kind, oid, column: 0 }
    }
}

/// `ACLMASK_ALL` or `ACLMASK_ANY`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    All,
    Any,
}

/// True when `aclmask` can stop with the result.
fn enough(how: How, result: u64, mask: u64) -> bool {
    match how {
        How::All => result == mask,
        How::Any => result != 0,
    }
}

/// `aclmask`: the privileges of the mask that the role has in the ACL, directly, through `PUBLIC` or through the roles that it has the privileges of. A role with the privileges of the owner has all the grant options.
fn aclmask(roles: &Roles, acl: &[AclItem], role: u32, owner: u32, mask: u64, how: How) -> u64 {
    if mask == 0 {
        return 0;
    }
    let mut result = 0;
    if mask & GOPTION_BITS != 0 && roles.has_privs_of_role(role, owner) {
        result = mask & GOPTION_BITS;
        if enough(how, result, mask) {
            return result;
        }
    }
    for item in acl.iter().filter(|item| item.grantee == PUBLIC || item.grantee == role) {
        result |= item.privs & mask;
        if enough(how, result, mask) {
            return result;
        }
    }
    let mut remaining = mask & !result;
    for item in acl.iter().filter(|item| item.grantee != PUBLIC && item.grantee != role) {
        if item.privs & remaining != 0 && roles.has_privs_of_role(role, item.grantee) {
            result |= item.privs & mask;
            if enough(how, result, mask) {
                return result;
            }
            remaining = mask & !result;
        }
    }
    result
}

/// `aclmask_direct`: as `aclmask`, but only the items of the role itself count, and only the owner has all the grant options.
fn aclmask_direct(acl: &[AclItem], role: u32, owner: u32, mask: u64, how: How) -> u64 {
    if mask == 0 {
        return 0;
    }
    let mut result = 0;
    if mask & GOPTION_BITS != 0 && role == owner {
        result = mask & GOPTION_BITS;
        if enough(how, result, mask) {
            return result;
        }
    }
    for item in acl.iter().filter(|item| item.grantee == role) {
        result |= item.privs & mask;
        if enough(how, result, mask) {
            return result;
        }
    }
    result
}

/// `aclupdate`: adds the privileges of the item to the item of the ACL with the same grantee and grantor, or removes them. An item without privileges goes away, and the grant options that the grantee loses go through `recursive_revoke`.
fn aclupdate(
    roles: &Roles,
    acl: &[AclItem],
    change: AclItem,
    add: bool,
    owner: u32,
    cascade: bool,
) -> Result<Vec<AclItem>> {
    if add && change.privs & GOPTION_BITS != 0 {
        check_circularity(roles, acl, change, owner)?;
    }
    let mut acl = acl.to_vec();
    let at = match acl
        .iter()
        .position(|item| item.grantee == change.grantee && item.grantor == change.grantor)
    {
        Some(at) => at,
        None => {
            acl.push(AclItem { privs: 0, ..change });
            acl.len() - 1
        }
    };
    let old = acl[at].privs;
    let new = if add { old | change.privs } else { old & !change.privs };
    acl[at].privs = new;
    if new == 0 {
        acl.remove(at);
    }
    let lost = option_to_privs(old & !new & GOPTION_BITS);
    if lost != 0 {
        return recursive_revoke(roles, acl, change.grantee, lost, owner, cascade);
    }
    Ok(acl)
}

/// `check_circularity`: a grantor cannot get grant options back from a role that has them from the grantor. The items of the grantee with grant options do not count.
fn check_circularity(roles: &Roles, acl: &[AclItem], change: AclItem, owner: u32) -> Result<()> {
    if change.grantor == owner {
        return Ok(());
    }
    let mut acl = acl.to_vec();
    while let Some(item) = acl
        .iter()
        .find(|item| item.grantee == change.grantee && item.privs & GOPTION_BITS != 0)
        .copied()
    {
        acl = aclupdate(roles, &acl, item, false, owner, true)?;
    }
    let goptions = option_to_privs(change.privs);
    let own = option_to_privs(aclmask(
        roles,
        &acl,
        change.grantor,
        owner,
        option_for(goptions),
        How::All,
    ));
    if goptions & !own != 0 {
        return Err(Error::new(
            SqlState::INVALID_GRANT_OPERATION,
            "grant options cannot be granted back to your own grantor",
        ));
    }
    Ok(())
}

/// `recursive_revoke`: when the grantee loses grant options, the privileges that it granted with them go too. `RESTRICT` refuses that.
fn recursive_revoke(
    roles: &Roles,
    mut acl: Vec<AclItem>,
    grantee: u32,
    mut revoke: u64,
    owner: u32,
    cascade: bool,
) -> Result<Vec<AclItem>> {
    if grantee == owner {
        return Ok(acl);
    }
    let still = aclmask(roles, &acl, grantee, owner, option_for(revoke), How::All);
    revoke &= !option_to_privs(still);
    if revoke == 0 {
        return Ok(acl);
    }
    while let Some(item) = acl
        .iter()
        .find(|item| item.grantor == grantee && item.privs & 0xFFFF_FFFF & revoke != 0)
        .copied()
    {
        if !cascade {
            return Err(Error::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                "dependent privileges exist",
            )
            .with_hint("Use CASCADE to revoke them too."));
        }
        let change =
            AclItem { grantee: item.grantee, grantor: grantee, privs: revoke | option_for(revoke) };
        acl = aclupdate(roles, &acl, change, false, owner, cascade)?;
    }
    Ok(acl)
}

/// `string_to_privilege`. `RULE` is an old privilege that gives nothing.
fn string_to_privilege(name: &str) -> Result<u64> {
    Ok(match name {
        "insert" => INSERT,
        "select" => SELECT,
        "update" => UPDATE,
        "delete" => DELETE,
        "truncate" => TRUNCATE,
        "references" => REFERENCES,
        "trigger" => TRIGGER,
        "execute" => EXECUTE,
        "usage" => USAGE,
        "create" => CREATE,
        "temporary" | "temp" => TEMP,
        "connect" => CONNECT,
        "set" => SET,
        "alter system" => ALTER_SYSTEM,
        "maintain" => MAINTAIN,
        "rule" => 0,
        _ => {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("unrecognized privilege type \"{name}\""),
            ));
        }
    })
}

/// `privilege_to_string`.
fn privilege_to_string(privilege: u64) -> &'static str {
    match privilege {
        INSERT => "INSERT",
        SELECT => "SELECT",
        UPDATE => "UPDATE",
        DELETE => "DELETE",
        TRUNCATE => "TRUNCATE",
        REFERENCES => "REFERENCES",
        TRIGGER => "TRIGGER",
        EXECUTE => "EXECUTE",
        USAGE => "USAGE",
        CREATE => "CREATE",
        TEMP => "TEMP",
        CONNECT => "CONNECT",
        SET => "SET",
        ALTER_SYSTEM => "ALTER SYSTEM",
        _ => "MAINTAIN",
    }
}

/// `InternalGrant`: the statement after the lookup of its roles and privileges.
struct Grant<'s> {
    is_grant: bool,
    objtype: ObjectType,
    all_privs: bool,
    privileges: u64,
    col_privs: Vec<&'s AccessPriv>,
    grantees: Vec<u32>,
    grant_option: bool,
    cascade: bool,
    grantor: Option<&'s RoleSpec>,
}

/// An object with the name of the messages of `restrict_and_check_grant`, and the name of the column for a column.
struct Target<'n> {
    object: Object,
    name: &'n str,
    column: Option<&'n str>,
}

/// A relation of `ExecGrant_Attribute`: its owner, its ACL before the statement and its columns.
struct Rel<'r> {
    oid: u32,
    name: &'r str,
    owner: u32,
    acl: &'r [AclItem],
    columns: &'r [(i16, String)],
}

impl Definer<'_, '_> {
    /// `ExecuteGrantStmt`: finds the objects, the grantees and the privileges, then runs `ExecGrantStmt_oids`.
    pub(super) fn grant(&mut self, stmt: &GrantStmt) -> Result<()> {
        let objects = if stmt.targtype == GrantTargetType::ACL_TARGET_ALL_IN_SCHEMA {
            self.objects_in_schemas(stmt)?
        } else {
            self.grant_objects(stmt)?
        };
        let mut grantees = Vec::new();
        for role in stmt.grantees.iter().flatten() {
            let Node::RoleSpec(role) = role else { return Err(Error::internal("a role")) };
            grantees.push(if role.roletype == RoleSpecType::ROLESPEC_PUBLIC {
                PUBLIC
            } else {
                self.role_oid(role)?
            });
        }
        let (all, word) = match stmt.objtype {
            ObjectType::OBJECT_TABLE => (ALL_RELATION | ALL_SEQUENCE, "relation"),
            ObjectType::OBJECT_SEQUENCE => (ALL_SEQUENCE, "sequence"),
            ObjectType::OBJECT_DOMAIN => (ALL_TYPE, "domain"),
            ObjectType::OBJECT_FUNCTION => (ALL_FUNCTION, "function"),
            ObjectType::OBJECT_LANGUAGE => (ALL_LANGUAGE, "language"),
            ObjectType::OBJECT_FDW => (ALL_FDW, "foreign-data wrapper"),
            ObjectType::OBJECT_FOREIGN_SERVER => (ALL_FOREIGN_SERVER, "foreign server"),
            ObjectType::OBJECT_SCHEMA => (ALL_SCHEMA, "schema"),
            ObjectType::OBJECT_PROCEDURE => (ALL_FUNCTION, "procedure"),
            ObjectType::OBJECT_ROUTINE => (ALL_FUNCTION, "routine"),
            ObjectType::OBJECT_LARGEOBJECT => (ALL_LARGEOBJECT, "large object"),
            ObjectType::OBJECT_DATABASE => (ALL_DATABASE, "database"),
            _ => (ALL_TYPE, "type"),
        };
        let mut grant = Grant {
            is_grant: stmt.is_grant,
            objtype: stmt.objtype,
            all_privs: stmt.privileges.is_empty(),
            privileges: 0,
            col_privs: Vec::new(),
            grantees,
            grant_option: stmt.grant_option,
            cascade: stmt.behavior == DropBehavior::DROP_CASCADE,
            grantor: stmt.grantor.as_deref(),
        };
        for privilege in stmt.privileges.iter().flatten() {
            let Node::AccessPriv(privilege) = privilege else {
                return Err(Error::internal("a privilege"));
            };
            if !privilege.cols.is_empty() {
                if stmt.objtype != ObjectType::OBJECT_TABLE {
                    return Err(Error::new(
                        SqlState::INVALID_GRANT_OPERATION,
                        "column privileges are only valid for relations",
                    ));
                }
                grant.col_privs.push(privilege);
                continue;
            }
            let Some(name) = privilege.priv_name.as_deref() else {
                return Err(Error::internal("AccessPriv node must specify privilege or columns"));
            };
            let bits = string_to_privilege(name)?;
            if bits & !all != 0 {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    format!("invalid privilege type {} for {word}", privilege_to_string(bits)),
                ));
            }
            grant.privileges |= bits;
        }
        let roles = rupg_pgcatalog::roles::current();
        for object in objects {
            if object.kind == Kind::Table {
                self.grant_relation(&roles, &grant, object.oid)?;
            } else {
                self.grant_common(&roles, &grant, object)?;
            }
        }
        Ok(())
    }

    /// `RemoveRoleFromObjectACL`: `REVOKE ALL ... FROM role CASCADE` on the object, with the grantor that `select_best_grantor` picks for the role of the statement. The revoke of a relation takes the privileges of its columns too.
    pub(super) fn remove_role_from_acl(&mut self, role: u32, object: ObjRef) -> Result<()> {
        let (objtype, kind) = match object.class {
            PG_CLASS => (ObjectType::OBJECT_TABLE, Kind::Table),
            PG_NAMESPACE => (ObjectType::OBJECT_SCHEMA, Kind::Schema),
            PG_PROC => (ObjectType::OBJECT_ROUTINE, Kind::Function),
            PG_TYPE => (ObjectType::OBJECT_TYPE, Kind::Type),
            PG_LANGUAGE => (ObjectType::OBJECT_LANGUAGE, Kind::Language),
            PG_FOREIGN_DATA_WRAPPER => (ObjectType::OBJECT_FDW, Kind::Fdw),
            PG_FOREIGN_SERVER => (ObjectType::OBJECT_FOREIGN_SERVER, Kind::Server),
            PG_LARGEOBJECT => (ObjectType::OBJECT_LARGEOBJECT, Kind::LargeObject),
            rupg_catalog::PG_DEFAULT_ACL => {
                return self.remove_role_from_default_acl(role, object.oid);
            }
            class => return Err(Error::internal(format!("unexpected object class {class}"))),
        };
        let grant = Grant {
            is_grant: false,
            objtype,
            all_privs: true,
            privileges: 0,
            col_privs: Vec::new(),
            grantees: vec![role],
            grant_option: false,
            cascade: true,
            grantor: None,
        };
        let roles = rupg_pgcatalog::roles::current();
        if kind == Kind::Table {
            self.grant_relation(&roles, &grant, object.oid)
        } else {
            self.grant_common(&roles, &grant, Object::new(kind, object.oid))
        }
    }

    /// `objectNamesToOids`: the objects that the statement names.
    fn grant_objects(&mut self, stmt: &GrantStmt) -> Result<Vec<Object>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let object = match (stmt.objtype, item) {
                (ObjectType::OBJECT_TABLE | ObjectType::OBJECT_SEQUENCE, Node::RangeVar(rv)) => {
                    let oid = match self.lookup_relation(rv)? {
                        Found::User(oid) => oid,
                        Found::Builtin(row) => row.oid,
                    };
                    Object::new(Kind::Table, oid)
                }
                (ObjectType::OBJECT_SCHEMA, Node::String(name)) => {
                    let name: &str = name;
                    let oid = self.an.schema(name).ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_SCHEMA,
                            format!("schema \"{name}\" does not exist"),
                        )
                    })?;
                    Object::new(Kind::Schema, oid)
                }
                (
                    ObjectType::OBJECT_FUNCTION
                    | ObjectType::OBJECT_PROCEDURE
                    | ObjectType::OBJECT_ROUTINE,
                    Node::ObjectWithArgs(item),
                ) => {
                    let found = self
                        .find_function(stmt.objtype, item, false)?
                        .ok_or_else(|| Error::internal("a function"))?;
                    Object::new(Kind::Function, found.oid)
                }
                (ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN, Node::List(list)) => {
                    let name = TypeName { names: list.clone(), ..TypeName::default() };
                    let (oid, _) =
                        self.an.type_name(&name).map_err(|error| error.with_position(None))?;
                    let domain = match self.catalog.type_by_oid(oid) {
                        Some(ty) => ty.domain.is_some(),
                        None => builtin::type_by_oid(oid).is_some_and(|t| t.kind == b'd'),
                    };
                    if stmt.objtype == ObjectType::OBJECT_DOMAIN && !domain {
                        return Err(Error::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!("\"{}\" is not a domain", names(list).join(".")),
                        ));
                    }
                    Object::new(Kind::Type, oid)
                }
                (ObjectType::OBJECT_LANGUAGE, Node::String(name)) => {
                    let name: &str = name;
                    let oid = self
                        .catalog
                        .languages()
                        .find(|l| l.name == name)
                        .map(|l| l.oid)
                        .or_else(|| {
                            builtin::owned(Owned::Language)
                                .iter()
                                .find(|l| l.name == name)
                                .map(|l| l.oid)
                        })
                        .ok_or_else(|| {
                            Error::new(
                                SqlState::UNDEFINED_OBJECT,
                                format!("language \"{name}\" does not exist"),
                            )
                        })?;
                    Object::new(Kind::Language, oid)
                }
                (ObjectType::OBJECT_FDW, Node::String(name)) => {
                    let name: &str = name;
                    let fdw = self.catalog.fdw_by_name(name).ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("foreign-data wrapper \"{name}\" does not exist"),
                        )
                    })?;
                    Object::new(Kind::Fdw, fdw.oid)
                }
                (ObjectType::OBJECT_FOREIGN_SERVER, Node::String(name)) => {
                    let name: &str = name;
                    let server = self.catalog.server_by_name(name).ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("server \"{name}\" does not exist"),
                        )
                    })?;
                    Object::new(Kind::Server, server.oid)
                }
                (ObjectType::OBJECT_LARGEOBJECT, item) => {
                    Object::new(Kind::LargeObject, self.large_object_oid(item)?)
                }
                (ObjectType::OBJECT_DATABASE, Node::String(name)) => {
                    let name: &str = name;
                    let oid = self.database_row(|row| row.name == name).map(|row| row.oid);
                    let oid = oid.ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_DATABASE,
                            format!("database \"{name}\" does not exist"),
                        )
                    })?;
                    Object::new(Kind::Database, oid)
                }
                _ => return Err(not_yet("GRANT on this kind of object", None)),
            };
            objects.push(object);
        }
        Ok(objects)
    }

    /// `oidparse` of the number of a large object, then the check of `LargeObjectExists`.
    pub(super) fn large_object_oid(&self, node: &Node) -> Result<u32> {
        let oid = match node {
            Node::Integer(n) => *n as u32,
            Node::Float(text) => text.parse::<u32>().map_err(|_| {
                Error::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    format!("value \"{text}\" is out of range for type oid"),
                )
            })?,
            _ => return Err(Error::internal("a number")),
        };
        if self.catalog.large_object(oid).is_none() {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("large object {oid} does not exist"),
            ));
        }
        Ok(oid)
    }

    /// The first row of `pg_database` for which `wanted` is true, with the changes of the statement and of the transaction: the row of a database that `CREATE DATABASE` made or that a statement changed, or a static row of `initdb`.
    fn database_row(
        &self,
        wanted: impl Fn(&rupg_catalog::Database) -> bool,
    ) -> Option<rupg_catalog::Database> {
        let databases = self.an.env.databases();
        let mut row = match databases.iter().find(|row| wanted(row)) {
            Some(row) => row.clone(),
            None => rupg_catalog::Database::initdb()
                .into_iter()
                .find(|row| databases.iter().all(|d| d.oid != row.oid) && wanted(row))?,
        };
        if let Some(change) = self.databases.iter().find(|c| c.oid == row.oid) {
            row.owner = change.owner;
            row.acl.clone_from(&change.acl);
        }
        Some(row)
    }

    /// `objectsInSchemaToOids`: the relations, the sequences or the routines of the schemas, after the check of `USAGE` on each schema.
    fn objects_in_schemas(&mut self, stmt: &GrantStmt) -> Result<Vec<Object>> {
        let roles = rupg_pgcatalog::roles::current();
        let mut objects = Vec::new();
        for name in names(&stmt.objects) {
            let namespace = self.an.schema(name).ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{name}\" does not exist"))
            })?;
            let schema = Object::new(Kind::Schema, namespace);
            if self.pg_aclmask(&roles, schema, self.user, USAGE) == 0 {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {name}"),
                ));
            }
            match stmt.objtype {
                ObjectType::OBJECT_TABLE | ObjectType::OBJECT_SEQUENCE => {
                    let kinds: &[u8] =
                        if stmt.objtype == ObjectType::OBJECT_TABLE { b"rvmfp" } else { b"S" };
                    let mut found: Vec<(u32, u8)> = builtin::named(Named::Class)
                        .iter()
                        .filter(|r| r.namespace == namespace)
                        .filter_map(|r| builtin::class_by_oid(r.oid))
                        .map(|r| (r.oid, r.kind))
                        .collect();
                    found.extend(
                        self.catalog
                            .relations()
                            .filter(|r| r.namespace == namespace)
                            .map(|r| (r.oid, self.kind_of(Found::User(r.oid)))),
                    );
                    for kind in kinds {
                        objects.extend(
                            found
                                .iter()
                                .filter(|(_, k)| k == kind)
                                .map(|&(oid, _)| Object::new(Kind::Table, oid)),
                        );
                    }
                }
                _ => {
                    let wanted = |kind: u8| match stmt.objtype {
                        ObjectType::OBJECT_FUNCTION => kind != b'p',
                        ObjectType::OBJECT_PROCEDURE => kind == b'p',
                        _ => true,
                    };
                    for proc in builtin::procs() {
                        if proc.namespace == namespace
                            && wanted(proc.kind)
                            && self.catalog.function(proc.oid).is_none()
                        {
                            objects.push(Object::new(Kind::Function, proc.oid));
                        }
                    }
                    for function in self.catalog.functions() {
                        if function.namespace == namespace && wanted(function.kind) {
                            objects.push(Object::new(Kind::Function, function.oid));
                        }
                    }
                }
            }
        }
        Ok(objects)
    }

    /// The name of an object for the messages: the relation of a column, without the schema.
    fn object_name(&self, object: Object) -> String {
        let text = |name: Option<&str>| name.unwrap_or_default().to_string();
        match object.kind {
            Kind::Table | Kind::Sequence | Kind::Column => {
                match self.catalog.relation(object.oid) {
                    Some(rel) => rel.name.clone(),
                    None => text(builtin::class_by_oid(object.oid).map(|r| r.name)),
                }
            }
            Kind::Schema => self.schema_name(object.oid),
            Kind::Function => match self.catalog.function(object.oid) {
                Some(function) => function.name.clone(),
                None => text(builtin::proc_by_oid(object.oid).map(|p| p.name)),
            },
            Kind::Type => match self.catalog.type_by_oid(object.oid) {
                Some(ty) => ty.name.clone(),
                None => text(builtin::type_by_oid(object.oid).map(|t| t.name)),
            },
            Kind::Language => match self.catalog.languages().find(|l| l.oid == object.oid) {
                Some(language) => language.name.clone(),
                None => text(builtin::owned_by_oid(Owned::Language, object.oid).map(|l| l.name)),
            },
            Kind::Fdw => text(self.catalog.fdw(object.oid).map(|f| f.name.as_str())),
            Kind::Server => text(self.catalog.server(object.oid).map(|s| s.name.as_str())),
            Kind::LargeObject => format!("large object {}", object.oid),
            Kind::Database => self
                .database_row(|row| row.oid == object.oid)
                .map(|row| row.name)
                .unwrap_or_default(),
        }
    }

    /// The owner of an object and its ACL, or `None` for a null ACL. The owner of a column is the owner of its relation.
    fn current_acl(&self, roles: &Roles, object: Object) -> (u32, Option<Vec<AclItem>>) {
        let stored = |class: u32| {
            self.catalog.object_acl(ObjRef::new(class, object.oid)).map(|acl| parse_acl(roles, acl))
        };
        let builtin_acl = |kind: Owned| {
            let row = builtin::owned_by_oid(kind, object.oid);
            let acl = row.and_then(|r| r.acl).map(|acl| parse_acl(roles, acl));
            (row.map_or(rupg_catalog::BOOTSTRAP_SUPERUSER, |r| r.owner), acl)
        };
        match object.kind {
            Kind::Table | Kind::Sequence => match self.catalog.relation(object.oid) {
                Some(rel) => (rel.owner, rel.acl.as_ref().map(|acl| parse_acl(roles, acl))),
                None => {
                    let row = builtin::class_by_oid(object.oid);
                    let owner = row.map_or(rupg_catalog::BOOTSTRAP_SUPERUSER, |r| r.owner);
                    let acl = stored(PG_CLASS)
                        .or_else(|| row.and_then(|r| r.acl).map(|acl| parse_acl(roles, acl)));
                    (owner, acl)
                }
            },
            Kind::Column => {
                let (owner, _) = self.current_acl(roles, Object::new(Kind::Table, object.oid));
                let acl = match self.catalog.object_acl(ObjRef::column(object.oid, object.column)) {
                    Some([]) => None,
                    Some(acl) => Some(parse_acl(roles, acl)),
                    None if self.catalog.relation(object.oid).is_some() => None,
                    None => builtin::attributes(object.oid)
                        .iter()
                        .find(|a| a.num == object.column)
                        .and_then(|a| a.acl)
                        .map(|acl| parse_acl(roles, acl)),
                };
                (owner, acl)
            }
            Kind::Schema => match self.catalog.schema(object.oid) {
                Some(schema) => (schema.owner, schema.acl.as_ref().map(|a| parse_acl(roles, a))),
                None => {
                    let (owner, acl) = builtin_acl(Owned::Namespace);
                    (owner, stored(PG_NAMESPACE).or(acl))
                }
            },
            Kind::Function => match self.catalog.function(object.oid) {
                Some(function) => (function.owner, stored(PG_PROC)),
                None => {
                    let (owner, acl) = builtin_acl(Owned::Function);
                    (owner, stored(PG_PROC).or(acl))
                }
            },
            Kind::Type => match self.catalog.type_by_oid(object.oid) {
                Some(ty) => (ty.owner, stored(PG_TYPE)),
                None => {
                    let (owner, acl) = builtin_acl(Owned::Type);
                    (owner, stored(PG_TYPE).or(acl))
                }
            },
            Kind::Language => match self.catalog.languages().find(|l| l.oid == object.oid) {
                Some(language) => (language.owner, stored(PG_LANGUAGE)),
                None => {
                    let (owner, acl) = builtin_acl(Owned::Language);
                    (owner, stored(PG_LANGUAGE).or(acl))
                }
            },
            Kind::Fdw => (
                self.catalog.fdw(object.oid).map_or(0, |f| f.owner),
                stored(PG_FOREIGN_DATA_WRAPPER),
            ),
            Kind::Server => {
                (self.catalog.server(object.oid).map_or(0, |s| s.owner), stored(PG_FOREIGN_SERVER))
            }
            Kind::LargeObject => (
                self.catalog.large_object(object.oid).map_or(0, |l| l.owner),
                stored(PG_LARGEOBJECT),
            ),
            Kind::Database => match self.database_row(|row| row.oid == object.oid) {
                Some(row) => (row.owner, row.acl.as_ref().map(|acl| parse_acl(roles, acl))),
                None => (0, None),
            },
        }
    }

    /// Writes the new ACL of an object. An empty ACL of a column is null.
    fn write_acl(&mut self, roles: &Roles, object: Object, acl: &[AclItem]) -> Result<()> {
        let texts = acl_texts(roles, acl);
        let user = match object.kind {
            Kind::Table | Kind::Sequence | Kind::Column => {
                self.catalog.relation(object.oid).is_some()
            }
            Kind::Schema => self.catalog.schema(object.oid).is_some(),
            _ => false,
        };
        match object.kind {
            Kind::Database => {
                let owner =
                    self.database_row(|row| row.oid == object.oid).map_or(0, |row| row.owner);
                self.databases.retain(|c| c.oid != object.oid);
                self.databases.push(rupg_pgcatalog::roles::DatabaseChange {
                    oid: object.oid,
                    owner,
                    acl: Some(texts),
                });
                Ok(())
            }
            Kind::Table | Kind::Sequence if user => self.catalog.set_acl(object.oid, Some(texts)),
            Kind::Schema if user => self.catalog.set_schema_acl(object.oid, Some(texts)),
            Kind::Column => {
                let column = ObjRef::column(object.oid, object.column);
                let stored = if !texts.is_empty() {
                    Some(texts)
                } else if !user
                    && builtin::attributes(object.oid)
                        .iter()
                        .any(|a| a.num == object.column && a.acl.is_some())
                {
                    Some(Vec::new())
                } else {
                    None
                };
                self.catalog.set_object_acl(column, stored);
                Ok(())
            }
            _ => {
                let class = match object.kind {
                    Kind::Table | Kind::Sequence => PG_CLASS,
                    Kind::Schema => PG_NAMESPACE,
                    Kind::Function => PG_PROC,
                    Kind::Type => PG_TYPE,
                    Kind::Fdw => PG_FOREIGN_DATA_WRAPPER,
                    Kind::Server => PG_FOREIGN_SERVER,
                    Kind::LargeObject => PG_LARGEOBJECT,
                    _ => PG_LANGUAGE,
                };
                self.catalog.set_object_acl(ObjRef::new(class, object.oid), Some(texts));
                Ok(())
            }
        }
    }

    /// `pg_class_aclcheck` of `ACL_TRUNCATE` for the current user on the table `oid`, as `truncate_check_perms` does it.
    pub(super) fn may_truncate(&self, oid: u32) -> bool {
        let roles = rupg_pgcatalog::roles::current();
        self.pg_aclmask(&roles, Object::new(Kind::Table, oid), self.user, TRUNCATE) != 0
    }

    /// `pg_aclmask` with `ACLMASK_ANY`: the privileges of the mask that the role has on the object. A superuser has all of them, and the privileges of the relation count for a column.
    fn pg_aclmask(&self, roles: &Roles, object: Object, role: u32, mask: u64) -> u64 {
        if roles.superuser(role) {
            return mask;
        }
        let acl_of = |object: Object| {
            let (owner, acl) = self.current_acl(roles, object);
            (owner, acl.unwrap_or_else(|| object.kind.default_acl(owner)))
        };
        match object.kind {
            Kind::Table | Kind::Sequence => {
                let (owner, acl) = acl_of(object);
                let mut result = aclmask(roles, &acl, role, owner, mask, How::Any);
                if mask & SELECT != 0
                    && result & SELECT == 0
                    && roles.has_privs_of_role(role, PG_READ_ALL_DATA)
                {
                    result |= SELECT;
                }
                let write = INSERT | UPDATE | DELETE;
                if mask & write != 0
                    && result & write == 0
                    && roles.has_privs_of_role(role, PG_WRITE_ALL_DATA)
                {
                    result |= mask & write;
                }
                if mask & MAINTAIN != 0
                    && result & MAINTAIN == 0
                    && roles.has_privs_of_role(role, PG_MAINTAIN)
                {
                    result |= MAINTAIN;
                }
                result
            }
            Kind::Column => {
                let kind = if self.relation_kind(object.oid) == b'S' {
                    Kind::Sequence
                } else {
                    Kind::Table
                };
                let class = self.pg_aclmask(roles, Object::new(kind, object.oid), role, mask);
                let (owner, acl) = self.current_acl(roles, object);
                class | acl.map_or(0, |acl| aclmask(roles, &acl, role, owner, mask, How::Any))
            }
            Kind::Schema => {
                let (owner, acl) = acl_of(object);
                let mut result = aclmask(roles, &acl, role, owner, mask, How::Any);
                if mask & USAGE != 0
                    && result & USAGE == 0
                    && (roles.has_privs_of_role(role, PG_READ_ALL_DATA)
                        || roles.has_privs_of_role(role, PG_WRITE_ALL_DATA))
                {
                    result |= USAGE;
                }
                result
            }
            _ => {
                let (owner, acl) = acl_of(object);
                aclmask(roles, &acl, role, owner, mask, How::Any)
            }
        }
    }

    /// `object_aclcheck` of `USAGE` on a foreign-data wrapper, or on a foreign server when `fdw` is false: true when the role may use it.
    pub(super) fn foreign_usage_allowed(&self, fdw: bool, oid: u32, role: u32) -> bool {
        let roles = rupg_pgcatalog::roles::current();
        let kind = if fdw { Kind::Fdw } else { Kind::Server };
        self.pg_aclmask(&roles, Object::new(kind, oid), role, USAGE) != 0
    }

    /// `object_aclcheck` of `USAGE` on a language for the role of the statement.
    pub(super) fn language_usage_allowed(&self, language: u32) -> bool {
        let roles = rupg_pgcatalog::roles::current();
        self.pg_aclmask(&roles, Object::new(Kind::Language, language), self.user, USAGE) != 0
    }

    /// `object_aclcheck` of `CREATE` on a schema: true when the role may create objects in it.
    pub(super) fn schema_create_allowed(&self, namespace: u32, role: u32) -> bool {
        let roles = rupg_pgcatalog::roles::current();
        self.pg_aclmask(&roles, Object::new(Kind::Schema, namespace), role, CREATE) != 0
    }

    /// `object_aclcheck` of `USAGE` on a type, with `pg_type_aclmask` and `aclcheck_error_type`. An array type has the privileges of its element type, and a multirange type has the privileges of its range type.
    pub(super) fn check_type_usage(&self, ty: u32) -> Result<()> {
        let element = crate::types::element(ty);
        let shown = if element != 0 { element } else { ty };
        let range = crate::types::multirange_range(shown);
        let owner = if range != 0 { range } else { shown };
        let roles = rupg_pgcatalog::roles::current();
        if self.pg_aclmask(&roles, Object::new(Kind::Type, owner), self.user, USAGE) != 0 {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for type {}", self.type_text(shown)),
        ))
    }

    /// `check_usage_on_types`: `USAGE` on each type that the references of a stored expression name, as `CheckUsageOnTypesInExpr` and `CheckUsageOnTypesInSingleRelExpr` check it. A pinned type needs no check.
    pub(super) fn check_usage_on_types(&self, refs: &[ObjRef]) -> Result<()> {
        for found in refs {
            if found.class == PG_TYPE && found.oid >= super::table::FIRST_UNPINNED_OID {
                self.check_type_usage(found.oid)?;
            }
        }
        Ok(())
    }

    /// `pg_class_aclcheck` of `SELECT` on a relation of the kind `kind`, with `aclcheck_error` for the object type of `get_relkind_objtype`.
    pub(super) fn check_relation_select(&self, relation: u32, kind: u8, name: &str) -> Result<()> {
        let roles = rupg_pgcatalog::roles::current();
        let object_kind = if kind == b'S' { Kind::Sequence } else { Kind::Table };
        if self.pg_aclmask(&roles, Object::new(object_kind, relation), self.user, SELECT) != 0 {
            return Ok(());
        }
        let shown = match kind {
            b'S' => "sequence",
            b'v' => "view",
            b'm' => "materialized view",
            b'f' => "foreign table",
            _ => "table",
        };
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for {shown} {name}"),
        ))
    }

    /// `pg_class_aclcheck` of `MAINTAIN` on a relation of the catalog or built in: true when the role may run `REINDEX` and the other statements of upkeep on it. A member of `pg_maintain` has the privilege on each relation.
    pub(super) fn may_maintain(&self, relation: u32) -> bool {
        let roles = rupg_pgcatalog::roles::current();
        self.pg_aclmask(&roles, Object::new(Kind::Table, relation), self.user, MAINTAIN) != 0
    }

    /// `has_privs_of_role` of the role of the statement for `pg_maintain`.
    pub(super) fn has_pg_maintain(&self) -> bool {
        rupg_pgcatalog::roles::current().has_privs_of_role(self.user, PG_MAINTAIN)
    }

    /// `object_aclcheck` of `EXECUTE` on a function, with `aclcheck_error`. `name` is the name of the function as the statement gives it.
    pub(super) fn check_function_execute(&self, function: u32, name: &str) -> Result<()> {
        let roles = rupg_pgcatalog::roles::current();
        if self.pg_aclmask(&roles, Object::new(Kind::Function, function), self.user, EXECUTE) != 0 {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for function {name}"),
        ))
    }

    /// The `relkind` of a relation of the catalog or built in.
    fn relation_kind(&self, oid: u32) -> u8 {
        match self.catalog.relation(oid) {
            Some(_) => self.kind_of(Found::User(oid)),
            None => builtin::class_by_oid(oid).map_or(b'r', |r| r.kind),
        }
    }

    /// `select_best_grantor`: the role that grants, and the grant options that it has for the privileges. The owner and a superuser grant as the owner. Another role grants as itself or as a role that it has the privileges of, the one with all the grant options or else the most of them.
    fn select_best_grantor(
        &self,
        roles: &Roles,
        grant: &Grant<'_>,
        privileges: u64,
        acl: &[AclItem],
        owner: u32,
    ) -> Result<(u32, u64)> {
        let needed = option_for(privileges);
        if let Some(granted_by) = grant.grantor {
            let grantor = self.role_oid(granted_by)?;
            if !roles.has_privs_of_role(self.user, grantor) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!(
                        "must inherit privileges of role \"{}\"",
                        roles.name(grantor).unwrap_or_default()
                    ),
                ));
            }
            return Ok((grantor, aclmask_direct(acl, grantor, owner, needed, How::All)));
        }
        if self.user == owner || roles.superuser(self.user) {
            return Ok((owner, needed));
        }
        let mut best = (self.user, 0);
        let mut count = 0;
        for other in roles.privs_of(self.user) {
            let privs = aclmask_direct(acl, other, owner, needed, How::All);
            if privs == needed {
                return Ok((other, privs));
            }
            if privs != 0 && privs.count_ones() > count {
                best = (other, privs);
                count = privs.count_ones();
            }
        }
        Ok(best)
    }

    /// `restrict_and_check_grant`: the privileges that the grantor can grant or revoke, with the warnings of the standard. A grantor without any privilege on the object gets an error.
    fn restrict(
        &mut self,
        roles: &Roles,
        grant: &Grant<'_>,
        all_privs: bool,
        privileges: u64,
        named: &Target<'_>,
        (grantor, avail): (u32, u64),
    ) -> Result<u64> {
        let whole = named.object.kind.whole();
        if avail == 0
            && self.pg_aclmask(roles, named.object, grantor, whole | option_for(whole)) == 0
        {
            let message = match named.column {
                Some(column) => {
                    format!(
                        "permission denied for column \"{column}\" of relation \"{}\"",
                        named.name
                    )
                }
                None => {
                    format!("permission denied for {} {}", named.object.kind.word(), named.name)
                }
            };
            return Err(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, message));
        }
        let this = privileges & option_to_privs(avail);
        let what = match named.column {
            Some(column) => format!("column \"{column}\" of relation \"{}\"", named.name),
            None => format!("\"{}\"", named.name),
        };
        let partial = !all_privs && this != privileges;
        if grant.is_grant {
            if this == 0 {
                self.warning(
                    SqlState::WARNING_PRIVILEGE_NOT_GRANTED,
                    format!("no privileges were granted for {what}"),
                );
            } else if partial {
                self.warning(
                    SqlState::WARNING_PRIVILEGE_NOT_GRANTED,
                    format!("not all privileges were granted for {what}"),
                );
            }
        } else if this == 0 {
            self.warning(
                SqlState::WARNING_PRIVILEGE_NOT_REVOKED,
                format!("no privileges could be revoked for {what}"),
            );
        } else if partial {
            self.warning(
                SqlState::WARNING_PRIVILEGE_NOT_REVOKED,
                format!("not all privileges could be revoked for {what}"),
            );
        }
        Ok(this)
    }

    /// `merge_acl_with_grant`: the ACL after the statement gives or takes the privileges for each grantee. `GRANT` gives the grant options only `WITH GRANT OPTION`, and `REVOKE GRANT OPTION FOR` takes only the grant options.
    fn merge(
        roles: &Roles,
        grant: &Grant<'_>,
        acl: &[AclItem],
        privileges: u64,
        grantor: u32,
        owner: u32,
    ) -> Result<Vec<AclItem>> {
        let mut acl = acl.to_vec();
        for &grantee in &grant.grantees {
            if grant.is_grant && grant.grant_option && grantee == PUBLIC {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "grant options can only be granted to roles",
                ));
            }
            let rights = if grant.is_grant || !grant.grant_option { privileges } else { 0 };
            let options =
                if !grant.is_grant || grant.grant_option { option_for(privileges) } else { 0 };
            let change = AclItem { grantee, grantor, privs: rights | options };
            acl = aclupdate(roles, &acl, change, grant.is_grant, owner, grant.cascade)?;
        }
        Ok(acl)
    }

    /// The columns of a relation that can have privileges: the system columns, except for a view, and the columns that are not dropped.
    fn grant_columns(&self, oid: u32, kind: u8) -> Vec<(i16, String)> {
        let Some(rel) = self.catalog.relation(oid) else {
            return builtin::attributes(oid)
                .iter()
                .filter(|a| !a.dropped)
                .map(|a| (a.num, a.name.to_string()))
                .collect();
        };
        let mut columns = Vec::new();
        if kind != b'v' {
            for name in ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"] {
                if let Some((number, _)) = system_column(name) {
                    columns.push((number, name.to_string()));
                }
            }
        }
        for (at, column) in rel.columns.iter().enumerate() {
            if !column.dropped
                && let Ok(number) = i16::try_from(at + 1)
            {
                columns.push((number, column.name.clone()));
            }
        }
        columns
    }

    /// `ExecGrant_Relation`: the privileges of a relation, then the privileges of its columns. `REVOKE` of a privilege that a column can have revokes it from each column too.
    fn grant_relation(&mut self, roles: &Roles, grant: &Grant<'_>, oid: u32) -> Result<()> {
        let kind = self.relation_kind(oid);
        let name = self.object_name(Object::new(Kind::Table, oid));
        let wrong = |message: String| Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message));
        if matches!(kind, b'i' | b'I') {
            return wrong(format!("\"{name}\" is an index"));
        }
        if kind == b'c' {
            return wrong(format!("\"{name}\" is a composite type"));
        }
        let sequence = kind == b'S';
        if grant.objtype == ObjectType::OBJECT_SEQUENCE && !sequence {
            return wrong(format!("\"{name}\" is not a sequence"));
        }
        let mut this = match (grant.all_privs && grant.privileges == 0, sequence) {
            (true, true) => ALL_SEQUENCE,
            (true, false) => ALL_RELATION,
            (false, _) => grant.privileges,
        };
        if grant.objtype == ObjectType::OBJECT_TABLE {
            if sequence {
                if this & !ALL_SEQUENCE != 0 {
                    self.warning(
                        SqlState::INVALID_GRANT_OPERATION,
                        format!(
                            "sequence \"{name}\" only supports USAGE, SELECT, and UPDATE privileges"
                        ),
                    );
                    this &= ALL_SEQUENCE;
                }
            } else if this & !ALL_RELATION != 0 {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "invalid privilege type USAGE for table",
                ));
            }
        }
        let columns = self.grant_columns(oid, kind);
        let mut col_privileges: BTreeMap<i16, u64> = BTreeMap::new();
        if !grant.is_grant && this & ALL_COLUMN != 0 {
            for (number, _) in &columns {
                *col_privileges.entry(*number).or_default() |= this & ALL_COLUMN;
            }
        }
        let object = Object::new(if sequence { Kind::Sequence } else { Kind::Table }, oid);
        let (owner, acl) = self.current_acl(roles, object);
        let old_rel_acl = acl.unwrap_or_else(|| object.kind.default_acl(owner));
        if this != 0 {
            let best = self.select_best_grantor(roles, grant, this, &old_rel_acl, owner)?;
            let named = Target { object, name: &name, column: None };
            let this = self.restrict(roles, grant, grant.all_privs, this, &named, best)?;
            let grantor = best.0;
            let acl = Self::merge(roles, grant, &old_rel_acl, this, grantor, owner)?;
            self.write_acl(roles, object, &acl)?;
        }
        for privilege in &grant.col_privs {
            let mut bits = match privilege.priv_name.as_deref() {
                None => ALL_COLUMN,
                Some(name) => string_to_privilege(name)?,
            };
            if bits & !ALL_COLUMN != 0 {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    format!("invalid privilege type {} for column", privilege_to_string(bits)),
                ));
            }
            if sequence && bits & !SELECT != 0 {
                self.warning(
                    SqlState::INVALID_GRANT_OPERATION,
                    format!("sequence \"{name}\" only supports SELECT column privileges"),
                );
                bits &= SELECT;
            }
            for column in names(&privilege.cols) {
                let Some(&(number, _)) = columns.iter().find(|(_, n)| n == column) else {
                    return Err(Error::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{column}\" of relation \"{name}\" does not exist"),
                    ));
                };
                *col_privileges.entry(number).or_default() |= bits;
            }
        }
        let rel = Rel { oid, name: &name, owner, acl: &old_rel_acl, columns: &columns };
        for (number, bits) in col_privileges {
            if bits != 0 {
                self.grant_attribute(roles, grant, &rel, number, bits)?;
            }
        }
        Ok(())
    }

    /// `ExecGrant_Attribute`: the privileges of a column. The grantor can use the privileges of the relation before the statement, with the privileges of the column.
    fn grant_attribute(
        &mut self,
        roles: &Roles,
        grant: &Grant<'_>,
        rel: &Rel<'_>,
        number: i16,
        privileges: u64,
    ) -> Result<()> {
        let object = Object { kind: Kind::Column, oid: rel.oid, column: number };
        let old = self.current_acl(roles, object).1.unwrap_or_default();
        let merged: Vec<AclItem> = rel.acl.iter().chain(&old).copied().collect();
        let best = self.select_best_grantor(roles, grant, privileges, &merged, rel.owner)?;
        let column = rel.columns.iter().find(|(n, _)| *n == number).map(|(_, name)| name.as_str());
        let named = Target { object, name: rel.name, column };
        let all = privileges == ALL_COLUMN;
        let this = self.restrict(roles, grant, all, privileges, &named, best)?;
        let acl = Self::merge(roles, grant, &old, this, best.0, rel.owner)?;
        self.write_acl(roles, object, &acl)
    }

    /// `ExecGrant_common` for a schema, a function, a type or a language, with the checks of `ExecGrant_Type_check` and `ExecGrant_Language_check`.
    fn grant_common(&mut self, roles: &Roles, grant: &Grant<'_>, object: Object) -> Result<()> {
        if object.kind == Kind::Type {
            let (array, multirange) = match self.catalog.type_by_oid(object.oid) {
                Some(ty) => (
                    ty.element != 0 && ty.relation == 0 && ty.domain.is_none(),
                    matches!(ty.range, Some(RangePart::Multirange(_))),
                ),
                None => {
                    let row = builtin::type_by_oid(object.oid);
                    (
                        row.is_some_and(|t| t.elem != 0 && t.len == -1 && t.subscript != 0),
                        row.is_some_and(|t| t.kind == b'm'),
                    )
                }
            };
            if array {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "cannot set privileges of array types",
                )
                .with_hint("Set the privileges of the element type instead."));
            }
            if multirange {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "cannot set privileges of multirange types",
                )
                .with_hint("Set the privileges of the range type instead."));
            }
        }
        let name = self.object_name(object);
        if object.kind == Kind::Language {
            // Of the built-in languages, only `sql` is trusted.
            let trusted = match self.catalog.languages().find(|l| l.oid == object.oid) {
                Some(language) => language.trusted,
                None => name == "sql",
            };
            if !trusted {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("language \"{name}\" is not trusted"),
                )
                .with_detail(
                    "GRANT and REVOKE are not allowed on untrusted languages, because only superusers can use untrusted languages.",
                ));
            }
        }
        let (owner, acl) = self.current_acl(roles, object);
        let old = acl.unwrap_or_else(|| object.kind.default_acl(owner));
        let privileges = if grant.all_privs && grant.privileges == 0 {
            object.kind.whole()
        } else {
            grant.privileges
        };
        let best = self.select_best_grantor(roles, grant, privileges, &old, owner)?;
        let named = Target { object, name: &name, column: None };
        let this = self.restrict(roles, grant, grant.all_privs, privileges, &named, best)?;
        let acl = Self::merge(roles, grant, &old, this, best.0, owner)?;
        self.write_acl(roles, object, &acl)
    }
}

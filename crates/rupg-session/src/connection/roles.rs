//! `CREATE ROLE`, `ALTER ROLE`, `ALTER ROLE ... RENAME TO`, `DROP ROLE` and `GRANT` and `REVOKE` of a role, as `user.c` runs them. A statement changes a copy of the roles of the lease, and the copy takes the place of the roles of the lease when the statement ends with no error. The commit of the transaction gives the roles to the store. The roles of `initdb` are static rows, so a statement that changes the attributes, the name or the row of one of them gives `0A000`.

use std::sync::Arc;

use rupg_catalog::{
    Catalog, ObjRef, PG_CLASS, PG_COLLATION, PG_EXTENSION, PG_LANGUAGE, PG_NAMESPACE, PG_PROC,
    PG_TS_CONFIG, PG_TS_DICT, PG_TYPE, RangePart, RelKind,
};
use rupg_common::{Error, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::roles::{Attrs, Member, Role, Roles};
use rupg_sql::Severity;
use rupg_sql::nodes::{
    AlterRoleStmt, CreateRoleStmt, DefElem, DropBehavior, DropRoleStmt, GrantRoleStmt, Node,
    ObjectType, RenameStmt, RoleSpec, RoleSpecType, RoleStmtType,
};
use rupg_types::{Value, oid};
use rupg_wire::{
    CommandTag, Hashes, PasswordType, SCRAM_ITERATIONS, SCRAM_SALT_LEN, ScramSecret, md5_encrypt,
    prepare_password, verify_password,
};

use super::copy::{def_boolean, def_string};
use super::databases::def_int32;
use super::{Connection, Done};
use crate::utility::Notice;

/// `BOOTSTRAP_SUPERUSERID`.
const BOOTSTRAP_SUPERUSER: u32 = 10;
/// `ROLE_PG_DATABASE_OWNER`.
const PG_DATABASE_OWNER: u32 = 6171;
/// `FirstNormalObjectId`: the roles of `initdb` have smaller OIDs.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;
/// `AuthMemRelationId`.
const PG_AUTH_MEMBERS: u32 = 1261;
/// `DatabaseRelationId`.
const PG_DATABASE: u32 = 1262;
/// `MAX_ENCRYPTED_PASSWORD_LEN`.
const MAX_ENCRYPTED_PASSWORD_LEN: usize = 512;
/// `MAX_REPORTED_DEPS`: the number of objects that the detail of `checkSharedDependencies` names.
const MAX_REPORTED_DEPS: usize = 100;

/// True for a statement of this module.
pub(crate) fn is_role_statement(node: &Node) -> bool {
    match node {
        Node::CreateRoleStmt(_)
        | Node::AlterRoleStmt(_)
        | Node::DropRoleStmt(_)
        | Node::GrantRoleStmt(_) => true,
        Node::RenameStmt(stmt) => stmt.renameType == ObjectType::OBJECT_ROLE,
        _ => false,
    }
}

/// The options of `CREATE ROLE` and `ALTER ROLE`, as the loops of `CreateRole` and `AlterRole` read them.
#[derive(Default)]
struct Options<'a> {
    password: Option<&'a DefElem>,
    superuser: Option<&'a DefElem>,
    inherit: Option<&'a DefElem>,
    createrole: Option<&'a DefElem>,
    createdb: Option<&'a DefElem>,
    login: Option<&'a DefElem>,
    replication: Option<&'a DefElem>,
    connlimit: Option<&'a DefElem>,
    add_to: Option<&'a DefElem>,
    members: Option<&'a DefElem>,
    admins: Option<&'a DefElem>,
    valid_until: Option<&'a DefElem>,
    bypassrls: Option<&'a DefElem>,
}

impl<'a> Options<'a> {
    /// The options of `options`. `create` is true for `CREATE ROLE`, and `members` is true for `ALTER GROUP ... ADD USER` and `DROP USER`.
    fn read(
        options: &'a rupg_sql::nodes::List,
        create: bool,
        members: bool,
        notices: &mut Vec<Notice>,
    ) -> Result<Options<'a>, Error> {
        let mut read = Options::default();
        for node in options.iter().flatten() {
            let Node::DefElem(def) = node else { continue };
            let def = &**def;
            let name = def.defname.as_deref().unwrap_or_default();
            let slot = match name {
                "password" => &mut read.password,
                "sysid" if create => {
                    notices.push(notice("SYSID can no longer be specified".to_string()));
                    continue;
                }
                "superuser" => &mut read.superuser,
                "inherit" => &mut read.inherit,
                "createrole" => &mut read.createrole,
                "createdb" => &mut read.createdb,
                "canlogin" => &mut read.login,
                "isreplication" => &mut read.replication,
                "connectionlimit" => &mut read.connlimit,
                "addroleto" if create => &mut read.add_to,
                "rolemembers" if create || members => &mut read.members,
                "adminmembers" if create => &mut read.admins,
                "validUntil" => &mut read.valid_until,
                "bypassrls" => &mut read.bypassrls,
                _ => return Err(Error::internal(format!("option \"{name}\" not recognized"))),
            };
            if slot.is_some() {
                return Err(Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
                    .with_position(usize::try_from(def.location).ok()));
            }
            *slot = Some(def);
        }
        Ok(read)
    }

    /// True when the statement changes an attribute that `AlterRole` checks with `CREATEROLE` and the admin option.
    fn changes_attributes(&self) -> bool {
        [
            self.inherit,
            self.createrole,
            self.createdb,
            self.login,
            self.connlimit,
            self.valid_until,
            self.replication,
            self.bypassrls,
        ]
        .iter()
        .any(Option::is_some)
    }
}

/// The value of a Boolean option.
fn flag(option: Option<&DefElem>) -> Result<Option<bool>, Error> {
    option.map(def_boolean).transpose()
}

/// The roles of an option that has a list of roles.
fn specs(option: Option<&DefElem>) -> Vec<&RoleSpec> {
    let Some(Node::List(list)) = option.and_then(|def| def.arg.as_ref()) else { return Vec::new() };
    list.iter()
        .flatten()
        .filter_map(|node| match node {
            Node::RoleSpec(spec) => Some(&**spec),
            _ => None,
        })
        .collect()
}

/// `GrantRoleOptions`: the options that the statement gives, each `None` when the statement does not give it.
#[derive(Clone, Copy, Default)]
struct GrantOptions {
    admin: Option<bool>,
    inherit: Option<bool>,
    set: Option<bool>,
}

/// `RevokeRoleGrantAction`.
#[derive(Clone, Copy, PartialEq)]
enum Action {
    Noop,
    Delete,
    RemoveAdmin,
    RemoveInherit,
    RemoveSet,
}

/// The walks of `roles_is_member_of`.
#[derive(Clone, Copy, PartialEq)]
enum Recurse {
    /// Each membership.
    Members,
    /// The memberships with `inherit_option`.
    Privs,
}

/// `roles_is_member_of`: the roles that `role` is a member of, directly or indirectly, with `role` first, and the first role of the walk that has the admin option on `admin_of`. As in `rupg-func`, the bootstrap superuser owns each database, so it is a member of `pg_database_owner`.
fn member_of(roles: &Roles, role: u32, how: Recurse, admin_of: u32) -> (Vec<u32>, Option<u32>) {
    let mut reached = vec![role];
    let mut admin = None;
    let mut at = 0;
    while let Some(&member) = reached.get(at) {
        at += 1;
        for edge in roles.edges().filter(|m| m.member == member) {
            if edge.role == admin_of && edge.admin && admin.is_none() {
                admin = Some(member);
            }
            if how == Recurse::Privs && !edge.inherit {
                continue;
            }
            if !reached.contains(&edge.role) {
                reached.push(edge.role);
            }
        }
        if member == BOOTSTRAP_SUPERUSER && !reached.contains(&PG_DATABASE_OWNER) {
            reached.push(PG_DATABASE_OWNER);
        }
    }
    (reached, admin)
}

/// `has_privs_of_role`.
fn has_privs_of_role(roles: &Roles, member: u32, role: u32) -> bool {
    member == role
        || roles.superuser(member)
        || member_of(roles, member, Recurse::Privs, 0).0.contains(&role)
}

/// `is_member_of_role_nosuper`.
fn is_member_nosuper(roles: &Roles, member: u32, role: u32) -> bool {
    member == role || member_of(roles, member, Recurse::Members, 0).0.contains(&role)
}

/// `has_admin_privs_of_role`.
fn has_admin_privs_of_role(roles: &Roles, member: u32, role: u32) -> bool {
    if roles.superuser(member) {
        return true;
    }
    member != role && member_of(roles, member, Recurse::Privs, role).1.is_some()
}

/// `is_admin_of_role`.
fn is_admin_of_role(roles: &Roles, member: u32, role: u32) -> bool {
    if roles.superuser(member) {
        return true;
    }
    member != role && member_of(roles, member, Recurse::Members, role).1.is_some()
}

/// `select_best_admin`.
fn select_best_admin(roles: &Roles, member: u32, role: u32) -> Option<u32> {
    if member == role {
        return None;
    }
    member_of(roles, member, Recurse::Privs, role).1
}

/// `IsReservedName`.
fn reserved(name: &str) -> bool {
    name.starts_with("pg_")
}

fn reserved_error(name: &str) -> Error {
    Error::new(SqlState::RESERVED_NAME, format!("role name \"{name}\" is reserved"))
}

fn does_not_exist(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist"))
}

fn denied(message: &str, detail: String) -> Error {
    Error::new(SqlState::INSUFFICIENT_PRIVILEGE, message).with_detail(detail)
}

fn notice(message: String) -> Notice {
    Notice {
        severity: Severity::Notice,
        error: Error::new(SqlState::SUCCESSFUL_COMPLETION, message),
    }
}

fn newline_check(name: &str) -> Result<(), Error> {
    if name.contains(['\n', '\r']) {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("role name \"{name}\" contains a newline or carriage return character"),
        ));
    }
    Ok(())
}

fn connection_limit(option: Option<&DefElem>) -> Result<Option<i32>, Error> {
    let Some(def) = option else { return Ok(None) };
    let limit = def_int32(def)?;
    if limit < -1 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid connection limit: {limit}"),
        ));
    }
    Ok(Some(limit))
}

/// The error for a change of a role of `initdb`.
fn static_role(name: &str) -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("a change of the role \"{name}\" of initdb is not supported yet"),
    )
}

/// The role with this OID, which a statement can change.
fn created_mut<'r>(roles: &'r mut Roles, oid: u32, name: &str) -> Result<&'r mut Role, Error> {
    roles.roles.iter_mut().find(|r| r.oid == oid).ok_or_else(|| static_role(name))
}

/// `get_role_oid`.
fn role_oid(roles: &Roles, name: &str) -> Result<u32, Error> {
    roles.oid(name).ok_or_else(|| does_not_exist(name))
}

/// The name of a role. A role that does not exist has its OID as its name.
fn role_name(roles: &Roles, oid: u32) -> String {
    roles.name(oid).map_or_else(|| oid.to_string(), str::to_owned)
}

/// `plan_recursive_revoke`.
fn plan_recursive_revoke(
    list: &[Member],
    actions: &mut [Action],
    index: usize,
    admin_only: bool,
    cascade: bool,
) -> Result<(), Error> {
    if actions[index] == Action::Delete || (actions[index] == Action::RemoveAdmin && admin_only) {
        return Ok(());
    }
    let row = list[index];
    if admin_only {
        if !row.admin {
            return Ok(());
        }
        actions[index] = Action::RemoveAdmin;
    } else {
        actions[index] = Action::Delete;
        if !row.admin {
            return Ok(());
        }
    }
    let still_admin = list
        .iter()
        .zip(actions.iter())
        .any(|(m, &a)| m.member == row.member && m.admin && a == Action::Noop);
    if still_admin {
        return Ok(());
    }
    for i in 0..list.len() {
        if list[i].grantor == row.member && actions[i] != Action::Delete {
            if !cascade {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    "dependent privileges exist",
                )
                .with_hint("Use CASCADE to revoke them too."));
            }
            plan_recursive_revoke(list, actions, i, false, cascade)?;
        }
    }
    Ok(())
}

/// `get_password_type`.
fn password_type(secret: &str) -> PasswordType {
    PasswordType::of(secret.as_bytes())
}

/// The objects of a catalog that have a dependency of `pg_shdepend` on their owner `role`, as `recordDependencyOnOwner` makes them. An index, a TOAST table, a composite type as a relation, the row type of a table, an array type and a multirange type have none.
fn owned(catalog: &Catalog, role: u32) -> Vec<ObjRef> {
    let mut objects = Vec::new();
    let mut add = |class: u32, oid: u32, owner: u32| {
        if owner == role {
            objects.push(ObjRef { class, oid, sub: 0 });
        }
    };
    for s in catalog.schemas() {
        add(PG_NAMESPACE, s.oid, s.owner);
    }
    for r in catalog.relations() {
        if !matches!(
            r.kind,
            RelKind::Index | RelKind::PartitionedIndex | RelKind::Toast | RelKind::Composite
        ) {
            add(PG_CLASS, r.oid, r.owner);
        }
    }
    for t in catalog.types() {
        let row_type = t.relation != 0
            && catalog.relation(t.relation).is_some_and(|r| r.kind != RelKind::Composite);
        let multirange = matches!(t.range, Some(RangePart::Multirange(_)));
        if t.element == 0 && !row_type && !multirange {
            add(PG_TYPE, t.oid, t.owner);
        }
    }
    for f in catalog.functions() {
        add(PG_PROC, f.oid, f.owner);
    }
    for c in catalog.collations() {
        add(PG_COLLATION, c.oid, c.owner);
    }
    for l in catalog.languages() {
        add(PG_LANGUAGE, l.oid, l.owner);
    }
    for e in catalog.extensions() {
        add(PG_EXTENSION, e.oid, e.owner);
    }
    for d in catalog.ts_dictionaries() {
        add(PG_TS_DICT, d.oid, d.owner);
    }
    for c in catalog.ts_configs() {
        add(PG_TS_CONFIG, c.oid, c.owner);
    }
    objects
}

impl Connection {
    /// Runs a statement of this module on a copy of the roles of the lease.
    pub(super) fn role_statement(
        &mut self,
        node: &Node,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        if self.lease.get_mut().is_none() {
            *self.lease.get_mut() = Some(self.store.lease(self.database_oid, &self.cancel)?);
        }
        let mut work = Roles::clone(&self.roles());
        let tag = match node {
            Node::CreateRoleStmt(stmt) => {
                self.create_role(&mut work, stmt, notices)?;
                CommandTag::CreateRole
            }
            Node::AlterRoleStmt(stmt) => {
                self.alter_role(&mut work, stmt, notices)?;
                CommandTag::AlterRole
            }
            Node::DropRoleStmt(stmt) => {
                self.drop_role(&mut work, stmt, notices)?;
                CommandTag::DropRole
            }
            Node::GrantRoleStmt(stmt) => {
                self.grant_role(&mut work, stmt, notices)?;
                if stmt.is_grant { CommandTag::GrantRole } else { CommandTag::RevokeRole }
            }
            Node::RenameStmt(stmt) => {
                self.rename_role(&mut work, stmt, notices)?;
                CommandTag::AlterRole
            }
            _ => return Err(Error::internal("not a statement of roles")),
        };
        if let Some(lease) = self.lease.get_mut() {
            lease.roles = Arc::new(work);
        }
        Ok(Done::Tag(tag))
    }

    /// `GetNewObjectId`.
    fn role_oid_new(&mut self) -> u32 {
        self.lease
            .get_mut()
            .as_mut()
            .map_or(0, |lease| Arc::make_mut(&mut lease.catalog).take_oid())
    }

    /// `get_rolespec_oid`.
    fn spec_oid(&self, roles: &Roles, spec: &RoleSpec) -> Result<u32, Error> {
        match spec.roletype {
            RoleSpecType::ROLESPEC_CSTRING => {
                role_oid(roles, spec.rolename.as_deref().unwrap_or_default())
            }
            RoleSpecType::ROLESPEC_PUBLIC => Err(does_not_exist("public")),
            _ => Ok(self.role),
        }
    }

    /// `get_rolespec_name`.
    fn spec_name(&self, roles: &Roles, spec: &RoleSpec) -> String {
        match spec.roletype {
            RoleSpecType::ROLESPEC_CSTRING => spec.rolename.as_deref().unwrap_or_default().into(),
            RoleSpecType::ROLESPEC_PUBLIC => "public".into(),
            _ => role_name(roles, self.role),
        }
    }

    /// `roleSpecsToIds`, with the name of each role for the messages.
    fn spec_ids(&self, roles: &Roles, specs: &[&RoleSpec]) -> Result<Vec<(String, u32)>, Error> {
        specs
            .iter()
            .map(|spec| Ok((self.spec_name(roles, spec), self.spec_oid(roles, spec)?)))
            .collect()
    }

    /// `has_createrole_privilege` for the current user.
    fn createrole(&self, roles: &Roles) -> bool {
        roles.attrs(self.role).is_some_and(|a| a.superuser || a.createrole)
    }

    /// True when the current user is a superuser or has the attribute that `pick` gives.
    fn has_attr(&self, roles: &Roles, pick: fn(&Attrs) -> bool) -> bool {
        roles.attrs(self.role).is_some_and(|a| a.superuser || pick(&a))
    }

    /// `encrypt_password` with `password_encryption`, and the check of `CreateRole` and `AlterRole` for an empty password. `None` is a password to clear.
    fn encrypt(
        &self,
        name: &str,
        password: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<Option<String>, Error> {
        if password.is_empty()
            || verify_password(&Hashes, name.as_bytes(), password.as_bytes(), b"")
        {
            notices.push(notice(
                "empty string is not a valid password, clearing password".to_string(),
            ));
            return Ok(None);
        }
        let settings = self.settings.borrow();
        let secret = if password_type(password) != PasswordType::Plaintext {
            password.to_string()
        } else if settings.get("password_encryption").as_deref() == Some("md5") {
            md5_encrypt(&Hashes, password.as_bytes(), name.as_bytes())
        } else {
            let iterations = settings
                .get("scram_iterations")
                .and_then(|v| v.parse().ok())
                .unwrap_or(SCRAM_ITERATIONS);
            let mut salt = [0; SCRAM_SALT_LEN];
            self.reader().strong_random(&mut salt);
            ScramSecret::build(&Hashes, &prepare_password(password.as_bytes()), &salt, iterations)
                .to_string()
        };
        if secret.len() > MAX_ENCRYPTED_PASSWORD_LEN {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "encrypted password is too long",
            )
            .with_detail(format!(
                "Encrypted passwords must be no longer than {MAX_ENCRYPTED_PASSWORD_LEN} bytes."
            )));
        }
        if settings.get("md5_password_warnings").as_deref() != Some("off")
            && password_type(&secret) == PasswordType::Md5
        {
            notices.push(Notice::warning(
                Error::new(SqlState::WARNING_DEPRECATED_FEATURE, "setting an MD5-encrypted password")
                    .with_detail("MD5 password support is deprecated and will be removed in a future release of PostgreSQL.")
                    .with_hint("Refer to the PostgreSQL documentation for details about migrating to another password type."),
            ));
        }
        Ok(Some(secret))
    }

    /// The text of `VALID UNTIL` as `timestamptz`.
    fn valid_until(&self, option: Option<&DefElem>) -> Result<Option<i64>, Error> {
        let Some(def) = option else { return Ok(None) };
        let text = def_string(def)?;
        match rupg_func::input(oid::TIMESTAMPTZ, &text, -1, &self.reader())? {
            Value::TimestampTz(time) => Ok(Some(time)),
            _ => Err(Error::internal("timestamptz_in gave a value that is not a timestamptz")),
        }
    }

    /// `CreateRole`.
    fn create_role(
        &mut self,
        work: &mut Roles,
        stmt: &CreateRoleStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let name = stmt.role.as_deref().unwrap_or_default();
        newline_check(name)?;
        let options = Options::read(&stmt.options, true, false, notices)?;
        let mut attrs =
            Attrs { login: stmt.stmt_type == RoleStmtType::ROLESTMT_USER, ..Attrs::default() };
        let password = match options.password.and_then(|def| def.arg.as_ref()) {
            Some(_) => options.password.map(def_string).transpose()?,
            None => None,
        };
        attrs.superuser = flag(options.superuser)?.unwrap_or(attrs.superuser);
        attrs.inherit = flag(options.inherit)?.unwrap_or(attrs.inherit);
        attrs.createrole = flag(options.createrole)?.unwrap_or(attrs.createrole);
        attrs.createdb = flag(options.createdb)?.unwrap_or(attrs.createdb);
        attrs.login = flag(options.login)?.unwrap_or(attrs.login);
        attrs.replication = flag(options.replication)?.unwrap_or(attrs.replication);
        let connlimit = connection_limit(options.connlimit)?.unwrap_or(-1);
        attrs.bypassrls = flag(options.bypassrls)?.unwrap_or(attrs.bypassrls);
        let current = self.role;
        if !work.superuser(current) {
            let only = |attr: &str| {
                denied(
                    "permission denied to create role",
                    format!(
                        "Only roles with the {attr} attribute may create roles with the {attr} attribute."
                    ),
                )
            };
            if !self.createrole(work) {
                return Err(denied(
                    "permission denied to create role",
                    "Only roles with the CREATEROLE attribute may create roles.".to_string(),
                ));
            }
            if attrs.superuser {
                return Err(only("SUPERUSER"));
            }
            if attrs.createdb && !self.has_attr(work, |a| a.createdb) {
                return Err(only("CREATEDB"));
            }
            if attrs.replication && !self.has_attr(work, |a| a.replication) {
                return Err(only("REPLICATION"));
            }
            if attrs.bypassrls && !self.has_attr(work, |a| a.bypassrls) {
                return Err(only("BYPASSRLS"));
            }
        }
        if reserved(name) {
            return Err(
                reserved_error(name).with_detail("Role names starting with \"pg_\" are reserved.")
            );
        }
        if work.oid(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("role \"{name}\" already exists"),
            ));
        }
        let valid_until = self.valid_until(options.valid_until)?;
        let password = match password {
            Some(password) => self.encrypt(name, &password, notices)?,
            None => None,
        };
        let oid = self.role_oid_new();
        work.roles.push(Role {
            oid,
            name: name.to_string(),
            attrs,
            connlimit,
            password,
            valid_until,
        });
        let this = vec![(name.to_string(), oid)];
        for spec in specs(options.add_to) {
            let old = self.spec_oid(work, spec)?;
            let old_name = role_name(work, old);
            self.membership_authorization(work, old, true)?;
            self.add_members(
                work,
                current,
                &old_name,
                old,
                &this,
                None,
                GrantOptions::default(),
                notices,
            )?;
        }
        if !work.superuser(current) {
            let me = vec![(role_name(work, current), current)];
            let only_admin =
                GrantOptions { admin: Some(true), inherit: Some(false), set: Some(false) };
            self.add_members(
                work,
                BOOTSTRAP_SUPERUSER,
                name,
                oid,
                &me,
                Some(BOOTSTRAP_SUPERUSER),
                only_admin,
                notices,
            )?;
            let words = self.settings.borrow().get("createrole_self_grant").unwrap_or_default();
            if !words.is_empty() {
                let mut grant =
                    GrantOptions { admin: None, inherit: Some(false), set: Some(false) };
                for word in words.split(',').map(str::trim) {
                    if word.eq_ignore_ascii_case("inherit") {
                        grant.inherit = Some(true);
                    } else if word.eq_ignore_ascii_case("set") {
                        grant.set = Some(true);
                    }
                }
                self.add_members(work, current, name, oid, &me, Some(current), grant, notices)?;
            }
        }
        let members = self.spec_ids(work, &specs(options.members))?;
        if !members.is_empty() {
            self.add_members(
                work,
                current,
                name,
                oid,
                &members,
                None,
                GrantOptions::default(),
                notices,
            )?;
        }
        let admins = self.spec_ids(work, &specs(options.admins))?;
        if !admins.is_empty() {
            let admin = GrantOptions { admin: Some(true), ..GrantOptions::default() };
            self.add_members(work, current, name, oid, &admins, None, admin, notices)?;
        }
        Ok(())
    }

    /// `AlterRole`.
    fn alter_role(
        &mut self,
        work: &mut Roles,
        stmt: &AlterRoleStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let Some(spec) = stmt.role.as_deref() else {
            return Err(Error::internal("ALTER ROLE with no role"));
        };
        if spec.roletype == RoleSpecType::ROLESPEC_CSTRING {
            let name = spec.rolename.as_deref().unwrap_or_default();
            if reserved(name) {
                return Err(reserved_error(name).with_detail("Cannot alter reserved roles."));
            }
        }
        let options = Options::read(&stmt.options, false, stmt.action != 0, notices)?;
        let password = match options.password.and_then(|def| def.arg.as_ref()) {
            Some(_) => options.password.map(def_string).transpose()?,
            None => None,
        };
        let connlimit = connection_limit(options.connlimit)?;
        let roleid = self.spec_oid(work, spec)?;
        let rolename = role_name(work, roleid);
        let current = self.role;
        let superuser = work.superuser(current);
        if !superuser && work.superuser(roleid) {
            return Err(denied(
                "permission denied to alter role",
                "Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute.".to_string(),
            ));
        }
        if !superuser && options.superuser.is_some() {
            return Err(denied(
                "permission denied to alter role",
                "Only roles with the SUPERUSER attribute may change the SUPERUSER attribute."
                    .to_string(),
            ));
        }
        if !self.createrole(work) || !is_admin_of_role(work, current, roleid) {
            if options.changes_attributes() {
                return Err(denied(
                    "permission denied to alter role",
                    format!(
                        "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{rolename}\" may alter this role."
                    ),
                ));
            }
            if options.password.is_some() && roleid != current {
                return Err(denied(
                    "permission denied to alter role",
                    "To change another role's password, the current user must have the CREATEROLE attribute and the ADMIN option on the role.".to_string(),
                ));
            }
        } else if !superuser {
            let only = |attr: &str| {
                denied(
                    "permission denied to alter role",
                    format!(
                        "Only roles with the {attr} attribute may change the {attr} attribute."
                    ),
                )
            };
            if options.createdb.is_some() && !self.has_attr(work, |a| a.createdb) {
                return Err(only("CREATEDB"));
            }
            if options.replication.is_some() && !self.has_attr(work, |a| a.replication) {
                return Err(only("REPLICATION"));
            }
            if options.bypassrls.is_some() && !self.has_attr(work, |a| a.bypassrls) {
                return Err(only("BYPASSRLS"));
            }
        }
        if options.members.is_some() && !has_admin_privs_of_role(work, current, roleid) {
            return Err(denied(
                "permission denied to alter role",
                format!(
                    "Only roles with the ADMIN option on role \"{rolename}\" may add or drop members."
                ),
            ));
        }
        let valid_until = self.valid_until(options.valid_until)?;
        let should_be_super = flag(options.superuser)?;
        if should_be_super == Some(false) && roleid == BOOTSTRAP_SUPERUSER {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "permission denied to alter role",
            )
            .with_detail("The bootstrap superuser must have the SUPERUSER attribute."));
        }
        let changes = options.superuser.is_some()
            || options.changes_attributes()
            || options.password.is_some();
        if changes {
            let password = match password {
                Some(password) => Some(self.encrypt(&rolename, &password, notices)?),
                None => options.password.map(|_| None),
            };
            let attrs = Attrs {
                superuser: should_be_super.unwrap_or_default(),
                inherit: flag(options.inherit)?.unwrap_or_default(),
                createrole: flag(options.createrole)?.unwrap_or_default(),
                createdb: flag(options.createdb)?.unwrap_or_default(),
                login: flag(options.login)?.unwrap_or_default(),
                replication: flag(options.replication)?.unwrap_or_default(),
                bypassrls: flag(options.bypassrls)?.unwrap_or_default(),
            };
            let row = created_mut(work, roleid, &rolename)?;
            let set = |slot: &mut bool, option: Option<&DefElem>, value: bool| {
                if option.is_some() {
                    *slot = value;
                }
            };
            set(&mut row.attrs.superuser, options.superuser, attrs.superuser);
            set(&mut row.attrs.inherit, options.inherit, attrs.inherit);
            set(&mut row.attrs.createrole, options.createrole, attrs.createrole);
            set(&mut row.attrs.createdb, options.createdb, attrs.createdb);
            set(&mut row.attrs.login, options.login, attrs.login);
            set(&mut row.attrs.replication, options.replication, attrs.replication);
            set(&mut row.attrs.bypassrls, options.bypassrls, attrs.bypassrls);
            if let Some(limit) = connlimit {
                row.connlimit = limit;
            }
            if let Some(password) = password {
                row.password = password;
            }
            if valid_until.is_some() {
                row.valid_until = valid_until;
            }
        }
        if options.members.is_some() {
            let members = self.spec_ids(work, &specs(options.members))?;
            if stmt.action == 1 {
                self.add_members(
                    work,
                    current,
                    &rolename,
                    roleid,
                    &members,
                    None,
                    GrantOptions::default(),
                    notices,
                )?;
            } else if stmt.action == -1 {
                self.del_members(
                    work,
                    current,
                    &rolename,
                    roleid,
                    &members,
                    None,
                    GrantOptions::default(),
                    false,
                    notices,
                )?;
            }
        }
        Ok(())
    }

    /// `RenameRole`.
    fn rename_role(
        &mut self,
        work: &mut Roles,
        stmt: &RenameStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let old = stmt.subname.as_deref().unwrap_or_default();
        let new = stmt.newname.as_deref().unwrap_or_default();
        newline_check(new)?;
        let roleid = role_oid(work, old)?;
        if roleid == self.session_role {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "session user cannot be renamed",
            ));
        }
        if roleid == self.role {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "current user cannot be renamed",
            ));
        }
        if reserved(old) {
            return Err(
                reserved_error(old).with_detail("Role names starting with \"pg_\" are reserved.")
            );
        }
        if reserved(new) {
            return Err(
                reserved_error(new).with_detail("Role names starting with \"pg_\" are reserved.")
            );
        }
        if work.oid(new).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("role \"{new}\" already exists"),
            ));
        }
        if work.superuser(roleid) {
            if !work.superuser(self.role) {
                return Err(denied(
                    "permission denied to rename role",
                    "Only roles with the SUPERUSER attribute may rename roles with the SUPERUSER attribute.".to_string(),
                ));
            }
        } else if !self.createrole(work) || !is_admin_of_role(work, self.role, roleid) {
            return Err(denied(
                "permission denied to rename role",
                format!(
                    "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{old}\" may rename this role."
                ),
            ));
        }
        let row = created_mut(work, roleid, old)?;
        row.name = new.to_string();
        if row.password.as_deref().is_some_and(|p| password_type(p) == PasswordType::Md5) {
            row.password = None;
            notices.push(notice("MD5 password cleared because of role rename".to_string()));
        }
        Ok(())
    }

    /// `DropRole`.
    fn drop_role(
        &mut self,
        work: &mut Roles,
        stmt: &DropRoleStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let current = self.role;
        if !self.createrole(work) {
            return Err(denied(
                "permission denied to drop role",
                "Only roles with the CREATEROLE attribute and the ADMIN option on the target roles may drop roles.".to_string(),
            ));
        }
        let mut dropped = Vec::new();
        for node in stmt.roles.iter().flatten() {
            let Node::RoleSpec(spec) = node else { continue };
            if spec.roletype != RoleSpecType::ROLESPEC_CSTRING {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "cannot use special role specifier in DROP ROLE",
                ));
            }
            let name = spec.rolename.as_deref().unwrap_or_default();
            let Some(roleid) = work.oid(name) else {
                if !stmt.missing_ok {
                    return Err(does_not_exist(name));
                }
                notices.push(notice(format!("role \"{name}\" does not exist, skipping")));
                continue;
            };
            if roleid == current {
                return Err(Error::new(SqlState::OBJECT_IN_USE, "current user cannot be dropped"));
            }
            if roleid == self.session_role {
                return Err(Error::new(SqlState::OBJECT_IN_USE, "session user cannot be dropped"));
            }
            if work.superuser(roleid) && !work.superuser(current) {
                return Err(denied(
                    "permission denied to drop role",
                    "Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute.".to_string(),
                ));
            }
            if !is_admin_of_role(work, current, roleid) {
                return Err(denied(
                    "permission denied to drop role",
                    format!(
                        "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may drop this role."
                    ),
                ));
            }
            work.members.retain(|m| m.role != roleid && m.member != roleid);
            if !dropped.contains(&roleid) {
                dropped.push(roleid);
            }
        }
        for roleid in dropped {
            let name = role_name(work, roleid);
            if roleid < FIRST_NORMAL_OBJECT_ID {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop role {name} because it is required by the database system"
                    ),
                ));
            }
            if let Some(detail) = self.shared_dependencies(work, roleid)? {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("role \"{name}\" cannot be dropped because some objects depend on it"),
                )
                .with_detail(detail));
            }
            work.roles.retain(|r| r.oid != roleid);
        }
        Ok(())
    }

    /// `checkSharedDependencies` for a role: the detail of the error, or `None` when no object depends on the role. The objects of the current database and the shared objects come first, in the order of their OIDs, then a count for each other database.
    fn shared_dependencies(&self, work: &Roles, roleid: u32) -> Result<Option<String>, Error> {
        // (OID, class, deptype, description) of each local or shared object.
        let mut objects: Vec<(u32, u32, u8, String)> = Vec::new();
        let mut remote = Vec::new();
        let local = self.catalog();
        for (row, catalog) in self.store.catalogs() {
            if row.owner == roleid {
                objects.push((row.oid, PG_DATABASE, b'o', format!("database {}", row.name)));
            }
            if row.oid == self.database_oid {
                continue;
            }
            let count = owned(&catalog, roleid).len();
            if count > 0 {
                remote.push((row.name, count));
            }
        }
        let reader = self.reader();
        for object in owned(&local, roleid) {
            let text = rupg_func::describe(object, &reader)?.unwrap_or_default();
            objects.push((object.oid, object.class, b'o', text));
        }
        for m in work.members.iter().filter(|m| m.grantor == roleid) {
            let text = format!(
                "membership of role {} in role {}",
                role_name(work, m.member),
                role_name(work, m.role)
            );
            objects.push((m.oid, PG_AUTH_MEMBERS, b'a', text));
        }
        objects.sort_by_key(|o| (o.0, o.1, o.2));
        let mut lines: Vec<String> = Vec::new();
        let mut hidden = 0;
        for (_, _, deptype, text) in objects {
            if lines.len() >= MAX_REPORTED_DEPS {
                hidden += 1;
                continue;
            }
            lines.push(if deptype == b'o' {
                format!("owner of {text}")
            } else {
                format!("privileges for {text}")
            });
        }
        let mut hidden_databases = 0;
        for (name, count) in remote {
            if lines.len() >= MAX_REPORTED_DEPS {
                hidden_databases += 1;
                continue;
            }
            let noun = if count == 1 { "object" } else { "objects" };
            lines.push(format!("{count} {noun} in database {name}"));
        }
        if lines.is_empty() {
            return Ok(None);
        }
        if hidden > 0 {
            let noun = if hidden == 1 { "object" } else { "objects" };
            lines.push(format!("and {hidden} other {noun} (see server log for list)"));
        }
        if hidden_databases > 0 {
            let noun = if hidden_databases == 1 { "database" } else { "databases" };
            lines.push(format!(
                "and objects in {hidden_databases} other {noun} (see server log for list)"
            ));
        }
        Ok(Some(lines.join("\n")))
    }

    /// `GrantRole`.
    fn grant_role(
        &mut self,
        work: &mut Roles,
        stmt: &GrantRoleStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let mut options = GrantOptions::default();
        for node in stmt.opt.iter().flatten() {
            let Node::DefElem(def) = node else { continue };
            let def = &**def;
            let name = def.defname.as_deref().unwrap_or_default();
            let at = usize::try_from(def.location).ok();
            let value = def_string(def)?;
            let slot = match name {
                "admin" => &mut options.admin,
                "inherit" => &mut options.inherit,
                "set" => &mut options.set,
                _ => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized role option \"{name}\""),
                    )
                    .with_position(at));
                }
            };
            let Some(parsed) = crate::guc::parse_bool(&value) else {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("unrecognized value for role option \"{name}\": \"{value}\""),
                )
                .with_position(at));
            };
            *slot = Some(parsed);
        }
        let grantor = stmt.grantor.as_deref().map(|spec| self.spec_oid(work, spec)).transpose()?;
        let grantees: Vec<&RoleSpec> = stmt
            .grantee_roles
            .iter()
            .flatten()
            .filter_map(|node| match node {
                Node::RoleSpec(spec) => Some(&**spec),
                _ => None,
            })
            .collect();
        let grantees = self.spec_ids(work, &grantees)?;
        let cascade = stmt.behavior != DropBehavior::DROP_RESTRICT;
        for node in stmt.granted_roles.iter().flatten() {
            let Node::AccessPriv(privilege) = node else { continue };
            let Some(rolename) =
                privilege.priv_name.as_deref().filter(|_| privilege.cols.is_empty())
            else {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "column names cannot be included in GRANT/REVOKE ROLE",
                ));
            };
            let roleid = role_oid(work, rolename)?;
            self.membership_authorization(work, roleid, stmt.is_grant)?;
            let current = self.role;
            if stmt.is_grant {
                self.add_members(
                    work, current, rolename, roleid, &grantees, grantor, options, notices,
                )?;
            } else {
                self.del_members(
                    work, current, rolename, roleid, &grantees, grantor, options, cascade, notices,
                )?;
            }
        }
        Ok(())
    }

    /// `check_role_membership_authorization`.
    fn membership_authorization(
        &self,
        roles: &Roles,
        roleid: u32,
        is_grant: bool,
    ) -> Result<(), Error> {
        let name = role_name(roles, roleid);
        if is_grant && roleid == PG_DATABASE_OWNER {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("role \"{name}\" cannot have explicit members"),
            ));
        }
        let (verb, verbs) = if is_grant { ("grant", "grant") } else { ("revoke", "revoke") };
        let message = format!("permission denied to {verb} role \"{name}\"");
        if roles.superuser(roleid) {
            if !roles.superuser(self.role) {
                return Err(denied(
                    &message,
                    format!(
                        "Only roles with the SUPERUSER attribute may {verbs} roles with the SUPERUSER attribute."
                    ),
                ));
            }
        } else if !has_admin_privs_of_role(roles, self.role, roleid) {
            return Err(denied(
                &message,
                format!(
                    "Only roles with the ADMIN option on role \"{name}\" may {verbs} this role."
                ),
            ));
        }
        Ok(())
    }

    /// `check_role_grantor`.
    fn role_grantor(
        &self,
        roles: &Roles,
        current: u32,
        roleid: u32,
        grantor: Option<u32>,
        is_grant: bool,
    ) -> Result<u32, Error> {
        let Some(grantor) = grantor else {
            if roles.superuser(current) {
                return Ok(BOOTSTRAP_SUPERUSER);
            }
            return select_best_admin(roles, current, roleid)
                .ok_or_else(|| Error::internal("no possible grantors"));
        };
        let name = role_name(roles, grantor);
        if is_grant {
            let message = format!("permission denied to grant privileges as role \"{name}\"");
            if !has_privs_of_role(roles, current, grantor) {
                return Err(denied(
                    &message,
                    format!(
                        "Only roles with privileges of role \"{name}\" may grant privileges as this role."
                    ),
                ));
            }
            if grantor != BOOTSTRAP_SUPERUSER
                && select_best_admin(roles, grantor, roleid) != Some(grantor)
            {
                return Err(denied(
                    &message,
                    format!(
                        "The grantor must have the ADMIN option on role \"{}\".",
                        role_name(roles, roleid)
                    ),
                ));
            }
        } else if !has_privs_of_role(roles, current, grantor) {
            return Err(denied(
                &format!("permission denied to revoke privileges granted by role \"{name}\""),
                format!(
                    "Only roles with privileges of role \"{name}\" may revoke privileges granted by this role."
                ),
            ));
        }
        Ok(grantor)
    }

    /// The memberships in `roleid` that a statement can change, in the order of the index of `pg_auth_members` on the role, the member and the grantor.
    fn membership_list(roles: &Roles, roleid: u32) -> Vec<Member> {
        let mut list: Vec<Member> =
            roles.members.iter().filter(|m| m.role == roleid).copied().collect();
        list.sort_by_key(|m| (m.member, m.grantor));
        list
    }

    /// `AddRoleMems`.
    #[allow(clippy::too_many_arguments)]
    fn add_members(
        &mut self,
        work: &mut Roles,
        current: u32,
        rolename: &str,
        roleid: u32,
        members: &[(String, u32)],
        grantor: Option<u32>,
        options: GrantOptions,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let grantor = self.role_grantor(work, current, roleid, grantor, true)?;
        for (name, member) in members {
            if *member == PG_DATABASE_OWNER {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("role \"{name}\" cannot be a member of any role"),
                ));
            }
            if is_member_nosuper(work, roleid, *member) {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    format!("role \"{rolename}\" is a member of role \"{name}\""),
                ));
            }
        }
        if options.admin == Some(true) && grantor != BOOTSTRAP_SUPERUSER {
            let back = || {
                Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "ADMIN option cannot be granted back to your own grantor",
                )
            };
            let list = Connection::membership_list(work, roleid);
            let mut actions = vec![Action::Noop; list.len()];
            for (_, member) in members {
                if *member == BOOTSTRAP_SUPERUSER {
                    return Err(back());
                }
                for i in 0..list.len() {
                    if list[i].member == *member {
                        plan_recursive_revoke(&list, &mut actions, i, false, true)?;
                    }
                }
            }
            let kept = list
                .iter()
                .zip(&actions)
                .any(|(m, &a)| a == Action::Noop && m.member == grantor && m.admin);
            if !kept {
                return Err(back());
            }
        }
        for (name, member) in members {
            let found = work
                .members
                .iter_mut()
                .find(|m| m.role == roleid && m.member == *member && m.grantor == grantor);
            if let Some(row) = found {
                let mut changed = false;
                for (given, slot) in [
                    (options.admin, &mut row.admin),
                    (options.inherit, &mut row.inherit),
                    (options.set, &mut row.set),
                ] {
                    if let Some(value) = given
                        && *slot != value
                    {
                        *slot = value;
                        changed = true;
                    }
                }
                if !changed {
                    notices.push(notice(format!(
                        "role \"{name}\" has already been granted membership in role \"{rolename}\" by role \"{}\"",
                        role_name(work, grantor)
                    )));
                }
                continue;
            }
            let inherit = match options.inherit {
                Some(inherit) => inherit,
                None => work.attrs(*member).map(|a| a.inherit).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for role {member}"))
                })?,
            };
            let oid = self.role_oid_new();
            work.members.push(Member {
                oid,
                role: roleid,
                member: *member,
                grantor,
                admin: options.admin.unwrap_or(false),
                inherit,
                set: options.set.unwrap_or(true),
            });
        }
        Ok(())
    }

    /// `DelRoleMems`.
    #[allow(clippy::too_many_arguments)]
    fn del_members(
        &self,
        work: &mut Roles,
        current: u32,
        rolename: &str,
        roleid: u32,
        members: &[(String, u32)],
        grantor: Option<u32>,
        options: GrantOptions,
        cascade: bool,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let grantor = self.role_grantor(work, current, roleid, grantor, false)?;
        let list = Connection::membership_list(work, roleid);
        let mut actions = vec![Action::Noop; list.len()];
        for (name, member) in members {
            // `plan_single_revoke`.
            let Some(i) = list.iter().position(|m| m.member == *member && m.grantor == grantor)
            else {
                notices.push(Notice::warning(Error::new(
                    SqlState::WARNING,
                    format!(
                        "role \"{name}\" has not been granted membership in role \"{rolename}\" by role \"{}\"",
                        role_name(work, grantor)
                    ),
                )));
                continue;
            };
            if options.inherit.is_some() {
                actions[i] = Action::RemoveInherit;
            } else if options.set.is_some() {
                actions[i] = Action::RemoveSet;
            } else {
                plan_recursive_revoke(&list, &mut actions, i, options.admin.is_some(), cascade)?;
            }
        }
        for (row, action) in list.iter().zip(actions) {
            if action == Action::Delete {
                work.members.retain(|m| m.oid != row.oid);
                continue;
            }
            let Some(m) = work.members.iter_mut().find(|m| m.oid == row.oid) else { continue };
            match action {
                Action::RemoveAdmin => m.admin = false,
                Action::RemoveInherit => m.inherit = false,
                Action::RemoveSet => m.set = false,
                Action::Noop | Action::Delete => {}
            }
        }
        Ok(())
    }
}

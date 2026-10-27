//! The roles that `CREATE ROLE` made and the memberships that `GRANT` made, with the lookups that see them together with the roles of `initdb`. The store of the server keeps them, and a session gives them to the thread of a statement with [`use_roles`], as it gives its types with [`crate::builtin::use_session_types`].

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::builtin::{self, MemberRow};

/// `BOOTSTRAP_SUPERUSERID`.
const BOOTSTRAP_SUPERUSER: u32 = 10;
/// `ROLE_PG_DATABASE_OWNER`.
const PG_DATABASE_OWNER: u32 = 6171;

/// A row of `pg_authid` that `CREATE ROLE` made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub oid: u32,
    pub name: String,
    pub attrs: Attrs,
    /// `rolconnlimit`, or -1 for no limit.
    pub connlimit: i32,
    /// `rolpassword`: the stored secret of the password.
    pub password: Option<String>,
    /// `rolvaliduntil`, in microseconds since 2000-01-01 UTC.
    pub valid_until: Option<i64>,
}

/// The attributes of a role that are true or false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    /// `rolsuper`.
    pub superuser: bool,
    /// `rolinherit`.
    pub inherit: bool,
    /// `rolcreaterole`.
    pub createrole: bool,
    /// `rolcreatedb`.
    pub createdb: bool,
    /// `rolcanlogin`.
    pub login: bool,
    /// `rolreplication`.
    pub replication: bool,
    /// `rolbypassrls`.
    pub bypassrls: bool,
}

impl Default for Attrs {
    /// The attributes of a new role: `INHERIT` and nothing else.
    fn default() -> Attrs {
        Attrs {
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            login: false,
            replication: false,
            bypassrls: false,
        }
    }
}

/// A row of `pg_auth_members` that `GRANT` made: `member` is a member of `role`, and `grantor` gave the membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub oid: u32,
    pub role: u32,
    pub member: u32,
    pub grantor: u32,
    /// `admin_option`.
    pub admin: bool,
    /// `inherit_option`.
    pub inherit: bool,
    /// `set_option`.
    pub set: bool,
}

/// A row of `pg_database` that `ALTER DATABASE ... OWNER TO` changed in a transaction: `datdba`, and `datacl` with the new owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseChange {
    pub oid: u32,
    pub owner: u32,
    pub acl: Option<Vec<String>>,
}

/// The roles and the memberships that the statements made, in the order that they were made, and the rows of `pg_database` that the transaction changed. The commit of the transaction moves the changes of the rows to the store, so the roles of a commit have no changes of rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub roles: Vec<Role>,
    pub members: Vec<Member>,
    pub databases: Vec<DatabaseChange>,
}

thread_local! {
    /// A session runs each statement on one thread, so the roles that the statement sees are the roles of the thread.
    static CURRENT: RefCell<Arc<Roles>> = RefCell::new(Arc::default());
    /// The OID of the database of the session of the thread and its owner at the last commit. A thread with no session has the owner of the databases of `initdb`.
    static DATABASE: Cell<(u32, u32)> = const { Cell::new((0, BOOTSTRAP_SUPERUSER)) };
}

/// Gives the database of the session and its owner at the last commit to the thread, for [`Roles::database_owner`].
pub fn use_database(oid: u32, owner: u32) {
    DATABASE.set((oid, owner));
}

/// Gives the roles that a session sees to the thread, for [`current`].
pub fn use_roles(roles: Arc<Roles>) {
    CURRENT.set(roles);
}

/// The roles that the session of the thread gave with [`use_roles`]. A thread with no session has no roles of its own.
pub fn current() -> Arc<Roles> {
    CURRENT.with_borrow(Arc::clone)
}

impl Roles {
    /// `datdba` of the database of the session: the owner that the transaction gave it, or its owner at the last commit. This role is a member of `pg_database_owner`.
    pub fn database_owner(&self) -> u32 {
        let (oid, owner) = DATABASE.get();
        self.databases.iter().find(|d| d.oid == oid).map_or(owner, |d| d.owner)
    }

    /// The role that `CREATE ROLE` made with this OID.
    pub fn created(&self, oid: u32) -> Option<&Role> {
        self.roles.iter().find(|r| r.oid == oid)
    }

    /// `get_role_oid`: the OID of the role with this name.
    pub fn oid(&self, name: &str) -> Option<u32> {
        match builtin::roles().iter().find(|r| r.name == name) {
            Some(row) => Some(row.oid),
            None => self.roles.iter().find(|r| r.name == name).map(|r| r.oid),
        }
    }

    /// `GetUserNameFromId`: the name of the role with this OID.
    pub fn name(&self, oid: u32) -> Option<&str> {
        match builtin::roles().iter().find(|r| r.oid == oid) {
            Some(row) => Some(row.name),
            None => self.created(oid).map(|r| r.name.as_str()),
        }
    }

    /// The attributes of the role with this OID.
    pub fn attrs(&self, oid: u32) -> Option<Attrs> {
        match builtin::roles().iter().find(|r| r.oid == oid) {
            Some(row) => Some(row.attrs),
            None => self.created(oid).map(|r| r.attrs),
        }
    }

    /// `superuser_arg`. A role that does not exist is not a superuser.
    pub fn superuser(&self, oid: u32) -> bool {
        self.attrs(oid).is_some_and(|a| a.superuser)
    }

    /// The memberships of `pg_auth_members`: the rows of `initdb`, then the rows that `GRANT` made.
    pub fn edges(&self) -> impl Iterator<Item = MemberRow> + '_ {
        builtin::members().iter().copied().chain(self.members.iter().map(|m| MemberRow {
            role: m.role,
            member: m.member,
            admin: m.admin,
            inherit: m.inherit,
            set: m.set,
        }))
    }

    /// `member_can_set_role`: a superuser can become any role, and another role can become a role that it reaches through memberships with `set_option`. The owner of the database of the session is a member of `pg_database_owner`.
    pub fn can_set_role(&self, member: u32, role: u32) -> bool {
        if self.superuser(member) {
            return true;
        }
        let owner = self.database_owner();
        let mut reached = vec![member];
        let mut at = 0;
        while let Some(&next) = reached.get(at) {
            at += 1;
            for edge in self.edges().filter(|m| m.member == next && m.set) {
                if !reached.contains(&edge.role) {
                    reached.push(edge.role);
                }
            }
            if next == owner && !reached.contains(&PG_DATABASE_OWNER) {
                reached.push(PG_DATABASE_OWNER);
            }
        }
        reached.contains(&role)
    }

    /// `roles_is_member_of` with `ROLERECURSE_PRIVS`: `role` and the roles that it reaches through memberships with `inherit_option`, in the order of the walk. The owner of the database of the session reaches `pg_database_owner`.
    pub fn privs_of(&self, role: u32) -> Vec<u32> {
        let owner = self.database_owner();
        let mut reached = vec![role];
        let mut at = 0;
        while let Some(&next) = reached.get(at) {
            at += 1;
            for edge in self.edges().filter(|m| m.member == next && m.inherit) {
                if !reached.contains(&edge.role) {
                    reached.push(edge.role);
                }
            }
            if next == owner && !reached.contains(&PG_DATABASE_OWNER) {
                reached.push(PG_DATABASE_OWNER);
            }
        }
        reached
    }

    /// `has_privs_of_role`: true when `member` is `role`, is a superuser, or reaches `role` through memberships with `inherit_option`.
    pub fn has_privs_of_role(&self, member: u32, role: u32) -> bool {
        member == role || self.superuser(member) || self.privs_of(member).contains(&role)
    }

    /// The names and the OIDs of all the roles, those of `initdb` first.
    pub fn all(&self) -> impl Iterator<Item = (&str, u32)> + '_ {
        let created = self.roles.iter().map(|r| (r.name.as_str(), r.oid));
        builtin::roles().iter().map(|r| (r.name, r.oid)).chain(created)
    }
}

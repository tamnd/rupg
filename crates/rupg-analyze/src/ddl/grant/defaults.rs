//! `ALTER DEFAULT PRIVILEGES` and the privileges of new objects, as `ExecAlterDefaultPrivilegesStmt`, `SetDefaultACL` and `get_user_default_acl` of `aclchk.c` run them. An entry for all the schemas takes the place of the hard-wired default ACL, and the entry of a schema adds to it.

use rupg_catalog::{ObjRef, PG_CLASS, PG_NAMESPACE, PG_PROC, PG_TYPE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::roles::Roles;
use rupg_sql::nodes::{
    AlterDefaultPrivilegesStmt, DefElem, DropBehavior, Node, ObjectType, RoleSpecType,
};

use super::super::Definer;
use super::super::sequence::conflicting;
use super::{
    ALL_FUNCTION, ALL_RELATION, ALL_SCHEMA, ALL_SEQUENCE, ALL_TYPE, AclItem, Grant, Kind, Object,
    PUBLIC, SELECT, UPDATE, acl_texts, parse_acl, privilege_to_string, string_to_privilege,
};
use crate::typename::names;

/// `ACL_ALL_RIGHTS_LARGEOBJECT`.
const ALL_LARGEOBJECT: u64 = SELECT | UPDATE;

/// `DEFACLOBJ_RELATION`.
const RELATION: u8 = b'r';
/// `DEFACLOBJ_SEQUENCE`.
const SEQUENCE: u8 = b'S';
/// `DEFACLOBJ_FUNCTION`.
const FUNCTION: u8 = b'f';
/// `DEFACLOBJ_TYPE`.
const TYPE: u8 = b'T';
/// `DEFACLOBJ_NAMESPACE`.
const NAMESPACE: u8 = b'n';
/// `DEFACLOBJ_LARGEOBJECT`.
const LARGEOBJECT: u8 = b'L';

/// The kind of the objects of an entry, or `None` for large objects.
fn kind_of(objtype: u8) -> Option<Kind> {
    match objtype {
        RELATION => Some(Kind::Table),
        SEQUENCE => Some(Kind::Sequence),
        FUNCTION => Some(Kind::Function),
        TYPE => Some(Kind::Type),
        NAMESPACE => Some(Kind::Schema),
        _ => None,
    }
}

/// `acldefault` for the objects of an entry.
fn hardwired(objtype: u8, owner: u32) -> Vec<AclItem> {
    match kind_of(objtype) {
        Some(kind) => kind.default_acl(owner),
        None => vec![AclItem { grantee: owner, grantor: owner, privs: ALL_LARGEOBJECT }],
    }
}

/// `aclitemsort`: the items in the order of the grantee, the grantor and the privileges.
fn sorted(mut acl: Vec<AclItem>) -> Vec<AclItem> {
    acl.sort_by_key(|item| (item.grantee, item.grantor, item.privs));
    acl
}

/// `aclmerge`: the items of `right` added to `left`, or `None` when both are empty.
fn aclmerge(
    roles: &Roles,
    left: Vec<AclItem>,
    right: &[AclItem],
    owner: u32,
) -> Result<Option<Vec<AclItem>>> {
    if left.is_empty() && right.is_empty() {
        return Ok(None);
    }
    let mut acl = left;
    for &item in right {
        acl = super::aclupdate(roles, &acl, item, true, owner, false)?;
    }
    Ok(Some(acl))
}

/// The value of an option of the statement.
fn option_list(option: &DefElem) -> &[Option<Node>] {
    match &option.arg {
        Some(Node::List(list)) => list.as_slice(),
        _ => &[],
    }
}

impl Definer<'_, '_> {
    /// `ExecAlterDefaultPrivilegesStmt`: the grantees, the privileges and the roles of the statement, then `SetDefaultACLsInSchemas` for each role.
    pub(in crate::ddl) fn alter_default_privileges(
        &mut self,
        stmt: &AlterDefaultPrivilegesStmt,
    ) -> Result<()> {
        let mut schemas: Option<&DefElem> = None;
        let mut for_roles: Option<&DefElem> = None;
        for option in stmt.options.iter().flatten() {
            let Node::DefElem(option) = option else { return Err(Error::internal("an option")) };
            let slot = match option.defname.as_deref() {
                Some("schemas") => &mut schemas,
                Some("roles") => &mut for_roles,
                name => {
                    return Err(Error::internal(format!(
                        "option \"{}\" not recognized",
                        name.unwrap_or_default()
                    )));
                }
            };
            if slot.is_some() {
                return Err(conflicting(option));
            }
            *slot = Some(option);
        }
        let Some(action) = stmt.action.as_deref() else {
            return Err(Error::internal("a GRANT statement"));
        };
        let mut grantees = Vec::new();
        for role in action.grantees.iter().flatten() {
            let Node::RoleSpec(role) = role else { return Err(Error::internal("a role")) };
            grantees.push(if role.roletype == RoleSpecType::ROLESPEC_PUBLIC {
                PUBLIC
            } else {
                self.role_oid(role)?
            });
        }
        let (objtype, all, word) = match action.objtype {
            ObjectType::OBJECT_TABLE => (RELATION, ALL_RELATION, "relation"),
            ObjectType::OBJECT_SEQUENCE => (SEQUENCE, ALL_SEQUENCE, "sequence"),
            ObjectType::OBJECT_FUNCTION => (FUNCTION, ALL_FUNCTION, "function"),
            ObjectType::OBJECT_PROCEDURE => (FUNCTION, ALL_FUNCTION, "procedure"),
            ObjectType::OBJECT_ROUTINE => (FUNCTION, ALL_FUNCTION, "routine"),
            ObjectType::OBJECT_TYPE => (TYPE, ALL_TYPE, "type"),
            ObjectType::OBJECT_SCHEMA => (NAMESPACE, ALL_SCHEMA, "schema"),
            ObjectType::OBJECT_LARGEOBJECT => (LARGEOBJECT, ALL_LARGEOBJECT, "large object"),
            other => {
                return Err(Error::internal(format!(
                    "unrecognized GrantStmt.objtype: {}",
                    other.0
                )));
            }
        };
        let mut privileges = 0;
        for privilege in action.privileges.iter().flatten() {
            let Node::AccessPriv(privilege) = privilege else {
                return Err(Error::internal("a privilege"));
            };
            if !privilege.cols.is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    "default privileges cannot be set for columns",
                ));
            }
            let Some(name) = privilege.priv_name.as_deref() else {
                return Err(Error::internal("AccessPriv node must specify privilege"));
            };
            let bits = string_to_privilege(name)?;
            if bits & !all != 0 {
                return Err(Error::new(
                    SqlState::INVALID_GRANT_OPERATION,
                    format!("invalid privilege type {} for {word}", privilege_to_string(bits)),
                ));
            }
            privileges |= bits;
        }
        let grant = Grant {
            is_grant: action.is_grant,
            objtype: action.objtype,
            all_privs: action.privileges.is_empty(),
            privileges,
            col_privs: Vec::new(),
            grantees,
            grant_option: action.grant_option,
            cascade: action.behavior == DropBehavior::DROP_CASCADE,
            grantor: None,
        };
        let schemas = schemas.map(option_list).unwrap_or_default();
        let Some(for_roles) = for_roles else {
            return self.set_default_acls_in_schemas(&grant, objtype, self.user, schemas);
        };
        for role in option_list(for_roles).iter().flatten() {
            let Node::RoleSpec(role) = role else { return Err(Error::internal("a role")) };
            let role = self.role_oid(role)?;
            if !self.owns(role) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied to change default privileges",
                ));
            }
            self.set_default_acls_in_schemas(&grant, objtype, role, schemas)?;
        }
        Ok(())
    }

    /// `SetDefaultACLsInSchemas`: the entry for all the schemas when the statement names none, or the entry of each schema that it names.
    fn set_default_acls_in_schemas(
        &mut self,
        grant: &Grant<'_>,
        objtype: u8,
        role: u32,
        schemas: &[Option<Node>],
    ) -> Result<()> {
        if schemas.is_empty() {
            return self.set_default_acl(grant, objtype, role, 0);
        }
        for name in names(schemas) {
            let namespace = self.an.schema(name).ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{name}\" does not exist"))
            })?;
            self.set_default_acl(grant, objtype, role, namespace)?;
        }
        Ok(())
    }

    /// `SetDefaultACL`: the new ACL of the entry, with the role as the grantor. An entry with the default value goes away. The default value of the entry for all the schemas is the hard-wired default ACL, and the default value of the entry of a schema is empty.
    fn set_default_acl(
        &mut self,
        grant: &Grant<'_>,
        objtype: u8,
        role: u32,
        namespace: u32,
    ) -> Result<()> {
        if namespace != 0 && matches!(objtype, NAMESPACE | LARGEOBJECT) {
            let what = if objtype == NAMESPACE {
                "GRANT/REVOKE ON SCHEMAS"
            } else {
                "GRANT/REVOKE ON LARGE OBJECTS"
            };
            return Err(Error::new(
                SqlState::INVALID_GRANT_OPERATION,
                format!("cannot use IN SCHEMA clause when using {what}"),
            ));
        }
        let roles = rupg_pgcatalog::roles::current();
        let default = if namespace == 0 { hardwired(objtype, role) } else { Vec::new() };
        let old = match self.catalog.find_default_acl(role, namespace, objtype) {
            Some(entry) => parse_acl(&roles, &entry.acl),
            None => default.clone(),
        };
        let privileges = match (grant.all_privs, kind_of(objtype)) {
            (false, _) => grant.privileges,
            (true, Some(kind)) => kind.whole(),
            (true, None) => ALL_LARGEOBJECT,
        };
        let new = sorted(Self::merge(&roles, grant, &old, privileges, role, role)?);
        let acl = (new != sorted(default)).then(|| acl_texts(&roles, &new));
        self.catalog.set_default_acl(role, namespace, objtype, acl);
        Ok(())
    }

    /// `get_user_default_acl` for a new object: the ACL that the default privileges of its owner give it. The object keeps a null ACL when the result is the hard-wired default ACL. An index, a TOAST table and the relation of a composite type have no default privileges. The callers do not give the row type of a relation, an array type or a multirange type, as `TypeCreate` does not for a dependent type.
    pub(in crate::ddl) fn apply_default_acl(&mut self, object: ObjRef) -> Result<()> {
        let oid = object.oid;
        let found = match object.class {
            PG_CLASS => self.catalog.relation(oid).and_then(|r| {
                let objtype = match r.kind {
                    RelKind::Table | RelKind::View | RelKind::Partitioned | RelKind::MatView => {
                        RELATION
                    }
                    RelKind::Sequence => SEQUENCE,
                    _ => return None,
                };
                Some((objtype, r.owner, r.namespace))
            }),
            PG_TYPE => self.catalog.type_by_oid(oid).map(|t| (TYPE, t.owner, t.namespace)),
            PG_PROC => self.catalog.function(oid).map(|f| (FUNCTION, f.owner, f.namespace)),
            PG_NAMESPACE => self.catalog.schema(oid).map(|s| (NAMESPACE, s.owner, 0)),
            _ => None,
        };
        let Some((objtype, owner, namespace)) = found else { return Ok(()) };
        let roles = rupg_pgcatalog::roles::current();
        let entry = |namespace: u32| {
            self.catalog
                .find_default_acl(owner, namespace, objtype)
                .map(|d| parse_acl(&roles, &d.acl))
        };
        let global = entry(0);
        let schema = if namespace == 0 { None } else { entry(namespace) };
        if global.is_none() && schema.is_none() {
            return Ok(());
        }
        let default = hardwired(objtype, owner);
        let global = global.unwrap_or_else(|| default.clone());
        let Some(acl) = aclmerge(&roles, global, schema.as_deref().unwrap_or_default(), owner)?
        else {
            return Ok(());
        };
        let acl = sorted(acl);
        let Some(kind) = kind_of(objtype) else { return Ok(()) };
        if acl == sorted(default) {
            return Ok(());
        }
        self.write_acl(&roles, Object::new(kind, oid), &acl)
    }

    /// `RemoveRoleFromObjectACL` for an entry of `pg_default_acl`: `SetDefaultACL` with the revoke of all the privileges of the role.
    pub(super) fn remove_role_from_default_acl(&mut self, role: u32, oid: u32) -> Result<()> {
        let Some(entry) = self.catalog.default_acl(oid) else { return Ok(()) };
        let (owner, namespace, objtype) = (entry.role, entry.namespace, entry.objtype);
        let grant = Grant {
            is_grant: false,
            objtype: ObjectType::OBJECT_TABLE,
            all_privs: true,
            privileges: 0,
            col_privs: Vec::new(),
            grantees: vec![role],
            grant_option: false,
            cascade: true,
            grantor: None,
        };
        self.set_default_acl(&grant, objtype, owner, namespace)
    }
}

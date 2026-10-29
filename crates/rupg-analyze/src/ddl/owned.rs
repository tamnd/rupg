//! `DROP OWNED` and `REASSIGN OWNED`, as `DropOwnedObjects`, `ReassignOwnedObjects`, `shdepDropOwned` and `shdepReassignOwned` run them. rupg keeps no `pg_shdepend`, so the objects of a role are the objects with the role as their owner and the objects with an ACL that names the role.

use rupg_catalog::{
    Catalog, ObjRef, PG_CLASS, PG_COLLATION, PG_CONVERSION, PG_DATABASE, PG_DEFAULT_ACL,
    PG_EVENT_TRIGGER, PG_EXTENSION, PG_FOREIGN_DATA_WRAPPER, PG_FOREIGN_SERVER, PG_LANGUAGE,
    PG_LARGEOBJECT, PG_NAMESPACE, PG_OPCLASS, PG_OPERATOR, PG_OPFAMILY, PG_PROC, PG_PUBLICATION,
    PG_STATISTIC_EXT, PG_TS_CONFIG, PG_TS_DICT, PG_TYPE, PG_USER_MAPPING, RangePart, RelKind,
};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{DropBehavior, DropOwnedStmt, List, Node, ReassignOwnedStmt};

use super::Definer;

/// `FirstUnpinnedObjectId`: `IsPinnedObject` is true for a role of `initdb` with a smaller OID.
const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;

/// The objects of a catalog that have a dependency of `pg_shdepend` on their owner `role`, as `recordDependencyOnOwner` makes them. An index, a TOAST table, a composite type as a relation, the row type of a table, an array type and a multirange type have none.
pub fn owned_objects(catalog: &Catalog, role: u32) -> Vec<ObjRef> {
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
    for s in catalog.stat_exts() {
        add(PG_STATISTIC_EXT, s.oid, s.owner);
    }
    for c in catalog.conversions() {
        add(PG_CONVERSION, c.oid, c.owner);
    }
    for t in catalog.event_triggers() {
        add(PG_EVENT_TRIGGER, t.oid, t.owner);
    }
    for l in catalog.large_objects() {
        add(PG_LARGEOBJECT, l.oid, l.owner);
    }
    for f in catalog.fdws() {
        add(PG_FOREIGN_DATA_WRAPPER, f.oid, f.owner);
    }
    for s in catalog.servers() {
        add(PG_FOREIGN_SERVER, s.oid, s.owner);
    }
    for p in catalog.publications() {
        add(PG_PUBLICATION, p.oid, p.owner);
    }
    for m in catalog.user_mappings() {
        add(PG_USER_MAPPING, m.oid, m.user);
    }
    for o in catalog.operators() {
        add(PG_OPERATOR, o.oid, o.owner);
    }
    for c in catalog.opclasses() {
        add(PG_OPCLASS, c.oid, c.owner);
    }
    for f in catalog.opfamilies() {
        add(PG_OPFAMILY, f.oid, f.owner);
    }
    for d in catalog.default_acls() {
        add(PG_DEFAULT_ACL, d.oid, d.role);
    }
    objects
}

/// The policies that name the role in `polroles`, as `CreatePolicy` records them in `pg_shdepend` with `SHARED_DEPENDENCY_POLICY`.
pub fn policy_targets(catalog: &Catalog, role: u32) -> Vec<ObjRef> {
    catalog
        .policies()
        .filter(|p| p.roles.contains(&role))
        .map(|p| ObjRef::new(rupg_catalog::PG_POLICY, p.oid))
        .collect()
}

/// The objects with an ACL that names the role other than as their owner, as `updateAclDependencies` records them in `pg_shdepend`. The ACL of a column gives an object with the number of the column.
pub fn acl_dependents(catalog: &Catalog, role: u32) -> Vec<ObjRef> {
    let names = |acl: &[String]| super::acl_roles(acl).contains(&role);
    let mut objects = Vec::new();
    for s in catalog.schemas() {
        if s.owner != role && s.acl.as_deref().is_some_and(names) {
            objects.push(ObjRef::new(PG_NAMESPACE, s.oid));
        }
    }
    for r in catalog.relations() {
        if r.owner != role && r.acl.as_deref().is_some_and(names) {
            objects.push(ObjRef::new(PG_CLASS, r.oid));
        }
    }
    for (object, acl) in catalog.object_acls() {
        if acl_owner(catalog, object) != role && names(acl) {
            objects.push(object);
        }
    }
    for d in catalog.default_acls() {
        if d.role != role && names(&d.acl) {
            objects.push(ObjRef::new(PG_DEFAULT_ACL, d.oid));
        }
    }
    objects
}

/// The owner of an object that `GRANT` gave an ACL to. A built-in object has the bootstrap superuser as its owner.
fn acl_owner(catalog: &Catalog, object: ObjRef) -> u32 {
    let owner = match object.class {
        PG_NAMESPACE => catalog.schema(object.oid).map(|s| s.owner),
        PG_CLASS => catalog.relation(object.oid).map(|r| r.owner),
        PG_TYPE => catalog.type_by_oid(object.oid).map(|t| t.owner),
        PG_PROC => catalog.function(object.oid).map(|f| f.owner),
        PG_LANGUAGE => catalog.languages().find(|l| l.oid == object.oid).map(|l| l.owner),
        PG_FOREIGN_DATA_WRAPPER => catalog.fdw(object.oid).map(|f| f.owner),
        PG_FOREIGN_SERVER => catalog.server(object.oid).map(|s| s.owner),
        PG_PUBLICATION => catalog.publication(object.oid).map(|p| p.owner),
        _ => None,
    };
    owner.unwrap_or(rupg_catalog::BOOTSTRAP_SUPERUSER)
}

/// The name of a role.
fn role_name(oid: u32) -> String {
    rupg_pgcatalog::roles::current().name(oid).unwrap_or_default().to_string()
}

impl Definer<'_, '_> {
    /// `DropOwnedObjects` and `shdepDropOwned`: the checks of the privileges, the revoke of the privileges of each role, and then `performMultipleDeletions` of the objects that the roles own. The caller removes the grants of membership that the roles made.
    pub(super) fn drop_owned(&mut self, stmt: &DropOwnedStmt) -> Result<()> {
        let roles = self.role_ids(&stmt.roles)?;
        for &role in &roles {
            if !self.owns(role) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied to drop objects",
                )
                .with_detail(format!(
                    "Only roles with privileges of role \"{}\" may drop objects owned by it.",
                    role_name(role)
                )));
            }
        }
        let mut doomed: Vec<ObjRef> = Vec::new();
        for &role in &roles {
            if role < FIRST_UNPINNED_OBJECT_ID {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop objects owned by role {} because they are required by the database system",
                        role_name(role)
                    ),
                ));
            }
            let mut revoked: Vec<ObjRef> = Vec::new();
            for object in acl_dependents(self.catalog, role) {
                let object = ObjRef::new(object.class, object.oid);
                if !revoked.contains(&object) {
                    revoked.push(object);
                }
            }
            // The ACL of a database is a shared object, which `shdepDropOwned` revokes from each database.
            for db in self.an.env.databases().iter() {
                let names =
                    db.acl.as_deref().is_some_and(|acl| super::acl_roles(acl).contains(&role));
                if db.owner != role && names {
                    revoked.push(ObjRef::new(PG_DATABASE, db.oid));
                }
            }
            revoked.sort_by_key(|o| o.oid);
            for object in revoked {
                self.remove_role_from_acl(role, object)?;
            }
            self.remove_role_from_policies(role, &mut doomed);
            let mut owned = owned_objects(self.catalog, role);
            owned.sort_by_key(|o| o.oid);
            for object in owned {
                if !doomed.contains(&object) {
                    doomed.push(object);
                }
            }
        }
        // The revoke of an entry of `pg_default_acl` can remove the entry.
        doomed.retain(|o| o.class != PG_DEFAULT_ACL || self.catalog.default_acl(o.oid).is_some());
        if !doomed.is_empty() {
            self.perform(&doomed, stmt.behavior == DropBehavior::DROP_CASCADE)?;
        }
        self.grantors = roles;
        Ok(())
    }

    /// `ReassignOwnedObjects` and `shdepReassignOwned`: the checks of the privileges, then the new owner of each object that the roles own, as `shdepReassignOwned_Owner` changes it.
    pub(super) fn reassign_owned(&mut self, stmt: &ReassignOwnedStmt) -> Result<()> {
        let roles = self.role_ids(&stmt.roles)?;
        let denied = |detail: String| {
            Error::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to reassign objects")
                .with_detail(detail)
        };
        for &role in &roles {
            if !self.owns(role) {
                return Err(denied(format!(
                    "Only roles with privileges of role \"{}\" may reassign objects owned by it.",
                    role_name(role)
                )));
            }
        }
        let spec = stmt.newrole.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&spec)?;
        if !self.owns(new) {
            return Err(denied(format!(
                "Only roles with privileges of role \"{}\" may reassign objects to it.",
                role_name(new)
            )));
        }
        for &role in &roles {
            if role < FIRST_UNPINNED_OBJECT_ID {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot reassign ownership of objects owned by role {} because they are required by the database system",
                        role_name(role)
                    ),
                ));
            }
            let mut owned = owned_objects(self.catalog, role);
            owned.sort_by_key(|o| o.oid);
            for object in owned {
                self.reassign(object, role, new)?;
            }
        }
        Ok(())
    }

    /// `shdepReassignOwned_Owner` for one object. `ATExecChangeOwner` with `recursing` changes a relation, so the indexes and the sequences of a table go with it, and an object that went with an earlier one keeps its new owner. A role that is not a superuser needs the checks of `AlterSchemaOwner_internal` for a schema and of `AlterObjectOwner_internal` for the other objects of `ALTER ... OWNER TO`.
    fn reassign(&mut self, object: ObjRef, old: u32, new: u32) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let (old_name, new_name) = (role_name(old), role_name(new));
        match object.class {
            PG_CLASS => {
                if self.catalog.relation(object.oid).is_some_and(|r| r.owner == old) {
                    self.change_owner(object.oid, new, true)?;
                }
                Ok(())
            }
            PG_NAMESPACE => {
                if !self.is_superuser() {
                    self.check_can_set_role(new)?;
                    if !self.database_create_allowed(self.user) {
                        return Err(Error::new(
                            SqlState::INSUFFICIENT_PRIVILEGE,
                            format!("permission denied for database {}", self.an.env.database()),
                        ));
                    }
                }
                self.catalog.set_schema_owner(object.oid, new, &old_name, &new_name)
            }
            // `DROP OWNED` removes the default privileges and the user mappings, and `REASSIGN OWNED` leaves them.
            PG_DEFAULT_ACL | PG_USER_MAPPING => Ok(()),
            PG_TYPE => {
                let Some(ty) = self.catalog.type_by_oid(object.oid) else { return Ok(()) };
                if ty.owner != old {
                    return Ok(());
                }
                let composite = self
                    .catalog
                    .relation(ty.relation)
                    .filter(|r| r.kind == RelKind::Composite)
                    .map(|r| r.oid);
                if let Some(relation) = composite {
                    return self.change_owner(relation, new, true);
                }
                self.catalog.set_type_owner(object.oid, new)?;
                self.catalog.set_object_owner(object, new, &old_name, &new_name)
            }
            _ => {
                if !self.is_superuser() {
                    self.check_can_set_role(new)?;
                    if let Some(namespace) = self.object_namespace(object)
                        && !self.may_create_in(namespace, new)
                    {
                        return Err(Error::new(
                            SqlState::INSUFFICIENT_PRIVILEGE,
                            format!("permission denied for schema {}", self.schema_name(namespace)),
                        ));
                    }
                }
                self.catalog.set_object_owner(object, new, &old_name, &new_name)
            }
        }
    }

    /// The schema of an object of `AlterObjectOwner_internal`, for the check of `CREATE` on it, or `None` for an object that is not in a schema.
    fn object_namespace(&self, object: ObjRef) -> Option<u32> {
        let catalog = &*self.catalog;
        let oid = object.oid;
        match object.class {
            PG_PROC => catalog.function(oid).map(|f| f.namespace),
            PG_COLLATION => catalog.collations().find(|c| c.oid == oid).map(|c| c.namespace),
            PG_OPERATOR => catalog.operators().find(|o| o.oid == oid).map(|o| o.namespace),
            PG_OPCLASS => catalog.opclasses().find(|c| c.oid == oid).map(|c| c.namespace),
            PG_OPFAMILY => catalog.opfamilies().find(|f| f.oid == oid).map(|f| f.namespace),
            PG_STATISTIC_EXT => catalog.stat_exts().find(|s| s.oid == oid).map(|s| s.namespace),
            PG_TS_DICT => catalog.ts_dictionaries().find(|d| d.oid == oid).map(|d| d.namespace),
            PG_TS_CONFIG => catalog.ts_configs().find(|c| c.oid == oid).map(|c| c.namespace),
            PG_CONVERSION => catalog.conversion(oid).map(|c| c.namespace),
            _ => None,
        }
    }

    /// `roleSpecsToIds`: the OIDs of the roles of the list, in its order. `PUBLIC` is not a role here.
    fn role_ids(&self, list: &List) -> Result<Vec<u32>> {
        let mut roles = Vec::new();
        for spec in list.iter().flatten() {
            let Node::RoleSpec(spec) = spec else { return Err(Error::internal("a role")) };
            roles.push(self.role_oid(spec)?);
        }
        Ok(roles)
    }
}

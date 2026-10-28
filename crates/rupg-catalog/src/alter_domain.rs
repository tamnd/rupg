//! `ALTER DOMAIN` with `SET DEFAULT`, `DROP DEFAULT`, `SET NOT NULL` and `DROP NOT NULL`: the changes of `AlterDomainDefault` and `AlterDomainNotNull` to the row of the domain in `pg_type`. The analyzer runs the checks and checks the rows.

use rupg_common::Result;

use super::{Catalog, DepKind, ObjRef, PG_COLLATION, PG_NAMESPACE, PG_TYPE, no_object};

impl Catalog {
    /// `AlterDomainDefault`: the new default of a domain in the form of the analyzer, or `None` for no default. `refs` are the objects that the default refers to. `GenerateTypeDependencies` then makes the dependencies of the domain again.
    ///
    /// # Errors
    ///
    /// An internal error for a domain that the catalog does not have.
    pub fn set_domain_default(
        &mut self,
        domain: u32,
        default: Option<String>,
        refs: &[ObjRef],
    ) -> Result<()> {
        let ty = self.types.get_mut(&domain).ok_or_else(|| no_object("type", domain))?;
        let namespace = ty.namespace;
        let Some(facts) = ty.domain.as_mut() else { return Err(no_object("domain", domain)) };
        facts.default = default;
        let (base, collation) = (facts.base, facts.collation);
        let me = ObjRef::new(PG_TYPE, domain);
        self.drop_depends_of(me);
        self.domain_depends(me, (base, namespace, collation), refs);
        Ok(())
    }

    /// Sets `typnotnull` of a domain, as `AlterDomainNotNull` and `AlterDomainDropConstraint` set it.
    ///
    /// # Errors
    ///
    /// An internal error for a domain that the catalog does not have.
    pub fn set_domain_not_null(&mut self, domain: u32, not_null: bool) -> Result<()> {
        let ty = self.types.get_mut(&domain).ok_or_else(|| no_object("type", domain))?;
        let Some(facts) = ty.domain.as_mut() else { return Err(no_object("domain", domain)) };
        facts.not_null = not_null;
        Ok(())
    }

    /// The dependencies of the domain `me` on its base type, its schema, its collation and the objects `refs` of its default, as `GenerateTypeDependencies` records them. `record_object_address_dependencies` sorts the objects by the OID of their catalog.
    pub(super) fn domain_depends(
        &mut self,
        me: ObjRef,
        (base, namespace, collation): (u32, u32, u32),
        refs: &[ObjRef],
    ) {
        self.depend(me, ObjRef::new(PG_TYPE, base), DepKind::Normal);
        self.depend(me, ObjRef::new(PG_NAMESPACE, namespace), DepKind::Normal);
        if collation != 0 {
            self.depend(me, ObjRef::new(PG_COLLATION, collation), DepKind::Normal);
        }
        for &referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
    }
}

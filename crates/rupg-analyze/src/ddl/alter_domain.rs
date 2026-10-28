//! `ALTER DOMAIN` with `SET DEFAULT`, `DROP DEFAULT`, `SET NOT NULL`, `DROP NOT NULL`, `ADD CONSTRAINT`, `DROP CONSTRAINT`, `VALIDATE CONSTRAINT` and `RENAME CONSTRAINT`, as `AlterDomainDefault`, `AlterDomainNotNull`, `AlterDomainAddConstraint`, `AlterDomainDropConstraint` and `AlterDomainValidateConstraint` of `typecmds.c` and `RenameConstraint` of `tablecmds.c` run them. The caller checks the rows of the tables of [`super::Defined::domain_checks`] against a new constraint.

use rupg_catalog::{ConKind, ObjRef, PG_CLASS, PG_CONSTRAINT, PG_TYPE, RangePart, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{AlterDomainStmt, AlterDomainType, ConstrType, DropBehavior, Node, TypeName};

use super::comment::not_owner;
use super::{Definer, DomainCheck, not_yet, references};
use crate::expr::Expr;
use crate::typename::type_name_text;

/// The depth of domains and of types that hold them that the search of the tables follows.
const MAX_DEPTH: usize = 32;

/// The domain of `ALTER DOMAIN`, as `checkDomainOwner` finds it.
struct Target {
    oid: u32,
    /// The name of the domain as the statement gives it, as `TypeNameToString` writes it.
    written: String,
    base: u32,
    typmod: i32,
    not_null: bool,
}

impl Definer<'_, '_> {
    /// `ALTER DOMAIN`: the subcommand of `ProcessUtilitySlow` for the statement.
    pub(super) fn alter_domain(&mut self, stmt: &AlterDomainStmt) -> Result<()> {
        let target = self.domain_target(&stmt.typeName)?;
        let name = stmt.name.as_deref().unwrap_or_default();
        match stmt.subtype {
            AlterDomainType::AD_AlterDefault => {
                self.alter_domain_default(&target, stmt.def.as_ref())
            }
            AlterDomainType::AD_DropNotNull => self.alter_domain_not_null(&target, false),
            AlterDomainType::AD_SetNotNull => self.alter_domain_not_null(&target, true),
            AlterDomainType::AD_AddConstraint => {
                self.alter_domain_add_constraint(&target, stmt.def.as_ref())
            }
            AlterDomainType::AD_DropConstraint => {
                let cascade = stmt.behavior == DropBehavior::DROP_CASCADE;
                self.alter_domain_drop_constraint(&target, name, cascade, stmt.missing_ok)
            }
            AlterDomainType::AD_ValidateConstraint => {
                self.alter_domain_validate_constraint(&target, name)
            }
            _ => {
                Err(Error::internal(format!("unrecognized alter domain type: {}", stmt.subtype.0)))
            }
        }
    }

    /// `RenameConstraint` and `rename_constraint_internal` for a constraint of a domain: `ALTER DOMAIN ... RENAME CONSTRAINT`. `RenameConstraintById` does not allow a name that the domain already has.
    pub(super) fn rename_domain_constraint(
        &mut self,
        object: Option<&Node>,
        name: &str,
        newname: &str,
    ) -> Result<()> {
        let names = match object {
            Some(Node::List(list)) => list.clone(),
            _ => Vec::new(),
        };
        let target = self.domain_target(&names)?;
        let named = |n: &str| {
            self.catalog.constraints_of_domain(target.oid).find(|c| c.name == n).map(|c| c.oid)
        };
        let Some(oid) = named(name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "constraint \"{name}\" for domain {} does not exist",
                    self.type_text(target.oid)
                ),
            ));
        };
        if named(newname).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!(
                    "constraint \"{newname}\" for domain {} already exists",
                    self.type_text(target.oid)
                ),
            ));
        }
        self.catalog.rename_constraint(oid, newname)
    }

    /// `typenameTypeId` and `checkDomainOwner`: the type must be a domain that the user owns.
    fn domain_target(&self, names: &[Option<Node>]) -> Result<Target> {
        let name = TypeName { names: names.to_vec(), ..TypeName::default() };
        let written = type_name_text(&name);
        let oid = self.an.lookup_type_name(&name)?.ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_OBJECT, format!("type \"{written}\" does not exist"))
        })?;
        let not_domain = || {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{} is not a domain", self.type_text(oid)),
            )
        };
        let Some(ty) = self.catalog.type_by_oid(oid) else {
            if builtin::domain_by_oid(oid).is_some() {
                return Err(not_yet("ALTER DOMAIN of a built-in domain", None));
            }
            return Err(not_domain());
        };
        let Some(domain) = &ty.domain else { return Err(not_domain()) };
        if !self.is_superuser() && !self.owns(ty.owner) {
            return Err(not_owner("type", &self.type_text(oid)));
        }
        Ok(Target {
            oid,
            written,
            base: domain.base,
            typmod: domain.typmod,
            not_null: domain.not_null,
        })
    }

    /// `AlterDomainDefault`: the new default, or no default for `DROP DEFAULT` and for a default that is a null constant.
    fn alter_domain_default(&mut self, target: &Target, raw: Option<&Node>) -> Result<()> {
        let name =
            self.catalog.type_by_oid(target.oid).map_or_else(String::new, |t| t.name.clone());
        let expr = self.domain_default(raw, (target.base, target.typmod), &name)?;
        let (default, refs) = match expr {
            Some(expr) => {
                let refs = references(self.catalog, 0, &[&expr])?;
                self.check_usage_on_types(&refs)?;
                (Some(crate::node::write(&expr)?), refs)
            }
            None => (None, Vec::new()),
        };
        self.catalog.set_domain_default(target.oid, default, &refs)
    }

    /// `AlterDomainNotNull`: `SET NOT NULL` adds the not-null constraint and checks the rows, and `DROP NOT NULL` drops it. Nothing changes when the domain already has the state.
    fn alter_domain_not_null(&mut self, target: &Target, not_null: bool) -> Result<()> {
        if target.not_null == not_null {
            return Ok(());
        }
        if not_null {
            self.catalog.add_domain_not_null(target.oid, None)?;
            self.validate_domain(target.oid, None)?;
        } else {
            let found = self
                .catalog
                .constraints_of_domain(target.oid)
                .find(|c| c.kind == ConKind::NotNull)
                .map(|c| c.oid);
            let Some(oid) = found else {
                let name = self.catalog.type_by_oid(target.oid).map_or("", |t| t.name.as_str());
                return Err(Error::internal(format!(
                    "could not find not-null constraint on domain \"{name}\""
                )));
            };
            self.perform(&[ObjRef::new(PG_CONSTRAINT, oid)], false)?;
        }
        self.catalog.set_domain_not_null(target.oid, not_null)
    }

    /// `AlterDomainAddConstraint`: a check constraint or a not-null constraint. A constraint with `NOT VALID` does not check the rows, and a not-null constraint on a domain that has one already does nothing.
    fn alter_domain_add_constraint(&mut self, target: &Target, def: Option<&Node>) -> Result<()> {
        let Some(Node::Constraint(con)) = def else {
            return Err(Error::internal("unrecognized node type of a domain constraint"));
        };
        match con.contype {
            ConstrType::CONSTR_CHECK => {
                let (oid, expr) =
                    self.add_domain_check(target.oid, con, (target.base, target.typmod), false)?;
                if con.skip_validation {
                    self.catalog.set_constraint_validated(oid, false)?;
                } else {
                    self.validate_domain(target.oid, Some(expr))?;
                }
            }
            ConstrType::CONSTR_NOTNULL => {
                if target.not_null {
                    return Ok(());
                }
                let oid = self.catalog.add_domain_not_null(target.oid, con.conname.as_deref())?;
                if con.skip_validation {
                    self.catalog.set_constraint_validated(oid, false)?;
                } else {
                    self.validate_domain(target.oid, None)?;
                }
            }
            _ => return Err(Error::internal("unrecognized constraint subtype of a domain")),
        }
        Ok(())
    }

    /// `AlterDomainDropConstraint`: the drop of a not-null constraint also clears `typnotnull`.
    fn alter_domain_drop_constraint(
        &mut self,
        target: &Target,
        name: &str,
        cascade: bool,
        missing_ok: bool,
    ) -> Result<()> {
        let found = self
            .catalog
            .constraints_of_domain(target.oid)
            .find(|c| c.name == name)
            .map(|c| (c.oid, c.kind));
        let Some((oid, kind)) = found else {
            let message =
                format!("constraint \"{name}\" of domain \"{}\" does not exist", target.written);
            if !missing_ok {
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
            return Ok(());
        };
        if kind == ConKind::NotNull {
            self.catalog.set_domain_not_null(target.oid, false)?;
        }
        self.perform(&[ObjRef::new(PG_CONSTRAINT, oid)], cascade)
    }

    /// `AlterDomainValidateConstraint`: a check constraint that is not valid checks the rows and becomes valid.
    fn alter_domain_validate_constraint(&mut self, target: &Target, name: &str) -> Result<()> {
        let found = self
            .catalog
            .constraints_of_domain(target.oid)
            .find(|c| c.name == name)
            .map(|c| (c.oid, c.kind, c.validated, c.expr.clone()));
        let Some((oid, kind, validated, text)) = found else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of domain \"{}\" does not exist", target.written),
            ));
        };
        if kind != ConKind::Check {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "constraint \"{name}\" of domain \"{}\" is not a check constraint",
                    target.written
                ),
            ));
        }
        if validated {
            return Ok(());
        }
        let Some(text) = text else {
            return Err(Error::internal(format!("null conbin for constraint {oid}")));
        };
        self.validate_domain(target.oid, Some(crate::node::read(&text)?))?;
        self.catalog.set_constraint_validated(oid, true)
    }

    /// `validateDomainCheckConstraint` with a check, and `validateDomainNotNullConstraint` without one: the tables of `get_rels_with_domain` go to the caller, which checks their rows.
    fn validate_domain(&mut self, domain: u32, check: Option<Expr>) -> Result<()> {
        let origin = self.type_text(domain);
        let mut tables = Vec::new();
        self.rels_with_domain(domain, &origin, &mut tables, 0)?;
        for (table, columns) in tables {
            self.scans.push(table);
            self.domain_checks.push(DomainCheck { table, columns, check: check.clone() });
        }
        Ok(())
    }

    /// `get_rels_with_domain`: the tables and the materialized views with columns of the domain, and of the domains over it, with the numbers of these columns. A column of another type that holds the domain, such as an array, a range or a composite type, stops the statement, as `find_composite_type_dependencies` finds it.
    fn rels_with_domain(
        &self,
        domain: u32,
        origin: &str,
        out: &mut Vec<(u32, Vec<i16>)>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(stack_depth());
        }
        for object in self.type_dependents(domain) {
            if object.class == PG_TYPE {
                if self.catalog.type_by_oid(object.oid).is_some_and(|t| t.domain.is_some()) {
                    self.rels_with_domain(object.oid, origin, out, depth + 1)?;
                } else {
                    self.composite_type_users(object.oid, origin, depth + 1)?;
                }
                continue;
            }
            if object.class != PG_CLASS || object.sub <= 0 {
                continue;
            }
            let Some(rel) = self.catalog.relation(object.oid) else { continue };
            let at = match out.iter().position(|(table, _)| *table == rel.oid) {
                Some(at) => at,
                None => {
                    self.composite_type_users(rel.row_type, origin, depth + 1)?;
                    if !matches!(rel.kind, RelKind::Table | RelKind::MatView) {
                        continue;
                    }
                    out.push((rel.oid, Vec::new()));
                    out.len() - 1
                }
            };
            let Ok(number) = i16::try_from(object.sub) else { continue };
            if rel.column(number).is_none_or(|c| c.dropped || c.ty != domain) {
                continue;
            }
            let columns = &mut out[at].1;
            let place = columns.partition_point(|&n| n < number);
            columns.insert(place, number);
        }
        Ok(())
    }

    /// `find_composite_type_dependencies` for a type that holds the domain `origin`: a column of a relation with storage that has the type, also through another type, stops the statement.
    fn composite_type_users(&self, ty: u32, origin: &str, depth: usize) -> Result<()> {
        if ty == 0 {
            return Ok(());
        }
        if depth > MAX_DEPTH {
            return Err(stack_depth());
        }
        for object in self.type_dependents(ty) {
            if object.class == PG_TYPE {
                self.composite_type_users(object.oid, origin, depth + 1)?;
                continue;
            }
            if object.class != PG_CLASS {
                continue;
            }
            let Some(rel) = self.catalog.relation(object.oid) else { continue };
            let column = i16::try_from(object.sub)
                .ok()
                .filter(|&n| n > 0)
                .and_then(|n| rel.column(n))
                .or_else(|| rel.columns.iter().find(|c| c.ty == ty && !c.dropped));
            let Some(column) = column else { continue };
            let stored = matches!(
                rel.kind,
                RelKind::Table
                    | RelKind::Index
                    | RelKind::Sequence
                    | RelKind::Toast
                    | RelKind::MatView
                    | RelKind::Partitioned
                    | RelKind::PartitionedIndex
            );
            if stored {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot alter type \"{origin}\" because column \"{}.{}\" uses it",
                        rel.name, column.name
                    ),
                ));
            }
            self.composite_type_users(rel.row_type, origin, depth + 1)?;
        }
        Ok(())
    }

    /// The objects that depend on the type `ty` in `pg_depend`: the rows of the catalog, then the range types with `ty` as their subtype, whose rows rupg does not keep.
    fn type_dependents(&self, ty: u32) -> Vec<ObjRef> {
        let me = ObjRef::new(PG_TYPE, ty);
        let mut objects: Vec<ObjRef> = self
            .catalog
            .depends()
            .iter()
            .filter(|d| d.referenced == me)
            .map(|d| d.object)
            .collect();
        for range in self.catalog.types() {
            if let Some(RangePart::Range(info)) = &range.range
                && info.subtype == ty
            {
                objects.push(ObjRef::new(PG_TYPE, range.oid));
            }
        }
        objects
    }
}

/// The error of `check_stack_depth`.
fn stack_depth() -> Error {
    Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
}

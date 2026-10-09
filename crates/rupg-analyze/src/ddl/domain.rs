//! `CREATE DOMAIN`, as `DefineDomain` and `domainAddCheckConstraint` of `typecmds.c` run it.

use rupg_catalog::{Domain, NewDomain};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{ConstrType, Constraint, CreateDomainStmt, Node};
use rupg_types::Value;

use super::{Definer, references};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::typename::{names, place};
use crate::types;

/// The kinds of type that a domain can have as its base type: base, composite, domain, enum, range and multirange.
const BASE_KINDS: &[u8] = b"bcderm";

fn syntax_error(message: &str, location: i32) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, message).at_opt(place(location))
}

fn definition_error(message: &str, location: i32) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message).at_opt(place(location))
}

impl Definer<'_, '_> {
    /// `DefineDomain`: the domain, its array type and its check constraints. The return value is the OID of the domain.
    pub(super) fn create_domain(&mut self, stmt: &CreateDomainStmt) -> Result<u32> {
        let parts = names(&stmt.domainname);
        let (schema, name) = self.an.split_name(&parts, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        self.check_create_in(namespace, None)?;
        let type_name = stmt.typeName.as_deref().ok_or_else(|| Error::internal("a domain type"))?;
        let type_at = place(type_name.location);
        let (base, typmod) = self.an.type_name(type_name)?;
        let row = types::row(base);
        let kind = row.map_or(b'c', |t| t.kind);
        if !BASE_KINDS.contains(&kind) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "\"{}\" is not a valid base type for a domain",
                    crate::typename::type_name_text(type_name)
                ),
            )
            .at_opt(type_at));
        }
        let base_collation = row.map_or(0, |t| t.collation);
        let collation = match &stmt.collClause {
            Some(clause) => self.an.collation_oid(&clause.collname, place(clause.location))?,
            None => base_collation,
        };
        if collation != 0 && base_collation == 0 {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("collations are not supported by type {}", types::name(base)),
            )
            .at_opt(type_at));
        }
        let constraints: Vec<&Constraint> = stmt
            .constraints
            .iter()
            .filter_map(|n| match n {
                Some(Node::Constraint(c)) => Some(&**c),
                _ => None,
            })
            .collect();
        let mut default = None;
        let mut default_refs = Vec::new();
        let mut saw_default = false;
        let mut null_defined = false;
        for con in &constraints {
            match con.contype {
                ConstrType::CONSTR_DEFAULT => {
                    if saw_default {
                        return Err(syntax_error("multiple default expressions", con.location));
                    }
                    saw_default = true;
                    if let Some(expr) = self.domain_default(con, base, typmod, name)? {
                        default_refs = references(0, &[&expr]);
                        default = Some(crate::node::write(&expr)?);
                    }
                }
                ConstrType::CONSTR_NOTNULL => {
                    if null_defined {
                        return Err(syntax_error(
                            "conflicting NULL/NOT NULL constraints",
                            con.location,
                        ));
                    }
                    return Err(super::not_yet("NOT NULL on a domain", place(con.location)));
                }
                ConstrType::CONSTR_NULL => null_defined = true,
                ConstrType::CONSTR_CHECK if con.is_no_inherit => {
                    return Err(definition_error(
                        "check constraints for domains cannot be marked NO INHERIT",
                        con.location,
                    ));
                }
                ConstrType::CONSTR_CHECK => {}
                ConstrType::CONSTR_UNIQUE => {
                    return Err(syntax_error(
                        "unique constraints not possible for domains",
                        con.location,
                    ));
                }
                ConstrType::CONSTR_PRIMARY => {
                    return Err(syntax_error(
                        "primary key constraints not possible for domains",
                        con.location,
                    ));
                }
                ConstrType::CONSTR_EXCLUSION => {
                    return Err(syntax_error(
                        "exclusion constraints not possible for domains",
                        con.location,
                    ));
                }
                ConstrType::CONSTR_FOREIGN => {
                    return Err(syntax_error(
                        "foreign key constraints not possible for domains",
                        con.location,
                    ));
                }
                ConstrType::CONSTR_ATTR_DEFERRABLE
                | ConstrType::CONSTR_ATTR_NOT_DEFERRABLE
                | ConstrType::CONSTR_ATTR_DEFERRED
                | ConstrType::CONSTR_ATTR_IMMEDIATE => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "specifying constraint deferrability not supported for domains",
                    )
                    .at_opt(place(con.location)));
                }
                ConstrType::CONSTR_GENERATED | ConstrType::CONSTR_IDENTITY => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "specifying GENERATED not supported for domains",
                    )
                    .at_opt(place(con.location)));
                }
                ConstrType::CONSTR_ATTR_ENFORCED | ConstrType::CONSTR_ATTR_NOT_ENFORCED => {
                    return Err(definition_error(
                        "specifying constraint enforceability not supported for domains",
                        con.location,
                    ));
                }
                _ => return Err(Error::internal("unrecognized constraint subtype")),
            }
        }
        let new = NewDomain {
            namespace,
            name: name.to_string(),
            owner: self.user,
            domain: Domain { base, typmod, collation, default },
        };
        let domain = self.catalog.create_domain(new, &default_refs)?;
        for con in constraints.iter().filter(|c| c.contype == ConstrType::CONSTR_CHECK) {
            self.add_domain_check(domain, con, (base, typmod))?;
        }
        Ok(domain)
    }

    /// `cookDefault` of the default of a domain: the expression cast to the base type, or `None` for a null default.
    fn domain_default(
        &mut self,
        con: &Constraint,
        base: u32,
        typmod: i32,
        domain: &str,
    ) -> Result<Option<Expr>> {
        let Some(raw) = con.raw_expr.as_ref() else { return Ok(None) };
        let expr = self.an.with_kind(Kind::ColumnDefault, |an| an.transform(Some(raw)))?;
        let from = expr.ty;
        let Some(expr) = self.an.coerce_to_target(expr, base, typmod, Context::Assignment, None)?
        else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "column \"{domain}\" is of type {} but default expression is of type {}",
                    types::name(base),
                    types::name(from)
                ),
            )
            .with_hint("You will need to rewrite or cast the expression."));
        };
        if matches!(expr.kind, ExprKind::Const(Value::Null)) {
            return Ok(None);
        }
        Ok(Some(expr))
    }

    /// `domainAddCheckConstraint`: the check of a domain, where `value` is the value of the base type that the domain checks.
    fn add_domain_check(
        &mut self,
        domain: u32,
        con: &Constraint,
        (base, typmod): (u32, i32),
    ) -> Result<()> {
        let old = self.an.domain_value.replace((base, typmod));
        let expr = self.an.with_kind(Kind::Check, |an| an.transform(con.raw_expr.as_ref()));
        self.an.domain_value = old;
        let expr = self.an.coerce_to_boolean(expr?, "CHECK")?;
        let refs = references(0, &[&expr]);
        let text = crate::node::write(&expr)?;
        self.catalog.add_domain_check(domain, con.conname.as_deref(), text, &refs)?;
        Ok(())
    }
}

//! `CreatePolicy`, `AlterPolicy`, `rename_policy`, `DROP POLICY` and `COMMENT ON POLICY`.
//!
//! The analyzer reads an expression of a policy as `CreatePolicy` does, with the table of the policy alone in the range table. The catalog keeps the text of the statement that gave each expression and the schemas of `search_path`, and [`policy_tree`] reads the expression again for `pg_get_expr`.

use rupg_catalog::{ObjRef, PG_POLICY, Policy, PolicyExpr};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    AlterPolicyStmt, CreatePolicyStmt, DropBehavior, DropStmt, List, Node, RangeVar, RenameStmt,
    RoleSpecType,
};

use super::comment::{name_list, not_owner, range_var};
use super::drop::owner_word;
use super::view::expr_references;
use super::{Definer, Found, Message, not_yet, relname};
use crate::agg::Kind;
use crate::expr::Expr;
use crate::select::Query;
use crate::{Analyzer, Env, Params};

/// `ACL_ID_PUBLIC`: the role of `PUBLIC` in `polroles`.
const PUBLIC: u32 = 0;

/// `parse_policy_command`: the `polcmd` code of a command.
fn command_code(cmd: &str) -> u8 {
    match cmd {
        "select" => b'r',
        "insert" => b'a',
        "update" => b'w',
        "delete" => b'd',
        _ => b'*',
    }
}

/// An error of `CreatePolicy` and `AlterPolicy` with `ERRCODE_SYNTAX_ERROR`.
fn syntax(message: &str) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, message)
}

/// The error of a policy that the table does not have.
fn no_policy(policy: &str, table: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!("policy \"{policy}\" for table \"{table}\" does not exist"),
    )
}

/// The error of a policy name that the table has already.
fn duplicate(policy: &str, table: &str) -> Error {
    Error::new(
        SqlState::DUPLICATE_OBJECT,
        format!("policy \"{policy}\" for table \"{table}\" already exists"),
    )
}

/// The expression of `USING`, or of `WITH CHECK` with `check`, of a statement of a policy.
fn stmt_expr(node: &Node, check: bool) -> Option<&Node> {
    match node {
        Node::CreatePolicyStmt(stmt) if check => stmt.with_check.as_ref(),
        Node::CreatePolicyStmt(stmt) => stmt.qual.as_ref(),
        Node::AlterPolicyStmt(stmt) if check => stmt.with_check.as_ref(),
        Node::AlterPolicyStmt(stmt) => stmt.qual.as_ref(),
        _ => None,
    }
}

/// The expression of a policy on the table `relation` with the name `name`, read again from the text that the catalog keeps. The result is the range table of the expression and the expression.
fn stored_expr(
    env: &dyn Env,
    relation: u32,
    name: &str,
    stored: &PolicyExpr,
    check: bool,
) -> Result<(Query, Expr)> {
    let (list, _) = rupg_sql::parse(&stored.text).map_err(Error::from)?;
    let node = list.iter().flatten().find_map(|node| match node {
        Node::RawStmt(raw) => stmt_expr(raw.stmt.as_ref()?, check),
        _ => None,
    });
    let Some(node) = node else {
        return Err(Error::internal("the text of a policy has no expression"));
    };
    let mut an = Analyzer::new(env, &Params::default());
    an.set_path(&stored.path);
    an.keep_source = true;
    an.policy_expr(node, relation, name).map_err(|error| error.with_position(None))
}

/// The expression of `USING` of the policy `oid`, or of `WITH CHECK` with `check`, with its range table, as `pg_get_expr` reads `polqual` and `polwithcheck`. The result is `None` when no policy has the OID or the policy has no such expression.
pub fn policy_tree(env: &dyn Env, oid: u32, check: bool) -> Result<Option<(Query, Expr)>> {
    let Some(catalog) = env.catalog() else { return Ok(None) };
    let Some(policy) = catalog.policy(oid) else { return Ok(None) };
    let stored = if check { &policy.check } else { &policy.qual };
    let Some(stored) = stored else { return Ok(None) };
    let name = catalog.relation(policy.relation).map_or("", |r| r.name.as_str());
    stored_expr(env, policy.relation, name, stored, check).map(Some)
}

impl Analyzer<'_> {
    /// `transformWhereClause` with `EXPR_KIND_POLICY` for an expression of a policy on the table `oid` with the name `name`. The result is the range table of the expression, with the privileges and the functions of its subqueries, and the expression.
    pub(crate) fn policy_expr(
        &mut self,
        node: &Node,
        oid: u32,
        name: &str,
    ) -> Result<(Query, Expr)> {
        self.policy_scope(oid, name)?;
        let expr = self.with_kind(Kind::Policy, |a| a.transform(Some(node)))?;
        let expr = self.coerce_to_boolean(expr, "POLICY")?;
        let base = Query {
            relations: std::mem::take(&mut self.scope.relations),
            perms: std::mem::take(&mut self.perms),
            funcs: std::mem::take(&mut self.funcs),
            ..Query::default()
        };
        Ok((base, expr))
    }
}

/// The table of a statement of a policy.
struct Table {
    oid: u32,
    name: String,
}

impl Definer<'_, '_> {
    /// `policy_role_list_to_array`: the OIDs of the roles of `TO`. No roles and `PUBLIC` give `PUBLIC` alone, and `PUBLIC` with other roles gives a warning.
    fn policy_roles(&mut self, roles: &List) -> Result<Vec<u32>> {
        let specs: Vec<_> = roles
            .iter()
            .flatten()
            .filter_map(|node| match node {
                Node::RoleSpec(spec) => Some(spec),
                _ => None,
            })
            .collect();
        if specs.is_empty() {
            return Ok(vec![PUBLIC]);
        }
        let mut oids = Vec::with_capacity(specs.len());
        for spec in &specs {
            if spec.roletype == RoleSpecType::ROLESPEC_PUBLIC {
                if specs.len() != 1 {
                    self.flush();
                    let warning = Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "ignoring specified roles other than PUBLIC",
                    )
                    .with_hint("All roles are members of the PUBLIC role.");
                    self.messages.push(Message::Warning(warning));
                }
                return Ok(vec![PUBLIC]);
            }
            oids.push(self.role_oid(spec)?);
        }
        Ok(oids)
    }

    /// `RangeVarGetRelidExtended` with `RangeVarCallbackForPolicy`: the table of the statement. The role must own it, it must not be a system catalog, and it must be a table.
    fn policy_table(&mut self, rv: &RangeVar) -> Result<Table> {
        let found = self.lookup_relation(rv)?;
        let name = relname(rv).to_string();
        let code = self.kind_of(found);
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner(owner_word(code), &name));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        if !matches!(code, b'r' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a table"),
            ));
        }
        let Found::User(oid) = found else {
            return Err(not_yet("a policy on a system catalog", None));
        };
        let name = self.catalog.relation(oid).map_or(name, |r| r.name.clone());
        Ok(Table { oid, name })
    }

    /// The expression of a policy, analyzed for the table, and the objects that it reads.
    fn analyze_policy_expr(&mut self, node: &Node, table: &Table) -> Result<Vec<ObjRef>> {
        // `CreatePolicy` makes the parse state with no text of the query, so an error has no position.
        let (base, expr) = self
            .an
            .policy_expr(node, table.oid, &table.name)
            .map_err(|error| error.with_position(None))?;
        let mut refs = Vec::new();
        expr_references(self.catalog, &expr, &[&base], &mut refs);
        self.check_usage_on_types(&refs)?;
        Ok(refs)
    }

    /// The objects that a stored expression of a policy reads.
    fn stored_policy_refs(
        &self,
        stored: Option<&PolicyExpr>,
        table: &Table,
        check: bool,
    ) -> Result<Vec<ObjRef>> {
        let Some(stored) = stored else { return Ok(Vec::new()) };
        let (base, expr) = stored_expr(self.an.env, table.oid, &table.name, stored, check)?;
        let mut refs = Vec::new();
        expr_references(self.catalog, &expr, &[&base], &mut refs);
        Ok(refs)
    }

    /// `CreatePolicy`. `text` is the text of the statement, which the catalog keeps for the expressions.
    pub(super) fn create_policy(&mut self, stmt: &CreatePolicyStmt, text: &str) -> Result<()> {
        let cmd = command_code(stmt.cmd_name.as_deref().unwrap_or("all"));
        if matches!(cmd, b'r' | b'd') && stmt.with_check.is_some() {
            return Err(syntax("WITH CHECK cannot be applied to SELECT or DELETE"));
        }
        if cmd == b'a' && stmt.qual.is_some() {
            return Err(syntax("only WITH CHECK expression allowed for INSERT"));
        }
        let roles = self.policy_roles(&stmt.roles)?;
        let rv = stmt.table.as_deref().cloned().unwrap_or_default();
        let table = self.policy_table(&rv)?;
        let qual_refs = match &stmt.qual {
            Some(node) => self.analyze_policy_expr(node, &table)?,
            None => Vec::new(),
        };
        let check_refs = match &stmt.with_check {
            Some(node) => self.analyze_policy_expr(node, &table)?,
            None => Vec::new(),
        };
        let name = stmt.policy_name.as_deref().unwrap_or_default();
        if self.catalog.policy_by_name(table.oid, name).is_some() {
            return Err(duplicate(name, &table.name));
        }
        let stored = || PolicyExpr { text: text.to_string(), path: self.an.path.clone() };
        let policy = Policy {
            oid: 0,
            name: name.to_string(),
            relation: table.oid,
            cmd,
            permissive: stmt.permissive,
            roles,
            qual: stmt.qual.as_ref().map(|_| stored()),
            check: stmt.with_check.as_ref().map(|_| stored()),
        };
        let policy = self.catalog.set_policy(policy, &qual_refs, &check_refs);
        self.collect(None, ObjRef::new(PG_POLICY, policy));
        Ok(())
    }

    /// `AlterPolicy`: new roles, a new expression of `USING` or a new expression of `WITH CHECK`. The policy gets new dependencies, also on the objects of an expression that it keeps.
    pub(super) fn alter_policy(&mut self, stmt: &AlterPolicyStmt, text: &str) -> Result<()> {
        let roles = match stmt.roles.is_empty() {
            true => None,
            false => Some(self.policy_roles(&stmt.roles)?),
        };
        let rv = stmt.table.as_deref().cloned().unwrap_or_default();
        let table = self.policy_table(&rv)?;
        let qual_refs = match &stmt.qual {
            Some(node) => Some(self.analyze_policy_expr(node, &table)?),
            None => None,
        };
        let check_refs = match &stmt.with_check {
            Some(node) => Some(self.analyze_policy_expr(node, &table)?),
            None => None,
        };
        let name = stmt.policy_name.as_deref().unwrap_or_default();
        let Some(old) = self.catalog.policy_by_name(table.oid, name).cloned() else {
            return Err(no_policy(name, &table.name));
        };
        if matches!(old.cmd, b'r' | b'd') && stmt.with_check.is_some() {
            return Err(syntax("only USING expression allowed for SELECT, DELETE"));
        }
        if old.cmd == b'a' && stmt.qual.is_some() {
            return Err(syntax("only WITH CHECK expression allowed for INSERT"));
        }
        let stored = || PolicyExpr { text: text.to_string(), path: self.an.path.clone() };
        let mut policy = old.clone();
        if let Some(roles) = roles {
            policy.roles = roles;
        }
        let qual_refs = match qual_refs {
            Some(refs) => {
                policy.qual = Some(stored());
                refs
            }
            None => self.stored_policy_refs(old.qual.as_ref(), &table, false)?,
        };
        let check_refs = match check_refs {
            Some(refs) => {
                policy.check = Some(stored());
                refs
            }
            None => self.stored_policy_refs(old.check.as_ref(), &table, true)?,
        };
        let policy = self.catalog.set_policy(policy, &qual_refs, &check_refs);
        self.collect(None, ObjRef::new(PG_POLICY, policy));
        Ok(())
    }

    /// `rename_policy`.
    pub(super) fn rename_policy(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let table = self.policy_table(&rv)?;
        if self.catalog.policy_by_name(table.oid, newname).is_some() {
            return Err(duplicate(newname, &table.name));
        }
        let subname = stmt.subname.as_deref().unwrap_or_default();
        let Some(policy) = self.catalog.policy_by_name(table.oid, subname).map(|p| p.oid) else {
            return Err(no_policy(subname, &table.name));
        };
        self.catalog.rename_policy(policy, newname);
        Ok(())
    }

    /// `get_object_address_relobject` and `check_object_ownership` for a policy: the policy of the table with the name, or `None` when the relation or the policy does not exist and `missing_ok` is true. `RemovePolicyById` gives the errors for a relation that is not a table and for a system catalog.
    fn policy_address(&mut self, object: Option<&Node>, missing_ok: bool) -> Result<Option<u32>> {
        let names = name_list(object);
        let Some((&policy, parent)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "must specify relation and object name",
            ));
        };
        let rv = range_var(parent)?;
        let found = match self.lookup_relation(&rv) {
            Ok(found) => found,
            Err(error)
                if missing_ok
                    && matches!(
                        error.state(),
                        SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                    ) =>
            {
                let message = error.message().to_string();
                self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Found::User(relation) = found else {
            if missing_ok {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!(
                        "policy \"{policy}\" for relation \"{}\" does not exist, skipping",
                        parent.join(".")
                    ),
                );
                return Ok(None);
            }
            return Err(no_policy(policy, relname(&rv)));
        };
        let rel = self.catalog.relation(relation);
        let relation_name = rel.map_or(String::new(), |r| r.name.clone());
        let owner = rel.map_or(0, |r| r.owner);
        let Some(oid) = self.catalog.policy_by_name(relation, policy).map(|p| p.oid) else {
            if missing_ok {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!(
                        "policy \"{policy}\" for relation \"{}\" does not exist, skipping",
                        parent.join(".")
                    ),
                );
                return Ok(None);
            }
            return Err(no_policy(policy, &relation_name));
        };
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner("relation", &relation_name));
        }
        Ok(Some(oid))
    }

    /// `RemoveObjects` for `DROP POLICY`.
    pub(super) fn drop_policy(&mut self, stmt: &DropStmt) -> Result<()> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            if let Some(oid) = self.policy_address(Some(item), stmt.missing_ok)? {
                objects.push(ObjRef::new(PG_POLICY, oid));
            }
        }
        self.perform(&objects, stmt.behavior == DropBehavior::DROP_CASCADE)
    }

    /// The policy of `COMMENT ON POLICY`.
    pub(super) fn comment_policy(&mut self, object: Option<&Node>) -> Result<ObjRef> {
        let oid = self.policy_address(object, false)?;
        Ok(ObjRef::new(PG_POLICY, oid.unwrap_or_default()))
    }

    /// `RemoveRoleFromObjectPolicy` for `DROP OWNED`: the policies that name the role lose it, and the policies that name only the role go into `doomed`.
    pub(super) fn remove_role_from_policies(&mut self, role: u32, doomed: &mut Vec<ObjRef>) {
        let policies: Vec<(u32, Vec<u32>)> = self
            .catalog
            .policies()
            .filter(|p| p.roles.contains(&role))
            .map(|p| (p.oid, p.roles.iter().copied().filter(|&r| r != role).collect()))
            .collect();
        for (oid, roles) in policies {
            if roles.is_empty() {
                let object = ObjRef::new(PG_POLICY, oid);
                if !doomed.contains(&object) {
                    doomed.push(object);
                }
            } else {
                self.catalog.set_policy_roles(oid, roles);
            }
        }
    }
}

//! `get_row_security_policies` of `rowsecurity.c` and the part of `fireRIRrules` that adds the `securityQuals` of the tables of a query.
//!
//! Each table of the query whose policies apply to the role of its checks becomes a subquery that reads the table and keeps the rows that pass the conditions of the policies. The executor treats such a subquery as a view with `security_barrier`, so the conditions of the query run on a row only after the conditions of the policies pass, except for a leakproof condition, as `order_qual_clauses` orders the `securityQuals` of the scan.

use rupg_catalog::{Policy, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{Value, oid};

use crate::Env;
use crate::expr::{BoolOp, Expr, ExprKind, Var};
use crate::from::{FromItem, Relation};
use crate::perm::{ACL_DELETE, ACL_INSERT, ACL_UPDATE, PermInfo, shift_expr};
use crate::select::{Query, Target};

/// `FirstNormalObjectId`: a relation with a lower OID is built in and has no policies.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// The result of `check_enable_rls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RlsStatus {
    /// `RLS_NONE`: the table has no row security.
    None,
    /// `RLS_NONE_ENV`: the policies do not apply to the role, but they can apply to another role.
    NoneEnv,
    /// `RLS_ENABLED`: the policies apply.
    Enabled,
}

/// The roles that the policies of the tables of a query apply to.
pub trait RowSecurity {
    /// `check_enable_rls` for the table `relid` and the role `check_as`, or the current user for 0. With `row_security` off, a table whose policies apply is an error.
    ///
    /// # Errors
    ///
    /// `42501` when the policies apply and `row_security` is off.
    fn status(&self, relid: u32, check_as: u32) -> Result<RlsStatus>;
    /// The role `check_as`, or the current user for 0.
    fn role(&self, check_as: u32) -> u32;
    /// `check_role_for_policy`: true when the role is a member of a role of `polroles`, or `polroles` is `PUBLIC`.
    fn applies(&self, roles: &[u32], role: u32) -> bool;
}

/// `fireRIRrules` for the policies: each table of the query and of the queries in it whose policies apply becomes a subquery with the conditions of the policies. The result is true when a table of the query has row security, so that a plan for another role or with another `row_security` needs the walk again, as `hasRowSecurity` marks the plan.
///
/// # Errors
///
/// `42P17` for a policy that reads its own table through the policies of other tables, `42501` for a table whose policies apply when `row_security` is off, and `0A000` for a write into a table whose policies apply.
pub fn apply_row_security(
    query: &mut Query,
    env: &dyn Env,
    security: &dyn RowSecurity,
) -> Result<bool> {
    let mut walk = Walk {
        env,
        security,
        perms: std::mem::take(&mut query.perms),
        funcs: Vec::new(),
        active: Vec::new(),
        seen: false,
    };
    let result = walk.query(query);
    query.perms = walk.perms;
    query.funcs.append(&mut walk.funcs);
    result.map(|()| walk.seen)
}

/// The state of the walk.
struct Walk<'a> {
    env: &'a dyn Env,
    security: &'a dyn RowSecurity,
    /// The privileges of the statement, which grow with the privileges of the subqueries of the policies.
    perms: Vec<PermInfo>,
    /// The functions of the conditions of the policies, for the checks of `EXECUTE`.
    funcs: Vec<u32>,
    /// `activeRIRs`: the tables whose policies the walk is in.
    active: Vec<u32>,
    /// `hasRowSecurity`.
    seen: bool,
}

/// The conditions of the policies of a table, each with the OIDs of the policies that give it, as `list_append_unique` compares them.
struct Quals {
    quals: Vec<Expr>,
    keys: Vec<Vec<u32>>,
}

impl Quals {
    fn push(&mut self, key: Vec<u32>, qual: Expr) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
            self.quals.push(qual);
        }
    }
}

impl Walk<'_> {
    /// The tables of the query, of its subqueries in `FROM`, of its queries of `WITH`, of its subqueries in expressions and of the queries of its write.
    fn query(&mut self, query: &mut Query) -> Result<()> {
        let target = match &query.write {
            Some(write) => self.write_target(write.table)?,
            None => None,
        };
        for index in 0..query.relations.len() {
            if let Some(sub) = query.relations[index].subquery.as_deref_mut() {
                self.query(sub)?;
                continue;
            }
            let relation = &query.relations[index];
            if relation.function.is_some()
                || relation.values.is_some()
                || relation.cte.is_some()
                || relation.perm.is_some_and(|p| Some(p) == target)
            {
                continue;
            }
            if let Some(quals) = self.relation_quals(relation)? {
                if system_columns(query, index) {
                    return Err(not_yet(
                        "a system column of a table that row-level security policies apply to",
                    ));
                }
                wrap(&mut query.relations[index], quals);
            }
        }
        for cte in &mut query.ctes {
            self.query(&mut cte.query)?;
        }
        for expr in query.exprs_mut() {
            self.expr(expr)?;
        }
        let Some(write) = query.write.as_mut() else { return Ok(()) };
        if let Some(returning) = write.returning.as_deref_mut() {
            self.query(returning)?;
        }
        if let Some(update) = write.on_conflict.as_mut().and_then(|c| c.update.as_deref_mut()) {
            self.query(update)?;
        }
        for clause in write.merge.iter_mut().flat_map(|m| &mut m.clauses) {
            self.query(&mut clause.query)?;
        }
        Ok(())
    }

    /// The subqueries of an expression.
    fn expr(&mut self, expr: &mut Expr) -> Result<()> {
        for child in expr.children_mut() {
            self.expr(child)?;
        }
        if let ExprKind::SubLink(link) = &mut expr.kind {
            self.query(&mut link.query)?;
        }
        Ok(())
    }

    /// The index of the privileges of the target table `table` of a write. A write into a table whose policies apply is not supported yet.
    fn write_target(&mut self, table: u32) -> Result<Option<usize>> {
        let writes = ACL_INSERT | ACL_UPDATE | ACL_DELETE;
        let Some(index) =
            self.perms.iter().position(|p| p.relid == table && p.required & writes != 0)
        else {
            return Ok(None);
        };
        let check_as = self.perms[index].check_as;
        match self.security.status(table, check_as)? {
            RlsStatus::None => {}
            RlsStatus::NoneEnv => self.seen = true,
            RlsStatus::Enabled => {
                return Err(not_yet(
                    "a write into a table that row-level security policies apply to",
                ));
            }
        }
        Ok(Some(index))
    }

    /// `get_row_security_policies` for a table that a query reads: the conditions of the policies that apply to the role of its checks, or `None` when no policy applies. With `FOR UPDATE` or `FOR SHARE`, the conditions of the policies of `UPDATE` come first.
    fn relation_quals(&mut self, relation: &Relation) -> Result<Option<Vec<Expr>>> {
        // A relation that needs no privileges is a relation of a statement that the server runs for its own work, which no policy limits.
        let Some(perm) =
            relation.perm.and_then(|p| self.perms.get(p)).filter(|p| p.required != 0).cloned()
        else {
            return Ok(None);
        };
        if relation.oid < FIRST_NORMAL_OBJECT_ID {
            return Ok(None);
        }
        let Some(rel) = self.env.catalog().and_then(|c| c.relation(relation.oid)) else {
            return Ok(None);
        };
        if !matches!(rel.kind, RelKind::Table | RelKind::Partitioned) {
            return Ok(None);
        }
        let name = rel.name.clone();
        match self.security.status(relation.oid, perm.check_as)? {
            RlsStatus::None => return Ok(None),
            RlsStatus::NoneEnv => {
                self.seen = true;
                return Ok(None);
            }
            RlsStatus::Enabled => self.seen = true,
        }
        let role = self.security.role(perm.check_as);
        let mut quals = Quals { quals: Vec::new(), keys: Vec::new() };
        if perm.required & ACL_UPDATE != 0 {
            self.add_quals(relation.oid, b'w', role, &mut quals)?;
        }
        self.add_quals(relation.oid, b'r', role, &mut quals)?;
        let mut quals = quals.quals;
        if quals.iter().any(has_sublink) {
            if self.active.contains(&relation.oid) {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!("infinite recursion detected in policy for relation \"{name}\""),
                ));
            }
            self.active.push(relation.oid);
            let result = quals.iter_mut().try_for_each(|qual| self.expr(qual));
            self.active.pop();
            result?;
        }
        Ok(Some(quals))
    }

    /// `get_policies_for_relation` and `add_security_quals`: the conditions of the policies of the table `relid` for the command `cmd` that apply to `role`. The restrictive policies come in the order of their names, then the permissive policies, which `OR` joins, in the reverse order of their names, as `RelationBuildRowSecurity` puts each policy at the start of the list. With no permissive policy, the condition is false.
    fn add_quals(&mut self, relid: u32, cmd: u8, role: u32, quals: &mut Quals) -> Result<()> {
        let Some(catalog) = self.env.catalog() else { return Ok(()) };
        let mut policies: Vec<&Policy> = catalog
            .policies()
            .filter(|p| p.relation == relid && (p.cmd == b'*' || p.cmd == cmd))
            .filter(|p| self.security.applies(&p.roles, role))
            .collect();
        policies.sort_by(|a, b| a.name.cmp(&b.name));
        let restrictive: Vec<u32> =
            policies.iter().filter(|p| !p.permissive && p.qual.is_some()).map(|p| p.oid).collect();
        let permissive: Vec<u32> = policies
            .iter()
            .rev()
            .filter(|p| p.permissive && p.qual.is_some())
            .map(|p| p.oid)
            .collect();
        if permissive.is_empty() {
            quals.push(Vec::new(), Expr::constant(Value::Bool(false), oid::BOOL, -1, None));
            return Ok(());
        }
        for policy in restrictive {
            if let Some(qual) = self.policy_qual(policy)? {
                quals.push(vec![policy], qual);
            }
        }
        let mut parts = Vec::with_capacity(permissive.len());
        for &policy in &permissive {
            parts.extend(self.policy_qual(policy)?);
        }
        let qual = if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::new(ExprKind::Bool(BoolOp::Or, parts), oid::BOOL)
        };
        quals.push(permissive, qual);
        Ok(())
    }

    /// The condition of `USING` of the policy, which reads the relation 0. The privileges and the functions of its subqueries go after those of the statement.
    fn policy_qual(&mut self, policy: u32) -> Result<Option<Expr>> {
        let Some((base, mut qual)) = crate::policy_tree(self.env, policy, false)? else {
            return Ok(None);
        };
        shift_expr(&mut qual, self.perms.len());
        self.perms.extend(base.perms);
        self.funcs.extend(base.funcs);
        Ok(Some(qual))
    }
}

/// True when the expression has a subquery.
fn has_sublink(expr: &Expr) -> bool {
    expr.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
}

/// True when the query reads a system column of the relation `index`, also in its subqueries.
fn system_columns(query: &Query, index: usize) -> bool {
    query.all_exprs(0).into_iter().any(|(expr, depth)| {
        expr.find(depth, &mut |e, depth| match e.kind {
            ExprKind::Var(Var { relation, attnum, levels_up })
                if relation == index && levels_up == depth && attnum < 0 =>
            {
                Some(())
            }
            _ => None,
        })
        .is_some()
    })
}

/// The table `relation` as a subquery that reads the table and keeps the rows that pass the conditions `quals`, in their order. Each column of the subquery is the column of the table with the same number, and a dropped column is a null.
fn wrap(relation: &mut Relation, quals: Vec<Expr>) {
    let mut inner = relation.clone();
    inner.perm = None;
    inner.lateral = false;
    relation.tablesample = None;
    let targets = relation
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let expr = if column.is_dropped() {
                Expr::constant(Value::Null, 0, -1, None)
            } else {
                let mut var =
                    Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), column.ty);
                var.typmod = column.typmod;
                var
            };
            Target {
                name: column.name.clone(),
                expr,
                origin: Some((relation.oid, attnum)),
                junk: false,
            }
        })
        .collect();
    let barrier = vec![1; quals.len()];
    let filter = match quals.len() {
        1 => quals.into_iter().next(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, quals), oid::BOOL)),
    };
    relation.subquery = Some(Box::new(Query {
        relations: vec![inner],
        from: vec![FromItem::Relation(0)],
        targets,
        filter,
        barrier,
        ..Query::default()
    }));
}

/// The error of a feature that the engine does not have yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

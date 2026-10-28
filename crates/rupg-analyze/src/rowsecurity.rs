//! `get_row_security_policies` of `rowsecurity.c` and the part of `fireRIRrules` that adds the `securityQuals` of the tables of a query.
//!
//! Each table of the query whose policies apply to the role of its checks becomes a subquery that reads the table and keeps the rows that pass the conditions of the policies. The executor treats such a subquery as a view with `security_barrier`, so the conditions of the query run on a row only after the conditions of the policies pass, except for a leakproof condition, as `order_qual_clauses` orders the `securityQuals` of the scan.

use rupg_catalog::{Policy, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{Value, oid};

use crate::Env;
use crate::expr::{BoolOp, Expr, ExprKind, Var};
use crate::from::{Column, FromItem, Relation, TABLE_OID_ATTNUM};
use crate::insert::{PolicyCheck, PolicyKind, WriteKind};
use crate::perm::{ACL_DELETE, ACL_INSERT, ACL_SELECT, ACL_UPDATE, PermInfo, shift_expr};
use crate::select::{Query, Target};

/// The attribute number of the system column `ctid`.
const CTID_ATTNUM: i16 = -1;

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
/// `42P17` for a policy that reads its own table through the policies of other tables, `42501` for a table whose policies apply when `row_security` is off, and `0A000` for `MERGE` into a table with children whose policies apply.
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
        target: None,
        target_quals: None,
        merge_target: false,
    };
    let result = walk.statement(query);
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
    /// The index of the privileges of the target table of the write.
    target: Option<usize>,
    /// The `securityQuals` of the target table of an `UPDATE` or a `DELETE`, which go in the query that reads the table.
    target_quals: Option<Vec<Expr>>,
    /// True when [`Walk::target_quals`] are the quals of the target table of a `MERGE`, which limit the rows before the join.
    merge_target: bool,
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

/// The checks of the policies of a write, each with the name of its policy, or `None` for the check of the permissive policies. Each check has a key of the name and of the policies and their conditions that give it, as `list_append_unique` compares the items.
#[derive(Default)]
struct Checks {
    checks: Vec<(Option<String>, Expr)>,
    keys: Vec<CheckKey>,
}

/// The key of a check: the name of its policy, and the OID of each policy that gives it with true when the condition is the condition of `WITH CHECK`.
type CheckKey = (Option<String>, Vec<(u32, bool)>);

impl Checks {
    fn push(&mut self, key: CheckKey, qual: Expr) {
        if !self.keys.contains(&key) {
            self.checks.push((key.0.clone(), qual));
            self.keys.push(key);
        }
    }
}

impl Walk<'_> {
    /// The tables of the query, of its subqueries in `FROM`, of its queries of `WITH`, of its subqueries in expressions and of the queries of its write.
    fn query(&mut self, query: &mut Query) -> Result<()> {
        for index in 0..query.relations.len() {
            if let Some(sub) = query.relations[index].subquery.as_deref_mut() {
                self.query(sub)?;
                continue;
            }
            let relation = &query.relations[index];
            if relation.function.is_some()
                || relation.values.is_some()
                || relation.cte.is_some()
                || relation.excluded
            {
                continue;
            }
            if relation.perm.is_some() && relation.perm == self.target {
                if let Some(quals) = self.target_quals.take() {
                    if self.merge_target {
                        add_merge_quals(query, index, quals);
                    } else {
                        add_target_quals(query, index, quals);
                    }
                }
                continue;
            }
            if let Some(quals) = self.relation_quals(relation)? {
                let system = system_columns(query, index);
                wrap(&mut query.relations[index], quals, system);
            }
        }
        for cte in &mut query.ctes {
            if cte.write.is_some() {
                self.statement(&mut cte.query)?;
            } else {
                self.query(&mut cte.query)?;
            }
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

    /// The statement `query` and the policies of the target table of its write, also for a statement of `WITH` that changes data, as `fireRIRrules` runs for the query of each such statement. The target of the statement replaces the target of the statement above it during the walk.
    fn statement(&mut self, query: &mut Query) -> Result<()> {
        let outer = (self.target.take(), self.target_quals.take(), self.merge_target);
        self.merge_target = false;
        let result = self.write(query).and_then(|()| self.query(query));
        (self.target, self.target_quals, self.merge_target) = outer;
        result
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

    /// `get_row_security_policies` for the target table of a write: the `securityQuals` of an `UPDATE` and of a `DELETE`, which [`Walk::query`] puts in the query that reads the table, and the `withCheckOptions` of an `INSERT` and of an `UPDATE`, which the write checks on each new row. When the write needs `SELECT` on the table, as for `RETURNING`, the policies of `SELECT` add to both.
    fn write(&mut self, query: &mut Query) -> Result<()> {
        let Some(write) = query.write.as_ref() else { return Ok(()) };
        let writes = ACL_INSERT | ACL_UPDATE | ACL_DELETE;
        // A `MERGE` with only `DO NOTHING` needs no privilege to write the target table.
        let merge = write.kind == WriteKind::Merge;
        let target = |p: &PermInfo| {
            p.relid == write.table && (p.required & writes != 0 || merge && p.required != 0)
        };
        // The target of an `UPDATE`, a `DELETE` or a `MERGE` is a relation of its query, and another statement of the query can write the same table.
        let own = query
            .relations
            .iter()
            .filter_map(|r| r.perm)
            .find(|&i| self.perms.get(i).is_some_and(target));
        let Some(index) = own.or_else(|| self.perms.iter().position(target)) else {
            return Ok(());
        };
        let Some(write) = query.write.as_mut() else { return Ok(()) };
        self.target = Some(index);
        let perm = self.perms[index].clone();
        let Some(rel) = self.env.catalog().and_then(|c| c.relation(write.table)) else {
            return Ok(());
        };
        if !matches!(rel.kind, RelKind::Table | RelKind::Partitioned) {
            return Ok(());
        }
        match self.security.status(write.table, perm.check_as)? {
            RlsStatus::None => return Ok(()),
            RlsStatus::NoneEnv => {
                self.seen = true;
                return Ok(());
            }
            RlsStatus::Enabled => self.seen = true,
        }
        let cmd = match write.kind {
            WriteKind::Insert => b'a',
            WriteKind::Update => b'w',
            WriteKind::Delete => b'd',
            WriteKind::Merge if write.inherit || rel.kind == RelKind::Partitioned => {
                return Err(not_yet(
                    "MERGE into a table with children that row-level security policies apply to",
                ));
            }
            WriteKind::Merge => b'm',
        };
        let (table, name) = (write.table, rel.name.clone());
        let columns = crate::insert::write_columns(rel);
        let role = self.security.role(perm.check_as);
        let select = perm.required & ACL_SELECT != 0;
        if cmd == b'm' {
            return self.merge(write, (table, &name, &columns), role, select);
        }
        if cmd != b'a' {
            let mut quals = Quals { quals: Vec::new(), keys: Vec::new() };
            self.add_quals(table, cmd, role, &mut quals)?;
            if select {
                self.add_quals(table, b'r', role, &mut quals)?;
            }
            let mut quals = quals.quals;
            self.sublinks(table, &name, &mut quals)?;
            self.target_quals = Some(quals);
        }
        if cmd != b'd' {
            let mut checks = Checks::default();
            self.add_checks(table, cmd, role, false, &mut checks)?;
            if select {
                self.add_checks(table, b'r', role, true, &mut checks)?;
            }
            write.policy_check =
                Some(self.check_query(table, (&name, &columns), checks, PolicyKind::New)?);
        }
        // `INSERT ... ON CONFLICT DO UPDATE` and `DO SELECT` check the row that the table has with the conditions of `USING` of the policies of `UPDATE` and `SELECT`, and `DO UPDATE` checks the new row with the policies of `UPDATE`.
        if let Some(conflict) = write.on_conflict.as_mut().filter(|c| c.update.is_some()) {
            let mut checks = Checks::default();
            if perm.required & ACL_UPDATE != 0 {
                self.add_checks(table, b'w', role, true, &mut checks)?;
            }
            if select {
                self.add_checks(table, b'r', role, true, &mut checks)?;
            }
            conflict.conflict_check =
                Some(self.check_query(table, (&name, &columns), checks, PolicyKind::Conflict)?);
            if conflict.select.is_none() {
                let mut checks = Checks::default();
                self.add_checks(table, b'w', role, false, &mut checks)?;
                if select {
                    self.add_checks(table, b'r', role, true, &mut checks)?;
                }
                conflict.update_check =
                    Some(self.check_query(table, (&name, &columns), checks, PolicyKind::New)?);
            }
        }
        Ok(())
    }

    /// `get_row_security_policies` for the target table of a `MERGE`. The conditions of `USING` of the policies of `SELECT` limit the rows of the target table before the join, and each action checks its rows: an `UPDATE` checks the row of the target table with the conditions of `USING` of the policies of `UPDATE`, and its new row with the policies of `UPDATE` and `SELECT`, a `DELETE` checks the row of the target table with the policies of `DELETE`, and an `INSERT` checks its new row with the policies of `INSERT`, and with the policies of `SELECT` when the statement has `RETURNING`.
    fn merge(
        &mut self,
        write: &mut crate::insert::Write,
        (table, name, columns): (u32, &str, &[Column]),
        role: u32,
        select: bool,
    ) -> Result<()> {
        if select {
            let mut quals = Quals { quals: Vec::new(), keys: Vec::new() };
            self.add_quals(table, b'r', role, &mut quals)?;
            let mut quals = quals.quals;
            self.sublinks(table, name, &mut quals)?;
            self.target_quals = Some(quals);
            self.merge_target = true;
        }
        let mut checks = Checks::default();
        self.add_checks(table, b'a', role, false, &mut checks)?;
        if select && write.returning.is_some() {
            self.add_checks(table, b'r', role, true, &mut checks)?;
        }
        write.policy_check =
            Some(self.check_query(table, (name, columns), checks, PolicyKind::New)?);
        let mut checks = Checks::default();
        self.add_checks(table, b'w', role, false, &mut checks)?;
        if select {
            self.add_checks(table, b'r', role, true, &mut checks)?;
        }
        let update_check = self.check_query(table, (name, columns), checks, PolicyKind::New)?;
        let mut checks = Checks::default();
        self.add_checks(table, b'w', role, true, &mut checks)?;
        let update_target = self.check_query(table, (name, columns), checks, PolicyKind::Target)?;
        let mut checks = Checks::default();
        self.add_checks(table, b'd', role, true, &mut checks)?;
        let delete_target = self.check_query(table, (name, columns), checks, PolicyKind::Target)?;
        if let Some(merge) = write.merge.as_mut() {
            merge.update_check = Some(update_check);
            merge.update_target = Some(update_target);
            merge.delete_target = Some(delete_target);
        }
        Ok(())
    }

    /// The query of the checks `checks` of the kind `kind` of the table `table`, which reads a row with the columns `columns` of the table.
    fn check_query(
        &mut self,
        table: u32,
        (name, columns): (&str, &[Column]),
        checks: Checks,
        kind: PolicyKind,
    ) -> Result<PolicyCheck> {
        let (policies, mut quals): (Vec<Option<String>>, Vec<Expr>) =
            checks.checks.into_iter().unzip();
        self.sublinks(table, name, &mut quals)?;
        let mut relation = Relation::new(0, columns.to_vec(), None, None, None);
        relation.name = name.to_string();
        let targets = quals
            .into_iter()
            .map(|expr| Target { name: String::new(), expr, origin: None, junk: false })
            .collect();
        let query = Query {
            relations: vec![relation],
            from: vec![FromItem::Relation(0)],
            targets,
            ..Query::default()
        };
        Ok(PolicyCheck { query: Box::new(query), policies, kind })
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
        self.sublinks(relation.oid, &name, &mut quals)?;
        Ok(Some(quals))
    }

    /// `fireRIRonSubLink` for the conditions of the policies of the table `relid`: the tables of their subqueries, with the table in `activeRIRs`.
    fn sublinks(&mut self, relid: u32, name: &str, quals: &mut [Expr]) -> Result<()> {
        if !quals.iter().any(has_sublink) {
            return Ok(());
        }
        if self.active.contains(&relid) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("infinite recursion detected in policy for relation \"{name}\""),
            ));
        }
        self.active.push(relid);
        let result = quals.iter_mut().try_for_each(|qual| self.expr(qual));
        self.active.pop();
        result
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
            if let Some(qual) = self.policy_qual(policy, false)? {
                quals.push(vec![policy], qual);
            }
        }
        let mut parts = Vec::with_capacity(permissive.len());
        for &policy in &permissive {
            parts.extend(self.policy_qual(policy, false)?);
        }
        let qual = if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::new(ExprKind::Bool(BoolOp::Or, parts), oid::BOOL)
        };
        quals.push(permissive, qual);
        Ok(())
    }

    /// `add_with_check_options`: the checks of the policies of the table `relid` for the command `cmd` that apply to `role`. The check of the permissive policies, which `OR` joins in the reverse order of their names, comes first and has no policy name. Then each restrictive policy has a check with its name, in the order of the names. Each policy checks its condition of `WITH CHECK`, or of `USING` when it has none or when `using` is true. With no permissive policy, the only check is false.
    fn add_checks(
        &mut self,
        relid: u32,
        cmd: u8,
        role: u32,
        using: bool,
        checks: &mut Checks,
    ) -> Result<()> {
        let Some(catalog) = self.env.catalog() else { return Ok(()) };
        let mut policies: Vec<&Policy> = catalog
            .policies()
            .filter(|p| p.relation == relid && (p.cmd == b'*' || p.cmd == cmd))
            .filter(|p| self.security.applies(&p.roles, role))
            .collect();
        policies.sort_by(|a, b| a.name.cmp(&b.name));
        // `QUAL_FOR_WCO`: the condition of `WITH CHECK`, or else of `USING`.
        let source = |p: &Policy| match (&p.check, &p.qual) {
            (Some(_), _) if !using => Some(true),
            (_, Some(_)) => Some(false),
            _ => None,
        };
        let permissive: Vec<(u32, bool)> = policies
            .iter()
            .rev()
            .filter(|p| p.permissive)
            .filter_map(|p| source(p).map(|c| (p.oid, c)))
            .collect();
        let restrictive: Vec<(String, u32, bool)> = policies
            .iter()
            .filter(|p| !p.permissive)
            .filter_map(|p| source(p).map(|c| (p.name.clone(), p.oid, c)))
            .collect();
        if permissive.is_empty() {
            let deny = Expr::constant(Value::Bool(false), oid::BOOL, -1, None);
            checks.checks.push((None, deny));
            checks.keys.push((None, Vec::new()));
            return Ok(());
        }
        let mut parts = Vec::with_capacity(permissive.len());
        for &(policy, check) in &permissive {
            parts.extend(self.policy_qual(policy, check)?);
        }
        let qual = if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::new(ExprKind::Bool(BoolOp::Or, parts), oid::BOOL)
        };
        checks.push((None, permissive), qual);
        for (name, policy, check) in restrictive {
            if let Some(qual) = self.policy_qual(policy, check)? {
                checks.push((Some(name), vec![(policy, check)]), qual);
            }
        }
        Ok(())
    }

    /// The condition of `USING` of the policy, or of `WITH CHECK` when `check` is true, which reads the relation 0. The privileges and the functions of its subqueries go after those of the statement.
    fn policy_qual(&mut self, policy: u32, check: bool) -> Result<Option<Expr>> {
        let Some((base, mut qual)) = crate::policy_tree(self.env, policy, check)? else {
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

/// The number of the parts of `AND` of a condition, as the executor splits a filter into its conditions.
fn conjuncts(expr: &Expr) -> usize {
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.iter().map(conjuncts).sum(),
        _ => 1,
    }
}

/// The `securityQuals` `quals` of the target table of a write, which is the relation `index` of `query`, at the start of the condition of the query. Each condition has its own security level, before the levels of the views of the query.
fn add_target_quals(query: &mut Query, index: usize, mut quals: Vec<Expr>) {
    renumber(&mut quals, index);
    let mut barrier: Vec<usize> = quals.iter().map(conjuncts).collect();
    barrier.append(&mut query.barrier);
    query.barrier = barrier;
    quals.extend(query.filter.take());
    query.filter = match quals.len() {
        1 => quals.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, quals), oid::BOOL)),
    };
}

/// The conditions `quals`, which read the relation 0, with the relation at the place `index` in its place.
fn renumber(quals: &mut [Expr], index: usize) {
    if index == 0 {
        return;
    }
    for qual in quals {
        qual.each_mut(0, &mut |e, depth| {
            if let ExprKind::Var(var) = &mut e.kind
                && var.levels_up == depth
                && var.relation == 0
            {
                var.relation = index;
            }
            false
        });
    }
}

/// `transform_MERGE_to_join` with the security quals `quals` of the target table at the place `index`: the target table joins a relation with one row on the conditions, so they limit the rows of the target table before the join with the source, and the source row of a row that they hide has no match.
fn add_merge_quals(query: &mut Query, index: usize, mut quals: Vec<Expr>) {
    renumber(&mut quals, index);
    let one = query.relations.len();
    let column = Column { name: "one".to_string(), ty: oid::BOOL, typmod: -1, not_null: false };
    let value = Expr::constant(Value::Bool(true), oid::BOOL, -1, None);
    let mut relation = Relation::new(0, vec![column], None, None, Some(vec![vec![value]]));
    relation.name = "*VALUES*".to_string();
    query.relations.push(relation);
    let mut on = match quals.len() {
        1 => quals.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, quals), oid::BOOL)),
    };
    for item in &mut query.from {
        if replace_relation(item, index, one, &mut on) {
            break;
        }
    }
}

/// Puts the join of the relation `index` with the relation `one` on the condition `on` in the place of the relation `index` in the tree `item`. True when the tree has the relation.
fn replace_relation(item: &mut FromItem, index: usize, one: usize, on: &mut Option<Expr>) -> bool {
    match item {
        FromItem::Relation(at) if *at == index => {
            *item = crate::merge::join(
                crate::from::JoinKind::Inner,
                FromItem::Relation(index),
                FromItem::Relation(one),
                on.take(),
            );
            true
        }
        FromItem::Join(join) => {
            replace_relation(&mut join.left, index, one, on)
                || replace_relation(&mut join.right, index, one, on)
        }
        _ => false,
    }
}

/// The table `relation` as a subquery that reads the table and keeps the rows that pass the conditions `quals`, in their order. Each column of the subquery is the column of the table with the same number, and a dropped column is a null.
fn wrap(relation: &mut Relation, quals: Vec<Expr>, system: bool) {
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
        .collect::<Vec<_>>();
    // A row of a table has the ID of its version and then the OID of its table after its columns, and the subquery gives them in the same places, so that the query reads the system columns of the table from the rows of the subquery.
    let mut targets = targets;
    if system {
        for (name, attnum, ty) in
            [("ctid", CTID_ATTNUM, oid::TID), ("tableoid", TABLE_OID_ATTNUM, oid::OID)]
        {
            let expr = Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), ty);
            targets.push(Target { name: name.to_string(), expr, origin: None, junk: false });
        }
    }
    let barrier = quals.iter().map(conjuncts).collect();
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

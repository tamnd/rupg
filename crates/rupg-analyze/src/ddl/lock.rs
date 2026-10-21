//! `LOCK TABLE`, as `LockTableCommand` in `lockcmds.c` runs it: the checks of each relation of the statement and of the relations that a view reads. rupg takes no lock on a relation. A query of rupg reads the catalog of the last commit and waits for no lock, and no statement of rupg changes the rows of a table, so the modes up to `ROW EXCLUSIVE` have no effect on another session. The session runs these modes as checks only. A stronger mode gives `0A000`.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{LockStmt, Node, RangeVar};

use super::relkind_detail;
use super::view::parse_bool;
use crate::expr::{Expr, ExprKind};
use crate::{Analyzer, Env, Params, Query};

/// The names of the lock modes, from `AccessShareLock`, which is 1.
const MODES: [&str; 8] = [
    "ACCESS SHARE",
    "ROW SHARE",
    "ROW EXCLUSIVE",
    "SHARE UPDATE EXCLUSIVE",
    "SHARE",
    "SHARE ROW EXCLUSIVE",
    "EXCLUSIVE",
    "ACCESS EXCLUSIVE",
];

/// `RowExclusiveLock`, the strongest mode that rupg runs.
const ROW_EXCLUSIVE: i32 = 3;

/// The privilege check of `LOCK TABLE`. `allowed(relid, role)` is `LockTableAclCheck` for the mode of the statement: true when the role can lock the relation.
pub type Allowed<'a> = dyn FnMut(u32, u32) -> Result<bool> + 'a;

/// `LockTableCommand` for the role `user`. The caller does `RequireTransactionBlock` first.
///
/// # Errors
///
/// `0A000` for a mode stronger than `ROW EXCLUSIVE`, the errors of `RangeVarGetRelid` for a name, `42809` for a relation that is not a table or a view, and `42501` for a relation that the role cannot lock. For a view, the owner of the view, or the current user when the view has `security_invoker`, must be able to lock each table and view that the query of the view reads.
pub fn lock_table(
    stmt: &LockStmt,
    env: &dyn Env,
    user: u32,
    allowed: &mut Allowed<'_>,
) -> Result<()> {
    if stmt.mode > ROW_EXCLUSIVE {
        let name = usize::try_from(stmt.mode - 1).ok().and_then(|i| MODES.get(i)).copied();
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("LOCK TABLE in {} mode is not supported yet", name.unwrap_or("this")),
        ));
    }
    let params = Params::default();
    let an = Analyzer::new(env, &params);
    let mut locker = Locker { an, allowed, ancestors: Vec::new() };
    for node in stmt.relations.iter().flatten() {
        let Node::RangeVar(rv) = node else { continue };
        let relid = lookup(&locker.an, rv)?;
        let name = rv.relname.as_deref().unwrap_or_default();
        let kind = relkind(env, relid);
        if !matches!(kind, b'r' | b'p' | b'v') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot lock relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        if !(locker.allowed)(relid, user)? {
            return Err(denied(kind, name));
        }
        if kind == b'v' {
            locker.view(relid, user)?;
        }
    }
    Ok(())
}

/// `RangeVarGetRelid` with no `missing_ok`. The errors have no position, as in PostgreSQL.
pub(super) fn lookup(an: &Analyzer<'_>, rv: &RangeVar) -> Result<u32> {
    let found = an.lookup_relation(rv).map_err(|error| error.with_position(None))?;
    if let Some(relid) = found {
        return Ok(relid);
    }
    let name = rv.relname.as_deref().unwrap_or_default();
    Err(match rv.schemaname.as_deref() {
        Some(schema) if an.schema(schema).is_none() => {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        }
        Some(schema) => Error::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{schema}.{name}\" does not exist"),
        ),
        None => {
            Error::new(SqlState::UNDEFINED_TABLE, format!("relation \"{name}\" does not exist"))
        }
    })
}

/// The state of one `LOCK TABLE`.
struct Locker<'a, 'f> {
    an: Analyzer<'a>,
    allowed: &'f mut Allowed<'f>,
    /// `ancestor_views`: the views that the walk is in.
    ancestors: Vec<u32>,
}

impl Locker<'_, '_> {
    /// `LockViewRecurse` for a view of the catalog. A built-in view has the bootstrap superuser as its owner and no `security_invoker`, so each check in its query passes, and the walk skips it.
    fn view(&mut self, relid: u32, user: u32) -> Result<()> {
        let Some(query) = self.an.view_query(relid)? else { return Ok(()) };
        self.view_query(relid, &query, user)
    }

    /// The walk of the query of the view `relid`, with the checks as the owner of the view or as `user`.
    fn view_query(&mut self, relid: u32, query: &Query, user: u32) -> Result<()> {
        let rel = self.an.env.catalog().and_then(|c| c.relation(relid));
        let invoker = rel.is_some_and(|r| {
            r.options
                .iter()
                .filter_map(|o| o.strip_prefix("security_invoker="))
                .next_back()
                .and_then(parse_bool)
                .unwrap_or(false)
        });
        let check_as = match rel {
            Some(rel) if !invoker => rel.owner,
            _ => user,
        };
        self.ancestors.push(relid);
        let result = self.walk(query, check_as, user);
        self.ancestors.pop();
        result
    }

    /// `LockViewRecurse_walker` for a query: the tables and the views of the query, then the subqueries in the expressions, in `WITH` and in `FROM`.
    fn walk(&mut self, query: &Query, check_as: u32, user: u32) -> Result<()> {
        for relation in &query.relations {
            if relation.oid == 0 || relation.cte.is_some() {
                continue;
            }
            let kind = relkind(self.an.env, relation.oid);
            if !matches!(kind, b'r' | b'p' | b'v') || self.ancestors.contains(&relation.oid) {
                continue;
            }
            if !(self.allowed)(relation.oid, check_as)? {
                return Err(denied(kind, &class_name(self.an.env, relation.oid)));
            }
            if let Some(sub) = relation.subquery.as_deref()
                && kind == b'v'
            {
                self.view_query(relation.oid, sub, user)?;
            }
        }
        for expr in query.exprs() {
            self.walk_expr(expr, check_as, user)?;
        }
        for cte in &query.ctes {
            self.walk(&cte.query, check_as, user)?;
        }
        for relation in &query.relations {
            if relation.oid == 0
                && relation.cte.is_none()
                && let Some(sub) = relation.subquery.as_deref()
            {
                self.walk(sub, check_as, user)?;
            }
        }
        Ok(())
    }

    /// The subqueries of `SubLink` in an expression.
    fn walk_expr(&mut self, expr: &Expr, check_as: u32, user: u32) -> Result<()> {
        if let ExprKind::SubLink(sub) = &expr.kind {
            self.walk(&sub.query, check_as, user)?;
        }
        for child in expr.children() {
            self.walk_expr(child, check_as, user)?;
        }
        Ok(())
    }
}

/// The `relkind` of a relation of the catalog or of the built-in catalog.
pub(crate) fn relkind(env: &dyn Env, relid: u32) -> u8 {
    match env.catalog().and_then(|c| c.relation(relid)) {
        Some(rel) => match rel.kind {
            RelKind::Table => b'r',
            RelKind::Index => b'i',
            RelKind::Sequence => b'S',
            RelKind::Toast => b't',
            RelKind::View => b'v',
            RelKind::Composite => b'c',
            RelKind::Partitioned => b'p',
        },
        None => builtin::class_by_oid(relid).map_or(0, |row| row.kind),
    }
}

/// The name of a relation of the catalog or of the built-in catalog.
pub(super) fn class_name(env: &dyn Env, relid: u32) -> String {
    match env.catalog().and_then(|c| c.relation(relid)) {
        Some(rel) => rel.name.clone(),
        None => builtin::named_by_oid(Named::Class, relid)
            .map_or_else(String::new, |n| n.name.to_string()),
    }
}

/// `aclcheck_error` with `ACLCHECK_NO_PRIV` for a table or a view.
fn denied(kind: u8, name: &str) -> Error {
    let object = if kind == b'v' { "view" } else { "table" };
    Error::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied for {object} {name}"))
}

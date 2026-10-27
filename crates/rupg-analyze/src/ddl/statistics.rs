//! The extended statistics objects: `CREATE STATISTICS` and `ALTER STATISTICS ... SET STATISTICS`, as `transformStatsStmt` of `parse_utilcmd.c`, `CreateStatistics` and `AlterStatistics` of `statscmds.c` run them, and the statistics objects of `DROP`, `COMMENT ON`, `RENAME TO`, `OWNER TO` and `SET SCHEMA`. The objects are found by name as `get_statistics_object_oid` of `namespace.c` finds them, and a name without a schema does not find an object of the temporary schema.

use rupg_catalog::names::{make_object_name, name_addition};
use rupg_catalog::{ObjRef, PG_STATISTIC_EXT, StatExt};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    AlterObjectSchemaStmt, AlterOwnerStmt, AlterStatsStmt, CreateStatsStmt, DropStmt, Node,
    RenameStmt,
};

use super::attmap::AttMap;
use super::comment::{name_list, not_owner};
use super::drop::{owner_word, quote_identifier};
use super::index::{BTREE, system_column};
use super::{Definer, Found, PG_TOAST_NAMESPACE, not_yet, references, relkind_detail, relname};
use crate::agg::Kind;
use crate::expr::{Expr, ExprKind};
use crate::typcache::default_opclass;
use crate::typename::names;

/// `STATS_MAX_DIMENSIONS`: the most columns and expressions of a statistics object.
const STATS_MAX_DIMENSIONS: usize = 8;
/// `MAX_STATISTICS_TARGET`.
const MAX_STATISTICS_TARGET: i32 = 10000;

/// The error of a statistics object on a system column.
fn system_columns() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "statistics creation on system columns is not supported",
    )
}

/// The error of the duplicate check of `report_namespace_conflict` for a statistics object.
fn conflict(name: &str, schema: &str) -> Error {
    Error::new(
        SqlState::DUPLICATE_OBJECT,
        format!("statistics object \"{name}\" already exists in schema \"{schema}\""),
    )
}

impl Definer<'_, '_> {
    /// The check of `lookup_type_cache` with `TYPECACHE_LT_OPR` for a column or an expression of a statistics object with more than one column: the type must have a default btree operator class. `what` names the column or the expression in the error.
    fn check_stats_type(&self, ty: u32, what: &str) -> Result<()> {
        if default_opclass(ty, BTREE).is_some() {
            return Ok(());
        }
        Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cannot create multivariate statistics on {what}"),
        )
        .with_detail(format!(
            "The type {} has no default btree operator class.",
            self.type_text(ty)
        )))
    }

    /// `QualifiedNameGetCreationNamespace` for a statistics object: the schema and the name. The schema `pg_temp` is the temporary schema of the session, which the first temporary object makes.
    fn stats_creation<'n>(&mut self, list: &[&'n str]) -> Result<(u32, &'n str)> {
        match list {
            ["pg_temp", name] => Ok((self.temp_namespace()?, name)),
            _ => self.ts_creation(list),
        }
    }

    /// `transformStatsStmt` and `CreateStatistics`.
    pub(super) fn create_statistics(&mut self, stmt: &CreateStatsStmt) -> Result<()> {
        if stmt.relations.len() != 1 {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "only a single relation is allowed in CREATE STATISTICS",
            ));
        }
        let Some(Some(Node::RangeVar(rv))) = stmt.relations.first() else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "CREATE STATISTICS only supports relation names in the FROM clause",
            ));
        };
        let found = self.lookup_relation(rv)?;
        let elems: Vec<_> = stmt
            .exprs
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::StatsElem(elem) => Some(&**elem),
                _ => None,
            })
            .collect();
        let table = match found {
            Found::User(oid) => oid,
            Found::Builtin(_) => 0,
        };
        // `transformStatsStmt`: the expressions see the columns of the relation.
        self.an.scope = crate::from::Scope::default();
        if let Some(rel) = self.catalog.relation(table) {
            let columns: Vec<(String, u32, i32)> =
                rel.columns.iter().map(crate::from::scope_column).collect();
            let relname = rel.name.clone();
            let row_type = rel.row_type;
            self.an.add_table(table, &relname, &columns, row_type);
        }
        let mut transformed = Vec::with_capacity(elems.len());
        for elem in &elems {
            let expr = match &elem.expr {
                Some(_) if table == 0 => {
                    return Err(not_yet(
                        "an expression of statistics on a built-in relation",
                        None,
                    ));
                }
                Some(node) => {
                    Some(self.an.with_kind(Kind::StatsExpression, |an| an.transform(Some(node)))?)
                }
                None => None,
            };
            transformed.push((elem.name.as_deref(), expr));
        }
        let kind = self.kind_of(found);
        let name = relname(rv).to_string();
        if !matches!(kind, b'r' | b'm' | b'f' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot define statistics for relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let rel_owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && !self.owns(rel_owner) {
            return Err(not_owner(owner_word(kind), &name));
        }
        if self.is_system(found) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let Some(rel) = self.catalog.relation(table).cloned() else {
            return Err(not_yet("statistics on a built-in relation", None));
        };
        let list = names(&stmt.defnames);
        let (namespace, stat_name) = if list.is_empty() {
            let names = elems.iter().map(|e| e.name.as_deref().unwrap_or("expr"));
            (rel.namespace, self.choose_stats_name(&rel.name, rel.namespace, names))
        } else {
            let (namespace, name) = self.stats_creation(&list)?;
            (namespace, name.to_string())
        };
        if namespace != self.an.temp {
            self.check_create_in(namespace, None)?;
        }
        if self.catalog.stat_ext_by_name(namespace, &stat_name).is_some() {
            if stmt.if_not_exists {
                self.notice(
                    SqlState::DUPLICATE_OBJECT,
                    format!("statistics object \"{stat_name}\" already exists, skipping"),
                );
                return Ok(());
            }
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("statistics object \"{stat_name}\" already exists"),
            ));
        }
        let numcols = transformed.len();
        if numcols > STATS_MAX_DIMENSIONS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("cannot have more than {STATS_MAX_DIMENSIONS} columns in statistics"),
            ));
        }
        let mut keys: Vec<i16> = Vec::new();
        let mut exprs: Vec<Expr> = Vec::new();
        for (column, expr) in transformed {
            match (column, expr) {
                (Some(column), _) => {
                    let at = rel.columns.iter().position(|c| !c.dropped && c.name == column);
                    let Some(at) = at else {
                        if system_column(column).is_some() {
                            return Err(system_columns());
                        }
                        return Err(Error::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{column}\" does not exist"),
                        ));
                    };
                    if numcols > 1 {
                        self.check_stats_type(rel.columns[at].ty, &format!("column \"{column}\""))?;
                    }
                    keys.push(i16::try_from(at + 1).unwrap_or(i16::MAX));
                }
                (None, Some(expr)) => {
                    if let ExprKind::Var(var) = &expr.kind {
                        if var.attnum <= 0 {
                            return Err(system_columns());
                        }
                        if numcols > 1 {
                            let column = rel.column(var.attnum).map_or("", |c| c.name.as_str());
                            self.check_stats_type(expr.ty, &format!("column \"{column}\""))?;
                        }
                        keys.push(var.attnum);
                        continue;
                    }
                    let system = expr.find(0, &mut |e, _| match &e.kind {
                        ExprKind::Var(var) if var.attnum <= 0 => Some(()),
                        _ => None,
                    });
                    if system.is_some() {
                        return Err(system_columns());
                    }
                    if numcols > 1 {
                        self.check_stats_type(expr.ty, "this expression")?;
                    }
                    exprs.push(expr);
                }
                (None, None) => return Err(Error::internal("a statistics element with no column")),
            }
        }
        if numcols < 2 && exprs.len() != 1 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "cannot create extended statistics on a single non-virtual column",
            )
            .with_detail(
                "Univariate statistics are already built for each individual non-virtual table column.",
            ));
        }
        if numcols == 1 && !stmt.stat_types.is_empty() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot specify statistics kinds when building univariate statistics",
            ));
        }
        let (mut ndistinct, mut dependencies, mut mcv) = (false, false, false);
        for kind in names(&stmt.stat_types) {
            match kind {
                "ndistinct" => ndistinct = true,
                "dependencies" => dependencies = true,
                "mcv" => mcv = true,
                _ => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized statistics kind \"{kind}\""),
                    ));
                }
            }
        }
        if stmt.stat_types.is_empty() && numcols >= 2 {
            (ndistinct, dependencies, mcv) = (true, true, true);
        }
        keys.sort_unstable();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                "duplicate column name in statistics definition",
            ));
        }
        if exprs.iter().any(|a| exprs.iter().filter(|b| a.same(b)).count() > 1) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                "duplicate expression in statistics definition",
            ));
        }
        let kinds: Vec<u8> = [(ndistinct, b'd'), (dependencies, b'f'), (mcv, b'm')]
            .into_iter()
            .chain([(!exprs.is_empty(), b'e')])
            .filter_map(|(on, code)| on.then_some(code))
            .collect();
        let refs = if exprs.is_empty() {
            Vec::new()
        } else {
            references(self.catalog, table, &exprs.iter().collect::<Vec<_>>())?
        };
        self.check_usage_on_types(&refs)?;
        let text = if exprs.is_empty() { None } else { Some(crate::node::write_list(&exprs)?) };
        let new = StatExt {
            oid: 0,
            name: stat_name,
            namespace,
            owner: if stmt.owner != 0 { stmt.owner } else { self.user },
            relation: table,
            keys,
            kinds,
            target: None,
            exprs: text,
        };
        self.catalog.create_stat_ext(new, &refs)?;
        Ok(())
    }

    /// `ChooseExtendedStatisticName` with the names of `ChooseExtendedStatisticNameAddition`: a name for a statistics object of the relation `relname` in the schema `namespace` that no other statistics object there has. `names` gives the names of the columns, with `expr` for each expression.
    fn choose_stats_name<'n>(
        &self,
        relname: &str,
        namespace: u32,
        names: impl IntoIterator<Item = &'n str>,
    ) -> String {
        let addition = name_addition(names);
        let mut label = "stat".to_string();
        let mut pass = 0;
        loop {
            let name = make_object_name(relname, Some(&addition), Some(&label));
            if self.catalog.stat_ext_by_name(namespace, &name).is_none() {
                return name;
            }
            pass += 1;
            label = format!("stat{pass}");
        }
    }

    /// `generateClonedExtStatsStmt` and `CreateStatistics` for `LIKE ... INCLUDING STATISTICS`: a copy of the statistics object `stat` for the new table `table`, with the column numbers of `map`, the kinds of the object and a name that [`Self::choose_stats_name`] gives. With `comments`, the copy gets the comment of the object.
    pub(super) fn clone_statistics(
        &mut self,
        stat: u32,
        table: u32,
        map: &AttMap,
        comments: bool,
    ) -> Result<()> {
        let (Some(old), Some(rel)) =
            (self.catalog.stat_ext(stat).cloned(), self.catalog.relation(table).cloned())
        else {
            return Ok(());
        };
        let mut keys = map.columns(&old.keys);
        keys.sort_unstable();
        let text = old.exprs.as_deref().map(|t| map.list_text(t)).transpose()?;
        let exprs = match &text {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        };
        let columns = keys.iter().map(|&k| rel.column(k).map_or("", |c| c.name.as_str()));
        let names: Vec<&str> = columns.chain(exprs.iter().map(|_| "expr")).collect();
        let name = self.choose_stats_name(&rel.name, rel.namespace, names);
        let refs = if exprs.is_empty() {
            Vec::new()
        } else {
            references(self.catalog, table, &exprs.iter().collect::<Vec<_>>())?
        };
        let new = StatExt {
            oid: 0,
            name,
            namespace: rel.namespace,
            owner: self.user,
            relation: table,
            keys,
            kinds: old.kinds,
            target: None,
            exprs: text,
        };
        let oid = self.catalog.create_stat_ext(new, &refs)?;
        if comments
            && let Some(text) =
                self.catalog.description(ObjRef::new(PG_STATISTIC_EXT, stat)).map(str::to_string)
        {
            self.catalog.set_description(ObjRef::new(PG_STATISTIC_EXT, oid), Some(text));
        }
        Ok(())
    }

    /// `get_statistics_object_oid`: the statistics object that a name gives. A name without a schema looks in the schemas of the path but not in the temporary schema. With `missing_ok`, a name that gives no object, or a schema that does not exist, is `None`.
    fn stats_lookup(&self, list: &[&str], missing_ok: bool) -> Result<Option<u32>> {
        let (schema, name) = match self.an.split_name(list, None) {
            Ok(split) => split,
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let found = match schema {
            Some(namespace) => self.catalog.stat_ext_by_name(namespace, name),
            None => self.an.path.iter().find_map(|&ns| self.catalog.stat_ext_by_name(ns, name)),
        };
        if found.is_none() && !missing_ok {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("statistics object \"{}\" does not exist", list.join(".")),
            ));
        }
        Ok(found.map(|s| s.oid))
    }

    /// `get_statistics_object_oid` with `missing_ok` false.
    pub(super) fn stats_oid(&self, list: &[&str]) -> Result<u32> {
        Ok(self.stats_lookup(list, false)?.unwrap_or_default())
    }

    /// The row of the statistics object that a name gives, which must exist.
    fn stats_row(&self, object: Option<&Node>) -> Result<StatExt> {
        let list = name_list(object);
        let oid = self.stats_lookup(&list, false)?.unwrap_or_default();
        self.catalog.stat_ext(oid).cloned().ok_or_else(|| Error::internal("no statistics object"))
    }

    /// `AlterStatistics`: `ALTER STATISTICS ... SET STATISTICS`. The target -1 and `DEFAULT` give the default, and a target over `MAX_STATISTICS_TARGET` gives a `WARNING` and the most.
    pub(super) fn alter_statistics(&mut self, stmt: &AlterStatsStmt) -> Result<()> {
        let target = match &stmt.stxstattarget {
            Some(Node::Integer(n)) if *n != -1 => Some(*n),
            _ => None,
        };
        let target = match target {
            Some(n) if n < 0 => {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("statistics target {n} is too low"),
                ));
            }
            Some(n) if n > MAX_STATISTICS_TARGET => {
                self.warning(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("lowering statistics target to {MAX_STATISTICS_TARGET}"),
                );
                Some(MAX_STATISTICS_TARGET)
            }
            other => other,
        };
        let list = names(&stmt.defnames);
        let Some(oid) = self.stats_lookup(&list, stmt.missing_ok)? else {
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("statistics object \"{}\" does not exist, skipping", list.join(".")),
            );
            return Ok(());
        };
        let owner = self.catalog.stat_ext(oid).map_or(0, |s| s.owner);
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner("statistics object", &list.join(".")));
        }
        let target = target.and_then(|n| i16::try_from(n).ok());
        self.catalog.alter_stat_ext(oid, |s| s.target = target);
        Ok(())
    }

    /// The statistics objects of `DROP STATISTICS`, as `RemoveObjects` finds them, with the check of `check_object_ownership`. The owner of the schema of an object can also remove it. A name that gives no object gives the notice of `does_not_exist_skipping` with `IF EXISTS`.
    pub(super) fn find_stats(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let list = name_list(Some(item));
            let Some(found) = self.stats_lookup(&list, stmt.missing_ok)? else {
                let schema = match list.as_slice() {
                    [schema, _] | [_, schema, _] => Some(*schema),
                    _ => None,
                };
                let message = match schema {
                    Some(schema) if self.an.schema(schema).is_none() => {
                        format!("schema \"{schema}\" does not exist, skipping")
                    }
                    _ => {
                        format!("statistics object \"{}\" does not exist, skipping", list.join("."))
                    }
                };
                self.notice(SqlState::SUCCESSFUL_COMPLETION, message);
                continue;
            };
            let (namespace, owner) =
                self.catalog.stat_ext(found).map_or((0, 0), |s| (s.namespace, s.owner));
            if !self.is_superuser()
                && !self.schema_owner(namespace).is_some_and(|o| self.owns(o))
                && !self.owns(owner)
            {
                return Err(not_owner("statistics object", &list.join(".")));
            }
            objects.push(ObjRef::new(PG_STATISTIC_EXT, found));
        }
        Ok(objects)
    }

    /// The object of `COMMENT ON STATISTICS`, after the check of its owner.
    pub(super) fn comment_stats(&self, object: Option<&Node>) -> Result<ObjRef> {
        let stat = self.stats_row(object)?;
        if !self.is_superuser() && !self.owns(stat.owner) {
            return Err(not_owner("statistics object", &name_list(object).join(".")));
        }
        Ok(ObjRef::new(PG_STATISTIC_EXT, stat.oid))
    }

    /// `AlterObjectRename_internal` for `ALTER STATISTICS ... RENAME TO`. A user who is not a superuser must own the object and be able to create in its schema.
    pub(super) fn rename_stats(&mut self, stmt: &RenameStmt) -> Result<()> {
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let stat = self.stats_row(stmt.object.as_ref())?;
        if !self.is_superuser() {
            if !self.owns(stat.owner) {
                return Err(not_owner("statistics object", &stat.name));
            }
            self.check_create_in(stat.namespace, None)?;
        }
        if self.catalog.stat_ext_by_name(stat.namespace, newname).is_some() {
            return Err(conflict(newname, &self.schema_name(stat.namespace)));
        }
        self.catalog.alter_stat_ext(stat.oid, |s| s.name = newname.to_string());
        Ok(())
    }

    /// `AlterObjectOwner_internal` for `ALTER STATISTICS ... OWNER TO`. A user who is not a superuser must own the object and be able to become the new owner, and the new owner must be able to create in the schema of the object.
    pub(super) fn alter_stats_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let stat = self.stats_row(stmt.object.as_ref())?;
        if stat.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if !self.owns(stat.owner) {
                return Err(not_owner("statistics object", &stat.name));
            }
            self.check_can_set_role(new)?;
            if !self.may_create_in(stat.namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(stat.namespace)),
                ));
            }
        }
        self.catalog.alter_stat_ext(stat.oid, |s| s.owner = new);
        Ok(())
    }

    /// `ExecAlterObjectSchemaStmt` and `AlterObjectNamespace_internal` for `ALTER STATISTICS ... SET SCHEMA`: `LookupCreationNamespace` and `CheckSetNamespace` for the new schema, the checks of the owner and of the name, and then the move. An object that is in the schema already needs no change.
    pub(super) fn alter_stats_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let stat = self.stats_row(stmt.object.as_ref())?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        if schema == "pg_temp" || self.catalog.is_temp_schema(stat.namespace) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        let namespace = self.an.schema(schema).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        })?;
        self.check_create_in(namespace, None)?;
        if stat.namespace == namespace {
            return Ok(());
        }
        if self.catalog.is_temp_schema(namespace) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if namespace == PG_TOAST_NAMESPACE || stat.namespace == PG_TOAST_NAMESPACE {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        if !self.is_superuser() && !self.owns(stat.owner) {
            return Err(not_owner("statistics object", &stat.name));
        }
        if self.catalog.stat_ext_by_name(namespace, &stat.name).is_some() {
            return Err(conflict(&stat.name, &self.schema_name(namespace)));
        }
        self.catalog.set_stat_ext_namespace(stat.oid, namespace);
        Ok(())
    }

    /// `getObjectDescription` for a statistics object: `statistics object` and its name, after its schema when `StatisticsObjIsVisible` is false. The temporary schema is not in the path for this check.
    pub(super) fn describe_stats(&self, oid: u32) -> String {
        let Some(stat) = self.catalog.stat_ext(oid) else {
            return format!("statistics object {oid}");
        };
        let first = self.an.path.iter().copied().find(|&ns| {
            ns == stat.namespace || self.catalog.stat_ext_by_name(ns, &stat.name).is_some()
        });
        if first == Some(stat.namespace) {
            return format!("statistics object {}", quote_identifier(&stat.name));
        }
        format!(
            "statistics object {}.{}",
            quote_identifier(&self.schema_name(stat.namespace)),
            quote_identifier(&stat.name)
        )
    }
}

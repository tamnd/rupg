//! The relation of `COPY`, as `DoCopy`, `CopyGetAttnums` and `BeginCopyTo` in `copy.c` and `copyto.c` check it: the name, the columns and the kind of the relation. The session reads the rows with a query of the columns, as `DoCopy` does for a table with row security.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    Alias, ColumnRef, CopyStmt, InsertStmt, Node, OverridingKind, ParamRef, RangeSubselect,
    RangeVar, ResTarget, SelectStmt,
};

use super::lock::{lookup, relkind};
use super::relkind_detail;
use crate::walk::{self, Walker};
use crate::{Analyzer, Env, Params};

/// The relation of a `COPY ... TO` and the query that reads its rows.
#[derive(Debug)]
pub struct CopyRelation {
    /// The name of the relation, for the errors.
    pub name: String,
    /// The names of the user columns of the relation, in the order of the catalog.
    pub columns: Vec<String>,
    /// The columns that `COPY` sends, as indexes into `columns`.
    pub selected: Vec<usize>,
    /// `SELECT` of the selected columns `FROM ONLY` the relation. For a partitioned table, the query reads the partitions, and `tableoid` comes after the selected columns.
    pub query: Node,
    /// The partitions of a partitioned table that have rows, in the order of `find_all_inheritors`. `DoCopyTo` sends the rows of each partition in this order.
    pub partitions: Option<Vec<u32>>,
}

/// The table of a `COPY ... FROM`, as `DoCopy` and `BeginCopyFrom` open it.
#[derive(Debug, Clone)]
pub struct CopyTarget {
    /// The OID of the relation.
    pub relid: u32,
    /// The name of the relation, for the errors.
    pub name: String,
    /// `relkind`. `CopyFrom` checks it after `CopyInResponse`.
    pub kind: u8,
    /// The names of the user columns of the relation, in the order of the catalog.
    pub columns: Vec<String>,
    /// The columns that the data gives, as indexes into `columns`.
    pub selected: Vec<usize>,
}

/// The checks of `table_openrv` and `CopyGetAttnums` for the relation of `COPY`.
///
/// # Errors
///
/// The errors of `RangeVarGetRelid` for the name, `42809` for an index or a composite type, `42703` and `42701` for the list of columns.
pub fn copy_target(stmt: &CopyStmt, rv: &RangeVar, env: &dyn Env) -> Result<CopyTarget> {
    let params = Params::default();
    let an = Analyzer::new(env, &params);
    let relid = lookup(&an, rv)?;
    let name = rv.relname.as_deref().unwrap_or_default().to_string();
    let kind = relkind(env, relid);
    if matches!(kind, b'i' | b'I' | b'c') {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot open relation \"{name}\""),
        )
        .with_detail(relkind_detail(kind)));
    }
    let columns = relation_columns(env, relid);
    let names: Vec<String> =
        stmt.attlist.iter().flatten().filter_map(string).map(str::to_string).collect();
    let mut selected = attnums(&columns, &names, Some(&name))?;
    // `CopyGetAttnums`: the default list has no generated column, and the list of the statement cannot name one.
    let generated: Vec<bool> =
        env.catalog().and_then(|c| c.relation(relid)).map_or_else(Vec::new, |rel| {
            rel.columns.iter().filter(|c| !c.dropped).map(|c| c.generated != 0).collect()
        });
    let is_generated = |i: usize| generated.get(i).copied().unwrap_or(false);
    if let Some(condition) = &stmt.whereClause {
        let mut refs = WhereColumns { relname: &name, columns: &columns, used: Vec::new() };
        walk::one(&mut refs, Some(condition));
        let mut used = refs.used;
        used.sort_unstable();
        if let Some(&i) = used.iter().find(|&&i| is_generated(i)) {
            return Err(Error::new(
                SqlState::INVALID_COLUMN_REFERENCE,
                "generated columns are not supported in COPY FROM WHERE conditions",
            )
            .with_detail(format!("Column \"{}\" is a generated column.", columns[i])));
        }
    }
    if names.is_empty() {
        selected.retain(|&i| !is_generated(i));
    } else if let Some(&i) = selected.iter().find(|&&i| is_generated(i)) {
        return Err(Error::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            format!("column \"{}\" is a generated column", columns[i]),
        )
        .with_detail("Generated columns cannot be used in COPY."));
    }
    Ok(CopyTarget { relid, name, kind, columns, selected })
}

/// `pull_varattnos` for the `WHERE` of `COPY ... FROM`, on the raw tree: the columns of the relation that the condition names, as indexes into `columns`. A whole-row reference gives all the columns.
struct WhereColumns<'a> {
    relname: &'a str,
    columns: &'a [String],
    used: Vec<usize>,
}

impl Walker for WhereColumns<'_> {
    fn node(&mut self, node: &Node) -> bool {
        let Node::ColumnRef(cref) = node else { return walk::walk(self, node) };
        let fields: Vec<Option<&str>> = cref.fields.iter().flatten().map(string).collect();
        let position = |name: &str| self.columns.iter().position(|c| c == name);
        match fields.as_slice() {
            [Some(name)] if position(name).is_some() => self.used.extend(position(name)),
            [Some(rel)] | [Some(rel), None] if *rel == self.relname => {
                self.used.extend(0..self.columns.len());
            }
            [Some(rel), Some(name)] if *rel == self.relname => self.used.extend(position(name)),
            _ => {}
        }
        false
    }

    fn select(&mut self, stmt: &SelectStmt) -> bool {
        walk::select(self, stmt)
    }
}

/// The checks of `DoCopy` and `BeginCopyTo` for the relation of `COPY ... TO`: `table_openrv`, `CopyGetAttnums` and the kind of the relation.
///
/// # Errors
///
/// The errors of `RangeVarGetRelid` for the name, `42809` for an index or a relation that is not a table, `42703` and `42701` for the list of columns.
pub fn copy_relation(stmt: &CopyStmt, rv: &RangeVar, env: &dyn Env) -> Result<CopyRelation> {
    let CopyTarget { relid, name, kind, columns, selected } = copy_target(stmt, rv, env)?;
    let mut partitions = None;
    match kind {
        b'r' => {}
        b'p' => partitions = Some(copy_partitions(env, relid, &name)?),
        b'm' => {
            let rel = env.catalog().and_then(|c| c.relation(relid));
            if rel.and_then(|r| r.view.as_ref()).is_some_and(|v| !v.populated) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot copy from unpopulated materialized view \"{name}\""),
                )
                .with_hint("Use the REFRESH MATERIALIZED VIEW command."));
            }
        }
        b'f' => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot copy from foreign table \"{name}\""),
            )
            .with_hint("Try the COPY (SELECT ...) TO variant."));
        }
        b'v' => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot copy from view \"{name}\""),
            )
            .with_hint("Try the COPY (SELECT ...) TO variant."));
        }
        b'S' => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot copy from sequence \"{name}\""),
            ));
        }
        _ => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot copy from non-table relation \"{name}\""),
            ));
        }
    }
    let tableoid = partitions.as_ref().map(|_| "tableoid");
    let targets = selected
        .iter()
        .map(|&i| columns[i].as_str())
        .chain(tableoid)
        .map(|column| {
            let field = Some(Node::String(column.into()));
            let column = ColumnRef { fields: vec![field], location: -1 };
            let target = ResTarget { val: Some(column.into()), location: -1, ..Default::default() };
            Some(target.into())
        })
        .collect();
    let from = RangeVar { inh: partitions.is_some(), alias: None, location: -1, ..rv.clone() };
    let select = SelectStmt {
        targetList: targets,
        fromClause: vec![Some(from.into())],
        ..Default::default()
    };
    Ok(CopyRelation { name, columns, selected, query: select.into(), partitions })
}

/// `find_all_inheritors` of `BeginCopyTo` for the partitioned table `root`: the partitions of each level after the level before it, each level in the order of the OIDs, with no partitioned tables, because they have no rows.
///
/// # Errors
///
/// `42809` for a foreign table in the partitions.
fn copy_partitions(env: &dyn Env, root: u32, name: &str) -> Result<Vec<u32>> {
    let Some(catalog) = env.catalog() else { return Ok(Vec::new()) };
    let mut all = vec![root];
    let mut at = 0;
    while let Some(&oid) = all.get(at) {
        at += 1;
        all.extend(catalog.partitions(oid).iter().map(|rel| rel.oid));
    }
    let mut out = Vec::new();
    for oid in all {
        match relkind(env, oid) {
            b'p' => {}
            b'f' => {
                let child = catalog.relation(oid).map_or("", |rel| rel.name.as_str());
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot copy from foreign table \"{child}\""),
                )
                .with_detail(format!(
                    "Partition \"{child}\" is a foreign table in partitioned table \"{name}\"."
                ))
                .with_hint("Try the COPY (SELECT ...) TO variant."));
            }
            _ => out.push(oid),
        }
    }
    Ok(out)
}

/// The `INSERT` that writes the rows of a `COPY ... FROM`, as `CopyFrom` fills a row: the parameter `$n` is the value of the selected column `n`, and the other columns get their defaults. `defaults` is true for a selected column whose field is the marker of `DEFAULT`. Such a column also gets its default, and the parameters skip it. `OVERRIDING SYSTEM VALUE` lets the data give a value to an identity column `GENERATED ALWAYS`, as `COPY` does.
#[must_use]
pub fn copy_insert(target: &CopyTarget, rv: &RangeVar, defaults: &[bool]) -> Node {
    let cols: Vec<_> = target
        .selected
        .iter()
        .enumerate()
        .filter(|&(k, _)| !defaults.get(k).copied().unwrap_or(false))
        .map(|(_, &i)| {
            let name = Some(target.columns[i].as_str().into());
            Some(ResTarget { name, location: -1, ..Default::default() }.into())
        })
        .collect();
    // With no columns, the statement is `INSERT ... DEFAULT VALUES`.
    let select = (!cols.is_empty()).then(|| values(cols.len()).into());
    let insert = InsertStmt {
        relation: Some(Box::new(RangeVar { alias: None, location: -1, ..rv.clone() })),
        cols,
        selectStmt: select,
        r#override: OverridingKind::OVERRIDING_SYSTEM_VALUE,
        ..Default::default()
    };
    insert.into()
}

/// The query of the condition `WHERE` of a `COPY ... FROM`. The parameter `$n` is the value of the column `n` of the row, and the query gives the value of the condition, as `ExecQual` gives it for the row of `CopyFrom`.
#[must_use]
pub fn copy_filter(target: &CopyTarget, rv: &RangeVar, condition: &Node) -> Node {
    let colnames =
        target.columns.iter().map(|name| Some(Node::String(name.as_str().into()))).collect();
    let alias = Alias { aliasname: rv.relname.clone(), colnames };
    let from = RangeSubselect {
        lateral: false,
        subquery: Some(values(target.columns.len()).into()),
        alias: Some(Box::new(alias)),
    };
    let target = ResTarget { val: Some(condition.clone()), location: -1, ..Default::default() };
    let select = SelectStmt {
        targetList: vec![Some(target.into())],
        fromClause: vec![Some(from.into())],
        ..Default::default()
    };
    select.into()
}

/// The `INSERT` that writes a row of a table again after `ALTER COLUMN ... TYPE`, as `ATRewriteTable` evaluates `tab->newvals`: the parameter `$n` is the old value of the selected column `n`, and `transforms` gives the expression of `USING` of a selected column, or `None` for a column that keeps its old value. The expressions refer to the columns by their names, through `FROM (VALUES ($1, ...)) AS relname(columns)`. The other columns get their defaults.
#[must_use]
pub fn rewrite_insert(target: &CopyTarget, rv: &RangeVar, transforms: &[Option<&Node>]) -> Node {
    let names: Vec<&str> = target.selected.iter().map(|&i| target.columns[i].as_str()).collect();
    let cols = names
        .iter()
        .map(|&name| {
            Some(ResTarget { name: Some(name.into()), location: -1, ..Default::default() }.into())
        })
        .collect();
    let targets = names
        .iter()
        .enumerate()
        .map(|(k, &name)| {
            let value = match transforms.get(k).copied().flatten() {
                Some(expr) => expr.clone(),
                None => {
                    let field = Some(Node::String(name.into()));
                    ColumnRef { fields: vec![field], location: -1 }.into()
                }
            };
            Some(ResTarget { val: Some(value), location: -1, ..Default::default() }.into())
        })
        .collect();
    let colnames = names.iter().map(|&name| Some(Node::String(name.into()))).collect();
    let alias = Alias { aliasname: rv.relname.clone(), colnames };
    let from = RangeSubselect {
        lateral: false,
        subquery: Some(values(names.len()).into()),
        alias: Some(Box::new(alias)),
    };
    let select = SelectStmt {
        targetList: targets,
        fromClause: vec![Some(from.into())],
        ..Default::default()
    };
    let insert = InsertStmt {
        relation: Some(Box::new(RangeVar { alias: None, location: -1, ..rv.clone() })),
        cols,
        selectStmt: Some(select.into()),
        r#override: OverridingKind::OVERRIDING_SYSTEM_VALUE,
        ..Default::default()
    };
    insert.into()
}

/// `VALUES ($1, ..., $count)`.
fn values(count: usize) -> SelectStmt {
    let params = (1..=count)
        .map(|n| {
            let number = i32::try_from(n).unwrap_or(i32::MAX);
            Some(ParamRef { number, location: -1 }.into())
        })
        .collect();
    SelectStmt { valuesLists: vec![Some(Node::List(params))], ..Default::default() }
}

/// `CopyGetAttnums`: the indexes of the columns that a list of names gives, or of all the columns when the list is empty. `rel` is the name of the relation for the error of a column that does not exist.
///
/// # Errors
///
/// `42703` for a name that is not a column and `42701` for a name that the list gives twice.
pub fn attnums(columns: &[String], names: &[String], rel: Option<&str>) -> Result<Vec<usize>> {
    if names.is_empty() {
        return Ok((0..columns.len()).collect());
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let Some(at) = columns.iter().position(|c| c == name) else {
            let message = match rel {
                Some(rel) => format!("column \"{name}\" of relation \"{rel}\" does not exist"),
                None => format!("column \"{name}\" does not exist"),
            };
            return Err(Error::new(SqlState::UNDEFINED_COLUMN, message));
        };
        if out.contains(&at) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{name}\" specified more than once"),
            ));
        }
        out.push(at);
    }
    Ok(out)
}

/// The names of the user columns of a relation of the catalog or of the built-in catalog.
pub(super) fn relation_columns(env: &dyn Env, relid: u32) -> Vec<String> {
    if let Some(rel) = env.catalog().and_then(|c| c.relation(relid)) {
        return rel.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect();
    }
    builtin::attributes(relid)
        .iter()
        .filter(|a| a.num > 0 && !a.dropped)
        .map(|a| a.name.to_string())
        .collect()
}

/// The text of a `String` node.
fn string(node: &Node) -> Option<&str> {
    match node {
        Node::String(s) => Some(s),
        _ => None,
    }
}

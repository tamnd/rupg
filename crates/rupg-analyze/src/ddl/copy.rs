//! The relation of `COPY`, as `DoCopy`, `CopyGetAttnums` and `BeginCopyTo` in `copy.c` and `copyto.c` check it: the name, the columns and the kind of the relation. The session reads the rows with a query of the columns, as `DoCopy` does for a table with row security.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    Alias, ColumnRef, CopyStmt, InsertStmt, Node, OverridingKind, ParamRef, RangeSubselect,
    RangeVar, ResTarget, SelectStmt,
};

use super::lock::{lookup, relkind};
use super::relkind_detail;
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
    /// `SELECT` of the selected columns `FROM ONLY` the relation.
    pub query: Node,
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
    let selected = attnums(&columns, &names, Some(&name))?;
    Ok(CopyTarget { relid, name, kind, columns, selected })
}

/// The checks of `DoCopy` and `BeginCopyTo` for the relation of `COPY ... TO`: `table_openrv`, `CopyGetAttnums` and the kind of the relation.
///
/// # Errors
///
/// The errors of `RangeVarGetRelid` for the name, `42809` for an index or a relation that is not a table, `42703` and `42701` for the list of columns.
pub fn copy_relation(stmt: &CopyStmt, rv: &RangeVar, env: &dyn Env) -> Result<CopyRelation> {
    let CopyTarget { name, kind, columns, selected, .. } = copy_target(stmt, rv, env)?;
    match kind {
        b'r' => {}
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
    let targets = selected
        .iter()
        .map(|&i| {
            let field = Some(Node::String(columns[i].as_str().into()));
            let column = ColumnRef { fields: vec![field], location: -1 };
            let target = ResTarget { val: Some(column.into()), location: -1, ..Default::default() };
            Some(target.into())
        })
        .collect();
    let from = RangeVar { inh: false, alias: None, location: -1, ..rv.clone() };
    let select = SelectStmt {
        targetList: targets,
        fromClause: vec![Some(from.into())],
        ..Default::default()
    };
    Ok(CopyRelation { name, columns, selected, query: select.into() })
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
fn relation_columns(env: &dyn Env, relid: u32) -> Vec<String> {
    if let Some(rel) = env.catalog().and_then(|c| c.relation(relid)) {
        return rel.columns.iter().map(|c| c.name.clone()).collect();
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

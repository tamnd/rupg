//! `transformFromClause` of `parse_clause.c` and the name lookup of `parse_relation.c`: the relations of `FROM`, the joins, and the column that a name gives.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Alias, ColumnRef, JoinExpr, JoinType, Node, RangeVar};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Var};
use crate::typename::place;

/// `MAX_FUZZY_DISTANCE` of `parse_relation.c`: the largest distance of a name that an error suggests.
const MAX_FUZZY_DISTANCE: usize = 3;

/// The attribute number of the system column `tableoid`.
pub const TABLE_OID_ATTNUM: i16 = -6;

/// The system columns other than `tableoid`, which rupg does not have yet.
const OTHER_SYSTEM_COLUMNS: [&str; 5] = ["ctid", "xmin", "cmin", "xmax", "cmax"];

/// A relation of `FROM`, as a `RangeTblEntry` of the kind `RTE_RELATION`.
#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    /// The OID of the `pg_class` row.
    pub oid: u32,
    /// The columns of the relation, with their real names.
    pub columns: Vec<Column>,
}

/// A column of a relation.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: u32,
    pub typmod: i32,
}

/// A part of `FROM`: a relation or a join.
#[derive(Clone, Debug, PartialEq)]
pub enum FromItem {
    /// The relation with this index in [`crate::Query::relations`].
    Relation(usize),
    Join(Box<Join>),
}

/// A join of two parts of `FROM`.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub left: FromItem,
    pub right: FromItem,
    /// The condition of `ON` or of `USING`, of type `boolean`, or `None` for a cross join.
    pub on: Option<Expr>,
}

/// The kinds of join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// An entry of the range table, as `RangeTblEntry`: a relation or a join. The errors search all the entries, also the entries that a name cannot see.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// `eref->aliasname`: the alias, or the name of the relation.
    name: String,
    /// True when the query gives an alias.
    aliased: bool,
    /// `eref->colnames`.
    columns: Vec<String>,
    /// The index in the relations for a relation, or `None` for a join.
    relation: Option<usize>,
    /// The OID of the relation, or 0 for a join.
    oid: u32,
}

/// An item of the namespace, as `ParseNamespaceItem`: a name that the query can use and its columns.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    /// The index of the entry in the range table.
    entry: usize,
    /// The name that qualifies the columns.
    name: String,
    /// The name and the value of each column that the item gives.
    columns: Vec<(String, Expr)>,
    /// True when a qualified name can use the item.
    rel_visible: bool,
    /// True when an unqualified column name can use the item.
    cols_visible: bool,
}

/// The state of the names of `FROM`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope {
    pub(crate) relations: Vec<Relation>,
    entries: Vec<Entry>,
    namespace: Vec<Item>,
}

/// `varstr_levenshtein` with costs of 1: the edit distance of two names, by characters.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// `FuzzyAttrMatchState`: the best matches of a column name that does not exist.
struct Fuzzy {
    distance: usize,
    first: Option<(Place, usize)>,
    second: Option<(Place, usize)>,
    exact: Vec<Place>,
}

/// The place of an entry of the range table: the query level, 0 for the query of the name, and the index of the entry at that level.
type Place = (usize, usize);

impl Fuzzy {
    /// `updateFuzzyAttrMatchState`.
    fn update(&mut self, penalty: usize, entry: Place, actual: &str, wanted: &str, column: usize) {
        if penalty > self.distance || actual.is_empty() {
            return;
        }
        let distance = levenshtein(actual, wanted);
        if distance > wanted.len() / 2 {
            return;
        }
        let distance = distance + penalty;
        if distance < self.distance {
            self.distance = distance;
            self.first = Some((entry, column));
            self.second = None;
        } else if distance == self.distance {
            if self.second.is_some() {
                self.first = None;
                self.second = None;
            } else if self.first.is_some() {
                self.second = Some((entry, column));
            }
        }
    }
}

/// The names of an alias column list.
fn alias_columns(alias: &Alias) -> Vec<String> {
    alias
        .colnames
        .iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// The strings of a list of `String` nodes, where `None` is `*`.
fn fields(list: &[Option<Node>]) -> Vec<Option<&str>> {
    list.iter()
        .map(|f| match f {
            Some(Node::String(s)) => Some(&**s),
            _ => None,
        })
        .collect()
}

impl Analyzer<'_> {
    /// `transformFromClause`: the parts of `FROM`, with their names in the namespace.
    pub(crate) fn transform_from(&mut self, list: &[Option<Node>]) -> Result<Vec<FromItem>> {
        let mut items = Vec::with_capacity(list.len());
        for node in list {
            let node = node.as_ref().ok_or_else(|| Error::internal("an empty FROM item"))?;
            let (item, _, namespace) = self.transform_from_item(node)?;
            self.check_conflicts(&self.scope.namespace, &namespace)?;
            self.scope.namespace.extend(namespace);
            items.push(item);
        }
        Ok(items)
    }

    /// `transformFromClauseItem`: one part of `FROM`, the namespace item of its result, and the namespace that it adds.
    fn transform_from_item(&mut self, node: &Node) -> Result<(FromItem, Item, Vec<Item>)> {
        match node {
            Node::RangeVar(rv) => {
                let item = self.relation(rv)?;
                let index = self.scope.entries[item.entry].relation.unwrap_or_default();
                Ok((FromItem::Relation(index), item.clone(), vec![item]))
            }
            Node::JoinExpr(j) => self.join(j),
            Node::RangeSubselect(_) => Err(not_yet("a subquery in FROM", None)),
            Node::RangeFunction(_) => Err(not_yet("a function in FROM", None)),
            Node::RangeTableSample(_) => Err(not_yet("TABLESAMPLE", None)),
            Node::RangeTableFunc(_) | Node::JsonTable(_) => {
                Err(not_yet("XMLTABLE and JSON_TABLE", None))
            }
            _ => Err(Error::internal("unrecognized node type in FROM")),
        }
    }

    /// `addRangeTableEntry` for a table of the catalog: the entry and its namespace item.
    fn relation(&mut self, rv: &RangeVar) -> Result<Item> {
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default();
        let catalog = self.lookup_relation(rv)?.ok_or_else(|| {
            let message = match &rv.schemaname {
                Some(schema) => format!("relation \"{schema}.{name}\" does not exist"),
                None => format!("relation \"{name}\" does not exist"),
            };
            Error::new(SqlState::UNDEFINED_TABLE, message).at_opt(at)
        })?;
        let columns: Vec<Column> = catalog
            .columns
            .iter()
            .map(|c| Column { name: c.name.to_string(), ty: c.type_oid, typmod: -1 })
            .collect();
        let alias = rv.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(name).to_string();
        let mut colnames: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        if let Some(alias) = alias {
            let given = alias_columns(alias);
            if given.len() > colnames.len() {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!(
                        "table \"{refname}\" has {} columns available but {} columns specified",
                        colnames.len(),
                        given.len()
                    ),
                ));
            }
            for (slot, name) in colnames.iter_mut().zip(given) {
                *slot = name;
            }
        }
        let index = self.scope.relations.len();
        let exprs: Vec<(String, Expr)> = colnames
            .iter()
            .zip(&columns)
            .enumerate()
            .map(|(i, (n, c))| {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let var = Var { relation: index, attnum, levels_up: 0 };
                (n.clone(), Expr::new(ExprKind::Var(var), c.ty))
            })
            .collect();
        self.scope.relations.push(Relation { oid: catalog.oid, columns });
        let entry = self.scope.entries.len();
        self.scope.entries.push(Entry {
            name: refname.clone(),
            aliased: alias.is_some(),
            columns: colnames,
            relation: Some(index),
            oid: catalog.oid,
        });
        Ok(Item { entry, name: refname, columns: exprs, rel_visible: true, cols_visible: true })
    }

    /// `addRangeTableEntryForRelation` and `addNSItemToQuery` for a table of a statement that defines an object: the expressions of the statement can use the columns. Each column is a name, a type and a typmod.
    pub(crate) fn add_table(&mut self, oid: u32, name: &str, columns: &[(String, u32, i32)]) {
        let index = self.scope.relations.len();
        let exprs: Vec<(String, Expr)> = columns
            .iter()
            .enumerate()
            .map(|(i, (n, ty, typmod))| {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let var = Var { relation: index, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), *ty);
                expr.typmod = *typmod;
                (n.clone(), expr)
            })
            .collect();
        let columns: Vec<Column> = columns
            .iter()
            .map(|(n, ty, typmod)| Column { name: n.clone(), ty: *ty, typmod: *typmod })
            .collect();
        let colnames = columns.iter().map(|c| c.name.clone()).collect();
        self.scope.relations.push(Relation { oid, columns });
        let entry = self.scope.entries.len();
        self.scope.entries.push(Entry {
            name: name.to_string(),
            aliased: false,
            columns: colnames,
            relation: Some(index),
            oid,
        });
        self.scope.namespace.push(Item {
            entry,
            name: name.to_string(),
            columns: exprs,
            rel_visible: true,
            cols_visible: true,
        });
    }

    /// `RangeVarGetRelid`: the table of the catalog that a name gives, or `None`.
    fn lookup_relation(&self, rv: &RangeVar) -> Result<Option<&'static rupg_pgcatalog::Catalog>> {
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default();
        if let Some(database) = &rv.catalogname
            && **database != self.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "cross-database references are not implemented: \"{database}.{}.{name}\"",
                    rv.schemaname.as_deref().unwrap_or_default()
                ),
            )
            .at_opt(at));
        }
        let in_schema = |namespace: u32| {
            if namespace == crate::PG_CATALOG_NAMESPACE {
                rupg_pgcatalog::catalog(name)
            } else {
                None
            }
        };
        match &rv.schemaname {
            // A schema that does not exist has no relation, as RangeVarGetRelid with missing_ok gives.
            Some(schema) => Ok(self.schema(schema).and_then(in_schema)),
            None => Ok(self.path.iter().find_map(|&ns| in_schema(ns))),
        }
    }

    /// `checkNameSpaceConflicts`: two items that the same name qualifies are an error, except two relations with no alias that are not the same relation.
    fn check_conflicts(&self, first: &[Item], second: &[Item]) -> Result<()> {
        for a in first.iter().filter(|i| i.rel_visible) {
            for b in second.iter().filter(|i| i.rel_visible) {
                if a.name != b.name {
                    continue;
                }
                let (ea, eb) = (&self.scope.entries[a.entry], &self.scope.entries[b.entry]);
                let plain = |e: &Entry| e.relation.is_some() && !e.aliased;
                if plain(ea) && plain(eb) && ea.oid != eb.oid {
                    continue;
                }
                return Err(Error::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("table name \"{}\" specified more than once", a.name),
                ));
            }
        }
        Ok(())
    }

    /// The `JoinExpr` case of `transformFromClauseItem`.
    fn join(&mut self, j: &JoinExpr) -> Result<(FromItem, Item, Vec<Item>)> {
        let left_node = j.larg.as_ref().ok_or_else(|| Error::internal("a join with no left"))?;
        let right_node = j.rarg.as_ref().ok_or_else(|| Error::internal("a join with no right"))?;
        let (left, l_item, l_namespace) = self.transform_from_item(left_node)?;
        let (right, r_item, r_namespace) = self.transform_from_item(right_node)?;
        self.check_conflicts(&l_namespace, &r_namespace)?;
        let mut namespace = l_namespace;
        namespace.extend(r_namespace);
        let kind = match j.jointype {
            JoinType::JOIN_INNER => JoinKind::Inner,
            JoinType::JOIN_LEFT => JoinKind::Left,
            JoinType::JOIN_RIGHT => JoinKind::Right,
            JoinType::JOIN_FULL => JoinKind::Full,
            _ => return Err(Error::internal("unrecognized join type")),
        };
        let l_names: Vec<&str> = l_item.columns.iter().map(|(n, _)| n.as_str()).collect();
        let r_names: Vec<&str> = r_item.columns.iter().map(|(n, _)| n.as_str()).collect();
        let using: Vec<String> = if j.isNatural {
            l_names.iter().filter(|n| r_names.contains(n)).map(|n| (*n).to_string()).collect()
        } else {
            fields(&j.usingClause).into_iter().flatten().map(str::to_string).collect()
        };
        let mut merged: Vec<(String, Expr)> = Vec::new();
        let mut l_used = Vec::new();
        let mut r_used = Vec::new();
        let mut on = None;
        if !using.is_empty() {
            let mut pairs = Vec::new();
            for name in &using {
                if merged.iter().any(|(n, _)| n == name) || pairs.iter().any(|(n, _, _)| n == name)
                {
                    return Err(Error::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column name \"{name}\" appears more than once in USING clause"),
                    ));
                }
                let find = |names: &[&str], side: &str| -> Result<usize> {
                    let mut found = None;
                    for (i, n) in names.iter().enumerate() {
                        if n == name {
                            if found.is_some() {
                                return Err(Error::new(
                                    SqlState::AMBIGUOUS_COLUMN,
                                    format!(
                                        "common column name \"{name}\" appears more than once in {side} table"
                                    ),
                                ));
                            }
                            found = Some(i);
                        }
                    }
                    found.ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{name}\" specified in USING clause does not exist in {side} table"),
                        )
                    })
                };
                let l = find(&l_names, "left")?;
                let r = find(&r_names, "right")?;
                pairs.push((name.clone(), l, r));
            }
            // transformJoinUsingClause: `l = r` for each column, joined with AND.
            let mut conds = Vec::new();
            for (_, l, r) in &pairs {
                let (lv, rv) = (l_item.columns[*l].1.clone(), r_item.columns[*r].1.clone());
                conds.push(self.make_op(&["="], Some(lv), rv, None)?);
            }
            let cond = if conds.len() == 1 {
                conds.remove(0)
            } else {
                Expr::new(ExprKind::Bool(crate::expr::BoolOp::And, conds), oid::BOOL)
            };
            on = Some(self.coerce_to_boolean(cond, "JOIN/USING")?);
            for (name, l, r) in pairs {
                let expr = self.merged_column(
                    kind,
                    l_item.columns[l].1.clone(),
                    r_item.columns[r].1.clone(),
                )?;
                merged.push((name, expr));
                l_used.push(l);
                r_used.push(r);
            }
        } else if let Some(quals) = &j.quals {
            // transformJoinOnClause: the condition sees only the two sides of the join.
            let saved = std::mem::replace(&mut self.scope.namespace, namespace.clone());
            let result = self.with_kind(Kind::JoinOn, |a| a.transform(Some(quals)));
            self.scope.namespace = saved;
            on = Some(self.coerce_to_boolean(result?, "JOIN/ON")?);
        }
        let mut columns = merged;
        let using_count = columns.len();
        for (i, c) in l_item.columns.iter().enumerate() {
            if !l_used.contains(&i) {
                columns.push(c.clone());
            }
        }
        for (i, c) in r_item.columns.iter().enumerate() {
            if !r_used.contains(&i) {
                columns.push(c.clone());
            }
        }
        let alias = j.alias.as_deref();
        if let Some(alias) = alias {
            let given = alias_columns(alias);
            let name = alias.aliasname.as_deref().unwrap_or_default();
            if given.len() > columns.len() {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!(
                        "join expression \"{name}\" has {} columns available but {} columns specified",
                        columns.len(),
                        given.len()
                    ),
                ));
            }
            for (slot, n) in columns.iter_mut().zip(given) {
                slot.0 = n;
            }
        }
        let entry = self.scope.entries.len();
        let name = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or("unnamed_join");
        self.scope.entries.push(Entry {
            name: name.to_string(),
            aliased: alias.is_some(),
            columns: columns.iter().map(|(n, _)| n.clone()).collect(),
            relation: None,
            oid: 0,
        });
        if let Some(using_alias) = j.join_using_alias.as_deref() {
            let item = Item {
                entry,
                name: using_alias.aliasname.as_deref().unwrap_or_default().to_string(),
                columns: columns[..using_count].to_vec(),
                rel_visible: true,
                cols_visible: true,
            };
            self.check_conflicts(std::slice::from_ref(&item), &namespace)?;
            namespace.push(item);
        }
        if alias.is_some() {
            namespace.clear();
        } else {
            for item in &mut namespace {
                item.cols_visible = false;
            }
        }
        let item = Item {
            entry,
            name: name.to_string(),
            columns,
            rel_visible: alias.is_some(),
            cols_visible: true,
        };
        namespace.push(item.clone());
        let join = Join { kind, left, right, on };
        Ok((FromItem::Join(Box::new(join)), item, namespace))
    }

    /// `buildMergedJoinVar`: the value of a column of `USING`.
    fn merged_column(&mut self, kind: JoinKind, l: Expr, r: Expr) -> Result<Expr> {
        let pair = [l, r];
        let ty = self.common_type(&pair, "JOIN/USING")?;
        let [l, r] = pair;
        let typmod = if l.typmod == r.typmod { l.typmod } else { -1 };
        let mut fix = |e: Expr| -> Result<Expr> {
            if e.ty != ty {
                self.coerce(e, ty, typmod, Context::Implicit, None)
            } else if e.typmod != typmod {
                Ok(Expr { kind: ExprKind::Relabel(Box::new(e)), ty, typmod, location: None })
            } else {
                Ok(e)
            }
        };
        let (l, r) = (fix(l)?, fix(r)?);
        let is_var = |e: &Expr| matches!(e.kind, ExprKind::Var(_));
        Ok(match kind {
            JoinKind::Inner if !is_var(&l) && is_var(&r) => r,
            JoinKind::Inner | JoinKind::Left => l,
            JoinKind::Right => r,
            JoinKind::Full => {
                Expr { kind: ExprKind::Coalesce(vec![l, r]), ty, typmod: -1, location: None }
            }
        })
    }

    /// `transformColumnRef`: the value of a column name.
    pub(crate) fn column_ref(&mut self, c: &ColumnRef) -> Result<Expr> {
        let at = place(c.location);
        if self.kind == Kind::ColumnDefault {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot use column reference in DEFAULT expression",
            )
            .at_opt(at));
        }
        let names = fields(&c.fields);
        let (schema, table, column) = match names.as_slice() {
            [Some(column)] => {
                if let Some(expr) = self.column_by_name(column, at)? {
                    return Ok(expr);
                }
                if self.item_by_name(None, column, at)?.is_some() {
                    return Err(not_yet("a whole-row reference", at));
                }
                return Err(self.missing_column(None, column, at));
            }
            [None] => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "SELECT * with no tables specified is not valid",
                )
                .at_opt(at));
            }
            [Some(table), Some(column)] => (None, *table, *column),
            [Some(schema), Some(table), Some(column)] => (Some(*schema), *table, *column),
            [Some(database), Some(schema), Some(table), Some(column)] => {
                if *database != self.env.database() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.iter().map(|n| n.unwrap_or("*")).collect::<Vec<_>>().join(".")
                        ),
                    )
                    .at_opt(at));
                }
                (Some(*schema), *table, *column)
            }
            [.., None] if names.len() <= 4 => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "a row expansion with * is supported only in the target list",
                )
                .at_opt(at));
            }
            _ => {
                let text: Vec<&str> = names.iter().map(|f| f.unwrap_or("*")).collect();
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {}", text.join(".")),
                )
                .at_opt(at));
            }
        };
        let Some((level, item)) = self.item_by_name(schema, table, at)? else {
            return Err(self.missing_entry(schema, table, at));
        };
        match self.column_in(level, &item, column, at)? {
            Some(expr) => Ok(expr),
            None => Err(self.missing_column(Some(table), column, at)),
        }
    }

    /// The scopes of the query and of the queries outside it, with their levels: 0 for the query, 1 for the query outside it, and so on.
    fn levels(&self) -> impl Iterator<Item = (usize, &Scope)> {
        std::iter::once(&self.scope).chain(self.outer.iter().rev()).enumerate()
    }

    /// The scope of a level.
    fn level(&self, level: usize) -> &Scope {
        self.levels().nth(level).map_or(&self.scope, |(_, scope)| scope)
    }

    /// `scanNSItemForColumn`: the column of an item of a level with this name, or `None`. The columns of the expression are `levels_up` of the level.
    fn column_in(
        &self,
        level: usize,
        item: &Item,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<Expr>> {
        let mut found = None;
        for (n, expr) in &item.columns {
            if n == name {
                if found.is_some() {
                    return Err(ambiguous(name, at));
                }
                found = Some(expr);
            }
        }
        if let Some(expr) = found {
            let mut expr = expr.clone().at(at);
            expr.raise(level);
            return Ok(Some(expr));
        }
        let entry = &self.level(level).entries[item.entry];
        let Some(relation) = entry.relation else { return Ok(None) };
        if item.columns.len() != entry.columns.len() {
            // The item of a USING alias gives only the columns of USING.
            return Ok(None);
        }
        if name == "tableoid" {
            let var = Var { relation, attnum: TABLE_OID_ATTNUM, levels_up: level };
            return Ok(Some(Expr::new(ExprKind::Var(var), oid::OID).at(at)));
        }
        if OTHER_SYSTEM_COLUMNS.contains(&name) {
            if self.kind == Kind::Check {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!("system column \"{name}\" reference in check constraint is invalid"),
                )
                .at_opt(at));
            }
            return Err(not_yet(&format!("the system column \"{name}\""), at));
        }
        Ok(None)
    }

    /// The name of the relation and the name of the column of a `Var`, as `eref->aliasname` and `get_rte_attribute_name` give them for an error.
    pub(crate) fn column_names(&self, var: Var) -> (String, String) {
        let scope = self.level(var.levels_up);
        let Some(entry) = scope.entries.iter().find(|e| e.relation == Some(var.relation)) else {
            return (String::new(), String::new());
        };
        let column = match usize::try_from(var.attnum) {
            Ok(n) if n > 0 => entry.columns.get(n - 1).cloned().unwrap_or_default(),
            _ => "tableoid".to_string(),
        };
        (entry.name.clone(), column)
    }

    /// `colNameToVar`: the column that an unqualified name gives, or `None`. The search goes up the query levels and stops at the first level that has the name.
    pub(crate) fn column_by_name(&self, name: &str, at: Option<usize>) -> Result<Option<Expr>> {
        for (level, scope) in self.levels() {
            let mut result = None;
            for item in scope.namespace.iter().filter(|i| i.cols_visible) {
                if let Some(expr) = self.column_in(level, item, name, at)? {
                    if result.is_some() {
                        return Err(ambiguous(name, at));
                    }
                    result = Some(expr);
                }
            }
            if result.is_some() {
                return Ok(result);
            }
        }
        Ok(None)
    }

    /// `refnameNamespaceItem`: the item that a table name gives and its level, or `None`. The search goes up the query levels and stops at the first level that has the name.
    fn item_by_name(
        &self,
        schema: Option<&str>,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<(usize, Item)>> {
        for (level, scope) in self.levels() {
            if let Some(item) = self.item_in(scope, schema, name, at)? {
                return Ok(Some((level, item)));
            }
        }
        Ok(None)
    }

    /// `scanNameSpaceForRefname` and `scanNameSpaceForRelid`: the item of a scope that a table name gives, or `None`.
    fn item_in(
        &self,
        scope: &Scope,
        schema: Option<&str>,
        name: &str,
        at: Option<usize>,
    ) -> Result<Option<Item>> {
        if let Some(schema) = schema {
            // scanNameSpaceForRelid: a relation with no alias in that schema.
            let rv = RangeVar {
                schemaname: Some(schema.into()),
                relname: Some(name.into()),
                location: at.map_or(-1, |a| i32::try_from(a).unwrap_or(-1)),
                ..RangeVar::default()
            };
            let Some(catalog) = self.lookup_relation(&rv)? else { return Ok(None) };
            let mut result = None;
            for item in scope.namespace.iter().filter(|i| i.rel_visible) {
                let entry = &scope.entries[item.entry];
                if entry.relation.is_some() && !entry.aliased && entry.oid == catalog.oid {
                    if result.is_some() {
                        return Err(Error::new(
                            SqlState::AMBIGUOUS_ALIAS,
                            format!("table reference {} is ambiguous", catalog.oid),
                        )
                        .at_opt(at));
                    }
                    result = Some(item.clone());
                }
            }
            return Ok(result);
        }
        let mut result = None;
        for item in scope.namespace.iter().filter(|i| i.rel_visible) {
            if item.name == name {
                if result.is_some() {
                    return Err(Error::new(
                        SqlState::AMBIGUOUS_ALIAS,
                        format!("table reference \"{name}\" is ambiguous"),
                    )
                    .at_opt(at));
                }
                result = Some(item.clone());
            }
        }
        Ok(result)
    }

    /// `errorMissingRTE`.
    fn missing_entry(&self, schema: Option<&str>, name: &str, at: Option<usize>) -> Error {
        // searchRangeTableForRel: an entry for the relation or with the name.
        let rv = RangeVar {
            schemaname: schema.map(Into::into),
            relname: Some(name.into()),
            ..RangeVar::default()
        };
        let oid = self.lookup_relation(&rv).ok().flatten().map(|c| c.oid);
        let found = self.levels().find_map(|(level, scope)| {
            let index = scope
                .entries
                .iter()
                .position(|e| (e.relation.is_some() && Some(e.oid) == oid) || e.name == name)?;
            Some((level, index))
        });
        let Some((level, found)) = found else {
            return Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("missing FROM-clause entry for table \"{name}\""),
            )
            .at_opt(at);
        };
        let entry = &self.level(level).entries[found];
        let error = Error::new(
            SqlState::UNDEFINED_TABLE,
            format!("invalid reference to FROM-clause entry for table \"{name}\""),
        )
        .at_opt(at);
        if entry.aliased && entry.name != name {
            let visible = matches!(
                self.item_by_name(None, &entry.name, None),
                Ok(Some((l, item))) if l == level && item.entry == found
            );
            if visible {
                return error.with_hint(format!(
                    "Perhaps you meant to reference the table alias \"{}\".",
                    entry.name
                ));
            }
        }
        error.with_detail(format!(
            "There is an entry for table \"{}\", but it cannot be referenced from this part of the query.",
            entry.name
        ))
    }

    /// `errorMissingColumn`, with the hint of a close name.
    fn missing_column(&self, table: Option<&str>, column: &str, at: Option<usize>) -> Error {
        let mut fuzzy = Fuzzy {
            distance: MAX_FUZZY_DISTANCE + 1,
            first: None,
            second: None,
            exact: Vec::new(),
        };
        let entries = self.levels().flat_map(|(level, scope)| {
            scope.entries.iter().enumerate().map(move |(index, entry)| ((level, index), entry))
        });
        for (index, entry) in entries {
            if entry.relation.is_none() {
                continue;
            }
            let penalty =
                table.map_or(0, |t| levenshtein(t, &entry.name).min(MAX_FUZZY_DISTANCE + 1));
            let mut exact = false;
            for (i, name) in entry.columns.iter().enumerate() {
                if name == column {
                    exact = true;
                }
                fuzzy.update(penalty, index, name, column, i);
            }
            if (exact || column == "tableoid") && penalty == 0 {
                fuzzy.exact.push(index);
            }
        }
        let message = match table {
            Some(t) => format!("column {t}.{column} does not exist"),
            None => format!("column \"{column}\" does not exist"),
        };
        let error = Error::new(SqlState::UNDEFINED_COLUMN, message).at_opt(at);
        let entry = |(level, index): Place| &self.level(level).entries[index];
        if fuzzy.exact.len() > 1 {
            let error = error.with_detail(format!("There are columns named \"{column}\", but they are in tables that cannot be referenced from this part of the query."));
            return if table.is_none() {
                error.with_hint("Try using a table-qualified name.")
            } else {
                error
            };
        }
        if let Some(&index) = fuzzy.exact.first() {
            let error = error.with_detail(format!("There is a column named \"{column}\" in table \"{}\", but it cannot be referenced from this part of the query.", entry(index).name));
            // rte_visible_if_qualified: the item has a name that a query can qualify.
            let qualified = self.levels().any(|(level, scope)| {
                level == index.0
                    && scope.namespace.iter().any(|i| i.entry == index.1 && i.rel_visible)
            });
            return if table.is_none() && qualified {
                error.with_hint("To reference that column, you must use a table-qualified name.")
            } else {
                error
            };
        }
        let name = |(e, c): (Place, usize)| format!("{}.{}", entry(e).name, entry(e).columns[c]);
        match (fuzzy.first, fuzzy.second) {
            (Some(first), None) => error.with_hint(format!(
                "Perhaps you meant to reference the column \"{}\".",
                name(first)
            )),
            (Some(first), Some(second)) => error.with_hint(format!(
                "Perhaps you meant to reference the column \"{}\" or the column \"{}\".",
                name(first),
                name(second)
            )),
            _ => error,
        }
    }

    /// `ExpandColumnRefStar`: the columns of `*` or `t.*` in a target list, with their names.
    pub(crate) fn expand_star(&mut self, c: &ColumnRef) -> Result<Option<Vec<(String, Expr)>>> {
        let at = place(c.location);
        let names = fields(&c.fields);
        let Some((None, qualifier)) = names.split_last() else { return Ok(None) };
        let items: Vec<Item> = match qualifier {
            [] => {
                let visible: Vec<Item> =
                    self.scope.namespace.iter().filter(|i| i.cols_visible).cloned().collect();
                if visible.is_empty() {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid",
                    )
                    .at_opt(at));
                }
                visible
            }
            [Some(table)] | [Some(_), Some(table)] | [Some(_), Some(_), Some(table)] => {
                let schema = match qualifier {
                    [Some(schema), _] | [_, Some(schema), _] => Some(*schema),
                    _ => None,
                };
                if let [Some(database), _, _] = qualifier
                    && *database != self.env.database()
                {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.iter().map(|n| n.unwrap_or("*")).collect::<Vec<_>>().join(".")
                        ),
                    )
                    .at_opt(at));
                }
                match self.item_by_name(schema, table, at)? {
                    Some((0, item)) => vec![item],
                    Some(_) => {
                        return Err(not_yet("a row expansion of a relation of an outer query", at));
                    }
                    None => return Err(self.missing_entry(schema, table, at)),
                }
            }
            _ => return Ok(None),
        };
        let mut columns = Vec::new();
        for item in items {
            for (name, expr) in item.columns {
                columns.push((name, expr.at(at)));
            }
        }
        Ok(Some(columns))
    }
}

/// The error of a name that two columns have.
fn ambiguous(name: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::AMBIGUOUS_COLUMN, format!("column reference \"{name}\" is ambiguous"))
        .at_opt(at)
}

/// The error of a part of `FROM` that the analyzer does not take yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

#[cfg(test)]
mod tests {
    use super::levenshtein;

    #[test]
    fn distances() {
        assert_eq!(levenshtein("relname", "relnam"), 1);
        assert_eq!(levenshtein("oid", "iod"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}

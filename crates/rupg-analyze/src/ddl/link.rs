//! `CREATE INDEX ... USING link`, the link index of spec/13 section 13.2 without a foreign key: `WITH (parent = 'orders')` names the parent table, and `parent_columns` names its key columns when they are not the primary key. The parent must have a primary key or a unique index on the key columns, as the referenced table of a foreign key must, and each column of the link must compare with its key column by an equality operator.

use rupg_catalog::{Catalog, ConKind, DepKind, NewIndexColumn, PG_CLASS, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::RangeVar;

use super::Definer;
use super::fkey::Keys;
use super::reloptions::{Kind, Opt};

/// The options of a link index: the parent table and its key columns, each a text.
pub(super) const OPTIONS: &[Opt] = &[
    Opt { name: "parent", kind: Kind::Text, toast: false },
    Opt { name: "parent_columns", kind: Kind::Text, toast: false },
];

/// The value of the text option `name`, or `None` when the options do not set it.
fn text_option<'a>(options: &'a [String], name: &str) -> Option<&'a str> {
    options.iter().find_map(|text| {
        let (found, value) = text.split_once('=')?;
        (found == name).then_some(value)
    })
}

/// `SplitIdentifierString`: the names of `text` with `separator` between them. A name in double quotes keeps its case and a doubled quote stands for one, and any other name is in lower case. `None` for an empty name or a quote that does not end.
fn split_identifiers(text: &str, separator: char) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        let mut name = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => name.push('"'),
                    '"' => break,
                    c => name.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|&c| c != separator && !c.is_ascii_whitespace()) {
                name.push(c.to_ascii_lowercase());
            }
        }
        if name.is_empty() {
            return None;
        }
        names.push(name);
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        match chars.next() {
            None => return Some(names),
            Some(c) if c == separator => {}
            Some(_) => return None,
        }
    }
}

/// The error of an option of a link index, `ERRCODE_INVALID_PARAMETER_VALUE`.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

impl Definer<'_, '_> {
    /// The parent table of a new link index on `table` with the key columns `columns`, after the checks of spec/13 section 13.2: the columns are plain columns, the index has no predicate, the parent is a table with a primary key or a unique index on the parent key, and each column compares with its key column.
    ///
    /// # Errors
    ///
    /// `0A000` for an expression or a predicate, `22023` for a missing or malformed option, `42P01` for a parent that does not exist, `42809` for a parent that is not a table, `42703` for a key column that does not exist, `42830` for a parent key with no unique index or a count of columns that differs, and `42804` for a pair of columns that no operator compares.
    pub(super) fn link_parent(
        &self,
        table: u32,
        columns: &[NewIndexColumn],
        partial: bool,
        options: &[String],
    ) -> Result<u32> {
        let unsupported = if columns.iter().any(|c| c.key == 0) {
            Some("index expressions")
        } else if partial {
            Some("partial indexes")
        } else {
            None
        };
        if let Some(what) = unsupported {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("access method \"link\" does not support {what}"),
            ));
        }
        let Some(text) = text_option(options, "parent") else {
            return Err(invalid("access method \"link\" requires the parameter \"parent\"".into()));
        };
        let names = split_identifiers(text, '.').ok_or_else(|| {
            invalid(format!("invalid name syntax in parameter \"parent\": {text}"))
        })?;
        let mut rv = RangeVar { inh: true, ..RangeVar::default() };
        match &names[..] {
            [name] => rv.relname = Some(name.as_str().into()),
            [schema, name] => {
                rv.schemaname = Some(schema.as_str().into());
                rv.relname = Some(name.as_str().into());
            }
            _ => {
                return Err(invalid(format!(
                    "improper qualified name in parameter \"parent\": {text}"
                )));
            }
        }
        let shown = names.last().map_or("", String::as_str);
        let Some(parent) = self.an.lookup_relation(&rv)? else {
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", names.join(".")),
            ));
        };
        let kind = self.catalog.relation(parent).map(|r| r.kind);
        if !matches!(kind, Some(RelKind::Table | RelKind::Partitioned)) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("parent relation \"{shown}\" is not a table"),
            ));
        }
        let (pk_keys, classes) = match text_option(options, "parent_columns") {
            None => {
                let (_, keys, classes) = self.primary_key(parent, shown)?;
                (keys, classes)
            }
            Some(list) => {
                let list = split_identifiers(list, ',').ok_or_else(|| {
                    invalid(format!("invalid name syntax in parameter \"parent_columns\": {list}"))
                })?;
                let rel = self.catalog.relation(parent);
                let mut keys = Vec::with_capacity(list.len());
                for name in &list {
                    let Some(number) = rel.and_then(|r| r.column_number(name)) else {
                        return Err(Error::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{name}\" of relation \"{shown}\" does not exist"),
                        ));
                    };
                    keys.push(number);
                }
                let (_, classes) = self.unique_index(parent, &keys, shown, false)?;
                (keys, classes)
            }
        };
        if pk_keys.len() != columns.len() {
            return Err(Error::new(
                SqlState::INVALID_FOREIGN_KEY,
                "number of link columns and parent key columns disagree",
            ));
        }
        let fk: Vec<i16> = columns.iter().map(|c| c.key).collect();
        let keys = Keys {
            table,
            fk: &fk,
            pk: parent,
            pk_keys: &pk_keys,
            classes: &classes,
            period: false,
        };
        let child_names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        let parent_names: Vec<String> = pk_keys
            .iter()
            .map(|&k| {
                self.catalog
                    .relation(parent)
                    .and_then(|r| r.column(k))
                    .map_or_else(String::new, |c| c.name.clone())
            })
            .collect();
        self.foreign_operators("link", &keys, (&child_names, &parent_names))?;
        Ok(parent)
    }
}

/// A link index of spec/13 section 13.2 as the planner and the executor use it: the child table with the link columns, and the parent table with its key columns in the same order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkDefinition {
    /// The OID of the link index.
    pub index: u32,
    /// The OID of the child table, which has the index.
    pub child: u32,
    /// The column number of each link column in the child table.
    pub keys: Vec<i16>,
    /// The OID of the parent table.
    pub parent: u32,
    /// The column number in the parent table of the key column of each link column.
    pub parent_keys: Vec<i16>,
}

/// The link index `index` with its parent, or `None` when `index` is not a link index that a join can use. The parent is the table that the index depends on, and its key columns are those of `parent_columns` or of its primary key. A join through the link gives the rows of a join on the `=` of the columns only when each child row has one parent row at most, so the parent and the child must be tables with no children, each pair of columns must have one type with no collation or a deterministic one, and the parent must have an immediate unique index with no predicate on its key columns whose operator classes are the default classes of `btree`, as the equality of the join is the equality of the type. A link that is not valid gives `None` too, and the join is then a hash join, with the same rows.
pub fn link_definition(catalog: &Catalog, index: u32) -> Option<LinkDefinition> {
    let relation = catalog.relation(index)?;
    let info = relation.index.as_ref()?;
    if info.method != rupg_index::am::LINK || !info.valid || !info.ready || info.predicate.is_some()
    {
        return None;
    }
    let child = info.table;
    let count = usize::try_from(info.key_count).ok()?;
    let keys: Vec<i16> = info.keys.get(..count)?.to_vec();
    if keys.is_empty() || keys.iter().any(|&k| k <= 0) {
        return None;
    }
    let parent = catalog.depends().iter().find_map(|d| {
        (d.object.class == PG_CLASS
            && d.object.oid == index
            && d.kind == DepKind::Normal
            && d.referenced.class == PG_CLASS
            && d.referenced.sub == 0)
            .then_some(d.referenced.oid)
    })?;
    let plain = |oid: u32| {
        catalog.relation(oid).is_some_and(|r| r.kind == RelKind::Table)
            && !catalog.has_children(oid)
    };
    if !plain(child) || !plain(parent) {
        return None;
    }
    let parent_rel = catalog.relation(parent)?;
    let options = relation.options.as_slice();
    let parent_keys: Vec<i16> = match text_option(options, "parent_columns") {
        Some(list) => split_identifiers(list, ',')?
            .iter()
            .map(|name| parent_rel.column_number(name))
            .collect::<Option<_>>()?,
        None => {
            let pk = catalog.constraints_of(parent).find(|c| c.kind == ConKind::Primary)?.index;
            let pk = catalog.relation(pk)?.index.as_ref()?;
            pk.keys.get(..usize::try_from(pk.key_count).ok()?)?.to_vec()
        }
    };
    if parent_keys.len() != keys.len() {
        return None;
    }
    let child_rel = catalog.relation(child)?;
    for (&k, &pk) in keys.iter().zip(&parent_keys) {
        let (c, p) = (child_rel.column(k)?, parent_rel.column(pk)?);
        // `DEFAULT_COLLATION_OID`, `C_COLLATION_OID` and `POSIX_COLLATION_OID` are deterministic.
        let deterministic = |collation: u32| matches!(collation, 0 | 100 | 950 | 951);
        if c.ty != p.ty || c.collation != p.collation || !deterministic(c.collation) {
            return None;
        }
    }
    let unique = catalog.indexes_of(parent).any(|r| {
        let Some(i) = r.index.as_ref() else { return false };
        let count = usize::try_from(i.key_count).unwrap_or(usize::MAX);
        let index_keys = i.keys.get(..count).unwrap_or_default();
        i.unique
            && i.valid
            && i.immediate
            && i.predicate.is_none()
            && i.method == rupg_index::am::BTREE
            && index_keys.len() == parent_keys.len()
            && index_keys.iter().all(|k| parent_keys.contains(k))
            && index_keys.iter().zip(&i.classes).all(|(&k, &class)| {
                parent_rel.column(k).is_some_and(|c| {
                    crate::typcache::default_opclass(c.ty, rupg_index::am::BTREE)
                        .is_some_and(|d| d.oid == class)
                })
            })
    });
    unique.then_some(LinkDefinition { index, child, keys, parent, parent_keys })
}

/// The link indexes of the table `table` that a join can use, as [`link_definition`] finds them, in the order of their OIDs.
pub fn links_of(catalog: &Catalog, table: u32) -> Vec<LinkDefinition> {
    let mut out: Vec<LinkDefinition> = catalog
        .indexes_of(table)
        .filter(|r| r.index.as_ref().is_some_and(|i| i.method == rupg_index::am::LINK))
        .filter_map(|r| link_definition(catalog, r.oid))
        .collect();
    out.sort_by_key(|l| l.index);
    out
}

#[cfg(test)]
mod tests {
    use super::split_identifiers;

    #[test]
    fn identifiers_split_as_split_identifier_string_splits_them() {
        let split = |t: &str, s| split_identifiers(t, s);
        assert_eq!(split("Orders", '.'), Some(vec!["orders".to_string()]));
        assert_eq!(split(" s . \"Or.ders\"", '.'), Some(vec!["s".into(), "Or.ders".into()]));
        assert_eq!(split("a, \"b\"\"c\"", ','), Some(vec!["a".into(), "b\"c".into()]));
        assert_eq!(split("a,,b", ','), None);
        assert_eq!(split("\"a", '.'), None);
        assert_eq!(split("a b", '.'), None);
        assert_eq!(split("", '.'), None);
    }
}

//! The foreign keys of `CREATE TABLE`, as `ATExecAddConstraint` and `ATAddForeignKeyConstraint` add them.

use rupg_catalog::names::name_addition;
use rupg_catalog::{ConKind, FkSide, ForeignKey, NewForeignKey, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::Constraint;

use super::{Definer, Found, not_yet, relkind_detail, relname};
use crate::coerce::{Context, can_coerce};
use crate::typename::{names, place};
use crate::types;

/// The strategy number of the equality operator of a btree operator family.
const EQUAL_STRATEGY: i16 = 3;
/// The most columns of a foreign key, `INDEX_MAX_KEYS`.
const MAX_KEYS: usize = 32;

/// An error with the SQLSTATE `invalid_foreign_key`.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_FOREIGN_KEY, message)
}

/// The columns of a foreign key and the operator classes of the unique index that it uses, for [`Definer::foreign_operators`].
pub(super) struct Keys<'a> {
    /// The referencing table.
    pub(super) table: u32,
    /// The referencing columns.
    pub(super) fk: &'a [i16],
    /// The referenced table.
    pub(super) pk: u32,
    /// The referenced columns.
    pub(super) pk_keys: &'a [i16],
    /// The operator class of the index for each referenced column.
    pub(super) classes: &'a [u32],
}

impl Definer<'_, '_> {
    /// The check of `ATAddForeignKeyConstraint` that a permanent table refers only to a permanent table, an unlogged table only to a permanent or an unlogged table, and a temporary table only to a temporary table of the same session.
    fn check_fk_persistence(&self, table: u32, pk: u32) -> Result<()> {
        let message = match (self.persistence(table), self.persistence(pk)) {
            (b'p', b'p') | (b'u', b'p' | b'u') => return Ok(()),
            (b'p', _) => "constraints on permanent tables may reference only permanent tables",
            (b'u', _) => {
                "constraints on unlogged tables may reference only permanent or unlogged tables"
            }
            (_, b't') if self.is_own_temp(table) && self.is_own_temp(pk) => return Ok(()),
            (_, b't') => {
                "constraints on temporary tables must involve temporary tables of this session"
            }
            _ => "constraints on temporary tables may reference only temporary tables",
        };
        Err(Error::new(SqlState::INVALID_TABLE_DEFINITION, message))
    }

    /// Adds the foreign key `con` to the table, as `ATAddForeignKeyConstraint` does. The constraint is not valid when `valid` is false.
    pub(super) fn add_foreign_key(
        &mut self,
        table: u32,
        con: &Constraint,
        valid: bool,
    ) -> Result<()> {
        let (table_name, namespace) = match self.catalog.relation(table) {
            Some(rel) => (rel.name.clone(), rel.namespace),
            None => {
                return Err(Error::internal(format!("cache lookup failed for relation {table}")));
            }
        };
        let fk_names = names(&con.fk_attrs);
        let name = match con.conname.as_deref() {
            Some(name) => {
                if self.catalog.constraints_of(table).any(|c| c.name == name) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{name}\" for relation \"{table_name}\" already exists"
                        ),
                    ));
                }
                name.to_string()
            }
            None => self.catalog.choose_constraint_name(
                &table_name,
                Some(&name_addition(fk_names.iter().copied())),
                "fkey",
                namespace,
            ),
        };
        let at = place(con.location);
        if con.fk_with_period || con.pk_with_period {
            return Err(not_yet("PERIOD", at));
        }
        if !con.fk_del_set_cols.is_empty() {
            return Err(not_yet("a column list for ON DELETE SET", at));
        }
        let rv = con.pktable.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let pk_name = relname(&rv).to_string();
        let kind = self.kind_of(found);
        if kind == b'i' || kind == b'I' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{pk_name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        if kind != b'r' && kind != b'p' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("referenced relation \"{pk_name}\" is not a table"),
            ));
        }
        if self.is_system(found) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{pk_name}\" is a system catalog"),
            ));
        }
        let Found::User(pk) = found else {
            return Err(not_yet("a foreign key to a built-in table", at));
        };
        self.check_fk_persistence(table, pk)?;
        let fk_keys = self.column_list(table, &fk_names)?;
        let mut pk_names: Vec<String> =
            names(&con.pk_attrs).iter().map(|s| s.to_string()).collect();
        let (pk_keys, index, classes) = if pk_names.is_empty() {
            let (index, keys, classes) = self.primary_key(pk, &pk_name)?;
            let rel = self.catalog.relation(pk);
            pk_names = keys
                .iter()
                .map(|&k| {
                    rel.and_then(|r| r.column(k)).map_or_else(String::new, |c| c.name.clone())
                })
                .collect();
            (keys, index, classes)
        } else {
            let list: Vec<&str> = pk_names.iter().map(String::as_str).collect();
            let keys = self.column_list(pk, &list)?;
            let (index, classes) = self.unique_index(pk, &keys, &pk_name)?;
            (keys, index, classes)
        };
        let pk_owner = self.catalog.relation(pk).map_or(0, |r| r.owner);
        if !self.is_superuser() && !self.owns(pk_owner) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {pk_name}"),
            ));
        }
        self.check_generated_keys(table, &fk_keys, con)?;
        if fk_keys.len() != pk_keys.len() {
            return Err(invalid(
                "number of referencing and referenced columns for foreign key disagree".into(),
            ));
        }
        let keys = Keys { table, fk: &fk_keys, pk, pk_keys: &pk_keys, classes: &classes };
        let names = (fk_names.as_slice(), pk_names.as_slice());
        let (pf_eq, pp_eq, ff_eq) = self.foreign_operators(&name, &keys, names)?;
        let foreign = ForeignKey {
            table: pk,
            keys: pk_keys,
            update: char::from(con.fk_upd_action),
            delete: char::from(con.fk_del_action),
            match_type: char::from(con.fk_matchtype),
            pf_eq,
            pp_eq,
            ff_eq,
        };
        let new = NewForeignKey {
            table,
            name: Some(name),
            keys: fk_keys,
            index,
            deferrable: con.deferrable,
            deferred: con.initdeferred,
            foreign,
        };
        let oid = self.catalog.add_fk_constraint(new, 0, FkSide::Both)?;
        // A `NOT VALID` foreign key does not check the rows that the table has, and its clones are not valid either.
        if !valid {
            self.catalog.set_constraint_validated(oid, false)?;
        }
        // A `NOT ENFORCED` foreign key is not valid, and it and its clones have no triggers.
        if !con.is_enforced {
            self.catalog.set_constraint_enforced(oid, false)?;
        }
        self.fk_recurse_referenced(oid, None)?;
        self.fk_recurse_referencing(oid, None)
    }

    /// The checks of `ATAddForeignKeyConstraint` for the generated columns of the referencing columns: the actions that write the column are not allowed, and a virtual column cannot be in a foreign key.
    fn check_generated_keys(&self, table: u32, keys: &[i16], con: &Constraint) -> Result<()> {
        let rel = self.catalog.relation(table);
        for &key in keys {
            let generated = rel.and_then(|r| r.column(key)).map_or(0, |c| c.generated);
            if generated == 0 {
                continue;
            }
            let invalid = |action: &str| {
                Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!(
                        "invalid {action} action for foreign key constraint containing generated column"
                    ),
                )
            };
            if matches!(con.fk_upd_action, b'n' | b'd' | b'c') {
                return Err(invalid("ON UPDATE"));
            }
            if matches!(con.fk_del_action, b'n' | b'd') {
                return Err(invalid("ON DELETE"));
            }
            if generated == b'v' {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "foreign key constraints on virtual generated columns are not supported",
                ));
            }
        }
        Ok(())
    }

    /// The equality operators `conpfeqop`, `conppeqop` and `conffeqop` of `ATAddForeignKeyConstraint` for the columns of a foreign key, from the operator classes of the unique index of the referenced table.
    ///
    /// # Errors
    ///
    /// `42804` for a pair of columns that no operator can compare.
    pub(super) fn foreign_operators(
        &self,
        name: &str,
        keys: &Keys<'_>,
        names: (&[impl AsRef<str>], &[impl AsRef<str>]),
    ) -> Result<(Vec<u32>, Vec<u32>, Vec<u32>)> {
        let (mut pf_eq, mut pp_eq, mut ff_eq) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..keys.fk.len() {
            let fk_type = self.column_type(keys.table, keys.fk[i]);
            let pk_type = self.column_type(keys.pk, keys.pk_keys[i]);
            let class = builtin::opclasses().iter().find(|c| c.oid == keys.classes[i]);
            let Some(class) = class else {
                return Err(Error::internal(format!(
                    "cache lookup failed for opclass {}",
                    keys.classes[i]
                )));
            };
            let (family, intype) = (class.family, class.intype);
            let pp = builtin::opfamily_member(family, intype, intype, EQUAL_STRATEGY).ok_or_else(
                || {
                    Error::internal(format!(
                        "missing operator {EQUAL_STRATEGY}({intype},{intype}) in opfamily {family}"
                    ))
                },
            )?;
            let fk_base = types::base(fk_type);
            let mut pf = builtin::opfamily_member(family, intype, fk_base, EQUAL_STRATEGY);
            let mut ff =
                pf.and_then(|_| builtin::opfamily_member(family, fk_base, fk_base, EQUAL_STRATEGY));
            if !(pf.is_some() && ff.is_some())
                && can_coerce(&[pk_type, fk_type], &[intype, intype], Context::Implicit)
            {
                pf = Some(pp);
                ff = Some(pp);
            }
            let Some(pf) = pf else {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("foreign key constraint \"{name}\" cannot be implemented"),
                )
                .with_detail(format!(
                    "Key columns \"{}\" of the referencing table and \"{}\" of the referenced table are of incompatible types: {} and {}.",
                    names.0[i].as_ref(),
                    names.1[i].as_ref(),
                    types::name(fk_type),
                    types::name(pk_type)
                )));
            };
            pf_eq.push(pf);
            pp_eq.push(pp);
            ff_eq.push(ff.unwrap_or(0));
        }
        Ok((pf_eq, pp_eq, ff_eq))
    }

    /// `transformColumnNameList`: the numbers of the columns of a foreign key.
    fn column_list(&self, table: u32, list: &[&str]) -> Result<Vec<i16>> {
        let rel = self.catalog.relation(table);
        let mut keys = Vec::with_capacity(list.len());
        for name in list {
            let Some(number) = rel.and_then(|r| r.column_number(name)) else {
                if super::table::SYSTEM_COLUMNS.contains(name) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "system columns cannot be used in foreign keys",
                    ));
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!(
                        "column \"{name}\" referenced in foreign key constraint does not exist"
                    ),
                ));
            };
            if keys.len() >= MAX_KEYS {
                return Err(Error::new(
                    SqlState::TOO_MANY_COLUMNS,
                    format!("cannot have more than {MAX_KEYS} keys in a foreign key"),
                ));
            }
            keys.push(number);
        }
        Ok(keys)
    }

    /// The type of a column.
    fn column_type(&self, table: u32, number: i16) -> u32 {
        self.catalog.relation(table).and_then(|r| r.column(number)).map_or(0, |c| c.ty)
    }

    /// `transformFkeyGetPrimaryKey`: the index, the columns and the operator classes of the primary key.
    fn primary_key(&self, table: u32, name: &str) -> Result<(u32, Vec<i16>, Vec<u32>)> {
        let index = self
            .catalog
            .constraints_of(table)
            .find(|c| c.kind == ConKind::Primary)
            .map(|c| c.index)
            .ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("there is no primary key for referenced table \"{name}\""),
                )
            })?;
        let info = self.catalog.relation(index).and_then(|r| r.index.as_ref());
        let Some(info) = info else {
            return Err(Error::internal(format!("cache lookup failed for index {index}")));
        };
        if !info.immediate {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot use a deferrable primary key for referenced table \"{name}\""),
            ));
        }
        let count = usize::try_from(info.key_count).unwrap_or_default();
        Ok((index, info.keys[..count].to_vec(), info.classes[..count].to_vec()))
    }

    /// `transformFkeyCheckAttrs`: the first unique index, in OID order, on exactly the columns. The operator classes follow the order of the columns.
    fn unique_index(&self, table: u32, keys: &[i16], name: &str) -> Result<(u32, Vec<u32>)> {
        for (i, key) in keys.iter().enumerate() {
            if keys[..i].contains(key) {
                return Err(invalid(
                    "foreign key referenced-columns list must not contain duplicates".into(),
                ));
            }
        }
        let mut deferrable = false;
        for rel in self.catalog.relations() {
            let Some(info) = rel.index.as_ref() else { continue };
            if !matches!(rel.kind, RelKind::Index | RelKind::PartitionedIndex)
                || info.table != table
                || !info.unique
                || usize::try_from(info.key_count).ok() != Some(keys.len())
                || info.exprs.is_some()
                || info.predicate.is_some()
            {
                continue;
            }
            let mut classes = Vec::with_capacity(keys.len());
            for key in keys {
                match info.keys[..keys.len()].iter().position(|k| k == key) {
                    Some(j) => classes.push(info.classes[j]),
                    None => break,
                }
            }
            if classes.len() != keys.len() {
                continue;
            }
            if !info.immediate {
                deferrable = true;
                continue;
            }
            return Ok((rel.oid, classes));
        }
        if deferrable {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot use a deferrable unique constraint for referenced table \"{name}\""
                ),
            ));
        }
        Err(invalid(format!(
            "there is no unique constraint matching given keys for referenced table \"{name}\""
        )))
    }
}

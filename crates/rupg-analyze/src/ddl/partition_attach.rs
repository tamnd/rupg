//! `ALTER TABLE ... ATTACH PARTITION` and `DETACH PARTITION`, and `ALTER INDEX ... ATTACH PARTITION`, as `ATExecAttachPartition`, `ATExecDetachPartition` and `ATExecAttachPartitionIdx` run them. The caller checks the rows of the new partition and of the default partition against their new bounds after the statement.
//!
//! rupg runs `DETACH PARTITION ... CONCURRENTLY` in one transaction, so a detach is never pending, and `FINALIZE` has no detach to complete.

use rupg_catalog::{ConKind, PartitionBound, RelKind, Relation, TRIGGER_ROW};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterTableCmd, AlterTableType, Node, PartitionBoundSpec, RangeVar};

use super::alter::Target;
use super::attmap::AttMap;
use super::comment::not_owner;
use super::drop::owner_word;
use super::partition_index::same_definition;
use super::{Definer, Found, PartitionCheck, not_yet, relkind_detail, relname};

impl Definer<'_, '_> {
    /// `ATPrepCmd` and `ATExecCmd` for the partition subcommands of `ALTER TABLE` and `ALTER INDEX`. The grammar gives each of them as the only subcommand.
    pub(super) fn partition_command(&mut self, target: &Target, cmd: &AlterTableCmd) -> Result<()> {
        let allowed: &[u8] =
            if cmd.subtype == AlterTableType::AT_AttachPartition { b"pI" } else { b"p" };
        self.simple_permissions(target, cmd, allowed)?;
        let Some(Node::PartitionCmd(part)) = &cmd.def else {
            return Err(Error::internal("a partition command with no partition"));
        };
        let rv = part.name.as_deref().cloned().unwrap_or_default();
        match cmd.subtype {
            AlterTableType::AT_AttachPartition if target.kind == b'I' => {
                // `transformPartitionCmd`: `ALTER TABLE` can give a bound to an index.
                if part.bound.is_some() {
                    return Err(Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!("\"{}\" is not a partitioned table", target.name),
                    ));
                }
                self.attach_partition_index(target, &rv)
            }
            AlterTableType::AT_AttachPartition => {
                let Some(spec) = part.bound.as_deref() else {
                    return Err(Error::internal("ATTACH PARTITION with no bound"));
                };
                self.attach_partition(target, cmd, &rv, spec)
            }
            AlterTableType::AT_DetachPartition => {
                self.detach_partition(target, &rv, part.concurrent)
            }
            _ => self.detach_partition_finalize(target, &rv),
        }
    }

    /// `ATExecAttachPartition`: the table `rv` becomes a partition of the partitioned table `target` with the bound `spec`.
    fn attach_partition(
        &mut self,
        target: &Target,
        cmd: &AlterTableCmd,
        rv: &RangeVar,
        spec: &PartitionBoundSpec,
    ) -> Result<()> {
        let parent = target.oid;
        let name = relname(rv);
        // `transformPartitionCmd` transforms the bound before the other checks.
        self.partition_bound(name, parent, spec, false)?;
        let child = self.open_attached(rv, cmd)?;
        let lookup = |oid: u32| Error::internal(format!("cache lookup failed for relation {oid}"));
        let rel = self.catalog.relation(child).cloned().ok_or_else(|| lookup(child))?;
        let parent_rel = self.catalog.relation(parent).cloned().ok_or_else(|| lookup(parent))?;
        if rel.partition_of.is_some() {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is already a partition", rel.name),
            ));
        }
        if rel.of_type != 0 {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "cannot attach a typed table as partition",
            ));
        }
        // `GetRelationExcludedPublications`: a change of the tree of partitions could change what the publications publish.
        let excluded: Vec<String> = self
            .catalog
            .publication_rels()
            .filter(|r| r.relation == child && r.except)
            .filter_map(|r| self.catalog.publication(r.publication))
            .map(|p| format!("\"{}\"", p.name))
            .collect();
        if !excluded.is_empty() {
            let word = if excluded.len() == 1 { "publication" } else { "publications" };
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot attach table \"{}\" as partition because it is referenced in {word} {} EXCEPT clause",
                    rel.name,
                    excluded.join(", ")
                ),
            )
            .with_detail("The publication EXCEPT clause cannot contain tables that are partitions.")
            .with_hint(
                "Change the publication's EXCEPT clause using ALTER PUBLICATION ... SET ALL TABLES.",
            ));
        }
        // A table of `INHERITS` cannot be a partition, and a table with children of `INHERITS` cannot either.
        if !rel.inherits.is_empty() {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "cannot attach inheritance child as partition",
            ));
        }
        if rel.kind == RelKind::Table && !self.catalog.children(child).is_empty() {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "cannot attach inheritance parent as partition",
            ));
        }
        if child == parent || self.descendants(child).contains(&parent) {
            return Err(Error::new(SqlState::DUPLICATE_TABLE, "circular inheritance not allowed")
                .with_detail(format!(
                    "\"{}\" is already a child of \"{}\".",
                    target.name, rel.name
                )));
        }
        let parent_temp = self.catalog.is_temp_schema(parent_rel.namespace);
        if parent_temp != self.catalog.is_temp_schema(rel.namespace) {
            let message = if parent_temp {
                format!(
                    "cannot attach a permanent relation as partition of temporary relation \"{}\"",
                    target.name
                )
            } else {
                format!(
                    "cannot attach a temporary relation as partition of permanent relation \"{}\"",
                    target.name
                )
            };
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message));
        }
        for column in rel.columns.iter().filter(|c| !c.dropped) {
            if column.identity != 0 {
                return Err(Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "table \"{}\" being attached contains an identity column \"{}\"",
                        rel.name, column.name
                    ),
                )
                .with_detail("The new partition may not contain an identity column."));
            }
            if parent_rel.column_number(&column.name).is_none() {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "table \"{}\" contains column \"{}\" not found in parent \"{}\"",
                        rel.name, column.name, target.name
                    ),
                )
                .with_detail("The new partition may contain only the columns present in parent."));
            }
        }
        self.check_transition_triggers(&rel, true)?;
        let bound = self.partition_bound(&rel.name, parent, spec, true)?;
        self.merge_attributes(&rel, &parent_rel)?;
        self.merge_constraints(&rel, &parent_rel)?;
        self.catalog.attach_partition(child, parent, bound)?;
        self.ensure_partition_indexes(&rel, &parent_rel)?;
        self.clone_triggers(parent, child)?;
        self.clone_foreign_keys(parent, child)?;
        // `QueuePartitionConstraintValidation` for the new partition, then for the default partition, whose bound is now smaller.
        let default = self.catalog.partitions(parent).into_iter().find(|r| {
            r.oid != child
                && r.partition_of.as_ref().is_some_and(|p| p.bound == PartitionBound::Default)
        });
        let default = default.map(|r| r.oid);
        self.checks.push(PartitionCheck { table: child, default: false });
        self.scans.extend(self.leaves(child));
        if let Some(default) = default {
            self.checks.push(PartitionCheck { table: default, default: true });
            self.scans.extend(self.leaves(default));
        }
        Ok(())
    }

    /// `table_openrv` and `ATSimplePermissions` for the table that `ATTACH PARTITION` attaches, and for the new parent of `INHERIT`.
    pub(super) fn open_attached(&self, rv: &RangeVar, cmd: &AlterTableCmd) -> Result<u32> {
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        let kind = self.kind_of(found);
        if matches!(kind, b'i' | b'I' | b'c') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let (oid, namespace, owner) = match found {
            Found::User(oid) => {
                let rel = self.catalog.relation(oid);
                (oid, rel.map_or(0, |r| r.namespace), rel.map_or(0, |r| r.owner))
            }
            Found::Builtin(row) => (row.oid, row.namespace, row.owner),
        };
        let target = Target { oid, kind, name: name.to_string(), namespace };
        self.simple_permissions(&target, cmd, b"rpf")?;
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner(owner_word(kind), name));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let Found::User(oid) = found else {
            let what = if cmd.subtype == AlterTableType::AT_AddInherit {
                "INHERIT of a built-in relation"
            } else {
                "ATTACH PARTITION of a built-in relation"
            };
            return Err(not_yet(what, None));
        };
        Ok(oid)
    }

    /// `FindTriggerIncompatibleWithInheritance`: a row trigger with a transition table cannot be on a partition, or on a child of `INHERIT` when `partition` is false.
    pub(super) fn check_transition_triggers(&self, rel: &Relation, partition: bool) -> Result<()> {
        let mut found: Vec<&str> = self
            .catalog
            .triggers()
            .filter(|t| t.relation == rel.oid && t.kind & TRIGGER_ROW != 0)
            .filter(|t| t.old_table.is_some() || t.new_table.is_some())
            .map(|t| t.name.as_str())
            .collect();
        found.sort_unstable();
        if let Some(trigger) = found.first() {
            let (becoming, detail) = if partition {
                (
                    "a partition",
                    "ROW triggers with transition tables are not supported on partitions.",
                )
            } else {
                (
                    "an inheritance child",
                    "ROW triggers with transition tables are not supported in inheritance hierarchies.",
                )
            };
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "trigger \"{trigger}\" prevents table \"{}\" from becoming {becoming}",
                    rel.name
                ),
            )
            .with_detail(detail));
        }
        Ok(())
    }

    /// `MergeAttributesIntoExisting`: each column of the parent must be in the child with the same type and collation, and with a not-null constraint when the parent has one. Each column then counts the parent. The columns of a partition come only from the parent and have its identity, but a child of `INHERIT` keeps its own columns local and has no identity from the parent.
    pub(super) fn merge_attributes(&mut self, rel: &Relation, parent: &Relation) -> Result<()> {
        let mut merged = Vec::new();
        for column in parent.columns.iter().filter(|c| !c.dropped) {
            let Some(number) = rel.column_number(&column.name) else {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("child table is missing column \"{}\"", column.name),
                ));
            };
            let Some(own) = rel.column(number) else { continue };
            if own.ty != column.ty || own.typmod != column.typmod {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "child table \"{}\" has different type for column \"{}\"",
                        rel.name, column.name
                    ),
                ));
            }
            if own.collation != column.collation {
                return Err(Error::new(
                    SqlState::COLLATION_MISMATCH,
                    format!(
                        "child table \"{}\" has different collation for column \"{}\"",
                        rel.name, column.name
                    ),
                ));
            }
            let parent_number = parent.column_number(&column.name).unwrap_or(0);
            let inherited = self.catalog.constraints_of(parent.oid).any(|c| {
                c.kind == ConKind::NotNull
                    && c.keys.first() == Some(&parent_number)
                    && !c.no_inherit
            });
            if column.not_null && !own.not_null && inherited {
                return Err(must_be_not_null(&column.name, &rel.name));
            }
            if column.generated != 0 && own.generated == 0 {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("column \"{}\" in child table must be a generated column", column.name),
                ));
            }
            if own.generated != 0 && column.generated == 0 {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" in child table must not be a generated column",
                        column.name
                    ),
                ));
            }
            if column.generated != 0 && own.generated != 0 && column.generated != own.generated {
                let kind = |g: u8| if g == b's' { "STORED" } else { "VIRTUAL" };
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" inherits from generated column of different kind",
                        column.name
                    ),
                )
                .with_detail(format!(
                    "Parent column is {}, child column is {}.",
                    kind(column.generated),
                    kind(own.generated)
                )));
            }
            let inhcount = own.inhcount.checked_add(1).ok_or_else(too_many_parents)?;
            merged.push((number, parent_number, own.local, inhcount));
        }
        let partition = parent.kind == RelKind::Partitioned;
        for (number, up, local, inhcount) in merged {
            if partition {
                let identity = parent.column(up).map_or(0, |c| c.identity);
                self.catalog.set_identity(rel.oid, number, identity)?;
            }
            self.catalog.set_column_inherited(rel.oid, number, local && !partition, inhcount)?;
        }
        Ok(())
    }

    /// `MergeConstraintsIntoExisting`: each check constraint and not-null constraint of the parent, in the order of their names, must be in the child with the same definition, with the numbers of the columns of the child. Each constraint then counts the parent. The constraints of a partition come only from the parent, but a child of `INHERIT` keeps its own constraints local.
    pub(super) fn merge_constraints(&mut self, rel: &Relation, parent: &Relation) -> Result<()> {
        let mut wanted: Vec<_> = self
            .catalog
            .constraints_of(parent.oid)
            .filter(|c| matches!(c.kind, ConKind::Check | ConKind::NotNull) && !c.no_inherit)
            .cloned()
            .collect();
        wanted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut own: Vec<_> = self.catalog.constraints_of(rel.oid).cloned().collect();
        own.sort_by(|a, b| a.name.cmp(&b.name));
        let map = AttMap::by_name(parent, rel);
        let mut merged = Vec::new();
        for con in &wanted {
            let column = con.keys.first().and_then(|&n| parent.column(n)).map(|c| c.name.as_str());
            let child_number = column.and_then(|name| rel.column_number(name));
            let found = own.iter().find(|c| {
                c.kind == con.kind
                    && match con.kind {
                        ConKind::Check => c.name == con.name,
                        _ => child_number.is_some() && c.keys.first() == child_number.as_ref(),
                    }
            });
            let Some(found) = found else {
                if con.kind == ConKind::NotNull {
                    return Err(must_be_not_null(column.unwrap_or_default(), &rel.name));
                }
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("child table is missing constraint \"{}\"", con.name),
                ));
            };
            let expr = con.expr.as_deref().map(|text| map.text(text)).transpose()?;
            if con.kind == ConKind::Check
                && (found.deferrable != con.deferrable
                    || found.deferred != con.deferred
                    || without_locations(found.expr.as_deref())
                        != without_locations(expr.as_deref()))
            {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "child table \"{}\" has different definition for check constraint \"{}\"",
                        rel.name, con.name
                    ),
                ));
            }
            if found.no_inherit {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "constraint \"{}\" conflicts with non-inherited constraint on child table \"{}\"",
                        found.name, rel.name
                    ),
                ));
            }
            if con.validated && found.enforced && !found.validated {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "constraint \"{}\" conflicts with NOT VALID constraint on child table \"{}\"",
                        found.name, rel.name
                    ),
                ));
            }
            if con.enforced && !found.enforced {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "constraint \"{}\" conflicts with NOT ENFORCED constraint on child table \"{}\"",
                        found.name, rel.name
                    ),
                ));
            }
            let inhcount = found.inhcount.checked_add(1).ok_or_else(too_many_parents)?;
            merged.push((found.oid, found.local, inhcount));
        }
        let partition = parent.kind == RelKind::Partitioned;
        for (oid, local, inhcount) in merged {
            self.catalog.set_constraint_inherited(oid, local && !partition, inhcount)?;
        }
        Ok(())
    }

    /// `AttachPartitionEnsureIndexes`: each index of the parent gets an index of the new partition, in the order of the OIDs of the indexes of the parent. A valid index of the partition with the same definition that is not a partition of another index yet becomes its partition, else the partition gets a new index.
    fn ensure_partition_indexes(&mut self, rel: &Relation, parent: &Relation) -> Result<()> {
        let indexes: Vec<u32> = self
            .catalog
            .relations()
            .filter(|r| r.index.as_ref().is_some_and(|i| i.table == parent.oid))
            .map(|r| r.oid)
            .collect();
        // A foreign table has no indexes, so the parent can have no unique index.
        if rel.kind == RelKind::Foreign {
            if indexes.iter().any(|&index| self.is_unique_index(index)) {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot attach foreign table \"{}\" as partition of partitioned table \"{}\"",
                        rel.name, parent.name
                    ),
                )
                .with_detail(format!(
                    "Partitioned table \"{}\" contains unique indexes.",
                    parent.name
                )));
            }
            return Ok(());
        }
        for index in indexes {
            let constraint = self
                .catalog
                .constraints_of(parent.oid)
                .find(|c| c.index == index && matches!(c.kind, ConKind::Primary | ConKind::Unique))
                .map(|c| c.oid);
            match self.matching_index(rel.oid, index, constraint, true) {
                Some((found, found_constraint, _)) => {
                    self.catalog.set_index_parent(found, index)?;
                    if let (Some(child), Some(parent)) = (found_constraint, constraint) {
                        self.catalog.set_constraint_parent(child, parent)?;
                    }
                }
                None => {
                    self.define_partition_index(rel.oid, index, constraint, true)?;
                }
            }
        }
        Ok(())
    }

    /// `ATExecDetachPartition`: the partition `rv` of the partitioned table `target` becomes a table of its own, with its rows.
    fn detach_partition(&mut self, target: &Target, rv: &RangeVar, concurrent: bool) -> Result<()> {
        let has_default =
            self.catalog.partitions(target.oid).into_iter().any(|r| {
                r.partition_of.as_ref().is_some_and(|p| p.bound == PartitionBound::Default)
            });
        if has_default && concurrent {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot detach partitions concurrently when a default partition exists",
            ));
        }
        let child = self.open_detached(target, rv)?;
        self.detach_checks(child);
        self.remove_inheritance(child, target.oid)?;
        // `DetachPartitionFinalize`: the columns of the partition lose their identity. The sequence stays with the parent.
        let identities: Vec<i16> = self.catalog.relation(child).map_or_else(Vec::new, |r| {
            (1..)
                .zip(&r.columns)
                .filter(|(_, c)| !c.dropped && c.identity != 0)
                .map(|(number, _)| number)
                .collect()
        });
        for number in identities {
            self.catalog.set_identity(child, number, 0)?;
        }
        self.drop_cloned_triggers(child)?;
        self.detach_foreign_keys(child)?;
        // `DetachPartitionFinalize`: the indexes of the partition are no longer partitions of the indexes of the parent.
        let indexes: Vec<(u32, u32)> = self
            .catalog
            .relations()
            .filter_map(|r| {
                let info = r.index.as_ref().filter(|i| i.table == child && i.parent != 0)?;
                Some((r.oid, info.parent))
            })
            .collect();
        for (index, parent_index) in indexes {
            self.catalog.clear_index_parent(index)?;
            let index_constraint = |table: u32, index: u32| {
                self.catalog
                    .constraints_of(table)
                    .find(|c| {
                        c.index == index && matches!(c.kind, ConKind::Primary | ConKind::Unique)
                    })
                    .map(|c| c.oid)
            };
            let own = index_constraint(child, index);
            if index_constraint(target.oid, parent_index).is_some()
                && let Some(own) = own
            {
                self.catalog.clear_constraint_parent(own)?;
            }
        }
        Ok(())
    }

    /// `ATExecDetachPartitionFinalize`: rupg has no pending detach, so a partition gives the error of `DeleteInheritsTuple`.
    fn detach_partition_finalize(&mut self, target: &Target, rv: &RangeVar) -> Result<()> {
        let child = self.open_detached(target, rv)?;
        let name = self.catalog.relation(child).map_or("", |r| r.name.as_str());
        Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("cannot complete detaching partition \"{name}\""),
        )
        .with_detail("There's no pending concurrent detach."))
    }

    /// `table_openrv` for the partition of `DETACH PARTITION`, and the check of `RemoveInheritance` that it is a partition of the parent.
    fn open_detached(&self, target: &Target, rv: &RangeVar) -> Result<u32> {
        let child = self.open_table(rv)?;
        let name = relname(rv);
        let parent =
            self.catalog.relation(child).and_then(|r| r.partition_of.as_ref()).map(|p| p.parent);
        if parent != Some(target.oid) {
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{name}\" is not a partition of relation \"{}\"", target.name),
            ));
        }
        Ok(child)
    }

    /// `table_openrv`: the relation that a name gives, which cannot be an index or a composite type.
    pub(super) fn open_table(&self, rv: &RangeVar) -> Result<u32> {
        let found = self.lookup_relation(rv)?;
        let kind = self.kind_of(found);
        if matches!(kind, b'i' | b'I' | b'c') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{}\"", relname(rv)),
            )
            .with_detail(relkind_detail(kind)));
        }
        Ok(match found {
            Found::User(oid) => oid,
            Found::Builtin(row) => row.oid,
        })
    }

    /// `RemoveInheritance`: the columns and the constraints that `child` has from the parent no longer count it, and they become local again when they have no other parent. A partition loses its bound, and a child of `INHERITS` loses the parent.
    pub(super) fn remove_inheritance(&mut self, child: u32, parent: u32) -> Result<()> {
        let lookup = |oid: u32| Error::internal(format!("cache lookup failed for relation {oid}"));
        let rel = self.catalog.relation(child).cloned().ok_or_else(|| lookup(child))?;
        let parent_rel = self.catalog.relation(parent).cloned().ok_or_else(|| lookup(parent))?;
        for (i, column) in rel.columns.iter().enumerate() {
            if column.dropped
                || column.inhcount <= 0
                || parent_rel.column_number(&column.name).is_none()
            {
                continue;
            }
            let number = i16::try_from(i + 1).unwrap_or_default();
            let inhcount = column.inhcount - 1;
            self.catalog.set_column_inherited(
                child,
                number,
                column.local || inhcount == 0,
                inhcount,
            )?;
        }
        let mut checks: Vec<String> = Vec::new();
        let mut not_nulls: Vec<i16> = Vec::new();
        for con in self.catalog.constraints_of(parent).filter(|c| !c.no_inherit) {
            match con.kind {
                ConKind::Check => checks.push(con.name.clone()),
                ConKind::NotNull => {
                    let column = con.keys.first().and_then(|&n| parent_rel.column(n));
                    if let Some(number) = column.and_then(|c| rel.column_number(&c.name)) {
                        not_nulls.push(number);
                    }
                }
                _ => {}
            }
        }
        let matched: Vec<(u32, bool, i16)> = self
            .catalog
            .constraints_of(child)
            .filter(|c| match c.kind {
                ConKind::Check => checks.contains(&c.name),
                ConKind::NotNull => c.keys.first().is_some_and(|n| not_nulls.contains(n)),
                _ => false,
            })
            .map(|c| (c.oid, c.local, c.inhcount))
            .collect();
        for (oid, local, inhcount) in matched {
            let inhcount = inhcount.saturating_sub(1);
            self.catalog.set_constraint_inherited(oid, local || inhcount == 0, inhcount)?;
        }
        if rel.partition_of.is_some() {
            self.catalog.detach_partition(child)
        } else {
            self.catalog.remove_inherit(child, parent)
        }
    }

    /// `ATExecAttachPartitionIdx`: the index `rv` of a partition becomes a partition of the partitioned index `target`, and `validatePartitionedIndex` then makes the index valid when each partition has a valid index.
    fn attach_partition_index(&mut self, target: &Target, rv: &RangeVar) -> Result<()> {
        let lookup = |oid: u32| Error::internal(format!("cache lookup failed for index {oid}"));
        let parent = self.catalog.relation(target.oid).and_then(|r| r.index.clone());
        let parent = parent.ok_or_else(|| lookup(target.oid))?;
        let parent_table = parent.table;
        let parent_table_name =
            self.catalog.relation(parent_table).map_or("", |r| r.name.as_str()).to_string();
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        if !matches!(self.kind_of(found), b'i' | b'I') {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("\"{name}\" is not an index"),
            ));
        }
        let cannot = |detail: String| {
            Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot attach index \"{name}\" as a partition of index \"{}\"",
                    target.name
                ),
            )
            .with_detail(detail)
        };
        let Found::User(index) = found else {
            return Err(cannot(format!(
                "Index \"{name}\" is not an index on any partition of table \"{parent_table_name}\"."
            )));
        };
        let info = self
            .catalog
            .relation(index)
            .and_then(|r| r.index.clone())
            .ok_or_else(|| lookup(index))?;
        let table = self.catalog.relation(info.table).cloned().ok_or_else(|| lookup(info.table))?;
        if info.parent == target.oid {
            if !parent.valid {
                self.validate_partitioned_index(target.oid, parent_table)?;
            }
            return Ok(());
        }
        // `refuseDupeIndexAttach`
        let existing = self.catalog.relations().find(|r| {
            r.index.as_ref().is_some_and(|i| i.table == table.oid && i.parent == target.oid)
        });
        if let Some(existing) = existing {
            return Err(cannot(format!(
                "Another index \"{}\" is already attached for partition \"{}\".",
                existing.name, table.name
            )));
        }
        if info.parent != 0 {
            return Err(cannot(format!("Index \"{name}\" is already attached to another index.")));
        }
        if table.partition_of.as_ref().map(|p| p.parent) != Some(parent_table) {
            return Err(cannot(format!(
                "Index \"{name}\" is not an index on any partition of table \"{parent_table_name}\"."
            )));
        }
        let invalid = |detail: String| {
            Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "cannot attach index \"{name}\" as a partition of index \"{}\"",
                    target.name
                ),
            )
            .with_detail(detail)
        };
        let wanted = self.att_map(parent_table, table.oid)?.index(&parent)?;
        if !same_definition(&info, &wanted) {
            return Err(invalid("The index definitions do not match.".into()));
        }
        let index_constraint = |table: u32, index: u32| {
            self.catalog
                .constraints_of(table)
                .find(|c| c.index == index && matches!(c.kind, ConKind::Primary | ConKind::Unique))
                .map(|c| c.oid)
        };
        let constraint = index_constraint(parent_table, target.oid);
        let own = index_constraint(table.oid, index);
        if constraint.is_some() && own.is_none() {
            return Err(invalid(format!(
                "The index \"{}\" belongs to a constraint in table \"{parent_table_name}\" but no constraint exists for index \"{name}\".",
                target.name
            )));
        }
        if parent.primary {
            let key_count = usize::try_from(info.key_count).unwrap_or_default();
            for &key in info.keys.iter().take(key_count) {
                if let Some(column) = table.column(key).filter(|c| !c.not_null) {
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "invalid primary key definition",
                    )
                    .with_detail(format!(
                        "Column \"{}\" of relation \"{}\" is not marked NOT NULL.",
                        column.name, table.name
                    )));
                }
            }
        }
        self.catalog.set_index_parent(index, target.oid)?;
        if let (Some(own), Some(constraint)) = (own, constraint) {
            self.catalog.set_constraint_parent(own, constraint)?;
        }
        self.validate_partitioned_index(target.oid, parent_table)
    }

    /// `validatePartitionedIndex`: a partitioned index is valid when each partition of its table has a valid index that is its partition. A valid index that is itself a partition then makes its parent index valid too, when it can.
    fn validate_partitioned_index(&mut self, index: u32, table: u32) -> Result<()> {
        let valid = self
            .catalog
            .relations()
            .filter(|r| r.index.as_ref().is_some_and(|i| i.parent == index && i.valid))
            .count();
        if valid != self.catalog.partitions(table).len() {
            return Ok(());
        }
        self.catalog.set_index_valid(index, true)?;
        let up =
            self.catalog.relation(index).and_then(|r| r.index.as_ref()).map_or(0, |i| i.parent);
        let up_table =
            self.catalog.relation(table).and_then(|r| r.partition_of.as_ref()).map(|p| p.parent);
        if up != 0
            && let Some(up_table) = up_table
        {
            return self.validate_partitioned_index(up, up_table);
        }
        Ok(())
    }

    /// The partitions of a partitioned table and their partitions, as `find_all_inheritors` gives them with no table itself.
    fn descendants(&self, table: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for part in self.catalog.partitions(table) {
            out.push(part.oid);
            out.extend(self.descendants(part.oid));
        }
        out
    }

    /// The leaf partitions of a table, or the table when it is not partitioned: the tables whose rows a check of a partition constraint reads.
    pub(super) fn leaves(&self, table: u32) -> Vec<u32> {
        if self.catalog.relation(table).is_some_and(|r| r.kind != RelKind::Partitioned) {
            return vec![table];
        }
        self.catalog.partitions(table).iter().flat_map(|p| self.leaves(p.oid)).collect()
    }
}

/// The error of `MergeAttributesIntoExisting` for a column of the partition with no not-null constraint.
fn must_be_not_null(column: &str, table: &str) -> Error {
    Error::new(
        SqlState::DATATYPE_MISMATCH,
        format!("column \"{column}\" in child table \"{table}\" must be marked NOT NULL"),
    )
}

/// The error of `MergeAttributesIntoExisting` and `MergeConstraintsIntoExisting` for a count of parents that does not fit in `int2`.
fn too_many_parents() -> Error {
    Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "too many inheritance parents")
}

/// The text of an expression with no locations, which `constraints_equivalent` compares as the text of `pg_get_expr`.
pub(super) fn without_locations(text: Option<&str>) -> String {
    let text = text.unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(" :location ") {
        out.push_str(&rest[..at]);
        let after = &rest[at + " :location ".len()..];
        let end = after.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

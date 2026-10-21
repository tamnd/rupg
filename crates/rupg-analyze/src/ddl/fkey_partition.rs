//! The foreign keys of partitioned tables, as `addFkRecurseReferenced`, `addFkRecurseReferencing`, `CloneForeignKeyConstraints` and `DetachPartitionFinalize` make and change them.
//!
//! A foreign key of a partitioned referencing table has a clone in each partition, with the check triggers of the partition. A foreign key that refers to a partitioned table has a clone for each partition of that table, with the action triggers of the partition. The clones of the referenced side keep the referencing table and get the name of the constraint with `_1`, `_2` and so on.

use rupg_catalog::{
    ConKind, Constraint, FkSide, NewForeignKey, ObjRef, PG_CONSTRAINT, PG_TRIGGER, RelKind,
};
use rupg_common::{Error, Result};

use super::Definer;

impl Definer<'_, '_> {
    /// `addFkRecurseReferenced`: the action triggers of the foreign key `constraint` on its referenced table, as clones of `parents` when it has them. For a partitioned referenced table, each partition gets a clone of the constraint, which goes on to the partitions of the partition.
    pub(super) fn fk_recurse_referenced(
        &mut self,
        constraint: u32,
        parents: Option<(u32, u32)>,
    ) -> Result<()> {
        let triggers = self.catalog.add_fk_action_triggers(constraint, parents)?;
        let con = self.foreign_constraint(constraint)?;
        let Some(foreign) = con.foreign.clone() else { return Ok(()) };
        if !self.is_partitioned(foreign.table) {
            return Ok(());
        }
        for partition in self.partition_oids(foreign.table) {
            let index = self.partition_index(partition, con.index)?;
            let keys = self.map_keys(foreign.table, partition, &foreign.keys)?;
            let new = NewForeignKey {
                table: con.relation,
                name: Some(con.name.clone()),
                keys: con.keys.clone(),
                index,
                deferrable: con.deferrable,
                deferred: con.deferred,
                foreign: rupg_catalog::ForeignKey { table: partition, keys, ..foreign.clone() },
            };
            let clone = self.catalog.add_fk_constraint(new, constraint, FkSide::Referenced)?;
            self.fk_recurse_referenced(clone, Some(triggers))?;
        }
        Ok(())
    }

    /// `addFkRecurseReferencing`: the check triggers of the foreign key `constraint` on its referencing table, as clones of `parents` when it has them. For a partitioned referencing table, each partition takes a foreign key of its own that is the same, or else it gets a clone of the constraint, which goes on to the partitions of the partition.
    pub(super) fn fk_recurse_referencing(
        &mut self,
        constraint: u32,
        parents: Option<(u32, u32)>,
    ) -> Result<()> {
        let triggers = self.catalog.add_fk_check_triggers(constraint, parents)?;
        let con = self.foreign_constraint(constraint)?;
        let Some(foreign) = con.foreign.clone() else { return Ok(()) };
        if !self.is_partitioned(con.relation) {
            return Ok(());
        }
        for partition in self.partition_oids(con.relation) {
            let keys = self.map_keys(con.relation, partition, &con.keys)?;
            if self.try_attach_foreign_key(partition, &con, &keys, triggers)? {
                continue;
            }
            let new = NewForeignKey {
                table: partition,
                name: Some(con.name.clone()),
                keys,
                index: con.index,
                deferrable: con.deferrable,
                deferred: con.deferred,
                foreign: foreign.clone(),
            };
            let clone = self.catalog.add_fk_constraint(new, constraint, FkSide::Referencing)?;
            self.fk_recurse_referencing(clone, Some(triggers))?;
        }
        Ok(())
    }

    /// `CloneForeignKeyConstraints`: the new partition `partition` of `parent` gets the foreign keys of `parent`, then the clones of the foreign keys that refer to `parent`.
    pub(super) fn clone_foreign_keys(&mut self, parent: u32, partition: u32) -> Result<()> {
        // `CloneFkReferencing`: a clone of a constraint whose parent is in the list comes with its parent.
        let own: Vec<Constraint> = self
            .catalog
            .constraints_of(parent)
            .filter(|c| c.kind == ConKind::Foreign)
            .cloned()
            .collect();
        let oids: Vec<u32> = own.iter().map(|c| c.oid).collect();
        for con in own.iter().filter(|c| !oids.contains(&c.parent)) {
            let Some(foreign) = con.foreign.clone() else { continue };
            let keys = self.map_keys(parent, partition, &con.keys)?;
            let triggers = self.fk_triggers(con.oid, parent, true);
            if self.try_attach_foreign_key(partition, con, &keys, triggers)? {
                continue;
            }
            let new = NewForeignKey {
                table: partition,
                name: Some(con.name.clone()),
                keys,
                index: con.index,
                deferrable: con.deferrable,
                deferred: con.deferred,
                foreign,
            };
            let clone = self.catalog.add_fk_constraint(new, con.oid, FkSide::Referencing)?;
            self.fk_recurse_referencing(clone, Some(triggers))?;
        }
        // `CloneFkReferenced`: a self-referencing foreign key gets no clone.
        let referring: Vec<Constraint> = self
            .catalog
            .constraints()
            .filter(|c| c.foreign.as_ref().is_some_and(|f| f.table == parent))
            .cloned()
            .collect();
        let oids: Vec<u32> = referring.iter().map(|c| c.oid).collect();
        for con in &referring {
            if oids.contains(&con.parent) || con.relation == parent || con.relation == partition {
                continue;
            }
            let Some(foreign) = con.foreign.clone() else { continue };
            let index = self.partition_index(partition, con.index)?;
            let keys = self.map_keys(parent, partition, &foreign.keys)?;
            let triggers = self.fk_triggers(con.oid, parent, false);
            let new = NewForeignKey {
                table: con.relation,
                name: Some(con.name.clone()),
                keys: con.keys.clone(),
                index,
                deferrable: con.deferrable,
                deferred: con.deferred,
                foreign: rupg_catalog::ForeignKey { table: partition, keys, ..foreign },
            };
            let clone = self.catalog.add_fk_constraint(new, con.oid, FkSide::Referenced)?;
            self.fk_recurse_referenced(clone, Some(triggers))?;
        }
        Ok(())
    }

    /// `tryAttachPartitionForeignKey` and `AttachPartitionForeignKey`: a foreign key of the partition with no parent that is the same as `parent`, with the columns `keys` of the partition, becomes a clone of `parent`. It loses its clones for the partitions of a partitioned referenced table and its action triggers, as the constraint `parent` does that work, and its check triggers become clones of `triggers`. False when the partition has no such foreign key.
    fn try_attach_foreign_key(
        &mut self,
        partition: u32,
        parent: &Constraint,
        keys: &[i16],
        triggers: (u32, u32),
    ) -> Result<bool> {
        let Some(wanted) = parent.foreign.as_ref() else { return Ok(false) };
        let found = self.catalog.constraints_of(partition).find(|c| {
            c.kind == ConKind::Foreign
                && c.parent == 0
                && c.keys == keys
                && c.deferrable == parent.deferrable
                && c.deferred == parent.deferred
                && c.foreign.as_ref().is_some_and(|f| {
                    f.table == wanted.table
                        && f.keys == wanted.keys
                        && f.pf_eq == wanted.pf_eq
                        && f.update == wanted.update
                        && f.delete == wanted.delete
                        && f.match_type == wanted.match_type
                })
        });
        let Some(oid) = found.map(|c| c.oid) else { return Ok(false) };
        // `RemoveInheritedConstraint`: the clones of the constraint for the partitions of the referenced table go, with their triggers.
        if self.is_partitioned(wanted.table) {
            let clones: Vec<u32> = self
                .catalog
                .constraints_of(partition)
                .filter(|c| c.parent == oid)
                .map(|c| c.oid)
                .collect();
            for clone in clones {
                self.drop_fk_clone(clone)?;
            }
        }
        let actions: Vec<u32> = self
            .catalog
            .triggers_of(wanted.table)
            .into_iter()
            .filter(|t| t.constraint == oid && t.constraint_relation == partition)
            .map(|t| t.oid)
            .collect();
        for trigger in actions {
            let object = ObjRef::new(PG_TRIGGER, trigger);
            self.catalog.drop_depends_of(object);
            self.perform(&[object], false)?;
        }
        self.catalog.set_constraint_parent(oid, parent.oid)?;
        let (insert, update) = self.fk_triggers(oid, partition, true);
        self.catalog.set_trigger_parent(insert, triggers.0);
        self.catalog.set_trigger_parent(update, triggers.1);
        Ok(true)
    }

    /// The part of `DetachPartitionFinalize` for the foreign keys of the partition `partition`: a clone of a foreign key of the parent is a foreign key of its own again, with its check triggers, and it gets its own action triggers and clones for the partitions of a partitioned referenced table. The clones for the partition of the foreign keys that refer to the parent go.
    pub(super) fn detach_foreign_keys(&mut self, partition: u32) -> Result<()> {
        let own: Vec<Constraint> = self
            .catalog
            .constraints_of(partition)
            .filter(|c| c.kind == ConKind::Foreign)
            .cloned()
            .collect();
        let oids: Vec<u32> = own.iter().map(|c| c.oid).collect();
        for con in own.iter().filter(|c| c.parent != 0 && !oids.contains(&c.parent)) {
            self.catalog.clear_constraint_parent(con.oid)?;
            let (insert, update) = self.fk_triggers(con.oid, partition, true);
            self.catalog.clear_trigger_parent(insert);
            self.catalog.clear_trigger_parent(update);
            self.fk_recurse_referenced(con.oid, None)?;
        }
        // `GetParentedForeignKeyRefs`: the partition is no longer a part of the key space of these constraints.
        for clone in self.parented_refs(partition) {
            self.catalog.clear_constraint_parent(clone)?;
            self.drop_fk_clone(clone)?;
        }
        Ok(())
    }

    /// Drops the clone `clone` of a foreign key with its triggers, as `DetachPartitionFinalize` and `tryAttachPartitionForeignKey` drop it after the clone and its triggers lose their parents.
    fn drop_fk_clone(&mut self, clone: u32) -> Result<()> {
        let triggers: Vec<u32> =
            self.catalog.triggers().filter(|t| t.constraint == clone).map(|t| t.oid).collect();
        for trigger in triggers {
            self.catalog.clear_trigger_parent(trigger);
        }
        let object = ObjRef::new(PG_CONSTRAINT, clone);
        self.catalog.drop_depends_of(object);
        self.perform(&[object], false)
    }

    /// `ATDetachCheckNoForeignKeyRefs`: the caller checks that no row of a referencing table has a key of `partition` before `DETACH PARTITION` takes the partition out of the key space of the foreign keys that refer to its parent.
    pub(super) fn detach_checks(&mut self, partition: u32) {
        if !self.parented_refs(partition).is_empty() {
            self.detached.push(partition);
        }
    }

    /// The clones of the foreign keys that refer to the parent of `partition` for the partition itself.
    fn parented_refs(&self, partition: u32) -> Vec<u32> {
        self.catalog
            .constraints()
            .filter(|c| c.parent != 0 && c.foreign.as_ref().is_some_and(|f| f.table == partition))
            .map(|c| c.oid)
            .collect()
    }

    /// The foreign key `oid`.
    fn foreign_constraint(&self, oid: u32) -> Result<Constraint> {
        self.catalog
            .constraint(oid)
            .filter(|c| c.kind == ConKind::Foreign)
            .cloned()
            .ok_or_else(|| Error::internal(format!("cache lookup failed for constraint {oid}")))
    }

    /// `GetForeignKeyCheckTriggers` with `check`, else `GetForeignKeyActionTriggers`: the two triggers of the foreign key `constraint` on the table `table`, for an insert or a delete and for an update.
    fn fk_triggers(&self, constraint: u32, table: u32, check: bool) -> (u32, u32) {
        let (mut first, mut second) = (0, 0);
        for trigger in self.catalog.triggers_of(table) {
            if trigger.constraint != constraint {
                continue;
            }
            let Some(function) = rupg_catalog::foreign_key_function(trigger.function) else {
                continue;
            };
            if (function < 2) != check {
                continue;
            }
            if function % 2 == 1 {
                second = trigger.oid;
            } else {
                first = trigger.oid;
            }
        }
        (first, second)
    }

    /// True for a partitioned table.
    fn is_partitioned(&self, table: u32) -> bool {
        self.catalog.relation(table).is_some_and(|r| r.kind == RelKind::Partitioned)
    }

    /// The partitions of a partitioned table.
    fn partition_oids(&self, table: u32) -> Vec<u32> {
        self.catalog.partitions(table).iter().map(|r| r.oid).collect()
    }

    /// `index_get_partition`: the index of the partition that is a partition of the index `index`.
    fn partition_index(&self, partition: u32, index: u32) -> Result<u32> {
        self.catalog
            .relations()
            .find(|r| r.index.as_ref().is_some_and(|i| i.table == partition && i.parent == index))
            .map(|r| r.oid)
            .ok_or_else(|| {
                let name = self.catalog.relation(partition).map_or("", |r| r.name.as_str());
                Error::internal(format!("index for {index} not found in partition {name}"))
            })
    }

    /// The numbers in `to` of the columns `keys` of `from`, matched by name, as `build_attrmap_by_name` maps them.
    fn map_keys(&self, from: u32, to: u32, keys: &[i16]) -> Result<Vec<i16>> {
        let (Some(from_rel), Some(to_rel)) =
            (self.catalog.relation(from), self.catalog.relation(to))
        else {
            return Err(Error::internal(format!("cache lookup failed for relation {to}")));
        };
        keys.iter()
            .map(|&key| {
                from_rel.column(key).and_then(|c| to_rel.column_number(&c.name)).ok_or_else(|| {
                    Error::internal(format!("could not map column {key} of relation {from}"))
                })
            })
            .collect()
    }
}

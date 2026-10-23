//! The removal of objects: the walk of `findDependentObjects` and the work of `deleteOneObject` and `doDeletion` in `dependency.c`. The analyzer finds the objects that a statement names, writes the messages of `reportDependentObjects` with the names of the objects, and then calls [`Catalog::remove`].

use std::cmp::Ordering;
use std::sync::Arc;

use rupg_common::{Error, Result};

use super::{
    Catalog, Column, ConKind, DepKind, ObjRef, PG_AMOP, PG_AMPROC, PG_ATTRDEF, PG_CAST, PG_CLASS,
    PG_CONSTRAINT, PG_NAMESPACE, PG_OPCLASS, PG_OPERATOR, PG_OPFAMILY, PG_PROC, PG_REWRITE,
    PG_TRIGGER, PG_TS_CONFIG, PG_TS_DICT, PG_TS_PARSER, PG_TS_TEMPLATE, PG_TYPE, is_pinned,
};

/// The reasons that the walk reached an object, the bits `DEPFLAG_*` of `dependency.c`.
pub mod flag {
    /// `DEPFLAG_ORIGINAL`: the statement names the object.
    pub const ORIGINAL: u32 = 0x0001;
    /// `DEPFLAG_NORMAL`: a normal dependency reached the object.
    pub const NORMAL: u32 = 0x0002;
    /// `DEPFLAG_AUTO`: an automatic dependency reached the object.
    pub const AUTO: u32 = 0x0004;
    /// `DEPFLAG_INTERNAL`: an internal dependency reached the object.
    pub const INTERNAL: u32 = 0x0008;
    /// `DEPFLAG_EXTENSION`: the object is a member of an extension that the walk removes.
    pub const EXTENSION: u32 = 0x0010;
    /// `DEPFLAG_PARTITION`: a partition dependency reached the object.
    pub const PARTITION: u32 = 0x0020;
    /// `DEPFLAG_REVERSE`: the walk came to the object from a part of it.
    pub const REVERSE: u32 = 0x0040;
    /// `DEPFLAG_IS_PART`: the object has a partition dependency, so the walk must also remove the object that it is a partition of.
    pub const IS_PART: u32 = 0x0080;
    /// `DEPFLAG_SUBOBJECT`: the object is a column of a relation that the walk also removes.
    pub const SUBOBJECT: u32 = 0x0100;
}

/// An object that a removal takes away, `ObjectAddressExtra` with its address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The object.
    pub object: ObjRef,
    /// The bits of [`flag`].
    pub flags: u32,
    /// The object that the walk came from, or `None` for an object that the statement names. For an object with [`flag::IS_PART`], the object that it is a partition of.
    pub dependee: Option<ObjRef>,
}

/// The reason that a removal cannot start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The database system needs the object.
    Pinned(ObjRef),
    /// The object is a part of `owner`, and the statement does not remove `owner`.
    Owned {
        /// The object.
        object: ObjRef,
        /// The object that it is a part of, or the object that it is a partition of.
        owner: ObjRef,
    },
}

/// One level of the walk, `ObjectAddressStack`.
struct Frame {
    object: ObjRef,
    flags: u32,
}

/// `object_address_comparator`: the highest OID first, then the catalog, then the column number as an unsigned number.
fn compare(a: ObjRef, b: ObjRef) -> Ordering {
    b.oid
        .cmp(&a.oid)
        .then(a.class.cmp(&b.class))
        .then(a.sub.cast_unsigned().cmp(&b.sub.cast_unsigned()))
}

/// True when `found` is the object or, for a whole object, one of its columns.
fn covers(object: ObjRef, found: ObjRef) -> bool {
    found.class == object.class
        && found.oid == object.oid
        && (object.sub == 0 || found.sub == object.sub)
}

/// True when a row of `pg_depend` joins a whole object to one of its own columns. The walk skips these rows.
fn is_self(object: ObjRef, other: ObjRef) -> bool {
    object.sub == 0 && other.class == object.class && other.oid == object.oid
}

/// `object_address_present_add_flags`, for the list of targets or for the stack of the walk. The result is true when the list has the object or its whole relation. When the list has a column of the object, the column gets the flags and `SUBOBJECT`.
fn present_add<'a>(
    object: ObjRef,
    flags: u32,
    entries: impl DoubleEndedIterator<Item = (ObjRef, &'a mut u32)>,
) -> bool {
    let mut result = false;
    for (found, found_flags) in entries.rev() {
        if found.class != object.class || found.oid != object.oid {
            continue;
        }
        if found.sub == object.sub {
            *found_flags |= flags;
            result = true;
        } else if found.sub == 0 {
            result = true;
        } else if object.sub == 0 && flags != 0 {
            *found_flags |= flags | flag::SUBOBJECT;
        }
    }
    result
}

/// `object_address_present`: true when `objects` has the object or its whole relation.
fn present(object: ObjRef, objects: &[ObjRef]) -> bool {
    objects.iter().any(|o| {
        o.class == object.class && o.oid == object.oid && (o.sub == object.sub || o.sub == 0)
    })
}

impl Catalog {
    /// `performMultipleDeletions` up to the report: the objects that a removal of `objects` takes away, in the order of the removal. An object comes after the objects that depend on it.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a pinned object, and for a part of an object that `objects` does not have.
    pub fn deletions(&self, objects: &[ObjRef]) -> std::result::Result<Vec<Target>, Refusal> {
        let mut targets = Vec::new();
        for &object in objects {
            self.find_dependents(object, flag::ORIGINAL, &mut Vec::new(), &mut targets, objects)?;
        }
        Ok(targets)
    }

    /// `findDependentObjects`: adds the object to `targets` after the objects that depend on it. A partition dependency of the object on a primary object, else on a secondary object, gives the object [`flag::IS_PART`] and the object that it is a partition of.
    fn find_dependents(
        &self,
        object: ObjRef,
        mut flags: u32,
        stack: &mut Vec<Frame>,
        targets: &mut Vec<Target>,
        pending: &[ObjRef],
    ) -> std::result::Result<(), Refusal> {
        if present_add(object, flags, stack.iter_mut().map(|f| (f.object, &mut f.flags))) {
            return Ok(());
        }
        if present_add(object, flags, targets.iter_mut().map(|t| (t.object, &mut t.flags))) {
            return Ok(());
        }
        if is_pinned(object) {
            return Err(Refusal::Pinned(object));
        }
        let mut owners = Vec::new();
        let mut partition = None;
        for d in self.depends.iter().filter(|d| covers(object, d.object)) {
            if is_self(object, d.referenced) {
                continue;
            }
            match d.kind {
                DepKind::Internal | DepKind::Extension => owners.push(d.referenced),
                DepKind::PartitionPri => {
                    flags |= flag::IS_PART;
                    partition = Some(d.referenced);
                }
                DepKind::PartitionSec => {
                    if flags & flag::IS_PART == 0 {
                        partition = Some(d.referenced);
                    }
                    flags |= flag::IS_PART;
                }
                DepKind::Normal | DepKind::Auto => {}
            }
        }
        let mut owner = None;
        for other in owners {
            if stack.is_empty() {
                if present(other, pending) {
                    return Ok(());
                }
                owner.get_or_insert(other);
                continue;
            }
            if present_add(other, 0, stack.iter_mut().map(|f| (f.object, &mut f.flags))) {
                continue;
            }
            self.find_dependents(other, flag::REVERSE, stack, targets, pending)?;
            present_add(object, flags, targets.iter_mut().map(|t| (t.object, &mut t.flags)));
            return Ok(());
        }
        if let Some(owner) = owner {
            return Err(Refusal::Owned { object, owner: partition.unwrap_or(owner) });
        }
        let mut dependents: Vec<(ObjRef, u32)> = self
            .depends
            .iter()
            .filter(|d| covers(object, d.referenced) && !is_self(object, d.object))
            .map(|d| {
                let flags = match d.kind {
                    DepKind::Normal => flag::NORMAL,
                    DepKind::Auto => flag::AUTO,
                    DepKind::Internal => flag::INTERNAL,
                    DepKind::Extension => flag::EXTENSION,
                    DepKind::PartitionPri | DepKind::PartitionSec => flag::PARTITION,
                };
                (d.object, flags)
            })
            .collect();
        dependents.sort_by(|a, b| compare(a.0, b.0));
        stack.push(Frame { object, flags });
        for (dependent, flags) in dependents {
            self.find_dependents(dependent, flags, stack, targets, pending)?;
        }
        let frame = stack.pop().unwrap_or(Frame { object, flags });
        let dependee = if frame.flags & flag::IS_PART != 0 {
            partition
        } else {
            stack.last().map(|f| f.object)
        };
        targets.push(Target { object, flags: frame.flags, dependee });
        Ok(())
    }

    /// `deleteObjectsInList`: removes the targets in order, each with its rows of `pg_depend` and its comments, as `deleteOneObject` does. After an error the caller discards the catalog.
    ///
    /// # Errors
    ///
    /// An internal error for an object of a class that the catalog cannot remove.
    pub fn remove(&mut self, targets: &[Target]) -> Result<()> {
        for target in targets {
            let object = target.object;
            self.remove_one(object)?;
            self.depends.retain(|d| !covers(object, d.object));
            if self.descriptions.keys().any(|found| covers(object, *found)) {
                Arc::make_mut(&mut self.descriptions).retain(|found, _| !covers(object, *found));
            }
            if self.acls.keys().any(|found| covers(object, *found)) {
                Arc::make_mut(&mut self.acls).retain(|found, _| !covers(object, *found));
            }
        }
        Ok(())
    }

    /// `doDeletion` for one object. A rule has no row of its own: it goes with its view.
    fn remove_one(&mut self, object: ObjRef) -> Result<()> {
        match object.class {
            // `RemoveAttributeById`: the row of `pg_attribute` stays with `attisdropped`, so the later columns keep their numbers.
            PG_CLASS if object.sub != 0 => {
                if let Ok(number) = i16::try_from(object.sub)
                    && let Ok(column) = self.column_mut(object.oid, number)
                {
                    *column = Column {
                        name: format!("........pg.dropped.{}........", object.sub),
                        ty: 0,
                        typmod: -1,
                        ndims: 0,
                        collation: 0,
                        not_null: false,
                        has_default: false,
                        identity: 0,
                        dropped: true,
                        stat_target: None,
                        storage: column.storage,
                        compression: column.compression,
                        options: Vec::new(),
                        local: column.local,
                        inhcount: column.inhcount,
                    };
                }
            }
            PG_CLASS => {
                self.relations.remove(&object.oid);
            }
            PG_TYPE => {
                self.types.remove(&object.oid);
            }
            PG_ATTRDEF => {
                if let Some(default) = self.defaults.remove(&object.oid)
                    && let Ok(column) = self.column_mut(default.relation, default.column)
                {
                    column.has_default = false;
                }
            }
            PG_CONSTRAINT => {
                // `dropconstraint_internal` clears `attnotnull` with the not-null constraint of the column.
                if let Some(con) = self.constraints.remove(&object.oid)
                    && con.kind == ConKind::NotNull
                    && let [number] = con.keys[..]
                    && let Ok(column) = self.column_mut(con.relation, number)
                {
                    column.not_null = false;
                }
            }
            PG_NAMESPACE => {
                self.schemas.remove(&object.oid);
            }
            PG_PROC => {
                self.functions.remove(&object.oid);
            }
            PG_TRIGGER => {
                self.triggers.remove(&object.oid);
            }
            PG_CAST => {
                self.casts.remove(&object.oid);
            }
            PG_OPERATOR => self.remove_operator(object.oid),
            PG_OPFAMILY => {
                self.opfamilies.remove(&object.oid);
            }
            PG_OPCLASS => {
                self.opclasses.remove(&object.oid);
            }
            PG_AMOP => {
                self.amops.remove(&object.oid);
            }
            PG_AMPROC => {
                self.amprocs.remove(&object.oid);
            }
            PG_TS_TEMPLATE => {
                self.ts_templates.remove(&object.oid);
            }
            PG_TS_PARSER => {
                self.ts_parsers.remove(&object.oid);
            }
            PG_TS_DICT => {
                self.ts_dictionaries.remove(&object.oid);
            }
            PG_TS_CONFIG => {
                self.ts_configs.remove(&object.oid);
                self.ts_mappings.retain(|m| m.config != object.oid);
            }
            PG_REWRITE => {
                self.rules.remove(&object.oid);
            }
            class => {
                return Err(Error::internal(format!("unsupported object class: {class}")));
            }
        }
        Ok(())
    }

    /// The view of a `_RETURN` rule.
    pub fn view_of_rule(&self, rule: u32) -> Option<&super::Relation> {
        self.relations.values().find(|r| r.view.as_ref().is_some_and(|v| v.rule == rule))
    }
}

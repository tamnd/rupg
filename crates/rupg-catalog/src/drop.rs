//! The removal of objects: the walk of `findDependentObjects` and the work of `deleteOneObject` and `doDeletion` in `dependency.c`. The analyzer finds the objects that a statement names, writes the messages of `reportDependentObjects` with the names of the objects, and then calls [`Catalog::remove`].

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};

use super::{
    Catalog, DepKind, ObjRef, PG_ATTRDEF, PG_CLASS, PG_CONSTRAINT, PG_NAMESPACE, PG_PROC,
    PG_REWRITE, PG_TRIGGER, PG_TYPE, is_pinned,
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
    /// `DEPFLAG_REVERSE`: the walk came to the object from a part of it.
    pub const REVERSE: u32 = 0x0040;
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
    /// The object that the walk came from, or `None` for an object that the statement names.
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
        /// The object that it is a part of.
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

    /// `findDependentObjects`: adds the object to `targets` after the objects that depend on it.
    fn find_dependents(
        &self,
        object: ObjRef,
        flags: u32,
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
        let owners: Vec<ObjRef> = self
            .depends
            .iter()
            .filter(|d| {
                covers(object, d.object) && matches!(d.kind, DepKind::Internal | DepKind::Extension)
            })
            .map(|d| d.referenced)
            .filter(|r| !is_self(object, *r))
            .collect();
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
            return Err(Refusal::Owned { object, owner });
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
        let dependee = stack.last().map(|f| f.object);
        targets.push(Target { object, flags: frame.flags, dependee });
        Ok(())
    }

    /// `deleteObjectsInList`: removes the targets in order, each with its rows of `pg_depend` and its comments, as `deleteOneObject` does. After an error the caller discards the catalog.
    ///
    /// # Errors
    ///
    /// `0A000` for a column, because rupg cannot remove a column yet.
    pub fn remove(&mut self, targets: &[Target]) -> Result<()> {
        for target in targets {
            let object = target.object;
            self.remove_one(object)?;
            self.depends.retain(|d| !covers(object, d.object));
            self.descriptions.retain(|found, _| !covers(object, *found));
        }
        Ok(())
    }

    /// `doDeletion` for one object. A rule has no row of its own: it goes with its view.
    fn remove_one(&mut self, object: ObjRef) -> Result<()> {
        match object.class {
            PG_CLASS if object.sub != 0 => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "the removal of a column is not supported yet",
                ));
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
                self.constraints.remove(&object.oid);
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
            PG_REWRITE => {}
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

//! The event triggers of `pg_event_trigger` that `CREATE EVENT TRIGGER` makes. They change as `CreateEventTrigger`, `AlterEventTrigger`, `AlterEventTriggerOwner_internal` and `AlterObjectRename_internal` of `event_trigger.c` and `alter.c` change them, with the same rows of `pg_depend`.

use super::{Catalog, DepKind, ObjRef, PG_PROC};

/// The OID of `pg_event_trigger`.
pub const PG_EVENT_TRIGGER: u32 = 3466;

/// An event trigger, one row of `pg_event_trigger`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTrigger {
    /// The OID. [`Catalog::create_event_trigger`] sets it.
    pub oid: u32,
    /// `evtname`: the name.
    pub name: String,
    /// `evtevent`: the event, such as `ddl_command_start`.
    pub event: String,
    /// `evtowner`: the OID of the owner role.
    pub owner: u32,
    /// `evtfoid`: the OID of the function.
    pub func: u32,
    /// `evtenabled`: `O` for the origin and local modes, `R` for the replica mode, `A` for all modes, and `D` when it is disabled.
    pub enabled: u8,
    /// `evttags`: the command tags of the filter, in upper case. `None` when the trigger fires for all the tags.
    pub tags: Option<Vec<String>>,
}

impl Catalog {
    /// `insert_event_trigger_tuple`: a new event trigger, which gets the next OID. It depends on its function. The analyzer checks the name, the event and the tags first. The return value is the OID of the event trigger.
    pub fn create_event_trigger(&mut self, mut new: EventTrigger) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_EVENT_TRIGGER, oid);
        self.depend(me, ObjRef::new(PG_PROC, new.func), DepKind::Normal);
        self.event_triggers.insert(oid, new);
        oid
    }

    /// The event triggers, in the order of their OIDs.
    pub fn event_triggers(&self) -> impl Iterator<Item = &EventTrigger> {
        self.event_triggers.values()
    }

    /// The event trigger with the OID.
    pub fn event_trigger(&self, oid: u32) -> Option<&EventTrigger> {
        self.event_triggers.get(&oid)
    }

    /// `get_event_trigger_oid`: the event trigger with the name.
    pub fn event_trigger_by_name(&self, name: &str) -> Option<&EventTrigger> {
        self.event_triggers.values().find(|t| t.name == name)
    }

    /// Changes the row of the event trigger.
    pub fn alter_event_trigger(&mut self, oid: u32, change: impl FnOnce(&mut EventTrigger)) {
        if let Some(trigger) = self.event_triggers.get_mut(&oid) {
            change(trigger);
        }
    }
}

rupg_common::persist_struct!(EventTrigger { oid, name, event, owner, func, enabled, tags });

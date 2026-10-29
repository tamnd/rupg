//! `CREATE EVENT TRIGGER` and `ALTER EVENT TRIGGER`, as `CreateEventTrigger` and `AlterEventTrigger` of `event_trigger.c` run them, and `ALTER EVENT TRIGGER` with `RENAME TO` and `OWNER TO`, as `AlterObjectRename_internal` and `AlterEventTriggerOwner_internal` run them.

use rupg_catalog::{EventTrigger, ObjRef, PG_EVENT_TRIGGER};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterEventTrigStmt, AlterOwnerStmt, CreateEventTrigStmt, Node, RenameStmt};
use rupg_types::oid;
use rupg_wire::CommandTag;

use super::Definer;
use super::comment::not_owner;
use crate::typename::names;

/// The events that an event trigger can fire on.
const EVENTS: [&str; 5] =
    ["ddl_command_start", "ddl_command_end", "sql_drop", "login", "table_rewrite"];

/// `TRIGGER_FIRES_ON_ORIGIN`: the mode of a new event trigger.
const FIRES_ON_ORIGIN: u8 = b'O';

/// The error of a missing event trigger.
fn missing(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("event trigger \"{name}\" does not exist"))
}

/// The error of `CreateEventTrigger` and of a rename for a name that an event trigger has.
fn taken(name: &str) -> Error {
    Error::new(SqlState::DUPLICATE_OBJECT, format!("event trigger \"{name}\" already exists"))
}

/// The name of an event trigger in the object of a statement.
fn object_name(object: Option<&Node>) -> &str {
    match object {
        Some(Node::String(name)) => name,
        _ => "",
    }
}

/// `validate_ddl_tags` and `validate_table_rewrite_tags`: each tag must be a known command tag that the event can fire for.
fn validate_tags(event: &str, tags: &[&str]) -> Result<()> {
    for &text in tags {
        let tag = CommandTag::from_name(text);
        let ok = if event == "table_rewrite" {
            tag.table_rewrite_ok()
        } else {
            if tag == CommandTag::Unknown {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("filter value \"{text}\" not recognized for filter variable \"tag\""),
                ));
            }
            tag.event_trigger_ok()
        };
        if !ok {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("event triggers are not supported for {text}"),
            ));
        }
    }
    Ok(())
}

impl Definer<'_, '_> {
    /// `CreateEventTrigger`: only a superuser can make an event trigger. The checks of the event, of the filter, of the name and of the function come in the order of PostgreSQL.
    pub(super) fn create_event_trigger(&mut self, stmt: &CreateEventTrigStmt) -> Result<()> {
        let name = stmt.trigname.as_deref().unwrap_or_default();
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create event trigger \"{name}\""),
            )
            .with_hint("Must be superuser to create an event trigger.".to_string()));
        }
        let event = stmt.eventname.as_deref().unwrap_or_default();
        if !EVENTS.contains(&event) {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("unrecognized event name \"{event}\""),
            ));
        }
        let mut tags: Option<Vec<&str>> = None;
        for item in stmt.whenclause.iter().flatten() {
            let Node::DefElem(def) = item else { continue };
            let variable = def.defname.as_deref().unwrap_or_default();
            if variable != "tag" {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unrecognized filter variable \"{variable}\""),
                ));
            }
            if tags.is_some() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("filter variable \"{variable}\" specified more than once"),
                ));
            }
            tags = Some(match &def.arg {
                Some(Node::List(list)) => names(list),
                _ => Vec::new(),
            });
        }
        if let Some(tags) = &tags {
            if event == "login" {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "tag filtering is not supported for login event triggers",
                ));
            }
            validate_tags(event, tags)?;
        }
        if self.catalog.event_trigger_by_name(name).is_some() {
            return Err(taken(name));
        }
        let func_name = names(&stmt.funcname);
        let (func, rettype) = self.func_oid(&func_name, &[])?;
        if rettype != oid::EVENT_TRIGGER {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("function {} must return type event_trigger", func_name.join(".")),
            ));
        }
        let new = EventTrigger {
            oid: 0,
            name: name.to_string(),
            event: event.to_string(),
            owner: self.user,
            func,
            enabled: FIRES_ON_ORIGIN,
            tags: tags.map(|tags| tags.iter().map(|t| t.to_ascii_uppercase()).collect()),
        };
        self.catalog.create_event_trigger(new);
        Ok(())
    }

    /// The event trigger of a name, which must exist, with the check of its owner.
    fn owned_event_trigger(&self, name: &str) -> Result<EventTrigger> {
        let found =
            self.catalog.event_trigger_by_name(name).cloned().ok_or_else(|| missing(name))?;
        if !self.is_superuser() && !self.owns(found.owner) {
            return Err(not_owner("event trigger", name));
        }
        Ok(found)
    }

    /// `AlterEventTrigger`: the owner can enable and disable the event trigger.
    pub(super) fn alter_event_trigger(&mut self, stmt: &AlterEventTrigStmt) -> Result<()> {
        let name = stmt.trigname.as_deref().unwrap_or_default();
        let found = self.owned_event_trigger(name)?;
        self.catalog.alter_event_trigger(found.oid, |t| t.enabled = stmt.tgenabled);
        Ok(())
    }

    /// `AlterObjectRename_internal` for an event trigger.
    pub(super) fn rename_event_trigger(&mut self, stmt: &RenameStmt) -> Result<()> {
        let name = object_name(stmt.object.as_ref());
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let found = self.owned_event_trigger(name)?;
        if self.catalog.event_trigger_by_name(newname).is_some() {
            return Err(taken(newname));
        }
        let newname = newname.to_string();
        self.catalog.alter_event_trigger(found.oid, |t| t.name = newname);
        Ok(())
    }

    /// `AlterEventTriggerOwner_internal`: the new owner must be a superuser.
    pub(super) fn alter_event_trigger_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let name = object_name(stmt.object.as_ref());
        let found =
            self.catalog.event_trigger_by_name(name).cloned().ok_or_else(|| missing(name))?;
        let new = self.role_oid(&role)?;
        if found.owner == new {
            return Ok(());
        }
        if !self.is_superuser() && !self.owns(found.owner) {
            return Err(not_owner("event trigger", name));
        }
        let roles = rupg_pgcatalog::roles::current();
        if !roles.superuser(new) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to change owner of event trigger \"{name}\""),
            )
            .with_hint("The owner of an event trigger must be a superuser.".to_string()));
        }
        let (old, new_name) = (
            roles.name(found.owner).unwrap_or_default().to_string(),
            roles.name(new).unwrap_or_default().to_string(),
        );
        self.catalog.set_object_owner(
            ObjRef::new(PG_EVENT_TRIGGER, found.oid),
            new,
            &old,
            &new_name,
        )
    }

    /// The object of `COMMENT ON EVENT TRIGGER`, after the check of its owner.
    pub(super) fn comment_event_trigger(&self, object: Option<&Node>) -> Result<ObjRef> {
        let found = self.owned_event_trigger(object_name(object))?;
        Ok(ObjRef::new(PG_EVENT_TRIGGER, found.oid))
    }

    /// `getObjectDescription` for an event trigger.
    pub(super) fn describe_event_trigger(&self, oid: u32) -> String {
        match self.catalog.event_trigger(oid) {
            Some(trigger) => format!("event trigger {}", trigger.name),
            None => format!("event trigger {oid}"),
        }
    }
}

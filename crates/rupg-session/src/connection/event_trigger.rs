//! The firing of the event triggers `ddl_command_start`, `ddl_command_end` and `sql_drop`, as `EventTriggerDDLCommandStart`, `EventTriggerDDLCommandEnd` and `EventTriggerSQLDrop` of `event_trigger.c` run them.

use rupg_analyze::Dropped;
use rupg_common::{Error, SqlState};
use rupg_types::Value;
use rupg_wire::CommandTag;

use super::Connection;
use crate::utility::Notice;

/// `TRIGGER_FIRES_ON_ORIGIN`, `TRIGGER_FIRES_ALWAYS`, `TRIGGER_FIRES_ON_REPLICA` and `TRIGGER_DISABLED`.
const ON_ORIGIN: u8 = b'O';
const ALWAYS: u8 = b'A';
const ON_REPLICA: u8 = b'R';

/// `filter_event_trigger`: a trigger in the mode `enabled` fires for the role `replica` of `session_replication_role`. A disabled trigger does not fire.
fn fires(enabled: u8, replica: bool) -> bool {
    match enabled {
        ALWAYS => true,
        ON_ORIGIN => !replica,
        ON_REPLICA => replica,
        _ => false,
    }
}

/// The state of the event triggers of a command, as `EventTriggerQueryState` keeps it.
#[derive(Debug, Default)]
pub(super) struct Frame {
    /// True when an event trigger of `sql_drop` can fire for the command, so that the command keeps the objects that it drops.
    track: bool,
    /// The rows of `pg_event_trigger_dropped_objects`, as `SQLDropList` keeps them.
    dropped: Vec<Vec<Value>>,
    /// True while the event triggers of `sql_drop` run, as `in_sql_drop` gives.
    in_sql_drop: bool,
}

impl Frame {
    /// The state of a command that keeps the objects that it drops when `track` is true.
    pub(super) fn new(track: bool) -> Self {
        Self { track, ..Self::default() }
    }
}

impl Connection {
    /// `EventTriggerDDLCommandStart` and `EventTriggerDDLCommandEnd`: the functions of the event triggers of the event `event` that the tag `tag` and `session_replication_role` let fire, in the order of their names. A tag that event triggers do not support fires nothing, and `event_triggers = off` turns the firing off. The messages of `RAISE` go to `notices`.
    ///
    /// # Errors
    ///
    /// The errors of the functions, which stop the command.
    pub(super) fn fire_event_triggers(
        &self,
        event: &str,
        tag: CommandTag,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        for func in self.armed_event_triggers(event, tag) {
            let said = self.trigger_warnings.borrow().len();
            let result = self.pl_event_trigger(func, event, tag.name());
            let warnings = self.trigger_warnings.borrow_mut().split_off(said);
            notices.extend(warnings.into_iter().map(Notice::of_function));
            result?;
        }
        Ok(())
    }

    /// `EventTriggerCommonSetup`: the functions of the event triggers of the event `event` that fire for the tag `tag`, in the order of the names of the triggers.
    pub(super) fn armed_event_triggers(&self, event: &str, tag: CommandTag) -> Vec<u32> {
        if !tag.event_trigger_ok() {
            return Vec::new();
        }
        let (replica, off) = {
            let settings = self.settings.borrow();
            (
                settings.get("session_replication_role").as_deref() == Some("replica"),
                settings.get("event_triggers").as_deref() == Some("off"),
            )
        };
        if off {
            return Vec::new();
        }
        let catalog = self.catalog();
        let name = tag.name();
        let mut armed: Vec<(&str, u32)> = catalog
            .event_triggers()
            .filter(|t| t.event == event && fires(t.enabled, replica))
            .filter(|t| t.tags.as_ref().is_none_or(|tags| tags.iter().any(|t| t == name)))
            .map(|t| (t.name.as_str(), t.func))
            .collect();
        armed.sort_unstable();
        armed.into_iter().map(|(_, func)| func).collect()
    }

    /// `EventTriggerSQLDropAddObject`: the rows of `pg_event_trigger_dropped_objects` for the objects `dropped` of the command that runs, from the catalog before the drop. `slist_push_head` keeps the last object first.
    pub(super) fn keep_dropped(&self, dropped: &[Dropped]) {
        if dropped.is_empty() || !self.event_frames.borrow().last().is_some_and(|f| f.track) {
            return;
        }
        let rows: Vec<Vec<Value>> = {
            let reader = self.reader();
            dropped
                .iter()
                .rev()
                .filter_map(|d| rupg_func::dropped_object(d.object, d.original, d.normal, &reader))
                .collect()
        };
        if let Some(frame) = self.event_frames.borrow_mut().last_mut() {
            frame.dropped.extend(rows);
        }
    }

    /// `EventTriggerSQLDrop`: the event triggers of `sql_drop` for the objects that the command of `frame` dropped. `pg_event_trigger_dropped_objects` gives the objects while they run.
    pub(super) fn fire_sql_drop(
        &self,
        mut frame: Frame,
        tag: CommandTag,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        if frame.dropped.is_empty() {
            return Ok(());
        }
        frame.in_sql_drop = true;
        self.event_frames.borrow_mut().push(frame);
        let result = self.fire_event_triggers("sql_drop", tag, notices);
        self.event_frames.borrow_mut().pop();
        result
    }

    /// The rows of `pg_event_trigger_dropped_objects`, or `None` outside of an event trigger of `sql_drop`.
    pub(crate) fn dropped_objects(&self) -> Option<Vec<Vec<Value>>> {
        let frames = self.event_frames.borrow();
        frames.last().filter(|f| f.in_sql_drop).map(|f| f.dropped.clone())
    }

    /// The call of the event trigger function `func`, which must be in PL/pgSQL.
    fn pl_event_trigger(&self, func: u32, event: &str, tag: &str) -> Result<(), Error> {
        let catalog = self.catalog();
        if catalog.function(func).is_none_or(|f| f.lang != rupg_plpgsql::PLPGSQL_LANGUAGE) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "an event trigger function in a language other than PL/pgSQL is not supported yet",
            ));
        }
        self.pl_event_call(func, event, tag)
    }
}

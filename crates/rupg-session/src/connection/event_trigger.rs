//! The firing of the event triggers `ddl_command_start` and `ddl_command_end`, as `EventTriggerDDLCommandStart` and `EventTriggerDDLCommandEnd` of `event_trigger.c` run them.

use rupg_common::{Error, SqlState};
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
        if !tag.event_trigger_ok() {
            return Ok(());
        }
        let (replica, off) = {
            let settings = self.settings.borrow();
            (
                settings.get("session_replication_role").as_deref() == Some("replica"),
                settings.get("event_triggers").as_deref() == Some("off"),
            )
        };
        if off {
            return Ok(());
        }
        let catalog = self.catalog();
        let name = tag.name();
        let mut armed: Vec<(&str, u32)> = catalog
            .event_triggers()
            .filter(|t| t.event == event && fires(t.enabled, replica))
            .filter(|t| t.tags.as_ref().is_none_or(|tags| tags.iter().any(|t| t == name)))
            .map(|t| (t.name.as_str(), t.func))
            .collect();
        if armed.is_empty() {
            return Ok(());
        }
        armed.sort_unstable();
        for (_, func) in armed {
            let said = self.trigger_warnings.borrow().len();
            let result = self.pl_event_trigger(func, event, name);
            let warnings = self.trigger_warnings.borrow_mut().split_off(said);
            notices.extend(warnings.into_iter().map(Notice::of_function));
            result?;
        }
        Ok(())
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

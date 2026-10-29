//! The firing of the event triggers `ddl_command_start`, `ddl_command_end`, `sql_drop` and `login`, as `EventTriggerDDLCommandStart`, `EventTriggerDDLCommandEnd`, `EventTriggerSQLDrop` and `EventTriggerOnLogin` of `event_trigger.c` run them.

use rupg_analyze::{Collected, Dropped, Rewrite};
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::Node;
use rupg_types::TimeZone;
use rupg_types::Value;
use rupg_wire::CommandTag;

use super::Connection;
use crate::utility::Notice;

/// `TRIGGER_FIRES_ON_ORIGIN`, `TRIGGER_FIRES_ALWAYS`, `TRIGGER_FIRES_ON_REPLICA` and `TRIGGER_DISABLED`.
const ON_ORIGIN: u8 = b'O';
const ALWAYS: u8 = b'A';
const ON_REPLICA: u8 = b'R';
const DISABLED: u8 = b'D';
/// `AT_REWRITE_COLUMN_REWRITE`.
const COLUMN_REWRITE: i32 = 0x04;

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
    /// The commands that the command did, as `currentCommand` and `commandList` keep them, with the tag of each command.
    commands: Vec<Collected>,
    /// The rows of `pg_event_trigger_ddl_commands` while the event triggers of `ddl_command_end` run, or `None`.
    ddl: Option<Vec<Vec<Value>>>,
    /// The table and the reasons while the event triggers of `table_rewrite` run, or `None`.
    rewrite: Option<(u32, i32)>,
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
        if event != "login" && !tag.event_trigger_ok() {
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

    /// `SetDatabaseHasLoginEventTriggers` after `CREATE EVENT TRIGGER` of `login` and after `ALTER EVENT TRIGGER` that enables an event trigger of `login`.
    pub(super) fn note_login_event(&self, node: &Node) {
        let login = match node {
            Node::CreateEventTrigStmt(stmt) => stmt.eventname.as_deref() == Some("login"),
            Node::AlterEventTrigStmt(stmt) => {
                let name = stmt.trigname.as_deref().unwrap_or_default();
                stmt.tgenabled != DISABLED
                    && self
                        .catalog()
                        .event_trigger_by_name(name)
                        .is_some_and(|t| t.event == "login")
            }
            _ => false,
        };
        if login {
            self.store.set_login_event(self.database_oid, true);
        }
    }

    /// `EventTriggerOnLogin`: when `dathasloginevt` is true, the event triggers of `login` fire in a transaction of their own. When no trigger fires, `dathasloginevt` becomes false.
    ///
    /// # Errors
    ///
    /// The errors of the functions, which end the session.
    pub(super) fn on_login(&mut self, notices: &mut Vec<Notice>) -> Result<(), Error> {
        if !self.store.login_event(self.database_oid)
            || self.settings.borrow().get("event_triggers").as_deref() == Some("off")
        {
            return Ok(());
        }
        self.start_statement();
        self.start_xact();
        self.start_triggers();
        let funcs = self.armed_event_triggers("login", CommandTag::Unknown);
        if funcs.is_empty() {
            self.store.set_login_event(self.database_oid, false);
        }
        let mut result = Ok(());
        for func in funcs {
            let said = self.trigger_warnings.borrow().len();
            result = self.pl_event_trigger(func, "login", "LOGIN");
            let warnings = self.trigger_warnings.borrow_mut().split_off(said);
            notices.extend(warnings.into_iter().map(Notice::of_function));
            if result.is_err() {
                break;
            }
        }
        match result {
            Ok(()) => self.finish_xact(),
            Err(_) => self.abort(),
        }
        result
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

    /// `EventTriggerCollectSimpleCommand` and `EventTriggerCollectGrant`: keeps the commands `commands` of the statement of the tag `tag` that runs. A command without a tag gets the tag of the statement.
    pub(super) fn keep_commands(&self, commands: &[Collected], tag: CommandTag) {
        let mut frames = self.event_frames.borrow_mut();
        let Some(frame) = frames.last_mut().filter(|f| !f.in_sql_drop && f.ddl.is_none()) else {
            return;
        };
        frame.commands.extend(commands.iter().map(|command| match *command {
            Collected::Object { tag: None, object } => {
                Collected::Object { tag: Some(tag.name()), object }
            }
            other => other,
        }));
    }

    /// `EventTriggerDDLCommandEnd`: the event triggers of `ddl_command_end` for the command of `frame`. `pg_event_trigger_ddl_commands` gives the commands of the frame while they run, with the identities that the catalog gives then.
    pub(super) fn fire_ddl_end(
        &self,
        frame: Frame,
        tag: CommandTag,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        if self.armed_event_triggers("ddl_command_end", tag).is_empty() {
            return Ok(());
        }
        let rows = {
            let reader = self.reader();
            (0_i64..)
                .zip(&frame.commands)
                .filter_map(|(at, command)| match *command {
                    Collected::Object { tag, object } => {
                        rupg_func::ddl_command(tag.unwrap_or_default(), object, at, &reader)
                    }
                    Collected::Grant { grant, kind } => Some(vec![
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text(if grant { "GRANT" } else { "REVOKE" }.into()),
                        Value::Text(kind.into()),
                        Value::Null,
                        Value::Null,
                        Value::Bool(false),
                        Value::Int8(at),
                    ]),
                })
                .collect()
        };
        self.event_frames.borrow_mut().push(Frame { ddl: Some(rows), ..Frame::default() });
        let result = self.fire_event_triggers("ddl_command_end", tag, notices);
        self.event_frames.borrow_mut().pop();
        result
    }

    /// The rows of `pg_event_trigger_ddl_commands`, or `None` outside of an event trigger of `ddl_command_end`.
    pub(crate) fn ddl_commands(&self) -> Option<Vec<Vec<Value>>> {
        self.event_frames.borrow().last()?.ddl.clone()
    }

    /// `EventTriggerSQLDrop`: the event triggers of `sql_drop` for the objects that the command of `frame` dropped. `pg_event_trigger_dropped_objects` gives the objects while they run.
    pub(super) fn fire_sql_drop(
        &self,
        frame: &mut Frame,
        tag: CommandTag,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        if frame.dropped.is_empty() {
            return Ok(());
        }
        let dropped = Frame {
            in_sql_drop: true,
            dropped: std::mem::take(&mut frame.dropped),
            ..Frame::default()
        };
        self.event_frames.borrow_mut().push(dropped);
        let result = self.fire_event_triggers("sql_drop", tag, notices);
        self.event_frames.borrow_mut().pop();
        result
    }

    /// `EventTriggerTableRewrite`: the event triggers of `table_rewrite` for each table that the command `node` writes again, before the rows go to the new files. A change between `timestamp` and `timestamptz` writes the rows again only when the zone of the session has an offset that is not 0, as `TimestampTimestampTzRequiresRewrite` checks it.
    pub(super) fn fire_table_rewrite(
        &self,
        node: &Node,
        rewrites: &[Rewrite],
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        if rewrites.is_empty() || !super::event_trigger_object(node) {
            return Ok(());
        }
        let tag = super::definition_tag(node);
        if self.armed_event_triggers("table_rewrite", tag).is_empty() {
            return Ok(());
        }
        let zoned = || {
            let name = self.settings.borrow().get("TimeZone").unwrap_or_else(|| "UTC".to_owned());
            rupg_types::tz::load(&name).is_none_or(|zone| zone.fixed_offset() != Some(0))
        };
        for rewrite in rewrites {
            let mut reason = rewrite.reason;
            if rewrite.zoned && zoned() {
                reason |= COLUMN_REWRITE;
            }
            if reason == 0 {
                continue;
            }
            let frame = Frame { rewrite: Some((rewrite.table, reason)), ..Frame::default() };
            self.event_frames.borrow_mut().push(frame);
            let result = self.fire_event_triggers("table_rewrite", tag, notices);
            self.event_frames.borrow_mut().pop();
            result?;
        }
        Ok(())
    }

    /// The table and the reasons of the event trigger of `table_rewrite` that runs, or `None` outside of one.
    pub(crate) fn table_rewrite(&self) -> Option<(u32, i32)> {
        self.event_frames.borrow().last().and_then(|f| f.rewrite)
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

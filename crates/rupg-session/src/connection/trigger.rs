//! The triggers of `CREATE TRIGGER` on the writes of a table, as `trigger.c` fires them: the triggers `BEFORE` before the write, and the queue of the triggers `AFTER`, which fire at the end of the statement, as `AfterTriggerEndQuery` fires them.
//!
//! The triggers `BEFORE ... FOR EACH STATEMENT` fire after the query of the write gives its rows. The triggers `BEFORE ... FOR EACH ROW` fire for all the rows before the first row goes into the heap, so a trigger does not see the rows that its statement wrote before. Only a trigger function in PL/pgSQL can fire yet.

use std::rc::Rc;

use rupg_analyze::{Expr, Query, Write, WriteKind};
use rupg_catalog::{TRIGGER_BEFORE, TRIGGER_DELETE, TRIGGER_INSERT, TRIGGER_ROW, TRIGGER_UPDATE};
use rupg_catalog::{TRIGGER_INSTEAD, Trigger};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_pgcatalog::builtin::Named;
use rupg_types::Value;

use super::Connection;
use crate::query::{Captured, Transition};
use crate::utility::Notice;

/// A trigger `AFTER` in the queue of the statement, `AfterTriggerEvent`.
#[derive(Clone, Debug)]
pub(super) struct Event {
    trigger: Trigger,
    /// The bit of the event in `tgtype`.
    event: i16,
    /// The old row of a trigger `FOR EACH ROW` for an `UPDATE` or a `DELETE`.
    old: Option<Vec<Value>>,
    /// The new row of a trigger `FOR EACH ROW` for an `INSERT` or an `UPDATE`.
    new: Option<Vec<Value>>,
    /// The transition tables of a trigger with `REFERENCING`.
    transition: Option<Transition>,
}

/// A trigger that fires for a write, with its condition of `WHEN`.
pub(super) struct Armed {
    trigger: Trigger,
    /// The condition and the query that has `old` and `new` as its relations.
    when: Option<(Query, Expr)>,
}

/// The triggers of a write, in the order of their names.
#[derive(Default)]
pub(super) struct Triggers {
    list: Vec<Armed>,
    /// The bit of the event of the write in `tgtype`.
    event: i16,
}

impl Triggers {
    /// True when the write fires no trigger.
    pub(super) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// True when a trigger `AFTER ... FOR EACH ROW` or a trigger with transition tables gets the rows of the write.
    pub(super) fn after_rows(&self) -> bool {
        self.of(true, false).next().is_some()
            || self.list.iter().any(|t| has_transition(&t.trigger))
    }

    /// The triggers with the level `level` and the time `before`. A trigger `INSTEAD OF` is not one of them.
    fn of(&self, row: bool, before: bool) -> impl Iterator<Item = &Armed> {
        self.list.iter().filter(move |t| {
            (t.trigger.kind & TRIGGER_ROW != 0) == row
                && (t.trigger.kind & TRIGGER_BEFORE != 0) == before
                && t.trigger.kind & TRIGGER_INSTEAD == 0
        })
    }
}

/// The bits in `tgtype` of a trigger `INSTEAD OF ... FOR EACH ROW`.
const INSTEAD_ROW: i16 = TRIGGER_ROW | TRIGGER_INSTEAD;

/// True for a trigger with `REFERENCING OLD TABLE` or `REFERENCING NEW TABLE`.
fn has_transition(trigger: &Trigger) -> bool {
    trigger.old_table.is_some() || trigger.new_table.is_some()
}

/// The transition tables of the trigger `trigger` with the rows `rows`, or `None` for a trigger with no `REFERENCING`.
fn transition_of(trigger: &Trigger, rows: &Rc<Captured>) -> Option<Transition> {
    has_transition(trigger).then(|| Transition {
        rows: Rc::clone(rows),
        old: trigger.old_table.clone(),
        new: trigger.new_table.clone(),
    })
}

/// The words of `TG_OP` for the bit of an event.
fn op_name(event: i16) -> &'static str {
    match event {
        TRIGGER_INSERT => "INSERT",
        TRIGGER_UPDATE => "UPDATE",
        TRIGGER_DELETE => "DELETE",
        _ => "TRUNCATE",
    }
}

/// The words of `TG_WHEN` of a trigger.
fn when_name(trigger: &Trigger) -> &'static str {
    if trigger.kind & TRIGGER_BEFORE != 0 {
        "BEFORE"
    } else if trigger.kind & TRIGGER_INSTEAD != 0 {
        "INSTEAD OF"
    } else {
        "AFTER"
    }
}

/// The row with a null for each column after its values, as `heap_getattr` gives a column that `ADD COLUMN` added after the version.
fn padded(mut row: Vec<Value>, width: usize) -> Vec<Value> {
    row.resize(width, Value::Null);
    row
}

impl Connection {
    /// The triggers of `CREATE TRIGGER` that fire for the write `info`, in the order of their names.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn write_triggers(&self, info: &Write) -> Result<Triggers, Error> {
        let event = match info.kind {
            WriteKind::Insert => TRIGGER_INSERT,
            WriteKind::Update => TRIGGER_UPDATE,
            WriteKind::Delete => TRIGGER_DELETE,
            WriteKind::Merge => return Ok(Triggers::default()),
        };
        let catalog = self.catalog();
        let found = catalog.triggers_of(info.table);
        let mut found =
            found.into_iter().filter(|t| !t.internal && t.fires() && t.kind & event != 0);
        let Some(first) = found.next() else { return Ok(Triggers::default()) };
        let reader = self.reader();
        let mut list = Vec::new();
        for trigger in std::iter::once(first).chain(found) {
            let when = rupg_analyze::trigger_when(&reader, trigger.oid)?;
            list.push(Armed { trigger: trigger.clone(), when });
        }
        Ok(Triggers { list, event })
    }

    /// `TriggerEnabled`: true when the trigger fires for the rows `old` and `new`. A trigger `UPDATE OF` fires when `SET` gives one of its columns, and a trigger with `WHEN` fires when its condition is true.
    fn enabled(
        &self,
        armed: &Armed,
        info: &Write,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> Result<bool, Error> {
        let t = &armed.trigger;
        if info.kind == WriteKind::Update
            && !t.columns.is_empty()
            && !t.columns.iter().any(|c| info.updated.contains(c))
        {
            return Ok(false);
        }
        let Some((base, qual)) = &armed.when else { return Ok(true) };
        let nulls = || vec![Value::Null; self.table_width(info)];
        let old = old.map_or_else(nulls, <[Value]>::to_vec);
        let new = new.map_or_else(nulls, <[Value]>::to_vec);
        let reader = self.reader();
        rupg_exec::trigger_when(base, qual, old, new, &reader)
    }

    /// A call of the trigger function of `trigger` for the event `event`, with the transition tables `transition`. It gives the row that the function returns.
    fn call_trigger(
        &self,
        trigger: &Trigger,
        event: i16,
        (old, new): (Option<Vec<Value>>, Option<Vec<Value>>),
        transition: Option<Transition>,
    ) -> Result<Option<Vec<Value>>, Error> {
        let catalog = self.catalog();
        let function = catalog.function(trigger.function);
        if function.is_none_or(|f| f.lang != rupg_plpgsql::PLPGSQL_LANGUAGE) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a trigger function in a language other than PL/pgSQL is not supported yet",
            ));
        }
        let Some(rel) = catalog.relation(trigger.relation) else {
            return Err(Error::internal(format!(
                "could not open relation with OID {}",
                trigger.relation
            )));
        };
        let schema = match catalog.schema(rel.namespace) {
            Some(schema) => schema.name.clone(),
            None => rupg_pgcatalog::builtin::named_by_oid(Named::Namespace, rel.namespace)
                .map_or_else(String::new, |n| n.name.to_string()),
        };
        let columns = rel.columns.iter();
        let columns =
            columns.map(|c| (c.name.clone(), if c.dropped { 0 } else { c.ty }, c.typmod)).collect();
        let row = trigger.kind & TRIGGER_ROW != 0;
        let call = rupg_plpgsql::TriggerCall {
            trigger,
            when: when_name(trigger),
            level: if row { "ROW" } else { "STATEMENT" },
            op: op_name(event),
            relname: &rel.name,
            schema: &schema,
            columns,
            new,
            old,
        };
        self.pl_trigger(&call, transition)
    }

    /// `ExecBSInsertTriggers` and the other calls of the triggers `BEFORE ... FOR EACH STATEMENT` of a write.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn before_statement(&self, triggers: &Triggers, info: &Write) -> Result<(), Error> {
        for armed in triggers.of(false, true) {
            if self.enabled(armed, info, None, None)? {
                self.call_trigger(&armed.trigger, triggers.event, (None, None), None)?;
            }
        }
        Ok(())
    }

    /// `ExecBRInsertTriggers` and the other calls of the triggers `BEFORE ... FOR EACH ROW` of a write, for the rows of the plan. A trigger that gives a null row skips the row, and a trigger that gives a row changes the new row. Then the checks of the constraints run on each new row. The rows of an `INSERT` and of an `UPDATE` have the columns, the conditions of the checks and then the other values, and the rows of an `UPDATE` and of a `DELETE` end with the ID of the version of the old row.
    ///
    /// # Errors
    ///
    /// The errors of the triggers and of the checks.
    pub(super) fn before_rows(
        &self,
        plan: &Plan,
        info: &Write,
        triggers: &Triggers,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>, Error> {
        let before: Vec<&Armed> = triggers.of(true, true).collect();
        if before.is_empty() {
            return Ok(rows);
        }
        let width = info.width;
        let all = self.table_width(info);
        let checked = info.recheck.is_some();
        let conds = if checked { info.checks.len() } else { 0 };
        let heap = self.store.heap();
        let mut out = Vec::with_capacity(rows.len());
        'rows: for mut row in rows {
            let old = match (info.kind, row.last()) {
                (WriteKind::Insert, _) => None,
                (_, Some(Value::Int8(tid))) => {
                    let version =
                        heap.versions(self.database_oid, info.table, &[tid.cast_unsigned()]);
                    Some(padded(version.into_iter().next().unwrap_or_default(), all))
                }
                _ => return Err(Error::internal("a row to change with no ID of its version")),
            };
            if info.kind == WriteKind::Delete {
                for armed in &before {
                    if self.enabled(armed, info, old.as_deref(), None)?
                        && self
                            .call_trigger(
                                &armed.trigger,
                                triggers.event,
                                (old.clone(), None),
                                None,
                            )?
                            .is_none()
                    {
                        continue 'rows;
                    }
                }
                out.push(row);
                continue;
            }
            let tail = row.split_off(width + conds);
            let given = row.split_off(width);
            let mut new = row;
            let mut fired = false;
            for armed in &before {
                if !self.enabled(armed, info, old.as_deref(), Some(&new))? {
                    continue;
                }
                let returned = self.call_trigger(
                    &armed.trigger,
                    triggers.event,
                    (old.clone(), Some(new.clone())),
                    None,
                )?;
                let Some(returned) = returned else { continue 'rows };
                new = returned;
                fired = true;
            }
            let reader = self.reader();
            let mut row = if fired {
                plan.recheck(new, &reader)?
            } else {
                new.extend(given);
                new
            };
            row.extend(tail);
            if checked {
                out.extend(plan.check_rows(vec![row], &reader)?);
            } else {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// `ExecIRInsertTriggers`, `ExecIRUpdateTriggers` and `ExecIRDeleteTriggers`: the triggers `INSTEAD OF ... FOR EACH ROW` of a view for the rows of the plan, in the order of their names. A trigger that gives a null row skips the row, and the row that a trigger gives is the new row for the next trigger and for `RETURNING`. The rows of an `INSERT` and of an `UPDATE` start with the new row, and the rows of an `UPDATE` and of a `DELETE` end with the old row. It gives the number of rows that the triggers did not skip, and the rows of `RETURNING`.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn instead_rows(
        &self,
        info: &Write,
        triggers: &Triggers,
        rows: Vec<Vec<Value>>,
    ) -> Result<(u64, Vec<Vec<Value>>), Error> {
        let instead: Vec<&Armed> =
            triggers.list.iter().filter(|t| t.trigger.kind & INSTEAD_ROW == INSTEAD_ROW).collect();
        let all = self.table_width(info);
        let returning = info.returning.is_some();
        let (mut count, mut written) = (0, Vec::new());
        'rows: for mut row in rows {
            let old = (info.kind != WriteKind::Insert)
                .then(|| row.split_off(row.len().saturating_sub(all)));
            let mut new = (info.kind != WriteKind::Delete).then(|| row[..info.width].to_vec());
            for armed in &instead {
                let rows = (old.clone(), new.clone());
                let Some(returned) =
                    self.call_trigger(&armed.trigger, triggers.event, rows, None)?
                else {
                    continue 'rows;
                };
                if new.is_some() {
                    new = Some(returned);
                }
            }
            count += 1;
            if returning {
                match new {
                    Some(mut new) => {
                        new.extend(row.drain(info.width..));
                        written.push(new);
                    }
                    None => written.push(row),
                }
            }
        }
        Ok((count, written))
    }

    /// `ExecARInsertTriggers`, `ExecASInsertTriggers` and the other calls of the triggers `AFTER` of a write: the events of the triggers `FOR EACH ROW` for the rows `old` and `new` that the write changed, then the events of the triggers `FOR EACH STATEMENT`, at the end of the queue. A trigger with transition tables reads all the rows of the write in them.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn after_write(
        &self,
        info: &Write,
        triggers: &Triggers,
        old: &[Vec<Value>],
        new: &[Vec<Value>],
    ) -> Result<(), Error> {
        let width = self.table_width(info);
        let old: Vec<Vec<Value>> = old.iter().map(|row| padded(row.clone(), width)).collect();
        let new: Vec<Vec<Value>> =
            new.iter().map(|row| padded(row[..width.min(row.len())].to_vec(), width)).collect();
        let captured = if triggers.list.iter().any(|t| has_transition(&t.trigger)) {
            // The old rows of an `INSERT` and the new rows of a `DELETE` are empty tables.
            Rc::new(Captured {
                table: info.table,
                old: Rc::new(old.clone()),
                new: Rc::new(new.clone()),
            })
        } else {
            Rc::default()
        };
        let mut events = Vec::new();
        for i in 0..old.len().max(new.len()) {
            let (old, new) = (old.get(i), new.get(i));
            for armed in triggers.of(true, false) {
                if self.enabled(armed, info, old.map(Vec::as_slice), new.map(Vec::as_slice))? {
                    let trigger = armed.trigger.clone();
                    let transition = transition_of(&trigger, &captured);
                    let (old, new) = (old.cloned(), new.cloned());
                    events.push(Event { trigger, event: triggers.event, old, new, transition });
                }
            }
        }
        for armed in triggers.of(false, false) {
            if self.enabled(armed, info, None, None)? {
                let trigger = armed.trigger.clone();
                let transition = transition_of(&trigger, &captured);
                let (old, new) = (None, None);
                events.push(Event { trigger, event: triggers.event, old, new, transition });
            }
        }
        self.events.borrow_mut().extend(events);
        Ok(())
    }

    /// `afterTriggerInvokeEvents`: fires the triggers `AFTER` of the queue from the place `mark`, in the order of the queue. The events of the statements of a trigger fire at the end of each statement. After an error the queue goes back to the place.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn fire_events(&self, mark: usize) -> Result<(), Error> {
        loop {
            let events = {
                let mut queue = self.events.borrow_mut();
                if queue.len() <= mark {
                    return Ok(());
                }
                queue.split_off(mark)
            };
            for event in events {
                let rows = (event.old, event.new);
                let fired = self.call_trigger(&event.trigger, event.event, rows, event.transition);
                if let Err(error) = fired {
                    self.events.borrow_mut().truncate(mark);
                    return Err(error);
                }
            }
        }
    }

    /// The number of the columns of the table of the write, with the dropped columns. The rows of a `DELETE` have no columns.
    fn table_width(&self, info: &Write) -> usize {
        self.catalog().relation(info.table).map_or(info.width, |r| r.columns.len())
    }

    /// The place at the end of the queue of the triggers `AFTER`.
    pub(super) fn event_mark(&self) -> usize {
        self.events.borrow().len()
    }

    /// The messages of `RAISE` of the triggers that fired, as notices.
    pub(super) fn trigger_notices(&self) -> Vec<Notice> {
        self.trigger_warnings.take().into_iter().map(Notice::of_function).collect()
    }

    /// The start of a statement of the client: no events in the queue and no messages of the triggers.
    pub(super) fn start_triggers(&self) {
        self.events.borrow_mut().clear();
        self.trigger_warnings.borrow_mut().clear();
    }
}

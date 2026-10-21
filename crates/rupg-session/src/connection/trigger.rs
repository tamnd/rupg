//! The triggers of `CREATE TRIGGER` on the writes of a table, as `trigger.c` fires them: the triggers `BEFORE` before the write, and the queue of the triggers `AFTER`, which fire at the end of the statement, as `AfterTriggerEndQuery` fires them.
//!
//! The triggers `BEFORE ... FOR EACH STATEMENT` fire after the query of the write gives its rows. The triggers `BEFORE ... FOR EACH ROW` fire for all the rows before the first row goes into the heap, so a trigger does not see the rows that its statement wrote before. `INSERT ... ON CONFLICT` fires them for each row in turn, as `upsert.rs` tells, and `MERGE` fires them for each action, as `merge.rs` tells. Only a trigger function in PL/pgSQL can fire yet.

use std::rc::Rc;

use rupg_analyze::{Expr, Merge, MergeCommand, Query, Write, WriteKind};
use rupg_catalog::{Catalog, TRIGGER_INSTEAD, Trigger};
use rupg_catalog::{TRIGGER_BEFORE, TRIGGER_DELETE, TRIGGER_INSERT, TRIGGER_ROW, TRIGGER_UPDATE};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_pgcatalog::builtin::Named;
use rupg_types::Value;

use super::Connection;
use super::merge::Action;
use super::upsert::{Upsert, Upserted};
use crate::query::{Captured, Reader, Transition};
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
#[derive(Debug)]
pub(super) struct Armed {
    trigger: Trigger,
    /// The condition and the query that has `old` and `new` as its relations.
    when: Option<(Query, Expr)>,
}

/// The triggers of a write, in the order of their names.
#[derive(Debug, Default)]
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

    /// True when the write fires a trigger `BEFORE ... FOR EACH ROW`.
    pub(super) fn before_rows(&self) -> bool {
        self.of(true, true).next().is_some()
    }

    /// Keeps only the triggers `FOR EACH STATEMENT`. The triggers `FOR EACH ROW` of a partitioned table do not fire, as their clones in the leaf partitions fire.
    pub(super) fn retain_statement(&mut self) {
        self.list.retain(|t| t.trigger.kind & TRIGGER_ROW == 0);
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

/// The triggers of a `MERGE`, for each action of its `WHEN` clauses. An action that no clause takes has no triggers.
#[derive(Debug, Default)]
pub(super) struct MergeTriggers {
    insert: Triggers,
    update: Triggers,
    delete: Triggers,
}

impl MergeTriggers {
    /// True when the `MERGE` fires no trigger.
    pub(super) fn is_empty(&self) -> bool {
        self.insert.is_empty() && self.update.is_empty() && self.delete.is_empty()
    }

    /// The triggers of the action `command`. `DO NOTHING` fires no trigger.
    pub(super) fn of(&self, command: MergeCommand) -> Option<&Triggers> {
        match command {
            MergeCommand::Insert => Some(&self.insert),
            MergeCommand::Update => Some(&self.update),
            MergeCommand::Delete => Some(&self.delete),
            MergeCommand::Nothing => None,
        }
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

/// The rows of a write that the transition tables of `triggers` read, or no rows when no trigger has `REFERENCING`. The old rows of an `INSERT` and the new rows of a `DELETE` are empty tables.
fn captured(
    info: &Write,
    triggers: &Triggers,
    old: &[Vec<Value>],
    new: &[Vec<Value>],
) -> Rc<Captured> {
    if triggers.list.iter().any(|t| has_transition(&t.trigger)) {
        Rc::new(Captured {
            table: info.table,
            old: Rc::new(old.to_vec()),
            new: Rc::new(new.to_vec()),
        })
    } else {
        Rc::default()
    }
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

/// The triggers of `CREATE TRIGGER` of the table `table` that fire for the event `event`, in the order of their names.
///
/// # Errors
///
/// The error of the condition of `WHEN` of a trigger.
pub(super) fn armed(
    catalog: &Catalog,
    reader: &Reader<'_>,
    table: u32,
    event: i16,
) -> Result<Triggers, Error> {
    let found = catalog.triggers_of(table);
    let found = found.into_iter().filter(|t| !t.internal && t.fires() && t.kind & event != 0);
    let mut list = Vec::new();
    for trigger in found {
        let when = rupg_analyze::trigger_when(reader, trigger.oid)?;
        list.push(Armed { trigger: trigger.clone(), when });
    }
    Ok(Triggers { list, event })
}

/// The old and the new row of a change, as a trigger `FOR EACH ROW` gets them.
pub(super) type RowPair = (Option<Vec<Value>>, Option<Vec<Value>>);

/// The columns that an `UPDATE` sets and the number of the columns of the table with the dropped columns, which `TriggerEnabled` reads.
pub(super) type Shape<'a> = (&'a [i16], usize);

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
        self.triggers_of(info, event)
    }

    /// The triggers of a `MERGE` for the actions of its `WHEN` clauses, as `ExecInitMerge` gives `mt_merge_subcommands`.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn merge_triggers(
        &self,
        info: &Write,
        merge: &Merge,
    ) -> Result<MergeTriggers, Error> {
        let of = |command: MergeCommand, event: i16| {
            if merge.clauses.iter().any(|c| c.command == command) {
                self.triggers_of(info, event)
            } else {
                Ok(Triggers::default())
            }
        };
        Ok(MergeTriggers {
            insert: of(MergeCommand::Insert, TRIGGER_INSERT)?,
            update: of(MergeCommand::Update, TRIGGER_UPDATE)?,
            delete: of(MergeCommand::Delete, TRIGGER_DELETE)?,
        })
    }

    /// The triggers `UPDATE` that fire for `INSERT ... ON CONFLICT DO UPDATE`, or no triggers for another write.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn conflict_triggers(&self, info: &Write) -> Result<Triggers, Error> {
        if info.on_conflict.as_ref().is_some_and(|c| c.update.is_some()) {
            self.triggers_of(info, TRIGGER_UPDATE)
        } else {
            Ok(Triggers::default())
        }
    }

    /// The triggers of the table of the write for the event `event`, in the order of their names.
    fn triggers_of(&self, info: &Write, event: i16) -> Result<Triggers, Error> {
        let catalog = self.catalog();
        let reader = self.reader();
        armed(&catalog, &reader, info.table, event)
    }

    /// `TriggerEnabled`: true when the trigger fires for the rows `old` and `new`. A trigger `UPDATE OF` fires when `SET` gives one of its columns, and a trigger with `WHEN` fires when its condition is true.
    fn enabled(
        &self,
        armed: (&Armed, i16),
        info: &Write,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> Result<bool, Error> {
        self.enabled_in(armed, (&info.updated, self.table_width(info)), old, new)
    }

    /// `TriggerEnabled` for a table of the shape `shape`.
    fn enabled_in(
        &self,
        (armed, event): (&Armed, i16),
        (updated, width): Shape<'_>,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> Result<bool, Error> {
        let t = &armed.trigger;
        if event == TRIGGER_UPDATE
            && !t.columns.is_empty()
            && !t.columns.iter().any(|c| updated.contains(c))
        {
            return Ok(false);
        }
        let Some((base, qual)) = &armed.when else { return Ok(true) };
        let nulls = || vec![Value::Null; width];
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
        // `ExecCallTriggerFunc`: the depth goes up for the call and down after it, also after an error.
        self.trigger_depth.set(self.trigger_depth.get() + 1);
        let result = self.pl_trigger(&call, transition);
        self.trigger_depth.set(self.trigger_depth.get() - 1);
        result
    }

    /// `pg_trigger_depth`: the number of the trigger functions that run, one in the other.
    pub(crate) fn trigger_depth(&self) -> i32 {
        self.trigger_depth.get()
    }

    /// `ExecBSInsertTriggers` and the other calls of the triggers `BEFORE ... FOR EACH STATEMENT` of a write.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn before_statement(&self, triggers: &Triggers, info: &Write) -> Result<(), Error> {
        for armed in triggers.of(false, true) {
            if self.enabled((armed, triggers.event), info, None, None)? {
                self.call_trigger(&armed.trigger, triggers.event, (None, None), None)?;
            }
        }
        Ok(())
    }

    /// `fireBSTriggers` for a `MERGE`: the triggers `BEFORE ... FOR EACH STATEMENT` of `INSERT`, then of `UPDATE`, then of `DELETE`.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn before_merge(&self, fire: &MergeTriggers, info: &Write) -> Result<(), Error> {
        for triggers in [&fire.insert, &fire.update, &fire.delete] {
            self.before_statement(triggers, info)?;
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
        let checked = info.recheck.is_some();
        if before.is_empty() {
            // The rows of an upsert with only triggers of `DO UPDATE` have the conditions of the checks.
            return if checked { plan.check_rows(rows, &self.reader()) } else { Ok(rows) };
        }
        let width = info.width;
        let all = self.table_width(info);
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
                    if self.enabled((armed, triggers.event), info, old.as_deref(), None)?
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
                if !self.enabled((armed, triggers.event), info, old.as_deref(), Some(&new))? {
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
        let (old, new) = self.after_rows_of(info, old, new);
        let captured = captured(info, triggers, &old, &new);
        let mut events = Vec::new();
        for i in 0..old.len().max(new.len()) {
            self.row_events(info, (triggers, &captured), (old.get(i), new.get(i)), &mut events)?;
        }
        self.statement_events(info, triggers, &captured, &mut events)?;
        self.events.borrow_mut().extend(events);
        Ok(())
    }

    /// The calls of the triggers `AFTER` of `INSERT ... ON CONFLICT`, as `fireASTriggers` gives them: the events of the triggers `FOR EACH ROW` in the order of the rows, then the triggers `UPDATE ... FOR EACH STATEMENT` of `DO UPDATE`, then the triggers `INSERT ... FOR EACH STATEMENT`. The transition tables of the triggers `INSERT` have the rows that went in, and the transition tables of the triggers `UPDATE` have the rows that changed.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn after_upsert(
        &self,
        info: &Write,
        fire: &Upsert,
        done: &Upserted,
    ) -> Result<(), Error> {
        let (_, inserted) = self.after_rows_of(info, &[], &done.inserted);
        let (old, new) = self.after_rows_of(info, &done.old, &done.new);
        let insert = captured(info, &fire.insert, &[], &inserted);
        let update = captured(info, &fire.update, &old, &new);
        let mut events = Vec::new();
        let (mut ins, mut upd) = (inserted.iter(), old.iter().zip(&new));
        for &went_in in &done.steps {
            if went_in {
                let row = (None, ins.next());
                self.row_events(info, (&fire.insert, &insert), row, &mut events)?;
            } else if let Some((old, new)) = upd.next() {
                let row = (Some(old), Some(new));
                self.row_events(info, (&fire.update, &update), row, &mut events)?;
            }
        }
        self.statement_events(info, &fire.update, &update, &mut events)?;
        self.statement_events(info, &fire.insert, &insert, &mut events)?;
        self.events.borrow_mut().extend(events);
        Ok(())
    }

    /// The old and the new rows of a write with the columns of the table and the nulls of the columns that `ADD COLUMN` added after a version.
    fn after_rows_of(
        &self,
        info: &Write,
        old: &[Vec<Value>],
        new: &[Vec<Value>],
    ) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
        let width = self.table_width(info);
        let old = old.iter().map(|row| padded(row.clone(), width)).collect();
        let new =
            new.iter().map(|row| padded(row[..width.min(row.len())].to_vec(), width)).collect();
        (old, new)
    }

    /// `AfterTriggerSaveEvent` for one row: the events of the triggers `AFTER ... FOR EACH ROW` of `triggers` for the old row and the new row.
    fn row_events(
        &self,
        info: &Write,
        fire: (&Triggers, &Rc<Captured>),
        rows: (Option<&Vec<Value>>, Option<&Vec<Value>>),
        events: &mut Vec<Event>,
    ) -> Result<(), Error> {
        self.row_events_in((&info.updated, self.table_width(info)), fire, rows, events)
    }

    /// `AfterTriggerSaveEvent` for one row of a table of the shape `shape`.
    fn row_events_in(
        &self,
        shape: Shape<'_>,
        (triggers, captured): (&Triggers, &Rc<Captured>),
        (old, new): (Option<&Vec<Value>>, Option<&Vec<Value>>),
        events: &mut Vec<Event>,
    ) -> Result<(), Error> {
        for armed in triggers.of(true, false) {
            if self.enabled_in(
                (armed, triggers.event),
                shape,
                old.map(Vec::as_slice),
                new.map(Vec::as_slice),
            )? {
                let trigger = armed.trigger.clone();
                let transition = transition_of(&trigger, captured);
                let (old, new) = (old.cloned(), new.cloned());
                events.push(Event { trigger, event: triggers.event, old, new, transition });
            }
        }
        Ok(())
    }

    /// The events of the triggers `AFTER ... FOR EACH STATEMENT` of `triggers`, at the end of the queue.
    fn statement_events(
        &self,
        info: &Write,
        triggers: &Triggers,
        captured: &Rc<Captured>,
        events: &mut Vec<Event>,
    ) -> Result<(), Error> {
        for armed in triggers.of(false, false) {
            if self.enabled((armed, triggers.event), info, None, None)? {
                let trigger = armed.trigger.clone();
                let transition = transition_of(&trigger, captured);
                let (old, new) = (None, None);
                events.push(Event { trigger, event: triggers.event, old, new, transition });
            }
        }
        Ok(())
    }

    /// `ExecBRInsertTriggers` or `ExecBRUpdateTriggers` for one new row `new`: a row of `ON CONFLICT DO UPDATE` or of an action of a `MERGE`, with the row `old` of the table for an update. The new row has the columns, then the conditions of the checks. A trigger that gives a null row skips the row, and then the checks of the constraints run on the new row. It gives the columns of the new row.
    ///
    /// # Errors
    ///
    /// The errors of the triggers and of the checks.
    pub(super) fn before_row(
        &self,
        plan: &Plan,
        info: &Write,
        triggers: &Triggers,
        old: Option<&[Value]>,
        mut new: Vec<Value>,
    ) -> Result<Option<Vec<Value>>, Error> {
        let old = old.map(|old| padded(old.to_vec(), self.table_width(info)));
        let given = new.split_off(info.width.min(new.len()));
        let mut fired = false;
        for armed in triggers.of(true, true) {
            if !self.enabled((armed, triggers.event), info, old.as_deref(), Some(&new))? {
                continue;
            }
            let rows = (old.clone(), Some(new.clone()));
            let Some(returned) = self.call_trigger(&armed.trigger, triggers.event, rows, None)?
            else {
                return Ok(None);
            };
            new = returned;
            fired = true;
        }
        let reader = self.reader();
        let row = if fired {
            plan.recheck(new, &reader)?
        } else {
            new.extend(given);
            new
        };
        Ok(plan.check_rows(vec![row], &reader)?.into_iter().next())
    }

    /// `ExecBRDeleteTriggers` for the row `old` of the table that an action of a `MERGE` deletes. It is false when a trigger gives a null row, which skips the row.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn before_delete(
        &self,
        info: &Write,
        triggers: &Triggers,
        old: &[Value],
    ) -> Result<bool, Error> {
        let old = padded(old.to_vec(), self.table_width(info));
        for armed in triggers.of(true, true) {
            if self.enabled((armed, triggers.event), info, Some(&old), None)?
                && self
                    .call_trigger(&armed.trigger, triggers.event, (Some(old.clone()), None), None)?
                    .is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `fireASTriggers` for a `MERGE` and the calls of its triggers `AFTER ... FOR EACH ROW`: the events of the triggers `FOR EACH ROW` in the order of the actions, then the triggers `FOR EACH STATEMENT` of `DELETE`, then of `UPDATE`, then of `INSERT`. The transition tables of the triggers of each event have the rows of the actions of that event.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn after_merge(
        &self,
        info: &Write,
        fire: &MergeTriggers,
        done: &[Action],
    ) -> Result<(), Error> {
        let width = self.table_width(info);
        let pad = |row: &[Value]| padded(row[..width.min(row.len())].to_vec(), width);
        let of = |command: MergeCommand| done.iter().filter(move |a| a.command == command);
        let news = |command| of(command).filter_map(|a| a.new.as_deref().map(pad)).collect();
        let olds = |command| of(command).map(|a| pad(&a.old)).collect::<Vec<_>>();
        let inserted: Vec<_> = news(MergeCommand::Insert);
        let (old, new) = (olds(MergeCommand::Update), news(MergeCommand::Update));
        let deleted = olds(MergeCommand::Delete);
        let insert = captured(info, &fire.insert, &[], &inserted);
        let update = captured(info, &fire.update, &old, &new);
        let delete = captured(info, &fire.delete, &deleted, &[]);
        let mut events = Vec::new();
        for action in done {
            let (triggers, captured) = match action.command {
                MergeCommand::Insert => (&fire.insert, &insert),
                MergeCommand::Update => (&fire.update, &update),
                MergeCommand::Delete => (&fire.delete, &delete),
                MergeCommand::Nothing => continue,
            };
            let old = (action.command != MergeCommand::Insert).then(|| pad(&action.old));
            let new = action.new.as_deref().map(pad);
            let rows = (old.as_ref(), new.as_ref());
            self.row_events(info, (triggers, captured), rows, &mut events)?;
        }
        self.statement_events(info, &fire.delete, &delete, &mut events)?;
        self.statement_events(info, &fire.update, &update, &mut events)?;
        self.statement_events(info, &fire.insert, &insert, &mut events)?;
        self.events.borrow_mut().extend(events);
        Ok(())
    }

    /// `ExecBRInsertTriggers`, `ExecBRUpdateTriggers` and `ExecBRDeleteTriggers` for a row of a leaf partition of the shape `shape`, with all the columns of the leaf. A trigger that gives a null row skips the row, and the row that a trigger gives is the new row for the next trigger. After each clone of a trigger of a partitioned table, `stays` must be true for the new row: a row cannot move to another partition. The result is the new row, or the old row for a `DELETE`.
    ///
    /// # Errors
    ///
    /// The errors of the triggers and of `stays`, and `0A000` for a row that a trigger moves to another partition. `leaf` is the name of the leaf with its schema for the error.
    pub(super) fn leaf_before(
        &self,
        (triggers, shape): (&Triggers, Shape<'_>),
        (old, mut new): (Option<Vec<Value>>, Option<Vec<Value>>),
        leaf: &str,
        stays: &mut dyn FnMut(&[Value]) -> Result<bool, Error>,
    ) -> Result<Option<Vec<Value>>, Error> {
        let width = shape.1;
        let old = old.map(|row| padded(row, width));
        for armed in triggers.of(true, true) {
            if !self.enabled_in((armed, triggers.event), shape, old.as_deref(), new.as_deref())? {
                continue;
            }
            let rows = (old.clone(), new.clone());
            let Some(returned) = self.call_trigger(&armed.trigger, triggers.event, rows, None)?
            else {
                return Ok(None);
            };
            let Some(_) = new else { continue };
            if armed.trigger.parent != 0 && !stays(&returned)? {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "moving row to another partition during a BEFORE FOR EACH ROW trigger is not supported",
                )
                .with_detail(format!(
                    "Before executing trigger \"{}\", the row was to be in partition \"{leaf}\".",
                    armed.trigger.name
                )));
            }
            new = Some(returned);
        }
        Ok(new.or(old))
    }

    /// The events of the triggers `AFTER ... FOR EACH ROW` of a leaf partition of the shape `shape` for its rows `rows`, the old and the new row of each change, at the end of the queue.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn leaf_after(
        &self,
        (triggers, shape): (&Triggers, Shape<'_>),
        rows: &[RowPair],
    ) -> Result<(), Error> {
        let width = shape.1;
        let mut events = Vec::new();
        let none = Rc::default();
        for (old, new) in rows {
            let old = old.clone().map(|row| padded(row, width));
            let new = new.as_ref().map(|row| padded(row[..width.min(row.len())].to_vec(), width));
            self.row_events_in(
                shape,
                (triggers, &none),
                (old.as_ref(), new.as_ref()),
                &mut events,
            )?;
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
                // `afterTriggerMarkEvents`: the event of a deferred constraint trigger goes to the queue of the transaction.
                if event.trigger.deferrable && self.modes.borrow().trigger_deferred(&event.trigger)
                {
                    self.deferred_events.borrow_mut().push(event);
                    continue;
                }
                let rows = (event.old, event.new);
                let fired = self.call_trigger(&event.trigger, event.event, rows, event.transition);
                if let Err(error) = fired {
                    self.events.borrow_mut().truncate(mark);
                    return Err(error);
                }
            }
        }
    }

    /// `afterTriggerInvokeEvents` for the queue of the deferred constraint triggers: the events whose triggers are no longer deferred fire in order, and the other events stay in the queue.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn fire_deferred_events(&self) -> Result<(), Error> {
        let events = std::mem::take(&mut *self.deferred_events.borrow_mut());
        let (later, now): (Vec<Event>, Vec<Event>) = {
            let modes = self.modes.borrow();
            events.into_iter().partition(|e| modes.trigger_deferred(&e.trigger))
        };
        self.deferred_events.borrow_mut().extend(later);
        for event in now {
            let rows = (event.old, event.new);
            self.call_trigger(&event.trigger, event.event, rows, event.transition)?;
        }
        Ok(())
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

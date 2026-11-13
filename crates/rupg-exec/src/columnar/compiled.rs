//! The compiled engine of document 14 section 14.12 for the groups of a query of one table.
//!
//! [`grouped`] lowers the conditions of `WHERE`, the keys of `GROUP BY` and the aggregate calls into the tree of `rupg_jit::plan`. The generator compiles the tree into one pipeline, and the driver runs it on the tier that `rupg.compile` names, with each vector of the columns as one batch. The groups then go in the order that the row engine gives them, and `HAVING` and the targets run with [`Eval`] on one row for each group, as for the groups of the vectors. A query that the lowering, the generator or the driver refuses runs on the vectors.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rupg_analyze::{Aggref, Expr, ExprKind, Query};
use rupg_column::segment::ROWS;
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_jit::drive::{self, Batch, Encoded};
use rupg_jit::plan::{self, Column, Rel, Scope, Type};
use rupg_jit::rt::Rt;
use rupg_jit::tier::{Options, Platform, Tier, Tiers};
use rupg_summary::zone::{Test, Verdict};
use rupg_types::Value;

use super::{Decoded, DecodedData, Exec, GroupKeys, Grouped, Want, decode, group_order, kind_of};
use crate::{Eval, Group, scan};

/// The tier that the pipelines start on and whether they stay on it, from `rupg.compile`, or `None` when the vectors run the query. Under `auto` the compiled engine runs only in a build with a tier of machine code, as the interpreter of the pipelines is slower than the vectors.
fn options(session: &dyn Session) -> Option<Options> {
    let native = [Tier::Direct, Tier::Clif].into_iter().find(|t| t.built());
    let (tier, stay) = match session.setting("rupg.compile").as_deref() {
        Some("interpreter") => (Tier::Interp, true),
        // The tier of machine code that the build has, the single pass one first.
        Some("direct") => (native.unwrap_or(Tier::Interp), false),
        _ => {
            native?;
            (Tier::Auto, false)
        }
    };
    Some(Options { tier, stay, ..Options::default() })
}

/// The groups of the grouped query `query` of one table from the compiled engine, with the projection of its targets, or `None` when the engine does not run the query. `exec` has the columns of the table, `conds` the conditions of `WHERE` in the order of the plan, `calls` the aggregate calls, and `kept` the columns that the targets and `HAVING` read outside the calls.
///
/// # Errors
///
/// The error of a condition, an aggregate, `HAVING` or a target.
pub(super) fn grouped(
    exec: &Exec<'_>,
    query: &Query,
    conds: &[&Expr],
    calls: &[(&Aggref, &Expr)],
    kept: &[bool],
    rows: usize,
    needed: usize,
) -> Result<Option<Grouped>> {
    let Some(options) = options(exec.session) else { return Ok(None) };
    let Some((clock, tasks)) = exec.session.platform() else { return Ok(None) };
    if !exec.links.is_empty() || !crate::agg::outer_calls(query).is_empty() {
        return Ok(None);
    }
    // The column of each key, which must be a column of the table, so that a group gives the values of the columns that the targets read.
    let mut keys = Vec::with_capacity(query.group.len());
    for item in &query.group {
        let ExprKind::Var(var) = &query.targets[item.target].expr.kind else { return Ok(None) };
        let Some(column) = usize::try_from(var.attnum - 1).ok() else { return Ok(None) };
        if var.levels_up != 0 || var.relation != 0 {
            return Ok(None);
        }
        keys.push(column);
    }
    if kept.iter().enumerate().any(|(c, &k)| k && !keys.contains(&c)) {
        return Ok(None);
    }
    let Ok(rel) = lower(exec, query, conds, calls) else { return Ok(None) };
    let graph = rupg_jit::pipe::split(&rel);
    // The driver asks the session at each vector whether the statement is cancelled, so the flag of the compiled code stays clear.
    let mut rt = Rt::new(Arc::new(AtomicBool::new(false)));
    let Ok(compiled) = rupg_jit::codegen::generate(&graph, &mut rt) else { return Ok(None) };
    if drive::check(&graph, &compiled).is_err() {
        return Ok(None);
    }
    let tiers = Tiers::new(&compiled.module, options, &Platform { clock, tasks });
    let tests = conds.iter().filter_map(|c| exec.zone_test(c)).collect();
    let mut vectors = Vectors { exec, rows, tests };
    let groups = drive::run(&graph, &compiled, &tiers, &mut rt, &mut vectors)?;
    let width = keys.len();
    let kept_columns: Vec<usize> = (0..kept.len()).filter(|&c| kept[c]).collect();
    let mut key_values = Vec::with_capacity(groups.len() * width);
    let mut firsts = Vec::with_capacity(groups.len());
    for group in &groups {
        key_values.extend_from_slice(&group[..width]);
        let value =
            |c: &usize| keys.iter().position(|k| k == c).map_or(Value::Null, |k| group[k].clone());
        firsts.push(kept_columns.iter().map(value).collect::<Vec<_>>());
    }
    let order = group_order(
        query,
        exec.session,
        &GroupKeys::Values(&key_values),
        None,
        groups.len(),
        false,
        Want::All,
    )?;
    let tables = scan::kept_tables(query, firsts, kept);
    let levels = crate::set_levels(&query.targets);
    let tuples: Vec<[usize; 1]> = (0..groups.len()).map(|g| [g]).collect();
    let mut out = Vec::new();
    let mut eval = Eval::new(exec.params, exec.session, &tables, exec.cache, &[]);
    for (n, g) in order.into_iter().enumerate() {
        if out.len() >= needed {
            break;
        }
        crate::interrupt(exec.session, n)?;
        eval.tuple = &tuples[g];
        let aggs = calls.iter().zip(&groups[g][width..]);
        let aggs = aggs.map(|((agg, _), value)| (std::ptr::from_ref(*agg), value.clone()));
        eval.group = Group { aggs: aggs.collect(), ..Group::default() };
        if let Some(having) = &query.having
            && eval.eval(having)? != Value::Bool(true)
        {
            continue;
        }
        eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
    }
    drop(eval);
    Ok(Some((out, tables, false)))
}

/// The tree of `query`: the scan of the columns of `exec`, the conditions `conds`, and the aggregate of the keys of `GROUP BY` and the calls `calls`.
fn lower(
    exec: &Exec<'_>,
    query: &Query,
    conds: &[&Expr],
    calls: &[(&Aggref, &Expr)],
) -> plan::Result<Rel> {
    let relation = &query.relations[0];
    let mut columns = vec![None; exec.stored.len()];
    let mut attnums = vec![0; exec.stored.len()];
    for (c, &place) in exec.places.iter().enumerate() {
        if place == usize::MAX {
            continue;
        }
        let column = &relation.columns[c];
        // A column whose type has no kind is not in the columns of the scan.
        if kind_of(column.ty).is_none() {
            return Err(plan::Refusal::new(
                column.name.clone(),
                "a column that is not of a plain kind",
            ));
        }
        let ty = Type { oid: column.ty, typmod: column.typmod };
        columns[place] = Some(Column { name: column.name.clone(), ty });
        attnums[place] = i16::try_from(c + 1)
            .map_err(|_| plan::Refusal::new("column", "past the attribute numbers"))?;
    }
    let columns: Option<Vec<Column>> = columns.into_iter().collect();
    let columns = columns.ok_or_else(|| plan::Refusal::new("scan", "a column with no place"))?;
    let scope = Scope { relation: 0, places: &exec.places, params: exec.params };
    let input =
        Rel::Scan { relation: relation.oid, table: relation.name.clone(), attnums, columns };
    let preds = conds.iter().map(|c| plan::expr(c, &scope)).collect::<plan::Result<Vec<_>>>()?;
    let input = Rel::filtered(input, preds);
    let groups = query
        .group
        .iter()
        .map(|g| plan::expr(&query.targets[g.target].expr, &scope))
        .collect::<plan::Result<Vec<_>>>()?;
    let aggregates = calls
        .iter()
        .map(|(agg, e)| plan::aggregate(agg, Type::of(e), &scope))
        .collect::<plan::Result<Vec<_>>>()?;
    let columns = groups
        .iter()
        .map(|g| g.ty)
        .chain(aggregates.iter().map(|a| a.ty))
        .enumerate()
        .map(|(k, ty)| Column { name: format!("c{k}"), ty })
        .collect();
    Ok(Rel::Aggregate { input: Box::new(input), groups, aggregates, columns, runs: None })
}

/// The vectors of the columns of one table, which the driver reads as its batches.
struct Vectors<'a, 'q> {
    exec: &'a Exec<'q>,
    /// The rows of the table.
    rows: usize,
    /// The zone test of each condition that has one, by the place of its column: a vector whose zone shows that no row passes is not read.
    tests: Vec<(usize, Test)>,
}

impl drive::Scan for Vectors<'_, '_> {
    fn scan(
        &mut self,
        _relation: u32,
        _attnums: &[i16],
        columns: &[Column],
        read: &[bool],
        each: &mut dyn FnMut(Batch) -> Result<bool>,
    ) -> Result<()> {
        let exec = self.exec;
        for segment in 0..self.rows.div_ceil(ROWS) {
            let segment_rows = self.rows.saturating_sub(segment * ROWS).min(ROWS);
            let vectors = stored::vectors(segment_rows);
            for vector in 0..vectors {
                exec.session.interrupted()?;
                let none = |(place, test): &(usize, Test)| {
                    exec.zones(*place, segment, vectors)
                        .is_some_and(|zones| zones[vector].test(test) == Verdict::None)
                };
                if self.tests.iter().any(none) {
                    continue;
                }
                let rows = (segment_rows - vector * VECTOR).min(VECTOR);
                let mut out = Vec::with_capacity(columns.len());
                for (place, column) in columns.iter().enumerate() {
                    out.push(if read.get(place).copied().unwrap_or(false) {
                        encoded(&exec.stored[place], column.ty, segment, vector, rows)?
                    } else {
                        Encoded::default()
                    });
                }
                if !each(Batch { rows, columns: out })? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// The column of `ty` of the vector `vector` of the segment `segment` of `stored`, which has `rows` rows.
fn encoded(
    stored: &Stored,
    ty: Type,
    segment: usize,
    vector: usize,
    rows: usize,
) -> Result<Encoded> {
    let Decoded { data, nulls } = decode(stored, segment, vector)?;
    let null = |i: usize| nulls.as_ref().is_some_and(|n| n[i]);
    let mut e = Encoded::new(ty)?;
    match data {
        DecodedData::Int(values) => {
            for (i, v) in values.into_iter().enumerate() {
                e.push(ty, &if null(i) { Value::Null } else { stored.kind.int_value(v) })?;
            }
        }
        DecodedData::Float(values) => {
            for (i, v) in values.into_iter().enumerate() {
                e.push(ty, &if null(i) { Value::Null } else { stored.kind.float_value(v) })?;
            }
        }
        DecodedData::Code(codes) => {
            let dict = stored.dict.as_ref();
            for (i, code) in codes.into_iter().enumerate() {
                if null(i) {
                    e.push(ty, &Value::Null)?;
                } else {
                    e.push_bytes(dict.map(|d| d.get(code)).unwrap_or_default())?;
                }
            }
        }
    }
    if e.rows() != rows {
        return Err(Error::corrupted("a vector has another number of values than its rows"));
    }
    Ok(e)
}

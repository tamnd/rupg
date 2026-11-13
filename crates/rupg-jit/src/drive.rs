// Lifted from rudb crates/rudb-qc/src/feed.rs at cd9f9676 (2026-10-05).
// Changes: the module `drive` of rupg-jit, which runs the stages of a `Graph` with the bodies `crate::codegen` made for them over the batches a `Scan` hands it, a pipeline on one worker, and gives the answer as rows of rupg's `Value`s. The `run` of rudb-qc's `Compiled` is here as `run`, over a scan of rupg's in place of rudb's executor, with no statistics that answer an aggregate, no handoff of join keys to the scan and no top N cut from a morsel. A batch's columns are made from values by `Encoded`, flat, so the dictionaries of the first engine, `Held::coded`, the `LIKE` answers by dictionary value and the group rows by code are not lifted: a text column and each column of `Body::coded` are read through the codes `0, 1, 2, ...`, and the column of `Body::keyed` is a null address. An error site raises its message with the SQLSTATE of its kind, `22003` for an overflow or a value out of range, `22012` for a division by zero, `22P02` for a conversion and `57014` for a cancel, as PostgreSQL raises them. A pipeline that moves to another worker, the merge of their tables and the parallel scan are not lifted yet. The tests run whole queries on each tier and check the rows and the errors against PostgreSQL's.

//! The driver of the compiled tiers: it runs the stages of a query with the functions [`crate::codegen`] generated for its pipelines and gives the rows of the answer, per section 14.12 of `spec/14-execution.md`.
//!
//! A pipeline's state is set up once: the header, the address of the one row of an aggregate with no groups, and the join tables its probes read, which the builds before it finished. Then each [`Batch`] the source hands over is one morsel, or several when [`Tiers::split`] cuts it, and the body runs over it on the tier [`Tiers::morsel`] picks. The status it returns says what comes next: on to the next morsel, the buffers grown and the morsel run again for a result past a probe, the morsel run again on a guard's fallback, or the error of an error site, which is raised with PostgreSQL's SQLSTATE and message. After the last morsel the sink is finished: a result's rows were read after each call, an aggregate's groups are read out of its table with each accumulator finished by [`finish`], and a build publishes its table to the probes after it.
//!
//! A sort, a top N, a limit and the rows a semi or anti join keeps are stages of their own, and run here over the rows of the stage they read.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::codegen::{AccOp, Body, Grouping, Out, Query};
use crate::ir::status::{Kind as StatusKind, Status};
use crate::ir::{ErrorKind, Module};
use crate::pipe::{Graph, Pipeline, Source, Stage, StateHeader, Step};
use crate::plan::{Column, Kind, Refusal, Type};
use crate::rt::abi::{Col, Morsel};
use crate::rt::table::GroupTable;
use crate::rt::{RUNTIME_ERROR, Rt, text};
use crate::tier::{self, Tiers};

pub mod finish;
mod merge;
#[cfg(test)]
mod tests;

pub use finish::{Encoded, readable, value};

/// The rows of one source of a pipeline, a column per column of the source.
///
/// A column the body does not read may be left empty, with no rows, which is what [`Scan::scan`] is told it may do.
#[derive(Debug, Default)]
pub struct Batch {
    /// How many rows there are.
    pub rows: usize,
    /// The columns.
    pub columns: Vec<Encoded>,
}

impl Batch {
    /// The batch of `rows` over `columns`, every column made from the values.
    ///
    /// # Errors
    ///
    /// For a column whose type [`readable`] does not take, or a value that is not of its type.
    pub fn of_rows(columns: &[Column], rows: &[Vec<Value>]) -> Result<Batch> {
        let mut out = Vec::with_capacity(columns.len());
        for (i, c) in columns.iter().enumerate() {
            let mut e = Encoded::new(c.ty)?;
            for row in rows {
                e.push(c.ty, row.get(i).unwrap_or(&Value::Null))?;
            }
            out.push(e);
        }
        Ok(Batch { rows: rows.len(), columns: out })
    }

    /// The batch of `rows` over `columns`, with only the columns `read` says made from the values and the others left empty.
    fn of_read(columns: &[Column], rows: &[Vec<Value>], read: &[bool]) -> Result<Batch> {
        let mut out = Vec::with_capacity(columns.len());
        for (i, c) in columns.iter().enumerate() {
            let mut e = Encoded::default();
            if read.get(i).copied().unwrap_or(false) {
                e = Encoded::new(c.ty)?;
                for row in rows {
                    e.push(c.ty, row.get(i).unwrap_or(&Value::Null))?;
                }
            }
            out.push(e);
        }
        Ok(Batch { rows: rows.len(), columns: out })
    }
}

/// Where the rows of a table come from.
pub trait Scan {
    /// Hands `each` the rows of the table `relation`, a [`Batch`] at a time with a column per attribute of `attnums`, until there are no more or `each` returns `false`.
    ///
    /// The columns are of the types of `columns`. Only those `read` marks are read by the pipeline, and the others may be left empty.
    ///
    /// # Errors
    ///
    /// Whatever reading the table raises, and whatever `each` returns.
    fn scan(
        &mut self,
        relation: u32,
        attnums: &[i16],
        columns: &[Column],
        read: &[bool],
        each: &mut dyn FnMut(Batch) -> Result<bool>,
    ) -> Result<()>;
}

/// Whether the driver can run `graph` with the bodies of `query`: every pipeline has a body, every column a body reads or a stage gives is of a type [`readable`] takes, or a `numeric` an aggregate gives that nothing after it reads, and every sort key is a column.
///
/// # Errors
///
/// The refusal, which sends the query to the vector executor.
pub fn check(graph: &Graph, query: &Query) -> std::result::Result<(), Refusal> {
    let refuse = |what: String, why: &str| Err(Refusal::new(what, why));
    for (at, stage) in graph.stages.iter().enumerate() {
        match stage {
            Stage::Pipeline(p) => {
                let Some(Some(body)) = query.bodies.get(at) else {
                    return refuse(format!("stage {at}"), "a pipeline with no body");
                };
                let source = p.source.columns();
                for &c in body.reads.iter().chain(body.likes.iter().map(|m| &m.column)) {
                    let ty = source.get(c).map(|c| c.ty);
                    if !ty.is_some_and(readable) {
                        let ty = ty.map_or_else(|| "none".to_owned(), |t| t.to_string());
                        return refuse(
                            format!("a column of {ty}"),
                            "the driver reads no column of the type",
                        );
                    }
                }
                if !body.domains.is_empty() {
                    return refuse(
                        format!("stage {at}"),
                        "a group key read through the domains of the statistics",
                    );
                }
                let out = stage.columns();
                let numeric = |ty: Type| ty.oid == rupg_types::oid::NUMERIC;
                if let Some(c) =
                    out.iter().find(|c| !readable(c.ty) && !(numeric(c.ty) && aggregated(body)))
                {
                    return refuse(
                        format!("a column of {}", c.ty),
                        "the driver gives no column of the type",
                    );
                }
            }
            Stage::Sort { keys, .. } | Stage::TopN { keys, .. } => {
                if keys.iter().any(|k| !matches!(k.expr.kind, Kind::Column(_))) {
                    return refuse("a sort key".to_owned(), "a sort key that is not a column");
                }
            }
            Stage::Limit { .. } | Stage::Marked { .. } => {}
        }
    }
    // A `numeric` is only given by an aggregate, and read only by the stages that order and cut rows, which compare it as a value.
    for stage in &graph.stages {
        if let Stage::Pipeline(p) = stage
            && let Source::Stage { columns, .. } = &p.source
            && let Some(c) = columns.iter().find(|c| !readable(c.ty))
        {
            return refuse(
                format!("a column of {}", c.ty),
                "a pipeline over a column the driver reads no value of",
            );
        }
    }
    Ok(())
}

/// Whether the body is a hash aggregate's, whose accumulators may finish into a `numeric`.
fn aggregated(body: &Body) -> bool {
    matches!(body.sink, Out::Aggregate(_))
}

/// Runs the query `graph`, with the bodies of `query` on `tiers` and the runtime `rt` the bodies were generated with, over the tables `scan` reads, and gives the rows of its last stage.
///
/// [`check`] says first whether the driver can.
///
/// # Errors
///
/// Whatever the query raises: an overflow, a division by zero, a failed cast, a cancel, or an error from the scan underneath.
pub fn run(
    graph: &Graph,
    query: &Query,
    tiers: &Tiers,
    rt: &mut Rt,
    scan: &mut dyn Scan,
) -> Result<Vec<Vec<Value>>> {
    let mut outputs: Vec<Option<Vec<Vec<Value>>>> = Vec::with_capacity(graph.stages.len());
    for (at, stage) in graph.stages.iter().enumerate() {
        let rows = match stage {
            Stage::Pipeline(p) => {
                let body = query
                    .bodies
                    .get(at)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| Error::internal("a pipeline with no body"))?;
                // An earlier stage's rows are taken before the pipeline starts, because how many there are is what decides its tier.
                let input = match &p.source {
                    Source::Stage { stage, .. } => Some(take(&mut outputs, *stage)?),
                    Source::Scan { .. } | Source::Values { .. } => None,
                };
                let rows = match &p.source {
                    Source::Values { rows, .. } => Some(rows.len()),
                    Source::Stage { .. } => input.as_ref().map(Vec::len),
                    Source::Scan { .. } => None,
                };
                if let Some(f) = tiers.func(&body.func) {
                    tiers.prepare(&query.module, f, rows, !body.probes.is_empty());
                }
                let mut feed = Feed::new(&query.module, tiers, p, body, rt, rows)?;
                let read = feed.read(p.source.columns().len());
                match &p.source {
                    Source::Scan { relation, attnums, columns, .. } => {
                        scan.scan(*relation, attnums, columns, &read, &mut |batch| {
                            feed.push(&batch)
                        })?;
                    }
                    Source::Values { rows, columns } => {
                        feed.push(&Batch::of_read(columns, rows, &read)?)?;
                    }
                    Source::Stage { columns, .. } => {
                        let rows = input.unwrap_or_default();
                        for chunk in rows.chunks(MORSEL_ROWS) {
                            if !feed.push(&Batch::of_read(columns, chunk, &read)?)? {
                                break;
                            }
                        }
                    }
                }
                feed.finish()?
            }
            Stage::Sort { input, keys, .. } => {
                finish::sort(take(&mut outputs, *input)?, keys, None, 0)?
            }
            Stage::TopN { input, keys, count, offset, .. } => {
                finish::sort(take(&mut outputs, *input)?, keys, Some(*count), *offset)?
            }
            Stage::Limit { input, count, offset, .. } => {
                finish::limit(take(&mut outputs, *input)?, *count, *offset)
            }
            Stage::Marked { build, semi, columns, .. } => {
                let Some(Out::Build(b)) =
                    query.bodies.get(*build).and_then(Option::as_ref).map(|b| &b.sink)
                else {
                    return Err(Error::internal("marked rows of no build"));
                };
                let table =
                    rt.join(b.table).ok_or_else(|| Error::internal("a join table is gone"))?;
                finish::kept(&table.kept(*semi), &b.layout, columns)?
            }
        };
        outputs.push(Some(rows));
    }
    Ok(outputs.pop().flatten().unwrap_or_default())
}

/// How many rows of an earlier stage go into one batch, the vector size of the executor.
const MORSEL_ROWS: usize = 1024;

/// The rows of stage `stage`, which only one stage reads.
fn take(outputs: &mut [Option<Vec<Vec<Value>>>], stage: usize) -> Result<Vec<Vec<Value>>> {
    outputs
        .get_mut(stage)
        .and_then(Option::take)
        .ok_or_else(|| Error::internal(format!("the rows of stage {stage} are gone")))
}

/// One cache line of state.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u8; 64]);

/// The output buffers of one result column for one call.
#[derive(Debug, Default)]
struct Buffers {
    values: Vec<u128>,
    valid: Vec<u8>,
}

/// The bytes of a result column's values.
fn values_of(b: &Buffers) -> &[u8] {
    // SAFETY: the buffer was allocated as `u128`s, which any byte pattern is, and is read as the bytes it holds.
    unsafe { std::slice::from_raw_parts(b.values.as_ptr().cast::<u8>(), b.values.len() * 16) }
}

/// Writes the state header, which points at the state's own block.
fn head(state: &mut [Line]) {
    let header = StateHeader::new(state.as_ptr().cast());
    // SAFETY: the first line of the state is 64 bytes aligned to 64, which is the header.
    unsafe { state.as_mut_ptr().cast::<StateHeader>().write(header) };
}

/// The state as bytes.
fn bytes(state: &mut [Line]) -> &mut [u8] {
    // SAFETY: a `Line` is 64 bytes with no padding and any byte pattern is one.
    unsafe { std::slice::from_raw_parts_mut(state.as_mut_ptr().cast::<u8>(), state.len() * 64) }
}

fn put(st: &mut [u8], at: u32, word: u64) {
    st[at as usize..at as usize + 8].copy_from_slice(&word.to_le_bytes());
}

fn word(st: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&st[at..at + 8]);
    u64::from_le_bytes(w)
}

/// One pipeline as it runs: its body, its state and the rows it has given so far.
struct Feed<'a> {
    module: &'a Module,
    tiers: &'a Tiers,
    body: &'a Body,
    steps: Vec<Step>,
    rt: &'a mut Rt,
    /// The body's function in the tiers.
    func: usize,
    /// The version of it for a morsel with no NULL.
    nonull: Option<usize>,
    /// How many rows the pipeline reads, when that is known before it starts.
    rows: Option<usize>,
    state: Vec<Line>,
    /// The folds of an aggregate's accumulators, for the close of an aggregate in runs.
    folds: Vec<merge::Fold>,
    /// The rows of a result so far.
    out: Vec<Vec<Value>>,
    /// The buffers a result body writes its rows into, kept from one batch to the next.
    buffers: Vec<Buffers>,
    /// `0, 1, 2, ...`, the codes a flat text column is read through, as long as the longest batch so far.
    codes: Vec<u32>,
    /// Whether the body said it needs no more rows.
    done: bool,
}

impl<'a> Feed<'a> {
    /// A feed for pipeline `p`, whose generated body is `body`, with its init step run.
    fn new(
        module: &'a Module,
        tiers: &'a Tiers,
        p: &Pipeline,
        body: &'a Body,
        rt: &'a mut Rt,
        rows: Option<usize>,
    ) -> Result<Feed<'a>> {
        let func = tiers
            .func(&body.func)
            .ok_or_else(|| Error::internal(format!("no function {} in the module", body.func)))?;
        let mut state = vec![Line([0; 64]); (body.state as usize).div_ceil(64).max(1)];
        // Init. With one worker the local state is the shared state, so the header points at its own block, which never moves because the vector is never grown.
        head(&mut state);
        if let Out::Aggregate(g) = &body.sink
            && let Some(at) = g.row
        {
            // The one group of an aggregate with no groups was made with the table, and rows never move, so its address is written once.
            let table = rt.table(g.table).ok_or_else(|| gone(g.table))?;
            put(bytes(&mut state), at, table.address(0) as u64);
        }
        for probe in &body.probes {
            // The build ran and was finished before this pipeline started, and its table does not move after that.
            let table = rt
                .join(probe.table)
                .filter(|t| t.is_finished())
                .ok_or_else(|| Error::internal("a probe of a join table that is not built"))?;
            let published = table.published();
            let st = bytes(&mut state);
            put(st, probe.directory, published.directory as u64);
            put(st, probe.shift, published.shift);
            put(st, probe.tags, published.tags as u64);
        }
        let folds = match &body.sink {
            Out::Aggregate(g) if g.runs.is_some() => merge::folds(g)?,
            _ => Vec::new(),
        };
        Ok(Feed {
            module,
            tiers,
            body,
            steps: p.steps(),
            rt,
            func,
            nonull: body.nonull.as_deref().and_then(|name| tiers.func(name)),
            rows,
            state,
            folds,
            out: Vec::new(),
            buffers: Vec::new(),
            codes: Vec::new(),
            done: false,
        })
    }

    /// Which of the source's `n` columns the body reads, itself or through a `LIKE`.
    fn read(&self, n: usize) -> Vec<bool> {
        let mut read = vec![false; n];
        for &c in self.body.reads.iter().chain(self.body.likes.iter().map(|m| &m.column)) {
            if let Some(r) = read.get_mut(c) {
                *r = true;
            }
        }
        read
    }

    /// Runs the body over `batch`, and says whether the pipeline takes more rows.
    fn push(&mut self, batch: &Batch) -> Result<bool> {
        if self.done {
            return Ok(false);
        }
        let rows = batch.rows;
        if rows == 0 {
            return Ok(true);
        }
        let column = |c: usize| -> Result<&Encoded> {
            batch.columns.get(c).filter(|e| e.rows == rows).ok_or_else(|| {
                Error::internal(format!("a batch with no column {c} of {rows} rows"))
            })
        };
        if self.codes.len() < rows {
            let top = u32::try_from(rows).map_err(|_| Error::internal("a batch too long"))?;
            self.codes = (0..top).collect();
        }
        let each = self.codes.as_ptr();
        let mut clean = true;
        let mut cols: Vec<Col> =
            Vec::with_capacity(self.body.reads.len() + self.body.likes.len() + 1);
        for &c in &self.body.reads {
            let e = column(c)?;
            clean &= e.clean;
            let codes =
                if e.text || self.body.coded.contains(&c) { each } else { std::ptr::null() };
            cols.push(Col { values: e.values.as_ptr().cast(), valid: e.valid.as_ptr(), codes });
        }
        // Each `LIKE` the body reads as a column is answered here for the whole morsel, a byte a row.
        let mut answers = Vec::with_capacity(self.body.likes.len());
        for m in &self.body.likes {
            let e = column(m.column)?;
            clean &= e.clean;
            let like =
                self.rt.like(m.like).ok_or_else(|| Error::internal("a LIKE pattern is gone"))?;
            let answer: Vec<u8> = (0..rows)
                .map(|i| {
                    let header = e.header(i);
                    // SAFETY: the header was made by `Encoded::push` over bytes the column keeps.
                    u8::from(e.is_valid(i) && like.matches(unsafe { text::bytes(&header) }))
                })
                .collect();
            answers.push((answer, e.valid.as_ptr()));
        }
        for (answer, valid) in &answers {
            cols.push(Col { values: answer.as_ptr(), valid: *valid, codes: each });
        }
        // A body that reads its group rows by the key's codes has one more column for their array, which a null address leaves unused, as no column here is coded.
        if self.body.keyed.is_some() {
            cols.push(Col {
                values: std::ptr::null(),
                valid: std::ptr::null(),
                codes: std::ptr::null(),
            });
        }
        let mut morsel = Morsel {
            source: 0,
            chunk: 0,
            begin: 0,
            end: u32::try_from(rows)
                .map_err(|_| Error::internal("a batch too long for a morsel"))?,
            seq: 0,
            enc: 0,
            flags: 0,
            cols: cols.as_ptr(),
        };
        let sink = match &self.body.sink {
            Out::Result { .. } => tier::Sink::Result,
            Out::Aggregate(_) => tier::Sink::Aggregate,
            // The marks a probe sets are live in the table it probes, as a build's entries are.
            Out::Build(_) | Out::Mark => tier::Sink::Build,
        };
        // A morsel with no NULL in what the body reads runs the version that checks none, which is a guard checked before the call and so never sent back.
        let mut f = self.func;
        if let Some(v) = self.nonull
            && clean
            && self.tiers.speculate(v)
        {
            self.tiers.prepare(self.module, v, self.rows, !self.body.probes.is_empty());
            f = v;
        }
        let start = self.tiers.start(f);
        let before = match &self.body.sink {
            Out::Aggregate(g) if g.runs.is_some() => {
                self.rt.table(g.table).map_or(0, GroupTable::len)
            }
            _ => 0,
        };
        let mut room = rows;
        let mut buffers = std::mem::take(&mut self.buffers);
        'attempt: loop {
            if let Out::Result { count, columns, capacity } = &self.body.sink {
                let st = bytes(&mut self.state);
                put(st, *count, 0);
                if let Some(at) = capacity {
                    put(st, *at, room as u64);
                }
                buffers.resize_with(buffers.len().max(columns.len()), Buffers::default);
                for (slot, b) in columns.iter().zip(buffers.iter_mut()) {
                    let values = (room * slot.ty.bytes() as usize).div_ceil(16);
                    if b.values.len() < values {
                        b.values.resize(values, 0);
                    }
                    if b.valid.len() < room {
                        b.valid.resize(room, 0);
                    }
                    put(st, slot.values, b.values.as_mut_ptr() as u64);
                    put(st, slot.valid, b.valid.as_mut_ptr() as u64);
                }
            }
            let st = self.state.as_mut_ptr().cast::<u8>();
            // A batch is cut into morsels of `split` rows when that is set, all of them run on the same version, and a result's rows go on where the morsel before left them.
            let step = match self.tiers.split() {
                0 => rows,
                n => n.min(rows),
            };
            let mut begin = 0;
            while begin < rows {
                let end = (begin + step).min(rows);
                (morsel.begin, morsel.end) = (begin as u32, end as u32);
                begin = end;
                // The tier is picked once per morsel, so a switch only ever happens between two.
                let native = self.tiers.morsel(f, sink);
                loop {
                    let status =
                        self.tiers.call(native, f, st, (&raw const morsel).cast(), self.rt);
                    match Status(status).kind() {
                        StatusKind::Ok => break,
                        // The body saved where it got to in the header's cursor and picks up there.
                        StatusKind::Yield => {}
                        StatusKind::Done => {
                            self.done = true;
                            break 'attempt;
                        }
                        // Only a result sink past a probe asks, and only when its buffers are full.
                        StatusKind::NeedMemory
                            if matches!(self.body.sink, Out::Result { capacity: Some(_), .. }) =>
                        {
                            room = room.checked_mul(2).ok_or_else(|| {
                                Error::internal("a join made more rows than memory holds")
                            })?;
                            continue 'attempt;
                        }
                        // A guard failed inside the body. Only a result sink starts the morsel over from nothing, so only there is it run again on the guard's fallback.
                        StatusKind::Deopt if matches!(self.body.sink, Out::Result { .. }) => {
                            let fallback = usize::try_from(Status(status).payload())
                                .ok()
                                .and_then(|at| self.module.guards.get(at))
                                .and_then(|g| self.tiers.func(&g.fallback))
                                .filter(|&back| back != f)
                                .ok_or_else(|| raised(self.module, Status(status), self.rt))?;
                            self.tiers.deopt(f);
                            self.tiers.prepare(self.module, fallback, self.rows, false);
                            f = fallback;
                            continue 'attempt;
                        }
                        _ => return Err(raised(self.module, Status(status), self.rt)),
                    }
                }
            }
            break;
        }
        drop(answers);
        drop(cols);
        if let Out::Aggregate(g) = &self.body.sink {
            self.appended(g);
            if let Some(at) = g.runs {
                runs_closed(g, at, bytes(&mut self.state), self.rt, before)?;
            }
        }
        if let Some(start) = start {
            self.tiers.ran(self.module, f, rows, start);
        }
        if let Out::Result { count, columns, .. } = &self.body.sink {
            let n = word(bytes(&mut self.state), *count as usize);
            let n = usize::try_from(n).unwrap_or(usize::MAX).min(room);
            self.out.reserve(n);
            for i in 0..n {
                let mut row = Vec::with_capacity(columns.len());
                for (slot, b) in columns.iter().zip(&buffers) {
                    let w = slot.ty.bytes() as usize;
                    row.push(if b.valid[i] != 0 {
                        value(slot.logical, &values_of(b)[i * w..(i + 1) * w])?
                    } else {
                        Value::Null
                    });
                }
                self.out.push(row);
            }
        }
        self.buffers = buffers;
        Ok(!self.done)
    }

    /// Takes in what the body appended outside the table for an aggregate: the pairs of a `count(DISTINCT)`, and the rows it appended to the table's lanes without looking, which count as its groups.
    fn appended(&mut self, g: &Grouping) {
        let st = bytes(&mut self.state);
        for acc in &g.accs {
            if let AccOp::Distinct(h, Some(at)) = acc.op {
                self.rt.take_appended(h, &mut st[at as usize..at as usize + 16]);
            }
        }
        if let Some(at) = g.probe {
            let at = at as usize;
            if word(st, at + 32) != 0
                && let Some(table) = self.rt.table_mut(g.table)
            {
                table.made_blind(word(st, at + 40));
                let [tails, left] = table.blind_words();
                st[at + 32..at + 40].copy_from_slice(&tails.to_le_bytes());
                st[at + 40..at + 48].copy_from_slice(&left.to_le_bytes());
            }
        }
    }

    /// Runs the steps after the body once every batch has been pushed, and gives the rows the pipeline produced.
    fn finish(self) -> Result<Vec<Vec<Value>>> {
        let mut out = self.out;
        for step in &self.steps {
            match step {
                // The accumulators of an aggregate with no groups are the one row of its table, so there is nothing kept aside to flush.
                Step::Init | Step::Body | Step::LocalFin => {}
                Step::Merge => return Err(Error::internal("a merge step with one worker")),
                Step::Finalize => match &self.body.sink {
                    Out::Aggregate(g) => {
                        if g.runs.is_some() {
                            let table = self.rt.table_mut(g.table).ok_or_else(|| gone(g.table))?;
                            let layout = table.layout().clone();
                            let mine = std::mem::replace(table, GroupTable::new(layout.clone()));
                            let fold = |d: &mut [u8], s: &[u8]| merge::fold(&self.folds, d, s);
                            *table = GroupTable::close(layout, vec![mine], fold);
                        }
                        let table = self.rt.table(g.table).ok_or_else(|| gone(g.table))?;
                        let sets = finish::sets(self.rt, g)?;
                        out = finish::groups(table, &sets, g)?;
                    }
                    // The build publishes its table to the probes and produces no rows.
                    Out::Build(b) => {
                        self.rt.finish_join(b.table)?;
                        out = Vec::new();
                    }
                    Out::Result { .. } | Out::Mark => {}
                },
            }
        }
        Ok(out)
    }
}

/// Gives the rows a morsel of runs took their group ids, and marks the groups that may share their key with another morsel's: its first and its last, since a run may go on past either end, and every one of them when runs promised ascending were not.
fn runs_closed(g: &Grouping, at: u32, state: &[u8], rt: &mut Rt, before: usize) -> Result<()> {
    let next = word(state, at as usize);
    let table = rt.table_mut(g.table).ok_or_else(|| gone(g.table))?;
    table.claim(next as usize);
    let after = table.len();
    if after == before {
        return Ok(());
    }
    table.edge(before);
    table.edge(after - 1);
    if g.ascending && !ascending(g, table, before, after) {
        for gid in before + 1..after - 1 {
            table.edge(gid);
        }
    }
    Ok(())
}

/// Whether the keys of groups `from..to` of `table` go up, which a NULL key never does.
fn ascending(g: &Grouping, table: &GroupTable, from: usize, to: usize) -> bool {
    let Some((k, ty)) = g.keys.first() else { return true };
    let (o, w) = (8 + k.offset as usize, k.width as usize);
    let float = matches!(ty.oid, rupg_types::oid::FLOAT4 | rupg_types::oid::FLOAT8);
    let unsigned = crate::codegen::unsigned(*ty);
    let key = |gid: usize| {
        let row = table.row(gid);
        if row[8 + k.null() as usize] != 0 {
            return None;
        }
        let mut b = [0u8; 16];
        b[..w].copy_from_slice(&row[o..o + w]);
        // Sign extended from the key's width, but for an unsigned one.
        let shift = 128 - 8 * w as u32;
        let v = i128::from_le_bytes(b) << shift;
        Some(if unsigned { (v as u128 >> shift) as i128 } else { v >> shift })
    };
    let mut last = None;
    for gid in from..to {
        let Some(v) = key(gid) else { return false };
        if let Some(l) = last {
            let up = if float {
                let f = |x: i128| match w {
                    4 => f64::from(f32::from_bits(x as u32)),
                    _ => f64::from_bits(x as u64),
                };
                f(l) < f(v)
            } else {
                l < v
            };
            if !up {
                return false;
            }
        }
        last = Some(v);
    }
    true
}

fn gone(table: u64) -> Error {
    Error::internal(format!("the table {table} is not in the runtime"))
}

/// The error a status that stops the pipeline stands for, with the SQLSTATE PostgreSQL raises it with.
fn raised(module: &Module, s: Status, rt: &mut Rt) -> Error {
    match s.kind() {
        StatusKind::Cancelled => {
            Error::new(SqlState::QUERY_CANCELED, "canceling statement due to user request")
        }
        StatusKind::Error if s.payload() == RUNTIME_ERROR => rt
            .take_error()
            .unwrap_or_else(|| Error::internal("the runtime failed and did not say why")),
        StatusKind::Error => {
            let Some(site) = usize::try_from(s.payload()).ok().and_then(|at| module.errors.get(at))
            else {
                return Error::internal(format!("status {s:?} names no error site"));
            };
            let state = match site.kind {
                ErrorKind::Overflow | ErrorKind::OutOfRange => SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                ErrorKind::DivideByZero => SqlState::DIVISION_BY_ZERO,
                ErrorKind::Conversion => SqlState::INVALID_TEXT_REPRESENTATION,
                ErrorKind::Cancel => SqlState::QUERY_CANCELED,
                ErrorKind::Internal => return Error::internal(site.text.clone()),
            };
            Error::new(state, site.text.clone())
        }
        // A guard that fails in a body that cannot start its morsel over, or names no fallback, and a body that grows that is not a result sink past a probe, are bugs.
        StatusKind::Deopt => Error::internal(format!("a pipeline deoptimized at {s:?}")),
        StatusKind::NeedMemory => Error::internal(format!("a pipeline asked for memory, {s:?}")),
        _ => Error::internal(format!("a pipeline returned status {s:?}")),
    }
}

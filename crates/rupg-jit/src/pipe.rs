// Lifted from rudb crates/rudb-qc-pipe/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the module `pipe` of rupg-jit, over the tree of `crate::plan`. A scan names its table by OID and its columns by attribute number, as the executor hands them over, and an aggregate keeps its `pg_proc` OID. The fetch stage, which reads whole rows back by ordinal for the late materialization of rudb's planner, is not lifted, since the columnar executor hands the compiled code only the columns a query reads. The tests build the trees by hand instead of parsing rudb plans.

//! Pipelines: a [`Rel`] tree cut into the loops the generator compiles.
//!
//! [`split`] cuts a [`Rel`] tree at its breakers and returns the stages in the order they run. A
//! [`Pipeline`] is a source, the operators that apply to it in order and a sink. The filters and
//! projections between a source and the next breaker are folded into it by substitution, so a
//! filter above a projection becomes a filter over the source's columns and the sink's expressions
//! read the source directly. Nothing between two breakers is left as an operator of its own, which
//! is the point of compiling a pipeline: one loop over the rows of a morsel, with no chunk in
//! between.
//!
//! A hash join is the fused join of section 10.5 of `spec/compiler/10-joins.md`. Its build side is
//! a pipeline of its own that ends in a [`Sink::Build`], and the side that probes carries on as the
//! same pipeline with an [`Op::Probe`] in it, which looks each row up and runs the rest of the
//! pipeline once per match. The columns a match brings are numbered after the source's, so an
//! expression past a probe reads them like any other column.
//!
//! The breakers are the aggregate, which is a sink, and the sort, top N and limit, which in
//! C1 are stages of their own over the rows the pipeline before them produced. They are few rows
//! or cheap work in ClickBench, and a compiled top N is part of C3.
//!
//! The stages form the graph of section 5.2: [`Graph::edges`] lists which stage waits for which,
//! and every edge in C1 is a [`EdgeKind::Finalize`] edge, because a stage only ever reads what an
//! earlier one finished, a join table included. Each pipeline runs as the steps of section 5.4,
//! which [`Pipeline::steps`] derives from the kinds of its state slots, and every step starts from
//! a state whose first line is a [`StateHeader`]. C1 runs every pipeline on one worker, so no
//! pipeline has a merge step and its local state is its shared state.

use std::fmt;

use rupg_types::Value;

use crate::plan::{Aggregate, BuildSide, Column, Expr, JoinType, Key, Kind, Rel, Runs};

pub use crate::ir::status::Status;
pub use crate::rt::abi::StateHeader;

/// Where a pipeline's rows come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A table, whose columns the executor hands over a morsel at a time.
    Scan {
        /// The OID of the table.
        relation: u32,
        /// The table, for messages.
        table: String,
        /// The attribute number of each column.
        attnums: Vec<i16>,
        /// The columns.
        columns: Vec<Column>,
    },
    /// Literal rows.
    Values {
        /// The rows.
        rows: Vec<Vec<Value>>,
        /// The columns.
        columns: Vec<Column>,
    },
    /// The rows an earlier stage produced.
    Stage {
        /// The stage.
        stage: usize,
        /// Its columns.
        columns: Vec<Column>,
    },
}

impl Source {
    /// The columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Source::Scan { columns, .. }
            | Source::Values { columns, .. }
            | Source::Stage { columns, .. } => columns,
        }
    }
}

/// Where a pipeline's rows go.
#[derive(Clone, Debug, PartialEq)]
pub enum Sink {
    /// Rows out, one per input row that passed the filters.
    Result {
        /// The output columns, over the source's.
        exprs: Vec<Expr>,
        /// Their names and types.
        columns: Vec<Column>,
    },
    /// The build side of a hash join: every row whose keys are all valid goes into the table.
    Build {
        /// The key expressions, over the source's columns.
        keys: Vec<Expr>,
        /// The columns a match hands the probe, over the source's columns.
        payload: Vec<Expr>,
        /// Their names and types.
        columns: Vec<Column>,
        /// For a semi or anti join whose left side builds, which one. Each entry gets a mark the
        /// probe sets, and for an anti join a row with a null key goes in too, since it is kept.
        marked: Option<JoinType>,
    },
    /// Nothing out: every row went through a probe that marks the entries it matches.
    Mark,
    /// A hash aggregate. The output is the groups and then the aggregates.
    Aggregate {
        /// The group expressions, over the source's columns.
        groups: Vec<Expr>,
        /// The aggregate calls, over the source's columns.
        aggregates: Vec<Aggregate>,
        /// Their names and types.
        columns: Vec<Column>,
        /// Whether the one key arrives in runs.
        runs: Option<Runs>,
    },
}

/// A lookup of each row in a join table a build pipeline filled.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// The stage whose [`Sink::Build`] filled the table.
    pub build: usize,
    /// The key expressions, over the pipeline's columns so far, in the order of the build's.
    pub keys: Vec<Expr>,
    /// The number the first payload column gets. The payload columns come after the source's
    /// and after those of every probe before this one.
    pub first: usize,
    /// The payload columns.
    pub columns: Vec<Column>,
    /// Which rows go on. After an inner probe what follows runs once per match. After a semi
    /// probe it runs once for a row with a match, and after an anti probe once for a row with
    /// none, and neither reads the payload.
    pub kind: JoinType,
    /// For a semi or an anti probe, what a match has to pass as well to count, over the columns
    /// so far and the payload. An inner probe's are filters after it.
    pub residual: Vec<Expr>,
    /// Whether the probe marks every entry it matches instead, for a semi or anti join whose left
    /// side builds. It is the last operator, before a [`Sink::Mark`].
    pub marks: bool,
}

/// One operator between a pipeline's source and its sink.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// A predicate the row has to pass.
    Filter(Expr),
    /// A join probe. What follows runs once per match, or once per row for a semi or an anti
    /// probe.
    Probe(Probe),
}

/// One compiled loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Pipeline {
    /// Where the rows come from.
    pub source: Source,
    /// The operators a row goes through, in order. An expression in one reads the source's
    /// columns and the payload of the probes before it.
    pub ops: Vec<Op>,
    /// Where the rows go.
    pub sink: Sink,
}

/// The kind of a state slot, from the table in section 5.5.1. Only the kinds C1 fills are here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// The output rows of a result sink.
    ResultSink,
    /// A hash aggregate's table.
    AggTable,
    /// The accumulators of an aggregate with no groups.
    ScalarAcc,
    /// The join table a build fills.
    HtBuild,
    /// What a probe reads of a join table another pipeline built.
    HtProbe,
}

/// One of the steps a pipeline runs as, per section 5.4. Every step has the same signature, a
/// state and a morsel in and a [`Status`] out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Once per worker before its first morsel: set up the local state.
    Init,
    /// Once per morsel: the fused loop.
    Body,
    /// Once per worker after its last morsel: flush what the worker kept to itself.
    LocalFin,
    /// Once per worker: fold its local state into the shared state.
    Merge,
    /// Once per pipeline: build and publish what later stages read.
    Finalize,
}

/// How many workers run a pipeline. C1 has one runtime per query and runs every pipeline on one.
pub const DOP: usize = 1;

impl Pipeline {
    /// The state slots the pipeline writes, in the order they are laid out.
    #[must_use]
    pub fn slots(&self) -> Vec<SlotKind> {
        // A marking probe writes into the table it probes and has no state of its own.
        let mut slots: Vec<SlotKind> = match &self.sink {
            Sink::Result { .. } => vec![SlotKind::ResultSink],
            Sink::Build { .. } => vec![SlotKind::HtBuild],
            Sink::Aggregate { groups, .. } if groups.is_empty() => vec![SlotKind::ScalarAcc],
            Sink::Aggregate { .. } => vec![SlotKind::AggTable],
            Sink::Mark => Vec::new(),
        };
        slots.extend(self.probes().map(|_| SlotKind::HtProbe));
        slots
    }

    /// The probes, in order.
    pub fn probes(&self) -> impl Iterator<Item = &Probe> {
        self.ops.iter().filter_map(|op| match op {
            Op::Probe(p) => Some(p),
            Op::Filter(_) => None,
        })
    }

    /// The source's columns and then the payload of every probe: what an expression in the
    /// sink reads.
    #[must_use]
    pub fn columns(&self) -> Vec<Column> {
        let mut out = self.source.columns().to_vec();
        for p in self.probes() {
            out.extend(p.columns.iter().cloned());
        }
        out
    }

    /// The steps the pipeline runs, in order, from its slots and [`DOP`].
    ///
    /// Every pipeline has an init step, which writes the state header, and a body. A slot that
    /// keeps partials needs a local finish and one that others read needs a finalize. A merge
    /// is only there with more than one worker.
    #[must_use]
    pub fn steps(&self) -> Vec<Step> {
        let slots = self.slots();
        let mut steps = vec![Step::Init, Step::Body];
        if slots.contains(&SlotKind::ScalarAcc) {
            steps.push(Step::LocalFin);
        }
        if DOP > 1 {
            steps.push(Step::Merge);
        }
        if slots
            .iter()
            .any(|s| matches!(s, SlotKind::AggTable | SlotKind::ScalarAcc | SlotKind::HtBuild))
        {
            steps.push(Step::Finalize);
        }
        steps
    }
}

/// Why one stage waits for another, per section 5.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// The consumer reads what the producer built: a table, a buffer of rows, a sorted run.
    Finalize,
    /// The consumer's scan applies a filter the producer publishes.
    FilterPublish,
    /// The consumer has to see its rows after the producer's.
    Order,
}

/// A dependency between two stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The stage that has to finish first.
    pub from: usize,
    /// The stage that waits for it.
    pub to: usize,
    /// Why.
    pub kind: EdgeKind,
}

/// One step of a query.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// A compiled pipeline.
    Pipeline(Pipeline),
    /// All of an earlier stage's rows in order.
    Sort {
        /// The stage.
        input: usize,
        /// The keys, over its columns.
        keys: Vec<Key>,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// The first rows of an earlier stage in order.
    TopN {
        /// The stage.
        input: usize,
        /// The keys, over its columns.
        keys: Vec<Key>,
        /// How many rows.
        count: u64,
        /// How many to skip first.
        offset: u64,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// Some of an earlier stage's rows.
    Limit {
        /// The stage.
        input: usize,
        /// How many rows, all when `None`.
        count: Option<u64>,
        /// How many to skip first.
        offset: u64,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// The rows of a join table a semi or anti join whose left side builds keeps, once every
    /// probe marked the entries it matched.
    Marked {
        /// The stage that built the table.
        build: usize,
        /// The stage that probed it.
        probe: usize,
        /// Whether the marked rows are kept, for a semi join, or the rest, for an anti join.
        semi: bool,
        /// The build side's columns.
        columns: Vec<Column>,
    },
}

impl Stage {
    /// The columns the stage produces.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Stage::Pipeline(p) => match &p.sink {
                Sink::Result { columns, .. }
                | Sink::Build { columns, .. }
                | Sink::Aggregate { columns, .. } => columns,
                Sink::Mark => &[],
            },
            Stage::Sort { columns, .. }
            | Stage::TopN { columns, .. }
            | Stage::Limit { columns, .. }
            | Stage::Marked { columns, .. } => columns,
        }
    }

    /// Gives the columns the stage produces the names in `columns`, which match them in number
    /// and type.
    fn rename(&mut self, columns: Vec<Column>) {
        let own = match self {
            Stage::Pipeline(p) => match &mut p.sink {
                Sink::Result { columns, .. }
                | Sink::Build { columns, .. }
                | Sink::Aggregate { columns, .. } => columns,
                Sink::Mark => return,
            },
            Stage::Sort { columns, .. }
            | Stage::TopN { columns, .. }
            | Stage::Limit { columns, .. }
            | Stage::Marked { columns, .. } => columns,
        };
        for (own, new) in own.iter_mut().zip(columns) {
            own.name = new.name;
        }
    }

    /// The stages this one reads: the one its rows come from and the builds its probes read.
    #[must_use]
    pub fn inputs(&self) -> Vec<usize> {
        match self {
            Stage::Pipeline(p) => {
                let mut out = match &p.source {
                    Source::Stage { stage, .. } => vec![*stage],
                    Source::Scan { .. } | Source::Values { .. } => Vec::new(),
                };
                out.extend(p.probes().map(|probe| probe.build));
                out
            }
            Stage::Sort { input, .. } | Stage::TopN { input, .. } | Stage::Limit { input, .. } => {
                vec![*input]
            }
            Stage::Marked { build, probe, .. } => vec![*build, *probe],
        }
    }
}

/// A query as stages. Each stage reads only stages before it, and the last one is the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Graph {
    /// The stages in the order they run.
    pub stages: Vec<Stage>,
}

impl Graph {
    /// The columns of the answer.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        self.stages.last().map_or(&[], Stage::columns)
    }

    /// The pipelines, with their stage numbers.
    pub fn pipelines(&self) -> impl Iterator<Item = (usize, &Pipeline)> {
        self.stages.iter().enumerate().filter_map(|(i, s)| match s {
            Stage::Pipeline(p) => Some((i, p)),
            _ => None,
        })
    }

    /// The edges between stages, by the stage that waits.
    #[must_use]
    pub fn edges(&self) -> Vec<Edge> {
        let mut edges = Vec::new();
        for (to, stage) in self.stages.iter().enumerate() {
            for from in stage.inputs() {
                edges.push(Edge { from, to, kind: EdgeKind::Finalize });
            }
        }
        edges
    }
}

/// Cuts a physical plan into stages.
#[must_use]
pub fn split(rel: &Rel) -> Graph {
    let mut g = Graph { stages: Vec::new() };
    let open = g.open(rel);
    g.close(open);
    g
}

/// A pipeline still being built: a source and what has been folded into it so far.
struct Open {
    source: Source,
    ops: Vec<Op>,
    /// The current columns, as expressions over the source's and the probes' payload.
    exprs: Vec<Expr>,
    columns: Vec<Column>,
    /// How many columns the source and the probes so far give.
    width: usize,
}

impl Open {
    fn over(source: Source) -> Open {
        let columns = source.columns().to_vec();
        let exprs = columns.iter().enumerate().map(|(i, c)| Expr::column(i, c.ty)).collect();
        let width = columns.len();
        Open { source, ops: Vec::new(), exprs, columns, width }
    }

    fn is_identity(&self) -> bool {
        self.exprs.len() == self.source.columns().len()
            && self.exprs.iter().enumerate().all(|(i, e)| e.kind == Kind::Column(i))
    }
}

impl Graph {
    fn push(&mut self, stage: Stage) -> Open {
        let columns = stage.columns().to_vec();
        self.stages.push(stage);
        Open::over(Source::Stage { stage: self.stages.len() - 1, columns })
    }

    /// Ends a pipeline with a result sink, and returns its stage.
    fn close(&mut self, open: Open) -> usize {
        if let Source::Stage { stage, .. } = open.source
            && open.ops.is_empty()
            && open.is_identity()
        {
            // A projection that only renames, like the `AS revenue` over an aggregate, passes the
            // stage through but keeps its names.
            self.stages[stage].rename(open.columns);
            return stage;
        }
        let sink = Sink::Result { exprs: open.exprs, columns: open.columns };
        self.stages.push(Stage::Pipeline(Pipeline { source: open.source, ops: open.ops, sink }));
        self.stages.len() - 1
    }

    fn open(&mut self, rel: &Rel) -> Open {
        match rel {
            Rel::Scan { relation, table, attnums, columns } => Open::over(Source::Scan {
                relation: *relation,
                table: table.clone(),
                attnums: attnums.clone(),
                columns: columns.clone(),
            }),
            Rel::Values { rows, columns } => {
                Open::over(Source::Values { rows: rows.clone(), columns: columns.clone() })
            }
            Rel::Filter { input, predicate } => {
                let mut open = self.open(input);
                let predicate = predicate.substitute(&open.exprs);
                filters(predicate, &mut open.ops);
                open
            }
            Rel::Project { input, exprs, columns } => {
                let mut open = self.open(input);
                open.exprs = exprs.iter().map(|e| e.substitute(&open.exprs)).collect();
                open.columns = columns.clone();
                open
            }
            Rel::Aggregate { input, groups, aggregates, columns, runs } => {
                let open = self.open(input);
                let groups = groups.iter().map(|g| g.substitute(&open.exprs)).collect();
                let aggregates = aggregates
                    .iter()
                    .map(|a| Aggregate {
                        name: a.name,
                        oid: a.oid,
                        args: a.args.iter().map(|x| x.substitute(&open.exprs)).collect(),
                        distinct: a.distinct,
                        filter: a.filter.as_ref().map(|f| f.substitute(&open.exprs)),
                        ty: a.ty,
                    })
                    .collect();
                let sink =
                    Sink::Aggregate { groups, aggregates, columns: columns.clone(), runs: *runs };
                let p = Pipeline { source: open.source, ops: open.ops, sink };
                self.push(Stage::Pipeline(p))
            }
            Rel::Sort { input, keys } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::Sort { input, keys: keys.clone(), columns })
            }
            Rel::TopN { input, keys, count, offset } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::TopN {
                    input,
                    keys: keys.clone(),
                    count: *count,
                    offset: *offset,
                    columns,
                })
            }
            Rel::Limit { input, count, offset } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::Limit { input, count: *count, offset: *offset, columns })
            }
            Rel::Join { left, right, build, kind, keys, residual: residual_of, columns } => {
                let (built, probing) = match build {
                    BuildSide::Right => (right, left),
                    BuildSide::Left => (left, right),
                };
                let side = |k: &(Expr, Expr)| match build {
                    BuildSide::Right => (k.1.clone(), k.0.clone()),
                    BuildSide::Left => (k.0.clone(), k.1.clone()),
                };
                let (build_keys, probe_keys): (Vec<Expr>, Vec<Expr>) =
                    keys.iter().map(side).unzip();
                let mut b = self.open(built);
                let inner = *kind == JoinType::Inner;
                // The probe marks the entries it matches, and the rows come out of the table.
                let marks = !inner && *build == BuildSide::Left;
                // A semi or anti probe only reads the payload to test the residual.
                if !inner && !marks && residual_of.is_empty() {
                    b.columns.clear();
                }
                let sink = Sink::Build {
                    keys: build_keys.iter().map(|k| k.substitute(&b.exprs)).collect(),
                    payload: if b.columns.is_empty() { Vec::new() } else { b.exprs },
                    columns: b.columns.clone(),
                    marked: marks.then_some(*kind),
                };
                self.stages.push(Stage::Pipeline(Pipeline { source: b.source, ops: b.ops, sink }));
                let stage = self.stages.len() - 1;
                let mut open = self.open(probing);
                let first = open.width;
                open.width += b.columns.len();
                let payload: Vec<Expr> = b
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(j, c)| Expr::column(first + j, c.ty))
                    .collect();
                // The residual is over the left columns and then the right ones.
                let mut residual = Vec::new();
                if !inner {
                    let both: Vec<Expr> = match build {
                        BuildSide::Right => open.exprs.iter().chain(&payload).cloned().collect(),
                        BuildSide::Left => payload.iter().chain(&open.exprs).cloned().collect(),
                    };
                    for r in residual_of {
                        conjuncts(r.substitute(&both), &mut residual);
                    }
                }
                open.ops.push(Op::Probe(Probe {
                    build: stage,
                    keys: probe_keys.iter().map(|k| k.substitute(&open.exprs)).collect(),
                    first,
                    columns: b.columns,
                    kind: *kind,
                    residual,
                    marks,
                }));
                if marks {
                    let source = open.source;
                    let ops = open.ops;
                    let sink = Sink::Mark;
                    self.stages.push(Stage::Pipeline(Pipeline { source, ops, sink }));
                    let probe = self.stages.len() - 1;
                    let semi = *kind == JoinType::Semi;
                    let columns = columns.clone();
                    return self.push(Stage::Marked { build: stage, probe, semi, columns });
                }
                open.columns = columns.clone();
                if !inner {
                    return open;
                }
                open.exprs = match build {
                    BuildSide::Right => open.exprs.into_iter().chain(payload).collect(),
                    BuildSide::Left => payload.into_iter().chain(open.exprs).collect(),
                };
                for r in residual_of {
                    let r = r.substitute(&open.exprs);
                    filters(r, &mut open.ops);
                }
                open
            }
        }
    }
}

/// Adds a filter per conjunct of `e`.
fn filters(e: Expr, out: &mut Vec<Op>) {
    let mut each = Vec::new();
    conjuncts(e, &mut each);
    out.extend(each.into_iter().map(Op::Filter));
}

/// Adds each conjunct of `e` to `out`.
fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e.kind {
        Kind::And(children) => children.into_iter().for_each(|c| conjuncts(c, out)),
        kind => out.push(Expr { kind, ty: e.ty }),
    }
}

impl fmt::Display for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, s) in self.stages.iter().enumerate() {
            match s {
                Stage::Pipeline(p) => {
                    let from = match &p.source {
                        Source::Scan { table, .. } => format!("scan {table}"),
                        Source::Values { rows, .. } => format!("{} literal rows", rows.len()),
                        Source::Stage { stage, .. } => format!("stage {stage}"),
                    };
                    let to = match &p.sink {
                        Sink::Result { exprs, .. } => format!("{} columns out", exprs.len()),
                        Sink::Build { keys, payload, .. } => format!(
                            "join table by {} keys holding {} columns",
                            keys.len(),
                            payload.len()
                        ),
                        Sink::Aggregate { groups, aggregates, runs, .. } => {
                            let runs = match runs {
                                Some(Runs::Ascending) => " arriving ascending",
                                Some(Runs::Grouped) => " arriving grouped",
                                None => "",
                            };
                            format!(
                                "aggregate by {} keys{runs} into {} accumulators",
                                groups.len(),
                                aggregates.len()
                            )
                        }
                        Sink::Mark => "nothing out".to_owned(),
                    };
                    let filters = p.ops.iter().filter(|op| matches!(op, Op::Filter(_))).count();
                    let mut ops = vec![format!("{filters} filters")];
                    for probe in p.probes() {
                        let what = match (probe.kind, probe.marks) {
                            (_, true) => "marking ".to_owned(),
                            (JoinType::Inner, false) => String::new(),
                            (kind, false) => format!("{} ", kind.keyword().to_lowercase()),
                        };
                        ops.push(format!("{what}probe stage {}", probe.build));
                    }
                    writeln!(f, "stage {i}: pipeline from {from}, {}, {to}", ops.join(", "))?;
                }
                Stage::Sort { input, keys, .. } => {
                    writeln!(f, "stage {i}: sort stage {input} by {} keys", keys.len())?
                }
                Stage::TopN { input, count, offset, .. } => {
                    writeln!(f, "stage {i}: top {count} offset {offset} of stage {input}")?;
                }
                Stage::Limit { input, count, offset, .. } => match count {
                    Some(n) => {
                        writeln!(f, "stage {i}: limit {n} offset {offset} of stage {input}")?
                    }
                    None => writeln!(f, "stage {i}: offset {offset} of stage {input}")?,
                },
                Stage::Marked { build, probe, semi, .. } => {
                    let which = if *semi { "marked" } else { "unmarked" };
                    writeln!(f, "stage {i}: the {which} rows of stage {build} after stage {probe}")?
                }
            }
        }
        let edges = self.edges();
        if !edges.is_empty() {
            let edges: Vec<String> =
                edges.iter().map(|e| format!("{} -{:?}-> {}", e.from, e.kind, e.to)).collect();
            writeln!(f, "edges: {}", edges.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

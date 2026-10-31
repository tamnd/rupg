//! `ExplainPrintPlan` and `ExplainNode` of `explain.c`: the nodes of a plan with their names, their estimates and their parts, in the form that the options of `EXPLAIN` choose.

use std::collections::HashMap;

use rupg_analyze::{Expr, ExprKind, JoinKind, Relation};
use rupg_common::Result;

use crate::format::Writer;
use crate::{Item, Kind, Memoize, Node, Plan, Replaced, Rollup, SortKey, Split, Strategy, Texts};

/// The form of the output, the option `FORMAT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Text,
    Xml,
    Json,
    Yaml,
}

/// The options of `EXPLAIN` that change the output of a plan with no run.
#[derive(Clone, Debug)]
pub struct Options {
    pub verbose: bool,
    pub costs: bool,
    pub format: Format,
    /// With `SUMMARY`, the time of the planning in milliseconds.
    pub planning_time: Option<f64>,
    /// `ANALYZE`: the plan ran, and each node shows its counts.
    pub analyze: bool,
    /// `TIMING`: each node that ran shows its times.
    pub timing: bool,
    /// With `SUMMARY` and `ANALYZE`, the time of the run in milliseconds.
    pub execution_time: Option<f64>,
    /// `BUFFERS`: each node and the planning show the counts of the buffers. rupg reads the rows from memory and has no buffer pool, so each count is 0, and the text form shows no line for them.
    pub buffers: bool,
    /// `track_io_timing` with `BUFFERS`: the other forms also show the times of the reads and the writes of the blocks.
    pub io_timing: bool,
    /// `IO`: the most buffers that the read stream of a scan pins, as `distance_capacity`. A scan of pages in memory pulls each page at a distance of 1 and makes no I/O.
    pub io: Option<i64>,
    /// `SERIALIZE`: the output of the rows of the result.
    pub serialize: Option<Serialized>,
    /// `MEMORY`: the bytes that the planning used and allocated.
    pub memory: Option<(u64, u64)>,
    /// `SETTINGS`: the name and the value of each parameter that changes the planning and does not have its boot value.
    pub settings: Option<Vec<(String, String)>>,
    /// `VERBOSE` with `compute_query_id` on: the query identifier of the statement.
    pub query_id: Option<i64>,
}

/// The output of the rows of the result for `SERIALIZE`, as `SerializeMetrics` keeps it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Serialized {
    /// True for `SERIALIZE BINARY`, which uses the send functions of the types.
    pub binary: bool,
    /// `bytesSent`: the bytes of the `DataRow` messages, without the type byte and the length word of each message.
    pub bytes: u64,
    /// `timeSpent`: the time of the output functions in milliseconds.
    pub time: f64,
}

/// The counts of `show_buffer_usage` in the forms other than text.
const BLOCKS: [&str; 10] = [
    "Shared Hit Blocks",
    "Shared Read Blocks",
    "Shared Dirtied Blocks",
    "Shared Written Blocks",
    "Local Hit Blocks",
    "Local Read Blocks",
    "Local Dirtied Blocks",
    "Local Written Blocks",
    "Temp Read Blocks",
    "Temp Written Blocks",
];

/// The times of `show_buffer_usage` with `track_io_timing`.
const IO_TIMES: [&str; 6] = [
    "Shared I/O Read Time",
    "Shared I/O Write Time",
    "Local I/O Read Time",
    "Local I/O Write Time",
    "Temp I/O Read Time",
    "Temp I/O Write Time",
];

impl Default for Options {
    fn default() -> Self {
        Options {
            verbose: false,
            costs: true,
            format: Format::Text,
            planning_time: None,
            analyze: false,
            timing: false,
            execution_time: None,
            buffers: false,
            io_timing: false,
            io: None,
            serialize: None,
            memory: None,
            settings: None,
            query_id: None,
        }
    }
}

/// The counts of a node of a plan that ran, as its `Instrumentation` and the state of the node keep them for `EXPLAIN ANALYZE`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Actual {
    /// `nloops`: the number of runs of the node.
    pub loops: u64,
    /// `ntuples`: the rows of all the runs.
    pub rows: u64,
    /// The time to the first row of each run, summed, in milliseconds.
    pub startup: f64,
    /// The time of all the runs, in milliseconds.
    pub total: f64,
    /// `nfiltered1` and `nfiltered2`: the rows that a condition of the node removed.
    pub filtered: [u64; 2],
    /// `nsearches`: the searches of an index.
    pub searches: u64,
    /// `exact_pages`: the pages of a `Bitmap Heap Scan`.
    pub exact_blocks: u64,
    /// The memory in kB of a `Sort` that sorted its rows.
    pub sorted: Option<u64>,
    /// True when a `Sort` under a `Limit` kept only the first rows in a heap, as a bounded sort does.
    pub heapsort: bool,
    /// The buckets, the batches and the memory in kB of a `Hash` that built its table.
    pub hash: Option<(u64, u64, u64)>,
    /// The memory in kB of the hash table of an `Aggregate`.
    pub hashagg: Option<u64>,
    /// The memory in kB of the tuplestore of a `CTE Scan`, a `Materialize` or a `WindowAgg`.
    pub storage: Option<u64>,
    /// The hits, the misses, the evictions, the overflows and the memory in kB of a `Memoize`.
    pub memo: Option<[u64; 5]>,
    /// The inserts, the updates and the deletes of a `MERGE`, as `mt_merge_inserted`, `mt_merge_updated` and `mt_merge_deleted` count them. `filtered[0]` of the node has the rows of the plan below it.
    pub merged: Option<[u64; 3]>,
    /// The pages that a `Seq Scan` read, which its read stream pulled.
    pub blocks: u64,
}

/// The counts of the nodes of a plan, by the address of each node, as [`node_key`] gives it.
pub type Actuals = HashMap<usize, Actual>;

/// The key of a node in [`Actuals`].
pub fn node_key(node: &Node<'_>) -> usize {
    std::ptr::from_ref(node).addr()
}

/// The output of `EXPLAIN` for a plan. The text form has a line for each row of the result, and the other forms are one value.
///
/// # Errors
///
/// The errors of the deparse of the expressions.
pub fn explain(plan: &Plan<'_>, options: &Options, texts: &mut dyn Texts) -> Result<String> {
    explain_analyzed(plan, options, None, texts)
}

/// [`explain`] of a plan that ran, with the counts `actuals` of its nodes.
///
/// # Errors
///
/// The errors of the deparse of the expressions.
pub fn explain_analyzed(
    plan: &Plan<'_>,
    options: &Options,
    actuals: Option<&Actuals>,
    texts: &mut dyn Texts,
) -> Result<String> {
    let mut e = Explainer {
        w: Writer::new(options.format),
        options,
        texts,
        plan,
        many: plan.rtable_size() > 1,
        actuals,
    };
    e.w.begin();
    e.w.open_group("Query", None, true);
    e.node(&plan.root, None, None)?;
    if let Some(settings) = &options.settings {
        e.settings(settings);
    }
    // `ExplainPrintPlan`: `compute_query_id = regress` shows no identifier, so that the output of the tests does not change.
    if let Some(id) = options.query_id {
        e.w.integer("Query Identifier", None, id);
    }
    // `peek_buffer_usage`: the other forms show the counts of the buffers of the planning even when they are 0.
    let planning_buffers = options.buffers && options.format != Format::Text;
    if planning_buffers || options.memory.is_some() {
        let text = options.format == Format::Text;
        e.w.open_group("Planning", Some("Planning"), true);
        if text {
            e.w.indent_text();
            e.w.buf.push_str("Planning:\n");
            e.w.indent += 1;
        }
        if planning_buffers {
            e.buffers();
        }
        if let Some((used, allocated)) = options.memory {
            e.memory(used, allocated);
        }
        if text {
            e.w.indent -= 1;
        }
        e.w.close_group("Planning", true);
    }
    if let Some(ms) = options.planning_time {
        e.w.float("Planning Time", Some("ms"), ms, 3);
    }
    // `ExplainPrintTriggers`: the other forms show the group of the triggers even with no trigger.
    if options.analyze && options.format != Format::Text {
        e.w.open_group("Triggers", Some("Triggers"), false);
        e.w.close_group("Triggers", false);
    }
    if let Some(serialized) = &options.serialize {
        e.serialization(serialized);
    }
    if let Some(ms) = options.execution_time {
        e.w.float("Execution Time", Some("ms"), ms, 3);
    }
    e.w.close_group("Query", true);
    e.w.end();
    let mut out = e.w.buf;
    if options.format == Format::Text && out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// The state of `ExplainState` that the nodes read.
struct Explainer<'a, 'q> {
    w: Writer,
    options: &'a Options,
    texts: &'a mut dyn Texts,
    plan: &'a Plan<'q>,
    /// `list_length(es->rtable) > 1`: the plan reads more than one relation, so the columns show the names of their relations.
    many: bool,
    /// With `ANALYZE`, the counts of the nodes.
    actuals: Option<&'a Actuals>,
}

/// The names of a node: the name of the text form, the name of `Node Type`, and the `Strategy`, the `Partial Mode` and the `Operation` of the other forms.
struct Names {
    pname: String,
    sname: &'static str,
    strategy: Option<&'static str>,
    partial: Option<&'static str>,
    operation: Option<&'static str>,
}

/// True for the text form of `EXPLAIN`.
fn text_format(format: Format) -> bool {
    format == Format::Text
}

/// The names of a node of this kind.
fn names(kind: &Kind<'_>) -> Names {
    let simple = |name: &'static str| Names {
        pname: name.to_string(),
        sname: name,
        strategy: None,
        partial: None,
        operation: None,
    };
    match kind {
        Kind::Result { .. } => simple("Result"),
        Kind::SeqScan(_) => simple("Seq Scan"),
        Kind::IndexScan { only: false, .. } => simple("Index Scan"),
        Kind::IndexScan { only: true, .. } => simple("Index Only Scan"),
        Kind::BitmapHeapScan { .. } => simple("Bitmap Heap Scan"),
        Kind::TidScan { range: false, .. } => simple("Tid Scan"),
        Kind::TidScan { range: true, .. } => simple("Tid Range Scan"),
        Kind::BitmapIndexScan { .. } => simple("Bitmap Index Scan"),
        Kind::BitmapAnd => simple("BitmapAnd"),
        Kind::BitmapOr => simple("BitmapOr"),
        Kind::SampleScan(_) => simple("Sample Scan"),
        Kind::FunctionScan(_) => simple("Function Scan"),
        Kind::TableFunctionScan(_) => simple("Table Function Scan"),
        Kind::ValuesScan(_) => simple("Values Scan"),
        Kind::SubqueryScan(_) => simple("Subquery Scan"),
        Kind::CteScan(_) => simple("CTE Scan"),
        Kind::WorkTableScan(_) => simple("WorkTable Scan"),
        Kind::NestedLoop { .. } => simple("Nested Loop"),
        Kind::MergeJoin { .. } => Names { pname: "Merge".to_string(), ..simple("Merge Join") },
        Kind::HashJoin { .. } => Names { pname: "Hash".to_string(), ..simple("Hash Join") },
        Kind::Hash => simple("Hash"),
        Kind::Materialize => simple("Materialize"),
        Kind::Memoize(_) => simple("Memoize"),
        Kind::Aggregate { strategy: Strategy::Group, .. } => simple("Group"),
        Kind::Aggregate { strategy, split, .. } => {
            let (pname, strategy) = match strategy {
                Strategy::Plain => ("Aggregate", "Plain"),
                Strategy::Sorted => ("GroupAggregate", "Sorted"),
                Strategy::Hashed => ("HashAggregate", "Hashed"),
                Strategy::Mixed => ("MixedAggregate", "Mixed"),
                Strategy::Group => ("Group", "Group"),
            };
            // `ExplainNode`: a node of a partial aggregate has the mode in its name.
            let (pname, partial) = match split {
                Split::Simple => (pname.to_string(), "Simple"),
                Split::Partial => (format!("Partial {pname}"), "Partial"),
                Split::Final => (format!("Finalize {pname}"), "Finalize"),
            };
            Names {
                pname,
                sname: "Aggregate",
                strategy: Some(strategy),
                partial: Some(partial),
                operation: None,
            }
        }
        Kind::Sort(_) => simple("Sort"),
        Kind::IncrementalSort { .. } => simple("Incremental Sort"),
        Kind::Limit => simple("Limit"),
        Kind::Gather { .. } => simple("Gather"),
        Kind::GatherMerge { .. } => simple("Gather Merge"),
        Kind::Unique => simple("Unique"),
        Kind::Append => simple("Append"),
        Kind::MergeAppend(_) => simple("Merge Append"),
        Kind::SetOp { sorted: true, .. } => Names { strategy: Some("Sorted"), ..simple("SetOp") },
        Kind::SetOp { .. } => {
            Names { pname: "HashSetOp".to_string(), strategy: Some("Hashed"), ..simple("SetOp") }
        }
        Kind::RecursiveUnion => simple("Recursive Union"),
        Kind::WindowAgg { .. } => simple("WindowAgg"),
        Kind::ProjectSet => simple("ProjectSet"),
        Kind::LockRows => simple("LockRows"),
        Kind::Modify { operation, .. } => Names {
            pname: operation.name().to_string(),
            operation: Some(operation.name()),
            ..simple("ModifyTable")
        },
    }
}

/// The table of the scan of a member below a node that has the scope `scope`.
fn find_member(node: &Node<'_>, scope: usize) -> Option<u32> {
    if node.scope == scope && node.member.is_some() {
        return node.member;
    }
    node.children.iter().find_map(|child| find_member(child, scope))
}

/// The name of a kind of join.
fn join_type(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "Inner",
        JoinKind::Left => "Left",
        JoinKind::Right => "Right",
        JoinKind::Full => "Full",
        JoinKind::Semi => "Semi",
        JoinKind::Anti => "Anti",
        JoinKind::RightSemi => "Right Semi",
        JoinKind::RightAnti => "Right Anti",
    }
}

/// The target of a scan or of a `ModifyTable`, as `ExplainTargetRel`: the label and the name of the object, its schema, and the name of the relation in the plan.
struct Target {
    tag: Option<&'static str>,
    object: Option<String>,
    schema: Option<String>,
    refname: String,
}

impl Explainer<'_, '_> {
    /// `ExplainNode`: a node, its parts, then its subplans and the nodes below it. `relationship` is the relation to the node above, and `plan_name` is the name of a subplan.
    fn node(
        &mut self,
        node: &Node<'_>,
        relationship: Option<&str>,
        plan_name: Option<&str>,
    ) -> Result<()> {
        let text = self.w.format == Format::Text;
        let save_indent = self.w.indent;
        let n = names(&node.kind);
        if text {
            if let Some(name) = plan_name {
                self.w.indent_text();
                self.w.buf.push_str(name);
                self.w.buf.push('\n');
                self.w.indent += 1;
            }
            if self.w.indent > 0 {
                self.w.indent_text();
                self.w.buf.push_str("->  ");
                self.w.indent += 2;
            }
            if node.parallel {
                self.w.buf.push_str("Parallel ");
            }
            self.w.buf.push_str(&n.pname);
            self.w.indent += 1;
        } else {
            self.w.open_group(
                "Plan",
                if relationship.is_some() { None } else { Some("Plan") },
                true,
            );
            self.w.text("Node Type", n.sname);
            if let Some(strategy) = n.strategy {
                self.w.text("Strategy", strategy);
            }
            if let Some(partial) = n.partial {
                self.w.text("Partial Mode", partial);
            }
            if let Some(operation) = n.operation {
                self.w.text("Operation", operation);
            }
            if let Some(relationship) = relationship {
                self.w.text("Parent Relationship", relationship);
            }
            if let Some(name) = plan_name {
                self.w.text("Subplan Name", name);
            }
            self.w.boolean("Parallel Aware", node.parallel);
            self.w.boolean("Async Capable", false);
        }
        match node.kind {
            Kind::IndexScan { index, backward, .. } => self.index_details(index, backward),
            Kind::BitmapIndexScan { index, .. } => {
                let name = self.texts.object(index, false).0;
                if self.w.format == Format::Text {
                    self.w.buf.push_str(&format!(" on {}", self.texts.quote(&name)));
                } else {
                    self.w.text("Index Name", &name);
                }
            }
            _ => {}
        }
        if let Some(target) = self.target(node) {
            self.write_target(&target);
        }
        match &node.kind {
            Kind::NestedLoop { join, .. }
            | Kind::MergeJoin { join, .. }
            | Kind::HashJoin { join, .. } => {
                if !text {
                    self.w.text("Join Type", join_type(*join));
                } else if *join != JoinKind::Inner {
                    self.w.buf.push_str(&format!(" {} Join", join_type(*join)));
                } else if matches!(node.kind, Kind::HashJoin { .. } | Kind::MergeJoin { .. }) {
                    self.w.buf.push_str(" Join");
                }
            }
            Kind::SetOp { except, all, .. } => {
                let command = match (except, all) {
                    (false, false) => "Intersect",
                    (false, true) => "Intersect All",
                    (true, false) => "Except",
                    (true, true) => "Except All",
                };
                if text {
                    self.w.buf.push(' ');
                    self.w.buf.push_str(command);
                } else {
                    self.w.text("Command", command);
                }
            }
            _ => {}
        }
        if self.options.costs {
            let c = node.cost;
            if text {
                self.w.buf.push_str(&format!(
                    "  (cost={:.2}..{:.2} rows={:.0} width={})",
                    c.startup, c.total, c.rows, c.width
                ));
            } else {
                self.w.float("Startup Cost", None, c.startup, 2);
                self.w.float("Total Cost", None, c.total, 2);
                self.w.float("Plan Rows", None, c.rows, 0);
                self.w.integer("Plan Width", None, i64::from(c.width));
            }
        }
        if self.options.analyze {
            self.actual_line(node);
        }
        if text {
            self.w.buf.push('\n');
        }
        if !text || node.disabled {
            self.w.boolean("Disabled", node.disabled);
        }
        if self.options.verbose && !node.output.is_empty() {
            // The calls of a window below are columns of the input of a `WindowAgg`.
            let window = matches!(node.kind, Kind::WindowAgg { .. });
            if window {
                self.texts.below(computed(node));
            }
            let output = self.items(node.scope, &node.output, self.many);
            if window {
                self.texts.below(Vec::new());
            }
            let output = output?;
            self.w.list("Output", &output);
        }
        if matches!(
            node.kind,
            Kind::NestedLoop { .. } | Kind::MergeJoin { .. } | Kind::HashJoin { .. }
        ) && (!text || self.options.verbose && node.inner_unique)
        {
            // The text form shows only a true value, and only with `VERBOSE`.
            self.w.boolean("Inner Unique", node.inner_unique);
        }
        self.parts(node)?;
        self.scan_io(node);
        if self.options.buffers {
            self.buffers();
        }
        let has_children =
            !node.init_plans.is_empty() || !node.children.is_empty() || !node.sub_plans.is_empty();
        if has_children {
            self.w.open_group("Plans", Some("Plans"), false);
        }
        for sub in &node.init_plans {
            self.node(&sub.plan, Some("InitPlan"), Some(&sub.name))?;
        }
        for (i, child) in node.children.iter().enumerate() {
            let relationship = match node.kind {
                Kind::Append | Kind::MergeAppend(_) | Kind::BitmapAnd | Kind::BitmapOr => "Member",
                Kind::SubqueryScan(_) => "Subquery",
                _ if i == 0 => "Outer",
                _ => "Inner",
            };
            self.node(child, Some(relationship), None)?;
        }
        for sub in &node.sub_plans {
            self.node(&sub.plan, Some("SubPlan"), Some(&sub.name))?;
        }
        if has_children {
            self.w.close_group("Plans", false);
        }
        if text {
            self.w.indent = save_indent;
        }
        self.w.close_group("Plan", true);
        Ok(())
    }

    /// `ExplainIndexScanDetails`: the direction of an index scan and the name of its index.
    fn index_details(&mut self, index: u32, backward: bool) {
        let name = self.texts.object(index, false).0;
        if self.w.format == Format::Text {
            if backward {
                self.w.buf.push_str(" Backward");
            }
            self.w.buf.push_str(&format!(" using {}", self.texts.quote(&name)));
        } else {
            self.w.text("Scan Direction", if backward { "Backward" } else { "Forward" });
            self.w.text("Index Name", &name);
        }
    }

    /// The target of a scan or of a `ModifyTable`.
    fn target(&mut self, node: &Node<'_>) -> Option<Target> {
        let scope = node.scope;
        let query = self.plan.scopes[scope].query;
        let relation_of = |index: usize| -> &Relation { &query.relations[index] };
        let (index, tag, object): (usize, Option<&'static str>, Option<(u32, bool)>) = match node
            .kind
        {
            Kind::SeqScan(r)
            | Kind::SampleScan(r)
            | Kind::IndexScan { relation: r, .. }
            | Kind::TidScan { relation: r, .. }
            | Kind::BitmapHeapScan { relation: r, .. } => {
                let table = node.member.unwrap_or(relation_of(r).oid);
                (r, Some("Relation Name"), Some((table, false)))
            }
            Kind::FunctionScan(r) => {
                let function = relation_of(r).function.as_ref();
                let call = match function.map(|f| f.calls.as_slice()) {
                    Some([(call, _)]) => match &call.kind {
                        ExprKind::Func(f) => Some((f.oid, true)),
                        _ => None,
                    },
                    _ => None,
                };
                (r, Some("Function Name"), call)
            }
            Kind::TableFunctionScan(r) => {
                let refname = self.texts.refname(scope, r);
                return Some(Target {
                    tag: Some("Table Function Name"),
                    object: Some("json_table".to_string()),
                    schema: None,
                    refname,
                });
            }
            Kind::CteScan(r) | Kind::WorkTableScan(r) => {
                let name = relation_of(r).cte.as_ref().map(|c| c.name.clone());
                let refname = self.texts.refname(scope, r);
                return Some(Target { tag: Some("CTE Name"), object: name, schema: None, refname });
            }
            Kind::ValuesScan(r) | Kind::SubqueryScan(r) => (r, None, None),
            Kind::Modify { table, relation, .. } => {
                (relation, Some("Relation Name"), Some((table, false)))
            }
            _ => return None,
        };
        let refname = self.texts.refname(scope, index);
        let (object, schema) = match object {
            Some((oid, function)) => {
                let (name, schema) = self.texts.object(oid, function);
                (Some(name), self.options.verbose.then_some(schema))
            }
            None => (None, None),
        };
        Some(Target { tag, object, schema, refname })
    }

    /// `ExplainTargetRel`: ` on` with the name of the target in text, or its properties in the other forms.
    fn write_target(&mut self, target: &Target) {
        if self.w.format == Format::Text {
            self.w.buf.push_str(" on");
            match (&target.schema, &target.object) {
                (Some(schema), Some(object)) => {
                    let text =
                        format!(" {}.{}", self.texts.quote(schema), self.texts.quote(object));
                    self.w.buf.push_str(&text);
                }
                (None, Some(object)) => {
                    let text = format!(" {}", self.texts.quote(object));
                    self.w.buf.push_str(&text);
                }
                _ => {}
            }
            if target.object.as_deref() != Some(target.refname.as_str()) {
                let text = format!(" {}", self.texts.quote(&target.refname));
                self.w.buf.push_str(&text);
            }
        } else {
            if let (Some(tag), Some(object)) = (target.tag, &target.object) {
                self.w.text(tag, object);
            }
            if let Some(schema) = &target.schema {
                self.w.text("Schema", schema);
            }
            self.w.text("Alias", &target.refname);
        }
    }

    /// The parts of a node after its target list: the conditions, the keys and the other properties of its kind.
    fn parts(&mut self, node: &Node<'_>) -> Result<()> {
        let scope = node.scope;
        let upper = self.many || self.options.verbose;
        match &node.kind {
            Kind::Result { replaces, one_time } => {
                if let Some(replaced) = replaces {
                    let text = match replaced {
                        Replaced::Scan(r, _) => {
                            format!("Scan on {}", self.texts.refname(scope, *r))
                        }
                        Replaced::Join(rs) => {
                            let names: Vec<String> =
                                rs.iter().map(|r| self.texts.refname(scope, *r)).collect();
                            format!("Join on {}", names.join(", "))
                        }
                        // `show_result_replacement_info` shows `RESULT_TYPE_UPPER` as an aggregate.
                        Replaced::Upper(rs) if rs.is_empty() => "Aggregate".to_string(),
                        Replaced::Upper(rs) => {
                            let names: Vec<String> =
                                rs.iter().map(|r| self.texts.refname(scope, *r)).collect();
                            format!("Aggregate on {}", names.join(", "))
                        }
                        Replaced::MinMax => "MinMaxAggregate".to_string(),
                    };
                    self.w.text("Replaces", &text);
                }
                self.qual("One-Time Filter", scope, one_time, upper)?;
                self.qual("Filter", scope, &node.filter, upper)?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::SeqScan(r) => {
                let own = Some(*r);
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::ValuesScan(r)
            | Kind::CteScan(r)
            | Kind::WorkTableScan(r)
            | Kind::TableFunctionScan(r) => {
                let own = Some(*r);
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
                self.removed(node, "Rows Removed by Filter", 0);
                if matches!(node.kind, Kind::CteScan(_)) {
                    self.storage(node);
                }
            }
            Kind::IndexScan { cond, order_by, relation, only, exprs, .. } => {
                let own = Some(*relation);
                if *only {
                    self.texts.below(exprs.clone());
                }
                let done = self
                    .scan_qual("Index Cond", scope, cond, self.options.verbose, own)
                    .and_then(|()| {
                        self.scan_qual("Order By", scope, order_by, self.options.verbose, own)
                    })
                    .and_then(|()| {
                        self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)
                    });
                self.texts.below(Vec::new());
                done?;
                self.removed(node, "Rows Removed by Filter", 0);
                if *only && self.options.analyze {
                    let fetches = self.actual(node).map_or(0, |a| a.filtered[1]);
                    self.w.float("Heap Fetches", None, fetches as f64, 0);
                }
                self.searches(node);
            }
            Kind::TidScan { cond, relation, .. } => {
                let own = Some(*relation);
                self.scan_qual("TID Cond", scope, cond, self.options.verbose, own)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::BitmapIndexScan { cond, relation, .. } => {
                self.scan_qual("Index Cond", scope, cond, self.options.verbose, Some(*relation))?;
                self.searches(node);
            }
            Kind::BitmapHeapScan { recheck, relation } => {
                let own = Some(*relation);
                self.scan_qual("Recheck Cond", scope, recheck, self.options.verbose, own)?;
                if !recheck.is_empty() {
                    self.removed(node, "Rows Removed by Index Recheck", 1);
                }
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
                self.removed(node, "Rows Removed by Filter", 0);
                self.heap_blocks(node);
            }
            Kind::SampleScan(r) => {
                self.sampling(scope, *r, upper)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, Some(*r))?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::FunctionScan(r) => {
                if self.options.verbose {
                    let relation = &self.plan.scopes[scope].query.relations[*r];
                    if let Some(function) = &relation.function {
                        let calls: Vec<String> = function
                            .calls
                            .iter()
                            .map(|(call, _)| self.texts.expr(scope, call, true))
                            .collect::<Result<_>>()?;
                        let text = if calls.len() == 1 {
                            calls.join("")
                        } else {
                            format!("ROWS FROM({})", calls.join(", "))
                        };
                        self.w.text("Function Call", &text);
                    }
                }
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, Some(*r))?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::SubqueryScan(_) => {
                self.qual("Filter", scope, &node.filter, true)?;
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::NestedLoop { join_filter, .. } => {
                self.texts.below(computed(node));
                let done = self.join_quals(node, None, join_filter, upper);
                self.texts.below(Vec::new());
                done?;
            }
            Kind::MergeJoin { cond, join_filter, .. } => {
                self.texts.below(computed(node));
                let done = self.join_quals(node, Some(("Merge Cond", cond)), join_filter, upper);
                self.texts.below(Vec::new());
                done?;
            }
            Kind::HashJoin { cond, join_filter, .. } => {
                self.texts.below(computed(node));
                let done = self.join_quals(node, Some(("Hash Cond", cond)), join_filter, upper);
                self.texts.below(Vec::new());
                done?;
            }
            Kind::Gather { workers, .. } | Kind::GatherMerge { workers } => {
                self.qual("Filter", scope, &node.filter, self.options.verbose)?;
                self.removed(node, "Rows Removed by Filter", 0);
                self.w.integer("Workers Planned", None, i64::from(*workers));
                // The session runs the plan in the leader, with no worker.
                if self.options.analyze {
                    self.w.integer("Workers Launched", None, 0);
                }
                if let Kind::Gather { single_copy, .. } = node.kind
                    && (single_copy || !text_format(self.w.format))
                {
                    self.w.boolean("Single Copy", single_copy);
                }
            }
            Kind::Hash => self.hash_info(node),
            Kind::Materialize => self.storage(node),
            Kind::Memoize(memo) => {
                self.memoize(scope, memo, upper)?;
                self.memo_counts(node);
            }
            Kind::Aggregate { strategy, keys, sets, .. } => {
                if sets.is_empty() {
                    if !keys.is_empty() {
                        let keys = self.items(scope, keys, upper)?;
                        self.w.list("Group Key", &keys);
                    }
                } else {
                    self.grouping_sets(scope, sets, upper)?;
                }
                // `show_upper_qual`: a key that the node below computes is a column of its output, so it shows in parentheses.
                self.texts.below(computed(node));
                let filter = self.qual("Filter", scope, &node.filter, upper);
                self.texts.below(Vec::new());
                filter?;
                if matches!(strategy, Strategy::Hashed | Strategy::Mixed) {
                    self.hashagg_info(node);
                }
                self.removed(node, "Rows Removed by Filter", 0);
            }
            Kind::Sort(keys) => {
                // A key that a `WindowAgg` below computes shows as the `WindowAgg` shows it: the calls and the columns that it reads from below are in parentheses.
                let window =
                    node.children.first().filter(|c| matches!(c.kind, Kind::WindowAgg { .. }));
                if let Some(child) = window {
                    self.texts.below(computed(child));
                }
                let keys = self.sort_keys(scope, keys, upper);
                if window.is_some() {
                    self.texts.below(Vec::new());
                }
                let keys = keys?;
                self.w.list("Sort Key", &keys);
                self.sort_info(node);
            }
            Kind::IncrementalSort { keys, presorted } => {
                let texts = self.sort_keys(scope, keys, upper)?;
                self.w.list("Sort Key", &texts);
                let items: Vec<Item<'_>> =
                    keys[..*presorted].iter().map(|k| k.item.clone()).collect();
                let presorted = self.items(scope, &items, upper)?;
                self.w.list("Presorted Key", &presorted);
            }
            Kind::MergeAppend(keys) => {
                let keys = self.sort_keys(scope, keys, upper)?;
                self.w.list("Sort Key", &keys);
                if self.w.format != Format::Text {
                    self.w.integer("Subplans Removed", None, 0);
                }
            }
            Kind::Append => {
                if self.w.format != Format::Text {
                    self.w.integer("Subplans Removed", None, 0);
                }
            }
            Kind::WindowAgg { name, window, rows_frame, partition, run, .. } => {
                // `show_window_keys` shows the keys as the child shows them: a key that the child reads from below is in parentheses.
                self.texts.below(passed(node));
                let spec = self.texts.window(scope, *window, *rows_frame, partition, upper);
                self.texts.below(Vec::new());
                let spec = spec?;
                self.w.text("Window", &format!("{} AS {spec}", self.texts.quote(name)));
                let run: Vec<Item<'_>> = run.iter().map(Item::of).collect();
                self.qual("Run Condition", scope, &run, upper)?;
                // The calls of the windows below are columns of the input.
                self.texts.below(computed(node));
                let filter = self.qual("Filter", scope, &node.filter, upper);
                self.texts.below(Vec::new());
                filter?;
                self.removed(node, "Rows Removed by Filter", 0);
                self.storage(node);
            }
            Kind::Modify { operation, targets, conflict, .. } => {
                self.modify_targets(node, operation.name(), targets);
                self.merge_tuples(node);
                let Some(conflict) = conflict else { return Ok(()) };
                self.w.text("Conflict Resolution", conflict.resolution);
                if !conflict.arbiters.is_empty() {
                    let names: Vec<String> = conflict
                        .arbiters
                        .iter()
                        .map(|oid| self.texts.object(*oid, false).0)
                        .collect();
                    self.w.list("Conflict Arbiter Indexes", &names);
                }
                if let Some(update) = conflict.scope {
                    self.qual("Conflict Filter", update, &conflict.filter, upper)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The conditions of a join, as `ExplainNode` shows them for `NestLoop`, `MergeJoin` and `HashJoin`, with the rows that each one removed.
    fn join_quals(
        &mut self,
        node: &Node<'_>,
        cond: Option<(&str, &[Item<'_>])>,
        join_filter: &[Item<'_>],
        upper: bool,
    ) -> Result<()> {
        let scope = node.scope;
        if let Some((label, cond)) = cond {
            self.qual(label, scope, cond, upper)?;
        }
        self.qual("Join Filter", scope, join_filter, upper)?;
        if !join_filter.is_empty() {
            self.removed(node, "Rows Removed by Join Filter", 0);
        }
        self.qual("Filter", scope, &node.filter, upper)?;
        if !node.filter.is_empty() {
            self.removed(node, "Rows Removed by Filter", 1);
        }
        Ok(())
    }

    /// The counts of a node that ran, or `None` without `ANALYZE` or for a node with no counts.
    fn actual(&self, node: &Node<'_>) -> Option<Actual> {
        self.actuals.and_then(|a| a.get(&node_key(node))).copied()
    }

    /// The part of the first line of a node with its counts, as `ExplainNode` shows it after the estimates: the times, the rows and the runs, or `never executed`.
    fn actual_line(&mut self, node: &Node<'_>) {
        let text = self.w.format == Format::Text;
        let timing = self.options.timing;
        match self.actual(node).filter(|a| a.loops > 0) {
            Some(a) => {
                let loops = a.loops as f64;
                let (startup, total, rows) =
                    (a.startup / loops, a.total / loops, a.rows as f64 / loops);
                if text {
                    self.w.buf.push_str(" (actual ");
                    if timing {
                        self.w.buf.push_str(&format!("time={startup:.3}..{total:.3} "));
                    }
                    self.w.buf.push_str(&format!("rows={rows:.2} loops={loops:.0})"));
                } else {
                    if timing {
                        self.w.float("Actual Startup Time", Some("ms"), startup, 3);
                        self.w.float("Actual Total Time", Some("ms"), total, 3);
                    }
                    self.w.float("Actual Rows", None, rows, 2);
                    self.w.float("Actual Loops", None, loops, 0);
                }
            }
            None if text => self.w.buf.push_str(" (never executed)"),
            None => {
                if timing {
                    self.w.float("Actual Startup Time", Some("ms"), 0.0, 3);
                    self.w.float("Actual Total Time", Some("ms"), 0.0, 3);
                }
                self.w.float("Actual Rows", None, 0.0, 0);
                self.w.float("Actual Loops", None, 0.0, 0);
            }
        }
    }

    /// `show_instrumentation_count`: the rows that a condition of a node with one removed in each run, for a node with the condition. The text form shows no zero.
    fn removed(&mut self, node: &Node<'_>, label: &str, which: usize) {
        if !self.options.analyze {
            return;
        }
        let has = match &node.kind {
            Kind::NestedLoop { join_filter, .. }
            | Kind::MergeJoin { join_filter, .. }
            | Kind::HashJoin { join_filter, .. }
                if which == 0 =>
            {
                !join_filter.is_empty()
            }
            Kind::BitmapHeapScan { recheck, .. } if which == 1 => !recheck.is_empty(),
            _ => !node.filter.is_empty(),
        };
        if !has {
            return;
        }
        let a = self.actual(node).unwrap_or_default();
        let count = a.filtered[which.min(1)];
        let count =
            if matches!(node.kind, Kind::BitmapHeapScan { .. }) && which == 1 { 0 } else { count };
        if count > 0 || self.w.format != Format::Text {
            let value = if a.loops > 0 { count as f64 / a.loops as f64 } else { 0.0 };
            self.w.float(label, None, value, 0);
        }
    }

    /// `show_indexsearches_info`: the searches of an index scan.
    fn searches(&mut self, node: &Node<'_>) {
        if self.options.analyze {
            let searches = self.actual(node).map_or(0, |a| a.searches);
            self.w.integer("Index Searches", None, i64::try_from(searches).unwrap_or(i64::MAX));
        }
    }

    /// `show_tidbitmap_info`: the pages of a `Bitmap Heap Scan`.
    fn heap_blocks(&mut self, node: &Node<'_>) {
        if !self.options.analyze {
            return;
        }
        let exact = self.actual(node).map_or(0, |a| a.exact_blocks);
        if self.w.format != Format::Text {
            self.w.integer("Exact Heap Blocks", None, i64::try_from(exact).unwrap_or(i64::MAX));
            self.w.integer("Lossy Heap Blocks", None, 0);
        } else if exact > 0 {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Heap Blocks: exact={exact}\n"));
        }
    }

    /// `show_buffer_usage` with counts of 0: the other forms show each count, and the text form shows no line.
    fn buffers(&mut self) {
        if self.w.format == Format::Text {
            return;
        }
        for label in BLOCKS {
            self.w.integer(label, None, 0);
        }
        if self.options.io_timing {
            for label in IO_TIMES {
                self.w.float(label, Some("ms"), 0.0, 3);
            }
        }
    }

    /// `show_scan_io_usage`: the prefetch of the pages of a `Seq Scan` or a `Bitmap Heap Scan` that read pages. The pages are in memory, so each page comes at a distance of 1 with no I/O, and the text form shows no line for the I/O.
    fn scan_io(&mut self, node: &Node<'_>) {
        let Some(capacity) = self.options.io else { return };
        let Some(actual) = self.actual(node) else { return };
        let pulled = match node.kind {
            Kind::SeqScan(_) => actual.blocks,
            Kind::BitmapHeapScan { .. } => actual.exact_blocks,
            _ => return,
        };
        if pulled == 0 {
            return;
        }
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Prefetch: avg=1.00 max=1 capacity={capacity}\n"));
        } else {
            self.w.float("Average Prefetch Distance", None, 1.0, 3);
            self.w.integer("Max Prefetch Distance", None, 1);
            self.w.integer("Prefetch Capacity", None, capacity);
            self.w.integer("I/O Count", None, 0);
            self.w.integer("I/O Waits", None, 0);
            self.w.float("Average I/O Size", None, 0.0, 3);
            self.w.float("Average I/Os In Progress", None, 0.0, 3);
        }
    }

    /// `ExplainPrintSerialize`: the output of the rows of the result.
    /// `ExplainPrintSettings`: the text form shows one line, and no line when no parameter changed. The other forms show a group, which can be empty.
    fn settings(&mut self, settings: &[(String, String)]) {
        if self.w.format == Format::Text {
            if !settings.is_empty() {
                let line: Vec<String> =
                    settings.iter().map(|(name, value)| format!("{name} = '{value}'")).collect();
                self.w.text("Settings", &line.join(", "));
            }
            return;
        }
        self.w.open_group("Settings", Some("Settings"), true);
        for (name, value) in settings {
            self.w.text(name, value);
        }
        self.w.close_group("Settings", true);
    }

    /// `show_memory_counters`: the bytes of the planning in kilobytes, rounded up.
    fn memory(&mut self, used: u64, allocated: u64) {
        let used = used.div_ceil(1024);
        let allocated = allocated.div_ceil(1024);
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Memory: used={used}kB  allocated={allocated}kB\n"));
        } else {
            self.w.integer("Memory Used", Some("kB"), i64::try_from(used).unwrap_or(i64::MAX));
            self.w.integer(
                "Memory Allocated",
                Some("kB"),
                i64::try_from(allocated).unwrap_or(i64::MAX),
            );
        }
    }

    fn serialization(&mut self, serialized: &Serialized) {
        let format = if serialized.binary { "binary" } else { "text" };
        let kb = serialized.bytes.div_ceil(1024);
        self.w.open_group("Serialization", Some("Serialization"), true);
        if self.w.format == Format::Text {
            self.w.indent_text();
            let time = if self.options.timing {
                format!("time={:.3} ms  ", serialized.time)
            } else {
                String::new()
            };
            self.w.buf.push_str(&format!("Serialization: {time}output={kb}kB  format={format}\n"));
        } else {
            if self.options.timing {
                self.w.float("Time", Some("ms"), serialized.time, 3);
            }
            self.w.integer("Output Volume", Some("kB"), i64::try_from(kb).unwrap_or(i64::MAX));
            self.w.text("Format", format);
            if self.options.buffers {
                self.buffers();
            }
        }
        self.w.close_group("Serialization", true);
    }

    /// `show_storage_info`: the tuplestore of a `CTE Scan`, a `Materialize` or a `WindowAgg` that ran.
    fn storage(&mut self, node: &Node<'_>) {
        let Some(kb) = self.actual(node).and_then(|a| a.storage) else { return };
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Storage: Memory  Maximum Storage: {kb}kB\n"));
        } else {
            self.w.text("Storage", "Memory");
            self.w.integer("Maximum Storage", Some("kB"), i64::try_from(kb).unwrap_or(i64::MAX));
        }
    }

    /// `show_modifytable_info` of a `MERGE` with `ANALYZE`: the rows of the plan below that went in, changed or went out, and the rows with no action. The text form shows only the counts above zero, and no line when the plan below gave no rows.
    fn merge_tuples(&mut self, node: &Node<'_>) {
        let Some(actual) = self.actual(node) else { return };
        let Some([inserted, updated, deleted]) = actual.merged else { return };
        let total = actual.filtered[0];
        let skipped = total.saturating_sub(inserted + updated + deleted);
        if self.w.format == Format::Text {
            if total == 0 {
                return;
            }
            self.w.indent_text();
            self.w.buf.push_str("Tuples:");
            let counts = [
                ("inserted", inserted),
                ("updated", updated),
                ("deleted", deleted),
                ("skipped", skipped),
            ];
            for (label, count) in counts {
                if count > 0 {
                    self.w.buf.push_str(&format!(" {label}={count}"));
                }
            }
            self.w.buf.push('\n');
        } else {
            let counts = [
                ("Tuples Inserted", inserted),
                ("Tuples Updated", updated),
                ("Tuples Deleted", deleted),
                ("Tuples Skipped", skipped),
            ];
            for (label, count) in counts {
                self.w.float(label, None, count as f64, 0);
            }
        }
    }

    /// `show_sort_info`: the method and the memory of a `Sort` that sorted its rows.
    fn sort_info(&mut self, node: &Node<'_>) {
        let Some(actual) = self.actual(node) else { return };
        let Some(kb) = actual.sorted else { return };
        let method = if actual.heapsort { "top-N heapsort" } else { "quicksort" };
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Sort Method: {method}  Memory: {kb}kB\n"));
        } else {
            self.w.text("Sort Method", method);
            self.w.integer("Sort Space Used", Some("kB"), i64::try_from(kb).unwrap_or(i64::MAX));
            self.w.text("Sort Space Type", "Memory");
        }
    }

    /// `show_hash_info`: the buckets, the batches and the memory of a `Hash` that built its table.
    fn hash_info(&mut self, node: &Node<'_>) {
        let Some((buckets, batches, kb)) = self.actual(node).and_then(|a| a.hash) else { return };
        let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!(
                "Buckets: {buckets}  Batches: {batches}  Memory Usage: {kb}kB\n"
            ));
        } else {
            self.w.integer("Hash Buckets", None, int(buckets));
            self.w.integer("Original Hash Buckets", None, int(buckets));
            self.w.integer("Hash Batches", None, int(batches));
            self.w.integer("Original Hash Batches", None, int(batches));
            self.w.integer("Peak Memory Usage", Some("kB"), int(kb));
        }
    }

    /// `show_hashagg_info`: the partitions of the plan and the memory of the hash table of an `Aggregate`.
    fn hashagg_info(&mut self, node: &Node<'_>) {
        let memory = self.actual(node).and_then(|a| a.hashagg).filter(|kb| *kb > 0);
        if self.w.format != Format::Text {
            if self.options.costs {
                self.w.integer("Planned Partitions", None, 0);
            }
            if let Some(kb) = memory {
                self.w.integer("HashAgg Batches", None, 1);
                self.w.integer(
                    "Peak Memory Usage",
                    Some("kB"),
                    i64::try_from(kb).unwrap_or(i64::MAX),
                );
                self.w.integer("Disk Usage", Some("kB"), 0);
            }
        } else if let Some(kb) = memory {
            self.w.indent_text();
            self.w.buf.push_str(&format!("Batches: 1  Memory Usage: {kb}kB\n"));
        }
    }

    /// The counts of the cache of a `Memoize` that missed, as `show_memoize_info` shows them.
    fn memo_counts(&mut self, node: &Node<'_>) {
        let Some(memo) = self.actual(node).and_then(|a| a.memo).filter(|m| m[1] > 0) else {
            return;
        };
        let [hits, misses, evictions, overflows, kb] = memo;
        if self.w.format == Format::Text {
            self.w.indent_text();
            self.w.buf.push_str(&format!(
                "Hits: {hits}  Misses: {misses}  Evictions: {evictions}  Overflows: {overflows}  Memory Usage: {kb}kB\n"
            ));
        } else {
            let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
            self.w.integer("Cache Hits", None, int(hits));
            self.w.integer("Cache Misses", None, int(misses));
            self.w.integer("Cache Evictions", None, int(evictions));
            self.w.integer("Cache Overflows", None, int(overflows));
            self.w.integer("Peak Memory Usage", Some("kB"), int(kb));
        }
    }

    /// The tables of the members of an inheritance tree that a `ModifyTable` changes, as `show_modifytable_info` labels them: a line for each in text, as `Update on r1 r_1`, and a group `Target Tables` in the other forms.
    fn modify_targets(&mut self, node: &Node<'_>, operation: &str, targets: &[usize]) {
        if targets.is_empty() {
            return;
        }
        let text = self.w.format == Format::Text;
        self.w.open_group("Target Tables", Some("Target Tables"), false);
        for &scope in targets {
            let Some(leaf) = self.plan.scopes.get(scope).and_then(|s| s.leaf) else { continue };
            let member = find_member(node, scope).unwrap_or_default();
            let (name, schema) = self.texts.object(member, false);
            let refname = self.texts.refname(scope, leaf.relation);
            let target = Target {
                tag: Some("Relation Name"),
                object: Some(name),
                schema: self.options.verbose.then_some(schema),
                refname,
            };
            self.w.open_group("Target Table", None, true);
            if text {
                self.w.indent_text();
                self.w.buf.push_str(operation);
            }
            self.write_target(&target);
            if text {
                self.w.buf.push('\n');
            }
            self.w.close_group("Target Table", true);
        }
        self.w.close_group("Target Tables", false);
    }

    /// `show_memoize_info`: the keys of the cache, its mode and, with costs, the estimates of the planner.
    fn memoize(&mut self, scope: usize, memo: &Memoize<'_>, prefix: bool) -> Result<()> {
        let keys = self.items(scope, &memo.keys, prefix)?;
        self.w.text("Cache Key", &keys.join(", "));
        self.w.text("Cache Mode", if memo.binary { "binary" } else { "logical" });
        if !self.options.costs {
            return Ok(());
        }
        if self.w.format == Format::Text {
            self.w.indent_text();
            let line = format!(
                "Estimates: capacity={} distinct keys={:.0} lookups={:.0} hit percent={:.2}%\n",
                memo.capacity,
                memo.distinct,
                memo.lookups,
                memo.hit * 100.0
            );
            self.w.buf.push_str(&line);
        } else {
            self.w.integer("Estimated Capacity", None, i64::from(memo.capacity));
            self.w.float("Estimated Distinct Lookup Keys", None, memo.distinct, 0);
            self.w.float("Estimated Lookups", None, memo.lookups, 0);
            self.w.float("Estimated Hit Percent", None, memo.hit * 100.0, 2);
        }
        Ok(())
    }

    /// `show_qual`: the conditions of a node, with `AND` between them, or nothing for no condition.
    fn qual(&mut self, label: &str, scope: usize, items: &[Item<'_>], prefix: bool) -> Result<()> {
        self.scan_qual(label, scope, items, prefix, None)
    }

    /// [`Self::qual`] of a scan of the relation `own`. A column of another relation is the value of a parameter of a nested loop, which shows the name of its relation, as `get_parameter`.
    fn scan_qual(
        &mut self,
        label: &str,
        scope: usize,
        items: &[Item<'_>],
        prefix: bool,
        own: Option<usize>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let texts = self.scan_items(scope, items, prefix, own)?;
        let text = if texts.len() == 1 {
            texts.into_iter().next().unwrap_or_default()
        } else {
            format!("({})", texts.join(" AND "))
        };
        self.w.text(label, &text);
        Ok(())
    }

    /// The texts of the expressions of a node. An expression that the node reads from the node below and that is not a column is in parentheses, as `get_special_variable` writes it.
    fn items(&mut self, scope: usize, items: &[Item<'_>], prefix: bool) -> Result<Vec<String>> {
        self.scan_items(scope, items, prefix, None)
    }

    /// [`Self::items`] of a scan of the relation `own`, as [`Self::scan_qual`].
    fn scan_items(
        &mut self,
        scope: usize,
        items: &[Item<'_>],
        prefix: bool,
        own: Option<usize>,
    ) -> Result<Vec<String>> {
        items
            .iter()
            .map(|item| {
                let below = item.below
                    && !matches!(item.expr.kind, ExprKind::Var(_) | ExprKind::PlaceHolder(_));
                let text = match own {
                    Some(own) => self.texts.scan_expr(scope, &item.expr, prefix, own)?,
                    None if below => self.texts.below_expr(scope, &item.expr, prefix)?,
                    None => self.texts.expr(scope, &item.expr, prefix)?,
                };
                Ok(if below { format!("({text})") } else { text })
            })
            .collect()
    }

    /// `show_sort_group_keys`: the keys with ` DESC`, ` USING` and the place of the nulls when they are not the defaults of the order.
    fn sort_keys(
        &mut self,
        scope: usize,
        keys: &[SortKey<'_>],
        prefix: bool,
    ) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let mut text = self.items(scope, std::slice::from_ref(&key.item), prefix)?.remove(0);
            // `show_sortorder_options`: a collation that is not the collation of the type.
            let own = rupg_analyze::types::row(key.item.expr.ty).map_or(0, |t| t.collation);
            if key.collation != 0 && key.collation != own {
                text.push_str(" COLLATE ");
                text.push_str(&self.texts.quote(&self.texts.collation_name(key.collation)));
            }
            let reverse = match &key.using {
                Some(op) => {
                    text.push_str(" USING ");
                    text.push_str(op);
                    key.desc
                }
                None if key.desc => {
                    text.push_str(" DESC");
                    true
                }
                None => false,
            };
            if key.nulls_first && !reverse {
                text.push_str(" NULLS FIRST");
            } else if !key.nulls_first && reverse {
                text.push_str(" NULLS LAST");
            }
            out.push(text);
        }
        Ok(out)
    }

    /// `show_grouping_sets` and `show_grouping_set_keys`: for each rollup, the `Sort Key` of its own sort, then a `Hash Key` for each set in a hash table or a `Group Key` for each set of sorted rows. Text shows `()` for a set with no keys, and the keys of a rollup with a sort one level in.
    fn grouping_sets(&mut self, scope: usize, rollups: &[Rollup<'_>], prefix: bool) -> Result<()> {
        let text = self.w.format == Format::Text;
        self.w.open_group("Grouping Sets", Some("Grouping Sets"), false);
        for rollup in rollups {
            let (label, labels) =
                if rollup.hashed { ("Hash Key", "Hash Keys") } else { ("Group Key", "Group Keys") };
            self.w.open_group("Grouping Set", None, true);
            let sorted = !rollup.sort.is_empty();
            if sorted {
                let keys = self.sort_keys(scope, &rollup.sort, prefix)?;
                self.w.list("Sort Key", &keys);
                if text {
                    self.w.indent += 1;
                }
            }
            self.w.open_group(labels, Some(labels), false);
            for keys in &rollup.sets {
                let texts = self.items(scope, keys, prefix)?;
                if texts.is_empty() && text {
                    self.w.text(label, "()");
                } else {
                    self.w.list_nested(label, &texts);
                }
            }
            self.w.close_group(labels, false);
            if sorted && text {
                self.w.indent -= 1;
            }
            self.w.close_group("Grouping Set", true);
        }
        self.w.close_group("Grouping Sets", false);
        Ok(())
    }

    /// `show_tablesample`: the method of `TABLESAMPLE` with its arguments, and `REPEATABLE`.
    fn sampling(&mut self, scope: usize, relation: usize, prefix: bool) -> Result<()> {
        let Some(sample) = &self.plan.scopes[scope].query.relations[relation].tablesample else {
            return Ok(());
        };
        let (method, _) = self.texts.object(sample.handler, true);
        let args: Vec<String> = sample
            .args
            .iter()
            .map(|arg| self.texts.expr(scope, arg, prefix))
            .collect::<Result<_>>()?;
        let repeatable =
            sample.repeatable.as_ref().map(|r| self.texts.expr(scope, r, prefix)).transpose()?;
        // `show_tablesample`: the text form has one line, and the other forms have a property for each part.
        if self.w.format == Format::Text {
            let mut text = format!("{method} ({})", args.join(", "));
            if let Some(repeatable) = &repeatable {
                text.push_str(&format!(" REPEATABLE ({repeatable})"));
            }
            self.w.text("Sampling", &text);
        } else {
            self.w.text("Sampling Method", &method);
            self.w.list("Sampling Parameters", &args);
            if let Some(repeatable) = &repeatable {
                self.w.text("Repeatable Seed", repeatable);
            }
        }
        Ok(())
    }
}

/// The expressions of the targets of the children of a node that are not columns and that the children read from below.
fn passed(node: &Node<'_>) -> Vec<Expr> {
    node.children
        .iter()
        .flat_map(|child| &child.output)
        .filter(|item| item.below && !matches!(item.expr.kind, ExprKind::Var(_)))
        .map(|item| item.expr.as_ref().clone())
        .collect()
}

/// The expressions of the targets of the two sides of a join that are not columns, which a condition of the join reads from the side as `fix_join_expr` finds them in its target list. A `Finalize` aggregate computes its aggregate calls from the partial states below, so they are not values of the node below.
fn computed(node: &Node<'_>) -> Vec<Expr> {
    let finalize = matches!(node.kind, Kind::Aggregate { split: Split::Final, .. });
    node.children
        .iter()
        .flat_map(|child| &child.output)
        .filter(|item| !matches!(item.expr.kind, ExprKind::Var(_)))
        .filter(|item| !(finalize && matches!(item.expr.kind, ExprKind::Agg(_))))
        .map(|item| item.expr.as_ref().clone())
        .collect()
}

//! `ExplainPrintPlan` and `ExplainNode` of `explain.c`: the nodes of a plan with their names, their estimates and their parts, in the form that the options of `EXPLAIN` choose.

use rupg_analyze::{Expr, ExprKind, JoinKind, Relation};
use rupg_common::Result;

use crate::format::Writer;
use crate::{Item, Kind, Memoize, Node, Plan, Replaced, Rollup, SortKey, Strategy, Texts};

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
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub verbose: bool,
    pub costs: bool,
    pub format: Format,
    /// With `SUMMARY`, the time of the planning in milliseconds.
    pub planning_time: Option<f64>,
}

impl Default for Options {
    fn default() -> Self {
        Options { verbose: false, costs: true, format: Format::Text, planning_time: None }
    }
}

/// The output of `EXPLAIN` for a plan. The text form has a line for each row of the result, and the other forms are one value.
///
/// # Errors
///
/// The errors of the deparse of the expressions.
pub fn explain(plan: &Plan<'_>, options: &Options, texts: &mut dyn Texts) -> Result<String> {
    let mut e = Explainer {
        w: Writer::new(options.format),
        options,
        texts,
        plan,
        many: plan.rtable_size() > 1,
    };
    e.w.begin();
    e.w.open_group("Query", None, true);
    e.node(&plan.root, None, None)?;
    if let Some(ms) = options.planning_time {
        e.w.float("Planning Time", Some("ms"), ms, 3);
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
}

/// The names of a node: the name of the text form, the name of `Node Type`, and the `Strategy`, the `Partial Mode` and the `Operation` of the other forms.
struct Names {
    pname: String,
    sname: &'static str,
    strategy: Option<&'static str>,
    partial: Option<&'static str>,
    operation: Option<&'static str>,
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
        Kind::Aggregate { strategy, .. } => {
            let (pname, strategy) = match strategy {
                Strategy::Plain => ("Aggregate", "Plain"),
                Strategy::Sorted => ("GroupAggregate", "Sorted"),
                Strategy::Hashed => ("HashAggregate", "Hashed"),
                Strategy::Mixed => ("MixedAggregate", "Mixed"),
                Strategy::Group => ("Group", "Group"),
            };
            Names {
                pname: pname.to_string(),
                sname: "Aggregate",
                strategy: Some(strategy),
                partial: Some("Simple"),
                operation: None,
            }
        }
        Kind::Sort(_) => simple("Sort"),
        Kind::IncrementalSort { .. } => simple("Incremental Sort"),
        Kind::Limit => simple("Limit"),
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
            self.w.boolean("Parallel Aware", false);
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
            }
            Kind::SeqScan(r) => {
                let own = Some(*r);
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
            }
            Kind::ValuesScan(r)
            | Kind::CteScan(r)
            | Kind::WorkTableScan(r)
            | Kind::TableFunctionScan(r) => {
                let own = Some(*r);
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
            }
            Kind::IndexScan { cond, order_by, relation, .. } => {
                let own = Some(*relation);
                self.scan_qual("Index Cond", scope, cond, self.options.verbose, own)?;
                self.scan_qual("Order By", scope, order_by, self.options.verbose, own)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
            }
            Kind::TidScan { cond, relation, .. } => {
                let own = Some(*relation);
                self.scan_qual("TID Cond", scope, cond, self.options.verbose, own)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
            }
            Kind::BitmapIndexScan { cond, relation, .. } => {
                self.scan_qual("Index Cond", scope, cond, self.options.verbose, Some(*relation))?;
            }
            Kind::BitmapHeapScan { recheck, relation } => {
                let own = Some(*relation);
                self.scan_qual("Recheck Cond", scope, recheck, self.options.verbose, own)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, own)?;
            }
            Kind::SampleScan(r) => {
                self.sampling(scope, *r, upper)?;
                self.scan_qual("Filter", scope, &node.filter, self.options.verbose, Some(*r))?;
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
            }
            Kind::SubqueryScan(_) => self.qual("Filter", scope, &node.filter, true)?,
            Kind::NestedLoop { join_filter, .. } => {
                self.texts.below(computed(node));
                self.qual("Join Filter", scope, join_filter, upper)?;
                self.qual("Filter", scope, &node.filter, upper)?;
                self.texts.below(Vec::new());
            }
            Kind::MergeJoin { cond, join_filter, .. } => {
                self.texts.below(computed(node));
                self.qual("Merge Cond", scope, cond, upper)?;
                self.qual("Join Filter", scope, join_filter, upper)?;
                self.qual("Filter", scope, &node.filter, upper)?;
                self.texts.below(Vec::new());
            }
            Kind::HashJoin { cond, join_filter, .. } => {
                self.texts.below(computed(node));
                self.qual("Hash Cond", scope, cond, upper)?;
                self.qual("Join Filter", scope, join_filter, upper)?;
                self.qual("Filter", scope, &node.filter, upper)?;
                self.texts.below(Vec::new());
            }
            Kind::Memoize(memo) => self.memoize(scope, memo, upper)?,
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
                if matches!(strategy, Strategy::Hashed | Strategy::Mixed)
                    && self.options.costs
                    && self.w.format != Format::Text
                {
                    self.w.integer("Planned Partitions", None, 0);
                }
            }
            Kind::Sort(keys) => {
                let keys = self.sort_keys(scope, keys, upper)?;
                self.w.list("Sort Key", &keys);
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
            }
            Kind::Modify { operation, targets, conflict, .. } => {
                self.modify_targets(node, operation.name(), targets);
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

/// The expressions of the targets of the two sides of a join that are not columns, which a condition of the join reads from the side as `fix_join_expr` finds them in its target list.
fn computed(node: &Node<'_>) -> Vec<Expr> {
    node.children
        .iter()
        .flat_map(|child| &child.output)
        .filter(|item| !matches!(item.expr.kind, ExprKind::Var(_)))
        .map(|item| item.expr.as_ref().clone())
        .collect()
}

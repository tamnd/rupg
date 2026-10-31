//! Parallel plans, as `allpaths.c`, `clauses.c` and `costsize.c` make them: the parallel safety of a query and of the expressions of a relation, the number of workers of a parallel scan, and the costs of `Gather` and `Gather Merge`.
//!
//! The plan of a query in parallel mode can give the rows of a relation by a scan that each process of a group reads a part of, and a `Gather` above it collects the rows of the group. The session runs the plan in one process, so the plan shows the parallel nodes that PostgreSQL makes, and the rows are the same.

use rupg_analyze::{Expr, ExprKind, Query};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Kind, Node};

use crate::Enable;

/// The parallel safety of a function or of a query, as `proparallel`, in the order of `max_parallel_hazard_test`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Hazard {
    /// `PROPARALLEL_SAFE`: a worker can run it.
    Safe,
    /// `PROPARALLEL_RESTRICTED`: only the leader can run it.
    Restricted,
    /// `PROPARALLEL_UNSAFE`: the query cannot be in parallel mode.
    Unsafe,
}

/// `func_parallel`: the parallel safety of the function with the OID `oid`. A function that the catalog does not have is unsafe.
fn function(oid: u32) -> Hazard {
    match builtin::proc_by_oid(oid).map(|p| p.parallel) {
        Some(b's') => Hazard::Safe,
        Some(b'r') => Hazard::Restricted,
        _ => Hazard::Unsafe,
    }
}

/// `check_functions_in_node`: the greatest hazard of the functions that the node calls itself, not those of its arguments.
fn node_functions(expr: &Expr) -> Hazard {
    match &expr.kind {
        ExprKind::Func(f) => function(f.oid),
        ExprKind::NullIf { equal: func, .. }
        | ExprKind::Distinct { equal: func, .. }
        | ExprKind::ScalarArrayOp { func, .. } => function(*func),
        ExprKind::Agg(agg) => function(agg.oid),
        ExprKind::Window(window) => function(window.oid),
        ExprKind::RowCompare(rc) => {
            rc.compares.iter().map(|&f| function(f)).max().unwrap_or(Hazard::Safe)
        }
        _ => Hazard::Safe,
    }
}

/// `max_parallel_hazard`: the greatest hazard of the query, of its subqueries and of its queries of `WITH`, as `max_parallel_hazard_walker` finds it. A query with `FOR UPDATE` or another lock of rows is unsafe.
pub(crate) fn query_hazard(query: &Query) -> Hazard {
    if !query.row_marks.is_empty() {
        return Hazard::Unsafe;
    }
    let mut hazard = Hazard::Safe;
    for cte in &query.ctes {
        hazard = hazard.max(query_hazard(&cte.query));
    }
    for relation in &query.relations {
        if let Some(sub) = &relation.subquery {
            hazard = hazard.max(query_hazard(sub));
        }
    }
    for expr in query.exprs() {
        hazard = hazard.max(expr_hazard(expr));
        if hazard == Hazard::Unsafe {
            break;
        }
    }
    hazard
}

/// The greatest hazard of an expression and of the subqueries in it. A window function, a cast to a domain and a subquery are restricted, as `max_parallel_hazard_walker` treats them.
fn expr_hazard(expr: &Expr) -> Hazard {
    let mut hazard = node_functions(expr);
    match &expr.kind {
        ExprKind::Window(_) | ExprKind::CoerceToDomain(..) => {
            hazard = hazard.max(Hazard::Restricted)
        }
        ExprKind::SubLink(sub) => {
            hazard = hazard.max(Hazard::Restricted).max(query_hazard(&sub.query));
        }
        _ => {}
    }
    for child in expr.children() {
        if hazard == Hazard::Unsafe {
            break;
        }
        hazard = hazard.max(expr_hazard(child));
    }
    hazard
}

/// `is_parallel_safe`: true when a worker can compute the expression. A function that is not safe, a window function, a cast to a domain, a subquery and a column of a query outside, which is a parameter of the executor, are not safe.
pub(crate) fn safe(expr: &Expr) -> bool {
    if node_functions(expr) != Hazard::Safe {
        return false;
    }
    match &expr.kind {
        ExprKind::Window(_) | ExprKind::CoerceToDomain(..) | ExprKind::SubLink(_) => return false,
        ExprKind::Var(var) if var.levels_up > 0 => return false,
        ExprKind::Agg(agg) if agg.levels_up > 0 => return false,
        ExprKind::PlaceHolder(p) if p.levels_up > 0 => return false,
        _ => {}
    }
    expr.children().into_iter().all(safe)
}

/// `compute_parallel_worker` for the heap of a base relation with `pages` pages: the `parallel_workers` option of the table when it has one, or else one worker, and one more each time that the table has three times the pages of the last threshold, from `min_parallel_table_scan_size`. A table with fewer pages than `min_parallel_table_scan_size` has no worker.
pub(crate) fn workers(enable: &Enable, pages: f64, option: Option<i32>) -> i32 {
    let workers = match option {
        Some(n) => n,
        None => {
            let min = enable.min_parallel_table_scan_size;
            if pages < f64::from(min) {
                return 0;
            }
            let mut workers = 1;
            let mut threshold = min.max(1);
            while pages >= f64::from(threshold) * 3.0 {
                workers += 1;
                threshold *= 3;
                if threshold > i32::MAX / 3 {
                    break;
                }
            }
            workers
        }
    };
    workers.min(enable.max_parallel_workers_per_gather)
}

/// `get_parallel_divisor`: the share of the rows of a parallel scan that one process reads. The leader reads rows too with `parallel_leader_participation`, less as the workers are more.
pub(crate) fn divisor(enable: &Enable, workers: i32) -> f64 {
    let mut divisor = f64::from(workers);
    if enable.parallel_leader_participation {
        let leader = 1.0 - 0.3 * f64::from(workers);
        if leader > 0.0 {
            divisor += leader;
        }
    }
    divisor
}

/// `create_gather_path` and `cost_gather`: a `Gather` of the `rows` rows of the parallel plan `child` with `workers` workers. The node starts after `parallel_setup_cost`, and each row costs `parallel_tuple_cost` more.
pub(crate) fn gather<'q>(enable: &Enable, child: Node<'q>, workers: i32, rows: f64) -> Node<'q> {
    let c = child.cost;
    let startup = c.startup + enable.parallel_setup_cost;
    let run = c.total - c.startup + enable.parallel_tuple_cost * rows;
    let mut node =
        Node::new(Kind::Gather { workers, single_copy: false }, child.scope, vec![child]);
    node.cost = Cost { startup, total: startup + run, rows, width: c.width };
    node
}

/// True when a query of the scope can have a parallel plan, as `parallelModeOK` of `standard_planner`: a `SELECT` or a `CREATE TABLE AS` with no query of `WITH` that writes, with `max_parallel_workers_per_gather` above 0, and with no function or lock that is unsafe.
pub(crate) fn mode_ok(enable: &Enable, query: &Query) -> bool {
    enable.max_parallel_workers_per_gather > 0
        && query.write.is_none()
        && !query.ctes.iter().any(|c| c.write.is_some())
        && query_hazard(query) != Hazard::Unsafe
}

/// `cost_gather` again for a `Gather` whose plan below has new costs, as `apply_scanjoin_target_to_paths` makes the `Gather` after it adds the target to the plan below.
pub(crate) fn regather(node: &mut Node<'_>) {
    if let (Kind::Gather { .. }, [child]) = (&node.kind, node.children.as_slice()) {
        let enable = crate::enable();
        let c = child.cost;
        let startup = c.startup + enable.parallel_setup_cost;
        let run = c.total - c.startup + enable.parallel_tuple_cost * node.cost.rows;
        node.cost = Cost { startup, total: startup + run, rows: node.cost.rows, width: c.width };
    }
}

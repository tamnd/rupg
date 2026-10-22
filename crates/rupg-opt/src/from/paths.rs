//! `add_path` and `set_cheapest`: the plans of a relation that the planner keeps, and the plan of them that it chooses. Two costs that differ by less than 1% are the same cost, so a plan that costs a little less can lose to a plan with a lower start cost.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::OrderKey;

/// `STD_FUZZ_FACTOR`.
const FUZZ: f64 = 1.01;

/// `PathCostComparison` and `PathKeysComparison`: the result of a comparison of two plans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compare {
    Equal,
    Better1,
    Better2,
    Different,
}

/// A plan of a relation with its costs and the part of the order of its rows that the planner can use above it, as `truncate_useless_pathkeys` finds it. A parameterized plan reads the values of the relations of `outer`, `required_outer`, and gives the rows of its parameterization.
#[derive(Debug)]
struct Path<T> {
    startup: f64,
    total: f64,
    keys: Vec<OrderKey>,
    outer: BTreeSet<usize>,
    rows: f64,
    plan: T,
}

/// The plans of a relation that `add_path` keeps, in the order of their total cost. All the plans of a relation that are not parameterized have the same rows.
#[derive(Debug)]
pub(crate) struct Paths<T> {
    list: Vec<Path<T>>,
    /// `consider_startup`: true when a plan with a lower start cost is a different plan, which is so when the caller of the query or its `LIMIT` reads only a part of its rows.
    startup: bool,
    /// `consider_param_startup`: the same for a parameterized plan, which is so for the inner side of a semi join or an anti join that is one table.
    param_startup: bool,
}

/// `bms_subset_compare` of the `required_outer` of two plans.
fn compare_outer(a: &BTreeSet<usize>, b: &BTreeSet<usize>) -> Compare {
    match (a.is_subset(b), b.is_subset(a)) {
        (true, true) => Compare::Equal,
        (true, false) => Compare::Better1,
        (false, true) => Compare::Better2,
        (false, false) => Compare::Different,
    }
}

impl<T> Paths<T> {
    pub(crate) fn new(startup: bool) -> Self {
        Paths { list: Vec::new(), startup, param_startup: false }
    }

    /// The paths of a relation that also has parameterized plans.
    pub(super) fn with_params(startup: bool, param_startup: bool) -> Self {
        Paths { list: Vec::new(), startup, param_startup }
    }

    /// `add_path` for a plan that is not parameterized.
    pub(crate) fn add(&mut self, startup: f64, total: f64, keys: Vec<OrderKey>, plan: T) {
        self.add_param(startup, total, keys, BTreeSet::new(), 0.0, plan);
    }

    /// `add_path`: adds a plan unless a plan of the list is as good, and takes away each plan of the list that the new plan is as good as. A plan is as good as another when it costs less or about the same, its order is as useful, it reads the values of fewer relations or the same relations and it gives no more rows.
    pub(super) fn add_param(
        &mut self,
        startup: f64,
        total: f64,
        keys: Vec<OrderKey>,
        outer: BTreeSet<usize>,
        rows: f64,
        plan: T,
    ) {
        let new = (startup, total);
        let new_flag = if outer.is_empty() { self.startup } else { self.param_startup };
        let mut accept = true;
        let mut insert_at = 0;
        let mut i = 0;
        while i < self.list.len() {
            let old = &self.list[i];
            let old_flag = if old.outer.is_empty() { self.startup } else { self.param_startup };
            let flags = (new_flag, old_flag);
            let costs = compare_costs(new, (old.startup, old.total), FUZZ, flags);
            let mut remove = false;
            if costs != Compare::Different {
                let order = compare_keys(&keys, &old.keys);
                let o = compare_outer(&outer, &old.outer);
                let first = matches!(o, Compare::Equal | Compare::Better1) && rows <= old.rows;
                let second = matches!(o, Compare::Equal | Compare::Better2) && rows >= old.rows;
                match (costs, order) {
                    (_, Compare::Different) => {}
                    (Compare::Equal, Compare::Better1) => remove = first,
                    (Compare::Equal, Compare::Better2) => accept = !second,
                    // With the same order and the same relations, the plan with fewer rows wins, and then the new plan must cost less than the old plan by more than a rounding error.
                    (Compare::Equal, _) if o == Compare::Equal => {
                        if rows < old.rows {
                            remove = true;
                        } else if rows > old.rows {
                            accept = false;
                        } else if compare_costs(
                            new,
                            (old.startup, old.total),
                            1.000_000_000_1,
                            flags,
                        ) == Compare::Better1
                        {
                            remove = true;
                        } else {
                            accept = false;
                        }
                    }
                    (Compare::Equal, _) => {
                        remove = o == Compare::Better1 && rows <= old.rows;
                        accept = !(o == Compare::Better2 && rows >= old.rows);
                    }
                    (Compare::Better1, order) => remove = order != Compare::Better2 && first,
                    (Compare::Better2, order) => accept = !(order != Compare::Better1 && second),
                    (Compare::Different, _) => {}
                }
            }
            if remove {
                self.list.remove(i);
            } else {
                if total >= old.total {
                    insert_at = i + 1;
                }
                i += 1;
            }
            if !accept {
                return;
            }
        }
        self.list.insert(insert_at, Path { startup, total, keys, outer, rows, plan });
    }

    /// The parameterized plans that the list keeps, in the order of their total cost.
    pub(super) fn params(self) -> Vec<T> {
        self.list.into_iter().filter(|p| !p.outer.is_empty()).map(|p| p.plan).collect()
    }

    /// `get_cheapest_fractional_path`: the plan that costs the least to give a fraction of its rows, as the caller of the query reads them. A fraction of 1 or more is a number of rows, of the `rows` of the plan of [`Self::best`]. With a fraction of 0, all the rows, it is the plan of [`Self::best`].
    pub(crate) fn fractional(self, fraction: f64, rows: impl Fn(&T) -> f64) -> Option<T> {
        let first = self.best()?;
        let mut best = first;
        if fraction > 0.0 {
            let n = rows(&self.list[first].plan);
            let fraction = if fraction >= 1.0 && n > 0.0 { fraction / n } else { fraction };
            for (i, path) in self.list.iter().enumerate() {
                if i != first && compare_fractional(&self.list[best], path, fraction).is_gt() {
                    best = i;
                }
            }
        }
        self.list.into_iter().nth(best).map(|p| p.plan)
    }

    /// `set_cheapest`: the plan with the lowest total cost, then with the lowest start cost, then with the most useful order.
    fn best(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, path) in self.list.iter().enumerate() {
            let better = best.map(|b| &self.list[b]).is_none_or(|b| {
                path.total < b.total
                    || path.total == b.total
                        && (path.startup < b.startup
                            || path.startup == b.startup
                                && compare_keys(&b.keys, &path.keys) == Compare::Better2)
            });
            if better {
                best = Some(i);
            }
        }
        best
    }

    /// The `pathlist` after `set_cheapest`: the plan that [`Self::best`] chooses first, then the other plans in the order of their total cost.
    pub(crate) fn kept(self) -> Vec<T> {
        let best = self.best();
        let mut out: Vec<T> = self.list.into_iter().map(|p| p.plan).collect();
        if let Some(b) = best {
            let first = out.remove(b);
            out.insert(0, first);
        }
        out
    }
}

/// `compare_fractional_path_costs`: the costs of two plans to give a fraction of their rows. With a fraction of 0 or of 1 or more, the total costs and then the start costs, as `compare_path_costs`.
fn compare_fractional<T>(a: &Path<T>, b: &Path<T>, fraction: f64) -> Ordering {
    if fraction <= 0.0 || fraction >= 1.0 {
        return a.total.total_cmp(&b.total).then(a.startup.total_cmp(&b.startup));
    }
    let cost = |p: &Path<T>| p.startup + fraction * (p.total - p.startup);
    cost(a).total_cmp(&cost(b))
}

/// `compare_path_costs_fuzzily`: the total costs, and then the start costs, of two plans with a fuzz factor. When the plan that costs more in total has the flag of `consider_startup`, it is a different plan if it costs less to start.
fn compare_costs(a: (f64, f64), b: (f64, f64), fuzz: f64, startup: (bool, bool)) -> Compare {
    if a.1 > b.1 * fuzz {
        return if startup.0 && b.0 > a.0 * fuzz { Compare::Different } else { Compare::Better2 };
    }
    if b.1 > a.1 * fuzz {
        return if startup.1 && a.0 > b.0 * fuzz { Compare::Different } else { Compare::Better1 };
    }
    if a.0 > b.0 * fuzz {
        Compare::Better2
    } else if b.0 > a.0 * fuzz {
        Compare::Better1
    } else {
        Compare::Equal
    }
}

/// `compare_pathkeys`: the order of a plan is better when the order of the other plan is the first keys of it.
fn compare_keys(a: &[OrderKey], b: &[OrderKey]) -> Compare {
    let same =
        |x: &OrderKey, y: &OrderKey| x.exprs.iter().any(|e| y.sorts(e, x.desc, x.nulls_first));
    if a.iter().zip(b).any(|(x, y)| !same(x, y)) {
        return Compare::Different;
    }
    match a.len().cmp(&b.len()) {
        Ordering::Greater => Compare::Better1,
        Ordering::Less => Compare::Better2,
        Ordering::Equal => Compare::Equal,
    }
}

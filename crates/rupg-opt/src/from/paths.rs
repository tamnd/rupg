//! `add_path` and `set_cheapest`: the plans of a relation that the planner keeps, and the plan of them that it chooses. Two costs that differ by less than 1% are the same cost, so a plan that costs a little less can lose to a plan with a lower start cost.

use std::cmp::Ordering;

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

/// A plan of a relation with its costs and the part of the order of its rows that the planner can use above it, as `truncate_useless_pathkeys` finds it.
#[derive(Debug)]
struct Path<T> {
    startup: f64,
    total: f64,
    keys: Vec<OrderKey>,
    plan: T,
}

/// The plans of a relation that `add_path` keeps, in the order of their total cost. All the plans of a relation have the same rows.
#[derive(Debug)]
pub(crate) struct Paths<T> {
    list: Vec<Path<T>>,
    /// `consider_startup`: true when a plan with a lower start cost is a different plan, which is so when the caller of the query or its `LIMIT` reads only a part of its rows.
    startup: bool,
}

impl<T> Paths<T> {
    pub(crate) fn new(startup: bool) -> Self {
        Paths { list: Vec::new(), startup }
    }

    /// `add_path`: adds a plan unless a plan of the list is as good, and takes away each plan of the list that the new plan is as good as. A plan is as good as another when it costs less or about the same and its order is as useful.
    pub(crate) fn add(&mut self, startup: f64, total: f64, keys: Vec<OrderKey>, plan: T) {
        let new = (startup, total);
        let mut accept = true;
        let mut insert_at = 0;
        let mut i = 0;
        while i < self.list.len() {
            let old = &self.list[i];
            let costs = compare_costs(new, (old.startup, old.total), FUZZ, self.startup);
            let order = compare_keys(&keys, &old.keys);
            let mut remove = false;
            if costs != Compare::Different && order != Compare::Different {
                match (costs, order) {
                    (Compare::Equal, Compare::Better1) => remove = true,
                    (Compare::Equal, Compare::Better2) => accept = false,
                    // With the same order, the new plan must cost less than the old plan by more than a rounding error.
                    (Compare::Equal, _) => {
                        if compare_costs(
                            new,
                            (old.startup, old.total),
                            1.000_000_000_1,
                            self.startup,
                        ) == Compare::Better1
                        {
                            remove = true;
                        } else {
                            accept = false;
                        }
                    }
                    (Compare::Better1, o) => remove = o != Compare::Better2,
                    (Compare::Better2, o) => accept = o == Compare::Better1,
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
        self.list.insert(insert_at, Path { startup, total, keys, plan });
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
    pub(super) fn kept(self) -> Vec<T> {
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

/// `compare_path_costs_fuzzily`: the total costs, and then the start costs, of two plans with a fuzz factor. With `startup`, a plan that costs more in total but less to start is a different plan.
fn compare_costs(a: (f64, f64), b: (f64, f64), fuzz: f64, startup: bool) -> Compare {
    if a.1 > b.1 * fuzz {
        return if startup && b.0 > a.0 * fuzz { Compare::Different } else { Compare::Better2 };
    }
    if b.1 > a.1 * fuzz {
        return if startup && a.0 > b.0 * fuzz { Compare::Different } else { Compare::Better1 };
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

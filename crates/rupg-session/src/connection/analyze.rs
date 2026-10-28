//! The statistics of the columns of a table that `ANALYZE` writes to `pg_statistic`, as `examine_attribute`, `std_typanalyze`, `compute_trivial_stats`, `compute_distinct_stats` and `compute_scalar_stats` of `analyze.c` compute them from the sample of the rows.

use std::cmp::Ordering;

use rupg_catalog::Column;
use rupg_common::Result;
use rupg_func::{
    Call, STATISTIC_KIND_CORRELATION, STATISTIC_KIND_HISTOGRAM, STATISTIC_KIND_MCV, Session,
    StatSlot, Statistic,
};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

/// `WIDTH_THRESHOLD`: a value that is longer than this with its header does not count for the most common values and the histogram.
const WIDTH_THRESHOLD: usize = 1024;

/// The analysis of a column that `std_typanalyze` chooses from the `<` and the `=` operators of its type.
#[derive(Clone, Copy, Debug)]
enum Method {
    /// `compute_scalar_stats`, with the functions of `<` and `=`.
    Scalar { lt: u32, eq: u32, lt_func: u32 },
    /// `compute_distinct_stats`, with the function of `=`.
    Distinct { eq: u32, eq_func: u32 },
    /// `compute_trivial_stats`.
    Trivial,
}

/// A column that `ANALYZE` computes statistics for, `VacAttrStats`.
#[derive(Clone, Debug)]
pub(super) struct Target {
    /// `tupattnum`.
    pub(super) attnum: i16,
    /// `attrtypid`.
    ty: u32,
    /// `attrcollid`.
    collation: u32,
    /// `typlen`.
    len: i16,
    /// `attstattarget`, with `default_statistics_target` for a column with no target.
    target: i32,
    method: Method,
    /// The options `n_distinct` and `n_distinct_inherited` of the column, or 0.
    n_distinct: (f64, f64),
}

/// `examine_attribute` with `std_typanalyze` for the column `column` with the number `attnum`: `None` for a dropped column, a virtual generated column and a column with the statistics target 0. A column of a range type, of a multirange type and of `tsvector` has its own `typanalyze` in PostgreSQL, which the engine does not have, so it has no statistics either.
pub(super) fn examine(column: &Column, attnum: i16, default_target: i32) -> Option<Target> {
    if column.dropped || column.generated == b'v' || column.stat_target == Some(0) {
        return None;
    }
    let row = builtin::type_by_oid(column.ty)?;
    let base = builtin::type_by_oid(rupg_func::base_type(column.ty))?;
    if matches!(base.kind, b'r' | b'm') || base.oid == oid::TSVECTOR {
        return None;
    }
    let (lt, eq) = rupg_analyze::sort_group_operators(column.ty);
    let code = |op: u32| builtin::operator_by_oid(op).map_or(0, |o| o.code);
    let method = if lt != 0 && eq != 0 {
        Method::Scalar { lt, eq, lt_func: code(lt) }
    } else if eq != 0 {
        Method::Distinct { eq, eq_func: code(eq) }
    } else {
        Method::Trivial
    };
    let option = |name: &str| {
        column
            .options
            .iter()
            .find_map(|o| o.strip_prefix(name)?.strip_prefix('=')?.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    Some(Target {
        attnum,
        ty: column.ty,
        collation: column.collation,
        len: row.len,
        target: column.stat_target.filter(|&t| t >= 0).map_or(default_target, i32::from),
        method,
        n_distinct: (option("n_distinct"), option("n_distinct_inherited")),
    })
}

/// `targrows` of `do_analyze_rel`: the most of `minrows` of the columns of the table and of the expression columns of its indexes, which is 300 times the statistics target, and 100.
pub(super) fn sample_rows<'a>(targets: impl Iterator<Item = &'a Target>) -> usize {
    let most = targets.map(|t| 300 * t.target.max(0)).max().unwrap_or(0);
    usize::try_from(most.max(100)).unwrap_or(100)
}

/// One value of the sample for a column: the value, `VARSIZE_ANY` and `toast_raw_datum_size` of it.
pub(super) type Sampled<'a> = (&'a Value, (usize, usize));

/// The comparisons of the values of a column with the operators of its type.
struct Compare<'a> {
    ty: u32,
    session: &'a dyn Session,
}

impl Compare<'_> {
    /// The result of the function `func` of an operator on two values.
    fn test(&self, func: u32, a: &Value, b: &Value) -> Result<bool> {
        let kernel = rupg_func::kernel(func).ok_or_else(|| {
            rupg_common::Error::new(
                rupg_common::SqlState::FEATURE_NOT_SUPPORTED,
                format!("function {func} is not supported yet"),
            )
        })?;
        let types = [self.ty, self.ty];
        let call = Call {
            session: self.session,
            args: &types,
            ret: oid::BOOL,
            variadic: false,
            want: None,
        };
        Ok(kernel(&call, &[a.clone(), b.clone()])? == Value::Bool(true))
    }

    /// `ApplySortComparator` with the `<` operator `lt_func`: the B-tree comparison of the type when the engine has it, else two calls of the operator.
    fn order(&self, lt_func: u32, a: &Value, b: &Value) -> Result<Ordering> {
        if let Ok(order) = rupg_func::compare(self.ty, a, self.ty, b) {
            return Ok(order);
        }
        Ok(if self.test(lt_func, a, b)? {
            Ordering::Less
        } else if self.test(lt_func, b, a)? {
            Ordering::Greater
        } else {
            Ordering::Equal
        })
    }
}

/// The null fraction, the width and the numbers of the values of the sample that the three methods share.
struct Counts {
    null_cnt: usize,
    nonnull_cnt: usize,
    total_width: f64,
}

impl Target {
    /// True for a type of variable length, `is_varwidth`.
    fn is_varwidth(&self) -> bool {
        self.len < 0
    }

    /// `stawidth`: the average width of the values that are not null for a type of variable length, or `typlen`.
    fn width(&self, counts: &Counts) -> i32 {
        if self.is_varwidth() {
            (counts.total_width / counts.nonnull_cnt as f64) as i32
        } else {
            i32::from(self.len)
        }
    }

    /// The statistics of a column with only nulls in the sample: the column is all null.
    fn all_null(&self, inherit: bool) -> Statistic {
        Statistic {
            attnum: self.attnum,
            inherit,
            null_frac: 1.0,
            width: if self.is_varwidth() { 0 } else { i32::from(self.len) },
            distinct: 0.0,
            slots: Vec::new(),
        }
    }

    /// `compute_stats` of the column for the values `sample` of the rows of the sample, from a table of `totalrows` rows, with the option `n_distinct` or `n_distinct_inherited` of the column after it, as `do_analyze_rel` applies it. `None` when the sample has no rows.
    pub(super) fn compute(
        &self,
        sample: &[Sampled<'_>],
        totalrows: f64,
        inherit: bool,
        session: &dyn Session,
    ) -> Result<Option<Statistic>> {
        let compare = Compare { ty: self.ty, session };
        let stats = match self.method {
            Method::Scalar { lt, eq, lt_func } => {
                self.scalar(sample, totalrows, inherit, (lt, eq, lt_func), &compare)?
            }
            Method::Distinct { eq, eq_func } => {
                self.distinct(sample, totalrows, inherit, (eq, eq_func), &compare)?
            }
            Method::Trivial => self.trivial(sample, inherit),
        };
        Ok(stats.map(|mut stats| {
            let n_distinct = if inherit { self.n_distinct.1 } else { self.n_distinct.0 };
            if n_distinct != 0.0 {
                stats.distinct = n_distinct as f32;
            }
            stats
        }))
    }

    /// The counts of the nulls and the values, and the sum of the widths of the values for a type of variable length. For `varlena` the width is the width in the version, and for `cstring` the length with its end.
    fn count(&self, value: &Value, width: usize, counts: &mut Counts) -> bool {
        if value.is_null() {
            counts.null_cnt += 1;
            return false;
        }
        counts.nonnull_cnt += 1;
        if self.len == -1 {
            counts.total_width += width as f64;
        } else if self.len < 0 {
            let len = match value {
                Value::Text(text) => text.len(),
                _ => width,
            };
            counts.total_width += (len + 1) as f64;
        }
        true
    }

    /// `compute_trivial_stats`: the fraction of the nulls and the average width, for a type with no `=` operator.
    fn trivial(&self, sample: &[Sampled<'_>], inherit: bool) -> Option<Statistic> {
        let mut counts = Counts { null_cnt: 0, nonnull_cnt: 0, total_width: 0.0 };
        for &(value, (width, _)) in sample {
            self.count(value, width, &mut counts);
        }
        if counts.nonnull_cnt > 0 {
            Some(Statistic {
                attnum: self.attnum,
                inherit,
                null_frac: (counts.null_cnt as f64 / sample.len() as f64) as f32,
                width: self.width(&counts),
                distinct: 0.0,
                slots: Vec::new(),
            })
        } else if counts.null_cnt > 0 {
            Some(self.all_null(inherit))
        } else {
            None
        }
    }

    /// `compute_distinct_stats`: the fraction of the nulls, the average width, the most common values and the number of distinct values, for a type with `=` and no `<`. The list of the values that it tracks has the values that it saw more than once by their counts, then the values that it saw once, newest first, up to twice the statistics target.
    fn distinct(
        &self,
        sample: &[Sampled<'_>],
        totalrows: f64,
        inherit: bool,
        (eq, eq_func): (u32, u32),
        compare: &Compare<'_>,
    ) -> Result<Option<Statistic>> {
        let samplerows = sample.len();
        let mut counts = Counts { null_cnt: 0, nonnull_cnt: 0, total_width: 0.0 };
        let mut toowide_cnt = 0;
        let mut num_mcv = usize::try_from(self.target).unwrap_or(0);
        let track_max = (2 * num_mcv).max(10);
        let mut track: Vec<(&Value, usize)> = Vec::with_capacity(track_max);
        for &(value, (width, raw)) in sample {
            if !self.count(value, width, &mut counts) {
                continue;
            }
            if self.len == -1 && raw > WIDTH_THRESHOLD {
                toowide_cnt += 1;
                continue;
            }
            let mut found = None;
            let mut firstcount1 = track.len();
            for (j, item) in track.iter().enumerate() {
                if compare.test(eq_func, value, item.0)? {
                    found = Some(j);
                    break;
                }
                if j < firstcount1 && item.1 == 1 {
                    firstcount1 = j;
                }
            }
            if let Some(mut j) = found {
                track[j].1 += 1;
                while j > 0 && track[j].1 > track[j - 1].1 {
                    track.swap(j, j - 1);
                    j -= 1;
                }
            } else if firstcount1 < track_max {
                if track.len() == track_max {
                    track.pop();
                }
                track.insert(firstcount1, (value, 1));
            }
        }
        if counts.nonnull_cnt == 0 {
            return Ok((counts.null_cnt > 0).then(|| self.all_null(inherit)));
        }
        // `stanullfrac` is a `float4`, which the later steps read.
        let null_frac = f64::from((counts.null_cnt as f64 / samplerows as f64) as f32);
        let nmultiple = track.iter().take_while(|t| t.1 > 1).count();
        let summultiple: usize = track.iter().take_while(|t| t.1 > 1).map(|t| t.1).sum();
        let track_cnt = track.len();
        let mut distinct = if nmultiple == 0 {
            -(1.0 - null_frac)
        } else if track_cnt < track_max && toowide_cnt == 0 && nmultiple == track_cnt {
            track_cnt as f64
        } else {
            let f1 = counts.nonnull_cnt - summultiple;
            let d = f1 + nmultiple;
            haas_stokes(samplerows - counts.null_cnt, f1, d, totalrows * (1.0 - null_frac))
        };
        if distinct > 0.1 * totalrows {
            distinct = -(distinct / totalrows);
        }
        if track_cnt < track_max && toowide_cnt == 0 && distinct > 0.0 && track_cnt <= num_mcv {
            num_mcv = track_cnt;
        } else {
            num_mcv = num_mcv.min(track_cnt);
            if num_mcv > 0 {
                let mcv_counts: Vec<usize> = track[..num_mcv].iter().map(|t| t.1).collect();
                num_mcv = analyze_mcv_list(&mcv_counts, distinct, null_frac, samplerows, totalrows);
            }
        }
        let mut slots = Vec::new();
        if num_mcv > 0 {
            slots.push(StatSlot {
                kind: STATISTIC_KIND_MCV,
                operator: eq,
                collation: self.collation,
                numbers: Some(
                    track[..num_mcv]
                        .iter()
                        .map(|t| (t.1 as f64 / samplerows as f64) as f32)
                        .collect(),
                ),
                values: Some(track[..num_mcv].iter().map(|t| t.0.clone()).collect()),
            });
        }
        Ok(Some(Statistic {
            attnum: self.attnum,
            inherit,
            null_frac: null_frac as f32,
            width: self.width(&counts),
            distinct: distinct as f32,
            slots,
        }))
    }

    /// `compute_scalar_stats`: the fraction of the nulls, the average width, the most common values, the number of distinct values, the histogram of the other values and the correlation of the order of the values with the order of the rows, for a type with `<` and `=`. The values sort by their order and then by their place in the sample, so that the copies of a value are next to each other.
    fn scalar(
        &self,
        sample: &[Sampled<'_>],
        totalrows: f64,
        inherit: bool,
        (lt, eq, lt_func): (u32, u32, u32),
        compare: &Compare<'_>,
    ) -> Result<Option<Statistic>> {
        let samplerows = sample.len();
        let mut counts = Counts { null_cnt: 0, nonnull_cnt: 0, total_width: 0.0 };
        let mut toowide_cnt = 0;
        let mut values: Vec<(&Value, usize)> = Vec::new();
        for &(value, (width, raw)) in sample {
            if !self.count(value, width, &mut counts) {
                continue;
            }
            if self.len == -1 && raw > WIDTH_THRESHOLD {
                toowide_cnt += 1;
                continue;
            }
            values.push((value, values.len()));
        }
        // `stanullfrac` is a `float4`, which the later steps read.
        let null_frac = f64::from((counts.null_cnt as f64 / samplerows as f64) as f32);
        if values.is_empty() {
            if counts.nonnull_cnt > 0 {
                return Ok(Some(Statistic {
                    attnum: self.attnum,
                    inherit,
                    null_frac: null_frac as f32,
                    width: self.width(&counts),
                    distinct: -(1.0 - null_frac) as f32,
                    slots: Vec::new(),
                }));
            }
            return Ok((counts.null_cnt > 0).then(|| self.all_null(inherit)));
        }
        rupg_common::qsort(&mut values, &mut |a: &(&Value, usize), b: &(&Value, usize)| {
            Ok::<_, rupg_common::Error>(compare.order(lt_func, a.0, b.0)?.then(a.1.cmp(&b.1)))
        })?;
        let values_cnt = values.len();
        let mut num_mcv = usize::try_from(self.target).unwrap_or(0);
        let num_bins = num_mcv;
        // The count and the first place in `values` of each of the most common values so far.
        let mut track: Vec<(usize, usize)> = Vec::with_capacity(num_mcv);
        let mut corr_xysum = 0.0;
        let (mut ndistinct, mut nmultiple, mut dups_cnt): (u32, u32, usize) = (0, 0, 0);
        for i in 0..values_cnt {
            corr_xysum += i as f64 * values[i].1 as f64;
            dups_cnt += 1;
            let last = i + 1 == values_cnt
                || compare.order(lt_func, values[i].0, values[i + 1].0)? != Ordering::Equal;
            if !last {
                continue;
            }
            ndistinct += 1;
            if dups_cnt > 1 {
                nmultiple += 1;
                if track.len() < num_mcv || track.last().is_some_and(|t| dups_cnt > t.0) {
                    if track.len() == num_mcv {
                        track.pop();
                    }
                    let at = track.iter().position(|t| dups_cnt > t.0).unwrap_or(track.len());
                    track.insert(at, (dups_cnt, i + 1 - dups_cnt));
                }
            }
            dups_cnt = 0;
        }
        let mut distinct = if nmultiple == 0 {
            -(1.0 - null_frac)
        } else if toowide_cnt == 0 && nmultiple == ndistinct {
            f64::from(ndistinct)
        } else {
            let f1 = usize::try_from(ndistinct - nmultiple).unwrap_or(0) + toowide_cnt;
            let d = f1 + usize::try_from(nmultiple).unwrap_or(0);
            haas_stokes(samplerows - counts.null_cnt, f1, d, totalrows * (1.0 - null_frac))
        };
        if distinct > 0.1 * totalrows {
            distinct = -(distinct / totalrows);
        }
        let ndistinct = usize::try_from(ndistinct).unwrap_or(0);
        let track_cnt = track.len();
        if track_cnt == ndistinct && toowide_cnt == 0 && distinct > 0.0 && track_cnt <= num_mcv {
            num_mcv = track_cnt;
        } else {
            num_mcv = num_mcv.min(track_cnt);
            if num_mcv > 0 {
                let mcv_counts: Vec<usize> = track[..num_mcv].iter().map(|t| t.0).collect();
                num_mcv = analyze_mcv_list(&mcv_counts, distinct, null_frac, samplerows, totalrows);
            }
        }
        let mut slots = Vec::new();
        if num_mcv > 0 {
            slots.push(StatSlot {
                kind: STATISTIC_KIND_MCV,
                operator: eq,
                collation: self.collation,
                numbers: Some(
                    track[..num_mcv]
                        .iter()
                        .map(|t| (t.0 as f64 / samplerows as f64) as f32)
                        .collect(),
                ),
                values: Some(track[..num_mcv].iter().map(|t| values[t.1].0.clone()).collect()),
            });
        }
        // A histogram needs two distinct values that are not in the list of the most common values.
        let mut num_hist = ndistinct - num_mcv;
        if num_hist > num_bins {
            num_hist = num_bins + 1;
        }
        if num_hist >= 2 {
            let mut mcvs = track[..num_mcv].to_vec();
            mcvs.sort_by_key(|t| t.1);
            let mut rest: Vec<&Value> = Vec::with_capacity(values_cnt);
            let mut next = mcvs.iter().peekable();
            let mut src = 0;
            while src < values_cnt {
                if let Some(&&(count, first)) = next.peek()
                    && src >= first
                {
                    src = first + count;
                    next.next();
                    continue;
                }
                rest.push(values[src].0);
                src += 1;
            }
            let nvals = rest.len();
            let (delta, deltafrac) = ((nvals - 1) / (num_hist - 1), (nvals - 1) % (num_hist - 1));
            let (mut pos, mut posfrac) = (0, 0);
            let mut bounds = Vec::with_capacity(num_hist);
            for _ in 0..num_hist {
                bounds.push(rest[pos].clone());
                pos += delta;
                posfrac += deltafrac;
                if posfrac >= num_hist - 1 {
                    pos += 1;
                    posfrac -= num_hist - 1;
                }
            }
            slots.push(StatSlot {
                kind: STATISTIC_KIND_HISTOGRAM,
                operator: lt,
                collation: self.collation,
                numbers: None,
                values: Some(bounds),
            });
        }
        if values_cnt > 1 {
            let n = values_cnt as f64;
            let corr_xsum = (n - 1.0) * n / 2.0;
            let corr_x2sum = (n - 1.0) * n * (2.0 * n - 1.0) / 6.0;
            let corr =
                (n * corr_xysum - corr_xsum * corr_xsum) / (n * corr_x2sum - corr_xsum * corr_xsum);
            slots.push(StatSlot {
                kind: STATISTIC_KIND_CORRELATION,
                operator: lt,
                collation: self.collation,
                numbers: Some(vec![corr as f32]),
                values: None,
            });
        }
        Ok(Some(Statistic {
            attnum: self.attnum,
            inherit,
            null_frac: null_frac as f32,
            width: self.width(&counts),
            distinct: distinct as f32,
            slots,
        }))
    }
}

/// The estimator `Duj1` of Haas and Stokes for the number of distinct values, `n*d / (n - f1 + f1*n/N)`, of `n` values in the sample with `d` distinct values, `f1` of them seen once, in `big_n` values of the table, from `d` up to `big_n`, rounded.
fn haas_stokes(n: usize, f1: usize, d: usize, big_n: f64) -> f64 {
    let (n, f1, d) = (n as f64, f1 as f64, d as f64);
    let mut distinct = if big_n > 0.0 { (n * d) / ((n - f1) + f1 * n / big_n) } else { 0.0 };
    if distinct < d {
        distinct = d;
    }
    if distinct > big_n {
        distinct = big_n;
    }
    (distinct + 0.5).floor()
}

/// `analyze_mcv_list`: the number of the values of `mcv_counts`, the counts of the most common values in the sample from the most common, that are much more common than the values that are not in the list, with the lower end of a confidence interval of the hypergeometric distribution. A sample of the whole table keeps all of them.
fn analyze_mcv_list(
    mcv_counts: &[usize],
    distinct: f64,
    null_frac: f64,
    samplerows: usize,
    totalrows: f64,
) -> usize {
    let mut num_mcv = mcv_counts.len();
    let samples = samplerows as f64;
    if samples == totalrows || totalrows <= 1.0 {
        return num_mcv;
    }
    let ndistinct_table = if distinct < 0.0 { -distinct * totalrows } else { distinct };
    let mut sumcount: f64 = mcv_counts[..num_mcv - 1].iter().map(|&c| c as f64).sum();
    while num_mcv > 0 {
        let mut selec = (1.0 - sumcount / samples - null_frac).clamp(0.0, 1.0);
        let otherdistinct = ndistinct_table - (num_mcv - 1) as f64;
        if otherdistinct > 1.0 {
            selec /= otherdistinct;
        }
        let (big_n, n) = (totalrows, samples);
        let count = mcv_counts[num_mcv - 1] as f64;
        let k = big_n * count / n;
        let variance = n * k * (big_n - k) * (big_n - n) / (big_n * big_n * (big_n - 1.0));
        let stddev = variance.sqrt();
        if count > selec * samples + 2.0 * stddev + 0.5 {
            break;
        }
        num_mcv -= 1;
        if num_mcv == 0 {
            break;
        }
        sumcount -= mcv_counts[num_mcv - 1] as f64;
    }
    num_mcv
}

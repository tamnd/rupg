//! The rows of `pg_statistic`, which `ANALYZE` writes and the planner reads.

use rupg_types::Value;

/// `STATISTIC_KIND_MCV`: the most common values of a column and their frequencies.
pub const STATISTIC_KIND_MCV: i16 = 1;
/// `STATISTIC_KIND_HISTOGRAM`: the bounds of the bins of equal size of the values that are not in the list of the most common values.
pub const STATISTIC_KIND_HISTOGRAM: i16 = 2;
/// `STATISTIC_KIND_CORRELATION`: the correlation of the order of the values with the physical order of the rows.
pub const STATISTIC_KIND_CORRELATION: i16 = 3;

/// The statistics of a column of a table, a row of `pg_statistic` that `update_attstats` writes for `ANALYZE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Statistic {
    /// `staattnum`.
    pub attnum: i16,
    /// `stainherit`: true for the statistics of the rows of the table and of its inheritance children.
    pub inherit: bool,
    /// `stanullfrac`.
    pub null_frac: f32,
    /// `stawidth`.
    pub width: i32,
    /// `stadistinct`: the number of distinct values, a negative number for a fraction of the rows, or 0 when it is not known.
    pub distinct: f32,
    /// The slots that have a kind, in the order of their numbers.
    pub slots: Vec<StatSlot>,
}

/// A slot of a row of `pg_statistic`: `stakindN`, `staopN`, `stacollN`, `stanumbersN` and `stavaluesN`.
#[derive(Clone, Debug, PartialEq)]
pub struct StatSlot {
    pub kind: i16,
    pub operator: u32,
    pub collation: u32,
    pub numbers: Option<Vec<f32>>,
    pub values: Option<Vec<Value>>,
}

rupg_common::persist_struct!(Statistic { attnum, inherit, null_frac, width, distinct, slots });
rupg_common::persist_struct!(StatSlot { kind, operator, collation, numbers, values });

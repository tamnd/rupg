//! `pg_stat_get_autovacuum_scores` from `autovacuum.c`: the scores of autovacuum for each table, materialized view and TOAST table of the database that is not temporary, and the decisions of autovacuum, as `relation_needs_vacanalyze` computes them. The view `pg_stat_autovacuum_scores` reads this function.
//!
//! rupg has no multixacts, so the next multixact is the first, and the multixact score of a relation is 0.

use rupg_common::Result;
use rupg_types::Value;

use crate::Call;

/// `FirstNormalTransactionId`.
const FIRST_NORMAL_XID: u32 = 3;

/// `ReadNextMultiXactId`: rupg has no multixacts, so the next is the first.
const NEXT_MULTI_XACT_ID: u32 = 1;

/// `StatisticRelationId`: autovacuum does not analyze `pg_statistic`.
const STATISTIC_RELATION_ID: u32 = 2619;

/// The rows of `pg_class` that autovacuum looks at, in the order of the columns that [`scores`] reads.
const QUERY: &str = "SELECT oid, relkind, reltoastrelid, reloptions, relfrozenxid, relminmxid, reltuples, relpages, relallfrozen FROM pg_catalog.pg_class WHERE relkind IN ('r', 'm', 't') AND relpersistence <> 't'";

/// `AutoVacOpts`: the options of autovacuum that a relation sets in `reloptions`. A value that the relation does not set is -1, or -2 for the two thresholds where -1 means off.
#[derive(Clone, Copy, Debug)]
struct Options {
    enabled: bool,
    vacuum_threshold: i32,
    vacuum_max_threshold: i32,
    vacuum_ins_threshold: i32,
    analyze_threshold: i32,
    vacuum_scale_factor: f64,
    vacuum_ins_scale_factor: f64,
    analyze_scale_factor: f64,
    freeze_max_age: i32,
    multixact_freeze_max_age: i32,
}

impl Options {
    /// `extract_autovac_opts`: the options of a relation, or `None` when `reloptions` is null.
    fn of(reloptions: &Value) -> Option<Options> {
        let Value::Array(array) = reloptions else { return None };
        let mut out = Options {
            enabled: true,
            vacuum_threshold: -1,
            vacuum_max_threshold: -2,
            vacuum_ins_threshold: -2,
            analyze_threshold: -1,
            vacuum_scale_factor: -1.0,
            vacuum_ins_scale_factor: -1.0,
            analyze_scale_factor: -1.0,
            freeze_max_age: -1,
            multixact_freeze_max_age: -1,
        };
        for option in array.values.iter().flatten() {
            let Value::Text(option) = option else { continue };
            let Some((name, value)) = option.split_once('=') else { continue };
            let int = || value.parse::<i32>().ok();
            let real = || value.parse::<f64>().ok();
            match name {
                "autovacuum_enabled" => {
                    out.enabled = !matches!(value, "false" | "off" | "f" | "no" | "n" | "0");
                }
                "autovacuum_vacuum_threshold" => out.vacuum_threshold = int()?,
                "autovacuum_vacuum_max_threshold" => out.vacuum_max_threshold = int()?,
                "autovacuum_vacuum_insert_threshold" => out.vacuum_ins_threshold = int()?,
                "autovacuum_analyze_threshold" => out.analyze_threshold = int()?,
                "autovacuum_vacuum_scale_factor" => out.vacuum_scale_factor = real()?,
                "autovacuum_vacuum_insert_scale_factor" => out.vacuum_ins_scale_factor = real()?,
                "autovacuum_analyze_scale_factor" => out.analyze_scale_factor = real()?,
                "autovacuum_freeze_max_age" => out.freeze_max_age = int()?,
                "autovacuum_multixact_freeze_max_age" => out.multixact_freeze_max_age = int()?,
                _ => {}
            }
        }
        Some(out)
    }
}

/// The settings of autovacuum that the scores use.
struct Settings {
    active: bool,
    vacuum_threshold: i32,
    vacuum_max_threshold: i32,
    vacuum_ins_threshold: i32,
    analyze_threshold: i32,
    vacuum_scale_factor: f64,
    vacuum_ins_scale_factor: f64,
    analyze_scale_factor: f64,
    freeze_max_age: i32,
    multixact_freeze_max_age: i32,
    failsafe_age: i32,
    multixact_failsafe_age: i32,
    freeze_weight: f64,
    multixact_freeze_weight: f64,
    vacuum_weight: f64,
    vacuum_insert_weight: f64,
    analyze_weight: f64,
}

impl Settings {
    fn read(call: &Call<'_>) -> Settings {
        let text = |name: &str| call.session.setting(name).unwrap_or_default();
        let int = |name: &str| text(name).parse::<i32>().unwrap_or_default();
        let real = |name: &str| text(name).parse::<f64>().unwrap_or_default();
        Settings {
            // `AutoVacuumingActive`.
            active: text("autovacuum") == "on" && text("track_counts") == "on",
            vacuum_threshold: int("autovacuum_vacuum_threshold"),
            vacuum_max_threshold: int("autovacuum_vacuum_max_threshold"),
            vacuum_ins_threshold: int("autovacuum_vacuum_insert_threshold"),
            analyze_threshold: int("autovacuum_analyze_threshold"),
            vacuum_scale_factor: real("autovacuum_vacuum_scale_factor"),
            vacuum_ins_scale_factor: real("autovacuum_vacuum_insert_scale_factor"),
            analyze_scale_factor: real("autovacuum_analyze_scale_factor"),
            freeze_max_age: int("autovacuum_freeze_max_age"),
            multixact_freeze_max_age: int("autovacuum_multixact_freeze_max_age"),
            failsafe_age: int("vacuum_failsafe_age"),
            multixact_failsafe_age: int("vacuum_multixact_failsafe_age"),
            freeze_weight: real("autovacuum_freeze_score_weight"),
            multixact_freeze_weight: real("autovacuum_multixact_freeze_score_weight"),
            vacuum_weight: real("autovacuum_vacuum_score_weight"),
            vacuum_insert_weight: real("autovacuum_vacuum_insert_score_weight"),
            analyze_weight: real("autovacuum_analyze_score_weight"),
        }
    }
}

/// A relation of `pg_class` as autovacuum reads it.
struct Relation {
    oid: u32,
    kind: u8,
    toast: u32,
    options: Option<Options>,
    frozen_xid: u32,
    min_mxid: u32,
    tuples: f32,
    pages: i32,
    all_frozen: i32,
}

impl Relation {
    fn of(row: &[Value]) -> Option<Relation> {
        let oid = |at: usize| match row.get(at) {
            Some(Value::Oid(n)) => Some(*n),
            _ => None,
        };
        let int =
            |at: usize| row.get(at).and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok());
        Some(Relation {
            oid: oid(0)?,
            kind: match row.get(1) {
                Some(Value::Char(c)) => *c,
                _ => return None,
            },
            toast: oid(2)?,
            options: Options::of(row.get(3)?),
            frozen_xid: oid(4)?,
            min_mxid: oid(5)?,
            tuples: match row.get(6) {
                Some(Value::Float4(n)) => *n,
                _ => return None,
            },
            pages: int(7)?,
            all_frozen: int(8)?,
        })
    }
}

/// `AutoVacuumScores` and the three decisions of `relation_needs_vacanalyze`.
#[derive(Debug, Default, PartialEq)]
struct Scores {
    max: f64,
    xid: f64,
    mxid: f64,
    vac: f64,
    vac_ins: f64,
    anl: f64,
    vacuum: bool,
    analyze: bool,
    wraparound: bool,
}

/// `TransactionIdPrecedes` and `MultiXactIdPrecedes`: the modulo-2^32 comparison of two IDs.
fn precedes(a: u32, b: u32) -> bool {
    a.wrapping_sub(b).cast_signed() < 0
}

/// `relation_needs_vacanalyze`: the scores and the decisions of autovacuum for one relation.
fn needs_vacanalyze(
    rel: &Relation,
    opts: Option<&Options>,
    s: &Settings,
    next_xid: u32,
    counts: Option<&crate::RelationCounts>,
) -> Scores {
    let mut out = Scores::default();
    let pick_int =
        |own: Option<i32>, min: i32, setting: i32| own.filter(|&v| v >= min).unwrap_or(setting);
    let pick_real = |own: Option<f64>, setting: f64| own.filter(|&v| v >= 0.0).unwrap_or(setting);
    let vac_scale_factor = pick_real(opts.map(|o| o.vacuum_scale_factor), s.vacuum_scale_factor);
    let vac_base_thresh = pick_int(opts.map(|o| o.vacuum_threshold), 0, s.vacuum_threshold);
    let vac_max_thresh = pick_int(opts.map(|o| o.vacuum_max_threshold), -1, s.vacuum_max_threshold);
    let vac_ins_scale_factor =
        pick_real(opts.map(|o| o.vacuum_ins_scale_factor), s.vacuum_ins_scale_factor);
    let vac_ins_base_thresh =
        pick_int(opts.map(|o| o.vacuum_ins_threshold), -1, s.vacuum_ins_threshold);
    let anl_scale_factor = pick_real(opts.map(|o| o.analyze_scale_factor), s.analyze_scale_factor);
    let anl_base_thresh = pick_int(opts.map(|o| o.analyze_threshold), 0, s.analyze_threshold);
    let freeze_max_age = match opts.map(|o| o.freeze_max_age) {
        Some(v) if v >= 0 => v.min(s.freeze_max_age),
        _ => s.freeze_max_age,
    };
    // `MultiXactMemberFreezeThreshold` gives the setting, because rupg has no members of multixacts.
    let multixact_freeze_max_age = match opts.map(|o| o.multixact_freeze_max_age) {
        Some(v) if v >= 0 => v.min(s.multixact_freeze_max_age),
        _ => s.multixact_freeze_max_age,
    };
    let av_enabled = opts.is_none_or(|o| o.enabled) && s.active;

    let mut xid_force_limit = next_xid.wrapping_sub(freeze_max_age.cast_unsigned());
    if xid_force_limit < FIRST_NORMAL_XID {
        xid_force_limit = xid_force_limit.wrapping_sub(FIRST_NORMAL_XID);
    }
    let normal = rel.frozen_xid >= FIRST_NORMAL_XID;
    let mut force_vacuum = normal && precedes(rel.frozen_xid, xid_force_limit);
    if !force_vacuum {
        let mut multi_force_limit =
            NEXT_MULTI_XACT_ID.wrapping_sub(multixact_freeze_max_age.cast_unsigned());
        if multi_force_limit < 1 {
            multi_force_limit = multi_force_limit.wrapping_sub(1);
        }
        force_vacuum = rel.min_mxid != 0 && precedes(rel.min_mxid, multi_force_limit);
    }
    out.wraparound = force_vacuum;

    let xid_age = if normal { next_xid.wrapping_sub(rel.frozen_xid) } else { 0 };
    let mxid_age =
        if rel.min_mxid != 0 { NEXT_MULTI_XACT_ID.wrapping_sub(rel.min_mxid) } else { 0 };
    out.xid = f64::from(xid_age) / f64::from(freeze_max_age);
    out.mxid = f64::from(mxid_age) / f64::from(multixact_freeze_max_age.max(1));

    // The effective failsafe ages are `int` in PostgreSQL, so each step truncates.
    let mut xid_failsafe =
        f64::from(s.failsafe_age).max(f64::from(s.freeze_max_age) * 1.05).trunc();
    let mut mxid_failsafe = f64::from(s.multixact_failsafe_age)
        .max(f64::from(s.multixact_freeze_max_age) * 1.05)
        .trunc();
    if s.freeze_weight > 1.0 {
        xid_failsafe = (xid_failsafe / s.freeze_weight).trunc();
    }
    if s.multixact_freeze_weight > 1.0 {
        mxid_failsafe = (mxid_failsafe / s.multixact_freeze_weight).trunc();
    }
    if f64::from(xid_age) >= xid_failsafe && out.xid > 1.0 {
        out.xid = out.xid.powf((f64::from(xid_age) / 100_000_000.0).max(1.0));
    }
    if f64::from(mxid_age) >= mxid_failsafe && out.mxid > 1.0 {
        out.mxid = out.mxid.powf((f64::from(mxid_age) / 100_000_000.0).max(1.0));
    }
    out.xid *= s.freeze_weight;
    out.mxid *= s.multixact_freeze_weight;
    out.max = out.xid.max(out.mxid);
    if force_vacuum {
        out.vacuum = true;
    }

    let Some(counts) = counts else { return out };
    // PostgreSQL keeps the counts and the thresholds in `float4`.
    #[expect(clippy::cast_precision_loss, reason = "float4 as in PostgreSQL")]
    let (vactuples, instuples, anltuples) = (
        counts.dead_tuples as f32,
        counts.ins_since_vacuum as f32,
        counts.mod_since_analyze as f32,
    );
    let reltuples = rel.tuples.max(0.0);
    let mut pcnt_unfrozen = 1.0_f32;
    if rel.pages > 0 && rel.all_frozen > 0 {
        #[expect(clippy::cast_precision_loss, reason = "float4 as in PostgreSQL")]
        let frozen = rel.all_frozen.min(rel.pages) as f32 / rel.pages as f32;
        pcnt_unfrozen = 1.0 - frozen;
    }
    #[expect(clippy::cast_possible_truncation, reason = "float4 as in PostgreSQL")]
    let (vac_scale_factor, vac_ins_scale_factor, anl_scale_factor) =
        (vac_scale_factor as f32, vac_ins_scale_factor as f32, anl_scale_factor as f32);
    #[expect(clippy::cast_precision_loss, reason = "float4 as in PostgreSQL")]
    let threshold = |base: i32| base as f32;
    let mut vacthresh = threshold(vac_base_thresh) + vac_scale_factor * reltuples;
    if vac_max_thresh >= 0 && vacthresh > threshold(vac_max_thresh) {
        vacthresh = threshold(vac_max_thresh);
    }
    let vacinsthresh =
        threshold(vac_ins_base_thresh) + vac_ins_scale_factor * reltuples * pcnt_unfrozen;
    let anlthresh = threshold(anl_base_thresh) + anl_scale_factor * reltuples;

    out.vac = f64::from(vactuples) / f64::from(vacthresh.max(1.0)) * s.vacuum_weight;
    out.max = out.max.max(out.vac);
    if av_enabled && vactuples > vacthresh {
        out.vacuum = true;
    }
    if vac_ins_base_thresh >= 0 {
        out.vac_ins =
            f64::from(instuples) / f64::from(vacinsthresh.max(1.0)) * s.vacuum_insert_weight;
        out.max = out.max.max(out.vac_ins);
        if av_enabled && instuples > vacinsthresh {
            out.vacuum = true;
        }
    }
    if rel.oid != STATISTIC_RELATION_ID && rel.kind != b't' {
        out.anl = f64::from(anltuples) / f64::from(anlthresh.max(1.0)) * s.analyze_weight;
        out.max = out.max.max(out.anl);
        if av_enabled && anltuples > anlthresh {
            out.analyze = true;
        }
    }
    out
}

/// `pg_stat_get_autovacuum_scores`: one row for each relation that autovacuum looks at. A TOAST table with no options of its own takes the options of its table.
pub(crate) fn scores(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let settings = Settings::read(call);
    #[expect(clippy::cast_possible_truncation, reason = "the 32 bits of an xid")]
    let next_xid = call.session.next_transaction_id() as u32;
    let (_, rows) = call.session.run_query(QUERY)?;
    let relations: Vec<Relation> = rows.iter().filter_map(|row| Relation::of(row)).collect();
    let toast_options = |toast: u32| {
        relations
            .iter()
            .find(|r| r.kind != b't' && r.toast == toast && r.toast != 0)
            .and_then(|r| r.options.as_ref())
    };
    Ok(relations
        .iter()
        .map(|rel| {
            let opts = match &rel.options {
                Some(own) => Some(own),
                None if rel.kind == b't' => toast_options(rel.oid),
                None => None,
            };
            let counts = call.session.relation_counts(rel.oid);
            let s = needs_vacanalyze(rel, opts, &settings, next_xid, counts.as_ref());
            vec![
                Value::Oid(rel.oid),
                Value::Float8(s.max),
                Value::Float8(s.xid),
                Value::Float8(s.mxid),
                Value::Float8(s.vac),
                Value::Float8(s.vac_ins),
                Value::Float8(s.anl),
                Value::Bool(s.vacuum),
                Value::Bool(s.analyze),
                Value::Bool(s.wraparound),
            ]
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            active: true,
            vacuum_threshold: 50,
            vacuum_max_threshold: 100_000_000,
            vacuum_ins_threshold: 1000,
            analyze_threshold: 50,
            vacuum_scale_factor: 0.2,
            vacuum_ins_scale_factor: 0.2,
            analyze_scale_factor: 0.1,
            freeze_max_age: 200_000_000,
            multixact_freeze_max_age: 400_000_000,
            failsafe_age: 1_600_000_000,
            multixact_failsafe_age: 1_600_000_000,
            freeze_weight: 1.0,
            multixact_freeze_weight: 1.0,
            vacuum_weight: 1.0,
            vacuum_insert_weight: 1.0,
            analyze_weight: 1.0,
        }
    }

    fn relation(kind: u8, frozen_xid: u32) -> Relation {
        Relation {
            oid: 16_384,
            kind,
            toast: 0,
            options: None,
            frozen_xid,
            min_mxid: 1,
            tuples: -1.0,
            pages: 0,
            all_frozen: 0,
        }
    }

    fn counts(dead: i64, inserted: i64, modified: i64) -> crate::RelationCounts {
        crate::RelationCounts {
            dead_tuples: dead,
            ins_since_vacuum: inserted,
            mod_since_analyze: modified,
            ..Default::default()
        }
    }

    #[test]
    fn the_age_of_relfrozenxid_is_the_xid_score() {
        let s = needs_vacanalyze(&relation(b'r', 3), None, &settings(), 11, None);
        assert_eq!(s.xid, 8.0 / 200_000_000.0);
        assert_eq!(s.max, s.xid);
        assert_eq!(s.mxid, 0.0);
        assert!(!s.vacuum && !s.analyze && !s.wraparound);
    }

    #[test]
    fn an_old_relfrozenxid_forces_a_vacuum() {
        let s = needs_vacanalyze(&relation(b'r', 3), None, &settings(), 300_000_000, None);
        assert!(s.wraparound && s.vacuum);
        assert!(s.xid > 1.0);
    }

    #[test]
    fn the_counts_give_the_thresholds() {
        let s = settings();
        let rel = relation(b'r', 3);
        let out = needs_vacanalyze(&rel, None, &s, 4, Some(&counts(51, 999, 60)));
        assert!(out.vacuum && out.analyze);
        assert_eq!(out.vac, 51.0 / 50.0);
        assert_eq!(out.vac_ins, 999.0 / 1000.0);
        assert_eq!(out.anl, 60.0 / 50.0);
        assert_eq!(out.max, out.anl);
        // A TOAST table is not analyzed.
        let out = needs_vacanalyze(&relation(b't', 3), None, &s, 4, Some(&counts(0, 0, 60)));
        assert!(!out.analyze);
        assert_eq!(out.anl, 0.0);
    }

    #[test]
    fn the_options_of_a_relation_win() {
        let options = Value::Array(Box::new(rupg_types::Array::one(vec![
            Some(Value::text("autovacuum_enabled=false")),
            Some(Value::text("autovacuum_vacuum_threshold=10")),
        ])));
        let opts = Options::of(&options).expect("options");
        assert!(!opts.enabled);
        let out = needs_vacanalyze(
            &relation(b'r', 3),
            Some(&opts),
            &settings(),
            4,
            Some(&counts(11, 0, 0)),
        );
        assert_eq!(out.vac, 11.0 / 10.0);
        assert!(!out.vacuum, "autovacuum_enabled=false");
        assert!(Options::of(&Value::Null).is_none());
    }
}

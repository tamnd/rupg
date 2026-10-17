//! The sample methods `system` and `bernoulli` of `TABLESAMPLE`, from `tsm_system.c` and `tsm_bernoulli.c`. The rows of rupg have no pages, so both methods choose rows as `bernoulli` does, with a hash of the number of the row and the seed. So a sample with `REPEATABLE` keeps other rows than PostgreSQL keeps, because those rows depend on the pages of the table.

use rupg_common::{Error, Result, SqlState};

use crate::{Session, hash, random};

/// `BeginSampleScan` of the two methods and the choice of the rows: the numbers of the rows, of `count`, that the sample keeps. `seed` is the value of `REPEATABLE`. With `None`, the seed comes from `pg_global_prng_state`, as in `tablesample_init`.
///
/// # Errors
///
/// `2202H` when the percentage is not between 0 and 100.
pub fn sample_rows(
    percent: f32,
    seed: Option<f64>,
    count: usize,
    session: &dyn Session,
) -> Result<Vec<usize>> {
    if percent.is_nan() || !(0.0..=100.0).contains(&percent) {
        return Err(Error::new(
            SqlState::INVALID_TABLESAMPLE_ARGUMENT,
            "sample percentage must be between 0 and 100",
        ));
    }
    let seed = match seed {
        Some(value) => hash::float8_hash(value),
        None => {
            let value = random::global_range(session, 0, u64::from(u32::MAX));
            u32::try_from(value).unwrap_or(u32::MAX)
        }
    };
    // A row stays when its hash is less than `rint((PG_UINT32_MAX + 1) * percent / 100)`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cutoff =
        ((f64::from(u32::MAX) + 1.0) * f64::from(percent) / 100.0).round_ties_even() as u64;
    Ok((0..count).filter(|&row| u64::from(hash::sample_hash(row as u64, seed)) < cutoff).collect())
}

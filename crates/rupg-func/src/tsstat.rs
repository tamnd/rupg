//! `ts_stat` of `tsvector_op.c`: the statistics of the lexemes of the `tsvector` values that a query gives.

use std::collections::BTreeMap;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Value, oid, weight_of};

use crate::{Call, bad_value, io::base_type};

/// The mask of `ts_stat2` for the weights in the text: bit 3 for `A` down to bit 0 for `D`. Other characters add no bit.
fn weight_mask(text: &str) -> u16 {
    text.chars().fold(0, |mask, c| {
        mask | match c {
            'A' | 'a' => 1 << 3,
            'B' | 'b' => 1 << 2,
            'C' | 'c' => 1 << 1,
            'D' | 'd' => 1,
            _ => 0,
        }
    })
}

/// `ts_stat_sql`: for each lexeme of the values of the query, the number of values that have it and the number of its positions. With a mask, only the positions with a weight in the mask count, and a lexeme with no such position does not count. Without a mask, a lexeme with no positions counts as one. The rows come in the descending order of the bytes of the lexemes, as the walk of the tree of `ts_stat` gives them.
fn stat(call: &Call<'_>, text: &str, mask: u16) -> Result<Vec<Vec<Value>>> {
    let (types, rows) = call.session.run_query(text)?;
    if types.len() != 1 || base_type(types[0]) != oid::TSVECTOR {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "ts_stat query must return one tsvector column",
        ));
    }
    let mut words: BTreeMap<&str, (i32, i32)> = BTreeMap::new();
    for row in &rows {
        let lexemes = match row.first() {
            Some(Value::TsVector(lexemes)) => lexemes,
            Some(Value::Null) => continue,
            _ => return Err(bad_value()),
        };
        for lexeme in lexemes {
            let n = if mask == 0 {
                lexeme.positions.len().max(1)
            } else {
                lexeme.positions.iter().filter(|&&pos| mask & (1 << weight_of(pos)) != 0).count()
            };
            if n > 0 {
                let entry = words.entry(lexeme.word.as_str()).or_default();
                entry.0 = entry.0.wrapping_add(1);
                entry.1 = entry.1.wrapping_add(i32::try_from(n).unwrap_or(i32::MAX));
            }
        }
    }
    Ok(words
        .into_iter()
        .rev()
        .map(|(word, (ndoc, nentry))| {
            vec![Value::text(word), Value::Int4(ndoc), Value::Int4(nentry)]
        })
        .collect())
}

/// `ts_stat1`: `ts_stat(query)`.
pub(crate) fn stat1(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let [text] = args else { return Err(bad_value()) };
    stat(call, text.as_str().ok_or_else(bad_value)?, 0)
}

/// `ts_stat2`: `ts_stat(query, weights)`.
pub(crate) fn stat2(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let [text, weights] = args else { return Err(bad_value()) };
    let mask = weight_mask(weights.as_str().ok_or_else(bad_value)?);
    stat(call, text.as_str().ok_or_else(bad_value)?, mask)
}

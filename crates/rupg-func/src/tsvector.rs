//! The functions of `tsvector` in `tsvector_op.c` that need no text search configuration: `length`, `strip`, `setweight`, `ts_delete`, `tsvector_to_array`, `array_to_tsvector`, `ts_filter`, `unnest`, the operator `||` and `tsvector_cmp`. It also has `ts_lexize` of `dict.c`.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    Array, Lexeme, TSVECTOR_MAX_DATA, TSVECTOR_MAX_POSITIONS, TSVECTOR_MAX_WORD, Value,
    limit_position, position_of, ts_compare_string, tsvector_data_len, weight_of, with_weight,
};

use crate::{Call, Kernel, bad_value, type_error};

/// The largest position.
const MAX_POSITION: u16 = (1 << 14) - 1;

/// The lexemes of a `tsvector` argument.
fn lexemes(value: &Value) -> Result<&[Lexeme]> {
    match value {
        Value::TsVector(lexemes) => Ok(lexemes),
        _ => Err(bad_value()),
    }
}

/// The order of two lexemes in a value.
fn word_order(a: &str, b: &str) -> Ordering {
    ts_compare_string(a.as_bytes(), b.as_bytes()).cmp(&0)
}

/// `tsvector_bsearch`: the index of the lexeme in the value.
fn find(lexemes: &[Lexeme], word: &str) -> Option<usize> {
    lexemes.binary_search_by(|lexeme| word_order(&lexeme.word, word)).ok()
}

/// `parse_weight`: the weight of a letter, 3 for `A` down to 0 for `D`.
fn parse_weight(c: u8) -> Result<u16> {
    Ok(match c {
        b'A' | b'a' => 3,
        b'B' | b'b' => 2,
        b'C' | b'c' => 1,
        b'D' | b'd' => 0,
        // A byte that is not printable ASCII shows in octal, as `charout` shows it.
        0x20..0x7f => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized weight: \"{}\"", char::from(c)),
            ));
        }
        _ => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized weight: \"\\{c:03o}\""),
            ));
        }
    })
}

/// The weight of a `"char"` argument.
fn weight_arg(value: &Value) -> Result<u16> {
    let Value::Char(c) = value else { return Err(bad_value()) };
    parse_weight(*c)
}

/// The elements of an array argument.
fn elements(value: &Value) -> Result<&[Option<Value>]> {
    match value {
        Value::Array(array) => Ok(&array.values),
        _ => Err(bad_value()),
    }
}

/// The error of a value that is too large.
fn too_long(len: usize) -> Error {
    Error::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("string is too long for tsvector ({len} bytes, max {TSVECTOR_MAX_DATA} bytes)"),
    )
}

/// `tsvector_length`: the number of lexemes.
fn length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv] = args else { return Err(bad_value()) };
    Ok(Value::Int4(i32::try_from(lexemes(tsv)?.len()).unwrap_or(i32::MAX)))
}

/// `tsvector_strip`: the lexemes with no positions.
fn strip(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv] = args else { return Err(bad_value()) };
    let out = lexemes(tsv)?
        .iter()
        .map(|lexeme| Lexeme { word: lexeme.word.clone(), positions: Vec::new() })
        .collect();
    Ok(Value::TsVector(out))
}

/// Sets the weight of each position of the lexeme.
fn set_weight(lexeme: &mut Lexeme, weight: u16) {
    for pos in &mut lexeme.positions {
        *pos = with_weight(*pos, weight);
    }
}

/// `tsvector_setweight`: each position with the weight.
fn setweight(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv, weight] = args else { return Err(bad_value()) };
    let weight = weight_arg(weight)?;
    let mut out = lexemes(tsv)?.to_vec();
    for lexeme in &mut out {
        set_weight(lexeme, weight);
    }
    Ok(Value::TsVector(out))
}

/// `tsvector_setweight_by_filter`: the positions of the lexemes in the array with the weight. A null element is not a lexeme.
fn setweight_by_filter(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv, weight, words] = args else { return Err(bad_value()) };
    let weight = weight_arg(weight)?;
    let mut out = lexemes(tsv)?.to_vec();
    for word in elements(words)?.iter().flatten() {
        let word = word.as_str().ok_or_else(bad_value)?;
        if let Some(i) = find(&out, word) {
            set_weight(&mut out[i], weight);
        }
    }
    Ok(Value::TsVector(out))
}

/// The value with no lexeme at the indexes.
fn delete(tsv: &[Lexeme], indexes: &[usize]) -> Value {
    let out = tsv
        .iter()
        .enumerate()
        .filter(|(i, _)| !indexes.contains(i))
        .map(|(_, lexeme)| lexeme.clone())
        .collect();
    Value::TsVector(out)
}

/// `tsvector_delete_str`: the value with no lexeme that is equal to the text.
fn delete_str(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv, word] = args else { return Err(bad_value()) };
    let tsv = lexemes(tsv)?;
    let word = word.as_str().ok_or_else(bad_value)?;
    Ok(delete(tsv, find(tsv, word).as_slice()))
}

/// `tsvector_delete_arr`: the value with no lexeme that is equal to an element of the array.
fn delete_arr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv, words] = args else { return Err(bad_value()) };
    let tsv = lexemes(tsv)?;
    let mut indexes = Vec::new();
    for word in elements(words)?.iter().flatten() {
        indexes.extend(find(tsv, word.as_str().ok_or_else(bad_value)?));
    }
    Ok(delete(tsv, &indexes))
}

/// `tsvector_to_array`: the lexemes as a `text[]`.
fn to_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv] = args else { return Err(bad_value()) };
    let words = lexemes(tsv)?.iter().map(|lexeme| Some(Value::text(lexeme.word.clone())));
    Ok(Value::Array(Box::new(Array::one(words.collect()))))
}

/// `array_to_tsvector`: the elements of a `text[]` as lexemes with no positions, in order with no two equal lexemes.
fn from_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [words] = args else { return Err(bad_value()) };
    let mut out = Vec::new();
    for word in elements(words)? {
        let Some(word) = word else {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "lexeme array may not contain nulls",
            ));
        };
        let word = word.as_str().ok_or_else(bad_value)?;
        if word.is_empty() {
            return Err(Error::new(
                SqlState::ZERO_LENGTH_CHARACTER_STRING,
                "lexeme array may not contain empty strings",
            ));
        }
        if word.len() > TSVECTOR_MAX_WORD {
            return Err(type_error(rupg_types::word_too_long(word.len())));
        }
        out.push(Lexeme { word: word.to_string(), positions: Vec::new() });
    }
    out.sort_by(|a, b| word_order(&a.word, &b.word));
    out.dedup_by(|a, b| a.word == b.word);
    let len = tsvector_data_len(&out);
    if len > TSVECTOR_MAX_DATA {
        return Err(too_long(len));
    }
    Ok(Value::TsVector(out))
}

/// `tsvector_filter`: the positions with a weight in the `"char"[]`, and only the lexemes that keep a position.
fn filter(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [tsv, weights] = args else { return Err(bad_value()) };
    let mut mask = 0u16;
    for weight in elements(weights)? {
        let Some(weight) = weight else {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "weight array may not contain nulls",
            ));
        };
        mask |= 1 << weight_arg(weight)?;
    }
    let mut out = Vec::new();
    for lexeme in lexemes(tsv)? {
        let positions: Vec<u16> = lexeme
            .positions
            .iter()
            .copied()
            .filter(|&pos| mask & (1 << weight_of(pos)) != 0)
            .collect();
        if !positions.is_empty() {
            out.push(Lexeme { word: lexeme.word.clone(), positions });
        }
    }
    Ok(Value::TsVector(out))
}

/// `add_pos`: the positions of `src` after the positions of `dest`, each moved up by `maxpos`. The list stops at 256 positions or after the maximum position.
fn add_positions(dest: &mut Vec<u16>, src: &[u16], maxpos: u16) {
    for &pos in src {
        if dest.len() >= TSVECTOR_MAX_POSITIONS
            || dest.last().is_some_and(|&last| position_of(last) == MAX_POSITION)
        {
            break;
        }
        let moved = limit_position(i32::from(position_of(pos)) + i32::from(maxpos));
        dest.push(with_weight(moved, weight_of(pos)));
    }
}

/// `tsvector_concat`: the lexemes of both values. The positions of the right value come after the largest position of the left value.
fn concat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [left, right] = args else { return Err(bad_value()) };
    let (left, right) = (lexemes(left)?, lexemes(right)?);
    let maxpos = left.iter().flat_map(|l| &l.positions).map(|&p| position_of(p)).max().unwrap_or(0);
    let moved = |lexeme: &Lexeme| {
        let mut positions = Vec::new();
        add_positions(&mut positions, &lexeme.positions, maxpos);
        Lexeme { word: lexeme.word.clone(), positions }
    };
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut i, mut j) = (0, 0);
    while let (Some(a), Some(b)) = (left.get(i), right.get(j)) {
        match word_order(&a.word, &b.word) {
            Ordering::Less => {
                out.push(a.clone());
                i += 1;
            }
            Ordering::Greater => {
                out.push(moved(b));
                j += 1;
            }
            Ordering::Equal => {
                let mut lexeme = a.clone();
                add_positions(&mut lexeme.positions, &b.positions, maxpos);
                out.push(lexeme);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend(left[i..].iter().cloned());
    out.extend(right[j..].iter().map(moved));
    let len = tsvector_data_len(&out);
    if len > TSVECTOR_MAX_DATA {
        return Err(too_long(len));
    }
    Ok(Value::TsVector(out))
}

/// `tsvector_cmp`: the order of `silly_cmp_tsvector`, with the difference of the bytes when two lexemes differ.
fn cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Int4(rupg_types::tsvector_cmp(lexemes(a)?, lexemes(b)?)))
}

/// `tsvector_unnest`: a row for each lexeme with the lexeme, its positions as an `int2[]` and its weights as a `text[]`. A lexeme with no positions has null in both arrays.
pub(crate) fn unnest(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let [tsv] = args else { return Err(bad_value()) };
    let rows = lexemes(tsv)?.iter().map(|lexeme| {
        if lexeme.positions.is_empty() {
            return vec![Value::text(lexeme.word.clone()), Value::Null, Value::Null];
        }
        let positions = lexeme
            .positions
            .iter()
            .map(|&pos| Some(Value::Int2(i16::try_from(position_of(pos)).unwrap_or(i16::MAX))));
        let weights = lexeme.positions.iter().map(|&pos| {
            Some(Value::text(char::from(b"DCBA"[usize::from(weight_of(pos))]).to_string()))
        });
        vec![
            Value::text(lexeme.word.clone()),
            Value::Array(Box::new(Array::one(positions.collect()))),
            Value::Array(Box::new(Array::one(weights.collect()))),
        ]
    });
    Ok(rows.collect())
}

/// `ts_lexize`: the lexemes of a word after the dictionary, or a null when the dictionary does not know the word.
fn lexize(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [dict, word] = args else { return Err(bad_value()) };
    let dict = dict.as_oid().ok_or_else(bad_value)?;
    let word = word.as_str().ok_or_else(bad_value)?;
    let dictionary = rupg_analyze::tsdict::dictionary(call.session.catalog(), dict)?;
    Ok(match dictionary.lexize(word) {
        None => Value::Null,
        Some(words) => {
            let words = words.into_iter().map(|w| Some(Value::text(w))).collect();
            Value::Array(Box::new(Array::one(words)))
        }
    })
}

/// The kernel of a function of `tsvector` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "tsvector_length" => length,
        "tsvector_strip" => strip,
        "tsvector_setweight" => setweight,
        "tsvector_setweight_by_filter" => setweight_by_filter,
        "tsvector_delete_str" => delete_str,
        "tsvector_delete_arr" => delete_arr,
        "tsvector_to_array" => to_array,
        "array_to_tsvector" => from_array,
        "tsvector_filter" => filter,
        "tsvector_concat" => concat,
        "tsvector_cmp" => cmp,
        "ts_lexize" => lexize,
        _ => return None,
    })
}

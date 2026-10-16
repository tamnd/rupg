//! The match operator `@@` of `tsvector` and `tsquery`, from `ts_match_vq` and `TS_execute` of `tsvector_op.c`. A query matches a vector with the rules of PostgreSQL for weights, prefixes, `!` and the distances of `<N>`.

use rupg_common::Result;
use rupg_types::{
    Lexeme, QueryItem, TSQUERY_AND, TSQUERY_NOT, TSQUERY_OR, TSQUERY_PHRASE, Value, position_of,
};

use crate::{Call, Kernel, bad_value};

/// `TSTernaryValue`. `Maybe` is a match of an operand that has no positions, where the positions decide the result.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ternary {
    No,
    Yes,
    Maybe,
}

/// `ExecPhraseData`: the positions where a part of a query under `<N>` matches.
#[derive(Default)]
pub(crate) struct Phrase {
    /// The positions in order with no two equal positions, as `WordEntryPos` values. Only the 14 low bits count.
    pub(crate) pos: Vec<u16>,
    /// True when the part matches at each position that is not in `pos`.
    negate: bool,
    /// The number of lexemes of the match, less one. A position is the position of the last lexeme of the match.
    width: i32,
}

/// The check of the operand at an index of the query, as the callback `chkcond` of `TS_execute` does. The positions go into the `Phrase` when the caller gives one.
pub(crate) type Check<'c> = dyn Fn(usize, Option<&mut Phrase>) -> Ternary + 'c;

/// `TSPO_L_ONLY`, `TSPO_R_ONLY` and `TSPO_BOTH`.
const L_ONLY: u8 = 1;
const R_ONLY: u8 = 2;
const BOTH: u8 = 4;

/// `WEP_GETPOS` as an `int`.
fn pos_of(pos: u16) -> i32 {
    i32::from(position_of(pos))
}

/// `checkclass_str`: the match of an operand with a lexeme of the vector. A lexeme with no positions matches with any weight, but it gives `Maybe` when the caller needs the positions.
fn check_class(lexeme: &Lexeme, weight: u8, data: Option<&mut Phrase>) -> Ternary {
    if lexeme.positions.is_empty() {
        return if data.is_some() { Ternary::Maybe } else { Ternary::Yes };
    }
    let wanted = |pos: &&u16| weight == 0 || weight & (1 << (*pos >> 14)) != 0;
    match data {
        Some(data) => {
            data.pos = lexeme.positions.iter().filter(wanted).copied().collect();
            if data.pos.is_empty() { Ternary::No } else { Ternary::Yes }
        }
        None if lexeme.positions.iter().any(|pos| wanted(&pos)) => Ternary::Yes,
        None => Ternary::No,
    }
}

/// `checkcondition_str`: the match of an operand with the vector. A prefix operand matches each lexeme that starts with it, and the positions are the positions of all these lexemes.
fn check(lexemes: &[Lexeme], operand: &QueryItem, mut data: Option<&mut Phrase>) -> Ternary {
    let QueryItem::Operand { word, weight, prefix } = operand else { return Ternary::No };
    let found = lexemes.binary_search_by(|lexeme| {
        rupg_types::ts_compare_string(lexeme.word.as_bytes(), word.as_bytes()).cmp(&0)
    });
    let (mut res, start) = match found {
        Ok(i) => (check_class(&lexemes[i], *weight, data.as_deref_mut()), i),
        Err(i) => (Ternary::No, i),
    };
    if !*prefix || (res == Ternary::Yes && data.is_none()) {
        return res;
    }
    if let Some(data) = data.as_deref_mut() {
        *data = Phrase { width: data.width, ..Phrase::default() };
    }
    res = Ternary::No;
    let mut all = Vec::new();
    for lexeme in &lexemes[start..] {
        if (res == Ternary::Yes && data.is_none())
            || !lexeme.word.as_bytes().starts_with(word.as_bytes())
        {
            break;
        }
        match check_class(lexeme, *weight, data.as_deref_mut()) {
            Ternary::No => {}
            Ternary::Maybe if data.is_some() => {
                all.clear();
                res = Ternary::Maybe;
                break;
            }
            sub => match data.as_deref_mut() {
                Some(data) => all.append(&mut data.pos),
                None if sub == Ternary::Yes || res == Ternary::No => res = sub,
                None => {}
            },
        }
    }
    if let Some(data) = data
        && !all.is_empty()
    {
        all.sort_by_key(|pos| position_of(*pos));
        all.dedup_by_key(|pos| position_of(*pos));
        data.pos = all;
        res = Ternary::Yes;
    }
    res
}

/// `TS_phrase_output`: merges the positions of the two operands as `emit` gives, with the offsets added. The result is `Yes` when there is a position.
fn output(
    data: &mut Phrase,
    left: &Phrase,
    right: &Phrase,
    emit: u8,
    offsets: (i32, i32),
) -> Ternary {
    let (mut l, mut r) = (0, 0);
    while l < left.pos.len() || r < right.pos.len() {
        let lpos = match left.pos.get(l) {
            Some(&pos) => pos_of(pos) + offsets.0,
            None if emit & R_ONLY == 0 => break,
            None => i32::MAX,
        };
        let rpos = match right.pos.get(r) {
            Some(&pos) => pos_of(pos) + offsets.1,
            None if emit & L_ONLY == 0 => break,
            None => i32::MAX,
        };
        let mut out = 0;
        if lpos < rpos {
            if emit & L_ONLY != 0 {
                out = lpos;
            }
            l += 1;
        } else if lpos == rpos {
            if emit & BOTH != 0 {
                out = rpos;
            }
            l += 1;
            r += 1;
        } else {
            if emit & R_ONLY != 0 {
                out = rpos;
            }
            r += 1;
        }
        if out > 0 {
            // A `WordEntryPos` keeps 16 bits of the position, and the next operator reads only the 14 low bits.
            data.pos.push(u16::try_from(out & 0xffff).unwrap_or_default());
        }
    }
    if data.pos.is_empty() { Ternary::No } else { Ternary::Yes }
}

/// `TS_phrase_execute`: the match of the part of the query at `at`, which is under a `<N>`, with its positions.
fn phrase(items: &[QueryItem], at: usize, data: &mut Phrase, check: &Check<'_>) -> Ternary {
    let QueryItem::Operator { op, distance, left } = &items[at] else {
        return check(at, Some(data));
    };
    let mut l = Phrase::default();
    let mut r = Phrase::default();
    let left = at + *left as usize;
    match *op {
        TSQUERY_NOT => match phrase(items, at + 1, data, check) {
            Ternary::No => {
                data.negate = true;
                Ternary::Yes
            }
            Ternary::Yes if !data.pos.is_empty() => {
                data.negate = !data.negate;
                Ternary::Yes
            }
            Ternary::Yes => {
                data.negate = false;
                Ternary::No
            }
            Ternary::Maybe => Ternary::Maybe,
        },
        TSQUERY_PHRASE | TSQUERY_AND => {
            let lmatch = phrase(items, left, &mut l, check);
            if lmatch == Ternary::No {
                return Ternary::No;
            }
            let rmatch = phrase(items, at + 1, &mut r, check);
            if rmatch == Ternary::No {
                return Ternary::No;
            }
            if lmatch == Ternary::Maybe || rmatch == Ternary::Maybe {
                return Ternary::Maybe;
            }
            let offsets = if *op == TSQUERY_PHRASE {
                data.width = i32::from(*distance) + l.width + r.width;
                (i32::from(*distance) + r.width, 0)
            } else {
                data.width = l.width.max(r.width);
                (data.width - l.width, data.width - r.width)
            };
            match (l.negate, r.negate) {
                (true, true) => {
                    output(data, &l, &r, BOTH | L_ONLY | R_ONLY, offsets);
                    data.negate = true;
                    Ternary::Yes
                }
                (true, false) => output(data, &l, &r, R_ONLY, offsets),
                (false, true) => output(data, &l, &r, L_ONLY, offsets),
                (false, false) => output(data, &l, &r, BOTH, offsets),
            }
        }
        TSQUERY_OR => {
            let lmatch = phrase(items, left, &mut l, check);
            let rmatch = phrase(items, at + 1, &mut r, check);
            if lmatch == Ternary::No && rmatch == Ternary::No {
                return Ternary::No;
            }
            if lmatch == Ternary::Maybe || rmatch == Ternary::Maybe {
                return Ternary::Maybe;
            }
            if lmatch == Ternary::No {
                l.width = 0;
            }
            if rmatch == Ternary::No {
                r.width = 0;
            }
            data.width = l.width.max(r.width);
            let offsets = (data.width - l.width, data.width - r.width);
            let emit = match (l.negate, r.negate) {
                (true, true) => BOTH,
                (true, false) => L_ONLY,
                (false, true) => R_ONLY,
                (false, false) => return output(data, &l, &r, BOTH | L_ONLY | R_ONLY, offsets),
            };
            output(data, &l, &r, emit, offsets);
            data.negate = true;
            Ternary::Yes
        }
        _ => Ternary::No,
    }
}

/// `TS_execute_recurse`: the match of the part of the query at `at`, which is not under a `<N>`. A `<N>` that cannot get the positions does not match.
fn execute(items: &[QueryItem], at: usize, check: &Check<'_>) -> Ternary {
    let QueryItem::Operator { op, left, .. } = &items[at] else {
        return check(at, None);
    };
    let left = at + *left as usize;
    match *op {
        TSQUERY_NOT => match execute(items, at + 1, check) {
            Ternary::No => Ternary::Yes,
            Ternary::Yes => Ternary::No,
            Ternary::Maybe => Ternary::Maybe,
        },
        TSQUERY_AND => {
            let lmatch = execute(items, left, check);
            if lmatch == Ternary::No {
                return Ternary::No;
            }
            match execute(items, at + 1, check) {
                Ternary::Yes => lmatch,
                other => other,
            }
        }
        TSQUERY_OR => {
            let lmatch = execute(items, left, check);
            if lmatch == Ternary::Yes {
                return Ternary::Yes;
            }
            match execute(items, at + 1, check) {
                Ternary::No => lmatch,
                other => other,
            }
        }
        TSQUERY_PHRASE => match phrase(items, at, &mut Phrase::default(), check) {
            Ternary::Yes => Ternary::Yes,
            _ => Ternary::No,
        },
        _ => Ternary::No,
    }
}

/// True when the query matches the vector. An empty query matches no vector.
pub(crate) fn matches(lexemes: &[Lexeme], items: &[QueryItem]) -> bool {
    !items.is_empty() && matches_with(items, &|at, data| check(lexemes, &items[at], data))
}

/// `TS_execute` with the check of the caller: true when the query matches. The query must not be empty.
pub(crate) fn matches_with(items: &[QueryItem], check: &Check<'_>) -> bool {
    execute(items, 0, check) != Ternary::No
}

/// The vector and the query of the arguments.
fn args_of<'a>(vector: &'a Value, query: &'a Value) -> Result<(&'a [Lexeme], &'a [QueryItem])> {
    match (vector, query) {
        (Value::TsVector(lexemes), Value::TsQuery(items)) => Ok((lexemes, items)),
        _ => Err(bad_value()),
    }
}

/// `ts_match_vq`: the operator `tsvector @@ tsquery`.
fn match_vq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [vector, query] = args else { return Err(bad_value()) };
    let (lexemes, items) = args_of(vector, query)?;
    Ok(Value::Bool(matches(lexemes, items)))
}

/// `ts_match_qv`: the operator `tsquery @@ tsvector`.
fn match_qv(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [query, vector] = args else { return Err(bad_value()) };
    let (lexemes, items) = args_of(vector, query)?;
    Ok(Value::Bool(matches(lexemes, items)))
}

/// The kernel of a match function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "ts_match_vq" => match_vq,
        "ts_match_qv" => match_qv,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(vector: &str, query: &str) -> bool {
        let lexemes = rupg_types::tsvector_parse(vector).unwrap();
        matches(&lexemes, &rupg_types::tsquery_parse(query).unwrap())
    }

    #[test]
    fn match_query() {
        assert!(at("a b c", "a & !(d | e)") && !at("a b c", "a & !(b | d)"));
        assert!(at("a:1A b:2B", "b:BC") && !at("a:1A b:2B", "a:B") && at("a b", "a:B"));
        assert!(
            at("cat catalog", "cat:*")
                && !at("ca dog", "cat:*")
                && !at("catalog:1A cats:2B", "cat:*C")
        );
        assert!(
            at("a:1 b:2 c:3", "a <-> b <-> c")
                && !at("a:1 b:2 c:3", "b <-> a")
                && at("a:1 b:2 c:3", "a <2> c")
        );
        assert!(
            at("a:1 b:2 c:3 d:4", "a <-> !c")
                && !at("a:1 b:2 c:3 d:4", "a <-> !b")
                && at("a:1 b:2 c:3 d:4", "(!a | !b) <-> c")
        );
        assert!(!at("a b c", "a <-> b") && at("a b c", "a | b <-> c") && at("a b:2", "!(a <-> b)"));
        assert!(
            !at("a:1 b:6 c:16383 d:5", "((a <5> b) | c) <-> d")
                && at("a:1 b:6 c:16383 d:7", "((a <5> b) | c) <-> d")
        );
        assert!(!at("a b", ""));
    }
}

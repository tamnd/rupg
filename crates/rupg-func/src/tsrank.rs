//! `ts_rank` and `ts_rank_cd` of `tsrank.c`: the rank of a `tsvector` for a `tsquery`. The arithmetic is in `float4` and `float8` at the same steps as in PostgreSQL, so the results are the same to the last bit.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    Array, Lexeme, QueryItem, TSQUERY_AND, TSQUERY_PHRASE, Value, position_of, ts_compare_string,
    weight_of,
};

use crate::tsmatch::{Phrase, Ternary, matches_with};
use crate::{Call, Kernel, bad_value};

/// The weights of `D`, `C`, `B` and `A` when the caller gives none.
const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];

/// `MAXENTRYPOS`.
const MAX_ENTRY_POS: i32 = 1 << 14;

/// The flags of the normalization of the rank.
const NORM_LOG_LENGTH: i32 = 0x01;
const NORM_LENGTH: i32 = 0x02;
const NORM_EXT_DIST: i32 = 0x04;
const NORM_UNIQ: i32 = 0x08;
const NORM_LOG_UNIQ: i32 = 0x10;
const NORM_RDIVRPLUS1: i32 = 0x20;

/// `word_distance`: the weight of two words at a distance.
fn word_distance(distance: i32) -> f32 {
    if distance > 100 {
        return 1e-30;
    }
    #[allow(clippy::cast_precision_loss)]
    let distance = f64::from(distance as f32);
    (1.0 / (1.005 + 0.05 * (distance / 1.5 - 2.0).exp())) as f32
}

/// `cnt_length`: the number of positions, with one for each lexeme with no positions.
fn cnt_length(lexemes: &[Lexeme]) -> i32 {
    let len: usize = lexemes.iter().map(|lexeme| lexeme.positions.len().max(1)).sum();
    i32::try_from(len).unwrap_or(i32::MAX)
}

/// The number of lexemes as an `int`.
fn size(lexemes: &[Lexeme]) -> i32 {
    i32::try_from(lexemes.len()).unwrap_or(i32::MAX)
}

/// `find_wordentry`: the lexemes that match a word, or each lexeme that starts with it for a prefix.
fn find(lexemes: &[Lexeme], word: &str, prefix: bool) -> std::ops::Range<usize> {
    let start = lexemes
        .partition_point(|lexeme| ts_compare_string(lexeme.word.as_bytes(), word.as_bytes()) < 0);
    let end = if prefix {
        start
            + lexemes[start..]
                .iter()
                .take_while(|lexeme| lexeme.word.as_bytes().starts_with(word.as_bytes()))
                .count()
    } else if lexemes.get(start).is_some_and(|lexeme| lexeme.word == word) {
        start + 1
    } else {
        start
    };
    start..end
}

/// `SortAndUniqItems`: the words and the prefix flags of the operands, in order with no two equal words.
fn operands(items: &[QueryItem]) -> Vec<(&str, bool)> {
    let mut out: Vec<(&str, bool)> = items
        .iter()
        .filter_map(|item| match item {
            QueryItem::Operand { word, prefix, .. } => Some((word.as_str(), *prefix)),
            QueryItem::Operator { .. } => None,
        })
        .collect();
    out.sort_by(|a, b| ts_compare_string(a.0.as_bytes(), b.0.as_bytes()).cmp(&0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// The weight of a position.
fn wpos(weights: &[f32; 4], pos: u16) -> f32 {
    weights[usize::from(weight_of(pos))]
}

/// `calc_rank_and`: the rank of the pairs of operands by the distances of their positions.
fn rank_and(weights: &[f32; 4], lexemes: &[Lexeme], items: &[QueryItem]) -> f32 {
    let operands = operands(items);
    if operands.len() < 2 {
        return rank_or(weights, lexemes, items);
    }
    // The position of a lexeme with no positions.
    let none = [position_of(u16::MAX)];
    let mut pos: Vec<Option<(&[u16], bool)>> = vec![None; operands.len()];
    let mut res: f32 = -1.0;
    for (i, &(word, prefix)) in operands.iter().enumerate() {
        for lexeme in &lexemes[find(lexemes, word, prefix)] {
            let post: (&[u16], bool) = if lexeme.positions.is_empty() {
                (&none, true)
            } else {
                (&lexeme.positions, false)
            };
            pos[i] = Some(post);
            for ct in pos[..i].iter().flatten() {
                for &l in post.0 {
                    for &p in ct.0 {
                        let mut dist =
                            (i32::from(position_of(l)) - i32::from(position_of(p))).abs();
                        if dist != 0 || post.1 || ct.1 {
                            if dist == 0 {
                                dist = MAX_ENTRY_POS;
                            }
                            let curw = f64::from(
                                wpos(weights, l) * wpos(weights, p) * word_distance(dist),
                            )
                            .sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                (1.0 - (1.0 - f64::from(res)) * (1.0 - f64::from(curw))) as f32
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

/// `calc_rank_or`: the mean over the operands of a harmonic sum of the weights of their positions.
#[allow(clippy::cast_precision_loss)]
fn rank_or(weights: &[f32; 4], lexemes: &[Lexeme], items: &[QueryItem]) -> f32 {
    let operands = operands(items);
    // A lexeme with no positions counts as one position of the weight `D`.
    let none = [0];
    let mut res: f32 = 0.0;
    for &(word, prefix) in &operands {
        for lexeme in &lexemes[find(lexemes, word, prefix)] {
            let post: &[u16] = if lexeme.positions.is_empty() { &none } else { &lexeme.positions };
            let mut resj: f32 = 0.0;
            let mut wjm: f32 = -1.0;
            let mut jm = 0;
            for (j, &pos) in post.iter().enumerate() {
                let w = wpos(weights, pos);
                resj += w / ((j + 1) * (j + 1)) as f32;
                if w > wjm {
                    wjm = w;
                    jm = j;
                }
            }
            let term = resj - wjm / ((jm + 1) * (jm + 1)) as f32 + wjm;
            res = (f64::from(res) + f64::from(term) / 1.644_934_066_85) as f32;
        }
    }
    if !operands.is_empty() {
        res /= operands.len() as f32;
    }
    res
}

/// `calc_rank`: the rank of `ts_rank` with the normalization of `method`.
#[allow(clippy::cast_precision_loss)]
fn rank(weights: &[f32; 4], lexemes: &[Lexeme], items: &[QueryItem], method: i32) -> f32 {
    if lexemes.is_empty() || items.is_empty() {
        return 0.0;
    }
    let mut res = match items[0] {
        QueryItem::Operator { op: TSQUERY_AND | TSQUERY_PHRASE, .. } => {
            rank_and(weights, lexemes, items)
        }
        _ => rank_or(weights, lexemes, items),
    };
    if res < 0.0 {
        res = 1e-20;
    }
    if method & NORM_LOG_LENGTH != 0 {
        res = (f64::from(res) / (f64::from(cnt_length(lexemes) + 1).ln() / 2.0_f64.ln())) as f32;
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(lexemes);
        if len > 0 {
            res /= len as f32;
        }
    }
    if method & NORM_UNIQ != 0 {
        res /= size(lexemes) as f32;
    }
    if method & NORM_LOG_UNIQ != 0 {
        res = (f64::from(res) / (f64::from(size(lexemes) + 1).ln() / 2.0_f64.ln())) as f32;
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        res /= res + 1.0;
    }
    res
}

/// The error for a weight that is more than one.
fn weight_out_of_range() -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, "weight out of range")
}

/// `getWeights`: the four weights of an array, with the default weight for a negative value.
fn weights_of(array: &Array<Value>) -> Result<[f32; 4]> {
    if array.dims.len() != 1 {
        return Err(Error::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "array of weight must be one-dimensional",
        ));
    }
    if array.values.len() < DEFAULT_WEIGHTS.len() {
        return Err(Error::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "array of weight is too short"));
    }
    if array.values.iter().any(Option::is_none) {
        return Err(Error::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "array of weight must not contain nulls",
        ));
    }
    let mut out = DEFAULT_WEIGHTS;
    for (weight, value) in out.iter_mut().zip(&array.values) {
        let Some(Value::Float4(value)) = value else { return Err(bad_value()) };
        if *value >= 0.0 {
            *weight = *value;
        }
        if *weight > 1.0 {
            return Err(weight_out_of_range());
        }
    }
    Ok(out)
}

/// The weights, the vector, the query and the method of a call of `ts_rank` or `ts_rank_cd`.
type RankArgs<'a> = ([f32; 4], &'a [Lexeme], &'a [QueryItem], i32);

/// The arguments of the forms of `ts_rank` and `ts_rank_cd`: the weights, the vector, the query and the method, each one optional at the same places as in PostgreSQL.
fn rank_args(args: &[Value]) -> Result<RankArgs<'_>> {
    let (weights, args) = match args {
        [Value::Array(array), rest @ ..] => (weights_of(array)?, rest),
        _ => (DEFAULT_WEIGHTS, args),
    };
    match args {
        [Value::TsVector(lexemes), Value::TsQuery(items)] => Ok((weights, lexemes, items, 0)),
        [Value::TsVector(lexemes), Value::TsQuery(items), Value::Int4(method)] => {
            Ok((weights, lexemes, items, *method))
        }
        _ => Err(bad_value()),
    }
}

/// `ts_rank_wttf`, `ts_rank_wtt`, `ts_rank_ttf` and `ts_rank_tt`.
fn ts_rank(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (weights, lexemes, items, method) = rank_args(args)?;
    Ok(Value::Float4(rank(&weights, lexemes, items, method)))
}

/// `DocRepresentation`: a position of the vector with the operands of the query that match it.
#[derive(Debug)]
struct Doc {
    pos: u16,
    /// The index of the lexeme.
    entry: usize,
    /// The indexes of the operands in the query.
    items: Vec<usize>,
}

/// `get_docrep`: the positions of the vector that match an operand, in order of the position, the weight and the lexeme. A lexeme with no positions does not count.
fn doc_rep(lexemes: &[Lexeme], items: &[QueryItem]) -> Vec<Doc> {
    let mut raw: Vec<(u16, usize, usize)> = Vec::new();
    for (at, item) in items.iter().enumerate() {
        let QueryItem::Operand { word, weight, prefix } = item else { continue };
        let range = find(lexemes, word, *prefix);
        for (entry, lexeme) in lexemes[range.clone()].iter().enumerate() {
            for &pos in &lexeme.positions {
                if *weight == 0 || weight & (1 << weight_of(pos)) != 0 {
                    raw.push((pos, range.start + entry, at));
                }
            }
        }
    }
    raw.sort_by_key(|&(pos, entry, _)| (position_of(pos), weight_of(pos), entry));
    let mut doc: Vec<Doc> = Vec::new();
    for (pos, entry, at) in raw {
        match doc.last_mut() {
            Some(last) if last.pos == pos && last.entry == entry => last.items.push(at),
            _ => doc.push(Doc { pos, entry, items: vec![at] }),
        }
    }
    doc
}

/// `QueryRepresentationOperand`: the positions of the cover where an operand matches, in the order of the scan.
#[derive(Debug, Default, Clone)]
struct Operand {
    exists: bool,
    pos: Vec<u16>,
}

/// `QueryRepresentation`: the operands of the query in a part of the vector. `reverse` is true for a scan from right to left.
#[derive(Debug)]
struct Repr {
    operands: Vec<Operand>,
    reverse: bool,
}

impl Repr {
    /// `resetQueryRepresentation`.
    fn reset(&mut self, reverse: bool) {
        self.operands.fill(Operand::default());
        self.reverse = reverse;
    }

    /// `fillQueryRepresentationData`: adds a position of the vector to its operands.
    fn fill(&mut self, doc: &Doc) {
        for &at in &doc.items {
            let operand = &mut self.operands[at];
            operand.exists = true;
            if operand.pos.last().is_none_or(|&last| position_of(last) != position_of(doc.pos)) {
                operand.pos.push(doc.pos);
            }
        }
    }

    /// `TS_execute` with `checkcondition_QueryOperand`: true when the operands that the part has match the query.
    fn matches(&self, items: &[QueryItem]) -> bool {
        matches_with(items, &|at, data: Option<&mut Phrase>| {
            let operand = &self.operands[at];
            if !operand.exists {
                return Ternary::No;
            }
            if let Some(data) = data {
                data.pos.clone_from(&operand.pos);
                if self.reverse {
                    data.pos.reverse();
                }
            }
            Ternary::Yes
        })
    }
}

/// `CoverExt`: a cover, which is a shortest part of the vector that matches the query.
#[derive(Debug)]
struct Cover {
    begin: usize,
    end: usize,
    p: i32,
    q: i32,
}

/// `Cover`: the next cover from the index `from`, which moves to the index after the start of the cover.
fn cover(doc: &[Doc], items: &[QueryItem], repr: &mut Repr, from: &mut usize) -> Option<Cover> {
    loop {
        repr.reset(false);
        let mut q = 0;
        let mut end = None;
        for (at, entry) in doc.iter().enumerate().skip(*from) {
            repr.fill(entry);
            if repr.matches(items) {
                let pos = i32::from(position_of(entry.pos));
                if pos > q {
                    q = pos;
                    end = Some(at);
                }
                break;
            }
        }
        let end = end?;
        repr.reset(true);
        let mut p = i32::MAX;
        let mut begin = None;
        for at in (*from..=end).rev() {
            repr.fill(&doc[at]);
            if repr.matches(items) {
                let pos = i32::from(position_of(doc[at].pos));
                if pos < p {
                    p = pos;
                }
                begin = Some(at);
                break;
            }
        }
        if let Some(begin) = begin
            && p <= q
        {
            *from = begin + 1;
            return Some(Cover { begin, end, p, q });
        }
        *from += 1;
    }
}

/// `calc_rank_cd`: the rank of `ts_rank_cd` by the covers of the query in the vector, with the normalization of `method`.
#[allow(clippy::cast_precision_loss)]
fn rank_cd(
    weights: &[f32; 4],
    lexemes: &[Lexeme],
    items: &[QueryItem],
    method: i32,
) -> Result<f32> {
    let mut invws = [0.0; 4];
    for (inv, (&weight, &default)) in invws.iter_mut().zip(weights.iter().zip(&DEFAULT_WEIGHTS)) {
        *inv = f64::from(if weight >= 0.0 { weight } else { default });
        if *inv > 1.0 {
            return Err(weight_out_of_range());
        }
        *inv = 1.0 / *inv;
    }
    let doc = doc_rep(lexemes, items);
    if doc.is_empty() {
        return Ok(0.0);
    }
    let mut repr = Repr { operands: vec![Operand::default(); items.len()], reverse: false };
    let mut from = 0;
    let mut wdoc = 0.0;
    let mut sum_dist = 0.0;
    let mut prev_ext_pos = 0.0;
    let mut extents = 0;
    while let Some(ext) = cover(&doc, items, &mut repr, &mut from) {
        let inv_sum: f64 = doc[ext.begin..=ext.end]
            .iter()
            .map(|entry| invws[usize::from(weight_of(entry.pos))])
            .sum();
        let width = i32::try_from(ext.end - ext.begin).unwrap_or(i32::MAX);
        let cpos = f64::from(width + 1) / inv_sum;
        let mut noise = (ext.q - ext.p) - width;
        if noise < 0 {
            noise = width / 2;
        }
        wdoc += cpos / f64::from(1 + noise);
        let cur_ext_pos = f64::from(ext.q + ext.p) / 2.0;
        if extents > 0 && cur_ext_pos > prev_ext_pos {
            sum_dist += 1.0 / (cur_ext_pos - prev_ext_pos);
        }
        prev_ext_pos = cur_ext_pos;
        extents += 1;
    }
    if method & NORM_LOG_LENGTH != 0 {
        wdoc /= f64::from(cnt_length(lexemes) + 1).ln();
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(lexemes);
        if len > 0 {
            wdoc /= f64::from(len);
        }
    }
    if method & NORM_EXT_DIST != 0 && extents > 0 && sum_dist > 0.0 {
        wdoc /= f64::from(extents) / sum_dist;
    }
    if method & NORM_UNIQ != 0 {
        wdoc /= f64::from(size(lexemes));
    }
    if method & NORM_LOG_UNIQ != 0 {
        wdoc /= f64::from(size(lexemes) + 1).ln() / 2.0_f64.ln();
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        wdoc /= wdoc + 1.0;
    }
    Ok(wdoc as f32)
}

/// `ts_rankcd_wttf`, `ts_rankcd_wtt`, `ts_rankcd_ttf` and `ts_rankcd_tt`.
fn ts_rank_cd(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (weights, lexemes, items, method) = rank_args(args)?;
    Ok(Value::Float4(rank_cd(&weights, lexemes, items, method)?))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "ts_rank_wttf" | "ts_rank_wtt" | "ts_rank_ttf" | "ts_rank_tt" => ts_rank,
        "ts_rankcd_wttf" | "ts_rankcd_wtt" | "ts_rankcd_ttf" | "ts_rankcd_tt" => ts_rank_cd,
        _ => return None,
    })
}

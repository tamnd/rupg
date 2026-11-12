//! `contrib/pg_trgm`: the similarity of strings by their trigrams. A port of `trgm_op.c`.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_ext::CustomSetting;
use rupg_func::{Call, Kernel, Session};
use rupg_types::{Array, Value};

use crate::support::index_support;

/// The settings that `_PG_init` of `trgm_op.c` defines.
pub(crate) static SETTINGS: &[CustomSetting] = &[
    CustomSetting {
        name: "pg_trgm.similarity_threshold",
        short_desc: "Sets the threshold used by the % operator.",
        long_desc: "Valid range is 0.0 .. 1.0.",
        default: 0.3,
        min: 0.0,
        max: 1.0,
    },
    CustomSetting {
        name: "pg_trgm.word_similarity_threshold",
        short_desc: "Sets the threshold used by the <% operator.",
        long_desc: "Valid range is 0.0 .. 1.0.",
        default: 0.6,
        min: 0.0,
        max: 1.0,
    },
    CustomSetting {
        name: "pg_trgm.strict_word_similarity_threshold",
        short_desc: "Sets the threshold used by the <<% operator.",
        long_desc: "Valid range is 0.0 .. 1.0.",
        default: 0.5,
        min: 0.0,
        max: 1.0,
    },
];

/// The C symbols of `pg_trgm` and their kernels.
pub(crate) static SYMBOLS: &[(&str, Kernel)] = &[
    ("gin_extract_query_trgm", index_support),
    ("gin_extract_value_trgm", index_support),
    ("gin_trgm_consistent", index_support),
    ("gin_trgm_triconsistent", index_support),
    ("gtrgm_compress", index_support),
    ("gtrgm_consistent", index_support),
    ("gtrgm_decompress", index_support),
    ("gtrgm_distance", index_support),
    ("gtrgm_in", gtrgm_in),
    ("gtrgm_options", index_support),
    ("gtrgm_out", gtrgm_out),
    ("gtrgm_penalty", index_support),
    ("gtrgm_picksplit", index_support),
    ("gtrgm_same", index_support),
    ("gtrgm_union", index_support),
    ("set_limit", set_limit),
    ("show_limit", show_limit),
    ("show_trgm", show_trgm),
    ("similarity", similarity),
    ("similarity_dist", similarity_dist),
    ("similarity_op", similarity_op),
    ("strict_word_similarity", strict_word_similarity),
    ("strict_word_similarity_commutator_op", strict_word_similarity_commutator_op),
    ("strict_word_similarity_dist_commutator_op", strict_word_similarity_dist_commutator_op),
    ("strict_word_similarity_dist_op", strict_word_similarity_dist_op),
    ("strict_word_similarity_op", strict_word_similarity_op),
    ("word_similarity", word_similarity),
    ("word_similarity_commutator_op", word_similarity_commutator_op),
    ("word_similarity_dist_commutator_op", word_similarity_dist_commutator_op),
    ("word_similarity_dist_op", word_similarity_dist_op),
    ("word_similarity_op", word_similarity_op),
];

/// `LPADDING`: the spaces before each word.
const LPADDING: usize = 2;
/// `RPADDING`: the spaces after each word.
const RPADDING: usize = 1;

/// `TRGM_BOUND_LEFT`: the trigram is the first one of a word.
const BOUND_LEFT: u8 = 0x01;
/// `TRGM_BOUND_RIGHT`: the trigram is the last one of a word.
const BOUND_RIGHT: u8 = 0x02;

/// `WORD_SIMILARITY_CHECK_ONLY`: only whether the similarity reaches the threshold counts, not its maximum.
const CHECK_ONLY: u8 = 0x01;
/// `WORD_SIMILARITY_STRICT`: the bounds of the extent are the bounds of words.
const STRICT: u8 = 0x02;

/// A trigram: three single-byte characters, or the hash of three characters with more bytes.
type Trgm = [u8; 3];

/// The character classes and the case mapping of the database, which `t_isalnum_with_len` and `str_tolower` with `DEFAULT_COLLATION_OID` read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ctype {
    /// The C locale: only the ASCII letters and digits are word characters, and only the ASCII letters change case.
    C,
    /// A locale with the classes of Unicode.
    Unicode,
}

impl Ctype {
    /// The ctype of the database of the session. The databases of `initdb` have the C locale.
    pub(crate) fn of(session: &dyn Session) -> Ctype {
        let name = session.database();
        let databases = session.databases();
        let Some(database) = databases.iter().find(|d| d.name == name) else { return Ctype::C };
        let unicode = match database.locale_provider {
            b'i' => true,
            b'b' => database.locale.as_deref().is_some_and(|l| l != "C"),
            _ => !matches!(database.ctype.as_str(), "C" | "POSIX"),
        };
        if unicode { Ctype::Unicode } else { Ctype::C }
    }

    /// `ISWORDCHR`.
    fn is_word(self, c: char) -> bool {
        match self {
            Ctype::C => c.is_ascii_alphanumeric(),
            Ctype::Unicode => c.is_alphanumeric(),
        }
    }

    /// `str_tolower`.
    fn lower(self, word: &str) -> String {
        match self {
            Ctype::C => word.to_ascii_lowercase(),
            Ctype::Unicode => word.to_lowercase(),
        }
    }
}

/// `CMPTRGM` with `GetDefaultCharSignedness` true, as on x86: the bytes compare as `signed char`.
fn cmp_trgm(a: &Trgm, b: &Trgm) -> Ordering {
    let signed = |t: &Trgm| t.map(u8::cast_signed);
    signed(a).cmp(&signed(b))
}

/// `compact_trigram`: the three characters of `chars`, as they are when they have one byte each, else the first three bytes in memory of the legacy CRC-32 of their bytes.
fn compact_trigram(chars: &[u8]) -> Trgm {
    if let [a, b, c] = *chars {
        return [a, b, c];
    }
    let crc = rupg_types::legacy_crc32(chars).cast_unsigned();
    let [a, b, c, _] = crc.to_le_bytes();
    [a, b, c]
}

/// `make_trigrams`: the trigrams of `word`, a word with its padding, one for each three characters in a row.
fn make_trigrams(dst: &mut Vec<Trgm>, word: &str) {
    let starts: Vec<usize> = word.char_indices().map(|(at, _)| at).chain([word.len()]).collect();
    for window in starts.windows(4) {
        dst.push(compact_trigram(&word.as_bytes()[window[0]..window[3]]));
    }
}

/// `generate_trgm_only`: the trigrams of the words of `text` in order, with duplicates, and with the bounds of the words when `bounds` is true.
fn generate_trgm_only(text: &str, ctype: Ctype, bounds: bool) -> (Vec<Trgm>, Vec<u8>) {
    let mut trgms = Vec::new();
    let mut marks = Vec::new();
    for word in text.split(|c| !ctype.is_word(c)).filter(|w| !w.is_empty()) {
        let mut padded = " ".repeat(LPADDING);
        padded.push_str(&ctype.lower(word));
        padded.push_str(&" ".repeat(RPADDING));
        let old = trgms.len();
        make_trigrams(&mut trgms, &padded);
        if bounds && trgms.len() > old {
            marks.resize(trgms.len(), 0);
            marks[old] |= BOUND_LEFT;
            marks[trgms.len() - 1] |= BOUND_RIGHT;
        }
    }
    (trgms, marks)
}

/// `generate_trgm`: the trigrams of `text` in order, without duplicates.
pub(crate) fn generate_trgm(text: &str, ctype: Ctype) -> Vec<Trgm> {
    let (mut trgms, _) = generate_trgm_only(text, ctype, false);
    trgms.sort_by(cmp_trgm);
    trgms.dedup();
    trgms
}

/// `CALCSML` with `DIVUNION`.
#[allow(clippy::cast_precision_loss)]
fn calc_sml(count: usize, len1: usize, len2: usize) -> f32 {
    count as f32 / (len1 + len2 - count) as f32
}

/// `cnt_sml` of two exact sets of trigrams.
pub(crate) fn cnt_sml(trg1: &[Trgm], trg2: &[Trgm]) -> f32 {
    if trg1.is_empty() || trg2.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < trg1.len() && j < trg2.len() {
        match cmp_trgm(&trg1[i], &trg2[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                i += 1;
                j += 1;
                count += 1;
            }
        }
    }
    calc_sml(count, trg1.len(), trg2.len())
}

/// The thresholds of the three operators, as the settings give them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Thresholds {
    similarity: f64,
    word: f64,
    strict: f64,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds {
            similarity: SETTINGS[0].default,
            word: SETTINGS[1].default,
            strict: SETTINGS[2].default,
        }
    }
}

impl Thresholds {
    fn of(session: &dyn Session) -> Thresholds {
        let read = |at: usize| {
            let setting = &SETTINGS[at];
            session
                .setting(setting.name)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(setting.default)
        };
        Thresholds { similarity: read(0), word: read(1), strict: read(2) }
    }
}

/// `iterate_word_similarity`: the greatest similarity of `ulen1` trigrams of the pattern with an extent of the text.
fn iterate_word_similarity(
    trg2indexes: &[usize],
    found: &[bool],
    ulen1: usize,
    flags: u8,
    bounds: &[u8],
    thresholds: Thresholds,
) -> f32 {
    let strict = flags & STRICT != 0;
    let check_only = flags & CHECK_ONLY != 0;
    let threshold = if strict { thresholds.strict } else { thresholds.word };
    let len2 = trg2indexes.len();
    let mut lastpos: Vec<Option<usize>> = vec![None; found.len()];
    let mut ulen2 = 0;
    let mut count = 0;
    let mut smlr_max = 0.0f32;
    let mut lower: Option<usize> = if strict { Some(0) } else { None };
    for i in 0..len2 {
        let trgindex = trg2indexes[i];
        if lower.is_some() || found[trgindex] {
            if lastpos[trgindex].is_none() {
                ulen2 += 1;
                if found[trgindex] {
                    count += 1;
                }
            }
            lastpos[trgindex] = Some(i);
        }
        let upper_bound = if strict { bounds[i] & BOUND_RIGHT != 0 } else { found[trgindex] };
        if !upper_bound {
            continue;
        }
        let upper = i;
        // The first trigram of the pattern is the first lower bound of the plain word similarity.
        let low = lower.unwrap_or_else(|| {
            ulen2 = 1;
            i
        });
        let mut smlr_cur = calc_sml(count, ulen1, ulen2);
        let mut tmp_count = count;
        let mut tmp_ulen2 = ulen2;
        let prev_lower = low;
        let mut new_lower = low;
        for tmp_lower in low..=upper {
            if !strict || bounds[tmp_lower] & BOUND_LEFT != 0 {
                let smlr_tmp = calc_sml(tmp_count, ulen1, tmp_ulen2);
                if smlr_tmp > smlr_cur {
                    smlr_cur = smlr_tmp;
                    ulen2 = tmp_ulen2;
                    new_lower = tmp_lower;
                    count = tmp_count;
                }
                if check_only && f64::from(smlr_cur) >= threshold {
                    break;
                }
            }
            let tmp_trgindex = trg2indexes[tmp_lower];
            if lastpos[tmp_trgindex] == Some(tmp_lower) {
                tmp_ulen2 -= 1;
                if found[tmp_trgindex] {
                    tmp_count -= 1;
                }
            }
        }
        lower = Some(new_lower);
        smlr_max = smlr_max.max(smlr_cur);
        if check_only && f64::from(smlr_max) >= threshold {
            break;
        }
        for (tmp_lower, &tmp_trgindex) in
            trg2indexes.iter().enumerate().take(new_lower).skip(prev_lower)
        {
            if lastpos[tmp_trgindex] == Some(tmp_lower) {
                lastpos[tmp_trgindex] = None;
            }
        }
    }
    smlr_max
}

/// `calc_word_similarity`: the similarity of the pattern `str1` with the extent of the text `str2` most like it.
pub(crate) fn calc_word_similarity(
    str1: &str,
    str2: &str,
    flags: u8,
    ctype: Ctype,
    thresholds: Thresholds,
) -> f32 {
    let (trg1, _) = generate_trgm_only(str1, ctype, false);
    let (trg2, bounds) = generate_trgm_only(str2, ctype, flags & STRICT != 0);
    // `make_positional_trgm`: the pattern's trigrams have no position.
    let mut ptrg: Vec<(Trgm, Option<usize>)> = trg1
        .iter()
        .map(|t| (*t, None))
        .chain(trg2.iter().enumerate().map(|(i, t)| (*t, Some(i))))
        .collect();
    if ptrg.is_empty() {
        return 0.0;
    }
    // `comp_ptrgm`: by the trigram, then by the position, where no position is -1.
    ptrg.sort_by(|a, b| cmp_trgm(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
    let mut trg2indexes = vec![0; trg2.len()];
    let mut found = vec![false; ptrg.len()];
    let mut ulen1 = 0;
    let mut j = 0;
    for i in 0..ptrg.len() {
        if i > 0 && cmp_trgm(&ptrg[i - 1].0, &ptrg[i].0) != Ordering::Equal {
            if found[j] {
                ulen1 += 1;
            }
            j += 1;
        }
        match ptrg[i].1 {
            Some(index) => trg2indexes[index] = j,
            None => found[j] = true,
        }
    }
    if found[j] {
        ulen1 += 1;
    }
    iterate_word_similarity(&trg2indexes, &found, ulen1, flags, &bounds, thresholds)
}

/// `ISPRINTABLECHAR`.
fn is_printable(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b' '
}

/// The text of a trigram as `show_trgm` gives it: the three characters, or `0x` and six hexadecimal digits of `trgm2int` for a hash.
pub(crate) fn trgm_text(t: &Trgm) -> String {
    if t.iter().all(|b| is_printable(*b)) {
        return t.iter().map(|b| char::from(*b)).collect();
    }
    let value = u32::from(t[0]) << 16 | u32::from(t[1]) << 8 | u32::from(t[2]);
    format!("0x{value:06x}")
}

fn text_arg(value: &Value) -> Result<&str> {
    match value {
        Value::Text(s) => Ok(s),
        _ => Err(Error::internal("a function got a value of the wrong kind")),
    }
}

fn two_texts(args: &[Value]) -> Result<(&str, &str)> {
    let [a, b] = args else {
        return Err(Error::internal("a function got a value of the wrong kind"));
    };
    Ok((text_arg(a)?, text_arg(b)?))
}

/// `set_limit`: sets `pg_trgm.similarity_threshold` to the text of the `float4`, and gives the threshold.
fn set_limit(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float4(limit)] = args else {
        return Err(Error::internal("a function got a value of the wrong kind"));
    };
    let mut text = Vec::new();
    rupg_types::float4_out(*limit, call.session.extra_float_digits(), &mut text);
    let text = String::from_utf8_lossy(&text);
    call.session.set_setting(SETTINGS[0].name, Some(&text), false)?;
    show_limit(call, &[])
}

/// `show_limit`.
#[allow(clippy::cast_possible_truncation)]
fn show_limit(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Float4(Thresholds::of(call.session).similarity as f32))
}

/// `show_trgm`: the trigrams of a text as an array of texts.
fn show_trgm(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text] = args else {
        return Err(Error::internal("a function got a value of the wrong kind"));
    };
    let trgms = generate_trgm(text_arg(text)?, Ctype::of(call.session));
    let values = trgms.iter().map(|t| Some(Value::Text(trgm_text(t)))).collect();
    Ok(Value::Array(Box::new(Array::one(values))))
}

/// `similarity` without the call.
fn sml(call: &Call<'_>, args: &[Value]) -> Result<f32> {
    let (a, b) = two_texts(args)?;
    let ctype = Ctype::of(call.session);
    Ok(cnt_sml(&generate_trgm(a, ctype), &generate_trgm(b, ctype)))
}

fn similarity(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    sml(call, args).map(Value::Float4)
}

fn similarity_dist(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(1.0 - sml(call, args)?))
}

fn similarity_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let res = sml(call, args)?;
    Ok(Value::Bool(f64::from(res) >= Thresholds::of(call.session).similarity))
}

/// `calc_word_similarity` of the two texts of a call, in the order of the arguments or the other order when `commute` is true.
fn word_sml(
    call: &Call<'_>,
    args: &[Value],
    flags: u8,
    commute: bool,
) -> Result<(f32, Thresholds)> {
    let (a, b) = two_texts(args)?;
    let (pattern, text) = if commute { (b, a) } else { (a, b) };
    let thresholds = Thresholds::of(call.session);
    Ok((
        calc_word_similarity(pattern, text, flags, Ctype::of(call.session), thresholds),
        thresholds,
    ))
}

fn word_similarity(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(word_sml(call, args, 0, false)?.0))
}

fn strict_word_similarity(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(word_sml(call, args, STRICT, false)?.0))
}

fn word_similarity_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (res, t) = word_sml(call, args, CHECK_ONLY, false)?;
    Ok(Value::Bool(f64::from(res) >= t.word))
}

fn word_similarity_commutator_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (res, t) = word_sml(call, args, CHECK_ONLY, true)?;
    Ok(Value::Bool(f64::from(res) >= t.word))
}

fn word_similarity_dist_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(1.0 - word_sml(call, args, 0, false)?.0))
}

fn word_similarity_dist_commutator_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(1.0 - word_sml(call, args, 0, true)?.0))
}

fn strict_word_similarity_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (res, t) = word_sml(call, args, CHECK_ONLY | STRICT, false)?;
    Ok(Value::Bool(f64::from(res) >= t.strict))
}

fn strict_word_similarity_commutator_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (res, t) = word_sml(call, args, CHECK_ONLY | STRICT, true)?;
    Ok(Value::Bool(f64::from(res) >= t.strict))
}

fn strict_word_similarity_dist_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(1.0 - word_sml(call, args, STRICT, false)?.0))
}

fn strict_word_similarity_dist_commutator_op(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float4(1.0 - word_sml(call, args, STRICT, true)?.0))
}

/// `gtrgm_in`: the type `gtrgm` has no text form.
fn gtrgm_in(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot accept a value of type gtrgm"))
}

/// `gtrgm_out`: the type `gtrgm` has no text form.
fn gtrgm_out(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot display a value of type gtrgm"))
}

#[cfg(test)]
#[path = "test_session.rs"]
pub(crate) mod test_session;

#[cfg(test)]
mod tests {
    use rupg_types::oid;

    use super::test_session::TestSession;
    use super::*;

    fn show(text: &str) -> Vec<String> {
        generate_trgm(text, Ctype::C).iter().map(trgm_text).collect()
    }

    fn texts(a: &str, b: &str) -> [Value; 2] {
        [Value::Text(a.to_string()), Value::Text(b.to_string())]
    }

    #[test]
    fn trigrams() {
        assert!(show("").is_empty());
        assert!(show("(*&^$@%@").is_empty());
        assert_eq!(show("a b c"), ["  a", "  b", "  c", " a ", " b ", " c "]);
        assert_eq!(
            show(" aA bB cC "),
            ["  a", "  b", "  c", " aa", " bb", " cc", "aa ", "bb ", "cc "]
        );
        assert_eq!(show("a b C0*%^"), ["  a", "  b", "  c", " a ", " b ", " c0", "c0 "]);
        // A trigram of characters with more than one byte is a hash.
        let unicode = generate_trgm("é", Ctype::Unicode);
        assert_eq!(unicode.len(), 2);
        assert!(unicode.iter().all(|t| trgm_text(t).starts_with("0x")));
        assert!(generate_trgm("é", Ctype::C).is_empty());
    }

    #[test]
    fn similarities() {
        let sml =
            |a: &str, b: &str| cnt_sml(&generate_trgm(a, Ctype::C), &generate_trgm(b, Ctype::C));
        assert!((sml("wow", "WOWa ") - 0.5).abs() < 1e-6);
        assert!((sml("wow", " WOW ") - 1.0).abs() < 1e-6);
        assert!(sml("---", "####---").abs() < 1e-6);
        assert!(sml("Szczecin", "Warsaw").abs() < 1e-6);
        assert!((sml("qwertyu0988", "gwertyu0988") - 0.6).abs() < 1e-6);
        assert!((sml("qwertyu0988", "gwertyu1988") - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn word_similarities() {
        let t = Thresholds::default();
        let word = |a: &str, b: &str| calc_word_similarity(a, b, 0, Ctype::C, t);
        let strict = |a: &str, b: &str| calc_word_similarity(a, b, STRICT, Ctype::C, t);
        let close = |x: f32, y: f64| (f64::from(x) - y).abs() < 1e-6;
        assert!(close(word("Baykal", "Boloto Baykal"), 1.0));
        assert!(close(word("Baykal", "Baykalikha"), 0.857_143));
        assert!(close(word("Baykal", "Zabaykal"), 0.714_286));
        assert!(close(word("Kabankala", "Kabankalan City Public Plaza"), 0.9));
        assert!(close(word("Kabankala", "Abankala"), 0.7));
        assert!(close(word("Kabankala", "Ntombankala School"), 0.6));
        assert!(close(strict("Baykal", "Boloto Baykal"), 1.0));
        assert!(close(strict("Baykal", "Baykalo-Amurskaya Zheleznaya Doroga"), 0.666_667));
        assert!(close(strict("Baykal", "Baykalovo"), 0.545_455));
        assert!(close(strict("Baykal", "Baykalikha"), 0.5));
        assert!(close(strict("Kabankala", "Kabankalan City Public Plaza"), 0.75));
        assert!(close(strict("Kabankala", "Kabakala"), 0.583_333));
        assert!(close(word("", "abc"), 0.0));
        assert!(close(word("abc", ""), 0.0));
    }

    #[test]
    fn kernels() {
        let session = TestSession::default();
        let text2 = [oid::TEXT, oid::TEXT];
        let value = session.call(show_limit, &[], oid::FLOAT4, &[]).unwrap();
        assert_eq!(value, Value::Float4(0.3));
        let value =
            session.call(set_limit, &[oid::FLOAT4], oid::FLOAT4, &[Value::Float4(0.5)]).unwrap();
        assert_eq!(value, Value::Float4(0.5));
        assert_eq!(session.setting("pg_trgm.similarity_threshold").as_deref(), Some("0.5"));
        let op =
            |k: Kernel, a: &str, b: &str| session.call(k, &text2, oid::BOOL, &texts(a, b)).unwrap();
        assert_eq!(op(similarity_op, "wow", "WOWa "), Value::Bool(true));
        assert_eq!(op(similarity_op, "qwertyu0988", "gwertyu1988"), Value::Bool(false));
        assert_eq!(op(word_similarity_op, "Kabankala", "Abankala"), Value::Bool(true));
        assert_eq!(op(word_similarity_op, "Kabankala", "Ntombankala School"), Value::Bool(true));
        assert_eq!(
            op(word_similarity_commutator_op, "Ntombankala School", "Kabankala"),
            Value::Bool(true)
        );
        assert_eq!(op(strict_word_similarity_op, "Kabankala", "Abankala"), Value::Bool(true));
        assert_eq!(op(strict_word_similarity_op, "Baykal", "Baykalovo"), Value::Bool(true));
        assert_eq!(
            op(strict_word_similarity_commutator_op, "Baykalovo", "Baykal"),
            Value::Bool(true)
        );
        session
            .set_setting("pg_trgm.strict_word_similarity_threshold", Some("0.6"), false)
            .unwrap();
        assert_eq!(op(strict_word_similarity_op, "Baykal", "Baykalovo"), Value::Bool(false));
        let dist =
            session.call(similarity_dist, &text2, oid::FLOAT4, &texts("wow", "WOWa ")).unwrap();
        assert_eq!(dist, Value::Float4(0.5));
        let dist = session.call(
            word_similarity_dist_commutator_op,
            &text2,
            oid::FLOAT4,
            &texts("Abankala", "Kabankala"),
        );
        assert!(matches!(dist.unwrap(), Value::Float4(d) if (d - 0.3).abs() < 1e-6));
        let shown = session
            .call(show_trgm, &[oid::TEXT], oid::TEXT_ARRAY, &[Value::Text("a".into())])
            .unwrap();
        let Value::Array(array) = shown else { panic!("not an array") };
        assert_eq!(
            array.values,
            [Some(Value::Text("  a".into())), Some(Value::Text(" a ".into()))]
        );
        let err =
            session.call(gtrgm_out, &[oid::INTERNAL], oid::CSTRING, &[Value::Null]).unwrap_err();
        assert_eq!(err.message(), "cannot display a value of type gtrgm");
    }
}

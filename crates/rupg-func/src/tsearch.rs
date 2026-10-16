//! The functions of text search that use the parser: `ts_parse` and `ts_token_type` of `wparser.c`, `to_tsvector`, `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery` and `websearch_to_tsquery` of `to_tsany.c` with `parsetext` of `ts_parse.c`, `get_current_ts_config` of `ts_utils.c`, and `ts_match_tq` and `ts_match_tt` of `tsvector_op.c`. A function without a configuration uses the configuration of `default_text_search_config`, as `getTSCurrentConfig` finds it.

use rupg_analyze::tsdict::{self, Config};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::TOKEN_TYPES;
use rupg_pgcatalog::builtin::{self, Named};
use rupg_types::{
    Lexeme, QueryItem, RegKind, TSQUERY_AND, TSQUERY_PHRASE, TSVECTOR_MAX_DATA,
    TSVECTOR_MAX_POSITIONS, TSVECTOR_MAX_WORD, TsQueryMode, TsQueryPush, TypeError, Value,
    limit_position, ts_compare_string, tsquery_parse_with, tsvector_data_len,
};

use crate::{Call, Kernel, Session, bad_value, reg, tsparser};

/// `MAXENTRYPOS - 1`: the largest position.
const MAX_POSITION: u16 = (1 << 14) - 1;

/// `ParsedText`: the lexemes of `parsetext` with their positions, and the position of the last word.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    words: Vec<(String, u16)>,
    pos: i32,
}

/// The notice of `parsetext` for a word or a lexeme that is longer than `MAXSTRLEN`.
fn too_long_word() -> Error {
    Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "word is too long to be indexed")
        .with_detail(format!("Words longer than {TSVECTOR_MAX_WORD} characters are ignored."))
        .as_notice()
}

/// `parsetext`: the tokens of the parser, each through the dictionaries of its type in order. The first dictionary that knows the word gives the lexemes at the next position. A stop word takes a position and gives no lexemes. A token of a type with no dictionaries takes no position.
pub(crate) fn parse_text(session: &dyn Session, config: &Config, text: &str, out: &mut Parsed) {
    for (token, word) in tsparser::tokens(text) {
        if word.len() > TSVECTOR_MAX_WORD {
            session.warn(too_long_word());
            continue;
        }
        let Some(dicts) = config.map.get(usize::from(token)) else { continue };
        let Some(lexemes) = dicts.iter().find_map(|dict| dict.lexize(word)) else { continue };
        out.pos += 1;
        for lexeme in lexemes {
            if lexeme.len() > TSVECTOR_MAX_WORD {
                session.warn(too_long_word());
                continue;
            }
            out.words.push((lexeme, limit_position(out.pos)));
        }
    }
}

/// `make_tsvector`: the lexemes in order with their positions. A lexeme keeps at most 255 positions, and no position after the largest position.
///
/// # Errors
///
/// `54000` when the value is too large.
pub(crate) fn make_tsvector(parsed: Parsed) -> Result<Vec<Lexeme>> {
    let mut words = parsed.words;
    words.sort_by(|a, b| {
        ts_compare_string(a.0.as_bytes(), b.0.as_bytes()).cmp(&0).then(a.1.cmp(&b.1))
    });
    let mut out: Vec<Lexeme> = Vec::new();
    for (word, pos) in words {
        match out.last_mut() {
            Some(last) if last.word == word => {
                let top = last.positions.last().copied().unwrap_or(0);
                if last.positions.len() < TSVECTOR_MAX_POSITIONS - 1
                    && top != MAX_POSITION
                    && top != pos
                {
                    last.positions.push(pos);
                }
            }
            _ => out.push(Lexeme { word, positions: vec![pos] }),
        }
    }
    let len = tsvector_data_len(&out);
    if len > TSVECTOR_MAX_DATA {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("string is too long for tsvector ({len} bytes, max {TSVECTOR_MAX_DATA} bytes)"),
        ));
    }
    Ok(out)
}

/// `getTSCurrentConfig`: the configuration of `default_text_search_config`.
///
/// # Errors
///
/// `XX000` when the parameter is empty, and the errors of the input of `regconfig` for a configuration that does not exist.
pub(crate) fn current_config(session: &dyn Session) -> Result<u32> {
    let name = session.setting("default_text_search_config").unwrap_or_default();
    if name.is_empty() {
        return Err(Error::internal("text search configuration isn't set"));
    }
    reg::input(RegKind::Config, &name, session)?
}

/// `to_tsvector_byid`: the lexemes of a text with a configuration.
fn to_tsvector_with(session: &dyn Session, config: u32, text: &str) -> Result<Value> {
    let config = tsdict::config(session.catalog(), config)?;
    let mut parsed = Parsed::default();
    parse_text(session, &config, text, &mut parsed);
    Ok(Value::TsVector(make_tsvector(parsed)?))
}

/// `to_tsvector_byid`: `to_tsvector(regconfig, text)`.
fn to_tsvector_byid(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [config, text] = args else { return Err(bad_value()) };
    let config = config.as_oid().ok_or_else(bad_value)?;
    to_tsvector_with(call.session, config, text.as_str().ok_or_else(bad_value)?)
}

/// `to_tsvector`: `to_tsvector(text)` with the configuration of `default_text_search_config`.
fn to_tsvector(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text] = args else { return Err(bad_value()) };
    let config = current_config(call.session)?;
    to_tsvector_with(call.session, config, text.as_str().ok_or_else(bad_value)?)
}

/// `get_current_ts_config`: the configuration of `default_text_search_config`.
fn get_current_ts_config(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Oid(current_config(call.session)?))
}

/// `ts_match_tq`: the operator `text @@ tsquery`, which is `to_tsvector(text) @@ tsquery`.
fn match_tq(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text, query] = args else { return Err(bad_value()) };
    let Value::TsQuery(items) = query else { return Err(bad_value()) };
    let config = current_config(call.session)?;
    let vector = to_tsvector_with(call.session, config, text.as_str().ok_or_else(bad_value)?)?;
    let Value::TsVector(lexemes) = vector else { return Err(bad_value()) };
    Ok(Value::Bool(crate::tsmatch::matches(&lexemes, items)))
}

/// `pushval_morph`: the lexemes of an operand of a query, from `parsetext`. The lexemes of one word have `&` between them, and the words have `op` with the distance 1 between them. A stop word in the operand keeps its place, so the distances of `<->` stay correct. An operand with no lexemes is the place of a stop word.
fn push_morph(
    session: &dyn Session,
    config: &Config,
    op: u8,
    push: &mut TsQueryPush<'_>,
    text: &str,
    weight: u8,
    prefix: bool,
) -> std::result::Result<(), TypeError> {
    let mut parsed = Parsed::default();
    parse_text(session, config, text, &mut parsed);
    if parsed.words.is_empty() {
        push.stop();
        return Ok(());
    }
    let mut words = parsed.words.into_iter().peekable();
    let mut pos = 0;
    let mut first = true;
    while let Some((word, at)) = words.next() {
        if pos > 0 {
            while pos + 1 < at {
                push.stop();
                if !first {
                    push.operator(op, 1);
                }
                first = false;
                pos += 1;
            }
        }
        pos = at;
        push.value(word, weight, prefix)?;
        while let Some((word, _)) = words.next_if(|word| word.1 == pos) {
            push.value(word, weight, prefix)?;
            push.operator(TSQUERY_AND, 0);
        }
        if !first {
            push.operator(op, 1);
        }
        first = false;
    }
    Ok(())
}

/// `parse_tsquery` with `pushval_morph`: the query of a text with a configuration, with the notice of PostgreSQL for an empty query.
fn to_tsquery_with(
    session: &dyn Session,
    config: u32,
    text: &str,
    mode: TsQueryMode,
    op: u8,
) -> Result<Vec<QueryItem>> {
    let config = tsdict::config(session.catalog(), config)?;
    let mut pushval = |push: &mut TsQueryPush<'_>, word: &str, weight: u8, prefix: bool| {
        push_morph(session, &config, op, push, word, weight, prefix)
    };
    let (items, stops) = tsquery_parse_with(text, mode, &mut pushval)?;
    if stops {
        session.warn(Error::new(
            SqlState::SUCCESSFUL_COMPLETION,
            "text-search query contains only stop words or doesn't contain lexemes, ignored",
        ));
    } else if items.is_empty() {
        session.warn(crate::tsquery::no_lexemes(text));
    }
    Ok(items)
}

/// The functions that make a query, with a configuration as the first argument or with the configuration of `default_text_search_config`. `mode` is the syntax of the text, and `op` is the operator between the words of an operand.
fn to_tsquery_kind(call: &Call<'_>, args: &[Value], mode: TsQueryMode, op: u8) -> Result<Value> {
    let (config, text) = match args {
        [config, text] => (config.as_oid().ok_or_else(bad_value)?, text),
        [text] => (current_config(call.session)?, text),
        _ => return Err(bad_value()),
    };
    let text = text.as_str().ok_or_else(bad_value)?;
    Ok(Value::TsQuery(to_tsquery_with(call.session, config, text, mode, op)?))
}

/// `to_tsquery_byid` and `to_tsquery`.
fn to_tsquery(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    to_tsquery_kind(call, args, TsQueryMode::Standard, TSQUERY_PHRASE)
}

/// `plainto_tsquery_byid` and `plainto_tsquery`.
fn plainto_tsquery(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    to_tsquery_kind(call, args, TsQueryMode::Plain, TSQUERY_AND)
}

/// `phraseto_tsquery_byid` and `phraseto_tsquery`.
fn phraseto_tsquery(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    to_tsquery_kind(call, args, TsQueryMode::Plain, TSQUERY_PHRASE)
}

/// `websearch_to_tsquery_byid` and `websearch_to_tsquery`.
fn websearch_to_tsquery(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    to_tsquery_kind(call, args, TsQueryMode::Web, TSQUERY_PHRASE)
}

/// `ts_match_tt`: the operator `text @@ text`, which is `to_tsvector(text) @@ plainto_tsquery(text)`.
fn match_tt(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [text, query] = args else { return Err(bad_value()) };
    let config = current_config(call.session)?;
    let vector = to_tsvector_with(call.session, config, text.as_str().ok_or_else(bad_value)?)?;
    let Value::TsVector(lexemes) = vector else { return Err(bad_value()) };
    let query = query.as_str().ok_or_else(bad_value)?;
    let items = to_tsquery_with(call.session, config, query, TsQueryMode::Plain, TSQUERY_AND)?;
    Ok(Value::Bool(crate::tsmatch::matches(&lexemes, &items)))
}

/// `lookup_ts_parser_cache`: the default parser is the only parser.
///
/// # Errors
///
/// `XX000` when no parser has the OID.
fn parser(oid: u32) -> Result<u32> {
    match builtin::named_by_oid(Named::Parser, oid) {
        Some(row) => Ok(row.oid),
        None => Err(Error::internal(format!("cache lookup failed for text search parser {oid}"))),
    }
}

/// The OID of the parser of the first argument of `ts_parse` and `ts_token_type`: an OID for the functions `_byid`, and a name for the functions `_byname`.
fn parser_arg(call: &Call<'_>, arg: &Value) -> Result<u32> {
    match arg {
        Value::Oid(oid) => parser(*oid),
        _ => reg::ts_parser_oid(arg.as_str().ok_or_else(bad_value)?, call.session),
    }
}

/// `ts_token_type_byid` and `ts_token_type_byname`: the number, the alias and the description of each token type of the parser.
pub(crate) fn token_types(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let [parser] = args else { return Err(bad_value()) };
    parser_arg(call, parser)?;
    Ok((1..)
        .zip(TOKEN_TYPES)
        .map(|(id, (alias, description))| {
            vec![Value::Int4(id), Value::text(alias), Value::text(description)]
        })
        .collect())
}

/// `ts_parse_byid` and `ts_parse_byname`: the type and the text of each token of the text.
pub(crate) fn parse(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let [parser, text] = args else { return Err(bad_value()) };
    parser_arg(call, parser)?;
    let text = text.as_str().ok_or_else(bad_value)?;
    Ok(tsparser::tokens(text)
        .into_iter()
        .map(|(token, word)| vec![Value::Int4(i32::from(token)), Value::text(word)])
        .collect())
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "to_tsvector_byid" => to_tsvector_byid,
        "to_tsvector" => to_tsvector,
        "get_current_ts_config" => get_current_ts_config,
        "ts_match_tq" => match_tq,
        "to_tsquery_byid" | "to_tsquery" => to_tsquery,
        "plainto_tsquery_byid" | "plainto_tsquery" => plainto_tsquery,
        "phraseto_tsquery_byid" | "phraseto_tsquery" => phraseto_tsquery,
        "websearch_to_tsquery_byid" | "websearch_to_tsquery" => websearch_to_tsquery,
        "ts_match_tt" => match_tt,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let text = "Héllo wörld ÄÖ 123 foo-bar x@y.com http://a.b/c 1.5e3";
        let got: Vec<String> =
            tsparser::tokens(text).iter().map(|(t, w)| format!("({t},{w})")).collect();
        assert_eq!(
            got.join(" "),
            "(2,Héllo) (12, ) (2,wörld) (12, ) (2,ÄÖ) (12, ) (22,123) (12, ) (16,foo-bar) (11,foo) (12,-) (11,bar) (12, ) (4,x@y.com) (12, ) (14,http://) (19,a.b/c) (12, ) (7,1.5e3)"
        );
    }

    #[test]
    fn merge_positions() {
        let words = ["b", "a", "b", "b"].iter().zip([1, 2, 3, 3]);
        let parsed = Parsed { words: words.map(|(w, p)| ((*w).to_string(), p)).collect(), pos: 3 };
        let out = make_tsvector(parsed).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].word.as_str(), &out[0].positions[..]), ("a", &[2][..]));
        assert_eq!((out[1].word.as_str(), &out[1].positions[..]), ("b", &[1, 3][..]));
    }
}

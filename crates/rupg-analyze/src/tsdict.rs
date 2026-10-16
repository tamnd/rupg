//! The text search dictionaries of the templates `simple` and `snowball`: the init functions `dsimple_init` and `dsnowball_init`, which read the options of a dictionary, and the lexize functions `dsimple_lexize` and `dsnowball_lexize`. `CREATE TEXT SEARCH DICTIONARY` runs the init function to check the options, as `verify_dictoptions` does, and `ts_lexize` runs both. It also has the configurations of `lookup_ts_config_cache`, with the dictionaries of their maps.
//!
//! The options of a dictionary are in `dictinitoption` in the form that `serialize_deflist` writes, and [`deserialize`] reads them back as `deserialize_deflist` does. The stop word files are the files of the share directory of PostgreSQL 19, which `rupg-pgcatalog` holds.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use rupg_catalog::{Catalog, TsMapping};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_snowball::Stemmer;

/// The longest word in bytes that `dsnowball_lexize` gives to the stemmer.
const MAX_STEM_INPUT: usize = 1000;

/// The words of a stop word file, in lower case.
type StopList = &'static HashSet<String>;

/// A dictionary after its init function.
#[derive(Clone, Copy, Debug)]
pub enum Dictionary {
    /// `DictSimple`: the stop words and the flag `Accept`.
    Simple {
        /// The stop words, or `None` without `StopWords`.
        stop: Option<StopList>,
        /// True when a word that is not a stop word is a lexeme. Else the result is null, so that the next dictionary gets the word.
        accept: bool,
    },
    /// `DictSnowball`: the stemmer and the stop words.
    Snowball {
        /// The stemmer of `Language`.
        stemmer: Stemmer,
        /// The stop words, or `None` without `StopWords`.
        stop: Option<StopList>,
    },
}

/// An option of a dictionary: the name and the value as `defGetString` gives it.
pub type DictOption = (String, String);

fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `readstoplist` for the file `name.stop`: the first word of each line that is not empty, in lower case. The stop word files that `initdb` installs are the only files.
///
/// # Errors
///
/// `22023` for a name with a character other than `a` to `z`, `0` to `9` and `_`, as `get_tsearch_config_filename` checks it, and `F0000` when the file does not exist.
fn stop_list(name: &str) -> Result<Option<StopList>> {
    static LISTS: OnceLock<BTreeMap<&'static str, HashSet<String>>> = OnceLock::new();
    if name.is_empty() {
        return Ok(None);
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(invalid(format!("invalid text search configuration file name \"{name}\"")));
    }
    let lists = LISTS.get_or_init(|| {
        let mut lists = BTreeMap::new();
        for file in ["danish", "dutch", "english", "finnish", "french", "german", "hungarian"]
            .into_iter()
            .chain(["italian", "nepali", "norwegian", "portuguese", "russian", "spanish"])
            .chain(["swedish", "turkish"])
        {
            let text = rupg_pgcatalog::stop_words(file).unwrap_or_default();
            let words = text
                .lines()
                .map(|line| line.split(char::is_whitespace).next().unwrap_or_default())
                .filter(|word| !word.is_empty())
                .map(str::to_ascii_lowercase)
                .collect();
            lists.insert(file, words);
        }
        lists
    });
    match lists.get(name) {
        Some(list) => Ok(Some(list)),
        None => Err(Error::new(
            SqlState::CONFIG_FILE_ERROR,
            format!(
                "could not open stop-word file \"/usr/local/pgsql/share/tsearch_data/{name}.stop\": No such file or directory"
            ),
        )),
    }
}

/// `defGetBoolean` for the text of an option.
fn boolean(name: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a Boolean value"))),
    }
}

impl Dictionary {
    /// The init function with this `prosrc` on the options. `None` when rupg does not have the init function.
    ///
    /// # Errors
    ///
    /// The errors of `dsimple_init` and `dsnowball_init` for the options.
    pub fn init(init: &str, options: &[DictOption]) -> Result<Option<Dictionary>> {
        match init {
            "dsimple_init" => Self::simple(options).map(Some),
            "dsnowball_init" => Self::snowball(options).map(Some),
            _ => Ok(None),
        }
    }

    /// `dsimple_init`.
    fn simple(options: &[DictOption]) -> Result<Dictionary> {
        let (mut stop, mut accept) = (None, None);
        for (name, value) in options {
            match name.as_str() {
                "stopwords" => {
                    if stop.is_some() {
                        return Err(invalid("multiple StopWords parameters"));
                    }
                    stop = Some(stop_list(value)?);
                }
                "accept" => {
                    if accept.is_some() {
                        return Err(invalid("multiple Accept parameters"));
                    }
                    accept = Some(boolean(name, value)?);
                }
                _ => {
                    return Err(invalid(format!(
                        "unrecognized simple dictionary parameter: \"{name}\""
                    )));
                }
            }
        }
        Ok(Dictionary::Simple { stop: stop.flatten(), accept: accept.unwrap_or(true) })
    }

    /// `dsnowball_init`. The stemmers work on UTF-8, the encoding of each database.
    fn snowball(options: &[DictOption]) -> Result<Dictionary> {
        let (mut stop, mut stemmer) = (None, None);
        for (name, value) in options {
            match name.as_str() {
                "stopwords" => {
                    if stop.is_some() {
                        return Err(invalid("multiple StopWords parameters"));
                    }
                    stop = Some(stop_list(value)?);
                }
                "language" => {
                    if stemmer.is_some() {
                        return Err(invalid("multiple Language parameters"));
                    }
                    stemmer = Some(Stemmer::find(value).ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!(
                                "no Snowball stemmer available for language \"{value}\" and encoding \"UTF8\""
                            ),
                        )
                    })?);
                }
                _ => {
                    return Err(invalid(format!("unrecognized Snowball parameter: \"{name}\"")));
                }
            }
        }
        let Some(stemmer) = stemmer else { return Err(invalid("missing Language parameter")) };
        Ok(Dictionary::Snowball { stemmer, stop: stop.flatten() })
    }

    /// The lexize function: the lexemes of a word. An empty list is a stop word, and `None` is a word that the dictionary does not know. The word is in lower case, as `str_tolower` makes it in the C locale.
    pub fn lexize(&self, word: &str) -> Option<Vec<String>> {
        let word = word.to_ascii_lowercase();
        let is_stop = |stop: &Option<StopList>| stop.is_some_and(|s| s.contains(&word));
        match self {
            Dictionary::Simple { stop, accept } => {
                if word.is_empty() || is_stop(stop) {
                    Some(Vec::new())
                } else if *accept {
                    Some(vec![word])
                } else {
                    None
                }
            }
            Dictionary::Snowball { stemmer, stop } => {
                if word.len() > MAX_STEM_INPUT {
                    Some(vec![word])
                } else if word.is_empty() || is_stop(stop) {
                    Some(Vec::new())
                } else {
                    Some(vec![stemmer.stem(&word)])
                }
            }
        }
    }
}

/// The OID in a cell of a static row, for a column of type `oid` or `regproc`, or 0.
pub(crate) fn cell_oid(catalog: &rupg_pgcatalog::Catalog, row: usize, column: &str) -> u32 {
    cell_text(catalog, row, column).and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// The text of a cell of a static row, or `None` for a null.
fn cell_text(catalog: &rupg_pgcatalog::Catalog, row: usize, column: &str) -> Option<String> {
    catalog.column(column).and_then(|i| catalog.rows[i].text(row))
}

/// The static rows of a catalog whose column `column` is `value`.
pub(crate) fn static_rows(name: &str, column: &str, value: u32) -> Vec<usize> {
    let Some(catalog) = rupg_pgcatalog::catalog(name) else { return Vec::new() };
    (0..catalog.len).filter(|&row| cell_oid(catalog, row, column) == value).collect()
}

/// The first static row of a catalog with the OID, and the catalog.
fn static_row(name: &str, oid: u32) -> Option<(&'static rupg_pgcatalog::Catalog, usize)> {
    let catalog = rupg_pgcatalog::catalog(name)?;
    static_rows(name, "oid", oid).first().map(|&row| (catalog, row))
}

/// The name of a template and the `prosrc` of its init function. The text is empty for a template without an init function. `None` when no template has the OID.
pub fn template_init(catalog: Option<&Catalog>, template: u32) -> Option<(String, String)> {
    let (name, init) = match catalog.and_then(|c| c.ts_template(template)) {
        Some(t) => (t.name.clone(), t.init),
        None => {
            let name = builtin::named_by_oid(Named::Template, template)?.name.to_string();
            let init = static_row("pg_ts_template", template)
                .map_or(0, |(rows, row)| cell_oid(rows, row, "tmplinit"));
            (name, init)
        }
    };
    let src = match (init, catalog.and_then(|c| c.function(init))) {
        (0, _) => String::new(),
        (_, Some(f)) => f.src.clone(),
        (_, None) => builtin::proc_by_oid(init).map_or_else(String::new, |p| p.src.to_string()),
    };
    Some((name, src))
}

/// `lookup_ts_dictionary_cache`: the dictionary with the OID after the init function of its template.
///
/// # Errors
///
/// `XX000` when no dictionary has the OID, `0A000` for a template whose init function rupg does not have, and the errors of the init function for the options.
pub fn dictionary(catalog: Option<&Catalog>, oid: u32) -> Result<Dictionary> {
    let lost = || Error::internal(format!("cache lookup failed for text search dictionary {oid}"));
    let (template, options) = match catalog.and_then(|c| c.ts_dictionary(oid)) {
        Some(d) => (d.template, d.options.clone()),
        None => {
            let (rows, row) = static_row("pg_ts_dict", oid).ok_or_else(lost)?;
            (cell_oid(rows, row, "dicttemplate"), cell_text(rows, row, "dictinitoption"))
        }
    };
    let (name, init) = template_init(catalog, template).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for text search template {template}"))
    })?;
    let options = match options {
        Some(text) => deserialize(&text)?,
        None => Vec::new(),
    };
    Dictionary::init(&init, &options)?.ok_or_else(|| {
        Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("a dictionary of the template {name} is not supported yet"),
        )
    })
}

/// A text search configuration after `lookup_ts_config_cache`: the parser and the dictionaries of each token type.
#[derive(Clone, Debug)]
pub struct Config {
    /// `cfgparser`: the OID of the parser.
    pub parser: u32,
    /// The dictionaries of each token type in the order of `mapseqno`, by the number of the token type. A token type with no row of `pg_ts_config_map` has no dictionaries.
    pub map: Vec<Vec<Dictionary>>,
}

/// The rows of `pg_ts_config_map` of a configuration of the catalog or of a built-in configuration, in the order of the index on the configuration, the token type and the place.
pub fn mappings(catalog: Option<&Catalog>, config: u32) -> Vec<TsMapping> {
    if let Some(catalog) = catalog.filter(|c| c.ts_config(config).is_some()) {
        let mut list: Vec<TsMapping> =
            catalog.ts_mappings().iter().filter(|m| m.config == config).copied().collect();
        list.sort_by_key(|m| (m.token, m.seqno));
        return list;
    }
    let Some(rows) = rupg_pgcatalog::catalog("pg_ts_config_map") else { return Vec::new() };
    static_rows("pg_ts_config_map", "mapcfg", config)
        .into_iter()
        .map(|row| {
            let int = |column| i32::try_from(cell_oid(rows, row, column)).unwrap_or(0);
            TsMapping {
                config,
                token: int("maptokentype"),
                seqno: int("mapseqno"),
                dict: cell_oid(rows, row, "mapdict"),
            }
        })
        .collect()
}

/// `lookup_ts_config_cache`: the configuration with the OID, with each dictionary of its map after the init function.
///
/// # Errors
///
/// `XX000` when no configuration has the OID, and the errors of [`dictionary`].
pub fn config(catalog: Option<&Catalog>, oid: u32) -> Result<Config> {
    let parser = match catalog.and_then(|c| c.ts_config(oid)) {
        Some(c) => c.parser,
        None => static_row("pg_ts_config", oid)
            .map(|(rows, row)| cell_oid(rows, row, "cfgparser"))
            .ok_or_else(|| {
                Error::internal(format!("cache lookup failed for text search configuration {oid}"))
            })?,
    };
    let mut map: Vec<Vec<Dictionary>> = Vec::new();
    for m in mappings(catalog, oid) {
        let Ok(token) = usize::try_from(m.token) else { continue };
        if map.len() <= token {
            map.resize_with(token + 1, Vec::new);
        }
        map[token].push(dictionary(catalog, m.dict)?);
    }
    Ok(Config { parser, map })
}

/// `buildDefItem` and `defGetString` for a value without quotes: an integer becomes the text of `%d`, and another value stays the same.
fn word_value(value: &str) -> String {
    value.parse::<i32>().map_or_else(|_| value.to_string(), |v| v.to_string())
}

/// `deserialize_deflist`: the options of `dictinitoption`. A key or a value in double quotes, a value in single quotes with or without `E`, and a value without quotes are all valid.
///
/// # Errors
///
/// `42601` for a key without `=`, and for a text that ends inside a key or a value.
pub fn deserialize(text: &str) -> Result<Vec<DictOption>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        WaitKey,
        InKey,
        InQuotedKey,
        WaitEq,
        WaitValue,
        InSingle,
        InDouble,
        InWord,
    }
    let bad =
        || Error::new(SqlState::SYNTAX_ERROR, format!("invalid parameter list format: \"{text}\""));
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut key, mut value) = (String::new(), String::new());
    let mut state = State::WaitKey;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            State::WaitKey => {
                if !(c.is_ascii_whitespace() || c == ',') {
                    key.clear();
                    if c == '"' {
                        state = State::InQuotedKey;
                    } else {
                        key.push(c);
                        state = State::InKey;
                    }
                }
            }
            State::InKey => {
                if c.is_ascii_whitespace() {
                    state = State::WaitEq;
                } else if c == '=' {
                    state = State::WaitValue;
                } else {
                    key.push(c);
                }
            }
            State::InQuotedKey => {
                if c == '"' && next == Some('"') {
                    key.push('"');
                    i += 1;
                } else if c == '"' {
                    state = State::WaitEq;
                } else {
                    key.push(c);
                }
            }
            State::WaitEq => {
                if c == '=' {
                    state = State::WaitValue;
                } else if !c.is_ascii_whitespace() {
                    return Err(bad());
                }
            }
            State::WaitValue => {
                value.clear();
                if c == '\'' {
                    state = State::InSingle;
                } else if c == 'E' && next == Some('\'') {
                    i += 1;
                    state = State::InSingle;
                } else if c == '"' {
                    state = State::InDouble;
                } else if !c.is_ascii_whitespace() {
                    value.push(c);
                    state = State::InWord;
                }
            }
            State::InSingle | State::InDouble => {
                let quote = if state == State::InSingle { '\'' } else { '"' };
                if c == quote && next == Some(quote) {
                    value.push(c);
                    i += 1;
                } else if c == quote {
                    out.push((key.clone(), value.clone()));
                    state = State::WaitKey;
                } else if state == State::InSingle && c == '\\' && next == Some('\\') {
                    value.push(c);
                    i += 1;
                } else {
                    value.push(c);
                }
            }
            State::InWord => {
                if c == ',' || c.is_ascii_whitespace() {
                    out.push((key.clone(), word_value(&value)));
                    state = State::WaitKey;
                } else {
                    value.push(c);
                }
            }
        }
        i += 1;
    }
    match state {
        State::WaitKey => Ok(out),
        State::InWord => {
            out.push((key, word_value(&value)));
            Ok(out)
        }
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Dictionary, deserialize};

    fn parse(text: &str) -> Vec<(String, String)> {
        deserialize(text).expect("options")
    }

    #[test]
    fn options() {
        let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(
            deserialize(
                "language = 'danish', stopwords = 'it''s', x = 1.5, \"Odd Name\" = E'a\\\\b'"
            )
            .expect("options"),
            vec![
                pair("language", "danish"),
                pair("stopwords", "it's"),
                pair("x", "1.5"),
                pair("Odd Name", "a\\b")
            ]
        );
        assert!(deserialize("a b").is_err());
        assert!(deserialize("a = 'b").is_err());
        assert_eq!(deserialize("").expect("options"), vec![]);
    }

    #[test]
    fn snowball_and_simple() {
        let english = parse("language = 'english', stopwords = 'english'");
        let d = Dictionary::init("dsnowball_init", &english).expect("init").expect("known");
        assert_eq!(d.lexize("Jumping"), Some(vec!["jump".to_string()]));
        assert_eq!(d.lexize("The"), Some(vec![]));
        assert_eq!(d.lexize(""), Some(vec![]));
        let simple = parse("accept = false, stopwords = english");
        let d = Dictionary::init("dsimple_init", &simple).expect("init").expect("known");
        assert_eq!(d.lexize("the"), Some(vec![]));
        assert_eq!(d.lexize("cat"), None);
        let error = |init: &str, text: &str| {
            Dictionary::init(init, &parse(text)).expect_err("error").message().to_string()
        };
        assert_eq!(error("dsnowball_init", "stopwords = english"), "missing Language parameter");
        assert_eq!(
            error("dsnowball_init", "language = klingon"),
            "no Snowball stemmer available for language \"klingon\" and encoding \"UTF8\""
        );
        assert_eq!(
            error("dsimple_init", "stopwords = '../x'"),
            "invalid text search configuration file name \"../x\""
        );
        assert_eq!(error("dsimple_init", "accept = maybe"), "accept requires a Boolean value");
    }
}

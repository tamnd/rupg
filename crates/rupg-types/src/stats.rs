//! `pg_ndistinct` and `pg_dependencies`, the types of the extended statistics of the kinds `d` and `f`, in the text format.
//!
//! The text is JSON. This is a port of `pg_ndistinct_in` and `pg_ndistinct_out` in `src/backend/utils/adt/pg_ndistinct.c` and of `pg_dependencies_in` and `pg_dependencies_out` in `src/backend/utils/adt/pg_dependencies.c`, with the same detail text for each error. The semantic actions of PostgreSQL get the events of [`json::parse`] in the same order, so the first error is the error of PostgreSQL. A JSON syntax error gives the detail "Input data must be valid JSON.".
//!
//! A value is the bytes of `statext_ndistinct_serialize` or `statext_dependencies_serialize` on a little-endian server. The casts to `bytea` and the binary output give these bytes.

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::float::float8_in;
use crate::json::{self, Kind, Open, Sink, Stop};
use crate::number::{int2_in, int4_in};

/// `STATS_MAX_DIMENSIONS`: the most columns and expressions that a statistics object can have.
const STATS_MAX_DIMENSIONS: usize = 8;
/// The lowest attribute number, which is the number of the last expression of a statistics object with the most expressions.
const MIN_ATTNUM: i16 = -8;
/// `STATS_NDISTINCT_MAGIC`, the first 4 bytes of a `pg_ndistinct` value.
const NDISTINCT_MAGIC: u32 = 0xA352_BFA4;
/// `STATS_DEPS_MAGIC`, the first 4 bytes of a `pg_dependencies` value.
const DEPENDENCIES_MAGIC: u32 = 0xB454_9A2C;
/// `STATS_NDISTINCT_TYPE_BASIC` and `STATS_DEPS_TYPE_BASIC`.
const TYPE_BASIC: u32 = 1;
/// The key of the attribute numbers in the items of both types.
const ATTRIBUTES: &str = "attributes";

/// The type of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stats {
    NDistinct,
    Dependencies,
}

impl Stats {
    fn name(self) -> &'static str {
        match self {
            Stats::NDistinct => "pg_ndistinct",
            Stats::Dependencies => "pg_dependencies",
        }
    }
}

/// The state of the parse, as `NDistinctSemanticState` and `DependenciesSemanticState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Item,
    Key,
    AttnumList,
    Attnum,
    /// The value of `ndistinct`.
    NDistinct,
    /// The value of `dependency`.
    Dependency,
    /// The value of `degree`.
    Degree,
    Complete,
}

impl State {
    /// The number of the state in the enum of PostgreSQL for the type, which some errors show.
    fn code(self, stats: Stats) -> i32 {
        match (self, stats) {
            (State::Start, _) => 0,
            (State::Item, _) => 1,
            (State::Key, _) => 2,
            (State::AttnumList, _) => 3,
            (State::Attnum, _) => 4,
            (State::NDistinct | State::Dependency, _) => 5,
            (State::Degree, _) | (State::Complete, Stats::NDistinct) => 6,
            (State::Complete, Stats::Dependencies) => 7,
        }
    }
}

/// An item of a value: `MVNDistinctItem` with its number of distinct values, or `MVDependency` with its degree. The attribute numbers of a dependency end with the attribute number of `dependency`.
#[derive(Debug, Clone, PartialEq)]
struct Item {
    attributes: Vec<i16>,
    value: f64,
}

/// The semantic actions and the state of the parse, as `NDistinctParseState` and `DependenciesParseState`.
struct Parser<'a> {
    stats: Stats,
    input: &'a str,
    state: State,
    items: Vec<Item>,
    found_attributes: bool,
    /// True when the item has the key `ndistinct` or the key `dependency`.
    found_value: bool,
    found_degree: bool,
    attnums: Vec<i16>,
    ndistinct: i32,
    dependency: i16,
    degree: f64,
}

/// `valid_subsequent_attnum`: after a positive number comes a greater number or a negative number, and after a negative number comes a smaller number.
fn valid_subsequent_attnum(prev: i16, cur: i16) -> bool {
    if prev > 0 { cur > prev || cur < 0 } else { cur < prev }
}

/// The attribute numbers with a comma and a space between them.
fn attnum_list(attnums: &[i16]) -> String {
    attnums.iter().map(i16::to_string).collect::<Vec<_>>().join(", ")
}

impl Parser<'_> {
    /// The error `22P02` of a value that is not good, with the detail.
    fn malformed(&self, detail: String) -> TypeError {
        let mut error = TypeError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("malformed {}: \"{}\"", self.stats.name(), self.input),
        );
        error.detail = Some(detail);
        error
    }

    /// The internal error of `elog` for an event in a state that the parse cannot get to.
    fn unexpected(&self, event: &str) -> TypeError {
        TypeError::new(
            SqlState::INTERNAL_ERROR,
            format!(
                "{event} of \"{}\" found in unexpected parse state: {}.",
                self.stats.name(),
                self.state.code(self.stats)
            ),
        )
    }

    /// `ndistinct_object_end` and `dependencies_object_end`: the checks of a complete item.
    fn object_end(&mut self) -> Result<(), TypeError> {
        if self.state != State::Key {
            return Err(self.unexpected("object end"));
        }
        let (value_key, min, max, noun) = match self.stats {
            Stats::NDistinct => ("ndistinct", 2, STATS_MAX_DIMENSIONS, "attributes"),
            Stats::Dependencies => ("dependency", 1, STATS_MAX_DIMENSIONS - 1, "elements"),
        };
        if !self.found_attributes {
            return Err(self.malformed(format!("Item must contain \"{ATTRIBUTES}\" key.")));
        }
        if !self.found_value {
            return Err(self.malformed(format!("Item must contain \"{value_key}\" key.")));
        }
        if self.stats == Stats::Dependencies && !self.found_degree {
            return Err(self.malformed("Item must contain \"degree\" key.".into()));
        }
        let natts = self.attnums.len();
        if natts < min || natts > max {
            return Err(self.malformed(format!(
                "The \"{ATTRIBUTES}\" key must contain an array of at least {min} and no more than {max} {noun}."
            )));
        }
        let mut attributes = std::mem::take(&mut self.attnums);
        let value = match self.stats {
            Stats::NDistinct => f64::from(self.ndistinct),
            Stats::Dependencies => {
                if attributes.contains(&self.dependency) {
                    return Err(self.malformed(format!(
                        "Item \"dependency\" with value {} has been found in the \"{ATTRIBUTES}\" list.",
                        self.dependency
                    )));
                }
                attributes.push(self.dependency);
                self.degree
            }
        };
        self.items.push(Item { attributes, value });
        self.ndistinct = 0;
        self.dependency = 0;
        self.degree = 0.0;
        self.found_attributes = false;
        self.found_value = false;
        self.found_degree = false;
        self.state = State::Item;
        Ok(())
    }

    /// `build_mvndistinct` and `build_mvdependencies`: the checks of the whole value after the parse, then its bytes.
    fn build(self) -> Result<Vec<u8>, TypeError> {
        match self.state {
            State::Complete if !self.items.is_empty() => {}
            State::Complete => {
                return Err(TypeError::new(
                    SqlState::INTERNAL_ERROR,
                    format!("{} parsing ended with an empty item list.", self.stats.name()),
                ));
            }
            State::Start => return Err(self.malformed("Value cannot be empty.".into())),
            state => {
                return Err(self.malformed(format!(
                    "Unexpected end state has been found: {}.",
                    state.code(self.stats)
                )));
            }
        }
        for (i, item) in self.items.iter().enumerate() {
            if self.items[..i].iter().any(|other| other.attributes == item.attributes) {
                return Err(self.malformed(match self.stats {
                    Stats::NDistinct => format!(
                        "Duplicated \"{ATTRIBUTES}\" array has been found: [{}].",
                        attnum_list(&item.attributes)
                    ),
                    Stats::Dependencies => {
                        let (dependency, list) = item.attributes.split_last().unwrap_or((&0, &[]));
                        format!(
                            "Duplicated \"{ATTRIBUTES}\" array has been found: [{}] for key \"dependency\" and value {dependency}.",
                            attnum_list(list)
                        )
                    }
                }));
            }
        }
        if self.stats == Stats::NDistinct {
            // Each list of attribute numbers is a subset of the first longest list.
            let mut longest = 0;
            for (i, item) in self.items.iter().enumerate() {
                if item.attributes.len() > self.items[longest].attributes.len() {
                    longest = i;
                }
            }
            let reference = &self.items[longest].attributes;
            for item in &self.items {
                if !item.attributes.iter().all(|a| reference.contains(a)) {
                    return Err(self.malformed(format!(
                        "\"{ATTRIBUTES}\" array [{}] must be a subset of array [{}].",
                        attnum_list(&item.attributes),
                        attnum_list(reference)
                    )));
                }
            }
        }
        Ok(serialize(self.stats, &self.items))
    }
}

impl Sink for Parser<'_> {
    fn open(&mut self, open: Open) -> Result<(), TypeError> {
        match open {
            Open::Object => {
                let detail = match self.state {
                    State::Item => {
                        self.state = State::Key;
                        return Ok(());
                    }
                    State::Start => "Initial element must be an array.".to_owned(),
                    State::Key => "A key was expected.".to_owned(),
                    State::AttnumList => {
                        format!("Value of \"{ATTRIBUTES}\" must be an array of attribute numbers.")
                    }
                    State::Attnum => {
                        "Attribute lists can only contain attribute numbers.".to_owned()
                    }
                    State::NDistinct => "Value of \"ndistinct\" must be an integer.".to_owned(),
                    State::Dependency => "Value of \"dependency\" must be an integer.".to_owned(),
                    State::Degree => "Value of \"degree\" must be an integer.".to_owned(),
                    State::Complete => return Err(self.unexpected("object start")),
                };
                Err(self.malformed(detail))
            }
            Open::Array => {
                self.state = match self.state {
                    State::AttnumList => State::Attnum,
                    State::Start => State::Item,
                    _ => {
                        return Err(self
                            .malformed("Array has been found at an unexpected location.".into()));
                    }
                };
                Ok(())
            }
        }
    }

    fn close(&mut self, open: Open) -> Result<(), TypeError> {
        if open == Open::Object {
            return self.object_end();
        }
        match self.state {
            State::Attnum if !self.attnums.is_empty() => self.state = State::Key,
            State::Attnum => {
                return Err(
                    self.malformed(format!("The \"{ATTRIBUTES}\" key must be a non-empty array."))
                );
            }
            State::Item if !self.items.is_empty() => self.state = State::Complete,
            State::Item => return Err(self.malformed("Item array cannot be empty.".into())),
            _ => return Err(self.unexpected("array end")),
        }
        Ok(())
    }

    fn key(&mut self, key: &str) -> Result<(), TypeError> {
        let state = match (key, self.stats) {
            (ATTRIBUTES, _) => State::AttnumList,
            ("ndistinct", Stats::NDistinct) => State::NDistinct,
            ("dependency", Stats::Dependencies) => State::Dependency,
            ("degree", Stats::Dependencies) => State::Degree,
            (_, Stats::NDistinct) => {
                return Err(self.malformed(format!(
                    "Only allowed keys are \"{ATTRIBUTES}\" and \"ndistinct\"."
                )));
            }
            (_, Stats::Dependencies) => {
                return Err(self.malformed(format!(
                    "Only allowed keys are \"{ATTRIBUTES}\", \"dependency\", and \"degree\"."
                )));
            }
        };
        let found = match state {
            State::AttnumList => &mut self.found_attributes,
            State::Degree => &mut self.found_degree,
            _ => &mut self.found_value,
        };
        if std::mem::replace(found, true) {
            return Err(self.malformed(format!("Multiple \"{key}\" keys are not allowed.")));
        }
        self.state = state;
        Ok(())
    }

    fn element(&mut self, null: bool) -> Result<(), TypeError> {
        let detail = match self.state {
            State::Attnum | State::Item if !null => return Ok(()),
            State::Attnum => "Attribute number array cannot be null.",
            State::Item => "Item list elements cannot be null.",
            _ => return Err(self.unexpected("array element start")),
        };
        Err(self.malformed(detail.into()))
    }

    fn scalar(&mut self, kind: Kind, token: &str, text: &str) -> Result<(), TypeError> {
        // The action of PostgreSQL gets the decoded text of a string and the bytes of another token.
        let token = if kind == Kind::String { text } else { token };
        let incorrect = |key: &str| format!("Key \"{key}\" has an incorrect value.");
        match self.state {
            State::Attnum => {
                let attnum = int2_in(token).map_err(|_| self.malformed(incorrect(ATTRIBUTES)))?;
                if attnum == 0 || attnum < MIN_ATTNUM {
                    return Err(self.malformed(format!(
                        "Invalid \"{ATTRIBUTES}\" element has been found: {attnum}."
                    )));
                }
                if let Some(&prev) = self.attnums.last()
                    && !valid_subsequent_attnum(prev, attnum)
                {
                    return Err(self.malformed(format!(
                        "Invalid \"{ATTRIBUTES}\" element has been found: {attnum} cannot follow {prev}."
                    )));
                }
                self.attnums.push(attnum);
            }
            State::NDistinct => {
                self.ndistinct =
                    int4_in(token).map_err(|_| self.malformed(incorrect("ndistinct")))?;
                self.state = State::Key;
            }
            State::Dependency => {
                let dependency =
                    int2_in(token).map_err(|_| self.malformed(incorrect("dependency")))?;
                if dependency == 0 || dependency < MIN_ATTNUM {
                    return Err(self.malformed(format!(
                        "Key \"dependency\" has an incorrect value: {dependency}."
                    )));
                }
                self.dependency = dependency;
                self.state = State::Key;
            }
            State::Degree => {
                self.degree = float8_in(token).map_err(|_| self.malformed(incorrect("degree")))?;
                self.state = State::Key;
            }
            _ => return Err(self.malformed("Unexpected scalar has been found.".into())),
        }
        Ok(())
    }
}

/// The text input of the type: the parse, then the checks of the whole value.
fn stats_in(stats: Stats, s: &str) -> Result<Vec<u8>, TypeError> {
    let mut parser = Parser {
        stats,
        input: s,
        state: State::Start,
        items: Vec::new(),
        found_attributes: false,
        found_value: false,
        found_degree: false,
        attnums: Vec::new(),
        ndistinct: 0,
        dependency: 0,
        degree: 0.0,
    };
    match json::parse(s, true, &mut parser) {
        Ok(()) => parser.build(),
        Err(Stop::Action(error)) => Err(error),
        Err(Stop::Syntax(_)) => Err(parser.malformed("Input data must be valid JSON.".into())),
    }
}

/// `pg_ndistinct_in`: the bytes of the value.
///
/// # Errors
///
/// `22P02` with the detail of PostgreSQL for text that is not a good value.
pub fn pg_ndistinct_in(s: &str) -> Result<Vec<u8>, TypeError> {
    stats_in(Stats::NDistinct, s)
}

/// `pg_dependencies_in`: the bytes of the value.
///
/// # Errors
///
/// `22P02` with the detail of PostgreSQL for text that is not a good value.
pub fn pg_dependencies_in(s: &str) -> Result<Vec<u8>, TypeError> {
    stats_in(Stats::Dependencies, s)
}

/// `statext_ndistinct_serialize` and `statext_dependencies_serialize`: the magic number, the type and the number of items, then for each item its value as a `float8` and its attribute numbers after their count. The count is an `int4` for `pg_ndistinct` and an `int2` for `pg_dependencies`.
fn serialize(stats: Stats, items: &[Item]) -> Vec<u8> {
    let magic = match stats {
        Stats::NDistinct => NDISTINCT_MAGIC,
        Stats::Dependencies => DEPENDENCIES_MAGIC,
    };
    let mut out = Vec::new();
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&TYPE_BASIC.to_le_bytes());
    out.extend_from_slice(&u32::try_from(items.len()).unwrap_or(u32::MAX).to_le_bytes());
    for item in items {
        out.extend_from_slice(&item.value.to_le_bytes());
        let count = i16::try_from(item.attributes.len()).unwrap_or(i16::MAX);
        match stats {
            Stats::NDistinct => out.extend_from_slice(&i32::from(count).to_le_bytes()),
            Stats::Dependencies => out.extend_from_slice(&count.to_le_bytes()),
        }
        for attnum in &item.attributes {
            out.extend_from_slice(&attnum.to_le_bytes());
        }
    }
    out
}

/// A reader of the bytes of a value.
struct Reader<'a> {
    data: &'a [u8],
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], TypeError> {
        let Some((head, rest)) = self.data.split_first_chunk::<N>() else {
            return Err(TypeError::new(
                SqlState::INTERNAL_ERROR,
                "invalid extended statistics value: too short".into(),
            ));
        };
        self.data = rest;
        Ok(*head)
    }
}

/// `statext_ndistinct_deserialize` and `statext_dependencies_deserialize`: the items of the bytes of a value.
fn deserialize(stats: Stats, data: &[u8]) -> Result<Vec<Item>, TypeError> {
    let bad = |what: &str| {
        TypeError::new(SqlState::INTERNAL_ERROR, format!("invalid {what} in {}", stats.name()))
    };
    let mut reader = Reader { data };
    let magic = u32::from_le_bytes(reader.take()?);
    let ty = u32::from_le_bytes(reader.take()?);
    let expected = match stats {
        Stats::NDistinct => NDISTINCT_MAGIC,
        Stats::Dependencies => DEPENDENCIES_MAGIC,
    };
    if magic != expected {
        return Err(bad("magic number"));
    }
    if ty != TYPE_BASIC {
        return Err(bad("type"));
    }
    let count = u32::from_le_bytes(reader.take()?);
    let mut items = Vec::new();
    for _ in 0..count {
        let value = f64::from_le_bytes(reader.take()?);
        let natts = match stats {
            Stats::NDistinct => i64::from(i32::from_le_bytes(reader.take()?)),
            Stats::Dependencies => i64::from(i16::from_le_bytes(reader.take()?)),
        };
        let natts = usize::try_from(natts).map_err(|_| bad("number of attributes"))?;
        let mut attributes = Vec::new();
        for _ in 0..natts {
            attributes.push(i16::from_le_bytes(reader.take()?));
        }
        items.push(Item { attributes, value });
    }
    if !reader.data.is_empty() {
        return Err(bad("length"));
    }
    Ok(items)
}

/// `pg_ndistinct_out`: the items as JSON, with the number of distinct values as an integer.
///
/// # Errors
///
/// An internal error for bytes that are not a value of the type.
pub fn pg_ndistinct_out(data: &[u8], out: &mut Vec<u8>) -> Result<(), TypeError> {
    let items = deserialize(Stats::NDistinct, data)?;
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        // The input makes each number of distinct values from an `int4`, so the number has no fraction.
        let text = format!(
            "{{\"{ATTRIBUTES}\": [{}], \"ndistinct\": {}}}",
            attnum_list(&item.attributes),
            item.value
        );
        out.extend_from_slice(text.as_bytes());
    }
    out.push(b']');
    Ok(())
}

/// `pg_dependencies_out`: the items as JSON, with the degree in the format `%f` of the `snprintf` of PostgreSQL.
///
/// # Errors
///
/// An internal error for bytes that are not a value of the type.
pub fn pg_dependencies_out(data: &[u8], out: &mut Vec<u8>) -> Result<(), TypeError> {
    let items = deserialize(Stats::Dependencies, data)?;
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        let Some((dependency, list)) = item.attributes.split_last() else {
            return Err(TypeError::new(
                SqlState::INTERNAL_ERROR,
                "invalid zero-length nattributes array in MVDependencies".into(),
            ));
        };
        // The `snprintf` of PostgreSQL writes `NaN`, `Infinity` and `-Infinity` for `%f`.
        let degree = match item.value {
            v if v.is_nan() => "NaN".to_owned(),
            f64::INFINITY => "Infinity".to_owned(),
            f64::NEG_INFINITY => "-Infinity".to_owned(),
            v => format!("{v:.6}"),
        };
        let text = format!(
            "{{\"{ATTRIBUTES}\": [{}], \"dependency\": {dependency}, \"degree\": {degree}}}",
            attnum_list(list)
        );
        out.extend_from_slice(text.as_bytes());
    }
    out.push(b']');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The input and the detail that `pg_input_error_info` of PostgreSQL 19 gives for `pg_ndistinct`, or `None` for a good value.
    const NDISTINCT: &[(&str, Option<&str>)] = &[
        (r#""#, Some(r#"Input data must be valid JSON."#)),
        (r#"{}"#, Some(r#"Initial element must be an array."#)),
        (r#"[]"#, Some(r#"Item array cannot be empty."#)),
        (r#"[null]"#, Some(r#"Item list elements cannot be null."#)),
        (r#"["x"]"#, Some(r#"Unexpected scalar has been found."#)),
        (r#"[[1]]"#, Some(r#"Array has been found at an unexpected location."#)),
        (r#"[{}]"#, Some(r#"Item must contain "attributes" key."#)),
        (r#"[{"attributes": [1]}]"#, Some(r#"Item must contain "ndistinct" key."#)),
        (
            r#"[{"attributes": [1], "dependency": 2}]"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [], "ndistinct": 4}]"#,
            Some(r#"The "attributes" key must be a non-empty array."#),
        ),
        (r#"[{"attributes": [1,2,3,4,5,6,7,8], "ndistinct": 4}]"#, None),
        (
            r#"[{"attributes": [1], "ndistinct": 4, "x": 1}]"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "ndistinct": 4}]"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4, "degree": 1}]"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (
            r#"[{"attributes": [1], "attributes": [1], "ndistinct": 4}]"#,
            Some(r#"Multiple "attributes" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1, 3], "ndistinct": 4}, {"attributes": [1, 3], "ndistinct": 4}]"#,
            Some(r#"Duplicated "attributes" array has been found: [1, 3]."#),
        ),
        (
            r#"[{"attributes": [3, 1], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: 1 cannot follow 3."#),
        ),
        (
            r#"[{"attributes": [1, null], "ndistinct": 4}]"#,
            Some(r#"Attribute number array cannot be null."#),
        ),
        (
            r#"[{"attributes": {}, "ndistinct": 4}]"#,
            Some(r#"Value of "attributes" must be an array of attribute numbers."#),
        ),
        (
            r#"[{"attributes": [{}], "ndistinct": 4}]"#,
            Some(r#"Attribute lists can only contain attribute numbers."#),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2}x"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (r#"[{"x": [1]}]"#, Some(r#"Only allowed keys are "attributes" and "ndistinct"."#)),
        (r#"[{"x""#, Some(r#"Input data must be valid JSON."#)),
        (
            r#"[{"attributes": [1], "ndistinct": 4}, null]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}] 1"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}, {"attributes": [1], "ndistinct": 4, {}}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "ndistinct": 4}, {"attributes": [1], "ndistinct": 4}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (r#"[{"attributes": [1, 2], "ndistinct": 4}]"#, None),
        (
            r#"[{"attributes":[2,3],"ndistinct":4},{"attributes":[2,-1],"ndistinct":5},{"attributes":[2,3,-1],"ndistinct":6}]"#,
            None,
        ),
        (r#"[{"ndistinct": 4, "attributes": ["1", "2"]}]"#, None),
        (r#"[{"attributes": [1, 2], "ndistinct": "11"}]"#, None),
        (r#"[{"attributes": [ 1 , 2 ] , "ndistinct" : -4 } ]"#, None),
        (r#" "#, Some(r#"Input data must be valid JSON."#)),
        (r#"1"#, Some(r#"Unexpected scalar has been found."#)),
        (r#"[1]"#, Some(r#"Unexpected scalar has been found."#)),
        (r#"[[]]"#, Some(r#"Array has been found at an unexpected location."#)),
        (r#"[{"attributes": [1, 2]}]"#, Some(r#"Item must contain "ndistinct" key."#)),
        (r#"[{"ndistinct": 4}]"#, Some(r#"Item must contain "attributes" key."#)),
        (
            r#"[{"attributes": [1,2,3,4,5,6,7,8,9], "ndistinct": 4}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 2 and no more than 8 attributes."#,
            ),
        ),
        (
            r#"[{"attributes": [1, {}], "ndistinct": 4}]"#,
            Some(r#"Attribute lists can only contain attribute numbers."#),
        ),
        (
            r#"[{"attributes": [1, [2]], "ndistinct": 4}]"#,
            Some(r#"Array has been found at an unexpected location."#),
        ),
        (r#"[{"attributes": 1, "ndistinct": 4}]"#, Some(r#"Unexpected scalar has been found."#)),
        (
            r#"[{"attributes": [1, 2], "ndistinct": {}}]"#,
            Some(r#"Value of "ndistinct" must be an integer."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": []}]"#,
            Some(r#"Array has been found at an unexpected location."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 1.5}]"#,
            Some(r#"Key "ndistinct" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": null}]"#,
            Some(r#"Key "ndistinct" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 99999999999}]"#,
            Some(r#"Key "ndistinct" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4, "x": 1}]"#,
            Some(r#"Only allowed keys are "attributes" and "ndistinct"."#),
        ),
        (
            r#"[{"attributes": [1, 2], "attributes": [1, 2], "ndistinct": 4}]"#,
            Some(r#"Multiple "attributes" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4, "ndistinct": 4}]"#,
            Some(r#"Multiple "ndistinct" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1, 0], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: 0."#),
        ),
        (
            r#"[{"attributes": [1, -9], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: -9."#),
        ),
        (r#"[{"attributes": [1, -8], "ndistinct": 4}]"#, None),
        (
            r#"[{"attributes": [2, 1], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: 1 cannot follow 2."#),
        ),
        (
            r#"[{"attributes": [-1, -1], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: -1 cannot follow -1."#),
        ),
        (
            r#"[{"attributes": [-1, 1], "ndistinct": 4}]"#,
            Some(r#"Invalid "attributes" element has been found: 1 cannot follow -1."#),
        ),
        (
            r#"[{"attributes": [1, 99999], "ndistinct": 4}]"#,
            Some(r#"Key "attributes" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1, true], "ndistinct": 4}]"#,
            Some(r#"Key "attributes" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}, {"attributes": [1, 2], "ndistinct": 5}]"#,
            Some(r#"Duplicated "attributes" array has been found: [1, 2]."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}, {"attributes": [1, 3], "ndistinct": 5}]"#,
            Some(r#""attributes" array [1, 3] must be a subset of array [1, 2]."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}, {"attributes": [1, 2, 3], "ndistinct": 5}, {"attributes": [1, 4], "ndistinct": 5}]"#,
            Some(r#""attributes" array [1, 4] must be a subset of array [1, 2, 3]."#),
        ),
        (r#"[{"attributes": [1, 2], "ndistinct": 4}"#, Some(r#"Input data must be valid JSON."#)),
        (r#"[{"attributes": [1, 2]}x"#, Some(r#"Input data must be valid JSON."#)),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}] x"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}, null]"#,
            Some(r#"Item list elements cannot be null."#),
        ),
        (
            r#"[{"attributes": [1, 2], "ndistinct": 4}, {"attributes": [1, 2], "ndistinct": 4, {}}]"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (r#"[{"attributes": [" 1 ", "+2"], "ndistinct": "0x10"}]"#, None),
        (
            r#"[{"attributes": [1_0, 2], "ndistinct": 4}]"#,
            Some(r#"Input data must be valid JSON."#),
        ),
    ];

    /// The input and the detail that `pg_input_error_info` of PostgreSQL 19 gives for `pg_dependencies`, or `None` for a good value.
    const DEPENDENCIES: &[(&str, Option<&str>)] = &[
        (r#""#, Some(r#"Input data must be valid JSON."#)),
        (r#"{}"#, Some(r#"Initial element must be an array."#)),
        (r#"[]"#, Some(r#"Item array cannot be empty."#)),
        (r#"[null]"#, Some(r#"Item list elements cannot be null."#)),
        (r#"["x"]"#, Some(r#"Unexpected scalar has been found."#)),
        (r#"[[1]]"#, Some(r#"Array has been found at an unexpected location."#)),
        (r#"[{}]"#, Some(r#"Item must contain "attributes" key."#)),
        (r#"[{"attributes": [1]}]"#, Some(r#"Item must contain "dependency" key."#)),
        (r#"[{"attributes": [1], "dependency": 2}]"#, Some(r#"Item must contain "degree" key."#)),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": {}}]"#,
            Some(r#"Value of "degree" must be an integer."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": {}, "degree": 1}]"#,
            Some(r#"Value of "dependency" must be an integer."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": "x"}]"#,
            Some(r#"Key "degree" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1e400}]"#,
            Some(r#"Key "degree" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 0, "degree": 1}]"#,
            Some(r#"Key "dependency" has an incorrect value: 0."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": -9, "degree": 1}]"#,
            Some(r#"Key "dependency" has an incorrect value: -9."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 99999, "degree": 1}]"#,
            Some(r#"Key "dependency" has an incorrect value."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 1, "degree": 1}]"#,
            Some(r#"Item "dependency" with value 1 has been found in the "attributes" list."#),
        ),
        (
            r#"[{"attributes": [], "dependency": 2, "degree": 1}]"#,
            Some(r#"The "attributes" key must be a non-empty array."#),
        ),
        (
            r#"[{"attributes": [1,2,3,4,5,6,7,8], "dependency": -1, "degree": 1}]"#,
            Some(
                r#"The "attributes" key must contain an array of at least 1 and no more than 7 elements."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1, "x": 1}]"#,
            Some(r#"Only allowed keys are "attributes", "dependency", and "degree"."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "dependency": 2, "degree": 1}]"#,
            Some(r#"Multiple "dependency" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1, "degree": 1}]"#,
            Some(r#"Multiple "degree" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1], "attributes": [1], "dependency": 2, "degree": 1}]"#,
            Some(r#"Multiple "attributes" keys are not allowed."#),
        ),
        (
            r#"[{"attributes": [1, 3], "dependency": 2, "degree": 1}, {"attributes": [1, 3], "dependency": 2, "degree": 0.5}]"#,
            Some(
                r#"Duplicated "attributes" array has been found: [1, 3] for key "dependency" and value 2."#,
            ),
        ),
        (
            r#"[{"attributes": [1, 3], "dependency": 2, "degree": 1}, {"attributes": [1, 3], "dependency": 4, "degree": 0.5}]"#,
            None,
        ),
        (
            r#"[{"attributes": [3, 1], "dependency": 2, "degree": 1}]"#,
            Some(r#"Invalid "attributes" element has been found: 1 cannot follow 3."#),
        ),
        (
            r#"[{"attributes": [1, null], "dependency": 2, "degree": 1}]"#,
            Some(r#"Attribute number array cannot be null."#),
        ),
        (
            r#"[{"attributes": {}, "dependency": 2, "degree": 1}]"#,
            Some(r#"Value of "attributes" must be an array of attribute numbers."#),
        ),
        (
            r#"[{"attributes": [{}], "dependency": 2, "degree": 1}]"#,
            Some(r#"Attribute lists can only contain attribute numbers."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": [2], "degree": 1}]"#,
            Some(r#"Array has been found at an unexpected location."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1}"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (r#"[{"attributes": [1], "dependency": 2}x"#, Some(r#"Input data must be valid JSON."#)),
        (
            r#"[{"x": [1]}]"#,
            Some(r#"Only allowed keys are "attributes", "dependency", and "degree"."#),
        ),
        (r#"[{"x""#, Some(r#"Input data must be valid JSON."#)),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1}, null]"#,
            Some(r#"Item list elements cannot be null."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1}] 1"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1}, {"attributes": [1], "dependency": 2, "degree": 1, {}}]"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (
            r#"[{"attributes": [1], "dependency": 2, "degree": 1}, {"attributes": [1], "dependency": 2, "degree": 1}]"#,
            Some(
                r#"Duplicated "attributes" array has been found: [1] for key "dependency" and value 2."#,
            ),
        ),
        (
            r#"[{"attributes": [1], "dependency": "\u0000", "degree": 1}]"#,
            Some(r#"Input data must be valid JSON."#),
        ),
        (r#"[{"attributes": [1], "dependency": 2, "degree": 1}]"#, None),
    ];

    /// The detail of the error for `input`, or `None` when `input` is a good value.
    fn detail(result: Result<Vec<u8>, TypeError>) -> Option<String> {
        match result {
            Ok(_) => None,
            Err(error) => {
                assert_eq!(error.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
                Some(error.detail.unwrap_or_default())
            }
        }
    }

    #[test]
    fn ndistinct_details() {
        for (input, want) in NDISTINCT {
            assert_eq!(detail(pg_ndistinct_in(input)).as_deref(), *want, "{input}");
        }
    }

    #[test]
    fn dependencies_details() {
        for (input, want) in DEPENDENCIES {
            assert_eq!(detail(pg_dependencies_in(input)).as_deref(), *want, "{input}");
        }
    }

    #[test]
    fn round_trip() {
        let mut out = Vec::new();
        let text = r#"[{"attributes": [2, 3], "ndistinct": 4}, {"attributes": [2, -1], "ndistinct": 5}, {"attributes": [2, 3, -1], "ndistinct": 6}]"#;
        pg_ndistinct_out(&pg_ndistinct_in(text).unwrap(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), text);
        let mut out = Vec::new();
        let text = r#"[{"attributes": [1, 2], "dependency": -1, "degree": 1.000000}, {"attributes": [1], "dependency": 3, "degree": 0.250000}]"#;
        pg_dependencies_out(&pg_dependencies_in(text).unwrap(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), text);
    }
}

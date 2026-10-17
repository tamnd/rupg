//! The executor of `jsonpath` from `jsonpath_exec.c`: `jsonb_path_exists`, `jsonb_path_match`, `jsonb_path_query`, `jsonb_path_query_array`, `jsonb_path_query_first`, their `_tz` forms, and the operators `@?` and `@@`.
//!
//! A path runs on the items of a `jsonb` value. An item is a scalar, or an array or an object in a parsed document. In the lax mode, a step unwraps an array and wraps a scalar, and the structural errors are not errors. A call that is not silent gives the error of a step to the caller. In a silent call, a step gives the result "error", and a predicate takes that result as unknown.
//!
//! The datetime methods parse a string in the standard mode of `to_timestamp`. The value of a datetime item has its type, and the text of the item is in the XSD style. The `_tz` functions can cast between the types with and without a zone, with the zone of the session. The other functions give an error for these casts.
//!
//! `.keyvalue()` gives each pair an `id` from the place of the object in the binary form of its document, as PostgreSQL does. Thus [`Doc::layout`] computes the layout of `convertToJsonb`.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::io::Write;
use std::rc::Rc;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    DateFormat, DateOrder, DateStyle, FixedZone, JSONPATH_LAST, JsonItem, JsonNode, JsonPath,
    JsonTree, Numeric, NumericSign, PathItem, PathOp, UNIX_TO_POSTGRES_USECS, USECS_PER_SEC, Value,
    adjust_time, adjust_timestamp, bool_in, date_out, float8_in, int4_in, int8_in, json_escape,
    jsonpath_regex_flags, local_offset, numeric_in, numeric_out, oid, time_out, timestamp_out,
    timestamptz_out,
};

use crate::cast::midnight;
use crate::compare::{compare, datetime_order};
use crate::formatting::parse_datetime;
use crate::json::text;
use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a `jsonpath` function by its `prosrc`. The `_tz` forms differ only in the casts of the datetime values.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "jsonb_path_exists" | "jsonb_path_exists_opr" => |call, args| exists(call, args, false),
        "jsonb_path_exists_tz" => |call, args| exists(call, args, true),
        "jsonb_path_match" | "jsonb_path_match_opr" => |call, args| path_match(call, args, false),
        "jsonb_path_match_tz" => |call, args| path_match(call, args, true),
        "jsonb_path_query_array" => |call, args| query_array(call, args, false),
        "jsonb_path_query_array_tz" => |call, args| query_array(call, args, true),
        "jsonb_path_query_first" => |call, args| query_first(call, args, false),
        "jsonb_path_query_first_tz" => |call, args| query_first(call, args, true),
        _ => return None,
    })
}

/// The ISO formats that the datetime methods with no template try in order: `date`, `timetz`, `time`, `timestamptz` and `timestamp`, and the timestamps with `T` that `to_json` gives.
const ISO_FORMATS: [&str; 13] = [
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZ",
    "HH24:MI:SSTZ",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZ",
    "yyyy-mm-dd HH24:MI:SSTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZ",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

/// The depth of the steps that a path takes before it stops with the error of `check_stack_depth`.
const MAX_DEPTH: usize = 2_000;

/// The multiplier of the id of the base object in the `id` of `.keyvalue()`.
const BASE_ID_FACTOR: i64 = 10_000_000_000;

/// A `jsonb` value that the items of a path refer into.
struct Doc {
    text: String,
    nodes: Vec<JsonNode>,
    /// The place of each array and object in the binary form, from [`Doc::layout`].
    offsets: OnceCell<Vec<i64>>,
}

impl Doc {
    /// Parses the text of a `jsonb` value.
    fn parse(text: String) -> Result<Rc<Doc>> {
        let nodes = JsonTree::parse(&text).map_err(type_error)?.into_nodes();
        Ok(Rc::new(Doc { text, nodes, offsets: OnceCell::new() }))
    }

    fn item(&self, at: usize) -> &JsonItem {
        &self.nodes[at].item
    }

    /// The text of a node, which is in the form of `jsonb_out`.
    fn text(&self, at: usize) -> &str {
        let node = &self.nodes[at];
        &self.text[node.start..node.end]
    }

    /// `fillJsonbValue`: the item of a node. A scalar becomes a scalar item.
    fn get(self: &Rc<Self>, at: usize) -> Result<Item> {
        Ok(match self.item(at) {
            JsonItem::Null => Item::Null,
            JsonItem::Bool(b) => Item::Bool(*b),
            JsonItem::Number => Item::Numeric(numeric_in(self.text(at), -1).map_err(type_error)?),
            JsonItem::String(s) => Item::String(s.clone()),
            JsonItem::Array(_) | JsonItem::Object(_) => Item::Node(Rc::clone(self), at),
        })
    }

    /// The node of the value of the key `name`, when the node is an object. The keys of `jsonb` are unique.
    fn field(&self, at: usize, name: &str) -> Option<usize> {
        match self.item(at) {
            JsonItem::Object(fields) => fields.iter().find(|(k, _)| k == name).map(|f| f.1),
            _ => None,
        }
    }

    /// The nodes of the elements of an array or of the values of an object, in the order of the binary form.
    fn children(&self, at: usize) -> Vec<usize> {
        match self.item(at) {
            JsonItem::Array(items) => items.clone(),
            JsonItem::Object(fields) => fields.iter().map(|f| f.1).collect(),
            _ => Vec::new(),
        }
    }

    /// The place of an array or an object in the binary form, from the start of the root container.
    fn offset(&self, at: usize) -> i64 {
        self.offsets.get_or_init(|| self.layout())[at]
    }

    /// `convertToJsonb`: the places of the arrays and the objects. The binary form starts with a header of 4 bytes. A container and a number start at a multiple of 4. A container has a header of 4 bytes and an entry of 4 bytes for each element, or for each key and each value. Then the data of the elements follows, or the keys and then the values.
    fn layout(&self) -> Vec<i64> {
        let mut offsets = vec![0; self.nodes.len()];
        self.place(0, 4, &mut offsets);
        offsets
    }

    /// Puts the node at `pos` and gives the end of its data.
    fn place(&self, at: usize, pos: usize, offsets: &mut [i64]) -> usize {
        let mut start = pos.next_multiple_of(4);
        let (header, children) = match self.item(at) {
            JsonItem::Null | JsonItem::Bool(_) => return pos,
            JsonItem::String(s) => return pos + s.len(),
            JsonItem::Number => {
                return start + numeric_in(self.text(at), -1).map_or(8, |n| numeric_size(&n));
            }
            JsonItem::Array(items) => (4 + 4 * items.len(), items.clone()),
            JsonItem::Object(fields) => {
                let keys: usize = fields.iter().map(|f| f.0.len()).sum();
                (4 + 8 * fields.len() + keys, fields.iter().map(|f| f.1).collect())
            }
        };
        offsets[at] = i64::try_from(start - 4).unwrap_or(i64::MAX);
        start += header;
        for child in children {
            start = self.place(child, start, offsets);
        }
        start
    }
}

/// `VARSIZE` of a `numeric` that `make_result` makes: the header of the varlena, the short or the long header of the number, and the digits.
fn numeric_size(n: &Numeric) -> usize {
    let short = n.dscale() <= 0x3f && (-64..=63).contains(&n.weight());
    4 + if short { 2 } else { 4 } + 2 * n.digits().len()
}

/// The text of a number as `numeric_out` gives it.
fn numeric_text(n: &Numeric) -> String {
    let mut out = Vec::new();
    numeric_out(n, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// The number as an `integer`, rounded as `numeric_int4_safe` rounds it.
fn int4(n: &Numeric) -> Option<i32> {
    n.to_integer("integer").ok().and_then(|v| i32::try_from(v).ok())
}

/// True for `NaN` and the infinities.
fn is_special(n: &Numeric) -> bool {
    matches!(n.sign(), NumericSign::NaN | NumericSign::Infinity | NumericSign::NegativeInfinity)
}

/// An SQL/JSON item: a `JsonbValue` with the scalars extracted.
#[derive(Clone)]
enum Item {
    Null,
    Bool(bool),
    Numeric(Numeric),
    String(String),
    /// An array or an object in a document.
    Node(Rc<Doc>, usize),
    /// A `date`, `time`, `timetz`, `timestamp` or `timestamptz` value, and its text.
    Datetime(Value, String),
}

impl Item {
    /// The elements of the item, when it is an array.
    fn array(&self) -> Option<(&Rc<Doc>, usize, usize)> {
        match self {
            Item::Node(doc, at) => match doc.item(*at) {
                JsonItem::Array(items) => Some((doc, *at, items.len())),
                _ => None,
            },
            _ => None,
        }
    }

    fn is_array(&self) -> bool {
        self.array().is_some()
    }

    /// The document and the node of the item, when it is an object.
    fn object(&self) -> Option<(&Rc<Doc>, usize)> {
        match self {
            Item::Node(doc, at) if matches!(doc.item(*at), JsonItem::Object(_)) => Some((doc, *at)),
            _ => None,
        }
    }

    /// `JsonbTypeName`.
    fn type_name(&self) -> &'static str {
        match self {
            Item::Null => "null",
            Item::Bool(_) => "boolean",
            Item::Numeric(_) => "number",
            Item::String(_) => "string",
            Item::Node(..) if self.is_array() => "array",
            Item::Node(..) => "object",
            Item::Datetime(value, _) => match value {
                Value::Date(_) => "date",
                Value::Time(_) => "time without time zone",
                Value::TimeTz(..) => "time with time zone",
                Value::Timestamp(_) => "timestamp without time zone",
                _ => "timestamp with time zone",
            },
        }
    }

    /// Adds the text of the item as `jsonb`.
    fn write(&self, out: &mut String) {
        match self {
            Item::Null => out.push_str("null"),
            Item::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Item::Numeric(n) => out.push_str(&numeric_text(n)),
            Item::String(s) | Item::Datetime(_, s) => json_escape(s, out),
            Item::Node(doc, at) => out.push_str(doc.text(*at)),
        }
    }

    /// The item as a `jsonb` value.
    fn to_value(&self) -> Value {
        let mut out = String::new();
        self.write(&mut out);
        Value::Text(out)
    }
}

/// `JsonPathExecResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Res {
    Ok,
    NotFound,
    Error,
}

/// `JsonPathBool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truth {
    False,
    True,
    Unknown,
}

impl Truth {
    fn of(b: bool) -> Truth {
        if b { Truth::True } else { Truth::False }
    }
}

/// The list of the items that a step finds, or `None` when the caller only wants to know if there is an item.
type Found<'f> = Option<&'f mut Vec<Item>>;

/// The test of a predicate on an item of the left operand and an item of the right operand.
type Test<'t> = &'t mut dyn FnMut(&Item, Option<&Item>) -> Result<Truth>;

/// `JsonBaseObjectInfo`: the container that the ids of `.keyvalue()` count from, and its id.
#[derive(Clone)]
struct Base {
    node: Option<(Rc<Doc>, usize)>,
    id: i64,
}

impl Base {
    /// `setBaseObject`.
    fn of(item: &Item, id: i64) -> Base {
        let node = match item {
            Item::Node(doc, at) => Some((Rc::clone(doc), *at)),
            _ => None,
        };
        Base { node, id }
    }
}

/// The arguments of `executeAnyItem` that stay the same at each level.
struct Walk<'c> {
    /// The steps to run on each item, or no steps to add the items.
    chain: &'c [PathItem],
    first: u32,
    last: u32,
    ignore_structural: bool,
    unwrap: bool,
}

impl Walk<'_> {
    /// The walk of one level.
    fn one(chain: &[PathItem], unwrap: bool) -> Walk<'_> {
        Walk { chain, first: 1, last: 1, ignore_structural: false, unwrap }
    }
}

/// The error of `check_stack_depth`.
fn too_deep() -> Error {
    Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.")
}

/// The first item of an operand.
fn head(chain: &[PathItem]) -> Result<&PathItem> {
    chain.first().ok_or_else(|| Error::internal("empty jsonpath expression"))
}

/// The variables of a path.
enum Vars<'a> {
    /// An operator, which has no variables.
    None,
    /// The object of the argument `vars` of a function.
    Object(Rc<Doc>),
    /// The `PASSING` clause of a SQL/JSON query function.
    Passing(&'a [PathVar]),
}

/// A value of the `PASSING` clause of a SQL/JSON query function: its name, its type and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct PathVar {
    pub name: String,
    pub ty: u32,
    pub value: Value,
}

/// An item that a SQL/JSON query function finds.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlJsonItem {
    Null,
    Bool(bool),
    Numeric(Numeric),
    String(String),
    /// A `date`, `time`, `timetz`, `timestamp` or `timestamptz` value, and its text in `jsonb`.
    Datetime(Value, String),
    /// An array or an object, as the text of `jsonb`.
    Container(String),
}

impl SqlJsonItem {
    /// The text of the item as `jsonb`.
    pub fn jsonb(&self) -> String {
        let mut out = String::new();
        match self {
            SqlJsonItem::Null => out.push_str("null"),
            SqlJsonItem::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            SqlJsonItem::Numeric(n) => out.push_str(&numeric_text(n)),
            SqlJsonItem::String(s) | SqlJsonItem::Datetime(_, s) => json_escape(s, &mut out),
            SqlJsonItem::Container(text) => out.push_str(text),
        }
        out
    }
}

/// `JsonItemFromDatum`: the item of a value of `PASSING`.
fn passing_item(call: &Call<'_>, var: &PathVar) -> Result<Item> {
    Ok(match (var.ty, &var.value) {
        (_, Value::Null) => Item::Null,
        (oid::BOOL, Value::Bool(b)) => Item::Bool(*b),
        (oid::NUMERIC, Value::Numeric(n)) => Item::Numeric(n.clone()),
        (oid::INT2 | oid::INT4 | oid::INT8, value) => {
            Item::Numeric(Numeric::from_integer(value.as_i64().ok_or_else(bad_value)?.into()))
        }
        (oid::FLOAT4, value @ Value::Float4(_)) => {
            match crate::cast::float4_to_numeric(call, std::slice::from_ref(value))? {
                Value::Numeric(n) => Item::Numeric(n),
                _ => return Err(bad_value()),
            }
        }
        (oid::FLOAT8, Value::Float8(f)) => Item::Numeric(Numeric::from_f64(*f)),
        (oid::TEXT | oid::VARCHAR, Value::Text(s)) => Item::String(s.clone()),
        (oid::DATE | oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ, value) => {
            Item::Datetime(value.clone(), datetime_text(value, 0)?)
        }
        (oid::JSONB, Value::Text(text)) => Doc::parse(text.clone())?.get(0)?,
        (oid::JSON, Value::Text(text)) => {
            let Value::Text(text) = crate::input(oid::JSONB, text, -1, call.session)? else {
                return Err(bad_value());
            };
            Doc::parse(text)?.get(0)?
        }
        (ty, _) => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "could not convert value of type {} to jsonpath",
                    crate::type_message_name(ty, call.session)
                ),
            ));
        }
    })
}

/// `JsonPathExecContext`.
struct Exec<'a> {
    call: &'a Call<'a>,
    vars: Vars<'a>,
    lax: bool,
    ignore_structural: bool,
    throw: bool,
    root: Item,
    current: Item,
    base: Base,
    last_id: i64,
    /// The size of the innermost array of a subscript, for `last`.
    innermost: Option<usize>,
    depth: usize,
    /// True for the `_tz` functions, which can cast a datetime value with the zone of the session.
    use_tz: bool,
}

impl Exec<'_> {
    /// `RETURN_ERROR`: the error when the call is not silent, or the result "error".
    fn fail(&self, error: Error) -> Result<Res> {
        if self.throw { Err(error) } else { Ok(Res::Error) }
    }

    /// [`Exec::fail`] for a step that gives a value: `None` is the result "error".
    fn fail_value<T>(&self, error: Error) -> Result<Option<T>> {
        if self.throw { Err(error) } else { Ok(None) }
    }

    /// The error of a method on an item that it does not take.
    fn method_fail(&self, state: SqlState, op: PathOp, what: &str) -> Result<Res> {
        self.fail(Error::new(
            state,
            format!("jsonpath item method .{}() can only be applied to {what}", op.name()),
        ))
    }

    /// The error of a method for an argument that it cannot cast.
    fn invalid_argument(&self, text: &str, op: PathOp, sqltype: &str) -> Result<Res> {
        self.fail(Error::new(
            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
            format!(
                "argument \"{text}\" of jsonpath item method .{}() is invalid for type {sqltype}",
                op.name()
            ),
        ))
    }

    /// Runs `f` with the errors silent, as the `NoThrow` functions do.
    fn quiet<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let throw = std::mem::replace(&mut self.throw, false);
        let out = f(self);
        self.throw = throw;
        out
    }

    /// `executeItem`.
    fn item(&mut self, chain: &[PathItem], jb: &Item, found: Found<'_>) -> Result<Res> {
        self.target(chain, jb, found, self.lax)
    }

    /// `executeNextItem`: runs `rest` on `v`, or adds `v` to `found` when there are no more steps.
    fn next(&mut self, rest: &[PathItem], v: Item, found: Found<'_>) -> Result<Res> {
        if !rest.is_empty() {
            return self.item(rest, &v, found);
        }
        if let Some(found) = found {
            found.push(v);
        }
        Ok(Res::Ok)
    }

    /// `executeItemOptUnwrapTarget`.
    fn target(
        &mut self,
        chain: &[PathItem],
        jb: &Item,
        found: Found<'_>,
        unwrap: bool,
    ) -> Result<Res> {
        if self.depth >= MAX_DEPTH {
            return Err(too_deep());
        }
        self.depth += 1;
        let res = self.step(chain, jb, found, unwrap);
        self.depth -= 1;
        res
    }

    /// `executeItemUnwrapTargetArray`: runs the steps on each element of the array, with no unwrap of the elements.
    fn unwrap_target(&mut self, chain: &[PathItem], jb: &Item, found: Found<'_>) -> Result<Res> {
        let Some((doc, at, _)) = jb.array() else { return Err(bad_value()) };
        self.any(&Walk::one(chain, false), doc, at, found, 1)
    }

    /// The body of `executeItemOptUnwrapTarget` for the first step of `chain`.
    fn step(
        &mut self,
        chain: &[PathItem],
        jb: &Item,
        mut found: Found<'_>,
        unwrap: bool,
    ) -> Result<Res> {
        let (jsp, rest) = (head(chain)?, &chain[1..]);
        match jsp {
            PathItem::Null
            | PathItem::Bool(_)
            | PathItem::Numeric(_)
            | PathItem::String(_)
            | PathItem::Variable(_) => {
                // Skip the evaluation, but not for a variable, which must give the error when it is missing.
                if rest.is_empty() && found.is_none() && !matches!(jsp, PathItem::Variable(_)) {
                    return Ok(Res::Ok);
                }
                let base = self.base.clone();
                let v = self.literal(jsp)?;
                let res = self.next(rest, v, found);
                self.base = base;
                res
            }
            PathItem::Binary(
                op @ (PathOp::Add | PathOp::Sub | PathOp::Mul | PathOp::Div | PathOp::Mod),
                l,
                r,
            ) => self.binary_arith(*op, l, r, rest, jb, found),
            PathItem::Unary(op @ (PathOp::Plus | PathOp::Minus), arg) => {
                self.unary_arith(*op, arg, rest, jb, found)
            }
            PathItem::Unary(PathOp::Filter, cond) => {
                if unwrap && jb.is_array() {
                    return self.unwrap_target(chain, jb, found);
                }
                if self.nested_truth(cond, jb)? != Truth::True {
                    return Ok(Res::NotFound);
                }
                self.next(rest, jb.clone(), found)
            }
            PathItem::Binary(..) | PathItem::Unary(..) | PathItem::LikeRegex { .. } => {
                let st = self.truth(jsp, jb)?;
                self.append_bool(rest, found, st)
            }
            PathItem::AnyArray => {
                if let Some((doc, at, _)) = jb.array() {
                    return self.any(&Walk::one(rest, self.lax), doc, at, found, 1);
                }
                if self.lax {
                    return self.next(rest, jb.clone(), found);
                }
                if !self.ignore_structural {
                    return self.fail(Error::new(
                        SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                        "jsonpath wildcard array accessor can only be applied to an array",
                    ));
                }
                Ok(Res::NotFound)
            }
            PathItem::AnyKey => {
                if let Some((doc, at)) = jb.object() {
                    return self.any(&Walk::one(rest, self.lax), doc, at, found, 1);
                }
                if unwrap && jb.is_array() {
                    return self.unwrap_target(chain, jb, found);
                }
                if !self.ignore_structural {
                    return self.fail(Error::new(
                        SqlState::SQL_JSON_OBJECT_NOT_FOUND,
                        "jsonpath wildcard member accessor can only be applied to an object",
                    ));
                }
                Ok(Res::NotFound)
            }
            PathItem::IndexArray(subscripts) => {
                if !jb.is_array() && !self.lax {
                    if !self.ignore_structural {
                        return self.fail(Error::new(
                            SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                            "jsonpath array accessor can only be applied to an array",
                        ));
                    }
                    return Ok(Res::NotFound);
                }
                let saved = self.innermost;
                let res = self.subscripts(subscripts, rest, jb, found);
                self.innermost = saved;
                res
            }
            PathItem::Any { first, last } => {
                let mut res = Res::NotFound;
                // First try with no steps between.
                if *first == 0 {
                    let saved = std::mem::replace(&mut self.ignore_structural, true);
                    res = self.next(rest, jb.clone(), found.as_deref_mut())?;
                    self.ignore_structural = saved;
                    if res == Res::Ok && found.is_none() {
                        return Ok(res);
                    }
                }
                if let Item::Node(doc, at) = jb {
                    let walk = Walk {
                        chain: rest,
                        first: *first,
                        last: *last,
                        ignore_structural: true,
                        unwrap: self.lax,
                    };
                    res = self.any(&walk, doc, *at, found, 1)?;
                }
                Ok(res)
            }
            PathItem::Key(name) => {
                if let Some((doc, at)) = jb.object() {
                    if let Some(child) = doc.field(at, name) {
                        return self.next(rest, doc.get(child)?, found);
                    }
                    if !self.ignore_structural {
                        return self.fail(Error::new(
                            SqlState::SQL_JSON_MEMBER_NOT_FOUND,
                            format!("JSON object does not contain key \"{name}\""),
                        ));
                    }
                    return Ok(Res::NotFound);
                }
                if unwrap && jb.is_array() {
                    return self.unwrap_target(chain, jb, found);
                }
                if !self.ignore_structural {
                    return self.fail(Error::new(
                        SqlState::SQL_JSON_MEMBER_NOT_FOUND,
                        "jsonpath member accessor can only be applied to an object",
                    ));
                }
                Ok(Res::NotFound)
            }
            PathItem::Current => {
                let current = self.current.clone();
                self.next(rest, current, found)
            }
            PathItem::Root => {
                let root = self.root.clone();
                let base = std::mem::replace(&mut self.base, Base::of(&root, 0));
                let res = self.next(rest, root, found);
                self.base = base;
                res
            }
            PathItem::Last => {
                let Some(size) = self.innermost else {
                    return Err(Error::internal(
                        "evaluating jsonpath LAST outside of array subscript",
                    ));
                };
                if rest.is_empty() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let last = i128::try_from(size).unwrap_or(i128::MAX) - 1;
                self.next(rest, Item::Numeric(Numeric::from_integer(last)), found)
            }
            PathItem::Method(op, args) => self.method(*op, args, chain, jb, found, unwrap),
        }
    }

    /// `getJsonPathItem`: the value of a literal or of a variable. A variable makes the object of the variables the base object.
    fn literal(&mut self, jsp: &PathItem) -> Result<Item> {
        Ok(match jsp {
            PathItem::Null => Item::Null,
            PathItem::Bool(b) => Item::Bool(*b),
            PathItem::Numeric(n) => Item::Numeric(n.clone()),
            PathItem::String(s) => Item::String(s.clone()),
            PathItem::Variable(name) => {
                let missing = || {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("could not find jsonpath variable \"{name}\""),
                    )
                };
                match &self.vars {
                    Vars::None => return Err(missing()),
                    Vars::Object(vars) => {
                        let vars = Rc::clone(vars);
                        let child = vars.field(0, name).ok_or_else(missing)?;
                        self.base = Base { node: Some((Rc::clone(&vars), 0)), id: 1 };
                        vars.get(child)?
                    }
                    Vars::Passing(vars) => {
                        let at = vars.iter().position(|v| v.name == *name).ok_or_else(missing)?;
                        let item = passing_item(self.call, &vars[at])?;
                        self.base = Base::of(&item, i64::try_from(at + 1).unwrap_or(i64::MAX));
                        item
                    }
                }
            }
            _ => return Err(Error::internal("unexpected jsonpath item type")),
        })
    }

    /// The subscripts of `jpiIndexArray` on an array, or on a scalar that the lax mode wraps.
    fn subscripts(
        &mut self,
        subscripts: &[(Vec<PathItem>, Option<Vec<PathItem>>)],
        rest: &[PathItem],
        jb: &Item,
        mut found: Found<'_>,
    ) -> Result<Res> {
        let array = jb.array();
        let size = array.map_or(1, |a| a.2);
        self.innermost = Some(size);
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        let mut res = Res::NotFound;
        for (from, to) in subscripts {
            let Some(index_from) = self.index(from, jb)? else {
                res = Res::Error;
                break;
            };
            let index_to = match to {
                Some(to) => match self.index(to, jb)? {
                    Some(index) => index,
                    None => {
                        res = Res::Error;
                        break;
                    }
                },
                None => index_from,
            };
            let (index_from, index_to) = (i64::from(index_from), i64::from(index_to));
            if !self.ignore_structural
                && (index_from < 0 || index_from > index_to || index_to >= size)
            {
                return self.fail(Error::new(
                    SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                    "jsonpath array subscript is out of bounds",
                ));
            }
            res = Res::NotFound;
            for index in index_from.max(0)..=index_to.min(size - 1) {
                let v = match array {
                    None => jb.clone(),
                    Some((doc, at, _)) => {
                        let JsonItem::Array(items) = doc.item(at) else { break };
                        match usize::try_from(index).ok().and_then(|i| items.get(i)) {
                            Some(&child) => doc.get(child)?,
                            None => continue,
                        }
                    }
                };
                if rest.is_empty() && found.is_none() {
                    return Ok(Res::Ok);
                }
                res = self.next(rest, v, found.as_deref_mut())?;
                if res == Res::Error || (res == Res::Ok && found.is_none()) {
                    break;
                }
            }
            if res == Res::Error || (res == Res::Ok && found.is_none()) {
                break;
            }
        }
        Ok(res)
    }

    /// `getArrayIndex`: the subscript as an integer, truncated, or `None` for the result "error".
    fn index(&mut self, chain: &[PathItem], jb: &Item) -> Result<Option<i32>> {
        let mut found = Vec::new();
        if self.item(chain, jb, Some(&mut found))? == Res::Error {
            return Ok(None);
        }
        let [Item::Numeric(n)] = found.as_slice() else {
            return self.fail_value(Error::new(
                SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                "jsonpath array subscript is not a single numeric value",
            ));
        };
        match n.trunc(0).ok().as_ref().and_then(int4) {
            Some(index) => Ok(Some(index)),
            None => self.fail_value(Error::new(
                SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                "jsonpath array subscript is out of integer range",
            )),
        }
    }

    /// `executeAnyItem`: `.**`, `.*` and `[*]` on the container at `at`. `level` is the level of its children.
    fn any(
        &mut self,
        walk: &Walk<'_>,
        doc: &Rc<Doc>,
        at: usize,
        mut found: Found<'_>,
        level: u32,
    ) -> Result<Res> {
        let mut res = Res::NotFound;
        if level > walk.last {
            return Ok(res);
        }
        if self.depth >= MAX_DEPTH {
            return Err(too_deep());
        }
        self.depth += 1;
        for child in doc.children(at) {
            let v = doc.get(child)?;
            let nested = matches!(v, Item::Node(..));
            // `.**{last}` takes only the leaves.
            if level >= walk.first
                || (walk.first == JSONPATH_LAST && walk.last == JSONPATH_LAST && !nested)
            {
                if !walk.chain.is_empty() {
                    let saved = self.ignore_structural;
                    if walk.ignore_structural {
                        self.ignore_structural = true;
                    }
                    res = self.target(walk.chain, &v, found.as_deref_mut(), walk.unwrap)?;
                    self.ignore_structural = saved;
                    if res == Res::Error || (res == Res::Ok && found.is_none()) {
                        break;
                    }
                } else if let Some(found) = found.as_deref_mut() {
                    found.push(v);
                } else {
                    self.depth -= 1;
                    return Ok(Res::Ok);
                }
            }
            if level < walk.last && nested {
                res = self.any(walk, doc, child, found.as_deref_mut(), level + 1)?;
                if res == Res::Error || (res == Res::Ok && found.is_none()) {
                    break;
                }
            }
        }
        self.depth -= 1;
        Ok(res)
    }

    /// `executeItemOptUnwrapResult`: the items of the steps, with each array unwrapped in the lax mode when `unwrap` is true.
    fn sequence(
        &mut self,
        chain: &[PathItem],
        jb: &Item,
        unwrap: bool,
        found: &mut Vec<Item>,
    ) -> Result<Res> {
        if !(unwrap && self.lax) {
            return self.item(chain, jb, Some(found));
        }
        let mut seq = Vec::new();
        if self.item(chain, jb, Some(&mut seq))? == Res::Error {
            return Ok(Res::Error);
        }
        for item in seq {
            match item.array() {
                Some((doc, at, _)) => {
                    self.any(&Walk::one(&[], false), doc, at, Some(found), 1)?;
                }
                None => found.push(item),
            }
        }
        Ok(Res::Ok)
    }

    /// `executeBoolItem`.
    fn truth(&mut self, jsp: &PathItem, jb: &Item) -> Result<Truth> {
        Ok(match jsp {
            PathItem::Binary(PathOp::And, l, r) => {
                let res = self.truth(head(l)?, jb)?;
                if res == Truth::False {
                    return Ok(Truth::False);
                }
                // SQL/JSON says to check the second operand when the first is unknown.
                let res2 = self.truth(head(r)?, jb)?;
                if res2 == Truth::True { res } else { res2 }
            }
            PathItem::Binary(PathOp::Or, l, r) => {
                let res = self.truth(head(l)?, jb)?;
                if res == Truth::True {
                    return Ok(Truth::True);
                }
                let res2 = self.truth(head(r)?, jb)?;
                if res2 == Truth::False { res } else { res2 }
            }
            PathItem::Unary(PathOp::Not, arg) => match self.truth(head(arg)?, jb)? {
                Truth::Unknown => Truth::Unknown,
                Truth::True => Truth::False,
                Truth::False => Truth::True,
            },
            PathItem::Unary(PathOp::IsUnknown, arg) => {
                Truth::of(self.truth(head(arg)?, jb)? == Truth::Unknown)
            }
            PathItem::Binary(
                op @ (PathOp::Equal
                | PathOp::NotEqual
                | PathOp::Less
                | PathOp::Greater
                | PathOp::LessOrEqual
                | PathOp::GreaterOrEqual),
                l,
                r,
            ) => {
                let (op, call, use_tz) = (*op, self.call, self.use_tz);
                let mut test = |a: &Item, b: Option<&Item>| match b {
                    Some(b) => compare_items(call, use_tz, op, a, b),
                    None => Ok(Truth::Unknown),
                };
                self.predicate(l, Some(r), jb, true, &mut test)?
            }
            PathItem::Binary(PathOp::StartsWith, l, r) => {
                let mut test = |whole: &Item, initial: Option<&Item>| {
                    Ok(match (whole, initial) {
                        (Item::String(whole), Some(Item::String(initial))) => {
                            Truth::of(whole.as_bytes().starts_with(initial.as_bytes()))
                        }
                        _ => Truth::Unknown,
                    })
                };
                self.predicate(l, Some(r), jb, false, &mut test)?
            }
            PathItem::LikeRegex { expr, pattern, flags } => {
                let cflags = jsonpath_regex_flags(*flags).map_err(type_error)?;
                let mut regex = None;
                let mut test = |s: &Item, _: Option<&Item>| {
                    let Item::String(s) = s else { return Ok(Truth::Unknown) };
                    if regex.is_none() {
                        regex = Some(crate::regex::compile(pattern, cflags)?);
                    }
                    Ok(Truth::of(regex.as_ref().is_some_and(|r| r.is_match(s))))
                };
                self.predicate(expr, None, jb, false, &mut test)?
            }
            PathItem::Unary(PathOp::Exists, arg) => {
                if self.lax {
                    match self.quiet(|x| x.item(arg, jb, None))? {
                        Res::Error => Truth::Unknown,
                        res => Truth::of(res == Res::Ok),
                    }
                } else {
                    // The strict mode needs all the items to check that there is no error.
                    let mut vals = Vec::new();
                    match self.quiet(|x| x.item(arg, jb, Some(&mut vals)))? {
                        Res::Error => Truth::Unknown,
                        _ => Truth::of(!vals.is_empty()),
                    }
                }
            }
            _ => return Err(Error::internal("invalid boolean jsonpath item type")),
        })
    }

    /// `executeNestedBoolItem`: the condition of a filter, with `@` as the item.
    fn nested_truth(&mut self, cond: &[PathItem], jb: &Item) -> Result<Truth> {
        let prev = std::mem::replace(&mut self.current, jb.clone());
        let res = self.truth(head(cond)?, jb);
        self.current = prev;
        res
    }

    /// `executePredicate`: true when a pair of an item of the left operand and an item of the right operand passes the test. The strict mode checks all the pairs for errors.
    fn predicate(
        &mut self,
        l: &[PathItem],
        r: Option<&[PathItem]>,
        jb: &Item,
        unwrap_right: bool,
        test: Test<'_>,
    ) -> Result<Truth> {
        let mut lseq = Vec::new();
        if self.quiet(|x| x.sequence(l, jb, true, &mut lseq))? == Res::Error {
            return Ok(Truth::Unknown);
        }
        let mut rseq = Vec::new();
        if let Some(r) = r
            && self.quiet(|x| x.sequence(r, jb, unwrap_right, &mut rseq))? == Res::Error
        {
            return Ok(Truth::Unknown);
        }
        let (mut error, mut found) = (false, false);
        for lval in &lseq {
            let mut check = |rval: Option<&Item>| -> Result<bool> {
                match test(lval, rval)? {
                    Truth::Unknown => {
                        error = true;
                        Ok(!self.lax)
                    }
                    Truth::True => {
                        found = true;
                        Ok(self.lax)
                    }
                    Truth::False => Ok(false),
                }
            };
            let stop = if r.is_some() {
                let mut stop = false;
                for rval in &rseq {
                    if check(Some(rval))? {
                        stop = true;
                        break;
                    }
                }
                stop
            } else {
                check(None)?
            };
            if stop {
                // An error in the strict mode gives unknown, not success.
                if error && !self.lax {
                    return Ok(Truth::Unknown);
                }
                break;
            }
        }
        Ok(if found {
            Truth::True
        } else if error {
            Truth::Unknown
        } else {
            Truth::False
        })
    }

    /// `appendBoolResult`: the result of a predicate as an item. Unknown is `null`.
    fn append_bool(&mut self, rest: &[PathItem], found: Found<'_>, st: Truth) -> Result<Res> {
        if rest.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        let v = match st {
            Truth::Unknown => Item::Null,
            st => Item::Bool(st == Truth::True),
        };
        self.next(rest, v, found)
    }

    /// `executeBinaryArithmExpr`: the operands must be single numbers. The lax mode unwraps the arrays.
    fn binary_arith(
        &mut self,
        op: PathOp,
        l: &[PathItem],
        r: &[PathItem],
        rest: &[PathItem],
        jb: &Item,
        found: Found<'_>,
    ) -> Result<Res> {
        let (mut lseq, mut rseq) = (Vec::new(), Vec::new());
        if self.sequence(l, jb, true, &mut lseq)? == Res::Error
            || self.sequence(r, jb, true, &mut rseq)? == Res::Error
        {
            return Ok(Res::Error);
        }
        let single = |side: &str| {
            Error::new(
                SqlState::SINGLETON_SQL_JSON_ITEM_REQUIRED,
                format!(
                    "{side} operand of jsonpath operator {} is not a single numeric value",
                    op.name()
                ),
            )
        };
        let [Item::Numeric(a)] = lseq.as_slice() else { return self.fail(single("left")) };
        let [Item::Numeric(b)] = rseq.as_slice() else { return self.fail(single("right")) };
        let v = match op {
            PathOp::Add => a.add(b),
            PathOp::Sub => a.sub(b),
            PathOp::Mul => a.mul(b),
            PathOp::Div => a.div(b),
            _ => a.modulo(b),
        };
        let v = match v {
            Ok(v) => v,
            Err(error) => return self.fail(type_error(error)),
        };
        if rest.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        self.next(rest, Item::Numeric(v), found)
    }

    /// `executeUnaryArithmExpr`: the operator on each number of the operand.
    fn unary_arith(
        &mut self,
        op: PathOp,
        arg: &[PathItem],
        rest: &[PathItem],
        jb: &Item,
        mut found: Found<'_>,
    ) -> Result<Res> {
        let mut seq = Vec::new();
        if self.sequence(arg, jb, true, &mut seq)? == Res::Error {
            return Ok(Res::Error);
        }
        let mut res = Res::NotFound;
        let has_next = !rest.is_empty();
        for val in seq {
            let Item::Numeric(n) = val else {
                if found.is_none() && !has_next {
                    continue;
                }
                return self.fail(Error::new(
                    SqlState::SQL_JSON_NUMBER_NOT_FOUND,
                    format!(
                        "operand of unary jsonpath operator {} is not a numeric value",
                        op.name()
                    ),
                ));
            };
            if found.is_none() && !has_next {
                return Ok(Res::Ok);
            }
            let n = if op == PathOp::Minus { n.negate() } else { n };
            match self.next(rest, Item::Numeric(n), found.as_deref_mut())? {
                Res::Error => return Ok(Res::Error),
                Res::Ok if found.is_none() => return Ok(Res::Ok),
                Res::Ok => res = Res::Ok,
                Res::NotFound => {}
            }
        }
        Ok(res)
    }

    /// The item methods.
    fn method(
        &mut self,
        op: PathOp,
        args: &[PathItem],
        chain: &[PathItem],
        jb: &Item,
        found: Found<'_>,
        unwrap: bool,
    ) -> Result<Res> {
        let rest = &chain[1..];
        match op {
            PathOp::Type => self.next(rest, Item::String(jb.type_name().to_string()), found),
            PathOp::Size => {
                let size = match jb.array() {
                    Some((.., size)) => size,
                    None if !self.lax => {
                        if !self.ignore_structural {
                            return self.method_fail(
                                SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                                op,
                                "an array",
                            );
                        }
                        return Ok(Res::NotFound);
                    }
                    None => 1,
                };
                let size = i128::try_from(size).unwrap_or(i128::MAX);
                self.next(rest, Item::Numeric(Numeric::from_integer(size)), found)
            }
            _ if unwrap && jb.is_array() => self.unwrap_target(chain, jb, found),
            PathOp::Abs | PathOp::Floor | PathOp::Ceiling => {
                let Item::Numeric(n) = jb else {
                    return self.method_fail(
                        SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                        op,
                        "a numeric value",
                    );
                };
                let v = match op {
                    PathOp::Abs => n.abs(),
                    PathOp::Floor => n.whole(false).map_err(type_error)?,
                    _ => n.whole(true).map_err(type_error)?,
                };
                if rest.is_empty() && found.is_none() {
                    return Ok(Res::Ok);
                }
                self.next(rest, Item::Numeric(v), found)
            }
            PathOp::Double => {
                let not_finite = || {
                    Error::new(
                        SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                        format!(
                            "NaN or Infinity is not allowed for jsonpath item method .{}()",
                            op.name()
                        ),
                    )
                };
                let sqltype = "double precision";
                let v = match jb {
                    Item::Numeric(n) => {
                        let text = numeric_text(n);
                        match float8_in(&text) {
                            Err(_) => return self.invalid_argument(&text, op, sqltype),
                            Ok(f) if !f.is_finite() => return self.fail(not_finite()),
                            Ok(_) => jb.clone(),
                        }
                    }
                    Item::String(s) => match float8_in(s) {
                        Err(_) => return self.invalid_argument(s, op, sqltype),
                        Ok(f) if !f.is_finite() => return self.fail(not_finite()),
                        Ok(f) => Item::Numeric(Numeric::from_f64(f)),
                    },
                    _ => {
                        return self.method_fail(
                            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                            op,
                            "a string or numeric value",
                        );
                    }
                };
                self.next(rest, v, found)
            }
            PathOp::Bigint | PathOp::Integer => {
                let sqltype = if op == PathOp::Bigint { "bigint" } else { "integer" };
                let v = match jb {
                    Item::Numeric(n) => {
                        let v = n.to_integer(sqltype).ok().filter(|&v| match op {
                            PathOp::Bigint => i64::try_from(v).is_ok(),
                            _ => i32::try_from(v).is_ok(),
                        });
                        match v {
                            Some(v) => v,
                            None => return self.invalid_argument(&numeric_text(n), op, sqltype),
                        }
                    }
                    Item::String(s) => {
                        let v = match op {
                            PathOp::Bigint => int8_in(s).map(i128::from),
                            _ => int4_in(s).map(i128::from),
                        };
                        match v {
                            Ok(v) => v,
                            Err(_) => return self.invalid_argument(s, op, sqltype),
                        }
                    }
                    _ => {
                        return self.method_fail(
                            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                            op,
                            "a string or numeric value",
                        );
                    }
                };
                self.next(rest, Item::Numeric(Numeric::from_integer(v)), found)
            }
            PathOp::Boolean => {
                let v = match jb {
                    Item::Bool(b) => *b,
                    Item::Numeric(n) => {
                        let text = numeric_text(n);
                        match int4_in(&text) {
                            Ok(v) => v != 0,
                            Err(_) => return self.invalid_argument(&text, op, "boolean"),
                        }
                    }
                    Item::String(s) => match parse_bool(s) {
                        Some(b) => b,
                        None => return self.invalid_argument(s, op, "boolean"),
                    },
                    _ => {
                        return self.method_fail(
                            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                            op,
                            "a boolean, string, or numeric value",
                        );
                    }
                };
                self.next(rest, Item::Bool(v), found)
            }
            PathOp::Decimal | PathOp::Number => self.number(op, args, rest, jb, found),
            PathOp::StringFunc => {
                let v = match jb {
                    Item::String(s) => s.clone(),
                    Item::Numeric(n) => numeric_text(n),
                    Item::Bool(b) => b.to_string(),
                    Item::Datetime(_, text) => text.clone(),
                    _ => {
                        return self.method_fail(
                            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                            op,
                            "a boolean, string, numeric, or datetime value",
                        );
                    }
                };
                self.next(rest, Item::String(v), found)
            }
            PathOp::Replace
            | PathOp::Lower
            | PathOp::Upper
            | PathOp::Ltrim
            | PathOp::Rtrim
            | PathOp::Btrim
            | PathOp::Initcap
            | PathOp::SplitPart => self.string_method(op, args, rest, jb, found),
            PathOp::KeyValue => self.keyvalue(rest, jb, found),
            PathOp::Datetime
            | PathOp::Date
            | PathOp::Time
            | PathOp::TimeTz
            | PathOp::Timestamp
            | PathOp::TimestampTz => self.datetime_method(op, args, rest, jb, found),
            _ => Err(Error::internal("unrecognized jsonpath item type")),
        }
    }

    /// `.decimal()` and `.number()`. `.decimal()` with a precision and a scale gives the value with that typmod.
    fn number(
        &mut self,
        op: PathOp,
        args: &[PathItem],
        rest: &[PathItem],
        jb: &Item,
        found: Found<'_>,
    ) -> Result<Res> {
        let not_finite = || {
            Error::new(
                SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                format!("NaN or Infinity is not allowed for jsonpath item method .{}()", op.name()),
            )
        };
        let (num, text) = match jb {
            Item::Numeric(n) => (n.clone(), None),
            Item::String(s) => match numeric_in(s, -1) {
                Ok(n) => (n, Some(s.as_str())),
                Err(_) => return self.invalid_argument(s, op, "numeric"),
            },
            _ => {
                return self.method_fail(
                    SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                    op,
                    "a string or numeric value",
                );
            }
        };
        if is_special(&num) {
            return self.fail(not_finite());
        }
        let num = match (op, args.first()) {
            (PathOp::Decimal, Some(precision)) => {
                let out_of_range = |what: &str| {
                    Error::new(
                        SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                        format!(
                            "{what} of jsonpath item method .{}() is out of range for type integer",
                            op.name()
                        ),
                    )
                };
                let PathItem::Numeric(precision) = precision else {
                    return Err(Error::internal(
                        "invalid jsonpath item type for .decimal() precision",
                    ));
                };
                let Some(precision) = int4(precision) else {
                    return self.fail(out_of_range("precision"));
                };
                let scale = match args.get(1) {
                    None => 0,
                    Some(PathItem::Numeric(scale)) => match int4(scale) {
                        Some(scale) => scale,
                        None => return self.fail(out_of_range("scale")),
                    },
                    Some(_) => {
                        return Err(Error::internal(
                            "invalid jsonpath item type for .decimal() scale",
                        ));
                    }
                };
                if !(1..=1000).contains(&precision) {
                    return self.fail(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("NUMERIC precision {precision} must be between 1 and 1000"),
                    ));
                }
                if !(-1000..=1000).contains(&scale) {
                    return self.fail(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("NUMERIC scale {scale} must be between -1000 and 1000"),
                    ));
                }
                let typmod = ((precision << 16) | (scale & 0x7ff)) + 4;
                let text = text.map_or_else(|| numeric_text(&num), str::to_string);
                match numeric_in(&text, typmod) {
                    Ok(n) => n,
                    Err(_) => return self.invalid_argument(&text, op, "numeric"),
                }
            }
            _ => num,
        };
        self.next(rest, Item::Numeric(num), found)
    }

    /// `executeStringInternalMethod`: the string functions of SQL on a string.
    fn string_method(
        &mut self,
        op: PathOp,
        args: &[PathItem],
        rest: &[PathItem],
        jb: &Item,
        found: Found<'_>,
    ) -> Result<Res> {
        let Item::String(s) = jb else {
            return self.method_fail(SqlState::INVALID_PARAMETER_VALUE, op, "a string");
        };
        let string = |at: usize| match args.get(at) {
            Some(PathItem::String(s)) => Ok(Value::text(s.as_str())),
            _ => Err(Error::internal(format!(
                "invalid jsonpath item type for .{}() argument",
                op.name()
            ))),
        };
        let mut call_args = vec![Value::text(s.as_str())];
        let src = match op {
            PathOp::Replace => {
                call_args.extend([string(0)?, string(1)?]);
                "replace_text"
            }
            PathOp::Lower => "lower",
            PathOp::Upper => "upper",
            PathOp::Initcap => "initcap",
            PathOp::Ltrim | PathOp::Rtrim | PathOp::Btrim => {
                if !args.is_empty() {
                    call_args.push(string(0)?);
                }
                match op {
                    PathOp::Ltrim => "ltrim",
                    PathOp::Rtrim => "rtrim",
                    _ => "btrim",
                }
            }
            _ => {
                let position = |what: &str| {
                    Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("field position of jsonpath item method .{}() {what}", op.name()),
                    )
                };
                call_args.push(string(0)?);
                let Some(PathItem::Numeric(n)) = args.get(1) else {
                    return Err(Error::internal("invalid jsonpath item type for .split_part()"));
                };
                match int4(n) {
                    None => return self.fail(position("is out of range for type integer")),
                    Some(0) => return self.fail(position("must not be zero")),
                    Some(n) => call_args.push(Value::Int4(n)),
                }
                "split_part"
            }
        };
        let kernel = crate::text::by_src(src).ok_or_else(bad_value)?;
        let v = kernel(self.call, &call_args)?;
        let v = v.as_str().ok_or_else(bad_value)?.to_string();
        if rest.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        self.next(rest, Item::String(v), found)
    }

    /// `executeDateTimeMethod`: `.datetime()` and the methods of the datetime types on a string. With no template, the string must have one of the ISO formats. A method of a type casts the value to the type.
    #[allow(clippy::too_many_lines)]
    fn datetime_method(
        &mut self,
        op: PathOp,
        args: &[PathItem],
        rest: &[PathItem],
        jb: &Item,
        found: Found<'_>,
    ) -> Result<Res> {
        use Value as V;
        let invalid = SqlState::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION;
        let Item::String(s) = jb else { return self.method_fail(invalid, op, "a string") };
        let not_recognized =
            |name: &str| Error::new(invalid, format!("{name} format is not recognized: \"{s}\""));
        let mut precision = None;
        let (mut value, mut tz) = match (op, args.first()) {
            (PathOp::Datetime, Some(template)) => {
                let PathItem::String(template) = template else {
                    return Err(Error::internal(
                        "invalid jsonpath item type for .datetime() argument",
                    ));
                };
                match parse_datetime(self.call, s, template)? {
                    Ok(v) => v,
                    Err(error) => return self.fail(error),
                }
            }
            _ => {
                if op != PathOp::Datetime
                    && op != PathOp::Date
                    && let Some(arg) = args.first()
                {
                    let PathItem::Numeric(n) = arg else {
                        return Err(Error::internal(format!(
                            "invalid jsonpath item type for {} argument",
                            op.name()
                        )));
                    };
                    let Some(n) = int4(n) else {
                        return self.fail(Error::new(
                            invalid,
                            format!(
                                "time precision of jsonpath item method .{}() is out of range for type integer",
                                op.name()
                            ),
                        ));
                    };
                    precision = Some(n);
                }
                let iso = ISO_FORMATS.iter().find_map(|format| {
                    parse_datetime(self.call, s, format).ok().and_then(Result::ok)
                });
                match iso {
                    Some(v) => v,
                    None if op == PathOp::Datetime => {
                        return self.fail(not_recognized("datetime").with_hint(
                            "Use a datetime template argument to specify the input data format.",
                        ));
                    }
                    None => return self.fail(not_recognized(op.name())),
                }
            }
        };
        let call = self.call;
        let use_tz = self.use_tz;
        value = match (op, &value) {
            (PathOp::Datetime, _)
            | (PathOp::Date, V::Date(_))
            | (PathOp::Time, V::Time(_))
            | (PathOp::TimeTz, V::TimeTz(..))
            | (PathOp::Timestamp, V::Timestamp(_))
            | (PathOp::TimestampTz, V::TimestampTz(_)) => value,
            (PathOp::Date, V::Timestamp(_)) => convert(call, "timestamp_date", &value)?,
            (PathOp::Date, V::TimestampTz(_)) => {
                need_tz(use_tz, "timestamptz", "date")?;
                convert(call, "timestamptz_date", &value)?
            }
            (PathOp::Time, V::TimeTz(..)) => {
                need_tz(use_tz, "timetz", "time")?;
                convert(call, "timetz_time", &value)?
            }
            (PathOp::Time, V::Timestamp(_)) => convert(call, "timestamp_time", &value)?,
            (PathOp::Time, V::TimestampTz(_)) => {
                need_tz(use_tz, "timestamptz", "time")?;
                convert(call, "timestamptz_time", &value)?
            }
            (PathOp::TimeTz, V::Time(_)) => {
                need_tz(use_tz, "time", "timetz")?;
                convert(call, "time_timetz", &value)?
            }
            (PathOp::TimeTz, V::TimestampTz(_)) => convert(call, "timestamptz_timetz", &value)?,
            (PathOp::Timestamp, V::Date(_)) => convert(call, "date_timestamp", &value)?,
            (PathOp::Timestamp, V::TimestampTz(_)) => {
                need_tz(use_tz, "timestamptz", "timestamp")?;
                convert(call, "timestamptz_timestamp", &value)?
            }
            (PathOp::TimestampTz, V::Date(date)) => {
                need_tz(use_tz, "date", "timestamptz")?;
                tz = session_west(call, midnight(*date)?)?;
                convert(call, "date_timestamptz", &value)?
            }
            (PathOp::TimestampTz, V::Timestamp(ts)) => {
                need_tz(use_tz, "timestamp", "timestamptz")?;
                tz = session_west(call, *ts)?;
                convert(call, "timestamp_timestamptz", &value)?
            }
            _ => return self.fail(not_recognized(op.name())),
        };
        if let Some(precision) = precision {
            let precision = typmod_check(call, op, precision)?;
            value = match value {
                V::Time(time) => V::Time(adjust_time(time, precision)),
                V::TimeTz(time, zone) => V::TimeTz(adjust_time(time, precision), zone),
                V::Timestamp(ts) => {
                    V::Timestamp(adjust_timestamp(ts, precision).map_err(type_error)?)
                }
                V::TimestampTz(ts) => {
                    V::TimestampTz(adjust_timestamp(ts, precision).map_err(type_error)?)
                }
                v => v,
            };
        }
        if rest.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        let text = datetime_text(&value, tz)?;
        self.next(rest, Item::Datetime(value, text), found)
    }

    /// `executeKeyValueMethod`: an object `{"id": id, "key": key, "value": value}` for each pair. Each object becomes the base object of the steps after it.
    fn keyvalue(&mut self, rest: &[PathItem], jb: &Item, mut found: Found<'_>) -> Result<Res> {
        let Some((doc, at)) = jb.object() else {
            return self.method_fail(
                SqlState::SQL_JSON_OBJECT_NOT_FOUND,
                PathOp::KeyValue,
                "an object",
            );
        };
        let JsonItem::Object(fields) = doc.item(at) else { return Err(bad_value()) };
        let mut offset = doc.offset(at);
        if let Some((base, base_at)) = &self.base.node
            && Rc::ptr_eq(base, doc)
        {
            offset -= doc.offset(*base_at);
        }
        let id = offset + self.base.id * BASE_ID_FACTOR;
        let mut res = Res::NotFound;
        for (key, value) in fields {
            res = Res::Ok;
            if rest.is_empty() && found.is_none() {
                break;
            }
            let mut text = format!("{{\"id\": {id}, \"key\": ");
            json_escape(key, &mut text);
            text.push_str(", \"value\": ");
            text.push_str(doc.text(*value));
            text.push('}');
            let obj = Doc::parse(text)?;
            let base = Base { node: Some((Rc::clone(&obj), 0)), id: self.last_id };
            self.last_id += 1;
            let base = std::mem::replace(&mut self.base, base);
            let next = self.next(rest, Item::Node(obj, 0), found.as_deref_mut());
            self.base = base;
            res = next?;
            if res == Res::Error {
                return Ok(res);
            }
            if res == Res::Ok && found.is_none() {
                break;
            }
        }
        Ok(res)
    }
}

/// The error of `checkTimezoneIsUsedForCast` for a cast that needs the zone of the session in a function that is not `_tz`.
fn need_tz(use_tz: bool, from: &str, to: &str) -> Result<()> {
    if use_tz {
        return Ok(());
    }
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("cannot convert value from {from} to {to} without time zone usage"),
    )
    .with_hint("Use *_tz() function for time zone support."))
}

/// A cast between the datetime types by the `prosrc` of its function.
fn convert(call: &Call<'_>, src: &str, value: &Value) -> Result<Value> {
    let kernel =
        crate::cast::by_src(src).or_else(|| crate::calendar::by_src(src)).ok_or_else(bad_value)?;
    kernel(call, std::slice::from_ref(value))
}

/// `DetermineTimeZoneOffset`: the offset west of UTC of the zone of the session at a local time.
fn session_west(call: &Call<'_>, local: i64) -> Result<i32> {
    let zone = call.session.zone()?;
    let seconds = local.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
    Ok(local_offset(seconds, zone.as_ref()).0)
}

/// `anytime_typmod_check` and `anytimestamp_typmod_check`: an error for a precision below zero, and a warning and the largest precision for a precision above it.
fn typmod_check(call: &Call<'_>, op: PathOp, precision: i32) -> Result<i32> {
    let (kind, zone) = match op {
        PathOp::Time => ("TIME", ""),
        PathOp::TimeTz => ("TIME", " WITH TIME ZONE"),
        PathOp::Timestamp => ("TIMESTAMP", ""),
        _ => ("TIMESTAMP", " WITH TIME ZONE"),
    };
    if precision < 0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{kind}({precision}){zone} precision must not be negative"),
        ));
    }
    if precision > 6 {
        call.session.warn(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{kind}({precision}){zone} precision reduced to maximum allowed, 6"),
        ));
        return Ok(6);
    }
    Ok(precision)
}

/// `JsonEncodeDateTime`: the text of a datetime value in the XSD style. A `timestamptz` shows the time at the offset `tz` west of UTC.
fn datetime_text(value: &Value, tz: i32) -> Result<String> {
    const XSD: DateFormat = DateFormat { style: DateStyle::Xsd, order: DateOrder::Mdy };
    let mut out = Vec::new();
    match *value {
        Value::Date(date) => date_out(date, XSD, &mut out),
        Value::Time(time) => time_out(time, &mut out),
        Value::TimeTz(time, zone) => {
            time_out(time, &mut out);
            // `EncodeTimezone` in the XSD style always shows the minutes.
            let total = zone.unsigned_abs();
            let sign = if zone <= 0 { '+' } else { '-' };
            write!(out, "{sign}{:02}:{:02}", total / 3600, total / 60 % 60)
                .map_err(|_| bad_value())?;
            if total % 60 != 0 {
                write!(out, ":{:02}", total % 60).map_err(|_| bad_value())?;
            }
        }
        Value::Timestamp(ts) => timestamp_out(ts, XSD, &mut out).map_err(type_error)?,
        Value::TimestampTz(ts) => {
            let zone = FixedZone { offset: -tz, abbrev: String::new() };
            timestamptz_out(ts, XSD, &zone, &mut out).map_err(type_error)?;
        }
        _ => return Err(bad_value()),
    }
    String::from_utf8(out).map_err(|_| bad_value())
}

/// `compareDatetime`: the order of two datetime values, or `None` when their types do not compare. A cast that needs the zone of the session is an error when `use_tz` is false.
fn compare_datetime(
    call: &Call<'_>,
    use_tz: bool,
    a: &Value,
    b: &Value,
) -> Result<Option<Ordering>> {
    use Value as V;
    let timetz = |v: &Value| {
        need_tz(use_tz, "time", "timetz")?;
        convert(call, "time_timetz", v)
    };
    Ok(Some(match (a, b) {
        (V::Time(_), V::TimeTz(..)) => compare(0, &timetz(a)?, 0, b)?,
        (V::TimeTz(..), V::Time(_)) => compare(0, a, 0, &timetz(b)?)?,
        (V::Time(_), V::Time(_)) | (V::TimeTz(..), V::TimeTz(..)) => compare(0, a, 0, b)?,
        (V::Time(_) | V::TimeTz(..), _) | (_, V::Time(_) | V::TimeTz(..)) => return Ok(None),
        (V::Date(_), V::TimestampTz(_)) | (V::TimestampTz(_), V::Date(_)) => {
            need_tz(use_tz, "date", "timestamptz")?;
            datetime_order(call, a, b)?
        }
        (V::Timestamp(_), V::TimestampTz(_)) | (V::TimestampTz(_), V::Timestamp(_)) => {
            need_tz(use_tz, "timestamp", "timestamptz")?;
            datetime_order(call, a, b)?
        }
        _ => datetime_order(call, a, b)?,
    }))
}

/// `parse_bool`: the words of `bool` with no white space around them.
fn parse_bool(s: &str) -> Option<bool> {
    let space = |c: char| c.is_ascii_whitespace() || c == '\u{b}';
    if s.starts_with(space) || s.ends_with(space) {
        return None;
    }
    bool_in(s).ok()
}

/// `compareItems`: a comparison of two items. A null is equal only to a null, and items of other different types do not compare.
fn compare_items(call: &Call<'_>, use_tz: bool, op: PathOp, a: &Item, b: &Item) -> Result<Truth> {
    let order = match (a, b) {
        (Item::Null, Item::Null) => Ordering::Equal,
        (Item::Null, _) | (_, Item::Null) => return Ok(Truth::of(op == PathOp::NotEqual)),
        (Item::Bool(x), Item::Bool(y)) => x.cmp(y),
        (Item::Numeric(x), Item::Numeric(y)) => x.compare(y),
        (Item::String(x), Item::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Item::Datetime(x, _), Item::Datetime(y, _)) => {
            match compare_datetime(call, use_tz, x, y)? {
                Some(order) => order,
                None => return Ok(Truth::Unknown),
            }
        }
        _ => return Ok(Truth::Unknown),
    };
    Ok(Truth::of(match op {
        PathOp::Equal => order.is_eq(),
        PathOp::NotEqual => order.is_ne(),
        PathOp::Less => order.is_lt(),
        PathOp::Greater => order.is_gt(),
        PathOp::LessOrEqual => order.is_le(),
        _ => order.is_ge(),
    }))
}

/// `executeJsonPath`: the result and the items of the path on the value. With `want` false, the caller wants only to know if there is an item.
fn execute(
    call: &Call<'_>,
    args: &[Value],
    want: bool,
    use_tz: bool,
) -> Result<(Res, Vec<Item>, bool)> {
    let target = Doc::parse(text(args, 0)?.to_string())?;
    let Some(Value::JsonPath(path)) = args.get(1) else { return Err(bad_value()) };
    let (vars, silent) = match args {
        [_, _] => (None, true),
        [_, _, vars, Value::Bool(silent)] => {
            (Some(Doc::parse(vars.as_str().ok_or_else(bad_value)?.to_string())?), *silent)
        }
        _ => return Err(bad_value()),
    };
    if let Some(vars) = &vars
        && !matches!(vars.item(0), JsonItem::Object(_))
    {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "\"vars\" argument is not an object",
        )
        .with_detail(
            "Jsonpath parameters should be encoded as key-value pairs of \"vars\" object.",
        ));
    }
    let last_id = 1 + i64::from(vars.is_some());
    let vars = vars.map_or(Vars::None, Vars::Object);
    let (res, items) = run(call, &target, path, vars, last_id, !silent, want, use_tz)?;
    Ok((res, items, silent))
}

/// The run of a path on the root of a document, with the id that `.keyvalue()` gives after the ids of the variables.
#[allow(clippy::too_many_arguments)]
fn run(
    call: &Call<'_>,
    target: &Rc<Doc>,
    path: &JsonPath,
    vars: Vars<'_>,
    last_id: i64,
    throw: bool,
    want: bool,
    use_tz: bool,
) -> Result<(Res, Vec<Item>)> {
    let root = target.get(0)?;
    let mut exec = Exec {
        call,
        last_id,
        vars,
        lax: path.lax,
        ignore_structural: path.lax,
        throw,
        root: root.clone(),
        current: root.clone(),
        base: Base { node: None, id: 0 },
        innermost: None,
        depth: 0,
        use_tz,
    };
    let mut items = Vec::new();
    let res = if !want && !path.lax {
        // The strict mode needs all the items to check that there is no error.
        match exec.item(&path.expr, &root, Some(&mut items))? {
            Res::Error => Res::Error,
            _ if items.is_empty() => Res::NotFound,
            _ => Res::Ok,
        }
    } else {
        exec.item(&path.expr, &root, want.then_some(&mut items))?
    };
    Ok((res, items))
}

/// `executeJsonPath` for `JSON_EXISTS`, `JSON_QUERY` and `JSON_VALUE`: whether the path on the `jsonb` value finds an item, and the items when `want` is true. The result is `None` for an error of the path when `throw` is false. The casts of the datetime values can use the zone of the session.
///
/// # Errors
///
/// The error of the path when `throw` is true, and the errors that a silent path also gives, such as a variable that `PASSING` does not have.
pub fn sql_json_path(
    call: &Call<'_>,
    target: &Value,
    path: &Value,
    vars: &[PathVar],
    throw: bool,
    want: bool,
) -> Result<Option<(bool, Vec<SqlJsonItem>)>> {
    let Value::Text(target) = target else { return Err(bad_value()) };
    let Value::JsonPath(path) = path else { return Err(bad_value()) };
    let target = Doc::parse(target.clone())?;
    let last_id = 1 + i64::try_from(vars.len()).unwrap_or(i64::MAX);
    let (res, items) = run(call, &target, path, Vars::Passing(vars), last_id, throw, want, true)?;
    if res == Res::Error {
        return Ok(None);
    }
    let items = items
        .into_iter()
        .map(|item| match item {
            Item::Null => SqlJsonItem::Null,
            Item::Bool(b) => SqlJsonItem::Bool(b),
            Item::Numeric(n) => SqlJsonItem::Numeric(n),
            Item::String(s) => SqlJsonItem::String(s),
            Item::Datetime(value, text) => SqlJsonItem::Datetime(value, text),
            Item::Node(doc, at) => SqlJsonItem::Container(doc.text(at).to_owned()),
        })
        .collect();
    Ok(Some((res == Res::Ok, items)))
}

/// `jsonb_path_exists` and `@?`: null for an error in a silent call.
fn exists(call: &Call<'_>, args: &[Value], use_tz: bool) -> Result<Value> {
    Ok(match execute(call, args, false, use_tz)?.0 {
        Res::Error => Value::Null,
        res => Value::Bool(res == Res::Ok),
    })
}

/// `jsonb_path_match` and `@@`: the result of a path that gives one boolean.
fn path_match(call: &Call<'_>, args: &[Value], use_tz: bool) -> Result<Value> {
    let (_, items, silent) = execute(call, args, true, use_tz)?;
    match items.as_slice() {
        [Item::Bool(b)] => Ok(Value::Bool(*b)),
        [Item::Null] => Ok(Value::Null),
        _ if !silent => Err(Error::new(
            SqlState::SINGLETON_SQL_JSON_ITEM_REQUIRED,
            "single boolean result is expected",
        )),
        _ => Ok(Value::Null),
    }
}

/// `jsonb_path_query`: a row for each item.
pub(crate) fn jsonb_path_query(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    query(call, args, false)
}

/// `jsonb_path_query_tz`.
pub(crate) fn jsonb_path_query_tz(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    query(call, args, true)
}

fn query(call: &Call<'_>, args: &[Value], use_tz: bool) -> Result<Vec<Vec<Value>>> {
    let (_, items, _) = execute(call, args, true, use_tz)?;
    Ok(items.iter().map(|item| vec![item.to_value()]).collect())
}

/// `jsonb_path_query_array`: the items in an array.
fn query_array(call: &Call<'_>, args: &[Value], use_tz: bool) -> Result<Value> {
    let (_, items, _) = execute(call, args, true, use_tz)?;
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        item.write(&mut out);
    }
    out.push(']');
    Ok(Value::Text(out))
}

/// `jsonb_path_query_first`: the first item, or null.
fn query_first(call: &Call<'_>, args: &[Value], use_tz: bool) -> Result<Value> {
    let (_, items, _) = execute(call, args, true, use_tz)?;
    Ok(items.first().map_or(Value::Null, Item::to_value))
}

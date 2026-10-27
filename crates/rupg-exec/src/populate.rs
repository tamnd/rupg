//! `json_populate_record`, `json_populate_recordset`, `json_to_record`, `json_to_recordset` and the functions of `jsonb` with these names: the rows that the keys of JSON objects give.
//!
//! A key gives the value of the column with the same name. The value goes to the type of the column as `populate_record_field` makes it: a string or a scalar with the input function of the type, an array to an array type, and an object to a composite type. A column with no key takes its value from the base record, or is null.

use std::sync::Arc;

use rupg_analyze::{Column, Func};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{JsonItem, JsonTree, Record, Value, oid};

use crate::jsonexpr::{Js, Part, Soft, base_and_typmod};
use crate::{Eval, not_yet, strict};

/// A function of the family, as the worker of PostgreSQL gets it.
#[derive(Clone, Copy)]
pub(crate) struct Populate {
    /// The name in the messages.
    name: &'static str,
    jsonb: bool,
    /// True when the first argument is the base record.
    base: bool,
    set: bool,
    /// `jsonb_populate_record_valid`: true when the record has no soft error.
    valid: bool,
}

/// The function of the family with this OID.
pub(crate) fn populate_function(func: u32) -> Option<Populate> {
    let (name, jsonb, base, set) = match func {
        3960 => ("json_populate_record", false, true, false),
        3961 => ("json_populate_recordset", false, true, true),
        3204 => ("json_to_record", false, false, false),
        3205 => ("json_to_recordset", false, false, true),
        3209 | 6338 => ("jsonb_populate_record", true, true, false),
        3475 => ("jsonb_populate_recordset", true, true, true),
        3490 => ("jsonb_to_record", true, false, false),
        3491 => ("jsonb_to_recordset", true, false, true),
        _ => return None,
    };
    Some(Populate { name, jsonb, base, set, valid: func == 6338 })
}

/// The columns of a row type: the name, the type and the typmod of each one.
struct Row {
    columns: Vec<(String, u32, i32)>,
    /// The names of the fields of a `record` value, or `None` for a named type.
    names: Option<Arc<[String]>>,
}

impl Row {
    /// The row of a `record` with these columns.
    fn record(columns: Vec<(String, u32, i32)>) -> Row {
        let names = columns.iter().map(|c| c.0.clone()).collect();
        Row { columns, names: Some(names) }
    }
}

/// `lookup_rowtype_tupdesc`: the columns of a composite type that are not dropped, from a system catalog or from a relation of the catalog of the session.
fn composite_row(ty: u32, session: &dyn rupg_func::Session) -> Option<Row> {
    let relid = builtin::type_by_oid(ty).filter(|row| row.kind == b'c' && row.relid != 0)?.relid;
    let columns = if let Some(catalog) = rupg_pgcatalog::catalog_by_oid(relid) {
        catalog.columns.iter().map(|c| (c.name.to_owned(), c.type_oid, -1)).collect()
    } else {
        let rel = session.catalog()?.relation(relid)?;
        rel.columns
            .iter()
            .filter(|c| !c.dropped)
            .map(|c| (c.name.clone(), c.ty, c.typmod))
            .collect()
    };
    Some(Row { columns, names: None })
}

/// `JsValueToJsObject`: the object at the node `at`. A scalar or an array is a soft error.
fn object(part: Part<'_>, at: usize, name: &str) -> std::result::Result<usize, Error> {
    match part.tree.item(at) {
        JsonItem::Object(_) => Ok(at),
        JsonItem::Array(_) => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("cannot call {name} on an array"),
        )),
        _ => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("cannot call {name} on a scalar"),
        )),
    }
}

impl Eval<'_> {
    /// The values of a call of a function of the family. `defined` is the column definition list of a call in `FROM`, or empty. A call that does not give a set gives one value.
    pub(crate) fn populate_call(
        &mut self,
        f: &Func,
        p: Populate,
        defined: &[Column],
    ) -> Result<Vec<Value>> {
        let exprs = rupg_analyze::expand_arguments(f)?;
        let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        if strict(f.oid) && args.iter().any(Value::is_null) {
            return Ok(if p.set { Vec::new() } else { vec![Value::Null] });
        }
        let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
        self.populate(p, &types, &args, defined)
    }

    /// `populate_record_worker` and `populate_recordset_worker`.
    fn populate(
        &mut self,
        p: Populate,
        types: &[u32],
        args: &[Value],
        defined: &[Column],
    ) -> Result<Vec<Value>> {
        let argtype = if p.base { types.first().copied().unwrap_or(0) } else { oid::RECORD };
        let base = args.first().filter(|value| p.base && !value.is_null());
        // The domain of the first argument, whose checks the record passes.
        let mut domain = None;
        let row = if argtype != oid::RECORD {
            let (ty, _) = base_and_typmod(argtype, -1);
            if ty != argtype {
                domain = Some(argtype);
            }
            composite_row(ty, self.session).ok_or_else(|| {
                Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("first argument of {} must be a row type", p.name),
                )
            })?
        } else if let Some(Value::Record(record)) = base {
            let names = record.names.clone();
            let name = |i: usize| match &names {
                Some(names) => names.get(i).cloned().unwrap_or_default(),
                None => format!("f{}", i + 1),
            };
            Row::record(record.types.iter().enumerate().map(|(i, &ty)| (name(i), ty, -1)).collect())
        } else if !defined.is_empty() {
            Row::record(defined.iter().map(|c| (c.name.clone(), c.ty, c.typmod)).collect())
        } else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("could not determine row type for result of {}", p.name),
            )
            .with_hint(
                "Provide a non-null record argument, or call the function in the FROM clause using a column definition list.",
            ));
        };
        let base = match base {
            Some(Value::Record(record)) => Some(&**record),
            _ => None,
        };
        let json = match args.get(usize::from(p.base)) {
            None | Some(Value::Null) if p.set => return Ok(Vec::new()),
            None | Some(Value::Null) if p.valid => return Ok(vec![Value::Bool(true)]),
            None | Some(Value::Null) => {
                return Ok(vec![base.map_or(Value::Null, |r| Value::Record(Box::new(r.clone())))]);
            }
            Some(Value::Text(text)) => text,
            Some(_) => return Err(Error::internal("the argument of a JSON function is not text")),
        };
        let tree = JsonTree::parse(json).map_err(rupg_func::type_error)?;
        let part = Part { tree: &tree, json: !p.jsonb };
        if !p.set {
            let record = match object(part, 0, "populate_composite") {
                Ok(at) => self.populate_record(&row, base, part, at)?,
                Err(error) => Err(error),
            };
            let result = match record {
                Ok(value) => match domain {
                    Some(domain) => match self.domain_failure(domain, &value)? {
                        Some(error) => Err(error),
                        None => Ok(value),
                    },
                    None => Ok(value),
                },
                Err(error) => Err(error),
            };
            return Ok(vec![match result {
                _ if p.valid => Value::Bool(result.is_ok()),
                Ok(value) => value,
                Err(error) => return Err(error),
            }]);
        }
        let invalid = |what: &str| Error::new(SqlState::INVALID_PARAMETER_VALUE, what.to_owned());
        let items = match tree.item(0) {
            JsonItem::Array(items) => items,
            JsonItem::Object(_) if part.json => {
                return Err(invalid(&format!("cannot call {} on an object", p.name)));
            }
            _ if part.json => return Err(invalid(&format!("cannot call {} on a scalar", p.name))),
            _ => return Err(invalid(&format!("cannot call {} on a non-array", p.name))),
        };
        let mut values = Vec::with_capacity(items.len());
        for &at in items {
            if !matches!(tree.item(at), JsonItem::Object(_)) {
                return Err(invalid(&format!(
                    "argument of {} must be an array of objects",
                    p.name
                )));
            }
            let value = self.populate_record(&row, base, part, at)??;
            values.push(match domain {
                Some(domain) => self.domain_check(domain, value)?,
                None => value,
            });
        }
        Ok(values)
    }

    /// `populate_record`: the record of the object at the node `at`. With a base record, an empty object gives the base record, and a column with no key keeps its value in the base record.
    fn populate_record(
        &mut self,
        row: &Row,
        base: Option<&Record>,
        part: Part<'_>,
        at: usize,
    ) -> Soft<Value> {
        if let (Some(base), JsonItem::Object(fields)) = (base, part.tree.item(at))
            && fields.is_empty()
        {
            return Ok(Ok(Value::Record(Box::new(base.clone()))));
        }
        let mut values = Vec::with_capacity(row.columns.len());
        for (i, (name, ty, typmod)) in row.columns.iter().enumerate() {
            let default = base.and_then(|base| base.values.get(i));
            let js = match part.tree.field(at, name) {
                Some(field) => part.js(field),
                None if base.is_some() => {
                    values.push(default.cloned().unwrap_or(Value::Null));
                    continue;
                }
                None => Js::Null,
            };
            match self.populate_field_with(
                js,
                *ty,
                *typmod,
                default.filter(|v| !v.is_null()),
                Some(name),
            )? {
                Ok(value) => values.push(value),
                Err(error) => return Ok(Err(error)),
            }
        }
        let types = row.columns.iter().map(|c| c.1).collect();
        Ok(Ok(Value::Record(Box::new(Record { types, values, names: row.names.clone() }))))
    }

    /// `populate_composite` for a field or an element: the record of a JSON object. `default` is the value of the field in the base record.
    pub(crate) fn populate_composite(
        &mut self,
        js: Js<'_>,
        ty: u32,
        default: Option<&Value>,
    ) -> Soft<Value> {
        let (part, at) = match js {
            Js::Root(tree) => (Part { tree, json: false }, 0),
            Js::Element(tree, at) => (Part { tree, json: false }, at),
            Js::Json(tree, at) => (Part { tree, json: true }, at),
            Js::Null | Js::Unquoted(_) => {
                return Err(Error::internal("populate_composite of a value that is not JSON"));
            }
        };
        let Some(row) = composite_row(ty, self.session) else {
            return Err(not_yet("a JSON object for a value of type record"));
        };
        let at = match object(part, at, "populate_composite") {
            Ok(at) => at,
            Err(error) => return Ok(Err(error)),
        };
        let base = match default {
            Some(Value::Record(record)) => Some(&**record),
            _ => None,
        };
        self.populate_record(&row, base, part, at)
    }
}

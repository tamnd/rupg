//! The functions of `xml.c` that make and test `xml` values: `xmlcomment`, `xmltext`, `xml`, `text`, `xmlconcat2`, the tests of `xml_is_well_formed` and the SQL/XML functions with their own syntax, such as `XMLELEMENT`.

use rupg_analyze::{XmlExpr, XmlOp};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    DateFormat, DateOrder, DateStyle, Value, XmlStandalone, date_out, escape_xml, oid,
    timestamp_out, timestamptz_out, xml_base64, xml_binhex, xml_comment, xml_concat, xml_element,
    xml_in, xml_pi, xml_root, xml_text,
};

use crate::io::{array_of, base_type, output};
use crate::reg::text_arg;
use crate::{Call, Kernel, Session, bad_value, type_error};

/// The kernel of a function of `xml` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "xmlcomment" => xmlcomment,
        "xmltext" => xmltext,
        "texttoxml" => texttoxml,
        "xmltotext" => xmltotext,
        "xmlconcat2" => xmlconcat2,
        "xmlvalidate" => xmlvalidate,
        "xml_is_well_formed" => xml_is_well_formed,
        "xml_is_well_formed_document" => xml_is_well_formed_document,
        "xml_is_well_formed_content" => xml_is_well_formed_content,
        _ => return None,
    })
}

/// True when the `xmloption` of the session is `DOCUMENT`.
fn document_option(session: &dyn Session) -> bool {
    session.setting("xmloption").is_some_and(|v| v.eq_ignore_ascii_case("document"))
}

/// `xml_parse` with a hard error: the text checked as a document or as content. The warnings of libxml2 go to the client.
fn parse(text: &str, document: bool, session: &dyn Session) -> Result<()> {
    xml_in(text.as_bytes(), document, &mut |warning| {
        session.warn(Error::new(SqlState::WARNING, warning));
    })
    .map_err(type_error)
}

/// `wellformed_xml`: true when the text parses as a document or as content.
fn well_formed(text: &str, document: bool, session: &dyn Session) -> bool {
    parse(text, document, session).is_ok()
}

fn xmlcomment(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(xml_comment(text_arg(args)?).map_err(type_error)?))
}

fn xmltext(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(xml_text(text_arg(args)?)))
}

/// `texttoxml`: `xml(text)`, which parses the text by the `xmloption` of the session.
fn texttoxml(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = text_arg(args)?;
    parse(text, document_option(call.session), call.session)?;
    Ok(Value::text(text))
}

/// `xmltotext`: `text(xml)`, the text as it is stored.
fn xmltotext(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(text_arg(args)?))
}

/// `xmlconcat2`, the transition function of `xmlagg`. It is not strict: a null argument does not count.
fn xmlconcat2(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    match args {
        [Value::Null, Value::Null] => Ok(Value::Null),
        [Value::Null, v] | [v, Value::Null] => Ok(v.clone()),
        [a, b] => {
            let (a, b) = (a.as_str().ok_or_else(bad_value)?, b.as_str().ok_or_else(bad_value)?);
            Ok(Value::text(xml_concat(&[a, b])))
        }
        _ => Err(bad_value()),
    }
}

fn xmlvalidate(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "xmlvalidate is not implemented"))
}

fn xml_is_well_formed(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let document = document_option(call.session);
    Ok(Value::Bool(well_formed(text_arg(args)?, document, call.session)))
}

fn xml_is_well_formed_document(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(well_formed(text_arg(args)?, true, call.session)))
}

fn xml_is_well_formed_content(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(well_formed(text_arg(args)?, false, call.session)))
}

/// The error of an infinite date or timestamp, which XML does not have.
fn infinite(what: &str) -> Error {
    Error::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("{what} out of range"))
        .with_detail(format!("XML does not support infinite {what} values."))
}

/// `map_sql_value_to_xml_value`: the text of a value that is not null in XML. An array gives an `element` for each element that is not null. A `boolean`, a date, a timestamp and a `bytea` have the forms of XML Schema. The other types give their output, which has the special characters escaped when `escape` is true, except for `xml`.
///
/// # Errors
///
/// `22008` for an infinite date or timestamp, and the errors of the output of the type.
pub fn xml_value(ty: u32, value: &Value, escape: bool, session: &dyn Session) -> Result<String> {
    let ty = base_type(ty);
    if let Value::Array(array) = value {
        let element = array_of(ty)
            .map(|(e, _)| e)
            .or_else(|| builtin::type_by_oid(ty).map(|row| row.elem))
            .unwrap_or(oid::UNKNOWN);
        let mut out = String::new();
        for v in array.values.iter().flatten() {
            out.push_str("<element>");
            out.push_str(&xml_value(element, v, true, session)?);
            out.push_str("</element>");
        }
        return Ok(out);
    }
    let format = DateFormat { style: DateStyle::Xsd, order: DateOrder::Mdy };
    let mut buf = Vec::new();
    match (ty, value) {
        (oid::BOOL, Value::Bool(b)) => return Ok(if *b { "true" } else { "false" }.to_string()),
        (oid::DATE, Value::Date(v)) => {
            date_out(*v, format, &mut buf);
            if buf.ends_with(b"infinity") {
                return Err(infinite("date"));
            }
        }
        (oid::TIMESTAMP, Value::Timestamp(v)) => {
            timestamp_out(*v, format, &mut buf).map_err(type_error)?;
            if buf.ends_with(b"infinity") {
                return Err(infinite("timestamp"));
            }
        }
        (oid::TIMESTAMPTZ, Value::TimestampTz(v)) => {
            timestamptz_out(*v, format, &*session.zone()?, &mut buf).map_err(type_error)?;
            if buf.ends_with(b"infinity") {
                return Err(infinite("timestamp"));
            }
        }
        (oid::BYTEA, Value::Bytea(bytes)) => {
            let hex = session.setting("xmlbinary").is_some_and(|v| v.eq_ignore_ascii_case("hex"));
            return Ok(if hex { xml_binhex(bytes) } else { xml_base64(bytes) });
        }
        _ => {
            output(ty, value, session, &mut buf)?;
            let text = String::from_utf8(buf).map_err(|_| bad_value())?;
            return Ok(if ty == oid::XML || !escape { text } else { escape_xml(&text) });
        }
    }
    String::from_utf8(buf).map_err(|_| bad_value())
}

/// The text of an argument of type `text` or `xml`.
fn text_of(value: &Value) -> Result<&str> {
    value.as_str().ok_or_else(bad_value)
}

/// `ExecEvalXmlExpr`: the value of a SQL/XML function from the values of its named arguments and of its other arguments, in the order of [`XmlExpr::children`].
///
/// # Errors
///
/// The errors of the function, such as `2200M` for `XMLPARSE` of a value that is not a document.
pub fn xml_expr(x: &XmlExpr, values: &[Value], session: &dyn Session) -> Result<Value> {
    let (named, args) = values.split_at(x.named.len().min(values.len()));
    let arg = |i: usize| args.get(i).ok_or_else(bad_value);
    let name = x.name.as_deref().unwrap_or_default();
    Ok(match x.op {
        XmlOp::Concat => {
            let texts: Vec<&str> =
                args.iter().filter(|v| !v.is_null()).map(text_of).collect::<Result<_>>()?;
            if texts.is_empty() { Value::Null } else { Value::text(xml_concat(&texts)) }
        }
        XmlOp::Element => {
            let mut attributes = Vec::new();
            for ((attr, e), v) in x.named.iter().zip(named) {
                if !v.is_null() {
                    attributes.push((attr.as_str(), xml_value(e.ty, v, false, session)?));
                }
            }
            let mut content = Vec::new();
            for (e, v) in x.args.iter().zip(args) {
                if !v.is_null() {
                    content.push(xml_value(e.ty, v, true, session)?);
                }
            }
            let attributes: Vec<(&str, &str)> =
                attributes.iter().map(|(n, v)| (*n, v.as_str())).collect();
            Value::text(xml_element(name, &attributes, &content))
        }
        XmlOp::Forest => {
            let mut out = String::new();
            let mut any = false;
            for ((tag, e), v) in x.named.iter().zip(named) {
                if !v.is_null() {
                    any = true;
                    out.push_str(&format!("<{tag}>{}</{tag}>", xml_value(e.ty, v, true, session)?));
                }
            }
            if any { Value::text(out) } else { Value::Null }
        }
        XmlOp::Parse => match arg(0)? {
            Value::Null => Value::Null,
            v => {
                let text = text_of(v)?;
                parse(text, x.document, session)?;
                Value::text(text)
            }
        },
        XmlOp::Pi => {
            let (text, null) = match args.first() {
                Some(Value::Null) => (None, true),
                Some(v) => (Some(text_of(v)?), false),
                None => (None, false),
            };
            xml_pi(name, text, null).map_err(type_error)?.map_or(Value::Null, Value::text)
        }
        XmlOp::Root => match arg(0)? {
            Value::Null => Value::Null,
            data => {
                let version = match arg(1)? {
                    Value::Null => None,
                    v => Some(text_of(v)?),
                };
                let standalone = match arg(2)? {
                    Value::Int4(0) => XmlStandalone::Yes,
                    Value::Int4(1) => XmlStandalone::No,
                    Value::Int4(2) => XmlStandalone::NoValue,
                    _ => XmlStandalone::Omitted,
                };
                Value::text(xml_root(text_of(data)?, version, standalone))
            }
        },
        XmlOp::Serialize => match arg(0)? {
            Value::Null => Value::Null,
            v => {
                let text = text_of(v)?;
                if x.indent {
                    return Err(crate::not_yet("XMLSERIALIZE with INDENT"));
                }
                if x.document && xml_in(text.as_bytes(), true, &mut |_| {}).is_err() {
                    return Err(Error::new(SqlState::NOT_AN_XML_DOCUMENT, "not an XML document"));
                }
                Value::text(text)
            }
        },
        XmlOp::Document => match arg(0)? {
            Value::Null => Value::Null,
            v => Value::Bool(well_formed(text_of(v)?, true, session)),
        },
    })
}

//! The actions of SQL/JSON that the translator cannot write: `JSON_TABLE`, the value expressions and the `FORMAT` clause.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/json.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

impl rules::json_table for Parser<'_> {
    fn json_table_1(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: Option<Str>,
        v7: List,
        v10: List,
        v12: Option<Node>,
        at1: i32,
        at5: i32,
        at6: i32,
    ) -> Result<Option<Node>, Error> {
        let pathstring = match v5.as_ref().and_then(A_Const::peek) {
            Some(A_Const { val: Some(Node::String(s)), .. }) => s.clone(),
            _ => {
                let message =
                    "only string constants are supported in JSON_TABLE path specification";
                return Err(self.error(ERRCODE_FEATURE_NOT_SUPPORTED, message, at5));
            }
        };
        let n = JsonTable {
            context_item: castNode(v3)?,
            pathspec: Some(Box::new(makeJsonTablePathSpec(Some(pathstring), v6, at5, at6))),
            passing: v7,
            columns: v10,
            on_error: castNode(v12)?,
            location: at1,
            ..JsonTable::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::json_value_expr for Parser<'_> {
    fn json_value_expr_1(
        &mut self,
        v1: Option<Node>,
        v2: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        // `formatted_expr` is set in the parse analysis.
        Ok(Some(makeJsonValueExpr(v1, None, castNode(v2)?).into()))
    }
}

impl rules::json_format_clause for Parser<'_> {
    fn json_format_clause_1(
        &mut self,
        v4: Option<Str>,
        at1: i32,
        at4: i32,
    ) -> Result<Option<Node>, Error> {
        // `pg_strcasecmp`, which folds the case of the ASCII letters only.
        let name = v4.as_deref().unwrap_or_default();
        let encoding = if name.eq_ignore_ascii_case("utf8") {
            JsonEncoding::JS_ENC_UTF8
        } else if name.eq_ignore_ascii_case("utf16") {
            JsonEncoding::JS_ENC_UTF16
        } else if name.eq_ignore_ascii_case("utf32") {
            JsonEncoding::JS_ENC_UTF32
        } else {
            let message = format!("unrecognized JSON encoding: {name}");
            return Err(self.error(ERRCODE_INVALID_PARAMETER_VALUE, &message, at4));
        };
        Ok(Some(makeJsonFormat(JsonFormatType::JS_FORMAT_JSON, encoding, at1).into()))
    }
}

//! The functions of `makefuncs.c` and `value.c` that the actions of `gram.y` use, with the names and the arguments of C. A function that gives a pointer to a node type in C gives the node by value here, and the caller makes it a [`Node`] or a box as the C casts it.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/make.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::castNode;
use crate::error::Error;
use crate::nodes::*;

/// `makeInteger`.
pub(crate) fn makeInteger(i: i32) -> Node {
    Node::Integer(i)
}

/// `makeFloat`. The number stays a string, as in C.
pub(crate) fn makeFloat(numericStr: Option<Str>) -> Node {
    Node::Float(numericStr.unwrap_or_default())
}

/// `makeBoolean`.
pub(crate) fn makeBoolean(val: bool) -> Node {
    Node::Boolean(val)
}

/// `makeString`.
pub(crate) fn makeString(str: Option<Str>) -> Node {
    Node::String(str.unwrap_or_default())
}

/// `makeBitString`.
pub(crate) fn makeBitString(str: Option<Str>) -> Node {
    Node::BitString(str.unwrap_or_default())
}

/// `makeA_Expr`.
pub(crate) fn makeA_Expr(
    kind: A_Expr_Kind,
    name: List,
    lexpr: Option<Node>,
    rexpr: Option<Node>,
    location: i32,
) -> A_Expr {
    A_Expr { kind, name, lexpr, rexpr, location, ..A_Expr::default() }
}

/// `makeSimpleA_Expr`: an `A_Expr` with an operator name that has no schema.
pub(crate) fn makeSimpleA_Expr(
    kind: A_Expr_Kind,
    name: &str,
    lexpr: Option<Node>,
    rexpr: Option<Node>,
    location: i32,
) -> A_Expr {
    let name = vec![Some(makeString(Some(name.into())))];
    A_Expr { kind, name, lexpr, rexpr, location, ..A_Expr::default() }
}

/// `makeBoolExpr`.
pub(crate) fn makeBoolExpr(boolop: BoolExprType, args: List, location: i32) -> BoolExpr {
    BoolExpr { boolop, args, location }
}

/// `makeAlias`.
pub(crate) fn makeAlias(aliasname: Option<Str>, colnames: List) -> Alias {
    Alias { aliasname, colnames }
}

/// `makeRangeVar`.
pub(crate) fn makeRangeVar(
    schemaname: Option<Str>,
    relname: Option<Str>,
    location: i32,
) -> RangeVar {
    RangeVar {
        catalogname: None,
        schemaname,
        relname,
        inh: true,
        relpersistence: RELPERSISTENCE_PERMANENT,
        alias: None,
        location,
    }
}

/// `makeTypeName`: the type name of an unqualified name.
pub(crate) fn makeTypeName(typnam: &str) -> TypeName {
    makeTypeNameFromNameList(vec![Some(makeString(Some(typnam.into())))])
}

/// `makeTypeNameFromNameList`.
pub(crate) fn makeTypeNameFromNameList(names: List) -> TypeName {
    TypeName { names, typmods: List::new(), typemod: -1, location: -1, ..TypeName::default() }
}

/// `makeStringConst`.
pub(crate) fn makeStringConst(str: Option<Str>, location: i32) -> Node {
    A_Const { val: Some(makeString(str)), isnull: false, location }.into()
}

/// `makeFuncCall`.
pub(crate) fn makeFuncCall(
    name: List,
    args: List,
    funcformat: CoercionForm,
    location: i32,
) -> FuncCall {
    FuncCall { funcname: name, args, funcformat, location, ..FuncCall::default() }
}

/// `makeGroupingSet`.
pub(crate) fn makeGroupingSet(kind: GroupingSetKind, content: List, location: i32) -> GroupingSet {
    GroupingSet { kind, content, location }
}

/// `makeJsonFormat`.
pub(crate) fn makeJsonFormat(
    format_type: JsonFormatType,
    encoding: JsonEncoding,
    location: i32,
) -> JsonFormat {
    JsonFormat { format_type, encoding, location }
}

/// `makeJsonValueExpr`.
pub(crate) fn makeJsonValueExpr(
    raw_expr: Option<Node>,
    formatted_expr: Option<Node>,
    format: Option<Box<JsonFormat>>,
) -> JsonValueExpr {
    JsonValueExpr { raw_expr, formatted_expr, format }
}

/// `makeJsonBehavior`.
pub(crate) fn makeJsonBehavior(
    btype: JsonBehaviorType,
    expr: Option<Node>,
    location: i32,
) -> JsonBehavior {
    JsonBehavior { btype, expr, location, ..JsonBehavior::default() }
}

/// `makeJsonKeyValue`.
pub(crate) fn makeJsonKeyValue(key: Option<Node>, value: Option<Node>) -> Result<Node, Error> {
    Ok(JsonKeyValue { key, value: castNode(value)? }.into())
}

/// `makeJsonIsPredicate`.
pub(crate) fn makeJsonIsPredicate(
    expr: Option<Node>,
    format: JsonFormat,
    item_type: JsonValueType,
    unique_keys: bool,
    exprBaseType: u32,
    location: i32,
) -> Node {
    let format = Some(Box::new(format));
    JsonIsPredicate { expr, format, item_type, unique_keys, exprBaseType, location }.into()
}

/// `makeJsonTablePathSpec`.
pub(crate) fn makeJsonTablePathSpec(
    string: Option<Str>,
    name: Option<Str>,
    string_location: i32,
    name_location: i32,
) -> JsonTablePathSpec {
    JsonTablePathSpec {
        string: Some(makeStringConst(string, string_location)),
        name,
        name_location,
        location: string_location,
    }
}

/// `makeDefElem`.
pub(crate) fn makeDefElem(name: Option<Str>, arg: Option<Node>, location: i32) -> DefElem {
    DefElem {
        defnamespace: None,
        defname: name,
        arg,
        defaction: DefElemAction::DEFELEM_UNSPEC,
        location,
    }
}

/// `makeDefElemExtended`.
pub(crate) fn makeDefElemExtended(
    nameSpace: Option<Str>,
    name: Option<Str>,
    arg: Option<Node>,
    defaction: DefElemAction,
    location: i32,
) -> DefElem {
    DefElem { defnamespace: nameSpace, defname: name, arg, defaction, location }
}

/// `makeVacuumRelation`.
pub(crate) fn makeVacuumRelation(
    relation: Option<Box<RangeVar>>,
    oid: u32,
    va_cols: List,
) -> VacuumRelation {
    VacuumRelation { relation, oid, va_cols }
}

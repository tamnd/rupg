//! The static functions of the prologue and the epilogue of `gram.y`, with the names and the arguments of C. A function that takes `yyscanner` in C takes the [`Parser`] here, for its errors.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/gram.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use super::Parser;
use super::funcs::listLocation;
use super::list::*;
use super::make::*;
use crate::error::Error;
use crate::generated::glue::{
    CAS_DEFERRABLE, CAS_ENFORCED, CAS_INITIALLY_DEFERRED, CAS_NO_INHERIT, CAS_NOT_ENFORCED,
    CAS_NOT_VALID, SelectLimit,
};
use crate::nodes::*;

impl Parser<'_> {
    /// `ereport(ERROR, errcode(code), errmsg(message), parser_errposition(location))`.
    pub(crate) fn error(&self, code: SqlState, message: &str, location: i32) -> Error {
        Error {
            code,
            message: message.to_owned(),
            detail: None,
            hint: None,
            location: usize::try_from(location).ok(),
        }
    }
}

/// `strVal`: the text of a `String` node.
pub(crate) fn strVal(node: Option<&Node>) -> Option<Str> {
    match node {
        Some(Node::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `intVal`: the value of an `Integer` node.
pub(crate) fn intVal(node: Option<&Node>) -> Result<i32, Error> {
    match node {
        Some(Node::Integer(i)) => Ok(*i),
        _ => Err(Error::internal("intVal of a node that is not an Integer")),
    }
}

/// `boolVal`: the value of a `Boolean` node.
pub(crate) fn boolVal(node: Option<&Node>) -> Result<bool, Error> {
    match node {
        Some(Node::Boolean(b)) => Ok(*b),
        _ => Err(Error::internal("boolVal of a node that is not a Boolean")),
    }
}

/// `defGetInt32` of `define.c`: the value of an option that must be an integer. The error has no location, because the function has no parser.
pub(crate) fn defGetInt32(def: &DefElem) -> Result<i32, Error> {
    match &def.arg {
        Some(Node::Integer(i)) => Ok(*i),
        _ => {
            let name = def.defname.as_deref().unwrap_or_default();
            let message = format!("{name} requires an integer value");
            Err(Error { code: ERRCODE_SYNTAX_ERROR, ..Error::internal(&message) })
        }
    }
}

/// `NameListToString` of `namespace.c`: the names joined with dots, and `*` for `A_Star`.
pub(crate) fn NameListToString(names: &List) -> String {
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        match name {
            Some(Node::String(s)) => out.push_str(s),
            Some(Node::A_Star(_)) => out.push('*'),
            _ => {}
        }
    }
    out
}

/// `makeRawStmt`.
pub(crate) fn makeRawStmt(stmt: Option<Node>, stmt_location: i32) -> RawStmt {
    // `stmt_len` can change later.
    RawStmt { stmt, stmt_location, stmt_len: 0 }
}

/// `updateRawStmtEnd`: the statement does not run to the end of the text.
pub(crate) fn updateRawStmtEnd(rs: &mut RawStmt, end_location: i32) {
    // A length that is set stays, for a text such as `select foo ;; select bar`, where the same statement is the last one for more than one semicolon.
    if rs.stmt_len > 0 {
        return;
    }
    rs.stmt_len = end_location - rs.stmt_location;
}

/// `makeColumnRef`: a `ColumnRef`, in an `A_Indirection` when the indirection has a subscript. A field selection at the start of the indirection goes into the fields of the `ColumnRef`.
pub(crate) fn makeColumnRef(
    colname: Option<Str>,
    mut indirection: List,
    location: i32,
    yyscanner: &Parser<'_>,
) -> Result<Node, Error> {
    let mut c = ColumnRef { fields: List::new(), location };
    let length = indirection.len();
    // The loop stops at the first subscript, so `nfields` is the number of field selections before it.
    for (nfields, item) in indirection.iter().enumerate() {
        if matches!(item, Some(Node::A_Indices(_))) {
            let mut i = A_Indirection::default();
            if nfields == 0 {
                // All the indirection goes to the `A_Indirection`.
                c.fields = vec![Some(makeString(colname))];
                i.indirection = check_indirection(indirection, yyscanner)?;
            } else {
                // Split the list in two.
                let tail = indirection.split_off(nfields);
                i.indirection = check_indirection(tail, yyscanner)?;
                c.fields = indirection;
                c.fields.insert(0, Some(makeString(colname)));
            }
            i.arg = Some(c.into());
            return Ok(i.into());
        } else if matches!(item, Some(Node::A_Star(_))) {
            // `*` is only at the end of a `ColumnRef`.
            if nfields + 1 < length {
                return Err(yyscanner.yyerror("improper use of \"*\""));
            }
        }
    }
    // No subscript, so all the indirection goes to the fields.
    c.fields = indirection;
    c.fields.insert(0, Some(makeString(colname)));
    Ok(c.into())
}

/// `makeTypeCast`.
pub(crate) fn makeTypeCast(
    arg: Option<Node>,
    typename: Option<Box<TypeName>>,
    location: i32,
) -> Node {
    TypeCast { arg, typeName: typename, location }.into()
}

/// `makeStringConstCast`.
pub(crate) fn makeStringConstCast(
    str: Option<Str>,
    location: i32,
    typename: Option<Box<TypeName>>,
) -> Node {
    let s = makeStringConst(str, location);
    makeTypeCast(Some(s), typename, -1)
}

/// `makeIntConst`.
pub(crate) fn makeIntConst(val: i32, location: i32) -> Node {
    A_Const { val: Some(makeInteger(val)), isnull: false, location }.into()
}

/// `makeFloatConst`.
pub(crate) fn makeFloatConst(str: Option<Str>, location: i32) -> Node {
    A_Const { val: Some(makeFloat(str)), isnull: false, location }.into()
}

/// `makeBoolAConst`.
pub(crate) fn makeBoolAConst(state: bool, location: i32) -> Node {
    A_Const { val: Some(makeBoolean(state)), isnull: false, location }.into()
}

/// `makeBitStringConst`.
pub(crate) fn makeBitStringConst(str: Option<Str>, location: i32) -> Node {
    A_Const { val: Some(makeBitString(str)), isnull: false, location }.into()
}

/// `makeNullAConst`.
pub(crate) fn makeNullAConst(location: i32) -> Node {
    A_Const { val: None, isnull: true, location }.into()
}

/// `makeRoleSpec`.
pub(crate) fn makeRoleSpec(r#type: RoleSpecType, location: i32) -> RoleSpec {
    RoleSpec { roletype: r#type, location, ..RoleSpec::default() }
}

/// `check_qualified_name`: the `qualified_name` rule lets subscripts and `*` through, and this rejects them.
pub(crate) fn check_qualified_name(names: &List, yyscanner: &Parser<'_>) -> Result<(), Error> {
    if names.iter().any(|name| !matches!(name, Some(Node::String(_)))) {
        return Err(yyscanner.yyerror("syntax error"));
    }
    Ok(())
}

/// `check_func_name`: the `func_name` rule lets subscripts and `*` through, and this rejects them.
pub(crate) fn check_func_name(names: List, yyscanner: &Parser<'_>) -> Result<List, Error> {
    check_qualified_name(&names, yyscanner)?;
    Ok(names)
}

/// `check_indirection`: `*` is only at the end of the list.
pub(crate) fn check_indirection(indirection: List, yyscanner: &Parser<'_>) -> Result<List, Error> {
    let length = indirection.len();
    for (l, item) in indirection.iter().enumerate() {
        if matches!(item, Some(Node::A_Star(_))) && l + 1 < length {
            return Err(yyscanner.yyerror("improper use of \"*\""));
        }
    }
    Ok(indirection)
}

/// `insertSelectOptions`: puts `ORDER BY` and the other options into a `SelectStmt`.
pub(crate) fn insertSelectOptions(
    stmt: &mut SelectStmt,
    sortClause: List,
    lockingClause: List,
    limitClause: Option<Box<SelectLimit>>,
    withClause: Option<Box<WithClause>>,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    // The tests reject a statement such as `(SELECT foo ORDER BY bar) ORDER BY baz`.
    if !sortClause.is_empty() {
        if !stmt.sortClause.is_empty() {
            let location = listLocation(&sortClause);
            return Err(yyscanner.error(
                ERRCODE_SYNTAX_ERROR,
                "multiple ORDER BY clauses not allowed",
                location,
            ));
        }
        stmt.sortClause = sortClause;
    }
    // More than one locking clause is correct.
    stmt.lockingClause.extend(lockingClause);
    if let Some(mut limitClause) = limitClause {
        if let Some(limitOffset) = limitClause.limitOffset.take() {
            if stmt.limitOffset.is_some() {
                let location = limitClause.offsetLoc;
                return Err(yyscanner.error(
                    ERRCODE_SYNTAX_ERROR,
                    "multiple OFFSET clauses not allowed",
                    location,
                ));
            }
            stmt.limitOffset = Some(limitOffset);
        }
        if let Some(limitCount) = limitClause.limitCount.take() {
            if stmt.limitCount.is_some() {
                let location = limitClause.countLoc;
                return Err(yyscanner.error(
                    ERRCODE_SYNTAX_ERROR,
                    "multiple LIMIT clauses not allowed",
                    location,
                ));
            }
            stmt.limitCount = Some(limitCount);
        }
        let with_ties = limitClause.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES;
        if stmt.sortClause.is_empty() && with_ties {
            let message = "WITH TIES cannot be specified without ORDER BY clause";
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, message, limitClause.optionLoc));
        }
        if with_ties {
            for lock in &stmt.lockingClause {
                if let Some(Node::LockingClause(lock)) = lock
                    && lock.waitPolicy == LockWaitPolicy::LockWaitSkip
                {
                    let message = "SKIP LOCKED and WITH TIES options cannot be used together";
                    return Err(yyscanner.error(
                        ERRCODE_SYNTAX_ERROR,
                        message,
                        limitClause.optionLoc,
                    ));
                }
            }
        }
        stmt.limitOption = limitClause.limitOption;
    }
    if let Some(withClause) = withClause {
        if stmt.withClause.is_some() {
            let location = withClause.location;
            return Err(yyscanner.error(
                ERRCODE_SYNTAX_ERROR,
                "multiple WITH clauses not allowed",
                location,
            ));
        }
        stmt.withClause = Some(withClause);
    }
    Ok(())
}

/// `makeSetOp`.
pub(crate) fn makeSetOp(
    op: SetOperation,
    all: bool,
    larg: Option<Node>,
    rarg: Option<Node>,
) -> Node {
    let select = |node: Option<Node>| node.and_then(|node| SelectStmt::from_node(node).ok());
    SelectStmt { op, all, larg: select(larg), rarg: select(rarg), ..SelectStmt::default() }.into()
}

/// `SystemFuncName`: the name of a built-in function, in `pg_catalog`.
pub(crate) fn SystemFuncName(name: &str) -> List {
    vec![Some(makeString(Some("pg_catalog".into()))), Some(makeString(Some(name.into())))]
}

/// `SystemTypeName`: the name of a built-in type, in `pg_catalog`.
pub(crate) fn SystemTypeName(name: &str) -> TypeName {
    makeTypeNameFromNameList(SystemFuncName(name))
}

/// `doNegate`: the negation of a number constant is a constant, so that `-123.456` stays a string until the type is known. The location of the constant becomes that of the `-`.
pub(crate) fn doNegate(n: Option<Node>, location: i32) -> Node {
    match n {
        Some(Node::A_Const(mut con))
            if matches!(con.val, Some(Node::Integer(_) | Node::Float(_))) =>
        {
            con.location = location;
            match &mut con.val {
                Some(Node::Integer(ival)) => *ival = ival.wrapping_neg(),
                Some(Node::Float(fval)) => doNegateFloat(fval),
                _ => {}
            }
            Node::A_Const(con)
        }
        Some(Node::A_Const(mut con)) => {
            con.location = location;
            makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, "-", None, Some(Node::A_Const(con)), location)
                .into()
        }
        n => makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, "-", None, n, location).into(),
    }
}

/// `doNegateFloat`.
pub(crate) fn doNegateFloat(v: &mut Str) {
    let oldval = v.strip_prefix('+').unwrap_or(v);
    *v = match oldval.strip_prefix('-') {
        // Remove the `-`.
        Some(rest) => rest.into(),
        None => format!("-{oldval}").into(),
    };
}

/// `makeAndExpr`: `a AND b AND c` is one `BoolExpr`.
pub(crate) fn makeAndExpr(lexpr: Option<Node>, rexpr: Option<Node>, location: i32) -> Node {
    match lexpr {
        Some(Node::BoolExpr(mut blexpr)) if blexpr.boolop == BoolExprType::AND_EXPR => {
            blexpr.args.push(rexpr);
            Node::BoolExpr(blexpr)
        }
        lexpr => makeBoolExpr(BoolExprType::AND_EXPR, vec![lexpr, rexpr], location).into(),
    }
}

/// `makeOrExpr`: `a OR b OR c` is one `BoolExpr`.
pub(crate) fn makeOrExpr(lexpr: Option<Node>, rexpr: Option<Node>, location: i32) -> Node {
    match lexpr {
        Some(Node::BoolExpr(mut blexpr)) if blexpr.boolop == BoolExprType::OR_EXPR => {
            blexpr.args.push(rexpr);
            Node::BoolExpr(blexpr)
        }
        lexpr => makeBoolExpr(BoolExprType::OR_EXPR, vec![lexpr, rexpr], location).into(),
    }
}

/// `makeNotExpr`.
pub(crate) fn makeNotExpr(expr: Option<Node>, location: i32) -> Node {
    makeBoolExpr(BoolExprType::NOT_EXPR, vec![expr], location).into()
}

/// `makeAArrayExpr`.
pub(crate) fn makeAArrayExpr(elements: List, location: i32, location_end: i32) -> Node {
    A_ArrayExpr { elements, location, list_start: location, list_end: location_end }.into()
}

/// `makeSQLValueFunction`. The type comes later, in the analysis.
pub(crate) fn makeSQLValueFunction(op: SQLValueFunctionOp, typmod: i32, location: i32) -> Node {
    SQLValueFunction { op, typmod, location, ..SQLValueFunction::default() }.into()
}

/// `makeXmlExpr`. The analysis splits the `ResTarget` list `named_args` into the names and the expressions, and sets the type. The caller sets `xmloption` where it applies.
pub(crate) fn makeXmlExpr(
    op: XmlExprOp,
    name: Option<Str>,
    named_args: List,
    args: List,
    location: i32,
) -> XmlExpr {
    XmlExpr { op, name, named_args, args, location, ..XmlExpr::default() }
}

/// `makeRangeVarFromQualifiedName`: a `RangeVar` from the name and the names after it of the `relation_name` rule.
pub(crate) fn makeRangeVarFromQualifiedName(
    name: Option<Str>,
    namelist: List,
    location: i32,
    yyscanner: &Parser<'_>,
) -> Result<RangeVar, Error> {
    check_qualified_name(&namelist, yyscanner)?;
    let mut r = makeRangeVar(None, None, location);
    match namelist.as_slice() {
        [relname] => {
            r.schemaname = name;
            r.relname = strVal(relname.as_ref());
        }
        [schemaname, relname] => {
            r.catalogname = name;
            r.schemaname = strVal(schemaname.as_ref());
            r.relname = strVal(relname.as_ref());
        }
        _ => {
            let mut names = namelist;
            names.insert(0, Some(makeString(name)));
            let message = format!(
                "improper qualified name (too many dotted names): {}",
                NameListToString(&names)
            );
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, &message, location));
        }
    }
    Ok(r)
}

/// `makeAConst`: a constant of an `Integer` or a `Float`.
pub(crate) fn makeAConst(v: Option<Node>, location: i32) -> Option<Node> {
    match v {
        Some(Node::Float(f)) => Some(makeFloatConst(Some(f), location)),
        Some(Node::Integer(i)) => Some(makeIntConst(i, location)),
        // Not used.
        _ => None,
    }
}

/// `makeRangeVarFromAnyName`: a `RangeVar` from a list of one to three names, with the location.
pub(crate) fn makeRangeVarFromAnyName(
    names: List,
    position: i32,
    yyscanner: &Parser<'_>,
) -> Result<RangeVar, Error> {
    let mut r = RangeVar::default();
    match names.as_slice() {
        [relname] => r.relname = strVal(relname.as_ref()),
        [schemaname, relname] => {
            r.schemaname = strVal(schemaname.as_ref());
            r.relname = strVal(relname.as_ref());
        }
        [catalogname, schemaname, relname] => {
            r.catalogname = strVal(catalogname.as_ref());
            r.schemaname = strVal(schemaname.as_ref());
            r.relname = strVal(relname.as_ref());
        }
        _ => {
            let message = format!(
                "improper qualified name (too many dotted names): {}",
                NameListToString(&names)
            );
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, &message, position));
        }
    }
    r.relpersistence = RELPERSISTENCE_PERMANENT;
    r.location = position;
    Ok(r)
}

/// `SplitColQualList`: moves the `COLLATE` clause out of the list of the constraints of a column. There can be one `COLLATE` clause at most.
pub(crate) fn SplitColQualList(
    qualList: List,
    constraintList: &mut List,
    collClause: &mut Option<Box<CollateClause>>,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    *collClause = None;
    let mut constraints = List::new();
    for n in qualList {
        match n {
            // Keep it in the list.
            Some(n @ Node::Constraint(_)) => constraints.push(Some(n)),
            Some(Node::CollateClause(c)) => {
                if collClause.is_some() {
                    let message = "multiple COLLATE clauses not allowed";
                    return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, message, c.location));
                }
                *collClause = Some(c);
            }
            _ => return Err(Error::internal("unexpected node type")),
        }
    }
    *constraintList = constraints;
    Ok(())
}

/// Sets the flag that a pointer of `processCASbits` points to. A `NULL` pointer gives `false`.
fn set(flag: &mut Option<&mut bool>, value: bool) -> bool {
    match flag {
        Some(flag) => {
            **flag = value;
            true
        }
        None => false,
    }
}

/// `processCASbits`: sets the flags of a constraint from the bits of `ConstraintAttributeSpec`. A `NULL` flag is one that the constraint type does not have, and a bit for it is an error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn processCASbits(
    cas_bits: i32,
    location: i32,
    constrType: &str,
    mut deferrable: Option<&mut bool>,
    mut initdeferred: Option<&mut bool>,
    mut is_enforced: Option<&mut bool>,
    mut not_valid: Option<&mut bool>,
    mut no_inherit: Option<&mut bool>,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    let error = |what: &str| {
        let message = format!("{constrType} constraints cannot be marked {what}");
        Err(yyscanner.error(ERRCODE_FEATURE_NOT_SUPPORTED, &message, location))
    };
    // The defaults.
    set(&mut deferrable, false);
    set(&mut initdeferred, false);
    set(&mut not_valid, false);
    set(&mut is_enforced, true);

    if cas_bits & (CAS_DEFERRABLE | CAS_INITIALLY_DEFERRED) != 0 && !set(&mut deferrable, true) {
        return error("DEFERRABLE");
    }
    if cas_bits & CAS_INITIALLY_DEFERRED != 0 && !set(&mut initdeferred, true) {
        return error("DEFERRABLE");
    }
    if cas_bits & CAS_NOT_VALID != 0 && !set(&mut not_valid, true) {
        return error("NOT VALID");
    }
    if cas_bits & CAS_NO_INHERIT != 0 && !set(&mut no_inherit, true) {
        return error("NO INHERIT");
    }
    if cas_bits & CAS_NOT_ENFORCED != 0 {
        if !set(&mut is_enforced, false) {
            return error("NOT ENFORCED");
        }
        // A constraint that is NOT ENFORCED is not validated either, so that it is in the correct state when it is changed to ENFORCED later.
        set(&mut not_valid, true);
    }
    if cas_bits & CAS_ENFORCED != 0 && !set(&mut is_enforced, true) {
        return error("ENFORCED");
    }
    Ok(())
}

/// `preprocess_pubobj_list`: gives each object of a publication with no keyword before it the type of the object before it, and checks the objects.
pub(crate) fn preprocess_pubobj_list(
    pubobjspec_list: &mut List,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    if pubobjspec_list.is_empty() {
        return Ok(());
    }
    let pubobj = castRef::<PublicationObjSpec>(linitial(pubobjspec_list))?;
    if pubobj.pubobjtype == PublicationObjSpecType::PUBLICATIONOBJ_CONTINUATION {
        let message = "invalid publication object list";
        return Err(Error {
            detail: Some(
                "One of TABLE or TABLES IN SCHEMA must be specified before a standalone table or schema name.",
            ),
            ..yyscanner.error(ERRCODE_SYNTAX_ERROR, message, pubobj.location)
        });
    }
    let mut prevobjtype = PublicationObjSpecType::PUBLICATIONOBJ_CONTINUATION;
    for cell in pubobjspec_list.iter_mut() {
        let pubobj = castMut::<PublicationObjSpec>(cell)?;
        let location = pubobj.location;
        let error = |message| Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, message, location));
        if pubobj.pubobjtype == PublicationObjSpecType::PUBLICATIONOBJ_CONTINUATION {
            pubobj.pubobjtype = prevobjtype;
        }
        match pubobj.pubobjtype {
            PublicationObjSpecType::PUBLICATIONOBJ_TABLE => {
                // The name of the relation or the table must be set for this type of object.
                if pubobj.name.is_none() && pubobj.pubtable.is_none() {
                    return error("invalid table name");
                }
                if let Some(name) = pubobj.name.take() {
                    // Change it to a `PublicationTable`.
                    let relation = makeRangeVar(None, Some(name), location);
                    let pubtable = PublicationTable {
                        relation: Some(Box::new(relation)),
                        ..Default::default()
                    };
                    pubobj.pubtable = Some(Box::new(pubtable));
                }
            }
            PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_SCHEMA
            | PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_CUR_SCHEMA => {
                if let Some(pubtable) = &pubobj.pubtable {
                    // A schema object has no WHERE clause and no column list.
                    if pubtable.whereClause.is_some() {
                        return error("WHERE clause not allowed for schema");
                    }
                    if !pubtable.columns.is_empty() {
                        return error("column specification not allowed for schema");
                    }
                }
                // The name and the table tell the types of schema objects apart.
                pubobj.pubobjtype = match (&pubobj.name, &pubobj.pubtable) {
                    (Some(_), _) => PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_SCHEMA,
                    (None, None) => PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_CUR_SCHEMA,
                    (None, Some(_)) => return error("invalid schema name"),
                };
            }
            _ => {}
        }
        prevobjtype = pubobj.pubobjtype;
    }
    Ok(())
}

/// `preprocess_pub_all_objtype_list`: sets the flags of `FOR ALL TABLES` and `FOR ALL SEQUENCES` and adds the tables of `EXCEPT` to the publication objects. Each kind can come once only.
pub(crate) fn preprocess_pub_all_objtype_list(
    mut all_objects_list: List,
    pubobjects: &mut List,
    all_tables: &mut bool,
    all_sequences: &mut bool,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    if all_objects_list.is_empty() {
        return Ok(());
    }
    *all_tables = false;
    *all_sequences = false;
    for cell in &mut all_objects_list {
        let obj = castMut::<PublicationAllObjSpec>(cell)?;
        let error = |detail| Error {
            detail: Some(detail),
            ..yyscanner.error(ERRCODE_SYNTAX_ERROR, "invalid publication object list", obj.location)
        };
        if obj.pubobjtype == PublicationAllObjType::PUBLICATION_ALL_TABLES {
            if *all_tables {
                return Err(error("ALL TABLES can be specified only once."));
            }
            *all_tables = true;
            pubobjects.append(&mut obj.except_tables);
        } else if obj.pubobjtype == PublicationAllObjType::PUBLICATION_ALL_SEQUENCES {
            if *all_sequences {
                return Err(error("ALL SEQUENCES can be specified only once."));
            }
            *all_sequences = true;
        }
    }
    Ok(())
}

/// `extractArgTypes`: the types of the input arguments of a list of `FunctionParameter` nodes.
pub(crate) fn extractArgTypes(parameters: &List) -> Result<List, Error> {
    let mut result = List::new();
    for p in parameters {
        let p = castRef::<FunctionParameter>(p.as_ref())?;
        if p.mode != FunctionParameterMode::FUNC_PARAM_OUT
            && p.mode != FunctionParameterMode::FUNC_PARAM_TABLE
        {
            result.push(p.argType.clone().map(NodeType::into_node));
        }
    }
    Ok(result)
}

/// `extractAggrArgTypes`: the types of the arguments of an `aggr_args`, a list of the arguments and the number of the direct arguments.
pub(crate) fn extractAggrArgTypes(aggrargs: &List) -> Result<List, Error> {
    match linitial(aggrargs) {
        None => Ok(List::new()),
        Some(Node::List(arguments)) => extractArgTypes(arguments),
        Some(_) => Err(Error::internal("an aggr_args that is not a list")),
    }
}

/// `makeOrderedSetArgs`: the `aggr_args` of an ordered-set aggregate, the direct arguments and the aggregated arguments in one list and the number of the direct arguments.
pub(crate) fn makeOrderedSetArgs(
    directargs: List,
    mut orderedargs: List,
    yyscanner: &Parser<'_>,
) -> Result<List, Error> {
    let lastd = castRef::<FunctionParameter>(llast(&directargs))?;
    // No restriction unless the last direct argument is VARIADIC.
    if lastd.mode == FunctionParameterMode::FUNC_PARAM_VARIADIC {
        let firsto = castRef::<FunctionParameter>(linitial(&orderedargs))?;
        // The names are ignored. There are no defaults to check, because aggr_arg has none.
        if orderedargs.len() != 1
            || firsto.mode != FunctionParameterMode::FUNC_PARAM_VARIADIC
            || !lastd.argType.equal(&firsto.argType)
        {
            let message = "an ordered-set aggregate with a VARIADIC direct argument must have one VARIADIC aggregated argument of the same data type";
            return Err(yyscanner.error(ERRCODE_FEATURE_NOT_SUPPORTED, message, firsto.location));
        }
        // Drop the VARIADIC argument that is the same in the two lists.
        orderedargs = List::new();
    }
    let ndirectargs = makeInteger(list_length(&directargs));
    Ok(list_make2(listNode(list_concat(directargs, orderedargs)), Some(ndirectargs)))
}

/// `mergeTableFuncParameters`: the arguments and the columns of `RETURNS TABLE` in one list.
pub(crate) fn mergeTableFuncParameters(
    func_args: List,
    columns: List,
    yyscanner: &Parser<'_>,
) -> Result<List, Error> {
    // OUT and INOUT arguments are not used in this syntax.
    for p in &func_args {
        let p = castRef::<FunctionParameter>(p.as_ref())?;
        if p.mode != FunctionParameterMode::FUNC_PARAM_DEFAULT
            && p.mode != FunctionParameterMode::FUNC_PARAM_IN
            && p.mode != FunctionParameterMode::FUNC_PARAM_VARIADIC
        {
            let message = "OUT and INOUT arguments aren't allowed in TABLE functions";
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, message, p.location));
        }
    }
    Ok(list_concat(func_args, columns))
}

/// `TableFuncTypeName`: the result type of `RETURNS TABLE`, a set of the type of the one column, or else a set of `record`.
pub(crate) fn TableFuncTypeName(columns: &List) -> Result<TypeName, Error> {
    let mut result = if list_length(columns) == 1 {
        let p = castRef::<FunctionParameter>(linitial(columns))?;
        let argType =
            p.argType.as_deref().ok_or_else(|| Error::internal("a column with no type"))?;
        argType.clone()
    } else {
        SystemTypeName("record")
    };
    result.setof = true;
    Ok(result)
}

/// `parsePartitionStrategy`: the strategy of `PARTITION BY`. `pg_strcasecmp` folds the case of the ASCII letters only.
pub(crate) fn parsePartitionStrategy(
    strategy: Option<Str>,
    location: i32,
    yyscanner: &Parser<'_>,
) -> Result<PartitionStrategy, Error> {
    let name = strategy.as_deref().unwrap_or_default();
    if name.eq_ignore_ascii_case("list") {
        Ok(PartitionStrategy::PARTITION_STRATEGY_LIST)
    } else if name.eq_ignore_ascii_case("range") {
        Ok(PartitionStrategy::PARTITION_STRATEGY_RANGE)
    } else if name.eq_ignore_ascii_case("hash") {
        Ok(PartitionStrategy::PARTITION_STRATEGY_HASH)
    } else {
        let message = format!("unrecognized partitioning strategy \"{name}\"");
        Err(yyscanner.error(ERRCODE_INVALID_PARAMETER_VALUE, &message, location))
    }
}

/// `makeRecursiveViewSelect`: the query of `CREATE RECURSIVE VIEW`, as `WITH RECURSIVE relname (aliases) AS (query) SELECT aliases FROM relname`.
pub(crate) fn makeRecursiveViewSelect(
    relname: Option<Str>,
    aliases: &List,
    query: Option<Node>,
) -> Node {
    let cte = CommonTableExpr {
        ctename: relname.clone(),
        aliascolnames: aliases.clone(),
        ctematerialized: CTEMaterialize::CTEMaterializeDefault,
        ctequery: query,
        location: -1,
        ..Default::default()
    };
    let w = WithClause { ctes: list_make1(Some(cte.into())), recursive: true, location: -1 };
    // The target list of the new SELECT is the alias list of the view.
    let mut tl = List::new();
    for alias in aliases {
        // `makeColumnRef` with no indirection.
        let val = ColumnRef {
            fields: list_make1(Some(makeString(strVal(alias.as_ref())))),
            location: -1,
        };
        let rt = ResTarget { val: Some(val.into()), location: -1, ..Default::default() };
        tl.push(Some(rt.into()));
    }
    let s = SelectStmt {
        withClause: Some(Box::new(w)),
        targetList: tl,
        fromClause: list_make1(Some(makeRangeVar(None, relname, -1).into())),
        ..Default::default()
    };
    s.into()
}

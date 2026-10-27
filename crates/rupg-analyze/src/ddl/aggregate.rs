//! `CREATE AGGREGATE`: `DefineAggregate` of `aggregatecmds.c`, which reads the options, and `AggregateCreate` of `pg_aggregate.c`, which finds the support functions and adds the rows of `pg_proc` and `pg_aggregate`.

use rupg_catalog::{Aggregate, Function, ObjRef, PG_OPERATOR, PG_PROC};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{DefElem, DefineStmt, FunctionParameterMode, Node, TypeName};
use rupg_types::oid;

use super::Definer;
use super::reloptions::def_boolean;
use super::tsearch::{def_name, def_string, no_parameter, qualified_name};
use crate::coerce::AtOpt;
use crate::typename::{names, place, type_name_text};
use crate::types;

/// The OID of the language `internal`, which an aggregate has in `pg_proc`.
const INTERNAL_LANGUAGE: u32 = 12;
/// `FUNC_MAX_ARGS`.
const FUNC_MAX_ARGS: usize = 100;

/// The options of `CREATE AGGREGATE`, as `DefineAggregate` reads them.
#[derive(Default)]
struct Options<'d> {
    transfn: Option<Vec<&'d str>>,
    finalfn: Option<Vec<&'d str>>,
    combinefn: Option<Vec<&'d str>>,
    serialfn: Option<Vec<&'d str>>,
    deserialfn: Option<Vec<&'d str>>,
    mtransfn: Option<Vec<&'d str>>,
    minvtransfn: Option<Vec<&'d str>>,
    mfinalfn: Option<Vec<&'d str>>,
    finalextra: bool,
    mfinalextra: bool,
    finalmodify: u8,
    mfinalmodify: u8,
    sortop: Option<Vec<&'d str>>,
    support: Option<Vec<&'d str>>,
    basetype: Option<TypeName>,
    transtype: Option<TypeName>,
    transspace: i32,
    mtranstype: Option<TypeName>,
    mtransspace: i32,
    initval: Option<String>,
    minitval: Option<String>,
    parallel: Option<String>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message.into())
}

fn mismatch(message: String) -> Error {
    Error::new(SqlState::DATATYPE_MISMATCH, message)
}

/// `defGetTypeName`.
pub(super) fn def_type_name(def: &DefElem) -> Result<TypeName> {
    match &def.arg {
        None => Err(no_parameter(def)),
        Some(Node::TypeName(name)) => Ok((**name).clone()),
        Some(Node::String(name)) => {
            Ok(TypeName { names: vec![Some(Node::String(name.clone()))], ..TypeName::default() })
        }
        Some(_) => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("argument of {} must be a type name", def_name(def)),
        )),
    }
}

/// `defGetInt32`.
fn def_int32(def: &DefElem) -> Result<i32> {
    match &def.arg {
        Some(Node::Integer(n)) => Ok(*n),
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("{} requires an integer value", def_name(def)),
        )),
    }
}

/// `extractModify`: `r`, `s` or `w`.
fn extract_modify(def: &DefElem) -> Result<u8> {
    match def_string(def)?.as_str() {
        "read_only" => Ok(b'r'),
        "shareable" => Ok(b's'),
        "read_write" => Ok(b'w'),
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("parameter \"{}\" must be READ_ONLY, SHAREABLE, or READ_WRITE", def_name(def)),
        )),
    }
}

/// `IsPolymorphicTypeFamily1`.
fn family1(ty: u32) -> bool {
    matches!(
        ty,
        oid::ANYELEMENT
            | oid::ANYARRAY
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
    )
}

/// `IsPolymorphicTypeFamily2`.
fn family2(ty: u32) -> bool {
    matches!(
        ty,
        oid::ANYCOMPATIBLE
            | oid::ANYCOMPATIBLEARRAY
            | oid::ANYCOMPATIBLENONARRAY
            | oid::ANYCOMPATIBLERANGE
            | oid::ANYCOMPATIBLEMULTIRANGE
    )
}

/// `check_valid_polymorphic_signature`: the detail of the error when a polymorphic result has no input that tells its type.
pub(super) fn polymorphic_detail(ret: u32, args: &[u32]) -> Option<String> {
    let (ok, inputs): (fn(u32) -> bool, &str) = match ret {
        oid::ANYRANGE | oid::ANYMULTIRANGE => {
            (|t| matches!(t, oid::ANYRANGE | oid::ANYMULTIRANGE), "anyrange or anymultirange")
        }
        oid::ANYCOMPATIBLERANGE | oid::ANYCOMPATIBLEMULTIRANGE => (
            |t| matches!(t, oid::ANYCOMPATIBLERANGE | oid::ANYCOMPATIBLEMULTIRANGE),
            "anycompatiblerange or anycompatiblemultirange",
        ),
        _ if family1(ret) => {
            (family1, "anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange")
        }
        _ if family2(ret) => (
            family2,
            "anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange",
        ),
        _ => return None,
    };
    if args.iter().any(|&ty| ok(ty)) {
        return None;
    }
    Some(format!(
        "A result of type {} requires at least one input of type {inputs}.",
        types::name(ret)
    ))
}

/// True for a pseudo-type that is not polymorphic.
fn pseudo(ty: u32) -> bool {
    builtin::type_by_oid(ty).is_some_and(|t| t.kind == b'p')
}

impl Definer<'_, '_> {
    /// `DefineAggregate` and `AggregateCreate`.
    pub(super) fn create_aggregate(&mut self, stmt: &DefineStmt) -> Result<()> {
        let list = names(&stmt.defnames);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let mut kind = b'n';
        let mut ndirect = 0;
        let mut params = None;
        if !stmt.oldstyle {
            if let Some(Some(Node::Integer(n))) = stmt.args.get(1)
                && *n >= 0
            {
                kind = b'o';
                ndirect = usize::try_from(*n).unwrap_or_default();
            }
            params = Some(match stmt.args.first() {
                Some(Some(Node::List(list))) => list.as_slice(),
                _ => &[],
            });
        }
        let defs: Vec<&DefElem> = stmt
            .definition
            .iter()
            .flatten()
            .filter_map(|n| if let Node::DefElem(d) = n { Some(&**d) } else { None })
            .collect();
        let mut o = Options::default();
        for def in defs {
            match def_name(def) {
                "sfunc" | "sfunc1" => o.transfn = Some(qualified_name(def)?),
                "finalfunc" => o.finalfn = Some(qualified_name(def)?),
                "combinefunc" => o.combinefn = Some(qualified_name(def)?),
                "serialfunc" => o.serialfn = Some(qualified_name(def)?),
                "deserialfunc" => o.deserialfn = Some(qualified_name(def)?),
                "msfunc" => o.mtransfn = Some(qualified_name(def)?),
                "minvfunc" => o.minvtransfn = Some(qualified_name(def)?),
                "mfinalfunc" => o.mfinalfn = Some(qualified_name(def)?),
                "finalfunc_extra" => o.finalextra = def_boolean(def_name(def), def.arg.as_ref())?,
                "mfinalfunc_extra" => {
                    o.mfinalextra = def_boolean(def_name(def), def.arg.as_ref())?;
                }
                "finalfunc_modify" => o.finalmodify = extract_modify(def)?,
                "mfinalfunc_modify" => o.mfinalmodify = extract_modify(def)?,
                "sortop" => o.sortop = Some(qualified_name(def)?),
                "support" => o.support = Some(qualified_name(def)?),
                "basetype" => o.basetype = Some(def_type_name(def)?),
                "hypothetical" => {
                    if def_boolean(def_name(def), def.arg.as_ref())? {
                        if kind == b'n' {
                            return Err(invalid("only ordered-set aggregates can be hypothetical"));
                        }
                        kind = b'h';
                    }
                }
                "stype" | "stype1" => o.transtype = Some(def_type_name(def)?),
                "sspace" => o.transspace = def_int32(def)?,
                "mstype" => o.mtranstype = Some(def_type_name(def)?),
                "msspace" => o.mtransspace = def_int32(def)?,
                "initcond" | "initcond1" => o.initval = Some(def_string(def)?),
                "minitcond" => o.minitval = Some(def_string(def)?),
                "parallel" => o.parallel = Some(def_string(def)?),
                other => self.warning(
                    SqlState::SYNTAX_ERROR,
                    format!("aggregate attribute \"{other}\" not recognized"),
                ),
            }
        }
        let Some(transtype) = o.transtype.take() else {
            return Err(invalid("aggregate stype must be specified"));
        };
        let Some(transfn) = o.transfn.take() else {
            return Err(invalid("aggregate sfunc must be specified"));
        };
        if o.mtranstype.is_some() {
            if o.mtransfn.is_none() {
                return Err(invalid("aggregate msfunc must be specified when mstype is specified"));
            }
            if o.minvtransfn.is_none() {
                return Err(invalid(
                    "aggregate minvfunc must be specified when mstype is specified",
                ));
            }
        } else {
            let given = [
                (o.mtransfn.is_some(), "msfunc"),
                (o.minvtransfn.is_some(), "minvfunc"),
                (o.mfinalfn.is_some(), "mfinalfunc"),
                (o.mtransspace != 0, "msspace"),
                (o.minitval.is_some(), "minitcond"),
            ];
            if let Some((_, option)) = given.iter().find(|(given, _)| *given) {
                return Err(invalid(format!(
                    "aggregate {option} must not be specified without mstype"
                )));
            }
        }
        let default_modify = if kind == b'n' { b'r' } else { b'w' };
        if o.finalmodify == 0 {
            o.finalmodify = default_modify;
        }
        if o.mfinalmodify == 0 {
            o.mfinalmodify = default_modify;
        }
        let mut argtypes = Vec::new();
        let mut argmodes = Vec::new();
        let mut argnames = Vec::new();
        let mut variadic = 0;
        match params {
            None => {
                let Some(basetype) = &o.basetype else {
                    return Err(invalid("aggregate input type must be specified"));
                };
                if !type_name_text(basetype).eq_ignore_ascii_case("any") {
                    argtypes
                        .push(self.an.type_name(basetype).map_err(|e| e.with_position(None))?.0);
                }
            }
            Some(params) => {
                if o.basetype.is_some() {
                    return Err(invalid(
                        "basetype is redundant with aggregate input type specification",
                    ));
                }
                for param in params.iter().flatten() {
                    let Node::FunctionParameter(param) = param else { continue };
                    let at = place(param.location);
                    let ty = param
                        .argType
                        .as_deref()
                        .ok_or_else(|| Error::internal("an argument type"))?;
                    if ty.setof {
                        return Err(invalid("aggregates cannot accept set arguments").at_opt(at));
                    }
                    // `interpret_function_parameter_list` gives the name of a type that does not exist without quotes.
                    let Some(ty) = self.an.lookup_type_name(ty)? else {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("type {} does not exist", type_name_text(ty)),
                        )
                        .at_opt(place(ty.location)));
                    };
                    if variadic != 0 {
                        return Err(invalid("VARIADIC parameter must be the last input parameter")
                            .at_opt(at));
                    }
                    let mode = if param.mode == FunctionParameterMode::FUNC_PARAM_VARIADIC {
                        if !matches!(ty, oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY | oid::ANY)
                            && types::element(ty) == 0
                        {
                            return Err(invalid("VARIADIC parameter must be an array").at_opt(at));
                        }
                        variadic = ty;
                        b'v'
                    } else {
                        b'i'
                    };
                    let name = param.name.as_deref().unwrap_or_default();
                    if !name.is_empty() && argnames.iter().any(|n: &String| n == name) {
                        return Err(invalid(format!(
                            "parameter name \"{name}\" used more than once"
                        ))
                        .at_opt(at));
                    }
                    argtypes.push(ty);
                    argmodes.push(mode);
                    argnames.push(name.to_string());
                }
            }
        }
        let transtype = self.an.type_name(&transtype).map_err(|e| e.with_position(None))?.0;
        let superuser = self.is_superuser();
        let check_transtype = |ty: u32| {
            if pseudo(ty) && !types::is_polymorphic(ty) && !(ty == oid::INTERNAL && superuser) {
                return Err(invalid(format!(
                    "aggregate transition data type cannot be {}",
                    types::name(ty)
                )));
            }
            Ok(())
        };
        check_transtype(transtype)?;
        match (&o.serialfn, &o.deserialfn) {
            (Some(_), Some(_)) if transtype != oid::INTERNAL => {
                return Err(invalid(
                    "serialization functions may be specified only when the aggregate transition data type is internal",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(invalid(
                    "must specify both or neither of serialization and deserialization functions",
                ));
            }
            _ => {}
        }
        let mtranstype = match o.mtranstype.take() {
            Some(name) => {
                let ty = self.an.type_name(&name).map_err(|e| e.with_position(None))?.0;
                check_transtype(ty)?;
                ty
            }
            None => 0,
        };
        if let Some(initval) = &o.initval
            && !pseudo(transtype)
        {
            self.an.env.input(transtype, initval, -1)?;
        }
        if let Some(minitval) = &o.minitval
            && mtranstype != 0
            && !pseudo(mtranstype)
        {
            self.an.env.input(mtranstype, minitval, -1)?;
        }
        let parallel = match o.parallel.as_deref() {
            None | Some("unsafe") => b'u',
            Some("safe") => b's',
            Some("restricted") => b'r',
            Some(_) => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
                ));
            }
        };

        // `AggregateCreate`.
        let nargs = argtypes.len();
        if nargs > FUNC_MAX_ARGS - 1 {
            return Err(Error::new(
                SqlState::TOO_MANY_ARGUMENTS,
                format!("aggregates cannot have more than {} arguments", FUNC_MAX_ARGS - 1),
            ));
        }
        let no_transtype =
            |detail: String| invalid("cannot determine transition data type").with_detail(detail);
        if let Some(detail) = polymorphic_detail(transtype, &argtypes) {
            return Err(no_transtype(detail));
        }
        if mtranstype != 0
            && let Some(detail) = polymorphic_detail(mtranstype, &argtypes)
        {
            return Err(no_transtype(detail));
        }
        if kind != b'n' && variadic != 0 && variadic != oid::ANY {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a variadic ordered-set aggregate must use VARIADIC type ANY",
            ));
        }
        if kind == b'h' && ndirect < nargs {
            let aggregated = nargs - ndirect;
            if variadic != 0
                || ndirect < aggregated
                || argtypes[ndirect - aggregated..ndirect] != argtypes[ndirect..]
            {
                return Err(invalid(
                    "a hypothetical-set aggregate must have direct arguments matching its aggregated arguments",
                ));
            }
        }
        let mut fnargs = vec![transtype];
        if kind == b'n' {
            fnargs.extend(&argtypes);
        } else if ndirect < nargs {
            fnargs.extend(&argtypes[ndirect..]);
        } else {
            fnargs.extend(argtypes.last());
        }
        let strict = |f: u32| builtin::proc_by_oid(f).is_some_and(|p| p.strict);
        let compatible = |ty: u32| {
            argtypes.first().is_some_and(|&first| crate::typcache::is_binary_coercible(first, ty))
        };
        let (transfn_oid, rettype) = self.an.agg_support_function(&transfn, &fnargs, variadic)?;
        if rettype != transtype {
            return Err(mismatch(format!(
                "return type of transition function {} is not {}",
                transfn.join("."),
                self.type_text(transtype)
            )));
        }
        let omitted = || {
            invalid(
                "must not omit initial value when transition function is strict and transition type is not compatible with input type",
            )
        };
        if strict(transfn_oid) && o.initval.is_none() && !compatible(transtype) {
            return Err(omitted());
        }
        let mut mtransfn = 0;
        let mut mstrict = false;
        if let Some(names) = &o.mtransfn {
            fnargs[0] = mtranstype;
            let (found, rettype) = self.an.agg_support_function(names, &fnargs, variadic)?;
            if rettype != mtranstype {
                return Err(mismatch(format!(
                    "return type of transition function {} is not {}",
                    names.join("."),
                    self.type_text(mtranstype)
                )));
            }
            mstrict = strict(found);
            if mstrict && o.minitval.is_none() && !compatible(mtranstype) {
                return Err(omitted());
            }
            mtransfn = found;
        }
        let mut minvtransfn = 0;
        if let Some(names) = &o.minvtransfn {
            let (found, rettype) = self.an.agg_support_function(names, &fnargs, variadic)?;
            if rettype != mtranstype {
                return Err(mismatch(format!(
                    "return type of inverse transition function {} is not {}",
                    names.join("."),
                    self.type_text(mtranstype)
                )));
            }
            if strict(found) != mstrict {
                return Err(invalid(
                    "strictness of aggregate's forward and inverse transition functions must match",
                ));
            }
            minvtransfn = found;
        }
        // The arguments of a final function: the state and all the arguments with `FINALFUNC_EXTRA`, else the state and the direct arguments.
        let final_function = |an: &crate::Analyzer<'_>, names: &[&str], state: u32, extra: bool| {
            let mut args = vec![state];
            let mut variadic = variadic;
            if extra {
                args.extend(&argtypes);
            } else {
                args.extend(&argtypes[..ndirect]);
                if ndirect < nargs {
                    variadic = 0;
                }
            }
            let (found, rettype) = an.agg_support_function(names, &args, variadic)?;
            if extra && strict(found) {
                return Err(invalid(
                    "final function with extra arguments must not be declared STRICT",
                ));
            }
            Ok((found, rettype))
        };
        let (finalfn, finaltype) = match &o.finalfn {
            Some(names) => final_function(&self.an, names, transtype, o.finalextra)?,
            None => (0, transtype),
        };
        let mut combinefn = 0;
        if let Some(names) = &o.combinefn {
            let (found, rettype) =
                self.an.agg_support_function(names, &[transtype, transtype], 0)?;
            if rettype != transtype {
                return Err(mismatch(format!(
                    "return type of combine function {} is not {}",
                    names.join("."),
                    self.type_text(transtype)
                )));
            }
            if transtype == oid::INTERNAL && strict(found) {
                return Err(invalid(format!(
                    "combine function with transition type {} must not be declared STRICT",
                    self.type_text(transtype)
                )));
            }
            combinefn = found;
        }
        let mut serialfn = 0;
        if let Some(names) = &o.serialfn {
            let (found, rettype) = self.an.agg_support_function(names, &[oid::INTERNAL], 0)?;
            if rettype != oid::BYTEA {
                return Err(mismatch(format!(
                    "return type of serialization function {} is not bytea",
                    names.join(".")
                )));
            }
            serialfn = found;
        }
        let mut deserialfn = 0;
        if let Some(names) = &o.deserialfn {
            let (found, rettype) =
                self.an.agg_support_function(names, &[oid::BYTEA, oid::INTERNAL], 0)?;
            if rettype != oid::INTERNAL {
                return Err(mismatch(format!(
                    "return type of deserialization function {} is not internal",
                    names.join(".")
                )));
            }
            deserialfn = found;
        }
        if let Some(detail) = polymorphic_detail(finaltype, &argtypes) {
            return Err(mismatch("cannot determine result data type".into()).with_detail(detail));
        }
        if finaltype == oid::INTERNAL && !argtypes.contains(&oid::INTERNAL) {
            return Err(invalid("unsafe use of pseudo-type \"internal\"").with_detail(
                "A result of type internal requires at least one input of type internal.",
            ));
        }
        let mut mfinalfn = 0;
        if mtranstype != 0 {
            let rettype = match &o.mfinalfn {
                Some(names) => {
                    let (found, rettype) =
                        final_function(&self.an, names, mtranstype, o.mfinalextra)?;
                    mfinalfn = found;
                    rettype
                }
                None => mtranstype,
            };
            if rettype != finaltype {
                return Err(invalid(format!(
                    "moving-aggregate implementation returns type {}, but plain implementation returns type {}",
                    self.type_text(rettype),
                    self.type_text(finaltype)
                )));
            }
        }
        let mut sortop = 0;
        if let Some(list) = &o.sortop {
            if nargs != 1 {
                return Err(invalid(
                    "sort operator can only be specified for single-argument aggregates",
                ));
            }
            let ty = argtypes[0];
            let Some(found) = self.opername_get_oprid(list, ty, ty)? else {
                return Err(Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {}", self.op_signature(list, ty, ty)),
                ));
            };
            sortop = found.oid;
        }
        let mut support = 0;
        if let Some(names) = &o.support {
            let (found, rettype) = self.an.agg_support_function(names, &[oid::INTERNAL], 0)?;
            if rettype != oid::INTERNAL {
                return Err(mismatch(format!(
                    "return type of support function {} is not internal",
                    names.join(".")
                )));
            }
            if !superuser {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be superuser to specify a support function",
                ));
            }
            support = found;
        }

        // `ProcedureCreate`.
        let named = argnames.iter().any(|n| !n.is_empty());
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            lang: INTERNAL_LANGUAGE,
            cost: 1.0,
            rows: 0,
            support,
            kind: b'a',
            strict: false,
            leakproof: false,
            volatile: b'i',
            parallel,
            rettype: finaltype,
            retset: false,
            allargtypes: if variadic != 0 { argtypes.clone() } else { Vec::new() },
            argtypes,
            argmodes: if variadic != 0 { argmodes } else { Vec::new() },
            argnames: if named { argnames } else { Vec::new() },
            argdefaults: None,
            nargdefaults: 0,
            src: "aggregate_dummy".into(),
            bin: String::new(),
            body: None,
            config: Vec::new(),
            secdef: false,
        };
        let aggregate = Aggregate {
            function: 0,
            kind,
            ndirect: i16::try_from(ndirect).unwrap_or_default(),
            transfn: transfn_oid,
            finalfn,
            combinefn,
            serialfn,
            deserialfn,
            mtransfn,
            minvtransfn,
            mfinalfn,
            finalextra: o.finalextra,
            mfinalextra: o.mfinalextra,
            finalmodify: o.finalmodify,
            mfinalmodify: o.mfinalmodify,
            sortop,
            transtype,
            transspace: o.transspace,
            mtranstype,
            mtransspace: o.mtransspace,
            initval: o.initval,
            minitval: o.minitval,
        };
        let old = self
            .catalog
            .functions()
            .find(|f| f.namespace == namespace && f.name == name && f.argtypes == function.argtypes)
            .cloned();
        if let Some(old) = &old {
            if !stmt.replace {
                return Err(Error::new(
                    SqlState::DUPLICATE_FUNCTION,
                    format!("function \"{name}\" already exists with same argument types"),
                ));
            }
            self.check_replace(old, &function)?;
            if let Some(prior) = self.catalog.aggregate(old.oid) {
                if prior.kind != kind {
                    let what = match prior.kind {
                        b'n' => "an ordinary aggregate function",
                        b'o' => "an ordered-set aggregate",
                        _ => "a hypothetical-set aggregate",
                    };
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        "cannot change routine kind",
                    )
                    .with_detail(format!("\"{name}\" is {what}.")));
                }
                if prior.ndirect != aggregate.ndirect {
                    return Err(invalid(
                        "cannot change number of direct arguments of an aggregate function",
                    ));
                }
            }
        }
        let mut refs = self.function_refs(&function, &[], support, Vec::new())?;
        let procs = [
            aggregate.transfn,
            aggregate.finalfn,
            aggregate.combinefn,
            aggregate.serialfn,
            aggregate.deserialfn,
            aggregate.mtransfn,
            aggregate.minvtransfn,
            aggregate.mfinalfn,
        ];
        let objects = procs
            .iter()
            .filter(|&&f| f != 0)
            .map(|&f| ObjRef::new(PG_PROC, f))
            .chain((sortop != 0).then(|| ObjRef::new(PG_OPERATOR, sortop)));
        for object in objects {
            if !refs.contains(&object) {
                refs.push(object);
            }
        }
        match old {
            Some(old) => self.catalog.replace_aggregate(old.oid, function, aggregate, &refs),
            None => {
                let oid = self.catalog.create_aggregate(function, aggregate, &refs)?;
                self.apply_default_acl(ObjRef::new(PG_PROC, oid))
            }
        }
    }
}

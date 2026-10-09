//! The choice of a function or an operator for the types of the arguments, as `parse_func.c` and `parse_oper.c` make it, with the errors of PostgreSQL when no function fits or when more than one fits.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, AggregateRow, OperatorRow};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::{Kind, Parts};
use crate::coerce::{AtOpt, Context, Path, can_coerce, find_path};
use crate::expr::{Expr, ExprKind, Func, FuncForm};
use crate::poly;
use crate::types;

/// `FUNC_MAX_ARGS`.
pub(crate) const FUNC_MAX_ARGS: usize = 100;

/// `FGC_SCHEMA_GIVEN`: the name has a schema.
const SCHEMA_GIVEN: u32 = 0x1;
/// `FGC_NAME_EXISTS`: a function of the name exists in some schema.
const NAME_EXISTS: u32 = 0x4;
/// `FGC_NAME_VISIBLE`: a function of the name exists in a schema of the path or in the given schema.
const NAME_VISIBLE: u32 = 0x8;
/// `FGC_ARGCOUNT_MATCH`: a function of the name takes the number of arguments.
const ARGCOUNT_MATCH: u32 = 0x10;

/// `FuncCandidateList`: a function or an operator that the name gives, with the argument types after the expansion of a variadic argument.
#[derive(Clone, Debug)]
struct Candidate {
    /// The OID of the function or of the operator, or 0 when two candidates have the same argument types and neither one wins.
    oid: u32,
    args: Vec<u32>,
    /// The number of arguments that the variadic argument takes, or 0.
    nvargs: usize,
    pathpos: usize,
}

/// The result of `func_get_detail`.
enum Detail {
    NotFound,
    Multiple,
    /// `type(x)` is a cast to the type.
    Coercion(u32),
    Normal(Candidate),
}

/// `funcname_signature_string`: the name and the argument types, such as `lower(integer)`.
/// The checks of `ParseFuncOrColumn` of the parts of a call that only an aggregate or a window function can have.
fn check_parts(
    parts: &Parts<'_>,
    detail: &Detail,
    prokind: u8,
    aggregate: Option<&AggregateRow>,
    name: &str,
    at: Option<usize>,
) -> Result<()> {
    let wrong = |message: String| Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message).at_opt(at));
    let plain = match detail {
        Detail::Normal(_) => prokind == b'f',
        Detail::Coercion(_) => true,
        _ => false,
    };
    if plain {
        if parts.star {
            return wrong(format!("{name}(*) specified, but {name} is not an aggregate function"));
        }
        if parts.distinct {
            return wrong(format!("DISTINCT specified, but {name} is not an aggregate function"));
        }
        if parts.within_group {
            return wrong(format!(
                "WITHIN GROUP specified, but {name} is not an aggregate function"
            ));
        }
        if !parts.order.is_empty() {
            return wrong(format!("ORDER BY specified, but {name} is not an aggregate function"));
        }
        if parts.filter.is_some() {
            return wrong(format!("FILTER specified, but {name} is not an aggregate function"));
        }
        if parts.over.is_some() {
            return wrong(format!(
                "OVER specified, but {name} is not a window function nor an aggregate function"
            ));
        }
        if parts.null_treatment {
            return wrong(format!(
                "RESPECT/IGNORE NULLS specified, but {name} is not a window function"
            ));
        }
    } else if let Some(row) = aggregate {
        if row.kind == b'o' || row.kind == b'h' {
            if !parts.within_group {
                return wrong(format!("WITHIN GROUP is required for ordered-set aggregate {name}"));
            }
            if parts.over.is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("OVER is not supported for ordered-set aggregate {name}"),
                )
                .at_opt(at));
            }
        } else if parts.within_group {
            return wrong(format!(
                "{name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"
            ));
        }
        if parts.null_treatment {
            return wrong("aggregate functions do not accept RESPECT/IGNORE NULLS".into());
        }
    } else if prokind == b'w' {
        if parts.over.is_none() {
            return wrong(format!("window function {name} requires an OVER clause"));
        }
        if parts.within_group {
            return wrong(format!("window function {name} cannot have WITHIN GROUP"));
        }
    }
    Ok(())
}

fn signature(name: &str, args: &[u32]) -> String {
    let args: Vec<String> = args.iter().map(|&t| types::name(t)).collect();
    format!("{name}({})", args.join(", "))
}

/// `op_signature_string`: such as `integer + text` or `- text`.
fn op_signature(name: &str, left: u32, right: u32) -> String {
    if left == 0 {
        format!("{name} {}", types::name(right))
    } else {
        format!("{} {name} {}", types::name(left), types::name(right))
    }
}

/// `func_match_argtypes`: the candidates that the inputs can take with implicit casts.
fn match_argtypes(inputs: &[u32], candidates: &[Candidate]) -> Vec<Candidate> {
    candidates.iter().filter(|c| can_coerce(inputs, &c.args, Context::Implicit)).cloned().collect()
}

/// Keeps the candidates with the most points, where `points` gives the points of a candidate.
fn keep_best(candidates: Vec<Candidate>, points: impl Fn(&Candidate) -> usize) -> Vec<Candidate> {
    let best = candidates.iter().map(&points).max().unwrap_or(0);
    candidates.into_iter().filter(|c| points(c) == best).collect()
}

/// `func_select_candidate`: the one candidate that the rules of PostgreSQL choose, or `None` when they do not choose one.
fn select_candidate(inputs: &[u32], candidates: Vec<Candidate>) -> Option<Candidate> {
    let mut bases: Vec<u32> = inputs
        .iter()
        .map(|&t| if t == oid::UNKNOWN { oid::UNKNOWN } else { types::base(t) })
        .collect();
    let unknowns = bases.iter().filter(|&&t| t == oid::UNKNOWN).count();

    // Keep the candidates with the most exact matches on the known inputs.
    let mut candidates = keep_best(candidates, |c| {
        bases.iter().zip(&c.args).filter(|&(&b, &a)| b != oid::UNKNOWN && a == b).count()
    });
    if candidates.len() == 1 {
        return candidates.pop();
    }

    // Then the most exact matches or preferred types of the category of the input.
    let categories: Vec<u8> = bases.iter().map(|&t| types::category(t)).collect();
    candidates = keep_best(candidates, |c| {
        bases
            .iter()
            .zip(&c.args)
            .zip(&categories)
            .filter(|&((&b, &a), &cat)| {
                b != oid::UNKNOWN && (a == b || types::is_preferred(cat, a))
            })
            .count()
    });
    if candidates.len() == 1 {
        return candidates.pop();
    }
    if unknowns == 0 {
        return None;
    }

    // For each unknown input, find the category that the candidates take there. The string category wins over a conflict.
    let mut slots = vec![(types::INVALID, false); inputs.len()];
    let mut resolved = true;
    for (i, slot) in slots.iter_mut().enumerate() {
        if bases[i] != oid::UNKNOWN {
            continue;
        }
        let mut conflict = false;
        for c in &candidates {
            let (category, preferred) = types::category_preferred(c.args[i]);
            if slot.0 == types::INVALID {
                *slot = (category, preferred);
            } else if category == slot.0 {
                slot.1 |= preferred;
            } else if category == types::STRING {
                *slot = (category, preferred);
            } else {
                conflict = true;
            }
        }
        if conflict && slot.0 != types::STRING {
            resolved = false;
            break;
        }
    }
    if resolved {
        let kept: Vec<Candidate> = candidates
            .iter()
            .filter(|c| {
                (0..inputs.len()).all(|i| {
                    if bases[i] != oid::UNKNOWN {
                        return true;
                    }
                    let (category, preferred) = types::category_preferred(c.args[i]);
                    category == slots[i].0 && (!slots[i].1 || preferred)
                })
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            candidates = kept;
        }
        if candidates.len() == 1 {
            return candidates.pop();
        }
    }

    // When all the known inputs have one type, take the unknown inputs as that type too.
    if unknowns < inputs.len() {
        let mut known = bases.iter().filter(|&&t| t != oid::UNKNOWN);
        let first = *known.next()?;
        if known.all(|&t| t == first) {
            bases.fill(first);
            let mut fits =
                candidates.into_iter().filter(|c| can_coerce(&bases, &c.args, Context::Implicit));
            let one = fits.next()?;
            return if fits.next().is_none() { Some(one) } else { None };
        }
    }
    None
}

impl Analyzer<'_> {
    /// The schema of a name, from the session.
    pub(crate) fn schema(&self, name: &str) -> Option<u32> {
        self.env.namespace(name)
    }

    /// The position of the schema in the search path, or `None` when it is not in the path.
    fn path_position(&self, namespace: u32) -> Option<usize> {
        self.path.iter().position(|&ns| ns == namespace)
    }

    /// `DeconstructQualifiedName`: the schema that the name gives, if any, and the last name.
    pub(crate) fn split_name<'n>(
        &self,
        names: &[&'n str],
        at: Option<usize>,
    ) -> Result<(Option<u32>, &'n str)> {
        match names {
            [name] => Ok((None, name)),
            [schema, name] => {
                let ns = self.schema(schema).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    )
                    .at_opt(at)
                })?;
                Ok((Some(ns), name))
            }
            [catalog, schema, name] => {
                if *catalog != self.env.database() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.join(".")
                        ),
                    )
                    .at_opt(at));
                }
                self.split_name(&[schema, name], at)
            }
            _ => Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("improper qualified name (too many dotted names): {}", names.join(".")),
            )
            .at_opt(at)),
        }
    }

    /// `FuncnameGetCandidates` with positional arguments and no defaults.
    fn func_candidates(
        &self,
        names: &[&str],
        nargs: usize,
        expand_variadic: bool,
        at: Option<usize>,
    ) -> Result<(Vec<Candidate>, u32)> {
        let (schema, name) = self.split_name(names, at)?;
        let mut flags = if schema.is_some() { SCHEMA_GIVEN } else { 0 };
        let mut list: Vec<Candidate> = Vec::new();
        for proc in builtin::procs_named(name) {
            flags |= NAME_EXISTS;
            let pathpos = match schema {
                Some(ns) if proc.namespace == ns => 0,
                Some(_) => continue,
                None => match self.path_position(proc.namespace) {
                    Some(pos) => pos,
                    None => continue,
                },
            };
            flags |= NAME_VISIBLE;
            let pronargs = proc.argtypes.len();
            let variadic = pronargs <= nargs && expand_variadic && proc.variadic != 0;
            if pronargs != nargs && !variadic {
                continue;
            }
            flags |= ARGCOUNT_MATCH;
            let mut args = proc.argtypes.to_vec();
            let mut nvargs = 0;
            if variadic {
                nvargs = nargs - pronargs + 1;
                args.truncate(pronargs - 1);
                args.resize(nargs, proc.variadic);
            }
            let new = Candidate { oid: proc.oid, args, nvargs, pathpos };
            if let Some(prev) = list.iter_mut().find(|p| p.args == new.args) {
                let preference = if new.pathpos != prev.pathpos {
                    new.pathpos.cmp(&prev.pathpos) as i32
                } else if variadic && prev.nvargs == 0 {
                    1
                } else if !variadic && prev.nvargs > 0 {
                    -1
                } else {
                    0
                };
                match preference {
                    p if p > 0 => {}
                    p if p < 0 => *prev = new,
                    _ => prev.oid = 0,
                }
                continue;
            }
            list.push(new);
        }
        Ok((list, flags))
    }

    /// `FuncNameAsType`: the type of the name of a function, for `type(x)`.
    fn func_name_as_type(&self, names: &[&str]) -> Option<u32> {
        let row = match names {
            [name] => self.path.iter().find_map(|&ns| builtin::type_by_name(ns, name)),
            [schema, name] => builtin::type_by_name(self.schema(schema)?, name),
            _ => None,
        }?;
        (row.relid == 0).then_some(row.oid)
    }

    /// `func_get_detail` for positional arguments.
    fn func_detail(
        &self,
        names: &[&str],
        args: &[Expr],
        inputs: &[u32],
        expand_variadic: bool,
        at: Option<usize>,
    ) -> Result<(Detail, u32)> {
        let (candidates, flags) = self.func_candidates(names, inputs.len(), expand_variadic, at)?;
        let exact = candidates.iter().find(|c| inputs.is_empty() || c.args == inputs);
        let best = match exact {
            Some(c) => Some(c.clone()),
            None => {
                if let ([arg], Some(target)) = (args, self.func_name_as_type(names)) {
                    let source = inputs[0];
                    let coercion =
                        if source == oid::UNKNOWN && matches!(arg.kind, ExprKind::Const(_)) {
                            true
                        } else {
                            match find_path(target, source, Context::Explicit) {
                                Path::Relabel => true,
                                Path::ViaIo => {
                                    let complex = source == oid::RECORD
                                        || types::row(source).is_some_and(|t| t.relid != 0);
                                    !(complex && types::category(target) == types::STRING)
                                }
                                _ => false,
                            }
                        };
                    if coercion {
                        return Ok((Detail::Coercion(target), flags));
                    }
                }
                let matched = match_argtypes(inputs, &candidates);
                match matched.len() {
                    0 => None,
                    1 => matched.into_iter().next(),
                    _ => match select_candidate(inputs, matched) {
                        Some(c) => Some(c),
                        None => return Ok((Detail::Multiple, flags)),
                    },
                }
            }
        };
        Ok(match best {
            Some(c) if c.oid == 0 => (Detail::Multiple, flags),
            Some(c) => (Detail::Normal(c), flags),
            None => (Detail::NotFound, flags),
        })
    }

    /// `ParseFuncOrColumn` for a call of a plain function with positional arguments.
    pub(crate) fn make_func(
        &mut self,
        names: &[&str],
        args: Vec<Expr>,
        variadic_keyword: bool,
        parts: Option<Parts<'_>>,
        at: Option<usize>,
    ) -> Result<Expr> {
        if args.len() > FUNC_MAX_ARGS {
            return Err(Error::new(
                SqlState::TOO_MANY_ARGUMENTS,
                format!("cannot pass more than {FUNC_MAX_ARGS} arguments to a function"),
            )
            .at_opt(at));
        }
        let inputs: Vec<u32> = args.iter().map(|a| a.ty).collect();
        let (detail, flags) = self.func_detail(names, &args, &inputs, !variadic_keyword, at)?;
        let name = names.join(".");
        let proc = match &detail {
            Detail::Normal(c) => builtin::proc_by_oid(c.oid),
            _ => None,
        };
        let prokind = proc.map_or(b'f', |p| p.kind);
        if prokind == b'p' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{} is a procedure", signature(&name, &inputs)),
            )
            .with_hint("To call a procedure, use CALL.")
            .at_opt(at));
        }
        let aggregate = match (&detail, prokind) {
            (Detail::Normal(_), b'a') => builtin::aggregate(proc.map_or(0, |p| p.oid)),
            _ => None,
        };
        if let Some(parts) = &parts {
            check_parts(parts, &detail, prokind, aggregate, &name, at)?;
        }
        let candidate = match detail {
            Detail::Coercion(target) => {
                let arg = args.into_iter().next().expect("one argument");
                return self.coerce(arg, target, -1, Context::Explicit, at);
            }
            Detail::Multiple => {
                return Err(Error::new(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("function {} is not unique", signature(&name, &inputs)),
                )
                .with_detail("Could not choose a best candidate function.")
                .with_hint("You might need to add explicit type casts.")
                .at_opt(at));
            }
            Detail::NotFound => {
                if let Some(parts) = &parts
                    && parts.order.len() > 1
                    && !parts.within_group
                {
                    return Err(Error::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("function {} does not exist", signature(&name, &inputs)),
                    )
                    .with_detail("No aggregate function matches the given name and argument types.")
                    .with_hint("Perhaps you misplaced ORDER BY; ORDER BY must appear after all regular arguments of the aggregate.")
                    .at_opt(at));
                }
                let error = Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function {} does not exist", signature(&name, &inputs)),
                );
                let error = if flags & NAME_VISIBLE == 0 {
                    if flags & SCHEMA_GIVEN != 0 {
                        error
                    } else if flags & NAME_EXISTS == 0 {
                        error.with_detail("There is no function of that name.")
                    } else {
                        error.with_detail(
                            "A function of that name exists, but it is not in the search_path.",
                        )
                    }
                } else if flags & ARGCOUNT_MATCH == 0 {
                    error.with_detail(
                        "No function of that name accepts the given number of arguments.",
                    )
                } else {
                    error
                        .with_detail("No function of that name accepts the given argument types.")
                        .with_hint("You might need to add explicit type casts.")
                };
                return Err(error.at_opt(at));
            }
            Detail::Normal(c) => c,
        };
        let proc = builtin::proc_by_oid(candidate.oid).expect("candidate in pg_proc");
        let mut declared = candidate.args.clone();
        let result = poly::resolve(&inputs, &mut declared, proc.rettype)?;
        self.check_internal(&declared, result, at)?;
        let mut args = self.make_arguments(args, &declared)?;
        let mut variadic = variadic_keyword;
        if candidate.nvargs > 0 && proc.variadic != oid::ANY {
            let rest = args.split_off(args.len() - candidate.nvargs);
            let element = rest[0].ty;
            let place = rest[0].place();
            let array = types::array_of(element);
            if array == 0 {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("could not find array type for data type {}", types::name(element)),
                )
                .at_opt(place));
            }
            args.push(Expr {
                kind: ExprKind::Array { element, multidims: false, elements: rest },
                ty: array,
                typmod: -1,
                location: place,
            });
            variadic = true;
        }
        if variadic_keyword && proc.variadic == oid::ANY {
            let last = args.last().expect("a VARIADIC argument");
            if types::element(types::base(last.ty)) == 0 {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "VARIADIC argument must be an array",
                )
                .at_opt(last.place()));
            }
        }
        if proc.retset {
            self.check_srf_placement(&signature(&name, &inputs), at)?;
        }
        let over = parts.as_ref().is_some_and(|p| p.over.is_some());
        if over && let Some(parts) = parts {
            return self.window_call(
                proc,
                aggregate.is_some().then_some(&*name),
                args,
                parts,
                result,
                at,
            );
        }
        if let (Some(row), Some(parts)) = (aggregate, parts) {
            if args.len() > FUNC_MAX_ARGS - 1 {
                return Err(Error::new(
                    SqlState::TOO_MANY_ARGUMENTS,
                    format!("aggregates cannot have more than {} arguments", FUNC_MAX_ARGS - 1),
                )
                .at_opt(at));
            }
            if args.is_empty() && !parts.star && !parts.within_group {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("{name}(*) must be used to call a parameterless aggregate function"),
                )
                .at_opt(at));
            }
            if row.kind != b'n' {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("ordered-set aggregate {name} is not supported yet"),
                )
                .at_opt(at));
            }
            return self.aggregate_call(proc.oid, args, parts, variadic, result, at);
        }
        if prokind != b'f' {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a call of this function is not supported yet",
            )
            .at_opt(at));
        }
        if proc.retset {
            self.srfs = (self.srfs.0 + 1, at);
        }
        Ok(Expr {
            kind: ExprKind::Func(Func {
                oid: proc.oid,
                args,
                form: FuncForm::Call,
                variadic,
                retset: proc.retset,
            }),
            ty: result,
            typmod: -1,
            location: at,
        })
    }

    /// `check_srf_call_placement`: the error of a call of a function that gives a set in a part of the query where it cannot be. A function in `FROM` can only be at the top, which `transformRangeFunction` checks.
    fn check_srf_placement(&self, signature: &str, at: Option<usize>) -> Result<()> {
        let message = match self.kind {
            Kind::Select
            | Kind::GroupBy
            | Kind::OrderBy
            | Kind::DistinctOn
            | Kind::FromFunction => return Ok(()),
            Kind::JoinOn => "set-returning functions are not allowed in JOIN conditions".to_owned(),
            Kind::WindowPartition | Kind::WindowOrder => {
                "set-returning functions are not allowed in window definitions".to_owned()
            }
            Kind::Where
            | Kind::Having
            | Kind::Filter
            | Kind::Limit
            | Kind::Offset
            | Kind::Values
            | Kind::ColumnDefault
            | Kind::Check
            | Kind::IndexExpression
            | Kind::IndexPredicate
            | Kind::ExecuteParameter => {
                format!("set-returning functions are not allowed in {}", self.kind.name())
            }
            Kind::ValuesSingle | Kind::Other => {
                format!("set-returning function {signature} is not supported yet")
            }
        };
        Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, message).at_opt(at))
    }

    /// The error of a call of a function that gives a set in the arguments of `CASE` or `COALESCE`, when the analysis made such a call after `before`, which is the number of the calls before the arguments.
    pub(crate) fn no_srf_since(&self, before: usize, construct: &str) -> Result<()> {
        if self.srfs.0 == before {
            return Ok(());
        }
        Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("set-returning functions are not allowed in {construct}"),
        )
        .with_hint(crate::agg::SRF_HINT)
        .at_opt(self.srfs.1))
    }

    /// The errors of a function that takes or gives `internal`.
    fn check_internal(&self, declared: &[u32], result: u32, at: Option<usize>) -> Result<()> {
        if declared.contains(&oid::INTERNAL) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "functions accepting type \"internal\" cannot be called explicitly",
            )
            .at_opt(at));
        }
        if result == oid::INTERNAL {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "functions returning type \"internal\" cannot be called explicitly",
            )
            .at_opt(at));
        }
        Ok(())
    }

    /// `make_fn_arguments`: each argument cast to its declared type.
    fn make_arguments(&mut self, args: Vec<Expr>, declared: &[u32]) -> Result<Vec<Expr>> {
        args.into_iter()
            .zip(declared)
            .map(|(arg, &ty)| {
                if arg.ty == ty {
                    Ok(arg)
                } else {
                    self.coerce(arg, ty, -1, Context::Implicit, None)
                }
            })
            .collect()
    }

    /// `OpernameGetCandidates` and `OpernameGetOprid`: the operators of the name and the kind, `b` for an infix operator and `l` for a prefix operator.
    fn oper_candidates(
        &self,
        names: &[&str],
        kind: u8,
        at: Option<usize>,
    ) -> Result<(Vec<(&'static OperatorRow, usize)>, u32)> {
        let (schema, name) = self.split_name(names, at)?;
        let mut flags = if schema.is_some() { SCHEMA_GIVEN } else { 0 };
        let mut list = Vec::new();
        for op in builtin::operators_named(name) {
            flags |= NAME_EXISTS;
            let pathpos = match schema {
                Some(ns) if op.namespace == ns => 0,
                Some(_) => continue,
                None => match self.path_position(op.namespace) {
                    Some(pos) => pos,
                    None => continue,
                },
            };
            flags |= NAME_VISIBLE;
            if op.kind == kind {
                list.push((op, pathpos));
            }
        }
        list.sort_by_key(|&(_, pos)| pos);
        Ok((list, flags))
    }

    /// `oper` and `left_oper`: the operator for the types, with `left` 0 for a prefix operator.
    pub(crate) fn find_operator(
        &self,
        names: &[&str],
        left: u32,
        right: u32,
        at: Option<usize>,
    ) -> Result<&'static OperatorRow> {
        let kind = if left == 0 { b'l' } else { b'b' };
        let (ops, flags) = self.oper_candidates(names, kind, at)?;
        let exact = |l: u32, r: u32| {
            ops.iter().find(|(op, _)| op.left == l && op.right == r).map(|&(op, _)| op)
        };
        // `binary_oper_exact`: an unknown input takes the type of the other input.
        let (mut l, mut r, mut was_unknown) = (left, right, false);
        if left == oid::UNKNOWN && right != 0 {
            (l, was_unknown) = (right, true);
        } else if right == oid::UNKNOWN && left != 0 {
            (r, was_unknown) = (left, true);
        }
        let mut found = exact(l, r);
        if found.is_none() && was_unknown {
            let base = types::base(if left == 0 { r } else { l });
            if base != l {
                found = exact(if left == 0 { 0 } else { base }, base);
            }
        }
        let mut multiple = false;
        if found.is_none() && !ops.is_empty() {
            let inputs: Vec<u32> = if left == 0 { vec![right] } else { vec![left, right] };
            let candidates: Vec<Candidate> = ops
                .iter()
                .map(|&(op, pathpos)| Candidate {
                    oid: op.oid,
                    args: if left == 0 { vec![op.right] } else { vec![op.left, op.right] },
                    nvargs: 0,
                    pathpos,
                })
                .collect();
            let matched = match_argtypes(&inputs, &candidates);
            let chosen = match matched.len() {
                0 => None,
                1 => matched.into_iter().next(),
                _ => {
                    let chosen = select_candidate(&inputs, matched);
                    multiple = chosen.is_none();
                    chosen
                }
            };
            found = chosen.and_then(|c| builtin::operator_by_oid(c.oid));
        }
        let name = names.join(".");
        match found {
            Some(op) => Ok(op),
            None if multiple => Err(Error::new(
                SqlState::AMBIGUOUS_FUNCTION,
                format!("operator is not unique: {}", op_signature(&name, left, right)),
            )
            .with_detail("Could not choose a best candidate operator.")
            .with_hint("You might need to add explicit type casts.")
            .at_opt(at)),
            None => {
                let error = Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {}", op_signature(&name, left, right)),
                );
                let error = if flags & NAME_VISIBLE == 0 {
                    if flags & SCHEMA_GIVEN != 0 {
                        error
                    } else if flags & NAME_EXISTS == 0 {
                        error.with_detail("There is no operator of that name.")
                    } else {
                        error.with_detail(
                            "An operator of that name exists, but it is not in the search_path.",
                        )
                    }
                } else if left == 0 {
                    error
                        .with_detail("No operator of that name accepts the given argument type.")
                        .with_hint("You might need to add an explicit type cast.")
                } else {
                    error
                        .with_detail("No operator of that name accepts the given argument types.")
                        .with_hint("You might need to add explicit type casts.")
                };
                Err(error.at_opt(at))
            }
        }
    }

    /// `make_op`: the call of the operator for `left op right`, or `op right` when `left` is `None`.
    pub(crate) fn make_op(
        &mut self,
        names: &[&str],
        left: Option<Expr>,
        right: Expr,
        at: Option<usize>,
    ) -> Result<Expr> {
        let (op, args, inputs) = match left {
            None => {
                let op = self.find_operator(names, 0, right.ty, at)?;
                let inputs = vec![right.ty];
                (op, vec![right], inputs)
            }
            Some(left) => {
                let op = self.find_operator(names, left.ty, right.ty, at)?;
                let inputs = vec![left.ty, right.ty];
                (op, vec![left, right], inputs)
            }
        };
        let name = names.join(".");
        if op.code == 0 {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("operator is only a shell: {}", op_signature(&name, op.left, op.right)),
            )
            .at_opt(at));
        }
        let mut declared = if op.left == 0 { vec![op.right] } else { vec![op.left, op.right] };
        let result = poly::resolve(&inputs, &mut declared, op.result)?;
        self.check_internal(&declared, result, at)?;
        let args = self.make_arguments(args, &declared)?;
        if builtin::proc_by_oid(op.code).is_some_and(|p| p.retset) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "set-returning operator {} is not supported yet",
                    op_signature(&name, op.left, op.right)
                ),
            )
            .at_opt(at));
        }
        Ok(Expr {
            kind: ExprKind::Func(Func {
                oid: op.code,
                args,
                form: FuncForm::Operator(op.oid),
                variadic: false,
                retset: false,
            }),
            ty: result,
            typmod: -1,
            location: at,
        })
    }

    /// The OID of the function of the operator `name` for two inputs of the type, for `NULLIF`, `IS DISTINCT FROM`, `GREATEST` and `LEAST`, with the inputs cast to the types of the operator.
    pub(crate) fn operator_function(
        &mut self,
        name: &str,
        left: Expr,
        right: Expr,
        at: Option<usize>,
    ) -> Result<(u32, u32, Expr, Expr)> {
        let call = self.make_op(&[name], Some(left), right, at)?;
        let ty = call.ty;
        let ExprKind::Func(Func { oid, mut args, .. }) = call.kind else {
            unreachable!("make_op gives a call")
        };
        let right = args.pop().expect("two arguments");
        let left = args.pop().expect("two arguments");
        Ok((oid, ty, left, right))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(oid: u32, args: &[u32]) -> Candidate {
        Candidate { oid, args: args.to_vec(), nvargs: 0, pathpos: 0 }
    }

    #[test]
    fn select() {
        // `1 + 1.5`: int4 and numeric. The numeric operator is the one exact match.
        let list = vec![
            candidate(1, &[oid::INT4, oid::INT4]),
            candidate(2, &[oid::NUMERIC, oid::NUMERIC]),
            candidate(3, &[oid::FLOAT8, oid::FLOAT8]),
        ];
        let inputs = [oid::INT4, oid::UNKNOWN];
        assert_eq!(select_candidate(&inputs, list.clone()).map(|c| c.oid), Some(1));
        // An unknown input takes the string category.
        let list = vec![candidate(1, &[oid::TEXT]), candidate(2, &[oid::INT4])];
        assert_eq!(select_candidate(&[oid::UNKNOWN], list).map(|c| c.oid), Some(1));
        // No unknowns and no winner.
        let list = vec![
            candidate(1, &[oid::INT8, oid::INT8]),
            candidate(2, &[oid::NUMERIC, oid::NUMERIC]),
        ];
        assert!(select_candidate(&[oid::INT4, oid::INT2], list).is_none());
        assert_eq!(signature("f", &[oid::INT4, oid::TEXT]), "f(integer, text)");
        assert_eq!(op_signature("-", 0, oid::TEXT), "- text");
    }
}

//! The choice of a function or an operator for the types of the arguments, as `parse_func.c` and `parse_oper.c` make it, with the errors of PostgreSQL when no function fits or when more than one fits.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, AggregateRow, OperatorRow, ProcRow};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::{Kind, Parts};
use crate::coerce::{AtOpt, Context, Path, can_coerce, find_path};
use crate::expr::{Expr, ExprKind, Func, FuncForm, NamedArg, NullTreatment};
use crate::funcargs::{all_arguments, arg_defaults};
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
/// `FGC_ARGNAMES_MATCH`: a function has all the names of the named arguments.
const ARGNAMES_MATCH: u32 = 0x20;
/// `FGC_ARGNAMES_NONDUP`: no named argument is also a positional argument.
const ARGNAMES_NONDUP: u32 = 0x40;
/// `FGC_ARGNAMES_ALL`: the call gives all the arguments that have no default.
const ARGNAMES_ALL: u32 = 0x80;
/// `FGC_ARGNAMES_VALID`: a function fits the call with named arguments.
const ARGNAMES_VALID: u32 = 0x100;
/// `FGC_VARIADIC_FAIL`: `VARIADIC` with named arguments is not at the variadic argument.
const VARIADIC_FAIL: u32 = 0x200;

/// `FuncCandidateList`: a function or an operator that the name gives, with the argument types after the expansion of a variadic argument.
#[derive(Clone, Debug)]
struct Candidate {
    /// The OID of the function or of the operator, or 0 when two candidates have the same argument types and neither one wins.
    oid: u32,
    args: Vec<u32>,
    /// The number of arguments that the variadic argument takes, or 0.
    nvargs: usize,
    /// The number of defaults at the end of `args`, which the call does not give.
    ndargs: usize,
    /// For a call with named arguments, the number of the argument of the function for each argument of the call, then for each default.
    argnumbers: Option<Vec<usize>>,
    pathpos: usize,
}

impl Candidate {
    /// The argument types without the defaults, which decide if two candidates are the same.
    fn given(&self) -> &[u32] {
        &self.args[..self.args.len() - self.ndargs]
    }
}

/// The result of `func_get_detail`.
enum Detail {
    NotFound,
    Multiple,
    /// `type(x)` is a cast to the type.
    Coercion(u32),
    Normal(Candidate),
}

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
        if parts.nulls != NullTreatment::None {
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
        if parts.nulls != NullTreatment::None {
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

/// `funcname_signature_string`: the name and the argument types, such as `lower(integer)`. The names of the named arguments, which are the last arguments, come before their types, as `f(integer, b => text)`.
fn signature(name: &str, args: &[u32], argnames: &[&str]) -> String {
    let positional = args.len() - argnames.len();
    let args: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, &t)| match i.checked_sub(positional) {
            Some(n) => format!("{} => {}", argnames[n], types::name(t)),
            None => types::name(t),
        })
        .collect();
    format!("{name}({})", args.join(", "))
}

/// `MatchNamedCall`: the number of the argument of the function for each argument of the call, then for each default that the call needs. `None` when the names do not fit the function, with the flags that tell how far the match got.
fn match_named_call(
    proc: &ProcRow,
    nargs: usize,
    argnames: &[&str],
    flags: &mut u32,
) -> Option<Vec<usize>> {
    let names = proc.argnames?;
    let pronargs = proc.argtypes.len();
    let positional = nargs - argnames.len();
    let mut argnumbers: Vec<usize> = (0..positional).collect();
    let mut given = vec![false; names.len().max(pronargs)];
    given[..positional].fill(true);
    let mut twice = false;
    for &argname in argnames {
        // The number counts only the input arguments.
        let inputs = names.iter().enumerate().filter(|&(i, _)| {
            let mode = proc.argmodes.and_then(|m| m.get(i).copied()).unwrap_or(b'i');
            matches!(mode, b'i' | b'b' | b'v')
        });
        let pp = inputs.map(|(_, &name)| name).position(|name| name == argname)?;
        twice |= given[pp];
        given[pp] = true;
        argnumbers.push(pp);
    }
    *flags |= ARGNAMES_MATCH;
    if twice {
        return None;
    }
    *flags |= ARGNAMES_NONDUP;
    if nargs < pronargs {
        let first = pronargs.saturating_sub(usize::try_from(proc.nargdefaults).unwrap_or(0));
        for (pp, &done) in given.iter().enumerate().take(pronargs).skip(positional) {
            if done {
                continue;
            }
            if pp < first {
                return None;
            }
            argnumbers.push(pp);
        }
    }
    *flags |= ARGNAMES_ALL;
    Some(argnumbers)
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
    /// The schema of a name, from the session. `pg_temp` names the temporary schema of the session, as `LookupExplicitNamespace` reads it.
    pub(crate) fn schema(&self, name: &str) -> Option<u32> {
        if name == "pg_temp" && self.temp != 0 {
            return Some(self.temp);
        }
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

    /// `FuncnameGetCandidates` with the defaults of the arguments. `argnames` has the names of the named arguments, which are the last arguments. With `include_out_arguments`, for a `CALL`, a candidate takes all its arguments, also the output arguments.
    fn func_candidates(
        &self,
        names: &[&str],
        nargs: usize,
        argnames: &[&str],
        expand_variadic: bool,
        include_out_arguments: bool,
        at: Option<usize>,
    ) -> Result<(Vec<Candidate>, u32)> {
        let (schema, name) = self.split_name(names, at)?;
        let mut flags = if schema.is_some() { SCHEMA_GIVEN } else { 0 };
        let mut list: Vec<Candidate> = Vec::new();
        for proc in builtin::procs_named(name) {
            let all;
            let proc = if include_out_arguments {
                all = all_arguments(proc);
                &all
            } else {
                proc
            };
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
            let defaults = usize::try_from(proc.nargdefaults).unwrap_or(0);
            let use_defaults = pronargs > nargs;
            if use_defaults && nargs + defaults < pronargs {
                continue;
            }
            let mut variadic = false;
            let mut argnumbers = None;
            if argnames.is_empty() {
                variadic = pronargs <= nargs && expand_variadic && proc.variadic != 0;
                if pronargs != nargs && !variadic && !use_defaults {
                    continue;
                }
                flags |= ARGCOUNT_MATCH;
            } else {
                if pronargs != nargs && !use_defaults {
                    continue;
                }
                flags |= ARGCOUNT_MATCH;
                let Some(numbers) = match_named_call(proc, nargs, argnames, &mut flags) else {
                    continue;
                };
                // Named arguments cannot fit the arguments that a variadic array would have, so a variadic function fits only with `VARIADIC`.
                if proc.variadic != 0 && expand_variadic {
                    continue;
                }
                flags |= ARGNAMES_VALID;
                argnumbers = Some(numbers);
            }
            let mut args = match &argnumbers {
                Some(numbers) => numbers.iter().map(|&n| proc.argtypes[n]).collect(),
                None => proc.argtypes.to_vec(),
            };
            let mut nvargs = 0;
            if variadic {
                nvargs = nargs - pronargs + 1;
                args.truncate(pronargs - 1);
                args.resize(nargs, proc.variadic);
            }
            let ndargs = if use_defaults { pronargs - nargs } else { 0 };
            let new = Candidate { oid: proc.oid, args, nvargs, ndargs, argnumbers, pathpos };
            // The defaults do not count in the choice between two candidates with the same arguments.
            if let Some(prev) = list.iter_mut().find(|p| p.given() == new.given()) {
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

    /// `func_get_detail`. Only the arguments that the call gives decide the match, and not the defaults.
    #[allow(clippy::too_many_arguments)]
    fn func_detail(
        &self,
        names: &[&str],
        args: &[Expr],
        inputs: &[u32],
        argnames: &[&str],
        expand_variadic: bool,
        proc_call: bool,
        at: Option<usize>,
    ) -> Result<(Detail, u32)> {
        let nargs = inputs.len();
        let (candidates, mut flags) =
            self.func_candidates(names, nargs, argnames, expand_variadic, proc_call, at)?;
        let exact = candidates.iter().find(|c| nargs == 0 || c.args[..nargs] == *inputs);
        let best = match exact {
            Some(c) => Some(c.clone()),
            None => {
                if let ([arg], [], Some(target)) = (args, argnames, self.func_name_as_type(names)) {
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
            // `VARIADIC` with named arguments must be at the variadic argument.
            Some(c)
                if !argnames.is_empty()
                    && !expand_variadic
                    && nargs > 0
                    && c.argnumbers.as_ref().is_some_and(|n| n[nargs - 1] != nargs - 1) =>
            {
                flags |= VARIADIC_FAIL;
                (Detail::NotFound, flags)
            }
            Some(c) => (Detail::Normal(c), flags),
            None => (Detail::NotFound, flags),
        })
    }

    /// `unify_hypothetical_args`: each hypothetical direct argument of type `"any"` and its item of `WITHIN GROUP` get their common type. The type of the item wins when both are good, so that the aggregate casts one value and not all the rows.
    fn unify_hypothetical_args(
        &mut self,
        args: &mut [Expr],
        aggregated: usize,
        declared: &[u32],
    ) -> Result<()> {
        let direct = args.len() - aggregated;
        let Some(first) = direct.checked_sub(aggregated) else {
            return Err(Error::internal(
                "incorrect number of arguments to hypothetical-set aggregate",
            ));
        };
        for harg in first..direct {
            let aarg = direct + harg - first;
            if declared.get(harg) != declared.get(aarg) {
                return Err(Error::internal(
                    "hypothetical-set aggregate has inconsistent declared argument types",
                ));
            }
            if declared.get(harg) != Some(&oid::ANY) {
                continue;
            }
            let pair = [args[aarg].clone(), args[harg].clone()];
            let ty = self.common_type(&pair, "WITHIN GROUP")?;
            let [a, h] = pair;
            args[harg] = self.coerce_to_common(h, ty, "WITHIN GROUP")?;
            args[aarg] = self.coerce_to_common(a, ty, "WITHIN GROUP")?;
        }
        Ok(())
    }

    /// The types of the defaults that a candidate takes, in the order of `args` of the candidate.
    fn default_types(proc: &ProcRow, candidate: &Candidate) -> Result<Vec<u32>> {
        if candidate.ndargs == 0 {
            return Ok(Vec::new());
        }
        let defaults = arg_defaults(proc)?;
        if candidate.ndargs > defaults.len() {
            return Err(Error::internal("not enough default arguments"));
        }
        Ok(match &candidate.argnumbers {
            // The defaults of the arguments that the call does not name, in the order of the arguments.
            Some(numbers) => {
                let defaulted = &numbers[numbers.len() - candidate.ndargs..];
                let first = proc.argtypes.len() - defaults.len();
                defaults
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| defaulted.contains(&(first + i)))
                    .map(|(_, d)| d.ty)
                    .collect()
            }
            None => defaults[defaults.len() - candidate.ndargs..].iter().map(|d| d.ty).collect(),
        })
    }

    /// `ParseFuncOrColumn` for a call of a function. The named arguments come after the positional arguments. `proc_call` is true for the call of a `CALL`, which must find a procedure, and whose arguments are all the arguments of the procedure, also the output arguments.
    pub(crate) fn make_func(
        &mut self,
        names: &[&str],
        mut args: Vec<Expr>,
        variadic_keyword: bool,
        parts: Option<Parts<'_>>,
        proc_call: bool,
        at: Option<usize>,
    ) -> Result<Expr> {
        if args.len() > FUNC_MAX_ARGS {
            let what = if proc_call { "procedure" } else { "function" };
            return Err(Error::new(
                SqlState::TOO_MANY_ARGUMENTS,
                format!("cannot pass more than {FUNC_MAX_ARGS} arguments to a {what}"),
            )
            .at_opt(at));
        }
        let mut owned_names: Vec<String> = Vec::new();
        for arg in &args {
            if let ExprKind::NamedArg(na) = &arg.kind {
                if owned_names.contains(&na.name) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("argument name \"{}\" used more than once", na.name),
                    )
                    .at_opt(arg.location));
                }
                owned_names.push(na.name.clone());
            } else if !owned_names.is_empty() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "positional argument cannot follow named argument",
                )
                .at_opt(arg.place()));
            }
        }
        let argnames: Vec<&str> = owned_names.iter().map(String::as_str).collect();
        let inputs: Vec<u32> = args.iter().map(|a| a.ty).collect();
        let (detail, flags) =
            self.func_detail(names, &args, &inputs, &argnames, !variadic_keyword, proc_call, at)?;
        let name = names.join(".");
        let proc = match &detail {
            Detail::Normal(c) => builtin::proc_by_oid(c.oid),
            _ => None,
        };
        let prokind = proc.map_or(b'f', |p| p.kind);
        let found = matches!(detail, Detail::Normal(_) | Detail::Coercion(_));
        if proc_call && found && prokind != b'p' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{} is not a procedure", signature(&name, &inputs, &argnames)),
            )
            .with_hint("To call a function, use SELECT.")
            .at_opt(at));
        }
        if prokind == b'p' && !proc_call {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{} is a procedure", signature(&name, &inputs, &argnames)),
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
        let what = if proc_call { "procedure" } else { "function" };
        let candidate = match detail {
            Detail::Coercion(target) => {
                let arg = args.into_iter().next().expect("one argument");
                return self.coerce(arg, target, -1, Context::Explicit, at);
            }
            Detail::Multiple => {
                return Err(Error::new(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("{what} {} is not unique", signature(&name, &inputs, &argnames)),
                )
                .with_detail(format!("Could not choose a best candidate {what}."))
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
                        format!("function {} does not exist", signature(&name, &inputs, &argnames)),
                    )
                    .with_detail("No aggregate function matches the given name and argument types.")
                    .with_hint("Perhaps you misplaced ORDER BY; ORDER BY must appear after all regular arguments of the aggregate.")
                    .at_opt(at));
                }
                let error = Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("{what} {} does not exist", signature(&name, &inputs, &argnames)),
                );
                let error = if flags & NAME_VISIBLE == 0 {
                    if flags & SCHEMA_GIVEN != 0 {
                        error
                    } else if flags & NAME_EXISTS == 0 {
                        error.with_detail(format!("There is no {what} of that name."))
                    } else {
                        error.with_detail(format!(
                            "A {what} of that name exists, but it is not in the search_path."
                        ))
                    }
                } else if flags & ARGCOUNT_MATCH == 0 {
                    error.with_detail(format!(
                        "No {what} of that name accepts the given number of arguments."
                    ))
                } else if !argnames.is_empty() && flags & ARGNAMES_MATCH == 0 {
                    error.with_detail(format!(
                        "No {what} of that name accepts the given argument names."
                    ))
                } else if !argnames.is_empty() && flags & ARGNAMES_NONDUP == 0 {
                    error.with_detail("In the closest available match, an argument was specified both positionally and by name.")
                } else if !argnames.is_empty() && flags & ARGNAMES_ALL == 0 {
                    error.with_detail(
                        "In the closest available match, not all required arguments were supplied.",
                    )
                } else if !argnames.is_empty() && flags & ARGNAMES_VALID == 0 {
                    error.with_hint("This call would be correct if the variadic array were labeled VARIADIC and placed last.")
                } else if flags & VARIADIC_FAIL != 0 {
                    error.with_hint(
                        "The VARIADIC parameter must be placed last, even when using argument names.",
                    )
                } else {
                    error
                        .with_detail(format!(
                            "No {what} of that name accepts the given argument types."
                        ))
                        .with_hint("You might need to add explicit type casts.")
                };
                return Err(error.at_opt(at));
            }
            Detail::Normal(c) => c,
        };
        let proc = builtin::proc_by_oid(candidate.oid).expect("candidate in pg_proc");
        let all;
        let proc = if proc_call {
            all = all_arguments(proc);
            &all
        } else {
            proc
        };
        if let Some(numbers) = &candidate.argnumbers {
            for (arg, &number) in args.iter_mut().zip(numbers) {
                if let ExprKind::NamedArg(na) = &mut arg.kind {
                    na.number = number;
                }
            }
        }
        if let (Some(row), Some(parts)) = (aggregate, &parts)
            && (row.kind == b'o' || row.kind == b'h')
        {
            let aggregated = parts.order.len();
            let direct = args.len() - aggregated;
            let wrong = |hint: String| {
                Err(Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function {} does not exist", signature(&name, &inputs, &argnames)),
                )
                .with_hint(hint)
                .at_opt(at))
            };
            let need = usize::try_from(row.ndirect).unwrap_or(0);
            let plural = if need == 1 { "" } else { "s" };
            if proc.variadic == 0 {
                if direct != need {
                    return wrong(format!(
                        "There is an ordered-set aggregate {name}, but it requires {need} direct argument{plural}, not {direct}."
                    ));
                }
            } else if row.kind == b'h' {
                if candidate.nvargs != 2 * aggregated {
                    return wrong(format!(
                        "To use the hypothetical-set aggregate {name}, the number of hypothetical direct arguments (here {}) must match the number of ordering columns (here {aggregated}).",
                        candidate.nvargs.saturating_sub(aggregated)
                    ));
                }
            } else if candidate.nvargs <= aggregated {
                return wrong(format!(
                    "There is an ordered-set aggregate {name}, but it requires at least {need} direct argument{plural}."
                ));
            }
            if row.kind == b'h' {
                self.unify_hypothetical_args(&mut args, aggregated, &candidate.args)?;
            }
        }
        // The types of the defaults count for the polymorphic types, but the call does not keep the defaults. The planner adds them.
        let mut actual: Vec<u32> = args.iter().map(|a| a.ty).collect();
        actual.extend(Self::default_types(proc, &candidate)?);
        let mut declared = candidate.args.clone();
        let result = poly::resolve(&actual, &mut declared, proc.rettype)?;
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
            self.check_srf_placement(&signature(&name, &inputs, &argnames), at)?;
        }
        let over = parts.as_ref().is_some_and(|p| p.over.is_some());
        if over && !argnames.is_empty() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a window function call with named arguments is not supported yet",
            )
            .at_opt(at));
        }
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
            if !argnames.is_empty() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "aggregates cannot use named arguments",
                )
                .at_opt(at));
            }
            return self.aggregate_call(proc.oid, row.kind, args, parts, variadic, result, at);
        }
        if prokind != b'f' && !proc_call {
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
            Kind::WindowPartition
            | Kind::WindowOrder
            | Kind::WindowRange
            | Kind::WindowRows
            | Kind::WindowGroups => {
                "set-returning functions are not allowed in window definitions".to_owned()
            }
            Kind::Where
            | Kind::Having
            | Kind::Filter
            | Kind::Limit
            | Kind::Offset
            | Kind::Values
            | Kind::UpdateSource
            | Kind::MergeWhen
            | Kind::Returning
            | Kind::ColumnDefault
            | Kind::FunctionDefault
            | Kind::Check
            | Kind::IndexExpression
            | Kind::IndexPredicate
            | Kind::ExecuteParameter
            | Kind::Transform
            | Kind::TriggerWhen
            | Kind::CallArgument
            | Kind::PartitionBound
            | Kind::PartitionExpression => {
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

    /// `make_fn_arguments`: each argument cast to its declared type. A named argument keeps its name above the cast.
    fn make_arguments(&mut self, args: Vec<Expr>, declared: &[u32]) -> Result<Vec<Expr>> {
        args.into_iter()
            .zip(declared)
            .map(|(arg, &ty)| {
                if arg.ty == ty {
                    return Ok(arg);
                }
                match arg.kind {
                    ExprKind::NamedArg(na) => {
                        let NamedArg { arg: inner, name, number } = *na;
                        let inner = self.coerce(inner, ty, -1, Context::Implicit, None)?;
                        let (ty, typmod) = (inner.ty, inner.typmod);
                        let kind =
                            ExprKind::NamedArg(Box::new(NamedArg { arg: inner, name, number }));
                        Ok(Expr { kind, ty, typmod, location: arg.location })
                    }
                    kind => self.coerce(Expr { kind, ..arg }, ty, -1, Context::Implicit, None),
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
                    ndargs: 0,
                    argnumbers: None,
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
        Candidate { oid, args: args.to_vec(), nvargs: 0, ndargs: 0, argnumbers: None, pathpos: 0 }
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
        assert_eq!(signature("f", &[oid::INT4, oid::TEXT], &[]), "f(integer, text)");
        assert_eq!(op_signature("-", 0, oid::TEXT), "- text");
    }
}

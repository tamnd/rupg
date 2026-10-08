//! The casts of `parse_coerce.c`: which casts exist in each context, the expression of a cast, the common type of a list of expressions, and the cast to `boolean` of a condition.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::expr::{Expr, ExprKind, Func, FuncForm};
use crate::types;

/// `CoercionContext`: where a cast happens. A cast is allowed in its own context and in each later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Context {
    /// An expression, such as the argument of a function.
    Implicit,
    /// The value of `INSERT` and `UPDATE`.
    Assignment,
    /// `CAST(x AS type)` and `x::type`.
    Explicit,
}

/// `CoercionPathType`: how a cast happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    None,
    /// The function with this OID.
    Func(u32),
    /// No change of the binary form.
    Relabel,
    /// Each element of an array, with the cast of the element type.
    ArrayCoerce,
    /// The output function of the source, then the input function of the target.
    ViaIo,
}

/// `find_coercion_pathway`.
pub fn find_path(target: u32, source: u32, context: Context) -> Path {
    let (target, source) = (types::base(target), types::base(source));
    if source == target {
        return Path::Relabel;
    }
    if source == oid::INTERNAL || target == oid::INTERNAL {
        return Path::None;
    }
    if let Some(cast) = builtin::cast(source, target) {
        let allowed = match cast.context {
            b'i' => Context::Implicit,
            b'a' => Context::Assignment,
            _ => Context::Explicit,
        };
        if context < allowed {
            return Path::None;
        }
        return match cast.method {
            b'f' => Path::Func(cast.func),
            b'i' => Path::ViaIo,
            _ => Path::Relabel,
        };
    }
    if target != oid::OIDVECTOR && target != oid::INT2VECTOR {
        let (target_element, source_element) = (types::element(target), types::element(source));
        if target_element != 0
            && source_element != 0
            && find_path(target_element, source_element, context) != Path::None
        {
            return Path::ArrayCoerce;
        }
    }
    // Any type has an I/O cast to a string type in assignment, and a string type has an explicit I/O cast to any type.
    if (context >= Context::Assignment && types::category(target) == types::STRING)
        || (context >= Context::Explicit && types::category(source) == types::STRING)
    {
        Path::ViaIo
    } else {
        Path::None
    }
}

/// `can_coerce_type`: true if each input type has a cast to its target type in the context.
pub fn can_coerce(inputs: &[u32], targets: &[u32], context: Context) -> bool {
    let mut generic = false;
    for (&input, &target) in inputs.iter().zip(targets) {
        if input == target {
            continue;
        }
        if input == oid::INTERNAL || target == oid::INTERNAL {
            return false;
        }
        if target == oid::ANY {
            continue;
        }
        if types::is_polymorphic(target) {
            generic = true;
            continue;
        }
        if input == oid::UNKNOWN {
            continue;
        }
        if find_path(target, input, context) != Path::None {
            continue;
        }
        return false;
    }
    !generic || crate::poly::consistent(inputs, targets)
}

/// `select_common_type_from_oids`: the type that all the types can take. A list of only `unknown` gives `text`. When two types are in different categories, the error gives the two types and the index of the second.
pub(crate) fn common_of(tys: &[u32]) -> std::result::Result<u32, (u32, u32, usize)> {
    let Some(&first) = tys.first() else { return Ok(oid::TEXT) };
    if first != oid::UNKNOWN && tys.iter().all(|&t| t == first) {
        return Ok(first);
    }
    let mut ptype = types::base(first);
    let (mut pcategory, mut ppreferred) = types::category_preferred(ptype);
    for (i, &next) in tys.iter().enumerate().skip(1) {
        let ntype = types::base(next);
        if ntype == oid::UNKNOWN || ntype == ptype {
            continue;
        }
        let (ncategory, npreferred) = types::category_preferred(ntype);
        if ptype == oid::UNKNOWN {
            (ptype, pcategory, ppreferred) = (ntype, ncategory, npreferred);
        } else if ncategory != pcategory {
            return Err((ptype, ntype, i));
        } else if !ppreferred
            && can_coerce(&[ptype], &[ntype], Context::Implicit)
            && !can_coerce(&[ntype], &[ptype], Context::Implicit)
        {
            (ptype, pcategory, ppreferred) = (ntype, ncategory, npreferred);
        }
    }
    Ok(if ptype == oid::UNKNOWN { oid::TEXT } else { ptype })
}

/// `find_typmod_coercion_function`: the function that applies a typmod to a value of the type, and whether it applies to each element of an array.
fn typmod_function(ty: u32) -> Option<(u32, bool)> {
    let (ty, array) = match types::element(ty) {
        0 => (ty, false),
        element => (element, true),
    };
    builtin::cast(ty, ty).filter(|cast| cast.func != 0).map(|cast| (cast.func, array))
}

impl Analyzer<'_> {
    /// `coerce_to_target_type`: the expression cast to the type and the typmod, or `None` if the cast does not exist in the context.
    pub(crate) fn coerce_to_target(
        &mut self,
        expr: Expr,
        target: u32,
        typmod: i32,
        context: Context,
        location: Option<usize>,
    ) -> Result<Option<Expr>> {
        if !can_coerce(&[expr.ty], &[target], context) {
            return Ok(None);
        }
        let expr = self.coerce(expr, target, typmod, context, location)?;
        Ok(Some(self.coerce_typmod(expr, target, typmod, context, location)))
    }

    /// `coerce_type`: the expression cast to the type. The caller has checked that the cast exists.
    pub(crate) fn coerce(
        &mut self,
        expr: Expr,
        target: u32,
        typmod: i32,
        context: Context,
        location: Option<usize>,
    ) -> Result<Expr> {
        let input = expr.ty;
        if target == input {
            return Ok(expr);
        }
        if matches!(
            target,
            oid::ANY
                | oid::ANYELEMENT
                | oid::ANYNONARRAY
                | oid::ANYCOMPATIBLE
                | oid::ANYCOMPATIBLENONARRAY
        ) {
            return Ok(expr);
        }
        if types::is_polymorphic(target) && input != oid::UNKNOWN {
            return Ok(expr);
        }
        if input == oid::UNKNOWN {
            if let ExprKind::Const(value) = &expr.kind {
                // The input function takes the typmod only for `interval`. The caller applies any other typmod with a length cast.
                let base = types::base(target);
                let input_typmod = if base == oid::INTERVAL { typmod } else { -1 };
                let value = match value {
                    Value::Text(text) => {
                        let text = text.clone();
                        self.env.input(base, &text, input_typmod).map_err(|e| {
                            match expr.location {
                                Some(at) => e.at(at),
                                None => e,
                            }
                        })?
                    }
                    _ => Value::Null,
                };
                let typmod = if base == oid::INTERVAL { typmod } else { -1 };
                return Ok(Expr::constant(value, target, typmod, expr.location));
            }
            if let ExprKind::Param(n) = expr.kind
                && let Some(ty) = self.param_type(n, target, expr.location)?
            {
                return Ok(Expr {
                    kind: ExprKind::Param(n),
                    ty,
                    typmod: -1,
                    location: expr.location,
                });
            }
        }
        let form = if context == Context::Explicit {
            FuncForm::ExplicitCast
        } else {
            FuncForm::ImplicitCast
        };
        match find_path(target, input, context) {
            Path::None => Err(Error::internal(format!(
                "failed to find conversion function from {} to {}",
                types::name(input),
                types::name(target)
            ))),
            Path::Relabel => Ok(Expr {
                kind: ExprKind::Relabel(Box::new(expr)),
                ty: target,
                typmod: -1,
                location,
            }),
            Path::ViaIo => Ok(Expr {
                kind: ExprKind::CoerceViaIo(Box::new(expr)),
                ty: target,
                typmod: -1,
                location,
            }),
            Path::Func(func) => Ok(build_cast(expr, func, target, typmod, context, form, location)),
            // ArrayCoerceExpr with a relabel of each element: the array keeps its values.
            Path::ArrayCoerce
                if find_path(types::element(target), types::element(input), context)
                    == Path::Relabel =>
            {
                Ok(Expr {
                    kind: ExprKind::Relabel(Box::new(expr)),
                    ty: target,
                    typmod: -1,
                    location,
                })
            }
            Path::ArrayCoerce => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "a cast from {} to {} is not supported yet",
                    types::name(input),
                    types::name(target)
                ),
            )
            .at_opt(location)),
        }
    }

    /// `coerce_type_typmod`: the length cast of a type that has a typmod, or a relabel that shows the typmod.
    pub(crate) fn coerce_typmod(
        &mut self,
        expr: Expr,
        target: u32,
        typmod: i32,
        context: Context,
        location: Option<usize>,
    ) -> Expr {
        if typmod < 0 || expr.typmod == typmod && expr.ty == target {
            return expr;
        }
        match typmod_function(target) {
            Some((func, false)) => {
                let form = if context == Context::Explicit {
                    FuncForm::ExplicitCast
                } else {
                    FuncForm::ImplicitCast
                };
                build_cast(expr, func, target, typmod, context, form, location)
            }
            _ => {
                let mut expr = expr;
                if let ExprKind::Const(_) = expr.kind {
                    expr.typmod = typmod;
                    expr
                } else {
                    Expr { kind: ExprKind::Relabel(Box::new(expr)), ty: target, typmod, location }
                }
            }
        }
    }

    /// `select_common_type`: the type that all the expressions can take, for `CASE`, `COALESCE` and the others that `context` names.
    pub(crate) fn common_type(&self, exprs: &[Expr], context: &str) -> Result<u32> {
        let tys: Vec<u32> = exprs.iter().map(|e| e.ty).collect();
        common_of(&tys).map_err(|(p, n, i)| {
            Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "{context} types {} and {} cannot be matched",
                    types::name(p),
                    types::name(n)
                ),
            )
            .at_opt(exprs[i].place())
        })
    }

    /// `coerce_to_common_type`.
    pub(crate) fn coerce_to_common(
        &mut self,
        expr: Expr,
        target: u32,
        context: &str,
    ) -> Result<Expr> {
        if expr.ty == target {
            return Ok(expr);
        }
        if !can_coerce(&[expr.ty], &[target], Context::Implicit) {
            return Err(Error::new(
                SqlState::CANNOT_COERCE,
                format!(
                    "{context} could not convert type {} to {}",
                    types::name(expr.ty),
                    types::name(target)
                ),
            )
            .at_opt(expr.place()));
        }
        self.coerce(expr, target, -1, Context::Implicit, None)
    }

    /// `coerce_to_boolean`: the condition of `construct`, such as `WHERE` or `AND`, cast to `boolean`.
    pub(crate) fn coerce_to_boolean(&mut self, expr: Expr, construct: &str) -> Result<Expr> {
        self.coerce_to_specific(expr, oid::BOOL, construct)
    }

    /// `coerce_to_specific_type`: the expression as a value of the type, with an assignment cast, for a clause such as `LIMIT`.
    pub(crate) fn coerce_to_specific(
        &mut self,
        expr: Expr,
        target: u32,
        construct: &str,
    ) -> Result<Expr> {
        if expr.ty == target {
            return Ok(expr);
        }
        let ty = expr.ty;
        let place = expr.place();
        match self.coerce_to_target(expr, target, -1, Context::Assignment, None)? {
            Some(expr) => Ok(expr),
            None => Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "argument of {construct} must be type {}, not type {}",
                    types::name(target),
                    types::name(ty)
                ),
            )
            .at_opt(place)),
        }
    }
}

/// `build_coercion_expression` for a cast function. A function of two arguments takes the typmod, and one of three also takes true for an explicit cast.
fn build_cast(
    expr: Expr,
    func: u32,
    target: u32,
    typmod: i32,
    context: Context,
    form: FuncForm,
    location: Option<usize>,
) -> Expr {
    let nargs = builtin::proc_by_oid(func).map_or(1, |p| p.nargs);
    let mut args = vec![expr];
    if nargs >= 2 {
        args.push(Expr::constant(Value::Int4(typmod), oid::INT4, -1, None));
    }
    if nargs >= 3 {
        args.push(Expr::constant(Value::Bool(context == Context::Explicit), oid::BOOL, -1, None));
    }
    // A length cast shows the typmod that it applies, as `exprTypmod` reads it from the second argument.
    let typmod = if nargs >= 2 && typmod_function(target).is_some_and(|(f, _)| f == func) {
        typmod
    } else {
        -1
    };
    Expr {
        kind: ExprKind::Func(Func { oid: func, args, form, variadic: false }),
        ty: target,
        typmod,
        location,
    }
}

/// A place on an error when there is one.
pub(crate) trait AtOpt {
    fn at_opt(self, location: Option<usize>) -> Self;
}

impl AtOpt for Error {
    fn at_opt(self, location: Option<usize>) -> Error {
        match location {
            Some(at) => self.at(at),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(find_path(oid::INT8, oid::INT4, Context::Implicit), Path::Func(481));
        assert_eq!(find_path(oid::INT4, oid::INT8, Context::Implicit), Path::None);
        assert_eq!(find_path(oid::INT4, oid::INT8, Context::Assignment), Path::Func(480));
        assert_eq!(find_path(oid::TEXT, oid::VARCHAR, Context::Implicit), Path::Relabel);
        assert_eq!(find_path(oid::TEXT, oid::INT4, Context::Implicit), Path::None);
        assert_eq!(find_path(oid::TEXT, oid::INT4, Context::Assignment), Path::ViaIo);
        assert_eq!(find_path(oid::INT4, oid::TEXT, Context::Explicit), Path::ViaIo);
        assert_eq!(
            find_path(oid::INT8_ARRAY, oid::INT4_ARRAY, Context::Implicit),
            Path::ArrayCoerce
        );
        assert_eq!(find_path(oid::OID_ARRAY, oid::OIDVECTOR, Context::Implicit), Path::ArrayCoerce);
        assert_eq!(find_path(oid::OIDVECTOR, oid::OID_ARRAY, Context::Explicit), Path::None);
        assert!(can_coerce(&[oid::UNKNOWN, oid::INT2], &[oid::INT4, oid::INT4], Context::Implicit));
        assert!(!can_coerce(&[oid::TEXT], &[oid::INT4], Context::Implicit));
        assert_eq!(typmod_function(oid::VARCHAR).map(|(_, a)| a), Some(false));
        assert_eq!(typmod_function(oid::INT4), None);
    }
}

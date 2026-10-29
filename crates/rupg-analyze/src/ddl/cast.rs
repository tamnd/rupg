//! `CREATE CAST`, as `CreateCast` and `CastCreate` run it.

use rupg_catalog::Cast;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CoercionContext, CreateCastStmt, ObjectType};
use rupg_types::oid;

use super::Definer;
use crate::typename::type_name_text;
use crate::types;

/// `BOOTSTRAP_SUPERUSERID`, the owner of the built-in types.
const BOOTSTRAP_SUPERUSER: u32 = 10;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message.into())
}

impl Definer<'_, '_> {
    /// `CreateCast`: the checks of the two types and of the function, then the cast of `CastCreate`.
    pub(super) fn create_cast(&mut self, stmt: &CreateCastStmt) -> Result<()> {
        let (Some(source_name), Some(target_name)) = (&stmt.sourcetype, &stmt.targettype) else {
            return Err(Error::internal("the types of a cast"));
        };
        // `typenameTypeId` with no parse state gives no position.
        let no_position = |error: Error| error.with_position(None);
        let (source, _) = self.an.type_name(source_name).map_err(no_position)?;
        let (target, _) = self.an.type_name(target_name).map_err(no_position)?;
        let kind = |ty: u32| types::row(ty).map_or(b'b', |row| row.kind);
        let (source_kind, target_kind) = (kind(source), kind(target));
        let wrong = |message: String| Error::new(SqlState::WRONG_OBJECT_TYPE, message);
        if source_kind == b'p' {
            return Err(wrong(format!(
                "source data type {} is a pseudo-type",
                type_name_text(source_name)
            )));
        }
        if target_kind == b'p' {
            return Err(wrong(format!(
                "target data type {} is a pseudo-type",
                type_name_text(target_name)
            )));
        }
        let owner = |ty: u32| self.catalog.type_by_oid(ty).map_or(BOOTSTRAP_SUPERUSER, |t| t.owner);
        if !self.owns(owner(source)) && !self.owns(owner(target)) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "must be owner of type {} or type {}",
                    self.type_text(source),
                    self.type_text(target)
                ),
            ));
        }
        self.check_type_usage(source)?;
        self.check_type_usage(target)?;
        if source_kind == b'd' {
            let message = "cast will be ignored because the source data type is a domain";
            self.warning(SqlState::WRONG_OBJECT_TYPE, message.to_string());
        } else if target_kind == b'd' {
            let message = "cast will be ignored because the target data type is a domain";
            self.warning(SqlState::WRONG_OBJECT_TYPE, message.to_string());
        }
        let method = if stmt.func.is_some() {
            b'f'
        } else if stmt.inout {
            b'i'
        } else {
            b'b'
        };
        let (mut func, mut nargs, mut casts) = (0, 0, [0; 2]);
        if let Some(item) = &stmt.func {
            let found = self
                .find_function(ObjectType::OBJECT_FUNCTION, item, false)?
                .ok_or_else(|| Error::internal("a function that the lookup did not find"))?;
            (func, nargs) = (found.oid, found.argtypes.len());
            if !(1..=3).contains(&nargs) {
                return Err(invalid("cast function must take one to three arguments"));
            }
            let Some(incast) = binary_cast(source, found.argtypes[0]) else {
                return Err(invalid(
                    "argument of cast function must match or be binary-coercible from source data type",
                ));
            };
            if nargs > 1 && found.argtypes[1] != oid::INT4 {
                return Err(invalid("second argument of cast function must be type integer"));
            }
            if nargs > 2 && found.argtypes[2] != oid::BOOL {
                return Err(invalid("third argument of cast function must be type boolean"));
            }
            let (rettype, retset) = match self.catalog.function(func) {
                Some(f) => (f.rettype, f.retset),
                None => builtin::proc_by_oid(func).map_or((0, false), |p| (p.rettype, p.retset)),
            };
            let Some(outcast) = binary_cast(rettype, target) else {
                return Err(invalid(
                    "return data type of cast function must match or be binary-coercible to target data type",
                ));
            };
            if found.kind != b'f' {
                return Err(invalid("cast function must be a normal function"));
            }
            if retset {
                return Err(invalid("cast function must not return a set"));
            }
            casts = [incast, outcast];
        }
        if method == b'b' {
            if !self.is_superuser() {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be superuser to create a cast WITHOUT FUNCTION",
                ));
            }
            let form = |ty: u32| types::row(ty).map(|row| (row.len, row.by_val, row.align));
            if form(source) != form(target) {
                return Err(invalid("source and target data types are not physically compatible"));
            }
            let either = |test: &dyn Fn(u32) -> bool| test(source) || test(target);
            if either(&|ty| kind(ty) == b'c') {
                return Err(invalid("composite data types are not binary-compatible"));
            }
            if either(&|ty| types::element(ty) != 0) {
                return Err(invalid("array data types are not binary-compatible"));
            }
            if either(&|ty| matches!(kind(ty), b'r' | b'm')) {
                return Err(invalid("range data types are not binary-compatible"));
            }
            if either(&|ty| kind(ty) == b'e') {
                return Err(invalid("enum data types are not binary-compatible"));
            }
            if either(&|ty| kind(ty) == b'd') {
                return Err(invalid("domain data types must not be marked binary-compatible"));
            }
        }
        if source == target && nargs < 2 {
            return Err(invalid("source data type and target data type are the same"));
        }
        let context = match stmt.context {
            CoercionContext::COERCION_IMPLICIT => b'i',
            CoercionContext::COERCION_ASSIGNMENT => b'a',
            _ => b'e',
        };
        if builtin::cast(source, target).is_some()
            || self.catalog.casts().any(|c| c.source == source && c.target == target)
        {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!(
                    "cast from type {} to type {} already exists",
                    self.type_text(source),
                    self.type_text(target)
                ),
            ));
        }
        let cast = Cast { oid: 0, source, target, func, context, method };
        let cast = self.catalog.create_cast(cast, casts);
        self.collect(None, rupg_catalog::ObjRef::new(rupg_catalog::PG_CAST, cast));
        Ok(())
    }
}

/// `IsBinaryCoercibleWithCast`: `Some` when the values of `source` are values of `target`, with the OID of the binary cast that makes them so, or 0 when no cast is needed.
fn binary_cast(source: u32, target: u32) -> Option<u32> {
    if !crate::typcache::is_binary_coercible(source, target) {
        return None;
    }
    let cast = builtin::cast(types::base(source), target);
    Some(cast.filter(|c| c.method == b'b' && c.context == b'i').map_or(0, |c| c.oid))
}

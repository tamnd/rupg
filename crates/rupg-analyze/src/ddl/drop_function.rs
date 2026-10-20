//! `DROP FUNCTION`, `DROP PROCEDURE` and `DROP ROUTINE`: the functions of `RemoveObjects`, which `LookupFuncWithArgs` finds by name and by the types of the input arguments, or for a procedure also by the types of all the arguments.

use rupg_catalog::{FIRST_NORMAL_OID, ObjRef, PG_PROC, is_pinned};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{DropStmt, FunctionParameterMode, Node, ObjectType, ObjectWithArgs};

use super::Definer;
use super::comment::not_owner;
use super::drop::builtin_object;
use crate::typename::{names, type_name_text};

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in functions.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// A function that a name can find: a function of the catalog or a built-in function.
pub(super) struct Candidate {
    pub(super) oid: u32,
    pub(super) argtypes: Vec<u32>,
    /// The types of all the arguments, also the output arguments.
    pub(super) allargtypes: Vec<u32>,
    pub(super) owner: u32,
    pub(super) kind: u8,
}

/// The word of the messages of `LookupFuncWithArgs` and `does_not_exist_skipping` for the kind of object.
pub(super) fn routine_word(objtype: ObjectType) -> &'static str {
    match objtype {
        ObjectType::OBJECT_PROCEDURE => "procedure",
        ObjectType::OBJECT_ROUTINE => "routine",
        _ => "function",
    }
}

/// The objtype check of `LookupFuncNameInternal`: `FUNCTION` skips a procedure, and `PROCEDURE` skips all that is not a procedure.
fn kind_matches(objtype: ObjectType, kind: u8) -> bool {
    match objtype {
        ObjectType::OBJECT_FUNCTION => kind != b'p',
        ObjectType::OBJECT_PROCEDURE => kind == b'p',
        _ => true,
    }
}

impl Definer<'_, '_> {
    /// The functions of `RemoveObjects` for `DROP FUNCTION`, `DROP PROCEDURE` and `DROP ROUTINE`, with the checks of `LookupFuncWithArgs` and `check_object_ownership`.
    pub(super) fn find_functions(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let Node::ObjectWithArgs(item) = item else {
                return Err(Error::internal("a function name with arguments"));
            };
            let Some(found) = self.find_function(stmt.removeType, item, stmt.missing_ok)? else {
                continue;
            };
            let name = names(&item.objname).join(".");
            if found.kind == b'a' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is an aggregate function"),
                )
                .with_hint("Use DROP AGGREGATE to drop aggregate functions."));
            }
            if !self.is_superuser() && found.owner != self.user {
                return Err(not_owner(routine_word(stmt.removeType), &name));
            }
            let object = ObjRef::new(PG_PROC, found.oid);
            if found.oid < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// `LookupFuncWithArgs`: the function of the name, or with `missing_ok` `None` and the notice of `does_not_exist_skipping` when it does not exist. A name with no list of arguments must find one function of the kind `objtype`. A list of arguments finds a function or a procedure by the types of its input arguments, and for `PROCEDURE` and `ROUTINE` a list with no argument modes also finds a procedure by the types of all its arguments. Then `FUNCTION` must not find a procedure, and `PROCEDURE` must find one.
    pub(super) fn find_function(
        &mut self,
        objtype: ObjectType,
        item: &ObjectWithArgs,
        missing_ok: bool,
    ) -> Result<Option<Candidate>> {
        let word = routine_word(objtype);
        let parts = names(&item.objname);
        let (schema, name) = match self.an.split_name(&parts, None) {
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return self.skip(error.message().to_string());
            }
            split => split?,
        };
        let mut argtypes = Vec::new();
        let mut written = Vec::new();
        for arg in item.objargs.iter().flatten() {
            let Node::TypeName(arg) = arg else { return Err(Error::internal("a type name")) };
            let text = type_name_text(arg);
            let Some(ty) = self.an.lookup_type_name(arg)? else {
                let message = format!("type \"{text}\" does not exist");
                if missing_ok {
                    return self.skip(message);
                }
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            };
            argtypes.push(ty);
            written.push(text);
        }
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let mut candidates: Vec<Candidate> = Vec::new();
        for &namespace in &path {
            for found in self.functions_in(namespace, name) {
                if !candidates.iter().any(|c| c.argtypes == found.argtypes) {
                    candidates.push(found);
                }
            }
        }
        let parts = parts.join(".");
        let ambiguous = || {
            let error = Error::new(
                SqlState::AMBIGUOUS_FUNCTION,
                format!("{word} name \"{parts}\" is not unique"),
            );
            if item.args_unspecified {
                error.with_hint(format!(
                    "Specify the argument list to select the {word} unambiguously."
                ))
            } else {
                error
            }
        };
        let found = if item.args_unspecified {
            candidates.retain(|c| kind_matches(objtype, c.kind));
            if candidates.len() > 1 {
                return Err(ambiguous());
            }
            candidates.pop()
        } else {
            let by_inputs = candidates.iter().position(|c| c.argtypes == argtypes);
            let modes = item.objfuncargs.iter().flatten().any(|arg| {
                matches!(arg, Node::FunctionParameter(p) if p.mode != FunctionParameterMode::FUNC_PARAM_DEFAULT)
            });
            let by_all =
                if matches!(objtype, ObjectType::OBJECT_PROCEDURE | ObjectType::OBJECT_ROUTINE)
                    && !modes
                {
                    candidates
                        .iter()
                        .position(|c| kind_matches(objtype, c.kind) && c.allargtypes == argtypes)
                } else {
                    None
                };
            match (by_inputs, by_all) {
                (Some(a), Some(b)) if a != b => return Err(ambiguous()),
                (Some(at), _) | (None, Some(at)) => Some(candidates.swap_remove(at)),
                (None, None) => None,
            }
        };
        if let Some(found) = found {
            let signature = || {
                let args: Vec<String> = argtypes.iter().map(|&ty| self.type_text(ty)).collect();
                format!("{parts}({})", args.join(", "))
            };
            if objtype == ObjectType::OBJECT_FUNCTION && found.kind == b'p' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("{} is not a function", signature()),
                ));
            }
            if objtype == ObjectType::OBJECT_PROCEDURE && found.kind != b'p' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("{} is not a procedure", signature()),
                ));
            }
            return Ok(Some(found));
        }
        if missing_ok {
            return self.skip(format!("{word} {parts}({}) does not exist", written.join(",")));
        }
        // `ROUTINE` gives the messages of `FUNCTION`.
        let word = if objtype == ObjectType::OBJECT_PROCEDURE { "procedure" } else { "function" };
        if item.args_unspecified {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("could not find a {word} named \"{parts}\""),
            ));
        }
        let args: Vec<String> = argtypes.iter().map(|&ty| self.type_text(ty)).collect();
        Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("{word} {parts}({}) does not exist", args.join(", ")),
        ))
    }

    /// The functions with the name in the schema: the functions of the catalog, and the built-in functions that the catalog does not have.
    pub(super) fn functions_in(&self, namespace: u32, name: &str) -> Vec<Candidate> {
        let mut found: Vec<Candidate> = self
            .catalog
            .functions()
            .filter(|f| f.namespace == namespace && f.name == name)
            .map(|f| Candidate {
                oid: f.oid,
                argtypes: f.argtypes.clone(),
                allargtypes: if f.allargtypes.is_empty() {
                    f.argtypes.clone()
                } else {
                    f.allargtypes.clone()
                },
                owner: f.owner,
                kind: f.kind,
            })
            .collect();
        for proc in builtin::procs_named(name) {
            if proc.namespace == namespace
                && proc.oid < FIRST_NORMAL_OID
                && self.catalog.function(proc.oid).is_none()
            {
                found.push(Candidate {
                    oid: proc.oid,
                    argtypes: proc.argtypes.to_vec(),
                    allargtypes: proc.allargtypes.unwrap_or(proc.argtypes).to_vec(),
                    owner: BOOTSTRAP_SUPERUSER,
                    kind: proc.kind,
                });
            }
        }
        found
    }

    /// The notice of `does_not_exist_skipping`, for a function that `IF EXISTS` skips.
    fn skip(&mut self, message: String) -> Result<Option<Candidate>> {
        self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
        Ok(None)
    }
}

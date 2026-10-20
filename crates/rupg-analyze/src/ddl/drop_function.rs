//! `DROP FUNCTION`: the functions of `RemoveObjects`, which `LookupFuncWithArgs` finds by name and by the types of the input arguments.

use rupg_catalog::{FIRST_NORMAL_OID, ObjRef, PG_PROC, is_pinned};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{DropStmt, Node, ObjectWithArgs};

use super::Definer;
use super::comment::not_owner;
use super::drop::builtin_object;
use crate::typename::{names, type_name_text};

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in functions.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// A function that a name can find: a function of the catalog or a built-in function.
struct Candidate {
    oid: u32,
    argtypes: Vec<u32>,
    owner: u32,
    kind: u8,
}

impl Definer<'_, '_> {
    /// The functions of `RemoveObjects` for `DROP FUNCTION`, with the checks of `LookupFuncWithArgs` and `check_object_ownership`.
    pub(super) fn find_functions(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let Node::ObjectWithArgs(item) = item else {
                return Err(Error::internal("a function name with arguments"));
            };
            let Some(found) = self.find_function(item, stmt.missing_ok)? else { continue };
            let name = names(&item.objname).join(".");
            if found.kind == b'a' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is an aggregate function"),
                )
                .with_hint("Use DROP AGGREGATE to drop aggregate functions."));
            }
            if !self.is_superuser() && found.owner != self.user {
                return Err(not_owner("function", &name));
            }
            let object = ObjRef::new(PG_PROC, found.oid);
            if found.oid < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// `LookupFuncWithArgs` for `OBJECT_FUNCTION`: the function of the name, or with `missing_ok` `None` and the notice of `does_not_exist_skipping` when it does not exist. A name with no list of arguments must find one function.
    fn find_function(
        &mut self,
        item: &ObjectWithArgs,
        missing_ok: bool,
    ) -> Result<Option<Candidate>> {
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
        if item.args_unspecified {
            if candidates.len() > 1 {
                return Err(Error::new(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("function name \"{parts}\" is not unique"),
                )
                .with_hint("Specify the argument list to select the function unambiguously."));
            }
            if let Some(found) = candidates.pop() {
                return Ok(Some(found));
            }
            if missing_ok {
                return self.skip(format!("function {parts}() does not exist"));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("could not find a function named \"{parts}\""),
            ));
        }
        if let Some(found) = candidates.into_iter().find(|c| c.argtypes == argtypes) {
            return Ok(Some(found));
        }
        if missing_ok {
            return self.skip(format!("function {parts}({}) does not exist", written.join(",")));
        }
        let args: Vec<String> = argtypes.iter().map(|&ty| self.type_text(ty)).collect();
        Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {parts}({}) does not exist", args.join(", ")),
        ))
    }

    /// The functions with the name in the schema: the functions of the catalog, and the built-in functions that the catalog does not have.
    fn functions_in(&self, namespace: u32, name: &str) -> Vec<Candidate> {
        let mut found: Vec<Candidate> = self
            .catalog
            .functions()
            .filter(|f| f.namespace == namespace && f.name == name)
            .map(|f| Candidate {
                oid: f.oid,
                argtypes: f.argtypes.clone(),
                owner: f.owner,
                kind: b'f',
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

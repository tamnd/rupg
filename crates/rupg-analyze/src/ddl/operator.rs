//! `CREATE OPERATOR`, `DROP OPERATOR` and `COMMENT ON OPERATOR`: `DefineOperator` of `operatorcmds.c`, `OperatorCreate` of `pg_operator.c`, and the lookup of `LookupOperWithArgs` of `parse_oper.c`. The operators are found by name as `OpernameGetOprid` finds them, in the catalog and in the built-in rows.

use rupg_catalog::{FIRST_NORMAL_OID, ObjRef, Operator, PG_OPERATOR, is_pinned};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, OperatorRow};
use rupg_sql::nodes::{DefElem, DefineStmt, DropStmt, Node, ObjectWithArgs, TypeName};
use rupg_types::oid::{self, INTERNAL};

use super::Definer;
use super::comment::not_owner;
use super::drop::{builtin_object, quote_identifier};
use super::reloptions::def_boolean;
use super::tsearch::{def_name, qualified_name};
use crate::typename::{names, type_name_text};

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in operators.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// `FirstGenbkiObjectId`: the functions below it are built in for the checks of the estimators.
const FIRST_GENBKI_OID: u32 = 10000;

/// The facts of an operator that the lookups need, of the catalog or built in.
#[derive(Clone, Copy, Debug)]
struct Found {
    oid: u32,
    owner: u32,
    /// True for an operator that is not a shell.
    defined: bool,
}

/// `validOperatorName`.
fn valid_operator_name(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len >= 64 {
        return false;
    }
    if !name.bytes().all(|c| b"~!@#^&|`?+-*/%<>=".contains(&c)) {
        return false;
    }
    if name.contains("/*") || name.contains("--") {
        return false;
    }
    if len > 1
        && name.ends_with(['+', '-'])
        && !name[..len - 1].bytes().any(|c| b"~!@#^&|`?%".contains(&c))
    {
        return false;
    }
    name != "!="
}

/// The error of `OperatorCreate` and `OperatorShellMake` for a name that is not a valid operator name.
fn check_name(name: &str) -> Result<()> {
    if valid_operator_name(name) {
        return Ok(());
    }
    Err(Error::new(SqlState::INVALID_NAME, format!("\"{name}\" is not a valid operator name")))
}

/// An error of `DefineOperator` and `OperatorValidateParams` with `ERRCODE_INVALID_FUNCTION_DEFINITION`.
fn invalid(message: &str) -> Error {
    Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message)
}

/// `OperatorValidateParams`.
fn validate_params(op: &Operator, commutator: bool, negator: bool) -> Result<()> {
    if op.left == 0 || op.right == 0 {
        let checks = [
            (commutator, "only binary operators can have commutators"),
            (op.join != 0, "only binary operators can have join selectivity"),
            (op.can_merge, "only binary operators can merge join"),
            (op.can_hash, "only binary operators can hash"),
        ];
        if let Some((_, message)) = checks.iter().find(|(bad, _)| *bad) {
            return Err(invalid(message));
        }
    }
    if op.result != oid::BOOL {
        let checks = [
            (negator, "only boolean operators can have negators"),
            (op.restrict != 0, "only boolean operators can have restriction selectivity"),
            (op.join != 0, "only boolean operators can have join selectivity"),
            (op.can_merge, "only boolean operators can merge join"),
            (op.can_hash, "only boolean operators can hash"),
        ];
        if let Some((_, message)) = checks.iter().find(|(bad, _)| *bad) {
            return Err(invalid(message));
        }
    }
    Ok(())
}

/// The built-in row of an operator that the catalog does not have.
fn builtin_row(oid: u32) -> Option<&'static OperatorRow> {
    builtin::operator_by_oid(oid).filter(|o| o.oid < FIRST_NORMAL_OID)
}

impl Definer<'_, '_> {
    /// `OperatorGet`: the operator of the schema with the name and the input types.
    fn operator_in(&self, namespace: u32, name: &str, left: u32, right: u32) -> Option<Found> {
        if let Some(o) = self.catalog.operator_by_name(namespace, name, left, right) {
            return Some(Found { oid: o.oid, owner: o.owner, defined: o.code != 0 });
        }
        builtin::operators_named(name)
            .find(|o| {
                o.oid < FIRST_NORMAL_OID
                    && o.namespace == namespace
                    && o.left == left
                    && o.right == right
            })
            .map(|o| Found { oid: o.oid, owner: BOOTSTRAP_SUPERUSER, defined: true })
    }

    /// `OpernameGetOprid`: the operator that a name and the input types give, in the schema of the name or in the first schema of the search path that has it. A schema that does not exist gives `None`.
    fn opername_get_oprid(&self, list: &[&str], left: u32, right: u32) -> Result<Option<Found>> {
        let (schema, name) = match self.an.split_name(list, None) {
            Ok(split) => split,
            Err(error) if error.state() == SqlState::UNDEFINED_SCHEMA => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(match schema {
            Some(namespace) => self.operator_in(namespace, name, left, right),
            None => self.an.path.iter().find_map(|&ns| self.operator_in(ns, name, left, right)),
        })
    }

    /// `op_signature_string`: the left type, the name and the right type.
    fn op_signature(&self, list: &[&str], left: u32, right: u32) -> String {
        let mut out = String::new();
        if left != 0 {
            out.push_str(&self.type_text(left));
            out.push(' ');
        }
        out.push_str(&list.join("."));
        out.push(' ');
        out.push_str(&self.type_text(right));
        out
    }

    /// `defGetTypeName` and `typenameTypeId` for `LEFTARG` and `RIGHTARG`.
    fn operator_arg_type(&mut self, def: &DefElem) -> Result<u32> {
        let name = match &def.arg {
            None => return Err(super::tsearch::no_parameter(def)),
            Some(Node::TypeName(name)) => (**name).clone(),
            Some(Node::String(name)) => {
                TypeName { names: vec![Some(Node::String(name.clone()))], ..TypeName::default() }
            }
            Some(_) => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("argument of {} must be a type name", def_name(def)),
                ));
            }
        };
        let (ty, _) = self.an.type_name(&name).map_err(|e| e.with_position(None))?;
        Ok(ty)
    }

    /// `LookupFuncName` with the exact types of the arguments: the function and its result type.
    fn lookup_func_name(&self, list: &[&str], args: &[u32]) -> Result<Option<(u32, u32)>> {
        let (schema, name) = self.an.split_name(list, None)?;
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let found = path.iter().find_map(|&namespace| {
            self.functions_in(namespace, name).into_iter().find(|c| c.argtypes == args)
        });
        Ok(found.map(|c| {
            let rettype = match self.catalog.function(c.oid) {
                Some(f) => f.rettype,
                None => builtin::proc_by_oid(c.oid).map_or(0, |p| p.rettype),
            };
            (c.oid, rettype)
        }))
    }

    /// `LookupFuncName` with `noError` false.
    fn func_oid(&self, list: &[&str], args: &[u32]) -> Result<(u32, u32)> {
        if let Some(found) = self.lookup_func_name(list, args)? {
            return Ok(found);
        }
        let types: Vec<String> = args.iter().map(|&ty| self.type_text(ty)).collect();
        Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {}({}) does not exist", list.join("."), types.join(", ")),
        ))
    }

    /// The checks of `ValidateRestrictionEstimator` and `ValidateJoinEstimator` after the lookup: the estimator gives `float8`, and only a superuser can give one that is not built in.
    fn check_estimator(&self, list: &[&str], found: (u32, u32), word: &str) -> Result<u32> {
        let (function, rettype) = found;
        if rettype != oid::FLOAT8 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("{word} estimator function {} must return type float8", list.join(".")),
            ));
        }
        if function >= FIRST_GENBKI_OID && !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be superuser to specify a non-built-in {word} estimator function"),
            ));
        }
        Ok(function)
    }

    /// `ValidateRestrictionEstimator`.
    fn restriction_estimator(&self, list: &[&str]) -> Result<u32> {
        let found = self.func_oid(list, &[INTERNAL, oid::OID, INTERNAL, oid::INT4])?;
        self.check_estimator(list, found, "restriction")
    }

    /// `ValidateJoinEstimator`: the estimator has five arguments, or four in the old form.
    fn join_estimator(&self, list: &[&str]) -> Result<u32> {
        let five = [INTERNAL, oid::OID, INTERNAL, oid::INT2, INTERNAL];
        let found =
            match (self.lookup_func_name(list, &five)?, self.lookup_func_name(list, &five[..4])?) {
                (Some(_), Some(_)) => {
                    return Err(Error::new(
                        SqlState::AMBIGUOUS_FUNCTION,
                        format!("join estimator function {} has multiple matches", list.join(".")),
                    ));
                }
                (Some(found), None) | (None, Some(found)) => found,
                (None, None) => self.func_oid(list, &five)?,
            };
        self.check_estimator(list, found, "join")
    }

    /// `DefineOperator` and `OperatorCreate`.
    pub(super) fn create_operator(&mut self, stmt: &DefineStmt) -> Result<()> {
        let list = names(&stmt.defnames);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let mut new = Operator::shell(name, namespace, self.user, 0, 0);
        let (mut left, mut right) = (None, None);
        let mut function = None;
        let (mut commutator, mut negator) = (None, None);
        let (mut restrict, mut join) = (None, None);
        for def in stmt.definition.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            match def_name(def) {
                "leftarg" | "rightarg" => {
                    if matches!(&def.arg, Some(Node::TypeName(t)) if t.setof) {
                        return Err(invalid("SETOF type not allowed for operator argument"));
                    }
                    if def_name(def) == "leftarg" {
                        left = Some(&**def);
                    } else {
                        right = Some(&**def);
                    }
                }
                "function" | "procedure" => function = Some(qualified_name(def)?),
                "commutator" => commutator = Some(qualified_name(def)?),
                "negator" => negator = Some(qualified_name(def)?),
                "restrict" => restrict = Some(qualified_name(def)?),
                "join" => join = Some(qualified_name(def)?),
                "hashes" => new.can_hash = def_boolean("hashes", def.arg.as_ref())?,
                "merges" => new.can_merge = def_boolean("merges", def.arg.as_ref())?,
                "sort1" | "sort2" | "ltcmp" | "gtcmp" => new.can_merge = true,
                other => {
                    let message = format!("operator attribute \"{other}\" not recognized");
                    self.warning(SqlState::SYNTAX_ERROR, message);
                }
            }
        }
        let Some(function) = function else {
            return Err(invalid("operator function must be specified"));
        };
        if let Some(def) = left {
            new.left = self.operator_arg_type(def)?;
        }
        if let Some(def) = right {
            new.right = self.operator_arg_type(def)?;
        }
        if new.left == 0 && new.right == 0 {
            return Err(invalid("operator argument types must be specified"));
        }
        if new.right == 0 {
            return Err(invalid("operator right argument type must be specified")
                .with_detail("Postfix operators are not supported."));
        }
        new.kind = if new.left == 0 { b'l' } else { b'b' };
        let args: Vec<u32> = [new.left, new.right].into_iter().filter(|&ty| ty != 0).collect();
        (new.code, new.result) = self.func_oid(&function, &args)?;
        if let Some(list) = &restrict {
            new.restrict = self.restriction_estimator(list)?;
        }
        if let Some(list) = &join {
            new.join = self.join_estimator(list)?;
        }
        check_name(name)?;
        validate_params(&new, commutator.is_some(), negator.is_some())?;
        let shell = self.operator_in(namespace, name, new.left, new.right);
        if let Some(found) = shell {
            if found.defined {
                return Err(Error::new(
                    SqlState::DUPLICATE_FUNCTION,
                    format!("operator {name} already exists"),
                ));
            }
            if !self.is_superuser() && found.owner != self.user {
                return Err(not_owner("operator", name));
            }
        }
        let base = shell.map(|f| f.oid);
        let mut self_commutator = false;
        if let Some(list) = &commutator {
            match self.other_operator(list, new.right, new.left, &new)? {
                Some(other) => new.commutator = other,
                None => self_commutator = true,
            }
        }
        if let Some(list) = &negator {
            match self.other_operator(list, new.left, new.right, &new)? {
                Some(other) if Some(other) != base => new.negator = other,
                _ => return Err(invalid("operator cannot be its own negator")),
            }
        }
        let oid = match base {
            Some(oid) => {
                self.catalog.replace_operator(oid, new.clone())?;
                oid
            }
            None => self.catalog.create_operator(new.clone()),
        };
        if self_commutator {
            new.commutator = oid;
        }
        self.operator_upd(oid, new.commutator, new.negator)
    }

    /// `get_other_operator`: the commutator or the negator that a name and the input types give. A missing operator with the name and the types of the new operator is the new operator itself, which is `None`. Another missing operator is a new shell.
    fn other_operator(
        &mut self,
        list: &[&str],
        left: u32,
        right: u32,
        new: &Operator,
    ) -> Result<Option<u32>> {
        if let Some(found) = self.opername_get_oprid(list, left, right)? {
            if !self.is_superuser() && found.owner != self.user {
                return Err(not_owner("operator", &list.join(".")));
            }
            return Ok(Some(found.oid));
        }
        let (namespace, name) = self.ts_creation(list)?;
        if name == new.name && namespace == new.namespace && left == new.left && right == new.right
        {
            return Ok(None);
        }
        self.check_create_in(namespace, None)?;
        check_name(name)?;
        Ok(Some(
            self.catalog.create_operator(Operator::shell(name, namespace, self.user, left, right)),
        ))
    }

    /// The name of an operator, for the messages of `OperatorUpd`.
    fn operator_name(&self, oid: u32) -> String {
        match self.catalog.operator(oid) {
            Some(o) => o.name.clone(),
            None => builtin::operator_by_oid(oid)
                .map_or_else(|| oid.to_string(), |o| o.name.to_string()),
        }
    }

    /// `OperatorUpd`: the commutator and the negator of the new operator `base` link back to it.
    fn operator_upd(&mut self, base: u32, commutator: u32, negator: u32) -> Result<()> {
        for (other, word) in [(commutator, "commutator"), (negator, "negator")] {
            if other == 0 {
                continue;
            }
            let (now, user) = match self.catalog.operator(other) {
                Some(o) if word == "commutator" => (o.commutator, true),
                Some(o) => (o.negator, true),
                None => match builtin_row(other) {
                    Some(o) if word == "commutator" => (o.commutator, false),
                    Some(o) => (o.negator, false),
                    None => continue,
                },
            };
            if now == base {
                continue;
            }
            if now != 0 {
                return Err(invalid(&format!(
                    "{word} operator {} is already the {word} of operator {}",
                    self.operator_name(other),
                    self.operator_name(now)
                )));
            }
            if !user {
                return Err(super::not_yet(
                    "a built-in operator as a commutator or a negator",
                    None,
                ));
            }
            self.catalog.alter_operator(other, |o| {
                if word == "commutator" {
                    o.commutator = base;
                } else {
                    o.negator = base;
                }
            })?;
        }
        Ok(())
    }

    /// `LookupOperWithArgs`: the operator of a name with the types of its inputs, where `NONE` is no type. With `missing_ok`, a type that does not exist is no type, and an operator that does not exist is `None`.
    fn lookup_oper_with_args(
        &self,
        item: &ObjectWithArgs,
        missing_ok: bool,
    ) -> Result<Option<Found>> {
        let mut types = [0; 2];
        for (slot, arg) in types.iter_mut().zip(item.objargs.iter()) {
            let Some(Node::TypeName(arg)) = arg else { continue };
            *slot = match self.an.lookup_type_name(arg)? {
                Some(ty) => ty,
                None if missing_ok => 0,
                None => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("type \"{}\" does not exist", type_name_text(arg)),
                    ));
                }
            };
        }
        let [left, right] = types;
        let list = names(&item.objname);
        let found = self.opername_get_oprid(&list, left, right)?;
        if found.is_some() || missing_ok {
            return Ok(found);
        }
        if right == 0 {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "postfix operators are not supported"));
        }
        Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("operator does not exist: {}", self.op_signature(&list, left, right)),
        ))
    }

    /// The schema of an operator.
    fn operator_namespace(&self, oid: u32) -> u32 {
        match self.catalog.operator(oid) {
            Some(o) => o.namespace,
            None => builtin_row(oid).map_or(0, |o| o.namespace),
        }
    }

    /// The operators of `RemoveObjects` for `DROP OPERATOR`, with the notices of `does_not_exist_skipping` and the checks of `check_object_ownership`.
    pub(super) fn find_operators(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let Node::ObjectWithArgs(item) = item else {
                return Err(Error::internal("an operator name with arguments"));
            };
            let list = names(&item.objname);
            let Some(found) = self.lookup_oper_with_args(item, stmt.missing_ok)? else {
                let schema = match list.as_slice() {
                    [schema, _] | [_, schema, _] => Some(*schema),
                    _ => None,
                };
                let missing_type = item.objargs.iter().flatten().find_map(|arg| match arg {
                    Node::TypeName(arg) if matches!(self.an.lookup_type_name(arg), Ok(None)) => {
                        Some(type_name_text(arg))
                    }
                    _ => None,
                });
                let message = match (schema, missing_type) {
                    (Some(schema), _) if self.an.schema(schema).is_none() => {
                        format!("schema \"{schema}\" does not exist, skipping")
                    }
                    (_, Some(ty)) => format!("type \"{ty}\" does not exist, skipping"),
                    _ => format!("operator {} does not exist, skipping", list.join(".")),
                };
                self.notice(SqlState::SUCCESSFUL_COMPLETION, message);
                continue;
            };
            let namespace = self.operator_namespace(found.oid);
            if !self.is_superuser()
                && self.schema_owner(namespace) != Some(self.user)
                && found.owner != self.user
            {
                return Err(not_owner("operator", &list.join(".")));
            }
            let object = ObjRef::new(PG_OPERATOR, found.oid);
            if found.oid < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// An operator for `COMMENT ON OPERATOR`, by `LookupOperWithArgs`, after the check of its owner.
    pub(super) fn comment_operator(&self, object: Option<&Node>) -> Result<ObjRef> {
        let Some(Node::ObjectWithArgs(item)) = object else {
            return Err(Error::internal("an operator name with arguments"));
        };
        let found = self
            .lookup_oper_with_args(item, false)?
            .ok_or_else(|| Error::internal("an operator that the lookup did not find"))?;
        if !self.is_superuser() && found.owner != self.user {
            return Err(not_owner("operator", &names(&item.objname).join(".")));
        }
        if found.oid < FIRST_NORMAL_OID {
            return Err(super::not_yet("COMMENT on a built-in object", None));
        }
        Ok(ObjRef::new(PG_OPERATOR, found.oid))
    }

    /// `getObjectDescription` for an operator: `format_operator`, the name after its schema when the operator is not visible, with the input types, or `NONE` for no type.
    pub(super) fn describe_operator(&self, oid: u32) -> String {
        let (namespace, name, left, right) = match self.catalog.operator(oid) {
            Some(o) => (o.namespace, o.name.as_str(), o.left, o.right),
            None => match builtin_row(oid) {
                Some(o) => (o.namespace, o.name, o.left, o.right),
                None => return format!("operator {oid}"),
            },
        };
        let visible =
            self.an.path.iter().find(|&&ns| self.operator_in(ns, name, left, right).is_some())
                == Some(&namespace);
        let side = |ty: u32| if ty == 0 { "NONE".to_string() } else { self.type_text(ty) };
        let schema = if visible {
            String::new()
        } else {
            format!("{}.", quote_identifier(&self.schema_name(namespace)))
        };
        format!("operator {schema}{name}({},{})", side(left), side(right))
    }
}

//! `CREATE OPERATOR FAMILY`, `CREATE OPERATOR CLASS`, `ALTER OPERATOR FAMILY ... ADD` and `... DROP`, and `DROP OPERATOR CLASS` and `DROP OPERATOR FAMILY`: `DefineOpFamily`, `DefineOpClass` and `AlterOpFamily` of `opclasscmds.c`, with the `amadjustmembers` functions of the index access methods. The families and the classes are found by name as `OpfamilynameGetOpfid` and `OpclassnameGetOpcid` find them, in the catalog and in the built-in rows.
//!
//! rupg cannot change a built-in family, so a class in a built-in family, and `ALTER OPERATOR FAMILY` of a built-in family, give `0A000` after the checks.

use rupg_catalog::{
    AmOp, AmProc, FIRST_NORMAL_OID, MemberLink, ObjRef, OpClass, OpFamily, PG_AMOP, PG_AMPROC,
    PG_OPCLASS, PG_OPFAMILY, PG_PROC, is_pinned,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{
    AlterObjectSchemaStmt, AlterOpFamilyStmt, AlterOwnerStmt, CreateOpClassItem, CreateOpClassStmt,
    CreateOpFamilyStmt, DropStmt, List, Node, OPCLASS_ITEM_FUNCTION, OPCLASS_ITEM_OPERATOR,
    OPCLASS_ITEM_STORAGETYPE, ObjectType, ObjectWithArgs, RenameStmt, TypeName,
};
use rupg_types::oid::{self, INTERNAL};

use super::comment::{name_list, not_owner};
use super::drop::{builtin_object, quote_identifier};
use super::{Definer, PG_TOAST_NAMESPACE, not_yet};
use crate::typename::{names, type_name_text};

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in families and classes.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// `BTREE_AM_OID`.
const BTREE_AM: u32 = 403;
/// `HASH_AM_OID`.
const HASH_AM: u32 = 405;
/// `GIST_AM_OID`.
const GIST_AM: u32 = 783;
/// `GIN_AM_OID`.
const GIN_AM: u32 = 2742;
/// `SPGIST_AM_OID`.
const SPGIST_AM: u32 = 4000;
/// `BRIN_AM_OID`.
const BRIN_AM: u32 = 3580;

/// The facts of the `IndexAmRoutine` of an index access method that the commands read.
#[derive(Debug)]
pub(super) struct Method {
    pub(super) oid: u32,
    pub(super) name: &'static str,
    /// `amstrategies`, or 0 when the operators have no fixed numbers.
    strategies: i32,
    /// `amsupport`.
    support: i32,
    /// `amoptsprocnum`.
    options: i32,
    /// `amcanorder`.
    can_order: bool,
    /// `amcanorderbyop`.
    can_order_by_op: bool,
    /// `amcanhash`.
    can_hash: bool,
    /// `amstorage`.
    storage: bool,
    /// `amcanunique`.
    pub(super) can_unique: bool,
    /// `amcanmulticol`.
    pub(super) can_multi_col: bool,
    /// `amcaninclude`.
    pub(super) can_include: bool,
}

/// The index access methods of `pg_am`, as `bthandler`, `hashhandler`, `gisthandler`, `ginhandler`, `spghandler` and `brinhandler` give them.
static METHODS: [Method; 6] = [
    Method {
        oid: BTREE_AM,
        name: "btree",
        strategies: 5,
        support: 6,
        options: 5,
        can_order: true,
        can_order_by_op: false,
        can_hash: false,
        storage: false,
        can_unique: true,
        can_multi_col: true,
        can_include: true,
    },
    Method {
        oid: HASH_AM,
        name: "hash",
        strategies: 1,
        support: 3,
        options: 3,
        can_order: false,
        can_order_by_op: false,
        can_hash: true,
        storage: false,
        can_unique: false,
        can_multi_col: false,
        can_include: false,
    },
    Method {
        oid: GIST_AM,
        name: "gist",
        strategies: 0,
        support: 12,
        options: 10,
        can_order: false,
        can_order_by_op: true,
        can_hash: false,
        storage: true,
        can_unique: false,
        can_multi_col: true,
        can_include: true,
    },
    Method {
        oid: GIN_AM,
        name: "gin",
        strategies: 0,
        support: 7,
        options: 7,
        can_order: false,
        can_order_by_op: false,
        can_hash: false,
        storage: true,
        can_unique: false,
        can_multi_col: true,
        can_include: false,
    },
    Method {
        oid: SPGIST_AM,
        name: "spgist",
        strategies: 0,
        support: 7,
        options: 7,
        can_order: false,
        can_order_by_op: true,
        can_hash: false,
        storage: true,
        can_unique: false,
        can_multi_col: false,
        can_include: true,
    },
    Method {
        oid: BRIN_AM,
        name: "brin",
        strategies: 0,
        support: 15,
        options: 5,
        can_order: false,
        can_order_by_op: false,
        can_hash: false,
        storage: true,
        can_unique: false,
        can_multi_col: true,
        can_include: false,
    },
];

/// `GetIndexAmRoutineByAmId`: the index access method of the OID.
pub(super) fn method_of(oid: u32) -> Option<&'static Method> {
    METHODS.iter().find(|m| m.oid == oid)
}

/// `get_index_am_oid` and `GetIndexAmRoutineByAmId`: the index access method of the name. The table access method `heap` is not an index access method.
pub(super) fn index_method(name: &str) -> Result<&'static Method> {
    if let Some(method) = METHODS.iter().find(|m| m.name == name) {
        return Ok(method);
    }
    if name == "heap" {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("access method \"{name}\" is not of type INDEX"),
        ));
    }
    Err(Error::new(SqlState::UNDEFINED_OBJECT, format!("access method \"{name}\" does not exist")))
}

/// The name of an access method for `get_am_name`.
fn method_name(oid: u32) -> &'static str {
    METHODS.iter().find(|m| m.oid == oid).map_or("", |m| m.name)
}

/// The word of a family or a class in a message.
fn opcf_word(family: bool) -> &'static str {
    if family { "operator family" } else { "operator class" }
}

/// An error with `ERRCODE_INVALID_OBJECT_DEFINITION`.
fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message.into())
}

/// `OpFamilyMember`: an operator or a support function of a family, with the facts of its dependencies.
#[derive(Debug)]
struct Member {
    is_func: bool,
    /// The OID of the operator or the function.
    object: u32,
    number: i32,
    left: u32,
    right: u32,
    /// The B-tree family of an ordering operator, or 0.
    sortfamily: u32,
    /// `ref_is_hard`.
    hard: bool,
    /// `ref_is_family`.
    of_family: bool,
    /// `refobjid`.
    owner: u32,
    /// The input types of the operator or the function, for `typeDepNeeded`.
    inputs: Vec<u32>,
}

impl Member {
    fn new(is_func: bool, object: u32, number: i32) -> Member {
        Member {
            is_func,
            object,
            number,
            left: 0,
            right: 0,
            sortfamily: 0,
            hard: false,
            of_family: false,
            owner: 0,
            inputs: Vec::new(),
        }
    }

    /// The member depends on the family `family` and is not a part of it.
    fn soft(&mut self, family: u32) {
        self.hard = false;
        self.of_family = true;
        self.owner = family;
    }

    /// The member is a part of the class `class`.
    fn hard(&mut self, class: u32) {
        self.hard = true;
        self.of_family = false;
        self.owner = class;
    }

    fn link(&self) -> MemberLink {
        let class = if self.of_family { PG_OPFAMILY } else { PG_OPCLASS };
        MemberLink {
            hard: self.hard,
            owner: ObjRef::new(class, self.owner),
            inputs: self.inputs.clone(),
        }
    }

    fn number(&self) -> i16 {
        i16::try_from(self.number).unwrap_or(i16::MAX)
    }
}

/// The check of the number of an operator or a function of `DefineOpClass` and `AlterOpFamily`.
fn check_number(word: &str, number: i32, max: i32) -> Result<()> {
    if number <= 0 || number > max {
        return Err(invalid(format!(
            "invalid {word} number {number}, must be between 1 and {max}"
        )));
    }
    Ok(())
}

/// The highest number of an operator of the method: `amstrategies`, or `SHRT_MAX` when it is 0.
fn max_operator(method: &Method) -> i32 {
    if method.strategies <= 0 { i32::from(i16::MAX) } else { method.strategies }
}

/// The item of a list of `CreateOpClassItem` nodes.
fn items(list: &List) -> impl Iterator<Item = &CreateOpClassItem> {
    list.iter().flatten().filter_map(|node| match node {
        Node::CreateOpClassItem(item) => Some(&**item),
        _ => None,
    })
}

impl Definer<'_, '_> {
    /// `addFamilyMember`: a member joins the list, which must not have one with the same number and types.
    fn add_family_member(&self, list: &mut Vec<Member>, member: Member) -> Result<()> {
        if list.iter().any(|old| {
            old.number == member.number && old.left == member.left && old.right == member.right
        }) {
            let word = if member.is_func { "function" } else { "operator" };
            return Err(invalid(format!(
                "{word} number {} for ({},{}) appears more than once",
                member.number,
                self.type_text(member.left),
                self.type_text(member.right)
            )));
        }
        list.push(member);
        Ok(())
    }

    /// The family of the method with the name in the schema, of the catalog or built in.
    fn opfamily_in(&self, method: u32, namespace: u32, name: &str) -> Option<u32> {
        if let Some(f) = self
            .catalog
            .opfamilies()
            .find(|f| f.method == method && f.namespace == namespace && f.name == name)
        {
            return Some(f.oid);
        }
        builtin::named(Named::Opfamily)
            .iter()
            .find(|r| r.method == method && r.namespace == namespace && r.name == name)
            .map(|r| r.oid)
    }

    /// The class of the method with the name in the schema, of the catalog or built in.
    fn opclass_in(&self, method: u32, namespace: u32, name: &str) -> Option<u32> {
        if let Some(c) = self
            .catalog
            .opclasses()
            .find(|c| c.method == method && c.namespace == namespace && c.name == name)
        {
            return Some(c.oid);
        }
        builtin::named(Named::Opclass)
            .iter()
            .find(|r| r.method == method && r.namespace == namespace && r.name == name)
            .map(|r| r.oid)
    }

    /// `OpFamilyCacheLookup` and `OpClassCacheLookup`: the family or the class of the method that a name gives, in the schema of the name or in the first schema of the search path that has it. With `missing_ok`, a name that gives no object, or a schema that does not exist, is `None`.
    pub(super) fn opcf_lookup(
        &self,
        family: bool,
        method: &Method,
        list: &[&str],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        let (schema, name) = match self.an.split_name(list, None) {
            Ok(split) => split,
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let find = |ns: u32| {
            if family {
                self.opfamily_in(method.oid, ns, name)
            } else {
                self.opclass_in(method.oid, ns, name)
            }
        };
        let found = match schema {
            Some(namespace) => find(namespace),
            None => self.an.path.iter().find_map(|&ns| find(ns)),
        };
        if found.is_none() && !missing_ok {
            let word = if family { "operator family" } else { "operator class" };
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "{word} \"{}\" does not exist for access method \"{}\"",
                    list.join("."),
                    method.name
                ),
            ));
        }
        Ok(found)
    }

    /// The error of `index_opclass_options` for options of a class whose family has no options procedure, `amoptsprocnum`, of the access method for the type of the class. The class is named as `generate_opclass_name` names it. `None` when the class has the procedure.
    pub(super) fn opclass_without_options(&self, class: u32) -> Option<Error> {
        let (family, intype) = match self.catalog.opclass(class) {
            Some(c) => (c.family, c.intype),
            None => builtin::opclasses()
                .iter()
                .find(|c| c.oid == class)
                .map(|c| (c.family, c.intype))?,
        };
        let (namespace, name, method, _) = self.opcf_row(false, class)?;
        let method = METHODS.iter().find(|m| m.oid == method)?;
        let number = i16::try_from(method.options).ok().filter(|n| *n != 0);
        let has = number.is_some_and(|number| {
            let is =
                |left: u32, right: u32, n: i16| left == intype && right == intype && n == number;
            builtin::amprocs(family, intype).iter().any(|a| is(a.left, a.right, a.number))
                || self
                    .catalog
                    .amprocs()
                    .any(|a| a.family == family && is(a.left, a.right, a.number))
        });
        if has {
            return None;
        }
        // The build of an index runs with the search path `pg_catalog, pg_temp`, as `RestrictSearchPath` sets it, so only a class of `pg_catalog` is visible.
        let visible = namespace == crate::PG_CATALOG_NAMESPACE;
        let shown = if visible {
            quote_identifier(&name)
        } else {
            format!(
                "{}.{}",
                quote_identifier(&self.schema_name(namespace)),
                quote_identifier(&name)
            )
        };
        Some(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("operator class {shown} has no options"),
        ))
    }

    /// `OpclassnameGetOpcid` and `get_opclass_oid` of `ResolveOpClass`: the class of the access method that the name gives, of the catalog or built in, and the type that it indexes.
    pub(super) fn opclass_by_name(&self, method: u32, list: &[&str]) -> Result<(u32, u32)> {
        let Some(method) = METHODS.iter().find(|m| m.oid == method) else {
            return Err(Error::internal(format!("cache lookup failed for access method {method}")));
        };
        let class = self.opcf_lookup(false, method, list, false)?.unwrap_or_default();
        let intype = match self.catalog.opclass(class) {
            Some(c) => c.intype,
            None => builtin::opclasses().iter().find(|c| c.oid == class).map_or(0, |c| c.intype),
        };
        Ok((class, intype))
    }

    /// `get_opfamily_oid` with `missing_ok` false.
    fn opfamily_oid(&self, method: &Method, list: &List) -> Result<u32> {
        Ok(self.opcf_lookup(true, method, &names(list), false)?.unwrap_or_default())
    }

    /// The schema, the name, the access method and the owner of a family or a class, of the catalog or built in.
    fn opcf_row(&self, family: bool, object: u32) -> Option<(u32, String, u32, u32)> {
        let user = if family {
            self.catalog.opfamily(object).map(|f| (f.namespace, f.name.clone(), f.method, f.owner))
        } else {
            self.catalog.opclass(object).map(|c| (c.namespace, c.name.clone(), c.method, c.owner))
        };
        user.or_else(|| {
            let kind = if family { Named::Opfamily } else { Named::Opclass };
            builtin::named_by_oid(kind, object)
                .map(|r| (r.namespace, r.name.to_string(), r.method, BOOTSTRAP_SUPERUSER))
        })
    }

    /// `typenameTypeId` with no parse state.
    fn type_id(&mut self, name: &TypeName) -> Result<u32> {
        let (ty, _) = self.an.type_name(name).map_err(|e| e.with_position(None))?;
        Ok(ty)
    }

    /// `processTypesSpec`: the left and the right type of a list of one or two types.
    fn process_types_spec(&mut self, args: &List) -> Result<(u32, u32)> {
        let types: Vec<&TypeName> = args
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::TypeName(t) => Some(&**t),
                _ => None,
            })
            .collect();
        let Some(first) = types.first() else {
            return Err(Error::internal("a list of types"));
        };
        let left = self.type_id(first)?;
        let right = match types.get(1) {
            Some(second) => self.type_id(second)?,
            None => left,
        };
        if types.len() > 2 {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "one or two argument types must be specified",
            ));
        }
        Ok((left, right))
    }

    /// `LookupOperName` with the types of the inputs, and `noError` false.
    fn lookup_oper_name(&self, list: &[&str], left: u32, right: u32) -> Result<u32> {
        if let Some(found) = self.opername_get_oprid(list, left, right)? {
            return Ok(found.oid);
        }
        Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("operator does not exist: {}", self.op_signature(list, left, right)),
        ))
    }

    /// The operator of an item: `LookupOperWithArgs` with the types of the item, or `LookupOperName` with the type of the class for both inputs.
    fn item_operator(&self, name: &ObjectWithArgs, ty: u32) -> Result<u32> {
        if name.objargs.is_empty() {
            return self.lookup_oper_name(&names(&name.objname), ty, ty);
        }
        Ok(self.lookup_oper_with_args(name, false)?.map_or(0, |f| f.oid))
    }

    /// The input types and the result type of a function, of the catalog or built in.
    fn function_signature(&self, function: u32) -> Option<(Vec<u32>, u32)> {
        if let Some(f) = self.catalog.function(function) {
            return Some((f.argtypes.clone(), f.rettype));
        }
        builtin::proc_by_oid(function).map(|p| (p.argtypes.to_vec(), p.rettype))
    }

    /// `assignOperTypes`: the checks of an operator of a family, and its types, which are the types of its inputs when the item does not give them.
    fn assign_oper_types(&self, member: &mut Member, method: &Method) -> Result<()> {
        let op = self.operator_row(member.object).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for operator {}", member.object))
        })?;
        if op.kind != b'b' {
            return Err(invalid("index operators must be binary"));
        }
        if member.sortfamily != 0 {
            if !method.can_order_by_op {
                return Err(invalid(format!(
                    "access method \"{}\" does not support ordering operators",
                    method.name
                )));
            }
        } else if op.result != oid::BOOL {
            return Err(invalid("index search operators must return boolean"));
        }
        if member.left == 0 {
            member.left = op.left;
        }
        if member.right == 0 {
            member.right = op.right;
        }
        member.inputs = vec![op.left, op.right];
        Ok(())
    }

    /// `assignProcTypes`: the checks of a support function of a family, as the access method needs them, and its types. `ty` is the type of the class, or 0 for `ALTER OPERATOR FAMILY`.
    fn assign_proc_types(&self, member: &mut Member, method: &Method, ty: u32) -> Result<()> {
        let (args, rettype) = self.function_signature(member.object).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", member.object))
        })?;
        let arg = |i: usize| args.get(i).copied().unwrap_or_default();
        if member.number == method.options {
            if ty != 0 {
                if (member.left != 0 && member.left != ty)
                    || (member.right != 0 && member.right != ty)
                {
                    return Err(invalid(
                        "associated data types for operator class options parsing functions must match opclass input type",
                    ));
                }
            } else if member.left != member.right {
                return Err(invalid(
                    "left and right associated data types for operator class options parsing functions must match",
                ));
            }
            if rettype != oid::VOID || args.len() != 1 || arg(0) != INTERNAL {
                return Err(invalid("invalid operator class options parsing function").with_hint(
                    "Valid signature of operator class options parsing function is (internal) RETURNS void.",
                ));
            }
        } else if method.can_order {
            match member.number {
                1 => {
                    if args.len() != 2 {
                        return Err(invalid(
                            "ordering comparison functions must have two arguments",
                        ));
                    }
                    if rettype != oid::INT4 {
                        return Err(invalid("ordering comparison functions must return integer"));
                    }
                    if member.left == 0 {
                        member.left = arg(0);
                    }
                    if member.right == 0 {
                        member.right = arg(1);
                    }
                }
                2 => {
                    if args.len() != 1 || arg(0) != INTERNAL {
                        return Err(invalid(
                            "ordering sort support functions must accept type \"internal\"",
                        ));
                    }
                    if rettype != oid::VOID {
                        return Err(invalid("ordering sort support functions must return void"));
                    }
                }
                3 => {
                    if args.len() != 5 {
                        return Err(invalid(
                            "ordering in_range functions must have five arguments",
                        ));
                    }
                    if rettype != oid::BOOL {
                        return Err(invalid("ordering in_range functions must return boolean"));
                    }
                    if member.left == 0 {
                        member.left = arg(0);
                    }
                    if member.right == 0 {
                        member.right = arg(2);
                    }
                }
                4 => {
                    if args.len() != 1 {
                        return Err(invalid(
                            "ordering equal image functions must have one argument",
                        ));
                    }
                    if rettype != oid::BOOL {
                        return Err(invalid("ordering equal image functions must return boolean"));
                    }
                    if member.left != member.right {
                        return Err(invalid(
                            "ordering equal image functions must not be cross-type",
                        ));
                    }
                }
                6 => {
                    if args.len() != 1 || arg(0) != INTERNAL {
                        return Err(invalid(
                            "btree skip support functions must accept type \"internal\"",
                        ));
                    }
                    if rettype != oid::VOID {
                        return Err(invalid("btree skip support functions must return void"));
                    }
                    if member.left != member.right {
                        return Err(invalid("btree skip support functions must not be cross-type"));
                    }
                }
                _ => {}
            }
        } else if method.can_hash {
            if member.number == 1 {
                if args.len() != 1 {
                    return Err(invalid("hash function 1 must have one argument"));
                }
                if rettype != oid::INT4 {
                    return Err(invalid("hash function 1 must return integer"));
                }
            } else if member.number == 2 {
                if args.len() != 2 {
                    return Err(invalid("hash function 2 must have two arguments"));
                }
                if rettype != oid::INT8 {
                    return Err(invalid("hash function 2 must return bigint"));
                }
            }
            if member.left == 0 {
                member.left = arg(0);
            }
            if member.right == 0 {
                member.right = arg(0);
            }
        }
        if member.left == 0 {
            member.left = ty;
        }
        if member.right == 0 {
            member.right = ty;
        }
        if member.left == 0 || member.right == 0 {
            return Err(invalid(
                "associated data types must be specified for index support function",
            ));
        }
        member.inputs = args;
        Ok(())
    }

    /// `opclass_for_family_datatype`: the class of the method in the family for the type, or 0.
    fn opclass_for_family_datatype(&self, method: u32, family: u32, ty: u32) -> u32 {
        self.catalog
            .opclasses()
            .find(|c| c.method == method && c.family == family && c.intype == ty)
            .map(|c| c.oid)
            .or_else(|| {
                builtin::opclasses()
                    .iter()
                    .find(|c| c.method == method && c.family == family && c.intype == ty)
                    .map(|c| c.oid)
            })
            .unwrap_or_default()
    }

    /// The `amadjustmembers` function of the method: `btadjustmembers`, `hashadjustmembers`, `gistadjustmembers`, `ginadjustmembers` or `spgadjustmembers`. `class` is the new class, or 0 for `ALTER OPERATOR FAMILY`.
    fn adjust_members(
        &self,
        method: &Method,
        family: u32,
        class: u32,
        operators: &mut [Member],
        functions: &mut [Member],
    ) {
        match method.oid {
            BTREE_AM | HASH_AM => {
                let mut intype = self.catalog.opclass(class).map_or(0, |c| c.intype);
                let mut class = class;
                for member in operators.iter_mut().chain(functions.iter_mut()) {
                    if (member.is_func && member.number != 1) || member.left != member.right {
                        member.soft(family);
                        continue;
                    }
                    if member.left != intype {
                        intype = member.left;
                        class = self.opclass_for_family_datatype(method.oid, family, intype);
                    }
                    if class != 0 {
                        member.hard(class);
                    } else {
                        member.soft(family);
                    }
                }
            }
            GIST_AM | GIN_AM | SPGIST_AM => {
                let hard: &[i32] = match method.oid {
                    GIST_AM => &[1, 2, 5, 6, 7],
                    GIN_AM => &[2, 3],
                    _ => &[1, 2, 3, 4, 5],
                };
                for member in operators.iter_mut() {
                    member.soft(family);
                }
                for member in functions.iter_mut() {
                    if hard.contains(&member.number) {
                        member.hard = true;
                    } else {
                        member.soft(family);
                    }
                }
            }
            _ => {}
        }
    }

    /// `storeOperators` and `storeProcedures`: the rows of `pg_amop` and `pg_amproc` of the members, with their dependencies. With `is_add`, a member that the family has gives `42710`.
    fn store_members(
        &mut self,
        list: &List,
        method: &Method,
        family: u32,
        operators: &[Member],
        functions: &[Member],
        is_add: bool,
    ) -> Result<()> {
        let shown = names(list).join(".");
        for op in operators {
            let strategy = op.number();
            if is_add
                && self.catalog.amops().any(|a| {
                    a.family == family
                        && a.left == op.left
                        && a.right == op.right
                        && a.strategy == strategy
                })
            {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "operator {}({},{}) already exists in operator family \"{shown}\"",
                        op.number,
                        self.type_text(op.left),
                        self.type_text(op.right)
                    ),
                ));
            }
            let row = AmOp {
                oid: 0,
                family,
                left: op.left,
                right: op.right,
                strategy,
                purpose: if op.sortfamily != 0 { b'o' } else { b's' },
                operator: op.object,
                method: method.oid,
                sortfamily: op.sortfamily,
            };
            self.catalog.add_amop(row, &op.link());
        }
        for proc in functions {
            let number = proc.number();
            if is_add
                && self.catalog.amprocs().any(|a| {
                    a.family == family
                        && a.left == proc.left
                        && a.right == proc.right
                        && a.number == number
                })
            {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "function {}({},{}) already exists in operator family \"{shown}\"",
                        proc.number,
                        self.type_text(proc.left),
                        self.type_text(proc.right)
                    ),
                ));
            }
            let row = AmProc {
                oid: 0,
                family,
                left: proc.left,
                right: proc.right,
                number,
                function: proc.object,
            };
            self.catalog.add_amproc(row, &proc.link());
        }
        Ok(())
    }

    /// `CreateOpFamily`: a new family, after the check that the schema has no family of the method with the name.
    fn new_opfamily(
        &mut self,
        method: &Method,
        namespace: u32,
        name: &str,
        amname: &str,
    ) -> Result<u32> {
        if self.opfamily_in(method.oid, namespace, name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("operator family \"{name}\" for access method \"{amname}\" already exists"),
            ));
        }
        Ok(self.catalog.create_opfamily(OpFamily {
            oid: 0,
            method: method.oid,
            name: name.to_string(),
            namespace,
            owner: self.user,
        }))
    }

    /// `DefineOpFamily`.
    pub(super) fn create_opfamily(&mut self, stmt: &CreateOpFamilyStmt) -> Result<()> {
        let list = names(&stmt.opfamilyname);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let amname = stmt.amname.as_deref().unwrap_or_default();
        let method = index_method(amname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create an operator family",
            ));
        }
        self.new_opfamily(method, namespace, name, amname)?;
        Ok(())
    }

    /// `DefineOpClass`: the class, with a new family of its name when it names no family and its schema has no family of the method with its name, and with its operators and its support functions.
    pub(super) fn create_opclass(&mut self, stmt: &CreateOpClassStmt) -> Result<()> {
        let list = names(&stmt.opclassname);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let amname = stmt.amname.as_deref().unwrap_or_default();
        let method = index_method(amname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create an operator class",
            ));
        }
        let datatype =
            stmt.datatype.as_deref().ok_or_else(|| Error::internal("the type of the class"))?;
        let ty = self.type_id(datatype)?;
        let family = if !stmt.opfamilyname.is_empty() {
            self.opfamily_oid(method, &stmt.opfamilyname)?
        } else if let Some(family) = self.opfamily_in(method.oid, namespace, name) {
            family
        } else {
            self.new_opfamily(method, namespace, name, amname)?
        };
        let mut operators = Vec::new();
        let mut functions = Vec::new();
        let mut storage = 0;
        for item in items(&stmt.items) {
            match item.itemtype {
                OPCLASS_ITEM_OPERATOR => {
                    check_number("operator", item.number, max_operator(method))?;
                    let op_name =
                        item.name.as_deref().ok_or_else(|| Error::internal("an operator"))?;
                    let operator = self.item_operator(op_name, ty)?;
                    let sortfamily = if item.order_family.is_empty() {
                        0
                    } else {
                        self.opfamily_oid(&METHODS[0], &item.order_family)?
                    };
                    let mut member = Member::new(false, operator, item.number);
                    member.sortfamily = sortfamily;
                    self.assign_oper_types(&mut member, method)?;
                    self.add_family_member(&mut operators, member)?;
                }
                OPCLASS_ITEM_FUNCTION => {
                    check_number("function", item.number, method.support)?;
                    let function = self.item_function(item)?;
                    let mut member = Member::new(true, function, item.number);
                    if !item.class_args.is_empty() {
                        (member.left, member.right) = self.process_types_spec(&item.class_args)?;
                    }
                    self.assign_proc_types(&mut member, method, ty)?;
                    self.add_family_member(&mut functions, member)?;
                }
                OPCLASS_ITEM_STORAGETYPE => {
                    if storage != 0 {
                        return Err(invalid("storage type specified more than once"));
                    }
                    let stored = item
                        .storedtype
                        .as_deref()
                        .ok_or_else(|| Error::internal("a storage type"))?;
                    storage = self.type_id(stored)?;
                }
                other => return Err(Error::internal(format!("unrecognized item type: {other}"))),
            }
        }
        if storage == ty {
            storage = 0;
        } else if storage != 0 && !method.storage {
            return Err(invalid(format!(
                "storage type cannot be different from data type for access method \"{amname}\""
            )));
        }
        if self.opclass_in(method.oid, namespace, name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("operator class \"{name}\" for access method \"{amname}\" already exists"),
            ));
        }
        if stmt.isDefault {
            let user = self
                .catalog
                .opclasses()
                .find(|c| c.method == method.oid && c.intype == ty && c.default)
                .map(|c| c.name.clone());
            let other = user.or_else(|| {
                builtin::opclasses()
                    .iter()
                    .find(|c| c.method == method.oid && c.intype == ty && c.default)
                    .map(|c| c.name.to_string())
            });
            if let Some(other) = other {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "could not make operator class \"{name}\" be default for type {}",
                        type_name_text(datatype)
                    ),
                )
                .with_detail(format!("Operator class \"{other}\" already is the default.")));
            }
        }
        if family < FIRST_NORMAL_OID {
            return Err(not_yet("an operator class in a built-in operator family", None));
        }
        let class = self.catalog.create_opclass(OpClass {
            oid: 0,
            method: method.oid,
            name: name.to_string(),
            namespace,
            owner: self.user,
            family,
            intype: ty,
            default: stmt.isDefault,
            keytype: storage,
        });
        for member in operators.iter_mut().chain(functions.iter_mut()) {
            member.hard(class);
        }
        self.adjust_members(method, family, class, &mut operators, &mut functions);
        self.store_members(&stmt.opfamilyname, method, family, &operators, &functions, false)?;
        self.catalog.opclass_depends(class);
        Ok(())
    }

    /// The function of an item: `LookupFuncWithArgs` for `OBJECT_FUNCTION`.
    fn item_function(&mut self, item: &CreateOpClassItem) -> Result<u32> {
        let name = item.name.as_deref().ok_or_else(|| Error::internal("a function"))?;
        let found = self.find_function(ObjectType::OBJECT_FUNCTION, name, false)?;
        Ok(found.map_or(0, |c| c.oid))
    }

    /// `AlterOpFamily`: `ALTER OPERATOR FAMILY ... ADD` and `... DROP`.
    pub(super) fn alter_opfamily(&mut self, stmt: &AlterOpFamilyStmt) -> Result<()> {
        let amname = stmt.amname.as_deref().unwrap_or_default();
        let method = index_method(amname)?;
        let family = self.opfamily_oid(method, &stmt.opfamilyname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to alter an operator family",
            ));
        }
        if stmt.isDrop {
            self.alter_opfamily_drop(stmt, method, family)
        } else {
            self.alter_opfamily_add(stmt, method, family)
        }
    }

    /// `AlterOpFamilyAdd`: the new members depend on the family and are not a part of it, unless the access method makes them a part of a class.
    fn alter_opfamily_add(
        &mut self,
        stmt: &AlterOpFamilyStmt,
        method: &Method,
        family: u32,
    ) -> Result<()> {
        let mut operators = Vec::new();
        let mut functions = Vec::new();
        for item in items(&stmt.items) {
            match item.itemtype {
                OPCLASS_ITEM_OPERATOR => {
                    check_number("operator", item.number, max_operator(method))?;
                    let op_name =
                        item.name.as_deref().ok_or_else(|| Error::internal("an operator"))?;
                    if op_name.objargs.is_empty() {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            "operator argument types must be specified in ALTER OPERATOR FAMILY",
                        ));
                    }
                    let operator = self.item_operator(op_name, 0)?;
                    let sortfamily = if item.order_family.is_empty() {
                        0
                    } else {
                        self.opfamily_oid(&METHODS[0], &item.order_family)?
                    };
                    let mut member = Member::new(false, operator, item.number);
                    member.sortfamily = sortfamily;
                    member.soft(family);
                    self.assign_oper_types(&mut member, method)?;
                    self.add_family_member(&mut operators, member)?;
                }
                OPCLASS_ITEM_FUNCTION => {
                    check_number("function", item.number, method.support)?;
                    let function = self.item_function(item)?;
                    let mut member = Member::new(true, function, item.number);
                    member.soft(family);
                    if !item.class_args.is_empty() {
                        (member.left, member.right) = self.process_types_spec(&item.class_args)?;
                    }
                    self.assign_proc_types(&mut member, method, 0)?;
                    self.add_family_member(&mut functions, member)?;
                }
                OPCLASS_ITEM_STORAGETYPE => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "STORAGE cannot be specified in ALTER OPERATOR FAMILY",
                    ));
                }
                other => return Err(Error::internal(format!("unrecognized item type: {other}"))),
            }
        }
        self.adjust_members(method, family, 0, &mut operators, &mut functions);
        if family < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER OPERATOR FAMILY of a built-in operator family", None));
        }
        self.store_members(&stmt.opfamilyname, method, family, &operators, &functions, true)
    }

    /// `AlterOpFamilyDrop`: `dropOperators` and `dropProcedures` remove each member as `performDeletion` with `DROP_RESTRICT` does.
    fn alter_opfamily_drop(
        &mut self,
        stmt: &AlterOpFamilyStmt,
        method: &Method,
        family: u32,
    ) -> Result<()> {
        let mut operators = Vec::new();
        let mut functions = Vec::new();
        for item in items(&stmt.items) {
            let is_func = match item.itemtype {
                OPCLASS_ITEM_OPERATOR => {
                    check_number("operator", item.number, max_operator(method))?;
                    false
                }
                OPCLASS_ITEM_FUNCTION => {
                    check_number("function", item.number, method.support)?;
                    true
                }
                other => return Err(Error::internal(format!("unrecognized item type: {other}"))),
            };
            let (left, right) = self.process_types_spec(&item.class_args)?;
            let mut member = Member::new(is_func, 0, item.number);
            (member.left, member.right) = (left, right);
            let list = if is_func { &mut functions } else { &mut operators };
            self.add_family_member(list, member)?;
        }
        if family < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER OPERATOR FAMILY of a built-in operator family", None));
        }
        let shown = names(&stmt.opfamilyname).join(".");
        for member in operators.iter().chain(functions.iter()) {
            let number = member.number();
            let found = if member.is_func {
                self.catalog
                    .amprocs()
                    .find(|a| {
                        a.family == family
                            && a.left == member.left
                            && a.right == member.right
                            && a.number == number
                    })
                    .map(|a| ObjRef::new(PG_AMPROC, a.oid))
            } else {
                self.catalog
                    .amops()
                    .find(|a| {
                        a.family == family
                            && a.left == member.left
                            && a.right == member.right
                            && a.strategy == number
                    })
                    .map(|a| ObjRef::new(PG_AMOP, a.oid))
            };
            let Some(object) = found else {
                let word = if member.is_func { "function" } else { "operator" };
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "{word} {}({},{}) does not exist in operator family \"{shown}\"",
                        member.number,
                        self.type_text(member.left),
                        self.type_text(member.right)
                    ),
                ));
            };
            self.perform(&[object], false)?;
        }
        Ok(())
    }

    /// The families or the classes of `RemoveObjects` for `DROP OPERATOR FAMILY` and `DROP OPERATOR CLASS`, with the notices of `does_not_exist_skipping` and the checks of `check_object_ownership`. The owner of the schema of an object can also remove it.
    pub(super) fn find_opcf(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let family = stmt.removeType == ObjectType::OBJECT_OPFAMILY;
        let word = if family { "operator family" } else { "operator class" };
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let list = name_list(Some(item));
            let Some((amname, name)) = list.split_first() else {
                return Err(Error::internal("an access method and a name"));
            };
            let method = index_method(amname)?;
            let Some(found) = self.opcf_lookup(family, method, name, stmt.missing_ok)? else {
                let schema = match name {
                    [schema, _] | [_, schema, _] => Some(*schema),
                    _ => None,
                };
                let message = match schema {
                    Some(schema) if self.an.schema(schema).is_none() => {
                        format!("schema \"{schema}\" does not exist, skipping")
                    }
                    _ => format!(
                        "{word} \"{}\" does not exist for access method \"{amname}\", skipping",
                        name.join(".")
                    ),
                };
                self.notice(SqlState::SUCCESSFUL_COMPLETION, message);
                continue;
            };
            let (namespace, _, _, owner) = self.opcf_row(family, found).unwrap_or_default();
            if self.schema_owner(namespace).is_none_or(|o| !self.owns(o)) && !self.owns(owner) {
                return Err(not_owner(word, &list.join(".")));
            }
            let class = if family { PG_OPFAMILY } else { PG_OPCLASS };
            let object = ObjRef::new(class, found);
            if found < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// `get_object_address_opcf` for `ALTER OPERATOR FAMILY` and `ALTER OPERATOR CLASS`: the family or the class of a name list that starts with the name of the access method, with the name, the schema, the access method and the owner of the object.
    fn altered_opcf(
        &self,
        family: bool,
        object: Option<&Node>,
    ) -> Result<(u32, String, u32, &'static Method, u32)> {
        let list = name_list(object);
        let Some((amname, name)) = list.split_first() else {
            return Err(Error::internal("an access method and a name"));
        };
        let method = index_method(amname)?;
        let oid = self.opcf_lookup(family, method, name, false)?.unwrap_or_default();
        let (namespace, name, _, owner) = self.opcf_row(family, oid).unwrap_or_default();
        Ok((oid, name, namespace, method, owner))
    }

    /// `IsThereOpFamilyInNamespace` and `IsThereOpClassInNamespace`: an error when the schema has a family or a class of the access method with the name.
    fn check_opcf_free(
        &self,
        family: bool,
        name: &str,
        method: &Method,
        namespace: u32,
    ) -> Result<()> {
        let taken = if family {
            self.opfamily_in(method.oid, namespace, name).is_some()
        } else {
            self.opclass_in(method.oid, namespace, name).is_some()
        };
        if !taken {
            return Ok(());
        }
        let word = if family { "operator family" } else { "operator class" };
        Err(Error::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "{word} \"{name}\" for access method \"{}\" already exists in schema \"{}\"",
                method.name,
                self.schema_name(namespace)
            ),
        ))
    }

    /// `AlterObjectRename_internal` for `ALTER OPERATOR FAMILY ... RENAME TO` and `ALTER OPERATOR CLASS ... RENAME TO`. A user who is not a superuser must own the object and be able to create in its schema.
    pub(super) fn rename_opcf(&mut self, stmt: &RenameStmt) -> Result<()> {
        let family = stmt.renameType == ObjectType::OBJECT_OPFAMILY;
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let (oid, name, namespace, method, owner) =
            self.altered_opcf(family, stmt.object.as_ref())?;
        if !self.is_superuser() {
            if !self.owns(owner) {
                return Err(not_owner(opcf_word(family), &name));
            }
            self.check_create_in(namespace, None)?;
        }
        self.check_opcf_free(family, newname, method, namespace)?;
        if oid < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER of a built-in operator class or operator family", None));
        }
        if family {
            self.catalog.alter_opfamily(oid, |f| f.name = newname.to_string());
        } else {
            self.catalog.alter_opclass(oid, |c| c.name = newname.to_string());
        }
        Ok(())
    }

    /// `AlterObjectOwner_internal` for `ALTER OPERATOR FAMILY ... OWNER TO` and `ALTER OPERATOR CLASS ... OWNER TO`. A user who is not a superuser must own the object and be able to become the new owner, and the new owner must be able to create in the schema of the object.
    pub(super) fn alter_opcf_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let family = stmt.objectType == ObjectType::OBJECT_OPFAMILY;
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let (oid, name, namespace, _, owner) = self.altered_opcf(family, stmt.object.as_ref())?;
        if owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if !self.owns(owner) {
                return Err(not_owner(opcf_word(family), &name));
            }
            self.check_can_set_role(new)?;
            if !self.may_create_in(namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(namespace)),
                ));
            }
        }
        if oid < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER of a built-in operator class or operator family", None));
        }
        if family {
            self.catalog.alter_opfamily(oid, |f| f.owner = new);
        } else {
            self.catalog.alter_opclass(oid, |c| c.owner = new);
        }
        Ok(())
    }

    /// `ExecAlterObjectSchemaStmt` and `AlterObjectNamespace_internal` for `ALTER OPERATOR FAMILY ... SET SCHEMA` and `ALTER OPERATOR CLASS ... SET SCHEMA`: `LookupCreationNamespace` and `CheckSetNamespace` for the new schema, the checks of the owner and of the name, and then the move. An object that is in the schema already needs no change.
    pub(super) fn alter_opcf_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let family = stmt.objectType == ObjectType::OBJECT_OPFAMILY;
        let (oid, name, old, method, owner) = self.altered_opcf(family, stmt.object.as_ref())?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        if schema == "pg_temp" {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        let namespace = self.an.schema(schema).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        })?;
        self.check_create_in(namespace, None)?;
        if old == namespace {
            return Ok(());
        }
        if namespace == PG_TOAST_NAMESPACE || old == PG_TOAST_NAMESPACE {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner(opcf_word(family), &name));
        }
        self.check_opcf_free(family, &name, method, namespace)?;
        if oid < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER of a built-in operator class or operator family", None));
        }
        self.catalog.set_opcf_namespace(family, oid, namespace);
        Ok(())
    }

    /// `getObjectDescription` for a family, a class, or a member of a family.
    pub(super) fn describe_opcf(&self, object: ObjRef) -> String {
        match object.class {
            PG_OPCLASS => match self.opcf_row(false, object.oid) {
                Some(_) => format!("operator class {}", self.opcf_text(false, object.oid)),
                None => format!("operator class {}", object.oid),
            },
            PG_OPFAMILY => self.opfamily_description(object.oid),
            PG_AMOP => match self.catalog.amop(object.oid) {
                Some(a) => format!(
                    "operator {} ({}, {}) of {}: {}",
                    a.strategy,
                    self.type_text(a.left),
                    self.type_text(a.right),
                    self.opfamily_description(a.family),
                    self.describe_operator(a.operator).trim_start_matches("operator ")
                ),
                None => format!("operator of access method {}", object.oid),
            },
            _ => match self.catalog.amproc(object.oid) {
                Some(a) => format!(
                    "function {} ({}, {}) of {}: {}",
                    a.number,
                    self.type_text(a.left),
                    self.type_text(a.right),
                    self.opfamily_description(a.family),
                    self.describe(ObjRef::new(PG_PROC, a.function)).trim_start_matches("function ")
                ),
                None => format!("function of access method {}", object.oid),
            },
        }
    }

    /// `getOpFamilyDescription`.
    pub(super) fn opfamily_description(&self, family: u32) -> String {
        format!("operator family {}", self.opcf_text(true, family))
    }

    /// The name of a family or a class, after its schema when it is not visible, and its access method.
    fn opcf_text(&self, family: bool, object: u32) -> String {
        let Some((namespace, name, method, _)) = self.opcf_row(family, object) else {
            return object.to_string();
        };
        let find = |ns: u32| {
            if family {
                self.opfamily_in(method, ns, &name)
            } else {
                self.opclass_in(method, ns, &name)
            }
        };
        let visible = self.an.path.iter().find_map(|&ns| find(ns)) == Some(object);
        let shown = if visible {
            quote_identifier(&name)
        } else {
            format!(
                "{}.{}",
                quote_identifier(&self.schema_name(namespace)),
                quote_identifier(&name)
            )
        };
        format!("{shown} for access method {}", method_name(method))
    }
}

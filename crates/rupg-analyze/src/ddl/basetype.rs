//! `CREATE TYPE` of a base type and of a shell type, as `DefineType` runs it.

use rupg_catalog::{BaseType, ObjRef, PG_TYPE};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{DefElem, DefineStmt, Node};
use rupg_types::oid::{self, CSTRING};

use super::aggregate::def_type_name;
use super::reloptions::def_boolean;
use super::sequence::conflicting;
use super::tsearch::{def_name, def_string, qualified_name};
use super::{Definer, Message};
use crate::coerce::AtOpt;
use crate::typename::{names, place, type_name_text};

/// `DEFAULT_COLLATION_OID`.
const DEFAULT_COLLATION: u32 = 100;
/// `F_RAW_ARRAY_SUBSCRIPT_HANDLER`, the subscripting function of a fixed-length type with an element type and no subscripting function of its own.
const RAW_ARRAY_SUBSCRIPT_HANDLER: u32 = 6180;

/// The parameters of `DefineType`, in the order of its checks.
const PARAMETERS: [&str; 19] = [
    "like",
    "internallength",
    "input",
    "output",
    "receive",
    "send",
    "typmod_in",
    "typmod_out",
    "analyze",
    "subscript",
    "category",
    "preferred",
    "delimiter",
    "element",
    "default",
    "passedbyvalue",
    "alignment",
    "storage",
    "collatable",
];

fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message.into())
}

fn invalid_value(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `defGetTypeLength`: a positive length or `variable`, which gives -1.
fn type_length(def: &DefElem) -> Result<i16> {
    let bad = |text: String| {
        Error::new(
            SqlState::SYNTAX_ERROR,
            format!("invalid argument for {}: \"{text}\"", def_name(def)),
        )
    };
    match &def.arg {
        Some(Node::Integer(n)) => i16::try_from(*n).map_err(|_| bad(n.to_string())),
        Some(Node::Float(text)) => Err(bad(text.to_string())),
        Some(Node::TypeName(name)) if names(&name.names) == ["variable"] => Ok(-1),
        Some(Node::String(text)) if text.eq_ignore_ascii_case("variable") => Ok(-1),
        Some(Node::TypeName(name)) => Err(bad(type_name_text(name))),
        Some(Node::String(text)) => Err(bad(text.to_string())),
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("{} requires a parameter", def_name(def)),
        )),
    }
}

impl Definer<'_, '_> {
    /// `DefineType`: a shell type when the statement has no parameters, else the base type that fills in the shell type of the name, with its array type. Only a superuser makes a base type, and the check comes first, as for a shell type too.
    pub(super) fn define_type(&mut self, stmt: &DefineStmt) -> Result<()> {
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create a base type",
            ));
        }
        let parts = names(&stmt.defnames);
        let (namespace, name) = self.ts_creation(&parts)?;
        let old = self.catalog.type_by_name(namespace, name).map(|t| (t.oid, t.is_shell()));
        let shell = match old {
            Some((ty, true)) => Some(ty),
            _ => {
                self.check_type_name(namespace, name)?;
                None
            }
        };
        if stmt.definition.is_empty() {
            if shell.is_some() {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("type \"{name}\" already exists"),
                ));
            }
            // `TypeShellMake` gives a shell type no privileges, so the default privileges do not apply.
            return self.catalog.create_shell_type(namespace, name, self.user).map(|_| ());
        }
        let Some(ty) = shell else {
            return Err(Error::new(SqlState::DUPLICATE_OBJECT, format!("type \"{name}\" does not exist"))
                .with_hint("Create the type as a shell type, then create its I/O functions, then do a full CREATE TYPE."));
        };
        let mut given: [Option<&DefElem>; PARAMETERS.len()] = [None; PARAMETERS.len()];
        for def in stmt.definition.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let word = match def_name(def) {
                "analyse" => "analyze",
                other => other,
            };
            let Some(i) = PARAMETERS.iter().position(|&p| p == word) else {
                self.flush();
                self.messages.push(Message::Warning(
                    Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("type attribute \"{}\" not recognized", def_name(def)),
                    )
                    .at_opt(place(def.location)),
                ));
                continue;
            };
            if given[i].is_some() {
                return Err(conflicting(def));
            }
            given[i] = Some(def);
        }
        let [
            like,
            length,
            input,
            output,
            receive,
            send,
            modin,
            modout,
            analyze,
            subscript,
            category,
            preferred,
            delimiter,
            element,
            default,
            by_value,
            alignment,
            storage,
            collatable,
        ] = given;
        let mut base = BaseType {
            defined: true,
            len: -1,
            by_val: false,
            category: b'U',
            preferred: false,
            delim: b',',
            input: 0,
            output: 0,
            receive: 0,
            send: 0,
            modin: 0,
            modout: 0,
            analyze: 0,
            subscript: 0,
            align: b'i',
            storage: b'p',
            collation: 0,
            default: None,
        };
        if let Some(def) = like {
            let (like, _) = self.an.type_name(&def_type_name(def)?)?;
            (base.len, base.by_val, base.align, base.storage) = self.storage_of(like)?;
        }
        if let Some(def) = length {
            base.len = type_length(def)?;
        }
        let input = input.map(qualified_name).transpose()?;
        let output = output.map(qualified_name).transpose()?;
        let receive = receive.map(qualified_name).transpose()?;
        let send = send.map(qualified_name).transpose()?;
        let modin = modin.map(qualified_name).transpose()?;
        let modout = modout.map(qualified_name).transpose()?;
        let analyze = analyze.map(qualified_name).transpose()?;
        let subscript = subscript.map(qualified_name).transpose()?;
        if let Some(def) = category {
            let text = def_string(def)?;
            let c = text.bytes().next().unwrap_or(0);
            if !(32..=126).contains(&c) {
                return Err(invalid_value(format!(
                    "invalid type category \"{text}\": must be simple ASCII"
                )));
            }
            base.category = c;
        }
        if let Some(def) = preferred {
            base.preferred = def_boolean(def_name(def), def.arg.as_ref())?;
        }
        if let Some(def) = delimiter {
            base.delim = def_string(def)?.bytes().next().unwrap_or(0);
        }
        let mut element_type = 0;
        if let Some(def) = element {
            let (ty, _) = self.an.type_name(&def_type_name(def)?)?;
            if rupg_pgcatalog::builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'p') {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("array element type cannot be {}", self.type_text(ty)),
                ));
            }
            element_type = ty;
        }
        if let Some(def) = default {
            base.default = Some(def_string(def)?);
        }
        if let Some(def) = by_value {
            base.by_val = def_boolean(def_name(def), def.arg.as_ref())?;
        }
        if let Some(def) = alignment {
            let text = def_string(def)?;
            base.align = match text.to_ascii_lowercase().as_str() {
                "double" | "float8" | "pg_catalog.float8" => b'd',
                "int4" | "pg_catalog.int4" => b'i',
                "int2" | "pg_catalog.int2" => b's',
                "char" | "pg_catalog.bpchar" => b'c',
                _ => return Err(invalid_value(format!("alignment \"{text}\" not recognized"))),
            };
        }
        if let Some(def) = storage {
            let text = def_string(def)?;
            base.storage = match text.to_ascii_lowercase().as_str() {
                "plain" => b'p',
                "external" => b'e',
                "extended" => b'x',
                "main" => b'm',
                _ => return Err(invalid_value(format!("storage \"{text}\" not recognized"))),
            };
        }
        if let Some(def) = collatable {
            let on = def_boolean(def_name(def), def.arg.as_ref())?;
            base.collation = if on { DEFAULT_COLLATION } else { 0 };
        }
        let Some(input) = input else {
            return Err(invalid("type input function must be specified"));
        };
        let Some(output) = output else {
            return Err(invalid("type output function must be specified"));
        };
        if modin.is_none() && modout.is_some() {
            return Err(invalid(
                "type modifier output function is useless without a type modifier input function",
            ));
        }
        base.input = self.type_input_function(&input, ty)?;
        base.output = self.type_output_function(&output, ty)?;
        if let Some(list) = &receive {
            base.receive = self.type_receive_function(list, ty)?;
        }
        if let Some(list) = &send {
            base.send = self.type_support_function(list, ty, oid::BYTEA, "type send")?;
            self.warn_volatile(base.send, "type send", list);
        }
        if let Some(list) = &modin {
            base.modin =
                self.type_support_function(list, oid::CSTRING_ARRAY, oid::INT4, "typmod_in")?;
            self.warn_volatile(base.modin, "type modifier input", list);
        }
        if let Some(list) = &modout {
            base.modout = self.type_support_function(list, oid::INT4, CSTRING, "typmod_out")?;
            self.warn_volatile(base.modout, "type modifier output", list);
        }
        if let Some(list) = &analyze {
            base.analyze =
                self.type_support_function(list, oid::INTERNAL, oid::BOOL, "type analyze")?;
        }
        if let Some(list) = &subscript {
            base.subscript = self.type_support_function(
                list,
                oid::INTERNAL,
                oid::INTERNAL,
                "type subscripting",
            )?;
        } else if element_type != 0 {
            let (element_len, ..) = self.storage_of(element_type)?;
            if base.len > 0 && !base.by_val && element_len > 0 {
                base.subscript = RAW_ARRAY_SUBSCRIPT_HANDLER;
            } else {
                return Err(invalid_value(
                    "element type cannot be specified without a subscripting function".to_string(),
                ));
            }
        }
        self.catalog.define_base_type(ty, base, element_type)?;
        self.apply_default_acl(ObjRef::new(PG_TYPE, ty))
    }

    /// The `typlen`, `typbyval`, `typalign` and `typstorage` of a type, which `LIKE` copies.
    fn storage_of(&self, ty: u32) -> Result<(i16, bool, u8, u8)> {
        if let Some(row) = rupg_pgcatalog::builtin::type_by_oid(ty) {
            return Ok((row.len, row.by_val, row.align, row.storage));
        }
        match self.catalog.type_by_oid(ty).and_then(|t| t.base.as_ref()) {
            Some(b) => Ok((b.len, b.by_val, b.align, b.storage)),
            None => Ok((-1, false, b'd', b'x')),
        }
    }

    /// The warning of `findTypeInputFunction` and the other lookups for a volatile function.
    fn warn_volatile(&mut self, function: u32, what: &str, list: &[&str]) {
        let volatile = match self.catalog.function(function) {
            Some(f) => f.volatile,
            None => rupg_pgcatalog::builtin::proc_by_oid(function).map_or(b'i', |p| p.volatile),
        };
        if volatile == b'v' {
            let message = format!("{what} function {} should not be volatile", list.join("."));
            self.warning(SqlState::INVALID_OBJECT_DEFINITION, message);
        }
    }

    /// `findTypeInputFunction`: the function takes `cstring`, or `cstring`, `oid` and `int4`, and gives the type.
    fn type_input_function(&mut self, list: &[&str], ty: u32) -> Result<u32> {
        let args = [CSTRING, oid::OID, oid::INT4];
        self.one_of_two(list, &args, ty, "input")
    }

    /// `findTypeReceiveFunction`: the function takes `internal`, or `internal`, `oid` and `int4`, and gives the type.
    fn type_receive_function(&mut self, list: &[&str], ty: u32) -> Result<u32> {
        let args = [oid::INTERNAL, oid::OID, oid::INT4];
        self.one_of_two(list, &args, ty, "receive")
    }

    /// The lookup of an input or a receive function, with one argument or with three.
    fn one_of_two(&mut self, list: &[&str], args: &[u32; 3], ty: u32, word: &str) -> Result<u32> {
        let one = self.lookup_func_name(list, &args[..1])?;
        let three = self.lookup_func_name(list, args)?;
        let (function, rettype) = match (one, three) {
            (Some(_), Some(_)) => {
                return Err(Error::new(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("type {word} function {} has multiple matches", list.join(".")),
                ));
            }
            (Some(found), None) | (None, Some(found)) => found,
            (None, None) => {
                return Err(Error::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!(
                        "function {}({}) does not exist",
                        list.join("."),
                        self.type_text(args[0])
                    ),
                ));
            }
        };
        if rettype != ty {
            return Err(invalid(format!(
                "type {word} function {} must return type {}",
                list.join("."),
                self.type_text(ty)
            )));
        }
        self.warn_volatile(function, &format!("type {word}"), list);
        Ok(function)
    }

    /// `findTypeOutputFunction`: the function takes the type and gives `cstring`.
    fn type_output_function(&mut self, list: &[&str], ty: u32) -> Result<u32> {
        let function = self.type_support_function(list, ty, CSTRING, "type output")?;
        self.warn_volatile(function, "type output", list);
        Ok(function)
    }

    /// The lookup of a support function of a type with one argument of the exact type, which must give `rettype`.
    fn type_support_function(
        &mut self,
        list: &[&str],
        arg: u32,
        rettype: u32,
        what: &str,
    ) -> Result<u32> {
        let (function, found) = self.func_oid(list, &[arg])?;
        if found != rettype {
            return Err(invalid(format!(
                "{what} function {} must return type {}",
                list.join("."),
                self.type_text(rettype)
            )));
        }
        Ok(function)
    }
}

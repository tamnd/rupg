//! The commands of text search of `tsearchcmds.c`: `CREATE TEXT SEARCH TEMPLATE`, `CREATE TEXT SEARCH PARSER`, `CREATE TEXT SEARCH DICTIONARY`, `CREATE TEXT SEARCH CONFIGURATION`, `ALTER TEXT SEARCH CONFIGURATION`, `ALTER TEXT SEARCH` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`, and `COMMENT ON` and `DROP` for these objects. The objects are found by name as `get_ts_template_oid`, `get_ts_parser_oid`, `get_ts_dict_oid` and `get_ts_config_oid` of `namespace.c` find them, in the catalog and in the built-in rows.
//!
//! `CREATE TEXT SEARCH DICTIONARY` runs the init function of the template on the options, as `verify_dictoptions` does. rupg has the init functions of the templates `simple` and `snowball`. A dictionary of another template gives `0A000`.

use rupg_catalog::{
    FIRST_NORMAL_OID, ObjRef, PG_TS_CONFIG, PG_TS_DICT, PG_TS_PARSER, PG_TS_TEMPLATE, TsConfig,
    TsDictionary, TsMapping, TsParser, TsTemplate, is_pinned,
};
use rupg_common::{Error, Object, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::ObjectType;
use rupg_sql::nodes::{
    AlterObjectSchemaStmt, AlterOwnerStmt, AlterTSConfigType, AlterTSConfigurationStmt, DefElem,
    DefineStmt, DropStmt, Node, RenameStmt,
};
use rupg_types::format_type;
use rupg_types::oid::{self, INTERNAL};

use super::comment::{name_list, not_owner};
use super::drop::{builtin_object, quote_identifier};
use super::{Definer, not_yet};
use crate::tsdict::{Dictionary, cell_oid, mappings, static_rows, template_init};
use crate::typename::{names, type_name_text};

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in objects.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// The OID of the default parser of text search, `default`.
const DEFAULT_PARSER: u32 = 3722;

/// A kind of object of text search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Template,
    Parser,
    Dictionary,
    Config,
}

impl Kind {
    /// The words for the object in the messages.
    fn word(self) -> &'static str {
        match self {
            Kind::Template => "text search template",
            Kind::Parser => "text search parser",
            Kind::Dictionary => "text search dictionary",
            Kind::Config => "text search configuration",
        }
    }

    /// The built-in rows of the kind.
    fn named(self) -> Named {
        match self {
            Kind::Template => Named::Template,
            Kind::Parser => Named::Parser,
            Kind::Dictionary => Named::Dictionary,
            Kind::Config => Named::Config,
        }
    }

    /// The OID of the catalog of the kind.
    fn class(self) -> u32 {
        match self {
            Kind::Template => PG_TS_TEMPLATE,
            Kind::Parser => PG_TS_PARSER,
            Kind::Dictionary => PG_TS_DICT,
            Kind::Config => PG_TS_CONFIG,
        }
    }

    /// The kind of an `ObjectType`.
    fn of(objtype: ObjectType) -> Option<Kind> {
        match objtype {
            ObjectType::OBJECT_TSTEMPLATE => Some(Kind::Template),
            ObjectType::OBJECT_TSPARSER => Some(Kind::Parser),
            ObjectType::OBJECT_TSDICTIONARY => Some(Kind::Dictionary),
            ObjectType::OBJECT_TSCONFIGURATION => Some(Kind::Config),
            _ => None,
        }
    }
}

/// True for the kinds of `DefineStmt` of text search.
pub(super) fn is_ts_kind(objtype: ObjectType) -> bool {
    Kind::of(objtype).is_some()
}

/// The name of an option.
pub(super) fn def_name(def: &DefElem) -> &str {
    def.defname.as_deref().unwrap_or_default()
}

/// The error of `defGetString` and `defGetQualifiedName` for an option without a value.
pub(super) fn no_parameter(def: &DefElem) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, format!("{} requires a parameter", def_name(def)))
}

/// `defGetString`.
pub(super) fn def_string(def: &DefElem) -> Result<String> {
    Ok(match &def.arg {
        None => return Err(no_parameter(def)),
        Some(Node::Integer(n)) => n.to_string(),
        Some(Node::Float(s) | Node::String(s) | Node::BitString(s)) => s.to_string(),
        Some(Node::Boolean(b)) => b.to_string(),
        Some(Node::TypeName(name)) => type_name_text(name),
        Some(Node::List(list)) => names(list).join("."),
        Some(Node::A_Star(_)) => "*".to_string(),
        Some(_) => String::new(),
    })
}

/// `defGetQualifiedName`.
pub(super) fn qualified_name(def: &DefElem) -> Result<Vec<&str>> {
    match &def.arg {
        None => Err(no_parameter(def)),
        Some(Node::TypeName(name)) => Ok(names(&name.names)),
        Some(Node::List(list)) => Ok(names(list)),
        Some(Node::String(name)) => Ok(vec![&**name]),
        Some(_) => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("argument of {} must be a name", def_name(def)),
        )),
    }
}

/// `serialize_deflist`: the text of `dictinitoption`. An integer or a float has no quotes. Another value is in single quotes, with `E` before the quotes when it has a backslash.
fn serialize(options: &[&DefElem]) -> Result<String> {
    let mut out = String::new();
    for (i, def) in options.iter().enumerate() {
        let value = def_string(def)?;
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_identifier(def_name(def)));
        out.push_str(" = ");
        if matches!(def.arg, Some(Node::Integer(_) | Node::Float(_))) {
            out.push_str(&value);
            continue;
        }
        if value.contains('\\') {
            out.push('E');
        }
        out.push('\'');
        for c in value.chars() {
            if c == '\'' || c == '\\' {
                out.push(c);
            }
            out.push(c);
        }
        out.push('\'');
    }
    Ok(out)
}

impl Definer<'_, '_> {
    /// The object of the kind with the name in the schema, of the catalog or built in.
    fn ts_in(&self, kind: Kind, namespace: u32, name: &str) -> Option<u32> {
        let user = match kind {
            Kind::Template => self.catalog.ts_template_by_name(namespace, name).map(|t| t.oid),
            Kind::Parser => self.catalog.ts_parser_by_name(namespace, name).map(|p| p.oid),
            Kind::Dictionary => self.catalog.ts_dictionary_by_name(namespace, name).map(|d| d.oid),
            Kind::Config => self.catalog.ts_config_by_name(namespace, name).map(|c| c.oid),
        };
        user.or_else(|| {
            builtin::named(kind.named())
                .iter()
                .find(|r| r.namespace == namespace && r.name == name)
                .map(|r| r.oid)
        })
    }

    /// `get_ts_*_oid`: the object of the kind that a name gives. With `missing_ok`, a name that gives no object, or a schema that does not exist, is `None`.
    fn ts_lookup(&self, kind: Kind, list: &[&str], missing_ok: bool) -> Result<Option<u32>> {
        let (schema, name) = match self.an.split_name(list, None) {
            Ok(split) => split,
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let found = match schema {
            Some(namespace) => self.ts_in(kind, namespace, name),
            None => self.an.path.iter().find_map(|&ns| self.ts_in(kind, ns, name)),
        };
        if found.is_none() && !missing_ok {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("{} \"{}\" does not exist", kind.word(), list.join(".")),
            ));
        }
        Ok(found)
    }

    /// `get_object_address` for an object of text search.
    pub(super) fn ts_address(&self, objtype: ObjectType, list: &[&str]) -> Result<ObjRef> {
        let Some(kind) = Kind::of(objtype) else {
            return Err(Error::internal("not an object of text search"));
        };
        Ok(ObjRef::new(kind.class(), self.ts_oid(kind, list)?))
    }

    /// `get_ts_*_oid` with `missing_ok` false.
    fn ts_oid(&self, kind: Kind, list: &[&str]) -> Result<u32> {
        Ok(self.ts_lookup(kind, list, false)?.unwrap_or_default())
    }

    /// `QualifiedNameGetCreationNamespace`: the schema and the name of a new object.
    pub(super) fn ts_creation<'n>(&self, list: &[&'n str]) -> Result<(u32, &'n str)> {
        let (schema, name) = self.an.split_name(list, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        Ok((namespace, name))
    }

    /// `DefineTSTemplate`, `DefineTSDictionary` and `DefineTSConfiguration`.
    pub(super) fn define_ts(&mut self, stmt: &DefineStmt) -> Result<()> {
        let list = names(&stmt.defnames);
        let defs: Vec<&DefElem> = stmt
            .definition
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::DefElem(def) => Some(&**def),
                _ => None,
            })
            .collect();
        match Kind::of(stmt.kind) {
            Some(Kind::Template) => self.create_ts_template(&list, &defs),
            Some(Kind::Dictionary) => self.create_ts_dictionary(&list, &defs),
            Some(Kind::Config) => self.create_ts_config(&list, &defs),
            Some(Kind::Parser) => self.create_ts_parser(&list, &defs),
            None => Err(Error::internal("not an object of text search")),
        }
    }

    /// `DefineTSTemplate`.
    fn create_ts_template(&mut self, list: &[&str], defs: &[&DefElem]) -> Result<()> {
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create text search templates",
            ));
        }
        let (namespace, name) = self.ts_creation(list)?;
        let (mut init, mut lexize) = (0, 0);
        for def in defs {
            match def_name(def) {
                "init" => init = self.ts_function(def, &[INTERNAL], INTERNAL)?,
                "lexize" => lexize = self.ts_function(def, &[INTERNAL; 4], INTERNAL)?,
                other => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("text search template parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        if lexize == 0 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "text search template lexize method is required",
            ));
        }
        let new = TsTemplate { oid: 0, name: name.to_string(), namespace, init, lexize };
        self.catalog.create_ts_template(new)?;
        Ok(())
    }

    /// `DefineTSParser`.
    fn create_ts_parser(&mut self, list: &[&str], defs: &[&DefElem]) -> Result<()> {
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create text search parsers",
            ));
        }
        let (namespace, name) = self.ts_creation(list)?;
        let mut new = TsParser {
            oid: 0,
            name: name.to_string(),
            namespace,
            start: 0,
            token: 0,
            end: 0,
            headline: 0,
            lextype: 0,
        };
        for def in defs {
            match def_name(def) {
                "start" => new.start = self.ts_function(def, &[INTERNAL, oid::INT4], INTERNAL)?,
                "gettoken" => new.token = self.ts_function(def, &[INTERNAL; 3], INTERNAL)?,
                "end" => new.end = self.ts_function(def, &[INTERNAL], oid::VOID)?,
                "headline" => {
                    new.headline =
                        self.ts_function(def, &[INTERNAL, INTERNAL, oid::TSQUERY], INTERNAL)?;
                }
                "lextypes" => new.lextype = self.ts_function(def, &[INTERNAL], INTERNAL)?,
                other => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("text search parser parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        let methods = [
            (new.start, "start"),
            (new.token, "gettoken"),
            (new.end, "end"),
            (new.lextype, "lextypes"),
        ];
        if let Some((_, method)) = methods.iter().find(|(function, _)| *function == 0) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("text search parser {method} method is required"),
            ));
        }
        self.catalog.create_ts_parser(new)?;
        Ok(())
    }

    /// `get_ts_template_func` and `get_ts_parser_func`: the function of an option of a template or a parser with the argument types `args`, as `LookupFuncName` finds it. It must give the type `result`.
    fn ts_function(&self, def: &DefElem, args: &[u32], result: u32) -> Result<u32> {
        let list = qualified_name(def)?;
        let (schema, name) = self.an.split_name(&list, None)?;
        let schemas = match schema {
            Some(namespace) => vec![namespace],
            None => self.an.path.clone(),
        };
        let found = schemas.iter().find_map(|&namespace| {
            let user = self
                .catalog
                .functions()
                .find(|f| f.namespace == namespace && f.name == name && f.argtypes == args)
                .map(|f| (f.oid, f.rettype));
            user.or_else(|| {
                builtin::procs_named(name)
                    .find(|p| p.namespace == namespace && p.argtypes == args)
                    .map(|p| (p.oid, p.rettype))
            })
        });
        let types: Vec<String> = args.iter().map(|&ty| format_type(ty).into_owned()).collect();
        let signature = format!("{}({})", list.join("."), types.join(", "));
        match found {
            Some((function, ty)) if ty == result => Ok(function),
            Some(_) => Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("function {signature} should return type {}", format_type(result)),
            )),
            None => Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {signature} does not exist"),
            )),
        }
    }

    /// `verify_dictoptions`: the init function of the template must accept the options.
    fn verify_dictoptions(&self, template: u32, options: &[&DefElem]) -> Result<()> {
        let (name, init) = template_init(Some(self.catalog), template).unwrap_or_default();
        if init.is_empty() {
            if options.is_empty() {
                return Ok(());
            }
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("text search template \"{name}\" does not accept options"),
            ));
        }
        let pairs = options
            .iter()
            .map(|def| Ok((def_name(def).to_string(), def_string(def)?)))
            .collect::<Result<Vec<_>>>()?;
        match Dictionary::init(&init, &pairs)? {
            Some(_) => Ok(()),
            None => Err(not_yet(&format!("a dictionary of the template {name}"), None)),
        }
    }

    /// `DefineTSDictionary`.
    fn create_ts_dictionary(&mut self, list: &[&str], defs: &[&DefElem]) -> Result<()> {
        let (namespace, name) = self.ts_creation(list)?;
        self.check_create_in(namespace, None)?;
        let mut template = 0;
        let mut options = Vec::new();
        for &def in defs {
            if def_name(def) == "template" {
                template = self.ts_oid(Kind::Template, &qualified_name(def)?)?;
            } else {
                options.push(def);
            }
        }
        if template == 0 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "text search template is required",
            ));
        }
        self.verify_dictoptions(template, &options)?;
        let options = if options.is_empty() { None } else { Some(serialize(&options)?) };
        let new = TsDictionary {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            template,
            options,
        };
        let dictionary = self.catalog.create_ts_dictionary(new)?;
        self.collect(None, ObjRef::new(PG_TS_DICT, dictionary));
        Ok(())
    }

    /// The parser of a configuration.
    fn config_parser(&self, config: u32) -> u32 {
        match self.catalog.ts_config(config) {
            Some(c) => c.parser,
            None => rupg_pgcatalog::catalog("pg_ts_config")
                .and_then(|catalog| {
                    static_rows("pg_ts_config", "oid", config)
                        .first()
                        .map(|&row| cell_oid(catalog, row, "cfgparser"))
                })
                .unwrap_or(DEFAULT_PARSER),
        }
    }

    /// The rows of `pg_ts_config_map` of a configuration, in the order of the index on the configuration, the token type and the place.
    fn config_mappings(&self, config: u32) -> Vec<TsMapping> {
        mappings(Some(self.catalog), config)
    }

    /// `DefineTSConfiguration`. With `COPY`, the new configuration gets the parser and the map of the old one.
    fn create_ts_config(&mut self, list: &[&str], defs: &[&DefElem]) -> Result<()> {
        let (namespace, name) = self.ts_creation(list)?;
        self.check_create_in(namespace, None)?;
        let (mut parser, mut source) = (0, 0);
        for def in defs {
            match def_name(def) {
                "parser" => parser = self.ts_oid(Kind::Parser, &qualified_name(def)?)?,
                "copy" => source = self.ts_oid(Kind::Config, &qualified_name(def)?)?,
                other => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("text search configuration parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        if source != 0 && parser != 0 {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "cannot specify both PARSER and COPY options",
            ));
        }
        let mut copy = Vec::new();
        if source != 0 {
            parser = self.config_parser(source);
            copy = self.config_mappings(source);
        }
        if parser == 0 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "text search parser is required",
            ));
        }
        let new = TsConfig { oid: 0, name: name.to_string(), namespace, owner: self.user, parser };
        let config = self.catalog.create_ts_config(new, &copy)?;
        self.collect(None, ObjRef::new(PG_TS_CONFIG, config));
        Ok(())
    }

    /// `getTokenTypes`: the numbers of the token types of the default parser, without duplicates.
    fn token_types(list: &[Option<Node>]) -> Result<Vec<(i32, String)>> {
        let mut out: Vec<(i32, String)> = Vec::new();
        for name in names(list) {
            if out.iter().any(|(_, n)| n == name) {
                continue;
            }
            let number = (1..)
                .zip(rupg_pgcatalog::TOKEN_TYPES)
                .find(|(_, (alias, _))| *alias == name)
                .map(|(n, _)| n)
                .ok_or_else(|| {
                    Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("token type \"{name}\" does not exist"),
                    )
                })?;
            out.push((number, name.to_string()));
        }
        Ok(out)
    }

    /// `AlterTSConfiguration`: `ADD MAPPING`, `ALTER MAPPING` and `DROP MAPPING`.
    pub(super) fn alter_ts_config(&mut self, stmt: &AlterTSConfigurationStmt) -> Result<()> {
        let list = names(&stmt.cfgname);
        let shown = list.join(".");
        let Some(config) = self.ts_lookup(Kind::Config, &list, true)? else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("text search configuration \"{shown}\" does not exist"),
            ));
        };
        self.check_ts_owner(Kind::Config, config, &shown)?;
        if !stmt.dicts.is_empty() {
            self.make_mapping(stmt, config)?;
        } else if !stmt.tokentype.is_empty() {
            self.drop_mapping(stmt, config)?;
        }
        self.catalog.update_ts_config_depends(config);
        Ok(())
    }

    /// `MakeConfigurationMapping`.
    fn make_mapping(&mut self, stmt: &AlterTSConfigurationStmt, config: u32) -> Result<()> {
        let tokens: Vec<i32> =
            Self::token_types(&stmt.tokentype)?.into_iter().map(|(n, _)| n).collect();
        let mut dicts = Vec::new();
        for dict in stmt.dicts.iter().flatten() {
            let Node::List(list) = dict else { continue };
            dicts.push(self.ts_oid(Kind::Dictionary, &names(list))?);
        }
        if stmt.kind == AlterTSConfigType::ALTER_TSCONFIG_REPLACE_DICT
            || stmt.kind == AlterTSConfigType::ALTER_TSCONFIG_REPLACE_DICT_FOR_TOKEN
        {
            let (Some(&old), Some(&new)) = (dicts.first(), dicts.get(1)) else {
                return Err(Error::internal("ALTER MAPPING REPLACE needs two dictionaries"));
            };
            self.catalog.replace_ts_dictionary(config, &tokens, old, new);
            return Ok(());
        }
        let keep = |m: &&TsMapping| !(stmt.r#override && tokens.contains(&m.token));
        for &token in &tokens {
            for seqno in (1..).take(dicts.len()) {
                let taken = self
                    .catalog
                    .ts_mappings()
                    .iter()
                    .filter(keep)
                    .any(|m| m.config == config && m.token == token && m.seqno == seqno);
                if taken {
                    return Err(Error::new(
                        SqlState::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_ts_config_map_index\"",
                    )
                    .with_detail(format!(
                        "Key (mapcfg, maptokentype, mapseqno)=({config}, {token}, {seqno}) already exists."
                    ))
                    .with_object(Object::Schema, "pg_catalog")
                    .with_object(Object::Table, "pg_ts_config_map")
                    .with_object(Object::Constraint, "pg_ts_config_map_index"));
                }
            }
        }
        self.catalog.add_ts_mapping(config, &tokens, &dicts, stmt.r#override);
        Ok(())
    }

    /// `DropConfigurationMapping`. With `IF EXISTS`, a token type without rows gives a `NOTICE`.
    fn drop_mapping(&mut self, stmt: &AlterTSConfigurationStmt, config: u32) -> Result<()> {
        for (token, name) in Self::token_types(&stmt.tokentype)? {
            if self.catalog.drop_ts_mapping(config, token) {
                continue;
            }
            if !stmt.missing_ok {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("mapping for token type \"{name}\" does not exist"),
                ));
            }
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("mapping for token type \"{name}\" does not exist, skipping"),
            );
        }
        Ok(())
    }

    /// `check_object_ownership` for an object of text search. A template and a parser need a superuser, and a dictionary and a configuration need their owner. rupg cannot change a built-in object.
    fn check_ts_owner(&self, kind: Kind, object: u32, shown: &str) -> Result<()> {
        let (user, owner) = match kind {
            Kind::Template => (self.catalog.ts_template(object).is_some(), None),
            Kind::Parser => (self.catalog.ts_parser(object).is_some(), None),
            Kind::Dictionary => {
                let dict = self.catalog.ts_dictionary(object);
                (dict.is_some(), dict.map(|d| d.owner))
            }
            Kind::Config => {
                let config = self.catalog.ts_config(object);
                (config.is_some(), config.map(|c| c.owner))
            }
        };
        if !self.is_superuser() {
            if matches!(kind, Kind::Template | Kind::Parser) {
                return Err(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, "must be superuser"));
            }
            if !owner.is_some_and(|o| self.owns(o)) {
                return Err(not_owner(kind.word(), shown));
            }
        }
        if !user {
            return Err(not_yet("a change of a built-in object of text search", None));
        }
        Ok(())
    }

    /// The schema, the name and the owner of an object of the kind, of the catalog or built in. A template, a parser and a built-in object have the bootstrap superuser as owner.
    fn ts_row(&self, kind: Kind, object: u32) -> Option<(u32, String, u32)> {
        let user = match kind {
            Kind::Template => {
                self.catalog.ts_template(object).map(|t| (t.namespace, t.name.clone(), 0))
            }
            Kind::Parser => {
                self.catalog.ts_parser(object).map(|p| (p.namespace, p.name.clone(), 0))
            }
            Kind::Dictionary => {
                self.catalog.ts_dictionary(object).map(|d| (d.namespace, d.name.clone(), d.owner))
            }
            Kind::Config => {
                self.catalog.ts_config(object).map(|c| (c.namespace, c.name.clone(), c.owner))
            }
        };
        user.or_else(|| {
            builtin::named_by_oid(kind.named(), object).map(|r| (r.namespace, r.name.into(), 0))
        })
        .map(|(namespace, name, owner)| {
            (namespace, name, if owner == 0 { BOOTSTRAP_SUPERUSER } else { owner })
        })
    }

    /// The objects of `DROP TEXT SEARCH`, as `RemoveObjects` finds them, with the check of `check_object_ownership`. The owner of the schema of an object can also remove it. A name that gives no object gives the notice of `does_not_exist_skipping` with `IF EXISTS`.
    pub(super) fn find_ts(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let Some(kind) = Kind::of(stmt.removeType) else {
            return Err(Error::internal("not an object of text search"));
        };
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let list = name_list(Some(item));
            let Some(found) = self.ts_lookup(kind, &list, stmt.missing_ok)? else {
                let schema = match list.as_slice() {
                    [schema, _] | [_, schema, _] => Some(*schema),
                    _ => None,
                };
                let message = match schema {
                    Some(schema) if self.an.schema(schema).is_none() => {
                        format!("schema \"{schema}\" does not exist, skipping")
                    }
                    _ => format!("{} \"{}\" does not exist, skipping", kind.word(), list.join(".")),
                };
                self.notice(SqlState::SUCCESSFUL_COMPLETION, message);
                continue;
            };
            let (namespace, _, owner) = self.ts_row(kind, found).unwrap_or_default();
            if !self.is_superuser() && !self.schema_owner(namespace).is_some_and(|o| self.owns(o)) {
                if matches!(kind, Kind::Template | Kind::Parser) {
                    return Err(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, "must be superuser"));
                }
                if !self.owns(owner) {
                    return Err(not_owner(kind.word(), &list.join(".")));
                }
            }
            let object = ObjRef::new(kind.class(), found);
            if found < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// `getObjectDescription` for an object of text search: the words of its kind and its name, after its schema when the object is not visible.
    pub(super) fn describe_ts(&self, object: ObjRef) -> String {
        let kind = match object.class {
            PG_TS_TEMPLATE => Kind::Template,
            PG_TS_PARSER => Kind::Parser,
            PG_TS_DICT => Kind::Dictionary,
            _ => Kind::Config,
        };
        match self.ts_row(kind, object.oid) {
            Some((namespace, name, _)) => {
                let shown =
                    self.qualify(namespace, &name, |ns| self.ts_in(kind, ns, &name).is_some());
                format!("{} {shown}", kind.word())
            }
            None => format!("{} {}", kind.word(), object.oid),
        }
    }

    /// The object of an `ALTER TEXT SEARCH` statement, as `get_object_address` finds it: the kind, the OID, the schema, the name and the owner.
    fn ts_target(
        &self,
        objtype: ObjectType,
        object: Option<&Node>,
    ) -> Result<(Kind, u32, u32, String, u32)> {
        let Some(kind) = Kind::of(objtype) else {
            return Err(Error::internal("not an object of text search"));
        };
        let found = self.ts_oid(kind, &name_list(object))?;
        let (namespace, name, owner) = self.ts_row(kind, found).unwrap_or_default();
        Ok((kind, found, namespace, name, owner))
    }

    /// The error of a change of a built-in object of text search, which rupg cannot change.
    fn check_ts_builtin(object: u32) -> Result<()> {
        if object < FIRST_NORMAL_OID {
            return Err(not_yet("a change of a built-in object of text search", None));
        }
        Ok(())
    }

    /// The error of `AlterObjectRename_internal` and `AlterObjectNamespace_internal` for a template or a parser, which has no owner, when the user is not a superuser. `action` is the words of the change.
    fn ts_superuser(&self, kind: Kind, object: u32, action: &str) -> Error {
        let shown = self.describe_ts(ObjRef::new(kind.class(), object));
        Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be superuser to {action} {shown}"),
        )
    }

    /// `AlterObjectRename_internal` for `ALTER TEXT SEARCH ... RENAME TO`. A user who is not a superuser must own the object and be able to create in its schema. A template and a parser need a superuser.
    pub(super) fn rename_ts(&mut self, stmt: &RenameStmt) -> Result<()> {
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let (kind, found, namespace, name, owner) =
            self.ts_target(stmt.renameType, stmt.object.as_ref())?;
        if !self.is_superuser() {
            if matches!(kind, Kind::Template | Kind::Parser) {
                return Err(self.ts_superuser(kind, found, "rename"));
            }
            if !self.owns(owner) {
                return Err(not_owner(kind.word(), &name));
            }
            self.check_create_in(namespace, None)?;
        }
        if self.ts_in(kind, namespace, newname).is_some() {
            return Err(self.ts_conflict(kind, newname, namespace));
        }
        Self::check_ts_builtin(found)?;
        self.catalog.rename_ts(kind.class(), found, newname);
        Ok(())
    }

    /// `AlterObjectOwner_internal` for `ALTER TEXT SEARCH DICTIONARY` and `ALTER TEXT SEARCH CONFIGURATION` with `OWNER TO`. A user who is not a superuser must own the object and be able to become the new owner, and the new owner must be able to create in the schema of the object.
    pub(super) fn alter_ts_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let (kind, found, namespace, name, owner) =
            self.ts_target(stmt.objectType, stmt.object.as_ref())?;
        if owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if !self.owns(owner) {
                return Err(not_owner(kind.word(), &name));
            }
            self.check_can_set_role(new)?;
            if !self.may_create_in(namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(namespace)),
                ));
            }
        }
        Self::check_ts_builtin(found)?;
        self.catalog.set_object_owner(ObjRef::new(kind.class(), found), new, "", "")
    }

    /// `ExecAlterObjectSchemaStmt` and `AlterObjectNamespace_internal` for `ALTER TEXT SEARCH ... SET SCHEMA`: `LookupCreationNamespace` for the new schema, then `CheckSetNamespace`, the checks of the owner and of the name, and the move. An object that is in the schema already needs no change. A template and a parser need a superuser.
    pub(super) fn alter_ts_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let (kind, found, old, name, owner) =
            self.ts_target(stmt.objectType, stmt.object.as_ref())?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        let namespace = self.lookup_creation_namespace(schema)?;
        if old == namespace {
            return Ok(());
        }
        self.check_set_namespace(old, namespace)?;
        if !self.is_superuser() {
            if matches!(kind, Kind::Template | Kind::Parser) {
                return Err(self.ts_superuser(kind, found, "set schema of"));
            }
            if !self.owns(owner) {
                return Err(not_owner(kind.word(), &name));
            }
        }
        if self.ts_in(kind, namespace, &name).is_some() {
            return Err(self.ts_conflict(kind, &name, namespace));
        }
        Self::check_ts_builtin(found)?;
        self.catalog.set_ts_namespace(kind.class(), found, namespace);
        Ok(())
    }

    /// `report_namespace_conflict` for an object of text search.
    fn ts_conflict(&self, kind: Kind, name: &str, namespace: u32) -> Error {
        Error::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "{} \"{name}\" already exists in schema \"{}\"",
                kind.word(),
                self.schema_name(namespace)
            ),
        )
    }

    /// The object of `COMMENT ON TEXT SEARCH`, after the check of its owner.
    pub(super) fn comment_ts(&self, objtype: ObjectType, object: Option<&Node>) -> Result<ObjRef> {
        let Some(kind) = Kind::of(objtype) else {
            return Err(Error::internal("not an object of text search"));
        };
        let list = name_list(object);
        let found = self.ts_oid(kind, &list)?;
        self.check_ts_owner(kind, found, &list.join("."))?;
        Ok(ObjRef::new(kind.class(), found))
    }
}

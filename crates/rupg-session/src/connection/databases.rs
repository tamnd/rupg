//! `CREATE DATABASE`, `ALTER DATABASE ... SET` and `DROP DATABASE`: the checks of `createdb` and `dropdb` in the order of PostgreSQL, on the rows of `pg_database` that the store keeps. The engine has the encoding `UTF8` and the locale `C` only, so a new database must have them. The checks of PostgreSQL come first, and then the error `0A000` for an encoding or a locale that the engine does not have.

use rupg_catalog::Database;
use rupg_common::{Error, SqlState};
use rupg_func::{char_to_encoding, encoding_to_char};
use rupg_pgcatalog::builtin;
use rupg_sql::Severity;
use rupg_sql::nodes::{
    AlterDatabaseSetStmt, CreatedbStmt, DefElem, DropdbStmt, Node, VariableSetKind,
};
use rupg_wire::CommandTag;

use super::copy::{def_boolean, def_string};
use super::{Connection, Done};
use crate::store::NewDatabase;
use crate::utility::{self, Notice};

/// `FirstNormalObjectId`.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;
/// The number of the encoding `UTF8`.
const UTF8: i32 = 6;
/// `PG_ENCODING_BE_LAST`: the number of the last server encoding.
const LAST_SERVER_ENCODING: i32 = 34;
/// The OIDs of `pg_default` and `pg_global`.
const DEFAULT_TABLESPACE: u32 = 1663;
const GLOBAL_TABLESPACE: u32 = 1664;
/// The version of the collations of the builtin provider.
const BUILTIN_VERSION: &str = "1";

/// The options of `CREATE DATABASE`, as the loop of `createdb` reads them.
#[derive(Default)]
struct Options<'a> {
    tablespace: Option<&'a DefElem>,
    owner: Option<&'a DefElem>,
    template: Option<&'a DefElem>,
    encoding: Option<&'a DefElem>,
    locale: Option<&'a DefElem>,
    builtin_locale: Option<&'a DefElem>,
    collate: Option<&'a DefElem>,
    ctype: Option<&'a DefElem>,
    icu_locale: Option<&'a DefElem>,
    icu_rules: Option<&'a DefElem>,
    provider: Option<&'a DefElem>,
    is_template: Option<&'a DefElem>,
    allow_connections: Option<&'a DefElem>,
    connection_limit: Option<&'a DefElem>,
    collation_version: Option<&'a DefElem>,
    strategy: Option<&'a DefElem>,
    oid: u32,
}

impl<'a> Options<'a> {
    fn read(stmt: &'a CreatedbStmt, notices: &mut Vec<Notice>) -> Result<Options<'a>, Error> {
        let mut options = Options::default();
        for node in stmt.options.iter().flatten() {
            let Node::DefElem(def) = node else { continue };
            let at = usize::try_from(def.location).ok();
            let slot = match def.defname.as_deref().unwrap_or_default() {
                "tablespace" => &mut options.tablespace,
                "owner" => &mut options.owner,
                "template" => &mut options.template,
                "encoding" => &mut options.encoding,
                "locale" => &mut options.locale,
                "builtin_locale" => &mut options.builtin_locale,
                "lc_collate" => &mut options.collate,
                "lc_ctype" => &mut options.ctype,
                "icu_locale" => &mut options.icu_locale,
                "icu_rules" => &mut options.icu_rules,
                "locale_provider" => &mut options.provider,
                "is_template" => &mut options.is_template,
                "allow_connections" => &mut options.allow_connections,
                "connection_limit" => &mut options.connection_limit,
                "collation_version" => &mut options.collation_version,
                "strategy" => &mut options.strategy,
                "location" => {
                    notices.push(Notice::warning(
                        Error::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "LOCATION is not supported anymore",
                        )
                        .with_hint("Consider using tablespaces instead.")
                        .with_position(at),
                    ));
                    continue;
                }
                "oid" => {
                    options.oid = def_object_id(def)?;
                    if options.oid < FIRST_NORMAL_OBJECT_ID {
                        return Err(Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!(
                                "OIDs less than {FIRST_NORMAL_OBJECT_ID} are reserved for system objects"
                            ),
                        ));
                    }
                    continue;
                }
                name => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("option \"{name}\" not recognized"),
                    )
                    .with_position(at));
                }
            };
            if slot.is_some() {
                return Err(Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
                    .with_position(at));
            }
            *slot = Some(def);
        }
        Ok(options)
    }
}

/// The value of an option that is not `DEFAULT`.
fn given(option: Option<&DefElem>) -> Option<&DefElem> {
    option.filter(|def| def.arg.is_some())
}

/// The text of an option that is not `DEFAULT`.
fn text(option: Option<&DefElem>) -> Result<Option<String>, Error> {
    given(option).map(def_string).transpose()
}

/// `defGetInt32`.
fn def_int32(def: &DefElem) -> Result<i32, Error> {
    match &def.arg {
        Some(Node::Integer(value)) => Ok(*value),
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("{} requires an integer value", def.defname.as_deref().unwrap_or_default()),
        )),
    }
}

/// `defGetObjectId`. A number that is too large for `integer` is a `Float` node, which goes through the input function of `oid`.
fn def_object_id(def: &DefElem) -> Result<u32, Error> {
    match &def.arg {
        #[allow(clippy::cast_sign_loss)]
        Some(Node::Integer(value)) => Ok(*value as u32),
        Some(Node::Float(text)) => rupg_types::oid_in(text).map_err(Error::from),
        _ => Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("{} requires a numeric value", def.defname.as_deref().unwrap_or_default()),
        )),
    }
}

/// `PG_VALID_BE_ENCODING` with the name of the encoding.
fn server_encoding(number: i32) -> bool {
    (0..=LAST_SERVER_ENCODING).contains(&number) && !encoding_to_char(i64::from(number)).is_empty()
}

/// What `check_locale` finds for the name of a libc locale.
enum Locale {
    /// A locale that the engine has, with its canonical name.
    Known(String),
    /// A name in the form of a locale of the system. The engine does not have it.
    Other,
    /// A name that is not a locale.
    Invalid,
}

/// `check_locale` without the locales of the system. `C` and `POSIX` are the locale `C`. A name in the form `language[_TERRITORY][.codeset][@modifier]`, and `C.UTF-8`, can be a locale of the system, which the engine does not have.
fn check_locale(name: &str) -> Locale {
    if name == "C" || name == "POSIX" {
        return Locale::Known("C".to_owned());
    }
    let (base, _modifier) = name.split_once('@').unwrap_or((name, ""));
    let (base, codeset) = base.split_once('.').map_or((base, None), |(b, c)| (b, Some(c)));
    let (language, territory) = base.split_once('_').map_or((base, None), |(l, t)| (l, Some(t)));
    let language_ok = (2..=3).contains(&language.len())
        && language.bytes().all(|b| b.is_ascii_lowercase())
        || language == "C" && codeset.is_some() && territory.is_none();
    let territory_ok =
        territory.is_none_or(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_uppercase()));
    let codeset_ok = codeset
        .is_none_or(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    if language_ok && territory_ok && codeset_ok { Locale::Other } else { Locale::Invalid }
}

/// `pg_get_encoding_from_locale`: the encoding that the codeset of a locale requires, or -1 when it does not require one. The locale `C` takes any encoding, as `SQL_ASCII` does.
fn locale_encoding(name: &str) -> i32 {
    if name == "C" || name == "POSIX" {
        return 0;
    }
    let base = name.split_once('@').map_or(name, |(base, _)| base);
    match base.split_once('.') {
        Some((_, codeset))
            if codeset.eq_ignore_ascii_case("utf8") || codeset.eq_ignore_ascii_case("utf-8") =>
        {
            UTF8
        }
        _ => -1,
    }
}

/// `collprovider_name`.
fn provider_name(provider: u8) -> &'static str {
    match provider {
        b'b' => "builtin",
        b'i' => "icu",
        _ => "libc",
    }
}

/// The error of `check_locale` for a name that is not a locale.
fn invalid_locale(category: &str, name: &str, provider: u8) -> Error {
    let error = Error::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("invalid {category} locale name: \"{name}\""),
    );
    match provider {
        b'b' => error.with_hint(
            "If the locale name is specific to the builtin provider, use BUILTIN_LOCALE.",
        ),
        b'i' => {
            error.with_hint("If the locale name is specific to the ICU provider, use ICU_LOCALE.")
        }
        _ => error,
    }
}

/// `check_encoding_locale_matches` for one locale.
fn locale_matches(encoding: i32, locale: &str, category: &str) -> Result<(), Error> {
    let required = locale_encoding(locale);
    if required == encoding || required == 0 || required == -1 || encoding == 0 {
        return Ok(());
    }
    Err(Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "encoding \"{}\" does not match locale \"{locale}\"",
            encoding_to_char(i64::from(encoding))
        ),
    )
    .with_detail(format!(
        "The chosen {category} setting requires encoding \"{}\".",
        encoding_to_char(i64::from(required))
    )))
}

/// The error for a part of a new database that the engine does not have.
fn not_supported(what: String) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

impl Connection {
    /// `PreventInTransactionBlock` for `CREATE DATABASE` and `DROP DATABASE`. The caller commits the statement at once, also in the extended protocol.
    fn prevent_block(&self, statement: &str) -> Result<(), Error> {
        if self.transaction.in_block() {
            return Err(Error::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{statement} cannot run inside a transaction block"),
            ));
        }
        if self.pipelining {
            return Err(Error::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{statement} cannot be executed within a pipeline"),
            ));
        }
        Ok(())
    }

    /// `superuser()` for the role of the session.
    fn superuser(&self) -> bool {
        builtin::roles().iter().any(|role| role.oid == self.role && role.superuser)
    }

    /// `get_role_oid`: the roles of the catalog, and the user of the session, which is the bootstrap superuser when it is not a role of the catalog.
    fn role_named(&self, name: &str) -> Result<u32, Error> {
        builtin::roles()
            .iter()
            .find(|role| role.name == name)
            .map(|role| role.oid)
            .or_else(|| (name == self.user).then_some(self.role))
            .ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist"))
            })
    }

    /// `createdb`.
    pub(super) fn create_database(
        &mut self,
        stmt: &CreatedbStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        self.prevent_block("CREATE DATABASE")?;
        let name = stmt.dbname.as_deref().unwrap_or_default();
        if name.contains(['\n', '\r']) {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("database name \"{name}\" contains a newline or carriage return character"),
            ));
        }
        let options = Options::read(stmt, notices)?;
        let owner = text(options.owner)?;
        let template_name = text(options.template)?;
        let mut encoding = -1;
        if let Some(def) = given(options.encoding) {
            let at = usize::try_from(def.location).ok();
            if let Some(Node::Integer(number)) = &def.arg {
                encoding = *number;
                if !server_encoding(encoding) {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("{encoding} is not a valid encoding code"),
                    )
                    .with_position(at));
                }
            } else {
                let text = def_string(def)?;
                encoding = char_to_encoding(&text);
                if !server_encoding(encoding) {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("{text} is not a valid encoding name"),
                    )
                    .with_position(at));
                }
            }
        }
        let (mut collate, mut ctype, mut locale) = match text(options.locale)? {
            Some(text) => (Some(text.clone()), Some(text.clone()), Some(text)),
            None => (None, None, None),
        };
        if let Some(text) = text(options.builtin_locale)? {
            locale = Some(text);
        }
        if let Some(text) = text(options.collate)? {
            collate = Some(text);
        }
        if let Some(text) = text(options.ctype)? {
            ctype = Some(text);
        }
        if let Some(text) = text(options.icu_locale)? {
            locale = Some(text);
        }
        let mut icu_rules = text(options.icu_rules)?;
        let mut provider = None;
        if let Some(text) = text(options.provider)? {
            provider = Some(match text.to_ascii_lowercase().as_str() {
                "builtin" => b'b',
                "icu" => b'i',
                "libc" => b'c',
                _ => {
                    return Err(Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!("unrecognized locale provider: {text}"),
                    ));
                }
            });
        }
        let is_template = given(options.is_template).map(def_boolean).transpose()?.unwrap_or(false);
        let allow_connections =
            given(options.allow_connections).map(def_boolean).transpose()?.unwrap_or(true);
        let mut connection_limit = rupg_catalog::database::UNLIMITED;
        if let Some(def) = given(options.connection_limit) {
            connection_limit = def_int32(def)?;
            if connection_limit < rupg_catalog::database::UNLIMITED {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid connection limit: {connection_limit}"),
                ));
            }
        }
        let mut collation_version = options.collation_version.map(def_string).transpose()?;

        let owner = match &owner {
            Some(name) => self.role_named(name)?,
            None => self.role,
        };
        if !self.superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied to create database",
            ));
        }

        let template_name = template_name.unwrap_or_else(|| "template1".to_owned());
        let Some(source) = self.store.database(&template_name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_DATABASE,
                format!("template database \"{template_name}\" does not exist"),
            ));
        };
        if !source.is_template && !self.superuser() && source.owner != self.role {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to copy database \"{template_name}\""),
            ));
        }
        if let Some(strategy) = text(options.strategy)?
            && !strategy.eq_ignore_ascii_case("wal_log")
            && !strategy.eq_ignore_ascii_case("file_copy")
        {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid create database strategy \"{strategy}\""),
            )
            .with_hint("Valid strategies are \"wal_log\" and \"file_copy\"."));
        }

        if encoding < 0 {
            encoding = source.encoding;
        }
        let provider = provider.unwrap_or(source.locale_provider);
        if locale.is_none() && provider == source.locale_provider {
            locale.clone_from(&source.locale);
        }
        if icu_rules.is_none() {
            icu_rules.clone_from(&source.icu_rules);
        }
        // The parts that the engine does not have. PostgreSQL makes the database, so the error comes after all the checks of PostgreSQL.
        let mut unsupported = None;
        let mut canonical = |category: &str, name: Option<String>, fallback: &str| {
            let name = name.unwrap_or_else(|| fallback.to_owned());
            match check_locale(&name) {
                Locale::Known(canonical) => Ok(canonical),
                Locale::Other => {
                    unsupported.get_or_insert_with(|| format!("locale \"{name}\""));
                    Ok(name)
                }
                Locale::Invalid => Err(invalid_locale(category, &name, provider)),
            }
        };
        let collate = canonical("LC_COLLATE", collate, &source.collate)?;
        let ctype = canonical("LC_CTYPE", ctype, &source.ctype)?;
        locale_matches(encoding, &ctype, "LC_CTYPE")?;
        locale_matches(encoding, &collate, "LC_COLLATE")?;
        if provider != b'b' && options.builtin_locale.is_some() {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "BUILTIN_LOCALE cannot be specified unless locale provider is builtin",
            ));
        }
        if provider != b'i' {
            if options.icu_locale.is_some() {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "ICU locale cannot be specified unless locale provider is ICU",
                ));
            }
            if icu_rules.is_some() {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "ICU rules cannot be specified unless locale provider is ICU",
                ));
            }
        }
        match provider {
            b'b' => {
                let Some(name) = &locale else {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "LOCALE or BUILTIN_LOCALE must be specified",
                    ));
                };
                let canonical = match name.as_str() {
                    "C" => "C",
                    "C.UTF-8" | "C.UTF8" => "C.UTF-8",
                    "PG_UNICODE_FAST" => "PG_UNICODE_FAST",
                    _ => {
                        return Err(Error::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!("invalid locale name \"{name}\" for builtin provider"),
                        ));
                    }
                };
                if canonical != "C" && encoding != UTF8 {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!(
                            "encoding \"{}\" does not match locale \"{name}\"",
                            encoding_to_char(i64::from(encoding))
                        ),
                    ));
                }
                if canonical != "C" {
                    unsupported.get_or_insert_with(|| format!("builtin locale \"{canonical}\""));
                }
                locale = Some(canonical.to_owned());
            }
            b'i' => {
                if locale.is_none() {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "LOCALE or ICU_LOCALE must be specified",
                    ));
                }
                return Err(not_supported("the ICU locale provider".to_owned()));
            }
            _ => locale = None,
        }

        if template_name != "template0" {
            let hint = |what: &str| {
                format!(
                    "Use the same {what} as in the template database, or use template0 as template."
                )
            };
            if encoding != source.encoding {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "new encoding ({}) is incompatible with the encoding of the template database ({})",
                        encoding_to_char(i64::from(encoding)),
                        encoding_to_char(i64::from(source.encoding))
                    ),
                )
                .with_hint(hint("encoding")));
            }
            if collate != source.collate {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "new collation ({collate}) is incompatible with the collation of the template database ({})",
                        source.collate
                    ),
                )
                .with_hint(hint("collation")));
            }
            if ctype != source.ctype {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "new LC_CTYPE ({ctype}) is incompatible with the LC_CTYPE of the template database ({})",
                        source.ctype
                    ),
                )
                .with_hint(hint("LC_CTYPE")));
            }
            if provider != source.locale_provider {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "new locale provider ({}) does not match locale provider of the template database ({})",
                        provider_name(provider),
                        provider_name(source.locale_provider)
                    ),
                )
                .with_hint(hint("locale provider")));
            }
        }

        // `get_collation_actual_version`: the builtin provider has a version, and the libc locale `C` has none.
        let actual_version = (provider == b'b').then(|| BUILTIN_VERSION.to_owned());
        if let Some(version) = &source.collation_version
            && options.collation_version.is_none()
        {
            let Some(actual) = &actual_version else {
                return Err(Error::internal(format!(
                    "template database \"{template_name}\" has a collation version, but no actual collation version could be determined"
                )));
            };
            if actual != version {
                return Err(Error::internal(format!(
                    "template database \"{template_name}\" has a collation version mismatch"
                ))
                .with_detail(format!(
                    "The template database was created using collation version {version}, but the operating system provides version {actual}."
                ))
                .with_hint(format!(
                    "Rebuild all objects in the template database that use the default collation and run ALTER DATABASE {} REFRESH COLLATION VERSION, or build PostgreSQL with the right library version.",
                    rupg_func::quote_identifier(&template_name)
                )));
            }
        }
        if collation_version.is_none() {
            collation_version = source.collation_version.clone().or(actual_version);
        }

        if encoding != UTF8 {
            unsupported.get_or_insert_with(|| {
                format!("encoding \"{}\"", encoding_to_char(i64::from(encoding)))
            });
        }
        if let Some(what) = unsupported {
            return Err(not_supported(what));
        }

        let tablespace = match text(options.tablespace)? {
            Some(name) => {
                let oid = match name.as_str() {
                    "pg_default" => DEFAULT_TABLESPACE,
                    "pg_global" => GLOBAL_TABLESPACE,
                    _ => {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("tablespace \"{name}\" does not exist"),
                        ));
                    }
                };
                if oid == GLOBAL_TABLESPACE {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "pg_global cannot be used as default tablespace",
                    ));
                }
                oid
            }
            None => source.tablespace,
        };

        let row = Database {
            oid: options.oid,
            name: name.to_owned(),
            owner,
            encoding,
            locale_provider: provider,
            is_template,
            allow_connections,
            connection_limit,
            frozen_xid: source.frozen_xid,
            min_mxid: source.min_mxid,
            tablespace,
            collate,
            ctype,
            locale,
            icu_rules,
            collation_version,
            acl: None,
        };
        self.store
            .create_database(NewDatabase { row, template: source.oid }, self.activity.number)?;
        Ok(Done::Tag(CommandTag::CreateDatabase))
    }

    /// `DropDatabase` and `dropdb`. The databases of `initdb` are static rows of `pg_database`, so the session cannot drop them yet.
    pub(super) fn drop_database(
        &mut self,
        stmt: &DropdbStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        self.prevent_block("DROP DATABASE")?;
        let mut force = false;
        for node in stmt.options.iter().flatten() {
            let Node::DefElem(def) = node else { continue };
            match def.defname.as_deref().unwrap_or_default() {
                "force" => force = true,
                name => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized DROP DATABASE option \"{name}\""),
                    )
                    .with_position(usize::try_from(def.location).ok()));
                }
            }
        }
        let name = stmt.dbname.as_deref().unwrap_or_default();
        let Some(row) = self.store.database(name) else {
            if !stmt.missing_ok {
                return Err(Error::new(
                    SqlState::UNDEFINED_DATABASE,
                    format!("database \"{name}\" does not exist"),
                ));
            }
            notices.push(Notice {
                severity: Severity::Notice,
                error: Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("database \"{name}\" does not exist, skipping"),
                ),
            });
            return Ok(Done::Tag(CommandTag::DropDatabase));
        };
        if !self.superuser() && row.owner != self.role {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of database {name}"),
            ));
        }
        if row.is_template {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, "cannot drop a template database"));
        }
        if row.oid == self.database_oid {
            return Err(Error::new(
                SqlState::OBJECT_IN_USE,
                "cannot drop the currently open database",
            ));
        }
        if row.oid < FIRST_NORMAL_OBJECT_ID {
            return Err(not_supported(format!(
                "DROP DATABASE of the database \"{name}\" of initdb"
            )));
        }
        self.store.drop_database(&row, force, self.activity.number)?;
        Ok(Done::Tag(CommandTag::DropDatabase))
    }

    /// `AlterDatabaseSet` and `AlterSetting`: the change of the stored settings of a database, which a new session of the database applies. `GUCArrayAdd` puts a new value in place of the value of the same parameter, and `GUCArrayDelete` and `RESET ALL` remove values.
    pub(super) fn alter_database_set(
        &mut self,
        stmt: &AlterDatabaseSetStmt,
    ) -> Result<Done, Error> {
        let name = stmt.dbname.as_deref().unwrap_or_default();
        let Some(row) = self.store.database(name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_DATABASE,
                format!("database \"{name}\" does not exist"),
            ));
        };
        if !self.superuser() && row.owner != self.role {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of database {name}"),
            ));
        }
        let Some(set) = &stmt.setstmt else {
            return Err(Error::internal("ALTER DATABASE SET with no setting"));
        };
        let parameter = set.name.as_deref().unwrap_or_default();
        // `ExtractSetVariableArgs`.
        let value = match set.kind {
            VariableSetKind::VAR_SET_VALUE => utility::flatten(parameter, &set.args)?,
            VariableSetKind::VAR_SET_CURRENT => Some(self.settings.borrow().show(parameter)?.1),
            _ => None,
        };
        if set.kind == VariableSetKind::VAR_RESET_ALL {
            self.store.change_database_settings(row.oid, Vec::clear);
            return Ok(Done::Tag(CommandTag::AlterDatabase));
        }
        let stored = self.settings.borrow().check_stored(parameter, value.as_deref())?;
        let prefix = format!("{stored}=");
        self.store.change_database_settings(row.oid, |settings| {
            let at = settings.iter().position(|item| item.starts_with(&prefix));
            match (value, at) {
                (Some(value), Some(at)) => settings[at] = format!("{prefix}{value}"),
                (Some(value), None) => settings.push(format!("{prefix}{value}")),
                (None, Some(at)) => {
                    settings.remove(at);
                }
                (None, None) => {}
            }
        });
        Ok(Done::Tag(CommandTag::AlterDatabase))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        assert!(matches!(check_locale("C"), Locale::Known(name) if name == "C"));
        assert!(matches!(check_locale("POSIX"), Locale::Known(name) if name == "C"));
        for name in ["en_US.utf8", "en_US.UTF-8", "C.UTF-8", "C.utf8", "de_DE@euro", "en_AG"] {
            assert!(matches!(check_locale(name), Locale::Other), "{name}");
        }
        for name in ["nosuch", "", "PG_UNICODE_FAST", "en_us", "e"] {
            assert!(matches!(check_locale(name), Locale::Invalid), "{name}");
        }
        assert_eq!(locale_encoding("en_US.UTF-8"), UTF8);
        assert_eq!(locale_encoding("C"), 0);
        assert_eq!(locale_encoding("en_AG"), -1);
    }

    #[test]
    fn server_encodings() {
        assert!(server_encoding(0));
        assert!(server_encoding(UTF8));
        assert!(!server_encoding(7));
        assert!(!server_encoding(35));
        assert!(!server_encoding(-1));
    }
}

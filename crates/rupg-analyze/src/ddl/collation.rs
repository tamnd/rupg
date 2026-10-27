//! `CREATE COLLATION`, as `DefineCollation` of `collationcmds.c` and `CollationCreate` of `pg_collation.c` run it. A collation of ICU is not supported yet.

use rupg_catalog::Collation;
use rupg_common::encoding::encoding_to_char;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{DefElem, DefineStmt, Node};

use super::drop::quote_identifier;
use super::reloptions::def_boolean;
use super::sequence::conflicting;
use super::tsearch::{def_name, def_string, qualified_name};
use super::{Definer, not_yet};
use crate::coerce::AtOpt;
use crate::typename::{names, place};

/// `PG_UTF8`: the encoding of the databases of rupg.
const UTF8: i32 = 6;

/// `PG_SQL_ASCII`.
const SQL_ASCII: i32 = 0;

/// The OID of the collation `default`.
const DEFAULT_COLLATION: u32 = 100;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message)
}

fn redundant(detail: &str) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options").with_detail(detail)
}

/// True for the names `C` and `POSIX`, which `make_libc_collator` does not give to `newlocale`.
fn c_locale(name: &str) -> bool {
    name == "C" || name == "POSIX"
}

/// `builtin_validate_locale`: the canonical name of a locale of the built-in provider, which must fit the encoding of the database.
fn builtin_locale(locale: &str) -> Result<&'static str> {
    let canonical = match locale {
        "C" => "C",
        "C.UTF-8" | "C.UTF8" => "C.UTF-8",
        "PG_UNICODE_FAST" => "PG_UNICODE_FAST",
        _ => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("invalid locale name \"{locale}\" for builtin provider"),
            ));
        }
    };
    Ok(canonical)
}

/// `builtin_locale_encoding`: -1 for `C`, which fits any encoding, and `UTF8` for the others.
fn builtin_encoding(locale: &str) -> i32 {
    if locale == "C" { -1 } else { UTF8 }
}

/// `check_encoding_locale_matches`: the encoding of each locale of the C library must be the encoding of the database, `SQL_ASCII` or unknown.
fn check_encoding_locale(collate: &str, ctype: &str) -> Result<()> {
    for (locale, setting) in [(ctype, "LC_CTYPE"), (collate, "LC_COLLATE")] {
        let encoding = rupg_platform::os::locale_encoding(locale).unwrap_or(-1);
        if encoding != UTF8 && encoding != SQL_ASCII && encoding != -1 {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("encoding \"UTF8\" does not match locale \"{locale}\""),
            )
            .with_detail(format!(
                "The chosen {setting} setting requires encoding \"{}\".",
                encoding_to_char(encoding.into())
            )));
        }
    }
    Ok(())
}

/// `make_libc_collator`: `newlocale` must know each locale that is not `C` or `POSIX`, or `report_newlocale_failure` gives the error.
fn check_libc_locales(collate: &str, ctype: &str) -> Result<()> {
    for locale in [collate, ctype] {
        if !c_locale(locale) && rupg_platform::os::locale_codeset(locale).is_none() {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("could not create locale \"{locale}\": No such file or directory"),
            )
            .with_detail(format!(
                "The operating system could not find any locale data for the locale name \"{locale}\"."
            )));
        }
    }
    Ok(())
}

/// `get_collation_actual_version`: `1` for each locale of the built-in provider, none for `C`, `POSIX` and the names that start with `C.` of the C library, and the version of the C library for the other locales of the C library.
fn actual_version(provider: u8, locale: &str) -> Option<String> {
    match provider {
        b'b' => Some("1".to_string()),
        b'c' if c_locale(locale) || locale.starts_with("C.") => None,
        b'c' => rupg_platform::os::libc_version(),
        _ => rupg_pgcatalog::icu::locales()
            .find(|icu| icu.tag == locale)
            .map(|icu| icu.version.to_string()),
    }
}

/// A row of `pg_collation` that `pg_collation.dat` has.
fn builtin_row(oid: u32) -> Option<Collation> {
    let (name, provider, encoding, collate, locale) = match oid {
        100 => ("default", b'd', -1, None, None),
        950 => ("C", b'c', -1, Some("C"), None),
        951 => ("POSIX", b'c', -1, Some("POSIX"), None),
        962 => ("ucs_basic", b'b', UTF8, None, Some("C")),
        963 => ("unicode", b'i', -1, None, Some("und")),
        811 => ("pg_c_utf8", b'b', UTF8, None, Some("C.UTF-8")),
        6411 => ("pg_unicode_fast", b'b', UTF8, None, Some("PG_UNICODE_FAST")),
        _ => return None,
    };
    Some(Collation {
        oid,
        name: name.to_string(),
        namespace: crate::PG_CATALOG_NAMESPACE,
        owner: 10,
        provider,
        deterministic: true,
        encoding,
        libc_locale: collate.map(str::to_string),
        ctype: collate.map(str::to_string),
        locale: locale.map(str::to_string),
        icu_rules: None,
        version: None,
    })
}

/// The options of `CREATE COLLATION`, one `DefElem` for each.
#[derive(Default)]
struct Options<'n> {
    from: Option<&'n DefElem>,
    locale: Option<&'n DefElem>,
    lc_collate: Option<&'n DefElem>,
    lc_ctype: Option<&'n DefElem>,
    provider: Option<&'n DefElem>,
    deterministic: Option<&'n DefElem>,
    rules: Option<&'n DefElem>,
    version: Option<&'n DefElem>,
}

impl Definer<'_, '_> {
    /// `DefineCollation`: the check of the schema, the options, the collation that `FROM` copies or the collation of the provider, and then `CollationCreate`.
    pub(super) fn create_collation(&mut self, stmt: &DefineStmt) -> Result<()> {
        let list = names(&stmt.defnames);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let defs: Vec<&DefElem> = stmt
            .definition
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::DefElem(def) => Some(&**def),
                _ => None,
            })
            .collect();
        let mut options = Options::default();
        for &def in &defs {
            let slot = match def_name(def) {
                "from" => &mut options.from,
                "locale" => &mut options.locale,
                "lc_collate" => &mut options.lc_collate,
                "lc_ctype" => &mut options.lc_ctype,
                "provider" => &mut options.provider,
                "deterministic" => &mut options.deterministic,
                "rules" => &mut options.rules,
                "version" => &mut options.version,
                other => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("collation attribute \"{other}\" not recognized"),
                    )
                    .at_opt(place(def.location)));
                }
            };
            if slot.is_some() {
                return Err(conflicting(def));
            }
            *slot = Some(def);
        }
        if options.locale.is_some() && (options.lc_collate.is_some() || options.lc_ctype.is_some())
        {
            return Err(redundant(
                "LOCALE cannot be specified together with LC_COLLATE or LC_CTYPE.",
            ));
        }
        if options.from.is_some() && defs.len() != 1 {
            return Err(redundant("FROM cannot be specified together with any other options."));
        }
        let mut new = match options.from {
            Some(from) => self.copied_collation(from)?,
            None => self.provider_collation(&options)?,
        };
        if new.version.is_none() {
            let locale = if new.provider == b'c' { &new.libc_locale } else { &new.locale };
            new.version = actual_version(new.provider, locale.as_deref().unwrap_or_default());
        }
        new.name = name.to_string();
        new.namespace = namespace;
        new.owner = self.user;
        if !self.check_collation_name(&new, stmt.if_not_exists)? {
            return Ok(());
        }
        if new.provider == b'c' {
            let collate = new.libc_locale.as_deref().unwrap_or_default();
            check_libc_locales(collate, new.ctype.as_deref().unwrap_or_default())?;
        }
        self.catalog.create_user_collation(new);
        Ok(())
    }

    /// The collation that `FROM` copies, without its version. The collation `default` cannot be copied.
    fn copied_collation(&self, from: &DefElem) -> Result<Collation> {
        let parts = qualified_name(from)?;
        let oid = self.collation_lookup(&parts)?;
        let row = self.catalog.collation(oid).cloned().or_else(|| builtin_row(oid));
        let mut row = row.ok_or_else(|| Error::internal("a collation"))?;
        if row.provider == b'd' || oid == DEFAULT_COLLATION {
            return Err(invalid("collation \"default\" cannot be copied"));
        }
        row.version = None;
        Ok(row)
    }

    /// The collation of `PROVIDER`, `LOCALE`, `LC_COLLATE`, `LC_CTYPE`, `DETERMINISTIC`, `RULES` and `VERSION`.
    fn provider_collation(&self, options: &Options<'_>) -> Result<Collation> {
        let deterministic = match options.deterministic {
            Some(def) => def_boolean(def_name(def), def.arg.as_ref())?,
            None => true,
        };
        let rules = options.rules.map(def_string).transpose()?;
        let version = options.version.map(def_string).transpose()?;
        let provider = match options.provider.map(def_string).transpose()? {
            None => b'c',
            Some(name) => match name.to_ascii_lowercase().as_str() {
                "builtin" => b'b',
                "icu" => b'i',
                "libc" => b'c',
                _ => return Err(invalid(format!("unrecognized collation provider: {name}"))),
            },
        };
        let mut collate = None;
        let mut ctype = None;
        let mut locale = None;
        if let Some(def) = options.locale {
            let value = def_string(def)?;
            if provider == b'c' {
                collate = Some(value.clone());
                ctype = Some(value);
            } else {
                locale = Some(value);
            }
        }
        if let Some(def) = options.lc_collate {
            collate = Some(def_string(def)?);
        }
        if let Some(def) = options.lc_ctype {
            ctype = Some(def_string(def)?);
        }
        let missing = |name: &str| invalid(format!("parameter \"{name}\" must be specified"));
        match provider {
            b'b' => {
                let given = locale.as_deref().ok_or_else(|| missing("locale"))?;
                // The databases of rupg are in `UTF8`, so each locale of the built-in provider fits the encoding.
                locale = Some(builtin_locale(given)?.to_string());
            }
            b'c' => {
                if collate.is_none() {
                    return Err(missing("lc_collate"));
                }
                if ctype.is_none() {
                    return Err(missing("lc_ctype"));
                }
            }
            _ => {
                if locale.is_none() {
                    return Err(missing("locale"));
                }
                return Err(not_yet("CREATE COLLATION with the provider icu", None));
            }
        }
        if !deterministic {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "nondeterministic collations not supported with this provider",
            ));
        }
        if rules.is_some() {
            return Err(invalid("ICU rules cannot be specified unless locale provider is ICU"));
        }
        let encoding = match provider {
            b'b' => builtin_encoding(locale.as_deref().unwrap_or_default()),
            _ => {
                let collate = collate.as_deref().unwrap_or_default();
                check_encoding_locale(collate, ctype.as_deref().unwrap_or_default())?;
                UTF8
            }
        };
        Ok(Collation {
            oid: 0,
            name: String::new(),
            namespace: 0,
            owner: 0,
            provider,
            deterministic,
            encoding,
            libc_locale: collate,
            ctype,
            locale,
            icu_rules: rules,
            version,
        })
    }

    /// The checks of `CollationCreate` on the name: no collation of the schema has the name for the same encoding, and a collation for any encoding and one for the encoding of the database cannot have the same name. With `IF NOT EXISTS` a name that is taken gives a notice and false.
    fn check_collation_name(&mut self, new: &Collation, if_not_exists: bool) -> Result<bool> {
        let taken = |encoding: i32| {
            let user = self.catalog.collations().any(|c| {
                c.namespace == new.namespace && c.name == new.name && c.encoding == encoding
            });
            user || builtin::named(Named::Collation).iter().any(|r| {
                r.namespace == new.namespace && r.name == new.name && r.encoding == encoding
            })
        };
        let name = &new.name;
        let message = if taken(new.encoding) {
            match new.encoding {
                -1 => format!("collation \"{name}\" already exists"),
                encoding => format!(
                    "collation \"{name}\" for encoding \"{}\" already exists",
                    encoding_to_char(encoding.into())
                ),
            }
        } else if taken(if new.encoding == -1 { UTF8 } else { -1 }) {
            format!("collation \"{name}\" already exists")
        } else {
            return Ok(true);
        };
        if if_not_exists {
            self.notice(SqlState::DUPLICATE_OBJECT, format!("{message}, skipping"));
            return Ok(false);
        }
        Err(Error::new(SqlState::DUPLICATE_OBJECT, message))
    }

    /// `get_collation_oid`: the collation of a name, in the schema of the name or in the first schema of the path that has one.
    pub(super) fn collation_lookup(&self, parts: &[&str]) -> Result<u32> {
        let (schema, name) = self.an.split_name(parts, None)?;
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        path.iter().find_map(|&namespace| self.collation_in(namespace, name)).ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("collation \"{}\" for encoding \"UTF8\" does not exist", parts.join(".")),
            )
        })
    }

    /// `getObjectDescription` for a collation: the name is qualified when the collation is not visible on the search path.
    pub(super) fn describe_collation(&self, oid: u32) -> String {
        let Some(collation) = self.catalog.collation(oid) else {
            return format!("collation {oid}");
        };
        let name = quote_identifier(&collation.name);
        let first = self.an.path.iter().copied().find(|&ns| {
            ns == collation.namespace || self.collation_in(ns, &collation.name).is_some()
        });
        if first == Some(collation.namespace) {
            return format!("collation {name}");
        }
        format!("collation {}.{name}", quote_identifier(&self.schema_name(collation.namespace)))
    }
}

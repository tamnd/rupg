//! The objects of text search: the templates of `pg_ts_template`, the parsers of `pg_ts_parser`, the dictionaries of `pg_ts_dict`, and the configurations of `pg_ts_config` with their rows of `pg_ts_config_map`. They change as `DefineTSTemplate`, `DefineTSParser`, `DefineTSDictionary`, `DefineTSConfiguration` and `AlterTSConfiguration` of `tsearchcmds.c` change them, with the same rows of `pg_depend`.

use rupg_common::{Error, Object, Result, SqlState};

use super::{Catalog, DepKind, ObjRef, PG_NAMESPACE, PG_PROC};

/// The OID of `pg_ts_template`.
pub const PG_TS_TEMPLATE: u32 = 3764;
/// The OID of `pg_ts_dict`.
pub const PG_TS_DICT: u32 = 3600;
/// The OID of `pg_ts_parser`.
pub const PG_TS_PARSER: u32 = 3601;
/// The OID of `pg_ts_config`.
pub const PG_TS_CONFIG: u32 = 3602;

/// A text search template, one row of `pg_ts_template`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsTemplate {
    /// The OID. [`Catalog::create_ts_template`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// `tmplinit`: the OID of the init function, or 0.
    pub init: u32,
    /// `tmpllexize`: the OID of the lexize function.
    pub lexize: u32,
}

/// A text search parser, one row of `pg_ts_parser`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsParser {
    /// The OID. [`Catalog::create_ts_parser`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// `prsstart`: the OID of the start function.
    pub start: u32,
    /// `prstoken`: the OID of the function that gives the next token.
    pub token: u32,
    /// `prsend`: the OID of the end function.
    pub end: u32,
    /// `prsheadline`: the OID of the headline function, or 0.
    pub headline: u32,
    /// `prslextype`: the OID of the function that gives the token types.
    pub lextype: u32,
}

/// A text search dictionary, one row of `pg_ts_dict`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsDictionary {
    /// The OID. [`Catalog::create_ts_dictionary`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// `dicttemplate`: the OID of the template.
    pub template: u32,
    /// `dictinitoption`: the options in the form of `serialize_deflist`, or `None` for no option.
    pub options: Option<String>,
}

/// A text search configuration, one row of `pg_ts_config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsConfig {
    /// The OID. [`Catalog::create_ts_config`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// `cfgparser`: the OID of the parser.
    pub parser: u32,
}

/// One row of `pg_ts_config_map`: for a token type of a configuration, the dictionary with this place in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsMapping {
    /// `mapcfg`: the OID of the configuration.
    pub config: u32,
    /// `maptokentype`: the number of the token type.
    pub token: i32,
    /// `mapseqno`: the place of the dictionary in the list of the token type, from 1.
    pub seqno: i32,
    /// `mapdict`: the OID of the dictionary.
    pub dict: u32,
}

/// The error of the unique index on the name and the schema of a catalog of text search, such as `pg_ts_dict_dictname_index` on `dictname` and `dictnamespace`. The prefix of the columns is `prefix`.
fn duplicate(catalog: &str, prefix: &str, name: &str, namespace: u32) -> Error {
    Error::new(
        SqlState::UNIQUE_VIOLATION,
        format!("duplicate key value violates unique constraint \"{catalog}_{prefix}name_index\""),
    )
    .with_detail(format!(
        "Key ({prefix}name, {prefix}namespace)=({name}, {namespace}) already exists."
    ))
    .with_object(Object::Schema, "pg_catalog")
    .with_object(Object::Table, catalog)
    .with_object(Object::Constraint, format!("{catalog}_{prefix}name_index"))
}

impl Catalog {
    /// `DefineTSTemplate`: a new template, which gets the next OID. It depends on its schema, its lexize function and its init function.
    ///
    /// # Errors
    ///
    /// `23505` when the schema has a template with the name.
    pub fn create_ts_template(&mut self, mut new: TsTemplate) -> Result<u32> {
        let oid = self.new_oid();
        if self.ts_template_by_name(new.namespace, &new.name).is_some() {
            return Err(duplicate("pg_ts_template", "tmpl", &new.name, new.namespace));
        }
        new.oid = oid;
        let mut refs =
            vec![ObjRef::new(PG_NAMESPACE, new.namespace), ObjRef::new(PG_PROC, new.lexize)];
        if new.init != 0 {
            refs.push(ObjRef::new(PG_PROC, new.init));
        }
        self.depend_all(ObjRef::new(PG_TS_TEMPLATE, oid), refs);
        self.ts_templates.insert(oid, new);
        Ok(oid)
    }

    /// `DefineTSParser`: a new parser, which gets the next OID. It depends on its schema and its functions.
    ///
    /// # Errors
    ///
    /// `23505` when the schema has a parser with the name.
    pub fn create_ts_parser(&mut self, mut new: TsParser) -> Result<u32> {
        let oid = self.new_oid();
        if self.ts_parser_by_name(new.namespace, &new.name).is_some() {
            return Err(duplicate("pg_ts_parser", "prs", &new.name, new.namespace));
        }
        new.oid = oid;
        let mut refs = vec![ObjRef::new(PG_NAMESPACE, new.namespace)];
        for function in [new.start, new.token, new.end, new.lextype, new.headline] {
            if function != 0 {
                refs.push(ObjRef::new(PG_PROC, function));
            }
        }
        self.depend_all(ObjRef::new(PG_TS_PARSER, oid), refs);
        self.ts_parsers.insert(oid, new);
        Ok(oid)
    }

    /// `DefineTSDictionary`: a new dictionary, which gets the next OID. It depends on its schema and on its template.
    ///
    /// # Errors
    ///
    /// `23505` when the schema has a dictionary with the name.
    pub fn create_ts_dictionary(&mut self, mut new: TsDictionary) -> Result<u32> {
        let oid = self.new_oid();
        if self.ts_dictionary_by_name(new.namespace, &new.name).is_some() {
            return Err(duplicate("pg_ts_dict", "dict", &new.name, new.namespace));
        }
        new.oid = oid;
        let refs = vec![
            ObjRef::new(PG_NAMESPACE, new.namespace),
            ObjRef::new(PG_TS_TEMPLATE, new.template),
        ];
        self.depend_all(ObjRef::new(PG_TS_DICT, oid), refs);
        self.ts_dictionaries.insert(oid, new);
        Ok(oid)
    }

    /// `DefineTSConfiguration`: a new configuration, which gets the next OID, with a copy of the rows of `pg_ts_config_map` of `copy`.
    ///
    /// # Errors
    ///
    /// `23505` when the schema has a configuration with the name.
    pub fn create_ts_config(&mut self, mut new: TsConfig, copy: &[TsMapping]) -> Result<u32> {
        let oid = self.new_oid();
        if self.ts_config_by_name(new.namespace, &new.name).is_some() {
            return Err(duplicate("pg_ts_config", "cfg", &new.name, new.namespace));
        }
        new.oid = oid;
        self.ts_configs.insert(oid, new);
        self.ts_mappings.extend(copy.iter().map(|m| TsMapping { config: oid, ..*m }));
        self.ts_config_depends(oid);
        Ok(oid)
    }

    /// `makeConfigurationDependencies`: the dependencies of a configuration on its schema, its parser and the dictionaries of its map. The old dependencies go first.
    fn ts_config_depends(&mut self, oid: u32) {
        let Some(config) = self.ts_configs.get(&oid) else { return };
        let me = ObjRef::new(PG_TS_CONFIG, oid);
        let mut refs = vec![
            ObjRef::new(PG_NAMESPACE, config.namespace),
            ObjRef::new(PG_TS_PARSER, config.parser),
        ];
        refs.extend(
            self.ts_mappings
                .iter()
                .filter(|m| m.config == oid)
                .map(|m| ObjRef::new(PG_TS_DICT, m.dict)),
        );
        self.depends.retain(|d| d.object != me || d.kind == DepKind::Extension);
        self.depend_all(me, refs);
    }

    /// `record_object_address_dependencies`: the normal dependencies on the objects without duplicates, in the order of `object_address_comparator`. The OIDs go down, then the OIDs of the catalogs go up.
    pub(crate) fn depend_all(&mut self, me: ObjRef, mut refs: Vec<ObjRef>) {
        refs.sort_unstable_by(|a, b| {
            b.oid.cmp(&a.oid).then(a.class.cmp(&b.class)).then(a.sub.cmp(&b.sub))
        });
        refs.dedup();
        for referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
    }

    /// The `ADD MAPPING` and `ALTER MAPPING` parts of `MakeConfigurationMapping`. With `replace`, the rows of the token types go first. Then each token type gets the dictionaries in order.
    pub fn add_ts_mapping(&mut self, config: u32, tokens: &[i32], dicts: &[u32], replace: bool) {
        if replace {
            self.ts_mappings.retain(|m| m.config != config || !tokens.contains(&m.token));
        }
        for &token in tokens {
            for (seqno, &dict) in (1..).zip(dicts) {
                self.ts_mappings.push(TsMapping { config, token, seqno, dict });
            }
        }
        self.ts_config_depends(config);
    }

    /// The `ALTER MAPPING REPLACE` part of `MakeConfigurationMapping`: each row of the token types, or of all token types when `tokens` is empty, that has the dictionary `old` gets the dictionary `new`.
    pub fn replace_ts_dictionary(&mut self, config: u32, tokens: &[i32], old: u32, new: u32) {
        for m in &mut self.ts_mappings {
            if m.config == config
                && m.dict == old
                && (tokens.is_empty() || tokens.contains(&m.token))
            {
                m.dict = new;
            }
        }
        self.ts_config_depends(config);
    }

    /// `DropConfigurationMapping` for one token type: false when the configuration has no row for it.
    pub fn drop_ts_mapping(&mut self, config: u32, token: i32) -> bool {
        let before = self.ts_mappings.len();
        self.ts_mappings.retain(|m| m.config != config || m.token != token);
        self.ts_mappings.len() != before
    }

    /// Updates the dependencies of a configuration after [`Catalog::drop_ts_mapping`].
    pub fn update_ts_config_depends(&mut self, config: u32) {
        self.ts_config_depends(config);
    }

    /// The templates, in the order of their OIDs.
    pub fn ts_templates(&self) -> impl Iterator<Item = &TsTemplate> {
        self.ts_templates.values()
    }

    /// The template with the OID.
    pub fn ts_template(&self, oid: u32) -> Option<&TsTemplate> {
        self.ts_templates.get(&oid)
    }

    /// The template with the name in the schema.
    pub fn ts_template_by_name(&self, namespace: u32, name: &str) -> Option<&TsTemplate> {
        self.ts_templates.values().find(|t| t.namespace == namespace && t.name == name)
    }

    /// The parsers, in the order of their OIDs.
    pub fn ts_parsers(&self) -> impl Iterator<Item = &TsParser> {
        self.ts_parsers.values()
    }

    /// The parser with the OID.
    pub fn ts_parser(&self, oid: u32) -> Option<&TsParser> {
        self.ts_parsers.get(&oid)
    }

    /// The parser with the name in the schema.
    pub fn ts_parser_by_name(&self, namespace: u32, name: &str) -> Option<&TsParser> {
        self.ts_parsers.values().find(|p| p.namespace == namespace && p.name == name)
    }

    /// The dictionaries, in the order of their OIDs.
    pub fn ts_dictionaries(&self) -> impl Iterator<Item = &TsDictionary> {
        self.ts_dictionaries.values()
    }

    /// The dictionary with the OID.
    pub fn ts_dictionary(&self, oid: u32) -> Option<&TsDictionary> {
        self.ts_dictionaries.get(&oid)
    }

    /// The dictionary with the name in the schema.
    pub fn ts_dictionary_by_name(&self, namespace: u32, name: &str) -> Option<&TsDictionary> {
        self.ts_dictionaries.values().find(|d| d.namespace == namespace && d.name == name)
    }

    /// The configurations, in the order of their OIDs.
    pub fn ts_configs(&self) -> impl Iterator<Item = &TsConfig> {
        self.ts_configs.values()
    }

    /// The configuration with the OID.
    pub fn ts_config(&self, oid: u32) -> Option<&TsConfig> {
        self.ts_configs.get(&oid)
    }

    /// The configuration with the name in the schema.
    pub fn ts_config_by_name(&self, namespace: u32, name: &str) -> Option<&TsConfig> {
        self.ts_configs.values().find(|c| c.namespace == namespace && c.name == name)
    }

    /// The rows of `pg_ts_config_map`, in the order that they were made.
    pub fn ts_mappings(&self) -> &[TsMapping] {
        &self.ts_mappings
    }

    /// The name and the schema of the object of text search of the class `class`, for a change.
    fn ts_name_namespace(&mut self, class: u32, oid: u32) -> Option<(&mut String, &mut u32)> {
        match class {
            PG_TS_TEMPLATE => {
                self.ts_templates.get_mut(&oid).map(|t| (&mut t.name, &mut t.namespace))
            }
            PG_TS_PARSER => self.ts_parsers.get_mut(&oid).map(|p| (&mut p.name, &mut p.namespace)),
            PG_TS_DICT => {
                self.ts_dictionaries.get_mut(&oid).map(|d| (&mut d.name, &mut d.namespace))
            }
            PG_TS_CONFIG => self.ts_configs.get_mut(&oid).map(|c| (&mut c.name, &mut c.namespace)),
            _ => None,
        }
    }

    /// `AlterObjectRename_internal` for an object of text search: the new name of the object of the class `class`.
    pub fn rename_ts(&mut self, class: u32, oid: u32, name: &str) {
        if let Some((old, _)) = self.ts_name_namespace(class, oid) {
            *old = name.to_string();
        }
    }

    /// `AlterObjectNamespace_internal` for an object of text search: the new schema of the object of the class `class`, and the dependency on the new schema.
    pub fn set_ts_namespace(&mut self, class: u32, oid: u32, namespace: u32) {
        let old =
            self.ts_name_namespace(class, oid).map(|(_, ns)| std::mem::replace(ns, namespace));
        if let Some(old) = old {
            self.change_namespace_dependency(ObjRef::new(class, oid), old, namespace);
        }
    }
}

rupg_common::persist_struct!(TsTemplate { oid, name, namespace, init, lexize });
rupg_common::persist_struct!(TsParser {
    oid,
    name,
    namespace,
    start,
    token,
    end,
    headline,
    lextype
});
rupg_common::persist_struct!(TsDictionary { oid, name, namespace, owner, template, options });
rupg_common::persist_struct!(TsConfig { oid, name, namespace, owner, parser });
rupg_common::persist_struct!(TsMapping { config, token, seqno, dict });

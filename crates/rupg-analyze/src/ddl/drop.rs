//! `DROP TABLE`, `DROP VIEW`, `DROP INDEX`, `DROP SEQUENCE`, `DROP SCHEMA`, `DROP TYPE`, `DROP DOMAIN`, `DROP FUNCTION`, `DROP TEXT SEARCH`, `DROP OPERATOR` and `DROP STATISTICS`, as `RemoveRelations` and `RemoveObjects` run them. The statement finds all its objects first, then [`Catalog::deletions`] finds the objects that depend on them, and the report of `reportDependentObjects` names these objects as `getObjectDescription` does. A built-in object that is not pinned, such as the schema `public` or a view of `information_schema`, gives `0A000`.

use rupg_catalog::{
    FIRST_NORMAL_OID, ObjRef, PG_AM, PG_AMOP, PG_AMPROC, PG_ATTRDEF, PG_CAST, PG_CLASS,
    PG_COLLATION, PG_CONSTRAINT, PG_CONVERSION, PG_EVENT_TRIGGER, PG_FOREIGN_DATA_WRAPPER,
    PG_FOREIGN_SERVER, PG_LANGUAGE, PG_LARGEOBJECT, PG_NAMESPACE, PG_OPCLASS, PG_OPERATOR,
    PG_OPFAMILY, PG_POLICY, PG_PROC, PG_PUBLICATION, PG_PUBLICATION_NAMESPACE, PG_PUBLICATION_REL,
    PG_REWRITE, PG_STATISTIC_EXT, PG_TRIGGER, PG_TS_CONFIG, PG_TS_DICT, PG_TS_PARSER,
    PG_TS_TEMPLATE, PG_TYPE, PG_USER_MAPPING, PUBLIC, Refusal, RelKind, Target, flag, is_pinned,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{DropBehavior, DropStmt, Node, ObjectType, RangeVar};
use rupg_sql::{Category, keyword};
use rupg_types::{TypeInfo, format_type};

use super::comment::{name_list, not_owner, range_var};
use super::{Definer, Found, PG_TOAST_NAMESPACE, not_yet, qualified, relname};
use crate::typename::type_name_text;

/// `BOOTSTRAP_SUPERUSERID`: the owner of the built-in types.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// `MAX_REPORTED_DEPS`: the number of objects that the detail of the report names.
const MAX_REPORTED: usize = 100;

/// A line of `dropmsgstringarray`: the messages of `DropErrorMsgNonExistent` and `DropErrorMsgWrongType` for one `relkind`.
struct Kind {
    code: u8,
    missing_state: SqlState,
    word: &'static str,
    article: &'static str,
    hint: &'static str,
}

/// `dropmsgstringarray`, without the kinds that rupg does not have.
const KINDS: [Kind; 7] = [
    Kind {
        code: b'r',
        missing_state: SqlState::UNDEFINED_TABLE,
        word: "table",
        article: "a table",
        hint: "Use DROP TABLE to remove a table.",
    },
    Kind {
        code: b'S',
        missing_state: SqlState::UNDEFINED_TABLE,
        word: "sequence",
        article: "a sequence",
        hint: "Use DROP SEQUENCE to remove a sequence.",
    },
    Kind {
        code: b'v',
        missing_state: SqlState::UNDEFINED_TABLE,
        word: "view",
        article: "a view",
        hint: "Use DROP VIEW to remove a view.",
    },
    Kind {
        code: b'i',
        missing_state: SqlState::UNDEFINED_OBJECT,
        word: "index",
        article: "an index",
        hint: "Use DROP INDEX to remove an index.",
    },
    Kind {
        code: b'c',
        missing_state: SqlState::UNDEFINED_OBJECT,
        word: "type",
        article: "a type",
        hint: "Use DROP TYPE to remove a type.",
    },
    Kind {
        code: b'm',
        missing_state: SqlState::UNDEFINED_TABLE,
        word: "materialized view",
        article: "a materialized view",
        hint: "Use DROP MATERIALIZED VIEW to remove a materialized view.",
    },
    Kind {
        code: b'f',
        missing_state: SqlState::UNDEFINED_OBJECT,
        word: "foreign table",
        article: "a foreign table",
        hint: "Use DROP FOREIGN TABLE to remove a foreign table.",
    },
];

/// The line of [`KINDS`] for a `relkind`. A partitioned table counts as a table and a partitioned index as an index.
fn kind(code: u8) -> Option<&'static Kind> {
    let code = match code {
        b'p' => b'r',
        b'I' => b'i',
        code => code,
    };
    KINDS.iter().find(|k| k.code == code)
}

/// The word of `get_relkind_objtype` in the error of `aclcheck_error` with `ACLCHECK_NOT_OWNER`.
pub(super) fn owner_word(code: u8) -> &'static str {
    match code {
        b'i' | b'I' => "index",
        b'S' => "sequence",
        b'v' => "view",
        b'm' => "materialized view",
        b'c' => "type",
        b'f' => "foreign table",
        _ => "table",
    }
}

/// The word of `getRelationDescription` for a `relkind`. A partitioned table is a table, and a partitioned index is an index.
fn relation_word(code: u8) -> &'static str {
    match code {
        b'i' | b'I' => "index",
        b'S' => "sequence",
        b't' => "toast table",
        b'v' => "view",
        b'm' => "materialized view",
        b'c' => "composite type",
        b'f' => "foreign table",
        _ => "table",
    }
}

/// `quote_identifier` of `ruleutils.c`: the name in double quotes unless it is a simple name in lower case that is not a key word, or that is an unreserved key word.
pub(crate) fn quote_identifier(name: &str) -> String {
    let simple = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && keyword(name).is_none_or(|k| k.category == Category::Unreserved);
    if simple { name.to_owned() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// The error for a built-in object that is not pinned, which rupg cannot remove.
pub(super) fn builtin_object() -> Error {
    not_yet("DROP of a built-in object", None)
}

impl Definer<'_, '_> {
    /// `ExecDropStmt` for the kinds of objects that rupg can remove. The caller runs `PreventInTransactionBlock` for `DROP INDEX CONCURRENTLY`.
    pub(super) fn drop(&mut self, stmt: &DropStmt) -> Result<()> {
        let objects = match stmt.removeType {
            ObjectType::OBJECT_SCHEMA => self.find_schemas(stmt)?,
            ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN => self.find_types(stmt)?,
            ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_FOREIGN_TABLE => self.find_relations(stmt)?,
            ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSCONFIGURATION => self.find_ts(stmt)?,
            ObjectType::OBJECT_OPERATOR => self.find_operators(stmt)?,
            ObjectType::OBJECT_STATISTIC_EXT => self.find_stats(stmt)?,
            ObjectType::OBJECT_OPCLASS | ObjectType::OBJECT_OPFAMILY => self.find_opcf(stmt)?,
            ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE => self.find_functions(stmt)?,
            ObjectType::OBJECT_RULE => return self.drop_rule(stmt),
            ObjectType::OBJECT_POLICY => return self.drop_policy(stmt),
            ObjectType::OBJECT_TRIGGER => return self.drop_trigger(stmt),
            ObjectType::OBJECT_ACCESS_METHOD
            | ObjectType::OBJECT_CAST
            | ObjectType::OBJECT_COLLATION
            | ObjectType::OBJECT_CONVERSION
            | ObjectType::OBJECT_EXTENSION
            | ObjectType::OBJECT_LANGUAGE
            | ObjectType::OBJECT_FDW
            | ObjectType::OBJECT_FOREIGN_SERVER
            | ObjectType::OBJECT_EVENT_TRIGGER
            | ObjectType::OBJECT_PUBLICATION => return self.drop_named(stmt),
            _ => return Err(not_yet("DROP of this kind of object", None)),
        };
        self.perform(&objects, stmt.behavior == DropBehavior::DROP_CASCADE)
    }

    /// The relations of `RemoveRelations`, with the checks of `RangeVarCallbackForDropRelation`.
    fn find_relations(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        if stmt.concurrent {
            if stmt.objects.len() != 1 {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
                ));
            }
            if stmt.behavior == DropBehavior::DROP_CASCADE {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "DROP INDEX CONCURRENTLY does not support CASCADE",
                ));
            }
        }
        let expected = match stmt.removeType {
            ObjectType::OBJECT_INDEX => &KINDS[3],
            ObjectType::OBJECT_SEQUENCE => &KINDS[1],
            ObjectType::OBJECT_VIEW => &KINDS[2],
            ObjectType::OBJECT_MATVIEW => &KINDS[5],
            ObjectType::OBJECT_FOREIGN_TABLE => &KINDS[6],
            _ => &KINDS[0],
        };
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let rv = range_var(&name_list(Some(item)))?;
            let Some(found) = self.find_relation(&rv)? else {
                self.missing_relation(&rv, expected, stmt.missing_ok)?;
                continue;
            };
            let name = relname(&rv);
            let code = self.kind_of(found);
            if kind(code).is_none_or(|k| k.code != expected.code) {
                let error = Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is not {}", expected.article),
                );
                return Err(match kind(code) {
                    Some(actual) => error.with_hint(actual.hint),
                    None => error,
                });
            }
            let (owner, namespace) = match found {
                Found::User(oid) => {
                    self.catalog.relation(oid).map_or((0, 0), |r| (r.owner, r.namespace))
                }
                Found::Builtin(row) => (row.owner, row.namespace),
            };
            // `RemoveRelations` marks a drop of a temporary table.
            self.an.note_temp(namespace);
            if !self.is_superuser()
                && !self.owns(owner)
                && !self.schema_owner(namespace).is_some_and(|o| self.owns(o))
            {
                return Err(not_owner(owner_word(code), name));
            }
            if self.is_system(found) && !self.an.env.allow_system_table_mods() {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied: \"{name}\" is a system catalog"),
                ));
            }
            // `RemoveRelations`: the drop of a partitioned index can not be concurrent. The drop of a temporary index is never concurrent.
            if stmt.concurrent
                && !self.catalog.is_temp_schema(namespace)
                && let Found::User(oid) = found
                && self.catalog.relation(oid).is_some_and(|r| r.kind == RelKind::PartitionedIndex)
            {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot drop partitioned index \"{name}\" concurrently"),
                ));
            }
            match found {
                Found::User(oid) if oid >= FIRST_NORMAL_OID => {
                    objects.push(ObjRef::new(PG_CLASS, oid));
                }
                _ => return Err(builtin_object()),
            }
        }
        Ok(objects)
    }

    /// `RangeVarGetRelidExtended` with `RVR_MISSING_OK`: the relation of a name, or `None` when the schema or the relation does not exist.
    fn find_relation(&self, rv: &RangeVar) -> Result<Option<Found>> {
        if let Some(catalog) = rv.catalogname.as_deref()
            && catalog != self.an.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cross-database references are not implemented: \"{}\"", qualified(rv)),
            ));
        }
        let name = relname(rv);
        Ok(match rv.schemaname.as_deref() {
            Some(schema) => {
                self.an.explicit_schema(schema, None)?.and_then(|ns| self.relation_in(ns, name))
            }
            None => self.an.temp_path.iter().find_map(|&ns| self.relation_in(ns, name)),
        })
    }

    /// `DropErrorMsgNonExistent`: the error, or with `IF EXISTS` the notice, for a name that gives no relation.
    fn missing_relation(&mut self, rv: &RangeVar, expected: &Kind, missing_ok: bool) -> Result<()> {
        let (state, message) = match rv.schemaname.as_deref() {
            Some(schema) if self.an.schema(schema).is_none() => {
                (SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
            }
            _ => (
                expected.missing_state,
                format!("{} \"{}\" does not exist", expected.word, relname(rv)),
            ),
        };
        if !missing_ok {
            return Err(Error::new(state, message));
        }
        self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
        Ok(())
    }

    /// The schemas of `RemoveObjects`, with the check of `check_object_ownership`.
    fn find_schemas(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let name = match item {
                Node::String(name) => name.as_ref(),
                _ => "",
            };
            let oid = match self.catalog.schema_by_name(name) {
                Some(schema) => Some(schema.oid),
                None => self.builtin_schema(name).map(|n| n.oid),
            };
            let Some(oid) = oid else {
                let message = format!("schema \"{name}\" does not exist");
                if !stmt.missing_ok {
                    return Err(Error::new(SqlState::UNDEFINED_SCHEMA, message));
                }
                self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
                continue;
            };
            if !self.is_superuser() && !self.schema_owner(oid).is_some_and(|o| self.owns(o)) {
                return Err(not_owner("schema", name));
            }
            let object = ObjRef::new(PG_NAMESPACE, oid);
            // The schema `public` is an ordinary object of `initdb`, which `DROP` can remove.
            if oid < FIRST_NORMAL_OID && !is_pinned(object) && oid != PUBLIC {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// The types of `RemoveObjects`, with the checks of `get_object_address_type` and `check_object_ownership`. `DROP DOMAIN` accepts only a domain.
    fn find_types(&mut self, stmt: &DropStmt) -> Result<Vec<ObjRef>> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let Node::TypeName(name) = item else {
                return Err(Error::internal("a type name"));
            };
            let found = match self.an.lookup_type_name(name) {
                Err(error) if stmt.missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                    self.notice(
                        SqlState::SUCCESSFUL_COMPLETION,
                        format!("{}, skipping", error.message()),
                    );
                    continue;
                }
                found => found?,
            };
            let Some(oid) = found else {
                let message = format!("type \"{}\" does not exist", type_name_text(name));
                if !stmt.missing_ok {
                    return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
                }
                self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
                continue;
            };
            let (kind, owner) = match self.catalog.type_by_oid(oid) {
                Some(ty) if ty.domain.is_some() => (b'd', ty.owner),
                Some(ty) if ty.relation != 0 => (b'c', ty.owner),
                Some(ty) => (b'b', ty.owner),
                None => {
                    (builtin::type_by_oid(oid).map_or(b'b', |row| row.kind), BOOTSTRAP_SUPERUSER)
                }
            };
            if stmt.removeType == ObjectType::OBJECT_DOMAIN && kind != b'd' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a domain", type_name_text(name)),
                ));
            }
            if !self.is_superuser() && !self.owns(owner) {
                let word = if kind == b'd' { "domain" } else { "type" };
                return Err(not_owner(word, &self.type_text(oid)));
            }
            let object = ObjRef::new(PG_TYPE, oid);
            if oid < FIRST_NORMAL_OID && !is_pinned(object) {
                return Err(builtin_object());
            }
            objects.push(object);
        }
        Ok(objects)
    }

    /// `DISCARD TEMP`, as `ResetTempTableNamespace` runs it: the objects of the temporary schema of the session go, with the objects that depend on them, and with no notice. The schema stays, as `RemoveTempRelations` skips the original object, and the TOAST tables go with their tables.
    pub(super) fn discard_temp(&mut self) -> Result<()> {
        let Some(n) = self.an.env.temp_number() else { return Ok(()) };
        let Some(schema) = self.catalog.schema_by_name(&format!("pg_temp_{n}")) else {
            return Ok(());
        };
        let schema = ObjRef::new(PG_NAMESPACE, schema.oid);
        let mut targets = self
            .catalog
            .deletions(&[schema])
            .map_err(|_| Error::internal("the temporary schema has an object that cannot go"))?;
        targets.retain(|target| target.object != schema);
        self.catalog.remove(&targets)
    }

    /// `performMultipleDeletions`: the walk of the dependencies, the report and the removal.
    pub(super) fn perform(&mut self, objects: &[ObjRef], cascade: bool) -> Result<()> {
        let targets = match self.catalog.deletions(objects) {
            Ok(targets) => targets,
            Err(Refusal::Pinned(object)) => {
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop {} because it is required by the database system",
                        self.describe(object)
                    ),
                ));
            }
            Err(Refusal::Owned { object, owner }) => {
                let owner = self.describe(owner);
                return Err(Error::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop {} because {owner} requires it", self.describe(object)),
                )
                .with_hint(format!("You can drop {owner} instead.")));
            }
        };
        // `reportDependentObjects`: a partition of an object goes only with the object that it is a partition of.
        if let Some(target) =
            targets.iter().find(|t| t.flags & flag::IS_PART != 0 && t.flags & flag::PARTITION == 0)
        {
            let other = target.dependee.map_or_else(String::new, |other| self.describe(other));
            return Err(Error::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {} because {other} requires it", self.describe(target.object)),
            )
            .with_hint(format!("You can drop {other} instead.")));
        }
        self.report(&targets, cascade, objects)?;
        self.dropped.extend(targets.iter().map(|target| super::Dropped {
            object: target.object,
            original: target.flags & flag::ORIGINAL != 0,
            normal: target.flags & (flag::NORMAL | flag::REVERSE) != 0,
        }));
        self.catalog.remove(&targets)
    }

    /// `reportDependentObjects`: the error of `RESTRICT` when other objects depend on the objects, or the notice of `CASCADE` with the objects that also go. The objects that go by an automatic, an internal, a partition or an extension dependency are not in the report.
    fn report(&mut self, targets: &[Target], cascade: bool, objects: &[ObjRef]) -> Result<()> {
        let mut lines = Vec::new();
        let mut hidden = 0;
        let mut ok = true;
        for target in targets.iter().rev() {
            if target.flags & (flag::ORIGINAL | flag::SUBOBJECT) != 0
                || target.flags & (flag::AUTO | flag::INTERNAL | flag::PARTITION | flag::EXTENSION)
                    != 0
            {
                continue;
            }
            let line = if cascade {
                Some(format!("drop cascades to {}", self.describe(target.object)))
            } else {
                ok = false;
                target.dependee.map(|other| {
                    format!("{} depends on {}", self.describe(target.object), self.describe(other))
                })
            };
            match line {
                Some(line) if lines.len() < MAX_REPORTED => lines.push(line),
                _ => hidden += 1,
            }
        }
        let shown = lines.len();
        let mut detail = lines.join("\n");
        if hidden > 0 {
            let s = if hidden == 1 { "" } else { "s" };
            detail.push_str(&format!("\nand {hidden} other object{s} (see server log for list)"));
        }
        if !ok {
            let message = match objects {
                [object] => format!(
                    "cannot drop {} because other objects depend on it",
                    self.describe(*object)
                ),
                _ => "cannot drop desired object(s) because other objects depend on them".into(),
            };
            return Err(Error::new(SqlState::DEPENDENT_OBJECTS_STILL_EXIST, message)
                .with_detail(detail)
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        if shown > 1 {
            let count = shown + hidden;
            let s = if count == 1 { "" } else { "s" };
            let notice = Error::new(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("drop cascades to {count} other object{s}"),
            )
            .with_detail(detail);
            self.flush();
            self.messages.push(super::Message::Notice(notice));
        } else if shown == 1 {
            self.notice(SqlState::SUCCESSFUL_COMPLETION, detail);
        }
        Ok(())
    }

    /// `getObjectDescription`.
    pub(super) fn describe(&self, object: ObjRef) -> String {
        match object.class {
            PG_CLASS if object.sub != 0 => {
                let column = self
                    .catalog
                    .relation(object.oid)
                    .and_then(|r| r.column(i16::try_from(object.sub).ok()?))
                    .map_or_else(String::new, |c| c.name.clone());
                format!("column {column} of {}", self.describe_relation(object.oid))
            }
            PG_CLASS => self.describe_relation(object.oid),
            PG_TYPE => format!("type {}", self.type_text(object.oid)),
            PG_CONSTRAINT => match self.catalog.constraint(object.oid) {
                Some(con) if con.relation != 0 => {
                    format!("constraint {} on {}", con.name, self.describe_relation(con.relation))
                }
                Some(con) => format!("constraint {}", con.name),
                None => format!("constraint {}", object.oid),
            },
            PG_ATTRDEF => match self.catalog.defaults().find(|d| d.oid == object.oid) {
                Some(def) => format!(
                    "default value for {}",
                    self.describe(ObjRef::column(def.relation, def.column))
                ),
                None => format!("default value {}", object.oid),
            },
            PG_TRIGGER => match self.catalog.trigger(object.oid) {
                Some(trigger) => {
                    format!(
                        "trigger {} on {}",
                        trigger.name,
                        self.describe_relation(trigger.relation)
                    )
                }
                None => format!("trigger {}", object.oid),
            },
            PG_REWRITE => match self.catalog.view_of_rule(object.oid) {
                Some(view) => format!("rule _RETURN on {}", self.describe_relation(view.oid)),
                None => match self.catalog.rule(object.oid) {
                    Some(rule) => {
                        format!("rule {} on {}", rule.name, self.describe_relation(rule.relation))
                    }
                    None => format!("rule {}", object.oid),
                },
            },
            PG_POLICY => match self.catalog.policy(object.oid) {
                Some(policy) => {
                    format!("policy {} on {}", policy.name, self.describe_relation(policy.relation))
                }
                None => format!("policy {}", object.oid),
            },
            PG_NAMESPACE => format!("schema {}", self.schema_name(object.oid)),
            PG_TS_PARSER | PG_TS_DICT | PG_TS_TEMPLATE | PG_TS_CONFIG => self.describe_ts(object),
            PG_OPERATOR => self.describe_operator(object.oid),
            PG_STATISTIC_EXT => self.describe_stats(object.oid),
            PG_COLLATION => self.describe_collation(object.oid),
            PG_CONVERSION => self.describe_conversion(object.oid),
            PG_EVENT_TRIGGER => self.describe_event_trigger(object.oid),
            PG_AM => self.describe_access_method(object.oid),
            PG_LARGEOBJECT => format!("large object {}", object.oid),
            PG_LANGUAGE => self.describe_language(object.oid),
            PG_FOREIGN_DATA_WRAPPER | PG_FOREIGN_SERVER | PG_USER_MAPPING => {
                self.describe_foreign(object)
            }
            PG_OPCLASS | PG_OPFAMILY | PG_AMOP | PG_AMPROC => self.describe_opcf(object),
            PG_PUBLICATION | PG_PUBLICATION_REL | PG_PUBLICATION_NAMESPACE => {
                self.describe_publication(object)
            }
            PG_PROC => match self.catalog.function(object.oid) {
                Some(function) => {
                    let args: Vec<String> =
                        function.argtypes.iter().map(|&t| self.type_text(t)).collect();
                    let name = self.qualify(function.namespace, &function.name, |ns| {
                        self.catalog
                            .functions()
                            .any(|f| f.namespace == ns && f.name == function.name)
                    });
                    format!("function {name}({})", args.join(","))
                }
                None => match builtin::proc_by_oid(object.oid) {
                    Some(proc) => {
                        let args: Vec<String> =
                            proc.argtypes.iter().map(|&t| self.type_text(t)).collect();
                        let name = self.qualify(proc.namespace, proc.name, |ns| {
                            builtin::procs_named(proc.name).any(|p| p.namespace == ns)
                        });
                        format!("function {name}({})", args.join(","))
                    }
                    None => format!("function {}", object.oid),
                },
            },
            PG_CAST => match self.catalog.casts().find(|c| c.oid == object.oid) {
                Some(cast) => format!(
                    "cast from {} to {}",
                    self.type_text(cast.source),
                    self.type_text(cast.target)
                ),
                None => format!("cast {}", object.oid),
            },
            class => format!("object {class} {}", object.oid),
        }
    }

    /// `getRelationDescription`: the kind and the name, with the schema when the search path does not find the relation by its name.
    pub(super) fn describe_relation(&self, oid: u32) -> String {
        let (code, namespace, name) = match self.catalog.relation(oid) {
            Some(rel) => {
                let code = match rel.kind {
                    RelKind::Index | RelKind::PartitionedIndex => b'i',
                    RelKind::Sequence => b'S',
                    RelKind::Toast => b't',
                    RelKind::View => b'v',
                    RelKind::Composite => b'c',
                    RelKind::Table => b'r',
                    RelKind::Partitioned => b'p',
                    RelKind::MatView => b'm',
                    RelKind::Foreign => b'f',
                };
                (code, rel.namespace, rel.name.as_str())
            }
            None => match builtin::class_by_oid(oid) {
                Some(row) => (row.kind, row.namespace, row.name),
                None => return format!("relation {oid}"),
            },
        };
        let name = self.qualify(namespace, name, |ns| self.relation_in(ns, name).is_some());
        format!("{} {name}", relation_word(code))
    }

    /// `format_type_be` for a type of the catalog or a built-in type: the name with the schema when the search path does not find the type by its name, and the element type with `[]` for an array.
    pub(super) fn type_text(&self, oid: u32) -> String {
        let Some(ty) = self.catalog.type_by_oid(oid) else { return format_type(oid).into_owned() };
        if ty.element != 0 && ty.relation == 0 && ty.domain.is_none() {
            return format!("{}[]", self.type_text(ty.element));
        }
        self.qualify(ty.namespace, &ty.name, |ns| {
            self.catalog.type_by_name(ns, &ty.name).is_some()
                || (ns == crate::PG_CATALOG_NAMESPACE && TypeInfo::by_name(&ty.name).is_some())
        })
    }

    /// `quote_qualified_identifier`: the quoted name, after the quoted schema when the object is not visible. `exists(ns)` is true when the schema `ns` has an object with the name. The object is visible when its schema is the first schema of the search path that has an object with the name.
    pub(super) fn qualify(
        &self,
        namespace: u32,
        name: &str,
        exists: impl Fn(u32) -> bool,
    ) -> String {
        let first = self.an.temp_path.iter().copied().find(|&ns| ns == namespace || exists(ns));
        if first == Some(namespace) && namespace != PG_TOAST_NAMESPACE {
            return quote_identifier(name);
        }
        format!("{}.{}", quote_identifier(&self.schema_name(namespace)), quote_identifier(name))
    }
}

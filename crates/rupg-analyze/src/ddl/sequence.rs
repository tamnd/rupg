//! `CREATE SEQUENCE` and `ALTER SEQUENCE`, as `DefineSequence`, `AlterSequence`, `init_params` and `process_owned_by` of `sequence.c` run them.

use rupg_catalog::{Column, DepKind, NewRelation, ObjRef, PG_CLASS, SequenceData, SequenceInfo};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterSeqStmt, CreateSeqStmt, DefElem, Node, RangeVar};
use rupg_types::oid;

use super::comment::not_owner;
use super::drop::owner_word;
use super::table::SYSTEM_COLUMNS;
use super::{Definer, Found, relkind_detail, relname};
use crate::coerce::AtOpt;
use crate::typename::{names, place};

/// The result of `init_params`.
struct Params {
    /// The row of `pg_sequence`.
    info: SequenceInfo,
    /// The `last_value` and `is_called` of the row of the file.
    last_value: i64,
    is_called: bool,
    /// True when `log_cnt` goes back to 0.
    reset_state: bool,
    /// True when the sequence needs a new file, for each option but `OWNED BY`.
    rewrite: bool,
    /// The names of `OWNED BY`.
    owned_by: Option<Vec<String>>,
}

/// The options of `init_params` by name.
#[derive(Default)]
struct Options<'n> {
    as_type: Option<&'n DefElem>,
    start: Option<&'n DefElem>,
    restart: Option<&'n DefElem>,
    increment: Option<&'n DefElem>,
    max: Option<&'n DefElem>,
    min: Option<&'n DefElem>,
    cache: Option<&'n DefElem>,
    cycle: Option<&'n DefElem>,
    owned_by: Option<&'n DefElem>,
}

/// `errorConflictingDefElem`.
pub(super) fn conflicting(def: &DefElem) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
        .at_opt(place(def.location))
}

/// `format_type_be` of a type that a sequence can have.
fn type_text(ty: u32) -> &'static str {
    match ty {
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        _ => "bigint",
    }
}

/// The range of the values of a type that a sequence can have.
fn type_range(ty: u32) -> (i64, i64) {
    match ty {
        oid::INT2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
        oid::INT4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        _ => (i64::MIN, i64::MAX),
    }
}

/// An error of `init_params` for a value that is not valid.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// The names of a `List` of `String` nodes, as `defGetQualifiedName` gives them.
fn qualified_names(def: &DefElem) -> Vec<String> {
    match &def.arg {
        Some(Node::List(list)) => names(list).into_iter().map(str::to_string).collect(),
        Some(Node::String(name)) => vec![name.to_string()],
        _ => Vec::new(),
    }
}

impl Definer<'_, '_> {
    /// `DefineSequence`.
    pub(super) fn create_sequence(&mut self, stmt: &CreateSeqStmt) -> Result<()> {
        let rv = stmt.sequence.as_deref().cloned().unwrap_or_default();
        let name = relname(&rv).to_string();
        if stmt.if_not_exists {
            let namespace = self.creation_namespace(&rv)?;
            if self.relation_in(namespace, &name).is_some() {
                self.notice(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists, skipping"),
                );
                return Ok(());
            }
        }
        let params = self.init_params(&stmt.options, stmt.for_identity, None)?;
        let namespace = self.creation_namespace(&rv)?;
        self.check_relation_name(namespace, &name)?;
        let columns = vec![
            Column::new("last_value", oid::INT8, -1, 0),
            Column::new("log_cnt", oid::INT8, -1, 0),
            Column::new("is_called", oid::BOOL, -1, 0),
        ];
        let owner = if stmt.ownerId == 0 { self.user } else { stmt.ownerId };
        let new = NewRelation { namespace, name, owner, columns };
        let mut info = params.info;
        info.data = SequenceData::new(params.last_value);
        let sequence = self.catalog.create_sequence(new, info)?;
        self.apply_default_acl(ObjRef::new(PG_CLASS, sequence))?;
        if rv.relpersistence == b'u' {
            self.catalog.set_unlogged(sequence, true)?;
        }
        if let Some(owned_by) = &params.owned_by {
            self.process_owned_by(sequence, owned_by, stmt.for_identity)?;
        }
        Ok(())
    }

    /// `AlterSequence`. A change of any option but `OWNED BY` gives the sequence a new file with the new row, so that a rollback undoes the change.
    pub(super) fn alter_sequence(&mut self, stmt: &AlterSeqStmt) -> Result<()> {
        let rv = stmt.sequence.as_deref().cloned().unwrap_or_default();
        let found = match self.lookup_relation(&rv) {
            Ok(found) => found,
            Err(e)
                if stmt.missing_ok
                    && matches!(
                        e.state(),
                        SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                    ) =>
            {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("relation \"{}\" does not exist, skipping", relname(&rv)),
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let kind = self.kind_of(found);
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner(owner_word(kind), relname(&rv)));
        }
        let sequence = match found {
            Found::User(oid) => self.catalog.relation(oid),
            Found::Builtin(_) => None,
        };
        let Some((oid, info)) = sequence.and_then(|r| Some((r.oid, r.sequence.clone()?))) else {
            return Err(self.not_a_sequence(found, &rv));
        };
        let row = self.an.env.sequence_row(info.file).unwrap_or(info.data);
        let params = self.init_params(&stmt.options, stmt.for_identity, Some((&info, row)))?;
        let rewrite = params.rewrite.then_some(SequenceData {
            last_value: params.last_value,
            log_cnt: if params.reset_state { 0 } else { row.log_cnt },
            is_called: params.is_called,
        });
        self.catalog.alter_sequence(oid, params.info, rewrite)?;
        if let Some(owned_by) = &params.owned_by {
            self.process_owned_by(oid, owned_by, stmt.for_identity)?;
        }
        Ok(())
    }

    /// The error of `sequence_open` for a relation that is not a sequence.
    fn not_a_sequence(&self, found: Found, rv: &RangeVar) -> Error {
        let name = match found {
            Found::User(oid) => self.catalog.relation(oid).map(|r| r.name.clone()),
            Found::Builtin(row) => Some(row.name.to_string()),
        };
        let name = name.unwrap_or_else(|| relname(rv).to_string());
        Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not a sequence"))
    }

    /// `defGetInt64`: the value of an option as a `bigint`.
    fn int64_option(&self, def: &DefElem) -> Result<i64> {
        let text = match &def.arg {
            Some(Node::Integer(value)) => return Ok(i64::from(*value)),
            Some(Node::Float(text)) => text,
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("{} requires a numeric value", def.defname.as_deref().unwrap_or("")),
                ));
            }
        };
        self.an
            .env
            .input(oid::INT8, text, -1)?
            .as_i64()
            .ok_or_else(|| Error::internal("the input of bigint gave a value of another type"))
    }

    /// The row of `pg_sequence` of the sequence of an identity column, from the options of the column.
    pub(super) fn identity_sequence_info(
        &mut self,
        options: &[Option<Node>],
    ) -> Result<SequenceInfo> {
        let params = self.init_params(options, true, None)?;
        let mut info = params.info;
        info.data = SequenceData::new(params.last_value);
        Ok(info)
    }

    /// `init_params`: the checks of the options and the new row of `pg_sequence`. `current` is the row of `pg_sequence` and the row of the file for `ALTER SEQUENCE`, and `None` for `CREATE SEQUENCE`, which takes the defaults for the options that it does not give.
    fn init_params(
        &mut self,
        options: &[Option<Node>],
        for_identity: bool,
        current: Option<(&SequenceInfo, SequenceData)>,
    ) -> Result<Params> {
        let mut o = Options::default();
        let mut rewrite = false;
        for def in options.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let def: &DefElem = def;
            let slot = match def.defname.as_deref().unwrap_or("") {
                "as" => &mut o.as_type,
                "increment" => &mut o.increment,
                "start" => &mut o.start,
                "restart" => &mut o.restart,
                "maxvalue" => &mut o.max,
                "minvalue" => &mut o.min,
                "cache" => &mut o.cache,
                "cycle" => &mut o.cycle,
                "owned_by" => &mut o.owned_by,
                "sequence_name" => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "invalid sequence option SEQUENCE NAME",
                    )
                    .at_opt(place(def.location)));
                }
                other => return Err(Error::internal(format!("option \"{other}\" not recognized"))),
            };
            if slot.is_some() {
                return Err(conflicting(def));
            }
            *slot = Some(def);
            if def.defname.as_deref() != Some("owned_by") {
                rewrite = true;
            }
        }
        let init = current.is_none();
        let mut info = match current {
            Some((info, _)) => info.clone(),
            None => SequenceInfo {
                ty: oid::INT8,
                start: 0,
                increment: 0,
                max: 0,
                min: 0,
                cache: 0,
                cycle: false,
                file: 0,
                data: SequenceData::new(0),
            },
        };
        let (mut last_value, mut is_called) =
            current.map_or((0, false), |(_, row)| (row.last_value, row.is_called));
        let mut reset_state = init;
        let mut reset_max = false;
        let mut reset_min = false;
        if let Some(def) = o.as_type {
            let ty = match &def.arg {
                Some(Node::TypeName(name)) => self.an.type_name(name)?.0,
                _ => 0,
            };
            if !matches!(ty, oid::INT2 | oid::INT4 | oid::INT8) {
                return Err(invalid(if for_identity {
                    "identity column type must be smallint, integer, or bigint".to_string()
                } else {
                    "sequence type must be smallint, integer, or bigint".to_string()
                }));
            }
            if !init {
                let (min, max) = type_range(info.ty);
                reset_max = info.max == max;
                reset_min = info.min == min;
            }
            info.ty = ty;
        }
        if let Some(def) = o.increment {
            info.increment = self.int64_option(def)?;
            if info.increment == 0 {
                return Err(invalid("INCREMENT must not be zero".to_string()));
            }
            reset_state = true;
        } else if init {
            info.increment = 1;
        }
        if let Some(def) = o.cycle {
            info.cycle = matches!(def.arg, Some(Node::Boolean(true)));
            reset_state = true;
        } else if init {
            info.cycle = false;
        }
        let (type_min, type_max) = type_range(info.ty);
        match o.max {
            Some(def) if def.arg.is_some() => {
                info.max = self.int64_option(def)?;
                reset_state = true;
            }
            max if init || max.is_some() || reset_max => {
                info.max = if info.increment > 0 || reset_max { type_max } else { -1 };
                reset_state = true;
            }
            _ => {}
        }
        if info.max < type_min || info.max > type_max {
            return Err(invalid(format!(
                "MAXVALUE ({}) is out of range for sequence data type {}",
                info.max,
                type_text(info.ty)
            )));
        }
        match o.min {
            Some(def) if def.arg.is_some() => {
                info.min = self.int64_option(def)?;
                reset_state = true;
            }
            min if init || min.is_some() || reset_min => {
                info.min = if info.increment < 0 || reset_min { type_min } else { 1 };
                reset_state = true;
            }
            _ => {}
        }
        if info.min < type_min || info.min > type_max {
            return Err(invalid(format!(
                "MINVALUE ({}) is out of range for sequence data type {}",
                info.min,
                type_text(info.ty)
            )));
        }
        if info.min >= info.max {
            return Err(invalid(format!(
                "MINVALUE ({}) must be less than MAXVALUE ({})",
                info.min, info.max
            )));
        }
        if let Some(def) = o.start {
            info.start = self.int64_option(def)?;
        } else if init {
            info.start = if info.increment > 0 { info.min } else { info.max };
        }
        if info.start < info.min {
            return Err(invalid(format!(
                "START value ({}) cannot be less than MINVALUE ({})",
                info.start, info.min
            )));
        }
        if info.start > info.max {
            return Err(invalid(format!(
                "START value ({}) cannot be greater than MAXVALUE ({})",
                info.start, info.max
            )));
        }
        if let Some(def) = o.restart {
            last_value = if def.arg.is_some() { self.int64_option(def)? } else { info.start };
            is_called = false;
            reset_state = true;
        } else if init {
            last_value = info.start;
            is_called = false;
        }
        if last_value < info.min {
            return Err(invalid(format!(
                "RESTART value ({last_value}) cannot be less than MINVALUE ({})",
                info.min
            )));
        }
        if last_value > info.max {
            return Err(invalid(format!(
                "RESTART value ({last_value}) cannot be greater than MAXVALUE ({})",
                info.max
            )));
        }
        if let Some(def) = o.cache {
            info.cache = self.int64_option(def)?;
            if info.cache <= 0 {
                return Err(invalid(format!("CACHE ({}) must be greater than zero", info.cache)));
            }
            reset_state = true;
        } else if init {
            info.cache = 1;
        }
        let owned_by = o.owned_by.map(qualified_names);
        Ok(Params { info, last_value, is_called, reset_state, rewrite, owned_by })
    }

    /// `process_owned_by`: the sequence goes with the column of `OWNED BY table.column`, or with no column for `OWNED BY NONE`. The table must have the owner and the schema of the sequence.
    fn process_owned_by(
        &mut self,
        sequence: u32,
        owned_by: &[String],
        for_identity: bool,
    ) -> Result<()> {
        let kind = if for_identity { DepKind::Internal } else { DepKind::Auto };
        let column = match owned_by {
            [none] if none == "none" => None,
            [_] => {
                return Err(Error::new(SqlState::SYNTAX_ERROR, "invalid OWNED BY option")
                    .with_hint("Specify OWNED BY table.column or OWNED BY NONE."));
            }
            [relation @ .., column] => Some(self.owned_by_column(sequence, relation, column)?),
            [] => return Err(Error::internal("OWNED BY with no names")),
        };
        if kind == DepKind::Auto {
            let me = ObjRef::new(PG_CLASS, sequence);
            let identity = self.catalog.depends().iter().find(|d| {
                d.object == me && d.referenced.class == PG_CLASS && d.kind == DepKind::Internal
            });
            if let Some(identity) = identity {
                let table = self.catalog.relation(identity.referenced.oid).map(|r| r.name.clone());
                let name = self.catalog.relation(sequence).map(|r| r.name.clone());
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot change ownership of identity sequence",
                )
                .with_detail(format!(
                    "Sequence \"{}\" is linked to table \"{}\".",
                    name.unwrap_or_default(),
                    table.unwrap_or_default()
                )));
            }
        }
        self.catalog.set_owned_by(sequence, column, kind);
        Ok(())
    }

    /// The table and the attribute number of the column of `OWNED BY table.column`.
    fn owned_by_column(
        &self,
        sequence: u32,
        relation: &[String],
        column: &str,
    ) -> Result<(u32, i16)> {
        let mut rv = RangeVar::default();
        match relation {
            [rel] => rv.relname = Some(rel.as_str().into()),
            [schema, rel] => {
                rv.schemaname = Some(schema.as_str().into());
                rv.relname = Some(rel.as_str().into());
            }
            [catalog, schema, rel] => {
                rv.catalogname = Some(catalog.as_str().into());
                rv.schemaname = Some(schema.as_str().into());
                rv.relname = Some(rel.as_str().into());
            }
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!(
                        "improper relation name (too many dotted names): {}",
                        relation.join(".")
                    ),
                ));
            }
        }
        let found = self.lookup_relation(&rv)?;
        let kind = self.kind_of(found);
        let (table, name, owner, namespace) = match found {
            Found::User(oid) => {
                let rel = self.catalog.relation(oid);
                (
                    oid,
                    rel.map(|r| r.name.clone()).unwrap_or_default(),
                    rel.map_or(0, |r| r.owner),
                    rel.map_or(0, |r| r.namespace),
                )
            }
            Found::Builtin(row) => (row.oid, row.name.to_string(), row.owner, row.namespace),
        };
        if !matches!(kind, b'r' | b'f' | b'v' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("sequence cannot be owned by relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let seq = self.catalog.relation(sequence);
        if seq.map_or(0, |r| r.owner) != owner {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "sequence must have same owner as table it is linked to",
            ));
        }
        if seq.map_or(0, |r| r.namespace) != namespace {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "sequence must be in same schema as table it is linked to",
            ));
        }
        let number = match found {
            Found::User(oid) => self.catalog.relation(oid).and_then(|r| r.column_number(column)),
            Found::Builtin(_) => None,
        };
        let system = SYSTEM_COLUMNS.iter().position(|c| *c == column);
        let number = number.or_else(|| system.and_then(|i| i16::try_from(i).ok()).map(|i| i - 6));
        match number {
            Some(number) => Ok((table, number)),
            None => Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{name}\" does not exist"),
            )),
        }
    }
}
